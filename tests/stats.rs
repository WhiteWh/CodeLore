//! Task 149: `lore find` records counters (no text), `lore show` gives the hit
//! position, a new fault after an unopened find is a miss, and `lore stats`
//! prints the mix.
use serde_json::{json, Value};
use std::path::Path;
use codelore::builtin::BUILTIN;
use codelore::cli::{self, Env};

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn run(root: &Path, argv: &[&str], stdin: &str) -> cli::Output {
    let mut full = vec!["-C", root.to_str().unwrap()];
    full.extend(argv);
    let env = Env {
        cwd: root.to_path_buf(),
        store: None,
        stdin: stdin.as_bytes().to_vec(),
        lore_by: Some("tester".into()),
        machine: None,
        finder: Some(&BUILTIN),
        roots: None,
        now: None,
        history: None,
        history_scope: Default::default(),
        doorbell: None,
        clock: None,
    };
    cli::run(&full, &env)
}

fn json(root: &Path, argv: &[&str], stdin: &str) -> Value {
    let out = run(root, argv, stdin);
    assert_eq!(out.code, 0, "lore {argv:?}: {}{}", out.stdout, out.stderr);
    serde_json::from_str(&out.stdout).unwrap_or_else(|error| panic!("not JSON: {error}: {}", out.stdout))
}

#[test]
fn the_counters_and_lore_stats_work_without_any_query_text() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "tester"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    std::fs::write(root.join("README.md"), "# fixture\n").unwrap();
    git(&root, &["add", "README.md"]);
    git(&root, &["commit", "-qm", "fixture"]);
    let cache = base.path().join("cache");
    let stats_db = base.path().join("stats.sqlite");
    std::env::set_var("LORE_CACHE", &cache);
    std::env::set_var("LORE_STATS_DB", &stats_db);
    std::env::set_var("LORE_SESSION", "stats-test-session");
    std::env::set_var("LORE_CALLER", "member:codex");
    assert_eq!(run(&root, &["init", "--areas", "general"], "").code, 0);
    let fault = json!({
        "kind": "fault", "area": "general",
        "title": "the atlas checksum mismatch",
        "symptom": "the nightly staging step aborts with a checksum mismatch on a texture atlas",
        "cause": "a half-written atlas from an interrupted run is reused because its stamp file survived",
        "fix": "delete the stale stamp file and re-import the atlas whole",
        "check": "the staging step finishes and the atlas hash matches the manifest",
    });
    let added = json(&root, &["add", "fault", "--stdin", "--new", "--json"], &fault.to_string());
    let id = added["records"][0]["id"].as_str().unwrap().to_owned();
    // This exact query is a paraphrase and finds the record; showing it gives
    // the hit position.
    let found = json(&root, &["find", "atlas checksum mismatch", "--json", "--text-only"], "");
    assert_eq!(found["results"][0]["record"]["id"], id, "{found}");
    assert_eq!(run(&root, &["show", &id, "--json"], "").code, 0);
    let stats = json(&root, &["stats", "--json"], "");
    assert_eq!(stats["finds"], 1, "{stats}");
    assert_eq!(stats["by_class"]["paraphrase"]["finds"], 1, "{stats}");
    assert_eq!(stats["by_class"]["paraphrase"]["hit1"], 1, "{stats}");
    assert_eq!(stats["by_caller"]["member:codex"]["finds"], 1, "{stats}");
    assert_eq!(stats["tiers"]["words"], 1, "{stats}");
    assert_eq!(stats["misses"], 0, "{stats}");
    // An exact query with no result, then a new fault: a miss.
    let exact = "0x80070005";
    let empty = json(&root, &["find", exact, "--json", "--text-only"], "");
    assert_eq!(empty["results"].as_array().unwrap().len(), 0, "{empty}");
    let second = json!({
        "kind": "fault", "area": "general", "title": "the device was lost",
        "symptom": "a D3D device removed", "cause": "the driver reset", "fix": "reopen the device", "check": "frames render",
    });
    assert_eq!(run(&root, &["add", "fault", "--stdin", "--new", "--json"], &second.to_string()).code, 0);
    let stats = json(&root, &["stats", "--json"], "");
    assert_eq!(stats["by_class"]["exact"]["finds"], 1, "{stats}");
    assert_eq!(stats["misses"], 1, "{stats}");
    assert_eq!(stats["finds"], 2, "{stats}");
    // Neither the query text nor the record text is in the counters file.
    let db = codelore::stats::open_at(&stats_db).expect("the counters file opens");
    let mut leaks = 0;
    let tables: Vec<String> = {
        let mut statement = db.prepare("SELECT name FROM sqlite_master WHERE type='table'").unwrap();
        statement.query_map([], |row| row.get(0)).unwrap().flatten().collect()
    };
    for table in tables {
        let mut statement = db.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                let mut values = Vec::new();
                for index in 0..columns {
                    values.push(row.get::<_, Option<String>>(index).ok().flatten().unwrap_or_default());
                }
                Ok(values)
            })
            .unwrap();
        for row in rows.flatten() {
            for value in row {
                if value.contains("atlas checksum") || value.contains("0x80070005") || value.contains("texture atlas") {
                    leaks += 1;
                }
            }
        }
    }
    assert_eq!(leaks, 0, "the counters hold query or record text");
    std::env::remove_var("LORE_CACHE");
    std::env::remove_var("LORE_STATS_DB");
    std::env::remove_var("LORE_SESSION");
    std::env::remove_var("LORE_CALLER");
}
