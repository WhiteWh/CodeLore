//! Verbatim history, federated and never copied (docs/lore.md 11.7). Lore
//! keeps no transcript and no index of one: `find` asks a `HistorySource`
//! for at most 10 hits, keeps the 3 best, redacts every excerpt before it
//! leaves the provider and bounds the channel to 8 KiB. Zerus's provider is
//! its own content-search index (`hgs search --indexed`); standalone lore
//! has none and says so once.
use crate::{secrets, LoreError};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// At most this many hits are asked for.
pub const ASK: usize = 10;
/// At most this many are returned.
pub const KEEP: usize = 3;
pub const EXCERPT: usize = 400;
pub const CHANNEL_BYTES: usize = 8 * 1024;
/// A provider slower than this is left out of the search.
pub const WAIT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistoryHit {
    pub session: String,
    pub run_id: String,
    pub conversation_id: String,
    pub message_id: String,
    pub role: String,
    pub at: f64,
    pub excerpt: String,
    pub score: f64,
}

impl HistoryHit {
    /// `{session, message_id, role, at, excerpt}` (11.7).
    pub fn json(&self) -> Value {
        json!({"session": self.session, "message_id": self.message_id, "role": self.role, "at": self.at, "excerpt": self.excerpt})
    }
}

/// Whose sessions a caller may search (11.7): a member its own, the lead its
/// team's, the owner any. `sessions: None` is the owner's scope.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scope {
    pub sessions: Option<Vec<String>>,
    /// Sessions whose `memory_history` is off: never returned.
    pub excluded: Vec<String>,
}

impl Scope {
    pub fn admits(&self, session: &str) -> bool {
        !self.excluded.iter().any(|name| name == session) && self.sessions.as_ref().is_none_or(|names| names.iter().any(|name| name == session))
    }
}

/// A corpus of verbatim history that lore searches and never stores.
pub trait HistorySource: Send + Sync {
    fn search(&self, query: &str, scope: &Scope, limit: usize) -> Result<Vec<HistoryHit>, LoreError>;
}

/// The excerpt of `text` around the first query word it holds, at most 400
/// characters.
pub fn excerpt(text: &str, query: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = flat.to_lowercase();
    let at = crate::working::words_of(query).iter().filter_map(|word| lower.find(word.as_str())).min().unwrap_or(0);
    let chars: Vec<char> = flat.chars().collect();
    let centre = lower[..at.min(lower.len())].chars().count();
    let start = centre.saturating_sub(EXCERPT / 2);
    let end = (start + EXCERPT).min(chars.len());
    let start = end.saturating_sub(EXCERPT);
    chars[start..end].iter().collect()
}

/// Asks the provider (at most 500 ms), keeps the best 3 in scope, redacts and
/// bounds them. The note says why the channel is empty when it is.
pub fn federate(source: Option<&Arc<dyn HistorySource>>, query: &str, scope: &Scope) -> (Vec<HistoryHit>, Option<String>) {
    let Some(source) = source else { return (Vec::new(), Some("no history source".into())) };
    let (send, receive) = mpsc::channel();
    let (source, wanted, inner) = (Arc::clone(source), query.to_owned(), scope.clone());
    std::thread::spawn(move || {
        let _ = send.send(source.search(&wanted, &inner, ASK));
    });
    let hits = match receive.recv_timeout(WAIT) {
        Ok(Ok(hits)) => hits,
        Ok(Err(error)) => return (Vec::new(), Some(format!("history unavailable: {error}"))),
        Err(_) => return (Vec::new(), Some("history unavailable: the source took longer than 500 ms".into())),
    };
    let mut hits: Vec<HistoryHit> = hits.into_iter().filter(|hit| scope.admits(&hit.session)).take(ASK).collect();
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    let mut bytes = 0;
    for mut hit in hits.into_iter().take(KEEP) {
        // Redacted before it leaves the provider; an allowed literal too.
        let short: String = hit.excerpt.chars().take(EXCERPT).collect();
        hit.excerpt = secrets::redact(&short, &[]).0;
        let size = hit.json().to_string().len();
        if bytes + size > CHANNEL_BYTES {
            break;
        }
        bytes += size;
        out.push(hit);
    }
    (out, None)
}

/// Zerus's provider: its local content-search index over the sessions'
/// histories (`hgs search --indexed`, content-search-query-v1). Every
/// returned session is proved by hgs itself.
pub struct HgsSource {
    pub hgs: PathBuf,
}

impl HistorySource for HgsSource {
    fn search(&self, query: &str, scope: &Scope, limit: usize) -> Result<Vec<HistoryHit>, LoreError> {
        let query: String = query.chars().filter(|ch| !ch.is_control()).take(256).collect();
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let output = std::process::Command::new(&self.hgs)
            .args(["search", "--indexed", "--query", query.trim(), "--limit", &limit.clamp(1, 100).to_string()])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| LoreError::new(format!("hgs search: {error}")))?;
        if !output.status.success() {
            let reason = String::from_utf8_lossy(&output.stderr).lines().next().unwrap_or_default().to_owned();
            return Err(LoreError::new(format!("hgs search failed: {reason}")));
        }
        let page: Value = serde_json::from_slice(&output.stdout).map_err(|error| LoreError::new(format!("hgs search: {error}")))?;
        Ok(hits_of(&page, &query, scope))
    }
}

