use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use super::{error, path_text, store, Context, Hashes, Selection, Source};
use crate::{config::Config, LoreError};

#[derive(Clone, Copy)]
pub(in crate::atlas) enum Direction {
    In,
    Out,
}

impl Direction {
    pub fn parse(value: &str) -> Result<Self, LoreError> {
        match value {
            "in" => Ok(Self::In),
            "out" => Ok(Self::Out),
            _ => Err(error("direction must be in or out")),
        }
    }
}

pub(in crate::atlas) struct QueryOptions<'a> {
    pub target: &'a str,
    pub selection: Selection<'a>,
    pub profile: &'a str,
    pub direction: Direction,
    pub depth: usize,
    pub limit: Option<usize>,
    pub candidates: bool,
    pub status: bool,
}

struct Mounted<'a> {
    source: &'a Source,
    db: Connection,
    state: usize,
}

struct Graph<'a> {
    maps: Vec<Mounted<'a>>,
    states: Vec<Value>,
    diagnostics: Vec<Value>,
    hashes: Hashes,
    freshness: BTreeMap<(usize, String), bool>,
    nodes: BTreeMap<String, Node>,
}

#[derive(Clone, Default)]
struct Node {
    symbol: Value,
    definitions: Vec<Value>,
    declarations: Vec<Value>,
    candidates: Vec<Value>,
}

impl Node {
    fn location(&self) -> Value {
        if self.definitions.len() == 1 { self.definitions[0].clone() } else { Value::Null }
    }

    fn json(&self) -> Value {
        let mut node = self.symbol.clone();
        node["definition"] = self.location();
        node["declaration"] = self.declarations.first().cloned().unwrap_or_default();
        let pending = self.definitions.iter().filter(|_| self.definitions.len() > 1)
            .map(|location| json!({"location": location, "status": "ambiguous"}));
        let candidates = self.candidates.iter().filter(|candidate| {
            let trusted = if candidate["role"] == "definition" { &self.definitions } else { &self.declarations };
            !trusted.iter().any(|location| same_place(location, &candidate["location"]))
        });
        node["candidates"] = json!(candidates.cloned().chain(pending).collect::<Vec<_>>());
        node
    }
}

pub(in crate::atlas) fn navigate(config: &Config, options: QueryOptions<'_>) -> Result<Value, LoreError> {
    if !(1..=32).contains(&options.depth) {
        return Err(error("depth must be between 1 and 32"));
    }
    if options.limit == Some(0) {
        return Err(error("limit must be positive"));
    }
    let context = Context::new(config, options.profile, options.selection)?;
    let mut graph = Graph::new(&context, options.selection)?;
    let mut starts = Vec::new();
    for id in graph.find(options.target, &context.project.root)? {
        starts.push(graph.node(&id)?.json());
    }
    // A selector with multiple identities is evidence for choosing a start, not a traversal.
    let (mut edges, mut truncated) = (Vec::new(), false);
    if starts.len() == 1 {
        let id = starts[0]["id"].as_str().expect("symbol id");
        (edges, truncated) = graph.walk(id, options.direction, options.depth, options.limit, options.candidates)?;
    }
    graph.health(options.status)?;
    if let Some((engine, _)) = &context.engine {
        engine.verify_build()?;
    }
    Ok(json!({
        "schema_version": store::VERSION, "profile": options.profile, "sources": graph.states,
        "direction": match options.direction { Direction::In => "in", Direction::Out => "out" },
        "target": options.target, "start_candidates": starts, "edges": edges, "truncated": truncated, "diagnostics": graph.diagnostics,
        "start_resolution": match starts.len() { 0 => "missing", 1 => "exact", _ => "ambiguous" },
    }))
}

