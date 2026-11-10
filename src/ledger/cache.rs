//! The ledger's fold, cached in the user cache beside the lore's index. Each
//! journal is read on from where the last read stopped: the cache keeps, per
//! file, how far it read and a hash of those bytes. A file that got shorter,
//! went away, or whose read part changed broke the append-only rule: that is
//! said by name and the cache is built again from every file. Only whole lines
//! are read; an unfinished last line waits for its LF.
//!
//! One row per entry with its fold and the counts the queries filter on, and
//! beside it where the entry's lines are: file, offset and length in the
//! journals. A journal only grows and its read part is checked by hash, so a
//! place once read stays right; the cache does not hold the lines a second
//! time. Every line's id names its entry, and every parent not (yet) among its
//! entry's lines is kept, so a parent that turns out to be another entry's line
//! is refused whichever arrives first. A rebuild parses the files and folds
//! the entries on every core. What the cache holds is derived and disposable:
//! a row that no longer reads back is a named error and the next command
//! builds the cache again, never an empty state.
use super::fold::{self, Entry, Side};
use super::{journal_author, whole_lines, Line};
use crate::config::{Config, Ledger};
use crate::py::repr_str;
use super::canonical;
use crate::{LoreError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Raised when what the cache holds changes meaning.
const CACHE_FORMAT: &str = "5";
pub const FILE_NAME: &str = "ledger-5.sqlite";
/// Earlier caches' files, removed when the current one is opened.
const OLD_FILES: [&str; 4] = ["ledger.sqlite", "ledger-2.sqlite", "ledger-3.sqlite", "ledger-4.sqlite"];
/// A line's place: file id (u32), offset (u64), length (u32), little-endian.
const PLACE: usize = 16;

pub type Key = (String, String);

pub struct Cache {
    db: Connection,
    pub path: PathBuf,
    /// The ledger's folder, where the places point.
    folder: PathBuf,
    /// What the last refresh found worth saying (rebuilds, stray files, unfinished lines).
    pub warnings: Vec<String>,
    /// Journals that end in an unfinished line, with its length: the state
    /// read is then not known to be complete.
    pub incomplete: Vec<(String, usize)>,
}

/// The whole state, with pending lines over it when asked.
pub struct State {
    pub entries: BTreeMap<Key, Entry>,
    pub warnings: Vec<String>,
}

/// What `entries` reads: everything, or only what a query can use.
pub enum Filter {
    All,
    Conflicted,
    Dangling,
    /// Entries whose stored state holds each of these texts - a prefilter;
    /// the caller checks the values exactly.
    Holding(Vec<String>),
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
    let retired: Vec<String> = ledger.retired_fields.iter().map(|(kind, field)| format!("{kind}.{field}")).chain(ledger.retired_kinds.iter().cloned()).collect();
    canonical(&serde_json::json!({"cache": CACHE_FORMAT, "fold": fold::VERSION, "v": super::VERSION, "dir": ledger.dir, "kinds": kinds, "retired": retired}))
}

/// The refusal for a cache row that does not read back.
fn damaged(what: &str) -> LoreError {
    LoreError::new(format!("the ledger cache is damaged ({what}); it is built again by the next command - run it again"))
}

fn entry_from_json(text: &str) -> Result<Entry> {
    let value: Value = serde_json::from_str(text).map_err(|_| damaged("a stored state is not JSON"))?;
    let object = value.as_object().ok_or_else(|| damaged("a stored state is not an object"))?;
    let strings = |value: Option<&Value>| -> Result<Vec<String>> {
        value
            .and_then(Value::as_array)
            .ok_or_else(|| damaged("a stored list is missing"))?
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(|| damaged("a stored id is not a string")))
            .collect()
    };
    let text = |value: &Value, key: &str| -> Result<String> { value.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| damaged("a stored field is missing")) };
    let mut conflicts = BTreeMap::new();
    for (field, sides) in object.get("conflicts").and_then(Value::as_object).ok_or_else(|| damaged("stored conflicts are missing"))? {
        let mut list = Vec::new();
        for side in sides.as_array().ok_or_else(|| damaged("a stored conflict is not a list"))? {
            list.push(Side { id: text(side, "id")?, by: text(side, "by")?, at: text(side, "at")?, value: side.get("value").cloned().ok_or_else(|| damaged("a stored side has no value"))? });
        }
        conflicts.insert(field.clone(), list);
    }
    let mut dangling = Vec::new();
    for pair in object.get("dangling").and_then(Value::as_array).ok_or_else(|| damaged("stored missing parents are missing"))? {
        dangling.push((text(pair, "line")?, text(pair, "parent")?));
    }
    let set = object.get("set").and_then(Value::as_object).cloned().ok_or_else(|| damaged("a stored set is missing"))?;
    Ok(Entry { set, heads: strings(object.get("heads"))?, conflicts, dangling, order: Vec::new(), cycle: Vec::new(), collisions: Vec::new() })
}

