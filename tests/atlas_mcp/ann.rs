use super::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::UNIX_EPOCH;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

const DIM: usize = 768;
const MODEL: &str = "multilingual-e5-base";

struct Encoder {
    finished: Arc<AtomicBool>,
    queries: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Encoder {
    fn start(fixture: &Fixture, binary: &OsString) -> Self {
        let mut command = Command::new(binary);
        hide(&mut command);
        let version = command.arg("--version").output().unwrap();
        assert!(version.status.success());
        let text = String::from_utf8(version.stdout).unwrap();
        let version = text.trim().strip_prefix("lore ").unwrap();
        let (version, commit) = version.split_once(" (").map_or((version, ""), |(v, c)| (v, c.trim_end_matches(')')));
        let metadata = fs::metadata(binary).unwrap();
        let time = metadata.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let build = format!("{version}+{commit}+{}-{time}/t2048", metadata.len());
        let suffix = format!("{:x}", sha1::Sha1::digest(build.as_bytes()));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = fixture.temp.path().join("models/serve").join(format!("{MODEL}-v1-{}.json", &suffix[..8]));
        emit_json(&state, json!({"port": listener.local_addr().unwrap().port(), "token": "ann-test-token", "build": build}));
        let finished = Arc::new(AtomicBool::new(false));
        let queries = Arc::new(AtomicUsize::new(0));
        let thread = {
            let finished = Arc::clone(&finished);
            let queries = Arc::clone(&queries);
            std::thread::spawn(move || {
                let out = json!({"vectors":[unit_vector()]});
                while !finished.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut conn, _)) => {
                            conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                            let mut request = String::new();
                            if BufReader::new(&conn).read_line(&mut request).unwrap_or_default() == 0 {
                                continue;
                            }
                            let request: Value = serde_json::from_str(&request).unwrap();
                            assert_eq!(request["token"], "ann-test-token");
                            let texts = request["texts"].as_array().unwrap();
                            assert_eq!(texts.len(), 1);
                            assert!(texts[0].as_str().unwrap().starts_with("query: "));
                            queries.fetch_add(1, Ordering::Relaxed);
                            writeln!(conn, "{out}").unwrap();
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(4)),
                        Err(error) => panic!("{error}"),
                    }
                }
            })
        };
        Self { finished, queries, thread: Some(thread) }
    }

    fn count(&self) -> usize {
        self.queries.load(Ordering::Relaxed)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn unit_vector() -> Vec<f32> {
    let mut vector = vec![0.0; DIM];
    vector[0] = 1.0;
    vector
}

fn seed(db: &Path, model: &str, pad: bool) -> Vec<(i64, String, String)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.busy_timeout(Duration::from_secs(3)).unwrap();
    let rows: Vec<(i64, String, String)> = conn.prepare("SELECT id, ehash, path FROM chunks ORDER BY id").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap();
    let blob: Vec<_> = unit_vector().iter().flat_map(|item| item.to_le_bytes()).collect();
    let tx = conn.unchecked_transaction().unwrap();
    for (_, hash, _) in &rows {
        tx.execute("INSERT OR REPLACE INTO vectors VALUES(?1,?2,?3)", rusqlite::params![model, hash, blob]).unwrap();
    }
    if pad {
        tx.execute(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<4097)
             INSERT INTO chunks(path,start,stop,symbols,ehash,body)
             SELECT 'pad',1,1,'',printf('%040x',i),'' FROM n", [],
        ).unwrap();
    }
    tx.commit().unwrap();
    rows
}

