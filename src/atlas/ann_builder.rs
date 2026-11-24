//! Background publication for one source and vector space.
use super::{current, inspect, inspect_compatible, options, valid_generation, Binding, Manifest, Stamp, Unavailable, CONNECTIVITY, EXPANSION_ADD};
use crate::atlas::store::{self, Atlas};
use crate::config::Config;
use crate::LoreError;
use fs2::FileExt;
use memmap2::MmapOptions;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use usearch::Index;

const BUILD_LOCK: &str = "build.lock";
const REQUEST: &str = "request.json";
const STATE: &str = "state.json";
const THREADS: usize = 4;
const BATCH: usize = 4096;
const START_GRACE: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_secs(5);
/// While a published generation for the same source and space serves
/// searches, a rebuild waits this long after the last publication.
const REBUILD_AFTER: Duration = Duration::from_secs(60);

fn rebuild_after() -> Duration {
    std::env::var("LORE_ATLAS_ANN_REBUILD_SECS").ok().and_then(|value| value.trim().parse().ok())
        .map_or(REBUILD_AFTER, Duration::from_secs)
}

fn published_within(dir: &Path, limit: Duration) -> bool {
    fs::metadata(dir.join("current.json")).and_then(|m| m.modified()).ok()
        .and_then(|m| m.elapsed().ok()).is_some_and(|age| age < limit)
}

#[derive(Serialize, Deserialize)]
struct Job {
    root: PathBuf,
    binding: Binding,
}

#[derive(Serialize, Deserialize)]
struct State {
    state: String,
    binding: Binding,
    pid: Option<u32>,
    error: Option<String>,
}

fn problem(detail: impl std::fmt::Display) -> LoreError {
    LoreError::new(format!("atlas ANN: {detail}"))
}

fn enabled(config: &Config) -> bool {
    config.atlas.semantic && config.embed_provider != "none"
}

fn eligible(atlas: &Atlas, space: &str, model: &str, dimension: usize) -> Option<Binding> {
    if atlas.small_enough_for_exact().ok()? {
        return None;
    }
    let covered = atlas.compatible_coverage(model, dimension)?;
    if covered == 0 {
        return None;
    }
    atlas.ann_binding(space, model, dimension, covered)
}

fn read_state(dir: &Path) -> Option<State> {
    serde_json::from_reader(File::open(dir.join(STATE)).ok()?).ok()
}

fn recent(dir: &Path, timeout: Duration) -> bool {
    fs::metadata(dir.join(STATE)).and_then(|m| m.modified()).ok()
        .and_then(|m| m.elapsed().ok()).is_some_and(|age| age < timeout)
}

fn locked(dir: &Path) -> bool {
    File::open(dir.join(BUILD_LOCK)).ok().is_some_and(|file| {
        FileExt::try_lock_shared(&file).is_err_and(|error| store::is_locked(&error))
    })
}

fn write(path: &Path, value: &impl Serialize) -> Result<(), LoreError> {
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    {
        let mut file = File::create(&temp).map_err(problem)?;
        serde_json::to_writer(&mut file, value).map_err(problem)?;
        file.sync_all().map_err(problem)?;
    }
    fs::rename(&temp, path).map_err(problem)
}

