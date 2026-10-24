"""What a large migration of project documents into a lore met: retirements,
batches, families of records, plans, checks at scale, sandboxes, and the
coverage of a source document."""
import json
import os
import unittest
from unittest import mock

from helpers import RepoCase, git

from codelore import FORMAT_VERSION, gitio
from codelore.cover import coverage, parts_of
from codelore.schema import validate


DOC = """# Handbook

Some words about the handbook.

## Setup

1. Install the tools
   with the second line of the first step
2. Run the gate

## Faults

| Error | Fix |
|---|---|
| `E_FAIL` on resize | restart |
| a hang on exit | wait |

```
## not a heading, it is in a fence
```

## Setup

The second setup, said again.
"""


class Parts(unittest.TestCase):
    def test_headings_rows_items_and_repeats(self):
        names = [(p.kind, p.name) for p in parts_of(DOC)]
        self.assertEqual(names, [
            ("heading", "Handbook"),
            ("text", "Handbook / text"),
            ("heading", "Setup"),
            ("item", "Setup / 1. Install the tools"),
            ("item", "Setup / 2. Run the gate"),
            ("heading", "Faults"),
            ("text", "Faults / text"),  # the fence under it is its own content
            ("row", "Faults | `E_FAIL` on resize"),
            ("row", "Faults | a hang on exit"),
            ("heading", "Setup (2)"),
        ])

    def test_a_heading_entry_covers_what_is_under_it(self):
        parts = parts_of(DOC)
        cov = coverage(parts, [{"part": "faults", "skip": "history"}])
        self.assertNotIn("Faults | a hang on exit", [p.name for p in cov.uncovered])
        # a heading that says nothing itself is covered by its items
        cov = coverage(parts, [{"part": "Setup / 1. Install the tools", "records": ["a"]},
                               {"part": "Setup / 2. Run the gate", "skip": "one-off"}])
        self.assertNotIn("Setup", [p.name for p in cov.uncovered])
        # what a heading says itself needs its own entry, which covers nothing else
        self.assertIn("Handbook / text", [p.name for p in cov.uncovered])
        cov = coverage(parts, [{"part": "Handbook / text", "skip": "an introduction"}])
        self.assertIn("Setup / 2. Run the gate", [p.name for p in cov.uncovered])
        self.assertEqual(coverage(parts, [{"part": "No such part", "skip": "x"}]).unknown, ["No such part"])


