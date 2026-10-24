import hashlib
import json
import math
import time
import unittest
from array import array
from datetime import date, timedelta

from helpers import RepoCase, git

from codelore import embed
from codelore.index import terms_of
from codelore.search import errors_of


class FakeProvider:
    """Bag-of-words vectors: deterministic meaning for the fusion tests."""
    name = "fake"
    model = "fake-model"

    def embed(self, texts):
        out = []
        for t in texts:
            v = [0.0] * 64
            for w in t.lower().split():
                w = w.strip(".,:;()")
                if len(w) > 2:
                    v[int(hashlib.md5(w.encode()).hexdigest(), 16) % 64] += 1.0
            out.append(embed.normalise(v))
        return out


class Search(RepoCase):
    def setUp(self):
        super().setUp()
        self.add(
            self.fault("git archive piped into tar fails", symptom="every file fails to extract",
                       signals=["tar.exe: Damaged tar archive (bad header checksum)"],
                       refs={"files": ["src/chain.py"], "symbols": ["scene.depth_buffer"]}),
            self.fault("the device was lost", area="render", symptom="every frame reads the same stale picture",
                       signals=["ERR_DEVICE_LOST", "0x80070005"]),
            self.fault("old thing", area="build", status="obsolete", symptom="an obsolete symptom"))

    def ids(self, *query, **kw):
        args = ["find", *query]
        for k, v in kw.items():
            args += [f"--{k}"] if v is True else [f"--{k}", v]
        return [h["record"]["id"] for h in self.lore_json(*args)["results"]]

    def test_exact_signal_first(self):
        self.assertEqual(self.ids("whatever", "tar.exe: Damaged tar archive (bad header checksum)", "more")[0],
                         "git-archive-piped-into-tar-fails")
        self.assertEqual(self.ids("0x80070005")[0], "the-device-was-lost")
        self.assertEqual(self.ids("ERR_DEVICE_LOST")[0], "the-device-was-lost")

    def test_a_copied_phrase_beats_a_bag_of_its_words(self):
        # many long records full of the phrase's common words, and one short one that says it
        noise = [self.fault(f"noise {i}", area="build",
                            symptom=("the file on the line wrote every head of the line, the file on every line " * 3).strip())
                 for i in range(12)]
        self.add(*noise, self.fault("crlf rewrite", area="build",
                                    symptom="the file it wrote differs from HEAD on every line"))
        self.assertEqual(self.ids("the file it wrote differs from HEAD on every line")[0], "crlf-rewrite")
        out = self.lore_json("find", "the file it wrote differs from HEAD on every line")
        self.assertEqual(out["results"][0]["via"][0], "phrase")

    def test_words(self):
        self.assertEqual(self.ids("same stale picture on every frame")[0], "the-device-was-lost")

    def test_terms_are_whole(self):
        self.assertIn("scene.depth_buffer", terms_of("it writes scene.depth_buffer here"))
        self.assertIn("ERR_DEVICE_LOST", terms_of("got ERR_DEVICE_LOST"))
        self.assertIn("0x80070005", terms_of("HRESULT 0x80070005."))
        self.assertEqual(self.ids("scene.depth_buffer")[0], "git-archive-piped-into-tar-fails")

    def test_retired_hidden_unless_all(self):
        self.assertNotIn("old-thing", self.ids("obsolete symptom"))
        self.assertIn("old-thing", self.ids("obsolete symptom", all=True))
        self.assertEqual(self.ids("picture", kind="fault", area="render"), ["the-device-was-lost"])

    def test_fresh_after_change(self):
        self.lore("update", "the-device-was-lost", "-s", "symptom=the frame turns purple")
        self.assertEqual(self.ids("purple")[0], "the-device-was-lost")
        self.assertNotIn("the-device-was-lost", self.ids("stale picture"))

    def test_pasted_traceback(self):
        log = ("Traceback (most recent call last):\n  File \"x.py\", line 3, in <module>\n"
               "    run()\nRuntimeError: renderer: GetData failed, HRESULT 0x80070005\n")
        q = errors_of(log)
        self.assertNotIn("File", q)
        hits = self.lore_json("find", "-", stdin=log)["results"]
        self.assertEqual(hits[0]["record"]["id"], "the-device-was-lost")

    def test_for_file_dir_symbol(self):
        for t in ("src/chain.py", "src", "src/", str(self.root / "src" / "chain.py"), "scene.depth_buffer", "scene"):
            with self.subTest(t=t):
                ids = [r["id"] for r in self.lore_json("for", t)]
                self.assertEqual(ids, ["git-archive-piped-into-tar-fails"])
        self.assertEqual(self.lore_json("for", "src/other.py"), [])

    def test_provider_down_is_text_only_and_quick(self):
        cfg = (self.root / "lore.toml").read_text(encoding="utf-8").replace(
            'provider = "none"', 'provider = "ollama"\nurl = "http://127.0.0.1:9"\ntimeout = 1.0')
        (self.root / "lore.toml").write_text(cfg, encoding="utf-8")
        t = time.monotonic()
        out = self.lore_json("find", "stale picture")
        self.assertLess(time.monotonic() - t, 4.0)
        self.assertTrue(any("unavailable" in n for n in out["notes"]), out["notes"])
        self.assertEqual(out["results"][0]["record"]["id"], "the-device-was-lost")

    def test_fusion_with_meaning(self):
        from codelore import cli
        orig = embed.provider
        embed.provider = lambda cfg: FakeProvider()
        cli.embed.provider = embed.provider
        try:
            out = self.lore_json("find", "frame picture stale")
            self.assertIn("meaning", out["results"][0]["via"])
            self.assertEqual(out["results"][0]["record"]["id"], "the-device-was-lost")
        finally:
            embed.provider = orig
            cli.embed.provider = orig


