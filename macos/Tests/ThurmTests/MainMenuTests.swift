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

    func testUnknownKeysAndModifiersAreRejected() {
        XCTAssertNil(parse(""))
        XCTAssertNil(parse("cmd+"))
        XCTAssertNil(parse("hyper+k"))
        XCTAssertNil(parse("cmd+launchpad"))
    }
}
