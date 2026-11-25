import AppKit
import XCTest
@testable import Thurm

/// Verifies single-window tab/workspace switching in sidebar mode (zero NSWindow churn)
/// and batched workspace switching in native-tab mode.
final class FastWorkspaceSwitchTests: XCTestCase {
    private let manager = SessionManager.shared
    private var savedWorkspaces: [Workspace] = []
    private var savedPanes: [PaneKey: PaneInfo] = [:]
    private var savedSidebarTabs = true
    private var savedSidebarByHost = false
    private var savedSidebarAllWorkspaces = false
    private var savedSidebarHost: HostId?
    private var savedSidebarActiveOnly = false
    private var savedRemoteNames: [HostId] = []
    private var savedIsConnected: (HostId) -> Bool = TabSidebarViewController.isConnected
    private var savedHistory = TabHistory()

    override func setUp() {
        super.setUp()
        _ = NSApplication.shared
        savedWorkspaces = manager.workspaces
        savedPanes = manager.panes
        savedSidebarTabs = manager.config.sidebarTabs
        savedSidebarByHost = manager.config.sidebarByHost
        savedSidebarAllWorkspaces = manager.config.sidebarAllWorkspaces
        savedSidebarHost = manager.config.sidebarHost
        savedSidebarActiveOnly = manager.config.sidebarActiveOnly
        savedRemoteNames = manager.config.remoteNames
        savedIsConnected = TabSidebarViewController.isConnected
        savedHistory = manager.tabHistory
        manager.config.sidebarTabs = true
        manager.config.sidebarByHost = false
        manager.config.sidebarAllWorkspaces = false
        manager.config.sidebarHost = nil
        manager.config.sidebarActiveOnly = false
        manager.config.remoteNames = []
        manager.workspaces = []
        manager.panes = [:]
        manager.tabHistory = TabHistory()
        Remotes.shared.pendingCloses = [:]
    }

    override func tearDown() {
        for c in manager.liveControllers {
            c.closingWithoutConfirmation = true
            c.window?.close()
        }
        manager.config.sidebarTabs = savedSidebarTabs
        manager.config.sidebarByHost = savedSidebarByHost
        manager.config.sidebarAllWorkspaces = savedSidebarAllWorkspaces
        manager.config.sidebarHost = savedSidebarHost
        manager.config.sidebarActiveOnly = savedSidebarActiveOnly
        manager.config.remoteNames = savedRemoteNames
        TabSidebarViewController.isConnected = savedIsConnected
        manager.workspaces = savedWorkspaces
        manager.panes = savedPanes
        manager.tabHistory = savedHistory
        Remotes.shared.pendingCloses = [:]
        super.tearDown()
    }

    private func pane(_ id: UInt64, host: HostId = localHost) -> PaneKey { PaneKey(host, id) }

    private func makeSingleWindow(workspace: Workspace, paneIDs: [UInt64],
                                  frame: NSRect = NSRect(x: 120, y: 140, width: 920, height: 640))
        -> TerminalWindowController
    {
        let items = paneIDs.map { id in
            TabItem(root: .leaf(pane(id, host: workspace.host)), focused: pane(id, host: workspace.host), zoomed: nil)
        }
        let c = manager.makeController(tabs: items, selectedIndex: 0, frame: frame)
        c.workspaceID = workspace.id
        c.window?.layoutIfNeeded()
        return c
    }

    // MARK: - Single-window sidebar tab & workspace switching

