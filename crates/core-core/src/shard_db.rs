use core_backup::{ backup::{ BackupManager, BackupManifest }, progress::BackupStats };
use core_index::{
    analyzer::Analyzer,
    document::IndexPolicy,
    lsm::{ LsmIndex, index_worker::{ IndexingStats, ReindexProgress, ReindexingStats } },
    types::ShardId,
};
use core_protocol::{
    command_reponse_definitions::{ LookupCommand, LookupResponse },
    errors::{ CorelamoError, DocFailure, FailReason },
};
use core_timing::timed;
use std::{ collections::VecDeque, io, path::{ Path, PathBuf }, sync::Arc, time::{ Duration, Instant, SystemTime } };

use crate::{
    background_worker::{BackgroundConfig, BackgroundWorker}, metrics::{ BackgroundActivity, BackgroundStats, DatabaseMetrics, DbStats, ShardStatsHandle }, options::DatabaseOptions, reindex::{ CompletedShardReindex, ReindexParams, ReindexTask }, shard_worker::ShardCmd, shared_state::SharedShardState,
};
use core_embed::ModelRegistry;
use core_logs::logger;
use core_storage::{
    binary_store::{
        BinaryDocumentStore,
        CompletedSegmentCompaction,
        DEFAULT_SEGMENT_SIZE,
        SegmentCompactionJob,
        save_maps,
    }, document_store::StoredDocument, search_database::{
        DeleteReport, DocumentInput, FlushTicket, IndexMode, InsertReport, ReplaceReport, SearchDatabase,
    }, wal::{ SyncMode, Wal, WalRecord, WalRecordRef},
};

use crate::embed::EmbeddingJob;
use slog::{ Logger, error, info, warn };

#[derive(Debug, Clone, PartialEq)]
pub struct DatabaseStats {
    pub document_count: usize,
    pub segment_count: usize,
    pub background_compaction_enabled: bool,
    pub metrics: DatabaseMetrics,
    pub indexing: IndexingStats,
    pub reindexing: ReindexingStats,
    pub backup: BackupStats,
    pub background: BackgroundStats,
    pub shard_count: usize,
    pub restoring: bool,
}
//has it been quiet long enough to flush
pub(crate) const IDLE_FLUSH_AFTER: Duration = Duration::from_secs(2);
pub struct ShardDb {
    shard_id: ShardId,
    shard_count: u16,
    root: PathBuf,
    policy: IndexPolicy,
    options: DatabaseOptions,
    db: Option<SearchDatabase<BinaryDocumentStore>>,
    background_worker: Option<BackgroundWorker>,
    models: Arc<ModelRegistry>,
    stats: ShardStatsHandle,
    log: Logger,
    wal: Wal,
    generation: u64,
    shared: Arc<SharedShardState>,
    backup: BackupManager,
    pending: u32,
    last_write: Instant,
    failed: bool,
    /// Checkpoints started but not yet finished, oldest first. At most one is in flight.
    pending_checkpoints: VecDeque<PendingCheckpoint>,
    command_tx:Option<crossbeam_channel::Sender<ShardCmd>>,
    //inserts during reindex 
    wal_reindex:Option<u64>
}
struct PendingCheckpoint{
    through_segment:u64,
    ticket:FlushTicket
}
 fn open_wal(root: &Path, mode: SyncMode) -> io::Result<Wal> {
    Wal::open(root.join("wal"), mode)
}
impl ShardDb {
    pub fn shared_state(&self) -> Arc<SharedShardState> {
        self.shared.clone()
    }

    #[timed(database_lifecycle)]
    pub fn create_shard(
        root: impl AsRef<Path>,
        db_root: impl AsRef<Path>,
        shard_id: ShardId,
        shard_count: u16,
        options: DatabaseOptions,
        policy: IndexPolicy,
        stats: ShardStatsHandle,
        models: Arc<ModelRegistry>,
    ) -> Result<Self, CorelamoError> {
        let root = root.as_ref().to_path_buf();
        let name = format!("shard-{}", shard_id);

        if root.exists() {
            return Err(
                CorelamoError::AlreadyExists(format!("shard at {} already exists", root.display()))
            );
        }

        std::fs::create_dir_all(&root)?;

        let log = logger::shard_logger(&root, &name);
               let wal = open_wal(&root, options.sync_mode).map_err(|e|
            CorelamoError::Internal(format!("failed to open WAL: {e}"))
        )?;
        let store_path = root.join("documents");
        BinaryDocumentStore::open(&store_path)?;
        let backup_dir = db_root.as_ref().join("backups");

        std::fs::create_dir_all(&backup_dir)?;
        let backup = BackupManager::new(&root, backup_dir, name.clone(), 0, 0);
        Ok(Self {
            shard_id,
            shard_count,
            shared: Arc::new(SharedShardState::new(root.clone())),
            policy,
            options,
            db: None,
            background_worker: None,
            models,
            log,
            wal,
            generation: 0,
            stats,
            root,
            backup,
            pending: 0,
            last_write: Instant::now(),
            failed: false,
            pending_checkpoints:VecDeque::new(),
            command_tx:None,
            wal_reindex:None
        })
    }

    #[timed(database_lifecycle)]
    pub fn load(
        root: impl AsRef<Path>,
        db_root: impl AsRef<Path>,
        shard_count: u16,
        policy: &IndexPolicy,
        options: &DatabaseOptions,
        stats: ShardStatsHandle,
        models: Arc<ModelRegistry>
    ) -> Result<Self, CorelamoError> {
        let root = root.as_ref().to_path_buf();
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        if !root.exists() {
            return Err(CorelamoError::NotFound(format!("shard not found at {}", root.display())));
        }

        let shard_id = name
            .strip_prefix("shard-")
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| {
                CorelamoError::InvalidData(format!("invalid shard directory name: {}", name))
            })?;

