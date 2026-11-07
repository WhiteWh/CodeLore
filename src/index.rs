//! The disposable index: SQLite in the user cache, rebuilt from the JSONL when
//! a file changes. Deleting it costs one rebuild and nothing else. It is never
//! committed. This is a port of CodeLore's `index.py` and keeps its schema
//! (user_version 3), so the two read each other's file while the Python
//! version is the conformance reference.
use crate::config::Config;
use crate::embed::Embeddings;
use crate::{parse, LoreError};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

pub const INDEX_VERSION: i64 = 3;

/// How long a connection waits for another process's write lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(120);

/// An embedding reads the start of a long text; the model's window is not the point.
const EMBED_MAIN: usize = 1500;

/// A token that only means something whole: `scene.depth_buffer`,
/// `ERR_DEVICE_LOST`, `0x80070005`, `src/render/post.py`, `E_FAIL`.
fn term_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"0x[0-9A-Fa-f]{4,}|[A-Za-z0-9_]+(?:[./\\:-][A-Za-z0-9_]+)+|[A-Za-z0-9]*_[A-Za-z0-9_]+|\b[A-Z][A-Z0-9]{2,}\b",
        )
        .expect("the term pattern is valid")
    })
}

/// A word of two or more characters, as `\w{2,}`.
pub fn word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\w{2,}").expect("the word pattern is valid"))
}

/// The whole tokens of a text (see `term_re`), each once, in order of first sight.
pub fn terms_of(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for found in term_re().find_iter(text) {
        let token = found.as_str().replace('\\', "/");
        let token = token.trim_matches(|c| matches!(c, '.' | ':' | '-' | '/'));
        if token.chars().count() >= 3 && seen.insert(token.to_lowercase()) {
            out.push(token.to_owned());
        }
    }
    out
}

fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        Value::Object(map) => map.values().for_each(|item| strings(item, out)),
        _ => {}
    }
}

/// Every string a record holds, in key order.
pub fn all_strings(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    strings(value, &mut out);
    out
}

/// The sentence a record is about, for display and for its embedding.
pub fn main_text(record: &Value) -> String {
    for key in ["symptom", "what", "question", "when", "goal", "body", "description"] {
        if let Some(text) = record.get(key).and_then(Value::as_str) {
            if !text.is_empty() {
                return text.to_owned();
            }
        }
    }
    String::new()
}

const SKIP: [&str; 11] = ["v", "id", "kind", "area", "status", "created", "updated", "by", "title", "signals", "plan"];

/// What a record is indexed as: signals, title, main text, the rest, whole terms.
pub fn columns(record: &Value) -> (String, String, String, String, String) {
    let signal_list: Vec<String> = record
        .get("signals")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let signals = signal_list.join("\n");
    let title = record.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
    let main = main_text(record);
    let mut body_parts: Vec<String> = Vec::new();
    if let Some(map) = record.as_object() {
        for (key, value) in map {
            if SKIP.contains(&key.as_str()) || key == "refs" {
                continue;
            }
            if value.as_str() == Some(main.as_str()) {
                continue;
            }
            strings(value, &mut body_parts);
        }
    }
    let mut files: Vec<String> = Vec::new();
    if let Some(refs) = record.get("refs") {
        if let Some(symbols) = refs.get("symbols").and_then(Value::as_array) {
            body_parts.extend(symbols.iter().filter_map(|item| item.as_str().map(str::to_owned)));
        }
        if let Some(list) = refs.get("files").and_then(Value::as_array) {
            files.extend(list.iter().filter_map(|item| item.as_str().map(str::to_owned)));
        }
    }
    let body = body_parts.join("\n");
    let mut all = vec![signals.clone(), title.clone(), main.clone(), body.clone()];
    all.extend(files);
    let terms = terms_of(&all.join("\n")).join(" ");
    (signals, title, main, body, terms)
}

