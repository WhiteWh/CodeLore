//! `lore atlas <command>`: the code map's commands, text or `--json`.
use super::store::{Atlas, Filter, Hit};
use super::{index_embedder, open_fresh, query_embedder, refresh_project, watch};
use crate::config::{self, Config};
use crate::search::{self, Filters};
use crate::LoreError;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
#[path = "calls_cli.rs"]
mod calls_cli;
#[path = "sources.rs"]
mod sources;

pub const USAGE: &str = "usage: lore atlas <command>
  status                         what the map holds, where, and whether it is on
  index [--rebuild] [--text-only]
                                 bring the map up to date and embed its pieces
  watch start|stop|status        manage background refresh for the project
  search <query...> [-n N] [--lang L] [--path P] [--text-only]
                                 the places in the code, by words, terms, symbols and meaning (`-` reads stdin)
  symbol <name>                  where a symbol is defined, which files name it, what the lore says
  symbols <file>                 a file's definitions
  calls-index --compdb PATH [--libclang PATH] [--source project|unreal|NAME]
                                 build a C++ call index for a compilation context
  calls <name|id|file:line> [--direction in|out] [--depth N] [-n N] [--candidates]
                                 navigate C++ callers and callees
  calls-status                   show call index availability and diagnostics
  deps <file>                    what a file imports, and what imports it
  impact <file|dir|symbol> [--depth N]
                                 every file a change reaches, and the lore about each
  cycles                         the import cycles
  drift                          records whose files or symbols the code no longer has
  guide                          how an agent uses the map

  status, index, search, symbol, symbols, calls and calls-status accept --source project|unreal|all
  status, index, search, symbol and symbols also accept a configured source name
  calls commands accept a configured source name to select a project + external graph
  project reads accept --snapshot to use the last completed index without refreshing";

/// What the command layer hands the atlas: the words after `atlas`, the folder
/// it runs in, stdin, and the global switches already read.
pub struct Call<'a> {
    pub argv: &'a [String],
    pub start: PathBuf,
    pub stdin: &'a [u8],
    pub json: bool,
    pub text_only: bool,
}

pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    snapshot_only: bool,
    snapshot: Option<watch::Snapshot>,
    dependencies: Option<super::dependencies::Catalogue>,
}

pub(super) struct Opts {
    pub command: String,
    pub positional: Vec<String>,
    pub options: BTreeMap<String, String>,
    pub flags: BTreeSet<String>,
}

fn parse(argv: &[String]) -> Result<Opts, String> {
    let mut opts = Opts { command: String::new(), positional: Vec::new(), options: BTreeMap::new(), flags: BTreeSet::new() };
    let mut at = 0;
    while at < argv.len() {
        let word = &argv[at];
        at += 1;
        let valued = |name: &str| matches!(name, "-n" | "--limit" | "--lang" | "--path" | "--depth" | "--source" | "--direction" | "--profile" | "--compdb" | "--libclang");
        if word.starts_with('-') && word != "-" {
            let (name, inline) = match word.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
                None => (word.clone(), None),
            };
            if valued(&name) {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        let value = argv.get(at).cloned().ok_or_else(|| format!("argument {name}: expected one argument"))?;
                        at += 1;
                        value
                    }
                };
                let key = if name == "--limit" { "-n".to_owned() } else { name };
                opts.options.insert(key, value);
            } else if matches!(name.as_str(), "--rebuild" | "--text-only" | "--json" | "--all" | "--candidates" | "--snapshot") {
                opts.flags.insert(name);
            } else {
                return Err(format!("unrecognized arguments: {word}"));
            }
            continue;
        }
        if opts.command.is_empty() {
            opts.command = word.clone();
        } else {
            opts.positional.push(word.clone());
        }
    }
    Ok(opts)
}

