//! The accepted FTS5 index (task 143) plugged into the command layer's
//! [`Finder`] seam: words and whole terms from the index, meaning from the
//! built-in embedder (task 146), fused by reciprocal rank as CodeLore does.
use crate::config::Config;
use crate::embed;
use crate::finder;
use crate::index;
use crate::search::{self, Filters, Finder, Found, IndexInfo};
use crate::Result;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the `lore` binary (`app::main`): a missing model is fetched rather
/// than only reported. A library caller - the tests, another package's
/// harness - never downloads anything unless it asks.
static AUTO_FETCH: AtomicBool = AtomicBool::new(false);

/// Turns the model fetch on for this process unless `LORE_MODEL_FETCH` is
/// `off` (or `0`, `false`, `no`).
pub fn enable_auto_fetch() {
    let off = std::env::var("LORE_MODEL_FETCH").ok().map(|value| value.trim().to_lowercase()).is_some_and(|value| matches!(value.as_str(), "off" | "0" | "false" | "no"));
    AUTO_FETCH.store(!off, Ordering::Relaxed);
    BACKGROUND.store(true, Ordering::Relaxed);
}

/// Set by the real CLI only (`app::main`): background processes - a model
/// server, an index - start from `current_exe`, which in a test is the test
/// binary. Independent of `LORE_MODEL_FETCH`, which is about downloads.
static BACKGROUND: AtomicBool = AtomicBool::new(false);

fn background_allowed() -> bool {
    BACKGROUND.load(Ordering::Relaxed)
}

pub fn auto_fetch() -> bool {
    AUTO_FETCH.load(Ordering::Relaxed)
}

/// After a search that left records without a vector (a whole lore, once the
/// model arrived): one detached `lore -C <root> index` embeds them, so the next
/// searches have meaning everywhere. A marker beside the index keeps a second
/// search from starting another; the note says what this one did.
pub fn index_in_background(config: &Config, notes: &mut Vec<String>) {
    if !auto_fetch() || !notes.iter().any(|note| note.contains(finder::UNEMBEDDED_NOTE)) {
        return;
    }
    let Some(dir) = index::index_dir_candidates(&config.root).next() else { return };
    let lock = dir.join("index.embedding");
    let fresh = std::fs::metadata(&lock).and_then(|meta| meta.modified()).ok().and_then(|at| at.elapsed().ok()).is_some_and(|age| age.as_secs() < 3 * 3600);
    if fresh {
        notes.push("`lore index` is embedding them in the background".to_owned());
        return;
    }
    let started = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&lock, std::process::id().to_string())?;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg("-C")
            .arg(&config.root)
            .arg("index")
            .env("LORE_FETCH_LOCK", &lock)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        spawn_detached(&mut command)
    })();
    match started {
        Ok(()) => notes.push("`lore index` started in the background to embed them".to_owned()),
        Err(_) => {
            let _ = std::fs::remove_file(&lock);
        }
    }
}

/// The marker a background fetch holds while it runs: `<models>/<model>.fetching`.
pub fn fetch_lock(model: embed::EmbeddingModel) -> PathBuf {
    let dir = embed::model_dir(model);
    let mut name = dir.file_name().map(|name| name.to_os_string()).unwrap_or_default();
    name.push(".fetching");
    dir.with_file_name(name)
}

/// A fetch of `model` started in the last three hours and not finished.
pub fn fetching(model: embed::EmbeddingModel) -> bool {
    std::fs::metadata(fetch_lock(model))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age.as_secs() < 3 * 3600)
}

/// Starts `lore model fetch --model <model>` detached from this process, once:
/// the marker keeps a second search from starting another. The note says what
/// this search does meanwhile.
pub fn fetch_in_background(model: embed::EmbeddingModel) -> String {
    let size_mib = model.files().iter().map(|file| file.bytes).sum::<u64>() / (1024 * 1024);
    if fetching(model) {
        return format!("the model {} is being fetched in the background ({size_mib} MiB); text only until it is in", model.id());
    }
    let lock = fetch_lock(model);
    let started = (|| -> std::io::Result<()> {
        if let Some(parent) = lock.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&lock, std::process::id().to_string())?;
        let exe = std::env::current_exe()?;
        let mut command = std::process::Command::new(exe);
        command
            .args(["model", "fetch", "--model", model.id()])
            .env("LORE_FETCH_LOCK", &lock)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        spawn_detached(&mut command)
    })();
    match started {
        Ok(()) => format!("the model {} was missing and is being fetched in the background ({size_mib} MiB; LORE_MODEL_FETCH=off stops this); text only until it is in", model.id()),
        Err(error) => {
            let _ = std::fs::remove_file(&lock);
            format!("the model {} is missing and could not be fetched in the background ({error}); run `lore model fetch`", model.id())
        }
    }
}

