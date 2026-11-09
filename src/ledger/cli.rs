//! `lore ledger ...`: the commands over the journals (docs/FORMAT.md,
//! "ledger, version 4"). They write files and nothing else; whoever keeps the
//! files versions them.
use super::cache::{Cache, Key};
use super::fold::{self, Entry};
use super::{author, field_type, journal_author, journal_of, machine, parse_bytes, set_errors, Line};
use crate::config::{self, Config, FieldType, Ledger};
use crate::py::repr_str;
use crate::store::canonical;
use crate::{LoreError, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// The ledger's part of `lore guide`, printed when lore.toml has `[ledger]`.
pub const GUIDE: &str = include_str!("guide.md");

pub struct Call<'a> {
    /// The words after `ledger`.
    pub argv: &'a [String],
    /// Where lore.toml is looked for, and relative files are taken from.
    pub start: PathBuf,
    pub stdin: &'a [u8],
    pub json: bool,
    /// `--by` given before `ledger`, else `LORE_BY`.
    pub by: Option<String>,
    /// `LORE_MACHINE`.
    pub machine: Option<String>,
    /// UTC seconds; None: the clock.
    pub now: Option<i64>,
}

pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

const USAGE: &str = "usage: lore ledger [--pending <file>] <set|get|find|fold|log|conflicts|resolve|check|status|promote> ...";

#[derive(Default)]
struct Opts {
    command: String,
    positional: Vec<String>,
    json: bool,
    by: Option<String>,
    pending: Vec<String>,
    to: Option<String>,
    batch: bool,
    unset: Vec<String>,
    parents: Option<Vec<String>>,
    note: Option<String>,
    wheres: Vec<String>,
    keys: Vec<String>,
    kinds: Vec<String>,
}

fn parse(argv: &[String]) -> std::result::Result<Opts, String> {
    let mut opts = Opts::default();
    let mut at = 0;
    let mut rest_only = false;
    while at < argv.len() {
        let word = argv[at].clone();
        at += 1;
        if !rest_only && word == "--" {
            rest_only = true;
            continue;
        }
        if !rest_only && word.starts_with("--") {
            let (name, inline) = match word.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
                None => (word.clone(), None),
            };
            let mut value = || -> std::result::Result<String, String> {
                if let Some(value) = inline.clone() {
                    return Ok(value);
                }
                let value = argv.get(at).cloned().ok_or_else(|| format!("argument {name}: expected one argument"))?;
                at += 1;
                Ok(value)
            };
            match name.as_str() {
                "--json" => opts.json = true,
                "--batch" => opts.batch = true,
                "--by" => opts.by = Some(value()?),
                "--pending" => opts.pending.push(value()?),
                "--to" => opts.to = Some(value()?),
                "--unset" => opts.unset.push(value()?),
                "--parents" => {
                    let text = value()?;
                    opts.parents = Some(text.split(',').map(str::trim).filter(|id| !id.is_empty()).map(str::to_owned).collect());
                }
                "--note" => opts.note = Some(value()?),
                "--where" => opts.wheres.push(value()?),
                "--key" => opts.keys.push(value()?),
                "--kind" => opts.kinds.push(value()?),
                _ => return Err(format!("unrecognized arguments: {word}")),
            }
            continue;
        }
        if opts.command.is_empty() {
            opts.command = word;
        } else {
            opts.positional.push(word);
        }
    }
    let count = opts.positional.len();
    let need = |least: usize, most: Option<usize>, names: &str| -> std::result::Result<(), String> {
        if count < least {
            return Err(format!("the following arguments are required: {names}"));
        }
        if most.is_some_and(|most| count > most) {
            return Err(format!("unrecognized arguments: {}", opts.positional[most.unwrap_or(0)..].join(" ")));
        }
        Ok(())
    };
    match opts.command.as_str() {
        "" => return Err("a ledger command is needed".into()),
        "set" if opts.batch => need(0, Some(0), "")?,
        "set" => need(2, None, "kind key field=value")?,
        "get" | "log" => need(2, Some(2), "kind key")?,
        "find" => need(1, Some(1), "kind")?,
        "fold" | "check" | "status" => need(0, Some(0), "")?,
        "conflicts" => need(0, Some(1), "")?,
        "resolve" => need(3, Some(4), "kind key field value")?,
        "promote" => need(1, Some(1), "file")?,
        other => return Err(format!("argument <ledger command>: invalid choice: {}", repr_str(other))),
    }
    Ok(opts)
}

