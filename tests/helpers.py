"""Scratch repositories and a CLI runner for the tests."""
from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from codelore import cli  # noqa: E402


def git(root: Path, *args: str, input: bytes | None = None) -> str:
    p = subprocess.run(["git", "-C", str(root), *args], input=input, capture_output=True)
    if p.returncode != 0:
        raise AssertionError(f"git {args}: {p.stderr.decode()}")
    return p.stdout.decode().strip()


CONFIG = """version = 1
dir = "lore"
areas = ["git", "build", "render"]

[commit]
mode = "{mode}"
branch = "main"

[embed]
provider = "none"
"""


class RepoCase(unittest.TestCase):
    mode = "plain"

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="lore-test-"))
        self.root = self.tmp / "repo"
        self.root.mkdir()
        os.environ["LORE_CACHE"] = str(self.tmp / "cache")
        os.environ.pop("LORE_BY", None)
        git(self.root, "init", "-q", "-b", "main")
        git(self.root, "config", "user.name", "tester")
        git(self.root, "config", "user.email", "t@example.com")
        git(self.root, "config", "core.autocrlf", "false")
        (self.root / "src").mkdir()
        (self.root / "src" / "chain.py").write_text("x = 1\n", encoding="utf-8")
        (self.root / "lore.toml").write_text(CONFIG.format(mode=self.mode), encoding="utf-8", newline="\n")
        (self.root / ".gitattributes").write_text("lore/** text eol=lf\n", encoding="utf-8", newline="\n")
        git(self.root, "add", "-A")
        git(self.root, "commit", "-qm", "init")
        self.c0 = git(self.root, "rev-parse", "HEAD")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def lore(self, *argv: str, stdin: str = "", ok: bool = True, console: str | None = None) -> str:
        """`console` decodes stdin as a Windows console would (cp1252), the
        way `sys.stdin` reads there; by default stdin is plain text."""
        out, err = io.StringIO(), io.StringIO()
        old_in = sys.stdin
        sys.stdin = (io.TextIOWrapper(io.BytesIO(stdin.encode("utf-8")), encoding=console)
                     if console else io.StringIO(stdin))
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = cli.main(["-C", str(self.root), *argv])
        finally:
            sys.stdin = old_in
        text = out.getvalue() + err.getvalue()
        if ok and code != 0:
            raise AssertionError(f"lore {' '.join(argv)} exited {code}:\n{text}")
        if not ok and code == 0:
            raise AssertionError(f"lore {' '.join(argv)} should have refused:\n{text}")
        return text

    def lore_json(self, *argv: str, stdin: str = ""):
        return json.loads(self.lore("--json", *argv, stdin=stdin))

    def fault(self, title="a fault", **kw) -> dict:
        r = {"kind": "fault", "area": "git", "title": title,
             "symptom": kw.pop("symptom", f"the symptom of {title}"),
             "cause": kw.pop("cause", "a cause"), "fix": kw.pop("fix", "a fix"),
             "check": kw.pop("check", "a check"), "status": "live"}
        r.update(kw)
        return r

    def add(self, *records: dict, new: bool = True) -> dict:
        args = ["add", "any", "--stdin"] + (["--new"] if new else [])
        return self.lore_json(*args, stdin=json.dumps(list(records)))

    def head(self) -> str:
        return git(self.root, "rev-parse", "HEAD")
