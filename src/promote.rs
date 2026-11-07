//! Promote (docs/lore.md 11.8) and duplicates (11.10). A working entry or a
//! history excerpt becomes a durable record only under the strict rules: the
//! draft is validated, scanned for secrets and checked for duplicates inside
//! the write, and written in one commit with `Zerus-Promoted: <entry>`. A
//! blocking duplicate that is the same fault becomes an occurrence on it
//! (`seen`); one that disagrees is written and named as a conflict.
use crate::config::Config;
use crate::dedupe;
use crate::history::HistoryHit;
use crate::ops::{self, Change, Options};
use crate::schema::{self, today, KNOWLEDGE, SEEN_MAX, SEEN_TEXT};
use crate::working::Entry;
use crate::{LoreError, Result};
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::BTreeSet;

/// What a promote (or a resolved add) wrote.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Promoted {
    /// The record written, or the one the occurrence went to.
    pub record: String,
    pub occurrence_of: Option<String>,
    pub conflicts_with: Vec<String>,
    pub commit: Option<String>,
    /// The entry had been promoted before: nothing was written.
    pub existing: bool,
    pub notes: Vec<String>,
}

impl Promoted {
    pub fn json(&self) -> Value {
        let mut value = json!({"record": self.record, "commit": self.commit, "notes": self.notes});
        if let Some(id) = &self.occurrence_of {
            value["occurrence_of"] = json!(id);
        }
        if !self.conflicts_with.is_empty() {
            value["conflicts_with"] = json!(self.conflicts_with);
        }
        if self.existing {
            value["existing"] = json!(true);
        }
        value
    }
}

fn first_line(text: &str) -> String {
    text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default().chars().take(120).collect()
}

/// The draft a working entry prefills (11.8). `fields` fill and override.
pub fn draft(entry: &Entry, kind: Option<&str>, fields: &Map<String, Value>, areas: &[String], task_title: Option<&str>) -> Result<Value> {
    let kind = match (entry.kind.as_str(), kind) {
        ("brief", None | Some("topic")) => "topic",
        ("brief", Some(other)) => return Err(LoreError::new(format!("a brief becomes a topic, not a {other}"))),
        ("message", Some(kind @ ("decision" | "topic"))) => kind,
        ("message", _) => return Err(LoreError::new("a message becomes a decision or a topic: give --kind decision or --kind topic")),
        ("pulse", Some(kind @ ("fault" | "reading" | "topic"))) => kind,
        ("pulse", _) => return Err(LoreError::new("a pulse becomes a fault, a reading or a topic: give --kind")),
        (other, _) => return Err(LoreError::new(format!("a {other} is not promoted"))),
    };
    let mut record = Map::new();
    record.insert("kind".into(), json!(kind));
    if areas.len() == 1 {
        record.insert("area".into(), json!(areas[0]));
    }
    let spec = schema::kind(kind).ok_or_else(|| LoreError::new(format!("unknown kind {kind}")))?;
    record.insert("status".into(), json!(spec.statuses[0]));
    match (entry.kind.as_str(), kind) {
        ("brief", _) => {
            if entry.text.chars().count() > 3_000 {
                return Err(LoreError::new(format!(
                    "refused: the brief has {} characters, too long for a topic (3,000); it is never cut - promote a shorter brief or write the topics yourself",
                    entry.text.chars().count()
                )));
            }
            record.insert("title".into(), json!(task_title.map(str::to_owned).unwrap_or_else(|| format!("Brief: {}", entry.task))));
            record.insert("body".into(), json!(entry.text));
        }
        ("message", "decision") => {
            record.insert("title".into(), json!(first_line(&entry.text)));
            record.insert("question".into(), json!(first_line(&entry.text)));
            record.insert("decided_by".into(), json!(entry.by));
        }
        ("message", _) => {
            record.insert("title".into(), json!(first_line(&entry.text)));
            record.insert("body".into(), json!(entry.text));
        }
        ("pulse", "fault") => {
            record.insert("title".into(), json!(first_line(if entry.blocked.is_empty() { &entry.done } else { &entry.blocked })));
            record.insert("symptom".into(), json!(entry.blocked));
        }
        ("pulse", _) => {
            let text = [entry.done.as_str(), entry.next.as_str()].iter().filter(|part| !part.is_empty()).copied().collect::<Vec<_>>().join("\n");
            record.insert("title".into(), json!(first_line(&text)));
            if kind == "reading" {
                record.insert("what".into(), json!(text));
            } else {
                record.insert("body".into(), json!(text));
            }
        }
        _ => {}
    }
    for (key, value) in fields {
        record.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(record))
}

