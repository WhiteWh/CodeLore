//! Published external source catalogues. Discovery and indexing happen in
//! the project watcher; readers only load its catalogue and ready databases.
mod discovery;
mod listed;
pub(super) mod named;
pub(super) use discovery::implementation;

use super::store::{self, dependency_store, Atlas};
use super::unreal::digest;
use crate::{config::{AtlasSource, Config}, LoreError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const CATALOGUE: &str = "atlas-dependencies.json";
const ENABLED: &str = "atlas-dependencies.enabled";
const LEASE: &str = "atlas-dependencies-worker.lock";
const CONTROL: &str = "atlas-dependencies-control.lock";
const STARTING: &str = "atlas-dependencies.starting";
const START_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_secs(2);
const PROFILE: &str = "atlas-build-dependencies-v1";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Evidence {
    artifact: PathBuf,
    field: &'static str,
    context: String,
    recorded_at: String,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(super) struct Source {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    configured_path: Option<PathBuf>,
    pub canonical_path: PathBuf,
    pub source_root: Option<PathBuf>,
    pub index_key: String,
    scope: String,
    version: Option<String>,
    observed_files: usize,
    implementation: String,
    freshness: String,
    missing_files: usize,
    provenance: Vec<Value>,
    provenance_count: usize,
    search_paths: Vec<PathBuf>,
    state: String,
    note: Option<String>,
}

impl Source {
    pub(in crate::atlas) fn directory(&self) -> PathBuf {
        crate::index::cache_dir().join("atlas-dependencies").join(&self.index_key)
    }

    pub fn snapshot(&self) -> Result<Option<Atlas>, LoreError> {
        if self.index_key.is_empty() {
            return Ok(None);
        }
        dependency_store::open_ready(self)
    }

    pub fn ready(&self) -> bool {
        self.state == "ready" ||
            self.state == "indexing" && self.snapshot().ok().flatten().is_some()
    }

    pub fn status(&self) -> Value {
        let mut value = json!(self);
        let kind = if self.name.is_some() { "external" } else { "dependency" };
        value["source"] = json!(kind);
        if self.index_key.is_empty() {
            value["canonical_path"] = Value::Null;
            value["root"] = json!(self.configured_path);
            value["index_key"] = Value::Null;
            value["path_role"] = json!("configured_root");
        } else {
            value["root"] = json!(self.canonical_path);
            value["source_id"] = json!(format!("{kind}:{}", self.index_key));
            value["path_role"] = json!(if self.source_root.is_some() { "declared_source_root" } else { "observed_container" });
        }
        value.as_object_mut().expect("source status").remove("binding");
        value
    }

    pub fn implementation_at(&self, atlas: &Atlas) -> &'static str {
        if atlas.meta("dependency_implementations").and_then(|count| count.parse::<usize>().ok()).unwrap_or_default() > 0 {
            "available"
        } else if self.implementation == "unconfirmed" {
            "unconfirmed"
        } else {
            "unavailable"
        }
    }
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
pub(super) struct Catalogue {
    pub sources: Vec<Source>,
    pub state: String,
    pub notes: Vec<String>,
}

impl Catalogue {
    /// Match persisted attachments to the current settings without touching
    /// external files. Edits and removals take effect even during a refresh.
    pub fn for_config(mut self, config: &Config) -> Self {
        self.sources.retain(|source| {
            let Some(name) = &source.name else { return true };
            config.atlas.sources.get(name).is_some_and(|profile| {
                source.binding.as_deref() == Some(named::binding(config, profile).as_str())
            })
        });
        for (name, profile) in &config.atlas.sources {
            if !self.sources.iter().any(|source| source.name.as_deref() == Some(name.as_str())) {
                self.sources.push(named::pending(config, name, profile));
            }
        }
        self.sources.sort_by(|a, b| a.name.is_none().cmp(&b.name.is_none()).then(a.name.cmp(&b.name)));
        if matches!(self.state.as_str(), "ready" | "indexing" | "empty") {
            self.update_state(false);
        }
        self
    }

    pub fn read(dir: &Path) -> Self {
        let path = dir.join(CATALOGUE);
        let mut catalogue = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| Self::error(&path, error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self {
                state: if enabled(dir) { "discovering" } else { "unconfigured" }.to_owned(), ..Self::default()
            },
            Err(error) => Self::error(&path, error),
        };
        if enabled(dir) && super::watch::enabled(dir) {
            match running(dir) {
                Ok(true) => {}
                Ok(false) if starting(dir) => {}
                Ok(false) => {
                    catalogue.state = "unavailable".to_owned();
                    catalogue.notes.push("Dependency worker is unavailable. Run a dependency-enabled MCP search to restart it.".to_owned());
                }
                Err(error) => {
                    catalogue.state = "unavailable".to_owned();
                    catalogue.notes.push(format!("Dependency worker lease: {error}"));
                }
            }
        }
        catalogue
    }

    fn error(path: &Path, error: impl std::fmt::Display) -> Self {
        Self { state: "error".to_owned(), notes: vec![format!("{}: {error}", path.display())], ..Self::default() }
    }
}

fn mark_enabled(dir: &Path) -> io::Result<()> {
    match fs::OpenOptions::new().write(true).create_new(true).open(dir.join(ENABLED)) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn starting(dir: &Path) -> bool {
    dir.join(STARTING).metadata().and_then(|meta| meta.modified())
        .ok().and_then(|stamp| stamp.elapsed().ok()).is_some_and(|age| age < START_GRACE)
}

pub(super) fn running(dir: &Path) -> io::Result<bool> {
    let lease = match fs::File::open(dir.join(LEASE)) {
        Ok(lease) => lease,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match fs2::FileExt::try_lock_shared(&lease) {
        Ok(()) => Ok(false),
        Err(error) if store::is_locked(&error) => Ok(true),
        Err(error) => Err(error),
    }
}

/// Independent of the project worker, including watchers started by an
/// earlier build. The shared project marker still controls both lifetimes.
pub(super) fn enable(root: &Path, dir: &Path) -> Result<(), LoreError> {
    let start = || -> Result<(), Box<dyn std::error::Error>> {
        mark_enabled(dir)?;
        if running(dir)? || starting(dir) {
            return Ok(());
        }
        let control = store::lock_file(dir, CONTROL)?;
        match fs2::FileExt::try_lock_exclusive(&control) {
            Ok(()) => {}
            Err(error) if store::is_locked(&error) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        if running(dir)? || starting(dir) || !super::watch::enabled(dir) {
            return Ok(());
        }
        let marker = dir.join(STARTING);
        let mut command = Command::new(std::env::current_exe()?);
        command.arg("--atlas-dependency-worker").arg(root).arg(dir).current_dir(dir)
            .env("LORE_CACHE", std::env::current_dir()?.join(crate::index::cache_dir()))
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        fs::write(&marker, b"")?;
        if let Err(error) = crate::builtin::spawn_detached(&mut command) {
            let _ = fs::remove_file(marker);
            return Err(error.into());
        }
        Ok(())
    };
    start().map_err(|error| LoreError::new(format!("atlas dependency discovery: {error}")))
}

pub(super) fn enabled(dir: &Path) -> bool {
    dir.join(ENABLED).is_file()
}

pub(super) fn disable(dir: &Path) -> std::io::Result<()> {
    match fs::remove_file(dir.join(ENABLED)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn publish(dir: &Path, catalogue: &Catalogue) -> Result<(), LoreError> {
    let value = serde_json::to_vec(catalogue).map_err(|error| LoreError::new(error.to_string()))?;
    let path = dir.join(CATALOGUE);
    if fs::read(&path).is_ok_and(|previous| previous == value) {
        return Ok(());
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temp, value).and_then(|_| fs::rename(&temp, &path))
        .map_err(|error| LoreError::new(format!("{}: {error}", path.display())))
}

#[derive(Clone)]
struct Plan {
    source: Source,
    files: Files,
}

#[derive(Clone)]
pub(in crate::atlas) enum Files {
    Listed(Arc<BTreeSet<PathBuf>>),
    Tree(AtlasSource),
}

pub(super) fn path_key(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { path.to_lowercase() } else { path }
}

fn source_key(anchor: &Path, scope: &str, version: Option<&str>, files: &BTreeSet<PathBuf>) -> String {
    let mut files: Vec<_> = files.iter().map(|path| path_key(path)).collect();
    files.sort_unstable();
    digest(json!([PROFILE, store::ATLAS_VERSION, path_key(anchor), scope, version, files]).to_string())
}

pub(crate) fn run_worker(root: &Path, dir: &Path) -> Result<(), LoreError> {
    let lease = store::lock_file(dir, LEASE)?;
    match fs2::FileExt::try_lock_exclusive(&lease) {
        Ok(()) => {}
        Err(error) if store::is_locked(&error) => return Ok(()),
        Err(error) => return Err(LoreError::new(format!("atlas dependency worker lease: {error}"))),
    }
    let _ = fs::remove_file(dir.join(STARTING));
    let mut discovery = discovery::Discovery::default();
    let mut trees = named::TreeWatches::default();
    let mut listed = listed::ListedWatches::default();
    let live = || enabled(dir) && super::watch::enabled(dir);
    while live() {
        let result = super::mcp::project_config(root).and_then(|config| {
            if super::off_reason(&config).is_some() {
                return Ok(());
            }
            reconcile(&config, dir, &mut discovery, &mut trees, &mut listed, live)
        });
        if let Err(error) = result {
            let mut previous = Catalogue::read(dir);
            previous.state = "error".to_owned();
            previous.notes = vec![error.to_string()];
            let _ = publish(dir, &previous);
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

fn reconcile(config: &Config, dir: &Path, discovery: &mut discovery::Discovery, trees: &mut named::TreeWatches, listed: &mut listed::ListedWatches, active: impl Fn() -> bool) -> Result<(), LoreError> {
    let previous = Catalogue::read(dir);
    let (mut plans, notes, discovery_error) = match discovery.plans(config) {
        Ok((plans, notes)) => (plans, notes, false),
        Err(error) => (Vec::new(), vec![error.to_string()], true),
    };
    listed.retain(&plans.iter().map(|plan| plan.source.index_key.as_str()).collect());
    let (configured, failures) = named::plans(config);
    trees.retain(&configured.iter().map(|plan| plan.source.index_key.as_str()).collect());
    plans.extend(configured);
    let space = store::unreal_store::VectorSpace::new(config.embedding_model());
    for plan in &mut plans {
        if let Some(last) = previous.sources.iter().find(|source| source.index_key == plan.source.index_key && source.name == plan.source.name) {
            plan.source.missing_files = last.missing_files;
            plan.source.note = last.note.clone();
        }
        if let Some(atlas) = plan.source.snapshot().ok().flatten() {
            super::ann::schedule(&atlas, config, &space.key, &space.key, space.dimension);
            let files = atlas.counts()?["files"];
            plan.source.state = if files > 0 { "ready" } else { "unavailable" }.to_owned();
            plan.source.implementation = plan.source.implementation_at(&atlas).to_owned();
            if plan.source.name.is_some() {
                plan.source.observed_files = atlas.meta("dependency_selected_files").and_then(|count| count.parse().ok()).unwrap_or_default();
            } else if let Some(missing) = atlas.meta("dependency_listed_missing").and_then(|count| count.parse().ok()) {
                plan.source.missing_files = missing;
            }
        }
    }
    let mut catalogue = Catalogue { state: if plans.is_empty() { "empty" } else { "indexing" }.to_owned(), notes, ..Catalogue::default() };
    if !active() {
        return Ok(());
    }
    catalogue.sources = plans.iter().map(|plan| plan.source.clone()).collect();
    catalogue.sources.extend(failures);
    if discovery_error {
        catalogue.sources.extend(previous.sources.iter().filter(|source| source.name.is_none()).cloned());
    }
    catalogue.update_state(discovery_error);
    let mut published = previous;
    if catalogue != published {
        publish(dir, &catalogue)?;
        published = catalogue.clone();
    }
    let mut listed_attempted = false;
    for i in listed.order(&plans) {
        if !active() {
            return Ok(());
        }
        let plan = &plans[i];
        let source = &mut catalogue.sources[i];
        let result = match &plan.files {
            Files::Tree(profile) => match trees.due(source, profile) {
                Ok(Some(watch)) => {
                    let result = dependency_store::refresh(source, &plan.files, &active);
                    watch.finished(matches!(result, Ok(Some(_))));
                    result
                }
                Ok(None) => continue,
                Err(error) => {
                    refresh_error(source, error);
                    continue;
                }
            },
            // Stagger expensive passes when several sources are due at
            // once. Rotation also gives other sources a turn after a retry.
            Files::Listed(_) if listed_attempted => continue,
            Files::Listed(files) => match listed.due(source, files) {
                Ok(Some(watch)) => {
                    listed_attempted = true;
                    let result = dependency_store::refresh(source, &plan.files, &active);
                    watch.finished(matches!(result, Ok(Some(_))));
                    result
                }
                Ok(None) => continue,
                Err(error) => {
                    refresh_error(source, error);
                    continue;
                }
            },
        };
        match result {
            Ok(Some(info)) => {
                source.missing_files = info.missing;
                if source.name.is_some() {
                    source.observed_files = info.selected;
                }
                source.state = if info.indexed == 0 { "unavailable" } else { "ready" }.to_owned();
                source.note = None;
                source.implementation = match (source.implementation.as_str(), info.implementations) {
                    (_, count) if count > 0 => "available",
                    ("unconfirmed", _) => "unconfirmed",
                    _ => "unavailable",
                }.to_owned();
            }
            Ok(None) => {}
            Err(error) => refresh_error(source, error),
        }
    }
    catalogue.update_state(discovery_error);
    if catalogue != published && active() {
        publish(dir, &catalogue)?;
    }
    Ok(())
}

fn refresh_error(source: &mut Source, error: LoreError) {
    if source.state != "ready" {
        source.state = "error".to_owned();
    }
    source.note = Some(error.to_string());
}

impl Catalogue {
    fn update_state(&mut self, discovery_error: bool) {
        self.state = if self.sources.iter().any(|source| matches!(source.state.as_str(), "unindexed" | "indexing")) {
            "indexing"
        } else if self.sources.iter().any(|source| source.state == "ready") {
            "ready"
        } else if discovery_error || self.sources.iter().any(|source| source.state == "error") {
            "error"
        } else if self.sources.is_empty() {
            "empty"
        } else {
            "unavailable"
        }.to_owned();
    }
}
