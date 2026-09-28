import AppKit
import UserNotifications

/// Desktop notifications through UNUserNotificationCenter. Only works for a real, bundled and
/// signed app; when run as a bare executable (e.g. `swift run`) everything is a no-op, because
/// `UNUserNotificationCenter.current()` raises for processes without a bundle identifier.
final class Notifications: NSObject, UNUserNotificationCenterDelegate {
    static let shared = Notifications()

    static let paneKey = "pane"

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
        UNUserNotificationCenter.current().delegate = self
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

    /// Posts a notification for `pane`. Clicking it focuses the pane.
    func post(pane: UInt64, title: String, body: String) {
        guard isAvailable else { return }
        requestAuthorizationIfNeeded()
        let content = UNMutableNotificationContent()
        content.title = title.isEmpty ? "Thurm" : title
        content.body = body
        content.sound = .default
        content.userInfo = [Notifications.paneKey: NSNumber(value: pane)]
        let request = UNNotificationRequest(identifier: "pane-\(pane)-\(UUID().uuidString)",
                                            content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request) { error in
            if let error = error {
                tlog("could not post notification: \(error.localizedDescription)")
            }
        }
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
        let pane = jsonUInt64(info[Notifications.paneKey])
        DispatchQueue.main.async {
            if let pane = pane {
                SessionManager.shared.focusPane(pane, activate: true)
            }
        }
        completionHandler()
    }
}
