use std::fmt;
use std::io;

mod config;
mod device;
mod embedder;
mod encoder;
mod model;
mod registry;

pub use config::ModelConfig;
pub use device::{Device, DevicePreference};
pub use embedder::{EmbedLease, Embedder};
pub use model::{MODELS, ModelSpec, find_model};
pub use registry::ModelRegistry;

/// int8 value = round(component * `QUANT_SCALE`). Components are within [-1, 1] after normalizing.
pub const QUANT_SCALE: f32 = 127.0;

/// Errors from downloading, loading or running a model.
pub(crate) fn model_error(e: impl fmt::Display) -> io::Error {
    io::Error::other(format!("embedding model: {e}"))
}