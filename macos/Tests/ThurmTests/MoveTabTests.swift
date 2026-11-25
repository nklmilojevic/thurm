import AppKit
import XCTest
@testable import Thurm

final class MoveTabTests: XCTestCase {
    private let manager = SessionManager.shared
    private var savedWorkspaces: [Workspace] = []
    private var savedSidebarTabs = true

    override func setUp() {
        super.setUp()
        savedWorkspaces = manager.workspaces
        savedSidebarTabs = manager.config.sidebarTabs
        manager.config.sidebarTabs = true
        manager.workspaces = []
        Remotes.shared.pendingCloses = [:]
    }

    override func tearDown() {
        for c in manager.liveControllers {
            c.closingWithoutConfirmation = true
            c.window?.close()
        }
        manager.config.sidebarTabs = savedSidebarTabs
        manager.workspaces = savedWorkspaces
        Remotes.shared.pendingCloses = [:]
        super.tearDown()
    }

    private func pane(_ id: UInt64) -> PaneKey { PaneKey(localHost, id) }

    private func tab(_ root: SplitNode, focused: PaneKey) -> TerminalWindowController {
        manager.makeController(root: root, focused: focused, zoomed: nil, title: nil, frame: nil)
    }

    private func setUp(shown roots: [SplitNode]) -> (tabs: [TerminalWindowController], shown: Workspace,
                                                     hidden: Workspace) {
        let hidden = manager.makeWorkspace(name: "hidden")
        hidden.hiddenTabs = [TabLayout(title: nil, root: .pane(pane(9)), focused: 9, zoomed: nil)]
        hidden.lastActive = Date(timeIntervalSinceNow: -60)
        let shown = manager.makeWorkspace(name: "shown")
        var tabs: [TerminalWindowController] = []
        for root in roots {
            let c = tab(root, focused: root.panes[0])
            if let first = tabs.first {
                manager.attachAsTab(c, to: first)
            } else {
                c.workspaceID = shown.id
            }
            tabs.append(c)
        }
        return (tabs, shown, hidden)
    }

    private func assertNoPaneClosed(file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertTrue(Remotes.shared.pendingCloses.values.allSatisfy(\.isEmpty), "no pane is closed",
                      file: file, line: line)
    }

    func testMovesOneOfSeveralTabsInTheBackground() throws {
        let (tabs, shown, hidden) = setUp(shown: [.leaf(pane(1)), .leaf(pane(2))])
        XCTAssertEqual(manager.group(of: tabs[0]).count, 2)

        manager.moveTab(tabs[1], to: hidden)

        XCTAssertTrue(tabs[1].isClosed)
        XCTAssertFalse(tabs[0].isClosed)
        XCTAssertEqual(tabs[0].workspaceID, shown.id)
        XCTAssertEqual(hidden.hiddenTabs.map(\.panes), [[pane(9)], [pane(2)]])
        XCTAssertEqual(hidden.hiddenSelectedTab, 0, "the target keeps its selected tab")
        XCTAssertNil(manager.controller(for: pane(2)))
        XCTAssertEqual(manager.workspace(containing: pane(2))?.id, hidden.id)
        assertNoPaneClosed()
    }

    func testSplitsMoveWithTheTab() {
        let split = SplitNode.split(axis: .horizontal, ratio: 0.5, first: .leaf(pane(2)), second: .leaf(pane(3)))
        let (tabs, _, hidden) = setUp(shown: [.leaf(pane(1)), split])
        tabs[1].content.focus(pane(3))
        tabs[1].content.zoom(pane(3), true)

        manager.moveTab(tabs[1], to: hidden)

        let moved = hidden.hiddenTabs.last
        XCTAssertEqual(moved?.panes, [pane(2), pane(3)])
        XCTAssertEqual(moved?.focusedKey, pane(3))
        XCTAssertEqual(moved?.zoomedKey, pane(3))
        assertNoPaneClosed()
    }

    func testFloatingPanesMoveWithTheTab() throws {
        let (tabs, _, hidden) = setUp(shown: [.leaf(pane(1))])
        let card = FloatingPane(pane: pane(4), frame: [0.1, 0.2, 0.5, 0.4])
        let c = manager.makeController(root: .leaf(pane(2)), focused: pane(2), zoomed: nil, title: nil, frame: nil,
                                       floating: [card], floatingHidden: true)
        manager.attachAsTab(c, to: tabs[0])

        manager.moveTab(c, to: hidden)

        let moved = try XCTUnwrap(hidden.hiddenTabs.last)
        XCTAssertEqual(moved.floating?.map(\.pane), [pane(4)])
        let frame = try XCTUnwrap(moved.floating?.first?.frame)
        for (a, b) in zip(frame, card.frame) { XCTAssertEqual(a, b, accuracy: 0.001) }
        XCTAssertEqual(moved.floatingHidden, true)
        XCTAssertEqual(moved.panes, [pane(2), pane(4)])
        assertNoPaneClosed()
    }

