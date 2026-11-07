//! git, through its plumbing (CodeLore's `gitio.py`). The three commit modes
//! live here.
//!
//! `shared-tree` is for a working tree several sessions edit at once: the
//! change is applied to each file's blob at the branch's tip, never to the live
//! file, the tree is built in a private index, and the branch moves only if
//! nobody moved it in between.
use crate::{LoreError, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }
}

thread_local! {
    /// Every git command's first argument, for tests that count processes.
    static CALLS: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// Runs git in `root`. With `check`, a failure is a refusal naming the command.
pub fn git(root: &Path, args: &[&str], input: Option<&[u8]>, env: &[(&str, &str)], check: bool) -> Result<Output> {
    CALLS.with(|calls| {
        if let Some(calls) = calls.borrow_mut().as_mut() {
            calls.push(args.first().copied().unwrap_or_default().to_owned());
        }
    });
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args).stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|error| LoreError::new(format!("git {} failed: {error}", args.iter().take(3).copied().collect::<Vec<_>>().join(" "))))?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let data = input.to_vec();
        // A writer thread, so a large input never deadlocks against git's output.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
        let output = child.wait_with_output().map_err(|error| LoreError::new(format!("git failed: {error}")))?;
        let _ = writer.join();
        return finish(args, output, check);
    }
    let output = child.wait_with_output().map_err(|error| LoreError::new(format!("git failed: {error}")))?;
    finish(args, output, check)
}

fn finish(args: &[&str], output: std::process::Output, check: bool) -> Result<Output> {
    let result = Output { code: output.status.code().unwrap_or(-1), stdout: output.stdout, stderr: output.stderr };
    if check && !result.ok() {
        let stderr = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        let message = if stderr.is_empty() { String::from_utf8_lossy(&result.stdout).trim().to_owned() } else { stderr };
        return Err(LoreError::new(format!("git {} failed: {message}", args.iter().take(3).copied().collect::<Vec<_>>().join(" "))));
    }
    Ok(result)
}

/// Counts git processes by their first argument while `body` runs.
pub fn count_calls<T>(body: impl FnOnce() -> T) -> (T, Vec<String>) {
    CALLS.with(|calls| *calls.borrow_mut() = Some(Vec::new()));
    let value = body();
    let calls = CALLS.with(|calls| calls.borrow_mut().take().unwrap_or_default());
    (value, calls)
}

pub fn out(root: &Path, args: &[&str], input: Option<&[u8]>, env: &[(&str, &str)]) -> Result<String> {
    Ok(git(root, args, input, env, true)?.text())
}

pub fn toplevel(start: &Path) -> Option<PathBuf> {
    let output = git(start, &["rev-parse", "--show-toplevel"], None, &[], false).ok()?;
    output.ok().then(|| PathBuf::from(output.text()))
}

pub fn head(root: &Path) -> Option<String> {
    let output = git(root, &["rev-parse", "--verify", "-q", "HEAD^{commit}"], None, &[], false).ok()?;
    Some(output.text()).filter(|text| !text.is_empty())
}

pub fn reference(root: &Path, name: &str) -> Option<String> {
    let spec = format!("{name}^{{commit}}");
    let output = git(root, &["rev-parse", "--verify", "-q", &spec], None, &[], false).ok()?;
    Some(output.text()).filter(|text| !text.is_empty())
}

pub fn blob(root: &Path, rev: Option<&str>, path: &str) -> Option<Vec<u8>> {
    let spec = format!("{}:{path}", rev?);
    let output = git(root, &["cat-file", "blob", &spec], None, &[], false).ok()?;
    output.ok().then_some(output.stdout)
}

pub fn ls_tree(root: &Path, rev: Option<&str>, directory: &str) -> Vec<String> {
    let Some(rev) = rev else { return Vec::new() };
    let folder = format!("{directory}/");
    match git(root, &["ls-tree", "-r", "--name-only", rev, "--", &folder], None, &[], false) {
        Ok(output) => String::from_utf8_lossy(&output.stdout).lines().filter(|line| !line.is_empty()).map(str::to_owned).collect(),
        Err(_) => Vec::new(),
    }
}

