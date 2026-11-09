//! The ledger (docs/FORMAT.md, "ledger, version 4") through the command layer:
//! journals per writer, the fold over `parents`, conflicts and their
//! resolution, pending lines, the cache that reads on, and a lore without
//! `[ledger]` that is what it was.
use codelore::cli::{self, Env};
use codelore::config::{FieldType, Ledger};
use codelore::ledger::{fold::fold, Line};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const BASE: &str = "version = 1\ndir = \"lore\"\nareas = [\"notes\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n";
const LEDGER: &str = "\n[ledger]\ndir = \"ledger\"\n\n[ledger.kinds.item]\nlabel = \"string\"\nstate = { enum = [\"todo\", \"done\"] }\nsize = \"number\"\nshared = \"bool\"\ntags = \"list\"\nextra = \"object\"\n";
/// 2026-11-09T12:00:00Z
const NOON: i64 = 1_794_225_600;

struct Repo {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn new(ledger: bool) -> Self {
        let tmp = tempfile::Builder::new().prefix("lore-ledger-").tempdir().unwrap();
        let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap()).join("repo");
        std::fs::create_dir_all(root.join("lore")).unwrap();
        let config = if ledger { format!("{BASE}{LEDGER}") } else { BASE.to_owned() };
        std::fs::write(root.join("lore.toml"), config).unwrap();
        Self { _tmp: tmp, root }
    }

    /// One call as `by` on `machine` at `now`.
    fn run_as(&self, by: &str, machine: &str, now: i64, argv: &[&str], stdin: &str) -> (i32, String, String) {
        let env = Env { cwd: self.root.clone(), stdin: stdin.as_bytes().to_vec(), lore_by: Some(by.into()), machine: Some(machine.into()), now: Some(now), ..Env::default() };
        let out = cli::run(argv, &env);
        (out.code, out.stdout, out.stderr)
    }

    fn ok(&self, by: &str, argv: &[&str]) -> String {
        let (code, out, err) = self.run_as(by, "m1", NOON, argv, "");
        assert_eq!(code, 0, "{argv:?}: {out}{err}");
        out
    }

    fn json(&self, by: &str, argv: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend(argv);
        serde_json::from_str(&self.ok(by, &all)).unwrap()
    }

    /// The id of the line a `set` wrote.
    fn set(&self, by: &str, now: i64, argv: &[&str]) -> String {
        let mut all = vec!["--json", "ledger", "set"];
        all.extend(argv);
        let (code, out, err) = self.run_as(by, "m1", now, &all, "");
        assert_eq!(code, 0, "{argv:?}: {out}{err}");
        let value: Value = serde_json::from_str(&out).unwrap();
        value["lines"][0]["id"].as_str().unwrap().to_owned()
    }

    fn get(&self, key: &str) -> Value {
        self.json("alice", &["ledger", "get", "item", key])
    }

    fn journal(&self, author: &str) -> PathBuf {
        self.root.join("ledger").join(format!("{author}@m1")).join("2026-11.jsonl")
    }
}

fn ledger() -> Ledger {
    let mut fields = BTreeMap::new();
    fields.insert("label".to_owned(), FieldType::String);
    fields.insert("state".to_owned(), FieldType::Enum(vec!["todo".into(), "done".into()]));
    let mut kinds = BTreeMap::new();
    kinds.insert("item".to_owned(), fields);
    Ledger { dir: "ledger".into(), kinds }
}

fn line(set: Value, parents: &[&Line], by: &str, at: &str) -> Line {
    let set: Map<String, Value> = set.as_object().unwrap().clone();
    Line::new("item", "a/b/c", set, parents.iter().map(|parent| parent.id.clone()).collect(), by, at, None)
}

#[test]
fn two_writers_on_different_fields_keep_both() {
    let repo = Repo::new(true);
    let first = repo.set("alice", NOON, &["item", "a/b/c", "label=one"]);
    // bob has not seen alice's line: concurrent, but on another field.
    let (code, out, err) = repo.run_as("bob", "m2", NOON + 5, &["--json", "ledger", "set", "item", "a/b/c", "state=done", "--parents", ""], "");
    assert_eq!(code, 0, "{out}{err}");
    let entry = repo.get("a/b/c");
    assert_eq!(entry["set"], json!({"label": "one", "state": "done"}));
    assert_eq!(entry["conflicts"], json!({}));
    assert_eq!(entry["heads"].as_array().unwrap().len(), 2);
    assert!(entry["heads"].as_array().unwrap().contains(&json!(first)));
    // Each wrote only to a file of its own.
    assert!(repo.journal("alice").is_file());
    assert!(repo.root.join("ledger/bob@m2/2026-11.jsonl").is_file());
}

