//! The server-wide set of loaded models.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{ Path, PathBuf };
use std::sync::{ Arc, Mutex };

use crate::config::ModelConfig;
use crate::embedder::Embedder;
use crate::model::{ ModelSpec, find_model, known_model_ids };

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
        let default_model = find_model(&config.model_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown model id `{}`", config.model_id)
            )
        })?;
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
                format!("unknown embedding model '{model_id}' (known: {})", known_model_ids())
            )
        })?;
        // Held during loading so concurrent first requests load the model only once.
        let mut loaded = self.loaded
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
