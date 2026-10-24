"""The record kinds of FORMAT.md, as rules. `validate` refuses; it never repairs."""
from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import date

from codelore import FORMAT_VERSION, READS, LoreError

ID = re.compile(r"^[a-z0-9][a-z0-9-]{1,47}$")
ITEM_ID = re.compile(r"^([a-z0-9][a-z0-9-]{1,47})#([1-9][0-9]{0,5})$")
DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
SHA = re.compile(r"^[0-9a-f]{4,64}$")

STR, LIST, DEPS, INT = "str", "list", "deps", "int"
#: One record's id; a list of them; a source's ledger of parts.
REF, IDS, PARTS = "ref", "ids", "parts"


@dataclass(frozen=True)
class Field:
    type: str
    max: int | None = None          # characters for STR, per element for LIST
    required: bool = False
    when: tuple[str, ...] = ()      # required only under these statuses
    only: tuple[str, ...] = ()      # allowed only under these statuses (`when` implies it)
    hint: str = ""                  # said when given under a status it does not belong to


COMMON = {
    "v": Field(INT, required=True),
    "id": Field(STR, required=True),
    "kind": Field(STR, required=True),
    "area": Field(STR, required=True),
    "title": Field(STR, 120, required=True),
    "status": Field(STR, required=True),
    "created": Field(STR, required=True),
    "updated": Field(STR, required=True),
    "by": Field(STR, 120, required=True),
}

KINDS: dict[str, dict] = {
    "fault": {
        "statuses": ("live", "cured", "obsolete", "superseded"),
        "fields": {
            "symptom": Field(STR, 600, required=True),
            "signals": Field(LIST, 200),
            "cause": Field(STR, 600, required=True),
            "fix": Field(STR, 800, required=True),
            "check": Field(STR, 300, required=True),
            "superseded_by": Field(REF, when=("superseded",)),
        },
    },
    "reading": {
        "statuses": ("current", "superseded"),
        "fields": {
            "what": Field(STR, 300, required=True),
            "command": Field(STR, 300, required=True),
            "value": Field(STR, 200, required=True),
            "tolerance": Field(STR, 200),
            "measured_at": Field(STR, 64, required=True),
            "superseded_by": Field(REF, when=("superseded",)),
        },
    },
    "decision": {
        "statuses": ("active", "reversed", "superseded"),
        "fields": {
            "question": Field(STR, 300, required=True),
            "choice": Field(STR, 600, required=True),
            "reason": Field(STR, 800, required=True),
            "decided_by": Field(STR, 120, required=True),
            "reversed_by": Field(REF, when=("reversed",)),
            "superseded_by": Field(REF, when=("superseded",)),
        },
    },
    "topic": {
        "statuses": ("current", "superseded"),
        "fields": {
            "body": Field(STR, 3000, required=True),
            "superseded_by": Field(REF, when=("superseded",)),
        },
    },
    "recipe": {
        "statuses": ("current", "superseded"),
        "fields": {
            "when": Field(STR, 300, required=True),
            "steps": Field(LIST, 400, required=True),
            "check": Field(STR, 300, required=True),
            "superseded_by": Field(REF, when=("superseded",)),
        },
    },
    "plan": {
        "statuses": ("draft", "live", "paused", "closed"),
        "fields": {
            "goal": Field(STR, 800, required=True),
            "doc": Field(STR, 300),
            "outcome": Field(STR, 2000, when=("closed",)),
        },
    },
    "item": {
        "statuses": ("open", "done", "dropped", "superseded", "deferred"),
        "fields": {
            "plan": Field(STR, 48, required=True),
            "description": Field(STR, 2000),
            "criteria": Field(LIST, 300),
            "deps": Field(DEPS),
            "commits": Field(LIST, 64, when=("done",),
                             hint="the commits of an item still being built go in `refs.commits` "
                                  "(`-s refs.commits+=<sha>`); `lore item done --commit` closes it with them"),
            "reason": Field(STR, 600, when=("dropped", "deferred")),
            "decided_by": Field(STR, 120, when=("dropped",)),
            "superseded_by": Field(REF, when=("superseded",)),
            "outcome": Field(STR, 1000, only=("done", "dropped", "superseded")),
        },
    },
    "source": {
        "statuses": ("open", "covered", "history"),
        "fields": {
            "path": Field(STR, 300, required=True),
            "reason": Field(STR, 600, when=("history",)),
            "records": Field(IDS),
            "parts": Field(PARTS),
        },
    },
}