/// The text a record is embedded as.
pub fn embed_text(record: &Value) -> String {
    let main: String = main_text(record).chars().take(EMBED_MAIN).collect();
    let mut parts = vec![record.get("title").and_then(Value::as_str).unwrap_or_default().to_owned(), main];
    if let Some(cause) = record.get("cause").and_then(Value::as_str) {
        parts.push(cause.to_owned());
    }
    if let Some(signals) = record.get("signals").and_then(Value::as_array) {
        let list: Vec<&str> = signals.iter().filter_map(Value::as_str).collect();
        if !list.is_empty() {
            parts.push(list.join("; "));
        }
    }
    parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n")
}

fn sha1_hex(data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// The user cache folder for indexes: `LORE_CACHE`, else the platform's.
pub fn cache_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("LORE_CACHE").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    let home = || std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default();
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData").join("Local"));
        base.join("codelore")
    } else if cfg!(target_os = "macos") {
        home().join("Library").join("Caches").join("codelore")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".cache"))
            .join("codelore")
    }
}

fn resolved(root: &Path) -> String {
    let path = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let text = path.to_string_lossy().into_owned();
    text.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(text)
}

/// A short stable name for a repository root (the same key Python uses).
pub fn repo_key(root: &Path) -> String {
    let mut text = resolved(root);
    if cfg!(windows) {
        text = text.to_lowercase();
    }
    sha1_hex(text.as_bytes())[..16].to_owned()
}

/// Where the index may live, in order: the user cache, then the repository's
/// own git directory (never tracked, writable wherever the repository is).
pub fn index_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![cache_dir().join(repo_key(root))];
    let git = std::process::Command::new("git").arg("-C").arg(root).args(["rev-parse", "--git-path", "codelore"]).output();
    if let Ok(output) = git {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !text.is_empty() {
                let path = PathBuf::from(text);
                dirs.push(if path.is_absolute() { path } else { root.join(path) });
            }
        }
    }
    dirs
}

/// What one refresh did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Refresh {
    /// Files read again (new, or changed in content).
    pub files_read: usize,
    /// Files dropped because they are gone.
    pub files_dropped: usize,
    /// Records written to the index.
    pub records: usize,
    /// The index was built from nothing (a new file or another version).
    pub rebuilt: bool,
}

pub struct Index {
    root: PathBuf,
    lore_dir: PathBuf,
    path: PathBuf,
    db: Connection,
    /// Set when the index could not live in the user cache.
    pub place_note: Option<String>,
    /// The file was just created: the next refresh is a build.
    fresh: bool,
}

fn index_error(error: impl std::fmt::Display) -> LoreError {
    LoreError::new(format!("index: {error}"))
}

fn connect(path: &Path) -> Result<Connection, LoreError> {
    let db = Connection::open(path).map_err(|error| LoreError::new(format!("{}: {error}", path.display())))?;
    db.busy_timeout(BUSY_TIMEOUT).map_err(index_error)?;
    db.execute_batch("PRAGMA journal_mode=WAL").map_err(index_error)?;
    Ok(db)
}

fn create(db: &Connection) -> Result<(), LoreError> {
    db.execute_batch(&format!(
        r#"CREATE TABLE files(path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, sha TEXT);
        CREATE TABLE records(id TEXT PRIMARY KEY, path TEXT, kind TEXT, area TEXT,
            status TEXT, plan TEXT, title TEXT, updated TEXT, ehash TEXT, json TEXT);
        CREATE INDEX records_path ON records(path);
        CREATE VIRTUAL TABLE words USING fts5(id UNINDEXED, signals, title, main, body,
            tokenize = "porter unicode61 remove_diacritics 2 tokenchars '_'");
        CREATE VIRTUAL TABLE terms USING fts5(id UNINDEXED, toks,
            tokenize = "unicode61 remove_diacritics 0 tokenchars '_./:-'");
        CREATE TABLE vectors(model TEXT, hash TEXT, vec BLOB, PRIMARY KEY(model, hash));
        CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
        PRAGMA user_version = {INDEX_VERSION};"#
    ))
    .map_err(index_error)
}

