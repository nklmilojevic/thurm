import AppKit
import XCTest
@testable import Thurm

final class TabFloatTests: XCTestCase {
    private let manager = SessionManager.shared
    private var savedSidebarTabs = true

    override func setUp() {
        super.setUp()
        savedSidebarTabs = manager.config.sidebarTabs
        manager.config.sidebarTabs = true
    }

    override func tearDown() {
        for c in manager.liveControllers {
            c.closingWithoutConfirmation = true
            c.window?.close()
        }
        manager.config.sidebarTabs = savedSidebarTabs
        Remotes.shared.pendingCloses = [:]
        super.tearDown()
    }

    private func pane(_ id: UInt64) -> PaneKey { PaneKey(localHost, id) }

    private func tab(_ root: SplitNode, floating: [FloatingPane] = [], floatingHidden: Bool = false,
                     after host: TerminalWindowController? = nil) -> TerminalWindowController {
        let c = manager.makeController(root: root, focused: root.panes[0], zoomed: nil, title: nil, frame: nil,
                                       floating: floating, floatingHidden: floatingHidden)
        if let host { manager.attachAsTab(c, to: host) }
        return c
    }

    private func card(_ id: UInt64, _ frame: [Double] = [0.1, 0.1, 0.4, 0.4]) -> FloatingPane {
        FloatingPane(pane: pane(id), frame: frame)
    }

    func testAOnePaneTabFloatsIntoTheShownTab() {
        let shown = tab(.leaf(pane(1)))
        let other = tab(.leaf(pane(2)), after: shown)
        manager.floatTab(other, into: shown, at: CGPoint(x: 400, y: 300))

        XCTAssertTrue(other.isClosed)
        XCTAssertEqual(shown.content.paneIds, [pane(1), pane(2)])
        XCTAssertTrue(shown.content.isFloating(pane(2)))
        XCTAssertEqual(shown.content.focusedPane, pane(2))
        XCTAssertNotNil(shown.content.views[pane(2)])
        XCTAssertTrue(Remotes.shared.pendingCloses.isEmpty, "its pane keeps running")
        XCTAssertEqual(manager.group(of: shown).map(\.content.paneIds), [[pane(1), pane(2)]])
        let saved = shown.content.tabLayout(title: nil)
        XCTAssertEqual(saved.floating?.map(\.pane), [pane(2)])
        XCTAssertEqual(saved.floatingHidden, nil)
    }

    func testTheTabsOwnCardsComeAlongShown() {
        let shown = tab(.leaf(pane(1)))
        let other = tab(.leaf(pane(2)), floating: [card(3, [0.5, 0.5, 0.3, 0.3])], floatingHidden: true, after: shown)
        manager.floatTab(other, into: shown)

        XCTAssertTrue(other.isClosed)
        XCTAssertEqual(shown.content.floating.map(\.pane), [pane(3), pane(2)])
        XCTAssertEqual(shown.content.floating.first?.frame, CGRect(x: 0.5, y: 0.5, width: 0.3, height: 0.3))
        XCTAssertFalse(shown.content.floatingHidden)
        XCTAssertEqual(shown.content.focusedPane, pane(2))
        XCTAssertTrue(Remotes.shared.pendingCloses.isEmpty)
    }

    func testOnlyOnePaneTabsOfTheSameHostCanFloat() {
        let shown = tab(.leaf(pane(1)))
        _ = tab(.leaf(pane(2)), after: shown)
        let splits = tab(SplitNode.leaf(pane(3)).inserting(pane(4), at: pane(3), direction: .right), after: shown)
        let wall = tab(.leaf(pane(5)), after: shown)
        wall.wall = []
        let handoff = tab(.leaf(pane(6)), after: shown)
        handoff.handoffID = "h"
        let remote = tab(.leaf(PaneKey("devbox", 7)), after: shown)

        XCTAssertEqual(manager.floatSources(into: shown).map(\.content.paneIds), [[pane(2)]])

        manager.floatTab(splits, into: shown)
        manager.floatTab(remote, into: shown)
        manager.floatTab(shown, into: shown)
        XCTAssertFalse(splits.isClosed)
        XCTAssertFalse(remote.isClosed)
        XCTAssertEqual(shown.content.paneIds, [pane(1)])
        XCTAssertEqual(splits.content.paneIds, [pane(3), pane(4)])
        XCTAssertTrue(manager.floatSources(into: wall).isEmpty)
        XCTAssertTrue(manager.floatSources(into: handoff).isEmpty)
    }

