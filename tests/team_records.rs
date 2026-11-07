//! Task 128: the records Zerus writes for a team (docs/lore.md 6.1), in the
//! exact shapes `src/state/teams/lore_sync.rs` builds, pass lore's own
//! validation through the commands Zerus runs: one `add any --new` batch, one
//! `update --batch`, `item reopen` and `plan close`, written as `agent:zerus`
//! with the `Zerus-Team` trailer.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use codelore::cli::{self, Env};

const TEAM: &str = "33fbafeb-8291-4eda-a00c-6e1448ec6f20";
const CONFIG: &str = "version = 1\ndir = \"lore\"\nareas = [\"team-33fbafeb\"]\n\n[commit]\nmode = \"plain\"\nbranch = \"main\"\n\n[embed]\nprovider = \"none\"\n";

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct Workspace {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let tmp = tempfile::Builder::new().prefix("lore-team-").tempdir().unwrap();
        let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap()).join("workspace");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("lore").join("plans")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Zerus"]);
        git(&root, &["config", "user.email", "lore@zerus.invalid"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("src").join("inbox.rs"), "fn digest() {}\n").unwrap();
        std::fs::write(root.join("lore.toml"), CONFIG).unwrap();
        std::fs::write(root.join(".gitattributes"), "lore/** text eol=lf\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "init"]);
        Self { _tmp: tmp, root }
    }

    /// `lore -C <workspace> --json --by agent:zerus --trailer "Zerus-Team: ID" ...`
    fn zerus(&self, argv: &[&str], stdin: &str) -> cli::Output {
        let root = self.root.to_string_lossy().into_owned();
        let trailer = format!("Zerus-Team: {TEAM}");
        let mut full = vec!["-C", root.as_str(), "--json", "--by", "agent:zerus", "--trailer", trailer.as_str()];
        full.extend(argv);
        let env = Env { cwd: self.root.clone(), stdin: stdin.as_bytes().to_vec(), finder: Some(&codelore::builtin::BUILTIN), ..Env::default() };
        cli::run(&full, &env)
    }

    fn ok(&self, argv: &[&str], stdin: &str) -> Value {
        let out = self.zerus(argv, stdin);
        assert_eq!(out.code, 0, "lore {argv:?}:\n{}", out.text());
        serde_json::from_str(&out.stdout).unwrap_or(Value::Null)
    }

    fn record(&self, id: &str) -> Value {
        let out = self.zerus(&["show", id], "");
        assert_eq!(out.code, 0, "lore show {id}:\n{}", out.text());
        let value: Value = serde_json::from_str(&out.stdout).unwrap();
        value.get("record").cloned().unwrap_or(value)
    }
}

/// Zerus's first pass for a running team: the plan before its items, the
/// owner's decision, the faults and a reading, in one add.
fn first_pass(head: &str) -> Value {
    let plan = "t33fbafeb-phase-1";
    json!([
        {"kind":"plan","id":plan,"title":"Core: Memory","goal":"Teams keep their memory","status":"live"},
        {"kind":"item","id":format!("{plan}#1"),"plan":plan,"title":"Write the records","description":"Plans, outcomes, decisions",
            "criteria":["records pass validation"],"commits":[head],"outcome":"complete and tested","status":"done"},
        {"kind":"item","id":format!("{plan}#2"),"plan":plan,"title":"Old idea","reason":"out of scope","decided_by":"You","status":"dropped"},
        {"kind":"item","id":format!("{plan}#3"),"plan":plan,"title":"Relay","reason":"waits for the relay","status":"deferred"},
        {"kind":"item","id":format!("{plan}#4"),"plan":plan,"title":"Old title","deps":[{"item":format!("{plan}#1"),"kind":"soft"}],"status":"open"},
        {"kind":"decision","id":"t33fbafeb-p1","area":"team-33fbafeb","title":"Allowed add-member: add a QA member",
            "question":"add-member: add a QA member","choice":"allowed: add a QA member","reason":"the tests lag","decided_by":"owner","status":"active"},
        {"kind":"fault","id":"t33fbafeb-finding-f1","area":"team-33fbafeb","title":"The digest lost the decisions",
            "symptom":"a lead reads the inbox after an allowed card and sees no decision","cause":"The digest lost the decisions",
            "fix":"Kept the decisions in the digest\nand tested it","check":"cargo-test {\"filters\":[\"lore_sync\"]}","status":"cured",
            "refs":{"files":["src/inbox.rs"]}},
        {"kind":"fault","id":"t33fbafeb-rework-1-1","area":"team-33fbafeb","title":"Rework of #1: the digest drops the owner's decisions",
            "symptom":"#1 Write the records was sent back for rework (attempt 1)","cause":"the digest drops the owner's decisions",
            "fix":"Kept the decisions in the digest","check":"cargo-test {\"filters\":[\"lore_sync\"]}","status":"cured"},
        {"kind":"reading","id":"t33fbafeb-reading-cargo-test","area":"team-33fbafeb","title":"cargo-test duration, the latest passing run",
            "what":"cargo-test wall time on task #1","command":"cargo-test {\"filters\":[\"lore_sync\"]}","value":"4200 ms","measured_at":head,"status":"current"}
    ])
}

