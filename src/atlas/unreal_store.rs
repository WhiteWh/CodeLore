//! Engine cache lifecycle and versioned vector spaces, using the Atlas tables.
use std::fs::{self, File};

use rusqlite::{OpenFlags, TransactionBehavior};
use serde_json::json;

use super::super::unreal::{digest, Stamp, Unreal};
use super::*;
use crate::embed::EmbeddingModel;

#[derive(Default)]
pub(in crate::atlas) struct EngineRefresh {
    pub cache_hit: bool,
    pub files_seen: usize,
    pub files_hashed: usize,
    pub files_read: usize,
    pub files_dropped: usize,
    pub skipped: BTreeMap<&'static str, usize>,
}

pub(in crate::atlas) struct EngineWriter {
    pub atlas: Atlas,
    _lock: File,
}

impl Unreal {
    pub fn open_ready(&self) -> Result<Option<Atlas>, LoreError> {
        let snapshot = self.read_ready()?;
        if snapshot.as_ref().is_some_and(|atlas| !atlas.has_projections) {
            // Release the read transaction before taking the legacy-cache
            // upgrade path. It uses only SQLite and never scans Engine.
            drop(snapshot);
            self.upgrade_ready()?;
            return self.read_ready();
        }
        Ok(snapshot)
    }

    fn read_ready(&self) -> Result<Option<Atlas>, LoreError> {
        let exists = self.path.try_exists().map_err(error)?;
        if !exists {
            return Ok(None);
        }
        let db = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(error)?;
        db.busy_timeout(Duration::from_millis(300)).map_err(error)?;
        // Keep candidate ids, file contents and metadata from one committed revision.
        db.execute_batch("BEGIN").map_err(error)?;
        let version = version(&db)?;
        if version == 0 {
            return Ok(None);
        }
        check_identity(&db, self)?;
        let has_projections = derived::ready(&db)?;
        let atlas = Atlas { db, path: self.path.clone(), root: self.root.clone(), _writer: None, has_projections };
        Ok(atlas.meta("revision").is_some().then_some(atlas))
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_file_name("writer.lock")
    }

    fn upgrade_ready(&self) -> Result<(), LoreError> {
        let fallback = |e: &std::io::Error| is_locked(e) || matches!(e.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem);
        let lock = match OpenOptions::new().create(true).truncate(false).write(true).open(self.lock_path()) {
            Ok(lock) => lock,
            Err(e) if fallback(&e) => return Ok(()),
            Err(e) => return Err(error(e)),
        };
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => {}
            Err(e) if fallback(&e) => return Ok(()),
            Err(e) => return Err(error(e)),
        }
        let fallback = |e: &rusqlite::Error| matches!(e.sqlite_error_code(), Some(
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked |
            rusqlite::ErrorCode::ReadOnly | rusqlite::ErrorCode::CannotOpen
        ));
        let mut db = match Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_WRITE) {
            Ok(db) => db,
            Err(e) if fallback(&e) => return Ok(()),
            Err(e) => return Err(error(e)),
        };
        db.busy_timeout(Duration::ZERO).map_err(error)?;
        let tx = match db.transaction_with_behavior(TransactionBehavior::Immediate) {
            Ok(tx) => tx,
            Err(e) if fallback(&e) => return Ok(()),
            Err(e) => return Err(error(e)),
        };
        check_identity(&tx, self)?;
        derived::install(&tx)?;
        tx.commit().map_err(error)
    }

    pub fn writer(&self) -> Result<EngineWriter, LoreError> {
        let directory = self.path.parent().expect("engine database directory");
        fs::create_dir_all(directory).map_err(error)?;
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(self.lock_path()).map_err(error)?;
        fs2::FileExt::lock_exclusive(&lock).map_err(error)?;
        let db = Connection::open(&self.path).map_err(error)?;
        db.busy_timeout(Duration::from_secs(120)).map_err(error)?;
        // The engine FTS tables touch many pages in a single atomic refresh.
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA cache_size=-65536").map_err(error)?;
        Ok(EngineWriter { atlas: Atlas { db, path: self.path.clone(), root: self.root.clone(), _writer: None, has_projections: false }, _lock: lock })
    }
}

