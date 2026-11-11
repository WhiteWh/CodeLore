//! Russian text: another form of a word finds the record without the model,
//! the duplicate check compares stems, and a Russian title makes an id that
//! says something. And `lore digest`, the list a session-start hook shows.
use codelore::cli::{self, Env};
use codelore::Store;
use serde_json::{json, Value};
use std::path::PathBuf;

fn fault(id: &str, area: &str, title: &str, symptom: &str, status: &str, updated: &str) -> Value {
    json!({"v": 3, "id": id, "kind": "fault", "area": area, "title": title, "symptom": symptom, "cause": "c", "fix": "f", "check": "k",
           "status": status, "created": "2026-11-01", "updated": updated, "by": "me"})
}

fn store(records: &[Value]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = codelore::config::simplify(&std::fs::canonicalize(tmp.path()).unwrap());
    std::fs::write(root.join("lore.toml"), "version = 1\ndir = \"lore\"\nareas = [\"editor\", \"build\"]\n\n[commit]\nmode = \"none\"\n\n[embed]\nprovider = \"none\"\n").unwrap();
    std::fs::create_dir_all(root.join("lore")).unwrap();
    Store::new(&root).write("lore/editor.jsonl", records).unwrap();
    (tmp, root)
}

fn found(root: &PathBuf, query: &str) -> Vec<String> {
    let env = Env { cwd: root.clone(), finder: Some(&codelore::builtin::BUILTIN), ..Env::default() };
    let out = cli::run(&["--json", "find", query, "--text-only"], &env);
    assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let found: Value = serde_json::from_str(&out.stdout).unwrap();
    found["results"].as_array().unwrap().iter().filter_map(|hit| hit["record"]["id"].as_str().map(str::to_owned)).collect()
}

#[test]
fn another_form_of_a_russian_word_finds_the_record() {
    let (_tmp, root) = store(&[
        fault("import-crash", "editor", "Редактор крашит при импорте таблицы", "Импорт таблицы крашит редактор, если у строки вложенное имя", "live", "2026-11-02"),
        fault("other", "editor", "Медленная сборка шейдеров", "сборка идёт час", "live", "2026-11-02"),
    ]);
    for query in ["краш", "краша", "крашится", "вложенным", "таблицей", "импорта", "крашит"] {
        assert_eq!(found(&root, query), ["import-crash"], "{query}");
    }
}

#[test]
fn the_duplicate_check_compares_russian_stems() {
    let a = json!({"title": "Импорт таблицы крашит редактор", "symptom": "вложенное имя строки", "cause": "импортёр"});
    let b = json!({"title": "Импорта таблиц крашится редактором", "symptom": "вложенным именем строк", "cause": "импортёр"});
    let (wa, wb) = (codelore::dedupe::words(&a), codelore::dedupe::words(&b));
    assert!(!wa.is_empty(), "Russian words are compared at all");
    assert!(codelore::dedupe::jaccard(&wa, &wb) >= 0.5, "{wa:?} {wb:?}");
}

#[test]
fn a_russian_title_makes_a_latin_id() {
    assert_eq!(codelore::ops::slugify("Редактор крашит при импорте"), "redaktor-krashit-pri-importe");
    assert_eq!(codelore::ops::slugify("DataTable: импорт слеп к Name"), "datatable-import-slep-k-name");
    assert_eq!(codelore::ops::slugify("plain English title"), "plain-english-title");
}

#[test]
fn a_digest_lists_what_is_in_force_newest_first_within_its_bytes() {
    let mut records = vec![
        fault("old-live", "editor", "старая ловушка", "s", "live", "2026-11-01"),
        fault("new-live", "build", "новая ловушка", "s", "live", "2026-11-05"),
        fault("cured-one", "editor", "уже вылечено", "s", "cured", "2026-11-06"),
        fault("gone-one", "editor", "устарело", "s", "obsolete", "2026-11-06"),
    ];
    records.push(json!({"v": 3, "id": "a-choice", "kind": "decision", "area": "editor", "title": "держим одну модель", "question": "q", "choice": "c",
        "reason": "r", "decided_by": "owner", "status": "active", "created": "2026-11-01", "updated": "2026-11-03", "by": "me"}));
    let (_tmp, root) = store(&records);
    let env = Env { cwd: root.clone(), ..Env::default() };
    let json_of = |args: &[&str]| -> Value {
        let out = cli::run(args, &env);
        assert_eq!(out.code, 0, "{}{}", out.stdout, out.stderr);
        serde_json::from_str(&out.stdout).unwrap()
    };
    let ids = |digest: &Value| -> Vec<String> { digest["records"].as_array().unwrap().iter().map(|record| record["id"].as_str().unwrap().to_owned()).collect() };
    let all = json_of(&["--json", "digest"]);
    assert_eq!(ids(&all), ["new-live", "a-choice", "old-live"]);
    assert_eq!(all["total"], 3);
    assert_eq!(ids(&json_of(&["--json", "digest", "--kind", "fault", "--area", "editor"])), ["old-live"]);
    let small = json_of(&["--json", "digest", "--bytes", "60"]);
    assert_eq!((small["shown"].as_u64(), small["total"].as_u64()), (Some(1), Some(3)));
    let text = cli::run(&["digest", "--bytes", "60"], &env);
    assert!(text.stdout.contains("fault [build] new-live: новая ловушка") && text.stdout.contains("and 2 more"), "{}", text.stdout);
    assert_eq!(cli::run(&["digest", "--kind", "nonsense"], &env).code, 1);
}