/// Runs one `lore atlas` command.
pub fn run(call: &Call<'_>) -> Out {
    let mut out = Out { code: 0, stdout: String::new(), stderr: String::new(), snapshot_only: false, snapshot: None, dependencies: None };
    let opts = match parse(call.argv) {
        Ok(opts) => opts,
        Err(message) => {
            out.stderr = format!("{USAGE}\nlore: error: {message}\n");
            out.code = 2;
            return out;
        }
    };
    let json = call.json || opts.flags.contains("--json");
    out.snapshot_only = watch::requested(opts.flags.contains("--snapshot"));
    if opts.command.is_empty() || opts.command == "help" {
        out.stdout = format!("{USAGE}\n");
        out.code = if opts.command.is_empty() { 2 } else { 0 };
        return out;
    }
    if opts.command == "guide" {
        out.stdout = format!("{}\n", GUIDE.trim_end());
        return out;
    }
    let config = if opts.command == "watch" && matches!(opts.positional.as_slice(), [action] if action == "status" || action == "stop") {
        watch::control_root(&call.start).map(|root| Config::defaults(&root))
    } else {
        config::load(&call.start)
    };
    let result = config.and_then(|config| {
        for warning in &config.warnings {
            out.stderr.push_str(&format!("lore: warning: {warning}\n"));
        }
        if !config.atlas.sources.is_empty() && matches!(opts.command.as_str(), "status" | "search" | "symbol" | "symbols")
            && !matches!(opts.options.get("--source").map(String::as_str), Some("project" | "unreal"))
        {
            out.dependencies = Some(watch::dependencies(&config));
        }
        dispatch(&opts, &config, call, json, &mut out)
    });
    finish(&mut out, json, result);
    out
}

/// MCP already resolved the project and validated typed tool arguments. It
/// uses the same command results while always reading the published map.
pub(super) fn run_snapshot(config: &Config, opts: &Opts, dependencies: Option<super::dependencies::Catalogue>) -> (i32, Value) {
    let mut out = Out { code: 0, stdout: String::new(), stderr: String::new(), snapshot_only: true, snapshot: None, dependencies };
    let call = Call { argv: &[], start: config.root.clone(), stdin: &[], json: true, text_only: false };
    let result = dispatch(opts, config, &call, true, &mut out);
    finish(&mut out, true, result);
    let mut payload = serde_json::from_str::<Value>(&out.stdout).unwrap_or_else(|error| json!({"error": format!("invalid Atlas result: {error}")}));
    if let Some(dependencies) = &out.dependencies {
        payload["dependencies"] = json!({"state": dependencies.state, "notes": dependencies.notes});
    }
    (out.code, payload)
}

fn finish(out: &mut Out, json: bool, result: Result<(), LoreError>) {
    if let Err(error) = result {
        if json {
            out.stdout.clear();
            emit(out, &json!({"error": error.message, "details": error.details}));
        } else {
            out.stderr.push_str(&format!("lore: {error}\n"));
        }
        out.code = 1;
    }
    if !json {
        if let Some(snapshot) = &out.snapshot {
            out.stdout.insert_str(0, &format!("{}\n", snapshot.describe()));
        }
    }
}

fn say(out: &mut Out, text: &str) {
    out.stdout.push_str(text);
    out.stdout.push('\n');
}

fn emit(out: &mut Out, value: &Value) {
    let enriched = out.snapshot.as_ref().map(|snapshot| {
        let mut value = if value.is_object() { value.clone() } else { json!({"results": value}) };
        value["snapshot"] = snapshot.value.clone();
        value
    });
    out.stdout.push_str(&serde_json::to_string_pretty(enriched.as_ref().unwrap_or(value)).unwrap_or_default());
    out.stdout.push('\n');
}

fn need(opts: &Opts, what: &str) -> Result<String, LoreError> {
    if opts.positional.is_empty() {
        return Err(LoreError::new(format!("`lore atlas {}` needs {what}", opts.command)));
    }
    Ok(opts.positional.join(" "))
}

fn number(opts: &Opts, name: &str, default: usize) -> Result<usize, LoreError> {
    match opts.options.get(name) {
        Some(value) => value.parse().map_err(|_| LoreError::new(format!("argument {name}: invalid int value: '{value}'"))),
        None => Ok(default),
    }
}

