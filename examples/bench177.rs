//! Task 177's bench harness: the Rust searcher's hybrid hit@1/@5 for exact,
//! English paraphrase and Russian paraphrase on qa's copy of a production project's lore, using the
//! precomputed embedding vectors from `.qa-lore-bench` (numbers only; the project's
//! text is never committed), plus the e5 CPU embedding time per record with the
//! crate's loader.
//!
//! Run: `cargo run --example bench177 [-- --bench PATH]`. The model caches and
//! the vector .npy files stay outside the repository.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Instant;
use codelore::embed::{Embeddings, Embedder, EmbeddingModel, ModelSource};
use codelore::finder::{find_with, visible, Filters, Semantic};
use codelore::index;
use codelore::index::Index;

fn main() {
    let bench = bench_path();
    let records = load_records(&bench.join("work"));
    println!("records {} queries {}", records.len(), query_rows(&bench).0.len());
    let work = PathBuf::from(r"D:\Tools\Zerus-team-33fbafeb-hgs\.zerus-team\bench177");
    std::fs::create_dir_all(&work).ok();
    let work_dir = bench.join("work");
    // The Python bench read only the top-level lore files (no plans/items);
    // the index must see the same 6113 records as the corpus vectors.
    let lore_copy = work.join("lore");
    let _ = std::fs::remove_dir_all(&lore_copy);
    std::fs::create_dir_all(&lore_copy).ok();
    for entry in std::fs::read_dir(work_dir.join("lore")).expect("the bench lore folder").flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            if let Some(name) = path.file_name() {
                std::fs::copy(&path, lore_copy.join(name)).ok();
            }
        }
    }
    let mut index = Index::open_at(&work.join("index.sqlite"), &work_dir, &lore_copy).expect("index opens");
    index.refresh().expect("index refreshes");
    assert_eq!(index.count().unwrap(), records.len(), "the index and the records must agree");
    for (name, model, dim, doc, query, corpus, queries) in [
        ("nomic-ollama", "nomic-embed-text-v1.5", 768usize, "search_document: ", "search_query: ", bench.join("model-bench/nomic-ollama.npy"), work.join("nomic-queries.npy")),
        ("multilingual-e5-base", "multilingual-e5-base", 768, "passage: ", "query: ", bench.join("model-bench/multilingual-e5-base.npy"), bench.join("model-bench/multilingual-e5-base-queries.npy")),
        ("bge-m3", "bge-m3", 1024, "", "", bench.join("model-bench/bge-m3.npy"), bench.join("model-bench/bge-m3-queries.npy")),
    ] {
        let Some(corpus) = read_npy(&corpus) else {
            println!("{name}: corpus vectors missing ({})", corpus.display());
            continue;
        };
        let Some(queries) = read_npy(&queries) else {
            println!("{name}: query vectors missing ({})", queries.display());
            continue;
        };
        let started = Instant::now();
        let pairs: Vec<(String, Vec<f32>)> = records
            .iter()
            .zip(corpus.iter())
            .filter_map(|(record, vector)| record.get("id").and_then(Value::as_str).map(|id| (id.to_owned(), vector.clone())))
            .collect();
        index.load_vectors(model, dim, &pairs).expect("vectors load");
        let index_ms = started.elapsed().as_secs_f64() * 1000.0;
        // Loaded once per model: the visible records and the vector map.
        let visible_records = visible(&index, &Filters::default()).expect("visible records");
        let loaded: std::collections::HashMap<String, Vec<f32>> = index.vectors(model).expect("vectors").into_iter().collect();
        let (en, ru) = query_rows(&bench);
        let mut results: Vec<(&str, &str, Option<usize>)> = Vec::new();
        let provider = QueryVectors { vectors: &queries, next: std::sync::atomic::AtomicUsize::new(0), dim };
        let mut query_seconds = 0.0;
        for (row, ru_row) in en.iter().zip(ru.iter()) {
            let keys: Vec<String> = row["keys"].as_array().into_iter().flatten().filter_map(|key| key.as_str().map(str::to_owned)).collect();
            for (kind, lang, text) in [
                ("exact", "en", row["exact"].as_str().unwrap_or_default().to_owned()),
                ("para", "en", row["para"].as_str().unwrap_or_default().to_owned()),
                ("para", "ru", ru_row["para"].as_str().unwrap_or_default().to_owned()),
            ] {
                let started = Instant::now();
                let semantic = Semantic { embedder: &provider, model, doc_prefix: doc, query_prefix: query };
                let (hits, _) = find_with(&index, &visible_records, &loaded, model, &text, 10, Some(&semantic)).expect("find");
                query_seconds += started.elapsed().as_secs_f64();
                let rank = hits.iter().enumerate().find_map(|(position, hit)| {
                    index.get(&hit.id).ok().flatten().filter(|record| answers(record, &keys)).map(|_| position + 1)
                });
                results.push((kind, lang, rank));
            }
        }
        let rate = |kind: &str, lang: &str, k: usize| {
            let selected: Vec<&Option<usize>> = results.iter().filter(|(kind_, lang_, _)| *kind_ == kind && *lang_ == lang).map(|(_, _, rank)| rank).collect();
            let hit = selected.iter().filter(|rank| rank.is_some_and(|rank| rank <= k)).count();
            json!({"n": selected.len(), "hit@1": percent(hit, selected.len()), "hit@5": percent(selected.iter().filter(|rank| rank.is_some_and(|rank| rank <= 5)).count(), selected.len())})
        };
        let summary = json!({
            "model": name,
            "index_ms": (index_ms * 10.0).round() / 10.0,
            "query_ms": ((query_seconds / results.len().max(1) as f64) * 1000.0 * 1000.0).round() / 1000.0,
            "exact_en": rate("exact", "en", 5),
            "para_en": rate("para", "en", 5),
            "para_ru": rate("para", "ru", 5),
        });
        println!("{}", summary);
    }
    // The e5 loader on a few hundred records: CPU embedding time per record.
    if let Some(snapshot) = e5_snapshot(&bench) {
        let Some(embedder) = Embedder::open_model(EmbeddingModel::E5Base, ModelSource::Local(&snapshot)).ok() else {
            println!("e5: the loader could not open {}", snapshot.display());
            return;
        };
        let texts: Vec<String> = records.iter().take(250).map(index::embed_text).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let started = Instant::now();
        let vectors = embedder.embed_documents(&refs).expect("e5 embeds");
        let elapsed = started.elapsed().as_secs_f64();
        println!(
            "{}",
            json!({"model": "multilingual-e5-base (candle, CPU)", "records": vectors.len(),
                   "seconds": (elapsed * 10.0).round() / 10.0,
                   "ms_per_record": (elapsed * 1000.0 / vectors.len().max(1) as f64 * 10.0).round() / 10.0,
                   "device": "CPU"})
        );
    }
}

