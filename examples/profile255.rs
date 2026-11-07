//! Task 255's profile of the text index build and of what a `find` costs a
//! call, on a store the caller names (numbers only; the corpus is read and
//! never copied). Run: `cargo run --release --example profile255 -- STORE`.
use rusqlite::{params, Connection};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Instant;
use codelore::{config, finder, index, search, store};

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("the store folder"));
    let config = config::load(&root).expect("the store's config");
    let scratch = std::env::temp_dir().join("profile255");
    std::fs::create_dir_all(&scratch).expect("scratch folder");

    // The files, as `lore find` reads them on every call.
    let started = Instant::now();
    let snapshot = store::load_worktree(&config.root, &config.dir).expect("the store loads");
    let records: Vec<Value> = snapshot.all();
    println!("load_worktree (read + parse + canonical check of every record): {:.0} ms, {} records", ms(started), records.len());
    let mut bytes = 0usize;
    let mut texts: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in std::fs::read_dir(config.lore_dir()).expect("lore dir").flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            let data = std::fs::read(entry.path()).expect("file");
            bytes += data.len();
            texts.push((entry.path().display().to_string(), data));
        }
    }
    let started = Instant::now();
    let mut lines = 0;
    let mut parsed: Vec<Value> = Vec::new();
    for (_, data) in &texts {
        for line in std::str::from_utf8(data).unwrap().lines() {
            parsed.push(serde_json::from_str(line).unwrap());
            lines += 1;
        }
    }
    println!("  of it serde_json parse alone: {:.0} ms ({lines} lines, {} KB)", ms(started), bytes / 1024);
    let started = Instant::now();
    let canon: Vec<String> = parsed.iter().map(store::canonical).collect();
    println!("  of it canonical() of every record (deep key-sort clone + to_string): {:.0} ms", ms(started));
    let started = Instant::now();
    let cloned = records.clone();
    println!("  Context::records() clone of the whole vector: {:.0} ms ({} records)", ms(started), cloned.len());
    drop((canon, cloned));

    // The build, step by step, over the same records.
    let started = Instant::now();
    let embeds: Vec<String> = records.iter().map(index::embed_text).collect();
    println!("build: embed_text of every record: {:.0} ms", ms(started));
    let started = Instant::now();
    let columns: Vec<_> = records.iter().map(index::columns).collect();
    println!("build: columns() (strings, terms_of regex) of every record: {:.0} ms", ms(started));
    let started = Instant::now();
    let jsons: Vec<String> = records.iter().map(store::canonical).collect();
    println!("build: canonical() json column of every record: {:.0} ms", ms(started));
    drop((embeds, jsons));

    for (label, cached, tokenizer) in [
        ("execute (a statement parsed per insert), porter", false, true),
        ("prepare_cached, porter", true, true),
        ("prepare_cached, no stemming (unicode61 only)", true, false),
    ] {
        let path = scratch.join("build.sqlite");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let db = Connection::open(&path).expect("sqlite");
        db.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        let words = if tokenizer { "porter unicode61 remove_diacritics 2 tokenchars '_'" } else { "unicode61 remove_diacritics 2 tokenchars '_'" };
        db.execute_batch(&format!(
            r#"CREATE TABLE records(id TEXT PRIMARY KEY, path TEXT, kind TEXT, area TEXT,
                status TEXT, plan TEXT, title TEXT, updated TEXT, ehash TEXT, json TEXT);
            CREATE VIRTUAL TABLE words USING fts5(id UNINDEXED, signals, title, main, body, tokenize = "{words}");
            CREATE VIRTUAL TABLE terms USING fts5(id UNINDEXED, toks, tokenize = "unicode61 remove_diacritics 0 tokenchars '_./:-'");"#
        ))
        .unwrap();
        let (mut t_rec, mut t_words, mut t_terms) = (0.0, 0.0, 0.0);
        let started = Instant::now();
        let tx = db.unchecked_transaction().unwrap();
        for (record, (signals, title, main, body, terms)) in records.iter().zip(&columns) {
            let id = record.get("id").and_then(Value::as_str).unwrap_or_default();
            let t = Instant::now();
            let sql = "INSERT OR REPLACE INTO records VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)";
            let p = params![id, "f", "k", "a", "s", "p", title, "u", "h", "{}"];
            if cached { tx.prepare_cached(sql).unwrap().execute(p).unwrap(); } else { tx.execute(sql, p).unwrap(); }
            t_rec += ms(t);
            let t = Instant::now();
            let sql = "INSERT INTO words(id, signals, title, main, body) VALUES(?1,?2,?3,?4,?5)";
            let p = params![id, signals, title, main, body];
            if cached { tx.prepare_cached(sql).unwrap().execute(p).unwrap(); } else { tx.execute(sql, p).unwrap(); }
            t_words += ms(t);
            let t = Instant::now();
            let sql = "INSERT INTO terms(id, toks) VALUES(?1,?2)";
            let p = params![id, terms];
            if cached { tx.prepare_cached(sql).unwrap().execute(p).unwrap(); } else { tx.execute(sql, p).unwrap(); }
            t_terms += ms(t);
        }
        let t = Instant::now();
        tx.commit().unwrap();
        println!("sqlite, {label}: total {:.0} ms (records {:.0}, words {:.0}, terms {:.0}, commit {:.0})", ms(started), t_rec, t_words, t_terms, ms(t));
    }

    // What a find does on every call after the files are read.
    let cache = scratch.join("cache");
    let _ = std::fs::remove_dir_all(&cache);
    std::env::set_var("LORE_CACHE", &cache);
    let started = Instant::now();
    let built = index::open_index(&config).expect("index");
    println!("open_index into an empty cache (a build): {:.0} ms", ms(started));
    drop(built);
    let started = Instant::now();
    let opened = index::open_index(&config).expect("index");
    println!("open_index again (nothing changed: open, pragma, stat 18 files, empty write transaction): {:.0} ms", ms(started));
    let started = Instant::now();
    let all = opened.all().expect("all");
    println!("Index::all (SELECT json of every record + serde parse): {:.0} ms, {}", ms(started), all.len());
    let filters = finder::Filters::default();
    let started = Instant::now();
    let visible = finder::visible(&opened, &filters).expect("visible");
    println!("finder::visible (all + order + filters): {:.0} ms, {}", ms(started), visible.len());
    let started = Instant::now();
    let shown = search::visible(&records, &search::Filters::default());
    println!("search::visible (BTreeMap of cloned records for the final filter): {:.0} ms, {}", ms(started), shown.len());
    let started = Instant::now();
    let (hits, _) = finder::find(&opened, "the render pass fails on resize with a device lost error", &filters, 10, None).expect("find");
    println!("finder::find, one query, text only (includes visible again): {:.0} ms, {} hits", ms(started), hits.len());
    let started = Instant::now();
    let (_, _) = finder::find_with(&opened, &visible, &Default::default(), "", "the render pass fails on resize with a device lost error", 10, None).expect("find_with");
    println!("finder::find_with on loaded records (ranking only): {:.0} ms", ms(started));
}