/// A history excerpt as a quotation, its source named in the words (11.8).
pub fn quote(hit: &HistoryHit) -> String {
    format!("\"{}\" (session {}, message {})", hit.excerpt, hit.session, hit.message_id)
}

/// How a new knowledge record meets the records already there (11.10).
#[derive(Clone, Debug, PartialEq)]
pub enum Resolution {
    New,
    /// The same fault: an occurrence on this record.
    Occurrence(String),
    /// Blocking candidates whose cause or fix disagree.
    Conflict(Vec<String>),
}

fn words(text: &str) -> BTreeSet<String> {
    text.to_lowercase().split(|ch: char| !ch.is_alphanumeric()).filter(|word| word.len() >= 3).map(str::to_owned).collect()
}

/// The cause and fix words of a fault.
fn story(record: &Value) -> BTreeSet<String> {
    let get = |key: &str| record.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    words(&format!("{} {}", get("cause"), get("fix")))
}

pub fn resolve(config: &Config, existing: &[Value], record: &Value) -> Resolution {
    let found = dedupe::candidates(config, existing, std::slice::from_ref(record), None);
    let Some(matches) = found.get(&0) else { return Resolution::New };
    let blocking: Vec<&dedupe::Match> = matches.iter().filter(|m| m.blocking).collect();
    if blocking.is_empty() {
        return Resolution::New;
    }
    let by_id = |id: &str| existing.iter().find(|other| other.get("id").and_then(Value::as_str) == Some(id));
    let mut differ = Vec::new();
    for candidate in blocking {
        let Some(other) = by_id(&candidate.id) else { continue };
        let same = if record.get("kind").and_then(Value::as_str) == Some("fault") {
            let (mine, theirs) = (story(record), story(other));
            let shared = mine.intersection(&theirs).count() as f64;
            let union = mine.union(&theirs).count() as f64;
            candidate.how != "words" || (union > 0.0 && shared / union >= config.dedupe_jaccard)
        } else {
            true
        };
        if same {
            return Resolution::Occurrence(candidate.id.clone());
        }
        differ.push(candidate.id.clone());
    }
    Resolution::Conflict(differ)
}

/// `record` with one more occurrence line, the oldest dropped past 20.
pub fn with_occurrence(record: &Value, by: &str, place: &str) -> Value {
    let mut record = record.clone();
    let line: String = format!("{} {by} {place}", today()).chars().take(SEEN_TEXT).collect();
    let mut seen: Vec<Value> = record.get("seen").and_then(Value::as_array).cloned().unwrap_or_default();
    seen.push(json!(line));
    let from = seen.len().saturating_sub(SEEN_MAX);
    record["seen"] = Value::Array(seen[from..].to_vec());
    record
}

