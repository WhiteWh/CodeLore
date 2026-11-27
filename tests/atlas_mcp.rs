//! Exercise the public stdio entry point with MCP clients using Git fixtures.
#![cfg(feature = "atlas")]
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[path = "atlas_mcp/ann.rs"]
mod ann;

struct Fixture {
    temp: tempfile::TempDir,
    first: PathBuf,
    second: PathBuf,
    cache: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let temp_path = codelore::config::simplify(&temp.path().canonicalize().unwrap());
        let first = temp_path.join("Alpha workspace");
        let second = temp_path.join("Beta workspace");
        for (root, file, symbol) in [
            (&first, "math.cpp", "AlphaFunction"),
            (&second, "other.cpp", "BetaFunction"),
        ] {
            fs::create_dir_all(root.join("src")).unwrap();
            git(root, &["init", "-q"]);
            fs::write(root.join("src").join(file), format!("int {symbol}() {{\n    return 1;\n}}\n")).unwrap();
            fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
            git(root, &["add", "."]);
            git(root, &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture"]);
            fs::create_dir(root.join("ignored")).unwrap();
            fs::write(root.join("ignored/private.cpp"), "int IgnoredFunction() {}\n").unwrap();
            fs::create_dir(root.join("target")).unwrap();
            fs::write(root.join("target/generated.cpp"), "int GeneratedFunction() {}\n").unwrap();
        }
        let cache = temp_path.join("cache");
        Self { temp, first, second, cache }
    }

    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lore"));
        hide(&mut command);
        command.current_dir(cwd).env("LORE_CACHE", &self.cache)
            .env_remove("LORE_ATLAS").env_remove("LORE_ATLAS_SNAPSHOT")
            .env("LORE_MODEL_FETCH", "off").env("LORE_EMBED_SERVE", "off")
            // Tests write build evidence after the worker starts.
            .env("LORE_ATLAS_DISCOVERY_SECS", "0");
        command
    }

    fn client(&self, cwd: &Path, pinned: Option<&Path>, roots: Option<Vec<PathBuf>>, respond: bool, version: &str) -> Client {
        let mut command = self.command(cwd);
        if let Some(root) = pinned {
            command.arg("-C").arg(root);
        }
        command.args(["mcp", "serve"]);
        Client::start(command, roots, respond, version)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for root in [&self.first.join("src"), &self.first, &self.second] {
            let _ = self.command(root).args(["atlas", "watch", "stop"]).output();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let Ok(output) = self.command(root).args(["atlas", "watch", "status", "--json"]).output() else { break };
                let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
                if value["worker"]["state"] != "stopping" {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    hide(&mut command);
    let output = command.current_dir(root).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

fn hide(_command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        _command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
}

struct Client {
    child: Child,
    input: ChildStdin,
    output: Receiver<String>,
    next_id: u64,
    roots: Option<Vec<PathBuf>>,
    respond: bool,
    requested_roots: usize,
    structured: bool,
}

impl Client {
    fn start(mut command: Command, roots: Option<Vec<PathBuf>>, respond: bool, version: &str) -> Self {
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut client = Self {
            child, input, output: receiver, next_id: 0, roots, respond, requested_roots: 0,
            structured: version == "2025-11-25",
        };
        let capabilities = if client.roots.is_some() { json!({"roots": {"listChanged": true}}) } else { json!({}) };
        let initialized = client.call("initialize", json!({
            "protocolVersion": version, "capabilities": capabilities, "clientInfo": {"name": "atlas-tests", "version": "1"},
        }));
        assert_eq!(initialized["protocolVersion"], version);
        assert!(initialized["capabilities"]["tools"].is_object());
        client.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        client
    }

    fn send(&mut self, message: Value) {
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let frame = self.request(method, params);
        assert!(frame["error"].is_null(), "{frame}");
        frame["result"].clone()
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self.output.recv_timeout(Duration::from_secs(8)).unwrap_or_else(|e| panic!("{method}: {e}"));
            let frame: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
            assert_eq!(frame["jsonrpc"], "2.0");
            if frame["method"] == "roots/list" {
                self.requested_roots += 1;
                assert!(self.roots.is_some(), "roots was not advertised");
                if self.respond {
                    let roots: Vec<_> = self.roots.as_ref().unwrap().iter().map(|root| {
                        json!({"uri": url::Url::from_directory_path(root).unwrap().as_str()})
                    }).collect();
                    self.send(json!({"jsonrpc": "2.0", "id": frame["id"], "result": {"roots": roots}}));
                }
                continue;
            }
            assert_eq!(frame["id"], id);
            return frame;
        }
    }

    fn tool(&mut self, name: &str, arguments: Value, error: bool) -> Value {
        let response = self.call("tools/call", json!({"name": name, "arguments": arguments}));
        assert_eq!(response["isError"], error, "{response}");
        let body: Value = serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
        if self.structured {
            assert_eq!(response["structuredContent"], body);
        } else {
            assert!(response.get("structuredContent").is_none());
        }
        body
    }

    fn status(&mut self) -> Value {
        self.tool("code_status", json!({}), false)
    }

    fn wait(&mut self, mut satisfied: impl FnMut(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = self.status();
            if satisfied(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "{status}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = Some(roots);
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}));
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn atlas_mcp_reads_only_the_prepared_profile_and_named_call_graph() {
    let fixture = Fixture::new();
    let parent = fixture.first.parent().unwrap();
    let library = parent.join("Runtime");
    let other = parent.join("Other runtime");
    fs::create_dir(&library).unwrap();
    fs::create_dir(&other).unwrap();
    let configure = |root: &str| {
        fs::write(fixture.first.join("lore.toml"), format!(
            "version = 1\n[atlas]\nenabled = true\n[atlas.sources.runtime]\nroot = {root:?}\n"
        )).unwrap();
    };
    configure("../Runtime");

    let mut command = fixture.command(fixture.temp.path());
    command.env("LORE_LIBCLANG", fixture.temp.path().join("absent-libclang")).args(["mcp", "serve"]);
    let mut client = Client::start(command, Some(vec![fixture.first.clone()]), true, "2025-11-25");
    let tools = client.call("tools/list", json!({}));
    for name in ["code_calls", "code_calls_status"] {
        let tool = tools["tools"].as_array().unwrap().iter().find(|tool| tool["name"] == name).expect(name);
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert!(tool["inputSchema"]["required"].as_array().unwrap().contains(&json!("profile")));
    }
    let missing = client.request("tools/call", json!({"name":"code_calls_status","arguments":{"source":"runtime"}}));
    assert_eq!(missing["error"]["code"], -32602);
    assert!(missing["error"]["message"].as_str().unwrap().contains("explicit profile"), "{missing}");
    let args = json!({"source":"runtime","profile":"Editor"});
    let cold = client.tool("code_calls_status", args.clone(), false);
    assert_eq!(cold["root"], json!(fixture.first));
    assert_eq!(cold["state"], "unindexed");
    let missing_edge = client.tool("code_calls", json!({"source":"runtime","profile":"Editor","target":"CallRuntime"}), false);
    assert_eq!(missing_edge["start_resolution"], "missing");
    assert!(!fixture.cache.exists(), "graph requests created cache state");

    let Some(clang) = std::env::var_os("LORE_TEST_LIBCLANG") else {
        eprintln!("compiler fixture skipped: set LORE_TEST_LIBCLANG to load libclang");
        return;
    };
    let header = library.join("api.h");
    let implementation = library.join("api.cpp");
    let project = fixture.first.join("src/caller.cpp");
    let partial = fixture.first.join("src/partial.cpp");
    fs::write(&header, "namespace ext { int step(int); int tail(int); }\n").unwrap();
    let implementation_code = "#include \"api.h\"\nnamespace ext { int tail(int x) { return x; } int step(int x) { return tail(x); } }\n";
    fs::write(&implementation, implementation_code).unwrap();
    fs::write(&project, "#include \"api.h\"\nint CallRuntime(int x) { return ext::step(x) + ext::tail(x); }\nint CallRuntime(double x) { return int(x); }\n").unwrap();
    fs::write(&partial, "#include \"api.h\"\nint DiagnosticCaller(int x) { return ext::step(x); }\n#error incomplete profile\n").unwrap();
    let commands = [(&fixture.first, &project), (&library, &implementation), (&fixture.first, &partial)].map(|(cwd, file)| {
        json!({"directory":cwd,"file":file,"arguments":["clang++","-std=c++20","-I",library,"-c",file]})
    });
    let database = fixture.first.join("compile_commands.json");
    emit_json(&database, json!(commands));
    let build = |profile: &str| {
        let output = fixture.command(&fixture.first).args([
            "atlas", "calls-index", "--source", "runtime", "--profile", profile, "--compdb", "compile_commands.json", "--libclang",
        ]).arg(&clang).arg("--json").output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let indexed = build("Editor");
    assert_eq!(indexed["parsed"], 3, "{indexed}");
    assert_eq!(indexed["failed"], 1, "{indexed}");
    let db = PathBuf::from(indexed["source"]["path"].as_str().unwrap());
    let indexed_other_profile = build("Game");
    assert_ne!(indexed_other_profile["source"]["path"], indexed["source"]["path"]);
    let published_bytes = fs::read(&db).unwrap();

    let state = client.tool("code_calls_status", args.clone(), false);
    assert_eq!(state["state"], "partial", "{state}");
    assert_eq!(state["sources"][0]["name"], "runtime");
    assert_eq!(state["sources"][0]["scope"], "project_and_source");
    assert_eq!(state["sources"][0]["counts"]["units"], 3);
    assert_eq!(state["sources"][0]["failed_units"], 1);
    assert!(state["diagnostics"].to_string().contains("incomplete profile"));
    assert!(state["sources"][0]["compilation_key"].is_string());
    let no_profile = client.request("tools/call", json!({"name":"code_calls","arguments":{"source":"runtime","target":"CallRuntime"}}));
    assert!(no_profile["error"]["message"].as_str().unwrap().contains("explicit profile"));
    let wrong = client.tool("code_calls_status", json!({"source":"runtime","profile":"Missing"}), false);
    assert_eq!(wrong["state"], "unindexed");
    assert!(wrong["sources"][0]["note"].as_str().unwrap().contains("Missing"));
    let ambiguous = client.tool("code_calls", json!({"source":"runtime","profile":"Editor","target":"CallRuntime"}), false);
    assert_eq!(ambiguous["start_resolution"], "ambiguous");
    assert_eq!(ambiguous["edges"], json!([]));

    let outgoing = client.tool("code_calls", json!({
        "source":"runtime","profile":"Editor","target":"CallRuntime(int)","depth":2,"limit":1,
    }), false);
    assert_eq!(outgoing["edges"].as_array().unwrap().len(), 1);
    assert_eq!(outgoing["truncated"], true);
    let first = &outgoing["edges"][0];
    assert_eq!(first["resolution"], "confirmed");
    assert_eq!(first["site"]["line"], 2);
    assert!(first["site"]["column"].as_u64().unwrap() > 0);
    assert_eq!(address(Path::new(first["site"]["absolute_path"].as_str().unwrap())), address(&project));
    assert_eq!(first["callee"]["definition"]["source_name"], "runtime");
    assert_eq!(address(Path::new(first["callee"]["definition"]["absolute_path"].as_str().unwrap())), address(&implementation));

    let incoming = client.tool("code_calls", json!({
        "source":"runtime","profile":"Editor","target":"ext::step(int)","direction":"in",
    }), false);
    assert_eq!(incoming["edges"].as_array().unwrap().len(), 1);
    assert_eq!(incoming["edges"][0]["caller"]["signature"], "CallRuntime(int)");
    assert_eq!(incoming["edges"][0]["resolution"], "confirmed");
    let external = client.tool("code_calls", json!({
        "source":"runtime","profile":"Editor","target":"ext::step(int)","direction":"out",
    }), false);
    assert_eq!(external["edges"][0]["callee"]["name"], "ext::tail");
    assert_eq!(external["edges"][0]["site"]["source_name"], "runtime");
    assert_eq!(external["edges"][0]["resolution"], "confirmed");
    let point = client.tool("code_calls", json!({
        "source":"runtime","profile":"Editor","target":format!("{}:2", project.display()),
    }), false);
    assert_eq!(point["start_resolution"], "exact");
    let diagnostic = client.tool("code_calls", json!({
        "source":"runtime","profile":"Editor","target":"DiagnosticCaller","candidates":true,
    }), false);
    assert_eq!(diagnostic["edges"][0]["resolution"], "diagnostic_candidate");

    fs::write(&implementation, format!("{implementation_code}// stale fixture\n")).unwrap();
    let stale = client.tool("code_calls", json!({
        "source":"runtime","profile":"Editor","target":"ext::step(int)","candidates":true,
    }), false);
    assert_eq!(stale["edges"][0]["resolution"], "stale");
    assert_eq!(client.tool("code_calls_status", args.clone(), false)["state"], "stale");
    fs::write(&implementation, implementation_code).unwrap();
    let original_cdb = fs::read(&database).unwrap();
    fs::write(&database, [original_cdb.as_slice(), b"\n "].concat()).unwrap();
    let changed = client.tool("code_calls_status", args.clone(), false);
    assert_eq!(changed["sources"][0]["stale_units"], 3);
    fs::write(&database, original_cdb).unwrap();
    configure("../Other runtime");
    assert_eq!(client.tool("code_calls_status", args.clone(), false)["state"], "unindexed");
    configure("../Runtime");
    assert_eq!(fs::read(&db).unwrap(), published_bytes);
    assert_eq!(client.status()["snapshot"]["worker"]["state"], "disabled");
    fs::remove_file(db).unwrap();
    assert_eq!(client.tool("code_calls_status", args, false)["state"], "unindexed");
}

#[test]
fn atlas_mcp_builds_in_background_and_switches_active_roots() {
    let fixture = Fixture::new();
    let mut client = fixture.client(fixture.temp.path(), None, Some(vec![fixture.first.clone()]), true, "2025-11-25");
    let tools = client.call("tools/list", json!({}));
    assert_eq!(tools["tools"].as_array().unwrap().len(), 6);
    let cold = client.status();
    assert_eq!(cold["root"], json!(fixture.first));
    assert_eq!(cold["state"], "empty");
    assert_eq!(cold["snapshot"]["worker"]["state"], "disabled");

    // A blocked writer demonstrates that the first call does not index or
    // wait inline. Its search starts the same worker used by the CLI.
    let index_dir = fixture.cache.join(codelore::index::repo_key(&fixture.first));
    fs::create_dir_all(&index_dir).unwrap();
    let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(index_dir.join("atlas-writer.lock")).unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    let indexing = client.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    assert_eq!(indexing["state"], "indexing");
    assert_eq!(indexing["results"], json!([]));
    assert!(!index_dir.join("atlas.sqlite").exists());
    assert!(!fixture.first.join("lore.toml").exists());
    drop(lock);

    let first = client.wait(|status| status["state"] == "ready");
    assert_eq!(first["sources"][0]["counts"]["files"], 1);
    assert_eq!(first["sources"][0]["vectors"], 0);
    let lock = OpenOptions::new().read(true).write(true).open(index_dir.join("atlas-writer.lock")).unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    let search = client.tool("code_search", json!({"query":"AlphaFunction","lang":"cpp","path":"src/","limit":1,"snapshot":true}), false);
    drop(lock);
    assert_eq!(search["results"][0]["source"], "project");
    assert_eq!(search["results"][0]["path"], "src/math.cpp");
    assert_eq!(search["results"][0]["start"], 1);
    assert_eq!(search["snapshot"]["mode"], "background");
    let excluded = client.tool("code_search", json!({"query":"AlphaFunction","path":"ignored/"}), false);
    assert_eq!(excluded["results"], json!([]));
    let wrong_lang = client.tool("code_search", json!({"query":"AlphaFunction","lang":"rust"}), false);
    assert_eq!(wrong_lang["results"], json!([]));

    fs::write(fixture.first.join("src/math.cpp"), "int EditedFunction() {\n    return 17;\n}\n").unwrap();
    client.wait(|status| status["snapshot"]["revision"].as_u64() > first["snapshot"]["revision"].as_u64());
    let symbol = client.tool("code_symbol", json!({"name":"EditedFunction"}), false);
    assert_eq!(symbol["definitions"][0]["path"], "src/math.cpp");
    assert_eq!(symbol["definitions"][0]["line"], 1);
    let file = client.tool("code_symbols", json!({"file":"src/math.cpp"}), false);
    assert_eq!(file["results"][0]["name"], "EditedFunction");

    client.set_roots(vec![fixture.second.clone()]);
    assert_eq!(client.status()["state"], "empty");
    client.tool("code_search", json!({"query":"BetaFunction","text_only":true}), false);
    let second = client.wait(|status| status["state"] == "ready");
    assert_eq!(second["root"], json!(fixture.second));
    assert_ne!(second["snapshot"]["path"], first["snapshot"]["path"]);
    let search = client.tool("code_search", json!({"query":"BetaFunction","text_only":true}), false);
    assert_eq!(search["results"][0]["path"], "src/other.cpp");
    assert!(!fixture.second.join("lore.toml").exists());

    client.set_roots(vec![fixture.first.clone(), fixture.second.clone()]);
    let ambiguous = client.tool("code_status", json!({}), true);
    assert!(ambiguous["error"]["message"].as_str().unwrap().contains("multiple Git checkouts"));
    let explicit = client.tool("code_symbol", json!({"name":"EditedFunction","projectPath":fixture.first.join("src")}), false);
    assert_eq!(explicit["root"], json!(fixture.first));
    assert_eq!(explicit["definitions"][0]["path"], "src/math.cpp");
    client.set_roots(vec![fixture.first.join("src")]);
    let nested = client.status();
    assert_eq!(nested["state"], "empty");
    assert_eq!(nested["root"], json!(fixture.first.join("src")));
    client.set_roots(Vec::new());
    assert_eq!(client.tool("code_status", json!({}), true)["error"]["code"], "project_unresolved");
}

#[test]
fn atlas_mcp_indexes_only_the_advertised_subfolder_in_its_own_snapshot() {
    let fixture = Fixture::new();
    let workspace = fixture.first.join("src");
    fs::write(fixture.first.join("outside.cpp"), "int OuterBoundarySentinel() {\n    return 2;\n}\n").unwrap();
    fs::write(workspace.join("ignored.cpp"), "int ExcludedByParentRule() {\n    return 3;\n}\n").unwrap();
    fs::write(fixture.first.join(".gitignore"), "ignored/\n/src/ignored.cpp\n").unwrap();
    git(&fixture.first, &["add", "outside.cpp"]);
    let mut client = fixture.client(fixture.temp.path(), None, Some(vec![fixture.first.clone()]), true, "2025-11-25");
    client.tool("code_search", json!({"query":"OuterBoundarySentinel","text_only":true}), false);
    let full = client.wait(|status| status["state"] == "ready");
    assert_eq!(full["sources"][0]["counts"]["files"], 2);
    assert_eq!(client.tool("code_symbol", json!({"name":"OuterBoundarySentinel"}), false)["definitions"][0]["path"], "outside.cpp");

    client.set_roots(vec![workspace.clone()]);
    let empty = client.status();
    assert_eq!(empty["root"], json!(workspace));
    assert_eq!(empty["state"], "empty");
    let first = client.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    assert!(matches!(first["state"].as_str(), Some("indexing" | "ready")));
    let nested = client.wait(|status| status["state"] == "ready");
    assert_eq!(nested["sources"][0]["counts"]["files"], 1);
    assert_ne!(nested["snapshot"]["path"], full["snapshot"]["path"]);
    let found = client.tool("code_search", json!({"query":"AlphaFunction","path":"math.cpp","lang":"cpp","text_only":true}), false);
    assert_eq!(found["results"][0]["path"], "math.cpp");
    for name in ["OuterBoundarySentinel", "ExcludedByParentRule"] {
        assert_eq!(client.tool("code_symbol", json!({"name":name}), false)["definitions"], json!([]));
    }
    assert!(client.tool("code_search", json!({"query":"OuterBoundarySentinel","path":"outside.cpp","text_only":true}), false)["results"].as_array().unwrap().is_empty());

    fs::write(workspace.join("math.cpp"), "int InnerRevision() {\n    return 4;\n}\n").unwrap();
    fs::write(workspace.join("new.cpp"), "int NewlyUntracked() {\n    return 5;\n}\n").unwrap();
    let updated = client.wait(|status| {
        status["snapshot"]["revision"].as_u64() > nested["snapshot"]["revision"].as_u64()
            && status["sources"][0]["counts"]["files"] == 2
    });
    assert_eq!(updated["snapshot"]["path"], nested["snapshot"]["path"]);
    assert_eq!(client.tool("code_symbol", json!({"name":"InnerRevision"}), false)["definitions"][0]["path"], "math.cpp");
    assert_eq!(client.tool("code_symbol", json!({"name":"NewlyUntracked"}), false)["definitions"][0]["path"], "new.cpp");
    assert_eq!(client.tool("code_symbol", json!({"name":"OuterBoundarySentinel"}), false)["definitions"], json!([]));
    assert!(!fixture.first.join("lore.toml").exists());
    assert!(!workspace.join("lore.toml").exists());
}

#[test]
fn atlas_mcp_works_with_legacy_clients_and_confirmed_cwd() {
    let fixture = Fixture::new();
    let mut no_roots = fixture.client(&fixture.first.join("src"), None, None, false, "2024-11-05");
    assert_eq!(no_roots.status()["root"], json!(fixture.first));
    assert_eq!(no_roots.requested_roots, 0);
    let mut advertised = fixture.client(&fixture.second, None, Some(Vec::new()), false, "2024-11-05");
    assert_eq!(advertised.status()["root"], json!(fixture.second));
    assert_eq!(advertised.status()["root"], json!(fixture.second));
    assert_eq!(advertised.requested_roots, 1);
    let mut pinned = fixture.client(fixture.temp.path(), Some(&fixture.first), Some(vec![fixture.second.clone()]), true, "2024-11-05");
    assert_eq!(pinned.status()["root"], json!(fixture.first));
    assert_eq!(pinned.requested_roots, 0);
}

#[test]
fn atlas_mcp_answers_pipelined_requests_and_retires_an_idle_worker() {
    let fixture = Fixture::new();
    let mut command = fixture.command(&fixture.first);
    command.env("LORE_ATLAS_MCP_IDLE", "2").args(["mcp", "serve"]);
    let mut client = Client::start(command, None, false, "2025-11-25");
    // A tool call runs on its own thread: a ping sent behind it is answered,
    // and every request gets exactly one answer under its own id.
    client.send(json!({"jsonrpc": "2.0", "id": "search", "method": "tools/call",
        "params": {"name": "code_search", "arguments": {"query": "AlphaFunction", "text_only": true}}}));
    client.send(json!({"jsonrpc": "2.0", "id": "ping", "method": "ping"}));
    let mut answered = BTreeSet::new();
    while answered.len() < 2 {
        let line = client.output.recv_timeout(Duration::from_secs(8)).expect("an answer");
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert!(frame["error"].is_null(), "{frame}");
        assert!(answered.insert(frame["id"].as_str().unwrap().to_owned()), "{frame}");
    }
    assert_eq!(answered, BTreeSet::from(["ping".to_owned(), "search".to_owned()]));
    // A cancellation for a call that already answered changes nothing.
    client.send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": "search"}}));
    assert_eq!(client.call("ping", json!({})), json!({}));

    let started = client.wait(|status| status["snapshot"]["state"] == "ready");
    assert_ne!(started["snapshot"]["worker"]["state"], "disabled", "{started}");
    // code_status does not count as a lookup that needs the worker.
    let retired = client.wait(|status| status["snapshot"]["worker"]["state"] == "disabled");
    assert_eq!(retired["snapshot"]["state"], "ready", "the snapshot outlives the worker");
    client.tool("code_search", json!({"query": "AlphaFunction", "text_only": true}), false);
    let restarted = client.status();
    assert_ne!(restarted["snapshot"]["worker"]["state"], "disabled", "{restarted}");
}

fn emit_json(path: &Path, payload: Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&payload).unwrap()).unwrap();
}

fn msvc_dependencies(path: &Path, tu: &Path, includes: &[&Path]) {
    emit_json(path, json!({"Version":"1.2","Data":{"Source":tu,"Includes":includes}}));
}

fn dependency(status: &Value) -> &Value {
    status["sources"].as_array().and_then(|sources| sources.iter().find(|source| source["source"] == "dependency"))
        .unwrap_or(&Value::Null)
}

fn listed_passes(source: &Value) -> u64 {
    let db = rusqlite::Connection::open_with_flags(source["path"].as_str().unwrap(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.busy_timeout(Duration::from_secs(3)).unwrap();
    db.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='dependency_listed_scans'", [], |row| row.get(0)).unwrap()
}

#[test]
fn atlas_mcp_listed_plan_changes_do_not_rescan_unchanged_files() {
    let fixture = Fixture::new();
    let shared = fixture.first.parent().unwrap().join("Recorded headers");
    fs::create_dir(&shared).unwrap();
    let header = shared.join("Listed.hpp");
    fs::write(&header, "struct ListedBefore {};").unwrap();
    let report = fixture.first.join("build/editor/src.cpp.dep.json");
    let tu = fixture.first.join("src/math.cpp");
    msvc_dependencies(&report, &tu, &[&header]);
    let mut client = fixture.client(&fixture.first, None, None, false, "2025-11-25");
    client.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    let initial = client.wait(|status| dependency(status)["state"] == "ready");
    let original = dependency(&initial);
    assert_eq!(listed_passes(original), 1);

    // Changes to the report bytes advance discovery without changing its
    // file list or emitting events under the external container.
    let advance_tick = |client: &mut Client| {
        let sha = dependency(&client.status())["provenance"][0]["sha256"].clone();
        OpenOptions::new().append(true).open(&report).unwrap().write_all(b"\n").unwrap();
        client.wait(|status| {
            let next = &dependency(status)["provenance"][0]["sha256"];
            next.is_string() && *next != sha
        })
    };
    for _ in 0..2 {
        let unchanged = advance_tick(&mut client);
        assert_eq!(dependency(&unchanged)["index_key"], original["index_key"]);
        assert_eq!(listed_passes(dependency(&unchanged)), 1);
    }

    let writer = OpenOptions::new().read(true).write(true)
        .open(Path::new(original["path"].as_str().unwrap()).with_file_name("writer.lock")).unwrap();
    fs2::FileExt::lock_exclusive(&writer).unwrap();
    fs::write(&header, "struct ListedAfterWrite {};").unwrap();
    advance_tick(&mut client);
    assert_eq!(listed_passes(original), 1);
    assert!(client.tool("code_symbol", json!({"scope":"dependencies","name":"ListedAfterWrite"}), false)["definitions"].as_array().unwrap().is_empty());
    drop(writer);
    let changed = client.wait(|status| dependency(status)["revision"].as_u64() > original["revision"].as_u64());
    assert_eq!(listed_passes(dependency(&changed)), 2);
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"ListedAfterWrite"}), false)["definitions"][0]["absolute_path"], json!(header));

    let moved = shared.join("Renamed.hpp");
    fs::rename(&header, &moved).unwrap();
    let absent = client.wait(|status| dependency(status)["missing_files"] == 1);
    assert_eq!(dependency(&absent)["state"], "unavailable");
    assert_eq!(listed_passes(dependency(&absent)), 3);
    msvc_dependencies(&report, &tu, &[&moved]);
    let new_plan = client.wait(|status| dependency(status)["state"] == "ready" && dependency(status)["index_key"] != original["index_key"]);
    assert_eq!(listed_passes(dependency(&new_plan)), 1);
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"ListedAfterWrite"}), false)["definitions"][0]["absolute_path"], json!(moved));
}

