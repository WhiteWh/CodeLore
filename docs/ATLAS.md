# Atlas - the code map beside the lore

> Status: experimental, in `main` since 0.4.1. Built only with the
> `atlas` cargo feature, and on for a project only when its `lore.toml` says so.

CodeLore keeps what was **learned** about a project: faults and their fixes,
readings, decisions, plans. It deliberately does not index the source ("it
indexes what was learned, not the code" - README). An agent still needs the
other half every session: *where* things are in the code now, what a change
reaches, which files import which. Today that half comes from a separate tool -
SocratiCode, an MCP server with Qdrant, Ollama and Docker - or from grep.

Atlas is that half, rebuilt inside `lore`, and joined to the records.

## 1. What changes against SocratiCode

SocratiCode's ideas are good and kept: hybrid search (BM25 + vectors, fused by
reciprocal rank), pieces cut at definitions instead of fixed line counts, a
file graph from imports with impact and cycles, incremental updates by content
hash. What is revised is everything around them.

| | SocratiCode | Atlas |
|---|---|---|
| runtime | Node MCP server + Qdrant + Ollama, usually Docker | the `lore` binary; nothing else runs |
| vectors | Qdrant collections | the same SQLite file as the text index, in the user cache |
| embeddings | Ollama / OpenAI / Google / LM Studio | the built-in model already in `lore` (nomic-embed-text-v1.5, e5, bge-m3 on candle; CPU or CUDA) |
| freshness | a file watcher, a daemon, locks between MCP processes | git's blob ids compared on every command; no watcher, no daemon, no lock |
| parsing | ast-grep grammars per language | line patterns per language: no grammar to ship; a missed definition costs a chunk boundary, never the file |
| interface | MCP tools | CLI with `--json` (an MCP wrapper is a thin layer over it, if wanted) |
| knowledge | "context artifacts" (schemas, API specs) | the lore itself: records with `refs.files` / `refs.symbols` |
| licence | AGPL-3.0 | MIT, written from scratch: no SocratiCode code is copied |

The last two rows are the point of putting it in CodeLore. A code index alone
says *where*; the lore says *what we know about it*. Joined:

- `lore atlas impact <file|symbol>` - every file a change reaches, **and every
  record the lore holds about any of them**: the fault someone met in a caller
  three steps away shows up before the change, not after.
- `lore atlas drift` - records citing files or symbols the code no longer has.
  Knowledge that rots silently is the problem CodeLore exists for; the map can
  see it rot.
- `lore for <file>` - besides the records about the file, who imports it and
  what the lore says about those files.
- `lore find` (with `atlas.find = true`) - the best code places under the
  records.

## 2. Switches

| switch | where | effect |
|---|---|---|
| `atlas` cargo feature | build | compiles the map in. **On by default** since 0.4.1, so every `lore` - this repository's or one a package builds from this crate (Zerus pins it) - has `lore atlas`; it stays off at runtime until the switch below. `--no-default-features` leaves it out |
| `[atlas] enabled = true` | lore.toml | turns the map on for the project (default `false`) |
| `LORE_ATLAS=on\|off` | environment | overrides `enabled` for one user or one harness |
| `[atlas] semantic` | lore.toml | embed pieces (default `true`); `false` keeps the map text only |
| `[atlas] for` | lore.toml | `lore for` adds the importers (default `true`) |
| `[atlas] find` | lore.toml | `lore find` adds code places (default `false`) |
| `[atlas] include` | lore.toml | extra extensions indexed as text, e.g. `[".md", ".tpl"]` |
| `[atlas] exclude` | lore.toml | path prefixes or globs (`third_party`, `**/*.generated.cs`) beside `.gitignore` |
| `[atlas] max_kib` | lore.toml | larger files are skipped (default 512) |

A build without the feature reads the `[atlas]` table the same way (it is part
of the configuration, not of the feature) and answers `lore atlas` with how to
build it.

**Older binaries.** A lore.toml top-level table this version does not know is
now a warning on stderr, not a refusal, so a newer project file never takes an
older `lore` - and the whole project memory with it - down. Keys inside a known
table stay strict.

## 3. Commands

