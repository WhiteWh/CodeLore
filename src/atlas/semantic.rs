//! Bounded semantic candidates for the existing Atlas rank fusion.
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{error, params, Atlas, Filter, LoreError, OptionalExtension, PreparedFilter, DEPTH};
use crate::atlas::ann::{Binding, Reader, Unavailable};
use crate::atlas::unreal::digest;

pub(super) const EXACT_LIMIT: usize = 4096;
const FILTER_PROBES: usize = 16_384;
/// A generation that lags the snapshot: chunks it does not hold as they are
/// now are scored exactly, at most this many, out of at most DELTA_SCAN rows.
const DELTA_LIMIT: usize = EXACT_LIMIT;
const DELTA_SCAN: usize = 262_144;
const FILTER_SQL: &str = "substr(c.path, 1, length(?3)) = ?3 COLLATE NOCASE AND (?4 = '' OR f.lang = ?4)";

#[derive(Default)]
pub(in crate::atlas) struct SearchTimings {
    pub(super) fts: Duration,
    pub(super) filter_prepare: Duration,
    pub(super) prefilter_rows: Option<usize>,
    ann_open: Duration,
    ann: Duration,
    sqlite_filter: Duration,
    sqlite_rerank: Duration,
}

impl SearchTimings {
    pub fn requested() -> bool {
        std::env::var("LORE_ATLAS_SEARCH_TIMINGS").is_ok_and(|value| value == "1")
    }

    pub fn json(&self) -> Value {
        let ms = |time: Duration| time.as_secs_f64() * 1000.0;
        json!({"fts": ms(self.fts), "filter_prepare": ms(self.filter_prepare), "prefilter_rows": self.prefilter_rows,
            "ann_open": ms(self.ann_open), "ann": ms(self.ann),
            "sqlite_filter": ms(self.sqlite_filter), "sqlite_rerank": ms(self.sqlite_rerank)})
    }
}

enum Route {
    /// `behind`: built for an older revision of this source.
    Ann(Arc<Reader>, bool),
    Exact,
    Text,
}

pub(in crate::atlas) struct SemanticPlan {
    route: Route,
    state: &'static str,
    pub note: Option<String>,
    filtered_budget_reached: bool,
    delta_budget_reached: bool,
}

impl SemanticPlan {
    pub fn text(state: &'static str, note: Option<String>) -> Self {
        Self::routed(Route::Text, state, note)
    }

    fn routed(route: Route, state: &'static str, note: Option<String>) -> Self {
        Self { route, state, note, filtered_budget_reached: false, delta_budget_reached: false }
    }

    /// An ANN generation is missing or lags the snapshot: worth scheduling a build.
    pub fn wants_build(&self) -> bool {
        matches!(self.route, Route::Ann(_, true)) || matches!(self.state, "ann_missing" | "ann_stale")
    }

    pub fn uses_vectors(&self) -> bool {
        !matches!(self.route, Route::Text)
    }

    pub fn exact(&self) -> bool {
        matches!(self.route, Route::Exact)
    }

    pub fn no_vectors(&self) -> bool {
        self.state == "no_vectors"
    }

    pub fn model_unavailable(&mut self) {
        if self.uses_vectors() {
            *self = Self::text("model_unavailable", None);
        }
    }

    pub fn json(&self) -> Value {
        let mode = match self.route { Route::Ann(..) => "ann", Route::Exact => "exact", Route::Text => "text" };
        let mut value = json!({"mode": mode, "state": self.state});
        if let Some(note) = &self.note {
            value["note"] = json!(note);
        }
        if self.filtered_budget_reached {
            value["filter_budget_reached"] = json!(true);
        }
        if self.delta_budget_reached {
            value["delta_budget_reached"] = json!(true);
        }
        value
    }

