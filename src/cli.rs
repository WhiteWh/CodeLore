//! The `lore` command (CodeLore's `cli.py`) as a library call: `run(argv, env)`
//! answers with the exit code and the text it prints. The `lore` binary and
//! `hgs lore` only pass their arguments and stdin through.
//!
//! JSON shapes (`--json`, printed with two-space indents as Python's
//! `json.dumps(indent=2, ensure_ascii=False)` does; `shape_*` build them):
//! - any write (`add`, `update`, `plan …`, `item …`, `cover --add|--history`):
//!   `{"commit": sha|null, "records": [record...], "notes": [text...]}`, plus
//!   `"pushed"` and `"push_error"` when a push was asked for;
//! - `find`: `{"query", "notes", "results": [{"score", "via", "record"}]}`;
//! - `for`, `items`, `stale`: a list of records; `show`: the record;
//! - `exists`: `{id: null | {"kind", "status", "title"}}`, exit 1 if one is missing;
//! - `areas`: `{area: count}`; `plans`: `[{"plan", "counts"}]`;
//! - `check`: `{"records", "files", "errors", "warnings"}`, exit 1 on an error;
//! - `cover`: `{"path", "source", "parts", "uncovered": [{"part", "kind", "line"}],
//!   "unknown", "complete"}`, exit 1 until complete;
//! - `index`: `{"records", "vectors", "index", "semantic", "note"}`;
//! - any refusal: `{"error": <its first line>, "details": [...]}`, exit 1.
use crate::config::{self, Config};
use crate::cover;
use crate::dedupe;
use crate::ops::{self, Change, Options, Outcome, Patch};
use crate::plans;
use crate::py::{fill, repr_str};
use crate::refs::Roots;
use crate::schema::{self, check_limits, item_number, KNOWLEDGE, UNDECIDED};
use crate::search::{self, Filters, Finder};
use crate::store::{self, is_lore_file, serialize, Snapshot};
use crate::{gitio, private, LoreError, Result, VERSION};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const WIDTH: usize = 100;
pub const GUIDE: &str = include_str!("guide.md");

/// What a call runs with besides its arguments.
#[derive(Default)]
pub struct Env<'a> {
    /// Where it runs when no `-C` is given.
    pub cwd: PathBuf,
    /// stdin's bytes.
    pub stdin: Vec<u8>,
    /// `LORE_BY`.
    pub lore_by: Option<String>,
    /// The ranked search; without it `find` and `index` are refused.
    pub finder: Option<&'a dyn Finder>,
    /// A Zerus project store's reference roots (docs/lore.md 2.4).
    pub roots: Option<Roots>,
    /// The time the working tier reads (UTC seconds); None: the clock.
    pub now: Option<i64>,
    /// Verbatim history (11.7): Zerus's content-search index, or none.
    pub history: Option<std::sync::Arc<dyn crate::history::HistorySource>>,
    /// Whose sessions the history channel may return.
    pub history_scope: crate::history::Scope,
    /// A message's delivery (11.11): only Zerus has one.
    pub doorbell: Option<&'a dyn crate::working::Doorbell>,
    /// How expiry learns a task's life (11.14); None: the plans' items.
    pub clock: Option<&'a dyn crate::working::TaskClock>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// What a terminal shows: stdout, then stderr (CodeLore's tests read both).
    pub fn text(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

struct Io {
    out: String,
    err: String,
}

impl Io {
    fn say(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push('\n');
    }
    fn note(&mut self, text: &str) {
        self.say(&format!("note: {text}"));
    }
    fn progress(&mut self, text: &str) {
        self.err.push_str(text);
        self.err.push('\n');
    }
    fn json(&mut self, value: &Value) {
        self.say(&pretty(value));
    }
}

/// Python's `json.dumps(value, ensure_ascii=False, indent=2)`.
pub fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into())
}

// -- the JSON shapes ------------------------------------------------------------------

pub fn shape_write(outcome: &Outcome, push_line: Option<&str>) -> Value {
    let mut notes = outcome.notes.clone();
    if outcome.pushed == Some(false) {
        notes.extend(push_line.map(str::to_owned));
    }
    let mut value = json!({"commit": outcome.commit, "records": outcome.records, "notes": notes});
    if let Some(pushed) = outcome.pushed {
        value["pushed"] = json!(pushed);
        value["push_error"] = json!(outcome.push_error);
    }
    value
}

pub fn shape_find(query: &str, notes: &[String], results: &[search::Found]) -> Value {
    shape_find_at(query, notes, results, chrono::Utc::now().timestamp())
}

/// Task 133: every result says its tier and a confidence (additive keys).
pub fn shape_find_at(query: &str, notes: &[String], results: &[search::Found], now: i64) -> Value {
    json!({"query": query, "notes": notes, "results": results.iter().map(|hit| durable_result(hit, now)).collect::<Vec<_>>()})
}

pub fn durable_result(hit: &search::Found, now: i64) -> Value {
    let confidence = search::confidence("durable", &hit.via, hit.meaning, search::record_age(&hit.record, now), search::retired(&hit.record));
    json!({"score": hit.score, "via": hit.via, "record": hit.record, "tier": "durable", "confidence": confidence})
}

/// The text line a result's confidence reads as.
pub fn confidence_line(confidence: &Value) -> String {
    let mut parts = vec![format!("{} confidence", confidence["level"].as_str().unwrap_or("low")), confidence["tier"].as_str().unwrap_or_default().to_owned()];
    if confidence["exact"] == true {
        parts.push("exact match".into());
    }
    if let Some(cosine) = confidence["semantic"].as_f64() {
        parts.push(format!("semantic {}", crate::py::float_str(cosine)));
    }
    if let Some(days) = confidence["age_days"].as_i64() {
        parts.push(format!("{days} d old"));
    }
    if confidence["superseded"] == true {
        parts.push("superseded".into());
    }
    format!("  ({})", parts.join(", "))
}

pub fn shape_check(records: usize, files: usize, errors: &[String], warnings: &[String]) -> Value {
    json!({"records": records, "files": files, "errors": errors, "warnings": warnings})
}

pub fn shape_index(info: &search::IndexInfo) -> Value {
    json!({"records": info.records, "vectors": info.vectors, "index": info.index, "semantic": info.semantic, "note": info.note})
}

/// How agents search (task 149): counts only, never a query or a record's text.
pub fn shape_stats(report: &crate::stats::Report) -> Value {
    fn metric(metric: &crate::stats::Metric) -> Value {
        json!({"finds": metric.finds, "opened": metric.opened, "hit1": metric.hit1, "hit5": metric.hit5,
               "hit1_rate": metric.hit1_rate(), "hit5_rate": metric.hit5_rate()})
    }
    let mut by_class = serde_json::Map::new();
    for (name, value) in &report.by_class {
        by_class.insert(name.clone(), metric(value));
    }
    let mut by_caller = serde_json::Map::new();
    for (name, value) in &report.by_caller {
        by_caller.insert(name.clone(), metric(value));
    }
    json!({
        "window_days": report.window_days,
        "finds": report.finds,
        "opened": report.opened,
        "hit1": report.hit1,
        "hit5": report.hit5,
        "by_class": by_class,
        "by_caller": by_caller,
        "tiers": report.tiers,
        "latency_ms_mean": report.latency_ms,
        "misses": report.misses,
        "retention": {"days": crate::stats::RETENTION_DAYS, "max_finds": crate::stats::RETENTION_FINDS, "finds": report.retained},
    })
}

pub fn shape_error(error: &LoreError) -> Value {
    let first = error.to_string().lines().next().unwrap_or_default().to_owned();
    json!({"error": first, "details": error.details})
}

// -- the arguments ------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Args {
    c: Option<String>,
    json: bool,
    by: Option<String>,
    trailers: Vec<String>,
    mode: Option<String>,
    text_only: bool,
    /// The command's words, `plan new` as two.
    command: Vec<String>,
    positional: Vec<String>,
    /// Options by their long name; flags hold "".
    options: BTreeMap<String, Vec<String>>,
}

impl Args {
    fn flag(&self, name: &str) -> bool {
        self.options.contains_key(name)
    }
    fn one(&self, name: &str) -> Option<String> {
        self.options.get(name).and_then(|values| values.last().cloned())
    }
    fn many(&self, name: &str) -> Vec<String> {
        self.options.get(name).cloned().unwrap_or_default()
    }
    fn cmd(&self) -> String {
        self.command.join(" ")
    }
}

struct Usage(String);

