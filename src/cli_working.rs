//! The working tier's commands (docs/lore.md 11.13) and `find`'s other two
//! tiers (11.6, 11.7). Shapes are additive: a working result is
//! `{"score","via","tier":"working","entry"}`, a history one
//! `{"score","via","tier":"history","hit"}`; a durable one is unchanged.
use super::*;
use crate::history::{self, HistoryHit};
use crate::promote;
use crate::working::{self, Entry, Message, Pulse, Role, Viewer};

/// A working result's fused score is halved; history's is quartered (11.6, 11.7).
const WORKING_WEIGHT: f64 = 0.5;
const HISTORY_WEIGHT: f64 = 0.25;
const HISTORY_MAX: usize = 3;

fn now(ctx: &Ctx) -> i64 {
    ctx.env.now.unwrap_or_else(|| chrono::Utc::now().timestamp())
}

fn viewer(ctx: &Ctx) -> Result<Viewer> {
    let role = match ctx.args.one("role").as_deref() {
        None | Some("owner") => Role::Owner,
        Some("lead") => Role::Lead,
        Some("member") => Role::Member,
        Some(other) => return Err(LoreError::new(format!("argument --role: invalid choice: {} (choose from 'member', 'lead', 'owner')", repr_str(other)))),
    };
    Ok(Viewer { team: ctx.args.one("team"), name: ctx.by(), role })
}

fn team(ctx: &Ctx) -> String {
    ctx.args.one("team").unwrap_or_default()
}

fn store(ctx: &Ctx) -> Result<working::Store> {
    working::Store::open(&ctx.config.root, &ctx.config.working, &ctx.config.secrets_allow)
}

/// The store only when it exists: a read never creates it.
fn existing(ctx: &Ctx) -> Result<Option<working::Store>> {
    let path = ctx.config.root.join(working::PRIVATE_DIR).join(working::FILE_NAME);
    if !path.is_file() {
        return Ok(None);
    }
    working::Store::open_at(&path, &ctx.config.working, &ctx.config.secrets_allow).map(Some)
}

/// Task 187 (11.4: a read never waits for the cleanup): a read only takes
/// the note of a failed cleanup; expired entries are filtered by every read
/// anyway, and the next write runs the pass that is due.
fn read_notes(store: &working::Store) -> Vec<String> {
    store.take_cleanup_note().into_iter().collect()
}

/// One cleanup pass when one is due (at open, then every 10 minutes), before
/// a write; its failure is a note, never an error to the caller. It never
/// waits for another pass (the working lock is only tried).
fn tidy(ctx: &mut Ctx, store: &working::Store) -> Vec<String> {
    let at = now(ctx);
    if !store.cleanup_due(at) {
        return Vec::new();
    }
    let records = ctx.records().unwrap_or_default();
    let plans = working::PlanClock::from_records(&records);
    let clock: &dyn working::TaskClock = ctx.env.clock.unwrap_or(&plans);
    let _ = store.cleanup(at, clock);
    store.take_cleanup_note().into_iter().collect()
}

/// `in 13d`, `in 5h`, `in 20m`, `expired`; `while open` for none.
fn relative(expires: Option<i64>, now: i64) -> String {
    let Some(at) = expires else { return "while open".into() };
    let left = at - now;
    if left <= 0 {
        "expired".into()
    } else if left >= 86_400 {
        format!("in {}d", left / 86_400)
    } else if left >= 3_600 {
        format!("in {}h", left / 3_600)
    } else {
        format!("in {}m", (left / 60).max(1))
    }
}

