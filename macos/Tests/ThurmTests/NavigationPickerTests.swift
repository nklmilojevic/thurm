import AppKit
import XCTest
@testable import Thurm

final class NavigationPickerTests: XCTestCase {
    private let manager = SessionManager.shared
    private var savedWorkspaces: [Workspace] = []
    private var savedPanes: [PaneKey: PaneInfo] = [:]

    override func setUp() {
        super.setUp()
        _ = NSApplication.shared
        savedWorkspaces = manager.workspaces
        savedPanes = manager.panes
        manager.workspaces = []
        manager.panes = [:]
    }

    override func tearDown() {
        for c in manager.liveControllers {
            c.closingWithoutConfirmation = true
            c.window?.close()
        }
        manager.workspaces = savedWorkspaces
        manager.panes = savedPanes
        super.tearDown()
    }

    private func addPane(_ id: PaneKey, title: String = "Shell") {
        manager.panes[id] = PaneInfo(json: ["id": id.id, "title": title], host: id.host)!
    }

    private func tab(_ ids: [PaneKey], in ws: Workspace, title: String = "Task") -> TerminalWindowController {
        let root = ids.dropFirst().reduce(SplitNode.leaf(ids[0])) {
            .split(axis: .horizontal, ratio: 0.5, first: $0, second: .leaf($1))
        }
        let c = manager.makeController(root: root, focused: ids[0], zoomed: nil, title: title, frame: nil)
        c.workspaceID = ws.id
        return c
    }

    private func target(_ destination: NavigationTarget.Destination) throws -> NavigationTarget {
        try XCTUnwrap(manager.navigationTargets().first { $0.destination == destination })
    }

    func testListsSinglePaneTabsAgentsAndHiddenRemoteTargets() throws {
        let local = PaneKey.local(1), remote = PaneKey("devbox", 1)
        addPane(local, title: "Local task")
        addPane(remote, title: "Remote task")
        manager.panes[remote]?.cwd = "/tmp/checkout"
        manager.panes[remote]?.git = GitInfo(root: "/src/billing", branch: "fix-login", added: 0, removed: 0)
        manager.panes[remote]?.agent = AgentState(name: "Codex", kind: "codex", status: .working,
                                                 topic: "Repair login")
        let shown = manager.makeWorkspace(name: "Local work")
        let hidden = manager.makeWorkspace(name: "Deploy", host: "devbox")
        _ = tab([local], in: shown, title: "Editor")
        hidden.hiddenTabs = [TabLayout(title: "Release", root: .pane(remote), focused: 1, zoomed: nil)]
        hidden.lastActive = Date(timeIntervalSince1970: 1)

        let targets = manager.navigationTargets()
        XCTAssertEqual(targets.count, 6)
        XCTAssertEqual(targets.first?.destination, .workspace(shown.id))
        XCTAssertEqual(try target(.tab([local])).title, "Editor")
        XCTAssertEqual(try target(.tab([remote])).title, "Release")
        let agent = try target(.pane(remote))
        XCTAssertEqual(agent.title, "Repair login")
        XCTAssertTrue(agent.detail.hasPrefix("Agent · Deploy · devbox"))
        for text in ["Remote task", "billing", "/tmp/checkout", "fix-login", "Codex", "devbox"] {
            XCTAssertTrue(agent.searchText.contains(text), text)
            XCTAssertTrue(try target(.tab([remote])).searchText.contains(text), text)
        }
        XCTAssertTrue(try target(.pane(local)).detail.contains("local"))
    }

    func testPaneSelectionUsesCurrentWorkspaceAfterMove() throws {
        let pane = PaneKey.local(11), other = PaneKey.local(12)
        addPane(pane)
        addPane(other)
        let old = manager.makeWorkspace(name: "Old")
        let new = manager.makeWorkspace(name: "New")
        let c = tab([pane], in: old)
        let host = tab([other], in: old)
        manager.attachAsTab(host, to: c)
        let picked = try target(.pane(pane))
        manager.moveTab(c, to: new)

        manager.activateNavigationTarget(picked, from: host)

        XCTAssertEqual(manager.controller(for: pane)?.workspaceID, new.id)
        XCTAssertEqual(manager.controller(for: pane)?.focusedPane, pane)
        XCTAssertTrue(manager.isShown(new))
        XCTAssertFalse(manager.isShown(old))
    }