#[test]
fn one_field_set_by_two_concurrent_lines_is_a_conflict_and_the_smaller_id_holds() {
    let repo = Repo::new(true);
    let base = repo.set("alice", NOON, &["item", "k", "label=base"]);
    let a = repo.set("alice", NOON + 1, &["item", "k", "label=from-alice"]);
    let b = repo.set("bob", NOON + 2, &["item", "k", "label=from-bob", "--parents", &base]);
    let entry = repo.get("k");
    let sides: Vec<&str> = entry["conflicts"]["label"].as_array().unwrap().iter().map(|side| side["id"].as_str().unwrap()).collect();
    let mut want = vec![a.as_str(), b.as_str()];
    want.sort();
    assert_eq!(sides, want);
    let holding = if a < b { "from-alice" } else { "from-bob" };
    assert_eq!(entry["set"]["label"], json!(holding));
    let listed = repo.json("alice", &["ledger", "conflicts"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["holding"], json!(want[0]));
}

#[test]
fn an_offline_chain_against_another_writers_edit_is_a_conflict() {
    let base = line(json!({"label": "base"}), &[], "alice", "2026-11-01T00:00:00Z");
    let one = line(json!({"label": "offline-1"}), &[&base], "alice", "2026-11-02T00:00:00Z");
    let two = line(json!({"label": "offline-2"}), &[&one], "alice", "2026-11-03T00:00:00Z");
    let other = line(json!({"label": "bob"}), &[&base], "bob", "2026-11-02T12:00:00Z");
    let entry = fold(&[base, one.clone(), two.clone(), other.clone()]);
    let sides: Vec<String> = entry.conflicts["label"].iter().map(|side| side.id.clone()).collect();
    let mut want = vec![two.id.clone(), other.id.clone()];
    want.sort();
    assert_eq!(sides, want, "the chain's last line against bob's, not its first");
    assert!(!sides.contains(&one.id));
}

#[test]
fn a_value_set_back_makes_no_false_order() {
    let a1 = line(json!({"label": "a"}), &[], "alice", "2026-11-01T00:00:00Z");
    let b = line(json!({"label": "b"}), &[&a1], "alice", "2026-11-01T00:01:00Z");
    let a2 = line(json!({"label": "a"}), &[&b], "alice", "2026-11-01T00:02:00Z");
    assert_ne!(a1.id, a2.id);
    let entry = fold(&[a1.clone(), b.clone(), a2.clone()]);
    assert_eq!(entry.set["label"], json!("a"));
    assert!(entry.conflicts.is_empty());
    assert_eq!(entry.heads, vec![a2.id.clone()]);
    // A line made after `b` concurrently with the second `a` conflicts with it
    // and not with the first.
    let c = line(json!({"label": "c"}), &[&b], "bob", "2026-11-01T00:03:00Z");
    let entry = fold(&[a2.clone(), c.clone(), b, a1]);
    let sides: Vec<&str> = entry.conflicts["label"].iter().map(|side| side.id.as_str()).collect();
    let mut want = vec![a2.id.as_str(), c.id.as_str()];
    want.sort();
    assert_eq!(sides, want);
}

#[test]
fn a_wrong_clock_changes_nothing() {
    // The child says it was written decades before its parent; it still comes after.
    let parent = line(json!({"label": "old"}), &[], "alice", "2040-01-01T00:00:00Z");
    let child = line(json!({"label": "new"}), &[&parent], "bob", "2001-01-01T00:00:00Z");
    let entry = fold(&[child.clone(), parent.clone()]);
    assert_eq!(entry.set["label"], json!("new"));
    assert!(entry.conflicts.is_empty());
    assert_eq!(entry.order, vec![parent.id, child.id]);
}

#[test]
fn the_order_lines_are_read_in_changes_nothing() {
    let base = line(json!({"label": "base", "state": "todo"}), &[], "alice", "2026-11-01T00:00:00Z");
    let x = line(json!({"label": "x"}), &[&base], "alice", "2026-11-02T00:00:00Z");
    let y = line(json!({"label": "y", "state": "done"}), &[&base], "bob", "2026-11-02T00:00:00Z");
    let z = line(json!({"state": null}), &[&x, &y], "carol", "2026-11-03T00:00:00Z");
    let lines = vec![base, x, y, z];
    let first = fold(&lines);
    let orders = [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2]];
    for order in orders {
        let shuffled: Vec<Line> = order.iter().map(|at| lines[*at].clone()).collect();
        let mut twice = shuffled.clone();
        twice.extend(shuffled);
        assert_eq!(fold(&twice), first, "order {order:?}");
    }
    assert!(!first.set.contains_key("state"), "null unsets the field");
    assert!(first.conflicts.contains_key("label"));

    // Through the journals too: two writers' files, the cache built either way.
    let repo = Repo::new(true);
    repo.set("bob", NOON, &["item", "k", "label=bob", "--parents", ""]);
    repo.set("alice", NOON, &["item", "k", "label=alice", "--parents", ""]);
    let one = repo.json("alice", &["ledger", "fold"]);
    let other = Repo::new(true);
    other.set("alice", NOON, &["item", "k", "label=alice", "--parents", ""]);
    other.set("bob", NOON, &["item", "k", "label=bob", "--parents", ""]);
    assert_eq!(other.json("alice", &["ledger", "fold"]), one);
}