fn fmt_entry(entry: &Entry, now: i64, score: Option<f64>) -> String {
    let score = score.map(|score| format!("  score {}", crate::py::float_str(score))).unwrap_or_default();
    let mut lines = vec![format!("[working {}] {}  by {}  (expires {}){score}", entry.kind, entry.id, entry.by, relative(entry.expires, now))];
    match entry.kind.as_str() {
        "pulse" => {
            if !entry.task.is_empty() {
                lines[0].push_str(&format!("  task {}", entry.task));
            }
            for (name, value) in [("done", &entry.done), ("next", &entry.next), ("blocked", &entry.blocked)] {
                if !value.is_empty() {
                    lines.push(format!("  {name}: {value}"));
                }
            }
        }
        "message" => {
            let urgency = if entry.urgency == "urgent" { "  URGENT" } else { "" };
            lines[0].push_str(&format!("  to {}  {}{urgency}", entry.to.clone().unwrap_or_default(), entry.state));
            lines.push(format!("  {}", entry.text));
        }
        _ => {
            lines[0].push_str(&format!("  task {}  version {}", entry.task, entry.version));
            lines.push(format!("  {}", entry.text.lines().next().unwrap_or_default()));
        }
    }
    lines.join("\n")
}

fn fmt_hit(hit: &HistoryHit, score: f64) -> String {
    format!("[history] {}  message {}  {}  score {}\n  {}", hit.session, hit.message_id, hit.role, crate::py::float_str(score), hit.excerpt)
}

/// `find`'s answer across the tiers, when there is more than the durable one.
pub(super) struct Merged {
    pub json: Value,
    pub text: Vec<String>,
    pub notes: Vec<String>,
}

/// The durable results with the working tier's and history's below them
/// (11.6). None when neither tier takes part: CodeLore's own answer stands.
pub(super) fn tiers(ctx: &mut Ctx, query: &str, durable: &[search::Found], notes: &[String], limit: usize) -> Result<Option<Merged>> {
    let only_working = ctx.args.flag("working");
    let only_history = ctx.args.flag("history");
    let want_working = !ctx.args.flag("no-working") && !only_history;
    let want_history = !ctx.args.flag("no-history") && !only_working && (ctx.env.history.is_some() || only_history);
    let store = if want_working { existing(ctx)? } else { None };
    let at = now(ctx);
    let mut notes: Vec<String> = notes.to_vec();
    let mut working: Vec<(Entry, f64, Vec<String>)> = Vec::new();
    if let Some(store) = &store {
        notes.extend(read_notes(store));
        working = store.search(query, &viewer(ctx)?, at)?;
    }
    let mut history: Vec<HistoryHit> = Vec::new();
    if want_history {
        let (hits, note) = history::federate(ctx.env.history.as_ref(), query, &ctx.env.history_scope);
        notes.extend(note);
        history = hits;
    }
    if working.is_empty() && history.is_empty() && !only_working && !only_history {
        return Ok(None);
    }
    // (score, tier order, position, item): durable first on a tie.
    #[allow(clippy::large_enum_variant)]
    enum Item<'a> {
        Durable(&'a search::Found),
        Working(Entry, Vec<String>),
        History(HistoryHit),
    }
    let mut rows: Vec<(f64, u8, usize, Item)> = Vec::new();
    if !only_working && !only_history {
        for (n, found) in durable.iter().enumerate() {
            rows.push((found.score, 0, n, Item::Durable(found)));
        }
    }
    if !only_history {
        let cap = if only_working { limit } else { limit.div_ceil(2) };
        for (n, (entry, score, via)) in working.into_iter().take(cap).enumerate() {
            let mut via = via;
            via.insert(0, "working".into());
            rows.push(((score * WORKING_WEIGHT * 1e5).round() / 1e5, 1, n, Item::Working(entry, via)));
        }
    }
    if !only_working {
        for (n, hit) in history.into_iter().take(HISTORY_MAX).enumerate() {
            let score = 1.0 / (crate::finder::RRF_K + n as f64 + 1.0) * HISTORY_WEIGHT;
            rows.push(((score * 1e5).round() / 1e5, 2, n, Item::History(hit)));
        }
    }
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    rows.truncate(limit);
    let mut results = Vec::new();
    let mut text = Vec::new();
    for (score, _, _, item) in rows {
        match item {
            Item::Durable(found) => {
                let result = durable_result(found, at);
                text.push(format!("{}\n{}", fmt_record(&found.record, Some(found.score), &found.via), confidence_line(&result["confidence"])));
                results.push(result);
            }
            Item::Working(entry, via) => {
                let age = Some(((at - entry.at) / 86_400).max(0));
                let confidence = search::confidence("working", &via, None, age, !entry.promoted.is_empty());
                text.push(format!("{}\n{}", fmt_entry(&entry, at, Some(score)), confidence_line(&confidence)));
                results.push(json!({"score": score, "via": via, "tier": "working", "entry": entry.json(), "confidence": confidence}));
            }
            Item::History(hit) => {
                let age = Some(((at - hit.at as i64) / 86_400).max(0));
                let confidence = search::confidence("history", &["history".to_owned()], None, age, false);
                text.push(format!("{}\n{}", fmt_hit(&hit, score), confidence_line(&confidence)));
                results.push(json!({"score": score, "via": ["history"], "tier": "history", "hit": hit.json(), "confidence": confidence}));
            }
        }
    }
    let json = json!({"query": query, "notes": notes, "results": results});
    Ok(Some(Merged { json, text, notes }))
}

