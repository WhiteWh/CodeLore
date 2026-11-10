//! The ledger's contract beyond its fold: numbers that read back as they
//! were written, a state read in one go with what it rests on, unfinished lines
//! that are never taken for whole ones nor dropped, pending files that lose no
//! line, resolutions that keep to what was seen, parents that must be lines of
//! their own entry, retired fields and kinds, and a cache that never passes a
//! damage off as an empty state.
use codelore::cli::{self, Env};
use codelore::ledger::{canonical, fold::fold, fold_checked, Line};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

const BASE: &str = "version = 1\ndir = \"lore\"\nareas = [\"notes\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n";
const KINDS: &str = "\n[ledger]\n\n[ledger.kinds.item]\nlabel = \"string\"\nstate = { enum = [\"todo\", \"done\"] }\nsize = \"number\"\nshared = \"bool\"\ntags = \"list\"\nextra = \"object\"\n";
/// 2026-11-09T12:00:00Z
const NOON: i64 = 1_794_225_600;

struct Repo {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        Self::with(KINDS)
    }

    fn with(ledger: &str) -> Self {
        let tmp = tempfile::Builder::new().prefix("lore-ledger-contract-").tempdir().unwrap();
        let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap()).join("repo");
        std::fs::create_dir_all(root.join("lore")).unwrap();
        std::fs::write(root.join("lore.toml"), format!("{BASE}{ledger}")).unwrap();
        Self { _tmp: tmp, root }
    }

    fn config(&self, ledger: &str) {
        std::fs::write(self.root.join("lore.toml"), format!("{BASE}{ledger}")).unwrap();
    }

    fn run(&self, by: &str, now: i64, argv: &[&str], stdin: &str) -> (i32, String, String) {
        let env = Env { cwd: self.root.clone(), stdin: stdin.as_bytes().to_vec(), lore_by: Some(by.into()), machine: Some("m1".into()), now: Some(now), ..Env::default() };
        let out = cli::run(argv, &env);
        (out.code, out.stdout, out.stderr)
    }

    fn json(&self, argv: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend(argv);
        let (code, out, err) = self.run("alice", NOON, &all, "");
        assert_eq!(code, 0, "{argv:?}: {out}{err}");
        serde_json::from_str(&out).unwrap_or_else(|_| panic!("{out}{err}"))
    }

    fn refused(&self, by: &str, argv: &[&str]) -> String {
        let (code, out, err) = self.run(by, NOON, argv, "");
        assert_eq!(code, 1, "{argv:?} should be refused: {out}{err}");
        format!("{out}{err}")
    }

    /// The id of the one line a `set` wrote.
    fn set(&self, by: &str, now: i64, argv: &[&str]) -> String {
        let mut all = vec!["--json", "ledger", "set"];
        all.extend(argv);
        let (code, out, err) = self.run(by, now, &all, "");
        assert_eq!(code, 0, "{argv:?}: {out}{err}");
        serde_json::from_str::<Value>(&out).unwrap()["lines"][0]["id"].as_str().unwrap().to_owned()
    }

    fn journal(&self, author: &str) -> PathBuf {
        self.root.join("ledger").join(format!("{author}@m1")).join("2026-11.jsonl")
    }

    fn write_line(&self, author: &str, line: &Line) {
        let path = self.journal(author);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&line.text());
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }

    fn cache_file(&self) -> PathBuf {
        codelore::index::cache_dir().join(codelore::index::repo_key(&self.root)).join(codelore::ledger::cache::FILE_NAME)
    }
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn new_line(key: &str, set: Value, parents: &[&str], by: &str, second: u32) -> Line {
    Line::new("item", key, object(set), parents.iter().map(|id| (*id).to_owned()).collect(), by, &format!("2026-11-09T12:00:{second:02}Z"), None)
}

#[test]
fn a_number_written_reads_back_as_the_same_number_and_bytes() {
    // Every kind of finite double, from random bit patterns: written, read
    // back, written again - the same value and the same text.
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut checked = 0;
    while checked < 200_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let value = f64::from_bits(seed);
        if !value.is_finite() {
            continue;
        }
        let text = canonical(&json!(value));
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back.as_f64().unwrap().to_bits(), value.to_bits(), "{text}");
        assert_eq!(canonical(&back), text);
        checked += 1;
    }
    for text in ["0.1", "0.00001", "1e-6", "1e+20", "-0.0", "1000000000000000.0", "18446744073709551615", "-9223372036854775808"] {
        assert_eq!(canonical(&serde_json::from_str::<Value>(text).unwrap()), text);
    }
    // Numbers that only an exact reader reads back: the double just below
    // 1e-4, and one exactly between two shortest forms.
    for bits in [4547007122018943788u64, 4828130738419231434] {
        let text = canonical(&json!(f64::from_bits(bits)));
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap().as_f64().unwrap().to_bits(), bits, "{text}");
    }
    // Inside lists and objects too, and a number is never narrowed: such a
    // value is written and read back.
    let repo = Repo::new();
    repo.set("alice", NOON, &["item", "k", "size=1e-5", "tags=[-0.0,1e-7,1e20,18446744073709551615]"]);
    let entry = repo.json(&["ledger", "get", "item", "k"]);
    assert_eq!(canonical(&entry["set"]), r#"{"size":0.00001,"tags":[-0.0,1e-7,1e+20,18446744073709551615]}"#);
}

