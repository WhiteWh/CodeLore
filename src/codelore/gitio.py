"""git, through its plumbing. The three commit modes live here.

`shared-tree` is for a working tree several sessions edit at once: the change is
applied to each file's blob at HEAD, never to the live file, the tree is built in
a private index, and the branch moves only if nobody moved it in between.
"""
from __future__ import annotations

import os
import subprocess
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

from codelore import LoreError


def git(root: Path, *args: str, input: bytes | None = None, env: dict | None = None,
        check: bool = True) -> subprocess.CompletedProcess:
    full_env = None
    if env:
        full_env = {**os.environ, **env}
    p = subprocess.run(["git", "-C", str(root), *args], input=input, capture_output=True,
                       env=full_env)
    if check and p.returncode != 0:
        msg = p.stderr.decode("utf-8", "replace").strip() or p.stdout.decode("utf-8", "replace").strip()
        raise LoreError(f"git {' '.join(args[:3])} failed: {msg}")
    return p


def out(root: Path, *args: str, **kw) -> str:
    return git(root, *args, **kw).stdout.decode("utf-8", "replace").strip()


def toplevel(start: Path) -> Path | None:
    p = subprocess.run(["git", "-C", str(start), "rev-parse", "--show-toplevel"],
                       capture_output=True)
    if p.returncode != 0:
        return None
    return Path(p.stdout.decode().strip())


def head(root: Path) -> str | None:
    p = git(root, "rev-parse", "--verify", "-q", "HEAD^{commit}", check=False)
    return p.stdout.decode().strip() or None


def ref(root: Path, name: str) -> str | None:
    p = git(root, "rev-parse", "--verify", "-q", f"{name}^{{commit}}", check=False)
    return p.stdout.decode().strip() or None


def blob(root: Path, rev: str | None, path: str) -> bytes | None:
    if rev is None:
        return None
    p = git(root, "cat-file", "blob", f"{rev}:{path}", check=False)
    return p.stdout if p.returncode == 0 else None


def ls_tree(root: Path, rev: str | None, directory: str) -> list[str]:
    if rev is None:
        return []
    p = git(root, "ls-tree", "-r", "--name-only", rev, "--", directory + "/", check=False)
    return [l for l in p.stdout.decode("utf-8", "replace").splitlines() if l]


def commit_exists(root: Path, sha: str) -> bool:
    return git(root, "cat-file", "-e", f"{sha}^{{commit}}", check=False).returncode == 0


def file_at(root: Path, rev: str | None, path: str) -> bool:
    if rev is None:
        return False
    return git(root, "cat-file", "-e", f"{rev}:{path}", check=False).returncode == 0


def user_name(root: Path) -> str:
    return out(root, "config", "user.name", check=False)


def current_branch(root: Path) -> str | None:
    p = git(root, "symbolic-ref", "-q", "HEAD", check=False)
    s = p.stdout.decode().strip()
    return s[len("refs/heads/"):] if s.startswith("refs/heads/") else None


def touched_since(root: Path, since: str, paths: list[str]) -> bool:
    if not paths:
        return False
    p = git(root, "log", "-1", "--format=%H", f"--since={since}", "--", *paths, check=False)
    return bool(p.stdout.strip())


@dataclass
class Result:
    commit: str | None = None
    notes: list[str] = field(default_factory=list)


#: Given the files' old bytes and the revision they were read at (None: the
#: working tree), the new bytes of every file that changes.
Transform = Callable[[dict[str, bytes | None], str | None], dict[str, bytes]]


def _message(subject: str, body: str, trailers: list[str]) -> bytes:
    text = subject.strip() + "\n"
    if body.strip():
        text += "\n" + body.strip() + "\n"
    if trailers:
        text += "\n" + "\n".join(trailers) + "\n"
    return text.encode("utf-8")


def write_none(root: Path, paths: list[str], transform: Transform) -> Result:
    old = {p: _read_live(root, p) for p in paths}
    new = transform(old, None)
    for p, data in new.items():
        _write_live(root, p, data)
    return Result(notes=["written to the working tree; not committed (commit.mode = none)"])


def write_plain(root: Path, paths: list[str], transform: Transform, subject: str,
                body: str, trailers: list[str]) -> Result:
    old = {p: _read_live(root, p) for p in paths}
    new = transform(old, None)
    if not new:
        return Result(notes=["nothing changed"])
    for p, data in new.items():
        _write_live(root, p, data)
    changed = list(new)
    git(root, "add", "--", *changed)
    msg = _message(subject, body, trailers)
    p = git(root, "commit", "--only", "-q", "-F", "-", "--", *changed,
            input=msg, check=False)
    if p.returncode != 0:
        err = (p.stderr or p.stdout).decode("utf-8", "replace").strip()
        raise LoreError(f"the commit failed; the change is in the working tree: {err}")
    return Result(commit=head(root))


