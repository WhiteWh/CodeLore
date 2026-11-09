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

## The ledger - structured data beside the lore

FORMAT.md §8: journals per writer that are only appended to, so nothing ever
merges; the state is a fold over the lines' `parents` graph, conflicts are
found by the graph and resolved by a line, never by clocks. `lore ledger
set|get|find|fold|log|conflicts|resolve|clear|check|status|promote|ack|export|repair`.
The state comes in one read with what it rests on (`fold --with-state`), and
`--strict` refuses one that is not complete. Pending lines move by exact id
under a lock, the journal written and synced first and the pending file
replaced whole; `--keep` and `ack` split the copy from its confirmation. An
unfinished last line is never read nor written after, and `repair` sets its
bytes aside. A parent must be a line of its own entry, on every path and
whichever line arrives first. Fields and kinds retire without losing their
history. The fold is linear; the cache holds each entry's fold and where its
lines are, and reads on from where it stopped.

Checks (`tests/ledger.rs`, `tests/ledger_contract.rs`): the fold against its
definition on random graphs; two writers on different fields keep both; one
field set by two concurrent lines is a conflict and the smaller id holds; an
offline chain against another edit is a conflict; a value set back makes no
false order; a wrong clock and the order files are read in change nothing;
numbers read back as written; unknown kinds, fields and types are refused by
name, file and line; a missing parent applies and the state is not complete;
an unfinished line is neither read nor dropped; the cache reads on and notices
a journal that was not only appended to, and a damaged cache or an unreadable
folder is never an empty state; promote moves exact lines with their parents
and loses none to a writer waiting on the lock; resolve keeps to the heads it
saw; a parent is a line of its own entry; two lines under one id are refused;
find answers as a full scan would; clear unsets what was seen; retired fields
and kinds are read, kept and never written; the written lines match a fixture
byte for byte; a lore without `[ledger]` is what it was.

Next: holding less than the whole of a cold read in memory (1M lines take
about 2 GB at their peak); an entry edited tens of thousands of times costs
its whole history on each refresh that touches it.

## 0.4.1 - what the first harness found

Zerus builds its own `lore` from a pinned revision of this crate and keeps a
project store with a sub-store per team. What it met, and what changed:

- the project's memory found nothing: its ~440 records were in the team
  sub-store, and `find` searched only the store it ran in. `find --stores all`
  searches a store and its sub-stores (or `--stores <folder>,...` the ones
  named), each result marked with its `store`; without the option `find` is
  byte for byte what it was.
- the built-in model was never fetched on an installed build, so search was
  text only for good. The `lore` binary now fetches the configured model on
  first need: `lore index` waits for it, `lore find` answers on text and starts
  one detached fetch, `lore init` starts it. `LORE_MODEL_FETCH=off` stops it; the
  library never downloads. On Windows the detached fetch first held the
  caller's stdout pipe open until the download ended (a `find` read through a
  pipe took 20 s); it no longer inherits it (0.7 s).
- an older pinned `lore` refused a whole lore.toml for a table a newer one
  wrote, which would take a project's memory down. An unknown top-level key is
  a warning on stderr now; keys inside a known table stay strict.
- under `shared-tree`, reading the store at the branch tip (every write, and
  `lore cover`) ran one `git cat-file blob` per file: about a second each on a
  busy Windows tree, a minute for 105 files. One `git cat-file --batch` reads
  them all (`lore cover` on that lore: 13.1 s to 1.8 s, the output byte for
  byte the same), and a path the commit lacks is an error, not an empty file.
- models were fetched per `LORE_CACHE`, so each install or harness store
  fetched its own 0.5-2 GB. They live in one shared folder now (`LORE_MODELS`,
  else the platform cache's `models`), and a copy an older build fetched is used.
- the `directml` feature: the same models' ONNX exports on ONNX Runtime's
  DirectML provider, any DirectX 12 GPU (AMD, Intel, NVIDIA) on Windows; the
  same vectors as the CPU (256 pieces: cosine 0.999999 at worst). DirectML.dll is
  fetched pinned and delay-loaded, so the binary starts without it and stays on
  the CPU where no GPU answers.
- two `lore 0.4.0` builds on one PATH could not be told apart. `lore --version`
  names the commit a build came from, `+` for uncommitted changes; a build made
  as another package's git dependency keeps the bare version its packaging
  checks.

## 0.3.0 - what a large migration found

Three agent sessions moved about 350 documents into a lore of 6 200 records in
100 files. What they met, and what changed:

- `lore check` took 9-15 minutes: one git process per reference. It is one
  process for all commits and one for all files now - 12 min 43 s to 0.7 s on a
  scratch lore of 6 000 records - and writes check a batch's references the
  same way. No incremental mode: nothing is left for it to save.
- `lore check --evidence`: a cited commit must touch a file the record cites or
  be named in its source document.
- format version 3: the `source` kind and `lore cover`, a document's coverage
  part by part (headings, table rows, numbered items) with skips and their
  reasons in the store; `--history` for a document that holds nothing current.
  A decision recorded twice is retired as `superseded`, like every other kind.
  An item has its own `outcome`, so a plan's 2000 characters summarise rather
  than hold every piece's verdict.
- a batch refused for one duplicate names it (`#n`) and `--skip-duplicates`
  writes the rest; a bare class name (`AttributeError`) is not a signal.
- a family of records whose titles name different things (one per setting) is
  not refused on word overlap; it is named.
- `update --json-in` takes the id from the patch; `commits` on an open item
  says that its commits go in `refs.commits`; a failed push is one line.
- the index falls back to the repository's git directory where the user cache
  cannot be written; `lore show` and the new `lore exists` find a record
  committed by another session before it reaches this working tree, and `+=`
  appends to the record the commit is built on.
- `superseded_by` and `reversed_by` are checked as record ids, and the refusal
  quotes the value received. Reported: «the pair of ids is measured against 64»;
  not reproduced with two 48-character ids in either order - if a shell joins
  the two tokens, the refusal now shows it.

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