pub(in crate::atlas) fn status(atlas: &Atlas, config: &Config, space: &str, model: &str, dimension: usize) -> Value {
    if !enabled(config) {
        return json!({"mode":"text","state":"disabled"});
    }
    let covered = atlas.compatible_coverage(model, dimension);
    if covered == Some(0) {
        return json!({"mode":"text","state":"no_vectors"});
    }
    if atlas.small_enough_for_exact().is_ok_and(|small| small) {
        return json!({"mode":"exact","state":"ready"});
    }
    let Some(binding) = eligible(atlas, space, model, dimension) else {
        return json!({"mode":"text","state":"unavailable"});
    };
    let dir = binding.directory(&atlas.path);
    let failure = match inspect(&dir, &binding) {
        Ok((manifest, _, _)) => return json!({"mode":"ann","state":"ready","generation":manifest.generation}),
        Err(reason) => reason,
    };
    // An older generation of this source keeps serving while a new one builds.
    let behind = inspect_compatible(&dir, &binding).ok().map(|(manifest, _, _)| manifest.generation);
    let mode = if behind.is_some() { "ann" } else { "text" };
    let with_generation = |mut value: Value| {
        if let Some(generation) = &behind {
            value["generation"] = json!(generation);
        }
        value
    };
    if let Some(state) = read_state(&dir) {
        if locked(&dir) || state.state == "starting" && recent(&dir, START_GRACE) {
            return with_generation(json!({"mode":mode,"state":state.state,"pid":state.pid}));
        }
        if state.binding == binding && state.state == "error" {
            return with_generation(json!({"mode":mode,"state":"error","error":state.error}));
        }
    }
    if behind.is_some() {
        return with_generation(json!({"mode":"ann","state":"behind"}));
    }
    match failure {
        Unavailable::Missing => json!({"mode":"text","state":"unindexed"}),
        Unavailable::Stale => json!({"mode":"text","state":"stale"}),
        Unavailable::Damaged(err) => json!({"mode":"text","state":"damaged","error":err}),
    }
}

pub(in crate::atlas) fn schedule(atlas: &Atlas, config: &Config, space: &str, model: &str, dimension: usize) {
    if !enabled(config) {
        return;
    }
    let Some(binding) = eligible(atlas, space, model, dimension) else { return };
    let Ok(db) = fs::canonicalize(&atlas.path) else { return };
    let dir = binding.directory(&db);
    if inspect(&dir, &binding).is_ok() {
        return;
    }
    // Searches use the older generation and score changed chunks exactly, so
    // a burst of edits costs one rebuild per interval, not one per edit.
    if inspect_compatible(&dir, &binding).is_ok() && published_within(&dir, rebuild_after()) {
        return;
    }
    if let Err(error) = start(atlas, &db, &dir, &binding) {
        let failure = State { state: "error".to_owned(), binding, pid: None, error: Some(error.to_string()) };
        let _ = write(&dir.join(STATE), &failure);
    }
}

fn start(atlas: &Atlas, db: &Path, dir: &Path, binding: &Binding) -> Result<(), LoreError> {
    fs::create_dir_all(dir).map_err(problem)?;
    let control = store::lock_file(dir, "start.lock")?;
    match FileExt::try_lock_exclusive(&control) {
        Ok(()) => {}
        Err(error) if store::is_locked(&error) => return Ok(()),
        Err(error) => return Err(problem(error)),
    }
    if locked(dir) || inspect(dir, binding).is_ok() {
        return Ok(());
    }
    if let Some(state) = read_state(dir) {
        if state.state == "starting" && recent(dir, START_GRACE)
            || state.binding == *binding && state.state == "error" && recent(dir, RETRY)
        {
            return Ok(());
        }
    }
    write(&dir.join(REQUEST), &Job { root: atlas.ann_root().to_path_buf(), binding: binding.clone() })?;
    write(&dir.join(STATE), &State { state: "starting".to_owned(), binding: binding.clone(), pid: None, error: None })?;
    let mut command = Command::new(std::env::current_exe().map_err(problem)?);
    command.arg("--atlas-ann-worker").arg(db).arg(dir).current_dir(dir)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    crate::builtin::spawn_detached(&mut command).map_err(problem)
}

fn open(db: &Path, job: &Job, publish: bool) -> Result<Atlas, LoreError> {
    Atlas::open_ann_snapshot(db, &job.root, publish)?.ok_or_else(|| problem("SQLite snapshot is unavailable"))
}

fn latest(atlas: &Atlas, job: &Job) -> Option<Binding> {
    eligible(atlas, &job.binding.space, &job.binding.vector_model, job.binding.dimension)
        .filter(|binding| binding.source_key == job.binding.source_key)
}