fn place(file: i64, offset: usize, length: usize) -> [u8; PLACE] {
    let mut bytes = [0u8; PLACE];
    bytes[..4].copy_from_slice(&(file as u32).to_le_bytes());
    bytes[4..12].copy_from_slice(&(offset as u64).to_le_bytes());
    bytes[12..].copy_from_slice(&(length as u32).to_le_bytes());
    bytes
}

fn places(blob: &[u8]) -> Result<Vec<(i64, u64, usize)>> {
    if blob.len() % PLACE != 0 {
        return Err(damaged("a stored list of places is cut short"));
    }
    Ok(blob
        .chunks_exact(PLACE)
        .map(|chunk| {
            let number = |range: std::ops::Range<usize>| chunk[range].iter().rev().fold(0u64, |acc, byte| (acc << 8) | *byte as u64);
            (number(0..4) as i64, number(4..12), number(12..16) as usize)
        })
        .collect())
}

/// Reads lines back from their places, one open file per journal.
struct Reader<'a> {
    folder: &'a Path,
    paths: BTreeMap<i64, String>,
    open: BTreeMap<i64, std::fs::File>,
}

impl Reader<'_> {
    fn lines(&mut self, blob: &[u8]) -> Result<Vec<Line>> {
        let all = places(blob)?;
        let mut lines = Vec::with_capacity(all.len());
        let mut at = 0;
        while at < all.len() {
            // A run of places in one journal, each after the one before: read
            // as one span (an entry edited often has many), else one by one.
            let (file, start, _) = all[at];
            let mut end = at + 1;
            while end < all.len() && all[end].0 == file && all[end].1 >= all[end - 1].1 + all[end - 1].2 as u64 && all[end].1 - start < 64 << 20 {
                end += 1;
            }
            if !self.open.contains_key(&file) {
                let relative = self.paths.get(&file).ok_or_else(|| damaged("a place names a journal the cache does not know"))?;
                let handle = std::fs::File::open(self.folder.join(relative)).map_err(|error| LoreError::new(format!("{}: {error}", self.folder.join(relative).display())))?;
                self.open.insert(file, handle);
            }
            let handle = self.open.get_mut(&file).ok_or_else(|| damaged("a journal could not be opened"))?;
            let (_, last_offset, last_length) = all[end - 1];
            let mut span = vec![0u8; (last_offset + last_length as u64 - start) as usize];
            handle.seek(SeekFrom::Start(start)).and_then(|_| handle.read_exact(&mut span)).map_err(|_| damaged("a journal is shorter than its places"))?;
            for &(_, offset, length) in &all[at..end] {
                let from = (offset - start) as usize;
                // Checked when it was first read; a place that no longer holds
                // that line means a journal changed since this command's refresh.
                let line = std::str::from_utf8(&span[from..from + length]).ok().and_then(Line::from_checked).ok_or_else(|| damaged("a place no longer holds a line"))?;
                lines.push(line);
            }
            at = end;
        }
        Ok(lines)
    }
}

/// A new line and where it was read: for the refusals that name file and line.
struct Fresh {
    line: Line,
    place: [u8; PLACE],
    file: String,
    number: usize,
}

