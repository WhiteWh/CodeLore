"""Reading plans: counts, layers, what is ready, what nobody has touched."""
from __future__ import annotations

from datetime import date, timedelta

from codelore import gitio
from codelore.schema import UNDECIDED, item_number

STATUSES = ("open", "deferred", "done", "dropped", "superseded")


def items_of(records: list[dict], plan_id: str) -> list[dict]:
    return sorted((r for r in records if r.get("kind") == "item" and r.get("plan") == plan_id),
                  key=lambda r: item_number(r["id"]))


def summary(records: list[dict], include_closed: bool = False) -> list[dict]:
    res = []
    for p in sorted((r for r in records if r.get("kind") == "plan"), key=lambda r: r["id"]):
        if p.get("status") == "closed" and not include_closed:
            continue
        counts = {s: 0 for s in STATUSES}
        for i in items_of(records, p["id"]):
            counts[i.get("status")] = counts.get(i.get("status"), 0) + 1
        res.append({"plan": p, "counts": counts})
    return res


def layers(items: list[dict]) -> list[list[dict]]:
    """Depth by the longest path from the roots, over this plan's own items;
    a back edge (a cycle `lore check` already refuses) is ignored."""
    by_id = {i["id"]: i for i in items}
    depth: dict[str, int] = {}
    visiting: set[str] = set()

    def d(rid: str) -> int:
        if rid in depth:
            return depth[rid]
        if rid in visiting:
            return 0
        visiting.add(rid)
        best = 0
        for dep in by_id[rid].get("deps") or []:
            if dep.get("item") in by_id:
                best = max(best, d(dep["item"]) + 1)
        visiting.discard(rid)
        depth[rid] = best
        return best

    for rid in by_id:
        d(rid)
    out: list[list[dict]] = []
    for rid in sorted(by_id, key=lambda x: (depth[x], item_number(x))):
        while len(out) <= depth[rid]:
            out.append([])
        out[depth[rid]].append(by_id[rid])
    return out


def ready(item: dict, by_id: dict[str, dict]) -> bool:
    """Open, and every hard dependency done. A soft dependency only orders:
    whether it has started is the runner's knowledge, not the record's."""
    if item.get("status") != "open":
        return False
    for dep in item.get("deps") or []:
        if dep.get("kind") == "hard" and by_id.get(dep.get("item"), {}).get("status") != "done":
            return False
    return True


def stale(root, records: list[dict], days: int, today: date | None = None) -> list[dict]:
    """Undecided items whose record has not changed in `days` and whose files
    no commit has touched since it last changed."""
    today = today or date.today()
    cutoff = (today - timedelta(days=days)).isoformat()
    closed = {r["id"] for r in records if r.get("kind") == "plan" and r.get("status") == "closed"}
    res = []
    for r in records:
        if r.get("kind") != "item" or r.get("status") not in UNDECIDED or r.get("plan") in closed:
            continue
        if r.get("updated", "") > cutoff:
            continue
        files = (r.get("refs") or {}).get("files", [])
        if files and gitio.touched_since(root, r["updated"], files):
            continue
        res.append(r)
    res.sort(key=lambda r: (r.get("updated", ""), r.get("plan", ""), item_number(r["id"])))
    return res
