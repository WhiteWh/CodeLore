//! Which files the code map holds, and each one's content key: git's blob id.
//! Tracked files take the id git already has; a file changed in the working
//! tree, or untracked and not ignored, is hashed the way git would. No watcher:
//! every read compares these ids with what the map recorded.
use super::lang::{detect, Lang};
use crate::config::Config;
use crate::LoreError;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub blob: String,
    pub lang: Lang,
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, LoreError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        // A read must never take `index.lock`: `git status` refreshing the
        // index made a commit running beside it fail.
        .arg("--no-optional-locks")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .output()
        .map_err(|error| LoreError::new(format!("atlas: git could not run: {error}")))?;
    if !output.status.success() {
        return Err(LoreError::new(format!("atlas: git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim())));
    }
    Ok(output.stdout)
}

fn split_z(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes.split(|b| *b == 0).filter(|part| !part.is_empty()).map(|part| String::from_utf8_lossy(part).into_owned())
}

/// How the working tree differs from git's index.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    /// Tracked, gone from the working tree.
    pub deleted: BTreeSet<String>,
    /// Tracked, its working copy differs from the index (or unmerged).
    pub modified: BTreeSet<String>,
    /// Not tracked and not ignored, file by file.
    pub untracked: Vec<String>,
}

/// One `git status` instead of `ls-files -d`, `-m` and `-o`: each of those
/// walks the whole working tree on its own (0.9, 1.4 and 0.35 s on a
/// 6 000-file checkout - most of every `lore atlas` call), status walks it once
/// and refreshes the index's stat cache as it goes.
fn worktree_changes(root: &Path) -> Result<Changes, LoreError> {
    Ok(status(root)?.1)
}

/// `git status` with the branch header (HEAD's commit), raw and parsed.
pub fn status(root: &Path) -> Result<(Vec<u8>, Changes), LoreError> {
    let raw = git(root, &["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all", "--ignore-submodules=all", "--no-renames"])?;
    let changes = parse_status(&raw);
    Ok((raw, changes))
}

/// The modification time of `path` in nanoseconds and its size, or zeros.
fn stamp(path: &Path) -> String {
    std::fs::metadata(path).map_or_else(
        |_| "-".to_owned(),
        |meta| format!("{}:{}", meta.len(), meta.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |since| since.as_nanos())),
    )
}

/// git's index file of the repository at `root` (`.git` may be a file in a worktree).
fn index_file(root: &Path) -> std::path::PathBuf {
    let dot_git = root.join(".git");
    if let Ok(text) = std::fs::read_to_string(&dot_git) {
        if let Some(dir) = text.trim().strip_prefix("gitdir:") {
            let dir = Path::new(dir.trim());
            return if dir.is_absolute() { dir.join("index") } else { root.join(dir).join("index") };
        }
    }
    dot_git.join("index")
}

