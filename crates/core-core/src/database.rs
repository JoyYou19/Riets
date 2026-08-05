use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use core_index::{
    analyzer::Analyzer, document::IndexPolicy, lsm::{
        LsmIndex,
        index_worker::{IndexingStats, Phase, ProgressSnapshot, ReindexProgress, ReindexingStats},
        worker::CompactionWorker,
    }, types::DocId,
};
use core_protocol::errors::CorelamoError;
use core_query::{
    Query,
    planner::{QueryPlan, QueryPlanner},
    query_string_parser::parse_and_analyze,
};

use core_storage::{
    binary_store::BinaryDocumentStore,
    document_store::{DocumentStore, StoredDocument},
    search_database::{
        DocumentInput, IndexMode, InsertReport, PendingOp, SearchDatabase, SearchDocumentHit,
    },
    wal::{Wal, WalRecord},
};

use core_logs::logger;
use slog::{Logger, error, info, warn};

use crate::{
    command_reponse_definitions::{LookupCommand, SearchCommand},
    metrics::DatabaseMetrics,
    options::DatabaseOptions,
};
use indexmap::IndexMap;

/// Everything staging needs, cloned out under a brief lock so the build itself
/// can run unlocked.
pub struct ReindexParams {
    pub root: PathBuf,
    pub policy: IndexPolicy,
    pub options: DatabaseOptions,
    pub analyzer: Analyzer,
    pub progress: Arc<ReindexProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseState {
    Running,
    /// Handle is temporarily absent because the index swap is in progress.
    Swapping,
    Stopped,
}

impl DatabaseState {
    pub fn as_str(self) -> &'static str {
        match self {
            DatabaseState::Running => "running",
            DatabaseState::Swapping => "swapping",
            DatabaseState::Stopped => "stopped",
        }
    }
}

// Currently the main entry point to the database
pub struct CorelamoDatabase {
    root: PathBuf,
    policy_path: PathBuf,
    policy: IndexPolicy,
    options: DatabaseOptions,
    db: Option<SearchDatabase<BinaryDocumentStore>>,
    compaction_worker: Option<CompactionWorker>,
    metrics: Mutex<DatabaseMetrics>,
    progress: Arc<ReindexProgress>,
    log: Logger,
    wal: Wal,
    pending_ops: Vec<PendingOp>,
}

impl CorelamoDatabase {
    //Norcha paskaties
    const MAX_PENDING_OPS: usize = 500_000;

    fn config_full_path_from(root: &Path) -> std::path::PathBuf {
        root.join("config.toml")
    }

    //INFO: just creates everything for the database, doesnt start it
    pub fn create(root: impl AsRef<Path>, options: DatabaseOptions) -> Result<Self, CorelamoError> {
        let root = root.as_ref().to_path_buf();
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        if root.exists() {
            return Err(CorelamoError::AlreadyExists(format!(
                "database at {} already exists",
                root.display()
            )));
        }
        std::fs::create_dir_all(&root)?;
        //logging
        let log = logger::db_logger(&root, &name);
        let wal = Wal::open(root.join("wal.log"), core_storage::wal::SyncMode::SyncEach)
            .map_err(|e| CorelamoError::Internal(format!("failed to open WAL: {e}")))?;

        let policy_path = root.join("policy.toml");
        let policy = IndexPolicy::default_document();
        policy.save(&policy_path)?;
        options.save_to_file(Self::config_full_path_from(&root))?;

        let store_path = root.join("documents.bin");
        BinaryDocumentStore::open(&store_path)?;

        Ok(Self {
            root,
            policy_path,
            policy,
            options,
            db: None,
            metrics: Mutex::new(DatabaseMetrics::default()),
            compaction_worker: None,
            progress: ReindexProgress::new(),
            log,
            wal,
            pending_ops: Vec::new(),
        })
    }

