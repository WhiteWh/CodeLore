//! The working tier (docs/lore.md section 11): a fast memory beside the
//! durable lore that expires by itself. Pulses (what a member does), messages
//! (a bus between members, the lead and the owner) and briefs (what a fresh
//! session needs to take a task up) live in one SQLite file in the store's
//! private folder, never in JSONL, never in git and never in the replica.
//!
//! Every function takes `now` (UTC seconds), so expiry is tested with a fake
//! clock. A read never waits for the cleanup and never returns an entry past
//! its `expires`; the cleanup is bounded (500 rows a pass, transactions of at
//! most 50 ms) and takes its own lock, never the writer's.
use crate::config::Working as Settings;
use crate::index::{terms_of, word_re};
use crate::secrets;
use crate::{LoreError, Result};
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const KINDS: [&str; 3] = ["pulse", "message", "brief"];
const DAY: i64 = 86_400;
/// A pulse by the same writer and task within this many seconds replaces it.
const REPLACE_WITHIN: i64 = 300;
pub const PULSE_FIELD: usize = 500;
pub const MESSAGE_TEXT: usize = 2_000;
pub const BRIEF_BYTES: usize = 8 * 1024;
/// At most this many rows one cleanup pass deletes.
pub const PASS_ROWS: usize = 500;
/// At most this long one cleanup transaction stays open.
pub const PASS_TRANSACTION: Duration = Duration::from_millis(50);
/// The cleanup runs again this long after the last pass.
pub const CLEANUP_EVERY: i64 = 600;
/// A normal doorbell rings a recipient at most once in this many seconds.
pub const RING_COALESCE: i64 = 300;
/// Journal events kept in the file.
const EVENTS_KEPT: i64 = 1_000;
/// 2 (task 187): the search tables' rows carry their entry's rowid, so a
/// delete finds them by rowid instead of scanning the UNINDEXED id.
const USER_VERSION: i64 = 2;

/// The folder beside the store that git never sees (11.5): `.lore-private`.
pub const PRIVATE_DIR: &str = ".lore-private";
pub const FILE_NAME: &str = "working.sqlite";

fn error(error: impl std::fmt::Display) -> LoreError {
    LoreError::new(format!("working tier: {error}"))
}

/// How expiry learns a task's life (11.14). Zerus supplies its teams' tasks;
/// standalone lore reads them from the plans' items.
pub trait TaskClock {
    /// When the task closed (accepted, cancelled, or the team's finish), or
    /// None while it is open or unknown.
    fn closed_at(&self, team: &str, task: &str) -> Option<i64>;
    /// Whether its answers are to be applied at all (task 187): a clock that
    /// knows nothing must not reopen the tasks the store recorded as closed.
    fn knows(&self) -> bool {
        true
    }
}

/// No task ever closes.
pub struct OpenClock;
impl TaskClock for OpenClock {
    fn closed_at(&self, _team: &str, _task: &str) -> Option<i64> {
        None
    }
}

/// Task 187: a clock that knows no task. A cleanup with it keeps every
/// recorded close as it is (the store's own `tasks` table), where `OpenClock`
/// would reopen them all: the cleanup a full file runs before a write.
pub struct UnknownClock;
impl TaskClock for UnknownClock {
    fn closed_at(&self, _team: &str, _task: &str) -> Option<i64> {
        None
    }
    fn knows(&self) -> bool {
        false
    }
}

/// Standalone lore's clock: a task reference is a plan item id; it closed when
/// the item is done, dropped or superseded, at the start of its `updated` day.
pub struct PlanClock {
    closed: BTreeMap<String, i64>,
}

impl PlanClock {
    pub fn from_records(records: &[Value]) -> Self {
        let mut closed = BTreeMap::new();
        for record in records {
            if record.get("kind").and_then(Value::as_str) != Some("item") {
                continue;
            }
            if !matches!(record.get("status").and_then(Value::as_str), Some("done" | "dropped" | "superseded")) {
                continue;
            }
            let id = record.get("id").and_then(Value::as_str).unwrap_or_default();
            let day = record.get("updated").and_then(Value::as_str).unwrap_or_default();
            if let Ok(date) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") {
                let at = date.and_hms_opt(0, 0, 0).map(|time| time.and_utc().timestamp()).unwrap_or(0);
                closed.insert(id.to_owned(), at);
            }
        }
        Self { closed }
    }
}

impl TaskClock for PlanClock {
    fn closed_at(&self, _team: &str, task: &str) -> Option<i64> {
        self.closed.get(task.trim_start_matches('#')).or_else(|| self.closed.get(task)).copied()
    }
}

/// The delivery of a message's doorbell (11.11), which only Zerus can do.
pub trait Doorbell {
    /// Rings `recipient` with the one-line `text`; answers the safe point it
    /// was delivered at (`idle`, `hook`, `turn_end`), or None when it waits.
    fn ring(&self, recipient: &str, text: &str, urgent: bool) -> Option<String>;
}

/// Who reads (11.6): a member sees its team's pulses and briefs and the
/// messages to it, to `team` or from it; the lead all of its team's; the
/// owner every team's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Viewer {
    pub team: Option<String>,
    pub name: String,
    pub role: Role,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Member,
    Lead,
    Owner,
}

impl Viewer {
    pub fn owner() -> Self {
        Self { team: None, name: "owner".into(), role: Role::Owner }
    }

    fn sees(&self, entry: &Entry) -> bool {
        if self.role == Role::Owner && self.team.is_none() {
            return true;
        }
        if self.team.as_deref().is_some_and(|team| team != entry.team) {
            return false;
        }
        match (self.role, entry.kind.as_str()) {
            (Role::Owner | Role::Lead, _) => true,
            (Role::Member, "message") => {
                let to = entry.to.as_deref().unwrap_or_default();
                to == self.name || to == "team" || entry.by == self.name
            }
            (Role::Member, _) => true,
        }
    }
}

/// One working entry, as stored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub id: String,
    pub kind: String,
    pub team: String,
    pub by: String,
    pub at: i64,
    pub expires: Option<i64>,
    pub promoted: String,
    pub task: String,
    pub done: String,
    pub next: String,
    pub blocked: String,
    pub to: Option<String>,
    pub text: String,
    pub urgency: String,
    pub state: String,
    pub rung_at: Option<i64>,
    pub read_at: Option<i64>,
    pub safe_point: String,
    /// A message to `team`: who read it, when.
    pub read_by: BTreeMap<String, i64>,
    /// A message to `team`: who must read it, when known.
    pub recipients: Vec<String>,
    pub version: i64,
}

