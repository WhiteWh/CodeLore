//! The secret scan (docs/lore.md 11.9). Before the commit of every durable
//! write, every promote and every durable brief, each string field is scanned
//! and the write is refused with the match named, never repeated. Working
//! entries and history excerpts are redacted instead: a match becomes
//! `[redacted: <pattern>]`. Best-effort: it finds these patterns and
//! high-entropy strings; it does not prove a text holds no secret.
//!
//! hgs does not link this crate, so `src/state/teams/secrets.rs` carries a copy
//! of the scanner; both copies run the vectors of
//! `tests/fixtures/secret-scan-vectors.json`.
use crate::LoreError;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

/// One thing that looks like a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The pattern's short name, as a redaction shows it.
    pub pattern: &'static str,
    /// Byte offsets in the scanned text.
    pub start: usize,
    pub end: usize,
}

struct Pattern {
    name: &'static str,
    /// What the refusal calls it.
    said: &'static str,
    re: Regex,
    /// The capture group that is the secret itself (0: the whole match).
    group: usize,
}

fn patterns() -> &'static [Pattern] {
    static PATTERNS: OnceLock<Vec<Pattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let p = |name, said, re: &str, group| Pattern { name, said, re: Regex::new(re).expect("a valid secret pattern"), group };
        vec![
            p("pem-private-key", "a PEM private key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----", 0),
            p("aws-access-key-id", "an AWS access key id", r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b", 0),
            p("github-token", "a GitHub token", r"\bgh[pousr]_[A-Za-z0-9]{36,}", 0),
            p("slack-token", "a Slack token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}", 0),
            p("huggingface-token", "a Hugging Face token", r"\bhf_[A-Za-z0-9]{30,}", 0),
            p("api-key", "an API key", r"\bsk-(?:ant-)?[A-Za-z0-9_-]{20,}", 0),
            p("google-api-key", "a Google API key", r"\bAIza[0-9A-Za-z_-]{35}", 0),
            p("jwt", "a JSON Web Token", r"\beyJ[A-Za-z0-9_-]{4,}\.eyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]+", 0),
            p("url-credentials", "credentials in a URL", r"[A-Za-z][A-Za-z0-9+.-]*://[^\s:/@]+:([^\s@/]+)@", 1),
            // The name ends with one of these words, so `DB_PASSWORD`,
            // `access_token`, `client_secret` and a quoted JSON `"password"`
            // count too; the name is not the secret, its value is.
            p(
                "secret-assignment",
                "a secret assigned to a name",
                r#"(?i)(?:^|[^A-Za-z0-9])([A-Za-z0-9_.-]*(?:password|passwd|secret|token|api_key|apikey))\s*["']?\s*[=:]\s*["']?([^\s"',;]{8,})"#,
                2,
            ),
        ]
    })
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9+/_=.-]{24,}").expect("the token pattern"))
}

fn uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$").expect("the uuid pattern"))
}

/// Shannon entropy in bits per character.
pub fn entropy(text: &str) -> f64 {
    let mut counts = std::collections::HashMap::new();
    let total = text.chars().count() as f64;
    if total == 0.0 {
        return 0.0;
    }
    for ch in text.chars() {
        *counts.entry(ch).or_insert(0usize) += 1;
    }
    counts.values().map(|&n| {
        let p = n as f64 / total;
        -p * p.log2()
    }).sum()
}

/// A value that is a placeholder, not a secret: `<your token>`, `${NAME}`,
/// `********`, or a run of mask characters (`xxxxxxxxxxxx`). Only a value that
/// is *all* mask characters is a placeholder: `xxxxK7p2Lq9` is a secret whose
/// text happens to start with x.
fn placeholder(value: &str) -> bool {
    (value.starts_with('<') && value.ends_with('>'))
        || value.starts_with("${")
        || value.contains("***")
        || (value.chars().any(|ch| ch.is_alphabetic())
            && value.chars().all(|ch| matches!(ch, 'x' | 'X' | '*' | '-' | '_' | '.' | '#' | ' ')))
}