/// A cheap print of everything the map is built from, taken without walking
/// the whole tree for new files: git's index file (an add, a commit, a
/// checkout or a merge rewrites it), `git diff-files` (tracked files whose
/// working copy differs from the index) with each such file's size and time,
/// and the modification time of every folder the last scan saw (a file added,
/// removed or renamed in a folder changes the folder's time). Equal prints
/// mean a scan would find nothing new. On a 6 800-file checkout this is
/// ~0.1 s where `git status` took ~0.5 s.
pub fn quick_print(root: &Path, settings: &str, dirs: &BTreeSet<String>, untracked: &[String]) -> Result<String, LoreError> {
    let mut hasher = Sha1::new();
    hasher.update(settings.as_bytes());
    hasher.update(stamp(&index_file(root)).as_bytes());
    let changed = git(root, &["diff-files", "--name-only", "-z", "--ignore-submodules=all"])?;
    hasher.update(&changed);
    for path in split_z(&changed) {
        hasher.update(stamp(&root.join(&path)).as_bytes());
    }
    for dir in dirs {
        hasher.update(dir.as_bytes());
        hasher.update(stamp(&if dir.is_empty() { root.to_path_buf() } else { root.join(dir) }).as_bytes());
    }
    // An untracked file edited in place changes neither git's index nor its
    // folder's time: its own size and time are taken, file by file.
    for path in untracked {
        hasher.update(path.as_bytes());
        hasher.update(stamp(&root.join(path)).as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// `git status --porcelain=v2 -z --no-renames` output: `1 XY ... <path>` for a
/// changed entry (Y is the working tree side), `u ... <path>` unmerged, `? <path>`
/// untracked. Paths are relative to the repository root and carry spaces.
pub fn parse_status(raw: &[u8]) -> Changes {
    let mut changes = Changes::default();
    let mut records = raw.split(|b| *b == 0).filter(|part| !part.is_empty()).map(|part| String::from_utf8_lossy(part).into_owned());
    while let Some(record) = records.next() {
        let path_after = |fields: usize| record.splitn(fields + 1, ' ').nth(fields).map(str::to_owned);
        match record.as_bytes().first() {
            Some(b'1') => {
                // 1 XY sub mH mI mW hH hI path
                let Some(path) = path_after(8) else { continue };
                match record.as_bytes().get(3) {
                    Some(b'D') => {
                        changes.deleted.insert(path);
                    }
                    Some(b'.') | None => {}
                    Some(_) => {
                        changes.modified.insert(path);
                    }
                }
            }
            Some(b'2') => {
                // a rename (only without --no-renames): the original path follows
                if let Some(path) = path_after(9) {
                    changes.modified.insert(path);
                }
                records.next();
            }
            Some(b'u') => {
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                if let Some(path) = path_after(10) {
                    changes.modified.insert(path);
                }
            }
            Some(b'?') => changes.untracked.push(record[2..].to_owned()),
            _ => {}
        }
    }
    changes
}

/// git's id of a blob with these bytes.
pub fn blob_id(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// `*` matches within a path part, `**` across parts; a pattern without one is
/// a path prefix.
pub fn excluded(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim().trim_start_matches("./").replace('\\', "/");
        if pattern.is_empty() {
            return false;
        }
        if !pattern.contains('*') {
            let prefix = pattern.trim_end_matches('/');
            return path == prefix || path.starts_with(&format!("{prefix}/"));
        }
        glob(&pattern, path)
    })
}

fn glob(pattern: &str, text: &str) -> bool {
    let mut regex = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                regex.push_str(".*");
                i += 1;
            }
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            ch => regex.push_str(&regex::escape(&ch.to_string())),
        }
        i += 1;
    }
    regex.push('$');
    regex::Regex::new(&regex).is_ok_and(|re| re.is_match(text))
}

fn language(path: &str, include: &BTreeSet<String>) -> Option<Lang> {
    detect(path).or_else(|| {
        let ext = path.rsplit('/').next()?.rsplit_once('.')?.1.to_ascii_lowercase();
        include.contains(&ext).then_some(Lang::Text)
    })
}

/// The files of the repository the map holds, by path.
pub fn scan(config: &Config) -> Result<BTreeMap<String, Entry>, LoreError> {
    Ok(scan_with(config, &worktree_changes(&config.root)?)?.0)
}

/// [`scan`] with the working tree's changes already read; also every folder
/// that holds a tracked or untracked file, and their parents (see [`quick_print`]).
pub fn scan_with(config: &Config, changes: &Changes) -> Result<(BTreeMap<String, Entry>, BTreeSet<String>), LoreError> {
    let root = &config.root;
    let include: BTreeSet<String> = config.atlas.include.iter().map(|ext| ext.trim_start_matches('.').to_ascii_lowercase()).collect();
    let lore_dir = config.dir.trim_matches('/').to_owned();
    let max_bytes = config.atlas.max_kib * 1024;
    let wanted = |path: &str| -> Option<Lang> {
        if path.starts_with(&format!("{lore_dir}/")) || excluded(path, &config.atlas.exclude) {
            return None;
        }
        if path.split('/').any(|part| matches!(part, "node_modules" | "target" | ".git" | "dist" | "build" | "__pycache__" | ".venv" | "vendor")) {
            return None;
        }
        language(path, &include)
    };
    let mut out = BTreeMap::new();
    let mut dirs: BTreeSet<String> = BTreeSet::from([String::new()]);
    let mut add_dirs = |path: &str| {
        let mut at = path;
        while let Some((parent, _)) = at.rsplit_once('/') {
            if !dirs.insert(parent.to_owned()) {
                break;
            }
            at = parent;
        }
    };
    let (deleted, modified) = (&changes.deleted, &changes.modified);
    for line in split_z(&git(root, &["ls-files", "-s", "-z"])?) {
        let Some((meta, path)) = line.split_once('\t') else { continue };
        add_dirs(path);
        let mut parts = meta.split_whitespace();
        let (Some(mode), Some(blob)) = (parts.next(), parts.next()) else { continue };
        if mode == "160000" || mode == "120000" || deleted.contains(path) || modified.contains(path) {
            continue;
        }
        let Some(lang) = wanted(path) else { continue };
        let size = std::fs::metadata(root.join(path)).map(|m| m.len()).unwrap_or(0);
        if size == 0 || size > max_bytes {
            continue;
        }
        out.insert(path.to_owned(), Entry { path: path.to_owned(), blob: blob.to_owned(), lang });
    }
    for path in &changes.untracked {
        add_dirs(path);
    }
    for path in changes.modified.iter().chain(&changes.untracked).cloned() {
        let Some(lang) = wanted(&path) else { continue };
        let Ok(bytes) = std::fs::read(root.join(&path)) else { continue };
        if bytes.is_empty() || bytes.len() as u64 > max_bytes {
            continue;
        }
        out.insert(path.clone(), Entry { blob: blob_id(&bytes), path, lang });
    }
    Ok((out, dirs))
}

