//! Shared Atlas WAL databases for recorded files and configured trees.
use super::super::dependencies::{implementation, named, Files, Source};
use super::super::unreal::is_link;
use super::*;
use super::unreal_store::{VectorSpace, VectorWork};
use crate::embed::Embeddings;
use rusqlite::TransactionBehavior;
use std::cell::Cell;

const MAX_FILE: u64 = 512 * 1024;
const STOP_CHECK_EVERY: usize = 64;

#[derive(Default)]
pub(in crate::atlas) struct Publication {
    pub indexed: usize,
    pub implementations: usize,
    pub missing: usize,
    pub selected: usize,
}

fn valid(source: &Source) -> bool {
    source.canonical_path.is_absolute() && source.index_key.len() == 64
        && source.index_key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(in crate::atlas) fn open_ready(source: &Source) -> Result<Option<Atlas>, LoreError> {
    if !valid(source) {
        return Err(error("invalid published dependency identity"));
    }
    let path = source.directory().join(FILE_NAME);
    if !path.try_exists().map_err(error)? {
        return Ok(None);
    }
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(error)?;
    db.busy_timeout(Duration::ZERO).map_err(error)?;
    db.execute_batch("BEGIN").map_err(error)?;
    let atlas = Atlas { db, path, root: source.canonical_path.clone(), _writer: None, has_projections: true };
    Ok(ready(&atlas, source).then_some(atlas))
}

fn ready(atlas: &Atlas, source: &Source) -> bool {
    atlas.meta("dependency_identity").as_deref() == Some(source.index_key.as_str())
        && atlas.meta("revision").is_some()
}

pub(in crate::atlas) fn fill_named_vectors(
    source: &Source, max_bytes: u64, space: &VectorSpace, model: &dyn Embeddings, progress: impl FnMut(usize),
) -> Result<(Atlas, VectorWork), LoreError> {
    let dir = source.directory();
    let path = dir.join(FILE_NAME);
    let unindexed = || error("no ready text snapshot. Start the watcher with lore atlas watch start and wait for the source to be ready");
    if !path.try_exists().map_err(error)? {
        return Err(unindexed());
    }
    let lock = lock_file(&dir, "writer.lock")?;
    match fs2::FileExt::try_lock_exclusive(&lock) {
        Ok(()) => {}
        Err(e) if is_locked(&e) => return Err(error("the source writer is busy. Retry after the current update")),
        Err(e) => return Err(error(e)),
    }
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(error)?;
    db.busy_timeout(Duration::ZERO).map_err(error)?;
    let atlas = Atlas { db, path, root: source.canonical_path.clone(), _writer: Some(lock), has_projections: true };
    if !ready(&atlas, source) {
        return Err(unindexed());
    }
    let version: i64 = atlas.db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(error)?;
    if version != ATLAS_VERSION {
        return Err(error("incompatible dependency cache"));
    }
    derived::install_ann_clock(&atlas.db)?;
    let work = space.fill_checked(&atlas, model, progress, |paths| {
        let mut blobs = atlas.db.prepare("SELECT blob FROM dependency_files WHERE path = ?1").map_err(error)?;
        for path in paths {
            let expected: String = blobs.query_row([path], |row| row.get(0)).map_err(error)?;
            let absolute = atlas.root.join(path);
            let matches_path = absolute.canonicalize().ok().is_some_and(|resolved| crate::config::simplify(&resolved) == absolute);
            if !matches_path || !scan::read_source(&atlas.root, path, max_bytes).is_ok_and(|bytes| scan::blob_id(&bytes) == expected) {
                return Err(error(format!("{} changed or was removed during vector indexing. Wait for the watcher and retry", absolute.display())));
            }
        }
        Ok(())
    })?;
    Ok((atlas, work))
}

pub(in crate::atlas) fn refresh(source: &Source, files: &Files, active: &impl Fn() -> bool) -> Result<Option<Publication>, LoreError> {
    if !active() {
        return Ok(None);
    }
    let checks = Cell::new(0);
    let checkpoint = || {
        let next = (checks.get() + 1) % STOP_CHECK_EVERY;
        checks.set(next);
        next != 0 || active()
    };
    let dir = source.directory();
    fs::create_dir_all(&dir).map_err(error)?;
    let lock = lock_file(&dir, "writer.lock")?;
    match fs2::FileExt::try_lock_exclusive(&lock) {
        Ok(()) => {}
        Err(e) if is_locked(&e) => return Ok(None),
        Err(e) => return Err(error(e)),
    }
    let db = Connection::open(dir.join(FILE_NAME)).map_err(error)?;
    db.busy_timeout(Duration::ZERO).map_err(error)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(error)?;
    let tree = match files { Files::Tree(profile) => Some(profile), Files::Listed(_) => None };
    let mut old = BTreeMap::new();
    if version == 0 {
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA cache_size=-32768").map_err(error)?;
    } else {
        let identity: String = db.query_row("SELECT value FROM meta WHERE key='dependency_identity'", [], |row| row.get(0)).map_err(error)?;
        if version != ATLAS_VERSION || identity != source.index_key {
            return Err(error("incompatible dependency cache"));
        }
        let mut stmt = db.prepare("SELECT path, stamp, blob FROM dependency_files").map_err(error)?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, (row.get::<_, String>(1)?, row.get::<_, String>(2)?)))).map_err(error)?;
        old = rows.collect::<Result<_, _>>().map_err(error)?;
    }
    let tree_files;
    let files = match files {
        Files::Listed(files) => files.as_ref(),
        Files::Tree(profile) => {
            let Some(selected) = named::files(&source.canonical_path, profile, &checkpoint)? else { return Ok(None) };
            tree_files = selected;
            &tree_files
        }
    };
    let max_bytes = tree.map_or(MAX_FILE, |profile| profile.max_kib * 1024);
    let mut publication = Publication::default();
    let mut wanted = BTreeMap::new();
    for file in files {
        if !checkpoint() {
            return Ok(None);
        }
        let Some(relative) = file.strip_prefix(&source.canonical_path).ok().and_then(Path::to_str) else { continue };
        let path = relative.replace('\\', "/");
        let meta = match fs::symlink_metadata(file) {
            Ok(meta) if meta.is_file() && !is_link(&meta) => meta,
            _ => { publication.missing += 1; continue }
        };
        if meta.len() == 0 || meta.len() > max_bytes {
            continue;
        }
        let modified = meta.modified().map_err(error)?;
        // A replacement preserving mtime is unusual, but its creation
        // stamp is also available on supported file systems.
        let created = meta.created().unwrap_or(modified);
        let stamp = format!("{modified:?}|{created:?}|{}", meta.len());
        wanted.insert(path, stamp);
    }
    publication.selected = wanted.len();
    let changed = wanted.iter().any(|(path, stamp)| old.get(path).is_none_or(|(previous, _)| stamp != previous));
    if version != 0 && !changed && wanted.len() == old.len() {
        if !active() {
            return Ok(None);
        }
        if tree.is_some() {
            record_tree_pass(&db, publication.selected)?;
        } else {
            record_listed_pass(&db, publication.missing)?;
        }
        return info(&db, publication).map(Some);
    }
    let tx = rusqlite::Transaction::new_unchecked(&db, TransactionBehavior::Immediate).map_err(error)?;
    if version == 0 {
        create(&tx)?;
        derived::install_ann_clock(&tx)?;
        tx.execute_batch("CREATE TABLE dependency_files(path TEXT PRIMARY KEY, stamp TEXT NOT NULL, blob TEXT NOT NULL)").map_err(error)?;
        tx.execute("INSERT INTO meta VALUES('dependency_identity', ?1)", [&source.index_key]).map_err(error)?;
    }
    let mut content_changed = version == 0;
    for (path, stamp) in wanted {
        if !checkpoint() {
            return Ok(None);
        }
        let previous = old.remove(&path);
        if previous.as_ref().is_some_and(|(old_stamp, _)| *old_stamp == stamp) {
            continue;
        }
        let absolute = source.canonical_path.join(&path);
        if absolute.canonicalize().ok().map(|path| crate::config::simplify(&path)).as_ref() != Some(&absolute) {
            return Err(error(format!("dependency path changed during indexing: {}", absolute.display())));
        }
        let bytes = scan::read_source(&source.canonical_path, &path, max_bytes).map_err(error)?;
        let blob = scan::blob_id(&bytes);
        if previous.as_ref().is_none_or(|(_, previous_blob)| *previous_blob != blob) {
            if previous.is_some() {
                drop_file(&tx, &path)?;
            }
            let lang = match tree {
                Some(profile) => scan::language(&path, &profile.include).unwrap_or(Lang::Text),
                None => lang::detect(&path).unwrap_or(Lang::Cpp),
            };
            if let Some(text) = scan::text_from_bytes(&bytes) {
                put_file(&tx, &Entry { path: path.clone(), blob: blob.clone(), lang }, &text)?;
            } else {
                tx.execute("INSERT INTO files VALUES(?1, ?2, 'binary', 0)", [&path, &blob]).map_err(error)?;
            }
            content_changed = true;
        }
        tx.execute("INSERT OR REPLACE INTO dependency_files VALUES(?1, ?2, ?3)", params![path, stamp, blob]).map_err(error)?;
    }
    for path in old.keys() {
        if !checkpoint() {
            return Ok(None);
        }
        drop_file(&tx, path)?;
        tx.execute("DELETE FROM dependency_files WHERE path=?1", [path]).map_err(error)?;
        content_changed = true;
    }
    if !active() {
        return Ok(None);
    }
    if tree.is_some() {
        record_tree_pass(&tx, publication.selected)?;
    } else {
        record_listed_pass(&tx, publication.missing)?;
    }
    if content_changed {
        tx.execute_batch("INSERT OR REPLACE INTO meta VALUES('revision',
            1 + coalesce((SELECT CAST(value AS INTEGER) FROM meta WHERE key='revision'), 0))").map_err(error)?;
        tx.execute("INSERT OR REPLACE INTO meta VALUES('refreshed', ?1)", [chrono::Utc::now().to_rfc3339()]).map_err(error)?;
        let implementations = count_implementations(&tx)?;
        tx.execute("INSERT OR REPLACE INTO meta VALUES('dependency_implementations', ?1)", [implementations as i64]).map_err(error)?;
    }
    if !active() {
        return Ok(None);
    }
    tx.commit().map_err(error)?;
    info(&db, publication).map(Some)
}

