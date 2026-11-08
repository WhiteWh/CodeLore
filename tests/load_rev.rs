//! A store read at a commit (`shared-tree`'s base): every file in one
//! `git cat-file --batch`, not a process per file, and a path the commit does
//! not have is missing - never an empty file.
use codelore::gitio;
use std::path::Path;

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

const RECORD_A: &str = r#"{"area":"git","by":"me","created":"2026-11-08","id":"a-fault","kind":"fault","status":"live","symptom":"x","title":"a fault","updated":"2026-11-08","v":3}"#;
const RECORD_B: &str = r#"{"area":"build","by":"me","created":"2026-11-08","id":"b-fault","kind":"fault","status":"live","symptom":"y missing","title":"b fault","updated":"2026-11-08","v":3}"#;

fn repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@example.invalid"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::create_dir_all(root.join("lore")).unwrap();
    std::fs::write(root.join("lore.toml"), "version = 1\ndir = \"lore\"\nareas = [\"git\", \"build\"]\n\n[commit]\nmode = \"shared-tree\"\nbranch = \"main\"\n\n[embed]\nprovider = \"none\"\n").unwrap();
    std::fs::write(root.join("lore/git.jsonl"), format!("{RECORD_A}\n")).unwrap();
    std::fs::write(root.join("lore/build.jsonl"), format!("{RECORD_B}\n")).unwrap();
    // A file whose bytes hold the batch format's own words, and no final newline.
    std::fs::write(root.join("notes.txt"), "HEAD:x missing\n0 blob 12").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    tmp
}

#[test]
fn blobs_reads_every_path_in_one_process_and_names_the_missing_one() {
    let tmp = repo();
    let paths = vec!["lore/git.jsonl".to_owned(), "lore/gone.jsonl".to_owned(), "notes.txt".to_owned(), "lore/build.jsonl".to_owned()];
    let (found, calls) = gitio::count_calls(|| gitio::blobs(tmp.path(), "HEAD", &paths).unwrap());
    assert_eq!(calls, ["cat-file"]);
    assert_eq!(found["lore/git.jsonl"].as_deref(), Some(format!("{RECORD_A}\n").as_bytes()));
    assert_eq!(found["lore/gone.jsonl"], None);
    assert_eq!(found["notes.txt"].as_deref(), Some("HEAD:x missing\n0 blob 12".as_bytes()));
    assert_eq!(found["lore/build.jsonl"].as_deref(), Some(format!("{RECORD_B}\n").as_bytes()));
}

#[test]
fn load_rev_reads_the_store_at_a_commit_with_one_cat_file() {
    let tmp = repo();
    let config = codelore::config::load(tmp.path()).unwrap();
    let head = gitio::head(tmp.path());
    let (snapshot, calls) = gitio::count_calls(|| codelore::ops::load_rev(&config, head.as_deref()).unwrap());
    assert_eq!(calls.iter().filter(|call| *call == "cat-file").count(), 1, "{calls:?}");
    let ids: Vec<String> = snapshot.all().iter().map(|record| record["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"a-fault".to_owned()) && ids.contains(&"b-fault".to_owned()), "{ids:?}");
}
