//! The code map (docs/ATLAS.md) through the command layer, in a scratch
//! repository: off unless lore.toml turns it on, refreshed from git's blob ids,
//! searchable, with its file graph meeting the lore's records in `impact`,
//! `drift` and `lore for`.
#![cfg(feature = "atlas")]
use codelore::cli::{self, Env};
use codelore::Store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output().expect("git runs");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

const CONFIG: &str = "version = 1\ndir = \"lore\"\nareas = [\"code\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n";

struct Repo {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn new(atlas: bool) -> Self {
        let tmp = tempfile::Builder::new().prefix("lore-atlas-").tempdir().unwrap();
        let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap()).join("repo");
        std::fs::create_dir_all(root.join("src/render")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "T"]);
        git(&root, &["config", "user.email", "t@example.invalid"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        let config = if atlas { format!("{CONFIG}\n[atlas]\nenabled = true\n") } else { CONFIG.to_owned() };
        std::fs::write(root.join("lore.toml"), config).unwrap();
        let files = [
            ("src/lib.rs", "pub mod render;\npub mod store;\n\n/// Opens the depth buffer for a frame.\npub fn open_depth_buffer() -> u32 {\n    render::post::chain()\n}\n"),
            ("src/render/mod.rs", "pub mod post;\n"),
            ("src/render/post.rs", "use crate::store::Cache;\n\n/// Runs the post-processing chain: bloom, tone mapping, the vignette.\npub fn chain() -> u32 {\n    Cache::warm();\n    7\n}\n"),
            ("src/store.rs", "pub struct Cache;\n\nimpl Cache {\n    /// Fills the shader cache before the first frame.\n    pub fn warm() {}\n}\n"),
            ("tools/report.py", "from helpers import table\n\ndef summarize_frames(frames):\n    return table(frames)\n"),
            ("tools/helpers.py", "def table(rows):\n    return len(rows)\n"),
        ];
        for (path, text) in files {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), text).unwrap();
        }
        std::fs::create_dir_all(root.join("lore")).unwrap();
        let store = Store::new(&root);
        store
            .write("lore/code.jsonl", &[json!({"v":3,"id":"post-chain-reads-a-cold-cache","kind":"fault","area":"code","title":"post chain reads a cold cache",
                "symptom":"the first frame is black","status":"live","created":"2026-11-08","updated":"2026-11-08","by":"me",
                "refs":{"files":["src/render/post.rs"],"symbols":["chain","vanished_helper"]}})])
            .unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "init"]);
        Self { _tmp: tmp, root }
    }

    fn run(&self, argv: &[&str]) -> cli::Output {
        let root = self.root.to_string_lossy().into_owned();
        let mut full = vec!["-C", root.as_str()];
        full.extend(argv);
        let env = Env { cwd: self.root.clone(), finder: Some(&codelore::builtin::BUILTIN), ..Env::default() };
        cli::run(&full, &env)
    }

    fn json(&self, argv: &[&str]) -> Value {
        let mut full = vec!["--json"];
        full.extend(argv);
        let out = self.run(&full);
        assert_eq!(out.code, 0, "lore {argv:?}: {}{}", out.stdout, out.stderr);
        serde_json::from_str(&out.stdout).unwrap()
    }
}

#[test]
fn the_map_is_off_unless_the_project_turns_it_on() {
    let repo = Repo::new(false);
    let out = repo.run(&["atlas", "search", "chain"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("[atlas] enabled = true"), "{}", out.stderr);
    // and `lore for` says nothing about code
    let out = repo.run(&["for", "src/render/post.rs"]);
    assert!(!out.stdout.contains("code:"), "{}", out.stdout);
}

#[test]
fn search_finds_pieces_by_words_terms_and_symbols() {
    let repo = Repo::new(true);
    let index = repo.json(&["atlas", "index"]);
    assert_eq!(index["counts"]["files"], 6, "{index}");
    let found = repo.json(&["atlas", "search", "tone mapping vignette"]);
    assert_eq!(found["results"][0]["path"], "src/render/post.rs", "{found}");
    let found = repo.json(&["atlas", "search", "open_depth_buffer"]);
    assert_eq!(found["results"][0]["path"], "src/lib.rs");
    assert!(found["results"][0]["via"].as_array().unwrap().contains(&json!("symbol")), "{found}");
    // identifiers are found by their words too
    let found = repo.json(&["atlas", "search", "summarize frames"]);
    assert_eq!(found["results"][0]["path"], "tools/report.py");
}

const COLLISION_PATH: &str = "Engine/Source/Runtime/Shared_%/Probe.cpp";

fn write_at(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn build_at(root: &Path, patch: u32) {
    write_at(root, "Engine/Build/Build.version", &json!({
        "MajorVersion": 5, "MinorVersion": 8, "PatchVersion": patch,
        "Changelist": 58902052, "CompatibleChangelist": 55116800,
        "BranchName": "++UE5+Release-5.8", "IsLicenseeVersion": 0,
    }).to_string());
}

fn configure_engine(repo: &Repo, root: &Path) {
    let literal = json!(root.to_str().unwrap().replace('\\', "/"));
    write_at(&repo.root, "lore.toml", &format!("{CONFIG}\n[atlas]\nenabled = true\n[atlas.unreal]\nroot = {literal}\n"));
}

struct EngineFixture {
    tmp: tempfile::TempDir,
    root: PathBuf,
}

impl EngineFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Installed Engine");
        build_at(&root, 4);
        write_at(&root, COLLISION_PATH, "// engineunique\nvoid UShared::ProbeSymbol() {}\n");
        write_at(&root, "Engine/Source/Runtime/SharedFake/Another.cpp", "// engineunique\nvoid UOther::ProbeSymbol() {}\n");
        write_at(&root, "Engine/Source/ThirdParty/hidden.cpp", "void never_index_me() {}\n");
        write_at(&root, "Engine/Source/Runtime/Test.generated.h", "void generated_symbol() {}\n");
        write_at(&root, "Engine/Platforms/Test/Plugins/Group/TestPlugin/Test.uplugin", "{}");
        write_at(&root, "Engine/Platforms/Test/Plugins/Group/TestPlugin/Source/Platform.cpp", "void platform_test_symbol() {}\n");
        write_at(&root, "Engine/Plugins/Invalid/Source/hidden.cpp", "void no_plugin_descriptor() {}\n");
        Self { tmp, root }
    }

    fn invoke(&self, repo: &Repo, args: &[&str]) -> (i32, Value) {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_lore"))
            .arg("-C").arg(&repo.root).args(["--json", "atlas"]).args(args)
            .env("LORE_CACHE", self.tmp.path().join("cache"))
            .env("LORE_ATLAS", "on")
            .output().unwrap();
        let body = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!("{args:?}: {error}\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
        });
        (output.status.code().unwrap(), body)
    }

    fn json(&self, repo: &Repo, args: &[&str]) -> Value {
        let (code, body) = self.invoke(repo, args);
        assert_eq!(code, 0, "{args:?}: {body}");
        body
    }
}

