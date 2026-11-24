//! The CLI view of the project map and its optional, shared engine map.
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::atlas::ann;
use crate::atlas::dependencies;
use crate::atlas::store::dependency_store;
use crate::atlas::store::unreal_store::{EngineRefresh, VectorSpace, VectorWork};
use crate::atlas::store::{Atlas, Candidate, Definition, SearchTimings, SemanticPlan};
use crate::atlas::unreal::Unreal;

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Selection<'a> {
    Project,
    Unreal,
    Dependencies,
    Named(&'a str),
    All,
}

impl Selection<'_> {
    fn project(self) -> bool {
        matches!(self, Self::Project | Self::All)
    }

    fn unreal(self, config: &Config) -> bool {
        self == Self::Unreal || self == Self::All && config.atlas.unreal_root.is_some()
    }

    fn dependencies(self) -> bool {
        matches!(self, Self::Dependencies | Self::Named(_) | Self::All)
    }
}

struct Mounted {
    atlas: Atlas,
    origin: Origin,
}

enum Origin {
    Project(PathBuf),
    Unreal(Unreal),
    Dependency(dependencies::Source),
}

impl Origin {
    fn root(&self) -> &Path {
        match self {
            Self::Project(root) => root,
            Self::Unreal(source) => &source.root,
            Self::Dependency(source) => &source.canonical_path,
        }
    }

    fn engine(&self) -> bool {
        matches!(self, Self::Unreal(_))
    }

    fn contains_file(&self, path: &str) -> bool {
        match self {
            Self::Project(_) => true,
            Self::Unreal(source) => source.contains_file(path),
            Self::Dependency(_) => true, // membership is the pinned database
        }
    }

    fn source_json(&self) -> Value {
        match self {
            Self::Project(root) => json!({"source": "project", "root": root, "source_id": format!("project:{}", crate::index::repo_key(root))}),
            Self::Unreal(source) => source.source_json(),
            Self::Dependency(source) => source.status(),
        }
    }

    fn location_json(&self, path: &str, output: &mut Value) {
        let source = self.source_json();
        for field in ["source", "source_id", "root", "build"] {
            if let Some(value) = source.get(field) {
                output[field] = value.clone();
            }
        }
        if let Some(name) = source.get("name") {
            output["source_name"] = name.clone();
        }
        output["absolute_path"] = json!(config::simplify(&self.root().join(path)));
    }

    fn location(&self, path: &str, line: usize) -> String {
        match self {
            Self::Project(_) => format!("[project] {path}:{line}"),
            Self::Unreal(source) => format!("[{}] {}:{line}", source.label(), config::simplify(&source.root.join(path)).display()),
            Self::Dependency(source) => format!("[{}] {}:{line}", source.name.as_deref().unwrap_or("dependency"), source.canonical_path.join(path).display()),
        }
    }

    fn relative(&self, target: &str) -> Option<String> {
        let path = Path::new(target);
        let path = if path.is_absolute() {
            let path = if self.engine() { config::simplify(&std::fs::canonicalize(path).ok()?) } else { config::simplify(path) };
            let root = config::simplify(self.root());
            path.strip_prefix(root).ok()?.to_path_buf()
        } else {
            path.to_path_buf()
        };
        Some(path.to_str()?.replace('\\', "/").trim_start_matches("./").to_owned())
    }

    fn definition_json(&self, definition: &Definition) -> Value {
        let mut result = json!({"name": definition.name, "kind": definition.kind, "line": definition.line, "path": definition.path});
        self.location_json(&definition.path, &mut result);
        result
    }
}

impl Mounted {
    fn snapshot(&self, space: &VectorSpace) -> Result<Value, LoreError> {
        self.snapshot_with(space, false)
    }

    fn snapshot_status(&self, config: &Config, space: &VectorSpace, legacy: bool) -> Result<Value, LoreError> {
        let mut state = self.snapshot_with(space, legacy)?;
        let model = state["vector_space"].as_str().unwrap_or(&space.key);
        state["ann"] = ann::status(&self.atlas, config, &space.key, model, space.dimension);
        Ok(state)
    }

