import AppKit
import XCTest
@testable import Thurm

final class ActivityGroupsTests: XCTestCase {
    private func agents(_ statuses: [AgentStatus]) -> [SidebarTab] {
        statuses.enumerated().map { index, status in
            var row = SidebarTab(controller: nil, title: "Agent", status: status, selected: false)
            row.pane = .local(UInt64(index + 1))
            return row
        }
    }

    private func panes(_ groups: [SidebarGroup]) -> [[UInt64]] {
        groups.map { $0.tabs.compactMap { $0.pane?.id } }
    }

    func testStatusGroupsKeepPaneOrderWithTabsLast() {
        let tab = SidebarTab(controller: nil, title: "build", selected: true)
        let groups = TabSidebarViewController.activityGroups(agents: agents([.working, .done, .needsInput, .working, .error]),
                                                            tabs: [tab])
        XCTAssertEqual(groups.map(\.name), ["Needs input", "Failed", "Done", "Working", "Tabs"])
        XCTAssertEqual(panes(groups), [[3], [5], [2], [1, 4], []])
        XCTAssertEqual(groups.last?.tabs, [tab])
        XCTAssertEqual(Set(groups.map(\.key)).count, groups.count)
        XCTAssertFalse(groups.contains { $0.key == TabSidebarViewController.pinnedKey })
    }

    func testStatusChangesMoveOnlyThatAgent() {
        let statuses: [AgentStatus] = [.working, .error, .done, .needsInput, .working, .error, .done, .needsInput]
        XCTAssertEqual(panes(TabSidebarViewController.activityGroups(agents: agents(statuses), tabs: [])),
                       [[4, 8], [2, 6], [3, 7], [1, 5], []])
        var changed = statuses
        changed[0] = .needsInput
        changed[3] = .working
        XCTAssertEqual(panes(TabSidebarViewController.activityGroups(agents: agents(changed), tabs: [])),
                       [[1, 8], [2, 6], [3, 7], [4, 5], []])
    }

    private func info(_ id: UInt64, progress: String? = nil, agent: String? = nil) -> PaneInfo {
        var d: [String: Any] = ["id": id]
        if let progress { d["progress"] = ["state": progress, "percent": 50] }
        if let agent { d["agent"] = ["name": "Codex", "status": agent] }
        return PaneInfo(json: d)!
    }

    func testTabRowShowsForProgressOutsideTheAgentsOrTheUnlistedSelectedPane() {
        let agent = PaneKey.local(1), shell = PaneKey.local(2)
        let tab = SidebarTab(controller: nil, title: "repo", selected: false)
        var selected = tab
        selected.selected = true
        var info = [agent: self.info(1, progress: "error", agent: "Working"), shell: self.info(2)]
        let row = { (tab: SidebarTab, focused: PaneKey) in
            TabSidebarViewController.activityTabRow(tab, panes: [agent, shell], focused: focused, listed: [agent],
                                                    info: info)
        }

        // The agent's own progress belongs to its row.
        XCTAssertNil(row(tab, agent))
        XCTAssertNil(row(selected, agent))
        // The selected tab shows while its focused pane has no agent row.
        XCTAssertNotNil(row(selected, shell))
        XCTAssertNil(row(selected, shell)?.progress)
        XCTAssertNil(row(tab, shell))

        info[shell] = self.info(2, progress: "normal")
        XCTAssertEqual(row(tab, agent)?.progress?.state, .normal)
        XCTAssertEqual(row(tab, shell)?.progress?.state, .normal)
    }
}

final class ActivityListViewTests: XCTestCase {
    private let manager = SessionManager.shared
    private var savedConfig = AppConfig()
    private var savedWorkspaces: [Workspace] = []
    private var savedPanes: [PaneKey: PaneInfo] = [:]
    private var controllers: [TerminalWindowController] = []

    override func setUp() {
        super.setUp()
        _ = NSApplication.shared
        savedConfig = manager.config
        manager.config = AppConfig()
        savedWorkspaces = manager.workspaces
        savedPanes = manager.panes
        manager.workspaces = []
        manager.panes = [:]
        manager.config.sidebarTabs = true
        manager.config.sidebarGroupByRepo = false
    }