/// `lore add --occurrence` (11.10): a same fault becomes an occurrence on the
/// record already there, a differing one is written and its conflict named.
pub(super) fn add_resolved(ctx: &mut Ctx, io: &mut Io, records: Vec<Value>) -> Result<i32> {
    let [record]: [Value; 1] = records.try_into().map_err(|_| LoreError::new("--occurrence writes one record at a time"))?;
    let by = ctx.by();
    let options = ops::Options { by: by.clone(), trailers: ctx.args.trailers.clone(), mode: ctx.args.mode.clone(), roots: ctx.env.roots.clone(), identity: private::identity(&ctx.config.root, &by), ..ops::Options::default() };
    let promoted = promote::write_resolved(&ctx.config, record, &options, "lore add")?;
    say_promoted(ctx, io, &promoted);
    Ok(0)
}

fn say_promoted(ctx: &Ctx, io: &mut Io, promoted: &promote::Promoted) {
    if ctx.args.json {
        io.json(&promoted.json());
        return;
    }
    match (&promoted.occurrence_of, promoted.existing) {
        (_, true) => io.say(&format!("already promoted to {}", promoted.record)),
        (Some(id), _) => io.say(&format!("an occurrence of {id}: one more line in its `seen`")),
        (None, _) => io.say(&format!("wrote {}", promoted.record)),
    }
    if let Some(commit) = &promoted.commit {
        io.say(&format!("committed {}", commit.chars().take(10).collect::<String>()));
    }
    for note in &promoted.notes {
        io.note(note);
    }
}

fn say_entry(ctx: &Ctx, io: &mut Io, entry: &Entry, notes: &[String]) {
    if ctx.args.json {
        let mut value = entry.json();
        value["notes"] = json!(notes);
        io.json(&value);
        return;
    }
    io.say(&fmt_entry(entry, now(ctx), None));
    for note in notes {
        io.note(note);
    }
}

fn say_list(ctx: &Ctx, io: &mut Io, entries: &[Entry], notes: &[String]) {
    if ctx.args.json {
        io.json(&json!({"entries": entries.iter().map(Entry::json).collect::<Vec<_>>(), "notes": notes}));
        return;
    }
    for note in notes {
        io.note(note);
    }
    if entries.is_empty() {
        io.say("nothing here");
        return;
    }
    let at = now(ctx);
    io.say(&entries.iter().map(|entry| fmt_entry(entry, at, None)).collect::<Vec<_>>().join("\n\n"));
}

/// `--since`: UTC seconds or a `YYYY-MM-DD` day.
fn since(text: &str) -> Result<i64> {
    if let Ok(seconds) = text.parse::<i64>() {
        return Ok(seconds);
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|time| time.and_utc().timestamp())
        .ok_or_else(|| LoreError::new(format!("argument --since: {} is neither seconds nor YYYY-MM-DD", repr_str(text))))
}

/// The durable brief file of a task (11.12): `briefs/t<task>.md`.
pub fn brief_path(task: &str) -> String {
    let name: String = task.trim_start_matches('#').chars().map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '-' }).collect();
    format!("briefs/t{name}.md")
}