fn dispatch(opts: &Opts, config: &Config, call: &Call<'_>, json: bool, out: &mut Out) -> Result<(), LoreError> {
    if opts.command == "watch" {
        let [action] = opts.positional.as_slice() else {
            return Err(LoreError::new("use lore atlas watch start, stop or status"));
        };
        let status = watch::manage(config, action)?;
        if json {
            emit(out, &status.value);
        } else {
            say(out, &status.describe());
        }
        return Ok(());
    }
    if matches!(opts.command.as_str(), "calls" | "calls-index" | "calls-status") {
        return calls_cli::dispatch(opts, config, json, out);
    }
    let text_only = call.text_only || opts.flags.contains("--text-only");
    if sources::dispatch(opts, config, call, json, text_only, out)? {
        return Ok(());
    }
    match opts.command.as_str() {
        "status" => status(config, json, out),
        "index" => index(config, opts.flags.contains("--rebuild"), text_only, json, out),
        "search" => {
            let mut query = need(opts, "a query")?;
            if query.trim() == "-" {
                query = search::errors_of(&String::from_utf8_lossy(call.stdin));
            }
            let filter = Filter { path: opts.options.get("--path").cloned().unwrap_or_default().replace('\\', "/"), lang: opts.options.get("--lang").cloned().unwrap_or_default() };
            find(config, &query, number(opts, "-n", 5)?, &filter, text_only, json, out)
        }
        "symbol" => symbol(config, &need(opts, "a symbol name")?, json, out),
        "symbols" => {
            let (atlas, _) = open_project(config, out)?;
            let path = relative_target(config, &need(opts, "a file")?);
            let found = atlas.symbols_of(&path)?;
            if json {
                emit(out, &json!(found.iter().map(|d| json!({"name": d.name, "kind": d.kind, "line": d.line})).collect::<Vec<_>>()));
            } else if found.is_empty() {
                say(out, &format!("no definitions found in {path}"));
            } else {
                for d in found {
                    say(out, &format!("{:>6}  {:10} {}", d.line, d.kind, d.name));
                }
            }
            Ok(())
        }
        "deps" => deps(config, &need(opts, "a file")?, json, out),
        "impact" => impact(config, &need(opts, "a file, folder or symbol")?, number(opts, "--depth", 3)?, json, out),
        "cycles" => {
            let (atlas, _) = open_project(config, out)?;
            let groups = super::graph::cycles(&atlas.import_edges()?);
            if json {
                emit(out, &json!(groups));
            } else if groups.is_empty() {
                say(out, "no import cycles");
            } else {
                for (n, group) in groups.iter().enumerate() {
                    say(out, &format!("cycle {} ({} files): {}", n + 1, group.len(), group.join(" -> ")));
                }
            }
            Ok(())
        }
        "drift" => drift(config, opts.flags.contains("--all"), json, out),
        other => Err(LoreError::new(format!("unknown atlas command `{other}`; `lore atlas help` lists them"))),
    }
}

fn open_project(config: &Config, out: &mut Out) -> Result<(Atlas, super::store::Refresh), LoreError> {
    if let Some(reason) = super::off_reason(config) {
        return Err(LoreError::new(reason));
    }
    let background = watch::enabled_dir(&config.root);
    if background.is_none() && !out.snapshot_only {
        return open_fresh(config);
    }
    let mode = if background.is_some() { "background" } else { "snapshot" };
    let (atlas, snapshot) = watch::snapshot(config, background.as_deref(), mode);
    let result = atlas.map(|atlas| (atlas, super::store::Refresh::default())).ok_or_else(|| snapshot.missing());
    out.snapshot = Some(snapshot);
    result
}