    override func tearDown() {
        for controller in controllers {
            controller.closingWithoutConfirmation = true
            controller.window?.close()
        }
        controllers = []
        manager.config = savedConfig
        manager.workspaces = savedWorkspaces
        manager.panes = savedPanes
        Remotes.shared.pendingCloses = [:]
        super.tearDown()
    }

    private func setStatus(_ id: UInt64, _ status: AgentStatus) {
        manager.panes[.local(id)] = PaneInfo(json: ["id": id, "agent": ["name": "Codex", "status": status.rawValue]])!
    }

    /// Three tabs (panes 1, 2, 3) in one workspace, and pane 4 in a workspace no window shows.
    private func tabs() throws -> TerminalWindowController {
        let workspace = manager.makeWorkspace(name: "main")
        for id: UInt64 in [1, 2, 3] {
            let controller = manager.makeController(root: .leaf(.local(id)), focused: .local(id), zoomed: nil,
                                                     title: nil, frame: nil)
            controller.workspaceID = workspace.id
            if let previous = controllers.last { manager.attachAsTab(controller, to: previous) }
            controllers.append(controller)
        }
        let hidden = manager.makeWorkspace(name: "api")
        hidden.hiddenTabs = [TabLayout(title: nil, root: .pane(.local(4)), focused: 4, zoomed: nil)]
        return try XCTUnwrap(controllers.first)
    }

    private func outline(_ sidebar: TabSidebarViewController) throws -> NSOutlineView {
        let scroll = try XCTUnwrap(sidebar.view.subviews.compactMap { $0 as? NSScrollView }.first)
        return try XCTUnwrap(scroll.documentView as? NSOutlineView)
    }

    /// The rows as listed: a heading's name, an agent row's pane ("1"), or a tab's row ("tab").
    private func listed(_ outline: NSOutlineView) -> [String] {
        (0..<outline.numberOfRows).map { row in
            switch outline.item(atRow: row) {
            case let g as SidebarGroup: return g.name
            case let box as TabBox: return box.tab.pane.map { String($0.id) } ?? "tab"
            default: return "?"
            }
        }
    }

    /// By host: This Mac's workspaces with their tabs, then a remote that is not connected,
    /// with its state and none of its tabs. Clicking another workspace's tab switches to it, and
    /// the window it comes into lists the same rows with that tab selected.
    func testByHostListsEveryHostAndAClickedTabKeepsTheList() throws {
        let first = try tabs()
        let remote = manager.makeWorkspace(name: "far", host: "devbox")
        remote.hiddenTabs = [TabLayout(title: nil, root: .pane(PaneKey("devbox", 9)), focused: 9, zoomed: nil)]
        manager.config.remoteNames = ["devbox"]
        manager.config.sidebarByHost = true
        let sidebar = try XCTUnwrap(first.sidebar)
        sidebar.reload()
        let outline = try outline(sidebar)
        let rows = ["This Mac", "main", "tab", "tab", "tab", "api", "tab", "devbox"]
        XCTAssertEqual(listed(outline), rows)
        XCTAssertEqual((outline.item(atRow: 7) as? SidebarGroup)?.state, Remotes.shared.phaseLabel("devbox"))
        XCTAssertEqual(outline.selectedRow, 2)
        let title = try XCTUnwrap(sidebar.view.subviews.compactMap { $0 as? NSButton }.first)
        XCTAssertEqual(title.title, "This Mac")

        manager.config.sidebarHost = "devbox"
        sidebar.reload()
        XCTAssertEqual(listed(outline), ["devbox"])
        manager.config.sidebarHost = localHost
        sidebar.reload()
        XCTAssertEqual(listed(outline), Array(rows.dropLast()))
        manager.config.sidebarHost = nil
        sidebar.reload()

        click(6, in: outline)
        let api = try XCTUnwrap(manager.workspaces.first { $0.name == "api" })
        let shown = try XCTUnwrap(manager.controllerShowing(api))
        controllers += manager.group(of: shown).filter { c in !controllers.contains { $0 === c } }
        let other = try self.outline(try XCTUnwrap(shown.sidebar))
        XCTAssertEqual(listed(other), rows)
        XCTAssertEqual(other.selectedRow, 6)
    }

