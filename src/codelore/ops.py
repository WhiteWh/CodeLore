"""Every write goes through `apply`: fill, validate, check references and plan
rules against the base the commit will be built on, then commit by the
configured mode. Nothing is half-written."""
from __future__ import annotations

import copy
import re
import unicodedata
from dataclasses import dataclass, field
from typing import Callable

from codelore import FORMAT_VERSION, LoreError
from codelore import cover, gitio
from codelore.schema import (AREA_OF, ITEM_ID, ITEM_MOVES, KNOWLEDGE, UNDECIDED, generic_signal, item_number, today,
                             validate)
from codelore.store import Snapshot, check_placement, is_lore_file, parse, path_of, serialize, load_worktree


@dataclass
class Change:
    """One record put: a new one, or the new version of an existing one."""
    record: dict
    is_new: bool
    reopen_reason: str | None = None


@dataclass
class Outcome:
    commit: str | None
    records: list[dict]
    notes: list[str] = field(default_factory=list)
    pushed: bool | None = None          # None: no push was asked for
    push_error: str | None = None


def slugify(title: str) -> str:
    s = unicodedata.normalize("NFKD", title).encode("ascii", "ignore").decode()
    words = [w for w in re.split(r"[^a-z0-9]+", s.lower()) if w]
    out = ""
    for w in words:
        if len(out) + len(w) + (1 if out else 0) > 40:
            break
        out = f"{out}-{w}" if out else w
    if not out and words:
        out = words[0][:40]
    return out if len(out) >= 2 else "record"


def load_rev(cfg, rev: str | None) -> Snapshot:
    if rev is None:
        return Snapshot({})
    files = {}
    for rel in gitio.ls_tree(cfg.root, rev, cfg.dir):
        if is_lore_file(rel, cfg.dir):
            files[rel] = parse(gitio.blob(cfg.root, rev, rel) or b"", f"{rel} at {rev[:10]}")
    return Snapshot(files)


def snapshot_for(cfg, rev_or_worktree: str | None, worktree: bool) -> Snapshot:
    return load_worktree(cfg.root, cfg.dir) if worktree else load_rev(cfg, rev_or_worktree)


def base_snapshot(cfg) -> Snapshot:
    """What the next write will be built on: the branch's tip under
    shared-tree, the working tree otherwise."""
    if cfg.commit_mode == "shared-tree":
        return load_rev(cfg, gitio.ref(cfg.root, f"refs/heads/{cfg.branch}"))
    return load_worktree(cfg.root, cfg.dir)


# -- the rules a change is held to -------------------------------------------------


def _cycles(items: dict[str, dict]) -> list[list[str]]:
    """Strongly connected components of more than one node, or a self-loop (Tarjan)."""
    index, low, on, stack, comps = {}, {}, set(), [], []
    counter = [0]

    def strong(v):
        index[v] = low[v] = counter[0]
        counter[0] += 1
        stack.append(v)
        on.add(v)
        for d in items[v].get("deps") or []:
            w = d.get("item") if isinstance(d, dict) else None
            if w not in items:
                continue
            if w not in index:
                strong(w)
                low[v] = min(low[v], low[w])
            elif w in on:
                low[v] = min(low[v], index[w])
        if low[v] == index[v]:
            comp = []
            while True:
                w = stack.pop()
                on.discard(w)
                comp.append(w)
                if w == v:
                    break
            if len(comp) > 1:
                comps.append(sorted(comp, key=item_number))

    import sys
    old = sys.getrecursionlimit()
    sys.setrecursionlimit(max(old, 10000))
    try:
        for v in items:
            if v not in index:
                strong(v)
    finally:
        sys.setrecursionlimit(old)
    return comps


def check_all(cfg, snap: Snapshot) -> list[str]:
    """Everything `lore check` holds a whole snapshot to, bar references to git."""
    errs = check_placement(snap, cfg.dir)
    by_id = {}
    for _, r in snap.records():
        errs.extend(validate(r, cfg.areas, cfg.limits))
        if isinstance(r.get("id"), str):
            by_id[r["id"]] = r
    errs.extend(_relations(by_id))
    return errs