pub fn run(call: &Call<'_>) -> Out {
    let mut out = Out { code: 0, stdout: String::new(), stderr: String::new() };
    let mut opts = match parse(call.argv) {
        Ok(opts) => opts,
        Err(message) => {
            out.code = 2;
            out.stderr = format!("{USAGE}\nlore: error: {message}\n");
            return out;
        }
    };
    opts.json |= call.json;
    let json = opts.json;
    match dispatch(&opts, call, &mut out) {
        Ok(code) => out.code = code,
        Err(error) => {
            out.code = 1;
            if json {
                out.stdout.push_str(&crate::cli::pretty(&json!({"error": error.message, "details": error.details})));
                out.stdout.push('\n');
            } else {
                out.stderr.push_str(&format!("lore: {error}\n"));
            }
        }
    }
    out
}

struct Ctx<'a> {
    config: Config,
    opts: &'a Opts,
    call: &'a Call<'a>,
}

impl Ctx<'_> {
    fn ledger(&self) -> &Ledger {
        self.config.ledger.as_ref().expect("checked in dispatch")
    }

    fn file(&self, given: &str) -> PathBuf {
        let path = PathBuf::from(given);
        if path.is_absolute() { path } else { self.call.start.join(path) }
    }

    fn author(&self) -> Result<String> {
        author(self.opts.by.as_deref().or(self.call.by.as_deref()))
    }

    fn now(&self) -> String {
        super::at_of(self.call.now.unwrap_or_else(|| chrono::Utc::now().timestamp()))
    }

    /// The lines of a pending file (none when it does not exist yet).
    fn read_pending(&self, given: &str) -> Result<Vec<Line>> {
        let path = self.file(given);
        match std::fs::read(&path) {
            Ok(bytes) => parse_bytes(&bytes, 1, given, self.ledger()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(LoreError::new(format!("{given}: {error}"))),
        }
    }

    /// Every `--pending` file's lines, and `--to`'s when it is written to.
    fn pending(&self) -> Result<Vec<Line>> {
        let mut lines = Vec::new();
        for given in self.opts.pending.iter().chain(self.opts.to.iter()) {
            lines.extend(self.read_pending(given)?);
        }
        Ok(lines)
    }

    fn say(out: &mut Out, text: &str) {
        out.stdout.push_str(text);
        out.stdout.push('\n');
    }

    fn json(out: &mut Out, value: &Value) {
        Self::say(out, &crate::cli::pretty(value));
    }
}

fn dispatch(opts: &Opts, call: &Call<'_>, out: &mut Out) -> Result<i32> {
    let config = config::load(&call.start)?;
    for warning in &config.warnings {
        out.stderr.push_str(&format!("lore: warning: {warning}\n"));
    }
    if config.ledger.is_none() {
        return Err(LoreError::new("this lore.toml has no [ledger]: declare its kinds under [ledger.kinds.<kind>] to keep a ledger"));
    }
    let ctx = Ctx { config, opts, call };
    match opts.command.as_str() {
        "check" => return cmd_check(&ctx, out),
        "promote" => return cmd_promote(&ctx, out),
        _ => {}
    }
    let cache = Cache::open(&ctx.config, ctx.ledger())?;
    for warning in &cache.warnings {
        out.stderr.push_str(&format!("lore: warning: {warning}\n"));
    }
    match opts.command.as_str() {
        "set" => cmd_set(&ctx, &cache, out),
        "get" => cmd_get(&ctx, &cache, out),
        "find" => cmd_find(&ctx, &cache, out),
        "fold" => cmd_fold(&ctx, &cache, out),
        "log" => cmd_log(&ctx, &cache, out),
        "conflicts" => cmd_conflicts(&ctx, &cache, out),
        "resolve" => cmd_resolve(&ctx, &cache, out),
        _ => cmd_status(&ctx, &cache, out),
    }
}