    //acknowledge the database on startup
    pub fn load(root: impl AsRef<Path>) -> Result<Self, CorelamoError> {
        let root = root.as_ref().to_path_buf();
        let policy_path = root.join("policy.toml");
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        if !policy_path.exists() {
            return Err(CorelamoError::NotFound(format!(
                "no policy found at {}, considering database to be corupted",
                root.display()
            )));
        }

        let policy = IndexPolicy::load(&policy_path)?;
        let options = DatabaseOptions::load_or_default(Self::config_full_path_from(&root));
        //logging
        let log = logger::db_logger(&root, &name);
        let wal = Wal::open(root.join("wal.log"), core_storage::wal::SyncMode::SyncEach)
            .map_err(|e| CorelamoError::Internal(format!("failed to open WAL: {e}")))?;

        Ok(Self {
            root,
            policy_path,
            policy,
            options,
            db: None,
            compaction_worker: None,
            metrics: Mutex::new(DatabaseMetrics::default()),
            progress: ReindexProgress::new(),
            log,
            wal,
            pending_ops: Vec::new(),
        })
    }

    /// Single construction point for the analyzer. `start`, staging, and the
    /// post-swap database all go through here, so they cannot drift apart.
    ///
    /// TODO: build this from `self.options`/`self.policy` once the analyzer is
    /// configurable. Until then the important property is that every site
    /// agrees, not which analyzer they agree on.
    fn analyzer(&self) -> Analyzer {
        Analyzer::new()
    }

    pub fn lookup(
        &self,
        command: &LookupCommand,
    ) -> Result<(Vec<(String, BTreeMap<String, String>)>, Vec<String>), CorelamoError> {
        self.db_ref()?
            .lookup_documents(&command.ids, command.return_fields.as_ref())
            .map_err(CorelamoError::from)
    }