#[test]
fn unreal_atlas_reuses_one_cache_across_projects_and_preserves_each_location() {
    let ue = EngineFixture::new();
    let a = Repo::new(true);
    let b = Repo::new(true);
    let legacy = ue.json(&a, &["search", "open_depth_buffer", "--text-only"]);
    assert_eq!(legacy["results"][0]["path"], "src/lib.rs");
    assert!(legacy.get("sources").is_none());
    write_at(&a.root, COLLISION_PATH, "void UShared::ProbeSymbol() {}\n");
    configure_engine(&a, &ue.root);
    configure_engine(&b, &ue.root.join("."));
    let pending = ue.json(&a, &["status", "--source", "unreal"]);
    assert_eq!(pending["sources"][0]["state"], "unindexed");
    assert!(!Path::new(pending["sources"][0]["path"].as_str().unwrap()).exists());
    assert!(!ue.root.join(".git").exists());

    let index = ue.json(&a, &["index", "--source", "unreal", "--text-only"]);
    let first = &index["sources"][0];
    assert_eq!(first["counts"]["files"], 3, "{index}");
    assert_eq!(first["files_read"], 3);
    assert_eq!(first["files_hashed"], 3);
    let index_path = first["path"].as_str().unwrap();
    assert!(!Path::new(index_path).starts_with(&a.root));
    let other = ue.json(&b, &["search", "engineunique", "--text-only"]);
    assert_eq!(other["results"][0]["source"], "unreal", "{other}");
    assert_eq!(other["sources"][1]["files_seen"], 0);
    assert_eq!(other["sources"][1]["path"], index_path);
    assert_eq!(other["sources"][1]["revision"], first["revision"]);
    let warm = ue.json(&b, &["index", "--source", "unreal", "--text-only"]);
    for field in ["cache_key", "path", "revision", "source_id"] {
        assert_eq!(warm["sources"][0][field], first[field], "{warm}");
    }
    assert_eq!(warm["sources"][0]["cache_hit"], true);
    for field in ["files_hashed", "files_read", "vectors_generated"] {
        assert_eq!(warm["sources"][0][field], 0, "{warm}");
    }
    let project_rebuild = ue.json(&b, &["index", "--rebuild", "--text-only"]);
    assert_eq!(project_rebuild["sources"].as_array().unwrap().len(), 1, "{project_rebuild}");
    assert_eq!(project_rebuild["sources"][0]["source"], "project");
    let engine_rebuild = ue.json(&b, &["index", "--rebuild", "--source", "unreal", "--text-only"]);
    assert_eq!(engine_rebuild["sources"][0]["files_hashed"], 3, "{engine_rebuild}");
    assert_ne!(engine_rebuild["sources"][0]["revision"], first["revision"]);

    let results = ue.json(&a, &["search", "UShared :: ProbeSymbol", "--text-only", "-n", "10"]);
    let exact: Vec<_> = results["results"].as_array().unwrap().iter().filter(|hit| hit["path"] == COLLISION_PATH).collect();
    assert_eq!(exact.len(), 2, "{results}");
    assert_ne!(exact[0]["source_id"], exact[1]["source_id"]);
    assert_ne!(exact[0]["absolute_path"], exact[1]["absolute_path"]);
    assert_eq!(results["results"][0]["path"], COLLISION_PATH);
    assert_eq!(results["results"][1]["path"], COLLISION_PATH);
    let paths = ue.json(&a, &["search", "engineunique", "--source", "unreal", "--path", "Engine/Source/Runtime/Shared_%/", "--text-only"]);
    assert!(paths["results"].as_array().unwrap().iter().all(|hit| hit["path"] == COLLISION_PATH), "{paths}");
    assert_eq!(paths["results"][0]["path"], COLLISION_PATH);

    let definitions = ue.json(&a, &["symbol", "UShared :: ProbeSymbol", "--lang", "cpp"]);
    assert_eq!(definitions["definitions"].as_array().unwrap().len(), 2, "{definitions}");
    assert_eq!(definitions["definitions"][0]["line"], 1);
    assert_eq!(definitions["definitions"][1]["line"], 2);
    let wrong_case = ue.json(&a, &["symbol", "ushared::ProbeSymbol"]);
    assert_eq!(wrong_case["definitions"], json!([]));
    let absolute = definitions["definitions"][1]["absolute_path"].as_str().unwrap();
    let file = ue.json(&a, &["symbols", absolute]);
    assert_eq!(file["definitions"].as_array().unwrap().len(), 1, "{file}");
    assert_eq!(file["definitions"][0]["source"], "unreal");
    let file = ue.json(&a, &["symbols", COLLISION_PATH]);
    assert_eq!(file["definitions"].as_array().unwrap().len(), 2, "{file}");
    let text = std::process::Command::new(env!("CARGO_BIN_EXE_lore"))
        .arg("-C").arg(&a.root).args(["atlas", "symbol", "UShared::ProbeSymbol"])
        .env("LORE_CACHE", ue.tmp.path().join("cache")).output().unwrap();
    let stdout = String::from_utf8(text.stdout).unwrap();
    assert!(stdout.contains("[project]") && stdout.contains("[UE 5.8.4 CL 58902052]") && stdout.contains("Probe.cpp:2"), "{stdout}");
}