fn version(db: &Connection) -> Result<i64, LoreError> {
    db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(error)
}

fn check_identity(db: &Connection, source: &Unreal) -> Result<(), LoreError> {
    if version(db)? != ATLAS_VERSION {
        return Err(error("incompatible Unreal cache schema"));
    }
    let identity: Option<String> = db.query_row("SELECT value FROM meta WHERE key = 'unreal_identity'", [], |row| row.get(0)).optional().map_err(error)?;
    if identity.as_deref() != Some(&source.identity()) {
        return Err(error("incompatible Unreal cache identity"));
    }
    Ok(())
}

impl EngineWriter {
    pub fn refresh(&mut self, source: &Unreal, rebuild: bool) -> Result<EngineRefresh, LoreError> {
        source.verify_build()?;
        let tx = self.atlas.db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(error)?;
        if version(&tx)? == 0 {
            create(&tx)?;
            tx.execute_batch(
                "CREATE TABLE unreal_files(path TEXT PRIMARY KEY, bytes INTEGER NOT NULL, modified TEXT NOT NULL, blob TEXT NOT NULL);"
            ).map_err(error)?;
            tx.execute("INSERT INTO meta VALUES ('unreal_identity', ?1)", [source.identity()]).map_err(error)?;
        } else {
            check_identity(&tx, source)?;
            derived::install(&tx)?;
        }
        let old_revision: Option<i64> = tx.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'revision'", [], |row| row.get(0)).optional().map_err(error)?;
        let mut known: BTreeMap<String, (Stamp, String)> = {
            let mut query = tx.prepare("SELECT path, bytes, modified, blob FROM unreal_files").map_err(error)?;
            let rows = query.query_map([], |row| Ok((row.get(0)?, (Stamp { bytes: row.get(1)?, modified: row.get(2)? }, row.get(3)?)))).map_err(error)?;
            rows.collect::<Result<_, _>>().map_err(error)?
        };
        let scan = source.scan()?;
        let mut refresh = EngineRefresh { cache_hit: old_revision.is_some(), files_seen: scan.files.len(), skipped: scan.skipped, ..EngineRefresh::default() };
        for (path, file) in scan.files {
            let previous = known.remove(&path);
            if !rebuild && previous.as_ref().is_some_and(|(stamp, _)| *stamp == file.stamp) {
                continue;
            }
            let bytes = file.read(&source.root.join(&path))?;
            refresh.files_hashed += 1;
            let blob = scan::blob_id(&bytes);
            if rebuild || previous.as_ref().is_none_or(|(_, known_blob)| *known_blob != blob) {
                if previous.is_some() {
                    drop_file(&tx, &path)?;
                }
                if let Some(text) = scan::text_from_bytes(&bytes) {
                    put_file(&tx, &Entry { path: path.clone(), blob: blob.clone(), lang: file.lang }, &text)?;
                } else {
                    tx.execute("INSERT INTO files VALUES(?1, ?2, 'binary', 0)", [&path, &blob]).map_err(error)?;
                }
                refresh.files_read += 1;
            }
            tx.execute("INSERT OR REPLACE INTO unreal_files VALUES (?1, ?2, ?3, ?4)", params![path, file.stamp.bytes, file.stamp.modified, blob]).map_err(error)?;
        }
        for path in known.keys() {
            drop_file(&tx, path)?;
            tx.execute("DELETE FROM unreal_files WHERE path = ?1", [path]).map_err(error)?;
        }
        refresh.files_dropped = known.len();
        let binary: usize = tx.query_row("SELECT count(*) FROM files WHERE lang = 'binary'", [], |row| row.get(0)).map_err(error)?;
        if binary > 0 {
            refresh.skipped.insert("binary", binary);
        }
        source.verify_build()?;
        if old_revision.is_none() || rebuild || refresh.files_read + refresh.files_dropped > 0 {
            tx.execute("INSERT OR REPLACE INTO meta VALUES ('revision', ?1)", [old_revision.unwrap_or(0) + 1]).map_err(error)?;
        }
        tx.execute("INSERT OR REPLACE INTO meta VALUES ('refreshed', ?1)", [chrono::Utc::now().to_rfc3339()]).map_err(error)?;
        tx.commit().map_err(error)?;
        self.atlas.has_projections = true;
        Ok(refresh)
    }
}