    pub fn start(&mut self) -> Result<(), CorelamoError> {
        if self.db.is_some() {
            return Ok(());
        }
        let index_root = self.root.join("index");
        let store_path = self.root.join("documents.bin");
        let analyzer = self.analyzer();
        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        let store = BinaryDocumentStore::open(&store_path)?;
        let mut db = SearchDatabase::with_policy(store, index, analyzer, self.policy.clone());

        //recovery
        let checkpoint = self
            .wal
            .read_checkpoint()
            .map_err(|e| CorelamoError::Internal(format!("failed to read WAL checkpoint: {e}")))?;
        let records = self
            .wal
            .replay_from(checkpoint)
            .map_err(|e| CorelamoError::Internal(format!("failed to replay WAL: {e}")))?;
        info!(self.log, "WAL recovery started";
                    "checkpoint" => checkpoint,
                    "durable_offset" => self.wal.durable_offset(),
                    "records_to_replay" => records.len(),
        );
        for (_offset, payload) in records {
            let (record, _): (WalRecord, usize) =
                bincode::decode_from_slice(&payload, bincode::config::standard()).map_err(|e| {
                    CorelamoError::Internal(format!("failed to decode WAL record: {e}"))
                })?;
            match record {
                WalRecord::Create(inputs) => {
                    info!(self.log, "WAL replay: Create"; "documents" => inputs.len());
                    let _ = db.put_documents_parallel(
                        inputs,
                        self.options.runtime.indexing_batch_size,
                        self.options.runtime.indexing_window_size,
                    );
                }
                WalRecord::Upsert(input) => {
                    info!(self.log, "WAL replay: Upsert"; "external_id" => &input.external_id);
                    db.upsert_document(input, IndexMode::StoreAndIndex)
                        .map_err(|e| {
                            CorelamoError::Internal(format!("recovery apply failed: {e}"))
                        })?;
                }
                WalRecord::Modify {
                    external_id,
                    payload,
                } => {
                    info!(self.log, "WAL replay: Modify"; "external_id" => external_id);
                    // decide: does Modify mean upsert-by-id? if so:
                    if let Some(doc) = payload.into_iter().next() {
                        db.upsert_document(doc, IndexMode::StoreAndIndex)
                            .map_err(|e| {
                                CorelamoError::Internal(format!("recovery apply failed: {e}"))
                            })?;
                    }
                }
                WalRecord::Delete { external_id } => {
                    let _ = db.delete_document(&external_id);
                    info!(self.log, "WAL replay: Delete"; "external_id" => external_id);
                }
                WalRecord::Clear => {
                    // skip -- clear was already applied before this WAL record was written
                }
            }
        }
        self.compaction_worker = if self.options.enable_background_compaction {
            Some(CompactionWorker::start(
                db.index_sender(),
                self.options.runtime.compaction,
                self.options.compaction_interval,
            ))
        } else {
            None
        };

        self.db = Some(db);
        info!(self.log, "database started");
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), CorelamoError> {
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
            info!(self.log, "compaction worker stopped");
        }
        if let Some(db) = self.db.take() {
            db.shutdown()?;
            info!(self.log, "database stopped");
        }
        self.wal
            .reset()
            .map_err(|e| CorelamoError::Internal(format!("wal reset failed: {e}")))?;
        self.wal
            .write_checkpoint(0)
            .map_err(|e| CorelamoError::Internal(format!("checkpoint write failed: {e}")))?;
        info!(self.log, "WAL reset on clean shutdown");
        Ok(())
    }

    /// Same work as `stop`; kept as a distinct entry point for the actor's
    /// terminal command so the two can never drift apart.
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.stop()
            .map_err(|e| io::Error::other(format!("shutdown failed: {e}")))
    }

    pub fn restart(&mut self) -> Result<(), CorelamoError> {
        self.stop()?;
        self.start()?;
        info!(self.log, "database restarted");
        Ok(())
    }

    /// Stops background compaction without touching the database handle.
    /// Called before staging so compaction is not merging the old index --
    /// that work is discarded at swap and competes with staging for I/O.
    pub fn pause_compaction(&mut self) -> Result<(), CorelamoError> {
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
            info!(self.log, "compaction paused for reindex");
        }
        Ok(())
    }

    fn start_compaction_worker(&mut self, db: &SearchDatabase<BinaryDocumentStore>) {
        self.compaction_worker = if self.options.enable_background_compaction {
            Some(CompactionWorker::start(
                db.index_sender(),
                self.options.runtime.compaction,
                self.options.compaction_interval,
            ))
        } else {
            None
        };
    }

    pub fn set_options(&mut self, options: DatabaseOptions) -> Result<(), CorelamoError> {
        options.save_to_file(Self::config_full_path_from(&self.root))?;
        self.options = options;
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.db.is_some()
    }

    pub fn options(&self) -> &DatabaseOptions {
        &self.options
    }

    pub fn put_documents_parallel(
        &mut self,
        inputs: Vec<DocumentInput>,
    ) -> io::Result<InsertReport> {
        let started = std::time::Instant::now();
        let count = inputs.len();
        let batch_size = self.options.runtime.indexing_batch_size;
        let window_size = self.options.runtime.indexing_window_size;

        // Captured before `inputs` is consumed below.
        let ids: Vec<String> = if self.progress.phase().is_running() {
            inputs.iter().map(|i| i.external_id.clone()).collect()
        } else {
            Vec::new()
        };

        let result = (|| {
            //WALis
            let record = WalRecord::Create(inputs.clone());
            let encoded = bincode::encode_to_vec(&record, bincode::config::standard())
                .map_err(|e| io::Error::other(format!("wal encode failed: {e}")))?;
            let offset = self
                .wal
                .append(&encoded)
                .map_err(|e| io::Error::other(format!("failed to append WAL record: {e}")))?;

            info!(self.log, "WAL append";
                "operation" => "create",
                "documents" => inputs.len(),
                "offset" => offset,
                "durable_offset" => self.wal.durable_offset(),
            );
            let db = self.db_mut()?;
            let report = db.put_documents_parallel(inputs, batch_size, window_size)?;
            // NOTE: this flush is why segment count tracked HTTP request count
            // exactly. Durability is already guaranteed by stop/shutdown; drop
            // this call if you would rather let the memtable threshold govern.
            db.flush()?;
            if let Err(e) = self.wal.write_checkpoint(offset) {
                warn!(self.log, "checkpoint write failed"; "error" => %e);
            }
            Ok(report)
        })();

        // Queue for replay against the new index, outside the closure so the
        // mutable borrow of `self` has ended.
        if result.is_ok() {
            for id in ids {
                let doc = match self.db_mut() {
                    Ok(db) => db
                        .get_document(&id)
                        .ok()
                        .flatten()
                        .map(|d| db.to_indexed(&d)),
                    Err(_) => None,
                };
                if let Some(doc) = doc {
                    self.queue_op(PendingOp::Index { doc });
                }
            }
        }

        let elapsed = started.elapsed();
        {
            let mut m = self.metrics.lock().unwrap();
            m.indexing_requests += 1;
            m.indexing_total_time += elapsed;
            if result.is_err() {
                m.indexing_errors += 1;
            }
        }

        match &result {
            Ok(_) => info!(self.log, "indexed batch";
                "documents" => count,
                "batch_size" => batch_size,
                "elapsed_ms" => elapsed.as_millis(),
            ),
            Err(e) => {
                error!(self.log, "indexing failed";
                    "documents" => count,
                    "batch_size" => batch_size,
                    "elapsed_ms" => elapsed.as_millis(),
                    "error" => %e,
                );
            }
        }

        result
    }

    pub fn build_query(&self, input: &str) -> Result<Option<Query>, CorelamoError> {
        let db = self.db_ref()?;
        parse_and_analyze(input, db.get_analyzer())
    }

    //INFO: changed the function call to take a SearchCommand not just a string of query
    pub fn search(&self, command: &SearchCommand) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        let started = std::time::Instant::now();
        //TODO: make the docs 10 default configurable
        let limit = command.docs.unwrap_or(10);
        let offset = command.offset.unwrap_or(0);

        let result = (|| {
            let Some(query) = self.build_query(&command.query)? else {
                return Ok(Vec::new());
            };
            //INFO: you can see how the query got parsed + analyzed to our structs
            let parsedq = format!("{:?}", query);
            info!(self.log, "query parsing result";
                "output" => parsedq,
                "input" => &command.query,

            );
            let plan = QueryPlanner::plan(query);
            self.search_plan(&plan, command.return_fields.as_ref(), offset, limit)
        })();

        let elapsed = started.elapsed();
        {
            let mut m = self.metrics.lock().unwrap();
            m.search_requests += 1;
            m.search_total_time += elapsed;
            if result.is_err() {
                m.search_errors += 1;
            }
        }

        match &result {
            Ok(hits) => {
                info!(self.log, "searched";
                    "query" => &command.query,
                    "offset" => offset,
                    "limit" => limit,
                    "returned" => hits.len(),
                    "elapsed_ms" => elapsed.as_millis(),
                );
            }
            Err(e) => {
                error!(self.log, "search failed";
                    "query" => &command.query,
                    "offset" => offset,
                    "limit" => limit,
                    "elapsed_ms" => elapsed.as_millis(),
                    "error" => %e,
                );
            }
        }

        result.map_err(CorelamoError::from)
    }

    //INFO: old one now we have parse_query()
    // fn build_query(&self, input: &str) -> io::Result<Option<Query>> {
    //     let db = self.db_ref()?;
    //
    //     let terms: Vec<String> = input
    //         .split_whitespace()
    //         .filter_map(|term| db.analyze_query_term(term))
    //         .collect();
    //
    //     Ok(match terms.len() {
    //         0 => None,
    //         1 => Some(Query::Term(terms[0].clone())),
    //         _ => Some(Query::And(terms.into_iter().map(Query::Term).collect())),
    //     })
    // }
    //

    pub fn delete_document(&mut self, external_id: &str) -> io::Result<()> {
        let record = WalRecord::Delete {
            external_id: external_id.to_string(),
        };
        let encoded = bincode::encode_to_vec(&record, bincode::config::standard())
            .map_err(|e| io::Error::other(format!("wal encode failed: {e}")))?;
        self.wal
            .append(&encoded)
            .map_err(|e| io::Error::other(format!("failed to append WAL record: {e}")))?;

        let old = self
            .db_mut()?
            .get_document(external_id)?
            .map(|d| d.internal_id);
        self.db_mut()?.delete_document(external_id)?;
        if let Some(internal_id) = old {
            self.queue_op(PendingOp::Tombstone { internal_id });
        }
        Ok(())
    }

    pub fn upsert_document(&mut self, input: DocumentInput) -> io::Result<()> {
        let record = WalRecord::Upsert(input.clone());
        let encoded = bincode::encode_to_vec(&record, bincode::config::standard())
            .map_err(|e| io::Error::other(format!("wal encode failed: {e}")))?;
        self.wal
            .append(&encoded)
            .map_err(|e| io::Error::other(format!("failed to append WAL record: {e}")))?;

        let external = input.external_id.clone();
        let old = self
            .db_mut()?
            .get_document(&external)?
            .map(|d| d.internal_id);

        self.db_mut()?
            .upsert_document(input, IndexMode::StoreAndIndex)?;

        if let Some(internal_id) = old {
            self.queue_op(PendingOp::Tombstone { internal_id });
        }
        let queued = {
            let db = self.db_mut()?;
            db.get_document(&external)?.map(|doc| db.to_indexed(&doc))
        };
        if let Some(doc) = queued {
            self.queue_op(PendingOp::Index { doc });
        }
        Ok(())
    }

    pub fn get_document(&self, external_id: &str) -> io::Result<Option<StoredDocument>> {
        self.db_ref()?.get_document(external_id)
    }

    fn db_mut(&mut self) -> io::Result<&mut SearchDatabase<BinaryDocumentStore>> {
        self.db
            .as_mut()
            .ok_or_else(|| io::Error::other("database is closed"))
    }

    fn db_ref(&self) -> io::Result<&SearchDatabase<BinaryDocumentStore>> {
        self.db
            .as_ref()
            .ok_or_else(|| io::Error::other("database is closed"))
    }

    pub fn search_plan(
        &self,
        plan: &QueryPlan,
        return_fields: Option<&IndexMap<String, bool>>,
        offset: usize,
        limit: usize,
    ) -> io::Result<Vec<SearchDocumentHit>> {
        self.db_ref()?
            .search_document_hits_plan(plan, return_fields, offset, limit)
    }

    pub fn analyze_query_term(&self, term: &str) -> io::Result<Option<String>> {
        Ok(self.db_ref()?.analyze_query_term(term))
    }

    /// Shared progress handle. The actor clones this once and reads it without
    /// taking the database mutex.
    pub fn progress(&self) -> Arc<ReindexProgress> {
        Arc::clone(&self.progress)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Cheap denominator for the reindex. O(1).
    pub fn document_count_hint(&self) -> u64 {
        self.db
            .as_ref()
            .map(|d| d.document_count() as u64)
            .unwrap_or(0)
    }

    pub fn get_logs(&self, date: Option<String>) -> Result<String, CorelamoError> {
        let log_dir = self.root.join("logs");
        let name = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let log_file = match date {
            Some(d) => {
                //INFO: this is so that someone doesnt try to do like get-logs date =
                //../../../home/admin/.ssh
                if d.len() != 10 || !d.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                    return Err(CorelamoError::InvalidData(format!(
                        "invalid date '{d}', expected YYYY-MM-DD"
                    )));
                }
                let file = log_dir.join(format!("{name}-{d}.log"));
                if !file.exists() {
                    return Err(CorelamoError::NotFound(format!("no logs for {d}")));
                }
                file
            }
            None => {
                //nav date = latest
                let prefix = format!("{name}-");
                let mut candidates: Vec<std::path::PathBuf> = Vec::new();
                if log_dir.exists() {
                    for entry in std::fs::read_dir(&log_dir).map_err(|e| {
                        CorelamoError::Internal(format!("failed to read log dir: {e}"))
                    })? {
                        let entry = entry.map_err(|e| {
                            CorelamoError::Internal(format!("failed to read log entry: {e}"))
                        })?;
                        let fname = entry.file_name().to_string_lossy().into_owned();
                        if fname.starts_with(&prefix) && fname.ends_with(".log") {
                            candidates.push(entry.path());
                        }
                    }
                }
                candidates.sort();
                match candidates.pop() {
                    Some(p) => p,
                    None => return Ok(String::new()),
                }
            }
        };

        std::fs::read_to_string(&log_file)
            .map_err(|e| CorelamoError::Internal(format!("failed to read logs: {e}")))
    }

    pub fn clear_logs(&mut self) -> Result<(), CorelamoError> {
        //delete logs
        let logs_dir = self.root.join("logs");
        if logs_dir.exists() {
            std::fs::remove_dir_all(&logs_dir)?;
            std::fs::create_dir_all(&logs_dir)?;
        }
        //fresh start
        let name = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        self.log = logger::db_logger(&self.root, &name);

        info!(self.log, "logs cleared");
        Ok(())
    }

    pub fn clear(&mut self) -> io::Result<()> {
        let record = WalRecord::Clear;
        let encoded = bincode::encode_to_vec(&record, bincode::config::standard())
            .map_err(|e| io::Error::other(format!("failed to encode WAL record: {e}")))?;
        self.wal
            .append(&encoded)
            .map_err(|e| io::Error::other(format!("failed to append WAL record: {e}")))?;
        // Shut the old database down BEFORE removing its files, otherwise the
        // live handle keeps writing to an unlinked inode (or the removal fails
        // outright on Windows).
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
        }
        if let Some(db) = self.db.take()
            && let Err(e) = db.shutdown()
        {
            warn!(self.log, "clear: shutdown of old database failed"; "error" => %e);
        }

        let index_root = self.root.join("index");
        let store_path = self.root.join("documents.bin");
        std::fs::remove_dir_all(&index_root).ok();
        std::fs::remove_dir_all(self.root.join("index.new")).ok();
        std::fs::remove_dir_all(self.root.join("index.old")).ok();
        std::fs::remove_file(&store_path).ok();

        let analyzer = self.analyzer();
        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        let store = BinaryDocumentStore::open(&store_path)?;
        let db = SearchDatabase::with_policy(store, index, analyzer, self.policy.clone());

        self.start_compaction_worker(&db);
        self.db = Some(db);
        self.wal
            .reset()
            .map_err(|e| io::Error::other(format!("wal reset failed: {e}")))?;
        info!(self.log, "WAL reset after clear");
        info!(self.log, "database cleared");
        Ok(())
    }

    pub fn state(&self) -> DatabaseState {
        if self.db.is_some() {
            DatabaseState::Running
        } else if self.progress.phase().is_running() {
            DatabaseState::Swapping
        } else {
            DatabaseState::Stopped
        }
    }

    /// Stats for a running database. The reindexing block always comes from the
    /// progress snapshot, so it can never disagree with what the actor reports
    /// while the lock is held elsewhere.
    pub fn stats(&self) -> io::Result<DatabaseStats> {
        let snapshot = self.progress.snapshot();
        let db = self.db_ref()?;

        let mut indexing = db.index_worker().get_stats()?;
        // During a reindex the staging index owns the real counter, so surface
        // the progress value when it is ahead.
        if snapshot.done > indexing.total_documents_indexed {
            indexing.total_documents_indexed = snapshot.done;
        }

        Ok(DatabaseStats {
            database_state: DatabaseState::Running,
            document_count: db.document_count(),
            segment_count: db.segment_count()?,
            background_compaction_enabled: self.compaction_worker.is_some(),
            metrics: self.metrics.lock().unwrap().clone(),
            indexing,
            reindexing: snapshot.into(),
            reindexing_total: snapshot.total,
        })
    }

    /// Stats when the database handle is unavailable because the swap holds it.
    /// Reports real progress counters instead of fabricating zeros.
    pub fn stats_swapping(snapshot: ProgressSnapshot, metrics: DatabaseMetrics) -> DatabaseStats {
        DatabaseStats {
            database_state: DatabaseState::Swapping,
            document_count: 0,
            segment_count: 0,
            background_compaction_enabled: false,
            metrics,
            indexing: IndexingStats {
                total_documents_indexed: snapshot.done,
                ..Default::default()
            },
            reindexing: snapshot.into(),
            reindexing_total: snapshot.total,
        }
    }

    //SMART SHIIT: if database running update the policy if not just validate->update file
    pub fn set_policy(&mut self, policy: IndexPolicy) -> io::Result<()> {
        policy.validate()?;
        if self.db.is_some() {
            self.db_mut()?.set_policy(policy.clone())?;
        }
        self.policy = policy;
        self.save_policy()?;
        info!(self.log, "policy set");
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.db_mut()?.flush()
    }

    pub fn policy(&self) -> &IndexPolicy {
        &self.policy
    }

    pub fn policy_path(&self) -> &Path {
        &self.policy_path
    }

    pub fn save_policy(&self) -> io::Result<()> {
        self.policy.save(&self.policy_path)?;
        Ok(())
    }

    pub fn reload_policy(&mut self) -> io::Result<()> {
        let policy = IndexPolicy::load(&self.policy_path)?;
        self.db_mut()?.set_policy(policy.clone())?;
        self.policy = policy;
        info!(self.log, "policy reloaded");
        Ok(())
    }

    // ---------------------------------------------------------------- reindex

    /// Cheap accessor so the actor can clone what staging needs.
    pub fn reindex_params(&self) -> ReindexParams {
        ReindexParams {
            root: self.root.clone(),
            policy: self.policy.clone(),
            options: self.options.clone(),
            analyzer: self.analyzer(),
            progress: self.progress(),
        }
    }

    /// Runs with NO lock held. Builds index.new from documents.bin.
    /// Returns the watermark: highest internal_id present when staging opened
    /// its snapshot of the store.
    ///
    /// INVARIANT: documents *updated* or *deleted* while this runs are not
    /// picked up -- the catch-up pass keys on internal_id and therefore only
    /// covers appends. Safe for append-only ingest; revisit before the upsert
    /// and delete endpoints see concurrent use during a reindex.
    pub fn build_staging_index(params: &ReindexParams) -> io::Result<DocId> {
        let temp_index_root = params.root.join("index.new");
        std::fs::remove_dir_all(&temp_index_root).ok();
        std::fs::create_dir_all(&temp_index_root)?;

        let read_store = BinaryDocumentStore::open(params.root.join("documents.bin"))?;
        let watermark = read_store.max_internal_id();

        let new_index =
            LsmIndex::persistent(&temp_index_root, params.options.runtime.flush_threshold)?;
        let mut staging = SearchDatabase::with_policy(
            read_store,
            new_index,
            params.analyzer.clone(),
            params.policy.clone(),
        );

        staging.reindex_existing_documents(
            params.options.runtime.indexing_batch_size,
            params.options.runtime.indexing_window_size,
            params.progress.as_ref(),
        )?;
        staging.shutdown_into_store()?;

        // Guard against a staging build that flushed nothing: renaming an empty
        // index over a working one is silent data loss.
        let bytes: u64 = std::fs::read_dir(&temp_index_root)?
            .filter_map(Result::ok)
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        if bytes == 0 && watermark > 0 {
            return Err(io::Error::other(
                "staging index is empty after build, refusing to swap",
            ));
        }

        Ok(watermark)
    }

    /// Runs WITH the lock held. Short: rename, catch up, install.
    ///
    /// Every failure path restores a working database: no exit leaves `self.db`
    /// as None.
    pub fn reindex_swap(&mut self, watermark: DocId) -> io::Result<()> {
        if self.progress.is_cancelled() {
            return Err(io::Error::other("reindex cancelled before swap"));
        }
        self.progress.set_phase(Phase::Swapping);
        // A leftover index.old from an earlier failed run makes the first
        // rename fail with ENOTEMPTY.
        std::fs::remove_dir_all(self.root.join("index.old")).ok();

        let result = self.reindex_swap_inner(watermark);

        match &result {
            Ok(()) => {
                self.progress.set_phase(Phase::Complete);
                std::fs::remove_dir_all(self.root.join("index.old")).ok();
                info!(self.log, "reindex complete");
            }
            Err(e) => {
                error!(self.log, "reindex swap failed"; "error" => %e);
                self.progress.set_phase(Phase::Failed);
                self.restore_after_failed_swap();
            }
        }
        result
    }

    fn reindex_swap_inner(&mut self, _watermark: DocId) -> io::Result<()> {
        let old_db = self
            .db
            .take()
            .ok_or_else(|| io::Error::other("database is closed"))?;
        let store = old_db.shutdown_into_store()?;

        let index_root = self.root.join("index");
        std::fs::rename(&index_root, self.root.join("index.old"))?;
        std::fs::rename(self.root.join("index.new"), &index_root)?;
        // Make the rename durable before anything else touches the directory.
        if let Ok(dir) = std::fs::File::open(&self.root) {
            let _ = dir.sync_all();
        }

        let analyzer = self.analyzer();
        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        let mut db = SearchDatabase::with_policy(store, index, analyzer, self.policy.clone());

        // The store's current count is the real denominator: it includes
        // anything appended while staging ran unlocked.
        self.progress.set_total(db.document_count() as u64);
        self.progress.set_phase(Phase::CatchUp);

        let ops = std::mem::take(&mut self.pending_ops);
        db.apply_pending(ops)?;
        db.flush()?;

        self.start_compaction_worker(&db);
        self.db = Some(db);
        Ok(())
    }

    /// Best effort: put index.old back if the new index never landed, then
    /// reopen from disk. `start()` already rebuilds index, store and worker.
    fn restore_after_failed_swap(&mut self) {
        let index_root = self.root.join("index");
        let old = self.root.join("index.old");

        if !index_root.exists()
            && old.exists()
            && let Err(e) = std::fs::rename(&old, &index_root)
        {
            error!(self.log, "could not restore index.old"; "error" => %e);
        }
        std::fs::remove_dir_all(self.root.join("index.new")).ok();

        if self.db.is_none() {
            match self.start() {
                Ok(()) => warn!(self.log, "database restored after failed reindex"),
                Err(e) => error!(self.log, "database did NOT recover after failed reindex";
                    "error" => %e),
            }
        }
        self.pending_ops.clear();
    }
    fn queue_op(&mut self, op: PendingOp) {
        if !self.progress.phase().is_running() {
            return;
        }
        if self.pending_ops.len() >= Self::MAX_PENDING_OPS {
            warn!(self.log, "pending mutation queue full, cancelling reindex");
            self.progress.request_cancel();
            self.pending_ops.clear();
            return;
        }
        self.pending_ops.push(op);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DatabaseStats {
    pub database_state: DatabaseState,
    pub document_count: usize,
    pub segment_count: usize,
    pub background_compaction_enabled: bool,
    pub metrics: DatabaseMetrics,
    pub indexing: IndexingStats,
    pub reindexing: ReindexingStats,
    /// Denominator behind `reindexing.progress`, so a client can show "n of m".
    pub reindexing_total: u64,
}
