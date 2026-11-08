# CodeLore - design

What `lore` is made of and why. The data format it reads and writes is
[FORMAT.md](FORMAT.md); the order it is built in is [ROADMAP.md](ROADMAP.md).

## 1. Shape

```
src/
 ├─ main.rs, app.rs  the `lore` binary: `codelore::app::main()`, `lore model`, the model fetch
 ├─ cli.rs           the command layer (`cli::run`): parsing, texts, `--json`
 ├─ config.rs        lore.toml: found upwards from the working directory
 ├─ schema.rs        the kinds, their fields and limits; refuses, never repairs
 ├─ store.rs         reads and writes lore/*.jsonl: canonical lines, sorted, LF
 ├─ gitio.rs         HEAD blobs, ref checks, the three commit modes
 ├─ ops.rs, writer.rs  every write: fill, validate, references, plan rules, commit
 ├─ dedupe.rs        the duplicate search before a new record
 ├─ index.rs         the disposable SQLite index: FTS5 tables, narrow fact tables, vectors
 ├─ embed/           the built-in models on candle: nomic-embed-text-v1.5, e5, bge-m3; fetch and pin
 ├─ finder.rs, builtin.rs, search.rs  find / for: signals, BM25 x2, cosine, reciprocal rank fusion
 ├─ plans.rs         counts, layers, readiness, stale
 ├─ cover.rs         a source document's parts and what accounts for each
 ├─ working.rs       the working tier (pulses, messages, briefs) a harness such as Zerus uses
 ├─ secrets.rs       the secret scan every write passes
 └─ guide.md         the agent instructions, printed by `lore guide`
```

One crate, one binary. SQLite with FTS5 is bundled (`rusqlite`), git is the one
outside program. The crate is also a library: another package builds its own
`lore` with `fn main() { codelore::app::main() }` and pins the revision (Zerus
does); its default features are the binary every such package gets, so a new
part comes behind a feature that is off by default.

## 2. Where things live

| what | where | in git |
|---|---|---|
| records | `<repo>/lore/*.jsonl` | yes - the only source of truth |
| configuration | `<repo>/lore.toml` | yes |
| index | `$LORE_CACHE/<repo-key>/`, else the user cache: `%LOCALAPPDATA%\codelore\<repo-key>\index.sqlite`, `~/.cache/codelore/<repo-key>/`; where that cannot be written, `.git/codelore/` | no |
| embedding cache | beside the index, keyed by model and content hash | no |
| embedding model | `$LORE_MODELS/<model>/`, else `<platform cache>/models/<model>/` - one folder for every `lore` on the account, whatever `LORE_CACHE` says; pinned files, fetched on first use | no |
| DirectML runtime (`directml` builds) | `<models>/runtimes/directml-ort-1.31.0/DirectML.dll`, pinned | no |

`<repo-key>` is a hash of the repository's absolute path. Deleting the cache
costs one rebuild and nothing else. A sandboxed agent may be unable to create a
folder in the user cache while it can write the repository; the index then goes
to the repository's git directory - never tracked, never committed - and a line
on stderr says where and why.

## 3. Reading: the index and the search

**Freshness.** Every command that reads first compares each JSONL file's size,
mtime and git blob hash with what the index recorded; a changed file is
re-read whole (they are small) and its rows replaced. No watcher, no daemon.

**Text.** Two FTS5 tables. `words` holds the prose, stemmed (`porter
unicode61`, `_` kept inside a word), with columns weighted for BM25: `signals`
10, `title` 5, the record's main sentence (`symptom`, `what`, `question`,
`goal`) 3, everything else 1. `terms` holds only the code-like tokens pulled out
of every field and of `refs` - dotted names, paths, `snake_case`, hex codes,
`ALLCAPS` - with `.`, `/`, `:`, `-` and `_` kept inside a token, so
`scene.depth_buffer`, `src/render/post.py`, `ERR_DEVICE_LOST` and `0x80070005`
match whole. One table could not do both: the characters that keep a code
together would glue a word to the full stop after it.

**Exact signals.** Before ranking, the query is checked against every stored
`signals` string as a substring both ways; a hit is put first. An error pasted
from a terminal finds its record even when the surrounding text matches
nothing.

**Phrases.** A query of 16 characters or more that appears word for word in a
record's text (case, whitespace and quoting marks folded) puts that record
first among the non-signal results - what a fixed-string grep would find. A bag
of common words otherwise ranks a long record full of them above the short one
that says exactly the phrase.

**Meaning.** Each record has one embedding of `title + symptom|what|question|goal
+ cause + signals`, with the model's document prefix (`search_document: ` for
nomic models); a query gets the query prefix. Vectors are cached by model and
by the hash of that text, so an index rebuild embeds nothing twice and a model
change re-embeds once. Cosine similarity against all vectors, top 50.

