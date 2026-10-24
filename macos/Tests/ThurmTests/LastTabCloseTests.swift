import AppKit
import XCTest
@testable import Thurm

/// Closing the last tab of a workspace shows another workspace instead of closing the window.
final class LastTabCloseTests: XCTestCase {
    private let manager = SessionManager.shared
    private var savedWorkspaces: [Workspace] = []

    override func setUp() {
        super.setUp()
        savedWorkspaces = manager.workspaces
        manager.workspaces = []
    }

    override func tearDown() {
        for c in manager.liveControllers {
            c.closingWithoutConfirmation = true
            c.window?.close()
        }
        manager.workspaces = savedWorkspaces
        Remotes.shared.pendingCloses = [:]
        super.tearDown()
    }

    private func pane(_ id: UInt64) -> PaneKey { PaneKey(localHost, id) }

    /// A shown workspace with one tab on pane 1, and a hidden one, used earlier, on pane 2.
    private func twoWorkspaces() -> (TerminalWindowController, shown: Workspace, hidden: Workspace) {
        let hidden = manager.makeWorkspace(name: "hidden")
        hidden.hiddenTabs = [TabLayout(title: nil, root: .pane(pane(2)), focused: 2, zoomed: nil)]
        hidden.lastActive = Date(timeIntervalSinceNow: -60)
        let shown = manager.makeWorkspace(name: "shown")
        let c = manager.makeController(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: nil, frame: nil)
        c.workspaceID = shown.id
        return (c, shown, hidden)
    }

    private func assertSwitched(from c: TerminalWindowController, shown: Workspace, hidden: Workspace,
                                file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertTrue(c.isClosed, file: file, line: line)
        XCTAssertNil(manager.workspace(shown.id), "the emptied workspace goes", file: file, line: line)
        let now = manager.controllerShowing(hidden)
        XCTAssertNotNil(now, "the hidden workspace is shown", file: file, line: line)
        XCTAssertEqual(now?.content.paneIds, [pane(2)], file: file, line: line)
        XCTAssertTrue(hidden.hiddenTabs.isEmpty, file: file, line: line)
    }

    func testClosingLastTabShowsPreviousWorkspace() throws {
        let (c, shown, hidden) = twoWorkspaces()
        let window = try XCTUnwrap(c.window)
        XCTAssertFalse(c.windowShouldClose(window))
        assertSwitched(from: c, shown: shown, hidden: hidden)
    }

    func testLastPaneGoingShowsPreviousWorkspace() {
        let (c, shown, hidden) = twoWorkspaces()
        manager.removePaneFromUI(pane(1))
        assertSwitched(from: c, shown: shown, hidden: hidden)
    }

    func testLastWorkspaceStillCloses() throws {
        let shown = manager.makeWorkspace(name: "only")
        let c = manager.makeController(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: nil, frame: nil)
        c.workspaceID = shown.id
        XCTAssertTrue(c.windowShouldClose(try XCTUnwrap(c.window)))
        manager.removePaneFromUI(pane(1))
        XCTAssertTrue(c.isClosed)
        XCTAssertNotNil(manager.workspace(shown.id))
    }
}