/// Opens the file; another version is deleted and built again. Returns whether
/// the file is new.
fn open_db(path: &Path) -> Result<(Connection, bool), LoreError> {
    let db = connect(path)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(index_error)?;
    if version == INDEX_VERSION {
        return Ok((db, false));
    }
    drop(db);
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        if file.exists() {
            std::fs::remove_file(&file).map_err(|error| LoreError::new(format!("{}: {error}", file.display())))?;
        }
    }
    let db = connect(path)?;
    create(&db)?;
    Ok((db, true))
}

/// The index of `config`, up to date: what a search starts with.
pub fn open_index(config: &Config) -> Result<Index, LoreError> {
    let mut tried: Vec<String> = Vec::new();
    for dir in index_dirs(&config.root) {
        let attempt = std::fs::create_dir_all(&dir)
            .map_err(|error| LoreError::new(format!("{}: {error}", dir.display())))
            .and_then(|()| Index::open_at(&dir.join("index.sqlite"), &config.root, &config.lore_dir()));
        match attempt {
            Ok(mut index) => {
                if !tried.is_empty() {
                    index.place_note = Some(format!(
                        "the index is in {}, since {}; set LORE_CACHE to choose the folder",
                        dir.display(),
                        tried[0]
                    ));
                }
                index.refresh()?;
                return Ok(index);
            }
            Err(error) => tried.push(format!("{}: {error}", dir.display())),
        }
    }
    Err(LoreError::new(format!(
        "no folder the index can be written to - set LORE_CACHE to a writable folder ({})",
        tried.join("; ")
    )))
}

