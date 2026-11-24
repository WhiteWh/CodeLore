use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use super::{clang, compdb, digest, error, hash_file, path_text, Context, Hashes, Selection, Source};
use crate::{config::Config, LoreError};

pub(super) const VERSION: i64 = 1;
const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE units (
    path TEXT PRIMARY KEY, command TEXT NOT NULL, target TEXT NOT NULL,
    errors INTEGER NOT NULL, diagnostics TEXT NOT NULL
);
CREATE TABLE inputs (
    unit TEXT NOT NULL REFERENCES units(path) ON DELETE CASCADE,
    path TEXT NOT NULL, hash TEXT NOT NULL, PRIMARY KEY(unit, path)
);
CREATE TABLE functions (
    unit TEXT NOT NULL REFERENCES units(path) ON DELETE CASCADE,
    id TEXT NOT NULL, name TEXT NOT NULL, short TEXT NOT NULL, signature TEXT NOT NULL,
    relative TEXT NOT NULL, path TEXT NOT NULL, start_line INTEGER NOT NULL, end_line INTEGER NOT NULL,
    offset INTEGER NOT NULL, role TEXT NOT NULL, symbol TEXT NOT NULL, location TEXT NOT NULL,
    PRIMARY KEY(unit, id, path, offset, role)
);
CREATE INDEX functions_id ON functions(id);
CREATE INDEX functions_name ON functions(name);
CREATE INDEX functions_short ON functions(short);
CREATE INDEX functions_path ON functions(relative, start_line);
CREATE TABLE sites (
    unit TEXT NOT NULL REFERENCES units(path) ON DELETE CASCADE,
    caller TEXT NOT NULL, callee TEXT, path TEXT NOT NULL, offset INTEGER NOT NULL,
    range_start INTEGER NOT NULL, range_end INTEGER NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX sites_caller ON sites(caller);
CREATE INDEX sites_callee ON sites(callee);
CREATE INDEX sites_location ON sites(path, offset, range_start, range_end, caller);
PRAGMA user_version = 1;";

pub(super) fn decode(text: &str) -> Result<Value, LoreError> {
    serde_json::from_str(text).map_err(error)
}

fn sql_version(db: &Connection) -> Result<i64, LoreError> {
    db.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(error)
}

fn check_version(version: i64) -> Result<(), LoreError> {
    if version != VERSION {
        return Err(error(format!("unsupported call index schema {version}")));
    }
    Ok(())
}

pub(super) fn open(source: &Source) -> Result<Option<Connection>, LoreError> {
    if !source.db.try_exists().map_err(error)? {
        return Ok(None);
    }
    let db = Connection::open_with_flags(&source.db, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(error)?;
    db.busy_timeout(Duration::from_secs(2)).map_err(error)?;
    db.execute_batch("BEGIN").map_err(error)?;
    let version = sql_version(&db)?;
    if version == 0 {
        return Ok(None);
    }
    check_version(version)?;
    Ok(Some(db))
}

pub(super) fn meta(db: &Connection, key: &str) -> Result<Option<String>, LoreError> {
    db.query_row("SELECT value FROM meta WHERE key=?1", [key], |row| row.get(0)).optional().map_err(error)
}

fn set_meta(db: &Connection, key: &str, value: impl ToString) -> Result<(), LoreError> {
    db.execute("INSERT OR REPLACE INTO meta VALUES (?1, ?2)", params![key, value.to_string()]).map_err(error)?;
    Ok(())
}

pub(super) fn fresh(db: &Connection, unit: &str, cache: &mut Hashes) -> Result<bool, LoreError> {
    let mut query = db.prepare_cached("SELECT path, hash FROM inputs WHERE unit=?1").map_err(error)?;
    let mut rows = query.query([unit]).map_err(error)?;
    while let Some(row) = rows.next().map_err(error)? {
        let path: String = row.get(0).map_err(error)?;
        let hash: String = row.get(1).map_err(error)?;
        if hash_file(cache, Path::new(&path)).as_deref() != Some(&hash) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(in crate::atlas) struct IndexOptions<'a> {
    pub compdb: &'a Path,
    pub library: &'a Path,
    pub selection: Selection<'a>,
    pub profile: &'a str,
    pub prefix: Option<&'a str>,
    pub rebuild: bool,
}

pub(in crate::atlas) fn index(config: &Config, options: IndexOptions<'_>) -> Result<Value, LoreError> {
    let started = Instant::now();
    let context = Context::new(config, options.profile, options.selection)?;
    let source = match options.selection {
        Selection::Project => &context.project,
        Selection::Unreal => context.unreal()?,
        Selection::Named(_) => context.named.as_ref().expect("named context"),
        Selection::All => return Err(error("index project and unreal separately with their own compilation databases")),
    };
    let prefix = options.prefix.map(|path| path.replace('\\', "/"));
    let mut roots = vec![source.root.as_path()];
    if options.selection == Selection::Project {
        if let Some((_, source)) = &context.engine {
            roots.push(&source.root);
        }
    }
    let compdb_path = config.root.join(options.compdb);
    let compilation = if context.named.is_some() {
        roots.push(&context.project.root);
        let path = fs::canonicalize(&compdb_path).map_err(error)?;
        let hash = digest(fs::read(&path).map_err(error)?);
        Some((path, hash))
    } else {
        None
    };
    let commands = compdb::read(&compdb_path, &roots, prefix.as_deref())?;
    let library = fs::canonicalize(config.root.join(options.library)).map_err(error)?;
    let clang = clang::Clang::load(&library)?;
    let system = toolchain_roots(&library);
    let compiler_version = clang.version();
    let compiler = digest(json!([compiler_version, digest(fs::read(&library).map_err(error)?)]).to_string());
    if let Some(parent) = source.db.parent() {
        fs::create_dir_all(parent).map_err(error)?;
    }
    let mut db = Connection::open(&source.db).map_err(error)?;
    db.busy_timeout(Duration::from_secs(120)).map_err(error)?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;").map_err(error)?;
    let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(error)?;
    let version = sql_version(&transaction)?;
    if version == 0 {
        transaction.execute_batch(SCHEMA).map_err(error)?;
    } else {
        check_version(version)?;
    }
    let old_compiler = meta(&transaction, "compiler_key")?;
    if old_compiler.as_deref().is_some_and(|old| old != compiler) {
        // One USR space per profile. Old rows outside a prefix cannot mix with the new compiler.
        if prefix.is_some() {
            return Err(error("libclang changed; run calls-index once without --path for this profile"));
        }
    }
    let environment: BTreeMap<_, _> = ["CPATH", "CPLUS_INCLUDE_PATH", "C_INCLUDE_PATH", "INCLUDE", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET"]
        .into_iter().filter_map(|key| std::env::var(key).ok().map(|value| (key, value))).collect();
    let compilation_key = compilation.as_ref().map(|(path, hash)| digest(json!([path_text(path), hash, compiler, environment]).to_string()));
    if let Some(key) = &compilation_key {
        let old = meta(&transaction, "compilation_key")?;
        if prefix.is_some() && old.as_ref().is_some_and(|old| old != key) {
            return Err(error("compilation changed; run calls-index once without --path for this profile"));
        }
        set_meta(&transaction, "compilation_key", key)?;
    }
    let mut hashes = Hashes::new();
    let mut seen = BTreeSet::new();
    let (mut parsed, mut reused, mut failed, mut removed) = (0, 0, 0, 0);
    let mut diagnostics = Vec::new();
    for command in commands {
        if options.selection == Selection::Unreal && !command.cwd.starts_with(&source.root) {
            return Err(error(format!("shared Unreal analysis requires an Engine-local compilation directory: {}", command.cwd.display())));
        }
        let unit = path_text(&command.file);
        seen.insert(unit.clone());
        let mut command_inputs = json!([compiler, command.argv, command.responses.iter().map(|(p, h)| (path_text(p), h)).collect::<Vec<_>>(), environment]);
        if let Some(key) = &compilation_key {
            command_inputs.as_array_mut().expect("command inputs").push(json!(key));
        }
        let key = digest(command_inputs.to_string());
        let previous: Option<(String, bool)> = transaction.query_row(
            "SELECT command, errors FROM units WHERE path=?1", [&unit], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(error)?;
        if !options.rebuild && previous.as_ref().is_some_and(|(old, errors)| *old == key && !errors) && fresh(&transaction, &unit, &mut hashes)? {
            reused += 1;
            continue;
        }
        let primary = hash_file(&mut hashes, &command.file).ok_or_else(|| error(format!("input disappeared: {unit}")))?;
        let mut result = clang.parse(&command.file, &command.cwd, &command.argv)?;
        result.inputs.entry(command.file.clone()).or_insert(primary);
        result.inputs.extend(command.responses);
        if let Some((path, hash)) = &compilation {
            result.inputs.insert(path.clone(), hash.clone());
        }
        if options.selection == Selection::Unreal {
            if let Some(input) = result.inputs.keys().find(|path| !path.starts_with(&source.root) &&
                (path.starts_with(&context.project.root) || !system.iter().any(|root| path.starts_with(root))))
            {
                return Err(error(format!("{} depends on non-engine input {}; index this TU in the project profile", unit, input.display())));
            }
        }
        for (path, parsed_hash) in &result.inputs {
            // The bytes Clang saw and the file we will publish must agree.
            let disk_path = PathBuf::from(path_text(path));
            if hash_file(&mut hashes, &disk_path).as_deref() != Some(parsed_hash) {
                return Err(error(format!("analysis input changed while indexing: {}", path.display())));
            }
        }
        transaction.execute("DELETE FROM units WHERE path=?1", [&unit]).map_err(error)?;
        write_unit(&transaction, &context, &command.file, &unit, &key, &compiler, &result)?;
        parsed += 1;
        failed += usize::from(result.errors);
        if result.errors {
            diagnostics.push(json!({"unit": unit, "messages": result.diagnostics}));
        }
    }
    let existing = {
        let mut query = transaction.prepare("SELECT path FROM units").map_err(error)?;
        let rows = query.query_map([], |row| row.get::<_, String>(0)).map_err(error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(error)?
    };
    for path in existing {
        if seen.contains(&path) {
            continue;
        }
        let local = Path::new(&path);
        let selected = roots.iter().any(|root| {
            local.strip_prefix(Path::new(&path_text(root))).ok().is_some_and(|relative| compdb::wanted(relative, prefix.as_deref()))
        });
        if selected {
            transaction.execute("DELETE FROM units WHERE path=?1", [&path]).map_err(error)?;
            removed += 1;
        }
    }
    if let Some((engine, _)) = &context.engine {
        engine.verify_build()?;
    }
    if let Some((path, hash)) = &compilation {
        if digest(fs::read(path).map_err(error)?) != *hash {
            return Err(error(format!("compilation database changed while indexing: {}", path.display())));
        }
    }
    let previous = meta(&transaction, "revision")?.and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
    let revision = if previous == 0 || parsed + removed > 0 { previous + 1 } else { previous };
    set_meta(&transaction, "revision", revision)?;
    set_meta(&transaction, "compiler", &compiler_version)?;
    set_meta(&transaction, "compiler_key", compiler)?;
    let counts = counts(&transaction)?;
    transaction.commit().map_err(error)?;
    Ok(json!({
        "schema_version": VERSION, "source": source.status(), "profile": options.profile,
        "backend": "libclang", "compiler": compiler_version, "revision": revision,
        "parsed": parsed, "reused": reused, "failed": failed, "removed": removed,
        "counts": counts, "diagnostics": diagnostics, "seconds": started.elapsed().as_secs_f64(),
    }))
}

fn toolchain_roots(library: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(root) = library.parent().and_then(Path::parent).filter(|path| path.parent().is_some()) {
        roots.push(root.to_owned());
    }
    #[cfg(windows)]
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(variable) {
            roots.extend(["Microsoft Visual Studio", "Windows Kits"].iter().filter_map(|folder| fs::canonicalize(PathBuf::from(&base).join(folder)).ok()));
        }
    }
    #[cfg(unix)]
    for root in ["/usr/include", "/usr/lib", "/usr/local/include"] {
        roots.extend(fs::canonicalize(root).ok());
    }
    roots
}

pub(super) fn counts(db: &Connection) -> Result<Value, LoreError> {
    let mut value = json!({});
    for (label, table) in [("units", "units"), ("functions", "functions"), ("call_sites", "sites")] {
        let count: i64 = db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0)).map_err(error)?;
        value[label] = json!(count);
    }
    Ok(value)
}

struct Unit<'a> {
    db: &'a Connection,
    context: &'a Context,
    file: &'a Path,
    path: &'a str,
    compiler: &'a str,
    target: &'a str,
}

fn write_unit(db: &Connection, context: &Context, file: &Path, path: &str, key: &str, compiler: &str, parsed: &clang::Parsed) -> Result<(), LoreError> {
    db.execute(
        "INSERT INTO units VALUES (?1, ?2, ?3, ?4, ?5)",
        params![path, key, parsed.target, parsed.errors, json!(parsed.diagnostics).to_string()],
    ).map_err(error)?;
    let mut input = db.prepare_cached("INSERT INTO inputs VALUES (?1, ?2, ?3)").map_err(error)?;
    for (input_path, hash) in &parsed.inputs {
        input.execute(params![path, path_text(input_path), hash]).map_err(error)?;
    }
    let unit = Unit { db, context, file, path, compiler, target: &parsed.target };
    for function in &parsed.definitions {
        unit.function(function, "definition")?;
    }
    for call in &parsed.calls {
        unit.call(call)?;
    }
    Ok(())
}

impl Unit<'_> {
    fn function(&self, function: &clang::Function, role: &str) -> Result<Value, LoreError> {
        let symbol = self.context.symbol(function, self.file, self.compiler, self.target);
        let location = self.context.span(&function.location);
        self.db.prepare_cached(
            "INSERT OR IGNORE INTO functions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
        ).map_err(error)?.execute(params![
            self.path, symbol["id"].as_str(), function.name, function.short, function.signature,
            location["path"].as_str(), location["absolute_path"].as_str(),
            function.location.first_line, function.location.last_line, function.location.point.byte, role,
            symbol.to_string(), location.to_string()
        ]).map_err(error)?;
        Ok(symbol)
    }

    fn call(&self, call: &clang::Call) -> Result<(), LoreError> {
        let caller = self.context.symbol(&call.caller, self.file, self.compiler, self.target);
        let callee = if let Some(function) = &call.callee {
            if call.definition.as_ref().is_some_and(|body| body.location.point == function.location.point) {
                Some(self.context.symbol(function, self.file, self.compiler, self.target))
            } else {
                Some(self.function(function, "declaration")?)
            }
        } else {
            None
        };
        if let Some(definition) = &call.definition {
            self.function(definition, "definition")?;
        }
        let site = self.context.span(&call.site);
        let declaration = call.callee.as_ref().map(|callee| self.context.span(&callee.location));
        let data = json!({
            "caller": caller, "callee": callee,
            "site": site, "dispatch": call.dispatch, "declaration": declaration,
            "spelling": call.spelling.as_ref().map(|point| self.context.location(point)),
        });
        self.db.prepare_cached("INSERT INTO sites VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)").map_err(error)?.execute(params![
            self.path, caller["id"].as_str(), callee.as_ref().and_then(|symbol| symbol["id"].as_str()),
            site["absolute_path"].as_str(), call.site.point.byte, call.site.start, call.site.end, data.to_string()
        ]).map_err(error)?;
        Ok(())
    }
}
