//! The ledger (docs/FORMAT.md, "ledger, version 4"): a journal of structured
//! project data - labels, chosen values, settings per object - kept beside the
//! lore and never mixed with it. Each writer appends to files of its own
//! (`<dir>/<author>@<machine>/<YYYY-MM>.jsonl`), so two writers never touch one
//! file and nothing ever needs merging. The state is a fold over the graph the
//! lines' `parents` make, never over their clocks. Ledger commands write files
//! and nothing else: versioning them is the business of whoever keeps them.
use crate::config::{FieldType, Ledger};
use crate::store::canonical;
use crate::{LoreError, Result};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use std::path::{Path, PathBuf};

pub mod cache;
pub mod cli;
pub mod fold;

/// The version every ledger line carries.
pub const VERSION: u64 = 4;
/// Hex digits of a line's id: the first 64 bits of its sha256.
pub const ID_LEN: usize = 16;
/// The longest `note`, in characters.
pub const NOTE_MAX: usize = 1000;
/// The prefix a knowledge record's `refs.records` names a ledger entry with.
pub const REF_PREFIX: &str = "ledger:";
const FIELDS: [&str; 9] = ["at", "by", "id", "key", "kind", "note", "parents", "set", "v"];

/// One line of a journal.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub id: String,
    pub kind: String,
    pub key: String,
    /// Field -> value; `null` unsets the field.
    pub set: Map<String, Value>,
    /// The ids of the entry's heads the writer saw, sorted; empty for the first.
    pub parents: Vec<String>,
    pub by: String,
    /// `YYYY-MM-DDTHH:MM:SSZ`; shown to people, never ordering anything.
    pub at: String,
    pub note: Option<String>,
}

impl Line {
    /// A new line, its parents sorted and its id computed.
    pub fn new(kind: &str, key: &str, set: Map<String, Value>, parents: Vec<String>, by: &str, at: &str, note: Option<String>) -> Self {
        let mut parents = parents;
        parents.sort();
        parents.dedup();
        let mut line = Self { id: String::new(), kind: kind.into(), key: key.into(), set, parents, by: by.into(), at: at.into(), note };
        line.id = id_of(&line.value_without_id());
        line
    }

    fn value_without_id(&self) -> Value {
        let mut map = Map::new();
        map.insert("v".into(), Value::from(VERSION));
        map.insert("kind".into(), Value::String(self.kind.clone()));
        map.insert("key".into(), Value::String(self.key.clone()));
        map.insert("set".into(), Value::Object(self.set.clone()));
        map.insert("parents".into(), Value::Array(self.parents.iter().cloned().map(Value::String).collect()));
        map.insert("by".into(), Value::String(self.by.clone()));
        map.insert("at".into(), Value::String(self.at.clone()));
        if let Some(note) = &self.note {
            map.insert("note".into(), Value::String(note.clone()));
        }
        Value::Object(map)
    }

    pub fn value(&self) -> Value {
        let mut value = self.value_without_id();
        if let Value::Object(map) = &mut value {
            map.insert("id".into(), Value::String(self.id.clone()));
        }
        value
    }

    /// The line as written: canonical JSON, no newline.
    pub fn text(&self) -> String {
        canonical(&self.value())
    }

    /// A line that was checked when it was first read (the cache's copy),
    /// taken back without hashing or checking it again.
    pub fn from_checked(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let text_of = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            id: text_of("id")?,
            kind: text_of("kind")?,
            key: text_of("key")?,
            set: value.get("set")?.as_object()?.clone(),
            parents: value.get("parents")?.as_array()?.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect(),
            by: text_of("by")?,
            at: text_of("at")?,
            note: text_of("note"),
        })
    }

    /// The entry the line belongs to.
    pub fn entry(&self) -> (String, String) {
        (self.kind.clone(), self.key.clone())
    }
}

