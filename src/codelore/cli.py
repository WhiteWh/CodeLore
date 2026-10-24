"""The `lore` command."""
from __future__ import annotations

import argparse
import copy
import json
import os
import sys
import textwrap
from importlib import resources
from pathlib import Path

from codelore import LoreError, __version__
from codelore import config as configmod
from codelore import cover, dedupe, embed, gitio, ops, plans, search
from codelore.index import open_index
from codelore.schema import KINDS, KNOWLEDGE, SHOWN, UNDECIDED, check_limits, item_number, validate
from codelore.store import canonical, check_placement, is_lore_file, serialize, sort_key

WIDTH = 100


# -- output ------------------------------------------------------------------------


def emit_json(obj) -> None:
    sys.stdout.write(json.dumps(obj, ensure_ascii=False, indent=2) + "\n")


def say(text: str = "") -> None:
    sys.stdout.write(text + "\n")


def note(text: str) -> None:
    say(f"note: {text}")


def fmt_record(r: dict, score: float | None = None, via: list[str] | None = None) -> str:
    head = f"{r.get('kind')} {r.get('id')}  [{r.get('status')}]"
    if r.get("kind") not in ("plan", "item"):
        head += f"  area: {r.get('area')}"
    if score is not None:
        head += f"  score {score}" + (f" ({'+'.join(via)})" if via else "")
    lines = [head, textwrap.indent(textwrap.fill(r.get("title", ""), WIDTH - 2), "  ")]
    for k in SHOWN.get(r.get("kind"), []):
        v = r.get(k)
        if v in (None, "", []):
            continue
        if k == "deps":
            v = ", ".join(f"{d['item']}" + ("" if d["kind"] == "hard" else " (soft)") for d in v)
        elif k == "signals":
            v = "  |  ".join(v)
        elif k == "steps":
            lines.append("  steps:")
            lines.extend(textwrap.fill(f"{n}. {s}", WIDTH, initial_indent="    ", subsequent_indent="       ")
                         for n, s in enumerate(v, 1))
            continue
        elif k == "parts":
            lines.append(f"  parts: {len(v)}")
            lines.extend(textwrap.fill(f"{e.get('part')} -> " + "; ".join(
                filter(None, [", ".join(e.get("records") or []), e.get("skip") and f"skip: {e['skip']}"])),
                WIDTH, initial_indent="    ", subsequent_indent="        ") for e in v if isinstance(e, dict))
            continue
        elif isinstance(v, list):
            v = "; ".join(v)
        lines.append(textwrap.fill(f"{k}: {v}", WIDTH, initial_indent="  ", subsequent_indent="      "))
    refs = r.get("refs") or {}
    if refs:
        parts = [f"{k} {', '.join(v)}" for k, v in refs.items() if v]
        lines.append(textwrap.fill("refs: " + "; ".join(parts), WIDTH, initial_indent="  ",
                                   subsequent_indent="      "))
    lines.append(f"  ({r.get('updated')} by {r.get('by')})")
    return "\n".join(lines)


# -- setting fields from the command line ------------------------------------------


def parse_sets(items: list[str], base: dict | None = None) -> dict:
    """`key=value`, `key+=value` (append to a list), `key:=<json>`, `key=` (remove).
    `refs.files+=path` reaches into refs; `deps+=plan#2:soft` makes a dependency."""
    patch: dict = {}
    base = base or {}
    for s in items or []:
        if ":=" in s and s.index(":=") < (s.index("=") if "=" in s else len(s)):
            k, v = s.split(":=", 1)
            try:
                val = json.loads(v)
            except json.JSONDecodeError as e:
                raise LoreError(f"`-s {s}`: not JSON ({e.msg})") from e
            _put(patch, base, k, val, append=False)
        elif "+=" in s:
            k, v = s.split("+=", 1)
            if k == "deps":
                v = dep_of(v)
            _put(patch, base, k, v, append=True)
        elif "=" in s:
            k, v = s.split("=", 1)
            _put(patch, base, k, v if v != "" else None, append=False)
        else:
            raise LoreError(f"`-s {s}`: expected key=value, key+=value or key:=json")
    return patch


def _put(patch: dict, base: dict, key: str, val, append: bool) -> None:
    if key.startswith("refs."):
        sub = key[5:]
        refs = patch.setdefault("refs", dict(base.get("refs") or {}))
        if append:
            refs[sub] = list(refs.get(sub, [])) + [val]
        elif val is None:
            refs.pop(sub, None)
        else:
            refs[sub] = val if isinstance(val, list) else [val]
        return
    if append:
        cur = patch.get(key, base.get(key)) or []
        if isinstance(cur, str):
            raise LoreError(f"`-s {key}+=...`: `{key}` is text, and += appends to a list - "
                            f"give the whole new text with `-s {key}=...`")
        patch[key] = list(cur) + [val]
    else:
        patch[key] = val


def dep_of(s: str) -> dict:
    item, _, kind = s.partition(":")
    kind = kind or "hard"
    if kind not in ("hard", "soft"):
        raise LoreError(f"dependency {s!r}: the kind after `:` is hard or soft")
    return {"item": item, "kind": kind}


def stdin_text() -> str:
    """stdin as UTF-8 whatever the console's code page: on Windows `sys.stdin`
    decodes as cp1252, which turns «» and … into two or three characters each
    and writes them into a record that way."""
    buf = getattr(sys.stdin, "buffer", None)
    if buf is None:
        return sys.stdin.read()
    data = buf.read()
    try:
        return data.decode("utf-8-sig")
    except UnicodeDecodeError as e:
        raise LoreError(f"stdin is not UTF-8 ({e})") from e


