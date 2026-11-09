//! The ledger (docs/FORMAT.md, "ledger, version 4"): a journal of structured
//! project data - labels, chosen values, settings per object - kept beside the
//! lore and never mixed with it. Each writer appends to files of its own
//! (`<dir>/<author>@<machine>/<YYYY-MM>.jsonl`), so two writers never touch one
//! file and nothing ever needs merging. The state is a fold over the graph the
//! lines' `parents` make, never over their clocks. Ledger commands write files
//! and nothing else: versioning them is the business of whoever keeps them.
use crate::config::{FieldType, Ledger};
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

/// A ledger line's canonical text: keys sorted at every depth, no
/// insignificant whitespace, non-ASCII as is; an integer exactly, any other
/// number in the shortest form that reads back as the same double (`0.1`,
/// `0.00001`, `1e-6`, `1e+20`, `-0.0`). Read back, the text gives the same value and so
/// the same bytes and id.
pub fn canonical(value: &Value) -> String {
    crate::store::canonical(value)
}

/// [`set_errors`] for a line about to be written: no retired kind or field either.
pub fn write_errors(ledger: &Ledger, kind: &str, set: &Map<String, Value>) -> Vec<String> {
    let mut errs = set_errors(ledger, kind, set);
    if ledger.retired_kinds.contains(kind) {
        errs.push(format!("kind {kind} is retired: its lines are kept and read, no new one is written"));
    } else {
        for field in set.keys().filter(|field| ledger.retired_fields.contains(&(kind.to_owned(), (*field).clone()))) {
            errs.push(format!("{kind}.{field} is retired: its lines are kept and read, no new value is written"));
        }
    }
    errs
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
/// `check` can name them. A folder that does not exist yet holds no journal; a
/// folder that cannot be listed is a refusal, never an empty ledger.
pub fn files(dir: &Path) -> Result<Vec<String>> {
    let listing = |path: &Path| -> Result<Vec<std::fs::DirEntry>> {
        let refused = |error: std::io::Error| LoreError::new(format!("{}: cannot be listed ({error}); nothing was read", path.display()));
        std::fs::read_dir(path).map_err(refused)?.collect::<std::io::Result<Vec<_>>>().map_err(refused)
    };
    match std::fs::metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(LoreError::new(format!("{}: {error}; nothing was read", dir.display()))),
        Ok(_) => {}
    }
    let mut found = Vec::new();
    for folder in listing(dir)? {
        let path = folder.path();
        let name = folder.file_name().to_string_lossy().into_owned();
        let kind = folder.file_type().map_err(|error| LoreError::new(format!("{}: {error}", path.display())))?;
        if kind.is_dir() {
            for entry in listing(&path)? {
                if entry.file_type().map_err(|error| LoreError::new(format!("{}: {error}", entry.path().display())))?.is_file() {
                    found.push(format!("{name}/{}", entry.file_name().to_string_lossy()));
                }
            }
        } else if kind.is_file() {
            found.push(name);
        }
    }
    found.sort();
    Ok(found)
}

/// The part of a file that is whole lines, and the length of what follows its
/// last LF: an unfinished line a writer is appending, or one an interrupted
/// write left. Readers take whole lines only.
pub fn whole_lines(bytes: &[u8]) -> (&[u8], usize) {
    match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(at) => (&bytes[..=at], bytes.len() - at - 1),
        None => (&bytes[..0], bytes.len()),
    }
}

/// The whole lines of a file's bytes, and how many bytes of an unfinished
/// last line follow them.
pub fn parse_whole(bytes: &[u8], place: &str, ledger: &Ledger) -> Result<(Vec<Line>, usize)> {
    let (whole, tail) = whole_lines(bytes);
    if whole.is_empty() && bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(LoreError::new(format!("{place}: starts with a BOM; ledger files are UTF-8 without one")));
    }
    Ok((parse_bytes(whole, 1, place, ledger)?, tail))
}

/// A fold that refuses what only a collision of ids can make: two different
/// lines under one id, or a cycle of parents.
pub fn fold_checked(kind: &str, key: &str, lines: &[Line]) -> Result<fold::Entry> {
    let entry = fold::fold(lines);
    if !entry.collisions.is_empty() {
        return Err(LoreError::new(format!(
            "{kind} {}: lines {} each stand for two different lines - an id collision, or a copy that was changed; nothing was read",
            crate::py::repr_str(key),
            entry.collisions.join(", ")
        )));
    }
    if !entry.cycle.is_empty() {
        return Err(LoreError::new(format!("{kind} {}: lines {} are on a cycle of parents; nothing was read", crate::py::repr_str(key), entry.cycle.join(", "))));
    }
    Ok(entry)
}

/// Runs `work` holding `<path>.lock` exclusively: the protocol every writer of
/// a pending file follows, so a promote never drops a line written meanwhile.
pub fn locked<T>(path: &Path, work: impl FnOnce() -> Result<T>) -> Result<T> {
    use fs2::FileExt;
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let lock_path = PathBuf::from(name);
    if let Some(parent) = lock_path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| LoreError::new(format!("{}: {error}", parent.display())))?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| LoreError::new(format!("{}: {error}", lock_path.display())))?;
    lock.lock_exclusive().map_err(|error| LoreError::new(format!("{}: cannot lock ({error})", lock_path.display())))?;
    let result = work();
    let _ = fs2::FileExt::unlock(&lock);
    result
}