fn sidecar(db: &Path, space: &str, rows: &[(i64, String, String)], lost_identity: bool) -> (PathBuf, Value) {
    let conn = rusqlite::Connection::open(db).unwrap();
    let meta = |key| conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get::<_, String>(0)).unwrap();
    let source = meta("dependency_identity");
    let folder = ann_dir(db, space);
    fs::create_dir_all(&folder).unwrap();
    let generation = if lost_identity { "2" } else { "1" }.repeat(64);
    let index_path = folder.join(format!("{generation}.usearch"));
    let key_path = folder.join(format!("{generation}.keys"));
    let index = Index::new(&IndexOptions { dimensions: DIM, metric: MetricKind::IP, quantization: ScalarKind::F16,
        connectivity: 24, expansion_add: 112, expansion_search: 1536, multi: false }).unwrap();
    index.reserve(rows.len()).unwrap();
    let mut keys = Vec::new();
    for (id, hash, name) in rows {
        index.add(*id as u64, &unit_vector()).unwrap();
        keys.extend(id.to_le_bytes());
        let hash = if lost_identity && name.ends_with(".rs") { "0".repeat(40) } else { hash.clone() };
        keys.extend(hash.as_bytes());
    }
    fs::write(&key_path, keys).unwrap();
    let mut serialized = vec![0; index.serialized_length()];
    index.save_to_buffer(&mut serialized).unwrap();
    fs::write(&index_path, serialized).unwrap();
    let stamp = |path: &Path| {
        let meta = fs::metadata(path).unwrap();
        json!({"bytes":meta.len(),"modified_ns":meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64})
    };
    let manifest = json!({"format":1,"metric":"f16_ip","connectivity":24,"expansion_add":112,"generation":generation,
        "binding":{"source_key":format!("dependency:{source}"),"space":space,"vector_model":space,"dimension":DIM,
            "revision":meta("revision"),"refreshed":meta("refreshed"),"vector_generation":vector_generation(&conn,space),"covered":rows.len()},
        "index":stamp(&index_path),"keys":stamp(&key_path)});
    (folder.join("current.json"), manifest)
}

fn ann_dir(db: &Path, space: &str) -> PathBuf {
    db.with_file_name("ann-v1").join(format!("{:x}", Sha256::digest(space.as_bytes())))
}

fn vector_generation(conn: &rusqlite::Connection, model: &str) -> Option<String> {
    let get = |key: &str| conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get::<_, String>(0)).ok();
    get("ann_vector_clock").map(|clock| format!("{clock}:{}", get(&format!("ann_vector_generation:{model}")).unwrap_or_else(|| "0".to_owned())))
}

fn lock(dir: &Path, name: &str) -> fs::File {
    fs::create_dir_all(dir).unwrap();
    let file = OpenOptions::new().create(true).truncate(false).write(true).read(true).open(dir.join(name)).unwrap();
    fs2::FileExt::lock_exclusive(&file).unwrap();
    file
}

fn meaning_paths(result: &Value) -> Vec<&str> {
    result["results"].as_array().unwrap().iter()
        .filter(|hit| hit["via"].as_array().unwrap().contains(&json!("meaning")))
        .map(|hit| hit["path"].as_str().unwrap()).collect()
}