    fn snapshot_with(&self, space: &VectorSpace, legacy_project: bool) -> Result<Value, LoreError> {
        let mut state = self.origin.source_json();
        state["state"] = json!("ready");
        state["path"] = json!(self.atlas.path);
        state["counts"] = json!(self.atlas.counts()?);
        let (key, vectors) = if legacy_project && matches!(self.origin, Origin::Project(_)) {
            let key = space.model.id();
            (key, space.coverage_for(&self.atlas, key)?)
        } else {
            (space.key.as_str(), space.coverage(&self.atlas)?)
        };
        state["vectors"] = json!(vectors);
        state["vector_space"] = json!(key);
        state["refreshed_at"] = json!(self.atlas.meta("refreshed"));
        if self.origin.engine() {
            state["cache_hit"] = json!(true);
            state["files_seen"] = json!(0);
            state["revision"] = json!(self.atlas.meta("revision").and_then(|value| value.parse::<u64>().ok()));
            state["refresh"] = json!("explicit");
        }
        if let Origin::Dependency(source) = &self.origin {
            state["revision"] = json!(self.atlas.meta("revision").and_then(|value| value.parse::<u64>().ok()));
            if source.name.is_some() {
                state["model"] = json!(space.model.id());
                state["vectors_missing"] = json!(state["counts"]["chunks"].as_u64().unwrap_or_default().saturating_sub(vectors as u64));
            }
            let counts = state["counts"]["files"].as_u64().unwrap_or_default();
            if counts == 0 {
                state["state"] = json!("unavailable");
            }
            state["implementation"] = json!(source.implementation_at(&self.atlas));
        }
        Ok(state)
    }
}

pub(super) fn dispatch(opts: &Opts, config: &Config, call: &Call<'_>, json: bool, text_only: bool, out: &mut Out) -> Result<bool, LoreError> {
    let requested = opts.options.get("--source").map(String::as_str);
    if !matches!(opts.command.as_str(), "status" | "index" | "search" | "symbol" | "symbols") {
        if requested.is_some() {
            return Err(LoreError::new(format!("atlas {} does not accept --source", opts.command)));
        }
        return Ok(false);
    }
    let rebuild = opts.flags.contains("--rebuild");
    let observed = out.dependencies.as_ref().is_some_and(|catalogue| {
        if opts.command == "status" {
            !catalogue.sources.is_empty()
        } else {
            catalogue.sources.iter().any(dependencies::Source::ready)
        }
    });
    let selection = match requested {
        Some("project") => Selection::Project,
        Some("unreal") => Selection::Unreal,
        Some("all") => Selection::All,
        Some("dependencies") if out.dependencies.is_some() => Selection::Dependencies,
        Some(name) if (opts.command == "index" || out.dependencies.is_some()) && config.atlas.sources.contains_key(name) => Selection::Named(name),
        Some(other) => {
            let expected = if config.atlas.sources.is_empty() { "project, unreal or all" }
                else { "project, unreal, all or a configured source name" };
            return Err(LoreError::new(format!("invalid Atlas source {other:?}; use {expected}")));
        }
        None if config.atlas.unreal_root.is_some() && !(opts.command == "index" && rebuild) => Selection::All,
        None if observed => Selection::All,
        None => Selection::Project,
    };
    let filtered_symbol = opts.command == "symbol" && (opts.options.contains_key("--lang") || opts.options.contains_key("--path"));
    if config.atlas.unreal_root.is_none() && selection.project() && !filtered_symbol && !observed {
        return Ok(false);
    }
    if let Some(reason) = crate::atlas::off_reason(config) {
        if opts.command == "status" {
            status(config, json, out)?;
            return Ok(true);
        }
        return Err(LoreError::new(reason));
    }
    let space = VectorSpace::new(config.embedding_model());
    match opts.command.as_str() {
        "index" if matches!(selection, Selection::Named(_)) => {
            let Selection::Named(name) = selection else { unreachable!() };
            index_named(config, name, &space, text_only, rebuild, json, out)?;
        }
        "index" => index_sources(config, selection, &space, text_only, rebuild, json, out)?,
        "status" => status_sources(config, selection, &space, json, out)?,
        _ => query_sources(config, selection, &space, opts, call, text_only, json, out)?,
    }
    Ok(true)
}

fn unavailable(source: Option<&Unreal>, config: &Config, state: &str, note: impl std::fmt::Display) -> Value {
    let mut value = source.map(Unreal::source_json).unwrap_or_else(|| json!({"source": "unreal", "root": config.atlas.unreal_root}));
    value["state"] = json!(state);
    value["note"] = json!(note.to_string());
    value["cache_hit"] = json!(false);
    value["files_seen"] = json!(0);
    value
}

fn resolve_engine(config: &Config) -> Result<Unreal, Value> {
    let Some(root) = &config.atlas.unreal_root else {
        return Err(unavailable(None, config, "unconfigured", "set atlas.unreal.root in lore.toml"));
    };
    Unreal::resolve(root).map_err(|error| unavailable(None, config, "unavailable", error))
}