fn status(config: &Config, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let off = super::off_reason(config);
    if let Some(reason) = &off {
        if json {
            emit(out, &json!({"enabled": false, "reason": reason}));
        } else {
            say(out, reason);
        }
        return Ok(());
    }
    let (atlas, refresh) = match open_project(config, out) {
        Ok(found) => found,
        Err(error) if out.snapshot.is_some() => {
            if json {
                emit(out, &json!({"enabled": true, "note": error.message}));
            } else {
                say(out, &error.message);
            }
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let counts = atlas.counts()?;
    let model = config.embedding_model();
    let vectors = atlas.vector_count(model.id());
    let space = super::store::unreal_store::VectorSpace::new(model);
    let ann = super::ann::status(&atlas, config, &space.key, model.id(), space.dimension);
    let installed = crate::embed::status_for(model, &crate::embed::model_dir(model)).available;
    if json {
        emit(out, &json!({
            "enabled": true,
            "path": atlas.path.display().to_string(),
            "counts": counts,
            "vectors": vectors,
            "model": model.id(),
            "model_installed": installed,
            "semantic": config.atlas.semantic,
            "ann": ann,
            "refreshed": {"files_read": refresh.files_read, "files_dropped": refresh.files_dropped},
        }));
        return Ok(());
    }
    say(out, &format!("atlas: {}", atlas.path.display()));
    say(out, &format!(
        "  {} files, {} pieces, {} symbols, {} imports ({} between files)",
        counts["files"], counts["chunks"], counts["symbols"], counts["imports"], counts["edges"]
    ));
    let meaning = if !config.atlas.semantic {
        "off (atlas.semantic = false)".to_owned()
    } else if !installed {
        format!("{} not installed yet; text only", model.id())
    } else {
        format!("{} of {} pieces embedded with {}", vectors, counts["chunks"], model.id())
    };
    say(out, &format!("  meaning: {meaning}"));
    say(out, &format!("  semantic search {} [{}]", ann["mode"].as_str().unwrap_or_default(), ann["state"].as_str().unwrap_or_default()));
    if let Some(error) = ann["error"].as_str() {
        say(out, &format!("  {error}"));
    }
    if refresh.files_read + refresh.files_dropped > 0 {
        say(out, &format!("  brought up to date: {} file(s) read, {} dropped", refresh.files_read, refresh.files_dropped));
    }
    Ok(())
}

fn index(config: &Config, rebuild: bool, text_only: bool, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let started = std::time::Instant::now();
    let (atlas, refresh) = refresh_project(config, rebuild)?;
    let (embedder, mut notes) = index_embedder(config, text_only);
    let model = config.embedding_model();
    let mut embedded = 0;
    if let Some(embedder) = &embedder {
        let mut last = std::time::Instant::now();
        let mut progress = |done: usize, total: usize| {
            if !json && (done == total || last.elapsed().as_secs() >= 5) {
                last = std::time::Instant::now();
                eprintln!("lore: atlas: embedded {done} / {total} pieces");
            }
        };
        match atlas.vector_batch(|_| atlas.ensure_vectors(embedder, model.id(), model.document_prefix(), None, &mut progress)) {
            Ok((done, _)) => embedded = done,
            Err(error) => notes.push(format!("code meaning unavailable: {error}; text only")),
        }
    }
    if !text_only {
        let space = super::store::unreal_store::VectorSpace::new(model);
        super::ann::schedule(&atlas, config, &space.key, model.id(), space.dimension);
    }
    let counts = atlas.counts()?;
    let seconds = started.elapsed().as_secs_f64();
    if json {
        emit(out, &json!({
            "path": atlas.path.display().to_string(),
            "files_read": refresh.files_read,
            "files_dropped": refresh.files_dropped,
            "counts": counts,
            "embedded": embedded,
            "vectors": atlas.vector_count(model.id()),
            "seconds": (seconds * 100.0).round() / 100.0,
            "notes": notes,
        }));
        return Ok(());
    }
    for note in &notes {
        say(out, &format!("note: {note}"));
    }
    say(out, &format!(
        "atlas: {} files ({} read now), {} pieces, {} symbols, {} file links; {} pieces embedded now, {} with vectors; {seconds:.1} s",
        counts["files"], refresh.files_read, counts["chunks"], counts["symbols"], counts["edges"], embedded, atlas.vector_count(model.id())
    ));
    Ok(())
}

fn preview(hit: &Hit, lines: usize) -> Vec<String> {
    hit.text.lines().filter(|line| !line.trim().is_empty()).take(lines).map(|line| format!("  | {}", line.chars().take(160).collect::<String>())).collect()
}

fn hit_json(hit: &Hit) -> Value {
    json!({"path": hit.path, "start": hit.start, "end": hit.end, "symbols": hit.symbols, "lang": hit.lang, "score": hit.score, "via": hit.via, "text": hit.text})
}

fn query_vector(result: Result<Vec<f32>, LoreError>, model: &crate::builtin::QueryModel, notes: &mut Vec<String>) -> Option<Vec<f32>> {
    if let Some(note) = model.fallback_note() {
        notes.push(note.to_owned());
    }
    match result {
        Ok(vector) => Some(vector),
        Err(error) => {
            notes.push(format!("code meaning unavailable: {error}; text only"));
            None
        }
    }
}

fn find(config: &Config, query: &str, limit: usize, filter: &Filter, text_only: bool, json: bool, out: &mut Out) -> Result<(), LoreError> {
    if query.trim().is_empty() {
        return Err(LoreError::new("nothing to search for"));
    }
    let (atlas, _) = open_project(config, out)?;
    let model = config.embedding_model();
    let snapshot = out.snapshot.is_some();
    let semantic = !text_only && config.atlas.semantic && config.embed_provider != "none";
    let mut timings = super::store::SearchTimings::default();
    let filter = atlas.prepare_filter(filter, &mut timings);
    let mut plan = if semantic && filter.is_empty() {
        super::store::SemanticPlan::text("empty_filter", None)
    } else if semantic {
        let space = super::store::unreal_store::VectorSpace::new(model);
        atlas.prepare_semantics(&space.key, model.id(), space.dimension, !snapshot, &mut timings)
    } else {
        super::store::SemanticPlan::text("disabled", None)
    };
    let (embedder, mut notes) = if text_only {
        (None, Vec::new())
    } else if !config.atlas.semantic || config.embed_provider == "none" {
        let notes = (!snapshot).then(|| "code meaning is off (atlas.semantic = false or embed.provider = \"none\"); text only".to_owned());
        (None, notes.into_iter().collect())
    } else if plan.no_vectors() {
        (None, vec!["no compatible vectors in the snapshot. Run lore atlas index to fill them.".to_owned()])
    } else if !plan.uses_vectors() {
        (None, Vec::new())
    } else {
        query_embedder(model, !snapshot)
    };
    let mut vector = None;
    let mut encoding_ms = None;
    if let Some(embedder) = &embedder {
        // A few new pieces are embedded here; many wait for `lore atlas index`.
        // A model that fails here leaves the search to the text, as an index does.
        let filled = if snapshot || !plan.exact() { Ok((0, 0)) } else {
            atlas.vector_batch(|_| atlas.ensure_vectors(embedder, model.id(), model.document_prefix(), Some(super::store::SEARCH_EMBED_BUDGET), &mut |_, _| {}))
        };
        let encoded = match filled {
            Ok((_, missing)) => {
                if missing > 0 {
                    notes.push(format!("{missing} code pieces have no vector yet; `lore atlas index` embeds them"));
                }
                let started = std::time::Instant::now();
                let encoded = embedder.embed_query(query);
                encoding_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
                encoded
            }
            Err(error) => Err(error),
        };
        vector = query_vector(encoded, embedder, &mut notes);
    }
    let meaning = plan.retrieve(&atlas, vector.as_deref(), model.id(), &filter, &mut timings);
    notes.extend(plan.note.iter().cloned());
    let hits = atlas.search_with_meaning(query, limit, &filter, meaning, &mut timings)?;
    if json {
        let mut response = json!({"query": query, "notes": notes, "results": hits.iter().map(hit_json).collect::<Vec<_>>()});
        if semantic {
            response["semantic_search"] = plan.json();
        }
        if super::store::SearchTimings::requested() {
            let mut measured = timings.json();
            measured["encoding"] = json!(encoding_ms);
            response["timings_ms"] = measured;
        }
        emit(out, &response);
        return Ok(());
    }
    for note in &notes {
        say(out, &format!("note: {note}"));
    }
    if hits.is_empty() {
        say(out, "nothing found in the code");
        return Ok(());
    }
    let mut blocks = Vec::new();
    for hit in &hits {
        let names = if hit.symbols.is_empty() { String::new() } else { format!("  {}", hit.symbols.iter().take(3).cloned().collect::<Vec<_>>().join(", ")) };
        let mut block = vec![format!("{}:{}-{}{names}  [{}]", hit.path, hit.start, hit.end, hit.via.join(", "))];
        block.extend(preview(hit, 4));
        blocks.push(block.join("\n"));
    }
    say(out, &blocks.join("\n\n"));
    Ok(())
}

/// A target as the map names it: repository-relative, forward slashes.
pub fn relative_target(config: &Config, target: &str) -> String {
    let mut wanted = target.replace('\\', "/").trim().to_owned();
    let root = format!("{}/", config.root.to_string_lossy().replace('\\', "/").trim_end_matches('/'));
    if wanted.to_lowercase().starts_with(&root.to_lowercase()) {
        wanted = wanted[root.len()..].to_owned();
    }
    wanted.trim_start_matches("./").trim_end_matches('/').to_owned()
}

pub fn list_short(items: &[String], most: usize) -> String {
    let mut text = items.iter().take(most).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > most {
        text.push_str(&format!(" and {} more", items.len() - most));
    }
    text
}

fn records(config: &Config) -> Vec<Value> {
    if config.atlas.checkout.is_some() {
        return Vec::new();
    }
    crate::store::load_worktree(&config.root, &config.dir).map(|snapshot| snapshot.all()).unwrap_or_default()
}

/// Live records about any of `files` (not those already about `skip`), each
/// with the file it was found by.
pub fn records_about(config: &Config, records: &[Value], files: &[String], skip: &[String]) -> Vec<(Value, String)> {
    let skipped: BTreeSet<String> = skip.iter().flat_map(|path| search::for_target(config, records, path, &Filters::default())).filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_owned)).collect();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for file in files {
        for record in search::for_target(config, records, file, &Filters::default()) {
            let id = record.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
            if skipped.contains(&id) || !seen.insert(id) {
                continue;
            }
            out.push((record, file.clone()));
        }
    }
    out
}

