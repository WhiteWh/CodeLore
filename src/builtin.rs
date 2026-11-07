//! The accepted FTS5 index (task 143) plugged into the command layer's
//! [`Finder`] seam: words and whole terms from the index, meaning from the
//! built-in embedder (task 146), fused by reciprocal rank as CodeLore does.
use crate::config::Config;
use crate::embed;
use crate::finder;
use crate::index;
use crate::search::{self, Filters, Finder, Found, IndexInfo};
use crate::{LoreError, Result};
use serde_json::Value;

/// The search the `lore` binary hands `cli::run`.
pub struct Builtin;

/// A static instance, because the command layer borrows the finder.
pub static BUILTIN: Builtin = Builtin;

impl Builtin {
    /// The embedder, when the configuration and the machine have one.
    fn semantic(&self, config: &Config, text_only: bool) -> (Option<embed::Shared>, Vec<String>) {
        let mut notes = Vec::new();
        if text_only {
            return (None, notes);
        }
        if config.embed_provider == "none" {
            notes.push("semantic search is off (embed.provider = \"none\"); text only".to_owned());
            return (None, notes);
        }
        let selected = config.embedding_model();
        match embed::Embedder::open_model(selected, embed::ModelSource::Cache).map(|embedder| std::sync::Arc::new(embedder) as embed::Shared) {
            Ok(shared) => (Some(shared), notes),
            Err(_error) => {
                let status = embed::status_for(selected, &embed::model_dir(selected));
                notes.push(format!(
                    "semantic search unavailable: {}; text only",
                    if status.reason.is_empty() { "the model is not installed".to_owned() } else { status.reason }
                ));
                (None, notes)
            }
        }
    }
}

impl Finder for Builtin {
    fn find(
        &self,
        config: &Config,
        records: &[Value],
        query: &str,
        filters: &Filters,
        limit: usize,
        text_only: bool,
    ) -> Result<(Vec<Found>, Vec<String>)> {
        let index = index::open_index(config)?;
        let (shared, mut notes) = self.semantic(config, text_only);
        let wanted = finder::Filters {
            kind: filters.kind.clone(),
            area: filters.area.clone(),
            status: filters.status.clone(),
            plan: filters.plan.clone(),
            all: filters.all,
            sources: filters.sources,
        };
        let selected = config.embedding_model();
        let semantic = shared.as_ref().map(|shared| finder::Semantic {
            embedder: shared.as_ref(),
            model: selected.id(),
            doc_prefix: selected.document_prefix(),
            query_prefix: selected.query_prefix(),
        });
        let (hits, mut more) = finder::find(&index, query, &wanted, limit, semantic.as_ref())?;
        notes.append(&mut more);
        let visible = search::visible(records, filters);
        let found = hits
            .into_iter()
            .filter_map(|hit| {
                visible.get(&hit.id).map(|record| Found { record: record.clone(), score: hit.score, via: hit.via, meaning: hit.meaning })
            })
            .collect();
        Ok((found, notes))
    }

    fn index(&self, config: &Config, _records: &[Value], rebuild: bool, text_only: bool) -> Result<IndexInfo> {
        let mut index = index::open_index(config)?;
        if rebuild {
            let path = index.path().to_path_buf();
            drop(index);
            for suffix in ["", "-wal", "-shm", "-journal"] {
                let file = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
                if file.exists() {
                    std::fs::remove_file(&file)
                        .map_err(|error| LoreError::new(format!("{}: {error}", file.display())))?;
                }
            }
            index = index::open_index(config)?;
        }
        let (shared, mut notes) = self.semantic(config, text_only);
        let mut vectors = 0;
        let mut semantic = false;
        if let Some(shared) = &shared {
            let selected = config.embedding_model();
            match index.ensure_vectors(shared.as_ref(), selected.id(), selected.document_prefix()) {
                Ok(()) => {
                    semantic = true;
                    vectors = index.vectors(selected.id()).map(|vectors| vectors.len()).unwrap_or(0);
                }
                Err(error) => notes.push(format!("semantic search unavailable: {error}; text only")),
            }
        }
        let info = IndexInfo {
            records: index.count()?,
            vectors,
            index: index.path().display().to_string(),
            semantic,
            note: if notes.is_empty() { None } else { Some(notes.join("; ")) },
        };
        Ok(info)
    }
}