def _relations(by_id: dict[str, dict]) -> list[str]:
    errs = []
    items = {k: v for k, v in by_id.items() if v.get("kind") == "item"}
    for rid, r in by_id.items():
        for target in (r.get("refs") or {}).get("records", []):
            if target not in by_id:
                errs.append(f"{rid}: refs.records names {target!r}, which does not exist")
        for k in ("superseded_by", "reversed_by"):
            t = r.get(k)
            if t and t not in by_id:
                errs.append(f"{rid}: `{k}` names {t!r}, which does not exist")
        if r.get("kind") == "source":
            for t in r.get("records") or []:
                if t not in by_id:
                    errs.append(f"{rid}: `records` names {t!r}, which does not exist")
            for e in r.get("parts") or []:
                for t in (e.get("records") or []) if isinstance(e, dict) else []:
                    if t not in by_id:
                        errs.append(f"{rid}: part {e.get('part')!r} names {t!r}, which does not exist")
        if r.get("kind") == "item":
            plan = by_id.get(r.get("plan"))
            if plan is None or plan.get("kind") != "plan":
                errs.append(f"{rid}: its plan {r.get('plan')!r} does not exist")
            for d in r.get("deps") or []:
                if isinstance(d, dict) and d.get("item") not in items:
                    errs.append(f"{rid}: depends on {d.get('item')!r}, which is not an item")
    paths: dict[str, str] = {}
    for rid, r in sorted(by_id.items()):
        if r.get("kind") == "source" and isinstance(r.get("path"), str):
            if r["path"] in paths:
                errs.append(f"{rid}: {r['path']} already has a source record, {paths[r['path']]}")
            paths.setdefault(r["path"], rid)
    for comp in _cycles(items):
        errs.append("a dependency cycle: " + " -> ".join(comp + [comp[0]]))
    for rid, r in by_id.items():
        if r.get("kind") == "plan" and r.get("status") == "closed":
            open_ = sorted((i for i in items.values() if i.get("plan") == rid and i.get("status") in UNDECIDED),
                           key=lambda i: item_number(i["id"]))
            if open_:
                errs.append(f"plan {rid} is closed with {len(open_)} item(s) still undecided: "
                            + ", ".join(f"{i['id']} ({i['status']})" for i in open_))
    return errs


def commits_of(r: dict) -> set[str]:
    """Every commit a record cites."""
    c = set((r.get("refs") or {}).get("commits", [])) | set(r.get("commits") or [])
    if r.get("kind") == "reading" and r.get("measured_at"):
        c.add(r["measured_at"])
    return c


def _ref_errors(cfg, pairs: list[tuple[dict, dict | None]], rev: str | None) -> list[str]:
    """Commits must exist; files must exist at the base - only for references
    a change adds, since a file may go away after a record names it. One git
    process for the whole change, however many records it holds."""
    added = []
    for r, old in pairs:
        commits = sorted(commits_of(r) - commits_of(old or {}))
        files = sorted(set((r.get("refs") or {}).get("files", []))
                       - set(((old or {}).get("refs") or {}).get("files", [])))
        added.append((r.get("id"), commits, files))
    files_rev = rev or gitio.head(cfg.root)
    have_c = gitio.commits_exist(cfg.root, sorted({c for _, cs, _ in added for c in cs}))
    have_f = gitio.files_at(cfg.root, files_rev, sorted({p.rstrip("/") for _, _, fs in added for p in fs}))
    errs = []
    for rid, commits, files in added:
        errs.extend(f"{rid}: commit {c} is not in this repository" for c in commits if not have_c.get(c))
        errs.extend(f"{rid}: file {p!r} is not in {'HEAD' if not rev else rev[:10]}"
                    for p in files if not have_f.get(p.rstrip("/")))
    return errs


def _signal_errors(r: dict, old: dict | None) -> list[str]:
    """A signal is the exact text this fault prints; a bare class name or one
    word is printed by every fault of its family, finds them all and makes
    each new one look like a duplicate. Only signals the change adds."""
    new = [s for s in r.get("signals") or [] if s not in set((old or {}).get("signals") or [])]
    return [f"{r.get('id')}: signal {s!r} is a bare name every fault of its kind prints - give the "
            f"line it prints whole (`{s}: <the message>`), or leave it out" for s in new if generic_signal(s)]


def _transition_errors(new: dict, old: dict | None, reopen_reason: str | None) -> list[str]:
    if old is None or new.get("kind") != "item":
        return []
    a, b = old.get("status"), new.get("status")
    if b in ITEM_MOVES.get(a, set()):
        return []
    if b in UNDECIDED and reopen_reason:
        return []
    return [f"{new['id']}: an item that is `{a}` does not become `{b}` - "
            "reopen it with a reason (`lore item reopen`)"]


# -- apply -----------------------------------------------------------------------


Builder = Callable[[Snapshot], list[Change]]


