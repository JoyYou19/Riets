//! Embedding models whose pooling runs inside ONNX Runtime, on the GPU when there is one.
//!
//! The model file is the stock Hugging Face export that fastembed uses too. At load time its
//! graph is extended in memory, through ONNX Runtime's model editor, with masked sum pooling
//! and L2 normalization after the transformer's per-token output. Nothing is exported or
//! prepared by hand. On CUDA a batch crosses the `PCIe` bus twice: token ids in, finished
//! `[batch, dims]` vectors out. ONNX Runtime keeps only shape bookkeeping (a few integers per
//! run) on the CPU. Without CUDA the same extended graph runs on the CPU.
//!
//! GPU memory: a run's activations grow with `texts × tokens` and its attention scores with
//! `texts × tokens²`, so runs are cut by token length (`next_run`), not by a fixed number of
//! texts. On CUDA the memory a run took goes back to the device when the run ends, and a run
//! that still does not fit is split in two and retried with a smaller budget from then on.

mod graph;
mod opset;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ort::ep::{ArenaExtendStrategy, CUDA};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::{HasSelectedOutputs, OutputSelector, RunOptions, Session};
use ort::value::{Tensor, ValueType};
use tokenizers::{Encoding, Tokenizer, TruncationParams};

use crate::device::{Device, DevicePreference, nvidia_driver_present};
use crate::model_error;

const INPUT_IDS: &str = "input_ids";
const ATTENTION_MASK: &str = "attention_mask";
const TOKEN_TYPE_IDS: &str = "token_type_ids";
/// The output the pooling nodes add.
const OUTPUT: &str = "sentence_embedding";
/// ONNX Runtime run option: give the memory a run allocated back to the device after the run.
const ARENA_SHRINKAGE: &str = "memory.enable_memory_arena_shrinkage";

/// How the transformer's per-token output becomes one vector per text. Use what the model was
/// trained with; the other gives vectors that load fine but match badly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pooling {
    /// Average over the text's tokens (e5, ModernBERT-embed).
    Mean,
    /// The first token's output (`[CLS]`) alone (bge, Snowflake arctic-embed).
    Cls,
}

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
    /// The most tokens a text keeps after truncation.
    max_tokens: usize,
    device: Device,
    /// Why `Auto` ended up on the CPU on a machine that has an NVIDIA driver.
    cpu_fallback: Option<String>,
    /// Selects the pooled output; on CUDA also returns each run's memory to the device.
    run_options: RunOptions<HasSelectedOutputs>,
    /// How often a run did not fit in memory; each time halves the memory budget of later runs.
    halvings: u32,
}

impl PooledModel {
    /// Builds the extended model on the preferred device. `cpu_threads` is only used when the
    /// model runs on the CPU.
    pub(crate) fn load(
        files: &ModelFiles,
        dims: usize,
        pooling: Pooling,
        max_tokens: usize,
        preference: DevicePreference,
        cpu_threads: usize,
    ) -> io::Result<Self> {
        let (tokenizer, pad_id, max_tokens) = load_tokenizer(files, max_tokens)?;
        let opset = opset::onnx_opset(&files.model)
            .map_err(|e| model_error(format!("{}: {e}", files.model.display())))?;

        let on_cpu = || {
            Pipeline::build(
                &files.model,
                opset,
                dims,
                pooling,
                Target::Cpu {
                    threads: cpu_threads,
                },
            )
            .map_err(|e| model_error(format!("{} on the CPU: {e}", files.model.display())))
        };
        let on_cuda = |device_id| {
            Pipeline::build(&files.model, opset, dims, pooling, Target::Cuda { device_id })
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

        // Ask for the added output by name: ort's session caches the output list from before
        // the edit, which still names the per-token output.
        let mut run_options = RunOptions::new()
            .map_err(model_error)?
            .with_outputs(OutputSelector::no_default().with(OUTPUT));
        if let Device::Cuda { device_id } = device {
            // Batch shapes change with text length; a memory pool that keeps every shape's
            // buffers ends up holding the whole GPU. Each run's buffers go back after the run.
            run_options
                .set(ARENA_SHRINKAGE, format!("gpu:{device_id}"))
                .map_err(model_error)?;
        }

        Ok(Self {
            session: pipeline.session,
            token_type_ids: pipeline.token_type_ids,
            tokenizer,
            pad_id,
            dims,
            max_tokens,
            device,
            cpu_fallback,
            run_options,
            halvings: 0,
        })
    }

    pub(crate) fn device(&self) -> Device {
        self.device
    }

    pub(crate) fn cpu_fallback(&self) -> Option<&str> {
        self.cpu_fallback.as_deref()
    }

    /// Embeds `texts`, returning their vectors in input order, L2-normalized.
    ///
    /// `batch_size` is the number of texts of the full `max_tokens` length a run may hold; runs
    /// of shorter texts hold more of them at the same memory (see `next_run`).
    pub(crate) fn embed(
        &mut self,
        texts: &[String],
        batch_size: usize,
    ) -> io::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let inputs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let encodings = self
            .tokenizer
            .encode_batch(inputs, true)
            .map_err(model_error)?;

        // Shortest first, so each run pads its texts to a length close to their own.
        let mut order: Vec<usize> = (0..encodings.len()).collect();
        order.sort_by_key(|&row| encodings[row].get_ids().len());
        let lengths: Vec<usize> = order
            .iter()
            .map(|&row| encodings[row].get_ids().len())
            .collect();

        let mut vectors: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        let mut start = 0;
        while start < order.len() {
            // Planned one run at a time: a run that did not fit lowers the budget of the next.
            let count = next_run(&lengths[start..], batch_size, self.max_tokens, self.halvings);
            self.run_splitting(&order[start..start + count], &encodings, &mut vectors)?;
            start += count;
        }
        Ok(vectors)
    }

