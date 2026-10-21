//! Turning texts into vectors: model prefixes, character truncation, length-sorted batching,
//! L2 normalization and int8 quantization, on top of a loaded model copy.

use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};

use crate::QUANT_SCALE;
use crate::config::ModelConfig;
use crate::device::Device;
use crate::model::{LoadedModel, ModelSpec, load_model};

/// One embedding model, shared through `Arc`.
///
/// Bulk document embedding goes through `lease()`, one holder per model copy at a time.
/// Queries use a separate copy, so a search never waits behind a bulk embedding round.
pub struct Embedder {
    spec: &'static ModelSpec,
    cache_dir: PathBuf,
    config: ModelConfig,
    /// Written to vector files; changes whenever stored vectors would change meaning.
    vector_id: String,
    /// Where the bulk copies run, as decided when the first copy loaded.
    device: Device,
    /// Why `auto` chose the CPU on a machine with an NVIDIA driver.
    cpu_fallback: Option<String>,
    bulk: Vec<Mutex<LoadedModel>>,
    queries: OnceLock<Mutex<LoadedModel>>,
}

/// Exclusive use of one bulk model copy until dropped.
pub struct EmbedLease<'a> {
    owner: &'a Embedder,
    model: MutexGuard<'a, LoadedModel>,
}

impl Embedder {
    pub(crate) fn load(
        spec: &'static ModelSpec,
        cache_dir: PathBuf,
        config: &ModelConfig,
    ) -> io::Result<Self> {
        let mut bulk = Vec::with_capacity(config.instances);
        for _ in 0..config.instances {
            bulk.push(load_model(spec, cache_dir.clone(), config)?);
        }
        let device = bulk.first().map_or(Device::Cpu, LoadedModel::device);
        let cpu_fallback = bulk
            .first()
            .and_then(LoadedModel::cpu_fallback)
            .map(str::to_owned);
        // Both backends give the same vectors for the same model and settings (the pooled one
        // pools inside ONNX Runtime instead of in fastembed), so neither the backend nor the
        // device is part of the id: switching between them never forces a re-embed.
        let vector_id = format!("{}/t{}", spec.id, config.max_tokens);
        Ok(Self {
            spec,
            cache_dir,
            config: config.clone(),
            vector_id,
            device,
            cpu_fallback,
            bulk: bulk.into_iter().map(Mutex::new).collect(),
            queries: OnceLock::new(),
        })
    }

    pub fn spec(&self) -> &'static ModelSpec {
        self.spec
    }

    /// Where the bulk model copies run.
    pub fn device(&self) -> Device {
        self.device
    }

    /// Set when `embedding-device = "auto"` found an NVIDIA driver but CUDA did not start;
    /// holds the reason. The model then runs on the CPU.
    pub fn cpu_fallback(&self) -> Option<&str> {
        self.cpu_fallback.as_deref()
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
                    return Err(io::Error::other("embedding model lock poisoned"));
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "query is empty",
            ));
        }
        let queries = if let Some(model) = self.queries.get() { model } else {
            let model = load_model(self.spec, self.cache_dir.clone(), &self.config)?;
            // Two concurrent first queries may both load; the loser's copy is dropped.
            let _ = self.queries.set(Mutex::new(model));
            self.queries.get().expect("set above")
        };
        let mut model = queries
            .lock()
            .map_err(|_| io::Error::other("query model lock poisoned"))?;
        let prefixed = format!(
            "{}{}",
            self.spec.query_prefix,
            truncate_chars(text, self.config.max_chars)
        );
        let mut v = model
            .embed(vec![prefixed], self.config.batch_size)?
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
            .map(|&i| {
                format!(
                    "{}{}",
                    spec.document_prefix,
                    truncate_chars(texts[i], config.max_chars)
                )
            })
            .collect();

        let vectors = self.model.embed(sorted, config.batch_size)?;

        let mut out: Vec<Vec<i8>> = vec![Vec::new(); texts.len()];
        for (&original, mut v) in order.iter().zip(vectors) {
            check_dims(spec, &v)?;
            normalize(&mut v);
            out[original] = quantize(&v);
        }
        Ok(out)
    }
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