/// A segment that reads as a word: letters, optionally a trailing number
/// (`post`, `process2`, `v15`, `2026`). A random token's digits sit between its
/// letters (`Qx7pL2mZ9vR4...`), so a base64 string is not a word.
fn word(segment: &str) -> bool {
    if segment.is_empty() || segment.chars().count() > 40 {
        return false;
    }
    if segment.chars().all(|ch| ch.is_ascii_digit()) {
        return true;
    }
    let mut letters = 0;
    let mut digits_after_letters = false;
    for ch in segment.chars() {
        if ch.is_ascii_digit() {
            digits_after_letters = true;
            continue;
        }
        if !ch.is_ascii_alphabetic() || digits_after_letters {
            return false;
        }
        letters += 1;
    }
    letters > 0
}

/// Whether a token reads as a path or a dotted name (`src/render/post.py`,
/// `scene.depth_buffer`), so the entropy test may let it pass. A base64 string
/// with `/` or `.` does not: its parts are not words.
fn path_or_name(token: &str) -> bool {
    if token.contains("://") {
        return true;
    }
    let parts: Vec<&str> = token.split(['/', '.', '-', '_']).filter(|part| !part.is_empty()).collect();
    parts.len() >= 2
        && parts.len() <= 24
        && parts.iter().any(|part| part.chars().any(|ch| ch.is_alphabetic()))
        && parts.iter().all(|part| segment_ok(part))
}

/// A path segment: a word, a hex commit or hash, or a UUID (a path may hold
/// any of them, as `.git/objects/<hash>/` does).
fn segment_ok(segment: &str) -> bool {
    word(segment)
        || (segment.len() >= 7 && segment.len() <= 64 && segment.chars().all(|ch| ch.is_ascii_hexdigit()))
        || uuid_re().is_match(segment)
}

/// Exempt: a hex string of 7 to 64 characters (a commit or hash, in any
/// field), a UUID, and a token that reads as a path or a name.
fn exempt_token(token: &str) -> bool {
    let hex = token.len() >= 7 && token.len() <= 64 && token.chars().all(|ch| ch.is_ascii_hexdigit());
    hex || uuid_re().is_match(token) || path_or_name(token) || file_path(token) || anchor(token) || camel_case(token)
}

/// The extensions a cited file ends with.
const EXTENSIONS: &[&str] = &[
    "md", "txt", "rs", "py", "pyi", "json", "jsonl", "toml", "yml", "yaml", "ts", "tsx", "js", "jsx", "mjs", "cjs", "css",
    "html", "svg", "png", "kt", "kts", "java", "gradle", "swift", "sh", "ps1", "bat", "c", "h", "cc", "cpp", "hpp", "go",
    "rb", "sql", "xml", "proto", "cfg", "ini", "lock", "csv", "log", "qml", "cmake", "plist", "pbxproj", "sqlite3",
];

/// A part of a slug: lowercase letters and digits in any order, with a letter
/// (`phase3a`, `i18n`, `80x24`, `5wmh42`).
fn slug(part: &str) -> bool {
    (1..=24).contains(&part.len())
        && part.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        && part.chars().any(|ch| ch.is_ascii_lowercase())
}

/// A file path as documents cite it: a `/`, a known extension, and parts that
/// are words, hashes or lowercase slugs
/// (`docs/superpowers/plans/2026-10-18-task-tracker-phase3a-desktop.md`,
/// `client/scripts/generate-i18n-catalog.py`). Zerus task #40: `word` refused
/// `phase3a`, `i18n` and `80x24` (letters after digits), so whole batches of
/// plan citations were refused. A base64 string has mixed-case runs between
/// its digits, so its parts are none of these.
fn file_path(token: &str) -> bool {
    let Some((stem, extension)) = token.rsplit_once('.') else { return false };
    if !stem.contains('/') || !EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) {
        return false;
    }
    let parts: Vec<&str> = stem.split(['/', '.', '-', '_']).filter(|part| !part.is_empty()).collect();
    !parts.is_empty() && parts.len() <= 32 && parts.iter().all(|part| segment_ok(part) || slug(part) || camel_humps(part, 1))
}