#[test]
fn atlas_ann_cli_and_mcp_share_one_encoding_and_guard_snapshot_candidates() {
    let binary = std::env::var_os("LORE_TEST_ANN_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_lore").into());
    let fixture = Fixture::new();
    let runtime = fixture.first.parent().unwrap().join("Runtime");
    fs::create_dir_all(runtime.join("src")).unwrap();
    fs::write(runtime.join("src/engine.cpp"), "void UShared::Probe() {}\nvoid UEngine::Another() {}\n").unwrap();
    fs::write(runtime.join("src/library.rs"), "pub fn from_runtime() {}\n").unwrap();
    fs::write(fixture.first.join("src/shared.cpp"), "void UShared::Probe() {}\n").unwrap();
    fs::write(fixture.first.join("lore.toml"), "version=1\n[embed]\nprovider=\"builtin\"\nmodel=\"multilingual-e5-base\"\n[atlas]\nenabled=true\n").unwrap();
    fs::write(fixture.first.join("lore.local.toml"), "[atlas.sources.runtime]\nroot=\"../Runtime\"\n").unwrap();
    let command = || {
        let mut cmd = Command::new(&binary);
        hide(&mut cmd);
        cmd.current_dir(&fixture.first).env("LORE_CACHE", &fixture.cache)
            .env("LORE_MODELS", fixture.temp.path().join("models"))
            .env("LORE_MODEL_DIR", fixture.temp.path().join("invalid-model"))
            .env("LORE_MODEL_FETCH", "off").env("LORE_EMBED_SERVE", "on").env("LORE_EMBED_MAX_TOKENS", "2048")
            .env("LORE_ATLAS_SEARCH_TIMINGS", "1").env_remove("LORE_ATLAS").env_remove("LORE_ATLAS_SNAPSHOT");
        cmd
    };
    let cli = |args: &[&str]| {
        let output = command().args(args).output().unwrap();
        assert!(output.status.success(), "{args:?} {}", String::from_utf8_lossy(&output.stdout));
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let encoder = Encoder::start(&fixture, &binary);
    let mut mcp = command();
    mcp.args(["mcp", "serve"]);
    let mut client = Client::start(mcp, None, false, "2025-11-25");
    client.tool("code_search", json!({"query":"UShared::Probe","text_only":true}), false);
    let status = client.wait(|value| value["snapshot"]["state"] == "ready" && external(value, "runtime")["state"] == "ready");
    let project = PathBuf::from(status["path"].as_str().unwrap());
    let source = external(&status, "runtime");
    let db = PathBuf::from(source["path"].as_str().unwrap());
    let space = source["vector_space"].as_str().unwrap();
    // This test deliberately supplies its own generations for reader QA.
    let _pause = lock(&ann_dir(&db, space), "build.lock");
    seed(&project, MODEL, false);
    let rows = seed(&db, space, true);
    assert_eq!(rows.len(), 2);
    let prompt = "entirely latent semantic query";

    let missing = cli(&["atlas","search","UShared::Probe","--source","runtime","--snapshot","--json"]);
    assert_eq!(encoder.count(), 0, "a large source without ANN encoded a query or scanned vectors");
    assert_eq!(external(&missing,"runtime")["semantic_search"]["state"], "ann_missing", "{missing}");
    assert!(meaning_paths(&missing).is_empty());
    assert_eq!(missing["results"][0]["path"], "src/engine.cpp");
    let missing = client.tool("code_search",json!({"source":"runtime","query":"UShared::Probe"}),false);
    assert_eq!(external(&missing,"runtime")["semantic_search"]["state"],"ann_missing");
    assert_eq!(encoder.count(), 0);

    let (pointer, ready) = sidecar(&db, space, &rows, false);
    fs::write(&pointer, "{broken").unwrap();
    let corrupt = client.tool("code_search",json!({"source":"runtime","query":"UShared::Probe"}),false);
    assert_eq!(external(&corrupt,"runtime")["semantic_search"]["state"],"ann_damaged");
    assert_eq!(encoder.count(), 0);
    emit_json(&pointer, ready.clone());
    let joint = client.tool("code_search",json!({"query":"UShared::Probe","limit":20}),false);
    assert_eq!(encoder.count(), 1);
    assert_eq!(external(&joint,"runtime")["semantic_search"]["mode"],"ann");
    assert!(joint["timings_ms"]["encoding"].is_number());
    let hits = joint["results"].as_array().unwrap();
    assert!(hits[..2].iter().all(|hit| hit["symbols"].as_array().unwrap().contains(&json!("UShared::Probe"))));
    assert!(hits[..2].iter().any(|hit| hit["source"] == "project"));
    assert!(hits[..2].iter().any(|hit| hit["source_name"] == "runtime"));
    assert!(meaning_paths(&joint).contains(&"src/shared.cpp"));
    assert!(meaning_paths(&joint).contains(&"src/engine.cpp"));
    let before_empty = encoder.count();
    let empty = client.tool("code_search",json!({"source":"runtime","query":prompt,"path":"RogueDOD/src/","lang":"cpp"}),false);
    assert!(empty["results"].as_array().unwrap().is_empty());
    assert_eq!(encoder.count(), before_empty, "a prefix with no eligible chunks must bypass the encoder");
    assert_eq!(external(&empty,"runtime")["timings_ms"]["prefilter_rows"], 0);
    let filtered = client.tool("code_search",json!({"source":"runtime","query":prompt,"path":"SRC/ENGINE.CPP","lang":"cpp"}),false);
    assert_eq!(meaning_paths(&filtered),["src/engine.cpp"]);
    assert_eq!(external(&filtered,"runtime")["timings_ms"]["prefilter_rows"], 1);
    assert_eq!(external(&filtered,"runtime")["timings_ms"]["sqlite_filter"], 0.0);
    let rust = cli(&["atlas","search",prompt,"--snapshot","--source","runtime","--path","SRC/LIBRARY.RS","--lang","rust","--json"]);
    assert_eq!(meaning_paths(&rust),["src/library.rs"]);
    assert_eq!(external(&rust,"runtime")["timings_ms"]["prefilter_rows"], 1);
    let lang_only = client.tool("code_search",json!({"source":"runtime","query":prompt,"lang":"rust"}),false);
    assert_eq!(meaning_paths(&lang_only),["src/library.rs"]);
    assert!(external(&lang_only,"runtime")["timings_ms"]["prefilter_rows"].is_null());
    let project_only = client.tool("code_search",json!({"scope":"project","query":prompt}),false);
    assert!(meaning_paths(&project_only).contains(&"src/shared.cpp"));
    let before = encoder.count();
    let text = client.tool("code_search",json!({"query":"UShared::Probe","text_only":true}),false);
    assert!(meaning_paths(&text).is_empty());
    assert_eq!(encoder.count(), before);

    let current = rows.iter().find(|(_, _, path)| path == "src/library.rs").unwrap().0;
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.busy_timeout(Duration::from_secs(3)).unwrap();
    conn.execute("UPDATE chunks SET id=-?1 WHERE id=?1", [current]).unwrap();
    let removed_id = client.tool("code_search",json!({"source":"runtime","query":prompt,"lang":"rust"}),false);
    assert!(meaning_paths(&removed_id).is_empty(), "{removed_id}");
    conn.execute("UPDATE chunks SET id=?1 WHERE id=-?1", [current]).unwrap();
    drop(conn);

    let (_, old) = sidecar(&db, space, &rows, true);
    emit_json(&pointer, old);
    let stale_key = client.tool("code_search",json!({"source":"runtime","query":prompt,"path":"src/library.rs","lang":"rust"}),false);
    assert_eq!(external(&stale_key,"runtime")["timings_ms"]["prefilter_rows"], 1);
    assert!(meaning_paths(&stale_key).is_empty());
    let unchanged = client.tool("code_search",json!({"source":"runtime","query":prompt,"lang":"cpp"}),false);
    assert_eq!(meaning_paths(&unchanged),["src/engine.cpp"]);

    let count = encoder.count();
    for key in ["space", "vector_generation", "source_key"] {
        let mut invalid = ready.clone();
        invalid["binding"][key] = json!("other");
        emit_json(&pointer, invalid);
        let response = client.tool("code_search",json!({"source":"runtime","query":"UShared::Probe"}),false);
        assert_eq!(external(&response,"runtime")["semantic_search"]["state"],"ann_stale");
        assert!(meaning_paths(&response).is_empty());
    }
    assert_eq!(encoder.count(), count);
}

