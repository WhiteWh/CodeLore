//! Read the static edges emitted by CMake's Ninja generator. A consuming
//! object must agree with the compilation database before its link graph
//! contributes sources. Order-only inputs and include flags grant no scope.
use super::*;
use std::time::UNIX_EPOCH;

pub(super) const MANIFEST: &str = "build.ninja";
pub(super) const DEPS_LOG: &str = ".ninja_deps";

#[derive(Default)]
struct Graph {
    objects: HashMap<String, String>,
    links: Vec<Link>,
    producers: HashMap<String, usize>,
    declared: BTreeSet<PathBuf>,
}

struct Link {
    target: String,
    inputs: Vec<String>,
    implicit: Vec<String>,
}

#[derive(PartialEq)]
enum Token {
    Word(String),
    Colon,
    Implicit,
    EndInputs,
}

fn tokens(line: &str) -> Option<Vec<Token>> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut chars = line.chars().peekable();
    let flush = |word: &mut String, result: &mut Vec<Token>| {
        if !word.is_empty() {
            result.push(Token::Word(std::mem::take(word)));
        }
    };
    while let Some(ch) = chars.next() {
        match ch {
            '$' => match chars.next()? {
                literal @ (' ' | ':' | '$') => word.push(literal),
                _ => return None, // no evaluation of Ninja variables
            },
            '#' => break,
            ':' | '|' => {
                flush(&mut word, &mut result);
                if ch == ':' {
                    result.push(Token::Colon);
                } else if matches!(chars.peek(), Some('|' | '@')) {
                    chars.next();
                    result.push(Token::EndInputs);
                    break;
                } else {
                    result.push(Token::Implicit);
                }
            }
            ' ' | '\t' => flush(&mut word, &mut result),
            _ => word.push(ch),
        }
    }
    flush(&mut word, &mut result);
    Some(result)
}

fn absolute(base: &Path, path: &str) -> PathBuf {
    let mut absolute = PathBuf::new();
    for part in base.join(path).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => { absolute.pop(); }
            _ => absolute.push(part),
        }
    }
    absolute
}

fn key(base: &Path, path: &str) -> String {
    path_key(&absolute(base, path))
}

fn words(input: &[Token]) -> impl Iterator<Item = &str> {
    input.iter().filter_map(|token| match token { Token::Word(word) => Some(word.as_str()), _ => None })
}

fn cxx_rule(rule: &str, kind: &str) -> bool {
    rule.strip_prefix("CXX_").or_else(|| rule.strip_prefix("C_"))
        .is_some_and(|rule| rule.starts_with(kind))
}

impl Graph {
    fn read(base: &Path, text: &str) -> Self {
        let mut graph = Self::default();
        let mut pending = String::new();
        let mut active_link: Option<usize> = None;
        for line in text.lines() {
            if line.as_bytes().iter().rev().take_while(|&&ch| ch == b'$').count() % 2 != 0 {
                pending.push_str(if pending.is_empty() { &line[..line.len()-1] } else { line.trim_start().strip_suffix('$').unwrap_or_default() });
                continue;
            }
            let full = if pending.is_empty() { std::borrow::Cow::Borrowed(line) } else {
                pending.push_str(line.trim_start());
                std::borrow::Cow::Owned(std::mem::take(&mut pending))
            };
            let line = full.as_ref();
            if line.starts_with([' ', '\t']) {
                if let (Some(index), Some(libraries)) = (active_link, line.trim_start().strip_prefix("LINK_LIBRARIES = ")) {
                    graph.link_libraries(index, base, libraries);
                }
                continue;
            }
            active_link = line.strip_prefix("build ").and_then(|line| graph.add_edge(base, line));
        }
        graph
    }

