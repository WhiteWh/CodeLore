//! Embeds the source files of this repository in pieces with the configured
//! engine and writes the vectors, for comparing engines (`LORE_DEVICE=cpu`
//! forces the CPU) and timing them:
//!     cargo run --release --features directml --example embed_check -- nomic out.json
//! EMBED_CAPS=1500,600 (characters per piece) and EMBED_BATCHES=32,8 time each
//! pair; the vectors written are the first pair's.
use codelore::embed::{EmbeddingModel, Embedder, ModelSource};

fn list(name: &str, default: &str) -> Vec<usize> {
    std::env::var(name).unwrap_or_else(|_| default.into()).split(',').filter_map(|value| value.trim().parse().ok()).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model = EmbeddingModel::from_name(args.first().map_or("nomic", String::as_str)).expect("a model name");
    let out = args.get(1).cloned().unwrap_or_else(|| "vectors.json".into());
    let mut pieces: Vec<String> = Vec::new();
    for entry in std::fs::read_dir("src").unwrap().flatten() {
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            for piece in text.lines().collect::<Vec<_>>().chunks(40) {
                pieces.push(piece.join("\n"));
            }
        }
    }
    pieces.truncate(256);
    let started = std::time::Instant::now();
    let embedder = Embedder::open_model(model, ModelSource::Cache).expect("the model opens");
    eprintln!("{} on {}, load {:.2} s", model.id(), embedder.device().label(), started.elapsed().as_secs_f64());
    let mut first = None;
    for cap in list("EMBED_CAPS", "1500") {
        let texts: Vec<String> = pieces.iter().map(|piece| piece.chars().take(cap).collect()).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        for batch in list("EMBED_BATCHES", "32") {
            let embedder = Embedder::open_model(model, ModelSource::Cache).expect("the model opens").with_batch_size(batch);
            embedder.embed_documents(&refs[..batch.min(refs.len())]).expect("warm-up");
            let started = std::time::Instant::now();
            let vectors = embedder.embed_documents(&refs).expect("embedding works");
            let took = started.elapsed().as_secs_f64();
            eprintln!("  cap {cap:>5} chars, batch {batch:>3}: {} pieces in {took:.2} s = {:.0} pieces/min", vectors.len(), vectors.len() as f64 * 60.0 / took);
            first.get_or_insert(vectors);
        }
    }
    std::fs::write(out, serde_json::to_string(&first.unwrap_or_default()).unwrap()).unwrap();
}