    /// Runs `rows` and stores their vectors. A run that does not fit in memory is split in
    /// two, and every later run is planned with half the memory.
    fn run_splitting(
        &mut self,
        rows: &[usize],
        encodings: &[Encoding],
        out: &mut [Vec<f32>],
    ) -> io::Result<()> {
        match self.run(rows, encodings) {
            Ok(vectors) => {
                for (&row, vector) in rows.iter().zip(vectors) {
                    out[row] = vector;
                }
                Ok(())
            }
            Err(error) if rows.len() > 1 && is_out_of_memory(&error) => {
                self.halvings = self.halvings.saturating_add(1);
                let (left, right) = rows.split_at(rows.len() / 2);
                self.run_splitting(left, encodings, out)?;
                self.run_splitting(right, encodings, out)
            }
            Err(error) if is_out_of_memory(&error) => Err(model_error(format!(
                "one text of {} tokens does not fit in the device's memory: {error}",
                encodings[rows[0]].get_ids().len()
            ))),
            Err(error) => Err(model_error(error)),
        }
    }

    /// One model run over `rows`, padded to the longest of them.
    fn run(&mut self, rows: &[usize], encodings: &[Encoding]) -> ort::Result<Vec<Vec<f32>>> {
        let batch = rows.len();
        let seq = rows
            .iter()
            .map(|&row| encodings[row].get_ids().len())
            .max()
            .unwrap_or(0)
            .max(1);
        let mut ids = vec![self.pad_id; batch * seq];
        let mut mask = vec![0_i64; batch * seq];
        for (slot, &row) in rows.iter().enumerate() {
            let start = slot * seq;
            let encoding = &encodings[row];
            let tokens = encoding.get_ids().iter().zip(encoding.get_attention_mask());
            for (col, (&id, &attend)) in tokens.enumerate() {
                ids[start + col] = i64::from(id);
                mask[start + col] = i64::from(attend);
            }
        }

        let ids = Tensor::from_array(([batch, seq], ids))?;
        let mask = Tensor::from_array(([batch, seq], mask))?;
        let outputs = if self.token_type_ids {
            let types = Tensor::from_array(([batch, seq], vec![0_i64; batch * seq]))?;
            self.session.run_with_options(
                ort::inputs![INPUT_IDS => ids, ATTENTION_MASK => mask, TOKEN_TYPE_IDS => types],
                &self.run_options,
            )?
        } else {
            self.session.run_with_options(
                ort::inputs![INPUT_IDS => ids, ATTENTION_MASK => mask],
                &self.run_options,
            )?
        };

        let dims = self.dims;
        let (shape, data) = outputs[OUTPUT].try_extract_tensor::<f32>()?;
        if !shape.iter().copied().eq([onnx_dim(batch), onnx_dim(dims)]) {
            return Err(ort::Error::new(format!(
                "model returned shape {:?}, expected [{batch}, {dims}]",
                &shape[..]
            )));
        }
        Ok(data.chunks_exact(dims).map(<[f32]>::to_vec).collect())
    }
}

