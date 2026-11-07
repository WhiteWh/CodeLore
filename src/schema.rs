//! The record kinds of FORMAT.md, as rules (CodeLore's `schema.py`).
//! `validate` refuses; it never repairs. Every sentence is CodeLore's.
use crate::py::{repr, repr_opt};
use crate::{LoreError, FORMAT_VERSION, READS};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub type RecordError = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Str,
    List,
    Deps,
    Int,
    /// One record's id.
    Ref,
    /// A list of record ids.
    Ids,
    /// A source's ledger of parts.
    Parts,
}

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub kind: Type,
    /// Characters for a string, per element for a list.
    pub max: Option<usize>,
    pub required: bool,
    /// Required only under these statuses (and allowed only under them).
    pub when: &'static [&'static str],
    /// Allowed only under these statuses.
    pub only: &'static [&'static str],
    /// Said when given under a status it does not belong to.
    pub hint: &'static str,
}

const fn field(name: &'static str, kind: Type, max: Option<usize>) -> Field {
    Field { name, kind, max, required: false, when: &[], only: &[], hint: "" }
}
const fn required(name: &'static str, kind: Type, max: Option<usize>) -> Field {
    Field { required: true, ..field(name, kind, max) }
}
const fn when(name: &'static str, kind: Type, max: Option<usize>, statuses: &'static [&'static str]) -> Field {
    Field { when: statuses, ..field(name, kind, max) }
}

pub const COMMON: &[Field] = &[
    required("v", Type::Int, None),
    required("id", Type::Str, None),
    required("kind", Type::Str, None),
    required("area", Type::Str, None),
    required("title", Type::Str, Some(120)),
    required("status", Type::Str, None),
    required("created", Type::Str, None),
    required("updated", Type::Str, None),
    required("by", Type::Str, Some(120)),
];

pub struct Kind {
    pub name: &'static str,
    pub statuses: &'static [&'static str],
    pub fields: &'static [Field],
}

pub const KINDS: &[Kind] = &[
    Kind {
        name: "fault",
        statuses: &["live", "cured", "obsolete", "superseded"],
        fields: &[
            required("symptom", Type::Str, Some(600)),
            field("signals", Type::List, Some(200)),
            required("cause", Type::Str, Some(600)),
            required("fix", Type::Str, Some(800)),
            required("check", Type::Str, Some(300)),
            when("superseded_by", Type::Ref, None, &["superseded"]),
        ],
    },
    Kind {
        name: "reading",
        statuses: &["current", "superseded"],
        fields: &[
            required("what", Type::Str, Some(300)),
            required("command", Type::Str, Some(300)),
            required("value", Type::Str, Some(200)),
            field("tolerance", Type::Str, Some(200)),
            required("measured_at", Type::Str, Some(64)),
            when("superseded_by", Type::Ref, None, &["superseded"]),
        ],
    },
    Kind {
        name: "decision",
        statuses: &["active", "reversed", "superseded"],
        fields: &[
            required("question", Type::Str, Some(300)),
            required("choice", Type::Str, Some(600)),
            required("reason", Type::Str, Some(800)),
            required("decided_by", Type::Str, Some(120)),
            when("reversed_by", Type::Ref, None, &["reversed"]),
            when("superseded_by", Type::Ref, None, &["superseded"]),
        ],
    },
    Kind {
        name: "topic",
        statuses: &["current", "superseded"],
        fields: &[required("body", Type::Str, Some(3000)), when("superseded_by", Type::Ref, None, &["superseded"])],
    },
    Kind {
        name: "recipe",
        statuses: &["current", "superseded"],
        fields: &[
            required("when", Type::Str, Some(300)),
            required("steps", Type::List, Some(400)),
            required("check", Type::Str, Some(300)),
            when("superseded_by", Type::Ref, None, &["superseded"]),
        ],
    },
    Kind {
        name: "plan",
        statuses: &["draft", "live", "paused", "closed"],
        fields: &[required("goal", Type::Str, Some(800)), field("doc", Type::Str, Some(300)), when("outcome", Type::Str, Some(2000), &["closed"])],
    },
    Kind {
        name: "item",
        statuses: &["open", "done", "dropped", "superseded", "deferred"],
        fields: &[
            required("plan", Type::Str, Some(48)),
            field("description", Type::Str, Some(2000)),
            field("criteria", Type::List, Some(300)),
            field("deps", Type::Deps, None),
            Field {
                hint: "the commits of an item still being built go in `refs.commits` (`-s refs.commits+=<sha>`); `lore item done --commit` closes it with them",
                ..when("commits", Type::List, Some(64), &["done"])
            },
            when("reason", Type::Str, Some(600), &["dropped", "deferred"]),
            when("decided_by", Type::Str, Some(120), &["dropped"]),
            when("superseded_by", Type::Ref, None, &["superseded"]),
            Field { only: &["done", "dropped", "superseded"], ..field("outcome", Type::Str, Some(1000)) },
        ],
    },
    Kind {
        name: "source",
        statuses: &["open", "covered", "history"],
        fields: &[
            required("path", Type::Str, Some(300)),
            when("reason", Type::Str, Some(600), &["history"]),
            field("records", Type::Ids, None),
            field("parts", Type::Parts, None),
        ],
    },
];