def read_stdin_json():
    text = stdin_text()
    if not text.strip():
        raise LoreError("expected JSON on stdin and got nothing")
    try:
        return json.loads(text)
    except json.JSONDecodeError as e:
        raise LoreError(f"stdin is not JSON ({e.msg} at line {e.lineno})") from e


def read_stdin_jsonl() -> list[dict]:
    res = []
    for n, line in enumerate(stdin_text().splitlines(), 1):
        if not line.strip():
            continue
        try:
            r = json.loads(line)
        except json.JSONDecodeError as e:
            raise LoreError(f"stdin line {n} is not JSON ({e.msg})") from e
        if not isinstance(r, dict):
            raise LoreError(f"stdin line {n} is not a JSON object")
        res.append(r)
    return res


# -- context -----------------------------------------------------------------------


class Ctx:
    def __init__(self, args):
        self.args = args
        start = Path(args.C) if args.C else Path.cwd()
        self.cfg = configmod.load(start)
        check_limits(self.cfg.limits)
        self._idx = None

    @property
    def idx(self):
        if self._idx is None:
            self._idx = open_index(self.cfg)
            if self._idx.place_note:
                progress(f"note: {self._idx.place_note}")
        return self._idx

    def record(self, rid: str) -> tuple[dict | None, str | None]:
        """A record by id, live: the index (the working tree), and when the
        working tree does not have it, the branch the next write builds on -
        another session's commit that has not reached this working tree yet."""
        r = self.idx.get(rid)
        if r is not None:
            return r, None
        rev = gitio.ref(self.cfg.root, f"refs/heads/{self.cfg.branch}") if self.cfg.commit_mode == "shared-tree" \
            else gitio.head(self.cfg.root)
        if rev is None:
            return None, None
        r = ops.load_rev(self.cfg, rev).by_id().get(rid)
        return r, (rev if r is not None else None)

    def provider(self):
        if getattr(self.args, "text_only", False):
            return None
        return embed.provider(self.cfg)

    def by(self) -> str:
        return (self.args.by or os.environ.get("LORE_BY") or gitio.user_name(self.cfg.root) or "unknown").strip()

    def write(self, build, subject=None, notes: list[str] | None = None) -> int:
        out = ops.apply(self.cfg, build, self.by(), subject=subject,
                        trailers=self.args.trailer, mode=self.args.mode)
        out.notes = list(notes or []) + out.notes
        where = f"{self.cfg.remote}/{self.cfg.branch}"
        line = None
        if out.commit:
            line = {None: f"committed {out.commit[:10]}",
                    True: f"committed {out.commit[:10]}, pushed to {where}",
                    False: f"committed {out.commit[:10]} locally, push to {where} failed: {out.push_error}; "
                           "the next write's push carries it"}[out.pushed]
        if self.args.json:
            emit_json({"commit": out.commit, "records": out.records,
                       "notes": out.notes + ([line] if out.pushed is False else []),
                       **({"pushed": out.pushed, "push_error": out.push_error} if out.pushed is not None else {})})
            return 0
        if line:
            say(line)
        for r in out.records:
            say(f"  {r['kind']} {r['id']}  [{r['status']}]  {r.get('title', '')}")
        for n in out.notes:
            note(n)
        return 0


def progress(msg: str) -> None:
    sys.stderr.write(msg + "\n")
    sys.stderr.flush()


# -- commands ----------------------------------------------------------------------


def cmd_init(args) -> int:
    start = Path(args.C) if args.C else Path.cwd()
    root = gitio.toplevel(start)
    if root is None:
        raise LoreError(f"{start} is not inside a git repository")
    path = root / configmod.CONFIG_NAME
    if path.exists():
        raise LoreError(f"{path} exists already")
    areas = [a.strip() for a in (args.areas or "general").split(",") if a.strip()]
    text = configmod.TEMPLATE.format(dir=args.dir, areas=", ".join(f'"{a}"' for a in areas),
                                     mode=args.mode_init, branch=args.branch or gitio.current_branch(root) or "main")
    cfg = configmod.parse(__import__("tomllib").loads(text), root)
    path.write_text(text, encoding="utf-8", newline="\n")
    (root / cfg.dir / "plans").mkdir(parents=True, exist_ok=True)
    ga = root / ".gitattributes"
    line = f"{cfg.dir}/** text eol=lf"
    cur = ga.read_text(encoding="utf-8") if ga.exists() else ""
    if line not in cur.splitlines():
        ga.write_text(cur + ("" if not cur or cur.endswith("\n") else "\n") + line + "\n",
                      encoding="utf-8", newline="\n")
    say(f"wrote {path.relative_to(root)} and the `{line}` line in .gitattributes; commit them.")
    say("Add this line to AGENTS.md (or CLAUDE.md):")
    say("")
    say("    Project knowledge and plans are in CodeLore: run `lore guide` once per session.")
    return 0


def cmd_guide(args) -> int:
    say(resources.files("codelore").joinpath("guide.md").read_text(encoding="utf-8").rstrip())
    return 0


def cmd_areas(ctx: Ctx) -> int:
    counts = {a: 0 for a in ctx.cfg.areas}
    for r in ctx.idx.all():
        if r.get("area") in counts:
            counts[r["area"]] += 1
    if ctx.args.json:
        emit_json(counts)
    else:
        for a, n in counts.items():
            say(f"{a:20} {n}")
    return 0


