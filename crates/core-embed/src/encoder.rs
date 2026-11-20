//! The candle backend: runs a model's transformer on Metal, CUDA or the CPU and pools its
//! per-token output into one vector per text.
//!
//! Weights come from the model's Hugging Face repository: `.safetensors` files are memory
//! mapped, `pytorch_model.bin` files go through candle's pickle reader. Everything runs in
//! f32 on purpose: XLM-R position ids are a cumulative sum in the model's dtype, and f16 and
//! bf16 stop counting exactly at 2048 and 256 tokens.
//!
//! Device memory: a run's activations grow with `texts × tokens` and its attention scores
//! with `texts × tokens²`, so runs are cut by token length (`next_run`), not by a fixed number
//! of texts. A run that does not fit on the GPU is split in two and retried, and later runs
//! get half the budget.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use candle_core::{DType, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::{bert, xlm_roberta};
use tokenizers::{Encoding, Tokenizer, TruncationParams};

use crate::device::{Device, DevicePreference, open_device};
use crate::model_error;

/// How the transformer's per-token output becomes one vector per text. Use what the model was
/// trained with; the other gives vectors that load fine but match badly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pooling {
    /// Average over the text's tokens (e5).
    Mean,
    /// The first token's output (`[CLS]`) alone (bge, mxbai).
    Cls,
}

/// The transformer a model's weights are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Architecture {
    /// XLM-R (bge-m3, multilingual-e5).
    XlmRoberta,
    /// BERT (mxbai-embed).
    Bert,
}

/// The files of a Hugging Face model repository that a model is built from.
pub(crate) struct ModelFiles {
    pub(crate) weights: PathBuf,
    pub(crate) tokenizer: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) tokenizer_config: PathBuf,
}

enum Transformer {
    XlmRoberta(xlm_roberta::XLMRobertaModel),
    Bert(bert::BertModel),
}

/// One loaded model copy.
pub(crate) struct LoadedModel {
    transformer: Transformer,
    pooling: Pooling,
    tokenizer: Tokenizer,
    pad_id: u32,
    /// The most tokens a text keeps after truncation.
    max_tokens: usize,
    /// Where the weights live and the runs happen.
    candle_device: candle_core::Device,
    device: Device,
    /// Why `auto` ended up on the CPU although this build supports a GPU.
    cpu_fallback: Option<String>,
    /// How often a run did not fit in memory; each time halves the memory budget of later runs.
    halvings: u32,
}

impl LoadedModel {
    /// Loads the weights onto the preferred device. `dims` must match the model's hidden size.
    pub(crate) fn load(
        files: &ModelFiles,
        architecture: Architecture,
        pooling: Pooling,
        dims: usize,
        max_tokens: usize,
        preference: DevicePreference,
    ) -> io::Result<Self> {
        let config = read_json(&files.config)?;
        let (tokenizer, pad_id, max_tokens) = load_tokenizer(files, &config, max_tokens)?;
        let (candle_device, device, cpu_fallback) = open_device(preference)?;
        let weights = open_weights(&files.weights, &candle_device)?;
        let bad_config =
            |e: serde_json::Error| model_error(format!("{}: {e}", files.config.display()));
        let (transformer, hidden_size) = match architecture {
            Architecture::XlmRoberta => {
                let config: xlm_roberta::Config =
                    serde_json::from_value(config).map_err(bad_config)?;
                let model =
                    xlm_roberta::XLMRobertaModel::new(&config, weights).map_err(model_error)?;
                (Transformer::XlmRoberta(model), config.hidden_size)
            }
            Architecture::Bert => {
                let config: bert::Config = serde_json::from_value(config).map_err(bad_config)?;
                let model = bert::BertModel::load(weights, &config).map_err(model_error)?;
                (Transformer::Bert(model), config.hidden_size)
            }
        };
        if hidden_size != dims {
            return Err(model_error(format!(
                "{} has hidden size {hidden_size}, expected {dims}",
                files.config.display()
            )));
        }
        Ok(Self {
            transformer,
            pooling,
            tokenizer,
            pad_id,
            max_tokens,
            candle_device,
            device,
            cpu_fallback,
            halvings: 0,
        })
    }

    pub(crate) fn device(&self) -> Device {
        self.device
    }

    pub(crate) fn cpu_fallback(&self) -> Option<&str> {
        self.cpu_fallback.as_deref()
    }

