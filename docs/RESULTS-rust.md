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


## Speed (Python 0.3.0 against Rust 0.4.0)

Method: the same read-only copy of DCB's lore (6773 records in 105 files),
text only (no embedding model, so the two are compared on their own work),
every command a fresh process as an agent calls it, each build into an empty
index cache. Python 0.3.0, Rust before the build fixes and Rust 0.4.0 ran one
after the other in each of 4 rounds, so machine load hit all three alike;
medians. `find` is 40 bench queries alternating the binaries.

| | Python 0.3.0 | Rust, first port | Rust 0.4.0 |
|---|---|---|---|
| start (`lore --version`) | 192 ms | 15 ms | **15 ms** |
| text index build (`index --text-only --rebuild`) | 5.4 s | 2.1 s | **1.1 s** |
| `find`, median | 557 ms | 584 ms | **421 ms** |
| `lore check` | 2.6-4.6 s | 1.7 s | **1.7 s** |

What made the build fast: `--rebuild` used to build the index twice (open,
delete, build again); records were deep-copied to sort their keys twice per
record per build; the insert statements were prepared again for every record.
The index and every search result are unchanged: the rows, the FTS postings
and the output of 176 bench queries hash equal before and after.

An earlier single run of this table showed the Rust build at 10 s against
Python's 3.5 s; it did not reproduce (the machine was loaded by other builds at
the time) and is withdrawn.

What a `find` still spends (about 420 ms) is mostly fixed overhead: reading and
parsing all records and the freshness check on every call, not the search
itself (a long query adds ~20 ms). Removing it is the next step.
