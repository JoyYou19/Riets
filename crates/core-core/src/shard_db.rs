use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, PartialEq)]
pub struct DatabaseStats {
    pub database_state: DatabaseState,
    pub document_count: usize,
    pub segment_count: usize,
    pub background_compaction_enabled: bool,
    //pub metrics: DatabaseMetrics,
    pub indexing: IndexingStats,
    pub reindexing: ReindexingStats,
    pub reindexing_total: u64,
}

use core_index::{
    analyzer::Analyzer,
    document::IndexPolicy,
    lsm::{
        LsmIndex,
        index_worker::{IndexingStats, ReindexProgress, ReindexingStats},
        worker::CompactionWorker,
    },
    types::{DocId, ShardId},
};
use core_protocol::{
    command_reponse_definitions::{LookupCommand, LookupResponse},
    errors::CorelamoError,
};
use core_query::{Query, query_string_parser::parse_and_analyze};

use core_storage::{
    binary_store::BinaryDocumentStore,
    document_store::StoredDocument,
    search_database::{
        DocumentInput, IndexMode, InsertReport, PendingOp, SearchDatabase, SearchDocumentHit,
    },
    wal::{Wal, WalRecord},
};

use crate::{metrics::DatabaseMetrics, options::DatabaseOptions};
use core_logs::logger;
use slog::{Logger, error, info, warn};

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

pub struct ShardDb {
    shard_id: ShardId,
    root: PathBuf,

    policy: IndexPolicy,
    options: DatabaseOptions,

    //each shard kip holds this yeye
    db: Option<SearchDatabase<BinaryDocumentStore>>,
    compaction_worker: Option<CompactionWorker>,
    metrics: Mutex<DatabaseMetrics>,
    progress: Arc<ReindexProgress>,
    log: Logger,
    wal: Wal,
    pending_ops: Vec<PendingOp>,
}

impl ShardDb {
    //TODO: make configurable
    const MAX_PENDING_OPS: usize = 500_000;

