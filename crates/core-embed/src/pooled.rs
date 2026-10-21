//! Embedding models whose pooling runs inside ONNX Runtime, on the GPU when there is one.
//!
//! The model file is the stock Hugging Face export that fastembed uses too. At load time its
//! graph is extended in memory, through ONNX Runtime's model editor, with masked sum pooling
//! and L2 normalization after the transformer's per-token output. Nothing is exported or
//! prepared by hand. On CUDA a batch crosses the `PCIe` bus twice: token ids in, finished
//! `[batch, dims]` vectors out. ONNX Runtime keeps only shape bookkeeping (a few integers per
//! run) on the CPU. Without CUDA the same extended graph runs on the CPU.

mod graph;
mod opset;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ort::ep::CUDA;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::{OutputSelector, RunOptions, Session};
use ort::value::{Tensor, ValueType};
use tokenizers::{Tokenizer, TruncationParams};

use crate::device::{Device, DevicePreference, nvidia_driver_present};
use crate::model_error;

const INPUT_IDS: &str = "input_ids";
const ATTENTION_MASK: &str = "attention_mask";
const TOKEN_TYPE_IDS: &str = "token_type_ids";
/// The output the pooling nodes add.
const OUTPUT: &str = "sentence_embedding";

/// The files of a Hugging Face model repository that a pooled model is built from.
pub(crate) struct ModelFiles {
    pub model: PathBuf,
    pub tokenizer: PathBuf,
    pub config: PathBuf,
    pub tokenizer_config: PathBuf,
}

pub(crate) struct PooledModel {
    session: Session,
    /// Whether the model takes `token_type_ids` (BERT and XLM-R exports do; all zeros here).
    token_type_ids: bool,
    tokenizer: Tokenizer,
    pad_id: i64,
    dims: usize,
    device: Device,
    /// Why `Auto` ended up on the CPU on a machine that has an NVIDIA driver.
    cpu_fallback: Option<String>,
}

impl PooledModel {
    /// Builds the extended model on the preferred device. `cpu_threads` is only used when the
    /// model runs on the CPU.
    pub(crate) fn load(
        files: &ModelFiles,
        dims: usize,
        max_tokens: usize,
        preference: DevicePreference,
        cpu_threads: usize,
    ) -> io::Result<Self> {
        let (tokenizer, pad_id) = load_tokenizer(files, max_tokens)?;
        let opset = opset::onnx_opset(&files.model)
            .map_err(|e| model_error(format!("{}: {e}", files.model.display())))?;

        let on_cpu = || {
            Pipeline::build(
                &files.model,
                opset,
                dims,
                Target::Cpu {
                    threads: cpu_threads,
                },
            )
            .map_err(|e| model_error(format!("{} on the CPU: {e}", files.model.display())))
        };
        let on_cuda = |device_id| {
            Pipeline::build(&files.model, opset, dims, Target::Cuda { device_id })
                .map_err(|e| e.to_string())
        };

        let (pipeline, device, cpu_fallback) = match preference {
            DevicePreference::Cpu => (on_cpu()?, Device::Cpu, None),
            DevicePreference::Cuda { device_id } => {
                let pipeline = on_cuda(device_id).map_err(|e| {
                    model_error(format!(
                        "embedding-device requires CUDA, but it could not start: {e}"
                    ))
                })?;
                (pipeline, Device::Cuda { device_id }, None)
            }
            DevicePreference::Auto if !nvidia_driver_present() => (on_cpu()?, Device::Cpu, None),
            DevicePreference::Auto => match on_cuda(0) {
                Ok(pipeline) => (pipeline, Device::Cuda { device_id: 0 }, None),
                Err(cuda_error) => (on_cpu()?, Device::Cpu, Some(cuda_error)),
            },
        };

        Ok(Self {
            session: pipeline.session,
            token_type_ids: pipeline.token_type_ids,
            tokenizer,
            pad_id,
            dims,
            device,
            cpu_fallback,
        })
    }

    pub(crate) fn device(&self) -> Device {
        self.device
    }

    pub(crate) fn cpu_fallback(&self) -> Option<&str> {
        self.cpu_fallback.as_deref()
    }

    /// Embeds `texts` in order, `batch_size` per model run. Vectors come back L2-normalized.
    pub(crate) fn embed(&mut self, texts: &[String], batch_size: usize) -> io::Result<Vec<Vec<f32>>> {
        let mut vectors = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(batch_size.max(1)) {
            self.embed_batch(chunk, &mut vectors)?;
        }
        Ok(vectors)
    }

