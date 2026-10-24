"""`lore.toml`: finding it, reading it, refusing what it may not say."""
from __future__ import annotations

import re
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

from codelore import LoreError

CONFIG_NAME = "lore.toml"
SLUG = re.compile(r"^[a-z0-9][a-z0-9-]{0,47}$")
MODES = ("none", "plain", "shared-tree")
PROVIDERS = ("ollama", "none")


@dataclass
class Config:
    root: Path                      # the repository root, where lore.toml is
    dir: str = "lore"               # relative to root, forward slashes
    areas: list[str] = field(default_factory=list)
    commit_mode: str = "plain"
    branch: str = "main"
    remote: str = "origin"
    push: bool = False
    trailers: list[str] = field(default_factory=list)
    embed_provider: str = "ollama"
    embed_url: str = "http://127.0.0.1:11434"
    embed_model: str = "nomic-embed-text"
    embed_timeout: float = 2.0
    doc_prefix: str | None = None
    query_prefix: str | None = None
    limits: dict[str, int] = field(default_factory=dict)
    dedupe_cosine: float = 0.95
    dedupe_jaccard: float = 0.5

    @property
    def lore_dir(self) -> Path:
        return self.root / self.dir

    def prefixes(self) -> tuple[str, str]:
        """nomic-embed-text is trained with task prefixes; other models get none
        unless the configuration names them."""
        nomic = "nomic" in self.embed_model
        doc = self.doc_prefix if self.doc_prefix is not None else (
            "search_document: " if nomic else "")
        query = self.query_prefix if self.query_prefix is not None else (
            "search_query: " if nomic else "")
        return doc, query


def find_root(start: Path) -> Path:
    here = start.resolve()
    for p in (here, *here.parents):
        if (p / CONFIG_NAME).is_file():
            return p
    raise LoreError(f"no {CONFIG_NAME} in {here} or above - run `lore init` "
                    "at the repository root")


def load(start: Path) -> Config:
    root = find_root(start)
    path = root / CONFIG_NAME
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (tomllib.TOMLDecodeError, UnicodeDecodeError) as e:
        raise LoreError(f"{path}: not valid TOML: {e}") from e
    return parse(data, root, str(path))


def parse(data: dict, root: Path, where: str = CONFIG_NAME) -> Config:
    bad: list[str] = []
    known_top = {"version", "dir", "areas", "commit", "embed", "limits", "dedupe"}
    for k in data:
        if k not in known_top:
            bad.append(f"unknown key `{k}`")
    if data.get("version") != 1:
        bad.append("`version` must be 1")
    cfg = Config(root=root)

    d = data.get("dir", "lore")
    if not isinstance(d, str) or not d or d.startswith("/") or ".." in Path(d).parts:
        bad.append("`dir` must be a relative path inside the repository")
    else:
        cfg.dir = d.replace("\\", "/").strip("/")

    areas = data.get("areas", [])
    if not isinstance(areas, list) or not all(isinstance(a, str) for a in areas):
        bad.append("`areas` must be a list of strings")
    else:
        for a in areas:
            if not SLUG.match(a):
                bad.append(f"area `{a}` must match {SLUG.pattern}")
            if a == "plans":
                bad.append("`plans` is reserved for plan files and is not an area")
        if len(set(areas)) != len(areas):
            bad.append("`areas` has duplicates")
        cfg.areas = list(areas)

    c = data.get("commit", {})
    if not isinstance(c, dict):
        bad.append("`[commit]` must be a table")
        c = {}
    for k in c:
        if k not in ("mode", "branch", "remote", "push", "trailers"):
            bad.append(f"unknown key `commit.{k}`")
    cfg.commit_mode = c.get("mode", cfg.commit_mode)
    if cfg.commit_mode not in MODES:
        bad.append(f"`commit.mode` must be one of {', '.join(MODES)}")
    cfg.branch = c.get("branch", cfg.branch)
    cfg.remote = c.get("remote", cfg.remote)
    cfg.push = c.get("push", cfg.push)
    if not isinstance(cfg.push, bool):
        bad.append("`commit.push` must be true or false")
    cfg.trailers = c.get("trailers", [])
    if not isinstance(cfg.trailers, list) or not all(isinstance(t, str) for t in cfg.trailers):
        bad.append("`commit.trailers` must be a list of strings")

    e = data.get("embed", {})
    if not isinstance(e, dict):
        bad.append("`[embed]` must be a table")
        e = {}
    for k in e:
        if k not in ("provider", "url", "model", "timeout", "doc_prefix", "query_prefix"):
            bad.append(f"unknown key `embed.{k}`")
    cfg.embed_provider = e.get("provider", cfg.embed_provider)
    if cfg.embed_provider not in PROVIDERS:
        bad.append(f"`embed.provider` must be one of {', '.join(PROVIDERS)}")
    cfg.embed_url = str(e.get("url", cfg.embed_url)).rstrip("/")
    cfg.embed_model = str(e.get("model", cfg.embed_model))
    cfg.embed_timeout = float(e.get("timeout", cfg.embed_timeout))
    cfg.doc_prefix = e.get("doc_prefix")
    cfg.query_prefix = e.get("query_prefix")

    lim = data.get("limits", {})
    if not isinstance(lim, dict) or not all(isinstance(v, int) and v > 0 for v in lim.values()):
        bad.append("`[limits]` must map `kind.field` to positive integers")
    else:
        cfg.limits = dict(lim)

    dd = data.get("dedupe", {})
    if isinstance(dd, dict):
        cfg.dedupe_cosine = float(dd.get("cosine", cfg.dedupe_cosine))
        cfg.dedupe_jaccard = float(dd.get("jaccard", cfg.dedupe_jaccard))
    else:
        bad.append("`[dedupe]` must be a table")

    if bad:
        raise LoreError(f"{where} is refused", bad)
    return cfg


TEMPLATE = """version = 1
dir = "{dir}"
areas = [{areas}]

[commit]
mode = "{mode}"          # "none" | "plain" | "shared-tree"
branch = "{branch}"
push = false

[embed]
provider = "ollama"     # "ollama" | "none"
url = "http://127.0.0.1:11434"
model = "nomic-embed-text"
"""
