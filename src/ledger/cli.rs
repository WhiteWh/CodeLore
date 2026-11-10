//! `lore ledger ...`: the commands over the journals (docs/FORMAT.md,
//! "ledger, version 4"). They write files and nothing else; whoever keeps the
//! files versions them.
use super::cache::{Cache, Filter, Key};
use super::fold::{self, Entry};
use super::{author, canonical, field_type, journal_author, journal_of, machine, parse_whole, write_errors, Line};
use crate::config::{self, Config, FieldType, Ledger};
use crate::py::repr_str;
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

const USAGE: &str = "usage: lore ledger [--pending <file>] <set|get|find|fold|log|conflicts|resolve|clear|check|status|promote|ack|export|repair> ...";

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
    missing: Vec<String>,
    keys: Vec<String>,
    kinds: Vec<String>,
    ids: Vec<String>,
    keep: bool,
    strict: bool,
    with_state: bool,
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
                "--keep" => opts.keep = true,
                "--strict" => opts.strict = true,
                "--with-state" => opts.with_state = true,
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
                "--missing" => opts.missing.push(value()?),
                "--key" => opts.keys.push(value()?),
                "--kind" => opts.kinds.push(value()?),
                "--id" => opts.ids.extend(value()?.split(',').map(str::trim).filter(|id| !id.is_empty()).map(str::to_owned)),
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
        "get" | "log" | "clear" => need(2, Some(2), "kind key")?,
        "find" => need(1, Some(1), "kind")?,
        "fold" | "check" | "status" => need(0, Some(0), "")?,
        "conflicts" => need(0, Some(1), "")?,
        "resolve" => need(3, Some(4), "kind key field value")?,
        "promote" | "export" => need(1, Some(1), "file")?,
        "ack" => {
            need(1, Some(1), "file")?;
            if opts.ids.is_empty() {
                return Err("ack names the lines it confirms: --id <id>[,<id>...]".into());
            }
        }
        "repair" => need(0, Some(1), "")?,
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

/// The lines of the pending files read over the journals, and the files that
/// end in an unfinished line.
#[derive(Default)]
struct Pending {
    lines: Vec<Line>,
    incomplete: Vec<(String, usize)>,
}

impl Pending {
    fn by_entry(&self) -> BTreeMap<Key, Vec<Line>> {
        let mut grouped: BTreeMap<Key, Vec<Line>> = BTreeMap::new();
        for line in &self.lines {
            grouped.entry(line.entry()).or_default().push(line.clone());
        }
        grouped
    }
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