impl Index {
    /// Opens (creating) the index file at `path` for the lore under `root`.
    pub fn open_at(path: &Path, root: &Path, lore_dir: &Path) -> Result<Self, LoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| LoreError::new(format!("{}: {error}", parent.display())))?;
        }
        let (db, fresh) = open_db(path)?;
        Ok(Self { root: root.to_path_buf(), lore_dir: lore_dir.to_path_buf(), path: path.to_path_buf(), db, place_note: None, fresh })
    }

    pub fn path(&self) -> &Path { &self.path }
    pub fn root(&self) -> &Path { &self.root }

    /// The lore's files in the order Python reads them: the lore folder's,
    /// then the plans'. The order decides the order of ties.
    fn lore_files(&self) -> Vec<(String, PathBuf)> {
        let mut ordered = Vec::new();
        for dir in [self.lore_dir.clone(), self.lore_dir.join("plans")] {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let mut names: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") && path.is_file())
                .collect();
            names.sort();
            for path in names {
                let relative = path.strip_prefix(&self.root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                ordered.push((relative, path));
            }
        }
        ordered
    }

    /// Brings the index up to date with the JSONL files: a file is read again
    /// only when its size or modification time moved and its bytes differ.
    pub fn refresh(&mut self) -> Result<Refresh, LoreError> {
        let ordered = self.lore_files();
        let mut summary = Refresh { rebuilt: self.fresh, ..Refresh::default() };
        self.fresh = false;
        // The write lock first (waiting for another process's), then what is known:
        // a deferred transaction that reads first cannot wait for a writer.
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate).map_err(index_error)?;
        let mut known: BTreeMap<String, (i64, i64, String)> = BTreeMap::new();
        {
            let mut statement = tx.prepare("SELECT path, size, mtime, sha FROM files").map_err(index_error)?;
            let rows = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?))))
                .map_err(index_error)?;
            for row in rows.flatten() {
                known.insert(row.0, row.1);
            }
        }
        let found: HashSet<&str> = ordered.iter().map(|(relative, _)| relative.as_str()).collect();
        for relative in known.keys().filter(|relative| !found.contains(relative.as_str())) {
            drop_file(&tx, relative)?;
            summary.files_dropped += 1;
        }
        for (relative, path) in &ordered {
            let meta = std::fs::metadata(path).map_err(|error| LoreError::new(format!("{}: {error}", path.display())))?;
            let size = meta.len() as i64;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |span| span.as_nanos() as i64);
            let old = known.get(relative);
            if old.is_some_and(|old| old.0 == size && old.1 == mtime) {
                continue;
            }
            let data = std::fs::read(path).map_err(|error| LoreError::new(format!("{}: {error}", path.display())))?;
            let sha = sha1_hex(&data);
            if old.is_some_and(|old| old.2 == sha) {
                tx.execute("UPDATE files SET size=?1, mtime=?2 WHERE path=?3", params![size, mtime, relative]).map_err(index_error)?;
                continue;
            }
            let records = parse(&data, relative)?;
            drop_file(&tx, relative)?;
            for record in &records {
                if put(&tx, relative, record)? {
                    summary.records += 1;
                }
            }
            tx.execute("INSERT OR REPLACE INTO files VALUES(?1,?2,?3,?4)", params![relative, size, mtime, sha]).map_err(index_error)?;
            summary.files_read += 1;
        }
        tx.commit().map_err(index_error)?;
        Ok(summary)
    }

    /// Every record, in the order the index holds them.
    pub fn all(&self) -> Result<Vec<Value>, LoreError> {
        let mut statement = self.db.prepare("SELECT json FROM records").map_err(index_error)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0)).map_err(index_error)?;
        Ok(rows.flatten().filter_map(|text| serde_json::from_str(&text).ok()).collect())
    }

    pub fn get(&self, id: &str) -> Result<Option<Value>, LoreError> {
        let text: Option<String> = self
            .db
            .query_row("SELECT json FROM records WHERE id=?1", params![id], |row| row.get(0))
            .optional()
            .map_err(index_error)?;
        Ok(text.and_then(|text| serde_json::from_str(&text).ok()))
    }

    pub fn count(&self) -> Result<usize, LoreError> {
        let count: i64 = self.db.query_row("SELECT count(*) FROM records", [], |row| row.get(0)).map_err(index_error)?;
        Ok(count as usize)
    }

    pub(crate) fn db(&self) -> &Connection { &self.db }

    /// Embeds every record that has no vector under `model`. The error is the
    /// provider's, for the caller to turn into a note.
    pub fn ensure_vectors(&self, embedder: &dyn Embeddings, model: &str, doc_prefix: &str) -> Result<(), LoreError> {
        // The index records which model and dimension its vectors came from;
        // a change (lore.toml's embed.model, or the model's dimension) makes
        // every stored vector useless, so they are dropped and rebuilt.
        let dimension = embedder.dimensions().to_string();
        let stored_model: Option<String> = self.db.query_row("SELECT value FROM meta WHERE key='model'", [], |row| row.get(0)).optional().map_err(index_error)?;
        let stored_dim: Option<String> = self.db.query_row("SELECT value FROM meta WHERE key='dimension'", [], |row| row.get(0)).optional().map_err(index_error)?;
        if stored_model.as_deref() != Some(model) || stored_dim.as_deref() != Some(dimension.as_str()) {
            self.db.execute("DELETE FROM vectors", []).map_err(index_error)?;
            self.db.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('model', ?1)", params![model]).map_err(index_error)?;
            self.db.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('dimension', ?1)", params![dimension]).map_err(index_error)?;
        }
        let mut have: HashSet<String> = HashSet::new();
        {
            let mut statement = self.db.prepare("SELECT hash FROM vectors WHERE model=?1").map_err(index_error)?;
            let rows = statement.query_map(params![model], |row| row.get::<_, String>(0)).map_err(index_error)?;
            have.extend(rows.flatten());
        }
        let mut todo: Vec<(String, String)> = Vec::new();
        {
            let mut statement = self.db.prepare("SELECT json, ehash FROM records").map_err(index_error)?;
            let rows = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .map_err(index_error)?;
            for (text, hash) in rows.flatten() {
                if have.insert(hash.clone()) {
                    if let Ok(record) = serde_json::from_str::<Value>(&text) {
                        todo.push((hash, format!("{doc_prefix}{}", embed_text(&record))));
                    }
                }
            }
        }
        if todo.is_empty() {
            return Ok(());
        }
        let texts: Vec<String> = todo.iter().map(|(_, text)| text.clone()).collect();
        let vectors = embedder.embed(&texts)?;
        if vectors.len() != todo.len() {
            return Err(LoreError::new("the provider answered with another number of vectors"));
        }
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate).map_err(index_error)?;
        for ((hash, _), vector) in todo.iter().zip(vectors) {
            let blob: Vec<u8> = vector.iter().flat_map(|value| value.to_le_bytes()).collect();
            tx.execute("INSERT OR REPLACE INTO vectors VALUES(?1,?2,?3)", params![model, hash, blob]).map_err(index_error)?;
        }
        tx.commit().map_err(index_error)
    }

    /// Puts `model`'s precomputed vectors (by record id) into the index and
    /// sets its model/dimension meta. The bench harness uses it; the searcher's
    /// own path is `ensure_vectors`.
    pub fn load_vectors(&self, model: &str, dimension: usize, vectors: &[(String, Vec<f32>)]) -> Result<(), LoreError> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate).map_err(index_error)?;
        tx.execute("DELETE FROM vectors", []).map_err(index_error)?;
        tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('model', ?1)", params![model]).map_err(index_error)?;
        tx.execute("INSERT OR REPLACE INTO meta(key, value) VALUES('dimension', ?1)", params![dimension.to_string()]).map_err(index_error)?;
        for (id, vector) in vectors {
            let hash: String = tx.query_row("SELECT ehash FROM records WHERE id=?1", params![id], |row| row.get(0)).map_err(index_error)?;
            let blob: Vec<u8> = vector.iter().flat_map(|value| value.to_le_bytes()).collect();
            tx.execute("INSERT OR REPLACE INTO vectors VALUES(?1,?2,?3)", params![model, hash, blob]).map_err(index_error)?;
        }
        tx.commit().map_err(index_error)
    }

    /// The vector of every record that has one under `model`, by id.
    pub fn vectors(&self, model: &str) -> Result<Vec<(String, Vec<f32>)>, LoreError> {
        let mut statement = self
            .db
            .prepare("SELECT r.id, v.vec FROM records r JOIN vectors v ON v.hash = r.ehash AND v.model = ?1")
            .map_err(index_error)?;
        let rows = statement
            .query_map(params![model], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))
            .map_err(index_error)?;
        Ok(rows
            .flatten()
            .map(|(id, blob)| {
                let (chunks, _) = blob.as_chunks::<4>();
                (id, chunks.iter().map(|bytes| f32::from_le_bytes(*bytes)).collect())
            })
            .collect())
    }
}

