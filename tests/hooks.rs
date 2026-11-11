//! What a session sees without asking: `lore digest` with pinned records
//! first, and `lore hook pre-edit` naming what is known about a file about to
//! change. `LORE_STORE` finds the store without a path in the hook's command.
use codelore::cli::{self, Env};
use codelore::Store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn fault(id: &str, title: &str, updated: &str, extra: Value) -> Value {
    let mut record = json!({"v": 3, "id": id, "kind": "fault", "area": "a", "title": title, "symptom": "s", "cause": "c", "fix": format!("fix of {id}"),
        "check": "k", "status": "live", "created": "2026-01-01", "updated": updated, "by": "me"});
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    record
}

/// A project repository with a source file, and a store elsewhere (as a
/// Zerus store is) whose records cite the project's file.
fn project_and_store(records: &[Value]) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap());
    let project = base.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    std::fs::write(project.join("src/import.py"), "def run(): pass\n").unwrap();
    let store = base.join("store");
    std::fs::create_dir_all(store.join("lore")).unwrap();
    std::fs::write(store.join("lore.toml"), "version = 1\ndir = \"lore\"\nareas = [\"a\", \"b\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n").unwrap();
    Store::new(&store).write("lore/a.jsonl", records).unwrap();
    (tmp, project, store)
}

#[test]
fn a_pinned_record_is_in_the_digest_however_old() {
    let mut records: Vec<Value> = (0..40).map(|at| fault(&format!("new-{at:02}"), &format!("a newer fault, number {at:02}"), "2026-11-01", json!({}))).collect();
    records.push(fault("base-trap", "the table import is blind to a nested name", "2025-01-01", json!({"pinned": true})));
    records.push(fault("old-trap", "an old trap nobody pinned", "2025-01-01", json!({})));
    let (_tmp, _project, store) = project_and_store(&records);
    let env = Env { cwd: store.clone(), ..Env::default() };
    let out = cli::run(&["digest", "--bytes", "300"], &env);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert!(lines[1].starts_with("* fault [a] base-trap:"), "pinned first: {}", out.stdout);
    assert!(!out.stdout.contains("old-trap"), "the unpinned old one is cut: {}", out.stdout);
    // a pinned record of another area is shown under --area too
    let out = cli::run(&["--json", "digest", "--area", "b"], &env);
    let digest: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(digest["records"][0]["id"], "base-trap", "{digest}");
    assert_eq!(digest["records"][0]["pinned"], true);
}

#[test]
fn pinned_is_true_or_false() {
    let (_tmp, _project, store) = project_and_store(&[fault("one", "one fault", "2026-01-01", json!({"pinned": "yes"}))]);
    let out = cli::run(&["check"], &Env { cwd: store, ..Env::default() });
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.stdout.contains("`pinned` is true or false"), "{}", out.stdout);
}

#[test]
fn the_edit_hook_names_what_is_known_about_the_file_once_a_session() {
    let records = [
        fault("import-blind", "the importer is blind to a nested name", "2026-01-01", json!({"refs": {"files": ["src/import.py"]}})),
        fault("import-cured", "an import fault already cured", "2026-01-01", json!({"status": "cured", "refs": {"files": ["src/import.py"]}})),
    ];
    let (_tmp, project, store) = project_and_store(&records);
    let file = project.join("src/import.py");
    let event = |session: &str| json!({"session_id": session, "cwd": project, "hook_event_name": "PreToolUse", "tool_name": "Edit", "tool_input": {"file_path": file}}).to_string();
    let run = |session: &str| {
        // the hook runs in the project; the store comes from LORE_STORE
        let env = Env { cwd: project.clone(), store: Some(store.clone()), stdin: event(session).into_bytes(), ..Env::default() };
        cli::run(&["hook", "pre-edit"], &env)
    };
    let out = run("session-one");
    assert_eq!(out.code, 0, "{}", out.stderr);
    let said: Value = serde_json::from_str(&out.stdout).unwrap_or_else(|_| panic!("JSON: {}", out.stdout));
    let context = said["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert_eq!(said["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert!(context.contains("src/import.py") && context.contains("import-blind") && context.contains("fix of import-blind"), "{context}");
    assert!(!context.contains("import-cured"), "only what is in force: {context}");
    assert_eq!(run("session-one").stdout, "", "once a session");
    assert!(!run("session-two").stdout.is_empty(), "another session is told again");
}

#[test]
fn a_hook_without_a_store_or_a_known_file_says_nothing() {
    let (_tmp, project, store) = project_and_store(&[]);
    let nowhere = Env { cwd: project.clone(), stdin: b"{}".to_vec(), ..Env::default() };
    for event in ["session-start", "pre-edit"] {
        let out = cli::run(&["hook", event], &nowhere);
        assert_eq!((out.code, out.stdout.as_str()), (0, ""), "{event}: {}", out.stderr);
    }
    let event = json!({"session_id": "s", "tool_input": {"file_path": project.join("src/import.py")}}).to_string();
    let out = cli::run(&["hook", "pre-edit"], &Env { cwd: project, store: Some(store), stdin: event.into_bytes(), ..Env::default() });
    assert_eq!((out.code, out.stdout.as_str()), (0, ""));
}
