//include ALL
use crate::ShardDb;
use crate::metrics::{DbStats, EmbeddingStatus};
use crate::reindex::{PendingReindexJob, ReindexPool};
use crate::shard_db::DatabaseStats;
use crate::shard_worker::{self, PreparedShardSearch, ShardCmd, ShardHandle};
use crate::{DatabaseOptions, shard_for};
use core_backup::backup::compress_file;
use core_backup::backup::{BackupManifest, BackupType};
use core_backup::progress::BackupProgress;
use core_index::analyzer::Analyzer;
use core_index::document::IndexPolicy;
use core_index::document::all_fields::AllFields;
use core_index::document::policy::FieldKind;
use core_index::fuzzy::{FuzzyExpansion, FuzzySpec};
use core_index::lsm::index_worker::Phase;
use core_index::types::{DocId, ShardId, XPathId, shard_of};
use core_index::vector::{VectorHit, merge_hits};
use core_protocol::command_reponse_definitions::{
    DidYouMeanRequest, HighlightFieldOptions, LookupCommand, LookupResponse, SearchCommand,
    SortMode, TotalHits,
};
use core_protocol::errors::CorelamoError;
use core_query::dictionary::{
    DATABASE_FILE_NAME, DictionaryDocument, SynonymManager, SynonymRegistry,
};
use core_query::executor::{DidYouMeanReport, WordSuggestions};
use core_query::highlight::{
    HighlightMatcher, collect_highlight_matchers, field_texts, highlight_text,
};
use core_query::planner::RankPlan;
use core_query::resolver::{
    FieldQuery, compile_query, is_blank_query, parse_fuzziness, resolve_suggest_xpaths,
};
use core_query::{CorpusStats, SearchHit};
use core_storage::document_projections::id_path_to_strip;
use core_storage::document_store::StoredDocument;
use core_storage::search_database::{DeleteReport, InsertReport, ReplaceReport, WordStats};
use core_storage::search_database::{DocumentInput, SearchDocumentHit};
use core_timing::timed;
use crossbeam_channel::bounded;
use parking_lot::{Mutex, RwLock};
use simd_json::OwnedValue;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};
use tokio::task::JoinSet;

use crate::shard_manager_helpers::{
    compile_filters, fold_sort_bounds, order_blended, order_strict, resolve_sorts,
};
use core_embed::{Embedder, ModelRegistry};

//mististiskas konstantes
/// How many hits each list contributes to fusion. Deeper than a page, so a document ranked
/// moderately in both lists can still reach the top.
const FUSION_DEPTH: usize = 100;
/// The usual RRF constant: damps the difference between the very top ranks.
const RRF_K: f32 = 60.0;
/// Fewer words than this are usually names or codes, which word search already handles best.
const SEMANTIC_MIN_WORDS: usize = 3;
/// Automatic fusion needs at least this share of documents embedded (percent).
const SEMANTIC_MIN_PROGRESS: u8 = 95;
/// Largest result count a vector-only query returns (matches the total-hits cap).
const MAX_VECTOR_RESULTS: usize = 1000;
/// A query that is only `{"vector": "text"}` or `{"vector": {"text": "...", "field": "..."}}`.
struct VectorRequest {
    text: String,
    field: Option<String>,
}

fn vector_request(query: &OwnedValue) -> Result<Option<VectorRequest>, CorelamoError> {
    let OwnedValue::Object(obj) = query else {
        return Ok(None);
    };
    let Some(node) = obj.get("vector") else {
        return Ok(None);
    };
    if obj.len() != 1 {
        return Err(CorelamoError::InvalidData(
            "\"vector\" cannot be combined with other operators at the same level".into(),
        ));
    }
    let (text, field) = match node {
        OwnedValue::String(text) => (text.clone(), None),
        OwnedValue::Object(inner) => {
            let text = match inner.get("text") {
                Some(OwnedValue::String(text)) => text.clone(),
                _ => {
                    return Err(CorelamoError::InvalidData(
                        "\"vector\" object needs a \"text\" string".into(),
                    ));
                }
            };
            let field = match inner.get("field") {
                None => None,
                Some(OwnedValue::String(field)) => Some(field.clone()),
                Some(_) => {
                    return Err(CorelamoError::InvalidData(
                        "\"vector.field\" must be a string".into(),
                    ));
                }
            };
            (text, field)
        }
        _ => {
            return Err(CorelamoError::InvalidData(
                "\"vector\" must be a string or {\"text\": ..., \"field\": ...}".into(),
            ));
        }
    };
    if text.trim().is_empty() {
        return Err(CorelamoError::InvalidData(
            "\"vector\" text is empty".into(),
        ));
    }
    Ok(Some(VectorRequest { text, field }))
}