pub(in crate::atlas) struct VectorSpace {
    pub model: EmbeddingModel,
    pub key: String,
    pub dimension: usize,
    prefix: &'static str,
}

impl VectorSpace {
    pub fn new(model: EmbeddingModel) -> Self {
        let limit = match model {
            EmbeddingModel::Nomic => 8192,
            EmbeddingModel::BgeM3 | EmbeddingModel::E5Base => 512,
        };
        let dimension = model.dimensions();
        let prefix = model.document_prefix();
        let identity = json!([
            "atlas-embedding-v1", model.repository(), model.revision(),
            model.candle_files().iter().map(|file| file.sha256).collect::<Vec<_>>(),
            dimension, crate::embed::token_cap(limit), prefix, model.query_prefix(), EMBED_CHARS
        ]);
        Self { model, key: format!("space:{}", digest(identity.to_string())), dimension, prefix }
    }

    pub fn coverage(&self, atlas: &Atlas) -> Result<usize, LoreError> {
        self.coverage_for(atlas, &self.key)
    }

    pub(in crate::atlas) fn coverage_for(&self, atlas: &Atlas, key: &str) -> Result<usize, LoreError> {
        let sql = if atlas.has_projections {
            "SELECT coalesce((SELECT covered FROM atlas_vector_counts WHERE model = ?1 AND bytes = ?2), 0)"
        } else {
            "SELECT count(*) FROM chunks c JOIN vectors v ON v.model = ?1 AND c.ehash = v.hash
             WHERE typeof(v.vec) = 'blob' AND length(v.vec) = ?2"
        };
        atlas.db.query_row(sql, params![key, self.dimension * 4], |row| row.get(0)).map_err(error)
    }

    pub fn fill(&self, atlas: &Atlas, embedder: &dyn Embeddings, progress: impl FnMut(usize)) -> Result<VectorWork, LoreError> {
        derived::install_ann_clock(&atlas.db)?;
        self.fill_inner(atlas, embedder, progress, true, |_| {})
    }

    pub(in crate::atlas) fn fill_checked(
        &self, atlas: &Atlas, embedder: &dyn Embeddings, progress: impl FnMut(usize),
        check: impl FnOnce(&BTreeSet<String>) -> Result<(), LoreError>,
    ) -> Result<VectorWork, LoreError> {
        atlas.vector_batch(|_| {
            let mut paths = BTreeSet::new();
            // Results are shared by readers of this corpus after the check
            // and commit. Do not write unchecked vectors to the global cache.
            let work = self.fill_inner(atlas, embedder, progress, false, |path| { paths.insert(path.to_owned()); })?;
            check(&paths)?;
            Ok(work)
        })
    }