KNOWLEDGE = ("fault", "reading", "decision", "topic", "recipe")
#: The format version a kind first appears in.
KIND_SINCE = {"topic": 2, "recipe": 2, "source": 3}
#: The format version a kind's status first appears in.
STATUS_SINCE = {("decision", "superseded"): 3}
PLAN_KINDS = ("plan", "item")
#: Kinds that live outside the knowledge areas, and the area they name.
AREA_OF = {"plan": "plans", "item": "plans", "source": "sources"}
REF_KEYS = ("commits", "files", "symbols", "links", "records")

#: Statuses a search hides unless asked for everything.
RETIRED = {"obsolete", "superseded", "reversed", "closed"}
#: Item statuses that still need someone's decision.
UNDECIDED = ("open", "deferred")

#: The fields a person writes for each kind, in the order they are shown.
SHOWN = {k: list(v["fields"]) for k, v in KINDS.items()}


def today() -> str:
    return date.today().isoformat()


def check_limits(limits: dict[str, int]) -> None:
    bad = []
    for key, n in limits.items():
        kind, _, name = key.partition(".")
        f = KINDS.get(kind, {}).get("fields", {}).get(name) or (COMMON.get(name) if kind in KINDS else None)
        if f is None or f.max is None:
            bad.append(f"`limits.{key}` names no limited field")
        elif n > f.max:
            bad.append(f"`limits.{key}` = {n} raises the format's ceiling of {f.max}; limits may only lower it")
    if bad:
        raise LoreError("lore.toml's [limits] is refused", bad)


def _limit(kind: str, name: str, f: Field, limits: dict[str, int]) -> int | None:
    return limits.get(f"{kind}.{name}", f.max)


