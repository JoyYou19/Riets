//! Text embedding for corelamo vector search.
//!
//! Models are loaded once per process through `ModelRegistry` and shared by every database.
//! Documents are embedded as L2-normalized int8 vectors (scale `QUANT_SCALE`); queries as
//! L2-normalized f32 vectors, scored against int8 rows as `dot(query, row) / QUANT_SCALE`.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

// /// int8 value = round(component * QUANT_SCALE). Components are within [-1, 1] after normalizing.
 pub const QUANT_SCALE: f32 = 127.0;

// /// Docs per model call. Attention memory grows with batch * tokens^2, so keep it small on CPU.
// const BATCH_SIZE: usize = 128;
// /// Token cap per text. The start of a document says what it is about.
// const MAX_TOKENS: usize = 256;
// /// Character cap applied before tokenizing, so the tokenizer never processes whole articles.
// const MAX_CHARS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub model_id: String,
    /// Bulk model copies; up to this many shards embed at once.
    pub instances: usize,
    /// Texts per model run. Attention memory grows with batch * tokens^2.
    pub batch_size: usize,
    /// Tokens the model reads per text. Changing it changes the vectors (they are re-embedded).
    pub max_tokens: usize,
    /// Characters kept before tokenizing, so the tokenizer never processes whole articles.
    pub max_chars: usize,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            model_id: "bge-small-en-v1.5".to_string(),
            instances: 1,
            batch_size: 32,
            max_tokens: 256,
            max_chars: 2000,
        }
    }
}
impl ModelConfig {
    pub fn validate(&self) -> io::Result<()> {
        let invalid = |msg: String| Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
        if find_model(&self.model_id).is_none() {
            return invalid(format!(
                "unknown embedding model '{}' (known: {})",
                self.model_id,
                known_model_ids()
            ));
        }
        if self.instances == 0 || self.batch_size == 0 {
            return invalid("embedding instances and batch size must be at least 1".into());
        }
        if !(16..=512).contains(&self.max_tokens) {
            return invalid(format!("embedding max tokens must be 16..=512, got {}", self.max_tokens));
        }
        if self.max_chars < self.max_tokens * 4 {
            return invalid(format!(
                "embedding max chars ({}) must be at least 4x max tokens ({}), or text is cut before the model sees it",
                self.max_chars, self.max_tokens
            ));
        }
        Ok(())
    }
}
/// A supported embedding model. `id` is what settings and vector file headers store.
pub struct ModelSpec {
    pub id: &'static str,
    pub dims: usize,
    model: fn() -> EmbeddingModel,
    query_prefix: &'static str,
    document_prefix: &'static str,
}

pub struct Embedder {
    spec: &'static ModelSpec,
    cache_dir: PathBuf,
    config: ModelConfig,
    /// Written to vector files; changes whenever stored vectors would change meaning.
    vector_id: String,
    bulk: Vec<Mutex<TextEmbedding>>,
    queries: OnceLock<Mutex<TextEmbedding>>,
}

/// Exclusive use of one bulk model copy until dropped.
pub struct EmbedLease<'a> {
    owner: &'a Embedder,
    model: MutexGuard<'a, TextEmbedding>,
}

impl Embedder {
    fn load(spec: &'static ModelSpec, cache_dir: PathBuf, config: &ModelConfig) -> io::Result<Self> {
        let mut bulk = Vec::with_capacity(config.instances);
        for _ in 0..config.instances {
            bulk.push(Mutex::new(load_model(spec, cache_dir.clone(), config.max_tokens)?));
        }
        Ok(Self {
            spec,
            cache_dir,
            config: config.clone(),
            vector_id: format!("{}/t{}", spec.id, config.max_tokens),
            bulk,
            queries: OnceLock::new(),
        })
    }

