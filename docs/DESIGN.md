# CodeLore - design

What `lore` is made of and why. The data format it reads and writes is
[FORMAT.md](FORMAT.md); the order it is built in is [ROADMAP.md](ROADMAP.md).

## 1. Shape

```
src/codelore/
 ├─ cli.py      the `lore` command
 ├─ config.py   lore.toml: found upwards from the working directory, unknown keys refused
 ├─ schema.py   the kinds, their fields and limits; refuses, never repairs
 ├─ store.py    reads and writes lore/*.jsonl: canonical lines, sorted, LF
 ├─ gitio.py    HEAD blobs, ref checks, the three commit modes
 ├─ ops.py      every write: fill, validate, references, plan rules, commit
 ├─ dedupe.py   the duplicate search before a new record
 ├─ index.py    the disposable SQLite index: two FTS5 tables + vectors
 ├─ embed.py    providers: ollama | none
 ├─ search.py   find / for: exact signals, BM25 x2, cosine, reciprocal rank fusion
 ├─ plans.py    counts, layers, readiness, stale
 └─ guide.md    the agent instructions, printed by `lore guide`
```

Standard library only: `sqlite3` (FTS5), `json`, `tomllib`, `urllib`,
`subprocess` for git. No numpy: cosine over a few thousand 768-float vectors is
milliseconds in pure Python with `array` and is not the bottleneck. One package,
one entry point.

## 2. Where things live

| what | where | in git |
|---|---|---|
| records | `<repo>/lore/*.jsonl` | yes - the only source of truth |
| configuration | `<repo>/lore.toml` | yes |
| index | user cache: `%LOCALAPPDATA%\codelore\<repo-key>\index.sqlite`, `~/.cache/codelore/<repo-key>/` | no |
| embedding cache | beside the index, keyed by model and content hash | no |

`<repo-key>` is a hash of the repository's absolute path. Deleting the cache
costs one rebuild and nothing else.

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
unless `--all`.

**No model.** If the provider does not answer, `find` runs on text alone and the
first line of its output says so. Whether the server is there is decided by a
probe held to `embed.timeout` (2 s by default); a server that is there may take
longer on its first call, while it loads the model, and is allowed to. A dead
provider never fails a search.

**Output.** The top records **whole** - five by default, each a few lines - not
chunks of them. `--json` gives the records as stored, with their scores.

`lore for <path|symbol>` is a different question with an exact answer: every
record whose `refs.files` holds the path (or a path under the directory) or
whose `refs.symbols` holds the symbol.

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
3. **Check references**: each commit exists (`git cat-file -e`), each file exists
   at `HEAD`, each record and dependency exists, dependencies make no cycle.
4. **Look for duplicates** (new knowledge records only): against every record of
   the same kind, and against the batch's earlier records. The evidence comes in
   two strengths, and only the strong kind refuses. **Strong**: a `signals`
   string another record already carries, embedding cosine at or above
   `dedupe.cosine` (0.95), or word overlap of title, main sentence and cause at
   or above `dedupe.jaccard` (0.5) - the write is refused, the matches named,
   and `lore update <id>` or `--new` asked for. **Near**: cosine at or above
   0.80 (or overlap at or above 0.25 without embeddings) - the write goes
   through and names the three nearest records, so the writer can merge.

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

`push = true` pushes the branch after the commit; a failed push is reported and
the commit stays.

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