fn mounted_engine(config: &Config) -> Result<Mounted, Value> {
    let source = resolve_engine(config)?;
    let atlas = source.open_ready().map_err(|error| unavailable(Some(&source), config, "unavailable", error))?
        .ok_or_else(|| unavailable(Some(&source), config, "unindexed", "run lore atlas index --source unreal"))?;
    Ok(Mounted { atlas, origin: Origin::Unreal(source) })
}

fn mounted_dependencies(out: &Out, selection: Selection<'_>) -> (Vec<Mounted>, Vec<Value>) {
    let mut maps = Vec::new();
    let mut states = Vec::new();
    if let Some(catalogue) = &out.dependencies {
        for source in &catalogue.sources {
            if let Selection::Named(name) = selection {
                if source.name.as_deref() != Some(name) {
                    continue;
                }
            }
            match source.snapshot() {
                Ok(Some(atlas)) => maps.push(Mounted { atlas, origin: Origin::Dependency(source.clone()) }),
                Ok(None) => states.push(source.status()),
                Err(error) => {
                    let mut state = source.status();
                    state["state"] = json!("unavailable");
                    state["note"] = json!(error.message);
                    states.push(state);
                }
            }
        }
    }
    (maps, states)
}

fn describe(out: &mut Out, state: &Value) {
    let source = state["source"].as_str().unwrap_or("source");
    if let Some(note) = state["note"].as_str() {
        say(out, &format!("{source}: {note}"));
        return;
    }
    let label = if source == "unreal" {
        let build = &state["build"];
        format!("UE {}.{}.{} CL {}", build["MajorVersion"], build["MinorVersion"], build["PatchVersion"], build["Changelist"])
    } else {
        state["name"].as_str().unwrap_or(source).to_owned()
    };
    say(out, &format!(
        "{label}: {} files, {} pieces, {} symbols, {} with vectors ({})",
        state["counts"]["files"], state["counts"]["chunks"], state["counts"]["symbols"], state["vectors"], state["path"].as_str().unwrap_or_default()
    ));
    if let Some(read) = state["files_read"].as_u64() {
        say(out, &format!("  {read} files read for indexing, {} vectors generated", state["vectors_generated"]));
    }
    if let Some(model) = state["model"].as_str() {
        let note = state["model_note"].as_str().map_or(String::new(), |note| format!(" ({note})"));
        say(out, &format!("  model {model}, {} pieces missing compatible vectors{note}", state["vectors_missing"]));
    }
    if let Some(generated) = state["vectors_generated"].as_u64().filter(|_| state["name"].is_string()) {
        say(out, &format!("  {generated} vectors generated, {} reused", state["vector_hits"]));
    }
    if let Some(device) = state["embedding_device"].as_str() {
        say(out, &format!("  embedding device: {device}"));
    }
    if let Some(mode) = state["ann"]["mode"].as_str() {
        say(out, &format!("  semantic search {mode} [{}]", state["ann"]["state"].as_str().unwrap_or_default()));
        if let Some(error) = state["ann"]["error"].as_str() {
            say(out, &format!("  {error}"));
        }
    }
}

fn named_model_status(config: &Config, space: &VectorSpace, states: &mut [Value]) {
    let named = states.iter_mut().filter(|state| state["name"].is_string());
    let mut installed = None;
    for state in named {
        let local = installed.get_or_insert_with(|| crate::embed::status_for(space.model, &crate::embed::model_dir(space.model)));
        state["model"] = json!(space.model.id());
        state["model_installed"] = json!(local.available);
        state["semantic"] = json!(config.atlas.semantic && config.embed_provider != "none");
        if !local.available {
            state["model_note"] = json!(local.reason);
        }
    }
}

