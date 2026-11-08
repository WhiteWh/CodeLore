//! Ported CodeLore conformance cases: the same inputs and the same refusals.
use serde_json::{json, Value};
use codelore::{canonical, parse, serialize, validate};

fn fault(id: &str) -> Value {
    json!({"v":1,"id":id,"kind":"fault","area":"git","title":"t","status":"live","created":"2026-10-24",
        "updated":"2026-10-24","by":"me","symptom":"s","cause":"c","fix":"f","check":"k"})
}

#[test]
fn round_trip_keeps_bytes_and_unknown_fields() {
    let mut first = fault("b-one");
    first["extra"] = json!({"x":[1,"ü"]});
    let records = vec![first, fault("a-two")];
    let bytes = serialize(&records);
    let back = parse(&bytes, "x.jsonl").unwrap();
    assert_eq!(serialize(&back), bytes);
    assert_eq!(back[1]["extra"], json!({"x":[1,"ü"]}));
    assert!(bytes.windows(2).any(|pair| pair == "ü".as_bytes()));
}

#[test]
fn items_sort_by_number() {
    let plan = json!({"v":1,"id":"pl","kind":"plan","area":"plans","title":"p","goal":"g","status":"live",
        "created":"2026-10-24","updated":"2026-10-24","by":"me"});
    let item = |n: u32| json!({"v":1,"id":format!("pl#{n}"),"kind":"item","area":"plans","plan":"pl",
        "title":format!("i{n}"),"status":"open","created":"2026-10-24","updated":"2026-10-24","by":"me"});
    let bytes = serialize(&[item(10), item(9), plan, item(2)]);
    let ids: Vec<String> = parse(&bytes, "p.jsonl").unwrap().iter().map(|r| r["id"].as_str().unwrap().into()).collect();
    assert_eq!(ids, ["pl", "pl#2", "pl#9", "pl#10"]);
}

#[test]
fn store_refusals_match_python() {
    let good = serialize(&[fault("aa"), fault("bb")]);
    let crlf: Vec<u8> = good.iter().flat_map(|byte| if *byte == b'\n' { vec![b'\r', b'\n'] } else { vec![*byte] }).collect();
    assert!(parse(&crlf, "x").is_err());
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend(&good);
    assert!(parse(&bom, "x").unwrap_err().to_string().contains("BOM"));
    let lines: Vec<&[u8]> = good.split(|b| *b == b'\n').collect();
    let swapped = [lines[1], b"\n", lines[0], b"\n"].concat();
    assert!(parse(&swapped, "x").unwrap_err().to_string().contains("out of order"));
    let blank = [lines[0], b"\n\n", lines[1], b"\n"].concat();
    assert!(parse(&blank, "x").unwrap_err().to_string().contains("blank"));
    assert!(parse(&good[..good.len() - 1], "x").unwrap_err().to_string().contains("final newline"));
    let twice = [lines[0], b"\n", lines[0], b"\n"].concat();
    assert!(parse(&twice, "x").unwrap_err().to_string().contains("twice"));
    let spaced = canonical(&fault("aa")).replace("\":\"", "\": \"").into_bytes();
    assert!(parse(&[spaced, b"\n".to_vec()].concat(), "x").unwrap_err().to_string().contains("canonical"));
    assert!(parse(b"[1]\n", "x").is_err());
}

#[test]
fn schema_refusals_match_python() {
    let areas = vec!["git".to_owned()];
    assert!(validate(&fault("aa"), &areas, None).is_empty());
    let cases = [
        (json!({"v":1,"id":"aa","kind":"fault","area":"git","title":"t","status":"fixed","created":"2026-10-24",
            "updated":"2026-10-24","by":"me","symptom":"s","cause":"c","fix":"f","check":"k"}), "`status` must be one of"),
        (json!({"v":1,"id":"Bad Id","kind":"fault","area":"git","title":"t","status":"live","created":"2026-10-24",
            "updated":"2026-10-24","by":"me","symptom":"s","cause":"c","fix":"f","check":"k"}), "`id` must match"),
        (json!({"v":1,"id":"aa","kind":"fault","area":"nowhere","title":"t","status":"live","created":"2026-10-24",
            "updated":"2026-10-24","by":"me","symptom":"s","cause":"c","fix":"f","check":"k"}), "not one of lore.toml's areas"),
    ];
    for (record, words) in cases {
        let errors = validate(&record, &areas, None);
        assert!(errors.iter().any(|error| error.contains(words)), "{words} in {errors:?}");
    }
    let mut long = fault("aa");
    long["title"] = json!("x".repeat(121));
    assert!(validate(&long, &areas, None).iter().any(|e| e.contains("limit is 120")));
    let mut missing = fault("aa");
    missing.as_object_mut().unwrap().remove("cause");
    assert!(validate(&missing, &areas, None).iter().any(|e| e.contains("`cause` is required")));
    let mut item = json!({"v":1,"id":"pl#1","kind":"item","area":"plans","plan":"pl","title":"i","status":"done",
        "created":"2026-10-24","updated":"2026-10-24","by":"me"});
    assert!(validate(&item, &areas, None).iter().any(|e| e.contains("`commits` is required when `done`")));
    item["status"] = json!("dropped");
    item["reason"] = json!("r");
    assert!(validate(&item, &areas, None).iter().any(|e| e.contains("`decided_by` is required when `dropped`")));
}