fn brief_file(title: &str, text: &str) -> Vec<u8> {
    format!("# Brief: {title}\n\n{}\n", text.trim_end()).into_bytes()
}

/// The text of a brief file, without its title line.
fn brief_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    if lines.clone().next().is_some_and(|line| line.starts_with("# Brief:")) {
        lines.next();
    }
    lines.collect::<Vec<_>>().join("\n").trim().to_owned()
}

/// Commits the latest brief under the writer lock, as the configured mode does.
fn commit_brief(ctx: &Ctx, path: &str, bytes: Vec<u8>, trailer: String) -> Result<Option<String>> {
    let by = ctx.by();
    // The writer lock lives in the private folder, never as a file git sees.
    let _lock = crate::writer::Writer::acquire(&working::private_dir(&ctx.config.root)?, &by)?;
    let mut transform = |_old: &BTreeMap<String, Option<Vec<u8>>>, _rev: Option<&str>| -> Result<BTreeMap<String, Vec<u8>>> {
        Ok(BTreeMap::from([(path.to_owned(), bytes.clone())]))
    };
    let paths = vec![path.to_owned()];
    let subject = format!("lore: brief {path}");
    let mut trailers = ctx.config.trailers.clone();
    trailers.push(trailer);
    let identity = private::identity(&ctx.config.root, &by);
    let mode = ctx.args.mode.clone().unwrap_or_else(|| ctx.config.commit_mode.clone());
    let written = match mode.as_str() {
        "none" => gitio::write_none(&ctx.config.root, &paths, &mut transform)?,
        "plain" => gitio::write_plain(&ctx.config.root, &paths, &mut transform, &subject, "", &trailers, identity.as_ref())?,
        _ => gitio::write_shared(&ctx.config.root, &ctx.config.branch, &paths, &mut transform, &subject, "", &trailers, 3, None, identity.as_ref())?,
    };
    Ok(written.commit)
}

