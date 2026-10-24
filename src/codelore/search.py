"""find and for. Exact signals first, then reciprocal rank fusion of three
lists: words (BM25), whole terms (BM25), and meaning (cosine)."""
from __future__ import annotations

import re
from dataclasses import dataclass, field

from codelore import embed as embedmod
from codelore.index import WORD, Index, terms_of
from codelore.schema import RETIRED

RRF_K = 60
DEPTH = 50
#: BM25 weights for the word table's columns: signals, title, main, body.
WEIGHTS = (10.0, 5.0, 3.0, 1.0)


@dataclass
class Hit:
    record: dict
    score: float
    via: list[str] = field(default_factory=list)


@dataclass
class Filters:
    kind: str | None = None
    area: str | None = None
    status: str | None = None
    plan: str | None = None
    all: bool = False


def _quote(tok: str) -> str:
    return '"' + tok.replace('"', '""') + '"'


def _visible(idx: Index, f: Filters) -> dict[str, dict]:
    """The records a search may return, by id."""
    recs = {r["id"]: r for r in idx.all() if isinstance(r.get("id"), str)}
    closed_plans = {rid for rid, r in recs.items() if r.get("kind") == "plan" and r.get("status") == "closed"}
    res = {}
    for rid, r in recs.items():
        if f.kind and r.get("kind") != f.kind:
            continue
        if f.area and r.get("area") != f.area:
            continue
        if f.plan and r.get("plan") != f.plan and rid != f.plan:
            continue
        if f.status:
            if r.get("status") != f.status:
                continue
        elif not f.all:
            if r.get("status") in RETIRED:
                continue
            if r.get("kind") == "item" and r.get("plan") in closed_plans:
                continue
        res[rid] = r
    return res


def _fts(idx: Index, table: str, match: str, weights: tuple | None) -> list[str]:
    if not match:
        return []
    rank = f"bm25({table}, 0, {', '.join(str(w) for w in weights)})" if weights else f"bm25({table})"
    sql = f"SELECT id FROM {table} WHERE {table} MATCH ? ORDER BY {rank} LIMIT ?"
    try:
        return [r[0] for r in idx.db.execute(sql, (match, DEPTH * 4))]
    except Exception:
        return []


def word_query(text: str) -> str:
    toks = []
    for t in WORD.findall(text):
        t = t.lower()
        if t not in toks:
            toks.append(t)
    return " OR ".join(_quote(t) for t in toks[:64])


def term_query(text: str) -> str:
    return " OR ".join(_quote(t) for t in terms_of(text)[:32])


def signal_hits(query: str, visible: dict[str, dict]) -> list[tuple[str, int]]:
    """Records one of whose exact strings is in the query, or which hold the
    query in one of them. Longest match first."""
    q = query.strip()
    ql = q.lower()
    hits = []
    for rid, r in visible.items():
        best = 0
        for s in r.get("signals") or []:
            sl = s.lower()
            if (len(s) >= 6 and sl in ql) or (len(q) >= 8 and ql in sl):
                best = max(best, len(s))
        if best:
            hits.append((rid, best))
    hits.sort(key=lambda x: -x[1])
    return hits


#: A query shorter than this is a few words, not a phrase someone copied.
PHRASE_MIN = 16
_MARKUP = re.compile(r"[`*«»\"']")
_SPACE = re.compile(r"\s+")


def flat(text: str) -> str:
    return _SPACE.sub(" ", _MARKUP.sub("", text.lower())).strip()


def phrase_hits(query: str, visible: dict[str, dict]) -> list[str]:
    """Records whose own text holds the whole query, word for word - what a
    fixed-string grep finds. A bag of common words ranks a long record that
    has many of them above the short one that says exactly this; the phrase
    puts that one back."""
    q = flat(query)
    if len(q) < PHRASE_MIN:
        return []
    hits = []
    for rid, r in visible.items():
        text = flat(" ".join(str(v) for k, v in r.items()
                             if isinstance(v, str) and k not in ("id", "kind", "area", "status", "by")))
        if q in text:
            hits.append(rid)
    return hits


