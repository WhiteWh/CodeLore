# CodeLore - roadmap

Each milestone ends with its checks green. Nothing is called done on one
example: the checks run over scratch repositories in `tests/` **and** over the
first real lore (the migration of the first production user's docs).

| milestone | 0.1.0 | what is left |
|---|---|---|
| M0 format and the gate | built | - |
| M1 writing and committing | built | - |
| M2 index and text search | built | - |
| M3 embeddings and fusion | built | the ranking measured on a real lore's query set |
| M4 plans | built | - |
| M5 agents and packaging | built; the duplicate rule set from the first real lore (0.1.1) | a PyPI release |

## M0 - format and the gate

The schema of [FORMAT.md](FORMAT.md) as code; the store reads and writes
canonical sorted JSONL; `lore check`, `lore fmt`.

Checks (`tests/test_store_schema.py`): round-trip keeps the bytes and unknown
fields; items sort by number; each rule has a case that breaks it and sees the
refusal; a file with CRLF, a BOM, an unsorted line, a blank line, a repeated id
or a non-canonical line is refused by name; `[limits]` only lowers.

## M1 - writing and committing

`lore add` / `lore update` with validation, ref checks and the three commit
modes.

Checks (`tests/test_write.py`): `plain` commits only its file while another is
staged; a refused write leaves no commit and no file; `shared-tree` leaves a
foreign uncommitted record in the live file, keeps it out of the commit, leaves
the shared index's other entries alone and `git status` clean for its own file;
a branch moved between read and update is retried on the new head and both
commits land; it refuses on another branch; `none` writes and does not commit.

## M2 - the index and the text search

The cache index, freshness by size, mtime and content hash, two FTS5 tables,
exact signals, `lore find` (and `find -` for a pasted log) and `lore for`.

Checks (`tests/test_search_plans.py`): an exact signal ranks first wherever it
sits in the query; `scene.depth_buffer`, `ERR_DEVICE_LOST` and `0x80070005`
are one term each; a changed record is found by its new text and not its old;
retired records are hidden unless `--all`; a pasted traceback finds its record;
`for` answers for a file, its directory, an absolute path and a symbol.

## M3 - embeddings and fusion

The `ollama` and `none` providers, the embedding cache, RRF.

Checks: with the server not listening, `find` answers on text within four
seconds and says so; with a deterministic fake provider the meaning list joins
the fusion. **Left**: a fixed query set from the first real lore - exact
strings and paraphrases - read text-only and fused, the right record's rank
recorded for each, and fusion not worse on any.

## M4 - plans

`plan` and `item` commands, dependency rules, `plans`, `plan show`, `items
--ready`, `stale`, `plan close`, `--batch`.

Checks: done needs a commit that exists; dropped needs a reason and who
decided; reopen needs a reason and clears the old status's fields; a done item
does not silently become open; a plan with an open or deferred item does not
close, and a closed one takes no new item; a cycle is refused naming its
members; an unknown dependency is refused; `stale` lists exactly the items
untouched in the window.

## M5 - agents and packaging

`lore guide`, `lore init`, an editable install, shims.

Checks: `lore init` in an empty repository produces a configuration that
`lore check` accepts; every command the guide shows exists; a shared signal
refuses and a neighbour is only named. **The duplicate rule** comes from the
first real lore - a pilot of 37 records - where cosine did not separate
duplicates (0.69-0.85) from different faults (0.87-0.88) and word overlap did
(different records at most 0.32): similarity names, only strong evidence
refuses (DESIGN.md §4). **Left**: a PyPI release.

## 0.2.1 - retiring a duplicate

- a fault recorded twice is folded into its twin and keeps its line: status
  `superseded`, `superseded_by` the twin; a search hides it like any retired
  record. An amendment to format version 2 (one more status), made the day the
  version was published, before any other reader existed.
- `-s field+=value` on a text field refuses by name and says to give the whole
  text; += appends to lists only.

## 0.2.0 - topics and recipes

- two kinds for the knowledge that is not a fault or a number: `topic`, how
  something works now (up to 3000 characters, the present only), and
  `recipe`, a procedure (`when`, `steps`, `check`). A project's whole
  documentation can now be records, not only its troubleshooting.
- format version 2: version 1 records stay valid as written and take version 2
  when next written; a topic or recipe needs version 2.
- an embedding reads the first 1 500 characters of a long main text.

## 0.1.2 - a copied phrase

- a query of 16 characters or more found word for word in a record's text puts
  that record above any bag-of-words ranking (+0.5, below an exact signal's
  +1). On the first real lore at full size (37 records among 1 490 others) the exact
  phrase «the file it wrote differs from HEAD on every line» ranked its own
  record below ten others that merely used its common words; a fixed-string
  grep finds it, and lore now does what grep does well.

## 0.1.1 - what the first real lore found

- stdin was read in the console's code page: on Windows `«»` and `…` reached a
  record as two or three characters each. It is read as UTF-8 bytes now, and a
  test feeds it through a cp1252 console.
- the duplicate search refused on cosine 0.86, which on a real lore refuses
  different faults and misses real duplicates; see M5.
- `lore update --batch`, one commit for many patches; a write that changes
  nothing but its stamps makes no commit, and a commit's subject counts the
  records that changed, not the ones asked for.
