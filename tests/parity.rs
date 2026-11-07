//! Conformance against the Python CodeLore's own vectors.
//!
//! `tests/reference/lore-parity/` is the shared parity fixture recorded for
//! task 148 by running CodeLore 0.3.0's `codelore/embed.py` (its Ollama
//! provider, model `nomic-embed-text`, L2-normalised, 768 dimensions) on 50
//! synthetic records and 10 queries; `vectors.json` also holds the exact
//! prefixed strings that were embedded. The vectors are the Python side's, not
//! a reimplementation.
//!
//! The corpus test needs no model and always runs. The parity test loads the
//! model when it is installed (the user cache, or `LORE_MODEL_DIR`) and skips
//! with a note otherwise, so `cargo test` works on a machine that never
//! downloaded the weights.
use codelore::embed::{self, EmbeddingModel, Embedder, ModelSource, DIMENSIONS, DOCUMENT_PREFIX, QUERY_PREFIX};

const VECTORS: &str = include_str!("reference/lore-parity/vectors.json");

fn fixture() -> serde_json::Value {
    serde_json::from_str(VECTORS).expect("the parity fixture is valid JSON")
}

fn vector_of(value: &serde_json::Value, id: &str) -> Vec<f32> {
    value[&id]
        .as_array()
        .unwrap_or_else(|| panic!("the fixture has a vector for {id}"))
        .iter()
        .map(|value| value.as_f64().expect("a float") as f32)
        .collect()
}

fn inputs(value: &serde_json::Value, group: &str) -> Vec<(String, String)> {
    value[group]
        .as_object()
        .unwrap_or_else(|| panic!("the fixture has {group}"))
        .iter()
        .map(|(id, text)| (id.clone(), text.as_str().expect("a string").to_owned()))
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut left = 0.0f32;
    let mut right = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        left += x * x;
        right += y * y;
    }
    if left == 0.0 || right == 0.0 {
        return 0.0;
    }
    dot / (left.sqrt() * right.sqrt())
}

/// The recorded fixture is complete, the right size, normalised, and its
/// inputs carry the Python prefixes; this is the test that runs without the
/// model.
#[test]
fn the_reference_corpus_is_complete_and_normalised() {
    let fixture = fixture();
    assert_eq!(fixture["model"], "nomic-embed-text");
    assert_eq!(fixture["dim"], DIMENSIONS);
    assert_eq!(fixture["normalised"], true);
    assert_eq!(fixture["doc_prefix"], DOCUMENT_PREFIX);
    assert_eq!(fixture["query_prefix"], QUERY_PREFIX);
    let documents = inputs(&fixture, "document_inputs");
    let queries = inputs(&fixture, "query_inputs");
    assert_eq!(documents.len(), 50);
    assert_eq!(queries.len(), 10);
    for (id, input) in &documents {
        assert!(input.starts_with(DOCUMENT_PREFIX), "{id}: {input}");
    }
    for (id, input) in &queries {
        assert!(input.starts_with(QUERY_PREFIX), "{id}: {input}");
    }
    for (group, count) in [("corpus", 50), ("queries", 10)] {
        let vectors = fixture[group].as_object().unwrap();
        assert_eq!(vectors.len(), count, "{group}");
        for (id, _) in vectors {
            let vector = vector_of(&fixture[group], id);
            assert_eq!(vector.len(), DIMENSIONS, "{group}/{id}");
            let norm: f32 = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "{group}/{id}: {norm}");
        }
    }
}

/// Without a model the API answers "no embeddings", which is what search turns
/// into its FTS5 fallback.
#[test]
fn a_missing_model_answers_no_embeddings() {
    let dir = std::env::temp_dir().join(format!("lore-no-model-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let error = Embedder::open(ModelSource::Local(&dir)).err().expect("no model is installed");
    assert!(matches!(error, embed::Error::NoEmbeddings(_)), "{error}");
    assert!(error.to_string().starts_with("no embeddings:"), "{error}");
    assert!(!embed::status_at(&dir).available);
}

/// The Rust model's vectors against the Python ones, per text: the acceptance
/// bar is cosine 0.99; the real minimum and the time per record are printed.
#[test]
fn the_model_vectors_match_the_python_ones() {
    let fixture = fixture();
    let embedder = match Embedder::open_model(EmbeddingModel::Nomic, ModelSource::Cache) {
        Ok(embedder) => embedder,
        Err(error) => {
            eprintln!("skipping the parity run: the model is not installed ({error})");
            eprintln!("install it with `lore model fetch` or point LORE_MODEL_DIR at it");
            return;
        }
    };
    let documents = inputs(&fixture, "document_inputs");
    let queries = inputs(&fixture, "query_inputs");
    let inputs: Vec<&str> = documents.iter().map(|(_, text)| text.as_str()).collect();
    let started = std::time::Instant::now();
    let ours = embedder.embed_prefixed(&inputs).expect("the model embeds the fixture's records");
    let elapsed = started.elapsed();
    let mut minimum = f32::INFINITY;
    for ((id, _), vector) in documents.iter().zip(ours.iter()) {
        let reference = vector_of(&fixture["corpus"], id);
        let cosine = cosine(&reference, vector);
        assert!(cosine >= 0.99, "record {id}: cosine {cosine}");
        minimum = minimum.min(cosine);
    }
    for (id, text) in &queries {
        let vector = embedder.embed_prefixed(&[text.as_str()]).expect("the model embeds the queries");
        let reference = vector_of(&fixture["queries"], id);
        let cosine = cosine(&reference, &vector[0]);
        assert!(cosine >= 0.99, "query {id}: cosine {cosine}");
        minimum = minimum.min(cosine);
    }
    eprintln!(
        "parity: {} records and {} queries, minimum cosine {:.6}, {:.1} ms per record",
        documents.len(),
        queries.len(),
        minimum,
        elapsed.as_secs_f64() * 1000.0 / documents.len() as f64
    );
}
