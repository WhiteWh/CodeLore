import json
import unittest

from helpers import RepoCase


class RetireDuplicate(RepoCase):
    def test_fold_a_duplicate_into_its_twin(self):
        self.add(self.fault("count reads a docstring", symptom="the resource count is one too many"),
                 self.fault("resources off by one", symptom="dx resources read one more than the brief"))
        self.lore("update", "count-reads-a-docstring", "-s", "status=superseded",
                  "-s", "superseded_by=resources-off-by-one")
        r = self.lore_json("show", "count-reads-a-docstring")
        self.assertEqual((r["status"], r["superseded_by"]), ("superseded", "resources-off-by-one"))
        ids = [h["record"]["id"] for h in self.lore_json("find", "resource count one too many")["results"]]
        self.assertNotIn("count-reads-a-docstring", ids)
        self.assertIn("count-reads-a-docstring",
                      [h["record"]["id"] for h in self.lore_json("find", "resource count one too many", "--all")["results"]])
        self.lore("update", "resources-off-by-one", "-s", "status=superseded", ok=False)  # names no twin
        self.lore("update", "resources-off-by-one", "-s", "status=superseded", "-s", "superseded_by=nope",
                  ok=False)  # a twin that does not exist

    def test_append_to_text_refuses_clearly(self):
        self.add(self.fault("one"))
        text = self.lore("update", "one", "-s", "fix+=more", ok=False)
        self.assertIn("is text, and += appends to a list", text)
        self.lore("update", "one", "-s", "fix=a whole new fix")
        self.assertEqual(self.lore_json("show", "one")["fix"], "a whole new fix")


if __name__ == "__main__":
    unittest.main()
