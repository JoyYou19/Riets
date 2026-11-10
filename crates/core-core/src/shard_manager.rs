//include ALL
use crate::ShardDb;
use crate::metrics::{DbStats, EmbeddingStatus};

use crate::shard_db::DatabaseStats;
use crate::shard_worker::{self, PreparedShardSearch, ShardCmd, ShardHandle};
use crate::{DatabaseOptions, shard_for};
use ahash::{HashMap, HashMapExt};
use core_backup::backup::{BackupManifest, BackupType};
use core_backup::backup::{compress_file, is_valid_backup_id};
use core_backup::progress::BackupProgress;
use core_index::analyzer::Analyzer;
use core_index::analyzer::analyzer::AnalyzerSet;
use core_index::document::IndexPolicy;
use core_index::document::all_fields::AllFields;
use core_index::document::policy::FieldKind;
use core_index::fuzzy::{FuzzyExpansion, FuzzySpec, select_expansions};
use core_index::lsm::index_worker::Phase;
use core_index::types::{DocId, ShardId, XPathId, shard_of};
use core_index::vector::{VectorHit, merge_hits};
use core_protocol::command_reponse_definitions::{
    DidYouMeanRequest, Fuzziness, HighlightFieldOptions, LookupCommand, LookupResponse, MatchMode,
    QueryCorrection, SearchCommand, SortMode, TotalHits,
};
use core_protocol::errors::CorelamoError;
use core_query::dictionary::{
    DATABASE_FILE_NAME, DictionaryDocument, SynonymManager, SynonymRegistry,
};
use core_query::executor::{DidYouMeanReport, WordSuggestions};
use core_query::highlight::{
    field_texts, highlight_field, highlight_field_array, highlight_field_row_keyed,
    highlight_field_row_keyed_flattened,
};
use core_query::planner::{RankPlan, TierKind};
use core_query::resolver::{
    FieldQuery, QueryGroup, compile_query, field_analyzer, parse_fuzziness, resolve_suggest_xpaths,
};
use core_query::{CorpusStats, MatchedTerm, SearchHit, rank_order};
use core_storage::document_projections::id_path_to_strip;
use core_storage::document_store::StoredDocument;
use core_storage::search_database::{DeleteReport, InsertReport, ReplaceReport, WordStats};
use core_storage::search_database::{DocumentInput, SearchDocumentHit};
use core_timing::timed;
use crossbeam_channel::bounded;
use parking_lot::{Mutex, RwLock};
use simd_json::OwnedValue;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};
use tokio::task::JoinSet;

use crate::shard_manager_helpers::{
    ResolvedSort, compile_filters, fold_sort_bounds, order_blended, order_strict, resolve_sorts,
};
use core_embed::{Embedder, ModelRegistry};

//mististiskas konstantes
/// How many hits each list contributes to fusion. Deeper than a page, so a document ranked
/// moderately in both lists can still reach the top.
const FUSION_DEPTH: usize = 100;
/// The usual RRF constant: damps the difference between the very top ranks.
const RRF_K: f32 = 10.0;
/// Fewer words than this are usually names or codes, which word search already handles best.
const SEMANTIC_MIN_WORDS: usize = 3;
/// Automatic fusion needs at least this share of documents embedded (percent).
const SEMANTIC_MIN_PROGRESS: u8 = 95;
/// Largest result count a vector-only query returns (matches the total-hits cap).
const MAX_VECTOR_RESULTS: usize = 1000;
/// Most documents a search can page through (offset + docs). The total hit count is not
/// limited by it.
const MAX_RESULTS_SIZE: usize = 1000;
/// A query that is only `{"vector": "text"}` or `{"vector": {"text": "...", "field": "..."}}`.
/// How many times as common a correction must be than every equally close or closer word to
/// replace the query on its own ("batmna" -> "batman"); below that it is only suggested.
const CORRECTION_DOMINANCE: u32 = 20;
/// True when `best` is clearly what was meant: every other candidate at the same or a smaller
/// edit distance is in at most `1/CORRECTION_DOMINANCE` as many documents.
const CONTEXT_CANDIDATES: usize = 8;
/// A correction chosen by context is trusted when its documents with the other words
/// outnumber the next candidate's this many times.
const CONTEXT_DOMINANCE: u64 = 3;
/// A word at least this many documents contain is spelled right: it is never replaced.
const TRUSTED_DOC_FREQ: u32 = 20;
fn is_confident_correction(best: &FuzzyExpansion, candidates: &[FuzzyExpansion]) -> bool {
    candidates
        .iter()
        .filter(|other| other.term != best.term && other.edits <= best.edits)
        .all(|other| best.doc_freq >= other.doc_freq.saturating_mul(CORRECTION_DOMINANCE))
}
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
                    .any(|name| *name == field.full_path || *name == field.name)
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
    analyzer_set: RwLock<AnalyzerSet>,
    db_stats: Arc<DbStats>,
    backup_dir: PathBuf,
    all_fields: RwLock<AllFields>,
    // the database's synonym dictionary, dictionary.toml in its directory
    synonyms: Arc<SynonymManager>,
    models: Arc<ModelRegistry>,
    // taken and joined in shutdown(); a Mutex because shutdown() only gets &self
    joins: Mutex<Vec<JoinHandle<()>>>, //JoinHandle atgriez kad thread uztaisits
    // main, delete and rename all reach shutdown(), so it has to be safe twice
    shutdown_done: AtomicBool,
}
struct WordCorrection {
    suggestions: Vec<String>,
    confident: bool,
}
impl ShardManager {
    const DEFAULT_QUEUE_DEPTH: usize = 256;
    /// Candidates of a misspelled word that are checked against the rest of the query.

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
        fs::create_dir_all(&shards_dir)?;

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
            let shard_root = shards_dir.join(format!("shard-{shard_id}"));
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

