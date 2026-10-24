"""The disposable index: SQLite in the user cache, rebuilt from the JSONL when a
file changes. Deleting it costs one rebuild and nothing else."""
from __future__ import annotations

import hashlib
import json
import os
import re
import sqlite3
import sys
from array import array
from pathlib import Path

from codelore import LoreError
from codelore import embed as embedmod
from codelore.schema import main_text
from codelore.store import parse

INDEX_VERSION = 3

#: A token that only means something whole: `scene.depth_buffer`,
#: `ERR_DEVICE_LOST`, `0x80070005`, `src/render/post.py`, `E_FAIL`.
TERM = re.compile(
    r"0x[0-9A-Fa-f]{4,}"
    r"|[A-Za-z0-9_]+(?:[./\\:-][A-Za-z0-9_]+)+"
    r"|[A-Za-z0-9]*_[A-Za-z0-9_]+"
    r"|\b[A-Z][A-Z0-9]{2,}\b"
)
WORD = re.compile(r"\w{2,}", re.UNICODE)


def cache_dir() -> Path:
    env = os.environ.get("LORE_CACHE")
    if env:
        return Path(env)
    if sys.platform == "win32":
        base = os.environ.get("LOCALAPPDATA") or str(Path.home() / "AppData" / "Local")
        return Path(base) / "codelore"
    if sys.platform == "darwin":
        return Path.home() / "Library" / "Caches" / "codelore"
    return Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache") / "codelore"


def repo_key(root: Path) -> str:
    s = str(root.resolve())
    if sys.platform == "win32":
        s = s.lower()
    return hashlib.sha1(s.encode("utf-8")).hexdigest()[:16]


def terms_of(text: str) -> list[str]:
    seen, outl = set(), []
    for m in TERM.finditer(text):
        t = m.group(0).replace("\\", "/").strip(".:-/")
        if len(t) >= 3 and t.lower() not in seen:
            seen.add(t.lower())
            outl.append(t)
    return outl


def _strings(v) -> list[str]:
    if isinstance(v, str):
        return [v]
    if isinstance(v, list):
        res = []
        for x in v:
            res.extend(_strings(x))
        return res
    if isinstance(v, dict):
        res = []
        for x in v.values():
            res.extend(_strings(x))
        return res
    return []


SKIP = {"v", "id", "kind", "area", "status", "created", "updated", "by", "title",
        "signals", "plan"}


def columns(r: dict) -> tuple[str, str, str, str, str]:
    signals = "\n".join(r.get("signals") or [])
    title = r.get("title", "")
    main = main_text(r)
    body_parts = []
    for k, v in sorted(r.items()):
        if k in SKIP or k == "refs":
            continue
        if isinstance(v, str) and v == main:
            continue
        body_parts.extend(_strings(v))
    refs = r.get("refs") or {}
    body_parts.extend(refs.get("symbols", []))
    body = "\n".join(body_parts)
    all_text = "\n".join([signals, title, main, body, *refs.get("files", [])])
    terms = " ".join(terms_of(all_text))
    return signals, title, main, body, terms


def embed_text(r: dict) -> str:
    parts = [r.get("title", ""), main_text(r)]
    if r.get("cause"):
        parts.append(r["cause"])
    if r.get("signals"):
        parts.append("; ".join(r["signals"]))
    return "\n".join(p for p in parts if p)