#[test]
fn the_state_comes_in_one_read_with_what_it_rests_on() {
    let repo = Repo::new();
    let base = repo.set("alice", NOON, &["item", "k", "label=base"]);
    repo.set("alice", NOON + 1, &["item", "k", "label=a"]);
    repo.set("bob", NOON + 2, &["item", "k", "label=b", "--parents", &base]);
    repo.set("alice", NOON + 3, &["item", "j", "label=j", "--parents", "0123456789abcdef"]);
    let state = repo.json(&["ledger", "fold", "--with-state"]);
    assert_eq!(state["complete"], json!(false));
    assert!(state["gaps"][0].as_str().unwrap().contains("names parent 0123456789abcdef"), "{state}");
    let k = &state["entries"]["item"]["k"];
    assert_eq!(k["heads"].as_array().unwrap().len(), 2);
    assert_eq!(k["conflicts"]["label"].as_array().unwrap().len(), 2);
    assert_eq!(state["entries"]["item"]["j"]["dangling"][0]["parent"], json!("0123456789abcdef"));
    // The plain fold keeps its shape; --strict refuses an incomplete state.
    assert_eq!(repo.json(&["ledger", "fold"])["item"]["j"], json!({"label": "j"}));
    assert!(repo.refused("alice", &["ledger", "fold", "--strict"]).contains("not complete (--strict)"));
    assert!(repo.refused("alice", &["ledger", "get", "item", "j", "--strict"]).contains("names parent 0123456789abcdef"));
    assert_eq!(repo.json(&["ledger", "get", "item", "k", "--strict"])["complete"], json!(true));
    assert_eq!(repo.json(&["ledger", "get", "item", "j"])["complete"], json!(false));
    let found = repo.json(&["ledger", "find", "item", "--where", "label=j"]);
    assert_eq!(found["entries"][0]["dangling"][0]["parent"], json!("0123456789abcdef"));
    assert_eq!(found["complete"], json!(false));
    // check --strict makes the missing parent an error.
    let (code, _, _) = repo.run("alice", NOON, &["ledger", "check", "--strict"], "");
    assert_eq!(code, 1);
}

#[test]
fn an_unfinished_line_is_neither_read_nor_dropped() {
    let repo = Repo::new();
    repo.set("alice", NOON, &["item", "k", "label=one"]);
    repo.set("alice", NOON + 1, &["item", "j", "label=two"]);
    // A batch cut by a crash: whole lines, then half of the next.
    let half = new_line("k", json!({"label": "three"}), &[], "alice", 9).text();
    let cut = &half[..half.len() / 2];
    let journal = repo.journal("alice");
    let mut bytes = std::fs::read(&journal).unwrap();
    bytes.extend_from_slice(cut.as_bytes());
    std::fs::write(&journal, &bytes).unwrap();
    // Read: the whole lines, said to be incomplete.
    let (code, out, err) = repo.run("alice", NOON, &["--json", "ledger", "fold", "--with-state"], "");
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("ends in an unfinished line"), "{err}");
    let state: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(state["complete"], json!(false));
    assert_eq!(state["entries"]["item"]["k"]["set"], json!({"label": "one"}));
    assert!(repo.refused("alice", &["ledger", "fold", "--strict"]).contains("unfinished line"));
    assert_eq!(repo.json(&["ledger", "status"])["complete"], json!(false));
    // A write after it is refused, the file untouched.
    assert!(repo.refused("alice", &["ledger", "set", "item", "k", "label=four"]).contains("`lore ledger repair`"));
    assert_eq!(std::fs::read(&journal).unwrap(), bytes);
    let (code, out, _) = repo.run("alice", NOON, &["ledger", "check"], "");
    assert_eq!(code, 1);
    assert!(out.contains("ends in an unfinished line"), "{out}");
    // repair sets the bytes aside, all of them; then writing goes on.
    let repaired = repo.json(&["ledger", "repair"]);
    let kept = PathBuf::from(repaired["repaired"][0]["kept"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), cut);
    assert_eq!(std::fs::read(&journal).unwrap(), &bytes[..bytes.len() - cut.len()]);
    repo.set("alice", NOON + 5, &["item", "k", "label=four"]);
    assert_eq!(repo.json(&["ledger", "get", "item", "k", "--strict"])["set"]["label"], json!("four"));
    // Another writer's journal cut short (an incomplete copy) is read up to its last whole line.
    let bob = repo.root.join("ledger/bob@m2/2026-11.jsonl");
    std::fs::create_dir_all(bob.parent().unwrap()).unwrap();
    let whole = new_line("b", json!({"label": "bob"}), &[], "bob", 1).text();
    std::fs::write(&bob, format!("{whole}\n{}", &whole[..10])).unwrap();
    assert_eq!(repo.json(&["ledger", "get", "item", "b"])["complete"], json!(false));
    std::fs::write(&bob, format!("{whole}\n")).unwrap();
    assert_eq!(repo.json(&["ledger", "get", "item", "b"])["complete"], json!(true));
}

