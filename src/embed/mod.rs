//! Embeddings: the trait the search's index fuses with, and the built-in
//! RustCodeLore embedder.
//!
//! The models: bge-m3 (the default where a GPU works; CLS-pooled, 1024-d, no
//! prefix), multilingual-e5-base (the CPU default; mean-pooled, 768-d,
//! `passage: `/`query: `), and nomic-embed-text-v1.5 (kept for parity tests).
//! `lore.toml`'s `embed.model` selects one ("auto" follows the device). Candle
//! runs them; the CUDA feature is off unless built with it, and the device is
//! chosen at runtime (GPU when the build has it and a device initialises, else
//! the CPU) and reported by `lore model status`.
//!
//! The weights are never in the binary: [`fetch_model`] downloads them from
//! Hugging Face into the user cache, each file pinned by size and SHA-256, or
//! [`ModelSource::Local`] verifies an offline install. Without the model the
//! API answers [`Error::NoEmbeddings`] and search falls back to FTS5.
pub mod models;
#[cfg(feature = "directml")]
mod onnx;
#[cfg(feature = "directml")]
pub mod runtime;
pub mod serve;
pub mod system;

mod fetch;
mod model;

pub use fetch::verify_all as verify;
pub use fetch::{fetch, fetch as fetch_model};
pub use models::{DeviceKind, EmbeddingModel, PinnedFile, Pooling};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::LoreError;

/// The provider seam the index's semantic list fuses with.
pub trait Embeddings: Send + Sync {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LoreError>;

    /// The vector size; the index stores it and rebuilds when it changes.
    fn dimensions(&self) -> usize {
        768
    }
}

/// A recorded corpus for tests: each text's vector, never a model call.
#[derive(Clone, Default)]
pub struct Fixed(pub Vec<Vec<f32>>);

impl Embeddings for Fixed {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LoreError> {
        if texts.len() > self.0.len() {
            return Err(LoreError::new("the fixed embedding corpus is too small"));
        }
        Ok(self.0[..texts.len()].to_vec())
    }

    fn dimensions(&self) -> usize {
        self.0.first().map_or(768, Vec::len)
    }
}

pub type Shared = Arc<dyn Embeddings>;

/// The configured model's embedder, when it is installed; None keeps search
/// text-only.
pub fn builtin_shared() -> Option<Shared> {
    Embedder::open_cached().ok().map(|embedder| Arc::new(embedder) as Shared)
}

// -- the model's files ----------------------------------------------------------

/// Kept for the port's first model and the parity tests.
pub const MODEL: &str = "nomic-embed-text-v1.5";
pub const DIMENSIONS: usize = 768;
pub const DOCUMENT_PREFIX: &str = "search_document: ";
pub const QUERY_PREFIX: &str = "search_query: ";

/// What went wrong, and whether the caller should fall back to text search.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no embeddings: {0}")]
    NoEmbeddings(String),
    #[error("the model files are incomplete or corrupt: {0}")]
    Model(String),
    #[error("the model could not be fetched: {0}")]
    Fetch(String),
    #[error("a text could not be encoded: {0}")]
    Tokenize(String),
    #[error("the model failed: {0}")]
    Run(String),
}

/// Where the model's files come from.
#[derive(Clone, Copy, Debug)]
pub enum ModelSource<'a> {
    /// The shared models folder ([`model_dir`]; or `LORE_MODEL_DIR`).
    Cache,
    /// A directory that already holds the pinned files (an offline install).
    Local(&'a Path),
}

/// Tokens of one text that are embedded, at most: the model's own limit, cut
/// to [`TOKEN_CAP`] (`LORE_EMBED_MAX_TOKENS` changes it, at least 128). A
/// model's memory grows with the square of a text's length: one code piece of
/// long generated lines at nomic's 8192 tokens took 6-7 GB of an 8 GB card,
/// and what is past the first couple of thousand tokens hardly moves the
/// vector. Records are shorter than the cap.
pub fn token_cap(model_limit: usize) -> usize {
    let cap = std::env::var("LORE_EMBED_MAX_TOKENS").ok().and_then(|value| value.trim().parse::<usize>().ok()).map_or(TOKEN_CAP, |value| value.max(128));
    model_limit.min(cap)
}

/// The default of [`token_cap`]; part of the shared vector cache's key.
pub const TOKEN_CAP: usize = 2048;