#[cfg(windows)]
fn spawn_detached(command: &mut std::process::Command) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // The child gets null stdio of its own, but CreateProcess also hands it every
    // inheritable handle of ours - the pipe our caller reads stdout from among
    // them - and the caller would wait for the end of a download to see EOF. Our
    // standard handles stop being inheritable first.
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> *mut std::ffi::c_void;
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for which in [-10i32, -11, -12] {
        // SAFETY: GetStdHandle takes any std-handle id; SetHandleInformation
        // only changes the inherit flag of a handle this process owns, and a
        // null or invalid handle makes it fail harmlessly.
        unsafe {
            let handle = GetStdHandle(which as u32);
            if !handle.is_null() && handle as isize != -1 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    // Out of the caller's job when the job allows it (a harness may kill its
    // job's processes when the command ends); else inside it.
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    match command.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Ok(_) => Ok(()),
        Err(_) => command.creation_flags(base).spawn().map(|_| ()),
    }
}

#[cfg(not(windows))]
fn spawn_detached(command: &mut std::process::Command) -> std::io::Result<()> {
    command.spawn().map(|_| ())
}

/// Fetches `model` now, its progress on stderr: what `lore index` does when the
/// model is missing, since an index is what the person asked for.
fn fetch_now(model: embed::EmbeddingModel) -> std::result::Result<(), String> {
    let lock = fetch_lock(model);
    if let Some(parent) = lock.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&lock, std::process::id().to_string());
    let size_mib = model.files().iter().map(|file| file.bytes).sum::<u64>() / (1024 * 1024);
    eprintln!("lore: the model {} is not installed; fetching it ({size_mib} MiB, once; LORE_MODEL_FETCH=off stops this)", model.id());
    let mut last = std::time::Instant::now();
    let mut progress = |name: &str, done: u64, total: u64| {
        if done == total || last.elapsed().as_secs() >= 5 {
            last = std::time::Instant::now();
            eprintln!("lore: {name}: {} / {} MiB", done / (1024 * 1024), total / (1024 * 1024));
        }
    };
    let result = embed::fetch_model(model, embed::ModelSource::Cache, &mut progress).map(|_| ()).map_err(|error| error.to_string());
    let _ = std::fs::remove_file(&lock);
    result
}

/// The search the `lore` binary hands `cli::run`.
pub struct Builtin;

/// A static instance, because the command layer borrows the finder.
pub static BUILTIN: Builtin = Builtin;

impl Builtin {
    /// The embedder, when the configuration and the machine have one.
    fn semantic(&self, config: &Config, text_only: bool, wait: bool) -> (Option<embed::Shared>, Vec<String>) {
        let mut notes = Vec::new();
        if text_only {
            return (None, notes);
        }
        if config.embed_provider == "none" {
            notes.push("semantic search is off (embed.provider = \"none\"); text only".to_owned());
            return (None, notes);
        }
        let selected = config.embedding_model();
        // A search asks the model server first (src/embed/serve.rs): no model of
        // its own to open, the query is embedded in the server's memory.
        let serve = !wait && background_allowed() && embed::serve::enabled();
        if serve {
            if let Some(remote) = embed::serve::connect(selected) {
                return (Some(std::sync::Arc::new(remote) as embed::Shared), notes);
            }
        }
        // `wait` is an index being built: the GPU for its thousands of texts; a
        // search embeds one query and a few new records, faster on the CPU.
        let open = || {
            let opened = if wait { embed::Embedder::open_model(selected, embed::ModelSource::Cache) } else { embed::Embedder::open_model_for_queries(selected, embed::ModelSource::Cache) };
            opened.map(|embedder| std::sync::Arc::new(embedder) as embed::Shared)
        };
        let mut opened = open();
        if opened.is_err() && auto_fetch() && !embed::status_for(selected, &embed::model_dir(selected)).available {
            if wait && !fetching(selected) {
                match fetch_now(selected) {
                    Ok(()) => opened = open(),
                    Err(error) => notes.push(format!("the model {} could not be fetched: {error}", selected.id())),
                }
            } else {
                notes.push(fetch_in_background(selected));
                return (None, notes);
            }
        }
        match opened {
            Ok(shared) => {
                if serve {
                    // for the searches after this one
                    embed::serve::start(selected, spawn_detached);
                }
                (Some(shared), notes)
            }
            Err(_error) => {
                let status = embed::status_for(selected, &embed::model_dir(selected));
                notes.push(format!(
                    "semantic search unavailable: {}; text only",
                    if status.reason.is_empty() { "the model is not installed".to_owned() } else { status.reason }
                ));
                (None, notes)
            }
        }
    }
}