#[test]
fn fixed_embeddings_keep_the_ollama_shape_offline() {
    use codelore::embed::{Embeddings, Fixed};
    let fixed = Fixed(vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
    let vectors = fixed.embed(&["one".into(), "two".into()]).unwrap();
    assert_eq!(vectors, vec![vec![0.1, 0.2], vec![0.3, 0.4]]);
    assert!(fixed.embed(&["one".into(), "two".into(), "three".into()]).is_err());
}

#[test]
fn config_refuses_like_python() {
    use codelore::config::parse;
    let data: toml::Value = toml::from_str("version = 2\ndir = \"lore\"\nareas = [\"Plans\", \"plans\"]\n[commit]\nmode = \"yolo\"\n[other]\nx = 1").unwrap();
    let error = parse(&data, std::path::Path::new("."), "lore.toml").unwrap_err().to_string();
    for word in ["version", "must match", "reserved", "commit.mode"] {
        assert!(error.contains(word), "{word} missing in {error}");
    }
    // A top-level table this version does not know - a newer lore's - is passed
    // over with a warning, so an older binary keeps the project's memory.
    assert!(!error.contains("`other`"), "{error}");
    let newer: toml::Value = toml::from_str("version = 1\n[future]\nx = 1\n[commit]\nmode = \"plain\"").unwrap();
    let config = parse(&newer, std::path::Path::new("."), "lore.toml").unwrap();
    assert_eq!(config.warnings, ["lore.toml: unknown key `future` is ignored (written by a newer lore?)"]);
    let strict: toml::Value = toml::from_str("version = 1\n[commit]\nmood = \"plain\"").unwrap();
    assert!(parse(&strict, std::path::Path::new("."), "lore.toml").unwrap_err().to_string().contains("unknown key `commit.mood`"));
    let good: toml::Value = toml::from_str("version = 1\nareas = [\"git\"]\n[commit]\nmode = \"plain\"\n[embed]\nmodel = \"nomic-embed-text-v1.5\"").unwrap();
    let config = parse(&good, std::path::Path::new("."), "lore.toml").unwrap();
    assert_eq!(config.prefixes(), ("search_document: ".into(), "search_query: ".into()));
}

#[test]
fn plan_items_move_through_their_lifecycle() {
    use serde_json::json;
    use codelore::{Store, Writer};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let plan = json!({"v":3,"id":"pl","kind":"plan","area":"plans","title":"p","goal":"g","status":"live",
        "created":"2026-10-24","updated":"2026-10-24","by":"me"});
    let item = json!({"v":3,"id":"pl#1","kind":"item","area":"plans","plan":"pl","title":"i","status":"open",
        "created":"2026-10-24","updated":"2026-10-24","by":"me"});
    store.write("lore/plans/pl.jsonl", &[plan, item]).unwrap();
    let writer = Writer::acquire(dir.path(), "test").unwrap();
    let areas = vec!["plans".to_owned()];
    assert!(writer.item_status(&store, "pl", "pl#1", "done", &json!({}).as_object().unwrap().clone(), &areas).is_err());
    writer.item_status(&store, "pl", "pl#1", "done", json!({"commits":["abcd1234"]}).as_object().unwrap(), &areas).unwrap();
    assert!(Writer::close_plan(&store, "pl", "finished", &areas).is_ok());
}

#[test]
fn the_embed_provider_is_builtin_none_or_the_old_name_of_builtin() {
    use codelore::config::parse;
    for text in ["builtin", "ollama", "none"] {
        let data: toml::Value = toml::from_str(&format!("version = 1\n[embed]\nprovider = \"{text}\"")).unwrap();
        assert_eq!(parse(&data, std::path::Path::new("."), "lore.toml").unwrap().embed_provider, text);
    }
    let data: toml::Value = toml::from_str("version = 1\n[embed]\nprovider = \"openai\"").unwrap();
    let error = parse(&data, std::path::Path::new("."), "lore.toml").unwrap_err().to_string();
    assert!(error.contains("builtin, ollama, none"), "{error}");
}
