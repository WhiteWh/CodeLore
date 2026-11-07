//! The search seam. `find` ranks through the index (FTS5 words and terms,
//! vectors, RRF - the index's own module), which plugs in through `Finder`;
//! what needs no index is here, as CodeLore's `search.py` has it: which
//! records a search may return, `lore for`, and the lines of a pasted log
//! worth searching for. No Zerus type appears in this crate.
use crate::config::Config;
use crate::schema::RETIRED;
use crate::LoreError;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub id: String,
    pub title: String,
    pub score: f32,
}

/// A corpus the search fuses with the lore store. Zerus implements this for
/// session history; the crate only sees the trait.
pub trait Source: Send + Sync {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, LoreError>;
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filters {
    pub kind: Option<String>,
    pub area: Option<String>,
    pub status: Option<String>,
    pub plan: Option<String>,
    pub all: bool,
    /// Source records too, without `--kind source`.
    pub sources: bool,
}

/// One result of `lore find`: `{"score", "via", "record"}`.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub record: Value,
    pub score: f64,
    pub via: Vec<String>,
    /// Task 133: the cosine of the meaning list, when it found the record.
    pub meaning: Option<f64>,
}

/// Task 133 (owner, 2026-11-07): how far a find result may be trusted - its
/// tier, whether it matched exactly (a signal or the whole phrase), its
/// semantic score, its age in days and whether it is superseded - and a
/// level that sums them: `high`, `medium` or `low`.
pub fn confidence(tier: &str, via: &[String], semantic: Option<f64>, age_days: Option<i64>, superseded: bool) -> Value {
    let exact = via.iter().any(|list| list == "signal" || list == "phrase");
    let level = if superseded || tier == "history" {
        "low"
    } else if tier == "working" {
        if age_days.is_some_and(|days| days <= 1) { "medium" } else { "low" }
    } else if exact || semantic.is_some_and(|cosine| cosine >= 0.8) {
        if age_days.is_some_and(|days| days > 365) { "medium" } else { "high" }
    } else if semantic.is_some_and(|cosine| cosine >= 0.6) || via.len() >= 2 {
        "medium"
    } else {
        "low"
    };
    serde_json::json!({"tier": tier, "exact": exact, "semantic": semantic, "age_days": age_days, "superseded": superseded, "level": level})
}

/// A record's age in days from its `updated` date (UTC), at `now`.
pub fn record_age(record: &Value, now: i64) -> Option<i64> {
    let day = record.get("updated").and_then(Value::as_str)?;
    let date = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let at = date.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
    Some(((now - at) / 86_400).max(0))
}

/// Whether a record no longer holds: superseded, obsolete or reversed.
pub fn retired(record: &Value) -> bool {
    matches!(record.get("status").and_then(Value::as_str), Some("superseded" | "obsolete" | "reversed"))
}

/// What `lore index` reports: `{"records", "vectors", "index", "semantic", "note"}`.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexInfo {
    pub records: usize,
    pub vectors: usize,
    pub index: String,
    pub semantic: bool,
    pub note: Option<String>,
}

/// The ranked search over a store's records: the index implements it.
pub trait Finder {
    /// The best records for the query, and notes about how they were found.
    fn find(&self, config: &Config, records: &[Value], query: &str, filters: &Filters, limit: usize, text_only: bool) -> Result<(Vec<Found>, Vec<String>), LoreError>;
    /// `lore index [--rebuild]`.
    fn index(&self, config: &Config, records: &[Value], rebuild: bool, text_only: bool) -> Result<IndexInfo, LoreError>;
    /// A note to print once, such as where the index had to go.
    fn place_note(&self, _config: &Config) -> Option<String> {
        None
    }
}

fn text<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    record.get(key).and_then(Value::as_str)
}

