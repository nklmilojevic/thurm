import XCTest
@testable import Thurm

final class UsageLimitsTests: XCTestCase {
    func testClaudeUsage() throws {
        let json = """
        {"five_hour": {"utilization": 62.0, "resets_at": "2025-12-23T16:00:00.000Z"},
         "seven_day": {"utilization": 18, "resets_at": "2025-12-29T23:00:00+00:00"},
         "seven_day_opus": null, "extra_usage": {"is_enabled": false}}
        """
        let windows = try XCTUnwrap(UsageLimits.parseClaudeUsage(Data(json.utf8)))
        XCTAssertEqual(windows.map(\.label), ["5h", "wk"])
        XCTAssertEqual(windows.map(\.usedPercent), [62, 18])
        XCTAssertEqual(windows[0].resetsAt, Date(timeIntervalSince1970: 1_766_505_600))
        XCTAssertEqual(windows[1].resetsAt, Date(timeIntervalSince1970: 1_767_049_200))
    }

    /// A window without utilization (no use yet on a new plan) is left out, not shown as full.
    func testClaudeUsageSkipsEmptyWindows() throws {
        let json = #"{"five_hour": null, "seven_day": {"utilization": 4, "resets_at": null}}"#
        let windows = try XCTUnwrap(UsageLimits.parseClaudeUsage(Data(json.utf8)))
        XCTAssertEqual(windows, [UsageWindow(minutes: 10080, usedPercent: 4, resetsAt: nil)])
        XCTAssertNil(UsageLimits.parseClaudeUsage(Data("[]".utf8)))
    }

    func testClaudeCredentials() throws {
        let json = #"{"claudeAiOauth": {"accessToken": "t0k", "expiresAt": 1766505600000}}"#
        let c = try XCTUnwrap(UsageLimits.parseClaudeCredentials(Data(json.utf8)))
        XCTAssertEqual(c.token, "t0k")
        XCTAssertEqual(c.expiresAt, Date(timeIntervalSince1970: 1_766_505_600))
        // `security -w` prints hex for a value it takes for binary.
        let hex = Data(json.utf8).map { String(format: "%02x", $0) }.joined() + "\n"
        XCTAssertEqual(UsageLimits.parseClaudeCredentials(Data(hex.utf8))?.token, "t0k")
        // Only an MCP sign-in: nothing for usage.
        XCTAssertNil(UsageLimits.parseClaudeCredentials(Data(#"{"mcpOAuth": {}}"#.utf8)))
    }

    func testCodexSnapshotTakesTheLastOne() throws {
        let log = """
        {"timestamp":"2026-10-19T10:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","primary":{"used_percent":10.0,"window_minutes":300,"resets_at":1792876939},"secondary":{"used_percent":40.0,"window_minutes":10080,"resets_at":1793000000}}}}
        {"timestamp":"2026-10-19T10:01:00.000Z","type":"response_item","payload":{"type":"message"}}
        {"timestamp":"2026-10-19T10:02:00.000Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","primary":{"used_percent":12.5,"window_minutes":300,"resets_at":1792876939},"secondary":{"used_percent":41.0,"window_minutes":10080,"resets_at":1793000000}}}}
        {"timestamp":"2026-10-19T10:03:00.000Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex_other","primary":{"used_percent":99.0,"window_minutes":300}}}}
        {"timestamp":"2026-10-19T10:04:00.000Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":null}}
        {"timestamp":"2026-10-19T10:05:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"\\"rate_limits\\""}}
        {"truncated
        """
        let report = try XCTUnwrap(UsageLimits.codexSnapshot(Data(log.utf8)))
        XCTAssertEqual(report.windows, [
            UsageWindow(minutes: 300, usedPercent: 12.5, resetsAt: Date(timeIntervalSince1970: 1_792_876_939)),
            UsageWindow(minutes: 10080, usedPercent: 41, resetsAt: Date(timeIntervalSince1970: 1_793_000_000)),
        ])
        XCTAssertEqual(report.updated, Date(timeIntervalSince1970: 1_792_404_120))
    }

    /// Plans with one weekly window, and older Codex's `resets_in_seconds`.
    func testCodexSnapshotWeeklyOnlyAndRelativeReset() throws {
        let log = """
        {"timestamp":"2026-10-19T10:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":17.0,"window_minutes":10080,"resets_in_seconds":3600},"secondary":null}}}
        """
        let report = try XCTUnwrap(UsageLimits.codexSnapshot(Data(log.utf8)))
        XCTAssertEqual(report.windows.map(\.label), ["wk"])
        XCTAssertEqual(report.windows[0].resetsAt, Date(timeIntervalSince1970: 1_792_404_000 + 3600))
        XCTAssertNil(UsageLimits.codexSnapshot(Data("{}\n".utf8)))
    }

    func testWindowLabels() {
        XCTAssertEqual(UsageWindow(minutes: 300, usedPercent: 0).label, "5h")
        XCTAssertEqual(UsageWindow(minutes: 10080, usedPercent: 0).label, "wk")
        XCTAssertEqual(UsageWindow(minutes: 1440 * 30, usedPercent: 0).label, "30d")
        XCTAssertEqual(UsageWindow(minutes: 45, usedPercent: 0).label, "45m")
        XCTAssertEqual(UsageWindow(minutes: 300, usedPercent: 0).title, "5-hour")
        XCTAssertEqual(UsageWindow(minutes: 10080, usedPercent: 0).title, "Weekly")
    }

    func testLeftIsFullAfterTheReset() {
        let now = Date()
        let w = UsageWindow(minutes: 300, usedPercent: 62, resetsAt: now.addingTimeInterval(60))
        XCTAssertEqual(w.leftPercent(at: now), 38)
        XCTAssertEqual(w.leftPercent(at: now.addingTimeInterval(61)), 100)
        XCTAssertEqual(UsageWindow(minutes: 300, usedPercent: 130).leftPercent(at: now), 0)
    }

    func testRowModel() {
        let now = Date()
        let report = UsageReport(windows: [UsageWindow(minutes: 300, usedPercent: 62.4,
                                                       resetsAt: now.addingTimeInterval(3600)),
                                           UsageWindow(minutes: 10080, usedPercent: 18)],
                                 updated: now, problem: "Anthropic answered 500")
        let m = UsageRowModel(provider: .claude, report: report, now: now)
        XCTAssertEqual(m.gauges, [.init(label: "5h", left: 37), .init(label: "wk", left: 82)])
        XCTAssertTrue(m.stale)
        XCTAssertTrue(m.tooltip.hasPrefix("Claude\n5-hour: 37% left, resets "))
        XCTAssertTrue(m.tooltip.contains("Weekly: 82% left"))
        XCTAssertTrue(m.tooltip.contains("Anthropic answered 500"))
    }
}
