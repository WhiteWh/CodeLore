//! Task 255: the text index builds once and fast. What changed to get there must not
//! change what the index or the store says: canonical lines are the same bytes as
//! before, an accepted line is the record's own, `--rebuild` throws the old index away
//! before its one build, and the index folders are asked for (and `git` started) only
//! when a search needs them.
use serde_json::{json, Map, Value};
use std::path::Path;
use codelore::search::Finder;
use codelore::store::{canonical, parse, parse_lines, serialize, sorted};
use codelore::{config, index, LoreError};

/// What `canonical` was before it stopped copying the value: serde_json over the sorted copy.
fn reference(value: &Value) -> String {
    serde_json::to_string(&sorted(value)).unwrap()
}

/// A deterministic pseudo-random JSON value: keys out of order, odd text, every number shape.
fn rnd(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

fn value(seed: &mut u64, depth: u32) -> Value {
    let strings = ["", "plain", "Zebra", "apple", "tab\there", "quote\"s", "back\\slash", "line\nbreak", "\u{0}\u{1f}", "ключ", "日本語", "emoji \u{1F600}", "</script>", "é", "e\u{301}"];
    match rnd(seed) % if depth == 0 { 6 } else { 9 } {
        0 => Value::Null,
        1 => json!(rnd(seed).is_multiple_of(2)),
        2 => json!(rnd(seed) as i64),
        3 => json!(-((rnd(seed) % 1000) as i64)),
        4 => json!((rnd(seed) % 100000) as f64 / 7.0),
        5 => json!(strings[(rnd(seed) % strings.len() as u64) as usize]),
        6 | 7 => {
            let mut map = Map::new();
            for _ in 0..(rnd(seed) % 6) {
                let key = strings[(rnd(seed) % strings.len() as u64) as usize].to_owned() + &(rnd(seed) % 4).to_string();
                map.insert(key, value(seed, depth - 1));
            }
            Value::Object(map)
        }
        _ => Value::Array((0..(rnd(seed) % 5)).map(|_| value(seed, depth - 1)).collect()),
    }
}

#[test]
fn canonical_is_byte_for_byte_what_the_sorted_copy_serialized_to() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..2000 {
        let record = value(&mut seed, 4);
        assert_eq!(canonical(&record), reference(&record), "{record}");
    }
    for fixed in [
        json!({}),
        json!([]),
        json!({"b":{"z":1,"a":[{"y":2,"x":null}]},"a":"é","c":1.5,"d":1e21,"e":-0.0,"f":[true,false]}),
        json!("a bare string"),
        json!(12345678901234567890u64),
    ] {
        assert_eq!(canonical(&fixed), reference(&fixed), "{fixed}");
    }
}

fn fault(id: &str, extra: Value) -> Value {
    let mut record = json!({"v":3,"id":id,"kind":"fault","area":"git","title":id,"symptom":"x","status":"live",
        "created":"2026-10-24","updated":"2026-10-24","by":"me"});
    for (key, item) in extra.as_object().unwrap() {
        record[key] = item.clone();
    }
    record
}

#[test]
fn an_accepted_line_is_the_records_canonical_form() {
    let records = vec![fault("a-one", json!({"signals":["E_FAIL"],"refs":{"symbols":["x.y"]}})), fault("b-two", json!({"symptom":"ключ \"quoted\""}))];
    let bytes = serialize(&records);
    let lines = parse_lines(&bytes, "lore/git.jsonl").unwrap();
    assert_eq!(lines.len(), 2);
    for (record, line) in &lines {
        assert_eq!(*line, canonical(record));
    }
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert_eq!(lines.iter().map(|(_, line)| *line).collect::<Vec<_>>(), text.lines().collect::<Vec<_>>());
    assert_eq!(parse(&bytes, "x").unwrap(), lines.iter().map(|(record, _)| record.clone()).collect::<Vec<_>>());
    // The refusals are the same ones: a line that is not canonical is still named.
    let spaced = String::from_utf8(bytes).unwrap().replacen("\":\"", "\": \"", 1);
    let refused = parse_lines(spaced.as_bytes(), "lore/git.jsonl").unwrap_err().to_string();
    assert!(refused.contains("refused"), "{refused}");
}

fn repo(dir: &Path) {
    std::fs::create_dir_all(dir.join("lore")).unwrap();
    std::fs::write(dir.join("lore.toml"), "version = 1\nareas = [\"git\"]\n[embed]\nprovider = \"none\"\n").unwrap();
    let records = vec![fault("a-one", json!({"title":"first"})), fault("b-two", json!({"title":"second"}))];
    std::fs::write(dir.join("lore").join("git.jsonl"), serialize(&records)).unwrap();
}

#[test]
fn rebuild_discards_the_old_index_and_builds_the_one_new_one() -> Result<(), LoreError> {
    let cache = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::env::set_var("LORE_CACHE", cache.path());
    repo(work.path());
    let config = config::load(work.path())?;
    let built = index::open_index(&config)?;
    let path = built.path().to_path_buf();
    drop(built);
    // A mark only the old file has: gone after a rebuild, kept by a plain index.
    let mark = || rusqlite::Connection::open(&path).unwrap().execute("INSERT OR REPLACE INTO meta(key, value) VALUES('mark','old')", []).unwrap();
    let marked = || -> bool {
        rusqlite::Connection::open(&path).unwrap().query_row("SELECT count(*) FROM meta WHERE key='mark'", [], |row| row.get::<_, i64>(0)).unwrap() == 1
    };
    mark();
    let info = codelore::builtin::BUILTIN.index(&config, &[], false, true)?;
    assert_eq!(info.records, 2);
    assert!(marked(), "a plain `lore index` keeps the file");
    let info = codelore::builtin::BUILTIN.index(&config, &[], true, true)?;
    assert_eq!(info.records, 2);
    assert!(path.exists(), "the rebuilt index is where it was");
    assert!(!marked(), "`--rebuild` starts from nothing");
    // remove_indexes alone leaves no file for the next open to find.
    index::remove_indexes(work.path())?;
    for suffix in ["", "-wal", "-shm"] {
        assert!(!Path::new(&format!("{}{suffix}", path.display())).exists(), "{suffix}");
    }
    assert_eq!(index::open_index(&config)?.count()?, 2);
    Ok(())
}

#[test]
fn the_git_folder_is_found_only_when_the_iterator_reaches_it() {
    let work = tempfile::tempdir().unwrap();
    // Outside a repository: the user cache, then the temporary folder; `git`
    // is asked only when the iterator gets past the first.
    assert_eq!(index::index_dirs(work.path()).len(), 2);
    let first = index::index_dir_candidates(work.path()).next().unwrap();
    assert!(first.ends_with(index::repo_key(work.path())));
    let init = std::process::Command::new("git").arg("-C").arg(work.path()).args(["init", "-q"]).status();
    if init.map(|status| status.success()).unwrap_or(false) {
        let dirs = index::index_dirs(work.path());
        assert_eq!(dirs.len(), 3);
        assert!(dirs[2].starts_with(std::env::temp_dir()), "the temporary folder comes last");
        assert_eq!(dirs[0], first);
        assert!(dirs[1].ends_with("codelore") && dirs[1].to_string_lossy().contains(".git"), "{:?}", dirs[1]);
    }
}