#[test]
fn atlas_mcp_listed_reserve_recovers_without_native_events() {
    let fixture = Fixture::new();
    let shared = fixture.first.parent().unwrap().join("Missed event headers");
    fs::create_dir(&shared).unwrap();
    let header = shared.join("Reserve.hpp");
    fs::write(&header, "struct BeforeReserve {};").unwrap();
    msvc_dependencies(&fixture.first.join("build/editor/src.cpp.dep.json"), &fixture.first.join("src/math.cpp"), &[&header]);
    let mut cmd = fixture.command(&fixture.first);
    cmd.env("LORE_ATLAS_TEST_LISTED_FALLBACK_SECS", "5").args(["mcp", "serve"]);
    let mut client = Client::start(cmd, None, false, "2025-11-25");
    client.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    let before = client.wait(|status| dependency(status)["state"] == "ready");
    assert_eq!(listed_passes(dependency(&before)), 1);
    fs::write(&header, "struct RescuedByReserve {};").unwrap();
    let after = client.wait(|status| dependency(status)["revision"].as_u64() > dependency(&before)["revision"].as_u64());
    assert_eq!(listed_passes(dependency(&after)), 2);
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"RescuedByReserve"}), false)["definitions"][0]["absolute_path"], json!(header));
    let status = fixture.command(&fixture.first).args(["atlas", "watch", "status", "--json"]).output().unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["state"], "ready");
}

