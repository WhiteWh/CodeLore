use crate::atlas::{calls, off_reason};
use serde_json::Value;

use super::{emit, need, number, say, Config, LoreError, Opts, Out, Path, PathBuf};

pub(super) fn dispatch(opts: &Opts, config: &Config, json: bool, out: &mut Out) -> Result<(), LoreError> {
    if let Some(reason) = off_reason(config) {
        return Err(LoreError::new(reason));
    }
    let option = |name: &str| opts.options.get(name).map(String::as_str);
    let index = opts.command == "calls-index";
    let status = opts.command == "calls-status";
    let default = if index || config.atlas.unreal_root.is_none() { "project" } else { "all" };
    let selection = calls::Selection::parse(option("--source").unwrap_or(default), config)?;
    let profile = option("--profile").unwrap_or("default");
    let value = if index {
        let compdb = option("--compdb").ok_or_else(|| LoreError::new("lore atlas calls-index needs --compdb <compile_commands.json>"))?;
        let library = option("--libclang").map(PathBuf::from).or_else(|| std::env::var_os("LORE_LIBCLANG").map(PathBuf::from))
            .ok_or_else(|| LoreError::new("select libclang with --libclang <path> or LORE_LIBCLANG"))?;
        calls::index(config, calls::IndexOptions {
            compdb: Path::new(compdb), library: &library, selection, profile,
            prefix: option("--path"), rebuild: opts.flags.contains("--rebuild"),
        })?
    } else {
        let target = if status { String::new() } else { need(opts, "a C++ function name, id, or file:line")? };
        calls::navigate(config, calls::QueryOptions {
            target: &target, selection, profile, direction: calls::Direction::parse(option("--direction").unwrap_or("out"))?,
            depth: number(opts, "--depth", 1)?, limit: option("-n").map(|_| number(opts, "-n", 0)).transpose()?,
            candidates: opts.flags.contains("--candidates"), status,
        })?
    };
    if json {
        emit(out, &value);
    } else if index {
        say(out, &format!(
            "{} call index revision {}: {} parsed, {} reused, {} failed, {} removed, {} call sites",
            string(&value["source"]["source"]), value["revision"], value["parsed"], value["reused"],
            value["failed"], value["removed"], value["counts"]["call_sites"]
        ));
        diagnostics(out, &value);
    } else {
        for source in array(&value["sources"]) {
            let message = source["note"].as_str().unwrap_or_else(|| string(&source["path"]));
            let label = source["name"].as_str().unwrap_or_else(|| string(&source["source"]));
            let health = source["health"].as_str().unwrap_or_else(|| string(&source["state"]));
            say(out, &format!("{label} calls [{health}] {message}"));
        }
        diagnostics(out, &value);
        if !status {
            navigation(out, &value);
        }
    }
    Ok(())
}

fn string(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn diagnostics(out: &mut Out, result: &Value) {
    for diagnostic in array(&result["diagnostics"]) {
        say(out, &format!("{}:", string(&diagnostic["unit"])));
        for message in array(&diagnostic["messages"]) {
            say(out, &format!("  {}", string(message)));
        }
    }
}

fn location(place: &Value) -> String {
    format!("[{}] {}:{}:{}", string(&place["source"]), string(&place["path"]), place["line"], place["column"])
}

fn navigation(out: &mut Out, result: &Value) {
    let starts = array(&result["start_candidates"]);
    if starts.is_empty() {
        say(out, "No matching C++ function in the selected call indexes.");
        return;
    }
    if starts.len() > 1 {
        say(out, "Multiple C++ functions match. Select a signature or id.");
        for start in starts {
            say(out, &format!("  {}  {}", string(&start["signature"]), string(&start["id"])));
        }
        return;
    }
    let edges = array(&result["edges"]);
    if edges.is_empty() {
        say(out, "No matching call sites.");
        return;
    }
    for edge in edges {
        let target = edge["callee"]["signature"].as_str().unwrap_or("unresolved");
        say(out, &format!(
            "{} -> {target} [{} / {}] {}",
            string(&edge["caller"]["signature"]), string(&edge["dispatch"]),
            string(&edge["resolution"]), location(&edge["site"])
        ));
        for (role, place) in [
            ("caller definition", &edge["caller"]["definition"]),
            ("callee definition", &edge["callee"]["definition"]),
            ("callee declaration", &edge["callee"]["declaration"]),
        ] {
            if !place.is_null() {
                say(out, &format!("  {role} {}", location(place)));
            }
        }
        for candidate in array(&edge["callee"]["candidates"]) {
            say(out, &format!("  callee candidate [{}] {}", string(&candidate["status"]), location(&candidate["location"])));
        }
        for alternative in array(&edge["target_candidates"]) {
            say(out, &format!("  alternative target {}  {}", string(&alternative["signature"]), string(&alternative["id"])));
        }
    }
}