#[test]
fn unknown_kinds_fields_and_types_are_refused_by_name() {
    let repo = Repo::new(true);
    for (args, want) in [
        (vec!["item2", "k", "label=x"], "kind 'item2' is not declared"),
        (vec!["item", "k", "colour=red"], "kind item has no field 'colour'"),
        (vec!["item", "k", "state=maybe"], "item.state must be one of todo, done"),
        (vec!["item", "k", "size=big"], "item.size must be number"),
        (vec!["item", "k", "shared=1"], "item.shared must be bool, not 1"),
    ] {
        let mut argv = vec!["ledger", "set"];
        argv.extend(args);
        let (code, out, err) = repo.run_as("alice", "m1", NOON, &argv, "");
        assert_eq!(code, 1, "{argv:?}");
        assert!(err.contains(want), "{argv:?}: {out}{err}");
    }
    assert!(!repo.root.join("ledger").exists(), "nothing was written");
    let (code, _, err) = repo.run_as("Alice:x", "m1", NOON, &["ledger", "set", "item", "k", "label=x"], "");
    assert_eq!(code, 1);
    assert!(err.contains("must match ^[a-z0-9][a-z0-9._-]{0,63}$"), "{err}");

    // A line written by hand with a field the kind does not have: refused by file and line.
    repo.set("alice", NOON, &["item", "k", "label=x"]);
    let mut bad = Line::new("item", "k", json!({"colour": "red"}).as_object().unwrap().clone(), vec![], "alice", "2026-11-09T12:00:01Z", None).text();
    bad.push('\n');
    let mut text = std::fs::read_to_string(repo.journal("alice")).unwrap();
    text.push_str(&bad);
    std::fs::write(repo.journal("alice"), text).unwrap();
    let (code, out, _) = repo.run_as("alice", "m1", NOON, &["ledger", "check"], "");
    assert_eq!(code, 1);
    assert!(out.contains("ledger/alice@m1/2026-11.jsonl is refused") && out.contains("line 2: kind item has no field 'colour'"), "{out}");
    let (code, _, err) = repo.run_as("alice", "m1", NOON, &["ledger", "get", "item", "k"], "");
    assert_eq!(code, 1);
    assert!(err.contains("line 2: kind item has no field 'colour'"), "{err}");
}

#[test]
fn a_parent_not_here_yet_is_applied_and_reported() {
    let repo = Repo::new(true);
    let missing = "0123456789abcdef";
    repo.set("alice", NOON, &["item", "k", "label=x", "--parents", missing]);
    assert_eq!(repo.get("k")["set"]["label"], json!("x"));
    assert_eq!(repo.get("k")["dangling"][0]["parent"], json!(missing));
    let status = repo.json("alice", &["ledger", "status"]);
    assert_eq!(status["dangling"][0]["parent"], json!(missing));
    let check = repo.json("alice", &["ledger", "check"]);
    assert_eq!(check["errors"], json!([]));
    assert!(check["warnings"][0].as_str().unwrap().contains(&format!("parent {missing} is not here (yet)")));
}

