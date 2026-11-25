import AppKit
import XCTest
@testable import Thurm

final class TabHistoryTests: XCTestCase {
    private let manager = SessionManager.shared
    private var savedWorkspaces: [Workspace] = []
    private var savedHistory = TabHistory()
    private var savedSidebarTabs = true

    override func setUp() {
        super.setUp()
        savedWorkspaces = manager.workspaces
        savedHistory = manager.tabHistory
        savedSidebarTabs = manager.config.sidebarTabs
        manager.config.sidebarTabs = true
        manager.workspaces = []
        manager.tabHistory = TabHistory()
        Remotes.shared.pendingCloses = [:]
    }

    override func tearDown() {
        for c in manager.liveControllers {
            c.closingWithoutConfirmation = true
            c.window?.close()
        }
        manager.config.sidebarTabs = savedSidebarTabs
        manager.workspaces = savedWorkspaces
        manager.tabHistory = savedHistory
        Remotes.shared.pendingCloses = [:]
        super.tearDown()
    }

    private func pane(_ id: UInt64) -> PaneKey { PaneKey(localHost, id) }

    private func tab(_ ids: UInt64...) -> [PaneKey] { ids.map(pane) }

    private func history(_ tabs: [[PaneKey]]) -> TabHistory {
        var h = TabHistory()
        for t in tabs { h.visit(t) }
        return h
    }

    private let allOpen: ([PaneKey]) -> Bool = { _ in true }

    func testBackAndForwardWalkTheVisits() {
        var h = history([tab(1), tab(2), tab(3)])

        XCTAssertEqual(h.back(isOpen: allOpen), tab(2))
        XCTAssertEqual(h.back(isOpen: allOpen), tab(1))
        XCTAssertEqual(h.forward(isOpen: allOpen), tab(2))
        XCTAssertEqual(h.forward(isOpen: allOpen), tab(3))
    }

    func testEndsReturnNothing() {
        var h = history([tab(1), tab(2)])

        XCTAssertNil(h.forward(isOpen: allOpen))
        XCTAssertEqual(h.back(isOpen: allOpen), tab(1))
        XCTAssertNil(h.back(isOpen: allOpen))
        XCTAssertEqual(h.cursor, 0)
        var empty = TabHistory()
        XCTAssertNil(empty.back(isOpen: allOpen))
    }

    func testVisitAfterBackDropsTheForwardTabs() {
        var h = history([tab(1), tab(2), tab(3)])
        _ = h.back(isOpen: allOpen)
        _ = h.back(isOpen: allOpen)

        h.visit(tab(4))

        XCTAssertEqual(h.entries, [tab(1), tab(4)])
        XCTAssertNil(h.forward(isOpen: allOpen))
        XCTAssertEqual(h.back(isOpen: allOpen), tab(1))
    }

    func testArrivingAtTheTabNavigatedToRecordsNothing() {
        var h = history([tab(1), tab(2), tab(3)])

        let target = h.back(isOpen: allOpen)
        h.visit(target!)

        XCTAssertEqual(h.entries, [tab(1), tab(2), tab(3)])
        XCTAssertEqual(h.forward(isOpen: allOpen), tab(3))
        h.visit(tab(3))
        XCTAssertEqual(h.entries.count, 3)
    }

    func testClosedTabsAreSkippedAndPruned() {
        var h = history([tab(1), tab(2), tab(3), tab(4)])
        let open: ([PaneKey]) -> Bool = { $0 != self.tab(2) && $0 != self.tab(3) }

        XCTAssertEqual(h.back(isOpen: open), tab(1))
        XCTAssertEqual(h.entries, [tab(1), tab(4)])
        XCTAssertEqual(h.forward(isOpen: open), tab(4))
    }

    func testForwardSkipsAClosedTab() {
        var h = history([tab(1), tab(2), tab(3)])
        _ = h.back(isOpen: allOpen)
        _ = h.back(isOpen: allOpen)

        XCTAssertEqual(h.forward(isOpen: { $0 != self.tab(2) }), tab(3))
    }

    func testBackFromAClosedCurrentTabGoesToTheOneBefore() {
        var h = history([tab(1), tab(2), tab(3)])

        XCTAssertEqual(h.back(isOpen: { $0 != self.tab(3) }), tab(2))
        XCTAssertEqual(h.entries, [tab(1), tab(2)])
        XCTAssertNil(h.forward(isOpen: allOpen))
    }