/// The user cache root CodeLore uses: `LORE_CACHE`, else the platform cache
/// folder under `codelore`.
pub fn cache_root() -> PathBuf {
    if let Some(configured) = std::env::var_os("LORE_CACHE").filter(|value| !value.is_empty()) {
        return PathBuf::from(configured);
    }
    platform_cache()
}

/// The platform's per-user cache folder under `codelore`, whatever
/// `LORE_CACHE` says.
pub fn platform_cache() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty()) {
            return PathBuf::from(local).join("codelore");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
            return PathBuf::from(home).join("Library/Caches/codelore");
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(xdg).join("codelore");
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/codelore")
}

/// The one folder every `lore` on this account keeps its models in:
/// `LORE_MODELS` when set (a folder shared by several accounts, say), else
/// `models` under the platform cache - not under `LORE_CACHE`, which a harness
/// sets per store or per install: a model is 0.5-2 GB and is fetched once for
/// all of them.
pub fn models_root() -> PathBuf {
    if let Some(configured) = std::env::var_os("LORE_MODELS").filter(|value| !value.is_empty()) {
        return PathBuf::from(configured);
    }
    platform_cache().join("models")
}

/// Where `model` lives: `LORE_MODEL_DIR` when set (an offline install or a
/// test fixture); else the shared models folder, unless a complete copy is
/// already in an older place (`<LORE_CACHE>/models/`, where builds before
/// 0.4.1 fetched it), which is then used rather than fetched again.
pub fn model_dir(model: EmbeddingModel) -> PathBuf {
    if let Some(configured) = std::env::var_os("LORE_MODEL_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(configured);
    }
    let shared = models_root().join(model.directory());
    if complete(model, &shared) {
        return shared;
    }
    let older = cache_root().join("models").join(model.directory());
    if older != shared && complete(model, &older) {
        return older;
    }
    shared
}

/// Every pinned file of `model` is in `dir` at its pinned size.
fn complete(model: EmbeddingModel, dir: &Path) -> bool {
    model.files().iter().all(|file| std::fs::metadata(dir.join(file.name)).is_ok_and(|meta| meta.len() == file.bytes))
}

/// The default (device-following) model's folder.
pub fn default_model_dir() -> PathBuf {
    let (device, _) = models::device();
    model_dir(EmbeddingModel::default_for(&device))
}

/// Whether `model` can be used from `dir`, with the reason it cannot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub model: EmbeddingModel,
    pub device: DeviceKind,
    pub dir: PathBuf,
    pub available: bool,
    pub reason: String,
}

/// The model's state at `dir` without loading it: every pinned file present
/// and of the pinned size.
pub fn status_for(model: EmbeddingModel, dir: &Path) -> Status {
    let (_, device) = models::device();
    for file in model.files() {
        let path = dir.join(file.name);
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() == file.bytes => {}
            Ok(metadata) => {
                return Status::missing(model, device, dir, format!(
                    "{} is {} bytes, the pinned file is {} bytes; run `lore model fetch`",
                    path.display(), metadata.len(), file.bytes))
            }
            Err(_) => {
                return Status::missing(model, device, dir, format!(
                    "{} is missing; run `lore model fetch`", path.display()))
            }
        }
    }
    Status { model, device, dir: dir.to_path_buf(), available: true, reason: String::new() }
}

impl Status {
    fn missing(model: EmbeddingModel, device: DeviceKind, dir: &Path, reason: String) -> Self {
        Self { model, device, dir: dir.to_path_buf(), available: false, reason }
    }
}

/// The default (device-following) model's state.
pub fn status() -> Status {
    let (device, _) = models::device();
    let model = EmbeddingModel::default_for(&device);
    status_for(model, &model_dir(model))
}

/// The nomic model's state at `dir` (the port's first model; parity tests).
pub fn status_at(dir: &Path) -> Status {
    status_for(EmbeddingModel::Nomic, dir)
}

/// The embedder: one loaded model and tokenizer, ready to embed.
/// The engine an embedder runs on: candle (CPU or CUDA), or ONNX Runtime on
/// DirectML.
enum Engine {
    Candle(model::Model),
    #[cfg(feature = "directml")]
    Onnx(onnx::OnnxModel),
}

impl Engine {
    fn kind(&self) -> EmbeddingModel {
        match self {
            Self::Candle(model) => model.kind(),
            #[cfg(feature = "directml")]
            Self::Onnx(model) => model.kind(),
        }
    }

