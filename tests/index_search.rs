//! The search and the disposable index (task 143), ported from CodeLore's
//! `tests/test_search_plans.py` (Search): the same records, the same queries,
//! the same expected first hit; plus the index's own contract (disposable,
//! incremental, shared with Python's schema) and the embedder seam's.
use serde_json::{json, Value};
use std::path::Path;
use codelore::config;
use codelore::embed::Embeddings;
use codelore::index::{terms_of, Index};
use codelore::finder::{errors_of, find, for_target, Filters, Semantic};
use codelore::{LoreError, Store};

fn normalise(vector: &[f32]) -> Vec<f32> {
    let norm = vector.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>().sqrt();
    let norm = if norm == 0.0 { 1.0 } else { norm };
    vector.iter().map(|x| (f64::from(*x) / norm) as f32).collect()
}

fn fault(id: &str, area: &str, extra: Value) -> Value {
    let mut record = json!({"v":3,"id":id,"kind":"fault","area":area,"title":id.replace('-', " "),
        "symptom":"x","status":"live","created":"2026-10-24","updated":"2026-10-24","by":"me"});
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    record
}

/// The three records of Python's `Search.setUp`.
fn records() -> Vec<(&'static str, Vec<Value>)> {
    vec![
        ("lore/git.jsonl", vec![fault("git-archive-piped-into-tar-fails", "git", json!({
            "title":"git archive piped into tar fails","symptom":"every file fails to extract",
            "signals":["tar.exe: Damaged tar archive (bad header checksum)"],
            "refs":{"files":["src/chain.py"],"symbols":["scene.depth_buffer"]}}))]),
        ("lore/render.jsonl", vec![fault("the-device-was-lost", "render", json!({
            "title":"the device was lost","symptom":"every frame reads the same stale picture",
            "signals":["ERR_DEVICE_LOST","0x80070005"]}))]),
        ("lore/build.jsonl", vec![fault("old-thing", "build", json!({
            "title":"old thing","status":"obsolete","symptom":"an obsolete symptom"}))]),
    ]
}

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Self { dir };
        let store = Store::new(repo.root());
        for (file, records) in records() {
            store.write(file, &records).unwrap();
        }
        repo
    }
    fn root(&self) -> &Path { self.dir.path() }
    fn index(&self) -> Index {
        let mut index = Index::open_at(&self.root().join("cache").join("index.sqlite"), self.root(), &self.root().join("lore")).unwrap();
        index.refresh().unwrap();
        index
    }
    fn add(&self, file: &str, records: Vec<Value>) {
        let store = Store::new(self.root());
        let mut all = store.read(file).unwrap_or_default();
        all.extend(records);
        store.write(file, &all).unwrap();
    }
}

fn ids(index: &Index, query: &str, filters: &Filters) -> Vec<String> {
    find(index, query, filters, 5, None).unwrap().0.into_iter().map(|hit| hit.id).collect()
}

fn first(index: &Index, query: &str) -> String {
    ids(index, query, &Filters::default()).remove(0)
}

#[test]
fn exact_signal_first() {
    let repo = Repo::new();
    let index = repo.index();
    assert_eq!(first(&index, "whatever tar.exe: Damaged tar archive (bad header checksum) more"), "git-archive-piped-into-tar-fails");
    assert_eq!(first(&index, "0x80070005"), "the-device-was-lost");
    assert_eq!(first(&index, "ERR_DEVICE_LOST"), "the-device-was-lost");
    let (hits, _) = find(&index, "ERR_DEVICE_LOST", &Filters::default(), 5, None).unwrap();
    assert_eq!(hits[0].via[0], "signal");
}