    pub fn create_shard(
        root: impl AsRef<Path>,
        shard_id: ShardId,
        options: DatabaseOptions,
        policy: IndexPolicy,
    ) -> Result<Self, CorelamoError> {
        let root = root.as_ref().to_path_buf();
        let name = format!("shard-{}", shard_id);

        if root.exists() {
            return Err(CorelamoError::AlreadyExists(format!(
                "shard at {} already exists",
                root.display()
            )));
        }

        std::fs::create_dir_all(&root)?;

        let log = logger::db_logger(&root, &name);
        let wal = Wal::open(root.join("wal.log"), core_storage::wal::SyncMode::SyncEach)
            .map_err(|e| CorelamoError::Internal(format!("failed to open WAL: {e}")))?;

        let store_path = root.join("documents.bin");
        BinaryDocumentStore::open(&store_path)?;

        Ok(Self {
            shard_id,
            root,
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

    pub fn load(
        root: impl AsRef<Path>,
        policy: &IndexPolicy,
        options: &DatabaseOptions,
    ) -> Result<Self, CorelamoError> {
        let root = root.as_ref().to_path_buf();
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        if !root.exists() {
            return Err(CorelamoError::NotFound(format!(
                "shard not found at {}",
                root.display()
            )));
        }

        let shard_id = name
            .strip_prefix("shard-")
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| {
                CorelamoError::InvalidData(format!("invalid shard directory name: {}", name))
            })?;

        let log = logger::db_logger(&root, &name);
        let wal = Wal::open(root.join("wal.log"), core_storage::wal::SyncMode::SyncEach)
            .map_err(|e| CorelamoError::Internal(format!("failed to open WAL: {e}")))?;

        Ok(Self {
            shard_id: ShardId::from(shard_id),
            root,
            policy: policy.clone(),
            options: options.clone(),
            db: None,
            compaction_worker: None,
            metrics: Mutex::new(DatabaseMetrics::default()),
            progress: ReindexProgress::new(),
            log,
            wal,
            pending_ops: Vec::new(),
        })
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
        let mut db = SearchDatabase::with_shard_policy(
            store,
            index,
            analyzer,
            self.policy.clone(),
            self.shard_id,
        )?;

        let checkpoint = self
            .wal
            .read_checkpoint()
            .map_err(|e| CorelamoError::Internal(format!("failed to read WAL checkpoint: {e}")))?;
        let records = self
            .wal
            .replay_from(checkpoint)
            .map_err(|e| CorelamoError::Internal(format!("failed to replay WAL: {e}")))?;

        info!(self.log, "WAL recovery started";
            "shard_id" => self.shard_id,
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
                    info!(self.log, "WAL replay: Create"; "shard_id" => self.shard_id, "documents" => inputs.len());
                    let _ = db.put_documents_parallel(
                        inputs,
                        self.options.runtime.indexing_batch_size,
                        self.options.runtime.indexing_window_size,
                    );
                }
                WalRecord::Upsert(input) => {
                    info!(self.log, "WAL replay: Upsert"; "shard_id" => self.shard_id, "external_id" => &input.external_id);
                    db.upsert_document(input, IndexMode::StoreAndIndex)
                        .map_err(|e| {
                            CorelamoError::Internal(format!("recovery apply failed: {e}"))
                        })?;
                }
                WalRecord::Modify {
                    external_id,
                    payload,
                } => {
                    info!(self.log, "WAL replay: Modify"; "shard_id" => self.shard_id, "external_id" => external_id);
                    if let Some(doc) = payload.into_iter().next() {
                        db.upsert_document(doc, IndexMode::StoreAndIndex)
                            .map_err(|e| {
                                CorelamoError::Internal(format!("recovery apply failed: {e}"))
                            })?;
                    }
                }
                WalRecord::Delete { external_id } => {
                    let _ = db.delete_document(&external_id);
                    info!(self.log, "WAL replay: Delete"; "shard_id" => self.shard_id, "external_id" => external_id);
                }
                WalRecord::Clear => {}
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
        info!(self.log, "shard started"; "shard_id" => self.shard_id);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), CorelamoError> {
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
            info!(self.log, "compaction worker stopped"; "shard_id" => self.shard_id);
        }

        if let Some(db) = self.db.take() {
            db.shutdown()?;
            info!(self.log, "shard stopped"; "shard_id" => self.shard_id);
        }

        self.wal
            .reset()
            .map_err(|e| CorelamoError::Internal(format!("wal reset failed: {e}")))?;
        self.wal
            .write_checkpoint(0)
            .map_err(|e| CorelamoError::Internal(format!("checkpoint write failed: {e}")))?;

        info!(self.log, "WAL reset on clean shutdown"; "shard_id" => self.shard_id);
        Ok(())
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        self.stop()
            .map_err(|e| io::Error::other(format!("shutdown failed: {e}")))
    }

    pub fn restart(&mut self) -> Result<(), CorelamoError> {
        self.stop()?;
        self.start()?;
        info!(self.log, "shard restarted"; "shard_id" => self.shard_id);
        Ok(())
    }

    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    pub fn policy(&self) -> &IndexPolicy {
        &self.policy
    }

    pub fn options(&self) -> &DatabaseOptions {
        &self.options
    }

    pub fn is_running(&self) -> bool {
        self.db.is_some()
    }

    fn db_ref(&self) -> io::Result<&SearchDatabase<BinaryDocumentStore>> {
        self.db
            .as_ref()
            .ok_or_else(|| io::Error::other("shard is not running"))
    }

    fn db_mut(&mut self) -> io::Result<&mut SearchDatabase<BinaryDocumentStore>> {
        self.db
            .as_mut()
            .ok_or_else(|| io::Error::other("shard is not running"))
    }

    pub fn set_policy(&mut self, policy: IndexPolicy) -> Result<(), CorelamoError> {
        policy.validate()?;
        if let Some(db) = &mut self.db {
            db.set_policy(policy.clone())?;
        }
        self.policy = policy;
        info!(self.log, "policy set"; "shard_id" => self.shard_id);
        Ok(())
    }

    pub fn set_options(&mut self, options: DatabaseOptions) -> Result<(), CorelamoError> {
        self.options = options;
        Ok(())
    }

    fn analyzer(&self) -> Analyzer {
        Analyzer::new()
    }

    // ====== Read Operations ======

    pub fn search(&self, query: &Query, k: usize) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        let db = self
            .db_ref()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?;
        db.search_document_hits_all_fields_top_k(query, k)
            .map_err(|e| CorelamoError::Internal(e.to_string()))
    }

