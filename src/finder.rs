//! `find` and `for`: exact signals first, then reciprocal rank fusion of three
//! lists: words (BM25), whole terms (BM25) and meaning (cosine); a phrase
//! bonus. A port of CodeLore's `search.py`, tie for tie. The search seam
//! (`Source`) stays: a local index today, Zerus's session history later. No
//! Zerus type appears in this crate.
use crate::embed::Embeddings;
use crate::index::{terms_of, word_re, Index};
use crate::LoreError;
use regex::Regex;
use rusqlite::params;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

pub const RRF_K: f64 = 60.0;
pub const DEPTH: usize = 50;
/// BM25 weights for the word table's columns: signals, title, main, body.
const WEIGHTS: [f64; 4] = [50.0, 20.0, 1.0, 1.0];
/// Statuses that retire a record from ordinary search.
pub const RETIRED: [&str; 4] = ["obsolete", "superseded", "reversed", "closed"];
/// A query shorter than this is a few words, not a phrase someone copied.
const PHRASE_MIN: usize = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub id: String,
    pub title: String,
    pub score: f64,
    /// The lists that found it: signal, phrase, words, terms, meaning.
    pub via: Vec<String>,
    /// Task 133: its cosine with the query when the meaning list found it.
    pub meaning: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct Filters {
    pub kind: Option<String>,
    pub area: Option<String>,
    pub status: Option<String>,
    pub plan: Option<String>,
    pub all: bool,
    /// Source records too, without `kind = source`.
    pub sources: bool,
}

/// A corpus the search fuses with the lore store. Zerus implements this for
/// session history; the crate only sees the trait.
pub trait Source: Send + Sync {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>, LoreError>;
}

/// The embedding side of a search: the provider, its model and the prefixes.
pub struct Semantic<'a> {
    pub embedder: &'a dyn Embeddings,
    pub model: &'a str,
    pub doc_prefix: &'a str,
    pub query_prefix: &'a str,
}

fn quote(token: &str) -> String {
    format!("\"{}\"", token.replace('"', "\"\""))
}

fn text<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    record.get(key).and_then(Value::as_str)
}

/// The records a search may return: by id, in the order the index holds them.
pub fn visible(index: &Index, filters: &Filters) -> Result<Vec<(String, Value)>, LoreError> {
    let mut order: Vec<(String, Value)> = Vec::new();
    let mut position: HashMap<String, usize> = HashMap::new();
    for record in index.all()? {
        let Some(id) = text(&record, "id").map(str::to_owned) else { continue };
        match position.get(&id) {
            Some(&at) => order[at].1 = record,
            None => {
                position.insert(id.clone(), order.len());
                order.push((id, record));
            }
        }
    }
    let closed_plans: HashSet<String> = order
        .iter()
        .filter(|(_, record)| text(record, "kind") == Some("plan") && text(record, "status") == Some("closed"))
        .map(|(id, _)| id.clone())
        .collect();
    let mut out = Vec::new();
    for (id, record) in &order {
        let kind = text(record, "kind");
        if filters.kind.as_deref().is_some_and(|wanted| kind != Some(wanted)) {
            continue;
        }
        if filters.kind.is_none() && kind == Some("source") && !filters.sources {
            continue; // a ledger of a document, not knowledge: found with --kind source
        }
        if filters.area.as_deref().is_some_and(|wanted| text(record, "area") != Some(wanted)) {
            continue;
        }
        if let Some(plan) = filters.plan.as_deref() {
            if text(record, "plan") != Some(plan) && id != plan {
                continue;
            }
        }
        if let Some(status) = filters.status.as_deref() {
            if text(record, "status") != Some(status) {
                continue;
            }
        } else if !filters.all {
            if text(record, "status").is_some_and(|status| RETIRED.contains(&status)) {
                continue;
            }
            if kind == Some("item") && text(record, "plan").is_some_and(|plan| closed_plans.contains(plan)) {
                continue;
            }
        }
        out.push((id.clone(), record.clone()));
    }
    Ok(out)
}

