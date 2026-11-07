//! RustCodeLore: the port of CodeLore 0.3.0 (github.com/WhiteWh/CodeLore).
//! The record kinds of FORMAT.md v3 with strict validation, the canonical
//! JSONL store, git's three commit modes, the one write path with its
//! reference and relation checks, duplicates, plans, sources and the command
//! layer (`cli::run`) whose text and `--json` the `lore` binary prints. Error
//! texts and notes stay word for word with the Python reference.
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::Path;

pub mod builtin;
pub mod cli;
pub mod config;
pub mod cover;
pub mod dedupe;
pub mod embed;
pub mod finder;
pub mod gitio;
pub mod history;
pub mod index;
pub mod ops;
pub mod plans;
pub mod private;
pub mod py;
pub mod promote;
pub mod refs;
pub mod schema;
pub mod search;
pub mod secrets;
pub mod stats;
pub mod store;
pub mod working;
pub mod writer;

pub use schema::{validate, RecordError};
pub use store::{canonical, parse, serialize, Store};
pub use writer::{Duplicates, Writer};

pub const VERSION: &str = "0.3.0";
pub const FORMAT_VERSION: u64 = 3;
/// Record versions this lore reads; a record keeps the version it was written at.
pub const READS: [u64; 3] = [1, 2, 3];

/// A refusal: something the user or the data must change, said by name, with
/// its details listed under it as CodeLore's `LoreError` prints them.
#[derive(Clone, Debug, PartialEq)]
pub struct LoreError {
    pub message: String,
    pub details: Vec<String>,
}

impl LoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), details: Vec::new() }
    }

    pub fn with(message: impl Into<String>, details: Vec<String>) -> Self {
        Self { message: message.into(), details }
    }
}

impl std::fmt::Display for LoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)?;
        for detail in &self.details {
            write!(f, "\n  - {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for LoreError {}

pub type Result<T> = std::result::Result<T, LoreError>;

/// Reads `lore/<area>.jsonl` (or a plan/source file) under `root`.
pub fn read_file(root: &Path, relative: &str) -> Result<Vec<Value>> {
    let bytes = std::fs::read(root.join(relative)).map_err(|error| LoreError::new(format!("{relative}: {error}")))?;
    parse(&bytes, relative)
}

/// The kinds of FORMAT.md v3.
pub fn kinds() -> &'static [&'static str] {
    &["fault", "reading", "decision", "topic", "recipe", "plan", "item", "source"]
}

/// A shared duplicate signal: lower-case words of the story fields (the
/// foundation's quick check; `dedupe` holds CodeLore's rules).
pub fn signals(record: &Value) -> BTreeSet<String> {
    let mut words = BTreeSet::new();
    for key in ["title", "symptom", "cause", "question", "choice", "what", "body", "goal", "description"] {
        if let Some(text) = record.get(key).and_then(Value::as_str) {
            for word in text.to_lowercase().split(|ch: char| !ch.is_alphanumeric()) {
                if word.len() >= 4 {
                    words.insert(word.to_owned());
                }
            }
        }
    }
    words
}

/// Jaccard similarity of two records' signals.
pub fn similarity(a: &Value, b: &Value) -> f64 {
    let (a, b) = (signals(a), signals(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(&b).count() as f64;
    shared / (a.len() + b.len() - shared as usize) as f64
}

/// The map a record was parsed into.
pub fn object(record: &Value) -> &Map<String, Value> {
    record.as_object().unwrap_or_else(|| {
        static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
        EMPTY.get_or_init(Map::new)
    })
}
