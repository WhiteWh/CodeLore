import json
import unittest

from helpers import RepoCase

from codelore.schema import validate

AREAS = ["git", "build"]


def topic(**kw):
    r = {"v": 2, "id": "the-frame", "kind": "topic", "area": "build", "title": "How the frame is drawn",
         "status": "current", "created": "2026-10-24", "updated": "2026-10-24", "by": "me",
         "body": "The frame draws the ground, then the props, then the post chain."}
    r.update(kw)
    return {k: v for k, v in r.items() if v is not None}


def recipe(**kw):
    r = {"v": 2, "id": "cut-a-release", "kind": "recipe", "area": "build", "title": "Cutting a release",
         "status": "current", "created": "2026-10-24", "updated": "2026-10-24", "by": "me",
         "when": "a version is to be handed out", "steps": ["build", "check", "publish"],
         "check": "the published index names the version"}
    r.update(kw)
    return {k: v for k, v in r.items() if v is not None}


class Kinds(unittest.TestCase):
    def test_valid(self):
        self.assertEqual(validate(topic(), AREAS), [])
        self.assertEqual(validate(recipe(), AREAS), [])

    def test_rules(self):
        cases = [
            (topic(body=None), "`body` is required"),
            (topic(body="x" * 3001), "the limit is 3000"),
            (topic(status="live"), "`status` must be one of"),
            (topic(status="superseded"), "`superseded_by` is required when `superseded`"),
            (topic(v=1), "a topic needs format version 2"),
            (recipe(steps=[]), "`steps` is required"),
            (recipe(steps="one"), "list of non-empty strings"),
            (recipe(steps=["x" * 401]), "the limit is 400"),
            (recipe(when=None), "`when` is required"),
            (recipe(check=None), "`check` is required"),
            (topic(v=3), "newer than this lore reads"),
        ]
        for rec, words in cases:
            with self.subTest(words=words):
                errs = validate(rec, AREAS)
                self.assertTrue(any(words in e for e in errs), f"{words!r} not in {errs}")

    def test_version_one_records_still_read(self):
        f = {"v": 1, "id": "old-fault", "kind": "fault", "area": "git", "title": "t", "status": "live",
             "created": "2026-10-24", "updated": "2026-10-24", "by": "me", "symptom": "s", "cause": "c",
             "fix": "f", "check": "k"}
        self.assertEqual(validate(f, AREAS), [])


class Cli(RepoCase):
    def test_add_find_show_update(self):
        out = self.lore_json("add", "topic", "--stdin", stdin=json.dumps({
            "area": "build", "title": "How the frame is drawn",
            "body": "The frame draws the ground first, then the props, then the post chain, in that order."}))
        rid = out["records"][0]["id"]
        rec = self.lore_json("show", rid)
        self.assertEqual((rec["v"], rec["status"]), (2, "current"))
        self.assertEqual(self.lore_json("find", "in what order is the frame drawn")["results"][0]["record"]["id"], rid)
        self.lore("add", "recipe", "--stdin", stdin=json.dumps({
            "area": "build", "title": "Cutting a release", "when": "a version is to be handed out",
            "steps": ["build the bundle", "run the release check", "publish the index"],
            "check": "the published index names the version"}))
        text = self.lore("show", "cutting-a-release")
        self.assertIn("1. build the bundle", text)
        self.assertIn("3. publish the index", text)
        self.lore("update", "cutting-a-release", "-s", "steps+=announce it")
        self.assertEqual(len(self.lore_json("show", "cutting-a-release")["steps"]), 4)
        self.assertEqual(self.lore_json("find", "release", "--kind", "recipe")["results"][0]["record"]["id"],
                         "cutting-a-release")

    def test_old_record_updated_moves_to_v2(self):
        self.add(self.fault("a fault"))
        p = self.root / "lore" / "git.jsonl"
        from codelore.store import parse, serialize
        recs = parse(p.read_bytes(), "x")
        recs[0]["v"] = 1
        p.write_bytes(serialize(recs))
        self.assertIn("ok", self.lore("check"))
        self.lore("update", "a-fault", "-s", "fix=a better fix")
        self.assertEqual(self.lore_json("show", "a-fault")["v"], 2)


if __name__ == "__main__":
    unittest.main()
