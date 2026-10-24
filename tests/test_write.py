import json
import unittest

from helpers import RepoCase, git

from codelore import gitio


class Plain(RepoCase):
    mode = "plain"

    def test_commits_only_its_file_while_another_is_staged(self):
        (self.root / "theirs.txt").write_text("theirs\n", encoding="utf-8")
        git(self.root, "add", "theirs.txt")
        out = self.add(self.fault("tar pipe"))
        self.assertTrue(out["commit"])
        files = git(self.root, "show", "--name-only", "--format=", "HEAD").split()
        self.assertEqual(files, ["lore/git.jsonl"])
        self.assertIn("theirs.txt", git(self.root, "diff", "--cached", "--name-only"))

    def test_refuses_and_writes_nothing(self):
        before = self.head()
        self.lore("add", "any", "--stdin", "--new", stdin=json.dumps(
            self.fault("bad", refs={"commits": ["deadbeef"]})), ok=False)
        self.lore("add", "any", "--stdin", "--new", stdin=json.dumps(
            self.fault("bad", refs={"files": ["src/missing.py"]})), ok=False)
        self.assertEqual(self.head(), before)
        self.assertFalse((self.root / "lore" / "git.jsonl").exists())

    def test_ids_are_made_and_unique(self):
        a = self.add(self.fault("Git archive pipe!"))["records"][0]["id"]
        b = self.add(self.fault("git archive pipe"))["records"][0]["id"]
        self.assertEqual(a, "git-archive-pipe")
        self.assertEqual(b, "git-archive-pipe-2")
        self.lore("add", "any", "--stdin", "--new", stdin=json.dumps(self.fault("x", id=a)), ok=False)

    def test_references_that_exist_pass(self):
        r = self.add(self.fault("good", refs={"commits": [self.c0[:8]], "files": ["src/chain.py"]}))
        self.assertTrue(r["commit"])

    def test_update_and_kind_is_fixed(self):
        rid = self.add(self.fault("one"))["records"][0]["id"]
        self.lore("update", rid, "-s", "fix=a better fix", "-s", "signals+=E_FAIL")
        r = self.lore_json("show", rid)
        self.assertEqual((r["fix"], r["signals"]), ("a better fix", ["E_FAIL"]))
        self.lore("update", rid, "-s", "kind=reading", ok=False)
        self.lore("update", rid, "-s", "area=build", ok=False)

    def test_stdin_is_utf8_whatever_the_console(self):
        rec = self.fault("quotes «patched» and an ellipsis…", symptom="it prints «patched» and stops…")
        out = json.loads(self.lore("--json", "add", "any", "--stdin", "--new",
                                   stdin=json.dumps(rec, ensure_ascii=False), console="cp1252"))
        r = self.lore_json("show", out["records"][0]["id"])
        self.assertEqual(r["symptom"], "it prints «patched» and stops…")
        self.assertNotIn("Â", (self.root / "lore" / "git.jsonl").read_text(encoding="utf-8"))

    def test_update_batch_is_one_commit_and_a_no_op_is_none(self):
        a = self.add(self.fault("one"))["records"][0]["id"]
        b = self.add(self.fault("two"))["records"][0]["id"]
        before = self.head()
        batch = "\n".join(json.dumps(p) for p in ({"id": a, "fix": "fix one"}, {"id": b, "fix": "fix two"}))
        self.lore("update", "--batch", stdin=batch + "\n" + json.dumps({"id": a}))
        self.assertEqual(git(self.root, "rev-list", "--count", f"{before}..HEAD"), "1")
        self.assertEqual(git(self.root, "log", "-1", "--format=%s"), "lore: 2 records (fault)")
        self.assertEqual(self.lore_json("show", b)["fix"], "fix two")
        same = self.head()
        out = json.loads(self.lore("--json", "update", "--batch", stdin=batch))
        self.assertIsNone(out["commit"])
        self.assertEqual(self.head(), same)
        self.lore("update", "--batch", stdin=json.dumps({"id": "nope", "fix": "x"}), ok=False)

    def test_duplicate_search_refuses_without_new(self):
        self.add(self.fault("git archive piped into tar fails",
                            symptom="extracting a git archive through a pipe fails on every file"))
        text = self.lore("add", "any", "--stdin", stdin=json.dumps(self.fault(
            "git archive piped to tar fails", symptom="extracting the git archive through a pipe fails on each file")),
            ok=False)
        self.assertIn("a record of this fault exists", text)
        self.assertIn("git-archive-piped-into-tar-fails (words", text)

    def test_shared_signal_refuses_and_a_neighbour_is_only_named(self):
        self.add(self.fault("device lost", signals=["ERR_DEVICE_LOST"],
                            symptom="every frame reads the same stale picture after a while"))
        text = self.lore("add", "any", "--stdin", stdin=json.dumps(self.fault(
            "a different title", symptom="nothing alike", signals=["err_device_lost"])), ok=False)
        self.assertIn("(signal 1.0)", text)
        out = json.loads(self.lore("--json", "add", "any", "--stdin", stdin=json.dumps(self.fault(
            "a stale frame after the device stopped", symptom="the picture stays old, other cause"))))
        self.assertTrue(out["commit"])
        self.assertTrue(any("is near device-lost" in n for n in out["notes"]), out["notes"])