def apply(cfg, build: Builder, by: str, subject: str | None = None,
          trailers: list[str] | None = None, mode: str | None = None) -> Outcome:
    mode = mode or cfg.commit_mode
    done: dict = {}

    def transform(old_bytes: dict[str, bytes | None], rev: str | None) -> dict[str, bytes]:
        snap = load_worktree(cfg.root, cfg.dir) if mode != "shared-tree" else load_rev(cfg, rev)
        changes = build(snap)
        existing = snap.by_id()
        date = today()
        files = {k: list(v) for k, v in snap.files.items()}
        errs: list[str] = []
        put: list[dict] = []
        for ch in changes:
            r = copy.deepcopy(ch.record)
            old = existing.get(r.get("id"))
            if ch.is_new and old is not None:
                errs.append(f"{r.get('id')!r} already exists - update it, or give the new record another id")
                continue
            if not ch.is_new and old is None:
                errs.append(f"{r.get('id')!r} does not exist")
                continue
            r["v"] = FORMAT_VERSION
            r["by"] = by
            r["updated"] = date
            r.setdefault("created", (old or {}).get("created", date))
            r = {k: v for k, v in r.items() if v is not None}
            if old is not None and same(r, old):
                continue  # nothing but the stamps would change: no write, no commit
            errs.extend(validate(r, cfg.areas, cfg.limits))
            errs.extend(_transition_errors(r, old, ch.reopen_reason))
            if old is not None and old.get("kind") != r.get("kind"):
                errs.append(f"{r['id']}: a record does not change kind ({old.get('kind')} -> {r.get('kind')})")
            if old is not None:
                try:
                    if path_of(old, cfg.dir) != path_of(r, cfg.dir):
                        errs.append(f"{r['id']}: moving a record to another area is a new record "
                                    "that supersedes this one")
                except KeyError:
                    pass
            errs.extend(_signal_errors(r, old))
            if r.get("kind") == "source":
                errs.extend(cover.ledger_errors(cfg, r, old, rev))
            put.append(r)
        if not errs:
            errs.extend(_ref_errors(cfg, [(r, existing.get(r["id"])) for r in put], rev))
        if errs:
            raise LoreError("refused, nothing was written", errs)
        for r in put:
            rel = path_of(r, cfg.dir)
            rs = [x for x in files.get(rel, []) if x.get("id") != r["id"]]
            rs.append(r)
            files[rel] = rs
        whole = Snapshot(files)
        errs = check_placement(whole, cfg.dir) + _relations(whole.by_id())
        # Only the faults this change causes: an untouched record already in
        # the base is `lore check`'s to report.
        before = set(check_placement(snap, cfg.dir) + _relations(existing))
        errs = [e for e in errs if e not in before]
        if errs:
            raise LoreError("refused, nothing was written", errs)
        touched = sorted({path_of(r, cfg.dir) for r in put})
        done["records"] = put
        return {rel: serialize(files[rel]) for rel in touched}

    snap0 = base_snapshot(cfg)
    probe = build(snap0)
    if not probe:
        raise LoreError("nothing to write")
    # The message names what changes, not what was asked: a batch whose
    # patches mostly match what is there commits only the rest.
    known = snap0.by_id()
    probe = [c for c in probe if c.is_new or known.get(c.record.get("id")) is None
             or not same(c.record, known[c.record["id"]])]
    if not probe:
        return Outcome(None, [], ["nothing changed: every record already says this"])
    paths = sorted({path_of(c.record, cfg.dir) for c in probe if _placeable(c.record)})
    subj = subject or _subject(probe)
    body = "\n".join(f"- {c.record.get('id')}: {c.record.get('title', '')}" for c in probe[:50])
    trailers = list(cfg.trailers) + list(trailers or [])
    if mode == "none":
        res = gitio.write_none(cfg.root, paths, transform)
    elif mode == "plain":
        res = gitio.write_plain(cfg.root, paths, transform, subj, body, trailers)
    else:
        res = gitio.write_shared(cfg.root, cfg.branch, paths, transform, subj, body, trailers)
    out = Outcome(res.commit, done.get("records", []), list(res.notes))
    if res.commit and cfg.push:
        err = gitio.push(cfg.root, cfg.remote, cfg.branch)
        out.push_error = _first_line(err) if err else None
        out.pushed = err is None
    return out


