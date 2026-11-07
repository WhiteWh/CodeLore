# lore-parity: Python CodeLore baseline vectors and rankings for the Rust parity checks

Task 148. Everything here is synthetic: no DCB record or query is included, so this
fixture can live in the repository. The DCB numbers are in `baseline.json` as
numbers only.

## Layout

| file | what it is |
|---|---|
| `corpus.jsonl` | 50 synthetic records, one JSON per line: `id`, `title`, `text` (the exact CodeLore embed text: title, newline, symptom/main text, cause, signals) |
| `queries.jsonl` | 10 synthetic queries: `id`, `query`, `expected` (the corpus id that should answer) |
| `vectors.json` | `model` (`nomic-embed-text`), `dim` (768), `normalised` (true), `doc_prefix` (`search_document: `), `query_prefix` (`search_query: `), `corpus` and `queries` maps id -> vector, and `document_inputs`/`query_inputs` with the exact prefixed strings that were embedded |
| `python-rankings.json` | per query: `expected`, `ids` (the Python `lore find` order, top 10), `scores`, `via` (the search channels that found each result), `found_at` |
| `baseline.json` | the DCB benchmark numbers (hit@1/hit@5/hit@3/hit@10/MRR per system and kind), the method, and the gap against `docs/RESULTS.md` |
| `build_fixture.py` | the script that produced the fixture (synthetic data only; rerunning it needs CodeLore's `src` on the path and Ollama) |

## How the Rust tests use it

1. Load `vectors.json`; every vector is L2-normalised, so similarity is the dot product.
2. For a query, embed the query with the same model and the `search_query: ` prefix
   (or compare against `query_inputs` to see the exact input), then rank the 50
   corpus vectors by cosine.
3. Compare the Rust ranking with `python-rankings.json` for the same query; the
   Python reference is `lore find -n 10` (hybrid: signals + words + meaning), so a
   pure-vector ranking may differ on close results. The reference hit@1 is 10/10.
4. `python-rankings.json` scores are CodeLore's fused scores, not cosine: use them
   as the expected order, not as a threshold.

## Provenance

- Model: Ollama `nomic-embed-text` (768 dimensions), reached at
  `http://127.0.0.1:11434`; vectors computed with CodeLore 0.3.0's own
  `codelore.embed.Ollama` and `codelore.index.embed_text`, so they are the Python
  side's vectors, not a reimplementation.
- The 50 texts and 10 queries were written for this fixture; they describe a
  fictional tool and share no text with DCB.
- Regenerate with `python build_fixture.py` after setting `LORE_CACHE` and putting
  a `lore` wrapper on `PATH`.