#[test]
fn the_cache_reads_on_and_notices_a_journal_that_was_not_only_appended_to() {
    let repo = Repo::new(true);
    repo.set("alice", NOON, &["item", "k", "label=one"]);
    assert_eq!(repo.get("k")["set"]["label"], json!("one"));
    // Another writer's file arrives; then alice appends: both read on.
    let bob = Line::new("item", "j", json!({"label": "bob"}).as_object().unwrap().clone(), vec![], "bob", "2026-11-09T12:00:00Z", None);
    std::fs::create_dir_all(repo.root.join("ledger/bob@m2")).unwrap();
    std::fs::write(repo.root.join("ledger/bob@m2/2026-11.jsonl"), format!("{}\n", bob.text())).unwrap();
    repo.set("alice", NOON + 60, &["item", "k", "label=two"]);
    let (code, out, err) = repo.run_as("alice", "m1", NOON, &["--json", "ledger", "fold"], "");
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("warning"), "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), json!({"item": {"j": {"label": "bob"}, "k": {"label": "two"}}}));

    // The journal loses its last line: said by name, built again.
    let text = std::fs::read_to_string(repo.journal("alice")).unwrap();
    let first_line = text.lines().next().unwrap();
    std::fs::write(repo.journal("alice"), format!("{first_line}\n")).unwrap();
    let (code, out, err) = repo.run_as("alice", "m1", NOON, &["--json", "ledger", "get", "item", "k"], "");
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("ledger/alice@m1/2026-11.jsonl got shorter") && err.contains("only ever appended to"), "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["set"]["label"], json!("one"));

    // Its start rewritten at the same length: noticed as well.
    let changed = repo.journal("alice");
    let other = Line::new("item", "k", json!({"label": "ONE"}).as_object().unwrap().clone(), vec![], "alice", "2026-11-09T12:00:00Z", None).text();
    let extra = format!("{other}\n{}\n", Line::new("item", "z", json!({"label": "z"}).as_object().unwrap().clone(), vec![], "alice", "2026-11-09T12:00:00Z", None).text());
    std::fs::write(&changed, extra).unwrap();
    let (code, out, err) = repo.run_as("alice", "m1", NOON, &["--json", "ledger", "get", "item", "k"], "");
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("changed before its end"), "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["set"]["label"], json!("ONE"));
    // And the next call is quiet again.
    let (_, _, err) = repo.run_as("alice", "m1", NOON, &["ledger", "get", "item", "k"], "");
    assert!(!err.contains("warning"), "{err}");
}

#[test]
fn pending_lines_are_read_over_the_journals_and_promoted_by_key() {
    let repo = Repo::new(true);
    repo.set("alice", NOON, &["item", "a/1", "label=journal"]);
    let pending = repo.root.join("pending.jsonl");
    let pending_arg = pending.to_string_lossy().into_owned();
    repo.set("alice", NOON + 1, &["item", "a/1", "label=pending", "--to", &pending_arg]);
    repo.set("alice", NOON + 2, &["item", "a/1", "state=done", "--to", &pending_arg]);
    repo.set("alice", NOON + 3, &["item", "b/2", "label=other", "--to", &pending_arg]);
    // Without --pending the journal alone; with it, the lines over it, chained.
    assert_eq!(repo.get("a/1")["set"], json!({"label": "journal"}));
    let over = repo.json("alice", &["ledger", "--pending", &pending_arg, "get", "item", "a/1"]);
    assert_eq!(over["set"], json!({"label": "pending", "state": "done"}));
    assert_eq!(over["conflicts"], json!({}));
    let (code, _, _) = repo.run_as("alice", "m1", NOON, &["ledger", "get", "item", "b/2"], "");
    assert_eq!(code, 1, "b/2 is only pending");

    // Someone else's promote of alice's lines is refused.
    let (code, _, err) = repo.run_as("bob", "m1", NOON, &["ledger", "promote", &pending_arg, "--key", "a/1"], "");
    assert_eq!(code, 1);
    assert!(err.contains("only bob's lines go into bob's journal"), "{err}");

    let moved = repo.json("alice", &["ledger", "promote", &pending_arg, "--key", "a/1"]);
    assert_eq!(moved["moved"].as_array().unwrap().len(), 2);
    assert_eq!(moved["left"], json!(1));
    assert_eq!(repo.get("a/1")["set"], json!({"label": "pending", "state": "done"}));
    let left = std::fs::read_to_string(&pending).unwrap();
    assert_eq!(left.lines().count(), 1);
    assert!(left.contains("\"key\":\"b/2\""));
    assert_eq!(std::fs::read_to_string(repo.journal("alice")).unwrap().lines().count(), 3);
}

