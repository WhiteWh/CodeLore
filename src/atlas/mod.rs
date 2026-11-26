//! Atlas: the code map beside the lore (docs/ATLAS.md). The lore holds what was
//! learned about a project; the atlas holds where things are in its code now -
//! pieces of every file searchable by words, whole terms and meaning, the
//! symbols each file defines, and the file graph of its imports. The two meet
//! where a record names a file or a symbol: `lore atlas impact` lists what the
//! lore knows about every file a change reaches, `lore atlas drift` the records
//! whose files or symbols the code no longer has, and `lore for <file>` names
//! the files that import it.
//!
//! Off unless switched on: the `atlas` cargo feature builds it, `[atlas]
//! enabled = true` in lore.toml turns it on for a project, `LORE_ATLAS=on|off`
//! overrides that for one user. One binary, no server: SQLite in the user cache
//! and the built-in embedding model, the same as the lore's own index.
pub(crate) mod ann;
mod calls;
pub(crate) mod dependencies;
pub mod chunk;
pub mod cli;
pub mod graph;
pub mod lang;
pub(crate) mod mcp;
pub mod scan;
pub mod store;
mod unreal;
pub(crate) mod watch;

use crate::config::Config;
use crate::embed;
use crate::LoreError;

pub use store::{Atlas, Filter, Hit};

/// The reason the code map does not run here, when it does not.
pub fn off_reason(config: &Config) -> Option<String> {
    if config.atlas.active() {
        return None;
    }
    let env = std::env::var("LORE_ATLAS").ok().filter(|value| !value.trim().is_empty());
    Some(match env {
        Some(value) => format!("the code map is off (LORE_ATLAS={value})"),
        None => "the code map is off for this project: set `[atlas] enabled = true` in lore.toml, or LORE_ATLAS=on for yourself".to_owned(),
    })
}

/// The code map, open and up to date, when it is on.
pub fn open_fresh(config: &Config) -> Result<(Atlas, store::Refresh), LoreError> {
    refresh_project(config, false)
}

fn refresh_project(config: &Config, rebuild: bool) -> Result<(Atlas, store::Refresh), LoreError> {
    if let Some(reason) = off_reason(config) {
        return Err(LoreError::new(reason));
    }
    let directory = watch::enabled_dir(&config.root);
    let mut atlas = store::writer(config, directory.as_deref(), std::time::Duration::from_secs(120))?
        .ok_or_else(|| LoreError::new("atlas: project writer is busy; retry lore atlas index"))?;
    let kind = if rebuild {
        store::RefreshKind::Rebuild
    } else if directory.is_some() {
        store::RefreshKind::Reconcile
    } else {
        store::RefreshKind::Quick
    };
    let refresh = atlas.refresh_with(config, kind, || true)?;
    Ok((atlas, refresh))
}

/// What `lore for` says when the map is on but not built yet.
pub const NOT_BUILT: &str = "code: the code map is not built yet - `lore index` builds it (minutes on a large tree)";

/// The map for a side section of `find` or `for`: never built from nothing
/// here (a fresh clone's first build takes minutes, and is `lore index`'s
/// job), and never waited for while another process is writing it - then it
/// is read as it is. None when the map is off or not built yet.
fn open_side(config: &Config, background: Option<&std::path::Path>, snapshot_only: bool) -> Result<(Atlas, Option<String>), String> {
    if background.is_some() || snapshot_only {
        let mode = if background.is_some() { "background" } else { "snapshot" };
        let (atlas, snapshot) = watch::snapshot(config, background, mode);
        let line = snapshot.describe();
        return atlas.map(|atlas| (atlas, Some(line.clone())))
            .ok_or_else(|| format!("{line}\n  {}", snapshot.missing()));
    }
    let open_ready = || store::open_snapshot(config, None).ok().flatten()
        .map(|atlas| (atlas, None)).ok_or_else(|| NOT_BUILT.to_owned());
    // Avoid creating an empty database for a side section.
    drop(open_ready()?);
    let Some(mut atlas) = store::writer(config, None, std::time::Duration::from_millis(300)).ok().flatten() else {
        return open_ready();
    };
    atlas.wait_at_most(std::time::Duration::from_millis(300));
    let _ = atlas.refresh(config);
    Ok((atlas, None))
}

