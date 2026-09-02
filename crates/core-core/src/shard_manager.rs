use crate::ShardDb;
use crate::metrics::DbStats;
use crate::reindex::{ReindexJob, ReindexPool};
use crate::shard_db::DatabaseStats;
use crate::shard_worker::{self, ShardCmd, ShardHandle};
use crate::{DatabaseOptions, shard_for};
use core_backup::backup::compress_file;
use core_backup::backup::{BackupManifest, BackupType};
use core_backup::progress::BackupProgress;
use core_index::analyzer::Analyzer;
use core_index::document::IndexPolicy;
use core_index::document::all_fields::AllFields;
use core_index::document::policy::IndexKind;
use core_index::lsm::index_worker::Phase;
use core_index::types::{ShardId, XPathId, shard_of};
use core_protocol::command_reponse_definitions::{LookupCommand, LookupResponse, SearchCommand};
use core_protocol::errors::CorelamoError;
use core_query::query_string_parser::parse_and_analyze;
use core_query::{Query, SearchHit};
use core_storage::document_store::StoredDocument;
use core_storage::search_database::{DeleteReport, InsertReport, ReplaceReport, WordStats};
use core_storage::search_database::{DocumentInput, SearchDocumentHit};
use core_timing::timed;
use crossbeam_channel::bounded;
use parking_lot::RwLock;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};
use std::{fs, u8};
use tokio::task::JoinSet;

pub struct ShardManager {
    shards: Vec<ShardHandle>,
    joins: Vec<JoinHandle<()>>, //JoinHandle atgriez kad thread uztaisits
    root: PathBuf,
    policy: RwLock<IndexPolicy>,
    options: RwLock<DatabaseOptions>,
    analyzer: Analyzer,
    reindex_pool: ReindexPool,
    db_stats: Arc<DbStats>,
    backup_dir: PathBuf,
    all_fields: RwLock<AllFields>,
}

impl ShardManager {
    const DEFAULT_QUEUE_DEPTH: usize = 256;

    pub fn all_alive(&self) -> bool {
        self.shards.iter().all(|h| h.is_alive())
    }

    pub fn record_search(&self, failed: bool, elapsed: std::time::Duration) {
        self.db_stats.record_search(failed, elapsed);
    }

    #[timed(database_lifecycle)]
    pub fn create(
        root: PathBuf,
        options: DatabaseOptions,
        shard_count: u16,
    ) -> Result<Self, CorelamoError> {
        if shard_count == 0 {
            return Err(CorelamoError::InvalidData(
                "num_shards must be > 0".to_string(),
            ));
        }

        let shards_dir = root.join("shards");
        std::fs::create_dir_all(&shards_dir)?;

        //single policy and config
        let mut policy = IndexPolicy::default_document();
        policy.save(&root)?;
        options.save_to_file(&root)?;
        let backup_dir = root.join("backups");
        let db_stats = DbStats::new(shard_count as usize);
        let mut shards = Vec::new();
        let mut joins = Vec::new();
        let mut boot_rxs = Vec::new();
        let all_fields = AllFields::new();
        all_fields.save(&root)?;

        for shard_id in 0..shard_count {
            let shard_root = shards_dir.join(format!("shard-{}", shard_id));
            let db = ShardDb::create_shard(
                shard_root,
                &root,
                ShardId::from(shard_id),
                options.clone(),
                policy.clone(),
                db_stats.handle(shard_id as usize),
            )?;
            let (handle, join, boot_rx) =
                shard_worker::spawn(db, Self::DEFAULT_QUEUE_DEPTH, options.bootable)?;
            shards.push(handle);
            joins.push(join);
            boot_rxs.push((shard_id, boot_rx));
        }
        for (i, boot_rx) in boot_rxs {
            boot_rx.recv().map_err(|_| {
                CorelamoError::Internal(format!("shard {i} thread died during start"))
            })??;
        }
        Ok(Self {
            shards,
            joins,
            root,
            policy: RwLock::new(policy),
            options: RwLock::new(options),
            analyzer: Analyzer::new(),
            reindex_pool: ReindexPool::start(1),
            all_fields: RwLock::new(all_fields),
            db_stats,
            backup_dir,
        })
    }

    pub fn all_fields(&self) -> AllFields {
        self.all_fields.read().clone()
    }