fn status_sources(config: &Config, selection: Selection<'_>, space: &VectorSpace, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let mut states = Vec::new();
    let mut summary = json!({"enabled": true});
    if selection.project() {
        match open_project(config, out) {
            Ok((atlas, refresh)) => {
                let project = Mounted { atlas, origin: Origin::Project(config.root.clone()) };
                let mut state = project.snapshot_status(config, space, config.atlas.unreal_root.is_none() && out.dependencies.is_some())?;
                state["refreshed"] = json!({"files_read": refresh.files_read, "files_dropped": refresh.files_dropped});
                let model = config.embedding_model();
                summary = json!({
                    "enabled": true, "path": state["path"], "counts": state["counts"], "vectors": state["vectors"], "model": model.id(),
                    "model_installed": crate::embed::status_for(model, &crate::embed::model_dir(model)).available,
                    "semantic": config.atlas.semantic, "refreshed": state["refreshed"], "ann": state["ann"],
                });
                states.push(state);
            }
            Err(error) if out.snapshot.is_some() => states.push(project_issue(config, out, &error)),
            Err(error) => return Err(error),
        }
    }
    if selection.unreal(config) {
        let state = match mounted_engine(config) {
            Ok(mounted) => mounted.snapshot_status(config, space, false).unwrap_or_else(|error| {
                let Origin::Unreal(source) = &mounted.origin else { unreachable!() };
                unavailable(Some(source), config, "unavailable", error)
            }),
            Err(state) => state,
        };
        states.push(state);
    }
    if selection.dependencies() {
        let (maps, pending) = mounted_dependencies(out, selection);
        states.extend(pending);
        for mounted in maps {
            states.push(mounted.snapshot_status(config, space, false).unwrap_or_else(|error| {
                let mut state = mounted.origin.source_json();
                state["state"] = json!("unavailable");
                state["note"] = json!(error.message);
                state
            }));
        }
    }
    named_model_status(config, space, &mut states);
    if json {
        summary["sources"] = json!(states);
        emit(out, &summary);
    } else {
        for state in &states {
            describe(out, state);
        }
    }
    Ok(())
}

struct NamedModel {
    model: crate::embed::EmbeddingModel,
    opened: OnceLock<Result<NamedEncoder, crate::embed::Error>>,
}

enum NamedEncoder {
    Query(crate::builtin::QueryModel),
    #[cfg(feature = "directml")]
    Batch(crate::embed::Embedder),
}

impl NamedEncoder {
    fn open(model: crate::embed::EmbeddingModel) -> Result<Self, crate::embed::Error> {
        #[cfg(feature = "directml")]
        if crate::embed::models::gpu_wanted() {
            return crate::embed::Embedder::open_model(model, crate::embed::ModelSource::Cache).map(Self::Batch);
        }
        crate::builtin::open_query_model(model).map(Self::Query)
    }

    fn embedder(&self) -> &dyn crate::embed::Embeddings {
        match self {
            Self::Query(model) => model,
            #[cfg(feature = "directml")]
            Self::Batch(model) => model,
        }
    }

    fn report(&self, notes: &mut Vec<String>) -> String {
        match self {
            Self::Query(model) => {
                notes.extend(model.fallback_note().map(str::to_owned));
                model.encoding_device()
            }
            #[cfg(feature = "directml")]
            Self::Batch(model) => {
                if model.fell_back_to_cpu() {
                    notes.push("GPU inference switched to CPU. See stderr for the runtime error".to_owned());
                    return "CPU (GPU inference fallback)".to_owned();
                }
                if !model.device().is_gpu() {
                    notes.extend(crate::embed::directml_fallback().map(|reason| format!("DirectML initialization failed: {reason}")));
                }
                model.device().label()
            }
        }
    }
}

impl crate::embed::Embeddings for NamedModel {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LoreError> {
        let result = self.opened.get_or_init(|| NamedEncoder::open(self.model));
        let model = result.as_ref().map_err(|error| LoreError::new(error.to_string()))?;
        model.embedder().embed(texts)
    }

    fn dimensions(&self) -> usize {
        self.model.dimensions()
    }
}

fn index_named(config: &Config, name: &str, space: &VectorSpace, text_only: bool, rebuild: bool, json: bool, out: &mut Out) -> Result<(), LoreError> {
    if rebuild {
        return Err(LoreError::new("configured sources use the background watcher for text refresh. Omit --rebuild"));
    }
    let started = std::time::Instant::now();
    let profile = &config.atlas.sources[name];
    let source = dependencies::named::resolve(config, name, profile)?;
    let model = NamedModel { model: space.model, opened: OnceLock::new() };
    let mut notes = Vec::new();
    let mut work = VectorWork::default();
    let mut published = None;
    if !text_only {
        if !config.atlas.semantic || config.embed_provider == "none" {
            notes.push("code meaning is off (atlas.semantic = false or embed.provider = \"none\"); text only".to_owned());
        } else {
            let mut last = started;
            match dependency_store::fill_named_vectors(&source, profile.max_kib * 1024, space, &model, |total| {
                if !json && last.elapsed().as_secs() >= 5 {
                    eprintln!("lore: atlas: {total} vectors generated for {name}");
                    last = std::time::Instant::now();
                }
            }) {
                Ok((atlas, done)) => {
                    published = Some(atlas);
                    work = done;
                }
                Err(error) => {
                    out.code = 1;
                    notes.push(format!("{name}: {error}. No new vectors were published"));
                }
            }
        }
    }
    let device = model.opened.get().and_then(|result| result.as_ref().ok()).map(|encoder| encoder.report(&mut notes));
    if matches!(model.opened.get(), Some(Err(_))) {
        notes.push(format!("code meaning unavailable. Run lore model fetch for {} and retry", space.model.id()));
    }
    // Read back a successful publication before releasing the writer. A
    // fresh reader can race with WAL teardown immediately after close.
    let snapshot = match published {
        Some(atlas) => Some(atlas),
        None => dependency_store::open_ready(&source)?,
    };
    let mut state = match snapshot {
        Some(atlas) => {
            if !text_only && out.code == 0 {
                ann::schedule(&atlas, config, &space.key, &space.key, space.dimension);
            }
            Mounted { atlas, origin: Origin::Dependency(source) }.snapshot(space)?
        }
        None => {
            out.code = 1;
            let mut state = source.status();
            state["state"] = json!("unindexed");
            state["note"] = json!("Waiting for a ready text snapshot. Run lore atlas watch start and check lore atlas status.");
            state
        }
    };
    state["vectors_generated"] = json!(work.generated);
    state["vector_hits"] = json!(work.hits);
    if let Some(device) = device.filter(|_| work.generated > 0) {
        state["embedding_device"] = json!(device);
    }
    named_model_status(config, space, std::slice::from_mut(&mut state));
    if json {
        emit(out, &json!({"sources": [state], "notes": notes, "seconds": started.elapsed().as_secs_f64()}));
    } else {
        describe(out, &state);
        for note in notes {
            say(out, &format!("note: {note}"));
        }
    }
    Ok(())
}

