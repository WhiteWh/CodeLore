//! A model kept in memory between searches. Every `lore find` is a new process,
//! and opening the model costs ~0.5 s of each semantic search (the weights are
//! read into memory) where embedding the query takes ~75 ms. The first search
//! starts `lore model serve` in the background: it holds the model on the CPU,
//! answers on 127.0.0.1 to whoever shows the token in its state file, and ends
//! after ten idle minutes. Later searches send it their query (a millisecond to
//! connect) and open no model of their own. `LORE_EMBED_SERVE=off` turns it off;
//! `LORE_EMBED_SERVE_IDLE` is the idle limit in seconds.
//!
//! One request per connection: a line of JSON `{"token", "texts": [...]}` (the
//! texts carry their prefixes, as [`Embeddings::embed`] gets them), and a line
//! back: `{"vectors": [[...]]}` or `{"error": "..."}`.
use super::{models_root, EmbeddingModel, Embedder, Embeddings, ModelSource};
use crate::LoreError;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROTOCOL: &str = "v1";

/// Whether searches may use (and start) a server: not with `LORE_EMBED_SERVE=off`.
pub fn enabled() -> bool {
    !std::env::var("LORE_EMBED_SERVE").is_ok_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

fn idle_limit() -> Duration {
    Duration::from_secs(std::env::var("LORE_EMBED_SERVE_IDLE").ok().and_then(|value| value.trim().parse().ok()).unwrap_or(600))
}

fn state_dir() -> PathBuf {
    models_root().join("serve")
}

/// Where a running server of `model` says its port and token.
fn state_file(model: EmbeddingModel) -> PathBuf {
    state_dir().join(format!("{}-{PROTOCOL}.json", model.id()))
}

/// What a server's vectors depend on besides the model: the build of `lore`
/// that runs it and the token cap. A server of another build (an install that
/// replaced `lore` while it ran) is not asked: its vectors may differ.
fn build() -> String {
    format!("{}/t{}", env!("LORE_BUILD_COMMIT"), crate::embed::token_cap(usize::MAX))
}

/// A token nobody else on the machine can guess from outside this user's files:
/// std's per-process random hash keys, the time and the process id, hashed.
fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for round in 0..2u8 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u8(round);
        hasher.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos()));
        hasher.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

/// A server's answer to the queries of this process.
pub struct Remote {
    addr: SocketAddr,
    token: String,
    dimensions: usize,
}

/// The running server of `model`, when one answers.
pub fn connect(model: EmbeddingModel) -> Option<Remote> {
    let text = std::fs::read_to_string(state_file(model)).ok()?;
    let state: serde_json::Value = serde_json::from_str(&text).ok()?;
    if state["build"].as_str() != Some(build().as_str()) {
        return None;
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], u16::try_from(state["port"].as_u64()?).ok()?));
    // a port that refuses at once is a server that is gone
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    Some(Remote { addr, token: state["token"].as_str()?.to_owned(), dimensions: model.dimensions() })
}

impl Embeddings for Remote {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LoreError> {
        let fail = |error: String| LoreError::new(format!("the model server: {error}"));
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_secs(2)).map_err(|error| fail(error.to_string()))?;
        stream.set_read_timeout(Some(Duration::from_secs(120))).map_err(|error| fail(error.to_string()))?;
        let request = serde_json::json!({"token": self.token, "texts": texts});
        stream.write_all(format!("{request}\n").as_bytes()).map_err(|error| fail(error.to_string()))?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).map_err(|error| fail(error.to_string()))?;
        let reply: serde_json::Value = serde_json::from_str(&line).map_err(|error| fail(error.to_string()))?;
        if let Some(error) = reply["error"].as_str() {
            return Err(fail(error.to_owned()));
        }
        serde_json::from_value(reply["vectors"].clone()).map_err(|error| fail(error.to_string()))
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

