"""The duplicate search before a new record.

Two answers, because the evidence is of two strengths. On the first real lore
(37 records about git in a shared tree) embedding similarity did not separate
duplicates from neighbours: rows that were the same fault scored 0.69-0.85,
three different faults 0.87-0.88, and the median pair 0.69. So similarity
alone never refuses a write; it names the nearest records so the writer can
look. A write is refused only on strong evidence - an exact signal another
record already carries, near-identical text (cosine >= `dedupe.cosine`,
0.95), or a word overlap (>= `dedupe.jaccard`, 0.5) no two different records
reached (their highest was 0.32).
"""
from __future__ import annotations

import re
from dataclasses import dataclass

from codelore import embed as embedmod
from codelore.index import Index, embed_text
from codelore.schema import KNOWLEDGE, main_text

STOP = set("""a an the and or of to in on at for from by with without is are was were be been
it its this that these those as not no into than then when while which who what why how do does
did can cannot could should would will just only also there here out up so if but all any each
""".split())

#: Below this a neighbour is not worth naming.
NEAR_COSINE = 0.80
NEAR_JACCARD = 0.25


def words(r: dict) -> set[str]:
    text = " ".join([r.get("title", ""), main_text(r), r.get("cause", "")]).lower()
    return {w for w in re.findall(r"[a-z0-9_]{3,}", text) if w not in STOP}


def jaccard(a: set[str], b: set[str]) -> float:
    if not a or not b:
        return 0.0
    return len(a & b) / len(a | b)


@dataclass
class Match:
    id: str
    similarity: float
    how: str           # "signal", "meaning", "words"
    blocking: bool


def candidates(cfg, idx: Index, new: list[dict], prov=None) -> dict[int, list[Match]]:
    """For each new knowledge record (by position), the existing or earlier
    batch records that look like it, strongest first."""
    existing = [r for r in idx.all() if r.get("kind") in KNOWLEDGE]
    use_vec = prov is not None and idx.ensure_vectors(prov)
    vecs = idx.vectors(prov.model) if use_vec else {}
    new_vecs = []
    if use_vec:
        doc_prefix, _ = cfg.prefixes()
        try:
            new_vecs = prov.embed([doc_prefix + embed_text(r) for r in new])
        except embedmod.Unavailable:
            use_vec = False
    res: dict[int, list[Match]] = {}
    for i, r in enumerate(new):
        if r.get("kind") not in KNOWLEDGE:
            continue
        w = words(r)
        sigs = {s.lower() for s in r.get("signals") or []}
        pool = [(e, vecs.get(e["id"])) for e in existing if e.get("kind") == r.get("kind")]
        pool += [(new[j], new_vecs[j] if use_vec else None) for j in range(i)
                 if new[j].get("kind") == r.get("kind")]
        found: list[Match] = []
        for e, ev in pool:
            eid = e.get("id") or e.get("title", "?")
            shared = sigs & {s.lower() for s in e.get("signals") or []}
            if shared:
                found.append(Match(eid, 1.0, "signal", True))
                continue
            j = jaccard(w, words(e))
            cos = embedmod.dot(new_vecs[i], ev) if use_vec and ev is not None else None
            if cos is not None and cos >= cfg.dedupe_cosine:
                found.append(Match(eid, round(cos, 3), "meaning", True))
            elif j >= cfg.dedupe_jaccard:
                found.append(Match(eid, round(j, 3), "words", True))
            elif cos is not None and cos >= NEAR_COSINE:
                found.append(Match(eid, round(cos, 3), "meaning", False))
            elif cos is None and j >= NEAR_JACCARD:
                found.append(Match(eid, round(j, 3), "words", False))
        if found:
            found.sort(key=lambda m: (not m.blocking, -m.similarity))
            res[i] = found[:3]
    return res
