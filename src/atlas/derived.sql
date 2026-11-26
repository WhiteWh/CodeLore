CREATE TABLE atlas_short_names(
    short TEXT COLLATE NOCASE NOT NULL,
    name TEXT COLLATE NOCASE NOT NULL,
    PRIMARY KEY(short, name)
) WITHOUT ROWID;
CREATE INDEX atlas_short_names_by_name ON atlas_short_names(name);

CREATE TABLE atlas_counts(name TEXT PRIMARY KEY, total INTEGER NOT NULL) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS chunks_ehash ON chunks(ehash);
CREATE TABLE atlas_vector_sizes(
    hash TEXT NOT NULL,
    model TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY(hash, model)
) WITHOUT ROWID;
CREATE TABLE atlas_vector_counts(
    model TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    entries INTEGER NOT NULL,
    covered INTEGER NOT NULL,
    PRIMARY KEY(model, bytes)
) WITHOUT ROWID;

-- Coverage counts pieces, even when several pieces share one embedding.
-- A non-BLOB vector is included in the raw entry count only.
INSERT INTO atlas_vector_sizes
    SELECT hash, model, CASE WHEN typeof(vec) = 'blob' THEN length(vec) ELSE -1 END
    FROM vectors;
INSERT INTO atlas_vector_counts
    SELECT model, bytes, count(*),
           sum(CASE WHEN bytes >= 0 THEN
               (SELECT count(*) FROM chunks WHERE ehash = size.hash)
               ELSE 0 END)
    FROM atlas_vector_sizes size GROUP BY model, bytes;

-- Keep the previous shape independently to handle REPLACE with or without
-- SQLite's recursive_triggers setting. Replacing a blob of the same size
-- also leaves both counters unchanged.
CREATE TRIGGER atlas_vector_insert AFTER INSERT ON vectors BEGIN
    DELETE FROM atlas_vector_sizes WHERE hash = new.hash AND model = new.model;
    INSERT INTO atlas_vector_sizes VALUES
        (new.hash, new.model, CASE WHEN typeof(new.vec) = 'blob' THEN length(new.vec) ELSE -1 END);
END;
CREATE TRIGGER atlas_vector_delete AFTER DELETE ON vectors BEGIN
    DELETE FROM atlas_vector_sizes WHERE hash = old.hash AND model = old.model;
END;
CREATE TRIGGER atlas_vector_update AFTER UPDATE ON vectors BEGIN
    DELETE FROM atlas_vector_sizes WHERE hash = old.hash AND model = old.model;
    INSERT INTO atlas_vector_sizes VALUES
        (new.hash, new.model, CASE WHEN typeof(new.vec) = 'blob' THEN length(new.vec) ELSE -1 END);
END;
CREATE TRIGGER atlas_vector_size_insert AFTER INSERT ON atlas_vector_sizes BEGIN
    INSERT INTO atlas_vector_counts VALUES (
        new.model, new.bytes, 1,
        CASE WHEN new.bytes >= 0 THEN (SELECT count(*) FROM chunks WHERE ehash = new.hash) ELSE 0 END)
    ON CONFLICT(model, bytes) DO UPDATE SET
        entries = entries + 1,
        covered = covered + excluded.covered;
END;
CREATE TRIGGER atlas_vector_size_delete AFTER DELETE ON atlas_vector_sizes BEGIN
    UPDATE atlas_vector_counts SET
        entries = entries - 1,
        covered = covered -
            CASE WHEN old.bytes >= 0 THEN (SELECT count(*) FROM chunks WHERE ehash = old.hash) ELSE 0 END
    WHERE model = old.model AND bytes = old.bytes;
    DELETE FROM atlas_vector_counts WHERE model = old.model AND bytes = old.bytes AND entries = 0;
END;

CREATE TRIGGER atlas_piece_vector_insert AFTER INSERT ON chunks BEGIN
    UPDATE atlas_vector_counts SET covered = covered + 1 WHERE (model, bytes) IN
        (SELECT model, bytes FROM atlas_vector_sizes WHERE hash = new.ehash AND bytes >= 0);
END;
CREATE TRIGGER atlas_piece_vector_delete AFTER DELETE ON chunks BEGIN
    UPDATE atlas_vector_counts SET covered = covered - 1 WHERE (model, bytes) IN
        (SELECT model, bytes FROM atlas_vector_sizes WHERE hash = old.ehash AND bytes >= 0);
END;
CREATE TRIGGER atlas_piece_vector_update AFTER UPDATE OF ehash ON chunks WHEN old.ehash IS NOT new.ehash BEGIN
    UPDATE atlas_vector_counts SET covered = covered - 1 WHERE (model, bytes) IN
        (SELECT model, bytes FROM atlas_vector_sizes WHERE hash = old.ehash AND bytes >= 0);
    UPDATE atlas_vector_counts SET covered = covered + 1 WHERE (model, bytes) IN
        (SELECT model, bytes FROM atlas_vector_sizes WHERE hash = new.ehash AND bytes >= 0);
END;
