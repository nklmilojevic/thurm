import XCTest
@testable import Thurm

final class SidebarTests: XCTestCase {
    /// Distinct live objects standing in for the tabs' window controllers.
    private let a = NSObject(), b = NSObject(), c = NSObject(), d = NSObject()

    private func id(_ o: NSObject) -> ObjectIdentifier { ObjectIdentifier(o) }

    /// A retitled tab keeps its row: the same windows in the same groups and order.
    func testSameRowsKeepsRowsWhenOnlyContentChanged() {
        let shown = [[id(a), id(b)], [id(c)]]
        XCTAssertTrue(TabSidebarViewController.sameRows(shown, [[id(a), id(b)], [id(c)]]))
    }

    func testSameRowsRejectsSameLengthReorder() {
        let shown = [[id(a), id(b)], [id(c)]]
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(b), id(a)], [id(c)]]))
    }

    func testSameRowsRejectsSameLengthReplacement() {
        let shown = [[id(a), id(b)], [id(c)]]
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a), id(d)], [id(c)]]))
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a), id(b)], [id(d)]]))
    }

    func testSameRowsRejectsAddedOrRemovedTabs() {
        let shown = [[id(a), id(b)], [id(c)]]
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a), id(b), id(d)], [id(c)]]))
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a)], [id(c)]]))
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a), id(b)]]))
    }

    func testSameRowsRejectsRegrouping() {
        let shown = [[id(a), id(b)], [id(c)]]
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a)], [id(b), id(c)]]))
    }

    private func pane(_ id: UInt64, progress: String?, percent: Int? = nil, agent: String? = nil) -> PaneInfo {
        var d: [String: Any] = ["id": id]
        if let progress { d["progress"] = ["state": progress, "percent": percent as Any] }
        if let agent { d["agent"] = ["name": "Claude Code", "status": agent] }
        return PaneInfo(json: d)!
    }

    /// The focused pane's progress wins; without one, the first other pane reporting shows.
    func testTabProgressPrefersTheFocusedPane() {
        let one = PaneKey.local(1), two = PaneKey.local(2), three = PaneKey.local(3)
        let info = [one: pane(1, progress: nil),
                    two: pane(2, progress: "normal", percent: 40),
                    three: pane(3, progress: "error", percent: 90)]
        let order = [one, two, three]
        XCTAssertEqual(TabSidebarViewController.progress(focused: three, panes: order, info: info),
                       ProgressReport(state: .error, percent: 90))
        XCTAssertEqual(TabSidebarViewController.progress(focused: one, panes: order, info: info),
                       ProgressReport(state: .normal, percent: 40))
        XCTAssertNil(TabSidebarViewController.progress(focused: one, panes: [one], info: info))
    }

    /// A working agent's busy bar stays hidden here as on the pane, and does not hide another
    /// pane's progress.
    func testTabProgressSkipsAWorkingAgentsBusyBar() {
        let agent = PaneKey.local(1), build = PaneKey.local(2)
        var info = [agent: pane(1, progress: "indeterminate", agent: "Working")]
        XCTAssertNil(TabSidebarViewController.progress(focused: agent, panes: [agent], info: info))
        info[build] = pane(2, progress: "paused", percent: 10)
        XCTAssertEqual(TabSidebarViewController.progress(focused: agent, panes: [agent, build], info: info),
                       ProgressReport(state: .paused, percent: 10))
        info[agent] = pane(1, progress: "indeterminate", agent: "Idle")
        XCTAssertEqual(TabSidebarViewController.progress(focused: agent, panes: [agent, build], info: info)?.state,
                       .indeterminate)
    }

    /// Only a progress change moves the bar alone; any other change reconfigures the row.
    func testProgressOnlyChange() {
        let tab = SidebarTab(controller: nil, title: "cargo", selected: false)
        var moved = tab
        moved.progress = ProgressReport(state: .normal, percent: 50)
        XCTAssertTrue(moved.differsInProgressOnly(from: tab))
        XCTAssertFalse(tab.differsInProgressOnly(from: tab))
        var retitled = moved
        retitled.title = "cargo build"
        XCTAssertFalse(retitled.differsInProgressOnly(from: tab))
        XCTAssertNotEqual(moved, tab)
    }

    /// The row's bar shows only while there is progress, and setting it leaves the rest alone.
    func testRowBarFollowsProgress() {
        let row = TabRowView()
        var tab = SidebarTab(controller: nil, title: "cargo", selected: false)
        row.configure(tab)
        let bar = row.subviews.compactMap { $0 as? ProgressBarView }.first!
        XCTAssertTrue(bar.isHidden)
        tab.progress = ProgressReport(state: .normal, percent: 40)
        row.setProgress(tab.progress)
        XCTAssertFalse(bar.isHidden)
        XCTAssertEqual(bar.report, tab.progress)
        row.setProgress(nil)
        XCTAssertTrue(bar.isHidden)
    }

    /// Grouped, each repository's tabs follow its first tab, and tabs outside one come last.
    func testListingOrderGroupsByFirstAppearance() {
        let tabs = ["a1", "x1", "b1", "a2", "x2", "b2"]
        let repo: (String) -> String? = { $0.hasPrefix("x") ? nil : String($0.prefix(1)) }
        XCTAssertEqual(TabSidebarViewController.listingOrder(tabs, key: repo), ["a1", "a2", "b1", "b2", "x1", "x2"])
        XCTAssertEqual(TabSidebarViewController.listingOrder(tabs) { _ in nil }, tabs)
    }

    /// Off, or with every workspace listed (under the workspace), tabs keep their tab order.
    func testGroupingByRepositoryFollowsTheSettings() {
        let config = AppConfig()
        XCTAssertTrue(TabSidebarViewController.groupsByRepo(config))
        config.sidebarGroupByRepo = false
        XCTAssertFalse(TabSidebarViewController.groupsByRepo(config))
        config.sidebarGroupByRepo = true
        config.sidebarAllWorkspaces = true
        XCTAssertFalse(TabSidebarViewController.groupsByRepo(config))
        config.sidebarAllWorkspaces = false
        config.sidebarByHost = true
        XCTAssertFalse(TabSidebarViewController.groupsByRepo(config))
    }

    /// A remote workspace's header names its host, so the header doesn't change when a window
    /// switches to it.
    func testWorkspaceHeading() {
        XCTAssertEqual(TabSidebarViewController.heading(Workspace(id: 1, name: "api")), "api")
        XCTAssertEqual(TabSidebarViewController.heading(Workspace(id: 2, name: "api", host: "devbox")), "api (devbox)")
    }

    /// A host's header keeps its state whole beside a short name.
    func testHostHeadingShowsItsWholeState() {
        let heading = SidebarHeadingView("offbox", symbol: "globe", detail: "reconnecting")
        heading.frame = NSRect(x: 0, y: 0, width: 220, height: SidebarHeadingView.hostHeight)
        heading.layoutSubtreeIfNeeded()
        let detail = heading.subviews.compactMap { $0 as? NSTextField }.first { $0.stringValue == "reconnecting" }!
        XCTAssertGreaterThanOrEqual(detail.frame.width, detail.cell!.cellSize.width)
        XCTAssertLessThanOrEqual(heading.label.frame.maxX, detail.frame.minX)
    }

    /// A name as wide as the sidebar leaves the state half the row, and is cut short itself.
    func testHostHeadingKeepsItsStateBesideALongName() {
        let heading = SidebarHeadingView(String(repeating: "devbox-", count: 12), symbol: "globe",
                                         detail: "needs attention")
        heading.frame = NSRect(x: 0, y: 0, width: 220, height: SidebarHeadingView.hostHeight)
        heading.layoutSubtreeIfNeeded()
        let detail = heading.subviews.compactMap { $0 as? NSTextField }.first { $0.stringValue == "needs attention" }!
        XCTAssertGreaterThanOrEqual(detail.frame.width, min(detail.cell!.cellSize.width, 80))
        XCTAssertLessThan(heading.label.frame.width, heading.label.intrinsicContentSize.width)
        XCTAssertLessThanOrEqual(heading.label.frame.maxX, detail.frame.minX)
    }

    /// Under its host's header, a workspace goes without the host's name in front.
    func testWorkspaceNameUnderItsHost() {
        let name = { TabSidebarViewController.nameUnderHost(Workspace(id: 1, name: $0, host: $1)) }
        XCTAssertEqual(name("devbox-quiet-otter", "devbox"), "quiet-otter")
        XCTAssertEqual(name("devbox", "devbox"), "devbox")
        XCTAssertEqual(name("devbox-", "devbox"), "devbox-")
        XCTAssertEqual(name("api", "devbox"), "api")
        XCTAssertEqual(name("local-thing", localHost), "local-thing")
    }

    /// A tab is active while an agent in any of its panes isn't idle, or it shows progress.
    func testActiveTabs() {
        let shell = PaneKey.local(1), agent = PaneKey.local(2)
        var info = [shell: pane(1, progress: nil), agent: pane(2, progress: nil, agent: "Idle")]
        XCTAssertFalse(TabSidebarViewController.isActive([shell, agent], info: info, progress: nil))
        XCTAssertTrue(TabSidebarViewController.isActive([shell], info: info,
                                                        progress: ProgressReport(state: .normal, percent: 10)))
        for status in ["Working", "NeedsInput", "Done", "Error"] {
            info[agent] = pane(2, progress: nil, agent: status)
            XCTAssertTrue(TabSidebarViewController.isActive([shell, agent], info: info, progress: nil), status)
            XCTAssertFalse(TabSidebarViewController.isActive([shell], info: info, progress: nil), status)
        }
    }

    func testTabLayoutCodesPin() throws {
        let old = try JSONDecoder().decode(TabLayout.self, from: Data(#"{"root":{"type":"pane","id":1},"focused":1}"#.utf8))
        XCTAssertNil(old.pinned)
        XCTAssertFalse(String(decoding: try JSONEncoder().encode(old), as: UTF8.self).contains("pinned"))
        var tab = old
        tab.pinned = true
        let json = String(decoding: try JSONEncoder().encode(tab), as: UTF8.self)
        XCTAssertTrue(json.contains(#""pinned":true"#))
        XCTAssertEqual(try JSONDecoder().decode(TabLayout.self, from: Data(json.utf8)), tab)
    }

    func testPinnedTabsComeFirstInTabOrder() {
        let split = TabSidebarViewController.pinnedFirst(["a", "B", "c", "D"]) { $0.uppercased() == $0 }
        XCTAssertEqual(split.pinned, ["B", "D"])
        XCTAssertEqual(split.rest, ["a", "c"])
    }

    func testPinnedTabsReorderOnlyAmongThisWindowsPins() {
        let pinned = SidebarGroup(name: "Pinned", key: TabSidebarViewController.pinnedKey)
        let terminals = SidebarGroup(name: "Terminals")
        let elsewhere = SidebarGroup(name: "api", key: "workspace:2", elsewhere: true)
        let drop = { (group: SidebarGroup?, index: Int, pinned: Bool, own: Int) in
            TabSidebarViewController.allowsDrop(into: group, at: index, pinned: pinned, ownPinnedRows: own)
        }
        XCTAssertTrue(drop(pinned, 0, true, 2))
        XCTAssertTrue(drop(pinned, 2, true, 2))
        XCTAssertFalse(drop(pinned, 3, true, 2))
        XCTAssertFalse(drop(pinned, 1, false, 2))
        XCTAssertFalse(drop(pinned, NSOutlineViewDropOnItemIndex, true, 2))
        XCTAssertFalse(drop(terminals, 0, true, 2))
        XCTAssertTrue(drop(terminals, 0, false, 0))
        XCTAssertTrue(drop(nil, 0, false, 0))
        XCTAssertFalse(drop(elsewhere, 0, false, 0))
    }
}

final class SidebarHostsTests: XCTestCase {
    /// Workspaces in the order they were made: local 1, devbox 2, local 3, buildbox 4, and 5 on
    /// a host gone from the config.
    private let workspaces: [(id: UInt64, host: HostId)] = [(1, localHost), (2, "devbox"), (3, localHost),
                                                            (4, "buildbox"), (5, "old")]
    private let remotes = ["devbox", "buildbox", "staging"]

    private func sections(shown: UInt64?, filter: HostId? = nil,
                          offline: [HostId: String] = [:]) -> [SidebarHosts.Section] {
        SidebarHosts.sections(remotes: remotes, workspaces: workspaces, shown: shown, filter: filter) { offline[$0] }
    }

    /// This Mac first, then the remotes in config order, then hosts only workspaces still name;
    /// each host's workspaces in the order they were made.
    func testHostsInOrderWithTheirWorkspaces() {
        let s = sections(shown: 1)
        XCTAssertEqual(s.map(\.host), [localHost, "devbox", "buildbox", "staging", "old"])
        XCTAssertEqual(s.map(\.workspaces), [[1, 3], [2], [4], [], [5]])
        XCTAssertEqual(SidebarHosts.title(localHost), "This Mac")
        XCTAssertEqual(SidebarHosts.title("devbox"), "devbox")
    }

    /// A connected host without a workspace offers one; one with workspaces does not.
    func testEmptyConnectedHostOffersAWorkspace() {
        let s = sections(shown: 1)
        XCTAssertEqual(s.filter(\.offersNewWorkspace).map(\.host), ["staging"])
        XCTAssertNil(s.first { $0.host == "staging" }?.state)
    }

    /// A host that is not connected shows its state and lists nothing, but the workspace this
    /// window shows; it offers no workspace. This Mac is always connected.
    func testDisconnectedHostShowsItsStateAndNoRows() {
        let offline = ["buildbox": "connecting", "staging": "needs attention", localHost: "never"]
        var s = sections(shown: 1, offline: offline)
        XCTAssertEqual(s.first { $0.host == "buildbox" },
                       SidebarHosts.Section(host: "buildbox", state: "connecting", workspaces: [],
                                            offersNewWorkspace: false))
        XCTAssertEqual(s.first { $0.host == "staging" }?.offersNewWorkspace, false)
        XCTAssertNil(s.first?.state)
        s = sections(shown: 4, offline: offline)
        XCTAssertEqual(s.first { $0.host == "buildbox" }?.workspaces, [4])
    }

    /// Narrowed to one host, only it is listed; a host that is gone means every host.
    /// The chosen host is saved as a TOML string whatever its name.
    func testHostSavesAsATOMLString() {
        XCTAssertEqual(SessionManager.tomlString("devbox"), #""devbox""#)
        XCTAssertEqual(SessionManager.tomlString(""), #""""#)
        XCTAssertEqual(SessionManager.tomlString("dev\"box\\x\u{1}\n"), #""dev\"box\\x\u0001\n""#)
    }

    func testFilterToOneHost() {
        XCTAssertEqual(sections(shown: 1, filter: "devbox").map(\.host), ["devbox"])
        XCTAssertEqual(sections(shown: 1, filter: localHost).map(\.workspaces), [[1, 3]])
        XCTAssertEqual(sections(shown: 1, filter: "old").map(\.host), ["old"])
        XCTAssertEqual(sections(shown: 1, filter: "gone").count, 5)
        XCTAssertNil(SidebarHosts.effectiveFilter("gone", hosts: [localHost, "devbox"]))
        XCTAssertEqual(SidebarHosts.effectiveFilter("devbox", hosts: [localHost, "devbox"]), "devbox")
    }

    /// Switching to a workspace on another connected host lists the same thing: only the
    /// selection moves.
    func testSwitchingAcrossHostsKeepsTheListing() {
        let offline = ["buildbox": "connecting"]
        let before = sections(shown: 1, offline: offline)
        for shown: UInt64? in [2, 3, 5, nil] {
            XCTAssertEqual(sections(shown: shown, offline: offline), before, "shown \(String(describing: shown))")
        }
    }

    /// The popover: the list as before, every host, then each host; the choice is checked.
    func testPopoverRowsCheckTheCurrentChoice() {
        let hosts = [localHost, "devbox"]
        let state: (HostId) -> String? = { $0 == "devbox" ? "connecting" : nil }
        var rows = SidebarHostsPopover.rows(byHost: false, filter: nil, allWorkspaces: false, hosts: hosts, state: state)
        XCTAssertEqual(rows.map(\.title), ["This Workspace", "All Hosts", "This Mac", "devbox"])
        XCTAssertEqual(rows.map(\.checked), [true, false, false, false])
        XCTAssertEqual(rows[3].detail, "connecting")
        XCTAssertEqual(rows[2].choice, SidebarHostsPopover.Choice(byHost: true, host: localHost))
        rows = SidebarHostsPopover.rows(byHost: true, filter: nil, allWorkspaces: true, hosts: hosts, state: state)
        XCTAssertEqual(rows.first?.title, "All Workspaces")
        XCTAssertEqual(rows.map(\.checked), [false, true, false, false])
        rows = SidebarHostsPopover.rows(byHost: true, filter: "devbox", allWorkspaces: false, hosts: hosts, state: state)
        XCTAssertEqual(rows.map(\.checked), [false, false, false, true])
        rows = SidebarHostsPopover.rows(byHost: true, filter: "gone", allWorkspaces: false, hosts: hosts, state: state)
        XCTAssertEqual(rows.map(\.checked), [false, true, false, false])
    }

    /// Past its height limit, the hosts list scrolls to its last host; a short list is as tall
    /// as its rows.
    func testPopoverListScrollsPastItsHeight() throws {
        let hosts = [localHost] + (1...40).map { "host\($0)" }
        let rows = SidebarHostsPopover.rows(byHost: true, filter: nil, allWorkspaces: false, hosts: hosts) { _ in nil }
        let long = HostsListController(rows: rows, maxHeight: 300) { _ in }
        let scroll = try XCTUnwrap(long.view as? NSScrollView)
        XCTAssertEqual(scroll.frame.height, 300)
        XCTAssertEqual(long.preferredContentSize.height, 300)
        scroll.layoutSubtreeIfNeeded()
        let list = try XCTUnwrap(scroll.documentView)
        XCTAssertGreaterThan(list.frame.height, CGFloat(rows.count) * 26)
        let stack = try XCTUnwrap(list.subviews.first as? NSStackView)
        let frame = { (v: NSView) in v.convert(v.bounds, to: scroll.contentView) }
        let first = try XCTUnwrap(stack.arrangedSubviews.first), last = try XCTUnwrap(stack.arrangedSubviews.last)
        // It opens at the first row, and scrolls to the last.
        XCTAssertTrue(scroll.contentView.bounds.contains(frame(first)))
        XCTAssertFalse(scroll.contentView.bounds.contains(frame(last)))
        XCTAssertTrue(last.scrollToVisible(last.bounds))
        XCTAssertTrue(scroll.contentView.bounds.contains(frame(last)))

        let short = HostsListController(rows: Array(rows.prefix(3)), maxHeight: 300) { _ in }
        let fitted = try XCTUnwrap(short.view as? NSScrollView)
        XCTAssertLessThan(fitted.frame.height, 300)
        XCTAssertEqual(fitted.frame.height, try XCTUnwrap(fitted.documentView).fittingSize.height, accuracy: 1)
    }

    /// The popover names a host's state as its sidebar header does.
    func testPopoverStateMatchesTheHeader() {
        let manager = SessionManager.shared
        let saved = (manager.config, TabSidebarViewController.isConnected)
        defer { (manager.config, TabSidebarViewController.isConnected) = saved }
        manager.config.remoteNames = ["devbox", "up"]
        TabSidebarViewController.isConnected = { $0 == "up" }
        XCTAssertNil(SidebarHostsPopover.state(localHost))
        XCTAssertEqual(SidebarHostsPopover.state("up"), "connected")
        XCTAssertEqual(SidebarHostsPopover.state("devbox"), TabSidebarViewController.hostState("devbox"))
        XCTAssertEqual(SidebarHostsPopover.state("gone"), "not configured")
        XCTAssertEqual(TabSidebarViewController.hostState("gone"), "not configured")
    }
}

final class CommandHoldTests: XCTestCase {
    private var scheduled: [DispatchWorkItem] = []
    private lazy var hold = CommandHold { [unowned self] in scheduled.append($0) }

    /// Runs the reveals still due, as the main queue would after the delay.
    private func elapse() {
        for work in scheduled where !work.isCancelled { work.perform() }
        scheduled = []
    }

    func testHeldCommandShowsAfterTheDelay() {
        hold.update(.flagsChanged, .command)
        XCTAssertFalse(hold.held)
        elapse()
        XCTAssertTrue(hold.held)
        hold.update(.flagsChanged, [])
        XCTAssertFalse(hold.held)
    }

    func testReleaseBeforeTheDelayShowsNothing() {
        hold.update(.flagsChanged, .command)
        hold.update(.flagsChanged, [])
        elapse()
        XCTAssertFalse(hold.held)
    }

    func testKeyPressCancelsAndHides() {
        hold.update(.flagsChanged, .command)
        hold.update(.keyDown, .command)
        elapse()
        XCTAssertFalse(hold.held)
        hold.update(.flagsChanged, .command)
        elapse()
        XCTAssertTrue(hold.held)
        hold.update(.keyDown, .command)
        XCTAssertFalse(hold.held)
    }

    func testOtherModifiersCancel() {
        for other: NSEvent.ModifierFlags in [.shift, .option, .control, .function] {
            hold.update(.flagsChanged, [.command, other])
            elapse()
            XCTAssertFalse(hold.held, "\(other)")
            hold.update(.flagsChanged, .command)
            elapse()
            XCTAssertTrue(hold.held)
            hold.update(.flagsChanged, [.command, other])
            XCTAssertFalse(hold.held, "\(other)")
        }
    }

    func testLosingFocusHides() {
        hold.update(.flagsChanged, .command)
        elapse()
        NotificationCenter.default.post(name: NSApplication.didResignActiveNotification, object: nil)
        XCTAssertFalse(hold.held)
    }

    /// A row shows its hint while Command is held, and a row configured during the hold shows it too.
    func testRowFollowsTheHold() {
        let row = TabRowView(hold: hold)
        row.configure(SidebarTab(controller: nil, title: "zsh", shortcut: 4, selected: false))
        XCTAssertEqual(row.shownShortcut, "")
        hold.update(.flagsChanged, .command)
        elapse()
        XCTAssertEqual(row.shownShortcut, "⌘4")
        row.configure(SidebarTab(controller: nil, title: "zsh", shortcut: 2, selected: false))
        XCTAssertEqual(row.shownShortcut, "⌘2")
        hold.update(.flagsChanged, [])
        XCTAssertEqual(row.shownShortcut, "")
    }
}
