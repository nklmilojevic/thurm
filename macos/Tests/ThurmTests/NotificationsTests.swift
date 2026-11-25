import AppKit
import UserNotifications
import XCTest
@testable import Thurm

final class NotificationsTests: XCTestCase {
    private func notice(_ id: String, pane: UInt64, host: String? = nil) -> (identifier: String, userInfo: [AnyHashable: Any]) {
        var info: [AnyHashable: Any] = [Notifications.paneKey: NSNumber(value: pane)]
        if let host { info[Notifications.hostKey] = host }
        return (id, info)
    }

    func testPicksTheNotificationsOfThePanesRead() {
        let delivered = [notice("a", pane: 1, host: localHost), notice("b", pane: 2, host: localHost),
                         notice("c", pane: 1, host: localHost)]
        XCTAssertEqual(Notifications.identifiers(of: delivered, for: [.local(1)]), ["a", "c"])
        XCTAssertEqual(Notifications.identifiers(of: delivered, for: [.local(1), .local(2)]), ["a", "b", "c"])
    }

    /// Pane ids are per daemon: the same number on another host is another pane.
    func testAnotherHostsPaneWithTheSameNumberStays() {
        let delivered = [notice("local", pane: 7, host: localHost), notice("remote", pane: 7, host: "devbox")]
        XCTAssertEqual(Notifications.identifiers(of: delivered, for: [.local(7)]), ["local"])
        XCTAssertEqual(Notifications.identifiers(of: delivered, for: [PaneKey("devbox", 7)]), ["remote"])
    }

    /// A notification posted without a host is this Mac's.
    func testNoHostMeansTheLocalDaemon() {
        XCTAssertEqual(Notifications.identifiers(of: [notice("a", pane: 3)], for: [.local(3)]), ["a"])
    }

    /// Thurm's own notifications (an update, a remote host) carry no pane.
    func testNotificationsWithoutAPaneStay() {
        let own: (identifier: String, userInfo: [AnyHashable: Any]) = ("update", [Notifications.urlKey: "https://example.com"])
        XCTAssertEqual(Notifications.identifiers(of: [own, notice("a", pane: 1)], for: [.local(1)]), ["a"])
        XCTAssertEqual(Notifications.identifiers(of: [own], for: []), [])
    }

    /// Clicking a notification routes local and remote panes to focusPane; dismiss does nothing.
    func testHandleResponseRoutesClicksAndActions() {
        var focused: [PaneKey] = []
        var opened: [URL] = []
        var answered: [(PaneKey, UInt64, Bool)] = []

        Notifications.handleResponse(
            userInfo: [Notifications.paneKey: NSNumber(value: 12), Notifications.hostKey: "studio"],
            action: UNNotificationDefaultActionIdentifier,
            openURL: { opened.append($0) },
            answerPermission: { answered.append(($0, $1, $2)); return true },
            focusPane: { focused.append($0) }
        )
        XCTAssertEqual(focused, [PaneKey("studio", 12)])

        Notifications.handleResponse(
            userInfo: [Notifications.paneKey: NSNumber(value: 3), Notifications.hostKey: localHost],
            action: UNNotificationDismissActionIdentifier,
            openURL: { opened.append($0) },
            answerPermission: { answered.append(($0, $1, $2)); return true },
            focusPane: { focused.append($0) }
        )
        XCTAssertEqual(focused, [PaneKey("studio", 12)], "dismissing should not steal focus")

        Notifications.handleResponse(
            userInfo: [Notifications.urlKey: "https://example.com"],
            action: UNNotificationDefaultActionIdentifier,
            openURL: { opened.append($0) },
            answerPermission: { answered.append(($0, $1, $2)); return true },
            focusPane: { focused.append($0) }
        )
        XCTAssertEqual(opened.map(\.absoluteString), ["https://example.com"])

        // When answering a permission fails (already answered in terminal), fall back to focusing.
        Notifications.handleResponse(
            userInfo: [Notifications.paneKey: NSNumber(value: 9), Notifications.hostKey: "studio",
                       Notifications.permissionKey: NSNumber(value: 42)],
            action: "approve",
            openURL: { opened.append($0) },
            answerPermission: { answered.append(($0, $1, $2)); return false },
            focusPane: { focused.append($0) }
        )
        XCTAssertEqual(answered.count, 1)
        XCTAssertEqual(answered[0].0, PaneKey("studio", 9))
        XCTAssertEqual(answered[0].1, 42)
        XCTAssertTrue(answered[0].2)
        XCTAssertEqual(focused.last, PaneKey("studio", 9))
    }

