//! Explicit lifecycle for the project's background refresher. The two locks
//! have different lifetimes: the worker lease lasts until exit, and the
//! project writer lock is held only during a refresh.
use super::store::{self, Atlas, RefreshKind};
use crate::config::{self, Config};
use crate::LoreError;
use fs2::FileExt;
use notify::event::{AccessKind, AccessMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const ENABLED: &str = "atlas-watch.enabled";
const CONTROL: &str = "atlas-watch-control.lock";
const LEASE: &str = "atlas-watch-worker.lock";
const STATE: &str = "atlas-watch.json";
const MCP_MARKER: &str = "mcp";
const START_GRACE: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(200);
const QUIET: Duration = Duration::from_millis(250);
const MAX_DEBOUNCE: Duration = Duration::from_secs(1);
const RECONCILE: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_secs(2);
const WATCH_RETRY: Duration = Duration::from_secs(5);

fn error(e: impl std::fmt::Display) -> LoreError {
    LoreError::new(format!("atlas watch: {e}"))
}

pub(super) fn enabled_dir(root: &Path) -> Option<PathBuf> {
    store::project_dirs(root).find(|dir| enabled(dir))
}

fn worker_dir(root: &Path) -> Option<PathBuf> {
    enabled_dir(root).or_else(|| store::project_dirs(root).find(|dir| dir.join(LEASE).is_file()))
}

pub(super) fn control_root(start: &Path) -> Result<PathBuf, LoreError> {
    let start = config::simplify(&fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf()));
    start.ancestors().find(|dir| dir.join(config::CONFIG_NAME).is_file() || worker_dir(dir).is_some()).map(Path::to_path_buf)
        .map(Ok).unwrap_or_else(|| config::find_root(&start))
}

pub(super) fn enabled(dir: &Path) -> bool {
    dir.join(ENABLED).is_file()
}

fn running(dir: &Path) -> Result<bool, Error> {
    let file = match File::open(dir.join(LEASE)) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match FileExt::try_lock_shared(&file) {
        Ok(()) => Ok(false),
        Err(e) if store::is_locked(&e) => Ok(true),
        Err(e) => Err(e),
    }
}

fn status_file(dir: &Path) -> Value {
    fs::read(dir.join(STATE)).ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object).unwrap_or_else(|| json!({}))
}

fn starting(dir: &Path, status: &Value) -> bool {
    status["state"] == "starting" && dir.join(STATE).metadata().and_then(|meta| meta.modified())
        .ok().and_then(|time| time.elapsed().ok()).is_some_and(|age| age < START_GRACE)
}

fn worker_status(dir: Option<&Path>) -> Value {
    let Some(dir) = dir else { return json!({"state": "disabled"}) };
    let active = enabled(dir);
    let mut status = status_file(dir);
    let live = running(dir);
    let state = match (&live, active) {
        (Ok(true), true) => status["state"].as_str().unwrap_or("starting"),
        (Ok(true), false) => "stopping",
        (Ok(false), false) if super::dependencies::running(dir).unwrap_or(false) => "stopping",
        (Ok(false), true) if starting(dir, &status) => "starting",
        (Ok(false), false) => "disabled",
        _ => "unavailable",
    }.to_owned();
    status["state"] = json!(state);
    status["reconcile_every_seconds"] = json!(RECONCILE.as_secs());
    if active && state == "unavailable" {
        status["action"] = json!("run lore atlas watch start to restart the worker, or lore atlas watch stop to use synchronous refresh");
    }
    if let Err(e) = live {
        status["error"] = json!(e.to_string());
    }
    status
}

pub(super) struct Snapshot {
    pub value: Value,
}

impl Snapshot {
    pub fn describe(&self) -> String {
        let field = |key: &str| self.value[key].as_str().unwrap_or("—");
        let worker = &self.value["worker"];
        let mut line = format!(
            "project snapshot [{}, {}], published {}, checked {}. Worker {}.",
            field("state"), field("mode"), field("refreshed_at"), field("checked_at"),
            worker["state"].as_str().unwrap_or("unknown"),
        );
        for note in [self.value["error"].as_str(), worker["error"].as_str(), worker["watcher_error"].as_str(), worker["action"].as_str()].into_iter().flatten() {
            line.push_str(&format!(" {note}"));
        }
        line
    }