/// `field=value` as the field's declared type: a string or enum is taken as
/// written, the other types are JSON.
fn value_of(ledger: &Ledger, kind: &str, field: &str, text: &str) -> std::result::Result<Value, String> {
    match field_type(ledger, kind, field) {
        None | Some(FieldType::String) | Some(FieldType::Enum(_)) => Ok(Value::String(text.to_owned())),
        Some(kind_of) => serde_json::from_str::<Value>(text).map_err(|_| format!("{kind}.{field} must be {}, not {}", kind_of.name(), repr_str(text))),
    }
}

fn assignments(ledger: &Ledger, kind: &str, words: &[String], unset: &[String]) -> Result<Map<String, Value>> {
    let mut set = Map::new();
    let mut errs = Vec::new();
    for word in words {
        let Some((field, text)) = word.split_once('=') else {
            errs.push(format!("{} is not field=value", repr_str(word)));
            continue;
        };
        match value_of(ledger, kind, field, text) {
            Ok(value) => {
                set.insert(field.to_owned(), value);
            }
            Err(error) => errs.push(error),
        }
    }
    for field in unset {
        set.insert(field.clone(), Value::Null);
    }
    if errs.is_empty() { Ok(set) } else { Err(LoreError::with("refused, nothing was written", errs)) }
}

/// One line to write, before its parents are known.
struct Request {
    kind: String,
    key: String,
    set: Map<String, Value>,
    parents: Option<Vec<String>>,
    note: Option<String>,
}

fn batch_requests(stdin: &[u8]) -> Result<Vec<Request>> {
    let text = std::str::from_utf8(stdin).map_err(|_| LoreError::new("stdin is not UTF-8"))?;
    let mut requests = Vec::new();
    let mut errs = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let number = index + 1;
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw) else {
            errs.push(format!("stdin line {number}: not a JSON object"));
            continue;
        };
        if let Some(extra) = map.keys().find(|key| !["kind", "key", "set", "parents", "note"].contains(&key.as_str())) {
            errs.push(format!("stdin line {number}: unknown field {} (a line is {{\"kind\", \"key\", \"set\", \"parents\"?, \"note\"?}})", repr_str(extra)));
            continue;
        }
        let (Some(kind), Some(key), Some(set)) = (map.get("kind").and_then(Value::as_str), map.get("key").and_then(Value::as_str), map.get("set").and_then(Value::as_object)) else {
            errs.push(format!("stdin line {number}: needs \"kind\" and \"key\" strings and a \"set\" object"));
            continue;
        };
        let parents = match map.get("parents") {
            None => None,
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => Some(items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect()),
            Some(_) => {
                errs.push(format!("stdin line {number}: \"parents\" must be a list of line ids"));
                continue;
            }
        };
        let note = match map.get("note") {
            None => None,
            Some(Value::String(note)) => Some(note.clone()),
            Some(_) => {
                errs.push(format!("stdin line {number}: \"note\" must be a string"));
                continue;
            }
        };
        requests.push(Request { kind: kind.into(), key: key.into(), set: set.clone(), parents, note });
    }
    if errs.is_empty() { Ok(requests) } else { Err(LoreError::with("refused, nothing was written", errs)) }
}

