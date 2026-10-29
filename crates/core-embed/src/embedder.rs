//! Turning texts into vectors: model prefixes, character truncation, length-sorted batching
//! and L2 normalization, on top of loaded model copies.

use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};

use crate::config::ModelConfig;
use crate::device::Device;
use crate::model::{LoadedModel, ModelSpec, load_model};

/// One embedding model, shared through `Arc` by every database on the server.
///
/// Bulk document embedding goes through `lease()`, one holder per model copy at a time.
/// Queries prefer a separate copy, so a search normally never waits behind a bulk round; when
/// that copy is busy they borrow any free bulk copy, and wait only when every copy is busy.
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
    /// Query-only copy, loaded on the first query.
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
        let vector_id = vector_id(spec, config);
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
    /// Never fails today; the `Result` is kept so callers need not change if loading copies
    /// on demand is added later.
    pub fn try_lease(&self) -> io::Result<Option<EmbedLease<'_>>> {
        Ok(self
            .bulk
            .iter()
            .find_map(try_lock_model)
            .map(|model| EmbedLease { owner: self, model }))
    }

    /// A free bulk model copy if there is one, otherwise waits for the first.
    pub fn lease(&self) -> io::Result<EmbedLease<'_>> {
        if let Some(lease) = self.try_lease()? {
            return Ok(lease);
        }
        let first = self
            .bulk
            .first()
            .ok_or_else(|| io::Error::other("embedding model has no instances"))?;
        Ok(EmbedLease {
            owner: self,
            model: lock_model(first),
        })
    }

    /// Embeds documents on any bulk copy; one normalized f32 vector per input, in input order.
    pub fn embed_documents(&self, texts: &[&str]) -> io::Result<Vec<Vec<f32>>> {
        self.lease()?.embed_documents(texts)
    }

    /// Embeds a search query as a normalized f32 vector.
    ///
    /// Uses the query copy when it is free, otherwise any free bulk copy, and waits only when
    /// every copy is busy. Queries from all databases share these copies, so this must never
    /// hold one copy while waiting for another.
    pub fn embed_query(&self, text: &str) -> io::Result<Vec<f32>> {
        let text = text.trim();
        if text.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "query is empty"));
        }
        let prefixed = format!(
            "{}{}",
            self.spec.query_prefix,
            truncate_chars(text, self.config.max_chars)
        );

        let query_copy = self.query_copy();
        if let Ok(slot) = query_copy
            && let Some(mut model) = try_lock_model(slot)
        {
            return self.run_query(&mut model, prefixed);
        }
        if let Some(mut lease) = self.try_lease()? {
            return self.run_query(&mut lease.model, prefixed);
        }
        // Every copy is busy. Wait for the query copy: its holders run single queries, so it
        // frees up sooner than a bulk copy in the middle of a batch.
        match query_copy {
            Ok(slot) => self.run_query(&mut lock_model(slot), prefixed),
            Err(_) => self.run_query(&mut self.lease()?.model, prefixed),
        }
    }

    /// The query-only copy, loaded on first use. Two concurrent first queries may both load;
    /// the loser's copy is dropped. A failed load is retried on the next query.
    fn query_copy(&self) -> io::Result<&Mutex<LoadedModel>> {
        if let Some(slot) = self.queries.get() {
            return Ok(slot);
        }
        let model = load_model(self.spec, self.cache_dir.clone(), &self.config)?;
        let _ = self.queries.set(Mutex::new(model));
        Ok(self.queries.get().expect("set above"))
    }

    fn run_query(&self, model: &mut LoadedModel, prefixed: String) -> io::Result<Vec<f32>> {
        let mut vector = model
            .embed(vec![prefixed], self.config.batch_size)?
            .pop()
            .ok_or_else(|| io::Error::other("model returned no vector for the query"))?;
        check_dims(self.spec, &vector)?;
        normalize(&mut vector);
        Ok(vector)
    }
}

impl EmbedLease<'_> {
    pub fn spec(&self) -> &'static ModelSpec {
        self.owner.spec
    }

    pub fn vector_id(&self) -> &str {
        &self.owner.vector_id
    }

    /// Embeds documents, returning one L2-normalized f32 vector per input, in input order.
    /// Empty or whitespace-only inputs are rejected so callers never store zero vectors.
    pub fn embed_documents(&mut self, texts: &[&str]) -> io::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(position) = texts.iter().position(|t| t.trim().is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("text at position {position} is empty"),
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
        if vectors.len() != texts.len() {
            return Err(io::Error::other(format!(
                "model returned {} vectors for {} texts",
                vectors.len(),
                texts.len()
            )));
        }

        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        for (&original, mut vector) in order.iter().zip(vectors) {
            check_dims(spec, &vector)?;
            normalize(&mut vector);
            out[original] = vector;
        }
        Ok(out)
    }
}

/// Locks a model copy, recovering it if a panic poisoned the lock.
///
/// A panic during inference does not leave the copy half-updated: each run is independent and
/// the session keeps no state between runs. So the copy is safe to keep using, and recovering
/// avoids failing every later search until a restart.
fn lock_model(slot: &Mutex<LoadedModel>) -> MutexGuard<'_, LoadedModel> {
    slot.lock().unwrap_or_else(|poisoned| {
        slot.clear_poison();
        poisoned.into_inner()
    })
}

/// `lock_model` without waiting: `None` when another holder has the copy.
fn try_lock_model(slot: &Mutex<LoadedModel>) -> Option<MutexGuard<'_, LoadedModel>> {
    match slot.try_lock() {
        Ok(model) => Some(model),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Poisoned(poisoned)) => {
            slot.clear_poison();
            Some(poisoned.into_inner())
        }
    }
}

/// Identifies vectors made by `spec` with `config`; stored in every vector file. Both backends
/// give the same vectors for the same model and settings (the pooled one pools inside ONNX
/// Runtime instead of in fastembed), so neither the backend nor the device is part of the id:
/// switching between them never forces a re-embed.
pub(crate) fn vector_id(spec: &ModelSpec, config: &ModelConfig) -> String {
    format!("{}/t{}", spec.id, config.max_tokens)
}

fn check_dims(spec: &ModelSpec, vector: &[f32]) -> io::Result<()> {
    if vector.len() != spec.dims {
        return Err(io::Error::other(format!(
            "model '{}' returned {} dims, expected {}",
            spec.id,
            vector.len(),
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

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|x| *x /= norm);
    }
}