pub const KNOWLEDGE: &[&str] = &["fault", "reading", "decision", "topic", "recipe"];
pub const PLAN_KINDS: &[&str] = &["plan", "item"];
pub const REF_KEYS: &[&str] = &["commits", "files", "symbols", "links", "records"];
/// Statuses a search hides unless asked for everything.
pub const RETIRED: &[&str] = &["obsolete", "superseded", "reversed", "closed"];
/// Item statuses that still need someone's decision.
pub const UNDECIDED: &[&str] = &["open", "deferred"];

pub fn kind(name: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|kind| kind.name == name)
}

pub fn kind_names() -> Vec<&'static str> {
    KINDS.iter().map(|kind| kind.name).collect()
}

/// The format version a kind first appears in.
fn kind_since(kind: &str) -> u64 {
    match kind {
        "topic" | "recipe" => 2,
        "source" => 3,
        _ => 1,
    }
}

/// The format version a kind's status first appears in.
fn status_since(kind: &str, status: Option<&str>) -> u64 {
    if kind == "decision" && status == Some("superseded") { 3 } else { 1 }
}

/// Kinds that live outside the knowledge areas, and the area they name.
pub fn area_of(kind: &str) -> Option<&'static str> {
    match kind {
        "plan" | "item" => Some("plans"),
        "source" => Some("sources"),
        _ => None,
    }
}

/// The fields a person writes for each kind, in the order they are shown.
pub fn shown(kind: &str) -> Vec<&'static str> {
    self::kind(kind).map(|kind| kind.fields.iter().map(|field| field.name).collect()).unwrap_or_default()
}

/// Today's local date, as CodeLore stamps records.
pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

pub fn check_limits(limits: &BTreeMap<String, usize>) -> crate::Result<()> {
    let mut bad = Vec::new();
    for (key, n) in limits {
        let (kind_name, name) = key.split_once('.').unwrap_or((key.as_str(), ""));
        let found = kind(kind_name).and_then(|kind| kind.fields.iter().find(|field| field.name == name).or_else(|| COMMON.iter().find(|field| field.name == name)));
        match found.and_then(|field| field.max) {
            None => bad.push(format!("`limits.{key}` names no limited field")),
            Some(max) if *n > max => bad.push(format!("`limits.{key}` = {n} raises the format's ceiling of {max}; limits may only lower it")),
            Some(_) => {}
        }
    }
    if bad.is_empty() { Ok(()) } else { Err(LoreError::with("lore.toml's [limits] is refused", bad)) }
}

/// `^[a-z0-9][a-z0-9-]{min,max}$` without a regex.
pub(crate) fn slug(text: &str, min_rest: usize, max_rest: usize) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else { return false };
    let rest: Vec<char> = chars.collect();
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && (min_rest..=max_rest).contains(&rest.len())
        && rest.iter().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '-')
}

/// `ID`: `^[a-z0-9][a-z0-9-]{1,47}$`.
pub fn plain_id(text: &str) -> bool {
    slug(text, 1, 47)
}

/// `ITEM_ID`: the plan id and the item's number.
pub fn item_id(text: &str) -> Option<(&str, u64)> {
    let (plan, number) = text.split_once('#')?;
    let digits_ok = !number.is_empty() && number.len() <= 6 && number.chars().all(|ch| ch.is_ascii_digit()) && !number.starts_with('0');
    (plain_id(plan) && digits_ok).then(|| (plan, number.parse().unwrap_or(0)))
}