#[test]
fn a_copied_phrase_beats_a_bag_of_its_words() {
    let repo = Repo::new();
    // Many long records full of the phrase's common words, and one short one that says it.
    let noise: Vec<Value> = (0..12)
        .map(|n| {
            let symptom = "the file on the line wrote every head of the line, the file on every line ".repeat(3);
            fault(&format!("noise-{n}"), "build", json!({"title": format!("noise {n}"), "symptom": symptom.trim()}))
        })
        .collect();
    repo.add("lore/build.jsonl", noise);
    repo.add("lore/build.jsonl", vec![fault("crlf-rewrite", "build", json!({
        "title":"crlf rewrite","symptom":"the file it wrote differs from HEAD on every line"}))]);
    let index = repo.index();
    let query = "the file it wrote differs from HEAD on every line";
    assert_eq!(first(&index, query), "crlf-rewrite");
    let (hits, _) = find(&index, query, &Filters::default(), 5, None).unwrap();
    assert_eq!(hits[0].via[0], "phrase");
}

#[test]
fn words_and_whole_terms_find_their_records() {
    let repo = Repo::new();
    let index = repo.index();
    assert_eq!(first(&index, "same stale picture on every frame"), "the-device-was-lost");
    assert!(terms_of("it writes scene.depth_buffer here").contains(&"scene.depth_buffer".to_owned()));
    assert!(terms_of("got ERR_DEVICE_LOST").contains(&"ERR_DEVICE_LOST".to_owned()));
    assert!(terms_of("HRESULT 0x80070005.").contains(&"0x80070005".to_owned()));
    assert!(terms_of(r"in src\render\post.py").contains(&"src/render/post.py".to_owned()));
    assert_eq!(first(&index, "scene.depth_buffer"), "git-archive-piped-into-tar-fails");
}

#[test]
fn retired_records_are_hidden_unless_all_and_filters_narrow() {
    let repo = Repo::new();
    let index = repo.index();
    assert!(!ids(&index, "obsolete symptom", &Filters::default()).contains(&"old-thing".to_owned()));
    assert!(ids(&index, "obsolete symptom", &Filters { all: true, ..Filters::default() }).contains(&"old-thing".to_owned()));
    let narrow = Filters { kind: Some("fault".into()), area: Some("render".into()), ..Filters::default() };
    assert_eq!(ids(&index, "picture", &narrow), vec!["the-device-was-lost".to_owned()]);
}