    func testACardBecomesATabNextToTheRowItDroppedOn() throws {
        let first = tab(.leaf(pane(1)), floating: [card(3)])
        let second = tab(.leaf(pane(2)), after: first)
        manager.moveCardToNewTab(pane(3), from: first, next: (second, false))

        XCTAssertEqual(first.content.paneIds, [pane(1)])
        XCTAssertEqual(manager.group(of: first).map(\.content.paneIds), [[pane(1)], [pane(3)], [pane(2)]])
        let made = try XCTUnwrap(manager.controller(for: pane(3)))
        XCTAssertEqual(made.content.root.panes, [pane(3)])
        XCTAssertTrue(made.content.floating.isEmpty)
        XCTAssertEqual(made.workspaceID, first.workspaceID)
        XCTAssertTrue(Remotes.shared.pendingCloses.isEmpty)
        XCTAssertNil(first.content.tabLayout(title: nil).floating)
        let saved = made.content.tabLayout(title: nil)
        XCTAssertEqual(saved.root.panes, [pane(3)])
        XCTAssertEqual(saved.focused, 3)
        XCTAssertNil(saved.floating)
    }

    func testWallAndHandoffCardsStayPut() {
        let wall = tab(.leaf(pane(1)), floating: [card(3)])
        wall.wall = []
        let handoff = tab(.leaf(pane(2)), floating: [card(4)], after: wall)
        handoff.handoffID = "h"
        let origin = tab(.leaf(pane(5)), floating: [card(6)], after: wall)
        wall.wall = [WallOrigin(index: 0, tab: TabLayout(title: nil, root: .pane(pane(5)), focused: 5, zoomed: nil))]
        let target = tab(.leaf(pane(7)), after: wall)
        XCTAssertFalse(manager.floatSources(into: target).contains { $0 === origin })
        manager.moveCardToNewTab(pane(6), from: origin)
        manager.moveCardToNewTab(pane(3), from: wall)
        manager.moveCardToNewTab(pane(4), from: handoff)
        XCTAssertEqual(manager.group(of: wall).map(\.content.paneIds),
                       [[pane(1), pane(3)], [pane(7)], [pane(5), pane(6)], [pane(2), pane(4)]])
    }

    func testMoveFloatingPaneToNewTabAddsItAfterItsTab() throws {
        let first = tab(.leaf(pane(1)), floating: [card(3)])
        first.content.focus(pane(3))
        first.moveFloatingPaneToNewTab(nil)
        XCTAssertEqual(manager.group(of: first).count, 1, "single-window sidebar mode reuses the window")
        XCTAssertEqual(manager.windowTabs(of: first).map(\.content.paneIds), [[pane(1)], [pane(3)]])

        let again = try XCTUnwrap(manager.controller(for: pane(3)))
        again.moveFloatingPaneToNewTab(nil)
        XCTAssertEqual(manager.windowTabs(of: first).count, 2, "a pane of the splits stays put")
    }

    func testMoveFloatingPaneToNewTabAddsItAfterItsTabInNativeMode() throws {
        manager.config.sidebarTabs = false
        let first = tab(.leaf(pane(1)), floating: [card(3)])
        first.content.focus(pane(3))
        first.moveFloatingPaneToNewTab(nil)
        XCTAssertEqual(manager.group(of: first).map(\.content.paneIds), [[pane(1)], [pane(3)]])

        let again = try XCTUnwrap(manager.controller(for: pane(3)))
        again.moveFloatingPaneToNewTab(nil)
        XCTAssertEqual(manager.group(of: first).count, 2, "a pane of the splits stays put")
    }

