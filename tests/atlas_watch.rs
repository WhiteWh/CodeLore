//! Exercise the real CLI and the detached worker against isolated Git projects.
#![cfg(feature = "atlas")]
use fs2::FileExt;
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const CONFIG: &str = "version = 1\n[atlas]\nenabled = true\nsemantic = false\nexclude_file = \".atlasignore\"\n";

fn hidden_command(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
    index: PathBuf,
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        let root = codelore::config::simplify(&fs::canonicalize(root).unwrap());
        let cache = temp.path().join("cache");
        let index = cache.join(codelore::index::repo_key(&root));
        let project = Self { _temp: temp, root, cache, index };
        project.git(&["init", "-q", "-b", "main"]);
        project.git(&["config", "user.name", "Atlas CLI"]);
        project.git(&["config", "user.email", "atlas@example.invalid"]);
        project.git(&["config", "core.autocrlf", "false"]);
        project.write("lore.toml", CONFIG);
        project.write(".atlasignore", "");
        project.write("src/left.cpp", &format!("// atomic marker\n{}", cpp("AtomicMarkerOldLeft", 1)));
        project.write("src/right.cpp", &format!("// atomic marker\n{}", cpp("AtomicMarkerOldRight", 2)));
        project.git(&["add", "."]);
        project.git(&["commit", "-qm", "fixture"]);
        project
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn git(&self, args: &[&str]) {
        let output = hidden_command("git").current_dir(&self.root).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = hidden_command(env!("CARGO_BIN_EXE_lore"));
        command.current_dir(self._temp.path()).arg("-C").arg(&self.root).arg("atlas").args(args)
            .env("LORE_CACHE", &self.cache)
            .env("LORE_MODEL_FETCH", "off")
            .env_remove("LORE_ATLAS")
            .env_remove("LORE_ATLAS_SNAPSHOT");
        command
    }

    fn raw(&self, args: &[&str], unavailable_git: bool) -> Output {
        let mut command = self.command(args);
        if unavailable_git {
            command.env("GIT_DIR", self._temp.path().join("missing.git"));
        }
        command.output().unwrap()
    }

    fn atlas(&self, args: &[&str]) -> Value {
        success(self.raw(args, false))
    }

    fn status(&self) -> Value {
        self.atlas(&["watch", "status", "--json"])
    }

    fn wait(&self, what: &str, timeout: Duration, mut check: impl FnMut(&Value) -> bool) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.status();
            if check(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "{what}: {status}");
            std::thread::sleep(Duration::from_millis(75));
        }
    }

    fn wait_after(&self, previous: &Value) -> Value {
        self.wait("new completed revision", Duration::from_secs(25), |status| {
            status["worker"]["state"] == "idle"
                && status["revision"].as_u64() > previous["revision"].as_u64()
        })
    }

    fn has_symbol(&self, symbol: &str, path: &str) -> bool {
        let result = success(self.raw(&["symbol", symbol, "--json"], true));
        result["definitions"].as_array().unwrap().iter().any(|item| item["path"] == path)
    }

    fn writer_lock(&self) -> File {
        fs::create_dir_all(&self.index).unwrap();
        let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(self.index.join("atlas-writer.lock")).unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        lock
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = self.command(&["watch", "stop"]).output();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let Ok(output) = self.command(&["watch", "status", "--json"]).output() else { break };
            let status: Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
            if status["worker"]["state"] != "stopping" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn success(output: Output) -> Value {
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn cpp(name: &str, result: u32) -> String {
    format!("int {name}() {{\n    return {result};\n}}\n")
}

#[test]
fn atlas_snapshot_uses_existing_ranks_and_never_needs_git() {
    let project = Project::new();
    let missing = project.raw(&["search", "AtomicMarker", "--snapshot", "--json"], true);
    assert!(!missing.status.success());
    let missing: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing["snapshot"]["state"], "unindexed");
    assert!(missing["error"].as_str().unwrap().contains("lore atlas index"));
    assert!(!project.index.join("atlas.sqlite").exists());

    project.atlas(&["index", "--text-only", "--json"]);
    let args = ["search", "atomic marker", "--text-only", "--json"];
    let synchronous = project.atlas(&args);
    let baseline = project.raw(&args, true);
    assert!(!baseline.status.success());
    assert!(String::from_utf8_lossy(&baseline.stdout).contains("git diff-files"));

    let snapshot = success(project.raw(&["search", "atomic marker", "--snapshot", "--text-only", "--json"], true));
    assert_eq!(snapshot["results"], synchronous["results"]);
    assert!(!snapshot["results"].as_array().unwrap().is_empty());
    assert_eq!(snapshot["snapshot"]["mode"], "snapshot");
    assert!(snapshot["snapshot"]["refreshed_at"].is_string());
    let text = project.raw(&["deps", "src/left.cpp", "--snapshot"], true);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("project snapshot [ready, snapshot]"));
    let symbols = success(project.raw(&["symbols", "src/left.cpp", "--snapshot", "--json"], true));
    assert_eq!(symbols["results"][0]["name"], "AtomicMarkerOldLeft");
    let filtered = success(project.raw(&["symbol", "AtomicMarkerOldLeft", "--lang", "cpp", "--snapshot", "--json"], true));
    assert_eq!(filtered["definitions"][0]["path"], "src/left.cpp");
    assert_eq!(filtered["snapshot"]["revision"], snapshot["snapshot"]["revision"]);

    let fallback = project.root.join(".git/codelore");
    fs::create_dir(&fallback).unwrap();
    fs::copy(project.index.join("atlas.sqlite"), fallback.join("atlas.sqlite")).unwrap();
    let unusable_cache = project._temp.path().join("unusable-cache");
    fs::write(&unusable_cache, []).unwrap();
    let fallback_read = success(project.command(&args)
        .env("GIT_DIR", project._temp.path().join("missing.git"))
        .env("LORE_CACHE", unusable_cache)
        .env("LORE_ATLAS_SNAPSHOT", "on")
        .output().unwrap());
    assert_eq!(fallback_read["results"], synchronous["results"]);
    assert_eq!(fallback_read["snapshot"]["mode"], "snapshot");
}

