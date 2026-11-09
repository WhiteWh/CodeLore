//! CodeLore 0.3.0's whole Python test suite (D:/Tools/CodeLore/tests), ported
//! test for test on the same inputs: `RepoCase` makes the same scratch
//! repository and drives `cli::run` as the Python tests drive `cli.main`.
//! Each Rust test is named `<file>__<class>__<test>`.
//!
//! `find` ranks through the index, which plugs in as a `Finder` (`finder()`
//! below). Until it is in this crate, a test whose subject is ranking is
//! ignored with that reason, and a test that only touches `find` in passing
//! runs everything else and says which assertion waits.
#![allow(non_snake_case)]

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use codelore::cli::{self, Env};
use codelore::search::Finder;
use codelore::store::{canonical, parse, serialize};
use codelore::{gitio, schema, FORMAT_VERSION};

/// The ranked search, once the index is in this crate.
fn finder() -> Option<&'static dyn Finder> {
    Some(&codelore::builtin::BUILTIN)
}

const WAITS: &str = "waits for the FTS5 index (find ranks through it)";

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

const CONFIG: &str = "version = 1\ndir = \"lore\"\nareas = [\"git\", \"build\", \"render\"]\n\n[commit]\nmode = \"{mode}\"\nbranch = \"main\"\n\n[embed]\nprovider = \"none\"\n";

struct Case {
    tmp: tempfile::TempDir,
    root: PathBuf,
    c0: String,
}

impl Case {
    fn new(mode: &str) -> Self {
        let tmp = tempfile::Builder::new().prefix("lore-test-").tempdir().unwrap();
        let base = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap());
        let root = base.join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "tester"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("src").join("chain.py"), "x = 1\n").unwrap();
        std::fs::write(root.join("lore.toml"), CONFIG.replace("{mode}", mode)).unwrap();
        std::fs::write(root.join(".gitattributes"), "lore/** text eol=lf\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "init"]);
        let c0 = git(&root, &["rev-parse", "HEAD"]);
        Self { tmp, root, c0 }
    }

    fn tmp(&self) -> PathBuf {
        codelore::config::simplify(&std::fs::canonicalize(self.tmp.path()).unwrap())
    }

    fn run(&self, argv: &[&str], stdin: &str) -> cli::Output {
        let root = self.root.to_string_lossy().into_owned();
        let mut full = vec!["-C", root.as_str()];
        full.extend(argv);
        let env = Env { cwd: self.root.clone(), stdin: stdin.as_bytes().to_vec(), lore_by: None, finder: finder(), roots: None, ..Env::default() };
        cli::run(&full, &env)
    }

    /// `lore ...`: the text it prints; it must succeed.
    fn lore(&self, argv: &[&str]) -> String {
        self.lore_in(argv, "")
    }

    fn lore_in(&self, argv: &[&str], stdin: &str) -> String {
        let out = self.run(argv, stdin);
        assert_eq!(out.code, 0, "lore {} exited {}:\n{}", argv.join(" "), out.code, out.text());
        out.text()
    }

    /// `lore ...` that must refuse; the text it prints.
    fn refuse(&self, argv: &[&str]) -> String {
        self.refuse_in(argv, "")
    }

    fn refuse_in(&self, argv: &[&str], stdin: &str) -> String {
        let out = self.run(argv, stdin);
        assert_ne!(out.code, 0, "lore {} should have refused:\n{}", argv.join(" "), out.text());
        out.text()
    }

    fn json(&self, argv: &[&str]) -> Value {
        self.json_in(argv, "")
    }

    fn json_in(&self, argv: &[&str], stdin: &str) -> Value {
        let mut full = vec!["--json"];
        full.extend(argv);
        let out = self.run(&full, stdin);
        assert_eq!(out.code, 0, "lore --json {} exited {}:\n{}", argv.join(" "), out.code, out.text());
        serde_json::from_str(&out.stdout).unwrap_or_else(|error| panic!("{error}: {}", out.stdout))
    }

    /// `find`'s ids, or None while there is no index.
    fn find_ids(&self, argv: &[&str]) -> Option<Vec<String>> {
        finder()?;
        let mut full = vec!["find"];
        full.extend(argv);
        Some(self.json(&full)["results"].as_array().unwrap().iter().map(|hit| hit["record"]["id"].as_str().unwrap().to_owned()).collect())
    }

    fn add(&self, records: &[Value]) -> Value {
        self.json_in(&["add", "any", "--stdin", "--new"], &Value::Array(records.to_vec()).to_string())
    }

    fn head(&self) -> String {
        git(&self.root, &["rev-parse", "HEAD"])
    }

    fn file(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

/// CodeLore's `RepoCase.fault`: kind, area, title, symptom, cause, fix, check,
/// status, then the extra fields.
fn fault(title: &str, extra: Value) -> Value {
    let mut extra = extra.as_object().cloned().unwrap_or_default();
    let mut record = Map::new();
    record.insert("kind".into(), json!("fault"));
    record.insert("area".into(), json!("git"));
    record.insert("title".into(), json!(title));
    record.insert("symptom".into(), extra.remove("symptom").unwrap_or_else(|| json!(format!("the symptom of {title}"))));
    record.insert("cause".into(), extra.remove("cause").unwrap_or(json!("a cause")));
    record.insert("fix".into(), extra.remove("fix").unwrap_or(json!("a fix")));
    record.insert("check".into(), extra.remove("check").unwrap_or(json!("a check")));
    record.insert("status".into(), json!("live"));
    for (key, value) in extra {
        record.insert(key, value);
    }
    Value::Object(record)
}

fn f(title: &str) -> Value {
    fault(title, json!({}))
}

fn ids(value: &Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|record| record["id"].as_str().unwrap().to_owned()).collect()
}

fn has(errors: &[String], words: &str) -> bool {
    errors.iter().any(|error| error.contains(words))
}

// ============================ test_store_schema.py ============================

fn schema_fault(extra: Value) -> Value {
    let mut record = json!({"v": 1, "id": "tar-pipe", "kind": "fault", "area": "git", "title": "t", "status": "live",
        "created": "2026-10-24", "updated": "2026-10-24", "by": "me", "symptom": "s", "cause": "c", "fix": "f", "check": "k"});
    for (key, value) in extra.as_object().unwrap() {
        if value.is_null() {
            record.as_object_mut().unwrap().remove(key);
        } else {
            record[key] = value.clone();
        }
    }
    record
}

fn schema_item(n: u32, extra: Value) -> Value {
    let mut record = json!({"v": 1, "id": format!("pl#{n}"), "kind": "item", "area": "plans", "plan": "pl", "title": format!("i{n}"),
        "status": "open", "created": "2026-10-24", "updated": "2026-10-24", "by": "me"});
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    record
}

fn areas(list: &[&str]) -> Vec<String> {
    list.iter().map(|area| (*area).to_owned()).collect()
}

#[test]
fn store_schema__Store__round_trip_keeps_bytes_and_unknown_fields() {
    let records = vec![schema_fault(json!({"id": "b-one", "extra": {"x": [1, "ü"]}})), schema_fault(json!({"id": "a-two"}))];
    let data = serialize(&records);
    let back = parse(&data, "x.jsonl").unwrap();
    assert_eq!(serialize(&back), data);
    assert_eq!(back[1]["extra"], json!({"x": [1, "ü"]}));
    assert!(data.windows(2).any(|pair| pair == "ü".as_bytes()));
}