/// The id of a line: sha256 of its canonical text without `id`, the first 16 hex digits.
pub fn id_of(without_id: &Value) -> String {
    let digest = Sha256::digest(canonical(without_id).as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()[..ID_LEN].to_owned()
}

pub fn is_id(text: &str) -> bool {
    text.len() == ID_LEN && text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// An author or machine name: `^[a-z0-9][a-z0-9._-]{0,63}$` - safe as a folder
/// name everywhere, and the same on file systems that ignore case.
pub fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && text.len() <= 64
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'))
}

fn is_at(text: &str) -> bool {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%SZ").is_ok() && text.len() == 20
}

/// `at` for a moment (UTC seconds).
pub fn at_of(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The refusal for an author that cannot name a folder.
pub fn name_rule(what: &str, name: &str) -> String {
    format!("{what} {} must match ^[a-z0-9][a-z0-9._-]{{0,63}}$ (it names the journal's folder)", crate::py::repr_str(name))
}

/// What is wrong with `set` for `kind` under `ledger`'s declarations.
pub fn set_errors(ledger: &Ledger, kind: &str, set: &Map<String, Value>) -> Vec<String> {
    let Some(fields) = ledger.kinds.get(kind) else {
        return vec![format!("kind {} is not declared in [ledger.kinds]", crate::py::repr_str(kind))];
    };
    let mut errs = Vec::new();
    if set.is_empty() {
        errs.push("`set` names no field".into());
    }
    for (field, value) in set {
        match fields.get(field) {
            None => errs.push(format!("kind {kind} has no field {}", crate::py::repr_str(field))),
            Some(want) if !want.admits(value) => errs.push(format!("{kind}.{field} must be {}, not {}", want.name(), canonical(value))),
            Some(_) => {}
        }
    }
    errs
}

/// A line read from text, every way it breaks the format said.
pub fn parse_line(text: &str, ledger: &Ledger) -> std::result::Result<Line, Vec<String>> {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => return Err(vec![format!("not JSON ({error})")]),
    };
    let Some(map) = value.as_object() else { return Err(vec!["not a JSON object".into()]) };
    let mut errs = Vec::new();
    for key in map.keys() {
        if !FIELDS.contains(&key.as_str()) {
            errs.push(format!("unknown field {}", crate::py::repr_str(key)));
        }
    }
    if map.get("v").and_then(Value::as_u64) != Some(VERSION) {
        errs.push(format!("`v` must be {VERSION}"));
    }
    let string = |name: &str, errs: &mut Vec<String>| -> String {
        match map.get(name).and_then(Value::as_str) {
            Some(text) => text.to_owned(),
            None => {
                errs.push(format!("`{name}` must be a string"));
                String::new()
            }
        }
    };
    let id = string("id", &mut errs);
    let kind = string("kind", &mut errs);
    let key = string("key", &mut errs);
    let by = string("by", &mut errs);
    let at = string("at", &mut errs);
    if key.is_empty() && map.get("key").is_some_and(Value::is_string) {
        errs.push("`key` is empty".into());
    }
    if !by.is_empty() && !is_name(&by) {
        errs.push(name_rule("`by`", &by));
    }
    if !at.is_empty() && !is_at(&at) {
        errs.push("`at` must be YYYY-MM-DDTHH:MM:SSZ".into());
    }
    let note = match map.get("note") {
        None => None,
        Some(Value::String(note)) if !note.is_empty() && note.chars().count() <= NOTE_MAX => Some(note.clone()),
        Some(_) => {
            errs.push(format!("`note` must be a string of 1-{NOTE_MAX} characters"));
            None
        }
    };
    let parents: Vec<String> = match map.get("parents").and_then(Value::as_array) {
        Some(items) if items.iter().all(|item| item.as_str().is_some_and(is_id)) => items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect(),
        _ => {
            errs.push(format!("`parents` must be a list of line ids ({ID_LEN} hex digits)"));
            Vec::new()
        }
    };
    if parents.windows(2).any(|pair| pair[0] >= pair[1]) {
        errs.push("`parents` must be sorted, each id once".into());
    }
    let set = match map.get("set").and_then(Value::as_object) {
        Some(set) => set.clone(),
        None => {
            errs.push("`set` must be an object".into());
            Map::new()
        }
    };
    if !kind.is_empty() && map.get("set").is_some_and(Value::is_object) {
        errs.extend(set_errors(ledger, &kind, &set));
    }
    if canonical(&value) != text {
        errs.push("not canonical (keys sorted, no spaces, non-ASCII as is)".into());
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    let line = Line { id: id.clone(), kind, key, set, parents, by, at, note };
    if id_of(&line.value_without_id()) != id {
        return Err(vec![format!("`id` {id} is not the hash of the line")]);
    }
    Ok(line)
}

/// The lines of one file's bytes, numbered from `first` (1 for a whole file),
/// or a refusal naming the file and each bad line.
pub fn parse_bytes(bytes: &[u8], first: usize, place: &str, ledger: &Ledger) -> Result<Vec<Line>> {
    parse_bytes_raw(bytes, first, place, ledger).map(|lines| lines.into_iter().map(|(line, _)| line).collect())
}

/// [`parse_bytes`] with each line's own text: a line that was accepted is
/// canonical, so a caller that stores it need not write it again.
pub fn parse_bytes_raw(bytes: &[u8], first: usize, place: &str, ledger: &Ledger) -> Result<Vec<(Line, String)>> {
    if first == 1 && bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(LoreError::new(format!("{place}: starts with a BOM; ledger files are UTF-8 without one")));
    }
    if bytes.contains(&b'\r') {
        return Err(LoreError::new(format!("{place}: has CR line ends; ledger files are LF")));
    }
    let text = std::str::from_utf8(bytes).map_err(|error| LoreError::new(format!("{place}: not UTF-8 ({error})")))?;
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut errs = Vec::new();
    if !text.ends_with('\n') {
        errs.push("no final newline".to_owned());
    }
    let mut lines = Vec::new();
    for (index, raw) in text.strip_suffix('\n').unwrap_or(text).split('\n').enumerate() {
        let number = first + index;
        if raw.trim().is_empty() {
            errs.push(format!("line {number}: blank"));
            continue;
        }
        match parse_line(raw, ledger) {
            Ok(line) => lines.push((line, raw.to_owned())),
            Err(found) => errs.extend(found.into_iter().map(|error| format!("line {number}: {error}"))),
        }
    }
    if errs.is_empty() { Ok(lines) } else { Err(LoreError::with(format!("{place} is refused"), errs)) }
}

/// Many files' lines - (place, bytes, first line number) each - parsed on as
/// many threads as there are cores: checking lines is most of reading them.
pub fn parse_many(jobs: &[(String, &[u8], usize)], ledger: &Ledger) -> Vec<Result<Vec<(Line, String)>>> {
    let one = |(place, bytes, first): &(String, &[u8], usize)| parse_bytes_raw(bytes, *first, place, ledger);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(jobs.len().max(1));
    if threads <= 1 || jobs.iter().map(|job| job.1.len()).sum::<usize>() < 1 << 20 {
        return jobs.iter().map(one).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<Result<Vec<(Line, String)>>>>> = jobs.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if at >= jobs.len() {
                    break;
                }
                let result = one(&jobs[at]);
                *slots[at].lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
            });
        }
    });
    slots.into_iter().map(|slot| slot.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner()).unwrap_or_else(|| Ok(Vec::new()))).collect()
}

