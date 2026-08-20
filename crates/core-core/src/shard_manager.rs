use crate::ShardDb;
use crate::metrics::DbStats;
use crate::reindex::{ ReindexJob, ReindexPool };
use crate::shard_db::DatabaseStats;
use crate::shard_worker::{ self, ShardCmd, ShardHandle };
use crate::{ DatabaseOptions, shard_for };
use core_backup::backup::BackupManifest;
use core_backup::backup::compress_file;
use core_backup::progress::BackupProgress;
use core_index::analyzer::Analyzer;
use core_index::document::IndexPolicy;
use core_index::types::{ ShardId, shard_of };
use core_protocol::command_reponse_definitions::{ LookupCommand, LookupResponse, SearchCommand };
use core_protocol::errors::CorelamoError;
use core_query::SearchHit;
use core_query::query_string_parser::parse_and_analyze;
use core_storage::document_store::StoredDocument;
use core_storage::search_database::{ DeleteReport, InsertReport, ReplaceReport };
use core_storage::search_database::{ DocumentInput, SearchDocumentHit };
use crossbeam_channel::bounded;
use parking_lot::RwLock;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::path::{ Path, PathBuf };
use std::sync::Arc;
use std::thread::JoinHandle;
use tokio::task::JoinSet;
pub struct ShardManager {
    shards: Vec<ShardHandle>,
    joins: Vec<JoinHandle<()>>, //JoinHandle atgriez ka thread uztaisits
    root: PathBuf,
    policy: RwLock<IndexPolicy>,
    options: RwLock<DatabaseOptions>,
    analyzer: Analyzer,
    reindex_pool: ReindexPool,
    db_stats: Arc<DbStats>,
    backup_dir: PathBuf,
}

impl ShardManager {
    const CONFIG_PATH_NAME: &'static str = "config.toml";
    const DEFAULT_QUEUE_DEPTH: usize = 256;

    fn config_path(root: &Path) -> PathBuf {
        root.join(Self::CONFIG_PATH_NAME)
    }

    pub fn all_alive(&self) -> bool {
        self.shards.iter().all(|h| h.is_alive())
    }

    pub fn record_search(&self, failed: bool, elapsed: std::time::Duration) {
        self.db_stats.record_search(failed, elapsed);
    }

    pub fn create(root: PathBuf, options: DatabaseOptions) -> Result<Self, CorelamoError> {
        if options.shard_count == 0 {
            return Err(CorelamoError::InvalidData("num_shards must be > 0".to_string()));
        }

        let shards_dir = root.join("shards");
        std::fs::create_dir_all(&shards_dir)?;

        //single policy and config
        let mut policy = IndexPolicy::default_document();
        policy.save(&root)?;
        options.save_to_file(Self::config_path(&root))?;
        let backup_dir = root.join("backups");
        let db_stats = DbStats::new(options.shard_count as usize);
        let mut shards = Vec::new();
        let mut joins = Vec::new();

        for shard_id in 0..options.shard_count {
            let shard_root = shards_dir.join(format!("shard-{}", shard_id));
            let db = ShardDb::create_shard(
                shard_root,
                &root,
                ShardId::from(shard_id),
                options.clone(),
                policy.clone(),
                db_stats.handle(shard_id as usize)
            )?;
            let (handle, join) = shard_worker::spawn(
                db,
                Self::DEFAULT_QUEUE_DEPTH,
                options.bootable
            )?;
            shards.push(handle);
            joins.push(join);
        }
        Ok(Self {
            shards,
            joins,
            root,
            policy: RwLock::new(policy),
            options: RwLock::new(options),
            analyzer: Analyzer::new(),
            reindex_pool: ReindexPool::start(1),
            db_stats,
            backup_dir,
        })
    }