def cmd_find(ctx: Ctx) -> int:
    a = ctx.args
    q = " ".join(a.query)
    if q.strip() == "-":
        q = search.errors_of(stdin_text())
    if not q.strip():
        raise LoreError("nothing to search for")
    f = search.Filters(kind=a.kind, area=a.area, status=a.status, plan=a.plan, all=a.all)
    hits, notes = search.find(ctx.idx, q, f, limit=a.n, prov=ctx.provider(), progress=progress)
    if a.json:
        emit_json({"query": q, "notes": notes,
                   "results": [{"score": h.score, "via": h.via, "record": h.record} for h in hits]})
        return 0
    for n in notes:
        note(n)
    if not hits:
        say("nothing found")
        return 0
    say("\n\n".join(fmt_record(h.record, h.score, h.via) for h in hits))
    return 0


def cmd_for(ctx: Ctx) -> int:
    a = ctx.args
    res = search.for_target(ctx.idx, a.target, search.Filters(all=a.all))
    if a.json:
        emit_json(res)
    elif not res:
        say(f"nothing recorded about {a.target}")
    else:
        say("\n\n".join(fmt_record(r) for r in res))
    return 0


def cmd_show(ctx: Ctx) -> int:
    r, rev = ctx.record(ctx.args.id)
    if r is None:
        raise LoreError(f"{ctx.args.id!r} does not exist")
    if ctx.args.json:
        emit_json(r)
    else:
        if rev:
            note(f"not in the working tree yet; shown as committed at {rev[:10]}")
        say(fmt_record(r))
        if r.get("kind") == "plan":
            say("")
            _show_items(ctx, plans.items_of(ctx.idx.all(), r["id"]))
    return 0


def cmd_exists(ctx: Ctx) -> int:
    """Which of these ids are records now - read live, never from a copy."""
    res = {}
    for rid in ctx.args.ids:
        r, _ = ctx.record(rid)
        res[rid] = None if r is None else {"kind": r.get("kind"), "status": r.get("status"),
                                           "title": r.get("title", "")}
    if ctx.args.json:
        emit_json(res)
    else:
        for rid, r in res.items():
            say(f"{rid}  " + ("MISSING" if r is None else f"{r['kind']} [{r['status']}]  {r['title']}"))
    return 0 if all(res.values()) else 1


def _new_from_args(ctx: Ctx, kind: str) -> list[dict]:
    a = ctx.args
    if a.batch:
        recs = read_stdin_jsonl()
        for r in recs:
            r.setdefault("kind", kind)
    elif a.json_in:
        data = read_stdin_json()
        recs = data if isinstance(data, list) else [data]
        for r in recs:
            if not isinstance(r, dict):
                raise LoreError("stdin must hold a JSON object or a list of objects")
            r.setdefault("kind", kind)
    else:
        recs = [{"kind": kind, **parse_sets(a.set)}]
    for r in recs:
        if r.get("kind") != kind and kind != "any":
            raise LoreError(f"a {r.get('kind')} record given to `lore add {kind}`")
        if r.get("kind") in KINDS:
            r.setdefault("status", KINDS[r["kind"]]["statuses"][0])
    return recs


def cmd_add(ctx: Ctx) -> int:
    a = ctx.args
    recs = _new_from_args(ctx, a.kind)
    found = dedupe.candidates(ctx.cfg, ctx.idx, recs, ctx.provider())
    blocking = {i: ms for i, ms in found.items() if any(m.blocking for m in ms)}

    def named(i: int) -> str:
        r = recs[i]
        return (f"#{i + 1} " if len(recs) > 1 else "") + (f"{r['id']} " if r.get("id") else "") \
            + repr(r.get("title", "?"))

    def matches(ms) -> str:
        return "; ".join(f"{m.id} ({m.how} {m.similarity})" for m in ms if m.blocking)

    skipped = []
    if blocking and not a.new:
        if not a.skip_duplicates:
            lines = [f"{named(i)} is: {matches(ms)}" for i, ms in sorted(blocking.items())]
            rest = len(recs) - len(blocking)
            how = ("improve the existing record with `lore update <id>`, or repeat with --new if yours is "
                   "a different one")
            if rest:
                how += f"; --skip-duplicates writes the other {rest} and leaves these out"
            raise LoreError(f"refused, nothing was written: {len(blocking)} of {len(recs)} record(s) "
                            f"already exist - {how}", lines)
        skipped = [f"left out {named(i)}: it is {matches(ms)}" for i, ms in sorted(blocking.items())]
        keep = [i for i in range(len(recs)) if i not in blocking]
        if not keep:
            raise LoreError("nothing to write: every record is a duplicate", skipped)
        found = {keep.index(i): ms for i, ms in found.items() if i in keep}
        recs = [recs[i] for i in keep]
        blocking = {}
    near = [f"{recs[i].get('title', '?')!r} is near " + ", ".join(f"{m.id} ({m.how} {m.similarity})" for m in ms)
            + " - if one says the same, merge into it with `lore update`"
            for i, ms in sorted(found.items()) if i not in blocking]
    return ctx.write(ops.adder(recs), notes=skipped + near)


def cmd_update(ctx: Ctx) -> int:
    a = ctx.args
    if a.batch:
        if a.id or a.set:
            raise LoreError("--batch reads every patch from stdin: give no id and no -s")
        patches = read_stdin_jsonl()
        for n, p in enumerate(patches, 1):
            if not isinstance(p.get("id"), str):
                raise LoreError(f"stdin line {n}: a batch patch names its record by `id`")
        return ctx.write(ops.updater_many(patches))
    if a.json_in:
        if a.set:
            raise LoreError("--json-in reads the patch from stdin: give no -s")
        patch = read_stdin_json()
        if isinstance(patch, list):
            raise LoreError("stdin holds a list; several patches go through `lore update --batch` (JSONL)")
        if not isinstance(patch, dict):
            raise LoreError("the patch on stdin must be a JSON object")
        rid = a.id or patch.get("id")
        if not isinstance(rid, str) or not rid:
            raise LoreError("which record? give `lore update <id> --json-in`, or `id` in the patch")
        if a.id and patch.get("id") not in (None, a.id):
            raise LoreError(f"the command names {a.id!r} and the patch names {patch['id']!r}")
        return ctx.write(ops.updater(rid, patch))
    if not a.id:
        raise LoreError("which record? `lore update <id>`, or --json-in with `id` in the patch, or --batch")
    sets = a.set
    return ctx.write(ops.updater(a.id, lambda cur: parse_sets(sets, cur)))


