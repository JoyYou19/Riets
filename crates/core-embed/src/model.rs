//! The supported embedding models, and how each one is downloaded and loaded.
//!
//! Every model runs on the candle backend in `encoder.rs`, so only models whose architecture
//! candle implements are listed. Files come from each model's Hugging Face repository at a
//! pinned commit, so a new upload never changes the vectors without notice.

use std::io;
use std::path::{Path, PathBuf};

use hf_hub::api::sync::ApiBuilder;
use hf_hub::{Repo, RepoType};

use crate::config::ModelConfig;
use crate::encoder::{Architecture, ModelFiles, Pooling};
use crate::model_error;

pub(crate) use crate::encoder::LoadedModel;

/// A supported embedding model. `id` is what settings and vector file headers store.
pub struct ModelSpec {
    pub id: &'static str,
    pub dims: usize,
    /// Hugging Face repository the files come from, and the commit they are pinned to.
    repo: &'static str,
    revision: &'static str,
    /// The weights file: `.safetensors`, or `pytorch_model.bin` where there is none.
    weights: &'static str,
    architecture: Architecture,
    pooling: Pooling,
    pub(crate) query_prefix: &'static str,
    pub(crate) document_prefix: &'static str,
}

/// Loads one copy of `spec`'s model, downloading its files on first use.
pub(crate) fn load_model(
    spec: &'static ModelSpec,
    cache_dir: PathBuf,
    config: &ModelConfig,
) -> io::Result<LoadedModel> {
    let files = fetch_model_files(spec, &cache_dir)?;
    LoadedModel::load(
        &files,
        spec.architecture,
        spec.pooling,
        spec.dims,
        config.max_tokens,
        config.device,
    )
}

/// Downloads (or finds in the cache) the model's files. `HF_HOME` and `HF_ENDPOINT` override
/// the cache folder and the server, as in other Hugging Face tools.
fn fetch_model_files(spec: &ModelSpec, cache_dir: &Path) -> io::Result<ModelFiles> {
    let cache_dir =
        std::env::var_os("HF_HOME").map_or_else(|| cache_dir.to_path_buf(), PathBuf::from);
    let endpoint =
        std::env::var("HF_ENDPOINT").unwrap_or_else(|_| "https://huggingface.co".to_owned());
    let repo = ApiBuilder::new()
        .with_cache_dir(cache_dir)
        .with_endpoint(endpoint)
        .with_progress(false)
        .build()
        .map_err(model_error)?
        .repo(Repo::with_revision(
            spec.repo.to_owned(),
            RepoType::Model,
            spec.revision.to_owned(),
        ));
    let get = |file: &str| {
        repo.get(file)
            .map_err(|e| model_error(format!("{}/{file}: {e}", spec.repo)))
    };
    Ok(ModelFiles {
        weights: get(spec.weights)?,
        tokenizer: get("tokenizer.json")?,
        config: get("config.json")?,
        tokenizer_config: get("tokenizer_config.json")?,
    })
}

pub static MODELS: &[ModelSpec] = &[
    // Best for multilingual text: 100+ languages, up to 8192 tokens. The repository has no
    // safetensors file, so its PyTorch weights are read.
    ModelSpec {
        id: "bge-m3",
        dims: 1024,
        repo: "BAAI/bge-m3",
        revision: "5617a9f61b028005a4858fdac845db406aefb181",
        weights: "pytorch_model.bin",
        architecture: Architecture::XlmRoberta,
        pooling: Pooling::Cls,
        query_prefix: "",
        document_prefix: "",
    },
    // Strong English model, 512 tokens.
    ModelSpec {
        id: "mxbai-embed-large-v1",
        dims: 1024,
        repo: "mixedbread-ai/mxbai-embed-large-v1",
        revision: "b33106f585b9ce46904ad7443a3b52b7a63e231c",
        weights: "model.safetensors",
        architecture: Architecture::Bert,
        pooling: Pooling::Cls,
        query_prefix: "Represent this sentence for searching relevant passages: ",
        document_prefix: "",
    },
    // Multilingual, 512 tokens.
    ModelSpec {
        id: "multilingual-e5-large",
        dims: 1024,
        repo: "intfloat/multilingual-e5-large",
        revision: "3d7cfbdacd47fdda877c5cd8a79fbcc4f2a574f3",
        weights: "model.safetensors",
        architecture: Architecture::XlmRoberta,
        pooling: Pooling::Mean,
        query_prefix: "query: ",
        document_prefix: "passage: ",
    },
    ModelSpec {
        id: "multilingual-e5-base",
        dims: 768,
        repo: "intfloat/multilingual-e5-base",
        revision: "d128750597153bb5987e10b1c3493a34e5a4502a",
        weights: "model.safetensors",
        architecture: Architecture::XlmRoberta,
        pooling: Pooling::Mean,
        query_prefix: "query: ",
        document_prefix: "passage: ",
    },
];

#[must_use]
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