//! The ledger works wherever its cache cannot be written: a sandbox that
//! denies the user cache gets it in the temporary folder, one that denies
//! that too gets the state folded in memory - the same answer, with a warning.
//! One test in its own process: it changes LORE_CACHE and TEMP.
use codelore::cli::{self, Env};
use serde_json::Value;

const CONFIG: &str = "version = 1\ndir = \"lore\"\nareas = [\"notes\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n\n[ledger]\n\n[ledger.kinds.item]\nlabel = \"string\"\n";

#[test]
fn the_ledger_reads_when_no_cache_folder_can_be_written() {
    let tmp = tempfile::tempdir().unwrap();
    let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap()).join("repo");
    std::fs::create_dir_all(root.join("lore")).unwrap();
    std::fs::write(root.join("lore.toml"), CONFIG).unwrap();
    // A plain file stands where a folder would have to be made.
    let blocker = tmp.path().join("not-a-folder");
    std::fs::write(&blocker, "").unwrap();
    let run = |argv: &[&str]| {
        let env = Env { cwd: root.clone(), lore_by: Some("alice".into()), machine: Some("m1".into()), now: Some(1_794_225_600), ..Env::default() };
        cli::run(argv, &env)
    };
    let set = run(&["ledger", "set", "item", "k", "label=one"]);
    assert_eq!(set.code, 0, "{}{}", set.stdout, set.stderr);

    std::env::set_var("LORE_CACHE", blocker.join("cache"));
    let got = run(&["--json", "ledger", "get", "item", "k"]);
    assert_eq!(got.code, 0, "{}{}", got.stdout, got.stderr);
    assert!(got.stderr.contains("the ledger cache is in"), "{}", got.stderr);
    assert_eq!(serde_json::from_str::<Value>(&got.stdout).unwrap()["set"]["label"], "one");

    for var in ["TEMP", "TMP", "TMPDIR"] {
        std::env::set_var(var, blocker.join("temp"));
    }
    let set = run(&["ledger", "set", "item", "k", "label=two"]);
    assert_eq!(set.code, 0, "{}{}", set.stdout, set.stderr);
    let got = run(&["--json", "ledger", "get", "item", "k"]);
    assert_eq!(got.code, 0, "{}{}", got.stdout, got.stderr);
    assert!(got.stderr.contains("folded in memory"), "{}", got.stderr);
    assert_eq!(serde_json::from_str::<Value>(&got.stdout).unwrap()["set"]["label"], "two");
    let fold = run(&["--json", "ledger", "fold", "--with-state", "--strict"]);
    assert_eq!(fold.code, 0, "{}{}", fold.stdout, fold.stderr);
}