#[test]
fn resolve_ends_a_conflict() {
    let repo = Repo::new(true);
    repo.set("alice", NOON, &["item", "k", "label=a", "--parents", ""]);
    repo.set("bob", NOON, &["item", "k", "label=b", "--parents", ""]);
    assert!(repo.get("k")["conflicts"].get("label").is_some());
    let (code, _, err) = repo.run_as("carol", "m3", NOON, &["ledger", "resolve", "item", "k", "state", "done"], "");
    assert_eq!(code, 1, "no conflict on state");
    assert!(err.contains("has no conflict on state"), "{err}");
    let (code, out, err) = repo.run_as("carol", "m3", NOON + 10, &["--json", "ledger", "resolve", "item", "k", "label", "agreed"], "");
    assert_eq!(code, 0, "{out}{err}");
    let entry = repo.get("k");
    assert_eq!(entry["set"]["label"], json!("agreed"));
    assert_eq!(entry["conflicts"], json!({}));
    assert_eq!(entry["heads"].as_array().unwrap().len(), 1);
    assert_eq!(repo.json("alice", &["ledger", "conflicts"]), json!([]));
    let log = repo.json("alice", &["ledger", "log", "item", "k"]);
    assert_eq!(log.as_array().unwrap().len(), 3);
    assert_eq!(log[2]["by"], json!("carol"));
}

#[test]
fn a_batch_writes_every_line_in_one_call_and_chains_them() {
    let repo = Repo::new(true);
    let stdin = "{\"kind\":\"item\",\"key\":\"k\",\"set\":{\"label\":\"one\",\"tags\":[\"x\"]}}\n{\"kind\":\"item\",\"key\":\"k\",\"set\":{\"label\":\"two\",\"extra\":{\"n\":1}},\"note\":\"second\"}\n";
    let (code, out, err) = repo.run_as("alice", "m1", NOON, &["--json", "ledger", "set", "--batch"], stdin);
    assert_eq!(code, 0, "{out}{err}");
    let entry = repo.get("k");
    assert_eq!(entry["set"], json!({"label": "two", "tags": ["x"], "extra": {"n": 1}}));
    assert_eq!(entry["conflicts"], json!({}));
    // One bad line refuses all of them.
    let bad = "{\"kind\":\"item\",\"key\":\"q\",\"set\":{\"label\":\"ok\"}}\n{\"kind\":\"item\",\"key\":\"q\",\"set\":{\"size\":\"big\"}}\n";
    let (code, _, err) = repo.run_as("alice", "m1", NOON, &["ledger", "set", "--batch"], bad);
    assert_eq!(code, 1);
    assert!(err.contains("line 2: item.size must be number"), "{err}");
    assert_eq!(std::fs::read_to_string(repo.journal("alice")).unwrap().lines().count(), 2);
    let found = repo.json("alice", &["ledger", "find", "item", "--where", "label=two"]);
    assert_eq!(found[0]["key"], json!("k"));
    assert_eq!(repo.json("alice", &["ledger", "find", "item", "--where", "label=one"]), json!([]));
}

#[test]
fn the_written_lines_match_the_fixture_byte_for_byte() {
    let repo = Repo::new(true);
    let first = repo.set("alice", NOON, &["item", "a/b/c", "label=Größe ü", "size=2.5", "shared=true"]);
    repo.set("alice", NOON + 61, &["item", "a/b/c", "--unset", "size", "tags=[\"b\",\"a\"]", "--note", "tidied"]);
    repo.set("alice", NOON + 62, &["item", "x", "extra={\"z\":1,\"a\":[true,null]}", "state=todo", "--parents", &first]);
    let written = std::fs::read(repo.journal("alice")).unwrap();
    let place = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ledger/alice@m1/2026-11.jsonl");
    if std::env::var_os("LORE_BLESS").is_some() {
        std::fs::create_dir_all(place.parent().unwrap()).unwrap();
        std::fs::write(&place, &written).unwrap();
    }
    let fixture = std::fs::read(&place).unwrap();
    assert_eq!(String::from_utf8_lossy(&written), String::from_utf8_lossy(&fixture));
    assert_eq!(written, fixture);
}