class Shared(RepoCase):
    mode = "shared-tree"

    def test_foreign_live_edit_stays_out_and_in_place(self):
        self.add(self.fault("first"))
        from codelore.store import parse, serialize
        live = self.root / "lore" / "git.jsonl"
        recs = parse(live.read_bytes(), "x")
        other = dict(recs[0], id="zz-someone", title="someone else's uncommitted record")
        foreign = serialize(recs + [other])
        live.write_bytes(foreign)
        index_before = git(self.root, "ls-files", "-s")
        out = self.add(self.fault("second"))
        self.assertTrue(any("left as it was" in n for n in out["notes"]))
        self.assertEqual(live.read_bytes(), foreign)
        blob = git(self.root, "show", "HEAD:lore/git.jsonl")
        self.assertNotIn("zz-someone", blob)
        self.assertIn('"second"', blob)
        self.assertEqual(git(self.root, "ls-files", "-s"), index_before)

    def test_shared_index_untouched_for_other_paths(self):
        (self.root / "src" / "chain.py").write_text("x = 2\n", encoding="utf-8")
        git(self.root, "add", "src/chain.py")
        staged = git(self.root, "diff", "--cached", "--name-only")
        self.add(self.fault("one"))
        self.assertEqual(git(self.root, "diff", "--cached", "--name-only"), staged)
        self.assertEqual(git(self.root, "show", "--name-only", "--format=", "HEAD"), "lore/git.jsonl")
        # the new file's own entry is brought along, so status is clean for it
        self.assertEqual(git(self.root, "status", "--short", "--", "lore"), "")

    def test_race_retries_on_the_new_head(self):
        def other_commit():
            (self.root / "other.txt").write_text("other\n", encoding="utf-8")
            sha = git(self.root, "hash-object", "-w", "other.txt")
            tree_idx = self.tmp / "idx"
            import os
            env = dict(os.environ, GIT_INDEX_FILE=str(tree_idx))
            import subprocess
            subprocess.run(["git", "-C", str(self.root), "read-tree", "HEAD"], env=env, check=True)
            subprocess.run(["git", "-C", str(self.root), "update-index", "--add", "--cacheinfo",
                            f"100644,{sha},other.txt"], env=env, check=True)
            tree = subprocess.run(["git", "-C", str(self.root), "write-tree"], env=env, check=True,
                                  capture_output=True).stdout.decode().strip()
            c = git(self.root, "commit-tree", tree, "-p", "HEAD", "-m", "other session")
            git(self.root, "update-ref", "refs/heads/main", c)

        def transform(old, rev):
            return {"lore/git.jsonl": b"line\n"}

        res = gitio.write_shared(self.root, "main", ["lore/git.jsonl"], transform, "s", "", [],
                                 _before_update=other_commit)
        self.assertTrue(any("moved while writing" in n for n in res.notes))
        log = git(self.root, "log", "--format=%s", "-3").splitlines()
        self.assertEqual(log[:2], ["s", "other session"])
        self.assertEqual(git(self.root, "show", "HEAD:other.txt"), "other")

    def test_live_file_an_older_version_is_brought_up(self):
        self.add(self.fault("first"))
        live = self.root / "lore" / "git.jsonl"
        old = live.read_bytes()
        self.add(self.fault("second"))
        # a sync that could not replace the file leaves it, and its index entry, one version behind
        live.write_bytes(old)
        git(self.root, "add", "lore/git.jsonl")
        out = self.add(self.fault("third"))
        self.assertTrue(any("older version of itself" in n for n in out["notes"]), out["notes"])
        self.assertEqual(live.read_bytes().decode("utf-8"), git(self.root, "show", "HEAD:lore/git.jsonl") + "\n")
        self.assertEqual(git(self.root, "status", "--short", "--", "lore"), "")

    def test_held_live_file_leaves_the_commit_and_a_note(self):
        from unittest import mock
        self.add(self.fault("first"))
        live = self.root / "lore" / "git.jsonl"
        before = live.read_bytes()
        with mock.patch.object(gitio.os, "replace", side_effect=PermissionError("held")), \
                mock.patch.object(gitio.time, "sleep"):
            res = gitio.write_shared(self.root, "main", ["lore/git.jsonl"],
                                     lambda old, rev: {"lore/git.jsonl": b"line\n"}, "s", "", [])
        self.assertTrue(res.commit)
        self.assertTrue(any("another process holds" in n for n in res.notes), res.notes)
        self.assertEqual(live.read_bytes(), before)
        self.assertFalse((self.root / "lore" / "git.jsonl.lore-tmp").exists())

    def test_two_writers_both_land(self):
        self.add(self.fault("one"))
        self.add(self.fault("two"))
        blob = git(self.root, "show", "HEAD:lore/git.jsonl")
        self.assertEqual(len(blob.splitlines()), 2)

    def test_refuses_off_branch(self):
        git(self.root, "checkout", "-q", "-b", "side")
        self.lore("add", "any", "--stdin", "--new", stdin=json.dumps(self.fault("x")), ok=False)


class NoneMode(RepoCase):
    mode = "none"

    def test_writes_without_committing(self):
        before = self.head()
        self.add(self.fault("one"))
        self.assertEqual(self.head(), before)
        self.assertTrue((self.root / "lore" / "git.jsonl").exists())


if __name__ == "__main__":
    unittest.main()