    fn add_edge(&mut self, base: &Path, line: &str) -> Option<usize> {
        let parts = tokens(line)?;
        let colon = parts.iter().position(|token| *token == Token::Colon)?;
        let (rule, inputs) = parts[colon+1..].split_first()?;
        let Token::Word(rule) = rule else { return None };
        let split = inputs.iter().position(|token| matches!(token, Token::Implicit | Token::EndInputs)).unwrap_or(inputs.len());
        let mut outputs = words(&parts[..colon]);
        let output = outputs.next()?;
        if rule.starts_with("RERUN_CMAKE") && output == MANIFEST {
            for path in words(inputs) {
                let path = absolute(base, path);
                if path.file_name().is_some_and(|name| name == "CMakeLists.txt") {
                    if let Some(dir) = path.parent().and_then(canonical) {
                        self.declared.insert(dir);
                    }
                }
            }
        } else if cxx_rule(rule, "COMPILER__") {
            if let Some(source) = words(&inputs[..split]).next() {
                self.objects.insert(key(base, output), key(base, source));
            }
        } else if ["STATIC_LIBRARY_LINKER__", "SHARED_LIBRARY_LINKER__", "MODULE_LIBRARY_LINKER__", "EXECUTABLE_LINKER__"]
            .iter().any(|kind| cxx_rule(rule, kind))
        {
            let index = self.links.len();
            for output in std::iter::once(output).chain(outputs) {
                self.producers.insert(key(base, output), index);
            }
            self.links.push(Link {
                target: output.to_owned(),
                inputs: words(&inputs[..split]).map(|path| key(base, path)).collect(),
                implicit: words(&inputs[split..]).map(|path| key(base, path)).collect(),
            });
            return Some(index);
        }
        None
    }

    fn link_libraries(&mut self, index: usize, base: &Path, libraries: &str) {
        let Some(held) = super::super::super::calls::command_arguments(&[], Some(&format!("link {libraries}"))) else { return };
        let link = &mut self.links[index];
        for raw in held.iter().skip(1) {
            if raw.starts_with('-') || raw.contains('$') {
                continue;
            }
            let path = key(base, raw);
            if link.implicit.contains(&path) {
                link.inputs.push(path);
            }
        }
    }

    fn linked_objects(&self, consumer: usize) -> BTreeSet<&str> {
        let mut targets = vec![consumer];
        let mut visited = BTreeSet::new();
        let mut objects = BTreeSet::new();
        while let Some(index) = targets.pop() {
            if !visited.insert(index) {
                continue;
            }
            for input in &self.links[index].inputs {
                if self.objects.contains_key(input) {
                    objects.insert(input.as_str());
                } else if let Some(link) = self.producers.get(input) {
                    targets.push(*link);
                }
            }
        }
        objects
    }
}

struct CompileUnit<'a> {
    source: PathBuf,
    object: PathBuf,
    row: &'a CompileCommand,
    cwd: PathBuf,
}

pub(super) fn groups(
    workspace: &Path, db: &Artifact, db_sha: &str, rows: &[CompileCommand],
    manifest: &Artifact, deps: Option<&Artifact>, inputs: &mut BTreeMap<PathBuf, Option<SystemTime>>,
) -> Result<Vec<(PathBuf, Group)>, LoreError> {
    let base = manifest.path.parent().expect("Ninja build directory");
    let bytes = read_bytes(manifest)?;
    let content = std::str::from_utf8(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes)).map_err(error)?;
    let graph = Graph::read(base, content);
    let graph_sha = super::super::digest(&bytes);
    let mut units = BTreeMap::new();
    let mut consumers = BTreeSet::new();
    for row in rows {
        let Some(output) = row.output.as_ref().and_then(|path| path.to_str()) else { continue };
        let cwd = base.join(&row.directory);
        let file = cwd.join(&row.file);
        let Some(file_text) = file.to_str() else { continue };
        let object = absolute(&cwd, output);
        let identity = path_key(&object);
        if !implementation(&file) || graph.objects.get(&identity) != Some(&key(&cwd, file_text)) {
            continue;
        }
        if context_tu(&file, workspace, inputs).is_some() {
            consumers.insert(identity.clone());
        }
        if let Some(source) = recorded_path(&file) {
            units.insert(identity, CompileUnit { source, object, cwd, row });
        }
    }
    if consumers.is_empty() {
        return Ok(Vec::new());
    }
    let mut linked = Vec::new();
    for (index, link) in graph.links.iter().enumerate() {
        if link.inputs.iter().any(|input| consumers.contains(input)) {
            linked.push((link, graph.linked_objects(index)));
        }
    }
    let selected: BTreeSet<_> = linked.iter().flat_map(|(_, objects)| objects.iter().copied()).collect();
    if deps.is_some() {
        for object in &selected {
            if let Some(unit) = units.get(*object) {
                inputs.entry(unit.object.clone()).or_insert_with(|| input_stamp(&unit.object));
            }
        }
    }
    let observed = deps.and_then(|artifact| {
        let bytes = read_bytes(artifact).ok()?;
        let records = dependency_headers(&bytes, base, &units, &selected)?;
        Some((artifact, super::super::digest(&bytes), records))
    });
    let mut groups = Vec::new();
    for (link, objects) in linked {
        let mut roots = BTreeMap::<PathBuf, Group>::new();
        let mut dirs = Vec::new();
        let mut headers = BTreeSet::new();
        for object in objects {
            let Some(unit) = units.get(object) else { continue };
            if let Some((_, _, records)) = &observed {
                if let Some(observed) = records.get(object) {
                    headers.extend(observed.iter().cloned());
                }
            }
            if consumers.contains(object) {
                append_dirs(&mut dirs, include_dirs(unit.row, &unit.cwd));
            }
            if under(&unit.source, base) || under(&unit.source, workspace) || toolchain(&unit.source) {
                continue;
            }
            let root = graph.declared.iter().filter(|root| !under(workspace, root) &&
                !under(root, workspace) && under(&unit.source, root)).max_by_key(|root| root.components().count());
            let Some(root) = root else { continue };
            let group = roots.entry(root.clone()).or_default();
            group.files.insert(unit.source.clone());
            group.stale |= stale(&unit.source, db);
        }
        for (root, mut group) in roots {
            group.evidence.insert(evidence(db, db_sha, "file/output", link.target.clone()));
            group.evidence.insert(evidence(manifest, &graph_sha, "build/link inputs and RERUN_CMAKE", link.target.clone()));
            append_dirs(&mut group.search_paths, dirs.iter().filter(|dir| under(dir, &root)).cloned());
            if let Some((artifact, sha, _)) = &observed {
                let before = group.files.len();
                group.files.extend(headers.iter().filter(|path| under(path, &root)).cloned());
                if group.files.len() > before {
                    group.evidence.insert(evidence(artifact, sha, "object header dependencies", link.target.clone()));
                }
            }
            groups.push((root, group));
        }
    }
    Ok(groups)
}

