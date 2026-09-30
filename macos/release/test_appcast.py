"""Tests for appcast.py: python3 -m unittest discover -s macos/release"""

import unittest
import xml.etree.ElementTree as ET

import appcast

NS = {"sparkle": appcast.SPARKLE_NS}


def meta(channel, version, short, notes=""):
    return {
        "channel": channel,
        "version": str(version),
        "short_version": short,
        "commit": "0123456789abcdef",
        "url": f"https://example.com/Thurm-{version}.zip",
        "length": 1234,
        "ed_signature": "c2lnbmF0dXJl",
        "minimum_system_version": "14.0",
        "published": "Mon, 28 Sep 2026 10:00:00 +0000",
        "notes": notes,
        "tag": "tip" if channel == "tip" else f"v{short}",
    }


class RenderTest(unittest.TestCase):
    def parse(self, items):
        return ET.fromstring(appcast.render(items)).findall("./channel/item")

    def test_newest_version_first_and_channels(self):
        items = self.parse([
            meta("release", 40, "0.2.0"),
            meta("tip", 57, "0.2.0-tip.abc1234"),
            meta("release", 12, "0.1.0"),
        ])
        versions = [i.find("sparkle:version", NS).text for i in items]
        self.assertEqual(versions, ["57", "40", "12"])
        channels = [getattr(i.find("sparkle:channel", NS), "text", None) for i in items]
        # Releases are in the default channel, which every app accepts.
        self.assertEqual(channels, ["tip", None, None])

    def test_tip_goes_first_when_a_release_has_the_same_version(self):
        items = self.parse([
            meta("release", 78, "0.1.0"),
            meta("tip", 78, "0.1.0-tip.2749235"),
            meta("tip", 77, "0.1.0-tip.bb1eb59"),
        ])
        order = [(i.find("sparkle:version", NS).text,
                  getattr(i.find("sparkle:channel", NS), "text", None)) for i in items]
        self.assertEqual(order, [("78", "tip"), ("78", None), ("77", "tip")])

    def test_enclosure_and_requirements(self):
        (item,) = self.parse([meta("release", 40, "0.2.0")])
        enc = item.find("enclosure")
        self.assertEqual(enc.get("url"), "https://example.com/Thurm-40.zip")
        self.assertEqual(enc.get("length"), "1234")
        self.assertEqual(enc.get(f"{{{appcast.SPARKLE_NS}}}edSignature"), "c2lnbmF0dXJl")
        self.assertEqual(item.find("sparkle:minimumSystemVersion", NS).text, "14.0.0")
        self.assertEqual(item.find("sparkle:hardwareRequirements", NS).text, "arm64")

    def test_notes_are_markdown_and_survive_cdata_terminators(self):
        (item,) = self.parse([meta("release", 40, "0.2.0", notes="## Fixed\n- a ]]> b <c>")])
        desc = item.find("description")
        self.assertEqual(desc.get(f"{{{appcast.SPARKLE_NS}}}format"), "markdown")
        self.assertEqual(desc.text, "## Fixed\n- a ]]> b <c>")

    def test_tip_without_notes_names_its_commit(self):
        (item,) = self.parse([meta("tip", 57, "0.2.0-tip.abc1234")])
        self.assertEqual(item.find("description").text, "Built from `0123456`.")


if __name__ == "__main__":
    unittest.main()