class Writes(RepoCase):
    def test_retire_with_long_ids_and_a_value_that_is_not_an_id(self):
        a, b = "a" * 48, "b" * 48
        self.add(self.fault("x", id=a), self.fault("y", id=b, symptom="nothing like it", cause="z q"))
        self.lore("update", a, "-s", "status=superseded", "-s", f"superseded_by={b}")
        self.assertEqual(self.lore_json("show", a)["superseded_by"], b)
        text = self.lore("update", b, "-s", "status=superseded", "-s", f"superseded_by={a} {b}", ok=False)
        self.assertIn(f"names one record by its id, and '{a} {b}' is not an id", text)

    def test_update_json_in_takes_the_id_from_the_patch(self):
        rid = self.add(self.fault("one"))["records"][0]["id"]
        self.lore("update", "--json-in", stdin=json.dumps({"id": rid, "fix": "from the patch"}))
        self.assertEqual(self.lore_json("show", rid)["fix"], "from the patch")
        self.lore("update", "other", "--json-in", stdin=json.dumps({"id": rid, "fix": "x"}), ok=False)
        self.assertIn("which record?", self.lore("update", "--json-in", stdin=json.dumps({"fix": "x"}), ok=False))

    def test_a_batch_with_one_duplicate_names_it_and_can_write_the_rest(self):
        self.add(self.fault("device lost", signals=["ERR_DEVICE_LOST: the device was removed"]))
        batch = [self.fault("fresh one", symptom="first new"),
                 self.fault("other words", symptom="second", signals=["err_device_lost: the device was removed"]),
                 self.fault("fresh two", symptom="third new", cause="another cause entirely")]
        text = self.lore("add", "any", "--stdin", stdin=json.dumps(batch), ok=False)
        self.assertIn("1 of 3 record(s) already exist", text)
        self.assertIn("#2 'other words' is: device-lost (signal 1.0)", text)
        self.assertIn("--skip-duplicates writes the other 2", text)
        out = json.loads(self.lore("--json", "add", "any", "--stdin", "--skip-duplicates", stdin=json.dumps(batch)))
        self.assertEqual([r["title"] for r in out["records"]], ["fresh one", "fresh two"])
        self.assertTrue(any("left out #2 'other words'" in n for n in out["notes"]), out["notes"])

    def test_a_bare_class_name_is_not_a_signal(self):
        text = self.lore("add", "any", "--stdin", "--new", stdin=json.dumps(
            self.fault("x", signals=["AttributeError"])), ok=False)
        self.assertIn("signal 'AttributeError' is a bare name", text)
        self.add(self.fault("x", signals=["AttributeError: 'NoneType' object has no attribute 'depth'",
                                          "ERR_DEVICE_LOST", "0x80070005"]))

    def test_a_family_of_named_records_is_not_refused(self):
        body = "the fields: intensity, threshold, radius, enabled, quality, each with its default"
        self.lore("add", "topic", "--stdin", stdin=json.dumps(
            {"area": "render", "title": "`BloomSettings` - the bloom's settings", "body": body + " for bloom"}))
        out = json.loads(self.lore("--json", "add", "topic", "--stdin", stdin=json.dumps(
            {"area": "render", "title": "`DofSettings` - the depth of field's settings", "body": body + " for dof"})))
        self.assertTrue(out["commit"])
        self.assertTrue(any("names differ" in n for n in out["notes"]), out["notes"])
        # the same title said twice is still refused
        self.lore("add", "topic", "--stdin", stdin=json.dumps(
            {"area": "render", "title": "`DofSettings` - the depth of field's settings", "body": body}), ok=False)

    def test_a_duplicate_decision_is_retired_as_superseded(self):
        d = {"kind": "decision", "area": "build", "question": "which build tool?", "choice": "make",
             "reason": "it is there", "decided_by": "lead"}
        self.add(dict(d, title="build with make"), dict(d, title="we build with make", id="build-make-again"))
        self.lore("update", "build-make-again", "-s", "status=superseded", "-s", "superseded_by=build-with-make")
        self.assertNotIn("build-make-again", [h["record"]["id"] for h in self.lore_json("find", "build tool")["results"]])
        old = dict(d, v=2, id="x-y", title="t", status="superseded", superseded_by="build-with-make",
                   created="2026-01-01", updated="2026-01-01", by="t")
        self.assertTrue(any("needs format version 3" in e for e in validate(old, ["build"])))

    def test_commits_of_an_open_item_say_where_they_go(self):
        self.lore("plan", "new", "--title", "p", "--goal", "g", "--id", "pl")
        self.lore("item", "add", "pl", "--title", "one")
        text = self.lore("update", "pl#1", "-s", f"commits+={self.c0}", ok=False)
        self.assertIn("go in `refs.commits`", text)
        self.lore("update", "pl#1", "-s", f"refs.commits+={self.c0}")

    def test_an_item_keeps_its_outcome_and_reopening_clears_it(self):
        self.lore("plan", "new", "--title", "p", "--goal", "g", "--id", "pl")
        self.lore("item", "add", "pl", "--title", "one")
        self.lore("item", "done", "pl#1", "--commit", self.c0, "--outcome", "built the lane; its cache was wrong")
        self.assertEqual(self.lore_json("show", "pl#1")["outcome"], "built the lane; its cache was wrong")
        self.lore("item", "reopen", "pl#1", "--reason", "the cache")
        self.assertNotIn("outcome", self.lore_json("show", "pl#1"))
        self.lore("update", "pl#1", "-s", "outcome=not yet", ok=False)

    def test_exists_reads_live(self):
        rid = self.add(self.fault("one"))["records"][0]["id"]
        self.assertIn(f"{rid}  fault [live]", self.lore("exists", rid))
        text = self.lore("exists", rid, "nope", ok=False)
        self.assertIn("nope  MISSING", text)