    fn embed(&self, texts: &[&str], batch_size: usize, threads: usize) -> Result<Vec<Vec<f32>>, Error> {
        match self {
            Self::Candle(model) => model.embed(texts, batch_size, threads),
            #[cfg(feature = "directml")]
            Self::Onnx(model) => model.embed(texts, batch_size),
        }
    }
}

pub struct Embedder {
    inner: Engine,
    /// What it runs on: `lore model status` says so.
    device: DeviceKind,
    batch_size: usize,
    threads: usize,
    /// The model's folder, for the CPU fallback of a GPU engine.
    dir: PathBuf,
    /// A GPU engine that failed mid-run (a hung or reset device): the rest of
    /// the process embeds on candle's CPU path with the same model.
    fallback: std::sync::OnceLock<model::Model>,
}

impl Embedder {
    /// Loads the device-following model from the cache. The model is not
    /// downloaded here: a missing model is [`Error::NoEmbeddings`].
    pub fn open_cached() -> Result<Self, Error> {
        let (device, _) = models::device();
        Self::open_model(EmbeddingModel::default_for(&device), ModelSource::Cache)
    }

    /// Backwards compatible: the device-following model.
    pub fn open(source: ModelSource<'_>) -> Result<Self, Error> {
        let (device, _) = models::device();
        Self::open_model(EmbeddingModel::default_for(&device), source)
    }

    /// Loads `model` from `source`. A local directory is verified (size and
    /// SHA-256) before it is trusted.
    pub fn open_model(model: EmbeddingModel, source: ModelSource<'_>) -> Result<Self, Error> {
        Self::open_model_on(model, source, true)
    }

    /// [`open_model`](Self::open_model) on the CPU: what a search wants. A
    /// DirectML session builds its graph on the card in every new process
    /// (~1-2 s), so one query is embedded faster by candle on the CPU (~0.5 s
    /// with the load); the GPU pays off for an index's thousands of texts.
    pub fn open_model_for_queries(model: EmbeddingModel, source: ModelSource<'_>) -> Result<Self, Error> {
        Self::open_model_on(model, source, false)
    }

    fn open_model_on(model: EmbeddingModel, source: ModelSource<'_>, gpu: bool) -> Result<Self, Error> {
        let dir = match source {
            ModelSource::Cache => model_dir(model),
            ModelSource::Local(path) => path.to_path_buf(),
        };
        let status = status_for(model, &dir);
        if !status.available {
            return Err(Error::NoEmbeddings(status.reason));
        }
        if matches!(source, ModelSource::Local(_)) {
            verify(model, &dir)?;
        }
        let (device, kind) = models::device();
        #[cfg(not(feature = "directml"))]
        let _ = gpu;
        // No CUDA here: an AMD, Intel or NVIDIA card through DirectML, when this
        // build has it and the card answers; otherwise candle on the CPU.
        #[cfg(feature = "directml")]
        if gpu && !kind.is_gpu() && models::gpu_wanted() {
            match onnx::OnnxModel::load(&dir, model) {
                Ok(onnx) => {
                    return Ok(Self { inner: Engine::Onnx(onnx), device: DeviceKind::DirectMl("DirectX 12 adapter".into()), batch_size: 32, threads: 1, dir, fallback: std::sync::OnceLock::new() })
                }
                Err(error) => directml_note(error.to_string()),
            }
        }
        let inner = Engine::Candle(model::Model::load(&dir, model, device)?);
        Ok(Self { inner, device: kind, batch_size: 32, threads: default_threads(), dir, fallback: std::sync::OnceLock::new() })
    }

    /// Embeds on the engine, or - once a GPU engine has failed - on the CPU
    /// with the same model and vectors (tests/parity.rs holds them equal).
    fn run(&self, texts: &[&str], batch_size: usize, threads: usize) -> Result<Vec<Vec<f32>>, Error> {
        if let Some(cpu) = self.fallback.get() {
            return cpu.embed(texts, batch_size, default_threads().max(threads));
        }
        match self.inner.embed(texts, batch_size, threads) {
            Ok(vectors) => Ok(vectors),
            Err(error) if matches!(self.inner, Engine::Candle(_)) => Err(error),
            Err(error) => {
                eprintln!("lore: note: the GPU embedder failed ({error}); going on on the CPU");
                let cpu = model::Model::load(&self.dir, self.inner.kind(), candle_core::Device::Cpu)?;
                let cpu = self.fallback.get_or_init(|| cpu);
                cpu.embed(texts, batch_size, default_threads())
            }
        }
    }