impl Entry {
    /// `{"id","kind","team","by","at","expires","promoted", …the kind's fields}` (11.13).
    pub fn json(&self) -> Value {
        let mut map = Map::new();
        map.insert("id".into(), json!(self.id));
        map.insert("kind".into(), json!(self.kind));
        map.insert("team".into(), json!(self.team));
        map.insert("by".into(), json!(self.by));
        map.insert("at".into(), json!(self.at));
        map.insert("expires".into(), json!(self.expires));
        map.insert("promoted".into(), json!(self.promoted));
        match self.kind.as_str() {
            "pulse" => {
                map.insert("task".into(), json!(self.task));
                map.insert("done".into(), json!(self.done));
                map.insert("next".into(), json!(self.next));
                map.insert("blocked".into(), json!(self.blocked));
            }
            "message" => {
                map.insert("to".into(), json!(self.to));
                map.insert("from".into(), json!(self.by));
                map.insert("text".into(), json!(self.text));
                map.insert("urgency".into(), json!(self.urgency));
                map.insert("state".into(), json!(self.state));
                map.insert("rung_at".into(), json!(self.rung_at));
                map.insert("read_at".into(), json!(self.read_at));
                map.insert("safe_point".into(), json!(self.safe_point));
                if self.to.as_deref() == Some("team") {
                    map.insert("read_by".into(), json!(self.read_by));
                }
            }
            _ => {
                map.insert("task".into(), json!(self.task));
                map.insert("version".into(), json!(self.version));
                map.insert("text".into(), json!(self.text));
            }
        }
        Value::Object(map)
    }

    /// The text the working FTS tables hold.
    fn searchable(&self) -> String {
        [self.text.as_str(), &self.done, &self.next, &self.blocked, self.to.as_deref().unwrap_or_default(), &self.by, &self.task].join("\n")
    }

    fn unread(&self) -> bool {
        self.kind == "message" && self.state != "read"
    }
}

/// A pulse to write.
#[derive(Clone, Debug, Default)]
pub struct Pulse {
    pub team: String,
    pub by: String,
    pub task: String,
    pub done: String,
    pub next: String,
    pub blocked: String,
}

/// A message to send.
#[derive(Clone, Debug, Default)]
pub struct Message {
    pub team: String,
    pub by: String,
    pub to: String,
    pub text: String,
    pub urgent: bool,
    /// For a message to `team`: who must read it (when the caller knows).
    pub recipients: Vec<String>,
}

/// What a write answers besides the entry: notes (a redaction, a replace).
#[derive(Clone, Debug, Default)]
pub struct Written {
    pub entry: Entry,
    pub notes: Vec<String>,
}

/// What one cleanup pass did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cleaned {
    pub deleted: usize,
    pub bytes_freed: i64,
    /// More waits: the next pass continues.
    pub more: bool,
    /// The pass was skipped: another one holds the working lock.
    pub busy: bool,
    pub events: Vec<Value>,
}

/// The store's working tier.
pub struct Store {
    db: Connection,
    path: PathBuf,
    lock_path: PathBuf,
    settings: Settings,
    allow: Vec<String>,
    /// The longest cleanup transaction (11.4: 50 ms); a test lowers it.
    budget: std::cell::Cell<Duration>,
}