/// Indexing opens the model on its selected device and fetches missing
/// weights in the foreground.
fn index_embedder(config: &Config, text_only: bool) -> (Option<embed::Embedder>, Vec<String>) {
    let mut notes = Vec::new();
    if text_only {
        return (None, notes);
    }
    if !config.atlas.semantic || config.embed_provider == "none" {
        notes.push("code meaning is off (atlas.semantic = false or embed.provider = \"none\"); text only".to_owned());
        return (None, notes);
    }
    let model = config.embedding_model();
    let open = || embed::Embedder::open_model(model, embed::ModelSource::Cache);
    if let Ok(embedder) = open() {
        return (Some(embedder), notes);
    }
    if crate::builtin::auto_fetch() && !crate::builtin::fetching(model) {
        eprintln!("lore: fetching the model {} for the code map", model.id());
        let mut progress = |_: &str, _: u64, _: u64| {};
        match embed::fetch_model(model, embed::ModelSource::Cache, &mut progress) {
            Ok(_) => {
                if let Ok(embedder) = open() {
                    return (Some(embedder), notes);
                }
            }
            Err(error) => notes.push(format!("the model could not be fetched: {error}")),
        }
    } else if crate::builtin::auto_fetch() {
        notes.push(crate::builtin::fetch_in_background(model));
        return (None, notes);
    }
    let status = embed::status_for(model, &embed::model_dir(model));
    notes.push(format!("code meaning unavailable: {}; text only", if status.reason.is_empty() { "the model is not installed".to_owned() } else { status.reason }));
    (None, notes)
}

/// Queries reuse the same local service as `lore find`. Snapshot reads may
/// start a server for installed weights. They do not start downloads.
fn query_embedder(model: embed::EmbeddingModel, allow_fetch: bool) -> (Option<std::sync::Arc<crate::builtin::QueryModel>>, Vec<String>) {
    // A long-lived MCP server keeps one query model, or its server connection,
    // across requests instead of opening one per call.
    static KEPT: std::sync::Mutex<Option<std::sync::Arc<crate::builtin::QueryModel>>> = std::sync::Mutex::new(None);
    let keep = mcp::serving();
    if keep {
        if let Some(kept) = KEPT.lock().ok().and_then(|kept| kept.clone()) {
            if kept.model() == model && kept.reusable() {
                return (Some(kept), Vec::new());
            }
        }
    }
    match crate::builtin::open_query_model(model) {
        Ok(embedder) => {
            let embedder = std::sync::Arc::new(embedder);
            if keep {
                if let Ok(mut kept) = KEPT.lock() {
                    *kept = Some(std::sync::Arc::clone(&embedder));
                }
            }
            (Some(embedder), Vec::new())
        }
        Err(_) if allow_fetch && crate::builtin::auto_fetch() => {
            (None, vec![crate::builtin::fetch_in_background(model)])
        }
        Err(error) => (None, vec![format!("code meaning unavailable: {error}; text only")]),
    }
}

/// `lore for <file>`'s code lines: who imports the file, and what the lore
/// knows about them. None when the map is off, `atlas.for` is false, or the
/// target is not a file the map holds.
pub fn for_lines(config: &Config, records: &[serde_json::Value], target: &str) -> Option<Vec<String>> {
    if !config.atlas.in_for || off_reason(config).is_some() {
        return None;
    }
    let directory = watch::enabled_dir(&config.root);
    let (atlas, snapshot) = match open_side(config, directory.as_deref(), watch::requested(false)) {
        Ok(found) => found,
        Err(note) => return Some(vec![note]),
    };
    let path = cli::relative_target(config, target);
    if !atlas.has_file(&path) {
        return snapshot.map(|line| vec![line]);
    }
    let (imports, imported_by) = atlas.neighbours(&path).ok()?;
    let mut lines = vec![format!("code: {path} imports {} file(s), is imported by {}", imports.len(), imported_by.len())];
    lines.extend(snapshot);
    if !imported_by.is_empty() {
        lines.push(format!("  imported by: {}", cli::list_short(&imported_by, 8)));
    }
    let around = cli::records_about(config, records, &imported_by, &[path.clone()]);
    if !around.is_empty() {
        lines.push("  the lore about files that import it:".to_owned());
        for (record, file) in around.iter().take(8) {
            let field = |key: &str| record.get(key).and_then(serde_json::Value::as_str).unwrap_or("").to_owned();
            lines.push(format!("    {}  {}  (via {file})", field("id"), field("title")));
        }
    }
    Some(lines)
}

/// `lore find`'s code lines when `atlas.find` is on: the best places in the
/// code, text only so a find never waits for a model.
pub fn find_lines(config: &Config, query: &str) -> Option<Vec<String>> {
    if !config.atlas.in_find || off_reason(config).is_some() {
        return None;
    }
    let directory = watch::enabled_dir(&config.root);
    let (atlas, snapshot) = match open_side(config, directory.as_deref(), watch::requested(false)) {
        Ok(found) => found,
        Err(note) if directory.is_some() || watch::requested(false) => return Some(vec![note]),
        Err(_) => return None,
    };
    let hits = atlas.search(query, 3, &Filter::default(), None).ok()?;
    if hits.is_empty() {
        return snapshot.map(|line| vec![line]);
    }
    let mut lines = vec!["code:".to_owned()];
    lines.extend(snapshot);
    for hit in hits {
        lines.push(format!("  {}:{}-{}  {}", hit.path, hit.start, hit.end, hit.symbols.first().cloned().unwrap_or_default()));
    }
    Some(lines)
}
