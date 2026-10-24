# The CodeLore format, version 1

This is the contract between a repository's lore and any program that reads or
writes it. `lore` is one such program; nothing here depends on it.

## 1. Layout

```
lore.toml                    the project's configuration (§6)
lore/<area>.jsonl            knowledge: fault, reading, decision records
lore/plans/<plan-id>.jsonl   one plan: its plan record, then its items
```

`lore/` is the default and `lore.toml` can move it (`dir = "docs/lore"`). Areas
are the project's own words - `git`, `render`, `build` - declared in
`lore.toml`; a record's `area` names the file it lives in.

## 2. Encoding

- UTF-8, LF line ends, no BOM. A repository should hold
  `lore/** text eol=lf` in `.gitattributes`.
- **One record per line**, each line one JSON object, written canonically: keys
  sorted, no insignificant whitespace, non-ASCII kept as is
  (`json.dumps(r, sort_keys=True, separators=(",", ":"), ensure_ascii=False)`).
- **Lines are sorted by `id`.** Two people adding a record to the same area land
  on different lines, so a merge of two branches rarely conflicts. In a plan file
  the `plan` record comes first and the items follow **by number**, so
  `<plan>#10` comes after `<plan>#9`.
- No comments, no blank lines, a final newline.

A reader must refuse a file that breaks any of this, by name and line - it does
not repair.

## 3. Fields every record has

| field | type | rule |
|---|---|---|
| `v` | int | the format version; `1` |
| `id` | string | unique in the repository; `[a-z0-9][a-z0-9-]{1,47}`; an item's is `<plan-id>#<n>` |
| `kind` | string | `fault`, `reading`, `decision`, `plan`, `item` |
| `area` | string | one of `lore.toml`'s areas; for `plan` and `item` it is `plans` |
| `title` | string | 1-120 characters, says what the record is about |
| `status` | string | one of the kind's statuses (§4) |
| `created` | string | ISO date, `YYYY-MM-DD` |
| `updated` | string | ISO date |
| `by` | string | who wrote the last change: a person's name, or `agent:<model>` |
| `refs` | object | optional; §5 |

Unknown fields are kept and round-tripped, so a project or a newer program may
add its own; a reader must not drop them on rewrite. A field this document
defines may not change type.

## 4. The kinds

Lengths are ceilings, in characters. They are what keeps a record a record: the
story of how it was found belongs in the commit message, and `refs.commits`
points at it.

### fault - something that went wrong, and how to get past it

| field | | |
|---|---|---|
| `symptom` | required, <= 600 | what is observed, in the words of someone meeting it |
| `signals` | list of strings, each <= 200 | exact text it prints: an error message, an HRESULT, an exit code. Searched verbatim |
| `cause` | required, <= 600 | why it happens |
| `fix` | required, <= 800 | what to do |
| `check` | required, <= 300 | the command or test that sees it, or `none` with the reason |

`status`: `live` (can still happen), `cured` (the code no longer allows it; the
record stays, so a search for the old symptom finds the answer), `obsolete` (the
thing it is about is gone).

### reading - a measured number that a change is judged against

| field | | |
|---|---|---|
| `what` | required, <= 300 | what is measured, on what input |
| `command` | required, <= 300 | how to measure it again |
| `value` | required, string | the number with its unit, as the command prints it |
| `tolerance` | optional, string | what counts as unchanged |
| `measured_at` | required | the commit the value was read at |

`status`: `current`, `superseded` (a newer reading of the same `what` replaced
it; `superseded_by` names it).

### decision - a choice that should not be undone by accident

| field | | |
|---|---|---|
| `question` | required, <= 300 | |
| `choice` | required, <= 600 | |
| `reason` | required, <= 800 | |
| `decided_by` | required | who had the authority |

`status`: `active`, `reversed` (`reversed_by` names the decision that replaced it).

### plan - a goal and the items that reach it

| field | | |
|---|---|---|
| `goal` | required, <= 800 | what is true when the plan is done |
| `doc` | optional | path to a prose document with the reasoning, if the plan has one |
| `outcome` | required when `closed`, <= 2000 | what was built, what was dropped and why, what the plan got wrong |

`status`: `draft`, `live`, `paused`, `closed`. A plan is `closed` only when none
of its items is `open` or `deferred`, and a closed plan takes no new items.

### item - one step of a plan

| field | | |
|---|---|---|
| `plan` | required | the plan's `id` |
| `description` | optional, <= 2000 | |
| `criteria` | list of strings, each <= 300 | what must hold for it to be done |
| `deps` | list of `{"item": id, "kind": "hard" \| "soft"}` | hard: waits until that item is `done`; soft: may start once that item has started elsewhere |
| `commits` | required when `done` | the commits that did it |
| `reason` | required when `dropped` or `deferred`, <= 600 | why - "the current state is enough" is a reason |
| `decided_by` | required when `dropped` | who decided to drop it |
| `superseded_by` | required when `superseded` | the item that replaced it |

`status`: `open`, `done`, `dropped`, `superseded`, `deferred`. There is no
"in progress": who is working on what right now is runtime state and belongs to
whoever runs the work. `open` and `deferred` may move to any status; `done`,
`dropped` and `superseded` move only back to `open`, and only with a stated
reason (`lore item reopen`). A move removes the fields the old status carried.

Dependencies must not form a cycle, and must name items that exist.

## 5. `refs`

```json
{"commits": ["3f9a1c2e"], "files": ["src/render/post/chain.py"],
 "symbols": ["scene.depth_buffer"], "links": ["https://..."], "records": ["git-archive-pipe"]}
```

Every list is optional. `commits` are full or abbreviated hashes that exist in
the repository; `files` are paths relative to the repository root that exist at
`HEAD` when the record is written (a later check reports the ones that went
away); `records` names other records by `id`.

## 6. `lore.toml`

```toml
version = 1
dir = "lore"                       # where the JSONL lives
areas = ["git", "build", "render"] # knowledge areas; plans are always "plans"

[commit]
mode = "plain"     # "none" | "plain" | "shared-tree"
branch = "main"    # shared-tree only: the branch it moves
remote = "origin"
push = false       # push after each write
trailers = []      # lines added to every commit message

[embed]
provider = "ollama"                # "ollama" | "none"
url = "http://127.0.0.1:11434"
model = "nomic-embed-text"
timeout = 2.0                      # seconds to decide the server is not there
# doc_prefix / query_prefix: task prefixes; nomic models get theirs by default

[limits]           # optional; may only lower §4's ceilings
"fault.cause" = 400

[dedupe]           # optional; when a new record is refused as a duplicate
cosine = 0.95      # embedding similarity (near-identical text)
jaccard = 0.5      # word overlap of title, main sentence and cause
```

Unknown keys are refused, so a typo is not silently ignored.

## 7. Versioning

`v` is per record. A reader handles every version it knows and refuses a newer
one by name. A change that only adds an optional field keeps the version; any
other change raises it, and `lore migrate` rewrites the files.
