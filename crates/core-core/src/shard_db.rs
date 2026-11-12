use ahash::{ HashSet, HashSetExt };
use core_backup::{ backup::{ BackupManager, BackupManifest }, progress::BackupStats };
use core_index::{
    analyzer::Analyzer,
    disk::reader::DiskSegment,
    document::IndexPolicy,
    lsm::{
        LsmIndex,
        index_worker::{ IndexCommand, IndexingStats, ReindexProgress, ReindexingStats },
    },
    search::SearchIndex,
    types::{ ARRAY_ROW_FLAG, DocId, ShardId },
};
use core_protocol::{
    command_reponse_definitions::{ LookupCommand, LookupResponse },
    command_response_helpers::apply_merge_patch,
    errors::{ CorelamoError, DocFailure, FailReason },
    format::Format,
};
use core_timing::timed;
use indexmap::IndexMap;
use simd_json::OwnedValue;
use std::{
    collections::VecDeque,
    fs::{ self, File },
    io::{ self, Write },
    path::{ Path, PathBuf },
    sync::{ Arc, mpsc },
    time::{ Duration, Instant, SystemTime },
};

use crate::{
    background_worker::{ BackgroundConfig, BackgroundWorker },
    metrics::{ BackgroundActivity, BackgroundStats, DatabaseMetrics, DbStats, ShardStatsHandle },
    options::DatabaseOptions,
    reindex::{ CompletedShardReindex, REINDEX_COMPLETE, ReindexParams, ReindexTask },
    shard_worker::ShardCmd,
    shared_state::SharedShardState,
};
use core_embed::ModelRegistry;
use core_logs::logger;
use core_storage::{
    binary_store::{
        BinaryDocumentStore,
        CompletedSegmentCompaction,
        DEFAULT_SEGMENT_SIZE,
        SegmentCompactionJob,
        StoreSnapshot,
        save_maps,
    },
    document_store::{ DocumentStore, StoredDocument },
    json_parse::{ parse_into_node, parse_source_into_node },
    search_database::{
        DeleteReport,
        DocumentInput,
        FlushTicket,
        IndexMode,
        InsertReport,
        ReplaceReport,
        SearchDatabase,
    },
    wal::{ SyncMode, Wal, WalDocument, WalDocumentRef, WalRecord, WalRecordRef },
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
const RESTORE_STAGING: &str = "restore.new";
const RESTORE_OLD: &str = "restore.old";
const RESTORE_DATA_DIRS: [&str; 2] = ["index", "documents"];
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
    pending_checkpoints: VecDeque<PendingCheckpoint>,
    command_tx: Option<crossbeam_channel::Sender<ShardCmd>>,
    wal_reindex: Option<u64>,
}
struct PendingCheckpoint {
    through_segment: u64,
    ticket: FlushTicket,
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
        models: Arc<ModelRegistry>
    ) -> Result<Self, CorelamoError> {
        let root = root.as_ref().to_path_buf();
        let name = format!("shard-{shard_id}");

        if root.exists() {
            return Err(
                CorelamoError::AlreadyExists(format!("shard at {} already exists", root.display()))
            );
        }

        fs::create_dir_all(&root)?;

        let log = logger::shard_logger(&root, &name);
        let wal = open_wal(&root, options.sync_mode).map_err(|e|
            CorelamoError::Internal(format!("failed to open WAL: {e}"))
        )?;
        let store_path = root.join("documents");
        BinaryDocumentStore::open(&store_path)?;
        let backup_dir = db_root.as_ref().join("backups");

        fs::create_dir_all(&backup_dir)?;
        let backup = BackupManager::new(backup_dir, name.clone(), 0, 0, log.clone());
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
            pending_checkpoints: VecDeque::new(),
            command_tx: None,
            wal_reindex: None,
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
                CorelamoError::InvalidData(format!("invalid shard directory name: {name}"))
            })?;

        let log = logger::shard_logger(&root, &name);
        match recover_interrupted_restore(&root) {
            Ok(true) => warn!(log, "finished a restore interrupted by a crash"; "shard" => &name),
            Ok(false) => {}
            Err(e) => {
                return Err(
                    CorelamoError::Internal(
                        format!("finishing an interrupted restore in {}: {e}", root.display())
                    )
                );
            }
        }
        match recover_interrupted_reindex(&root) {
            Ok(Some(what)) => warn!(log, "{what}"; "shard" => &name),
            Ok(None) => {}
            Err(e) => {
                return Err(
                    CorelamoError::Internal(
                        format!("recovering an interrupted reindex in {}: {e}", root.display())
                    )
                );
            }
        }
        // load
        let wal = open_wal(&root, options.sync_mode).map_err(|e|
            CorelamoError::Internal(format!("failed to open WAL: {e}"))
        )?;
        let backup_dir = db_root.as_ref().join("backups");
        fs::create_dir_all(&backup_dir)?;
        let backup = BackupManager::new(backup_dir, name.clone(), 0, 0, log.clone());
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
            pending_checkpoints: VecDeque::new(),
            command_tx: None,
            wal_reindex: None,
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
        let skip_bad = self.options.skip_unreplayable_wal_records;

        let mut records: Vec<(u64, usize, Option<Vec<u8>>, WalRecord)> = Vec::new();
        for (segment, n, payload) in self.wal
            .replay_all()
            .map_err(|e| CorelamoError::Internal(format!("failed to replay WAL: {e}")))? {
            match bincode::decode_from_slice::<WalRecord, _>(&payload, bincode::config::standard()) {
                Ok((record, _)) => {
                    records.push((segment, n, skip_bad.then_some(payload), record));
                }
                Err(e) if skip_bad => {
                    let path = self
                        .quarantine_wal_record(segment, n, &payload)
                        .map_err(|qe| {
                            CorelamoError::Internal(
                                format!(
                                    "quarantining WAL record {n} in segment {segment} failed: {qe}"
                                )
                            )
                        })?;
                    warn!(self.log, "WAL replay: undecodable record quarantined";
                    "shard_id" => self.shard_id,
                    "segment" => segment,
                    "record" => n,
                    "error" => %e,
                    "path" => %path.display(),
                );
                }
                Err(e) => {
                    return Err(
                        CorelamoError::Internal(
                            format!(
                                "failed to decode WAL record {n} in segment {segment}: {e}; \
                     set skip_unreplayable_wal_records to quarantine it and start"
                            )
                        )
                    );
                }
            }
        }
        if
            let Some(last_clear) = records
                .iter()
                .rposition(|(_, _, _, r)| matches!(r, WalRecord::Clear))
        {
            info!(self.log, "WAL replay: finishing an interrupted clear";
            "shard_id" => self.shard_id, "records_dropped" => last_clear + 1);
            self
                .remove_data_files()
                .map_err(|e| {
                    CorelamoError::Internal(format!("finishing an interrupted clear failed: {e}"))
                })?;
            records.drain(..=last_clear);
        }
        let index_root = self.root.join("index");
        let store_path = self.root.join("documents");
        let analyzer = self.analyzer();

        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        if !index.removed_leftovers().is_empty() {
            info!(self.log, "removed index files left over from an interrupted write";
                "shard_id" => self.shard_id,
                "files" => ?index.removed_leftovers());
        }
        if !index.lost_segments().is_empty() {
            warn!(self.log, "index segments missing or unreadable at startup; a reindex will rebuild them";
                "shard_id" => self.shard_id,
                "segments" => ?index.lost_segments());
        }
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

        let batch_size = self.options.runtime.indexing_batch_size;
        for (segment, n, payload, record) in records {
            let kind = match &record {
                WalRecord::Create(_) => "Create",
                WalRecord::Upsert(_) => "Upsert",
                WalRecord::Replace(_) => "Replace",
                WalRecord::Delete(_) => "Delete",
                WalRecord::Clear => "Clear",
            };
            let applied = (|| -> Result<(), CorelamoError> {
                match record {
                    WalRecord::Create(docs) => {
                        let inputs = documents_from_wal(docs, &self.policy, &self.log);
                        info!(self.log, "WAL replay: Create"; "shard_id" => self.shard_id, "documents" => inputs.len());
                        let mut fresh = Vec::with_capacity(inputs.len());
                        let mut needs_indexing = Vec::new();
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
                                    needs_indexing.push(input);
                                }
                                Some(_) => {
                                    kept_existing += 1;
                                }
                            }
                        }
                        db
                            .upsert_documents(
                                keep_last_per_id(needs_indexing),
                                IndexMode::StoreAndIndex
                            )
                            .map_err(|e|
                                CorelamoError::Internal(format!("recovery create failed: {e}"))
                            )?;
                        let report = db
                            .put_documents_parallel(fresh, batch_size)
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
                    WalRecord::Upsert(docs) => {
                        let inputs = documents_from_wal(docs, &self.policy, &self.log);
                        info!(self.log, "WAL replay: Upsert"; "shard_id" => self.shard_id, "documents" => inputs.len());
                        db
                            .upsert_documents(keep_last_per_id(inputs), IndexMode::StoreAndIndex)
                            .map_err(|e|
                                CorelamoError::Internal(format!("recovery apply failed: {e}"))
                            )?;
                    }
                    WalRecord::Replace(docs) => {
                        let inputs = documents_from_wal(docs, &self.policy, &self.log);
                        info!(self.log, "WAL replay: Replace"; "shard_id" => self.shard_id, "documents" => inputs.len());
                        db
                            .upsert_documents(keep_last_per_id(inputs), IndexMode::StoreAndIndex)
                            .map_err(|e|
                                CorelamoError::Internal(format!("recovery replace failed: {e}"))
                            )?;
                    }
                    WalRecord::Delete(ids) => {
                        info!(self.log, "WAL replay: Delete"; "shard_id" => self.shard_id, "documents" => ids.len());

                        if let Err(e) = db.delete_documents(&ids) {
                            warn!(self.log, "WAL replay: delete failed";
                                "shard_id" => self.shard_id,
                                "documents" => %ids.len(),
                                "error" => %e,
                            );
                        }
                    }
                    WalRecord::Clear => {}
                }
                Ok(())
            })();
            if let Err(e) = applied {
                let Some(payload) = payload.filter(|_| skip_bad) else {
                    return Err(
                        CorelamoError::Internal(
                            format!(
                                "WAL record {n} ({kind}) in segment {segment} failed to replay: {e}; \
                     set skip_unreplayable_wal_records to quarantine it and start"
                            )
                        )
                    );
                };
                let path = self
                    .quarantine_wal_record(segment, n, &payload)
                    .map_err(|qe| {
                        CorelamoError::Internal(
                            format!("quarantining WAL record {n} in segment {segment} failed: {qe}")
                        )
                    })?;
                warn!(self.log, "WAL replay: record quarantined";
                "shard_id" => self.shard_id,
                "segment" => segment,
                "record" => n,
                "kind" => kind,
                "error" => %e,
                "path" => %path.display(),
            );
            }
        }
        let live: HashSet<DocId> = self.shared.locations
            .iter()
            .map(|entry| entry.value().internal_id)
            .collect();
        let sender = db.index_sender();
        let (reply, rx) = mpsc::channel();
        sender
            .send(IndexCommand::DiskSegments { reply })
            .map_err(|_| CorelamoError::Internal("index worker stopped during recovery".into()))?;
        let segments = rx
            .recv()
            .map_err(|_| CorelamoError::Internal("index worker dropped the segment list".into()))?
            .map_err(|e| CorelamoError::Internal(format!("listing segments failed: {e}")))?;
        let snapshot = self.shared.snapshot.get();
        let mut stale: HashSet<DocId> = HashSet::new();
        for path in segments {
            let segment = DiskSegment::open(&path).map_err(|e|
                CorelamoError::Internal(format!("reading segment failed: {e}"))
            )?;
            for ((doc_id, _), _) in segment.doc_lengths().iter() {
                if (doc_id & ARRAY_ROW_FLAG) != 0 {
                    continue; // array row id, not a document id
                }
                if !live.contains(&doc_id) && !snapshot.is_deleted(doc_id) {
                    stale.insert(doc_id);
                }
            }
        }
        if !stale.is_empty() {
            let (ack, acked) = mpsc::channel();
            sender
                .send(IndexCommand::DeleteDocuments {
                    doc_ids: stale.iter().copied().collect(),
                    publish: true,
                    ack: Some(ack),
                })
                .map_err(|_|
                    CorelamoError::Internal("index worker stopped during recovery".into())
                )?;
            acked
                .recv()
                .map_err(|_| CorelamoError::Internal("index worker dropped a delete".into()))?
                .map_err(|e|
                    CorelamoError::Internal(format!("removing stale entries failed: {e}"))
                )?;
        }

        if replayed_any || !stale.is_empty() {
            self.pending = self.pending.max(1);
        }
        // Cleared only now, so a replay that fails leaves the shard marked failed.
        self.failed = false;
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
        self.wal_reindex = None;
        if let Err(e) = self.wait_checkpoints() {
            warn!(self.log, "checkpoint failed during stop; WAL kept for replay";
            "shard_id" => self.shard_id, "error" => %e);
        }

        // read before the database is dropped: the maps file must keep the highest id ever used
        let highest_id = self.db.as_ref().map_or(0, |db| db.store().max_internal_id());

        if let Some(db) = self.db.take() {
            db.shutdown()?;
            info!(self.log, "shard stopped"; "shard_id" => self.shard_id);
        }

        let store_path = self.root.join("documents");
        save_maps(&store_path, &self.shared.locations, highest_id).map_err(|e|
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
        // the WAL decides on every append whether to fsync, so a new mode applies at onc
        self.options = options;
        // compaction, embedding and backup settings are read when the background worker starts
        if self.db.is_some() {
            self.start_background();
        }
        info!(self.log, "options set"; "shard_id" => self.shard_id, "user" => user);
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
    fn wal_encode_record(record: &WalRecordRef<'_>) -> Result<Vec<u8>, CorelamoError> {
        bincode
            ::encode_to_vec(record, bincode::config::standard())
            .map_err(|e| CorelamoError::Internal(format!("wal encode failed: {e}")))
    }
    fn wal_append_encoded(&mut self, encoded: &[u8]) -> Result<u64, CorelamoError> {
        self.wal
            .append(encoded)
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
        let mut wal_touched = false;
        let result = (|| -> Result<InsertReport, CorelamoError> {
            let encoded = {
                let docs: Vec<WalDocumentRef<'_>> = inputs
                    .iter()
                    .map(WalDocumentRef::from)
                    .collect();
                Self::wal_encode_record(&WalRecordRef::Create(&docs))?
            };
            wal_touched = true;
            self.wal_append_encoded(&encoded)?;
            self.pending += count as u32;
            let should_checkpoint = self.pending >= (batch_size as u32);
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
                self.stats.add_documents_indexed(u64::from(report.inserted));
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
                if wal_touched {
                    self.failed = true;
                }
                error!(self.log, "indexing failed";
                    "shard_id" => %self.shard_id,
                    "documents" => count,
                    "batch_size" => batch_size,
                    "elapsed_ms" => elapsed.as_millis(),
                    "error" => %e,
                    "user" => user,
                    "Shard_failed" => wal_touched
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
        for entry in fs::read_dir(&logs_dir)? {
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
            let content = fs::read_to_string(&file_path)?;
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

        for entry in fs::read_dir(&logs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "log") {
                fs::remove_file(&path)?;
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
        let mut failures: Vec<DocFailure> = Vec::new();

        self
            .wal_append_record(&WalRecordRef::Delete(&ids))
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;

        self.pending += count as u32;
        let batch_size = self.options.runtime.indexing_batch_size;
        let should_checkpoint = self.pending >= (batch_size as u32);

        let applied = {
            let db = self.db_mut().map_err(|e| CorelamoError::Internal(e.to_string()))?;

            // 1. check: which ids exist; a repeated id is gone after its first copy
            let mut to_delete: Vec<String> = Vec::with_capacity(count);
            let mut seen: HashSet<String> = HashSet::with_capacity(count);
            for id in ids {
                if !seen.insert(id.clone()) {
                    failures.push(DocFailure::new(None, Some(id), FailReason::NotFound));
                    continue;
                }
                match db.get_document(&id) {
                    Ok(Some(_)) => to_delete.push(id),
                    Ok(None) =>
                        failures.push(DocFailure::new(None, Some(id), FailReason::NotFound)),
                    Err(e) =>
                        failures.push(
                            DocFailure::new(None, Some(id), FailReason::Internal(e.to_string()))
                        ),
                }
            }

            // 2. apply: one batch
            match db.delete_documents(&to_delete) {
                Ok(()) => Ok(to_delete.len() as u32),
                Err(e) => {
                    fail_batch(&mut failures, to_delete, &e);
                    Err(e)
                }
            }
        };
        let deleted = match applied {
            Ok(n) => n,
            Err(e) => {
                // the WAL holds this batch; a restart replays it over whatever was applied
                self.failed = true;
                error!(self.log, "delete batch failed; shard needs a restart";
                "shard_id" => %self.shard_id, "error" => %e, "user" => user.clone());
                0
            }
        };

        if should_checkpoint && !self.failed {
            self.start_checkpoint()?;
        }
        self.publish_stats();
        self.last_write = Instant::now();
        info!(self.log, "delete batch";
        "user" => user,
        "shard_id" => %self.shard_id,
        "requested" => count,
        "deleted" => deleted,
        "failed" => failures.len(),
        "elapsed_ms" => started.elapsed().as_millis(),
    );
        Ok(DeleteReport { deleted, failures })
    }

    #[timed(reindex)]
    pub fn prepare_reindex(
        &mut self,
        progress: Arc<ReindexProgress>,
        stats: Arc<DbStats>
    ) -> Result<(), CorelamoError> {
        if !self.is_running() {
            return Err(
                CorelamoError::DatabaseNotRunning(format!("shard {} is not running", self.shard_id))
            );
        }
        if self.background_worker.is_none() {
            return Err(
                CorelamoError::Internal(format!("shard {} has no background worker", self.shard_id))
            );
        }
        self.flush()?;
        let store = StoreSnapshot::capture(
            self.root.join("documents"),
            &self.shared.locations,
            self
                .db_ref()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?
                .store()
                .max_internal_id()
        );
        self.generation += 1;
        self.wal_reindex = Some(self.generation);
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
            Some(worker) =>
                worker.submit_reindex(ReindexTask {
                    store,
                    params,
                    progress,
                    stats,
                }),
            None =>
                Err(
                    CorelamoError::Internal(
                        format!("shard {} has no background worker", self.shard_id)
                    )
                ),
        };
        if submitted.is_err() {
            self.wal_reindex = None;
        }
        submitted
    }

    #[timed(reindex)]
    pub fn commit_reindex(&mut self, done: CompletedShardReindex) -> Result<(), CorelamoError> {
        if done.generation != self.generation {
            let _ = fs::remove_dir_all(&done.staging_root);
            return Err(CorelamoError::Conflict("reindex result is from a superseded run".into()));
        }
        if !done.staging_root.join(REINDEX_COMPLETE).exists() {
            let _ = fs::remove_dir_all(&done.staging_root);
            self.wal_reindex = None;
            return Err(
                CorelamoError::Internal(
                    "reindex output is incomplete; the current index is kept".into()
                )
            );
        }

        let index_root = self.root.join("index");
        let old_root = self.root.join("index.old");

        // close the live index before touching its directory
        if let Some(db) = self.db.take() {
            db.shutdown().map_err(|e| CorelamoError::Internal(e.to_string()))?;
        }

        remove_dir_if_exists(&old_root)?;
        fs::rename(&index_root, &old_root)?;
        if let Err(e) = fs::rename(&done.staging_root, &index_root) {
            fs::rename(&old_root, &index_root)?;
            sync_dir(&self.root)?;
            self.wal_reindex = None;
            self.start()?;
            return Err(CorelamoError::Internal(format!("reindex swap failed:{e}")));
        }
        sync_dir(&self.root)?;
        if let Err(e) = self.start() {
            warn!(self.log, "new index failed to start; rolling back to the previous index";
            "shard_id" => %self.shard_id, "error" => %e);
            fs::rename(&index_root, &done.staging_root)?;
            fs::rename(&old_root, &index_root)?;
            sync_dir(&self.root)?;
            self.wal_reindex = None;
            self.start()?;
            return Err(
                CorelamoError::Internal(
                    format!("reindex rolled back, the new index could not start: {e}")
                )
            );
        }
        self.wal_reindex = None;
        self.flush()?;
        remove_dir_if_exists(&old_root)?;
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
        let _ = fs::remove_dir_all(self.root.join("index.new"));
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
        let mut failures: Vec<DocFailure> = Vec::new();

        // 1. check only, no changes
        let inputs: Vec<DocumentInput> = {
            let db = self.db_mut().map_err(|e| CorelamoError::Internal(e.to_string()))?;
            let mut existing = Vec::with_capacity(count);

            for input in keep_last_per_id(inputs) {
                match db.get_document(&input.external_id) {
                    Ok(Some(_)) => existing.push(input),
                    Ok(None) =>
                        failures.push(
                            DocFailure::new(None, Some(input.external_id), FailReason::NotFound)
                        ),
                    Err(e) =>
                        failures.push(
                            DocFailure::new(
                                None,
                                Some(input.external_id),
                                FailReason::Internal(e.to_string())
                            )
                        ),
                }
            }
            existing
        };

        if inputs.is_empty() {
            return Ok(ReplaceReport { replaced: 0, failures });
        }

        // 2. WAL (fsync) only what will be applied
       let docs: Vec<WalDocumentRef<'_>> = inputs.iter().map(WalDocumentRef::from).collect();
        self.wal_append_record(&WalRecordRef::Replace(&docs))?;
        self.pending += inputs.len() as u32;
        let batch_size = self.options.runtime.indexing_batch_size;
        let should_checkpoint = self.pending >= (batch_size as u32);

        let ids: Vec<String> = inputs
            .iter()
            .map(|i| i.external_id.clone())
            .collect();
        let applied = self
            .db_mut()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?
            .upsert_documents(inputs, IndexMode::StoreAndIndex);
        let replaced = match applied {
            Ok(()) => ids.len() as u32,
            Err(e) => {
                self.failed = true;
                error!(self.log, "replace batch failed; shard needs a restart";
                "shard_id" => %self.shard_id, "error" => %e, "user" => user.clone());
                fail_batch(&mut failures, ids, &e);
                0
            }
        };
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
    pub fn partial_replace(
        &mut self,
        items: Vec<(String, OwnedValue)>,
        user: String
    ) -> Result<ReplaceReport, CorelamoError> {
        self.ensure_running()?;
        let mut failures = Vec::new();
        let mut patched: IndexMap<String, OwnedValue> = IndexMap::new();
        for (id, patch) in items {
            if let Some(value) = patched.get_mut(&id) {
                apply_merge_patch(value, &patch);
                continue;
            }
            let stored = self
                .db_ref()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?
                .get_document(&id)
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
            let Some(doc) = stored else {
                failures.push(DocFailure::new(None, Some(id), FailReason::NotFound));
                continue;
            };
            let mut buf = doc.source.to_vec();
            let mut value: OwnedValue = match simd_json::to_owned_value(&mut buf) {
                Ok(v) => v,
                Err(e) => {
                    failures.push(
                        DocFailure::new(
                            None,
                            Some(id),
                            FailReason::Internal(format!("stored document is not valid JSON: {e}"))
                        )
                    );
                    continue;
                }
            };
            apply_merge_patch(&mut value, &patch);
            patched.insert(id, value);
        }
        let mut inputs = Vec::with_capacity(patched.len());
        for (id, value) in patched {
            let parsed = match parse_into_node(&value, &self.policy) {
                Ok(node) => node,
                Err(reason) => {
                    failures.push(DocFailure::new(None, Some(id), reason));
                    continue;
                }
            };
            let source = match simd_json::to_vec(&value) {
                Ok(bytes) => bytes,
                Err(e) => {
                    failures.push(
                        DocFailure::new(None, Some(id), FailReason::Internal(e.to_string()))
                    );
                    continue;
                }
            };
            inputs.push(DocumentInput {
                external_id: id,
                parsed,
                source,
                format: Format::JSON,
            });
        }
        let mut report = if inputs.is_empty() {
            ReplaceReport { replaced: 0, failures: Vec::new() }
        } else {
            self.replace(inputs, user)?
        };
        report.failures.extend(failures);
        Ok(report)
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
        let mut failures: Vec<DocFailure> = Vec::new();

        let inputs = keep_last_per_id(inputs);
        if inputs.is_empty() {
            return Ok(InsertReport { inserted: 0, failures });
        }

        let docs: Vec<WalDocumentRef<'_>> = inputs.iter().map(WalDocumentRef::from).collect();
        self.wal_append_record(&WalRecordRef::Upsert(&docs))?;
        self.pending += inputs.len() as u32;
        let batch_size = self.options.runtime.indexing_batch_size;
        let should_checkpoint = self.pending >= (batch_size as u32);

        let ids: Vec<String> = inputs
            .iter()
            .map(|i| i.external_id.clone())
            .collect();
        let applied = self
            .db_mut()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?
            .upsert_documents(inputs, IndexMode::StoreAndIndex);
        let inserted = match applied {
            Ok(()) => ids.len() as u32,
            Err(e) => {
                self.failed = true;
                error!(self.log, "upsert batch failed; shard needs a restart";
                "shard_id" => %self.shard_id, "error" => %e, "user" => user.clone());
                fail_batch(&mut failures, ids, &e);
                0
            }
        };

        if should_checkpoint && !self.failed {
            self.start_checkpoint()?;
        }
        self.stats.add_documents_indexed(u64::from(inserted));
        self.publish_stats();
        self.last_write = Instant::now();
        info!(self.log, "upsert batch";
        "user" => user,
        "shard_id" => %self.shard_id,
        "requested" => count,
        "upserted" => inserted,
        "failed" => failures.len(),
        "elapsed_ms" => started.elapsed().as_millis(),
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
        self.db.as_ref().map_or(0, SearchDatabase::document_count)
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
        let installed = db.mut_store().install_segment_compaction(completed)?;
        if installed {
            // compaction rewrote segments: the incremental bookmark no longer matches them
            if let Err(e) = self.backup.require_full_backup() {
                warn!(self.log, "could not mark the next backup as full after store compaction";
                    "error" => %e);
            }
        }
        self.publish_stats();
        Ok(installed)
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
        self.wal_reindex = None;
        if let Some(db) = self.db.take() && let Err(e) = db.shutdown() {
            warn!(self.log, "clear: shutdown of old database failed";
                "shard_id" => self.shard_id, "error" => %e);
        }

        // If this fails, the Clear record stays in the WAL and the next start finishes the clear.
        self
            .remove_data_files()
            .map_err(|e| {
                CorelamoError::Internal(format!("clear: removing data files failed: {e}"))
            })?;
        self.wal.reset().map_err(|e| CorelamoError::Internal(format!("wal reset failed: {e}")))?;

        if !was_running {
            self.shared.release();
            info!(self.log, "shard cleared while stopped; not reopening"; "shard_id" => self.shard_id);
            return Ok(());
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
        let db = SearchDatabase::with_shard_policy_and_snapshot(
            store,
            index,
            analyzer,
            self.policy.clone(),
            self.shard_id,
            self.shared.snapshot.clone()
        )?;

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
        if self.wal_reindex.is_some() {
            return Err(CorelamoError::Busy("a reindex is running; full backup skipped".into()));
        }
        self.flush()?;

        if let Some(worker) = self.background_worker.take() && let Err(err) = worker.stop() {
            warn!(self.log, "background worker exited with error before backup"; "error" => %err);
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
    pub fn prepare_restore(&mut self, backup_id: &str, user: String) -> Result<(), CorelamoError> {
        info!(self.log, "Restore: preparing"; "backup id" => backup_id, "user" => user);
        self.stop()?;
        let staging = self.root.join(RESTORE_STAGING);
        remove_dir_if_exists(&staging).map_err(|e|
            CorelamoError::Internal(format!("restore: {e}"))
        )?;
        if let Err(e) = self.backup.restore_chain(backup_id, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(CorelamoError::Internal(e.to_string()));
        }
        Ok(())
    }

    /// Phase 2: swaps the prepared data in, empties the WAL (its records belong to the data
    /// that was replaced), and only then forgets the backups newer than the restored one.
    #[timed(restore)]
    pub fn commit_restore(&mut self, backup_id: &str) -> Result<(), CorelamoError> {
        let internal = |e: io::Error| CorelamoError::Internal(format!("restore commit: {e}"));
        swap_in_restore(&self.root).map_err(internal)?;
        self.reset_wal_after_restore().map_err(internal)?;
        finish_restore_swap(&self.root).map_err(internal)?;
        self.backup
            .cleanup_after_restore(backup_id)
            .map_err(|e| CorelamoError::Internal(e.to_string()))?;
        info!(self.log, "Restore: committed"; "backup id" => backup_id);
        Ok(())
    }

    /// Any prepare failed: drops the prepared data, the shard keeps its old data.
    pub fn abort_restore(&mut self) -> Result<(), CorelamoError> {
        remove_dir_if_exists(&self.root.join(RESTORE_STAGING)).map_err(|e|
            CorelamoError::Internal(format!("restore abort: {e}"))
        )
    }

    /// The WAL may still hold records if a checkpoint failed in `stop()`; replayed on top of
    /// the restored data they would bring newer writes back. Recreated empty instead of
    /// `reset()`, which refuses a poisoned WAL.
    fn reset_wal_after_restore(&mut self) -> io::Result<()> {
        remove_dir_if_exists(&self.root.join("wal"))?;
        self.wal = open_wal(&self.root, self.options.sync_mode)?;
        self.failed = false;
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
    fn remove_data_files(&self) -> io::Result<()> {
        for dir in ["index", "documents", "index.new", "index.old"] {
            let path = self.root.join(dir);
            match fs::remove_dir_all(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display())));
                }
            }
        }
        match fs::remove_file(self.root.join("documents.maps.bin")) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e);
            }
        }
        File::open(&self.root)?.sync_all()
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
            Ok(PendingCheckpoint {
                through_segment,
                ticket,
            })
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
        if let Ok(()) = &outcome {
            self.publish_stats();
        } else {
            self.failed = true;
            // Later checkpoints depend on this one's segments; they are replayed instead.
            self.pending_checkpoints.clear();
        }
        outcome
    }

    fn quarantine_wal_record(&self, segment: u64, n: usize, payload: &[u8]) -> io::Result<PathBuf> {
        let dir = self.root.join("wal_quarantine");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("seg{segment}-rec{n}.bin"));
        let tmp = path.with_extension("bin.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(payload)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        File::open(&dir)?.sync_all()?;
        Ok(path)
    }
    pub fn maybe_idle_flush(&mut self) -> Result<(), CorelamoError> {
        self.reap_checkpoints()?;
        if
            self.pending == 0 ||
            !self.pending_checkpoints.is_empty() ||
            self.last_write.elapsed() < IDLE_FLUSH_AFTER
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

        self.background_worker = Some(
            BackgroundWorker::start(BackgroundConfig {
                index: db.index_sender(),
                shard_tx,
                compaction,
                store_compaction: compacting.then_some(self.options.dead_file_treshold),
                interval: self.options.compaction_interval,

                embedding,
                models: Arc::clone(&self.models),
                stats: self.stats.clone(),
                log: self.log.new(slog::o!("component" => "background")),
            })
        );
    }
}
//helper functions
fn keep_last_per_id(inputs: Vec<DocumentInput>) -> Vec<DocumentInput> {
    let mut by_id: IndexMap<String, DocumentInput> = IndexMap::with_capacity(inputs.len());
    let mut generated = Vec::new();
    for input in inputs {
        if input.external_id.is_empty() {
            generated.push(input);
        } else {
            by_id.insert(input.external_id.clone(), input);
        }
    }
    let mut out: Vec<DocumentInput> = by_id.into_values().collect();
    out.extend(generated);
    out
}
fn documents_from_wal(
    docs: Vec<WalDocument>,
    policy: &IndexPolicy,
    log: &Logger
) -> Vec<DocumentInput> {
    let mut inputs = Vec::with_capacity(docs.len());
    for doc in docs {
        match parse_source_into_node(&doc.source, policy) {
            Ok(parsed) =>
                inputs.push(DocumentInput {
                    external_id: doc.external_id,
                    parsed,
                    source: doc.source,
                    format: doc.format,
                }),
            Err(reason) =>
                warn!(log, "WAL replay: document no longer matches the policy, skipped";
                "external_id" => %doc.external_id, "reason" => ?reason),
        }
    }
    inputs
}
fn fail_batch(failiures: &mut Vec<DocFailure>, ids: Vec<String>, error: &io::Error) {
    for id in ids {
        failiures.push(DocFailure::new(None, Some(id), FailReason::Internal(error.to_string())));
    }
}
fn remove_dir_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// fsyncs every file and directory under `path`.
fn sync_tree(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            File::open(entry.path())?.sync_all()?;
        }
    }
    File::open(path)?.sync_all()
}
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn swap_in_restore(root: &Path) -> io::Result<()> {
    let staging = root.join(RESTORE_STAGING);
    let old = root.join(RESTORE_OLD);
    if !old.exists() {
        for dir in RESTORE_DATA_DIRS {
            sync_tree(&staging.join(dir))?;
        }
        fs::create_dir(&old)?;
        File::open(root)?.sync_all()?;
    }
    for dir in RESTORE_DATA_DIRS {
        let prepared = staging.join(dir);
        if !prepared.exists() {
            continue; // already swapped before a crash
        }
        let live = root.join(dir);
        if live.exists() && !old.join(dir).exists() {
            fs::rename(&live, old.join(dir))?;
        }
        remove_dir_if_exists(&live)?; // a half-moved copy from before a crash
        fs::rename(&prepared, &live)?;
    }
    // derived from the replaced data; rebuilt from the restored files at start
    remove_dir_if_exists(&root.join("index.new"))?;
    remove_dir_if_exists(&root.join("index.old"))?;
    match fs::remove_file(root.join("documents.maps.bin")) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e);
        }
        _ => {}
    }
    File::open(root)?.sync_all()
}

