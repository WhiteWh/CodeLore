//! Build evidence supplies exact files and optional explicit source roots.
//! An include search path is recorded as provenance, never walked.
use super::{path_key, source_key, Evidence, Plan, Source};
use crate::{config::{self, Config}, LoreError};
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

mod ninja;

const ROOT_MANIFEST: &str = "atlas-build-sources.json";
const COMPDB: &str = "compile_commands.json";
const MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ARTIFACTS: usize = 20_000;
const MAX_DIRS: usize = 30_000;
/// The worker polls every two seconds for its source trees. Build outputs are
/// walked again at most this often: a large build folder is thousands of
/// directory reads, and new build evidence can wait this long.
const WALK_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Deserialize)]
struct MsvcReport {
    #[serde(rename = "Data")]
    data: MsvcData,
}

#[derive(Deserialize)]
struct MsvcData {
    #[serde(rename = "Source")]
    source: PathBuf,
    #[serde(rename = "Includes", default)]
    includes: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct CompileCommand {
    directory: PathBuf,
    file: PathBuf,
    output: Option<PathBuf>,
    #[serde(default)]
    arguments: Vec<String>,
    command: Option<String>,
}

#[derive(Deserialize)]
struct RootManifest {
    version: u32,
    contexts: Vec<Context>,
}

#[derive(Deserialize)]
struct Context {
    id: String,
    translation_units: Vec<PathBuf>,
    compilation_database: Option<PathBuf>,
    sources: Vec<Declaration>,
}

#[derive(Deserialize)]
struct Declaration {
    root: PathBuf,
    version: Option<String>,
    #[serde(default)]
    files: Vec<PathBuf>,
}

#[derive(Clone, Eq, PartialEq)]
struct Artifact {
    path: PathBuf,
    modified: SystemTime,
    bytes: u64,
}

#[derive(Default)]
struct Group {
    files: BTreeSet<PathBuf>,
    evidence: BTreeSet<Evidence>,
    search_paths: Vec<PathBuf>,
    stale: bool,
}

#[derive(Default)]
pub(super) struct Discovery {
    cached: Option<Cached>,
}

struct Cached {
    walked: std::time::Instant,
    artifacts: Vec<Artifact>,
    inputs: BTreeMap<PathBuf, Option<SystemTime>>,
    plans: Vec<Plan>,
    notes: Vec<String>,
}

impl Discovery {
    pub fn plans(&mut self, config: &Config) -> Result<(Vec<Plan>, Vec<String>), LoreError> {
        if let Some(previous) = self.cached.as_ref().filter(|cached| cached.walked.elapsed() < walk_every()) {
            return Ok((previous.plans.clone(), previous.notes.clone()));
        }
        let walked = std::time::Instant::now();
        let artifacts = artifacts(config)?;
        if let Some(previous) = &mut self.cached {
            if previous.artifacts == artifacts && previous.inputs.iter().all(|(path, stamp)| input_stamp(path) == *stamp) {
                previous.walked = walked;
                return Ok((previous.plans.clone(), previous.notes.clone()));
            }
        }
        let mut inputs = BTreeMap::new();
        let (plans, notes) = discover(&config.root, &artifacts, &mut inputs)?;
        self.cached = Some(Cached { walked, artifacts, inputs, plans: plans.clone(), notes: notes.clone() });
        Ok((plans, notes))
    }
}

fn walk_every() -> std::time::Duration {
    std::env::var("LORE_ATLAS_DISCOVERY_SECS").ok().and_then(|value| value.trim().parse().ok())
        .map_or(WALK_EVERY, std::time::Duration::from_secs)
}

fn error(detail: impl std::fmt::Display) -> LoreError {
    LoreError::new(format!("atlas build evidence: {detail}"))
}

fn list(directory: &Path) -> Result<Vec<fs::DirEntry>, LoreError> {
    let read = match fs::read_dir(directory) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(error(format!("{}: {e}", directory.display()))),
    };
    let mut entries = read.collect::<io::Result<Vec<_>>>().map_err(error)?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn is_artifact(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
        name == COMPDB || name == ROOT_MANIFEST || name.ends_with(".dep.json")
    })
}

fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && !super::super::unreal::is_link(&meta))
}

fn artifacts(config: &Config) -> Result<Vec<Artifact>, LoreError> {
    let mut selected = BTreeMap::new();
    let mut locations = vec![config.root.clone()];
    // Some build systems put metadata one level below a checkout. Only
    // probe output paths, regardless of the names of the source folders.
    locations.extend(list(&config.root)?.into_iter().filter_map(|entry| {
        let path = entry.path();
        (entry.file_name() != ".git" && real_dir(&path)).then_some(path)
    }));
    if let Some(checkout) = &config.atlas.checkout {
        locations.push(checkout.clone());
    }
    let mut pending = Vec::new();
    let mut seen = BTreeSet::new();
    for location in locations {
        // A scoped workspace may share the checkout's build outputs. Only
        // its own translation units can authorize dependencies.
        for name in [ROOT_MANIFEST, COMPDB] {
            record_artifact(location.join(name), &mut selected)?;
        }
        for parts in [&["build"][..], &["out"][..], &["Intermediate", "Build"][..]] {
            let mut path = location.clone();
            if parts.iter().all(|part| { path.push(part); real_dir(&path) }) {
                pending.push((path, 0));
            }
        }
    }
    for entry in list(&config.root)? {
        if is_artifact(&entry.path()) {
            record_artifact(entry.path(), &mut selected)?;
        }
    }
    while let Some((dir, depth)) = pending.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        if seen.len() > MAX_DIRS {
            return Err(error("too many build output directories"));
        }
        for entry in list(&dir)? {
            let path = entry.path();
            if is_artifact(&path) {
                record_artifact(path, &mut selected)?;
            } else if depth < 16 && real_dir(&path) && entry.file_name() != ".git" {
                pending.push((path, depth + 1));
            }
        }
    }
    let compdbs: Vec<_> = selected.keys().filter(|path| path.file_name().is_some_and(|name| name == COMPDB))
        .cloned().collect();
    for compdb in compdbs {
        let build = compdb.parent().expect("compilation database directory");
        record_artifact(build.join(ninja::MANIFEST), &mut selected)?;
        record_artifact(build.join(ninja::DEPS_LOG), &mut selected)?;
    }
    Ok(selected.into_values().collect())
}

fn record_artifact(path: PathBuf, selected: &mut BTreeMap<PathBuf, Artifact>) -> Result<(), LoreError> {
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(error(format!("{}: {e}", path.display()))),
    };
    if !meta.is_file() || super::super::unreal::is_link(&meta) {
        return Ok(());
    }
    let modified = meta.modified().map_err(error)?;
    selected.entry(path.clone()).or_insert(Artifact { path, modified, bytes: meta.len() });
    if selected.len() > MAX_ARTIFACTS {
        return Err(error("too many dependency reports"));
    }
    Ok(())
}

fn read_bytes(artifact: &Artifact) -> Result<Vec<u8>, LoreError> {
    let read = || -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let mut bytes = Vec::new();
        File::open(&artifact.path)?.take(MAX_ARTIFACT_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
            return Err("build artifact exceeds 32 MiB".into());
        }
        Ok(bytes)
    };
    read().map_err(|e| error(format!("{}: {e}", artifact.path.display())))
}

fn read<T: serde::de::DeserializeOwned>(artifact: &Artifact) -> Result<(T, String), LoreError> {
    let bytes = read_bytes(artifact)?;
    let parsed = serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes))
        .map_err(|e| error(format!("{}: {e}", artifact.path.display())))?;
    Ok((parsed, super::digest(&bytes)))
}

fn evidence(artifact: &Artifact, sha256: &str, field: &'static str, context: String) -> Evidence {
    Evidence {
        artifact: artifact.path.clone(), field, context, sha256: sha256.to_owned(),
        recorded_at: chrono::DateTime::<chrono::Utc>::from(artifact.modified).to_rfc3339(),
    }
}

fn under(path: &Path, root: &Path) -> bool {
    let root = path_key(root);
    let path = path_key(path);
    path == root || path.strip_prefix(root.trim_end_matches('/')).is_some_and(|rest| rest.starts_with('/'))
}