#[test]
fn items_of_a_closed_plan_and_sources_are_hidden_like_python() {
    let repo = Repo::new();
    let plan = json!({"v":3,"id":"pl","kind":"plan","area":"plans","title":"zebra plan","goal":"zebra goal","status":"closed",
        "created":"2026-10-24","updated":"2026-10-24","by":"me"});
    let item = json!({"v":3,"id":"pl#1","kind":"item","area":"plans","plan":"pl","title":"zebra item","status":"open",
        "created":"2026-10-24","updated":"2026-10-24","by":"me"});
    repo.add("lore/plans/pl.jsonl", vec![plan, item]);
    let source = json!({"v":3,"id":"src","kind":"source","area":"docs","title":"zebra source","path":"docs/z.md","status":"open",
        "created":"2026-10-24","updated":"2026-10-24","by":"me"});
    repo.add("lore/sources.jsonl", vec![source]);
    let index = repo.index();
    assert!(ids(&index, "zebra", &Filters::default()).is_empty());
    let all = ids(&index, "zebra", &Filters { all: true, ..Filters::default() });
    assert!(all.contains(&"pl".to_owned()) && all.contains(&"pl#1".to_owned()), "{all:?}");
    assert_eq!(ids(&index, "zebra", &Filters { kind: Some("source".into()), ..Filters::default() }), vec!["src".to_owned()]);
    let found: Vec<String> = for_target(&index, "docs/z.md", &Filters::default()).unwrap().iter().map(|r| r["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(found, vec!["src".to_owned()]);
}

#[test]
fn the_index_is_fresh_after_a_change() {
    let repo = Repo::new();
    let mut index = repo.index();
    let mut changed = fault("the-device-was-lost", "render", json!({
        "title":"the device was lost","symptom":"the frame turns purple","signals":["ERR_DEVICE_LOST","0x80070005"]}));
    changed["updated"] = json!("2026-10-25");
    Store::new(repo.root()).write("lore/render.jsonl", &[changed]).unwrap();
    index.refresh().unwrap();
    assert_eq!(first(&index, "purple"), "the-device-was-lost");
    assert!(!ids(&index, "stale picture", &Filters::default()).contains(&"the-device-was-lost".to_owned()));
}

#[test]
fn a_pasted_traceback_is_searched_by_its_error_lines() {
    let repo = Repo::new();
    let index = repo.index();
    let log = "Traceback (most recent call last):\n  File \"x.py\", line 3, in <module>\n    run()\nRuntimeError: renderer: GetData failed, HRESULT 0x80070005\n";
    let query = errors_of(log);
    assert!(!query.contains("File"), "{query}");
    assert_eq!(first(&index, &query), "the-device-was-lost");
    // No error-looking line: the tail of the text, at most 2000 characters.
    assert_eq!(errors_of("just words\nmore words"), "just words\nmore words");
    // At most twelve lines, the last ones, each once.
    let many: String = (0..30).map(|n| format!("error {n}\nerror {n}\n")).collect();
    let kept = errors_of(&many);
    assert_eq!(kept.lines().count(), 12);
    assert!(kept.starts_with("error 18") && kept.ends_with("error 29"), "{kept}");
}

#[test]
fn for_a_file_a_directory_or_a_symbol() {
    let repo = Repo::new();
    let index = repo.index();
    let absolute = repo.root().join("src").join("chain.py").to_string_lossy().into_owned();
    for target in ["src/chain.py", "src", "src/", absolute.as_str(), "scene.depth_buffer", "scene", "./src/chain.py", r"src\chain.py"] {
        let found: Vec<String> = for_target(&index, target, &Filters::default()).unwrap().iter().map(|r| r["id"].as_str().unwrap().to_owned()).collect();
        assert_eq!(found, vec!["git-archive-piped-into-tar-fails".to_owned()], "{target}");
    }
    assert!(for_target(&index, "src/other.py", &Filters::default()).unwrap().is_empty());
}

/// Bag-of-words vectors: deterministic meaning for the fusion test.
struct Bag;

impl Embeddings for Bag {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LoreError> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut vector = vec![0.0f32; 64];
                for word in text.to_lowercase().split_whitespace() {
                    let word = word.trim_matches(|c: char| ".,:;()".contains(c));
                    if word.chars().count() > 2 {
                        let slot = word.bytes().fold(7usize, |hash, byte| hash.wrapping_mul(31).wrapping_add(byte as usize)) % 64;
                        vector[slot] += 1.0;
                    }
                }
                normalise(&vector)
            })
            .collect())
    }
}

#[test]
fn fusion_with_meaning_adds_a_list_and_reuses_vectors() {
    let repo = Repo::new();
    let index = repo.index();
    let semantic = Semantic { embedder: &Bag, model: "fake-model", doc_prefix: "", query_prefix: "" };
    let (hits, notes) = find(&index, "frame picture stale", &Filters::default(), 5, Some(&semantic)).unwrap();
    assert!(notes.is_empty(), "{notes:?}");
    assert!(hits[0].via.contains(&"meaning".to_owned()), "{:?}", hits[0].via);
    assert_eq!(hits[0].id, "the-device-was-lost");
    // The vectors are in the index: a refusing provider is not asked again for the records.
    struct OnlyQueries;
    impl Embeddings for OnlyQueries {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LoreError> {
            assert_eq!(texts.len(), 1, "only the query is embedded");
            Bag.embed(texts)
        }
    }
    let again = Semantic { embedder: &OnlyQueries, model: "fake-model", doc_prefix: "", query_prefix: "" };
    let (hits, notes) = find(&index, "frame picture stale", &Filters::default(), 5, Some(&again)).unwrap();
    assert!(notes.is_empty() && hits[0].via.contains(&"meaning".to_owned()));
}

/// The model is not installed: the built-in embedder refuses with `no embeddings`.
struct NotInstalled;