/// Last step: the old data is no longer needed once the WAL is empty.
fn finish_restore_swap(root: &Path) -> io::Result<()> {
    remove_dir_if_exists(&root.join(RESTORE_OLD))?;
    remove_dir_if_exists(&root.join(RESTORE_STAGING))?;
    File::open(root)?.sync_all()
}

fn recover_interrupted_restore(root: &Path) -> io::Result<bool> {
    if root.join(RESTORE_OLD).exists() {
        swap_in_restore(root)?;
        remove_dir_if_exists(&root.join("wal"))?;
        finish_restore_swap(root)?;
        return Ok(true);
    }
    remove_dir_if_exists(&root.join(RESTORE_STAGING))?;
    Ok(false)
}
fn recover_interrupted_reindex(root: &Path) -> io::Result<Option<&'static str>> {
    let index = root.join("index");
    let new = root.join("index.new");
    let old = root.join("index.old");
    let mut action = None;

    if !index.exists() {
        if new.join(REINDEX_COMPLETE).exists() {
            fs::rename(&new, &index)?;
            action = Some("finished a reindex swap interrupted by a crash");
        } else if old.exists() {
            fs::rename(&old, &index)?;
            action = Some("restored the previous index after an interrupted reindex");
        }
        sync_dir(root)?;
    }
    if new.exists() {
        remove_dir_if_exists(&new)?;
        action = action.or(Some("removed an unfinished reindex"));
    }
    if old.exists() {
        remove_dir_if_exists(&old)?;
        action = action.or(Some("removed the index replaced by a reindex"));
    }
    if action.is_some() {
        sync_dir(root)?;
    }
    Ok(action)
}