/// `<root>/.lore-private`, excluded from git through `info/exclude` so no
/// commit ever stages it (11.5).
pub fn private_dir(root: &Path) -> Result<PathBuf> {
    let dir = root.join(PRIVATE_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| LoreError::new(format!("{}: {e}", dir.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    exclude(root);
    Ok(dir)
}

/// Adds `/.lore-private/` to the repository's `info/exclude` once.
fn exclude(root: &Path) {
    let Ok(path) = crate::gitio::out(root, &["rev-parse", "--git-path", "info/exclude"], None, &[]) else { return };
    let path = PathBuf::from(path);
    let path = if path.is_absolute() { path } else { root.join(path) };
    let line = format!("/{PRIVATE_DIR}/");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current.lines().any(|existing| existing.trim() == line) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let joiner = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
    let _ = std::fs::write(&path, format!("{current}{joiner}{line}\n"));
}

fn new_id(kind: &str, salt: &str) -> String {
    use sha1::{Digest, Sha1};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let digest = Sha1::digest(format!("{nanos}:{count}:{}:{salt}", std::process::id()).as_bytes());
    let hex: String = digest.iter().take(4).map(|byte| format!("{byte:02x}")).collect();
    format!("w-{kind}-{hex}")
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

const COLUMNS: &str = "id, kind, team, by, at, expires, promoted, task, done, next, blocked, to_, text, urgency, state, rung_at, read_at, safe_point, read_by, recipients, version";

fn row_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let read_by: String = row.get(18)?;
    let recipients: String = row.get(19)?;
    Ok(Entry {
        id: row.get(0)?,
        kind: row.get(1)?,
        team: row.get(2)?,
        by: row.get(3)?,
        at: row.get(4)?,
        expires: row.get(5)?,
        promoted: row.get(6)?,
        task: row.get(7)?,
        done: row.get(8)?,
        next: row.get(9)?,
        blocked: row.get(10)?,
        to: row.get(11)?,
        text: row.get(12)?,
        urgency: row.get(13)?,
        state: row.get(14)?,
        rung_at: row.get(15)?,
        read_at: row.get(16)?,
        safe_point: row.get(17)?,
        read_by: serde_json::from_str(&read_by).unwrap_or_default(),
        recipients: serde_json::from_str(&recipients).unwrap_or_default(),
        version: row.get(20)?,
    })
}

impl Store {
    /// Opens (or creates) `<root>/.lore-private/working.sqlite`.
    pub fn open(root: &Path, settings: &Settings, allow: &[String]) -> Result<Self> {
        let dir = private_dir(root)?;
        Self::open_at(&dir.join(FILE_NAME), settings, allow)
    }

    pub fn open_at(path: &Path, settings: &Settings, allow: &[String]) -> Result<Self> {
        let db = Connection::open(path).map_err(|e| LoreError::new(format!("{}: {e}", path.display())))?;
        db.busy_timeout(Duration::from_secs(10)).map_err(error)?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(error)?;
        if version == 0 {
            // auto_vacuum must be set before the first table exists.
            db.execute_batch(
                r#"PRAGMA auto_vacuum = INCREMENTAL;
                PRAGMA journal_mode = WAL;
                CREATE TABLE IF NOT EXISTS entries(
                    id TEXT PRIMARY KEY, kind TEXT NOT NULL, team TEXT NOT NULL DEFAULT '', by TEXT NOT NULL,
                    at INTEGER NOT NULL, expires INTEGER, promoted TEXT NOT NULL DEFAULT '',
                    task TEXT NOT NULL DEFAULT '', done TEXT NOT NULL DEFAULT '', next TEXT NOT NULL DEFAULT '',
                    blocked TEXT NOT NULL DEFAULT '', to_ TEXT, text TEXT NOT NULL DEFAULT '',
                    urgency TEXT NOT NULL DEFAULT '', state TEXT NOT NULL DEFAULT '', rung_at INTEGER, read_at INTEGER,
                    safe_point TEXT NOT NULL DEFAULT '', read_by TEXT NOT NULL DEFAULT '{}',
                    recipients TEXT NOT NULL DEFAULT '[]', version INTEGER NOT NULL DEFAULT 0);
                CREATE INDEX IF NOT EXISTS entries_kind ON entries(kind, team, at);
                CREATE INDEX IF NOT EXISTS entries_expires ON entries(expires);
                CREATE INDEX IF NOT EXISTS entries_task ON entries(team, task);
                CREATE INDEX IF NOT EXISTS entries_to ON entries(to_, state);
                CREATE VIRTUAL TABLE IF NOT EXISTS words USING fts5(id UNINDEXED, main,
                    tokenize = "porter unicode61 remove_diacritics 2 tokenchars '_'");
                CREATE VIRTUAL TABLE IF NOT EXISTS terms USING fts5(id UNINDEXED, toks,
                    tokenize = "unicode61 remove_diacritics 0 tokenchars '_./:-'");
                CREATE TABLE IF NOT EXISTS tasks(team TEXT NOT NULL, task TEXT NOT NULL, closed_at INTEGER,
                    PRIMARY KEY(team, task));
                CREATE TABLE IF NOT EXISTS events(n INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL,
                    kind TEXT NOT NULL, body TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);"#,
            )
            .map_err(error)?;
            db.execute_batch(&format!("PRAGMA user_version = {USER_VERSION}")).map_err(error)?;
        } else if version > USER_VERSION {
            return Err(error(format!("{} was written by a newer lore (version {version})", path.display())));
        } else {
            db.execute_batch("PRAGMA journal_mode = WAL").map_err(error)?;
        }
        let lock_path = path.with_extension("lock");
        let store = Self { db, path: path.to_path_buf(), lock_path, settings: settings.clone(), allow: allow.to_vec(), budget: std::cell::Cell::new(PASS_TRANSACTION) };
        if (1..USER_VERSION).contains(&version) {
            store.reindex_by_rowid()?;
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Task 187: a version 1 store's search rows are rebuilt keyed by their
    /// entry's rowid, once, in one transaction (at most `entries` rows).
    fn reindex_by_rowid(&self) -> Result<()> {
        self.db.execute_batch("BEGIN IMMEDIATE").map_err(error)?;
        let result = (|| -> Result<()> {
            // A concurrent opener may have done it meanwhile.
            let version: i64 = self.db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(error)?;
            if version >= USER_VERSION {
                return Ok(());
            }
            self.db.execute_batch("DELETE FROM words; DELETE FROM terms;").map_err(error)?;
            for entry in self.select("1 = 1", &[])? {
                self.index(&entry)?;
            }
            self.db.execute_batch(&format!("PRAGMA user_version = {USER_VERSION}")).map_err(error)?;
            Ok(())
        })();
        match result {
            Ok(()) => self.db.execute_batch("COMMIT").map_err(error),
            Err(failure) => {
                let _ = self.db.execute_batch("ROLLBACK");
                Err(failure)
            }
        }
    }

    fn file_bytes(&self) -> u64 {
        let wal = self.path.with_extension("sqlite-wal");
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0) + std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0)
    }

    fn meta(&self, key: &str) -> Option<String> {
        self.db.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0)).optional().ok().flatten()
    }

    fn set_meta(&self, key: &str, value: &str) {
        let _ = self.db.execute("INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value]);
    }

    /// Records a journal event in the file (bounded); Zerus forwards them.
    pub fn event(&self, now: i64, kind: &str, body: Value) {
        let _ = self.db.execute("INSERT INTO events(at, kind, body) VALUES(?1, ?2, ?3)", params![now, kind, body.to_string()]);
        let _ = self.db.execute("DELETE FROM events WHERE n <= (SELECT MAX(n) FROM events) - ?1", [EVENTS_KEPT]);
    }

    /// Journal events after `after` (an event number), oldest first.
    pub fn events(&self, after: i64, limit: usize) -> Result<Vec<Value>> {
        let mut statement = self.db.prepare("SELECT n, at, kind, body FROM events WHERE n > ?1 ORDER BY n LIMIT ?2").map_err(error)?;
        let rows = statement
            .query_map(params![after, limit as i64], |row| {
                let body: String = row.get(3)?;
                Ok(json!({"n": row.get::<_, i64>(0)?, "at": row.get::<_, i64>(1)?, "kind": row.get::<_, String>(2)?,
                    "body": serde_json::from_str::<Value>(&body).unwrap_or(Value::Null)}))
            })
            .map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    fn insert(&self, entry: &Entry) -> Result<()> {
        self.db
            .execute(
                &format!("INSERT INTO entries({COLUMNS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)"),
                params![
                    entry.id, entry.kind, entry.team, entry.by, entry.at, entry.expires, entry.promoted, entry.task, entry.done, entry.next,
                    entry.blocked, entry.to, entry.text, entry.urgency, entry.state, entry.rung_at, entry.read_at, entry.safe_point,
                    serde_json::to_string(&entry.read_by).unwrap_or_else(|_| "{}".into()),
                    serde_json::to_string(&entry.recipients).unwrap_or_else(|_| "[]".into()),
                    entry.version
                ],
            )
            .map_err(error)?;
        self.index(entry)
    }

    /// The entry's rowid, which keys its search rows (task 187).
    fn rowid(&self, id: &str) -> rusqlite::Result<Option<i64>> {
        self.db.query_row("SELECT rowid FROM entries WHERE id = ?1", [id], |row| row.get(0)).optional()
    }

    fn index(&self, entry: &Entry) -> Result<()> {
        let text = entry.searchable();
        let rowid = self.rowid(&entry.id).map_err(error)?.ok_or_else(|| error(format!("{} is not stored", entry.id)))?;
        // By rowid: an FTS rowid lookup, never a scan of the UNINDEXED id.
        self.db.execute("DELETE FROM words WHERE rowid = ?1", [rowid]).map_err(error)?;
        self.db.execute("DELETE FROM terms WHERE rowid = ?1", [rowid]).map_err(error)?;
        self.db.execute("INSERT INTO words(rowid, id, main) VALUES(?1, ?2, ?3)", params![rowid, entry.id, text]).map_err(error)?;
        self.db.execute("INSERT INTO terms(rowid, id, toks) VALUES(?1, ?2, ?3)", params![rowid, entry.id, terms_of(&text).join(" ")]).map_err(error)?;
        Ok(())
    }

    fn delete_rows(&self, ids: &[String]) -> rusqlite::Result<()> {
        for id in ids {
            self.db.execute("DELETE FROM meta WHERE key = ?1", [format!("told:{id}")])?;
            if let Some(rowid) = self.rowid(id)? {
                self.db.execute("DELETE FROM words WHERE rowid = ?1", [rowid])?;
                self.db.execute("DELETE FROM terms WHERE rowid = ?1", [rowid])?;
            }
            self.db.execute("DELETE FROM entries WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    /// An entry by id, while it has not expired.
    pub fn get(&self, id: &str, now: i64) -> Result<Option<Entry>> {
        self.db
            .query_row(&format!("SELECT {COLUMNS} FROM entries WHERE id = ?1 AND (expires IS NULL OR expires > ?2)"), params![id, now], row_entry)
            .optional()
            .map_err(error)
    }

    fn select(&self, sql_where: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Entry>> {
        let sql = format!("SELECT {COLUMNS} FROM entries WHERE {sql_where}");
        let mut statement = self.db.prepare(&sql).map_err(error)?;
        let rows = statement.query_map(args, row_entry).map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    // -- the task clock and expiry ---------------------------------------------------

    fn task_closed(&self, team: &str, task: &str) -> Option<i64> {
        self.db
            .query_row("SELECT closed_at FROM tasks WHERE team = ?1 AND task = ?2", params![team, task], |row| row.get::<_, Option<i64>>(0))
            .optional()
            .ok()
            .flatten()
            .flatten()
    }

    /// The expiry of an entry by the rules of 11.3.
    fn expiry(&self, entry: &Entry, latest_brief: bool) -> Option<i64> {
        let s = &self.settings;
        match entry.kind.as_str() {
            "pulse" if entry.task.is_empty() => Some(entry.at + s.pulse_days * DAY),
            "pulse" => self.task_closed(&entry.team, &entry.task).map(|closed| closed + s.pulse_days * DAY),
            "message" => {
                if entry.state == "read" {
                    let last = entry.read_by.values().copied().max().or(entry.read_at).unwrap_or(entry.at);
                    Some(last + s.message_days * DAY)
                } else {
                    Some(entry.at + s.unread_days * DAY)
                }
            }
            _ if latest_brief => None,
            _ => self.task_closed(&entry.team, &entry.task),
        }
    }

    fn latest_brief_version(&self, team: &str, task: &str) -> i64 {
        self.db
            .query_row("SELECT COALESCE(MAX(version), 0) FROM entries WHERE kind = 'brief' AND team = ?1 AND task = ?2", params![team, task], |row| row.get(0))
            .unwrap_or(0)
    }

    /// Recomputes the expiry of every entry of one task (a close, a reopen).
    fn refresh_task(&self, team: &str, task: &str) -> Result<()> {
        let latest = self.latest_brief_version(team, task);
        for entry in self.select("team = ?1 AND task = ?2", &[&team, &task])? {
            let expires = self.expiry(&entry, entry.kind == "brief" && entry.version == latest);
            if expires != entry.expires {
                self.db.execute("UPDATE entries SET expires = ?1 WHERE id = ?2", params![expires, entry.id]).map_err(error)?;
            }
        }
        Ok(())
    }

    /// Records that a task closed at `at` (or reopened, with None): its pulses
    /// and briefs get their expiry, or lose it again (11.3).
    pub fn set_task_closed(&self, team: &str, task: &str, at: Option<i64>) -> Result<()> {
        self.db
            .execute(
                "INSERT INTO tasks(team, task, closed_at) VALUES(?1, ?2, ?3) ON CONFLICT(team, task) DO UPDATE SET closed_at = excluded.closed_at",
                params![team, task, at],
            )
            .map_err(error)?;
        self.refresh_task(team, task)
    }

    /// Asks the clock about the tasks working entries wait on, at most `limit`.
    fn sync_clock(&self, clock: &dyn TaskClock, limit: usize) -> Result<()> {
        // Task 187: a clock that knows nothing changes no recorded close.
        if !clock.knows() {
            return Ok(());
        }
        let mut statement = self.db.prepare("SELECT DISTINCT team, task FROM entries WHERE task <> '' LIMIT ?1").map_err(error)?;
        let tasks: Vec<(String, String)> = statement
            .query_map([limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(error)?;
        for (team, task) in tasks {
            let now = clock.closed_at(&team, &task);
            if now != self.task_closed(&team, &task) {
                self.set_task_closed(&team, &task, now)?;
            }
        }
        Ok(())
    }

    // -- writes ------------------------------------------------------------------

    fn redact(&self, text: &str, field: &str, notes: &mut Vec<String>) -> String {
        let (clean, names) = secrets::redact(text, &self.allow);
        if !names.is_empty() {
            notes.push(format!("`{field}` held what looks like a secret ({}); it was stored redacted", names.join(", ")));
        }
        clean
    }

    fn full(&self, now: i64) -> Result<()> {
        if self.file_bytes() > self.settings.file_bytes {
            // Task 187: with a clock that knows nothing, closed tasks stay
            // closed (OpenClock reopened them all and so freed nothing).
            let _ = self.cleanup(now, &UnknownClock);
            if self.file_bytes() > self.settings.file_bytes {
                let mib = self.settings.file_bytes / (1024 * 1024);
                self.event(now, "working_full", json!({"bytes": self.file_bytes()}));
                return Err(LoreError::new(format!("the working tier is full ({mib} MiB)")));
            }
        }
        Ok(())
    }

    /// Writes a pulse (11.2): replaces the same writer's pulse of the same
    /// task within 5 minutes in place.
    pub fn pulse(&self, pulse: &Pulse, now: i64) -> Result<Written> {
        for (name, value) in [("done", &pulse.done), ("next", &pulse.next), ("blocked", &pulse.blocked)] {
            if chars(value) > PULSE_FIELD {
                return Err(LoreError::new(format!("refused: `{name}` has {} characters; a pulse field holds at most {PULSE_FIELD}", chars(value))));
            }
        }
        if pulse.done.trim().is_empty() && pulse.next.trim().is_empty() && pulse.blocked.trim().is_empty() {
            return Err(LoreError::new("refused: a pulse says at least one of `done`, `next` or `blocked`"));
        }
        self.full(now)?;
        let mut notes = Vec::new();
        let done = self.redact(&pulse.done, "done", &mut notes);
        let next = self.redact(&pulse.next, "next", &mut notes);
        let blocked = self.redact(&pulse.blocked, "blocked", &mut notes);
        let previous = self
            .select(
                "kind = 'pulse' AND team = ?1 AND by = ?2 AND task = ?3 AND at >= ?4 ORDER BY at DESC LIMIT 1",
                &[&pulse.team, &pulse.by, &pulse.task, &(now - REPLACE_WITHIN)],
            )?
            .pop();
        let mut entry = previous.clone().unwrap_or_else(|| Entry {
            id: new_id("pulse", &pulse.by),
            kind: "pulse".into(),
            team: pulse.team.clone(),
            by: pulse.by.clone(),
            task: pulse.task.clone(),
            ..Entry::default()
        });
        entry.at = now;
        entry.done = done;
        entry.next = next;
        entry.blocked = blocked;
        entry.expires = self.expiry(&entry, false);
        if previous.is_some() {
            self.db
                .execute(
                    "UPDATE entries SET at = ?1, done = ?2, next = ?3, blocked = ?4, expires = ?5 WHERE id = ?6",
                    params![entry.at, entry.done, entry.next, entry.blocked, entry.expires, entry.id],
                )
                .map_err(error)?;
            self.index(&entry)?;
            notes.push(format!("replaced {}, written {} s before", entry.id, now - previous.map(|p| p.at).unwrap_or(now)));
        } else {
            self.insert(&entry)?;
        }
        Ok(Written { entry, notes })
    }

    /// The unread messages waiting for `recipient` at an urgency.
    fn unread_count(&self, recipient: &str, urgent: bool) -> Result<usize> {
        let urgency = if urgent { "urgent" } else { "normal" };
        self.db
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE kind = 'message' AND to_ = ?1 AND state <> 'read' AND urgency = ?2",
                params![recipient, urgency],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .map_err(error)
    }

    /// Sends a message (11.2, 11.11). The doorbell, when there is one, rings
    /// an urgent message at once and a normal one at most every 5 minutes.
    pub fn send(&self, message: &Message, doorbell: Option<&dyn Doorbell>, now: i64) -> Result<Written> {
        if message.to.trim().is_empty() {
            return Err(LoreError::new("refused: a message names its recipient (`--to`): a member, lead, team or owner"));
        }
        if message.text.trim().is_empty() {
            return Err(LoreError::new("refused: the message is empty"));
        }
        if chars(&message.text) > MESSAGE_TEXT {
            return Err(LoreError::new(format!("refused: the message has {} characters; a message holds at most {MESSAGE_TEXT}", chars(&message.text))));
        }
        let cap = if message.urgent { self.settings.unread_urgent } else { self.settings.unread_normal };
        let waiting = self.unread_count(&message.to, message.urgent)?;
        if waiting >= cap {
            let urgency = if message.urgent { "urgent" } else { "normal" };
            self.event(now, "working_unread_cap", json!({"to": message.to, "from": message.by, "urgency": urgency, "count": waiting}));
            return Err(LoreError::new(format!(
                "refused: {} has {waiting} unread {urgency} messages, the most it may have; it does not read them (the lead is told)",
                message.to
            )));
        }
        self.full(now)?;
        let mut notes = Vec::new();
        let text = self.redact(&message.text, "text", &mut notes);
        let mut entry = Entry {
            id: new_id("message", &message.by),
            kind: "message".into(),
            team: message.team.clone(),
            by: message.by.clone(),
            at: now,
            to: Some(message.to.clone()),
            text,
            urgency: if message.urgent { "urgent".into() } else { "normal".into() },
            state: "queued".into(),
            recipients: message.recipients.clone(),
            ..Entry::default()
        };
        entry.expires = self.expiry(&entry, false);
        self.insert(&entry)?;
        if let Some(doorbell) = doorbell {
            self.ring(&mut entry, doorbell, now)?;
        } else if message.urgent {
            notes.push("no doorbell here: the urgent message is marked and waits until it is read".into());
        }
        Ok(Written { entry, notes })
    }

    /// The doorbell's one line (11.11), at most 200 characters.
    pub fn doorbell_text(entry: &Entry) -> String {
        let urgency = if entry.urgency == "urgent" { "URGENT" } else { "normal" };
        let text = format!("[Zerus lore · message · from {} · {urgency} · {}] read it with hgs lore messages --unread", entry.by, entry.id);
        text.chars().take(200).collect()
    }

    fn ring(&self, entry: &mut Entry, doorbell: &dyn Doorbell, now: i64) -> Result<()> {
        let recipient = entry.to.clone().unwrap_or_default();
        let key = format!("rung:{recipient}");
        if entry.urgency != "urgent" {
            let last = self.meta(&key).and_then(|value| value.parse::<i64>().ok()).unwrap_or(i64::MIN / 2);
            if now - last < RING_COALESCE {
                return Ok(()); // coalesced: the earlier doorbell names the count
            }
        }
        if let Some(point) = doorbell.ring(&recipient, &Self::doorbell_text(entry), entry.urgency == "urgent") {
            entry.state = "rung".into();
            entry.rung_at = Some(now);
            entry.safe_point = point;
            self.db
                .execute("UPDATE entries SET state = 'rung', rung_at = ?1, safe_point = ?2 WHERE id = ?3", params![now, entry.safe_point, entry.id])
                .map_err(error)?;
            self.set_meta(&key, &now.to_string());
        }
        Ok(())
    }

    /// Marks a message read by `reader` (11.11).
    pub fn mark_read(&self, id: &str, reader: &str, now: i64) -> Result<Entry> {
        let mut entry = self.get(id, now)?.filter(|entry| entry.kind == "message").ok_or_else(|| LoreError::new(format!("no message {id}")))?;
        if entry.to.as_deref() == Some("team") {
            entry.read_by.entry(reader.to_owned()).or_insert(now);
            let everyone = if entry.recipients.is_empty() {
                true // who must read it is unknown: the first reader settles it
            } else {
                entry.recipients.iter().all(|name| entry.read_by.contains_key(name))
            };
            if everyone {
                entry.state = "read".into();
                entry.read_at = Some(entry.read_at.unwrap_or(now));
            }
        } else if entry.state != "read" {
            entry.state = "read".into();
            entry.read_at = Some(now);
        }
        entry.expires = self.expiry(&entry, false);
        self.db
            .execute(
                "UPDATE entries SET state = ?1, read_at = ?2, read_by = ?3, expires = ?4 WHERE id = ?5",
                params![entry.state, entry.read_at, serde_json::to_string(&entry.read_by).unwrap_or_default(), entry.expires, entry.id],
            )
            .map_err(error)?;
        Ok(entry)
    }

    /// Writes a brief's next version (11.2, 11.12); the older versions lose
    /// their protection and expire with the task.
    pub fn brief(&self, team: &str, task: &str, by: &str, text: &str, now: i64) -> Result<Written> {
        self.check_brief(task, text, now)?;
        let mut notes = Vec::new();
        let clean = self.redact(text, "text", &mut notes);
        let version = self.latest_brief_version(team, task) + 1;
        let mut entry = Entry {
            id: new_id("brief", &format!("{team}/{task}")),
            kind: "brief".into(),
            team: team.to_owned(),
            by: by.to_owned(),
            at: now,
            task: task.to_owned(),
            text: clean,
            version,
            ..Entry::default()
        };
        entry.expires = None;
        self.insert(&entry)?;
        self.refresh_task(team, task)?;
        Ok(Written { entry, notes })
    }

    /// Task 187: everything `brief` refuses (the task, the size, an empty text,
    /// a full file), checked before the durable file is committed, so a brief
    /// is never committed that the working tier then refuses (11.12).
    pub fn check_brief(&self, task: &str, text: &str, now: i64) -> Result<()> {
        if task.trim().is_empty() {
            return Err(LoreError::new("refused: a brief names its task (`--task`)"));
        }
        if text.len() > BRIEF_BYTES {
            return Err(LoreError::new(format!(
                "refused: the brief has {} bytes; a brief holds at most 8 KiB - name a longer subject as a document the brief points to, or split it into topics",
                text.len()
            )));
        }
        if text.trim().is_empty() {
            return Err(LoreError::new("refused: the brief is empty"));
        }
        self.full(now)
    }

    /// The latest brief of a task, or one version, while its row exists.
    pub fn brief_of(&self, team: &str, task: &str, version: Option<i64>, now: i64) -> Result<Option<Entry>> {
        let version = version.unwrap_or_else(|| self.latest_brief_version(team, task));
        Ok(self
            .select("kind = 'brief' AND team = ?1 AND task = ?2 AND version = ?3 AND (expires IS NULL OR expires > ?4)", &[&team, &task, &version, &now])?
            .pop())
    }

    /// Records the durable record a working entry was promoted to.
    pub fn set_promoted(&self, id: &str, record: &str) -> Result<()> {
        self.db.execute("UPDATE entries SET promoted = ?1 WHERE id = ?2", params![record, id]).map_err(error)?;
        Ok(())
    }

    /// Protects an entry a promote is reading from the bounds (11.4).
    pub fn reading(&self, id: &str, now: i64) {
        self.set_meta(&format!("reading:{id}"), &now.to_string());
    }

    pub fn done_reading(&self, id: &str) {
        let _ = self.db.execute("DELETE FROM meta WHERE key = ?1", [format!("reading:{id}")]);
    }

    // -- reads -------------------------------------------------------------------

    /// The current pulse per `(by, task)`, or with `all` every unexpired row,
    /// newest first.
    pub fn pulses(&self, viewer: &Viewer, task: Option<&str>, by: Option<&str>, since: Option<i64>, all: bool, now: i64) -> Result<Vec<Entry>> {
        let rows = self.select("kind = 'pulse' AND (expires IS NULL OR expires > ?1) ORDER BY at DESC, id", &[&now])?;
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for entry in rows {
            if task.is_some_and(|task| task != entry.task) || by.is_some_and(|by| by != entry.by) || since.is_some_and(|since| entry.at < since) {
                continue;
            }
            if !viewer.sees(&entry) {
                continue;
            }
            if !all && !seen.insert((entry.team.clone(), entry.by.clone(), entry.task.clone())) {
                continue;
            }
            out.push(entry);
        }
        Ok(out)
    }

    /// Messages the viewer may read, newest first.
    pub fn messages(&self, viewer: &Viewer, unread: bool, to: Option<&str>, from: Option<&str>, now: i64) -> Result<Vec<Entry>> {
        let rows = self.select("kind = 'message' AND (expires IS NULL OR expires > ?1) ORDER BY at DESC, id", &[&now])?;
        Ok(rows
            .into_iter()
            .filter(|entry| viewer.sees(entry))
            .filter(|entry| to.is_none_or(|to| entry.to.as_deref() == Some(to)))
            .filter(|entry| from.is_none_or(|from| entry.by == from))
            .filter(|entry| {
                if !unread {
                    return true;
                }
                if entry.to.as_deref() == Some("team") {
                    !entry.read_by.contains_key(&viewer.name) && entry.state != "read"
                } else {
                    entry.unread()
                }
            })
            .collect())
    }

    /// The working entries a search finds (11.6): words and terms, RRF with
    /// k = 60 and depth 50; scores before the tier's weight.
    pub fn search(&self, query: &str, viewer: &Viewer, now: i64) -> Result<Vec<(Entry, f64, Vec<String>)>> {
        let lists = [
            ("words", "words", crate::finder::word_query(query)),
            ("terms", "terms", crate::finder::term_query(query)),
        ];
        let mut scores: Vec<(String, f64, Vec<String>)> = Vec::new();
        let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
        for (name, table, fts) in lists {
            if fts.is_empty() {
                continue;
            }
            let sql = format!("SELECT id FROM {table} WHERE {table} MATCH ?1 ORDER BY bm25({table}) LIMIT ?2");
            let ids: Vec<String> = match self.db.prepare(&sql) {
                Ok(mut statement) => {
                    let found: Vec<String> = statement
                        .query_map(params![fts, (crate::finder::DEPTH * 4) as i64], |row| row.get::<_, String>(0))
                        .and_then(|rows| rows.collect())
                        .unwrap_or_default();
                    found
                }
                Err(_) => Vec::new(),
            };
            let mut rank = 0usize;
            for id in ids {
                if rank >= crate::finder::DEPTH {
                    break;
                }
                let entry = match entries.get(&id) {
                    Some(entry) => entry.clone(),
                    None => match self.get(&id, now)? {
                        Some(entry) if viewer.sees(&entry) => {
                            entries.insert(id.clone(), entry.clone());
                            entry
                        }
                        _ => continue,
                    },
                };
                let by = 1.0 / (crate::finder::RRF_K + rank as f64 + 1.0);
                rank += 1;
                match scores.iter_mut().find(|row| row.0 == entry.id) {
                    Some(row) => {
                        row.1 += by;
                        row.2.push(name.into());
                    }
                    None => scores.push((entry.id.clone(), by, vec![name.into()])),
                }
            }
        }
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scores.into_iter().filter_map(|(id, score, via)| entries.remove(&id).map(|entry| (entry, score, via))).collect())
    }

    /// `lore working status` (11.13).
    pub fn status(&self, now: i64) -> Result<Value> {
        let mut counts = Map::new();
        for kind in KINDS {
            let n: i64 = self
                .db
                .query_row("SELECT COUNT(*) FROM entries WHERE kind = ?1 AND (expires IS NULL OR expires > ?2)", params![kind, now], |row| row.get(0))
                .map_err(error)?;
            counts.insert(kind.into(), json!(n));
        }
        let oldest: Option<i64> = self.db.query_row("SELECT MIN(at) FROM entries", [], |row| row.get(0)).map_err(error)?;
        Ok(json!({
            "counts": counts,
            "bytes": self.file_bytes(),
            "oldest": oldest,
            "last_cleanup": self.meta("last_cleanup").and_then(|v| v.parse::<i64>().ok()),
            "cleanup_error": self.meta("cleanup_error"),
            "path": self.path.display().to_string(),
        }))
    }

    /// A one-line note about the last cleanup that failed, once.
    pub fn take_cleanup_note(&self) -> Option<String> {
        let note = self.meta("cleanup_error")?;
        let _ = self.db.execute("DELETE FROM meta WHERE key = 'cleanup_error'", []);
        Some(format!("the working tier's last cleanup failed: {note}"))
    }

    // -- the cleanup -------------------------------------------------------------

    /// Whether a pass is due (at open, and every 10 minutes after the last).
    pub fn cleanup_due(&self, now: i64) -> bool {
        self.meta("last_cleanup").and_then(|v| v.parse::<i64>().ok()).is_none_or(|last| now - last >= CLEANUP_EVERY)
    }

    /// One bounded cleanup pass (11.4): expired entries, then the bounds, at
    /// most 500 rows, transactions of at most 50 ms, then an incremental
    /// vacuum. A failure is an event and a note on the next read.
    pub fn cleanup(&self, now: i64, clock: &dyn TaskClock) -> Result<Cleaned> {
        let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&self.lock_path).map_err(error)?;
        if lock.try_lock_exclusive().is_err() {
            return Ok(Cleaned { busy: true, ..Cleaned::default() });
        }
        let result = self.cleanup_locked(now, clock);
        let _ = fs2::FileExt::unlock(&lock);
        match result {
            Ok(cleaned) => Ok(cleaned),
            Err(failure) => {
                self.event(now, "working_cleanup_failed", json!({"error": failure.to_string()}));
                self.set_meta("cleanup_error", &failure.to_string());
                Ok(Cleaned::default())
            }
        }
    }

    fn cleanup_locked(&self, now: i64, clock: &dyn TaskClock) -> Result<Cleaned> {
        let before = self.file_bytes() as i64;
        self.sync_clock(clock, 200)?;
        let mut victims: Vec<String> = Vec::new();
        let mut events: Vec<Value> = Vec::new();
        let protected = self.protected(now)?;
        self.tell_stalls(now, &mut events)?;
        // Expired entries; an unread message is not dropped silently: its
        // event is written in the same transaction as its delete (task 187),
        // so a crash between them can neither lose the event nor repeat it.
        let mut with_event: BTreeMap<String, Value> = BTreeMap::new();
        for entry in self.select("expires IS NOT NULL AND expires <= ?1 ORDER BY expires LIMIT ?2", &[&now, &((PASS_ROWS + 1) as i64)])? {
            if entry.unread() {
                let body = json!({"from": entry.by, "to": entry.to, "at": entry.at, "text": entry.text.chars().take(80).collect::<String>(),
                    "inbox": matches!(entry.to.as_deref(), Some("lead" | "owner"))});
                with_event.insert(entry.id.clone(), body);
            }
            victims.push(entry.id);
        }
        // The bounds, oldest first, never a protected entry.
        let s = &self.settings;
        let teams: Vec<String> = {
            let mut statement = self.db.prepare("SELECT DISTINCT team FROM entries").map_err(error)?;
            let rows = statement.query_map([], |row| row.get(0)).map_err(error)?;
            rows.collect::<rusqlite::Result<_>>().map_err(error)?
        };
        let over = |kind_where: &str, cap: usize, args: &[&dyn rusqlite::ToSql], order: &str, victims: &mut Vec<String>| -> Result<()> {
            let rows = self.select(&format!("{kind_where} ORDER BY {order}"), args)?;
            let alive: Vec<&Entry> = rows.iter().filter(|entry| !victims.contains(&entry.id)).collect();
            if alive.len() <= cap {
                return Ok(());
            }
            let mut excess = alive.len() - cap;
            for entry in alive {
                if excess == 0 || victims.len() > PASS_ROWS {
                    break;
                }
                if protected.contains(&entry.id) {
                    continue;
                }
                victims.push(entry.id.clone());
                excess -= 1;
            }
            Ok(())
        };
        for team in &teams {
            // Pulses: closed tasks' first, then the oldest overall.
            over(
                "kind = 'pulse' AND team = ?1",
                s.pulses_per_team,
                &[team],
                "CASE WHEN task <> '' AND EXISTS(SELECT 1 FROM tasks WHERE tasks.team = entries.team AND tasks.task = entries.task AND closed_at IS NOT NULL) THEN 0 ELSE 1 END, at",
                &mut victims,
            )?;
            // Messages: the oldest read ones; unread never for a bound.
            let read: i64 = self
                .db
                .query_row("SELECT COUNT(*) FROM entries WHERE kind = 'message' AND team = ?1", [team], |row| row.get(0))
                .map_err(error)?;
            if read as usize > s.messages_per_team {
                let excess = read as usize - s.messages_per_team;
                let oldest = self.select("kind = 'message' AND team = ?1 AND state = 'read' ORDER BY at LIMIT ?2", &[team, &(excess as i64)])?;
                victims.extend(oldest.into_iter().map(|entry| entry.id).filter(|id| !protected.contains(id)));
            }
            // Brief versions per task: the oldest go, the latest never.
            let tasks: Vec<String> = {
                let mut statement = self.db.prepare("SELECT DISTINCT task FROM entries WHERE kind = 'brief' AND team = ?1").map_err(error)?;
                let rows = statement.query_map([team], |row| row.get(0)).map_err(error)?;
                rows.collect::<rusqlite::Result<_>>().map_err(error)?
            };
            for task in tasks {
                over("kind = 'brief' AND team = ?1 AND task = ?2", s.brief_versions, &[team, &task], "version", &mut victims)?;
            }
        }
        // Store-wide: the same order, read messages first, then old pulses.
        let total: i64 = self.db.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0)).map_err(error)?;
        if total as usize > s.entries {
            let mut excess = total as usize - s.entries;
            for sql in ["kind = 'message' AND state = 'read' ORDER BY at", "kind = 'pulse' ORDER BY at", "kind = 'brief' ORDER BY version"] {
                for entry in self.select(sql, &[])? {
                    if excess == 0 || victims.len() > PASS_ROWS {
                        break;
                    }
                    if protected.contains(&entry.id) || victims.contains(&entry.id) {
                        continue;
                    }
                    victims.push(entry.id);
                    excess -= 1;
                }
            }
        }
        let more = victims.len() > PASS_ROWS;
        victims.truncate(PASS_ROWS);
        // Delete in short transactions. Task 187: the budget starts once the
        // transaction holds the lock (a wait for BEGIN is not its time), and
        // each transaction deletes at least one row, so a slow start can
        // never leave the pass looping on empty transactions.
        let mut deleted = 0;
        let mut queue = victims.as_slice();
        while !queue.is_empty() {
            self.db.execute_batch("BEGIN IMMEDIATE").map_err(error)?;
            let started = Instant::now();
            let mut n = 0;
            while n < queue.len() && (n == 0 || started.elapsed() < self.budget.get()) {
                let id = &queue[n];
                let step = self.delete_rows(std::slice::from_ref(id)).and_then(|()| match with_event.get(id) {
                    Some(body) => self
                        .db
                        .execute("INSERT INTO events(at, kind, body) VALUES(?1, 'working_unread_expired', ?2)", params![now, body.to_string()])
                        .map(|_| ()),
                    None => Ok(()),
                });
                if let Err(failure) = step {
                    let _ = self.db.execute_batch("ROLLBACK");
                    return Err(error(failure));
                }
                if let Some(body) = with_event.get(id) {
                    events.push(json!({"kind": "working_unread_expired", "body": body}));
                }
                n += 1;
            }
            self.db.execute_batch("COMMIT").map_err(error)?;
            deleted += n;
            queue = &queue[n..];
        }
        let _ = self.db.execute("DELETE FROM events WHERE n <= (SELECT MAX(n) FROM events) - ?1", [EVENTS_KEPT]);
        for event in events.iter().filter(|event| event["kind"] != "working_unread_expired") {
            self.event(now, event["kind"].as_str().unwrap_or_default(), event["body"].clone());
        }
        let _ = self.db.execute_batch("PRAGMA incremental_vacuum(200)");
        let freed = (before - self.file_bytes() as i64).max(0);
        self.set_meta("last_cleanup", &now.to_string());
        let mut out = Cleaned { deleted, bytes_freed: freed, more, busy: false, events: events.clone() };
        if deleted > 0 {
            // One `working_cleaned` an hour at most; the counts add up.
            let pending: Value = self.meta("cleaned_pending").and_then(|v| serde_json::from_str(&v).ok()).unwrap_or(json!({"count": 0, "bytes": 0}));
            let count = pending["count"].as_i64().unwrap_or(0) + deleted as i64;
            let bytes = pending["bytes"].as_i64().unwrap_or(0) + freed;
            let last = self.meta("cleaned_event").and_then(|v| v.parse::<i64>().ok());
            if last.is_none_or(|last| now - last >= 3_600) {
                let body = json!({"count": count, "bytes_freed": bytes});
                self.event(now, "working_cleaned", body.clone());
                out.events.push(json!({"kind": "working_cleaned", "body": body}));
                self.set_meta("cleaned_event", &now.to_string());
                self.set_meta("cleaned_pending", &json!({"count": 0, "bytes": 0}).to_string());
            } else {
                self.set_meta("cleaned_pending", &json!({"count": count, "bytes": bytes}).to_string());
            }
        }
        Ok(out)
    }

    /// 11.11: an urgent message still unrung past `urgent_bound`, and a normal
    /// one unread for 24 hours, are each told to the lead once.
    fn tell_stalls(&self, now: i64, events: &mut Vec<Value>) -> Result<()> {
        let bound = self.settings.urgent_bound * 60;
        let urgent = self.select("kind = 'message' AND urgency = 'urgent' AND state = 'queued' AND at <= ?1", &[&(now - bound)])?;
        let normal = self.select("kind = 'message' AND urgency = 'normal' AND state <> 'read' AND at <= ?1", &[&(now - DAY)])?;
        for (entry, kind) in urgent.iter().map(|e| (e, "working_urgent_undelivered")).chain(normal.iter().map(|e| (e, "working_unread_stall"))) {
            let key = format!("told:{}", entry.id);
            if self.meta(&key).is_some() {
                continue;
            }
            self.set_meta(&key, &now.to_string());
            let body = json!({"id": entry.id, "from": entry.by, "to": entry.to, "minutes": (now - entry.at) / 60,
                "text": entry.text.chars().take(200).collect::<String>()});
            events.push(json!({"kind": kind, "body": body}));
        }
        Ok(())
    }

    /// What no bound deletes: the current pulse of each `(by, task)`, an
    /// unread urgent message, the latest brief of a task, an entry a promote
    /// is reading.
    fn protected(&self, now: i64) -> Result<BTreeSet<String>> {
        let mut keep = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for entry in self.select("kind = 'pulse' ORDER BY at DESC", &[])? {
            if seen.insert((entry.team.clone(), entry.by.clone(), entry.task.clone())) {
                keep.insert(entry.id);
            }
        }
        for entry in self.select("kind = 'message' AND urgency = 'urgent' AND state <> 'read'", &[])? {
            keep.insert(entry.id);
        }
        for entry in self.select("kind = 'brief' AND version = (SELECT MAX(version) FROM entries AS other WHERE other.kind = 'brief' AND other.team = entries.team AND other.task = entries.task)", &[])? {
            keep.insert(entry.id);
        }
        let mut statement = self.db.prepare("SELECT key, value FROM meta WHERE key LIKE 'reading:%'").map_err(error)?;
        let rows: Vec<(String, String)> = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).map_err(error)?.collect::<rusqlite::Result<_>>().map_err(error)?;
        for (key, at) in rows {
            // A promote reads for seconds; an old mark is a crashed one.
            if at.parse::<i64>().is_ok_and(|at| now - at < 600) {
                keep.insert(key.trim_start_matches("reading:").to_owned());
            }
        }
        Ok(keep)
    }
}

/// The words a working entry is found by, for tests.
pub fn words_of(text: &str) -> Vec<String> {
    word_re().find_iter(text).map(|m| m.as_str().to_lowercase()).collect()
}

#[cfg(test)]
#[path = "working_tests.rs"]
mod tests;
