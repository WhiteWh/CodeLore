# CodeLore

**Project memory for coding agents, kept in your repo.** Faults met and their
fixes, measured readings, decisions, and plan items that never disappear
silently. JSONL in git, hybrid full-text + semantic search, one CLI - `lore` -
that checks and commits every entry.

> Status: **0.1.2.** Every command below works and is tested;
> the search's ranking and the duplicate threshold are still to be measured on a
> large real lore ([docs/ROADMAP.md](docs/ROADMAP.md)). The format is in
> [docs/FORMAT.md](docs/FORMAT.md), the design in [docs/DESIGN.md](docs/DESIGN.md).

## Install

```bash
git clone https://github.com/WhiteWh/CodeLore.git
python -m pip install --user -e CodeLore
python -m codelore --version        # or `lore`, once pip's scripts folder is on PATH
```

Then, at a repository's root:

```bash
lore init --areas build,render,deploy        # writes lore.toml and a .gitattributes line
git add lore.toml .gitattributes && git commit -m "lore init"
lore guide                                   # what an agent needs to know
```

## Measured

On its first production user, a private repository worked by several agent
sessions at once: 41 faults asked 82 ways, CodeLore at full size against grep
over the project's 20.7 MB of markdown and against a semantic code index. The right record came **first for 98% of pasted errors
and 71% of paraphrased symptoms** (grep 61% and 7%, the semantic index 39% and
24%), and an agent read **about 1 KB to reach the answer** instead of 4-13 KB.
The method, the table, why, and what the numbers do not say yet:
[docs/RESULTS.md](docs/RESULTS.md).

## The problem it solves

A project worked on by agents accumulates knowledge fast and loses it just as
fast. What it looks like after a few months:

- a troubleshooting page of 1 500 table rows and 1.4 MB, each row a story,
  found by grep when the exact error string is known and missed otherwise;
- a verification page of 29 000 lines that only ever grew, with plans and
  post-mortems filed under unrelated headings;
- hundreds of hand-back reports written once and read never;
- plans of which part was done, part silently abandoned, and nothing says which;
- agent instruction files that keep growing because there is nowhere else to
  put what was learned.

All of these numbers are from one real repository. The cause is the same in
every case: knowledge is written as **prose in shared documents**, appended to
and never retired, with no unit smaller than a file and no rule for what an
entry must say.

## What CodeLore does instead

- **Knowledge is records, not prose.** A fault is *symptom, the exact strings
  it shows, cause, fix, and the check that sees it* - a few lines, not a page.
  A reading is *what is measured, by which command, the value, at which commit*.
  A decision is *the question, the choice, the reason, who decided*.
- **Plans are records too.** A plan has a goal and items; an item ends **done**
  (with its commits), **dropped** (with a reason and who decided),
  **superseded** or **deferred** - never just forgotten. A plan cannot close
  while an item is open, and `lore stale` lists what nobody has touched.
- **The records live in your repository**, as a few JSONL files - one record a
  line, sorted, readable in a diff, cloned with the project. No server, no
  account, no database to sync.
- **One CLI writes them, and it is strict.** Required fields, length limits that
  keep a record a record, references checked against `HEAD`, and a duplicate
  search before every new entry. `lore` commits its own change, so an agent
  never stages a shared file by hand.
- **Search is hybrid.** SQLite FTS5 for exact strings - an error message, a code,
  a symbol - and embeddings for "something like this", merged by reciprocal rank
  fusion. Embeddings come from a local model (Ollama by default) and are
  optional: without one, `lore` searches text and says so.
- **The index is disposable.** It is built from the JSONL into your user cache
  and never committed.

## A taste of it

```bash
lore find "tar: Damaged tar archive (bad header checksum)"
lore find - < traceback.txt          # paste an error, get the records that know it
lore for src/render/post/chain.py    # what is known about this file before you edit it

lore add fault --json < record.json  # validated, de-duplicated, committed
lore plans                           # live plans: open / done / dropped
lore stale --days 14                 # open items nobody touched
lore item drop api-v2#4 --reason "the v1 endpoint is enough for now"
lore plan close api-v2 --outcome "..."
lore check                           # the gate: schema, refs, dependency cycles
```

## For agents

`lore guide` prints the instructions an agent needs - how to search, how to
record, how to work a plan. A project's `AGENTS.md` / `CLAUDE.md` then needs one
line instead of a growing essay:

```markdown
Project knowledge and plans are in CodeLore: run `lore guide` once per session.
```

## Orchestrators

CodeLore is the durable half of planning, not a task runner. A multi-agent
orchestrator (an agent-teams runner, say) plans and executes; it writes the
plan, its items and how each closed into the project's lore through the same
CLI (`--json` in and out). Who is working on what *right now* is the
orchestrator's runtime state and stays there. CodeLore knows nothing about any
orchestrator; without one, a person or a single agent keeps the same records by
hand.

## What it is not

Not a tracker: no assignees, no notifications, no board. Not a server. Not a
transcript or chat memory. Not a replacement for code search - it indexes what
was *learned*, not the source.

## Requirements

Python 3.11+ (standard library only; SQLite with FTS5, which every current
CPython ships), git. Optional: an embedding model behind Ollama
(`ollama pull nomic-embed-text`).

## Development

```bash
python -m unittest discover -s tests -t tests
```

The tests build scratch git repositories; they need git and nothing else.

## License

MIT - see [LICENSE](LICENSE).