/// How many of `lengths` (token counts, shortest first) the next run takes.
///
/// A run of `n` texts padded to `len` tokens needs memory for `n·len` token states and
/// `n·len²` attention scores. It may need as much as `batch_size` texts of `max_tokens` tokens,
/// halved `halvings` times, and never more: short texts run many at a time, long ones few. A
/// run always takes at least one text.
fn next_run(lengths: &[usize], batch_size: usize, max_tokens: usize, halvings: u32) -> usize {
    let batch = batch_size.max(1) as u128;
    let max = max_tokens.max(1) as u128;
    let shift = halvings.min(64);
    let token_budget = (batch * max).checked_shr(shift).unwrap_or(0);
    let score_budget = (batch * max * max).checked_shr(shift).unwrap_or(0);

    let mut count = 1;
    while count < lengths.len() {
        // shortest first: the text being added is the longest of the run
        let len = lengths[count].max(1) as u128;
        let texts = count as u128 + 1;
        if texts * len > token_budget || texts * len * len > score_budget {
            break;
        }
        count += 1;
    }
    count.min(lengths.len())
}

/// Whether ONNX Runtime failed because device memory ran out.
fn is_out_of_memory(error: &ort::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("failed to allocate")
        || message.contains("out of memory")
        || message.contains("alloc_failed")
        || message.contains("memoryallocation")
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
    fn build(
        model_path: &Path,
        opset: u32,
        dims: usize,
        pooling: Pooling,
        target: Target,
    ) -> ort::Result<Self> {
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
                        // Grow the device memory pool by what a run asks for, not by doubling.
                        .with_arena_extend_strategy(ArenaExtendStrategy::SameAsRequested)
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

        editable.apply_model(&graph::pooling_model(inputs, &hidden_state, opset, dims, pooling)?)?;
        Ok(Self {
            session: editable.into_session()?,
            token_type_ids,
        })
    }
}

/// The repository's tokenizer, truncating like fastembed does, its padding token id, and the
/// most tokens a text keeps.
fn load_tokenizer(files: &ModelFiles, max_tokens: usize) -> io::Result<(Tokenizer, i64, usize)> {
    let config = read_json(&files.config)?;
    let tokenizer_config = read_json(&files.tokenizer_config)?;
    let pad_id = config["pad_token_id"]
        .as_i64()
        .ok_or_else(|| model_error(format!("{} has no pad_token_id", files.config.display())))?;
    // Some repositories store a huge placeholder here; it is ignored when it does not fit.
    let model_max = tokenizer_config["model_max_length"]
        .as_u64()
        .filter(|&max| max > 0)
        .and_then(|max| usize::try_from(max).ok())
        .unwrap_or(max_tokens);
    let max_tokens = max_tokens.min(model_max);
    let mut tokenizer = Tokenizer::from_file(&files.tokenizer)
        .map_err(|e| model_error(format!("{}: {e}", files.tokenizer.display())))?;
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: max_tokens,
            ..Default::default()
        }))
        .map_err(model_error)?;
    // Runs are padded here, to the longest text of each run.
    tokenizer.with_padding(None);
    Ok((tokenizer, pad_id, max_tokens))
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

#[cfg(test)]
mod tests {
    use super::next_run;

    /// Every run of `lengths` (sorted), as `next_run` plans them.
    fn runs(lengths: &[usize], batch: usize, max: usize, halvings: u32) -> Vec<usize> {
        let mut out = Vec::new();
        let mut start = 0;
        while start < lengths.len() {
            let count = next_run(&lengths[start..], batch, max, halvings);
            assert!(count >= 1);
            out.push(count);
            start += count;
        }
        out
    }

    #[test]
    fn full_length_texts_run_batch_size_at_a_time() {
        assert_eq!(runs(&[512; 200], 64, 512, 0), vec![64, 64, 64, 8]);
    }

    #[test]
    fn short_texts_fill_the_same_memory_with_more_texts() {
        // 64 tokens: the token budget (64·512) allows 512 texts, the score budget 4096
        assert_eq!(runs(&[64; 1000], 64, 512, 0), vec![512, 488]);
    }

    #[test]
    fn a_long_text_shrinks_its_run() {
        let mut lengths = vec![32; 100];
        lengths.extend([512; 3]);
        let planned = runs(&lengths, 2, 512, 0);
        // short texts run 32 at a time (2·512 = 1024 tokens), the long ones two at a time
        assert_eq!(planned, vec![32, 32, 32, 4, 2, 1]);
        let total: usize = planned.iter().sum();
        assert_eq!(total, lengths.len());
    }

    #[test]
    fn halving_halves_the_memory_of_a_run() {
        assert_eq!(runs(&[512; 64], 64, 512, 1), vec![32, 32]);
        assert_eq!(runs(&[512; 64], 64, 512, 6), vec![1; 64]);
        // far past the budget, a text still runs on its own
        assert_eq!(runs(&[512; 3], 1, 512, 40), vec![1, 1, 1]);
    }
}
