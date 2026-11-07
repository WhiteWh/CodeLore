//! The duplicate search before a new record (CodeLore's `dedupe.py`).
//!
//! A write is refused only on strong evidence - an exact signal another
//! record already carries, near-identical text (cosine >= `dedupe.cosine`),
//! or a word overlap (>= `dedupe.jaccard`) - unless each title names
//! something the other record never mentions. Weaker likeness only names the
//! neighbour.
use crate::config::Config;
use crate::schema::{main_text, KNOWLEDGE};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

const STOP: &str = "a an the and or of to in on at for from by with without is are was were be been
it its this that these those as not no into than then when while which who what why how do does
did can cannot could should would will just only also there here out up so if but all any each";

/// Below this a neighbour is not worth naming.
pub const NEAR_COSINE: f64 = 0.80;
pub const NEAR_JACCARD: f64 = 0.25;

fn stop() -> &'static BTreeSet<&'static str> {
    static SET: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| STOP.split_whitespace().collect())
}

fn term_pattern() -> &'static regex::Regex {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        regex::Regex::new(r"0x[0-9A-Fa-f]{4,}|[A-Za-z0-9_]+(?:[./\\:-][A-Za-z0-9_]+)+|[A-Za-z0-9]*_[A-Za-z0-9_]+|\b[A-Z][A-Z0-9]{2,}\b").expect("the term pattern")
    })
}

/// Tokens that only mean something whole: `scene.depth_buffer`,
/// `ERR_DEVICE_LOST`, `0x80070005`, `src/render/post.py`, `E_FAIL`.
pub fn terms_of(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for found in term_pattern().find_iter(text) {
        let term = found.as_str().replace('\\', "/");
        let term = term.trim_matches(|ch| ".:-/".contains(ch)).to_owned();
        if term.chars().count() >= 3 && seen.insert(term.to_lowercase()) {
            out.push(term);
        }
    }
    out
}

fn get_str<'a>(record: &'a Value, key: &str) -> &'a str {
    record.get(key).and_then(Value::as_str).unwrap_or_default()
}

pub fn words(record: &Value) -> BTreeSet<String> {
    static WORD: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = WORD.get_or_init(|| regex::Regex::new(r"[a-z0-9_]{3,}").expect("the word pattern"));
    let text = [get_str(record, "title"), &main_text(record), get_str(record, "cause")].join(" ").to_lowercase();
    pattern.find_iter(&text).map(|found| found.as_str().to_owned()).filter(|word| !stop().contains(word.as_str())).collect()
}

fn word_char(ch: Option<char>) -> bool {
    ch.is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
}

/// The quoted spans of a title: `x`, «x», "x", and 'x' standing alone.
fn quoted(title: &str) -> Vec<String> {
    let chars: Vec<char> = title.chars().collect();
    let mut found = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let close = match chars[index] {
            '`' => Some(('`', 1)),
            '«' => Some(('»', 1)),
            '"' => Some(('"', 1)),
            '\'' if !word_char(index.checked_sub(1).map(|at| chars[at])) => Some(('\'', 2)),
            _ => None,
        };
        if let Some((end, least)) = close {
            if let Some(offset) = chars[index + 1..].iter().position(|ch| *ch == end) {
                let inner: String = chars[index + 1..index + 1 + offset].iter().collect();
                let after = chars.get(index + 2 + offset).copied();
                let alone = end != '\'' || !word_char(after);
                if inner.chars().count() >= least && alone {
                    found.push(inner);
                    index += offset + 2;
                    continue;
                }
            }
        }
        index += 1;
    }
    found
}

/// What a record's title names: quoted spans, code-like tokens, CamelCase words.
pub fn names(record: &Value) -> BTreeSet<String> {
    static CAMEL: OnceLock<regex::Regex> = OnceLock::new();
    let camel = CAMEL.get_or_init(|| regex::Regex::new(r"[A-Za-z][a-z0-9]+[A-Z][A-Za-z0-9]*").expect("the camel pattern"));
    let title = get_str(record, "title");
    let mut found: Vec<String> = quoted(title);
    found.extend(terms_of(title));
    found.extend(camel.find_iter(title).map(|word| word.as_str().to_owned()));
    found.into_iter().map(|name| name.trim().to_lowercase()).filter(|name| name.chars().count() >= 2).collect()
}