pub(crate) fn run_worker(db: &Path, dir: &Path) -> Result<(), LoreError> {
    let job: Job = serde_json::from_reader(File::open(dir.join(REQUEST)).map_err(problem)?).map_err(problem)?;
    if job.binding.directory(db) != dir {
        return Err(problem("invalid ANN worker directory"));
    }
    let lease = store::lock_file(dir, BUILD_LOCK)?;
    match FileExt::try_lock_exclusive(&lease) {
        Ok(()) => {}
        Err(error) if store::is_locked(&error) => return Ok(()),
        Err(error) => return Err(problem(error)),
    }
    let mut binding = job.binding.clone();
    let result = build(db, dir, &job, &mut binding);
    if let Err(error) = &result {
        write(&dir.join(STATE), &State { state: "error".to_owned(), binding, pid: None, error: Some(error.to_string()) })?;
    }
    result
}

fn build(db: &Path, dir: &Path, job: &Job, binding: &mut Binding) -> Result<(), LoreError> {
    let mut atlas = open(db, job, false)?;
    if latest(&atlas, job).is_none() {
        return Err(problem("no compatible active vectors in the selected source"));
    }
    // Upgrade legacy schema-2 snapshots in a short separate write transaction.
    if atlas.meta("ann_vector_clock").is_none() {
        drop(atlas);
        let writer = open(db, job, true)?;
        writer.install_ann_clock()?;
        writer.db.execute_batch("COMMIT").map_err(problem)?;
        drop(writer);
        atlas = open(db, job, false)?;
    }
    *binding = latest(&atlas, job).ok_or_else(|| problem("SQLite data changed before the build"))?;
    if inspect(dir, binding).is_ok() {
        return Ok(());
    }
    write(&dir.join(STATE), &State { state: "building".to_owned(), binding: binding.clone(), pid: Some(std::process::id()), error: None })?;

    let previous = current(dir).ok().map(|manifest| manifest.generation);
    let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let generation = super::super::unreal::digest(format!("{}:{now}:{}", binding.source_key, std::process::id()));
    let keys = dir.join(format!("{generation}.keys"));
    let index_file = dir.join(format!("{generation}.usearch"));
    let key_tmp = dir.join(format!("{generation}.keys.tmp"));
    let idx_tmp = dir.join(format!("{generation}.usearch.tmp"));
    cleanup(dir, std::iter::once(previous.as_deref()));
    let mut out = BufWriter::new(OpenOptions::new().write(true).create_new(true).open(&key_tmp).map_err(problem)?);
    let index = Index::new(&options(binding.dimension)).map_err(problem)?;
    index.reserve_capacity_and_threads(binding.covered, THREADS).map_err(problem)?;
    {
        let mut stmt = atlas.db.prepare("SELECT c.id,c.ehash,v.vec FROM chunks c
            CROSS JOIN vectors v ON v.model=?1 AND v.hash=c.ehash
            JOIN files f ON f.path=c.path
            WHERE typeof(v.vec)='blob' AND length(v.vec)=?2 ORDER BY c.id").map_err(problem)?;
        let mut rows = stmt.query(params![binding.vector_model, binding.dimension * 4]).map_err(problem)?;
        let mut group = Vec::with_capacity(BATCH);
        while let Some(row) = rows.next().map_err(problem)? {
            let id = row.get::<_, u64>(0).map_err(problem)?;
            let hash = row.get_ref(1).map_err(problem)?.as_str().map_err(problem)?;
            let blob = row.get_ref(2).map_err(problem)?.as_blob().map_err(problem)?;
            let vector: Vec<_> = blob.chunks_exact(4)
                .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("f32 bytes"))).collect();
            if id == 0 || hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit())
                || vector.iter().any(|v| !v.is_finite())
            {
                return Err(problem("invalid active SQLite vector or chunk identity"));
            }
            out.write_all(&id.to_le_bytes()).and_then(|_| out.write_all(hash.as_bytes())).map_err(problem)?;
            group.push((id, vector));
            if group.len() == BATCH {
                add(&index, &group)?;
                group.clear();
            }
        }
        add(&index, &group)?;
    }
    drop(atlas);
    if index.size() != binding.covered {
        return Err(problem("ANN count disagrees with SQLite vector coverage"));
    }
    let file = out.into_inner().map_err(problem)?;
    file.sync_all().map_err(problem)?;
    drop(file);
    let file = OpenOptions::new().read(true).write(true).create_new(true).open(&idx_tmp).map_err(problem)?;
    file.set_len(index.serialized_length() as u64).map_err(problem)?;
    {
        // SAFETY: This worker exclusively owns the new staging file. Its
        // length is fixed and it is not published while the map is writable.
        let mut mapping = unsafe { MmapOptions::new().map_mut(&file) }.map_err(problem)?;
        index.save_to_buffer(&mut mapping).map_err(problem)?;
        mapping.flush().map_err(problem)?;
    }
    file.sync_all().map_err(problem)?;
    drop(file);
    drop(index);
    fs::rename(&key_tmp, &keys).map_err(problem)?;
    fs::rename(&idx_tmp, &index_file).map_err(problem)?;
    let manifest = Manifest { format: 1, metric: "f16_ip".to_owned(), connectivity: CONNECTIVITY,
        expansion_add: EXPANSION_ADD, generation: generation.clone(), binding: binding.clone(),
        index: Stamp::read(&index_file).map_err(problem)?, keys: Stamp::read(&keys).map_err(problem)? };
    // Serialize publication with SQLite's writer, but finish all expensive
    // work before taking it. On any failure the old pointer stays in place.
    // The rows came from one read transaction, so the generation matches
    // `binding` exactly. SQLite may have moved on since: readers then treat it
    // as behind and score the changed chunks themselves, and the next
    // schedule builds again. Only a different source or space is refused.
    let writer = open(db, job, true)?;
    if !latest(&writer, job).is_some_and(|now| now.compatible(binding)) {
        return Err(problem("SQLite changed before ANN publication"));
    }
    write(&dir.join("current.json"), &manifest)?;
    drop(writer);
    cleanup(dir, [Some(generation.as_str()), previous.as_deref()]);
    write(&dir.join(STATE), &State { state: "ready".to_owned(), binding: binding.clone(), pid: None, error: None })?;
    Ok(())
}