fn index_sources(config: &Config, selection: Selection<'_>, space: &VectorSpace, text_only: bool, rebuild: bool, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let started = std::time::Instant::now();
    let mut states = Vec::new();
    let mut summary = json!({});
    let mut notes = Vec::new();
    let mut model = None;
    let mut fill = |atlas: &Atlas, project: bool| {
        let selected = model.get_or_insert_with(|| {
            let (loaded, diagnostics) = index_embedder(config, text_only);
            notes.extend(diagnostics);
            loaded
        });
        let Some(model) = selected else { return VectorWork::default() };
        let mut last = std::time::Instant::now();
        let mut generated = 0;
        let mut fill = || space.fill(atlas, model, |total| {
            generated = total;
            if !json && last.elapsed().as_secs() >= 5 {
                eprintln!("lore: atlas: {total} vectors generated for {}", atlas.path.display());
                last = std::time::Instant::now();
            }
        });
        let result = if project { atlas.vector_batch(|_| fill()) } else { fill() };
        result.unwrap_or_else(|error| {
            notes.push(format!("{}: meaning unavailable: {error}; text index is ready", atlas.path.display()));
            VectorWork { generated: if project { 0 } else { generated }, ..VectorWork::default() }
        })
    };
    if selection.project() {
        let (atlas, refresh) = refresh_project(config, rebuild)?;
        let project = Mounted { atlas, origin: Origin::Project(config.root.clone()) };
        let work = fill(&project.atlas, true);
        if !text_only {
            ann::schedule(&project.atlas, config, &space.key, &space.key, space.dimension);
        }
        let mut state = project.snapshot(space)?;
        state["files_read"] = json!(refresh.files_read);
        state["files_dropped"] = json!(refresh.files_dropped);
        state["vectors_generated"] = json!(work.generated);
        state["vector_hits"] = json!(work.hits);
        summary = json!({
            "path": state["path"], "counts": state["counts"], "files_read": refresh.files_read, "files_dropped": refresh.files_dropped,
            "embedded": work.generated, "vectors": state["vectors"],
        });
        states.push(state);
    }
    if selection.unreal(config) {
        let state = match resolve_engine(config) {
            Ok(source) => {
                let result = (|| {
                    let mut writer = source.writer()?;
                    let refresh = writer.refresh(&source, rebuild)?;
                    let work = fill(&writer.atlas, false);
                    source.verify_build()?;
                    if !text_only {
                        ann::schedule(&writer.atlas, config, &space.key, &space.key, space.dimension);
                    }
                    let atlas = source.open_ready()?.ok_or_else(|| LoreError::new("Unreal index was not published"))?;
                    let snapshot = Mounted { atlas, origin: Origin::Unreal(source.clone()) }.snapshot(space)?;
                    Ok::<_, LoreError>(refresh_summary(snapshot, refresh, work))
                })();
                result.unwrap_or_else(|error| unavailable(Some(&source), config, "unavailable", error))
            }
            Err(state) => state,
        };
        if state["state"] != "ready" {
            notes.push(note(&state));
            out.code = 1;
        }
        states.push(state);
    }
    if json {
        summary["sources"] = json!(states);
        summary["notes"] = json!(notes);
        summary["seconds"] = json!(started.elapsed().as_secs_f64());
        emit(out, &summary);
    } else {
        for state in &states {
            describe(out, state);
        }
        for note in notes {
            say(out, &format!("note: {note}"));
        }
    }
    Ok(())
}

