import AppKit
import UserNotifications

/// The states of the notification matrix (`[notifications]` in config.toml).
enum NoticeState: String, CaseIterable {
    case working, waiting, blocked, finished, failed, idle
}

/// One state's row of the matrix: its tab indicator, notification, Dock badge and sound.
struct StateNotice: Equatable {
    var tabIndicator = true
    var notify = false
    var badge = false
    /// "default", "none", or a sound file's name.
    var sound = "none"
}

extension AgentState {
    var noticeState: NoticeState {
        switch status {
        case .working: return .working
        case .idle: return .idle
        case .needsInput: return permission == nil ? .waiting : .blocked
        case .done: return .finished
        case .error: return .failed
        }
    }
}

extension AppConfig {
    func notice(for agent: AgentState) -> StateNotice { notices[agent.noticeState] ?? StateNotice() }
}

/// Desktop notifications through UNUserNotificationCenter. Only works for a real, bundled and
/// signed app; when run as a bare executable (e.g. `swift run`) everything is a no-op, because
/// `UNUserNotificationCenter.current()` raises for processes without a bundle identifier.
final class Notifications: NSObject, UNUserNotificationCenterDelegate {
    static let shared = Notifications()

    static let paneKey = "pane"
    static let hostKey = "host"
    static let permissionKey = "permission"
    static let urlKey = "url"

    /// A coding agent's permission prompt, answerable from the notification.
    private static let permissionCategory = "agent-permission"
    private static let approveAction = "approve"
    private static let denyAction = "deny"

    private var authorizationRequested = false
    private(set) var authorized = false
    /// Notifications posted while the first authorization prompt is still open: sent once it is
    /// answered (posting before then loses them).
    private var waitingForAuthorization: [UNNotificationRequest]? = []

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
                let n = Notifications.shared
                n.authorized = granted
                let waiting = n.waitingForAuthorization ?? []
                n.waitingForAuthorization = nil
                if granted { waiting.forEach(n.add) }
            }
        }
    }

    /// Posts a notification for `pane`. Clicking it focuses the pane. With `permission` (the
    /// agent's permission prompt) it also offers to approve or deny it.
    func post(pane: PaneKey, title: String, body: String, sound: String = "default", permission: UInt64? = nil) {
        guard isAvailable else { return }
        requestAuthorizationIfNeeded()
        let content = UNMutableNotificationContent()
        content.title = title.isEmpty ? "Thurm" : title
        content.body = body
        switch sound {
        case "none": content.sound = nil
        case "default": content.sound = .default
        default: content.sound = UNNotificationSound(named: UNNotificationSoundName(sound))
        }
        content.userInfo = [Notifications.paneKey: pane.number, Notifications.hostKey: pane.host]
        var identifier = "pane-\(pane.host)-\(pane.id)-\(UUID().uuidString)"
        if let permission = permission {
            content.categoryIdentifier = Notifications.permissionCategory
            content.userInfo[Notifications.permissionKey] = NSNumber(value: permission)
            identifier = Notifications.permissionIdentifier(pane: pane)
        }
        let request = UNNotificationRequest(identifier: identifier, content: content, trigger: nil)
        if waitingForAuthorization != nil {
            waitingForAuthorization?.append(request)
        } else {
            add(request)
        }
    }

    /// Posts a notification of Thurm itself, not a pane's. One per `id`: a new one replaces it.
    /// Clicking it opens `url`.
    func post(id: String, title: String, body: String, url: URL?) {
        guard isAvailable else { return }
        requestAuthorizationIfNeeded()
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        if let url { content.userInfo = [Notifications.urlKey: url.absoluteString] }
        let request = UNNotificationRequest(identifier: id, content: content, trigger: nil)
        if waitingForAuthorization != nil {
            waitingForAuthorization?.append(request)
        } else {
            add(request)
        }
    }

    private func add(_ request: UNNotificationRequest) {
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

    /// The pane was read (here, or on a phone): none of its notifications is news any more.
    func withdraw(pane: PaneKey) {
        withdraw(panes: [pane])
    }

    /// These panes were read: their notifications come down, delivered or still waiting for
    /// the authorization prompt.
    func withdraw(panes: Set<PaneKey>) {
        guard isAvailable, !panes.isEmpty else { return }
        waitingForAuthorization?.removeAll {
            Notifications.pane(of: $0.content.userInfo).map(panes.contains) ?? false
        }
        let center = UNUserNotificationCenter.current()
        center.getDeliveredNotifications { delivered in
            let ids = Notifications.identifiers(
                of: delivered.map { (identifier: $0.request.identifier, userInfo: $0.request.content.userInfo) },
                for: panes)
            if !ids.isEmpty { center.removeDeliveredNotifications(withIdentifiers: ids) }
        }
    }

    /// The identifiers of the notifications that belong to one of `panes`. Thurm's own (an
    /// update, a remote host) belong to no pane and stay.
    static func identifiers(of notifications: [(identifier: String, userInfo: [AnyHashable: Any])],
                            for panes: Set<PaneKey>) -> [String] {
        notifications
            .filter { pane(of: $0.userInfo).map(panes.contains) ?? false }
            .map(\.identifier)
    }

    private static func pane(of info: [AnyHashable: Any]) -> PaneKey? {
        jsonUInt64(info[paneKey]).map { PaneKey((info[hostKey] as? String) ?? localHost, $0) }
    }

    /// Handles a clicked notification (or its approve/deny action) on the main thread.
    static func handleResponse(userInfo info: [AnyHashable: Any], action: String,
                               openURL: (URL) -> Void = { NSWorkspace.shared.open($0) },
                               answerPermission: (PaneKey, UInt64, Bool) -> Bool = { pane, permission, allow in
                                   let resp = Core.shared.request(object: ["AnswerPermission": [
                                       "pane": pane.number, "prompt": NSNumber(value: permission),
                                       "allow": allow,
                                   ]], host: pane.host)
                                   return resp != nil && (resp as? [String: Any])?["error"] == nil
                               },
                               focusPane: (PaneKey) -> Void = { SessionManager.shared.focusPane($0, activate: true) }) {
        guard action != UNNotificationDismissActionIdentifier else { return }
        if let url = (info[Notifications.urlKey] as? String).flatMap(URL.init(string:)) {
            openURL(url)
            return
        }
        guard let pane = Notifications.pane(of: info) else { return }
        if let permission = jsonUInt64(info[Notifications.permissionKey]),
           action == Notifications.approveAction || action == Notifications.denyAction {
            // Answered in the terminal meanwhile, or the agent moved on: show what it is at.
            if answerPermission(pane, permission, action == Notifications.approveAction) { return }
        }
        focusPane(pane)
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
        let action = response.actionIdentifier
        DispatchQueue.main.async {
            defer { completionHandler() }
            Notifications.handleResponse(userInfo: info, action: action)
        }
    }
}
