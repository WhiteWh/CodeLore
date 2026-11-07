# The CodeLore format, version 3

This is the contract between a repository's lore and any program that reads or
writes it. `lore` is one such program; nothing here depends on it.

## 1. Layout

```
lore.toml                    the project's configuration (§6)
lore/<area>.jsonl            knowledge: fault, reading, decision, topic, recipe records
lore/plans/<plan-id>.jsonl   one plan: its plan record, then its items
lore/sources.jsonl           source records: what became of each document moved into lore
```

`lore/` is the default and `lore.toml` can move it (`dir = "docs/lore"`). Areas
are the project's own words - `git`, `render`, `build` - declared in
`lore.toml`; a record's `area` names the file it lives in. `plans` and
`sources` are not areas.

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
| `v` | int | the format version the record was written at: `1`, `2` or `3` (topic and recipe need `2`; source, and a superseded decision, need `3`) |
| `id` | string | unique in the repository; `[a-z0-9][a-z0-9-]{1,47}`; an item's is `<plan-id>#<n>` |
| `kind` | string | `fault`, `reading`, `decision`, `topic`, `recipe`, `plan`, `item`, `source` |
| `area` | string | one of `lore.toml`'s areas; for `plan` and `item` it is `plans`, for `source` it is `sources` |
| `title` | string | 1-120 characters, says what the record is about |
| `status` | string | one of the kind's statuses (§4) |
| `created` | string | ISO date, `YYYY-MM-DD` |
| `updated` | string | ISO date |
| `by` | string | who wrote the last change: a person's name, or `agent:<model>` |
| `refs` | object | optional; §5 |
| `seen` | list of strings | optional, the five knowledge kinds only: the occurrences of the same fault or fact, `<date> <by> <where>`, at most 20 of at most 160 characters, oldest dropped (RustCodeLore's working tier, docs/lore.md 11.10) |

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
thing it is about is gone), `superseded` (the same fault was recorded twice and
folded into the other record, which `superseded_by` names).

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

`status`: `active`, `reversed` (the choice was undone; `reversed_by` names the decision
that replaced it), `superseded` (the same decision was recorded twice and folded
into the other record, which `superseded_by` names).

### topic - how something works now

| field | | |
|---|---|---|
| `body` | required, <= 3000 | what is true now: how the thing works, what it holds, what it is for |

`status`: `current`, `superseded` (`superseded_by` names the topic that replaced
it). A topic describes the present only - its history is in the commits and in
whatever archive the project keeps. A subject longer than one topic is several
topics, each with its own title, linked by `refs.records`.

### recipe - a procedure done more than once

| field | | |
|---|---|---|
| `when` | required, <= 300 | the situation it is for |
| `steps` | required, list of strings, each <= 400 | what to do, in order |
| `check` | required, <= 300 | how to see that it worked |

`status`: `current`, `superseded` (`superseded_by` names its replacement).

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
| `outcome` | optional when `done`, `dropped` or `superseded`, <= 1000 | what this item built or settled, and what it got wrong |

`status`: `open`, `done`, `dropped`, `superseded`, `deferred`. There is no
"in progress": who is working on what right now is runtime state and belongs to
whoever runs the work. `open` and `deferred` may move to any status; `done`,
`dropped` and `superseded` move only back to `open`, and only with a stated
reason (`lore item reopen`). A move removes the fields the old status carried.

Dependencies must not form a cycle, and must name items that exist. The commits
of an item still being built go in its `refs.commits`; `commits` is the closing
list of a `done` item. A plan's `outcome` is its summary; each item's own
`outcome` holds the verdict on that piece.

### source - what became of a document moved into lore

| field | | |
|---|---|---|
| `path` | required, <= 300 | the document's path from the repository root |
| `parts` | list of entries | the ledger, below |
| `records` | list of ids | the records that hold what still stands of it |
| `reason` | required when `history`, <= 600 | why nothing in it is current |

`status`: `open` (parts are still unaccounted for), `covered` (every part is),
`history` (the document holds nothing current; `records` names what still
stands). There is one source record for a path.

A Markdown document's **parts** are its headings (named by their text after the
`#`s), its table rows (`<heading> | <first cell>`), its numbered list items
(`<heading> / <n>. <first words>`) and, for a heading that also has parts under
it, what the heading says itself (`<heading> / text`). Headings in code fences
are not headings; a repeated name takes ` (2)`, ` (3)` in order of appearance;
names compare without regard to case or runs of whitespace.

Each entry of `parts` is `{"part": "<name>", "records": [<id>...], "skip":
"<why>"}` with `records`, `skip` (<= 300) or both. An entry for a heading
accounts for everything under it; a heading without one is accounted for when
everything under it is. A writer refuses an entry that names no part of the
document as it is, and `covered` while a part is unaccounted for.

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

[working]          # optional, RustCodeLore only: the working tier's expiry and bounds
pulse_days = 14    # 1-365; message_days = 7, unread_days = 30, urgent_bound = 10 (minutes, 1-120)
                   # bounds may be lowered, never raised: pulses_per_team 2000,
                   # messages_per_team 1000, unread_normal 50, unread_urgent 5,
                   # brief_versions 20, entries 20000, file_mib 64

[secrets]          # optional, RustCodeLore only
allow = []         # exact strings the secret scan lets pass (each a literal, never a pattern)
```

Unknown keys are refused, so a typo is not silently ignored.

## 7. Versioning

`v` is per record. A reader handles every version it knows and refuses a newer
one by name. A change that only adds an optional field keeps the version; any
other change raises it. A record keeps the version it was written at until it is
next written, when it takes the current one: version 2 added the `topic` and
`recipe` kinds and changed nothing else, so a version 1 file is a valid version
2 file. Version 3 added the `source` kind, the decision's `superseded` status and
the item's optional `outcome`, and made `superseded_by` and `reversed_by` record
ids rather than strings of up to 64 characters; every version 2 file is a valid
version 3 file. The optional `seen` list (§3) only adds an optional field, so it
keeps version 3, and a reader that does not know it keeps it on rewrite.
