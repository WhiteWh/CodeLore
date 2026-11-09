//! The ledger's fold, cached in the user cache beside the lore's index. Each
//! journal is read on from where the last read stopped: the cache keeps, per
//! file, how far it read and a hash of those bytes. A file that got shorter,
//! went away, or whose read part changed broke the append-only rule: that is
//! said by name and the cache is built again from every file.
use super::fold::{self, Entry, Side};
use super::{journal_author, Line};
use crate::config::{Config, Ledger};
use crate::store::canonical;
use crate::{LoreError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Raised when what the cache holds changes meaning.
const CACHE_FORMAT: &str = "1";
pub const FILE_NAME: &str = "ledger.sqlite";

pub type Key = (String, String);

pub struct Cache {
    db: Connection,
    pub path: PathBuf,
    /// What the last refresh found worth saying (rebuilds, stray files).
    pub warnings: Vec<String>,
}

/// The whole state, with pending lines over it when asked.
pub struct State {
    pub entries: BTreeMap<Key, Entry>,
    pub warnings: Vec<String>,
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn db_error(path: &Path) -> impl Fn(rusqlite::Error) -> LoreError + '_ {
    move |error| LoreError::new(format!("{}: {error}", path.display()))
}

fn mtime(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|since| since.as_nanos() as i64).unwrap_or(0)
}

/// What the declarations say, so a changed lore.toml reads every line again.
fn schema_print(ledger: &Ledger) -> String {
    let kinds: Map<String, Value> = ledger
        .kinds
        .iter()
        .map(|(kind, fields)| (kind.clone(), Value::Object(fields.iter().map(|(field, kind_of)| (field.clone(), Value::String(kind_of.name()))).collect())))
        .collect();
    canonical(&serde_json::json!({"cache": CACHE_FORMAT, "v": super::VERSION, "dir": ledger.dir, "kinds": kinds}))
}

fn entry_from_json(value: &Value) -> Entry {
    let strings = |value: Option<&Value>| -> Vec<String> { value.and_then(Value::as_array).into_iter().flatten().filter_map(|item| item.as_str().map(str::to_owned)).collect() };
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let conflicts = value
        .get("conflicts")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(field, sides)| {
            let sides = sides
                .as_array()
                .into_iter()
                .flatten()
                .map(|side| Side { id: text(side, "id"), by: text(side, "by"), at: text(side, "at"), value: side.get("value").cloned().unwrap_or(Value::Null) })
                .collect();
            (field.clone(), sides)
        })
        .collect();
    let dangling = value.get("dangling").and_then(Value::as_array).into_iter().flatten().map(|pair| (text(pair, "line"), text(pair, "parent"))).collect();
    Entry { set: value.get("set").and_then(Value::as_object).cloned().unwrap_or_default(), heads: strings(value.get("heads")), conflicts, dangling, order: Vec::new() }
}

impl Cache {
    /// The cache of `config`'s ledger, brought up to date with the journals.
    pub fn open(config: &Config, ledger: &Ledger) -> Result<Self> {
        let dir = crate::index::cache_dir().join(crate::index::repo_key(&config.root));
        std::fs::create_dir_all(&dir).map_err(|error| LoreError::new(format!("{}: {error}; set LORE_CACHE to a writable folder", dir.display())))?;
        Self::open_at(&dir.join(FILE_NAME), &config.root, ledger)
    }