fn record_line(record: &Value) -> String {
    let field = |key: &str| record.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    format!("{} [{}] {}", field("id"), field("kind"), field("title"))
}

fn symbol(config: &Config, name: &str, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let (atlas, _) = open_project(config, out)?;
    let definitions = atlas.definitions(name)?;
    let defined_in: BTreeSet<String> = definitions.iter().map(|d| d.path.clone()).collect();
    let mentions: Vec<String> = atlas.mentions(name, 200)?.into_iter().filter(|path| !defined_in.contains(path)).collect();
    let all = records(config);
    let known = search::for_target(config, &all, name, &Filters::default());
    if json {
        emit(out, &json!({
            "symbol": name,
            "definitions": definitions.iter().map(|d| json!({"path": d.path, "line": d.line, "kind": d.kind, "name": d.name})).collect::<Vec<_>>(),
            "mentioned_in": mentions,
            "records": known,
        }));
        return Ok(());
    }
    if definitions.is_empty() {
        say(out, &format!("{name}: no definition found"));
    }
    for d in &definitions {
        say(out, &format!("{}:{}  {} {}", d.path, d.line, d.kind, d.name));
    }
    if !mentions.is_empty() {
        say(out, &format!("named in {} other file(s): {}", mentions.len(), list_short(&mentions, 10)));
    }
    if !known.is_empty() {
        say(out, "the lore about it:");
        for record in &known {
            say(out, &format!("  {}", record_line(record)));
        }
    }
    Ok(())
}

