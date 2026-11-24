//! Real libclang integration. Set LORE_TEST_LIBCLANG to enable the compiler fixtures.
#![cfg(feature = "atlas")]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

struct Fixture {
    tmp: tempfile::TempDir,
    library: OsString,
}

impl Fixture {
    fn new() -> Option<Self> {
        let Some(library) = std::env::var_os("LORE_TEST_LIBCLANG") else {
            eprintln!("compiler fixture skipped: set LORE_TEST_LIBCLANG to load libclang");
            return None;
        };
        Some(Self { tmp: tempfile::tempdir().unwrap(), library })
    }

    fn project(&self, name: &str, unreal: Option<&Path>) -> PathBuf {
        let root = self.tmp.path().join(name);
        let mut config = "version = 1\n[atlas]\nenabled = true\n".to_owned();
        if let Some(engine) = unreal {
            config.push_str(&format!("[atlas.unreal]\nroot = {}\n", json!(path(engine))));
        }
        write(&root, "lore.toml", &config);
        root
    }

    fn build(&self, project: &Path, db: &Path, source: &str) -> Value {
        self.run(project, &["calls-index", "--compdb", &path(db), "--source", source])
    }

    fn run(&self, project: &Path, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_lore"))
            .arg("-C").arg(project).args(["--json", "atlas"]).args(args)
            .env("LORE_LIBCLANG", &self.library)
            .env("LORE_CACHE", self.tmp.path().join("cache"))
            .env("LORE_ATLAS", "on")
            .output().unwrap();
        assert!(output.status.success(), "{args:?}\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn path(path: &Path) -> String {
    path.to_str().unwrap().replace('\\', "/")
}

fn write(root: &Path, relative: &str, text: &str) -> PathBuf {
    let file = root.join(relative);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, text).unwrap();
    file
}

fn compdb(root: &Path, files: &[&str], include: &[&Path]) -> PathBuf {
    let commands: Vec<_> = files.iter().map(|file| {
        let mut args = vec!["clang++".to_owned(), "-std=c++20".to_owned()];
        for include in include {
            args.extend(["-I".to_owned(), path(include)]);
        }
        args.extend(["-c".to_owned(), (*file).to_owned()]);
        json!({"directory":path(root), "file":file, "arguments":args})
    }).collect();
    write(root, "compile_commands.json", &json!(commands).to_string())
}

#[test]
fn atlas_calls_crosses_project_and_shared_engine_by_identity() {
    let Some(fixture) = Fixture::new() else { return };
    let engine = fixture.tmp.path().join("installed engine");
    write(&engine, "Engine/Build/Build.version", r#"{"MajorVersion":5,"MinorVersion":8,"PatchVersion":4,"Changelist":58902052,"BranchName":"fixture"}"#);
    write(&engine, "Engine/Source/foreign.h", "namespace ue { int tick(int); }\n");
    write(&engine, "Engine/Source/foreign.cpp", "#include \"foreign.h\"\nnamespace ue { int tick(int x) { return x; } }\n");
    let engine_db = compdb(&engine, &["Engine/Source/foreign.cpp"], &[]);
    let a = fixture.project("project a", Some(&engine));
    let b = fixture.project("project b", Some(&engine));
    for project in [&a, &b] {
        write(project, "src/api.h", concat!(
            "namespace api { int free(int); struct Left { int tap(int); }; struct Right { int tap(int); }; struct Base { virtual int run(); };\n",
            "inline int inline_target() { return free(0); } }\n"
        ));
        write(project, "src/api.cpp", concat!(
            "#include \"api.h\"\n", "int api::free(int x) { return x; }\n",
            "int api::Left::tap(int x) { return x; }\n", "int api::Right::tap(int x) { return x; }\n",
            "int api::Base::run() { return 1; }\n"
        ));
        write(project, "src/caller.cpp", concat!(
            "#include \"api.h\"\n", "#include \"foreign.h\"\n",
            "namespace api {\n", "int drive(Left& left, Base& base) {\n",
            "  return free(1) + left.tap(2) + ue::tick(3) + base.run();\n", "}\n",
            "struct Derived : Base { int explicit_call() { return Base::run(); } };\n",
            "Left& acquire(Left& left) { return left; }\n",
            "int chained(Left& left) { return acquire(left).tap(2); }\n",
            "int to_header() { return inline_target(); }\n", "}\n"
        ));
        let db = compdb(project, &["src/api.cpp", "src/caller.cpp"], &[&engine.join("Engine/Source")]);
        let results = fixture.build(project, &db, "project");
        assert_eq!(results["failed"], 0, "{results}");
        assert_eq!(results["parsed"], 2);
    }
    let detached = fixture.run(&a, &["calls", "api::drive"]);
    let external = detached["edges"].as_array().unwrap().iter().find(|edge| edge["callee"]["name"] == "ue::tick").unwrap();
    assert_eq!(external["resolution"], "declaration_only");
    assert_eq!(external["callee"]["declaration"]["source"], "unreal");
    let engine_index = fixture.build(&a, &engine_db, "unreal");
    assert_eq!(engine_index["failed"], 0, "{engine_index}");
    assert_eq!(engine_index["parsed"], 1);
    let status_b = fixture.run(&b, &["calls-status"]);
    assert_eq!(status_b["sources"][1]["build"]["Changelist"], 58902052);
    assert_eq!(status_b["sources"][1]["path"], engine_index["source"]["path"]);
    assert_eq!(status_b["sources"][1]["revision"], engine_index["revision"]);
    let engine_repeat = fixture.build(&b, &engine_db, "unreal");
    assert_eq!(engine_repeat["parsed"], 0);
    assert_eq!(engine_repeat["reused"], 1);
    assert_eq!(engine_repeat["revision"], engine_index["revision"]);

    let result = fixture.run(&b, &["calls", "src/caller.cpp:5"]);
    let edges = result["edges"].as_array().unwrap();
    let edge = |name| edges.iter().find(|edge| edge["callee"]["name"] == name).unwrap();
    assert_eq!(edges.len(), 4, "{result}");
    assert_eq!(edge("api::free")["callee"]["definition"]["path"], "src/api.cpp");
    assert_eq!(edge("api::Left::tap")["callee"]["definition"]["line"], 3);
    assert_eq!(edge("api::Base::run")["dispatch"], "virtual_static_target");
    let call = edge("ue::tick");
    assert_eq!(call["resolution"], "confirmed", "{call}");
    assert_eq!(call["site"]["source"], "project");
    assert_eq!(call["site"]["line"], 5);
    assert_eq!(call["callee"]["definition"]["source"], "unreal");
    assert_eq!(call["callee"]["definition"]["line"], 2);
    let incoming = fixture.run(&b, &["calls", "ue::tick", "--direction", "in"]);
    assert_eq!(incoming["edges"][0]["caller"]["name"], "api::drive");
    let explicit = fixture.run(&b, &["calls", "api::Derived::explicit_call"]);
    assert_eq!(explicit["edges"][0]["dispatch"], "direct", "{explicit}");
    let chained = fixture.run(&b, &["calls", "api::chained"]);
    let chained = chained["edges"].as_array().unwrap();
    assert_eq!(chained.len(), 2, "{chained:?}");
    assert!(chained.iter().all(|edge| edge["resolution"] == "confirmed"));
    let inline = fixture.run(&b, &["calls", "api::to_header"]);
    assert_eq!(inline["edges"][0]["callee"]["definition"]["path"], "src/api.h");
    assert_eq!(inline["edges"][0]["resolution"], "confirmed");
    let ambiguity = fixture.run(&b, &["calls", "tap", "--direction", "in"]);
    assert_eq!(ambiguity["start_resolution"], "ambiguous");
    assert_eq!(ambiguity["start_candidates"].as_array().unwrap().len(), 2);
    assert_eq!(ambiguity["edges"], json!([]));
    let other = fixture.run(&b, &["calls", "api::Right::tap", "--direction", "in"]);
    assert_eq!(other["edges"], json!([]));

    write(&engine, "Engine/Build/Build.version", r#"{"MajorVersion":5,"MinorVersion":8,"PatchVersion":5,"Changelist":58902053,"BranchName":"fixture"}"#);
    let upgrade = fixture.run(&b, &["calls-status"]);
    assert_eq!(upgrade["sources"][1]["state"], "unindexed");
    assert_ne!(upgrade["sources"][1]["path"], status_b["sources"][1]["path"]);
}

#[test]
fn atlas_calls_rechecks_headers_updates_calls_and_keeps_ambiguous_overloads_unconfirmed() {
    let Some(fixture) = Fixture::new() else { return };
    let root = fixture.project("plain source", None);
    write(&root, "api.h", "int act(int); int act(double);\n");
    write(&root, "defs.cpp", "#include \"api.h\"\nint act(int x) { return x; }\nint act(double x) { return int(x); }\n");
    write(&root, "ok.cpp", "#include \"api.h\"\nint caller() { return act(1); }\n");
    write(&root, "bad.cpp", "#include \"api.h\"\nint ambiguous() { return act(1L); }\n");
    let db = compdb(&root, &["ok.cpp", "bad.cpp", "defs.cpp"], &[]);
    let first = fixture.build(&root, &db, "project");
    assert_eq!(first["parsed"], 3);
    assert_eq!(first["failed"], 1, "{first}");
    let invalid = fixture.run(&root, &["calls", "ambiguous", "--candidates"]);
    assert!(invalid["diagnostics"].to_string().contains("ambiguous"));
    assert!(!invalid["edges"].as_array().unwrap().iter().any(|edge| edge["resolution"] == "confirmed"), "{invalid}");
    let exact = fixture.run(&root, &["calls", "caller"]);
    assert_eq!(exact["edges"][0]["callee"]["signature"], "act(int)");
    assert_eq!(exact["edges"][0]["callee"]["definition"]["line"], 2);

    write(&root, "api.h", "\nint act(int); int act(double);\n");
    let stale = fixture.run(&root, &["calls", "caller", "--candidates"]);
    assert_eq!(stale["edges"][0]["resolution"], "stale", "{stale}");
    write(&root, "ok.cpp", "#include \"api.h\"\n\nint caller() { return act(2.0); }\n");
    let refreshed = fixture.build(&root, &db, "project");
    assert_eq!(refreshed["parsed"], 3);
    let updated = fixture.run(&root, &["calls", "caller"]);
    assert_eq!(updated["edges"][0]["callee"]["signature"], "act(double)");
    assert_eq!(updated["edges"][0]["site"]["line"], 3);
    fs::remove_file(root.join("ok.cpp")).unwrap();
    let deleted = fixture.build(&root, &db, "project");
    assert_eq!(deleted["removed"], 1);
    assert_eq!(fixture.run(&root, &["calls", "caller"])["start_candidates"], json!([]));
}