/// Each object name (`<sha>^{commit}`, `<rev>:<path>`) to the full hash it
/// names, or None - one `git cat-file` for all of them.
pub fn resolve(root: &Path, specs: &[String]) -> BTreeMap<String, Option<String>> {
    let mut unique: Vec<&String> = Vec::new();
    for spec in specs.iter().filter(|spec| !spec.contains('\n')) {
        if !unique.contains(&spec) {
            unique.push(spec);
        }
    }
    let mut result = BTreeMap::new();
    if unique.is_empty() {
        return result;
    }
    let input: String = unique.iter().map(|spec| format!("{spec}\n")).collect();
    let lines: Vec<String> = match git(root, &["cat-file", "--batch-check=%(objectname)"], Some(input.as_bytes()), &[], false) {
        Ok(output) => String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect(),
        Err(_) => Vec::new(),
    };
    for (index, spec) in unique.iter().enumerate() {
        let found = lines.get(index).map(|line| line.trim().to_owned()).filter(|line| !line.contains(' ') && !line.is_empty());
        result.insert((*spec).clone(), found);
    }
    result
}

pub fn commits_exist(root: &Path, shas: &[String]) -> BTreeMap<String, bool> {
    let specs: Vec<String> = shas.iter().map(|sha| format!("{sha}^{{commit}}")).collect();
    let found = resolve(root, &specs);
    shas.iter().map(|sha| (sha.clone(), found.get(&format!("{sha}^{{commit}}")).is_some_and(Option::is_some))).collect()
}

pub fn files_at(root: &Path, rev: Option<&str>, paths: &[String]) -> BTreeMap<String, bool> {
    let Some(rev) = rev else { return paths.iter().map(|path| (path.clone(), false)).collect() };
    let specs: Vec<String> = paths.iter().map(|path| format!("{rev}:{path}")).collect();
    let found = resolve(root, &specs);
    paths.iter().map(|path| (path.clone(), found.get(&format!("{rev}:{path}")).is_some_and(Option::is_some))).collect()
}

/// The paths each commit changed against its parents (all of them for a root
/// commit), by the hash as given. Commits that do not exist are absent.
pub fn files_touched(root: &Path, shas: &[String]) -> BTreeMap<String, BTreeSet<String>> {
    let specs: Vec<String> = shas.iter().map(|sha| format!("{sha}^{{commit}}")).collect();
    let names = resolve(root, &specs);
    let full: BTreeMap<&String, Option<String>> = shas.iter().map(|sha| (sha, names.get(&format!("{sha}^{{commit}}")).cloned().flatten())).collect();
    let wanted: BTreeSet<String> = full.values().flatten().cloned().collect();
    let mut touched: BTreeMap<String, BTreeSet<String>> = wanted.iter().map(|hash| (hash.clone(), BTreeSet::new())).collect();
    if !wanted.is_empty() {
        let input: String = wanted.iter().map(|hash| format!("{hash}\n")).collect();
        if let Ok(output) = git(root, &["diff-tree", "--stdin", "-r", "-m", "--root", "--name-only", "--no-renames"], Some(input.as_bytes()), &[], false) {
            let mut current: Option<String> = None;
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if touched.contains_key(line) {
                    current = Some(line.to_owned());
                } else if let (false, Some(current)) = (line.is_empty(), &current) {
                    touched.get_mut(current).map(|set| set.insert(line.to_owned()));
                }
            }
        }
    }
    full.into_iter().filter_map(|(sha, hash)| hash.map(|hash| (sha.clone(), touched.get(&hash).cloned().unwrap_or_default()))).collect()
}

pub fn user_name(root: &Path) -> String {
    git(root, &["config", "user.name"], None, &[], false).map(|output| output.text()).unwrap_or_default()
}

pub fn current_branch(root: &Path) -> Option<String> {
    let output = git(root, &["symbolic-ref", "-q", "HEAD"], None, &[], false).ok()?;
    output.text().strip_prefix("refs/heads/").map(str::to_owned)
}