/// The records a search may return, by id.
pub fn visible(records: &[Value], filters: &Filters) -> BTreeMap<String, Value> {
    let by_id: BTreeMap<String, &Value> = records.iter().filter_map(|record| text(record, "id").map(|id| (id.to_owned(), record))).collect();
    let closed: BTreeSet<&String> = by_id.iter().filter(|(_, record)| text(record, "kind") == Some("plan") && text(record, "status") == Some("closed")).map(|(id, _)| id).collect();
    let mut result = BTreeMap::new();
    for (id, record) in &by_id {
        let kind = text(record, "kind");
        if let Some(want) = &filters.kind {
            if kind != Some(want.as_str()) {
                continue;
            }
        } else if kind == Some("source") && !filters.sources {
            continue; // a ledger of a document, not knowledge
        }
        if filters.area.as_deref().is_some_and(|area| text(record, "area") != Some(area)) {
            continue;
        }
        if let Some(plan) = &filters.plan {
            if text(record, "plan") != Some(plan.as_str()) && id != plan {
                continue;
            }
        }
        if let Some(status) = &filters.status {
            if text(record, "status") != Some(status.as_str()) {
                continue;
            }
        } else if !filters.all {
            if text(record, "status").is_some_and(|status| RETIRED.contains(&status)) {
                continue;
            }
            if kind == Some("item") && text(record, "plan").is_some_and(|plan| closed.contains(&plan.to_owned())) {
                continue;
            }
        }
        result.insert(id.clone(), (*record).clone());
    }
    result
}

/// Every visible record whose refs name the file, a file under the directory,
/// or the symbol; a source record names its document.
pub fn for_target(config: &Config, records: &[Value], target: &str, filters: &Filters) -> Vec<Value> {
    let mut wanted = target.replace('\\', "/").trim().to_owned();
    let root = format!("{}/", config.root.to_string_lossy().replace('\\', "/").trim_end_matches('/'));
    if wanted.to_lowercase().starts_with(&root.to_lowercase()) {
        wanted = wanted[root.len()..].to_owned();
    }
    if let Some(rest) = wanted.strip_prefix("./") {
        wanted = rest.to_owned();
    }
    let folder = format!("{}/", wanted.trim_end_matches('/'));
    let filters = Filters { sources: true, ..filters.clone() };
    let mut result: Vec<Value> = Vec::new();
    for record in visible(records, &filters).into_values() {
        let mut hit = text(&record, "kind") == Some("source") && text(&record, "path").is_some_and(|path| path == wanted || path == wanted.trim_end_matches('/'));
        let refs = record.get("refs");
        let list = |key: &str| -> Vec<String> { refs.and_then(|refs| refs.get(key)).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect() };
        for path in list("files") {
            if path == wanted || path.starts_with(&folder) || wanted.starts_with(&format!("{}/", path.trim_end_matches('/'))) {
                hit = true;
            }
        }
        for symbol in list("symbols") {
            if symbol == wanted || symbol.starts_with(&format!("{wanted}.")) || wanted.starts_with(&format!("{symbol}.")) || symbol.ends_with(&format!(".{wanted}")) {
                hit = true;
            }
        }
        if hit {
            result.push(record);
        }
    }
    result.sort_by_key(|record| (text(record, "kind").unwrap_or_default().to_owned(), text(record, "id").unwrap_or_default().to_owned()));
    result
}

/// The lines of a pasted log or traceback worth searching for.
pub fn errors_of(log: &str) -> String {
    static ERRORISH: OnceLock<regex::Regex> = OnceLock::new();
    static NOISE: OnceLock<regex::Regex> = OnceLock::new();
    let errorish = ERRORISH.get_or_init(|| {
        regex::Regex::new(r"(?i)(error|exception|fatal|failed|failure|refused|denied|cannot|can't|unable|not found|HRESULT|0x[0-9a-f]{8}|exit (code|status)|segfault|assert|panic|warning:)").expect("errorish")
    });
    let noise = NOISE.get_or_init(|| regex::Regex::new(r#"^\s*(File "|at |Traceback \(most recent call last\)|\^+\s*$|~+\s*$)"#).expect("noise"));
    let mut keep: Vec<String> = Vec::new();
    for line in log.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || noise.is_match(line) {
            continue;
        }
        if errorish.is_match(stripped) && !keep.iter().any(|kept| kept == stripped) {
            keep.push(stripped.chars().take(400).collect());
        }
    }
    if keep.is_empty() {
        let all = log.trim();
        let count = all.chars().count();
        return all.chars().skip(count.saturating_sub(2000)).collect();
    }
    let start = keep.len().saturating_sub(12);
    keep[start..].join("\n")
}