    //helper
    pub fn update_all_fields_from_partial_replace(
        &self,
        items: &[(String, BTreeMap<String, String>)],
    ) -> Result<(), CorelamoError> {
        let mut all_fields_map = BTreeMap::new();
        for (_, fields) in items {
            all_fields_map.extend(fields.clone());
        }
        self.update_all_fields_from_fields(&all_fields_map)
    }
    //peak name
    #[timed(shard_manager_doc_modifying)]
    fn update_all_fields_from_fields(
        &self,
        fields: &BTreeMap<String, String>,
    ) -> Result<(), CorelamoError> {
        if fields.is_empty() {
            return Ok(());
        }

        let policy = self.policy.read().clone();
        let mut all_fields = self.all_fields.write().clone();
        let mut changed = false;

        for (xpath, _) in fields {
            let kind = policy
                .fields
                .iter()
                .find(|f| f.name == *xpath)
                .map(|f| f.index.clone())
                .unwrap_or(IndexKind::None);

            if all_fields.get_fields().get(xpath) != Some(&kind) {
                all_fields.get_fields_mut().insert(xpath.clone(), kind);
                changed = true;
            }
        }

        if changed {
            all_fields.save(&self.root)?;
            *self.all_fields.write() = all_fields;
        }

        Ok(())
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

    #[timed(database_lifecycle)]
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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard task panicked: {je}"
                        )));
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

    #[timed(database_lifecycle)]
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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard task panicked: {je}"
                        )));
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

    #[timed(database_lifecycle)]
    pub async fn restart(&self) -> Result<(), CorelamoError> {
        self.stop().await?;
        self.start().await
    }

    #[timed(shard_manager_doc_modifying)]
    pub async fn upsert(
        &self,
        inputs: Vec<DocumentInput>,
        user: String,
    ) -> Result<InsertReport, CorelamoError> {
        let mut by_shard: HashMap<usize, Vec<DocumentInput>> = HashMap::new();

        let mut all_fields_map = BTreeMap::new();
        for input in &inputs {
            all_fields_map.extend(input.fields.clone());
        }
        self.update_all_fields_from_fields(&all_fields_map)?;

        for input in inputs {
            by_shard
                .entry(self.shard_index_for(&input.external_id))
                .or_default()
                .push(input);
        }

        let mut set = JoinSet::new();
        for (idx, batch) in by_shard {
            let handle = self.shards[idx].clone();
            let user = user.clone();
            set.spawn(async move { handle.upsert(batch, user).await });
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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard task panicked: {je}"
                        )));
                    }
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(report)
    }

    #[timed(retrieve_opps)]
    pub async fn retrieve_one(&self, id: &str) -> Result<Option<StoredDocument>, CorelamoError> {
        let shard_idx = self.shard_index_for(id);
        let docs = self.shards[shard_idx]
            .get_document_direct(&[id.to_string()])
            .await?;
        Ok(docs.into_iter().next().and_then(|(_, doc)| doc))
    }

    #[timed(shard_manager_doc_modifying)]
    pub async fn replace(
        &self,
        inputs: Vec<DocumentInput>,
        user: String,
    ) -> Result<ReplaceReport, CorelamoError> {
        let mut by_shard: HashMap<usize, Vec<DocumentInput>> = HashMap::new();

        let mut all_fields_map = BTreeMap::new();
        for input in &inputs {
            all_fields_map.extend(input.fields.clone());
        }
        self.update_all_fields_from_fields(&all_fields_map)?;

        for input in inputs {
            by_shard
                .entry(self.shard_index_for(&input.external_id))
                .or_default()
                .push(input);
        }

        let mut set = JoinSet::new();
        for (idx, batch) in by_shard {
            let handle = self.shards[idx].clone();
            let user = user.clone();
            set.spawn(async move { handle.replace(batch, user).await });
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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard task panicked: {je}"
                        )));
                    }
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(report)
    }

    #[timed(shard_manager_doc_modifying)]
    pub async fn delete(
        &self,
        ids: Vec<String>,
        user: String,
    ) -> Result<DeleteReport, CorelamoError> {
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
            by_shard
                .entry(self.shard_index_for(&id))
                .or_default()
                .push(id);
        }

        let mut set = JoinSet::new();
        for (idx, batch) in by_shard {
            let handle = self.shards[idx].clone();
            let user = user.clone();
            set.spawn(async move { handle.delete(batch, user).await });
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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard task panicked: {je}"
                        )));
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

    #[timed(database_lifecycle)]
    pub async fn clear_all(&self) -> Result<(), CorelamoError> {
        let mut set = JoinSet::new();
        let all_fields = AllFields::new();
        all_fields.save(&self.root)?;
        *self.all_fields.write() = all_fields;

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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard task panicked: {je}"
                        )));
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

    //INFO: manual load is basically only used for rename_database so that it doesnt start based on
    //boot : bool
    #[timed(database_lifecycle)]
    pub fn load(root: PathBuf, manual_load: bool) -> Result<Self, CorelamoError> {
        let shards_dir = root.join("shards");

        if !shards_dir.exists() {
            return Err(CorelamoError::NotFound(format!(
                "shards directory not found at {}",
                shards_dir.display()
            )));
        }

        let mut shard_paths = Vec::new();
        for entry in std::fs::read_dir(&shards_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                shard_paths.push(entry.path());
            }
        }

        shard_paths.sort_by_key(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|s| s.strip_prefix("shard-"))
                .and_then(|s| s.parse::<u8>().ok())
                .unwrap_or(u8::MAX)
        });

        let policy_path = root.join(IndexPolicy::POLICY_FILE_NAME);

        if !policy_path.exists() {
            return Err(CorelamoError::NotFound(format!(
                "policy not found at {}",
                policy_path.display()
            )));
        }

        let policy = IndexPolicy::load(&root)?;
        let options = DatabaseOptions::load_or_default(&root);

        let backup_dir = root.join("backups");
        let shard_count = shard_paths.len();
        let db_stats = DbStats::new(shard_count);
        let mut shards = Vec::new();
        let mut joins = Vec::new();
        let mut boot_rxs = Vec::new();
        let all_fields = AllFields::load(&root)?;

        //TODO: sitaa jobnutaa hujna nenotiek paraleeli, tapec start_database it leens
        for (i, shard_path) in shard_paths.iter().enumerate() {
            let db = ShardDb::load(shard_path, &root, &policy, &options, db_stats.handle(i))?;
            let (handle, join, boot_rx) = shard_worker::spawn(
                db,
                Self::DEFAULT_QUEUE_DEPTH,
                options.bootable && manual_load,
            )?;
            shards.push(handle);
            joins.push(join);
            boot_rxs.push((i, boot_rx));
        }
        for (i, boot_rx) in boot_rxs {
            boot_rx.recv().map_err(|_| {
                CorelamoError::Internal(format!("shard {i} thread died during start"))
            })??;
        }

        Ok(Self {
            shards,
            joins,
            root,
            policy: RwLock::new(policy),
            options: RwLock::new(options),
            analyzer: Analyzer::new(),
            reindex_pool: ReindexPool::start(1),
            all_fields: RwLock::new(all_fields),
            db_stats,
            backup_dir,
        })
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    #[timed(database_lifecycle)]
    pub fn shutdown(self) -> Result<(), CorelamoError> {
        let pending: Vec<_> = self
            .shards
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

    #[timed(database_lifecycle)]
    pub async fn set_policy_all(
        &self,
        mut policy: IndexPolicy,
        user: String,
    ) -> Result<(), CorelamoError> {
        policy.validate()?;
        policy.resolve(&self.root)?;

        let mut set = JoinSet::new();
        for h in &self.shards {
            let handle = h.clone();
            let p = policy.clone();
            let user = user.clone();
            set.spawn(async move { handle.set_policy(p, user).await });
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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard task panicked: {je}"
                        )));
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
        self.update_all_fields_from_policy()?;

        Ok(())
    }

    #[timed(database_lifecycle)]
    pub async fn set_options_all(
        &self,
        options: DatabaseOptions,
        user: String,
    ) -> Result<(), CorelamoError> {
        let mut set = JoinSet::new();
        for h in &self.shards {
            let handle = h.clone();
            let o = options.clone();
            let user = user.clone();
            set.spawn(async move { handle.set_config(o, user).await });
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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard task panicked: {je}"
                        )));
                    }
                }
                Ok(Ok(())) => {}
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }

        options.save_to_file(&self.root)?;
        *self.options.write() = options;
        Ok(())
    }

    #[timed(reindex)]
    pub fn reindex(&self) -> Result<(), CorelamoError> {
        if !self.db_stats.begin_reindex(self.shards.len()) {
            return Err(CorelamoError::Busy("reindex already in progress".into()));
        }
        let mut pending = Vec::with_capacity(self.shards.len());
        for h in &self.shards {
            let (rtx, rrx) = bounded(1);
            h.send_raw(ShardCmd::PrepareReindex { resp: rtx })
                .map_err(|(e, _)| e)?;
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
        //nodod reindex  pool
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

    #[timed(reindex)]
    pub fn abort_reindex(&self) {
        for h in &self.shards {
            h.progress().cancel();
            let _ = fs::remove_dir_all(self.root.join("index.new"));
        }
        self.db_stats.reindex_progress().set_phase(Phase::Cancelled);
    }
    fn shard_index_for(&self, external_id: &str) -> usize {
        shard_for(external_id, self.shards.len() as u16) as usize
    }

    fn group_by_shard<'a>(&self, ids: &'a [String]) -> HashMap<usize, Vec<&'a String>> {
        let mut by_shard: HashMap<usize, Vec<&'a String>> = HashMap::new();
        for id in ids {
            by_shard
                .entry(self.shard_index_for(id))
                .or_default()
                .push(id);
        }
        by_shard
    }

    pub fn stats(&self) -> DatabaseStats {
        let mut stats = self.db_stats.snapshot();
        stats.restoring = self.shards.iter().any(|h| h.is_restoring());
        stats
    }

    fn update_all_fields_from_policy(&self) -> Result<(), CorelamoError> {
        let policy = self.policy.read().clone();
        let mut all_fields = self.all_fields.read().clone();
        let mut changed = false;

        for (xpath, kind) in all_fields.get_fields_mut().iter_mut() {
            if let Some(field) = policy.fields.iter().find(|f| f.name == *xpath) {
                if *kind != field.index {
                    *kind = field.index.clone();
                    changed = true;
                }
            }
        }

        if changed {
            all_fields.save(&self.root)?;
            *self.all_fields.write() = all_fields;
        }

        Ok(())
    }

    #[timed(inserting)]
    pub async fn insert(
        &self,
        inputs: Vec<DocumentInput>,
        user: String,
    ) -> Result<InsertReport, CorelamoError> {
        let started = std::time::Instant::now();

        let mut all_fields_map = BTreeMap::new();
        for input in &inputs {
            all_fields_map.extend(input.fields.clone());
        }
        self.update_all_fields_from_fields(&all_fields_map)?;

        let mut by_shard: HashMap<usize, Vec<DocumentInput>> = HashMap::new();
        for input in inputs {
            by_shard
                .entry(self.shard_index_for(&input.external_id))
                .or_default()
                .push(input);
        }

        let mut set = JoinSet::new();
        for (idx, batch) in by_shard {
            let handle = self.shards[idx].clone();
            let user = user.clone();
            set.spawn(async move { handle.insert(batch, user).await });
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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard task panicked: {join_err}"
                        )));
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

    #[timed(retrieve_opps)]
    pub async fn lookup(&self, command: &LookupCommand) -> Result<LookupResponse, CorelamoError> {
        let by_shard = self.group_by_shard(&command.ids);
        let policy = self.policy.read().clone();

        let mut handles = Vec::new();
        for (idx, shard_ids) in by_shard {
            let ids: Vec<String> = shard_ids.iter().map(|s| s.to_string()).collect();
            let return_fields = command.return_fields.clone();
            let policy = policy.clone();
            let shard = self.shards[idx].clone();

            handles.push(tokio::spawn(async move {
                shard
                    .lookup_direct(&ids, return_fields.as_ref(), &policy)
                    .await
            }));
        }

        let mut all_docs = Vec::new();
        let mut all_not_found = Vec::new();
        for handle in handles {
            let response = handle.await.unwrap()?;
            all_docs.extend(response.docs);
            all_not_found.extend(response.not_found);
        }
        Ok(LookupResponse {
            docs: all_docs,
            not_found: all_not_found,
        })
    }

    #[timed(retrieve_opps)]
    pub async fn retrieve(
        &self,
        ids: Vec<String>,
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
            by_shard
                .entry(self.shard_index_for(&id))
                .or_default()
                .push(id);
        }

        let mut out = Vec::with_capacity(position.len());
        for (idx, batch) in by_shard {
            out.extend(self.shards[idx].get_document_direct(&batch).await?);
        }
        out.sort_by_key(|(id, _)| position.get(id).copied().unwrap_or(usize::MAX));
        Ok(out)
    }

    //policy fields parse before shards
    //japachecko to BM25

    #[timed(search)]
    pub fn hits_cmp(a: &SearchHit, b: &SearchHit) -> Ordering {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    }

    #[timed(search)]
    pub async fn search(
        &self,
        command: &SearchCommand,
    ) -> Result<Vec<SearchDocumentHit>, CorelamoError> {
        let limit = command.docs.unwrap_or(10);
        let offset = command.offset.unwrap_or(0);
        let fetch = offset.saturating_add(limit);
        if fetch == 0 {
            return Ok(Vec::new());
        }

        let query = Arc::new(parse_and_analyze(&command.query, &self.analyzer)?);
        let policy = self.policy.read().clone();

        let filters: Option<Arc<HashMap<String, (Option<Query>, XPathId)>>> =
            match command.filters.as_ref() {
                Some(fs) => {
                    let mut analyzed = HashMap::with_capacity(fs.len());
                    for (field, term) in fs {
                        if term.trim().is_empty() {
                            continue;
                        }
                        let field_pol = policy
                            .fields
                            .iter()
                            .find(|f| &f.name == field)
                            .filter(|f| f.index == IndexKind::Text)
                            .ok_or_else(|| CorelamoError::PathNotIndexed(field.clone()))?;
                        let query = parse_and_analyze(term, &self.analyzer)?;
                        analyzed.insert(field.clone(), (query, field_pol.xpath(&policy)));
                    }
                    Some(Arc::new(analyzed))
                }
                None => None,
            };

        let xpaths = Arc::new(policy.searchable_xpaths().collect::<Vec<_>>());

        let mut set = JoinSet::new();
        for handle in &self.shards {
            let handle = handle.clone();
            let query = Arc::clone(&query);
            let filters = filters.clone();
            let xpaths = Arc::clone(&xpaths);
            set.spawn_blocking(move || {
                handle.rank_top_k((*query).as_ref(), filters.as_deref(), &xpaths, fetch)
            });
        }

        let mut candidates: Vec<SearchHit> = Vec::new();
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(hits)) => candidates.extend(hits),
                Ok(Err(e)) if first_err.is_none() => {
                    first_err = Some(e);
                }
                Err(je) if first_err.is_none() => {
                    first_err = Some(CorelamoError::Internal(format!(
                        "shard search panicked: {je}"
                    )));
                }
                _ => {}
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }

        if candidates.len() > fetch {
            candidates.select_nth_unstable_by(fetch - 1, Self::hits_cmp);
            candidates.truncate(fetch);
        }
        candidates.sort_unstable_by(Self::hits_cmp);

        if offset >= candidates.len() {
            return Ok(Vec::new());
        }

        //for the resolve hit to save position
        let page: Vec<(usize, SearchHit)> = candidates
            .into_iter()
            .skip(offset)
            .take(limit)
            .enumerate()
            .collect();
        let page_len = page.len();

        let mut by_shard: HashMap<usize, Vec<(usize, SearchHit)>> = HashMap::new();
        for (pos, hit) in page {
            by_shard
                .entry(shard_of(hit.doc_id).0 as usize)
                .or_default()
                .push((pos, hit));
        }

        //resolve in paralel
        let mut set = JoinSet::new();
        for (idx, hits) in by_shard {
            let handle = self.shards[idx].clone();
            let return_fields = command.return_fields.clone();
            let policy = policy.clone();
            let positions: Vec<usize> = hits.iter().map(|(pos, _)| *pos).collect();
            let bare_hits: Vec<SearchHit> = hits.into_iter().map(|(_, h)| h).collect();
            set.spawn(async move {
                (
                    positions,
                    handle
                        .resolve_hits_direct(bare_hits, return_fields.as_ref(), &policy)
                        .await,
                )
            });
        }

        let mut resolved: Vec<Option<SearchDocumentHit>> = vec![None; page_len];
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok((positions, Ok(hits))) => {
                    for (pos, hit) in positions.into_iter().zip(hits) {
                        resolved[pos] = hit;
                    }
                }
                Ok((_, Err(e))) if first_err.is_none() => {
                    first_err = Some(e);
                }
                Err(je) if first_err.is_none() => {
                    first_err = Some(CorelamoError::Internal(format!(
                        "shard resolve panicked: {je}"
                    )));
                }
                _ => {}
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(resolved.into_iter().flatten().collect())
    }

    //viss ar backups
    #[timed(backup)]
    pub fn try_start_backup(&self) -> Result<(), CorelamoError> {
        if self.db_stats.restore_progress().is_running() {
            return Err(CorelamoError::Busy("restore in progress".into()));
        }
        if !self.db_stats.begin_backup(self.shards.len()) {
            return Err(CorelamoError::Busy("backup already in progress".into()));
        }
        Ok(())
    }

    #[timed(restore)]
    pub fn try_start_restore(&self) -> Result<(), CorelamoError> {
        if self.db_stats.backup_progress().is_running() {
            return Err(CorelamoError::Busy("backup in progress".into()));
        }
        if !self.db_stats.try_begin_restore() {
            return Err(CorelamoError::Busy("restore already in progress".into()));
        }
        Ok(())
    }

    #[timed(backup)]
    pub async fn backup_full(&self, user: String) -> Result<Vec<BackupManifest>, CorelamoError> {
        let backup_id = format!("full_{}", chrono::Utc::now().format("%Y-%m-%d_%H-%M-%S"));
        let backup_root = self.backup_dir.join(&backup_id);
        fs::create_dir_all(&backup_root).map_err(|e| CorelamoError::Internal(e.to_string()))?;
        for (src, dst) in &[
            (DatabaseOptions::CONFIG_FILE_NAME, "config.toml.gz"),
            (IndexPolicy::POLICY_FILE_NAME, "policy.toml.gz"),
            (IndexPolicy::REGISTRY_FILE_NAME, "xpath_registry.toml.gz"),
            (AllFields::FILE_NAME, "all_fields.toml.gz"),
        ] {
            let src_path = self.root.join(src);
            if src_path.exists() {
                compress_file(&src_path, &backup_root.join(dst), &BackupProgress::new())
                    .map_err(|e| CorelamoError::Internal(e.to_string()))?;
            }
        }
        let mut set = JoinSet::new();
        for (i, shard) in self.shards.iter().enumerate() {
            let handle = shard.clone();
            let shard_backup_path = backup_root.join(format!("shard-{i}"));
            let bid = backup_id.clone();

            //ko tik cilveks nedariitu lai uzvareetu rusta kompilatoru
            let user = user.clone();

            set.spawn(async move {
                handle
                    .backup_full(shard_backup_path, bid, user.clone())
                    .await
            });
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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard backup panicked: {je}"
                        )));
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

    #[timed(backup)]
    pub async fn backup_incremental(
        &self,
        user: String,
    ) -> Result<Vec<Option<BackupManifest>>, CorelamoError> {
        let backup_id = format!("incr_{}", chrono::Utc::now().format("%Y-%m-%d_%H-%M-%S"));
        let backup_root = self.backup_dir.join(&backup_id);

        fs::create_dir_all(&backup_root).map_err(|e| CorelamoError::Internal(e.to_string()))?;

        let mut set = JoinSet::new();
        for (i, shard) in self.shards.iter().enumerate() {
            let handle = shard.clone();
            let shard_backup_path = backup_root.join(format!("shard-{i}"));
            let bid = backup_id.clone();
            let user = user.clone();
            set.spawn(async move {
                handle
                    .backup_incremental(user, shard_backup_path, bid)
                    .await
            });
        }

        let mut manifests = Vec::with_capacity(self.shards.len());
        if manifests
            .iter()
            .all(|m: &Option<BackupManifest>| m.is_none())
        {
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
                        first_err = Some(CorelamoError::Internal(format!(
                            "shard incremental backup panicked: {je}"
                        )));
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

    #[timed(retrieve_opps)]
    pub async fn info_words(&self, words: Vec<String>) -> Result<Vec<WordStats>, CorelamoError> {
        if words.is_empty() {
            return Ok(Vec::new());
        }

        let xpaths = Arc::new(self.policy.read().searchable_xpaths().collect::<Vec<_>>());

        let mut set = JoinSet::new();
        for handle in &self.shards {
            let handle = handle.clone();
            let words = words.clone();
            let xpaths = Arc::clone(&xpaths);
            set.spawn_blocking(move || handle.info_words_direct(&words, &xpaths));
        }

        let mut merged: Vec<WordStats> = words
            .iter()
            .map(|word| WordStats {
                word: word.clone(),
                occurrences: 0,
                documents: 0,
            })
            .collect();

        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(shard_stats)) => {
                    for (i, s) in shard_stats.into_iter().enumerate() {
                        if let Some(m) = merged.get_mut(i) {
                            m.occurrences += s.occurrences;
                            m.documents += s.documents;
                        }
                    }
                }
                Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
                Err(je) if first_err.is_none() => {
                    first_err = Some(CorelamoError::Internal(format!(
                        "info-words shard task panicked: {je}"
                    )));
                }
                _ => {}
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(merged)
    }

    #[timed(restore)]
    pub async fn restore_backup(&self, backup_id: &str, user: String) -> Result<(), CorelamoError> {
        let mut failures = Vec::new();

        for shard in &self.shards {
            if let Err(e) = shard.restore_backup(backup_id, user.clone()).await {
                failures.push(format!("shard {}: {}", shard.id(), e));
            }
        }
        self.db_stats.finish_restore(failures.is_empty());

        if failures.is_empty() {
            Ok(())
        } else {
            Err(CorelamoError::Internal(
                //janomaina
                format!(
                    "restore failed on {} of {} shards: {}",
                    failures.len(),
                    self.shards.len(),
                    failures.join("; ")
                ),
            ))
        }
    }

    pub fn finish_restore(&self, ok: bool) {
        self.db_stats.finish_restore(ok);
    }

    pub fn finish_backup(&self, ok: bool) {
        self.db_stats.finish_backup(ok);
    }
    //to check before restore if there are any backups
    pub fn has_backup(&self, backup_id: &str) -> bool {
        self.backup_dir.join(backup_id).is_dir()
    }

    #[timed(backup)]
    pub fn list_backups(&self) -> Result<Vec<BackupManifest>, CorelamoError> {
        let mut merged: HashMap<String, BackupManifest> = HashMap::new();

        for shard in &self.shards {
            for manifest in shard.list_backups()? {
                merged
                    .entry(manifest.backup_id.clone())
                    .and_modify(|m| {
                        m.document_count += manifest.document_count;
                        if manifest.backup_type == BackupType::Incremental {
                            m.record_count += manifest.record_count;
                        }
                    })
                    .or_insert(manifest);
            }
        }

        Ok(merged.into_values().collect())
    }

    #[timed(backup)]
    pub async fn delete_backup(
        &self,
        backup_id: String,
        _user: String,
    ) -> Result<(), CorelamoError> {
        let backup_root = self.backup_dir.join(backup_id);
        fs::remove_dir_all(&backup_root).map_err(|e| CorelamoError::Internal(e.to_string()))?;
        Ok(())
    }

    #[timed(backup)]
    pub fn delete_backups_old(&self, lifetime: Duration) -> Result<(), CorelamoError> {
        let cutoff = SystemTime::now() - lifetime;
        let entries =
            fs::read_dir(&self.backup_dir).map_err(|e| CorelamoError::Internal(e.to_string()))?;
        for entry in entries.flatten() {
            let meta = entry
                .metadata()
                .map_err(|e| CorelamoError::Internal(e.to_string()))?;
            if meta.is_dir() {
                let modified = meta
                    .modified()
                    .map_err(|e| CorelamoError::Internal(e.to_string()))?;
                if modified < cutoff {
                    fs::remove_dir_all(entry.path())
                        .map_err(|e| CorelamoError::Internal(e.to_string()))?;
                }
            }
        }
        Ok(())
    }

    #[timed(backup)]
    pub fn start_backup_scheduler(self: &Arc<Self>) {
        let (inc_period, full_period) = {
            let o = self.options.read();
            (o.incremental_backup_interval, o.full_backup_interval)
        };
        if inc_period.is_zero() && full_period.is_zero() {
            return;
        }

        // interval() panics on zero -> treat zero as "never fires"
        let never = Duration::from_secs(u64::from(u32::MAX));
        let mut inc = tokio::time::interval(if inc_period.is_zero() {
            never
        } else {
            inc_period
        });
        let mut full = tokio::time::interval(if full_period.is_zero() {
            never
        } else {
            full_period
        });

        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            inc.tick().await; // first tick is immediate
            full.tick().await;

            loop {
                let is_full = tokio::select! {
                    _ = inc.tick() => false,
                    _ = full.tick() => true,
                };
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if this.try_start_backup().is_err() {
                    continue;
                }
                let ok = if is_full {
                    this.backup_full("System".to_string()).await.is_ok()
                } else {
                    this.backup_incremental("System".to_string()).await.is_ok()
                };
                this.finish_backup(ok);
                let lifetime = this.options.read().backup_lifetime;
                let _ = this.delete_backups_old(lifetime);
            }
        });
    }
}
