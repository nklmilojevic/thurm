import AppKit
import XCTest
@testable import Thurm

/// A tab that becomes key types into its terminal, whatever it was left focusing.
final class TabFocusTests: XCTestCase {
    private let manager = SessionManager.shared

    override func tearDown() {
        for c in manager.liveControllers {
            c.closingWithoutConfirmation = true
            c.window?.close()
        }
        super.tearDown()
    }

    private func becomeKey(_ c: TerminalWindowController) {
        c.windowDidBecomeKey(Notification(name: NSWindow.didBecomeKeyNotification, object: c.window))
    }

    private func controller() throws -> (TerminalWindowController, NSWindow) {
        let pane = PaneKey(localHost, 1)
        let c = manager.makeController(root: .leaf(pane), focused: pane, zoomed: nil, title: nil, frame: nil)
        return (c, try XCTUnwrap(c.window))
    }

    func testTerminalTakesFocusFromNothing() throws {
        let (c, window) = try controller()
        window.makeFirstResponder(nil)
        becomeKey(c)
        XCTAssertTrue(window.firstResponder === c.content.focusedView)
    }

    func testTerminalTakesFocusFromOutsideTheTab() throws {
        let (c, window) = try controller()
        let frame = try XCTUnwrap(window.contentView?.superview)
        let outside = NSTextField(frame: .zero)
        frame.addSubview(outside)
        XCTAssertTrue(window.makeFirstResponder(outside))
        becomeKey(c)
        XCTAssertTrue(window.firstResponder === c.content.focusedView)
    }

    func testFieldInsideTheTabKeepsFocus() throws {
        let (c, window) = try controller()
        c.content.showFindBar()
        // Editing the search field: its field editor, not the field, holds the keyboard.
        let editor = try XCTUnwrap(window.firstResponder as? NSTextView)
        XCTAssertTrue(editor.isFieldEditor)
        XCTAssertTrue(editor.delegate is NSSearchField)
        becomeKey(c)
        XCTAssertTrue(window.firstResponder === editor)
        editor.insertText("needle", replacementRange: editor.selectedRange())
        XCTAssertEqual((editor.delegate as? NSSearchField)?.stringValue, "needle")
    }
}
