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
    let deleted: BTreeSet<String> = split_z(&git(root, &["ls-files", "-d", "-z"])?).collect();
    let modified: BTreeSet<String> = split_z(&git(root, &["ls-files", "-m", "-z"])?).filter(|path| !deleted.contains(path)).collect();
    for line in split_z(&git(root, &["ls-files", "-s", "-z"])?) {
        let Some((meta, path)) = line.split_once('\t') else { continue };
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
    let untracked = split_z(&git(root, &["ls-files", "-o", "--exclude-standard", "-z"])?).collect::<Vec<_>>();
    for path in modified.into_iter().chain(untracked) {
        let Some(lang) = wanted(&path) else { continue };
        let Ok(bytes) = std::fs::read(root.join(&path)) else { continue };
        if bytes.is_empty() || bytes.len() as u64 > max_bytes {
            continue;
        }
        out.insert(path.clone(), Entry { blob: blob_id(&bytes), path, lang });
    }
    Ok(out)
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
