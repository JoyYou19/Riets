//! The server-wide set of loaded models.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{ Path, PathBuf };
use std::sync::Condvar;
use std::sync::{ Arc, Mutex };
use std::time::Duration;
use std::time::Instant;



use crate::config::ModelConfig;

use crate::embedder::{ Embedder, vector_id };
use crate::model::{ ModelSpec, find_model, known_model_ids };

enum ModelState {
    Loading,
    Ready(Arc<Embedder>),
    Failed {
        error: String,
        at: Instant,
    },
}

const RETRY_AFTER: Duration = Duration::from_secs(60);
const LOAD_WAIT: Duration = Duration::from_secs(30);
pub struct ModelRegistry {
    cache_dir: PathBuf,
    default_model: &'static ModelSpec,
    config: ModelConfig,
    model_status: Mutex<HashMap<&'static str, ModelState>>,
    loaded_changed: Condvar,

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
            model_status: Mutex::new(HashMap::new()),
            loaded_changed: Condvar::new(),
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

       let mut status = self
    .model_status
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match status.get(spec.id) {
                Some(ModelState::Ready(embedder)) => {
                    return Ok(Arc::clone(embedder));
                }
                Some(ModelState::Loading) => {
                    // releases the lock while asleep, takes it back when woken
                    let (guard, wait) = self.loaded_changed
                        .wait_timeout(status, LOAD_WAIT)
                        .map_err(|_| io::Error::other("model registry lock poisoned"))?;
                    status = guard;
                    if wait.timed_out() && matches!(status.get(spec.id), Some(ModelState::Loading)) {
                        return Err(
                            io::Error::other(
                                format!("embedding model '{}' is still loading", spec.id)
                            )
                        );
                    }
                }
                Some(ModelState::Failed { error, at }) if at.elapsed() < RETRY_AFTER => {
                    return Err(
                        io::Error::other(
                            format!("loading embedding model '{}' failed: {error}", spec.id)
                        )
                    );
                }
                _ => {
                    status.insert(spec.id, ModelState::Loading);
                    break;
                }
            }
        }
        drop(status);
        self.load_and_record(spec)
    }
    fn load_and_record(&self, spec: &'static ModelSpec) -> io::Result<Arc<Embedder>> {
        let result = std::panic
            ::catch_unwind(
                std::panic::AssertUnwindSafe(|| {
                    Embedder::load(spec, self.cache_dir.clone(), &self.config)
                })
            )
            .unwrap_or_else(|_| {
                Err(io::Error::other(format!("loading embedding model '{}' panicked", spec.id)))
            });
        let mut status = self.model_status
            .lock()
            .map_err(|_| io::Error::other("model registry lock poisoned"))?;
        let outcome = match result {
            Ok(embedder) => {
                let embedder = Arc::new(embedder);
                status.insert(spec.id, ModelState::Ready(Arc::clone(&embedder)));
                Ok(embedder)
            }
            Err(e) => {
                status.insert(spec.id, ModelState::Failed {
                    error: e.to_string(),
                    at: Instant::now(),
                });
                Err(e)
            }
        };
        drop(status);
        self.loaded_changed.notify_all();
        outcome
    }
    pub fn try_get(self: &Arc<Self>, model_id: &str) -> io::Result<Option<Arc<Embedder>>> {
        let spec = find_model(model_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown embedding model '{model_id}' (known: {})", known_model_ids())
            )
        })?;

        let mut status = self.model_status
            .lock()
            .map_err(|_| io::Error::other("model registry lock poisoned"))?;
        match status.get(spec.id) {
            Some(ModelState::Ready(embedder)) => {
                return Ok(Some(Arc::clone(embedder)));
            }
            Some(ModelState::Loading) => {
                return Ok(None);
            }
            Some(ModelState::Failed { error, at }) if at.elapsed() < RETRY_AFTER => {
                return Err(
                    io::Error::other(
                        format!("loading embedding model '{}' failed: {error}", spec.id)
                    )
                );
            }
            _ => {
                status.insert(spec.id, ModelState::Loading);
            }
        }
        drop(status);
        let registry = Arc::clone(self);
        let started = std::thread::Builder
            ::new()
            .name("model-load".into())
            .spawn(move || {
                // the outcome is recorded in the map; callers read it from there
                let _ = registry.load_and_record(spec);
            });
        if let Err(e) = started {
            // nobody will load it: record that, so callers don't wait for a load that
            // never started
            let message = format!("starting the model load thread: {e}");
            let mut status = self.model_status
                .lock()
                .map_err(|_| io::Error::other("model registry lock poisoned"))?;
            status.insert(spec.id, ModelState::Failed {
                error: message.clone(),
                at: Instant::now(),
            });
            drop(status);
            self.loaded_changed.notify_all();
            return Err(io::Error::other(message));
        }
        Ok(None)
    }
    pub fn try_get_default(self: &Arc<Self>) -> io::Result<Option<Arc<Embedder>>> {
        self.try_get(self.default_model.id)
    }
    /// The id the default model's vectors carry (see `Embedder::vector_id`), known without
    /// loading the model: progress can be measured while the model is unavailable.
    pub fn default_vector_id(&self) -> String {
        vector_id(self.default_model, &self.config)
    }

    pub fn default_embedder(&self) -> io::Result<Arc<Embedder>> {
        self.get(self.default_model.id)
    }
    /// Ids of the models loaded so far, sorted.
    pub fn loaded_model_ids(&self) -> Vec<&'static str> {
        let loaded = self.model_status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ids: Vec<&'static str> = loaded
            .iter()
            .filter(|(_, state)| matches!(state, ModelState::Ready(_)))
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }
}
