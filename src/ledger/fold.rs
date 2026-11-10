//! The fold of one entry (kind + key): its lines form a graph by `parents`, a
//! line coming after all its ancestors. A field's value is given by each line
//! that sets it and has no descendant setting it too; two such lines with
//! different values are a conflict, and until it is resolved the one with the
//! smaller id holds - the same on every machine, whatever order the files were
//! read in. `at` orders nothing.
use super::Line;
use super::canonical;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// One of a conflict's sides.
#[derive(Clone, Debug, PartialEq)]
pub struct Side {
    pub id: String,
    pub by: String,
    pub at: String,
    pub value: Value,
}

/// What an entry's lines come to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    /// Field -> value in force; unset fields are absent.
    pub set: Map<String, Value>,
    /// The lines no other line of the entry descends from, by id.
    pub heads: Vec<String>,
    /// Field -> the concurrent lines that set it differently, by id; the first holds.
    pub conflicts: BTreeMap<String, Vec<Side>>,
    /// (line, parent) where the parent is not among the entry's lines (yet).
    pub dangling: Vec<(String, String)>,
    /// Lines, deduplicated, in the graph's order (ancestors first, ties by id).
    pub order: Vec<String>,
    /// Lines on a cycle of parents, which only a hash collision can make: a
    /// reader refuses the entry rather than fold it without them.
    pub cycle: Vec<String>,
    /// Ids under which two different lines came: only a collision of the
    /// shortened hash, or a changed copy, makes one. A reader refuses the entry.
    pub collisions: Vec<String>,
}

/// Raised when the fold's rules change, so a cache made by an older one is built again.
pub const VERSION: u32 = 2;

impl Entry {
    pub fn to_json(&self) -> Value {
        let conflicts: Map<String, Value> = self
            .conflicts
            .iter()
            .map(|(field, sides)| {
                let sides: Vec<Value> = sides.iter().map(|side| serde_json::json!({"id": side.id, "by": side.by, "at": side.at, "value": side.value})).collect();
                (field.clone(), Value::Array(sides))
            })
            .collect();
        let dangling: Vec<Value> = self.dangling.iter().map(|(line, parent)| serde_json::json!({"line": line, "parent": parent})).collect();
        serde_json::json!({"set": self.set, "heads": self.heads, "conflicts": conflicts, "dangling": dangling})
    }
}

/// Folds the lines of one entry. Lines may come in any order and twice.
pub fn fold(lines: &[Line]) -> Entry {
    let mut by_id: BTreeMap<&str, &Line> = BTreeMap::new();
    let mut collisions: Vec<String> = Vec::new();
    for line in lines {
        match by_id.get(line.id.as_str()) {
            Some(first) if *first != line => collisions.push(line.id.clone()),
            Some(_) => {}
            None => {
                by_id.insert(line.id.as_str(), line);
            }
        }
    }
    collisions.sort();
    collisions.dedup();
    let nodes: Vec<&Line> = by_id.values().copied().collect();
    let count = nodes.len();
    let index: BTreeMap<&str, usize> = nodes.iter().enumerate().map(|(at, line)| (line.id.as_str(), at)).collect();
    let mut dangling = Vec::new();
    let parents: Vec<Vec<usize>> = nodes
        .iter()
        .map(|line| {
            line.parents
                .iter()
                .filter_map(|parent| match index.get(parent.as_str()) {
                    Some(at) => Some(*at),
                    None => {
                        dangling.push((line.id.clone(), parent.clone()));
                        None
                    }
                })
                .collect()
        })
        .collect();
    // Kahn's order, smallest id first among the ready: deterministic. A cycle
    // (it would take a hash collision) leaves its lines out of the order; they
    // are named in `cycle`, and a reader refuses the entry.
    let mut waiting: Vec<usize> = parents.iter().map(Vec::len).collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (child, list) in parents.iter().enumerate() {
        for parent in list {
            children[*parent].push(child);
        }
    }
    let mut ready: BTreeSet<usize> = (0..count).filter(|at| waiting[*at] == 0).collect();
    let mut order = Vec::with_capacity(count);
    while let Some(next) = ready.pop_first() {
        order.push(next);
        for child in &children[next] {
            waiting[*child] -= 1;
            if waiting[*child] == 0 {
                ready.insert(*child);
            }
        }
    }
    let mut has_child = vec![false; count];
    for list in &parents {
        for parent in list {
            has_child[*parent] = true;
        }
    }
    let heads: Vec<String> = (0..count).filter(|at| !has_child[*at]).map(|at| nodes[at].id.clone()).collect();

    // Linear in lines x fields: walking the graph from its heads back, each
    // line learns which fields some descendant of it sets. A line that sets a
    // field none of its descendants sets is one of the field's latest.
    let mut names: Vec<&str> = nodes.iter().flat_map(|line| line.set.keys().map(String::as_str)).collect();
    names.sort_unstable();
    names.dedup();
    let width = names.len().div_ceil(64).max(1);
    let field_at = |name: &str| names.binary_search(&name).unwrap_or(0);
    let own: Vec<Vec<u64>> = nodes
        .iter()
        .map(|line| {
            let mut bits = vec![0u64; width];
            for field in line.set.keys() {
                let at = field_at(field);
                bits[at / 64] |= 1 << (at % 64);
            }
            bits
        })
        .collect();
    let mut below: Vec<Vec<u64>> = vec![vec![0u64; width]; count];
    for &node in order.iter().rev() {
        let mut bits = vec![0u64; width];
        for &child in &children[node] {
            for word in 0..width {
                bits[word] |= own[child][word] | below[child][word];
            }
        }
        below[node] = bits;
    }
    let set_below = |node: usize, field: &str| {
        let at = field_at(field);
        below[node][at / 64] & (1 << (at % 64)) != 0
    };

    let mut setters: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (at, line) in nodes.iter().enumerate() {
        for field in line.set.keys() {
            setters.entry(field.as_str()).or_default().push(at);
        }
    }
    let mut set = Map::new();
    let mut conflicts = BTreeMap::new();
    for (field, lines) in setters {
        let latest: Vec<usize> = lines.iter().copied().filter(|&line| !set_below(line, field)).collect();
        let values: BTreeSet<String> = latest.iter().map(|&line| canonical(&nodes[line].set[field])).collect();
        // `latest` is in id order, so its first line holds.
        let holding = nodes[latest[0]].set[field].clone();
        if values.len() > 1 {
            let sides = latest
                .iter()
                .map(|&line| Side { id: nodes[line].id.clone(), by: nodes[line].by.clone(), at: nodes[line].at.clone(), value: nodes[line].set[field].clone() })
                .collect();
            conflicts.insert(field.to_owned(), sides);
        }
        if !holding.is_null() {
            set.insert(field.to_owned(), holding);
        }
    }
    let mut placed = vec![false; count];
    for &at in &order {
        placed[at] = true;
    }
    let cycle = (0..count).filter(|at| !placed[*at]).map(|at| nodes[at].id.clone()).collect();
    let order = order.into_iter().map(|at| nodes[at].id.clone()).collect();
    Entry { set, heads, conflicts, dangling, order, cycle, collisions }
}