    pub fn get_document(
        &self,
        ids: &[String],
    ) -> Result<Vec<(String, Option<StoredDocument>)>, CorelamoError> {
        let db = self
            .db_ref()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let doc = db
                .get_document(id)
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
            out.push((id.clone(), doc));
        }
        Ok(out)
    }

    pub fn lookup(&self, command: &LookupCommand) -> Result<LookupResponse, CorelamoError> {
        let db = self
            .db_ref()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?;
        db.lookup_documents(&command.ids, command.return_fields.as_ref())
            .map_err(CorelamoError::from)
    }

    pub fn build_query(&self, input: &str) -> Result<Option<Query>, CorelamoError> {
        let db = self
            .db_ref()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?;
        parse_and_analyze(input, db.get_analyzer())
    }

    // pub fn search_command(
    //     &self,
    //     command: &SearchCommand,
    // ) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
    //     let limit = command.docs.unwrap_or(10);
    //     let offset = command.offset.unwrap_or(0);
    //
    //     let Some(query) = self.build_query(&command.query)? else {
    //         return Ok(Vec::new());
    //     };
    //
    //     let plan = QueryPlanner::plan(query);
    //     self.search_plan(&plan, command.return_fields.as_ref(), offset, limit)
    // }
    //
    // pub fn search_plan(
    //     &self,
    //     plan: &QueryPlan,
    //     return_fields: Option<&IndexMap<String, bool>>,
    //     offset: usize,
    //     limit: usize,
    // ) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
    //     let db = self
    //         .db_ref()
    //         .map_err(|e| CorelamoError::Internal(e.to_string()))?;
    //     db.search_document_hits_plan(plan, return_fields, offset, limit)
    //         .map_err(|e| CorelamoError::Internal(e.to_string()))
    // }

    // ====== Write Operations ======

    pub fn insert(&mut self, inputs: Vec<DocumentInput>) -> Result<InsertReport, CorelamoError> {
        //stats
        let started = std::time::Instant::now();
        let count = inputs.len();
        let batch_size = self.options.runtime.indexing_batch_size;
        let window_size = self.options.runtime.indexing_window_size;

        let ids: Vec<String> = if self.progress.phase().is_running() {
            inputs.iter().map(|i| i.external_id.clone()).collect()
        } else {
            Vec::new()
        };

        let result = (|| -> Result<InsertReport, CorelamoError> {
            //WALis
            let record = WalRecord::Create(inputs.clone());
            let encoded = bincode::encode_to_vec(&record, bincode::config::standard())
                .map_err(|e| CorelamoError::Internal(format!("wal encode failed: {e}")))?;
            let offset = self
                .wal
                .append(&encoded)
                .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;

            info!(self.log, "WAL append";
                "operation" => "create",
                "shard_id" => %self.shard_id,
                "documents" => inputs.len(),
                "offset" => offset,
                "durable_offset" => self.wal.durable_offset(),
            );

            let db = self
                .db_mut()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
            let report = db
                .put_documents_parallel(inputs, batch_size, window_size)
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
            // NOTE: this flush is why segment count tracked HTTP request count
            // exactly. Durability is already guaranteed by stop/shutdown; drop
            // this call if you would rather let the memtable threshold govern.
            db.flush()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
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

        match &result {
            Ok(_) => info!(self.log, "indexed batch";
                "shard_id" => %self.shard_id,
                "documents" => count,
                "batch_size" => batch_size,
                "elapsed_ms" => elapsed.as_millis(),
            ),
            Err(e) => error!(self.log, "indexing failed";
                "shard_id" => %self.shard_id,
                "documents" => count,
                "batch_size" => batch_size,
                "elapsed_ms" => elapsed.as_millis(),
                "error" => %e,
            ),
        }

        result
    }

    //
    // pub fn delete(&mut self, external_id: &str) -> Result<(), CorelamoError> {
    //     let record = WalRecord::Delete {
    //         external_id: external_id.to_string(),
    //     };
    //     let encoded = bincode::encode_to_vec(&record, bincode::config::standard())
    //         .map_err(|e| CorelamoError::Internal(format!("wal encode failed: {e}")))?;
    //     self.wal
    //         .append(&encoded)
    //         .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;
    //
    //     let old = self
    //         .db_mut()
    //         .map_err(|e| CorelamoError::Internal(e.to_string()))?
    //         .get_document(external_id)
    //         .map_err(|e| CorelamoError::Internal(e.to_string()))?
    //         .map(|d| d.internal_id);
    //
    //     self.db_mut()
    //         .map_err(|e| CorelamoError::Internal(e.to_string()))?
    //         .delete_document(external_id)
    //         .map_err(|e| CorelamoError::Internal(e.to_string()))?;
    //
    //     if let Some(internal_id) = old {
    //         self.queue_op(PendingOp::Tombstone { internal_id });
    //     }
    //
    //     Ok(())
    // }
    //
    // pub fn upsert(&mut self, input: DocumentInput) -> Result<(), CorelamoError> {
    //     let record = WalRecord::Upsert(input.clone());
    //     let encoded = bincode::encode_to_vec(&record, bincode::config::standard())
    //         .map_err(|e| CorelamoError::Internal(format!("wal encode failed: {e}")))?;
    //     self.wal
    //         .append(&encoded)
    //         .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;
    //
    //     let external = input.external_id.clone();
    //     let old = self
    //         .db_mut()
    //         .map_err(|e| CorelamoError::Internal(e.to_string()))?
    //         .get_document(&external)
    //         .map_err(|e| CorelamoError::Internal(e.to_string()))?
    //         .map(|d| d.internal_id);
    //
    //     self.db_mut()
    //         .map_err(|e| CorelamoError::Internal(e.to_string()))?
    //         .upsert_document(input, IndexMode::StoreAndIndex)
    //         .map_err(|e| CorelamoError::Internal(e.to_string()))?;
    //
    //     if let Some(internal_id) = old {
    //         self.queue_op(PendingOp::Tombstone { internal_id });
    //     }
    //
    //     let queued = {
    //         let db = self
    //             .db_mut()
    //             .map_err(|e| CorelamoError::Internal(e.to_string()))?;
    //         db.get_document(&external)
    //             .map_err(|e| CorelamoError::Internal(e.to_string()))?
    //             .map(|doc| db.to_indexed(&doc))
    //     };
    //     if let Some(doc) = queued {
    //         self.queue_op(PendingOp::Index { doc });
    //     }
    //
    //     Ok(())
    // }

    pub fn flush(&mut self) -> Result<(), CorelamoError> {
        self.db_mut()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?
            .flush()
            .map_err(|e| CorelamoError::Internal(e.to_string()))
    }

    pub fn progress(&self) -> Arc<ReindexProgress> {
        Arc::clone(&self.progress)
    }

    pub fn document_count(&self) -> usize {
        self.db.as_ref().map(|d| d.document_count()).unwrap_or(0)
    }

    pub fn segment_count(&self) -> Result<usize, CorelamoError> {
        self.db_ref()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?
            .segment_count()
            .map_err(|e| CorelamoError::Internal(e.to_string()))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn queue_op(&mut self, op: PendingOp) {
        if !self.progress.phase().is_running() {
            return;
        }
        if self.pending_ops.len() >= Self::MAX_PENDING_OPS {
            warn!(self.log, "pending mutation queue full, cancelling reindex"; "shard_id" => self.shard_id);
            self.progress.request_cancel();
            self.pending_ops.clear();
            return;
        }
        self.pending_ops.push(op);
    }
}
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<ShardDb>();
};