/// A word of letters a person reads: two or more, with a vowel.
fn pronounceable(part: &str) -> bool {
    part.len() >= 2
        && part.chars().all(|ch| ch.is_ascii_alphabetic())
        && part.chars().any(|ch| matches!(ch.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y'))
}

/// A heading anchor or a slug: lowercase parts joined by `-`, at least three,
/// two thirds of them words (`phone-task-exchange-through-the-relay-implementation-plan-5b1`,
/// `tauri-client-phase-3b-settings-zerus-5wmh24`).
fn anchor(token: &str) -> bool {
    if !token.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
        return false;
    }
    let parts: Vec<&str> = token.split('-').filter(|part| !part.is_empty()).collect();
    let words = parts.iter().filter(|part| pronounceable(part)).count();
    parts.len() >= 3 && parts.iter().all(|part| slug(part)) && words * 3 >= parts.len() * 2
}

/// The humps of a CamelCase name: `PreferPage60FPSEnabled` is `Prefer`,
/// `Page`, `60`, `FPS`, `Enabled`.
fn humps(token: &str) -> Vec<&str> {
    let bytes = token.as_bytes();
    let mut humps = Vec::new();
    let mut start = 0;
    for at in 1..bytes.len() {
        let (before, here) = (bytes[at - 1], bytes[at]);
        let lower_next = bytes.get(at + 1).is_some_and(u8::is_ascii_lowercase);
        let starts = here.is_ascii_digit() != before.is_ascii_digit()
            || (here.is_ascii_uppercase() && (before.is_ascii_lowercase() || (before.is_ascii_uppercase() && lower_next)));
        if starts {
            humps.push(&token[start..at]);
            start = at;
        }
    }
    humps.push(&token[start..]);
    humps
}

/// A CamelCase identifier: at least three humps, each a word with a vowel, a
/// short acronym or a short number, the first possibly one lowercase letter
/// (`kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`,
/// `PreferPageRenderingUpdatesNear60FPSEnabled`). A random string's humps are
/// mostly one or two letters without a vowel (`Qx`, `7`, `pL`).
fn camel_case(token: &str) -> bool {
    camel_humps(token, 3)
}

/// A name of at least `least` humps, each a word with a vowel, a short
/// acronym or a short number - [`camel_case`]; in a cited file's path one
/// hump will do (`Docs/PhotoModePhase2Facts.md`, `Source/UE5Bridge.cpp`).
fn camel_humps(token: &str, least: usize) -> bool {
    if !token.starts_with(|ch: char| ch.is_ascii_alphabetic()) || !token.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return false;
    }
    let humps = humps(token);
    let numbers = humps.iter().filter(|hump| hump.starts_with(|ch: char| ch.is_ascii_digit())).count();
    let mut single_capitals = 0;
    humps.len() >= least
        && numbers <= 2
        && humps.iter().enumerate().all(|(n, hump)| {
            if hump.chars().all(|ch| ch.is_ascii_digit()) {
                hump.len() <= 4
            } else if hump.chars().all(|ch| ch.is_ascii_uppercase()) {
                single_capitals += usize::from(hump.len() == 1);
                hump.len() <= 6 && single_capitals <= 1
            } else {
                (n == 0 && hump.len() == 1) || pronounceable(hump)
            }
        })
}

/// Everything in `text` that looks like a secret, in order, without overlaps.
/// A literal in `allow` is not a match.
pub fn scan(text: &str, allow: &[String]) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    let overlaps = |found: &[Found], start: usize, end: usize| found.iter().any(|f| start < f.end && f.start < end);
    for pattern in patterns() {
        for captures in pattern.re.captures_iter(text) {
            let Some(hit) = captures.get(pattern.group) else { continue };
            let secret = hit.as_str();
            if pattern.name == "secret-assignment" && placeholder(secret) {
                continue;
            }
            if allow.iter().any(|allowed| allowed == secret) || overlaps(&found, hit.start(), hit.end()) {
                continue;
            }
            found.push(Found { pattern: pattern.name, start: hit.start(), end: hit.end() });
        }
    }
    for token in token_re().find_iter(text) {
        // A sentence's full stop is not part of the token.
        let word = token.as_str().trim_end_matches('.');
        let end = token.start() + word.len();
        if word.len() < 24 || exempt_token(word) || entropy(word) < 4.0 || allow.iter().any(|allowed| allowed == word) {
            continue;
        }
        if overlaps(&found, token.start(), end) {
            continue;
        }
        found.push(Found { pattern: "high-entropy", start: token.start(), end });
    }
    found.sort_by_key(|f| f.start);
    found
}