    fn fill_inner(
        &self, atlas: &Atlas, embedder: &dyn Embeddings, mut progress: impl FnMut(usize),
        publish_shared: bool, mut on_missing: impl FnMut(&str),
    ) -> Result<VectorWork, LoreError> {
        if self.dimension != embedder.dimensions() {
            return Err(error("embedding dimension does not match the Atlas vector space"));
        }
        let shared = SharedVectors::open(&self.key);
        let mut work = VectorWork::default();
        let mut cursor = 0_i64;
        loop {
            // Walk by chunk id to keep memory and statement cost bounded for full installations.
            let mut query = atlas.db.prepare(
                "SELECT c.id, c.ehash, c.path, c.symbols, c.body, v.vec
                 FROM chunks c LEFT JOIN vectors v ON v.model = ?1 AND c.ehash = v.hash
                 WHERE c.id > ?2 ORDER BY c.id LIMIT 256"
            ).map_err(error)?;
            let mut rows = query.query(params![self.key, cursor]).map_err(error)?;
            let mut missing = BTreeMap::new();
            let mut last = cursor;
            while let Some(row) = rows.next().map_err(error)? {
                last = row.get(0).map_err(error)?;
                if row.get_ref(5).map_err(error)?.as_blob().is_ok_and(|blob| valid_vector(blob, self.dimension)) {
                    work.hits += 1;
                    continue;
                }
                let hash: String = row.get(1).map_err(error)?;
                let path: String = row.get(2).map_err(error)?;
                on_missing(&path);
                let symbols: String = row.get(3).map_err(error)?;
                let body: String = row.get(4).map_err(error)?;
                missing.entry(hash).or_insert_with(|| embed_text(&path, &symbols, &body));
            }
            drop(rows);
            drop(query);
            if last == cursor {
                break;
            }
            cursor = last;
            if missing.is_empty() {
                continue;
            }
            let mut missing: Vec<_> = missing.into_iter().collect();
            let mut found = shared.as_ref().map_or_else(HashMap::new, |shared| shared.take(&missing));
            found.retain(|_, blob| valid_vector(blob, self.dimension));
            missing.retain(|(hash, _)| !found.contains_key(hash));
            work.hits += found.len();
            if !found.is_empty() {
                atlas.vector_batch(|db| {
                    for (hash, blob) in found {
                        db.execute("INSERT OR REPLACE INTO vectors VALUES (?1, ?2, ?3)", params![self.key, hash, blob]).map_err(error)?;
                    }
                    Ok(())
                })?;
            }
            for batch in missing.chunks(64) {
                let texts: Vec<_> = batch.iter().map(|(_, text)| format!("{}{text}", self.prefix)).collect();
                let vectors = embedder.embed(&texts)?;
                if vectors.len() != batch.len() || vectors.iter().any(|vector| vector.len() != self.dimension || vector.iter().any(|value| !value.is_finite())) {
                    return Err(error("embedder returned an invalid Atlas vector batch"));
                }
                let made = atlas.vector_batch(|db| {
                    let mut made = Vec::with_capacity(batch.len());
                    for ((hash, _), vector) in batch.iter().zip(vectors) {
                        let blob: Vec<u8> = vector.iter().flat_map(|value| value.to_le_bytes()).collect();
                        db.execute("INSERT OR REPLACE INTO vectors VALUES (?1, ?2, ?3)", params![self.key, hash, blob]).map_err(error)?;
                        made.push((hash.as_str(), blob));
                    }
                    Ok(made)
                })?;
                if let Some(shared) = shared.as_ref().filter(|_| publish_shared) {
                    shared.put(&made);
                }
                work.generated += batch.len();
                progress(work.generated);
            }
        }
        Ok(work)
    }
}

#[derive(Default)]
pub(in crate::atlas) struct VectorWork {
    pub hits: usize,
    pub generated: usize,
}