        let log = logger::shard_logger(&root, &name);
        // load
        let wal = open_wal(&root, options.sync_mode).map_err(|e|
            CorelamoError::Internal(format!("failed to open WAL: {e}"))
        )?;
        let backup_dir = db_root.as_ref().join("backups");
        std::fs::create_dir_all(&backup_dir)?;
        let backup = BackupManager::new(&root, backup_dir, name.clone(), 0, 0);
        Ok(Self {
            shard_id: ShardId::from(shard_id),
            shard_count,
            shared: Arc::new(SharedShardState::new(root.clone())),
            root,
            policy: policy.clone(),
            options: options.clone(),
            db: None,
            background_worker: None,
            models,
            stats,
            log,
            wal,
            generation: 0,
            pending: 0,
            backup,
            last_write: Instant::now(),
            failed: false,
            pending_checkpoints:VecDeque::new(),
            command_tx:None,
            wal_reindex:None
        })
    }

    #[timed(database_lifecycle)]
    pub fn start(&mut self) -> Result<(), CorelamoError> {
        if self.db.is_some() {
            return Ok(());
        }
        if self.failed {
           self.wal = open_wal(&self.root, self.options.sync_mode).map_err(|e|
                CorelamoError::Internal(format!("failed to reopen WAL: {e}"))
            )?;
        }
        self.failed=false;
        // Decoded before anything is opened: a Clear in the WAL means a clear that crashed
        // halfway, and it is finished first, so only what was written after it comes back.
        let mut records = Vec::new();
        for payload in self.wal
            .replay_all()
            .map_err(|e| CorelamoError::Internal(format!("failed to replay WAL: {e}")))? {
            let (record, _): (WalRecord, usize) = bincode
                ::decode_from_slice(&payload, bincode::config::standard())
                .map_err(|e| CorelamoError::Internal(format!("failed to decode WAL record: {e}")))?;
            records.push(record);
        }
        if let Some(last_clear) = records.iter().rposition(|r| matches!(r, WalRecord::Clear)) {
            info!(self.log, "WAL replay: finishing an interrupted clear";
                "shard_id" => self.shard_id, "records_dropped" => last_clear + 1);
            self.remove_data_files();
            records.drain(..=last_clear);
        }
        let index_root = self.root.join("index");
        let store_path = self.root.join("documents");
        let analyzer = self.analyzer();

        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        let store = BinaryDocumentStore::open_with_maps(
            &store_path,
            self.shared.docs.clone(),
            self.shared.internal_to_external.clone(),
            self.shared.locations.clone()
        )?;

        let mut db = SearchDatabase::with_shard_policy_and_snapshot(
            store,
            index,
            analyzer,
            self.policy.clone(),
            self.shard_id,
            self.shared.snapshot.clone()
        )?;

        self.pending_checkpoints.clear();
        
        let replayed_any = !records.is_empty();

        info!(self.log, "WAL recovery started";
            "shard_id" => self.shard_id,
            "wal_segment" => self.wal.current_segment(),
            "durable_offset" => self.wal.durable_offset(),
            "records_to_replay" => records.len(),
        );

        for record in records {
            match record {
                WalRecord::Create(inputs) => {
                    info!(self.log, "WAL replay: Create"; "shard_id" => self.shard_id, "documents" => inputs.len());
                    // The store may already hold documents of this record whose index entries were
                    // lost (stored, but the memtable was not flushed before the crash). A create
                    // would reject them as duplicates, so they are indexed again with upsert. A
                    // stored document with a different source is one the original create rejected
                    // as a duplicate, or one a later record of the WAL changed: it is left as it is.
                    let mut fresh = Vec::with_capacity(inputs.len());
                    let mut kept_existing = 0usize;
                    for input in inputs {
                        let stored = db
                            .get_document(&input.external_id)
                            .map_err(|e| {
                                CorelamoError::Internal(format!("recovery create failed: {e}"))
                            })?;
                        match stored {
                            None => fresh.push(input),
                            Some(doc) if *doc.source == *input.source => {
                                db
                                    .upsert_document(input, IndexMode::StoreAndIndex)
                                    .map_err(|e| {
                                        CorelamoError::Internal(
                                            format!("recovery create failed: {e}")
                                        )
                                    })?;
                            }
                            Some(_) => {
                                kept_existing += 1;
                            }
                        }
                    }
                    let report = db
                        .put_documents_parallel(fresh, self.options.runtime.indexing_batch_size)
                        .map_err(|e| {
                            CorelamoError::Internal(format!("recovery create failed: {e}"))
                        })?;
                    if kept_existing > 0 || !report.failures.is_empty() {
                        info!(self.log, "WAL replay: Create skipped documents";
                            "shard_id" => self.shard_id,
                            "kept_existing" => kept_existing,
                            "rejected" => report.failures.len(),
                        );
                    }
                }
                WalRecord::Upsert(inputs) => {
                    info!(self.log, "WAL replay: Upsert";"shard_id" => self.shard_id, "documents" => inputs.len());
                    for input in inputs {
                        db
                            .upsert_document(input, IndexMode::StoreAndIndex)
                            .map_err(|e| {
                                CorelamoError::Internal(format!("recovery apply failed: {e}"))
                            })?;
                    }
                }
                // WalRecord::Modify { external_id, payload } => {
                //     info!(self.log, "WAL replay: Modify"; "shard_id" => self.shard_id, "external_id" => external_id);
                //     if let Some(doc) = payload.into_iter().next() {
                //         db
                //             .upsert_document(doc, IndexMode::StoreAndIndex)
                //             .map_err(|e| {
                //                 CorelamoError::Internal(format!("recovery apply failed: {e}"))
                //             })?;
                //     }
                // }
                WalRecord::Replace(inputs) => {
                    info!(self.log,"WAL replay: Replace"; "shard_id" => self.shard_id,"documents" => inputs.len());
                    for input in inputs {
                        db
                            .upsert_document(input, IndexMode::StoreAndIndex)
                            .map_err(|e| {
                                CorelamoError::Internal(format!("recovery replace failed:{e}"))
                            })?;
                    }
                }
                WalRecord::Delete(ids) => {
                    info!(self.log, "WAL replay: Delete"; "shard_id" => self.shard_id, "documents" => ids.len());
                    for id in ids {
                        let _ = db.delete_document(&id);
                    }
                }
                WalRecord::Clear => {}
            }
        }
        if replayed_any{
            self.pending=self.pending.max(1);
        }
        self.db = Some(db);
        self.start_background();
        self.publish_stats();
        self.last_write = Instant::now();
        self.report_misplaced_documents();
        info!(self.log, "shard started"; "shard_id" => self.shard_id);
        Ok(())
    }

    /// Logs documents stored in this shard that `shard_for` routes to another shard.
    /// Retrieve, lookup, upsert, replace and delete never look for them here, and inserting
    /// the same id again creates a second copy elsewhere, so this must report nothing.
    fn report_misplaced_documents(&self) {
        const EXAMPLE_LIMIT: usize = 5;

        let mut misplaced = 0usize;
        let mut examples: Vec<String> = Vec::with_capacity(EXAMPLE_LIMIT);
        for entry in self.shared.locations.iter() {
            if crate::shard_for_untimed(entry.key(), self.shard_count) != self.shard_id.0 {
                misplaced += 1;
                if examples.len() < EXAMPLE_LIMIT {
                    examples.push(entry.key().clone());
                }
            }
        }

        if misplaced > 0 {
            error!(self.log, "documents are stored in the wrong shard and cannot be reached by id";
                "shard_id" => self.shard_id,
                "shard_count" => self.shard_count,
                "misplaced" => misplaced,
                "stored" => self.shared.locations.len(),
                "examples" => format!("{examples:?}"),
            );
        }
    }

    #[timed(database_lifecycle)]
    pub fn stop(&mut self) -> Result<(), CorelamoError> {
        if self.db.is_none() {
            return Ok(());
        }

        if let Some(worker) = self.background_worker.take() {
            worker.stop()?;
            info!(self.log, "background worker stopped"; "shard_id" => self.shard_id);

        }
        self.stats.set_activity(BackgroundActivity::Idle);
        self.wal_reindex=None;
        if let Err(e) = self.wait_checkpoints() {
            warn!(self.log, "checkpoint failed during stop; WAL kept for replay";
                "shard_id" => self.shard_id, "error" => %e);
        }
        if let Some(db) = self.db.take() {
            db.shutdown()?;
            info!(self.log, "shard stopped"; "shard_id" => self.shard_id);
        }

        let store_path = self.root.join("documents");
        save_maps(&store_path, &self.shared.locations).map_err(|e|
            CorelamoError::Internal(format!("failed to save doc maps: {e}"))
        )?;

        if !self.failed {
            self.wal
                .reset()
                .map_err(|e| CorelamoError::Internal(format!("wal reset failed: {e}")))?;
        }
        self.shared.release();

        info!(self.log, "WAL reset on clean shutdown"; "shard_id" => self.shard_id);
        self.stats.set_compaction_enabled(false);
        Ok(())
    }

    #[timed(database_lifecycle)]
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.stop().map_err(|e| io::Error::other(format!("shutdown failed: {e}")))
    }

    #[timed(database_lifecycle)]
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
        self.db.as_ref().ok_or_else(|| io::Error::other("shard is not running"))
    }

    fn db_mut(&mut self) -> io::Result<&mut SearchDatabase<BinaryDocumentStore>> {
        self.db.as_mut().ok_or_else(|| io::Error::other("shard is not running"))
    }

    #[timed(database_lifecycle)]
    pub fn set_policy(&mut self, policy: IndexPolicy, user: String) -> Result<(), CorelamoError> {
        policy.validate()?;
          if self.wal_reindex.is_some() {
            return Err(CorelamoError::Busy("a reindex is running on this shard".into()));
        }
        if let Some(db) = &mut self.db {
            db.set_policy(policy.clone())?;
        }
        self.policy = policy;
        if self.db.is_some() {
            self.start_background();
        }
        info!(self.log, "policy set"; "shard_id" => self.shard_id, "user" => user);
        Ok(())
    }

    #[timed(database_lifecycle)]
    pub fn set_options(
        &mut self,
        options: DatabaseOptions,
        user: String
    ) -> Result<(), CorelamoError> {
          if self.wal_reindex.is_some() {
            return Err(CorelamoError::Busy("a reindex is running on this shard".into()));
        }
        info!(self.log, "Options set"; "User"=>user);
        self.options = options;
        Ok(())
    }

    fn analyzer(&self) -> Analyzer {
        Analyzer::new()
    }

    pub fn apply_config(&mut self, policy: IndexPolicy, options: DatabaseOptions) {
        self.policy = policy;
        self.options = options;
        if self.db.is_some() {
            self.start_background();
        }
    }

    // ====== Read Operations ======

    #[timed(retrieve_opps)]
    pub fn get_document(
        &self,
        ids: &[String]
    ) -> Result<Vec<(String, Option<StoredDocument>)>, CorelamoError> {
        let db = self.db_ref().map_err(|e| CorelamoError::Internal(e.to_string()))?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let doc = db.get_document(id).map_err(|e| CorelamoError::Internal(e.to_string()))?;
            out.push((id.clone(), doc));
        }
        Ok(out)
    }

    #[timed(retrieve_opps)]
    pub fn lookup(&self, command: &LookupCommand) -> Result<LookupResponse, CorelamoError> {
        let db = self.db_ref().map_err(|e| CorelamoError::Internal(e.to_string()))?;
        db.lookup_documents(&command.ids, command.return_fields.as_ref()).map_err(
            CorelamoError::from
        )
    }

    // =========write operations =========
    #[timed(wal)]
    fn wal_append_record(&mut self, record: &WalRecordRef<'_>) -> Result<u64, CorelamoError> {
        let encoded = bincode
            ::encode_to_vec(record, bincode::config::standard())
            .map_err(|e| CorelamoError::Internal(format!("wal encode failed: {e}")))?;
        self.wal
            .append(&encoded)
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))
    }

   
    fn ensure_running(&self) -> Result<(), CorelamoError> {
        if self.db.is_none() {
            return Err(
                CorelamoError::DatabaseNotRunning(format!("shard {} is not running", self.shard_id))
            );
        }
        if self.failed {
            return Err(
                CorelamoError::Internal(
                    format!(
                        "shard {} failed a durability step; restart it to replay the WAL",
                        self.shard_id
                    )
                )
            );
        }
        Ok(())
    }

    #[timed(inserting)]
    pub fn insert(
        &mut self,
        inputs: Vec<DocumentInput>,
        user: String
    ) -> Result<InsertReport, CorelamoError> {
        self.ensure_running()?;
        let started = Instant::now();
        let count = inputs.len();
        let total_bytes: u64 = inputs
            .iter()
            .map(|d| d.source.len() as u64)
            .sum();
        core_timing::add_bytes("inserting", "insert", file!(), total_bytes);
        let batch_size = self.options.runtime.indexing_batch_size;

        // let window_size = self.options.runtime.indexing_window_size;
        self.pending += count as u32;
        let should_checkpoint = self.pending >= (batch_size as u32);
        let wal_record = WalRecord::Create(inputs);
        // self.wal_append_record(&wal_record)?;
        let WalRecord::Create(inputs) = wal_record else {
            return Err(CorelamoError::Internal("unexpected WAL record".into()));
        };
        let result = (|| -> Result<InsertReport, CorelamoError> {
            self.wal_append_record(&WalRecordRef::Create(&inputs))?;
            let db = self.db_mut().map_err(|e| CorelamoError::Internal(e.to_string()))?;
            let report = db.put_documents_parallel(inputs, batch_size)?; // moved, never cloned

            if should_checkpoint {
                self.start_checkpoint()?;
            }


            Ok(report)
        })();
        let elapsed = started.elapsed();
        match &result {
            Ok(report) => {
                self.stats.add_documents_indexed(report.inserted as u64);
                self.publish_stats();
                self.last_write = Instant::now();
                info!(self.log, "indexed batch";
                    "shard_id" => %self.shard_id,
                    "documents" => count,
                    "batch_size" => batch_size,
                    "elapsed_ms" => elapsed.as_millis(),
                    "user"=> user,
                );
            }
            Err(e) => {
                error!(self.log, "indexing failed";
                    "shard_id" => %self.shard_id,
                    "documents" => count,
                    "batch_size" => batch_size,
                    "elapsed_ms" => elapsed.as_millis(),
                    "error" => %e,
                    "user" => user,
                );
            }
        }
        result
    }

    pub fn get_logs(&self, date: Option<String>) -> Result<String, CorelamoError> {
        let logs_dir = self.root.join("logs");
        if !logs_dir.exists() {
            return Ok(String::new());
        }

        let mut files: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&logs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "log") {
                if let Some(ref date_str) = date {
                    if
                        path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|name| name.contains(date_str))
                    {
                        files.push(path);
                    }
                } else {
                    files.push(path);
                }
            }
        }

        files.sort();

        let mut output = String::new();
        for file_path in files {
            let content = std::fs::read_to_string(&file_path)?;
            output.push_str(&content);
            if !content.ends_with('\n') {
                output.push('\n');
            }
        }

        Ok(output)
    }

    pub fn clear_logs(&self) -> Result<(), CorelamoError> {
        let logs_dir = self.root.join("logs");
        if !logs_dir.exists() {
            return Ok(());
        }

        for entry in std::fs::read_dir(&logs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "log") {
                std::fs::remove_file(&path)?;
            }
        }

        info!(self.log, "logs cleared"; "shard_id" => self.shard_id);
        Ok(())
    }

    #[timed(modifying_database_documents)]
    pub fn delete(
        &mut self,
        ids: Vec<String>,
        user: String
    ) -> Result<DeleteReport, CorelamoError> {
        self.ensure_running()?;
        let started = Instant::now();
        let count = ids.len();

        self.pending += count as u32;
        let batch_size = self.options.runtime.indexing_batch_size;
        let should_checkpoint = self.pending >= (batch_size as u32);

        let mut deleted: u32 = 0;
        let mut failures: Vec<DocFailure> = Vec::new();

        let _offset = self
            .wal_append_record(&WalRecordRef::Delete(&ids))
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;
        {
            let db = self.db_mut().map_err(|e| CorelamoError::Internal(e.to_string()))?;

            for id in ids {
                let existing = match db.get_document(&id) {
                    Ok(doc) => doc,
                    Err(e) => {
                        failures.push(
                            DocFailure::new(
                                None,
                                Some(id.clone()),
                                FailReason::Internal(e.to_string())
                            )
                        );
                        continue;
                    }
                };

                let Some(_existing) = existing else {
                    failures.push(DocFailure::new(None, Some(id.clone()), FailReason::NotFound));
                    continue;
                };

                match db.delete_document(&id) {
                    Ok(()) => {
                        deleted += 1;
                    }
                    Err(e) => {
                        failures.push(
                            DocFailure::new(
                                None,
                                Some(id.clone()),
                                FailReason::Internal(e.to_string())
                            )
                        );
                    }
                }
            }
        }
        if should_checkpoint {
            self.start_checkpoint()?;
        }
        self.publish_stats();
        self.last_write = Instant::now();
        let elapsed = started.elapsed();
        info!(self.log, "delete batch";
            "user" => user.clone(),
            "shard_id" => %self.shard_id,
            "requested" => count,
            "deleted" => deleted,
            "failed" => failures.len(),
            "elapsed_ms" => elapsed.as_millis(),
        );
        Ok(DeleteReport { deleted, failures })
    }

    #[timed(reindex)]

    pub fn prepare_reindex(
        &mut self,
        progress: Arc<ReindexProgress>,
        stats: Arc<DbStats>,
    ) -> Result<(), CorelamoError> {
        if !self.is_running() {
            return Err(CorelamoError::DatabaseNotRunning(format!("shard {} is not running", self.shard_id)));
        }
        if self.background_worker.is_none() {
            return Err(CorelamoError::Internal(format!("shard {} has no background worker", self.shard_id)));
        }
        self.flush()?;
        self.generation += 1;
        self.wal_reindex= Some(self.generation);
        let params = ReindexParams {
            shard_id: self.shard_id,
            shard_root: self.root.clone(),
            policy: self.policy.clone(),
            options: self.options.clone(),
            wal_watermark: self.wal.durable_offset(),
            doc_count: self.document_count(),
            generation: self.generation,
        };
        let submitted = match self.background_worker.as_ref() {
            Some(worker) => worker.submit_reindex(ReindexTask { params, progress, stats }),
            None => Err(CorelamoError::Internal(format!("shard {} has no background worker", self.shard_id))),
        };
        if submitted.is_err() {
            self.wal_reindex = None;
        }
        submitted
    }

    #[timed(reindex)]
    pub fn commit_reindex(&mut self, done: CompletedShardReindex) -> Result<(), CorelamoError> {
        if done.generation != self.generation {
            let _ = std::fs::remove_dir_all(&done.staging_root);
            return Err(CorelamoError::Conflict("reindex result is from a superseded run".into()));
        }

        let index_root = self.root.join("index");
        let old_root = self.root.join("index.old");

        // close the live index before touching its directory
        if let Some(db) = self.db.take() {
            db.shutdown().map_err(|e| CorelamoError::Internal(e.to_string()))?;
        }

        if old_root.exists() {
            std::fs::remove_dir_all(&old_root)?;
        }
        std::fs::rename(&index_root, &old_root)?;
        std::fs::rename(&done.staging_root, &index_root)?;

        // reopen against the new index
        self.start()?;
        self.wal_reindex=None;
        self.flush()?;
        std::fs::remove_dir_all(&old_root).ok();
        self.publish_stats();
        self.last_write = Instant::now();
        info!(self.log, "reindex committed";
            "shard_id" => %self.shard_id,
            "generation" => self.generation,
            "built_through" => done.built_through,
        );
        Ok(())
    }
    pub fn abondoned_reindex(&mut self, generation: u64) {
        if generation != self.generation {
            return;
        }
        self.wal_reindex = None;
        let _ = std::fs::remove_dir_all(self.root.join("index.new"));
        info!(self.log, "reindex abandoned"; "shard_id" => self.shard_id, "generation" => generation);
        // the worker that built it may be parked waiting for a commit that failed
        
    }

    #[timed(modifying_database_documents)]
    pub fn replace(
        &mut self,
        inputs: Vec<DocumentInput>,
        user: String
    ) -> Result<ReplaceReport, CorelamoError> {
        self.ensure_running()?;
        let started = Instant::now();
        let count = inputs.len();

        self.pending += count as u32;
        let batch_size = self.options.runtime.indexing_batch_size;
        let should_checkpoint = self.pending >= (batch_size as u32);

        let mut replaced: u32 = 0;
        let mut failures: Vec<DocFailure> = Vec::new();
        let mut old_internal_ids = Vec::new();
        let mut written_ids = Vec::new();

        let _offset = self
            .wal_append_record(&WalRecordRef::Replace(&inputs))
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;
        //vajag info log?
        {
            let db = self.db_mut().map_err(|e| CorelamoError::Internal(e.to_string()))?;

            for input in inputs {
                let external_id = input.external_id.clone();

                let old = match db.get_document(&external_id) {
                    Ok(doc) => doc,
                    Err(e) => {
                        failures.push(
                            DocFailure::new(
                                None,
                                Some(external_id.clone()),
                                FailReason::Internal(e.to_string())
                            )
                        );
                        continue;
                    }
                };

                let Some(old) = old else {
                    failures.push(DocFailure::new(None, Some(external_id), FailReason::NotFound));
                    continue;
                };

                match db.upsert_document(input, IndexMode::StoreAndIndex) {
                    Ok(()) => {
                        replaced += 1;
                        old_internal_ids.push(old.internal_id);
                        written_ids.push(external_id);
                    }
                    Err(e) => {
                        failures.push(
                            DocFailure::new(
                                None,
                                Some(external_id),
                                FailReason::Internal(e.to_string())
                            )
                        );
                    }
                }
            }
        }
        if should_checkpoint {
            self.start_checkpoint()?;
        }
        let elapsed = started.elapsed();
        info!(self.log, "replace batch";
            "user" => user.clone(),
            "shard_id" => %self.shard_id,
            "requested" => count,
            "replaced" => replaced,
            "failed" => failures.len(),
            "elapsed_ms" => elapsed.as_millis(),
        );

        Ok(ReplaceReport { replaced, failures })
    }

    #[timed(modifying_database_documents)]
    pub fn upsert(
        &mut self,
        inputs: Vec<DocumentInput>,
        user: String
    ) -> Result<InsertReport, CorelamoError> {
        self.ensure_running()?;
        let started = Instant::now();
        let count = inputs.len();

        self.pending += count as u32;
        let batch_size = self.options.runtime.indexing_batch_size;
        let should_checkpoint = self.pending >= (batch_size as u32);
        //TODO: batch support plzs
        let mut inserted: u32 = 0;
        let mut failures: Vec<DocFailure> = Vec::new();
        let mut old_internal_ids = Vec::new();
        let mut written_ids = Vec::new();
        let _offset = self
            .wal_append_record(&WalRecordRef::Upsert(&inputs))
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;
        //vajag info log?
        {
            let db = self.db_mut().map_err(|e| CorelamoError::Internal(e.to_string()))?;

            for input in inputs {
                let external_id = input.external_id.clone();

                let old = match db.get_document(&external_id) {
                    Ok(doc) => doc,
                    Err(e) => {
                        failures.push(
                            DocFailure::new(
                                None,
                                Some(external_id.clone()),
                                FailReason::Internal(e.to_string())
                            )
                        );
                        continue;
                    }
                };

                match db.upsert_document(input, IndexMode::StoreAndIndex) {
                    Ok(()) => {
                        inserted += 1;
                        if let Some(old_doc) = old {
                            old_internal_ids.push(old_doc.internal_id);
                        }
                        written_ids.push(external_id);
                    }
                    Err(e) => {
                        failures.push(
                            DocFailure::new(
                                None,
                                Some(external_id),
                                FailReason::Internal(e.to_string())
                            )
                        );
                    }
                }
            }
        }
        if should_checkpoint {
            self.start_checkpoint()?;
        }
        let elapsed = started.elapsed();
        info!(self.log, "upsert batch";
            "user" => user.clone(),
            "shard_id" => %self.shard_id,
            "requested" => count,
            "upserted" => inserted,
            "failed" => failures.len(),
            "elapsed_ms" => elapsed.as_millis(),
        );

        Ok(InsertReport { inserted, failures })
    }

       /// Explicit flush: everything written so far goes to segments, and the WAL segments that
    /// held it are deleted. Returns once all of it is on disk.
    pub fn flush(&mut self) -> Result<(), CorelamoError> {
        self.ensure_running()?;
        self.start_checkpoint()?;
        self.wait_checkpoints()?;
        self.last_write = Instant::now();
        Ok(())
    }

    pub fn progress(&self) -> Arc<ReindexProgress> {
        Arc::clone(self.stats.reindex_progress())
    }

    pub fn document_count(&self) -> usize {
        self.db
            .as_ref()
            .map(|d| d.document_count())
            .unwrap_or(0)
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
    pub fn plan_segment_compaction(
        &self,
        dead_ratio_threshold: f64
    ) -> io::Result<Option<SegmentCompactionJob>> {
        let db = self.db_ref().map_err(io::Error::other)?;
        self.publish_stats();

        db.store().plan_compaction(dead_ratio_threshold, DEFAULT_SEGMENT_SIZE)
    }

    pub fn install_segment_compaction_cmd(
        &mut self,
        completed: CompletedSegmentCompaction
    ) -> io::Result<bool> {
        let db = self.db_mut().map_err(io::Error::other)?;
        let result = db.mut_store().install_segment_compaction(completed)?;
        self.publish_stats();
        Ok(result)
    }

    #[timed(database_lifecycle)]
    pub fn clear(&mut self) -> Result<(), CorelamoError> {
        let was_running = self.db.is_some();
        // Written first so a crash mid-clear is completed on replay.
        self
            .wal_append_record(&WalRecordRef::Clear)
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;

        if let Some(worker) = self.background_worker.take() {
            match worker.stop() {
                Ok(()) => {
                    info!(self.log, "background worker stopped for clear"; "shard_id" => self.shard_id);
                }
                Err(e) =>
                    warn!(self.log, "background worker exited with error before clear";
                "shard_id" => self.shard_id, "error" => %e),
            }
        }
        self.pending_checkpoints.clear();
        self.wal_reindex=None;
        if let Some(db) = self.db.take() {
            if let Err(e) = db.shutdown() {
                warn!(self.log, "clear: shutdown of old database failed";
                "shard_id" => self.shard_id, "error" => %e);
            }
        }
        let index_root = self.root.join("index");
        let store_path = self.root.join("documents");
        self.remove_data_files();
        // outright WAL reset
       
        self.wal
                .reset()
                .map_err(|e| CorelamoError::Internal(format!("wal reset failed: {e}")))?;
        if !was_running {
            self.shared.release();
            info!(self.log, "shard cleared while stopped; not reopening"; "shard_id" => self.shard_id);
            return Ok(());
        }
        let analyzer = self.analyzer();
        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        let store = BinaryDocumentStore::open_with_maps(
            &store_path,
            self.shared.docs.clone(),
            self.shared.internal_to_external.clone(),
            self.shared.locations.clone()
        )?;
        let db = SearchDatabase::with_shard_policy_and_snapshot(
            store,
            index,
            analyzer,
            self.policy.clone(),
            self.shard_id,
            self.shared.snapshot.clone()
        )?;

        if let Some(worker) = self.background_worker.take() {
            match worker.stop() {
                Ok(()) => {
                    info!(self.log, "background worker stopped for clear"; "shard_id" => self.shard_id);
                }
                Err(e) =>
                    warn!(self.log, "background worker exited with error before clear";
                "shard_id" => self.shard_id, "error" => %e),
            }
        }

        self.db = Some(db);
        self.start_background();
        self.publish_stats();
        self.last_write = Instant::now();
        info!(self.log, "shard cleared"; "shard_id" => self.shard_id);
        Ok(())
    }

    #[timed(backup)]
    pub fn backup_full(
        &mut self,
        user: String,
        shard_backup_path: PathBuf,
        backup_id: String
    ) -> Result<BackupManifest, CorelamoError> {
        self.flush()?;

        if let Some(worker) = self.background_worker.take() {
            if let Err(err) = worker.stop() {
                warn!(self.log, "background worker exited with error before backup"; "error" => %err);
            }
        }
        // Stopping the worker would cancel the reindex it is building, and the index about to
        // be copied is the one the reindex replaces: skip this backup, the next one runs.
        if self.wal_reindex.is_some() {
            return Err(CorelamoError::Busy("a reindex is running; full backup skipped".into()));
        }
        let result = self.run_full_backup(&shard_backup_path, &backup_id);
        self.restart_background();

        match &result {
            Ok(_) =>
                info!(self.log, "Full backup made";
            "backup_id" => %backup_id, "user" => %user),
            Err(err) =>
                error!(self.log, "Full backup failed";
            "backup_id" => %backup_id, "user" => %user, "error" => ?err),
        }

        result
    }

    fn run_full_backup(
        &mut self,
        shard_backup_path: &PathBuf,
        backup_id: &String
    ) -> Result<BackupManifest, CorelamoError> {
         let record_count = self.wal
            .replay_all()
            .map_err(|e| CorelamoError::Internal(format!("failed to replay WAL: {e}")))?
            .len();
        let progress = self.stats.backup_progress().clone();
        self.backup
            .create_full_backup(
                &self.root,
                shard_backup_path,
                backup_id,
                &progress,
                self.document_count(),
                record_count
            )
            .map_err(|e| CorelamoError::Internal(e.to_string()))
    }

   
    /// Starts the background worker again after a reindex or backup paused it.
    fn restart_background(&mut self) {
        if self.background_worker.is_some() {
            return;
        }
        if self.db.is_none() {
            warn!(self.log, "background worker not restarted: shard is not running";
                "shard_id" => self.shard_id);
            return;
        }
        self.start_background();
    }
    pub fn replace_background(&mut self) {
        if self.db.is_some() {
            self.start_background();
        } else if let Some(worker) = self.background_worker.take() {
            worker.stop_async();
        }
    }


    #[timed(backup)]
    pub fn backup_incremental(
        &mut self,
        shard_backup_path: PathBuf,
        backup_id: String,
        user: String,
        segment_dir: PathBuf
    ) -> Result<Option<BackupManifest>, CorelamoError> {
        info!(self.log, "Incremental backup made"; "user"=> user, "backup_id"=> backup_id.clone());
        let progress = self.stats.backup_progress().clone();
        self.backup
            .create_incremental_backup(
                &shard_backup_path,
                &backup_id,
                &segment_dir,
                self.document_count(),
                &progress
            )
            .map_err(|e| CorelamoError::Internal(e.to_string()))
    }

    #[timed(restore)]
    pub fn restore_backup(&mut self, backup_id: &str, user: String) -> Result<(), CorelamoError> {
        let root = self.root.clone();
        info!(self.log, "Restore to backup"; "backup id"=> backup_id, "user"=>user);
        self.stop()?;
        self.backup
            .restore_chain(&backup_id, &root)
            .map_err(|e| CorelamoError::Internal(e.to_string()))?;
        //self.start()?;

        Ok(())
    }

    fn publish_stats(&self) {
        let Ok(db) = self.db_ref() else {
            return;
        };
        self.stats.publish(db.document_count(), &db.index_stats_cached());
    }

    #[timed(backup)]
    pub fn list_backups(&self) -> Result<Vec<BackupManifest>, CorelamoError> {
        self.backup.list_backups().map_err(|e| CorelamoError::Internal(e.to_string()))
    }

    #[timed(backup)]
    pub fn delete_backup(&self, backup_id: &str, user: String) -> Result<(), CorelamoError> {
        self.backup.delete_backup(backup_id).map_err(|e| CorelamoError::Internal(e.to_string()))?;
        info!(self.log, "backup deleted"; "Backup id "=> backup_id, "User" =>user);
        Ok(())
    }
    #[timed(backup)]
    pub fn delete_backups_old(&self, cutoff: SystemTime) -> Result<(), CorelamoError> {
        self.backup.delete_backups_old(cutoff).map_err(|e| CorelamoError::Internal(e.to_string()))?;
        Ok(())
    }
     /// Deletes the index, the document store and the maps file, and any reindex leftovers.
    fn remove_data_files(&self) {
        for dir in ["index", "documents", "index.new", "index.old"] {
            let path = self.root.join(dir);
            if path.exists() {
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    warn!(self.log, "could not remove {dir}"; "shard_id" => self.shard_id, "error" => %e);
                }
            }
        }
        let maps = self.root.join("documents.maps.bin");
        if maps.exists() {
            let _ = std::fs::remove_file(maps);
        }
    }
   /// The shard's own command queue, for its background worker.
    pub fn set_command_sender(&mut self, tx: crossbeam_channel::Sender<ShardCmd>) {
        self.command_tx = Some(tx);
    }
    /// Double-buffered: while the previous checkpoint is still writing its segment, this one
    /// starts anyway, and the shard keeps inserting. Only a checkpoint older than that is
    /// waited for, which bounds unflushed data to about three checkpoint intervals.
    fn start_checkpoint(&mut self) -> Result<(), CorelamoError> {
        self.reap_checkpoints()?;
        while self.pending_checkpoints.len() > 1 {
            let oldest = self.pending_checkpoints.pop_front().expect("len checked");
            let result = oldest.ticket.wait();
            self.finish_checkpoint(oldest.through_segment, result)?;
        }
        let result = (|| {
            let through_segment = self.wal
                .rotate()
                .map_err(|e| CorelamoError::Internal(format!("wal rotate failed: {e}")))?;
            let ticket = self
                .db_mut()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?
                .start_flush()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
            Ok(PendingCheckpoint { through_segment, ticket })
        })();
        match result {
            Ok(checkpoint) => {
                self.pending_checkpoints.push_back(checkpoint);
                self.pending = 0;
                Ok(())
            }
            Err(e) => {
                self.failed = true;
                Err(e) // WAL stays intact; restart replays it
            }
        }
    }

    /// Finishes the checkpoints whose flush is done, without waiting.
    fn reap_checkpoints(&mut self) -> Result<(), CorelamoError> {
        while let Some(checkpoint) = self.pending_checkpoints.front_mut() {
            let Some(result) = checkpoint.ticket.try_complete() else {
                return Ok(());
            };
            let through_segment = checkpoint.through_segment;
            self.pending_checkpoints.pop_front();
            self.finish_checkpoint(through_segment, result)?;
        }
        Ok(())
    }

    /// Waits for every pending checkpoint and finishes it.
    fn wait_checkpoints(&mut self) -> Result<(), CorelamoError> {
        while let Some(checkpoint) = self.pending_checkpoints.pop_front() {
            let result = checkpoint.ticket.wait();
            self.finish_checkpoint(checkpoint.through_segment, result)?;
        }
        Ok(())
    }

    /// On success the covered WAL segments are deleted and the new segments show up in
    /// status. On failure the shard stops taking writes and keeps every segment.
    fn finish_checkpoint(
        &mut self,
        through_segment: u64,
        result: io::Result<()>
    ) -> Result<(), CorelamoError> {
        let outcome = result
            .map_err(|e| CorelamoError::Internal(format!("checkpoint flush failed: {e}")))
            .and_then(|()| {
                 if self.wal_reindex.is_some() {
                    // the reindex commit replays these
                    return Ok(());
                }
                self.wal
                    .remove_through(through_segment)
                    .map_err(|e| CorelamoError::Internal(format!("wal cleanup failed: {e}")))
            });
        match &outcome {
            Ok(()) => self.publish_stats(),
            Err(_) => {
                self.failed = true;
                // Later checkpoints depend on this one's segments; they are replayed instead.
                self.pending_checkpoints.clear();
            }
        }
        outcome
    }

    /// Runs about every second while the shard is idle: finishes checkpoints that completed in
    /// the meantime, and checkpoints anything written since the last one once the shard has
    /// been quiet for `IDLE_FLUSH_AFTER`, whatever `indexing_batch_size` is.
    pub fn maybe_idle_flush(&mut self) -> Result<(), CorelamoError> {
        self.reap_checkpoints()?;
        if self.pending == 0
            || !self.pending_checkpoints.is_empty()
            || self.last_write.elapsed() < IDLE_FLUSH_AFTER
        {
            return Ok(());
        }
        self.start_checkpoint()?;
        self.last_write = Instant::now();
        Ok(())
    } 
    /// Starts compaction and embedding for the live index, replacing any running worker.
    /// Starts compaction, embedding and store compaction for the live index, replacing any
    /// running worker.
    fn start_background(&mut self) {
        if let Some(worker) = self.background_worker.take() {
            worker.stop_async();
        }
        self.stats.set_activity(BackgroundActivity::Idle);
        self.stats.set_embedding_waiting(false);
        self.stats.set_embedding_error(None);
        let Some(db) = self.db.as_ref() else {
            return;
        };
        let Some(shard_tx) = self.command_tx.clone() else {
            warn!(self.log, "background worker not started: no command sender"; "shard_id" => self.shard_id);
            self.stats.set_embedding_enabled(false);
            self.stats.set_compaction_enabled(false);
            return;
        };
        let compacting = self.options.enable_background_compaction;
        let compaction = compacting.then_some(self.options.runtime.compaction);
        let embedding = EmbeddingJob::new(
            db.index_sender(),
            self.shared_state(),
            self.policy.clone(),
            self.options.embed,
            self.log.new(slog::o!("component" => "embedding"))
        );
        self.stats.set_embedding_enabled(embedding.is_some());
        if embedding.is_none() {
            // no vector fields any more: this shard has nothing to embed
            self.stats.set_vector_progress(0, 0);
        }
        self.stats.set_compaction_enabled(compacting);

        self.background_worker = Some(BackgroundWorker::start(BackgroundConfig {
            index: db.index_sender(),
            shard_tx,
            compaction,
            store_compaction: compacting.then_some(self.options.dead_file_treshold),
            interval: self.options.compaction_interval,
            
            embedding,
            models: Arc::clone(&self.models),
            stats: self.stats.clone(),
            log: self.log.new(slog::o!("component" => "background")),
        }));
    }
}