fn refresh_summary(mut state: Value, refresh: EngineRefresh, work: VectorWork) -> Value {
    state["cache_hit"] = json!(refresh.cache_hit);
    state["files_seen"] = json!(refresh.files_seen);
    state["files_hashed"] = json!(refresh.files_hashed);
    state["files_read"] = json!(refresh.files_read);
    state["files_dropped"] = json!(refresh.files_dropped);
    state["skipped"] = json!(refresh.skipped);
    state["vectors_generated"] = json!(work.generated);
    state["vector_hits"] = json!(work.hits);
    state
}

fn note(state: &Value) -> String {
    format!("Unreal source {}: {}", state["state"].as_str().unwrap_or_default(), state["note"].as_str().unwrap_or_default())
}

fn project_issue(config: &Config, out: &Out, error: &LoreError) -> Value {
    let mut state = Origin::Project(config.root.clone()).source_json();
    state["state"] = out.snapshot.as_ref().map_or(Value::Null, |snapshot| snapshot.value["state"].clone());
    state["note"] = json!(error.message);
    state
}

struct QueryMaps<'a> {
    config: &'a Config,
    maps: Vec<Mounted>,
    states: Vec<Value>,
    notes: Vec<String>,
    legacy_project: bool,
}

impl QueryMaps<'_> {
    fn engine_issue(&mut self, selection: Selection<'_>, state: Value) -> Result<(), LoreError> {
        if selection == Selection::Unreal {
            return Err(LoreError::new(note(&state)));
        }
        self.notes.push(note(&state));
        self.states.push(state);
        Ok(())
    }

    fn order_states(&mut self) {
        self.states.sort_by_key(|state| state["source"] == "unreal");
    }

    fn map_issue(&mut self, selection: Selection<'_>, origin: &Origin, error: LoreError) -> Result<(), LoreError> {
        match origin {
            Origin::Project(_) => Err(error),
            Origin::Unreal(source) => self.engine_issue(selection, unavailable(Some(source), self.config, "unavailable", error)),
            Origin::Dependency(source) => {
                let mut state = source.status();
                state["state"] = json!("unavailable");
                state["note"] = json!(error.message);
                self.states.push(state);
                Ok(())
            }
        }
    }
}

fn query_sources(config: &Config, selection: Selection<'_>, space: &VectorSpace, opts: &Opts, call: &Call<'_>, text_only: bool, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let mut sources = QueryMaps { config, maps: Vec::new(), states: Vec::new(), notes: Vec::new(),
        legacy_project: config.atlas.unreal_root.is_none() && out.dependencies.is_some() };
    if selection.project() {
        match open_project(config, out) {
            Ok((atlas, _)) => sources.maps.push(Mounted { atlas, origin: Origin::Project(config.root.clone()) }),
            Err(error) if selection == Selection::All && out.snapshot.is_some() => {
                sources.states.push(project_issue(config, out, &error));
                sources.notes.push(error.message);
            }
            Err(error) => return Err(error),
        }
    }
    if selection.unreal(config) {
        match mounted_engine(config) {
            Ok(mounted) => sources.maps.push(mounted),
            Err(state) => sources.engine_issue(selection, state)?,
        }
    }
    if selection.dependencies() {
        let (maps, pending) = mounted_dependencies(out, selection);
        sources.maps.extend(maps);
        sources.states.extend(pending);
    }
    if sources.maps.is_empty() && out.snapshot.is_some() {
        out.code = 1;
    }
    let filter = Filter { path: opts.options.get("--path").cloned().unwrap_or_default().replace('\\', "/"), lang: opts.options.get("--lang").cloned().unwrap_or_default() };
    let target = need(opts, if opts.command == "search" { "a query" } else if opts.command == "symbol" { "a symbol name" } else { "a file" })?;
    if opts.command == "search" {
        let query = if target.trim() == "-" { search::errors_of(&String::from_utf8_lossy(call.stdin)) } else { target };
        if query.trim().is_empty() {
            return Err(LoreError::new("nothing to search for"));
        }
        let limit = number(opts, "-n", 5)?;
        run_search(sources, selection, space, &query, &filter, limit, text_only, json, out)
    } else {
        run_definitions(sources, selection, space, &target, &filter, opts.command == "symbols", json, out)
    }
}