def find(idx: Index, query: str, f: Filters, limit: int = 5, prov=None,
         progress=None) -> tuple[list[Hit], list[str]]:
    """The best records for the query, and notes about how they were found."""
    notes: list[str] = []
    visible = _visible(idx, f)
    lists: dict[str, list[str]] = {}
    lists["words"] = [i for i in _fts(idx, "words", word_query(query), WEIGHTS) if i in visible][:DEPTH]
    lists["terms"] = [i for i in _fts(idx, "terms", term_query(query), None) if i in visible][:DEPTH]

    if prov is not None:
        if idx.ensure_vectors(prov, progress):
            _, qprefix = idx.cfg.prefixes()
            try:
                qv = prov.embed([qprefix + query[:4000]])[0]
                vecs = idx.vectors(prov.model)
                scored = sorted(((embedmod.dot(qv, v), rid) for rid, v in vecs.items() if rid in visible),
                                reverse=True)
                lists["meaning"] = [rid for _, rid in scored[:DEPTH]]
            except embedmod.Unavailable as e:
                notes.append(f"semantic search unavailable: {e}; text only")
        else:
            notes.append(idx.semantic_note or "text only")

    scores: dict[str, float] = {}
    via: dict[str, list[str]] = {}
    for name, ids in lists.items():
        for rank, rid in enumerate(ids):
            scores[rid] = scores.get(rid, 0.0) + 1.0 / (RRF_K + rank + 1)
            via.setdefault(rid, []).append(name)
    for rid in phrase_hits(query, visible):
        scores[rid] = scores.get(rid, 0.0) + 0.5
        via.setdefault(rid, []).insert(0, "phrase")
    for rid, n in signal_hits(query, visible):
        scores[rid] = scores.get(rid, 0.0) + 1.0 + n / 1000.0
        via.setdefault(rid, []).insert(0, "signal")
    ranked = sorted(scores.items(), key=lambda x: -x[1])[:limit]
    return [Hit(visible[rid], round(s, 5), via[rid]) for rid, s in ranked], notes


ERRORISH = re.compile(
    r"(error|exception|fatal|failed|failure|refused|denied|cannot|can't|unable|not found|"
    r"HRESULT|0x[0-9a-f]{8}|exit (code|status)|segfault|assert|panic|warning:)", re.I)
NOISE = re.compile(r'^\s*(File "|at |Traceback \(most recent call last\)|\^+\s*$|~+\s*$)')


def errors_of(text: str) -> str:
    """The lines of a pasted log or traceback worth searching for."""
    keep = []
    for line in text.splitlines():
        s = line.strip()
        if not s or NOISE.match(line):
            continue
        if ERRORISH.search(s) and s not in keep:
            keep.append(s[:400])
    if not keep:
        return text.strip()[-2000:]
    return "\n".join(keep[-12:])


def for_target(idx: Index, target: str, f: Filters) -> list[dict]:
    """Every visible record whose refs name the file, a file under the
    directory, or the symbol."""
    t = target.replace("\\", "/").strip()
    root = str(idx.cfg.root).replace("\\", "/").rstrip("/") + "/"
    if t.lower().startswith(root.lower()):
        t = t[len(root):]
    t = t.removeprefix("./")
    d = t.rstrip("/") + "/"
    res = []
    for r in _visible(idx, f).values():
        refs = r.get("refs") or {}
        hit = False
        for p in refs.get("files", []):
            if p == t or p.startswith(d) or t.startswith(p.rstrip("/") + "/"):
                hit = True
        for s in refs.get("symbols", []):
            if s == t or s.startswith(t + ".") or t.startswith(s + ".") or s.endswith("." + t):
                hit = True
        if hit:
            res.append(r)
    res.sort(key=lambda r: (r.get("kind", ""), r.get("id", "")))
    return res
