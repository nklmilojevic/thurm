import AppKit
import XCTest
@testable import Thurm

final class HeldPaneTests: XCTestCase {
    private let manager = SessionManager.shared
    private let shell = PaneKey(localHost, 900_100)
    private let key = PaneKey(localHost, 900_101)
    private var savedWorkspaces: [Workspace] = []
    private var controller: TerminalWindowController!

    override func setUp() {
        super.setUp()
        _ = NSApplication.shared
        savedWorkspaces = manager.workspaces
        manager.workspaces = []
        manager.coreDidReceiveEvent("PaneInfo", payload: ["id": NSNumber(value: key.id), "title": "echo",
                                                          "alive": true], host: localHost)
        let root = SplitNode.split(axis: .horizontal, ratio: 0.5, first: .leaf(shell), second: .leaf(key))
        controller = manager.makeController(root: root, focused: shell, zoomed: nil, title: nil, frame: nil)
    }

    override func tearDown() {
        for c in manager.liveControllers {
            c.closingWithoutConfirmation = true
            c.window?.close()
        }
        manager.workspaces = savedWorkspaces
        manager.panes[key] = nil
        super.tearDown()
    }

    private var pane: [String: Any] { ["pane": NSNumber(value: key.id)] }

    func testAnExitedPaneStaysInItsWindowUntilItIsClosed() {
        manager.coreDidReceiveEvent("PaneExited", payload: pane.merging(["code": 0]) { $1 }, host: localHost)
        XCTAssertEqual(manager.panes[key]?.alive, false)
        XCTAssertTrue(controller.content.paneIds.contains(key))

        manager.coreDidReceiveEvent("PaneClosed", payload: pane, host: localHost)
        XCTAssertNil(manager.panes[key])
        XCTAssertFalse(controller.content.paneIds.contains(key))
        XCTAssertTrue(controller.content.paneIds.contains(shell))
    }
}