fn canonical(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok().map(|path| config::simplify(&path))
}

fn recorded_path(path: &Path) -> Option<PathBuf> {
    if let Some(path) = canonical(path) {
        return Some(path);
    }
    // Preserve evidence for a missing file under the last present ancestor.
    for ancestor in path.ancestors().skip(1) {
        let tail = path.strip_prefix(ancestor).ok()?;
        if let Some(base) = canonical(ancestor) {
            return tail.components().all(|part| matches!(part, Component::Normal(_))).then(|| base.join(tail));
        }
    }
    None
}

pub(in crate::atlas) fn implementation(path: &Path) -> bool {
    matches!(extension(path).as_str(), "c" | "cc" | "cpp" | "cxx" | "c++" | "m" | "mm" | "ixx" | "cppm")
}

fn extension(path: &Path) -> String {
    path.extension().and_then(|ext| ext.to_str()).unwrap_or_default().to_ascii_lowercase()
}

fn source_file(path: &Path) -> bool {
    implementation(path) || matches!(extension(path).as_str(), "h" | "hh" | "hpp" | "hxx" | "inl" | "ipp" | "tpp")
}

fn toolchain(path: &Path) -> bool {
    let name = path_key(path);
    if cfg!(windows) {
        ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"].into_iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .any(|root| under(path, &root.join("Windows Kits")) ||
                (under(path, &root.join("Microsoft Visual Studio")) &&
                    (name.contains("/vc/tools/msvc/") || name.contains("/lib/clang/"))))
    } else {
        ["/usr/include", "/usr/lib/clang", "/usr/lib/gcc", "/Library/Developer", "/Applications/Xcode.app"]
            .iter().any(|root| under(path, Path::new(root)))
    }
}

fn input_stamp(path: &Path) -> Option<SystemTime> {
    path.metadata().ok().filter(|meta| meta.is_file())?.modified().ok()
}

fn context_tu(raw: &Path, workspace: &Path, inputs: &mut BTreeMap<PathBuf, Option<SystemTime>>) -> Option<PathBuf> {
    if !raw.is_absolute() || !implementation(raw) || !under(raw, workspace) {
        return None;
    }
    inputs.entry(raw.to_path_buf()).or_insert_with(|| input_stamp(raw));
    canonical(raw).filter(|path| path.is_file() && under(path, workspace))
}

fn stale(tu: &Path, artifact: &Artifact) -> bool {
    tu.metadata().and_then(|meta| meta.modified()).is_ok_and(|when| when > artifact.modified)
}

fn report_context(folder: &Path) -> PathBuf {
    for base in folder.ancestors() {
        let name = base.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if name.eq_ignore_ascii_case("build") || name.eq_ignore_ascii_case("out") {
            let intermediate = base.parent().and_then(Path::file_name).and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("Intermediate"));
            let prefix = folder.strip_prefix(base).expect("build directory ancestor");
            let mut context = base.to_path_buf();
            context.extend(prefix.components().take(if intermediate { 4 } else { 1 }));
            return context;
        }
    }
    folder.to_path_buf()
}

fn include_dirs(row: &CompileCommand, directory: &Path) -> Vec<PathBuf> {
    let Some(arguments) = super::super::calls::command_arguments(&row.arguments, row.command.as_deref()) else { return Vec::new() };
    let mut dirs = Vec::new();
    let mut args = arguments.iter().skip(1);
    while let Some(arg) = args.next() {
        let value = ["-I", "/I", "-isystem", "/external:I"].iter().find_map(|flag| arg.strip_prefix(flag));
        if let Some(value) = value {
            let value = if value.is_empty() { args.next().map_or("", String::as_str) } else { value };
            if !value.is_empty() {
                let path = directory.join(value);
                dirs.push(recorded_path(&path).unwrap_or(path));
            }
        }
    }
    dirs
}

fn append_dirs(dirs: &mut Vec<PathBuf>, added: impl IntoIterator<Item = PathBuf>) {
    for path in added {
        if dirs.len() < 64 && !dirs.contains(&path) {
            dirs.push(path);
        }
    }
}

