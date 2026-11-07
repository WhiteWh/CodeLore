//! A source document's coverage (CodeLore's `cover.py`): which of its parts a
//! record holds, which are skipped and why, which nobody has accounted for.
//!
//! A Markdown document's parts are its headings, the rows of its tables and
//! its numbered list items, named the way they are written:
//!
//! ```text
//! 6. The faults met                      a heading, its text after the #s
//! 6. The faults met | E_FAIL on resize   a table row: its heading | its first cell
//! Steps / 3. Run the gate                a numbered item: its heading / n. its first words
//! Setup / text                           what a heading says itself, when it also has parts
//! ```
//!
//! A repeated name takes ` (2)`, ` (3)` in order. An entry for a heading
//! covers everything under it; a heading with no entry is covered when
//! everything under it is.
use crate::config::Config;
use crate::gitio;
use crate::py::repr_str;
use crate::schema::{bad_path, part_key};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

pub const TOP: &str = "(top)";
/// How much of a row's first cell, or an item's first words, names it.
const ROW_NAME: usize = 80;
const ITEM_NAME: usize = 60;

#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub name: String,
    /// heading | text | row | item
    pub kind: String,
    pub line: usize,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    /// A heading that says something itself.
    pub prose: bool,
}

fn heading() -> &'static regex::Regex {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"^ {0,3}(#{1,6})\s+(.*?)(?:\s+#+)?\s*$").expect("heading"))
}
fn fence() -> &'static regex::Regex {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"^ {0,3}(```|~~~)").expect("fence"))
}
fn item() -> &'static regex::Regex {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"^ {0,3}(\d{1,9})[.)]\s+(.*)$").expect("item"))
}
fn separator() -> &'static regex::Regex {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"^\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$").expect("separator"))
}

fn short(text: &str, limit: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    let head: String = text.chars().take(limit).collect();
    let cut = head.rsplit_once(' ').map(|(before, _)| before.to_owned()).unwrap_or_default();
    let cut = if cut.is_empty() { head } else { cut };
    cut.trim_end_matches([' ', ',', ';', ':']).to_owned()
}