```bash
lore atlas status                       # what the map holds, where, whether meaning is on
lore atlas index [--rebuild] [--text-only]
lore atlas search "where the index decides a file changed" [-n 5] [--lang rust] [--path src/]
lore atlas search - < traceback.txt     # the error lines of a pasted log
lore atlas symbol open_index            # definition, files naming it, records about it
lore atlas symbols src/index.rs
lore atlas deps src/index.rs            # imports, imported by, outside the repository
lore atlas impact src/store.rs --depth 3
lore atlas cycles
lore atlas drift
lore atlas guide                        # the agent's part; `lore guide` prints it when the map is on
```

Every command takes the global `-C <dir>`, `--json` and `--text-only`.

## 4. How it works

**Files.** `git ls-files -s` gives every tracked file with its blob id;
`ls-files -m` and `-o --exclude-standard` the changed and untracked ones, hashed
the way git would. A file is in the map when its extension names a language
(Rust, Python, JS/TS, Go, Java, Kotlin, C#, C/C++, shaders - HLSL, GLSL, USF,
WGSL, Metal -, Lua, Ruby, PHP, Swift, shell, PowerShell, a few build files) or
`atlas.include`; the lore's own folder, vendored and build folders are left out.
On every command the ids are compared with the map's; only a file whose id moved
is read again. No watcher: there is nothing to keep running.

**Pieces.** Definitions are found by line patterns per language (a C function
counts only when a `{` follows before a `;`). A file is cut at its outermost
definitions, each piece starting above its doc comment and attributes; pieces
under 12 lines join the next, pieces over 120 are cut again at the definitions
inside (an `impl`, a class) or in windows. A file without definitions is cut in
60-line windows.

**Text search.** Two FTS5 tables like the lore's: `code_words` (path, symbol
names *and their split words* - `getUserName` is also `get user name` - and the
body, Porter-stemmed) and `code_terms` (whole code tokens: `a::b`, `x.y`,
`ERR_X`, `0x8007...`). A third list ranks the pieces that **define** an
identifier the query names (`open_index`, `ChunkStore`, `a::b`; plain English
words are not taken for names).

**Meaning.** Each piece's path, symbols and first 1500 characters are embedded
with the configured built-in model, cached by model and text hash beside the
pieces. `lore atlas index` embeds everything missing; a search embeds up to 48
new pieces itself and otherwise says how many wait for the index.

**Fusion.** Reciprocal rank fusion, `k = 60`, the definition list weighted
double; at most two pieces of one file.

**Graph.** Imports are read per language and resolved once every file is known:
Rust `mod x;` and `use crate|self|super::...` (to the module file, or the crate
root for its items), Python absolute and relative imports, relative JS/TS
imports with their extensions and `index` files, C/C++/shader includes (relative
first, then by path suffix when unambiguous), Go packages by folder, Java/Kotlin
by path, C# `using` by declared namespace, Lua `require`, Ruby
`require_relative`, PHP includes. An import that names no file of the
repository is external and makes no edge. A Rust `mod x;` is a *containment*
edge: it counts for impact, not for cycles.

## 5. The model is fetched, not shipped

The weights (0.5-2.2 GB) cannot be a cargo dependency: crates are limited in
size, a build may be offline, and a binary that grows by a gigabyte serves no
one. Instead the `lore` binary fetches the configured model itself the first
time it is needed, pinned by size and SHA-256 as `lore model fetch` does:

- `lore init` starts the download in the background;
- `lore index` and `lore atlas index` fetch it in the foreground (that is what
  was asked for);
- `lore find` and `lore atlas search` answer on text at once and start the
  download in the background, once (a marker file next to the model keeps a
  second search from starting another);
- `LORE_MODEL_FETCH=off` stops all of it; a library caller (tests, a harness
  calling `cli::run`) never downloads anything.

## 6. Measured

On CodeLore itself (50 files, 667 pieces, 1284 symbols, 150 file links), Windows,
CPU, on a machine busy with other builds:

- text map from nothing: 0.9 s; a command when nothing changed: the git scan only;
- embedding 667 pieces with nomic-embed-text-v1.5 on the CPU: 505 s. That is the
  cost the `cuda` feature exists for; the text map is useful without it;
- a search with meaning: 1.4-3 s, most of it loading the model.

## 7. Not yet

- No call graph: impact is at file level (who imports the file), not "who calls
  this function". The definition and mention lists are the step towards it.
- No cross-repository search (SocratiCode's linked projects).
- Pattern-based definitions miss unusual shapes (macros generating functions,
  C++ with heavy templates); such code is still searchable as text.
- No MCP server; `--json` is the interface a wrapper would use.