def cmd_check(ctx_args) -> int:
    start = Path(ctx_args.C) if ctx_args.C else Path.cwd()
    cfg = configmod.load(start)
    errs: list[str] = []
    warns: list[str] = []
    try:
        check_limits(cfg.limits)
    except LoreError as e:
        errs.extend(e.details)
    files: dict[str, list[dict]] = {}
    d = cfg.lore_dir
    if d.is_dir():
        for p in sorted(list(d.glob("*.jsonl")) + list((d / "plans").glob("*.jsonl"))):
            rel = p.relative_to(cfg.root).as_posix()
            try:
                from codelore.store import parse
                files[rel] = parse(p.read_bytes(), rel)
            except LoreError as e:
                errs.append(str(e).splitlines()[0])
                errs.extend("  " + x for x in e.details)
        for p in d.rglob("*"):
            rel = p.relative_to(cfg.root).as_posix()
            if p.is_file() and not is_lore_file(rel, cfg.dir):
                warns.append(f"{rel} is not a lore file and is ignored")
    from codelore.store import Snapshot
    snap = Snapshot(files)
    n = sum(len(v) for v in files.values())
    if not ctx_args.json:
        progress(f"checking {n} record(s) in {len(files)} file(s)...")
    errs.extend(ops.check_all(cfg, snap))
    head = gitio.head(cfg.root)
    records = [r for _, r in snap.records()]
    # Every reference in one git process each: one per reference took minutes.
    have_c = gitio.commits_exist(cfg.root, sorted({c for r in records for c in ops.commits_of(r)}))
    live = [r for r in records if r.get("status") not in ("obsolete", "superseded", "reversed")]
    have_f = gitio.files_at(cfg.root, head, sorted({p.rstrip("/") for r in live
                                                     for p in (r.get("refs") or {}).get("files", [])}))
    for r in records:
        for c in sorted(ops.commits_of(r)):
            if not have_c.get(c):
                errs.append(f"{r.get('id')}: commit {c} is not in this repository")
    if head:
        for r in live:
            for p in (r.get("refs") or {}).get("files", []):
                if not have_f.get(p.rstrip("/")):
                    warns.append(f"{r.get('id')}: file {p!r} is gone from HEAD - is the record obsolete?")
    for r in records:
        if r.get("kind") == "source" and r.get("status") == "covered":
            data = cover.read_doc(cfg, r["path"]) if isinstance(r.get("path"), str) else None
            if data is not None:
                cov = cover.coverage(cover.parts_of(cover.doc_text(data)), r.get("parts") or [])
                if cov.uncovered or cov.unknown:
                    warns.append(f"{r['id']}: {r['path']} changed since it was covered - {len(cov.uncovered)} "
                                 f"part(s) unaccounted for, {len(cov.unknown)} entry(ies) naming no part; "
                                 f"`lore cover {r['path']}`")
    if ctx_args.evidence:
        errs.extend(evidence(cfg, records, have_c))
    if not ctx_args.json:
        progress("...done")
    if ctx_args.json:
        emit_json({"records": n, "files": len(files), "errors": errs, "warnings": warns})
    else:
        for w in warns:
            say(f"warn: {w}")
        for e in errs:
            say(f"FAIL: {e}")
        say(f"{n} record(s) in {len(files)} file(s): "
            + ("ok" if not errs else f"{len(errs)} failure(s)"))
    return 1 if errs else 0


def evidence(cfg, records: list[dict], have_c: dict[str, bool]) -> list[str]:
    """A cited commit is evidence when it touches a file the record cites, or
    when its hash is written in a source document whose ledger names the
    record. A commit that exists and does neither may be cited by mistake -
    existence alone passes any real hash."""
    sources: dict[str, list[str]] = {}
    for r in records:
        if r.get("kind") == "source" and isinstance(r.get("path"), str):
            for t in set(r.get("records") or []) | {x for e in r.get("parts") or [] if isinstance(e, dict)
                                                    for x in e.get("records") or []}:
                sources.setdefault(t, []).append(r["path"])
    texts: dict[str, str] = {}

    def doc(path: str) -> str:
        if path not in texts:
            data = cover.read_doc(cfg, path)
            texts[path] = cover.doc_text(data).lower() if data else ""
        return texts[path]

    cited = {}
    for r in records:
        files = [p.rstrip("/") for p in (r.get("refs") or {}).get("files", [])]
        commits = [c for c in set((r.get("refs") or {}).get("commits", [])) | set(r.get("commits") or [])
                   if have_c.get(c)]
        if commits and (files or r.get("id") in sources):
            cited[r["id"]] = (r, files, commits)
    touched = gitio.files_touched(cfg.root, sorted({c for _, _, cs in cited.values() for c in cs}))
    errs = []
    for rid, (r, files, commits) in sorted(cited.items()):
        for c in sorted(commits):
            paths = touched.get(c, set())
            if any(p == f or p.startswith(f + "/") for p in paths for f in files):
                continue
            if any(c.lower()[:7] in doc(s) for s in sources.get(rid, [])):
                continue
            where = f"touches none of {', '.join(files)}" if files else "touches no file the record cites"
            errs.append(f"{rid}: commit {c} {where}"
                        + (f", and no source of it ({', '.join(sources[rid])}) mentions it" if rid in sources else "")
                        + " - is it the right commit?")
    return errs