    pub fn retrieve(&mut self, atlas: &Atlas, query: Option<&[f32]>, model: &str, filter: &PreparedFilter<'_>, timings: &mut SearchTimings) -> Vec<i64> {
        if filter.is_empty() {
            return Vec::new();
        }
        let Some(query) = query.filter(|query| !query.is_empty() && query.iter().all(|v| v.is_finite())) else {
            self.model_unavailable();
            return Vec::new();
        };
        let result = match &self.route {
            Route::Text => return Vec::new(),
            Route::Exact => {
                let started = Instant::now();
                let rows = atlas.exact_meaning(query, model, filter.filter);
                timings.sqlite_rerank = started.elapsed();
                rows
            }
            Route::Ann(reader, behind) => {
                let mut delta = (*behind).then_some(false);
                let ids = atlas.ann_meaning(reader, query, model, filter, timings, &mut self.filtered_budget_reached, delta.as_mut());
                self.delta_budget_reached = delta.unwrap_or(false);
                ids
            }
        };
        match result {
            Ok(ids) => ids,
            Err(error) => {
                let state = if self.exact() { "exact_error" } else { "ann_damaged" };
                *self = Self::text(state, Some(format!("code meaning unavailable: {error}. Showing text results")));
                Vec::new()
            }
        }
    }
}

impl Atlas {
    pub(in crate::atlas) fn small_enough_for_exact(&self) -> Result<bool, LoreError> {
        if self.has_projections {
            let count: usize = self.db.query_row("SELECT total FROM atlas_counts WHERE name='chunks'", [], |row| row.get(0)).map_err(error)?;
            return Ok(count <= EXACT_LIMIT);
        }
        let too_large = self.db.query_row("SELECT 1 FROM chunks LIMIT 1 OFFSET ?1", [EXACT_LIMIT], |_| Ok(()))
            .optional().map_err(error)?.is_some();
        Ok(!too_large)
    }

    pub(in crate::atlas) fn compatible_coverage(&self, model: &str, dimension: usize) -> Option<usize> {
        if !self.has_projections {
            return None;
        }
        self.db.query_row("SELECT coalesce((SELECT covered FROM atlas_vector_counts WHERE model=?1 AND bytes=?2), 0)",
            params![model, dimension * 4], |row| row.get::<_, usize>(0)).ok()
    }

    pub(in crate::atlas) fn prepare_semantics(&self, space: &str, model: &str, dimension: usize, allow_fill: bool, timings: &mut SearchTimings) -> SemanticPlan {
        let started = Instant::now();
        let small = self.small_enough_for_exact().unwrap_or(false);
        let covered = self.compatible_coverage(model, dimension);
        let exact = || SemanticPlan::routed(Route::Exact, "ready", None);
        let plan = match covered {
            Some(0) if !small || !allow_fill => SemanticPlan::text("no_vectors", None),
            Some(0) => exact(),
            Some(covered) => {
                let reader = self.ann_binding(space, model, dimension, covered)
                    .ok_or(Unavailable::Stale)
                    .and_then(|binding| Reader::open(&self.path, &binding).map(|reader| {
                        let behind = !reader.fresh_for(&binding);
                        (reader, behind)
                    }));
                match reader {
                    Ok((reader, false)) => SemanticPlan::routed(Route::Ann(reader, false), "ready", None),
                    // Exact similarity is complete for a small source.
                    _ if small => exact(),
                    Ok((reader, true)) => SemanticPlan::routed(Route::Ann(reader, true), "behind", Some(
                        "ANN index lags the snapshot. Changed chunks are scored exactly until it is rebuilt".to_owned(),
                    )),
                    Err(reason) => {
                        let (state, note) = match reason {
                            Unavailable::Missing => ("ann_missing", "ANN index is missing".to_owned()),
                            Unavailable::Stale => ("ann_stale", "ANN index does not match the current snapshot or vector space".to_owned()),
                            Unavailable::Damaged(error) => ("ann_damaged", format!("ANN index cannot be read: {error}")),
                        };
                        SemanticPlan::text(state, Some(format!("{note}. Showing text results")))
                    }
                }
            }
            None if small && (allow_fill || self.vector_count(model) > 0) => exact(),
            None if small => SemanticPlan::text("no_vectors", None),
            None => SemanticPlan::text("ann_stale", Some("ANN needs a compatible SQLite snapshot. Showing text results".to_owned())),
        };
        timings.ann_open = started.elapsed();
        plan
    }