    func testSidebarTabAndWorkspaceSwitchingCreatesZeroNewWindows() throws {
        let localWS = manager.makeWorkspace(name: "This Mac")
        let remoteWS = manager.makeWorkspace(name: "devbox", host: "devbox")
        remoteWS.hiddenTabs = [
            TabLayout(title: "remote-shell", root: .pane(pane(101, host: "devbox")), focused: 101, zoomed: nil),
            TabLayout(title: "remote-logs", root: .pane(pane(102, host: "devbox")), focused: 102, zoomed: nil),
        ]
        remoteWS.hiddenSelectedTab = 1

        let c = makeSingleWindow(workspace: localWS, paneIDs: [1, 2, 3, 4, 5, 6])
        let window = try XCTUnwrap(c.window)
        let split = try XCTUnwrap(c.tabSplit)
        let sidebar = try XCTUnwrap(c.sidebar)
        let localTabItems = c.tabs
        let localContents = localTabItems.map(\.content)

        // Switching tabs inside the local workspace reuses the exact same NSWindow/sidebar.
        XCTAssertEqual(manager.liveControllers.count, 1)
        c.selectTab(localTabItems[3])
        XCTAssertEqual(c.selectedTabIndex, 3)
        XCTAssertTrue(c.content === localContents[3])
        XCTAssertTrue(localContents[3].window === window)
        XCTAssertNil(localContents[0].window, "inactive tab content is detached from the window hierarchy")
        XCTAssertEqual(manager.liveControllers.count, 1)

        // Next/Previous tab and Cmd+1…9 operate on the single window.
        (window as? TerminalWindow)?.selectNextTab(nil)
        XCTAssertEqual(c.selectedTabIndex, 4)
        (window as? TerminalWindow)?.selectPreviousTab(nil)
        XCTAssertEqual(c.selectedTabIndex, 3)

        let cmd1 = NSMenuItem()
        cmd1.tag = 1
        c.selectTabByNumber(cmd1)
        XCTAssertEqual(c.selectedTabIndex, 0)
        let cmd9 = NSMenuItem()
        cmd9.tag = 9
        c.selectTabByNumber(cmd9)
        XCTAssertEqual(c.selectedTabIndex, 5, "Cmd+9 selects the last tab")

        // Switch local -> remote ("This Mac" -> "devbox"): zero new windows, same sidebar.
        c.selectTab(localTabItems[2])
        manager.switchWorkspace(in: c, to: remoteWS)

        XCTAssertEqual(manager.liveControllers.count, 1)
        XCTAssertFalse(c.isClosed)
        XCTAssertTrue(c.window === window)
        XCTAssertTrue(c.tabSplit === split)
        XCTAssertTrue(c.sidebar === sidebar)
        XCTAssertEqual(c.workspaceID, remoteWS.id)
        XCTAssertEqual(c.tabs.count, 2)
        XCTAssertEqual(c.selectedTabIndex, 1)
        XCTAssertEqual(c.content.paneIds, [pane(102, host: "devbox")])
        XCTAssertEqual(c.titleOverride, "remote-logs")
        XCTAssertEqual(localWS.hiddenTabs.count, 6)
        XCTAssertEqual(localWS.hiddenSelectedTab, 2)
        XCTAssertEqual(localWS.cachedTabs?.count, 6, "local tabs stay warm in the workspace cache")

        let remoteTabItems = c.tabs
        let remoteContents = remoteTabItems.map(\.content)

        // Switch back remote -> local ("devbox" -> "This Mac"): reuses cached TabItems & TabContentViews!
        manager.switchWorkspace(in: c, to: localWS)

        XCTAssertEqual(manager.liveControllers.count, 1)
        XCTAssertTrue(c.window === window)
        XCTAssertTrue(c.tabSplit === split)
        XCTAssertTrue(c.sidebar === sidebar)
        XCTAssertEqual(c.workspaceID, localWS.id)
        XCTAssertEqual(c.selectedTabIndex, 2)
        XCTAssertEqual(c.tabs.count, 6)
        for (reused, original) in zip(c.tabs, localTabItems) {
            XCTAssertTrue(reused === original, "cached TabItem reused across workspace switch")
        }
        for (reused, original) in zip(c.tabs.map(\.content), localContents) {
            XCTAssertTrue(reused === original, "cached TabContentView reused across workspace switch")
        }
        XCTAssertTrue(c.content.window === window)
        XCTAssertNil(remoteContents[1].window)

        // Switch back to remote again: reuses cached remote TabItems too.
        manager.switchWorkspace(in: c, to: remoteWS)
        XCTAssertEqual(manager.liveControllers.count, 1)
        XCTAssertTrue(c.window === window)
        for (reused, original) in zip(c.tabs, remoteTabItems) {
            XCTAssertTrue(reused === original)
        }
        XCTAssertEqual(c.selectedTabIndex, 1)
    }

