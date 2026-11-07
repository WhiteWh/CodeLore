//! `lore.toml` (CodeLore's `config.py`): finding it, reading it, refusing what
//! it may not say.
use crate::LoreError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const CONFIG_NAME: &str = "lore.toml";
pub const MODES: &[&str] = &["none", "plain", "shared-tree"];
/// `builtin` is the nomic-embed-text model in this process and `none` is text only. `ollama` is the
/// name Python's `lore init` and every lore.toml written before the owner dropped Ollama use: it
/// means the built-in model too, and the `url`, `model` and `timeout` keys beside it are ignored.
pub const PROVIDERS: &[&str] = &["builtin", "ollama", "none"];

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The repository root, where lore.toml is.
    pub root: PathBuf,
    /// Relative to root, forward slashes.
    pub dir: String,
    pub areas: Vec<String>,
    pub commit_mode: String,
    pub branch: String,
    pub remote: String,
    pub push: bool,
    pub trailers: Vec<String>,
    pub embed_provider: String,
    pub embed_url: String,
    pub embed_model: String,
    pub embed_timeout: f64,
    pub doc_prefix: Option<String>,
    pub query_prefix: Option<String>,
    pub limits: BTreeMap<String, usize>,
    pub dedupe_cosine: f64,
    pub dedupe_jaccard: f64,
    /// `[working]`: the working tier's expiry and bounds (docs/lore.md 11.3, 11.4).
    pub working: Working,
    /// `[secrets] allow`: literals the secret scan lets pass (11.9).
    pub secrets_allow: Vec<String>,
}

/// The working tier's settings. The expiry days are 1 to 365; every bound may
/// be lowered and never raised above the addendum's table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Working {
    pub pulse_days: i64,
    pub message_days: i64,
    pub unread_days: i64,
    /// Minutes an urgent message may stay unrung before the lead hears (1 to 120).
    pub urgent_bound: i64,
    pub pulses_per_team: usize,
    pub messages_per_team: usize,
    pub unread_normal: usize,
    pub unread_urgent: usize,
    pub brief_versions: usize,
    pub entries: usize,
    pub file_bytes: u64,
}

impl Default for Working {
    fn default() -> Self {
        Self {
            pulse_days: 14,
            message_days: 7,
            unread_days: 30,
            urgent_bound: 10,
            pulses_per_team: 2_000,
            messages_per_team: 1_000,
            unread_normal: 50,
            unread_urgent: 5,
            brief_versions: 20,
            entries: 20_000,
            file_bytes: 64 * 1024 * 1024,
        }
    }
}

/// The `[working]` keys: (name, least, most). A bound's most is its ceiling.
const WORKING_KEYS: [(&str, i64, i64); 11] = [
    ("pulse_days", 1, 365),
    ("message_days", 1, 365),
    ("unread_days", 1, 365),
    ("urgent_bound", 1, 120),
    ("pulses_per_team", 1, 2_000),
    ("messages_per_team", 1, 1_000),
    ("unread_normal", 1, 50),
    ("unread_urgent", 1, 5),
    ("brief_versions", 1, 20),
    ("entries", 1, 20_000),
    ("file_mib", 1, 64),
];

fn parse_working(value: &toml::Value, bad: &mut Vec<String>) -> Working {
    let mut working = Working::default();
    let Some(table) = value.as_table() else {
        bad.push("`[working]` must be a table".into());
        return working;
    };
    for (key, value) in table {
        let Some((_, least, most)) = WORKING_KEYS.iter().find(|(name, _, _)| name == key) else {
            bad.push(format!("unknown key `working.{key}`"));
            continue;
        };
        let Some(number) = value.as_integer().filter(|n| (*least..=*most).contains(n)) else {
            bad.push(format!("`working.{key}` must be a whole number from {least} to {most}"));
            continue;
        };
        match key.as_str() {
            "pulse_days" => working.pulse_days = number,
            "message_days" => working.message_days = number,
            "unread_days" => working.unread_days = number,
            "urgent_bound" => working.urgent_bound = number,
            "pulses_per_team" => working.pulses_per_team = number as usize,
            "messages_per_team" => working.messages_per_team = number as usize,
            "unread_normal" => working.unread_normal = number as usize,
            "unread_urgent" => working.unread_urgent = number as usize,
            "brief_versions" => working.brief_versions = number as usize,
            "entries" => working.entries = number as usize,
            _ => working.file_bytes = number as u64 * 1024 * 1024,
        }
    }
    working
}

