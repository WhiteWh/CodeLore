//! The installed engine is a filesystem corpus shared by projects on this machine.
use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::lang::{detect, Lang};
use super::store::{ATLAS_VERSION, FILE_NAME};
use crate::LoreError;

// Bump when the engine's scan policy, symbol extraction or chunk format changes.
const PROFILE: &str = "unreal-source-v1";
const MAX_BYTES: u64 = 512 * 1024;
const BUILD_FILE: &str = "Engine/Build/Build.version";

pub(super) fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn io_error(path: &Path, error: impl std::fmt::Display) -> LoreError {
    LoreError::new(format!("{}: {error}", path.display()))
}

fn metadata(path: &Path) -> Result<Metadata, LoreError> {
    fs::symlink_metadata(path).map_err(|error| io_error(path, error))
}

pub(super) fn is_link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // A junction need not report itself as a symlink.
        const REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    meta.is_symlink()
}

fn required_dir(root: &Path, relative: &str) -> Result<(), LoreError> {
    let path = root.join(relative);
    let meta = metadata(&path)?;
    if !meta.is_dir() || is_link(&meta) {
        return Err(io_error(&path, "expected a directory inside the Unreal installation"));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(super) struct Unreal {
    pub root: PathBuf,
    pub source_id: String,
    pub build: Value,
    pub cache_key: String,
    pub path: PathBuf,
}

impl Unreal {
    pub fn resolve(configured: &Path) -> Result<Self, LoreError> {
        let root = fs::canonicalize(configured).map_err(|error| io_error(configured, error))?;
        let build = Self::build_at(&root)?;
        let source_id = format!("ue:{}", digest(root.as_os_str().as_encoded_bytes()));
        let identity = json!([source_id, build, PROFILE, ATLAS_VERSION]).to_string();
        let cache_key = digest(identity);
        let path = crate::index::cache_dir().join("atlas-unreal").join(&cache_key).join(FILE_NAME);
        Ok(Self { root, source_id, build, cache_key, path })
    }

    fn build_at(root: &Path) -> Result<Value, LoreError> {
        for directory in ["Engine", "Engine/Source", "Engine/Build"] {
            required_dir(root, directory)?;
        }
        let path = root.join(BUILD_FILE);
        let meta = metadata(&path)?;
        if !meta.is_file() || is_link(&meta) {
            return Err(io_error(&path, "expected a regular Build.version"));
        }
        let raw = fs::read_to_string(&path).map_err(|error| io_error(&path, error))?;
        let data: Value = serde_json::from_str(raw.trim_start_matches('\u{feff}')).map_err(|error| io_error(&path, error))?;
        let mut build = serde_json::Map::new();
        for key in ["MajorVersion", "MinorVersion", "PatchVersion", "Changelist", "CompatibleChangelist", "BranchName", "IsLicenseeVersion", "IsPromotedBuild"] {
            let value = &data[key];
            let valid = match key {
                "BranchName" => value.is_string(),
                "IsLicenseeVersion" | "IsPromotedBuild" => value.is_null() || matches!(value.as_u64(), Some(0 | 1)),
                "CompatibleChangelist" => value.is_null() || value.as_u64().is_some(),
                _ => value.as_u64().is_some(),
            };
            if !valid {
                return Err(io_error(&path, format!("invalid or missing {key}")));
            }
            build.insert(key.to_owned(), value.clone());
        }
        Ok(Value::Object(build))
    }

    pub fn verify_build(&self) -> Result<(), LoreError> {
        if Self::build_at(&self.root)? != self.build {
            return Err(io_error(&self.root, "Unreal build changed during this command; retry with the current installation"));
        }
        Ok(())
    }

    pub fn label(&self) -> String {
        let part = |name: &str| self.build[name].as_u64().unwrap_or_default();
        format!("UE {}.{}.{} CL {}", part("MajorVersion"), part("MinorVersion"), part("PatchVersion"), part("Changelist"))
    }

    pub fn identity(&self) -> String {
        json!([self.source_id, self.build, PROFILE, ATLAS_VERSION]).to_string()
    }

    pub fn source_json(&self) -> Value {
        json!({
            "source": "unreal", "source_id": self.source_id, "build": self.build,
            "root": crate::config::simplify(&self.root), "cache_key": self.cache_key, "path": self.path
        })
    }

    pub fn scan(&self) -> Result<Scan, LoreError> {
        let mut scan = Scan::default();
        scan.walk(&self.root, "Engine", Scope::Engine)?;
        Ok(scan)
    }

    pub fn contains_file(&self, relative: &str) -> bool {
        let mut path = self.root.clone();
        let mut parts = Path::new(relative).components().peekable();
        while let Some(std::path::Component::Normal(name)) = parts.next() {
            path.push(name);
            let Ok(meta) = fs::symlink_metadata(&path) else { return false };
            if is_link(&meta) {
                return false;
            }
            if parts.peek().is_none() {
                return meta.is_file();
            }
            if !meta.is_dir() {
                return false;
            }
        }
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    pub bytes: u64,
    pub modified: String,
}

impl Stamp {
    fn from(path: &Path, meta: &Metadata) -> Result<Self, LoreError> {
        let modified = meta.modified().map_err(|error| io_error(path, error))?;
        let modified = match modified.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(since) => since.as_nanos().to_string(),
            Err(before) => format!("-{}", before.duration().as_nanos()),
        };
        Ok(Self { bytes: meta.len(), modified })
    }
}

pub(super) struct SourceFile {
    pub stamp: Stamp,
    pub lang: Lang,
}

impl SourceFile {
    pub fn read(&self, path: &Path) -> Result<Vec<u8>, LoreError> {
        let bytes = fs::read(path).map_err(|error| io_error(path, error))?;
        let after = metadata(path)?;
        if !after.is_file() || is_link(&after) || Stamp::from(path, &after)? != self.stamp || bytes.len() as u64 != self.stamp.bytes {
            return Err(io_error(path, "file changed during Unreal indexing; retry"));
        }
        Ok(bytes)
    }
}

#[derive(Default)]
pub(super) struct Scan {
    pub files: BTreeMap<String, SourceFile>,
    pub skipped: BTreeMap<&'static str, usize>,
}

#[derive(Clone, Copy)]
enum Scope {
    Engine,
    Platforms,
    Platform,
    Plugins,
    Source,
}

impl Scope {
    fn child(self, name: &str, plugin: bool) -> Option<Self> {
        match self {
            Self::Engine | Self::Platform if name.eq_ignore_ascii_case("Source") => Some(Self::Source),
            Self::Engine | Self::Platform if name.eq_ignore_ascii_case("Plugins") => Some(Self::Plugins),
            Self::Engine if name.eq_ignore_ascii_case("Platforms") => Some(Self::Platforms),
            Self::Platforms => Some(Self::Platform),
            Self::Plugins if plugin && name.eq_ignore_ascii_case("Source") => Some(Self::Source),
            Self::Plugins | Self::Source => Some(self),
            Self::Engine | Self::Platform => None,
        }
    }
}

impl Scan {
    fn skip(&mut self, reason: &'static str) {
        *self.skipped.entry(reason).or_default() += 1;
    }

    fn walk(&mut self, root: &Path, relative: &str, scope: Scope) -> Result<(), LoreError> {
        let dir = root.join(relative);
        let entries = fs::read_dir(&dir).map_err(|error| io_error(&dir, error))?
            .collect::<Result<Vec<_>, _>>().map_err(|error| io_error(&dir, error))?;
        let plugin = if matches!(scope, Scope::Plugins) {
            let mut found = false;
            for entry in &entries {
                if entry.file_name().to_str().is_some_and(|name| name.to_ascii_lowercase().ends_with(".uplugin")) {
                    let meta = metadata(&entry.path())?;
                    found |= meta.is_file() && !is_link(&meta);
                }
            }
            found
        } else {
            false
        };
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                self.skip("invalid_path");
                continue;
            };
            let lower = name.to_ascii_lowercase();
            if matches!(lower.as_str(), "binaries" | "intermediate" | "saved" | "deriveddatacache" | "content" | ".git" | "thirdparty") {
                self.skip("excluded");
                continue;
            }
            let path = entry.path();
            let meta = metadata(&path)?;
            if is_link(&meta) {
                self.skip("link");
                continue;
            }
            if meta.is_dir() {
                if let Some(child) = scope.child(name, plugin) {
                    self.walk(root, &format!("{relative}/{name}"), child)?;
                }
                continue;
            }
            if !matches!(scope, Scope::Source) || !meta.is_file() {
                continue;
            }
            if lower.ends_with(".generated.h") || lower.ends_with(".gen.cpp") || lower.ends_with(".generated.cpp") {
                self.skip("generated");
                continue;
            }
            let lang = match detect(name) {
                Some(lang @ (Lang::Cpp | Lang::C)) => lang,
                Some(Lang::CSharp) if lower.ends_with(".build.cs") || lower.ends_with(".target.cs") => Lang::CSharp,
                _ => {
                    self.skip("extension");
                    continue;
                }
            };
            if meta.len() == 0 || meta.len() > MAX_BYTES {
                self.skip("size");
                continue;
            }
            self.files.insert(format!("{relative}/{name}"), SourceFile { stamp: Stamp::from(&path, &meta)?, lang });
        }
        Ok(())
    }
}