def _first_line(err: str) -> str:
    """The line of git's complaint that says why: `fatal:` or `error:` first."""
    lines = [l.strip() for l in err.splitlines() if l.strip()]
    for l in lines:
        if l.startswith(("fatal:", "error:")):
            return l
    return lines[0] if lines else "no reason given"


def _placeable(r: dict) -> bool:
    try:
        path_of(r, "x")
        return True
    except KeyError:
        return False


def _subject(changes: list[Change]) -> str:
    if len(changes) == 1:
        c = changes[0]
        verb = "add" if c.is_new else "update"
        st = c.record.get("status")
        if c.record.get("kind") == "item" and not c.is_new and st:
            verb = {"done": "done", "dropped": "drop", "superseded": "supersede",
                    "deferred": "defer", "open": "reopen"}.get(st, verb)
        if c.record.get("kind") == "plan" and st == "closed":
            verb = "close"
        return f"lore: {verb} {c.record.get('kind')} {c.record.get('id')}"
    kinds = sorted({c.record.get("kind") for c in changes})
    return f"lore: {len(changes)} records ({', '.join(kinds)})"


# -- builders --------------------------------------------------------------------


def fill_ids(records: list[dict], snap: Snapshot) -> list[dict]:
    """Give each record without an id one: a slug of its title, or the next
    number in its plan. Ids in the same batch count."""
    taken = set(snap.by_id())
    nxt: dict[str, int] = {}
    for _, r in snap.records():
        if r.get("kind") == "item" and isinstance(r.get("plan"), str):
            nxt[r["plan"]] = max(nxt.get(r["plan"], 0), item_number(r.get("id", "")))
    for r in records:
        if r.get("id"):
            taken.add(r["id"])
            if r.get("kind") == "item":
                m = ITEM_ID.match(r["id"])
                if m:
                    nxt[m.group(1)] = max(nxt.get(m.group(1), 0), int(m.group(2)))
    res = []
    for r in records:
        r = dict(r)
        if not r.get("id"):
            if r.get("kind") == "item":
                plan = r.get("plan") or ""
                nxt[plan] = nxt.get(plan, 0) + 1
                r["id"] = f"{plan}#{nxt[plan]}"
            else:
                base = slugify(r.get("title", ""))
                cand, n = base, 2
                while cand in taken:
                    cand = f"{base[:44]}-{n}"
                    n += 1
                r["id"] = cand
            taken.add(r["id"])
        if r.get("kind") in AREA_OF:
            r.setdefault("area", AREA_OF[r["kind"]])
        res.append(r)
    return res


def adder(records: list[dict]) -> Builder:
    def build(snap: Snapshot) -> list[Change]:
        return [Change(r, True) for r in fill_ids(records, snap)]
    return build


def _patched(cur: dict, patch: dict) -> dict:
    """`patch` merged into the record: a key set to None is removed."""
    r = copy.deepcopy(cur)
    for k, v in patch.items():
        if k in ("id", "kind", "v", "created", "updated", "by"):
            if k in ("id", "kind") and v is not None and v != r.get(k):
                raise LoreError(f"{cur.get('id')}: `{k}` of a record does not change")
            continue
        if v is None:
            r.pop(k, None)
        else:
            r[k] = v
    return r


def updater(rid: str, patch: dict | Callable[[dict], dict], reopen_reason: str | None = None) -> Builder:
    """`patch` is a dict, or a function of the record as the write finds it -
    so `field+=value` appends to what the commit is built on, not to what an
    index read earlier."""
    def build(snap: Snapshot) -> list[Change]:
        cur = snap.by_id().get(rid)
        if cur is None:
            raise LoreError(f"{rid!r} does not exist")
        return [Change(_patched(cur, patch(cur) if callable(patch) else patch), False, reopen_reason)]
    return build


def updater_many(patches: list[dict]) -> Builder:
    """Several patches, each naming its record by `id`, in one commit."""
    def build(snap: Snapshot) -> list[Change]:
        by_id = snap.by_id()
        missing = [p["id"] for p in patches if p["id"] not in by_id]
        if missing:
            raise LoreError("refused, nothing was written", [f"{m!r} does not exist" for m in missing])
        return [Change(_patched(by_id[p["id"]], p), False) for p in patches]
    return build


#: Fields a write sets itself; a change that touches nothing else is no change.
STAMPS = ("updated", "by", "v")


def same(a: dict, b: dict) -> bool:
    return {k: v for k, v in a.items() if k not in STAMPS} == {k: v for k, v in b.items() if k not in STAMPS}


def knowledge_kinds() -> tuple[str, ...]:
    return KNOWLEDGE