#[test]
fn promote_moves_exact_lines_keeps_their_parents_and_loses_nothing() {
    let repo = Repo::new();
    let pending = repo.root.join("pending.jsonl");
    let arg = pending.to_string_lossy().into_owned();
    let one = repo.set("alice", NOON, &["item", "k", "label=one", "--to", &arg]);
    let two = repo.set("alice", NOON + 1, &["item", "k", "label=two", "--to", &arg]);
    let other = repo.set("alice", NOON + 2, &["item", "j", "label=j", "--to", &arg]);
    // `two` descends from `one`, which is only pending: alone it is refused.
    assert!(repo.refused("alice", &["ledger", "promote", &arg, "--id", &two]).contains(&format!("needs its parent {one}")));
    assert!(repo.refused("alice", &["ledger", "export", &arg, "--id", &two]).contains("only in the pending file"));
    let exported = repo.json(&["ledger", "export", &arg, "--id", &format!("{one},{two}")]);
    assert_eq!(exported["lines"].as_array().unwrap().len(), 2);
    // --keep copies into the journal and leaves the file as it was.
    let before = std::fs::read(&pending).unwrap();
    let copied = repo.json(&["ledger", "promote", &arg, "--id", &format!("{one},{two}"), "--keep"]);
    assert_eq!(copied["moved"].as_array().unwrap().len(), 2);
    assert_eq!(std::fs::read(&pending).unwrap(), before);
    // ack takes out only confirmed lines; `other` is in no journal yet.
    assert!(repo.refused("alice", &["ledger", "ack", &arg, "--id", &other]).contains("in no journal yet"));
    let acked = repo.json(&["ledger", "ack", &arg, "--id", &format!("{one},{two}")]);
    assert_eq!(acked["removed"].as_array().unwrap().len(), 2);
    let again = repo.json(&["ledger", "ack", &arg, "--id", &one]);
    assert_eq!(again["gone_before"], json!([one.clone()]), "a second ack of a confirmed line is done already");
    assert_eq!(std::fs::read_to_string(&pending).unwrap().lines().count(), 1);
    // A crash after the journal was written and before the pending file was:
    // the line is in both places; the next promote finds it and writes it once.
    let line = std::fs::read_to_string(&pending).unwrap();
    let journal = repo.journal("alice");
    let mut text = std::fs::read_to_string(&journal).unwrap();
    text.push_str(&line);
    std::fs::write(&journal, &text).unwrap();
    std::fs::write(pending.with_extension("jsonl.tmp"), b"half a replacement").unwrap();
    let moved = repo.json(&["ledger", "promote", &arg]);
    assert_eq!(moved["already"], json!(1));
    assert_eq!(std::fs::read_to_string(&journal).unwrap().matches(&other).count(), 1);
    assert_eq!(std::fs::read_to_string(&pending).unwrap(), "");
}

#[test]
fn a_line_written_to_a_pending_file_waits_for_its_lock() {
    use fs2::FileExt;
    let repo = Repo::new();
    let pending = repo.root.join("pending.jsonl");
    let arg = pending.to_string_lossy().into_owned();
    repo.set("alice", NOON, &["item", "k", "label=one", "--to", &arg]);
    // Someone holds the pending file's lock, as a promote does from read to rewrite.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(repo.root.join("pending.jsonl.lock")).unwrap();
    lock.lock_exclusive().unwrap();
    let root = repo.root.clone();
    let writer = std::thread::spawn(move || {
        let env = Env { cwd: root, lore_by: Some("alice".into()), machine: Some("m1".into()), now: Some(NOON + 1), ..Env::default() };
        cli::run(&["ledger", "set", "item", "j", "label=during", "--to", &arg], &env).code
    });
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert_eq!(std::fs::read_to_string(&pending).unwrap().lines().count(), 1, "the writer waits for the lock");
    fs2::FileExt::unlock(&lock).unwrap();
    assert_eq!(writer.join().unwrap(), 0);
    assert_eq!(std::fs::read_to_string(&pending).unwrap().lines().count(), 2);
}

