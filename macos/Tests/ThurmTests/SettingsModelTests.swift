import XCTest
@testable import Thurm

final class SettingsModelTests: XCTestCase {
    private let sample = """
    {
      "config_path": "/Users/me/.config/thurm/config.toml",
      "overlay_path": "/Users/me/.config/thurm/settings.toml",
      "config_read_only": true,
      "overlay_exists": true,
      "sections": [
        {"id": "font", "label": "Font"},
        {"id": "window", "label": "Window"},
        {"id": "notifications", "label": "Notifications"}
      ],
      "settings": [
        {"key": "font.size", "section": "font", "label": "Size", "kind": "float",
         "value": 15.0, "default": 16.0, "source": "overlay", "doc": "Font size in points.",
         "min": 4.0, "max": 200.0},
        {"key": "font.family", "section": "font", "label": "Family", "kind": "font",
         "value": "Menlo", "default": "JetBrains Mono", "source": "file", "doc": ""},
        {"key": "window.tab_style", "section": "window", "label": "Tabs", "kind": "enum",
         "value": "sidebar", "default": "sidebar", "source": "default", "doc": "Tabs in the titlebar.",
         "options": ["native", "sidebar"]},
        {"key": "notifications.bell_badge", "section": "notifications", "label": "Bell badge",
         "kind": "bool", "value": true, "default": true, "source": "default", "doc": ""},
        {"key": "notifications.waiting.notify", "section": "notifications", "label": "Waiting: Notify",
         "kind": "bool", "value": false, "default": true, "source": "overlay", "doc": "",
         "row": "waiting", "column": "notify"},
        {"key": "notifications.waiting.sound", "section": "notifications", "label": "Waiting: Sound",
         "kind": "text", "value": "default", "default": "default", "source": "overlay", "doc": "",
         "row": "waiting", "column": "sound"}
      ],
      "warnings": ["settings.toml: font.famliy is not a setting, ignored"],
      "error": null
    }
    """

    private func document() throws -> SettingsDocument {
        try XCTUnwrap(SettingsDocument(json: sample))
    }

    func testDecodesTheFilesSectionsAndSettings() throws {
        let doc = try document()
        XCTAssertTrue(doc.configReadOnly)
        XCTAssertTrue(doc.overlayExists)
        XCTAssertEqual(doc.overlayPath, "/Users/me/.config/thurm/settings.toml")
        XCTAssertNil(doc.error)
        XCTAssertEqual(doc.warnings.count, 1)
        XCTAssertEqual(doc.sections.map(\.id), ["font", "window", "notifications"])
        XCTAssertEqual(doc.label(ofSection: "window"), "Window")
        XCTAssertEqual(doc.settings.count, 6)
    }

    func testValueSourcesAndOverrides() throws {
        let doc = try document()
        let size = try XCTUnwrap(doc.settings.first { $0.key == "font.size" })
        XCTAssertEqual(size.source, .overlay)
        XCTAssertEqual(size.source.label, "settings.toml")
        XCTAssertTrue(size.isOverridden)
        XCTAssertEqual(size.min, 4)
        XCTAssertEqual(size.max, 200)
        XCTAssertEqual(size.text, "15")

        let family = try XCTUnwrap(doc.settings.first { $0.key == "font.family" })
        XCTAssertEqual(family.source.label, "config.toml")
        XCTAssertFalse(family.isOverridden)
        XCTAssertEqual(family.text, "Menlo")

        let tabs = try XCTUnwrap(doc.settings.first { $0.key == "window.tab_style" })
        XCTAssertEqual(tabs.source.label, "Default")
        XCTAssertEqual(tabs.options, ["native", "sidebar"])
    }

    func testMatrixCellsKnowTheirRowAndColumn() throws {
        let doc = try document()
        let cell = try XCTUnwrap(doc.settings.first { $0.key == "notifications.waiting.notify" })
        XCTAssertEqual(cell.row, "waiting")
        XCTAssertEqual(cell.column, "notify")
        XCTAssertNil(doc.settings.first { $0.key == "font.size" }?.row)
    }

    func testSearchMatchesEveryWordAcrossLabelKeyCommentAndSection() throws {
        let doc = try document()
        func keys(_ query: String) -> [String] {
            SettingsSearch.matches(doc.settings, query: query, sectionLabel: doc.label(ofSection:)).map(\.key)
        }
        XCTAssertEqual(keys(""), doc.settings.map(\.key))
        XCTAssertEqual(keys("  "), doc.settings.map(\.key))
        XCTAssertEqual(keys("FONT"), ["font.size", "font.family"])
        XCTAssertEqual(keys("points"), ["font.size"])
        XCTAssertEqual(keys("tabs titlebar"), ["window.tab_style"])
        XCTAssertEqual(keys("tabs titlebar sidebar"), [])
        XCTAssertEqual(keys("waiting sound"), ["notifications.waiting.sound"])
        XCTAssertEqual(keys("notifications.bell"), ["notifications.bell_badge"])
    }

    func testGroupsKeepSectionOrderAndDropEmptySections() throws {
        let doc = try document()
        XCTAssertEqual(doc.groups(matching: "").map(\.section.id), ["font", "window", "notifications"])
        XCTAssertEqual(doc.groups(matching: "badge").map(\.section.id), ["notifications"])
        XCTAssertTrue(doc.groups(matching: "zzz").isEmpty)
    }

    func testMalformedDocumentIsRefused() {
        XCTAssertNil(SettingsDocument(json: "[1, 2]"))
        XCTAssertNil(SettingsDocument(json: "not json"))
    }

    func testThemeSpecsParseLikeTheConfig() {
        var pair = ColorThemeSpec.parse("light:catppuccin-latte,dark:catppuccin-mocha")
        XCTAssertEqual(pair.light, "catppuccin-latte")
        XCTAssertEqual(pair.dark, "catppuccin-mocha")
        pair = ColorThemeSpec.parse("nord")
        XCTAssertEqual(pair.light, "nord")
        XCTAssertEqual(pair.dark, "nord")
        pair = ColorThemeSpec.parse("dark:dracula")
        XCTAssertEqual(pair.light, "dracula")
        XCTAssertEqual(pair.dark, "dracula")
        pair = ColorThemeSpec.parse("")
        XCTAssertEqual(pair.dark, "thurm")
        XCTAssertEqual(ColorThemeSpec.encode(light: "nord", dark: "nord"), "nord")
        XCTAssertEqual(ColorThemeSpec.encode(light: "a", dark: "b"), "light:a,dark:b")
    }

    func testHexColorsParseAndFormat() throws {
        let color = try XCTUnwrap(HexColor.parse("#0e1116"))
        XCTAssertEqual(color.red, 0x0e)
        XCTAssertEqual(color.green, 0x11)
        XCTAssertEqual(color.blue, 0x16)
        XCTAssertEqual(try XCTUnwrap(HexColor.parse("0xffffff")).red, 255)
        XCTAssertEqual(try XCTUnwrap(HexColor.parse("#abc")).green, 0xbb)
        XCTAssertNil(HexColor.parse("#12345"))
        XCTAssertNil(HexColor.parse("red"))
        XCTAssertEqual(HexColor.format(red: 14, green: 17, blue: 22), "#0e1116")
    }
}
