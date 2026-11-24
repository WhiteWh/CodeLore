//! Local MCP tools over newline-delimited JSON-RPC on stdio.
use super::{cli, watch};
use crate::{config::{self, Config}, LoreError};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const ROOTS_TIMEOUT: Duration = Duration::from_secs(1);

/// MCP activation is local to Atlas and the watcher it launched. A project's
/// file selection and embedding settings still come from its own config.
pub(super) fn project_config(root: &Path) -> Result<Config, LoreError> {
    let checkout = checkout_boundary(root).filter(|_| root.canonicalize()
        .is_ok_and(|current| config::simplify(&current) == root))
        .ok_or_else(|| LoreError::new(format!("{} is no longer inside a Git checkout", root.display())))?;
    let mut config = if checkout.join(config::CONFIG_NAME).try_exists()
        .map_err(|error| LoreError::new(format!("{}: {error}", checkout.display())))?
    {
        config::load_at(checkout)?
    } else {
        Config::defaults(checkout)
    };
    if checkout != root {
        config.root = root.to_path_buf();
        config.atlas.checkout = Some(checkout.to_path_buf());
        // A workspace scoped to a folder contains only that folder's code.
        config.atlas.unreal_root = None;
    }
    config.atlas.enabled = true;
    Ok(config)
}

fn checkout_boundary(path: &Path) -> Option<&Path> {
    path.ancestors().find(|dir| dir.join(".git").exists())
}

pub(crate) fn run(pinned: Option<&Path>) -> i32 {
    let result = (|| {
        let cwd = std::env::current_dir()?;
        let (send, read) = mpsc::channel();
        // A host may advertise roots and never answer. Keep stdin reads
        // independent of the timeout for that request.
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if !line.is_empty() && send.send(serde_json::from_str::<Value>(&line)).is_err() {
                    break;
                }
            }
        });
        let mut stdio = Stdio {
            read, deferred: VecDeque::new(), write: io::BufWriter::new(io::stdout().lock()), next_id: 0,
        };
        let mut session = Session {
            pinned: pinned.map(|path| cwd.join(path)), cwd, projects: BTreeMap::new(),
            protocol: None, initialized: false, roots: false, roots_unresponsive: false,
        };
        while let Some(message) = stdio.recv() {
            match message {
                Ok(message) => session.handle(message, &mut stdio)?,
                Err(error) => stdio.error(&Value::Null, -32700, &error.to_string())?,
            }
        }
        Ok::<_, io::Error>(())
    })();
    if let Err(error) = result {
        if error.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("lore mcp: {error}");
            return 1;
        }
    }
    0
}

type Frame = serde_json::Result<Value>;

struct Stdio<W> {
    read: mpsc::Receiver<Frame>,
    deferred: VecDeque<Frame>,
    write: W,
    next_id: u64,
}

impl<W: Write> Stdio<W> {
    fn recv(&mut self) -> Option<Frame> {
        self.deferred.pop_front().or_else(|| self.read.recv().ok())
    }

    fn send(&mut self, message: Value) -> io::Result<()> {
        serde_json::to_writer(&mut self.write, &message)?;
        self.write.write_all(b"\n")?;
        self.write.flush()
    }

    fn result(&mut self, id: &Value, result: Value) -> io::Result<()> {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn error(&mut self, id: &Value, code: i32, message: &str) -> io::Result<()> {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
    }

    fn roots(&mut self) -> io::Result<Option<Value>> {
        self.next_id += 1;
        let id = json!(format!("atlas-roots-{}", self.next_id));
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "roots/list"}))?;
        let deadline = Instant::now() + ROOTS_TIMEOUT;
        while let Ok(frame) = self.read.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if let Ok(message) = &frame {
                if message["jsonrpc"] == "2.0" && message["id"] == id && message.get("method").is_none() {
                    return Ok(message["result"].get("roots").filter(|roots| roots.is_array()).cloned());
                }
            }
            self.deferred.push_back(frame);
            if Instant::now() >= deadline {
                break;
            }
        }
        Ok(None)
    }
}

struct Session {
    cwd: PathBuf,
    pinned: Option<PathBuf>,
    projects: BTreeMap<PathBuf, PathBuf>,
    protocol: Option<&'static str>,
    initialized: bool,
    roots: bool,
    roots_unresponsive: bool,
}