    func testHiddenRemotePaneWithSameNumberSelectsCorrectHost() throws {
        let local = PaneKey.local(1), remote = PaneKey("devbox", 1)
        addPane(local)
        addPane(remote)
        let shown = manager.makeWorkspace(name: "Local")
        let hidden = manager.makeWorkspace(name: "Remote", host: "devbox")
        let host = tab([local], in: shown)
        hidden.hiddenTabs = [TabLayout(title: "Remote task", root: .pane(remote), focused: 1, zoomed: nil)]

        manager.activateNavigationTarget(try target(.pane(remote)), from: host)

        XCTAssertEqual(manager.controller(for: remote)?.focusedPane, remote)
        XCTAssertEqual(manager.controller(for: remote)?.workspaceID, hidden.id)
        XCTAssertNil(manager.controller(for: local))
        XCTAssertEqual(shown.hiddenTabs.first?.panes, [local])
    }

    func testTabSelectionKeepsCurrentFocusAndSurvivesClosedPane() throws {
        let first = PaneKey.local(21), second = PaneKey.local(22)
        addPane(first)
        addPane(second)
        let ws = manager.makeWorkspace(name: "Work")
        let c = tab([first, second], in: ws)
        let picked = try target(.tab([first, second]))
        c.content.focus(second)
        manager.activateNavigationTarget(picked, from: c)
        XCTAssertEqual(c.focusedPane, second)

        _ = c.content.remove(pane: first)
        manager.panes.removeValue(forKey: first)
        manager.activateNavigationTarget(picked, from: c)
        XCTAssertEqual(c.focusedPane, second)
    }

    func testTabSelectionStaysWithOriginalTabWhenOnePaneMoves() throws {
        let first = PaneKey.local(23), second = PaneKey.local(24)
        addPane(first)
        addPane(second)
        let ws = manager.makeWorkspace(name: "Work")
        let c = tab([first, second], in: ws)
        let picked = try target(.tab([first, second]))
        _ = c.content.remove(pane: first)
        let moved = tab([first], in: ws)
        c.content.focus(second)
        c.window?.makeFirstResponder(nil)
        manager.activateNavigationTarget(picked, from: moved)
        XCTAssertEqual(c.focusedPane, second)
        XCTAssertTrue(c.window?.firstResponder === c.content.focusedView)
    }

    func testTabSelectionRejectsOriginalControllerAfterAllPickedPanesMove() throws {
        let first = PaneKey.local(25), second = PaneKey.local(26), replacement = PaneKey.local(27)
        for pane in [first, second, replacement] { addPane(pane) }
        let ws = manager.makeWorkspace(name: "Work")
        let original = tab([first, second], in: ws)
        let picked = try target(.tab([first, second]))
        original.content.setRoot(.leaf(replacement))
        let moved = tab([first, second], in: ws)
        moved.content.focus(second)
        original.window?.makeFirstResponder(nil)
        moved.window?.makeFirstResponder(nil)

        manager.activateNavigationTarget(picked, from: original)

        XCTAssertFalse(original.isClosed)
        XCTAssertEqual(original.content.paneIds, [replacement])
        XCTAssertTrue(moved.window?.firstResponder === moved.content.focusedView)
        XCTAssertEqual(moved.focusedPane, second)
        XCTAssertFalse(original.window?.firstResponder === original.content.focusedView)
    }

    func testHiddenTabSelectionPrefersSurvivingTabOverMovedShownPane() throws {
        let first = PaneKey.local(28), second = PaneKey.local(29), unrelated = PaneKey.local(30)
        for pane in [first, second, unrelated] { addPane(pane) }
        let hidden = manager.makeWorkspace(name: "Hidden")
        let shown = manager.makeWorkspace(name: "Shown")
        hidden.hiddenTabs = [TabLayout(title: "Task", root: .split(dir: .right, ratio: 0.5,
            first: .pane(first), second: .pane(second)), focused: first.id, zoomed: nil)]
        let picked = try target(.tab([first, second]))
        // Another tab now occupies the old index. Match surviving panes in the source workspace.
        hidden.hiddenTabs = [TabLayout(title: "Other", root: .pane(unrelated), focused: unrelated.id, zoomed: nil),
                             TabLayout(title: "Task", root: .pane(second), focused: second.id, zoomed: nil)]
        let moved = tab([first], in: shown)

        manager.activateNavigationTarget(picked, from: moved)

        let restored = try XCTUnwrap(manager.controller(for: second))
        XCTAssertEqual(restored.workspaceID, hidden.id)
        XCTAssertEqual(restored.focusedPane, second)
        XCTAssertTrue(restored.window?.firstResponder === restored.content.focusedView)
        XCTAssertFalse(manager.isShown(shown))
        XCTAssertEqual(shown.hiddenTabs.first?.panes, [first])
    }