    func testActiveOnlyListsAgentsByStatusFromEveryWorkspace() throws {
        let first = try tabs()
        setStatus(1, .working)
        setStatus(2, .needsInput)
        setStatus(3, .idle)
        setStatus(4, .error)
        manager.config.sidebarActiveOnly = true
        let sidebar = try XCTUnwrap(first.sidebar)
        sidebar.reload()
        let outline = try outline(sidebar)
        XCTAssertEqual(listed(outline), ["Needs input", "2", "Failed", "4", "Working", "1"])
        let hidden = try XCTUnwrap(outline.item(atRow: 3) as? TabBox)
        XCTAssertTrue(hidden.tab.subtitle?.hasSuffix("api") == true, hidden.tab.subtitle ?? "")
        XCTAssertNil(hidden.tab.controller)
        // The focused agent pane of this window is the selection.
        XCTAssertEqual(outline.selectedRow, 5)
        XCTAssertFalse(sidebar.outlineView(outline, shouldSelectItem: try XCTUnwrap(outline.item(atRow: 0))))

        // A status change moves that agent alone; the others keep their order.
        setStatus(2, .working)
        sidebar.reload()
        XCTAssertEqual(listed(outline), ["Failed", "4", "Working", "1", "2"])

        // The selected tab shows while its focused pane has no agent row.
        setStatus(1, .idle)
        sidebar.reload()
        XCTAssertEqual(listed(outline), ["Failed", "4", "Working", "2", "Tabs", "tab"])
        XCTAssertEqual(outline.selectedRow, 5)

        manager.config.sidebarActiveOnly = false
        sidebar.reload()
        XCTAssertEqual(listed(outline), ["tab", "tab", "tab"])
    }

    private func click(_ row: Int, in outline: NSOutlineView) {
        outline.selectRowIndexes([row], byExtendingSelection: false)
        _ = outline.sendAction(outline.action, to: outline.target)
    }

    func testClickingAnAgentRowBringsItsPaneForward() throws {
        let first = try tabs()
        let hidden = try XCTUnwrap(manager.workspaces.first { $0.name == "api" })
        setStatus(2, .needsInput)
        setStatus(4, .error)
        manager.config.sidebarActiveOnly = true
        let sidebar = try XCTUnwrap(first.sidebar)
        sidebar.reload()
        let outline = try outline(sidebar)
        XCTAssertEqual(listed(outline), ["Needs input", "2", "Failed", "4", "Tabs", "tab"])

        // Another tab of this window.
        click(1, in: outline)
        let second = controllers[1]
        XCTAssertTrue(first.window?.tabGroup?.selectedWindow === second.window)
        XCTAssertEqual(second.focusedPane, .local(2))

        // A workspace no window shows: it comes into this window, with the agent's pane focused.
        click(3, in: outline)
        let shown = try XCTUnwrap(manager.controllerShowing(hidden))
        controllers += manager.group(of: shown).filter { c in !controllers.contains { $0 === c } }
        XCTAssertEqual(shown.focusedPane, .local(4))
    }

    func testAgentRowsHaveNoHoverCardAndWinTheSelectionOverTheirPinnedTab() throws {
        let first = try tabs()
        first.pinned = true
        setStatus(1, .working)
        manager.config.sidebarActiveOnly = true
        let sidebar = try XCTUnwrap(first.sidebar)
        sidebar.view.frame = NSRect(x: 0, y: 0, width: 240, height: 600)
        sidebar.reload()
        sidebar.view.layoutSubtreeIfNeeded()
        let outline = try outline(sidebar)
        XCTAssertEqual(listed(outline), ["Pinned", "tab", "Working", "1"])
        XCTAssertEqual(outline.selectedRow, 3)
        let tab = try XCTUnwrap(outline.view(atColumn: 0, row: 1, makeIfNecessary: true) as? TabRowView)
        let agent = try XCTUnwrap(outline.view(atColumn: 0, row: 3, makeIfNecessary: true) as? TabRowView)
        XCTAssertTrue(tab.showsHoverCard)
        XCTAssertFalse(agent.showsHoverCard)
    }