    pub fn missing(&self) -> LoreError {
        if let Some(error) = self.value["error"].as_str() {
            return LoreError::new(format!("project snapshot unavailable ({error})"));
        }
        let action = match self.value["worker"]["state"].as_str() {
            Some("starting" | "updating" | "idle" | "waiting_for_writer") => "wait for the worker or check lore atlas watch status",
            Some("unavailable" | "error") => "check lore atlas watch status, restart with lore atlas watch start, or run lore atlas index",
            _ => "run lore atlas index or lore atlas watch start",
        };
        LoreError::new(format!("atlas: no completed project snapshot; {action}"))
    }
}

pub(super) fn snapshot(config: &Config, directory: Option<&Path>, mode: &str) -> (Option<Atlas>, Snapshot) {
    let (atlas, issue) = match store::open_snapshot(config, directory.filter(|_| mode == "background")) {
        Ok(atlas) => (atlas, None),
        Err(error) => (None, Some(error.message)),
    };
    let meta = |key| atlas.as_ref().and_then(|atlas| atlas.meta(key));
    let refreshed = meta("refreshed");
    let checked = meta("checked").or_else(|| refreshed.clone());
    let snapshot = Snapshot { value: json!({
        "mode": mode,
        "state": if issue.is_some() { "unavailable" } else if atlas.is_some() { "ready" } else { "unindexed" },
        "error": issue,
        "path": atlas.as_ref().map(|atlas| &atlas.path),
        "refreshed_at": refreshed,
        "checked_at": checked,
        "revision": meta("project_revision").and_then(|value| value.parse::<u64>().ok()),
        "worker": worker_status(directory),
    }) };
    (atlas, snapshot)
}

pub(super) fn requested(snapshot_flag: bool) -> bool {
    snapshot_flag || std::env::var("LORE_ATLAS_SNAPSHOT").is_ok_and(|value| value.eq_ignore_ascii_case("on"))
}

pub(super) fn manage(config: &Config, action: &str) -> Result<Snapshot, LoreError> {
    let root = &config.root;
    let mut directory = worker_dir(root);
    match action {
        "status" => {}
        "start" => {
            let dir = start(config, false)?;
            if !config.atlas.sources.is_empty() {
                super::dependencies::enable(root, &dir)?;
            }
            // The CLI waits for the process handshake. Index construction
            // stays in the worker.
            let started = Instant::now();
            while !running(&dir).map_err(error)? && starting(&dir, &status_file(&dir))
                && started.elapsed() < Duration::from_secs(2)
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            directory = Some(dir);
        }
        "stop" => {
            if let Some(dir) = &directory {
                let control = store::lock_file(dir, CONTROL)?;
                FileExt::lock_exclusive(&control).map_err(error)?;
                match fs::remove_file(dir.join(ENABLED)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(error(e)),
                }
                super::dependencies::disable(dir).map_err(error)?;
            }
        }
        _ => return Err(error("use lore atlas watch start, stop or status")),
    }
    let mode = if directory.as_deref().is_some_and(enabled) { "background" } else { "synchronous" };
    let (_, status) = snapshot(config, directory.as_deref(), mode);
    Ok(status)
}

pub(super) fn ensure_mcp(config: &Config) -> Result<PathBuf, LoreError> {
    start(config, true)
}

pub(super) fn dependencies(config: &Config) -> super::dependencies::Catalogue {
    worker_dir(&config.root).map_or_else(|| super::dependencies::Catalogue {
        state: "unconfigured".to_owned(), ..Default::default()
    }, |dir| super::dependencies::Catalogue::read(&dir)).for_config(config)
}

