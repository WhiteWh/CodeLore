import unittest

from helpers import RepoCase  # noqa: F401  (puts src on the path)

from codelore import LoreError
from codelore.config import parse as parse_config
from codelore.schema import check_limits, validate
from codelore.store import canonical, parse, serialize

AREAS = ["git"]


def fault(**kw):
    r = {"v": 1, "id": "tar-pipe", "kind": "fault", "area": "git", "title": "t", "status": "live",
         "created": "2026-10-24", "updated": "2026-10-24", "by": "me", "symptom": "s",
         "cause": "c", "fix": "f", "check": "k"}
    r.update(kw)
    return {k: v for k, v in r.items() if v is not None}


def item(n, **kw):
    r = {"v": 1, "id": f"pl#{n}", "kind": "item", "area": "plans", "plan": "pl", "title": f"i{n}",
         "status": "open", "created": "2026-10-24", "updated": "2026-10-24", "by": "me"}
    r.update(kw)
    return r


class Store(unittest.TestCase):
    def test_round_trip_keeps_bytes_and_unknown_fields(self):
        recs = [fault(id="b-one", extra={"x": [1, "ü"]}), fault(id="a-two")]
        data = serialize(recs)
        back = parse(data, "x.jsonl")
        self.assertEqual(serialize(back), data)
        self.assertEqual(back[1]["extra"], {"x": [1, "ü"]})
        self.assertIn("ü".encode(), data)

    def test_items_sort_by_number(self):
        plan = {"v": 1, "id": "pl", "kind": "plan", "area": "plans", "title": "p", "goal": "g",
                "status": "live", "created": "2026-10-24", "updated": "2026-10-24", "by": "me"}
        data = serialize([item(10), item(9), plan, item(2)])
        ids = [r["id"] for r in parse(data, "p.jsonl")]
        self.assertEqual(ids, ["pl", "pl#2", "pl#9", "pl#10"])

    def refuses(self, data: bytes, words: str):
        with self.assertRaises(LoreError) as cm:
            parse(data, "x.jsonl")
        self.assertIn(words, str(cm.exception))

    def test_refusals(self):
        good = serialize([fault(id="aa"), fault(id="bb")])
        self.refuses(good.replace(b"\n", b"\r\n"), "CR line ends")
        self.refuses(b"\xef\xbb\xbf" + good, "BOM")
        lines = good.split(b"\n")
        self.refuses(lines[1] + b"\n" + lines[0] + b"\n", "out of order")
        self.refuses(lines[0] + b"\n\n" + lines[1] + b"\n", "blank")
        self.refuses(good.rstrip(b"\n"), "no final newline")
        self.refuses(lines[0] + b"\n" + lines[0] + b"\n", "twice")
        spaced = canonical(fault(id="aa")).replace('":"', '": "', 1).encode() + b"\n"
        self.refuses(spaced, "not canonical")
        self.refuses(b"[1]\n", "not a JSON object")


class Schema(unittest.TestCase):
    def test_valid(self):
        self.assertEqual(validate(fault(), AREAS), [])

    def test_each_rule_refuses(self):
        cases = [
            (fault(symptom=None), "`symptom` is required"),
            (fault(status="fixed"), "`status` must be one of"),
            (fault(area="nowhere"), "is not one of lore.toml's areas"),
            (fault(id="Bad Id"), "`id` must match"),
            (fault(title="x" * 121), "the limit is 120"),
            (fault(cause="x" * 601), "the story goes in the commit"),
            (fault(signals="one"), "list of non-empty strings"),
            (fault(signals=["a", "a"]), "duplicates"),
            (fault(created="2026-13-01"), "must be a date"),
            (fault(created="2026-10-25"), "`updated` is before `created`"),
            (fault(v=3), "newer than this lore reads"),
            (fault(refs={"files": ["C:\\x"]}), "paths are relative"),
            (fault(refs={"commits": ["zz"]}), "not a hash"),
            (fault(refs={"other": ["x"]}), "is not one of"),
            (fault(kind="nope"), "`kind` must be one of"),
            (item(1, status="done"), "`commits` is required when `done`"),
            (item(1, status="dropped", reason="r"), "`decided_by` is required when `dropped`"),
            (item(1, status="dropped", decided_by="me"), "`reason` is required when `dropped`"),
            (item(1, reason="r"), "belongs to status"),
            (item(1, id="ql#1"), "names plan 'ql'"),
            (item(1, deps=[{"item": "pl#2"}]), "each dependency is"),
            (item(1, deps=[{"item": "pl#1", "kind": "hard"}]), "depends on itself"),
            (item(1, area="git"), "a item's `area` is `plans`"),
        ]
        for rec, words in cases:
            with self.subTest(words=words):
                errs = validate(rec, AREAS)
                self.assertTrue(any(words in e for e in errs), f"{words!r} not in {errs}")

    def test_limits_only_lower(self):
        self.assertTrue(any("the limit is 10" in e for e in validate(fault(cause="x" * 11), AREAS, {"fault.cause": 10})))
        with self.assertRaises(LoreError):
            check_limits({"fault.cause": 5000})
        with self.assertRaises(LoreError):
            check_limits({"fault.nope": 5})
        check_limits({"fault.cause": 100})


class Config(unittest.TestCase):
    def test_refusals(self):
        from pathlib import Path
        with self.assertRaises(LoreError) as cm:
            parse_config({"version": 2, "areas": ["Plans", "plans"], "commit": {"mode": "yolo"}, "x": 1}, Path("."))
        text = str(cm.exception)
        for w in ("version", "must match", "reserved", "commit.mode", "unknown key `x`"):
            self.assertIn(w, text)


if __name__ == "__main__":
    unittest.main()
