//! The code map (docs/ATLAS.md) through the command layer, in a scratch
//! repository: off unless lore.toml turns it on, refreshed from git's blob ids,
//! searchable, with its file graph meeting the lore's records in `impact`,
//! `drift` and `lore for`.
#![cfg(feature = "atlas")]
use codelore::cli::{self, Env};
use codelore::Store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

const CONFIG: &str = "version = 1\ndir = \"lore\"\nareas = [\"code\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n";

struct Repo {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn new(atlas: bool) -> Self {
        let tmp = tempfile::Builder::new().prefix("lore-atlas-").tempdir().unwrap();
        let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap()).join("repo");
        std::fs::create_dir_all(root.join("src/render")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "user.email", "t@example.invalid"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        let config = if atlas { format!("{CONFIG}\n[atlas]\nenabled = true\n") } else { CONFIG.to_owned() };
        std::fs::write(root.join("lore.toml"), config).unwrap();
        let files = [
            ("src/lib.rs", "pub mod render;\npub mod store;\n\n/// Opens the depth buffer for a frame.\npub fn open_depth_buffer() -> u32 {\n    render::post::chain()\n}\n"),
            ("src/render/mod.rs", "pub mod post;\n"),
            ("src/render/post.rs", "use crate::store::Cache;\n\n/// Runs the post-processing chain: bloom, tone mapping, the vignette.\npub fn chain() -> u32 {\n    Cache::warm();\n    7\n}\n"),
            ("src/store.rs", "pub struct Cache;\n\nimpl Cache {\n    /// Fills the shader cache before the first frame.\n    pub fn warm() {}\n}\n"),
            ("tools/report.py", "from helpers import table\n\ndef summarize_frames(frames):\n    return table(frames)\n"),
            ("tools/helpers.py", "def table(rows):\n    return len(rows)\n"),
        ];
        for (path, text) in files {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), text).unwrap();
        }
        std::fs::create_dir_all(root.join("lore")).unwrap();
        let store = Store::new(&root);
        store
            .write("lore/code.jsonl", &[json!({"v":3,"id":"post-chain-reads-a-cold-cache","kind":"fault","area":"code","title":"post chain reads a cold cache",
                "symptom":"the first frame is black","status":"live","created":"2026-11-08","updated":"2026-11-08","by":"me",
                "refs":{"files":["src/render/post.rs"],"symbols":["chain","vanished_helper"]}})])
            .unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "init"]);
        Self { _tmp: tmp, root }
    }

    fn run(&self, argv: &[&str]) -> cli::Output {
        let root = self.root.to_string_lossy().into_owned();
        let mut full = vec!["-C", root.as_str()];
        full.extend(argv);
        let env = Env { cwd: self.root.clone(), finder: Some(&codelore::builtin::BUILTIN), ..Env::default() };
        cli::run(&full, &env)
    }

    fn json(&self, argv: &[&str]) -> Value {
        let mut full = vec!["--json"];
        full.extend(argv);
        let out = self.run(&full);
        assert_eq!(out.code, 0, "lore {argv:?}: {}{}", out.stdout, out.stderr);
        serde_json::from_str(&out.stdout).unwrap()
    }
}