    func testWindowFrameSidebarStateMetadataFirstResponderAndLayoutPreservedAcrossSwitches() throws {
        manager.config.sidebarByHost = true
        let wsA = manager.makeWorkspace(name: "alpha")
        let wsB = manager.makeWorkspace(name: "beta")
        wsB.hiddenTabs = [
            TabLayout(title: "b-tab", root: .pane(pane(20)), focused: 20, zoomed: nil, pinned: true),
        ]

        // Register live panes so `buildLayout()` / `workspaceLayouts()` retains hidden tabs.
        let extraIDs: [UInt64] = Array(5...16)
        for id in ([1, 2, 3, 4, 20] as [UInt64]) + extraIDs {
            manager.panes[pane(id)] = PaneInfo(json: ["id": id, "title": "pane-\(id)"])
        }

        let frame = NSRect(x: 180, y: 200, width: 980, height: 340)
        let splitRoot = SplitNode.split(axis: .horizontal, ratio: 0.4,
                                        first: .leaf(pane(1)), second: .leaf(pane(2)))
        let card = FloatingPane(pane: pane(3), frame: [0.15, 0.2, 0.4, 0.35])
        let tab0 = TabItem(root: splitRoot, focused: pane(2), zoomed: pane(2),
                           floating: [card], floatingHidden: false, title: "custom-split")
        tab0.pinned = true
        let tab1 = TabItem(root: .leaf(pane(4)), focused: pane(4), zoomed: nil, title: "plain")
        let extraTabs = extraIDs.map { id in
            TabItem(root: .leaf(pane(id)), focused: pane(id), zoomed: nil, title: "extra-\(id)")
        }

        let c = manager.makeController(tabs: [tab0, tab1] + extraTabs, selectedIndex: 0, frame: frame)
        c.workspaceID = wsA.id
        let window = try XCTUnwrap(c.window)
        let split = try XCTUnwrap(c.tabSplit)
        let sidebar = try XCTUnwrap(c.sidebar)
        window.makeKeyAndOrderFront(nil)
        window.layoutIfNeeded()
        sidebar.reload()
        split.view.layoutSubtreeIfNeeded()

        // Adjust sidebar width & scroll offset after initial placement (as a user would).
        split.splitViewItems[0].isCollapsed = false
        sidebar.preferredWidth = nil
        split.splitView.setPosition(280, ofDividerAt: 0)
        split.view.layoutSubtreeIfNeeded()
        sidebar.scrollOffset = NSPoint(x: 0, y: 24)
        let savedWidth = split.sidebar.view.frame.width
        let savedScroll = sidebar.scrollOffset
        let savedFrame = window.frame
        XCTAssertEqual(savedWidth, 280, accuracy: 1.0)
        XCTAssertGreaterThan(savedScroll.y, 0, "sidebar has scrollable rows")

        // Switch tabs within wsA, then switch to wsB (same host list in All Hosts keeps rows & scroll).
        c.selectTab(tab1)
        XCTAssertTrue(window.firstResponder === tab1.content.focusedView)
        c.selectTab(tab0)
        XCTAssertTrue(window.firstResponder === tab0.content.focusedView)
        XCTAssertEqual(sidebar.scrollOffset.y, savedScroll.y, accuracy: 0.5,
                       "in-workspace tab switch preserves sidebar scroll offset")

        manager.switchWorkspace(in: c, to: wsB)
        XCTAssertEqual(window.frame, savedFrame)
        XCTAssertFalse(split.sidebarCollapsed)
        XCTAssertEqual(split.sidebar.view.frame.width, savedWidth, accuracy: 0.5)
        XCTAssertEqual(sidebar.scrollOffset.y, savedScroll.y, accuracy: 0.5,
                       "workspace switch preserves sidebar scroll offset")
        XCTAssertTrue(c.pinned)
        XCTAssertEqual(c.titleOverride, "b-tab")
        XCTAssertTrue(window.firstResponder === c.content.focusedView)

        // Collapse sidebar on wsB and verify collapse state persists across switching back to wsA.
        split.splitViewItems[0].isCollapsed = true

        // Layout serialization stays 100% compatible with session.json format.
        let layoutWhileOnB = manager.buildLayout()
        XCTAssertNotNil(layoutWhileOnB.jsonString())
        XCTAssertEqual(layoutWhileOnB.windows.count, 1)
        XCTAssertEqual(layoutWhileOnB.windows[0].workspace, wsB.id)
        XCTAssertEqual(layoutWhileOnB.windows[0].tabs.count, 1)
        XCTAssertEqual(layoutWhileOnB.windows[0].tabs[0].title, "b-tab")
        XCTAssertEqual(layoutWhileOnB.windows[0].tabs[0].pinned, true)
        let savedA = try XCTUnwrap(layoutWhileOnB.workspaces.first { $0.id == wsA.id })
        XCTAssertEqual(savedA.tabs.count, 2 + extraIDs.count)
        XCTAssertEqual(savedA.selectedTab, 0)
        XCTAssertEqual(savedA.tabs[0].title, "custom-split")
        XCTAssertEqual(savedA.tabs[0].pinned, true)
        XCTAssertEqual(savedA.tabs[0].focusedKey, pane(2))
        XCTAssertEqual(savedA.tabs[0].zoomedKey, pane(2))
        XCTAssertEqual(savedA.tabs[0].floating?.map { $0.pane }, [pane(3)])

        // Switch back to wsA and verify splits, floats, zoom, pin, title, and first responder.
        manager.switchWorkspace(in: c, to: wsA)
        XCTAssertEqual(window.frame, savedFrame)
        XCTAssertTrue(split.sidebarCollapsed)
        XCTAssertEqual(c.selectedTabIndex, 0)
        XCTAssertTrue(c.pinned)
        XCTAssertEqual(c.titleOverride, "custom-split")
        XCTAssertEqual(c.content.focusedPane, pane(2))
        XCTAssertEqual(c.content.zoomedPane, pane(2))
        XCTAssertEqual(c.content.floating.map(\.pane), [pane(3)])
        XCTAssertTrue(window.firstResponder === tab0.content.focusedView)

        // Find bar inside the active tab keeps keyboard focus when re-selecting or becoming key.
        tab0.content.showFindBar()
        let fieldEditor = try XCTUnwrap(window.firstResponder as? NSTextView)
        c.selectTab(tab0, focus: true)
        XCTAssertTrue(window.firstResponder === fieldEditor)
    }

    // MARK: - Live sidebarTabs toggle (sidebar <-> native)

