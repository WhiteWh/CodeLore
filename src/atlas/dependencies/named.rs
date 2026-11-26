//! Attach configured source roots without inspecting their contents on the
//! query path. The shared writer enumerates the tree after acquiring its lease.
use super::{digest, path_key, Files, Plan, Source};
use crate::atlas::{scan, store, unreal};
use crate::config::{self, AtlasSource, Config};
use crate::LoreError;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

mod watcher;
pub(super) use watcher::TreeWatches;

fn owner(config: &Config) -> &Path {
    config.atlas.checkout.as_deref().unwrap_or(&config.root)
}

fn configured_path(config: &Config, source: &AtlasSource) -> PathBuf {
    owner(config).join(&source.root)
}

fn selection(source: &AtlasSource) -> Value {
    json!([source.include, source.exclude, source.max_kib])
}

pub(super) fn binding(config: &Config, source: &AtlasSource) -> String {
    digest(json!([path_key(&configured_path(config, source)), selection(source)]).to_string())
}

pub(super) fn pending(config: &Config, name: &str, profile: &AtlasSource) -> Source {
    let path = configured_path(config, profile);
    Source {
        name: Some(name.to_owned()), binding: Some(binding(config, profile)), configured_path: Some(path.clone()),
        canonical_path: path, source_root: None, index_key: String::new(), scope: "configured_tree".to_owned(),
        version: None, observed_files: 0, implementation: "unconfirmed".to_owned(), freshness: "configured".to_owned(),
        missing_files: 0, provenance: vec![json!({
            "artifact": owner(config).join(profile.config_file), "field": format!("atlas.sources.{name}"),
            "profile": {"include": profile.include, "exclude": profile.exclude, "max_kib": profile.max_kib},
        })], provenance_count: 1, search_paths: Vec::new(), state: "unindexed".to_owned(),
        note: Some("Waiting for the external source worker. Run an MCP search or lore atlas watch start.".to_owned()),
    }
}

pub(super) fn plans(config: &Config) -> (Vec<Plan>, Vec<Source>) {
    let mut plans = Vec::new();
    let mut unavailable = Vec::new();
    for (name, profile) in &config.atlas.sources {
        match resolve(config, name, profile) {
            Ok(source) => plans.push(Plan { source, files: Files::Tree(profile.clone()) }),
            Err(error) => {
                let mut source = pending(config, name, profile);
                source.state = "unavailable".to_owned();
                source.note = Some(error.message);
                unavailable.push(source);
            }
        }
    }
    (plans, unavailable)
}

pub(in crate::atlas) fn resolve(config: &Config, name: &str, profile: &AtlasSource) -> Result<Source, LoreError> {
    let mut source = pending(config, name, profile);
    let root = canonical_root(&source.canonical_path, config)?;
    source.index_key = digest(json!([
        "atlas-configured-source-v1", store::ATLAS_VERSION, path_key(&root), selection(profile)
    ]).to_string());
    source.source_root = Some(root.clone());
    source.canonical_path = root;
    source.state = "indexing".to_owned();
    source.note = None;
    Ok(source)
}

fn canonical_root(path: &Path, config: &Config) -> Result<PathBuf, LoreError> {
    let identify = |error| LoreError::new(format!("{}: {error}", path.display()));
    let root = config::simplify(&path.canonicalize().map_err(identify)?);
    if !root.is_dir() {
        return Err(LoreError::new(format!("{} is not a directory", path.display())));
    }
    if root.parent().is_none() || config.root.starts_with(&root) {
        return Err(LoreError::new(format!("{} encloses the active workspace or is a filesystem root", root.display())));
    }
    Ok(root)
}

pub(in crate::atlas) fn files(root: &Path, profile: &AtlasSource, active: &impl Fn() -> bool) -> Result<Option<BTreeSet<PathBuf>>, LoreError> {
    let mut files = BTreeSet::new();
    let cache = crate::index::cache_dir().canonicalize().ok().map(|path| config::simplify(&path));
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if !active() {
            return Ok(None);
        }
        if cache.as_ref() == Some(&directory) {
            continue;
        }
        if directory.canonicalize().ok().map(|path| config::simplify(&path)).as_ref() != Some(&directory) {
            return Err(LoreError::new(format!("external directory changed while scanning: {}", directory.display())));
        }
        let error = |error| LoreError::new(format!("{}: {error}", directory.display()));
        for entry in fs::read_dir(&directory).map_err(error)? {
            if !active() {
                return Ok(None);
            }
            let entry = entry.map_err(error)?;
            let path = entry.path();
            let Some(relative) = path.strip_prefix(root).ok().and_then(Path::to_str) else { continue };
            let relative = relative.replace('\\', "/");
            if scan::default_excluded(&relative) || scan::excluded(&relative, &profile.exclude) {
                continue;
            }
            let meta = fs::symlink_metadata(&path).map_err(|error| LoreError::new(format!("{}: {error}", path.display())))?;
            if unreal::is_link(&meta) {
                continue;
            }
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file() && scan::language(&relative, &profile.include).is_some() {
                files.insert(path);
            }
        }
    }
    Ok(Some(files))
}