impl Cache {
    /// The cache of `config`'s ledger, brought up to date with the journals:
    /// in the user cache (`LORE_CACHE`), else in the system's temporary
    /// folder, else - a sandbox that lets neither be written - in memory,
    /// folded from the journals on every command, with a warning.
    pub fn open(config: &Config, ledger: &Ledger) -> Result<Self> {
        let key = crate::index::repo_key(&config.root);
        let mut tried = Vec::new();
        for dir in [crate::index::cache_dir().join(&key), std::env::temp_dir().join("codelore").join(&key)] {
            let path = dir.join(FILE_NAME);
            let opened = std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display())).and_then(|()| Self::connect(&path).map_err(|error| error.message));
            match opened {
                Ok(db) => {
                    for old in OLD_FILES {
                        for suffix in ["", "-wal", "-shm"] {
                            let _ = std::fs::remove_file(dir.join(format!("{old}{suffix}")));
                        }
                    }
                    let mut cache = Self::with_db(db, &path, &config.root, ledger)?;
                    if !tried.is_empty() {
                        cache.warnings.insert(0, format!("the ledger cache is in {} ({} could not be written)", dir.display(), tried.join("; ")));
                    }
                    return Ok(cache);
                }
                Err(error) => tried.push(error),
            }
        }
        let path = PathBuf::from(":memory:");
        let db = Connection::open_in_memory().map_err(db_error(&path))?;
        Self::schema(&db, &path)?;
        let mut cache = Self::with_db(db, &path, &config.root, ledger)?;
        cache.warnings.insert(
            0,
            format!("no folder for the ledger cache could be written ({}); the ledger is folded in memory, from every journal on every command - set LORE_CACHE to a writable folder", tried.join("; ")),
        );
        Ok(cache)
    }

    pub fn open_at(path: &Path, root: &Path, ledger: &Ledger) -> Result<Self> {
        let db = Self::connect(path)?;
        Self::with_db(db, path, root, ledger)
    }

    /// The cache file opened, its tables made.
    fn connect(path: &Path) -> Result<Connection> {
        let db = Connection::open(path).map_err(db_error(path))?;
        db.busy_timeout(std::time::Duration::from_secs(60)).map_err(db_error(path))?;
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;").map_err(db_error(path))?;
        Self::schema(&db, path)?;
        Ok(db)
    }

    fn schema(db: &Connection, path: &Path) -> Result<()> {
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS files (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
                 lines INTEGER NOT NULL, prefix TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS entries (kind TEXT NOT NULL, key TEXT NOT NULL, state TEXT NOT NULL,
                 conflicts INTEGER NOT NULL, dangling INTEGER NOT NULL, PRIMARY KEY (kind, key));
             CREATE TABLE IF NOT EXISTS bodies (kind TEXT NOT NULL, key TEXT NOT NULL, places BLOB NOT NULL, PRIMARY KEY (kind, key));
             CREATE TABLE IF NOT EXISTS ids (id INTEGER PRIMARY KEY, entry INTEGER NOT NULL) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS missing (parent TEXT NOT NULL, line TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, place TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS missing_parent ON missing (parent);",
        )
        .map_err(db_error(path))
    }

    /// The cache on an open database, brought up to date with the journals.
    fn with_db(db: Connection, path: &Path, root: &Path, ledger: &Ledger) -> Result<Self> {
        let mut cache = Self { db, path: path.to_path_buf(), folder: super::folder(root, ledger), warnings: Vec::new(), incomplete: Vec::new() };
        if let Err(error) = cache.refresh(ledger) {
            if error.message.starts_with("the ledger cache is damaged") {
                cache.invalidate();
            }
            return Err(error);
        }
        Ok(cache)
    }

    /// The next command builds the cache again.
    fn invalidate(&self) {
        let _ = self.db.execute("DELETE FROM meta WHERE key = 'schema'", []);
    }

    fn checked<T>(&self, result: Result<T>) -> Result<T> {
        if result.as_ref().is_err_and(|error| error.message.starts_with("the ledger cache is damaged")) {
            self.invalidate();
        }
        result
    }

    fn reader(&self) -> Result<Reader<'_>> {
        let mut statement = self.db.prepare_cached("SELECT id, path FROM files").map_err(db_error(&self.path))?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))).map_err(db_error(&self.path))?;
        let paths = rows.collect::<std::result::Result<BTreeMap<_, _>, _>>().map_err(db_error(&self.path))?;
        Ok(Reader { folder: &self.folder, paths, open: BTreeMap::new() })
    }

    fn refresh(&mut self, ledger: &Ledger) -> Result<()> {
        let path = self.path.clone();
        let folder = self.folder.clone();
        let shown = |relative: &str| format!("{}/{relative}", ledger.dir);
        let read_file = |relative: &str| std::fs::read(folder.join(relative)).map_err(|error| LoreError::new(format!("{}: {error}; nothing was read", shown(relative))));
        let tx = self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(db_error(&path))?;
        let print = schema_print(ledger);
        let stored_print: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = 'schema'", [], |row| row.get(0)).optional().map_err(db_error(&path))?;
        // path -> (id, size read, mtime, lines, prefix)
        let mut known: BTreeMap<String, (i64, i64, i64, i64, String)> = BTreeMap::new();
        {
            let mut statement = tx.prepare("SELECT path, id, size, mtime, lines, prefix FROM files").map_err(db_error(&path))?;
            let rows = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))))
                .map_err(db_error(&path))?;
            for row in rows {
                let (file, info) = row.map_err(db_error(&path))?;
                known.insert(file, info);
            }
        }
        let mut warnings = Vec::new();
        let mut journals = Vec::new();
        for relative in super::files(&folder)? {
            if journal_author(&relative).is_some() {
                journals.push(relative);
            } else if !relative.ends_with(".lock") && !relative.ends_with(".tmp") && !relative.contains(".jsonl.torn-") {
                warnings.push(format!("{} is not a journal (<author>@<machine>/<YYYY-MM>.jsonl) and is not read", shown(&relative)));
            }
        }
        let mut rebuild = stored_print.as_deref() != Some(print.as_str());
        let mut broken = Vec::new();
        for gone in known.keys().filter(|file| !journals.contains(file)) {
            broken.push(format!("{} is gone", shown(gone)));
        }
        let mut reads: Vec<Read> = Vec::new();
        for relative in &journals {
            let meta = std::fs::metadata(folder.join(relative)).map_err(|error| LoreError::new(format!("{}: {error}; nothing was read", shown(relative))))?;
            let (size, stamp) = (meta.len() as i64, mtime(&meta));
            match known.get(relative) {
                Some((_, old_size, old_stamp, _, _)) if *old_size == size && *old_stamp == stamp => {}
                Some((_, old_size, _, lines, prefix)) => {
                    let bytes = read_file(relative)?;
                    let old = *old_size as usize;
                    if bytes.len() < old {
                        broken.push(format!("{} got shorter", shown(relative)));
                    } else if sha_hex(&bytes[..old]) != *prefix {
                        broken.push(format!("{} changed before its end", shown(relative)));
                    } else {
                        reads.push((relative.clone(), bytes, old, *lines as usize + 1, stamp));
                    }
                }
                None => reads.push((relative.clone(), read_file(relative)?, 0, 1, stamp)),
            }
        }
        if !broken.is_empty() && !rebuild {
            warnings.push(format!("{}: journals are only ever appended to; the ledger cache was built again from every file", broken.join("; ")));
        }
        rebuild |= !broken.is_empty();
        if rebuild {
            tx.execute_batch(
                "DELETE FROM files; DELETE FROM entries; DELETE FROM bodies; DELETE FROM missing;
                 DELETE FROM ids;",
            )
            .map_err(db_error(&path))?;
            known.clear();
            reads.clear();
            for relative in &journals {
                let meta = std::fs::metadata(folder.join(relative)).map_err(|error| LoreError::new(format!("{}: {error}; nothing was read", shown(relative))))?;
                reads.push((relative.clone(), read_file(relative)?, 0, 1, mtime(&meta)));
            }
        }
        // Whole lines only: an unfinished last line is read when its LF comes.
        // A journal whose read part is whole lines and whose size grew past it
        // is read again by every refresh, so this is found every time.
        let mut incomplete = Vec::new();
        for (relative, bytes, from, _, _) in &mut reads {
            let (whole, tail) = whole_lines(&bytes[*from..]);
            let keep = *from + whole.len();
            if tail > 0 {
                warnings.push(format!(
                    "{} ends in an unfinished line ({tail} bytes), not read - a write in progress, a copy cut short or damage; the state is not complete",
                    shown(relative)
                ));
                incomplete.push((shown(relative), tail));
            }
            bytes.truncate(keep);
        }
        let mut errors: Vec<LoreError> = Vec::new();
        let mut fresh: Vec<Fresh> = Vec::new();
        // Parsing and checking the lines is most of a read: the files go in parallel.
        let parsed = parse_all(&reads, ledger, &shown);
        for ((relative, bytes, from, first, stamp), lines) in reads.iter().zip(parsed) {
            let (author, _) = journal_author(relative).unwrap_or_default();
            let place_name = shown(relative);
            let lines = match lines {
                Ok(lines) => lines,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            };
            let foreign: Vec<String> = lines
                .iter()
                .enumerate()
                .filter(|(_, (line, _))| line.by != author)
                .map(|(at, (line, _))| format!("line {}: `by` {} in {author}'s journal", first + at, line.by))
                .collect();
            if !foreign.is_empty() {
                errors.push(LoreError::with(format!("{place_name} is refused"), foreign));
                continue;
            }
            let count = (first - 1 + lines.len()) as i64;
            tx.execute(
                "INSERT INTO files (path, size, mtime, lines, prefix) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (path) DO UPDATE SET size = excluded.size, mtime = excluded.mtime, lines = excluded.lines, prefix = excluded.prefix",
                params![relative, bytes.len() as i64, stamp, count, sha_hex(bytes)],
            )
            .map_err(db_error(&path))?;
            let id: i64 = match known.get(relative) {
                Some((id, ..)) => *id,
                None => tx.query_row("SELECT id FROM files WHERE path = ?1", params![relative], |row| row.get(0)).map_err(db_error(&path))?,
            };
            let mut offset = *from;
            fresh.reserve(lines.len());
            for (at, (line, raw)) in lines.into_iter().enumerate() {
                fresh.push(Fresh { line, place: place(id, offset, raw.len()), file: place_name.clone(), number: first + at });
                offset += raw.len() + 1;
            }
        }
        if !errors.is_empty() {
            return Err(refused(errors));
        }
        // By entry, each entry's lines staying in the order they were read
        // (a stable sort of indices: the lines themselves are not moved about).
        let mut order: Vec<usize> = (0..fresh.len()).collect();
        order.sort_by(|a, b| (fresh[*a].line.kind.as_str(), fresh[*a].line.key.as_str()).cmp(&(fresh[*b].line.kind.as_str(), fresh[*b].line.key.as_str())));
        let mut slots: Vec<Option<Fresh>> = fresh.into_iter().map(Some).collect();
        let mut jobs: Vec<Job> = Vec::new();
        {
            let mut old = tx.prepare_cached("SELECT places FROM bodies WHERE kind = ?1 AND key = ?2").map_err(db_error(&path))?;
            let paths: BTreeMap<i64, String> = {
                let mut statement = tx.prepare("SELECT id, path FROM files").map_err(db_error(&path))?;
                let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))).map_err(db_error(&path))?;
                rows.collect::<std::result::Result<BTreeMap<_, _>, _>>().map_err(db_error(&path))?
            };
            let mut reader = Reader { folder: &folder, paths, open: BTreeMap::new() };
            for at in order {
                let Some(next) = slots[at].take() else { continue };
                if jobs.last().is_some_and(|job: &Job| job.key.0 == next.line.kind && job.key.1 == next.line.key) {
                    if let Some(job) = jobs.last_mut() {
                        job.push(next);
                    }
                    continue;
                }
                let key = next.line.entry();
                let had: Vec<u8> = if rebuild { Vec::new() } else { old.query_row(params![key.0, key.1], |row| row.get(0)).optional().map_err(db_error(&path))?.unwrap_or_default() };
                let lines = if had.is_empty() { Vec::new() } else { reader.lines(&had)? };
                let mut job = Job { key, blob: had, old: lines.len(), lines, new: Vec::new() };
                job.push(next);
                jobs.push(job);
            }
        }
        let folded = fold_many(&jobs);
        let mut wrong = Vec::new();
        for (job, entry) in jobs.iter().zip(&folded) {
            if !entry.cycle.is_empty() {
                wrong.push(format!("{} {}: lines {} are on a cycle of parents", job.key.0, repr_str(&job.key.1), entry.cycle.join(", ")));
            }
            if !entry.collisions.is_empty() {
                wrong.push(format!("{} {}: ids {} each stand for two different lines - an id collision, or a changed copy", job.key.0, repr_str(&job.key.1), entry.collisions.join(", ")));
            }
        }
        if !wrong.is_empty() {
            return Err(LoreError::with("the ledger is refused; nothing in it was read", wrong));
        }
        // The entries first, keeping their row numbers (an upsert, not a
        // replace), so each line's id can name its entry by number.
        let mut rows: Vec<i64> = Vec::with_capacity(jobs.len());
        {
            let mut put = tx
                .prepare_cached(
                    "INSERT INTO entries (kind, key, state, conflicts, dangling) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (kind, key) DO UPDATE SET state = excluded.state, conflicts = excluded.conflicts, dangling = excluded.dangling
                     RETURNING rowid",
                )
                .map_err(db_error(&path))?;
            let mut body = tx.prepare_cached("INSERT OR REPLACE INTO bodies (kind, key, places) VALUES (?1, ?2, ?3)").map_err(db_error(&path))?;
            for (job, entry) in jobs.iter().zip(&folded) {
                let row: i64 = put
                    .query_row(params![job.key.0, job.key.1, canonical(&entry.to_json()), entry.conflicts.len() as i64, entry.dangling.len() as i64], |row| row.get(0))
                    .map_err(db_error(&path))?;
                body.execute(params![job.key.0, job.key.1, job.blob]).map_err(db_error(&path))?;
                rows.push(row);
            }
        }
        // Every new line's id names its entry before any parent is looked up:
        // which file came first must not change the verdict. One id under two
        // entries is two different lines: a collision, refused. Ids go in sorted.
        let mut new_ids: Vec<(i64, usize, usize)> = Vec::new();
        for (at, job) in jobs.iter().enumerate() {
            for (index, line) in job.lines[job.old..].iter().enumerate() {
                new_ids.push((id_number(&line.id), at, index));
            }
        }
        new_ids.sort_unstable();
        {
            let mut owner = tx.prepare_cached("SELECT entry FROM ids WHERE id = ?1").map_err(db_error(&path))?;
            let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO ids (id, entry) VALUES (?1, ?2)").map_err(db_error(&path))?;
            let mut last: Option<(i64, i64)> = None;
            for (number, at, index) in &new_ids {
                let job = &jobs[*at];
                let (file, line_number) = &job.new[*index];
                let id = &job.lines[job.old + index].id;
                let clash = match last {
                    Some((previous, row)) if previous == *number => (row != rows[*at]).then_some(row),
                    _ if !rebuild => owner.query_row(params![number], |row| row.get::<_, i64>(0)).optional().map_err(db_error(&path))?.filter(|row| *row != rows[*at]),
                    _ => None,
                };
                if let Some(row) = clash {
                    let other = entry_named(&tx, &path, row)?;
                    wrong.push(format!("{file}: line {line_number}: id {id} is also a line of {} {} - an id collision", other.0, repr_str(&other.1)));
                }
                insert.execute(params![number, rows[*at]]).map_err(db_error(&path))?;
                last = Some((*number, rows[*at]));
            }
        }
        {
            let mut owner = tx.prepare_cached("SELECT entries.kind, entries.key FROM ids JOIN entries ON entries.rowid = ids.entry WHERE ids.id = ?1").map_err(db_error(&path))?;
            let mut waiting = tx.prepare_cached("SELECT line, kind, key, place FROM missing WHERE parent = ?1").map_err(db_error(&path))?;
            let mut forget = tx.prepare_cached("DELETE FROM missing WHERE parent = ?1 AND kind = ?2 AND key = ?3").map_err(db_error(&path))?;
            let mut wait = tx.prepare_cached("INSERT INTO missing (parent, line, kind, key, place) VALUES (?1, ?2, ?3, ?4, ?5)").map_err(db_error(&path))?;
            for job in &jobs {
                let own: BTreeSet<&str> = job.lines.iter().map(|line| line.id.as_str()).collect();
                for (line, (file, number)) in job.lines[job.old..].iter().zip(&job.new) {
                    // A line that was waiting for this one as its parent.
                    if !rebuild {
                        let rows = waiting
                            .query_map(params![line.id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)))
                            .map_err(db_error(&path))?;
                        for row in rows {
                            let (child, kind, key, at) = row.map_err(db_error(&path))?;
                            if (kind.as_str(), key.as_str()) != (job.key.0.as_str(), job.key.1.as_str()) {
                                wrong.push(format!(
                                    "{at}: line {child} of {kind} {} names parent {}, which {file}: line {number} shows is a line of {} {}",
                                    repr_str(&key),
                                    line.id,
                                    job.key.0,
                                    repr_str(&job.key.1)
                                ));
                            }
                        }
                        forget.execute(params![line.id, job.key.0, job.key.1]).map_err(db_error(&path))?;
                    }
                    for parent in line.parents.iter().filter(|parent| !own.contains(parent.as_str())) {
                        let found: Option<(String, String)> = owner.query_row(params![id_number(parent)], |row| Ok((row.get(0)?, row.get(1)?))).optional().map_err(db_error(&path))?;
                        match found {
                            Some((kind, key)) => wrong.push(format!(
                                "{file}: line {number}: parent {parent} is a line of {kind} {}, not of {} {}",
                                repr_str(&key),
                                job.key.0,
                                repr_str(&job.key.1)
                            )),
                            None => {
                                wait.execute(params![parent, line.id, job.key.0, job.key.1, format!("{file}: line {number}")]).map_err(db_error(&path))?;
                            }
                        }
                    }
                }
            }
        }
        if !wrong.is_empty() {
            return Err(LoreError::with("the ledger is refused; nothing in it was read - a line's id and parents must be its own entry's", wrong));
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
        self.incomplete = incomplete;
        Ok(())
    }

    /// The journal lines of one entry, in the order they were read.
    pub fn lines(&self, _ledger: &Ledger, kind: &str, key: &str) -> Result<Vec<Line>> {
        let blob: Option<Vec<u8>> =
            self.db.query_row("SELECT places FROM bodies WHERE kind = ?1 AND key = ?2", params![kind, key], |row| row.get(0)).optional().map_err(db_error(&self.path))?;
        match blob {
            Some(blob) => {
                let result = self.reader().and_then(|mut reader| reader.lines(&blob));
                self.checked(result)
            }
            None => Ok(Vec::new()),
        }
    }

    /// One entry's fold, from the journals alone.
    pub fn entry(&self, kind: &str, key: &str) -> Result<Option<Entry>> {
        let state: Option<String> =
            self.db.query_row("SELECT state FROM entries WHERE kind = ?1 AND key = ?2", params![kind, key], |row| row.get(0)).optional().map_err(db_error(&self.path))?;
        let result = state.map(|state| entry_from_json(&state)).transpose();
        self.checked(result)
    }

    /// Whether the journals hold an entry.
    pub fn exists(&self, kind: &str, key: &str) -> Result<bool> {
        let found: Option<i64> = self.db.query_row("SELECT 1 FROM entries WHERE kind = ?1 AND key = ?2", params![kind, key], |row| row.get(0)).optional().map_err(db_error(&self.path))?;
        Ok(found.is_some())
    }

    /// The entry a journal line belongs to, when some journal holds it.
    pub fn owner(&self, id: &str) -> Result<Option<Key>> {
        if !super::is_id(id) {
            return Ok(None);
        }
        self.db
            .query_row("SELECT entries.kind, entries.key FROM ids JOIN entries ON entries.rowid = ids.entry WHERE ids.id = ?1", params![id_number(id)], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
            .map_err(db_error(&self.path))
    }

    /// The folds the filter lets through, from the journals alone; of one kind when given.
    pub fn entries(&self, kind: Option<&str>, filter: &Filter) -> Result<BTreeMap<Key, Entry>> {
        let (clause, needles): (&str, &[String]) = match filter {
            Filter::All => ("", &[]),
            Filter::Conflicted => (" AND conflicts > 0", &[]),
            Filter::Dangling => (" AND dangling > 0", &[]),
            Filter::Holding(needles) => ("", needles.as_slice()),
        };
        let mut sql = format!("SELECT kind, key, state FROM entries WHERE (?1 IS NULL OR kind = ?1){clause}");
        for at in 0..needles.len() {
            sql.push_str(&format!(" AND instr(state, ?{}) > 0", at + 2));
        }
        let mut statement = self.db.prepare(&sql).map_err(db_error(&self.path))?;
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&kind];
        for needle in needles {
            values.push(needle);
        }
        let rows = statement.query_map(values.as_slice(), |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))).map_err(db_error(&self.path))?;
        let mut entries = BTreeMap::new();
        for row in rows {
            let (kind, key, state) = row.map_err(db_error(&self.path))?;
            let entry = self.checked(entry_from_json(&state))?;
            entries.insert((kind, key), entry);
        }
        Ok(entries)
    }

    /// (entries, conflicting fields, lines with a parent not here) over the journals.
    pub fn counts(&self) -> Result<(i64, i64, i64)> {
        self.db
            .query_row("SELECT count(*), coalesce(sum(conflicts), 0), coalesce(sum(dangling), 0) FROM entries", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(db_error(&self.path))
    }

    /// The journal lines that name `id` as a parent no journal holds yet:
    /// (line, kind, key, where it is).
    pub fn waiting(&self, id: &str) -> Result<Vec<(String, String, String, String)>> {
        let mut statement = self.db.prepare_cached("SELECT line, kind, key, place FROM missing WHERE parent = ?1").map_err(db_error(&self.path))?;
        let rows = statement.query_map(params![id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).map_err(db_error(&self.path))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_error(&self.path))
    }

    /// Whether the journals hold this line.
    pub fn has_line(&self, id: &str) -> Result<bool> {
        Ok(self.owner(id)?.is_some())
    }

    /// Per journal: (path, bytes read, lines).
    pub fn files(&self) -> Result<Vec<(String, i64, i64)>> {
        let mut statement = self.db.prepare("SELECT path, size, lines FROM files ORDER BY path").map_err(db_error(&self.path))?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).map_err(db_error(&self.path))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_error(&self.path))
    }

    /// The state with `pending` lines read over the journals: the entries they
    /// touch are folded again from both. The caller still applies its filter:
    /// an entry pending lines touch is in the answer whatever the filter says.
    pub fn state(&self, ledger: &Ledger, kind: Option<&str>, filter: &Filter, pending: &[Line]) -> Result<BTreeMap<Key, Entry>> {
        let mut entries = self.entries(kind, filter)?;
        let mut extra: BTreeMap<Key, Vec<Line>> = BTreeMap::new();
        for line in pending.iter().filter(|line| kind.is_none_or(|kind| line.kind == kind)) {
            extra.entry(line.entry()).or_default().push(line.clone());
        }
        for ((kind, key), more) in extra {
            let mut lines = self.lines(ledger, &kind, &key)?;
            lines.extend(more);
            let entry = super::fold_checked(&kind, &key, &lines)?;
            entries.insert((kind, key), entry);
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
        super::fold_checked(kind, key, &lines).map(Some)
    }
}

