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

    func testValuesAreEditedInTheFileThatSetsThem() throws {
        let doc = try document()
        let item = { (key: String) in try XCTUnwrap(doc.settings.first { $0.key == key }) }
        XCTAssertEqual(doc.editPath(for: try item("font.family")), doc.configPath)
        XCTAssertEqual(doc.editPath(for: try item("font.size")), doc.overlayPath)
        XCTAssertEqual(doc.editPath(for: try item("window.tab_style")), doc.overlayPath)
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

    func testNumbersTypedIntoFieldsParseOnlyWhenFinite() {
        XCTAssertEqual(SettingNumber.parse("0.125"), 0.125)
        XCTAssertEqual(SettingNumber.parse(" 2,5 "), 2.5)
        XCTAssertNil(SettingNumber.parse("inf"))
        XCTAssertNil(SettingNumber.parse("-inf"))
        XCTAssertNil(SettingNumber.parse("nan"))
        XCTAssertNil(SettingNumber.parse("1e400"))
        XCTAssertNil(SettingNumber.parse(""))
        XCTAssertNil(SettingNumber.parse("ten"))
    }

    func testFloatsKeepTheirPrecisionAndIntegersRound() throws {
        XCTAssertEqual(try XCTUnwrap(SettingNumber.value(0.125, kind: .float)) as? Double, 0.125)
        XCTAssertEqual(try XCTUnwrap(SettingNumber.value(2.5, kind: .int)) as? Int, 3)
        XCTAssertEqual(try XCTUnwrap(SettingNumber.value(16, kind: .int)) as? Int, 16)
    }

    func testNumbersThatCannotBeSavedAreRefused() {
        XCTAssertNil(SettingNumber.value(.infinity, kind: .float))
        XCTAssertNil(SettingNumber.value(.nan, kind: .int))
        XCTAssertNil(SettingNumber.value(1e30, kind: .int))
        XCTAssertNil(SettingNumber.value(-1e30, kind: .int))
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

    func testSummariesKeepTheFirstSentenceAndCutLongOnes() {
        XCTAssertEqual(SettingText.summary("Font size in points. Larger values read better."), "Font size in points.")
        XCTAssertEqual(SettingText.summary("  No period here  "), "No period here")
        XCTAssertEqual(SettingText.summary(""), "")
        let long = String(repeating: "word ", count: 40)
        let cut = SettingText.summary(long)
        XCTAssertTrue(cut.hasSuffix("word\u{2026}"))
        XCTAssertLessThanOrEqual(cut.count, SettingText.summaryLimit + 1)
    }

    func testOptionTitlesReadAsLabels() {
        XCTAssertEqual(SettingText.optionTitle("copy-paste"), "Copy paste")
        XCTAssertEqual(SettingText.optionTitle("sidebar"), "Sidebar")
    }

    func testSubgroupsFollowTheKeysTables() throws {
        let appearance = [
            item("cursor.style", section: "appearance"),
            item("colors.theme", section: "appearance"),
            item("colors.foreground", section: "appearance"),
            item("window.opacity", section: "appearance"),
        ]
        let groups = SettingGroups.split(appearance)
        XCTAssertEqual(groups.map(\.heading), [nil, "Theme", "Colors", "Cursor"])
        XCTAssertEqual(groups[0].items.map(\.key), ["window.opacity"])
        XCTAssertEqual(groups[1].items.map(\.key), ["colors.theme"])
        let notifications = try document().settings.filter { $0.section == "notifications" }
        let matrixGroups = SettingGroups.split(notifications)
        XCTAssertEqual(matrixGroups.map(\.heading), [nil, "States"])
        XCTAssertFalse(matrixGroups[0].isMatrix)
        XCTAssertTrue(matrixGroups[1].isMatrix)
    }

    func testEverySectionHasAnIcon() {
        for id in ["appearance", "font", "window", "terminal", "session", "security", "notifications",
                   "agents", "ai", "programs", "updates", "quick_terminal", "keys", "remotes"] {
            XCTAssertNotEqual(SettingsSectionIcons.symbol(for: id), "gearshape", id)
        }
        XCTAssertEqual(SettingsSectionIcons.symbol(for: "unknown"), "gearshape")
    }

    func testSystemSoundsAreTheSoundFilesByName() throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("thurm-sounds-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        for file in ["Glass.aiff", "Basso.aiff", "Ping.WAV", ".DS_Store", "README.txt"] {
            FileManager.default.createFile(atPath: directory.appendingPathComponent(file).path, contents: nil)
        }
        try FileManager.default.createDirectory(at: directory.appendingPathComponent("Pack.aiff"),
                                                withIntermediateDirectories: false)
        XCTAssertEqual(SystemSounds.names(in: [directory.path]), ["Basso", "Glass", "Ping"])
    }

    func testListsWithACommaInAnItemAreNotEditedAsText() {
        XCTAssertEqual(SettingList.parse(" a, b ,,c "), ["a", "b", "c"])
        XCTAssertTrue(SettingList.isEditableAsText(["a", "b"]))
        XCTAssertFalse(SettingList.isEditableAsText(["a,b", "c"]))
        XCTAssertFalse(SettingList.isEditableAsText([" a", "b"]))
        XCTAssertFalse(SettingList.isEditableAsText(["a", "", "b"]))
        XCTAssertTrue(SettingList.isEditableAsText(nil))
    }

    func testAnItemKnowsWhatConfigTomlSetsForIt() throws {
        let items = try XCTUnwrap(SettingsDocument(json: """
        {"config_path": "", "overlay_path": "", "config_read_only": false, "overlay_exists": true,
         "sections": [{"id": "appearance", "label": "Appearance"}],
         "settings": [
           {"key": "colors.cursor", "section": "appearance", "label": "Cursor", "kind": "color",
            "value": "#ff0000", "default": null, "source": "overlay", "file_value": "#00ff00", "doc": ""},
           {"key": "colors.background", "section": "appearance", "label": "Background", "kind": "color",
            "value": null, "default": null, "source": "default", "file_value": null, "doc": ""}
         ], "warnings": [], "error": null}
        """)).settings
        XCTAssertEqual(items[0].fileValue as? String, "#00ff00")
        XCTAssertNil(items[1].fileValue)
    }

    func testNoticeSaysWhatTheFilesMean() throws {
        let doc = try document()
        let notice = try XCTUnwrap(doc.notice)
        XCTAssertFalse(notice.isError)
        XCTAssertTrue(notice.text.contains("read-only"))
        XCTAssertTrue(notice.text.contains("font.famliy"))
        let broken = try XCTUnwrap(SettingsDocument(json: """
        {"config_path": "", "overlay_path": "", "config_read_only": false, "overlay_exists": false,
         "sections": [], "settings": [], "warnings": [], "error": "bad line"}
        """))
        XCTAssertEqual(broken.notice?.isError, true)
        let clean = try XCTUnwrap(SettingsDocument(json: """
        {"config_path": "", "overlay_path": "", "config_read_only": false, "overlay_exists": false,
         "sections": [], "settings": [], "warnings": [], "error": null}
        """))
        XCTAssertNil(clean.notice)
    }

    private func item(_ key: String, section: String) -> SettingItem {
        SettingItem(json: ["key": key, "section": section, "label": key, "kind": "text",
                           "value": "", "default": "", "source": "default", "doc": ""])!
    }
}

