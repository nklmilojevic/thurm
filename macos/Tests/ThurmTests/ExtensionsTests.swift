import XCTest
@testable import Thurm

final class ExtensionsTests: XCTestCase {
    /// Without the daemon's actions the menu has the config's commands; with them, every
    /// action but the app's own, and the daemon's bindings of the app's actions join
    /// `[keybindings]`.
    func testTheDaemonsActionsBuildTheCommandsMenu() {
        let ext = Extensions()
        let config = [UserCommand(name: "Deploy", key: "cmd+shift+d")]
        let (keys, commands) = ext.menuInput(keybindings: ["cmd+k": "new_tab"], commands: config)
        XCTAssertEqual(keys, ["cmd+k": "new_tab"])
        XCTAssertEqual(commands, config)

        ext.load(actions: [
            .init(name: "Deploy", title: "", category: "Commands", source: "config", hidden: false),
            .init(name: "git.sync", title: "Sync", category: "Git", source: "plugin:git", hidden: false),
            .init(name: "init.key.1", title: "cmd+u", category: "", source: "plugin:init", hidden: true),
            .init(name: "init.key.2", title: "", category: "", source: "plugin:init", hidden: true),
            .init(name: "split_down", title: "Split Down", category: "Shell", source: "Thurm.app", hidden: false),
        ], bindings: [
            .init(key: "cmd+shift+d", action: "Deploy"),
            .init(key: "cmd+u", action: "init.key.1"),
            .init(key: "cmd+shift+j", action: "split_down"),
        ], appActions: ["split_down"])
        let (merged, entries) = ext.menuInput(keybindings: ["cmd+k": "new_tab"], commands: config)
        XCTAssertEqual(merged, ["cmd+k": "new_tab", "cmd+shift+j": "split_down"])
        XCTAssertEqual(entries.map(\.name), ["Deploy", "git.sync", "init.key.1"])
        XCTAssertEqual(entries[0].key, "cmd+shift+d")
        XCTAssertEqual(entries[1].displayTitle, "Sync")
        XCTAssertEqual(entries[1].category, "Git")
        XCTAssertTrue(entries[2].hidden)
    }

    /// A second chord of an action, and a key with arguments, each get a hidden entry that
    /// runs the action (with those arguments); the visible entry keeps the first plain key.
    func testEveryKeyOfAnActionRunsIt() {
        let ext = Extensions()
        ext.load(actions: [
            .init(name: "git.checkout", title: "Checkout", category: "Git", source: "plugin:git", hidden: false),
        ], bindings: [
            .init(key: "cmd+shift+o", action: "git.checkout"),
            .init(key: "cmd+alt+o", action: "git.checkout"),
            .init(key: "cmd+alt+m", action: "git.checkout", args: "{\"branch\":\"main\"}"),
        ], appActions: [])
        let (_, entries) = ext.menuInput(keybindings: [:], commands: [])
        XCTAssertEqual(entries.map(\.key), ["cmd+shift+o", "cmd+alt+o", "cmd+alt+m"])
        XCTAssertEqual(entries.map(\.hidden), [false, true, true])
        XCTAssertEqual(entries.map(\.args), [nil, nil, "{\"branch\":\"main\"}"])
        XCTAssertTrue(entries.allSatisfy { $0.name == "git.checkout" })
    }

    /// A key bound with arguments to an app action (which takes none) is not bound.
    func testAppActionKeysWithArgumentsAreNotBound() {
        let ext = Extensions()
        ext.load(actions: [], bindings: [
            .init(key: "cmd+shift+j", action: "split_down", args: "{\"x\":1}"),
            .init(key: "cmd+shift+k", action: "split_down"),
        ], appActions: ["split_down"])
        let (keys, _) = ext.menuInput(keybindings: [:], commands: [])
        XCTAssertEqual(keys, ["cmd+shift+k": "split_down"])
    }

    func testSecureKeyboardEntryIsNotOffered() {
        XCTAssertTrue(Extensions.personalActions.contains("secure_keyboard_entry"))
    }

    /// An action for a pane goes to its window's responder chain, not the key window's; one
    /// nothing there takes is not sent.
    func testAnActionForAPaneGoesToItsWindow() {
        _ = NSApplication.shared
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 100, height: 100),
                              styleMask: [.titled], backing: .buffered, defer: true)
        let view = PaneActionView(frame: window.contentLayoutRect)
        window.contentView = view
        window.makeFirstResponder(view)
        let item = NSMenuItem(title: "Split", action: #selector(PaneActionView.splitForTest(_:)),
                              keyEquivalent: "")
        XCTAssertTrue(Extensions.target(for: #selector(PaneActionView.splitForTest(_:)),
                                        item: item, in: window) === view)
        let other = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 100, height: 100),
                             styleMask: [.titled], backing: .buffered, defer: true)
        XCTAssertNil(Extensions.target(for: #selector(PaneActionView.splitForTest(_:)),
                                       item: item, in: other))
        XCTAssertNil(Extensions.target(for: #selector(PaneActionView.splitForTest(_:)),
                                       item: item, in: nil))
    }
}

private final class PaneActionView: NSView {
    override var acceptsFirstResponder: Bool { true }
    @objc func splitForTest(_ sender: Any?) {}
}