    func testAgeRefreshChangesOnlyTheShownAges() throws {
        let first = try tabs()
        setStatus(1, .working)
        setStatus(2, .done)
        manager.panes[.local(1)]?.agent?.updatedAtMs = 1_800_000_000_000
        manager.config.sidebarActiveOnly = true
        let sidebar = try XCTUnwrap(first.sidebar)
        sidebar.view.frame = NSRect(x: 0, y: 0, width: 240, height: 600)
        sidebar.reload()
        sidebar.view.layoutSubtreeIfNeeded()
        let outline = try outline(sidebar)
        XCTAssertEqual(listed(outline), ["Done", "2", "Working", "1"])
        let box = try XCTUnwrap(outline.item(atRow: 3) as? TabBox)
        let done = try XCTUnwrap(outline.item(atRow: 1) as? TabBox)

        sidebar.refreshAges(now: Date(timeIntervalSince1970: 1_800_000_020))
        XCTAssertTrue(outline.item(atRow: 3) as? TabBox === box)
        XCTAssertEqual(box.tab.trailingDetail, "updated 20s ago")
        XCTAssertNil(done.tab.trailingDetail)
        sidebar.refreshAges(now: Date(timeIntervalSince1970: 1_800_000_120))
        XCTAssertEqual(box.tab.trailingDetail, "updated 2m ago")
    }

    func testActivityRowsCannotReorderTabs() throws {
        let first = try tabs()
        setStatus(1, .working)
        let sidebar = try XCTUnwrap(first.sidebar)
        let outline = try outline(sidebar)
        sidebar.reload()
        XCTAssertNotNil(sidebar.outlineView(outline, pasteboardWriterForItem: try XCTUnwrap(outline.item(atRow: 0))))
        manager.config.sidebarActiveOnly = true
        sidebar.reload()
        XCTAssertEqual(listed(outline), ["Working", "1"])
        XCTAssertNil(sidebar.outlineView(outline, pasteboardWriterForItem: try XCTUnwrap(outline.item(atRow: 1))))
    }

    func testHeadingsLineUpWithTheWorkspaceTitle() throws {
        let first = try tabs()
        setStatus(1, .working)
        setStatus(2, .done)
        manager.config.sidebarActiveOnly = true
        let sidebar = try XCTUnwrap(first.sidebar)
        sidebar.view.frame = NSRect(x: 0, y: 0, width: 240, height: 600)
        sidebar.reload()
        sidebar.view.layoutSubtreeIfNeeded()
        let outline = try outline(sidebar)
        let heading = try XCTUnwrap(outline.view(atColumn: 0, row: 0, makeIfNecessary: true) as? SidebarHeadingView)
        XCTAssertEqual(heading.label.stringValue, "Done")
        heading.layoutSubtreeIfNeeded()
        XCTAssertEqual(heading.label.convert(NSPoint.zero, to: sidebar.view).x, SidebarHeadingView.inset, accuracy: 0.5)
        let icon = try XCTUnwrap(sidebar.view.subviews.compactMap { $0 as? NSImageView }.first)
        XCTAssertEqual(icon.frame.minX, SidebarHeadingView.inset, accuracy: 0.5)
        let button = try XCTUnwrap(sidebar.view.subviews.compactMap { $0 as? NSButton }.first)
        XCTAssertEqual(button.frame.minX, SidebarHeadingView.inset + SidebarHeadingView.hostLabelOffset, accuracy: 0.5)
        XCTAssertEqual(outline.rect(ofRow: 0).height, SidebarHeadingView.height, accuracy: 4)
    }

    func testActivityListDoesNotChangeTabsOrShortcuts() throws {
        let first = try tabs()
        let original = first.orderedTabs
        let tabGroup = try XCTUnwrap(first.window?.tabGroup)
        let shortcuts = [(1, original[0]), (2, original[1]), (3, original[2]), (9, original[2])]
        func check() {
            for activeOnly in [true, false] {
                manager.config.sidebarActiveOnly = activeOnly
                first.sidebar?.reload()
                XCTAssertEqual(first.orderedTabs, original)
                XCTAssertEqual(tabGroup.windows, original)
                for (number, target) in shortcuts {
                    // Start on another tab so a failed action cannot pass.
                    tabGroup.selectedWindow = target === original[0] ? original[1] : original[0]
                    let item = NSMenuItem()
                    item.tag = number
                    first.selectTabByNumber(item)
                    XCTAssertTrue(tabGroup.selectedWindow === target, "Shortcut \(number), active only \(activeOnly)")
                }
            }
        }
        setStatus(1, .working)
        setStatus(2, .needsInput)
        setStatus(3, .error)
        check()
        setStatus(1, .done)
        setStatus(2, .working)
        setStatus(3, .needsInput)
        check()
    }
}