    /// In All Hosts mode, focusing a pane from a notification switches workspaces and tabs for
    /// both local and remote hosts, including split panes in hidden workspaces.
    func testFocusPaneSwitchesWorkspacesAndTabsInAllHostsMode() throws {
        _ = NSApplication.shared
        let manager = SessionManager.shared
        let savedWorkspaces = manager.workspaces
        let savedPanes = manager.panes
        let savedConfig = manager.config
        let savedIsConnected = TabSidebarViewController.isConnected
        defer {
            for c in manager.liveControllers {
                c.closingWithoutConfirmation = true
                c.window?.close()
            }
            manager.workspaces = savedWorkspaces
            manager.panes = savedPanes
            manager.config = savedConfig
            TabSidebarViewController.isConnected = savedIsConnected
        }

        manager.config = AppConfig()
        manager.config.sidebarTabs = true
        manager.config.sidebarByHost = true
        manager.config.sidebarHost = nil
        manager.config.remoteNames = ["studio"]
        TabSidebarViewController.isConnected = { _ in true }
        manager.workspaces = []
        manager.panes = [:]

        let localWS = manager.makeWorkspace(name: "This Mac")
        let remoteWS = manager.makeWorkspace(name: "studio", host: "studio")
        let remoteTab1 = PaneKey("studio", 10)
        let remoteSplitA = PaneKey("studio", 20)
        let remoteSplitB = PaneKey("studio", 21)
        remoteWS.hiddenTabs = [
            TabLayout(title: "shell", root: .pane(remoteTab1), focused: 10, zoomed: nil),
            TabLayout(title: "agent",
                      root: .split(dir: .right, ratio: 0.5, first: .pane(remoteSplitA), second: .pane(remoteSplitB)),
                      focused: 20, zoomed: nil),
        ]
        remoteWS.hiddenSelectedTab = 0

        let local1 = PaneKey.local(1)
        let local2 = PaneKey.local(2)
        let c = manager.makeController(
            tabs: [
                TabItem(root: .leaf(local1), focused: local1, zoomed: nil),
                TabItem(root: .leaf(local2), focused: local2, zoomed: nil),
            ],
            selectedIndex: 0,
            frame: NSRect(x: 100, y: 100, width: 900, height: 600)
        )
        c.workspaceID = localWS.id
        c.window?.layoutIfNeeded()
        XCTAssertEqual(c.focusedPane, local1)

        // 1. On local tab 1, click notification from local tab 2 (same workspace).
        manager.focusPane(local2, activate: false)
        XCTAssertEqual(c.workspaceID, localWS.id)
        XCTAssertEqual(c.selectedTabIndex, 1)
        XCTAssertEqual(c.focusedPane, local2)

        // 2. On local tab 2, click notification from remote split pane in hidden remote workspace.
        manager.focusPane(remoteSplitB, activate: false)
        XCTAssertEqual(c.workspaceID, remoteWS.id)
        XCTAssertEqual(c.selectedTabIndex, 1)
        XCTAssertEqual(c.focusedPane, remoteSplitB)
        XCTAssertEqual(manager.liveControllers.count, 1)

        // 3. On remote tab 2, click notification from local tab 1 (now parked/hidden).
        manager.focusPane(local1, activate: false)
        XCTAssertEqual(c.workspaceID, localWS.id)
        XCTAssertEqual(c.selectedTabIndex, 0)
        XCTAssertEqual(c.focusedPane, local1)
        XCTAssertEqual(manager.liveControllers.count, 1)

        // 4. On local tab 1, click notification from remote tab 1 (now parked/hidden).
        manager.focusPane(remoteTab1, activate: false)
        XCTAssertEqual(c.workspaceID, remoteWS.id)
        XCTAssertEqual(c.selectedTabIndex, 0)
        XCTAssertEqual(c.focusedPane, remoteTab1)
    }
}