#[test]
fn atlas_mcp_reuses_observed_dependencies_and_reports_disappearance() {
    use sha2::{Digest, Sha256};

    let fixture = Fixture::new();
    let include = codelore::config::simplify(&fixture.temp.path().canonicalize().unwrap()).join("Shared dependency");
    fs::create_dir(&include).unwrap();
    let header = include.join("PackApi.hpp");
    fs::write(&header, "struct ObservedPack {};\n").unwrap();
    let report = fixture.first.join("build/editor/math.cpp.dep.json");
    msvc_dependencies(&report, &fixture.first.join("src/math.cpp"), &[&header]);
    msvc_dependencies(&fixture.second.join("build/editor/other.cpp.dep.json"), &fixture.second.join("src/other.cpp"), &[&header]);

    let key_path = |path: &Path| {
        let path = path.to_string_lossy().replace('\\', "/");
        if cfg!(windows) { path.to_lowercase() } else { path }
    };
    let identity = json!(["atlas-build-dependencies-v1", codelore::atlas::store::ATLAS_VERSION,
        key_path(&include), "observed_files", null, [key_path(&header)]]);
    let key = format!("{:x}", Sha256::digest(identity.to_string()));
    let dir = fixture.cache.join("atlas-dependencies").join(&key);
    fs::create_dir_all(&dir).unwrap();
    let lock = || OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("writer.lock")).unwrap();
    let writer = lock();
    fs2::FileExt::lock_exclusive(&writer).unwrap();

    let mut client = fixture.client(&fixture.first, None, None, false, "2025-11-25");
    client.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    let first = client.wait(|status| status["state"] == "ready" && dependency(status)["state"] == "indexing");
    assert_eq!(dependency(&first)["index_key"], key);
    let search = client.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    assert_eq!(search["results"][0]["path"], "src/math.cpp");
    assert!(!dir.join("atlas.sqlite").exists());
    drop(writer);

    let first = client.wait(|status| dependency(status)["state"] == "ready");
    assert_eq!(dependency(&first)["implementation"], "unconfirmed");
    assert!(dependency(&first)["source_root"].is_null());
    assert_eq!(dependency(&first)["scope"], "observed_files");
    assert_eq!(dependency(&first)["provenance"][0]["field"], "Data.Includes");
    let search = client.tool("code_search", json!({"query":"ObservedPack","path":"PackApi.hpp","lang":"cpp","text_only":true}), false);
    assert_eq!(search["results"][0]["absolute_path"], json!(header));
    assert_eq!(search["results"][0]["source"], "dependency");
    let project = client.tool("code_search", json!({"scope":"project","query":"ObservedPack","text_only":true}), false);
    assert!(project["results"].as_array().unwrap().iter().all(|hit| hit["source"] == "project"));

    let writer = lock();
    fs2::FileExt::lock_exclusive(&writer).unwrap();
    let mut second = fixture.client(&fixture.second, None, None, false, "2025-11-25");
    second.tool("code_search", json!({"query":"ObservedPack","text_only":true}), false);
    let shared = second.wait(|status| dependency(status)["state"] == "ready");
    for field in ["path", "index_key", "revision", "refreshed_at"] {
        assert_eq!(dependency(&shared)[field], dependency(&first)[field]);
    }
    assert_ne!(dependency(&shared)["provenance"], dependency(&first)["provenance"]);
    let search = second.tool("code_search", json!({"scope":"dependencies","query":"ObservedPack","text_only":true}), false);
    assert_eq!(search["results"][0]["absolute_path"], json!(header));
    drop(writer);

    let replacement = include.join("CurrentApi.hpp");
    fs::write(&replacement, "struct CurrentPack {};\n").unwrap();
    msvc_dependencies(&report, &fixture.first.join("src/math.cpp"), &[&replacement]);
    let updated = client.wait(|status| dependency(status)["state"] == "ready" && dependency(status)["index_key"] != key);
    assert_eq!(dependency(&updated)["counts"]["files"], 1);
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"ObservedPack"}), false)["definitions"], json!([]));
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"CurrentPack"}), false)["definitions"][0]["absolute_path"], json!(replacement));

    fs::remove_file(&replacement).unwrap();
    let disappeared = client.wait(|status| dependency(status)["state"] == "unavailable" && dependency(status)["missing_files"] == 1);
    assert_eq!(dependency(&disappeared)["counts"]["files"], 0);
    fs::remove_file(&report).unwrap();
    client.wait(|status| status["dependencies"]["state"] == "empty");
    assert_eq!(dependency(&second.status())["index_key"], key);
}