impl Session {
    fn handle(&mut self, message: Value, stdio: &mut Stdio<impl Write>) -> io::Result<()> {
        let id = &message["id"];
        if !message.is_object() || message["jsonrpc"] != "2.0" {
            return stdio.error(&Value::Null, -32600, "invalid JSON-RPC request");
        }
        let Some(method) = message["method"].as_str() else {
            // A late reply to our roots request has no client method.
            if message.get("result").is_some() || message.get("error").is_some() {
                return Ok(());
            }
            return stdio.error(&Value::Null, -32600, "missing JSON-RPC method");
        };
        if message.get("id").is_none() {
            match method {
                "notifications/initialized" if self.protocol.is_some() => self.initialized = true,
                "notifications/roots/list_changed" => self.roots_unresponsive = false,
                _ => {}
            }
            return Ok(());
        }
        if !(id.is_string() || id.is_i64() || id.is_u64()) {
            return stdio.error(&Value::Null, -32600, "invalid JSON-RPC request id");
        }
        let params = &message["params"];
        if method == "initialize" {
            if self.protocol.is_some() {
                return stdio.error(id, -32600, "the MCP session is already initialized");
            }
            let Some(requested) = params["protocolVersion"].as_str() else {
                return stdio.error(id, -32602, "initialize needs protocolVersion");
            };
            let version = PROTOCOLS.iter().copied().find(|&version| version == requested).unwrap_or(PROTOCOLS[0]);
            self.protocol = Some(version);
            self.roots = params["capabilities"]["roots"].is_object();
            return stdio.result(id, json!({
                "protocolVersion": version, "serverInfo": {"name": "CodeLore Atlas", "version": crate::VERSION},
                "capabilities": {"tools": {}},
                "instructions": "Search the active Git workspace. A first search can start the background index. Check code_status for readiness.",
            }));
        }
        if method == "ping" {
            return stdio.result(id, json!({}));
        }
        if !self.initialized && matches!(method, "tools/list" | "tools/call") {
            return stdio.error(id, -32002, "initialize the MCP session first");
        }
        match method {
            "tools/list" => stdio.result(id, json!({"tools": tool_list()})),
            "tools/call" => {
                let tool = match Tool::parse(params) {
                    Ok(tool) => tool,
                    Err(error) => return stdio.error(id, -32602, &error),
                };
                let root = self.workspace(tool.project_path.as_deref(), stdio)?;
                let (data, is_error) = match root {
                    Ok(root) => call_tool(&root, &tool),
                    Err(error) => (json!({
                        "state": "error", "error": {"code": "project_unresolved", "message": error}, "notes": [error],
                    }), true),
                };
                let mut result = json!({"content": [{"type": "text", "text": data.to_string()}], "isError": is_error});
                if matches!(self.protocol, Some("2025-11-25" | "2025-06-18")) {
                    result["structuredContent"] = data;
                }
                stdio.result(id, result)
            }
            _ => stdio.error(id, -32601, "unknown MCP method"),
        }
    }

    fn workspace(&mut self, override_path: Option<&Path>, stdio: &mut Stdio<impl Write>) -> io::Result<Result<PathBuf, String>> {
        if let Some(path) = override_path.map(|path| self.cwd.join(path)).or_else(|| self.pinned.clone()) {
            return Ok(self.confirm(&path).ok_or_else(|| format!("{} is outside a confirmed Git checkout", path.display())));
        }
        if self.roots && !self.roots_unresponsive {
            match stdio.roots()? {
                Some(Value::Array(roots)) if !roots.is_empty() => {
                    let worktrees: BTreeSet<_> = roots.into_iter().filter_map(|root| {
                        let uri = url::Url::parse(root["uri"].as_str()?).ok()?;
                        if uri.scheme() != "file" || uri.query().is_some() || uri.fragment().is_some()
                            || uri.host_str().is_some_and(|host| host != "localhost")
                        {
                            return None;
                        }
                        let path = config::simplify(&uri.to_file_path().ok()?.canonicalize().ok()?);
                        self.confirmed_root(&path).map(|_| path)
                    }).collect();
                    return Ok(match worktrees.len() {
                        0 => Err("The host supplied no local folder in a confirmed Git checkout. Open a checkout or supply projectPath.".into()),
                        1 => Ok(worktrees.into_iter().next().expect("one checkout")),
                        _ => Err("MCP roots identifies multiple Git checkouts or workspace folders. Select one active folder or supply projectPath.".into()),
                    });
                }
                Some(_) => {}
                None => self.roots_unresponsive = true,
            }
        }
        let cwd = self.cwd.clone();
        Ok(self.confirm(&cwd).ok_or_else(|| "No confirmed Git checkout for the MCP workspace. Open one, launch from it, or supply projectPath.".into()))
    }