final class SettingsStoreNavigationTests: XCTestCase {
    func testPickingASectionLeavesTheSearch() {
        let store = SettingsStore()
        store.selectedSection = "font"
        store.query = "blink"
        XCTAssertNil(store.selectedSection)
        store.selectedSection = "font"
        XCTAssertEqual(store.query, "")
        XCTAssertEqual(store.selectedSection, "font")
    }

    func testClearingTheSearchReturnsToTheSectionBeforeIt() {
        let store = SettingsStore()
        store.selectedSection = "window"
        store.query = "size"
        store.query = ""
        XCTAssertEqual(store.selectedSection, "window")
    }
}

final class RemoteHostEntryTests: XCTestCase {
    func testEntriesReadTheRemoteValueWithItsDefaults() {
        let value = JSON.decode("""
        [{"name": "devbox", "host": "me@devbox", "enabled": false, "clipboard_read": "never"},
         {"name": "pi", "host": "pi"},
         {"host": "nameless"}]
        """)
        XCTAssertEqual(RemoteHostEntry.entries(value), [
            RemoteHostEntry(name: "devbox", target: "me@devbox", enabled: false, clipboardRead: "never"),
            RemoteHostEntry(name: "pi", target: "pi"),
        ])
    }

    func testAnUnsetValueHasNoHosts() {
        XCTAssertEqual(RemoteHostEntry.entries(NSNull()), [])
        XCTAssertEqual(RemoteHostEntry.entries(nil), [])
    }
}
