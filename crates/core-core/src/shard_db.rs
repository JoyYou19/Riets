use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone, PartialEq)]
pub struct DatabaseStats {
    pub document_count: usize,
    pub segment_count: usize,
    pub background_compaction_enabled: bool,
    pub metrics: DatabaseMetrics,
    pub indexing: IndexingStats,
    pub reindexing: ReindexingStats,
    pub backup: BackupStats,
    pub restoring: bool,
}

use core_backup::{
    backup::{BackupManager, BackupManifest},
    progress::BackupStats,
};
use core_index::{
    analyzer::Analyzer,
    document::IndexPolicy,
    lsm::{
        LsmIndex,
        index_worker::{IndexingStats, ReindexProgress, ReindexingStats},
        worker::CompactionWorker,
    },
    types::ShardId,
};
use core_protocol::{
    command_reponse_definitions::{LookupCommand, LookupResponse},
    errors::{CorelamoError, DocFailure, FailReason},
};
use core_query::{Query, SearchHit, query_string_parser::parse_and_analyze};

use crate::{
    metrics::DatabaseMetrics,
    metrics::ShardStatsHandle,
    options::DatabaseOptions,
    reindex::{CompletedShardReindex, ReindexParams},
    shared_state::SharedShardState,
};
use core_logs::logger;
use core_storage::{
    binary_store::BinaryDocumentStore,
    document_store::StoredDocument,
    search_database::{
        DeleteReport, DocumentInput, IndexMode, InsertReport, ReplaceReport, SearchDatabase,
        SearchDocumentHit,
    },
    wal::{Wal, WalRecord},
};
use slog::{Logger, error, info, warn};

pub struct ShardDb {
    shard_id: ShardId,
    root: PathBuf,
    policy: IndexPolicy,
    options: DatabaseOptions,
    //each shard kip holds this yeye
    db: Option<SearchDatabase<BinaryDocumentStore>>,
    compaction_worker: Option<CompactionWorker>,
    stats: ShardStatsHandle,
    log: Logger,
    wal: Wal,
    generation: u64,
    shared: Arc<SharedShardState>,
    backup: BackupManager,
}

impl ShardDb {
    pub fn shared_state(&self) -> Arc<SharedShardState> {
        self.shared.clone()
    }

    pub fn create_shard(
        root: impl AsRef<Path>,
        db_root: impl AsRef<Path>,
        shard_id: ShardId,
        options: DatabaseOptions,
        policy: IndexPolicy,
        stats: ShardStatsHandle,
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

        let log = logger::shard_logger(&root, &name);
        let wal = Wal::open(root.join("wal.log"), core_storage::wal::SyncMode::SyncEach)
            .map_err(|e| CorelamoError::Internal(format!("failed to open WAL: {e}")))?;
        let store_path = root.join("documents.bin");
        BinaryDocumentStore::open(&store_path)?;
        let backup_dir = db_root.as_ref().join("backups");
        std::fs::create_dir_all(&backup_dir)?;
        let backup = BackupManager::new(&root, backup_dir, name.clone()); // see note below
        Ok(Self {
            shard_id,
            shared: Arc::new(SharedShardState::new(root.clone())),
            policy,
            options,
            db: None,
            compaction_worker: None,
            log,
            wal,
            generation: 0,
            stats,
            root,
            backup,
        })
    }