fn deps(config: &Config, target: &str, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let (atlas, _) = open_project(config, out)?;
    let path = relative_target(config, target);
    if !atlas.has_file(&path) {
        return Err(LoreError::new(format!("{path} is not in the code map")));
    }
    let (imports, imported_by) = atlas.neighbours(&path)?;
    let external = atlas.external_imports(&path)?;
    if json {
        emit(out, &json!({"path": path, "imports": imports, "imported_by": imported_by, "external": external}));
        return Ok(());
    }
    say(out, &format!("{path}"));
    say(out, &format!("  imports ({}): {}", imports.len(), if imports.is_empty() { "-".to_owned() } else { list_short(&imports, 20) }));
    say(out, &format!("  imported by ({}): {}", imported_by.len(), if imported_by.is_empty() { "-".to_owned() } else { list_short(&imported_by, 20) }));
    if !external.is_empty() {
        say(out, &format!("  outside the repository: {}", list_short(&external, 12)));
    }
    Ok(())
}

/// The files a target stands for: a file, the files under a folder, or the
/// files that define a symbol.
fn targets(atlas: &Atlas, config: &Config, target: &str) -> Result<(Vec<String>, &'static str), LoreError> {
    let path = relative_target(config, target);
    if atlas.has_file(&path) {
        return Ok((vec![path], "file"));
    }
    let under = atlas.files_under(&path)?;
    if !under.is_empty() {
        return Ok((under, "folder"));
    }
    let defined: BTreeSet<String> = atlas.definitions(target)?.into_iter().map(|d| d.path).collect();
    if !defined.is_empty() {
        return Ok((defined.into_iter().collect(), "symbol"));
    }
    Err(LoreError::new(format!("{target} is neither a file, a folder nor a symbol of the code map")))
}