/// The options a command takes: (long name, aliases, takes a value).
fn options_of(command: &str) -> Option<Vec<(&'static str, &'static [&'static str], bool)>> {
    let writes: (&str, &[&str], bool) = ("set", &["-s"], true);
    let filters: [(&str, &[&str], bool); 5] = [("kind", &[], true), ("area", &[], true), ("status", &[], true), ("plan", &[], true), ("all", &[], false)];
    Some(match command {
        "init" => vec![("dir", &[], true), ("areas", &[], true), ("commit-mode", &[], true), ("branch", &[], true)],
        "guide" | "areas" | "fmt" | "show" | "exists" | "plan show" => vec![],
        "check" => vec![("evidence", &[], false)],
        "index" => vec![("rebuild", &[], false)],
        "stats" => vec![("days", &[], true)],
        "find" => {
            let mut list = filters.to_vec();
            list.push(("n", &["-n"], true));
            // docs/lore.md 11.6: the working tier and the history channel.
            list.extend([("working", &[] as &[&str], false), ("no-working", &[], false), ("history", &[], false), ("no-history", &[], false), ("team", &[], true), ("role", &[], true)]);
            list
        }
        "for" => vec![("all", &[], false)],
        "cover" => vec![("list", &[], false), ("add", &[], false), ("history", &[], false), ("reason", &[], true), ("record", &[], true), ("title", &[], true)],
        "add" => vec![writes, ("json-in", &["--stdin"], false), ("batch", &[], false), ("new", &[], false), ("skip-duplicates", &[], false), ("occurrence", &[], false)],
        "update" => vec![writes, ("json-in", &["--stdin"], false), ("batch", &[], false)],
        "plans" => vec![("all", &[], false)],
        "items" => vec![("plan", &[], true), ("status", &[], true), ("ready", &[], false), ("all", &[], false)],
        "stale" => vec![("days", &[], true)],
        "plan new" => vec![writes, ("title", &[], true), ("goal", &[], true), ("id", &[], true), ("doc", &[], true), ("status", &[], true)],
        "plan set" => vec![writes, ("status", &[], true)],
        "plan close" => vec![("outcome", &[], true)],
        "item add" => vec![writes, ("title", &[], true), ("description", &[], true), ("criterion", &[], true), ("dep", &[], true)],
        "item done" => vec![("commit", &[], true), ("outcome", &[], true)],
        "item drop" => vec![("reason", &[], true), ("decided-by", &[], true), ("outcome", &[], true)],
        "item supersede" => vec![("with", &[], true), ("outcome", &[], true)],
        "item defer" | "item reopen" => vec![("reason", &[], true)],
        // docs/lore.md 11.13: the working tier.
        "pulse add" => vec![("task", &[], true), ("done", &[], true), ("next", &[], true), ("blocked", &[], true), ("team", &[], true), ("role", &[], true)],
        "pulses" => vec![("task", &[], true), ("since", &[], true), ("all", &[], false), ("team", &[], true), ("role", &[], true)],
        "message send" => vec![("to", &[], true), ("urgent", &[], false), ("team", &[], true), ("role", &[], true), ("recipients", &[], true)],
        "messages" => vec![("unread", &[], false), ("to", &[], true), ("from", &[], true), ("peek", &[], false), ("team", &[], true), ("role", &[], true)],
        "message read" => vec![("team", &[], true), ("role", &[], true)],
        "brief set" => vec![("task", &[], true), ("title", &[], true), ("team", &[], true), ("role", &[], true)],
        "brief show" => vec![("task", &[], true), ("version", &[], true), ("team", &[], true), ("role", &[], true)],
        "promote" => vec![("kind", &[], true), ("field", &[], true), ("dry-run", &[], false), ("query", &[], true), ("team", &[], true), ("role", &[], true)],
        "supersede" => vec![],
        "working status" | "working cleanup" => vec![],
        _ => return None,
    })
}

/// The commands of two words and their second words.
fn subcommands(command: &str) -> Option<&'static [&'static str]> {
    match command {
        "plan" => Some(PLAN_COMMANDS),
        "item" => Some(ITEM_COMMANDS),
        "pulse" => Some(&["add"]),
        "message" => Some(&["send", "read"]),
        "brief" => Some(&["set", "show"]),
        "working" => Some(&["status", "cleanup"]),
        _ => None,
    }
}

/// The commands, as `lore guide` may name them.
pub const COMMANDS: &[&str] = &[
    "init", "guide", "areas", "check", "fmt", "index", "find", "for", "show", "exists", "cover", "add", "update", "plans", "items", "stale", "stats", "plan", "item",
    "pulse", "pulses", "message", "messages", "brief", "promote", "supersede", "working",
];
pub const PLAN_COMMANDS: &[&str] = &["new", "show", "set", "close"];
pub const ITEM_COMMANDS: &[&str] = &["add", "done", "drop", "supersede", "defer", "reopen"];

fn parse_args(argv: &[String]) -> std::result::Result<Args, Usage> {
    let mut args = Args::default();
    let mut index = 0;
    let mut spec: Option<Vec<(&'static str, &'static [&'static str], bool)>> = None;
    let mut rest_only = false;
    while index < argv.len() {
        let word = argv[index].clone();
        index += 1;
        let take = |index: &mut usize, inline: Option<String>, name: &str| -> std::result::Result<String, Usage> {
            if let Some(value) = inline {
                return Ok(value);
            }
            let value = argv.get(*index).cloned().ok_or_else(|| Usage(format!("argument {name}: expected one argument")))?;
            *index += 1;
            Ok(value)
        };
        if !rest_only && word == "--" {
            rest_only = true;
            continue;
        }
        if !rest_only && word.starts_with('-') && word != "-" && !(word.len() > 1 && word[1..].chars().all(|ch| ch.is_ascii_digit())) {
            let (name, inline) = match word.split_once('=') {
                Some((name, value)) if name.starts_with("--") => (name.to_owned(), Some(value.to_owned())),
                _ => (word.clone(), None),
            };
            match name.as_str() {
                "-C" => args.c = Some(take(&mut index, inline, "-C")?),
                "--json" => args.json = true,
                "--by" => args.by = Some(take(&mut index, inline, "--by")?),
                "--trailer" => args.trailers.push(take(&mut index, inline, "--trailer")?),
                "--mode" => {
                    let mode = take(&mut index, inline, "--mode")?;
                    if !config::MODES.contains(&mode.as_str()) {
                        return Err(Usage(format!("argument --mode: invalid choice: {} (choose from {})", repr_str(&mode), config::MODES.iter().map(|mode| repr_str(mode)).collect::<Vec<_>>().join(", "))));
                    }
                    args.mode = Some(mode);
                }
                "--text-only" => args.text_only = true,
                "--version" if args.command.is_empty() => {
                    args.command = vec!["--version".into()];
                    return Ok(args);
                }
                _ => {
                    let Some(options) = &spec else { return Err(Usage(format!("unrecognized arguments: {word}"))) };
                    let found = options.iter().find(|(long, aliases, _)| name == format!("--{long}") || aliases.contains(&name.as_str()));
                    // `-sKEY=VALUE`: a short option with its value attached.
                    let attached = if found.is_none() && !name.starts_with("--") && name.len() > 2 {
                        options.iter().find(|(_, aliases, value)| *value && aliases.contains(&&name[..2])).map(|option| (option, name[2..].to_owned()))
                    } else {
                        None
                    };
                    match (found, attached) {
                        (Some((long, _, true)), _) => {
                            let value = take(&mut index, inline, &format!("--{long}"))?;
                            args.options.entry((*long).to_owned()).or_default().push(value);
                        }
                        (Some((long, _, false)), _) => {
                            args.options.entry((*long).to_owned()).or_default();
                        }
                        (None, Some(((long, _, _), value))) => args.options.entry((*long).to_owned()).or_default().push(value),
                        _ => return Err(Usage(format!("unrecognized arguments: {word}"))),
                    }
                }
            }
            continue;
        }
        if args.command.is_empty() {
            if !COMMANDS.contains(&word.as_str()) {
                return Err(Usage(format!("argument <command>: invalid choice: {}", repr_str(&word))));
            }
            args.command.push(word.clone());
            if subcommands(&word).is_none() {
                spec = options_of(&word);
            }
            continue;
        }
        if args.command.len() == 1 && subcommands(&args.command[0]).is_some() {
            let allowed = subcommands(&args.command[0]).unwrap_or_default();
            if !allowed.contains(&word.as_str()) {
                return Err(Usage(format!("argument <{} command>: invalid choice: {}", args.command[0], repr_str(&word))));
            }
            args.command.push(word);
            spec = options_of(&args.cmd());
            continue;
        }
        args.positional.push(word);
    }
    Ok(args)
}

/// The positional arguments and required options each command must have.
fn check_shape(args: &Args) -> std::result::Result<(), Usage> {
    let command = args.cmd();
    let need = |names: &[&str]| -> std::result::Result<(), Usage> {
        let missing: Vec<String> = names.iter().filter(|name| !args.options.contains_key(**name)).map(|name| format!("--{name}")).collect();
        if missing.is_empty() { Ok(()) } else { Err(Usage(format!("the following arguments are required: {}", missing.join(", ")))) }
    };
    let positional = |least: usize, most: Option<usize>, names: &str| -> std::result::Result<(), Usage> {
        if args.positional.len() < least {
            return Err(Usage(format!("the following arguments are required: {names}")));
        }
        if most.is_some_and(|most| args.positional.len() > most) {
            return Err(Usage(format!("unrecognized arguments: {}", args.positional[most.unwrap_or(0)..].join(" "))));
        }
        Ok(())
    };
    match command.as_str() {
        "" => Err(Usage("a command is needed".into())),
        "plan" | "item" | "pulse" | "message" | "brief" | "working" => Err(Usage(format!("a {command} command is needed"))),
        "pulse add" | "pulses" | "messages" | "brief set" | "brief show" | "working status" | "working cleanup" => positional(0, Some(0), ""),
        "message send" => positional(0, None, ""),
        "message read" | "promote" => positional(1, Some(1), "id"),
        "supersede" => positional(2, Some(2), "old new"),
        "init" | "guide" | "areas" | "check" | "fmt" | "index" | "plans" | "items" | "stale" | "stats" => positional(0, Some(0), ""),
        "find" => positional(1, None, "query"),
        "for" => positional(1, Some(1), "target"),
        "show" | "plan show" => positional(1, Some(1), "id"),
        "exists" => positional(1, None, "id"),
        "cover" => positional(1, Some(1), "doc"),
        "add" => {
            positional(1, Some(1), "kind")?;
            let kind = &args.positional[0];
            let mut allowed: Vec<&str> = KNOWLEDGE.to_vec();
            allowed.extend(["plan", "item", "any"]);
            if !allowed.contains(&kind.as_str()) {
                return Err(Usage(format!("argument kind: invalid choice: {} (choose from {})", repr_str(kind), allowed.iter().map(|kind| repr_str(kind)).collect::<Vec<_>>().join(", "))));
            }
            Ok(())
        }
        "update" => positional(0, Some(1), ""),
        "plan new" => {
            positional(0, Some(0), "")?;
            need(&["title", "goal"])
        }
        "plan set" => positional(1, Some(1), "id"),
        "plan close" => {
            positional(1, Some(1), "id")?;
            need(&["outcome"])
        }
        "item add" => {
            positional(1, Some(1), "plan")?;
            need(&["title"])
        }
        "item done" => {
            positional(1, Some(1), "id")?;
            need(&["commit"])
        }
        "item drop" => {
            positional(1, Some(1), "id")?;
            need(&["reason", "decided-by"])
        }
        "item supersede" => {
            positional(1, Some(1), "id")?;
            need(&["with"])
        }
        "item defer" | "item reopen" => {
            positional(1, Some(1), "id")?;
            need(&["reason"])
        }
        _ => Ok(()),
    }
}

// -- setting fields from the command line -------------------------------------------

/// `key=value`, `key+=value` (append to a list), `key:=<json>`, `key=` (remove).
/// `refs.files+=path` reaches into refs; `deps+=plan#2:soft` makes a dependency.
pub fn parse_sets(items: &[String], base: Option<&Value>) -> Result<Map<String, Value>> {
    let empty = Value::Object(Map::new());
    let base = base.unwrap_or(&empty);
    let mut patch = Map::new();
    for item in items {
        let colon = item.find(":=");
        let equals = item.find('=');
        if let Some(at) = colon.filter(|at| *at < equals.unwrap_or(item.len())) {
            let (key, text) = (&item[..at], &item[at + 2..]);
            let value: Value = serde_json::from_str(text).map_err(|error| LoreError::new(format!("`-s {item}`: not JSON ({})", json_reason(&error))))?;
            put(&mut patch, base, key, value, false)?;
        } else if let Some(at) = item.find("+=") {
            let (key, text) = (&item[..at], &item[at + 2..]);
            let value = if key == "deps" { dep_of(text)? } else { Value::String(text.to_owned()) };
            put(&mut patch, base, key, value, true)?;
        } else if let Some(at) = equals {
            let (key, text) = (&item[..at], &item[at + 1..]);
            let value = if text.is_empty() { Value::Null } else { Value::String(text.to_owned()) };
            put(&mut patch, base, key, value, false)?;
        } else {
            return Err(LoreError::new(format!("`-s {item}`: expected key=value, key+=value or key:=json")));
        }
    }
    Ok(patch)
}

fn put(patch: &mut Map<String, Value>, base: &Value, key: &str, value: Value, append: bool) -> Result<()> {
    if let Some(sub) = key.strip_prefix("refs.") {
        let refs = patch.entry("refs").or_insert_with(|| base.get("refs").filter(|refs| refs.is_object()).cloned().unwrap_or_else(|| Value::Object(Map::new())));
        let Some(refs) = refs.as_object_mut() else { return Ok(()) };
        if append {
            let mut list = refs.get(sub).and_then(Value::as_array).cloned().unwrap_or_default();
            list.push(value);
            refs.insert(sub.to_owned(), Value::Array(list));
        } else if value.is_null() {
            refs.remove(sub);
        } else {
            let list = if value.is_array() { value } else { Value::Array(vec![value]) };
            refs.insert(sub.to_owned(), list);
        }
        return Ok(());
    }
    if append {
        let current = patch.get(key).or_else(|| base.get(key)).cloned().unwrap_or(Value::Null);
        if current.is_string() && !current.as_str().unwrap_or_default().is_empty() {
            return Err(LoreError::new(format!("`-s {key}+=...`: `{key}` is text, and += appends to a list - give the whole new text with `-s {key}=...`")));
        }
        let mut list = current.as_array().cloned().unwrap_or_default();
        list.push(value);
        patch.insert(key.to_owned(), Value::Array(list));
    } else {
        patch.insert(key.to_owned(), value);
    }
    Ok(())
}

pub fn dep_of(text: &str) -> Result<Value> {
    let (item, kind) = text.split_once(':').unwrap_or((text, ""));
    let kind = if kind.is_empty() { "hard" } else { kind };
    if kind != "hard" && kind != "soft" {
        return Err(LoreError::new(format!("dependency {}: the kind after `:` is hard or soft", repr_str(text))));
    }
    Ok(json!({"item": item, "kind": kind}))
}

fn json_reason(error: &serde_json::Error) -> String {
    let text = error.to_string();
    text.rfind(" at line ").map(|at| text[..at].to_owned()).unwrap_or(text)
}

// -- stdin ----------------------------------------------------------------------------

/// stdin as UTF-8, whatever the console's code page.
fn stdin_text(env: &Env) -> Result<String> {
    let data = env.stdin.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&env.stdin);
    String::from_utf8(data.to_vec()).map_err(|error| LoreError::new(format!("stdin is not UTF-8 ({error})")))
}

fn read_stdin_json(env: &Env) -> Result<Value> {
    let text = stdin_text(env)?;
    if text.trim().is_empty() {
        return Err(LoreError::new("expected JSON on stdin and got nothing"));
    }
    serde_json::from_str(&text).map_err(|error| LoreError::new(format!("stdin is not JSON ({} at line {})", json_reason(&error), error.line())))
}

fn read_stdin_jsonl(env: &Env) -> Result<Vec<Value>> {
    let mut records = Vec::new();
    for (index, line) in stdin_text(env)?.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|error| LoreError::new(format!("stdin line {} is not JSON ({})", index + 1, json_reason(&error))))?;
        if !value.is_object() {
            return Err(LoreError::new(format!("stdin line {} is not a JSON object", index + 1)));
        }
        records.push(value);
    }
    Ok(records)
}