impl<'a> Graph<'a> {
    fn new(context: &'a Context, selection: Selection<'_>) -> Result<Self, LoreError> {
        let mut graph = Self {
            maps: Vec::new(), states: Vec::new(), diagnostics: Vec::new(),
            hashes: Hashes::new(), freshness: BTreeMap::new(), nodes: BTreeMap::new(),
        };
        if let Some(source) = &context.named {
            graph.mount(source, &context.profile)?;
            graph.states[0]["scope"] = json!("project_and_source");
            graph.states[0]["project_root"] = json!(path_text(&context.project.root));
            return Ok(graph);
        }
        if selection != Selection::Unreal {
            graph.mount(&context.project, &context.profile)?;
        }
        if selection != Selection::Project {
            match context.unreal() {
                Ok(source) => graph.mount(source, &context.profile)?,
                Err(error) if selection == Selection::Unreal => return Err(error),
                Err(error) => graph.states.push(json!({"source": "unreal", "state": "unavailable", "note": error.to_string()})),
            }
        }
        Ok(graph)
    }

    fn mount(&mut self, source: &'a Source, profile: &str) -> Result<(), LoreError> {
        let mut state = source.status();
        if let Some(db) = store::open(source)? {
            state["state"] = json!("ready");
            state["revision"] = json!(store::meta(&db, "revision")?.and_then(|rev| rev.parse::<u64>().ok()));
            state["counts"] = store::counts(&db)?;
            state["failed_units"] = json!(db.query_row("SELECT count(*) FROM units WHERE errors != 0", [], |row| row.get::<_, u64>(0)).map_err(error)?);
            if let Some(key) = store::meta(&db, "compilation_key")? {
                state["compilation_key"] = json!(key);
            }
            {
                let mut query = db.prepare("SELECT path, diagnostics FROM units WHERE errors != 0 ORDER BY path LIMIT 100").map_err(error)?;
                let rows = query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
                for result in rows {
                    let (unit, diagnostics) = result.map_err(error)?;
                    self.diagnostics.push(json!({"source": source.kind, "unit": unit, "messages": store::decode(&diagnostics)?}));
                }
            }
            self.maps.push(Mounted { source, db, state: self.states.len() });
        } else {
            state["state"] = json!("unindexed");
            state["note"] = json!(format!("no call index for profile {profile:?}. Run lore atlas calls-index --source {} --profile {profile:?} --compdb <path>",
                source.name.as_deref().unwrap_or(source.kind)));
        }
        self.states.push(state);
        Ok(())
    }

    fn health(&mut self, check_all: bool) -> Result<(), LoreError> {
        for index in 0..self.maps.len() {
            if check_all {
                let units = {
                    let mut stmt = self.maps[index].db.prepare("SELECT path FROM units").map_err(error)?;
                    let rows = stmt.query_map([], |row| row.get::<_, String>(0)).map_err(error)?;
                    rows.collect::<Result<Vec<_>, _>>().map_err(error)?
                };
                for unit in units {
                    self.fresh(index, &unit)?;
                }
            }
            let state = &mut self.states[self.maps[index].state];
            let stale = self.freshness.iter().filter(|((map, _), fresh)| *map == index && !**fresh).count();
            state["freshness_scope"] = json!(if check_all { "all_units" } else { "queried_units" });
            state["stale_units"] = json!(stale);
            state["health"] = json!(if stale != 0 { "stale" } else if state["failed_units"].as_u64().unwrap_or_default() != 0 {
                "partial"
            } else if state["counts"]["units"] == 0 { "empty" } else { "ready" });
        }
        Ok(())
    }

    fn fresh(&mut self, map: usize, unit: &str) -> Result<bool, LoreError> {
        let key = (map, unit.to_owned());
        if let Some(fresh) = self.freshness.get(&key) {
            return Ok(*fresh);
        }
        let fresh = store::fresh(&self.maps[map].db, unit, &mut self.hashes)?;
        self.freshness.insert(key, fresh);
        Ok(fresh)
    }

