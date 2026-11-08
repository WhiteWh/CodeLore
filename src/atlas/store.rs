//! The code map's SQLite file, beside the lore's index in the user cache and as
//! disposable: files by blob id, their pieces, symbols, imports and the file
//! graph, two FTS5 tables and the pieces' vectors. A read first brings it up to
//! date with the working tree; only files whose blob id moved are read again.
use super::chunk::chunk;
use super::graph::{self, Resolver};
use super::lang::{self, Lang};
use super::scan::{self, Entry};
use crate::config::Config;
use crate::embed::Embeddings;
use crate::index::{index_dir_candidates, terms_of, word_re};
use crate::LoreError;
use rusqlite::{params, Connection, OptionalExtension};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const ATLAS_VERSION: i64 = 2;
pub const FILE_NAME: &str = "atlas.sqlite";
const RRF_K: f64 = 60.0;
const DEPTH: usize = 80;
/// The start of a piece an embedding reads.
const EMBED_CHARS: usize = 1500;
/// A search embeds at most this many new pieces itself; more wait for `lore atlas index`.
pub const SEARCH_EMBED_BUDGET: usize = 48;

fn error(error: impl std::fmt::Display) -> LoreError {
    LoreError::new(format!("atlas: {error}"))
}

fn sha1_hex(text: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub struct Atlas {
    pub db: Connection,
    pub path: PathBuf,
    root: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Refresh {
    pub files_read: usize,
    pub files_dropped: usize,
    pub chunks: usize,
    pub edges: usize,
    pub rebuilt: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub chunk: i64,
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub symbols: Vec<String>,
    pub lang: String,
    pub score: f64,
    pub via: Vec<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Definition {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: usize,
}

fn create(db: &Connection) -> Result<(), LoreError> {
    db.execute_batch(&format!(
        r#"CREATE TABLE files(path TEXT PRIMARY KEY, blob TEXT, lang TEXT, lines INTEGER);
        CREATE TABLE chunks(id INTEGER PRIMARY KEY, path TEXT, start INTEGER, stop INTEGER, symbols TEXT, ehash TEXT, body TEXT);
        CREATE INDEX chunks_path ON chunks(path);
        CREATE TABLE symbols(name TEXT, kind TEXT, path TEXT, line INTEGER);
        CREATE INDEX symbols_name ON symbols(name COLLATE NOCASE);
        CREATE INDEX symbols_path ON symbols(path);
        CREATE TABLE imports(path TEXT, spec TEXT, line INTEGER);
        CREATE INDEX imports_path ON imports(path);
        CREATE TABLE edges(src TEXT, dst TEXT, kind TEXT, PRIMARY KEY(src, dst));
        CREATE INDEX edges_dst ON edges(dst);
        CREATE VIRTUAL TABLE code_words USING fts5(chunk UNINDEXED, path, names, body,
            tokenize = "porter unicode61 remove_diacritics 2 tokenchars '_'");
        CREATE VIRTUAL TABLE code_terms USING fts5(chunk UNINDEXED, toks,
            tokenize = "unicode61 remove_diacritics 0 tokenchars '_./:-'");
        CREATE TABLE vectors(model TEXT, hash TEXT, vec BLOB, PRIMARY KEY(model, hash));
        CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
        PRAGMA user_version = {ATLAS_VERSION};"#
    ))
    .map_err(error)
}

fn connect(path: &Path) -> Result<(Connection, bool), LoreError> {
    let open = |path: &Path| -> Result<Connection, LoreError> {
        let db = Connection::open(path).map_err(|e| LoreError::new(format!("{}: {e}", path.display())))?;
        db.busy_timeout(Duration::from_secs(120)).map_err(error)?;
        db.execute_batch("PRAGMA journal_mode=WAL").map_err(error)?;
        Ok(db)
    };
    let db = open(path)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(error)?;
    if version == ATLAS_VERSION {
        return Ok((db, false));
    }
    drop(db);
    remove_file_set(path)?;
    let db = open(path)?;
    create(&db)?;
    Ok((db, true))
}

fn remove_file_set(path: &Path) -> Result<(), LoreError> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        if file.exists() {
            std::fs::remove_file(&file).map_err(|e| LoreError::new(format!("{}: {e}", file.display())))?;
        }
    }
    Ok(())
}

/// Deletes the code map of the repository at `root`, wherever it lives.
pub fn remove(root: &Path) -> Result<(), LoreError> {
    for dir in index_dir_candidates(root) {
        remove_file_set(&dir.join(FILE_NAME))?;
    }
    Ok(())
}

/// The code map of `config`, opened where the lore's index may live.
pub fn open(config: &Config) -> Result<Atlas, LoreError> {
    let mut tried = Vec::new();
    for dir in index_dir_candidates(&config.root) {
        let attempt = std::fs::create_dir_all(&dir).map_err(|e| LoreError::new(format!("{}: {e}", dir.display()))).and_then(|()| connect(&dir.join(FILE_NAME)));
        match attempt {
            Ok((db, _)) => return Ok(Atlas { db, path: dir.join(FILE_NAME), root: config.root.clone() }),
            Err(e) => tried.push(e.to_string()),
        }
    }
    Err(LoreError::new(format!("atlas: no folder the code map can be written to - set LORE_CACHE ({})", tried.join("; "))))
}

impl Atlas {
    /// Opens a map file at `path` directly (tests).
    pub fn open_at(path: &Path, root: &Path) -> Result<Self, LoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(error)?;
        }
        let (db, _) = connect(path)?;
        Ok(Self { db, path: path.to_path_buf(), root: root.to_path_buf() })
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.db.query_row("SELECT value FROM meta WHERE key=?1", [key], |row| row.get(0)).optional().ok().flatten()
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<(), LoreError> {
        self.db.execute("INSERT OR REPLACE INTO meta VALUES(?1, ?2)", params![key, value]).map_err(error)?;
        Ok(())
    }

    /// Brings the map up to date with the working tree.
    pub fn refresh(&mut self, config: &Config) -> Result<Refresh, LoreError> {
        let wanted = scan::scan(config)?;
        let mut known: HashMap<String, String> = HashMap::new();
        {
            let mut statement = self.db.prepare("SELECT path, blob FROM files").map_err(error)?;
            let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
            for row in rows {
                let (path, blob) = row.map_err(error)?;
                known.insert(path, blob);
            }
        }
        let mut info = Refresh { rebuilt: known.is_empty() && !wanted.is_empty(), ..Refresh::default() };
        let gone: Vec<String> = known.keys().filter(|path| !wanted.contains_key(*path)).cloned().collect();
        let changed: Vec<&Entry> = wanted.values().filter(|entry| known.get(&entry.path) != Some(&entry.blob)).collect();
        let settings = format!("{:?}|{:?}|{}", config.atlas.include, config.atlas.exclude, config.atlas.max_kib);
        let graph_missing = self.meta("graph").is_none() || self.meta("settings").as_deref() != Some(settings.as_str());
        if gone.is_empty() && changed.is_empty() && !graph_missing {
            return Ok(info);
        }
        let tx = self.db.transaction().map_err(error)?;
        for path in gone.iter().chain(changed.iter().map(|entry| &entry.path)) {
            drop_file(&tx, path)?;
        }
        info.files_dropped = gone.len();
        for entry in &changed {
            let Some(text) = scan::read_text(&self.root, &entry.path) else {
                // Binary: remembered by its blob so it is not read again.
                tx.execute("INSERT INTO files VALUES(?1, ?2, ?3, 0)", params![entry.path, entry.blob, "binary"]).map_err(error)?;
                continue;
            };
            info.files_read += 1;
            info.chunks += put_file(&tx, entry, &text)?;
        }
        info.edges = rebuild_edges(&tx)?;
        tx.execute("INSERT OR REPLACE INTO meta VALUES('graph', ?1)", [info.edges.to_string()]).map_err(error)?;
        tx.execute("INSERT OR REPLACE INTO meta VALUES('settings', ?1)", [settings]).map_err(error)?;
        tx.execute("INSERT OR REPLACE INTO meta VALUES('refreshed', ?1)", [chrono::Utc::now().to_rfc3339()]).map_err(error)?;
        tx.commit().map_err(error)?;
        Ok(info)
    }

    pub fn counts(&self) -> Result<BTreeMap<&'static str, i64>, LoreError> {
        let mut out = BTreeMap::new();
        for (name, sql) in [
            ("files", "SELECT count(*) FROM files WHERE lang <> 'binary'"),
            ("chunks", "SELECT count(*) FROM chunks"),
            ("symbols", "SELECT count(*) FROM symbols"),
            ("imports", "SELECT count(*) FROM imports"),
            ("edges", "SELECT count(*) FROM edges"),
        ] {
            out.insert(name, self.db.query_row(sql, [], |row| row.get(0)).map_err(error)?);
        }
        Ok(out)
    }

    /// Pieces that have no vector under `model`.
    pub fn missing_vectors(&self, model: &str) -> Result<Vec<(String, String)>, LoreError> {
        let mut statement = self
            .db
            .prepare("SELECT DISTINCT c.ehash, c.path, c.symbols, c.body FROM chunks c LEFT JOIN vectors v ON v.hash = c.ehash AND v.model = ?1 WHERE v.hash IS NULL")
            .map_err(error)?;
        let rows = statement
            .query_map([model], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)))
            .map_err(error)?;
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for row in rows {
            let (hash, path, symbols, body) = row.map_err(error)?;
            if seen.insert(hash.clone()) {
                out.push((hash, embed_text(&path, &symbols, &body)));
            }
        }
        Ok(out)
    }

    /// Embeds up to `budget` pieces without a vector; returns (embedded, still missing).
    pub fn ensure_vectors(&self, embedder: &dyn Embeddings, model: &str, doc_prefix: &str, budget: Option<usize>, progress: &mut dyn FnMut(usize, usize)) -> Result<(usize, usize), LoreError> {
        let dimension = embedder.dimensions().to_string();
        if self.meta("vector_model").as_deref() != Some(&format!("{model}/{dimension}")) {
            self.db.execute("DELETE FROM vectors", []).map_err(error)?;
            self.set_meta("vector_model", &format!("{model}/{dimension}"))?;
        }
        let todo = self.missing_vectors(model)?;
        let take = budget.map_or(todo.len(), |budget| budget.min(todo.len()));
        if budget.is_some() && todo.len() > take {
            return Ok((0, todo.len()));
        }
        let mut done = 0;
        for batch in todo[..take].chunks(64) {
            let texts: Vec<String> = batch.iter().map(|(_, text)| format!("{doc_prefix}{text}")).collect();
            let vectors = embedder.embed(&texts)?;
            let tx = self.db.unchecked_transaction().map_err(error)?;
            for ((hash, _), vector) in batch.iter().zip(vectors) {
                let blob: Vec<u8> = vector.iter().flat_map(|value| value.to_le_bytes()).collect();
                tx.execute("INSERT OR REPLACE INTO vectors VALUES(?1, ?2, ?3)", params![model, hash, blob]).map_err(error)?;
            }
            tx.commit().map_err(error)?;
            done += batch.len();
            progress(done, take);
        }
        Ok((done, todo.len() - done))
    }

    pub fn vector_count(&self, model: &str) -> i64 {
        self.db.query_row("SELECT count(*) FROM vectors WHERE model=?1", [model], |row| row.get(0)).unwrap_or(0)
    }

    fn fts(&self, table: &str, query: &str, filter: &Filter) -> Vec<i64> {
        if query.is_empty() {
            return Vec::new();
        }
        let rank = if table == "code_words" { format!("bm25({table}, 0, 4.0, 8.0, 1.0)") } else { format!("bm25({table})") };
        let sql = format!(
            "SELECT t.chunk FROM {table} t JOIN chunks c ON c.id = t.chunk JOIN files f ON f.path = c.path WHERE {table} MATCH ?1 AND (?2 = '' OR c.path LIKE ?2 || '%') AND (?3 = '' OR f.lang = ?3) ORDER BY {rank} LIMIT ?4"
        );
        let run = || -> rusqlite::Result<Vec<i64>> {
            let mut statement = self.db.prepare(&sql)?;
            let rows = statement.query_map(params![query, filter.path, filter.lang, DEPTH as i64], |row| row.get::<_, i64>(0))?;
            rows.collect()
        };
        run().unwrap_or_default()
    }

    fn meaning(&self, query_vector: &[f32], model: &str, filter: &Filter) -> Result<Vec<(i64, f64)>, LoreError> {
        let mut statement = self
            .db
            .prepare("SELECT c.id, v.vec FROM chunks c JOIN vectors v ON v.hash = c.ehash AND v.model = ?1 JOIN files f ON f.path = c.path WHERE (?2 = '' OR c.path LIKE ?2 || '%') AND (?3 = '' OR f.lang = ?3)")
            .map_err(error)?;
        let rows = statement.query_map(params![model, filter.path, filter.lang], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))).map_err(error)?;
        let mut scored = Vec::new();
        for row in rows {
            let (id, blob) = row.map_err(error)?;
            let vector = blob.chunks_exact(4).map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
            let score: f64 = vector.zip(query_vector).map(|(a, b)| f64::from(a) * f64::from(*b)).sum();
            scored.push((id, score));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(DEPTH);
        Ok(scored)
    }

    /// Ranked pieces for a query: words and whole terms by BM25, the pieces that
    /// define a symbol the query names, and meaning when a vector is given -
    /// fused by reciprocal rank. At most two pieces of one file.
    pub fn search(&self, query: &str, limit: usize, filter: &Filter, meaning: Option<(&[f32], &str)>) -> Result<Vec<Hit>, LoreError> {
        let words = word_query(query);
        let terms = crate::finder::term_query(query);
        let mut lists: Vec<(&str, Vec<i64>)> = vec![("words", self.fts("code_words", &words, filter)), ("terms", self.fts("code_terms", &terms, filter))];
        lists.push(("symbol", self.defining(query, filter)?));
        if let Some((vector, model)) = meaning {
            lists.push(("meaning", self.meaning(vector, model, filter)?.into_iter().map(|(id, _)| id).collect()));
        }
        let mut scores: HashMap<i64, (f64, Vec<String>)> = HashMap::new();
        let mut order: Vec<i64> = Vec::new();
        for (name, ids) in &lists {
            let weight = if *name == "symbol" { 2.0 } else { 1.0 };
            for (rank, id) in ids.iter().enumerate() {
                let entry = scores.entry(*id).or_insert_with(|| {
                    order.push(*id);
                    (0.0, Vec::new())
                });
                entry.0 += weight / (RRF_K + rank as f64 + 1.0);
                if !entry.1.contains(&(*name).to_owned()) {
                    entry.1.push((*name).to_owned());
                }
            }
        }
        order.sort_by(|a, b| scores[b].0.partial_cmp(&scores[a].0).unwrap_or(std::cmp::Ordering::Equal));
        let mut per_file: HashMap<String, usize> = HashMap::new();
        let mut out = Vec::new();
        for id in order {
            let Some(hit) = self.chunk(id)? else { continue };
            let count = per_file.entry(hit.path.clone()).or_default();
            if *count >= 2 {
                continue;
            }
            *count += 1;
            let (score, via) = scores[&id].clone();
            out.push(Hit { score: (score * 100_000.0).round() / 100_000.0, via, ..hit });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// The pieces whose definitions carry a name the query says whole.
    fn defining(&self, query: &str, filter: &Filter) -> Result<Vec<i64>, LoreError> {
        let mut out = Vec::new();
        // Only what reads as an identifier - `open_index`, `ChunkStore`, `a::b` - not
        // an English word that some function happens to be called.
        let looks_like_code = |w: &str| w.len() >= 3 && (w.contains('_') || w.contains("::") || w.contains('.') || w.chars().skip(1).any(char::is_uppercase));
        let names: BTreeSet<String> = word_re().find_iter(query).map(|m| m.as_str().to_owned()).chain(terms_of(query)).filter(|w| looks_like_code(w)).collect();
        for name in names.iter().take(12) {
            let short = name.rsplit(['.', ':']).next().unwrap_or(name).to_owned();
            let mut statement = self
                .db
                .prepare("SELECT c.id FROM symbols s JOIN chunks c ON c.path = s.path AND s.line BETWEEN c.start AND c.stop JOIN files f ON f.path = c.path WHERE s.name = ?1 COLLATE NOCASE AND (?2 = '' OR c.path LIKE ?2 || '%') AND (?3 = '' OR f.lang = ?3) LIMIT 8")
                .map_err(error)?;
            let rows = statement.query_map(params![short, filter.path, filter.lang], |row| row.get::<_, i64>(0)).map_err(error)?;
            for row in rows {
                let id = row.map_err(error)?;
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    pub fn chunk(&self, id: i64) -> Result<Option<Hit>, LoreError> {
        self.db
            .query_row(
                "SELECT c.path, c.start, c.stop, c.symbols, f.lang, c.body FROM chunks c JOIN files f ON f.path = c.path WHERE c.id = ?1",
                [id],
                |row| {
                    Ok(Hit {
                        chunk: id,
                        path: row.get(0)?,
                        start: row.get::<_, i64>(1)? as usize,
                        end: row.get::<_, i64>(2)? as usize,
                        symbols: row.get::<_, String>(3)?.split_whitespace().map(str::to_owned).collect(),
                        lang: row.get(4)?,
                        score: 0.0,
                        via: Vec::new(),
                        text: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(error)
    }

    /// Where `name` is defined (exact, case-insensitive; `a::b` and `a.b` by their last part).
    pub fn definitions(&self, name: &str) -> Result<Vec<Definition>, LoreError> {
        let short = name.rsplit(['.', ':']).next().unwrap_or(name);
        let mut statement = self.db.prepare("SELECT name, kind, path, line FROM symbols WHERE name = ?1 COLLATE NOCASE ORDER BY path, line").map_err(error)?;
        let rows = statement
            .query_map([short], |row| Ok(Definition { name: row.get(0)?, kind: row.get(1)?, path: row.get(2)?, line: row.get::<_, i64>(3)? as usize }))
            .map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    /// The symbols of one file, in line order.
    pub fn symbols_of(&self, path: &str) -> Result<Vec<Definition>, LoreError> {
        let mut statement = self.db.prepare("SELECT name, kind, path, line FROM symbols WHERE path = ?1 ORDER BY line").map_err(error)?;
        let rows = statement
            .query_map([path], |row| Ok(Definition { name: row.get(0)?, kind: row.get(1)?, path: row.get(2)?, line: row.get::<_, i64>(3)? as usize }))
            .map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    /// Files whose text names `name` as a whole word, beside where it is defined.
    pub fn mentions(&self, name: &str, limit: usize) -> Result<Vec<String>, LoreError> {
        let short = name.rsplit(['.', ':']).next().unwrap_or(name);
        let query = format!("\"{}\"", short.replace('"', "\"\""));
        let mut statement = self
            .db
            .prepare("SELECT DISTINCT c.path FROM code_words t JOIN chunks c ON c.id = t.chunk WHERE code_words MATCH ?1 LIMIT ?2")
            .map_err(error)?;
        let rows = statement.query_map(params![format!("body : {query}"), limit as i64], |row| row.get::<_, String>(0)).map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    pub fn has_file(&self, path: &str) -> bool {
        self.db.query_row("SELECT 1 FROM files WHERE path = ?1", [path], |_| Ok(())).optional().ok().flatten().is_some()
    }

    /// Indexed files under a folder (`src` or `src/`).
    pub fn files_under(&self, dir: &str) -> Result<Vec<String>, LoreError> {
        let dir = dir.trim_end_matches('/');
        let mut statement = self.db.prepare("SELECT path FROM files WHERE path LIKE ?1 || '/%' AND lang <> 'binary' ORDER BY path").map_err(error)?;
        let rows = statement.query_map([dir], |row| row.get::<_, String>(0)).map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    pub fn edges(&self) -> Result<Vec<(String, String)>, LoreError> {
        let mut statement = self.db.prepare("SELECT src, dst FROM edges ORDER BY src, dst").map_err(error)?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    /// The edges a cycle can be made of: imports, not a module holding its child.
    pub fn import_edges(&self) -> Result<Vec<(String, String)>, LoreError> {
        let mut statement = self.db.prepare("SELECT src, dst FROM edges WHERE kind = 'imports' ORDER BY src, dst").map_err(error)?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
    }

    /// What a file imports (resolved files) and what imports it.
    pub fn neighbours(&self, path: &str) -> Result<(Vec<String>, Vec<String>), LoreError> {
        let list = |sql: &str| -> Result<Vec<String>, LoreError> {
            let mut statement = self.db.prepare(sql).map_err(error)?;
            let rows = statement.query_map([path], |row| row.get::<_, String>(0)).map_err(error)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(error)
        };
        Ok((list("SELECT dst FROM edges WHERE src = ?1 ORDER BY dst")?, list("SELECT src FROM edges WHERE dst = ?1 ORDER BY src")?))
    }

    /// The imports of a file that name nothing in the repository.
    pub fn external_imports(&self, path: &str) -> Result<Vec<String>, LoreError> {
        let files: BTreeMap<String, Lang> = self.file_langs()?;
        let namespaces = self.namespaces()?;
        let resolver = Resolver::new(&files, &namespaces);
        let Some(lang) = files.get(path).copied() else { return Ok(Vec::new()) };
        let mut statement = self.db.prepare("SELECT spec FROM imports WHERE path = ?1 ORDER BY line").map_err(error)?;
        let rows = statement.query_map([path], |row| row.get::<_, String>(0)).map_err(error)?;
        let mut out = Vec::new();
        for spec in rows {
            let spec = spec.map_err(error)?;
            if resolver.resolve(path, lang, &spec).is_empty() && !out.contains(&spec) {
                out.push(spec);
            }
        }
        Ok(out)
    }

    fn file_langs(&self) -> Result<BTreeMap<String, Lang>, LoreError> {
        file_langs(&self.db)
    }

    fn namespaces(&self) -> Result<BTreeMap<String, Vec<String>>, LoreError> {
        namespaces(&self.db)
    }

    /// The files that depend on `targets`, with their distance.
    pub fn dependents(&self, targets: &[String], depth: usize) -> Result<BTreeMap<String, usize>, LoreError> {
        Ok(graph::dependents(&self.edges()?, targets, depth))
    }
}

/// Filters of a search: a path prefix and a language, '' for any.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub path: String,
    pub lang: String,
}

/// Words of a query, identifiers split too, for the `code_words` table.
fn word_query(text: &str) -> String {
    let mut seen: Vec<String> = Vec::new();
    for found in word_re().find_iter(text) {
        let token = found.as_str().to_lowercase();
        for word in std::iter::once(token.clone()).chain(lang::split_identifier(found.as_str())) {
            if !seen.contains(&word) {
                seen.push(word);
            }
        }
    }
    seen.iter().take(64).map(|token| format!("\"{}\"", token.replace('"', "\"\""))).collect::<Vec<_>>().join(" OR ")
}

/// What a piece's embedding reads: where it is, what it defines, its start.
fn embed_text(path: &str, symbols: &str, body: &str) -> String {
    let head: String = body.chars().take(EMBED_CHARS).collect();
    format!("{path} {symbols}\n{head}")
}

fn drop_file(tx: &Connection, path: &str) -> Result<(), LoreError> {
    for sql in [
        "DELETE FROM code_words WHERE chunk IN (SELECT id FROM chunks WHERE path = ?1)",
        "DELETE FROM code_terms WHERE chunk IN (SELECT id FROM chunks WHERE path = ?1)",
        "DELETE FROM chunks WHERE path = ?1",
        "DELETE FROM symbols WHERE path = ?1",
        "DELETE FROM imports WHERE path = ?1",
        "DELETE FROM files WHERE path = ?1",
    ] {
        tx.execute(sql, [path]).map_err(error)?;
    }
    Ok(())
}

fn put_file(tx: &Connection, entry: &Entry, text: &str) -> Result<usize, LoreError> {
    let lines: Vec<&str> = text.lines().collect();
    tx.execute("INSERT INTO files VALUES(?1, ?2, ?3, ?4)", params![entry.path, entry.blob, entry.lang.name(), lines.len() as i64]).map_err(error)?;
    let symbols = lang::symbols(entry.lang, text);
    for symbol in &symbols {
        tx.execute("INSERT INTO symbols VALUES(?1, ?2, ?3, ?4)", params![symbol.name, symbol.kind, entry.path, symbol.line as i64]).map_err(error)?;
    }
    for import in lang::imports(entry.lang, text) {
        tx.execute("INSERT INTO imports VALUES(?1, ?2, ?3)", params![entry.path, import.spec, import.line as i64]).map_err(error)?;
    }
    let pieces = chunk(text, &symbols);
    for piece in &pieces {
        let body = lines[piece.start - 1..piece.end].join("\n");
        let names = piece.symbols.join(" ");
        let ehash = sha1_hex(&embed_text(&entry.path, &names, &body));
        tx.execute("INSERT INTO chunks(path, start, stop, symbols, ehash, body) VALUES(?1, ?2, ?3, ?4, ?5, ?6)", params![entry.path, piece.start as i64, piece.end as i64, names, ehash, body])
            .map_err(error)?;
        let id = tx.last_insert_rowid();
        let split: Vec<String> = piece.symbols.iter().flat_map(|name| lang::split_identifier(name)).collect();
        let path_words = lang::split_identifier(&entry.path).join(" ");
        tx.execute("INSERT INTO code_words VALUES(?1, ?2, ?3, ?4)", params![id, format!("{} {path_words}", entry.path), format!("{names} {}", split.join(" ")), body]).map_err(error)?;
        let mut toks = terms_of(&body);
        toks.push(entry.path.clone());
        toks.extend(piece.symbols.iter().cloned());
        tx.execute("INSERT INTO code_terms VALUES(?1, ?2)", params![id, toks.join(" ")]).map_err(error)?;
    }
    Ok(pieces.len())
}

fn file_langs(db: &Connection) -> Result<BTreeMap<String, Lang>, LoreError> {
    let mut statement = db.prepare("SELECT path, lang FROM files WHERE lang <> 'binary'").map_err(error)?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (path, lang) = row.map_err(error)?;
        out.insert(path, Lang::from_name(&lang).unwrap_or(Lang::Text));
    }
    Ok(out)
}

fn namespaces(db: &Connection) -> Result<BTreeMap<String, Vec<String>>, LoreError> {
    let mut statement = db.prepare("SELECT s.name, s.path FROM symbols s JOIN files f ON f.path = s.path WHERE s.kind = 'namespace' AND f.lang = 'csharp'").map_err(error)?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        let (name, path) = row.map_err(error)?;
        out.entry(name).or_default().push(path);
    }
    Ok(out)
}

/// Every import resolved again against the current file set: a new file can
/// make an old import resolve, so the graph is rebuilt whole (lookups only).
fn rebuild_edges(tx: &Connection) -> Result<usize, LoreError> {
    let files = file_langs(tx)?;
    let namespaces = namespaces(tx)?;
    let resolver = Resolver::new(&files, &namespaces);
    tx.execute("DELETE FROM edges", []).map_err(error)?;
    let mut statement = tx.prepare("SELECT path, spec FROM imports").map_err(error)?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
    // `contains`: a Rust `mod x;` - the parent holds the child, which is not a
    // dependency a cycle is made of; `imports`: everything else.
    let mut edges: BTreeMap<(String, String), &'static str> = BTreeMap::new();
    for row in rows {
        let (path, spec) = row.map_err(error)?;
        let Some(lang) = files.get(&path).copied() else { continue };
        let kind = if spec.starts_with("mod:") { "contains" } else { "imports" };
        for target in resolver.resolve(&path, lang, &spec) {
            let entry = edges.entry((path.clone(), target)).or_insert(kind);
            if kind == "imports" {
                *entry = kind;
            }
        }
    }
    drop(statement);
    for ((src, dst), kind) in &edges {
        tx.execute("INSERT OR IGNORE INTO edges VALUES(?1, ?2, ?3)", params![src, dst, kind]).map_err(error)?;
    }
    Ok(edges.len())
}