impl Finder for Builtin {
    fn find_indexed(
        &self,
        config: &Config,
        query: &str,
        filters: &Filters,
        limit: usize,
        text_only: bool,
    ) -> Option<Result<(Vec<Found>, Vec<String>)>> {
        Some((|| -> Result<(Vec<Found>, Vec<String>)> {
            let index = index::open_index(config)?;
            let wanted = finder::Filters {
                kind: filters.kind.clone(),
                area: filters.area.clone(),
                status: filters.status.clone(),
                plan: filters.plan.clone(),
                all: filters.all,
                sources: filters.sources,
            };
            // The text search first: a pasted error that a record carries, a
            // phrase found whole, or a query that is code (a path, a symbol, an
            // error code, a quote) is answered there as well as with the model
            // (measured: exact errors 94% first without it, 93% with it) and in
            // a tenth of a second instead of the model's seconds. Words in
            // plain language go on to the model, where meaning pays.
            if !text_only && !filters.semantic {
                let (hits, notes) = finder::find_records(&index, query, &wanted, limit, None)?;
                let exact_hit = hits.first().is_some_and(|(hit, _)| hit.via.iter().any(|via| via == "signal" || via == "phrase"));
                if exact_hit || crate::stats::classify(query) == "exact" {
                    let found = hits.into_iter().map(|(hit, record)| Found { record, score: hit.score, via: hit.via, meaning: hit.meaning, store: None }).collect();
                    return Ok((found, notes));
                }
            }
            let (shared, mut notes) = self.semantic(config, text_only, false);
            let wanted = finder::Filters {
                kind: filters.kind.clone(),
                area: filters.area.clone(),
                status: filters.status.clone(),
                plan: filters.plan.clone(),
                all: filters.all,
                sources: filters.sources,
            };
            let selected = config.embedding_model();
            let semantic = shared.as_ref().map(|shared| finder::Semantic {
                embedder: shared.as_ref(),
                model: selected.id(),
                doc_prefix: selected.document_prefix(),
                query_prefix: selected.query_prefix(),
            });
            let (hits, mut more) = finder::find_records(&index, query, &wanted, limit, semantic.as_ref())?;
            notes.append(&mut more);
            index_in_background(config, &mut notes);
            let found = hits.into_iter().map(|(hit, record)| Found { record, score: hit.score, via: hit.via, meaning: hit.meaning, store: None }).collect();
            Ok((found, notes))
        })())
    }

    fn find(
        &self,
        config: &Config,
        records: &[Value],
        query: &str,
        filters: &Filters,
        limit: usize,
        text_only: bool,
    ) -> Result<(Vec<Found>, Vec<String>)> {
        let index = index::open_index(config)?;
        let (shared, mut notes) = self.semantic(config, text_only, false);
        let wanted = finder::Filters {
            kind: filters.kind.clone(),
            area: filters.area.clone(),
            status: filters.status.clone(),
            plan: filters.plan.clone(),
            all: filters.all,
            sources: filters.sources,
        };
        let selected = config.embedding_model();
        let semantic = shared.as_ref().map(|shared| finder::Semantic {
            embedder: shared.as_ref(),
            model: selected.id(),
            doc_prefix: selected.document_prefix(),
            query_prefix: selected.query_prefix(),
        });
        let (hits, mut more) = finder::find(&index, query, &wanted, limit, semantic.as_ref())?;
        notes.append(&mut more);
        index_in_background(config, &mut notes);
        let visible = search::visible(records, filters);
        let found = hits
            .into_iter()
            .filter_map(|hit| {
                visible.get(&hit.id).map(|record| Found { record: record.clone(), score: hit.score, via: hit.via, meaning: hit.meaning, store: None })
            })
            .collect();
        Ok((found, notes))
    }

    fn index(&self, config: &Config, _records: &[Value], rebuild: bool, text_only: bool) -> Result<IndexInfo> {
        if rebuild {
            // The old file goes first: opening it only to delete it would build it once for nothing.
            index::remove_indexes(&config.root)?;
        }
        let index = index::open_index(config)?;
        let (shared, mut notes) = self.semantic(config, text_only, true);
        let mut vectors = 0;
        let mut semantic = false;
        if let Some(shared) = &shared {
            let selected = config.embedding_model();
            match index.ensure_vectors(shared.as_ref(), selected.id(), selected.document_prefix()) {
                Ok(()) => {
                    semantic = true;
                    vectors = index.vectors(selected.id()).map(|vectors| vectors.len()).unwrap_or(0);
                }
                Err(error) => notes.push(format!("semantic search unavailable: {error}; text only")),
            }
        }
        let info = IndexInfo {
            records: index.count()?,
            vectors,
            index: index.path().display().to_string(),
            semantic,
            note: if notes.is_empty() { None } else { Some(notes.join("; ")) },
        };
        Ok(info)
    }
}