/// What a refusal shows of a match: at most its first and last 4 characters.
pub fn masked(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "…".into();
    }
    format!("{}…{}", chars[..4].iter().collect::<String>(), chars[chars.len() - 4..].iter().collect::<String>())
}

fn said(pattern: &str) -> &'static str {
    patterns().iter().find(|p| p.name == pattern).map(|p| p.said).unwrap_or("a high-entropy string")
}

/// `text` with every match replaced by `[redacted: <pattern>]`, and the
/// patterns that were replaced.
pub fn redact(text: &str, allow: &[String]) -> (String, Vec<&'static str>) {
    let found = scan(text, allow);
    if found.is_empty() {
        return (text.to_owned(), Vec::new());
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    let mut names = Vec::new();
    for f in &found {
        out.push_str(&text[at..f.start]);
        out.push_str(&format!("[redacted: {}]", f.pattern));
        at = f.end;
        if !names.contains(&f.pattern) {
            names.push(f.pattern);
        }
    }
    out.push_str(&text[at..]);
    (out, names)
}

/// One refused field: `{"field","pattern","length","masked"}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub field: String,
    pub pattern: &'static str,
    pub length: usize,
    pub masked: String,
}

impl Refused {
    pub fn line(&self) -> String {
        format!("`{}` holds what looks like {} (`{}`, {} characters)", self.field, said(self.pattern), self.masked, self.length)
    }
    pub fn json(&self) -> Value {
        json!({"field": self.field, "pattern": self.pattern, "length": self.length, "masked": self.masked})
    }
}

fn walk(value: &Value, path: &str, allow: &[String], out: &mut Vec<Refused>) {
    match value {
        Value::String(text) => {
            for f in scan(text, allow) {
                let secret = &text[f.start..f.end];
                out.push(Refused { field: path.to_owned(), pattern: f.pattern, length: secret.chars().count(), masked: masked(secret) });
            }
        }
        Value::Array(items) => {
            for (n, item) in items.iter().enumerate() {
                walk(item, &format!("{path}[{n}]"), allow, out);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                let inner = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                walk(item, &inner, allow, out);
            }
        }
        _ => {}
    }
}

/// Every string field of `record` that holds a secret.
pub fn check_record(record: &Value, allow: &[String]) -> Vec<Refused> {
    let mut out = Vec::new();
    walk(record, "", allow, &mut out);
    out
}

/// The refusal of a durable write: names each match, repeats none.
pub fn refuse(refused: &[Refused]) -> LoreError {
    let lines: Vec<String> = refused.iter().map(Refused::line).collect();
    let first = lines.first().cloned().unwrap_or_default();
    LoreError::with(format!("refused: {first}; nothing was written"), lines)
}

