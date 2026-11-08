//! The `lore` command: CodeLore's command line over `cli::run`, and the built-in
//! models' `lore model fetch`/`status`. It is a library function so that another
//! package (Zerus builds `lore` from a pinned git dependency of this crate) can have
//! its own `lore` binary whose `main` is one call: `codelore::app::main()`.
use std::io::Read;
use crate::builtin::BUILTIN;
use crate::cli::{self, Env};
use crate::embed::{self, EmbeddingModel, ModelSource};

/// Runs the command line of this process and exits with its code.
pub fn main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("model") {
        let code = model(&args[1..]);
        // A background fetch a search started holds this marker; it ends here.
        if let Some(lock) = std::env::var_os("LORE_FETCH_LOCK") {
            let _ = std::fs::remove_file(lock);
        }
        std::process::exit(code);
    }
    crate::builtin::enable_auto_fetch();
    let mut stdin = Vec::new();
    // stdin is read for `find -` and `--json`/`--batch` input. Not for
    // `--version`, nor from a terminal: either would wait for an EOF nobody sends.
    let version_only = args.iter().any(|arg| arg == "--version");
    if !version_only && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let _ = std::io::stdin().read_to_end(&mut stdin);
    }
    let env = Env {
        cwd: std::env::current_dir().unwrap_or_default(),
        stdin,
        lore_by: std::env::var("LORE_BY").ok(),
        finder: Some(&BUILTIN),
        roots: roots(),
        history: history_source(),
        history_scope: history_scope(),
        ..Env::default()
    };
    let references: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = cli::run(&references, &env);
    print!("{}", output.stdout);
    eprint!("{}", output.stderr);
    if output.code == 0 && args.iter().any(|arg| arg == "init") {
        prefetch_after_init(&args, &env.cwd);
    }
    // A background `lore index` a search started holds this marker; it ends here.
    if let Some(lock) = std::env::var_os("LORE_FETCH_LOCK") {
        let _ = std::fs::remove_file(lock);
    }
    std::process::exit(output.code);
}

/// After `lore init`: the configured model starts downloading in the background,
/// so the first search a moment later already finds it on its way.
fn prefetch_after_init(args: &[String], cwd: &std::path::Path) {
    if !crate::builtin::auto_fetch() {
        return;
    }
    let start = args.iter().position(|arg| arg == "-C").and_then(|at| args.get(at + 1)).map_or_else(|| cwd.to_path_buf(), |dir| cwd.join(dir));
    let Ok(config) = crate::config::load(&start) else { return };
    if config.embed_provider == "none" {
        return;
    }
    let model = config.embedding_model();
    if !embed::status_for(model, &embed::model_dir(model)).available {
        eprintln!("lore: {}", crate::builtin::fetch_in_background(model));
    }
}

/// docs/lore.md 2.4: a Zerus store's reference roots, the folders a cited
/// commit or file may be in, as `LORE_ROOTS` (the platform's path list); Zerus
/// sets it for the stores it writes. Unset: CodeLore's one repository.
fn roots() -> Option<crate::refs::Roots> {
    roots_from(std::env::var_os("LORE_ROOTS"))
}

fn roots_from(value: Option<std::ffi::OsString>) -> Option<crate::refs::Roots> {
    let value = value.filter(|value| !value.is_empty())?;
    let folders: Vec<std::path::PathBuf> = std::env::split_paths(&value).filter(|path| !path.as_os_str().is_empty()).collect();
    (!folders.is_empty()).then(|| crate::refs::Roots::new(folders, "this computer"))
}

/// docs/lore.md 11.7: Zerus registers its content-search index by naming its
/// hgs in `LORE_HISTORY_HGS`; standalone lore has no history source.
fn history_source() -> Option<std::sync::Arc<dyn crate::history::HistorySource>> {
    let hgs = std::env::var_os("LORE_HISTORY_HGS").filter(|value| !value.is_empty())?;
    Some(std::sync::Arc::new(crate::history::HgsSource { hgs: hgs.into() }))
}

/// Whose sessions the history channel returns: `LORE_HISTORY_SESSIONS` (a
/// member's own, the lead's team's; unset: the owner's, any) and
/// `LORE_HISTORY_EXCLUDE` (sessions whose memory_history is off).
fn history_scope() -> crate::history::Scope {
    let list = |name: &str| -> Option<Vec<String>> {
        let value = std::env::var(name).ok()?;
        Some(value.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_owned).collect())
    };
    crate::history::Scope { sessions: list("LORE_HISTORY_SESSIONS"), excluded: list("LORE_HISTORY_EXCLUDE").unwrap_or_default() }
}

