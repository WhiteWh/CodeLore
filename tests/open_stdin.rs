//! A caller that hands its own stdin on - a harness, a script, an agent's shell -
//! may keep that pipe open without ever writing to it. `lore` must not wait for
//! an EOF there: only the commands that take their input on stdin read it.
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const LORE: &str = env!("CARGO_BIN_EXE_lore");

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git").arg("-C").arg(root).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn lore(root: &Path) -> Command {
    let mut command = Command::new(LORE);
    command.arg("-C").arg(root).env("LORE_MODEL_FETCH", "off").env("LORE_CACHE", root.join(".cache")).stdout(Stdio::piped()).stderr(Stdio::piped());
    command
}

/// Waits for `child` at most `limit`; kills it and fails when it hangs.
fn finishes(mut child: Child, what: &str, limit: Duration) -> std::process::Output {
    let started = Instant::now();
    loop {
        if child.try_wait().expect("wait works").is_some() {
            return child.wait_with_output().expect("output");
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            panic!("`lore {what}` still ran after {limit:?} with an open, silent stdin");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn an_open_silent_stdin_is_not_waited_for() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@example.invalid"]);
    std::fs::write(root.join("README.md"), "# Notes\n\n## Builds\n\nA part.\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    let init = lore(root).args(["init", "--areas", "build"]).stdin(Stdio::null()).output().unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "lore init"]);

    // stdin that carries input is still read: a batch, its pipe closed after writing.
    let mut add = lore(root).args(["--json", "add", "fault", "--batch", "--new"]).stdin(Stdio::piped()).spawn().unwrap();
    let line = r#"{"area":"build","title":"the linker runs out of memory","symptom":"link.exe stops with an out-of-memory error","cause":"the 32-bit linker is picked","fix":"use the 64-bit host toolchain","check":"the build links","status":"live"}"#;
    add.stdin.take().unwrap().write_all(format!("{line}\n").as_bytes()).unwrap();
    let added = finishes(add, "add --batch", Duration::from_secs(60));
    assert!(added.status.success(), "{}{}", String::from_utf8_lossy(&added.stdout), String::from_utf8_lossy(&added.stderr));

    // every other command with a pipe that stays open and silent
    for args in [vec!["--json", "check"], vec!["--json", "find", "linker memory"], vec!["--json", "cover", "README.md"], vec!["--json", "plans"], vec!["--version"]] {
        let mut child = lore(root).args(&args).stdin(Stdio::piped()).spawn().unwrap();
        let held = child.stdin.take(); // kept open, never written
        let output = finishes(child, &args.join(" "), Duration::from_secs(60));
        drop(held);
        assert!(output.status.code().is_some(), "lore {args:?} did not exit normally");
    }
}

#[test]
fn only_the_commands_that_take_input_read_stdin() {
    let wants = |line: &str| codelore::app::wants_stdin(&line.split_whitespace().map(str::to_owned).collect::<Vec<_>>());
    for line in ["add fault --batch", "update --json-in", "add any --stdin", "find -", "cover docs/a.md --add", "brief set --task t1", "message send --to lead", "message send --to lead -"] {
        assert!(wants(line), "{line} reads stdin");
    }
    for line in ["check", "find linker memory", "cover docs/a.md", "show x", "plans", "--version", "message send --to lead hello there", "-C repo --json show x"] {
        assert!(!wants(line), "{line} does not read stdin");
    }
}