    /// The whole lines of a pending file (none when it does not exist yet),
    /// and the length of an unfinished last line.
    fn read_pending(&self, given: &str) -> Result<(Vec<Line>, usize)> {
        match std::fs::read(self.file(given)) {
            Ok(bytes) => parse_whole(&bytes, given, self.ledger()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((Vec::new(), 0)),
            Err(error) => Err(LoreError::new(format!("{given}: {error}"))),
        }
    }

    /// Every `--pending` file's lines, and `--to`'s when it is written to -
    /// refused, all of them, when one breaks the graph the journals and the
    /// other pending lines make (see `graph_errors`).
    fn pending(&self, cache: &Cache, out: &mut Out) -> Result<Pending> {
        let mut pending = Pending::default();
        for given in self.opts.pending.iter().chain(self.opts.to.iter()) {
            let (lines, tail) = self.read_pending(given)?;
            if tail > 0 {
                out.stderr.push_str(&format!("lore: warning: {given} ends in an unfinished line ({tail} bytes), not read; the state is not complete\n"));
                pending.incomplete.push((given.clone(), tail));
            }
            pending.lines.extend(lines);
        }
        let all: Vec<&Line> = pending.lines.iter().collect();
        let errs = graph_errors(&all, &pending.lines, cache)?;
        if !errs.is_empty() {
            return Err(LoreError::with("the pending lines are refused; nothing was read - a line's id and parents must be its own entry's", errs));
        }
        Ok(pending)
    }

    fn say(out: &mut Out, text: &str) {
        out.stdout.push_str(text);
        out.stdout.push('\n');
    }

    fn json(out: &mut Out, value: &Value) {
        Self::say(out, &crate::cli::pretty(value));
    }

    /// An entry as the state shows it: retired fields left out.
    fn project(&self, kind: &str, entry: &Entry) -> Entry {
        let ledger = self.ledger();
        let mut shown = entry.clone();
        shown.set.retain(|field, _| !ledger.is_retired(kind, field));
        shown.conflicts.retain(|field, _| !ledger.is_retired(kind, field));
        shown
    }

    fn live_kind(&self, kind: &str) -> Result<()> {
        let ledger = self.ledger();
        if !ledger.kinds.contains_key(kind) {
            return Err(LoreError::new(format!("kind {} is not declared in [ledger.kinds]", repr_str(kind))));
        }
        if ledger.retired_kinds.contains(kind) {
            return Err(LoreError::new(format!("kind {kind} is retired; `lore ledger log` still shows its history")));
        }
        Ok(())
    }
}

/// What is wrong with reading `lines` over the journals, or moving them into
/// one, with `context` (every pending line in play, `lines` among them) beside
/// them: one id standing for two lines, a parent that is a line of another
/// entry - in a journal or pending - and a journal line of another entry that
/// waits for one of `lines` as its parent. Lines of one entry under one id are
/// left to that entry's fold, which refuses them by name.
fn graph_errors(lines: &[&Line], context: &[Line], cache: &Cache) -> Result<Vec<String>> {
    let mut errs = Vec::new();
    let mut owners: BTreeMap<&str, &Line> = BTreeMap::new();
    for line in context {
        match owners.get(line.id.as_str()) {
            Some(other) if other.entry() != line.entry() => errs.push(format!(
                "id {} stands for a line of {} {} and one of {} {} - an id collision, or a changed copy",
                line.id,
                other.kind,
                repr_str(&other.key),
                line.kind,
                repr_str(&line.key)
            )),
            Some(_) => {}
            None => {
                owners.insert(line.id.as_str(), line);
            }
        }
    }
    for line in lines {
        let entry = line.entry();
        if let Some((kind, key)) = cache.owner(&line.id)?.filter(|owner| *owner != entry) {
            errs.push(format!("line {} of {} {} has the id of a journal line of {kind} {} - an id collision, or a changed copy", line.id, line.kind, repr_str(&line.key), repr_str(&key)));
        }
        for parent in &line.parents {
            let owner = match owners.get(parent.as_str()) {
                Some(other) => Some(other.entry()),
                None => cache.owner(parent)?,
            };
            if let Some((kind, key)) = owner.filter(|owner| *owner != entry) {
                errs.push(format!("line {} of {} {} names parent {parent}, which is a line of {kind} {}", line.id, line.kind, repr_str(&line.key), repr_str(&key)));
            }
        }
        for (child, kind, key, place) in cache.waiting(&line.id)? {
            if (kind.as_str(), key.as_str()) != (line.kind.as_str(), line.key.as_str()) {
                errs.push(format!("{place}: line {child} of {kind} {} names parent {}, which is a pending line of {} {}", repr_str(&key), line.id, line.kind, repr_str(&line.key)));
            }
        }
    }
    errs.sort();
    errs.dedup();
    Ok(errs)
}

/// Why a state is not known to be complete: missing parents and unfinished lines.
fn gaps(entries: &BTreeMap<Key, Entry>, cache: &Cache, pending: &Pending) -> Vec<String> {
    let mut found = Vec::new();
    for ((kind, key), entry) in entries {
        for (line, parent) in &entry.dangling {
            found.push(format!("{kind} {}: line {line} names parent {parent}, which is in no journal here", repr_str(key)));
        }
    }
    for (file, tail) in cache.incomplete.iter().chain(&pending.incomplete) {
        found.push(format!("{file} ends in an unfinished line ({tail} bytes)"));
    }
    found
}

fn strict_refusal(gaps: Vec<String>) -> LoreError {
    LoreError::with("the state is not complete (--strict)", gaps)
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
        "repair" => return cmd_repair(&ctx, out),
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
        "clear" => cmd_clear(&ctx, &cache, out),
        "promote" => cmd_promote(&ctx, &cache, out),
        "ack" => cmd_ack(&ctx, &cache, out),
        "export" => cmd_export(&ctx, &cache, out),
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
/// Returns the lines written.
fn write(ctx: &Ctx<'_>, cache: &Cache, requests: Vec<Request>, out: &mut Out) -> Result<Vec<Line>> {
    let ledger = ctx.ledger();
    let by = ctx.author()?;
    let at = ctx.now();
    let pending = ctx.pending(cache, out)?;
    // Per entry: the pending lines and this call's lines so far - each request
    // folds only its own entry, so a batch stays linear.
    let mut over: BTreeMap<Key, Vec<Line>> = pending.by_entry();
    let mut known_here: BTreeMap<String, Key> = pending.lines.iter().map(|line| (line.id.clone(), line.entry())).collect();
    // Per entry this call writes to: its heads now and its fields in conflict,
    // kept up as lines are added - a line after every head is the one head, and
    // ends every conflict on a field it sets - so an entry edited many times in
    // one batch, or with a long history, is folded once. A line with parents of
    // its own makes the entry be folded again.
    let mut seen: BTreeMap<Key, (Vec<String>, BTreeSet<String>)> = BTreeMap::new();
    let mut explicit: BTreeSet<Key> = BTreeSet::new();
    let mut lines: Vec<Line> = Vec::new();
    let mut errs = Vec::new();
    for (index, request) in requests.into_iter().enumerate() {
        let who = if ctx.opts.batch { format!("line {}: ", index + 1) } else { String::new() };
        let entry_key: Key = (request.kind.clone(), request.key.clone());
        let mut found = write_errors(ledger, &request.kind, &request.set);
        if request.key.is_empty() {
            found.push("the key is empty".into());
        }
        if let Some(note) = &request.note {
            if note.is_empty() || note.chars().count() > super::NOTE_MAX {
                found.push(format!("a note is 1-{} characters", super::NOTE_MAX));
            }
        }
        let empty = Vec::new();
        let defaulted = request.parents.is_none();
        let parents = match request.parents {
            Some(parents) => {
                for parent in &parents {
                    if !super::is_id(parent) {
                        found.push(format!("parent {} is not a line id ({} hex digits)", repr_str(parent), super::ID_LEN));
                        continue;
                    }
                    let owner = match known_here.get(parent) {
                        Some(owner) => Some(owner.clone()),
                        None => cache.owner(parent)?,
                    };
                    if let Some((kind, key)) = owner.filter(|owner| *owner != entry_key) {
                        found.push(format!("parent {parent} is a line of {kind} {}, not of {} {}", repr_str(&key), request.kind, repr_str(&request.key)));
                    }
                }
                parents
            }
            None => {
                if !seen.contains_key(&entry_key) {
                    let entry = cache.entry_with(ledger, &request.kind, &request.key, over.get(&entry_key).unwrap_or(&empty))?.unwrap_or_default();
                    let conflicts = ctx.project(&request.kind, &entry).conflicts.into_keys().collect();
                    seen.insert(entry_key.clone(), (entry.heads, conflicts));
                }
                seen.get(&entry_key).map(|(heads, _)| heads.clone()).unwrap_or_default()
            }
        };
        if !found.is_empty() {
            errs.extend(found.into_iter().map(|error| format!("{who}{error}")));
            continue;
        }
        let line = Line::new(&request.kind, &request.key, request.set, parents, &by, &at, request.note);
        if defaulted {
            if let Some((heads, conflicts)) = seen.get_mut(&entry_key) {
                *heads = vec![line.id.clone()];
                conflicts.retain(|field| !line.set.contains_key(field));
            }
        } else {
            seen.remove(&entry_key);
            explicit.insert(entry_key.clone());
        }
        over.entry(entry_key.clone()).or_default().push(line.clone());
        known_here.insert(line.id.clone(), entry_key);
        lines.push(line);
    }
    if !errs.is_empty() {
        return Err(LoreError::with("refused, nothing was written", errs));
    }
    if lines.is_empty() {
        return Err(LoreError::new("nothing to write"));
    }
    let shown = match &ctx.opts.to {
        Some(given) => {
            let path = ctx.file(given);
            super::locked(&path, || super::append(&path, &lines))?;
            given.clone()
        }
        None => {
            let relative = journal_of(&by, &machine(ctx.call.machine.as_deref())?, &at);
            super::append(&super::folder(&ctx.config.root, ledger).join(&relative), &lines)?;
            format!("{}/{relative}", ledger.dir)
        }
    };
    // What the entries come to now, to say a conflict they are in.
    let mut notes = Vec::new();
    let touched: BTreeSet<Key> = lines.iter().map(Line::entry).collect();
    for entry_key in &touched {
        let (kind, key) = entry_key;
        let conflicts: Vec<String> = match seen.get(entry_key) {
            Some((_, conflicts)) if !explicit.contains(entry_key) => conflicts.iter().cloned().collect(),
            _ => {
                let mut all = cache.lines(ledger, kind, key)?;
                all.extend(over.get(entry_key).cloned().unwrap_or_default());
                ctx.project(kind, &fold::fold(&all)).conflicts.into_keys().collect()
            }
        };
        if !conflicts.is_empty() {
            notes.push(format!(
                "{kind} {} has a conflict on {} - `lore ledger conflicts` shows it, `lore ledger resolve` ends it",
                repr_str(key),
                conflicts.join(", ")
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
    Ok(lines)
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
    write(ctx, cache, requests, out).map(|_| 0)
}

fn cmd_resolve(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let opts = ctx.opts;
    let (kind, key, field) = (&opts.positional[0], &opts.positional[1], &opts.positional[2]);
    let value = match (opts.positional.get(3), opts.unset.as_slice()) {
        (Some(text), []) => value_of(ctx.ledger(), kind, field, text).map_err(LoreError::new)?,
        (None, [unset]) if unset == field => Value::Null,
        _ => return Err(LoreError::new("resolve takes the field's value, or --unset <field> in its place")),
    };
    let pending = ctx.pending(cache, out)?;
    let entry = cache.entry_with(ctx.ledger(), kind, key, &pending.lines)?.ok_or_else(|| LoreError::new(format!("no ledger entry {kind} {}", repr_str(key))))?;
    if !entry.conflicts.contains_key(field.as_str()) {
        return Err(LoreError::new(format!("{kind} {} has no conflict on {field} - set it with `lore ledger set`", repr_str(key))));
    }
    let mut set = Map::new();
    set.insert(field.clone(), value);
    // The heads the person saw (--parents), else every head: the line comes
    // after each side of the conflict it was shown. A line that came later and
    // was not seen stays concurrent with it, and its conflict stays.
    let parents = opts.parents.clone().unwrap_or_else(|| entry.heads.clone());
    let written = write(ctx, cache, vec![Request { kind: kind.clone(), key: key.clone(), set, parents: Some(parents), note: opts.note.clone() }], out)?;
    let mut all = cache.lines(ctx.ledger(), kind, key)?;
    all.extend(pending.lines.iter().filter(|line| line.kind == *kind && line.key == *key).cloned());
    all.extend(written);
    if fold::fold(&all).conflicts.contains_key(field.as_str()) && !ctx.opts.json {
        Ctx::say(out, &format!("note: {field} is still in conflict - a line the resolution did not see sets it too"));
    }
    Ok(0)
}

/// Unsets every field the entry holds or has in conflict, after the heads the
/// writer saw. The entry stays: an empty entry is still one, and its history and
/// any conflict a concurrent line makes stay visible.
fn cmd_clear(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let (kind, key) = (&ctx.opts.positional[0], &ctx.opts.positional[1]);
    ctx.live_kind(kind)?;
    let pending = ctx.pending(cache, out)?;
    let Some(entry) = cache.entry_with(ctx.ledger(), kind, key, &pending.lines)? else {
        // An entry's first lines may be in what has not arrived whole.
        let missing = gaps(&BTreeMap::new(), cache, &pending);
        if ctx.opts.strict && !missing.is_empty() {
            return Err(strict_refusal(missing));
        }
        let unsure = if missing.is_empty() { String::new() } else { " - but the state is not complete, an unfinished line may hold it".to_owned() };
        return Err(LoreError::new(format!("no ledger entry {kind} {}{unsure}", repr_str(key))));
    };
    let entry = ctx.project(kind, &entry);
    let fields: BTreeSet<&String> = entry.set.keys().chain(entry.conflicts.keys()).collect();
    if fields.is_empty() {
        if ctx.opts.json {
            Ctx::json(out, &json!({"file": null, "lines": [], "notes": ["nothing to clear: no field is set"]}));
        } else {
            Ctx::say(out, "nothing to clear: no field is set");
        }
        return Ok(0);
    }
    let set: Map<String, Value> = fields.into_iter().map(|field| (field.clone(), Value::Null)).collect();
    let parents = ctx.opts.parents.clone().unwrap_or_else(|| entry.heads.clone());
    write(ctx, cache, vec![Request { kind: kind.clone(), key: key.clone(), set, parents: Some(parents), note: ctx.opts.note.clone() }], out).map(|_| 0)
}

fn entry_json(kind: &str, key: &str, entry: &Entry) -> Value {
    let mut value = entry.to_json();
    if let Value::Object(map) = &mut value {
        map.insert("kind".into(), Value::String(kind.into()));
        map.insert("key".into(), Value::String(key.into()));
        map.insert("complete".into(), Value::Bool(entry.dangling.is_empty()));
    }
    value
}

fn cmd_get(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let (kind, key) = (&ctx.opts.positional[0], &ctx.opts.positional[1]);
    ctx.live_kind(kind)?;
    let pending = ctx.pending(cache, out)?;
    let Some(entry) = cache.entry_with(ctx.ledger(), kind, key, &pending.lines)? else {
        return Err(LoreError::new(format!("no ledger entry {kind} {}", repr_str(key))));
    };
    let entry = ctx.project(kind, &entry);
    let mut one = BTreeMap::new();
    one.insert((kind.clone(), key.clone()), entry.clone());
    let missing = gaps(&one, cache, &pending);
    if ctx.opts.strict && !missing.is_empty() {
        return Err(strict_refusal(missing));
    }
    if ctx.opts.json {
        let mut value = entry_json(kind, key, &entry);
        value["complete"] = Value::Bool(missing.is_empty());
        Ctx::json(out, &value);
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
    for gap in &missing {
        Ctx::say(out, &format!("  not complete: {gap}"));
    }
    Ok(0)
}

fn cmd_find(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let kind = &ctx.opts.positional[0];
    ctx.live_kind(kind)?;
    let ledger = ctx.ledger();
    let mut wheres = Vec::new();
    for word in &ctx.opts.wheres {
        let Some((field, text)) = word.split_once('=') else { return Err(LoreError::new(format!("--where {} is not field=value", repr_str(word)))) };
        if field_type(ledger, kind, field).is_none() || ledger.is_retired(kind, field) {
            return Err(LoreError::new(format!("kind {kind} has no field {} in its state", repr_str(field))));
        }
        wheres.push((field.to_owned(), canonical(&value_of(ledger, kind, field, text).map_err(LoreError::new)?)));
    }
    for field in &ctx.opts.missing {
        if field_type(ledger, kind, field).is_none() || ledger.is_retired(kind, field) {
            return Err(LoreError::new(format!("kind {kind} has no field {} in its state", repr_str(field))));
        }
    }
    // The stored state is canonical, so `"field":value` is in every match:
    // sqlite narrows the entries down (never past one that matches), the
    // values are then compared exactly. Entries pending lines touch are
    // folded again and compared whatever the narrowing said.
    let needles = wheres.iter().map(|(field, want)| format!("{}:{want}", canonical(&Value::String(field.clone())))).collect();
    let pending = ctx.pending(cache, out)?;
    let entries = cache.state(ledger, Some(kind), &Filter::Holding(needles), &pending.lines)?;
    let shown: BTreeMap<Key, Entry> = entries.iter().map(|(at, entry)| (at.clone(), ctx.project(kind, entry))).collect();
    let found: BTreeMap<Key, Entry> = shown
        .into_iter()
        .filter(|(_, entry)| wheres.iter().all(|(field, want)| entry.set.get(field).is_some_and(|value| canonical(value) == *want)))
        .filter(|(_, entry)| ctx.opts.missing.iter().all(|field| !entry.set.contains_key(field)))
        .collect();
    // Whether the answer is complete is a question about the whole kind, not
    // about the matches: a parent still to come can set a field of any entry
    // with a missing parent, and so bring it into the answer or take it out -
    // an empty answer included. So every entry of the kind is looked at
    // before the values are compared.
    let unsure: BTreeMap<Key, Entry> =
        cache.state(ledger, Some(kind), &Filter::Dangling, &pending.lines)?.into_iter().filter(|(_, entry)| !entry.dangling.is_empty()).collect();
    let missing = gaps(&unsure, cache, &pending);
    if ctx.opts.strict && !missing.is_empty() {
        return Err(strict_refusal(missing));
    }
    if ctx.opts.json {
        let rows: Vec<Value> = found
            .iter()
            .map(|((_, key), entry)| {
                let dangling: Vec<Value> = entry.dangling.iter().map(|(line, parent)| json!({"line": line, "parent": parent})).collect();
                json!({"key": key, "set": entry.set, "conflicts": entry.conflicts.keys().collect::<Vec<_>>(), "dangling": dangling})
            })
            .collect();
        Ctx::json(out, &json!({"complete": missing.is_empty(), "gaps": missing, "entries": rows}));
        return Ok(0);
    }
    for ((_, key), entry) in &found {
        let mark = if entry.conflicts.is_empty() { String::new() } else { format!("  (conflict: {})", entry.conflicts.keys().cloned().collect::<Vec<_>>().join(", ")) };
        let gap = if entry.dangling.is_empty() { "" } else { "  (not complete)" };
        Ctx::say(out, &format!("{}  {}{mark}{gap}", repr_str(key), shown_set(&entry.set)));
    }
    if found.is_empty() {
        Ctx::say(out, "nothing found");
    }
    say_gaps(out, &missing);
    Ok(0)
}

/// The reasons a state is not complete, the first ten of them in text.
fn say_gaps(out: &mut Out, missing: &[String]) {
    const SHOWN: usize = 10;
    for gap in missing.iter().take(SHOWN) {
        Ctx::say(out, &format!("not complete: {gap}"));
    }
    if missing.len() > SHOWN {
        Ctx::say(out, &format!("not complete: and {} more - `lore ledger check` lists them", missing.len() - SHOWN));
    }
}

fn cmd_fold(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let pending = ctx.pending(cache, out)?;
    let ledger = ctx.ledger();
    let entries: BTreeMap<Key, Entry> = cache
        .state(ledger, None, &Filter::All, &pending.lines)?
        .into_iter()
        .filter(|((kind, _), _)| !ledger.retired_kinds.contains(kind))
        .map(|((kind, key), entry)| {
            let shown = ctx.project(&kind, &entry);
            ((kind, key), shown)
        })
        .collect();
    let missing = gaps(&entries, cache, &pending);
    if ctx.opts.strict && !missing.is_empty() {
        return Err(strict_refusal(missing));
    }
    let mut state: Map<String, Value> = Map::new();
    for ((kind, key), entry) in &entries {
        let by_kind = state.entry(kind.clone()).or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(map) = by_kind {
            // --with-state: the values with what they rest on, from one read.
            let value = if ctx.opts.with_state { entry.to_json() } else { Value::Object(entry.set.clone()) };
            map.insert(key.clone(), value);
        }
    }
    if ctx.opts.json {
        if ctx.opts.with_state {
            Ctx::json(out, &json!({"complete": missing.is_empty(), "gaps": missing, "entries": state}));
        } else {
            Ctx::json(out, &Value::Object(state));
        }
    } else {
        for ((kind, key), entry) in &entries {
            let mark = if entry.conflicts.is_empty() { String::new() } else { format!("  (conflict: {})", entry.conflicts.keys().cloned().collect::<Vec<_>>().join(", ")) };
            Ctx::say(out, &format!("{kind} {}  {}{mark}", repr_str(key), shown_set(&entry.set)));
        }
        say_gaps(out, &missing);
    }
    Ok(0)
}

fn cmd_log(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let (kind, key) = (&ctx.opts.positional[0], &ctx.opts.positional[1]);
    let mut lines = cache.lines(ctx.ledger(), kind, key)?;
    let journal: BTreeSet<String> = lines.iter().map(|line| line.id.clone()).collect();
    lines.extend(ctx.pending(cache, out)?.lines.into_iter().filter(|line| line.kind == *kind && line.key == *key));
    if lines.is_empty() {
        return Err(LoreError::new(format!("no ledger entry {kind} {}", repr_str(key))));
    }
    let entry = super::fold_checked(kind, key, &lines)?;
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
    let ledger = ctx.ledger();
    let pending = ctx.pending(cache, out)?;
    let entries = cache.state(ledger, kind, &Filter::Conflicted, &pending.lines)?;
    let mut rows = Vec::new();
    for ((kind, key), entry) in &entries {
        if ledger.retired_kinds.contains(kind) {
            continue;
        }
        let entry = ctx.project(kind, entry);
        for (field, sides) in entry.conflicts {
            rows.push((kind.clone(), key.clone(), field, sides));
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
    let pending = ctx.pending(cache, out)?;
    // Counted in sqlite; with pending lines the touched entries are folded again.
    let (count, conflicts) = if pending.lines.is_empty() {
        let (count, conflicts, _) = cache.counts()?;
        (count as usize, conflicts as usize)
    } else {
        let entries = cache.state(ledger, None, &Filter::All, &pending.lines)?;
        (entries.len(), entries.values().map(|entry| entry.conflicts.len()).sum())
    };
    let with_dangling = cache.state(ledger, None, &Filter::Dangling, &pending.lines)?;
    let missing = gaps(&with_dangling, cache, &pending);
    let mut dangling = Vec::new();
    for ((kind, key), entry) in &with_dangling {
        for (line, parent) in &entry.dangling {
            dangling.push(json!({"kind": kind, "key": key, "line": line, "parent": parent}));
        }
    }
    let lines: i64 = files.iter().map(|(_, _, lines)| lines).sum();
    let bytes: i64 = files.iter().map(|(_, bytes, _)| bytes).sum();
    let unfinished: Vec<Value> = cache.incomplete.iter().chain(&pending.incomplete).map(|(file, tail)| json!({"file": file, "bytes": tail})).collect();
    if ctx.opts.json {
        Ctx::json(
            out,
            &json!({
                "dir": ledger.dir,
                "files": files.iter().map(|(path, bytes, lines)| json!({"path": format!("{}/{path}", ledger.dir), "bytes": bytes, "lines": lines})).collect::<Vec<_>>(),
                "lines": lines,
                "bytes": bytes,
                "entries": count,
                "conflicts": conflicts,
                "dangling": dangling,
                "unfinished": unfinished,
                "complete": missing.is_empty(),
                "cache": cache.path.display().to_string(),
            }),
        );
        return Ok(0);
    }
    Ctx::say(
        out,
        &format!(
            "{}: {} journal{}, {lines} lines, {bytes} bytes; {} entries, {conflicts} conflicting field{}{}",
            ledger.dir,
            files.len(),
            if files.len() == 1 { "" } else { "s" },
            count,
            if conflicts == 1 { "" } else { "s" },
            if missing.is_empty() { "" } else { "; not complete" }
        ),
    );
    for (path, bytes, lines) in &files {
        Ctx::say(out, &format!("  {}/{path}  {lines} lines, {bytes} bytes", ledger.dir));
    }
    for gap in &missing {
        Ctx::say(out, &format!("  not complete: {gap}"));
    }
    Ok(0)
}

/// Every file, read whole and checked line by line, without the cache.
fn cmd_check(ctx: &Ctx<'_>, out: &mut Out) -> Result<i32> {
    let ledger = ctx.ledger();
    let folder = super::folder(&ctx.config.root, ledger);
    let (mut errors, mut warnings) = (Vec::new(), Vec::new());
    let mut all: Vec<(String, Line)> = Vec::new();
    let mut sources: Vec<(String, PathBuf, Option<String>)> = Vec::new();
    for relative in super::files(&folder)? {
        let shown = format!("{}/{relative}", ledger.dir);
        if relative.ends_with(".lock") || relative.ends_with(".tmp") {
            continue;
        }
        if relative.contains(".jsonl.torn-") {
            warnings.push(format!("{shown}: bytes a repair set aside - look at them, then delete the file"));
            continue;
        }
        match journal_author(&relative) {
            Some((author, _)) => sources.push((shown, folder.join(&relative), Some(author))),
            None => errors.push(format!("{shown}: not a journal - a journal is <author>@<machine>/<YYYY-MM>.jsonl")),
        }
    }
    for given in &ctx.opts.pending {
        sources.push((given.clone(), ctx.file(given), None));
    }
    let mut contents = Vec::new();
    for (shown, path, _) in &sources {
        contents.push(std::fs::read(path).map_err(|error| LoreError::new(format!("{shown}: {error}; nothing was checked")))?);
    }
    let count = sources.len();
    let mut jobs: Vec<(String, &[u8], usize)> = Vec::new();
    for ((shown, _, _), bytes) in sources.iter().zip(&contents) {
        let (whole, tail) = super::whole_lines(bytes);
        if tail > 0 {
            errors.push(format!("{shown}: ends in an unfinished line ({tail} bytes) - a write in progress, a copy cut short or damage; `lore ledger repair` sets it aside"));
        }
        jobs.push((shown.clone(), whole, 1));
    }
    for ((shown, _, author), parsed) in sources.iter().zip(super::parse_many(&jobs, ledger)) {
        match parsed {
            Ok(lines) => {
                for (at, (line, _)) in lines.into_iter().enumerate() {
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
    let mut first_of: BTreeMap<&str, &Line> = BTreeMap::new();
    for (place, line) in &all {
        places.entry(line.id.as_str()).or_default().push(place.as_str());
        match first_of.get(line.id.as_str()) {
            Some(first) if *first != line => errors.push(format!("{place}: id {} stands for another line too - an id collision, or a changed copy", line.id)),
            Some(_) => {}
            None => {
                first_of.insert(line.id.as_str(), line);
            }
        }
    }
    for (id, at) in places.iter().filter(|(_, at)| at.len() > 1) {
        warnings.push(format!("line {id} is written {} times ({}); it counts once", at.len(), at.join(", ")));
    }
    for (place, line) in &all {
        for parent in &line.parents {
            match first_of.get(parent.as_str()) {
                None => {
                    let text = format!("{place}: parent {parent} is in no journal here - its writer's journal may not have arrived");
                    if ctx.opts.strict { errors.push(text) } else { warnings.push(text) }
                }
                Some(other) if (other.kind.as_str(), other.key.as_str()) != (line.kind.as_str(), line.key.as_str()) => {
                    errors.push(format!("{place}: parent {parent} is a line of {} {}, not of this entry", other.kind, repr_str(&other.key)));
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

/// The lines of a pending file `--id`, `--key` and `--kind` choose: exact ids
/// when given (each must be there), else every line of the chosen entries.
fn choose<'a>(ctx: &Ctx<'_>, lines: &'a [Line], given: &str) -> Result<Vec<&'a Line>> {
    if !ctx.opts.ids.is_empty() {
        let mut chosen = Vec::new();
        let mut absent = Vec::new();
        for id in &ctx.opts.ids {
            match lines.iter().find(|line| line.id == *id) {
                Some(line) => chosen.push(line),
                None => absent.push(id.clone()),
            }
        }
        if !absent.is_empty() {
            return Err(LoreError::with(format!("refused, nothing was moved: {given} has no line"), absent));
        }
        return Ok(chosen);
    }
    Ok(lines.iter().filter(|line| (ctx.opts.kinds.is_empty() || ctx.opts.kinds.contains(&line.kind)) && (ctx.opts.keys.is_empty() || ctx.opts.keys.contains(&line.key))).collect())
}

/// A chosen line's parent that is still only in the pending file must go with it.
fn closure_errors(chosen: &[&Line], lines: &[Line], cache: &Cache) -> Result<Vec<String>> {
    let picked: BTreeSet<&str> = chosen.iter().map(|line| line.id.as_str()).collect();
    let here: BTreeSet<&str> = lines.iter().map(|line| line.id.as_str()).collect();
    let mut errs = Vec::new();
    for line in chosen {
        for parent in &line.parents {
            if here.contains(parent.as_str()) && !picked.contains(parent.as_str()) && !cache.has_line(parent)? {
                errs.push(format!("line {} needs its parent {}, which is only in the pending file - choose it too", line.id, parent));
            }
        }
    }
    Ok(errs)
}

/// The pending file's bytes without the given lines, its unfinished last
/// line (when there is one) kept as it was.
fn without(bytes: &[u8], lines: &[Line], gone: &BTreeSet<&str>) -> Vec<u8> {
    let (whole, _) = super::whole_lines(bytes);
    let mut rest: Vec<u8> = Vec::new();
    for line in lines.iter().filter(|line| !gone.contains(line.id.as_str())) {
        rest.extend_from_slice(line.text().as_bytes());
        rest.push(b'\n');
    }
    rest.extend_from_slice(&bytes[whole.len()..]);
    rest
}

/// Moves the chosen lines of a pending file into the author's journal and,
/// unless `--keep`, out of the pending file. Under the file's lock from read to
/// rewrite, so a line written to it meanwhile is never lost. The journal is
/// written and synced first: a crash before the pending file is rewritten
/// leaves the lines in both places, and the next promote finds them there.
fn cmd_promote(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let ledger = ctx.ledger();
    let given = &ctx.opts.positional[0];
    let path = ctx.file(given);
    let by = ctx.author()?;
    let relative = journal_of(&by, &machine(ctx.call.machine.as_deref())?, &ctx.now());
    let shown = format!("{}/{relative}", ledger.dir);
    let (moved, already, left, entries) = super::locked(&path, || {
        let bytes = std::fs::read(&path).map_err(|error| LoreError::new(format!("{given}: {error}")))?;
        let (lines, tail) = parse_whole(&bytes, given, ledger)?;
        if tail > 0 {
            out.stderr.push_str(&format!("lore: warning: {given} ends in an unfinished line ({tail} bytes); it is not moved and stays in the file\n"));
        }
        let chosen = choose(ctx, &lines, given)?;
        let mut errs: Vec<String> = chosen.iter().filter(|line| line.by != by).map(|line| format!("line {} is by {}: only {by}'s lines go into {by}'s journal", line.id, line.by)).collect();
        for line in &chosen {
            errs.extend(write_errors(ledger, &line.kind, &line.set).into_iter().map(|error| format!("line {}: {error} - it stays in {given}", line.id)));
        }
        errs.extend(closure_errors(&chosen, &lines, cache)?);
        errs.extend(graph_errors(&chosen, &lines, cache)?);
        if !errs.is_empty() {
            return Err(LoreError::with("refused, nothing was moved", errs));
        }
        let mut fresh = Vec::new();
        for line in &chosen {
            if !cache.has_line(&line.id)? {
                fresh.push((*line).clone());
            }
        }
        super::append(&super::folder(&ctx.config.root, ledger).join(&relative), &fresh).map_err(|mut error| {
            error.details.push(format!("{given} was not changed: its lines are all still there, and a promote moves only those no journal holds yet"));
            error
        })?;
        let gone: BTreeSet<&str> = chosen.iter().map(|line| line.id.as_str()).collect();
        let left = if ctx.opts.keep {
            lines.len()
        } else {
            super::replace(&path, &without(&bytes, &lines, &gone))?;
            lines.len() - gone.len()
        };
        let entries: BTreeSet<Key> = chosen.iter().map(|line| line.entry()).collect();
        Ok((fresh.iter().map(|line| line.id.clone()).collect::<Vec<_>>(), chosen.len() - fresh.len(), left, entries))
    })?;
    if ctx.opts.json {
        Ctx::json(
            out,
            &json!({"file": shown, "moved": moved, "already": already, "left": left, "kept": ctx.opts.keep, "entries": entries.iter().map(|(kind, key)| json!({"kind": kind, "key": key})).collect::<Vec<_>>()}),
        );
    } else {
        let verb = if ctx.opts.keep { "copied" } else { "moved" };
        Ctx::say(out, &format!("{verb} {} line{} of {} entr{} to {shown}; {left} left in {given}", moved.len(), if moved.len() == 1 { "" } else { "s" }, entries.len(), if entries.len() == 1 { "y" } else { "ies" }));
        if already > 0 {
            Ctx::say(out, &format!("note: {already} were in the journal already"));
        }
    }
    Ok(0)
}

/// Takes confirmed lines out of a pending file: each must be in a journal
/// already. When the confirmation comes is the caller's to say - a line's
/// copy in the journal is not one by itself. An id the file no longer holds
/// was taken out before, and counts as done.
fn cmd_ack(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let ledger = ctx.ledger();
    let given = &ctx.opts.positional[0];
    let path = ctx.file(given);
    let (removed, gone_before) = super::locked(&path, || {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(LoreError::new(format!("{given}: {error}"))),
        };
        let (lines, _) = parse_whole(&bytes, given, ledger)?;
        let mut errs = Vec::new();
        let mut gone: BTreeSet<&str> = BTreeSet::new();
        let mut before = Vec::new();
        for id in &ctx.opts.ids {
            match lines.iter().find(|line| line.id == *id) {
                Some(line) if cache.has_line(&line.id)? => {
                    gone.insert(line.id.as_str());
                }
                Some(line) => errs.push(format!("line {} is in no journal yet - promote it first", line.id)),
                None => before.push(id.clone()),
            }
        }
        if !errs.is_empty() {
            return Err(LoreError::with(format!("refused, {given} was not changed"), errs));
        }
        if !gone.is_empty() {
            super::replace(&path, &without(&bytes, &lines, &gone))?;
        }
        Ok((gone.into_iter().map(str::to_owned).collect::<Vec<_>>(), before))
    })?;
    if ctx.opts.json {
        Ctx::json(out, &json!({"file": given, "removed": removed, "gone_before": gone_before}));
    } else {
        Ctx::say(out, &format!("took {} confirmed line{} out of {given}", removed.len(), if removed.len() == 1 { "" } else { "s" }));
        if !gone_before.is_empty() {
            Ctx::say(out, &format!("note: {} were taken out before", gone_before.len()));
        }
    }
    Ok(0)
}

/// Prints the chosen lines of a pending file as they are, and changes nothing:
/// what an outside step is about to send, frozen by id.
fn cmd_export(ctx: &Ctx<'_>, cache: &Cache, out: &mut Out) -> Result<i32> {
    let ledger = ctx.ledger();
    let given = &ctx.opts.positional[0];
    let path = ctx.file(given);
    let texts = super::locked(&path, || {
        let bytes = std::fs::read(&path).map_err(|error| LoreError::new(format!("{given}: {error}")))?;
        let (lines, _) = parse_whole(&bytes, given, ledger)?;
        let chosen = choose(ctx, &lines, given)?;
        let mut errs = closure_errors(&chosen, &lines, cache)?;
        errs.extend(graph_errors(&chosen, &lines, cache)?);
        if !errs.is_empty() {
            return Err(LoreError::with("refused", errs));
        }
        Ok(chosen.iter().map(|line| line.value()).collect::<Vec<_>>())
    })?;
    if ctx.opts.json {
        Ctx::json(out, &json!({"file": given, "lines": texts}));
    } else {
        for value in &texts {
            Ctx::say(out, &canonical(value));
        }
    }
    Ok(0)
}

/// Sets aside the unfinished last line of the author's own journals on this
/// machine, or of the pending file given: its bytes go to `<file>.torn-<n>`.
fn cmd_repair(ctx: &Ctx<'_>, out: &mut Out) -> Result<i32> {
    let ledger = ctx.ledger();
    let mut targets: Vec<(String, PathBuf)> = Vec::new();
    match ctx.opts.positional.first() {
        Some(given) => targets.push((given.clone(), ctx.file(given))),
        None => {
            let by = ctx.author()?;
            let here = machine(ctx.call.machine.as_deref())?;
            let folder = super::folder(&ctx.config.root, ledger);
            for relative in super::files(&folder)? {
                if journal_author(&relative).is_some_and(|(author, machine)| author == by && machine == here) {
                    targets.push((format!("{}/{relative}", ledger.dir), folder.join(&relative)));
                }
            }
        }
    }
    // A file named here may be a pending file: its writers, promote and ack
    // hold `<file>.lock` from read to rewrite, so repair holds it too - a
    // promote that read the file before the cut can then not put the cut
    // bytes back. Journals are only appended to, under a lock on the file
    // itself, which repair takes as well.
    let named = !ctx.opts.positional.is_empty();
    let mut done = Vec::new();
    for (shown, path) in &targets {
        let repaired = if named { super::locked(path, || super::repair(path))? } else { super::repair(path)? };
        if let Some((tail, kept)) = repaired {
            done.push(json!({"file": shown, "bytes": tail, "kept": kept.display().to_string()}));
        }
    }
    if ctx.opts.json {
        Ctx::json(out, &json!({"repaired": done}));
    } else if done.is_empty() {
        Ctx::say(out, "nothing to repair: every file ends in a whole line");
    } else {
        for row in &done {
            Ctx::say(out, &format!("{}: set aside an unfinished line ({} bytes) in {}", row["file"].as_str().unwrap_or_default(), row["bytes"], row["kept"].as_str().unwrap_or_default()));
        }
    }
    Ok(0)
}