    func testTogglingSidebarTabsPreservesTabsSelectionOrderSplitsPinsAndWorkspaces() throws {
        let wsA = manager.makeWorkspace(name: "alpha")
        let wsB = manager.makeWorkspace(name: "beta")
        wsB.hiddenTabs = [TabLayout(title: "bg", root: .pane(pane(99)), focused: 99, zoomed: nil)]

        let split = SplitNode.split(axis: .vertical, ratio: 0.5, first: .leaf(pane(2)), second: .leaf(pane(3)))
        let card = FloatingPane(pane: pane(4), frame: [0.1, 0.1, 0.4, 0.4])
        let t0 = TabItem(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: "first")
        t0.pinned = true
        let t1 = TabItem(root: split, focused: pane(3), zoomed: nil, floating: [card], title: "second")
        let t2 = TabItem(root: .leaf(pane(5)), focused: pane(5), zoomed: nil, title: "third")

        let c = manager.makeController(tabs: [t0, t1, t2], selectedIndex: 1, frame: NSRect(x: 100, y: 100, width: 900, height: 600))
        c.workspaceID = wsA.id
        XCTAssertEqual(manager.liveControllers.count, 1)
        XCTAssertTrue(c.usesSingleWindowTabs)

        // Toggle sidebar -> native.
        manager.config.sidebarTabs = false
        manager.applyConfigToUI()

        let nativeGroup = manager.group(of: c)
        XCTAssertEqual(nativeGroup.count, 3)
        XCTAssertFalse(c.usesSingleWindowTabs)
        XCTAssertEqual(nativeGroup.map { $0.titleOverride ?? "" }, ["first", "second", "third"])
        XCTAssertTrue(nativeGroup[0].pinned)
        XCTAssertEqual(nativeGroup[1].content.paneIds, [pane(2), pane(3), pane(4)])
        XCTAssertEqual(nativeGroup[1].content.focusedPane, pane(3))
        XCTAssertEqual(nativeGroup[1].content.floating.map(\.pane), [pane(4)])
        XCTAssertTrue(c.window?.tabGroup?.selectedWindow === nativeGroup[1].window)
        XCTAssertEqual(wsB.hiddenTabs.map(\.panes), [[pane(99)]])

        // Toggle native -> sidebar.
        manager.config.sidebarTabs = true
        manager.applyConfigToUI()

        XCTAssertEqual(manager.liveControllers.count, 1)
        let folded = try XCTUnwrap(manager.controllerShowing(wsA))
        XCTAssertTrue(folded.usesSingleWindowTabs)
        XCTAssertEqual(folded.tabs.count, 3)
        XCTAssertEqual(folded.selectedTabIndex, 1)
        XCTAssertEqual(folded.tabs.map { $0.titleOverride ?? "" }, ["first", "second", "third"])
        XCTAssertTrue(folded.tabs[0].pinned)
        XCTAssertEqual(folded.tabs[1].content.paneIds, [pane(2), pane(3), pane(4)])
        XCTAssertEqual(folded.tabs[1].content.focusedPane, pane(3))
        XCTAssertEqual(folded.tabs[1].content.floating.map(\.pane), [pane(4)])
        XCTAssertEqual(wsB.hiddenTabs.map(\.panes), [[pane(99)]])
    }

    // MARK: - Closing tabs & last tab close

    func testClosingHostedTabAndLastTabInSingleWindowSidebarMode() throws {
        let prev = manager.makeWorkspace(name: "prev")
        prev.hiddenTabs = [TabLayout(title: "home", root: .pane(pane(50)), focused: 50, zoomed: nil)]
        prev.lastActive = Date(timeIntervalSinceNow: -60)

        let active = manager.makeWorkspace(name: "active")
        let c = makeSingleWindow(workspace: active, paneIDs: [1, 2, 3])
        let window = try XCTUnwrap(c.window)
        c.selectTab(c.tabs[1])

        // Closing selected hosted tab removes only that tab and keeps the window open.
        XCTAssertFalse(c.windowShouldClose(window))
        XCTAssertFalse(c.isClosed)
        XCTAssertEqual(c.tabs.map(\.focusedPane), [pane(1), pane(3)])
        XCTAssertEqual(c.selectedTabIndex, 1)

        // Closing a background hosted tab leaves the selected tab active.
        manager.closeTab(c.tabs[0])
        XCTAssertFalse(c.isClosed)
        XCTAssertEqual(c.tabs.map(\.focusedPane), [pane(3)])
        XCTAssertEqual(c.selectedTabIndex, 0)

        // Closing the LAST tab of `active` switches cleanly to `prev` without closing the window!
        XCTAssertFalse(c.windowShouldClose(window))
        XCTAssertFalse(c.isClosed)
        XCTAssertTrue(c.window === window)
        XCTAssertNil(manager.workspace(active.id))
        XCTAssertEqual(c.workspaceID, prev.id)
        XCTAssertEqual(c.tabs.map(\.focusedPane), [pane(50)])
        XCTAssertTrue(prev.hiddenTabs.isEmpty)
    }

    // MARK: - Single-window sidebar features (reorder, move, float, wall, focusPane, background close)

