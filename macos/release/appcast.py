#!/usr/bin/env python3
"""Build Thurm's Sparkle appcast from its GitHub releases.

Every published release (and the rolling `tip` prerelease) carries a `sparkle.json` asset
written by build-release.sh. The appcast is generated from those alone, so it never drifts
from what is actually published and needs no state of its own:

    macos/release/appcast.py --repo OWNER/REPO --output site/appcast.xml

Release items have no channel (everyone gets them); tip items are in the `tip` channel, which
only apps set to the tip channel accept (Updater.swift). Uses the `gh` CLI (GH_TOKEN in CI).
"""

import argparse
import json
import subprocess
import sys
from xml.sax.saxutils import escape, quoteattr

SPARKLE_NS = "http://www.andymatuschak.org/xml-namespaces/sparkle"
TIP_TAG = "tip"


def gh(*args):
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def download_meta(repo, tag):
    try:
        meta = json.loads(gh("release", "download", tag, "--repo", repo,
                             "--pattern", "sparkle.json", "--output", "-"))
    except subprocess.CalledProcessError as e:
        print(f"skipping {tag}: no sparkle.json ({e.stderr.strip()})", file=sys.stderr)
        return None
    meta["tag"] = tag
    return meta


def collect(repo, limit):
    """sparkle.json of the newest `limit` releases plus the tip build."""
    releases = json.loads(
        gh("release", "list", "--repo", repo, "--limit", str(limit + 1),
           "--json", "tagName,isDraft,isPrerelease")
    )
    # The rolling tip release was created long ago; ask for it by name.
    tags = [r["tagName"] for r in releases
            if not r["isDraft"] and not r["isPrerelease"] and r["tagName"] != TIP_TAG][:limit]
    tags.append(TIP_TAG)
    return [m for m in (download_meta(repo, t) for t in tags) if m]


def cdata(text):
    return "<![CDATA[" + text.replace("]]>", "]]]]><![CDATA[>") + "]]>"


def render_item(m):
    title = f"Thurm {m['short_version']}"
    lines = [
        "    <item>",
        f"      <title>{escape(title)}</title>",
        f"      <pubDate>{escape(m['published'])}</pubDate>",
        f"      <sparkle:version>{escape(str(m['version']))}</sparkle:version>",
        f"      <sparkle:shortVersionString>{escape(m['short_version'])}</sparkle:shortVersionString>",
    ]
    if m["channel"] != "release":
        lines.append(f"      <sparkle:channel>{escape(m['channel'])}</sparkle:channel>")
    minimum = m.get("minimum_system_version") or "14.0"
    if minimum.count(".") == 1:
        minimum += ".0"
    lines += [
        f"      <sparkle:minimumSystemVersion>{escape(minimum)}</sparkle:minimumSystemVersion>",
        "      <sparkle:hardwareRequirements>arm64</sparkle:hardwareRequirements>",
    ]
    notes = m.get("notes") or ""
    if not notes.strip() and m.get("commit"):
        notes = f"Built from `{m['commit'][:7]}`."
    if notes.strip():
        lines.append(f'      <description sparkle:format="markdown">{cdata(notes)}</description>')
    lines += [
        "      <enclosure url={} length={} type=\"application/octet-stream\" sparkle:edSignature={}/>".format(
            quoteattr(m["url"]), quoteattr(str(m["length"])), quoteattr(m["ed_signature"])
        ),
        "    </item>",
    ]
    return "\n".join(lines)


def render(items, title="Thurm"):
    """The appcast XML for `items` (sparkle.json dicts), newest version first."""
    items = sorted(items, key=lambda m: int(m["version"]), reverse=True)
    body = "\n".join(render_item(m) for m in items)
    return (
        '<?xml version="1.0" encoding="utf-8"?>\n'
        f'<rss version="2.0" xmlns:sparkle="{SPARKLE_NS}">\n'
        "  <channel>\n"
        f"    <title>{escape(title)}</title>\n"
        f"{body}\n"
        "  </channel>\n"
        "</rss>\n"
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--repo", required=True)
    ap.add_argument("--output", required=True)
    ap.add_argument("--limit", type=int, default=10, help="newest releases to list")
    args = ap.parse_args()
    items = collect(args.repo, args.limit)
    if not items:
        sys.exit("no release carries a sparkle.json; refusing to publish an empty appcast")
    with open(args.output, "w", encoding="utf-8") as f:
        f.write(render(items))
    for m in sorted(items, key=lambda m: int(m["version"]), reverse=True):
        print(f"{m['channel']:8} {m['version']:>6} {m['short_version']}  ({m['tag']})")


if __name__ == "__main__":
    main()
