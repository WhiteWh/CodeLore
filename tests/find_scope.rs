//! A narrow search finds what is in its scope however many better matches lie
//! outside it: the scope is applied while the text index ranks, not after a
//! fixed number of its best rows.
use codelore::cli::{self, Env};
use codelore::Store;
use serde_json::{json, Value};

#[test]
fn an_area_finds_its_record_below_hundreds_of_better_ones_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap());
    std::fs::write(root.join("lore.toml"), "version = 1\ndir = \"lore\"\nareas = [\"noise\", \"core\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n").unwrap();
    std::fs::create_dir_all(root.join("lore")).unwrap();
    let record = |id: String, area: &str, title: &str, symptom: &str| {
        json!({"v": 3, "id": id, "kind": "fault", "area": area, "title": title, "symptom": symptom, "cause": "c", "fix": "f", "check": "k",
               "status": "live", "created": "2026-11-08", "updated": "2026-11-08", "by": "me"})
    };
    let noise: Vec<Value> = (0..250).map(|at| record(format!("noise-{at:03}"), "noise", "shader cache warm shader cache warm", "the shader cache is warm and the shader cache stays warm")).collect();
    let store = Store::new(&root);
    store.write("lore/noise.jsonl", &noise).unwrap();
    store.write("lore/core.jsonl", &[record("core-one".into(), "core", "a frame waits on the disk", "the shader cache is read late")]).unwrap();
    let env = Env { cwd: root.clone(), finder: Some(&codelore::builtin::BUILTIN), ..Env::default() };
    let out = cli::run(&["--json", "find", "shader cache warm", "--area", "core", "--text-only"], &env);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let found: Value = serde_json::from_str(&out.stdout).unwrap();
    let ids: Vec<&str> = found["results"].as_array().or_else(|| found.as_array()).unwrap().iter().filter_map(|hit| hit["id"].as_str().or_else(|| hit["record"]["id"].as_str())).collect();
    assert_eq!(ids, ["core-one"], "{found}");
}

#[test]
fn a_path_no_record_carries_goes_on_to_meaning() {
    let tmp = tempfile::tempdir().unwrap();
    let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap());
    std::fs::write(root.join("lore.toml"), "version = 1\ndir = \"lore\"\nareas = [\"core\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n").unwrap();
    std::fs::create_dir_all(root.join("lore")).unwrap();
    let record = json!({"v": 3, "id": "src-words", "kind": "fault", "area": "core", "title": "the src folder holds rs files", "symptom": "src and rs everywhere",
        "cause": "c", "fix": "f", "check": "k", "status": "live", "created": "2026-11-08", "updated": "2026-11-08", "by": "me"});
    Store::new(&root).write("lore/core.jsonl", &[record]).unwrap();
    let env = Env { cwd: root.clone(), finder: Some(&codelore::builtin::BUILTIN), ..Env::default() };
    let notes = |query: &str| -> String {
        let out = cli::run(&["--json", "find", query], &env);
        assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
        serde_json::from_str::<Value>(&out.stdout).unwrap()["notes"].to_string()
    };
    // The words "src" and "rs" match; the path itself is in no record: meaning is asked.
    assert!(notes("src/nowhere/missing_fn.rs").contains("semantic search is off"), "the path went on to meaning");
    // A path a record carries whole is answered by the text.
    let record = json!({"v": 3, "id": "post-chain", "kind": "fault", "area": "core", "title": "post chain", "symptom": "src/render/post_chain.rs breaks",
        "cause": "c", "fix": "f", "check": "k", "status": "live", "created": "2026-11-08", "updated": "2026-11-08", "by": "me"});
    Store::new(&root).write("lore/core.jsonl", &[record]).unwrap();
    assert!(!notes("src/render/post_chain.rs").contains("semantic search is off"), "a whole term answers at once");
}
