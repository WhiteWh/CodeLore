//! The JSONL files (CodeLore's `store.py`): canonical lines, sorted, LF. A file
//! that is not exactly that is refused by name and line; the store never
//! repairs what it reads.
use crate::py::repr_opt;
use crate::schema::{item_id, item_number, PLAN_KINDS};
use crate::{LoreError, Result};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One record as one canonical line: keys sorted at every depth, no
/// insignificant whitespace, non-ASCII as is - Python's
/// `json.dumps(r, sort_keys=True, separators=(",", ":"), ensure_ascii=False)`.
pub fn canonical(record: &Value) -> String {
    serde_json::to_string(&sorted(record)).unwrap_or_else(|_| "{}".into())
}

/// The value with every object's keys sorted (records keep the order they
/// were given in memory; only their written form is sorted).
pub fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(keys.into_iter().map(|key| (key.clone(), sorted(&map[key]))).collect::<Map<String, Value>>())
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Knowledge sorts by id. In a plan file the plan record comes first and its
/// items follow by number, so #10 comes after #9.
pub fn sort_key(record: &Value) -> (u8, u64, String) {
    let id = record.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    match record.get("kind").and_then(Value::as_str) {
        Some("item") => (1, item_number(&id), id),
        _ => (0, 0, id),
    }
}

/// Canonical file bytes for `records`, in the format's order.
pub fn serialize(records: &[Value]) -> Vec<u8> {
    if records.is_empty() {
        return Vec::new();
    }
    let mut order: Vec<&Value> = records.iter().collect();
    order.sort_by_key(|record| sort_key(record));
    let lines: Vec<String> = order.into_iter().map(canonical).collect();
    format!("{}\n", lines.join("\n")).into_bytes()
}

/// Records of one file, or a refusal naming every way it is not canonical.
pub fn parse(bytes: &[u8], place: &str) -> Result<Vec<Value>> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(LoreError::new(format!("{place}: starts with a BOM; lore files are UTF-8 without one")));
    }
    if bytes.contains(&b'\r') {
        return Err(LoreError::new(format!(
            "{place}: has CR line ends; lore files are LF - add `<dir>/** text eol=lf` to .gitattributes and check the file out again"
        )));
    }
    let text = std::str::from_utf8(bytes).map_err(|error| LoreError::new(format!("{place}: not UTF-8 ({error})")))?;
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut errs = Vec::new();
    if !text.ends_with('\n') {
        errs.push("no final newline".to_owned());
    }
    let lines: Vec<&str> = if let Some(body) = text.strip_suffix('\n') { body.split('\n').collect() } else { text.split('\n').collect() };
    let mut records = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        if line.trim().is_empty() {
            errs.push(format!("line {number}: blank"));
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                errs.push(format!("line {number}: not JSON ({})", json_message(&error)));
                continue;
            }
        };
        if !value.is_object() {
            errs.push(format!("line {number}: not a JSON object"));
            continue;
        }
        if canonical(&value) != *line {
            errs.push(format!(
                "line {number}: not canonical (keys sorted, no spaces, non-ASCII as is) - the file was edited by hand; `lore fmt` rewrites it"
            ));
        }
        records.push(value);
    }
    let keys: Vec<(u8, u64, String)> = records.iter().map(sort_key).collect();
    for index in 1..keys.len() {
        if keys[index] < keys[index - 1] {
            errs.push(format!("line {}: out of order ({} after {})", index + 1, repr_opt(records[index].get("id")), repr_opt(records[index - 1].get("id"))));
        } else if keys[index] == keys[index - 1] {
            errs.push(format!("line {}: id {} twice", index + 1, repr_opt(records[index].get("id"))));
        }
    }
    if errs.is_empty() { Ok(records) } else { Err(LoreError::with(format!("{place} is refused"), errs)) }
}

/// serde_json's reason without its position (Python's `e.msg` has none).
fn json_message(error: &serde_json::Error) -> String {
    let text = error.to_string();
    match text.rfind(" at line ") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

/// The repository-relative path the record lives in; None when its kind and
/// area cannot name one (Python's KeyError).
pub fn path_of(record: &Value, lore_dir: &str) -> Option<String> {
    let id = record.get("id").and_then(Value::as_str);
    match record.get("kind").and_then(Value::as_str) {
        Some("plan") => Some(format!("{lore_dir}/plans/{}.jsonl", id?)),
        Some("item") => {
            let plan = match id.and_then(item_id) {
                Some((plan, _)) => plan.to_owned(),
                None => record.get("plan").and_then(Value::as_str).unwrap_or_default().to_owned(),
            };
            Some(format!("{lore_dir}/plans/{plan}.jsonl"))
        }
        _ => {
            let area = record.get("area")?;
            let area = area.as_str().map(str::to_owned).unwrap_or_else(|| area.to_string());
            Some(format!("{lore_dir}/{area}.jsonl"))
        }
    }
}

/// Every lore file at one point - the working tree or a commit - by
/// repository-relative path.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub files: BTreeMap<String, Vec<Value>>,
}

