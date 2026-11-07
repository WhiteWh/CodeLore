//! How agents actually search (task 149): counters only, never the query or a
//! record's text.
//!
//! Every `lore find` records one row in the store's private SQLite (beside the
//! index, in the user cache - never in the repository): the query *class*
//! (exact or paraphrase, see [`classify`]), the caller (from `LORE_CALLER`, or
//! `LORE_ROLE`/`LORE_AGENT`), the tiers that ran, the result count and the
//! latency. `lore show` and a write that cites records record which result was
//! opened or cited in the same session (`LORE_SESSION`, or the Zerus session
//! variables), so a find's row gains the hit position; a find with no opened
//! result followed by a new fault in its session is a miss.
//!
//! The rows are bounded: older than [`RETENTION_DAYS`] or beyond
//! [`RETENTION_FINDS`] finds are pruned on every write. The schema stores no
//! query text and no record text - only ids in `find_results` (needed for the
//! hit position), the class, the caller, the tiers, counts and times.
use crate::config::Config;
use crate::search::Found;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

/// Counters older than this are pruned on the next write.
pub const RETENTION_DAYS: i64 = 180;
/// At most this many finds are kept (and their result rows).
pub const RETENTION_FINDS: i64 = 100_000;
/// A find keeps at most this many result ids for the hit position.
pub const MAX_RESULTS: usize = 50;
const DAY: f64 = 86_400.0;

/// The current time in seconds, as the rows record it.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// The private counters file: `LORE_STATS_DB`, else `stats.sqlite` in the
/// index's folder (the user cache, or the repository's git folder as the index
/// falls back).
pub fn path(config: &Config) -> PathBuf {
    if let Some(configured) = std::env::var_os("LORE_STATS_DB").filter(|value| !value.is_empty()) {
        return PathBuf::from(configured);
    }
    for dir in crate::index::index_dirs(&config.root) {
        if std::fs::create_dir_all(&dir).is_ok() {
            return dir.join("stats.sqlite");
        }
    }
    config.lore_dir().join("stats.sqlite")
}

/// Opens (creating) the counters at `path`.
pub fn open_at(path: &std::path::Path) -> rusqlite::Result<rusqlite::Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA user_version=1;
         CREATE TABLE IF NOT EXISTS finds (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             at REAL NOT NULL,
             session TEXT NOT NULL,
             caller TEXT NOT NULL,
             class TEXT NOT NULL,
             tiers TEXT NOT NULL,
             results INTEGER NOT NULL,
             latency_ms REAL NOT NULL,
             hit_rank INTEGER,
             hit_kind TEXT
         );
         CREATE TABLE IF NOT EXISTS find_results (
             find_id INTEGER NOT NULL,
             rank INTEGER NOT NULL,
             record_id TEXT NOT NULL,
             PRIMARY KEY (find_id, rank)
         );
         CREATE TABLE IF NOT EXISTS opens (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             at REAL NOT NULL,
             session TEXT NOT NULL,
             kind TEXT NOT NULL,
             record_id TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS writes (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             at REAL NOT NULL,
             session TEXT NOT NULL,
             kind TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS finds_at ON finds(at);
         CREATE INDEX IF NOT EXISTS finds_session ON finds(session, id);
         CREATE INDEX IF NOT EXISTS opens_at ON opens(at);
         CREATE INDEX IF NOT EXISTS writes_at ON writes(at);",
    )?;
    Ok(conn)
}

/// The caller, as Zerus or a person names it: `LORE_CALLER` ("lead:claude",
/// "member:codex", "human"), else `LORE_ROLE` and `LORE_AGENT`, else unknown.
pub fn caller() -> String {
    if let Some(caller) = std::env::var_os("LORE_CALLER").filter(|value| !value.is_empty()) {
        return caller.to_string_lossy().into_owned();
    }
    let role = std::env::var("LORE_ROLE").unwrap_or_default();
    let agent = std::env::var("LORE_AGENT").unwrap_or_default();
    match (role.trim(), agent.trim()) {
        ("", "") => "unknown".to_owned(),
        ("", agent) => agent.to_owned(),
        (role, "") => role.to_owned(),
        (role, agent) => format!("{role}:{agent}"),
    }
}