fn discover(workspace: &Path, artifacts: &[Artifact], inputs: &mut BTreeMap<PathBuf, Option<SystemTime>>) -> Result<(Vec<Plan>, Vec<String>), LoreError> {
    let mut notes = Vec::new();
    let mut declarations = Vec::new();
    let mut compdb = Vec::new();
    let mut linked = Vec::new();
    let mut readable = 0;
    let mut observed = BTreeMap::<(PathBuf, PathBuf), Group>::new();
    let mut resolved = HashMap::<PathBuf, Option<PathBuf>>::new();
    for artifact in artifacts {
        let folder = artifact.path.parent().expect("artifact directory");
        let kind = artifact.path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if matches!(kind, ninja::MANIFEST | ninja::DEPS_LOG) {
            continue;
        }
        let loaded = if kind == ROOT_MANIFEST {
            read::<RootManifest>(artifact).and_then(|(manifest, sha256)| {
                if manifest.version != 1 { return Err(error("unsupported atlas-build-sources.json version")); }
                declarations.extend(manifest.contexts.into_iter().map(|context| (artifact, sha256.clone(), context)));
                Ok(())
            })
        } else if kind == COMPDB {
            read::<Vec<CompileCommand>>(artifact).map(|(rows, sha256)| {
                let companion = |name| artifacts.iter().find(|artifact| artifact.path == folder.join(name));
                if let Some(manifest) = companion(ninja::MANIFEST) {
                    match ninja::groups(workspace, artifact, &sha256, &rows, manifest, companion(ninja::DEPS_LOG), inputs) {
                        Ok(groups) => linked.extend(groups),
                        Err(error) => notes.push(error.to_string()),
                    }
                }
                let mut files = BTreeSet::new();
                let mut consumers = BTreeSet::new();
                let mut dirs = Vec::new();
                for row in rows {
                    let directory = folder.join(&row.directory);
                    let file = directory.join(&row.file);
                    if let Some(tu) = context_tu(&file, workspace, inputs) {
                        consumers.insert(tu);
                        append_dirs(&mut dirs, include_dirs(&row, &directory));
                    } else if source_file(&file) && !toolchain(&file) {
                        files.insert(file);
                    }
                }
                if !consumers.is_empty() {
                    compdb.push((artifact, sha256, consumers, files, dirs));
                }
            })
        } else {
            read::<MsvcReport>(artifact).map(|(report, sha256)| {
                let Some(tu) = context_tu(&report.data.source, workspace, inputs) else { return };
                let context = report_context(folder);
                let provenance = evidence(artifact, &sha256, "Data.Includes", context.display().to_string());
                let stale = stale(&tu, artifact);
                for path in report.data.includes {
                    if !path.is_absolute() || !source_file(&path) || under(&path, workspace) || toolchain(&path) {
                        continue;
                    }
                    let Some(path) = resolved.entry(path.clone()).or_insert_with(|| recorded_path(&path)).as_ref() else { continue };
                    if under(path, workspace) || toolchain(path) {
                        continue;
                    }
                    let volume = path.ancestors().last().expect("file volume").to_path_buf();
                    let group = observed.entry((context.clone(), volume)).or_default();
                    group.files.insert(path.clone());
                    group.evidence.insert(provenance.clone());
                    group.stale |= stale;
                }
            })
        };
        match loaded {
            Ok(()) => readable += 1,
            Err(e) => notes.push(e.to_string()),
        }
    }
    if readable == 0 && !notes.is_empty() {
        return Err(error(notes.join("\n")));
    }
    notes.truncate(16);

    let mut result = BTreeMap::new();
    let mut declared_files = BTreeSet::new();
    let mut explicit_roots = BTreeSet::new();
    for (artifact, sha256, context) in declarations {
        let folder = artifact.path.parent().expect("manifest directory");
        let requested_db = folder.join(context.compilation_database.as_deref().unwrap_or(Path::new(COMPDB)));
        let requested_db = canonical(&requested_db).unwrap_or(requested_db);
        let consumers: BTreeSet<_> = context.translation_units.iter()
            .filter_map(|tu| context_tu(&folder.join(tu), workspace, inputs)).collect();
        if consumers.is_empty() {
            continue;
        }
        for declaration in context.sources {
            let Some(root) = recorded_path(&folder.join(&declaration.root)) else { continue };
            if root.metadata().is_ok_and(|meta| !meta.is_dir()) || root.parent().is_none()
                || under(workspace, &root) || under(&root, workspace) || toolchain(&root)
            {
                continue;
            }
            let mut group = Group { stale: consumers.iter().any(|tu| stale(tu, artifact)), ..Group::default() };
            group.evidence.insert(evidence(artifact, &sha256, "contexts[].sources[]", context.id.clone()));
            let mut select = |raw: &Path| {
                if let Some(path) = recorded_path(raw) {
                    if under(&path, &root) && source_file(&path) && !toolchain(&path) {
                        group.files.insert(path);
                    }
                }
            };
            for path in declaration.files {
                select(&root.join(path));
            }
            for (db, db_sha, units, files, dirs) in &compdb {
                if path_key(&db.path) != path_key(&requested_db) || units.is_disjoint(&consumers) {
                    continue;
                }
                for file in files.iter().filter(|file| under(file, &root)) {
                    select(file);
                }
                append_dirs(&mut group.search_paths, dirs.iter().cloned());
                group.evidence.insert(evidence(db, db_sha, "file", context.id.clone()));
            }
            for reports in observed.values().filter(|reports| !reports.files.is_disjoint(&group.files)) {
                group.evidence.extend(reports.evidence.iter().cloned());
                group.stale |= reports.stale;
            }
            if !group.files.is_empty() {
                explicit_roots.insert(root.clone());
                declared_files.extend(group.files.iter().cloned());
                insert_plan(&mut result, root, true, declaration.version, group);
            }
        }
    }
    for (root, group) in linked {
        if !explicit_roots.contains(&root) {
            declared_files.extend(group.files.iter().cloned());
            insert_plan(&mut result, root, true, None, group);
        }
    }
    for (_, mut group) in observed {
        group.files.retain(|file| !declared_files.contains(file));
        if let Some(first) = group.files.first() {
            let mut ancestor = first.parent().expect("source parent");
            for path in &group.files {
                while !path.starts_with(ancestor) {
                    ancestor = ancestor.parent().expect("a common volume");
                }
            }
            let anchor = ancestor.to_path_buf();
            insert_plan(&mut result, anchor, false, None, group);
        }
    }
    Ok((result.into_values().collect(), notes))
}

