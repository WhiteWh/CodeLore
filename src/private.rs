//! A store that is its own private git repository (docs/lore.md 2.5): the
//! default store under Zerus's data, or a folder the owner chose outside any
//! repository. Zerus runs `git init` only in such a folder - never in a folder
//! inside another working tree, so a project folder is never initialized.
//! Writes commit as CodeLore's `plain` mode does, with no remote and no push,
//! and the commit's author is the writer (`by`).
use crate::config::{self, Config};
use crate::gitio::{self, Identity};
use crate::{LoreError, Result};
use std::path::Path;

/// The marker in the repository's git folder that says Zerus made it.
const MARKER: &str = "zerus-lore-store";
/// The address of every commit a private store makes (a reserved domain).
pub const EMAIL: &str = "lore@zerus.invalid";

/// Creates the store in `folder` (made when missing, otherwise empty) with its
/// areas, and commits lore.toml and .gitattributes as its first commit.
pub fn create(folder: &Path, areas: &[String]) -> Result<Config> {
    if folder.join(config::CONFIG_NAME).exists() {
        return Err(LoreError::new(format!("{} exists already", folder.join(config::CONFIG_NAME).display())));
    }
    if let Some(parent) = existing_ancestor(folder) {
        if let Some(top) = gitio::toplevel(&parent) {
            return Err(LoreError::new(format!(
                "{} is inside the git working tree {}; a private store is never made in a project folder",
                folder.display(),
                top.display()
            )));
        }
    }
    if folder.is_dir() && std::fs::read_dir(folder).map(|mut entries| entries.next().is_some()).unwrap_or(false) {
        return Err(LoreError::new(format!("{} is not empty; a private store starts in an empty folder", folder.display())));
    }
    std::fs::create_dir_all(folder.join("lore").join("plans")).map_err(|error| LoreError::new(format!("{}: {error}", folder.display())))?;
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "core.autocrlf", "false"],
        &["config", "user.name", "Zerus"],
        &["config", "user.email", EMAIL],
    ] {
        gitio::git(folder, args, None, &[], true)?;
    }
    let git_dir = gitio::out(folder, &["rev-parse", "--git-dir"], None, &[])?;
    let git_dir = folder.join(git_dir);
    std::fs::write(git_dir.join(MARKER), "a RustCodeLore store Zerus made; commits carry the writer\n").map_err(|error| LoreError::new(format!("{}: {error}", git_dir.display())))?;
    let text = config::template("lore", areas, "plain", "main");
    let data: toml::Value = toml::from_str(&text).map_err(|error| LoreError::new(error.to_string()))?;
    let config = config::parse(&data, folder, config::CONFIG_NAME)?;
    std::fs::write(folder.join(config::CONFIG_NAME), text).map_err(|error| LoreError::new(error.to_string()))?;
    std::fs::write(folder.join(".gitattributes"), "lore/** text eol=lf\n").map_err(|error| LoreError::new(error.to_string()))?;
    gitio::git(folder, &["add", "--", config::CONFIG_NAME, ".gitattributes"], None, &[], true)?;
    let message = gitio::message("lore: a new store", "", &[]);
    gitio::git(folder, &["commit", "-q", "-F", "-"], Some(&message), &[], true)?;
    Ok(config)
}

fn existing_ancestor(folder: &Path) -> Option<std::path::PathBuf> {
    let mut cursor = Some(folder);
    while let Some(path) = cursor {
        if path.is_dir() {
            return Some(path.to_path_buf());
        }
        cursor = path.parent();
    }
    None
}

/// Whether `root` is a store Zerus made as its own repository.
pub fn is_private(root: &Path) -> bool {
    let Ok(git_dir) = gitio::out(root, &["rev-parse", "--git-dir"], None, &[]) else { return false };
    let top = gitio::toplevel(root).map(|top| config::simplify(&std::fs::canonicalize(&top).unwrap_or(top)));
    let here = config::simplify(&std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()));
    top.as_deref() == Some(here.as_path()) && root.join(git_dir).join(MARKER).is_file()
}

/// The author of a write to `root`: the writer by name in a private store,
/// None (git's own identity) anywhere else.
pub fn identity(root: &Path, by: &str) -> Option<Identity> {
    is_private(root).then(|| Identity { name: by.to_owned(), email: EMAIL.to_owned() })
}