/// The session the find belongs to: the same session's show/cite attributes
/// the hit. Empty when nothing names it (no correlation, counts only).
pub fn session() -> String {
    for name in ["LORE_SESSION", "HGS_SESSION", "DSH_SESSION_ID", "HGS_RUN_ID"] {
        if let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) {
            return value.to_string_lossy().into_owned();
        }
    }
    String::new()
}

/// Exact: the query carries something a paraphrase would not - an error code
/// (0x…, E_…, ERR_…, HRESULT), a path (`a/b`, `a\b`, a file name with an
/// extension), a symbol (a dotted name, a snake_case or CamelCase identifier),
/// or a quoted phrase. Everything else is a paraphrase. Documented here so the
/// mix can be read beside the rule that made it.
pub fn classify(query: &str) -> &'static str {
    let code = OnceLock::new();
    let code = code.get_or_init(|| {
        regex::Regex::new(
            r"(?x)
              \b0x[0-9A-Fa-f]{4,}\b
            | \b[A-Z][A-Z0-9]*_[A-Z0-9_]{2,}\b
            | \bERR_[A-Z0-9_]+\b
            | \bHRESULT\b",
        )
        .expect("the code pattern is valid")
    });
    let path = OnceLock::new();
    let path = path.get_or_init(|| {
        regex::Regex::new(
            r"(?x)
              (?:[A-Za-z0-9_.-]+[\\/])+[A-Za-z0-9_.\\/-]+
            | \b[A-Za-z0-9_-]+\.(?:rs|py|ts|tsx|js|json|toml|md|txt|sql|yml|yaml|cs|cpp|h|hpp|go|java|kt|swift)\b",
        )
        .expect("the path pattern is valid")
    });
    let symbol = OnceLock::new();
    let symbol = symbol.get_or_init(|| {
        regex::Regex::new(
            r"(?x)
              \b[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]+)+\b
            | \b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b
            | \b[A-Z][a-z]+[A-Z][A-Za-z0-9]*\b",
        )
        .expect("the symbol pattern is valid")
    });
    let quoted = OnceLock::new();
    let quoted = quoted.get_or_init(|| regex::Regex::new(r#""[^"]{4,}""#).expect("the quoted pattern is valid"));
    if code.is_match(query) || path.is_match(query) || symbol.is_match(query) || quoted.is_match(query) {
        "exact"
    } else {
        "paraphrase"
    }
}

/// The tiers a find searched, from its results' channels plus words and terms
/// (always searched) - "meaning" only when the semantic list ran.
pub fn tiers(hits: &[Found]) -> String {
    let mut tiers: Vec<&str> = vec!["words", "terms"];
    for hit in hits {
        for via in &hit.via {
            let name = match via.as_str() {
                "signal" => "signal",
                "phrase" => "phrase",
                "meaning" => "meaning",
                _ => continue,
            };
            if !tiers.contains(&name) {
                tiers.push(name);
            }
        }
    }
    tiers.sort_unstable();
    tiers.join(",")
}

fn connect(config: &Config) -> Option<rusqlite::Connection> {
    open_at(&path(config)).ok()
}

/// Records one find. Never fails a search: a missing or locked counters file
/// is ignored.
pub fn record_find(config: &Config, query: &str, hits: &[Found], elapsed: Duration, tiers: &str) {
    let Some(conn) = connect(config) else { return };
    let _ = record_find_in(&conn, query, hits, elapsed, tiers, &caller(), &session(), now());
}

#[allow(clippy::too_many_arguments)]
fn record_find_in(
    conn: &rusqlite::Connection,
    query: &str,
    hits: &[Found],
    elapsed: Duration,
    tiers: &str,
    caller: &str,
    session: &str,
    at: f64,
) -> rusqlite::Result<()> {
    let class = classify(query);
    conn.execute(
        "INSERT INTO finds (at, session, caller, class, tiers, results, latency_ms) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![at, session, caller, class, tiers, hits.len() as i64, elapsed.as_secs_f64() * 1000.0],
    )?;
    let find = conn.last_insert_rowid();
    for (rank, hit) in hits.iter().take(MAX_RESULTS).enumerate() {
        let Some(id) = hit.record.get("id").and_then(Value::as_str) else { continue };
        conn.execute(
            "INSERT OR REPLACE INTO find_results (find_id, rank, record_id) VALUES (?1,?2,?3)",
            rusqlite::params![find, (rank + 1) as i64, id],
        )?;
    }
    prune(conn, at)
}

/// Records that a result was opened with `lore show`.
pub fn record_open(config: &Config, id: &str) {
    let Some(conn) = connect(config) else { return };
    let _ = record_open_in(&conn, id, &session(), now());
}

fn record_open_in(conn: &rusqlite::Connection, id: &str, session: &str, at: f64) -> rusqlite::Result<()> {
    if session.is_empty() {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO opens (at, session, kind, record_id) VALUES (?1,?2,'open',?3)",
        rusqlite::params![at, session, id],
    )?;
    attribute(conn, session, id, "open")
}

/// Records records a write cited (`refs.records`) - their ids only.
pub fn record_cite(config: &Config, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let Some(conn) = connect(config) else { return };
    let _ = record_cite_in(&conn, ids, &session(), now());
}

fn record_cite_in(conn: &rusqlite::Connection, ids: &[String], session: &str, at: f64) -> rusqlite::Result<()> {
    if session.is_empty() {
        return Ok(());
    }
    for id in ids {
        conn.execute(
            "INSERT INTO opens (at, session, kind, record_id) VALUES (?1,?2,'cite',?3)",
            rusqlite::params![at, session, id],
        )?;
        attribute(conn, session, id, "cite")?;
    }
    Ok(())
}

/// Records what a successful write wrote: a fault (for misses) and the
/// records it cited (for hit positions). No text.
pub fn record_write(config: &Config, records: &[Value]) {
    let Some(conn) = connect(config) else { return };
    let session = session();
    let at = now();
    let fault = records.iter().any(|record| record.get("kind").and_then(Value::as_str) == Some("fault"));
    if fault && !session.is_empty() {
        let _ = conn.execute(
            "INSERT INTO writes (at, session, kind) VALUES (?1,?2,'fault')",
            rusqlite::params![at, session],
        );
    }
    let ids: Vec<String> = records
        .iter()
        .flat_map(|record| {
            record
                .get("refs")
                .and_then(|refs| refs.get("records"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect();
    let _ = record_cite_in(&conn, &ids, &session, at);
}

/// Gives the latest find of `session` that has no hit position yet the rank of
/// `id`, when that find's results contain it. A newer find wins.
fn attribute(conn: &rusqlite::Connection, session: &str, id: &str, kind: &str) -> rusqlite::Result<()> {
    let find: Option<(i64, i64)> = conn
        .query_row(
            "SELECT f.id, r.rank FROM finds f JOIN find_results r ON r.find_id = f.id
             WHERE f.session = ?1 AND f.hit_rank IS NULL AND r.record_id = ?2
             ORDER BY f.id DESC LIMIT 1",
            rusqlite::params![session, id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    if let Some((find, rank)) = find {
        conn.execute(
            "UPDATE finds SET hit_rank = ?1, hit_kind = ?2 WHERE id = ?3",
            rusqlite::params![rank, kind, find],
        )?;
    }
    Ok(())
}

/// Bounds the counters: drops finds older than [`RETENTION_DAYS`] and all but
/// the newest [`RETENTION_FINDS`], with their result and event rows.
pub fn prune(conn: &rusqlite::Connection, at: f64) -> rusqlite::Result<()> {
    prune_with(conn, at, RETENTION_DAYS, RETENTION_FINDS)
}

fn prune_with(conn: &rusqlite::Connection, at: f64, days: i64, max_finds: i64) -> rusqlite::Result<()> {
    let cutoff = at - days as f64 * DAY;
    conn.execute("DELETE FROM finds WHERE at < ?1", rusqlite::params![cutoff])?;
    conn.execute(
        "DELETE FROM find_results WHERE find_id NOT IN (SELECT id FROM finds)",
        [],
    )?;
    conn.execute(
        "DELETE FROM finds WHERE id NOT IN (SELECT id FROM finds ORDER BY id DESC LIMIT ?1)",
        rusqlite::params![max_finds],
    )?;
    conn.execute("DELETE FROM find_results WHERE find_id NOT IN (SELECT id FROM finds)", [])?;
    conn.execute("DELETE FROM opens WHERE at < ?1", rusqlite::params![cutoff])?;
    conn.execute("DELETE FROM writes WHERE at < ?1", rusqlite::params![cutoff])?;
    Ok(())
}

/// One class's or caller's numbers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metric {
    pub finds: i64,
    pub opened: i64,
    pub hit1: i64,
    pub hit5: i64,
}

impl Metric {
    pub fn hit1_rate(&self) -> f64 {
        if self.finds == 0 { 0.0 } else { self.hit1 as f64 / self.finds as f64 }
    }
    pub fn hit5_rate(&self) -> f64 {
        if self.finds == 0 { 0.0 } else { self.hit5 as f64 / self.finds as f64 }
    }
}

/// What `lore stats` prints.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub window_days: i64,
    pub finds: i64,
    pub opened: i64,
    pub hit1: i64,
    pub hit5: i64,
    pub by_class: BTreeMap<String, Metric>,
    pub by_caller: BTreeMap<String, Metric>,
    pub tiers: BTreeMap<String, i64>,
    pub latency_ms: f64,
    pub misses: i64,
    pub retained: i64,
}

fn read_metric(conn: &rusqlite::Connection, cutoff: f64, class: Option<&str>) -> (Metric, f64) {
    let mut metric = Metric::default();
    let mut latency = 0.0;
    let sql = match class {
        Some(_) => "SELECT count(*), sum(hit_rank IS NOT NULL), sum(hit_rank <= 1), sum(hit_rank <= 5), avg(latency_ms) FROM finds WHERE at >= ?1 AND class = ?2",
        None => "SELECT count(*), sum(hit_rank IS NOT NULL), sum(hit_rank <= 1), sum(hit_rank <= 5), avg(latency_ms) FROM finds WHERE at >= ?1",
    };
    let fill = |row: &rusqlite::Row<'_>, metric: &mut Metric, latency: &mut f64| -> rusqlite::Result<()> {
        metric.finds = row.get(0)?;
        metric.opened = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
        metric.hit1 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
        metric.hit5 = row.get::<_, Option<i64>>(3)?.unwrap_or(0);
        *latency = row.get::<_, Option<f64>>(4)?.unwrap_or(0.0);
        Ok(())
    };
    match class {
        Some(class) => {
            let _ = conn.query_row(sql, rusqlite::params![cutoff, class], |row| fill(row, &mut metric, &mut latency));
        }
        None => {
            let _ = conn.query_row(sql, rusqlite::params![cutoff], |row| fill(row, &mut metric, &mut latency));
        }
    }
    (metric, latency)
}

/// Reads the counters of the last `days` (at most the retention).
pub fn report(config: &Config, days: i64) -> Report {
    let days = days.clamp(1, RETENTION_DAYS);
    let Some(conn) = connect(config) else { return Report { window_days: days, ..Default::default() } };
    let now = now();
    let cutoff = now - days as f64 * DAY;
    let mut report = Report { window_days: days, ..Default::default() };
    let (overall, latency) = read_metric(&conn, cutoff, None);
    report.finds = overall.finds;
    report.opened = overall.opened;
    report.hit1 = overall.hit1;
    report.hit5 = overall.hit5;
    report.latency_ms = latency;
    for class in ["exact", "paraphrase"] {
        let (metric, _) = read_metric(&conn, cutoff, Some(class));
        report.by_class.insert(class.to_owned(), metric);
    }
    if let Ok(mut statement) = conn.prepare(
        "SELECT caller, count(*), sum(hit_rank IS NOT NULL), sum(hit_rank <= 1), sum(hit_rank <= 5) FROM finds WHERE at >= ?1 GROUP BY caller ORDER BY caller",
    ) {
        if let Ok(rows) = statement.query_map(rusqlite::params![cutoff], |row| {
            Ok((row.get::<_, String>(0)?, Metric { finds: row.get(1)?, opened: row.get::<_, Option<i64>>(2)?.unwrap_or(0), hit1: row.get::<_, Option<i64>>(3)?.unwrap_or(0), hit5: row.get::<_, Option<i64>>(4)?.unwrap_or(0) }))
        }) {
            for row in rows.flatten() {
                report.by_caller.insert(row.0, row.1);
            }
        }
    }
    if let Ok(mut statement) = conn.prepare("SELECT tiers, count(*) FROM finds WHERE at >= ?1 GROUP BY tiers") {
        if let Ok(rows) = statement.query_map(rusqlite::params![cutoff], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))) {
            for row in rows.flatten() {
                for tier in row.0.split(',') {
                    *report.tiers.entry(tier.to_owned()).or_default() += row.1;
                }
            }
        }
    }
    report.misses = conn
        .query_row(
            "SELECT count(*) FROM finds f WHERE f.at >= ?1 AND f.hit_rank IS NULL AND f.session <> ''
             AND EXISTS (SELECT 1 FROM writes w WHERE w.session = f.session AND w.kind = 'fault' AND w.at >= f.at)",
            rusqlite::params![cutoff],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0);
    report.retained = conn.query_row("SELECT count(*) FROM finds", [], |row| row.get(0)).unwrap_or(0);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> (tempfile::TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_at(&dir.path().join(format!("{name}.sqlite"))).unwrap();
        (dir, conn)
    }

    fn found(id: &str, via: &[&str]) -> Found {
        Found {
            record: serde_json::json!({"id": id, "kind": "fault", "title": "a title"}),
            score: 0.5,
            via: via.iter().map(|v| v.to_string()).collect(),
            meaning: None,
        }
    }

    #[test]
    fn the_query_class_is_exact_only_for_a_code_path_symbol_or_quote() {
        for exact in [
            "0x80070005",
            "E_ASSET_STAMP_STALE",
            "ERR_DEVICE_LOST",
            "src/render/post.py",
            "scene.depth_buffer",
            "GetData",
            "the file C:\\x\\y.rs",
            "\"a quoted phrase\"",
        ] {
            assert_eq!(classify(exact), "exact", "{exact}");
        }
        for paraphrase in ["the atlas checksum mismatch", "why does the build stop", "a hang on exit"] {
            assert_eq!(classify(paraphrase), "paraphrase", "{paraphrase}");
        }
    }

    #[test]
    fn a_find_stores_no_query_or_record_text() {
        let (_dir, conn) = scratch("text");
        let query = "the atlas checksum mismatch in a very secret title";
        let record = found("secret-id", &["words"]);
        record_find_in(&conn, query, &[record], Duration::from_millis(3), "words,terms", "member:codex", "s1", 100.0).unwrap();
        let mut leaks = 0;
        let tables: Vec<String> = {
            let mut statement = conn.prepare("SELECT name FROM sqlite_master WHERE type='table'").unwrap();
            statement.query_map([], |row| row.get(0)).unwrap().flatten().collect()
        };
        for table in tables {
            let mut statement = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns = statement.column_count();
            let rows = statement
                .query_map([], |row| {
                    let mut values = Vec::new();
                    for index in 0..columns {
                        values.push(row.get::<_, Option<String>>(index).ok().flatten().unwrap_or_default());
                    }
                    Ok(values)
                })
                .unwrap();
            for row in rows.flatten() {
                for value in row {
                    if value.contains("atlas checksum") || value.contains("secret title") {
                        leaks += 1;
                    }
                }
            }
        }
        assert_eq!(leaks, 0, "the counters contain the query or record text");
        let schema: String = conn.query_row("SELECT group_concat(sql) FROM sqlite_master", [], |row| row.get(0)).unwrap();
        assert!(!schema.contains("query TEXT") && !schema.contains("text TEXT"), "{schema}");
    }

    #[test]
    fn an_opened_or_cited_result_sets_the_hit_position_once() {
        let (_dir, conn) = scratch("hits");
        let hits = [found("a", &["words"]), found("b", &["words", "phrase"]), found("c", &["terms"])];
        record_find_in(&conn, "some words", &hits, Duration::from_millis(1), "phrase,terms,words", "lead:claude", "s1", 100.0).unwrap();
        record_open_in(&conn, "b", "s1", 101.0).unwrap();
        let (rank, kind): (i64, String) =
            conn.query_row("SELECT hit_rank, hit_kind FROM finds", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!((rank, kind.as_str()), (2, "open"));
        // A second open does not move it.
        record_open_in(&conn, "a", "s1", 102.0).unwrap();
        let rank: i64 = conn.query_row("SELECT hit_rank FROM finds", [], |row| row.get(0)).unwrap();
        assert_eq!(rank, 2);
        // A cited record is the hit too (a new find, cited at rank 3).
        record_find_in(&conn, "0x80070005", &hits, Duration::from_millis(1), "terms,words", "member:codex", "s1", 200.0).unwrap();
        record_cite_in(&conn, &["c".to_owned()], "s1", 201.0).unwrap();
        let ranks: Vec<(i64, Option<i64>, Option<String>)> = {
            let mut statement = conn.prepare("SELECT id, hit_rank, hit_kind FROM finds ORDER BY id").unwrap();
            statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap().flatten().collect()
        };
        assert_eq!(ranks[0].1, Some(2));
        assert_eq!(ranks[1].1, Some(3));
        assert_eq!(ranks[1].2.as_deref(), Some("cite"));
        // A session with no open/cite never gets a position.
        record_find_in(&conn, "lonely", &hits, Duration::from_millis(1), "words", "human", "", 300.0).unwrap();
        record_open_in(&conn, "a", "", 301.0).unwrap();
        let lonely: Option<i64> = conn.query_row("SELECT hit_rank FROM finds WHERE session = ''", [], |row| row.get(0)).unwrap();
        assert_eq!(lonely, None);
    }

    #[test]
    fn a_find_with_no_opened_result_and_a_new_fault_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("LORE_STATS_DB", dir.path().join("stats.sqlite"));
        std::env::set_var("LORE_SESSION", "miss-session");
        std::env::set_var("LORE_CALLER", "member:codex");
        let config = Config::defaults(dir.path());
        let hits = [found("a", &["words"])];
        record_find(&config, "the build stops", &hits, Duration::from_millis(1), "words,terms");
        let first = report(&config, 30);
        assert_eq!((first.finds, first.misses, first.opened), (1, 0, 0));
        record_write(&config, &[serde_json::json!({"kind": "fault", "id": "new-fault"})]);
        assert_eq!(report(&config, 30).misses, 1);
        record_find(&config, "the build stops", &hits, Duration::from_millis(1), "words,terms");
        record_open(&config, "a");
        record_write(&config, &[serde_json::json!({"kind": "fault", "id": "another-fault"})]);
        let second = report(&config, 30);
        assert_eq!((second.finds, second.misses, second.opened, second.hit1), (2, 1, 1, 1));
        std::env::remove_var("LORE_STATS_DB");
        std::env::remove_var("LORE_SESSION");
        std::env::remove_var("LORE_CALLER");
    }

    #[test]
    fn the_counters_are_bounded_by_age_and_count() {
        let (_dir, conn) = scratch("prune");
        let at = 10_000_000.0;
        for n in 0..6 {
            conn.execute(
                "INSERT INTO finds (at, session, caller, class, tiers, results, latency_ms) VALUES (?1,'s','c','exact','words',1,1)",
                rusqlite::params![at - f64::from(n) * DAY],
            )
            .unwrap();
        }
        prune_with(&conn, at, 2, 3).unwrap();
        let count: i64 = conn.query_row("SELECT count(*) FROM finds", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 3, "the row cap keeps the newest");
        let oldest: f64 = conn.query_row("SELECT min(at) FROM finds", [], |row| row.get(0)).unwrap();
        assert!(oldest >= at - 2.0 * DAY, "the age cap drops older rows");
    }

    #[test]
    fn the_report_counts_by_class_and_caller_and_keeps_only_counts() {
        let (_dir, conn) = scratch("report");
        let at = now();
        let hits = [found("a", &["words"]), found("b", &["words"])];
        for (class, caller, session, rank) in [
            ("exact", "lead:claude", "s1", Some(1_i64)),
            ("exact", "lead:claude", "s2", Some(2)),
            ("paraphrase", "member:codex", "s3", Some(3)),
            ("paraphrase", "member:codex", "s4", Some(6)),
            ("paraphrase", "human", "", None),
        ] {
            record_find_in(
                &conn,
                if class == "exact" { "0x80070005" } else { "a plain question" },
                &hits,
                Duration::from_millis(2),
                "words,terms",
                caller,
                session,
                at,
            )
            .unwrap();
            if let Some(rank) = rank {
                let id: i64 = conn.query_row("SELECT max(id) FROM finds", [], |row| row.get(0)).unwrap();
                conn.execute("UPDATE finds SET hit_rank=?1, hit_kind='open' WHERE id=?2", rusqlite::params![rank, id]).unwrap();
            }
        }
        let report = read_report(&conn, 30);
        assert_eq!(report.finds, 5);
        assert_eq!(report.by_class["exact"].finds, 2);
        assert_eq!(report.by_class["exact"].hit1, 1);
        assert_eq!(report.by_class["paraphrase"].finds, 3);
        assert_eq!(report.by_class["paraphrase"].hit5, 1);
        assert_eq!(report.by_caller["lead:claude"].finds, 2);
        assert_eq!(report.by_caller["human"].opened, 0);
    }

    /// The report over an explicit connection (the command reads the same
    /// rows through its file).
    fn read_report(conn: &rusqlite::Connection, days: i64) -> Report {
        let cutoff = now() - days as f64 * DAY;
        let mut report = Report { window_days: days, ..Default::default() };
        let (overall, latency) = read_metric(conn, cutoff, None);
        report.finds = overall.finds;
        report.opened = overall.opened;
        report.hit1 = overall.hit1;
        report.hit5 = overall.hit5;
        report.latency_ms = latency;
        for class in ["exact", "paraphrase"] {
            let (metric, _) = read_metric(conn, cutoff, Some(class));
            report.by_class.insert(class.to_owned(), metric);
        }
        let mut statement = conn
            .prepare("SELECT caller, count(*), sum(hit_rank IS NOT NULL), sum(hit_rank <= 1), sum(hit_rank <= 5) FROM finds WHERE at >= ?1 GROUP BY caller")
            .unwrap();
        let rows = statement
            .query_map(rusqlite::params![cutoff], |row| {
                Ok((row.get::<_, String>(0)?, Metric { finds: row.get(1)?, opened: row.get::<_, Option<i64>>(2)?.unwrap_or(0), hit1: row.get::<_, Option<i64>>(3)?.unwrap_or(0), hit5: row.get::<_, Option<i64>>(4)?.unwrap_or(0) }))
            })
            .unwrap();
        for row in rows.flatten() {
            report.by_caller.insert(row.0, row.1);
        }
        report
    }
}