def _doc_path(ctx: Ctx, arg: str) -> str:
    """A document named on the command line, as a path from the repository root."""
    root = ctx.cfg.root.resolve()
    p = Path(arg)
    for cand in ([p] if p.is_absolute() else [Path.cwd() / p, root / p]):
        try:
            return cand.resolve().relative_to(root).as_posix()
        except ValueError:
            continue
    raise LoreError(f"{arg} is not inside the repository")


def _source_of(snap_records, path: str) -> dict | None:
    return next((r for r in snap_records if r.get("kind") == "source" and r.get("path") == path), None)


def cmd_cover(ctx: Ctx) -> int:
    a = ctx.args
    path = _doc_path(ctx, a.doc)
    if a.add or a.history:
        return _cover_write(ctx, path)
    src = _source_of(ops.base_snapshot(ctx.cfg).by_id().values(), path)
    data = cover.read_doc(ctx.cfg, path)
    if data is None:
        if src is None:
            raise LoreError(f"{path} is not in the working tree or HEAD, and no source record names it")
        parts, cov = [], None
    else:
        parts = cover.parts_of(cover.doc_text(data))
        cov = cover.coverage(parts, (src or {}).get("parts") or [])
    history = src is not None and src.get("status") == "history"
    entries = {cover.part_key(e["part"]): e for e in (src or {}).get("parts") or []}
    uncovered = [] if history or cov is None else cov.uncovered
    if a.json:
        emit_json({"path": path, "source": src, "parts": len(parts),
                   "uncovered": [{"part": p.name, "kind": p.kind, "line": p.line} for p in uncovered],
                   "unknown": [] if cov is None else cov.unknown,
                   "complete": history or (cov is not None and not uncovered)})
        return 0 if history or (cov is not None and not uncovered) else 1
    if src is None:
        say(f"{path}: no source record yet; {len(parts)} part(s), none accounted for")
    elif history:
        say(f"{path}: history ({src['id']}) - {src.get('reason')}")
        if src.get("records"):
            say("  what still stands is in: " + ", ".join(src["records"]))
    else:
        say(f"{path}: {src['id']} [{src['status']}] - {len(parts) - len(uncovered)} of {len(parts)} "
            f"part(s) accounted for")
    if a.list:
        lost = {id(p) for p in uncovered}
        for p in parts:
            e = entries.get(cover.part_key(p.name))
            what = ("  " + ", ".join(e.get("records") or []) + (f"  skip: {e['skip']}" if e.get("skip") else "")) if e \
                else ("  UNACCOUNTED" if id(p) in lost else "")
            say(f"  {p.line:>5} {p.kind:7} {p.name}{what}")
    elif uncovered:
        say("unaccounted for (name each in the ledger: `lore cover <doc> --add`):")
        for p in uncovered:
            say(f"  {p.line:>5} {p.kind:7} {p.name}")
    for u in [] if cov is None else cov.unknown:
        say(f"  ledger entry {u!r} names no part of the document now")
    return 0 if history or (cov is not None and not uncovered) else 1


def _cover_write(ctx: Ctx, path: str) -> int:
    a = ctx.args
    entries = read_stdin_jsonl() if a.add else []
    for n, e in enumerate(entries, 1):
        if not isinstance(e.get("part"), str):
            raise LoreError(f"stdin line {n}: a ledger entry is {{\"part\": ..., \"records\": [...], \"skip\": ...}}")
    cfg = ctx.cfg

    def build(snap) -> list[ops.Change]:
        cur = _source_of(snap.by_id().values(), path)
        r = copy.deepcopy(cur) if cur else {"kind": "source", "area": "sources", "path": path,
                                            "title": (a.title or f"source: {path}")[:120], "status": "open"}
        ledger = {cover.part_key(e["part"]): e for e in r.get("parts") or []}
        for e in entries:
            k = cover.part_key(e["part"])
            if set(e) == {"part"}:
                ledger.pop(k, None)  # an entry that names only its part takes it out
            else:
                ledger[k] = {x: e[x] for x in ("part", "records", "skip") if e.get(x) is not None}
        r["parts"] = list(ledger.values()) or None
        if a.history:
            r["status"], r["reason"] = "history", a.reason
            if a.record:
                r["records"] = list(dict.fromkeys((r.get("records") or []) + a.record))
        elif r.get("status") != "history":
            data = cover.read_doc(cfg, path)
            done = data is not None and not cover.coverage(cover.parts_of(cover.doc_text(data)),
                                                           r.get("parts") or []).uncovered
            r["status"] = "covered" if done else "open"
        if a.title:
            r["title"] = a.title
        if cur:
            return [ops.Change(r, False)]
        return [ops.Change(x, True) for x in ops.fill_ids([r], snap)]

    if a.history and not a.reason:
        raise LoreError("--history needs --reason: why nothing in the document is current")
    return ctx.write(build)


def cmd_fmt(args) -> int:
    """Rewrite hand-edited files canonically and sorted. Reads leniently."""
    start = Path(args.C) if args.C else Path.cwd()
    cfg = configmod.load(start)
    changed = 0
    d = cfg.lore_dir
    for p in sorted(list(d.glob("*.jsonl")) + list((d / "plans").glob("*.jsonl"))) if d.is_dir() else []:
        raw = p.read_bytes()
        text = raw.decode("utf-8-sig").replace("\r\n", "\n")
        recs = []
        for n, line in enumerate(text.split("\n"), 1):
            if not line.strip():
                continue
            try:
                recs.append(json.loads(line))
            except json.JSONDecodeError as e:
                raise LoreError(f"{p}: line {n} is not JSON ({e.msg}); fmt does not guess") from e
        data = serialize(recs)
        if data != raw:
            p.write_bytes(data)
            changed += 1
            say(f"rewrote {p.relative_to(cfg.root).as_posix()}")
    say(f"{changed} file(s) rewritten" + ("; commit them" if changed else ""))
    return 0