#[test]
fn atlas_ann_reuses_and_recovers_published_generations() {
    let binary = std::env::var_os("LORE_TEST_ANN_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_lore").into());
    let fixture = Fixture::new();
    let runtime = fixture.first.parent().unwrap().join("Shared Runtime");
    fs::create_dir_all(&runtime).unwrap();
    let cpp = runtime.join("external.cpp");
    fs::write(&cpp, "void UShared::BeforeEdit() {}\n").unwrap();
    fs::write(runtime.join("library.rs"), "pub fn external_helper() {}\n").unwrap();
    let configure = |root: &Path, model: &str, exclude: &str| {
        fs::write(root.join("lore.toml"), format!(
            "version=1\n[embed]\nprovider=\"builtin\"\nmodel={model:?}\n[atlas]\nenabled=true\n"
        )).unwrap();
        fs::write(root.join("lore.local.toml"), format!(
            "[atlas.sources.runtime]\nroot=\"../Shared Runtime\"\n{exclude}"
        )).unwrap();
    };
    configure(&fixture.first, MODEL, "");
    configure(&fixture.second, MODEL, "");
    let command = |root: &Path| {
        let mut cmd = Command::new(&binary);
        hide(&mut cmd);
        cmd.current_dir(root).env("LORE_CACHE", &fixture.cache)
            .env("LORE_MODELS", fixture.temp.path().join("models"))
            .env("LORE_MODEL_DIR", fixture.temp.path().join("invalid-model"))
            .env("LORE_MODEL_FETCH", "off").env("LORE_EMBED_SERVE", "on").env("LORE_EMBED_MAX_TOKENS", "2048")
            .env_remove("LORE_ATLAS").env_remove("LORE_ATLAS_SNAPSHOT");
        cmd
    };
    let client_for = |root: &Path| {
        let mut cmd = command(root);
        cmd.args(["mcp", "serve"]);
        Client::start(cmd, None, false, "2025-11-25")
    };
    let activate = |client: &mut Client| {
        client.tool("code_search", json!({"source":"runtime","query":"BeforeEdit","text_only":true}), false);
        client.wait(|result| external(result, "runtime")["state"] == "ready")
    };
    let ready = |client: &mut Client| {
        client.wait(|result| external(result, "runtime")["ann"]["state"] == "ready")
    };
    let hybrid = |client: &mut Client| {
        client.tool("code_search", json!({"source":"runtime","query":"latent vector retrieval"}), false)
    };

    let encoder = Encoder::start(&fixture, &binary);
    let mut first = client_for(&fixture.first);
    let state = activate(&mut first);
    let source = external(&state, "runtime");
    let database = PathBuf::from(source["path"].as_str().unwrap());
    let space = source["vector_space"].as_str().unwrap();
    let dir = ann_dir(&database, space);
    let pointer = dir.join("current.json");
    let manifest = || serde_json::from_slice::<Value>(&fs::read(&pointer).unwrap()).unwrap();
    // Emulate a pre-clock schema-2 cache with already available vectors.
    let previous_schema = rusqlite::Connection::open(&database).unwrap();
    previous_schema.busy_timeout(Duration::from_secs(3)).unwrap();
    previous_schema.execute_batch(
        "DROP TRIGGER atlas_ann_clock_INSERT; DROP TRIGGER atlas_ann_clock_DELETE; DROP TRIGGER atlas_ann_clock_UPDATE;
         DELETE FROM meta WHERE key='ann_vector_clock';"
    ).unwrap();
    drop(previous_schema);
    let initial = seed(&database, space, true);
    assert_eq!(initial.len(), 2);
    let state = ready(&mut first);
    let rev = external(&state, "runtime")["revision"].clone();
    let original = manifest();
    assert!(original["binding"]["vector_generation"].is_string(), "{original}");
    assert_eq!(encoder.count(), 0, "ANN construction must only use SQLite vectors");
    let cli = command(&fixture.first).args(["atlas","status","--source","runtime","--snapshot","--json"]).output().unwrap();
    assert!(cli.status.success(), "{}", String::from_utf8_lossy(&cli.stdout));
    let cli: Value = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(external(&cli, "runtime")["ann"]["generation"], original["generation"]);

    let mut second = client_for(&fixture.second);
    activate(&mut second);
    let second_state = ready(&mut second);
    assert_eq!(external(&second_state, "runtime")["ann"]["generation"], original["generation"]);
    assert_eq!(external(&second_state, "runtime")["path"], source["path"]);
    std::thread::scope(|scope| {
        let a = scope.spawn(|| hybrid(&mut first));
        let b = scope.spawn(|| hybrid(&mut second));
        assert_eq!(meaning_paths(&a.join().unwrap()).len(), 2);
        assert_eq!(meaning_paths(&b.join().unwrap()).len(), 2);
    });
    assert_eq!(encoder.count(), 2);

    let conn = rusqlite::Connection::open(&database).unwrap();
    conn.busy_timeout(Duration::from_secs(3)).unwrap();
    let hash = &initial[0].1;
    let blob: Vec<_> = unit_vector().iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut invalid = blob.clone();
    invalid[..4].copy_from_slice(&f32::NAN.to_le_bytes());
    conn.execute("UPDATE vectors SET vec=?1 WHERE model=?2 AND hash=?3", rusqlite::params![invalid, space, hash]).unwrap();
    let damaged = first.wait(|result| external(result, "runtime")["ann"]["state"] == "error");
    assert_eq!(external(&damaged, "runtime")["revision"], rev);
    assert!(external(&damaged, "runtime")["ann"]["error"].as_str().unwrap().contains("invalid active SQLite vector"));
    assert_eq!(manifest(), original);
    assert!(meaning_paths(&hybrid(&mut second)).is_empty());

    conn.execute("UPDATE vectors SET vec=?1 WHERE model=?2 AND hash=?3", rusqlite::params![blob, space, hash]).unwrap();
    let repaired = ready(&mut second);
    assert_eq!(external(&repaired, "runtime")["revision"], rev);
    let generation = manifest();
    assert_ne!(generation["generation"], original["generation"]);
    assert_ne!(generation["binding"]["vector_generation"], original["binding"]["vector_generation"]);
    assert_eq!(generation["binding"]["covered"], original["binding"]["covered"]);

    // Stop the real Windows child after files are durable but before it can
    // acquire the SQLite publication transaction.
    let scheduler = lock(&dir, "start.lock");
    let writer = lock(&dir, "build.lock");
    conn.execute("UPDATE vectors SET vec=?1 WHERE model=?2 AND hash=?3", rusqlite::params![blob, space, hash]).unwrap();
    let mut binding = generation["binding"].clone();
    binding["vector_generation"] = json!(vector_generation(&conn, space));
    emit_json(&dir.join("request.json"), json!({"root":runtime, "binding":binding}));
    let before: BTreeSet<_> = fs::read_dir(&dir).unwrap().map(|file| file.unwrap().file_name()).collect();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    drop(writer);
    let mut child = command(&fixture.first).arg("--atlas-ann-worker").arg(&database).arg(&dir)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let staged = fs::read_dir(&dir).unwrap().filter_map(Result::ok).any(|file| {
            file.path().extension().is_some_and(|ext| ext == "usearch") && !before.contains(&file.file_name())
        });
        if staged { break; }
        assert!(Instant::now() < deadline && child.try_wait().unwrap().is_none(), "builder failed before staging");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert_eq!(manifest(), generation);
    conn.execute_batch("ROLLBACK").unwrap();
    drop(scheduler);
    ready(&mut first);
    let after_interrupt = manifest();
    assert_ne!(after_interrupt["generation"], generation["generation"]);
    assert!(fs::read_dir(&dir).unwrap().filter_map(Result::ok).filter(|file| {
        file.path().extension().is_some_and(|ext| ext == "usearch")
    }).count() <= 3); // Older mapped generations can remain until readers release them.
    drop(conn);

    let edit_lease = lock(&dir, "build.lock");
    fs::write(&cpp, "void UShared::AfterEdit() {}\n").unwrap();
    first.wait(|result| external(result, "runtime")["revision"] != rev);
    let stale = hybrid(&mut second);
    assert!(meaning_paths(&stale).is_empty(), "{stale}");
    assert_eq!(external(&stale, "runtime")["semantic_search"]["state"], "ann_stale");
    drop(edit_lease);
    ready(&mut second);
    assert_eq!(meaning_paths(&hybrid(&mut first)), ["library.rs"]);
    fs::remove_file(&cpp).unwrap();
    first.wait(|result| external(result, "runtime")["counts"]["files"] == 1);
    let latest = ready(&mut first);
    assert!(meaning_paths(&hybrid(&mut second)).iter().all(|path| *path != "external.cpp"));

    // A new stdio session and worker retain the generation.
    drop(second);
    let stopped = command(&fixture.second).args(["atlas", "watch", "stop"]).output().unwrap();
    assert!(stopped.status.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stat = command(&fixture.second).args(["atlas", "watch", "status", "--json"]).output().unwrap();
        let status: Value = serde_json::from_slice(&stat.stdout).unwrap();
        if status["worker"]["state"] == "disabled" { break; }
        assert!(Instant::now() < deadline, "{status}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut second = client_for(&fixture.second);
    activate(&mut second);
    let resumed = ready(&mut second);
    assert_eq!(external(&resumed, "runtime")["ann"]["generation"], external(&latest, "runtime")["ann"]["generation"]);
    configure(&fixture.second, "nomic-embed-text-v1.5", "");
    assert_eq!(external(&second.status(), "runtime")["ann"]["state"], "no_vectors");
    configure(&fixture.second, MODEL, "exclude = [\"unrelated\"]\n");
    let other = activate(&mut second);
    assert_ne!(external(&other, "runtime")["path"], source["path"]);
    assert_eq!(external(&other, "runtime")["ann"]["state"], "no_vectors");
}
