import AppKit
import XCTest
@testable import Thurm

/// The Dock badge counts the agents of panes the app knows; a pane once closed stays gone.
final class DockBadgeTests: XCTestCase {
    private let manager = SessionManager.shared
    private let key = PaneKey(localHost, 900_001)

    override func setUp() {
        super.setUp()
        _ = NSApplication.shared
    }

    override func tearDown() {
        manager.panes[key] = nil
        manager.updateDockBadge()
        super.tearDown()
    }

    private func info(alive: Bool) -> [String: Any] {
        ["id": NSNumber(value: key.id), "title": "claude", "alive": alive,
         "agent": ["name": "Claude Code", "kind": "claude", "status": "Done"]]
    }

    /// An older daemon sends a closed pane's last PaneInfo after PaneClosed.
    func testAClosedPaneIsNotBroughtBackByItsLastPaneInfo() {
        manager.coreDidReceiveEvent("PaneInfo", payload: info(alive: true), host: localHost)
        XCTAssertNotNil(manager.panes[key])
        manager.coreDidReceiveEvent("PaneClosed", payload: ["pane": NSNumber(value: key.id)], host: localHost)
        manager.coreDidReceiveEvent("PaneInfo", payload: info(alive: false), host: localHost)
        XCTAssertNil(manager.panes[key])
    }

    /// A known pane that exits is still updated: a held pane shows its exit.
    func testAKnownPaneThatExitsIsUpdated() {
        manager.coreDidReceiveEvent("PaneInfo", payload: info(alive: true), host: localHost)
        manager.coreDidReceiveEvent("PaneInfo", payload: info(alive: false), host: localHost)
        XCTAssertEqual(manager.panes[key]?.alive, false)
    }

    /// A held pane outlives its agent: once it exits, its finished agent is off the badge.
    func testAnExitedPanesAgentIsOffTheBadge() {
        func count() -> Int { Int(NSApp.dockTile.badgeLabel ?? "") ?? 0 }
        manager.coreDidReceiveEvent("PaneInfo", payload: info(alive: true), host: localHost)
        let lit = count()
        XCTAssertGreaterThan(lit, 0)
        manager.coreDidReceiveEvent("PaneInfo", payload: info(alive: false), host: localHost)
        XCTAssertEqual(count(), lit - 1)
    }
}