/// The ids a table's MATCH ranks best, at most four pages deep; a query SQLite
/// refuses finds nothing.
fn fts(index: &Index, table: &str, query: &str, weighted: bool) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }
    let rank = if weighted {
        let weights: Vec<String> = WEIGHTS.iter().map(|weight| format!("{weight:?}")).collect();
        format!("bm25({table}, 0, {})", weights.join(", "))
    } else {
        format!("bm25({table})")
    };
    let sql = format!("SELECT id FROM {table} WHERE {table} MATCH ?1 ORDER BY {rank} LIMIT ?2");
    let run = || -> rusqlite::Result<Vec<String>> {
        let mut statement = index.db().prepare(&sql)?;
        let rows = statement.query_map(params![query, (DEPTH * 4) as i64], |row| row.get::<_, String>(0))?;
        rows.collect()
    };
    run().unwrap_or_default()
}

/// The FTS5 query of a text's words: lower-cased, each once, at most 64.
pub fn word_query(text: &str) -> String {
    let mut seen: Vec<String> = Vec::new();
    for found in word_re().find_iter(text) {
        let token = found.as_str().to_lowercase();
        if !seen.contains(&token) {
            seen.push(token);
        }
    }
    seen.iter().take(64).map(|token| quote(token)).collect::<Vec<_>>().join(" OR ")
}

/// The FTS5 query of a text's whole terms, at most 32.
pub fn term_query(text: &str) -> String {
    terms_of(text).iter().take(32).map(|token| quote(token)).collect::<Vec<_>>().join(" OR ")
}

/// Records one of whose exact strings is in the query, or which hold the
/// query in one of them. Longest match first.
fn signal_hits(query: &str, visible: &[(String, Value)]) -> Vec<(String, usize)> {
    let q = query.trim();
    let ql = q.to_lowercase();
    let q_len = q.chars().count();
    let mut hits: Vec<(String, usize)> = Vec::new();
    for (id, record) in visible {
        let mut best = 0usize;
        for signal in record.get("signals").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            let lower = signal.to_lowercase();
            let length = signal.chars().count();
            if (length >= 6 && ql.contains(&lower)) || (q_len >= 8 && lower.contains(&ql)) {
                best = best.max(length);
            }
        }
        if best > 0 {
            hits.push((id.clone(), best));
        }
    }
    hits.sort_by_key(|hit| std::cmp::Reverse(hit.1)); // stable: ties keep the index's order
    hits
}

/// Lower-cased, markup and quotes removed, whitespace one space.
pub fn flat(text: &str) -> String {
    let cleaned: String = text.to_lowercase().chars().filter(|c| !matches!(c, '`' | '*' | '«' | '»' | '"' | '\'')).collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Records whose own text holds the whole query, word for word - what a
/// fixed-string grep finds. A bag of common words ranks a long record that has
/// many of them above the short one that says exactly this; the phrase puts
/// that one back.
fn phrase_hits(query: &str, visible: &[(String, Value)]) -> Vec<String> {
    let q = flat(query);
    if q.chars().count() < PHRASE_MIN {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (id, record) in visible {
        let Some(map) = record.as_object() else { continue };
        let joined = map
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "id" | "kind" | "area" | "status" | "by"))
            .filter_map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if flat(&joined).contains(&q) {
            hits.push(id.clone());
        }
    }
    hits
}

pub(crate) fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum()
}

/// Scores in the order records were first scored, as Python's dict keeps them.
#[derive(Default)]
struct Scores {
    rows: Vec<(String, f64, Vec<String>)>,
    at: HashMap<String, usize>,
}

impl Scores {
    fn add(&mut self, id: &str, by: f64, via: &str, front: bool) {
        let position = match self.at.get(id) {
            Some(&position) => position,
            None => {
                self.rows.push((id.to_owned(), 0.0, Vec::new()));
                self.at.insert(id.to_owned(), self.rows.len() - 1);
                self.rows.len() - 1
            }
        };
        self.rows[position].1 += by;
        if front {
            self.rows[position].2.insert(0, via.to_owned());
        } else {
            self.rows[position].2.push(via.to_owned());
        }
    }
}