#[test]
fn atlas_mcp_confirmed_build_root_returns_cpp_and_respects_nested_workspace() {
    let fixture = Fixture::new();
    let project = fixture.first.join("src");
    let consumer = project.join("math.cpp");
    let root = codelore::config::simplify(&fixture.temp.path().canonicalize().unwrap()).join("custom-engine");
    let include = root.join("include");
    fs::create_dir_all(&include).unwrap();
    let header = include.join("TinyApi.hpp");
    let implementation = root.join("TinyCore.cpp");
    let unobserved = include.join("Unobserved.hpp");
    fs::write(&header, "struct TinyDependency {};\n").unwrap();
    fs::write(&implementation, "int ConfirmedImplementation() {\n    return 42;\n}\n").unwrap();
    fs::write(&unobserved, "struct MerelyOnSearchPath {};\n").unwrap();
    let other_variant = root.join("OtherVariant.cpp");
    fs::write(&other_variant, "int OtherBuildOnly() {\n    return 1;\n}\n").unwrap();
    let manifest = fixture.first.join("build/atlas-build-sources.json");
    let write_manifest = |version| emit_json(&manifest, json!({
        "version":1,"contexts":[{"id":"editor-x64","translation_units":[consumer],
            "sources":[{"root":root,"version":version,"files":[header]}]}]
    }));
    write_manifest("first");
    emit_json(&fixture.first.join("build/compile_commands.json"), json!([
        {"directory":fixture.first,"file":"src/math.cpp","arguments":["clang++","-I",include,"-c","src/math.cpp"]},
        {"directory":fixture.first,"file":implementation,"arguments":["clang++","-c",implementation]},
    ]));
    emit_json(&fixture.first.join("build/other/compile_commands.json"), json!([
        {"directory":fixture.first,"file":"src/math.cpp","arguments":["clang++","-c","src/math.cpp"]},
        {"directory":fixture.first,"file":other_variant,"arguments":["clang++","-c",other_variant]},
    ]));
    msvc_dependencies(&fixture.first.join("build/math.cpp.dep.json"), &consumer, &[&header]);
    let outside_tu = fixture.first.join("outside.cpp");
    fs::write(&outside_tu, "int OutsideWorkspace() {}\n").unwrap();
    msvc_dependencies(&fixture.first.join("build/outside.cpp.dep.json"), &outside_tu, &[&unobserved]);

    let mut client = fixture.client(fixture.temp.path(), None, Some(vec![project.clone()]), true, "2025-11-25");
    client.tool("code_search", json!({"query":"ConfirmedImplementation","text_only":true}), false);
    let ready = client.wait(|status| status["snapshot"]["state"] == "ready" && dependency(status)["state"] == "ready");
    let source = dependency(&ready);
    assert_eq!(source["scope"], "declared_target_files");
    assert_eq!(source["source_root"], json!(root));
    assert_eq!(source["counts"]["files"], 2);
    assert_eq!(source["search_paths"], json!([include]));
    assert_eq!(ready["root"], json!(project));
    let result = client.tool("code_search", json!({"scope":"dependencies","query":"ConfirmedImplementation","lang":"cpp","path":"TinyCore.cpp","text_only":true}), false);
    assert_eq!(result["results"][0]["absolute_path"], json!(implementation));
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"MerelyOnSearchPath"}), false)["definitions"], json!([]));
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"OtherBuildOnly"}), false)["definitions"], json!([]));
    assert_eq!(client.tool("code_symbol", json!({"scope":"project","name":"OutsideWorkspace"}), false)["definitions"], json!([]));

    write_manifest("second");
    let next = client.wait(|status| dependency(status)["state"] == "ready" && dependency(status)["version"] == "second");
    assert_ne!(dependency(&next)["path"], source["path"]);
}