def validate(r: dict, areas: list[str], limits: dict[str, int] | None = None) -> list[str]:
    """Every rule the record breaks, as sentences. Empty when it is valid."""
    limits = limits or {}
    errs: list[str] = []
    if not isinstance(r, dict):
        return ["a record is a JSON object"]
    kind = r.get("kind")
    if kind not in KINDS:
        return [f"`kind` must be one of {', '.join(KINDS)}, not {kind!r}"]
    spec = KINDS[kind]
    rid = r.get("id", "")
    who = f"{kind} {rid!r}" if rid else kind

    v = r.get("v")
    if v not in READS:
        if isinstance(v, int) and not isinstance(v, bool) and v > FORMAT_VERSION:
            errs.append(f"{who}: format version {v} is newer than this lore reads ({FORMAT_VERSION}) - upgrade lore")
        else:
            errs.append(f"{who}: `v` must be one of {', '.join(map(str, READS))}")
    elif v < KIND_SINCE.get(kind, 1):
        errs.append(f"{who}: a {kind} needs format version {KIND_SINCE[kind]} or later, not {v}")
    elif v < STATUS_SINCE.get((kind, r.get("status")), 1):
        errs.append(f"{who}: a {r.get('status')} {kind} needs format version "
                    f"{STATUS_SINCE[(kind, r.get('status'))]} or later, not {v}")

    status = r.get("status")
    fields = {**COMMON, **spec["fields"]}
    for name, f in fields.items():
        val = r.get(name)
        needed = f.required or (f.when and status in f.when)
        if val is None or val == "" or val == []:
            if needed:
                cond = f" when `{status}`" if f.when else ""
                errs.append(f"{who}: `{name}` is required{cond}")
            continue
        allowed = f.only or f.when
        if allowed and status not in allowed:
            errs.append(f"{who}: `{name}` belongs to status {'/'.join(allowed)}, not `{status}`"
                        + (f" - {f.hint}" if f.hint else ""))
        lim = _limit(kind, name, f, limits)
        if f.type == INT:
            if not isinstance(val, int) or isinstance(val, bool):
                errs.append(f"{who}: `{name}` must be an integer")
        elif f.type == STR:
            if not isinstance(val, str):
                errs.append(f"{who}: `{name}` must be a string")
            elif lim is not None and len(val) > lim:
                errs.append(f"{who}: `{name}` is {len(val)} characters, the limit is {lim} - "
                            "a record says what, not how it was found; the story goes in the commit")
            elif val != val.strip():
                errs.append(f"{who}: `{name}` has leading or trailing whitespace")
        elif f.type == LIST:
            if not isinstance(val, list) or not all(isinstance(x, str) and x for x in val):
                errs.append(f"{who}: `{name}` must be a list of non-empty strings")
            else:
                for x in val:
                    if lim is not None and len(x) > lim:
                        errs.append(f"{who}: an element of `{name}` is {len(x)} characters, the limit is {lim}")
                if len(set(val)) != len(val):
                    errs.append(f"{who}: `{name}` has duplicates")
        elif f.type == REF:
            if not is_id(val):
                errs.append(f"{who}: `{name}` names one record by its id, and {val!r} is not an id")
        elif f.type == IDS:
            if not isinstance(val, list) or not all(is_id(x) for x in val):
                errs.append(f"{who}: `{name}` must be a list of record ids")
            elif len(set(val)) != len(val):
                errs.append(f"{who}: `{name}` has duplicates")
        elif f.type == PARTS:
            errs.extend(f"{who}: {e}" for e in parts_errors(val))
        elif f.type == DEPS:
            if not isinstance(val, list):
                errs.append(f"{who}: `deps` must be a list")
            else:
                seen = set()
                for d in val:
                    if (not isinstance(d, dict) or set(d) != {"item", "kind"}
                            or d.get("kind") not in ("hard", "soft")
                            or not isinstance(d.get("item"), str) or not ITEM_ID.match(d["item"])):
                        errs.append(f"{who}: each dependency is {{\"item\": \"<plan>#<n>\", \"kind\": \"hard\"|\"soft\"}}, not {d!r}")
                        continue
                    if d["item"] in seen:
                        errs.append(f"{who}: depends on {d['item']} twice")
                    seen.add(d["item"])
                    if d["item"] == rid:
                        errs.append(f"{who}: depends on itself")

    if status not in spec["statuses"]:
        errs.append(f"{who}: `status` must be one of {', '.join(spec['statuses'])}, not {status!r}")

    if isinstance(rid, str):
        if kind == "item":
            m = ITEM_ID.match(rid)
            if not m:
                errs.append(f"{who}: an item's `id` is `<plan-id>#<n>`")
            elif r.get("plan") != m.group(1):
                errs.append(f"{who}: the id names plan {m.group(1)!r} but `plan` is {r.get('plan')!r}")
        elif not ID.match(rid):
            errs.append(f"{who}: `id` must match {ID.pattern}")

    area = r.get("area")
    if kind in AREA_OF:
        if area != AREA_OF[kind]:
            errs.append(f"{who}: a {kind}'s `area` is `{AREA_OF[kind]}`")
    elif area not in areas:
        errs.append(f"{who}: area {area!r} is not one of lore.toml's areas ({', '.join(areas) or 'none declared'})")

    for name in ("created", "updated"):
        val = r.get(name)
        if isinstance(val, str) and not _is_date(val):
            errs.append(f"{who}: `{name}` must be a date, YYYY-MM-DD")
    if _is_date(r.get("created")) and _is_date(r.get("updated")) and r["updated"] < r["created"]:
        errs.append(f"{who}: `updated` is before `created`")

    if kind == "item" and isinstance(r.get("commits"), list):
        for c in r["commits"]:
            if isinstance(c, str) and not SHA.match(c):
                errs.append(f"{who}: commit {c!r} is not a hash")
    if kind == "reading" and isinstance(r.get("measured_at"), str) and not SHA.match(r["measured_at"]):
        errs.append(f"{who}: `measured_at` must be a commit hash")

    refs = r.get("refs")
    if refs is not None:
        if not isinstance(refs, dict):
            errs.append(f"{who}: `refs` must be an object")
        else:
            for k, val in refs.items():
                if k not in REF_KEYS:
                    errs.append(f"{who}: `refs.{k}` is not one of {', '.join(REF_KEYS)}")
                elif not isinstance(val, list) or not all(isinstance(x, str) and x for x in val):
                    errs.append(f"{who}: `refs.{k}` must be a list of non-empty strings")
                elif k == "commits":
                    errs.extend(f"{who}: `refs.commits` holds {c!r}, not a hash" for c in val if not SHA.match(c))
                elif k == "files":
                    errs.extend(f"{who}: `refs.files` holds {p!r} - paths are relative, with forward slashes"
                                for p in val if "\\" in p or p.startswith("/") or re.match(r"^[A-Za-z]:", p))
    return errs