#[test]
fn store_schema__Store__items_sort_by_number() {
    let plan = json!({"v": 1, "id": "pl", "kind": "plan", "area": "plans", "title": "p", "goal": "g",
        "status": "live", "created": "2026-10-24", "updated": "2026-10-24", "by": "me"});
    let data = serialize(&[schema_item(10, json!({})), schema_item(9, json!({})), plan, schema_item(2, json!({}))]);
    let found: Vec<String> = parse(&data, "p.jsonl").unwrap().iter().map(|record| record["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(found, ["pl", "pl#2", "pl#9", "pl#10"]);
}

fn refuses(data: &[u8], words: &str) {
    let error = parse(data, "x.jsonl").unwrap_err().to_string();
    assert!(error.contains(words), "{words:?} not in {error}");
}

#[test]
fn store_schema__Store__refusals() {
    let good = serialize(&[schema_fault(json!({"id": "aa"})), schema_fault(json!({"id": "bb"}))]);
    let text = String::from_utf8(good.clone()).unwrap();
    refuses(text.replace('\n', "\r\n").as_bytes(), "CR line ends");
    refuses(&[&[0xEF, 0xBB, 0xBF][..], &good].concat(), "BOM");
    let lines: Vec<&str> = text.split('\n').collect();
    refuses(format!("{}\n{}\n", lines[1], lines[0]).as_bytes(), "out of order");
    refuses(format!("{}\n\n{}\n", lines[0], lines[1]).as_bytes(), "blank");
    refuses(text.trim_end_matches('\n').as_bytes(), "no final newline");
    refuses(format!("{}\n{}\n", lines[0], lines[0]).as_bytes(), "twice");
    let spaced = canonical(&schema_fault(json!({"id": "aa"}))).replacen("\":\"", "\": \"", 1);
    refuses(format!("{spaced}\n").as_bytes(), "not canonical");
    refuses(b"[1]\n", "not a JSON object");
}

#[test]
fn store_schema__Schema__valid() {
    assert!(schema::validate(&schema_fault(json!({})), &areas(&["git"]), None).is_empty());
}

#[test]
fn store_schema__Schema__each_rule_refuses() {
    let cases = [
        (schema_fault(json!({"symptom": null})), "`symptom` is required"),
        (schema_fault(json!({"status": "fixed"})), "`status` must be one of"),
        (schema_fault(json!({"area": "nowhere"})), "is not one of lore.toml's areas"),
        (schema_fault(json!({"id": "Bad Id"})), "`id` must match"),
        (schema_fault(json!({"title": "x".repeat(121)})), "the limit is 120"),
        (schema_fault(json!({"cause": "x".repeat(601)})), "the story goes in the commit"),
        (schema_fault(json!({"signals": "one"})), "list of non-empty strings"),
        (schema_fault(json!({"signals": ["a", "a"]})), "duplicates"),
        (schema_fault(json!({"created": "2026-13-01"})), "must be a date"),
        (schema_fault(json!({"created": "2026-10-25"})), "`updated` is before `created`"),
        (schema_fault(json!({"v": FORMAT_VERSION + 1})), "newer than this lore reads"),
        (schema_fault(json!({"refs": {"files": ["C:\\x"]}})), "paths are relative"),
        (schema_fault(json!({"refs": {"commits": ["zz"]}})), "not a hash"),
        (schema_fault(json!({"refs": {"other": ["x"]}})), "is not one of"),
        (schema_fault(json!({"kind": "nope"})), "`kind` must be one of"),
        (schema_item(1, json!({"status": "done"})), "`commits` is required when `done`"),
        (schema_item(1, json!({"status": "dropped", "reason": "r"})), "`decided_by` is required when `dropped`"),
        (schema_item(1, json!({"status": "dropped", "decided_by": "me"})), "`reason` is required when `dropped`"),
        (schema_item(1, json!({"reason": "r"})), "belongs to status"),
        (schema_item(1, json!({"id": "ql#1"})), "names plan 'ql'"),
        (schema_item(1, json!({"deps": [{"item": "pl#2"}]})), "each dependency is"),
        (schema_item(1, json!({"deps": [{"item": "pl#1", "kind": "hard"}]})), "depends on itself"),
        (schema_item(1, json!({"area": "git"})), "a item's `area` is `plans`"),
    ];
    for (record, words) in cases {
        let errors = schema::validate(&record, &areas(&["git"]), None);
        assert!(has(&errors, words), "{words:?} not in {errors:?}");
    }
}

#[test]
fn store_schema__Schema__limits_only_lower() {
    let limits = [("fault.cause".to_owned(), 10usize)].into();
    assert!(has(&schema::validate(&schema_fault(json!({"cause": "x".repeat(11)})), &areas(&["git"]), Some(&limits)), "the limit is 10"));
    assert!(schema::check_limits(&[("fault.cause".to_owned(), 5000usize)].into()).is_err());
    assert!(schema::check_limits(&[("fault.nope".to_owned(), 5usize)].into()).is_err());
    schema::check_limits(&[("fault.cause".to_owned(), 100usize)].into()).unwrap();
}

#[test]
fn store_schema__Config__refusals() {
    let data: toml::Value = toml::from_str("version = 2\nareas = [\"Plans\", \"plans\"]\nx = 1\n[commit]\nmode = \"yolo\"\n").unwrap();
    let text = codelore::config::parse(&data, Path::new("."), "lore.toml").unwrap_err().to_string();
    // `x`, unknown at the top level, is a warning (a newer lore's key), not a refusal.
    for words in ["version", "must match", "reserved", "commit.mode"] {
        assert!(text.contains(words), "{words:?} not in {text}");
    }
}

// ================================ test_write.py ================================

#[test]
fn write__Plain__commits_only_its_file_while_another_is_staged() {
    let case = Case::new("plain");
    std::fs::write(case.file("theirs.txt"), "theirs\n").unwrap();
    git(&case.root, &["add", "theirs.txt"]);
    let out = case.add(&[f("tar pipe")]);
    assert!(out["commit"].is_string());
    assert_eq!(git(&case.root, &["show", "--name-only", "--format=", "HEAD"]), "lore/git.jsonl");
    assert!(git(&case.root, &["diff", "--cached", "--name-only"]).contains("theirs.txt"));
}

#[test]
fn write__Plain__refuses_and_writes_nothing() {
    let case = Case::new("plain");
    let before = case.head();
    case.refuse_in(&["add", "any", "--stdin", "--new"], &fault("bad", json!({"refs": {"commits": ["deadbeef"]}})).to_string());
    case.refuse_in(&["add", "any", "--stdin", "--new"], &fault("bad", json!({"refs": {"files": ["src/missing.py"]}})).to_string());
    assert_eq!(case.head(), before);
    assert!(!case.file("lore/git.jsonl").exists());
}

#[test]
fn write__Plain__ids_are_made_and_unique() {
    let case = Case::new("plain");
    let a = case.add(&[f("Git archive pipe!")])["records"][0]["id"].as_str().unwrap().to_owned();
    let b = case.add(&[f("git archive pipe")])["records"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(a, "git-archive-pipe");
    assert_eq!(b, "git-archive-pipe-2");
    case.refuse_in(&["add", "any", "--stdin", "--new"], &fault("x", json!({"id": a})).to_string());
}

#[test]
fn write__Plain__references_that_exist_pass() {
    let case = Case::new("plain");
    let r = case.add(&[fault("good", json!({"refs": {"commits": [&case.c0[..8]], "files": ["src/chain.py"]}}))]);
    assert!(r["commit"].is_string());
}

#[test]
fn write__Plain__update_and_kind_is_fixed() {
    let case = Case::new("plain");
    let rid = case.add(&[f("one")])["records"][0]["id"].as_str().unwrap().to_owned();
    case.lore(&["update", &rid, "-s", "fix=a better fix", "-s", "signals+=E_FAIL"]);
    let r = case.json(&["show", &rid]);
    assert_eq!((r["fix"].as_str().unwrap(), r["signals"].clone()), ("a better fix", json!(["E_FAIL"])));
    case.refuse(&["update", &rid, "-s", "kind=reading"]);
    case.refuse(&["update", &rid, "-s", "area=build"]);
}

#[test]
fn write__Plain__stdin_is_utf8_whatever_the_console() {
    // Python decodes stdin as a cp1252 console would; the Rust call takes the
    // bytes and reads them as UTF-8, which is what the test holds it to.
    let case = Case::new("plain");
    let record = fault("quotes «patched» and an ellipsis…", json!({"symptom": "it prints «patched» and stops…"}));
    let out = case.json_in(&["add", "any", "--stdin", "--new"], &record.to_string());
    let r = case.json(&["show", out["records"][0]["id"].as_str().unwrap()]);
    assert_eq!(r["symptom"], "it prints «patched» and stops…");
    assert!(!std::fs::read_to_string(case.file("lore/git.jsonl")).unwrap().contains('Â'));
}

#[test]
fn write__Plain__update_batch_is_one_commit_and_a_no_op_is_none() {
    let case = Case::new("plain");
    let a = case.add(&[f("one")])["records"][0]["id"].as_str().unwrap().to_owned();
    let b = case.add(&[f("two")])["records"][0]["id"].as_str().unwrap().to_owned();
    let before = case.head();
    let batch = format!("{}\n{}", json!({"id": a, "fix": "fix one"}), json!({"id": b, "fix": "fix two"}));
    case.lore_in(&["update", "--batch"], &format!("{batch}\n{}", json!({"id": a})));
    assert_eq!(git(&case.root, &["rev-list", "--count", &format!("{before}..HEAD")]), "1");
    assert_eq!(git(&case.root, &["log", "-1", "--format=%s"]), "lore: 2 records (fault)");
    assert_eq!(case.json(&["show", &b])["fix"], "fix two");
    let same = case.head();
    let out = case.json_in(&["update", "--batch"], &batch);
    assert!(out["commit"].is_null());
    assert_eq!(case.head(), same);
    case.refuse_in(&["update", "--batch"], &json!({"id": "nope", "fix": "x"}).to_string());
}

#[test]
fn write__Plain__duplicate_search_refuses_without_new() {
    let case = Case::new("plain");
    case.add(&[fault("git archive piped into tar fails", json!({"symptom": "extracting a git archive through a pipe fails on every file"}))]);
    let text = case.refuse_in(&["add", "any", "--stdin"], &fault("git archive piped to tar fails", json!({"symptom": "extracting the git archive through a pipe fails on each file"})).to_string());
    assert!(text.contains("1 of 1 record(s) already exist"), "{text}");
    assert!(text.contains("git-archive-piped-into-tar-fails (words"), "{text}");
}

#[test]
fn write__Plain__shared_signal_refuses_and_a_neighbour_is_only_named() {
    let case = Case::new("plain");
    case.add(&[fault("device lost", json!({"signals": ["ERR_DEVICE_LOST"], "symptom": "every frame reads the same stale picture after a while"}))]);
    let text = case.refuse_in(&["add", "any", "--stdin"], &fault("a different title", json!({"symptom": "nothing alike", "signals": ["err_device_lost"]})).to_string());
    assert!(text.contains("(signal 1.0)"), "{text}");
    let out = case.json_in(&["add", "any", "--stdin"], &fault("a stale frame after the device stopped", json!({"symptom": "the picture stays old, other cause"})).to_string());
    assert!(out["commit"].is_string());
    let notes: Vec<&str> = out["notes"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(notes.iter().any(|note| note.contains("is near device-lost")), "{notes:?}");
}

#[test]
fn write__Shared__foreign_live_edit_stays_out_and_in_place() {
    let case = Case::new("shared-tree");
    case.add(&[f("first")]);
    let live = case.file("lore/git.jsonl");
    let records = parse(&std::fs::read(&live).unwrap(), "x").unwrap();
    let mut other = records[0].clone();
    other["id"] = json!("zz-someone");
    other["title"] = json!("someone else's uncommitted record");
    let foreign = serialize(&[records, vec![other]].concat());
    std::fs::write(&live, &foreign).unwrap();
    let index_before = git(&case.root, &["ls-files", "-s"]);
    let out = case.add(&[f("second")]);
    assert!(out["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().contains("left as it was")));
    assert_eq!(std::fs::read(&live).unwrap(), foreign);
    let blob = git(&case.root, &["show", "HEAD:lore/git.jsonl"]);
    assert!(!blob.contains("zz-someone"));
    assert!(blob.contains("\"second\""));
    assert_eq!(git(&case.root, &["ls-files", "-s"]), index_before);
}

#[test]
fn write__Shared__shared_index_untouched_for_other_paths() {
    let case = Case::new("shared-tree");
    std::fs::write(case.file("src/chain.py"), "x = 2\n").unwrap();
    git(&case.root, &["add", "src/chain.py"]);
    let staged = git(&case.root, &["diff", "--cached", "--name-only"]);
    case.add(&[f("one")]);
    assert_eq!(git(&case.root, &["diff", "--cached", "--name-only"]), staged);
    assert_eq!(git(&case.root, &["show", "--name-only", "--format=", "HEAD"]), "lore/git.jsonl");
    // the new file's own entry is brought along, so status is clean for it
    assert_eq!(git(&case.root, &["status", "--short", "--", "lore"]), "");
}

#[test]
fn write__Shared__race_retries_on_the_new_head() {
    let case = Case::new("shared-tree");
    let root = case.root.clone();
    let index = case.tmp().join("idx");
    let mut other_commit = || {
        std::fs::write(root.join("other.txt"), "other\n").unwrap();
        let sha = git(&root, &["hash-object", "-w", "other.txt"]);
        let run = |args: &[&str]| -> String {
            let output = std::process::Command::new("git").arg("-C").arg(&root).args(args).env("GIT_INDEX_FILE", &index).output().unwrap();
            assert!(output.status.success());
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        run(&["read-tree", "HEAD"]);
        run(&["update-index", "--add", "--cacheinfo", &format!("100644,{sha},other.txt")]);
        let tree = run(&["write-tree"]);
        let commit = git(&root, &["commit-tree", &tree, "-p", "HEAD", "-m", "other session"]);
        git(&root, &["update-ref", "refs/heads/main", &commit]);
    };
    let mut transform = |_: &std::collections::BTreeMap<String, Option<Vec<u8>>>, _: Option<&str>| -> codelore::Result<std::collections::BTreeMap<String, Vec<u8>>> {
        Ok([("lore/git.jsonl".to_owned(), b"line\n".to_vec())].into())
    };
    let result = gitio::write_shared(&case.root, "main", &["lore/git.jsonl".into()], &mut transform, "s", "", &[], 3, Some(&mut other_commit), None).unwrap();
    assert!(result.notes.iter().any(|note| note.contains("moved while writing")), "{:?}", result.notes);
    let log = git(&case.root, &["log", "--format=%s", "-3"]);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(&lines[..2], ["s", "other session"]);
    assert_eq!(git(&case.root, &["show", "HEAD:other.txt"]), "other");
}

#[test]
fn write__Shared__live_file_an_older_version_is_brought_up() {
    let case = Case::new("shared-tree");
    case.add(&[f("first")]);
    let live = case.file("lore/git.jsonl");
    let old = std::fs::read(&live).unwrap();
    case.add(&[f("second")]);
    // a sync that could not replace the file leaves it, and its index entry, one version behind
    std::fs::write(&live, &old).unwrap();
    git(&case.root, &["add", "lore/git.jsonl"]);
    let out = case.add(&[f("third")]);
    assert!(out["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().contains("older version of itself")), "{}", out["notes"]);
    assert_eq!(String::from_utf8(std::fs::read(&live).unwrap()).unwrap(), git(&case.root, &["show", "HEAD:lore/git.jsonl"]) + "\n");
    assert_eq!(git(&case.root, &["status", "--short", "--", "lore"]), "");
}

#[test]
fn write__Shared__held_live_file_leaves_the_commit_and_a_note() {
    let case = Case::new("shared-tree");
    case.add(&[f("first")]);
    let live = case.file("lore/git.jsonl");
    let before = std::fs::read(&live).unwrap();
    let mut transform = |_: &std::collections::BTreeMap<String, Option<Vec<u8>>>, _: Option<&str>| -> codelore::Result<std::collections::BTreeMap<String, Vec<u8>>> {
        Ok([("lore/git.jsonl".to_owned(), b"line\n".to_vec())].into())
    };
    let result = gitio::with_held_files(|| gitio::write_shared(&case.root, "main", &["lore/git.jsonl".into()], &mut transform, "s", "", &[], 3, None, None)).unwrap();
    assert!(result.commit.is_some());
    assert!(result.notes.iter().any(|note| note.contains("another process holds")), "{:?}", result.notes);
    assert_eq!(std::fs::read(&live).unwrap(), before);
    assert!(!case.file("lore/git.jsonl.lore-tmp").exists());
}

#[test]
fn write__Shared__two_writers_both_land() {
    let case = Case::new("shared-tree");
    case.add(&[f("one")]);
    case.add(&[f("two")]);
    assert_eq!(git(&case.root, &["show", "HEAD:lore/git.jsonl"]).lines().count(), 2);
}

#[test]
fn write__Shared__refuses_off_branch() {
    let case = Case::new("shared-tree");
    git(&case.root, &["checkout", "-q", "-b", "side"]);
    case.refuse_in(&["add", "any", "--stdin", "--new"], &f("x").to_string());
}

#[test]
fn write__NoneMode__writes_without_committing() {
    let case = Case::new("none");
    let before = case.head();
    case.add(&[f("one")]);
    assert_eq!(case.head(), before);
    assert!(case.file("lore/git.jsonl").exists());
}

// ============================ test_retire_duplicate.py ============================

#[test]
fn retire_duplicate__RetireDuplicate__fold_a_duplicate_into_its_twin() {
    let case = Case::new("plain");
    case.add(&[
        fault("count reads a docstring", json!({"symptom": "the resource count is one too many"})),
        fault("resources off by one", json!({"symptom": "dx resources read one more than the brief"})),
    ]);
    case.lore(&["update", "count-reads-a-docstring", "-s", "status=superseded", "-s", "superseded_by=resources-off-by-one"]);
    let r = case.json(&["show", "count-reads-a-docstring"]);
    assert_eq!((r["status"].as_str(), r["superseded_by"].as_str()), (Some("superseded"), Some("resources-off-by-one")));
    // The two find assertions wait for the index.
    if let Some(found) = case.find_ids(&["resource count one too many"]) {
        assert!(!found.contains(&"count-reads-a-docstring".to_owned()));
        assert!(case.find_ids(&["resource count one too many", "--all"]).unwrap().contains(&"count-reads-a-docstring".to_owned()));
    }
    case.refuse(&["update", "resources-off-by-one", "-s", "status=superseded"]); // names no twin
    case.refuse(&["update", "resources-off-by-one", "-s", "status=superseded", "-s", "superseded_by=nope"]); // a twin that does not exist
}

#[test]
fn retire_duplicate__RetireDuplicate__append_to_text_refuses_clearly() {
    let case = Case::new("plain");
    case.add(&[f("one")]);
    let text = case.refuse(&["update", "one", "-s", "fix+=more"]);
    assert!(text.contains("is text, and += appends to a list"), "{text}");
    case.lore(&["update", "one", "-s", "fix=a whole new fix"]);
    assert_eq!(case.json(&["show", "one"])["fix"], "a whole new fix");
}

// ============================== test_topic_recipe.py ==============================

fn topic(extra: Value) -> Value {
    let mut record = json!({"v": 2, "id": "the-frame", "kind": "topic", "area": "build", "title": "How the frame is drawn",
        "status": "current", "created": "2026-10-24", "updated": "2026-10-24", "by": "me",
        "body": "The frame draws the ground, then the props, then the post chain."});
    for (key, value) in extra.as_object().unwrap() {
        if value.is_null() { record.as_object_mut().unwrap().remove(key); } else { record[key] = value.clone(); }
    }
    record
}

fn recipe(extra: Value) -> Value {
    let mut record = json!({"v": 2, "id": "cut-a-release", "kind": "recipe", "area": "build", "title": "Cutting a release",
        "status": "current", "created": "2026-10-24", "updated": "2026-10-24", "by": "me",
        "when": "a version is to be handed out", "steps": ["build", "check", "publish"],
        "check": "the published index names the version"});
    for (key, value) in extra.as_object().unwrap() {
        if value.is_null() { record.as_object_mut().unwrap().remove(key); } else { record[key] = value.clone(); }
    }
    record
}

#[test]
fn topic_recipe__Kinds__valid() {
    let areas = areas(&["git", "build"]);
    assert!(schema::validate(&topic(json!({})), &areas, None).is_empty());
    assert!(schema::validate(&recipe(json!({})), &areas, None).is_empty());
}

#[test]
fn topic_recipe__Kinds__rules() {
    let areas = areas(&["git", "build"]);
    let cases = [
        (topic(json!({"body": null})), "`body` is required"),
        (topic(json!({"body": "x".repeat(3001)})), "the limit is 3000"),
        (topic(json!({"status": "live"})), "`status` must be one of"),
        (topic(json!({"status": "superseded"})), "`superseded_by` is required when `superseded`"),
        (topic(json!({"v": 1})), "a topic needs format version 2"),
        (recipe(json!({"steps": []})), "`steps` is required"),
        (recipe(json!({"steps": "one"})), "list of non-empty strings"),
        (recipe(json!({"steps": ["x".repeat(401)]})), "the limit is 400"),
        (recipe(json!({"when": null})), "`when` is required"),
        (recipe(json!({"check": null})), "`check` is required"),
        (topic(json!({"v": FORMAT_VERSION + 1})), "newer than this lore reads"),
    ];
    for (record, words) in cases {
        let errors = schema::validate(&record, &areas, None);
        assert!(has(&errors, words), "{words:?} not in {errors:?}");
    }
}

#[test]
fn topic_recipe__Kinds__version_one_records_still_read() {
    let record = json!({"v": 1, "id": "old-fault", "kind": "fault", "area": "git", "title": "t", "status": "live",
        "created": "2026-10-24", "updated": "2026-10-24", "by": "me", "symptom": "s", "cause": "c", "fix": "f", "check": "k"});
    assert!(schema::validate(&record, &areas(&["git", "build"]), None).is_empty());
}

#[test]
fn topic_recipe__Cli__add_find_show_update() {
    let case = Case::new("plain");
    let out = case.json_in(&["add", "topic", "--stdin"], &json!({"area": "build", "title": "How the frame is drawn",
        "body": "The frame draws the ground first, then the props, then the post chain, in that order."}).to_string());
    let rid = out["records"][0]["id"].as_str().unwrap().to_owned();
    let record = case.json(&["show", &rid]);
    assert_eq!((record["v"].as_u64(), record["status"].as_str()), (Some(FORMAT_VERSION), Some("current")));
    if let Some(found) = case.find_ids(&["in what order is the frame drawn"]) {
        assert_eq!(found[0], rid);
    }
    case.lore_in(&["add", "recipe", "--stdin"], &json!({"area": "build", "title": "Cutting a release", "when": "a version is to be handed out",
        "steps": ["build the bundle", "run the release check", "publish the index"], "check": "the published index names the version"}).to_string());
    let text = case.lore(&["show", "cutting-a-release"]);
    assert!(text.contains("1. build the bundle"), "{text}");
    assert!(text.contains("3. publish the index"), "{text}");
    case.lore(&["update", "cutting-a-release", "-s", "steps+=announce it"]);
    assert_eq!(case.json(&["show", "cutting-a-release"])["steps"].as_array().unwrap().len(), 4);
    if let Some(found) = case.find_ids(&["release", "--kind", "recipe"]) {
        assert_eq!(found[0], "cutting-a-release");
    }
}

#[test]
fn topic_recipe__Cli__old_record_updated_takes_the_current_version() {
    let case = Case::new("plain");
    case.add(&[f("a fault")]);
    let path = case.file("lore/git.jsonl");
    let mut records = parse(&std::fs::read(&path).unwrap(), "x").unwrap();
    records[0]["v"] = json!(1);
    std::fs::write(&path, serialize(&records)).unwrap();
    assert!(case.lore(&["check"]).contains("ok"));
    case.lore(&["update", "a-fault", "-s", "fix=a better fix"]);
    assert_eq!(case.json(&["show", "a-fault"])["v"].as_u64(), Some(FORMAT_VERSION));
}

// =============================== test_search_plans.py ===============================

fn search_case() -> Case {
    let case = Case::new("plain");
    case.add(&[
        fault("git archive piped into tar fails", json!({"symptom": "every file fails to extract",
            "signals": ["tar.exe: Damaged tar archive (bad header checksum)"],
            "refs": {"files": ["src/chain.py"], "symbols": ["scene.depth_buffer"]}})),
        fault("the device was lost", json!({"area": "render", "symptom": "every frame reads the same stale picture",
            "signals": ["ERR_DEVICE_LOST", "0x80070005"]})),
        fault("old thing", json!({"area": "build", "status": "obsolete", "symptom": "an obsolete symptom"})),
    ]);
    case
}

#[test]
fn search_plans__Search__a_writer_holding_the_index_is_waited_for() {
    let case = search_case();
    case.add(&[f("written while another process holds the index")]);
    assert!(case.find_ids(&["another process holds"]).expect(WAITS)[0].starts_with("written-while-another-process"));
}

#[test]
fn search_plans__Search__exact_signal_first() {
    let case = search_case();
    assert_eq!(case.find_ids(&["whatever", "tar.exe: Damaged tar archive (bad header checksum)", "more"]).expect(WAITS)[0], "git-archive-piped-into-tar-fails");
    assert_eq!(case.find_ids(&["0x80070005"]).expect(WAITS)[0], "the-device-was-lost");
    assert_eq!(case.find_ids(&["ERR_DEVICE_LOST"]).expect(WAITS)[0], "the-device-was-lost");
}

#[test]
fn search_plans__Search__a_copied_phrase_beats_a_bag_of_its_words() {
    let case = search_case();
    let noise = "the file on the line wrote every head of the line, the file on every line ".repeat(3).trim().to_owned();
    let mut records: Vec<Value> = (0..12).map(|i| fault(&format!("noise {i}"), json!({"area": "build", "symptom": noise}))).collect();
    records.push(fault("crlf rewrite", json!({"area": "build", "symptom": "the file it wrote differs from HEAD on every line"})));
    case.add(&records);
    assert_eq!(case.find_ids(&["the file it wrote differs from HEAD on every line"]).expect(WAITS)[0], "crlf-rewrite");
    let out = case.json(&["find", "the file it wrote differs from HEAD on every line"]);
    assert_eq!(out["results"][0]["via"][0], "phrase");
}

#[test]
fn search_plans__Search__words() {
    let case = search_case();
    assert_eq!(case.find_ids(&["same stale picture on every frame"]).expect(WAITS)[0], "the-device-was-lost");
}

#[test]
fn search_plans__Search__terms_are_whole() {
    let case = search_case();
    assert!(codelore::dedupe::terms_of("it writes scene.depth_buffer here").contains(&"scene.depth_buffer".to_owned()));
    assert!(codelore::dedupe::terms_of("got ERR_DEVICE_LOST").contains(&"ERR_DEVICE_LOST".to_owned()));
    assert!(codelore::dedupe::terms_of("HRESULT 0x80070005.").contains(&"0x80070005".to_owned()));
    // The find assertion waits for the index.
    if let Some(found) = case.find_ids(&["scene.depth_buffer"]) {
        assert_eq!(found[0], "git-archive-piped-into-tar-fails");
    }
}

#[test]
fn search_plans__Search__retired_hidden_unless_all() {
    let case = search_case();
    assert!(!case.find_ids(&["obsolete symptom"]).expect(WAITS).contains(&"old-thing".to_owned()));
    assert!(case.find_ids(&["obsolete symptom", "--all"]).expect(WAITS).contains(&"old-thing".to_owned()));
    assert_eq!(case.find_ids(&["picture", "--kind", "fault", "--area", "render"]).expect(WAITS), ["the-device-was-lost"]);
}

#[test]
fn search_plans__Search__fresh_after_change() {
    let case = search_case();
    case.lore(&["update", "the-device-was-lost", "-s", "symptom=the frame turns purple"]);
    assert_eq!(case.find_ids(&["purple"]).expect(WAITS)[0], "the-device-was-lost");
    assert!(!case.find_ids(&["stale picture"]).expect(WAITS).contains(&"the-device-was-lost".to_owned()));
}

#[test]
fn search_plans__Search__pasted_traceback() {
    let case = search_case();
    let log = "Traceback (most recent call last):\n  File \"x.py\", line 3, in <module>\n    run()\nRuntimeError: renderer: GetData failed, HRESULT 0x80070005\n";
    let query = codelore::search::errors_of(log);
    assert!(!query.contains("File"), "{query}");
    // The find assertion waits for the index.
    if finder().is_some() {
        let hits = case.json_in(&["find", "-"], log);
        assert_eq!(hits["results"][0]["record"]["id"], "the-device-was-lost");
    }
}

#[test]
fn search_plans__Search__for_file_dir_symbol() {
    let case = search_case();
    let absolute = case.root.join("src").join("chain.py").to_string_lossy().into_owned();
    for target in ["src/chain.py", "src", "src/", absolute.as_str(), "scene.depth_buffer", "scene"] {
        assert_eq!(ids(&case.json(&["for", target])), ["git-archive-piped-into-tar-fails"], "{target}");
    }
    assert_eq!(case.json(&["for", "src/other.py"]), json!([]));
}

#[test]
#[ignore = "Ollama's provider-down path does not exist with the built-in model"]
fn search_plans__Search__provider_down_is_text_only_and_quick() {
    let case = search_case();
    let config = std::fs::read_to_string(case.file("lore.toml")).unwrap().replace("provider = \"none\"", "provider = \"ollama\"\nurl = \"http://127.0.0.1:9\"\ntimeout = 1.0");
    std::fs::write(case.file("lore.toml"), config).unwrap();
    let started = std::time::Instant::now();
    let out = case.json(&["find", "stale picture"]);
    assert!(started.elapsed().as_secs_f64() < 4.0);
    assert!(out["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().contains("unavailable")));
    assert_eq!(out["results"][0]["record"]["id"], "the-device-was-lost");
}

#[test]
#[ignore = "needs a test-injected embedding corpus; the built-in model is measured by tests/parity.rs"]
fn search_plans__Search__fusion_with_meaning() {
    let case = search_case();
    let out = case.json(&["find", "frame picture stale"]);
    assert!(out["results"][0]["via"].as_array().unwrap().contains(&json!("meaning")));
    assert_eq!(out["results"][0]["record"]["id"], "the-device-was-lost");
}

fn plans_case() -> Case {
    let case = Case::new("plain");
    case.lore(&["plan", "new", "--id", "api-v2", "--title", "API v2", "--goal", "the v2 endpoints serve"]);
    case.lore(&["item", "add", "api-v2", "--title", "schema", "--criterion", "documented"]);
    case.lore(&["item", "add", "api-v2", "--title", "handlers", "--dep", "api-v2#1"]);
    case.lore(&["item", "add", "api-v2", "--title", "docs", "--dep", "api-v2#2:soft"]);
    case
}

#[test]
fn search_plans__Plans__ready_and_layers() {
    let case = plans_case();
    assert_eq!(ids(&case.json(&["items", "--ready"])), ["api-v2#1", "api-v2#3"]);
    let text = case.lore(&["plan", "show", "api-v2"]);
    assert!(text.contains("layer 2"), "{text}");
}

#[test]
fn search_plans__Plans__closing_rules() {
    let case = plans_case();
    case.refuse(&["item", "done", "api-v2#1", "--commit", "deadbeef"]); // no such commit
    case.lore(&["item", "done", "api-v2#1", "--commit", &case.c0[..8]]);
    case.lore(&["item", "drop", "api-v2#2", "--reason", "v1 is enough", "--decided-by", "owner"]);
    case.lore(&["item", "reopen", "api-v2#2", "--reason", "needed after all"]);
    let r = case.json(&["show", "api-v2#2"]);
    assert_eq!(r["status"], "open");
    assert!(r.get("reason").is_none());
    case.lore(&["item", "drop", "api-v2#2", "--reason", "v1 is enough", "--decided-by", "owner"]);
    case.refuse(&["plan", "close", "api-v2", "--outcome", "done"]); // #3 is open
    case.lore(&["item", "defer", "api-v2#3", "--reason", "after the release"]);
    case.refuse(&["plan", "close", "api-v2", "--outcome", "done"]); // deferred is undecided
    case.lore(&["item", "supersede", "api-v2#3", "--with", "api-v2#1"]);
    case.lore(&["plan", "close", "api-v2", "--outcome", "schema shipped; handlers dropped"]);
    assert_eq!(case.json(&["plans"]), json!([]));
    assert_eq!(case.json(&["plans", "--all"]).as_array().unwrap().len(), 1);
    case.refuse(&["item", "add", "api-v2", "--title", "late"]); // plan is closed
}

#[test]
fn search_plans__Plans__done_item_does_not_silently_reopen() {
    let case = plans_case();
    case.lore(&["item", "done", "api-v2#1", "--commit", &case.c0[..8]]);
    case.refuse(&["update", "api-v2#1", "-s", "status=open", "-s", "commits="]);
}

#[test]
fn search_plans__Plans__cycle_refused_by_name() {
    let case = plans_case();
    let text = case.refuse(&["update", "api-v2#1", "-s", "deps+=api-v2#3"]);
    assert!(text.contains("cycle"), "{text}");
    assert!(text.contains("api-v2#1"), "{text}");
}

#[test]
fn search_plans__Plans__unknown_dep_refused() {
    let case = plans_case();
    let text = case.refuse(&["item", "add", "api-v2", "--title", "x", "--dep", "api-v2#9"]);
    assert!(text.contains("not an item"), "{text}");
}

#[test]
fn search_plans__Plans__stale() {
    let case = plans_case();
    assert_eq!(case.json(&["stale", "--days", "3"]), json!([]));
    // age the plan file's records by rewriting their dates through the store
    let path = case.file("lore/plans/api-v2.jsonl");
    let old = (chrono::Local::now().date_naive() - chrono::Duration::days(30)).format("%Y-%m-%d").to_string();
    let mut records = parse(&std::fs::read(&path).unwrap(), "x").unwrap();
    for record in &mut records {
        if record["id"] != "api-v2#2" {
            record["created"] = json!(old);
            record["updated"] = json!(old);
        }
    }
    std::fs::write(&path, serialize(&records)).unwrap();
    assert_eq!(ids(&case.json(&["stale", "--days", "3"])), ["api-v2#1", "api-v2#3"]);
}

#[test]
fn search_plans__Gate__check_and_guide() {
    let case = Case::new("plain");
    case.add(&[f("one")]);
    assert!(case.lore(&["check"]).contains("ok"));
    let path = case.file("lore/git.jsonl");
    let bytes = String::from_utf8(std::fs::read(&path).unwrap()).unwrap().replace("{\"area\"", "{ \"area\"");
    std::fs::write(&path, bytes).unwrap();
    assert!(case.refuse(&["check"]).contains("not canonical"));
    case.lore(&["fmt"]);
    assert!(case.lore(&["check"]).contains("ok"));
    let guide = case.lore(&["guide"]);
    for line in guide.lines() {
        let trimmed = line.trim();
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        if line.starts_with("    lore ") && words.len() > 1 && !words[1].contains('<') {
            assert!(cli::COMMANDS.contains(&words[1]), "{trimmed}");
            if words[1] == "plan" {
                assert!(cli::PLAN_COMMANDS.contains(&words[2]), "{trimmed}");
            }
            if words[1] == "item" {
                assert!(cli::ITEM_COMMANDS.contains(&words[2]), "{trimmed}");
            }
        }
    }
}

#[test]
fn search_plans__Gate__init() {
    let case = Case::new("plain");
    let fresh = case.tmp().join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    git(&fresh, &["init", "-q", "-b", "main"]);
    let path = fresh.to_string_lossy().into_owned();
    let env = Env { cwd: fresh.clone(), ..Env::default() };
    assert_eq!(cli::run(&["-C", &path, "init", "--areas", "a,b"], &env).code, 0);
    assert_eq!(cli::run(&["-C", &path, "check"], &env).code, 0);
    assert!(std::fs::read_to_string(fresh.join(".gitattributes")).unwrap().contains("lore/** text eol=lf"));
}

// ================================ test_migration.py ================================

const DOC: &str = "# Handbook\n\nSome words about the handbook.\n\n## Setup\n\n1. Install the tools\n   with the second line of the first step\n2. Run the gate\n\n## Faults\n\n| Error | Fix |\n|---|---|\n| `E_FAIL` on resize | restart |\n| a hang on exit | wait |\n\n```\n## not a heading, it is in a fence\n```\n\n## Setup\n\nThe second setup, said again.\n";

use codelore::cover::{coverage, parts_of};

fn names(parts: &[codelore::cover::Part]) -> Vec<String> {
    parts.iter().map(|part| part.name.clone()).collect()
}

#[test]
fn migration__Parts__headings_rows_items_and_repeats() {
    let found: Vec<(String, String)> = parts_of(DOC).into_iter().map(|part| (part.kind, part.name)).collect();
    let want = [
        ("heading", "Handbook"),
        ("text", "Handbook / text"),
        ("heading", "Setup"),
        ("item", "Setup / 1. Install the tools"),
        ("item", "Setup / 2. Run the gate"),
        ("heading", "Faults"),
        ("text", "Faults / text"), // the fence under it is its own content
        ("row", "Faults | `E_FAIL` on resize"),
        ("row", "Faults | a hang on exit"),
        ("heading", "Setup (2)"),
    ];
    assert_eq!(found, want.iter().map(|(kind, name)| ((*kind).to_owned(), (*name).to_owned())).collect::<Vec<_>>());
}

#[test]
fn migration__Parts__a_heading_entry_covers_what_is_under_it() {
    let parts = parts_of(DOC);
    let found = coverage(&parts, &[json!({"part": "faults", "skip": "history"})]);
    assert!(!names(&found.uncovered).contains(&"Faults | a hang on exit".to_owned()));
    // a heading that says nothing itself is covered by its items
    let found = coverage(&parts, &[json!({"part": "Setup / 1. Install the tools", "records": ["a"]}), json!({"part": "Setup / 2. Run the gate", "skip": "one-off"})]);
    assert!(!names(&found.uncovered).contains(&"Setup".to_owned()));
    // what a heading says itself needs its own entry, which covers nothing else
    assert!(names(&found.uncovered).contains(&"Handbook / text".to_owned()));
    let found = coverage(&parts, &[json!({"part": "Handbook / text", "skip": "an introduction"})]);
    assert!(names(&found.uncovered).contains(&"Setup / 2. Run the gate".to_owned()));
    assert_eq!(coverage(&parts, &[json!({"part": "No such part", "skip": "x"})]).unknown, ["No such part"]);
}

#[test]
fn migration__Writes__retire_with_long_ids_and_a_value_that_is_not_an_id() {
    let case = Case::new("plain");
    let (a, b) = ("a".repeat(48), "b".repeat(48));
    case.add(&[fault("x", json!({"id": a})), fault("y", json!({"id": b, "symptom": "nothing like it", "cause": "z q"}))]);
    case.lore(&["update", &a, "-s", "status=superseded", "-s", &format!("superseded_by={b}")]);
    assert_eq!(case.json(&["show", &a])["superseded_by"], json!(b));
    let text = case.refuse(&["update", &b, "-s", "status=superseded", "-s", &format!("superseded_by={a} {b}")]);
    assert!(text.contains(&format!("names one record by its id, and '{a} {b}' is not an id")), "{text}");
}

#[test]
fn migration__Writes__update_json_in_takes_the_id_from_the_patch() {
    let case = Case::new("plain");
    let rid = case.add(&[f("one")])["records"][0]["id"].as_str().unwrap().to_owned();
    case.lore_in(&["update", "--json-in"], &json!({"id": rid, "fix": "from the patch"}).to_string());
    assert_eq!(case.json(&["show", &rid])["fix"], "from the patch");
    case.refuse_in(&["update", "other", "--json-in"], &json!({"id": rid, "fix": "x"}).to_string());
    assert!(case.refuse_in(&["update", "--json-in"], &json!({"fix": "x"}).to_string()).contains("which record?"));
}

#[test]
fn migration__Writes__a_batch_with_one_duplicate_names_it_and_can_write_the_rest() {
    let case = Case::new("plain");
    case.add(&[fault("device lost", json!({"signals": ["ERR_DEVICE_LOST: the device was removed"]}))]);
    let batch = json!([
        fault("fresh one", json!({"symptom": "first new"})),
        fault("other words", json!({"symptom": "second", "signals": ["err_device_lost: the device was removed"]})),
        fault("fresh two", json!({"symptom": "third new", "cause": "another cause entirely"})),
    ])
    .to_string();
    let text = case.refuse_in(&["add", "any", "--stdin"], &batch);
    assert!(text.contains("1 of 3 record(s) already exist"), "{text}");
    assert!(text.contains("#2 'other words' is: device-lost (signal 1.0)"), "{text}");
    assert!(text.contains("--skip-duplicates writes the other 2"), "{text}");
    let out = case.json_in(&["add", "any", "--stdin", "--skip-duplicates"], &batch);
    let titles: Vec<&str> = out["records"].as_array().unwrap().iter().map(|record| record["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["fresh one", "fresh two"]);
    assert!(out["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().contains("left out #2 'other words'")), "{}", out["notes"]);
}

#[test]
fn migration__Writes__a_bare_class_name_is_not_a_signal() {
    let case = Case::new("plain");
    let text = case.refuse_in(&["add", "any", "--stdin", "--new"], &fault("x", json!({"signals": ["AttributeError"]})).to_string());
    assert!(text.contains("signal 'AttributeError' is a bare name"), "{text}");
    case.add(&[fault("x", json!({"signals": ["AttributeError: 'NoneType' object has no attribute 'depth'", "ERR_DEVICE_LOST", "0x80070005"]}))]);
}

#[test]
fn migration__Writes__a_family_of_named_records_is_not_refused() {
    let case = Case::new("plain");
    let body = "the fields: intensity, threshold, radius, enabled, quality, each with its default";
    case.lore_in(&["add", "topic", "--stdin"], &json!({"area": "render", "title": "`BloomSettings` - the bloom's settings", "body": format!("{body} for bloom")}).to_string());
    let out = case.json_in(&["add", "topic", "--stdin"], &json!({"area": "render", "title": "`DofSettings` - the depth of field's settings", "body": format!("{body} for dof")}).to_string());
    assert!(out["commit"].is_string());
    assert!(out["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().contains("names differ")), "{}", out["notes"]);
    // the same title said twice is still refused
    case.refuse_in(&["add", "topic", "--stdin"], &json!({"area": "render", "title": "`DofSettings` - the depth of field's settings", "body": body}).to_string());
}

#[test]
fn migration__Writes__a_duplicate_decision_is_retired_as_superseded() {
    let case = Case::new("plain");
    let decision = |title: &str, extra: Value| {
        let mut record = json!({"kind": "decision", "area": "build", "question": "which build tool?", "choice": "make", "reason": "it is there", "decided_by": "lead", "title": title});
        for (key, value) in extra.as_object().unwrap() {
            record[key] = value.clone();
        }
        record
    };
    case.add(&[decision("build with make", json!({})), decision("we build with make", json!({"id": "build-make-again"}))]);
    case.lore(&["update", "build-make-again", "-s", "status=superseded", "-s", "superseded_by=build-with-make"]);
    // The find assertion waits for the index.
    if let Some(found) = case.find_ids(&["build tool"]) {
        assert!(!found.contains(&"build-make-again".to_owned()));
    }
    let old = decision("t", json!({"v": 2, "id": "x-y", "status": "superseded", "superseded_by": "build-with-make", "created": "2026-01-01", "updated": "2026-01-01", "by": "t"}));
    assert!(has(&schema::validate(&old, &areas(&["build"]), None), "needs format version 3"));
}

#[test]
fn migration__Writes__commits_of_an_open_item_say_where_they_go() {
    let case = Case::new("plain");
    case.lore(&["plan", "new", "--title", "p", "--goal", "g", "--id", "pl"]);
    case.lore(&["item", "add", "pl", "--title", "one"]);
    let text = case.refuse(&["update", "pl#1", "-s", &format!("commits+={}", case.c0)]);
    assert!(text.contains("go in `refs.commits`"), "{text}");
    case.lore(&["update", "pl#1", "-s", &format!("refs.commits+={}", case.c0)]);
}

#[test]
fn migration__Writes__an_item_keeps_its_outcome_and_reopening_clears_it() {
    let case = Case::new("plain");
    case.lore(&["plan", "new", "--title", "p", "--goal", "g", "--id", "pl"]);
    case.lore(&["item", "add", "pl", "--title", "one"]);
    case.lore(&["item", "done", "pl#1", "--commit", &case.c0, "--outcome", "built the lane; its cache was wrong"]);
    assert_eq!(case.json(&["show", "pl#1"])["outcome"], "built the lane; its cache was wrong");
    case.lore(&["item", "reopen", "pl#1", "--reason", "the cache"]);
    assert!(case.json(&["show", "pl#1"]).get("outcome").is_none());
    case.refuse(&["update", "pl#1", "-s", "outcome=not yet"]);
}

#[test]
fn migration__Writes__exists_reads_live() {
    let case = Case::new("plain");
    let rid = case.add(&[f("one")])["records"][0]["id"].as_str().unwrap().to_owned();
    assert!(case.lore(&["exists", &rid]).contains(&format!("{rid}  fault [live]")));
    let text = case.refuse(&["exists", &rid, "nope"]);
    assert!(text.contains("nope  MISSING"), "{text}");
}

#[test]
fn migration__Check__references_are_checked_in_one_git_process_each() {
    let case = Case::new("plain");
    let records: Vec<Value> = (0..30)
        .map(|n| fault(&format!("f{n}"), json!({"symptom": format!("s{n} distinct"), "cause": format!("c{n} other"), "refs": {"commits": [&case.c0[..10]], "files": ["src/chain.py"]}})))
        .collect();
    case.add(&records);
    let (out, calls) = gitio::count_calls(|| case.lore(&["check"]));
    assert!(out.contains("30 record(s) in 1 file(s): ok"), "{out}");
    assert!(calls.iter().filter(|call| *call == "cat-file").count() <= 2, "{calls:?}");
}

#[test]
fn migration__Check__evidence() {
    let case = Case::new("plain");
    std::fs::write(case.file("src/other.py"), "y = 1\n").unwrap();
    git(&case.root, &["add", "-A"]);
    git(&case.root, &["commit", "-qm", "other"]);
    let c1 = case.head();
    case.add(&[
        fault("right", json!({"refs": {"commits": [&case.c0[..10]], "files": ["src/chain.py"]}})),
        fault("wrong", json!({"symptom": "another", "cause": "d e", "refs": {"commits": [&c1[..10]], "files": ["src/chain.py"]}})),
    ]);
    case.lore(&["check"]);
    let text = case.refuse(&["check", "--evidence"]);
    assert!(text.contains(&format!("wrong: commit {} touches none of src/chain.py", &c1[..10])), "{text}");
    assert!(!text.contains("right:"), "{text}");
    // a hash written in the document the record came from is evidence too
    std::fs::write(case.file("doc.md"), format!("## Wrong\n\nfixed in {}\n", &c1[..8])).unwrap();
    git(&case.root, &["add", "doc.md"]);
    git(&case.root, &["commit", "-qm", "doc"]);
    case.lore_in(&["cover", "doc.md", "--add"], &json!({"part": "Wrong", "records": ["wrong"]}).to_string());
    case.lore(&["check", "--evidence"]);
}

#[test]
#[ignore = "needs an unwritable user cache; index_dirs covers the fallback"]
fn migration__Sandbox__index_falls_back_to_the_git_directory() {
    let case = Case::new("plain");
    case.lore_in(&["add", "any", "--stdin", "--new"], &f("one").to_string());
    let out = case.lore(&["show", "one"]);
    assert!(out.contains("the index is in"), "{out}");
    assert!(case.file(".git/codelore/index.sqlite").exists());
}

#[test]
fn migration__Sandbox__a_failed_push_is_one_clear_line() {
    let case = Case::new("plain");
    let config = std::fs::read_to_string(case.file("lore.toml")).unwrap().replace("branch = \"main\"", "branch = \"main\"\npush = true");
    std::fs::write(case.file("lore.toml"), config).unwrap();
    let missing = case.tmp().join("no-such-remote");
    git(&case.root, &["remote", "add", "origin", &missing.to_string_lossy()]);
    let out = case.lore_in(&["add", "any", "--stdin", "--new"], &f("one").to_string());
    let sha = case.head()[..10].to_owned();
    let line = out.lines().find(|line| line.starts_with("committed")).unwrap_or_default().to_owned();
    assert!(line.starts_with(&format!("committed {sha} locally, push to origin/main failed: ")), "{out}");
    assert!(line.ends_with("the next write's push carries it"), "{out}");
}

#[test]
fn migration__SharedShow__show_finds_a_commit_the_working_tree_has_not_caught_up_with() {
    let case = Case::new("shared-tree");
    case.add(&[f("first")]);
    let rid = gitio::with_held_files(|| case.add(&[fault("second", json!({"symptom": "other", "cause": "d e"}))]))["records"][0]["id"].as_str().unwrap().to_owned();
    assert!(!std::fs::read_to_string(case.file("lore/git.jsonl")).unwrap().contains(&rid));
    assert!(case.lore(&["show", &rid]).contains("not in the working tree yet"));
    case.lore(&["exists", &rid]);
    // += appends to the record the commit is built on, not to the stale working tree
    case.lore(&["update", &rid, "-s", "signals+=E_ONE"]);
    case.lore(&["update", &rid, "-s", "signals+=E_TWO"]);
    assert_eq!(case.json(&["show", &rid])["signals"], json!(["E_ONE", "E_TWO"]));
}

fn cover_case() -> Case {
    let case = Case::new("plain");
    std::fs::create_dir_all(case.file("docs")).unwrap();
    std::fs::write(case.file("docs/hand.md"), DOC).unwrap();
    git(&case.root, &["add", "-A"]);
    git(&case.root, &["commit", "-qm", "doc"]);
    case
}

fn jsonl(entries: &[Value]) -> String {
    entries.iter().map(Value::to_string).collect::<Vec<_>>().join("\n")
}

#[test]
fn migration__Cover__cover_a_document_part_by_part() {
    let case = cover_case();
    let out = case.refuse(&["cover", "docs/hand.md"]);
    assert!(out.contains("no source record yet; 10 part(s)"), "{out}");
    assert!(out.contains("Faults | a hang on exit"), "{out}");
    let rid = case.add(&[f("hang on exit")])["records"][0]["id"].as_str().unwrap().to_owned();
    let entries = [json!({"part": "Handbook / text", "skip": "an introduction"}), json!({"part": "Setup", "skip": "the old setup, replaced"}), json!({"part": "Faults | a hang on exit", "records": [rid]})];
    case.lore_in(&["cover", "docs/hand.md", "--add"], &jsonl(&entries));
    let res = case.run(&["--json", "cover", "docs/hand.md"], "");
    assert_eq!(res.code, 1); // not complete
    let res: Value = serde_json::from_str(&res.stdout).unwrap();
    assert_eq!(res["complete"], false);
    let uncovered: Vec<&str> = res["uncovered"].as_array().unwrap().iter().map(|part| part["part"].as_str().unwrap()).collect();
    assert_eq!(uncovered, ["Faults / text", "Faults | `E_FAIL` on resize", "Setup (2)"]);
    assert_eq!(res["source"]["status"], "open");
    // a name that is no part of the document is refused
    case.refuse_in(&["cover", "docs/hand.md", "--add"], &json!({"part": "Fautls", "skip": "x"}).to_string());
    case.lore_in(&["cover", "docs/hand.md", "--add"], &jsonl(&[
        json!({"part": "Faults | `E_FAIL` on resize", "skip": "one-off, never seen again"}),
        json!({"part": "setup  (2)", "skip": "said again"}),
        json!({"part": "Faults / text", "skip": "an example block"}),
    ]));
    let res = case.json(&["cover", "docs/hand.md"]);
    assert_eq!(res["complete"], true);
    assert_eq!(res["source"]["status"], "covered");
    // the document answers `lore for`, and it is not a search result
    let listed = case.json(&["for", "docs/hand.md"]);
    let kinds: Vec<&str> = listed.as_array().unwrap().iter().map(|record| record["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["source"]);
    if finder().is_some() {
        let found = case.json(&["find", "setup"]);
        assert!(!found["results"].as_array().unwrap().iter().any(|hit| hit["record"]["kind"] == "source"));
    }
    // a record a parts list names cannot go away quietly, and a document that grows is reported
    std::fs::write(case.file("docs/hand.md"), format!("{DOC}\n## New\n\nnew words\n")).unwrap();
    assert!(case.lore(&["check"]).contains("changed since it was covered"));
}

#[test]
fn migration__Cover__a_whole_document_is_history() {
    let case = cover_case();
    let rid = case.add(&[f("what still stands")])["records"][0]["id"].as_str().unwrap().to_owned();
    case.refuse(&["cover", "docs/hand.md", "--history"]); // needs a reason
    case.lore(&["cover", "docs/hand.md", "--history", "--reason", "a plan that was finished", "--record", &rid]);
    let out = case.lore(&["cover", "docs/hand.md"]);
    assert!(out.contains("history"), "{out}");
    assert!(out.contains(&format!("what still stands is in: {rid}")), "{out}");
    let source = &case.json(&["cover", "docs/hand.md"])["source"];
    assert_eq!((source["status"].as_str(), source["v"].as_u64()), (Some("history"), Some(FORMAT_VERSION)));
    case.lore(&["check"]);
}

// ============================ beyond the Python suite ============================

#[test]
fn json_shapes_of_every_command() {
    let case = Case::new("plain");
    // A write: commit, records, notes; no push keys without a push.
    let write = case.add(&[fault("shape", json!({"refs": {"files": ["src/chain.py"]}}))]);
    assert_eq!(write.as_object().unwrap().keys().collect::<Vec<_>>(), ["commit", "records", "notes"]);
    let rid = write["records"][0]["id"].as_str().unwrap().to_owned();
    // The record as written keeps its keys in the order it was built.
    let record = &write["records"][0];
    assert_eq!(record["v"], json!(FORMAT_VERSION));
    assert!(record["by"].is_string() && record["created"].is_string());
    // exists, areas, plans, items, stale, for, show, check, cover.
    let mut exists = Map::new();
    exists.insert(rid.clone(), json!({"kind": "fault", "status": "live", "title": "shape"}));
    assert_eq!(case.json(&["exists", &rid]), Value::Object(exists));
    assert_eq!(case.json(&["areas"]), json!({"git": 1, "build": 0, "render": 0}));
    case.lore(&["plan", "new", "--id", "pl", "--title", "p", "--goal", "g"]);
    let plans = case.json(&["plans"]);
    assert_eq!(plans[0]["counts"], json!({"open": 0, "deferred": 0, "done": 0, "dropped": 0, "superseded": 0}));
    assert_eq!(plans[0]["plan"]["id"], "pl");
    assert_eq!(case.json(&["items"]), json!([]));
    assert_eq!(case.json(&["stale"]), json!([]));
    assert_eq!(ids(&case.json(&["for", "src/chain.py"])), std::slice::from_ref(&rid));
    assert_eq!(case.json(&["show", &rid])["id"], json!(rid));
    let check = case.json(&["check"]);
    assert_eq!(check.as_object().unwrap().keys().collect::<Vec<_>>(), ["records", "files", "errors", "warnings"]);
    assert_eq!((check["records"].as_u64(), check["files"].as_u64()), (Some(2), Some(2)));
    std::fs::write(case.file("doc.md"), "# Doc\n\nwords\n").unwrap();
    let cover = case.run(&["--json", "cover", "doc.md"], "");
    let cover: Value = serde_json::from_str(&cover.stdout).unwrap();
    assert_eq!(cover.as_object().unwrap().keys().collect::<Vec<_>>(), ["path", "source", "parts", "uncovered", "unknown", "complete"]);
    assert_eq!(cover["uncovered"][0], json!({"part": "Doc", "kind": "heading", "line": 1}));
    // A refusal: the first line and its details, exit 1.
    let refused = case.run(&["--json", "update", "nope", "-s", "fix=x"], "");
    assert_eq!(refused.code, 1);
    assert_eq!(serde_json::from_str::<Value>(&refused.stdout).unwrap(), json!({"error": "'nope' does not exist", "details": []}));
    // The printed form is two-space indented JSON, as Python's indent=2.
    assert!(case.run(&["--json", "areas"], "").stdout.starts_with("{\n  \"git\": 1,\n"));
    // find and index refuse by name without the index.
    if finder().is_none() {
        let find = case.run(&["--json", "find", "x"], "");
        assert_eq!(find.code, 1);
        assert!(find.stdout.contains("needs the search index"));
    }
}

#[test]
fn a_failed_push_reports_both_push_keys_in_json() {
    let case = Case::new("plain");
    let config = std::fs::read_to_string(case.file("lore.toml")).unwrap().replace("branch = \"main\"", "branch = \"main\"\npush = true");
    std::fs::write(case.file("lore.toml"), config).unwrap();
    git(&case.root, &["remote", "add", "origin", &case.tmp().join("none").to_string_lossy()]);
    let out = case.add(&[f("one")]);
    assert_eq!(out["pushed"], false);
    assert!(out["push_error"].is_string());
    assert!(out["notes"].as_array().unwrap().iter().any(|note| note.as_str().unwrap().ends_with("the next write's push carries it")));
}