/// A line id as the 64-bit number its 16 hex digits are.
fn id_number(id: &str) -> i64 {
    u64::from_str_radix(id, 16).unwrap_or(0) as i64
}

/// The kind and key of the entry with this row number.
fn entry_named(db: &Connection, path: &Path, row: i64) -> Result<Key> {
    db.query_row("SELECT kind, key FROM entries WHERE rowid = ?1", params![row], |row| Ok((row.get(0)?, row.get(1)?))).map_err(db_error(path))
}

/// One entry a refresh folds again: the lines it had, then the new ones.
struct Job {
    key: Key,
    blob: Vec<u8>,
    lines: Vec<Line>,
    /// How many of `lines` were there before.
    old: usize,
    /// (file, line number) of each new line, in step with `lines[old..]`.
    new: Vec<(String, usize)>,
}

impl Job {
    fn push(&mut self, fresh: Fresh) {
        self.blob.extend_from_slice(&fresh.place);
        self.lines.push(fresh.line);
        self.new.push((fresh.file, fresh.number));
    }
}

fn refused(errors: Vec<LoreError>) -> LoreError {
    let mut details = Vec::new();
    for error in &errors {
        details.push(error.message.clone());
        details.extend(error.details.iter().map(|detail| format!("  {detail}")));
    }
    LoreError::with("the ledger is refused; nothing in it was read", details)
}