    pub(in crate::atlas) fn ann_binding(&self, space: &str, model: &str, dimension: usize, covered: usize) -> Option<Binding> {
        let (source_key, revision) = if let Some(key) = self.meta("dependency_identity") {
            (format!("dependency:{key}"), self.meta("revision")?)
        } else if let Some(identity) = self.meta("unreal_identity") {
            (format!("unreal:{}", digest(identity)), self.meta("revision")?)
        } else {
            (format!("project:{}", crate::index::repo_key(&self.root)), self.meta("project_revision")?)
        };
        let vector_generation = self.meta("ann_vector_clock").map(|clock| {
            let value = self.meta(&format!("ann_vector_generation:{model}")).unwrap_or_else(|| "0".to_owned());
            format!("{clock}:{value}")
        }).or_else(|| self.meta("ann_vector_generation"));
        Some(Binding {
            source_key, space: space.to_owned(), vector_model: model.to_owned(), dimension, revision,
            refreshed: self.meta("refreshed")?, vector_generation, covered,
        })
    }

    fn selected_hashes(&self, timings: &mut SearchTimings) -> Option<HashMap<u64, String>> {
        let started = Instant::now();
        let result = (|| -> rusqlite::Result<HashMap<u64, String>> {
            let mut stmt = self.db.prepare("SELECT e.id,c.ehash FROM temp.atlas_search_ids e
                CROSS JOIN chunks c ON c.id=e.id WHERE c.ehash IS NOT NULL")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)? as u64, row.get(1)?)))?;
            rows.collect()
        })();
        timings.filter_prepare += started.elapsed();
        result.ok()
    }

    #[allow(clippy::too_many_arguments)]
    fn ann_meaning(&self, reader: &Reader, query: &[f32], model: &str, selection: &PreparedFilter<'_>, timings: &mut SearchTimings, limited: &mut bool, delta: Option<&mut bool>) -> Result<Vec<i64>, LoreError> {
        let filter = selection.filter;
        let selected = selection.selected.and_then(|_| self.selected_hashes(timings));
        let started = Instant::now();
        let ids = if filter.path.is_empty() && filter.lang.is_empty() {
            reader.search(query).map_err(error)?
        } else if let Some(selected) = selected {
            let probes = Cell::new(0);
            let result = reader.filtered(query, |id| {
                if probes.get() >= FILTER_PROBES {
                    return false;
                }
                let Some(hash) = reader.hash(id) else { return false };
                probes.set(probes.get() + 1);
                selected.get(&id).is_some_and(|current| current == hash)
            }).map_err(error)?;
            *limited = probes.get() >= FILTER_PROBES;
            result
        } else {
            let mut stmt = self.db.prepare_cached(&format!(
                "SELECT EXISTS(SELECT 1 FROM chunks c JOIN files f ON f.path=c.path WHERE c.id=?1 AND c.ehash=?2 AND {FILTER_SQL})"
            )).map_err(error)?;
            let probes = RefCell::new((0, Duration::ZERO, None));
            let query_stmt = RefCell::new(&mut stmt);
            let result = reader.filtered(query, |id| {
                let Some(hash) = reader.hash(id) else { return false };
                let mut probe = probes.borrow_mut();
                if probe.0 >= FILTER_PROBES || probe.2.is_some() {
                    return false;
                }
                probe.0 += 1;
                let started = Instant::now();
                let matched = query_stmt.borrow_mut().query_row(params![id as i64, hash, filter.path, filter.lang], |row| row.get::<_, bool>(0));
                probe.1 += started.elapsed();
                match matched {
                    Ok(matched) => matched,
                    Err(error) => { probe.2 = Some(error); false }
                }
            });
            let (n, sql_time, failure) = probes.into_inner();
            timings.sqlite_filter = sql_time;
            *limited = n >= FILTER_PROBES;
            if let Some(failure) = failure {
                return Err(error(failure));
            }
            result.map_err(error)?
        };
        timings.ann = started.elapsed().saturating_sub(timings.sqlite_filter);
        let started = Instant::now();
        let mut stmt = self.db.prepare_cached(&format!(
            "SELECT v.vec FROM chunks c
             JOIN files f ON f.path=c.path
             JOIN vectors v ON v.hash=c.ehash AND v.model=?5
             WHERE c.id=?1 AND c.ehash=?2 AND {FILTER_SQL} AND length(v.vec)=?6 AND typeof(v.vec)='blob'"
        )).map_err(error)?;
        let mut scored = Vec::new();
        for id in ids {
            let Some(hash) = reader.hash(id) else { continue };
            let value = stmt.query_row(params![id as i64, hash, filter.path, filter.lang, model, query.len() * 4], |row| {
                Ok(score(row.get_ref(0)?.as_blob()?, query))
            }).optional().map_err(error)?.flatten();
            if let Some(value) = value {
                scored.push((id as i64, value));
            }
        }
        if let Some(delta_limited) = delta {
            *delta_limited = self.delta_meaning(reader, query, model, filter, &mut scored)?;
        }
        timings.sqlite_rerank = started.elapsed();
        Ok(top(scored))
    }

    /// Chunks with a compatible vector whose ID and embedding hash a lagging
    /// generation does not hold, scored exactly. True when a budget cut the set.
    fn delta_meaning(&self, reader: &Reader, query: &[f32], model: &str, filter: &Filter, scored: &mut Vec<(i64, f64)>) -> Result<bool, LoreError> {
        let bytes = query.len() * 4;
        let mut rows = self.db.prepare_cached(&format!(
            "SELECT c.id, c.ehash FROM chunks c
             JOIN files f ON f.path=c.path
             JOIN atlas_vector_sizes s ON s.hash=c.ehash AND s.model=?1 AND s.bytes=?2
             WHERE {FILTER_SQL} LIMIT ?5"
        )).map_err(error)?;
        let mut scanned = 0;
        let mut changed = Vec::new();
        let mut found = rows.query(params![model, bytes, filter.path, filter.lang, DELTA_SCAN]).map_err(error)?;
        while let Some(row) = found.next().map_err(error)? {
            scanned += 1;
            let id: i64 = row.get(0).map_err(error)?;
            let hash: String = row.get(1).map_err(error)?;
            let held = u64::try_from(id).ok().and_then(|id| reader.hash(id)).is_some_and(|known| known == hash);
            if !held {
                changed.push((id, hash));
            }
        }
        let limited = scanned >= DELTA_SCAN || changed.len() > DELTA_LIMIT;
        changed.truncate(DELTA_LIMIT);
        let known: HashSet<i64> = scored.iter().map(|(id, _)| *id).collect();
        let mut vector = self.db.prepare_cached(
            "SELECT vec FROM vectors WHERE model=?1 AND hash=?2 AND typeof(vec)='blob' AND length(vec)=?3"
        ).map_err(error)?;
        for (id, hash) in changed {
            if known.contains(&id) {
                continue;
            }
            let value = vector.query_row(params![model, hash, bytes], |row| Ok(score(row.get_ref(0)?.as_blob()?, query)))
                .optional().map_err(error)?.flatten();
            if let Some(value) = value {
                scored.push((id, value));
            }
        }
        Ok(limited)
    }
}

pub(super) fn score(blob: &[u8], query: &[f32]) -> Option<f64> {
    let values = blob.chunks_exact(4).map(|bytes| f32::from_le_bytes(bytes.try_into().expect("f32 bytes")));
    let value: f64 = values.zip(query).map(|(value, query)| f64::from(value) * f64::from(*query)).sum();
    value.is_finite().then_some(value)
}

pub(super) fn top(mut scored: Vec<(i64, f64)>) -> Vec<i64> {
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().take(DEPTH).map(|(id, _)| id).collect()
}