#[test]
fn a_resolution_keeps_to_the_heads_it_saw() {
    let repo = Repo::new();
    let base = repo.set("alice", NOON, &["item", "k", "label=base"]);
    let a = repo.set("alice", NOON + 1, &["item", "k", "label=a"]);
    let b = repo.set("bob", NOON + 2, &["item", "k", "label=b", "--parents", &base]);
    // While the conflict was shown, C came: it carries on from B and sets label again.
    let c = repo.set("carol", NOON + 3, &["item", "k", "label=c", "--parents", &b]);
    let (code, out, err) = repo.run("dave", NOON + 4, &["ledger", "resolve", "item", "k", "label", "agreed", "--parents", &format!("{a},{b}")], "");
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("still in conflict"), "{out}");
    let entry = repo.json(&["ledger", "get", "item", "k"]);
    let sides: Vec<&str> = entry["conflicts"]["label"].as_array().unwrap().iter().map(|side| side["value"].as_str().unwrap()).collect();
    let mut sides = sides;
    sides.sort();
    assert_eq!(sides, vec!["agreed", "c"], "C was not seen, so it still stands against the resolution");
    let _ = c;
    // Two concurrent resolutions: different values conflict again, equal ones agree.
    let repo = Repo::new();
    let base = repo.set("alice", NOON, &["item", "k", "label=base"]);
    let a = repo.set("alice", NOON + 1, &["item", "k", "label=a"]);
    let b = repo.set("bob", NOON + 2, &["item", "k", "label=b", "--parents", &base]);
    let both = format!("{a},{b}");
    repo.set("carol", NOON + 3, &["item", "k", "label=x", "--parents", &both]);
    repo.set("dave", NOON + 4, &["item", "k", "label=y", "--parents", &both]);
    assert!(repo.json(&["ledger", "get", "item", "k"])["conflicts"].get("label").is_some());
    let repo = Repo::new();
    let base = repo.set("alice", NOON, &["item", "k", "label=base"]);
    let a = repo.set("alice", NOON + 1, &["item", "k", "label=a"]);
    let b = repo.set("bob", NOON + 2, &["item", "k", "label=b", "--parents", &base]);
    let both = format!("{a},{b}");
    repo.set("carol", NOON + 3, &["item", "k", "label=same", "--parents", &both]);
    repo.set("dave", NOON + 4, &["item", "k", "label=same", "--parents", &both]);
    let entry = repo.json(&["ledger", "get", "item", "k"]);
    assert_eq!(entry["conflicts"], json!({}));
    assert_eq!(entry["set"]["label"], json!("same"));
    // An unset against an edit of the same field stays a conflict.
    let repo = Repo::new();
    let base = repo.set("alice", NOON, &["item", "k", "size=1"]);
    repo.set("alice", NOON + 1, &["item", "k", "--unset", "size"]);
    repo.set("bob", NOON + 2, &["item", "k", "size=2", "--parents", &base]);
    let entry = repo.json(&["ledger", "get", "item", "k"]);
    assert_eq!(entry["conflicts"]["size"].as_array().unwrap().len(), 2);
    assert_eq!(entry["heads"].as_array().unwrap().len(), 2);
}

#[test]
fn a_parent_is_a_line_of_its_own_entry_on_every_path() {
    let repo = Repo::new();
    let k = repo.set("alice", NOON, &["item", "k", "label=k"]);
    // Writing: refused.
    assert!(repo.refused("alice", &["ledger", "set", "item", "j", "label=j", "--parents", &k]).contains("is a line of item 'k'"));
    // Reading a journal that holds one: refused by get and fold, not only check.
    repo.write_line("bob", &new_line("j", json!({"label": "j"}), &[&k], "bob", 5));
    for argv in [vec!["ledger", "get", "item", "k"], vec!["ledger", "fold"], vec!["ledger", "status"]] {
        assert!(repo.refused("alice", &argv).contains("is a line of item 'k'"), "{argv:?}");
    }
    let (code, out, _) = repo.run("alice", NOON, &["ledger", "check"], "");
    assert_eq!(code, 1);
    assert!(out.contains("not of this entry"), "{out}");

    // A parent that arrives later and turns out to be another entry's line.
    let repo = Repo::new();
    let elsewhere = new_line("other", json!({"label": "o"}), &[], "carol", 1);
    repo.write_line("bob", &new_line("j", json!({"label": "j"}), &[&elsewhere.id], "bob", 2));
    assert_eq!(repo.json(&["ledger", "get", "item", "j"])["complete"], json!(false), "a parent not here yet");
    repo.write_line("carol", &elsewhere);
    assert!(repo.refused("alice", &["ledger", "get", "item", "j"]).contains("is a line of item 'other'"));

    // A child before its parent, then the parent: the cached state is the cold fold's.
    let repo = Repo::new();
    let parent = new_line("k", json!({"label": "p"}), &[], "bob", 1);
    let child = new_line("k", json!({"label": "c", "size": 3}), &[&parent.id], "carol", 2);
    repo.write_line("carol", &child);
    let early = repo.json(&["ledger", "get", "item", "k"]);
    assert_eq!(early["complete"], json!(false));
    repo.write_line("bob", &parent);
    let warm = repo.json(&["ledger", "get", "item", "k"]);
    std::fs::remove_file(repo.cache_file()).unwrap();
    let cold = repo.json(&["ledger", "get", "item", "k"]);
    assert_eq!(warm, cold);
    assert_eq!(cold["complete"], json!(true));
    assert_eq!(cold["set"], json!({"label": "c", "size": 3}));
}

#[test]
fn two_lines_under_one_id_are_refused_by_name() {
    let first = new_line("k", json!({"label": "a"}), &[], "alice", 1);
    let mut second = new_line("k", json!({"label": "b"}), &[], "alice", 2);
    second.id = first.id.clone();
    let entry = fold(&[first.clone(), second.clone()]);
    assert_eq!(entry.collisions, vec![first.id.clone()]);
    let refused = fold_checked("item", "k", &[second, first.clone()]).unwrap_err();
    assert!(refused.message.contains("id collision"), "{refused}");
    // The same line twice is one line.
    assert!(fold(&[first.clone(), first]).collisions.is_empty());
}