    fn confirm(&mut self, directory: &Path) -> Option<PathBuf> {
        let directory = config::simplify(&directory.canonicalize().ok()?);
        self.confirmed_root(&directory)
    }

    fn confirmed_root(&mut self, directory: &Path) -> Option<PathBuf> {
        if !directory.is_dir() {
            return None;
        }
        let boundary = checkout_boundary(directory)?;
        if let Some(root) = self.projects.get(boundary) {
            return Some(root.clone());
        }
        let root = config::simplify(&crate::gitio::toplevel(directory)?.canonicalize().ok()?);
        if root != boundary {
            return None;
        }
        self.projects.insert(boundary.to_path_buf(), root.clone());
        Some(root)
    }
}

const TOOLS: &[(&str, &str, Option<&str>, &str)] = &[
    ("code_search", "search", Some("query"), "Search the active project's code using its latest Atlas snapshot."),
    ("code_symbol", "symbol", Some("name"), "Find definitions of a named code symbol."),
    ("code_symbols", "symbols", Some("file"), "List the symbols defined in a source file."),
    ("code_status", "status", None, "Show Atlas snapshot readiness and background refresh status."),
    ("code_calls", "calls", Some("target"), "Read C++ callers or callees from an explicitly prepared Atlas call graph."),
    ("code_calls_status", "calls-status", None, "Check readiness, freshness and diagnostics for an Atlas call graph."),
];

fn call_graph(command: &str) -> bool {
    matches!(command, "calls" | "calls-status")
}

fn tool_list() -> Vec<Value> {
    TOOLS.iter().map(|&(name, command, required, description)| {
        let calls = call_graph(command);
        let mut properties = json!({
            "projectPath": {"type": "string", "minLength": 1, "description": "Override the workspace with a local Git checkout path."},
            "scope": {"type": "string", "enum": if calls { vec!["project", "unreal", "all"] } else { vec!["project", "unreal", "dependencies", "all"] }},
            "source": {"type": "string", "description": "Select one configured Atlas source by name. Use instead of scope."},
        });
        if let Some(required) = required {
            properties[required] = json!({"type": "string", "minLength": 1});
        }
        if matches!(command, "search" | "symbol") {
            properties["path"] = json!({"type": "string", "description": "Literal source-relative path prefix."});
            properties["lang"] = json!({"type": "string", "description": "Atlas language, for example cpp or rust."});
        }
        if command == "search" {
            properties["limit"] = json!({"type": "integer", "minimum": 1, "maximum": 100, "default": 5});
            properties["text_only"] = json!({"type": "boolean", "default": false});
            properties["snapshot"] = json!({"type": "boolean", "description": "Search reads a published snapshot."});
        }
        let mut required: Vec<_> = required.into_iter().collect();
        if calls {
            required.push("profile");
            properties["profile"] = json!({"type": "string", "minLength": 1, "description": "Exact profile used by lore atlas calls-index."});
            properties["source"]["description"] = json!("Select the explicitly prepared graph for the project and this configured source. Use instead of scope.");
        }
        if command == "calls" {
            properties["target"]["description"] = json!("C++ function name, exact signature, cpp: id, or file:line.");
            properties["direction"] = json!({"type": "string", "enum": ["in", "out"], "default": "out"});
            properties["depth"] = json!({"type": "integer", "minimum": 1, "maximum": 32, "default": 1});
            properties["limit"] = json!({"type": "integer", "minimum": 1, "maximum": 100, "default": 100});
            properties["candidates"] = json!({"type": "boolean", "default": false, "description": "Include stale, diagnostic, ambiguous and unresolved edges."});
        }
        let mut tool = json!({"name": name, "description": description, "inputSchema": {
            "type": "object", "properties": properties, "required": required, "additionalProperties": false,
        }});
        if calls {
            tool["annotations"] = json!({"readOnlyHint": true});
        }
        tool
    }).collect()
}

