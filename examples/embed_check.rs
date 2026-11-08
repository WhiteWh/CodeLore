//! Embeds the source files of this repository in pieces with the configured
//! engine and writes the vectors, for comparing engines (`LORE_DEVICE=cpu`
//! forces the CPU) and timing them:
//!     cargo run --release --features directml --example embed_check -- nomic out.json
use codelore::embed::{EmbeddingModel, Embedder, ModelSource};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model = EmbeddingModel::from_name(args.first().map_or("nomic", String::as_str)).expect("a model name");
    let out = args.get(1).cloned().unwrap_or_else(|| "vectors.json".into());
    let mut texts: Vec<String> = Vec::new();
    for entry in std::fs::read_dir("src").unwrap().flatten() {
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            for piece in text.lines().collect::<Vec<_>>().chunks(40) {
                texts.push(piece.join("\n").chars().take(1500).collect());
            }
        }
    }
    texts.truncate(256);
    let started = std::time::Instant::now();
    let embedder = Embedder::open_model(model, ModelSource::Cache).expect("the model opens");
    let loaded = started.elapsed();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    let vectors = embedder.embed_documents(&refs).expect("embedding works");
    let took = started.elapsed();
    eprintln!("{} on {}: {} pieces in {:.2} s ({:.1} ms each), load {:.2} s", model.id(), embedder.device().label(), vectors.len(), took.as_secs_f64(), took.as_secs_f64() * 1000.0 / vectors.len() as f64, loaded.as_secs_f64());
    std::fs::write(out, serde_json::to_string(&vectors).unwrap()).unwrap();
}
