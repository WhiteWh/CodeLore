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
