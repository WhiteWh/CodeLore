# RogueDOD / UE baseline bench

Control queries and the SocratiCode-vs-Atlas baseline for the code-map work
(task #1). The query set is frozen in `roguedod_queries.json`; do not edit it
to match a later Atlas result - add queries in a new file and keep this one as
the before-picture.

## Files

| file | what it is |
|---|---|
| `roguedod_queries.json` | 13 queries: exact identifiers, natural-language, VaCuus, UE, static-call cases; each with expected path/line/symbol, answer keys and call edges where applicable. One query (`rd-vacuus-plugin-gap`) is a coverage probe, excluded from totals. |
| `coverage.json` | what the two SocratiCode indexes and the RogueDOD Atlas index actually contain, with the commands that measured it. |
| `socraticode_mcp.mjs` | small MCP stdio client used to reach SocratiCode's `codebase_search` without a host. |
| `run_baseline.mjs` | runs every query `BENCH_REPS` times on both systems, scores top-10, writes `docs/bench/roguedod-baseline.json` and `.md`. |

## Run

Use a private copy of the Atlas cache for the checkout being measured.
`LORE_CACHE` is the parent directory containing the checkout's index directory.
The SocratiCode indexes for `RogueDOD/` and `Engine/Source` must already exist.

```powershell
$env:ROGUEDOD_ROOT = "<path to the RogueDOD checkout>"
$env:UE_SOURCE_ROOT = "<path to Engine/Source>"
$env:SOCRATICODE_ENTRY = "<path to the SocratiCode dist/index.js file>"
$env:LORE_CACHE = "<path to the private Atlas cache copy>"
$env:LORE_BIN = "target\release\lore.exe"
node tools/bench/run_baseline.mjs --artifact "<path to a local results.json file>"
```

`LORE_BIN` overrides the binary (default `target/debug/lore`, with `.exe`
on Windows). Select a release binary for performance measurements.
`BENCH_REPS` changes the repetitions (default 3).

## What the numbers mean

- **answer** - a result whose text holds one of the query's keys, exact and
  case-sensitive, as in `docs/BENCHMARK.md`; file-location queries answer on
  the expected path.
- **expected** - the first result on the expected path (and inside its line
  range), alternates counted. A header declaration is not the definition: the
  expected line is the definition's line.
- **latency** - wall clock of the call the agent makes, best of `BENCH_REPS`.
  SocratiCode's MCP server is warm; each `lore` call is a fresh process.

## Known limits

- The RogueDOD Atlas cache was built before this task and contains no vectors
  (`vectors: 0`); the measurement is text-only on purpose.
- This baseline predates shared external sources in Atlas. Its UE queries
  were measured using SocratiCode.
- `Plugins/VaCuus/**` (893 C++ files) is outside both indexed corpora. The
  `rd-vacuus-plugin-gap` probe records that, and `coverage.json` says why.
- The RogueDOD SocratiCode index was built by 1.16.2; the local package used
  for queries is 1.16.1. The UE index was built 2026-09-02, before some engine
  files were last written.