    func testHostedTabReorderMoveFloatAndAgentWallUseOneWindow() throws {
        let wsA = manager.makeWorkspace(name: "a")
        let wsB = manager.makeWorkspace(name: "b")
        wsB.hiddenTabs = [TabLayout(title: nil, root: .pane(pane(90)), focused: 90, zoomed: nil)]

        let c = makeSingleWindow(workspace: wsA, paneIDs: [1, 2, 3])
        let window = try XCTUnwrap(c.window)

        // Drag-to-reorder preserves selected tab.
        c.selectTab(c.tabs[1]) // pane 2
        c.moveHostedTab(c.tabs[2], toIndex: 0) // [3, 1, 2]
        XCTAssertEqual(c.tabs.map(\.focusedPane), [pane(3), pane(1), pane(2)])
        XCTAssertEqual(c.selectedTab.focusedPane, pane(2))

        // Focus pane switches hosted tab in place.
        manager.focusPane(pane(3), activate: false)
        XCTAssertEqual(c.selectedTab.focusedPane, pane(3))
        XCTAssertEqual(manager.liveControllers.count, 1)

        // Float tab 1 into tab 3, then move floating pane back to a new tab.
        let tab1 = try XCTUnwrap(manager.tab(for: pane(1)))
        manager.floatTab(tab1, into: c.selectedTab)
        XCTAssertEqual(manager.liveControllers.count, 1)
        XCTAssertTrue(c.window === window)
        XCTAssertEqual(c.tabs.count, 2)
        XCTAssertTrue(c.content.isFloating(pane(1)))

        manager.moveCardToNewTab(pane(1), from: c.selectedTab)
        XCTAssertEqual(manager.liveControllers.count, 1)
        XCTAssertTrue(c.window === window)
        XCTAssertEqual(c.tabs.count, 3)

        // Move one hosted tab to wsB in background (zero window churn).
        let moving = try XCTUnwrap(manager.tab(for: pane(1)))
        manager.moveTab(moving, to: wsB)
        XCTAssertEqual(manager.liveControllers.count, 1)
        XCTAssertEqual(c.tabs.count, 2)
        XCTAssertEqual(wsB.hiddenTabs.map(\.panes), [[pane(90)], [pane(1)]])

        // AgentWall gather and put-back in single-window sidebar mode.
        manager.panes[pane(2)] = PaneInfo(json: ["id": UInt64(2), "title": "a2", "cwd": "/tmp", "alive": true,
                                                 "agent": ["name": "Claude", "status": "Working"]])
        manager.panes[pane(3)] = PaneInfo(json: ["id": UInt64(3), "title": "a3", "cwd": "/tmp", "alive": true,
                                                 "agent": ["name": "Claude", "status": "Working"]])
        manager.toggleAgentWall(in: c, on: true)
        XCTAssertEqual(manager.liveControllers.count, 1)
        XCTAssertTrue(c.window === window)
        XCTAssertNotNil(c.selectedTab.wall)
        XCTAssertEqual(Set(c.content.paneIds), Set([pane(2), pane(3)]))

        manager.toggleAgentWall(in: c, on: false)
        XCTAssertEqual(manager.liveControllers.count, 1)
        XCTAssertTrue(c.window === window)
        XCTAssertNil(c.selectedTab.wall)
        XCTAssertEqual(Set(c.tabs.flatMap(\.content.paneIds)), Set([pane(2), pane(3)]))

        // Removing a pane from a cached background workspace updates both hiddenTabs and cachedTabs.
        manager.switchWorkspace(in: c, to: wsB)
        XCTAssertEqual(wsA.cachedTabs?.count, 2)
        manager.removePaneFromUI(pane(2))
        XCTAssertEqual(wsA.hiddenTabs.count, 1)
        XCTAssertEqual(wsA.cachedTabs?.count, 1)
        XCTAssertEqual(wsA.hiddenTabs[0].panes, [pane(3)])
    }

    // MARK: - Fast native-tab workspace switching

    func testNativeWorkspaceSwitchBatchesCleanlyAndPreservesOrderSelectionAndFrame() throws {
        manager.config.sidebarTabs = false
        let wsA = manager.makeWorkspace(name: "alpha")
        let wsB = manager.makeWorkspace(name: "beta")
        wsB.hiddenTabs = [
            TabLayout(title: "b1", root: .pane(pane(11)), focused: 11, zoomed: nil),
            TabLayout(title: "b2", root: .pane(pane(12)), focused: 12, zoomed: nil),
            TabLayout(title: "b3", root: .pane(pane(13)), focused: 13, zoomed: nil),
        ]
        wsB.hiddenSelectedTab = 1

        let frame = NSRect(x: 160, y: 180, width: 900, height: 620)
        let a1 = manager.makeController(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: "a1", frame: frame)
        a1.workspaceID = wsA.id
        let a2 = manager.makeController(root: .leaf(pane(2)), focused: pane(2), zoomed: nil, title: "a2", frame: frame)
        manager.attachAsTab(a2, to: a1)
        a1.window?.tabGroup?.selectedWindow = a1.window

        XCTAssertFalse(manager.isSwitchingWorkspace)
        manager.switchWorkspace(in: a1, to: wsB)
        XCTAssertFalse(manager.isSwitchingWorkspace, "batch guard always resets after switch")

        XCTAssertTrue(a1.isClosed)
        XCTAssertTrue(a2.isClosed)
        XCTAssertEqual(wsA.hiddenTabs.map(\.panes), [[pane(1)], [pane(2)]])
        XCTAssertEqual(wsA.hiddenSelectedTab, 0)

        let shownB = try XCTUnwrap(manager.controllerShowing(wsB))
        let groupB = manager.group(of: shownB)
        XCTAssertEqual(groupB.map(\.content.paneIds), [[pane(11)], [pane(12)], [pane(13)]])
        XCTAssertEqual(shownB.content.paneIds, [pane(12)])
        XCTAssertEqual(shownB.window?.frame.size, frame.size)
        XCTAssertTrue(wsB.hiddenTabs.isEmpty)
    }

    // MARK: - Remote workspace focus & cross-workspace Cmd+1…9 shortcuts

    private func findOutline(in root: NSView) -> SidebarOutlineView? {
        if let o = root as? SidebarOutlineView { return o }
        for sub in root.subviews {
            if let found = findOutline(in: sub) { return found }
        }
        return nil
    }

    private func cmdMenuItem(_ number: Int) -> NSMenuItem {
        let item = NSMenuItem()
        item.tag = number
        return item
    }