pub(super) fn run(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let at = now(ctx);
    match ctx.args.cmd().as_str() {
        "pulse add" => {
            let store = store(ctx)?;
            let mut notes = tidy(ctx, &store);
            let pulse = Pulse {
                team: team(ctx),
                by: ctx.by(),
                task: ctx.args.one("task").unwrap_or_default(),
                done: ctx.args.one("done").unwrap_or_default(),
                next: ctx.args.one("next").unwrap_or_default(),
                blocked: ctx.args.one("blocked").unwrap_or_default(),
            };
            let written = store.pulse(&pulse, at)?;
            notes.extend(written.notes);
            say_entry(ctx, io, &written.entry, &notes);
        }
        "pulses" => {
            let Some(store) = existing(ctx)? else {
                say_list(ctx, io, &[], &[]);
                return Ok(0);
            };
            let notes = read_notes(&store);
            let since = ctx.args.one("since").map(|text| since(&text)).transpose()?;
            let by = ctx.args.by.clone();
            let task = ctx.args.one("task");
            let list = store.pulses(&viewer(ctx)?, task.as_deref(), by.as_deref(), since, ctx.args.flag("all"), at)?;
            say_list(ctx, io, &list, &notes);
        }
        "message send" => {
            let mut text = ctx.args.positional.join(" ");
            if text.trim().is_empty() || text.trim() == "-" {
                text = stdin_text(ctx.env)?.trim_end().to_owned();
            }
            let to = ctx.args.one("to").ok_or_else(|| LoreError::new("the following arguments are required: --to"))?;
            let store = store(ctx)?;
            let mut notes = tidy(ctx, &store);
            let recipients: Vec<String> = ctx.args.one("recipients").map(|list| list.split(',').map(str::trim).filter(|name| !name.is_empty()).map(str::to_owned).collect()).unwrap_or_default();
            let message = Message { team: team(ctx), by: ctx.by(), to, text, urgent: ctx.args.flag("urgent"), recipients };
            let written = store.send(&message, ctx.env.doorbell, at)?;
            notes.extend(written.notes);
            if ctx.args.json {
                io.json(&json!({"id": written.entry.id, "state": written.entry.state, "urgency": written.entry.urgency, "notes": notes}));
            } else {
                io.say(&format!("sent {} ({}, {})", written.entry.id, written.entry.urgency, written.entry.state));
                for note in &notes {
                    io.note(note);
                }
            }
        }
        "messages" => {
            let Some(store) = existing(ctx)? else {
                say_list(ctx, io, &[], &[]);
                return Ok(0);
            };
            let notes = read_notes(&store);
            let reader = viewer(ctx)?;
            let (to, from) = (ctx.args.one("to"), ctx.args.one("from"));
            let list = store.messages(&reader, ctx.args.flag("unread"), to.as_deref(), from.as_deref(), at)?;
            if !ctx.args.flag("peek") {
                for entry in &list {
                    let addressed = matches!(entry.to.as_deref(), Some(to) if to == reader.name || to == "team")
                        || (reader.role != Role::Member && matches!(entry.to.as_deref(), Some("lead" | "owner")));
                    if addressed {
                        store.mark_read(&entry.id, &reader.name, at)?;
                    }
                }
            }
            say_list(ctx, io, &list, &notes);
        }
        "message read" => {
            let store = store(ctx)?;
            let reader = viewer(ctx)?;
            let entry = store.mark_read(&ctx.args.positional[0].clone(), &reader.name, at)?;
            say_entry(ctx, io, &entry, &[]);
        }
        "brief set" => {
            let task = ctx.args.one("task").ok_or_else(|| LoreError::new("the following arguments are required: --task"))?;
            let text = stdin_text(ctx.env)?.trim_end().to_owned();
            // The durable brief refuses a secret; the previous file and
            // working row stay as they are (11.12).
            crate::secrets::guard(&json!({"text": text}), &ctx.config.secrets_allow).map_err(|refusal| {
                LoreError::with(format!("{}; rewrite the brief", refusal.message), refusal.details)
            })?;
            if text.len() > working::BRIEF_BYTES {
                return Err(LoreError::new(format!("refused: the brief has {} bytes; a brief holds at most 8 KiB - name a longer subject as a document the brief points to, or split it into topics", text.len())));
            }
            let store = store(ctx)?;
            let mut notes = tidy(ctx, &store);
            // Task 187 (11.12): everything the working row would refuse is
            // checked first; only then is the durable file committed.
            store.check_brief(&task, &text, at)?;
            let team = team(ctx);
            let title = ctx.args.one("title").unwrap_or_else(|| task.clone());
            let version = store.brief_of(&team, &task, None, at)?.map(|entry| entry.version).unwrap_or(0) + 1;
            let path = brief_path(&task);
            let trailer = format!("Zerus-Brief: {}/t{}/v{version}", if team.is_empty() { "-" } else { &team }, task.trim_start_matches('#'));
            let commit = commit_brief(ctx, &path, brief_file(&title, &text), trailer)?;
            let written = store.brief(&team, &task, &ctx.by(), &text, at)?;
            notes.extend(written.notes);
            if let Some(commit) = commit {
                notes.push(format!("{path} committed {}", commit.chars().take(10).collect::<String>()));
            }
            say_entry(ctx, io, &written.entry, &notes);
        }
        "brief show" => {
            let task = ctx.args.one("task").ok_or_else(|| LoreError::new("the following arguments are required: --task"))?;
            let version = match ctx.args.one("version") {
                Some(text) => Some(text.parse::<i64>().map_err(|_| LoreError::new(format!("argument --version: invalid int value: {}", repr_str(&text))))?),
                None => None,
            };
            let file = ctx.config.root.join(brief_path(&task));
            let from_file = if version.is_none() { std::fs::read(&file).ok().map(|bytes| brief_text(&bytes)) } else { None };
            let row = match existing(ctx)? {
                Some(store) => store.brief_of(&team(ctx), &task, version, at)?,
                None => None,
            };
            let text = from_file.clone().or_else(|| row.as_ref().map(|entry| entry.text.clone()));
            let Some(text) = text else { return Err(LoreError::new(format!("no brief for {task}"))) };
            if ctx.args.json {
                io.json(&json!({"task": task, "version": row.as_ref().map(|entry| entry.version), "text": text,
                    "source": if from_file.is_some() { "file" } else { "working" }}));
            } else {
                io.say(&text);
            }
        }
        "promote" => return cmd_promote(ctx, io),
        "supersede" => {
            let (old, new) = (ctx.args.positional[0].clone(), ctx.args.positional[1].clone());
            let by = ctx.by();
            let options = ops::Options { by: by.clone(), trailers: ctx.args.trailers.clone(), mode: ctx.args.mode.clone(), roots: ctx.env.roots.clone(), identity: private::identity(&ctx.config.root, &by), ..ops::Options::default() };
            let outcome = promote::supersede(&ctx.config, &old, &new, &options)?;
            if ctx.args.json {
                io.json(&shape_write(&outcome, None));
            } else {
                io.say(&format!("{old} is superseded by {new}"));
                if let Some(commit) = &outcome.commit {
                    io.say(&format!("committed {}", commit.chars().take(10).collect::<String>()));
                }
            }
        }
        "working status" => {
            let Some(store) = existing(ctx)? else {
                let value = json!({"counts": {"pulse": 0, "message": 0, "brief": 0}, "bytes": 0, "oldest": null, "last_cleanup": null, "history": ctx.env.history.is_some()});
                if ctx.args.json { io.json(&value) } else { io.say("no working entries") }
                return Ok(0);
            };
            let mut value = store.status(at)?;
            value["history"] = json!(ctx.env.history.is_some());
            if ctx.args.json {
                io.json(&value);
            } else {
                let counts = &value["counts"];
                io.say(&format!("{} pulses, {} messages, {} briefs; {} bytes", counts["pulse"], counts["message"], counts["brief"], value["bytes"]));
                io.say(&format!("history source: {}", if ctx.env.history.is_some() { "registered" } else { "none" }));
            }
        }
        "working cleanup" => {
            let store = store(ctx)?;
            let records = ctx.records().unwrap_or_default();
            let plans = working::PlanClock::from_records(&records);
            let clock: &dyn working::TaskClock = ctx.env.clock.unwrap_or(&plans);
            let cleaned = store.cleanup(at, clock)?;
            let value = json!({"deleted": cleaned.deleted, "bytes_freed": cleaned.bytes_freed, "more": cleaned.more, "busy": cleaned.busy, "events": cleaned.events});
            if ctx.args.json {
                io.json(&value);
            } else if cleaned.busy {
                io.say("another cleanup is running");
            } else {
                io.say(&format!("deleted {} entries, {} bytes freed{}", cleaned.deleted, cleaned.bytes_freed, if cleaned.more { "; more waits for the next pass" } else { "" }));
            }
        }
        other => return Err(LoreError::new(format!("unknown command {other}"))),
    }
    Ok(0)
}