// -- the record as text ---------------------------------------------------------------

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => crate::py::repr(other),
    }
}

fn field<'a>(record: &'a Value, key: &str) -> &'a str {
    record.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn shown_field(record: &Value, key: &str) -> String {
    record.get(key).map(text_of).unwrap_or_else(|| "None".into())
}

pub fn fmt_record(record: &Value, score: Option<f64>, via: &[String]) -> String {
    let kind = shown_field(record, "kind");
    let mut head = format!("{kind} {}  [{}]", shown_field(record, "id"), shown_field(record, "status"));
    if kind != "plan" && kind != "item" {
        head.push_str(&format!("  area: {}", shown_field(record, "area")));
    }
    if let Some(score) = score {
        head.push_str(&format!("  score {}", crate::py::float_str(score)));
        if !via.is_empty() {
            head.push_str(&format!(" ({})", via.join("+")));
        }
    }
    let mut lines = vec![head, crate::py::indent(&fill(field(record, "title"), WIDTH - 2, "", ""), "  ")];
    for key in schema::shown(&kind) {
        let Some(value) = record.get(key).filter(|value| !value.is_null() && value.as_str() != Some("") && value.as_array().is_none_or(|items| !items.is_empty())) else { continue };
        let shown = match key {
            "deps" => value
                .as_array()
                .into_iter()
                .flatten()
                .map(|dep| format!("{}{}", field(dep, "item"), if field(dep, "kind") == "hard" { "" } else { " (soft)" }))
                .collect::<Vec<_>>()
                .join(", "),
            "signals" => value.as_array().into_iter().flatten().map(text_of).collect::<Vec<_>>().join("  |  "),
            "steps" => {
                lines.push("  steps:".into());
                for (n, step) in value.as_array().into_iter().flatten().enumerate() {
                    lines.push(fill(&format!("{}. {}", n + 1, text_of(step)), WIDTH, "    ", "       "));
                }
                continue;
            }
            "parts" => {
                let entries: Vec<&Value> = value.as_array().into_iter().flatten().collect();
                lines.push(format!("  parts: {}", entries.len()));
                for entry in entries.into_iter().filter(|entry| entry.is_object()) {
                    let records = entry.get("records").and_then(Value::as_array).map(|items| items.iter().map(text_of).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                    let skip = entry.get("skip").and_then(Value::as_str).filter(|skip| !skip.is_empty()).map(|skip| format!("skip: {skip}")).unwrap_or_default();
                    let said: Vec<String> = [records, skip].into_iter().filter(|text| !text.is_empty()).collect();
                    lines.push(fill(&format!("{} -> {}", shown_field(entry, "part"), said.join("; ")), WIDTH, "    ", "        "));
                }
                continue;
            }
            _ => match value {
                Value::Array(items) => items.iter().map(text_of).collect::<Vec<_>>().join("; "),
                other => text_of(other),
            },
        };
        lines.push(fill(&format!("{key}: {shown}"), WIDTH, "  ", "      "));
    }
    if let Some(refs) = record.get("refs").and_then(Value::as_object).filter(|refs| !refs.is_empty()) {
        let parts: Vec<String> = refs
            .iter()
            .filter(|(_, value)| !crate::ops::falsy(value))
            .map(|(key, value)| format!("{key} {}", value.as_array().map(|items| items.iter().map(text_of).collect::<Vec<_>>().join(", ")).unwrap_or_else(|| text_of(value))))
            .collect();
        lines.push(fill(&format!("refs: {}", parts.join("; ")), WIDTH, "  ", "      "));
    }
    lines.push(format!("  ({} by {})", shown_field(record, "updated"), shown_field(record, "by")));
    lines.join("\n")
}

// -- the context ----------------------------------------------------------------------

struct Ctx<'a> {
    args: Args,
    config: Config,
    env: &'a Env<'a>,
    records: Option<Vec<Value>>,
}

impl<'a> Ctx<'a> {
    fn new(args: Args, env: &'a Env<'a>) -> Result<Self> {
        let config = config::load(&start(&args, env))?;
        check_limits(&config.limits)?;
        Ok(Self { args, config, env, records: None })
    }

    /// The store's records as the working tree has them.
    fn records(&mut self) -> Result<Vec<Value>> {
        if self.records.is_none() {
            self.records = Some(store::load_worktree(&self.config.root, &self.config.dir)?.all());
        }
        Ok(self.records.clone().unwrap_or_default())
    }