    pub fn spec(&self) -> &'static ModelSpec {
        self.spec
    }

    /// Identifies vectors made by this model and settings; stored in every vector file.
    pub fn vector_id(&self) -> &str {
        &self.vector_id
    }

    pub fn instances(&self) -> usize {
        self.bulk.len()
    }

    /// Any free bulk model copy, without waiting.
    pub fn try_lease(&self) -> io::Result<Option<EmbedLease<'_>>> {
        for slot in &self.bulk {
            match slot.try_lock() {
                Ok(model) => return Ok(Some(EmbedLease { owner: self, model })),
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Poisoned(_)) => {
                    return Err(io::Error::other("embedding model lock poisoned"))
                }
            }
        }
        Ok(None)
    }

    /// A free bulk model copy if there is one, otherwise waits for the first.
    pub fn lease(&self) -> io::Result<EmbedLease<'_>> {
        if let Some(lease) = self.try_lease()? {
            return Ok(lease);
        }
        let model = self.bulk[0]
            .lock()
            .map_err(|_| io::Error::other("embedding model lock poisoned"))?;
        Ok(EmbedLease { owner: self, model })
    }

    pub fn embed_documents(&self, texts: &[&str]) -> io::Result<Vec<Vec<i8>>> {
        self.lease()?.embed_documents(texts)
    }

    /// Embeds a search query as a normalized f32 vector, on the query-only model copy.
    pub fn embed_query(&self, text: &str) -> io::Result<Vec<f32>> {
        let text = text.trim();
        if text.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "query is empty"));
        }
        let queries = match self.queries.get() {
            Some(model) => model,
            None => {
                let model = load_model(self.spec, self.cache_dir.clone(), self.config.max_tokens)?;
                // Two concurrent first queries may both load; the loser's copy is dropped.
                let _ = self.queries.set(Mutex::new(model));
                self.queries.get().expect("set above")
            }
        };
        let mut model = queries
            .lock()
            .map_err(|_| io::Error::other("query model lock poisoned"))?;
        let prefixed = format!(
            "{}{}",
            self.spec.query_prefix,
            truncate_chars(text, self.config.max_chars)
        );
        let mut v = run_model(&mut model, vec![prefixed], self.config.batch_size)?
            .pop()
            .ok_or_else(|| io::Error::other("model returned no vector for the query"))?;
        check_dims(self.spec, &v)?;
        normalize(&mut v);
        Ok(v)
    }
}

impl EmbedLease<'_> {
    pub fn spec(&self) -> &'static ModelSpec {
        self.owner.spec
    }

    pub fn vector_id(&self) -> &str {
        &self.owner.vector_id
    }

    /// Embeds documents, returning one int8 vector per input in input order.
    /// Empty or whitespace-only inputs are rejected so callers never store zero vectors.
    pub fn embed_documents(&mut self, texts: &[&str]) -> io::Result<Vec<Vec<i8>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(pos) = texts.iter().position(|t| t.trim().is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("text at position {pos} is empty"),
            ));
        }
        let spec = self.owner.spec;
        let config = &self.owner.config;

        // Embed in length order so each batch pads to a similar length.
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_unstable_by_key(|&i| texts[i].len());
        let sorted: Vec<String> = order
            .iter()
            .map(|&i| format!("{}{}", spec.document_prefix, truncate_chars(texts[i], config.max_chars)))
            .collect();

        let vectors = run_model(&mut self.model, sorted, config.batch_size)?;

        let mut out: Vec<Vec<i8>> = vec![Vec::new(); texts.len()];
        for (&original, mut v) in order.iter().zip(vectors) {
            check_dims(spec, &v)?;
            normalize(&mut v);
            out[original] = quantize(&v);
        }
        Ok(out)
    }
}

fn load_model(spec: &'static ModelSpec, cache_dir: PathBuf, max_tokens: usize) -> io::Result<TextEmbedding> {
    // keep your AVX2 check here if you added it
    let options = InitOptions::new((spec.model)())
        .with_max_length(max_tokens)
        .with_cache_dir(cache_dir)
        .with_show_download_progress(false);
    TextEmbedding::try_new(options).map_err(model_error)
}

