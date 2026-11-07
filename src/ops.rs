//! Every write goes through `apply` (CodeLore's `ops.py`): fill, validate,
//! check references and plan rules against the base the commit will be built
//! on, then commit by the configured mode. Nothing is half-written.
use crate::config::Config;
use crate::py::{repr_opt, repr_str};
use crate::refs::Roots;
use crate::schema::{area_of, generic_signal, item_id, item_moves, item_number, today, validate, UNDECIDED};
use crate::store::{check_placement, is_lore_file, load_worktree, parse, path_of, serialize, Snapshot};
use crate::{cover, gitio, LoreError, Result, FORMAT_VERSION};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;

/// One record put: a new one, or the new version of an existing one.
#[derive(Clone, Debug)]
pub struct Change {
    pub record: Value,
    pub is_new: bool,
    pub reopen_reason: Option<String>,
}

impl Change {
    pub fn new(record: Value) -> Self {
        Self { record, is_new: true, reopen_reason: None }
    }
    pub fn update(record: Value, reopen_reason: Option<String>) -> Self {
        Self { record, is_new: false, reopen_reason }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    pub commit: Option<String>,
    pub records: Vec<Value>,
    pub notes: Vec<String>,
    /// None: no push was asked for.
    pub pushed: Option<bool>,
    pub push_error: Option<String>,
}

/// A builder makes the changes from the snapshot the write is built on.
pub type Builder<'a> = dyn Fn(&Snapshot) -> Result<Vec<Change>> + 'a;

/// What a write is asked with beyond the configuration.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub by: String,
    pub subject: Option<String>,
    pub trailers: Vec<String>,
    /// `--mode`, overriding `commit.mode` for this call.
    pub mode: Option<String>,
    /// A Zerus project store's reference roots (docs/lore.md 2.4); None is
    /// CodeLore's one repository.
    pub roots: Option<Roots>,
    /// The commit's author in a store that is Zerus's own repository.
    pub identity: Option<gitio::Identity>,
}

/// Python's truth of a JSON value.
pub fn falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

fn text<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    record.get(key).and_then(Value::as_str)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

fn refs_list(record: &Value, key: &str) -> Vec<String> {
    strings(record.get("refs").and_then(|refs| refs.get(key)))
}