fn insert_plan(result: &mut BTreeMap<String, Plan>, anchor: PathBuf, declared: bool, version: Option<String>, group: Group) {
    let scope = if declared { "declared_target_files" } else { "observed_files" };
    let index_key = source_key(&anchor, scope, version.as_deref(), &group.files);
    let source = Source {
        name: None, binding: None, configured_path: None,
        canonical_path: anchor.clone(), source_root: declared.then_some(anchor), index_key: index_key.clone(),
        scope: scope.to_owned(), version, observed_files: group.files.len(),
        implementation: if group.files.iter().any(|path| implementation(path)) { "recorded" } else { "unconfirmed" }.to_owned(),
        freshness: if group.stale { "stale" } else { "recorded" }.to_owned(), missing_files: 0,
        provenance_count: group.evidence.len(), provenance: group.evidence.iter().take(16).map(|evidence| json!(evidence)).collect(),
        search_paths: group.search_paths, state: "indexing".to_owned(), note: None,
    };
    result.entry(index_key).and_modify(|previous| {
        previous.source.provenance_count += source.provenance_count;
        if source.freshness == "stale" {
            previous.source.freshness = source.freshness.clone();
        }
        for evidence in &source.provenance {
            if previous.source.provenance.len() < 16 && !previous.source.provenance.contains(evidence) {
                previous.source.provenance.push(evidence.clone());
            }
        }
        append_dirs(&mut previous.source.search_paths, source.search_paths.iter().cloned());
    }).or_insert(Plan { source, files: super::Files::Listed(group.files) });
}