/// The hits of one content-search page, best first, in the caller's scope.
pub fn hits_of(page: &Value, query: &str, scope: &Scope) -> Vec<HistoryHit> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    page.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(rank, row)| {
            let content = row.get("content").and_then(Value::as_str).or_else(|| row.get("snippet").and_then(Value::as_str)).unwrap_or_default();
            HistoryHit {
                session: text(row, "name"),
                run_id: text(row, "run_id"),
                conversation_id: text(row, "conversation_id"),
                message_id: text(row, "message_id"),
                role: text(row, "role"),
                at: row.get("at").and_then(Value::as_f64).unwrap_or(0.0),
                excerpt: excerpt(content, query),
                score: 1.0 / (rank as f64 + 1.0),
            }
        })
        .filter(|hit| scope.admits(&hit.session))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        hits: Vec<HistoryHit>,
        delay: Duration,
        asked: Mutex<Vec<usize>>,
    }

    impl HistorySource for Fake {
        fn search(&self, _query: &str, _scope: &Scope, limit: usize) -> Result<Vec<HistoryHit>, LoreError> {
            self.asked.lock().unwrap().push(limit);
            std::thread::sleep(self.delay);
            Ok(self.hits.clone())
        }
    }

    fn hit(session: &str, score: f64, excerpt: &str) -> HistoryHit {
        HistoryHit { session: session.into(), message_id: format!("m-{session}"), role: "assistant".into(), at: 1.0, excerpt: excerpt.into(), score, ..HistoryHit::default() }
    }

    /// 11.15 test 11: the provider fake, the 3-hit and 8 KiB bounds, the
    /// scope, the opt-out and the unavailable source.
    #[test]
    fn the_history_channel_is_bounded_scoped_and_redacted() {
        let hits: Vec<HistoryHit> = (0..12).map(|n| hit(&format!("s{n}"), n as f64, "the shader cache was stale")).collect();
        let fake: Arc<dyn HistorySource> = Arc::new(Fake { hits, delay: Duration::ZERO, asked: Mutex::new(Vec::new()) });
        let (found, note) = federate(Some(&fake), "shader cache", &Scope::default());
        assert_eq!(note, None);
        assert_eq!(found.iter().map(|hit| hit.session.as_str()).collect::<Vec<_>>(), ["s9", "s8", "s7"], "the 3 best of the 10 asked");
        // A member sees its own sessions only; an opted-out session nobody.
        let scope = Scope { sessions: Some(vec!["s2".into(), "s3".into()]), excluded: vec!["s3".into()] };
        let (found, _) = federate(Some(&fake), "shader", &scope);
        assert_eq!(found.iter().map(|hit| hit.session.as_str()).collect::<Vec<_>>(), ["s2"]);
        // Every excerpt is redacted before it leaves; an allowed literal too.
        let secret: Arc<dyn HistorySource> = Arc::new(Fake {
            hits: vec![hit("s", 1.0, &format!("token = {}", "Qx7pL2mZ9vR4tY8wK3nB6cD1fG5hJ0sA")), hit("t", 0.5, &"x ".repeat(3000))],
            delay: Duration::ZERO,
            asked: Mutex::new(Vec::new()),
        });
        let (found, _) = federate(Some(&secret), "token", &Scope::default());
        assert_eq!(found[0].excerpt, "token = [redacted: secret-assignment]");
        assert!(found[1].excerpt.chars().count() <= EXCERPT);
        let bytes: usize = found.iter().map(|hit| hit.json().to_string().len()).sum();
        assert!(bytes <= CHANNEL_BYTES);
        // Slow or absent: one note, no hits.
        let slow: Arc<dyn HistorySource> = Arc::new(Fake { hits: vec![hit("s", 1.0, "x")], delay: Duration::from_millis(900), asked: Mutex::new(Vec::new()) });
        let (found, note) = federate(Some(&slow), "x", &Scope::default());
        assert!(found.is_empty());
        assert_eq!(note.as_deref(), Some("history unavailable: the source took longer than 500 ms"));
        assert_eq!(federate(None, "x", &Scope::default()).1.as_deref(), Some("no history source"));
    }

    #[test]
    fn a_content_search_page_becomes_hits_in_scope() {
        let page = json!({"abi": "content-search-query-v1", "results": [
            {"name": "claude/zerus/lead", "run_id": "r1", "conversation_id": "c1", "message_id": "m1", "role": "assistant", "at": 5.0,
             "content": format!("{} the depth buffer resolve was wrong {}", "a ".repeat(400), "b ".repeat(400))},
            {"name": "codex/zerus/dev", "run_id": "r2", "conversation_id": "c2", "message_id": "m2", "role": "user", "at": 6.0, "snippet": "depth buffer"}
        ]});
        let all = hits_of(&page, "depth buffer", &Scope::default());
        assert_eq!(all.len(), 2);
        assert!(all[0].score > all[1].score);
        assert!(all[0].excerpt.contains("depth buffer resolve") && all[0].excerpt.chars().count() <= EXCERPT, "{}", all[0].excerpt);
        let mine = hits_of(&page, "depth", &Scope { sessions: Some(vec!["codex/zerus/dev".into()]), excluded: vec![] });
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].json(), json!({"session": "codex/zerus/dev", "message_id": "m2", "role": "user", "at": 6.0, "excerpt": "depth buffer"}));
    }
}
