//! The candle side: one loader for the three architectures, the tokenizer and
//! the batches.
//!
//! bge-m3 and multilingual-e5-base are XLM-RoBERTa (`xlm_roberta`); bge-m3 is
//! CLS-pooled, e5 mean-pooled, and both are L2-normalised. nomic-embed-text-v1.5
//! keeps its `nomic_bert` loader for the parity tests.
use crate::embed::models::{EmbeddingModel, Pooling};
use crate::embed::Error;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::nomic_bert::NomicBertModel;
use candle_transformers::models::xlm_roberta::XLMRobertaModel;
use std::path::Path;
use tokenizers::Tokenizer;

enum Backend {
    Nomic(NomicBertModel),
    Xlm(XLMRobertaModel),
}

/// A loaded model and its tokenizer on one device.
pub struct Model {
    backend: Backend,
    tokenizer: Tokenizer,
    device: Device,
    max_tokens: usize,
    kind: EmbeddingModel,
}

impl Model {
    /// Loads the model's pinned files from `dir` on `device`. The caller
    /// checked their size (and, for an offline install, their SHA-256).
    pub fn load(dir: &Path, kind: EmbeddingModel, device: Device) -> Result<Self, Error> {
        let config = std::fs::read(dir.join("config.json"))
            .map_err(|error| Error::Model(format!("cannot read config.json: {error}")))?;
        let (backend, max_tokens) = match kind {
            EmbeddingModel::Nomic => {
                let config: candle_transformers::models::nomic_bert::Config = serde_json::from_slice(&config)
                    .map_err(|error| Error::Model(format!("cannot parse config.json: {error}")))?;
                let weights = dir.join("model.safetensors");
                // SAFETY: the file was verified (size and SHA-256) before it was
                // installed or opened, and nothing writes it while it is mapped.
                let builder = unsafe { VarBuilder::from_mmaped_safetensors(std::slice::from_ref(&weights), DType::F32, &device) }
                    .map_err(|error| Error::Model(format!("cannot map {}: {error}", weights.display())))?;
                let model = NomicBertModel::load(builder, &config)
                    .map_err(|error| Error::Model(format!("cannot load the model: {error}")))?;
                (Backend::Nomic(model), config.n_positions.clamp(8, 8192))
            }
            EmbeddingModel::BgeM3 | EmbeddingModel::E5Base => {
                let config: candle_transformers::models::xlm_roberta::Config = serde_json::from_slice(&config)
                    .map_err(|error| Error::Model(format!("cannot parse config.json: {error}")))?;
                let weights = dir.join("pytorch_model.bin");
                let builder = if weights.is_file() {
                    // bge-m3 publishes only its PyTorch .bin; candle reads it.
                    VarBuilder::from_pth(&weights, DType::F32, &device)
                        .map_err(|error| Error::Model(format!("cannot read {}: {error}", weights.display())))?
                } else {
                    let weights = dir.join("model.safetensors");
                    // SAFETY: as above, the file is verified before use.
                    unsafe { VarBuilder::from_mmaped_safetensors(std::slice::from_ref(&weights), DType::F32, &device) }
                        .map_err(|error| Error::Model(format!("cannot map {}: {error}", weights.display())))?
                };
                let model = XLMRobertaModel::new(&config, builder)
                    .map_err(|error| Error::Model(format!("cannot load the model: {error}")))?;
                (Backend::Xlm(model), config.max_position_embeddings.clamp(8, 8192).min(512))
            }
        };
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|error| Error::Model(format!("cannot read tokenizer.json: {error}")))?;
        Ok(Self { backend, tokenizer, device, max_tokens, kind })
    }

    pub fn kind(&self) -> EmbeddingModel {
        self.kind
    }

    /// Embeds every text once, at most `batch_size` texts per forward pass and
    /// at most `threads` batches at a time. The result is in input order.
    pub fn embed(&self, texts: &[&str], batch_size: usize, threads: usize) -> Result<Vec<Vec<f32>>, Error> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let batch_size = batch_size.max(1);
        let threads = threads.clamp(1, 64);
        let batches: Vec<&[&str]> = texts.chunks(batch_size).collect();
        let mut vectors = Vec::with_capacity(texts.len());
        for group in batches.chunks(threads) {
            let mut rows: Vec<Vec<Vec<f32>>> = Vec::with_capacity(group.len());
            if group.len() == 1 {
                rows.push(self.embed_batch(group[0])?);
            } else {
                std::thread::scope(|scope| -> Result<(), Error> {
                    let handles: Vec<_> = group.iter().map(|batch| scope.spawn(move || self.embed_batch(batch))).collect();
                    for handle in handles {
                        let row = handle.join().map_err(|_| Error::Run("an embedding thread panicked".into()))??;
                        rows.push(row);
                    }
                    Ok(())
                })?;
            }
            for row in rows {
                vectors.extend(row);
            }
        }
        Ok(vectors)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        let mut encoded: Vec<Vec<u32>> = Vec::with_capacity(texts.len());
        for text in texts {
            let encoding = self.tokenizer.encode(*text, true).map_err(|error| Error::Tokenize(error.to_string()))?;
            let mut ids = encoding.get_ids().to_vec();
            ids.truncate(self.max_tokens);
            encoded.push(ids);
        }
        let width = encoded.iter().map(Vec::len).max().unwrap_or(0).max(1);
        let mut ids = vec![0u32; texts.len() * width];
        let mut mask = vec![0u32; texts.len() * width];
        for (row, tokens) in encoded.iter().enumerate() {
            for (column, id) in tokens.iter().enumerate() {
                ids[row * width + column] = *id;
                mask[row * width + column] = 1;
            }
        }
        let ids = Tensor::from_vec(ids, (texts.len(), width), &self.device).map_err(run)?;
        let mask = Tensor::from_vec(mask, (texts.len(), width), &self.device).map_err(run)?;
        let token_types = Tensor::zeros((texts.len(), width), DType::U32, &self.device).map_err(run)?;
        let hidden = self
            .device
            .with_context(|| match &self.backend {
                Backend::Nomic(model) => model.forward(&ids, None, Some(&mask)),
                Backend::Xlm(model) => model.forward(&ids, &mask, &token_types, None, None, None),
            })
            .map_err(run)?;
        let pooled = match self.kind.pooling() {
            Pooling::Cls => hidden.narrow(1, 0, 1).map_err(run)?.squeeze(1).map_err(run)?,
            Pooling::Mean => {
                let hidden_dim = hidden.dim(2).map_err(run)?;
                let mask_f = mask.to_dtype(DType::F32).map_err(run)?;
                let mask_3d = mask_f.unsqueeze(2).map_err(run)?.broadcast_as((texts.len(), width, hidden_dim)).map_err(run)?;
                let sum = hidden.mul(&mask_3d).map_err(run)?.sum(1).map_err(run)?;
                let count = mask_f.sum(1).map_err(run)?.unsqueeze(1).map_err(run)?.broadcast_as((texts.len(), hidden_dim)).map_err(run)?;
                sum.div(&count.clamp(1e-9, f64::MAX).map_err(run)?).map_err(run)?
            }
        };
        let norm = pooled.sqr().map_err(run)?.sum_keepdim(1).map_err(run)?.sqrt().map_err(run)?;
        let normalized = pooled.broadcast_div(&norm).map_err(run)?;
        normalized.to_vec2::<f32>().map_err(run)
    }
}

fn run(error: candle_core::Error) -> Error {
    Error::Run(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_without_the_config_is_no_model() {
        let dir = std::env::temp_dir().join(format!("lore-model-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let error = Model::load(&dir, EmbeddingModel::E5Base, Device::Cpu)
            .err()
            .expect("a directory without the config is no model");
        assert!(matches!(error, Error::Model(_)), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