pub fn touched_since(root: &Path, since: &str, paths: &[String]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let since = format!("--since={since}");
    let mut args = vec!["log", "-1", "--format=%H", since.as_str(), "--"];
    args.extend(paths.iter().map(String::as_str));
    git(root, &args, None, &[], false).is_ok_and(|output| !output.text().is_empty())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Written {
    pub commit: Option<String>,
    pub notes: Vec<String>,
}

/// The files' old bytes and the revision they were read at (None: the working
/// tree), to the new bytes of every file that changes.
pub type Transform<'a> = dyn FnMut(&BTreeMap<String, Option<Vec<u8>>>, Option<&str>) -> Result<BTreeMap<String, Vec<u8>>> + 'a;

/// The commit's author, when the store is Zerus's own private repository
/// (`private.rs`): the writer by name.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

fn identity_env(identity: Option<&Identity>) -> Vec<(&'static str, String)> {
    match identity {
        None => Vec::new(),
        Some(identity) => vec![
            ("GIT_AUTHOR_NAME", identity.name.clone()),
            ("GIT_AUTHOR_EMAIL", identity.email.clone()),
            ("GIT_COMMITTER_NAME", identity.name.clone()),
            ("GIT_COMMITTER_EMAIL", identity.email.clone()),
        ],
    }
}

pub fn message(subject: &str, body: &str, trailers: &[String]) -> Vec<u8> {
    let mut text = format!("{}\n", subject.trim());
    if !body.trim().is_empty() {
        text.push_str(&format!("\n{}\n", body.trim()));
    }
    if !trailers.is_empty() {
        text.push_str(&format!("\n{}\n", trailers.join("\n")));
    }
    text.into_bytes()
}

pub fn write_none(root: &Path, paths: &[String], transform: &mut Transform) -> Result<Written> {
    let old: BTreeMap<String, Option<Vec<u8>>> = paths.iter().map(|path| (path.clone(), read_live(root, path))).collect();
    let new = transform(&old, None)?;
    for (path, data) in &new {
        write_or_refuse(root, path, data)?;
    }
    Ok(Written { commit: None, notes: vec!["written to the working tree; not committed (commit.mode = none)".into()] })
}

pub fn write_plain(root: &Path, paths: &[String], transform: &mut Transform, subject: &str, body: &str, trailers: &[String], identity: Option<&Identity>) -> Result<Written> {
    let old: BTreeMap<String, Option<Vec<u8>>> = paths.iter().map(|path| (path.clone(), read_live(root, path))).collect();
    let new = transform(&old, None)?;
    if new.is_empty() {
        return Ok(Written { commit: None, notes: vec!["nothing changed".into()] });
    }
    for (path, data) in &new {
        write_or_refuse(root, path, data)?;
    }
    let changed: Vec<&str> = new.keys().map(String::as_str).collect();
    let mut add = vec!["add", "--"];
    add.extend(&changed);
    git(root, &add, None, &[], true)?;
    let text = message(subject, body, trailers);
    let mut commit = vec!["commit", "--only", "-q", "-F", "-", "--"];
    commit.extend(&changed);
    let env = identity_env(identity);
    let env: Vec<(&str, &str)> = env.iter().map(|(key, value)| (*key, value.as_str())).collect();
    let output = git(root, &commit, Some(&text), &env, false)?;
    if !output.ok() {
        let err = if output.stderr.is_empty() { &output.stdout } else { &output.stderr };
        return Err(LoreError::new(format!("the commit failed; the change is in the working tree: {}", String::from_utf8_lossy(err).trim())));
    }
    Ok(Written { commit: head(root), notes: Vec::new() })
}

/// The branch's blobs plus this change, in a private index, moved by
/// compare-and-swap. `before_update` runs once before the first move (tests
/// use it to move the branch underneath).
#[allow(clippy::too_many_arguments)]
pub fn write_shared(
    root: &Path,
    branch: &str,
    paths: &[String],
    transform: &mut Transform,
    subject: &str,
    body: &str,
    trailers: &[String],
    tries: usize,
    mut before_update: Option<&mut dyn FnMut()>,
    identity: Option<&Identity>,
) -> Result<Written> {
    let current = current_branch(root);
    if current.as_deref() != Some(branch) {
        let at = match &current {
            None => "detached".to_owned(),
            Some(name) => format!("`{name}`"),
        };
        return Err(LoreError::new(format!("commit.mode is shared-tree on `{branch}`, but HEAD is {at} - refusing to move a branch the working tree is not on")));
    }
    let mut notes = Vec::new();
    let text = message(subject, body, trailers);
    let mut last_err = String::new();
    let env = identity_env(identity);
    let env: Vec<(&str, &str)> = env.iter().map(|(key, value)| (*key, value.as_str())).collect();
    for attempt in 0..tries {
        let base = reference(root, &format!("refs/heads/{branch}"));
        let old: BTreeMap<String, Option<Vec<u8>>> = paths.iter().map(|path| (path.clone(), blob(root, base.as_deref(), path))).collect();
        let new = transform(&old, base.as_deref())?;
        if new.is_empty() {
            return Ok(Written { commit: None, notes: vec!["nothing changed".into()] });
        }
        let index = private_index_path();
        let index_text = index.to_string_lossy().into_owned();
        let built = (|| -> Result<String> {
            let index_env = [("GIT_INDEX_FILE", index_text.as_str())];
            if let Some(base) = &base {
                git(root, &["read-tree", base], None, &index_env, true)?;
            }
            for (path, data) in &new {
                let sha = out(root, &["hash-object", "-w", "--no-filters", "--stdin"], Some(data), &[])?;
                let info = format!("100644,{sha},{path}");
                git(root, &["update-index", "--add", "--cacheinfo", &info], None, &index_env, true)?;
            }
            out(root, &["write-tree"], None, &index_env)
        })();
        let _ = std::fs::remove_file(&index);
        let tree = built?;
        let mut args = vec!["commit-tree".to_owned(), tree];
        if let Some(base) = &base {
            args.push("-p".into());
            args.push(base.clone());
        }
        args.push("-F".into());
        args.push("-".into());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let commit = out(root, &args, Some(&text), &env)?;
        if let Some(hook) = before_update.take() {
            hook();
        }
        let target = format!("refs/heads/{branch}");
        let old_value = base.clone().unwrap_or_else(|| "0".repeat(40));
        let update = git(root, &["update-ref", &target, &commit, &old_value], None, &[], false)?;
        if update.ok() {
            notes.extend(sync_live(root, &old, &new)?);
            return Ok(Written { commit: Some(commit), notes });
        }
        last_err = String::from_utf8_lossy(&update.stderr).trim().to_owned();
        notes.push(format!("`{branch}` moved while writing (try {}); built again on the new head", attempt + 1));
    }
    Err(LoreError::new(format!("`{branch}` kept moving; nothing was committed after {tries} tries: {last_err}")))
}

fn private_index_path() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_nanos());
    std::env::temp_dir().join(format!("lore-index-{}-{stamp}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)))
}

/// Bring each live file, and its entry in the shared index, to the new blob -
/// only where they still held the old one, or an older version of the same
/// file (a copy an earlier sync could not replace). Anything else is somebody's
/// edit.
fn sync_live(root: &Path, old: &BTreeMap<String, Option<Vec<u8>>>, new: &BTreeMap<String, Vec<u8>>) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    for (path, data) in new {
        let live = read_live(root, path);
        let before = old.get(path).cloned().flatten();
        let flat = live.as_ref().map(|bytes| crlf_to_lf(bytes));
        let same = (live.is_none() && before.is_none()) || (live.is_some() && before.is_some() && flat == before);
        let lagging = !same && flat.as_ref().is_some_and(|flat| is_past_version(root, path, flat));
        if !(same || lagging) {
            notes.push(format!("{path}: the working tree holds an edit that is not in HEAD; left as it was - the commit has the change, the live file does not"));
            continue;
        }
        if !write_live(root, path, data) {
            notes.push(format!("{path}: another process holds the live file, so it could not be replaced; the commit has the change, and the next write brings the file up to date"));
            continue;
        }
        refresh_index_entry(root, path, before.as_deref(), data, if lagging { flat.as_deref() } else { None })?;
        if lagging {
            notes.push(format!("{path}: the live file was an older version of itself; brought up to date"));
        }
    }
    Ok(notes)
}