fn impact(config: &Config, target: &str, depth: usize, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let (atlas, _) = open_project(config, out)?;
    let (start, what) = targets(&atlas, config, target)?;
    let reached = atlas.dependents(&start, depth.max(1))?;
    let mut by_distance: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (path, distance) in &reached {
        by_distance.entry(*distance).or_default().push(path.clone());
    }
    let all = records(config);
    let own = records_about(config, &all, &start, &[]);
    let reached_files: Vec<String> = by_distance.values().flatten().cloned().collect();
    let around = records_about(config, &all, &reached_files, &start);
    if json {
        emit(out, &json!({
            "target": target,
            "as": what,
            "files": start,
            "dependents": reached.iter().map(|(path, distance)| json!({"path": path, "distance": distance})).collect::<Vec<_>>(),
            "records": own.iter().map(|(record, file)| json!({"via": file, "record": record})).collect::<Vec<_>>(),
            "records_of_dependents": around.iter().map(|(record, file)| json!({"via": file, "record": record})).collect::<Vec<_>>(),
        }));
        return Ok(());
    }
    say(out, &format!("{target} ({what}: {})", list_short(&start, 6)));
    if reached.is_empty() {
        say(out, "  nothing in the repository imports it");
    }
    for (distance, files) in &by_distance {
        let label = if *distance == 1 { "imports it directly".to_owned() } else { format!("{distance} steps away") };
        say(out, &format!("  {label} ({}): {}", files.len(), list_short(files, 12)));
    }
    if !own.is_empty() {
        say(out, "the lore about it:");
        for (record, _) in &own {
            say(out, &format!("  {}", record_line(record)));
        }
    }
    if !around.is_empty() {
        say(out, "the lore about the files it reaches:");
        for (record, file) in around.iter().take(20) {
            say(out, &format!("  {}  (via {file})", record_line(record)));
        }
    }
    Ok(())
}

fn drift(config: &Config, all: bool, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let (atlas, _) = open_project(config, out)?;
    let records = records(config);
    let filters = Filters { all, ..Filters::default() };
    let mut rows: Vec<Value> = Vec::new();
    for record in search::visible(&records, &filters).into_values() {
        let refs = record.get("refs");
        let list = |key: &str| -> Vec<String> { refs.and_then(|refs| refs.get(key)).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect() };
        let missing_files: Vec<String> = list("files")
            .into_iter()
            .filter(|path| {
                let rel = relative_target(config, path);
                !atlas.has_file(&rel) && !Path::new(&config.root).join(&rel).exists()
            })
            .collect();
        let missing_symbols: Vec<String> = list("symbols")
            .into_iter()
            .filter(|symbol| {
                let short = symbol.rsplit(['.', ':']).next().unwrap_or(symbol);
                short.chars().all(|c| c.is_alphanumeric() || c == '_') && short.len() >= 3 && atlas.definitions_exact(symbol).map(|d| d.is_empty()).unwrap_or(false) && atlas.mentions(symbol, 1).map(|m| m.is_empty()).unwrap_or(false)
            })
            .collect();
        if missing_files.is_empty() && missing_symbols.is_empty() {
            continue;
        }
        rows.push(json!({"id": record.get("id"), "kind": record.get("kind"), "title": record.get("title"), "missing_files": missing_files, "missing_symbols": missing_symbols}));
    }
    if json {
        emit(out, &json!(rows));
        return Ok(());
    }
    if rows.is_empty() {
        say(out, "every file and symbol the live records cite is still in the code");
        return Ok(());
    }
    say(out, &format!("{} record(s) cite what the code no longer has - update, or mark obsolete:", rows.len()));
    for row in &rows {
        let text = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
        let names = |key: &str| row.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
        let mut gone = names("missing_files");
        gone.extend(names("missing_symbols"));
        say(out, &format!("  {} [{}] {}\n      gone: {}", text("id"), text("kind"), text("title"), gone.join(", ")));
    }
    Ok(())
}

pub const GUIDE: &str = "\
The code map (lore atlas) - where things are in the code now, beside what the lore learned.

- Find code by what it does: `lore atlas search \"where the index decides it is stale\"`.
  Exact names and errors work too; `-` reads a pasted log. Results are pieces of files
  (path:start-end) - read those lines, not the whole file.
- Before changing a file or a function: `lore atlas impact <file|symbol>` lists every file
  the change reaches and what the lore knows about each of them. `lore for <file>` adds
  who imports the file.
- `lore atlas symbol <name>`: where it is defined and which files name it.
- `lore atlas deps <file>`, `lore atlas cycles`: the import graph.
- After a refactor: `lore atlas drift` lists records that cite files or symbols the code no
  longer has; update them or mark them obsolete.
- The map refreshes itself from git's blob ids. With `lore atlas watch start`, a worker
  refreshes it and commands read the last completed snapshot. Check it with
  `lore atlas watch status`; `lore atlas watch stop` restores synchronous refresh.
- `lore atlas search --snapshot <query>` reads an existing snapshot without a worker.
  `lore atlas index` embeds new pieces for search by meaning.";
