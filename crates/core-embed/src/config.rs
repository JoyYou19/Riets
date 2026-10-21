//! Embedding settings, as read from `CorelamoSettings.toml` by the server.

use std::io;
use std::path::PathBuf;

use crate::device::DevicePreference;
use crate::model::{find_model, known_model_ids};
use crate::runtime::RUNTIME_LIBRARY_NAME;

/// Settings shared by every loaded model.
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
    pub runtime_library: PathBuf,
    pub install_runtime: bool,
    /// Where models with in-graph pooling run. fastembed models always run on the CPU.
    pub device: DevicePreference,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            model_id: "bge-small-en-v1.5".to_string(),
            instances: 1,
            batch_size: 128,
            max_tokens: 256,
            max_chars: 2000,
            runtime_library: PathBuf::from(RUNTIME_LIBRARY_NAME),
            install_runtime: true,
            device: DevicePreference::Auto,
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
            return invalid(format!(
                "embedding max tokens must be 16..=512, got {}",
                self.max_tokens
            ));
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