#[test]
fn find_answers_as_a_full_scan_would() {
    let repo = Repo::new();
    let values = ["1", "1.0", "true", "\"1\"", "null"];
    let mut keys = Vec::new();
    for (at, size) in ["1", "1.0", "2"].iter().enumerate() {
        let key = format!("n{at}");
        repo.set("alice", NOON, &["item", &key, &format!("size={size}"), "label=null", "extra={\"size\":1}"]);
        keys.push(key);
    }
    repo.set("alice", NOON, &["item", "bare", "label=x"]);
    repo.set("alice", NOON, &["item", "flag", "shared=true"]);
    let pending = repo.root.join("p.jsonl");
    let arg = pending.to_string_lossy().into_owned();
    repo.set("alice", NOON + 1, &["item", "n2", "size=1", "--to", &arg]);
    let all: BTreeMap<String, Value> = repo.json(&["ledger", "--pending", &arg, "fold"])["item"].as_object().unwrap().clone().into_iter().collect();
    for field in ["size", "label", "shared"] {
        for value in values {
            let mut argv = vec!["ledger", "--pending", arg.as_str(), "find", "item", "--where"];
            let clause = format!("{field}={value}");
            argv.push(&clause);
            let (code, out, _) = repo.run("alice", NOON, &[&["--json"][..], &argv[..]].concat(), "");
            if code != 0 {
                continue; // a value that is not of the field's type
            }
            let got: Vec<String> = serde_json::from_str::<Value>(&out).unwrap()["entries"].as_array().unwrap().iter().map(|row| row["key"].as_str().unwrap().to_owned()).collect();
            let want_value = match codelore::config::load(&repo.root).unwrap().ledger.unwrap().kinds["item"][field] {
                codelore::config::FieldType::String => Value::String(value.to_owned()),
                _ => serde_json::from_str(value).unwrap(),
            };
            let want: Vec<String> = all.iter().filter(|(_, set)| set.get(field).is_some_and(|got| canonical(got) == canonical(&want_value))).map(|(key, _)| key.clone()).collect();
            assert_eq!(got, want, "{clause}");
        }
    }
    // --missing: no top-level value, even with a nested one of the same name.
    let found = repo.json(&["ledger", "--pending", &arg, "find", "item", "--missing", "size"]);
    let got: Vec<&str> = found["entries"].as_array().unwrap().iter().map(|row| row["key"].as_str().unwrap()).collect();
    assert_eq!(got, vec!["bare", "flag"]);
    // The pending line changed n2's size to 1: found by it, though the journal says 2.
    let found = repo.json(&["ledger", "--pending", &arg, "find", "item", "--where", "size=1"]);
    assert!(found["entries"].as_array().unwrap().iter().any(|row| row["key"] == json!("n2")));
}

#[test]
fn clear_unsets_what_was_seen_and_keeps_the_entry() {
    let repo = Repo::new();
    let base = repo.set("alice", NOON, &["item", "k", "label=a", "size=1"]);
    repo.set("alice", NOON + 1, &["item", "k", "size=2"]);
    repo.set("bob", NOON + 2, &["item", "k", "size=3", "--parents", &base]);
    let cleared = repo.json(&["ledger", "clear", "item", "k"]);
    assert_eq!(cleared["lines"][0]["set"], json!({"label": null, "size": null}), "the conflicted field too");
    let entry = repo.json(&["ledger", "get", "item", "k"]);
    assert_eq!(entry["set"], json!({}));
    assert_eq!(entry["conflicts"], json!({}));
    let again = repo.json(&["ledger", "clear", "item", "k"]);
    assert_eq!(again["lines"], json!([]), "nothing left to clear");
    // An edit concurrent with the clear stays visible as a conflict.
    let heads = entry["heads"][0].as_str().unwrap().to_owned();
    let _ = heads;
    let repo = Repo::new();
    let base = repo.set("alice", NOON, &["item", "k", "label=a"]);
    repo.set("alice", NOON + 1, &["item", "k", "label=b"]);
    let (code, _, _) = repo.run("bob", NOON + 2, &["ledger", "clear", "item", "k", "--parents", &base], "");
    assert_eq!(code, 0);
    let entry = repo.json(&["ledger", "get", "item", "k"]);
    assert_eq!(entry["conflicts"]["label"].as_array().unwrap().len(), 2);
}

#[test]
fn a_retired_field_or_kind_is_read_kept_and_never_written() {
    let repo = Repo::with("\n[ledger]\n\n[ledger.kinds.item]\nlabel = \"string\"\nsize = \"number\"\n\n[ledger.kinds.old]\nnote = \"string\"\n");
    repo.set("alice", NOON, &["item", "k", "label=a", "size=3"]);
    repo.set("alice", NOON, &["old", "o", "note=n"]);
    let pending = repo.root.join("p.jsonl");
    let arg = pending.to_string_lossy().into_owned();
    let waiting = repo.set("alice", NOON + 1, &["item", "k", "size=4", "--to", &arg]);
    repo.config("\n[ledger]\nretired = [\"old\"]\n\n[ledger.kinds.item]\nlabel = \"string\"\nsize = { type = \"number\", retired = true }\n\n[ledger.kinds.old]\nnote = \"string\"\n");
    assert_eq!(repo.json(&["ledger", "get", "item", "k"])["set"], json!({"label": "a"}));
    assert!(repo.refused("alice", &["ledger", "set", "item", "k", "size=5"]).contains("item.size is retired"));
    assert!(repo.refused("alice", &["ledger", "set", "old", "o", "note=m"]).contains("kind old is retired"));
    assert!(repo.refused("alice", &["ledger", "get", "old", "o"]).contains("kind old is retired"));
    assert_eq!(repo.json(&["ledger", "fold"]).get("old"), None);
    assert_eq!(repo.json(&["ledger", "log", "old", "o"])[0]["set"], json!({"note": "n"}));
    assert_eq!(repo.json(&["ledger", "log", "item", "k"])[0]["set"]["size"], json!(3));
    // A pending line that sets the retired field is not promoted, and stays.
    assert!(repo.refused("alice", &["ledger", "promote", &arg]).contains("item.size is retired"));
    assert!(std::fs::read_to_string(&pending).unwrap().contains(&waiting));
    // The old type still checks the old lines: a retired field keeps its declaration.
    let (code, out, _) = repo.run("alice", NOON, &["ledger", "check"], "");
    assert_eq!(code, 0, "{out}");
}