class Plans(RepoCase):
    def setUp(self):
        super().setUp()
        self.lore("plan", "new", "--id", "api-v2", "--title", "API v2", "--goal", "the v2 endpoints serve")
        self.lore("item", "add", "api-v2", "--title", "schema", "--criterion", "documented")
        self.lore("item", "add", "api-v2", "--title", "handlers", "--dep", "api-v2#1")
        self.lore("item", "add", "api-v2", "--title", "docs", "--dep", "api-v2#2:soft")

    def test_ready_and_layers(self):
        ready = [r["id"] for r in self.lore_json("items", "--ready")]
        self.assertEqual(ready, ["api-v2#1", "api-v2#3"])
        text = self.lore("plan", "show", "api-v2")
        self.assertIn("layer 2", text)

    def test_closing_rules(self):
        self.lore("item", "done", "api-v2#1", "--commit", "deadbeef", ok=False)   # no such commit
        self.lore("item", "done", "api-v2#1", "--commit", self.c0[:8])
        self.lore("item", "drop", "api-v2#2", "--reason", "v1 is enough", "--decided-by", "owner")
        self.lore("item", "reopen", "api-v2#2", "--reason", "needed after all")
        r = self.lore_json("show", "api-v2#2")
        self.assertEqual(r["status"], "open")
        self.assertNotIn("reason", r)
        self.lore("item", "drop", "api-v2#2", "--reason", "v1 is enough", "--decided-by", "owner")
        self.lore("plan", "close", "api-v2", "--outcome", "done", ok=False)       # #3 is open
        self.lore("item", "defer", "api-v2#3", "--reason", "after the release")
        self.lore("plan", "close", "api-v2", "--outcome", "done", ok=False)       # deferred is undecided
        self.lore("item", "supersede", "api-v2#3", "--with", "api-v2#1")
        self.lore("plan", "close", "api-v2", "--outcome", "schema shipped; handlers dropped")
        self.assertEqual(self.lore_json("plans"), [])
        self.assertEqual(len(self.lore_json("plans", "--all")), 1)
        self.lore("item", "add", "api-v2", "--title", "late", ok=False)            # plan is closed

    def test_done_item_does_not_silently_reopen(self):
        self.lore("item", "done", "api-v2#1", "--commit", self.c0[:8])
        self.lore("update", "api-v2#1", "-s", "status=open", "-s", "commits=", ok=False)

    def test_cycle_refused_by_name(self):
        text = self.lore("update", "api-v2#1", "-s", "deps+=api-v2#3", ok=False)
        self.assertIn("cycle", text)
        self.assertIn("api-v2#1", text)

    def test_unknown_dep_refused(self):
        text = self.lore("item", "add", "api-v2", "--title", "x", "--dep", "api-v2#9", ok=False)
        self.assertIn("not an item", text)

    def test_stale(self):
        self.assertEqual(self.lore_json("stale", "--days", "3"), [])
        # age the plan file's records by rewriting their dates through the store
        from codelore.store import parse, serialize
        p = self.root / "lore" / "plans" / "api-v2.jsonl"
        old = (date.today() - timedelta(days=30)).isoformat()
        recs = parse(p.read_bytes(), "x")
        for r in recs:
            if r["id"] != "api-v2#2":
                r["created"] = r["updated"] = old
        p.write_bytes(serialize(recs))
        ids = [r["id"] for r in self.lore_json("stale", "--days", "3")]
        self.assertEqual(ids, ["api-v2#1", "api-v2#3"])


class Gate(RepoCase):
    def test_check_and_guide(self):
        self.add(self.fault("one"))
        self.assertIn("ok", self.lore("check"))
        p = self.root / "lore" / "git.jsonl"
        p.write_bytes(p.read_bytes().replace(b'{"area"', b'{ "area"'))
        self.assertIn("not canonical", self.lore("check", ok=False))
        self.lore("fmt")
        self.assertIn("ok", self.lore("check"))
        guide = self.lore("guide")
        from codelore.cli import build_parser
        parser = build_parser()
        for line in guide.splitlines():
            s = line.strip()
            if line.startswith("    lore ") and "<" not in s.split()[1]:
                words = s.split()[1:3]
                with self.subTest(cmd=s):
                    sub = parser._subparsers._group_actions[0].choices
                    self.assertIn(words[0], sub)
                    if words[0] in ("plan", "item"):
                        self.assertIn(words[1], sub[words[0]]._subparsers._group_actions[0].choices)

    def test_init(self):
        import tempfile, shutil, os
        d = self.tmp / "fresh"
        d.mkdir()
        git(d, "init", "-q", "-b", "main")
        from helpers import cli
        import io, contextlib
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(cli.main(["-C", str(d), "init", "--areas", "a,b"]), 0)
            self.assertEqual(cli.main(["-C", str(d), "check"]), 0)
        self.assertIn("lore/** text eol=lf", (d / ".gitattributes").read_text())


if __name__ == "__main__":
    unittest.main()