#[test]
fn unreal_atlas_switches_builds_and_falls_back_after_source_loss() {
    let ue = EngineFixture::new();
    let repo = Repo::new(true);
    configure_engine(&repo, &ue.root);
    let first = ue.json(&repo, &["index", "--source", "unreal", "--text-only"]);
    build_at(&ue.root, 5);
    write_at(&ue.root, COLLISION_PATH, "// new build\n\nvoid UShared::ProbeSymbol() {}\n");
    let unavailable = ue.json(&repo, &["search", "open_depth_buffer", "--text-only"]);
    assert_eq!(unavailable["results"][0]["source"], "project");
    assert!(unavailable["sources"].as_array().unwrap().iter().any(|state| state["state"] == "unindexed"));
    assert_eq!(ue.invoke(&repo, &["symbol", "UShared::ProbeSymbol", "--source", "unreal"]).0, 1);
    let replacement = ue.json(&repo, &["index", "--source", "unreal", "--text-only"]);
    assert_ne!(first["sources"][0]["cache_key"], replacement["sources"][0]["cache_key"]);
    assert_eq!(first["sources"][0]["source_id"], replacement["sources"][0]["source_id"]);
    let new_definition = ue.json(&repo, &["symbol", "UShared::ProbeSymbol", "--source", "unreal"]);
    assert_eq!(new_definition["definitions"][0]["build"]["PatchVersion"], 5);
    assert_eq!(new_definition["definitions"][0]["line"], 3);

    write_at(&ue.root, COLLISION_PATH, "// changed in the installed sources\n\n\nvoid UShared::ProbeSymbol() {}\n");
    write_at(&ue.root, "Engine/Source/Runtime/New.cpp", "void added_test_symbol() {}\n");
    let edited = ue.json(&repo, &["index", "--source", "unreal", "--text-only"]);
    assert_eq!(edited["sources"][0]["cache_key"], replacement["sources"][0]["cache_key"]);
    assert_eq!(edited["sources"][0]["files_hashed"], 2, "{edited}");
    let moved = ue.json(&repo, &["symbol", "UShared::ProbeSymbol", "--source", "unreal"]);
    assert_eq!(moved["definitions"][0]["line"], 4);

    std::fs::remove_file(ue.root.join(COLLISION_PATH)).unwrap();
    let gone = ue.json(&repo, &["symbol", "UShared::ProbeSymbol", "--source", "unreal"]);
    assert_eq!(gone["definitions"], json!([]));
    assert!(!gone["notes"].as_array().unwrap().is_empty());
    let refreshed = ue.json(&repo, &["index", "--source", "unreal", "--text-only"]);
    assert_eq!(refreshed["sources"][0]["files_dropped"], 1);
    assert_ne!(edited["sources"][0]["revision"], refreshed["sources"][0]["revision"]);

    let other_root = ue.tmp.path().join("Other Engine");
    build_at(&other_root, 5);
    write_at(&other_root, COLLISION_PATH, "void UShared::ProbeSymbol() {}\n");
    configure_engine(&repo, &other_root);
    let new_root = ue.json(&repo, &["index", "--source", "unreal", "--text-only"]);
    assert_ne!(new_root["sources"][0]["source_id"], replacement["sources"][0]["source_id"]);
    assert_ne!(new_root["sources"][0]["cache_key"], replacement["sources"][0]["cache_key"]);

    configure_engine(&repo, &ue.tmp.path().join("missing"));
    let missing = ue.json(&repo, &["status"]);
    assert_eq!(missing["sources"][1]["state"], "unavailable");
    let still_searchable = ue.json(&repo, &["search", "open_depth_buffer", "--text-only"]);
    assert_eq!(still_searchable["results"][0]["source"], "project");
    let (code, partial) = ue.invoke(&repo, &["index", "--text-only"]);
    assert_eq!(code, 1);
    assert_eq!(partial["sources"][0]["state"], "ready");
    assert_eq!(partial["sources"][1]["state"], "unavailable");
}

