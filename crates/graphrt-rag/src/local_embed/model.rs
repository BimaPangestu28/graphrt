//! Sentence-embedding inference for BERT-family models with candle (CPU).

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use super::{MODEL_FILES, Pooling};
use crate::error::RagError;

/// How a loaded model turns token states into one vector per text.
#[derive(Clone, Copy, Debug)]
pub(super) struct ModelOptions {
    pub pooling: Pooling,
    pub normalize: bool,
    pub max_tokens: usize,
}

/// A BERT encoder with its tokenizer, ready to embed batches of text.
pub(super) struct SentenceModel {
    model: BertModel,
    tokenizer: Tokenizer,
    options: ModelOptions,
    dimensions: usize,
}

impl std::fmt::Debug for SentenceModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SentenceModel").field("dimensions", &self.dimensions).finish_non_exhaustive()
    }
}

fn model_error(context: &str, error: impl std::fmt::Display) -> RagError {
    RagError::Model(format!("{context}: {error}"))
}

impl SentenceModel {
    /// Load `config.json`, `tokenizer.json` and `model.safetensors` from `dir`.
    ///
    /// @throws `RagError::Model` when a file is missing or invalid, or the model
    /// is not a BERT-family encoder.
    pub(super) fn load(dir: &Path, options: ModelOptions) -> Result<Self, RagError> {
        for file in MODEL_FILES {
            if !dir.join(file).is_file() {
                return Err(RagError::Model(format!("{} is missing {file}", dir.display())));
            }
        }
        let config = read_config(&dir.join("config.json"))?;
        let tokenizer = load_tokenizer(&dir.join("tokenizer.json"), &config, options.max_tokens)?;
        let weights_path = dir.join("model.safetensors");
        let weights = std::fs::read(&weights_path).map_err(|e| model_error(&weights_path.display().to_string(), e))?;
        let vars = VarBuilder::from_buffered_safetensors(weights, DTYPE, &Device::Cpu)
            .map_err(|e| model_error("cannot read model.safetensors", e))?;
        let model = BertModel::load(vars, &config).map_err(|e| model_error("cannot load BERT weights", e))?;
        Ok(Self { model, tokenizer, options, dimensions: config.hidden_size })
    }

    /// Length of the vectors this model produces.
    pub(super) fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Embed `texts` in one forward pass (callers batch them).
    pub(super) fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, RagError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings =
            self.tokenizer.encode_batch(texts.to_vec(), true).map_err(|e| model_error("tokenization failed", e))?;
        let batch = encodings.len();
        let length = encodings.first().map_or(0, |e| e.get_ids().len());
        let flatten = |field: fn(&tokenizers::Encoding) -> &[u32]| -> Vec<u32> {
            encodings.iter().flat_map(|e| field(e).iter().copied()).collect()
        };
        let device = Device::Cpu;
        let tensor = |data: Vec<u32>| {
            Tensor::from_vec(data, (batch, length), &device).map_err(|e| model_error("cannot build input", e))
        };
        let input_ids = tensor(flatten(tokenizers::Encoding::get_ids))?;
        let type_ids = tensor(flatten(tokenizers::Encoding::get_type_ids))?;
        let mask = tensor(flatten(tokenizers::Encoding::get_attention_mask))?;

        let hidden =
            self.model.forward(&input_ids, &type_ids, Some(&mask)).map_err(|e| model_error("inference failed", e))?;
        let pooled = pool(&hidden, &mask, self.options.pooling).map_err(|e| model_error("pooling failed", e))?;
        let mut vectors = pooled.to_vec2::<f32>().map_err(|e| model_error("cannot read output", e))?;
        if self.options.normalize {
            vectors.iter_mut().for_each(|v| normalize(v));
        }
        Ok(vectors)
    }
}

fn read_config(path: &Path) -> Result<BertConfig, RagError> {
    let text = std::fs::read_to_string(path).map_err(|e| model_error(&path.display().to_string(), e))?;
    let model_type: Option<String> = serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|e| model_error("invalid config.json", e))?
        .get("model_type")
        .and_then(|t| t.as_str())
        .map(str::to_owned);
    // Other encoders (XLM-RoBERTa, ModernBERT, ...) have different weights; fail
    // clearly instead of producing garbage vectors.
    if let Some(kind) = model_type.filter(|k| k != "bert") {
        return Err(RagError::Model(format!(
            "unsupported model_type `{kind}`; local embeddings support BERT-family models \
             (e.g. sentence-transformers/all-MiniLM-L6-v2, BAAI/bge-small-en-v1.5, intfloat/multilingual-e5-small)"
        )));
    }
    serde_json::from_str(&text).map_err(|e| model_error("invalid BERT config.json", e))
}

fn load_tokenizer(path: &Path, config: &BertConfig, max_tokens: usize) -> Result<Tokenizer, RagError> {
    let mut tokenizer = Tokenizer::from_file(path).map_err(|e| model_error("invalid tokenizer.json", e))?;
    let max_length = max_tokens.clamp(1, config.max_position_embeddings);
    tokenizer
        .with_truncation(Some(TruncationParams { max_length, ..TruncationParams::default() }))
        .map_err(|e| model_error("invalid truncation", e))?;
    let pad_id = u32::try_from(config.pad_token_id).map_err(|e| model_error("invalid pad_token_id", e))?;
    let pad_token = tokenizer.id_to_token(pad_id).unwrap_or_else(|| "[PAD]".to_owned());
    tokenizer.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        pad_id,
        pad_token,
        ..PaddingParams::default()
    }));
    Ok(tokenizer)
}

/// `(batch, tokens, hidden)` token states → `(batch, hidden)`, ignoring padding.
fn pool(hidden: &Tensor, mask: &Tensor, pooling: Pooling) -> candle_core::Result<Tensor> {
    match pooling {
        Pooling::Cls => hidden.narrow(1, 0, 1)?.squeeze(1),
        Pooling::Mean => {
            let weights = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
            let summed = hidden.broadcast_mul(&weights)?.sum(1)?;
            let counts = weights.sum(1)?.clamp(1e-9f32, f32::MAX)?;
            summed.broadcast_div(&counts)
        }
    }
}

/// Scale `vector` to unit length (left unchanged when it is all zeros).
pub(super) fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|x| *x /= norm);
    }
}