def write_shared(root: Path, branch: str, paths: list[str], transform: Transform,
                 subject: str, body: str, trailers: list[str], tries: int = 3,
                 _before_update: Callable[[], None] | None = None) -> Result:
    """HEAD's blobs plus this change, in a private index, moved by compare-and-swap."""
    cur = current_branch(root)
    if cur != branch:
        raise LoreError(f"commit.mode is shared-tree on `{branch}`, but HEAD is "
                        f"{'detached' if cur is None else '`' + cur + '`'} - refusing to move a branch "
                        "the working tree is not on")
    notes: list[str] = []
    msg = _message(subject, body, trailers)
    last_err = ""
    for attempt in range(tries):
        base = ref(root, f"refs/heads/{branch}")
        old = {p: blob(root, base, p) for p in paths}
        new = transform(old, base)
        if not new:
            return Result(notes=["nothing changed"])
        fd, idx = tempfile.mkstemp(prefix="lore-index-")
        os.close(fd)
        os.unlink(idx)
        env = {"GIT_INDEX_FILE": idx}
        try:
            if base:
                git(root, "read-tree", base, env=env)
            for p, data in new.items():
                sha = out(root, "hash-object", "-w", "--no-filters", "--stdin", input=data)
                git(root, "update-index", "--add", "--cacheinfo", f"100644,{sha},{p}", env=env)
            tree = out(root, "write-tree", env=env)
        finally:
            if os.path.exists(idx):
                os.unlink(idx)
        args = ["commit-tree", tree]
        if base:
            args += ["-p", base]
        commit = out(root, *args, "-F", "-", input=msg)
        if _before_update:
            _before_update()
            _before_update = None
        upd = git(root, "update-ref", f"refs/heads/{branch}", commit, base or "0" * 40, check=False)
        if upd.returncode == 0:
            notes += _sync_live(root, old, new)
            return Result(commit=commit, notes=notes)
        last_err = upd.stderr.decode("utf-8", "replace").strip()
        notes.append(f"`{branch}` moved while writing (try {attempt + 1}); built again on the new head")
    raise LoreError(f"`{branch}` kept moving; nothing was committed after {tries} tries: {last_err}")


def _sync_live(root: Path, old: dict[str, bytes | None], new: dict[str, bytes]) -> list[str]:
    """Bring each live file, and its entry in the shared index, to the new blob -
    only where they still held the old one. Anything else is somebody's edit."""
    notes = []
    for p, data in new.items():
        live = _read_live(root, p)
        before = old.get(p)
        same = (live is None and before is None) or (
            live is not None and before is not None and live.replace(b"\r\n", b"\n") == before)
        if same:
            _write_live(root, p, data)
            _refresh_index_entry(root, p, before, data)
        else:
            notes.append(f"{p}: the working tree holds an edit that is not in HEAD; left as it was - "
                         "the commit has the change, the live file does not")
    return notes


def _refresh_index_entry(root: Path, path: str, before: bytes | None, data: bytes) -> None:
    p = git(root, "ls-files", "-s", "--", path, check=False)
    line = p.stdout.decode().strip()
    old_sha = None
    if before is not None:
        old_sha = out(root, "hash-object", "--no-filters", "--stdin", input=before)
    staged = line.split()[1] if line else None
    if staged != old_sha:
        return  # somebody staged something there, or the path is unknown in a way we did not cause
    new_sha = out(root, "hash-object", "--no-filters", "--stdin", input=data)
    git(root, "update-index", "--add", "--cacheinfo", f"100644,{new_sha},{path}", check=False)


def push(root: Path, remote: str, branch: str) -> str | None:
    p = git(root, "push", "-q", remote, branch, check=False)
    if p.returncode != 0:
        return (p.stderr or p.stdout).decode("utf-8", "replace").strip()
    return None


def _read_live(root: Path, rel: str) -> bytes | None:
    f = root / rel
    return f.read_bytes() if f.is_file() else None


def _write_live(root: Path, rel: str, data: bytes) -> None:
    f = root / rel
    if not data:
        if f.exists():
            f.unlink()
        return
    f.parent.mkdir(parents=True, exist_ok=True)
    tmp = f.with_name(f.name + ".lore-tmp")
    tmp.write_bytes(data)
    os.replace(tmp, f)
