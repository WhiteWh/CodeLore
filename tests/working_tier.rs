//! The working tier through the command line (docs/lore.md 11.15): working
//! entries expire and never reach JSONL or git, promote validates like add,
//! `find` ranks the working tier and history below the durable records and
//! marks them, duplicates become occurrences, and briefs survive as files.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use codelore::cli::{self, Env};
use codelore::history::{HistoryHit, HistorySource, Scope};
use codelore::LoreError;

const T0: i64 = 1_800_000_000;
const CONFIG: &str = "version = 1\ndir = \"lore\"\nareas = [\"render\"]\n\n[commit]\nmode = \"plain\"\nbranch = \"main\"\n\n[embed]\nprovider = \"none\"\n";

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct Repo {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    history: Option<Arc<dyn HistorySource>>,
}

impl Repo {
    fn new() -> Self {
        let tmp = tempfile::Builder::new().prefix("lore-working-").tempdir().unwrap();
        let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap()).join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "tester"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("src").join("post.py"), "x = 1\n").unwrap();
        std::fs::write(root.join("lore.toml"), CONFIG).unwrap();
        std::fs::write(root.join(".gitattributes"), "lore/** text eol=lf\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "init"]);
        Self { _tmp: tmp, root, history: None }
    }

    fn run_at(&self, now: i64, argv: &[&str], stdin: &str) -> cli::Output {
        let root = self.root.to_string_lossy().into_owned();
        let mut full = vec!["-C", root.as_str(), "--by", "dev"];
        full.extend(argv);
        let env = Env {
            cwd: self.root.clone(),
            stdin: stdin.as_bytes().to_vec(),
            finder: Some(&codelore::builtin::BUILTIN),
            now: Some(now),
            history: self.history.clone(),
            history_scope: Scope::default(),
            ..Env::default()
        };
        cli::run(&full, &env)
    }

    fn json(&self, argv: &[&str], stdin: &str) -> Value {
        let mut full = vec!["--json"];
        full.extend(argv);
        let out = self.run_at(T0, &full, stdin);
        assert_eq!(out.code, 0, "lore {argv:?}:\n{}", out.text());
        serde_json::from_str(&out.stdout).unwrap_or_else(|error| panic!("{error}: {}", out.stdout))
    }

    fn refuse(&self, argv: &[&str], stdin: &str) -> String {
        let out = self.run_at(T0, argv, stdin);
        assert_ne!(out.code, 0, "lore {argv:?} should refuse:\n{}", out.text());
        out.text()
    }

    fn head(&self) -> String {
        git(&self.root, &["rev-parse", "HEAD"])
    }
}

fn fault(id: &str, title: &str, cause: &str, fix: &str) -> Value {
    json!({"kind": "fault", "id": id, "area": "render", "title": title, "symptom": "the frame is black after resize",
        "cause": cause, "fix": fix, "check": "resize the window and look at the frame", "refs": {"files": ["src/post.py"]}})
}

/// The acceptance: working entries never reach JSONL or git; a brief is the
/// one working text committed, as its own file.
#[test]
fn working_entries_never_reach_jsonl_or_git() {
    let repo = Repo::new();
    repo.json(&["pulse", "add", "--task", "#7", "--done", "PULSE-MARKER resized the swapchain", "--team", "t"], "");
    repo.json(&["message", "send", "--to", "lead", "--team", "t", "MESSAGE-MARKER the frame is black"], "");
    let brief = repo.json(&["brief", "set", "--task", "#7", "--title", "Black frame", "--team", "t"], "Goal: fix the black frame. BRIEF-MARKER");
    assert_eq!(brief["kind"], "brief");
    let files = git(&repo.root, &["ls-files"]);
    assert!(!files.contains(".lore-private"), "{files}");
    assert!(files.contains("briefs/t7.md"), "the brief is a committed file: {files}");
    assert!(git(&repo.root, &["status", "--porcelain"]).is_empty(), "the private folder is ignored");
    let history = git(&repo.root, &["log", "-p", "--all"]);
    assert!(!history.contains("PULSE-MARKER") && !history.contains("MESSAGE-MARKER"), "working text is never in git");
    assert!(history.contains("BRIEF-MARKER") && history.contains("Zerus-Brief: t/t7/v1"));
    for path in std::fs::read_dir(repo.root.join("lore")).into_iter().flatten().flatten() {
        let text = std::fs::read_to_string(path.path()).unwrap_or_default();
        assert!(!text.contains("PULSE-MARKER") && !text.contains("MESSAGE-MARKER"), "never in JSONL");
    }
    // The brief is read back from its file, also after the working file goes.
    std::fs::remove_file(repo.root.join(".lore-private").join("working.sqlite")).unwrap();
    let shown = repo.json(&["brief", "show", "--task", "#7", "--team", "t"], "");
    assert_eq!(shown["source"], "file");
    assert!(shown["text"].as_str().unwrap().contains("BRIEF-MARKER"));
    // A brief with a secret is refused and the file keeps the previous version.
    let refused = repo.refuse(&["brief", "set", "--task", "#7", "--team", "t"], concat!("use ghp_", "0123456789abcdefghijABCDEFGHIJ012345"));
    assert!(refused.contains("GitHub token") && !refused.contains(concat!("ghp_", "0123456789abcdefghijABCDEFGHIJ012345")), "{refused}");
    assert!(std::fs::read_to_string(repo.root.join("briefs/t7.md")).unwrap().contains("BRIEF-MARKER"));
}