    pub fn get_logs(&self, date: Option<String>) -> Result<String, CorelamoError> {
        let mut all_lines: Vec<String> = Vec::new();
        for h in &self.shards {
            let logs = h.get_logs_direct(date.clone())?;
            for line in logs.lines() {
                if !line.trim().is_empty() {
                    all_lines.push(line.to_string());
                }
            }
        }
        //nu kas tas ir bet sakartojas pareizi lmao
        all_lines.sort_by(|a, b| {
            let a_ts = a.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            let b_ts = b.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            a_ts.cmp(&b_ts)
        });
        Ok(all_lines.join("\n") + (if all_lines.is_empty() { "" } else { "\n" }))
    }

    pub fn clear_logs(&self) -> Result<(), CorelamoError> {
        for h in &self.shards {
            h.clear_logs_direct()?;
        }

        let logs_dir = self.root.join("logs");
        if logs_dir.exists() {
            for entry in std::fs::read_dir(&logs_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() && path.extension().map_or(false, |e| e == "log") {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        Ok(())
    }

    pub fn create_and_start(
        root: PathBuf,
        options: DatabaseOptions
    ) -> Result<Self, CorelamoError> {
        Self::create(root, options)
    }

    pub async fn start(&self) -> Result<(), CorelamoError> {
        let mut set = JoinSet::new();
        for h in &self.shards {
            let handle = h.clone();
            set.spawn(async move { handle.start().await });
        }

        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(format!("shard task panicked: {je}"))
                        );
                    }
                }
                Ok(Ok(())) => {}
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn all_readable(&self) -> bool {
        self.shards.iter().all(|h| h.is_running())
    }

    pub fn all_running(&self) -> bool {
        self.shards.iter().all(|h| h.is_running())
    }

    pub async fn stop(&self) -> Result<(), CorelamoError> {
        let mut set = JoinSet::new();
        for h in &self.shards {
            let handle = h.clone();
            set.spawn(async move { handle.stop().await });
        }

        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(format!("shard task panicked: {je}"))
                        );
                    }
                }
                Ok(Ok(())) => {}
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub async fn restart(&self) -> Result<(), CorelamoError> {
        self.stop().await?;
        self.start().await
    }

    pub async fn upsert(&self, inputs: Vec<DocumentInput>) -> Result<InsertReport, CorelamoError> {
        let mut by_shard: HashMap<usize, Vec<DocumentInput>> = HashMap::new();
        for input in inputs {
            by_shard.entry(self.shard_index_for(&input.external_id)).or_default().push(input);
        }

        let mut set = JoinSet::new();
        for (idx, batch) in by_shard {
            let handle = self.shards[idx].clone();
            set.spawn(async move { handle.upsert(batch).await });
        }

        let mut report = InsertReport {
            inserted: 0,
            failures: Vec::new(),
        };
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(r)) => {
                    report.inserted += r.inserted;
                    report.failures.extend(r.failures);
                }
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(format!("shard task panicked: {je}"))
                        );
                    }
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(report)
    }

    pub async fn replace(
        &self,
        inputs: Vec<DocumentInput>
    ) -> Result<ReplaceReport, CorelamoError> {
        let mut by_shard: HashMap<usize, Vec<DocumentInput>> = HashMap::new();
        for input in inputs {
            by_shard.entry(self.shard_index_for(&input.external_id)).or_default().push(input);
        }

        let mut set = JoinSet::new();
        for (idx, batch) in by_shard {
            let handle = self.shards[idx].clone();
            set.spawn(async move { handle.replace(batch).await });
        }

        let mut report = ReplaceReport {
            replaced: 0,
            failures: Vec::new(),
        };
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(r)) => {
                    report.replaced += r.replaced;
                    report.failures.extend(r.failures);
                }
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(format!("shard task panicked: {je}"))
                        );
                    }
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(report)
    }

    pub async fn delete(&self, ids: Vec<String>) -> Result<DeleteReport, CorelamoError> {
        if ids.is_empty() {
            return Ok(DeleteReport {
                deleted: 0,
                failures: Vec::new(),
            });
        }
        let mut position: HashMap<String, usize> = HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            position.entry(id.clone()).or_insert(i);
        }
        let mut by_shard: HashMap<usize, Vec<String>> = HashMap::new();
        for id in ids {
            by_shard.entry(self.shard_index_for(&id)).or_default().push(id);
        }

        let mut set = JoinSet::new();
        for (idx, batch) in by_shard {
            let handle = self.shards[idx].clone();
            set.spawn(async move { handle.delete(batch).await });
        }

        let mut report = DeleteReport {
            deleted: 0,
            failures: Vec::new(),
        };
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(r)) => {
                    report.deleted += r.deleted;
                    report.failures.extend(r.failures);
                }
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(format!("shard task panicked: {je}"))
                        );
                    }
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }

        for failure in &mut report.failures {
            if let Some(id) = &failure.id {
                failure.index = position.get(id).copied();
            }
        }
        Ok(report)
    }

    pub async fn clear_all(&self) -> Result<(), CorelamoError> {
        let mut set = JoinSet::new();
        for h in &self.shards {
            let handle = h.clone();
            set.spawn(async move { handle.clear().await });
        }

        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(format!("shard task panicked: {je}"))
                        );
                    }
                }
                Ok(Ok(())) => {}
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn load(root: PathBuf) -> Result<Self, CorelamoError> {
        let shards_dir = root.join("shards");

        if !shards_dir.exists() {
            return Err(
                CorelamoError::NotFound(
                    format!("shards directory not found at {}", shards_dir.display())
                )
            );
        }

        let mut shard_paths = Vec::new();
        for entry in std::fs::read_dir(&shards_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                shard_paths.push(entry.path());
            }
        }
        shard_paths.sort();

        let policy_path = root.join(IndexPolicy::POLICY_FILE_NAME);
        let config_path = Self::config_path(&root);

        if !policy_path.exists() {
            return Err(
                CorelamoError::NotFound(format!("policy not found at {}", policy_path.display()))
            );
        }

        //FIX: we probably need a load_or_default for policy too
        let policy = IndexPolicy::load(&root)?;
        let options = DatabaseOptions::load_or_default(&config_path);
        let backup_dir = root.join("backups");
        let db_stats = DbStats::new(shard_paths.len());
        let mut shards = Vec::new();
        let mut joins = Vec::new();
        for (i, shard_path) in shard_paths.iter().enumerate() {
            let db = ShardDb::load(shard_path, &root, &policy, &options, db_stats.handle(i))?;
            let (handle, join) = shard_worker::spawn(
                db,
                Self::DEFAULT_QUEUE_DEPTH,
                options.bootable
            )?;
            shards.push(handle);
            joins.push(join);
        }

        Ok(Self {
            shards,
            joins,
            root,
            policy: RwLock::new(policy),
            options: RwLock::new(options),
            analyzer: Analyzer::new(),
            reindex_pool: ReindexPool::start(1),
            db_stats,
            backup_dir,
        })
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn shutdown(self) -> Result<(), CorelamoError> {
        let pending: Vec<_> = self.shards
            .iter()
            .map(|h| {
                let (rtx, rrx) = bounded(1);
                let _ = h.send_raw(ShardCmd::Shutdown { resp: rtx });
                rrx
            })
            .collect();

        let mut first_err = None;
        for rx in pending {
            match rx.recv() {
                Ok(Err(e)) if first_err.is_none() => {
                    first_err = Some(e);
                }
                _ => {}
            }
        }
        for j in self.joins {
            let _ = j.join();
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn policy(&self) -> IndexPolicy {
        self.policy.read().clone()
    }

    pub fn options(&self) -> DatabaseOptions {
        self.options.read().clone()
    }

    pub async fn set_policy_all(&self, mut policy: IndexPolicy) -> Result<(), CorelamoError> {
        policy.validate()?;
        policy.resolve(&self.root)?;

        let mut set = JoinSet::new();
        for h in &self.shards {
            let handle = h.clone();
            let p = policy.clone();
            set.spawn(async move { handle.set_policy(p).await });
        }

        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(format!("shard task panicked: {je}"))
                        );
                    }
                }
                Ok(Ok(())) => {}
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }

        policy.save(&self.root)?;
        *self.policy.write() = policy;

        Ok(())
    }

    pub async fn set_options_all(&self, options: DatabaseOptions) -> Result<(), CorelamoError> {
        let mut set = JoinSet::new();
        for h in &self.shards {
            let handle = h.clone();
            let o = options.clone();
            set.spawn(async move { handle.set_config(o).await });
        }

        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(format!("shard task panicked: {je}"))
                        );
                    }
                }
                Ok(Ok(())) => {}
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }

        options.save_to_file(&Self::config_path(&self.root))?;
        *self.options.write() = options;
        Ok(())
    }

    pub fn reindex(&self) -> Result<(), CorelamoError> {
        if !self.db_stats.begin_reindex(self.shards.len()) {
            return Err(CorelamoError::Busy("reindex already in progress".into()));
        }
        let mut pending = Vec::with_capacity(self.shards.len());
        for h in &self.shards {
            let (rtx, rrx) = bounded(1);
            h.send_raw(ShardCmd::PrepareReindex { resp: rtx }).map_err(|(e, _)| e)?;
            pending.push((h, rrx));
        }
        //savac params
        let mut tickets = Vec::with_capacity(pending.len());
        for (h, rx) in pending {
            let params = rx
                .recv()
                .map_err(|_| CorelamoError::Internal("Shard died during reindex".into()))??;
            tickets.push((h, params));
        }
        //nodod reindex pool
        for (h, params) in tickets {
            self.db_stats.add_reindex_total(params.doc_count as u64);
            self.reindex_pool.submit(ReindexJob {
                params,
                shard_tx: h.command_sender(),
                progress: Arc::clone(h.progress()),
                stats: Arc::clone(&self.db_stats),
            })?;
        }
        Ok(())
    }
    //paligfunkcija no viber
    fn shard_index_for(&self, external_id: &str) -> usize {
        shard_for(external_id, self.shards.len() as u16) as usize
    }

    fn group_by_shard<'a>(&self, ids: &'a [String]) -> HashMap<usize, Vec<&'a String>> {
        let mut by_shard: HashMap<usize, Vec<&'a String>> = HashMap::new();
        for id in ids {
            by_shard.entry(self.shard_index_for(id)).or_default().push(id);
        }
        by_shard
    }

    pub fn stats(&self) -> DatabaseStats {
        let mut stats = self.db_stats.snapshot();
        stats.restoring = self.shards.iter().any(|h| h.is_restoring());
        stats
    }

    pub async fn insert(&self, inputs: Vec<DocumentInput>) -> Result<InsertReport, CorelamoError> {
        let started = std::time::Instant::now();

        let mut by_shard: HashMap<usize, Vec<DocumentInput>> = HashMap::new();
        for input in inputs {
            by_shard.entry(self.shard_index_for(&input.external_id)).or_default().push(input);
        }

        let mut set = JoinSet::new();
        for (idx, batch) in by_shard {
            let handle = self.shards[idx].clone();
            set.spawn(async move { handle.insert(batch).await });
        }

        let mut report = InsertReport {
            inserted: 0,
            failures: Vec::new(),
        };

        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(r)) => {
                    report.inserted += r.inserted;
                    report.failures.extend(r.failures);
                }
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(join_err) => {
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(format!("shard task panicked: {join_err}"))
                        );
                    }
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        self.db_stats.record_indexing(false, started.elapsed());
        Ok(report)
    }

    pub fn lookup(&self, command: &LookupCommand) -> Result<LookupResponse, CorelamoError> {
        let by_shard = self.group_by_shard(&command.ids);
        let policy = self.policy.read().clone();

        let mut all_docs = Vec::new();
        let mut all_not_found = Vec::new();
        for (idx, shard_ids) in by_shard {
            let ids: Vec<String> = shard_ids
                .iter()
                .map(|s| s.to_string())
                .collect();
            let response = self.shards[idx].lookup_direct(
                &ids,
                command.return_fields.as_ref(),
                &policy
            )?;
            all_docs.extend(response.docs);
            all_not_found.extend(response.not_found);
        }
        Ok(LookupResponse {
            docs: all_docs,
            not_found: all_not_found,
        })
    }

    pub fn retrieve(
        &self,
        ids: Vec<String>
    ) -> Result<Vec<(String, Option<StoredDocument>)>, CorelamoError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut position: HashMap<String, usize> = HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            position.entry(id.clone()).or_insert(i);
        }
        let mut by_shard: HashMap<usize, Vec<String>> = HashMap::new();
        for id in ids {
            by_shard.entry(self.shard_index_for(&id)).or_default().push(id);
        }

        let mut out = Vec::with_capacity(position.len());
        for (idx, batch) in by_shard {
            out.extend(self.shards[idx].get_document_direct(&batch)?);
        }
        out.sort_by_key(|(id, _)| position.get(id).copied().unwrap_or(usize::MAX));
        Ok(out)
    }

    pub fn search(&self, command: &SearchCommand) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        let limit = command.docs.unwrap_or(10);
        let offset = command.offset.unwrap_or(0);
        let fetch = offset.saturating_add(limit);

        let query = parse_and_analyze(&command.query, &self.analyzer)?;
        let policy = self.policy.read().clone();

        let mut all_hits: Vec<SearchHit> = Vec::new();
        for h in &self.shards {
            let hits = h.rank_top_k(query.as_ref(), command.filters.as_ref(), fetch, &policy)?;
            all_hits.extend(hits);
        }
        all_hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });
        let page: Vec<SearchHit> = all_hits.into_iter().skip(offset).take(limit).collect();

        let mut by_shard: HashMap<usize, Vec<SearchHit>> = HashMap::new();
        for hit in page {
            by_shard
                .entry(shard_of(hit.doc_id).0 as usize)
                .or_default()
                .push(hit);
        }

        let mut resolved: Vec<SearchDocumentHit> = Vec::new();
        for (idx, hits) in by_shard {
            resolved.extend(
                self.shards[idx].resolve_hits_direct(hits, command.return_fields.as_ref(), &policy)?
            );
        }

        resolved.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.internal_id.cmp(&b.internal_id))
        });

        Ok(resolved)
    }

    pub fn try_start_backup(&self) -> Result<(), CorelamoError> {
        if self.db_stats.try_begin_restore() {
            return Err(CorelamoError::Busy("restore in progress".into()));
        }
        if !self.db_stats.begin_backup(self.shards.len()) {
            return Err(CorelamoError::Busy("backup already in progress".into()));
        }
        Ok(())
    }

    pub fn try_start_restore(&self) -> Result<(), CorelamoError> {
        if self.db_stats.backup_progress().is_running() {
            return Err(CorelamoError::Busy("backup in progress".into()));
        }
        if !self.db_stats.try_begin_restore() {
            return Err(CorelamoError::Busy("restore already in progress".into()));
        }
        Ok(())
    }

    pub async fn backup_full(&self) -> Result<Vec<BackupManifest>, CorelamoError> {
        let backup_id = format!("full_{}", chrono::Utc::now().format("%Y-%m-%d_%H-%M-%S"));
        let backup_root = self.backup_dir.join(&backup_id);
        fs::create_dir_all(&backup_root).map_err(|e| CorelamoError::Internal(e.to_string()))?;
        for (src, dst) in &[
            (Self::CONFIG_PATH_NAME, "config.toml.gz"),
            (IndexPolicy::POLICY_FILE_NAME, "policy.toml.gz"),
            (IndexPolicy::REGISTRY_FILE_NAME, "xpath_registry.toml.gz"),
        ] {
            let src_path = self.root.join(src);
            if src_path.exists() {
                compress_file(&src_path, &backup_root.join(dst), &BackupProgress::new()).map_err(|e|
                    CorelamoError::Internal(e.to_string())
                )?;
            }
        }
        let mut set = JoinSet::new();
        for (i, shard) in self.shards.iter().enumerate() {
            let handle = shard.clone();
            let shard_backup_path = backup_root.join(format!("shard-{i}"));
            let bid = backup_id.clone();
            set.spawn(async move { handle.backup_full(shard_backup_path, bid).await });
        }
        let mut manifests = Vec::with_capacity(self.shards.len());
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(manifest)) => {
                    self.db_stats.finish_shard_backup(true);
                    manifests.push(manifest);
                }
                Ok(Err(e)) => {
                    self.db_stats.finish_shard_backup(false);
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    self.db_stats.finish_shard_backup(false);
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(format!("shard backup panicked: {je}"))
                        );
                    }
                }
            }
        }
        if let Some(e) = first_err {
            let _ = fs::remove_dir_all(&backup_root);
            return Err(e);
        }
        Ok(manifests)
    }

    pub async fn backup_incremental(&self) -> Result<Vec<Option<BackupManifest>>, CorelamoError> {
        let backup_id = format!("incr_{}", chrono::Utc::now().format("%Y-%m-%d_%H-%M-%S"));
        let backup_root = self.backup_dir.join(&backup_id);

        fs::create_dir_all(&backup_root).map_err(|e| CorelamoError::Internal(e.to_string()))?;

        let mut set = JoinSet::new();
        for (i, shard) in self.shards.iter().enumerate() {
            let handle = shard.clone();
            let shard_backup_path = backup_root.join(format!("shard-{i}"));
            let bid = backup_id.clone();
            set.spawn(async move { handle.backup_incremental(shard_backup_path, bid).await });
        }

        let mut manifests = Vec::with_capacity(self.shards.len());
        if manifests.iter().all(|m: &Option<BackupManifest>| m.is_none()) {
            let _ = fs::remove_dir_all(&backup_root);
        }
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(manifest)) => manifests.push(manifest),
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    if first_err.is_none() {
                        first_err = Some(
                            CorelamoError::Internal(
                                format!("shard incremental backup panicked: {je}")
                            )
                        );
                    }
                }
            }
        }

        if let Some(e) = first_err {
            let _ = fs::remove_dir_all(&backup_root);
            return Err(e);
        }
        Ok(manifests)
    }

    pub async fn restore_backup(&self, backup_id: &str) -> Result<(), CorelamoError> {
        let mut failures = Vec::new();
        for shard in &self.shards {
            if let Err(e) = shard.restore_backup(backup_id).await {
                failures.push(format!("shard {}: {}", shard.id(), e));
            }
        }
        self.db_stats.finish_restore(failures.is_empty());

        if failures.is_empty() {
            Ok(())
        } else {
            Err(
                CorelamoError::Internal(
                    //janomaina
                    format!(
                        "restore failed on {} of {} shards: {}",
                        failures.len(),
                        self.shards.len(),
                        failures.join("; ")
                    )
                )
            )
        }
    }
    pub fn finish_restore(&self, ok:bool) {
        self.db_stats.finish_restore(ok);
    }
    //to check before restore if there are any backups
    pub fn has_backup(&self, backup_id: &str) -> bool {
        self.backup_dir.join(backup_id).is_dir()
    }
    // impl ShardManager
    pub async fn list_backups(&self) -> Result<Vec<BackupManifest>, CorelamoError> {
        let shard = self.shards.first().ok_or_else(|| CorelamoError::Internal("no shards".into()))?;
        shard.list_backups().await
    }
}