pub fn slugify(title: &str) -> String {
    let ascii: String = title.nfkd().filter(char::is_ascii).collect();
    let lower = ascii.to_lowercase();
    let words: Vec<&str> = lower.split(|ch: char| !(ch.is_ascii_lowercase() || ch.is_ascii_digit())).filter(|word| !word.is_empty()).collect();
    let mut out = String::new();
    for word in &words {
        let extra = if out.is_empty() { 0 } else { 1 };
        if out.len() + word.len() + extra > 40 {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(word);
    }
    if out.is_empty() && !words.is_empty() {
        out = words[0].chars().take(40).collect();
    }
    if out.len() >= 2 { out } else { "record".into() }
}

pub fn load_rev(config: &Config, rev: Option<&str>) -> Result<Snapshot> {
    let Some(rev) = rev else { return Ok(Snapshot::default()) };
    let mut files = BTreeMap::new();
    for rel in gitio::ls_tree(&config.root, Some(rev), &config.dir) {
        if is_lore_file(&rel, &config.dir) {
            let bytes = gitio::blob(&config.root, Some(rev), &rel).unwrap_or_default();
            let short: String = rev.chars().take(10).collect();
            files.insert(rel.clone(), parse(&bytes, &format!("{rel} at {short}"))?);
        }
    }
    Ok(Snapshot { files })
}

/// What the next write will be built on: the branch's tip under shared-tree,
/// the working tree otherwise.
pub fn base_snapshot(config: &Config) -> Result<Snapshot> {
    if config.commit_mode == "shared-tree" {
        load_rev(config, gitio::reference(&config.root, &format!("refs/heads/{}", config.branch)).as_deref())
    } else {
        load_worktree(&config.root, &config.dir)
    }
}

/// Strongly connected components of more than one node (Tarjan), each sorted
/// by item number.
fn cycles(items: &BTreeMap<String, Value>) -> Vec<Vec<String>> {
    struct State<'a> {
        items: &'a BTreeMap<String, Value>,
        index: BTreeMap<String, usize>,
        low: BTreeMap<String, usize>,
        on: BTreeSet<String>,
        stack: Vec<String>,
        counter: usize,
        comps: Vec<Vec<String>>,
    }
    fn deps(value: &Value) -> Vec<String> {
        value.get("deps").and_then(Value::as_array).into_iter().flatten().filter_map(|dep| dep.get("item").and_then(Value::as_str).map(str::to_owned)).collect()
    }
    fn strong(state: &mut State, node: &str) {
        state.index.insert(node.to_owned(), state.counter);
        state.low.insert(node.to_owned(), state.counter);
        state.counter += 1;
        state.stack.push(node.to_owned());
        state.on.insert(node.to_owned());
        for next in deps(&state.items[node]) {
            if !state.items.contains_key(&next) {
                continue;
            }
            if !state.index.contains_key(&next) {
                strong(state, &next);
                let low = state.low[node].min(state.low[&next]);
                state.low.insert(node.to_owned(), low);
            } else if state.on.contains(&next) {
                let low = state.low[node].min(state.index[&next]);
                state.low.insert(node.to_owned(), low);
            }
        }
        if state.low[node] == state.index[node] {
            let mut comp = Vec::new();
            while let Some(top) = state.stack.pop() {
                state.on.remove(&top);
                let done = top == node;
                comp.push(top);
                if done {
                    break;
                }
            }
            if comp.len() > 1 {
                comp.sort_by_key(|id| item_number(id));
                state.comps.push(comp);
            }
        }
    }
    let mut state = State { items, index: BTreeMap::new(), low: BTreeMap::new(), on: BTreeSet::new(), stack: Vec::new(), counter: 0, comps: Vec::new() };
    let order: Vec<String> = items.keys().cloned().collect();
    for node in order {
        if !state.index.contains_key(&node) {
            strong(&mut state, &node);
        }
    }
    state.comps
}

/// Everything `lore check` holds a whole snapshot to, bar references to git.
pub fn check_all(config: &Config, snapshot: &Snapshot) -> Vec<String> {
    let mut errs = check_placement(snapshot, &config.dir);
    for (_, record) in snapshot.records() {
        errs.extend(validate(record, &config.areas, Some(&config.limits)));
    }
    errs.extend(relations(&snapshot.by_id()));
    errs
}