    func testLastTabTakesTheWindowToTheTarget() throws {
        let (tabs, shown, hidden) = setUp(shown: [.leaf(pane(1))])
        let originalWindow = tabs[0].window

        manager.moveTab(tabs[0], to: hidden)

        XCTAssertFalse(tabs[0].isClosed, "single-window sidebar mode keeps the window open")
        XCTAssertNil(manager.workspace(shown.id), "the emptied workspace goes")
        let now = try XCTUnwrap(manager.controllerShowing(hidden))
        XCTAssertTrue(now === tabs[0])
        XCTAssertTrue(now.window === originalWindow)
        XCTAssertEqual(now.content.paneIds, [pane(1)], "the window shows the moved tab")
        XCTAssertEqual(manager.windowTabs(of: now).flatMap(\.content.paneIds).sorted { $0.id < $1.id },
                       [pane(1), pane(9)])
        XCTAssertTrue(hidden.hiddenTabs.isEmpty)
        assertNoPaneClosed()
    }

    func testLastTabTakesTheWindowToTheTargetInNativeMode() throws {
        manager.config.sidebarTabs = false
        let (tabs, shown, hidden) = setUp(shown: [.leaf(pane(1))])

        manager.moveTab(tabs[0], to: hidden)

        XCTAssertTrue(tabs[0].isClosed)
        XCTAssertNil(manager.workspace(shown.id), "the emptied workspace goes")
        let now = try XCTUnwrap(manager.controllerShowing(hidden))
        XCTAssertEqual(now.content.paneIds, [pane(1)], "the window shows the moved tab")
        XCTAssertEqual(manager.group(of: now).flatMap(\.content.paneIds).sorted { $0.id < $1.id },
                       [pane(1), pane(9)])
        XCTAssertTrue(hidden.hiddenTabs.isEmpty)
        assertNoPaneClosed()
    }

    func testMovingThereAndBackLeavesOneCopy() throws {
        let (tabs, shown, hidden) = setUp(shown: [.leaf(pane(1)), .leaf(pane(2))])
        manager.moveTab(tabs[1], to: hidden)
        manager.switchWorkspace(in: tabs[0], to: hidden)
        let back = try XCTUnwrap(manager.tab(for: pane(2)))

        manager.moveTab(back, to: shown)

        XCTAssertEqual(shown.hiddenTabs.map(\.panes), [[pane(1)], [pane(2)]])
        XCTAssertEqual(manager.controllerShowing(hidden)?.content.paneIds, [pane(9)])
        XCTAssertEqual(manager.liveTabs.filter { $0.content.contains(pane(2)) }.count, 0)
        assertNoPaneClosed()
    }

    func testMovingThereAndBackLeavesOneCopyInNativeMode() throws {
        manager.config.sidebarTabs = false
        let (tabs, shown, hidden) = setUp(shown: [.leaf(pane(1)), .leaf(pane(2))])
        manager.moveTab(tabs[1], to: hidden)
        manager.switchWorkspace(in: tabs[0], to: hidden)
        let back = try XCTUnwrap(manager.controller(for: pane(2)))

        manager.moveTab(back, to: shown)

        XCTAssertEqual(shown.hiddenTabs.map(\.panes), [[pane(1)], [pane(2)]])
        XCTAssertEqual(manager.controllerShowing(hidden)?.content.paneIds, [pane(9)])
        XCTAssertEqual(manager.liveControllers.filter { $0.content.contains(pane(2)) }.count, 0)
        assertNoPaneClosed()
    }

    func testTargets() {
        let (tabs, shown, hidden) = setUp(shown: [.leaf(pane(1)), .leaf(pane(2))])
        let remote = manager.makeWorkspace(name: "remote", host: "devbox")
        remote.hiddenTabs = [TabLayout(title: nil, root: .pane(PaneKey("devbox", 1)), focused: 1, zoomed: nil)]

        XCTAssertEqual(manager.moveTargets(for: tabs[0]).map(\.id), [hidden.id],
                       "not its own workspace, nor one on another host")

        let elsewhere = manager.makeWorkspace(name: "elsewhere")
        let other = tab(.leaf(pane(5)), focused: pane(5))
        other.workspaceID = elsewhere.id
        XCTAssertFalse(manager.moveTargets(for: tabs[0]).contains { $0 === elsewhere }, "not one a window shows")

        tabs[1].wall = []
        XCTAssertTrue(manager.moveTargets(for: tabs[1]).isEmpty, "the agent wall stays")

        let quick = manager.makeController(root: .leaf(pane(6)), focused: pane(6), zoomed: nil, title: nil,
                                           frame: nil, quick: true)
        XCTAssertTrue(manager.moveTargets(for: quick).isEmpty, "the quick terminal stays")

        tabs[1].isClosed = true
        XCTAssertTrue(manager.moveTargets(for: tabs[1]).isEmpty, "a closed tab moves nowhere")

        manager.moveTab(tabs[0], to: shown)
        XCTAssertFalse(tabs[0].isClosed, "a tab does not move to its own workspace")
    }

    func testMenuListsTargetsOrSaysThereAreNone() {
        let (tabs, _, hidden) = setUp(shown: [.leaf(pane(1))])
        let menu = NSMenu()

        manager.fillMoveTabMenu(menu, for: tabs[0])
        XCTAssertEqual(menu.items.map(\.title), [hidden.name])

        manager.fillMoveTabMenu(menu, for: nil)
        XCTAssertEqual(menu.items.map(\.title), ["No Other Workspaces"])
        XCTAssertFalse(menu.items[0].isEnabled)
    }
}