impl Config {
    pub fn defaults(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            dir: "lore".into(),
            areas: Vec::new(),
            commit_mode: "plain".into(),
            branch: "main".into(),
            remote: "origin".into(),
            push: false,
            trailers: Vec::new(),
            embed_provider: "ollama".into(),
            embed_url: "http://127.0.0.1:11434".into(),
            embed_model: "auto".into(),
            embed_timeout: 2.0,
            doc_prefix: None,
            query_prefix: None,
            limits: BTreeMap::new(),
            dedupe_cosine: 0.95,
            dedupe_jaccard: 0.5,
            working: Working::default(),
            secrets_allow: Vec::new(),
        }
    }

    /// The embedding model this configuration selects (`auto` follows the
    /// device: bge-m3 with a GPU, multilingual-e5-base on the CPU).
    pub fn embedding_model(&self) -> crate::embed::EmbeddingModel {
        let name = self.embed_model.trim();
        if name.is_empty() || name == "auto" {
            let (device, _) = crate::embed::models::device();
            return crate::embed::EmbeddingModel::default_for(&device);
        }
        let (device, _) = crate::embed::models::device();
        crate::embed::EmbeddingModel::from_name(name).unwrap_or_else(|| crate::embed::EmbeddingModel::default_for(&device))
    }

    pub fn lore_dir(&self) -> PathBuf {        self.root.join(&self.dir)
    }

    /// nomic-embed-text is trained with task prefixes; other models get none
    /// unless the configuration names them.
    pub fn prefixes(&self) -> (String, String) {
        let nomic = self.embed_model.contains("nomic");
        (
            self.doc_prefix.clone().unwrap_or_else(|| if nomic { "search_document: ".into() } else { String::new() }),
            self.query_prefix.clone().unwrap_or_else(|| if nomic { "search_query: ".into() } else { String::new() }),
        )
    }
}

/// `^[a-z0-9][a-z0-9-]{0,47}$`
pub fn area_slug(text: &str) -> bool {
    crate::schema::slug(text, 0, 47)
}

pub fn find_root(start: &Path) -> Result<PathBuf, LoreError> {
    let here = std::fs::canonicalize(start).map(|path| simplify(&path)).unwrap_or_else(|_| start.to_path_buf());
    let mut cursor = here.clone();
    loop {
        if cursor.join(CONFIG_NAME).is_file() {
            return Ok(cursor);
        }
        if !cursor.pop() {
            break;
        }
    }
    Err(LoreError::new(format!("no {CONFIG_NAME} in {} or above - run `lore init` at the repository root", here.display())))
}

/// A canonical Windows path without its `\\?\` prefix.
pub fn simplify(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => PathBuf::from(format!(r"\\{rest}")),
        None => PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text)),
    }
}

pub fn load(start: &Path) -> Result<Config, LoreError> {
    let root = find_root(start)?;
    let path = root.join(CONFIG_NAME);
    let text = std::fs::read(&path).map_err(|error| LoreError::new(format!("{}: {error}", path.display())))?;
    let text = String::from_utf8(text).map_err(|error| LoreError::new(format!("{}: not valid TOML: {error}", path.display())))?;
    let data: toml::Value = toml::from_str(&text).map_err(|error| LoreError::new(format!("{}: not valid TOML: {}", path.display(), error.message())))?;
    parse(&data, &root, &path.display().to_string())
}

