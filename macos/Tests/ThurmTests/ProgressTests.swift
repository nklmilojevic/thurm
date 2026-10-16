import XCTest
@testable import Thurm

final class ProgressTests: XCTestCase {
    private func pane(progress: String?, percent: Int? = nil, agent: String?) -> PaneInfo? {
        var d: [String: Any] = ["id": 1]
        if let progress { d["progress"] = ["state": progress, "percent": percent as Any] }
        if let agent { d["agent"] = ["name": "Claude Code", "status": agent] }
        return PaneInfo(json: d)
    }

    /// A program's progress shows. A working agent's busy bar does not: its dot already says it
    /// is working. An idle agent's dot is hidden, so its busy bar shows. A percentage always shows.
    func testWorkingAgentHidesTheBusyBar() {
        XCTAssertEqual(TerminalView.shownProgress(pane(progress: "indeterminate", agent: nil))?.state,
                       .indeterminate)
        XCTAssertNil(TerminalView.shownProgress(pane(progress: "indeterminate", agent: "Working")))
        XCTAssertEqual(TerminalView.shownProgress(pane(progress: "indeterminate", agent: "Idle"))?.state,
                       .indeterminate)
        XCTAssertEqual(TerminalView.shownProgress(pane(progress: "indeterminate", agent: "NeedsInput"))?.state,
                       .indeterminate)
        let percent = pane(progress: "normal", percent: 40, agent: "Working")
        XCTAssertEqual(TerminalView.shownProgress(percent)?.percent, 40)
        XCTAssertNil(TerminalView.shownProgress(pane(progress: nil, agent: nil)))
        XCTAssertNil(TerminalView.shownProgress(nil))
    }
}