/// The best records for the query, and notes about how they were found.
pub fn find(
    index: &Index,
    query: &str,
    filters: &Filters,
    limit: usize,
    semantic: Option<&Semantic<'_>>,
) -> Result<(Vec<Hit>, Vec<String>), LoreError> {
    let mut notes: Vec<String> = Vec::new();
    let visible = visible(index, filters)?;
    let loaded = match semantic {
        Some(semantic) => match index.ensure_vectors(semantic.embedder, semantic.model, semantic.doc_prefix) {
            Ok(()) => match index.vectors(semantic.model) {
                Ok(vectors) => Some((semantic.model.to_owned(), vectors.into_iter().collect::<HashMap<String, Vec<f32>>>())),
                Err(error) => {
                    notes.push(format!("semantic search unavailable: {error}; text only"));
                    None
                }
            },
            Err(error) => {
                notes.push(format!("semantic search unavailable: {error}; text only"));
                None
            }
        },
        None => None,
    };
    let vectors = loaded.as_ref().map(|(model, vectors)| (model.as_str(), vectors));
    find_ranked(index, &visible, vectors, query, limit, semantic, notes)
}

/// The same ranking over records and vectors the caller loaded once: a caller
/// that asks many queries of one store (the bench) reads the visible records
/// and the model's vectors once and reuses them.
pub fn find_with(
    index: &Index,
    visible: &[(String, Value)],
    vectors: &HashMap<String, Vec<f32>>,
    model: &str,
    query: &str,
    limit: usize,
    semantic: Option<&Semantic<'_>>,
) -> Result<(Vec<Hit>, Vec<String>), LoreError> {
    find_ranked(index, visible, Some((model, vectors)), query, limit, semantic, Vec::new())
}

fn find_ranked(
    index: &Index,
    visible: &[(String, Value)],
    vectors: Option<(&str, &HashMap<String, Vec<f32>>)>,
    query: &str,
    limit: usize,
    semantic: Option<&Semantic<'_>>,
    mut notes: Vec<String>,
) -> Result<(Vec<Hit>, Vec<String>), LoreError> {
    let visible_ids: HashSet<&str> = visible.iter().map(|(id, _)| id.as_str()).collect();
    let take = |ids: Vec<String>| -> Vec<String> { ids.into_iter().filter(|id| visible_ids.contains(id.as_str())).take(DEPTH).collect() };
    let mut lists: Vec<(&str, Vec<String>)> = Vec::new();
    let mut cosines: HashMap<String, f64> = HashMap::new();
    lists.push(("words", take(fts(index, "words", &word_query(query), true))));
    lists.push(("terms", take(fts(index, "terms", &term_query(query), false))));
    if let (Some(semantic), Some((_model, loaded))) = (semantic, vectors) {
        let truncated: String = query.chars().take(4000).collect();
        match semantic.embedder.embed(&[format!("{}{truncated}", semantic.query_prefix)]) {
            Ok(mut wanted) if !wanted.is_empty() => {
                let wanted = wanted.remove(0);
                let mut scored: Vec<(f64, String)> = loaded
                    .iter()
                    .filter(|(id, _)| visible_ids.contains(id.as_str()))
                    .map(|(id, vector)| (dot(&wanted, vector), id.clone()))
                    .collect();
                // Python sorts (score, id) pairs in reverse.
                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| b.1.cmp(&a.1)));
                let kept: Vec<(f64, String)> = scored.into_iter().take(DEPTH).collect();
                cosines.extend(kept.iter().map(|(cosine, id)| (id.clone(), *cosine)));
                lists.push(("meaning", kept.into_iter().map(|(_, id)| id).collect()));
            }
            Ok(_) => notes.push("semantic search unavailable: the provider returned no vector; text only".into()),
            Err(error) => notes.push(format!("semantic search unavailable: {error}; text only")),
        }
    }
    let mut scores = Scores::default();
    for (name, ids) in &lists {
        for (rank, id) in ids.iter().enumerate() {
            scores.add(id, 1.0 / (RRF_K + rank as f64 + 1.0), name, false);
        }
    }
    for id in phrase_hits(query, visible) {
        scores.add(&id, 0.5, "phrase", true);
    }
    for (id, length) in signal_hits(query, visible) {
        scores.add(&id, 1.0 + length as f64 / 1000.0, "signal", true);
    }
    let mut rows = scores.rows;
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)); // stable
    rows.truncate(limit);
    let by_id: HashMap<&str, &Value> = visible.iter().map(|(id, record)| (id.as_str(), record)).collect();
    let hits = rows
        .into_iter()
        .map(|(id, score, via)| {
            let title = by_id.get(id.as_str()).and_then(|record| text(record, "title")).unwrap_or_default().to_owned();
            let meaning = cosines.get(&id).map(|cosine| (cosine * 1e4).round() / 1e4);
            Hit { id, title, score: (score * 1e5).round() / 1e5, via, meaning }
        })
        .collect();
    Ok((hits, notes))
}