**Fusion.** Reciprocal rank fusion, `k = 60`, over the `words`, `terms` and
meaning lists; a whole phrase adds half a point, an exact signal a full point,
longest match first. Filters (`--kind`, `--area`, `--status`, `--plan`) apply before ranking;
the default hides `obsolete`, `superseded`, `reversed` and closed plans' items
unless `--all`, and source records - ledgers of documents, not knowledge -
unless `--kind source`.

**No model.** If the model is not installed, `find` runs on text alone and the
first line of its output says so. The `lore` binary then fetches it - once,
detached from the search, which answers at once and does not hold its caller's
output pipe open (a marker beside the model keeps a second search from starting
another fetch); `lore index` and `lore init` fetch it themselves. Each file is
pinned by size and SHA-256 and streamed into place only when it matches.
`LORE_MODEL_FETCH=off` stops all of it; the library (`cli::run` called by tests
or a harness) never downloads. A missing model never fails a search.

**Several stores.** A store may hold sub-stores - folders with a lore.toml of
their own, such as a Zerus project store's `teams/<team>`. `find` searches the
store it runs in; `find --stores all` searches it and every sub-store (four
levels deep at most), `--stores teams/a,teams/b` the ones named. Each store is
searched with its own index and configuration, every result is marked with its
store (`.` for the one `find` runs in), and the lists are merged by score -
reciprocal-rank sums of the same lists, so they compare. A store that cannot be
read is named in a note; the others still answer.

**Output.** The top records **whole** - five by default, each a few lines - not
chunks of them. `--json` gives the records as stored, with their scores.

`lore for <path|symbol>` is a different question with an exact answer: every
record whose `refs.files` holds the path (or a path under the directory) or
whose `refs.symbols` holds the symbol, and the source record of a document.

`lore show` and `lore exists` read the index, and for an id it lacks, the
branch the next write builds on: under `shared-tree` another session's commit
can be in the branch before its line is in this working tree.

`lore find -` reads stdin and keeps the lines that look like errors (exception
lines, `error:`/`fatal:`, HRESULTs, exit codes) as the query, so a whole
traceback can be pasted.

## 4. Writing

Every write goes through one path:

1. **Parse** the record (flags, or JSON on stdin with `--json`, or JSONL with
   `--batch`).
2. **Validate** against the schema: required fields, types, lengths, statuses,
   the transitions an item may make (`open` → anything; a closed item reopens
   only with `--reopen` and a reason).
3. **Check references**: each commit exists, each file exists at `HEAD` - all of
   a change's, in one `git cat-file --batch-check` - each record and dependency
   exists, dependencies make no cycle. A signal the change adds may not be a
   bare class name or one word (`AttributeError`, `Traceback`): every fault of
   its family prints it, so it finds them all and makes each next one look like
   a duplicate.