/// Builds the lines of `requests` in order, each one's default parents the
/// heads its entry has with the lines before it; then writes them all at once.
fn write(ctx: &Ctx<'_>, cache: &Cache, requests: Vec<Request>, out: &mut Out) -> Result<i32> {
    let ledger = ctx.ledger();
    let by = ctx.author()?;
    let at = ctx.now();
    let mut pending = ctx.pending()?;
    let mut lines: Vec<Line> = Vec::new();
    let mut errs = Vec::new();
    for (index, request) in requests.into_iter().enumerate() {
        let who = if ctx.opts.batch { format!("line {}: ", index + 1) } else { String::new() };
        let mut found = set_errors(ledger, &request.kind, &request.set);
        if request.key.is_empty() {
            found.push("the key is empty".into());
        }
        if let Some(note) = &request.note {
            if note.is_empty() || note.chars().count() > super::NOTE_MAX {
                found.push(format!("a note is 1-{} characters", super::NOTE_MAX));
            }
        }
        let parents = match request.parents {
            Some(parents) => {
                for parent in parents.iter().filter(|parent| !super::is_id(parent)) {
                    found.push(format!("parent {} is not a line id ({} hex digits)", repr_str(parent), super::ID_LEN));
                }
                parents
            }
            None => {
                let mut over = pending.clone();
                over.extend(lines.iter().cloned());
                cache.entry_with(ledger, &request.kind, &request.key, &over)?.map(|entry| entry.heads).unwrap_or_default()
            }
        };
        if !found.is_empty() {
            errs.extend(found.into_iter().map(|error| format!("{who}{error}")));
            continue;
        }
        lines.push(Line::new(&request.kind, &request.key, request.set, parents, &by, &at, request.note));
    }
    if !errs.is_empty() {
        return Err(LoreError::with("refused, nothing was written", errs));
    }
    if lines.is_empty() {
        return Err(LoreError::new("nothing to write"));
    }
    let (path, shown) = match &ctx.opts.to {
        Some(given) => (ctx.file(given), given.clone()),
        None => {
            let relative = journal_of(&by, &machine(ctx.call.machine.as_deref())?, &at);
            (super::folder(&ctx.config.root, ledger).join(&relative), format!("{}/{relative}", ledger.dir))
        }
    };
    super::append(&path, &lines)?;
    pending.extend(lines.iter().cloned());
    // What the entries come to now, to say a conflict they are in.
    let mut notes = Vec::new();
    let touched: BTreeSet<Key> = lines.iter().map(Line::entry).collect();
    for (kind, key) in &touched {
        let mut all = cache.lines(ledger, kind, key)?;
        all.extend(pending.iter().filter(|line| line.kind == *kind && line.key == *key).cloned());
        let entry = fold::fold(&all);
        if !entry.conflicts.is_empty() {
            notes.push(format!(
                "{kind} {} has a conflict on {} - `lore ledger conflicts` shows it, `lore ledger resolve` ends it",
                repr_str(key),
                entry.conflicts.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    if ctx.opts.json {
        Ctx::json(out, &json!({"file": shown, "lines": lines.iter().map(Line::value).collect::<Vec<_>>(), "notes": notes}));
    } else {
        Ctx::say(out, &format!("wrote {} line{} to {shown}", lines.len(), if lines.len() == 1 { "" } else { "s" }));
        for line in &lines {
            Ctx::say(out, &format!("  {} {} {}  {}", line.id, line.kind, repr_str(&line.key), shown_set(&line.set)));
        }
        for note in &notes {
            Ctx::say(out, &format!("note: {note}"));
        }
    }
    Ok(0)
}

fn shown_set(set: &Map<String, Value>) -> String {
    set.iter().map(|(field, value)| format!("{field}={}", shown_value(value))).collect::<Vec<_>>().join(" ")
}

fn shown_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "(unset)".into(),
        other => canonical(other),
    }
}

fn cmd_set(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let opts = ctx.opts;
    let requests = if opts.batch {
        if !opts.unset.is_empty() || opts.parents.is_some() || opts.note.is_some() {
            return Err(LoreError::new("with --batch, --unset, --parents and --note go in the stdin lines"));
        }
        batch_requests(ctx.call.stdin)?
    } else {
        let (kind, key) = (&opts.positional[0], &opts.positional[1]);
        let set = assignments(ctx.ledger(), kind, &opts.positional[2..], &opts.unset)?;
        vec![Request { kind: kind.clone(), key: key.clone(), set, parents: opts.parents.clone(), note: opts.note.clone() }]
    };
    write(ctx, cache, requests, out)
}

fn cmd_resolve(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let opts = ctx.opts;
    let (kind, key, field) = (&opts.positional[0], &opts.positional[1], &opts.positional[2]);
    let value = match (opts.positional.get(3), opts.unset.as_slice()) {
        (Some(text), []) => value_of(ctx.ledger(), kind, field, text).map_err(LoreError::new)?,
        (None, [unset]) if unset == field => Value::Null,
        _ => return Err(LoreError::new("resolve takes the field's value, or --unset <field> in its place")),
    };
    let pending = ctx.pending()?;
    let entry = cache.entry_with(ctx.ledger(), kind, key, &pending)?.ok_or_else(|| LoreError::new(format!("no ledger entry {kind} {}", repr_str(key))))?;
    if !entry.conflicts.contains_key(field.as_str()) {
        return Err(LoreError::new(format!("{kind} {} has no conflict on {field} - set it with `lore ledger set`", repr_str(key))));
    }
    let mut set = Map::new();
    set.insert(field.clone(), value);
    // Every head as a parent: the line comes after every side of the conflict.
    write(ctx, cache, vec![Request { kind: kind.clone(), key: key.clone(), set, parents: Some(entry.heads.clone()), note: opts.note.clone() }], out)
}

fn entry_json(kind: &str, key: &str, entry: &Entry) -> Value {
    let mut value = entry.to_json();
    if let Value::Object(map) = &mut value {
        map.insert("kind".into(), Value::String(kind.into()));
        map.insert("key".into(), Value::String(key.into()));
    }
    value
}

fn cmd_get(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let (kind, key) = (&ctx.opts.positional[0], &ctx.opts.positional[1]);
    let pending = ctx.pending()?;
    let Some(entry) = cache.entry_with(ctx.ledger(), kind, key, &pending)? else {
        return Err(LoreError::new(format!("no ledger entry {kind} {}", repr_str(key))));
    };
    if ctx.opts.json {
        Ctx::json(out, &entry_json(kind, key, &entry));
        return Ok(0);
    }
    Ctx::say(out, &format!("{kind} {}", repr_str(key)));
    for (field, value) in &entry.set {
        let mark = if entry.conflicts.contains_key(field) { "  (conflict)" } else { "" };
        Ctx::say(out, &format!("  {field} = {}{mark}", shown_value(value)));
    }
    for (field, sides) in &entry.conflicts {
        if !entry.set.contains_key(field) {
            Ctx::say(out, &format!("  {field} unset  (conflict)"));
        }
        for side in sides {
            Ctx::say(out, &format!("    {field}: {} by {} at {} ({})", shown_value(&side.value), side.by, side.at, side.id));
        }
    }
    for (line, parent) in &entry.dangling {
        Ctx::say(out, &format!("  note: line {line} names parent {parent}, not here yet"));
    }
    Ok(0)
}

fn cmd_find(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let kind = &ctx.opts.positional[0];
    let ledger = ctx.ledger();
    if !ledger.kinds.contains_key(kind.as_str()) {
        return Err(LoreError::new(format!("kind {} is not declared in [ledger.kinds]", repr_str(kind))));
    }
    let mut wheres = Vec::new();
    for word in &ctx.opts.wheres {
        let Some((field, text)) = word.split_once('=') else { return Err(LoreError::new(format!("--where {} is not field=value", repr_str(word)))) };
        if field_type(ledger, kind, field).is_none() {
            return Err(LoreError::new(format!("kind {kind} has no field {}", repr_str(field))));
        }
        wheres.push((field.to_owned(), canonical(&value_of(ledger, kind, field, text).map_err(LoreError::new)?)));
    }
    let entries = cache.state(ledger, Some(kind), &ctx.pending()?)?;
    let found: Vec<(&Key, &Entry)> = entries
        .iter()
        .filter(|(_, entry)| wheres.iter().all(|(field, want)| entry.set.get(field).is_some_and(|value| canonical(value) == *want)))
        .collect();
    if ctx.opts.json {
        Ctx::json(out, &Value::Array(found.iter().map(|((_, key), entry)| json!({"key": key, "set": entry.set, "conflicts": entry.conflicts.keys().collect::<Vec<_>>()})).collect()));
        return Ok(0);
    }
    for ((_, key), entry) in &found {
        let mark = if entry.conflicts.is_empty() { String::new() } else { format!("  (conflict: {})", entry.conflicts.keys().cloned().collect::<Vec<_>>().join(", ")) };
        Ctx::say(out, &format!("{}  {}{mark}", repr_str(key), shown_set(&entry.set)));
    }
    if found.is_empty() {
        Ctx::say(out, "nothing found");
    }
    Ok(0)
}

fn cmd_fold(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let entries = cache.state(ctx.ledger(), None, &ctx.pending()?)?;
    let mut state: Map<String, Value> = Map::new();
    for ((kind, key), entry) in &entries {
        let by_kind = state.entry(kind.clone()).or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(map) = by_kind {
            map.insert(key.clone(), Value::Object(entry.set.clone()));
        }
    }
    let state = Value::Object(state);
    if ctx.opts.json {
        Ctx::json(out, &state);
    } else {
        for ((kind, key), entry) in &entries {
            Ctx::say(out, &format!("{kind} {}  {}", repr_str(key), shown_set(&entry.set)));
        }
    }
    Ok(0)
}

fn cmd_log(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let (kind, key) = (&ctx.opts.positional[0], &ctx.opts.positional[1]);
    let mut lines = cache.lines(ctx.ledger(), kind, key)?;
    let journal: BTreeSet<String> = lines.iter().map(|line| line.id.clone()).collect();
    lines.extend(ctx.pending()?.into_iter().filter(|line| line.kind == *kind && line.key == *key));
    if lines.is_empty() {
        return Err(LoreError::new(format!("no ledger entry {kind} {}", repr_str(key))));
    }
    let entry = fold::fold(&lines);
    let by_id: BTreeMap<&str, &Line> = lines.iter().map(|line| (line.id.as_str(), line)).collect();
    let ordered: Vec<&Line> = entry.order.iter().filter_map(|id| by_id.get(id.as_str()).copied()).collect();
    if ctx.opts.json {
        let rows: Vec<Value> = ordered
            .iter()
            .map(|line| {
                let mut value = line.value();
                if let Value::Object(map) = &mut value {
                    map.insert("pending".into(), Value::Bool(!journal.contains(&line.id)));
                }
                value
            })
            .collect();
        Ctx::json(out, &Value::Array(rows));
        return Ok(0);
    }
    for line in ordered {
        let mark = if journal.contains(&line.id) { "" } else { "  (pending)" };
        let note = line.note.as_ref().map(|note| format!("  - {note}")).unwrap_or_default();
        Ctx::say(out, &format!("{}  {}  {}  {}{note}{mark}", line.at, line.by, line.id, shown_set(&line.set)));
    }
    Ok(0)
}

fn cmd_conflicts(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let kind = ctx.opts.positional.first().map(String::as_str);
    let entries = cache.state(ctx.ledger(), kind, &ctx.pending()?)?;
    let mut rows = Vec::new();
    for ((kind, key), entry) in &entries {
        for (field, sides) in &entry.conflicts {
            rows.push((kind, key, field, sides));
        }
    }
    if ctx.opts.json {
        let rows: Vec<Value> = rows
            .iter()
            .map(|(kind, key, field, sides)| {
                let sides: Vec<Value> = sides.iter().map(|side| json!({"id": side.id, "by": side.by, "at": side.at, "value": side.value})).collect();
                json!({"kind": kind, "key": key, "field": field, "holding": sides[0]["id"], "sides": sides})
            })
            .collect();
        Ctx::json(out, &Value::Array(rows));
        return Ok(0);
    }
    for (kind, key, field, sides) in &rows {
        Ctx::say(out, &format!("{kind} {} {field}:", repr_str(key)));
        for (at, side) in sides.iter().enumerate() {
            let mark = if at == 0 { "  (holds until resolved)" } else { "" };
            Ctx::say(out, &format!("  {} by {} at {} ({}){mark}", shown_value(&side.value), side.by, side.at, side.id));
        }
    }
    if rows.is_empty() {
        Ctx::say(out, "no conflicts");
    }
    Ok(0)
}

fn cmd_status(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let ledger = ctx.ledger();
    let files = cache.files()?;
    let entries = cache.state(ledger, None, &ctx.pending()?)?;
    let conflicts: usize = entries.values().map(|entry| entry.conflicts.len()).sum();
    let mut dangling = Vec::new();
    for ((kind, key), entry) in &entries {
        for (line, parent) in &entry.dangling {
            dangling.push(json!({"kind": kind, "key": key, "line": line, "parent": parent}));
        }
    }
    let lines: i64 = files.iter().map(|(_, _, lines)| lines).sum();
    let bytes: i64 = files.iter().map(|(_, bytes, _)| bytes).sum();
    if ctx.opts.json {
        Ctx::json(
            out,
            &json!({
                "dir": ledger.dir,
                "files": files.iter().map(|(path, bytes, lines)| json!({"path": format!("{}/{path}", ledger.dir), "bytes": bytes, "lines": lines})).collect::<Vec<_>>(),
                "lines": lines,
                "bytes": bytes,
                "entries": entries.len(),
                "conflicts": conflicts,
                "dangling": dangling,
                "cache": cache.path.display().to_string(),
            }),
        );
        return Ok(0);
    }
    Ctx::say(out, &format!("{}: {} journal{}, {lines} lines, {bytes} bytes; {} entries, {conflicts} conflicting field{}", ledger.dir, files.len(), if files.len() == 1 { "" } else { "s" }, entries.len(), if conflicts == 1 { "" } else { "s" }));
    for (path, bytes, lines) in &files {
        Ctx::say(out, &format!("  {}/{path}  {lines} lines, {bytes} bytes", ledger.dir));
    }
    for row in &dangling {
        Ctx::say(
            out,
            &format!("  dangling: {} {} line {} names parent {}, not here yet", row["kind"].as_str().unwrap_or_default(), repr_str(row["key"].as_str().unwrap_or_default()), row["line"].as_str().unwrap_or_default(), row["parent"].as_str().unwrap_or_default()),
        );
    }
    Ok(0)
}

/// Every file, read whole and checked line by line, without the cache.
fn cmd_check(ctx: &Ctx<'_>, out: &mut Out) -> Result<i32> {
    let ledger = ctx.ledger();
    let folder = super::folder(&ctx.config.root, ledger);
    let (mut errors, mut warnings) = (Vec::new(), Vec::new());
    let mut all: Vec<(String, Line)> = Vec::new();
    let mut count = 0;
    let mut sources: Vec<(String, PathBuf, Option<String>)> = Vec::new();
    for relative in super::files(&folder) {
        let shown = format!("{}/{relative}", ledger.dir);
        match journal_author(&relative) {
            Some((author, _)) => sources.push((shown, folder.join(&relative), Some(author))),
            None => errors.push(format!("{shown}: not a journal - a journal is <author>@<machine>/<YYYY-MM>.jsonl")),
        }
    }
    for given in &ctx.opts.pending {
        sources.push((given.clone(), ctx.file(given), None));
    }
    for (shown, path, author) in &sources {
        count += 1;
        let bytes = std::fs::read(path).map_err(|error| LoreError::new(format!("{shown}: {error}")))?;
        match parse_bytes(&bytes, 1, shown, ledger) {
            Ok(lines) => {
                for (at, line) in lines.into_iter().enumerate() {
                    if let Some(author) = author {
                        if line.by != *author {
                            errors.push(format!("{shown}: line {}: `by` {} in {author}'s journal", at + 1, line.by));
                        }
                    }
                    all.push((format!("{shown}: line {}", at + 1), line));
                }
            }
            Err(error) => {
                errors.push(error.message.clone());
                errors.extend(error.details.iter().map(|detail| format!("  {detail}")));
            }
        }
    }
    let mut places: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut entry_of: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for (place, line) in &all {
        places.entry(line.id.as_str()).or_default().push(place.as_str());
        entry_of.insert(line.id.as_str(), (line.kind.as_str(), line.key.as_str()));
    }
    for (id, at) in places.iter().filter(|(_, at)| at.len() > 1) {
        warnings.push(format!("line {id} is written {} times ({}); it counts once", at.len(), at.join(", ")));
    }
    for (place, line) in &all {
        for parent in &line.parents {
            match entry_of.get(parent.as_str()) {
                None => warnings.push(format!("{place}: parent {parent} is not here (yet) - its writer's journal may not have arrived")),
                Some((kind, key)) if (*kind, *key) != (line.kind.as_str(), line.key.as_str()) => {
                    errors.push(format!("{place}: parent {parent} is a line of {kind} {}, not of this entry", repr_str(key)));
                }
                Some(_) => {}
            }
        }
    }
    let entries: BTreeSet<(&str, &str)> = all.iter().map(|(_, line)| (line.kind.as_str(), line.key.as_str())).collect();
    if ctx.opts.json {
        Ctx::json(out, &json!({"files": count, "lines": all.len(), "entries": entries.len(), "errors": errors, "warnings": warnings}));
    } else {
        for error in &errors {
            Ctx::say(out, &format!("error: {error}"));
        }
        for warning in &warnings {
            Ctx::say(out, &format!("warning: {warning}"));
        }
        Ctx::say(out, &format!("{count} files, {} lines, {} entries: {}", all.len(), entries.len(), if errors.is_empty() { "ok" } else { "refused" }));
    }
    Ok(if errors.is_empty() { 0 } else { 1 })
}

/// Moves the lines of the chosen entries from a pending file into the
/// author's journal and leaves the rest where they are.
fn cmd_promote(ctx: &Ctx<'_>, out: &mut Out) -> Result<i32> {
    let ledger = ctx.ledger();
    let given = &ctx.opts.positional[0];
    let path = ctx.file(given);
    let bytes = std::fs::read(&path).map_err(|error| LoreError::new(format!("{given}: {error}")))?;
    let lines = parse_bytes(&bytes, 1, given, ledger)?;
    let by = ctx.author()?;
    let chosen = |line: &Line| (ctx.opts.kinds.is_empty() || ctx.opts.kinds.contains(&line.kind)) && (ctx.opts.keys.is_empty() || ctx.opts.keys.contains(&line.key));
    let (moving, staying): (Vec<Line>, Vec<Line>) = lines.into_iter().partition(|line| chosen(line));
    let foreign: Vec<String> = moving.iter().filter(|line| line.by != by).map(|line| format!("line {} is by {}", line.id, line.by)).collect();
    if !foreign.is_empty() {
        return Err(LoreError::with(format!("refused, nothing was moved: only {by}'s lines go into {by}'s journal"), foreign));
    }
    let cache = Cache::open(&ctx.config, ledger)?;
    for warning in &cache.warnings {
        out.stderr.push_str(&format!("lore: warning: {warning}\n"));
    }
    let mut fresh = Vec::new();
    for line in &moving {
        if !cache.has_line(&line.id)? {
            fresh.push(line.clone());
        }
    }
    let relative = journal_of(&by, &machine(ctx.call.machine.as_deref())?, &ctx.now());
    let shown = format!("{}/{relative}", ledger.dir);
    super::append(&super::folder(&ctx.config.root, ledger).join(&relative), &fresh)?;
    // The pending file is not a journal: it is written again without the moved lines.
    let mut rest = String::new();
    for line in &staying {
        rest.push_str(&line.text());
        rest.push('\n');
    }
    std::fs::write(&path, rest.as_bytes()).map_err(|error| LoreError::new(format!("{given}: {error}")))?;
    let entries: BTreeSet<Key> = moving.iter().map(Line::entry).collect();
    if ctx.opts.json {
        Ctx::json(
            out,
            &json!({"file": shown, "moved": fresh.iter().map(|line| line.id.clone()).collect::<Vec<_>>(), "already": moving.len() - fresh.len(), "left": staying.len(), "entries": entries.iter().map(|(kind, key)| json!({"kind": kind, "key": key})).collect::<Vec<_>>()}),
        );
    } else {
        Ctx::say(out, &format!("moved {} line{} of {} entr{} to {shown}; {} left in {given}", fresh.len(), if fresh.len() == 1 { "" } else { "s" }, entries.len(), if entries.len() == 1 { "y" } else { "ies" }, staying.len()));
        if moving.len() > fresh.len() {
            Ctx::say(out, &format!("note: {} were in the journal already", moving.len() - fresh.len()));
        }
    }
    Ok(0)
}

/// Whether `path` names a file under the ledger folder (for callers that list files).
pub fn is_ledger_path(config: &Config, relative: &str) -> bool {
    config.ledger.as_ref().is_some_and(|ledger| relative.starts_with(&format!("{}/", ledger.dir)))
}