fn percent(hit: usize, count: usize) -> f64 {
    if count == 0 { 0.0 } else { (1000.0 * hit as f64 / count as f64).round() / 10.0 }
}

fn bench_path() -> PathBuf {
    std::env::args()
        .skip_while(|arg| arg != "--bench")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"D:\Tools\Zerus\.qa-lore-bench"))
}

fn load_records(bench: &Path) -> Vec<Value> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(bench.join("lore")).expect("the bench lore folder").filter_map(|entry| entry.ok().map(|entry| entry.path())).filter(|path| path.extension().is_some_and(|ext| ext == "jsonl")).collect();
    files.sort();
    let mut records = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("a lore file");
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            if let Ok(record) = serde_json::from_str::<Value>(line) {
                records.push(record);
            }
        }
    }
    records
}

fn query_rows(bench: &Path) -> (Vec<Value>, Vec<Value>) {
    let read = |path: PathBuf| -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("a query file")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    };
    (read(bench.join("work/tools/lore_bench_queries.jsonl")), read(bench.join("work-ru/tools/lore_bench_queries.jsonl")))
}

fn answers(record: &Value, keys: &[String]) -> bool {
    let mut text = String::new();
    for field in ["title", "symptom", "what", "value", "command", "cause", "fix", "check", "signals", "body", "text"] {
        if let Some(value) = record.get(field) {
            match value {
                Value::Array(items) => text.push_str(&items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")),
                Value::String(text_) => text.push_str(text_),
                _ => {}
            }
            text.push('\n');
        }
    }
    keys.iter().any(|key| text.contains(key))
}

/// A `.npy` of f32 rows, little-endian C order: shape (rows, columns).
fn read_npy(path: &Path) -> Option<Vec<Vec<f32>>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 10 || &bytes[..6] != b"\x93NUMPY" {
        return None;
    }
    let major = bytes[6];
    let header_len = if major == 1 { u16::from_le_bytes([bytes[8], bytes[9]]) as usize } else { u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize };
    let offset = if major == 1 { 10 } else { 12 };
    let header = std::str::from_utf8(&bytes[offset..offset + header_len]).ok()?;
    if !header.contains("<f4") {
        return None;
    }
    let shape_at = header.find("'shape':")? + "'shape':".len();
    let shape_text = &header[shape_at..];
    let open = shape_text.find('(')?;
    let close = shape_text.find(')')?;
    let numbers: Vec<usize> = shape_text[open + 1..close].split(',').filter_map(|part| part.trim().parse().ok()).collect();
    if numbers.len() != 2 {
        return None;
    }
    let (rows, columns) = (numbers[0], numbers[1]);
    let data = &bytes[offset + header_len..];
    if data.len() < rows * columns * 4 {
        return None;
    }
    Some((0..rows).map(|row| (0..columns).map(|column| {
        let at = (row * columns + column) * 4;
        f32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
    }).collect()).collect())
}

fn e5_snapshot(bench: &Path) -> Option<PathBuf> {
    let root = bench.join("hf/hub/models--intfloat--multilingual-e5-base/snapshots");
    std::fs::read_dir(root).ok()?.flatten().map(|entry| entry.path()).find(|path| path.join("model.safetensors").is_file())
}

/// The precomputed query vectors of one model, in the bench's query order.
struct QueryVectors<'a> {
    vectors: &'a [Vec<f32>],
    next: std::sync::atomic::AtomicUsize,
    dim: usize,
}

impl Embeddings for QueryVectors<'_> {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, codelore::LoreError> {
        let mut out = Vec::new();
        for _ in texts {
            let index = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let vector = self.vectors.get(index).cloned().ok_or_else(|| codelore::LoreError::new("the bench query vectors ran out"))?;
            out.push(vector);
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dim
    }
}
