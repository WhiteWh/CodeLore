//! The code map's SQLite file, beside the lore's index in the user cache and as
//! disposable: files by blob id, their pieces, symbols, imports and the file
//! graph, two FTS5 tables and the pieces' vectors. Only files whose blob id
//! moved are read again. WAL readers can pin a committed project snapshot.
use super::chunk::chunk;
use super::graph::{self, Resolver};
use super::lang::{self, Lang};
use super::scan::{self, Entry};
use crate::config::Config;
use crate::embed::Embeddings;
use crate::index::{cache_dir, private_temp, repo_key, terms_of, word_re};
use crate::LoreError;
use regex::Regex;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

pub const ATLAS_VERSION: i64 = 2;
pub const FILE_NAME: &str = "atlas.sqlite";
#[path = "derived.rs"]
mod derived;
#[path = "semantic.rs"]
mod semantic;
pub(super) use semantic::{SemanticPlan, SearchTimings};
#[path = "dependency_store.rs"]
pub(super) mod dependency_store;
#[path = "unreal_store.rs"]
pub(super) mod unreal_store;
const RRF_K: f64 = 60.0;
const DEPTH: usize = 80;
const FILTER_LIMIT: usize = 65_536;
const FILTER_IDS: &str = "INSERT INTO temp.atlas_search_ids(id)
    SELECT c.id FROM files f CROSS JOIN chunks c ON c.path=f.path
    WHERE substr(f.path,1,length(?1)) = ?1 COLLATE NOCASE AND (?2 = '' OR f.lang = ?2)
    LIMIT ?3";
/// The start of a piece an embedding reads.
const EMBED_CHARS: usize = 1500;
/// A search embeds at most this many new pieces itself; more wait for `lore atlas index`.
pub const SEARCH_EMBED_BUDGET: usize = 48;

// The short key finds qualified names first. SQLite then searches the existing
// NOCASE name index for those names and for the exact spelling of the query.
const INDEXED_SHORT_MATCH: &str =
    "s.name COLLATE NOCASE IN (SELECT ?1 UNION ALL SELECT name FROM atlas_short_names WHERE short = ?1)";
const LEGACY_SHORT_MATCH: &str =
    "s.name = ?1 COLLATE NOCASE
     OR (length(s.name) >= length(?1) + 2 AND substr(s.name, -length(?1)) = ?1 COLLATE NOCASE AND substr(s.name, -length(?1) - 2, 2) = '::')
     OR (length(s.name) >= length(?1) + 1 AND substr(s.name, -length(?1)) = ?1 COLLATE NOCASE AND substr(s.name, -length(?1) - 1, 1) = '.')";

// Use the existing NOCASE index to seek the short name, then enforce Rust's
// exact case. Keep the name and language checks ahead of the chunk lookup.
const RUST_SYMBOL_CHUNKS: &str =
    "SELECT DISTINCT c.id, s.path
     FROM symbols s
     CROSS JOIN files f ON f.path = s.path
     CROSS JOIN chunks c ON c.path = s.path AND s.line BETWEEN c.start AND c.stop
     WHERE s.name = ?1 COLLATE NOCASE AND s.name = ?1 COLLATE BINARY
       AND f.lang = 'rust' AND substr(c.path, 1, length(?2)) = ?2 COLLATE NOCASE
     ORDER BY c.id";

fn error(error: impl std::fmt::Display) -> LoreError {
    LoreError::new(format!("atlas: {error}"))
}

fn sha1_hex(text: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// A C++/Rust qualified name as written: `UInventoryWeapon::Equip`. The word
/// pattern splits on `::`, so the query's qualified names are read here.
fn qualified_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z_]\w*(?:::[A-Za-z_]\w*)+").expect("the qualified name pattern is valid"))
}

fn join_qualifiers(name: &str) -> String {
    static SEPARATOR: OnceLock<Regex> = OnceLock::new();
    SEPARATOR.get_or_init(|| Regex::new(r"\s*::\s*").expect("qualified separator")).replace_all(name, "::").into_owned()
}

/// The name the symbol table is asked for: `::` stays whole (`Class::method`);
/// a dot name (`A.B`) keeps its old meaning, its last part, since C#/Java
/// symbols are stored short.
fn lookup_name(name: &str) -> &str {
    if name.contains("::") {
        name
    } else {
        name.rsplit('.').next().unwrap_or(name)
    }
}

/// The module part of a Rust path: `crate::render::post::chain` -> `render/post`.
/// `crate` is the root; `self`/`super` need a caller's file, so they confirm
/// nothing here. An empty qualifier means the crate root (`crate::chain`).
fn rust_qualifier(name: &str) -> Vec<String> {
    let parts: Vec<&str> = name.split("::").filter(|part| !part.is_empty()).collect();
    if parts.len() < 2 {
        return Vec::new();
    }
    let skip = usize::from(parts[0] == "crate");
    parts[skip..parts.len() - 1].iter().map(|part| (*part).to_owned()).collect()
}

/// A Rust file's module path: `src/render/mod.rs` -> `render`,
/// `src/render/post.rs` -> `render/post`, `src/lib.rs` -> empty.
fn rust_module(path: &str) -> Vec<String> {
    let path = path.replace('\\', "/");
    let path = path.strip_prefix("src/").unwrap_or(&path);
    let path = path.strip_suffix(".rs").unwrap_or(path);
    let mut parts: Vec<String> = path.split('/').filter(|part| !part.is_empty()).map(str::to_owned).collect();
    if parts.last().map(String::as_str) == Some("mod") {
        parts.pop();
    } else if matches!(parts.last().map(String::as_str), Some("lib") | Some("main")) {
        parts.pop();
    }
    parts
}

/// Whether `path` is the Rust module the qualified path names.
fn rust_module_matches(path: &str, qualifier: &[String]) -> bool {
    let module = rust_module(path);
    if qualifier.is_empty() {
        return module.is_empty();
    }
    module.len() >= qualifier.len() && module[module.len() - qualifier.len()..] == qualifier[..]
}

