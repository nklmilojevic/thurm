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
}
