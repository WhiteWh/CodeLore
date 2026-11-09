## The ledger

This project keeps a ledger beside its lore: structured data, not knowledge -
labels, chosen values, settings per object. Each entry is a kind (declared in
lore.toml under `[ledger.kinds.<kind>]`) and a key (a free string, often a
path). Every writer appends to journals of its own, so nothing ever merges; the
state is folded from the lines' `parents`, never from their clocks. Ledger
commands write files and nothing else - keeping the files is their owner's job.

    lore ledger set <kind> <key> field=value ... [--unset field] [--note "..."]
    lore ledger set --batch < lines.jsonl     # {"kind", "key", "set", "parents"?, "note"?} a line
    lore ledger get <kind> <key>              # the values in force, and any conflict
    lore ledger find <kind> --where field=value
    lore ledger fold                          # the whole state as one JSON
    lore ledger log <kind> <key>              # who set what, and when
    lore ledger conflicts                     # concurrent lines that set one field differently
    lore ledger resolve <kind> <key> <field> <value>
    lore ledger check | status

Who writes: `--by` or `LORE_BY` (`[a-z0-9][a-z0-9._-]*`); the machine is the
host name or `LORE_MACHINE`. A field's value is the type its kind declares: a
string or enum as written, a number, bool, list or object as JSON.

Pending lines: `set --to <file>` writes to a file of your own instead of the
journal; `--pending <file>` reads one over the journals for get, find, fold and
conflicts; `promote <file> [--key K ...] [--kind K ...]` moves the chosen
entries' lines into your journal and leaves the others in the file.

A knowledge record names a ledger entry in `refs.records` as `ledger:<kind>:<key>`.
