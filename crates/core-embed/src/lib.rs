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

/// int8 value = round(component * QUANT_SCALE). Components are within [-1, 1] after normalizing.
pub const QUANT_SCALE: f32 = 127.0;

/// Docs per model call. Attention memory grows with batch * tokens^2, so keep it small on CPU.
const BATCH_SIZE: usize = 32;
/// Token cap per text. The start of a document says what it is about.
const MAX_TOKENS: usize = 256;
/// Character cap applied before tokenizing, so the tokenizer never processes whole articles.
const MAX_CHARS: usize = 2000;

/// A supported embedding model. `id` is what settings and vector file headers store.
pub struct ModelSpec {
    pub id: &'static str,
    pub dims: usize,
    model: fn() -> EmbeddingModel,
    query_prefix: &'static str,
    document_prefix: &'static str,
}

fn bge_small_en() -> EmbeddingModel {
    EmbeddingModel::BGESmallENV15
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
pub struct Embedder {
    spec: &'static ModelSpec,
    cache_dir: PathBuf,
    bulk: Mutex<TextEmbedding>,
    queries: OnceLock<Mutex<TextEmbedding>>,
}

/// Exclusive use of the bulk model until dropped.
pub struct EmbedLease<'a> {
    spec: &'static ModelSpec,
    model: MutexGuard<'a, TextEmbedding>,
}

impl Embedder {
    fn load(spec: &'static ModelSpec, cache_dir: PathBuf) -> io::Result<Self> {
        let bulk = load_model(spec, cache_dir.clone())?;
        Ok(Self {
            spec,
            cache_dir,
            bulk: Mutex::new(bulk),
            queries: OnceLock::new(),
        })
    }

    pub fn spec(&self) -> &'static ModelSpec {
        self.spec
    }

    /// Blocks until the bulk model is free, then holds it until the lease is dropped.
    pub fn lease(&self) -> io::Result<EmbedLease<'_>> {
        let model = self
            .bulk
            .lock()
            .map_err(|_| io::Error::other("embedding model lock poisoned"))?;
        Ok(EmbedLease {
            spec: self.spec,
            model,
        })
    }

    /// Embeds documents with a short-lived lease. Prefer `lease()` for repeated batches.
    pub fn embed_documents(&self, texts: &[&str]) -> io::Result<Vec<Vec<i8>>> {
        self.lease()?.embed_documents(texts)
    }

    /// Embeds a search query as a normalized f32 vector, on the query-only model instance.
    pub fn embed_query(&self, text: &str) -> io::Result<Vec<f32>> {
        let text = text.trim();
        if text.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "query is empty"));
        }
        let queries = match self.queries.get() {
            Some(model) => model,
            None => {
                let model = load_model(self.spec, self.cache_dir.clone())?;
                // Two concurrent first queries may both load; the loser's copy is dropped.
                let _ = self.queries.set(Mutex::new(model));
                self.queries.get().expect("set above")
            }
        };
        let mut model = queries
            .lock()
            .map_err(|_| io::Error::other("query model lock poisoned"))?;
        let prefixed = format!("{}{}", self.spec.query_prefix, truncate_chars(text, MAX_CHARS));
        let mut v = run_model(&mut model, vec![prefixed])?
            .pop()
            .ok_or_else(|| io::Error::other("model returned no vector for the query"))?;
        check_dims(self.spec, &v)?;
        normalize(&mut v);
        Ok(v)
    }
    /// The bulk model if it is free right now, without waiting.
    pub fn try_lease(&self) -> io::Result<Option<EmbedLease<'_>>> {
        match self.bulk.try_lock() {
            Ok(model) => Ok(Some(EmbedLease {
                spec: self.spec,
                model,
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Poisoned(_)) => Err(io::Error::other("embedding model lock poisoned")),
        }
    }
}

impl EmbedLease<'_> {
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

        // Embed in length order so each batch pads to a similar length.
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_unstable_by_key(|&i| texts[i].len());
        let sorted: Vec<String> = order
            .iter()
            .map(|&i| format!("{}{}", self.spec.document_prefix, truncate_chars(texts[i], MAX_CHARS)))
            .collect();

        let vectors = run_model(&mut self.model, sorted)?;

        let mut out: Vec<Vec<i8>> = vec![Vec::new(); texts.len()];
        for (&original, mut v) in order.iter().zip(vectors) {
            check_dims(self.spec, &v)?;
            normalize(&mut v);
            out[original] = quantize(&v);
        }
        Ok(out)
    }
    pub fn spec(&self) -> &'static ModelSpec {
        self.spec
    }
}

fn load_model(spec: &'static ModelSpec, cache_dir: PathBuf) -> io::Result<TextEmbedding> {
    let options = InitOptions::new((spec.model)())
        .with_max_length(MAX_TOKENS)
        .with_cache_dir(cache_dir)
        .with_show_download_progress(false);
    TextEmbedding::try_new(options).map_err(model_error)
}

fn run_model(model: &mut TextEmbedding, texts: Vec<String>) -> io::Result<Vec<Vec<f32>>> {
    let count = texts.len();
    let vectors = model.embed(texts, Some(BATCH_SIZE)).map_err(model_error)?;
    if vectors.len() != count {
        return Err(io::Error::other(format!(
            "model returned {} vectors for {count} texts",
            vectors.len()
        )));
    }
    Ok(vectors)
}

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
    pub fn new(cache_dir: PathBuf, default_model_id: &str) -> io::Result<Self> {
        let default_model = find_model(default_model_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown embedding model '{default_model_id}' (known: {})",
                    known_model_ids()
                ),
            )
        })?;
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            cache_dir,
            default_model,
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
        let embedder = Arc::new(Embedder::load(spec, self.cache_dir.clone())?);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_resolve() {
        assert_eq!(find_model("bge-small-en-v1.5").map(|m| m.dims), Some(384));
        assert!(find_model("nope").is_none());
    }

    #[test]
    fn unknown_default_model_is_rejected_before_touching_disk() {
        let err = ModelRegistry::new(PathBuf::from("/nonexistent/models"), "nope").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate_chars("ārā", 2), "ār");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[test]
    fn quantize_stays_in_range() {
        let mut v = vec![3.0, -4.0];
        normalize(&mut v);
        assert_eq!(quantize(&v), vec![76, -102]);
    }
}