pub fn parse(data: &toml::Value, root: &Path, place: &str) -> Result<Config, LoreError> {
    let mut bad: Vec<String> = Vec::new();
    let empty = toml::map::Map::new();
    let table = data.as_table().unwrap_or(&empty);
    for key in table.keys() {
        if !["version", "dir", "areas", "commit", "embed", "limits", "dedupe", "working", "secrets"].contains(&key.as_str()) {
            bad.push(format!("unknown key `{key}`"));
        }
    }
    if table.get("version").and_then(toml::Value::as_integer) != Some(1) {
        bad.push("`version` must be 1".into());
    }
    let mut config = Config::defaults(root);

    match table.get("dir") {
        None => {}
        Some(dir) => match dir.as_str() {
            Some(dir) if !dir.is_empty() && !dir.starts_with('/') && !dir.split(['/', '\\']).any(|part| part == "..") => {
                config.dir = dir.replace('\\', "/").trim_matches('/').to_owned();
            }
            _ => bad.push("`dir` must be a relative path inside the repository".into()),
        },
    }

    match table.get("areas") {
        None => {}
        Some(areas) => match areas.as_array().filter(|items| items.iter().all(toml::Value::is_str)) {
            None => bad.push("`areas` must be a list of strings".into()),
            Some(items) => {
                let names: Vec<String> = items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect();
                for area in &names {
                    if !area_slug(area) {
                        bad.push(format!("area `{area}` must match ^[a-z0-9][a-z0-9-]{{0,47}}$"));
                    }
                    if area == "plans" || area == "sources" {
                        bad.push(format!("`{area}` is reserved for {} records and is not an area", &area[..area.len() - 1]));
                    }
                }
                let unique: std::collections::BTreeSet<&String> = names.iter().collect();
                if unique.len() != names.len() {
                    bad.push("`areas` has duplicates".into());
                }
                config.areas = names;
            }
        },
    }

    let commit = match table.get("commit") {
        None => empty.clone(),
        Some(value) => value.as_table().cloned().unwrap_or_else(|| {
            bad.push("`[commit]` must be a table".into());
            empty.clone()
        }),
    };
    for key in commit.keys() {
        if !["mode", "branch", "remote", "push", "trailers"].contains(&key.as_str()) {
            bad.push(format!("unknown key `commit.{key}`"));
        }
    }
    if let Some(mode) = commit.get("mode") {
        config.commit_mode = mode.as_str().map(str::to_owned).unwrap_or_else(|| mode.to_string());
    }
    if !MODES.contains(&config.commit_mode.as_str()) {
        bad.push(format!("`commit.mode` must be one of {}", MODES.join(", ")));
    }
    if let Some(branch) = commit.get("branch") {
        config.branch = branch.as_str().map(str::to_owned).unwrap_or_else(|| branch.to_string());
    }
    if let Some(remote) = commit.get("remote") {
        config.remote = remote.as_str().map(str::to_owned).unwrap_or_else(|| remote.to_string());
    }
    if let Some(push) = commit.get("push") {
        match push.as_bool() {
            Some(push) => config.push = push,
            None => bad.push("`commit.push` must be true or false".into()),
        }
    }
    if let Some(trailers) = commit.get("trailers") {
        match trailers.as_array().filter(|items| items.iter().all(toml::Value::is_str)) {
            Some(items) => config.trailers = items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect(),
            None => bad.push("`commit.trailers` must be a list of strings".into()),
        }
    }

    let embed = match table.get("embed") {
        None => empty.clone(),
        Some(value) => value.as_table().cloned().unwrap_or_else(|| {
            bad.push("`[embed]` must be a table".into());
            empty.clone()
        }),
    };
    for key in embed.keys() {
        if !["provider", "url", "model", "timeout", "doc_prefix", "query_prefix"].contains(&key.as_str()) {
            bad.push(format!("unknown key `embed.{key}`"));
        }
    }
    if let Some(provider) = embed.get("provider") {
        config.embed_provider = provider.as_str().map(str::to_owned).unwrap_or_else(|| provider.to_string());
    }
    if !PROVIDERS.contains(&config.embed_provider.as_str()) {
        bad.push(format!("`embed.provider` must be one of {}", PROVIDERS.join(", ")));
    }
    if let Some(url) = embed.get("url") {
        config.embed_url = url.as_str().map(str::to_owned).unwrap_or_else(|| url.to_string()).trim_end_matches('/').to_owned();
    }
    if let Some(model) = embed.get("model") {
        config.embed_model = model.as_str().map(str::to_owned).unwrap_or_else(|| model.to_string());
    }
    if config.embed_model.trim() != "auto" && crate::embed::EmbeddingModel::from_name(&config.embed_model).is_none() {
        bad.push(format!(
            "`embed.model` must be bge-m3, multilingual-e5-base, nomic-embed-text-v1.5 or auto, not `{}`",
            config.embed_model
        ));
    }
    if let Some(timeout) = embed.get("timeout") {
        config.embed_timeout = timeout.as_float().or_else(|| timeout.as_integer().map(|n| n as f64)).unwrap_or(config.embed_timeout);
    }
    config.doc_prefix = embed.get("doc_prefix").and_then(toml::Value::as_str).map(str::to_owned);
    config.query_prefix = embed.get("query_prefix").and_then(toml::Value::as_str).map(str::to_owned);

    match table.get("limits") {
        None => {}
        Some(limits) => match limits.as_table().filter(|map| map.values().all(|value| value.as_integer().is_some_and(|n| n > 0))) {
            None => bad.push("`[limits]` must map `kind.field` to positive integers".into()),
            Some(map) => {
                config.limits = map.iter().map(|(key, value)| (key.clone(), value.as_integer().unwrap_or(0) as usize)).collect();
            }
        },
    }

    match table.get("dedupe") {
        None => {}
        Some(dedupe) => match dedupe.as_table() {
            Some(map) => {
                let number = |value: &toml::Value| value.as_float().or_else(|| value.as_integer().map(|n| n as f64));
                if let Some(value) = map.get("cosine").and_then(number) {
                    config.dedupe_cosine = value;
                }
                if let Some(value) = map.get("jaccard").and_then(number) {
                    config.dedupe_jaccard = value;
                }
            }
            None => bad.push("`[dedupe]` must be a table".into()),
        },
    }

    if let Some(working) = table.get("working") {
        config.working = parse_working(working, &mut bad);
    }
    match table.get("secrets") {
        None => {}
        Some(secrets) => match secrets.as_table() {
            Some(map) => {
                for key in map.keys().filter(|key| key.as_str() != "allow") {
                    bad.push(format!("unknown key `secrets.{key}`"));
                }
                match map.get("allow").map(|allow| allow.as_array().filter(|items| items.iter().all(toml::Value::is_str))) {
                    None => {}
                    Some(Some(items)) => config.secrets_allow = items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect(),
                    Some(None) => bad.push("`secrets.allow` must be a list of literal strings".into()),
                }
            }
            None => bad.push("`[secrets]` must be a table".into()),
        },
    }

    if bad.is_empty() { Ok(config) } else { Err(LoreError::with(format!("{place} is refused"), bad)) }
}

/// `lore init`'s lore.toml.
pub fn template(dir: &str, areas: &[String], mode: &str, branch: &str) -> String {
    let areas: Vec<String> = areas.iter().map(|area| format!("\"{area}\"")).collect();
    format!(
        "version = 1\ndir = \"{dir}\"\nareas = [{}]\n\n[commit]\nmode = \"{mode}\"          # \"none\" | \"plain\" | \"shared-tree\"\nbranch = \"{branch}\"\npush = false\n\n[embed]\nprovider = \"ollama\"     # \"ollama\" | \"none\"\nurl = \"http://127.0.0.1:11434\"\nmodel = \"nomic-embed-text\"\n",
        areas.join(", ")
    )
}
