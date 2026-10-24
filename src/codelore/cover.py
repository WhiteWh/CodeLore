"""A source document's coverage: which of its parts a record holds, which are
skipped and why, which nobody has accounted for.

A Markdown document's parts are its headings, the rows of its tables and its
numbered list items. A part is named the way it is written, so a person can
read the name and find the place:

    6. The faults met                      a heading, its text after the #s
    6. The faults met | E_FAIL on resize   a table row: its heading | its first cell
    Steps / 3. Run the gate                a numbered item: its heading / n. its first words

    Setup / text                           what a heading says itself (prose, code,
                                           bullets), when it also has parts under it

A repeated name takes ` (2)`, ` (3)` in order. A `source` record's `parts` is
the ledger: each entry names a part and the records that hold it, or why it is
skipped, or both. An entry for a heading covers everything under it; a heading
with no entry is covered when everything under it is.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field

from codelore import gitio
from codelore.schema import part_key

HEADING = re.compile(r"^ {0,3}(#{1,6})\s+(.*?)(?:\s+#+)?\s*$")
FENCE = re.compile(r"^ {0,3}(```|~~~)")
ITEM = re.compile(r"^ {0,3}(\d{1,9})[.)]\s+(.*)$")
SEPARATOR = re.compile(r"^\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$")
TOP = "(top)"

#: How much of a row's first cell, or an item's first words, names it.
ROW_NAME = 80
ITEM_NAME = 60


@dataclass
class Part:
    name: str
    kind: str                      # heading | text | row | item
    line: int
    parent: "Part | None" = None
    children: list["Part"] = field(default_factory=list)
    prose: bool = False            # a heading that says something itself


def _short(text: str, n: int) -> str:
    text = " ".join(text.split())
    if len(text) <= n:
        return text
    cut = text[:n].rsplit(" ", 1)[0]
    return (cut or text[:n]).rstrip(" ,;:")


def _cells(line: str) -> list[str]:
    s = line.strip()
    if s.startswith("|"):
        s = s[1:]
    if s.endswith("|") and not s.endswith("\\|"):
        s = s[:-1]
    return [c.strip() for c in re.split(r"(?<!\\)\|", s)]


def parts_of(text: str) -> list[Part]:
    """Every part of a Markdown document, in order."""
    lines = text.replace("\r\n", "\n").split("\n")
    parts: list[Part] = []
    names: dict[str, int] = {}
    stack: list[tuple[int, Part]] = []
    top: Part | None = None

    def named(base: str) -> str:
        k = part_key(base)
        names[k] = names.get(k, 0) + 1
        return base if names[k] == 1 else f"{base} ({names[k]})"

    def owner() -> Part:
        nonlocal top
        if stack:
            return stack[-1][1]
        if top is None:
            top = Part(named(TOP), "heading", 1)
            parts.insert(0, top)
        return top

    def add(p: Part) -> None:
        if p.parent:
            p.parent.children.append(p)
        parts.append(p)

    fence = None
    in_item = False
    i = 0
    while i < len(lines):
        line = lines[i]
        n = i + 1
        if fence:
            if line.strip().startswith(fence):
                fence = None
            i += 1
            continue
        m = FENCE.match(line)
        if m:
            fence = m.group(1)
            owner().prose = True
            in_item = False
            i += 1
            continue
        if not line.strip():
            i += 1
            continue
        m = HEADING.match(line)
        if m:
            level, title = len(m.group(1)), m.group(2).strip()
            while stack and stack[-1][0] >= level:
                stack.pop()
            p = Part(named(title or f"(untitled, line {n})"), "heading", n, stack[-1][1] if stack else None)
            add(p)
            stack.append((level, p))
            in_item = False
            i += 1
            continue
        if line.lstrip().startswith("|"):
            block = []
            while i < len(lines) and lines[i].lstrip().startswith("|"):
                block.append((i + 1, lines[i]))
                i += 1
            if len(block) >= 2 and SEPARATOR.match(block[1][1].strip()):
                block = block[2:]
            head = owner()
            for ln, row in block:
                cell = next((c for c in _cells(row) if c), "")
                add(Part(named(f"{head.name} | {_short(cell, ROW_NAME)}"), "row", ln, head))
            in_item = False
            continue
        m = ITEM.match(line)
        if m:
            head = owner()
            add(Part(named(f"{head.name} / {m.group(1)}. {_short(m.group(2), ITEM_NAME)}"), "item", n, head))
            in_item = True
            i += 1
            continue
        if in_item and line[:1] in (" ", "\t"):
            i += 1  # the item's own continuation
            continue
        in_item = False
        owner().prose = True
        i += 1
    # What a heading says itself is a part of its own when it has others under
    # it, so an entry can account for it without covering the whole section.
    res: list[Part] = []
    for p in parts:
        res.append(p)
        if p.kind == "heading" and p.prose and p.children:
            t = Part(named(f"{p.name} / text"), "text", p.line, p)
            p.children.insert(0, t)
            res.append(t)
    return res


@dataclass
class Coverage:
    parts: list[Part]
    uncovered: list[Part]          # the parts an entry still has to name
    unknown: list[str]             # ledger entries that name no part


def coverage(parts: list[Part], ledger: list[dict]) -> Coverage:
    named = {part_key(e["part"]) for e in ledger if isinstance(e, dict) and isinstance(e.get("part"), str)}
    known = {part_key(p.name) for p in parts}
    memo: dict[int, bool] = {}

    def by_entry(p: Part | None) -> bool:
        while p is not None:
            if part_key(p.name) in named:
                return True
            p = p.parent
        return False

    def covered(p: Part) -> bool:
        if id(p) not in memo:
            if by_entry(p):
                memo[id(p)] = True
            elif p.kind == "heading" and p.children:
                memo[id(p)] = all(covered(c) for c in p.children)
            else:
                memo[id(p)] = p.kind == "heading" and not p.prose  # a heading with nothing under it
        return memo[id(p)]

    uncovered = [p for p in parts if not covered(p) and not (p.kind == "heading" and p.children)]
    unknown = [e["part"] for e in ledger if isinstance(e, dict) and isinstance(e.get("part"), str)
               and part_key(e["part"]) not in known]
    return Coverage(parts, uncovered, unknown)


def read_doc(cfg, path: str, rev: str | None = None) -> bytes | None:
    """The document as the working tree has it, else as the base commit does."""
    f = cfg.root / path
    if f.is_file():
        return f.read_bytes()
    return gitio.blob(cfg.root, rev or gitio.head(cfg.root), path)


def doc_text(data: bytes) -> str:
    return data.decode("utf-8-sig", "replace")


def ledger_errors(cfg, r: dict, old: dict | None, rev: str | None) -> list[str]:
    """What a source record may not say about its document: an entry this
    change adds that names no part of it (a typo would cover nothing), or
    `covered` while a part is unaccounted for."""
    rid, path = r.get("id"), r.get("path")
    if not isinstance(path, str) or "\\" in path or path.startswith("/") or re.match(r"^[A-Za-z]:", path):
        return [f"{rid}: `path` is the document's path from the repository root, with forward slashes"]
    data = read_doc(cfg, path, rev)
    if data is None:
        if r.get("status") == "covered":
            return [f"{rid}: {path} is neither in the working tree nor in HEAD, so `covered` cannot be "
                    "checked - a document that is gone is `history`"]
        return []
    ledger = [e for e in r.get("parts") or [] if isinstance(e, dict)]
    before = {part_key(e["part"]) for e in (old or {}).get("parts") or []
              if isinstance(e, dict) and isinstance(e.get("part"), str)}
    cov = coverage(parts_of(doc_text(data)), ledger)
    errs = [f"{rid}: part {u!r} is not in {path} - `lore cover {path}` lists its parts by name"
            for u in cov.unknown if part_key(u) not in before]
    if r.get("status") == "covered" and cov.uncovered:
        errs.append(f"{rid}: `covered`, but {len(cov.uncovered)} part(s) of {path} are unaccounted for, "
                    f"the first {cov.uncovered[0].name!r} - `lore cover {path}` lists them")
    return errs