#[test]
fn the_map_is_off_unless_the_project_turns_it_on() {
    let repo = Repo::new(false);
    let out = repo.run(&["atlas", "search", "chain"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("[atlas] enabled = true"), "{}", out.stderr);
    // and `lore for` says nothing about code
    let out = repo.run(&["for", "src/render/post.rs"]);
    assert!(!out.stdout.contains("code:"), "{}", out.stdout);
}

#[test]
fn search_finds_pieces_by_words_terms_and_symbols() {
    let repo = Repo::new(true);
    let index = repo.json(&["atlas", "index"]);
    assert_eq!(index["counts"]["files"], 6, "{index}");
    let found = repo.json(&["atlas", "search", "tone mapping vignette"]);
    assert_eq!(found["results"][0]["path"], "src/render/post.rs", "{found}");
    let found = repo.json(&["atlas", "search", "open_depth_buffer"]);
    assert_eq!(found["results"][0]["path"], "src/lib.rs");
    assert!(found["results"][0]["via"].as_array().unwrap().contains(&json!("symbol")), "{found}");
    // identifiers are found by their words too
    let found = repo.json(&["atlas", "search", "summarize frames"]);
    assert_eq!(found["results"][0]["path"], "tools/report.py");
}

#[test]
fn the_graph_resolves_imports_and_meets_the_lore() {
    let repo = Repo::new(true);
    let deps = repo.json(&["atlas", "deps", "src/render/post.rs"]);
    assert_eq!(deps["imports"], json!(["src/store.rs"]));
    assert_eq!(deps["imported_by"], json!(["src/render/mod.rs"]));
    let deps = repo.json(&["atlas", "deps", "tools/report.py"]);
    assert_eq!(deps["imports"], json!(["tools/helpers.py"]));

    // a change to the cache reaches post.rs, whose fault the lore holds
    let impact = repo.json(&["atlas", "impact", "src/store.rs"]);
    let reached: Vec<&str> = impact["dependents"].as_array().unwrap().iter().map(|d| d["path"].as_str().unwrap()).collect();
    assert!(reached.contains(&"src/render/post.rs"), "{impact}");
    let ids: Vec<&str> = impact["records_of_dependents"].as_array().unwrap().iter().map(|r| r["record"]["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["post-chain-reads-a-cold-cache"]);

    let symbol = repo.json(&["atlas", "symbol", "warm"]);
    assert_eq!(symbol["definitions"][0]["path"], "src/store.rs");
    assert_eq!(symbol["mentioned_in"], json!(["src/render/post.rs"]));

    // `lore for` names the importers in text; its JSON keeps its shape
    let text = repo.run(&["for", "src/store.rs"]).stdout;
    assert!(text.contains("is imported by") && text.contains("post-chain-reads-a-cold-cache"), "{text}");
    let plain = repo.json(&["for", "src/store.rs"]);
    assert!(plain.is_array(), "{plain}");
}

#[test]
fn a_change_in_the_working_tree_is_read_again_and_drift_is_named() {
    let repo = Repo::new(true);
    repo.json(&["atlas", "index"]);
    let drift = repo.json(&["atlas", "drift"]);
    assert_eq!(drift[0]["missing_symbols"], json!(["vanished_helper"]), "{drift}");

    // rename the function the record cites, uncommitted
    std::fs::write(repo.root.join("src/render/post.rs"), "use crate::store::Cache;\n\npub fn run_chain() -> u32 {\n    Cache::warm();\n    7\n}\n").unwrap();
    let lib = std::fs::read_to_string(repo.root.join("src/lib.rs")).unwrap().replace("post::chain()", "post::run_chain()");
    std::fs::write(repo.root.join("src/lib.rs"), lib).unwrap();
    let status = repo.json(&["atlas", "status"]);
    assert_eq!(status["refreshed"]["files_read"], 2, "{status}");
    let drift = repo.json(&["atlas", "drift"]);
    assert_eq!(drift[0]["missing_symbols"], json!(["chain", "vanished_helper"]), "{drift}");
    let found = repo.json(&["atlas", "search", "run_chain"]);
    assert_eq!(found["results"][0]["path"], "src/render/post.rs");

    // a new import cycle is seen
    std::fs::write(repo.root.join("src/store.rs"), "use crate::render::post::run_chain;\npub struct Cache;\nimpl Cache { pub fn warm() {} }\n").unwrap();
    let cycles = repo.json(&["atlas", "cycles"]);
    assert_eq!(cycles, json!([["src/render/post.rs", "src/store.rs"]]));
}

/// A search skips the scan while the quick print (index file, `git diff-files`,
/// folder times) is unchanged; every kind of change must still be seen.
#[test]
fn the_quick_freshness_check_misses_no_change() {
    let repo = Repo::new(true);
    repo.json(&["atlas", "index"]);
    let first = |query: &str| repo.json(&["atlas", "search", query])["results"][0]["path"].as_str().unwrap_or("").to_owned();
    let search_twice = |query: &str| {
        let a = first(query);
        assert_eq!(a, first(query), "a second search of {query} answers the same");
        a
    };
    assert_eq!(search_twice("open_depth_buffer"), "src/lib.rs");

    // a new file beside tracked ones
    std::fs::write(repo.root.join("src/render/glow.rs"), "pub fn glow_radius() -> u32 { 3 }\n").unwrap();
    assert_eq!(search_twice("glow_radius"), "src/render/glow.rs");
    // a new file in a new folder
    std::fs::create_dir_all(repo.root.join("src/audio/mix")).unwrap();
    std::fs::write(repo.root.join("src/audio/mix/bus.rs"), "pub fn duck_the_music() {}\n").unwrap();
    assert_eq!(search_twice("duck_the_music"), "src/audio/mix/bus.rs");
    // a second edit to a file already modified (the diff-files list stays the same)
    std::fs::write(repo.root.join("src/store.rs"), "pub struct Cache;\nimpl Cache { pub fn warm_slowly() {} }\n").unwrap();
    assert_eq!(search_twice("warm_slowly"), "src/store.rs");
    std::fs::write(repo.root.join("src/store.rs"), "pub struct Cache;\nimpl Cache { pub fn warm_quickly_now() {} }\n").unwrap();
    assert_eq!(search_twice("warm_quickly_now"), "src/store.rs");
    // a new file in an untracked folder seen before
    std::fs::write(repo.root.join("src/audio/mix/fade.rs"), "pub fn fade_out_slowly() {}\n").unwrap();
    assert_eq!(search_twice("fade_out_slowly"), "src/audio/mix/fade.rs");
    // staged and committed: the index file changes
    git(&repo.root, &["add", "-A"]);
    git(&repo.root, &["commit", "-qm", "two"]);
    assert_eq!(search_twice("glow_radius"), "src/render/glow.rs");
    // removed
    std::fs::remove_file(repo.root.join("src/render/glow.rs")).unwrap();
    assert_ne!(first("glow_radius"), "src/render/glow.rs");
}

#[test]
fn an_unknown_table_is_a_warning_not_a_refusal() {
    let repo = Repo::new(true);
    let config = std::fs::read_to_string(repo.root.join("lore.toml")).unwrap();
    std::fs::write(repo.root.join("lore.toml"), format!("{config}\n[from_the_future]\nx = 1\n")).unwrap();
    let out = repo.run(&["--json", "find", "cold cache"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("unknown key `from_the_future` is ignored"), "{}", out.stderr);
    serde_json::from_str::<Value>(&out.stdout).expect("stdout stays one JSON document");
}