def is_id(s) -> bool:
    return isinstance(s, str) and bool(ID.match(s) or ITEM_ID.match(s))


#: The keys of one entry in a source's ledger.
PART_KEYS = {"part", "records", "skip"}


def part_key(text: str) -> str:
    """A part's name as compared: case and runs of whitespace folded."""
    return " ".join(text.split()).casefold()


def parts_errors(val) -> list[str]:
    """A source's ledger: each entry names one part of the document and says
    which records hold it, or why it is skipped, or both."""
    if not isinstance(val, list):
        return ['`parts` must be a list of {"part", "records", "skip"} entries']
    errs, seen = [], set()
    for e in val:
        if (not isinstance(e, dict) or not isinstance(e.get("part"), str) or not e["part"].strip()
                or set(e) - PART_KEYS):
            errs.append('each entry of `parts` is {"part": "<name>", "records": [<id>...], '
                        f'"skip": "<why>"}}, not {e!r}')
            continue
        p = e["part"]
        if len(p) > 300:
            errs.append(f"part {p[:40]!r}...: the name is {len(p)} characters, the limit is 300")
        recs, skip = e.get("records"), e.get("skip")
        if recs is not None and (not isinstance(recs, list) or not recs or not all(is_id(x) for x in recs)):
            errs.append(f"part {p!r}: `records` must be a non-empty list of record ids")
        if skip is not None and (not isinstance(skip, str) or not skip.strip() or len(skip) > 300):
            errs.append(f"part {p!r}: `skip` is why it holds nothing current, 1-300 characters")
        if recs is None and skip is None:
            errs.append(f"part {p!r}: says neither which records hold it nor why it is skipped")
        k = part_key(p)
        if k in seen:
            errs.append(f"part {p!r} is in `parts` twice")
        seen.add(k)
    return errs


def _is_date(s) -> bool:
    if not isinstance(s, str) or not DATE.match(s):
        return False
    try:
        date.fromisoformat(s)
        return True
    except ValueError:
        return False


#: A bare exception or error class name: printed by every fault of its kind.
_BARE_CLASS = re.compile(r"^[A-Za-z_][\w.]*(Error|Exception|Warning|Failure|Fault)$")
_ONE_WORD = re.compile(r"^[A-Za-z]+$")


def generic_signal(s: str) -> bool:
    """A signal that would match every fault of a family, not this one:
    `AttributeError`, `System.IO.IOException`, `error`, `Traceback`. A code
    (`ERR_DEVICE_LOST`, `0x80070005`, `E_FAIL`) or a whole message is not."""
    s = s.strip()
    return bool(_BARE_CLASS.match(s) or _ONE_WORD.match(s))


def item_number(item_id: str) -> int:
    m = ITEM_ID.match(item_id)
    return int(m.group(2)) if m else 0


def main_text(r: dict) -> str:
    """The sentence a record is about, for display and for its embedding."""
    for k in ("symptom", "what", "question", "when", "goal", "body", "description"):
        if r.get(k):
            return r[k]
    return ""


#: How an item may move. Leaving a closed status needs `--reopen`.
ITEM_MOVES = {
    "open": {"done", "dropped", "superseded", "deferred", "open"},
    "deferred": {"open", "done", "dropped", "superseded", "deferred"},
    "done": {"done"},
    "dropped": {"dropped"},
    "superseded": {"superseded"},
}