    func testHiddenTabSelectionFollowsShownPaneWhenOriginalTabDisappears() throws {
        let pane = PaneKey.local(32), unrelated = PaneKey.local(33)
        addPane(pane)
        addPane(unrelated)
        let hidden = manager.makeWorkspace(name: "Hidden")
        let shown = manager.makeWorkspace(name: "Shown")
        hidden.hiddenTabs = [TabLayout(title: "Task", root: .pane(pane), focused: pane.id, zoomed: nil)]
        let picked = try target(.tab([pane]))
        hidden.hiddenTabs = [TabLayout(title: "Other", root: .pane(unrelated), focused: unrelated.id, zoomed: nil)]
        let moved = tab([pane], in: shown)
        moved.window?.makeFirstResponder(nil)

        manager.activateNavigationTarget(picked, from: moved)

        XCTAssertTrue(moved.window?.firstResponder === moved.content.focusedView)
        XCTAssertFalse(manager.isShown(hidden))
        XCTAssertTrue(manager.isShown(shown))
    }

    func testHiddenTabOpensWithoutAHostWindow() throws {
        let pane = PaneKey("devbox", 31)
        addPane(pane)
        let ws = manager.makeWorkspace(name: "Remote", host: "devbox")
        ws.hiddenTabs = [TabLayout(title: "Task", root: .pane(pane), focused: 31, zoomed: nil)]
        manager.activateNavigationTarget(try target(.tab([pane])), from: nil)
        XCTAssertEqual(manager.controller(for: pane)?.focusedPane, pane)
        XCTAssertTrue(ws.hiddenTabs.isEmpty)
    }

    func testClosedPaneAndDeletedWorkspaceDoNothing() throws {
        let pane = PaneKey.local(41)
        addPane(pane)
        let ws = manager.makeWorkspace(name: "Work")
        let c = tab([pane], in: ws)
        let picked = try target(.pane(pane))
        let workspace = try target(.workspace(ws.id))
        c.closingWithoutConfirmation = true
        c.window?.close()
        manager.panes.removeValue(forKey: pane)
        manager.workspaces = []

        manager.activateNavigationTarget(picked, from: c)
        manager.activateNavigationTarget(workspace, from: c)
        XCTAssertTrue(manager.liveControllers.isEmpty)
        XCTAssertTrue(manager.workspaces.isEmpty)
    }

    func testFloatingPaneSelectionRevealsThePane() throws {
        let base = PaneKey.local(51), floating = PaneKey.local(52)
        addPane(base)
        addPane(floating)
        let ws = manager.makeWorkspace(name: "Work")
        let c = manager.makeController(root: .leaf(base), focused: base, zoomed: nil, title: nil, frame: nil,
                                       floating: [FloatingPane(pane: floating, frame: [0.1, 0.1, 0.5, 0.5])],
                                       floatingHidden: true)
        c.workspaceID = ws.id
        manager.activateNavigationTarget(try target(.pane(floating)), from: c)
        XCTAssertEqual(c.focusedPane, floating)
        XCTAssertTrue(c.content.visiblePaneIds.contains(floating))
    }

    func testParkedSourceTabWinsOverPaneMovedIntoOldWindow() throws {
        let first = PaneKey.local(61), second = PaneKey.local(62), other = PaneKey.local(63)
        for p in [first, second, other] { addPane(p) }
        let savedSidebarTabs = manager.config.sidebarTabs
        manager.config.sidebarTabs = true
        defer { manager.config.sidebarTabs = savedSidebarTabs }

        let ws1 = manager.makeWorkspace(name: "Source")
        let ws2 = manager.makeWorkspace(name: "Target")
        ws2.hiddenTabs = [TabLayout(title: "Other", root: .pane(other), focused: other.id, zoomed: nil)]

        let c = tab([first, second], in: ws1, title: "Pair")
        let picked = try target(.tab([first, second]))
        // Park ws1 (switching the single window to ws2), then move `first` into ws2's window
        // while `second` stays in the parked source tab in ws1.
        manager.switchWorkspace(in: c, to: ws2)
        ws1.retainHiddenPanes { $0 != first }
        c.insertTab(TabItem(root: .leaf(first), focused: first, zoomed: nil, title: "Moved"), select: false)

        manager.activateNavigationTarget(picked, from: c)

        XCTAssertEqual(c.workspaceID, ws1.id)
        XCTAssertEqual(c.focusedPane, second)
        XCTAssertTrue(c.window?.firstResponder === c.content.focusedView)
    }
}