class Index:
    def __init__(self, cfg):
        self.cfg = cfg
        d = cache_dir() / repo_key(cfg.root)
        d.mkdir(parents=True, exist_ok=True)
        self.path = d / "index.sqlite"
        self.db = self._open()
        self.semantic_note: str | None = None

    def _open(self) -> sqlite3.Connection:
        db = sqlite3.connect(self.path)
        ver = db.execute("PRAGMA user_version").fetchone()[0]
        if ver != INDEX_VERSION:
            db.close()
            for suffix in ("", "-wal", "-shm", "-journal"):
                p = Path(str(self.path) + suffix)
                if p.exists():
                    p.unlink()
            db = sqlite3.connect(self.path)
            self._create(db)
        return db

    @staticmethod
    def _create(db: sqlite3.Connection) -> None:
        db.executescript(f"""
            CREATE TABLE files(path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, sha TEXT);
            CREATE TABLE records(id TEXT PRIMARY KEY, path TEXT, kind TEXT, area TEXT,
                status TEXT, plan TEXT, title TEXT, updated TEXT, ehash TEXT, json TEXT);
            CREATE INDEX records_path ON records(path);
            CREATE VIRTUAL TABLE words USING fts5(id UNINDEXED, signals, title, main, body,
                tokenize = "porter unicode61 remove_diacritics 2 tokenchars '_'");
            CREATE VIRTUAL TABLE terms USING fts5(id UNINDEXED, toks,
                tokenize = "unicode61 remove_diacritics 0 tokenchars '_./:-'");
            CREATE TABLE vectors(model TEXT, hash TEXT, vec BLOB, PRIMARY KEY(model, hash));
            PRAGMA user_version = {INDEX_VERSION};
        """)
        db.commit()

    # -- freshness -------------------------------------------------------------

    def refresh(self) -> None:
        root, d = self.cfg.root, self.cfg.lore_dir
        found: dict[str, Path] = {}
        if d.is_dir():
            for p in list(d.glob("*.jsonl")) + list((d / "plans").glob("*.jsonl")):
                found[p.relative_to(root).as_posix()] = p
        known = {row[0]: row[1:] for row in self.db.execute("SELECT path, size, mtime, sha FROM files")}
        with self.db:
            for rel in set(known) - set(found):
                self._drop_file(rel)
            for rel, p in found.items():
                st = p.stat()
                k = known.get(rel)
                if k and k[0] == st.st_size and k[1] == st.st_mtime_ns:
                    continue
                data = p.read_bytes()
                sha = hashlib.sha1(data).hexdigest()
                if k and k[2] == sha:
                    self.db.execute("UPDATE files SET size=?, mtime=? WHERE path=?",
                                    (st.st_size, st.st_mtime_ns, rel))
                    continue
                records = parse(data, rel)
                self._drop_file(rel)
                for r in records:
                    self._put(rel, r)
                self.db.execute("INSERT OR REPLACE INTO files VALUES(?,?,?,?)",
                                (rel, st.st_size, st.st_mtime_ns, sha))

    def _drop_file(self, rel: str) -> None:
        ids = [r[0] for r in self.db.execute("SELECT id FROM records WHERE path=?", (rel,))]
        for rid in ids:
            self.db.execute("DELETE FROM words WHERE id=?", (rid,))
            self.db.execute("DELETE FROM terms WHERE id=?", (rid,))
        self.db.execute("DELETE FROM records WHERE path=?", (rel,))
        self.db.execute("DELETE FROM files WHERE path=?", (rel,))

    def _put(self, rel: str, r: dict) -> None:
        rid = r.get("id")
        if not isinstance(rid, str):
            return
        etext = embed_text(r)
        ehash = hashlib.sha1(etext.encode("utf-8")).hexdigest()
        self.db.execute("INSERT OR REPLACE INTO records VALUES(?,?,?,?,?,?,?,?,?,?)",
                        (rid, rel, r.get("kind"), r.get("area"), r.get("status"), r.get("plan"),
                         r.get("title", ""), r.get("updated", ""), ehash,
                         json.dumps(r, ensure_ascii=False)))
        signals, title, main, body, terms = columns(r)
        self.db.execute("INSERT INTO words(id, signals, title, main, body) VALUES(?,?,?,?,?)",
                        (rid, signals, title, main, body))
        self.db.execute("INSERT INTO terms(id, toks) VALUES(?,?)", (rid, terms))

    # -- records ---------------------------------------------------------------

    def all(self) -> list[dict]:
        return [json.loads(j) for (j,) in self.db.execute("SELECT json FROM records")]

    def get(self, rid: str) -> dict | None:
        row = self.db.execute("SELECT json FROM records WHERE id=?", (rid,)).fetchone()
        return json.loads(row[0]) if row else None

    def count(self) -> int:
        return self.db.execute("SELECT count(*) FROM records").fetchone()[0]

    # -- vectors ---------------------------------------------------------------

    def ensure_vectors(self, prov, progress=None) -> bool:
        """Embed every record that has no vector under this model. False, with
        `semantic_note` set, when the provider is off or does not answer."""
        if prov.name == "none":
            self.semantic_note = "semantic search is off (embed.provider = \"none\"); text only"
            return False
        model = prov.model
        have = {h for (h,) in self.db.execute("SELECT hash FROM vectors WHERE model=?", (model,))}
        todo = [(json.loads(j), h) for j, h in self.db.execute("SELECT json, ehash FROM records")
                if h not in have]
        seen, uniq = set(), []
        for r, h in todo:
            if h not in seen:
                seen.add(h)
                uniq.append((r, h))
        if uniq:
            doc_prefix, _ = self.cfg.prefixes()
            texts = [doc_prefix + embed_text(r) for r, _ in uniq]
            if progress and len(texts) > 32:
                progress(f"embedding {len(texts)} record(s) with {model}...")
            try:
                vecs = prov.embed(texts)
            except embedmod.Unavailable as e:
                self.semantic_note = f"semantic search unavailable: {e}; text only"
                return False
            with self.db:
                for (_, h), v in zip(uniq, vecs):
                    self.db.execute("INSERT OR REPLACE INTO vectors VALUES(?,?,?)",
                                    (model, h, v.tobytes()))
        return True

    def vectors(self, model: str) -> dict[str, array]:
        res: dict[str, array] = {}
        for rid, blob in self.db.execute(
                "SELECT r.id, v.vec FROM records r JOIN vectors v ON v.hash = r.ehash AND v.model = ?",
                (model,)):
            a = array("f")
            a.frombytes(blob)
            res[rid] = a
        return res

    def close(self) -> None:
        self.db.close()


def open_index(cfg) -> Index:
    try:
        idx = Index(cfg)
    except sqlite3.OperationalError as e:
        if "fts5" in str(e).lower():
            raise LoreError("this Python's SQLite has no FTS5; lore needs a Python built with it") from e
        raise
    idx.refresh()
    return idx