fn drop_file(tx: &Connection, relative: &str) -> Result<(), LoreError> {
    let ids: Vec<String> = {
        let mut statement = tx.prepare("SELECT id FROM records WHERE path=?1").map_err(index_error)?;
        let rows = statement.query_map(params![relative], |row| row.get::<_, String>(0)).map_err(index_error)?;
        rows.flatten().collect()
    };
    for id in &ids {
        tx.execute("DELETE FROM words WHERE id=?1", params![id]).map_err(index_error)?;
        tx.execute("DELETE FROM terms WHERE id=?1", params![id]).map_err(index_error)?;
    }
    tx.execute("DELETE FROM records WHERE path=?1", params![relative]).map_err(index_error)?;
    tx.execute("DELETE FROM files WHERE path=?1", params![relative]).map_err(index_error)?;
    Ok(())
}

fn put(tx: &Connection, relative: &str, record: &Value) -> Result<bool, LoreError> {
    let Some(id) = record.get("id").and_then(Value::as_str) else { return Ok(false) };
    let ehash = sha1_hex(embed_text(record).as_bytes());
    let text = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_owned);
    tx.execute(
        "INSERT OR REPLACE INTO records VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            id,
            relative,
            text("kind"),
            text("area"),
            text("status"),
            text("plan"),
            text("title").unwrap_or_default(),
            text("updated").unwrap_or_default(),
            ehash,
            crate::canonical(record)
        ],
    )
    .map_err(index_error)?;
    let (signals, title, main, body, terms) = columns(record);
    tx.execute("INSERT INTO words(id, signals, title, main, body) VALUES(?1,?2,?3,?4,?5)", params![id, signals, title, main, body])
        .map_err(index_error)?;
    tx.execute("INSERT INTO terms(id, toks) VALUES(?1,?2)", params![id, terms]).map_err(index_error)?;
    Ok(true)
}