    func testRemoteWorkspaceSwitchAndSidebarMouseDownRestoreTerminalFocus() throws {
        manager.config.sidebarByHost = true
        manager.config.remoteNames = ["devbox"]
        TabSidebarViewController.isConnected = { _ in true }

        let localWS = manager.makeWorkspace(name: "This Mac")
        let remoteWS = manager.makeWorkspace(name: "devbox", host: "devbox")
        remoteWS.hiddenTabs = [
            TabLayout(title: "remote-1", root: .pane(pane(101, host: "devbox")), focused: 101, zoomed: nil),
        ]

        let c = makeSingleWindow(workspace: localWS, paneIDs: [1, 2])
        let window = try XCTUnwrap(c.window)
        let sidebar = try XCTUnwrap(c.sidebar)
        sidebar.reload()
        let outline = try XCTUnwrap(findOutline(in: sidebar.view))

        // Cold switch from local -> remote via showHiddenTab focuses the remote pane immediately.
        manager.showHiddenTab(HiddenTab(workspace: remoteWS.id, index: 0), from: c)
        XCTAssertEqual(c.workspaceID, remoteWS.id)
        XCTAssertTrue(window.firstResponder === c.content.focusedView,
                      "cold switch to remote workspace must focus the remote terminal view")

        // Dispatch a synthetic mouse-down event through SidebarOutlineView.mouseDown(with:) on a
        // non-draggable tab row while the outline holds first responder; the override must restore
        // first responder to the active terminal once super.mouseDown returns.
        let nonDraggableRow = try XCTUnwrap((0..<outline.numberOfRows).first { row in
            (outline.item(atRow: row) as? TabBox)?.tab.hidden != nil
        })
        let rowRect = outline.rect(ofRow: nonDraggableRow)
        let windowPoint = outline.convert(NSPoint(x: rowRect.midX, y: rowRect.midY), to: nil)
        let now = ProcessInfo.processInfo.systemUptime
        let down = try XCTUnwrap(NSEvent.mouseEvent(with: .leftMouseDown, location: windowPoint,
                                                    modifierFlags: [], timestamp: now,
                                                    windowNumber: window.windowNumber, context: nil,
                                                    eventNumber: 1, clickCount: 1, pressure: 1))
        let up = try XCTUnwrap(NSEvent.mouseEvent(with: .leftMouseUp, location: windowPoint,
                                                  modifierFlags: [], timestamp: now + 0.01,
                                                  windowNumber: window.windowNumber, context: nil,
                                                  eventNumber: 2, clickCount: 1, pressure: 0))
        XCTAssertTrue(window.makeFirstResponder(outline))
        XCTAssertTrue(window.firstResponder === outline)
        NSApp.postEvent(up, atStart: false)
        outline.mouseDown(with: down)
        XCTAssertTrue(window.firstResponder === c.content.focusedView,
                      "SidebarOutlineView.mouseDown(with:) must restore first responder from sidebar outline to active terminal")

        // Keyboard focus on the sidebar (without mouseDown) stays untouched so Return/Space works.
        XCTAssertTrue(window.makeFirstResponder(outline))
        XCTAssertTrue(window.firstResponder === outline)

        // Cached round-trip: switch remote -> local -> remote keeps terminal focus each time.
        manager.showHiddenTab(HiddenTab(workspace: localWS.id, index: 1), from: c)
        XCTAssertEqual(c.workspaceID, localWS.id)
        XCTAssertEqual(c.selectedTabIndex, 1)
        XCTAssertTrue(window.firstResponder === c.content.focusedView)

        manager.showHiddenTab(HiddenTab(workspace: remoteWS.id, index: 0), from: c)
        XCTAssertEqual(c.workspaceID, remoteWS.id)
        XCTAssertTrue(window.firstResponder === c.content.focusedView,
                      "warm switch back to remote workspace must focus the remote terminal view")
    }

