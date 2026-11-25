import AppKit
import XCTest
@testable import Thurm

final class TabOrderTests: XCTestCase {
    private let manager = SessionManager.shared
    private var savedWorkspaces: [Workspace] = []

    override func setUp() {
        super.setUp()
        _ = NSApplication.shared
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

    private func pane(_ id: UInt64) -> PaneKey { .local(id) }

    private func tab(_ id: UInt64, workspace: Workspace) -> TerminalWindowController {
        let c = manager.makeController(root: .leaf(pane(id)), focused: pane(id), zoomed: nil,
                                       title: nil, frame: nil)
        c.workspaceID = workspace.id
        return c
    }

    private func order(_ c: TerminalWindowController) -> [UInt64] {
        manager.group(of: c).map { $0.content.paneIds[0].id }
    }

    func testBackgroundTabsKeepCreationOrderAndSelection() {
        let first = tab(1, workspace: manager.makeWorkspace())
        let second = tab(2, workspace: manager.workspace(first.workspaceID)!)
        manager.attachAsTab(second, to: first)
        first.window?.tabGroup?.selectedWindow = second.window

        for id: UInt64 in [3, 4, 5] {
            manager.newTab(from: first, pane: pane(id), background: true)
        }

        XCTAssertEqual(order(first), [1, 2, 3, 4, 5])
        XCTAssertTrue(first.window?.tabGroup?.selectedWindow === second.window)
    }

    func testFirstBackgroundTabKeepsItsSourceSelected() {
        let first = tab(1, workspace: manager.makeWorkspace())
        manager.newTab(from: first, pane: pane(2), background: true)
        XCTAssertEqual(order(first), [1, 2])
        XCTAssertTrue(first.window?.tabGroup?.selectedWindow === first.window)
    }

    func testCommandTabsUseTheSourceWorkspace() {
        let source = tab(1, workspace: manager.makeWorkspace())
        let other = tab(9, workspace: manager.makeWorkspace())
        manager.controllerBecameKey(other)

        for id: UInt64 in [2, 3, 4] {
            manager.coreDidReceiveEvent("Ui", payload: ["Open": [
                "pane": id, "target": UInt64(1), "background": true, "new_window": false
            ]], host: localHost)
        }

        XCTAssertEqual(order(source), [1, 2, 3, 4])
        XCTAssertEqual(order(other), [9])
        XCTAssertTrue(source.window?.tabGroup?.selectedWindow === source.window)
    }

    func testCommandTabsStayInAHiddenSourceWorkspace() {
        let hidden = manager.makeWorkspace()
        hidden.hiddenTabs = [TabLayout(title: nil, root: .pane(pane(1)), focused: 1, zoomed: nil)]
        let shown = tab(9, workspace: manager.makeWorkspace())
        manager.controllerBecameKey(shown)

        manager.newCommandTab(pane(2), target: pane(1), background: true)
        manager.newCommandTab(pane(3), target: pane(1), background: true)

        XCTAssertEqual(hidden.hiddenTabs.flatMap(\.panes), [pane(1), pane(2), pane(3)])
        XCTAssertEqual(hidden.hiddenSelectedTab, 0)
        XCTAssertFalse(manager.isShown(hidden))
        XCTAssertEqual(order(shown), [9])
    }

    func testBackgroundSplitStaysInItsHiddenTab() throws {
        let hidden = manager.makeWorkspace()
        hidden.hiddenTabs = [TabLayout(title: nil, root: .pane(pane(1)), focused: 1, zoomed: nil)]
        let shown = tab(9, workspace: manager.makeWorkspace())
        manager.controllerBecameKey(shown)

        manager.coreDidReceiveEvent("Ui", payload: ["Open": [
            "pane": UInt64(2), "target": UInt64(1), "split": "right", "percent": UInt64(70), "background": true
        ]], host: localHost)

        XCTAssertEqual(hidden.hiddenTabs.count, 1)
        guard case .split(.right, let ratio, .pane(pane(1)), .pane(pane(2))) = hidden.hiddenTabs[0].root else {
            return XCTFail("pane 1 is split, pane 2 on its right: \(hidden.hiddenTabs[0].root)")
        }
        XCTAssertEqual(ratio, 0.3, accuracy: 0.001)
        XCTAssertEqual(hidden.hiddenTabs[0].focused, 1)
        XCTAssertFalse(manager.isShown(hidden))
        XCTAssertEqual(shown.content.paneIds, [pane(9)])
    }

    func testForegroundSplitRevealsItsHiddenTab() throws {
        let hidden = manager.makeWorkspace()
        hidden.hiddenTabs = [TabLayout(title: nil, root: .pane(pane(1)), focused: 1, zoomed: nil)]
        let shown = tab(9, workspace: manager.makeWorkspace())
        manager.controllerBecameKey(shown)

        manager.coreDidReceiveEvent("Ui", payload: ["Split": [
            "pane": UInt64(2), "target": UInt64(1), "dir": "down"
        ]], host: localHost)

        let revealed = try XCTUnwrap(manager.controller(for: pane(2)))
        XCTAssertEqual(revealed.workspaceID, hidden.id)
        XCTAssertEqual(revealed.content.paneIds, [pane(1), pane(2)])
        XCTAssertEqual(revealed.content.focusedPane, pane(2))
        XCTAssertNil(manager.controller(for: pane(9)), "the shown tab is not split")
    }

    func testUnsizedHiddenSplitAutoBalancesLikeAShownOne() {
        let saved = manager.config
        defer { manager.config = saved }
        manager.config.autoBalance = true
        let uneven = SplitNode.split(axis: .horizontal, ratio: 0.8, first: .leaf(pane(1)), second: .leaf(pane(3)))
        let hidden = manager.makeWorkspace()
        hidden.hiddenTabs = [TabLayout(title: nil, root: uneven.layoutNode, focused: 1, zoomed: nil)]
        manager.controllerBecameKey(tab(9, workspace: manager.makeWorkspace()))

        manager.coreDidReceiveEvent("Ui", payload: ["Open": [
            "pane": UInt64(2), "target": UInt64(1), "split": "right", "background": true
        ]], host: localHost)

        let expected = uneven.inserting(pane(2), at: pane(1), direction: .right).equalized().layoutNode
        XCTAssertEqual(hidden.hiddenTabs[0].root, expected)
    }

    func testHiddenSplitClearsTheTabsZoom() throws {
        let hidden = manager.makeWorkspace()
        let root = SplitNode.split(axis: .horizontal, ratio: 0.5, first: .leaf(pane(1)), second: .leaf(pane(3)))
        hidden.hiddenTabs = [TabLayout(title: nil, root: root.layoutNode, focused: 1, zoomed: 1)]
        manager.controllerBecameKey(tab(9, workspace: manager.makeWorkspace()))

        manager.coreDidReceiveEvent("Ui", payload: ["Split": [
            "pane": UInt64(2), "target": UInt64(1), "dir": "down"
        ]], host: localHost)

        let revealed = try XCTUnwrap(manager.controller(for: pane(2)))
        XCTAssertNil(revealed.content.zoomedPane)
        XCTAssertEqual(revealed.content.focusedPane, pane(2))
    }

    func testForegroundCommandTabsKeepCreationOrder() {
        let source = tab(1, workspace: manager.makeWorkspace())
        for id: UInt64 in [2, 3, 4] {
            manager.newCommandTab(pane(id), target: pane(1), background: false)
        }
        XCTAssertEqual(order(source), [1, 2, 3, 4])
        XCTAssertTrue(source.window?.tabGroup?.selectedWindow === manager.controller(for: pane(4))?.window)
    }

    func testForegroundCommandRevealsItsSourceWorkspace() throws {
        let hidden = manager.makeWorkspace()
        hidden.hiddenTabs = [TabLayout(title: nil, root: .pane(pane(1)), focused: 1, zoomed: nil)]
        let shown = tab(9, workspace: manager.makeWorkspace())
        manager.controllerBecameKey(shown)

        manager.newCommandTab(pane(2), target: pane(1), background: false)

        let selected = try XCTUnwrap(manager.controller(for: pane(2)))
        XCTAssertEqual(selected.workspaceID, hidden.id)
        XCTAssertEqual(order(selected), [1, 2])
        XCTAssertTrue(selected.window?.tabGroup?.selectedWindow === selected.window)
    }

    private func hiddenWorkspaces() -> (source: Workspace, recent: Workspace) {
        let source = manager.makeWorkspace()
        source.hiddenTabs = [TabLayout(title: nil, root: .pane(pane(1)), focused: 1, zoomed: nil)]
        source.lastActive = Date(timeIntervalSince1970: 1)
        let recent = manager.makeWorkspace()
        recent.hiddenTabs = [TabLayout(title: nil, root: .pane(pane(9)), focused: 9, zoomed: nil)]
        recent.lastActive = Date(timeIntervalSince1970: 2)
        XCTAssertNil(manager.currentRegularController)
        return (source, recent)
    }

    func testForegroundCommandOpensItsHiddenSourceWithNoWindow() throws {
        let (source, recent) = hiddenWorkspaces()

        manager.newCommandTab(pane(2), target: pane(1), background: false)

        let selected = try XCTUnwrap(manager.controller(for: pane(2)))
        XCTAssertEqual(selected.workspaceID, source.id)
        XCTAssertEqual(order(selected), [1, 2])
        XCTAssertTrue(selected.window?.tabGroup?.selectedWindow === selected.window)
        XCTAssertTrue(source.hiddenTabs.isEmpty)
        XCTAssertFalse(manager.isShown(recent))
        XCTAssertEqual(recent.hiddenTabs.flatMap(\.panes), [pane(9)])
        XCTAssertEqual(manager.workspaces.count, 2)
    }

    func testBackgroundCommandKeepsItsHiddenSourceWithNoWindow() {
        let (source, recent) = hiddenWorkspaces()

        manager.newCommandTab(pane(2), target: pane(1), background: true)
        manager.newCommandTab(pane(3), target: pane(1), background: true)

        XCTAssertNil(manager.currentRegularController)
        XCTAssertEqual(source.hiddenTabs.flatMap(\.panes), [pane(1), pane(2), pane(3)])
        XCTAssertEqual(source.hiddenSelectedTab, 0)
        XCTAssertEqual(recent.hiddenTabs.flatMap(\.panes), [pane(9)])
        XCTAssertEqual(manager.workspaces.count, 2)
    }

    func testManualTabsStillOpenNextToTheirSource() {
        let first = tab(1, workspace: manager.makeWorkspace())
        manager.newTab(from: first, pane: pane(2))
        manager.newTab(from: first, pane: pane(3))
        XCTAssertEqual(order(first), [1, 3, 2])
    }

    func testOpeningAWindowPreservesHiddenTabOrder() throws {
        let ws = manager.makeWorkspace()
        ws.hiddenTabs = [1, 2, 3].map {
            TabLayout(title: nil, root: .pane(pane($0)), focused: $0, zoomed: nil)
        }

        manager.newTab(from: nil, pane: pane(4))

        let selected = try XCTUnwrap(manager.controller(for: pane(4)))
        XCTAssertEqual(order(selected), [4, 1, 2, 3])
        XCTAssertTrue(ws.hiddenTabs.isEmpty)
    }
}