    pub fn load(
        root: impl AsRef<Path>,
        db_root: impl AsRef<Path>,
        policy: &IndexPolicy,
        options: &DatabaseOptions,
        stats: ShardStatsHandle,
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

        let log = logger::shard_logger(&root, &name);
        let wal = Wal::open(root.join("wal.log"), core_storage::wal::SyncMode::SyncEach)
            .map_err(|e| CorelamoError::Internal(format!("failed to open WAL: {e}")))?;
        let backup_dir = db_root.as_ref().join("backups");
        std::fs::create_dir_all(&backup_dir)?;
        let backup = BackupManager::new(&root, backup_dir, name.clone());
        Ok(Self {
            shard_id: ShardId::from(shard_id),
            shared: Arc::new(SharedShardState::new(root.clone())),
            root,
            policy: policy.clone(),
            options: options.clone(),
            db: None,
            compaction_worker: None,
            stats,
            log,
            wal,
            generation: 0,
            backup,
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
        let store = BinaryDocumentStore::open_with_maps(
            &store_path,
            self.shared.docs.clone(),
            self.shared.internal_to_external.clone(),
        )?;
        let mut db = SearchDatabase::with_shard_policy_and_snapshot(
            store,
            index,
            analyzer,
            self.policy.clone(),
            self.shard_id,
            self.shared.snapshot.clone(),
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
                WalRecord::Upsert(inputs) => {
                    info!(self.log, "WAL replay: Upsert";"shard_id" => self.shard_id, "documents" => inputs.len());
                    for input in inputs {
                        db.upsert_document(input, IndexMode::StoreAndIndex)
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
                        db.upsert_document(input, IndexMode::StoreAndIndex)
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
        self.stats
            .set_compaction_enabled(self.compaction_worker.is_some());
        self.publish_stats();
        info!(self.log, "shard started"; "shard_id" => self.shard_id);
        Ok(())
    }

    pub fn resolve_hits(
        &self,
        hits: Vec<SearchHit>,
    ) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        let db = self
            .db_ref()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?;
        db.resolve_document_hits(hits, None)
            .map_err(|e| CorelamoError::Internal(e.to_string()))
    }

    pub fn stop(&mut self) -> Result<(), CorelamoError> {
        if self.db.is_none() {
            return Ok(());
        }

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
        self.stats.set_compaction_enabled(false);
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

    pub fn set_policy(&mut self, policy: IndexPolicy, user: String) -> Result<(), CorelamoError> {
        policy.validate()?;
        if let Some(db) = &mut self.db {
            db.set_policy(policy.clone())?;
        }
        self.policy = policy;
        info!(self.log, "policy set"; "shard_id" => self.shard_id, "user" => user);
        Ok(())
    }

    pub fn set_options(
        &mut self,
        options: DatabaseOptions,
        user: String,
    ) -> Result<(), CorelamoError> {
        info!(self.log, "Options set"; "User"=>user);
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
            .map_err(|e|
            //fake search
            CorelamoError::Internal(e.to_string()))
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
    // =========write operations =========
    fn wal_append_record(&mut self, record: &WalRecord) -> Result<u64, CorelamoError> {
        let encoded = bincode::encode_to_vec(record, bincode::config::standard())
            .map_err(|e| CorelamoError::Internal(format!("wal encode failed: {e}")))?;
        self.wal
            .append(&encoded)
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))
    }

    pub fn insert(
        &mut self,
        inputs: Vec<DocumentInput>,
        user: String,
    ) -> Result<InsertReport, CorelamoError> {
        let started = std::time::Instant::now();
        let count = inputs.len();
        let batch_size = self.options.runtime.indexing_batch_size;
        let window_size = self.options.runtime.indexing_window_size;
        let result = (|| -> Result<InsertReport, CorelamoError> {
            let offset = self.wal_append_record(&WalRecord::Create(inputs.clone()))?;
            info!(self.log, "WAL append";
                "user" => user.clone(),
                "operation" => "create",
                "shard_id" => %self.shard_id,
                "documents" => count,
                "offset" => offset,
                "durable_offset" => self.wal.durable_offset(),
            );

            let db = self
                .db_mut()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
            let report = db
                .put_documents_parallel(inputs, batch_size, window_size)
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
            db.flush()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;

            if let Err(e) = self.wal.write_checkpoint(self.wal.durable_offset()) {
                warn!(self.log, "checkpoint write failed"; "error" => %e);
            }

            Ok(report)
        })();
        let elapsed = started.elapsed();
        match &result {
            Ok(report) => {
                self.stats.add_indexed(report.inserted as u64);
                self.publish_stats();
                info!(self.log, "indexed batch";
                    "shard_id" => %self.shard_id,
                    "documents" => count,
                    "batch_size" => batch_size,
                    "elapsed_ms" => elapsed.as_millis(),
                );
            }
            Err(e) => {
                error!(self.log, "indexing failed";
                    "shard_id" => %self.shard_id,
                    "documents" => count,
                    "batch_size" => batch_size,
                    "elapsed_ms" => elapsed.as_millis(),
                    "error" => %e,
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
                    if path
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

    pub fn delete(
        &mut self,
        ids: Vec<String>,
        user: String,
    ) -> Result<DeleteReport, CorelamoError> {
        let started = std::time::Instant::now();
        let count = ids.len();
        let mut deleted: u32 = 0;
        let mut failures: Vec<DocFailure> = Vec::new();

        let _offset = self
            .wal_append_record(&WalRecord::Delete(ids.clone()))
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;
        {
            let db = self
                .db_mut()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;

            for id in ids {
                let existing = match db.get_document(&id) {
                    Ok(doc) => doc,
                    Err(e) => {
                        failures.push(DocFailure::new(
                            None,
                            Some(id.clone()),
                            FailReason::Internal(e.to_string()),
                        ));
                        continue;
                    }
                };

                let Some(_existing) = existing else {
                    failures.push(DocFailure::new(
                        None,
                        Some(id.clone()),
                        FailReason::NotFound,
                    ));
                    continue;
                };

                match db.delete_document(&id) {
                    Ok(()) => {
                        deleted += 1;
                    }
                    Err(e) => {
                        failures.push(DocFailure::new(
                            None,
                            Some(id.clone()),
                            FailReason::Internal(e.to_string()),
                        ));
                    }
                }
            }

            db.flush()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
            if let Err(e) = self.wal.write_checkpoint(self.wal.durable_offset()) {
                warn!(self.log, "checkpoint write failed"; "error" => %e);
            }
        }
        self.publish_stats();
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

    pub fn prepare_reindex(&mut self) -> Result<ReindexParams, CorelamoError> {
        if !self.is_running() {
            return Err(CorelamoError::DatabaseNotRunning(format!(
                "shard {} is not running",
                self.shard_id
            )));
        }

        // the staging build reads documents.bin, so it must be consistent first
        self.flush()?;

        // the old index is about to be discarded; do not spend disk on compacting it
        if let Some(worker) = self.compaction_worker.take() {
            let _ = worker.stop();
        }

        self.generation += 1;

        Ok(ReindexParams {
            shard_id: self.shard_id,
            shard_root: self.root.clone(),
            policy: self.policy.clone(),
            options: self.options.clone(),
            wal_watermark: self.wal.durable_offset(),
            doc_count: self.document_count(),
            generation: self.generation,
        })
    }

    pub fn commit_reindex(&mut self, done: CompletedShardReindex) -> Result<(), CorelamoError> {
        if done.generation != self.generation {
            let _ = std::fs::remove_dir_all(&done.staging_root);
            return Err(CorelamoError::Conflict(
                "reindex result is from a superseded run".into(),
            ));
        }

        let index_root = self.root.join("index");
        let old_root = self.root.join("index.old");

        // close the live index before touching its directory
        if let Some(db) = self.db.take() {
            db.shutdown()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
        }

        if old_root.exists() {
            std::fs::remove_dir_all(&old_root)?;
        }
        std::fs::rename(&index_root, &old_root)?;
        std::fs::rename(&done.staging_root, &index_root)?;

        // reopen against the new index
        self.start()?;
        std::fs::remove_dir_all(&old_root).ok();
        self.publish_stats();
        info!(self.log, "reindex committed";
            "shard_id" => %self.shard_id,
            "generation" => self.generation,
            "built_through" => done.built_through,
        );
        Ok(())
    }

    pub fn partial_replace(
        &mut self,
        items: Vec<(String, serde_json::Value)>,
        user: String,
    ) -> Result<ReplaceReport, CorelamoError> {
        let started = std::time::Instant::now();
        let count = items.len();
        let mut replaced: u32 = 0;
        let mut failures: Vec<DocFailure> = Vec::new();

        let db = self
            .db_mut()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?;

        for (external_id, patch) in items {
            match db.partial_replace_document(&external_id, &patch) {
                Ok(Some(_)) => {
                    replaced += 1;
                }
                Ok(None) => {
                    failures.push(DocFailure::new(
                        None,
                        Some(external_id),
                        FailReason::NotFound,
                    ));
                }
                Err(e) => {
                    failures.push(DocFailure::new(
                        None,
                        Some(external_id),
                        FailReason::Internal(e.to_string()),
                    ));
                }
            }
        }

        db.flush()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?;

        let elapsed = started.elapsed();
        info!(self.log, "partial replace batch";
            "user" => user.clone(),
            "shard_id" => %self.shard_id,
            "requested" => count,
            "replaced" => replaced,
            "failed" => failures.len(),
            "elapsed_ms" => elapsed.as_millis(),
        );

        Ok(ReplaceReport { replaced, failures })
    }

    pub fn replace(
        &mut self,
        inputs: Vec<DocumentInput>,
        user: String,
    ) -> Result<ReplaceReport, CorelamoError> {
        let started = std::time::Instant::now();
        let count = inputs.len();
        let mut replaced: u32 = 0;
        let mut failures: Vec<DocFailure> = Vec::new();
        let mut old_internal_ids = Vec::new();
        let mut written_ids = Vec::new();

        //TOOD: batch support
        let _offset = self
            .wal_append_record(&WalRecord::Replace(inputs.clone()))
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;
        //vajag info log?
        {
            let db = self
                .db_mut()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;

            for input in inputs {
                let external_id = input.external_id.clone();

                let old = match db.get_document(&external_id) {
                    Ok(doc) => doc,
                    Err(e) => {
                        failures.push(DocFailure::new(
                            None,
                            Some(external_id.clone()),
                            FailReason::Internal(e.to_string()),
                        ));
                        continue;
                    }
                };

                let Some(old) = old else {
                    failures.push(DocFailure::new(
                        None,
                        Some(external_id),
                        FailReason::NotFound,
                    ));
                    continue;
                };

                match db.upsert_document(input, IndexMode::StoreAndIndex) {
                    Ok(()) => {
                        replaced += 1;
                        old_internal_ids.push(old.internal_id);
                        written_ids.push(external_id);
                    }
                    Err(e) => {
                        failures.push(DocFailure::new(
                            None,
                            Some(external_id),
                            FailReason::Internal(e.to_string()),
                        ));
                    }
                }
            }

            db.flush()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
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

    pub fn upsert(
        &mut self,
        inputs: Vec<DocumentInput>,
        user: String,
    ) -> Result<InsertReport, CorelamoError> {
        let started = std::time::Instant::now();
        let count = inputs.len();

        //TODO: batch support plzs
        let mut inserted: u32 = 0;
        let mut failures: Vec<DocFailure> = Vec::new();
        let mut old_internal_ids = Vec::new();
        let mut written_ids = Vec::new();
        let _offset = self
            .wal_append_record(&WalRecord::Upsert(inputs.clone()))
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;
        //vajag info log?
        {
            let db = self
                .db_mut()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;

            for input in inputs {
                let external_id = input.external_id.clone();

                let old = match db.get_document(&external_id) {
                    Ok(doc) => doc,
                    Err(e) => {
                        failures.push(DocFailure::new(
                            None,
                            Some(external_id.clone()),
                            FailReason::Internal(e.to_string()),
                        ));
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
                        failures.push(DocFailure::new(
                            None,
                            Some(external_id),
                            FailReason::Internal(e.to_string()),
                        ));
                    }
                }
            }

            db.flush()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
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

    pub fn flush(&mut self) -> Result<(), CorelamoError> {
        self.db_mut()
            .map_err(|e| CorelamoError::Internal(e.to_string()))?
            .flush()
            .map_err(|e| CorelamoError::Internal(e.to_string()))
    }

    pub fn progress(&self) -> Arc<ReindexProgress> {
        Arc::clone(self.stats.reindex_progress())
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

    pub fn clear(&mut self) -> Result<(), CorelamoError> {
        let _offset = self
            .wal_append_record(&WalRecord::Clear)
            .map_err(|e| CorelamoError::Internal(format!("wal append failed: {e}")))?;
        //vajag info log?
        if let Some(worker) = self.compaction_worker.take() {
            worker.stop()?;
            info!(self.log, "compaction worker stopped for clear"; "shard_id" => self.shard_id);
        }
        if let Some(db) = self.db.take()
            && let Err(e) = db.shutdown()
        {
            warn!(self.log, "clear: shutdown of old database failed"; "shard_id" => self.shard_id, "error" => %e);
        }
        let index_root = self.root.join("index");
        let store_path = self.root.join("documents.bin");

        std::fs::remove_dir_all(&index_root).ok();
        std::fs::remove_dir_all(self.root.join("index.new")).ok();
        std::fs::remove_dir_all(self.root.join("index.old")).ok();
        std::fs::remove_file(&store_path).ok();
        // outright WAL reset
        self.wal
            .reset()
            .map_err(|e| CorelamoError::Internal(format!("wal reset failed: {e}")))?;
        self.wal
            .write_checkpoint(0)
            .map_err(|e| CorelamoError::Internal(format!("checkpoint write failed: {e}")))?;

        //restart
        let analyzer = self.analyzer();
        let index = LsmIndex::persistent(&index_root, self.options.runtime.flush_threshold)?;
        let store = BinaryDocumentStore::open_with_maps(
            &store_path,
            self.shared.docs.clone(),
            self.shared.internal_to_external.clone(),
        )?;
        let db = SearchDatabase::with_shard_policy_and_snapshot(
            store,
            index,
            analyzer,
            self.policy.clone(),
            self.shard_id,
            self.shared.snapshot.clone(),
        )?;

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
        self.publish_stats();
        info!(self.log, "shard cleared"; "shard_id" => self.shard_id);
        Ok(())
    }

    pub fn backup_full(
        &mut self,
        shard_backup_path: PathBuf,
        backup_id: String,
    ) -> Result<BackupManifest, CorelamoError> {
        if let Some(worker) = self.compaction_worker.take() {
            worker
                .stop()
                .map_err(|e| CorelamoError::Internal(format!("failed to stop compaction: {e}")))?;
        }

        let progress = self.stats.backup_progress().clone();
        let result = self
            .backup
            .create_full_backup(
                &self.root,
                &shard_backup_path,
                &backup_id,
                &self.wal,
                &progress,
            )
            .map_err(|e| CorelamoError::Internal(e.to_string()));

        if self.options.enable_background_compaction {
            if let Ok(db) = self.db_ref() {
                self.compaction_worker = Some(CompactionWorker::start(
                    db.index_sender(),
                    self.options.runtime.compaction,
                    self.options.compaction_interval,
                ));
            }
        }

        result
    }

    pub fn backup_incremental(
        &mut self,
        shard_backup_path: PathBuf,
        backup_id: String,
    ) -> Result<Option<BackupManifest>, CorelamoError> {
        self.backup
            .create_incremental_backup(&shard_backup_path, &backup_id, &self.wal)
            .map_err(|e| CorelamoError::Internal(e.to_string()))
    }

    pub fn restore_backup(&mut self, backup_id: &str) -> Result<(), CorelamoError> {
        let root = self.root.clone();

        self.stop()?;
        self.backup
            .restore_chain(&backup_id, &root, &mut self.wal)
            .map_err(|e| CorelamoError::Internal(e.to_string()))?;
        self.start()?;

        Ok(())
    }

    pub fn restore_from_backup(
        &mut self,
        backup_id: &str,
        target_dir: &Path,
        wal: &mut Wal,
    ) -> Result<(), CorelamoError> {
        self.backup
            .restore_chain(backup_id, target_dir, wal)
            .map_err(|e| CorelamoError::Internal(e.to_string()))
    }
    fn publish_stats(&self) {
        let Ok(db) = self.db_ref() else {
            return;
        };
        let Ok(stats) = db.index_stats() else {
            return;
        };
        self.stats.publish(db.document_count(), &stats);
    }
    pub fn list_backups(&self) -> Result<Vec<BackupManifest>, CorelamoError> {
        self.backup
            .list_backups()
            .map_err(|e| CorelamoError::Internal(e.to_string()))
    }
}