/// `lore model serve --model M`: holds `model` and answers until it has been
/// idle for [`idle_limit`]. Returns the exit code.
pub fn run(model: EmbeddingModel) -> i32 {
    let embedder = match Embedder::open_model_for_queries(model, ModelSource::Cache) {
        Ok(embedder) => Arc::new(embedder),
        Err(error) => {
            eprintln!("lore: model serve: {error}");
            return 1;
        }
    };
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("lore: model serve: cannot listen: {error}");
            return 1;
        }
    };
    let Ok(addr) = listener.local_addr() else { return 1 };
    let token = token();
    let state = state_file(model);
    let written = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(state_dir())?;
        let temp = state.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temp, serde_json::json!({"port": addr.port(), "token": token, "pid": std::process::id(), "build": build()}).to_string())?;
        std::fs::rename(&temp, &state)
    })();
    if let Err(error) = written {
        eprintln!("lore: model serve: cannot write {}: {error}", state.display());
        return 1;
    }
    let _ = std::fs::remove_file(starting_marker(model));
    let _ = listener.set_nonblocking(true);
    let started = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    let mut checked = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                last.store(started.elapsed().as_secs(), Ordering::Relaxed);
                let (embedder, token, last) = (Arc::clone(&embedder), token.clone(), Arc::clone(&last));
                std::thread::spawn(move || {
                    answer(stream, embedder.as_ref(), &token);
                    last.store(started.elapsed().as_secs(), Ordering::Relaxed);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
        if checked.elapsed() < Duration::from_secs(1) {
            continue;
        }
        checked = Instant::now();
        let idle = started.elapsed().saturating_sub(Duration::from_secs(last.load(Ordering::Relaxed)));
        // another server took the state file over: this one is not found any more
        let ours = std::fs::read_to_string(&state).is_ok_and(|text| text.contains(&token));
        if idle >= idle_limit() || !ours {
            break;
        }
    }
    if std::fs::read_to_string(&state).is_ok_and(|text| text.contains(&token)) {
        let _ = std::fs::remove_file(&state);
    }
    0
}

fn answer(stream: TcpStream, embedder: &Embedder, token: &str) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let reply = match serde_json::from_str::<serde_json::Value>(&line) {
        Ok(request) if request["token"].as_str() == Some(token) => {
            let texts: Vec<&str> = request["texts"].as_array().map(|texts| texts.iter().filter_map(serde_json::Value::as_str).collect()).unwrap_or_default();
            match embedder.embed_prefixed(&texts) {
                Ok(vectors) => serde_json::json!({"vectors": vectors}),
                Err(error) => serde_json::json!({"error": error.to_string()}),
            }
        }
        Ok(_) => serde_json::json!({"error": "wrong token"}),
        Err(error) => serde_json::json!({"error": error.to_string()}),
    };
    let _ = writer.write_all(format!("{reply}\n").as_bytes());
}

/// The binary a server runs from: a copy of this one in the state folder,
/// named by its size and time. A running program's file cannot be replaced on
/// Windows, and a server living ten minutes on `lore.exe` itself would make an
/// install (ours, Zerus's) fail. Copies no server runs any more are removed.
fn server_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let meta = std::fs::metadata(&exe)?;
    let stamp = meta.modified().ok().and_then(|at| at.duration_since(UNIX_EPOCH).ok()).map_or(0, |since| since.as_secs());
    let extension = exe.extension().map(|ext| format!(".{}", ext.to_string_lossy())).unwrap_or_default();
    let name = format!("lore-serve-{}-{stamp}{extension}", meta.len());
    let copy = state_dir().join(&name);
    if let Ok(entries) = std::fs::read_dir(state_dir()) {
        for entry in entries.flatten() {
            let other = entry.file_name().to_string_lossy().into_owned();
            if other.starts_with("lore-serve-") && other != name {
                let _ = std::fs::remove_file(entry.path()); // fails while a server runs it
            }
        }
    }
    if std::fs::metadata(&copy).map(|m| m.len()).ok() != Some(meta.len()) {
        let temp = state_dir().join(format!("{name}.{}.tmp", std::process::id()));
        std::fs::copy(&exe, &temp)?;
        if std::fs::rename(&temp, &copy).is_err() {
            let _ = std::fs::remove_file(&temp); // another search copied it first
        }
    }
    Ok(copy)
}

fn starting_marker(model: EmbeddingModel) -> PathBuf {
    state_dir().join(format!("{}-{PROTOCOL}.starting", model.id()))
}

/// Starts `lore model serve` for `model` in the background, unless one is
/// starting already (a marker under a minute old). `spawn` is the caller's
/// detached spawn.
pub fn start(model: EmbeddingModel, spawn: impl FnOnce(&mut std::process::Command) -> std::io::Result<()>) {
    let marker = starting_marker(model);
    let fresh = std::fs::metadata(&marker).and_then(|meta| meta.modified()).ok().and_then(|at| at.elapsed().ok()).is_some_and(|age| age.as_secs() < 60);
    if fresh {
        return;
    }
    let _ = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(state_dir())?;
        std::fs::write(&marker, std::process::id().to_string())?;
        let mut command = std::process::Command::new(server_exe()?);
        command
            .args(["model", "serve", "--model", model.id()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        spawn(&mut command)
    })();
}
