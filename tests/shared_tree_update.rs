//! In shared-tree mode a record is updated as the branch lore writes to has
//! it. One that is only in the working tree is not there, and the refusal says
//! so instead of a bare "does not exist".
use codelore::cli::{self, Env};
use std::path::Path;

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn a_record_only_in_the_working_tree_is_named_as_such() {
    let tmp = tempfile::tempdir().unwrap();
    let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap());
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "T"]);
    git(&root, &["config", "user.email", "t@example.invalid"]);
    std::fs::write(root.join("lore.toml"), "version = 1\ndir = \"lore\"\nareas = [\"notes\"]\n\n[commit]\nmode = \"shared-tree\"\nbranch = \"main\"\n\n[embed]\nprovider = \"none\"\n").unwrap();
    std::fs::create_dir_all(root.join("lore")).unwrap();
    std::fs::write(root.join("lore/.keep"), "").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "init"]);
    let record = r#"{"area":"notes","title":"the cache is cold","symptom":"the first frame is black","signals":["first frame black","black at start"],"cause":"c","fix":"f","check":"k"}"#;
    let run = |argv: &[&str], stdin: &str| {
        let env = Env { cwd: root.clone(), stdin: stdin.as_bytes().to_vec(), lore_by: Some("t".into()), ..Env::default() };
        cli::run(argv, &env)
    };
    let added = run(&["--json", "add", "fault", "--json-in"], record);
    assert_eq!(added.code, 0, "{}{}", added.stdout, added.stderr);
    // The commit is undone; the record stays in the working tree.
    git(&root, &["reset", "-q", "--mixed", "HEAD~1"]);
    let updated = run(&["update", "the-cache-is-cold", "-s", "fix=g"], "");
    assert_eq!(updated.code, 1);
    let said = format!("{}{}", updated.stdout, updated.stderr);
    assert!(said.contains("is in the working tree but not on branch main") && said.contains("commit or merge it into main first"), "{said}");
    // A record that is nowhere still does not exist.
    let missing = run(&["update", "no-such-record", "-s", "fix=g"], "");
    assert!(format!("{}{}", missing.stdout, missing.stderr).contains("'no-such-record' does not exist"));
}
