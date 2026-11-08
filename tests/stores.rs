//! `find --stores`: a store with sub-stores of its own (a Zerus project store
//! and its `teams/<team>`) searched as one, each result marked with its store;
//! without the option `find` is what it was.
use codelore::cli::{self, Env};
use codelore::Store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const CONFIG: &str = "version = 1\ndir = \"lore\"\nareas = [\"git\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n";

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn fault(id: &str, title: &str) -> Value {
    json!({"v":3,"id":id,"kind":"fault","area":"git","title":title,"symptom":title,"status":"live",
        "created":"2026-11-08","updated":"2026-11-08","by":"me"})
}

fn store(dir: &Path, records: &[Value]) {
    std::fs::create_dir_all(dir.join("lore")).unwrap();
    std::fs::write(dir.join("lore.toml"), CONFIG).unwrap();
    Store::new(dir).write("lore/git.jsonl", records).unwrap();
}

struct Stores {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Stores {
    fn new() -> Self {
        let tmp = tempfile::Builder::new().prefix("lore-stores-").tempdir().unwrap();
        let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap()).join("project");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        store(&root, &[fault("root-push-rejected", "push rejected by the remote hook")]);
        store(&root.join("teams").join("aaaa1111"), &[fault("team-push-retried", "push retried after the remote hook refused")]);
        store(&root.join("teams").join("bbbb2222"), &[fault("other-team-cache", "the shader cache is cold")]);
        Self { _tmp: tmp, root }
    }

    fn find(&self, extra: &[&str]) -> Value {
        let root = self.root.to_string_lossy().into_owned();
        let mut argv = vec!["-C", root.as_str(), "--json", "find", "push remote hook"];
        argv.extend(extra);
        let env = Env { cwd: self.root.clone(), finder: Some(&codelore::builtin::BUILTIN), ..Env::default() };
        let out = cli::run(&argv, &env);
        assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
        serde_json::from_str(&out.stdout).unwrap()
    }
}

fn found(value: &Value) -> Vec<(Option<String>, String)> {
    value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| (result.get("store").and_then(Value::as_str).map(str::to_owned), result["record"]["id"].as_str().unwrap().to_owned()))
        .collect()
}

#[test]
fn without_stores_find_searches_its_own_store_and_marks_nothing() {
    let stores = Stores::new();
    assert_eq!(found(&stores.find(&[])), [(None, "root-push-rejected".to_owned())]);
    assert_eq!(found(&stores.find(&["--stores", "root"])), [(None, "root-push-rejected".to_owned())]);
}

#[test]
fn all_searches_every_sub_store_and_names_each_result_s_store() {
    let stores = Stores::new();
    let mut results = found(&stores.find(&["--stores", "all"]));
    results.sort();
    assert_eq!(results, [(Some(".".to_owned()), "root-push-rejected".to_owned()), (Some("teams/aaaa1111".to_owned()), "team-push-retried".to_owned())]);
}

#[test]
fn named_stores_are_searched_alone_and_a_folder_without_lore_toml_is_refused() {
    let stores = Stores::new();
    assert_eq!(found(&stores.find(&["--stores", "teams/aaaa1111"])), [(Some("teams/aaaa1111".to_owned()), "team-push-retried".to_owned())]);
    let root = stores.root.to_string_lossy().into_owned();
    let env = Env { cwd: stores.root.clone(), finder: Some(&codelore::builtin::BUILTIN), ..Env::default() };
    let out = cli::run(&["-C", root.as_str(), "find", "x", "--stores", "teams"], &env);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("teams has no lore.toml"), "{}", out.stderr);
}
