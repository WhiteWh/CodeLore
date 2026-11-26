//! SQLite projections shared by project and engine caches. The base tables
//! retain their schema-2 layout. Backfill and trigger installation publish
//! together, so a reader sees a complete projection or the original tables.
use super::{error, Connection, LoreError, OptionalExtension};

const VERSION_KEY: &str = "atlas_derived";

pub(super) const COUNTS: [(&str, &str, &str); 5] = [
    ("files", "files", "lang <> 'binary'"),
    ("chunks", "chunks", ""),
    ("symbols", "symbols", ""),
    ("imports", "imports", ""),
    ("edges", "edges", ""),
];

pub(super) fn ready(db: &Connection) -> Result<bool, LoreError> {
    db.query_row("SELECT 1 FROM meta WHERE key = ?1 AND value = '1'", [VERSION_KEY], |_| Ok(()))
        .optional()
        .map(|row| row.is_some())
        .map_err(error)
}

/// A per-model clock also advances when a vector is replaced at the same
/// size, independently of both coverage and the text revision.
pub(super) fn install_ann_clock(db: &Connection) -> Result<(), LoreError> {
    if db.query_row("SELECT 1 FROM meta WHERE key='ann_vector_clock'", [], |_| Ok(()))
        .optional().map_err(error)?.is_some()
    {
        return Ok(());
    }
    let install = |db: &Connection| {
        for (event, keys) in [("INSERT", &["new"][..]), ("DELETE", &["old"][..]), ("UPDATE", &["old", "new"][..])] {
            let statements = keys.iter().map(|row| format!(
                "INSERT INTO meta VALUES ('ann_vector_generation:' || {row}.model, '1')
                 ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1;"
            )).collect::<String>();
            db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS atlas_ann_clock_{event}
                AFTER {event} ON vectors BEGIN {statements} END;")).map_err(error)?;
        }
        db.execute("INSERT OR IGNORE INTO meta VALUES ('ann_vector_clock', lower(hex(randomblob(16))))", []).map_err(error)?;
        Ok::<_, LoreError>(())
    };
    if db.is_autocommit() {
        let tx = db.unchecked_transaction().map_err(error)?;
        install(&tx)?;
        tx.commit().map_err(error)?;
    } else {
        install(db)?;
    }
    Ok(())
}

// One key per distinct qualified name and suffix. The NOCASE B-trees use the
// same ASCII comparison as the old suffix predicates. No symbol rowids are
// stored, so VACUUM and reuse of deleted rowids do not affect the lookup.
fn insert_keys(names: &str) -> String {
    format!(
        "INSERT OR IGNORE INTO atlas_short_names(short, name)
         SELECT suffix, name FROM (
             WITH RECURSIVE tails(name, rest, suffix) AS (
                 SELECT name, name, NULL FROM ({names})
                 UNION ALL
                 SELECT name, substr(rest, instr(rest, ':') + 1),
                        CASE WHEN substr(rest, instr(rest, ':') + 1, 1) = ':'
                             THEN substr(rest, instr(rest, ':') + 2) END
                 FROM tails WHERE instr(rest, ':') > 0
             ), dots(name, rest, suffix) AS (
                 SELECT name, name, NULL FROM ({names})
                 UNION ALL
                 SELECT name, substr(rest, instr(rest, '.') + 1), substr(rest, instr(rest, '.') + 1)
                 FROM dots WHERE instr(rest, '.') > 0
             )
             SELECT suffix, name FROM tails WHERE suffix <> ''
             UNION ALL
             SELECT suffix, name FROM dots WHERE suffix <> ''
         );"
    )
}

/// Run on the caller's write transaction, including for a newly created map.
pub(super) fn install(db: &Connection) -> Result<(), LoreError> {
    if ready(db)? {
        return Ok(());
    }
    db.execute_batch(include_str!("derived.sql")).map_err(error)?;
    let keys = insert_keys(
        "SELECT DISTINCT name COLLATE NOCASE AS name FROM symbols
         WHERE instr(name, '::') > 0 OR instr(name, '.') > 0",
    );
    let new_keys = insert_keys("SELECT new.name AS name");
    let remove_keys = "DELETE FROM atlas_short_names WHERE name = old.name COLLATE NOCASE
        AND NOT EXISTS (SELECT 1 FROM symbols WHERE name = old.name COLLATE NOCASE);";
    db.execute_batch(&format!(
        "{keys}
         CREATE TRIGGER atlas_symbol_keys_insert AFTER INSERT ON symbols
         WHEN instr(new.name, '::') > 0 OR instr(new.name, '.') > 0
         BEGIN {new_keys} END;
         CREATE TRIGGER atlas_symbol_keys_delete AFTER DELETE ON symbols
         WHEN instr(old.name, '::') > 0 OR instr(old.name, '.') > 0
         BEGIN {remove_keys} END;
         CREATE TRIGGER atlas_symbol_keys_update AFTER UPDATE OF name ON symbols
         BEGIN {remove_keys} {new_keys} END;"
    ))
    .map_err(error)?;
    for (name, table, predicate) in COUNTS {
        let where_clause = if predicate.is_empty() { String::new() } else { format!(" WHERE {predicate}") };
        db.execute_batch(&format!(
            "INSERT INTO atlas_counts SELECT '{name}', count(*) FROM {table}{where_clause};"
        ))
        .map_err(error)?;
        for (event, row, delta) in [("INSERT", "new", 1), ("DELETE", "old", -1)] {
            let when = if predicate.is_empty() { String::new() } else { format!("WHEN {row}.{predicate}") };
            db.execute_batch(&format!(
                "CREATE TRIGGER atlas_count_{table}_{event} AFTER {event} ON {table} {when}
                 BEGIN UPDATE atlas_counts SET total = total + ({delta}) WHERE name = '{name}'; END;"
            ))
            .map_err(error)?;
        }
    }
    db.execute_batch(
        "CREATE TRIGGER atlas_count_files_update AFTER UPDATE OF lang ON files
         BEGIN UPDATE atlas_counts SET total = total
             + (new.lang IS NOT NULL AND new.lang <> 'binary')
             - (old.lang IS NOT NULL AND old.lang <> 'binary') WHERE name = 'files'; END;",
    )
    .map_err(error)?;
    db.execute("INSERT INTO meta VALUES (?1, '1')", [VERSION_KEY]).map_err(error)?;
    Ok(())
}