def cmd_index(ctx: Ctx) -> int:
    a = ctx.args
    if a.rebuild:
        p = ctx.idx.path
        ctx._idx.close()
        ctx._idx = None
        for suffix in ("", "-wal", "-shm", "-journal"):
            q = Path(str(p) + suffix)
            if q.exists():
                q.unlink()
    idx = ctx.idx
    prov = ctx.provider()
    ok = idx.ensure_vectors(prov, progress) if prov else False
    vecs = len(idx.vectors(prov.model)) if ok else 0
    info = {"records": idx.count(), "vectors": vecs, "index": str(idx.path),
            "semantic": ok, "note": idx.semantic_note}
    if a.json:
        emit_json(info)
    else:
        say(f"{info['records']} record(s), {vecs} with vectors, index at {info['index']}")
        if not ok:
            note(idx.semantic_note or "text only")
    return 0


# -- plans -------------------------------------------------------------------------


def cmd_plans(ctx: Ctx) -> int:
    rows = plans.summary(ctx.idx.all(), include_closed=ctx.args.all)
    if ctx.args.json:
        emit_json(rows)
        return 0
    if not rows:
        say("no plans")
    for row in rows:
        p, c = row["plan"], row["counts"]
        say(f"{p['id']:28} [{p['status']}]  open {c['open']}  deferred {c['deferred']}  done {c['done']}"
            f"  dropped {c['dropped']}  superseded {c['superseded']}  - {p.get('title', '')}")
    return 0


def _show_items(ctx: Ctx, items: list[dict]) -> None:
    by_id = {r["id"]: r for r in ctx.idx.all() if r.get("kind") == "item"}
    for n, layer in enumerate(plans.layers(items)):
        say(f"layer {n}")
        for i in layer:
            mark = "*" if plans.ready(i, by_id) else " "
            deps = ", ".join(d["item"] + ("~" if d["kind"] == "soft" else "") for d in i.get("deps") or [])
            say(f"  {mark} {i['id']:24} [{i['status']}] {i.get('title', '')}" + (f"  <- {deps}" if deps else ""))
    if items:
        say("(* ready; ~ a soft dependency)")


def cmd_items(ctx: Ctx) -> int:
    a = ctx.args
    recs = ctx.idx.all()
    by_id = {r["id"]: r for r in recs if r.get("kind") == "item"}
    closed = {r["id"] for r in recs if r.get("kind") == "plan" and r.get("status") == "closed"}
    res = []
    for r in sorted(by_id.values(), key=lambda r: (r.get("plan", ""), item_number(r["id"]))):
        if a.plan and r.get("plan") != a.plan:
            continue
        if a.status and r.get("status") != a.status:
            continue
        if not a.status and not a.all and (r.get("status") not in UNDECIDED or r.get("plan") in closed):
            continue
        if a.ready and not plans.ready(r, by_id):
            continue
        res.append(r)
    if a.json:
        emit_json(res)
        return 0
    if not res:
        say("no items")
    for r in res:
        mark = "*" if plans.ready(r, by_id) else " "
        say(f"{mark} {r['id']:28} [{r['status']}] {r.get('title', '')}")
    return 0


def cmd_stale(ctx: Ctx) -> int:
    res = plans.stale(ctx.cfg.root, ctx.idx.all(), ctx.args.days)
    if ctx.args.json:
        emit_json(res)
        return 0
    if not res:
        say(f"nothing undecided has gone untouched for {ctx.args.days} day(s)")
    for r in res:
        say(f"{r['id']:28} [{r['status']}] since {r.get('updated')}  {r.get('title', '')}  - keep, drop, or defer?")
    return 0


def cmd_plan_new(ctx: Ctx) -> int:
    a = ctx.args
    r = {"kind": "plan", "title": a.title, "goal": a.goal, "status": a.status, "area": "plans"}
    if a.id:
        r["id"] = a.id
    if a.doc:
        r["doc"] = a.doc.replace("\\", "/")
    r.update(parse_sets(a.set))
    return ctx.write(ops.adder([r]))


def cmd_plan_set(ctx: Ctx) -> int:
    a = ctx.args
    if a.status == "closed":
        raise LoreError("a plan closes with `lore plan close <id> --outcome ...`")
    sets, status = a.set, a.status

    def patch(cur: dict) -> dict:
        p = parse_sets(sets, cur)
        if status:
            p["status"] = status
        return p
    return ctx.write(ops.updater(a.id, patch))


def cmd_plan_close(ctx: Ctx) -> int:
    a = ctx.args
    return ctx.write(ops.updater(a.id, {"status": "closed", "outcome": a.outcome}))


def cmd_item_add(ctx: Ctx) -> int:
    a = ctx.args
    r = {"kind": "item", "plan": a.plan, "title": a.title, "status": "open", "area": "plans"}
    if a.description:
        r["description"] = a.description
    if a.criterion:
        r["criteria"] = a.criterion
    if a.dep:
        r["deps"] = [dep_of(d) for d in a.dep]
    r.update(parse_sets(a.set))
    return ctx.write(ops.adder([r]))


#: Fields that only some item statuses carry; a move clears the ones it leaves.
CONDITIONAL = {"commits": ("done",), "reason": ("dropped", "deferred"),
               "decided_by": ("dropped",), "superseded_by": ("superseded",),
               "outcome": ("done", "dropped", "superseded")}