fn start(config: &Config, mcp: bool) -> Result<PathBuf, LoreError> {
    if let Some(reason) = super::off_reason(config) {
        return Err(LoreError::new(reason));
    }
    let root = &config.root;
    let existing = worker_dir(root);
    let (dir, _) = store::writer_location(root, existing.as_deref())?;
    let dir = config::simplify(&fs::canonicalize(dir).map_err(error)?);
    let control = store::lock_file(&dir, CONTROL)?;
    FileExt::lock_exclusive(&control).map_err(error)?;
    if running(&dir).map_err(error)? {
        if !enabled(&dir) {
            return Err(error("worker is stopping; retry once lore atlas watch status shows disabled"));
        }
    } else if !enabled(&dir) || !starting(&dir, &status_file(&dir)) {
        write_state(&dir, &json!({"state": "starting"}))?;
        fs::write(dir.join(ENABLED), if mcp { MCP_MARKER } else { "" }).map_err(error)?;
        launch(root, &dir)?;
    }
    Ok(dir)
}

fn launch(root: &Path, dir: &Path) -> Result<(), LoreError> {
    let mut command = Command::new(std::env::current_exe().map_err(error)?);
    command.arg("--atlas-watch-worker").arg(root).arg(dir).current_dir(dir)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if let Err(e) = crate::builtin::spawn_detached(&mut command) {
        write_state(dir, &json!({"state": "unavailable", "error": e.to_string()}))?;
        return Err(error(e));
    }
    Ok(())
}

fn write_state(dir: &Path, value: &Value) -> Result<(), LoreError> {
    let temporary = dir.join(format!("atlas-watch.{}.tmp", std::process::id()));
    fs::write(&temporary, value.to_string()).map_err(error)?;
    fs::rename(&temporary, dir.join(STATE)).map_err(error)
}

fn relevant(event: &Event, root: &Path, dir: &Path) -> bool {
    if event.need_rescan() {
        return true;
    }
    if let EventKind::Access(access) = event.kind {
        if access != AccessKind::Close(AccessMode::Write) {
            return false;
        }
    }
    let git = root.join(".git");
    event.paths.is_empty() || event.paths.iter().any(|path| {
        !path.starts_with(dir) && (!path.starts_with(&git)
            || path.starts_with(git.join("refs"))
            || ["", "HEAD", "index", "info", "info/exclude", "packed-refs"].iter().any(|part| path == &git.join(part)))
    })
}