    func testFloatTabHereIsHiddenWhenTheTabCannotFloat() throws {
        _ = NSApplication.shared
        let shown = tab(.leaf(pane(1)))
        let sidebar = TabSidebarViewController()
        sidebar.controller = shown
        let row = NSMenuItem()
        XCTAssertFalse(sidebar.validateFloatItem(row, for: shown))
        XCTAssertTrue(row.isHidden, "the shown tab's own row")
        XCTAssertFalse(sidebar.validateFloatItem(row, for: nil))
        XCTAssertTrue(row.isHidden)

        let view = try XCTUnwrap(MainMenu.build().items.compactMap(\.submenu).first { $0.title == "View" })
        let floatTab = try XCTUnwrap(view.items.first { $0.title == "Float Tab Here" })
        FloatTabMenu.shared.menuNeedsUpdate(view)
        XCTAssertTrue(floatTab.isHidden, "nothing else to float")

        let other = tab(.leaf(pane(2)), after: shown)
        XCTAssertTrue(sidebar.validateFloatItem(row, for: other))
        XCTAssertFalse(row.isHidden)
        FloatTabMenu.shared.menuNeedsUpdate(view)
        XCTAssertFalse(floatTab.isHidden)
    }

    func testDroppedCardsCenterOnThePointInsideTheArea() {
        let area = CGRect(x: 0, y: 0, width: 1000, height: 500)
        let d = FloatingGeometry.defaultFraction(index: 0)
        let mid = FloatingGeometry.centered(at: CGPoint(x: 500, y: 250), in: area, index: 0)
        XCTAssertEqual(mid.midX, 0.5, accuracy: 0.0001)
        XCTAssertEqual(mid.midY, 0.5, accuracy: 0.0001)
        XCTAssertEqual(mid.size, d.size)
        let corner = FloatingGeometry.centered(at: CGPoint(x: 990, y: 0), in: area, index: 0)
        XCTAssertEqual(corner.maxX, 1, accuracy: 0.0001)
        XCTAssertEqual(corner.minY, 0)
    }

    private let sidebarRows = [CGRect(x: 0, y: 0, width: 200, height: 40), CGRect(x: 0, y: 40, width: 200, height: 40),
                               CGRect(x: 0, y: 104, width: 200, height: 40)]

    func testSidebarGapIsBeforeOrAfterTheNearestRow() throws {
        let rows = sidebarRows, keys: [String?] = ["", "", ""]
        XCTAssertNil(SidebarDrop.gap(rows: [], keys: [], key: "", at: 10))
        let top = try XCTUnwrap(SidebarDrop.gap(rows: rows, keys: keys, key: "", at: 10))
        XCTAssertEqual(top.index, 0)
        XCTAssertFalse(top.after)
        XCTAssertTrue(try XCTUnwrap(SidebarDrop.gap(rows: rows, keys: keys, key: "", at: 70)).after)
        XCTAssertEqual(try XCTUnwrap(SidebarDrop.gap(rows: rows, keys: keys, key: "", at: 100)).index, 2,
                       "a group header goes to the row after it")
        let below = try XCTUnwrap(SidebarDrop.gap(rows: rows, keys: keys, key: "", at: 400))
        XCTAssertEqual(below.index, 2)
        XCTAssertTrue(below.after)
    }

    func testSidebarGapStaysInTheCardsRepositoryGroup() throws {
        let rows = sidebarRows, keys: [String?] = ["/a", "/a", nil]
        let overOtherRepo = try XCTUnwrap(SidebarDrop.gap(rows: rows, keys: keys, key: nil, at: 10))
        XCTAssertEqual(overOtherRepo.index, 2)
        XCTAssertFalse(overOtherRepo.after)
        let overOwnRepo = try XCTUnwrap(SidebarDrop.gap(rows: rows, keys: keys, key: "/a", at: 130))
        XCTAssertEqual(overOwnRepo.index, 1)
        XCTAssertTrue(overOwnRepo.after)
        XCTAssertNil(SidebarDrop.gap(rows: rows, keys: keys, key: "/b", at: 10), "no row of its group")
    }

    func testANewGroupShowsAfterTheRepositoryGroupsOrAtTheEnd() throws {
        XCTAssertNil(SidebarDrop.newGroupEdge(keys: [], key: "/b"))
        let repo = try XCTUnwrap(SidebarDrop.newGroupEdge(keys: ["/a", "/a", nil], key: "/b"))
        XCTAssertEqual(repo.index, 1)
        XCTAssertTrue(repo.after)
        let first = try XCTUnwrap(SidebarDrop.newGroupEdge(keys: [nil, nil], key: "/b"))
        XCTAssertEqual(first.index, 0)
        XCTAssertFalse(first.after)
        let terminals = try XCTUnwrap(SidebarDrop.newGroupEdge(keys: ["/a"], key: nil))
        XCTAssertEqual(terminals.index, 0)
        XCTAssertTrue(terminals.after)
    }
}