    /// A record by id, live: the working tree, and when it does not have it,
    /// the branch the next write builds on - another session's commit that has
    /// not reached this working tree yet.
    fn record(&mut self, rid: &str) -> Result<(Option<Value>, Option<String>)> {
        if let Some(record) = self.records()?.into_iter().find(|record| record.get("id").and_then(Value::as_str) == Some(rid)) {
            return Ok((Some(record), None));
        }
        let rev = if self.config.commit_mode == "shared-tree" {
            gitio::reference(&self.config.root, &format!("refs/heads/{}", self.config.branch))
        } else {
            gitio::head(&self.config.root)
        };
        let Some(rev) = rev else { return Ok((None, None)) };
        let record = ops::load_rev(&self.config, Some(&rev))?.by_id().remove(rid);
        let at = record.as_ref().map(|_| rev);
        Ok((record, at))
    }

    fn by(&self) -> String {
        let by = self.args.by.clone().or_else(|| self.env.lore_by.clone()).filter(|by| !by.is_empty());
        let by = by.unwrap_or_else(|| {
            let name = gitio::user_name(&self.config.root);
            if name.is_empty() { "unknown".into() } else { name }
        });
        by.trim().to_owned()
    }

    fn write(&self, io: &mut Io, build: &ops::Builder, subject: Option<String>, notes: Vec<String>) -> Result<i32> {
        let by = self.by();
        let options = Options {
            by: by.clone(),
            subject,
            trailers: self.args.trailers.clone(),
            mode: self.args.mode.clone(),
            roots: self.env.roots.clone(),
            identity: private::identity(&self.config.root, &by),
        };
        let mut outcome = ops::apply(&self.config, build, &options)?;
        // Task 149: a fault written after a find is a miss; the records it
        // cited are the find's hit position. Ids and kinds only.
        crate::stats::record_write(&self.config, &outcome.records);
        let mut all = notes;
        all.append(&mut outcome.notes);
        outcome.notes = all;
        let place = format!("{}/{}", self.config.remote, self.config.branch);
        let line = outcome.commit.as_ref().map(|commit| {
            let short: String = commit.chars().take(10).collect();
            match outcome.pushed {
                None => format!("committed {short}"),
                Some(true) => format!("committed {short}, pushed to {place}"),
                Some(false) => format!("committed {short} locally, push to {place} failed: {}; the next write's push carries it", outcome.push_error.clone().unwrap_or_else(|| "None".into())),
            }
        });
        if self.args.json {
            io.json(&shape_write(&outcome, line.as_deref()));
            return Ok(0);
        }
        if let Some(line) = &line {
            io.say(line);
        }
        for record in &outcome.records {
            io.say(&format!("  {} {}  [{}]  {}", shown_field(record, "kind"), shown_field(record, "id"), shown_field(record, "status"), field(record, "title")));
        }
        for note in &outcome.notes {
            io.note(note);
        }
        Ok(0)
    }
}

fn start(args: &Args, env: &Env) -> PathBuf {
    match &args.c {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() { path } else { env.cwd.join(path) }
        }
        None => env.cwd.clone(),
    }
}

/// Runs one `lore` command line.
pub fn run(argv: &[&str], env: &Env) -> Output {
    let argv: Vec<String> = argv.iter().map(|word| (*word).to_owned()).collect();
    let mut io = Io { out: String::new(), err: String::new() };
    let args = match parse_args(&argv).and_then(|args| check_shape(&args).map(|_| args)) {
        Ok(args) => args,
        Err(Usage(message)) => {
            io.err.push_str(&format!("usage: lore [options] <command> ...\nlore: error: {message}\n"));
            return Output { code: 2, stdout: io.out, stderr: io.err };
        }
    };
    if args.cmd() == "--version" {
        io.say(&format!("lore {VERSION}"));
        return Output { code: 0, stdout: io.out, stderr: io.err };
    }
    let json = args.json;
    let result = dispatch(args, env, &mut io);
    let code = match result {
        Ok(code) => code,
        Err(error) => {
            if json {
                io.json(&shape_error(&error));
            } else {
                io.err.push_str(&format!("lore: {error}\n"));
            }
            1
        }
    };
    Output { code, stdout: io.out, stderr: io.err }
}

fn dispatch(args: Args, env: &Env, io: &mut Io) -> Result<i32> {
    match args.cmd().as_str() {
        "init" => return cmd_init(&args, env, io),
        "guide" => {
            io.say(GUIDE.trim_end());
            return Ok(0);
        }
        "check" => return cmd_check(&args, env, io),
        "fmt" => return cmd_fmt(&args, env, io),
        _ => {}
    }
    let mut ctx = Ctx::new(args, env)?;
    match ctx.args.cmd().as_str() {
        "areas" => cmd_areas(&mut ctx, io),
        "find" => cmd_find(&mut ctx, io),
        "for" => cmd_for(&mut ctx, io),
        "show" | "plan show" => cmd_show(&mut ctx, io),
        "exists" => cmd_exists(&mut ctx, io),
        "add" => cmd_add(&mut ctx, io),
        "update" => cmd_update(&mut ctx, io),
        "cover" => cmd_cover(&mut ctx, io),
        "index" => cmd_index(&mut ctx, io),
        "stats" => cmd_stats(&mut ctx, io),
        "plans" => cmd_plans(&mut ctx, io),
        "items" => cmd_items(&mut ctx, io),
        "stale" => cmd_stale(&mut ctx, io),
        "plan new" => cmd_plan_new(&ctx, io),
        "plan set" => cmd_plan_set(&ctx, io),
        "plan close" => {
            let patch = json!({"status": "closed", "outcome": ctx.args.one("outcome")});
            let build = ops::updater(ctx.args.positional[0].clone(), Patch::Fixed(patch.as_object().cloned().unwrap_or_default()), None);
            ctx.write(io, &build, None, Vec::new())
        }
        "item add" => cmd_item_add(&ctx, io),
        "pulse add" | "pulses" | "message send" | "messages" | "message read" | "brief set" | "brief show" | "promote" | "supersede" | "working status" | "working cleanup" => {
            working_cli::run(&mut ctx, io)
        }
        "item done" => {
            let fields = vec![("commits", json!(ctx.args.many("commit"))), ("outcome", json!(ctx.args.one("outcome")))];
            item_move(&ctx, io, "done", fields, None)
        }
        "item drop" => {
            let fields = vec![("reason", json!(ctx.args.one("reason"))), ("decided_by", json!(ctx.args.one("decided-by"))), ("outcome", json!(ctx.args.one("outcome")))];
            item_move(&ctx, io, "dropped", fields, None)
        }
        "item supersede" => {
            let fields = vec![("superseded_by", json!(ctx.args.one("with"))), ("outcome", json!(ctx.args.one("outcome")))];
            item_move(&ctx, io, "superseded", fields, None)
        }
        "item defer" => item_move(&ctx, io, "deferred", vec![("reason", json!(ctx.args.one("reason")))], None),
        "item reopen" => {
            let reason = ctx.args.one("reason");
            item_move(&ctx, io, "open", Vec::new(), reason)
        }
        other => Err(LoreError::new(format!("unknown command {other}"))),
    }
}

// -- the commands ---------------------------------------------------------------------

fn cmd_init(args: &Args, env: &Env, io: &mut Io) -> Result<i32> {
    let begin = start(args, env);
    let root = gitio::toplevel(&begin).ok_or_else(|| LoreError::new(format!("{} is not inside a git repository", begin.display())))?;
    let path = root.join(config::CONFIG_NAME);
    if path.exists() {
        return Err(LoreError::new(format!("{} exists already", path.display())));
    }
    let areas_text = args.one("areas").unwrap_or_else(|| "general".into());
    let areas: Vec<String> = areas_text.split(',').map(str::trim).filter(|area| !area.is_empty()).map(str::to_owned).collect();
    let mode = args.one("commit-mode").unwrap_or_else(|| "plain".into());
    if !config::MODES.contains(&mode.as_str()) {
        return Err(LoreError::new(format!("argument --commit-mode: invalid choice: {}", repr_str(&mode))));
    }
    let branch = args.one("branch").or_else(|| gitio::current_branch(&root)).unwrap_or_else(|| "main".into());
    let text = config::template(&args.one("dir").unwrap_or_else(|| "lore".into()), &areas, &mode, &branch);
    let data: toml::Value = toml::from_str(&text).map_err(|error| LoreError::new(error.message().to_owned()))?;
    let config = config::parse(&data, &root, config::CONFIG_NAME)?;
    std::fs::write(&path, &text).map_err(|error| LoreError::new(format!("{}: {error}", path.display())))?;
    std::fs::create_dir_all(root.join(&config.dir).join("plans")).map_err(|error| LoreError::new(error.to_string()))?;
    let attributes = root.join(".gitattributes");
    let line = format!("{}/** text eol=lf", config.dir);
    let current = std::fs::read_to_string(&attributes).unwrap_or_default();
    if !current.lines().any(|existing| existing == line) {
        let joiner = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&attributes, format!("{current}{joiner}{line}\n")).map_err(|error| LoreError::new(error.to_string()))?;
    }
    io.say(&format!("wrote {} and the `{line}` line in .gitattributes; commit them.", config::CONFIG_NAME));
    io.say("Add this line to AGENTS.md (or CLAUDE.md):");
    io.say("");
    io.say("    Project knowledge and plans are in CodeLore: run `lore guide` once per session.");
    Ok(0)
}

fn cmd_areas(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let mut counts: Map<String, Value> = ctx.config.areas.iter().map(|area| (area.clone(), json!(0))).collect();
    for record in ctx.records()? {
        if let Some(area) = record.get("area").and_then(Value::as_str) {
            if let Some(count) = counts.get_mut(area) {
                *count = json!(count.as_u64().unwrap_or(0) + 1);
            }
        }
    }
    if ctx.args.json {
        io.json(&Value::Object(counts));
    } else {
        for (area, count) in &counts {
            io.say(&format!("{area:20} {count}"));
        }
    }
    Ok(0)
}