#[test]
fn atlas_mcp_uses_ninja_link_edges_and_matching_object_dependencies() {
    let fixture = Fixture::new();
    let engine = codelore::config::simplify(&fixture.temp.path().canonicalize().unwrap()).join("Ninja Engine");
    let build = fixture.first.join("build");
    fs::create_dir_all(&engine).unwrap();
    fs::create_dir(&build).unwrap();
    let header = engine.join("PublicApi.hpp");
    let implementation = engine.join("LinkedCore.cpp");
    let irrelevant = engine.join("OrderOnly.cpp");
    let unobserved = engine.join("OnIncludePath.hpp");
    let declared = engine.join("CMakeLists.txt");
    fs::write(&declared, "add_library(core LinkedCore.cpp)\n").unwrap();
    fs::write(&header, "struct RecordedNinjaHeader {};\n").unwrap();
    fs::write(&implementation, "int LinkedNinjaImplementation() {\n    return 1;\n}\n").unwrap();
    fs::write(&irrelevant, "int OrderOnlyImplementation() {\n    return 2;\n}\n").unwrap();
    fs::write(&unobserved, "struct MerelyAnIncludePath {};\n").unwrap();
    let main = fixture.first.join("src/math.cpp");
    let object = build.join("math.obj");
    fs::write(&object, b"fixture object").unwrap();
    let ninja_path = |path: &Path| path.to_string_lossy().replace('$', "$$").replace(':', "$:").replace(' ', "$ ");
    fs::write(build.join("build.ninja"), format!(
        concat!(
            "build build.ninja: RERUN_CMAKE | {}\n",
            "build math.obj: CXX_COMPILER__consumer_Release {}\n",
            "build core.obj: CXX_COMPILER__core_Release {}\n",
            "build ignored.obj: CXX_COMPILER__order_Release {}\n",
            "build core.lib: CXX_STATIC_LIBRARY_LINKER__core_Release core.obj\n",
            "build ignored.lib: CXX_STATIC_LIBRARY_LINKER__order_Release ignored.obj\n",
            "build consumer.exe: CXX_EXECUTABLE_LINKER__consumer_Release math.obj | core.lib || ignored.lib\n",
            "  LINK_LIBRARIES = core.lib\n",
        ),
        ninja_path(&declared), ninja_path(&main), ninja_path(&implementation), ninja_path(&irrelevant),
    )).unwrap();
    emit_json(&build.join("compile_commands.json"), json!([
        {"directory":build,"file":main,"output":"math.obj","arguments":["clang++","-I",engine,"-c",main]},
        {"directory":build,"file":implementation,"output":"core.obj","arguments":["clang++","-c",implementation]},
        {"directory":build,"file":irrelevant,"output":"ignored.obj","arguments":["clang++","-c",irrelevant]},
    ]));
    let stamp = object.metadata().unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let stamp = (if cfg!(windows) { stamp / 100 - 9_782_968_000_000_000 } else { stamp }) as u64;
    let write_log = |stamp: u64| {
        let mut bytes = b"# ninjadeps\n\x04\0\0\0".to_vec();
        for (id, path) in ["math.obj", header.to_str().unwrap()].into_iter().enumerate() {
            let size = (path.len()+3)/4*4;
            bytes.extend_from_slice(&((size+4) as u32).to_le_bytes());
            bytes.extend_from_slice(path.as_bytes());
            bytes.resize(bytes.len() + size - path.len(), 0);
            bytes.extend_from_slice(&(!(id as u32)).to_le_bytes());
        }
        bytes.extend_from_slice(&0x8000_0010u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&stamp.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        fs::write(build.join(".ninja_deps"), bytes).unwrap();
    };
    write_log(stamp);

    let mut client = fixture.client(&fixture.first, None, None, false, "2025-11-25");
    client.tool("code_search", json!({"query":"LinkedNinjaImplementation","text_only":true}), false);
    let ready = client.wait(|status| dependency(status)["state"] == "ready" && dependency(status)["counts"]["files"] == 2);
    let source = dependency(&ready);
    assert_eq!(source["source_root"], json!(engine));
    assert_eq!(source["implementation"], "available");
    assert_eq!(source["provenance_count"], 3);
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"RecordedNinjaHeader"}), false)["definitions"][0]["absolute_path"], json!(header));
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"OrderOnlyImplementation"}), false)["definitions"], json!([]));
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"MerelyAnIncludePath"}), false)["definitions"], json!([]));
    write_log(stamp + 1);
    let changed = client.wait(|status| dependency(status)["state"] == "ready" && dependency(status)["counts"]["files"] == 1);
    assert_ne!(dependency(&changed)["index_key"], source["index_key"]);
    assert_eq!(client.tool("code_symbol", json!({"scope":"dependencies","name":"RecordedNinjaHeader"}), false)["definitions"], json!([]));
}