#[test]
fn unreal_atlas_upgrades_ready_schema_two_and_publishes_counts_with_edits() {
    let ue = EngineFixture::new();
    let repo = Repo::new(true);
    configure_engine(&repo, &ue.root);
    let model = codelore::config::load(&repo.root).unwrap().embedding_model();
    let first = ue.json(&repo, &["index", "--source", "unreal", "--text-only"]);
    let indexed = &first["sources"][0];
    let path = indexed["path"].as_str().unwrap();
    let space = indexed["vector_space"].as_str().unwrap();
    let counts = &indexed["counts"];

    let db = rusqlite::Connection::open(path).unwrap();
    let triggers: Vec<String> = db.prepare("SELECT name FROM sqlite_schema WHERE type = 'trigger' AND name LIKE 'atlas_%'")
        .unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    for trigger in triggers {
        db.execute_batch(&format!("DROP TRIGGER {trigger};")).unwrap();
    }
    db.execute_batch(
        "DROP TABLE atlas_short_names; DROP TABLE atlas_counts;
         DROP TABLE atlas_vector_sizes; DROP TABLE atlas_vector_counts;
         DELETE FROM meta WHERE key = 'atlas_derived';",
    ).unwrap();
    let chunk: String = db.query_row("SELECT ehash FROM chunks WHERE path = ?1 ORDER BY id LIMIT 1",
        [COLLISION_PATH], |row| row.get(0)).unwrap();
    let vector = vec![0_u8; model.dimensions() * 4];
    db.execute("INSERT INTO vectors VALUES(?1, ?2, ?3)", rusqlite::params![space, chunk, vector]).unwrap();

    // An existing writer keeps the legacy read path available. The next read
    // upgrades using only database rows, even with the sources moved away.
    let old_writer = std::fs::OpenOptions::new().write(true).open(Path::new(path).with_file_name("writer.lock")).unwrap();
    fs2::FileExt::lock_exclusive(&old_writer).unwrap();
    let legacy = ue.json(&repo, &["symbol", "ProbeSymbol", "--source", "unreal", "--lang", "cpp"]);
    assert_eq!(legacy["definitions"].as_array().unwrap().len(), 2);
    drop(old_writer);
    let source = ue.root.join("Engine/Source");
    let moved = ue.root.join("SavedSource");
    std::fs::rename(&source, &moved).unwrap();
    std::fs::create_dir(&source).unwrap();
    let migrated = ue.json(&repo, &["status", "--source", "unreal"]);
    let migrated = &migrated["sources"][0];
    assert_eq!(migrated["counts"], *counts);
    assert_eq!(migrated["vectors"], 1);
    assert_eq!(migrated["files_seen"], 0);
    assert_eq!(migrated["revision"], indexed["revision"]);
    assert_eq!(migrated["refreshed_at"], indexed["refreshed_at"]);
    assert_eq!(db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
    assert_eq!(db.query_row("SELECT value FROM meta WHERE key = 'atlas_derived'", [], |row| row.get::<_, String>(0)).unwrap(), "1");
    assert_eq!(db.query_row("SELECT vec FROM vectors WHERE model = ?1 AND hash = ?2", rusqlite::params![space, chunk],
        |row| row.get::<_, Vec<u8>>(0)).unwrap(), vector);
    std::fs::remove_dir(&source).unwrap();
    std::fs::rename(moved, source).unwrap();
    assert_eq!(ue.json(&repo, &["symbol", "ProbeSymbol", "--source", "unreal", "--lang", "cpp"]), legacy);

    // Vectors are committed with their coverage and resized on replacement.
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    db.execute("INSERT OR REPLACE INTO vectors VALUES(?1,?2,?3)", rusqlite::params![space, chunk, vec![0_u8; 4]]).unwrap();
    assert_eq!(ue.json(&repo, &["status", "--source", "unreal"])["sources"][0]["vectors"], 1);
    db.execute_batch("COMMIT").unwrap();
    assert_eq!(ue.json(&repo, &["status", "--source", "unreal"])["sources"][0]["vectors"], 0);
    db.execute("INSERT OR REPLACE INTO vectors VALUES(?1,?2,?3)", rusqlite::params![space, chunk, vector]).unwrap();

    db.execute_batch("BEGIN").unwrap();
    let read_count = |name| db.query_row("SELECT total FROM atlas_counts WHERE name = ?1", [name], |row| row.get::<_, i64>(0)).unwrap();
    let old_chunks = read_count("chunks");
    assert_eq!(old_chunks, counts["chunks"].as_i64().unwrap());
    write_at(&ue.root, COLLISION_PATH, "void UShared::RevisedSymbol() {}\n");
    std::fs::remove_file(ue.root.join("Engine/Source/Runtime/SharedFake/Another.cpp")).unwrap();
    let refreshed = ue.json(&repo, &["index", "--source", "unreal", "--text-only"]);
    assert_eq!(read_count("chunks"), old_chunks);
    db.execute_batch("COMMIT").unwrap();
    let published = &refreshed["sources"][0];
    assert_eq!(published["files_read"], 1);
    assert_eq!(published["files_dropped"], 1);
    assert_eq!(published["vectors_generated"], 0);
    assert_eq!(published["vectors"], 0);
    for (name, table, filter) in [
        ("files", "files", " WHERE lang <> 'binary'"),
        ("chunks", "chunks", ""),
        ("symbols", "symbols", ""),
        ("edges", "edges", ""),
        ("imports", "imports", ""),
    ] {
        let actual: i64 = db.query_row(&format!("SELECT count(*) FROM {table}{filter}"), [], |row| row.get(0)).unwrap();
        assert_eq!(published["counts"][name], actual, "{name}");
        assert_eq!(read_count(name), actual, "{name}");
    }
    assert_eq!(ue.json(&repo, &["symbol", "ProbeSymbol", "--source", "unreal"])["definitions"], json!([]));
    let new = ue.json(&repo, &["symbol", "RevisedSymbol", "--source", "unreal"]);
    assert_eq!(new["definitions"][0]["path"], COLLISION_PATH);
    assert_eq!(new["sources"][0]["counts"], published["counts"]);
}

#[test]
fn the_graph_resolves_imports_and_meets_the_lore() {
    let repo = Repo::new(true);
    let deps = repo.json(&["atlas", "deps", "src/render/post.rs"]);
    assert_eq!(deps["imports"], json!(["src/store.rs"]));
    assert_eq!(deps["imported_by"], json!(["src/render/mod.rs"]));
    let deps = repo.json(&["atlas", "deps", "tools/report.py"]);
    assert_eq!(deps["imports"], json!(["tools/helpers.py"]));

    // a change to the cache reaches post.rs, whose fault the lore holds
    let impact = repo.json(&["atlas", "impact", "src/store.rs"]);
    let reached: Vec<&str> = impact["dependents"].as_array().unwrap().iter().map(|d| d["path"].as_str().unwrap()).collect();
    assert!(reached.contains(&"src/render/post.rs"), "{impact}");
    let ids: Vec<&str> = impact["records_of_dependents"].as_array().unwrap().iter().map(|r| r["record"]["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["post-chain-reads-a-cold-cache"]);

    let symbol = repo.json(&["atlas", "symbol", "warm"]);
    assert_eq!(symbol["definitions"][0]["path"], "src/store.rs");
    assert_eq!(symbol["mentioned_in"], json!(["src/render/post.rs"]));

    // `lore for` names the importers in text; its JSON keeps its shape
    let text = repo.run(&["for", "src/store.rs"]).stdout;
    assert!(text.contains("is imported by") && text.contains("post-chain-reads-a-cold-cache"), "{text}");
    let plain = repo.json(&["for", "src/store.rs"]);
    assert!(plain.is_array(), "{plain}");
}

#[test]
fn a_change_in_the_working_tree_is_read_again_and_drift_is_named() {
    let repo = Repo::new(true);
    repo.json(&["atlas", "index"]);
    let drift = repo.json(&["atlas", "drift"]);
    assert_eq!(drift[0]["missing_symbols"], json!(["vanished_helper"]), "{drift}");

    // rename the function the record cites, uncommitted
    std::fs::write(repo.root.join("src/render/post.rs"), "use crate::store::Cache;\n\npub fn run_chain() -> u32 {\n    Cache::warm();\n    7\n}\n").unwrap();
    let lib = std::fs::read_to_string(repo.root.join("src/lib.rs")).unwrap().replace("post::chain()", "post::run_chain()");
    std::fs::write(repo.root.join("src/lib.rs"), lib).unwrap();
    let status = repo.json(&["atlas", "status"]);
    assert_eq!(status["refreshed"]["files_read"], 2, "{status}");
    let drift = repo.json(&["atlas", "drift"]);
    assert_eq!(drift[0]["missing_symbols"], json!(["chain", "vanished_helper"]), "{drift}");
    let found = repo.json(&["atlas", "search", "run_chain"]);
    assert_eq!(found["results"][0]["path"], "src/render/post.rs");

    // a new import cycle is seen
    std::fs::write(repo.root.join("src/store.rs"), "use crate::render::post::run_chain;\npub struct Cache;\nimpl Cache { pub fn warm() {} }\n").unwrap();
    let cycles = repo.json(&["atlas", "cycles"]);
    assert_eq!(cycles, json!([["src/render/post.rs", "src/store.rs"]]));
}

/// A search skips the scan while the quick print (index file, `git diff-files`,
/// folder times) is unchanged; every kind of change must still be seen.
#[test]
fn the_quick_freshness_check_misses_no_change() {
    let repo = Repo::new(true);
    repo.json(&["atlas", "index"]);
    let first = |query: &str| repo.json(&["atlas", "search", query])["results"][0]["path"].as_str().unwrap_or("").to_owned();
    let search_twice = |query: &str| {
        let a = first(query);
        assert_eq!(a, first(query), "a second search of {query} answers the same");
        a
    };
    assert_eq!(search_twice("open_depth_buffer"), "src/lib.rs");

    // a new file beside tracked ones
    std::fs::write(repo.root.join("src/render/glow.rs"), "pub fn glow_radius() -> u32 { 3 }\n").unwrap();
    assert_eq!(search_twice("glow_radius"), "src/render/glow.rs");
    // a new file in a new folder
    std::fs::create_dir_all(repo.root.join("src/audio/mix")).unwrap();
    std::fs::write(repo.root.join("src/audio/mix/bus.rs"), "pub fn duck_the_music() {}\n").unwrap();
    assert_eq!(search_twice("duck_the_music"), "src/audio/mix/bus.rs");
    // a second edit to a file already modified (the diff-files list stays the same)
    std::fs::write(repo.root.join("src/store.rs"), "pub struct Cache;\nimpl Cache { pub fn warm_slowly() {} }\n").unwrap();
    assert_eq!(search_twice("warm_slowly"), "src/store.rs");
    std::fs::write(repo.root.join("src/store.rs"), "pub struct Cache;\nimpl Cache { pub fn warm_quickly_now() {} }\n").unwrap();
    assert_eq!(search_twice("warm_quickly_now"), "src/store.rs");
    // a new file in an untracked folder seen before
    std::fs::write(repo.root.join("src/audio/mix/fade.rs"), "pub fn fade_out_slowly() {}\n").unwrap();
    assert_eq!(search_twice("fade_out_slowly"), "src/audio/mix/fade.rs");
    // staged and committed: the index file changes
    git(&repo.root, &["add", "-A"]);
    git(&repo.root, &["commit", "-qm", "two"]);
    assert_eq!(search_twice("glow_radius"), "src/render/glow.rs");
    // removed
    std::fs::remove_file(repo.root.join("src/render/glow.rs")).unwrap();
    assert_ne!(first("glow_radius"), "src/render/glow.rs");
}

#[test]
fn an_unknown_table_is_a_warning_not_a_refusal() {
    let repo = Repo::new(true);
    let config = std::fs::read_to_string(repo.root.join("lore.toml")).unwrap();
    std::fs::write(repo.root.join("lore.toml"), format!("{config}\n[from_the_future]\nx = 1\n")).unwrap();
    let out = repo.run(&["--json", "find", "cold cache"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("unknown key `from_the_future` is ignored"), "{}", out.stderr);
    serde_json::from_str::<Value>(&out.stdout).expect("stdout stays one JSON document");
}

#[test]
fn lore_for_neither_builds_the_map_nor_waits_for_it() {
    let repo = Repo::new(true);
    // A fresh clone: the map is not built, and `for` does not build it.
    let started = std::time::Instant::now();
    let text = repo.run(&["for", "src/store.rs"]).stdout;
    assert!(text.contains("the code map is not built yet"), "{text}");
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    let plain = repo.json(&["for", "src/store.rs"]);
    assert!(plain.is_array(), "the JSON keeps its shape: {plain}");
    // Once built, it is used.
    repo.json(&["atlas", "deps", "src/store.rs"]);
    let text = repo.run(&["for", "src/store.rs"]).stdout;
    assert!(text.contains("is imported by"), "{text}");
}

struct Counting(std::sync::atomic::AtomicUsize);

impl codelore::embed::Embeddings for Counting {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, codelore::LoreError> {
        self.0.fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
        Ok(texts.iter().map(|text| (0..768).map(|at| ((text.len() + at) % 7) as f32).collect()).collect())
    }
}

#[test]
fn a_second_clone_takes_the_vectors_the_first_made() {
    let first = Repo::new(true);
    let second = Repo::new(true);
    // Content of this run only, so an earlier run's vectors are not found.
    let unique = format!("pub fn only_in_this_run() -> &'static str {{ \"{}\" }}\n", first.root.display().to_string().replace('\\', "/"));
    for repo in [&first, &second] {
        std::fs::write(repo.root.join("src/unique.rs"), &unique).unwrap();
    }
    let embedded = |repo: &Repo| {
        let config = codelore::config::load(&repo.root).unwrap();
        let mut atlas = codelore::atlas::store::Atlas::open_at(&repo.root.join(".map.sqlite"), &repo.root).unwrap();
        atlas.refresh(&config).unwrap();
        let counting = Counting(std::sync::atomic::AtomicUsize::new(0));
        let (_, left) = atlas.ensure_vectors(&counting, "test-model", "", None, &mut |_, _| {}).unwrap();
        assert_eq!(left, 0);
        (counting.0.load(std::sync::atomic::Ordering::SeqCst), atlas.vector_count("test-model"))
    };
    let (made, held) = embedded(&first);
    assert!(made > 0 && held > 0);
    let (made_again, held_again) = embedded(&second);
    assert_eq!(made_again, 0, "the second clone embeds nothing");
    assert_eq!(held_again, held);
}

fn finds(repo: &Repo, word: &str, path: &str) -> bool {
    let found = repo.json(&["atlas", "search", word, "--text-only"]);
    found["results"].as_array().unwrap().iter().any(|hit| hit["path"] == path)
}

#[test]
fn an_untracked_file_edited_in_place_is_read_again() {
    let repo = Repo::new(true);
    std::fs::write(repo.root.join("src/new.rs"), "fn zebrafoo() { platypusbaz(); }\n").unwrap();
    // Two searches: the first one's status call may rewrite git's index, so the
    // print settles only on the second.
    assert!(finds(&repo, "platypusbaz", "src/new.rs"));
    assert!(finds(&repo, "platypusbaz", "src/new.rs"));
    // The same size, in place: neither the index nor the folder changes.
    std::fs::write(repo.root.join("src/new.rs"), "fn zebrafoo() { wombatqux12(); }\n").unwrap();
    assert!(finds(&repo, "wombatqux12", "src/new.rs"), "the edit is seen");
    assert!(!finds(&repo, "platypusbaz", "src/new.rs"), "the old text is gone");
    let mut text = std::fs::read_to_string(repo.root.join("src/new.rs")).unwrap();
    text.push_str("// echidnaxyz\n");
    std::fs::write(repo.root.join("src/new.rs"), text).unwrap();
    assert!(finds(&repo, "echidnaxyz", "src/new.rs"), "an append is seen");
}

#[test]
fn a_map_made_before_rowids_were_piece_ids_is_renumbered_in_place() {
    let repo = Repo::new(true);
    assert!(finds(&repo, "vignette", "src/render/post.rs"));
    // Make the map look like an older one: text rows numbered apart from their pieces.
    let config = codelore::config::load(&repo.root).unwrap();
    let path = codelore::index::index_dirs(&config.root).into_iter().map(|dir| dir.join("atlas.sqlite")).find(|path| path.exists()).unwrap();
    {
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TEMP TABLE w AS SELECT chunk, path, names, body FROM code_words;
             CREATE TEMP TABLE t AS SELECT chunk, toks FROM code_terms;
             DELETE FROM code_words; DELETE FROM code_terms;
             INSERT INTO code_words(rowid, chunk, path, names, body) SELECT chunk + 1000, chunk, path, names, body FROM w;
             INSERT INTO code_terms(rowid, chunk, toks) SELECT chunk + 1000, chunk, toks FROM t;
             DELETE FROM meta WHERE key = 'fts_rowid';",
        )
        .unwrap();
    }
    assert!(finds(&repo, "vignette", "src/render/post.rs"), "renumbered, still found");
    let db = rusqlite::Connection::open(&path).unwrap();
    let apart: i64 = db.query_row("SELECT count(*) FROM code_words WHERE rowid != chunk", [], |row| row.get(0)).unwrap();
    assert_eq!(apart, 0);
    drop(db);
    // A change to the file drops its old rows by rowid: the old words are gone.
    std::fs::write(repo.root.join("src/render/post.rs"), "pub fn chain() -> u32 { 9 }\n").unwrap();
    assert!(!finds(&repo, "vignette", "src/render/post.rs"));
}

/// Two C++ methods with the same short name in different classes and
/// namespaces, and callers that name each one whole.
fn cpp_symbols_repo(with_callers: bool) -> Repo {
    let repo = Repo::new(true);
    let mut files = vec![
        ("src/alpha.cpp", "void Alpha::Sword::Equip()\n{\n}\n"),
        ("src/beta.cpp", "void Beta::Axe::Equip()\n{\n}\n"),
    ];
    if with_callers {
        files.push(("src/call_alpha.cpp", "void call_alpha()\n{\n    Alpha::Sword::Equip();\n}\n"));
        files.push(("src/call_spaced.cpp", "void call_spaced()\n{\n    Alpha :: Sword :: Equip();\n}\n"));
        files.push(("src/call_beta.cpp", "// Alpha Sword Equip\nvoid call_beta()\n{\n    Beta::Axe::Equip();\n}\n"));
    }
    for (path, text) in files {
        std::fs::write(repo.root.join(path), text).unwrap();
    }
    git(&repo.root, &["add", "-A"]);
    git(&repo.root, &["commit", "-qm", "cpp symbols"]);
    repo
}

#[test]
fn qualified_cpp_symbols_keep_their_class_and_namespace() {
    let repo = cpp_symbols_repo(true);
    repo.json(&["atlas", "index"]);

    let full = repo.json(&["atlas", "symbol", "Alpha::Sword::Equip"]);
    assert_eq!(full["definitions"].as_array().unwrap().len(), 1, "{full}");
    assert_eq!(full["definitions"][0]["path"], "src/alpha.cpp", "{full}");
    assert_eq!(full["definitions"][0]["name"], "Alpha::Sword::Equip", "{full}");
    let mentioned: Vec<&str> = full["mentioned_in"].as_array().unwrap().iter().map(|value| value.as_str().unwrap()).collect();
    assert!(mentioned.contains(&"src/call_alpha.cpp"), "{full}");
    assert!(mentioned.contains(&"src/call_spaced.cpp"), "{full}");
    assert!(!mentioned.contains(&"src/call_beta.cpp"), "{full}");

    // The short name is not narrowed to one class: every Equip comes back.
    let short = repo.json(&["atlas", "symbol", "Equip"]);
    let names: Vec<&str> = short["definitions"].as_array().unwrap().iter().map(|def| def["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Alpha::Sword::Equip") && names.contains(&"Beta::Axe::Equip"), "{short}");

    // The full name ranks its own definition above the other class's Equip.
    let found = repo.json(&["atlas", "search", "Alpha::Sword::Equip", "--text-only"]);
    assert_eq!(found["results"][0]["path"], "src/alpha.cpp", "{found}");
    assert!(found["results"][0]["symbols"].as_array().unwrap().contains(&json!("Alpha::Sword::Equip")), "{found}");

    // impact and symbol agree on which file the qualified name stands for.
    let impact = repo.json(&["atlas", "impact", "Alpha::Sword::Equip"]);
    assert_eq!(impact["as"], "symbol", "{impact}");
    assert_eq!(impact["files"], json!(["src/alpha.cpp"]), "{impact}");
    let impact_short = repo.json(&["atlas", "impact", "Equip"]);
    let files: Vec<&str> = impact_short["files"].as_array().unwrap().iter().map(|value| value.as_str().unwrap()).collect();
    assert!(files.contains(&"src/alpha.cpp") && files.contains(&"src/beta.cpp"), "{impact_short}");
}

#[test]
fn drift_sees_a_deleted_qualified_symbol_beside_another_equip() {
    let repo = cpp_symbols_repo(false);
    let store = Store::new(&repo.root);
    store
        .write(
            "lore/qualified.jsonl",
            &[json!({"v":3,"id":"qualified-equip-ref","kind":"fault","area":"code","title":"qualified equip ref",
                "symptom":"the qualified method vanished","status":"live","created":"2026-11-18","updated":"2026-11-18","by":"me",
                "refs":{"files":[],"symbols":["Alpha::Sword::Equip"]}})],
        )
        .unwrap();
    git(&repo.root, &["add", "-A"]);
    git(&repo.root, &["commit", "-qm", "qualified symbol record"]);
    repo.json(&["atlas", "index"]);

    let drift = repo.json(&["atlas", "drift"]);
    assert!(drift.as_array().unwrap().iter().all(|row| row["id"] != "qualified-equip-ref"), "{drift}");

    // Delete Alpha's Equip; Beta::Axe::Equip still exists and supplies a short Equip.
    std::fs::write(repo.root.join("src/alpha.cpp"), "void Alpha::Sword::Unequip()\n{\n}\n").unwrap();
    let drift = repo.json(&["atlas", "drift"]);
    let row = drift.as_array().unwrap().iter().find(|row| row["id"] == "qualified-equip-ref").unwrap_or_else(|| panic!("{drift}"));
    assert_eq!(row["missing_symbols"], json!(["Alpha::Sword::Equip"]), "{drift}");

    let short = repo.json(&["atlas", "symbol", "Equip"]);
    let names: Vec<&str> = short["definitions"].as_array().unwrap().iter().map(|def| def["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Beta::Axe::Equip"], "{short}");
}

#[test]
fn a_missing_qualified_name_does_not_boost_another_class() {
    let repo = Repo::new(true);
    std::fs::write(repo.root.join("src/beta.cpp"), "void Beta::Axe::Equip()\n{\n}\n").unwrap();
    git(&repo.root, &["add", "-A"]);
    git(&repo.root, &["commit", "-qm", "only beta equips"]);
    repo.json(&["atlas", "index"]);

    // The ordinary text hit is still there, but it is not a symbol hit.
    let found = repo.json(&["atlas", "search", "Alpha::Sword::Equip", "--lang", "cpp", "--text-only"]);
    let hits = found["results"].as_array().unwrap();
    assert!(hits.iter().any(|hit| hit["path"] == "src/beta.cpp"), "{found}");
    for hit in hits {
        assert!(!hit["via"].as_array().unwrap().contains(&json!("symbol")), "{found}");
    }

    let symbol = repo.json(&["atlas", "symbol", "Alpha::Sword::Equip"]);
    assert!(symbol["definitions"].as_array().unwrap().is_empty(), "{symbol}");
    let out = repo.run(&["atlas", "impact", "Alpha::Sword::Equip"]);
    assert_eq!(out.code, 1, "impact must reject the missing C++ symbol: {}", out.stderr);
}

#[test]
fn nested_and_unrelated_neighbours_do_not_alias_a_qualified_name() {
    let repo = Repo::new(true);
    std::fs::write(repo.root.join("src/nested.cpp"), "void Other::Alpha::Sword::Equip()\n{\n}\n").unwrap();
    std::fs::write(repo.root.join("src/spaced.cpp"), "// Other :: Alpha :: Sword :: Equip()\n// Alpha :: Sword :: Equip :: nested()\n").unwrap();
    std::fs::write(repo.root.join("src/totally_unrelated.rs"), "pub fn equip() {}\npub fn Equip() {}\n").unwrap();
    git(&repo.root, &["add", "-A"]);
    git(&repo.root, &["commit", "-qm", "nested and unrelated neighbours"]);
    repo.json(&["atlas", "index"]);

    // The Rust Equip is another symbol; Other::Alpha::Sword::Equip is another chain.
    let symbol = repo.json(&["atlas", "symbol", "Alpha::Sword::Equip"]);
    assert!(symbol["definitions"].as_array().unwrap().is_empty(), "{symbol}");
    assert!(symbol["mentioned_in"].as_array().unwrap().is_empty(), "{symbol}");
    let out = repo.run(&["atlas", "impact", "Alpha::Sword::Equip"]);
    assert_eq!(out.code, 1, "impact must reject the missing C++ symbol: {}", out.stderr);

    // The nested chain is still a symbol of its own.
    let nested = repo.json(&["atlas", "symbol", "Other::Alpha::Sword::Equip"]);
    assert_eq!(nested["definitions"][0]["path"], "src/nested.cpp", "{nested}");

    // A text hit is kept, but it carries no symbol signal.
    let found = repo.json(&["atlas", "search", "Alpha::Sword::Equip", "--lang", "cpp", "--text-only"]);
    let hits = found["results"].as_array().unwrap();
    assert!(hits.iter().any(|hit| hit["path"] == "src/nested.cpp"), "{found}");
    for hit in hits {
        assert!(!hit["via"].as_array().unwrap().contains(&json!("symbol")), "{found}");
    }
}

#[test]
fn a_rust_module_path_still_finds_its_short_definition() {
    let repo = Repo::new(true);
    let scene_path = repo.root.join("src/scene.rs");
    let scene = "pub fn chain() -> u32 {\n    3\n}\n";
    std::fs::write(&scene_path, scene).unwrap();
    let store = Store::new(&repo.root);
    store
        .write(
            "lore/rust-path.jsonl",
            &[json!({"v":3,"id":"rust-path-ref","kind":"fault","area":"code","title":"rust module path ref",
                "symptom":"a module path is not a definition","status":"live","created":"2026-11-18","updated":"2026-11-18","by":"me",
                "refs":{"files":[],"symbols":["scene::chain"]}})],
        )
        .unwrap();
    git(&repo.root, &["add", "-A"]);
    git(&repo.root, &["commit", "-qm", "rust module path record"]);
    repo.json(&["atlas", "index"]);

    // `scene::chain` is confirmed by src/scene.rs.
    let symbol = repo.json(&["atlas", "symbol", "scene::chain"]);
    assert_eq!(symbol["definitions"].as_array().unwrap().len(), 1, "{symbol}");
    assert_eq!(symbol["definitions"][0]["path"], "src/scene.rs", "{symbol}");
    // `render::chain` must not take src/render/post.rs's chain: that file is module render/post.
    let wrong = repo.json(&["atlas", "symbol", "render::chain"]);
    assert!(wrong["definitions"].as_array().unwrap().is_empty(), "{wrong}");
    let drift = repo.json(&["atlas", "drift"]);
    assert!(drift.as_array().unwrap().iter().all(|row| row["id"] != "rust-path-ref"), "{drift}");

    // Remove the Rust definition: the live path becomes drift.
    std::fs::remove_file(&scene_path).unwrap();
    let drift = repo.json(&["atlas", "drift"]);
    let row = drift.as_array().unwrap().iter().find(|row| row["id"] == "rust-path-ref").unwrap_or_else(|| panic!("{drift}"));
    assert_eq!(row["missing_symbols"], json!(["scene::chain"]), "{drift}");
}

#[test]
fn rust_symbol_search_preserves_qualified_names_across_sources() {
    let ue = EngineFixture::new();
    let repo = Repo::new(true);
    configure_engine(&repo, &ue.root);
    write_at(&repo.root, "src/render/mod.rs", "pub mod post;\npub fn chain() -> u32 { 1 }\n");
    write_at(&repo.root, "src/other.rs", "pub fn chain() -> u32 { 2 }\npub fn Equip() {}\n");
    write_at(&repo.root, "src/alpha.cpp", "void Alpha::Sword::Equip() {}\n");
    let exact = "Engine/Source/Runtime/Exact.cpp";
    write_at(&ue.root, exact, "void Alpha::Sword::Equip() {}\n");
    write_at(&ue.root, "Engine/Source/Runtime/Nested.cpp", "void Other::Alpha::Sword::Equip() {}\n");
    git(&repo.root, &["add", "-A"]);
    git(&repo.root, &["commit", "-qm", "mixed symbol fixture"]);
    ue.json(&repo, &["index", "--text-only"]);

    let symbols = |query: &str, filters: &[&str]| {
        let mut args = vec!["search", query, "--snapshot", "--text-only"];
        args.extend_from_slice(filters);
        let result = ue.json(&repo, &args);
        let mut hits: Vec<_> = result["results"].as_array().unwrap().iter()
            .filter(|hit| hit["via"].as_array().unwrap().contains(&json!("symbol")))
            .map(|hit| (hit["source"].as_str().unwrap().to_owned(), hit["path"].as_str().unwrap().to_owned()))
            .collect();
        hits.sort();
        hits
    };
    let rust_hit = [("project".to_owned(), "src/render/mod.rs".to_owned())];
    assert_eq!(symbols("render::chain", &[]), rust_hit);
    assert_eq!(symbols("render::chain", &["--source", "project", "--lang", "rust", "--path", "SRC/RENDER/MOD.RS"]), rust_hit);
    for query in ["Render::chain", "render::CHAIN", "missing::chain"] {
        assert!(symbols(query, &[]).is_empty(), "{query}");
    }
    assert!(symbols("render::chain", &["--source", "unreal"]).is_empty());
    assert!(symbols("render::chain", &["--lang", "cpp"]).is_empty());
    assert!(symbols("render::chain", &["--path", "src/render/post.rs"]).is_empty());

    let simple = ue.json(&repo, &["symbol", "chain", "--snapshot", "--lang", "rust"]);
    let definitions = simple["definitions"].as_array().unwrap();
    assert_eq!(definitions.len(), 3, "{simple}");
    assert!(definitions.iter().any(|definition| definition["path"] == "src/other.rs"), "{simple}");

    let engine_hit = ("unreal".to_owned(), exact.to_owned());
    assert_eq!(symbols("Alpha::Sword::Equip", &[]), [
        ("project".to_owned(), "src/alpha.cpp".to_owned()), engine_hit.clone(),
    ]);
    assert_eq!(symbols("Alpha::Sword::Equip", &["--source", "unreal", "--lang", "cpp", "--path", exact]), [engine_hit]);
    assert!(symbols("Missing::Sword::Equip", &[]).is_empty());
}
