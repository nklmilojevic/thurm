import AppKit
import Carbon

/// Secure Keyboard Entry, modelled after Ghostty: `EnableSecureEventInput` /
/// `DisableSecureEventInput` are reference counted by the system, so this singleton makes sure
/// every enable is matched by exactly one disable.
///
/// Two sources can request it:
///  * the global user toggle (menu "Secure Keyboard Entry", persisted in UserDefaults), and
///  * a scoped automatic request while the focused pane shows a password prompt.
/// It is only ever active while the app is frontmost.
final class SecureInput {
    static let shared = SecureInput()

    /// Posted whenever `isActive` or `globalEnabled` changes.
    static let didChangeNotification = Notification.Name("ThurmSecureInputDidChange")

    private static let defaultsKey = "SecureKeyboardEntry"

    /// True while we hold one `EnableSecureEventInput` reference.
    private(set) var isActive = false

    /// User toggle.
    private(set) var globalEnabled = false

    /// Automatic request (password prompt in the focused pane).
    private(set) var scopedRequested = false

    private var appActive = true

    private init() {}

    func restoreUserPreference() {
        globalEnabled = UserDefaults.standard.bool(forKey: SecureInput.defaultsKey)
        appActive = NSApp.isActive
        apply()
    }

    func toggleGlobal() {
        setGlobal(!globalEnabled)
    }

    func setGlobal(_ enabled: Bool) {
        globalEnabled = enabled
        UserDefaults.standard.set(enabled, forKey: SecureInput.defaultsKey)
        apply()
    }

    func setScoped(_ requested: Bool) {
        guard requested != scopedRequested else { return }
        scopedRequested = requested
        apply()
    }

    func setAppActive(_ active: Bool) {
        appActive = active
        apply()
    }

    /// Releases our reference unconditionally (app termination).
    func releaseAll() {
        if isActive {
            _ = DisableSecureEventInput()
            isActive = false
        }
    }

    private func apply() {
        let want = appActive && (globalEnabled || scopedRequested)
        if want && !isActive {
            _ = EnableSecureEventInput()
            isActive = true
        } else if !want && isActive {
            _ = DisableSecureEventInput()
            isActive = false
        }
        NotificationCenter.default.post(name: SecureInput.didChangeNotification, object: self)
    }
}