fn external<'a>(status: &'a Value, name: &str) -> &'a Value {
    status["sources"].as_array().and_then(|sources| sources.iter().find(|source| source["name"] == name))
        .unwrap_or(&Value::Null)
}

fn address(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { path.to_lowercase() } else { path }
}

#[test]
fn atlas_mcp_and_cli_share_local_source_config() {
    let fixture = Fixture::new();
    let runtime = fixture.temp.path().join("Installed runtime");
    fs::create_dir(&runtime).unwrap();
    let file = runtime.join("Runtime.cpp");
    fs::write(&file, "int LocalRuntime() { return 1; }\n").unwrap();
    let config = concat!("version = 1\n[atlas]\nenabled = true\nsemantic = false\n",
        "[atlas.sources.runtime]\nroot = \"../Missing runtime\"\n");
    fs::write(fixture.first.join("lore.toml"), config).unwrap();
    fs::write(fixture.first.join("lore.local.toml"),
        "[atlas.sources.runtime]\nroot = \"../Installed runtime\"\n").unwrap();
    fs::write(fixture.second.join("lore.toml"), "version = 1\n[atlas]\nenabled = true\n").unwrap();

    let cli = || {
        let output = fixture.command(&fixture.first.join("src"))
            .args(["--json", "atlas", "status", "--snapshot"]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let configured = cli();
    let configured_path = external(&configured, "runtime")["configured_path"].as_str().unwrap();
    assert_eq!(Path::new(configured_path).canonicalize().unwrap(), runtime.canonicalize().unwrap());

    let mut local = fixture.client(&fixture.first.join("src"), None, None, false, "2025-11-25");
    local.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    let ready = local.wait(|status| external(status, "runtime")["state"] == "ready");
    let dependency = external(&ready, "runtime");
    assert_eq!(external(&cli(), "runtime")["index_key"], dependency["index_key"]);
    assert_eq!(dependency["provenance"][0]["artifact"], json!(fixture.first.join("lore.local.toml")));
    let search = |client: &mut Client, query| client.tool("code_search",
        json!({"source":"runtime","query":query,"text_only":true}), false);
    assert_eq!(search(&mut local, "LocalRuntime")["results"][0]["path"], "Runtime.cpp");
    fs::write(&file, "int UpdatedLocalRuntime() { return 42; }\n").unwrap();
    local.wait(|status| external(status, "runtime")["revision"].as_u64() > dependency["revision"].as_u64());
    assert_eq!(search(&mut local, "UpdatedLocalRuntime")["results"][0]["path"], "Runtime.cpp");

    let mut other = fixture.client(&fixture.second, None, None, false, "2025-11-25");
    other.tool("code_search", json!({"query":"BetaFunction","text_only":true}), false);
    let ready = other.wait(|status| status["snapshot"]["state"] == "ready");
    assert!(external(&ready, "runtime").is_null());
    let result = other.tool("code_search", json!({"query":"BetaFunction","text_only":true}), false);
    assert_eq!(result["results"][0]["path"], "src/other.cpp");
}

#[test]
fn atlas_cli_reports_invalid_local_config_with_its_filename() {
    let fixture = Fixture::new();
    fs::write(fixture.first.join("lore.toml"), "version = 1\n[atlas]\nenabled = true\n").unwrap();
    let local = fixture.first.join("lore.local.toml");
    for (config, diagnostic) in [
        ("[atlas]\nenabled = false\n", "atlas.enabled"),
        ("[atlas.sources.runtime]\nroot = 42\n", "atlas.sources.runtime.root"),
        ("[atlas.sources.runtime", "not valid TOML"),
    ] {
        fs::write(&local, config).unwrap();
        let output = fixture.command(&fixture.first).args(["atlas", "status", "--snapshot"]).output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("lore.local.toml") && stderr.contains(diagnostic), "{stderr}");
    }
}

#[test]
fn atlas_mcp_named_vector_fill_preserves_text_when_model_is_missing() {
    let fixture = Fixture::new();
    let library = fixture.first.parent().unwrap().join("Library");
    fs::create_dir(&library).unwrap();
    fs::write(library.join("Feature.cpp"), "int OptionalFeature() { return 42; }\n").unwrap();
    fs::write(fixture.first.join("lore.toml"), concat!(
        "version = 1\n[embed]\nmodel = \"multilingual-e5-base\"\n[atlas]\nenabled = true\n",
        "[atlas.sources.library]\nroot = \"../Library\"\n",
    )).unwrap();
    let command = || {
        let mut command = fixture.command(&fixture.first);
        command.env("LORE_MODEL_DIR", fixture.temp.path().join("missing-weights"));
        command
    };
    let index = || command().args(["atlas", "index", "--source", "library", "--json"]).output().unwrap();
    let cold = index();
    assert!(!cold.status.success());
    let state: Value = serde_json::from_slice(&cold.stdout).unwrap();
    assert_eq!(external(&state, "library")["state"], "unindexed");

    let mut mcp = command();
    mcp.args(["mcp", "serve"]);
    let mut client = Client::start(mcp, None, false, "2025-11-25");
    client.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    let ready = client.wait(|state| external(state, "library")["state"] == "ready");
    let state = external(&ready, "library");
    assert_eq!(state["model"], "multilingual-e5-base");
    assert_eq!(state["model_installed"], false);
    assert_eq!(state["vectors"], 0);
    assert_eq!(state["vectors_missing"], 1);

    let result = index();
    assert!(!result.status.success());
    let output: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(output["notes"].to_string().contains("lore model fetch"), "{output}");
    assert_eq!(external(&output, "library")["revision"], state["revision"]);
    assert_eq!(external(&output, "library")["vectors_generated"], 0);
    let search = client.tool("code_search", json!({"source":"library","query":"OptionalFeature","text_only":true}), false);
    assert_eq!(search["results"][0]["path"], "Feature.cpp");
}

#[test]
fn atlas_mcp_configured_folder_shares_its_ready_snapshot_between_projects() {
    use sha2::{Digest, Sha256};

    let fixture = Fixture::new();
    let shared = fixture.first.parent().unwrap().join("Common Source");
    fs::create_dir_all(shared.join("src")).unwrap();
    fs::create_dir_all(shared.join("private")).unwrap();
    let file = shared.join("src/runtime.cpp");
    fs::write(&file, "int PortableRuntime() {\n    return 12;\n}\n").unwrap();
    fs::write(shared.join("src/reference.note"), "ExtensionMarker\n").unwrap();
    fs::write(shared.join("private/secret.cpp"), "int PrivateFolderMarker() {}\n").unwrap();
    fs::write(shared.join("src/large.cpp"), format!("int ExceedsLimit() {{}}\n{}", "x".repeat(5000))).unwrap();
    assert!(!shared.join(".git").exists());
    fs::write(fixture.first.join("lore.toml"), concat!(
        "version = 1\n[atlas]\nenabled = true\n",
        "[atlas.sources.runtime]\nroot = \"../Common Source\"\n",
        "include = [\"NOTE\"]\nexclude = [\"private/\"]\nmax_kib = 4\n",
    )).unwrap();
    fs::write(fixture.second.join("lore.toml"), format!(
        "version = 1\n[atlas]\nenabled = true\n[atlas.sources.shared]\nroot = {}\ninclude = [\".note\"]\nexclude = [\"private\"]\nmax_kib = 4\n",
        json!(shared.to_string_lossy().replace('\\', "/")),
    )).unwrap();

    let identity = json!(["atlas-configured-source-v1", codelore::atlas::store::ATLAS_VERSION,
        address(&shared), [["note"], ["private"], 4]]);
    let key = format!("{:x}", Sha256::digest(identity.to_string()));
    let dir = fixture.cache.join("atlas-dependencies").join(&key);
    fs::create_dir_all(&dir).unwrap();
    let lock = || OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("writer.lock")).unwrap();
    let writer = lock();
    fs2::FileExt::lock_exclusive(&writer).unwrap();

    let mut first = fixture.client(&fixture.first, None, None, false, "2025-11-25");
    assert_eq!(external(&first.status(), "runtime")["state"], "unindexed");
    first.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    first.wait(|state| state["snapshot"]["state"] == "ready" && external(state, "runtime")["state"] == "indexing");
    let project = first.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    assert_eq!(project["results"][0]["path"], "src/math.cpp");
    assert!(!dir.join("atlas.sqlite").exists());
    drop(writer);

    let ready = first.wait(|state| external(state, "runtime")["state"] == "ready");
    let runtime = external(&ready, "runtime");
    assert_eq!(runtime["index_key"], key);
    assert_eq!(runtime["source"], "external");
    assert_eq!(runtime["scope"], "configured_tree");
    assert_eq!(runtime["source_root"], json!(shared));
    assert_eq!(runtime["counts"]["files"], 2);
    assert_eq!(runtime["provenance"][0]["field"], "atlas.sources.runtime");
    let tree_passes = || {
        let db = rusqlite::Connection::open_with_flags(dir.join("atlas.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='dependency_tree_scans'", [], |row| row.get::<_, u64>(0)).unwrap()
    };
    let initial_passes = tree_passes();
    assert_eq!(initial_passes, 1);
    let notice = first.tool("code_search", json!({"source":"runtime","query":"ExtensionMarker","lang":"text","text_only":true}), false);
    assert_eq!(notice["results"][0]["path"], "src/reference.note");
    for excluded in ["PrivateFolderMarker", "ExceedsLimit"] {
        let response = first.tool("code_symbol", json!({"source":"runtime","name":excluded}), false);
        assert_eq!(response["definitions"], json!([]));
    }

    let writer = lock();
    fs2::FileExt::lock_exclusive(&writer).unwrap();
    let mut second = fixture.client(&fixture.second, None, None, false, "2025-11-25");
    second.tool("code_search", json!({"query":"BetaFunction","text_only":true}), false);
    let attached = second.wait(|state| external(state, "shared")["state"] == "ready");
    let shared_status = external(&attached, "shared");
    for field in ["path", "index_key", "revision", "refreshed_at", "source_id"] {
        assert_eq!(shared_status[field], runtime[field], "{field}");
    }
    let result = second.tool("code_search", json!({"source":"shared","query":"PortableRuntime","path":"src/","lang":"cpp","text_only":true}), false);
    assert_eq!(result["results"][0]["source_name"], "shared");
    assert_eq!(address(Path::new(result["results"][0]["absolute_path"].as_str().unwrap())), address(&file));
    let symbols = second.tool("code_symbols", json!({"source":"shared","file":result["results"][0]["absolute_path"]}), false);
    assert_eq!(symbols["definitions"][0]["name"], "PortableRuntime");
    assert_eq!(symbols["definitions"][0]["source_name"], "shared");
    drop(writer);
    // Exercise several dependency-worker ticks with both projects attached.
    // SQLite's marker advances only after a successful tree pass.
    std::thread::sleep(Duration::from_millis(4500));
    assert_eq!(external(&second.status(), "shared")["revision"], shared_status["revision"]);
    assert_eq!(tree_passes(), initial_passes);

    fs::write(&file, "int UpdatedPortableRuntime() {\n    return 22;\n}\n").unwrap();
    let live_a = first.wait(|state| external(state, "runtime")["revision"].as_u64() > runtime["revision"].as_u64());
    let live_b = second.wait(|state| external(state, "shared")["revision"] == external(&live_a, "runtime")["revision"]);
    assert_eq!(external(&live_b, "shared")["path"], shared_status["path"]);
    let updated = second.tool("code_symbol", json!({"source":"shared","name":"UpdatedPortableRuntime"}), false);
    assert_eq!(address(Path::new(updated["definitions"][0]["absolute_path"].as_str().unwrap())), address(&file));
    assert!(tree_passes() > initial_passes);

    let offline = shared.with_extension("offline");
    fs::rename(&shared, &offline).unwrap();
    first.wait(|state| external(state, "runtime")["state"] == "unavailable");
    second.wait(|state| external(state, "shared")["state"] == "unavailable");
    fs::write(offline.join("src/runtime.cpp"), "int ReturnedPortableRuntime() {\n    return 33;\n}\n").unwrap();
    fs::rename(&offline, &shared).unwrap();
    let returned = second.wait(|state| external(state, "shared")["revision"].as_u64() > external(&live_a, "runtime")["revision"].as_u64());
    assert_eq!(external(&returned, "shared")["index_key"], key);
    let returned = second.tool("code_symbol", json!({"source":"shared","name":"ReturnedPortableRuntime"}), false);
    assert_eq!(address(Path::new(returned["definitions"][0]["absolute_path"].as_str().unwrap())), address(&file));
}

#[test]
fn atlas_mcp_named_scopes_filter_and_deduplicate_alongside_existing_sources() {
    let fixture = Fixture::new();
    let checkout = fixture.first.parent().unwrap().join("Library checkout");
    let other = fixture.first.parent().unwrap().join("Second library");
    let unreal = fixture.first.parent().unwrap().join("Configured engine");
    for root in [&checkout, &other] {
        fs::create_dir_all(root.join("src")).unwrap();
    }
    git(&checkout, &["init", "-q"]);
    let header = checkout.join("src/Api.hpp");
    fs::write(&header, "struct SharedApiMarker {};\n").unwrap();
    fs::write(checkout.join("src/Internal.cpp"), "int CheckoutInternal() {}\n").unwrap();
    git(&checkout, &["add", "."]);
    fs::write(other.join("src/Api.hpp"), "struct SecondRootMarker {};\n").unwrap();
    emit_json(&unreal.join("Engine/Build/Build.version"), json!({
        "MajorVersion":5,"MinorVersion":5,"PatchVersion":0,"Changelist":1,"BranchName":"fixture",
    }));
    fs::create_dir_all(unreal.join("Engine/Source/Runtime")).unwrap();
    fs::write(unreal.join("Engine/Source/Runtime/Legacy.hpp"), "struct ConfiguredUnrealMarker {};\n").unwrap();
    msvc_dependencies(&fixture.first.join("build/math.cpp.dep.json"), &fixture.first.join("src/math.cpp"), &[&header]);

    let config = |sdk: bool| {
        let mut text = format!(
            "version = 1\n[atlas]\nenabled = true\n[atlas.unreal]\nroot = {}\n",
            json!(unreal.to_string_lossy().replace('\\', "/")),
        );
        if sdk {
            text.push_str("[atlas.sources.sdk]\nroot = \"../Library checkout\"\n");
        }
        text.push_str(concat!(
            "[atlas.sources.public-sdk]\nroot = \"../Library checkout\"\nexclude = [\"src/Internal.cpp\"]\n",
            "[atlas.sources.other]\nroot = \"../Second library\"\n",
        ));
        fs::write(fixture.first.join("lore.toml"), text).unwrap();
    };
    config(true);
    let index = fixture.command(&fixture.first).args(["atlas","index","--source","unreal","--text-only"]).output().unwrap();
    assert!(index.status.success(), "{}", String::from_utf8_lossy(&index.stderr));

    let mut client = fixture.client(&fixture.first, None, None, false, "2025-11-25");
    client.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    let ready = client.wait(|state| {
        state["snapshot"]["state"] == "ready" && dependency(state)["state"] == "ready"
            && ["sdk", "public-sdk", "other"].iter().all(|name| external(state, name)["state"] == "ready")
    });
    let sdk = external(&ready, "sdk");
    let limited = external(&ready, "public-sdk");
    assert_eq!(sdk["source_root"], limited["source_root"]);
    assert_ne!(sdk["index_key"], limited["index_key"]);
    assert_ne!(sdk["index_key"], external(&ready, "other")["index_key"]);
    assert_eq!(sdk["counts"]["files"], 2);
    assert_eq!(limited["counts"]["files"], 1);

    let search = client.tool("code_search", json!({"query":"SharedApiMarker","limit":30,"text_only":true}), false);
    let matches = search["results"].as_array().unwrap().iter().filter(|hit| {
        hit["absolute_path"].as_str().is_some_and(|path| address(Path::new(path)) == address(&header))
    }).count();
    assert_eq!(matches, 1, "{search}");
    let definitions = client.tool("code_symbol", json!({"name":"SharedApiMarker"}), false);
    assert_eq!(definitions["definitions"].as_array().unwrap().len(), 1);
    let filtered = client.tool("code_symbol", json!({"source":"public-sdk","name":"CheckoutInternal"}), false);
    assert_eq!(filtered["definitions"], json!([]));

    let legacy = client.tool("code_search", json!({"scope":"unreal","query":"ConfiguredUnrealMarker","text_only":true}), false);
    assert_eq!(legacy["results"][0]["source"], "unreal");
    let list = fixture.command(&fixture.first).args(["atlas","status","--source","public-sdk","--json"]).output().unwrap();
    assert!(list.status.success(), "{}", String::from_utf8_lossy(&list.stderr));
    let list: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(external(&list, "public-sdk")["index_key"], limited["index_key"]);

    config(false);
    assert!(external(&client.status(), "sdk").is_null());
    let missing = client.tool("code_search", json!({"source":"sdk","query":"SharedApiMarker"}), true);
    assert!(missing["error"].as_str().unwrap().contains("invalid Atlas source"));
    fs::rename(&other, other.with_extension("removed")).unwrap();
    let lost = client.wait(|state| external(state, "other")["state"] == "unavailable");
    assert!(external(&lost, "other")["note"].as_str().unwrap().contains("Second library"));
    let project = client.tool("code_search", json!({"query":"AlphaFunction","text_only":true}), false);
    assert_eq!(project["results"][0]["path"], "src/math.cpp");
}
