import AppKit
import UserNotifications

/// Desktop notifications through UNUserNotificationCenter. Only works for a real, bundled and
/// signed app; when run as a bare executable (e.g. `swift run`) everything is a no-op, because
/// `UNUserNotificationCenter.current()` raises for processes without a bundle identifier.
final class Notifications: NSObject, UNUserNotificationCenterDelegate {
    static let shared = Notifications()

    static let paneKey = "pane"
    static let hostKey = "host"
    static let permissionKey = "permission"

    /// A coding agent's permission prompt, answerable from the notification.
    private static let permissionCategory = "agent-permission"
    private static let approveAction = "approve"
    private static let denyAction = "deny"

    private var authorizationRequested = false
    private(set) var authorized = false

    var isAvailable: Bool {
        Bundle.main.bundleIdentifier != nil && Bundle.main.bundleURL.pathExtension == "app"
    }

    func setup() {
        guard isAvailable else {
            tlog("not running from an app bundle; desktop notifications disabled")
            return
        }
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        let approve = UNNotificationAction(identifier: Notifications.approveAction, title: "Approve",
                                           options: [.authenticationRequired])
        let deny = UNNotificationAction(identifier: Notifications.denyAction, title: "Deny",
                                        options: [.destructive])
        center.setNotificationCategories([
            UNNotificationCategory(identifier: Notifications.permissionCategory,
                                   actions: [approve, deny], intentIdentifiers: [], options: []),
        ])
    }

    private func requestAuthorizationIfNeeded() {
        guard isAvailable, !authorizationRequested else { return }
        authorizationRequested = true
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { granted, error in
            if let error = error {
                tlog("notification authorization failed: \(error.localizedDescription)")
            }
            DispatchQueue.main.async {
                Notifications.shared.authorized = granted
            }
        }
    }

    /// Posts a notification for `pane`. Clicking it focuses the pane. With `permission` (the
    /// agent's permission prompt) it also offers to approve or deny it.
    func post(pane: PaneKey, title: String, body: String, permission: UInt64? = nil) {
        guard isAvailable else { return }
        requestAuthorizationIfNeeded()
        let content = UNMutableNotificationContent()
        content.title = title.isEmpty ? "Thurm" : title
        content.body = body
        content.sound = .default
        content.userInfo = [Notifications.paneKey: pane.number, Notifications.hostKey: pane.host]
        var identifier = "pane-\(pane.host)-\(pane.id)-\(UUID().uuidString)"
        if let permission = permission {
            content.categoryIdentifier = Notifications.permissionCategory
            content.userInfo[Notifications.permissionKey] = NSNumber(value: permission)
            identifier = Notifications.permissionIdentifier(pane: pane)
        }
        let request = UNNotificationRequest(identifier: identifier, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request) { error in
            if let error = error {
                tlog("could not post notification: \(error.localizedDescription)")
            }
        }
    }

    /// One per pane: a new prompt replaces the last one's notification.
    private static func permissionIdentifier(pane: PaneKey) -> String { "permission-\(pane.host)-\(pane.id)" }

    /// The pane's permission prompt was answered (or went away): its buttons would do nothing.
    func withdrawPermission(pane: PaneKey) {
        guard isAvailable else { return }
        let id = Notifications.permissionIdentifier(pane: pane)
        UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: [id])
    }

    // MARK: UNUserNotificationCenterDelegate

    func userNotificationCenter(_ center: UNUserNotificationCenter,
                                willPresent notification: UNNotification,
                                withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .sound])
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter,
                                didReceive response: UNNotificationResponse,
                                withCompletionHandler completionHandler: @escaping () -> Void) {
        let info = response.notification.request.content.userInfo
        let id = jsonUInt64(info[Notifications.paneKey])
        let host = (info[Notifications.hostKey] as? String) ?? localHost
        let permission = jsonUInt64(info[Notifications.permissionKey])
        let action = response.actionIdentifier
        DispatchQueue.main.async {
            guard let id else { return }
            let pane = PaneKey(host, id)
            if let permission = permission,
               action == Notifications.approveAction || action == Notifications.denyAction {
                let resp = Core.shared.request(object: ["AnswerPermission": [
                    "pane": pane.number, "prompt": NSNumber(value: permission),
                    "allow": action == Notifications.approveAction,
                ]], host: pane.host)
                // Answered in the terminal meanwhile, or the agent moved on: show what it is at.
                if resp != nil && (resp as? [String: Any])?["error"] == nil { return }
            }
            SessionManager.shared.focusPane(pane, activate: true)
        }
        completionHandler()
    }
}