    func testTabsThatMeetAfterAPruneMerge() {
        var h = history([tab(1), tab(2), tab(1), tab(3)])

        XCTAssertEqual(h.back(isOpen: { $0 != self.tab(2) }), tab(1))
        XCTAssertEqual(h.entries, [tab(1), tab(3)])
        XCTAssertNil(h.back(isOpen: allOpen))
    }

    func testKeepsTheLastFiftyVisits() {
        var h = history((1...60).map { tab($0) })

        XCTAssertEqual(h.entries.count, TabHistory.limit)
        XCTAssertEqual(h.entries.first, tab(11))
        XCTAssertEqual(h.cursor, TabHistory.limit - 1)
        XCTAssertEqual(h.back(isOpen: allOpen), tab(59))
    }

    func testATabKeepsItsPlaceWhenItsPanesChange() {
        var h = history([tab(1), tab(2)])

        h.visit(tab(2, 5))
        XCTAssertEqual(h.entries, [tab(1), tab(2, 5)])
        h.visit(tab(5))
        XCTAssertEqual(h.entries, [tab(1), tab(5)])
        XCTAssertEqual(h.back(isOpen: allOpen), tab(1))
    }

    func testBackFromATabNotRecordedGoesToTheTabLeft() {
        var h = history([tab(1), tab(2)])

        XCTAssertEqual(h.back(fromCurrent: false, isOpen: allOpen), tab(2))
        XCTAssertEqual(h.back(isOpen: allOpen), tab(1))
    }

    func testAnEntryFollowsThePanesThatStay() {
        var h = history([tab(1), tab(2)])

        h.tabChanged(from: tab(1), to: tab(1, 5))
        h.tabChanged(from: tab(1, 5), to: tab(5))

        XCTAssertEqual(h.entries, [tab(5), tab(2)])
    }

    func testAPaneMovedToAnotherTabLeavesTheEntryItCameFrom() {
        var h = history([tab(1, 5), tab(2)])

        h.tabChanged(from: tab(1, 5), to: tab(1))
        h.tabChanged(from: tab(2), to: tab(2, 5))

        XCTAssertEqual(h.entries, [tab(1), tab(2, 5)])
    }

    func testATabStaysInHistoryWhenItsPanesAreSplitAndClosed() throws {
        let a = manager.makeController(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: nil, frame: nil)
        let b = manager.makeController(root: .leaf(pane(2)), focused: pane(2), zoomed: nil, title: nil, frame: nil)
        manager.controllerBecameKey(a)

        a.content.split(newPane: pane(3), direction: .right)
        _ = a.content.remove(pane: pane(1))
        manager.controllerBecameKey(b)

        XCTAssertEqual(manager.tabHistory.entries, [tab(3), tab(2)])
        XCTAssertTrue(manager.canGoBackInTabHistory)
        guard case .shown(let found) = manager.tabTarget(manager.tabHistory.entries[0]) else {
            return XCTFail("tab A is still open")
        }
        XCTAssertTrue(found === a)
    }

    func testSwappingPanesBetweenTwoVisitedTabsKeepsBothInHistory() throws {
        let a = manager.makeController(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: nil, frame: nil)
        let split = SplitNode.split(axis: .horizontal, ratio: 0.5, first: .leaf(pane(2)), second: .leaf(pane(3)))
        let b = manager.makeController(root: split, focused: pane(2), zoomed: nil, title: nil, frame: nil)
        manager.controllerBecameKey(a)
        manager.controllerBecameKey(b)

        manager.swapPanes(pane(1), pane(2))

        XCTAssertEqual(manager.tabHistory.entries, [tab(2), tab(1, 3)])
        guard case .shown(let back) = manager.tabTarget(manager.tabHistory.entries[0]) else { return XCTFail("tab A") }
        XCTAssertTrue(back === a)
        guard case .shown(let here) = manager.tabTarget(manager.tabHistory.entries[1]) else { return XCTFail("tab B") }
        XCTAssertTrue(here === b)
    }

    func testBackFromTheAgentWallReturnsToTheTabLeft() {
        let a = manager.makeController(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: nil, frame: nil)
        let b = manager.makeController(root: .leaf(pane(2)), focused: pane(2), zoomed: nil, title: nil, frame: nil)
        let wall = manager.makeController(root: .leaf(pane(3)), focused: pane(3), zoomed: nil, title: nil, frame: nil)
        wall.wall = []
        manager.controllerBecameKey(a)
        manager.controllerBecameKey(b)
        manager.controllerBecameKey(wall)

        manager.goBackInTabHistory()

        XCTAssertEqual(manager.tabHistory.entries, [tab(1), tab(2)])
        XCTAssertEqual(manager.tabHistory.cursor, 1)
    }