#[test]
fn a_damaged_cache_or_an_unreadable_folder_is_never_an_empty_state() {
    let repo = Repo::new();
    repo.set("alice", NOON, &["item", "k", "label=a"]);
    repo.json(&["ledger", "get", "item", "k"]);
    let db = rusqlite::Connection::open(repo.cache_file()).unwrap();
    assert_eq!(db.execute("UPDATE entries SET state = 'not json'", []).unwrap(), 1);
    drop(db);
    assert!(repo.refused("alice", &["ledger", "get", "item", "k"]).contains("the ledger cache is damaged"));
    assert_eq!(repo.json(&["ledger", "get", "item", "k"])["set"]["label"], json!("a"), "the next command builds it again");
    let db = rusqlite::Connection::open(repo.cache_file()).unwrap();
    assert_eq!(db.execute("UPDATE bodies SET places = x'0102'", []).unwrap(), 1);
    drop(db);
    assert!(repo.refused("alice", &["ledger", "log", "item", "k"]).contains("the ledger cache is damaged"));
    assert_eq!(repo.json(&["ledger", "log", "item", "k"]).as_array().unwrap().len(), 1);
    // The ledger folder is a file: refused, not read as an empty ledger.
    let repo = Repo::new();
    std::fs::write(repo.root.join("ledger"), b"not a folder").unwrap();
    for argv in [vec!["ledger", "fold"], vec!["ledger", "check"]] {
        assert!(repo.refused("alice", &argv).contains("cannot be listed"), "{argv:?}");
    }
}


