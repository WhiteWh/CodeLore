//! Reading plans (CodeLore's `plans.py`): counts, layers, what is ready, what
//! nobody has touched.
use crate::gitio;
use crate::schema::{item_number, UNDECIDED};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const STATUSES: &[&str] = &["open", "deferred", "done", "dropped", "superseded"];

fn text<'a>(record: &'a Value, key: &str) -> &'a str {
    record.get(key).and_then(Value::as_str).unwrap_or_default()
}

pub fn items_of(records: &[Value], plan: &str) -> Vec<Value> {
    let mut items: Vec<Value> = records.iter().filter(|record| text(record, "kind") == "item" && text(record, "plan") == plan).cloned().collect();
    items.sort_by_key(|item| item_number(text(item, "id")));
    items
}

/// Each plan with its item counts: `[{"plan": {...}, "counts": {...}}]`.
pub fn summary(records: &[Value], include_closed: bool) -> Vec<Value> {
    let mut plans: Vec<&Value> = records.iter().filter(|record| text(record, "kind") == "plan").collect();
    plans.sort_by_key(|plan| text(plan, "id").to_owned());
    let mut rows = Vec::new();
    for plan in plans {
        if text(plan, "status") == "closed" && !include_closed {
            continue;
        }
        let mut counts: Map<String, Value> = STATUSES.iter().map(|status| ((*status).to_owned(), json!(0))).collect();
        for item in items_of(records, text(plan, "id")) {
            let status = item.get("status").map(|status| status.as_str().map(str::to_owned).unwrap_or_else(|| status.to_string())).unwrap_or_else(|| "null".into());
            let count = counts.get(&status).and_then(Value::as_u64).unwrap_or(0) + 1;
            counts.insert(status, json!(count));
        }
        rows.push(json!({"plan": plan, "counts": counts}));
    }
    rows
}

/// Depth by the longest path from the roots, over this plan's own items; a
/// back edge (a cycle `lore check` already refuses) is ignored.
pub fn layers(items: &[Value]) -> Vec<Vec<Value>> {
    let by_id: BTreeMap<String, &Value> = items.iter().map(|item| (text(item, "id").to_owned(), item)).collect();
    fn depth_of(id: &str, by_id: &BTreeMap<String, &Value>, depth: &mut BTreeMap<String, usize>, visiting: &mut BTreeSet<String>) -> usize {
        if let Some(known) = depth.get(id) {
            return *known;
        }
        if visiting.contains(id) {
            return 0;
        }
        visiting.insert(id.to_owned());
        let mut best = 0;
        for dep in by_id[id].get("deps").and_then(Value::as_array).into_iter().flatten() {
            if let Some(item) = dep.get("item").and_then(Value::as_str).filter(|item| by_id.contains_key(*item)) {
                best = best.max(depth_of(item, by_id, depth, visiting) + 1);
            }
        }
        visiting.remove(id);
        depth.insert(id.to_owned(), best);
        best
    }
    let mut depth = BTreeMap::new();
    let mut visiting = BTreeSet::new();
    for id in by_id.keys() {
        depth_of(id, &by_id, &mut depth, &mut visiting);
    }
    let mut order: Vec<&String> = by_id.keys().collect();
    order.sort_by_key(|id| (depth[*id], item_number(id)));
    let mut out: Vec<Vec<Value>> = Vec::new();
    for id in order {
        while out.len() <= depth[id] {
            out.push(Vec::new());
        }
        out[depth[id]].push(by_id[id].clone());
    }
    out
}

/// Open, and every hard dependency done. A soft dependency only orders.
pub fn ready(item: &Value, by_id: &BTreeMap<String, Value>) -> bool {
    if text(item, "status") != "open" {
        return false;
    }
    item.get("deps").and_then(Value::as_array).into_iter().flatten().all(|dep| {
        text(dep, "kind") != "hard" || by_id.get(text(dep, "item")).is_some_and(|other| text(other, "status") == "done")
    })
}

/// Undecided items whose record has not changed in `days` and whose files no
/// commit has touched since it last changed.
pub fn stale(root: &Path, records: &[Value], days: i64, today: Option<chrono::NaiveDate>) -> Vec<Value> {
    let today = today.unwrap_or_else(|| chrono::Local::now().date_naive());
    let cutoff = (today - chrono::Duration::days(days)).format("%Y-%m-%d").to_string();
    let closed: BTreeSet<&str> = records.iter().filter(|record| text(record, "kind") == "plan" && text(record, "status") == "closed").map(|record| text(record, "id")).collect();
    let mut result: Vec<Value> = Vec::new();
    for record in records {
        if text(record, "kind") != "item" || !UNDECIDED.contains(&text(record, "status")) || closed.contains(text(record, "plan")) {
            continue;
        }
        if text(record, "updated") > cutoff.as_str() {
            continue;
        }
        let files: Vec<String> = record.get("refs").and_then(|refs| refs.get("files")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        if !files.is_empty() && gitio::touched_since(root, text(record, "updated"), &files) {
            continue;
        }
        result.push(record.clone());
    }
    result.sort_by_key(|record| (text(record, "updated").to_owned(), text(record, "plan").to_owned(), item_number(text(record, "id"))));
    result
}
