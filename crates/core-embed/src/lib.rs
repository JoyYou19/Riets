//! Text embedding for corelamo vector search.
//!
//! Models are loaded once per process through `ModelRegistry` and shared by every database.
//! Documents are embedded as L2-normalized int8 vectors (scale `QUANT_SCALE`); queries as
//! L2-normalized f32 vectors, scored against int8 rows as `dot(query, row) / QUANT_SCALE`.
//!
//! Where things are:
//! - `config.rs`: `ModelConfig`, the embedding settings from `CorelamoSettings.toml`.
//! - `registry.rs`: `ModelRegistry`, the server-wide set of loaded models.
//! - `embedder.rs`: `Embedder` and `EmbedLease`, which turn texts into vectors: prefixes,
//!   truncation, batching order, normalization and int8 quantization.
//! - `model.rs`: the supported models, and how each one is downloaded, loaded and run.
//! - `pooled.rs`: the backend that adds pooling to the stock model inside ONNX Runtime, so
//!   it runs entirely on CUDA when there is a GPU (`pooled/graph.rs` builds the pooling
//!   nodes, `pooled/opset.rs` reads the opset version from the model file).
//! - `device.rs`: where models run (`embedding-device`) and NVIDIA driver detection.
//! - `runtime.rs`: finding, downloading and loading the ONNX Runtime library.

use std::fmt;
use std::io;

mod config;
mod device;
mod embedder;
mod model;
mod pooled;
mod registry;
mod runtime;

pub use config::ModelConfig;
pub use device::{Device, DevicePreference};
pub use embedder::{EmbedLease, Embedder};
pub use model::{MODELS, ModelSpec, find_model};
pub use registry::ModelRegistry;
pub use runtime::{RUNTIME_LIBRARY_NAME, RUNTIME_RELEASE, default_runtime_library};

/// int8 value = round(component * `QUANT_SCALE`). Components are within [-1, 1] after normalizing.
pub const QUANT_SCALE: f32 = 127.0;

/// Errors from downloading, loading or running a model.
pub(crate) fn model_error(e: impl fmt::Display) -> io::Error {
    io::Error::other(format!("embedding model: {e}"))
}