pub fn is_id(value: &Value) -> bool {
    value.as_str().is_some_and(|text| plain_id(text) || item_id(text).is_some())
}

/// `SHA`: `^[0-9a-f]{4,64}$`.
pub fn is_sha(text: &str) -> bool {
    (4..=64).contains(&text.len()) && text.chars().all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch))
}

pub fn item_number(id: &str) -> u64 {
    item_id(id).map(|(_, number)| number).unwrap_or(0)
}

/// A real calendar date written `YYYY-MM-DD`.
pub fn is_date(value: &Value) -> bool {
    let Some(text) = value.as_str() else { return false };
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !text.chars().enumerate().all(|(index, ch)| index == 4 || index == 7 || ch.is_ascii_digit()) {
        return false;
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok() && text[..4].parse::<u32>().is_ok_and(|year| year >= 1)
}

fn empty(value: &Value) -> bool {
    value.is_null() || value.as_str() == Some("") || value.as_array().is_some_and(Vec::is_empty)
}

fn python_strip(text: &str) -> &str {
    text.trim_matches(|ch: char| ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch))
}

fn is_int(value: &Value) -> bool {
    value.is_i64() || value.is_u64()
}

/// A path Windows or POSIX would read as absolute or backslashed.
pub fn bad_path(path: &str) -> bool {
    let mut chars = path.chars();
    let drive = matches!((chars.next(), chars.next()), (Some(letter), Some(':')) if letter.is_ascii_alphabetic());
    path.contains('\\') || path.starts_with('/') || drive
}