impl Embeddings for NotInstalled {
    fn embed(&self, _: &[String]) -> Result<Vec<Vec<f32>>, LoreError> {
        Err(LoreError::new("no embeddings: model.safetensors is missing; run `lore model fetch`"))
    }
}

#[test]
fn a_model_that_is_not_installed_is_text_only_and_says_so() {
    let repo = Repo::new();
    let index = repo.index();
    let semantic = Semantic { embedder: &NotInstalled, model: "nomic-embed-text-v1.5", doc_prefix: "", query_prefix: "" };
    let (hits, notes) = find(&index, "stale picture", &Filters::default(), 5, Some(&semantic)).unwrap();
    assert!(notes.iter().any(|note| note.contains("unavailable") && note.contains("no embeddings")), "{notes:?}");
    assert_eq!(hits[0].id, "the-device-was-lost");
    assert!(!hits[0].via.contains(&"meaning".to_owned()));
}

#[test]
fn the_index_is_disposable_and_rebuilt_from_the_jsonl() {
    let repo = Repo::new();
    let path = repo.root().join("cache").join("index.sqlite");
    let before = {
        let index = repo.index();
        find(&index, "stale picture", &Filters::default(), 5, None).unwrap().0
    };
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let mut index = Index::open_at(&path, repo.root(), &repo.root().join("lore")).unwrap();
    let summary = index.refresh().unwrap();
    assert!(summary.rebuilt && summary.files_read == 3 && summary.records == 3, "{summary:?}");
    assert_eq!(find(&index, "stale picture", &Filters::default(), 5, None).unwrap().0, before);
    // The index is not in the repository: nothing under the lore folder or the root's git state.
    assert!(!repo.root().join("lore").join("index.sqlite").exists());
}

#[test]
fn a_refresh_reads_only_what_changed() {
    let repo = Repo::new();
    let mut index = repo.index();
    let quiet = index.refresh().unwrap();
    assert_eq!((quiet.files_read, quiet.files_dropped, quiet.records, quiet.rebuilt), (0, 0, 0, false));
    // The same bytes written again: the file is looked at, not indexed again.
    let render = Store::new(repo.root()).read("lore/render.jsonl").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    Store::new(repo.root()).write("lore/render.jsonl", &render).unwrap();
    let same = index.refresh().unwrap();
    assert_eq!((same.files_read, same.records), (0, 0), "{same:?}");
    // One file changed: one file read.
    repo.add("lore/git.jsonl", vec![fault("zz-new-one", "git", json!({"title":"zz new one","symptom":"quokka"}))]);
    let changed = index.refresh().unwrap();
    assert_eq!((changed.files_read, changed.records), (1, 2), "{changed:?}");
    assert_eq!(first(&index, "quokka"), "zz-new-one");
    // A file that is gone is dropped with its records.
    std::fs::remove_file(repo.root().join("lore/git.jsonl")).unwrap();
    let gone = index.refresh().unwrap();
    assert_eq!(gone.files_dropped, 1);
    assert!(ids(&index, "quokka", &Filters::default()).is_empty());
    assert_eq!(index.count().unwrap(), 2);
}

#[test]
fn another_index_version_is_deleted_and_built_again() {
    let repo = Repo::new();
    let path = repo.root().join("cache").join("index.sqlite");
    drop(repo.index());
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA user_version = 2").unwrap();
    }
    let mut index = Index::open_at(&path, repo.root(), &repo.root().join("lore")).unwrap();
    assert!(index.refresh().unwrap().rebuilt);
    assert_eq!(index.count().unwrap(), 3);
    let db = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(version, codelore::index::INDEX_VERSION);
}