    /// Embeds `texts`, one vector per text in input order. The vectors are not normalized;
    /// `Embedder` does that.
    ///
    /// `batch_size` is the number of texts of the full `max_tokens` length a run may hold; runs
    /// of shorter texts hold more of them at the same memory (see `next_run`).
    pub(crate) fn embed(
        &mut self,
        texts: Vec<String>,
        batch_size: usize,
    ) -> io::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts, true)
            .map_err(model_error)?;

        // Shortest first, so each run pads its texts to a length close to their own.
        let mut order: Vec<usize> = (0..encodings.len()).collect();
        order.sort_by_key(|&row| encodings[row].get_ids().len());
        let lengths: Vec<usize> = order
            .iter()
            .map(|&row| encodings[row].get_ids().len())
            .collect();

        let mut vectors: Vec<Vec<f32>> = vec![Vec::new(); encodings.len()];
        let mut start = 0;
        while start < order.len() {
            // Planned one run at a time: a run that did not fit lowers the budget of the next.
            let count = next_run(
                &lengths[start..],
                batch_size,
                self.max_tokens,
                self.halvings,
            );
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
    fn run(&self, rows: &[usize], encodings: &[Encoding]) -> candle_core::Result<Vec<Vec<f32>>> {
        let batch = rows.len();
        let seq = rows
            .iter()
            .map(|&row| encodings[row].get_ids().len())
            .max()
            .unwrap_or(0)
            .max(1);
        let mut ids = vec![self.pad_id; batch * seq];
        let mut mask = vec![0_u32; batch * seq];
        for (slot, &row) in rows.iter().enumerate() {
            let start = slot * seq;
            let encoding = &encodings[row];
            let tokens = encoding.get_ids().iter().zip(encoding.get_attention_mask());
            for (col, (&id, &attend)) in tokens.enumerate() {
                ids[start + col] = id;
                mask[start + col] = attend;
            }
        }

        let device = &self.candle_device;
        let transformer = &self.transformer;
        let pooling = self.pooling;
        // On the CPU this runs on candle's own thread pool, not the process-wide rayon pool.
        device.with_context(move || {
            let ids = Tensor::from_vec(ids, (batch, seq), device)?;
            let mask = Tensor::from_vec(mask, (batch, seq), device)?;
            let token_types = ids.zeros_like()?;
            let hidden = match transformer {
                Transformer::XlmRoberta(model) => {
                    model.forward(&ids, &mask, &token_types, None, None, None)?
                }
                Transformer::Bert(model) => model.forward(&ids, &token_types, Some(&mask))?,
            };
            let pooled = match pooling {
                Pooling::Cls => hidden.narrow(1, 0, 1)?.squeeze(1)?,
                Pooling::Mean => {
                    let weights = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
                    let sum = hidden.broadcast_mul(&weights)?.sum(1)?;
                    let tokens = weights.sum(1)?.clamp(1_f32, f32::MAX)?;
                    sum.broadcast_div(&tokens)?
                }
            };
            // Only `[batch, dims]` leaves the device: copying a view would copy all of `hidden`.
            pooled.contiguous()?.to_vec2::<f32>()
        })
    }
}

/// A `VarBuilder` over the weights file, converting to f32 on `device`.
fn open_weights(path: &Path, device: &candle_core::Device) -> io::Result<VarBuilder<'static>> {
    let bad_file = |e: candle_core::Error| model_error(format!("{}: {e}", path.display()));
    if path.extension().is_some_and(|ext| ext == "safetensors") {
        // SAFETY: the file is in the model cache, which hf-hub writes once per file and never
        // changes in place, and nothing else writes it while the weights are copied out.
        unsafe { VarBuilder::from_mmaped_safetensors(&[path], DType::F32, device) }
            .map_err(bad_file)
    } else {
        VarBuilder::from_pth(path, DType::F32, device).map_err(bad_file)
    }
}

/// Whether a run failed because GPU memory ran out. On the CPU a failed allocation ends the
/// process instead, so `next_run`'s budget is the only protection there.
fn is_out_of_memory(error: &candle_core::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("out of memory")
        || message.contains("out_of_memory")
        || message.contains("failed to allocate")
        || message.contains("failed to create metal resource")
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

/// The repository's tokenizer, truncating to the most tokens a text keeps, its padding token
/// id, and that token limit.
fn load_tokenizer(
    files: &ModelFiles,
    config: &serde_json::Value,
    max_tokens: usize,
) -> io::Result<(Tokenizer, u32, usize)> {
    let tokenizer_config = read_json(&files.tokenizer_config)?;
    let pad_id = config["pad_token_id"]
        .as_u64()
        .and_then(|id| u32::try_from(id).ok())
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