/// Refuses a record that holds a secret.
pub fn guard(record: &Value, allow: &[String]) -> Result<(), LoreError> {
    let refused = check_record(record, allow);
    if refused.is_empty() { Ok(()) } else { Err(refuse(&refused)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/lore.md 11.15 test 8: the shared vectors, one sample per pattern and
    /// the near misses that pass, are the same for hgs's copy of the scanner.
    #[test]
    fn the_shared_vectors_are_found_or_clean() {
        let vectors: Value = serde_json::from_str(include_str!("../tests/fixtures/secret-scan-vectors.json")).expect("the shared secret vectors");
        for case in vectors["found"].as_array().expect("found vectors") {
            // The file stores secret-shaped samples split by a marker, so the
            // repository holds no literal in a real token format.
            let joined = case["text"].as_str().unwrap().replace("<split>", "");
            let text = joined.as_str();
            let pattern = case["pattern"].as_str().unwrap();
            assert_eq!(scan(text, &[]).first().map(|found| found.pattern), Some(pattern), "{text}");
        }
        for case in vectors["clean"].as_array().expect("clean vectors") {
            let text = case["text"].as_str().unwrap();
            assert_eq!(scan(text, &[]), Vec::new(), "{text}");
        }
    }

    #[test]
    fn a_refusal_names_the_field_and_masks_the_match() {
        let record = json!({"kind": "fault", "fix": concat!("rotate AKIA", "ABCDEFGHIJKLWXYZ"), "refs": {"files": ["a.py"]}});
        let error = guard(&record, &[]).unwrap_err();
        assert_eq!(error.message, "refused: `fix` holds what looks like an AWS access key id (`AKIA…WXYZ`, 20 characters); nothing was written");
        assert!(!error.to_string().contains(concat!("AKIA", "ABCDEFGHIJKLWXYZ")));
        let refused = check_record(&record, &[]);
        assert_eq!(refused[0].json(), json!({"field": "fix", "pattern": "aws-access-key-id", "length": 20, "masked": "AKIA…WXYZ"}));
        // An allowed literal passes the scan.
        assert!(guard(&record, &[concat!("AKIA", "ABCDEFGHIJKLWXYZ").into()]).is_ok());
    }

    #[test]
    fn ids_and_references_that_read_as_ids_are_not_entropy_tested() {
        // An id, a commit, a path and a slug read as themselves; a random token
        // in an id field is still a secret (no field name is a blind spot).
        let record = json!({
            "id": "f-3f2a9c1d",
            "refs": {"records": ["f-3f2a9c1d"], "files": ["src/render/post_process/temporal_resolve_pass.py"]},
            "commits": ["3f2a9c1d4e5b6a7c8d9e0f1a2b3c4d5e6f7a8b9c"],
            "path": "src/render/post_process/temporal_resolve_pass.py",
            "body": "plain words"
        });
        assert!(guard(&record, &[]).is_ok());
        let random = "Qx7pL2mZ9vR4tY8wK3nB6cD1fG5hJ0sA";
        assert_eq!(check_record(&json!({"id": random}), &[])[0].field, "id");
        assert_eq!(check_record(&json!({"refs": {"records": [random]}}), &[])[0].field, "refs.records[0]");
        // A pattern still applies to an id field.
        assert!(guard(&json!({"id": concat!("AKIA", "ABCDEFGHIJKLWXYZ")}), &[]).is_err());
    }

    #[test]
    fn a_cited_file_with_a_camel_case_name_is_a_path() {
        // a source's path and title, and a ref to it, in a game's docs
        for path in ["OwnGame/Docs/PhotoModePhase2Facts.md", "Source/OwnGame/UE5Bridge.cpp", "Plugins/AudioMixer/Docs/V2Notes.md"] {
            let record = json!({"path": path, "title": path, "refs": {"files": [path], "links": [path]}});
            assert!(guard(&record, &[]).is_ok(), "{path}: {:?}", check_record(&record, &[]));
        }
        // a random token dressed as a path is still one
        let random = "Qx7pL2mZ9vR4tY8wK3nB6cD1fG5hJ0sA";
        assert!(!check_record(&json!({"path": format!("docs/{random}.md")}), &[]).is_empty());
    }

    #[test]
    fn redaction_replaces_each_match_and_keeps_the_rest() {
        let (text, names) = redact(concat!("use ghp_", "0123456789abcdefghijABCDEFGHIJ012345 and password=hunter2hunter2"), &[]);
        assert_eq!(text, "use [redacted: github-token] and password=[redacted: secret-assignment]");
        assert_eq!(names, ["github-token", "secret-assignment"]);
        assert_eq!(redact("nothing here", &[]), ("nothing here".to_owned(), Vec::new()));
    }
}