/// Where a history excerpt goes in each kind's text (11.8).
fn quote_field(kind: &str) -> &'static str {
    match kind {
        "fault" => "symptom",
        "decision" => "reason",
        "reading" => "what",
        "recipe" => "when",
        _ => "body",
    }
}

fn cmd_promote(ctx: &mut Ctx, io: &mut Io) -> Result<i32> {
    let id = ctx.args.positional[0].clone();
    let at = now(ctx);
    let fields = parse_sets(&ctx.args.many("field"), None)?;
    let kind = ctx.args.one("kind");
    let by = ctx.by();
    let options = ops::Options { by: by.clone(), trailers: ctx.args.trailers.clone(), mode: ctx.args.mode.clone(), roots: ctx.env.roots.clone(), identity: private::identity(&ctx.config.root, &by), ..ops::Options::default() };
    if let Some(reference) = id.strip_prefix("history:") {
        // A history excerpt: found again through the provider, never stored.
        let (_, message) = reference.rsplit_once('/').ok_or_else(|| LoreError::new("a history hit is `history:<session>/<message>`"))?;
        let query = ctx.args.one("query").ok_or_else(|| LoreError::new("promoting a history hit needs the --query that found it"))?;
        let (hits, note) = history::federate(ctx.env.history.as_ref(), &query, &ctx.env.history_scope);
        let hit = hits.into_iter().find(|hit| hit.message_id == message).ok_or_else(|| LoreError::new(format!("the history hit {id} is not found{}", note.map(|note| format!(" ({note})")).unwrap_or_default())))?;
        let kind = kind.ok_or_else(|| LoreError::new("a history excerpt needs --kind"))?;
        let mut draft = Map::new();
        draft.insert("kind".into(), json!(kind));
        let spec = schema::kind(&kind).ok_or_else(|| LoreError::new(format!("argument --kind: invalid choice: {}", repr_str(&kind))))?;
        draft.insert("status".into(), json!(spec.statuses[0]));
        if ctx.config.areas.len() == 1 {
            draft.insert("area".into(), json!(ctx.config.areas[0]));
        }
        draft.insert("title".into(), json!(hit.excerpt.chars().take(120).collect::<String>()));
        draft.insert(quote_field(&kind).into(), json!(promote::quote(&hit)));
        draft.extend(fields);
        let draft = Value::Object(draft);
        if ctx.args.flag("dry-run") {
            return dry_run(ctx, io, &draft);
        }
        let errs = promote::check_draft(&ctx.config, &draft);
        if !errs.is_empty() {
            return Err(LoreError::with("refused, nothing was written", errs));
        }
        let mut options = options;
        options.trailers.push(format!("Zerus-Promoted: {id}"));
        let promoted = promote::write_resolved(&ctx.config, draft, &options, &format!("promoted {id}"))?;
        say_promoted(ctx, io, &promoted);
        return Ok(0);
    }
    let store = store(ctx)?;
    let entry = store.get(&id, at)?.ok_or_else(|| LoreError::new(format!("{} does not exist, or it expired", repr_str(&id))))?;
    let reader = viewer(ctx)?;
    if reader.role == Role::Member && entry.by != reader.name {
        return Err(LoreError::new(format!("{id} is {}'s; a member promotes its own entries (the lead or the owner promotes any)", entry.by)));
    }
    if reader.team.as_deref().is_some_and(|team| team != entry.team) {
        return Err(LoreError::new(format!("{id} is not this team's")));
    }
    let draft = promote::draft(&entry, kind.as_deref(), &fields, &ctx.config.areas, None)?;
    if ctx.args.flag("dry-run") {
        return dry_run(ctx, io, &draft);
    }
    // Task 187: the entry stays protected from the cleanup until its
    // promotion is recorded on it; a crash after the durable write leaves the
    // record, which a retry finds (`existing`) and records then.
    store.reading(&id, at);
    let result = promote::promote(&ctx.config, &entry, draft, &options).and_then(|promoted| {
        if !promoted.existing {
            store.set_promoted(&id, &promoted.record)?;
        }
        Ok(promoted)
    });
    store.done_reading(&id);
    let promoted = result?;
    say_promoted(ctx, io, &promoted);
    Ok(0)
}

/// `--dry-run`: the draft, the duplicate candidates and every refusal;
/// nothing is written.
fn dry_run(ctx: &mut Ctx, io: &mut Io, draft: &Value) -> Result<i32> {
    let records = ctx.records()?;
    let candidates: Vec<Value> = dedupe::candidates(&ctx.config, &records, std::slice::from_ref(draft), None)
        .remove(&0)
        .unwrap_or_default()
        .into_iter()
        .map(|m| json!({"id": m.id, "how": m.how, "similarity": m.similarity, "blocking": m.blocking}))
        .collect();
    let refusals = promote::check_draft(&ctx.config, draft);
    if ctx.args.json {
        io.json(&json!({"draft": draft, "candidates": candidates, "refusals": refusals, "written": false}));
    } else {
        io.say(&pretty(draft));
        for candidate in &candidates {
            io.say(&format!("candidate {} ({} {})", text_of(&candidate["id"]), text_of(&candidate["how"]), candidate["similarity"]));
        }
        for refusal in &refusals {
            io.say(&format!("refused: {refusal}"));
        }
        io.note("dry run: nothing was written");
    }
    Ok(0)
}