def _move(ctx: Ctx, rid: str, status: str, fields: dict, reopen_reason: str | None = None) -> int:
    patch = {k: None for k, sts in CONDITIONAL.items() if status not in sts}
    patch.update({k: v for k, v in fields.items() if v is not None or k not in ("outcome",)})
    patch["status"] = status
    subject = None
    if reopen_reason:
        subject = f"lore: reopen item {rid}\n\nreason: {reopen_reason}"
    out = ops.updater(rid, patch, reopen_reason)
    return ctx.write(out, subject=subject)


def cmd_item_done(ctx: Ctx) -> int:
    return _move(ctx, ctx.args.id, "done", {"commits": ctx.args.commit, "outcome": ctx.args.outcome})


def cmd_item_drop(ctx: Ctx) -> int:
    a = ctx.args
    return _move(ctx, a.id, "dropped", {"reason": a.reason, "decided_by": a.decided_by, "outcome": a.outcome})


def cmd_item_supersede(ctx: Ctx) -> int:
    return _move(ctx, ctx.args.id, "superseded", {"superseded_by": ctx.args.with_, "outcome": ctx.args.outcome})


def cmd_item_defer(ctx: Ctx) -> int:
    return _move(ctx, ctx.args.id, "deferred", {"reason": ctx.args.reason})


def cmd_item_reopen(ctx: Ctx) -> int:
    return _move(ctx, ctx.args.id, "open", {}, reopen_reason=ctx.args.reason)


# -- the parser --------------------------------------------------------------------


def _common(top: bool) -> argparse.ArgumentParser:
    """The options every command takes, before or after its name. Under a
    command their defaults are suppressed, so a value given before the command
    is not overwritten by the command's default."""
    c = argparse.ArgumentParser(add_help=False)
    kw = {} if top else {"default": argparse.SUPPRESS}
    c.add_argument("-C", metavar="PATH", help="run as if started in PATH", **kw)
    c.add_argument("--json", action="store_true", help="machine-readable output", **kw)
    c.add_argument("--by", help="who writes: a name, or agent:<model> (default: $LORE_BY, git user.name)", **kw)
    c.add_argument("--trailer", action="append", help="a commit trailer line, repeatable",
                   **({"default": []} if top else kw))
    c.add_argument("--mode", choices=configmod.MODES, help="override commit.mode for this call", **kw)
    c.add_argument("--text-only", action="store_true", help="do not use embeddings", **kw)
    return c