fn terminate_worker(pid: u64) {
    let pid = pid.to_string();
    let output = if cfg!(windows) {
        hidden_command("taskkill").args(["/PID", &pid, "/F"]).output().unwrap()
    } else {
        Command::new("kill").args(["-KILL", &pid]).output().unwrap()
    };
    assert!(output.status.success(), "stop test worker {pid}: {output:?}");
}

#[test]
fn atlas_watch_skips_excluded_noise_and_notices_rule_changes() {
    let project = Project::new();
    let ignore = "Game/custom.rules";
    let path = "Game/Saved/rescued.cpp";
    let tracked = "Game/Saved/tracked.cpp";
    project.write("lore.toml", &CONFIG.replace(".atlasignore", ignore));
    project.write(ignore, "Saved/\n");
    project.write(path, &cpp("RescuedFromSaved", 1));
    project.write(tracked, &cpp("TrackedInSaved", 1));
    project.git(&["add", tracked]);
    let git_ignore = "Game/Saved/git-excludes";
    project.write(git_ignore, "");
    project.git(&["config", "core.excludesFile", git_ignore]);

    let start = project.atlas(&["watch", "start", "--json"]);
    project.wait_after(&start);
    std::thread::sleep(Duration::from_millis(650));
    let initial = project.status();
    let updates = &initial["worker"]["updates"];
    assert_eq!(initial["worker"]["state"], "idle");
    assert!(!project.has_symbol("RescuedFromSaved", path));
    for revision in 0..6 {
        let noise = "Game/Saved/noise.cpp";
        let renamed = "Game/Saved/moved.cpp";
        project.write(noise, &cpp("IgnoredEventNoise", revision));
        fs::rename(project.root.join(noise), project.root.join(renamed)).unwrap();
        fs::remove_file(project.root.join(renamed)).unwrap();
        std::thread::sleep(Duration::from_millis(340));
    }
    std::thread::sleep(Duration::from_millis(650));
    let quiet = project.status();
    assert_eq!(&quiet["worker"]["updates"], updates, "{initial} -> {quiet}");
    assert_eq!(quiet["checked_at"], initial["checked_at"]);

    project.write(tracked, &cpp("TrackedInSaved", 2));
    project.wait("tracked modification starts a refresh despite exclusion", Duration::from_secs(8), |status| {
        status["worker"]["state"] == "idle" && status["worker"]["updates"].as_u64() > updates.as_u64()
    });
    assert!(!project.has_symbol("TrackedInSaved", tracked));
    project.write(ignore, "Saved/*\n!Saved/rescued.cpp\n");
    let included = project.wait_after(&initial);
    assert!(project.has_symbol("RescuedFromSaved", path));

    project.write(path, &cpp("RescuedSave", 2));
    let edited = project.wait_after(&included);
    assert!(project.has_symbol("RescuedSave", path));

    project.write("Game/Saved/.gitignore", "rescued.cpp\n");
    let hidden = project.wait_after(&edited);
    assert!(!project.has_symbol("RescuedSave", path));
    fs::remove_file(project.root.join("Game/Saved/.gitignore")).unwrap();
    let unhidden = project.wait_after(&hidden);
    assert!(project.has_symbol("RescuedSave", path));
    fs::remove_file(project.root.join(path)).unwrap();
    let removed = project.wait_after(&unhidden);
    assert!(!project.has_symbol("RescuedSave", path));

    project.write("src/in-checkout.cpp", &cpp("InCheckoutGitRule", 1));
    let visible = project.wait_after(&removed);
    assert!(project.has_symbol("InCheckoutGitRule", "src/in-checkout.cpp"));
    project.write(git_ignore, "/src/in-checkout.cpp\n");
    let hidden = project.wait("rule file under an excluded path triggers a full refresh", Duration::from_secs(8), |status| {
        status["worker"]["state"] == "idle" && status["revision"].as_u64() > visible["revision"].as_u64()
    });
    assert!(!project.has_symbol("InCheckoutGitRule", "src/in-checkout.cpp"));

    project.write("Game/other.rules", "");
    project.write("lore.toml", &CONFIG.replace(".atlasignore", "Game/other.rules"));
    project.wait_after(&hidden);
    assert!(project.has_symbol("TrackedInSaved", tracked));
}