fn crlf_to_lf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            index += 1;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

/// Whether `data` is what `path` held at one of its last `depth` commits.
fn is_past_version(root: &Path, path: &str, data: &[u8]) -> bool {
    let Ok(sha) = out(root, &["hash-object", "--no-filters", "--stdin"], Some(data), &[]) else { return false };
    let Ok(output) = git(root, &["log", "-n500", "--format=", "--raw", "--no-abbrev", "HEAD", "--", path], None, &[], false) else { return false };
    String::from_utf8_lossy(&output.stdout).lines().filter(|line| line.starts_with(':')).any(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        parts.get(2) == Some(&sha.as_str()) || parts.get(3) == Some(&sha.as_str())
    })
}

fn refresh_index_entry(root: &Path, path: &str, before: Option<&[u8]>, data: &[u8], also: Option<&[u8]>) -> Result<()> {
    let listed = git(root, &["ls-files", "-s", "--", path], None, &[], false)?.text();
    let mut ours: BTreeSet<Option<String>> = BTreeSet::new();
    if before.is_none() {
        ours.insert(None);
    }
    for bytes in [before, also].into_iter().flatten() {
        ours.insert(Some(out(root, &["hash-object", "--no-filters", "--stdin"], Some(bytes), &[])?));
    }
    let staged = listed.split_whitespace().nth(1).map(str::to_owned);
    if !ours.contains(&staged) {
        return Ok(()); // somebody staged something there
    }
    let sha = out(root, &["hash-object", "--no-filters", "--stdin"], Some(data), &[])?;
    let info = format!("100644,{sha},{path}");
    git(root, &["update-index", "--add", "--cacheinfo", &info], None, &[], false)?;
    Ok(())
}