#[test]
fn a_writer_holding_the_index_is_waited_for() {
    let repo = Repo::new();
    let mut index = repo.index();
    let path = index.path().to_path_buf();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let other = rusqlite::Connection::open(path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        held_tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        other.execute_batch("ROLLBACK").unwrap();
    });
    held_rx.recv().unwrap();
    repo.add("lore/git.jsonl", vec![fault("written-while-another-process-holds-the-index", "git", json!({
        "title":"written while another process holds the index","symptom":"waited"}))]);
    let started = std::time::Instant::now();
    index.refresh().unwrap();
    assert!(started.elapsed().as_millis() >= 1000, "the refresh waited for the holder");
    holder.join().unwrap();
    assert_eq!(first(&index, "another process holds"), "written-while-another-process-holds-the-index");
}

#[test]
fn open_index_puts_the_index_in_the_user_cache_keyed_by_the_repository() {
    let repo = Repo::new();
    let cache = tempfile::tempdir().unwrap();
    std::env::set_var("LORE_CACHE", cache.path());
    std::fs::write(repo.root().join("lore.toml"), "version = 1\nareas = [\"git\", \"render\", \"build\"]\n[embed]\nprovider = \"none\"\n").unwrap();
    let config = config::load(repo.root()).unwrap();
    let index = codelore::index::open_index(&config).unwrap();
    assert!(index.path().starts_with(cache.path()), "{}", index.path().display());
    assert_eq!(index.path().parent().unwrap().file_name().unwrap().to_string_lossy(), codelore::index::repo_key(repo.root()));
    assert_eq!(index.count().unwrap(), 3);
}

/// Task 177: the index records the model and dimension; a change drops every
/// stored vector and rebuilds, so a new `embed.model` cannot read old vectors.
#[test]
fn a_model_or_dimension_change_rebuilds_the_vectors() {
    use codelore::embed::Fixed;
    let repo = Repo::new();
    let index = repo.index();
    index.ensure_vectors(&Fixed(vec![vec![1.0; 768]; 3]), "bge-m3", "").unwrap();
    assert_eq!(index.vectors("bge-m3").unwrap().len(), 3);
    // The same model with a new dimension is a rebuild too.
    index.ensure_vectors(&Fixed(vec![vec![1.0; 1024]; 3]), "bge-m3", "").unwrap();
    let vectors = index.vectors("bge-m3").unwrap();
    assert_eq!(vectors.len(), 3);
    assert_eq!(vectors[0].1.len(), 1024);
    // A different model drops the old model's vectors.
    index.ensure_vectors(&Fixed(vec![vec![2.0; 768]; 3]), "multilingual-e5-base", "passage: ").unwrap();
    assert!(index.vectors("bge-m3").unwrap().is_empty());
    assert_eq!(index.vectors("multilingual-e5-base").unwrap().len(), 3);
}

/// Task 177: the preloaded variant ranks exactly as `find` does.
fn bge_semantic<'a>(embedder: &'a dyn Embeddings) -> Semantic<'a> {
    Semantic { embedder, model: "bge-m3", doc_prefix: "", query_prefix: "" }
}

#[test]
fn find_with_the_preloaded_vectors_ranks_as_find() {
    use std::collections::HashMap;
    use codelore::finder::{find, find_with, visible};
    let repo = Repo::new();
    let index = repo.index();
    let fixed = codelore::embed::Fixed(vec![vec![1.0; 1024]; 3]);
    index.ensure_vectors(&fixed, "bge-m3", "").unwrap();
    let vectors: HashMap<String, Vec<f32>> = index.vectors("bge-m3").unwrap().into_iter().collect();
    let visible_records = visible(&index, &Filters::default()).unwrap();
    let provider = codelore::embed::Fixed(vec![vec![1.0; 1024]]);
    for query in ["the device was lost", "damaged tar archive", "old thing"] {
        let (a, _) = find(&index, query, &Filters::default(), 5, Some(&bge_semantic(&provider))).unwrap();
        let (b, _) = find_with(&index, &visible_records, &vectors, "bge-m3", query, 5, Some(&bge_semantic(&provider))).unwrap();
        let ids = |hits: &[codelore::finder::Hit]| hits.iter().map(|hit| hit.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&a), ids(&b), "{query}");
    }
}
