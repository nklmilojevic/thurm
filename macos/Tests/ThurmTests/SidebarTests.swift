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