    func testTargetsResolveAcrossWorkspaces() throws {
        let hidden = manager.makeWorkspace(name: "hidden")
        hidden.hiddenTabs = [
            TabLayout(title: nil, root: .pane(pane(9)), focused: 9, zoomed: nil),
            TabLayout(title: nil, root: .pane(pane(8)), focused: 8, zoomed: nil, wall: []),
        ]
        let shown = manager.makeWorkspace(name: "shown")
        let c = manager.makeController(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: nil, frame: nil)
        c.workspaceID = shown.id

        guard case .shown(let found) = manager.tabTarget(tab(1)) else { return XCTFail("shown tab") }
        XCTAssertTrue(found === c)
        guard case .hidden(let h) = manager.tabTarget(tab(7, 9)) else { return XCTFail("hidden tab") }
        XCTAssertEqual(h, HiddenTab(workspace: hidden.id, index: 0))
        XCTAssertNil(manager.tabTarget(tab(8)), "the agent wall is no target")
        XCTAssertNil(manager.tabTarget(tab(42)), "a closed tab is no target")
        let wall = manager.makeController(root: .leaf(pane(4)), focused: pane(4), zoomed: nil, title: nil, frame: nil)
        wall.wall = []
        guard case .shown = manager.tabTarget(tab(4, 1)) else { return XCTFail("a pane on the wall is skipped") }
    }

    func testTheAgentWallIsNotRecorded() {
        let c = manager.makeController(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: nil, frame: nil)
        c.wall = []

        manager.recordTabVisit(c)

        XCTAssertTrue(manager.tabHistory.entries.isEmpty)
    }

    func testBackToAHiddenTabSwitchesTheWindowToItsWorkspace() throws {
        let hidden = manager.makeWorkspace(name: "hidden")
        hidden.hiddenTabs = [
            TabLayout(title: nil, root: .pane(pane(8)), focused: 8, zoomed: nil),
            TabLayout(title: nil, root: .pane(pane(9)), focused: 9, zoomed: nil),
        ]
        let shown = manager.makeWorkspace(name: "shown")
        let c = manager.makeController(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: nil, frame: nil)
        c.workspaceID = shown.id
        let originalWindow = c.window
        manager.tabHistory = history([tab(9), tab(1)])

        manager.goBackInTabHistory()

        let arrived = try XCTUnwrap(manager.controller(for: pane(9)))
        XCTAssertEqual(arrived.workspaceID, hidden.id)
        XCTAssertTrue(arrived === c, "single-window sidebar mode switches in place")
        XCTAssertTrue(arrived.window === originalWindow)
        XCTAssertFalse(c.isClosed)
        XCTAssertEqual(arrived.content.paneIds, [pane(9)])
        XCTAssertEqual(shown.hiddenTabs.map(\.panes), [tab(1)])
        manager.recordTabVisit(arrived)
        XCTAssertEqual(manager.tabHistory.entries, [tab(9), tab(1)])
        XCTAssertEqual(manager.tabHistory.cursor, 0)
    }

    func testBackToAHiddenTabInNativeModeSwitchesTheWindowToItsWorkspace() throws {
        manager.config.sidebarTabs = false
        let hidden = manager.makeWorkspace(name: "hidden")
        hidden.hiddenTabs = [
            TabLayout(title: nil, root: .pane(pane(8)), focused: 8, zoomed: nil),
            TabLayout(title: nil, root: .pane(pane(9)), focused: 9, zoomed: nil),
        ]
        let shown = manager.makeWorkspace(name: "shown")
        let c = manager.makeController(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: nil, frame: nil)
        c.workspaceID = shown.id
        manager.tabHistory = history([tab(9), tab(1)])

        manager.goBackInTabHistory()

        let arrived = try XCTUnwrap(manager.controller(for: pane(9)))
        XCTAssertEqual(arrived.workspaceID, hidden.id)
        XCTAssertTrue(c.isClosed)
        XCTAssertEqual(shown.hiddenTabs.map(\.panes), [tab(1)])
        manager.recordTabVisit(arrived)
        XCTAssertEqual(manager.tabHistory.entries, [tab(9), tab(1)])
        XCTAssertEqual(manager.tabHistory.cursor, 0)
    }
}