/// Every rule the record breaks, as sentences. Empty when it is valid.
pub fn validate(record: &Value, areas: &[String], limits: Option<&BTreeMap<String, usize>>) -> Vec<RecordError> {
    let empty_limits = BTreeMap::new();
    let limits = limits.unwrap_or(&empty_limits);
    let mut errs = Vec::new();
    let Some(map) = record.as_object() else { return vec!["a record is a JSON object".into()] };
    let kind_value = map.get("kind");
    let Some(spec) = kind_value.and_then(Value::as_str).and_then(kind) else {
        return vec![format!("`kind` must be one of {}, not {}", kind_names().join(", "), repr_opt(kind_value))];
    };
    let kind_name = spec.name;
    let rid = map.get("id").cloned().unwrap_or(Value::String(String::new()));
    let rid_present = !empty(&rid) && rid != Value::Bool(false) && rid.as_i64() != Some(0);
    let who = if rid_present { format!("{kind_name} {}", repr(&rid)) } else { kind_name.to_owned() };

    let version = map.get("v");
    let read = version.filter(|value| is_int(value)).and_then(Value::as_u64).filter(|number| READS.contains(number));
    match read {
        None => match version.and_then(Value::as_i64) {
            Some(number) if is_int(version.unwrap()) && number > FORMAT_VERSION as i64 => {
                errs.push(format!("{who}: format version {number} is newer than this lore reads ({FORMAT_VERSION}) - upgrade lore"));
            }
            _ => errs.push(format!("{who}: `v` must be one of {}", READS.iter().map(u64::to_string).collect::<Vec<_>>().join(", "))),
        },
        Some(number) if number < kind_since(kind_name) => {
            errs.push(format!("{who}: a {kind_name} needs format version {} or later, not {number}", kind_since(kind_name)));
        }
        Some(number) => {
            let status = map.get("status").and_then(Value::as_str);
            let since = status_since(kind_name, status);
            if number < since {
                errs.push(format!("{who}: a {} {kind_name} needs format version {since} or later, not {number}", status.unwrap_or("None")));
            }
        }
    }

    let status_value = map.get("status");
    let status = status_value.and_then(Value::as_str);
    for field in COMMON.iter().chain(spec.fields) {
        let value = map.get(field.name).unwrap_or(&Value::Null);
        let needed = field.required || (!field.when.is_empty() && status.is_some_and(|status| field.when.contains(&status)));
        if empty(value) {
            if needed {
                let condition = if field.when.is_empty() { String::new() } else { format!(" when `{}`", status.unwrap_or("None")) };
                errs.push(format!("{who}: `{}` is required{condition}", field.name));
            }
            continue;
        }
        let allowed = if field.only.is_empty() { field.when } else { field.only };
        if !allowed.is_empty() && !status.is_some_and(|status| allowed.contains(&status)) {
            let hint = if field.hint.is_empty() { String::new() } else { format!(" - {}", field.hint) };
            errs.push(format!("{who}: `{}` belongs to status {}, not `{}`{hint}", field.name, allowed.join("/"), status_text(status_value)));
        }
        let limit = limits.get(&format!("{kind_name}.{}", field.name)).copied().or(field.max);
        match field.kind {
            Type::Int => {
                if !is_int(value) {
                    errs.push(format!("{who}: `{}` must be an integer", field.name));
                }
            }
            Type::Str => match value.as_str() {
                None => errs.push(format!("{who}: `{}` must be a string", field.name)),
                Some(text) if limit.is_some_and(|limit| text.chars().count() > limit) => errs.push(format!(
                    "{who}: `{}` is {} characters, the limit is {} - a record says what, not how it was found; the story goes in the commit",
                    field.name,
                    text.chars().count(),
                    limit.unwrap_or(0)
                )),
                Some(text) if text != python_strip(text) => errs.push(format!("{who}: `{}` has leading or trailing whitespace", field.name)),
                Some(_) => {}
            },
            Type::List => match value.as_array().filter(|items| items.iter().all(|item| item.as_str().is_some_and(|text| !text.is_empty()))) {
                None => errs.push(format!("{who}: `{}` must be a list of non-empty strings", field.name)),
                Some(items) => {
                    for item in items {
                        let count = item.as_str().unwrap_or_default().chars().count();
                        if let Some(limit) = limit.filter(|limit| count > *limit) {
                            errs.push(format!("{who}: an element of `{}` is {count} characters, the limit is {limit}", field.name));
                        }
                    }
                    let unique: BTreeSet<&str> = items.iter().filter_map(Value::as_str).collect();
                    if unique.len() != items.len() {
                        errs.push(format!("{who}: `{}` has duplicates", field.name));
                    }
                }
            },
            Type::Ref => {
                if !is_id(value) {
                    errs.push(format!("{who}: `{}` names one record by its id, and {} is not an id", field.name, repr(value)));
                }
            }
            Type::Ids => match value.as_array().filter(|items| items.iter().all(is_id)) {
                None => errs.push(format!("{who}: `{}` must be a list of record ids", field.name)),
                Some(items) => {
                    let unique: BTreeSet<&str> = items.iter().filter_map(Value::as_str).collect();
                    if unique.len() != items.len() {
                        errs.push(format!("{who}: `{}` has duplicates", field.name));
                    }
                }
            },
            Type::Parts => errs.extend(parts_errors(value).into_iter().map(|error| format!("{who}: {error}"))),
            Type::Deps => match value.as_array() {
                None => errs.push(format!("{who}: `deps` must be a list")),
                Some(deps) => {
                    let mut seen = BTreeSet::new();
                    for dep in deps {
                        let item = dep.get("item").and_then(Value::as_str);
                        let good = dep.as_object().is_some_and(|map| map.len() == 2 && map.contains_key("item") && map.contains_key("kind"))
                            && matches!(dep.get("kind").and_then(Value::as_str), Some("hard" | "soft"))
                            && item.is_some_and(|item| item_id(item).is_some());
                        if !good {
                            errs.push(format!("{who}: each dependency is {{\"item\": \"<plan>#<n>\", \"kind\": \"hard\"|\"soft\"}}, not {}", repr(dep)));
                            continue;
                        }
                        let item = item.unwrap_or_default();
                        if seen.contains(item) {
                            errs.push(format!("{who}: depends on {item} twice"));
                        }
                        seen.insert(item);
                        if rid.as_str() == Some(item) {
                            errs.push(format!("{who}: depends on itself"));
                        }
                    }
                }
            },
        }
    }

    if !status.is_some_and(|status| spec.statuses.contains(&status)) {
        errs.push(format!("{who}: `status` must be one of {}, not {}", spec.statuses.join(", "), repr_opt(status_value)));
    }

    if let Some(id) = rid.as_str() {
        if kind_name == "item" {
            match id.split_once('#').filter(|_| item_id(id).is_some()) {
                None => errs.push(format!("{who}: an item's `id` is `<plan-id>#<n>`")),
                Some((plan, _)) => {
                    if map.get("plan").and_then(Value::as_str) != Some(plan) {
                        errs.push(format!("{who}: the id names plan {} but `plan` is {}", crate::py::repr_str(plan), repr_opt(map.get("plan"))));
                    }
                }
            }
        } else if !plain_id(id) {
            errs.push(format!("{who}: `id` must match ^[a-z0-9][a-z0-9-]{{1,47}}$"));
        }
    }

    let area = map.get("area");
    if let Some(expected) = area_of(kind_name) {
        if area.and_then(Value::as_str) != Some(expected) {
            errs.push(format!("{who}: a {kind_name}'s `area` is `{expected}`"));
        }
    } else if !area.and_then(Value::as_str).is_some_and(|area| areas.iter().any(|known| known == area)) {
        let declared = if areas.is_empty() { "none declared".to_owned() } else { areas.join(", ") };
        errs.push(format!("{who}: area {} is not one of lore.toml's areas ({declared})", repr_opt(area)));
    }

    for name in ["created", "updated"] {
        if let Some(value) = map.get(name).filter(|value| value.is_string()) {
            if !is_date(value) {
                errs.push(format!("{who}: `{name}` must be a date, YYYY-MM-DD"));
            }
        }
    }
    if let (Some(created), Some(updated)) = (map.get("created").filter(|value| is_date(value)), map.get("updated").filter(|value| is_date(value))) {
        if updated.as_str() < created.as_str() {
            errs.push(format!("{who}: `updated` is before `created`"));
        }
    }

    if kind_name == "item" {
        if let Some(commits) = map.get("commits").and_then(Value::as_array) {
            for commit in commits.iter().filter_map(Value::as_str) {
                if !is_sha(commit) {
                    errs.push(format!("{who}: commit {} is not a hash", crate::py::repr_str(commit)));
                }
            }
        }
    }
    if kind_name == "reading" {
        if let Some(at) = map.get("measured_at").and_then(Value::as_str) {
            if !is_sha(at) {
                errs.push(format!("{who}: `measured_at` must be a commit hash"));
            }
        }
    }

    if let Some(refs) = map.get("refs").filter(|refs| !refs.is_null()) {
        match refs.as_object() {
            None => errs.push(format!("{who}: `refs` must be an object")),
            Some(refs) => {
                for (key, value) in refs {
                    if !REF_KEYS.contains(&key.as_str()) {
                        errs.push(format!("{who}: `refs.{key}` is not one of {}", REF_KEYS.join(", ")));
                    } else if !value.as_array().is_some_and(|items| items.iter().all(|item| item.as_str().is_some_and(|text| !text.is_empty()))) {
                        errs.push(format!("{who}: `refs.{key}` must be a list of non-empty strings"));
                    } else if key == "commits" {
                        for commit in value.as_array().into_iter().flatten().filter_map(Value::as_str) {
                            if !is_sha(commit) {
                                errs.push(format!("{who}: `refs.commits` holds {}, not a hash", crate::py::repr_str(commit)));
                            }
                        }
                    } else if key == "files" {
                        for path in value.as_array().into_iter().flatten().filter_map(Value::as_str) {
                            if bad_path(path) {
                                errs.push(format!("{who}: `refs.files` holds {} - paths are relative, with forward slashes", crate::py::repr_str(path)));
                            }
                        }
                    }
                }
            }
        }
    }
    // docs/lore.md 11.10: `seen`, the occurrences of a knowledge record, is an
    // optional list of at most 20 strings of at most 160 characters.
    if let Some(seen) = map.get("seen") {
        match seen.as_array() {
            Some(items) if KNOWLEDGE.contains(&kind_name) => {
                if items.len() > SEEN_MAX {
                    errs.push(format!("{who}: `seen` holds at most {SEEN_MAX} occurrences"));
                }
                if items.iter().any(|item| item.as_str().is_none_or(|text| text.chars().count() > SEEN_TEXT)) {
                    errs.push(format!("{who}: each `seen` occurrence is a string of at most {SEEN_TEXT} characters"));
                }
            }
            Some(_) => errs.push(format!("{who}: only a knowledge record has `seen`")),
            None => errs.push(format!("{who}: `seen` must be a list")),
        }
    }
    errs
}