class Check(RepoCase):
    def test_references_are_checked_in_one_git_process_each(self):
        self.add(*[self.fault(f"f{n}", symptom=f"s{n} distinct", cause=f"c{n} other",
                              refs={"commits": [self.c0[:10]], "files": ["src/chain.py"]}) for n in range(30)])
        real = gitio.git
        calls = []

        def spy(root, *args, **kw):
            calls.append(args[0])
            return real(root, *args, **kw)
        with mock.patch.object(gitio, "git", spy):
            out = self.lore("check")
        self.assertIn("30 record(s) in 1 file(s): ok", out)
        self.assertLessEqual(calls.count("cat-file"), 2, calls)

    def test_evidence(self):
        (self.root / "src" / "other.py").write_text("y = 1\n", encoding="utf-8")
        git(self.root, "add", "-A")
        git(self.root, "commit", "-qm", "other")
        c1 = self.head()
        self.add(self.fault("right", refs={"commits": [self.c0[:10]], "files": ["src/chain.py"]}),
                 self.fault("wrong", symptom="another", cause="d e", refs={"commits": [c1[:10]], "files": ["src/chain.py"]}))
        self.lore("check")
        text = self.lore("check", "--evidence", ok=False)
        self.assertIn(f"wrong: commit {c1[:10]} touches none of src/chain.py", text)
        self.assertNotIn("right:", text)
        # a hash written in the document the record came from is evidence too
        (self.root / "doc.md").write_text(f"## Wrong\n\nfixed in {c1[:8]}\n", encoding="utf-8")
        git(self.root, "add", "doc.md")
        git(self.root, "commit", "-qm", "doc")
        self.lore("cover", "doc.md", "--add", stdin=json.dumps({"part": "Wrong", "records": ["wrong"]}))
        self.lore("check", "--evidence")


class Sandbox(RepoCase):
    def test_index_falls_back_to_the_git_directory(self):
        blocker = self.tmp / "not-a-folder"
        blocker.write_text("x", encoding="utf-8")
        os.environ["LORE_CACHE"] = str(blocker)
        self.lore("add", "any", "--stdin", "--new", stdin=json.dumps(self.fault("one")))
        out = self.lore("show", "one")
        self.assertIn("the index is in", out)
        self.assertTrue((self.root / ".git" / "codelore" / "index.sqlite").exists())

    def test_a_failed_push_is_one_clear_line(self):
        cfg = (self.root / "lore.toml").read_text(encoding="utf-8").replace('branch = "main"', 'branch = "main"\npush = true')
        (self.root / "lore.toml").write_text(cfg, encoding="utf-8", newline="\n")
        git(self.root, "remote", "add", "origin", str(self.tmp / "no-such-remote"))
        out = self.lore("add", "any", "--stdin", "--new", stdin=json.dumps(self.fault("one")))
        sha = self.head()[:10]
        line = next(l for l in out.splitlines() if l.startswith("committed"))
        self.assertTrue(line.startswith(f"committed {sha} locally, push to origin/main failed: "), out)
        self.assertTrue(line.endswith("the next write's push carries it"), out)