    fn embed_batch(&mut self, texts: &[String], out: &mut Vec<Vec<f32>>) -> io::Result<()> {
        let inputs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let encodings = self
            .tokenizer
            .encode_batch(inputs, true)
            .map_err(model_error)?;

        let batch = encodings.len();
        let seq = encodings
            .iter()
            .map(|encoding| encoding.get_ids().len())
            .max()
            .unwrap_or(0)
            .max(1);
        let mut ids = vec![self.pad_id; batch * seq];
        let mut mask = vec![0_i64; batch * seq];
        for (row, encoding) in encodings.iter().enumerate() {
            let start = row * seq;
            let tokens = encoding.get_ids().iter().zip(encoding.get_attention_mask());
            for (col, (&id, &attend)) in tokens.enumerate() {
                ids[start + col] = i64::from(id);
                mask[start + col] = i64::from(attend);
            }
        }

        let ids = Tensor::from_array(([batch, seq], ids)).map_err(model_error)?;
        let mask = Tensor::from_array(([batch, seq], mask)).map_err(model_error)?;
        // Ask for the added output by name: ort's session caches the output list from before
        // the edit, which still names the per-token output.
        let run_options = RunOptions::new()
            .map_err(model_error)?
            .with_outputs(OutputSelector::no_default().with(OUTPUT));
        let dims = self.dims;
        let outputs = if self.token_type_ids {
            let types = Tensor::from_array(([batch, seq], vec![0_i64; batch * seq]))
                .map_err(model_error)?;
            self.session.run_with_options(
                ort::inputs![INPUT_IDS => ids, ATTENTION_MASK => mask, TOKEN_TYPE_IDS => types],
                &run_options,
            )
        } else {
            self.session.run_with_options(
                ort::inputs![INPUT_IDS => ids, ATTENTION_MASK => mask],
                &run_options,
            )
        }
        .map_err(model_error)?;

        let (shape, data) = outputs[OUTPUT]
            .try_extract_tensor::<f32>()
            .map_err(model_error)?;
        if !shape.iter().copied().eq([onnx_dim(batch), onnx_dim(dims)]) {
            return Err(model_error(format!(
                "model returned shape {:?}, expected [{batch}, {dims}]",
                &shape[..]
            )));
        }
        out.extend(data.chunks_exact(dims).map(<[f32]>::to_vec));
        Ok(())
    }
}
#[derive(Clone, Copy)]
enum Target {
    Cpu { threads: usize },
    Cuda { device_id: i32 },
}

/// A session over the extended graph.
struct Pipeline {
    session: Session,
    token_type_ids: bool,
}

impl Pipeline {
    fn build(model_path: &Path, opset: u32, dims: usize, target: Target) -> ort::Result<Self> {
        let builder =
            Session::builder()?.with_optimization_level(GraphOptimizationLevel::Level3)?;
        let mut builder =
            match target {
                Target::Cpu { threads } => builder.with_intra_threads(threads.max(1))?,
                // The GPU does the math; the CPU only runs shape bookkeeping between kernels.
                Target::Cuda { device_id } => builder
                    .with_intra_threads(1)?
                    .with_execution_providers([CUDA::default()
                        .with_device_id(device_id)
                        .build()
                        // Report a CUDA that cannot start instead of silently running on the CPU.
                        .error_on_failure()])?,
            };

        let mut editable = builder.edit_from_file(model_path)?;
        let inputs: Vec<(String, ValueType)> = editable
            .inputs()
            .iter()
            .map(|input| (input.name().to_owned(), input.dtype().clone()))
            .collect();
        for required in [INPUT_IDS, ATTENTION_MASK] {
            if !inputs.iter().any(|(name, _)| name == required) {
                return Err(ort::Error::new(format!(
                    "the model has no `{required}` input"
                )));
            }
        }
        let token_type_ids = inputs.iter().any(|(name, _)| name == TOKEN_TYPE_IDS);
        let hidden_state = graph::hidden_state_output(editable.outputs(), dims)?;

        editable.apply_model(&graph::pooling_model(inputs, &hidden_state, opset, dims)?)?;
        Ok(Self {
            session: editable.into_session()?,
            token_type_ids,
        })
    }
}

/// The repository's tokenizer, truncating like fastembed does, and the padding token id.
fn load_tokenizer(files: &ModelFiles, max_tokens: usize) -> io::Result<(Tokenizer, i64)> {
    let config = read_json(&files.config)?;
    let tokenizer_config = read_json(&files.tokenizer_config)?;
    let pad_id = config["pad_token_id"]
        .as_i64()
        .ok_or_else(|| model_error(format!("{} has no pad_token_id", files.config.display())))?;
    // Some repositories store a huge placeholder here; `as` saturates it to usize::MAX.
     let model_max = tokenizer_config["model_max_length"]
        .as_u64()
        .filter(|&max| max > 0)
        .and_then(|max| usize::try_from(max).ok())
        .unwrap_or(max_tokens);
    let mut tokenizer = Tokenizer::from_file(&files.tokenizer)
        .map_err(|e| model_error(format!("{}: {e}", files.tokenizer.display())))?;
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: max_tokens.min(model_max),
            ..Default::default()
        }))
        .map_err(model_error)?;
    // Batches are padded here, to the longest text of each batch.
    tokenizer.with_padding(None);
    Ok((tokenizer, pad_id))
}

fn read_json(path: &Path) -> io::Result<serde_json::Value> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| model_error(format!("{}: {e}", path.display())))
}
/// Converts a length or dimension into an ONNX shape dimension.
///
/// Slice lengths are bounded by `isize::MAX` and model dimensions are small,
/// so failure means a corrupted value, not a recoverable condition.
fn onnx_dim(n: usize) -> i64 {
    i64::try_from(n).expect("dimension exceeds i64::MAX")
}