pub fn relations(by_id: &BTreeMap<String, Value>) -> Vec<String> {
    let mut errs = Vec::new();
    let items: BTreeMap<String, Value> = by_id.iter().filter(|(_, record)| text(record, "kind") == Some("item")).map(|(id, record)| (id.clone(), record.clone())).collect();
    for (rid, record) in by_id {
        for target in refs_list(record, "records") {
            if !by_id.contains_key(&target) {
                errs.push(format!("{rid}: refs.records names {}, which does not exist", repr_str(&target)));
            }
        }
        for key in ["superseded_by", "reversed_by"] {
            if let Some(target) = record.get(key).filter(|value| !falsy(value)) {
                if !target.as_str().is_some_and(|target| by_id.contains_key(target)) {
                    errs.push(format!("{rid}: `{key}` names {}, which does not exist", crate::py::repr(target)));
                }
            }
        }
        if text(record, "kind") == Some("source") {
            for target in strings(record.get("records")) {
                if !by_id.contains_key(&target) {
                    errs.push(format!("{rid}: `records` names {}, which does not exist", repr_str(&target)));
                }
            }
            for entry in record.get("parts").and_then(Value::as_array).into_iter().flatten() {
                for target in strings(entry.get("records")) {
                    if !by_id.contains_key(&target) {
                        errs.push(format!("{rid}: part {} names {}, which does not exist", repr_opt(entry.get("part")), repr_str(&target)));
                    }
                }
            }
        }
        if text(record, "kind") == Some("item") {
            let plan = record.get("plan").and_then(Value::as_str).and_then(|plan| by_id.get(plan));
            if !plan.is_some_and(|plan| text(plan, "kind") == Some("plan")) {
                errs.push(format!("{rid}: its plan {} does not exist", repr_opt(record.get("plan"))));
            }
            for dep in record.get("deps").and_then(Value::as_array).into_iter().flatten() {
                if dep.is_object() && !dep.get("item").and_then(Value::as_str).is_some_and(|item| items.contains_key(item)) {
                    errs.push(format!("{rid}: depends on {}, which is not an item", repr_opt(dep.get("item"))));
                }
            }
        }
    }
    let mut paths: BTreeMap<String, String> = BTreeMap::new();
    for (rid, record) in by_id {
        if text(record, "kind") == Some("source") {
            if let Some(path) = text(record, "path") {
                if let Some(first) = paths.get(path) {
                    errs.push(format!("{rid}: {path} already has a source record, {first}"));
                }
                paths.entry(path.to_owned()).or_insert_with(|| rid.clone());
            }
        }
    }
    for comp in cycles(&items) {
        let mut chain = comp.clone();
        chain.push(comp[0].clone());
        errs.push(format!("a dependency cycle: {}", chain.join(" -> ")));
    }
    for (rid, record) in by_id {
        if text(record, "kind") == Some("plan") && text(record, "status") == Some("closed") {
            let mut open: Vec<&Value> = items.values().filter(|item| text(item, "plan") == Some(rid) && text(item, "status").is_some_and(|status| UNDECIDED.contains(&status))).collect();
            open.sort_by_key(|item| item_number(text(item, "id").unwrap_or_default()));
            if !open.is_empty() {
                let listed: Vec<String> = open.iter().map(|item| format!("{} ({})", text(item, "id").unwrap_or_default(), text(item, "status").unwrap_or_default())).collect();
                errs.push(format!("plan {rid} is closed with {} item(s) still undecided: {}", open.len(), listed.join(", ")));
            }
        }
    }
    errs
}

/// Every commit a record cites.
pub fn commits_of(record: &Value) -> BTreeSet<String> {
    let mut commits: BTreeSet<String> = refs_list(record, "commits").into_iter().collect();
    commits.extend(strings(record.get("commits")));
    if text(record, "kind") == Some("reading") {
        if let Some(at) = text(record, "measured_at").filter(|at| !at.is_empty()) {
            commits.insert(at.to_owned());
        }
    }
    commits
}