fn error_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(error|exception|fatal|failed|failure|refused|denied|cannot|can't|unable|not found|HRESULT|0x[0-9a-f]{8}|exit (code|status)|segfault|assert|panic|warning:)",
        )
        .expect("the error pattern is valid")
    })
}

fn noise_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"^\s*(File "|at |Traceback \(most recent call last\)|\^+\s*$|~+\s*$)"#).expect("the noise pattern is valid")
    })
}

/// The lines of a pasted log or traceback worth searching for.
pub fn errors_of(text: &str) -> String {
    let mut keep: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || noise_re().is_match(line) {
            continue;
        }
        if error_re().is_match(stripped) && seen.insert(stripped.to_owned()) {
            keep.push(stripped.chars().take(400).collect());
        }
    }
    if keep.is_empty() {
        let trimmed = text.trim();
        let skip = trimmed.chars().count().saturating_sub(2000);
        return trimmed.chars().skip(skip).collect();
    }
    let from = keep.len().saturating_sub(12);
    keep[from..].join("\n")
}

/// Every visible record whose refs name the file, a file under the directory,
/// or the symbol.
pub fn for_target(index: &Index, target: &str, filters: &Filters) -> Result<Vec<Value>, LoreError> {
    let mut t = target.replace('\\', "/").trim().to_owned();
    let root = format!("{}/", index.root().to_string_lossy().replace('\\', "/").trim_end_matches('/'));
    if t.to_lowercase().starts_with(&root.to_lowercase()) {
        t = t[root.len()..].to_owned();
    }
    let t = t.strip_prefix("./").unwrap_or(&t).to_owned();
    let dir = format!("{}/", t.trim_end_matches('/'));
    let widened = Filters { sources: true, ..filters.clone() };
    let mut out = Vec::new();
    for (_, record) in visible(index, &widened)? {
        let refs = record.get("refs");
        let mut hit = text(&record, "kind") == Some("source")
            && text(&record, "path").is_some_and(|path| path == t || path == t.trim_end_matches('/'));
        for path in refs.and_then(|refs| refs.get("files")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if path == t || path.starts_with(&dir) || t.starts_with(&format!("{}/", path.trim_end_matches('/'))) {
                hit = true;
            }
        }
        for symbol in refs.and_then(|refs| refs.get("symbols")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if symbol == t
                || symbol.starts_with(&format!("{t}."))
                || t.starts_with(&format!("{symbol}."))
                || symbol.ends_with(&format!(".{t}"))
            {
                hit = true;
            }
        }
        if hit {
            out.push(record);
        }
    }
    out.sort_by(|a, b| {
        let key = |record: &Value| (text(record, "kind").unwrap_or_default().to_owned(), text(record, "id").unwrap_or_default().to_owned());
        key(a).cmp(&key(b))
    });
    Ok(out)
}