/// Vector fields of the policy, limited to `names` when given (full path or field name).
fn vector_xpaths(policy: &IndexPolicy, names: Option<&[String]>) -> Vec<XPathId> {
    policy
        .vector_fields()
        .into_iter()
        .filter(|field| {
            names.is_none_or(|names| {
                names
                    .iter()
                    .any(|name| (*name == field.full_path || *name == field.name))
            })
        })
        .map(|field| field.xpath(policy))
        .collect()
}
pub struct ShardManager {
    shards: Vec<ShardHandle>,
    root: PathBuf,
    policy: RwLock<IndexPolicy>,
    options: RwLock<DatabaseOptions>,
    analyzer: Analyzer,
    reindex_pool: ReindexPool,
    db_stats: Arc<DbStats>,
    backup_dir: PathBuf,
    all_fields: RwLock<AllFields>,
    // the database's synonym dictionary, dictionary.toml in its directory
    synonyms: Arc<SynonymManager>,
    models: Arc<ModelRegistry>,
    // taken and joined in shutdown(); a Mutex because shutdown() only gets &self
    joins: Mutex<Vec<JoinHandle<()>>>, //JoinHandle atgriez kad thread uztaisits
    // main, delete and rename all reach shutdown(), so it has to be safe twice
    shutdown_done: std::sync::atomic::AtomicBool,
}

impl ShardManager {
    const DEFAULT_QUEUE_DEPTH: usize = 256;

    #[timed(database_lifecycle)]
    pub fn create(
        root: PathBuf,
        options: DatabaseOptions,
        shard_count: u16,
        synonym_registry: &SynonymRegistry,
        models: Arc<ModelRegistry>,
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
        let synonyms = Arc::new(synonym_registry.open_database(&root)?);

        for shard_id in 0..shard_count {
            let shard_root = shards_dir.join(format!("shard-{}", shard_id));
            let db = ShardDb::create_shard(
                shard_root,
                &root,
                ShardId::from(shard_id),
                shard_count,
                options.clone(),
                policy.clone(),
                db_stats.handle(shard_id as usize),
                Arc::clone(&models),
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
            root,
            policy: RwLock::new(policy),
            options: RwLock::new(options),
            joins: Mutex::new(joins),
            shutdown_done: AtomicBool::new(false),
            analyzer: Analyzer::new(),
            reindex_pool: ReindexPool::start(shard_count as usize),
            all_fields: RwLock::new(all_fields),
            db_stats,
            backup_dir,
            synonyms,
            models,
        })
    }