impl Snapshot {
    pub fn records(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.files.iter().flat_map(|(path, records)| records.iter().map(move |record| (path, record)))
    }

    pub fn all(&self) -> Vec<Value> {
        self.records().map(|(_, record)| record.clone()).collect()
    }

    /// Records by id; a later file wins, as Python's dict does.
    pub fn by_id(&self) -> BTreeMap<String, Value> {
        self.records()
            .filter_map(|(_, record)| record.get("id").and_then(Value::as_str).map(|id| (id.to_owned(), record.clone())))
            .collect()
    }
}

pub fn is_lore_file(relative: &str, lore_dir: &str) -> bool {
    if !relative.ends_with(".jsonl") {
        return false;
    }
    let Some(rest) = relative.strip_prefix(&format!("{lore_dir}/")) else { return false };
    !rest.contains('/') || (rest.starts_with("plans/") && rest.matches('/').count() == 1)
}

/// The lore files of a folder, sorted by path: `<dir>/*.jsonl` and
/// `<dir>/plans/*.jsonl`.
pub fn lore_files(root: &Path, lore_dir: &str) -> Vec<PathBuf> {
    let base = root.join(lore_dir);
    let mut paths = Vec::new();
    for folder in [base.clone(), base.join("plans")] {
        if let Ok(entries) = std::fs::read_dir(&folder) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    paths
}

pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

pub fn load_worktree(root: &Path, lore_dir: &str) -> Result<Snapshot> {
    let mut files = BTreeMap::new();
    for path in lore_files(root, lore_dir) {
        let rel = relative(root, &path);
        let bytes = std::fs::read(&path).map_err(|error| LoreError::new(format!("{rel}: {error}")))?;
        let records = parse(&bytes, &rel)?;
        files.insert(rel, records);
    }
    Ok(Snapshot { files })
}

/// Each record is in the file its kind and area name; ids are unique.
pub fn check_placement(snapshot: &Snapshot, lore_dir: &str) -> Vec<String> {
    let mut errs = Vec::new();
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for (rel, record) in snapshot.records() {
        let id = record.get("id");
        if let Some(text) = id.and_then(Value::as_str) {
            if let Some(first) = seen.get(text) {
                errs.push(format!("id {} is in {first} and in {rel}", repr_opt(id)));
            }
            seen.insert(text.to_owned(), rel.clone());
        }
        let kind = record.get("kind").and_then(Value::as_str).unwrap_or_default();
        if PLAN_KINDS.contains(&kind) || record.get("area").is_some_and(Value::is_string) {
            let Some(want) = path_of(record, lore_dir) else { continue };
            if want != *rel {
                errs.push(format!("{} is in {rel}, its kind and area put it in {want}", repr_opt(id)));
            }
        }
    }
    for (rel, records) in &snapshot.files {
        if rel.starts_with(&format!("{lore_dir}/plans/")) {
            let plans = records.iter().filter(|record| record.get("kind").and_then(Value::as_str) == Some("plan")).count();
            if plans != 1 {
                errs.push(format!("{rel} must hold exactly one plan record, holds {plans}"));
            }
        }
    }
    errs
}

/// One store on disk, for the foundation's simple reads and writes.
#[derive(Clone, Debug)]
pub struct Store {
    pub root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn read(&self, relative: &str) -> Result<Vec<Value>> {
        let bytes = std::fs::read(self.root.join(relative)).map_err(|error| LoreError::new(format!("{relative}: {error}")))?;
        parse(&bytes, relative)
    }

    /// Writes one file with the canonical bytes, replacing it whole.
    pub fn write(&self, relative: &str, records: &[Value]) -> Result<()> {
        if crate::gitio::write_live(&self.root, relative, &serialize(records)) {
            Ok(())
        } else {
            Err(LoreError::new(format!(
                "{relative}: another process holds the file, so it could not be replaced; nothing was committed - try again when it is free"
            )))
        }
    }

    /// Adds a record to its file, refusing a duplicate id.
    pub fn add(&self, relative: &str, record: Value) -> Result<()> {
        let mut records = self.read(relative).unwrap_or_default();
        let id = record.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        if records.iter().any(|existing| existing.get("id").and_then(Value::as_str) == Some(id.as_str())) {
            return Err(LoreError::new(format!("'{id}' already exists - update it, or give the new record another id")));
        }
        records.push(record);
        self.write(relative, &records)
    }
}

/// A path inside the store, never escaping it.
pub fn safe_relative(path: &Path) -> bool {
    !path.is_absolute() && path.components().all(|part| matches!(part, std::path::Component::Normal(_)))
}