/// (file, bytes, first byte to parse, first line number, mtime)
type Read = (String, Vec<u8>, usize, usize, i64);

fn parse_all(reads: &[Read], ledger: &Ledger, shown: &(dyn Fn(&str) -> String + Sync)) -> Vec<Result<Vec<(Line, String)>>> {
    let jobs: Vec<(String, &[u8], usize)> = reads.iter().map(|(relative, bytes, from, first, _)| (shown(relative), &bytes[*from..], *first)).collect();
    super::parse_many(&jobs, ledger)
}

/// The folds of many entries, on as many threads as there are cores.
fn fold_many(jobs: &[Job]) -> Vec<Entry> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    if threads <= 1 || jobs.len() < 2_000 {
        return jobs.iter().map(|job| fold::fold(&job.lines)).collect();
    }
    let chunk = jobs.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = jobs.chunks(chunk).map(|part| scope.spawn(move || part.iter().map(|job| fold::fold(&job.lines)).collect::<Vec<_>>())).collect();
        handles.into_iter().flat_map(|handle| handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))).collect()
    })
}

/// The state of `config`'s ledger with `pending` over it.
pub fn load(config: &Config, ledger: &Ledger, pending: Option<&[Line]>) -> Result<State> {
    let cache = Cache::open(config, ledger)?;
    let entries = cache.state(ledger, None, &Filter::All, pending.unwrap_or_default())?;
    Ok(State { entries, warnings: cache.warnings.clone() })
}