struct Tool {
    opts: cli::Opts,
    project_path: Option<PathBuf>,
}

impl Tool {
    fn parse(params: &Value) -> Result<Self, String> {
        if !params.is_object() {
            return Err("tools/call needs an object of parameters".into());
        }
        let name = params["name"].as_str().unwrap_or_default();
        let &(_, command, required, _) = TOOLS.iter().find(|&&(tool, _, _, _)| tool == name)
            .ok_or_else(|| format!("unknown Atlas MCP tool {name:?}"))?;
        let empty = Map::new();
        let args = match params.get("arguments") {
            Some(arguments) => arguments.as_object().ok_or("tool arguments must be an object")?,
            None => &empty,
        };
        if args.contains_key("scope") && args.contains_key("source") {
            return Err("choose either scope or source".to_owned());
        }
        let mut opts = cli::Opts {
            command: command.to_owned(), positional: Vec::new(), options: BTreeMap::new(), flags: BTreeSet::new(),
        };
        let calls = call_graph(command);
        if calls && !args.contains_key("profile") {
            return Err(format!("{name} requires an explicit profile. Select the exact profile used with lore atlas calls-index."));
        }
        if let Some(key) = required {
            let value = args.get(key).and_then(Value::as_str).filter(|text| !text.trim().is_empty())
                .ok_or_else(|| format!("{name} needs a nonempty {key}"))?;
            opts.positional.push(value.to_owned());
        }
        let mut project_path = None;
        for (key, value) in args {
            if required == Some(key.as_str()) {
                continue;
            }
            let invalid = || format!("invalid {key} for {name}");
            match key.as_str() {
                "projectPath" => project_path = Some(PathBuf::from(value.as_str().filter(|path| !path.trim().is_empty()).ok_or_else(invalid)?)),
                "scope" => {
                    let scope = value.as_str().filter(|scope| matches!(*scope, "project" | "unreal" | "all")
                        || !calls && *scope == "dependencies").ok_or_else(invalid)?;
                    opts.options.insert("--source".into(), scope.to_owned());
                }
                "source" => {
                    let source = value.as_str().filter(|source| config::atlas_source_name(source)).ok_or_else(invalid)?;
                    opts.options.insert("--source".into(), source.to_owned());
                }
                "path" | "lang" if matches!(command, "search" | "symbol") => {
                    opts.options.insert(format!("--{key}"), value.as_str().ok_or_else(invalid)?.to_owned());
                }
                "limit" if matches!(command, "search" | "calls") => {
                    let limit = value.as_u64().filter(|limit| (1..=100).contains(limit)).ok_or_else(invalid)?;
                    opts.options.insert("-n".into(), limit.to_string());
                }
                "profile" if calls => {
                    let profile = value.as_str().filter(|profile| !profile.trim().is_empty()).ok_or_else(invalid)?;
                    opts.options.insert("--profile".into(), profile.to_owned());
                }
                "direction" if command == "calls" => {
                    let direction = value.as_str().filter(|direction| matches!(*direction, "in" | "out")).ok_or_else(invalid)?;
                    opts.options.insert("--direction".into(), direction.to_owned());
                }
                "depth" if command == "calls" => {
                    let depth = value.as_u64().filter(|depth| (1..=32).contains(depth)).ok_or_else(invalid)?;
                    opts.options.insert("--depth".into(), depth.to_string());
                }
                "candidates" if command == "calls" => {
                    if value.as_bool().ok_or_else(invalid)? {
                        opts.flags.insert("--candidates".into());
                    }
                }
                "snapshot" if command == "search" => { value.as_bool().ok_or_else(invalid)?; }
                "text_only" if command == "search" => {
                    if value.as_bool().ok_or_else(invalid)? {
                        opts.flags.insert("--text-only".into());
                    }
                }
                _ => return Err(format!("unknown argument {key} for {name}")),
            }
        }
        if command == "calls" {
            opts.options.entry("-n".into()).or_insert_with(|| "100".into());
        }
        Ok(Self { opts, project_path })
    }

    fn project(&self) -> bool {
        matches!(self.opts.options.get("--source").map(String::as_str), None | Some("all" | "project"))
    }