fn valid_vector(blob: &[u8], dimension: usize) -> bool {
    blob.len() == dimension * 4 && blob.chunks_exact(4).all(|bytes| {
        f32::from_le_bytes(bytes.try_into().expect("four-byte float")).is_finite()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::Fixed;

    fn engine(root: &Path) -> Unreal {
        let source = root.join("Engine/Source");
        let build = root.join("Engine/Build");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("Build.version"), r#"{"MajorVersion":5,"MinorVersion":8,"PatchVersion":4,"Changelist":0,"BranchName":""}"#).unwrap();
        fs::write(source.join("Atoms.cpp"), "void UAtomic::Reaction() {}\n").unwrap();
        let mut engine = Unreal::resolve(root).unwrap();
        engine.path = root.join("cache/atlas.sqlite");
        engine
    }

    #[test]
    fn unreal_atlas_reuses_vectors_and_omits_incompatible_and_corrupt_rows() {
        let temp = tempfile::tempdir().unwrap();
        let engine = engine(temp.path());
        let space = VectorSpace {
            model: EmbeddingModel::E5Base,
            key: format!("test:{}", digest(temp.path().as_os_str().as_encoded_bytes())),
            dimension: 2,
            prefix: "",
        };
        let filter = Filter { path: String::new(), lang: String::new() };
        let mut writer = engine.writer().unwrap();
        writer.refresh(&engine, false).unwrap();
        let project = Atlas::open_at(&temp.path().join("project.sqlite"), temp.path()).unwrap();
        let entry = Entry { path: "Engine/Source/Atoms.cpp".to_owned(), blob: "blob".to_owned(), lang: Lang::Cpp };
        put_file(&project.db, &entry, "void UAtomic::Reaction() {}\n").unwrap();

        let model = Fixed(vec![vec![1.0, 0.0]]);
        let work = space.fill(&writer.atlas, &model, |_| {}).unwrap();
        assert_eq!(work.generated, 1);
        let work = space.fill(&project, &model, |_| {}).unwrap();
        assert_eq!((work.generated, work.hits), (0, 1));
        drop(writer);

        let atlas = engine.open_ready().unwrap().unwrap();
        let candidates = |atlas: &Atlas, query: &str| {
            let ids = atlas.exact_meaning(&[1.0, 0.0], &space.key, &filter).unwrap();
            let mut timings = SearchTimings::default();
            let filter = atlas.prepare_filter(&filter, &mut timings);
            atlas.candidates(query, &filter, ids, &mut timings).unwrap()
        };
        let meaning = |atlas: &Atlas| candidates(atlas, "nonmatching");
        assert_eq!(meaning(&atlas)[0].hit.via, ["meaning"]);
        assert_eq!(meaning(&project)[0].hit.via, ["meaning"]);
        drop(atlas);

        let mut writer = engine.writer().unwrap();
        let refresh = writer.refresh(&engine, false).unwrap();
        assert_eq!((refresh.files_hashed, refresh.files_read), (0, 0));
        let work = space.fill(&writer.atlas, &model, |_| {}).unwrap();
        assert_eq!((work.generated, work.hits), (0, 1));
        for vector in [vec![0_u8; 4], [f32::NAN.to_le_bytes(), 0_f32.to_le_bytes()].concat()] {
            writer.atlas.db.execute("UPDATE vectors SET vec = ?1 WHERE model = ?2", params![vector, space.key]).unwrap();
            assert!(meaning(&writer.atlas).is_empty());
            let text = candidates(&writer.atlas, "UAtomic::Reaction");
            assert!(text[0].exact);
            assert!(!text[0].hit.via.contains(&"meaning".to_owned()));
        }
        writer.atlas.db.execute("UPDATE vectors SET model = 'legacy', vec = ?1", [vec![0_u8; 8]]).unwrap();
        assert!(meaning(&writer.atlas).is_empty());
    }

    #[test]
    fn unreal_atlas_serializes_concurrent_writers() {
        let temp = tempfile::tempdir().unwrap();
        let engine = engine(temp.path());
        let updates = std::thread::scope(|scope| {
            let run = || engine.writer().unwrap().refresh(&engine, false).unwrap();
            let first = scope.spawn(run);
            let second = scope.spawn(run);
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(updates.iter().filter(|refresh| refresh.cache_hit).count(), 1);
        assert_eq!(updates.iter().map(|refresh| refresh.files_read).sum::<usize>(), 1);
        let snapshot = engine.open_ready().unwrap().unwrap();
        assert_eq!(snapshot.meta("revision").as_deref(), Some("1"));
    }

    #[test]
    fn unreal_atlas_keeps_the_ready_snapshot_and_can_recheck_unchanged_stamps() {
        let temp = tempfile::tempdir().unwrap();
        let engine = engine(temp.path());
        let mut writer = engine.writer().unwrap();
        writer.refresh(&engine, false).unwrap();
        let path = "Engine/Source/Atoms.cpp";
        {
            let pending = writer.atlas.db.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
            drop_file(&pending, path).unwrap();
            let concurrent = engine.open_ready().unwrap().unwrap();
            assert_eq!(concurrent.symbols_of(path).unwrap()[0].name, "UAtomic::Reaction");
        }
        let modified = fs::metadata(engine.root.join(path)).unwrap().modified().unwrap();
        fs::write(engine.root.join(path), "void UAtomic::Revision() {}\n").unwrap();
        fs::OpenOptions::new().write(true).open(engine.root.join(path)).unwrap().set_modified(modified).unwrap();
        assert_eq!(writer.refresh(&engine, false).unwrap().files_hashed, 0);
        assert_eq!(writer.atlas.symbols_of(path).unwrap()[0].name, "UAtomic::Reaction");
        assert_eq!(writer.refresh(&engine, true).unwrap().files_hashed, 1);
        assert_eq!(writer.atlas.symbols_of(path).unwrap()[0].name, "UAtomic::Revision");
    }
}
