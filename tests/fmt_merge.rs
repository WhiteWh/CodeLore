//! A merge that joins both sides' lines (a plain concatenation) leaves a lore
//! file out of order with its common records twice. A reader refuses it and
//! says how to mend it; `lore fmt` sorts it and keeps one of each same record,
//! and leaves two different records under one id to a person.
use codelore::cli::{self, Env};
use codelore::store::{canonical, serialize};
use serde_json::{json, Value};

fn fault(id: &str, fix: &str) -> Value {
    json!({"v": 3, "id": id, "kind": "fault", "area": "notes", "title": format!("title of {id}"), "symptom": "s", "cause": "c", "fix": fix, "check": "k",
           "status": "live", "created": "2026-11-08", "updated": "2026-11-08", "by": "me"})
}

#[test]
fn a_joined_file_is_named_and_mended_by_fmt() {
    let tmp = tempfile::tempdir().unwrap();
    let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap());
    std::fs::write(root.join("lore.toml"), "version = 1\ndir = \"lore\"\nareas = [\"notes\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n").unwrap();
    std::fs::create_dir_all(root.join("lore")).unwrap();
    let env = Env { cwd: root.clone(), ..Env::default() };
    let file = root.join("lore/notes.jsonl");
    // Ours: aa, mm. Theirs: aa, bb. Joined: ours then theirs.
    let ours = serialize(&[fault("aa", "f"), fault("mm", "f")]);
    let theirs = serialize(&[fault("aa", "f"), fault("bb", "f")]);
    std::fs::write(&file, [ours, theirs].concat()).unwrap();
    let read = cli::run(&["check"], &env);
    let said = format!("{}{}", read.stdout, read.stderr);
    assert_ne!(read.code, 0);
    assert!(said.contains("`lore fmt` sorts the file"), "{said}");
    let fmt = cli::run(&["fmt"], &env);
    assert_eq!(fmt.code, 0, "{}{}", fmt.stdout, fmt.stderr);
    let text = std::fs::read_to_string(&file).unwrap();
    let ids: Vec<String> = text.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids, ["aa", "bb", "mm"]);
    assert_eq!(cli::run(&["check"], &env).code, 0);

    // Both sides changed one record: fmt does not choose.
    let mine = canonical(&fault("aa", "mine"));
    let yours = canonical(&fault("aa", "yours"));
    std::fs::write(&file, format!("{mine}\n{yours}\n")).unwrap();
    let fmt = cli::run(&["fmt"], &env);
    let said = format!("{}{}", fmt.stdout, fmt.stderr);
    assert_ne!(fmt.code, 0);
    assert!(said.contains("two different records") && said.contains("keep one by hand"), "{said}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{mine}\n{yours}\n"), "nothing rewritten");
}