    /// What this embedder runs on.
    pub fn device(&self) -> &DeviceKind {
        &self.device
    }

    pub fn model(&self) -> EmbeddingModel {
        self.inner.kind()
    }

    /// [`EmbeddingModel::dimensions`].
    pub fn dimensions(&self) -> usize {
        self.inner.kind().dimensions()
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.clamp(1, 256);
        self
    }

    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.clamp(1, 64);
        self
    }

    /// The embeddings of records, each text prefixed with the model's document
    /// prefix unless it already carries one of the model's prefixes.
    pub fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        let prefix = self.inner.kind().document_prefix();
        let prefixed: Vec<String> = texts
            .iter()
            .map(|text| if text.starts_with(prefix) || !prefix.is_empty() && text.starts_with(self.inner.kind().query_prefix()) {
                (*text).to_owned()
            } else {
                format!("{prefix}{text}")
            })
            .collect();
        let references: Vec<&str> = prefixed.iter().map(String::as_str).collect();
        self.run(&references, self.batch_size, self.threads)
    }

    /// The embedding of a search query, with the model's query prefix.
    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>, Error> {
        let prefix = self.inner.kind().query_prefix();
        let prefixed = if !prefix.is_empty() && text.starts_with(prefix) {
            text.to_owned()
        } else {
            format!("{prefix}{text}")
        };
        let mut vectors = self.run(&[prefixed.as_str()], 1, 1)?;
        vectors.pop().ok_or_else(|| Error::Run("the model returned no vector".into()))
    }

    /// Embeds texts with the prefix they already carry (the index's own text).
    pub fn embed_prefixed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, Error> {
        self.run(texts, self.batch_size, self.threads)
    }
}

/// The index calls `embed` with texts that already carry their prefix.
impl Embeddings for Embedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LoreError> {
        let owned: Vec<String> = texts.to_vec();
        let references: Vec<&str> = owned.iter().map(String::as_str).collect();
        self.embed_prefixed(&references).map_err(|error| LoreError::new(error.to_string()))
    }

    fn dimensions(&self) -> usize {
        self.dimensions()
    }
}

/// At most this many batches run at once by default: the physical cores, at
/// most four, so a loaded desktop keeps a core for the person.
fn default_threads() -> usize {
    std::thread::available_parallelism().map(|count| count.get()).unwrap_or(1).min(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_short_file_is_no_embeddings_and_says_so() {
        let dir = std::env::temp_dir().join(format!("lore-model-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let status = status_at(&dir);
        assert!(!status.available);
        assert!(status.reason.contains("missing"), "{}", status.reason);
        std::fs::write(dir.join(EmbeddingModel::Nomic.files()[0].name), b"short").unwrap();
        let status = status_at(&dir);
        assert!(!status.available);
        assert!(status.reason.contains("bytes"), "{}", status.reason);
        let error = Embedder::open_model(EmbeddingModel::Nomic, ModelSource::Local(&dir))
            .err()
            .expect("no model is installed");
        assert!(matches!(error, Error::NoEmbeddings(_)), "{error}");
        assert!(error.to_string().starts_with("no embeddings:"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_selected_model_comes_from_the_configuration() {
        // The model aliases and the auto default (CPU here).
        assert_eq!(EmbeddingModel::from_name("bge-m3"), Some(EmbeddingModel::BgeM3));
        assert_eq!(EmbeddingModel::from_name("e5-base"), Some(EmbeddingModel::E5Base));
        assert_eq!(EmbeddingModel::default_for(&candle_core::Device::Cpu), EmbeddingModel::E5Base);
        // The status names the model and the device.
        let dir = std::env::temp_dir().join(format!("lore-model-status2-{}", std::process::id()));
        let status = status_at(&dir);
        assert_eq!(status.model, EmbeddingModel::Nomic);
        assert!(!status.device.is_gpu());
    }
}

/// Why DirectML was not used, the last time it was tried (`lore model status`).
#[cfg(feature = "directml")]
static DIRECTML_NOTE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(feature = "directml")]
fn directml_note(note: String) {
    if let Ok(mut slot) = DIRECTML_NOTE.lock() {
        *slot = Some(note);
    }
}

/// The reason the last DirectML attempt fell back to the CPU, if any.
pub fn directml_fallback() -> Option<String> {
    #[cfg(feature = "directml")]
    {
        DIRECTML_NOTE.lock().ok().and_then(|slot| slot.clone())
    }
    #[cfg(not(feature = "directml"))]
    {
        None
    }
}
