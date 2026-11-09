//! How long the parts of a semantic search take: opening the model on the CPU
//! and embedding one query.
//!     cargo run --release --example load_time -- nomic
use codelore::embed::{EmbeddingModel, Embedder, ModelSource};

fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "nomic".into());
    let model = EmbeddingModel::from_name(&name).expect("a model name");
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let embedder = Embedder::open_model_for_queries(model, ModelSource::Cache).expect("the model opens");
        let opened = started.elapsed();
        let started = std::time::Instant::now();
        embedder.embed_query("how does the directional option borrow a direction from the probes harmonics").expect("a vector");
        eprintln!("open {:.0} ms, one query {:.0} ms", opened.as_secs_f64() * 1000.0, started.elapsed().as_secs_f64() * 1000.0);
    }
}