/// A journal's place: `<author>@<machine>/<YYYY-MM>.jsonl` under the ledger's
/// folder. None when the relative path is not one.
pub fn journal_author(relative: &str) -> Option<(String, String)> {
    let (folder, file) = relative.split_once('/')?;
    let (author, machine) = folder.split_once('@')?;
    let month = file.strip_suffix(".jsonl")?;
    let good_month = month.len() == 7 && chrono::NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d").is_ok();
    (is_name(author) && is_name(machine) && good_month && !file.contains('/')).then(|| (author.to_owned(), machine.to_owned()))
}

/// Every file under the ledger's folder, two levels down, sorted; the paths
/// are relative to that folder. Files that are not journals are listed too, so
/// `check` can name them.
pub fn files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(folders) = std::fs::read_dir(dir) else { return found };
    for folder in folders.flatten() {
        let path = folder.path();
        let name = folder.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                for entry in entries.flatten() {
                    if entry.path().is_file() {
                        found.push(format!("{name}/{}", entry.file_name().to_string_lossy()));
                    }
                }
            }
        } else if path.is_file() {
            found.push(name);
        }
    }
    found.sort();
    found
}

/// The ledger folder of a configuration.
pub fn folder(root: &Path, ledger: &Ledger) -> PathBuf {
    root.join(&ledger.dir)
}

/// Who writes: `--by`, else `LORE_BY`, else a refusal - never a name taken from
/// anything that keeps the files.
pub fn author(by: Option<&str>) -> Result<String> {
    let Some(by) = by.map(str::trim).filter(|by| !by.is_empty()) else {
        return Err(LoreError::new("the ledger needs its author: pass --by <name> or set LORE_BY"));
    };
    if is_name(by) { Ok(by.to_owned()) } else { Err(LoreError::new(name_rule("author", by))) }
}

/// The machine: `LORE_MACHINE`, else the host name in lower case.
pub fn machine(given: Option<&str>) -> Result<String> {
    let name = match given.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => name.to_owned(),
        None => host_name().to_lowercase(),
    };
    if is_name(&name) { Ok(name) } else { Err(LoreError::new(format!("{}; set LORE_MACHINE", name_rule("machine", &name)))) }
}