#[test]
fn a_lore_without_a_ledger_is_what_it_was() {
    let with = Repo::new(true);
    let without = Repo::new(false);
    let config = codelore::config::load(&without.root).unwrap();
    assert!(config.ledger.is_none());
    assert!(config.warnings.is_empty());
    let record = r#"{"kind":"fault","area":"notes","title":"the build stops","symptom":"the build stops at link time","cause":"a stale object","fix":"clean the folder","check":"the link step passes","status":"live","refs":{"records":[]}}"#;
    for repo in [&with, &without] {
        let (code, out, err) = repo.run_as("alice", "m1", NOON, &["add", "fault", "--json-in"], record);
        assert_eq!(code, 0, "{out}{err}");
    }
    let read = |repo: &Repo| std::fs::read(repo.root.join("lore/notes.jsonl")).unwrap();
    assert_eq!(read(&with), read(&without));
    assert!(String::from_utf8_lossy(&read(&without)).contains("\"v\":3"));
    let (code, _, err) = without.run_as("alice", "m1", NOON, &["ledger", "status"], "");
    assert_eq!(code, 1);
    assert!(err.contains("has no [ledger]"), "{err}");
    let (_, guide, _) = without.run_as("alice", "m1", NOON, &["guide"], "");
    assert!(!guide.contains("## The ledger"));
    let (_, guide, _) = with.run_as("alice", "m1", NOON, &["guide"], "");
    assert!(guide.contains("## The ledger"));
    // Ledger paths are never knowledge files.
    assert!(!codelore::store::is_lore_file("ledger/alice@m1/2026-11.jsonl", "lore"));
    assert!(!codelore::store::is_lore_file("lore/x@m/2026-11.jsonl", "lore"));
}

#[test]
fn a_knowledge_record_names_a_ledger_entry() {
    let repo = Repo::new(true);
    repo.set("alice", NOON, &["item", "a/b/c", "label=x"]);
    let record = |target: &str| format!(r#"{{"kind":"fault","area":"notes","title":"the build stops {target}","symptom":"the build stops at link time for {target}","cause":"a stale object","fix":"clean","check":"the link step passes","status":"live","refs":{{"records":["{target}"]}}}}"#);
    let (code, out, err) = repo.run_as("alice", "m1", NOON, &["add", "fault", "--json-in"], &record("ledger:item:a/b/c"));
    assert_eq!(code, 0, "{out}{err}");
    // An unknown kind is refused; a key the ledger does not have (yet) is not.
    let (code, _, err) = repo.run_as("alice", "m1", NOON, &["add", "fault", "--json-in"], &record("ledger:thing:a"));
    assert_eq!(code, 1);
    assert!(err.contains("kind thing is not declared in [ledger.kinds]"), "{err}");
    let (code, out, err) = repo.run_as("alice", "m1", NOON, &["add", "fault", "--json-in"], &record("ledger:item:not/yet"));
    assert_eq!(code, 0, "{out}{err}");
    let (code, out, err) = repo.run_as("alice", "m1", NOON, &["--json", "check"], "");
    let check: Value = serde_json::from_str(&out).unwrap_or_else(|_| panic!("{out}{err}"));
    assert_eq!(code, 0, "{out}{err}");
    let warnings: Vec<&str> = check["warnings"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(warnings.iter().any(|warning| warning.contains("'ledger:item:not/yet', which is not in the ledger (yet)")), "{warnings:?}");
    assert!(!warnings.iter().any(|warning| warning.contains("ledger:item:a/b/c")), "{warnings:?}");
}

#[test]
fn the_declarations_are_checked() {
    let parse = |text: &str| codelore::config::parse(&toml::from_str::<toml::Value>(&format!("{BASE}{text}")).unwrap(), Path::new("."), "lore.toml");
    let refused = |text: &str, want: &str| {
        let error = parse(text).unwrap_err();
        assert!(error.details.iter().any(|detail| detail.contains(want)), "{text}: {error}");
    };
    refused("[ledger]\ndir = \"lore/ledger\"\n[ledger.kinds.item]\nlabel = \"string\"\n", "must not hold one another");
    refused("[ledger]\ndir = \"../out\"\n", "relative path inside the repository");
    refused("[ledger.kinds.item]\nlabel = \"text\"\n", "ledger field `item.label` must be");
    refused("[ledger.kinds.Item]\nlabel = \"string\"\n", "ledger kind `Item` must match");
    refused("[ledger]\ncolour = 1\n", "unknown key `ledger.colour`");
    let config = parse("[ledger.kinds.item]\nstate = { enum = [\"a\", \"b\"] }\n").unwrap();
    let ledger = ledger();
    assert_eq!(config.ledger.unwrap().kinds["item"]["state"], FieldType::Enum(vec!["a".into(), "b".into()]));
    assert_eq!(ledger.dir, "ledger");
}