#[test]
fn working_entries_expire_on_reads() {
    let repo = Repo::new();
    repo.json(&["pulse", "add", "--done", "looked at the shader", "--team", "t"], "");
    let listed = |now: i64| -> usize {
        let out = repo.run_at(now, &["--json", "pulses", "--all", "--team", "t"], "");
        serde_json::from_str::<Value>(&out.stdout).unwrap()["entries"].as_array().unwrap().len()
    };
    assert_eq!(listed(T0 + 14 * 86_400 - 1), 1);
    assert_eq!(listed(T0 + 14 * 86_400), 0, "past its expiry, before any cleanup");
}

/// The acceptance: promote validates like add, scans for secrets, is
/// idempotent by entry id, and a dry run writes nothing.
#[test]
fn promote_validates_like_add() {
    let repo = Repo::new();
    let pulse = repo.json(&["pulse", "add", "--task", "#3", "--blocked", "the frame is black after a resize", "--team", "t"], "");
    let id = pulse["id"].as_str().unwrap().to_owned();
    let before = repo.head();
    // A fault needs its cause, fix and check, as `lore add` says.
    let refused = repo.refuse(&["promote", &id, "--kind", "fault"], "");
    assert!(refused.contains("`cause` is required") && refused.contains("`fix` is required"), "{refused}");
    // The secret scan refuses a field and names it, masked.
    let refused = repo.refuse(&["promote", &id, "--kind", "fault", "--field", "cause=the swapchain", "--field", concat!("fix=key AKIA", "ABCDEFGHIJKLWXYZ"), "--field", "check=resize it"], "");
    assert!(refused.contains("`fix` holds what looks like an AWS access key id (`AKIA…WXYZ`, 20 characters)"), "{refused}");
    assert!(!refused.contains(concat!("AKIA", "ABCDEFGHIJKLWXYZ")));
    // A dry run prints the draft and writes nothing.
    let dry = repo.json(&["promote", &id, "--kind", "fault", "--dry-run", "--field", "cause=x"], "");
    assert_eq!(dry["written"], false);
    assert_eq!(dry["draft"]["symptom"], "the frame is black after a resize");
    assert_eq!(repo.head(), before, "nothing was written");
    // Now it passes: one commit, its trailer, the entry remembers the record.
    let fields = ["--field", "cause=the swapchain is not recreated", "--field", "fix=recreate the swapchain on resize", "--field", "check=resize and look at the frame", "--field", "refs:={\"files\":[\"src/post.py\"]}"];
    let mut argv = vec!["promote", id.as_str(), "--kind", "fault"];
    argv.extend(fields);
    let promoted = repo.json(&argv, "");
    let record = promoted["record"].as_str().unwrap().to_owned();
    assert_ne!(repo.head(), before);
    assert!(git(&repo.root, &["log", "-1", "--format=%B"]).contains(&format!("Zerus-Promoted: {id}")));
    assert!(std::fs::read_to_string(repo.root.join("lore/render.jsonl")).unwrap().contains(&record));
    // Promoting again answers the same record and writes nothing.
    let head = repo.head();
    let again = repo.json(&argv, "");
    assert_eq!((again["record"].as_str(), again["existing"].as_bool()), (Some(record.as_str()), Some(true)));
    assert_eq!(repo.head(), head);
    // A member promotes only its own entries.
    let refused = repo.refuse(&["promote", &id, "--kind", "fault", "--role", "member", "--by", "qa"], "");
    assert!(refused.contains("a member promotes its own entries"), "{refused}");
}

/// The acceptance (11.9): every durable write passes the secret scan.
#[test]
fn a_durable_write_with_a_secret_is_refused() {
    let repo = Repo::new();
    let mut record = fault("leaked", "A leaked key", "the config held a key", "rotate it");
    record["fix"] = json!(concat!("rotate sk-ant-", "abcdefghijklmnopqrstuvwxyz0123"));
    let refused = repo.refuse(&["add", "fault", "--stdin"], &record.to_string());
    assert!(refused.contains("holds what looks like an API key") && !refused.contains(concat!("sk-ant-", "abcdefghijklmnopqrstuvwxyz0123")), "{refused}");
}