/// At most this many occurrences a record's `seen` keeps (11.10).
pub const SEEN_MAX: usize = 20;
pub const SEEN_TEXT: usize = 160;

/// A status as an f-string writes it: the text, or `None`.
fn status_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// The keys of one entry in a source's ledger.
pub const PART_KEYS: &[&str] = &["part", "records", "skip"];

/// A part's name as compared: case and runs of whitespace folded.
pub fn part_key(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// A source's ledger: each entry names one part of the document and says
/// which records hold it, or why it is skipped, or both.
pub fn parts_errors(value: &Value) -> Vec<String> {
    let Some(entries) = value.as_array() else { return vec!["`parts` must be a list of {\"part\", \"records\", \"skip\"} entries".into()] };
    let mut errs = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let part = entry.get("part").and_then(Value::as_str);
        let shaped = entry.as_object().is_some_and(|map| map.keys().all(|key| PART_KEYS.contains(&key.as_str()))) && part.is_some_and(|part| !part.trim().is_empty());
        if !shaped {
            errs.push(format!("each entry of `parts` is {{\"part\": \"<name>\", \"records\": [<id>...], \"skip\": \"<why>\"}}, not {}", repr(entry)));
            continue;
        }
        let part = part.unwrap_or_default();
        let shown = crate::py::repr_str(part);
        let count = part.chars().count();
        if count > 300 {
            let head: String = part.chars().take(40).collect();
            errs.push(format!("part {}...: the name is {count} characters, the limit is 300", crate::py::repr_str(&head)));
        }
        let records = entry.get("records").filter(|value| !value.is_null());
        let skip = entry.get("skip").filter(|value| !value.is_null());
        if let Some(records) = records {
            if !records.as_array().is_some_and(|items| !items.is_empty() && items.iter().all(is_id)) {
                errs.push(format!("part {shown}: `records` must be a non-empty list of record ids"));
            }
        }
        if let Some(skip) = skip {
            if !skip.as_str().is_some_and(|text| !text.trim().is_empty() && text.chars().count() <= 300) {
                errs.push(format!("part {shown}: `skip` is why it holds nothing current, 1-300 characters"));
            }
        }
        if records.is_none() && skip.is_none() {
            errs.push(format!("part {shown}: says neither which records hold it nor why it is skipped"));
        }
        if !seen.insert(part_key(part)) {
            errs.push(format!("part {shown} is in `parts` twice"));
        }
    }
    errs
}