    fn find(&self, target: &str, project: &Path) -> Result<BTreeSet<String>, LoreError> {
        let mut found = BTreeSet::new();
        let location = target.rsplit_once(':').and_then(|(path, line)| line.parse::<u32>().ok().map(|line| (path.replace('\\', "/"), line)));
        for mounted in &self.maps {
            if let Some((path, line)) = &location {
                let resolved = std::fs::canonicalize(project.join(path)).map(|path| path_text(&path)).unwrap_or_else(|_| path.clone());
                let mut query = mounted.db.prepare(
                    "SELECT DISTINCT id FROM functions WHERE (relative=?1 OR path=?2) AND ?3 BETWEEN start_line AND end_line"
                ).map_err(error)?;
                let rows = query.query_map(params![path, resolved, line], |row| row.get::<_, String>(0)).map_err(error)?;
                for row in rows {
                    found.insert(row.map_err(error)?);
                }
            } else {
                let mut query = mounted.db.prepare("SELECT DISTINCT id FROM functions WHERE id=?1 OR name=?1 OR short=?1 OR signature=?1").map_err(error)?;
                let rows = query.query_map([target], |row| row.get::<_, String>(0)).map_err(error)?;
                for row in rows {
                    found.insert(row.map_err(error)?);
                }
            }
        }
        Ok(found)
    }

