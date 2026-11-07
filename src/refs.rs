//! Where `refs.commits` and `refs.files` are checked (docs/lore.md 2.4).
//!
//! Without roots it is CodeLore exactly: the repository that holds the store,
//! files at the revision the write is built on. A Zerus project store has
//! reference roots instead - the project's folders on the computer that holds
//! it, the store's own repository first when it is inside one: a commit must
//! exist in one of them and a file at HEAD of one of them. A folder that is not
//! a git repository checks files on disk and verifies no commit.
use crate::gitio;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Roots {
    /// The folders, in the order they are tried.
    pub folders: Vec<PathBuf>,
    /// The computer they are on, for the refusal that names it.
    pub machine: String,
}

impl Roots {
    pub fn new(folders: Vec<PathBuf>, machine: impl Into<String>) -> Self {
        Self { folders, machine: machine.into() }
    }

    fn git_folders(&self) -> Vec<&PathBuf> {
        self.folders.iter().filter(|folder| gitio::head(folder).is_some() || gitio::toplevel(folder).is_some()).collect()
    }
}

/// What the references of a change or of a whole store resolve to.
#[derive(Clone, Debug, Default)]
pub struct Found {
    pub commits: BTreeMap<String, bool>,
    pub files: BTreeMap<String, bool>,
    /// The folder each found file was taken from (several may hold it).
    pub file_roots: BTreeMap<String, PathBuf>,
}

/// One git process per kind and folder, however many references.
pub fn resolve(store_root: &Path, rev: Option<&str>, roots: Option<&Roots>, commits: &[String], files: &[String]) -> Found {
    let Some(roots) = roots else {
        let files_rev = rev.map(str::to_owned).or_else(|| gitio::head(store_root));
        return Found {
            commits: gitio::commits_exist(store_root, commits),
            files: gitio::files_at(store_root, files_rev.as_deref(), files),
            file_roots: BTreeMap::new(),
        };
    };
    let mut found = Found::default();
    let mut commits_left: Vec<String> = commits.to_vec();
    let mut files_left: Vec<String> = files.to_vec();
    for folder in &roots.folders {
        if commits_left.is_empty() && files_left.is_empty() {
            break;
        }
        match gitio::head(folder) {
            Some(head) => {
                let have = gitio::commits_exist(folder, &commits_left);
                commits_left.retain(|commit| !have.get(commit).copied().unwrap_or(false));
                let have = gitio::files_at(folder, Some(&head), &files_left);
                for (file, here) in have {
                    if here {
                        found.file_roots.insert(file.clone(), folder.clone());
                    }
                }
                files_left.retain(|file| !found.file_roots.contains_key(file));
            }
            None => {
                for file in &files_left {
                    if crate::store::safe_relative(Path::new(file)) && folder.join(file).exists() {
                        found.file_roots.insert(file.clone(), folder.clone());
                    }
                }
                files_left.retain(|file| !found.file_roots.contains_key(file));
            }
        }
    }
    let left: BTreeSet<&String> = commits_left.iter().collect();
    found.commits = commits.iter().map(|commit| (commit.clone(), !left.contains(commit))).collect();
    found.files = files.iter().map(|file| (file.clone(), found.file_roots.contains_key(file))).collect();
    found
}

/// The refusal for a commit no root has.
pub fn missing_commit(rid: &str, commit: &str, roots: Option<&Roots>) -> String {
    match roots {
        None => format!("{rid}: commit {commit} is not in this repository"),
        Some(roots) if roots.git_folders().is_empty() => format!("{rid}: no project folder on `{}` is a git repository that has `{commit}`", roots.machine),
        Some(roots) => format!("{rid}: commit {commit} is in no project folder on `{}`", roots.machine),
    }
}

/// The refusal for a file no root holds.
pub fn missing_file(rid: &str, file: &str, rev: Option<&str>, roots: Option<&Roots>) -> String {
    let shown = crate::py::repr_str(file);
    match roots {
        None => format!("{rid}: file {shown} is not in {}", rev.map(|rev| rev.chars().take(10).collect::<String>()).unwrap_or_else(|| "HEAD".into())),
        Some(roots) => format!("{rid}: file {shown} is in no project folder on `{}`", roots.machine),
    }
}

/// The paths each commit touched, from the first root that has it.
pub fn files_touched(store_root: &Path, roots: Option<&Roots>, commits: &[String]) -> BTreeMap<String, BTreeSet<String>> {
    let Some(roots) = roots else { return gitio::files_touched(store_root, commits) };
    let mut result = BTreeMap::new();
    let mut left: Vec<String> = commits.to_vec();
    for folder in roots.git_folders() {
        if left.is_empty() {
            break;
        }
        let touched = gitio::files_touched(folder, &left);
        left.retain(|commit| !touched.contains_key(commit));
        result.extend(touched);
    }
    result
}