fn filters(args: &Args) -> Filters {
    Filters { kind: args.one("kind"), area: args.one("area"), status: args.one("status"), plan: args.one("plan"), all: args.flag("all"), sources: false }
}

fn cmd_find(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let mut query = ctx.args.positional.join(" ");
    if query.trim() == "-" {
        query = search::errors_of(&stdin_text(ctx.env)?);
    }
    if query.trim().is_empty() {
        return Err(LoreError::new("nothing to search for"));
    }
    if let Some(kind) = ctx.args.one("kind") {
        if schema::kind(&kind).is_none() {
            return Err(LoreError::new(format!("argument --kind: invalid choice: {}", repr_str(&kind))));
        }
    }
    let limit: usize = match ctx.args.one("n") {
        Some(n) => n.parse().map_err(|_| LoreError::new(format!("argument -n: invalid int value: {}", repr_str(&n))))?,
        None => 5,
    };
    let finder = ctx.env.finder.ok_or_else(|| LoreError::new("`lore find` needs the search index, and this build has none"))?;
    if let Some(note) = finder.place_note(&ctx.config) {
        io.progress(&format!("note: {note}"));
    }
    let records = ctx.records()?;
    let started = std::time::Instant::now();
    let (hits, notes) = finder.find(&ctx.config, &records, &query, &filters(&ctx.args), limit, ctx.args.text_only)?;
    // Task 149: counts only. Never the query or a record's text.
    crate::stats::record_find(&ctx.config, &query, &hits, started.elapsed(), &crate::stats::tiers(&hits));
    // docs/lore.md 11.6: without working entries and without a history
    // source, `find` is CodeLore's byte for byte.
    if let Some(merged) = working_cli::tiers(ctx, &query, &hits, &notes, limit)? {
        if ctx.args.json {
            io.json(&merged.json);
        } else {
            for note in &merged.notes {
                io.note(note);
            }
            if merged.text.is_empty() {
                io.say("nothing found");
            } else {
                io.say(&merged.text.join("\n\n"));
            }
        }
        return Ok(0);
    }
    let at = ctx.env.now.unwrap_or_else(|| chrono::Utc::now().timestamp());
    if ctx.args.json {
        io.json(&shape_find_at(&query, &notes, &hits, at));
        return Ok(0);
    }
    for note in &notes {
        io.note(note);
    }
    if hits.is_empty() {
        io.say("nothing found");
        return Ok(0);
    }
    io.say(&hits.iter().map(|hit| format!("{}\n{}", fmt_record(&hit.record, Some(hit.score), &hit.via), confidence_line(&durable_result(hit, at)["confidence"]))).collect::<Vec<_>>().join("\n\n"));
    Ok(0)
}

fn cmd_for(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let target = ctx.args.positional[0].clone();
    let records = ctx.records()?;
    let found = search::for_target(&ctx.config, &records, &target, &Filters { all: ctx.args.flag("all"), ..Filters::default() });
    if ctx.args.json {
        io.json(&Value::Array(found));
    } else if found.is_empty() {
        io.say(&format!("nothing recorded about {target}"));
    } else {
        io.say(&found.iter().map(|record| fmt_record(record, None, &[])).collect::<Vec<_>>().join("\n\n"));
    }
    Ok(0)
}

fn cmd_show(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let id = ctx.args.positional[0].clone();
    let (record, rev) = ctx.record(&id)?;
    let record = record.ok_or_else(|| LoreError::new(format!("{} does not exist", repr_str(&id))))?;
    crate::stats::record_open(&ctx.config, &id);
    if ctx.args.json {
        io.json(&record);
        return Ok(0);
    }
    if let Some(rev) = rev {
        io.note(&format!("not in the working tree yet; shown as committed at {}", rev.chars().take(10).collect::<String>()));
    }
    io.say(&fmt_record(&record, None, &[]));
    if field(&record, "kind") == "plan" {
        io.say("");
        let records = ctx.records()?;
        show_items(io, &records, &plans::items_of(&records, &id));
    }
    Ok(0)
}

fn show_items(io: &mut Io, records: &[Value], items: &[Value]) {
    let by_id: BTreeMap<String, Value> = records.iter().filter(|record| field(record, "kind") == "item").map(|record| (field(record, "id").to_owned(), record.clone())).collect();
    for (n, layer) in plans::layers(items).iter().enumerate() {
        io.say(&format!("layer {n}"));
        for item in layer {
            let mark = if plans::ready(item, &by_id) { "*" } else { " " };
            let deps: Vec<String> = item.get("deps").and_then(Value::as_array).into_iter().flatten().map(|dep| format!("{}{}", field(dep, "item"), if field(dep, "kind") == "soft" { "~" } else { "" })).collect();
            let tail = if deps.is_empty() { String::new() } else { format!("  <- {}", deps.join(", ")) };
            io.say(&format!("  {mark} {:24} [{}] {}{tail}", field(item, "id"), field(item, "status"), field(item, "title")));
        }
    }
    if !items.is_empty() {
        io.say("(* ready; ~ a soft dependency)");
    }
}

fn cmd_exists(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let mut result = Map::new();
    for id in ctx.args.positional.clone() {
        let (record, _) = ctx.record(&id)?;
        let value = match record {
            None => Value::Null,
            Some(record) => json!({"kind": record.get("kind"), "status": record.get("status"), "title": record.get("title").cloned().unwrap_or(json!(""))}),
        };
        result.insert(id, value);
    }
    if ctx.args.json {
        io.json(&Value::Object(result.clone()));
    } else {
        for (id, value) in &result {
            if value.is_null() {
                io.say(&format!("{id}  MISSING"));
            } else {
                io.say(&format!("{id}  {} [{}]  {}", shown_field(value, "kind"), shown_field(value, "status"), field(value, "title")));
            }
        }
    }
    Ok(if result.values().all(|value| !value.is_null()) { 0 } else { 1 })
}

fn new_from_args(ctx: &Ctx, kind: &str) -> Result<Vec<Value>> {
    let mut records: Vec<Value> = if ctx.args.flag("batch") {
        read_stdin_jsonl(ctx.env)?
    } else if ctx.args.flag("json-in") {
        match read_stdin_json(ctx.env)? {
            Value::Array(items) => items,
            other => vec![other],
        }
    } else {
        let mut record = Map::new();
        record.insert("kind".into(), json!(kind));
        record.extend(parse_sets(&ctx.args.many("set"), None)?);
        vec![Value::Object(record)]
    };
    for record in &mut records {
        let Some(map) = record.as_object_mut() else { return Err(LoreError::new("stdin must hold a JSON object or a list of objects")) };
        map.entry("kind").or_insert(json!(kind));
        let given = map.get("kind").and_then(Value::as_str).unwrap_or_default().to_owned();
        if given != kind && kind != "any" {
            return Err(LoreError::new(format!("a {} record given to `lore add {kind}`", shown_field(record, "kind"))));
        }
        if let Some(spec) = schema::kind(&given) {
            if let Some(map) = record.as_object_mut() {
                map.entry("status").or_insert(json!(spec.statuses[0]));
            }
        }
    }
    Ok(records)
}

fn cmd_add(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let kind = ctx.args.positional[0].clone();
    let mut records = new_from_args(ctx, &kind)?;
    // Task 150: a fault whose signals are only exact strings gets a warning,
    // never a refusal.
    for record in &records {
        for warning in schema::warnings(record) { io.progress(&format!("warn: {warning}")); }
    }
    if ctx.args.flag("occurrence") {
        return working_cli::add_resolved(ctx, io, records);
    }
    let existing = ctx.records()?;
    let mut found = dedupe::candidates(&ctx.config, &existing, &records, None);
    let mut blocking: BTreeMap<usize, Vec<dedupe::Match>> = found.iter().filter(|(_, matches)| matches.iter().any(|m| m.blocking)).map(|(at, matches)| (*at, matches.clone())).collect();
    let count = records.len();
    let named = |records: &[Value], at: usize| -> String {
        let record = &records[at];
        let number = if count > 1 { format!("#{} ", at + 1) } else { String::new() };
        let id = record.get("id").filter(|id| !crate::ops::falsy(id)).map(|id| format!("{} ", text_of(id))).unwrap_or_default();
        format!("{number}{id}{}", record.get("title").map(crate::py::repr).unwrap_or_else(|| "'?'".into()))
    };
    let matches = |list: &[dedupe::Match]| -> String {
        list.iter().filter(|m| m.blocking).map(|m| format!("{} ({} {})", m.id, m.how, crate::py::float_str(m.similarity))).collect::<Vec<_>>().join("; ")
    };
    let mut skipped = Vec::new();
    if !blocking.is_empty() && !ctx.args.flag("new") {
        if !ctx.args.flag("skip-duplicates") {
            let lines: Vec<String> = blocking.iter().map(|(at, list)| format!("{} is: {}", named(&records, *at), matches(list))).collect();
            let rest = count - blocking.len();
            let mut how = "improve the existing record with `lore update <id>`, or repeat with --new if yours is a different one".to_owned();
            if rest > 0 {
                how.push_str(&format!("; --skip-duplicates writes the other {rest} and leaves these out"));
            }
            return Err(LoreError::with(format!("refused, nothing was written: {} of {count} record(s) already exist - {how}", blocking.len()), lines));
        }
        skipped = blocking.iter().map(|(at, list)| format!("left out {}: it is {}", named(&records, *at), matches(list))).collect();
        let keep: Vec<usize> = (0..count).filter(|at| !blocking.contains_key(at)).collect();
        if keep.is_empty() {
            return Err(LoreError::with("nothing to write: every record is a duplicate", skipped));
        }
        found = found.into_iter().filter(|(at, _)| keep.contains(at)).map(|(at, list)| (keep.iter().position(|kept| *kept == at).unwrap_or(0), list)).collect();
        records = keep.iter().map(|at| records[*at].clone()).collect();
        blocking.clear();
    }
    let near: Vec<String> = found
        .iter()
        .filter(|(at, _)| !blocking.contains_key(at))
        .map(|(at, list)| {
            let title = records[*at].get("title").map(crate::py::repr).unwrap_or_else(|| "'?'".into());
            let names: Vec<String> = list.iter().map(|m| format!("{} ({} {})", m.id, m.how, crate::py::float_str(m.similarity))).collect();
            format!("{title} is near {} - if one says the same, merge into it with `lore update`", names.join(", "))
        })
        .collect();
    let mut notes = skipped;
    notes.extend(near);
    let build = ops::adder(records);
    ctx.write(io, &build, None, notes)
}

