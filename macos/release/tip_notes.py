#!/usr/bin/env python3
"""Release notes for a tip build: every change on main since the previous tip.

usage: tip_notes.py --repo OWNER/NAME --sha SHA [--since PREVIOUS_TIP_SHA]

Merged pull requests are listed by their title. Without --since (the first tip, or a tip that
isn't an ancestor of SHA), the newest changes are listed.
"""

import argparse
import re
import subprocess
import sys

MAX_ENTRIES = 50
MERGE_PR = re.compile(r"^Merge pull request #(\d+) from ")
# Merges that aren't changes of their own (a branch brought up to date).
MERGE_OTHER = re.compile(r"^Merge (branch|remote-tracking branch) ")


def commits(sha, since=None, limit=None):
    """(sha, subject, body) of main's own commits, newest first."""
    args = ["git", "log", "--first-parent", "--format=%H%x1f%s%x1f%b%x1e"]
    if limit:
        args.append(f"-{limit}")
    args.append(f"{since}..{sha}" if since else sha)
    out = subprocess.run(args, check=True, capture_output=True, text=True).stdout
    result = []
    for record in out.split("\x1e"):
        record = record.strip("\n")
        if not record:
            continue
        h, subject, body = (record.split("\x1f") + ["", ""])[:3]
        result.append((h, subject, body))
    return result


def entry(repo, h, subject, body):
    """One list line, or None for a merge that brings nothing of its own."""
    m = MERGE_PR.match(subject)
    if m:
        title = next((line.strip() for line in body.splitlines() if line.strip()), subject)
        return f"- {title} ([#{m.group(1)}](https://github.com/{repo}/pull/{m.group(1)}))"
    if MERGE_OTHER.match(subject):
        return None
    return f"- {subject} ([`{h[:7]}`](https://github.com/{repo}/commit/{h}))"


def render(repo, sha, since, items):
    lines = [f"**Tip build** of [`{sha[:7]}`](https://github.com/{repo}/commit/{sha})", ""]
    entries = [e for e in (entry(repo, *c) for c in items) if e]
    if since:
        compare = f"https://github.com/{repo}/compare/{since[:7]}...{sha[:7]}"
        lines.append(f"Changes since the previous tip ([`{since[:7]}`]({compare})):")
    else:
        compare = f"https://github.com/{repo}/commits/{sha}"
        lines.append("Latest changes:")
    lines.append("")
    if not entries:
        lines.append("- No changes to the app (rebuilt).")
    lines.extend(entries[:MAX_ENTRIES])
    if len(entries) > MAX_ENTRIES:
        lines.append(f"- …and [{len(entries) - MAX_ENTRIES} more]({compare})")
    return "\n".join(lines) + "\n"


def is_ancestor(a, b):
    return subprocess.run(["git", "merge-base", "--is-ancestor", a, b]).returncode == 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--repo", required=True)
    ap.add_argument("--sha", required=True)
    ap.add_argument("--since", default="")
    a = ap.parse_args(argv)
    since = a.since if a.since and a.since != a.sha and is_ancestor(a.since, a.sha) else None
    items = commits(a.sha, since, None if since else 20)
    sys.stdout.write(render(a.repo, a.sha, since, items))


if __name__ == "__main__":
    main()