/// A file's text when it is text: NUL in its first 8 KiB means binary. CRLF
/// becomes LF so line numbers and chunks do not depend on checkout settings.
pub fn read_text(root: &Path, path: &str) -> Option<String> {
    let bytes = std::fs::read(root.join(path)).ok()?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    Some(text.replace("\r\n", "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_says_what_changed_in_the_working_tree() {
        let raw = b"1 .M N... 100644 100644 100644 aaa aaa src/a b.rs\x001 .D N... 100644 100644 000000 bbb bbb src/gone.py\x00\
1 M. N... 100644 100644 100644 ccc ddd src/staged.rs\x00u UU N... 100644 100644 100644 100644 e1 e2 e3 src/merge.rs\x00\
? new dir/x.ts\x00";
        let changes = parse_status(raw);
        assert_eq!(changes.modified, BTreeSet::from(["src/a b.rs".to_owned(), "src/merge.rs".to_owned()]));
        assert_eq!(changes.deleted, BTreeSet::from(["src/gone.py".to_owned()]));
        assert_eq!(changes.untracked, vec!["new dir/x.ts".to_owned()]);
    }

    /// The same answer as the three `ls-files` walks it replaces, on a real repository.
    #[test]
    fn status_agrees_with_ls_files() {
        let dir = std::env::temp_dir().join(format!("lore-atlas-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let run = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap().status.success(), "{args:?}");
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.invalid"]);
        run(&["config", "user.name", "t"]);
        for name in ["a.rs", "b c.rs", "gone.rs", "staged.rs"] {
            std::fs::write(dir.join("src").join(name), "fn x() {}\n").unwrap();
        }
        run(&["add", "-A"]);
        run(&["commit", "-qm", "one"]);
        std::fs::write(dir.join("src/b c.rs"), "fn y() {}\n").unwrap();
        std::fs::remove_file(dir.join("src/gone.rs")).unwrap();
        std::fs::write(dir.join("src/staged.rs"), "fn z() {}\n").unwrap();
        run(&["add", "src/staged.rs"]);
        std::fs::write(dir.join("src/new.rs"), "fn n() {}\n").unwrap();
        let changes = worktree_changes(&dir).unwrap();
        let listed = |args: &[&str]| -> BTreeSet<String> { split_z(&git(&dir, args).unwrap()).collect() };
        let deleted = listed(&["ls-files", "-d", "-z"]);
        assert_eq!(changes.deleted, deleted);
        assert_eq!(changes.modified, listed(&["ls-files", "-m", "-z"]).difference(&deleted).cloned().collect());
        assert_eq!(changes.untracked.iter().cloned().collect::<BTreeSet<_>>(), listed(&["ls-files", "-o", "--exclude-standard", "-z"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blob_ids_are_gits() {
        use std::io::Write;
        let mut child = Command::new("git").args(["hash-object", "--stdin"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(blob_id(b"hello\n"), String::from_utf8_lossy(&output.stdout).trim());
    }

    #[test]
    fn exclusions_are_prefixes_or_globs() {
        let patterns = vec!["third_party".to_owned(), "**/*.generated.cs".to_owned(), "src/*.min.js".to_owned()];
        assert!(excluded("third_party/zlib/zlib.h", &patterns));
        assert!(!excluded("third_party_tools/a.rs", &patterns));
        assert!(excluded("a/b/c.generated.cs", &patterns));
        assert!(excluded("src/app.min.js", &patterns));
        assert!(!excluded("src/sub/app.min.js", &patterns));
    }
}
