//! Opt-in C++ call navigation. The text Atlas and the file graph have their own databases.
mod clang;
mod compdb;
mod library;
mod query;
mod store;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::unreal::{digest, Unreal};
use crate::config::Config;
use crate::LoreError;

pub(super) use query::{navigate, Direction, QueryOptions};
pub(super) use store::{index, IndexOptions};

pub(super) fn command_arguments(arguments: &[String], command: Option<&str>) -> Option<Vec<String>> {
    if arguments.is_empty() {
        compdb::split_command(command?).ok()
    } else {
        Some(arguments.to_vec())
    }
}

fn error(message: impl std::fmt::Display) -> LoreError {
    LoreError::new(format!("atlas calls: {message}"))
}

fn path_text(path: &Path) -> String {
    crate::config::simplify(path).to_string_lossy().replace('\\', "/")
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Selection<'a> {
    Project,
    Unreal,
    Named(&'a str),
    All,
}

impl<'a> Selection<'a> {
    pub fn parse(value: &'a str, config: &Config) -> Result<Self, LoreError> {
        match value {
            "project" => Ok(Self::Project),
            "unreal" => Ok(Self::Unreal),
            "all" => Ok(Self::All),
            name if config.atlas.sources.contains_key(name) => Ok(Self::Named(name)),
            other => Err(error(format!("unknown source {other:?}; use project, unreal, all or a configured source name"))),
        }
    }
}

struct Source {
    kind: &'static str,
    id: String,
    root: PathBuf,
    db: PathBuf,
    name: Option<String>,
    engine_build: Option<Value>,
}

impl Source {
    fn status(&self) -> Value {
        let mut state = json!({"source": self.kind, "source_id": self.id, "root": path_text(&self.root), "path": path_text(&self.db)});
        if let Some(name) = &self.name {
            state["name"] = json!(name);
        }
        if let Some(build) = &self.engine_build {
            state["build"] = build.clone();
        }
        state
    }
}

struct Context {
    project: Source,
    named: Option<Source>,
    engine: Option<(Unreal, Source)>,
    engine_error: Option<String>,
    profile: String,
}

impl Context {
    fn new(config: &Config, profile: &str, selection: Selection<'_>) -> Result<Self, LoreError> {
        if profile.trim().is_empty() {
            return Err(error("the analysis profile cannot be empty"));
        }
        let root = std::fs::canonicalize(&config.root).map_err(error)?;
        let repo_key = crate::index::repo_key(&config.root);
        let cache = crate::index::cache_dir();
        let named = if let Selection::Named(name) = selection {
            let source = super::dependencies::named::resolve(config, name, &config.atlas.sources[name]).map_err(error)?;
            let key = digest(json!([profile, name, source.index_key]).to_string());
            Some(Source {
                kind: "external", id: format!("external:{}", source.index_key),
                root: source.canonical_path.canonicalize().map_err(error)?,
                db: cache.join(&repo_key).join("calls-named-v1").join(format!("{key}.sqlite")),
                name: Some(name.to_owned()), engine_build: None,
            })
        } else {
            None
        };
        let (engine, engine_error) = match config.atlas.unreal_root.as_ref().filter(|_| named.is_none()) {
            Some(root) => match Unreal::resolve(root) {
                Ok(engine) => (Some(engine), None),
                Err(e) => (None, Some(e.to_string())),
            },
            None => (None, None),
        };
        let engine_key = engine.as_ref().map(|engine| engine.cache_key.as_str());
        let project = Source {
            kind: "project", id: format!("project:{repo_key}"), root, name: None, engine_build: None,
            db: cache.join(repo_key).join("calls-v1").join(format!("{}.sqlite", digest(json!([profile, engine_key]).to_string()))),
        };
        let engine = engine.map(|engine| {
            let source = Source {
                kind: "unreal", id: engine.source_id.clone(), root: engine.root.clone(), engine_build: Some(engine.build.clone()),
                name: None,
                db: engine.path.parent().expect("engine cache directory").join("calls-v1")
                    .join(format!("{}.sqlite", digest(profile))),
            };
            (engine, source)
        });
        Ok(Self { project, named, engine, engine_error, profile: profile.to_owned() })
    }

    fn unreal(&self) -> Result<&Source, LoreError> {
        self.engine.as_ref().map(|(_, source)| source).ok_or_else(|| {
            error(self.engine_error.as_deref().unwrap_or("set atlas.unreal.root to select the engine"))
        })
    }

    fn origin(&self, path: &Path) -> Option<&Source> {
        if let Some(source) = self.named.as_ref().filter(|source| path.starts_with(&source.root)) {
            return Some(source);
        }
        if let Some((_, engine)) = &self.engine {
            if path.starts_with(&engine.root) {
                return Some(engine);
            }
        }
        path.starts_with(&self.project.root).then_some(&self.project)
    }

    fn location(&self, point: &clang::Point) -> Value {
        let origin = self.origin(&point.file);
        let relative = origin.and_then(|source| point.file.strip_prefix(&source.root).ok());
        let mut place = json!({
            "source": origin.map_or("external", |source| source.kind),
            "source_id": origin.map(|source| source.id.as_str()),
            "path": path_text(relative.unwrap_or(&point.file)),
            "absolute_path": path_text(&point.file),
            "line": point.line,
            "column": point.column,
            "byte": point.byte,
        });
        if let Some(name) = origin.and_then(|source| source.name.as_deref()) {
            place["source_name"] = json!(name);
        }
        place
    }

    fn span(&self, span: &clang::Span) -> Value {
        let mut value = self.location(&span.point);
        value["start_byte"] = json!(span.start);
        value["end_byte"] = json!(span.end);
        value
    }

    fn symbol(&self, function: &clang::Function, tu: &Path, compiler: &str, target: &str) -> Value {
        let owner = self.origin(&function.owner);
        let owner_key = match owner {
            Some(source) if source.kind == "unreal" => self.engine.as_ref().map(|(engine, _)| engine.cache_key.clone()).unwrap_or_default(),
            Some(source) => source.id.clone(),
            None => path_text(&function.owner),
        };
        let local = (!function.external).then(|| path_text(tu));
        let key = json!([1, compiler, target, self.profile, owner_key, function.usr, local]);
        json!({
            "id": format!("cpp:{}", digest(key.to_string())),
            "usr": function.usr, "name": function.name, "signature": function.signature,
            "owner": owner.map(|source| source.id.as_str())
        })
    }
}

type Hashes = BTreeMap<PathBuf, Option<String>>;

fn hash_file(cache: &mut Hashes, path: &Path) -> Option<String> {
    cache.entry(path.to_owned()).or_insert_with(|| std::fs::read(path).ok().map(digest)).clone()
}