/// A table row's cells, split on `|` not escaped by a backslash.
fn cells(line: &str) -> Vec<String> {
    let mut text = line.trim();
    if let Some(rest) = text.strip_prefix('|') {
        text = rest;
    }
    if text.ends_with('|') && !text.ends_with("\\|") {
        text = &text[..text.len() - 1];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut previous = None;
    for ch in text.chars() {
        if ch == '|' && previous != Some('\\') {
            out.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(ch);
        }
        previous = Some(ch);
    }
    out.push(current.trim().to_owned());
    out
}

/// Every part of a Markdown document, in order.
pub fn parts_of(text: &str) -> Vec<Part> {
    let normal = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normal.split('\n').collect();
    let mut parts: Vec<Part> = Vec::new();
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut top: Option<usize> = None;
    let mut order: Vec<usize> = Vec::new();
    let named = |base: &str, names: &mut BTreeMap<String, usize>| -> String {
        let count = names.entry(part_key(base)).or_insert(0);
        *count += 1;
        if *count == 1 { base.to_owned() } else { format!("{base} ({count})") }
    };
    fn owner(stack: &[(usize, usize)], top: &mut Option<usize>, parts: &mut Vec<Part>, order: &mut Vec<usize>, names: &mut BTreeMap<String, usize>, named: &dyn Fn(&str, &mut BTreeMap<String, usize>) -> String) -> usize {
        if let Some((_, part)) = stack.last() {
            return *part;
        }
        if let Some(top) = top {
            return *top;
        }
        let index = parts.len();
        parts.push(Part { name: named(TOP, names), kind: "heading".into(), line: 1, parent: None, children: Vec::new(), prose: false });
        order.insert(0, index);
        *top = Some(index);
        index
    }
    fn add(part: Part, parts: &mut Vec<Part>, order: &mut Vec<usize>) {
        let index = parts.len();
        if let Some(parent) = part.parent {
            parts[parent].children.push(index);
        }
        parts.push(part);
        order.push(index);
    }
    let mut fence_mark: Option<String> = None;
    let mut in_item = false;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let number = index + 1;
        if let Some(mark) = &fence_mark {
            if line.trim().starts_with(mark.as_str()) {
                fence_mark = None;
            }
            index += 1;
            continue;
        }
        if let Some(found) = fence().captures(line) {
            fence_mark = Some(found[1].to_owned());
            let at = owner(&stack, &mut top, &mut parts, &mut order, &mut names, &named);
            parts[at].prose = true;
            in_item = false;
            index += 1;
            continue;
        }
        if line.trim().is_empty() {
            index += 1;
            continue;
        }
        if let Some(found) = heading().captures(line) {
            let level = found[1].len();
            let title = found[2].trim().to_owned();
            while stack.last().is_some_and(|(at, _)| *at >= level) {
                stack.pop();
            }
            let base = if title.is_empty() { format!("(untitled, line {number})") } else { title };
            let part = Part { name: named(&base, &mut names), kind: "heading".into(), line: number, parent: stack.last().map(|(_, part)| *part), children: Vec::new(), prose: false };
            let at = parts.len();
            add(part, &mut parts, &mut order);
            stack.push((level, at));
            in_item = false;
            index += 1;
            continue;
        }
        if line.trim_start().starts_with('|') {
            let mut block: Vec<(usize, &str)> = Vec::new();
            while index < lines.len() && lines[index].trim_start().starts_with('|') {
                block.push((index + 1, lines[index]));
                index += 1;
            }
            if block.len() >= 2 && separator().is_match(block[1].1.trim()) {
                block.drain(..2);
            }
            let head = owner(&stack, &mut top, &mut parts, &mut order, &mut names, &named);
            for (row_line, row) in block {
                let cell = cells(row).into_iter().find(|cell| !cell.is_empty()).unwrap_or_default();
                let base = format!("{} | {}", parts[head].name, short(&cell, ROW_NAME));
                let part = Part { name: named(&base, &mut names), kind: "row".into(), line: row_line, parent: Some(head), children: Vec::new(), prose: false };
                add(part, &mut parts, &mut order);
            }
            in_item = false;
            continue;
        }
        if let Some(found) = item().captures(line) {
            let head = owner(&stack, &mut top, &mut parts, &mut order, &mut names, &named);
            let base = format!("{} / {}. {}", parts[head].name, &found[1], short(&found[2], ITEM_NAME));
            let part = Part { name: named(&base, &mut names), kind: "item".into(), line: number, parent: Some(head), children: Vec::new(), prose: false };
            add(part, &mut parts, &mut order);
            in_item = true;
            index += 1;
            continue;
        }
        if in_item && (line.starts_with(' ') || line.starts_with('\t')) {
            index += 1; // the item's own continuation
            continue;
        }
        in_item = false;
        let at = owner(&stack, &mut top, &mut parts, &mut order, &mut names, &named);
        parts[at].prose = true;
        index += 1;
    }
    // What a heading says itself is a part of its own when it has others
    // under it, so an entry can account for it without covering the section.
    let mut result: Vec<Part> = Vec::new();
    let mut new_index: BTreeMap<usize, usize> = BTreeMap::new();
    let mut texts: Vec<(usize, usize)> = Vec::new();
    for at in &order {
        new_index.insert(*at, result.len());
        result.push(parts[*at].clone());
        let part = &parts[*at];
        if part.kind == "heading" && part.prose && !part.children.is_empty() {
            let base = format!("{} / text", part.name);
            let name = named(&base, &mut names);
            texts.push((result.len(), *at));
            result.push(Part { name, kind: "text".into(), line: part.line, parent: Some(*at), children: Vec::new(), prose: false });
        }
    }
    for part in &mut result {
        part.parent = part.parent.map(|parent| new_index[&parent]);
        part.children = part.children.iter().map(|child| new_index[child]).collect();
    }
    for (text_at, owner_at) in texts {
        let owner = new_index[&owner_at];
        result[owner].children.insert(0, text_at);
    }
    result
}

#[derive(Clone, Debug, PartialEq)]
pub struct Coverage {
    pub parts: Vec<Part>,
    /// The parts an entry still has to name.
    pub uncovered: Vec<Part>,
    /// Ledger entries that name no part.
    pub unknown: Vec<String>,
}