fn cmd_update(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let id = ctx.args.positional.first().cloned();
    let sets = ctx.args.many("set");
    if ctx.args.flag("batch") {
        if id.is_some() || !sets.is_empty() {
            return Err(LoreError::new("--batch reads every patch from stdin: give no id and no -s"));
        }
        let patches = read_stdin_jsonl(ctx.env)?;
        let mut maps = Vec::new();
        for (index, patch) in patches.into_iter().enumerate() {
            if !patch.get("id").is_some_and(Value::is_string) {
                return Err(LoreError::new(format!("stdin line {}: a batch patch names its record by `id`", index + 1)));
            }
            maps.push(patch.as_object().cloned().unwrap_or_default());
        }
        let build = ops::updater_many(maps);
        return ctx.write(io, &build, None, Vec::new());
    }
    if ctx.args.flag("json-in") {
        if !sets.is_empty() {
            return Err(LoreError::new("--json-in reads the patch from stdin: give no -s"));
        }
        let patch = read_stdin_json(ctx.env)?;
        if patch.is_array() {
            return Err(LoreError::new("stdin holds a list; several patches go through `lore update --batch` (JSONL)"));
        }
        let Some(map) = patch.as_object().cloned() else { return Err(LoreError::new("the patch on stdin must be a JSON object")) };
        let rid = id.clone().or_else(|| map.get("id").and_then(Value::as_str).map(str::to_owned)).filter(|rid| !rid.is_empty());
        let Some(rid) = rid else { return Err(LoreError::new("which record? give `lore update <id> --json-in`, or `id` in the patch")) };
        if let Some(named) = &id {
            if let Some(other) = map.get("id").filter(|other| !other.is_null() && other.as_str() != Some(named.as_str())) {
                return Err(LoreError::new(format!("the command names {} and the patch names {}", repr_str(named), crate::py::repr(other))));
            }
        }
        let build = ops::updater(rid, Patch::Fixed(map), None);
        return ctx.write(io, &build, None, Vec::new());
    }
    let Some(rid) = id else { return Err(LoreError::new("which record? `lore update <id>`, or --json-in with `id` in the patch, or --batch")) };
    let build = ops::updater(rid, Patch::Of(Box::new(move |current: &Value| parse_sets(&sets, Some(current)))), None);
    ctx.write(io, &build, None, Vec::new())
}