pub struct Atlas {
    pub db: Connection,
    pub path: PathBuf,
    root: PathBuf,
    _writer: Option<File>,
    has_projections: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refresh {
    pub files_read: usize,
    pub files_dropped: usize,
    pub chunks: usize,
    pub edges: usize,
    pub rebuilt: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub chunk: i64,
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub symbols: Vec<String>,
    pub lang: String,
    pub score: f64,
    pub via: Vec<String>,
    pub text: String,
}

pub(super) struct Candidate {
    pub hit: Hit,
    pub exact: bool,
}

struct Ranked {
    id: i64,
    score: f64,
    via: Vec<String>,
    exact: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Definition {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: usize,
}

fn create(db: &Connection) -> Result<(), LoreError> {
    db.execute_batch(&format!(
        r#"CREATE TABLE files(path TEXT PRIMARY KEY, blob TEXT, lang TEXT, lines INTEGER);
        CREATE TABLE chunks(id INTEGER PRIMARY KEY, path TEXT, start INTEGER, stop INTEGER, symbols TEXT, ehash TEXT, body TEXT);
        CREATE INDEX chunks_path ON chunks(path);
        CREATE TABLE symbols(name TEXT, kind TEXT, path TEXT, line INTEGER);
        CREATE INDEX symbols_name ON symbols(name COLLATE NOCASE);
        CREATE INDEX symbols_path ON symbols(path);
        CREATE TABLE imports(path TEXT, spec TEXT, line INTEGER);
        CREATE INDEX imports_path ON imports(path);
        CREATE TABLE edges(src TEXT, dst TEXT, kind TEXT, PRIMARY KEY(src, dst));
        CREATE INDEX edges_dst ON edges(dst);
        CREATE VIRTUAL TABLE code_words USING fts5(chunk UNINDEXED, path, names, body,
            tokenize = "porter unicode61 remove_diacritics 2 tokenchars '_'");
        CREATE VIRTUAL TABLE code_terms USING fts5(chunk UNINDEXED, toks,
            tokenize = "unicode61 remove_diacritics 0 tokenchars '_./:-'");
        CREATE TABLE vectors(model TEXT, hash TEXT, vec BLOB, PRIMARY KEY(model, hash));
        CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
        INSERT INTO meta VALUES('fts_rowid', 'chunk');
        PRAGMA user_version = {ATLAS_VERSION};"#
    ))
    .map_err(error)?;
    derived::install(db)
}

/// The text tables' rows carry their piece's id as their rowid, so a file's
/// rows are dropped by rowid; a map made before that has its rows renumbered
/// once, in place (its pieces and vectors are kept). A drop by the unindexed
/// `chunk` column read the whole table, twice per changed file.
fn rowid_is_chunk(db: &Connection) -> Result<(), LoreError> {
    let done = |db: &Connection| -> Result<bool, LoreError> {
        let value: Option<String> = db.query_row("SELECT value FROM meta WHERE key = 'fts_rowid'", [], |row| row.get(0)).optional().map_err(error)?;
        Ok(value.is_some())
    };
    if done(db)? {
        return Ok(());
    }
    // Under the write lock, looked at again: a process that waited for
    // another one's renumbering finds it done and does it no second time.
    // A failure part way rolls back (the transaction is dropped).
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate).map_err(error)?;
    if done(&tx)? {
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TEMP TABLE words_copy AS SELECT chunk, path, names, body FROM code_words;
         CREATE TEMP TABLE terms_copy AS SELECT chunk, toks FROM code_terms;
         DELETE FROM code_words; DELETE FROM code_terms;
         INSERT INTO code_words(rowid, chunk, path, names, body) SELECT chunk, chunk, path, names, body FROM words_copy;
         INSERT INTO code_terms(rowid, chunk, toks) SELECT chunk, chunk, toks FROM terms_copy;
         DROP TABLE words_copy; DROP TABLE terms_copy;
         INSERT OR REPLACE INTO meta VALUES('fts_rowid', 'chunk');",
    )
    .map_err(error)?;
    tx.commit().map_err(error)
}

fn connect(path: &Path) -> Result<Connection, LoreError> {
    let open = |path: &Path| -> Result<Connection, LoreError> {
        let db = Connection::open(path).map_err(|e| LoreError::new(format!("{}: {e}", path.display())))?;
        db.busy_timeout(Duration::from_secs(120)).map_err(error)?;
        db.execute_batch("PRAGMA journal_mode=WAL").map_err(error)?;
        Ok(db)
    };
    let db = open(path)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(error)?;
    if version == ATLAS_VERSION {
        rowid_is_chunk(&db)?;
        if !derived::ready(&db)? {
            let tx = db.unchecked_transaction().map_err(error)?;
            derived::install(&tx)?;
            tx.commit().map_err(error)?;
        }
        derived::install_ann_clock(&db)?;
        return Ok(db);
    }
    let db = if version == 0 {
        db
    } else {
        drop(db);
        remove_file_set(path)?;
        open(path)?
    };
    let tx = db.unchecked_transaction().map_err(error)?;
    create(&tx)?;
    derived::install_ann_clock(&tx)?;
    tx.commit().map_err(error)?;
    Ok(db)
}

fn remove_file_set(path: &Path) -> Result<(), LoreError> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        if file.exists() {
            std::fs::remove_file(&file).map_err(|e| LoreError::new(format!("{}: {e}", file.display())))?;
        }
    }
    Ok(())
}

/// Cache locations a snapshot reader can find without starting Git.
pub(super) fn project_dirs(root: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    std::iter::once(cache_dir().join(repo_key(root)))
        .chain(std::iter::once_with(|| scan::git_dir(root)).filter(|dir| dir.is_dir()).map(|dir| dir.join("codelore")))
        .chain(private_temp().map(|dir| dir.join("codelore").join(repo_key(root))))
}

pub(super) fn lock_file(dir: &Path, name: &str) -> Result<File, LoreError> {
    let path = dir.join(name);
    OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path)
        .map_err(|e| error(format!("{}: {e}", path.display())))
}

pub(super) fn is_locked(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

/// Select a directory by its ability to host the same writer lock used by
/// manual, synchronous and background refreshes.
pub(super) fn writer_location(root: &Path, preferred: Option<&Path>) -> Result<(PathBuf, File), LoreError> {
    let open = |dir: PathBuf| {
        fs::create_dir_all(&dir).map_err(|e| error(format!("{}: {e}", dir.display())))?;
        let lock = lock_file(&dir, "atlas-writer.lock")?;
        Ok((dir, lock))
    };
    if let Some(dir) = preferred {
        return open(dir.to_path_buf());
    }
    let mut tried = Vec::new();
    for dir in project_dirs(root) {
        match open(dir) {
            Ok(found) => return Ok(found),
            Err(e) => tried.push(e.to_string()),
        }
    }
    Err(error(format!("no writable cache directory; set LORE_CACHE ({})", tried.join("; "))))
}

/// Open for writing with the project lock. A busy writer is not a reason to
/// select a different cache directory.
pub(super) fn writer(config: &Config, preferred: Option<&Path>, wait: Duration) -> Result<Option<Atlas>, LoreError> {
    let (dir, lock) = writer_location(&config.root, preferred)?;
    let started = std::time::Instant::now();
    loop {
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => break,
            Err(e) if is_locked(&e) && started.elapsed() < wait => std::thread::sleep(Duration::from_millis(25)),
            Err(e) if is_locked(&e) => return Ok(None),
            Err(e) => return Err(error(e)),
        }
    }
    let path = dir.join(FILE_NAME);
    let db = connect(&path)?;
    Ok(Some(Atlas { db, path, root: config.root.clone(), _writer: Some(lock), has_projections: true }))
}

/// Open one committed revision without any schema changes, refreshes or waits
/// on a writer. The first SELECT establishes the read transaction's snapshot.
pub(super) fn open_snapshot(config: &Config, preferred: Option<&Path>) -> Result<Option<Atlas>, LoreError> {
    let open = |dir: &Path| {
        let path = dir.join(FILE_NAME);
        if !path.try_exists().map_err(error)? {
            return Ok(None);
        }
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| error(format!("{}: {e}", path.display())))?;
        db.busy_timeout(Duration::ZERO).map_err(error)?;
        db.execute_batch("BEGIN").map_err(error)?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(error)?;
        if version == 0 {
            return Ok(None);
        }
        if version != ATLAS_VERSION {
            return Err(error("incompatible project snapshot; run lore atlas index"));
        }
        let ready = db.query_row("SELECT 1 FROM meta WHERE key = 'graph'", [], |row| row.get::<_, i64>(0)).optional().map_err(error)?;
        let has_projections = derived::ready(&db)?;
        Ok(ready.map(|_| Atlas { db, path, root: config.root.clone(), _writer: None, has_projections }))
    };
    if let Some(dir) = preferred {
        return open(dir);
    }
    let mut issue = None;
    for dir in project_dirs(&config.root) {
        match open(&dir) {
            Ok(Some(atlas)) => return Ok(Some(atlas)),
            Ok(None) => {}
            Err(e) => { issue.get_or_insert(e); }
        }
    }
    issue.map_or(Ok(None), Err)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshKind {
    Quick,
    Reconcile,
    Rebuild,
}

impl Atlas {
    pub(in crate::atlas) fn open_ann_snapshot(path: &Path, root: &Path, publish: bool) -> Result<Option<Self>, LoreError> {
        if !path.try_exists().map_err(error)? {
            return Ok(None);
        }
        let flags = if publish { OpenFlags::SQLITE_OPEN_READ_WRITE } else { OpenFlags::SQLITE_OPEN_READ_ONLY };
        let db = Connection::open_with_flags(path, flags).map_err(error)?;
        db.busy_timeout(if publish { Duration::from_secs(2) } else { Duration::ZERO }).map_err(error)?;
        db.execute_batch(if publish { "BEGIN IMMEDIATE" } else { "BEGIN" }).map_err(error)?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(error)?;
        if version != ATLAS_VERSION || !derived::ready(&db)? {
            return Ok(None);
        }
        Ok(Some(Self { db, path: path.to_path_buf(), root: root.to_path_buf(), _writer: None, has_projections: true }))
    }