/// `lore model fetch [--local <dir>] [--json] [--model NAME]` and
/// `lore model status [--json] [--model NAME]`.
fn model(args: &[String]) -> i32 {
    let json = args.iter().any(|arg| arg == "--json");
    let local = args
        .iter()
        .position(|arg| arg == "--local")
        .and_then(|position| args.get(position + 1))
        .map(std::path::PathBuf::from);
    let named = args
        .iter()
        .position(|arg| arg == "--model")
        .and_then(|position| args.get(position + 1))
        .map(String::as_str);
    let command = args.iter().find(|arg| matches!(arg.as_str(), "fetch" | "status")).map(String::as_str);
    let (device, kind) = embed::models::device();
    let model = named.and_then(EmbeddingModel::from_name).unwrap_or_else(|| EmbeddingModel::default_for(&device));
    if named.is_some() && named.and_then(EmbeddingModel::from_name).is_none() {
        eprintln!("lore: unknown model; choose bge-m3, multilingual-e5-base or nomic-embed-text-v1.5");
        return 2;
    }
    if command.is_none() {
        eprintln!("usage: lore model fetch|status [--local <dir>] [--json] [--model NAME]");
        return 2;
    }
    if command == Some("status") {
        let status = embed::status_for(model, &embed::model_dir(model));
        // What it really runs on: the model is opened, so a GPU path that falls
        // back (no CUDA libraries, no DirectX 12 card) says so.
        let mut kind = kind;
        let mut fallback = None;
        if status.available {
            match embed::Embedder::open_model(model, ModelSource::Cache) {
                Ok(embedder) => kind = embedder.device().clone(),
                Err(error) => fallback = Some(error.to_string()),
            }
            fallback = fallback.or_else(embed::directml_fallback);
        }
        if let Some(note) = &fallback {
            eprintln!("lore: note: {note}");
        }
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "model": status.model.id(),
                    "device": kind.label(),
                    "gpu": kind.is_gpu(),
                    "dir": status.dir.display().to_string(),
                    "available": status.available,
                    "reason": status.reason,
                })
            );
        } else if status.available {
            println!("{} is installed at {}; running on {}", model.id(), status.dir.display(), kind.label());
        } else {
            println!("{} is not installed: {} ({}); run `lore model fetch`; the device is {}", model.id(), status.reason, status.dir.display(), kind.label());
        }
        return if status.available { 0 } else { 1 };
    }
    let source = match &local {
        Some(path) => ModelSource::Local(path),
        None => ModelSource::Cache,
    };
    let mut progress = |name: &str, done: u64, total: u64| {
        if !json && (done == total || done % (64 * 1024 * 1024) < 1024 * 1024) {
            eprintln!("{name}: {done}/{total}");
        }
    };
    match embed::fetch_model(model, source, &mut progress) {
        Ok(dir) => {
            if json {
                let files: Vec<serde_json::Value> = model
                    .files()
                    .iter()
                    .map(|file| serde_json::json!({"name": file.name, "bytes": file.bytes, "sha256": file.sha256}))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({"model": model.id(), "path": dir.display().to_string(), "files": files})
                );
            } else {
                println!("{} is installed at {}", model.id(), dir.display());
            }
            0
        }
        Err(error) => {
            if json {
                println!("{}", serde_json::json!({"error": error.to_string()}));
            } else {
                eprintln!("lore: {error}");
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::roots_from;
    use std::path::PathBuf;

    /// Task 128: Zerus names a store's reference roots in `LORE_ROOTS`, the
    /// platform's path list, the store first; unset or empty is CodeLore's
    /// one repository.
    #[test]
    fn lore_roots_name_the_reference_folders_in_order() {
        let folders = [PathBuf::from("store"), PathBuf::from("team"), PathBuf::from("member")];
        let joined = std::env::join_paths(&folders).unwrap();
        let roots = roots_from(Some(joined)).expect("roots");
        assert_eq!(roots.folders, folders);
        assert_eq!(roots.machine, "this computer");
        assert_eq!(roots_from(None), None);
        assert_eq!(roots_from(Some("".into())), None);
    }
}
