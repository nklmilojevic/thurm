"""Tests for tip_notes.py: python3 -m unittest discover -s macos/release"""

import os
import subprocess
import tempfile
import unittest

import tip_notes

REPO = "owner/thurm"


def git(cwd, *args):
    env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
               GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com")
    return subprocess.run(["git", *args], cwd=cwd, env=env, check=True,
                          capture_output=True, text=True).stdout.strip()


class NotesTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = self.tmp.name
        git(self.dir, "init", "-q", "-b", "main")
        git(self.dir, "commit", "-q", "--allow-empty", "-m", "first")
        self.previous = git(self.dir, "rev-parse", "HEAD")
        # Two pull requests (one brought up to date with main first) and a direct commit.
        for n, branch in [(1, "fix/a"), (2, "feat/b")]:
            git(self.dir, "switch", "-q", "-c", branch)
            git(self.dir, "commit", "-q", "--allow-empty", "-m", f"work on {branch}")
            if n == 2:
                git(self.dir, "merge", "-q", "--no-ff", "main", "-m",
                    f"Merge remote-tracking branch 'origin/main' into {branch}")
            git(self.dir, "switch", "-q", "main")
            git(self.dir, "merge", "-q", "--no-ff", branch, "-m",
                f"Merge pull request #{n} from owner/{branch}\n\nPR title {n}")
        git(self.dir, "commit", "-q", "--allow-empty", "-m", "direct change")
        self.head = git(self.dir, "rev-parse", "HEAD")
        self.cwd = os.getcwd()
        os.chdir(self.dir)

    def tearDown(self):
        os.chdir(self.cwd)
        self.tmp.cleanup()

    def notes(self, since):
        items = tip_notes.commits(self.head, since or None, None if since else 20)
        return tip_notes.render(REPO, self.head, since or None, items)

    def test_every_change_since_the_previous_tip(self):
        out = self.notes(self.previous)
        self.assertIn(f"compare/{self.previous[:7]}...{self.head[:7]}", out)
        entries = [line for line in out.splitlines() if line.startswith("- ")]
        self.assertEqual(len(entries), 3, out)
        self.assertTrue(entries[0].startswith("- direct change ([`"), entries[0])
        self.assertEqual(entries[1], f"- PR title 2 ([#2](https://github.com/{REPO}/pull/2))")
        self.assertEqual(entries[2], f"- PR title 1 ([#1](https://github.com/{REPO}/pull/1))")
        # Branch commits and up-to-date merges are inside the pull requests.
        self.assertNotIn("work on", out)
        self.assertNotIn("remote-tracking", out)

    def test_first_tip_lists_the_latest_changes(self):
        out = self.notes("")
        self.assertIn("Latest changes:", out)
        self.assertIn("- first ([`", out)

    def test_nothing_new(self):
        out = tip_notes.render(REPO, self.head, self.head, [])
        self.assertIn("No changes to the app", out)

    def test_long_lists_are_cut(self):
        items = [(f"{i:040x}", f"change {i}", "") for i in range(tip_notes.MAX_ENTRIES + 7)]
        out = tip_notes.render(REPO, self.head, self.previous, items)
        entries = [line for line in out.splitlines() if line.startswith("- ")]
        self.assertEqual(len(entries), tip_notes.MAX_ENTRIES + 1)
        self.assertIn("[7 more](https://github.com/owner/thurm/compare/", entries[-1])


if __name__ == "__main__":
    unittest.main()