/// Replaces the file at `path` with `bytes` so that a crash leaves the old
/// file or the new one, never a part of either: a sibling is written and
/// synced, then renamed over it.
pub fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    let temporary = PathBuf::from(name);
    let shown = temporary.display().to_string();
    let mut file = std::fs::File::create(&temporary).map_err(|error| LoreError::new(format!("{shown}: {error}")))?;
    file.write_all(bytes).map_err(|error| LoreError::new(format!("{shown}: {error}")))?;
    file.sync_all().map_err(|error| LoreError::new(format!("{shown}: {error}")))?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(|error| LoreError::new(format!("{}: could not be replaced ({error}); it is unchanged, the new content is in {shown}", path.display())))
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

/// The length of what follows the last LF of an open file, looked for
/// backwards in blocks.
fn tail_of(file: &mut std::fs::File, length: u64) -> std::io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut tail = 0u64;
    let mut block = vec![0u8; 4096];
    while tail < length {
        let start = length.saturating_sub(tail + block.len() as u64);
        let size = (length - tail - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut block[..size])?;
        match block[..size].iter().rposition(|byte| *byte == b'\n') {
            Some(at) => return Ok(tail + (size - at - 1) as u64),
            None => tail += size as u64,
        }
    }
    Ok(tail)
}

/// Sets aside the unfinished last line of the file at `path`: its bytes go to
/// `<path>.torn-<n>` (synced) before the file is cut back to its last LF. What
/// the bytes were - an interrupted write, a copy cut short, a damaged disk - is
/// not known, so nothing of them is dropped. None when the file ends in a whole
/// line.
pub fn repair(path: &Path) -> Result<Option<(u64, PathBuf)>> {
    use fs2::FileExt;
    use std::io::{Read, Seek, SeekFrom, Write};
    let shown = path.display().to_string();
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(|error| LoreError::new(format!("{shown}: {error}")))?;
    file.lock_exclusive().map_err(|error| LoreError::new(format!("{shown}: cannot lock ({error})")))?;
    let fail = |error: std::io::Error| LoreError::new(format!("{shown}: {error}; it was not changed"));
    let result = (|| {
        let length = file.metadata().map_err(fail)?.len();
        let tail = tail_of(&mut file, length).map_err(fail)?;
        if tail == 0 {
            return Ok(None);
        }
        let mut bytes = vec![0u8; tail as usize];
        file.seek(SeekFrom::Start(length - tail)).map_err(fail)?;
        file.read_exact(&mut bytes).map_err(fail)?;
        let mut number = 1;
        let kept = loop {
            let mut name = path.as_os_str().to_owned();
            name.push(format!(".torn-{number}"));
            let candidate = PathBuf::from(name);
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
                Ok(mut aside) => {
                    aside.write_all(&bytes).and_then(|()| aside.sync_all()).map_err(fail)?;
                    break candidate;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
                Err(error) => return Err(fail(error)),
            }
        };
        file.set_len(length - tail).map_err(fail)?;
        file.sync_all().map_err(fail)?;
        Ok(Some((tail, kept)))
    })();
    let _ = fs2::FileExt::unlock(&file);
    result
}

/// Appends `lines` to the file at `path` under an exclusive lock on it, and
/// syncs them to disk before it returns. Readers take whole lines only, so
/// they never see part of one; a batch is not atomic, and a crash may leave
/// some of its lines (a retry that must not write them twice goes through a
/// pending file, whose lines keep their ids). A file whose last line is
/// unfinished is refused: `lore ledger repair` sets those bytes aside.
pub fn append(path: &Path, lines: &[Line]) -> Result<()> {
    use fs2::FileExt;
    use std::io::{Seek, SeekFrom, Write};
    if lines.is_empty() {
        return Ok(());
    }
    let shown = path.display().to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| LoreError::new(format!("{}: {error}", parent.display())))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| LoreError::new(format!("{shown}: {error}")))?;
    file.lock_exclusive().map_err(|error| LoreError::new(format!("{shown}: cannot lock ({error})")))?;
    let fail = |error: std::io::Error| LoreError::new(format!("{shown}: {error}; the lines were not written"));
    let result = (|| {
        let length = file.metadata().map_err(fail)?.len();
        let tail = tail_of(&mut file, length).map_err(fail)?;
        if tail > 0 {
            return Err(LoreError::new(format!(
                "{shown} ends in an unfinished line ({tail} bytes); nothing was written - `lore ledger repair` sets those bytes aside, once you know what they are"
            )));
        }
        let mut text = String::new();
        for line in lines {
            text.push_str(&line.text());
            text.push('\n');
        }
        file.seek(SeekFrom::Start(length)).map_err(fail)?;
        file.write_all(text.as_bytes()).map_err(fail)?;
        file.sync_data().map_err(fail)
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
    let mut cache: Option<cache::Cache> = None;
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
        if cache.is_none() {
            match cache::Cache::open(config, ledger) {
                Ok(opened) => cache = Some(opened),
                Err(error) => {
                    warnings.push(format!("the ledger could not be read, so its entries were not looked up: {}", error.message));
                    return (errs, warnings);
                }
            }
        }
        if cache.as_ref().is_some_and(|cache| !cache.exists(kind, key).unwrap_or(false)) {
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