    pub fn open_at(path: &Path, root: &Path, ledger: &Ledger) -> Result<Self> {
        let db = Connection::open(path).map_err(db_error(path))?;
        db.busy_timeout(std::time::Duration::from_secs(60)).map_err(db_error(path))?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS files (path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL, lines INTEGER NOT NULL, prefix TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS lines (id TEXT NOT NULL, file TEXT NOT NULL, number INTEGER NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, text TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS lines_entry ON lines (kind, key);
             CREATE INDEX IF NOT EXISTS lines_id ON lines (id);
             CREATE TABLE IF NOT EXISTS entries (kind TEXT NOT NULL, key TEXT NOT NULL, state TEXT NOT NULL, PRIMARY KEY (kind, key));",
        )
        .map_err(db_error(path))?;
        let mut cache = Self { db, path: path.to_path_buf(), warnings: Vec::new() };
        cache.refresh(root, ledger)?;
        Ok(cache)
    }

    fn refresh(&mut self, root: &Path, ledger: &Ledger) -> Result<()> {
        let path = self.path.clone();
        let folder = super::folder(root, ledger);
        let shown = |relative: &str| format!("{}/{relative}", ledger.dir);
        let tx = self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(db_error(&path))?;
        let print = schema_print(ledger);
        let stored_print: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = 'schema'", [], |row| row.get(0)).optional().map_err(db_error(&path))?;
        let mut known: BTreeMap<String, (i64, i64, i64, String)> = BTreeMap::new();
        {
            let mut statement = tx.prepare("SELECT path, size, mtime, lines, prefix FROM files").map_err(db_error(&path))?;
            let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))).map_err(db_error(&path))?;
            for row in rows {
                let (file, info) = row.map_err(db_error(&path))?;
                known.insert(file, info);
            }
        }
        let mut warnings = Vec::new();
        let mut journals = Vec::new();
        for relative in super::files(&folder) {
            if journal_author(&relative).is_some() {
                journals.push(relative);
            } else {
                warnings.push(format!("{} is not a journal (<author>@<machine>/<YYYY-MM>.jsonl) and is not read", shown(&relative)));
            }
        }
        let mut rebuild = stored_print.as_deref() != Some(print.as_str());
        let mut broken = Vec::new();
        for gone in known.keys().filter(|file| !journals.contains(file)) {
            broken.push(format!("{} is gone", shown(gone)));
        }
        // (file, bytes, first byte to parse, first line number, size, mtime)
        let mut reads: Vec<(String, Vec<u8>, usize, usize, i64)> = Vec::new();
        for relative in &journals {
            let full = folder.join(relative);
            let meta = std::fs::metadata(&full).map_err(|error| LoreError::new(format!("{}: {error}", shown(relative))))?;
            let (size, stamp) = (meta.len() as i64, mtime(&meta));
            match known.get(relative) {
                Some((old_size, old_stamp, _, _)) if *old_size == size && *old_stamp == stamp => {}
                Some((old_size, _, lines, prefix)) => {
                    let bytes = std::fs::read(&full).map_err(|error| LoreError::new(format!("{}: {error}", shown(relative))))?;
                    let old = *old_size as usize;
                    if bytes.len() < old {
                        broken.push(format!("{} got shorter", shown(relative)));
                    } else if sha_hex(&bytes[..old]) != *prefix {
                        broken.push(format!("{} changed before its end", shown(relative)));
                    } else {
                        reads.push((relative.clone(), bytes, old, *lines as usize + 1, stamp));
                    }
                }
                None => {
                    let bytes = std::fs::read(&full).map_err(|error| LoreError::new(format!("{}: {error}", shown(relative))))?;
                    reads.push((relative.clone(), bytes, 0, 1, stamp));
                }
            }
        }
        if !broken.is_empty() && !rebuild {
            warnings.push(format!("{}: journals are only ever appended to; the ledger cache was built again from every file", broken.join("; ")));
        }
        rebuild |= !broken.is_empty();
        let mut dirty: BTreeSet<Key> = BTreeSet::new();
        if rebuild {
            // Indexes are made again after the bulk insert: far cheaper than kept up row by row.
            tx.execute_batch("DELETE FROM files; DELETE FROM lines; DELETE FROM entries; DROP INDEX IF EXISTS lines_entry; DROP INDEX IF EXISTS lines_id;").map_err(db_error(&path))?;
            reads.clear();
            for relative in &journals {
                let full = folder.join(relative);
                let meta = std::fs::metadata(&full).map_err(|error| LoreError::new(format!("{}: {error}", shown(relative))))?;
                let bytes = std::fs::read(&full).map_err(|error| LoreError::new(format!("{}: {error}", shown(relative))))?;
                reads.push((relative.clone(), bytes, 0, 1, mtime(&meta)));
            }
        }
        let mut errors: Vec<LoreError> = Vec::new();
        // On a rebuild every line is here already, so entries fold without reading them back.
        let mut fresh: BTreeMap<Key, Vec<Line>> = BTreeMap::new();
        // Parsing and checking the lines is most of a read: the files go in parallel.
        let parsed = parse_all(&reads, ledger, &shown);
        for ((relative, bytes, _, first, stamp), lines) in reads.iter().zip(parsed) {
            let (author, _) = journal_author(relative).unwrap_or_default();
            let place = shown(relative);
            let lines = match lines {
                Ok(lines) => lines,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            };
            let foreign: Vec<String> = lines.iter().enumerate().filter(|(_, (line, _))| line.by != author).map(|(at, (line, _))| format!("line {}: `by` {} in {author}'s journal", first + at, line.by)).collect();
            if !foreign.is_empty() {
                errors.push(LoreError::with(format!("{place} is refused"), foreign));
                continue;
            }
            let count = (first - 1 + lines.len()) as i64;
            {
                let mut insert = tx.prepare_cached("INSERT INTO lines (id, file, number, kind, key, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)").map_err(db_error(&path))?;
                for (at, (line, text)) in lines.iter().enumerate() {
                    insert.execute(params![line.id, relative, (*first + at) as i64, line.kind, line.key, text]).map_err(db_error(&path))?;
                    dirty.insert(line.entry());
                }
            }
            if rebuild {
                for (line, _) in lines {
                    fresh.entry(line.entry()).or_default().push(line);
                }
            }
            tx.execute(
                "INSERT OR REPLACE INTO files (path, size, mtime, lines, prefix) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![relative, bytes.len() as i64, stamp, count, sha_hex(bytes)],
            )
            .map_err(db_error(&path))?;
        }
        if rebuild {
            tx.execute_batch("CREATE INDEX IF NOT EXISTS lines_entry ON lines (kind, key); CREATE INDEX IF NOT EXISTS lines_id ON lines (id);").map_err(db_error(&path))?;
        }
        if !errors.is_empty() {
            drop(tx);
            let mut details = Vec::new();
            for error in &errors {
                details.push(error.message.clone());
                details.extend(error.details.iter().map(|detail| format!("  {detail}")));
            }
            return Err(LoreError::with("the ledger is refused; nothing in it was read", details));
        }
        {
            let mut put = tx.prepare_cached("INSERT OR REPLACE INTO entries (kind, key, state) VALUES (?1, ?2, ?3)").map_err(db_error(&path))?;
            for (kind, key) in &dirty {
                let entry = match fresh.remove(&(kind.clone(), key.clone())) {
                    Some(lines) => fold::fold(&lines),
                    None => fold::fold(&lines_in(&tx, &path, ledger, kind, key)?),
                };
                put.execute(params![kind, key, canonical(&entry.to_json())]).map_err(db_error(&path))?;
            }
        }
        tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('schema', ?1)", params![print]).map_err(db_error(&path))?;
        let wrote = !reads.is_empty() || rebuild;
        tx.commit().map_err(db_error(&path))?;
        if wrote {
            // Into the main file: a reader that opens the cache then has no
            // write-ahead log to read through first.
            let _ = self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
        self.warnings = warnings;
        Ok(())
    }

    /// The journal lines of one entry.
    pub fn lines(&self, ledger: &Ledger, kind: &str, key: &str) -> Result<Vec<Line>> {
        lines_in(&self.db, &self.path, ledger, kind, key)
    }

    /// One entry's fold, from the journals alone.
    pub fn entry(&self, kind: &str, key: &str) -> Result<Option<Entry>> {
        let state: Option<String> =
            self.db.query_row("SELECT state FROM entries WHERE kind = ?1 AND key = ?2", params![kind, key], |row| row.get(0)).optional().map_err(db_error(&self.path))?;
        Ok(state.and_then(|state| serde_json::from_str::<Value>(&state).ok()).map(|value| entry_from_json(&value)))
    }

    /// Every entry's fold, from the journals alone; of one kind when given.
    pub fn entries(&self, kind: Option<&str>) -> Result<BTreeMap<Key, Entry>> {
        let mut statement = self.db.prepare("SELECT kind, key, state FROM entries WHERE ?1 IS NULL OR kind = ?1").map_err(db_error(&self.path))?;
        let rows = statement.query_map(params![kind], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))).map_err(db_error(&self.path))?;
        let mut entries = BTreeMap::new();
        for row in rows {
            let (kind, key, state) = row.map_err(db_error(&self.path))?;
            let value: Value = serde_json::from_str(&state).unwrap_or(Value::Null);
            entries.insert((kind, key), entry_from_json(&value));
        }
        Ok(entries)
    }

    /// Whether a line with this id is in some journal.
    pub fn has_line(&self, id: &str) -> Result<bool> {
        let found: Option<i64> = self.db.query_row("SELECT 1 FROM lines WHERE id = ?1 LIMIT 1", params![id], |row| row.get(0)).optional().map_err(db_error(&self.path))?;
        Ok(found.is_some())
    }

    /// Per journal: (path, bytes, lines).
    pub fn files(&self) -> Result<Vec<(String, i64, i64)>> {
        let mut statement = self.db.prepare("SELECT path, size, lines FROM files ORDER BY path").map_err(db_error(&self.path))?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).map_err(db_error(&self.path))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_error(&self.path))
    }

    /// The state with `pending` lines read over the journals: the entries they
    /// touch are folded again from both.
    pub fn state(&self, ledger: &Ledger, kind: Option<&str>, pending: &[Line]) -> Result<BTreeMap<Key, Entry>> {
        let mut entries = self.entries(kind)?;
        let mut extra: BTreeMap<Key, Vec<Line>> = BTreeMap::new();
        for line in pending.iter().filter(|line| kind.is_none_or(|kind| line.kind == kind)) {
            extra.entry(line.entry()).or_default().push(line.clone());
        }
        for ((kind, key), more) in extra {
            let mut lines = self.lines(ledger, &kind, &key)?;
            lines.extend(more);
            entries.insert((kind, key), fold::fold(&lines));
        }
        Ok(entries)
    }

    /// One entry with `pending` over it.
    pub fn entry_with(&self, ledger: &Ledger, kind: &str, key: &str, pending: &[Line]) -> Result<Option<Entry>> {
        let more: Vec<Line> = pending.iter().filter(|line| line.kind == kind && line.key == key).cloned().collect();
        if more.is_empty() {
            return self.entry(kind, key);
        }
        let mut lines = self.lines(ledger, kind, key)?;
        lines.extend(more);
        Ok(Some(fold::fold(&lines)))
    }
}