#[test]
fn a_batch_keeps_each_entry_heads_as_it_goes() {
    let repo = Repo::new();
    let base = repo.set("alice", NOON, &["item", "k", "label=base"]);
    let stdin = format!(
        "{}\n{}\n{}\n",
        r#"{"kind":"item","key":"k","set":{"label":"one"}}"#,
        format_args!(r#"{{"kind":"item","key":"k","set":{{"label":"two"}},"parents":["{base}"]}}"#),
        r#"{"kind":"item","key":"k","set":{"size":3}}"#
    );
    let (code, out, err) = repo.run("alice", NOON + 1, &["--json", "ledger", "set", "--batch"], &stdin);
    assert_eq!(code, 0, "{out}{err}");
    let written: Value = serde_json::from_str(&out).unwrap();
    let ids: Vec<&str> = written["lines"].as_array().unwrap().iter().map(|line| line["id"].as_str().unwrap()).collect();
    assert_eq!(written["lines"][0]["parents"], json!([base]));
    assert_eq!(written["lines"][1]["parents"], json!([base]), "its own parents: concurrent with the first");
    let mut both = vec![ids[0], ids[1]];
    both.sort();
    assert_eq!(written["lines"][2]["parents"], json!(both), "after both heads the batch made");
    assert!(written["notes"][0].as_str().unwrap().contains("conflict on label"), "{written}");
    let entry = repo.json(&["ledger", "get", "item", "k"]);
    assert_eq!(entry["heads"], json!([ids[2]]));
    assert_eq!(entry["conflicts"]["label"].as_array().unwrap().len(), 2, "size was set after both; label still differs");
}

fn write_pending(path: &std::path::Path, lines: &[&Line]) {
    let text: String = lines.iter().map(|line| format!("{}\n", line.text())).collect();
    std::fs::write(path, text).unwrap();
}

#[test]
fn a_pending_line_is_held_to_the_journals_graph_before_it_is_read_or_moved() {
    // journal -> pending: a pending line names a parent that a journal shows
    // is another entry's line.
    let repo = Repo::new();
    let a = repo.set("alice", NOON, &["item", "a", "label=A"]);
    let journal_before = std::fs::read(repo.journal("alice")).unwrap();
    let file = repo.root.join("f.jsonl");
    let arg = file.to_string_lossy().into_owned();
    let b = new_line("b", json!({"label": "B"}), &[&a], "alice", 9);
    write_pending(&file, &[&b]);
    let pending_before = std::fs::read(&file).unwrap();
    for argv in [
        vec!["ledger", "--pending", &arg, "get", "item", "b"],
        vec!["ledger", "--pending", &arg, "find", "item", "--where", "label=B"],
        vec!["ledger", "--pending", &arg, "fold"],
        vec!["ledger", "export", &arg, "--id", &b.id],
        vec!["ledger", "promote", &arg],
        vec!["ledger", "promote", &arg, "--id", &b.id, "--keep"],
    ] {
        assert!(repo.refused("alice", &argv).contains("which is a line of item 'a'"), "{argv:?}");
    }
    assert_eq!(std::fs::read(&file).unwrap(), pending_before, "a refusal leaves the pending file as it was");
    assert_eq!(std::fs::read(repo.journal("alice")).unwrap(), journal_before, "and the journal");
    assert_eq!(repo.json(&["ledger", "fold", "--with-state"])["complete"], json!(true), "the journals still read");

    // pending -> pending: both lines only pending.
    let repo = Repo::new();
    let file = repo.root.join("f.jsonl");
    let arg = file.to_string_lossy().into_owned();
    let a = new_line("a", json!({"label": "A"}), &[], "alice", 1);
    let b = new_line("b", json!({"label": "B"}), &[&a.id], "alice", 2);
    write_pending(&file, &[&a, &b]);
    assert!(repo.refused("alice", &["ledger", "--pending", &arg, "get", "item", "a"]).contains("which is a line of item 'a'"));
    assert!(repo.refused("alice", &["ledger", "promote", &arg, "--id", &a.id, "--id", &b.id]).contains("which is a line of item 'a'"));
    assert!(!repo.journal("alice").exists());

    // A pending line that a journal line of another entry waits for as its parent.
    let repo = Repo::new();
    let file = repo.root.join("f.jsonl");
    let arg = file.to_string_lossy().into_owned();
    let late = new_line("d", json!({"label": "D"}), &[], "alice", 1);
    repo.write_line("bob", &new_line("c", json!({"label": "C"}), &[&late.id], "bob", 2));
    write_pending(&file, &[&late]);
    for argv in [vec!["ledger", "--pending", &arg, "get", "item", "d"], vec!["ledger", "promote", &arg]] {
        assert!(repo.refused("alice", &argv).contains("which is a pending line of item 'd'"), "{argv:?}");
    }
    assert!(!repo.journal("alice").exists());

    // A pending line whose parent is its own entry's journal line reads and moves.
    let repo = Repo::new();
    let k = repo.set("alice", NOON, &["item", "k", "label=one"]);
    let file = repo.root.join("f.jsonl");
    let arg = file.to_string_lossy().into_owned();
    let next = new_line("k", json!({"label": "two"}), &[&k], "alice", 30);
    write_pending(&file, &[&next]);
    assert_eq!(repo.json(&["ledger", "--pending", &arg, "get", "item", "k"])["set"]["label"], json!("two"));
    assert_eq!(repo.json(&["ledger", "promote", &arg])["moved"], json!([next.id]));
}

#[test]
fn find_is_complete_only_when_the_whole_kind_is() {
    let repo = Repo::new();
    repo.set("alice", NOON, &["item", "a", "label=A"]);
    // b's parent has not arrived: it may set b's label to A, or set size.
    repo.write_line("bob", &new_line("b", json!({"label": "B"}), &["1111111111111111"], "bob", 3));
    let found = repo.json(&["ledger", "find", "item", "--where", "label=Z"]);
    assert_eq!(found["entries"], json!([]));
    assert_eq!(found["complete"], json!(false), "an empty answer is not complete either");
    assert!(found["gaps"][0].as_str().unwrap().contains("1111111111111111"));
    for argv in [vec!["ledger", "find", "item", "--where", "label=Z", "--strict"], vec!["ledger", "find", "item", "--missing", "size", "--strict"]] {
        assert!(repo.refused("alice", &argv).contains("not complete (--strict)"), "{argv:?}");
    }
    let (code, out, _) = repo.run("alice", NOON, &["ledger", "find", "item", "--where", "label=Z"], "");
    assert_eq!(code, 0);
    assert!(out.contains("nothing found") && out.contains("not complete:"), "{out}");
    // A kind with no missing parent is complete.
    let repo = Repo::new();
    repo.set("alice", NOON, &["item", "a", "label=A"]);
    let found = repo.json(&["ledger", "find", "item", "--where", "label=Z", "--strict"]);
    assert_eq!(found, json!({"complete": true, "gaps": [], "entries": []}));
}

#[test]
fn a_failed_write_says_what_may_have_happened() {
    let repo = Repo::new();
    repo.set("alice", NOON, &["item", "k", "label=one"]);
    // Cut off half way: some bytes are in, the outcome is not confirmed.
    codelore::ledger::inject_fault(Some("append:write"));
    let (code, out, err) = repo.run("alice", NOON + 1, &["ledger", "set", "item", "k", "label=two"], "");
    codelore::ledger::inject_fault(None);
    assert_eq!(code, 1);
    let said = format!("{out}{err}");
    assert!(said.contains("not confirmed") && said.contains("do not write them again") && !said.contains("were not written"), "{said}");
    assert!(said.lines().map(|line| line.trim().trim_start_matches("- ")).any(|id| id.len() == 16 && id.chars().all(|c| c.is_ascii_hexdigit())), "the line's id is named: {said}");
    assert!(!std::fs::read(repo.journal("alice")).unwrap().ends_with(b"\n"), "half a line is in");
    // repair sets the half line aside; a failed cut says where the bytes are.
    codelore::ledger::inject_fault(Some("repair:cut"));
    let said = repo.refused("alice", &["ledger", "repair"]);
    codelore::ledger::inject_fault(None);
    assert!(said.contains("may or may not have been cut") && said.contains(".torn-1"), "{said}");
    let (code, out, _) = repo.run("alice", NOON, &["--json", "ledger", "repair"], "");
    assert_eq!(code, 0, "{out}");
    assert!(std::fs::read(repo.journal("alice")).unwrap().ends_with(b"\n"));
    // Written, then the sync fails: written but not confirmed.
    codelore::ledger::inject_fault(Some("append:sync"));
    let said = repo.refused("alice", &["ledger", "set", "item", "k", "label=three"]);
    codelore::ledger::inject_fault(None);
    assert!(said.contains("written but are not confirmed on disk"), "{said}");
    assert_eq!(repo.json(&["ledger", "get", "item", "k"])["set"]["label"], json!("three"));
}

#[test]
fn repairing_a_pending_file_waits_for_its_lock() {
    use fs2::FileExt;
    let repo = Repo::new();
    let pending = repo.root.join("pending.jsonl");
    let arg = pending.to_string_lossy().into_owned();
    repo.set("alice", NOON, &["item", "k", "label=one", "--to", &arg]);
    let mut bytes = std::fs::read(&pending).unwrap();
    bytes.extend_from_slice(b"{\"v\":4,\"id\":\"ab");
    std::fs::write(&pending, &bytes).unwrap();
    // A promote holds the lock from its read to its rewrite.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(repo.root.join("pending.jsonl.lock")).unwrap();
    lock.lock_exclusive().unwrap();
    let root = repo.root.clone();
    let given = arg.clone();
    let repairer = std::thread::spawn(move || {
        let env = Env { cwd: root, lore_by: Some("alice".into()), machine: Some("m1".into()), now: Some(NOON), ..Env::default() };
        cli::run(&["ledger", "repair", &given], &env).code
    });
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert_eq!(std::fs::read(&pending).unwrap(), bytes, "repair waits for the lock");
    fs2::FileExt::unlock(&lock).unwrap();
    assert_eq!(repairer.join().unwrap(), 0);
    assert!(std::fs::read(&pending).unwrap().ends_with(b"\n"));
    assert!(repo.root.join("pending.jsonl.torn-1").exists());
}

#[test]
fn a_journal_with_crlf_line_ends_reads_as_lf() {
    let repo = Repo::new();
    let first = new_line("k", json!({"label": "one"}), &[], "bob", 1);
    let second = new_line("k", json!({"label": "two"}), &[&first.id], "bob", 2);
    let path = repo.journal("bob");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // As a copy that converts line ends leaves it.
    std::fs::write(&path, format!("{}\r\n{}\r\n", first.text(), second.text())).unwrap();
    assert_eq!(repo.json(&["ledger", "get", "item", "k"])["set"]["label"], json!("two"));
    // A writer appends LF lines after them; both kinds read, warm as cold.
    repo.set("bob", NOON, &["item", "k", "label=three"]);
    let warm = repo.json(&["ledger", "fold", "--with-state"]);
    std::fs::remove_file(repo.cache_file()).unwrap();
    let cold = repo.json(&["ledger", "fold", "--with-state"]);
    assert_eq!(warm, cold);
    assert_eq!(cold["entries"]["item"]["k"]["set"]["label"], json!("three"));
    let (code, out, _) = repo.run("alice", NOON, &["ledger", "check"], "");
    assert_eq!(code, 0, "{out}");
    // A CR anywhere but before the LF is refused.
    let repo = Repo::new();
    let path = repo.journal("bob");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("{}\r\r\n", first.text())).unwrap();
    assert!(repo.refused("alice", &["ledger", "get", "item", "k"]).contains("a CR inside the line"));
}