fn record_tree_pass(db: &Connection, selected: usize) -> Result<(), LoreError> {
    db.execute(
        "INSERT OR REPLACE INTO meta VALUES ('dependency_tree_checked', ?1), ('dependency_selected_files', ?2),
         ('dependency_tree_scans', 1 + coalesce((SELECT CAST(value AS INTEGER) FROM meta WHERE key='dependency_tree_scans'), 0))",
        params![chrono::Utc::now().timestamp(), selected as i64],
    ).map_err(error)?;
    Ok(())
}

fn record_listed_pass(db: &Connection, missing: usize) -> Result<(), LoreError> {
    db.execute(
        "INSERT OR REPLACE INTO meta VALUES ('dependency_listed_missing', ?1),
         ('dependency_listed_scans', 1 + coalesce((SELECT CAST(value AS INTEGER) FROM meta WHERE key='dependency_listed_scans'), 0))",
        [missing],
    ).map_err(error)?;
    Ok(())
}

fn info(db: &Connection, mut publication: Publication) -> Result<Publication, LoreError> {
    publication.indexed = db.query_row("SELECT total FROM atlas_counts WHERE name='files'", [], |row| row.get(0)).map_err(error)?;
    publication.implementations = db.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='dependency_implementations'",
        [], |row| row.get(0)).map_err(error)?;
    Ok(publication)
}

fn count_implementations(db: &Connection) -> Result<usize, LoreError> {
    let mut total = 0;
    let mut statement = db.prepare("SELECT path FROM files WHERE lang <> 'binary'").map_err(error)?;
    let paths = statement.query_map([], |row| row.get::<_, String>(0)).map_err(error)?;
    for path in paths {
        if implementation(Path::new(&path.map_err(error)?)) {
            total += 1;
        }
    }
    Ok(total)
}