fn cmd_check(args: &Args, env: &Env, io: &mut Io) -> Result<i32> {
    let config = config::load(&start(args, env))?;
    let mut errs: Vec<String> = Vec::new();
    let mut warns: Vec<String> = Vec::new();
    if let Err(error) = check_limits(&config.limits) {
        errs.extend(error.details);
    }
    let mut files: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let lore_dir = config.lore_dir();
    if lore_dir.is_dir() {
        for path in store::lore_files(&config.root, &config.dir) {
            let rel = store::relative(&config.root, &path);
            let bytes = std::fs::read(&path).unwrap_or_default();
            match store::parse(&bytes, &rel) {
                Ok(records) => {
                    files.insert(rel, records);
                }
                Err(error) => {
                    errs.push(error.message.clone());
                    errs.extend(error.details.iter().map(|detail| format!("  {detail}")));
                }
            }
        }
        for path in walk(&lore_dir) {
            let rel = store::relative(&config.root, &path);
            if !is_lore_file(&rel, &config.dir) {
                warns.push(format!("{rel} is not a lore file and is ignored"));
            }
        }
    }
    let snapshot = Snapshot { files };
    let count: usize = snapshot.files.values().map(Vec::len).sum();
    if !args.json {
        io.progress(&format!("checking {count} record(s) in {} file(s)...", snapshot.files.len()));
    }
    errs.extend(ops::check_all(&config, &snapshot));
    let head = gitio::head(&config.root);
    let records: Vec<Value> = snapshot.all();
    let commits: Vec<String> = records.iter().flat_map(ops::commits_of).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let live: Vec<&Value> = records.iter().filter(|record| !["obsolete", "superseded", "reversed"].contains(&field(record, "status"))).collect();
    let live_files: Vec<String> = live
        .iter()
        .flat_map(|record| record.get("refs").and_then(|refs| refs.get("files")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(|path| path.trim_end_matches('/').to_owned()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let roots = env.roots.as_ref();
    let found = crate::refs::resolve(&config.root, head.as_deref(), roots, &commits, &live_files);
    for record in &records {
        for commit in ops::commits_of(record) {
            if !found.commits.get(&commit).copied().unwrap_or(false) {
                errs.push(crate::refs::missing_commit(&shown_field(record, "id"), &commit, roots));
            }
        }
    }
    if head.is_some() || roots.is_some() {
        for record in &live {
            for path in record.get("refs").and_then(|refs| refs.get("files")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                if !found.files.get(path.trim_end_matches('/')).copied().unwrap_or(false) {
                    warns.push(format!("{}: file {} is gone from HEAD - is the record obsolete?", shown_field(record, "id"), repr_str(path)));
                }
            }
        }
    }
    for record in &records {
        if field(record, "kind") == "source" && field(record, "status") == "covered" {
            let Some(path) = record.get("path").and_then(Value::as_str) else { continue };
            if let Some(data) = cover::read_doc(&config, path, None) {
                let ledger: Vec<Value> = record.get("parts").and_then(Value::as_array).cloned().unwrap_or_default();
                let found = cover::coverage(&cover::parts_of(&cover::doc_text(&data)), &ledger);
                if !found.uncovered.is_empty() || !found.unknown.is_empty() {
                    warns.push(format!(
                        "{}: {path} changed since it was covered - {} part(s) unaccounted for, {} entry(ies) naming no part; `lore cover {path}`",
                        field(record, "id"),
                        found.uncovered.len(),
                        found.unknown.len()
                    ));
                }
            }
        }
    }
    if args.flag("evidence") {
        errs.extend(evidence(&config, &records, &found.commits, roots));
    }
    if !args.json {
        io.progress("...done");
    }
    if args.json {
        io.json(&shape_check(count, snapshot.files.len(), &errs, &warns));
    } else {
        for warning in &warns {
            io.say(&format!("warn: {warning}"));
        }
        for error in &errs {
            io.say(&format!("FAIL: {error}"));
        }
        let verdict = if errs.is_empty() { "ok".to_owned() } else { format!("{} failure(s)", errs.len()) };
        io.say(&format!("{count} record(s) in {} file(s): {verdict}", snapshot.files.len()));
    }
    Ok(if errs.is_empty() { 0 } else { 1 })
}

/// Every file under a folder, sorted.
fn walk(folder: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// A cited commit is evidence when it touches a file the record cites, or when
/// its hash is written in a source document whose ledger names the record.
fn evidence(config: &Config, records: &[Value], have: &BTreeMap<String, bool>, roots: Option<&Roots>) -> Vec<String> {
    let mut sources: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for record in records {
        if field(record, "kind") == "source" {
            if let Some(path) = record.get("path").and_then(Value::as_str) {
                let mut targets: std::collections::BTreeSet<String> = record.get("records").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
                for entry in record.get("parts").and_then(Value::as_array).into_iter().flatten() {
                    targets.extend(entry.get("records").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
                }
                for target in targets {
                    sources.entry(target).or_default().push(path.to_owned());
                }
            }
        }
    }
    let mut texts: BTreeMap<String, String> = BTreeMap::new();
    let mut doc = |path: &str| -> String {
        texts.entry(path.to_owned()).or_insert_with(|| cover::read_doc(config, path, None).map(|data| cover::doc_text(&data).to_lowercase()).unwrap_or_default()).clone()
    };
    let mut cited: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
    for record in records {
        let id = field(record, "id").to_owned();
        let files: Vec<String> = record.get("refs").and_then(|refs| refs.get("files")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(|path| path.trim_end_matches('/').to_owned()).collect();
        let mut commits: std::collections::BTreeSet<String> = record.get("refs").and_then(|refs| refs.get("commits")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        commits.extend(record.get("commits").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
        let commits: Vec<String> = commits.into_iter().filter(|commit| have.get(commit).copied().unwrap_or(false)).collect();
        if !commits.is_empty() && (!files.is_empty() || sources.contains_key(&id)) {
            cited.insert(id, (files, commits));
        }
    }
    let all: Vec<String> = cited.values().flat_map(|(_, commits)| commits.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let touched = crate::refs::files_touched(&config.root, roots, &all);
    let mut errs = Vec::new();
    for (id, (files, commits)) in &cited {
        for commit in commits {
            let paths = touched.get(commit).cloned().unwrap_or_default();
            if paths.iter().any(|path| files.iter().any(|file| path == file || path.starts_with(&format!("{file}/")))) {
                continue;
            }
            let short: String = commit.to_lowercase().chars().take(7).collect();
            if sources.get(id).into_iter().flatten().any(|source| doc(source).contains(&short)) {
                continue;
            }
            let place = if files.is_empty() { "touches no file the record cites".to_owned() } else { format!("touches none of {}", files.join(", ")) };
            let from = sources.get(id).map(|list| format!(", and no source of it ({}) mentions it", list.join(", "))).unwrap_or_default();
            errs.push(format!("{id}: commit {commit} {place}{from} - is it the right commit?"));
        }
    }
    errs
}

fn doc_path(ctx: &Ctx, given: &str) -> Result<String> {
    let root = config::simplify(&std::fs::canonicalize(&ctx.config.root).unwrap_or_else(|_| ctx.config.root.clone()));
    let path = PathBuf::from(given);
    let candidates = if path.is_absolute() { vec![path] } else { vec![ctx.env.cwd.join(&path), root.join(&path)] };
    for candidate in candidates {
        let resolved = std::fs::canonicalize(&candidate).map(|path| config::simplify(&path)).unwrap_or_else(|_| normalize(&candidate));
        if let Ok(inside) = resolved.strip_prefix(&root) {
            return Ok(inside.to_string_lossy().replace('\\', "/"));
        }
    }
    Err(LoreError::new(format!("{given} is not inside the repository")))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn source_of(records: &[Value], path: &str) -> Option<Value> {
    records.iter().find(|record| field(record, "kind") == "source" && field(record, "path") == path).cloned()
}

fn cmd_cover(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let path = doc_path(ctx, &ctx.args.positional[0].clone())?;
    if ctx.args.flag("add") || ctx.args.flag("history") {
        return cover_write(ctx, io, path);
    }
    let base = ops::base_snapshot(&ctx.config)?.all();
    let source = source_of(&base, &path);
    let data = cover::read_doc(&ctx.config, &path, None);
    let (parts, found) = match &data {
        None => {
            if source.is_none() {
                return Err(LoreError::new(format!("{path} is not in the working tree or HEAD, and no source record names it")));
            }
            (Vec::new(), None)
        }
        Some(data) => {
            let parts = cover::parts_of(&cover::doc_text(data));
            let ledger: Vec<Value> = source.as_ref().and_then(|source| source.get("parts")).and_then(Value::as_array).cloned().unwrap_or_default();
            let found = cover::coverage(&parts, &ledger);
            (parts, Some(found))
        }
    };
    let history = source.as_ref().is_some_and(|source| field(source, "status") == "history");
    let entries: BTreeMap<String, Value> = source
        .as_ref()
        .and_then(|source| source.get("parts"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("part").and_then(Value::as_str).map(|part| (schema::part_key(part), entry.clone())))
        .collect();
    let uncovered: Vec<cover::Part> = if history { Vec::new() } else { found.as_ref().map(|found| found.uncovered.clone()).unwrap_or_default() };
    let complete = history || (found.is_some() && uncovered.is_empty());
    let unknown: Vec<String> = found.as_ref().map(|found| found.unknown.clone()).unwrap_or_default();
    if ctx.args.json {
        io.json(&json!({
            "path": path,
            "source": source,
            "parts": parts.len(),
            "uncovered": uncovered.iter().map(|part| json!({"part": part.name, "kind": part.kind, "line": part.line})).collect::<Vec<_>>(),
            "unknown": unknown,
            "complete": complete,
        }));
        return Ok(if complete { 0 } else { 1 });
    }
    match &source {
        None => io.say(&format!("{path}: no source record yet; {} part(s), none accounted for", parts.len())),
        Some(source) if history => {
            io.say(&format!("{path}: history ({}) - {}", field(source, "id"), shown_field(source, "reason")));
            if let Some(records) = source.get("records").and_then(Value::as_array).filter(|records| !records.is_empty()) {
                io.say(&format!("  what still stands is in: {}", records.iter().map(text_of).collect::<Vec<_>>().join(", ")));
            }
        }
        Some(source) => io.say(&format!("{path}: {} [{}] - {} of {} part(s) accounted for", field(source, "id"), field(source, "status"), parts.len() - uncovered.len(), parts.len())),
    }
    if ctx.args.flag("list") {
        let lost: std::collections::BTreeSet<(usize, String)> = uncovered.iter().map(|part| (part.line, part.name.clone())).collect();
        for part in &parts {
            let what = match entries.get(&schema::part_key(&part.name)) {
                Some(entry) => {
                    let records = entry.get("records").and_then(Value::as_array).map(|items| items.iter().map(text_of).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                    let skip = entry.get("skip").and_then(Value::as_str).filter(|skip| !skip.is_empty()).map(|skip| format!("  skip: {skip}")).unwrap_or_default();
                    format!("  {records}{skip}")
                }
                None if lost.contains(&(part.line, part.name.clone())) => "  UNACCOUNTED".into(),
                None => String::new(),
            };
            io.say(&format!("  {:>5} {:7} {}{what}", part.line, part.kind, part.name));
        }
    } else if !uncovered.is_empty() {
        io.say("unaccounted for (name each in the ledger: `lore cover <doc> --add`):");
        for part in &uncovered {
            io.say(&format!("  {:>5} {:7} {}", part.line, part.kind, part.name));
        }
    }
    for name in &unknown {
        io.say(&format!("  ledger entry {} names no part of the document now", repr_str(name)));
    }
    Ok(if complete { 0 } else { 1 })
}

fn cover_write(ctx: &mut Ctx, io: &mut Io, path: String) -> Result<i32> {
    let entries = if ctx.args.flag("add") { read_stdin_jsonl(ctx.env)? } else { Vec::new() };
    for (index, entry) in entries.iter().enumerate() {
        if !entry.get("part").is_some_and(Value::is_string) {
            return Err(LoreError::new(format!("stdin line {}: a ledger entry is {{\"part\": ..., \"records\": [...], \"skip\": ...}}", index + 1)));
        }
    }
    let history = ctx.args.flag("history");
    let reason = ctx.args.one("reason");
    if history && reason.as_deref().is_none_or(str::is_empty) {
        return Err(LoreError::new("--history needs --reason: why nothing in the document is current"));
    }
    let title = ctx.args.one("title");
    let extra_records = ctx.args.many("record");
    let config = ctx.config.clone();
    let build = move |snapshot: &Snapshot| -> Result<Vec<Change>> {
        let records = snapshot.all();
        let current = source_of(&records, &path);
        let mut record = current.clone().unwrap_or_else(|| {
            let title: String = title.clone().unwrap_or_else(|| format!("source: {path}")).chars().take(120).collect();
            json!({"kind": "source", "area": "sources", "path": path, "title": title, "status": "open"})
        });
        let mut ledger: Vec<(String, Value)> = record.get("parts").and_then(Value::as_array).into_iter().flatten().filter_map(|entry| entry.get("part").and_then(Value::as_str).map(|part| (schema::part_key(part), entry.clone()))).collect();
        for entry in &entries {
            let key = schema::part_key(entry.get("part").and_then(Value::as_str).unwrap_or_default());
            let only_part = entry.as_object().is_some_and(|map| map.len() == 1);
            let position = ledger.iter().position(|(existing, _)| *existing == key);
            if only_part {
                if let Some(at) = position {
                    ledger.remove(at); // an entry that names only its part takes it out
                }
                continue;
            }
            let mut kept = Map::new();
            for name in ["part", "records", "skip"] {
                if let Some(value) = entry.get(name).filter(|value| !value.is_null()) {
                    kept.insert(name.into(), value.clone());
                }
            }
            match position {
                Some(at) => ledger[at].1 = Value::Object(kept),
                None => ledger.push((key, Value::Object(kept))),
            }
        }
        let map = record.as_object_mut().ok_or_else(|| LoreError::new("a record is a JSON object"))?;
        let parts: Vec<Value> = ledger.into_iter().map(|(_, entry)| entry).collect();
        map.insert("parts".into(), if parts.is_empty() { Value::Null } else { Value::Array(parts.clone()) });
        if history {
            map.insert("status".into(), json!("history"));
            map.insert("reason".into(), json!(reason));
            if !extra_records.is_empty() {
                let mut listed: Vec<String> = map.get("records").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
                for id in &extra_records {
                    if !listed.contains(id) {
                        listed.push(id.clone());
                    }
                }
                map.insert("records".into(), json!(listed));
            }
        } else if map.get("status").and_then(Value::as_str) != Some("history") {
            let done = cover::read_doc(&config, &path, None).is_some_and(|data| cover::coverage(&cover::parts_of(&cover::doc_text(&data)), &parts).uncovered.is_empty());
            map.insert("status".into(), json!(if done { "covered" } else { "open" }));
        }
        if let Some(title) = &title {
            map.insert("title".into(), json!(title));
        }
        if current.is_some() {
            return Ok(vec![Change::update(record, None)]);
        }
        Ok(ops::fill_ids(&[record], snapshot).into_iter().map(Change::new).collect())
    };
    ctx.write(io, &build, None, Vec::new())
}

fn cmd_fmt(args: &Args, env: &Env, io: &mut Io) -> Result<i32> {
    let config = config::load(&start(args, env))?;
    let mut changed = 0;
    for path in store::lore_files(&config.root, &config.dir) {
        let raw = std::fs::read(&path).map_err(|error| LoreError::new(format!("{}: {error}", path.display())))?;
        let text = String::from_utf8_lossy(raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw)).replace("\r\n", "\n");
        let mut records = Vec::new();
        for (index, line) in text.split('\n').enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line).map_err(|error| LoreError::new(format!("{}: line {} is not JSON ({}); fmt does not guess", path.display(), index + 1, json_reason(&error))))?;
            records.push(value);
        }
        let data = serialize(&records);
        if data != raw {
            std::fs::write(&path, &data).map_err(|error| LoreError::new(format!("{}: {error}", path.display())))?;
            changed += 1;
            io.say(&format!("rewrote {}", store::relative(&config.root, &path)));
        }
    }
    io.say(&format!("{changed} file(s) rewritten{}", if changed > 0 { "; commit them" } else { "" }));
    Ok(0)
}

fn cmd_index(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let finder = ctx.env.finder.ok_or_else(|| LoreError::new("`lore index` needs the search index, and this build has none"))?;
    let records = ctx.records()?;
    let info = finder.index(&ctx.config, &records, ctx.args.flag("rebuild"), ctx.args.text_only)?;
    if ctx.args.json {
        io.json(&shape_index(&info));
    } else {
        io.say(&format!("{} record(s), {} with vectors, index at {}", info.records, info.vectors, info.index));
        if !info.semantic {
            io.note(info.note.as_deref().filter(|note| !note.is_empty()).unwrap_or("text only"));
        }
    }
    Ok(0)
}

/// How agents search (task 149): the mix, hit@1/hit@5 by class and caller,
/// misses and the counters' bounds. Counts only - no query or record text.
fn cmd_stats(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let days: i64 = match ctx.args.one("days") {
        Some(value) => value
            .parse()
            .map_err(|_| LoreError::new(format!("argument --days: invalid int value: {}", repr_str(&value))))?,
        None => 30,
    };
    let report = crate::stats::report(&ctx.config, days);
    if ctx.args.json {
        io.json(&shape_stats(&report));
        return Ok(0);
    }
    let percent = |rate: f64| format!("{:.0}%", rate * 100.0);
    io.say(&format!(
        "{} find(s) in the last {} day(s): {} opened ({} at rank 1, {} at rank 5 or better); {} miss(es); mean {:.1} ms",
        report.finds, report.window_days, report.opened, report.hit1, report.hit5, report.misses, report.latency_ms
    ));
    for (class, metric) in &report.by_class {
        io.say(&format!(
            "  {class}: {} find(s), hit@1 {}, hit@5 {}",
            metric.finds,
            percent(metric.hit1_rate()),
            percent(metric.hit5_rate())
        ));
    }
    for (caller, metric) in &report.by_caller {
        io.say(&format!(
            "  {caller}: {} find(s), hit@1 {}, hit@5 {}",
            metric.finds,
            percent(metric.hit1_rate()),
            percent(metric.hit5_rate())
        ));
    }
    let tiers: Vec<String> = report.tiers.iter().map(|(tier, count)| format!("{tier} {count}")).collect();
    io.say(&format!("  tiers: {}", if tiers.is_empty() { "none".to_owned() } else { tiers.join(", ") }));
    io.say(&format!(
        "  keeping {} find(s); older than {} day(s) or beyond {} find(s) are pruned",
        report.retained,
        crate::stats::RETENTION_DAYS,
        crate::stats::RETENTION_FINDS
    ));
    Ok(0)
}

fn cmd_plans(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {    let rows = plans::summary(&ctx.records()?, ctx.args.flag("all"));
    if ctx.args.json {
        io.json(&Value::Array(rows));
        return Ok(0);
    }
    if rows.is_empty() {
        io.say("no plans");
    }
    for row in &rows {
        let plan = &row["plan"];
        let count = |status: &str| row["counts"][status].as_u64().unwrap_or(0);
        io.say(&format!(
            "{:28} [{}]  open {}  deferred {}  done {}  dropped {}  superseded {}  - {}",
            field(plan, "id"),
            field(plan, "status"),
            count("open"),
            count("deferred"),
            count("done"),
            count("dropped"),
            count("superseded"),
            field(plan, "title")
        ));
    }
    Ok(0)
}

fn cmd_items(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let records = ctx.records()?;
    let by_id: BTreeMap<String, Value> = records.iter().filter(|record| field(record, "kind") == "item").map(|record| (field(record, "id").to_owned(), record.clone())).collect();
    let closed: std::collections::BTreeSet<&str> = records.iter().filter(|record| field(record, "kind") == "plan" && field(record, "status") == "closed").map(|record| field(record, "id")).collect();
    let mut items: Vec<&Value> = by_id.values().collect();
    items.sort_by_key(|item| (field(item, "plan").to_owned(), item_number(field(item, "id"))));
    let plan = ctx.args.one("plan");
    let status = ctx.args.one("status");
    let mut result = Vec::new();
    for item in items {
        if plan.as_deref().is_some_and(|plan| field(item, "plan") != plan) {
            continue;
        }
        if status.as_deref().is_some_and(|status| field(item, "status") != status) {
            continue;
        }
        if status.is_none() && !ctx.args.flag("all") && (!UNDECIDED.contains(&field(item, "status")) || closed.contains(field(item, "plan"))) {
            continue;
        }
        if ctx.args.flag("ready") && !plans::ready(item, &by_id) {
            continue;
        }
        result.push(item.clone());
    }
    if ctx.args.json {
        io.json(&Value::Array(result));
        return Ok(0);
    }
    if result.is_empty() {
        io.say("no items");
    }
    for item in &result {
        let mark = if plans::ready(item, &by_id) { "*" } else { " " };
        io.say(&format!("{mark} {:28} [{}] {}", field(item, "id"), field(item, "status"), field(item, "title")));
    }
    Ok(0)
}

fn cmd_stale(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let days: i64 = match ctx.args.one("days") {
        Some(days) => days.parse().map_err(|_| LoreError::new(format!("argument --days: invalid int value: {}", repr_str(&days))))?,
        None => 14,
    };
    let records = ctx.records()?;
    let result = plans::stale(&ctx.config.root, &records, days, None);
    if ctx.args.json {
        io.json(&Value::Array(result));
        return Ok(0);
    }
    if result.is_empty() {
        io.say(&format!("nothing undecided has gone untouched for {days} day(s)"));
    }
    for item in &result {
        io.say(&format!("{:28} [{}] since {}  {}  - keep, drop, or defer?", field(item, "id"), field(item, "status"), shown_field(item, "updated"), field(item, "title")));
    }
    Ok(0)
}

fn cmd_plan_new(ctx: &Ctx, io: &mut Io) -> Result<i32> {
    let status = ctx.args.one("status").unwrap_or_else(|| "live".into());
    if !["draft", "live", "paused"].contains(&status.as_str()) {
        return Err(LoreError::new(format!("argument --status: invalid choice: {}", repr_str(&status))));
    }
    let mut record = Map::new();
    record.insert("kind".into(), json!("plan"));
    record.insert("title".into(), json!(ctx.args.one("title")));
    record.insert("goal".into(), json!(ctx.args.one("goal")));
    record.insert("status".into(), json!(status));
    record.insert("area".into(), json!("plans"));
    if let Some(id) = ctx.args.one("id").filter(|id| !id.is_empty()) {
        record.insert("id".into(), json!(id));
    }
    if let Some(doc) = ctx.args.one("doc").filter(|doc| !doc.is_empty()) {
        record.insert("doc".into(), json!(doc.replace('\\', "/")));
    }
    record.extend(parse_sets(&ctx.args.many("set"), None)?);
    let build = ops::adder(vec![Value::Object(record)]);
    ctx.write(io, &build, None, Vec::new())
}

fn cmd_plan_set(ctx: &Ctx, io: &mut Io) -> Result<i32> {
    let status = ctx.args.one("status");
    if status.as_deref() == Some("closed") {
        return Err(LoreError::new("a plan closes with `lore plan close <id> --outcome ...`"));
    }
    if let Some(status) = &status {
        if !["draft", "live", "paused"].contains(&status.as_str()) {
            return Err(LoreError::new(format!("argument --status: invalid choice: {}", repr_str(status))));
        }
    }
    let sets = ctx.args.many("set");
    let build = ops::updater(
        ctx.args.positional[0].clone(),
        Patch::Of(Box::new(move |current: &Value| {
            let mut patch = parse_sets(&sets, Some(current))?;
            if let Some(status) = &status {
                patch.insert("status".into(), json!(status));
            }
            Ok(patch)
        })),
        None,
    );
    ctx.write(io, &build, None, Vec::new())
}

fn cmd_item_add(ctx: &Ctx, io: &mut Io) -> Result<i32> {
    let mut record = Map::new();
    record.insert("kind".into(), json!("item"));
    record.insert("plan".into(), json!(ctx.args.positional[0]));
    record.insert("title".into(), json!(ctx.args.one("title")));
    record.insert("status".into(), json!("open"));
    record.insert("area".into(), json!("plans"));
    if let Some(description) = ctx.args.one("description").filter(|text| !text.is_empty()) {
        record.insert("description".into(), json!(description));
    }
    let criteria = ctx.args.many("criterion");
    if !criteria.is_empty() {
        record.insert("criteria".into(), json!(criteria));
    }
    let deps = ctx.args.many("dep");
    if !deps.is_empty() {
        record.insert("deps".into(), Value::Array(deps.iter().map(|dep| dep_of(dep)).collect::<Result<Vec<_>>>()?));
    }
    record.extend(parse_sets(&ctx.args.many("set"), None)?);
    let build = ops::adder(vec![Value::Object(record)]);
    ctx.write(io, &build, None, Vec::new())
}

/// Fields that only some item statuses carry; a move clears the ones it leaves.
const CONDITIONAL: &[(&str, &[&str])] = &[
    ("commits", &["done"]),
    ("reason", &["dropped", "deferred"]),
    ("decided_by", &["dropped"]),
    ("superseded_by", &["superseded"]),
    ("outcome", &["done", "dropped", "superseded"]),
];

fn item_move(ctx: &Ctx, io: &mut Io, status: &str, fields: Vec<(&str, Value)>, reopen_reason: Option<String>) -> Result<i32> {
    let id = ctx.args.positional[0].clone();
    let mut patch = Map::new();
    for (name, statuses) in CONDITIONAL {
        if !statuses.contains(&status) {
            patch.insert((*name).into(), Value::Null);
        }
    }
    for (name, value) in fields {
        if !value.is_null() || name != "outcome" {
            patch.insert(name.into(), value);
        }
    }
    patch.insert("status".into(), json!(status));
    let subject = reopen_reason.as_ref().map(|reason| format!("lore: reopen item {id}\n\nreason: {reason}"));
    let build = ops::updater(id, Patch::Fixed(patch), reopen_reason);
    ctx.write(io, &build, subject, Vec::new())
}

#[path = "cli_working.rs"]
mod working_cli;