    pub(in crate::atlas) fn install_ann_clock(&self) -> Result<(), LoreError> {
        derived::install_ann_clock(&self.db)
    }

    pub(in crate::atlas) fn ann_root(&self) -> &Path {
        &self.root
    }

    /// Opens a map file at `path` directly (tests).
    pub fn open_at(path: &Path, root: &Path) -> Result<Self, LoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(error)?;
        }
        let db = connect(path)?;
        Ok(Self { db, path: path.to_path_buf(), root: root.to_path_buf(), _writer: None, has_projections: true })
    }

    /// How long a write waits for another process's lock before it gives up.
    pub fn wait_at_most(&self, limit: Duration) {
        let _ = self.db.busy_timeout(limit);
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.db.query_row("SELECT value FROM meta WHERE key=?1", [key], |row| row.get(0)).optional().ok().flatten()
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<(), LoreError> {
        self.db.execute("INSERT OR REPLACE INTO meta VALUES(?1, ?2)", params![key, value]).map_err(error)?;
        Ok(())
    }

    /// Brings the map up to date with the working tree.
    pub fn refresh(&mut self, config: &Config) -> Result<Refresh, LoreError> {
        self.refresh_with(config, RefreshKind::Quick, || true)
    }

    pub(super) fn refresh_with(&mut self, config: &Config, kind: RefreshKind, active: impl Fn() -> bool) -> Result<Refresh, LoreError> {
        // An exclude file's text is a setting too: an edit to it re-scans.
        let checkout = config.atlas.checkout.as_deref().unwrap_or(&config.root);
        let exclude_files: Vec<String> = config.atlas.exclude_files.iter().map(|file| std::fs::read_to_string(checkout.join(file)).unwrap_or_default()).collect();
        let settings = format!("{:?}|{:?}|{:?}|{}|{:?}|{:?}",
            config.dir, config.atlas.include, config.atlas.exclude,
            config.atlas.max_kib, config.atlas.exclude_files, exclude_files);
        // The quick print over the folders the last scan saw: when it is the same,
        // nothing a scan could find has changed. It is taken before the scan, so
        // an edit made while scanning shows up next time.
        let dirs: BTreeSet<String> = self.meta("dirs").map(|text| text.split('\n').map(str::to_owned).collect()).unwrap_or_default();
        let untracked: Vec<String> = self.meta("untracked").map(|text| text.split('\n').filter(|path| !path.is_empty()).map(str::to_owned).collect()).unwrap_or_default();
        let print = if dirs.is_empty() || kind != RefreshKind::Quick || config.atlas.checkout.is_some() {
            String::new()
        } else {
            scan::quick_print(&config.root, &settings, &dirs, &untracked)?
        };
        if !print.is_empty() && self.meta("fingerprint").as_deref() == Some(print.as_str()) && self.meta("graph").is_some() {
            return Ok(Refresh::default());
        }
        let changes = scan::changes(config)?;
        let (wanted, seen_dirs) = scan::scan_with(config, &changes)?;
        let untracked_paths: BTreeSet<&str> = changes.untracked.iter().map(String::as_str).collect();
        // The untracked files this scan saw: the next print takes their stamps.
        // A different list than the print was taken over means no print yet.
        let print = if changes.untracked == untracked { print } else { String::new() };
        // New folders: no print yet (one taken now could postdate the scan); the
        // next call takes one over these folders and scans once more.
        let print = if dirs == seen_dirs { print } else { String::new() };
        let mut known: HashMap<String, String> = HashMap::new();
        {
            let mut statement = self.db.prepare("SELECT path, blob FROM files").map_err(error)?;
            let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
            for row in rows {
                let (path, blob) = row.map_err(error)?;
                known.insert(path, blob);
            }
        }
        let rebuild = kind == RefreshKind::Rebuild;
        let mut info = Refresh { rebuilt: rebuild || known.is_empty() && !wanted.is_empty(), ..Refresh::default() };
        let gone: Vec<String> = known.keys().filter(|path| !wanted.contains_key(*path)).cloned().collect();
        let changed: Vec<&Entry> = wanted.values().filter(|entry| rebuild || known.get(&entry.path) != Some(&entry.blob)).collect();
        let graph_changed = rebuild || !gone.is_empty() || !changed.is_empty()
            || self.meta("graph").is_none() || self.meta("settings").as_deref() != Some(settings.as_str());
        let check_active = || {
            if active() { Ok(()) } else { Err(error("background refresh stopped")) }
        };
        check_active()?;
        let tx = self.db.transaction().map_err(error)?;
        for path in gone.iter().chain(changed.iter().map(|entry| &entry.path)) {
            drop_file(&tx, path)?;
        }
        info.files_dropped = gone.len();
        for entry in &changed {
            check_active()?;
            let path = self.root.join(&entry.path);
            let bytes = scan::read_source(&self.root, &entry.path, config.atlas.max_kib * 1024)
                .map_err(|e| error(format!("{}: {e}", path.display())))?;
            let dirty = changes.modified.contains(&entry.path) || untracked_paths.contains(entry.path.as_str());
            if dirty && scan::blob_id(&bytes) != entry.blob {
                return Err(error(format!("{} changed while indexing; retry the refresh", entry.path)));
            }
            let Some(text) = scan::text_from_bytes(&bytes) else {
                // Binary: remembered by its blob so it is not read again.
                tx.execute("INSERT INTO files VALUES(?1, ?2, ?3, 0)", params![entry.path, entry.blob, "binary"]).map_err(error)?;
                continue;
            };
            info.files_read += 1;
            info.chunks += put_file(&tx, entry, &text)?;
        }
        if graph_changed {
            info.edges = rebuild_edges(&tx)?;
        }
        let now = chrono::Utc::now().to_rfc3339();
        if graph_changed {
            tx.execute(
                "INSERT OR REPLACE INTO meta VALUES ('graph', ?1), ('settings', ?2), ('refreshed', ?3),
                 ('project_revision', 1 + coalesce((SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'project_revision'), 0))",
                params![info.edges.to_string(), settings, now],
            ).map_err(error)?;
        }
        tx.execute("INSERT OR REPLACE INTO meta VALUES('fingerprint', ?1), ('untracked', ?2), ('dirs', ?3), ('checked', ?4)",
            params![print, changes.untracked.join("\n"), seen_dirs.into_iter().collect::<Vec<_>>().join("\n"), now]).map_err(error)?;
        check_active()?;
        tx.commit().map_err(error)?;
        Ok(info)
    }

    pub fn counts(&self) -> Result<BTreeMap<&'static str, i64>, LoreError> {
        let mut out = BTreeMap::new();
        if self.has_projections {
            let mut read = self.db.prepare("SELECT total FROM atlas_counts WHERE name = ?1").map_err(error)?;
            for (name, _, _) in derived::COUNTS {
                out.insert(name, read.query_row([name], |row| row.get(0)).map_err(error)?);
            }
        } else {
            for (name, table, predicate) in derived::COUNTS {
                let filter = if predicate.is_empty() { String::new() } else { format!(" WHERE {predicate}") };
                let sql = format!("SELECT count(*) FROM {table}{filter}");
                out.insert(name, self.db.query_row(&sql, [], |row| row.get(0)).map_err(error)?);
            }
        }
        Ok(out)
    }

    /// Pieces that have no vector under `model`.
    pub fn missing_vectors(&self, model: &str) -> Result<Vec<(String, String)>, LoreError> {
        let mut statement = self
            .db
            .prepare("SELECT DISTINCT c.ehash, c.path, c.symbols, c.body FROM chunks c LEFT JOIN vectors v ON v.hash = c.ehash AND v.model = ?1 WHERE v.hash IS NULL")
            .map_err(error)?;
        let rows = statement
            .query_map([model], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)))
            .map_err(error)?;
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for row in rows {
            let (hash, path, symbols, body) = row.map_err(error)?;
            if seen.insert(hash.clone()) {
                out.push((hash, embed_text(&path, &symbols, &body)));
            }
        }
        Ok(out)
    }

    /// Keep a project vector fill invisible to snapshot readers until the
    /// full fill succeeds. The shared cache can still retain generated batches.
    pub(super) fn vector_batch<T>(&self, write: impl FnOnce(&Connection) -> Result<T, LoreError>) -> Result<T, LoreError> {
        if !self.db.is_autocommit() {
            return write(&self.db);
        }
        let tx = self.db.unchecked_transaction().map_err(error)?;
        let result = write(&tx)?;
        tx.commit().map_err(error)?;
        Ok(result)
    }

    /// Embeds up to `budget` pieces without a vector; returns (embedded, still missing).
    pub fn ensure_vectors(&self, embedder: &dyn Embeddings, model: &str, doc_prefix: &str, budget: Option<usize>, progress: &mut dyn FnMut(usize, usize)) -> Result<(usize, usize), LoreError> {
        let dimension = embedder.dimensions().to_string();
        if self.meta("vector_model").as_deref() != Some(&format!("{model}/{dimension}")) {
            self.db.execute("DELETE FROM vectors", []).map_err(error)?;
            self.set_meta("vector_model", &format!("{model}/{dimension}"))?;
        }
        let mut todo = self.missing_vectors(model)?;
        // Vectors another clone (or an earlier map of this one) already made
        // are taken from the shared cache rather than made again: a piece's
        // hash covers its path, symbols and text, so a clone of the same tree
        // finds nearly all of them there.
        let shared_key = format!("{model}/{dimension}/t{}", crate::embed::token_cap(usize::MAX));
        let shared = SharedVectors::open(&shared_key);
        if let Some(shared) = &shared {
            // A map's own vectors go to the shared cache once, so the clones
            // after it start from them.
            if self.meta("shared_vectors").as_deref() != Some(shared_key.as_str()) {
                let mut statement = self.db.prepare("SELECT hash, vec FROM vectors WHERE model = ?1").map_err(error)?;
                let rows = statement.query_map([model], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))).map_err(error)?;
                let mine: Vec<(String, Vec<u8>)> = rows.collect::<Result<_, _>>().map_err(error)?;
                drop(statement);
                shared.put(&mine.iter().map(|(hash, blob)| (hash.as_str(), blob.clone())).collect::<Vec<_>>());
                self.set_meta("shared_vectors", &shared_key)?;
            }
            let found = shared.take(&todo);
            if !found.is_empty() {
                self.vector_batch(|db| {
                    for (hash, blob) in &found {
                        db.execute("INSERT OR REPLACE INTO vectors VALUES(?1, ?2, ?3)", params![model, hash, blob]).map_err(error)?;
                    }
                    Ok(())
                })?;
                todo.retain(|(hash, _)| !found.contains_key(hash));
            }
        }
        let take = budget.map_or(todo.len(), |budget| budget.min(todo.len()));
        if budget.is_some() && todo.len() > take {
            return Ok((0, todo.len()));
        }
        let mut done = 0;
        for batch in todo[..take].chunks(64) {
            let texts: Vec<String> = batch.iter().map(|(_, text)| format!("{doc_prefix}{text}")).collect();
            let vectors = embedder.embed(&texts)?;
            let made = self.vector_batch(|db| {
                let mut made = Vec::with_capacity(batch.len());
                for ((hash, _), vector) in batch.iter().zip(vectors) {
                    let blob: Vec<u8> = vector.iter().flat_map(|value| value.to_le_bytes()).collect();
                    db.execute("INSERT OR REPLACE INTO vectors VALUES(?1, ?2, ?3)", params![model, hash, blob]).map_err(error)?;
                    made.push((hash.as_str(), blob));
                }
                Ok(made)
            })?;
            if let Some(shared) = &shared {
                shared.put(&made);
            }
            done += batch.len();
            progress(done, take);
        }
        Ok((done, todo.len() - done))
    }

    pub fn vector_count(&self, model: &str) -> i64 {
        let sql = if self.has_projections {
            "SELECT coalesce(sum(entries), 0) FROM atlas_vector_counts WHERE model = ?1"
        } else {
            "SELECT count(*) FROM vectors WHERE model = ?1"
        };
        self.db.query_row(sql, [model], |row| row.get(0)).unwrap_or(0)
    }

    pub(in crate::atlas) fn prepare_filter<'a>(&self, filter: &'a Filter, timings: &mut SearchTimings) -> PreparedFilter<'a> {
        let rowid_is_chunk = (!filter.path.is_empty() || !filter.lang.is_empty())
            && self.meta("fts_rowid").as_deref() == Some("chunk");
        let started = std::time::Instant::now();
        // Materialization is shared only while the SQLite snapshot is pinned
        // or this caller holds the project's writer lock.
        let selected = if rowid_is_chunk && !filter.path.is_empty() && (!self.db.is_autocommit() || self._writer.is_some()) {
            self.db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS atlas_search_ids(id INTEGER PRIMARY KEY);
                DELETE FROM temp.atlas_search_ids")
                .and_then(|_| self.db.execute(FILTER_IDS, params![filter.path, filter.lang, FILTER_LIMIT + 1]))
                .ok().filter(|size| *size <= FILTER_LIMIT)
        } else {
            None
        };
        timings.filter_prepare = started.elapsed();
        timings.prefilter_rows = selected;
        PreparedFilter { filter, rowid_is_chunk, selected }
    }

    fn fts(&self, table: &str, query: &str, selection: &PreparedFilter<'_>) -> Vec<i64> {
        if query.is_empty() {
            return Vec::new();
        }
        let filter = selection.filter;
        let rank = if table == "code_words" { format!("bm25({table}, 0, 4.0, 8.0, 1.0)") } else { format!("bm25({table})") };
        // Without a path or language filter the FTS table alone ranks and cuts:
        // joining every match to chunks and files first cost most of a search.
        let sql = if selection.selected.is_some() {
            format!("SELECT t.rowid FROM {table} t CROSS JOIN temp.atlas_search_ids eligible ON eligible.id=t.rowid
                WHERE {table} MATCH ?1 AND ?2 IS NOT NULL AND ?3 IS NOT NULL ORDER BY {rank}, t.rowid LIMIT ?4")
        } else if filter.path.is_empty() && filter.lang.is_empty() {
            format!("SELECT chunk FROM {table} WHERE {table} MATCH ?1 AND ?2 = '' AND ?3 = '' ORDER BY {rank}, rowid LIMIT ?4")
        } else {
            let id = if selection.rowid_is_chunk { "t.rowid" } else { "t.chunk" };
            format!(
                "SELECT {id} FROM {table} t JOIN chunks c ON c.id = {id} JOIN files f ON f.path = c.path WHERE {table} MATCH ?1 AND substr(c.path, 1, length(?2)) = ?2 COLLATE NOCASE AND (?3 = '' OR f.lang = ?3) ORDER BY {rank}, t.rowid LIMIT ?4"
            )
        };
        let run = || -> rusqlite::Result<Vec<i64>> {
            let mut statement = self.db.prepare(&sql)?;
            let rows = statement.query_map(params![query, filter.path, filter.lang, DEPTH as i64], |row| row.get::<_, i64>(0))?;
            rows.collect()
        };
        run().unwrap_or_default()
    }

    fn exact_meaning(&self, query_vector: &[f32], model: &str, filter: &Filter) -> Result<Vec<i64>, LoreError> {
        if query_vector.is_empty() || query_vector.iter().any(|value| !value.is_finite()) {
            return Ok(Vec::new());
        }
        let mut statement = self
            .db
            .prepare("SELECT c.id, v.vec FROM chunks c CROSS JOIN vectors v ON v.hash = c.ehash AND v.model = ?1 CROSS JOIN files f ON f.path = c.path WHERE substr(c.path, 1, length(?2)) = ?2 COLLATE NOCASE AND (?3 = '' OR f.lang = ?3) AND length(v.vec) = ?4 AND typeof(v.vec) = 'blob' LIMIT ?5")
            .map_err(error)?;
        let rows = statement.query_map(params![model, filter.path, filter.lang, query_vector.len() * 4, semantic::EXACT_LIMIT], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))).map_err(error)?;
        let mut scored = Vec::new();
        for row in rows {
            let (id, blob) = row.map_err(error)?;
            if let Some(score) = semantic::score(&blob, query_vector) {
                scored.push((id, score));
            }
        }
        Ok(semantic::top(scored))
    }

    /// Ranked pieces for a query: words and whole terms by BM25, the pieces that
    /// define a symbol the query names, and meaning when a vector is given -
    /// fused by reciprocal rank. At most two pieces of one file.
    pub fn search(&self, query: &str, limit: usize, filter: &Filter, meaning: Option<(&[f32], &str)>) -> Result<Vec<Hit>, LoreError> {
        let mut timings = SearchTimings::default();
        let selection = self.prepare_filter(filter, &mut timings);
        let meaning = match meaning {
            Some((vector, model)) if !selection.is_empty() && self.small_enough_for_exact()? => self.exact_meaning(vector, model, filter)?,
            _ => Vec::new(),
        };
        self.search_with_meaning(query, limit, &selection, meaning, &mut timings)
    }

    pub(super) fn search_with_meaning(&self, query: &str, limit: usize, filter: &PreparedFilter<'_>, meaning: Vec<i64>, timings: &mut SearchTimings) -> Result<Vec<Hit>, LoreError> {
        let ranked = self.ranked(query, filter, meaning, false, timings)?;
        let mut per_file: HashMap<String, usize> = HashMap::new();
        let mut out = Vec::new();
        for rank in ranked {
            let Some(mut hit) = self.chunk(rank.id)? else { continue };
            let count = per_file.entry(hit.path.clone()).or_default();
            if *count >= 2 {
                continue;
            }
            *count += 1;
            hit.score = (rank.score * 100_000.0).round() / 100_000.0;
            hit.via = rank.via;
            out.push(hit);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub(super) fn candidates(&self, query: &str, filter: &PreparedFilter<'_>, meaning: Vec<i64>, timings: &mut SearchTimings) -> Result<Vec<Candidate>, LoreError> {
        let mut out = Vec::new();
        for rank in self.ranked(query, filter, meaning, true, timings)? {
            if let Some(hit) = self.chunk(rank.id)? {
                out.push(Candidate { hit: Hit { score: rank.score, via: rank.via, ..hit }, exact: rank.exact });
            }
        }
        Ok(out)
    }

    fn ranked(&self, query: &str, filter: &PreparedFilter<'_>, meaning: Vec<i64>, strict_cpp: bool, timings: &mut SearchTimings) -> Result<Vec<Ranked>, LoreError> {
        if filter.is_empty() {
            return Ok(Vec::new());
        }
        let words = word_query(query);
        let terms = crate::finder::term_query(query);
        let (exact, short) = self.defining(query, filter.filter, strict_cpp)?;
        let exact_ids: BTreeSet<_> = exact.iter().copied().collect();
        let fts_started = std::time::Instant::now();
        let mut lists: Vec<(&str, Vec<i64>, f64)> = vec![
            ("words", self.fts("code_words", &words, filter), 1.0),
            ("terms", self.fts("code_terms", &terms, filter), 1.0),
        ];
        timings.fts = fts_started.elapsed();
        // A query that names a symbol whole outranks pieces that merely share
        // its short name: the exact definitions are their own, heavier list.
        if !exact.is_empty() {
            lists.push(("symbol", exact, 4.0));
        }
        if !short.is_empty() {
            lists.push(("symbol", short, 2.0));
        }
        if !meaning.is_empty() {
            lists.push(("meaning", meaning, 1.0));
        }
        let mut scores: HashMap<i64, (f64, Vec<String>)> = HashMap::new();
        let mut order: Vec<i64> = Vec::new();
        for (name, ids, weight) in &lists {
            for (rank, id) in ids.iter().enumerate() {
                let entry = scores.entry(*id).or_insert_with(|| {
                    order.push(*id);
                    (0.0, Vec::new())
                });
                entry.0 += *weight / (RRF_K + rank as f64 + 1.0);
                if !entry.1.contains(&(*name).to_owned()) {
                    entry.1.push((*name).to_owned());
                }
            }
        }
        order.sort_by(|a, b| scores[b].0.partial_cmp(&scores[a].0).unwrap_or(std::cmp::Ordering::Equal));
        Ok(order.into_iter().map(|id| {
            let (score, via) = scores.remove(&id).expect("a ranked chunk has a score");
            Ranked { id, score, via, exact: exact_ids.contains(&id) }
        }).collect())
    }

    /// The pieces whose definitions carry a name the query says whole: the
    /// exact qualified names first, then the other names by their short form.
    /// A qualified name never falls back to a same-named method of another
    /// class or namespace.
    fn defining(&self, query: &str, filter: &Filter, strict_cpp: bool) -> Result<(Vec<i64>, Vec<i64>), LoreError> {
        let joined = strict_cpp.then(|| join_qualifiers(query));
        let query = joined.as_deref().unwrap_or(query);
        // Only what reads as an identifier - `open_index`, `ChunkStore`, `a::b` - not
        // an English word that some function happens to be called.
        let looks_like_code = |w: &str| w.len() >= 3 && (w.contains('_') || w.contains("::") || w.contains('.') || w.chars().skip(1).any(char::is_uppercase));
        let mut qualified: Vec<String> = Vec::new();
        for found in qualified_re().find_iter(query) {
            let term = found.as_str().to_owned();
            if looks_like_code(&term) && !qualified.iter().any(|name| name.eq_ignore_ascii_case(&term)) {
                qualified.push(term);
            }
        }
        // The words of `Alpha::Sword::Equip` are not names in their own right:
        // taken short, `Equip` would boost every other class's Equip too.
        let segments: BTreeSet<String> = qualified.iter().flat_map(|name| name.split("::")).filter(|part| part.len() >= 2).map(|part| part.to_lowercase()).collect();
        let mut short: Vec<String> = Vec::new();
        for word in word_re().find_iter(query).map(|m| m.as_str().to_owned()).chain(terms_of(query)) {
            if !looks_like_code(&word) || segments.contains(&word.to_lowercase()) || qualified.iter().any(|name| name.eq_ignore_ascii_case(&word)) {
                continue;
            }
            if !short.iter().any(|name| name.eq_ignore_ascii_case(&word)) {
                short.push(word);
            }
        }
        let mut exact_ids = Vec::new();
        let mut short_ids = Vec::new();
        for name in qualified.iter().take(12) {
            let ids = self.symbol_chunks(name, filter, true, strict_cpp)?;
            if ids.is_empty() {
                // Rust stores `render::chain` as `chain`; the Rust file's
                // module path must confirm `render`. C++ files are never
                // candidates here, and `--lang cpp` excludes them outright.
                if filter.lang.is_empty() || filter.lang == "rust" {
                    for id in self.rust_symbol_chunks(name, filter)? {
                        if !short_ids.contains(&id) {
                            short_ids.push(id);
                        }
                    }
                }
                continue;
            }
            for id in ids {
                if !exact_ids.contains(&id) {
                    exact_ids.push(id);
                }
            }
        }
        for name in short.iter().take(12) {
            for id in self.symbol_chunks(name, filter, false, false)? {
                if !exact_ids.contains(&id) && !short_ids.contains(&id) {
                    short_ids.push(id);
                }
            }
        }
        Ok((exact_ids, short_ids))
    }

    /// Pieces holding the definition line of `name`. `exact` matches the whole
    /// stored name; otherwise a short name matches `Class::name` too, any class.
    fn symbol_chunks(&self, name: &str, filter: &Filter, exact: bool, strict_cpp: bool) -> Result<Vec<i64>, LoreError> {
        let name = lookup_name(name);
        let sql = if exact {
            "SELECT DISTINCT c.id FROM symbols s JOIN chunks c ON c.path = s.path AND s.line BETWEEN c.start AND c.stop JOIN files f ON f.path = c.path WHERE s.name = ?1 COLLATE NOCASE AND substr(c.path, 1, length(?2)) = ?2 COLLATE NOCASE AND (?3 = '' OR f.lang = ?3) AND (?4 = 0 OR f.lang NOT IN ('c', 'cpp') OR s.name = ?1 COLLATE BINARY) ORDER BY c.id LIMIT 8".to_owned()
        } else {
            let name_match = self.short_match();
            format!("SELECT c.id FROM symbols s CROSS JOIN chunks c ON c.path = s.path AND s.line BETWEEN c.start AND c.stop CROSS JOIN files f ON f.path = c.path WHERE ({name_match}) AND substr(c.path, 1, length(?2)) = ?2 COLLATE NOCASE AND (?3 = '' OR f.lang = ?3) AND ?4 = 0 GROUP BY c.id ORDER BY MAX(s.name = ?1 COLLATE NOCASE) DESC, c.id LIMIT 8")
        };
        let mut statement = self.db.prepare(&sql).map_err(error)?;
        let rows = statement.query_map(params![name, filter.path, filter.lang, strict_cpp], |row| row.get::<_, i64>(0)).map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    fn short_match(&self) -> &'static str {
        if self.has_projections { INDEXED_SHORT_MATCH } else { LEGACY_SHORT_MATCH }
    }

    /// A Rust path's definition chunks, confirmed by the file's module path:
    /// `render::chain` may match `src/render.rs` or `src/render/mod.rs`, never
    /// `src/render/post.rs`. `s.name` is exact (Rust is case-sensitive).
    fn rust_symbol_chunks(&self, name: &str, filter: &Filter) -> Result<Vec<i64>, LoreError> {
        let short = name.rsplit("::").next().unwrap_or(name);
        let qualifier = rust_qualifier(name);
        // No SQL LIMIT: the limit applies to candidates that pass the module check.
        let mut statement = self
            .db
            .prepare(RUST_SYMBOL_CHUNKS)
            .map_err(error)?;
        let rows = statement.query_map(params![short, filter.path], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, path) = row.map_err(error)?;
            if rust_module_matches(&path, &qualifier) && !out.contains(&id) {
                out.push(id);
                if out.len() >= 8 {
                    break;
                }
            }
        }
        Ok(out)
    }

    pub fn chunk(&self, id: i64) -> Result<Option<Hit>, LoreError> {
        self.db
            .query_row(
                "SELECT c.path, c.start, c.stop, c.symbols, f.lang, c.body FROM chunks c JOIN files f ON f.path = c.path WHERE c.id = ?1",
                [id],
                |row| {
                    Ok(Hit {
                        chunk: id,
                        path: row.get(0)?,
                        start: row.get::<_, i64>(1)? as usize,
                        end: row.get::<_, i64>(2)? as usize,
                        symbols: row.get::<_, String>(3)?.split_whitespace().map(str::to_owned).collect(),
                        lang: row.get(4)?,
                        score: 0.0,
                        via: Vec::new(),
                        text: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(error)
    }

    /// Where `name` is defined. A qualified `Class::method` matches that name
    /// whole; a short `method` matches the exact name and `Class::method` of
    /// every class and namespace. A qualified path with no whole definition
    /// falls back to a Rust file's short definition only - `render::chain`
    /// still finds Rust `chain`, while a missing `Alpha::Sword::Equip` never
    /// selects another class's C++ `Equip`.
    pub fn definitions(&self, name: &str) -> Result<Vec<Definition>, LoreError> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(Vec::new());
        }
        let name = lookup_name(name);
        let rows = self.definition_rows(name)?;
        if rows.is_empty() && name.contains("::") {
            return self.rust_definition_rows(name);
        }
        Ok(rows)
    }

    /// Definitions for `drift`: the same identity as `definitions`. A C++
    /// `Class::method` must match whole; a Rust module path may fall back to a
    /// Rust definition of its last part, so a live Rust reference is not drift.
    pub fn definitions_exact(&self, name: &str) -> Result<Vec<Definition>, LoreError> {
        self.definitions(name)
    }

    pub(super) fn definitions_filtered(&self, name: &str, filter: &Filter) -> Result<Vec<Definition>, LoreError> {
        let name = join_qualifiers(name.trim());
        if name.is_empty() {
            return Ok(Vec::new());
        }
        let qualified = name.contains("::");
        let matches = |definition: &Definition| {
            let lang = lang::detect(&definition.path).unwrap_or(Lang::Text);
            let path = definition.path.as_bytes();
            let prefix = filter.path.as_bytes();
            path.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
                && (filter.lang.is_empty() || filter.lang == lang.name())
                && (!qualified || !matches!(lang, Lang::Cpp | Lang::C) || definition.name == name)
        };
        let mut rows = self.definition_rows(lookup_name(&name))?;
        rows.retain(matches);
        if rows.is_empty() && qualified && (filter.lang.is_empty() || filter.lang == "rust") {
            rows = self.rust_definition_rows(&name)?;
            rows.retain(matches);
        }
        Ok(rows)
    }

    /// A Rust path's last part, confirmed by the Rust file's module path:
    /// `render::chain` -> `chain` in `src/render.rs` or `src/render/mod.rs`,
    /// never `chain` in `src/render/post.rs`. C++ symbols are not candidates.
    fn rust_definition_rows(&self, name: &str) -> Result<Vec<Definition>, LoreError> {
        let short = name.rsplit("::").next().unwrap_or(name);
        let qualifier = rust_qualifier(name);
        let mut statement = self
            .db
            .prepare("SELECT s.name, s.kind, s.path, s.line FROM symbols s JOIN files f ON f.path = s.path WHERE s.name = ?1 AND f.lang = 'rust' ORDER BY s.path, s.line")
            .map_err(error)?;
        let rows = statement
            .query_map(params![short], |row| Ok(Definition { name: row.get(0)?, kind: row.get(1)?, path: row.get(2)?, line: row.get::<_, i64>(3)? as usize }))
            .map_err(error)?;
        let mut out = Vec::new();
        for row in rows {
            let definition = row.map_err(error)?;
            if rust_module_matches(&definition.path, &qualifier) {
                out.push(definition);
            }
        }
        Ok(out)
    }

    fn definition_rows(&self, name: &str) -> Result<Vec<Definition>, LoreError> {
        let sql = if name.contains("::") {
            "SELECT name, kind, path, line FROM symbols WHERE name = ?1 COLLATE NOCASE ORDER BY path, line".to_owned()
        } else {
            let name_match = self.short_match();
            format!("SELECT name, kind, path, line FROM symbols s WHERE ({name_match}) ORDER BY (name = ?1 COLLATE NOCASE) DESC, path, line, rowid")
        };
        let mut statement = self.db.prepare(&sql).map_err(error)?;
        let rows = statement
            .query_map(params![name], |row| Ok(Definition { name: row.get(0)?, kind: row.get(1)?, path: row.get(2)?, line: row.get::<_, i64>(3)? as usize }))
            .map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    /// The symbols of one file, in line order.
    pub fn symbols_of(&self, path: &str) -> Result<Vec<Definition>, LoreError> {
        let mut statement = self.db.prepare("SELECT name, kind, path, line FROM symbols WHERE path = ?1 ORDER BY line").map_err(error)?;
        let rows = statement
            .query_map([path], |row| Ok(Definition { name: row.get(0)?, kind: row.get(1)?, path: row.get(2)?, line: row.get::<_, i64>(3)? as usize }))
            .map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    /// Files whose text names `name`, beside where it is defined. A qualified
    /// `Class::method` must carry the `::` itself: `// Alpha Sword Equip` is a
    /// phrase of stems, not a mention. Another class's method of the same short
    /// name is not a mention either.
    pub fn mentions(&self, name: &str, limit: usize) -> Result<Vec<String>, LoreError> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(Vec::new());
        }
        if name.contains("::") {
            let parts: Vec<String> = name.split("::").filter(|part| !part.is_empty()).map(str::to_owned).collect();
            let phrase = parts.iter().map(|part| part.to_lowercase()).collect::<Vec<_>>().join(" ");
            let chain = parts.iter().map(|part| regex::escape(part)).collect::<Vec<_>>().join(r"\s*::\s*");
            let re = Regex::new(&chain).map_err(error)?;
            let identifier_char = |ch: char| ch.is_alphanumeric() || ch == '_';
            // FTS is the fast prefilter; the limit applies to confirmed
            // matches, so a false candidate cannot hide the right mention.
            let mut statement = self
                .db
                .prepare("SELECT DISTINCT c.path, c.body FROM code_words t JOIN chunks c ON c.id = t.chunk WHERE code_words MATCH ?1")
                .map_err(error)?;
            let query = format!("body : \"{}\"", phrase.replace('"', "\"\""));
            let mut rows = statement.query(params![query]).map_err(error)?;
            let mut out: Vec<String> = Vec::new();
            while let Some(row) = rows.next().map_err(error)? {
                if out.len() >= limit {
                    break;
                }
                let path: String = row.get(0).map_err(error)?;
                let body: String = row.get(1).map_err(error)?;
                let whole_chain = re.find_iter(&body).any(|found| {
                    let before = &body[..found.start()];
                    let after = &body[found.end()..];
                    !before.chars().next_back().is_some_and(identifier_char)
                        && !after.chars().next().is_some_and(identifier_char)
                        && !before.trim_end().ends_with("::")
                        && !after.trim_start().starts_with("::")
                });
                if whole_chain && !out.contains(&path) {
                    out.push(path);
                }
            }
            return Ok(out);
        }
        let short = name.rsplit(['.', ':']).next().unwrap_or(name);
        let query = format!("body : \"{}\"", short.replace('"', "\"\""));
        let mut statement = self
            .db
            .prepare("SELECT DISTINCT c.path FROM code_words t JOIN chunks c ON c.id = t.chunk WHERE code_words MATCH ?1 LIMIT ?2")
            .map_err(error)?;
        let rows = statement.query_map(params![query, limit as i64], |row| row.get::<_, String>(0)).map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    pub fn has_file(&self, path: &str) -> bool {
        self.db.query_row("SELECT 1 FROM files WHERE path = ?1", [path], |_| Ok(())).optional().ok().flatten().is_some()
    }

    /// Indexed files under a folder (`src` or `src/`).
    pub fn files_under(&self, dir: &str) -> Result<Vec<String>, LoreError> {
        let dir = dir.trim_end_matches('/');
        let mut statement = self.db.prepare("SELECT path FROM files WHERE path LIKE ?1 || '/%' AND lang <> 'binary' ORDER BY path").map_err(error)?;
        let rows = statement.query_map([dir], |row| row.get::<_, String>(0)).map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    pub fn edges(&self) -> Result<Vec<(String, String)>, LoreError> {
        let mut statement = self.db.prepare("SELECT src, dst FROM edges ORDER BY src, dst").map_err(error)?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    /// The edges a cycle can be made of: imports, not a module holding its child.
    pub fn import_edges(&self) -> Result<Vec<(String, String)>, LoreError> {
        let mut statement = self.db.prepare("SELECT src, dst FROM edges WHERE kind = 'imports' ORDER BY src, dst").map_err(error)?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    /// What a file imports (resolved files) and what imports it.
    pub fn neighbours(&self, path: &str) -> Result<(Vec<String>, Vec<String>), LoreError> {
        let list = |sql: &str| -> Result<Vec<String>, LoreError> {
            let mut statement = self.db.prepare(sql).map_err(error)?;
            let rows = statement.query_map([path], |row| row.get::<_, String>(0)).map_err(error)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
        };
        Ok((list("SELECT dst FROM edges WHERE src = ?1 ORDER BY dst")?, list("SELECT src FROM edges WHERE dst = ?1 ORDER BY src")?))
    }

    /// The imports of a file that name nothing in the repository.
    pub fn external_imports(&self, path: &str) -> Result<Vec<String>, LoreError> {
        let files: BTreeMap<String, Lang> = self.file_langs()?;
        let namespaces = self.namespaces()?;
        let resolver = Resolver::new(&files, &namespaces);
        let Some(lang) = files.get(path).copied() else { return Ok(Vec::new()) };
        let mut statement = self.db.prepare("SELECT spec FROM imports WHERE path = ?1 ORDER BY line").map_err(error)?;
        let rows = statement.query_map([path], |row| row.get::<_, String>(0)).map_err(error)?;
        let mut out = Vec::new();
        for spec in rows {
            let spec = spec.map_err(error)?;
            if resolver.resolve(path, lang, &spec).is_empty() && !out.contains(&spec) {
                out.push(spec);
            }
        }
        Ok(out)
    }

    fn file_langs(&self) -> Result<BTreeMap<String, Lang>, LoreError> {
        file_langs(&self.db)
    }

    fn namespaces(&self) -> Result<BTreeMap<String, Vec<String>>, LoreError> {
        namespaces(&self.db)
    }

    /// The files that depend on `targets`, with their distance.
    pub fn dependents(&self, targets: &[String], depth: usize) -> Result<BTreeMap<String, usize>, LoreError> {
        Ok(graph::dependents(&self.edges()?, targets, depth))
    }
}

/// Filters of a search: a path prefix and a language, '' for any.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub path: String,
    pub lang: String,
}

pub(in crate::atlas) struct PreparedFilter<'a> {
    pub(super) filter: &'a Filter,
    rowid_is_chunk: bool,
    pub(super) selected: Option<usize>,
}

impl PreparedFilter<'_> {
    pub fn is_empty(&self) -> bool {
        self.selected == Some(0)
    }
}

/// Words of a query, identifiers split too, for the `code_words` table.
/// Code-piece vectors shared by every map on this account, by model (with its
/// dimension and token cap) and piece hash: `atlas-vectors.sqlite` in the user
/// cache. Best effort - a cache that cannot be opened or written is passed
/// over, and the map makes its own vectors.
struct SharedVectors {
    db: Connection,
    model: String,
}

impl SharedVectors {
    fn open(model: &str) -> Option<Self> {
        let dir = crate::index::cache_dir();
        std::fs::create_dir_all(&dir).ok()?;
        let db = Connection::open(dir.join("atlas-vectors.sqlite")).ok()?;
        db.busy_timeout(Duration::from_secs(10)).ok()?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS vectors(model TEXT, hash TEXT, vec BLOB, PRIMARY KEY(model, hash)) WITHOUT ROWID;").ok()?;
        Some(Self { db, model: model.to_owned() })
    }

    /// The vectors the cache holds for these pieces.
    fn take(&self, pieces: &[(String, String)]) -> HashMap<String, Vec<u8>> {
        let mut found = HashMap::new();
        let Ok(mut statement) = self.db.prepare("SELECT vec FROM vectors WHERE model = ?1 AND hash = ?2") else { return found };
        for (hash, _) in pieces {
            if let Ok(Some(blob)) = statement.query_row(params![self.model, hash], |row| row.get::<_, Vec<u8>>(0)).optional() {
                found.insert(hash.clone(), blob);
            }
        }
        found
    }

    fn put(&self, made: &[(&str, Vec<u8>)]) {
        let Ok(tx) = self.db.unchecked_transaction() else { return };
        for (hash, blob) in made {
            let _ = tx.execute("INSERT OR REPLACE INTO vectors VALUES(?1, ?2, ?3)", params![self.model, hash, blob]);
        }
        let _ = tx.commit();
    }
}

fn word_query(text: &str) -> String {
    let mut seen: Vec<String> = Vec::new();
    for found in word_re().find_iter(text) {
        let token = found.as_str().to_lowercase();
        for word in std::iter::once(token.clone()).chain(lang::split_identifier(found.as_str())) {
            if !seen.contains(&word) {
                seen.push(word);
            }
        }
    }
    seen.iter().take(64).map(|token| format!("\"{}\"", token.replace('"', "\"\""))).collect::<Vec<_>>().join(" OR ")
}

/// What a piece's embedding reads: where it is, what it defines, its start.
fn embed_text(path: &str, symbols: &str, body: &str) -> String {
    let head: String = body.chars().take(EMBED_CHARS).collect();
    format!("{path} {symbols}\n{head}")
}

fn drop_file(tx: &Connection, path: &str) -> Result<(), LoreError> {
    for sql in [
        "DELETE FROM code_words WHERE rowid IN (SELECT id FROM chunks WHERE path = ?1)",
        "DELETE FROM code_terms WHERE rowid IN (SELECT id FROM chunks WHERE path = ?1)",
        "DELETE FROM chunks WHERE path = ?1",
        "DELETE FROM symbols WHERE path = ?1",
        "DELETE FROM imports WHERE path = ?1",
        "DELETE FROM files WHERE path = ?1",
    ] {
        tx.execute(sql, [path]).map_err(error)?;
    }
    Ok(())
}

fn put_file(tx: &Connection, entry: &Entry, text: &str) -> Result<usize, LoreError> {
    let lines: Vec<&str> = text.lines().collect();
    tx.execute("INSERT INTO files VALUES(?1, ?2, ?3, ?4)", params![entry.path, entry.blob, entry.lang.name(), lines.len() as i64]).map_err(error)?;
    let symbols = lang::symbols(entry.lang, text);
    for symbol in &symbols {
        tx.execute("INSERT INTO symbols VALUES(?1, ?2, ?3, ?4)", params![symbol.name, symbol.kind, entry.path, symbol.line as i64]).map_err(error)?;
    }
    for import in lang::imports(entry.lang, text) {
        tx.execute("INSERT INTO imports VALUES(?1, ?2, ?3)", params![entry.path, import.spec, import.line as i64]).map_err(error)?;
    }
    let pieces = chunk(text, &symbols);
    for piece in &pieces {
        let body = lines[piece.start - 1..piece.end].join("\n");
        let names = piece.symbols.join(" ");
        let ehash = sha1_hex(&embed_text(&entry.path, &names, &body));
        tx.execute("INSERT INTO chunks(path, start, stop, symbols, ehash, body) VALUES(?1, ?2, ?3, ?4, ?5, ?6)", params![entry.path, piece.start as i64, piece.end as i64, names, ehash, body])
            .map_err(error)?;
        let id = tx.last_insert_rowid();
        let split: Vec<String> = piece.symbols.iter().flat_map(|name| lang::split_identifier(name)).collect();
        let path_words = lang::split_identifier(&entry.path).join(" ");
        tx.execute("INSERT INTO code_words(rowid, chunk, path, names, body) VALUES(?1, ?1, ?2, ?3, ?4)", params![id, format!("{} {path_words}", entry.path), format!("{names} {}", split.join(" ")), body]).map_err(error)?;
        let mut toks = terms_of(&body);
        toks.push(entry.path.clone());
        toks.extend(piece.symbols.iter().cloned());
        tx.execute("INSERT INTO code_terms(rowid, chunk, toks) VALUES(?1, ?1, ?2)", params![id, toks.join(" ")]).map_err(error)?;
    }
    Ok(pieces.len())
}

fn file_langs(db: &Connection) -> Result<BTreeMap<String, Lang>, LoreError> {
    let mut statement = db.prepare("SELECT path, lang FROM files WHERE lang <> 'binary'").map_err(error)?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (path, lang) = row.map_err(error)?;
        out.insert(path, Lang::from_name(&lang).unwrap_or(Lang::Text));
    }
    Ok(out)
}

fn namespaces(db: &Connection) -> Result<BTreeMap<String, Vec<String>>, LoreError> {
    let mut statement = db.prepare("SELECT s.name, s.path FROM symbols s JOIN files f ON f.path = s.path WHERE s.kind = 'namespace' AND f.lang = 'csharp'").map_err(error)?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        let (name, path) = row.map_err(error)?;
        out.entry(name).or_default().push(path);
    }
    Ok(out)
}

/// Every import resolved again against the current file set: a new file can
/// make an old import resolve, so the graph is rebuilt whole (lookups only).
fn rebuild_edges(tx: &Connection) -> Result<usize, LoreError> {
    let files = file_langs(tx)?;
    let namespaces = namespaces(tx)?;
    let resolver = Resolver::new(&files, &namespaces);
    tx.execute("DELETE FROM edges", []).map_err(error)?;
    let mut statement = tx.prepare("SELECT path, spec FROM imports").map_err(error)?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
    // `contains`: a Rust `mod x;` - the parent holds the child, which is not a
    // dependency a cycle is made of; `imports`: everything else.
    let mut edges: BTreeMap<(String, String), &'static str> = BTreeMap::new();
    for row in rows {
        let (path, spec) = row.map_err(error)?;
        let Some(lang) = files.get(&path).copied() else { continue };
        let kind = if spec.starts_with("mod:") { "contains" } else { "imports" };
        for target in resolver.resolve(&path, lang, &spec) {
            let entry = edges.entry((path.clone(), target)).or_insert(kind);
            if kind == "imports" {
                *entry = kind;
            }
        }
    }
    drop(statement);
    for ((src, dst), kind) in &edges {
        tx.execute("INSERT OR IGNORE INTO edges VALUES(?1, ?2, ?3)", params![src, dst, kind]).map_err(error)?;
    }
    Ok(edges.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_symbol_chunks_seek_name_before_chunks() {
        let db = Connection::open_in_memory().unwrap();
        create(&db).unwrap();
        let mut stmt = db.prepare(&format!("EXPLAIN QUERY PLAN {RUST_SYMBOL_CHUNKS}")).unwrap();
        let plan: Vec<String> = stmt.query_map(["chain", ""], |row| row.get(3)).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap();
        assert!(plan[0].starts_with("SEARCH s ") && plan[0].contains("symbols_name"), "{plan:?}");
        assert!(plan.iter().any(|step| step.starts_with("SEARCH c ") && step.contains("chunks_path")), "{plan:?}");
    }

    #[test]
    fn prepared_path_preserves_cutoff_ties_and_legacy_rowids() {
        let tmp = tempfile::tempdir().unwrap();
        let atlas = Atlas::open_at(&tmp.path().join("atlas.sqlite"), tmp.path()).unwrap();
        atlas.db.execute_batch(
            "BEGIN;
             INSERT INTO files VALUES ('RogueDOD/A_%/file.cpp','','cpp',1), ('RogueDOD/A_extra/file.cpp','','cpp',1), ('RogueDOD/A_%/file.rs','','rust',1);
             WITH RECURSIVE n(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM n WHERE id<98)
             INSERT INTO chunks(id,path,start,stop,symbols,ehash,body)
             SELECT id,CASE id WHEN 97 THEN 'RogueDOD/A_extra/file.cpp' WHEN 98 THEN 'RogueDOD/A_%/file.rs'
                 ELSE 'RogueDOD/A_%/file.cpp' END,1,1,'','', '' FROM n;
             INSERT INTO code_words(rowid,chunk,path,names,body)
             SELECT id,id,'same','marker','marker' FROM chunks;"
        ).unwrap();
        let filter = Filter { path: "roguedod/a_%/".to_owned(), lang: "cpp".to_owned() };
        let mut timings = SearchTimings::default();
        let selected = atlas.prepare_filter(&filter, &mut timings);
        assert_eq!(selected.selected, Some(96));
        let query = "\"marker\"";
        let expected: Vec<_> = (1..=DEPTH as i64).collect();
        assert_eq!(atlas.fts("code_words", query, &selected), expected);

        // A broad path exceeds the bounded temp set. It still uses the full
        // predicate and deterministic ranking through the ordinary join.
        atlas.db.execute(
            "WITH RECURSIVE n(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM n WHERE id<?1)
             INSERT INTO chunks(path,start,stop,symbols,ehash,body)
             SELECT 'RogueDOD/A_%/file.cpp',1,1,'','','' FROM n", [FILTER_LIMIT]
        ).unwrap();
        let wide = atlas.prepare_filter(&filter, &mut timings);
        assert_eq!(wide.selected, None);
        assert!(wide.rowid_is_chunk);
        assert_eq!(atlas.fts("code_words", query, &wide), expected);

        atlas.db.execute_batch("DELETE FROM meta WHERE key='fts_rowid'; UPDATE code_words SET rowid=rowid+1000").unwrap();
        let legacy = atlas.prepare_filter(&filter, &mut timings);
        assert!(!legacy.rowid_is_chunk);
        assert_eq!(legacy.selected, None);
        assert_eq!(atlas.fts("code_words", query, &legacy), expected);
    }
}