fn attach(root: &Path, dir: &Path, wake: &mpsc::SyncSender<()>, fault: &Arc<Mutex<Option<String>>>) -> notify::Result<RecommendedWatcher> {
    let (root, dir, wake, fault) = (root.to_path_buf(), dir.to_path_buf(), wake.clone(), fault.clone());
    let watch_root = root.clone();
    let mut watcher = RecommendedWatcher::new(move |event: notify::Result<Event>| {
        match event {
            Ok(event) if relevant(&event, &root, &dir) => {}
            Ok(_) => return,
            Err(e) => *fault.lock().expect("watcher fault lock") = Some(e.to_string()),
        }
        // Only a dirty signal is needed. One slot also keeps bursts arriving
        // during a long SQLite transaction bounded and visible to the next pass.
        let _ = wake.try_send(());
    }, notify::Config::default().with_follow_symlinks(false))?;
    watcher.watch(&watch_root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

fn debounce(wake: &mpsc::Receiver<()>) {
    let until = Instant::now() + MAX_DEBOUNCE;
    while wake.recv_timeout(QUIET.min(until.saturating_duration_since(Instant::now()))).is_ok() {
        if Instant::now() >= until {
            break;
        }
    }
}

fn reconcile(root: &Path, dir: &Path, started: impl FnOnce() -> Result<(), LoreError>) -> Result<bool, LoreError> {
    let mcp = fs::read_to_string(dir.join(ENABLED)).is_ok_and(|mode| mode == MCP_MARKER);
    let config = if mcp { super::mcp::project_config(root)? } else { config::load(root)? };
    if config.root != root {
        return Err(error(format!("missing project configuration in {}", root.display())));
    }
    if let Some(reason) = super::off_reason(&config) {
        return Err(error(reason));
    }
    let Some(mut writer) = store::writer(&config, Some(dir), Duration::ZERO)? else { return Ok(false) };
    started()?;
    writer.refresh_with(&config, RefreshKind::Reconcile, || enabled(dir))?;
    let space = store::unreal_store::VectorSpace::new(config.embedding_model());
    let model = if config.atlas.unreal_root.is_some() { &space.key } else { space.model.id() };
    super::ann::schedule(&writer, &config, &space.key, model, space.dimension);
    drop(writer);
    if let Some(engine) = config.atlas.unreal_root.as_ref().and_then(|root| super::unreal::Unreal::resolve(root).ok()) {
        if let Ok(Some(atlas)) = Atlas::open_ann_snapshot(&engine.path, &engine.root, false) {
            super::ann::schedule(&atlas, &config, &space.key, &space.key, space.dimension);
        }
    }
    Ok(true)
}

/// Entrypoint used only by the detached binary. Kernel locks are released
/// even if the process is killed. The user's enabled marker survives a crash.
pub(crate) fn run_worker(root: &Path, dir: &Path) -> Result<(), LoreError> {
    let lease = store::lock_file(dir, LEASE)?;
    match FileExt::try_lock_exclusive(&lease) {
        Ok(()) => {}
        Err(e) if store::is_locked(&e) => return Ok(()),
        Err(e) => return Err(error(e)),
    }
    let (send, wake) = mpsc::sync_channel(1);
    let fault = Arc::new(Mutex::new(None));
    let mut watcher = None;
    let mut watch_error = None;
    let mut next_attach = Instant::now();
    let mut next_refresh = Instant::now();
    let mut attempts = 0;
    let publish = |phase, attempts, message, watch_error: &Option<String>| write_state(dir, &json!({
        "state": phase, "pid": std::process::id(), "updates": attempts, "error": message, "watcher_error": watch_error,
    }));
    while enabled(dir) {
        let watcher_fault = fault.lock().expect("watcher fault lock").take().or_else(|| {
            // Some backends lose the root silently if it is moved or removed.
            (watcher.is_some() && !root.is_dir()).then(|| format!("watched checkout disappeared: {}", root.display()))
        });
        if let Some(e) = watcher_fault {
            watcher = None;
            watch_error = Some(e);
            next_attach = Instant::now() + RETRY;
            publish("error", attempts, None::<String>, &watch_error)?;
        }
        if watcher.is_none() && Instant::now() >= next_attach {
            match attach(root, dir, &send, &fault) {
                Ok(current) => {
                    watcher = Some(current);
                    watch_error = None;
                    next_refresh = Instant::now();
                }
                Err(e) => {
                    let message = format!("filesystem watcher: {e}");
                    if watch_error.as_deref() != Some(&message) {
                        watch_error = Some(message);
                        publish("error", attempts, None::<String>, &watch_error)?;
                    }
                }
            }
            next_attach = Instant::now() + WATCH_RETRY;
        }
        if Instant::now() >= next_refresh {
            let attempt = reconcile(root, dir, || {
                attempts += 1;
                publish("updating", attempts, None::<String>, &watch_error)
            });
            match attempt {
                Ok(true) => {
                    publish("idle", attempts, None::<String>, &watch_error)?;
                    next_refresh = Instant::now() + RECONCILE;
                }
                Ok(false) => {
                    publish("waiting_for_writer", attempts, None::<String>, &watch_error)?;
                    next_refresh = Instant::now() + POLL;
                }
                Err(e) if enabled(dir) => {
                    publish("error", attempts, Some(e.to_string()), &watch_error)?;
                    next_refresh = Instant::now() + RETRY;
                }
                Err(_) => break,
            }
        }
        if wake.recv_timeout(POLL.min(next_refresh.saturating_duration_since(Instant::now()))).is_ok() {
            debounce(&wake);
            next_refresh = Instant::now();
        }
    }
    Ok(())
}
