//! `lore cover --keep`: a document that stays the authority (a guide people
//! read) is accounted for whole by the records that point at it.
//! `lore cover --moved`: a document moved into an archive keeps its source
//! record and the records that cite it. And `lore --help`.
use codelore::cli::{self, Env};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn repo() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap());
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.invalid"]);
    git(&root, &["config", "user.name", "t"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("lore.toml"), "version = 1\ndir = \"lore\"\nareas = [\"docs\"]\n\n[commit]\nmode = \"plain\"\nbranch = \"main\"\n\n[embed]\nprovider = \"none\"\n").unwrap();
    std::fs::create_dir_all(root.join("lore")).unwrap();
    std::fs::create_dir_all(root.join("Docs/Guides")).unwrap();
    std::fs::write(root.join("Docs/Guides/level.md"), "# Level guide\n\nHow designers lay out a level.\n\n## Lights\n\nUse the light kit.\n\n## Doors\n\nDoors open inward.\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "start"]);
    (tmp, root)
}

fn run(root: &Path, args: &[&str], stdin: Option<&str>) -> cli::Output {
    let env = Env { cwd: root.to_path_buf(), stdin: stdin.unwrap_or_default().as_bytes().to_vec(), ..Env::default() };
    let mut argv = vec!["--by", "agent:test"];
    argv.extend_from_slice(args);
    cli::run(&argv, &env)
}

fn pointer(root: &Path) -> String {
    let topic = r#"{"area": "docs", "title": "the level guide is the authority on level layout", "body": "Level layout rules live in Docs/Guides/level.md; read it there.", "status": "current", "refs": {"files": ["Docs/Guides/level.md"]}}"#;
    let out = run(root, &["--json", "add", "topic", "--json-in"], Some(topic));
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let written: Value = serde_json::from_str(&out.stdout).unwrap();
    written["records"][0]["id"].as_str().unwrap().to_owned()
}

#[test]
fn a_kept_document_is_accounted_for_whole() {
    let (_tmp, root) = repo();
    let id = pointer(&root);
    assert_eq!(run(&root, &["cover", "Docs/Guides/level.md"], None).code, 1, "nothing is accounted for yet");
    let refused = run(&root, &["cover", "Docs/Guides/level.md", "--keep"], None);
    assert_eq!(refused.code, 1);
    assert!(refused.stderr.contains("--keep needs --reason"), "{}", refused.stderr);
    let out = run(&root, &["cover", "Docs/Guides/level.md", "--keep", "--reason", "a guide for designers, kept as Markdown", "--record", &id], None);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let shown = run(&root, &["cover", "Docs/Guides/level.md"], None);
    assert_eq!(shown.code, 0, "{}{}", shown.stdout, shown.stderr);
    assert!(shown.stdout.contains("kept, the document is the authority") && shown.stdout.contains(&id), "{}", shown.stdout);
    let check = run(&root, &["check"], None);
    assert_eq!(check.code, 0, "{}{}", check.stdout, check.stderr);
}

#[test]
fn a_moved_document_takes_its_source_and_citations_along() {
    let (_tmp, root) = repo();
    let id = pointer(&root);
    let parts = "{\"part\": \"Level guide\", \"records\": [\"".to_owned() + &id + "\"]}\n";
    let out = run(&root, &["cover", "Docs/Guides/level.md", "--add"], Some(&parts));
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    std::fs::create_dir_all(root.join("Docs/_Archive")).unwrap();
    git(&root, &["mv", "Docs/Guides/level.md", "Docs/_Archive/level.md"]);
    git(&root, &["commit", "-qm", "archive the level guide"]);
    let lost = run(&root, &["check"], None);
    assert!(lost.stdout.contains("--moved") || lost.stderr.contains("--moved"), "the check names the way out: {}{}", lost.stdout, lost.stderr);
    let out = run(&root, &["--json", "cover", "Docs/Guides/level.md", "--moved", "Docs/_Archive/level.md"], None);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let written: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(written["records"].as_array().unwrap().len(), 2, "the source and the topic: {written}");
    let cover = run(&root, &["--json", "cover", "Docs/_Archive/level.md"], None);
    let cover: Value = serde_json::from_str(&cover.stdout).unwrap();
    assert_eq!(cover["complete"], true, "{cover}");
    let topic: Value = serde_json::from_str(&run(&root, &["--json", "show", &id], None).stdout).unwrap();
    assert_eq!(topic["refs"]["files"], serde_json::json!(["Docs/_Archive/level.md"]), "{topic}");
    let check = run(&root, &["check"], None);
    assert_eq!(check.code, 0, "{}{}", check.stdout, check.stderr);
    let again = run(&root, &["cover", "Docs/Guides/level.md", "--moved", "Docs/_Archive/level.md"], None);
    assert_eq!(again.code, 1, "nothing names the old path now");
}

#[test]
fn help_lists_the_commands() {
    let (_tmp, root) = repo();
    for argv in [&["--help"][..], &["-h"], &["help"], &["find", "--help"]] {
        let out = cli::run(argv, &Env { cwd: root.clone(), ..Env::default() });
        assert_eq!(out.code, 0, "{argv:?}");
        for command in ["find", "digest", "cover", "plan", "atlas"] {
            assert!(out.stdout.contains(command), "{argv:?}: {command}");
        }
    }
}