class SharedShow(RepoCase):
    mode = "shared-tree"

    def test_show_finds_a_commit_the_working_tree_has_not_caught_up_with(self):
        self.add(self.fault("first"))
        with mock.patch.object(gitio.os, "replace", side_effect=PermissionError("held")), \
                mock.patch.object(gitio.time, "sleep"):
            rid = self.add(self.fault("second", symptom="other", cause="d e"))["records"][0]["id"]
        self.assertNotIn(rid, (self.root / "lore" / "git.jsonl").read_text(encoding="utf-8"))
        self.assertIn("not in the working tree yet", self.lore("show", rid))
        self.lore("exists", rid)
        # += appends to the record the commit is built on, not to the stale working tree
        self.lore("update", rid, "-s", "signals+=E_ONE")
        self.lore("update", rid, "-s", "signals+=E_TWO")
        self.assertEqual(self.lore_json("show", rid)["signals"], ["E_ONE", "E_TWO"])


class Cover(RepoCase):
    def setUp(self):
        super().setUp()
        (self.root / "docs").mkdir()
        (self.root / "docs" / "hand.md").write_text(DOC, encoding="utf-8", newline="\n")
        git(self.root, "add", "-A")
        git(self.root, "commit", "-qm", "doc")

    def test_cover_a_document_part_by_part(self):
        out = self.lore("cover", "docs/hand.md", ok=False)
        self.assertIn("no source record yet; 10 part(s)", out)
        self.assertIn("Faults | a hang on exit", out)
        rid = self.add(self.fault("hang on exit"))["records"][0]["id"]
        entries = [{"part": "Handbook / text", "skip": "an introduction"},
                   {"part": "Setup", "skip": "the old setup, replaced"},
                   {"part": "Faults | a hang on exit", "records": [rid]}]
        self.lore("cover", "docs/hand.md", "--add", stdin="\n".join(map(json.dumps, entries)))
        res = json.loads(self.lore("--json", "cover", "docs/hand.md", ok=False))  # exit 1: not complete
        self.assertFalse(res["complete"])
        self.assertEqual([u["part"] for u in res["uncovered"]], ["Faults / text", "Faults | `E_FAIL` on resize", "Setup (2)"])
        self.assertEqual(res["source"]["status"], "open")
        # a name that is no part of the document is refused
        self.lore("cover", "docs/hand.md", "--add", stdin=json.dumps({"part": "Fautls", "skip": "x"}), ok=False)
        self.lore("cover", "docs/hand.md", "--add", stdin="\n".join(map(json.dumps, [
            {"part": "Faults | `E_FAIL` on resize", "skip": "one-off, never seen again"},
            {"part": "setup  (2)", "skip": "said again"},
            {"part": "Faults / text", "skip": "an example block"}])))
        res = self.lore_json("cover", "docs/hand.md")
        self.assertTrue(res["complete"])
        self.assertEqual(res["source"]["status"], "covered")
        # the document answers `lore for`, and it is not a search result
        self.assertEqual([r["kind"] for r in self.lore_json("for", "docs/hand.md")], ["source"])
        self.assertNotIn("source", [h["record"]["kind"] for h in self.lore_json("find", "setup")["results"]])
        # a record a ledger names cannot go away quietly, and a document that grows is reported
        (self.root / "docs" / "hand.md").write_text(DOC + "\n## New\n\nnew words\n", encoding="utf-8", newline="\n")
        self.assertIn("changed since it was covered", self.lore("check"))

    def test_a_whole_document_is_history(self):
        rid = self.add(self.fault("what still stands"))["records"][0]["id"]
        self.lore("cover", "docs/hand.md", "--history", ok=False)  # needs a reason
        self.lore("cover", "docs/hand.md", "--history", "--reason", "a plan that was finished", "--record", rid)
        out = self.lore("cover", "docs/hand.md")
        self.assertIn("history", out)
        self.assertIn(f"what still stands is in: {rid}", out)
        src = self.lore_json("cover", "docs/hand.md")["source"]
        self.assertEqual((src["status"], src["v"]), ("history", FORMAT_VERSION))
        self.lore("check")


if __name__ == "__main__":
    unittest.main()