    fn node(&mut self, id: &str) -> Result<Node, LoreError> {
        if let Some(node) = self.nodes.get(id) {
            return Ok(node.clone());
        }
        let mut node = Node::default();
        for index in 0..self.maps.len() {
            let locations = {
                let mut query = self.maps[index].db.prepare_cached(
                    "SELECT f.unit, f.role, f.symbol, f.location, u.errors
                     FROM functions f JOIN units u ON u.path=f.unit WHERE f.id=?1 ORDER BY f.path, f.offset"
                ).map_err(error)?;
                let rows = query.query_map([id], |row| Ok((
                    row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?, row.get::<_, bool>(4)?,
                ))).map_err(error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(error)?
            };
            for (unit, role, symbol, location, errors) in locations {
                let location = store::decode(&location)?;
                if node.symbol.is_null() {
                    node.symbol = store::decode(&symbol)?;
                }
                let live = self.fresh(index, &unit)?;
                let set = if !live || errors {
                    let status = if live { "diagnostic_candidate" } else { "stale" };
                    let candidate = json!({"location": location, "status": status, "role": role});
                    if !node.candidates.contains(&candidate) {
                        node.candidates.push(candidate);
                    }
                    continue;
                } else if role == "definition" {
                    &mut node.definitions
                } else {
                    &mut node.declarations
                };
                if !set.iter().any(|seen| same_place(seen, &location)) {
                    set.push(location);
                }
            }
        }
        self.nodes.insert(id.to_owned(), node.clone());
        Ok(node)
    }

    fn walk(&mut self, start: &str, direction: Direction, depth: usize, limit: Option<usize>, candidates: bool) -> Result<(Vec<Value>, bool), LoreError> {
        let mut frontier = BTreeSet::from([start.to_owned()]);
        let mut visited = BTreeSet::new();
        let mut edges = Vec::new();
        let mut emitted = BTreeSet::new();
        for distance in 1..=depth {
            let mut next = BTreeSet::new();
            for from in frontier {
                if !visited.insert(from.clone()) {
                    continue;
                }
                for edge in self.edges(&from, direction, candidates)? {
                    let key = site_key(&edge);
                    if !emitted.insert(key) {
                        continue;
                    }
                    if limit == Some(edges.len()) {
                        return Ok((edges, true));
                    }
                    let endpoint = match direction { Direction::In => &edge["caller"], Direction::Out => &edge["callee"] };
                    if edge["dispatch"] == "direct" && edge["resolution"] == "confirmed" {
                        if let Some(id) = endpoint["id"].as_str() {
                            next.insert(id.to_owned());
                        }
                    }
                    let mut edge = edge;
                    edge["depth"] = json!(distance);
                    edges.push(edge);
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok((edges, false))
    }

    fn edges(&mut self, id: &str, direction: Direction, candidates: bool) -> Result<Vec<Value>, LoreError> {
        let mut found = BTreeMap::<String, Value>::new();
        let column = match direction { Direction::In => "callee", Direction::Out => "caller" };
        for index in 0..self.maps.len() {
            let sites = {
                let mut query = self.maps[index].db.prepare_cached(&format!(
                    "SELECT s.unit, s.data, u.errors FROM sites s JOIN units u ON u.path=s.unit WHERE s.{column}=?1 ORDER BY s.path,s.offset,s.unit"
                )).map_err(error)?;
                let rows = query.query_map([id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, bool>(2)?))).map_err(error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(error)?
            };
            for (unit, data, errors) in sites {
                let mut edge = store::decode(&data)?;
                let site = edge["site"].clone();
                let caller_id = edge["caller"]["id"].as_str().expect("caller id").to_owned();
                let caller = self.node(&caller_id)?;
                let callee_id = edge["callee"]["id"].as_str().map(str::to_owned);
                let callee = callee_id.as_deref().map(|id| self.node(id)).transpose()?;
                let live = self.fresh(index, &unit)?;
                let conflicts = if live && !errors && callee_id.is_some() { self.targets(&site, &caller_id)? } else { BTreeSet::new() };
                let status = if !live {
                    "stale"
                } else if errors {
                    "diagnostic_candidate"
                } else if conflicts.len() > 1 || callee.as_ref().is_some_and(|node| node.definitions.len() > 1) {
                    "ambiguous"
                } else if let Some(node) = &callee {
                    if node.definitions.is_empty() { "declaration_only" } else { "confirmed" }
                } else {
                    "unresolved"
                };
                if !candidates && !matches!(status, "confirmed" | "declaration_only") {
                    continue;
                }
                edge["caller"] = caller.json();
                if let Some(callee) = callee {
                    let mut target = callee.json();
                    if target["declaration"].is_null() && (live && !errors) {
                        target["declaration"] = edge["declaration"].clone();
                    }
                    edge["callee"] = target;
                }
                edge.as_object_mut().expect("stored edge").remove("declaration");
                edge["resolution"] = json!(status);
                if conflicts.len() > 1 {
                    let mut alternatives = Vec::new();
                    for target in &conflicts {
                        alternatives.push(self.node(target)?.json());
                    }
                    edge["target_candidates"] = json!(alternatives);
                }
                edge["index_source"] = json!(self.maps[index].source.kind);
                let key = site_key(&edge);
                let previous = found.entry(key).or_insert_with(|| edge.clone());
                if confidence(&edge) > confidence(previous) {
                    *previous = edge;
                }
            }
        }
        let mut edges: Vec<_> = found.into_values().collect();
        edges.sort_by_key(|edge| (
            edge["site"]["absolute_path"].as_str().unwrap_or_default().to_owned(),
            edge["site"]["byte"].as_u64().unwrap_or_default(),
            edge["callee"]["id"].as_str().unwrap_or_default().to_owned(),
        ));
        Ok(edges)
    }

    fn targets(&mut self, site: &Value, caller: &str) -> Result<BTreeSet<String>, LoreError> {
        let mut targets = BTreeSet::new();
        for index in 0..self.maps.len() {
            let rows = {
                let mut query = self.maps[index].db.prepare_cached(
                    "SELECT s.unit, s.callee FROM sites s JOIN units u ON u.path=s.unit
                     WHERE s.path=?1 AND s.offset=?2 AND s.range_start=?3 AND s.range_end=?4
                     AND s.caller=?5 AND u.errors=0 AND s.callee IS NOT NULL"
                ).map_err(error)?;
                let rows = query.query_map(params![site["absolute_path"].as_str(), site["byte"].as_u64(),
                    site["start_byte"].as_u64(), site["end_byte"].as_u64(), caller],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).map_err(error)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(error)?
            };
            for (unit, target) in rows {
                if self.fresh(index, &unit)? {
                    targets.insert(target);
                }
            }
        }
        Ok(targets)
    }
}

fn same_place(a: &Value, b: &Value) -> bool {
    a["absolute_path"] == b["absolute_path"] && a["byte"] == b["byte"]
}

fn site_key(edge: &Value) -> String {
    json!([edge["site"]["source_id"], edge["site"]["absolute_path"], edge["site"]["start_byte"],
        edge["site"]["end_byte"], edge["caller"]["id"], edge["callee"]["id"]]).to_string()
}

fn confidence(edge: &Value) -> u8 {
    match edge["resolution"].as_str() {
        Some("confirmed") => 4,
        Some("declaration_only") => 3,
        Some("ambiguous") => 2,
        Some("stale") => 0,
        _ => 1,
    }
}