/// 11.10: the same fault twice is one record with an occurrence; a
/// differing one is written with its conflict named.
#[test]
fn a_repeated_fault_becomes_an_occurrence() {
    let repo = Repo::new();
    repo.json(&["add", "fault", "--stdin"], &fault("black-frame", "Black frame after resize", "the swapchain is not recreated", "recreate the swapchain on resize").to_string());
    let same = repo.json(&["add", "fault", "--stdin", "--occurrence"], &fault("", "Black frame after resize", "the swapchain is not recreated on resize", "recreate the swapchain on resize").to_string());
    assert_eq!(same["occurrence_of"], "black-frame");
    let jsonl = std::fs::read_to_string(repo.root.join("lore/render.jsonl")).unwrap();
    assert_eq!(jsonl.lines().count(), 1, "one record");
    let record: Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    assert_eq!(record["seen"].as_array().unwrap().len(), 1);
    let differ = repo.json(&["add", "fault", "--stdin", "--occurrence"], &fault("", "Black frame after resize", "the swapchain is not recreated after a resize event", "restart the renderer process completely").to_string());
    assert_eq!(differ["conflicts_with"], json!(["black-frame"]));
    let jsonl = std::fs::read_to_string(repo.root.join("lore/render.jsonl")).unwrap();
    assert_eq!(jsonl.lines().count(), 2, "written as its own record");
    // Supersede only on request.
    let new = differ["record"].as_str().unwrap();
    repo.json(&["supersede", "black-frame", new], "");
    let jsonl = std::fs::read_to_string(repo.root.join("lore/render.jsonl")).unwrap();
    assert!(jsonl.contains("\"superseded_by\":\"") || jsonl.contains("\"superseded_by\": \""), "{jsonl}");
}

struct Fake;
impl HistorySource for Fake {
    fn search(&self, _query: &str, _scope: &Scope, _limit: usize) -> Result<Vec<HistoryHit>, LoreError> {
        Ok((0..5)
            .map(|n| HistoryHit { session: format!("claude/x/s{n}"), message_id: format!("m{n}"), role: "assistant".into(), at: 1.0,
                excerpt: format!("the swapchain resize {n}"), score: 1.0 - n as f64 / 10.0, ..HistoryHit::default() })
            .collect())
    }
}

/// 11.6, 11.7: the working tier and history rank below the durable records,
/// are marked, keep their caps, and leave `find` alone when there are none.
#[test]
fn find_ranks_and_marks_the_tiers() {
    let mut repo = Repo::new();
    repo.json(&["add", "fault", "--stdin"], &fault("black-frame", "Black frame after swapchain resize", "the swapchain is not recreated", "recreate the swapchain on resize").to_string());
    // No working entry, no history source: CodeLore's own answer.
    let plain = repo.json(&["find", "swapchain resize"], "");
    assert!(plain["results"].as_array().unwrap().iter().all(|hit| hit["tier"] == "durable" && hit["record"].is_object()));
    // Task 133: every result says its tier and how far to trust it.
    let confidence = &plain["results"][0]["confidence"];
    assert_eq!((confidence["tier"].as_str(), confidence["exact"].as_bool(), confidence["superseded"].as_bool()), (Some("durable"), Some(true), Some(false)), "{confidence}");
    assert_eq!(confidence["level"], "high", "a phrase match of a current record");
    assert!(confidence["age_days"].as_i64().is_some() && confidence.get("semantic").is_some());
    for n in 0..4 {
        repo.json(&["pulse", "add", "--done", &format!("swapchain resize investigation {n}"), "--team", "t", "--by", &format!("m{n}")], "");
    }
    repo.history = Some(Arc::new(Fake));
    let found = repo.json(&["find", "swapchain resize", "-n", "6"], "");
    let results = found["results"].as_array().unwrap();
    assert_eq!(results[0]["record"]["id"], "black-frame", "durable first");
    let tiers: Vec<&str> = results.iter().map(|hit| hit["tier"].as_str().unwrap_or("durable")).collect();
    assert_eq!(tiers.iter().filter(|tier| **tier == "working").count(), 3, "at most ceil(n/2) working: {tiers:?}");
    assert!(tiers.iter().filter(|tier| **tier == "history").count() <= 3);
    let working = results.iter().find(|hit| hit["tier"] == "working").unwrap();
    assert!(working["via"].as_array().unwrap().contains(&json!("working")));
    assert!(working.get("record").is_none() && working["entry"]["kind"] == "pulse");
    let scores: Vec<f64> = results.iter().map(|hit| hit["score"].as_f64().unwrap()).collect();
    assert!(scores.windows(2).all(|pair| pair[0] >= pair[1]), "{scores:?}");
    // The flags.
    let only = repo.json(&["find", "swapchain resize", "--working"], "");
    assert!(only["results"].as_array().unwrap().iter().all(|hit| hit["tier"] == "working"));
    let none = repo.json(&["find", "swapchain resize", "--no-working", "--no-history"], "");
    assert!(none["results"].as_array().unwrap().iter().all(|hit| hit["tier"] == "durable"));
    let levels: Vec<(&str, &str)> = results.iter().map(|hit| (hit["confidence"]["tier"].as_str().unwrap(), hit["confidence"]["level"].as_str().unwrap())).collect();
    assert!(levels.contains(&("working", "medium")) && levels.contains(&("history", "low")), "{levels:?}");
    let history = repo.json(&["find", "swapchain resize", "--history"], "");
    assert_eq!(history["results"].as_array().unwrap().len(), 3, "the 3 best history hits");
    assert!(history["results"][0]["hit"]["excerpt"].as_str().unwrap().contains("swapchain"));
    // Text output marks the tier.
    let text = repo.run_at(T0, &["find", "swapchain resize", "-n", "6"], "").stdout;
    assert!(text.contains("[working pulse]") && text.contains("[history]"), "{text}");
    assert!(text.contains("(high confidence, durable, exact match") && text.contains("(low confidence, history"), "{text}");
}

