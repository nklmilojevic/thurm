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

    func testNamedKeysMatchLinux() {
        XCTAssertEqual(parse("cmd++")!.0, "+")
        XCTAssertEqual(parse("cmd+plus")!.0, "+")
        XCTAssertEqual(parse("cmd+enter")!.0, "\r")
        XCTAssertEqual(parse("cmd+left_bracket")!.0, "[")
        XCTAssertEqual(parse("cmd+left")!.0, "\u{F702}")
        XCTAssertEqual(parse("f5")!.0, String(UnicodeScalar(UInt32(NSF5FunctionKey))!))
    }

    /// A command's key comes before a built-in item's, which has it back once no command does.
    func testCommandKeysDisplaceBuiltInOnes() {
        let main = NSMenu(title: "Main")
        let shell = NSMenu(title: "Shell")
        let split = NSMenuItem(title: "Split Down", action: nil, keyEquivalent: "d")
        split.keyEquivalentModifierMask = [.command, .shift]
        let tab = NSMenuItem(title: "New Tab", action: nil, keyEquivalent: "t")
        tab.keyEquivalentModifierMask = [.command]
        shell.addItem(split)
        shell.addItem(tab)
        let container = NSMenuItem(title: "Shell", action: nil, keyEquivalent: "")
        container.submenu = shell
        main.addItem(container)

        MainMenu.updateCommands([UserCommand(name: "Layout", key: "cmd+shift+d")], in: main)
        XCTAssertEqual(split.keyEquivalent, "")
        XCTAssertEqual(tab.keyEquivalent, "t")

        MainMenu.updateCommands([UserCommand(name: "Tab", key: "cmd+t")], in: main)
        XCTAssertEqual(split.keyEquivalent, "d")
        XCTAssertEqual(split.keyEquivalentModifierMask, [.command, .shift])
        XCTAssertEqual(tab.keyEquivalent, "")

        MainMenu.updateCommands([], in: main)
        XCTAssertEqual(tab.keyEquivalent, "t")
    }

    func testUnknownKeysAndModifiersAreRejected() {
        XCTAssertNil(parse(""))
        XCTAssertNil(parse("cmd+"))
        XCTAssertNil(parse("hyper+k"))
        XCTAssertNil(parse("cmd+launchpad"))
    }
}