/// A signal that would match every fault of a family, not this one:
/// `AttributeError`, `System.IO.IOException`, `error`, `Traceback`. A code
/// (`ERR_DEVICE_LOST`, `0x80070005`, `E_FAIL`) or a whole message is not.
pub fn generic_signal(signal: &str) -> bool {
    let text = signal.trim();
    let one_word = !text.is_empty() && text.chars().all(|ch| ch.is_ascii_alphabetic());
    let bare_class = {
        let mut chars = text.chars();
        let first_ok = chars.next().is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
        let body_ok = text.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '.');
        first_ok && body_ok && ["Error", "Exception", "Warning", "Failure", "Fault"].iter().any(|end| text.ends_with(end))
    };
    one_word || bare_class
}

/// The sentence a record is about, for display and for its embedding.
pub fn main_text(record: &Value) -> String {
    for key in ["symptom", "what", "question", "when", "goal", "body", "description"] {
        match record.get(key) {
            Some(value) if !crate::ops::falsy(value) => return value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string()),
            _ => {}
        }
    }
    String::new()
}

/// How an item may move. Leaving a closed status needs a reopen with a reason.
pub fn item_moves(from: &str) -> &'static [&'static str] {
    match from {
        "open" => &["done", "dropped", "superseded", "deferred", "open"],
        "deferred" => &["open", "done", "dropped", "superseded", "deferred"],
        "done" => &["done"],
        "dropped" => &["dropped"],
        "superseded" => &["superseded"],
        _ => &[],
    }
}

/// Task 150: a warning, never a refusal. A fault should name 2-3 signals in
/// the words a person would use, not only exact strings.
pub fn warnings(record: &Value) -> Vec<String> {
    if record.get("kind").and_then(Value::as_str) != Some("fault") { return Vec::new(); }
    let human = record.get("signals").and_then(Value::as_array).map(|signals| {
        signals.iter().filter_map(Value::as_str)
            .filter(|signal| signal.contains(' ') && !signal.starts_with("0x") && !signal.contains("\\\\"))
            .count()
    }).unwrap_or(0);
    if human < 2 {
        vec!["add 2-3 human-language signals (how a person would see it), not only exact strings".into()]
    } else {
        Vec::new()
    }
}