        let analyzer_set = AnalyzerSet::from_policy(&policy.analyzers);
        Ok(Self {
            shards,
            root,
            policy: RwLock::new(policy),
            options: RwLock::new(options),
            joins: Mutex::new(joins),
            shutdown_done: AtomicBool::new(false),
            analyzer_set: RwLock::new(analyzer_set),
            all_fields: RwLock::new(all_fields),
            db_stats,
            backup_dir,
            synonyms,
            models,
        })
    }

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
        for entry in fs::read_dir(&shards_dir)? {
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
        let analyzer_set = AnalyzerSet::from_policy(&policy.analyzers);
        Ok(Self {
            shards,
            joins: Mutex::new(joins),
            shutdown_done: AtomicBool::new(false),
            root,
            policy: RwLock::new(policy),
            options: RwLock::new(options),
            analyzer_set: RwLock::new(analyzer_set),
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
        let wanted: Vec<(&String, FieldKind)> = fields
            .keys()
            .map(|xpath| {
                let kind = policy
                    .fields
                    .iter()
                    .find(|f| f.name == *xpath)
                    .map_or(FieldKind::None, |f| f.kind);
                (xpath, kind)
            })
            .collect();

        {
            let current = self.all_fields.read();
            if wanted
                .iter()
                .all(|(xpath, kind)| current.get_fields().get(*xpath) == Some(kind))
            {
                return Ok(());
            }
        }

        // one writer at a time: the check, the save and the swap see the same map
        let mut guard = self.all_fields.write();
        let mut updated = guard.clone();
        let mut changed = false;
        for (xpath, kind) in wanted {
            if updated.get_fields().get(xpath) != Some(&kind) {
                updated.get_fields_mut().insert(xpath.clone(), kind);
                changed = true;
            }
        }
        if changed {
            updated.save(&self.root)?;
            *guard = updated;
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
            for entry in fs::read_dir(&logs_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|e| e == "log") {
                    fs::remove_file(&path)?;
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
        self.shards.iter().all(ShardHandle::is_running)
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
    ) -> Result<Option<String>, CorelamoError> {
        let previous = self.policy.read().clone();
        policy.validate()?;
        if self.db_stats.reindex_running() {
            return Err(CorelamoError::Busy(
                "a reindex is running; change the policy when it has finished".into(),
            ));
        }
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
        let failure = match first_err {
            Some(e) => Some(e),
            None => policy.save(&self.root).err().map(CorelamoError::from),
        };
        if let Some(e) = failure {
            for handle in &self.shards {
                let _ = handle.set_policy(previous.clone(), user.clone()).await;
            }
            return Err(e);
        }

        let changed = previous.analyzers != policy.analyzers || previous.fields != policy.fields;

        *self.analyzer_set.write() = AnalyzerSet::from_policy(&policy.analyzers);
        *self.policy.write() = policy;
        self.update_all_fields_from_policy()?;

        Ok(changed.then(|| "policy changed; reindex required to apply to existing documents".to_string()))
    }

    #[timed(database_lifecycle)]
    pub async fn set_options_all(
        &self,
        options: DatabaseOptions,
        user: String,
    ) -> Result<(), CorelamoError> {
        let previous = self.options.read().clone();
        if self.db_stats.reindex_running() {
            return Err(CorelamoError::Busy(
                "a reindex is running; change the config when it has finished".into(),
            ));
        }
        options
            .search
            .validate()
            .map_err(CorelamoError::InvalidData)?;
        
        

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
        let failure = match first_err {
            Some(e) => Some(e),
            None => options
                .save_to_file(&self.root)
                .err()
                .map(CorelamoError::from),
        };
        
        if let Some(e) = failure {
            for handle in &self.shards {
                let _ = handle.set_config(previous.clone(), user.clone()).await;
            }
            return Err(e);
        }

        *self.options.write() = options;
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
            let sent = h.send_raw(ShardCmd::PrepareReindex {
                progress: Arc::clone(h.progress()),
                stats: Arc::clone(&self.db_stats),
                resp: rtx,
            });
            let result = match sent {
                Err((e, _)) => Err(e),
                Ok(()) => rrx
                    .recv()
                    .unwrap_or_else(|_| Err(CorelamoError::Internal("shard stopped".into()))),
            };
            match result {
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
        stats.restoring = self.shards.iter().any(ShardHandle::is_restoring);
        stats
    }

    fn update_all_fields_from_policy(&self) -> Result<(), CorelamoError> {
        let policy = self.policy.read().clone();
        let mut all_fields = self.all_fields.read().clone();
        let mut changed = false;

        for (xpath, kind) in all_fields.get_fields_mut().iter_mut() {
            if let Some(field) = policy.fields.iter().find(|f| f.name == *xpath)
                && *kind != field.kind
            {
                *kind = field.kind;
                changed = true;
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
            let ids: Vec<String> = shard_ids.iter().map(|s| (*s).clone()).collect();
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
            let response = handle
                .await
                .map_err(|e| CorelamoError::Internal(format!("shard lookup task failed: {e}")))??;
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

    fn build_plans(
        query: &OwnedValue,
        query_groups: &[QueryGroup],
        cascade_order: &[TierKind],
        has_sort: bool,
        window: usize,
        word_fetch: usize,
    ) -> (Arc<Vec<RankPlan>>, Arc<Vec<RankPlan>>) {
        let full: Arc<Vec<RankPlan>> = Arc::new(
            query_groups
                .iter()
                .map(|group| match (query, group.query.as_ref()) {
                    (OwnedValue::String(text), Some(resolved)) => {
                        let has_exact = group.ctxs.iter().any(|ctx| ctx.exact_xpath.is_some());
                        RankPlan::cascade(
                            text,
                            resolved.clone(),
                            &group.analyzer,
                            has_exact,
                            cascade_order,
                        )
                        .stopping_after(if has_sort {
                            window
                        } else {
                            word_fetch
                        })
                    }
                    _ => RankPlan::default(),
                })
                .collect(),
        );
        let no_fuzzy = Arc::new(full.iter().map(RankPlan::without_fuzzy).collect());
        (full, no_fuzzy)
    }

    #[timed(search)]
    pub async fn search(
        &self,
        command: &SearchCommand,
    ) -> Result<(Vec<SearchDocumentHit>, TotalHits, Option<QueryCorrection>), CorelamoError> {
        self.run_search(command, true).await
    }

    /// The whole search. `allow_correction`: false for the re-run with a corrected spelling,
    /// so a spelling is fixed at most once.
    async fn run_search(
        &self,
        command: &SearchCommand,
        allow_correction: bool,
    ) -> Result<(Vec<SearchDocumentHit>, TotalHits, Option<QueryCorrection>), CorelamoError> {
        let Some(page) = requested_page(command)? else {
            return Ok((Vec::new(), TotalHits::default(), None));
        };

        if let Some(request) = vector_request(&command.query)? {
            return self
                .search_vectors_only(command, request, page.fetch, page.offset, page.limit)
                .await;
        }

        let search = self.compile_search(command, page)?;
        let stats = self.collect_statistics(&search, &command.query).await?;

        let suggestion = match self.check_spelling(command, &stats, allow_correction).await {
            SpellingResult::SearchInstead(text) => {
                let mut corrected = command.clone();
                corrected.query = OwnedValue::String(text.clone());
                let (docs, total, _) = Box::pin(self.run_search(&corrected, false)).await?;
                return Ok((docs, total, Some(QueryCorrection::Applied(text))));
            }
            SpellingResult::Suggest(suggestion) => Some(suggestion),
            SpellingResult::Fine => None,
        };

        // started only now: a corrected re-run must not leave an embedding search running
        let vector_task = search.semantic.clone().map(|(text, xpaths)| {
            self.spawn_vector_search(text, xpaths, FUSION_DEPTH, search.filters.clone())
        });
        let shard_hits = match self.rank_on_shards(&search, &stats).await {
            Ok(shard_hits) => shard_hits,
            Err(e) => {
                if let Some(task) = vector_task {
                    task.abort();
                }
                return Err(e);
            }
        };
        let vectors = match vector_task {
            Some(task) => task.await.ok().flatten(),
            None => None,
        };

        let more = shard_hits.some_full || shard_hits.items.len() > page.fetch;
        let mut items = merge_and_sort(&search, &stats.plans, shard_hits, vectors.as_deref());
        let groups = Arc::clone(&search.groups);
        items.truncate(page.fetch);
        let hits: Vec<SearchHit> = items.into_iter().map(|(hit, _)| hit).collect();
        let CompiledSearch {
            policy, highlight, ..
        } = search;
        let (docs, total) = self
            .resolve_page(
                hits,
                page.offset,
                page.limit,
                more,
                command,
                &policy,
                highlight,
                groups,
                &stats.shards,
                Some(Arc::clone(&stats.corpus)),
                Arc::clone(&stats.plans),
            )
            .await?;
        Ok((docs, total, suggestion))
    }

    //Turns the request into analyzed queries, filters, sorts, highlighting and rank plans.
    fn compile_search(
        &self,
        command: &SearchCommand,
        page: PageRequest,
    ) -> Result<CompiledSearch, CorelamoError> {
        let policy = self.policy.read().clone();
        let cascade_order = self.options.read().search.cascade_order();
        let dictionary = self.synonyms.snapshot();
        // one read guard: two reads could see two different analyzer sets
        let (groups, filters) = {
            let analyzers = self.analyzer_set.read();
            let groups = compile_query(
                &command.query,
                command.search_fields.as_deref(),
                &analyzers,
                &policy,
                Some(&dictionary),
            )?;
            (groups, compile_filters(&analyzers, command, &policy)?)
        };

        let sorts = resolve_sorts(command, &policy)?;
        let strict_sort = sorts
            .as_ref()
            .is_some_and(|s| matches!(s.mode, SortMode::Strict));

        let semantic = self.semantic_fields(&command.query, command, &policy, strict_sort);

        let word_fetch = if semantic.is_some() {
            page.fetch.max(FUSION_DEPTH)
        } else {
            page.fetch
        };

        let window = page.fetch.saturating_mul(50).clamp(1000, 5_000);
        let (plans_full, plans_no_fuzzy) = Self::build_plans(
            &command.query,
            &groups,
            &cascade_order,
            sorts.is_some(),
            window,
            word_fetch,
        );

        let searched: HashSet<XPathId> = groups
            .iter()
            .flat_map(|g| g.ctxs.iter().map(|c| c.xpath))
            .collect();
        let highlight = {
            let analyzers = self.analyzer_set.read();
            highlight_request(command, &policy, &analyzers, &searched)?
        };
        let sort_xpaths = sorts
            .as_ref()
            .map(|s| Arc::new(s.fields.iter().map(|f| f.xpath).collect()));
        Ok(CompiledSearch {
            policy,
            groups,
            filters,
            sorts,
            sort_xpaths,
            semantic,
            highlight,
            plans_full,
            plans_no_fuzzy,
            match_mode: command.match_mode,
            word_fetch,
            window,
        })
    }

    /// Every shard reads the query and reports its statistics; merged, they make scores the
    /// same whichever shard a document is in. Fuzzy expansions are the costly part, so they are
    /// collected only when a typed word is in no document at all (a second round).
    async fn collect_statistics(
        &self,
        search: &CompiledSearch,
        typed: &OwnedValue,
    ) -> Result<ShardStatistics, CorelamoError> {
        let (corpus, shards) = self
            .collect_statistics_once(&search.groups, &search.plans_no_fuzzy)
            .await?;
        let unknown_word = has_unknown_word(typed, &search.groups, &corpus);
        let has_fuzzy = search
            .plans_full
            .iter()
            .zip(search.plans_no_fuzzy.iter())
            .any(|(full, plain)| full.tiers.len() != plain.tiers.len());
        if !(unknown_word && has_fuzzy) {
            return Ok(ShardStatistics {
                corpus: Arc::new(corpus),
                shards,
                plans: Arc::clone(&search.plans_no_fuzzy),
                unknown_word,
            });
        }
        let (corpus, shards) = self
            .collect_statistics_once(&search.groups, &search.plans_full)
            .await?;
        Ok(ShardStatistics {
            corpus: Arc::new(corpus),
            shards,
            plans: Arc::clone(&search.plans_full),
            unknown_word,
        })
    }

    /// One round of `collect_statistics` on every shard.
    async fn collect_statistics_once(
        &self,
        groups: &Arc<Vec<QueryGroup>>,
        plans: &Arc<Vec<RankPlan>>,
    ) -> Result<(CorpusStats, Vec<Arc<PreparedShardSearch>>), CorelamoError> {
        let mut set = JoinSet::new();
        for (index, handle) in self.shards.iter().enumerate() {
            let handle = handle.clone();
            let groups = Arc::clone(groups);
            let plans = Arc::clone(plans);
            set.spawn_blocking(move || (index, handle.prepare_search(&groups, &plans)));
        }
        let mut prepared: Vec<Option<Arc<PreparedShardSearch>>> = vec![None; self.shards.len()];
        let mut corpus = CorpusStats::new();
        while let Some(res) = set.join_next().await {
            let (index, shard) =
                res.map_err(|je| CorelamoError::Internal(format!("shard prepare panicked: {je}")))?;
            corpus.merge(shard.stats());
            prepared[index] = Some(Arc::new(shard));
        }
        let shards = prepared
            .into_iter()
            .enumerate()
            .map(|(index, shard)| {
                shard.ok_or_else(|| {
                    CorelamoError::Internal(format!("shard {index} returned no statistics"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((corpus, shards))
    }

    /// A typed word no document has: search a clearly meant spelling instead (fuzzy favours
    /// rare close words and the embedding model cannot read a typo), or offer it.
    async fn check_spelling(
        &self,
        command: &SearchCommand,
        stats: &ShardStatistics,
        allow_correction: bool,
    ) -> SpellingResult {
        if !allow_correction || !stats.unknown_word {
            return SpellingResult::Fine;
        }
        let OwnedValue::String(text) = &command.query else {
            return SpellingResult::Fine;
        };
        let report = match self
            .suggest_corrections(text, command.search_fields.as_deref(), Fuzziness::Auto, 1)
            .await
        {
            Ok(report) => report,
            // the typed query still runs (fuzzy matches it); a correction is only an extra
            Err(_) => {
                return SpellingResult::Fine;
            }
        };
        if report.corrected.is_empty() || report.corrected == *text {
            return SpellingResult::Fine;
        }
        if report.confident {
            SpellingResult::SearchInstead(report.corrected)
        } else {
            SpellingResult::Suggest(QueryCorrection::Suggested(report.corrected))
        }
    }

    /// Every shard ranks its documents with the merged statistics and the plans it was
    /// prepared with.
    async fn rank_on_shards(
        &self,
        search: &CompiledSearch,
        stats: &ShardStatistics,
    ) -> Result<ShardHits, CorelamoError> {
        let mut set = JoinSet::new();
        for (handle, prepared) in self.shards.iter().zip(&stats.shards) {
            let handle = handle.clone();
            let prepared = Arc::clone(prepared);
            let corpus = Arc::clone(&stats.corpus);
            let groups = Arc::clone(&search.groups);
            let plans = Arc::clone(&stats.plans);
            let filters = search.filters.clone();
            let sort_xpaths = search.sort_xpaths.clone();
            let (window, word_fetch, match_mode) =
                (search.window, search.word_fetch, search.match_mode);
            set.spawn_blocking(move || match sort_xpaths.as_ref() {
                Some(sort_xpaths) => handle.rank_sorted(
                    &prepared,
                    &corpus,
                    &groups,
                    &plans,
                    filters.as_deref(),
                    sort_xpaths,
                    window,
                    match_mode,
                ),
                None => handle.rank_top_k(
                    &prepared,
                    &corpus,
                    &groups,
                    &plans,
                    filters.as_deref(),
                    word_fetch,
                    match_mode,
                ),
            });
        }

        let requested = search.shard_request();
        let mut shard_hits = ShardHits {
            items: Vec::new(),
            bounds: Vec::new(),
            some_full: false,
        };
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(ranked)) => {
                    shard_hits.some_full |= ranked.total >= requested;
                    shard_hits.items.extend(ranked.items);
                    if !ranked.bounds.is_empty() {
                        shard_hits.bounds.push(ranked.bounds);
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
        match first_err {
            Some(e) => Err(e),
            None => Ok(shard_hits),
        }
    }

    pub fn record_search(&self, failed: bool, elapsed: Duration) {
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
        if manifests.iter().all(Option::is_none) {
            let _ = fs::remove_dir_all(&backup_root);
        }
        Ok(manifests)
    }
    /// For each candidate, the documents holding it together with every context word, over
    /// all shards.
    async fn co_occurrence_fuzzu(
        &self,
        candidates: Arc<Vec<String>>,
        context: Arc<Vec<String>>,
        xpaths: Arc<Vec<XPathId>>,
    ) -> Result<Vec<u64>, CorelamoError> {
        let mut set = JoinSet::new();
        for handle in &self.shards {
            let handle = handle.clone();
            let (candidates, context, xpaths) = (
                Arc::clone(&candidates),
                Arc::clone(&context),
                Arc::clone(&xpaths),
            );
            set.spawn_blocking(move || handle.co_occurrence_fuzzy(&candidates, &context, &xpaths));
        }
        let mut totals = vec![0u64; candidates.len()];
        while let Some(res) = set.join_next().await {
            let counts = res.map_err(|je| {
                CorelamoError::Internal(format!("co-occurrence shard task panicked: {je}"))
            })?;
            for (total, count) in totals.iter_mut().zip(counts) {
                *total = total.saturating_add(count);
            }
        }
        Ok(totals)
    }
    #[timed(retrieve_opps)]
    pub async fn info_words(&self, words: Vec<String>) -> Result<Vec<WordStats>, CorelamoError> {
        if words.is_empty() {
            return Ok(Vec::new());
        }

        let xpaths = Arc::new(self.policy.read().searchable_xpaths());
        let analyzers_map = self
            .policy
            .read()
            .field_analyzers(&self.analyzer_set.read());

        let mut set = JoinSet::new();
        for handle in &self.shards {
            let handle = handle.clone();
            let words = words.clone();
            let xpaths = Arc::clone(&xpaths);
            let analyzers_map = analyzers_map.clone();
            set.spawn_blocking(move || handle.info_words_direct(&words, &xpaths, &analyzers_map));
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

    #[timed(search)]
    async fn suggest_corrections(
        &self,
        value: &str,
        search_fields: Option<&[String]>,
        fuzziness: Fuzziness,
        size: usize,
    ) -> Result<DidYouMeanReport, CorelamoError> {
        if value.trim().is_empty() || size == 0 {
            return Ok(DidYouMeanReport::empty(value));
        }

        let policy = self.policy.read().clone();

        let xpaths: Arc<Vec<XPathId>> = match search_fields {
            Some(names) => Arc::new(resolve_suggest_xpaths(names, &policy)?),
            None => Arc::new(policy.searchable_xpaths()),
        };

        let spec = FuzzySpec {
            prefix_length: 0,
            max_expansions: 0,
        };

        // one entry per typed piece, as `suggest_direct` returns them; a piece the analyzer
        // drops (a stopword) keeps its slot with no word, so the lists stay aligned
        let analyzer = self.analyzer_set.read().default_analyzer().clone();
        let words: Vec<Option<String>> = value
            .split_whitespace()
            .map(|piece| {
                analyzer
                    .analyze_query(piece)
                    .into_iter()
                    .next()
                    .map(|t| t.text)
            })
            .collect();

        let analyzers_map = self
            .policy
            .read()
            .field_analyzers(&self.analyzer_set.read());

        let mut set = JoinSet::new();
        for handle in &self.shards {
            let handle = handle.clone();
            let value = value.to_string();
            let xpaths = Arc::clone(&xpaths);
            let analyzers_map = analyzers_map.clone();

            set.spawn_blocking(move || {
                handle.suggest_direct(&value, &xpaths, fuzziness, spec, &analyzers_map)
            });
        }

        // One merged map per typed piece, so a term's document frequency is the sum
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

        // the words some document has as typed: the context a misspelled word is checked against
        let known: Vec<String> = words
            .iter()
            .zip(&merged)
            .filter_map(|(word, by_term)| {
                let word = word.as_ref()?;
                by_term
                    .get(word)
                    .is_some_and(|e| e.doc_freq > 0)
                    .then(|| word.clone())
            })
            .collect();
        let context = Arc::new(known);

        //suggestions per analysed word, before we decide which ones to trust.
        let mut by_word: HashMap<String, WordCorrection> = HashMap::new();
        for (word, by_term) in words.into_iter().zip(merged) {
            let Some(word) = word else {
                continue;
            };
            let candidates: Vec<FuzzyExpansion> = by_term.into_values().collect();

            // a word many documents contain is spelled right, however common a neighbour is
            let typed_freq = candidates
                .iter()
                .find(|e| e.term == word)
                .map_or(0, |e| e.doc_freq);
            if typed_freq >= TRUSTED_DOC_FREQ {
                by_word.insert(
                    word.clone(),
                    WordCorrection {
                        suggestions: vec![word],
                        confident: true,
                    },
                );
                continue;
            }

            let unknown = !context.contains(&word);
            let mut expansions = select_expansions(
                &word,
                candidates
                    .iter()
                    .map(|e| (e.term.clone(), e.edits, e.doc_freq)),
                size.max(CONTEXT_CANDIDATES),
            );
            let mut confident = expansions.first().is_some_and(|best| {
                best.term != word && is_confident_correction(best, &candidates)
            });

            // a misspelled word among known ones: the right spelling is the one found in the
            // same documents as the rest of the query ("ipjone 14" -> "iphone", not "ipone")
            if unknown && !context.is_empty() && !expansions.is_empty() {
                let terms = Arc::new(
                    expansions
                        .iter()
                        .map(|e| e.term.clone())
                        .collect::<Vec<_>>(),
                );
                let together = self
                    .co_occurrence_fuzzu(
                        Arc::clone(&terms),
                        Arc::clone(&context),
                        Arc::clone(&xpaths),
                    )
                    .await?;
                let mut ranked: Vec<(u64, FuzzyExpansion)> =
                    together.into_iter().zip(expansions).collect();
                ranked.sort_by(|(a_count, a), (b_count, b)| {
                    b_count
                        .cmp(a_count)
                        .then(a.edits.cmp(&b.edits))
                        .then(b.doc_freq.cmp(&a.doc_freq))
                });
                let best = ranked.first().map_or(0, |(count, _)| *count);
                let second = ranked.get(1).map_or(0, |(count, _)| *count);
                if best > 0 {
                    confident = best >= second.saturating_mul(CONTEXT_DOMINANCE);
                }
                expansions = ranked.into_iter().map(|(_, e)| e).collect();
            }
            expansions.truncate(size);
            let suggestions = expansions.into_iter().map(|e| e.term).collect();
            by_word.insert(
                word,
                WordCorrection {
                    suggestions,
                    confident,
                },
            );
        }

        //we need to construct the original sentence (keeping stopwords/no running -> run)
        let mut pieces: Vec<String> = Vec::new();
        let mut reported: Vec<WordSuggestions> = Vec::new();
        let mut changed = false;
        let mut all_confident = true;

        for piece in value.split_whitespace() {
            let Some(token) = analyzer.analyze_query(piece).into_iter().next() else {
                //stopword: not searchable, keep it as typed.
                pieces.push(piece.to_string());
                continue;
            };

            // `get`, not `remove`: a typo typed twice is corrected both times
            let Some(found) = by_word.get(&token.text) else {
                pieces.push(piece.to_string());
                continue;
            };

            match found.suggestions.first() {
                Some(top) if *top != token.text => {
                    changed = true;
                    all_confident &= found.confident;
                    pieces.push(top.clone());
                }
                _ => {
                    pieces.push(piece.to_string());
                    continue;
                }
            }

            reported.push(WordSuggestions {
                word: token.text,
                suggestions: found.suggestions.clone(),
            });
        }

        let corrected = if changed {
            pieces.join(" ")
        } else {
            value.to_string()
        };

        Ok(DidYouMeanReport {
            original: value.to_string(),
            corrected,
            words: reported,
            confident: changed && all_confident,
        })
    }

    pub async fn did_you_mean(
        &self,
        command: &DidYouMeanRequest,
    ) -> Result<DidYouMeanReport, CorelamoError> {
        let fuzziness = parse_fuzziness(command.fuzziness.as_deref())?;
        let size = command.did_you_mean_count.unwrap_or(3);
        self.suggest_corrections(
            &command.value,
            command.search_fields.as_deref(),
            fuzziness,
            size,
        )
        .await
    }
    #[timed(restore)]
    pub async fn restore_backup(&self, backup_id: &str, user: String) -> Result<(), CorelamoError> {
        if !is_valid_backup_id(backup_id) {
            return Err(CorelamoError::InvalidData(format!(
                "invalid backup id '{backup_id}'"
            )));
        }
        // incremental backups hold only document bytes: their documents need a reindex
        let needs_reindex = self
            .list_backups()?
            .iter()
            .any(|m| m.backup_id == backup_id && m.backup_type == BackupType::Incremental);

        // 1. every shard unpacks into its staging folder; live data untouched
        let mut failures = Vec::new();
        for shard in &self.shards {
            if let Err(e) = shard.prepare_restore(backup_id, user.clone()).await {
                failures.push(format!("shard {}: {}", shard.id(), e));
            }
        }
        if !failures.is_empty() {
            // 2. any failure: every shard drops its staging and runs on its old data again
            for shard in &self.shards {
                let _ = shard.abort_restore().await;
            }
            self.start().await?;
            return Err(CorelamoError::Internal(format!(
                "restore failed on {} of {} shards, nothing was changed: {}",
                failures.len(),
                self.shards.len(),
                failures.join("; ")
            )));
        }

        // 3. all prepared: swap in on every shard
        let mut failures = Vec::new();
        for shard in &self.shards {
            if let Err(e) = shard.commit_restore(backup_id).await {
                failures.push(format!("shard {}: {}", shard.id(), e));
            }
        }
        if !failures.is_empty() {
            // the copies are complete and fsynced; the startup check finishes the swap
            return Err(CorelamoError::Internal(format!(
                "restore commit failed on {} of {} shards; restart to finish it: {}",
                failures.len(),
                self.shards.len(),
                failures.join("; ")
            )));
        }

        self.start().await?;
        if needs_reindex {
            self.reindex()?;
        }
        Ok(())
    }

    pub fn finish_restore(&self, ok: bool) {
        self.db_stats.finish_restore(ok);
    }

    pub fn finish_backup(&self, ok: bool) {
        self.db_stats.finish_backup(ok);
    }
    //to check before restore if there are any backups
    pub fn has_backup(&self, backup_id: &str) -> bool {
        is_valid_backup_id(backup_id) && self.backup_dir.join(backup_id).is_dir()
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
    ) -> Result<(Vec<SearchDocumentHit>, TotalHits, Option<QueryCorrection>), CorelamoError> {
        if command.sort.is_some() {
            return Err(CorelamoError::InvalidData(
                "vector queries do not support sort yet".into(),
            ));
        }
        let policy = self.policy.read().clone();
        let filters = compile_filters(&self.analyzer_set.read(), command, &policy)?;
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
        // one source: more exist if the vector search filled what it was asked for, or
        // returned more than this page needs
        let more = hits.len() >= k || hits.len() > fetch;
        let just_hits: Vec<SearchHit> = hits
            .into_iter()
            .map(|hit| SearchHit {
                doc_id: hit.doc_id,
                matched_terms: 0,
                weight_sum: 0,
                distance_factor: 1.0,
                score: hit.score,
                tier: 0,
                ..SearchHit::default()
            })
            .collect();
        let (hits, total) = self
            .resolve_page(
                just_hits,
                offset,
                limit,
                more,
                command,
                &policy,
                None,
                Arc::new(Vec::new()),
                &[],
                None,
                Arc::new(Vec::new()),
            )
            .await?;
        Ok((hits, total, None))
    }

    //loads the requested page of ranked hits as documents, with highlights when asked.
    async fn resolve_page(
        &self,
        just_hits: Vec<SearchHit>,
        offset: usize,
        limit: usize,
        more: bool,
        command: &SearchCommand,
        policy: &IndexPolicy,
        highlight: Option<Vec<(XPathId, String, Analyzer, HighlightFieldOptions, bool, bool)>>,
        groups: Arc<Vec<QueryGroup>>,
        prepared: &[Arc<PreparedShardSearch>],
        corpus: Option<Arc<CorpusStats>>,
        plans: Arc<Vec<RankPlan>>,
    ) -> Result<(Vec<SearchDocumentHit>, TotalHits), CorelamoError> {
        if offset >= just_hits.len() {
            return Ok((
                Vec::new(),
                TotalHits {
                    value: 0,
                    more: false,
                },
            ));
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

        //recover matched spans only when the caller asked to highlight
        let want_matches = highlight.is_some();

        //sorted hits -> full documents + matched spans for pretty results :)
        let mut set = JoinSet::new();
        for (idx, hits) in by_shard {
            let handle = self.shards[idx].clone();
            let return_fields = command.return_fields.clone();
            let policy = policy.clone();
            let groups = Arc::clone(&groups);
            let prepared = prepared.get(idx).cloned();
            let corpus = corpus.as_ref().map(Arc::clone);
            let positions: Vec<usize> = hits.iter().map(|(pos, _)| *pos).collect();
            let bare_hits: Vec<SearchHit> = hits.into_iter().map(|(_, h)| h).collect();
            let plans = Arc::clone(&plans);
            set.spawn(async move {
                let matched = if want_matches {
                    match (&prepared, &corpus) {
                        (Some(prep), Some(corp)) => handle.matched_terms_for(
                            &bare_hits,
                            groups.as_slice(),
                            &plans,
                            prep,
                            corp,
                        ),
                        _ => vec![Vec::new(); bare_hits.len()],
                    }
                } else {
                    vec![Vec::new(); bare_hits.len()]
                };
                let resolved = handle
                    .resolve_hits_direct(bare_hits, return_fields.as_ref(), &policy)
                    .await;

                (positions, resolved, matched)
            });
        }

        //join results
        let mut resolved: Vec<Option<SearchDocumentHit>> = vec![None; page_len];
        let mut matched_by_doc: HashMap<DocId, Vec<MatchedTerm>> = HashMap::new();
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok((positions, Ok(hits), matched)) => {
                    for (pos, (hit, m)) in positions.into_iter().zip(hits.into_iter().zip(matched))
                    {
                        if let Some(h) = hit {
                            matched_by_doc.insert(h.internal_id, m);
                            resolved[pos] = Some(h);
                        }
                    }
                }
                Ok((_, Err(e), _)) if first_err.is_none() => {
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

        //populate highlights from the matched spans
        if let Some(fields) = &highlight {
            let fields = fields.clone();
            let matched_by_doc = matched_by_doc;
            resolved = tokio::task::spawn_blocking(move || {
                for slot in &mut resolved {
                    let Some(hit) = slot else {
                        continue;
                    };
                    let matched = matched_by_doc
                        .get(&hit.internal_id)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    let mut highlights = Vec::new();
                    if !matched.is_empty() {
                        let value = match &hit.full_doc {
                            //smart shit to highlight non-returned fields
                            Some(full) => Cow::Borrowed(full),
                            None => match hit.doc.value() {
                                Ok(v) => v,
                                Err(_) => continue,
                            },
                        };
                        for (xpath, full_path, analyzer, opts, flattened, row_keyed) in &fields {
                            let field_matches: Vec<&MatchedTerm> =
                                matched.iter().filter(|m| m.xpath == *xpath).collect();
                            if field_matches.is_empty() {
                                continue;
                            }

                            //i love edgecases
                            if *flattened && *row_keyed {
                                //cast{name, surname, aliases:[..,..,..]}
                                highlights.extend(highlight_field_row_keyed_flattened(
                                    &value,
                                    full_path,
                                    analyzer,
                                    &field_matches,
                                    opts,
                                ));
                            } else if *flattened {
                                //cast: ["tom holland", "kristians svikis"]
                                let texts = field_texts(&value, full_path);
                                highlights.extend(highlight_field_array(
                                    &texts,
                                    analyzer,
                                    &field_matches,
                                    opts,
                                ));
                            } else if *row_keyed {
                                //cast: [{name, surname}, {name, surnames}]
                                highlights.extend(highlight_field_row_keyed(
                                    &value,
                                    full_path,
                                    analyzer,
                                    &field_matches,
                                    opts,
                                ));
                            } else {
                                //name: "..."
                                for text in field_texts(&value, full_path) {
                                    highlights.extend(highlight_field(
                                        &text,
                                        analyzer,
                                        &field_matches,
                                        opts,
                                    ));
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

        let resolved: Vec<SearchDocumentHit> = resolved.into_iter().flatten().collect();
        let total = TotalHits {
            value: resolved.len() as u64,
            more,
        };
        Ok((resolved, total))
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
                    match this.backup_incremental("System".to_string()).await {
                        Ok(_) => true,
                        // no base backup, or store compaction ran since: take a full one instead
                        Err(_) => this.backup_full("System".to_string()).await.is_ok(),
                    }
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
        query_value: &OwnedValue,
        command: &SearchCommand,
        policy: &IndexPolicy,
        strict_sort: bool,
    ) -> Option<(String, Vec<XPathId>)> {
        let OwnedValue::String(text) = query_value else {
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
                EmbeddingStatus::Embedding
                | EmbeddingStatus::Waiting
                | EmbeddingStatus::Failing => embedding.progress >= SEMANTIC_MIN_PROGRESS,
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
        filters: Option<Arc<std::collections::HashMap<String, FieldQuery>>>,
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
/// Semantic fusion that keeps what the query names first: hits from the name and exact tiers
/// (tiers below `named_limit`) stay on top in their order; the others are fused with the
/// vector hits by reciprocal rank and follow. Fusing everything would let a document that is
/// only close in meaning (ranked well by both lists) pass the one whose title is the query.
/// `add_vector_only`: also add documents only the vector side found (not for sorted searches,
/// whose hits need sort keys). `items` must be sorted by `rank_order`.
fn fuse_keeping_named(
    mut items: Vec<(SearchHit, Vec<Option<f64>>)>,
    vectors: &[VectorHit],
    named_limit: u8,
    add_vector_only: bool,
) -> Vec<(SearchHit, Vec<Option<f64>>)> {
    let named = items.partition_point(|(hit, _)| hit.tier < named_limit);
    let rest = items.split_off(named);
    let kept: HashSet<DocId> = items.iter().map(|(hit, _)| hit.doc_id).collect();
    let vectors: Vec<VectorHit> = vectors
        .iter()
        .filter(|hit| !kept.contains(&hit.doc_id))
        .copied()
        .collect();
    items.extend(
        fuse_rrf(rest, &vectors, add_vector_only)
            .into_iter()
            .map(|(mut hit, keys)| {
                // after every kept hit, whatever later step orders by tier
                hit.tier = named_limit;
                hit.level = 0;
                (hit, keys)
            }),
    );
    items
}

/// Reciprocal Rank Fusion of the word ranking and the vector ranking: every document scores
/// Σ 1 / (`RRF_K` + rank) over the lists it appears in. It only uses ranks, so BM25 and cosine
/// scores never need to be on the same scale. `add_vector_only` adds documents only the vector
/// side found (they carry no sort keys, so sorted searches pass false). Fused scores are
/// scaled so the best is 1.0, the scale blended sorting expects; all hits share tier 0.
fn fuse_rrf(
    words: Vec<(SearchHit, Vec<Option<f64>>)>,
    vectors: &[VectorHit],
    add_vector_only: bool,
) -> Vec<(SearchHit, Vec<Option<f64>>)> {
    let contribution = |rank: usize| 1.0 / (RRF_K + (rank as f32) + 1.0);
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
                ..SearchHit::default()
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
            hit.level = 0;
            (hit, keys)
        })
        .collect();
    out.sort_unstable_by(|(a, _), (b, _)| {
        b.score.total_cmp(&a.score).then(a.doc_id.cmp(&b.doc_id))
    });
    out
}

/// The page a search returns: `fetch` = offset + limit hits are ranked.
#[derive(Debug, Clone, Copy)]
struct PageRequest {
    limit: usize,
    offset: usize,
    fetch: usize,
}

/// Everything a search decides before it asks a shard anything.
/// holly porno
struct CompiledSearch {
    policy: IndexPolicy,
    groups: Arc<Vec<QueryGroup>>,
    filters: Option<Arc<std::collections::HashMap<String, FieldQuery>>>,
    sorts: Option<ResolvedSort>,
    sort_xpaths: Option<Arc<Vec<XPathId>>>,
    semantic: Option<(String, Vec<XPathId>)>,
    //the bools are anything related to arrays
    highlight: Option<Vec<(XPathId, String, Analyzer, HighlightFieldOptions, bool, bool)>>,
    plans_full: Arc<Vec<RankPlan>>,
    plans_no_fuzzy: Arc<Vec<RankPlan>>,
    match_mode: MatchMode,
    /// hits each shard ranks: the page, or the fusion depth when vectors are fused
    word_fetch: usize,
    /// hits each shard ranks for a sorted search
    window: usize,
}

impl CompiledSearch {
    fn shard_request(&self) -> usize {
        if self.sorts.is_some() {
            self.window
        } else {
            self.word_fetch
        }
    }
}

/// Every shard prepared for ranking, with the statistics of all of them merged.
struct ShardStatistics {
    corpus: Arc<CorpusStats>,
    shards: Vec<Arc<PreparedShardSearch>>,
    /// the plans the shards were prepared with: rank with exactly these
    plans: Arc<Vec<RankPlan>>,
    /// a typed word is in no document of its group's fields
    unknown_word: bool,
}
/// What `check_spelling` decided about a typed word no document has.
enum SpellingResult {
    /// the correction is clearly what was meant: search it instead
    SearchInstead(String),
    /// keep the typed query and offer this spelling
    Suggest(QueryCorrection),
    Fine,
}

/// Every shard's ranked hits, before they are merged into one order.
struct ShardHits {
    items: Vec<(SearchHit, Vec<Option<f64>>)>,
    bounds: Vec<Vec<(Option<f64>, Option<f64>)>>,
    /// some shard had more matches than it returned
    some_full: bool,
}

/// The requested page, or `None` when it holds no documents.
fn requested_page(command: &SearchCommand) -> Result<Option<PageRequest>, CorelamoError> {
    let limit = command.docs.unwrap_or(10);
    if limit > MAX_RESULTS_SIZE {
        return Err(CorelamoError::InvalidData(format!(
            "docs may be at most {MAX_RESULTS_SIZE}, got {limit}"
        )));
    }

    let offset = command.offset.unwrap_or(0);
    let fetch = offset.saturating_add(limit);
    Ok((fetch > 0).then_some(PageRequest {
        limit,
        offset,
        fetch,
    }))
}

fn highlight_request(
    command: &SearchCommand,
    policy: &IndexPolicy,
    analyzers: &AnalyzerSet,
    searched: &HashSet<XPathId>,
) -> Result<
    Option<Vec<(XPathId, String, Analyzer, HighlightFieldOptions, bool, bool)>>,
    CorelamoError,
> {
    let Some(cmd) = &command.highlight else {
        return Ok(None);
    };
    let mut fields = Vec::with_capacity(cmd.fields.len());
    for (name, opts) in &cmd.fields {
        let field = policy
            .field_by_path(name)
            .ok_or_else(|| CorelamoError::PathNotIndexed(name.clone()))?;
        let xpath = field.xpath(policy);

        //a field can only be highlighted if the query actually searched it
        if !searched.contains(&xpath) {
            return Err(CorelamoError::InvalidData(format!(
                "cannot highlight '{name}': it is not a searched field"
            )));
        }

        let analyzer = field_analyzer(field, analyzers).clone();
        let flattened = field.repeated;
        let row_keyed = field.row_keyed;
        fields.push((
            xpath,
            field.full_path.clone(),
            analyzer,
            opts.clone(),
            flattened,
            row_keyed,
        ));
    }
    Ok(Some(fields))
}

/// A typed word, analyzed by a group's analyzer, is in no document of that group's fields.
fn has_unknown_word(typed: &OwnedValue, groups: &[QueryGroup], corpus: &CorpusStats) -> bool {
    let OwnedValue::String(text) = typed else {
        return false;
    };
    let fields: Vec<(&QueryGroup, Vec<XPathId>)> = groups
        .iter()
        .map(|group| {
            let xpaths = group
                .ctxs
                .iter()
                .filter(|ctx| !ctx.row_keyed)
                .map(|ctx| ctx.xpath)
                .collect();
            (group, xpaths)
        })
        .collect();
    text.split_whitespace().any(|word| {
        let mut analyzed_somewhere = false;
        let known = fields.iter().any(|(group, xpaths)| {
            let Some(token) = group.analyzer.analyze_query(word).into_iter().next() else {
                // a stopword or too short for this analyzer: says nothing about the word
                return false;
            };
            analyzed_somewhere = true;
            xpaths
                .iter()
                .any(|&xpath| corpus.term_doc_freq(xpath, &token.text) > 0)
        });
        analyzed_somewhere && !known
    })
}

/// Every shard's hits in one order: semantic fusion (what the query names stays first), then
/// the requested sort.
fn merge_and_sort(
    search: &CompiledSearch,
    plans: &[RankPlan],
    shard_hits: ShardHits,
    vectors: Option<&[VectorHit]>,
) -> Vec<(SearchHit, Vec<Option<f64>>)> {
    let mut items = shard_hits.items;
    let named_limit = plans
        .iter()
        .map(RankPlan::named_tier_limit)
        .min()
        .unwrap_or(0);
    items.sort_unstable_by(|(a, _), (b, _)| rank_order(a, b));
    match search.sorts.as_ref() {
        Some(sorts) => {
            if let Some(vectors) = vectors {
                // vector-only documents carry no sort keys: fuse only what the words found
                items = fuse_keeping_named(items, vectors, named_limit, false);
            }
            let bounds = fold_sort_bounds(&sorts.fields, shard_hits.bounds);
            match sorts.mode {
                SortMode::Blend => order_blended(&mut items, &sorts.fields, &bounds),
                SortMode::Strict => order_strict(&mut items, &sorts.fields),
            }
        }
        None => {
            if let Some(vectors) = vectors {
                items.truncate(search.word_fetch);
                items = fuse_keeping_named(items, vectors, named_limit, true);
            }
        }
    }
    items
}