type Read = (String, Vec<u8>, usize, usize, i64);

fn parse_all(reads: &[Read], ledger: &Ledger, shown: &(dyn Fn(&str) -> String + Sync)) -> Vec<Result<Vec<(Line, String)>>> {
    let jobs: Vec<(String, &[u8], usize)> = reads.iter().map(|(relative, bytes, from, first, _)| (shown(relative), &bytes[*from..], *first)).collect();
    super::parse_many(&jobs, ledger)
}

fn lines_in(db: &Connection, path: &Path, _ledger: &Ledger, kind: &str, key: &str) -> Result<Vec<Line>> {
    let mut statement = db.prepare_cached("SELECT text FROM lines WHERE kind = ?1 AND key = ?2").map_err(db_error(path))?;
    let rows = statement.query_map(params![kind, key], |row| row.get::<_, String>(0)).map_err(db_error(path))?;
    let mut lines = Vec::new();
    for row in rows {
        let text = row.map_err(db_error(path))?;
        // Checked when it was read: taken back without checking it again.
        if let Some(line) = Line::from_checked(&text) {
            lines.push(line);
        }
    }
    Ok(lines)
}

/// The state of `config`'s ledger with `pending` over it.
pub fn load(config: &Config, ledger: &Ledger, pending: Option<&[Line]>) -> Result<State> {
    let cache = Cache::open(config, ledger)?;
    let entries = cache.state(ledger, None, pending.unwrap_or_default())?;
    Ok(State { entries, warnings: cache.warnings.clone() })
}