fn run_search(mut sources: QueryMaps<'_>, selection: Selection<'_>, space: &VectorSpace, query: &str, filter: &Filter, limit: usize, text_only: bool, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let mut hits = Vec::new();
    let mut maps = Vec::new();
    let semantic = !text_only && sources.config.atlas.semantic && sources.config.embed_provider != "none";
    let measured = json && SearchTimings::requested();
    for mounted in std::mem::take(&mut sources.maps) {
        match mounted.snapshot_with(space, sources.legacy_project) {
            Ok(state) => {
                let mut timings = SearchTimings::default();
                let filter = mounted.atlas.prepare_filter(filter, &mut timings);
                let key = state["vector_space"].as_str().unwrap_or(&space.key);
                let plan = if semantic && filter.is_empty() {
                    SemanticPlan::text("empty_filter", None)
                } else if semantic {
                    mounted.atlas.prepare_semantics(&space.key, key, space.dimension, false, &mut timings)
                } else {
                    SemanticPlan::text("disabled", None)
                };
                maps.push((mounted, state, plan, timings, filter));
            }
            Err(error) => sources.map_issue(selection, &mounted.origin, error)?,
        }
    }
    let mut vector = None;
    let mut encoding_ms = None;
    if semantic {
        if maps.iter().any(|(_, _, plan, _, _)| plan.uses_vectors()) {
            let (model, diagnostics) = query_embedder(space.model, !out.snapshot_only && out.snapshot.is_none());
            sources.notes.extend(diagnostics);
            if let Some(model) = model {
                let started = std::time::Instant::now();
                vector = query_vector(model.embed_query(query), &model, &mut sources.notes);
                encoding_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        for (mounted, state, _, _, _) in &maps {
            if state["vectors"].as_u64().unwrap_or(0) < state["counts"]["chunks"].as_u64().unwrap_or(0) {
                let command = match &mounted.origin {
                    Origin::Dependency(source) => source.name.as_ref().map(|name| format!("lore atlas index --source {name}")),
                    _ => Some("lore atlas index".to_owned()),
                };
                if let Some(command) = command {
                    sources.notes.push(format!("{}: partial or missing compatible vectors; {command} fills them", state["name"].as_str().or_else(|| state["source"].as_str()).unwrap_or_default()));
                }
            }
        }
    }
    for (i, (mounted, state, plan, timings, filter)) in maps.iter_mut().enumerate() {
        let key = state["vector_space"].as_str().unwrap_or(&space.key);
        let neighbors = plan.retrieve(&mounted.atlas, vector.as_deref(), key, filter, timings);
        if let Some(note) = &plan.note {
            let label = state["name"].as_str().or_else(|| state["source"].as_str()).unwrap_or_default();
            sources.notes.push(format!("{label}: {note}"));
        }
        let found = mounted.atlas.candidates(query, filter, neighbors, timings).and_then(|found| {
            if let Origin::Unreal(source) = &mounted.origin {
                source.verify_build()?;
            }
            Ok(found)
        });
        match found {
            Ok(found) => {
                let mut state = state.clone();
                state["semantic_used"] = json!(found.iter().any(|candidate| candidate.hit.via.iter().any(|via| via == "meaning")));
                if semantic {
                    state["semantic_search"] = plan.json();
                }
                if measured {
                    state["timings_ms"] = timings.json();
                }
                sources.states.push(state);
                hits.extend(found.into_iter().map(|candidate| (i, candidate)));
            }
            Err(error) => sources.map_issue(selection, &mounted.origin, error)?,
        }
    }
    hits.sort_by(|(ia, a), (ib, b)| {
        b.exact.cmp(&a.exact).then_with(|| b.hit.score.total_cmp(&a.hit.score))
            .then(ia.cmp(ib)).then_with(|| a.hit.path.cmp(&b.hit.path)).then(a.hit.start.cmp(&b.hit.start)).then(a.hit.end.cmp(&b.hit.end))
    });
    let mut per_file = HashMap::new();
    let mut locations = BTreeSet::new();
    let mut missing = BTreeSet::new();
    let mut results = Vec::new();
    let mut lines = Vec::new();
    let mut returned = 0;
    for (i, Candidate { mut hit, .. }) in hits {
        if returned == limit {
            break;
        }
        let origin = &maps[i].0.origin;
        let count = per_file.entry((i, hit.path.clone())).or_insert(0);
        if *count >= 2 || missing.contains(&hit.path) && origin.engine() {
            continue;
        }
        if *count == 0 && !origin.contains_file(&hit.path) {
            missing.insert(hit.path);
            continue;
        }
        if out.dependencies.is_some() && !locations.insert((dependencies::path_key(&origin.root().join(&hit.path)), hit.start, hit.end)) {
            continue;
        }
        *count += 1;
        returned += 1;
        if json {
            hit.score = (hit.score * 100_000.0).round() / 100_000.0;
            let mut value = hit_json(&hit);
            origin.location_json(&hit.path, &mut value);
            results.push(value);
        } else {
            let names = if hit.symbols.is_empty() { String::new() } else { format!("  {}", hit.symbols.iter().take(3).cloned().collect::<Vec<_>>().join(", ")) };
            let mut block = vec![format!("{}-{}{names}  [{}]", origin.location(&hit.path, hit.start), hit.end, hit.via.join(", "))];
            block.extend(preview(&hit, 4));
            lines.push(block.join("\n"));
        }
    }
    for path in missing {
        sources.notes.push(format!("Unreal file {path} is no longer available; run lore atlas index --source unreal"));
    }
    if json {
        sources.order_states();
        let mut response = json!({"query": query, "notes": sources.notes, "sources": sources.states, "results": results});
        if measured {
            response["timings_ms"] = json!({"encoding": encoding_ms});
        }
        emit(out, &response);
    } else {
        print_notes(out, &sources.notes);
        if lines.is_empty() {
            say(out, "nothing found in the code");
        } else {
            say(out, &lines.join("\n\n"));
        }
    }
    Ok(())
}

fn run_definitions(mut sources: QueryMaps<'_>, selection: Selection<'_>, space: &VectorSpace, target: &str, filter: &Filter, file: bool, json: bool, out: &mut Out) -> Result<(), LoreError> {
    let mut definitions = Vec::new();
    let mut lines = Vec::new();
    let mut mentions = Vec::new();
    let mut known = Vec::new();
    let mut locations = BTreeSet::new();
    for mounted in std::mem::take(&mut sources.maps) {
        let found = (|| {
            let found = if file {
                match &mounted.origin {
                    Origin::Project(_) => mounted.atlas.symbols_of(&relative_target(sources.config, target))?,
                    Origin::Unreal(_) | Origin::Dependency(_) => match mounted.origin.relative(target) {
                        Some(path) => mounted.atlas.symbols_of(&path)?,
                        None => Vec::new(),
                    },
                }
            } else {
                mounted.atlas.definitions_filtered(target, filter)?
            };
            if let Origin::Unreal(source) = &mounted.origin {
                source.verify_build()?;
            }
            Ok::<_, LoreError>((found, mounted.snapshot_with(space, sources.legacy_project)?))
        })();
        let (found, state) = match found {
            Ok(found) => found,
            Err(error) => {
                sources.map_issue(selection, &mounted.origin, error)?;
                continue;
            }
        };
        sources.states.push(state);
        if !file && matches!(mounted.origin, Origin::Project(_)) {
            let defined_in: BTreeSet<_> = found.iter().map(|item| &item.path).collect();
            mentions = mounted.atlas.mentions(target, 200)?.into_iter().filter(|path| !defined_in.contains(path)).collect();
            known = search::for_target(sources.config, &records(sources.config), target, &Filters::default());
        }
        for definition in found {
            if !mounted.origin.contains_file(&definition.path) {
                sources.notes.push(format!("Unreal file {} is no longer available; run lore atlas index --source unreal", definition.path));
                continue;
            }
            if out.dependencies.is_some() && !locations.insert((
                dependencies::path_key(&mounted.origin.root().join(&definition.path)), definition.line, definition.name.clone(), definition.kind.clone(),
            )) {
                continue;
            }
            if json {
                definitions.push(mounted.origin.definition_json(&definition));
            } else {
                lines.push(format!("{}  {} {}", mounted.origin.location(&definition.path, definition.line), definition.kind, definition.name));
            }
        }
    }
    if json {
        sources.order_states();
        let mut result = json!({"definitions": definitions, "notes": sources.notes, "sources": sources.states});
        if !file {
            result["symbol"] = json!(target);
            result["mentioned_in"] = json!(mentions);
            result["records"] = json!(known);
        }
        emit(out, &result);
    } else {
        print_notes(out, &sources.notes);
        if lines.is_empty() {
            say(out, &format!("{target}: no definition found"));
        }
        for line in lines {
            say(out, &line);
        }
        if !mentions.is_empty() {
            say(out, &format!("named in {} other project file(s): {}", mentions.len(), list_short(&mentions, 10)));
        }
        for record in &known {
            say(out, &format!("  {}", record_line(record)));
        }
    }
    Ok(())
}

fn print_notes(out: &mut Out, notes: &[String]) {
    for note in notes {
        say(out, &format!("note: {note}"));
    }
}