#[test]
fn a_journal_line_never_rests_on_a_line_only_pending() {
    let repo = Repo::new();
    let file = repo.root.join("p.jsonl");
    let arg = file.to_string_lossy().into_owned();
    let pending = repo.set("alice", NOON, &["item", "k", "label=one", "--to", &arg]);
    // Its heads include the pending line: to the journal, refused; to the file, written.
    assert!(repo.refused("alice", &["ledger", "--pending", &arg, "set", "item", "k", "label=two"]).contains("is only in a pending file"));
    assert!(!repo.journal("alice").exists());
    assert!(repo.refused("alice", &["ledger", "set", "item", "k", "label=two", "--parents", &pending, "--pending", &arg]).contains("is only in a pending file"));
    repo.set("alice", NOON + 1, &["item", "k", "label=two", "--to", &arg]);
    // Once promoted, the journal holds it, and a journal line may rest on it.
    repo.json(&["ledger", "promote", &arg]);
    repo.set("alice", NOON + 2, &["item", "k", "label=three"]);
    assert_eq!(repo.json(&["ledger", "get", "item", "k", "--strict"])["set"]["label"], json!("three"));
}

#[test]
fn a_ledger_folder_is_never_the_repository_itself() {
    for dir in ["\\\\", "/", "a//b", "./x", "x/..", ""] {
        let repo = Repo::with(&format!("\n[ledger]\ndir = {}\n\n[ledger.kinds.item]\nlabel = \"string\"\n", toml_string(dir)));
        assert!(repo.refused("alice", &["ledger", "status"]).contains("ledger.dir"), "{dir:?}");
    }
    let repo = Repo::with("\n[ledger]\ndir = \"data\\\\ledger\\\\\"\n\n[ledger.kinds.item]\nlabel = \"string\"\n");
    repo.set("alice", NOON, &["item", "k", "label=one"]);
    assert!(repo.root.join("data/ledger/alice@m1/2026-11.jsonl").exists());
}

fn toml_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\"))
}