fn run_model(model: &mut TextEmbedding, texts: Vec<String>, batch_size: usize) -> io::Result<Vec<Vec<f32>>> {
    let count = texts.len();
    let vectors = model.embed(texts, Some(batch_size)).map_err(model_error)?;
    if vectors.len() != count {
        return Err(io::Error::other(format!(
            "model returned {} vectors for {count} texts",
            vectors.len()
        )));
    }
    Ok(vectors)
}
fn bge_small_en() -> EmbeddingModel {
    EmbeddingModel::BGESmallENV15 //EmbeddingModel::BGESmallENV15Q
}

fn multilingual_e5_small() -> EmbeddingModel {
    EmbeddingModel::MultilingualE5Small
}

pub static MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "bge-small-en-v1.5",
        dims: 384,
        model: bge_small_en,
        query_prefix: "Represent this sentence for searching relevant passages: ",
        document_prefix: "",
    },
    ModelSpec {
        id: "multilingual-e5-small",
        dims: 384,
        model: multilingual_e5_small,
        query_prefix: "query: ",
        document_prefix: "passage: ",
    },
];

pub fn find_model(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|spec| spec.id == id)
}

fn known_model_ids() -> String {
    MODELS.iter().map(|spec| spec.id).collect::<Vec<_>>().join(", ")
}

/// One loaded model. Shared through `Arc`; calls are serialized by an internal lock.
/// One embedding model, shared through `Arc`.
/// Bulk document embedding goes through `lease()`, one holder at a time. Queries use a
/// separate instance, so a search never waits behind a bulk embedding round.

fn check_dims(spec: &ModelSpec, v: &[f32]) -> io::Result<()> {
    if v.len() != spec.dims {
        return Err(io::Error::other(format!(
            "model '{}' returned {} dims, expected {}",
            spec.id,
            v.len(),
            spec.dims
        )));
    }
    Ok(())
}
/// Server-wide set of loaded models. Each model is loaded on first request and then shared.
pub struct ModelRegistry {
    cache_dir: PathBuf,
    default_model: &'static ModelSpec,
    config: ModelConfig,
    loaded: Mutex<HashMap<&'static str, Arc<Embedder>>>,
}

impl fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelRegistry")
            .field("cache_dir", &self.cache_dir)
            .field("default_model", &self.default_model.id)
            .finish_non_exhaustive()
    }
}

impl ModelRegistry {
    /// Validates the model id and prepares `cache_dir`. Loads nothing yet.
    pub fn new(cache_dir: PathBuf, config: ModelConfig) -> io::Result<Self> {
        config.validate()?;
        let default_model = find_model(&config.model_id).expect("validated above");
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            cache_dir,
            default_model,
            config,
            loaded: Mutex::new(HashMap::new()),
        })
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn default_model(&self) -> &'static ModelSpec {
        self.default_model
    }

    /// Returns the shared instance of `model_id`, loading it on first use.
    pub fn get(&self, model_id: &str) -> io::Result<Arc<Embedder>> {
        let spec = find_model(model_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown embedding model '{model_id}' (known: {})", known_model_ids()),
            )
        })?;
        // Held during loading so concurrent first requests load the model only once.
        let mut loaded = self
            .loaded
            .lock()
            .map_err(|_| io::Error::other("model registry lock poisoned"))?;
        if let Some(embedder) = loaded.get(spec.id) {
            return Ok(Arc::clone(embedder));
        }
        let embedder = Arc::new(Embedder::load(spec, self.cache_dir.clone(), &self.config)?);
        loaded.insert(spec.id, Arc::clone(&embedder));
        Ok(embedder)
    }

    pub fn default_embedder(&self) -> io::Result<Arc<Embedder>> {
        self.get(self.default_model.id)
    }
}

fn model_error(e: impl fmt::Display) -> io::Error {
    io::Error::other(format!("embedding model: {e}"))
}

fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

fn quantize(v: &[f32]) -> Vec<i8> {
    v.iter()
        .map(|&x| (x * QUANT_SCALE).round().clamp(-QUANT_SCALE, QUANT_SCALE) as i8)
        .collect()
}

 