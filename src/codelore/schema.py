"""The record kinds of FORMAT.md, as rules. `validate` refuses; it never repairs."""
from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import date

from codelore import FORMAT_VERSION, LoreError

ID = re.compile(r"^[a-z0-9][a-z0-9-]{1,47}$")
ITEM_ID = re.compile(r"^([a-z0-9][a-z0-9-]{1,47})#([1-9][0-9]{0,5})$")
DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
SHA = re.compile(r"^[0-9a-f]{4,64}$")

STR, LIST, DEPS, INT = "str", "list", "deps", "int"


@dataclass(frozen=True)
class Field:
    type: str
    max: int | None = None          # characters for STR, per element for LIST
    required: bool = False
    when: tuple[str, ...] = ()      # required only under these statuses


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
        "statuses": ("live", "cured", "obsolete"),
        "fields": {
            "symptom": Field(STR, 600, required=True),
            "signals": Field(LIST, 200),
            "cause": Field(STR, 600, required=True),
            "fix": Field(STR, 800, required=True),
            "check": Field(STR, 300, required=True),
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
            "superseded_by": Field(STR, 64, when=("superseded",)),
        },
    },
    "decision": {
        "statuses": ("active", "reversed"),
        "fields": {
            "question": Field(STR, 300, required=True),
            "choice": Field(STR, 600, required=True),
            "reason": Field(STR, 800, required=True),
            "decided_by": Field(STR, 120, required=True),
            "reversed_by": Field(STR, 64, when=("reversed",)),
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
            "commits": Field(LIST, 64, when=("done",)),
            "reason": Field(STR, 600, when=("dropped", "deferred")),
            "decided_by": Field(STR, 120, when=("dropped",)),
            "superseded_by": Field(STR, 64, when=("superseded",)),
        },
    },
}

KNOWLEDGE = ("fault", "reading", "decision")
PLAN_KINDS = ("plan", "item")
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
    if v != FORMAT_VERSION:
        if isinstance(v, int) and v > FORMAT_VERSION:
            errs.append(f"{who}: format version {v} is newer than this lore reads ({FORMAT_VERSION}) - upgrade lore")
        else:
            errs.append(f"{who}: `v` must be {FORMAT_VERSION}")

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
        if f.when and status not in f.when:
            errs.append(f"{who}: `{name}` belongs to status {'/'.join(f.when)}, not `{status}`")
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
    if kind in PLAN_KINDS:
        if area != "plans":
            errs.append(f"{who}: a {kind}'s `area` is `plans`")
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


def _is_date(s) -> bool:
    if not isinstance(s, str) or not DATE.match(s):
        return False
    try:
        date.fromisoformat(s)
        return True
    except ValueError:
        return False


def item_number(item_id: str) -> int:
    m = ITEM_ID.match(item_id)
    return int(m.group(2)) if m else 0


def main_text(r: dict) -> str:
    """The sentence a record is about, for display and for its embedding."""
    for k in ("symptom", "what", "question", "goal", "description"):
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
