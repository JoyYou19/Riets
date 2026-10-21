//! The supported embedding models, and how each one is downloaded, loaded and run.
//!
//! Two backends: fastembed (CPU only; the model runs and fastembed pools its per-token output
//! on the CPU), and the pooled backend in `pooled.rs` (the same stock model with pooling added
//! inside ONNX Runtime, so it runs entirely on CUDA when available, otherwise on the CPU).

use std::io;
use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use hf_hub::api::sync::ApiBuilder;

use crate::config::ModelConfig;
use crate::device::Device;
use crate::model_error;
use crate::pooled::{ModelFiles, PooledModel};
use crate::runtime::ensure_runtime;

/// How a model is loaded and run.
#[derive(Clone, Copy)]
enum Backend {
    /// fastembed downloads the model; per-token output is pooled on the CPU. CPU only.
    FastEmbed(fn() -> EmbeddingModel),
    /// The same stock model file fastembed would download, extended at load time with
    /// pooling and normalization inside ONNX Runtime. Runs on CUDA when available, otherwise
    /// on the CPU, with the same vectors either way.
    Pooled(fn() -> EmbeddingModel),
}

/// A supported embedding model. `id` is what settings and vector file headers store.
pub struct ModelSpec {
    pub id: &'static str,
    pub dims: usize,
    backend: Backend,
    pub(crate) query_prefix: &'static str,
    pub(crate) document_prefix: &'static str,
}

/// One loaded model copy.
pub(crate) enum LoadedModel {
    FastEmbed(TextEmbedding),
    Pooled(PooledModel),
}

impl LoadedModel {
    pub(crate) fn embed(
        &mut self,
        texts: Vec<String>,
        batch_size: usize,
    ) -> io::Result<Vec<Vec<f32>>> {
        match self {
            LoadedModel::FastEmbed(model) => run_model(model, texts, batch_size),
            LoadedModel::Pooled(model) => {
                let count = texts.len();
                let vectors = model.embed(&texts, batch_size)?;
                if vectors.len() != count {
                    return Err(io::Error::other(format!(
                        "model returned {} vectors for {count} texts",
                        vectors.len()
                    )));
                }
                Ok(vectors)
            }
        }
    }

    pub(crate) fn device(&self) -> Device {
        match self {
            LoadedModel::FastEmbed(_) => Device::Cpu,
            LoadedModel::Pooled(model) => model.device(),
        }
    }

    pub(crate) fn cpu_fallback(&self) -> Option<&str> {
        match self {
            LoadedModel::FastEmbed(_) => None,
            LoadedModel::Pooled(model) => model.cpu_fallback(),
        }
    }
}

/// Loads one copy of `spec`'s model, downloading its files on first use.
pub(crate) fn load_model(
    spec: &'static ModelSpec,
    cache_dir: PathBuf,
    config: &ModelConfig,
) -> io::Result<LoadedModel> {
    ensure_runtime(&config.runtime_library, config.install_runtime)?;
    match spec.backend {
        Backend::FastEmbed(model) => {
            let options = InitOptions::new(model())
                .with_max_length(config.max_tokens)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(false);
            TextEmbedding::try_new(options)
                .map(LoadedModel::FastEmbed)
                .map_err(model_error)
        }
        Backend::Pooled(model) => PooledModel::load(
            &fetch_model_files(&model(), &cache_dir)?,
            spec.dims,
            config.max_tokens,
            config.device,
            cpu_threads_per_instance(config.instances),
        )
        .map(LoadedModel::Pooled),
    }
}

/// Downloads (or finds in the cache) the files fastembed would use for `model`. Uses the
/// same cache folder and environment overrides as fastembed, so either one reuses the
/// other's downloads.
fn fetch_model_files(model: &EmbeddingModel, cache_dir: &Path) -> io::Result<ModelFiles> {
    let info = TextEmbedding::get_model_info(model).map_err(model_error)?;
    let cache_dir =
        std::env::var_os("HF_HOME").map_or_else(|| cache_dir.to_path_buf(), PathBuf::from);
    let endpoint =
        std::env::var("HF_ENDPOINT").unwrap_or_else(|_| "https://huggingface.co".to_string());
    let repo = ApiBuilder::new()
        .with_cache_dir(cache_dir)
        .with_endpoint(endpoint)
        .with_progress(false)
        .build()
        .map_err(model_error)?
        .model(info.model_code.clone());
    let get = |file: &str| {
        repo.get(file)
            .map_err(|e| model_error(format!("{}/{file}: {e}", info.model_code)))
    };
    Ok(ModelFiles {
        model: get(&info.model_file)?,
        tokenizer: get("tokenizer.json")?,
        config: get("config.json")?,
        tokenizer_config: get("tokenizer_config.json")?,
    })
}

/// CPU threads for one model copy when it runs on the CPU: the cores split between copies.
fn cpu_threads_per_instance(instances: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    (cores / instances.max(1)).max(1)
}

fn run_model(
    model: &mut TextEmbedding,
    texts: Vec<String>,
    batch_size: usize,
) -> io::Result<Vec<Vec<f32>>> {
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
fn multilingual_e5_small() -> EmbeddingModel {
    EmbeddingModel::MultilingualE5Small
}

fn bge_small_en() -> EmbeddingModel {
    EmbeddingModel::BGESmallENV15Q // EmbeddingModel::BGESmallENV15 
}

pub static MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "bge-small-en-v1.5",
        dims: 384,
        backend: Backend::FastEmbed(bge_small_en),
        query_prefix: "Represent this sentence for searching relevant passages: ",
        document_prefix: "",
    },
    ModelSpec {
        id: "multilingual-e5-small",
        dims: 384,
        backend: Backend::Pooled(multilingual_e5_small),
        query_prefix: "query: ",
        document_prefix: "passage: ",
    },
];

pub fn find_model(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|spec| spec.id == id)
}

pub(crate) fn known_model_ids() -> String {
    MODELS
        .iter()
        .map(|spec| spec.id)
        .collect::<Vec<_>>()
        .join(", ")
}