/// Writes `record` under the rules of 11.10, deciding inside the write so two
/// concurrent writers cannot both miss each other: a same fault becomes an
/// occurrence, a differing one is written with its conflicts named.
pub fn write_resolved(config: &Config, record: Value, options: &Options, place: &str) -> Result<Promoted> {
    let decided: RefCell<Resolution> = RefCell::new(Resolution::New);
    let filled: RefCell<String> = RefCell::new(String::new());
    let by = options.by.clone();
    let build = |snapshot: &crate::store::Snapshot| -> Result<Vec<Change>> {
        let existing: Vec<Value> = snapshot.by_id().into_values().collect();
        let record = ops::fill_ids(std::slice::from_ref(&record), snapshot).pop().unwrap_or_default();
        let resolution = if KNOWLEDGE.contains(&record.get("kind").and_then(Value::as_str).unwrap_or_default()) {
            resolve(config, &existing, &record)
        } else {
            Resolution::New
        };
        *filled.borrow_mut() = record.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let change = match &resolution {
            Resolution::Occurrence(id) => {
                let current = existing.iter().find(|other| other.get("id").and_then(Value::as_str) == Some(id.as_str())).cloned().unwrap_or_default();
                Change::update(with_occurrence(&current, &by, place), None)
            }
            _ => Change::new(record),
        };
        *decided.borrow_mut() = resolution;
        Ok(vec![change])
    };
    let outcome = ops::apply(config, &build, options)?;
    let resolution = decided.into_inner();
    let mut promoted = Promoted { commit: outcome.commit, notes: outcome.notes, ..Promoted::default() };
    match resolution {
        Resolution::Occurrence(id) => {
            promoted.record = id.clone();
            promoted.occurrence_of = Some(id);
        }
        Resolution::Conflict(ids) => {
            promoted.record = filled.into_inner();
            promoted.notes.push(format!(
                "{} disagrees with {}: supersede (`lore supersede`), keep both, or merge",
                promoted.record,
                ids.join(", ")
            ));
            promoted.conflicts_with = ids;
        }
        Resolution::New => promoted.record = filled.into_inner(),
    }
    Ok(promoted)
}

/// The refusals a draft meets before any write: the schema's (with the
/// stamps a write adds) and the secret scan's.
pub fn check_draft(config: &Config, draft: &Value) -> Vec<String> {
    let mut stamped = draft.clone();
    if let Some(map) = stamped.as_object_mut() {
        map.entry("id").or_insert(json!("draft"));
        map.insert("v".into(), json!(crate::FORMAT_VERSION));
        map.insert("by".into(), json!("draft"));
        map.insert("created".into(), json!(today()));
        map.insert("updated".into(), json!(today()));
    }
    let mut errs = schema::validate(&stamped, &config.areas, Some(&config.limits));
    errs.extend(crate::secrets::check_record(draft, &config.secrets_allow).iter().map(|found| found.line()));
    errs
}

/// Promotes a working entry (11.8). Promoting it again answers the record it
/// became and writes nothing.
pub fn promote(config: &Config, entry: &Entry, draft: Value, options: &Options) -> Result<Promoted> {
    if !entry.promoted.is_empty() {
        return Ok(Promoted { record: entry.promoted.clone(), existing: true, notes: vec![format!("{} was promoted before; nothing was written", entry.id)], ..Promoted::default() });
    }
    let errs = check_draft(config, &draft);
    if !errs.is_empty() {
        return Err(LoreError::with("refused, nothing was written", errs));
    }
    let mut options = options.clone();
    options.trailers.push(format!("Zerus-Promoted: {}", entry.id));
    write_resolved(config, draft, &options, &format!("promoted {}", entry.id))
}

/// `lore supersede OLD NEW` (11.10): in one commit the old record becomes
/// `superseded` with `superseded_by`. Only on request, never automatically.
pub fn supersede(config: &Config, old: &str, new: &str, options: &Options) -> Result<ops::Outcome> {
    let (old, new) = (old.to_owned(), new.to_owned());
    let build = move |snapshot: &crate::store::Snapshot| -> Result<Vec<Change>> {
        let by_id = snapshot.by_id();
        let current = by_id.get(&old).ok_or_else(|| LoreError::new(format!("{old} does not exist")))?;
        let replacement = by_id.get(&new).ok_or_else(|| LoreError::new(format!("{new} does not exist")))?;
        if current.get("kind") != replacement.get("kind") {
            return Err(LoreError::new(format!("{old} and {new} are different kinds")));
        }
        let mut record = current.clone();
        record["status"] = json!("superseded");
        record["superseded_by"] = json!(new);
        Ok(vec![Change::update(record, None)])
    };
    ops::apply(config, &build, options)
}