fn host_name() -> String {
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Some(name) = std::env::var(var).ok().map(|name| name.trim().to_owned()).filter(|name| !name.is_empty()) {
            return name;
        }
    }
    if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
        let name = name.trim().to_owned();
        if !name.is_empty() {
            return name;
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// The journal file a line written now goes to, relative to the ledger folder.
pub fn journal_of(author: &str, machine: &str, at: &str) -> String {
    format!("{author}@{machine}/{}.jsonl", &at[..7])
}

/// Appends `lines` to the file at `path` under an exclusive lock, refusing a
/// file whose last line is unfinished. One write, so a reader sees all or none
/// of a whole line.
pub fn append(path: &Path, lines: &[Line]) -> Result<()> {
    use fs2::FileExt;
    use std::io::{Read, Seek, SeekFrom, Write};
    if lines.is_empty() {
        return Ok(());
    }
    let shown = path.display().to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| LoreError::new(format!("{}: {error}", parent.display())))?;
    }
    let mut file = std::fs::OpenOptions::new().read(true).append(true).create(true).open(path).map_err(|error| LoreError::new(format!("{shown}: {error}")))?;
    file.lock_exclusive().map_err(|error| LoreError::new(format!("{shown}: cannot lock ({error})")))?;
    let result = (|| {
        let length = file.metadata().map_err(|error| LoreError::new(format!("{shown}: {error}")))?.len();
        if length > 0 {
            file.seek(SeekFrom::Start(length - 1)).map_err(|error| LoreError::new(format!("{shown}: {error}")))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last).map_err(|error| LoreError::new(format!("{shown}: {error}")))?;
            if last[0] != b'\n' {
                return Err(LoreError::new(format!("{shown}: its last line is unfinished; nothing was written - `lore ledger check` names it")));
            }
        }
        let mut text = String::new();
        for line in lines {
            text.push_str(&line.text());
            text.push('\n');
        }
        file.write_all(text.as_bytes()).map_err(|error| LoreError::new(format!("{shown}: {error}")))?;
        file.flush().map_err(|error| LoreError::new(format!("{shown}: {error}")))
    })();
    let _ = fs2::FileExt::unlock(&file);
    result
}

/// A knowledge record's reference to a ledger entry: `ledger:<kind>:<key>`.
pub fn parse_ref(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix(REF_PREFIX)?;
    let (kind, key) = rest.split_once(':')?;
    (!kind.is_empty() && !key.is_empty()).then_some((kind, key))
}

/// What `lore check` says about a store's `ledger:` references: unknown kinds
/// are errors, entries not (yet) in the ledger warnings - another writer's
/// journal may arrive later.
pub fn ref_findings(config: &crate::config::Config, references: &[(String, String)], look: bool) -> (Vec<String>, Vec<String>) {
    let (mut errs, mut warnings) = (Vec::new(), Vec::new());
    if references.is_empty() {
        return (errs, warnings);
    }
    let Some(ledger) = &config.ledger else {
        for (rid, target) in references {
            errs.push(format!("{rid}: refs.records names {}, but lore.toml has no [ledger]", crate::py::repr_str(target)));
        }
        return (errs, warnings);
    };
    let mut known: Option<std::collections::BTreeSet<(String, String)>> = None;
    for (rid, target) in references {
        let Some((kind, key)) = parse_ref(target) else {
            errs.push(format!("{rid}: refs.records names {} - a ledger entry is ledger:<kind>:<key>", crate::py::repr_str(target)));
            continue;
        };
        if !ledger.kinds.contains_key(kind) {
            errs.push(format!("{rid}: refs.records names {}, but kind {kind} is not declared in [ledger.kinds]", crate::py::repr_str(target)));
            continue;
        }
        if !look {
            continue;
        }
        if known.is_none() {
            known = Some(match cache::load(config, ledger, None) {
                Ok(state) => state.entries.into_keys().collect(),
                Err(error) => {
                    warnings.push(format!("the ledger could not be read, so its entries were not looked up: {}", error.message));
                    return (errs, warnings);
                }
            });
        }
        if known.as_ref().is_some_and(|known| !known.contains(&(kind.to_owned(), key.to_owned()))) {
            warnings.push(format!("{rid}: refs.records names {}, which is not in the ledger (yet)", crate::py::repr_str(target)));
        }
    }
    (errs, warnings)
}

/// `(record id, target)` for each `ledger:` reference of `records`.
pub fn refs_of<'a>(records: impl IntoIterator<Item = &'a Value>) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for record in records {
        let rid = record.get("id").and_then(Value::as_str).unwrap_or_default();
        for target in record.get("refs").and_then(|refs| refs.get("records")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if target.starts_with(REF_PREFIX) {
                found.push((rid.to_owned(), target.to_owned()));
            }
        }
    }
    found
}

/// The type of each of a kind's fields, for parsing `field=value`.
pub fn field_type<'a>(ledger: &'a Ledger, kind: &str, field: &str) -> Option<&'a FieldType> {
    ledger.kinds.get(kind)?.get(field)
}