#[test]
fn zerus_team_records_pass_lores_validation_through_its_commands() {
    let workspace = Workspace::new();
    let head = git(&workspace.root, &["rev-parse", "HEAD"]);
    workspace.ok(&["add", "any", "--stdin", "--new"], &first_pass(&head).to_string());
    let message = git(&workspace.root, &["log", "-1", "--format=%B"]);
    assert!(message.contains(&format!("Zerus-Team: {TEAM}")), "the commit carries the trailer:\n{message}");
    assert_eq!(git(&workspace.root, &["rev-list", "--count", "HEAD"]), "2", "one batch, one commit");
    let done = workspace.record("t33fbafeb-phase-1#1");
    assert_eq!((done["status"].as_str(), done["by"].as_str()), (Some("done"), Some("agent:zerus")), "{done}");

    // The changed open item: one JSONL patch, a removed field as null.
    let patches = [json!({"id":"t33fbafeb-phase-1#4","title":"Export","reason":null}), json!({"id":"t33fbafeb-phase-1#3","status":"open","reason":null})];
    let lines: Vec<String> = patches.iter().map(Value::to_string).collect();
    workspace.ok(&["update", "--batch"], &lines.join("\n"));
    assert_eq!(workspace.record("t33fbafeb-phase-1#4")["title"], "Export");
    assert_eq!(workspace.record("t33fbafeb-phase-1#3")["status"], "open", "a released hold is a plain update");

    // A decided item moves back only through reopen, with the reason.
    let refused = workspace.zerus(&["update", "--batch"], &json!({"id":"t33fbafeb-phase-1#1","status":"open"}).to_string());
    assert_ne!(refused.code, 0, "a done item does not become open by a patch");
    workspace.ok(&["item", "reopen", "t33fbafeb-phase-1#1", "--reason", "the task was reopened in its team"], "");
    let reopened = workspace.record("t33fbafeb-phase-1#1");
    assert_eq!(reopened["status"], "open");
    assert!(reopened.get("commits").is_none() && reopened.get("outcome").is_none(), "{reopened}");

    // The tasks are decided again: plain patches move open items to done or
    // dropped, and only then the finished team closes its plan.
    let decided = [
        json!({"id":"t33fbafeb-phase-1#1","status":"done","commits":[head],"outcome":"accepted again"}),
        json!({"id":"t33fbafeb-phase-1#3","status":"dropped","reason":"the relay moved","decided_by":"Lead"}),
        json!({"id":"t33fbafeb-phase-1#4","status":"dropped","reason":"cancelled","decided_by":"You"}),
    ];
    let early = workspace.zerus(&["plan", "close", "t33fbafeb-phase-1", "--outcome", "too early"], "");
    assert_ne!(early.code, 0, "a plan with undecided items stays open:\n{}", early.text());
    let lines: Vec<String> = decided.iter().map(Value::to_string).collect();
    workspace.ok(&["update", "--batch"], &lines.join("\n"));
    assert_eq!(workspace.record("t33fbafeb-phase-1#1")["status"], "done");
    // The finished team closes its plan with the outcome.
    workspace.ok(&["plan", "close", "t33fbafeb-phase-1", "--outcome", "1 of 2 tasks done; the team finished"], "");
    let plan = workspace.record("t33fbafeb-phase-1");
    assert_eq!((plan["status"].as_str(), plan["outcome"].as_str()), (Some("closed"), Some("1 of 2 tasks done; the team finished")));

    // A repeated pass is refused as a duplicate, never written twice.
    let again = workspace.zerus(&["add", "any", "--stdin", "--new"], &json!([first_pass(&head)[5]]).to_string());
    assert_ne!(again.code, 0, "the same id is not added twice:\n{}", again.text());
}