pub fn push(root: &Path, remote: &str, branch: &str) -> Option<String> {
    match git(root, &["push", "-q", remote, branch], None, &[], false) {
        Ok(output) if output.ok() => None,
        Ok(output) => {
            let err = if output.stderr.is_empty() { output.stdout } else { output.stderr };
            Some(String::from_utf8_lossy(&err).trim().to_owned())
        }
        Err(error) => Some(error.to_string()),
    }
}

pub fn read_live(root: &Path, relative: &str) -> Option<Vec<u8>> {
    let path = root.join(relative);
    if path.is_file() { std::fs::read(path).ok() } else { None }
}

/// About four seconds in all.
const REPLACE_TRIES: u32 = 12;

thread_local! {
    /// Tests: every replace of a live file fails as a held file does on
    /// Windows, without the waits (CodeLore's tests mock `os.replace`).
    static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// While `body` runs, every live file counts as held by another process.
pub fn with_held_files<T>(body: impl FnOnce() -> T) -> T {
    HELD.with(|held| held.set(true));
    let value = body();
    HELD.with(|held| held.set(false));
    value
}

/// Writes a live file whole through a temporary beside it. On Windows a file
/// another process has open cannot be replaced; that is retried, and if it
/// never frees, the temporary goes and false says the file was left as it was.
pub fn write_live(root: &Path, relative: &str, data: &[u8]) -> bool {
    let path = root.join(relative);
    if data.is_empty() {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        return true;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = path.with_file_name(format!("{name}.lore-tmp"));
    if std::fs::write(&temp, data).is_err() {
        return false;
    }
    let held = HELD.with(std::cell::Cell::get);
    for attempt in 0..REPLACE_TRIES {
        if !held && std::fs::rename(&temp, &path).is_ok() {
            return true;
        }
        if !held {
            std::thread::sleep(std::time::Duration::from_millis(50 * u64::from(attempt + 1)));
        }
    }
    let _ = std::fs::remove_file(&temp);
    false
}

fn write_or_refuse(root: &Path, relative: &str, data: &[u8]) -> Result<()> {
    if write_live(root, relative, data) {
        Ok(())
    } else {
        Err(LoreError::new(format!("{relative}: another process holds the file, so it could not be replaced; nothing was committed - try again when it is free")))
    }
}