fn add(index: &Index, group: &[(u64, Vec<f32>)]) -> Result<(), LoreError> {
    if group.is_empty() {
        return Ok(());
    }
    std::thread::scope(|scope| {
        let threads: Vec<_> = group.chunks(group.len().div_ceil(THREADS)).map(|rows| {
            scope.spawn(move || {
                for (id, vector) in rows {
                    index.add(*id, vector).map_err(problem)?;
                }
                Ok::<_, LoreError>(())
            })
        }).collect();
        for worker in threads {
            worker.join().map_err(|_| problem("ANN build thread exited"))??;
        }
        Ok(())
    })
}

fn cleanup<'a>(dir: &Path, retain: impl IntoIterator<Item = Option<&'a str>>) {
    let retain: Vec<_> = retain.into_iter().flatten().collect();
    let Ok(files) = fs::read_dir(dir) else { return };
    for file in files.flatten() {
        let name = file.file_name();
        let Some((generation, ext)) = name.to_str().and_then(|name| name.split_once('.')) else { continue };
        if valid_generation(generation) && !retain.contains(&generation)
            && matches!(ext, "usearch" | "keys" | "usearch.tmp" | "keys.tmp")
        {
            // Windows can hold a mapped generation open in another process.
            // Retry naturally at the next build, without touching its ACL.
            let _ = fs::remove_file(file.path());
        }
    }
}
