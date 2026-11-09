## The ledger

This project keeps a ledger beside its lore: structured data, not knowledge -
labels, chosen values, settings per object. Each entry is a kind (declared in
lore.toml under `[ledger.kinds.<kind>]`) and a key (a free string, often a
path). Every writer appends to journals of its own, so nothing ever merges; the
state is folded from the lines' `parents`, never from their clocks. Ledger
commands write files and nothing else - keeping the files is their owner's job.

    lore ledger set <kind> <key> field=value ... [--unset field] [--note "..."]
    lore ledger set --batch < lines.jsonl     # {"kind", "key", "set", "parents"?, "note"?} a line
    lore ledger get <kind> <key>              # the values in force, any conflict, and whether complete
    lore ledger find <kind> --where field=value [--missing field]
    lore ledger fold [--with-state]           # the whole state in one read; --with-state adds heads,
                                              # conflicts, missing parents and `complete`
    lore ledger log <kind> <key>              # who set what, and when
    lore ledger conflicts                     # concurrent lines that set one field differently
    lore ledger resolve <kind> <key> <field> <value> [--parents <ids you saw>]
    lore ledger clear <kind> <key>            # unset what is set; the entry and its history stay
    lore ledger check [--strict] | status

`--strict` (get, find, fold, check) refuses a state that is not complete: a
parent whose journal has not arrived, or a journal that ends in an unfinished
line. A write to a journal that ends that way is refused; `lore ledger repair`
sets the unfinished bytes aside in `<file>.torn-<n>`.

Who writes: `--by` or `LORE_BY` (`[a-z0-9][a-z0-9._-]*`); the machine is the
host name or `LORE_MACHINE`. A field's value is the type its kind declares: a
string or enum as written, a number, bool, list or object as JSON. A retired
field or kind is read and kept but never shown or written again.

Pending lines: `set --to <file>` writes to a file of your own instead of the
journal; `--pending <file>` reads one over the journals. `promote <file>` moves
lines into your journal - `--id` exact ones (a pending parent must come along),
`--key`/`--kind` whole entries; `--keep` copies them and leaves the file, and
`ack <file> --id ...` takes them out once whoever sends them confirms it.
`export <file> --id ...` prints the chosen lines without changing anything.

A knowledge record names a ledger entry in `refs.records` as `ledger:<kind>:<key>`.
