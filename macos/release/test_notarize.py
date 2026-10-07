"""Tests for notarize.sh with a stand-in notarytool: python3 -m unittest discover -s macos/release"""

import os
import plistlib
import shutil
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SUBMISSION = "18079496-055c-42b0-a032-8fdf441f4501"

# Answers notarytool and stapler from the files in $FIXTURES and records the calls.
XCRUN = """#!/usr/bin/env bash
echo "$*" >> "$FIXTURES/calls"
case "$1 $2" in
  "notarytool submit") cat "$FIXTURES/submit" ;;
  "notarytool wait") cat "$FIXTURES/wait" ;;
esac
"""
# `ditto -c -k … SRC DEST`: only DEST matters.
DITTO = """#!/usr/bin/env bash
touch "${@: -1}"
"""
# `plutil -extract KEY raw -o - FILE`, for Linux.
PLUTIL = """#!/usr/bin/env python3
import plistlib, sys
key, path = sys.argv[2], sys.argv[-1]
try:
    with open(path, "rb") as f:
        value = plistlib.load(f)[key]
except Exception:
    sys.exit(1)
print(value)
"""


def plist(**keys):
    return plistlib.dumps(keys)


class NotarizeTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = self.tmp.name
        bin_dir = os.path.join(self.dir, "bin")
        os.mkdir(bin_dir)
        tools = {"xcrun": XCRUN, "ditto": DITTO}
        if shutil.which("plutil") is None:
            tools["plutil"] = PLUTIL
        for name, body in tools.items():
            path = os.path.join(bin_dir, name)
            with open(path, "w") as f:
                f.write(body)
            os.chmod(path, 0o755)
        self.app = os.path.join(self.dir, "Thurm.app")
        os.mkdir(self.app)
        self.env = dict(os.environ, FIXTURES=self.dir, NOTARY_PROFILE="test",
                        PATH=bin_dir + os.pathsep + os.environ["PATH"])

    def tearDown(self):
        self.tmp.cleanup()

    def run_notarize(self, submit, wait=b""):
        for name, data in [("submit", submit), ("wait", wait)]:
            with open(os.path.join(self.dir, name), "wb") as f:
                f.write(data)
        result = subprocess.run(["bash", os.path.join(HERE, "notarize.sh"), self.app],
                                env=self.env, capture_output=True, text=True)
        with open(os.path.join(self.dir, "calls")) as f:
            calls = [line.split()[:3] for line in f]
        return result, calls

    def test_accepted_at_once_is_stapled(self):
        result, calls = self.run_notarize(plist(id=SUBMISSION, status="Accepted"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(["notarytool", "wait", SUBMISSION], calls)
        self.assertIn(["stapler", "staple", self.app], calls)

    def test_timeout_keeps_waiting_for_the_same_submission(self):
        timeout = plist(id=SUBMISSION, message="Timeout of 1800 second(s) was reached")
        result, calls = self.run_notarize(timeout, plist(id=SUBMISSION, status="Accepted"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(["notarytool", "wait", SUBMISSION], calls)
        self.assertIn(["stapler", "staple", self.app], calls)

    def test_rejected_after_waiting_shows_the_log_and_fails(self):
        timeout = plist(id=SUBMISSION, message="Timeout of 1800 second(s) was reached")
        result, calls = self.run_notarize(timeout, plist(id=SUBMISSION, status="Invalid"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("returned: Invalid", result.stderr)
        self.assertIn(["notarytool", "log", SUBMISSION], calls)
        self.assertFalse(any(c[0] == "stapler" for c in calls))

    def test_output_that_is_no_plist_fails_without_a_log(self):
        result, calls = self.run_notarize(b"Error: something broke\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("returned: unknown", result.stderr)
        self.assertEqual([c[:2] for c in calls], [["notarytool", "submit"]])

    def test_an_id_that_is_no_uuid_is_not_used(self):
        result, calls = self.run_notarize(plist(id="-" * 36, message="Timeout"))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([c[:2] for c in calls], [["notarytool", "submit"]])


if __name__ == "__main__":
    unittest.main()