#[test]
fn atlas_watch_coalesces_and_recovers_without_blocking_snapshot_reads() {
    let project = Project::new();
    // Git reads the external ignore rules, outside the recursive watcher.
    let external_ignore = project._temp.path().join("external-ignore");
    fs::write(&external_ignore, "").unwrap();
    project.git(&["config", "core.excludesFile", external_ignore.to_str().unwrap()]);
    let writer = project.writer_lock();
    let status = project.atlas(&["watch", "start", "--json"]);
    assert_eq!(status["mode"], "background");
    assert_eq!(status["state"], "unindexed");
    let missing = project.raw(&["symbol", "AtomicMarkerOldLeft", "--json"], true);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stdout).contains("no completed project snapshot"));
    drop(writer);
    let initial = project.wait_after(&status);
    let pid = initial["worker"]["pid"].as_u64().unwrap();
    assert_eq!(project.atlas(&["watch", "start", "--json"])["worker"]["pid"], pid);

    let writer = project.writer_lock();
    for revision in 0..12 {
        project.write("src/pulse.cpp", &cpp(&format!("PulseRevision{revision}"), revision));
    }
    project.wait("worker will retry a busy writer", Duration::from_secs(5), |state| {
        state["worker"]["state"] == "waiting_for_writer"
    });
    assert!(project.has_symbol("AtomicMarkerOldLeft", "src/left.cpp"));
    assert!(!project.has_symbol("PulseRevision11", "src/pulse.cpp"));
    let old_counts = success(project.raw(&["status", "--json"], true))["counts"].clone();
    drop(writer);
    let added = project.wait_after(&initial);
    assert_eq!(added["revision"].as_u64().unwrap(), initial["revision"].as_u64().unwrap() + 1);
    assert!(added["worker"]["updates"].as_u64().unwrap() - initial["worker"]["updates"].as_u64().unwrap() < 12);
    assert!(project.has_symbol("PulseRevision11", "src/pulse.cpp"));
    let new_counts = success(project.raw(&["status", "--json"], true))["counts"].clone();
    for field in ["files", "chunks", "symbols"] {
        assert_eq!(new_counts[field].as_i64().unwrap(), old_counts[field].as_i64().unwrap() + 1);
    }
    for revision in 12..24 {
        project.write("src/pulse.cpp", &cpp(&format!("PulseRevision{revision}"), revision));
        std::thread::sleep(Duration::from_millis(8));
    }
    let saved = project.wait("save burst on an idle worker", Duration::from_secs(8), |status| {
        status["worker"]["state"] == "idle" && project.has_symbol("PulseRevision23", "src/pulse.cpp")
    });
    assert!(saved["worker"]["updates"].as_u64().unwrap() - added["worker"]["updates"].as_u64().unwrap() < 6);
    fs::rename(project.root.join("src/pulse.cpp"), project.root.join("src/renamed.cpp")).unwrap();
    let renamed = project.wait_after(&saved);
    assert!(project.has_symbol("PulseRevision23", "src/renamed.cpp"));
    assert!(!project.has_symbol("PulseRevision23", "src/pulse.cpp"));
    fs::remove_file(project.root.join("src/renamed.cpp")).unwrap();
    let removed = project.wait_after(&renamed);
    assert!(!project.has_symbol("PulseRevision23", "src/renamed.cpp"));

    project.git(&["switch", "-qc", "changed"]);
    project.write("src/branch.cpp", &cpp("BranchOnly", 1));
    project.git(&["add", "src/branch.cpp"]);
    project.git(&["commit", "-qm", "branch"]);
    let branch = project.wait_after(&removed);
    project.git(&["switch", "-q", "main"]);
    let main = project.wait_after(&branch);
    assert!(!project.has_symbol("BranchOnly", "src/branch.cpp"));
    project.git(&["switch", "-q", "changed"]);
    let branch = project.wait_after(&main);
    assert!(project.has_symbol("BranchOnly", "src/branch.cpp"));

    project.write(".atlasignore", "src/branch.cpp\n");
    let excluded = project.wait_after(&branch);
    assert!(!project.has_symbol("BranchOnly", "src/branch.cpp"));
    fs::remove_file(project.root.join(".atlasignore")).unwrap();
    let error = project.wait("refresh error from missing rule file", Duration::from_secs(8), |status| status["worker"]["state"] == "error");
    assert_eq!(error["revision"], excluded["revision"]);
    assert!(error["worker"]["error"].as_str().unwrap().contains(".atlasignore"));
    assert!(project.has_symbol("AtomicMarkerOldRight", "src/right.cpp"));
    project.write(".atlasignore", "");
    let recovered = project.wait_after(&excluded);
    assert!(project.has_symbol("BranchOnly", "src/branch.cpp"));

    // Exercise loss of the filesystem watch itself and reattachment.
    let moved = project._temp.path().join("moved-checkout");
    fs::rename(&project.root, &moved).unwrap();
    let lost_watch = project.wait("watcher lost its directory", Duration::from_secs(5), |status| {
        status["worker"]["state"] == "error" && status["worker"]["watcher_error"].is_string()
    });
    assert_eq!(lost_watch["revision"], recovered["revision"]);
    fs::rename(&moved, &project.root).unwrap();
    let attached = project.wait("watcher reattached", Duration::from_secs(10), |status| {
        status["worker"]["state"] == "idle" && status["worker"]["watcher_error"].is_null()
    });
    assert_eq!(attached["worker"]["pid"], pid);

    project.write("src/external.cpp", &cpp("ExternalRule", 3));
    let visible = project.wait_after(&attached);
    assert!(project.has_symbol("ExternalRule", "src/external.cpp"));
    // Let any trailing notifications from the creation drain before writing
    // outside the watched tree. The fast path cannot pick up this change.
    std::thread::sleep(Duration::from_secs(2));
    let quiet = project.status();
    assert_eq!(quiet["worker"]["state"], "idle");
    fs::write(external_ignore, "external.cpp\n").unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(project.status()["worker"]["updates"], quiet["worker"]["updates"]);
    assert!(project.has_symbol("ExternalRule", "src/external.cpp"));
    let missed = project.wait("periodic reconciliation covers unwatched changes", Duration::from_secs(42), |status| {
        status["worker"]["state"] == "idle" && status["revision"].as_u64() > visible["revision"].as_u64()
    });
    assert!(!project.has_symbol("ExternalRule", "src/external.cpp"));

    terminate_worker(pid);
    let dead = project.wait("dead worker remains opted in", Duration::from_secs(5), |status| status["worker"]["state"] == "unavailable");
    assert_eq!(dead["mode"], "background");
    assert!(project.has_symbol("AtomicMarkerOldRight", "src/right.cpp"));
    project.write("src/offline.cpp", &cpp("OfflineChange", 7));
    assert!(!project.has_symbol("OfflineChange", "src/offline.cpp"));
    project.atlas(&["watch", "start", "--json"]);
    let restarted = project.wait_after(&missed);
    assert_ne!(restarted["worker"]["pid"], pid);
    assert!(project.has_symbol("OfflineChange", "src/offline.cpp"));

    let body: String = (0..150).map(|n| cpp(&format!("BulkName{n}"), n)).collect();
    for n in 0..94 {
        project.write(&format!("bulk/probe{n}.cpp"), &body);
    }
    project.write("src/left.cpp", &format!("// atomic marker\n{}", cpp("AtomicMarkerNewLeft", 11)));
    project.write("src/right.cpp", &format!("// atomic marker\n{}", cpp("AtomicMarkerNewRight", 12)));
    project.wait("bulk update in progress", Duration::from_secs(10), |status| status["worker"]["state"] == "updating");
    project.write("src/while-updating.cpp", &cpp("SavedDuringUpdate", 13));
    let query = |expected: Option<&str>| {
        let out = success(project.raw(&["search", "atomic marker", "--text-only", "--json"], true));
        let results = out["results"].as_array().unwrap();
        let left = results.iter().find(|item| item["path"] == "src/left.cpp").unwrap()["text"].as_str().unwrap();
        let right = results.iter().find(|item| item["path"] == "src/right.cpp").unwrap()["text"].as_str().unwrap();
        assert_eq!(left.contains("MarkerOld"), right.contains("MarkerOld"), "{out}");
        if let Some(expected) = expected {
            assert!(left.contains(expected) && right.contains(expected), "{out}");
        }
    };
    query(None);
    project.wait("save during refresh is observed", Duration::from_secs(25), |status| {
        status["worker"]["state"] == "idle" && project.has_symbol("SavedDuringUpdate", "src/while-updating.cpp")
    });
    query(Some("MarkerNew"));
    assert!(project.atlas(&["index", "--rebuild", "--text-only", "--json"])["files_read"].as_u64().unwrap() >= 94);

    project.atlas(&["watch", "stop", "--json"]);
    project.wait("worker stopped", Duration::from_secs(5), |status| status["worker"]["state"] == "disabled");
    project.write("src/sync.cpp", &cpp("SynchronousAgain", 14));
    let synchronous = project.atlas(&["symbol", "SynchronousAgain", "--json"]);
    assert_eq!(synchronous["definitions"][0]["path"], "src/sync.cpp");
    assert!(synchronous.get("snapshot").is_none());
}