4. **Look for duplicates** (new knowledge records only): against every record of
   the same kind, and against the batch's earlier records. The evidence comes in
   two strengths, and only the strong kind refuses. **Strong**: a `signals`
   string another record already carries, embedding cosine at or above
   `dedupe.cosine` (0.95), or word overlap of title, main sentence and cause at
   or above `dedupe.jaccard` (0.5) - the write is refused, the matches named,
   and `lore update <id>` or `--new` asked for. **Near**: cosine at or above
   0.80 (or overlap at or above 0.25 without embeddings) - the write goes
   through and names the three nearest records, so the writer can merge.

   Word overlap does not refuse when each title names something - a quoted
   or backticked span, a code-like token, a CamelCase word - that the other
   record never mentions. On the first large migration a family of 18 topics,
   one per settings object («`BloomSettings` - the bloom's settings»), shared a
   title template and a vocabulary at 0.5-0.67 overlap and said different
   things; such a pair is only named.

   A batch is refused whole, and the refusal names each record by its place
   (`#n`); `--skip-duplicates` writes the others and lists the ones left out.

   Why similarity alone does not refuse, measured on the first real lore (37
   records about git in a shared tree, nomic-embed-text): the rows that were
   one fault scored 0.69-0.85 against each other, three different faults
   0.87-0.88, the median of all 666 pairs 0.69. A domain where everything is
   about the same few things has no cosine that separates the two. Word overlap
   did: the highest between two different records was 0.32.
5. **Write and commit** by the configured mode (§5).

Refusals are loud: the field, the rule, what to do. Nothing is half-written.

## 5. The three commit modes

- **`none`** - edit the file in the working tree; committing is the user's.
- **`plain`** - edit the file, then `git commit --only -- <file>`: the commit holds
  that file and nothing else, whatever else is staged. The default.
- **`shared-tree`** - for a working tree several sessions edit at once, where
  the live file may hold someone else's uncommitted line:
  1. read the file's blob **from `HEAD`**, not from disk;
  2. apply this one change to it;
  3. build the tree in a private `GIT_INDEX_FILE` (`read-tree HEAD`,
     `update-index --cacheinfo`), `commit-tree` on `HEAD`;
  4. `update-ref refs/heads/<branch> <new> <old>` - if anyone moved the branch
     in between, the update fails, and the write starts again from step 1 on
     the new `HEAD` (three tries, then a refusal);
  5. bring the live file up to date **only if** it equalled the old `HEAD` blob,
     or a version the file held at an earlier commit - a copy an earlier write
     could not replace; otherwise leave it and say so - the user's or another
     session's edit is in it. On Windows a file another process has open
     cannot be replaced: the replace is retried for about four seconds, and if
     the file never frees the commit stands, the live file stays as it was, a
     note says so, and the next write brings it up.

  The shared `.git/index` is touched in one case only: the entry for a file
  this write changed, when that entry still held the old `HEAD` blob (or was
  absent for a new file) - so `git status` stays clean for it. An entry
  somebody staged is left alone, and no other path is touched.

`push = true` pushes the branch after the commit. A failed push leaves the commit
where it is and says so in one line - `committed <sha> locally, push to
<remote>/<branch> failed: <git's reason>; the next write's push carries it` -
which is true, since a push sends the branch with every commit not yet on the
remote.

## 6. Plans

- `lore plan new`, `lore item add`, `lore item done|drop|supersede|defer` are
  writes like any other (§4), one commit each, or one commit for a `--batch`.
- **Ready** items: `open`, every hard dependency `done`, every soft one
  started elsewhere (CodeLore cannot see "started", so for it a soft dependency
  only orders the list; an orchestrator decides).
- `lore plan close` refuses while an item is `open`, and asks for the outcome.
- `lore stale --days N`: open items whose record has not changed in N days and
  whose `refs.files` no commit has touched since. It is a review list: each line
  ends in "keep, drop, or defer".
- `lore plans` lists live plans with counts; `lore plan show <id>` lists items in
  dependency layers.

**Orchestrators** use the same commands with `--json`. The expected flow: the
orchestrator plans; on approval it writes the plan and items (`--batch`); when
it accepts a piece of work it marks the item `done` with the commits; when it
cancels one for good it marks it `dropped` with the reason; faults its agents
report go in with `lore add fault`. Who holds which item, attempts and delivery
stay in the orchestrator. CodeLore never calls an orchestrator.

## 7. Checks

`lore check` is the gate a project runs in CI or before a commit: every file
parses canonically and sorted, every record validates, ids are unique, refs
resolve (missing files are reported, not failed - the record may now be
`obsolete`), dependencies are acyclic, no closed plan has an open item. Exit
code 1 on any failure. `lore fmt` reads hand-edited files leniently and writes
them back canonical and sorted - the one repair, and only when asked.

References are checked in one git process for all commits and one for all
files: one process per reference took 12 minutes 43 seconds over 6 000 records
on Windows, the batch takes under a second, so there is no incremental mode to
trust. A `covered` source whose document has since changed is a warning.

`lore check --evidence` asks more of a cited commit than that it exists: that it
touches one of the record's `refs.files`, or that its hash is written in a
source document whose ledger names the record. A hash made up by an agent is
caught by existence; one that happens to exist is caught here.

## 7a. Sources

A project's documents moved into lore leave a `source` record each: which
records hold each part of the document, which parts were skipped and why. The
parts are found in the Markdown (headings, table rows, numbered items, and what
a heading says itself when it has parts under it - FORMAT.md §4), named the way
they are written so a person can find the place. `lore cover <doc>` lists what
is unaccounted for, `--add` merges ledger entries, `--history` marks a document
that holds nothing current with the records that carry what still stands. The
ledger lives in the store, not beside a migration script, so an archived
document still says where each of its facts went when no record links it.

## 8. Agents

`lore guide` prints, from text shipped in the package: when to search (before
diagnosing anything, before editing a file - `lore for`), how to search (exact
strings first, then words), when to record (a fault met that the next agent
would meet again; a number a change is judged by; a decision with its reason),
how to record (one record, no story, the commit carries the story), and how to
work a plan item. `lore init` writes `lore.toml`, the `.gitattributes` line and
prints the one line for `AGENTS.md`.

## 9. Traps, named in advance

- **CRLF.** On Windows with `core.autocrlf`, a JSONL file in the working tree may
  be CRLF while its blob is LF. `lore` reads blobs for writing and normalises
  only text it parses; the `.gitattributes` line removes the question.
- **A model change** makes every vector stale. The embedding cache is keyed by
  model name, so a change re-embeds everything once and mixes nothing.
- **A busy GPU.** The embedding model can be refused or slow when another program
  holds the card; the text search does not wait for it.
- **Ids that move.** Renaming an id breaks references; `lore` refuses to rename
  an id that something references and offers `superseded` instead.
- **Long lines in diffs.** A record is one line, so a diff of a changed record is
  one changed line, whole. That is the trade for merge-safety and is accepted.
