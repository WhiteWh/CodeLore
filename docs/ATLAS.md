# Atlas - the code map beside the lore

> Status: experimental, in `main` since 0.4.1. Built only with the
> `atlas` cargo feature. The CLI uses the project's Atlas switch. The local MCP
> server can open a confirmed Git checkout directly.

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
| runtime | Node MCP server + Qdrant + Ollama, usually Docker | one `lore` binary for the CLI, MCP, watcher and local model server |
| vectors | Qdrant collections | the same SQLite file as the text index, in the user cache; each piece's vector also in one cache shared by every clone, by model and piece hash, so a second clone of a tree embeds almost nothing |
| embeddings | Ollama / OpenAI / Google / LM Studio | the built-in model already in `lore` (nomic-embed-text-v1.5, e5, bge-m3 on candle; CPU or CUDA) |
| freshness | a file watcher, a daemon, locks between MCP processes | Git blob ids, with synchronous CLI refresh or background refresh and published SQLite snapshots |
| parsing | ast-grep grammars per language | line patterns per language: no grammar to ship; a missed definition costs a chunk boundary, never the file |
| interface | MCP tools | stdio MCP tools and CLI with `--json` |
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
  what the lore says about those files. It reads the map as it is: it never
  builds one from nothing (a fresh clone's first build takes minutes; `lore
  index` does it) and never waits while another process writes it.
- `lore find` (with `atlas.find = true`) - the best code places under the
  records.

## 2. Switches

| switch | where | effect |
|---|---|---|
| `atlas` cargo feature | build | compiles the map in. **On by default** since 0.4.1, so every `lore` - this repository's or one a package builds from this crate (Zerus pins it) - has `lore atlas`; it stays off at runtime until the switch below. `--no-default-features` leaves it out |
| `[atlas] enabled = true` | lore.toml | turns the map on for the project (default `false`) |
| `LORE_ATLAS=on\|off` | environment | overrides `enabled` for one user or one harness |
| `LORE_ATLAS_SNAPSHOT=on` | environment | reads the last project snapshot without refreshing, including the code sections of `lore for` and `lore find` |
| `[atlas] semantic` | lore.toml | embed pieces (default `true`); `false` keeps the map text only |
| `[atlas] for` | lore.toml | `lore for` adds the importers (default `true`) |
| `[atlas] find` | lore.toml | `lore find` adds code places (default `false`) |
| `[atlas] include` | lore.toml | extra extensions indexed as text, e.g. `[".md", ".tpl"]` |
| `[atlas] exclude` | lore.toml | path prefixes or globs (`third_party`, `**/*.generated.cs`) beside `.gitignore` |
| `[atlas] exclude_file` | lore.toml | a file (or a list of files) of patterns in gitignore's syntax, from the repository root - `!` re-includes, a trailing `/` is a folder, a leading `/` anchors to the file's folder; e.g. `exclude_file = "Game/.socraticodeignore"`. An edit to the file re-scans the map. A `lore` older than this key refuses the `[atlas]` table that has it |
| `[atlas] max_kib` | lore.toml | larger files are skipped (default 512) |
| `[atlas.unreal] root` | lore.toml | optional absolute path to one Unreal Engine installation, the directory containing `Engine/` |

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

### Connect an MCP host

Register one stdio server for Atlas in the host's user configuration. The
same command works across checkouts. Use a local absolute path in the user
configuration if the host cannot find `lore` on its PATH.

```json
{
  "mcpServers": {
    "atlas": { "command": "lore", "args": ["mcp", "serve"] }
  }
}
```

The server uses the workspace folder exposed by the host's MCP roots. The
folder must be inside a Git checkout. An advertised subfolder gets its own
snapshot and watcher, with result paths relative to that folder. Git selection
and Atlas exclusion rules still use checkout-relative paths.
With no roots available it looks for the checkout containing the server's
working directory. If the host supplies several folders, select one workspace
in the host or pass the optional `projectPath` argument. A server command can pin
the checkout with `lore -C <checkout> mcp serve`. The tool's `projectPath` takes
precedence over these defaults. [MCP roots specification](https://modelcontextprotocol.io/specification/2025-11-25/client/roots)

`code_search` accepts `query`, `limit`, `lang`, a literal `path` prefix,
`scope` and `text_only`. `code_symbol` takes `name` and supports the language
and path filters. `code_symbols` takes `file`. `code_status` shows whether a
snapshot is empty, indexing or ready, with per-source counts when available.
Searches and symbol lookups read published snapshots. The tools return Atlas
locations, source labels and snapshot metadata.

The first lookup starts the project's background text index if necessary and
returns its state. Poll `code_status` in the same connection until the snapshot
is ready. Atlas reads the existing project's filters when `lore.toml` is present
and uses defaults in a checkout without it. The worker shares the CLI cache
and continues refreshing after the MCP connection closes. Use
`lore -C <workspace> atlas watch stop` to stop it. A subsequent MCP lookup can start
it again. Set `LORE_ATLAS=off` in the server environment to disable Atlas.

### External sources from build evidence

MCP search includes ready dependency snapshots by default. Set
`scope: "dependencies"` for external evidence alone or `scope: "project"` for
the project map. An explicitly configured Unreal source still has its
`unreal` scope.

An independent background worker reads `*.dep.json` MSVC
`/sourceDependencies` reports from
the workspace and probes conventional `build`, `out` and `Intermediate/Build`
outputs. It probes those output paths one directory below a workspace as
well. A scoped workspace can use its containing checkout's build output.
Only reports whose `Data.Source` belongs to the active workspace contribute
observations. Toolchain and SDK paths have their own filter. The watcher reads
the exact recorded external C/C++ files into a reusable SQLite index.

For CMake-generated Ninja builds, Atlas matches `compile_commands.json`
source/output pairs to compiler edges in the adjacent `build.ninja`. Link
edges connect the workspace's compiled objects to external target objects.
The manifest's CMake reconfiguration inputs identify the owning source
directories. An adjacent `.ninja_deps` file provides exact header observations
for objects with matching timestamps. The indexed set is the reachable
translation units and those headers. [Ninja build statements](https://ninja-build.org/manual.html#_build_statements)

Build systems can also publish an explicit source root in `atlas-build-sources.json`.
For example, a file written alongside a compilation database may declare the
consumer context and the dependency's source files.

```json
{
  "version": 1,
  "contexts": [
    {
      "id": "debug-x64",
      "translation_units": ["../src/main.cpp"],
      "sources": [
        {
          "root": "<absolute path to the custom engine>",
          "version": "1.4",
          "files": ["include/Runtime.hpp", "src/Runtime.cpp"]
        }
      ]
    }
  ]
}
```

`translation_units` and a relative `root` resolve from the manifest folder.
A relative `files` entry resolves from its declared root. A compilation
database beside the manifest with a matching consumer TU can contribute
additional individual files inside that root. A context can instead name a
discovered database with `compilation_database`, relative to the manifest.
Include flags provide `search_paths` for status.
The indexed set consists of confirmed files, including for a declared root.

`code_status` reports each source's `scope`, `source_root`, `provenance`,
`implementation`, `freshness`, `missing_files`, `index_key` and snapshot
revision. Observed header sets report `implementation: "unconfirmed"`.
Dependency paths and the path filter are relative to the source's published
`root`. A project stays searchable during dependency discovery or indexing.
Matching physical file sets with the same scope and version share a writer
and a database across workspaces. Refreshes use artifact data and filesystem
reads. Compiler arguments are parsed as data. The dependency worker can
start alongside a project watcher from an older Atlas build. `atlas watch
stop` stops both processes for that workspace.

### Configured external source trees

Add one table per source to `lore.toml` for paths shared by all checkouts,
or to an optional, Git-ignored `lore.local.toml` beside it for machine-local
installations. The local file accepts only `[atlas.sources.<name>]` tables.
A local source replaces the whole shared profile with the same name. The
CLI, MCP and watcher all use these settings. Without the local file, the
workspace still uses the shared configuration.

A source name uses lowercase ASCII letters, digits, hyphens and underscores,
starting with a letter or digit. The names `project`, `unreal`, `all`,
`dependency`, `dependencies` and `external` are reserved.

```toml
[atlas.sources.runtime]
root = "../custom-runtime"
exclude = ["private", "**/*.generated.cpp"]
max_kib = 512

[atlas.sources.tools]
root = "../build-tools"
include = [".template"]
```

A relative root is resolved from the directory containing `lore.toml`.
The canonical directory bounds the tree. It may be a Git checkout or an
ordinary directory. Atlas indexes recognized source and text formats,
plus extensions named in `include`, and uses `exclude` with the same
root-relative prefix and glob rules as the project map. It skips the standard
generated and vendor directories, the index cache, symlinks and junctions
inside the tree. The selected root itself can be an alias to a canonical
directory. A filesystem root or an ancestor of the active workspace is
reported as unavailable. The default file size limit is 512 KiB.

MCP discovers the active workspace as usual. Its first search activates the
background worker. With the CLI, run `lore atlas watch start` to begin
refreshing configured sources, then use `lore atlas status`,
`lore atlas search`, `lore atlas symbol` or `lore atlas symbols` with the option
`--source runtime`. In MCP tools pass `source: "runtime"` instead of `scope` to
select one name. The dependency scope includes both named roots and sources
found in build data. Ordinary combined results deduplicate physical source
locations. Named hits include `source_name` and an `absolute_path`.

Status labels these sources `external` with a name, configured and canonical
paths, the selection profile and the originating config field. The scope is
`configured_tree`. The observed and indexed file counts reflect filtering
and readable content. The canonical root and effective selection profile
determine the shared SQLite key. Workspace names and aliases do not affect
the key. A content change increments the revision inside the same database.
Unavailable roots have a diagnostic, while the other sources stay available.
One worker watches each shared key. Filesystem changes trigger a tree pass.
New watcher ownership and a five-minute reserve check cover missed events
and unavailable native watchers. Other workspaces can keep reading the
published snapshot during either check.

After a named source has a ready text snapshot, run
`lore atlas index --source runtime` to fill its missing vectors for the
project's selected model. A DirectML build uses the local GPU for this batch
operation and falls back to CPU when DirectML fails. Set `LORE_DEVICE=cpu`
to use the CPU query path, which reuses the local model server when enabled.
The index command reports the encoder used when it generates vectors,
along with any DirectML fallback diagnostic. If the model is missing, run
`lore model fetch`. The watcher and first search only build the text map.
The same root and profile share vectors when the model space also matches.
While the command runs, CLI and MCP searches can read the previous snapshot.
If the writer is busy, retry after the current update. If an indexed file is
edited or removed during generation, the command discards the fill and the
watcher updates the text before a retry. Interrupting the fill also leaves the
previous snapshot available.

`lore atlas status --source runtime --json` and MCP `code_status` report the
selected `model` and `vector_space`, the number of covered pieces in `vectors`,
and `vectors_missing`. The `model_installed` field reports local weights.
Use `--text-only` for a search without the model or for a read-only index
check. Text refresh stays with the watcher.

### Ready vector search

CLI search and MCP `code_search` use one embedding per query across the
selected sources. Each source supplies its own semantic candidates to the
existing keyword and exact-symbol ranking. A ready ANN graph supplies up
to 160 candidates. Atlas checks each candidate's chunk ID, embedding hash,
model, path and language in the selected SQLite snapshot before rescoring
the original F32 vector. Up to 80 candidates enter rank fusion.

A source with at most 4096 chunks can use exact similarity. For a larger
source, an absent, stale or damaged ANN graph leaves text and symbol
results available. Search reports its `semantic_search` mode and state
per source. An incompatible vector space supplies no semantic results.
Filtered ANN uses SQLite predicates on the current snapshot and caps the
number of filter probes. `filter_budget_reached` marks a partial filtered
semantic candidate set. Text-only requests bypass the graph and encoder.
For a selective path prefix, Atlas selects up to 65,536 eligible chunk IDs
in temporary SQLite storage and shares that selection between FTS and ANN.
For broader filters or older FTS row IDs, it uses the SQLite join. Both
paths preserve the original BM25 scores and row-ID tie ordering.
Set `LORE_ATLAS_SEARCH_TIMINGS=1` for JSON timings of query encoding, graph
open, filter preparation, FTS, ANN, SQLite filtering and SQLite reranking.

The read-only sidecar lives beside `atlas.sqlite` under
`ann-v1/<SHA-256 of vector space>/`. Its `current.json` selects immutable
`<generation>.usearch` and `<generation>.keys` files, where generation is
64 lowercase hex characters. The ID-sorted key file uses 48 bytes per row:
an unsigned little-endian 64-bit chunk ID followed by its 40 ASCII hex
embedding hash. Version 1 uses USearch F16/IP, connectivity 24 and
construction expansion 112. The reader uses search expansion 1536.
The manifest binds the source key, compatible vector space, SQLite vector
model, dimension, text revision, refresh timestamp, vector coverage and
`meta.ann_vector_generation` when present. The reader verifies those
values and file stamps before opening a graph. Keep published generation
files immutable. Search only reads a prepared generation.

The project and source watchers schedule ANN construction for snapshots
above the exact-search limit when compatible vectors already exist.
An explicit vector fill can schedule the same job. Construction runs in
a separate local process with up to four insertion threads. It reads the
committed SQLite vectors in batches and leaves embedding generation to
the existing indexing command. A build lease belongs to the shared
database and model space. Workspaces tied to that database reuse the
publication.

SQLite tracks vector writes with a per-model counter, including replacements
that keep the same coverage and text revision. The sidecar binding combines
that counter and the database clock with the live text snapshot. A builder
checks the binding while building and under a short SQLite writer
transaction before atomically replacing `current.json`. An interrupted
build leaves the previous pointer available, and the watcher retries.
Newer publications retain the previous generation and remove abandoned
generation files when the OS releases them.

`lore atlas status` and MCP `code_status` expose `ann.mode`, `ann.state`,
and the published generation or the worker error where applicable. While
an updated graph builds, a large source can still return text and symbol
matches from its latest SQLite snapshot.

### Background project refresh

```sh
lore atlas watch start
lore atlas watch status --json
lore atlas watch stop
lore atlas search "UInventoryWeapon::Equip" --snapshot --text-only
```

`watch start` enables background refresh for the current checkout and cache.
The worker builds the first text index and refreshes it after batches of file
changes. It also reconciles with Git every 30 seconds to recover changes
outside the watcher, including Git ignore rules stored outside the checkout.
Use the same `LORE_CACHE` for the worker and its queries.

Project queries use the latest completed snapshot while background refresh is
enabled. Text output shows the snapshot and worker states. JSON adds
`snapshot` with the publication time (`refreshed_at`), the most recent successful
check (`checked_at`), the revision and worker status. Commands whose JSON
normally returns an array put that array under `results` in snapshot mode.
A query issued during the first build reports that the snapshot is unindexed.
If the worker exits, queries continue using the last snapshot and report that
the worker is unavailable. Run `watch start` again to resume it. `watch stop`
asks the worker to stop, keeps the cache available and restores synchronous
refresh for ordinary queries.

`--snapshot` on a project read or `LORE_ATLAS_SNAPSHOT=on` uses an existing cache
without requiring a worker. `lore atlas index` remains an explicit refresh.
It shares the project writer lock with the worker and can fill the semantic
vector cache. While an index refresh is in progress, snapshot queries use
the previously published files, symbols and graph. They can use compatible
vectors already cached.

### Share an Unreal Engine index

In a Git project's `lore.toml`, add the Unreal section to its Atlas settings.
The installation can be a Launcher build with shipped sources or a source
checkout. Atlas uses the configured path as-is and reads
`Engine/Build/Build.version` to identify the build.

```toml
[atlas]
enabled = true

[atlas.unreal]
root = "<absolute path to the Unreal Engine installation>"
```

```powershell
lore atlas index --source unreal --text-only
lore atlas status --json
lore atlas search CoreMinimal.h --source unreal --text-only
lore atlas search "component tick registration" -n 10
lore atlas symbol "UActorComponent::SetComponentTickEnabled" --lang cpp
lore atlas symbols "Engine/Source/Runtime/Engine/Private/Components/ActorComponent.cpp" --source unreal
```

`index`, `status`, `search`, `symbol` and `symbols` accept `--source project`,
`--source unreal` or `--source all`. The default is both when Unreal is configured.
`index --rebuild` without a selector rebuilds the project. Use
`index --source unreal --rebuild` to reread every engine file, including edits
that preserve the size and modification time. Use `--source all` to rebuild both.
`search` and `symbol` also accept `--path` (a literal, source-relative prefix)
and `--lang`. For `symbols`, an absolute path identifies the source. With a
relative path, definitions from both sources can appear at the same path.

Set the same root in another project to reuse the engine database immediately.
It lives under `atlas-unreal` in the user cache (or `LORE_CACHE`). The key covers
the canonical installation root, the build and the index profile. After an
engine update, run `lore atlas index --source unreal` to build the new cache.
Run the same command after manual source edits or installing more source files.
Queries use the last completed index for the selected build. A missing engine or
cache is reported in joint results, and the project stays searchable.

The first read of an older engine cache fills the short-name keys and snapshot
counts from SQLite. It preserves the engine revision, indexed source and
embeddings. A busy or read-only legacy cache remains readable with its original
lookup. Ready caches keep file, piece, symbol, import and vector coverage counts
in the same transaction as their index data.

The engine profile includes C/C++, `*.Build.cs` and `*.Target.cs` from
`Engine/Source`, plugin source directories under `Engine/Plugins`, and the
corresponding source and plugin directories under `Engine/Platforms`. It skips
third-party, generated and build-output directories/files, links and
junctions, and files larger than 512 KiB. Project Atlas filters apply to the
project. Engine result paths start with `Engine/`; JSON locations also include
`source`, `source_id`, `root`, `absolute_path` and the engine `build`. Joint
`symbols --json` returns an object with `definitions` and `sources`.

Drop `--text-only` from the index command to populate compatible vectors using
the built-in model. Search can use whatever compatible vectors are available
in each source, with text results available throughout. This index covers
search and definition locations. The dependency and impact commands use the
project map.

### Navigate C++ calls

Use a compilation database for one target and an installed libclang that
matches your host architecture. For Unreal, use a database for the selected UBT
target and keep its generated headers available. Each entry needs the file,
working directory and compiler arguments. Prefer the `arguments` array,
especially for paths with spaces. The analysis accepts normal include, define
and language flags. Use a separate analysis database for options that require
the original build driver or an MSVC PCH. Atlas reads the compile options and
parses source without running the compiler command.

```powershell
$env:LORE_LIBCLANG = '<absolute path to libclang.dll>'
lore atlas calls-index --compdb compile_commands.json --profile Editor
lore atlas calls 'api::open' --direction out --profile Editor
lore atlas calls 'api::read(int)' --direction in --profile Editor --json
lore atlas calls 'src/reader.cpp:42' --depth 2 --profile Editor
lore atlas calls-status --profile Editor --json
```

`calls-index` also accepts `--libclang PATH`, `--path PREFIX` to update a
source-relative subset and `--rebuild` to parse all selected files again. An
unchanged, successfully parsed translation unit is reused. Changes to its
source, known included headers, response files or compiler context trigger a
new parse. Rebuild after changing how include lookup works, for example after
adding a header that shadows an existing one. The graph checks cached inputs
before presenting confirmed locations and can be queried without loading libclang.

With an attached Unreal source, build an independent Engine database with
`calls-index --source unreal --compdb PATH --profile Editor`. Its compilation
directories and source dependencies must belong to the installation or the
recognized compiler/SDK directories. Use the project index for Engine units
that depend on a project. Projects sharing an installation and profile can
read the same Engine call index. Select matching build contexts for the
project and Engine database. Project calls into Engine can resolve to an Engine
declaration while the corresponding definition is unavailable.

The JSON reports exact and ambiguous start selectors, call sites, declarations,
and available definitions with source, path and location. Use an exact
signature or the displayed `cpp:…` id to disambiguate. `--candidates` includes
unresolved, stale, ambiguous and diagnostic sites. For virtual calls Atlas
can show the statically selected method. Traversal beyond the first hop
follows confirmed direct calls. The index covers named C++ calls visible to the
selected compiler context. It records call sites in the files selected by their
compile commands and links to referenced inline definitions in headers.
Delegate bindings, reflection, Blueprint targets
and indirect dispatch require runtime information. Search, symbol lookup and
the file dependency and impact commands use their existing indexes.

Every command takes the global `-C <dir>`, `--json` and `--text-only`.

### Call graphs in MCP

Prepare a graph explicitly with `lore atlas calls-index --compdb <path>
--profile <name>`. In the same workspace call `code_calls_status` with
that exact `profile`. Call `code_calls` with the profile and a `target`
such as a function name, exact signature, `cpp:` ID or `file:line`.
`direction` is `out` by default and accepts `in` for callers.
`depth` accepts 1–32, and `limit` caps the result at 1–100 edges.
Use `candidates: true` to include diagnostic, stale, unresolved and
ambiguous edges. Results retain the CLI's absolute file, line, column,
signature, dispatch and resolution. Select a signature or ID when several
functions match.

Both tools read existing SQLite graphs and require an explicit profile.
Status checks fingerprints across the selected graph. Navigation checks
the units it visits. The response reports missing, partial or stale
coverage and includes libclang diagnostics recorded during indexing.
The tools load neither libclang nor the background indexing worker.

For a project and one configured `[atlas.sources.runtime]` root, provide
one compilation database with the selected units from both roots and run
`lore atlas calls-index --source runtime --profile <name> --compdb <path>`.
Use the same `--source runtime` for CLI reads, or pass `source: "runtime"`
with `profile` in MCP. This selects one combined graph. Locations name the
actual project or external source. The compilation database explicitly
selects translation units within those two roots.

The named graph is isolated by workspace, exact profile, source name and
canonical external root with its configured selection profile. Its
compilation fingerprint includes the database path and contents, libclang
identity and include environment. Each unit also fingerprints its command,
response files and parsed inputs. Use a different profile for a different
build configuration, or reindex the profile as a whole when its compilation
changes. A partial `--path` refresh is accepted for the same compilation.
Existing project and `atlas.unreal` profiles keep their original keys.

## 4. How it works

**Files.** `git ls-files -s` gives every tracked file with its blob id;
`ls-files -m` and `-o --exclude-standard` the changed and untracked ones, hashed
the way git would. A file is in the map when its extension names a language
(Rust, Python, JS/TS, Go, Java, Kotlin, C#, C/C++, shaders - HLSL, GLSL, USF,
WGSL, Metal -, Lua, Ruby, PHP, Swift, shell, PowerShell, a few build files) or
`atlas.include`; the lore's own folder, vendored and build folders are left out.
By default a command compares the ids with those in the map. With background
refresh enabled, the worker does that comparison. Either mode reuses the
entries whose ids still match.

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
pieces. `lore atlas index` embeds everything missing. A synchronous search can
embed up to 48 new pieces. A snapshot search uses the vectors already cached.
Hybrid Atlas queries reuse the local model server used by `lore find` for the
same model and build. A combined project and Unreal query sends one text to
the model and compares the vector within each compatible index. Set
`LORE_EMBED_SERVE=off` to encode in the calling process. `--text-only` skips
the model.

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
