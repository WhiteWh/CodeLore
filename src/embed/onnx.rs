//! The ONNX Runtime side, built with the `directml` feature: the same three
//! models (their ONNX exports, pinned beside the candle weights) run through
//! DirectML, which is any DirectX 12 GPU on Windows - AMD, Intel or NVIDIA.
//! The tokenizer, the pooling and the normalisation are the candle side's, so
//! a record gets the same vector whichever engine made it (tests/parity.rs).
//!
//! ONNX Runtime is linked in statically; DirectML.dll is not shipped in the
//! binary but fetched once, pinned, into the shared models folder
//! (`runtime.rs`) and loaded by its path before the first session, so the
//! binary starts and runs on the CPU wherever DirectML is missing.
use crate::embed::models::{EmbeddingModel, Pooling};
use crate::embed::Error;
use ort::session::Session;
use ort::value::Tensor;
use std::path::Path;
use std::sync::Mutex;
use tokenizers::Tokenizer;

pub struct OnnxModel {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    max_tokens: usize,
    kind: EmbeddingModel,
    /// Whether the export takes `token_type_ids` (BERT does, XLM-R may not).
    token_types: bool,
    output: String,
}

fn run(error: impl std::fmt::Display) -> Error {
    Error::Run(format!("onnx: {error}"))
}

impl OnnxModel {
    /// Opens `kind`'s ONNX export in `dir` on DirectML. An error means "use
    /// the candle side"; the caller says why in `lore model status`.
    pub fn load(dir: &Path, kind: EmbeddingModel) -> Result<Self, Error> {
        let file = dir.join(kind.onnx_file().ok_or_else(|| Error::Model(format!("{} has no ONNX export pinned", kind.id())))?);
        crate::embed::runtime::load_directml()?;
        static ENVIRONMENT: std::sync::OnceLock<Result<ort::environment::Environment, String>> = std::sync::OnceLock::new();
        let environment = ENVIRONMENT
            .get_or_init(|| ort::init().with_name("lore").build().map_err(|error| error.to_string()))
            .as_ref()
            .map_err(|error| Error::Run(format!("onnx: {error}")))?;
        let session = Session::builder(environment)
            .map_err(run)?
            // DirectML wants one stream: no memory patterns, no parallel runs.
            .with_memory_pattern(false)
            .map_err(run)?
            .with_parallel_execution(false)
            .map_err(run)?
            .with_execution_providers([ort::ep::DirectML::default().with_performance_preference(ort::ep::directml::PerformancePreference::HighPerformance).build().error_on_failure()])
            .map_err(run)?
            .commit_from_file(&file)
            .map_err(run)?;
        let token_types = session.inputs().iter().any(|input| input.name() == "token_type_ids");
        let output = session
            .outputs()
            .iter()
            .map(|output| output.name().to_owned())
            .find(|name| name == "last_hidden_state")
            .or_else(|| session.outputs().first().map(|output| output.name().to_owned()))
            .ok_or_else(|| Error::Model("the ONNX export has no output".into()))?;
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|error| Error::Model(format!("cannot read tokenizer.json: {error}")))?;
        let max_tokens = match kind {
            EmbeddingModel::Nomic => 8192,
            EmbeddingModel::BgeM3 | EmbeddingModel::E5Base => 512,
        };
        Ok(Self { session: Mutex::new(session), tokenizer, max_tokens, kind, token_types, output })
    }

    pub fn kind(&self) -> EmbeddingModel {
        self.kind
    }

    /// Texts of a similar length go in one batch, and each batch is padded to
    /// a fixed width (a power of two): DirectML prepares its graph for every
    /// new input shape, so a handful of shapes is fast where one per batch is
    /// not. The result is in input order.
    pub fn embed(&self, texts: &[&str], batch_size: usize) -> Result<Vec<Vec<f32>>, Error> {
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&at| texts[at].len());
        let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for group in order.chunks(batch_size.max(1)) {
            let batch: Vec<&str> = group.iter().map(|&at| texts[at]).collect();
            for (&at, vector) in group.iter().zip(self.embed_batch(&batch)?) {
                out[at] = Some(vector);
            }
        }
        Ok(out.into_iter().map(Option::unwrap_or_default).collect())
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        let mut encoded: Vec<Vec<u32>> = Vec::with_capacity(texts.len());
        for text in texts {
            let encoding = self.tokenizer.encode(*text, true).map_err(|error| Error::Tokenize(error.to_string()))?;
            let mut ids = encoding.get_ids().to_vec();
            ids.truncate(self.max_tokens);
            encoded.push(ids);
        }
        let rows = texts.len();
        let longest = encoded.iter().map(Vec::len).max().unwrap_or(0).max(1);
        let width = longest.next_power_of_two().clamp(16, self.max_tokens.max(16));
        let mut ids = vec![0i64; rows * width];
        let mut mask = vec![0i64; rows * width];
        for (row, tokens) in encoded.iter().enumerate() {
            for (column, id) in tokens.iter().enumerate() {
                ids[row * width + column] = i64::from(*id);
                mask[row * width + column] = 1;
            }
        }
        let shape = vec![rows as i64, width as i64];
        let mut inputs = vec![
            ("input_ids".to_owned(), Tensor::from_array((shape.clone(), ids)).map_err(run)?.into_dyn()),
            ("attention_mask".to_owned(), Tensor::from_array((shape.clone(), mask.clone())).map_err(run)?.into_dyn()),
        ];
        if self.token_types {
            inputs.push(("token_type_ids".to_owned(), Tensor::from_array((shape, vec![0i64; rows * width])).map_err(run)?.into_dyn()));
        }
        let mut session = self.session.lock().map_err(|_| Error::Run("the ONNX session is poisoned".into()))?;
        let outputs = session.run(inputs).map_err(run)?;
        let (dims, hidden) = outputs[self.output.as_str()].try_extract_tensor::<f32>().map_err(run)?;
        let dims: Vec<i64> = dims.iter().copied().collect();
        if dims.len() != 3 || dims[0] as usize != rows {
            return Err(Error::Run(format!("onnx: unexpected output shape {dims:?}")));
        }
        let (seq, dim) = (dims[1] as usize, dims[2] as usize);
        let mut vectors = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut pooled = vec![0f32; dim];
            match self.kind.pooling() {
                Pooling::Cls => pooled.copy_from_slice(&hidden[row * seq * dim..row * seq * dim + dim]),
                Pooling::Mean => {
                    let mut count = 0f32;
                    for token in 0..seq.min(width) {
                        if mask[row * width + token] == 1 {
                            count += 1.0;
                            let at = (row * seq + token) * dim;
                            for (sum, value) in pooled.iter_mut().zip(&hidden[at..at + dim]) {
                                *sum += value;
                            }
                        }
                    }
                    let count = count.max(1e-9);
                    pooled.iter_mut().for_each(|value| *value /= count);
                }
            }
            let norm = pooled.iter().map(|value| value * value).sum::<f32>().sqrt().max(1e-12);
            pooled.iter_mut().for_each(|value| *value /= norm);
            vectors.push(pooled);
        }
        Ok(vectors)
    }
}
