//! `embed.model = "auto"` keeps the model the project's vectors were made with:
//! DCB renamed its provider from "ollama" to "builtin" and dropped the model key,
//! and auto picked multilingual-e5-base on that machine - a 2 GB fetch, then
//! every record and code piece embedded again. One test in this file: it sets
//! LORE_CACHE for the process.
use codelore::embed::EmbeddingModel;
use rusqlite::Connection;

#[test]
fn auto_keeps_the_model_of_the_vectors_already_made() {
    let scratch = std::env::temp_dir().join(format!("lore-auto-model-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let root = scratch.join("repo");
    std::fs::create_dir_all(root.join("lore")).unwrap();
    std::fs::write(root.join("lore.toml"), "version = 1\ndir = \"lore\"\n\n[embed]\nprovider = \"builtin\"\n").unwrap();
    std::env::set_var("LORE_CACHE", scratch.join("cache"));

    let config = codelore::config::load(&root).unwrap();
    let by_device = config.embedding_model();

    // a records index whose vectors are another model than the device's pick
    let other = if matches!(by_device, EmbeddingModel::Nomic) { "multilingual-e5-base" } else { "nomic-embed-text-v1.5" };
    let dir = codelore::index::cache_dir().join(codelore::index::repo_key(&config.root));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Connection::open(dir.join("index.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE vectors(model TEXT, hash TEXT, vec BLOB, PRIMARY KEY(model, hash));").unwrap();
    db.execute("INSERT INTO vectors VALUES(?1, 'h1', x'00')", [other]).unwrap();
    db.execute("INSERT INTO vectors VALUES(?1, 'h2', x'00')", [other]).unwrap();
    drop(db);

    let kept = codelore::config::load(&root).unwrap();
    assert_eq!(kept.embedding_model().id(), other, "auto follows the stored vectors");
    let (doc, query) = kept.prefixes();
    assert_eq!(!doc.is_empty() && !query.is_empty(), other.starts_with("nomic"), "nomic's task prefixes come with nomic");

    // an explicit model is never overridden
    std::fs::write(root.join("lore.toml"), format!("version = 1\ndir = \"lore\"\n\n[embed]\nprovider = \"builtin\"\nmodel = \"{}\"\n", by_device.id())).unwrap();
    assert_eq!(codelore::config::load(&root).unwrap().embedding_model().id(), by_device.id());
    let _ = std::fs::remove_dir_all(&scratch);
}
