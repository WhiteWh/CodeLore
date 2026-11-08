# RustCodeLore search: DCB lore

Method: the owner's bench (`tools/lore_bench.py`, 88 rows asked as `exact` and
`para`, 88 queries each) on the read-only copy of DCB's lore (6773 records);
a result answers when one of the row's keys is in the record's strings. Rust is
the `lore` binary (`lore --json -C <copy> find <query> -n 10`), as the bench
invokes it.

## Text only (built-in, no vectors)

| weights signals/title/main/body | exact hit@1 | exact hit@5 | para hit@1 | para hit@5 |
|---|---|---|---|---|
| before: 10 / 5 / 3 / 1 | 93.2% (82/88) | 97.7% (86/88) | 31.8% (28/88) | 51.1% (45/88) |
| after: 50 / 20 / 1 / 1 | **94.3% (83/88)** | **98.9% (87/88)** | **40.9% (36/88)** | **64.8% (57/88)** |

The owner's Python baseline is exact 93.2/97.7, paraphrase 31.8/51.1; the tuned
weights beat it on both classes. The "after" row was measured through the CLI
with `--text-only`; the "before" row through the task-143 harness on the same
corpus.

## With embeddings

The semantic after run did not finish: with `provider = "builtin"` and
`LORE_MODEL_DIR` pointing at the task-146 files
(`...\lore-model`: config.json, model.safetensors 547 MB, tokenizer.json), one
`lore find` spent more than 30 minutes embedding the 6773 records and was still
running, so the bench was stopped. The owner/Python baseline for the same
corpus is exact 94.3/98.9, paraphrase 45.5/75.0. The after numbers with the
built-in vectors are therefore **not measured**; do not read the baseline as
an after result.

Corpus and queries are never committed; only this table and the method are.


## Speed

The same read-only copy of DCB's lore (6773 records in 105 files), text only,
every command a fresh process as an agent calls it, each build into an empty
index cache; medians of 4 rounds.

| | lore 0.4.0 |
|---|---|
| start (`lore --version`) | 15 ms |
| text index build (`index --text-only --rebuild`) | 1.1 s |
| `find` (40 bench queries) | 421 ms (145 ms after task 257, below) |
| `lore check` | 1.7 s |

What made the build fast: `--rebuild` used to build the index twice (open,
delete, build again); records were deep-copied to sort their keys twice per
record per build; the insert statements were prepared again for every record.
The index and every search result are unchanged: the rows, the FTS postings
and the output of 176 bench queries hash equal before and after.

## Task 257: the index answers find, show and plans

Method: the same read-only copy of DCB's lore (6773 records), text only, one
fresh process per command, an empty index cache first. `old` is the Rust 0.4.0
port before task 257 (`lore-257new`, the measured 421 ms build); `new` is task
257's index answer. Forty bench queries, 3 alternating rounds for `find`; 20
`show`/`plans` calls after one `index --text-only --rebuild`. The machine was
loaded by other builds, so read these as paired relative numbers, not as an
idle-machine absolute.

| | old | new |
|---|---|---|
| `find`, median | 296 ms | **145 ms** |
| `find`, p90 | 992 ms | **270 ms** |
| `show`, median | 335 ms | **62 ms** |
| `plans`, median | 336 ms | **162 ms** |

A quieter run of the same `new` binary measured `find` median 78 ms and p90
108 ms; the earlier `3e08cbc` measured 99/141. `show` and `plans` now read the
index's stored JSON and refresh it by file size and mtime; only a miss (a record
that lives only in git history) reads the store. All 176 bench query outputs are
byte-identical between old and new (`176` compared, `0` different).

What is left of the p90 tail: the refresh stat of every lore file, the
per-process SQLite open and page-cache warmup, and queries whose phrase/signal
postings match many records before the limit is applied. A warm path (not
built): a single long-lived `hgs lore` server over the existing index, so the
open and the freshness scan are paid once, with the same Finder answer; it
would need a bounded cache keyed by root and index generation.


## lore against plain search (2026-11-09, lore 0.4.1)

The table in the README, with its method. The repository: DCB at `804aef7d`,
20.7 MB of markdown in `docs/` and 4 526 source files (`.h .hpp .inl .c .cpp
.cxx .py .fx .hlsl .glsl .frag .vert .rs .cs .ps1 .bat`). The questions:
DCB's `tools/lore_bench_queries.jsonl`, 88 faults of its troubleshooting page,
each asked **exact** (the text it prints) and as a **paraphrase** (held to share
no three-word run and no quoted name with the row). Each carries keys, strings
of the row's fix; a result answers when it holds one, judged the same way for
every system.

- **grep over the markdown**: `git grep -n -i -I` at the revision over
  `docs/*.md`, as an agent's Grep tool: an exact question fixed-string; a
  paraphrase, or an exact one that matched nothing, by its two rarest words (a
  second call). A result is the matched line with two lines either side.
- **grep over the code**: the same, over the source files above.
- **lore, text only**: `lore find -n 10 --text-only` in a lore of 1 527 records:
  the page's faults written as records, among its other 1 490 rows converted
  mechanically, so the records are searched among as many as a whole migration
  makes. A result is the record as `lore find` prints it.

Tokens are bytes / 4 of everything shown up to and including the answering
result (everything, on a miss).

| search | question | n | answer first | in top 3 | in top 10 | MRR | tokens to the answer, median | p90 | calls | time, median | p90 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| grep over the markdown | exact | 88 | 66% | 82% | 93% | 0.76 | ~1 120 | ~2 740 | 1.36 | 0.10 s | 0.59 s |
| grep over the markdown | paraphrase | 88 | 5% | 6% | 10% | 0.06 | ~3 050 | ~20 060 | 1.00 | 0.09 s | 0.51 s |
| grep over the code | exact | 88 | 2% | 3% | 3% | 0.03 | ~580 | ~8 050 | 1.85 | 0.34 s | 1.19 s |
| grep over the code | paraphrase | 88 | 0% | 0% | 0% | 0.00 | ~1 970 | ~11 120 | 1.00 | 0.17 s | 0.53 s |
| lore, text only | exact | 88 | 92% | 100% | 100% | 0.96 | ~290 | ~490 | 1.00 | 0.09 s | 0.44 s |
| lore, text only | paraphrase | 88 | 45% | 65% | 82% | 0.58 | ~600 | ~3 190 | 1.00 | 0.09 s | 0.45 s |

Over all 176 questions the median an agent reads is ~1 485 tokens with grep
over the markdown and ~335 with lore. A grep that misses costs more than its
row: the agent searches again, and that search is not counted.

To run it again: DCB's `tools/lore_bench.py` (`--system grep --rev <sha>`,
`--system lore-text --root <full-size lore>`; `--build-full` makes the lore).
The code row used a copy of it whose corpus is the source files instead of
`docs/*.md`.
