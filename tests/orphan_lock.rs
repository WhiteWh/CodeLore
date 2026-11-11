//! A write killed in the middle of updating the shared index (an agent's tool
//! timeout kills the whole process tree) leaves git's `index.lock`; every git
//! command in the checkout then stops, and lore's later writes committed but
//! could not update the index, which kept the lore files as they were - a
//! plain `git commit` would have undone the records. lore now removes a lock
//! its own killed write left, says when the index could not be updated, and
//! `lore check` names lore files the index holds otherwise than HEAD.
use codelore::cli::{self, Env};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn repo() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap());
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.invalid"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("lore.toml"), "version = 1\ndir = \"lore\"\nareas = [\"a\"]\n\n[commit]\nmode = \"shared-tree\"\nbranch = \"main\"\n\n[embed]\nprovider = \"none\"\n").unwrap();
    std::fs::create_dir_all(root.join("lore")).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "start"]);
    (tmp, root)
}

fn add(root: &Path, title: &str) -> cli::Output {
    let input = serde_json::json!({"area": "a", "title": title, "symptom": format!("{title} is seen"), "signals": [format!("error: {title}")],
        "cause": "c", "fix": "f", "check": "k", "status": "live"})
    .to_string();
    cli::run(&["--by", "agent:test", "add", "fault", "--json-in"], &Env { cwd: root.to_path_buf(), stdin: input.into_bytes(), ..Env::default() })
}

/// A file made `ago` in the past.
fn stale(path: &Path, text: &str, ago: Duration) {
    std::fs::write(path, text).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - ago).unwrap();
}

#[test]
fn a_lock_a_killed_lore_write_left_is_removed() {
    let (_tmp, root) = repo();
    let out = add(&root, "the importer is blind to a nested name");
    assert_eq!(out.code, 0, "{}", out.stderr);
    // a write killed while its git wrote the index: lore's mark, then git's lock
    stale(&root.join(".git/codelore-index.mark"), "999999", Duration::from_secs(30));
    stale(&root.join(".git/index.lock"), "half an index", Duration::from_secs(29));
    let out = add(&root, "save fails when the disk is full");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!root.join(".git/index.lock").exists(), "the orphan lock is gone");
    assert!(!root.join(".git/codelore-index.mark").exists());
    assert_eq!(git(&root, &["status", "--porcelain", "--", "lore"]), "", "the index and the tree are at HEAD");
}

#[test]
fn a_lock_another_git_holds_is_left_and_the_index_named() {
    let (_tmp, root) = repo();
    // another git's lock: no mark of lore's beside it
    std::fs::write(root.join(".git/index.lock"), "").unwrap();
    let out = add(&root, "the importer is blind to a nested name");
    assert_eq!(out.code, 0, "the commit needs no index: {}", out.stderr);
    assert!(root.join(".git/index.lock").exists(), "somebody else's lock is not lore's to remove");
    let said = format!("{}{}", out.stdout, out.stderr);
    assert!(said.contains("git reset -q -- lore/a.jsonl"), "the stale index entry is named: {said}");
    std::fs::remove_file(root.join(".git/index.lock")).unwrap();
    let out = cli::run(&["check"], &Env { cwd: root.clone(), ..Env::default() });
    assert!(out.stdout.contains("the index holds lore files otherwise than HEAD (D lore/a.jsonl)") && out.stdout.contains("git reset -q -- lore"), "{}", out.stdout);
    git(&root, &["reset", "-q", "--", "lore"]);
    let out = cli::run(&["check"], &Env { cwd: root, ..Env::default() });
    assert!(!out.stdout.contains("the index holds"), "{}", out.stdout);
}