    func testCmdNumberSwitchesAcrossAllHostsInSidebarOrder() throws {
        manager.config.sidebarByHost = true
        manager.config.remoteNames = ["devbox", "offlinebox"]
        TabSidebarViewController.isConnected = { $0 == "devbox" }

        let localWS = manager.makeWorkspace(name: "This Mac")
        let remoteWS = manager.makeWorkspace(name: "devbox", host: "devbox")
        remoteWS.hiddenTabs = [
            TabLayout(title: "r1", root: .pane(pane(101, host: "devbox")), focused: 101, zoomed: nil),
            TabLayout(title: "r2", root: .pane(pane(102, host: "devbox")), focused: 102, zoomed: nil),
        ]
        let offlineWS = manager.makeWorkspace(name: "offlinebox", host: "offlinebox")
        offlineWS.hiddenTabs = [
            TabLayout(title: "off", root: .pane(pane(201, host: "offlinebox")), focused: 201, zoomed: nil),
        ]

        let c = makeSingleWindow(workspace: localWS, paneIDs: [1, 2, 3, 4, 5, 6])
        let window = try XCTUnwrap(c.window)
        let sidebar = try XCTUnwrap(c.sidebar)
        sidebar.reload()
        let outline = try XCTUnwrap(findOutline(in: sidebar.view))

        // All Hosts order: 6 local tabs (⌘1..⌘6), then 2 connected remote tabs (⌘7..⌘8).
        // Disconnected host's hidden tabs are omitted from the sidebar and shortcuts.
        XCTAssertEqual(TabSidebarViewController.shortcutTargets(for: c).count, 8)
        let rowShortcuts = (0..<outline.numberOfRows).compactMap { row -> Int? in
            (outline.item(atRow: row) as? TabBox)?.tab.shortcut
        }
        XCTAssertEqual(rowShortcuts, [1, 2, 3, 4, 5, 6, 7, 8])

        // Cmd+7 switches directly to devbox's first tab and focuses its pane.
        c.selectTabByNumber(cmdMenuItem(7))
        XCTAssertEqual(c.workspaceID, remoteWS.id)
        XCTAssertEqual(c.content.focusedPane, pane(101, host: "devbox"))
        XCTAssertTrue(window.firstResponder === c.content.focusedView)

        // Numbering stays identical after switching to devbox.
        let shortcutsAfterSwitch = (0..<outline.numberOfRows).compactMap { row -> Int? in
            (outline.item(atRow: row) as? TabBox)?.tab.shortcut
        }
        XCTAssertEqual(shortcutsAfterSwitch, [1, 2, 3, 4, 5, 6, 7, 8])

        // Cmd+9 jumps to the last tab in All Hosts (devbox tab 2).
        c.selectTabByNumber(cmdMenuItem(9))
        XCTAssertEqual(c.workspaceID, remoteWS.id)
        XCTAssertEqual(c.content.focusedPane, pane(102, host: "devbox"))
        XCTAssertTrue(window.firstResponder === c.content.focusedView)

        // While on devbox, Cmd+3 jumps straight back to This Mac's 3rd tab and focuses it.
        c.selectTabByNumber(cmdMenuItem(3))
        XCTAssertEqual(c.workspaceID, localWS.id)
        XCTAssertEqual(c.content.focusedPane, pane(3))
        XCTAssertTrue(window.firstResponder === c.content.focusedView)

        // Activity filter (sidebarActiveOnly) does not scramble All Hosts Cmd+1…9 order.
        manager.config.sidebarActiveOnly = true
        sidebar.reload()
        c.selectTabByNumber(cmdMenuItem(8))
        XCTAssertEqual(c.workspaceID, remoteWS.id)
        XCTAssertEqual(c.content.focusedPane, pane(102, host: "devbox"))
        manager.config.sidebarActiveOnly = false

        // Narrowing host filter to "devbox" scopes Cmd+1…9 to devbox's 2 tabs only.
        manager.config.sidebarHost = "devbox"
        sidebar.reload()
        XCTAssertEqual(TabSidebarViewController.shortcutTargets(for: c).count, 2)
        c.selectTabByNumber(cmdMenuItem(1))
        XCTAssertEqual(c.workspaceID, remoteWS.id)
        XCTAssertEqual(c.content.focusedPane, pane(101, host: "devbox"))
        c.selectTabByNumber(cmdMenuItem(9))
        XCTAssertEqual(c.content.focusedPane, pane(102, host: "devbox"))
    }

    func testCmdNumberSwitchesAcrossAllWorkspacesInSidebarOrder() throws {
        manager.config.sidebarAllWorkspaces = true
        manager.config.sidebarByHost = false

        let ws1 = manager.makeWorkspace(name: "one")
        let ws2 = manager.makeWorkspace(name: "two", host: "devbox")
        ws2.hiddenTabs = [
            TabLayout(title: "w2-1", root: .pane(pane(21, host: "devbox")), focused: 21, zoomed: nil),
            TabLayout(title: "w2-2", root: .pane(pane(22, host: "devbox")), focused: 22, zoomed: nil),
        ]
        let ws3 = manager.makeWorkspace(name: "three")
        ws3.hiddenTabs = [
            TabLayout(title: "w3-1", root: .pane(pane(31)), focused: 31, zoomed: nil),
        ]

        let c = makeSingleWindow(workspace: ws1, paneIDs: [1, 2])
        let window = try XCTUnwrap(c.window)
        let sidebar = try XCTUnwrap(c.sidebar)
        sidebar.reload()
        let outline = try XCTUnwrap(findOutline(in: sidebar.view))

        let rowShortcuts = (0..<outline.numberOfRows).compactMap { row -> Int? in
            (outline.item(atRow: row) as? TabBox)?.tab.shortcut
        }
        XCTAssertEqual(rowShortcuts, [1, 2, 3, 4, 5])

        // Cmd+4 -> ws2 tab 2, Cmd+5 / Cmd+9 -> ws3 tab 1, Cmd+1 -> ws1 tab 1.
        c.selectTabByNumber(cmdMenuItem(4))
        XCTAssertEqual(c.workspaceID, ws2.id)
        XCTAssertEqual(c.content.focusedPane, pane(22, host: "devbox"))
        XCTAssertTrue(window.firstResponder === c.content.focusedView)

        c.selectTabByNumber(cmdMenuItem(9))
        XCTAssertEqual(c.workspaceID, ws3.id)
        XCTAssertEqual(c.content.focusedPane, pane(31))
        XCTAssertTrue(window.firstResponder === c.content.focusedView)

        c.selectTabByNumber(cmdMenuItem(1))
        XCTAssertEqual(c.workspaceID, ws1.id)
        XCTAssertEqual(c.content.focusedPane, pane(1))
        XCTAssertTrue(window.firstResponder === c.content.focusedView)
    }