/// Every string and list of the record, lower-cased, a list as Python prints it.
pub fn text_of(record: &Value) -> String {
    let Some(map) = record.as_object() else { return String::new() };
    map.values()
        .filter_map(|value| match value {
            Value::String(text) => Some(text.clone()),
            Value::Array(_) => Some(crate::py::repr(value)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Each title names something the other record never mentions.
pub fn names_differ(a: &Value, b: &Value, text_a: &str, text_b: &str) -> bool {
    let (names_a, names_b) = (names(a), names(b));
    if names_a.is_empty() || names_b.is_empty() {
        return false;
    }
    names_a.iter().any(|name| !text_b.contains(name.as_str())) && names_b.iter().any(|name| !text_a.contains(name.as_str()))
}

pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(b).count() as f64 / a.union(b).count() as f64
}

#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub id: String,
    pub similarity: f64,
    /// "signal", "meaning", "words", "words, names differ".
    pub how: String,
    pub blocking: bool,
}

/// Records' meaning vectors, when an embedder is configured: the existing
/// records' by id, and the new records' in order. None is text only.
pub struct Vectors {
    pub existing: BTreeMap<String, Vec<f32>>,
    pub new: Vec<Vec<f32>>,
}

fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum()
}

/// For each new knowledge record (by position), the existing or earlier batch
/// records that look like it, strongest first, at most three.
pub fn candidates(config: &Config, existing: &[Value], new: &[Value], vectors: Option<&Vectors>) -> BTreeMap<usize, Vec<Match>> {
    let existing: Vec<&Value> = existing.iter().filter(|record| KNOWLEDGE.contains(&get_str(record, "kind"))).collect();
    let mut memo: BTreeMap<*const Value, (BTreeSet<String>, String)> = BTreeMap::new();
    let mut seen = |record: &Value| -> (BTreeSet<String>, String) {
        memo.entry(record as *const Value).or_insert_with(|| (words(record), text_of(record))).clone()
    };
    let mut result = BTreeMap::new();
    for (index, record) in new.iter().enumerate() {
        if !KNOWLEDGE.contains(&get_str(record, "kind")) {
            continue;
        }
        let (own_words, own_text) = seen(record);
        let signals: BTreeSet<String> = record.get("signals").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_lowercase).collect();
        let mut pool: Vec<(&Value, Option<&Vec<f32>>)> = existing
            .iter()
            .filter(|other| other.get("kind") == record.get("kind"))
            .map(|other| (*other, vectors.and_then(|vectors| vectors.existing.get(get_str(other, "id")))))
            .collect();
        pool.extend(new[..index].iter().enumerate().filter(|(_, other)| other.get("kind") == record.get("kind")).map(|(at, other)| (other, vectors.and_then(|vectors| vectors.new.get(at)))));
        let own_vector = vectors.and_then(|vectors| vectors.new.get(index));
        let mut found = Vec::new();
        for (other, other_vector) in pool {
            let id = match other.get("id").filter(|id| !crate::ops::falsy(id)) {
                Some(id) => id.as_str().map(str::to_owned).unwrap_or_else(|| id.to_string()),
                None => other.get("title").and_then(Value::as_str).unwrap_or("?").to_owned(),
            };
            let other_signals: BTreeSet<String> = other.get("signals").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_lowercase).collect();
            if !signals.is_disjoint(&other_signals) {
                found.push(Match { id, similarity: 1.0, how: "signal".into(), blocking: true });
                continue;
            }
            let (other_words, other_text) = seen(other);
            let overlap = jaccard(&own_words, &other_words);
            let cosine = match (own_vector, other_vector) {
                (Some(own), Some(theirs)) => Some(dot(own, theirs)),
                _ => None,
            };
            let rounded = |value: f64| crate::py::round(value, 3);
            if let Some(cosine) = cosine.filter(|cosine| *cosine >= config.dedupe_cosine) {
                found.push(Match { id, similarity: rounded(cosine), how: "meaning".into(), blocking: true });
            } else if overlap >= config.dedupe_jaccard {
                if names_differ(record, other, &own_text, &other_text) {
                    found.push(Match { id, similarity: rounded(overlap), how: "words, names differ".into(), blocking: false });
                } else {
                    found.push(Match { id, similarity: rounded(overlap), how: "words".into(), blocking: true });
                }
            } else if let Some(cosine) = cosine.filter(|cosine| *cosine >= NEAR_COSINE) {
                found.push(Match { id, similarity: rounded(cosine), how: "meaning".into(), blocking: false });
            } else if cosine.is_none() && overlap >= NEAR_JACCARD {
                found.push(Match { id, similarity: rounded(overlap), how: "words".into(), blocking: false });
            }
        }
        if !found.is_empty() {
            found.sort_by(|a, b| (!a.blocking).cmp(&!b.blocking).then(b.similarity.partial_cmp(&a.similarity).unwrap_or(std::cmp::Ordering::Equal)));
            found.truncate(3);
            result.insert(index, found);
        }
    }
    result
}
