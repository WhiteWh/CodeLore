//! Writers at once: every record lands, and the duplicate check sees what the
//! writer before it wrote. Before the write lock, eight `lore add` at once in
//! `plain` mode committed one record and lost seven (git's index.lock, and a
//! file replaced over another writer's line).
use codelore::cli::{self, Env};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn repo(mode: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap());
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.invalid"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("lore.toml"), format!("version = 1\ndir = \"lore\"\nareas = [\"a\"]\n\n[commit]\nmode = \"{mode}\"\nbranch = \"main\"\n\n[embed]\nprovider = \"none\"\n")).unwrap();
    std::fs::create_dir_all(root.join("lore")).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "start"]);
    (tmp, root)
}

fn fault(title: &str, signal: &str) -> String {
    serde_json::json!({"area": "a", "title": title, "symptom": format!("{title} is seen"), "signals": [signal, format!("{title} in words")],
        "cause": "c", "fix": "f", "check": "k", "status": "live"}).to_string()
}

fn add_at_once(root: &Path, inputs: Vec<String>) -> Vec<i32> {
    let threads: Vec<_> = inputs
        .into_iter()
        .map(|input| {
            let root = root.to_path_buf();
            std::thread::spawn(move || {
                let env = Env { cwd: root, stdin: input.into_bytes(), ..Env::default() };
                cli::run(&["--by", "agent:test", "add", "fault", "--json-in"], &env).code
            })
        })
        .collect();
    threads.into_iter().map(|thread| thread.join().unwrap()).collect()
}

fn lines(root: &Path) -> usize {
    std::fs::read_to_string(root.join("lore/a.jsonl")).map(|text| text.lines().count()).unwrap_or(0)
}

#[test]
fn writers_at_once_each_land_in_every_mode() {
    for mode in ["plain", "shared-tree"] {
        let (_tmp, root) = repo(mode);
        let titles = ["shader cache misses after resize", "save fails when the disk is full", "tooltip flickers over the minimap",
            "audio stutters while streaming a level", "login hangs behind a proxy", "crash on exit with a gamepad attached"];
        let inputs = titles.iter().enumerate().map(|(at, title)| fault(title, &format!("error E{at}{at}{at}: {title}"))).collect();
        let codes = add_at_once(&root, inputs);
        assert!(codes.iter().all(|code| *code == 0), "{mode}: {codes:?}");
        assert_eq!(lines(&root), 6, "{mode}: every record is in the file");
        let log = Command::new("git").current_dir(&root).args(["log", "--oneline"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).lines().count(), 7, "{mode}: a commit each");
    }
}

#[test]
fn the_same_fault_at_once_is_written_once() {
    let (_tmp, root) = repo("shared-tree");
    let same = fault("the importer is blind to a nested name", "ImportError: nested Name field skipped in row");
    let codes = add_at_once(&root, vec![same.clone(), same.clone(), same]);
    assert_eq!(codes.iter().filter(|code| **code == 0).count(), 1, "{codes:?}");
    assert_eq!(lines(&root), 1);
}