pub fn coverage(parts: &[Part], ledger: &[Value]) -> Coverage {
    let entry_names: Vec<&str> = ledger.iter().filter_map(|entry| entry.get("part").and_then(Value::as_str)).collect();
    let named: BTreeSet<String> = entry_names.iter().map(|name| part_key(name)).collect();
    let known: BTreeSet<String> = parts.iter().map(|part| part_key(&part.name)).collect();
    let by_entry = |mut at: Option<usize>| -> bool {
        while let Some(index) = at {
            if named.contains(&part_key(&parts[index].name)) {
                return true;
            }
            at = parts[index].parent;
        }
        false
    };
    fn covered(index: usize, parts: &[Part], memo: &mut BTreeMap<usize, bool>, by_entry: &dyn Fn(Option<usize>) -> bool) -> bool {
        if let Some(known) = memo.get(&index) {
            return *known;
        }
        let part = &parts[index];
        let value = if by_entry(Some(index)) {
            true
        } else if part.kind == "heading" && !part.children.is_empty() {
            part.children.iter().all(|child| covered(*child, parts, memo, by_entry))
        } else {
            part.kind == "heading" && !part.prose
        };
        memo.insert(index, value);
        value
    }
    let mut memo = BTreeMap::new();
    let uncovered = (0..parts.len())
        .filter(|index| !covered(*index, parts, &mut memo, &by_entry) && !(parts[*index].kind == "heading" && !parts[*index].children.is_empty()))
        .map(|index| parts[index].clone())
        .collect();
    let unknown = entry_names.iter().filter(|name| !known.contains(&part_key(name))).map(|name| (*name).to_owned()).collect();
    Coverage { parts: parts.to_vec(), uncovered, unknown }
}

/// The document as the working tree has it, else as the base commit does.
pub fn read_doc(config: &Config, path: &str, rev: Option<&str>) -> Option<Vec<u8>> {
    let file = config.root.join(path);
    if file.is_file() {
        return std::fs::read(file).ok();
    }
    let rev = rev.map(str::to_owned).or_else(|| gitio::head(&config.root));
    gitio::blob(&config.root, rev.as_deref(), path)
}

/// UTF-8 with an optional BOM, undecodable bytes replaced.
pub fn doc_text(data: &[u8]) -> String {
    let data = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
    String::from_utf8_lossy(data).into_owned()
}

/// What a source record may not say about its document: an entry this change
/// adds that names no part of it, or `covered` while a part is unaccounted for.
pub fn ledger_errors(config: &Config, record: &Value, old: Option<&Value>, rev: Option<&str>) -> Vec<String> {
    let rid = record.get("id").map(|id| id.as_str().map(str::to_owned).unwrap_or_else(|| id.to_string())).unwrap_or_else(|| "None".into());
    let Some(path) = record.get("path").and_then(Value::as_str).filter(|path| !bad_path(path)) else {
        return vec![format!("{rid}: `path` is the document's path from the repository root, with forward slashes")];
    };
    let status = record.get("status").and_then(Value::as_str);
    let Some(data) = read_doc(config, path, rev) else {
        if status == Some("covered") {
            return vec![format!("{rid}: {path} is neither in the working tree nor in HEAD, so `covered` cannot be checked - a document that is gone is `history`")];
        }
        return Vec::new();
    };
    let ledger: Vec<Value> = record.get("parts").and_then(Value::as_array).into_iter().flatten().filter(|entry| entry.is_object()).cloned().collect();
    let before: BTreeSet<String> = old
        .and_then(|old| old.get("parts"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("part").and_then(Value::as_str))
        .map(part_key)
        .collect();
    let found = coverage(&parts_of(&doc_text(&data)), &ledger);
    let mut errs: Vec<String> = found
        .unknown
        .iter()
        .filter(|name| !before.contains(&part_key(name)))
        .map(|name| format!("{rid}: part {} is not in {path} - `lore cover {path}` lists its parts by name", repr_str(name)))
        .collect();
    if status == Some("covered") && !found.uncovered.is_empty() {
        errs.push(format!(
            "{rid}: `covered`, but {} part(s) of {path} are unaccounted for, the first {} - `lore cover {path}` lists them",
            found.uncovered.len(),
            repr_str(&found.uncovered[0].name)
        ));
    }
    errs
}
