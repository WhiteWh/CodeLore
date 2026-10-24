"""The JSONL files: canonical lines, sorted, LF. A file that is not exactly that
is refused by name and line - the store never repairs what it reads."""
from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from codelore import LoreError
from codelore.schema import ITEM_ID, PLAN_KINDS, item_number


def canonical(r: dict) -> str:
    return json.dumps(r, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def sort_key(r: dict):
    """Knowledge sorts by id. In a plan file the plan record comes first and
    its items follow by number, so #10 comes after #9."""
    if r.get("kind") == "plan":
        return (0, 0, r.get("id", ""))
    if r.get("kind") == "item":
        return (1, item_number(r.get("id", "")), r.get("id", ""))
    return (0, 0, r.get("id", ""))


def serialize(records: list[dict]) -> bytes:
    if not records:
        return b""
    lines = [canonical(r) for r in sorted(records, key=sort_key)]
    return ("\n".join(lines) + "\n").encode("utf-8")


def parse(data: bytes, where: str) -> list[dict]:
    """Records of one file, or a refusal naming every way it is not canonical."""
    errs: list[str] = []
    if data.startswith(b"\xef\xbb\xbf"):
        raise LoreError(f"{where}: starts with a BOM; lore files are UTF-8 without one")
    if b"\r" in data:
        raise LoreError(f"{where}: has CR line ends; lore files are LF - add "
                        "`<dir>/** text eol=lf` to .gitattributes and check the file out again")
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as e:
        raise LoreError(f"{where}: not UTF-8 ({e})") from e
    if not text:
        return []
    if not text.endswith("\n"):
        errs.append("no final newline")
    records: list[dict] = []
    for n, line in enumerate(text.split("\n")[:-1] if text.endswith("\n") else text.split("\n"), 1):
        if not line.strip():
            errs.append(f"line {n}: blank")
            continue
        try:
            r = json.loads(line)
        except json.JSONDecodeError as e:
            errs.append(f"line {n}: not JSON ({e.msg})")
            continue
        if not isinstance(r, dict):
            errs.append(f"line {n}: not a JSON object")
            continue
        if canonical(r) != line:
            errs.append(f"line {n}: not canonical (keys sorted, no spaces, non-ASCII as is) - "
                        "the file was edited by hand; `lore fmt` rewrites it")
        records.append(r)
    keys = [sort_key(r) for r in records]
    for i in range(1, len(keys)):
        if keys[i] < keys[i - 1]:
            errs.append(f"line {i + 1}: out of order ({records[i].get('id')!r} after {records[i - 1].get('id')!r})")
        elif keys[i] == keys[i - 1]:
            errs.append(f"line {i + 1}: id {records[i].get('id')!r} twice")
    if errs:
        raise LoreError(f"{where} is refused", errs)
    return records


def path_of(r: dict, lore_dir: str) -> str:
    """The repository-relative path the record lives in."""
    if r.get("kind") == "plan":
        return f"{lore_dir}/plans/{r['id']}.jsonl"
    if r.get("kind") == "item":
        m = ITEM_ID.match(r.get("id", ""))
        plan = m.group(1) if m else r.get("plan", "")
        return f"{lore_dir}/plans/{plan}.jsonl"
    return f"{lore_dir}/{r['area']}.jsonl"


def expected_path_ok(r: dict, rel: str, lore_dir: str) -> bool:
    return path_of(r, lore_dir) == rel


@dataclass
class Snapshot:
    """Every lore file at one point - the working tree or a commit - by
    repository-relative path."""
    files: dict[str, list[dict]]

    def records(self):
        for path, rs in self.files.items():
            for r in rs:
                yield path, r

    def by_id(self) -> dict[str, dict]:
        return {r["id"]: r for _, r in self.records() if isinstance(r.get("id"), str)}


def is_lore_file(rel: str, lore_dir: str) -> bool:
    if not rel.endswith(".jsonl") or not rel.startswith(lore_dir + "/"):
        return False
    rest = rel[len(lore_dir) + 1:]
    return "/" not in rest or (rest.startswith("plans/") and rest.count("/") == 1)


def load_worktree(root: Path, lore_dir: str) -> Snapshot:
    base = root / lore_dir
    files: dict[str, list[dict]] = {}
    if base.is_dir():
        paths = list(base.glob("*.jsonl")) + list((base / "plans").glob("*.jsonl"))
        for p in sorted(paths):
            rel = p.relative_to(root).as_posix()
            files[rel] = parse(p.read_bytes(), rel)
    return Snapshot(files)


def check_placement(snap: Snapshot, lore_dir: str) -> list[str]:
    """Each record is in the file its kind and area name; ids are unique."""
    errs: list[str] = []
    seen: dict[str, str] = {}
    for rel, r in snap.records():
        rid = r.get("id")
        if isinstance(rid, str):
            if rid in seen:
                errs.append(f"id {rid!r} is in {seen[rid]} and in {rel}")
            seen[rid] = rel
        if r.get("kind") in PLAN_KINDS or isinstance(r.get("area"), str):
            try:
                want = path_of(r, lore_dir)
            except KeyError:
                continue
            if want != rel:
                errs.append(f"{rid!r} is in {rel}, its kind and area put it in {want}")
    for rel, rs in snap.files.items():
        if rel.startswith(f"{lore_dir}/plans/"):
            plans = [r for r in rs if r.get("kind") == "plan"]
            if len(plans) != 1:
                errs.append(f"{rel} must hold exactly one plan record, holds {len(plans)}")
    return errs