#[test]
fn messages_mark_read_unless_peeked() {
    let repo = Repo::new();
    repo.json(&["message", "send", "--to", "qa", "--team", "t", "--urgent", "stop the matrix run"], "");
    let peek = repo.json(&["messages", "--unread", "--peek", "--team", "t", "--role", "member", "--by", "qa"], "");
    assert_eq!(peek["entries"].as_array().unwrap().len(), 1);
    assert_eq!(peek["entries"][0]["urgency"], "urgent");
    let read = repo.json(&["messages", "--unread", "--team", "t", "--role", "member", "--by", "qa"], "");
    assert_eq!(read["entries"].as_array().unwrap().len(), 1);
    let after = repo.json(&["messages", "--unread", "--team", "t", "--role", "member", "--by", "qa"], "");
    assert!(after["entries"].as_array().unwrap().is_empty(), "read by the listing");
    let status = repo.json(&["working", "status"], "");
    assert_eq!(status["counts"]["message"], 1);
    assert_eq!(status["history"], false);
}

/// Task 187 (HIGH 6, 11.12): a brief the working tier refuses is never
/// committed: an empty brief leaves no file and no commit.
#[test]
fn a_refused_brief_commits_nothing() {
    let repo = Repo::new();
    let before = repo.head();
    let refused = repo.refuse(&["brief", "set", "--task", "#8", "--team", "t"], "   \n");
    assert!(refused.contains("the brief is empty"), "{refused}");
    assert_eq!(repo.head(), before, "nothing was committed");
    assert!(!repo.root.join("briefs/t8.md").exists());
    // A valid one is committed and stored.
    repo.json(&["brief", "set", "--task", "#8", "--team", "t"], "Goal: the frame");
    assert_ne!(repo.head(), before);
    assert!(repo.root.join("briefs/t8.md").exists());
}

/// Task 187 (11.4: a read never waits for the cleanup): reads never run it;
/// an expired row is hidden by every read and deleted by the next write's pass.
#[test]
fn reads_never_run_the_cleanup_and_writes_do() {
    let repo = Repo::new();
    repo.json(&["pulse", "add", "--done", "looked at the shader", "--team", "t"], "");
    let later = T0 + 20 * 86_400;
    // (the oldest row's time and the last pass's: the counts hide expired rows)
    let rows = |now: i64| -> (i64, i64) {
        let out = repo.run_at(now, &["--json", "working", "status"], "");
        let value = serde_json::from_str::<Value>(&out.stdout).unwrap();
        (value["oldest"].as_i64().unwrap(), value["last_cleanup"].as_i64().unwrap())
    };
    let listed = repo.run_at(later, &["--json", "pulses", "--all", "--team", "t"], "");
    assert_eq!(serde_json::from_str::<Value>(&listed.stdout).unwrap()["entries"].as_array().unwrap().len(), 0);
    repo.run_at(later, &["--json", "messages", "--team", "t"], "");
    assert_eq!(rows(later), (T0, T0), "a read left the expired row and ran no pass");
    let out = repo.run_at(later, &["--json", "pulse", "add", "--done", "next step", "--team", "t"], "");
    assert_eq!(out.code, 0, "{}", out.text());
    assert_eq!(rows(later), (later, later), "the write's pass deleted the expired row");
}
