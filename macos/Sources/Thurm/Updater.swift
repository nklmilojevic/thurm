import AppKit
import Sparkle

/// Automatic updates with Sparkle, from the appcast in `SUFeedURL` (set by release builds).
///
/// Two channels: *release* (the default) gets tagged releases; *tip* also gets a build of every
/// commit to main. The appcast marks tip builds with `<sparkle:channel>tip</sparkle:channel>`,
/// so choosing a channel only changes which items Sparkle may pick. Development builds carry no
/// feed or key, and never update.
///
/// After an update the new app finds the daemon still running the old build and replaces it
/// in place (see `SessionManager.connectAndHello`), so the shells keep running.
final class Updater: NSObject, SPUUpdaterDelegate, NSMenuItemValidation {
    static let shared = Updater()

    enum Channel: String, CaseIterable {
        case release, tip

        var title: String {
            switch self {
            case .release: "Release"
            case .tip: "Tip (every commit to main)"
            }
        }
    }

    private static let channelKey = "UpdateChannel"
    private var controller: SPUStandardUpdaterController?

    var channel: Channel {
        get { Channel(rawValue: UserDefaults.standard.string(forKey: Self.channelKey) ?? "") ?? .release }
        set { UserDefaults.standard.set(newValue.rawValue, forKey: Self.channelKey) }
    }

    func start() {
        let info = Bundle.main.infoDictionary ?? [:]
        guard let feed = info["SUFeedURL"] as? String, !feed.isEmpty,
              let key = info["SUPublicEDKey"] as? String, !key.isEmpty
        else {
            tlog("updates off: this build has no SUFeedURL / SUPublicEDKey")
            return
        }
        controller = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: self,
                                                  userDriverDelegate: nil)
    }

    // MARK: Actions

    @objc func checkForUpdates(_ sender: Any?) {
        controller?.checkForUpdates(sender)
    }

    @objc func selectChannel(_ sender: NSMenuItem) {
        guard let picked = sender.representedObject as? String, let ch = Channel(rawValue: picked),
              ch != channel
        else { return }
        channel = ch
        tlog("update channel: \(ch.rawValue)")
        // Look right away: someone switching to tip wants the latest build now.
        if ch == .tip { controller?.updater.checkForUpdatesInBackground() }
    }

    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        switch item.action {
        case #selector(checkForUpdates(_:)):
            return controller?.updater.canCheckForUpdates ?? false
        case #selector(selectChannel(_:)):
            item.state = (item.representedObject as? String) == channel.rawValue ? .on : .off
            return controller != nil
        default:
            return true
        }
    }

    /// Menu items for the app menu: Check for Updates… and the channel picker.
    func menuItems() -> [NSMenuItem] {
        let check = NSMenuItem(title: "Check for Updates…", action: #selector(checkForUpdates(_:)),
                               keyEquivalent: "")
        check.target = self
        let channels = NSMenu(title: "Update Channel")
        for ch in Channel.allCases {
            let item = NSMenuItem(title: ch.title, action: #selector(selectChannel(_:)), keyEquivalent: "")
            item.representedObject = ch.rawValue
            item.target = self
            channels.addItem(item)
        }
        let channelItem = NSMenuItem(title: "Update Channel", action: nil, keyEquivalent: "")
        channelItem.submenu = channels
        return [check, channelItem]
    }

    // MARK: SPUUpdaterDelegate

    func allowedChannels(for updater: SPUUpdater) -> Set<String> {
        channel == .tip ? ["tip"] : []
    }
}