    fn dependencies(&self) -> bool {
        !matches!(self.opts.options.get("--source").map(String::as_str), Some("project" | "unreal"))
    }
}

fn call_tool(root: &Path, tool: &Tool) -> (Value, bool) {
    let run = || {
        let config = project_config(root)?;
        if call_graph(&tool.opts.command) {
            return Ok(cli::run_snapshot(&config, &tool.opts, None));
        }
        let startup_error = if (tool.project() || tool.dependencies()) && tool.opts.command != "status" {
            let dir = watch::ensure_mcp(&config)?;
            tool.dependencies().then(|| super::dependencies::enable(root, &dir).err()).flatten()
        } else {
            None
        };
        let dependencies = tool.dependencies().then(|| {
            let mut catalogue = watch::dependencies(&config);
            if let Some(error) = startup_error {
                catalogue.state = "unavailable".to_owned();
                catalogue.notes.push(error.message);
            }
            catalogue
        });
        Ok::<_, LoreError>(cli::run_snapshot(&config, &tool.opts, dependencies))
    };
    let (code, mut payload) = match run() {
        Ok(result) => result,
        Err(error) => return (json!({"root": root, "state": "error", "error": error.message, "details": error.details}), true),
    };
    payload["root"] = json!(root);
    if call_graph(&tool.opts.command) {
        let mut states = payload["sources"].as_array().into_iter().flatten()
            .filter_map(|state| state["health"].as_str().or_else(|| state["state"].as_str()));
        let first = states.next().unwrap_or("unindexed");
        let state = if code != 0 { "error" } else if states.all(|state| state == first) { first } else { "partial" };
        payload["state"] = json!(state);
        return (payload, code != 0);
    }
    let project_state = match payload["snapshot"]["state"].as_str() {
        Some("ready") => "ready",
        Some("unindexed") if matches!(payload["snapshot"]["worker"]["state"].as_str(),
            Some("starting" | "updating" | "waiting_for_writer" | "idle")) => "indexing",
        Some("unindexed") if matches!(payload["snapshot"]["worker"]["state"].as_str(),
            Some("error" | "unavailable")) => "error",
        Some("unindexed") => "empty",
        _ => "error",
    };
    if tool.project() && payload.get("sources").is_none() && payload.get("snapshot").is_some() {
        let mut project = json!({"source": "project", "state": project_state, "root": root});
        for field in ["counts", "vectors", "ann", "semantic_search", "timings_ms"] {
            if let Some(value) = payload.get(field) {
                project[field] = value.clone();
            }
        }
        if tool.opts.command == "status" && project_state == "empty" {
            project["counts"] = json!({"files": 0, "chunks": 0, "symbols": 0, "imports": 0, "edges": 0});
            project["vectors"] = json!(0);
        }
        payload["sources"] = json!([project]);
    }
    if let Some(sources) = payload.get_mut("sources").and_then(Value::as_array_mut) {
        for source in sources.iter_mut().filter(|source| source["source"] == "project" && source["state"] == "unindexed") {
            source["state"] = json!(project_state);
        }
    }
    for field in ["results", "definitions"] {
        if let Some(rows) = payload.get_mut(field).and_then(Value::as_array_mut) {
            for row in rows {
                if row.get("path").is_some() && row.get("source").is_none() {
                    row["source"] = json!("project");
                }
            }
        }
    }
    let any_ready = payload["sources"].as_array().is_some_and(|sources| sources.iter().any(|source| source["state"] == "ready"));
    let indexing = (tool.project() && project_state == "indexing") || matches!(
        payload["dependencies"]["state"].as_str(), Some("discovering" | "indexing")
    );
    let failed = code != 0 && !indexing;
    payload["state"] = json!(if failed { "error" } else if any_ready { "ready" } else if indexing { "indexing" }
        else if tool.project() && project_state == "error" { "error" } else { "empty" });
    if indexing && !any_ready {
        if let Some(object) = payload.as_object_mut() {
            let note = object.remove("error").or_else(|| object.remove("note"))
                .unwrap_or_else(|| json!("Atlas is building the first project snapshot in the background."));
            object.insert("notes".into(), json!([note]));
            if tool.opts.command == "search" {
                object.entry("results").or_insert_with(|| json!([]));
            }
        }
    }
    (payload, failed)
}
