import AppKit
import XCTest
@testable import Thurm

final class MainMenuTests: XCTestCase {
    private func parse(_ chord: String) -> (String, NSEvent.ModifierFlags)? {
        MainMenu.keyEquivalent(chord)
    }

    func testLettersAndModifiers() {
        let (key, mods) = parse("cmd+shift+L")!
        XCTAssertEqual(key, "l")
        XCTAssertEqual(mods, [.command, .shift])
        XCTAssertEqual(parse("ctrl+alt+1")!.1, [.control, .option])
        XCTAssertEqual(parse(" option + k ")!.0, "k")
    }

    func testNamedKeys() {
        XCTAssertEqual(parse("cmd++")!.0, "+")
        XCTAssertEqual(parse("cmd+plus")!.0, "+")
        XCTAssertEqual(parse("cmd+enter")!.0, "\r")
        XCTAssertEqual(parse("cmd+left_bracket")!.0, "[")
        XCTAssertEqual(parse("cmd+left")!.0, "\u{F702}")
        XCTAssertEqual(parse("f5")!.0, String(UnicodeScalar(UInt32(NSF5FunctionKey))!))
    }

    private func menuItem(_ title: String, _ key: String, _ mods: NSEvent.ModifierFlags,
                          name: String) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: key)
        item.keyEquivalentModifierMask = mods
        item.identifier = MainMenu.actionIdentifier(name)
        return item
    }

    /// A Shell menu with Split Right (⌘D), Split Down (⇧⌘D) and New Tab (⌘T).
    private func shellMenu() -> (NSMenu, NSMenu, right: NSMenuItem, down: NSMenuItem, tab: NSMenuItem) {
        let main = NSMenu(title: "Main")
        let shell = NSMenu(title: "Shell")
        let right = menuItem("Split Right", "d", [.command], name: "split_right")
        let down = menuItem("Split Down", "d", [.command, .shift], name: "split_down")
        let tab = menuItem("New Tab", "t", [.command], name: "new_tab")
        for item in [right, down, tab] { shell.addItem(item) }
        let container = NSMenuItem(title: "Shell", action: nil, keyEquivalent: "")
        container.submenu = shell
        main.addItem(container)
        return (main, shell, right, down, tab)
    }

    override func tearDown() {
        MainMenu.applyKeys([:], commands: [], in: nil)
        super.tearDown()
    }

    /// A command's key comes before a built-in item's, which has it back once no command does.
    func testCommandKeysDisplaceBuiltInOnes() {
        let (main, _, _, split, tab) = shellMenu()

        MainMenu.applyKeys([:], commands: [UserCommand(name: "Layout", key: "cmd+shift+d")], in: main)
        XCTAssertEqual(split.keyEquivalent, "")
        XCTAssertEqual(tab.keyEquivalent, "t")

        MainMenu.applyKeys([:], commands: [UserCommand(name: "Tab", key: "cmd+t")], in: main)
        XCTAssertEqual(split.keyEquivalent, "d")
        XCTAssertEqual(split.keyEquivalentModifierMask, [.command, .shift])
        XCTAssertEqual(tab.keyEquivalent, "")

        MainMenu.applyKeys([:], commands: [], in: main)
        XCTAssertEqual(tab.keyEquivalent, "t")
    }

    /// A binding moves the combination to its action, shown on its item; the key that item had
    /// stays on a hidden copy, and everything is back once the binding is gone.
    func testBindingMovesAKeyToAnAction() {
        let (main, shell, right, down, tab) = shellMenu()

        let warnings = MainMenu.applyKeys(["cmd+shift+d": "split_right"], commands: [], in: main)
        XCTAssertEqual(warnings, [])
        XCTAssertEqual(right.keyEquivalent, "d")
        XCTAssertEqual(right.keyEquivalentModifierMask, [.command, .shift])
        XCTAssertEqual(down.keyEquivalent, "")
        XCTAssertEqual(tab.keyEquivalent, "t")
        let extra = shell.items[1]
        XCTAssertTrue(extra.isHidden)
        XCTAssertTrue(extra.allowsKeyEquivalentWhenHidden)
        XCTAssertEqual(extra.identifier, MainMenu.actionIdentifier("split_right"))
        XCTAssertEqual(extra.keyEquivalent, "d")
        XCTAssertEqual(extra.keyEquivalentModifierMask, [.command])

        MainMenu.applyKeys([:], commands: [], in: main)
        XCTAssertEqual(shell.items.count, 3)
        XCTAssertEqual(right.keyEquivalentModifierMask, [.command])
        XCTAssertEqual(down.keyEquivalent, "d")
        XCTAssertEqual(down.keyEquivalentModifierMask, [.command, .shift])
    }

    func testBindingToNoneRemovesAKey() {
        let (main, _, right, down, tab) = shellMenu()
        MainMenu.applyKeys(["cmd+t": "none", "CMD+D": " none "], commands: [], in: main)
        XCTAssertEqual(tab.keyEquivalent, "")
        XCTAssertEqual(right.keyEquivalent, "")
        XCTAssertEqual(down.keyEquivalent, "d")
    }

    /// An item without a key just takes the bound one; a command still comes first.
    func testBindingAnItemWithoutAKeyAndCommandsAfterBindings() {
        let (main, shell, _, _, tab) = shellMenu()
        let rename = menuItem("Rename Workspace…", "", [], name: "rename_workspace")
        shell.addItem(rename)
        MainMenu.applyKeys(["cmd+shift+r": "rename_workspace", "cmd+n": "new_tab"],
                           commands: [UserCommand(name: "Reload", key: "cmd+shift+r")], in: main)
        XCTAssertEqual(rename.keyEquivalent, "")
        XCTAssertEqual(tab.keyEquivalent, "n")
        XCTAssertEqual(shell.items.filter { $0.isHidden }.map(\.keyEquivalent), ["t"])
    }

    func testBadBindingsWarnAndLeaveDefaults() {
        let (main, _, right, _, tab) = shellMenu()
        let warnings = MainMenu.applyKeys(["cmd+d": "split_sideways", "hyper+d": "split_right", "cmd+t": " "],
                                          commands: [], in: main)
        XCTAssertEqual(warnings, ["keybindings: unknown action \"split_sideways\"",
                                  "keybindings: unknown action \"\"",
                                  "keybindings: cannot read \"hyper+d\""])
        XCTAssertEqual(right.keyEquivalent, "d")
        XCTAssertEqual(tab.keyEquivalent, "t")
    }

    /// The action names of the visible items under `menu`.
    private func actionNames(in menu: NSMenu) -> [String] {
        menu.items.flatMap { item -> [String] in
            let sub = item.submenu.map { actionNames(in: $0) } ?? []
            let prefix = MainMenu.actionIdentifier("").rawValue
            guard !item.isHidden, let id = item.identifier?.rawValue, id.hasPrefix(prefix) else { return sub }
            return [String(id.dropFirst(prefix.count))] + sub
        }
    }

    /// Each action names one visible menu item, and the configuration docs list them all.
    func testActionNamesMatchTheDocs() throws {
        _ = NSApplication.shared
        let names = actionNames(in: MainMenu.build())
        XCTAssertEqual(names.count, Set(names).count)
        let docs = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().appendingPathComponent("docs/src/content/docs/configuration.md")
        let text = try String(contentsOf: docs, encoding: .utf8)
        let section = try XCTUnwrap(text.components(separatedBy: "## Key bindings").last?
            .components(separatedBy: "\n## ").first)
        let listed = section.split(separator: "\n").filter { $0.hasPrefix("| `") }
            .map { String($0.split(separator: "`")[1]) }
        XCTAssertEqual(names, listed)
    }

    func testUnknownKeysAndModifiersAreRejected() {
        XCTAssertNil(parse(""))
        XCTAssertNil(parse("cmd+"))
        XCTAssertNil(parse("hyper+k"))
        XCTAssertNil(parse("cmd+launchpad"))
    }

    /// Digits follow key positions: the digit row whatever it types, and the keypad, but not F20.
    func testDigitKeysByPosition() {
        XCTAssertEqual(MainMenu.digitKeys[0x12], "1")
        XCTAssertEqual(MainMenu.digitKeys[0x17], "5")
        XCTAssertEqual(MainMenu.digitKeys[0x1C], "8")
        XCTAssertEqual(MainMenu.digitKeys[0x19], "9")
        XCTAssertEqual(MainMenu.digitKeys[0x1D], "0")
        XCTAssertEqual(MainMenu.digitKeys[0x53], "1")
        XCTAssertEqual(MainMenu.digitKeys[0x59], "7")
        XCTAssertEqual(MainMenu.digitKeys[0x5B], "8")
        XCTAssertEqual(MainMenu.digitKeys[0x5C], "9")
        XCTAssertNil(MainMenu.digitKeys[0x5A])
        XCTAssertNil(MainMenu.digitKeys[0x0C])
    }

    /// ⌘1 finds Select Tab 1, the item a binding moved the combination to, or nothing for "none".
    func testDigitLookupFollowsBindings() {
        _ = NSApplication.shared
        let main = MainMenu.build()
        let select = MainMenu.actionIdentifier("select_tab_1")

        MainMenu.applyKeys([:], commands: [], in: main)
        XCTAssertEqual(MainMenu.keyedItem("1", [.command], in: main)?.identifier, select)
        XCTAssertNil(MainMenu.keyedItem("1", [.command, .shift], in: main))

        MainMenu.applyKeys(["cmd+1": "new_tab"], commands: [], in: main)
        XCTAssertEqual(MainMenu.keyedItem("1", [.command], in: main)?.identifier, MainMenu.actionIdentifier("new_tab"))

        MainMenu.applyKeys(["cmd+1": "none"], commands: [], in: main)
        XCTAssertNil(MainMenu.keyedItem("1", [.command], in: main))
    }

    private final class Receiver: NSObject, NSMenuItemValidation {
        var enabled = true
        var calls = 0
        @objc func fire(_ sender: Any?) { calls += 1 }
        func validateMenuItem(_ menuItem: NSMenuItem) -> Bool { enabled }
    }

    private func digitEvent(_ code: UInt16, _ typed: String, _ mods: NSEvent.ModifierFlags) -> NSEvent {
        NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: mods, timestamp: 0,
                         windowNumber: 0, context: nil, characters: typed,
                         charactersIgnoringModifiers: typed, isARepeat: false, keyCode: code)!
    }

    /// AZERTY ⌘1 types "&": the fallback fires the item bound to ⌘1 once, unless its validation
    /// disables it, the key is unbound or other modifiers are held.
    func testPhysicalFallbackDispatchesValidatedItem() {
        let app = NSApplication.shared
        let saved = app.mainMenu
        defer { app.mainMenu = saved }
        let receiver = Receiver()
        let item = NSMenuItem(title: "Select Tab 1", action: #selector(Receiver.fire(_:)), keyEquivalent: "1")
        item.keyEquivalentModifierMask = [.command]
        item.target = receiver
        let sub = NSMenu(title: "Window")
        sub.addItem(item)
        let main = NSMenu()
        main.addItem(NSMenuItem(title: "Window", action: nil, keyEquivalent: ""))
        main.items[0].submenu = sub
        app.mainMenu = main

        XCTAssertTrue(MainMenu.physicalFallback(digitEvent(0x12, "&", [.command])))
        XCTAssertEqual(receiver.calls, 1)
        XCTAssertTrue(MainMenu.physicalFallback(digitEvent(0x53, "1", [.command])))
        XCTAssertEqual(receiver.calls, 2)

        XCTAssertFalse(MainMenu.physicalFallback(digitEvent(0x12, "1", [.command, .shift])))
        XCTAssertFalse(MainMenu.physicalFallback(digitEvent(0x13, "é", [.command])))

        receiver.enabled = false
        XCTAssertFalse(MainMenu.physicalFallback(digitEvent(0x12, "&", [.command])))
        sub.autoenablesItems = false
        receiver.enabled = true
        item.isEnabled = false
        XCTAssertFalse(MainMenu.physicalFallback(digitEvent(0x12, "&", [.command])))
        XCTAssertEqual(receiver.calls, 2)

        item.isEnabled = true
        item.keyEquivalent = ""
        XCTAssertFalse(MainMenu.physicalFallback(digitEvent(0x12, "&", [.command])))
        XCTAssertEqual(receiver.calls, 2)
    }
}