/// Ninja deps-log v4 uses little-endian records consisting of a size word
/// and either a padded path with its complemented id, or an object id,
/// an mtime and a list of path ids. A matching object mtime is required.
fn dependency_headers(
    bytes: &[u8], base: &Path, units: &BTreeMap<String, CompileUnit<'_>>, selected: &BTreeSet<&str>,
) -> Option<HashMap<String, BTreeSet<PathBuf>>> {
    let mut remaining = bytes.strip_prefix(b"# ninjadeps\n\x04\0\0\0")?;
    let mut paths: Vec<&str> = Vec::new();
    let mut headers = HashMap::new();
    while !remaining.is_empty() {
        let size = word(remaining)?;
        remaining = remaining.get(4..)?;
        let bytes = (size & 0x7fff_ffff) as usize;
        if bytes % 4 != 0 {
            return None;
        }
        let record = remaining.get(..bytes)?;
        remaining = remaining.get(bytes..)?;
        if size & 0x8000_0000 == 0 {
            let (path, check) = record.split_at_checked(bytes.checked_sub(4)?)?;
            if !word(check)? != paths.len() as u32 {
                return None;
            }
            paths.push(std::str::from_utf8(path).ok()?.trim_end_matches('\0'));
            continue;
        }
        let output = key(base, paths.get(word(record)? as usize)?);
        if !selected.contains(output.as_str()) {
            continue;
        }
        let Some(unit) = units.get(&output) else { continue };
        let mark = u64::from_le_bytes(record.get(4..12)?.try_into().ok()?);
        if object_stamp(&unit.object) != Some(mark) {
            headers.remove(&output);
            continue;
        }
        let mut observed = BTreeSet::new();
        for input in record.get(12..)?.chunks_exact(4) {
            let path = absolute(base, paths.get(word(input)? as usize)?);
            if source_file(&path) && !implementation(&path) && !toolchain(&path) {
                if let Some(path) = recorded_path(&path) {
                    observed.insert(path);
                }
            }
        }
        headers.insert(output, observed);
    }
    Some(headers)
}

fn word(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn object_stamp(path: &Path) -> Option<u64> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || super::super::super::unreal::is_link(&meta) {
        return None;
    }
    let nanos = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    if cfg!(windows) {
        // Ninja uses FILETIME units from an epoch near 2000 on Windows.
        // FILETIME starts 11,644,473,600 seconds before the Unix epoch.
        u64::try_from((nanos / 100).checked_sub(10_000_000 * (12_622_770_400 - 11_644_473_600))?).ok()
    } else {
        u64::try_from(nanos).ok()
    }
}