def build_parser() -> argparse.ArgumentParser:
    common = _common(top=False)
    p = argparse.ArgumentParser(prog="lore", description="Project memory for coding agents, kept in your repo.",
                                parents=[_common(top=True)])
    p.add_argument("--version", action="version", version=f"lore {__version__}")
    sub = p.add_subparsers(dest="cmd", metavar="<command>")

    def cmd(name, func, help, **kw):
        sp = sub.add_parser(name, help=help, parents=[common], **kw)
        sp.set_defaults(func=func)
        return sp

    sp = cmd("init", cmd_init, "create lore.toml in this repository")
    sp.add_argument("--dir", default="lore")
    sp.add_argument("--areas", help="comma-separated knowledge areas")
    sp.add_argument("--commit-mode", dest="mode_init", default="plain", choices=configmod.MODES)
    sp.add_argument("--branch")
    cmd("guide", cmd_guide, "print the instructions for agents")
    cmd("areas", cmd_areas, "list the knowledge areas and their record counts")
    sp = cmd("check", cmd_check, "the gate: every file, record, reference and plan rule")
    sp.add_argument("--evidence", action="store_true",
                    help="also fail a cited commit that touches none of the record's files")
    cmd("fmt", cmd_fmt, "rewrite hand-edited files canonically")
    sp = cmd("index", cmd_index, "build the index and embeddings now; report")
    sp.add_argument("--rebuild", action="store_true")

    filt = argparse.ArgumentParser(add_help=False)
    filt.add_argument("--kind", choices=list(KINDS))
    filt.add_argument("--area")
    filt.add_argument("--status")
    filt.add_argument("--plan")
    filt.add_argument("--all", action="store_true", help="include retired records")
    sp = sub.add_parser("find", help="search: exact strings, words and meaning; `-` reads a log from stdin",
                        parents=[common, filt])
    sp.set_defaults(func=cmd_find)
    sp.add_argument("query", nargs="+")
    sp.add_argument("-n", type=int, default=5, help="how many results (default 5)")
    sp = sub.add_parser("for", help="what is recorded about a file, directory or symbol", parents=[common])
    sp.set_defaults(func=cmd_for)
    sp.add_argument("target")
    sp.add_argument("--all", action="store_true")
    sp = cmd("show", cmd_show, "one record by id")
    sp.add_argument("id")
    sp = cmd("exists", cmd_exists, "whether each id is a record now; exit 1 if one is not")
    sp.add_argument("ids", nargs="+", metavar="id")
    sp = cmd("cover", cmd_cover, "a source document's coverage: which parts records hold, which are skipped")
    sp.add_argument("doc", help="the document, a path in the repository")
    sp.add_argument("--list", action="store_true", help="every part, with what accounts for it")
    sp.add_argument("--add", action="store_true",
                    help="merge ledger entries from stdin (JSONL: part, records, skip); one commit")
    sp.add_argument("--history", action="store_true", help="the whole document is history, nothing current")
    sp.add_argument("--reason", help="with --history: why")
    sp.add_argument("--record", action="append", help="with --history: a record holding what still stands")
    sp.add_argument("--title", help="the source record's title")

    writes = argparse.ArgumentParser(add_help=False)
    writes.add_argument("-s", "--set", action="append", default=[], metavar="KEY=VALUE",
                        help="key=value, key+=value, key:=json, key= (remove)")
    sp = sub.add_parser("add", help="add records: -s fields, --json (stdin), --batch (JSONL stdin)",
                        parents=[common, writes])
    sp.set_defaults(func=cmd_add)
    sp.add_argument("kind", choices=list(KNOWLEDGE) + ["plan", "item", "any"])
    sp.add_argument("--json-in", "--stdin", dest="json_in", action="store_true",
                    help="read one record (or a list) as JSON from stdin")
    sp.add_argument("--batch", action="store_true", help="read JSONL from stdin, one commit")
    sp.add_argument("--new", action="store_true", help="write even if a similar record exists")
    sp.add_argument("--skip-duplicates", action="store_true",
                    help="leave out the records refused as duplicates and write the rest")
    sp = sub.add_parser("update", help="change fields of a record, or of many with --batch",
                        parents=[common, writes])
    sp.set_defaults(func=cmd_update)
    sp.add_argument("id", nargs="?")
    sp.add_argument("--json-in", "--stdin", dest="json_in", action="store_true",
                    help="read the patch as JSON from stdin; null removes a field")
    sp.add_argument("--batch", action="store_true",
                    help="read patches as JSONL from stdin, each naming its record by `id`; one commit")

    sp = cmd("plans", cmd_plans, "plans with their item counts")
    sp.add_argument("--all", action="store_true", help="include closed plans")
    sp = cmd("items", cmd_items, "undecided items")
    sp.add_argument("--plan")
    sp.add_argument("--status")
    sp.add_argument("--ready", action="store_true", help="only items whose hard dependencies are done")
    sp.add_argument("--all", action="store_true")
    sp = cmd("stale", cmd_stale, "undecided items nobody has touched")
    sp.add_argument("--days", type=int, default=14)

    pp = sub.add_parser("plan", help="plan commands").add_subparsers(dest="plan_cmd", metavar="<plan command>")
    sp = pp.add_parser("new", parents=[common, writes], help="a new plan")
    sp.set_defaults(func=cmd_plan_new)
    sp.add_argument("--title", required=True)
    sp.add_argument("--goal", required=True)
    sp.add_argument("--id")
    sp.add_argument("--doc", help="a prose document with the plan's reasoning")
    sp.add_argument("--status", default="live", choices=["draft", "live", "paused"])
    sp = pp.add_parser("show", parents=[common], help="a plan and its items in layers")
    sp.set_defaults(func=cmd_show)
    sp.add_argument("id")
    sp = pp.add_parser("set", parents=[common, writes], help="change a plan's status or fields")
    sp.set_defaults(func=cmd_plan_set)
    sp.add_argument("id")
    sp.add_argument("--status", choices=["draft", "live", "paused"])
    sp = pp.add_parser("close", parents=[common], help="close a plan; refused while an item is undecided")
    sp.set_defaults(func=cmd_plan_close)
    sp.add_argument("id")
    sp.add_argument("--outcome", required=True)

    ip = sub.add_parser("item", help="item commands").add_subparsers(dest="item_cmd", metavar="<item command>")
    sp = ip.add_parser("add", parents=[common, writes], help="a new item in a plan")
    sp.set_defaults(func=cmd_item_add)
    sp.add_argument("plan")
    sp.add_argument("--title", required=True)
    sp.add_argument("--description")
    sp.add_argument("--criterion", action="append")
    sp.add_argument("--dep", action="append", help="<plan>#<n>[:soft], repeatable")
    sp = ip.add_parser("done", parents=[common], help="done, with the commits that did it")
    sp.set_defaults(func=cmd_item_done)
    sp.add_argument("id")
    sp.add_argument("--commit", action="append", required=True)
    sp.add_argument("--outcome", help="what this item built or settled, and what it got wrong")
    sp = ip.add_parser("drop", parents=[common], help="dropped, with the reason and who decided")
    sp.set_defaults(func=cmd_item_drop)
    sp.add_argument("id")
    sp.add_argument("--reason", required=True)
    sp.add_argument("--decided-by", required=True)
    sp.add_argument("--outcome", help="what this item built or settled, and what it got wrong")
    sp = ip.add_parser("supersede", parents=[common], help="replaced by another item")
    sp.set_defaults(func=cmd_item_supersede)
    sp.add_argument("id")
    sp.add_argument("--with", dest="with_", required=True)
    sp.add_argument("--outcome", help="what this item built or settled, and what it got wrong")
    sp = ip.add_parser("defer", parents=[common], help="deferred, with the condition")
    sp.set_defaults(func=cmd_item_defer)
    sp.add_argument("id")
    sp.add_argument("--reason", required=True)
    sp = ip.add_parser("reopen", parents=[common], help="back to open, with the reason")
    sp.set_defaults(func=cmd_item_reopen)
    sp.add_argument("id")
    sp.add_argument("--reason", required=True)
    return p


NO_CTX = {cmd_init, cmd_guide, cmd_check, cmd_fmt}


def main(argv: list[str] | None = None) -> int:
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8")
        except (AttributeError, ValueError):
            pass
    parser = build_parser()
    args = parser.parse_args(argv)
    func = getattr(args, "func", None)
    if func is None:
        parser.print_help()
        return 2
    try:
        if func in NO_CTX:
            return func(args)
        return func(Ctx(args))
    except LoreError as e:
        if getattr(args, "json", False):
            emit_json({"error": str(e).splitlines()[0], "details": e.details})
        else:
            sys.stderr.write(f"lore: {e}\n")
        return 1
    except KeyboardInterrupt:
        return 130