/// Commits must exist; files must exist at the base - only for references a
/// change adds, since a file may go away after a record names it. One git
/// process for the whole change, however many records it holds.
fn ref_errors(config: &Config, pairs: &[(Value, Option<Value>)], rev: Option<&str>, roots: Option<&Roots>) -> Vec<String> {
    let mut added = Vec::new();
    for (record, old) in pairs {
        let before = old.clone().unwrap_or(Value::Object(Map::new()));
        let commits: Vec<String> = commits_of(record).difference(&commits_of(&before)).cloned().collect();
        let had: BTreeSet<String> = refs_list(&before, "files").into_iter().collect();
        let files: Vec<String> = refs_list(record, "files").into_iter().collect::<BTreeSet<_>>().difference(&had).cloned().collect();
        added.push((text(record, "id").unwrap_or_default().to_owned(), commits, files));
    }
    let all_commits: Vec<String> = added.iter().flat_map(|(_, commits, _)| commits.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let all_files: Vec<String> = added.iter().flat_map(|(_, _, files)| files.iter().map(|file| file.trim_end_matches('/').to_owned())).collect::<BTreeSet<_>>().into_iter().collect();
    let found = crate::refs::resolve(&config.root, rev, roots, &all_commits, &all_files);
    let mut errs = Vec::new();
    for (rid, commits, files) in added {
        for commit in commits {
            if !found.commits.get(&commit).copied().unwrap_or(false) {
                errs.push(crate::refs::missing_commit(&rid, &commit, roots));
            }
        }
        for file in files {
            if !found.files.get(file.trim_end_matches('/')).copied().unwrap_or(false) {
                errs.push(crate::refs::missing_file(&rid, &file, rev, roots));
            }
        }
    }
    errs
}

/// A signal is the exact text this fault prints; a bare class name or one word
/// is printed by every fault of its family. Only signals the change adds.
fn signal_errors(record: &Value, old: Option<&Value>) -> Vec<String> {
    let had: BTreeSet<String> = old.map(|old| strings(old.get("signals"))).unwrap_or_default().into_iter().collect();
    let id = record.get("id").map(|id| id.as_str().map(str::to_owned).unwrap_or_else(|| id.to_string())).unwrap_or_else(|| "None".into());
    strings(record.get("signals"))
        .into_iter()
        .filter(|signal| !had.contains(signal) && generic_signal(signal))
        .map(|signal| format!("{id}: signal {} is a bare name every fault of its kind prints - give the line it prints whole (`{signal}: <the message>`), or leave it out", repr_str(&signal)))
        .collect()
}

fn transition_errors(new: &Value, old: Option<&Value>, reopen_reason: Option<&str>) -> Vec<String> {
    let Some(old) = old else { return Vec::new() };
    if text(new, "kind") != Some("item") {
        return Vec::new();
    }
    let from = text(old, "status").unwrap_or_default();
    let to = text(new, "status").unwrap_or_default();
    if item_moves(from).contains(&to) {
        return Vec::new();
    }
    if UNDECIDED.contains(&to) && reopen_reason.is_some_and(|reason| !reason.is_empty()) {
        return Vec::new();
    }
    vec![format!("{}: an item that is `{from}` does not become `{to}` - reopen it with a reason (`lore item reopen`)", text(new, "id").unwrap_or_default())]
}

/// Fields a write sets itself; a change that touches nothing else is no change.
const STAMPS: &[&str] = &["updated", "by", "v"];

pub fn same(a: &Value, b: &Value) -> bool {
    let strip = |value: &Value| -> Map<String, Value> { value.as_object().map(|map| map.iter().filter(|(key, _)| !STAMPS.contains(&key.as_str())).map(|(key, value)| (key.clone(), value.clone())).collect()).unwrap_or_default() };
    strip(a) == strip(b)
}

fn placeable(record: &Value) -> bool {
    path_of(record, "x").is_some()
}

pub fn subject(changes: &[Change]) -> String {
    if let [change] = changes {
        let mut verb = if change.is_new { "add" } else { "update" };
        let status = text(&change.record, "status");
        let kind = text(&change.record, "kind");
        if kind == Some("item") && !change.is_new {
            verb = match status {
                Some("done") => "done",
                Some("dropped") => "drop",
                Some("superseded") => "supersede",
                Some("deferred") => "defer",
                Some("open") => "reopen",
                _ => verb,
            };
        }
        if kind == Some("plan") && status == Some("closed") {
            verb = "close";
        }
        return format!("lore: {verb} {} {}", kind.unwrap_or("None"), text(&change.record, "id").unwrap_or("None"));
    }
    let kinds: BTreeSet<&str> = changes.iter().map(|change| text(&change.record, "kind").unwrap_or("None")).collect();
    format!("lore: {} records ({})", changes.len(), kinds.into_iter().collect::<Vec<_>>().join(", "))
}

/// The line of git's complaint that says why: `fatal:` or `error:` first.
fn first_line(err: &str) -> String {
    let lines: Vec<&str> = err.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    lines.iter().find(|line| line.starts_with("fatal:") || line.starts_with("error:")).or(lines.first()).map(|line| (*line).to_owned()).unwrap_or_else(|| "no reason given".into())
}

pub fn apply(config: &Config, build: &Builder, options: &Options) -> Result<Outcome> {
    let mode = options.mode.clone().unwrap_or_else(|| config.commit_mode.clone());
    let mut done: Vec<Value> = Vec::new();
    let shared = mode == "shared-tree";
    let roots = options.roots.as_ref();
    let mut transform = |_old: &BTreeMap<String, Option<Vec<u8>>>, rev: Option<&str>| -> Result<BTreeMap<String, Vec<u8>>> {
        let snapshot = if shared { load_rev(config, rev)? } else { load_worktree(&config.root, &config.dir)? };
        let changes = build(&snapshot)?;
        let existing = snapshot.by_id();
        let date = today();
        let mut files = snapshot.files.clone();
        let mut errs: Vec<String> = Vec::new();
        let mut put: Vec<Value> = Vec::new();
        for change in &changes {
            let mut record = change.record.clone();
            let id = record.get("id").and_then(Value::as_str).map(str::to_owned);
            let old = id.as_ref().and_then(|id| existing.get(id));
            if change.is_new && old.is_some() {
                errs.push(format!("{} already exists - update it, or give the new record another id", repr_opt(record.get("id"))));
                continue;
            }
            if !change.is_new && old.is_none() {
                errs.push(format!("{} does not exist", repr_opt(record.get("id"))));
                continue;
            }
            let Some(map) = record.as_object_mut() else {
                errs.push("a record is a JSON object".into());
                continue;
            };
            map.insert("v".into(), Value::from(FORMAT_VERSION));
            map.insert("by".into(), Value::String(options.by.clone()));
            map.insert("updated".into(), Value::String(date.clone()));
            if !map.contains_key("created") {
                let created = old.and_then(|old| old.get("created")).cloned().unwrap_or_else(|| Value::String(date.clone()));
                map.insert("created".into(), created);
            }
            map.retain(|_, value| !value.is_null());
            if let Some(old) = old {
                if same(&record, old) {
                    continue; // nothing but the stamps would change: no write, no commit
                }
            }
            errs.extend(validate(&record, &config.areas, Some(&config.limits)));
            // docs/lore.md 11.9: every durable write passes the secret scan;
            // the refusal names the field and never repeats the match.
            let rid = text(&record, "id").unwrap_or_default().to_owned();
            errs.extend(crate::secrets::check_record(&record, &config.secrets_allow).iter().map(|found| format!("{rid}: {}", found.line())));
            errs.extend(transition_errors(&record, old, change.reopen_reason.as_deref()));
            if let Some(old) = old {
                if text(old, "kind") != text(&record, "kind") {
                    errs.push(format!("{}: a record does not change kind ({} -> {})", text(&record, "id").unwrap_or_default(), text(old, "kind").unwrap_or("None"), text(&record, "kind").unwrap_or("None")));
                }
                if let (Some(was), Some(now)) = (path_of(old, &config.dir), path_of(&record, &config.dir)) {
                    if was != now {
                        errs.push(format!("{}: moving a record to another area is a new record that supersedes this one", text(&record, "id").unwrap_or_default()));
                    }
                }
            }
            errs.extend(signal_errors(&record, old));
            if text(&record, "kind") == Some("source") {
                errs.extend(cover::ledger_errors(config, &record, old, rev));
            }
            put.push(record);
        }
        if errs.is_empty() {
            let pairs: Vec<(Value, Option<Value>)> = put.iter().map(|record| (record.clone(), text(record, "id").and_then(|id| existing.get(id)).cloned())).collect();
            errs.extend(ref_errors(config, &pairs, rev, roots));
        }
        if !errs.is_empty() {
            return Err(LoreError::with("refused, nothing was written", errs));
        }
        for record in &put {
            let Some(rel) = path_of(record, &config.dir) else { continue };
            let id = text(record, "id");
            let mut records: Vec<Value> = files.get(&rel).cloned().unwrap_or_default().into_iter().filter(|existing| text(existing, "id") != id).collect();
            records.push(record.clone());
            files.insert(rel, records);
        }
        let whole = Snapshot { files: files.clone() };
        let before: BTreeSet<String> = check_placement(&snapshot, &config.dir).into_iter().chain(relations(&existing)).collect();
        // Only the faults this change causes: an untouched record already in
        // the base is `lore check`'s to report.
        let errs: Vec<String> = check_placement(&whole, &config.dir).into_iter().chain(relations(&whole.by_id())).filter(|error| !before.contains(error)).collect();
        if !errs.is_empty() {
            return Err(LoreError::with("refused, nothing was written", errs));
        }
        let touched: BTreeSet<String> = put.iter().filter_map(|record| path_of(record, &config.dir)).collect();
        done = put.clone();
        Ok(touched.into_iter().map(|rel| {
            let bytes = serialize(&files[&rel]);
            (rel, bytes)
        }).collect())
    };

    let base = base_snapshot(config)?;
    let probe = build(&base)?;
    if probe.is_empty() {
        return Err(LoreError::new("nothing to write"));
    }
    // The message names what changes, not what was asked: a batch whose
    // patches mostly match what is there commits only the rest.
    let known = base.by_id();
    let probe: Vec<Change> = probe
        .into_iter()
        .filter(|change| change.is_new || text(&change.record, "id").and_then(|id| known.get(id)).is_none_or(|old| !same(&change.record, old)))
        .collect();
    if probe.is_empty() {
        return Ok(Outcome { commit: None, records: Vec::new(), notes: vec!["nothing changed: every record already says this".into()], pushed: None, push_error: None });
    }
    let paths: Vec<String> = probe.iter().filter(|change| placeable(&change.record)).filter_map(|change| path_of(&change.record, &config.dir)).collect::<BTreeSet<_>>().into_iter().collect();
    let subject_line = options.subject.clone().unwrap_or_else(|| subject(&probe));
    let body: Vec<String> = probe.iter().take(50).map(|change| format!("- {}: {}", text(&change.record, "id").unwrap_or("None"), text(&change.record, "title").unwrap_or(""))).collect();
    let body = body.join("\n");
    let mut trailers = config.trailers.clone();
    trailers.extend(options.trailers.iter().cloned());
    let identity = options.identity.as_ref();
    let written = match mode.as_str() {
        "none" => gitio::write_none(&config.root, &paths, &mut transform)?,
        "plain" => gitio::write_plain(&config.root, &paths, &mut transform, &subject_line, &body, &trailers, identity)?,
        _ => gitio::write_shared(&config.root, &config.branch, &paths, &mut transform, &subject_line, &body, &trailers, 3, None, identity)?,
    };
    let mut outcome = Outcome { commit: written.commit.clone(), records: done, notes: written.notes, pushed: None, push_error: None };
    if outcome.commit.is_some() && config.push {
        let err = gitio::push(&config.root, &config.remote, &config.branch);
        outcome.push_error = err.as_deref().map(first_line);
        outcome.pushed = Some(err.is_none());
    }
    Ok(outcome)
}

/// Gives each record without an id one: a slug of its title, or the next
/// number in its plan. Ids in the same batch count.
pub fn fill_ids(records: &[Value], snapshot: &Snapshot) -> Vec<Value> {
    let mut taken: BTreeSet<String> = snapshot.by_id().into_keys().collect();
    let mut next: BTreeMap<String, u64> = BTreeMap::new();
    for (_, record) in snapshot.records() {
        if text(record, "kind") == Some("item") {
            if let Some(plan) = text(record, "plan") {
                let number = item_number(text(record, "id").unwrap_or_default());
                let entry = next.entry(plan.to_owned()).or_insert(0);
                *entry = (*entry).max(number);
            }
        }
    }
    for record in records {
        if let Some(id) = record.get("id").filter(|id| !falsy(id)).and_then(Value::as_str) {
            taken.insert(id.to_owned());
            if text(record, "kind") == Some("item") {
                if let Some((plan, number)) = item_id(id) {
                    let entry = next.entry(plan.to_owned()).or_insert(0);
                    *entry = (*entry).max(number);
                }
            }
        }
    }
    let mut result = Vec::new();
    for record in records {
        let mut record = record.clone();
        let Some(map) = record.as_object_mut() else {
            result.push(record);
            continue;
        };
        if map.get("id").is_none_or(falsy) {
            let id = if map.get("kind").and_then(Value::as_str) == Some("item") {
                let plan = map.get("plan").and_then(Value::as_str).unwrap_or_default().to_owned();
                let number = next.entry(plan.clone()).or_insert(0);
                *number += 1;
                format!("{plan}#{number}")
            } else {
                let base = slugify(map.get("title").and_then(Value::as_str).unwrap_or_default());
                let mut candidate = base.clone();
                let mut n = 2;
                while taken.contains(&candidate) {
                    let head: String = base.chars().take(44).collect();
                    candidate = format!("{head}-{n}");
                    n += 1;
                }
                candidate
            };
            taken.insert(id.clone());
            map.insert("id".into(), Value::String(id));
        }
        if let Some(area) = map.get("kind").and_then(Value::as_str).and_then(area_of) {
            map.entry("area").or_insert(Value::String(area.into()));
        }
        result.push(record);
    }
    result
}

pub fn adder(records: Vec<Value>) -> impl Fn(&Snapshot) -> Result<Vec<Change>> {
    move |snapshot| Ok(fill_ids(&records, snapshot).into_iter().map(Change::new).collect())
}

/// `patch` merged into the record: a key set to null is removed.
pub fn patched(current: &Value, patch: &Map<String, Value>) -> Result<Value> {
    let mut record = current.clone();
    let map = record.as_object_mut().ok_or_else(|| LoreError::new("a record is a JSON object"))?;
    for (key, value) in patch {
        if ["id", "kind", "v", "created", "updated", "by"].contains(&key.as_str()) {
            if (key == "id" || key == "kind") && !value.is_null() && Some(value) != current.get(key) {
                return Err(LoreError::new(format!("{}: `{key}` of a record does not change", text(current, "id").unwrap_or("None"))));
            }
            continue;
        }
        if value.is_null() {
            map.remove(key);
        } else {
            map.insert(key.clone(), value.clone());
        }
    }
    Ok(record)
}

/// A patch, or a function of the record as the write finds it - so
/// `field+=value` appends to what the commit is built on.
pub enum Patch {
    Fixed(Map<String, Value>),
    Of(PatchFn),
}

/// A patch computed from the record as the write finds it.
pub type PatchFn = Box<dyn Fn(&Value) -> Result<Map<String, Value>>>;

pub fn updater(rid: String, patch: Patch, reopen_reason: Option<String>) -> impl Fn(&Snapshot) -> Result<Vec<Change>> {
    move |snapshot| {
        let by_id = snapshot.by_id();
        let current = by_id.get(&rid).ok_or_else(|| LoreError::new(format!("{} does not exist", repr_str(&rid))))?;
        let patch = match &patch {
            Patch::Fixed(map) => map.clone(),
            Patch::Of(function) => function(current)?,
        };
        Ok(vec![Change::update(patched(current, &patch)?, reopen_reason.clone())])
    }
}

/// Several patches, each naming its record by `id`, in one commit.
pub fn updater_many(patches: Vec<Map<String, Value>>) -> impl Fn(&Snapshot) -> Result<Vec<Change>> {
    move |snapshot| {
        let by_id = snapshot.by_id();
        let missing: Vec<String> = patches
            .iter()
            .filter_map(|patch| patch.get("id").and_then(Value::as_str))
            .filter(|id| !by_id.contains_key(*id))
            .map(|id| format!("{} does not exist", repr_str(id)))
            .collect();
        if !missing.is_empty() {
            return Err(LoreError::with("refused, nothing was written", missing));
        }
        patches.iter().map(|patch| Ok(Change::update(patched(&by_id[patch["id"].as_str().unwrap_or_default()], patch)?, None))).collect()
    }
}