    //INFO: manual load is basically only used for rename_database so that it doesnt start based on
    //boot : bool
    #[timed(database_lifecycle)]
    pub fn load(
        root: PathBuf,
        manual_load: bool,
        synonym_registry: &SynonymRegistry,
        models: Arc<ModelRegistry>,
    ) -> Result<Self, CorelamoError> {
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
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(u16::MAX)
        });

        // Routing returns a position in `shards`, while document ids carry the shard id, so
        // the shard at position i must be shard-i. Checked before any shard is started.
        let shard_count_u16 = u16::try_from(shard_paths.len()).map_err(|_| {
            CorelamoError::InvalidData(format!(
                "{} shard directories exceed the supported maximum of {}",
                shard_paths.len(),
                u16::MAX
            ))
        })?;
        for (position, path) in shard_paths.iter().enumerate() {
            let parsed = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|s| s.strip_prefix("shard-"))
                .and_then(|s| s.parse::<u16>().ok());
            if parsed.map(usize::from) != Some(position) {
                return Err(CorelamoError::InvalidData(format!(
                    "shard directory {} is at position {position}; shard directories must be shard-0 to shard-{}",
                    path.display(),
                    shard_paths.len() - 1
                )));
            }
        }

        let policy_path = root.join(IndexPolicy::POLICY_FILE_NAME);

        if !policy_path.exists() {
            return Err(CorelamoError::NotFound(format!(
                "policy not found at {}",
                policy_path.display()
            )));
        }

        let policy = IndexPolicy::load(&root)?;

        let options = match DatabaseOptions::load_from_file(&root) {
            Ok(o) => o,
            Err(e) => {
                return Err(CorelamoError::from(e));
            }
        };

        let backup_dir = root.join("backups");
        let shard_count = shard_paths.len();
        let db_stats = DbStats::new(shard_count);
        let mut shards = Vec::new();
        let mut joins = Vec::new();
        let mut boot_rxs = Vec::new();
        let all_fields = AllFields::load(&root)?;
        let synonyms = Arc::new(synonym_registry.open_database(&root)?);

        //TODO: sitaa jobnutaa hujna nenotiek paraleeli, tapec start_database it leens
        for (i, shard_path) in shard_paths.iter().enumerate() {
            let db = ShardDb::load(
                shard_path,
                &root,
                shard_count_u16,
                &policy,
                &options,
                db_stats.handle(i),
                Arc::clone(&models),
            )?;
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
            joins: Mutex::new(joins),
            shutdown_done: AtomicBool::new(false),
            root,
            policy: RwLock::new(policy),
            options: RwLock::new(options),
            analyzer: Analyzer::new(),
            reindex_pool: ReindexPool::start(shard_count as usize),
            all_fields: RwLock::new(all_fields),
            db_stats,
            backup_dir,
            synonyms,
            models,
        })
    }
    pub fn all_fields(&self) -> AllFields {
        self.all_fields.read().clone()
    }

    //peak name ik
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
                .map(|f| f.kind.clone())
                .unwrap_or(FieldKind::None);

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
        let policy = IndexPolicy::load(&self.root)?;
        let options = DatabaseOptions::load_from_file(&self.root)?;
        *self.policy.write() = policy.clone();
        *self.options.write() = options.clone();

        let mut set = JoinSet::new();
        for h in &self.shards {
            let handle = h.clone();
            let (policy, options) = (policy.clone(), options.clone());
            set.spawn(async move { handle.start(policy, options).await });
        }
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    first_err.get_or_insert(e);
                }
                Err(je) => {
                    first_err.get_or_insert(CorelamoError::Internal(format!(
                        "shard task panicked: {je}"
                    )));
                }
            }
        }
        first_err.map_or(Ok(()), Err)
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
            input.parsed.collect_leaf_values(&mut all_fields_map);
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
            input.parsed.collect_leaf_values(&mut all_fields_map);
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

    /// The configured embedding model, loaded on first use and shared by all databases.
    pub fn embedder(&self) -> Result<Arc<Embedder>, CorelamoError> {
        self.models
            .default_embedder()
            .map_err(|e| CorelamoError::Internal(format!("embedding model: {e}")))
    }
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    #[timed(database_lifecycle)]
    //karoc shard_manager vajadzeja ownership lai visi shutdowojas, tagad ir smuki ar extra
    //shutdown.done
    pub fn shutdown(&self) -> Result<(), CorelamoError> {
        if self
            .shutdown_done
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Ok(());
        }

        for h in &self.shards {
            h.progress().cancel();
        }

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

        let joins = std::mem::take(&mut *self.joins.lock());
        for j in joins {
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

    /// This database's dictionary.toml as it is in use, comments included.
    pub fn dictionary_text(&self) -> String {
        self.synonyms.config_text()
    }

    /// Replaces the dictionary with an already parsed document. Compiling and saving take a few
    /// hundred ms, so they run off the async threads; on any error the dictionary in use stays.
    pub async fn set_dictionary(&self, document: DictionaryDocument) -> Result<(), CorelamoError> {
        let synonyms = Arc::clone(&self.synonyms);
        tokio::task::spawn_blocking(move || synonyms.replace_document(document))
            .await
            .map_err(|e| CorelamoError::Internal(format!("dictionary update failed: {e}")))?
            .map_err(CorelamoError::from)
    }

    /// Re-reads dictionary.toml after it was edited by hand.
    pub async fn reload_dictionary(&self) -> Result<(), CorelamoError> {
        let synonyms = Arc::clone(&self.synonyms);
        tokio::task::spawn_blocking(move || synonyms.reload())
            .await
            .map_err(|e| CorelamoError::Internal(format!("dictionary reload failed: {e}")))?
            .map_err(CorelamoError::from)
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
        options.save_to_file(&self.root)?;
        *self.options.write() = options;

        let mut set = JoinSet::new();
        for h in &self.shards {
            let handle = h.clone();
            let opt = options.clone();
            let user = user.clone();
            set.spawn(async move { handle.set_config(opt, user).await });
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

        Ok(())
    }

    #[timed(reindex)]
    pub fn reindex(&self) -> Result<(), CorelamoError> {
        if !self.db_stats.begin_reindex(self.shards.len()) {
            return Err(CorelamoError::Busy("reindex already in progress".into()));
        }

        let mut submitted = 0usize;
        let mut first_err = None;

        for h in &self.shards {
            //nodod reindex  pool
            let (rtx, rrx) = bounded(1);
            if let Err((e, _)) = h.send_raw(ShardCmd::PrepareReindex { resp: rtx }) {
                first_err = Some(e);
                break;
            }

            let job = PendingReindexJob {
                rx: rrx,
                shard_tx: h.command_sender(),
                progress: Arc::clone(h.progress()),
                stats: Arc::clone(&self.db_stats),
            };
            match self.reindex_pool.submit(job) {
                Ok(()) => {
                    submitted += 1;
                }
                Err(e) => {
                    first_err = Some(e);
                    break;
                }
            }
        }

        if let Some(e) = first_err {
            // the shards without a job never reach finish_shard_reindex; settle them as
            // failed so the counter reaches zero and the phase ends at Failed, which lets
            // the next reindex start
            for _ in submitted..self.shards.len() {
                self.db_stats.finish_shard_reindex(false, Duration::ZERO);
            }
            return Err(e);
        }
        Ok(())
    }

    #[timed(reindex)]
    pub fn abort_reindex(&self) {
        for h in &self.shards {
            h.progress().cancel();
        }
        // staging dirs live under each shard root, not the db root
        for h in &self.shards {
            let _ = fs::remove_dir_all(h.shard_root().join("index.new"));
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
                if *kind != field.kind {
                    *kind = field.kind.clone();
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
        let total_bytes: u64 = inputs.iter().map(|d| d.source.len() as u64).sum();

        let mut all_fields_map = BTreeMap::new();
        for input in &inputs {
            input.parsed.collect_leaf_values(&mut all_fields_map);
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

        core_timing::add_bytes("inserting", "insert", file!(), total_bytes);
        self.db_stats.record_indexing(false, started.elapsed());
        Ok(report)
    }

    #[timed(retrieve_opps)]
    pub async fn lookup(&self, command: &LookupCommand) -> Result<LookupResponse, CorelamoError> {
        let by_shard = self.group_by_shard(&command.ids);
        let policy = self.policy.read().clone();
        let strip_id = id_path_to_strip(
            policy.id_field().map(|f| f.name.as_str()),
            command.return_fields.as_ref(),
        )
        .map(String::from);

        let mut position: HashMap<String, usize> = HashMap::new();
        for (i, id) in command.ids.iter().enumerate() {
            position.entry(id.clone()).or_insert(i);
        }

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

        //for users own happieness we return the found/not_found in the same oder as
        //he/she/they/them asked
        all_docs.sort_by_key(|(external_id, _)| {
            position.get(external_id).copied().unwrap_or(usize::MAX)
        });
        all_not_found.sort_by_key(|id| position.get(id).copied().unwrap_or(usize::MAX));

        Ok(LookupResponse::new(all_docs, all_not_found, strip_id))
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

    #[timed(search)]
    pub fn hits_cmp(a: &SearchHit, b: &SearchHit) -> Ordering {
        a.tier
            .cmp(&b.tier)
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    }

    #[timed(search)]
    pub async fn search(
        &self,
        command: &SearchCommand,
    ) -> Result<(Vec<SearchDocumentHit>, TotalHits), CorelamoError> {
        let limit = command.docs.unwrap_or(10);
        let offset = command.offset.unwrap_or(0);
        let fetch = offset.saturating_add(limit);
        if fetch == 0 {
            return Ok((Vec::new(), TotalHits::default()));
        }
        if let Some(request) = vector_request(&command.query)? {
            return self
                .search_vectors_only(command, request, fetch, offset, limit)
                .await;
        }
        if is_blank_query(&command.query) && command.filters.as_ref().is_none_or(|f| f.is_empty()) {
            return Err(CorelamoError::InvalidData(
                "Search for something not nothing....".into(),
            ));
        }

        let policy = self.policy.read().clone();
        //if no search_fields we should search in the arrays
        let matchmode = command.match_mode;
        let dictionary = self.synonyms.snapshot();

        let (query, ctxs) = compile_query(
            &command.query,
            command.search_fields.as_deref(),
            &self.analyzer,
            &policy,
            Some(&dictionary),
        )?;
        let filters = compile_filters(&self.analyzer, command, &policy)?;
        let sorts = resolve_sorts(command, &policy)?;
        //vector search
        let strict_sort = sorts
            .as_ref()
            .is_some_and(|specs| matches!(specs.mode, SortMode::Strict));
        let semantic = self.semantic_fields(command, &policy, strict_sort);
        // fusion ranks the top FUSION_DEPTH of each list, so the word side must fetch that deep
        let word_fetch = if semantic.is_some() {
            fetch.max(FUSION_DEPTH)
        } else {
            fetch
        };

        //INFO:                page * multiplier    min    max
        //WARN: results may change based on the "docs": x lol
        let window = fetch.saturating_mul(50).clamp(1000, 5_000);

        let full_plan = match (&command.query, query.as_ref()) {
            (OwnedValue::String(text), Some(resolved)) => {
                let has_exact_field = ctxs.iter().any(|ctx| ctx.exact_xpath.is_some());
                RankPlan::cascade(text, resolved.clone(), &self.analyzer, has_exact_field)
                    .stopping_after(if sorts.is_some() { window } else { word_fetch })
            }
            _ => RankPlan::default(),
        };
        // Fuzzy expansions are the costly part of preparing (every close spelling of every word,
        // in every field, on every shard). Prepare without them first; only when a word turns out
        // to be in no document of the whole database is it worth preparing them.
        let mut plan = Arc::new(full_plan.without_fuzzy());
        let typed_words: Vec<String> = match &command.query {
            OwnedValue::String(text) if plan.tiers.len() != full_plan.tiers.len() => {
                core_query::fuzzable_words(&self.analyzer, text)
            }
            _ => Vec::new(),
        };
        let query = Arc::new(query);
        let vector_task = semantic.map(|(text, xpaths)| {
            self.spawn_vector_search(text, xpaths, FUSION_DEPTH, filters.clone())
        });
        // resolve highlight fields once + collect query matchers once
        let highlight: Option<(Vec<(String, HighlightFieldOptions)>, Vec<HighlightMatcher>)> =
            match &command.highlight {
                Some(cmd) => {
                    let mut fields = Vec::with_capacity(cmd.fields.len());
                    for (name, opts) in &cmd.fields {
                        let field = policy
                            .field_by_path(name)
                            .ok_or_else(|| CorelamoError::PathNotIndexed(name.clone()))?;
                        fields.push((field.full_path.clone(), opts.clone()));
                    }
                    let matchers = (*query)
                        .as_ref()
                        .map(|q| collect_highlight_matchers(q, &self.analyzer))
                        .unwrap_or_default();
                    Some((fields, matchers))
                }
                None => None,
            };

        let sort_xpaths: Option<Arc<Vec<XPathId>>> = sorts
            .as_ref()
            .map(|s| Arc::new(s.fields.iter().map(|f| f.xpath).collect()));

        //Scores must not depend on which shard a document is in, so every shard scores with the
        //statistics of all shards together: first every shard reads the query and reports its
        //own statistics, then all of them rank
        let mut passes = 0;
        let (prepared, corpus) = loop {
            passes += 1;
            let mut prepared: Vec<Option<Arc<PreparedShardSearch>>> = vec![None; self.shards.len()];
            let mut set = JoinSet::new();
            for (index, handle) in self.shards.iter().enumerate() {
                let handle = handle.clone();
                let query = Arc::clone(&query);
                let plan = Arc::clone(&plan);
                let ctxs = Arc::clone(&ctxs);
                set.spawn_blocking(move || {
                    let shard = handle.prepare_search((*query).as_ref(), &plan, &ctxs);
                    (index, shard)
                });
            }
            let mut corpus = CorpusStats::new();
            while let Some(res) = set.join_next().await {
                match res {
                    Ok((index, shard)) => {
                        corpus.merge(shard.stats());
                        prepared[index] = Some(Arc::new(shard));
                    }
                    Err(je) => {
                        return Err(CorelamoError::Internal(format!(
                            "shard prepare panicked: {je}"
                        )));
                    }
                }
            }
            // a word no document has: the typo tier is needed after all, prepare again with it
            let plain_xpaths: Vec<XPathId> = ctxs
                .iter()
                .filter(|ctx| !ctx.row_keyed)
                .map(|ctx| ctx.xpath)
                .collect();
            let unknown_word = typed_words.iter().any(|word| {
                plain_xpaths
                    .iter()
                    .all(|&xpath| corpus.term_doc_freq(xpath, word) == 0)
            });
            if passes == 1 && unknown_word {
                plan = Arc::new(full_plan.clone());
                continue;
            }
            break (prepared, corpus);
        };
        let corpus = Arc::new(corpus);

        let mut set = JoinSet::new();
        for (handle, prepared) in self.shards.iter().zip(prepared) {
            let handle = handle.clone();
            let prepared = prepared.expect("every shard was prepared");
            let corpus = corpus.clone();
            let query = Arc::clone(&query);
            let plan = Arc::clone(&plan);
            let filters = filters.clone();
            let ctxs = Arc::clone(&ctxs);
            let sort_xpaths = sort_xpaths.clone();

            set.spawn_blocking(move || {
                //any sort mentioned? - we do smart sort, else just relevance
                if let Some(sort_xpaths) = sort_xpaths.as_ref() {
                    handle.rank_sorted(
                        &prepared,
                        &corpus,
                        (*query).as_ref(),
                        &plan,
                        filters.as_deref(),
                        &ctxs,
                        sort_xpaths,
                        window,
                        matchmode,
                    )
                } else {
                    handle.rank_top_k(
                        &prepared,
                        &corpus,
                        (*query).as_ref(),
                        &plan,
                        filters.as_deref(),
                        &ctxs,
                        // hits merge by (tier, score): each shard's top `fetch` covers the merged top `fetch`
                        word_fetch,
                        matchmode,
                    )
                }
            });
        }

        //join results
        let mut items: Vec<(SearchHit, Vec<Option<f64>>)> = Vec::new();
        let mut per_shard_bounds: Vec<Vec<(Option<f64>, Option<f64>)>> = Vec::new();
        let mut total_hits: u64 = 0;
        let mut first_err = None;

        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(r)) => {
                    total_hits += r.total as u64;
                    items.extend(r.items);
                    if !r.bounds.is_empty() {
                        per_shard_bounds.push(r.bounds);
                    }
                }
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

        // Any failure on the vector side leaves the word ranking as it is.
        let vector_hits = match vector_task {
            Some(task) => task.await.ok().flatten(),
            None => None,
        };
        if let Some(specs) = sorts.as_ref() {
            if let Some(vectors) = vector_hits.as_deref() {
                // blend: vector ranks feed the relevance part; only candidates with sort keys
                items.sort_unstable_by(|(a, _), (b, _)| Self::hits_cmp(a, b));
                items = fuse_rrf(items, vectors, false);
            }
            let bounds = fold_sort_bounds(&specs.fields, per_shard_bounds);
            match specs.mode {
                SortMode::Blend => order_blended(&mut items, &specs.fields, &bounds),
                SortMode::Strict => order_strict(&mut items, &specs.fields),
            }
        } else {
            items.sort_unstable_by(|(a, _), (b, _)| Self::hits_cmp(a, b));
            if let Some(vectors) = vector_hits.as_deref() {
                items.truncate(word_fetch);
                items = fuse_rrf(items, vectors, true);
            }
        }
        if items.len() > fetch {
            items.truncate(fetch);
        }
        let just_hits: Vec<SearchHit> = items.into_iter().map(|(hit, _)| hit).collect();
        self.resolve_page(
            just_hits, offset, limit, total_hits, command, &policy, highlight,
        )
        .await
    }

    pub fn record_search(&self, failed: bool, elapsed: std::time::Duration) {
        self.db_stats.record_search(failed, elapsed);
    }

    //viss ar backups
    #[timed(backup)]
    pub fn try_start_backup(&self) -> Result<(), CorelamoError> {
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
            (DATABASE_FILE_NAME, "dictionary.toml.gz"),
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

        // Nothing to back up (no shard produced a manifest): remove the empty directory.
        if manifests.iter().all(|m| m.is_none()) {
            let _ = fs::remove_dir_all(&backup_root);
        }
        Ok(manifests)
    }

    #[timed(retrieve_opps)]
    pub async fn info_words(&self, words: Vec<String>) -> Result<Vec<WordStats>, CorelamoError> {
        if words.is_empty() {
            return Ok(Vec::new());
        }

        let xpaths = Arc::new(self.policy.read().searchable_xpaths());

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
                Ok(Err(e)) if first_err.is_none() => {
                    first_err = Some(e);
                }
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

    //INFO: bisk porno funkcija, no analyzer parvers kkadu running par run un piedaavaa fun/gun...
    //tapec mums te ir max changes 1 prieks taa vai parbaudit analyzed/ne analyzed vardu
    #[timed(search)]
    pub async fn did_you_mean(
        &self,
        command: &DidYouMeanRequest,
    ) -> Result<DidYouMeanReport, CorelamoError> {
        let size = command.did_you_mean_count.unwrap_or(3);

        if command.value.trim().is_empty() || size == 0 {
            return Ok(DidYouMeanReport::empty(&command.value));
        }

        let policy = self.policy.read().clone();

        //in which xpaths to search in
        //WARN: manskiet te bij jauna logika bet man bail no merge conflikta
        let xpaths: Arc<Vec<XPathId>> = match &command.search_fields {
            Some(names) => Arc::new(resolve_suggest_xpaths(names, &policy)?),
            None => Arc::new(policy.searchable_xpaths()),
        };

        let fuzziness = parse_fuzziness(command.fuzziness.as_deref())?;

        //in this did_you_mean command we allow the prefix to be 0 since no seach ranking happens
        //its just the best fuzzed words
        let spec = FuzzySpec {
            prefix_length: 0,
            max_expansions: 0,
        };

        //convert words to be fuzzable spider-man -> spiderman
        let words = core_query::fuzzable_words(&self.analyzer, &command.value);

        let mut set = JoinSet::new();
        for handle in &self.shards {
            let handle = handle.clone();
            let value = command.value.clone();
            let xpaths = Arc::clone(&xpaths);

            set.spawn_blocking(move || handle.suggest_direct(&value, &xpaths, fuzziness, spec));
        }

        // One merged map per word, so a term's document frequency is the sum
        // over shards before anything gets ranked.
        let mut merged: Vec<BTreeMap<String, FuzzyExpansion>> =
            (0..words.len()).map(|_| BTreeMap::new()).collect();

        let mut first_err = None;

        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(per_word)) => {
                    for (i, expansions) in per_word.into_iter().enumerate() {
                        let Some(acc) = merged.get_mut(i) else {
                            continue;
                        };

                        for expansion in expansions {
                            match acc.get_mut(&expansion.term) {
                                Some(existing) => {
                                    existing.edits = existing.edits.min(expansion.edits);
                                    existing.doc_freq =
                                        existing.doc_freq.saturating_add(expansion.doc_freq);
                                }
                                None => {
                                    acc.insert(expansion.term.clone(), expansion);
                                }
                            }
                        }
                    }
                }
                Ok(Err(e)) if first_err.is_none() => {
                    first_err = Some(e);
                }
                Err(je) if first_err.is_none() => {
                    first_err = Some(CorelamoError::Internal(format!(
                        "did-you-mean shard task panicked: {je}"
                    )));
                }
                _ => {}
            }
        }

        if let Some(e) = first_err {
            return Err(e);
        }

        //WARN: bisk porniks saakas
        const MAX_ANALYSIS_LOSS: usize = 1;

        //suggestions per analysed word, before we decide which ones to trust.
        let mut by_word: HashMap<String, Vec<String>> = words
            .into_iter()
            .zip(merged)
            .map(|(word, by_term)| {
                let mut expansions: Vec<FuzzyExpansion> = by_term.into_values().collect();

                core_query::rank_and_cap(&mut expansions, size);

                (word, expansions.into_iter().map(|e| e.term).collect())
            })
            .collect();

        //we need to construct the original sentence (keeping stopwords/no running -> run)
        let mut pieces: Vec<String> = Vec::new();
        let mut reported: Vec<WordSuggestions> = Vec::new();
        let mut changed = false;

        for piece in command.value.split_whitespace() {
            let Some(token) = self.analyzer.analyze_query(piece).into_iter().next() else {
                //stopword: not searchable, keep it as typed.
                pieces.push(piece.to_string());
                continue;
            };

            //if a word from suggestions was changed too much we just keep the original
            let lost = piece
                .chars()
                .count()
                .saturating_sub(token.text.chars().count());

            if lost > MAX_ANALYSIS_LOSS {
                pieces.push(piece.to_string());
                continue;
            }

            let Some(found) = by_word.remove(&token.text) else {
                pieces.push(piece.to_string());
                continue;
            };

            match found.first() {
                Some(top) => {
                    changed = true;
                    pieces.push(top.clone());
                }
                None => pieces.push(piece.to_string()),
            }

            reported.push(WordSuggestions {
                word: token.text,
                suggestions: found,
            });
        }

        let corrected = if changed {
            pieces.join(" ")
        } else {
            command.value.clone()
        };

        Ok(DidYouMeanReport {
            original: command.value.clone(),
            corrected,
            words: reported,
        })
    }

    #[timed(restore)]
    pub async fn restore_backup(&self, backup_id: &str, user: String) -> Result<(), CorelamoError> {
        let mut failures = Vec::new();

        for shard in &self.shards {
            if let Err(e) = shard.restore_backup(backup_id, user.clone()).await {
                failures.push(format!("shard {}: {}", shard.id(), e));
            }
        }
        //not needed cuz the handler calls this too
        //self.db_stats.finish_restore(failures.is_empty());

        if failures.is_empty() {
            self.start().await?;
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

    /// `{"vector": ...}`: nearest documents by meaning only, no word matching.
    async fn search_vectors_only(
        &self,
        command: &SearchCommand,
        request: VectorRequest,
        fetch: usize,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<SearchDocumentHit>, TotalHits), CorelamoError> {
        if command.sort.is_some() {
            return Err(CorelamoError::InvalidData(
                "vector queries do not support sort yet".into(),
            ));
        }
        let policy = self.policy.read().clone();
        let filters = compile_filters(&self.analyzer, command, &policy)?;
        let names: Option<Vec<String>> = match request.field {
            Some(field) => Some(vec![field]),
            None => command.search_fields.clone(),
        };
        let xpaths = vector_xpaths(&policy, names.as_deref());
        if xpaths.is_empty() {
            return Err(CorelamoError::InvalidData(
                "no vector field to search: mark a text field with \"$vector\": true".into(),
            ));
        }

        let k = fetch.min(MAX_VECTOR_RESULTS);
        let hits = self
            .spawn_vector_search(request.text, xpaths, k, filters)
            .await
            .map_err(|e| CorelamoError::Internal(format!("vector search task panicked: {e}")))?
            .ok_or_else(|| {
                CorelamoError::Internal(
                    "vector search unavailable: the embedding model could not be loaded".into(),
                )
            })?;

        let total = hits.len() as u64;
        let just_hits: Vec<SearchHit> = hits
            .into_iter()
            .map(|hit| SearchHit {
                doc_id: hit.doc_id,
                matched_terms: 0,
                weight_sum: 0,
                distance_factor: 1.0,
                score: hit.score,
                tier: 0,
            })
            .collect();
        self.resolve_page(just_hits, offset, limit, total, command, &policy, None)
            .await
    }
    /// Loads the requested page of ranked hits as documents, with highlights when asked.
    /// Loads the requested page of ranked hits as documents, with highlights when asked.
    /// Shared by word, fused and vector-only searches.
    async fn resolve_page(
        &self,
        just_hits: Vec<SearchHit>,
        offset: usize,
        limit: usize,
        total_hits: u64,
        command: &SearchCommand,
        policy: &IndexPolicy,
        highlight: Option<(Vec<(String, HighlightFieldOptions)>, Vec<HighlightMatcher>)>,
    ) -> Result<(Vec<SearchDocumentHit>, TotalHits), CorelamoError> {
        if offset >= just_hits.len() {
            return Ok((Vec::new(), TotalHits::default()));
        }

        //for the resolve hit to save position
        let page: Vec<(usize, SearchHit)> = just_hits
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

        //sorted hits -> full documents for pretty results :)
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

        //join results
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

        //populate highlights
        if let Some((fields, matchers)) = &highlight {
            let fields = fields.clone();
            let matchers = matchers.clone();
            let analyzer = self.analyzer.clone();
            resolved = tokio::task::spawn_blocking(move || {
                for slot in &mut resolved {
                    let Some(hit) = slot else {
                        continue;
                    };
                    let mut highlights = Vec::new();
                    if !matchers.is_empty() {
                        if let Ok(value) = hit.doc.to_value(None) {
                            for (full_path, opts) in &fields {
                                for text in field_texts(&value, full_path) {
                                    highlights
                                        .extend(highlight_text(&text, &analyzer, &matchers, opts));
                                }
                            }
                        }
                    }
                    hit.highlights = highlights;
                }
                resolved
            })
            .await
            .map_err(|e| CorelamoError::Internal(format!("highlight task panicked: {e}")))?;
        }

        let total_hits = TotalHits {
            value: total_hits.min(1000),
            more: total_hits >= 1000,
        };

        Ok((resolved.into_iter().flatten().collect(), total_hits))
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
    /// The vector fields to search for this request, or None when fusion does not apply.
    ///
    ///
    /// VECTOR PRIKOLI
    fn semantic_fields(
        &self,
        command: &SearchCommand,
        policy: &IndexPolicy,
        strict_sort: bool,
    ) -> Option<(String, Vec<XPathId>)> {
        let OwnedValue::String(text) = &command.query else {
            return None;
        };
        if text.trim().is_empty() {
            return None;
        }
        // a strict sort ignores relevance, so vector ranks could not change the order
        if command.semantic == Some(false) || strict_sort {
            return None;
        }
        if command.semantic.is_none() {
            if text.split_whitespace().count() < SEMANTIC_MIN_WORDS {
                return None;
            }
            // partly embedded databases would favour documents that happen to have vectors
            let embedding = self.db_stats.snapshot().background.embedding;
            let ready = match embedding.status {
                EmbeddingStatus::Complete => true,
                EmbeddingStatus::Embedding => embedding.progress >= SEMANTIC_MIN_PROGRESS,
                EmbeddingStatus::Disabled => false,
            };
            if !ready {
                return None;
            }
        }
        let xpaths = vector_xpaths(policy, command.search_fields.as_deref());
        (!xpaths.is_empty()).then(|| (text.clone(), xpaths))
    }

    /// Embeds the query once and searches every shard and field in parallel.
    /// Resolves to None when the model or any step is unavailable.
    fn spawn_vector_search(
        &self,
        text: String,
        xpaths: Vec<XPathId>,
        k: usize,
        filters: Option<Arc<HashMap<String, FieldQuery>>>,
    ) -> tokio::task::JoinHandle<Option<Vec<VectorHit>>> {
        let models = Arc::clone(&self.models);
        let shards = self.shards.clone();
        tokio::task::spawn_blocking(move || {
            let embedder = models.default_embedder().ok()?;
            let query = embedder.embed_query(&text).ok()?;
            let filters = filters.as_deref();
            let lists: Vec<Vec<VectorHit>> = std::thread::scope(|scope| {
                let jobs: Vec<_> = shards
                    .iter()
                    .map(|shard| {
                        let (query, xpaths) = (&query, &xpaths);
                        scope.spawn(move || shard.vector_search(xpaths, query, k, filters))
                    })
                    .collect();
                jobs.into_iter()
                    .filter_map(|job| job.join().ok()?.ok())
                    .collect()
            });
            Some(merge_hits(lists, k))
        })
    }
}
//VECTOR
/// Reciprocal Rank Fusion of the word ranking and the vector ranking: every document scores
/// Σ 1 / (RRF_K + rank) over the lists it appears in. It only uses ranks, so BM25 and cosine
/// scores never need to be on the same scale. `add_vector_only` adds documents only the vector
/// side found (they carry no sort keys, so sorted searches pass false). Fused scores are
/// scaled so the best is 1.0, the scale blended sorting expects; all hits share tier 0.
fn fuse_rrf(
    words: Vec<(SearchHit, Vec<Option<f64>>)>,
    vectors: &[VectorHit],
    add_vector_only: bool,
) -> Vec<(SearchHit, Vec<Option<f64>>)> {
    let contribution = |rank: usize| 1.0 / (RRF_K + rank as f32 + 1.0);
    let mut fused: HashMap<DocId, (SearchHit, Vec<Option<f64>>, f32)> =
        HashMap::with_capacity(words.len() + vectors.len());

    for (rank, (hit, keys)) in words.into_iter().enumerate() {
        fused.insert(hit.doc_id, (hit, keys, contribution(rank)));
    }
    for (rank, hit) in vectors.iter().enumerate() {
        if let Some(entry) = fused.get_mut(&hit.doc_id) {
            entry.2 += contribution(rank);
        } else if add_vector_only {
            let vector_hit = SearchHit {
                doc_id: hit.doc_id,
                matched_terms: 0,
                weight_sum: 0,
                distance_factor: 1.0,
                score: 0.0,
                tier: 0,
            };
            fused.insert(hit.doc_id, (vector_hit, Vec::new(), contribution(rank)));
        }
    }

    let best = fused
        .values()
        .map(|(_, _, score)| *score)
        .fold(0.0f32, f32::max);
    let mut out: Vec<(SearchHit, Vec<Option<f64>>)> = fused
        .into_values()
        .map(|(mut hit, keys, score)| {
            hit.score = if best > 0.0 { score / best } else { 0.0 };
            hit.tier = 0;
            (hit, keys)
        })
        .collect();
    out.sort_unstable_by(|(a, _), (b, _)| {
        b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id))
    });
    out
}