    func testBackgroundTabLayoutDoesNotInheritSelectedTabHandoffOrWallMetadata() throws {
        let wsA = manager.makeWorkspace(name: "alpha")
        let wsB = manager.makeWorkspace(name: "beta")
        wsB.hiddenTabs = [TabLayout(title: "b", root: .pane(pane(99)), focused: 99, zoomed: nil)]
        for id: UInt64 in [1, 2, 99] {
            manager.panes[pane(id)] = PaneInfo(json: ["id": id, "title": "p\(id)"])
        }

        let special = TabItem(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: "special")
        special.handoffID = "handoff-42"
        special.wall = [WallOrigin(index: 0, after: nil,
                                   tab: TabLayout(title: "origin", root: .pane(pane(1)), focused: 1, zoomed: nil))]
        special.pinned = true
        let ordinary = TabItem(root: .leaf(pane(2)), focused: pane(2), zoomed: nil, title: "ordinary")

        let c = manager.makeController(tabs: [special, ordinary], selectedIndex: 0,
                                       frame: NSRect(x: 100, y: 100, width: 900, height: 600))
        c.workspaceID = wsA.id

        // Persisting the active window must not copy `special`'s handoff/wall/pin onto `ordinary`.
        let saved = manager.buildLayout()
        XCTAssertEqual(saved.windows.count, 1)
        XCTAssertEqual(saved.windows[0].tabs.count, 2)
        XCTAssertEqual(saved.windows[0].tabs[0].handoff, "handoff-42")
        XCTAssertNotNil(saved.windows[0].tabs[0].wall)
        XCTAssertEqual(saved.windows[0].tabs[0].pinned, true)
        XCTAssertNil(saved.windows[0].tabs[1].handoff)
        XCTAssertNil(saved.windows[0].tabs[1].wall)
        XCTAssertNil(saved.windows[0].tabs[1].pinned)

        // Parking wsA into background cache must likewise preserve `ordinary` as an ordinary tab.
        manager.switchWorkspace(in: c, to: wsB)
        XCTAssertEqual(wsA.hiddenTabs.count, 2)
        XCTAssertEqual(wsA.hiddenTabs[0].handoff, "handoff-42")
        XCTAssertNotNil(wsA.hiddenTabs[0].wall)
        XCTAssertNil(wsA.hiddenTabs[1].handoff)
        XCTAssertNil(wsA.hiddenTabs[1].wall)
        XCTAssertNil(wsA.hiddenTabs[1].pinned)
    }

    func testParkedWorkspaceSelectionShiftsWhenEarlierCachedTabIsRemoved() throws {
        let wsA = manager.makeWorkspace(name: "alpha")
        let wsB = manager.makeWorkspace(name: "beta")
        wsB.hiddenTabs = [TabLayout(title: "b", root: .pane(pane(90)), focused: 90, zoomed: nil)]

        let c = makeSingleWindow(workspace: wsA, paneIDs: [1, 2, 3, 4])
        let tab3 = c.tabs[2] // pane 3 (index 2)
        c.selectTab(tab3)
        manager.switchWorkspace(in: c, to: wsB)
        XCTAssertEqual(wsA.hiddenSelectedTab, 2)

        // Removing an earlier single-pane tab (pane 1 at index 0) must keep tab3 selected (now index 1).
        XCTAssertFalse(wsA.removeSinglePaneTab(pane(1)))
        XCTAssertEqual(wsA.hiddenSelectedTab, 1)
        XCTAssertTrue(wsA.cachedTabs?[1] === tab3)

        // Dropping another earlier tab via retainHiddenPanes (pane 2 at index 0) shifts tab3 to index 0.
        wsA.retainHiddenPanes { $0 != pane(2) }
        XCTAssertEqual(wsA.hiddenSelectedTab, 0)
        XCTAssertTrue(wsA.cachedTabs?[0] === tab3)

        // Switching back selects tab3 (pane 3).
        manager.switchWorkspace(in: c, to: wsA)
        XCTAssertTrue(c.selectedTab === tab3)
        XCTAssertEqual(c.content.focusedPane, pane(3))
    }

    func testParkTabsShiftsSelectionWhenAnEarlierTabIsEmpty() throws {
        let ws = manager.makeWorkspace(name: "alpha")
        let empty = TabItem(root: .leaf(pane(1)), focused: pane(1), zoomed: nil, title: "empty")
        let keptA = TabItem(root: .leaf(pane(2)), focused: pane(2), zoomed: nil, title: "keptA")
        let keptB = TabItem(root: .leaf(pane(3)), focused: pane(3), zoomed: nil, title: "keptB")
        XCTAssertTrue(empty.content.remove(pane: pane(1)))
        XCTAssertTrue(empty.content.isEmpty)

        ws.parkTabs([empty, keptA, keptB], selected: 1)
        XCTAssertEqual(ws.hiddenTabs.count, 2)
        XCTAssertEqual(ws.hiddenSelectedTab, 0)
        let taken = try XCTUnwrap(ws.takeCachedTabs())
        XCTAssertEqual(taken.selected, 0)
        XCTAssertTrue(taken.tabs[taken.selected] === keptA)
    }

    func testSingleTabWindowTogglesCleanlyBetweenSidebarAndNativeModes() throws {
        let ws = manager.makeWorkspace(name: "solo")
        let c = makeSingleWindow(workspace: ws, paneIDs: [1])
        XCTAssertTrue(c.usesSingleWindowTabs)

        manager.config.sidebarTabs = false
        manager.applyConfigToUI()
        XCTAssertFalse(c.usesSingleWindowTabs)
        XCTAssertNil(c.tabSplit)
        XCTAssertEqual(c.content.focusedPane, pane(1))

        manager.config.sidebarTabs = true
        manager.applyConfigToUI()
        XCTAssertTrue(c.usesSingleWindowTabs)
        XCTAssertNotNil(c.tabSplit)
        XCTAssertEqual(c.content.focusedPane, pane(1))
    }
}
