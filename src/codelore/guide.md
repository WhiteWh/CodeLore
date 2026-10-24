# lore - how to work with this project's memory

This repository keeps what was learned while working on it in **CodeLore**:
records of faults met, measured readings, decisions, and plans with their items.
They are searched, not read whole. Every command takes `--json`.

## Before you diagnose anything, search

    lore find "<the exact error text>"      # an error message, an HRESULT, an exit code
    lore find "<what you see, in words>"    # when there is no exact text
    lore find - < log.txt                   # paste a traceback or a log; its error lines are the query
    lore for <path/to/file.py>              # what is known about a file before you edit it
    lore for <module.symbol>
    lore show <id>

Search with the exact text first: a stored `signals` string that appears in your
query ranks first. A result's `fix` and `check` are what to do and how to see
that it worked. If the first line says the search is text only, the semantic
half is off - rephrase with the words the record would use.

## When you met something the next agent would meet again, record it

One record, a few lines. **No story**: how it was found belongs in the commit
message; the record says what it is. Every field has a length limit and lore
refuses a record that is past it.

    lore add fault --json <<'EOF'
    {"area": "<area>", "title": "<what it is, under 120 chars>",
     "symptom": "<what is observed>", "signals": ["<exact text it prints>"],
     "cause": "<why>", "fix": "<what to do>", "check": "<the command that sees it>",
     "status": "live", "refs": {"commits": ["<sha>"], "files": ["<path>"]}}
    EOF

- **fault**: `symptom`, `signals`, `cause`, `fix`, `check`; status `live` / `cured` / `obsolete`.
- **reading**: `what`, `command`, `value`, `measured_at` (a commit); status `current` / `superseded`.
- **decision**: `question`, `choice`, `reason`, `decided_by`; status `active` / `reversed`.
- **topic**: `body` (up to 3000 characters) - how something works now, never its history; status `current` / `superseded`. A longer subject is several topics.
- **recipe**: `when`, `steps` (a list), `check` - a procedure done more than once; status `current` / `superseded`.

Before you change how something works, `lore find` its topic; after, update the
topic in the same piece of work, so the next reader meets the present.

lore searches for a duplicate before it writes. On strong evidence - a signal
another record carries, near-identical text - it refuses and names the record:
improve that one with `lore update <id> -s field=value`, or, if yours really is
a different fault, repeat with `--new`. On weaker likeness it writes and names
the nearest records: read them, and if one is the same fault, fold yours into
it. `lore update --batch` takes many patches (JSONL, each with its `id`) in one
commit. lore checks that every commit and file you reference exists, and
commits the record itself - do not edit the JSONL files by hand and do not
stage them.

`lore areas` lists the areas. Set who you are once: `--by agent:<model>` or the
`LORE_BY` environment variable.

## Plans

A plan is a goal and items. An item ends in exactly one of four ways, and never
by being forgotten:

    lore item done <plan>#<n> --commit <sha>
    lore item drop <plan>#<n> --reason "<why it is not needed>" --decided-by <who>
    lore item supersede <plan>#<n> --with <plan>#<m>
    lore item defer <plan>#<n> --reason "<until what>"

    lore plans                    # live plans, with open / done / dropped counts
    lore plan show <plan>         # items in dependency layers, ready ones marked
    lore items --ready            # what can be started now
    lore stale --days 14          # undecided items nobody has touched
    lore plan new --title "..." --goal "..."
    lore item add <plan> --title "..." --criterion "..." --dep <plan>#<n>[:soft]
    lore plan close <plan> --outcome "<built, dropped and why, what the plan got wrong>"

Drop an item only with the authority to: `--decided-by` names who decided. A
plan does not close while an item is open or deferred.

## Rules

- Search before you diagnose; record before you finish.
- One record per fault. Improve an existing record rather than adding a second.
- A number is recorded with the commit it was measured at.
- Do not edit `lore/*.jsonl` by hand; `lore check` refuses a file that was.
