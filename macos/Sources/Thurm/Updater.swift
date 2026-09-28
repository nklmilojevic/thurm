import AppKit
import Sparkle

/// Automatic updates with Sparkle, from the appcast in `SUFeedURL` (set by release builds).
///
/// Two channels: *release* gets tagged releases; *tip* also gets a build of every commit to
/// main. `[updates]` in the config picks one (the menu writes it there); `auto`, the default,
/// is the channel of the running build, so a tip build keeps getting tip builds. The appcast
/// marks tip builds with `<sparkle:channel>tip</sparkle:channel>`, so the channel only changes
/// which items Sparkle may pick. Development builds carry no feed or key, and never update.
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

    private var controller: SPUStandardUpdaterController?

    /// `updates.channel`, with `auto` resolved to this build's channel.
    var channel: Channel {
        Channel(rawValue: SessionManager.shared.config.updateChannel) ?? Self.buildChannel
    }

    /// `ThurmChannel` in Info.plist, set by build.sh.
    private static var buildChannel: Channel {
        (Bundle.main.infoDictionary?["ThurmChannel"] as? String) == Channel.tip.rawValue ? .tip : .release
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
        configChanged()
    }

    /// Applies `[updates]`; called at start and after every config reload.
    func configChanged() {
        guard let updater = controller?.updater else { return }
        let cfg = SessionManager.shared.config
        if updater.automaticallyChecksForUpdates != cfg.checkForUpdates {
            updater.automaticallyChecksForUpdates = cfg.checkForUpdates
        }
        if updater.automaticallyDownloadsUpdates != cfg.downloadUpdates {
            updater.automaticallyDownloadsUpdates = cfg.downloadUpdates
        }
    }

    // MARK: Actions

    @objc func checkForUpdates(_ sender: Any?) {
        controller?.checkForUpdates(sender)
    }

    @objc func selectChannel(_ sender: NSMenuItem) {
        guard let picked = sender.representedObject as? String, let ch = Channel(rawValue: picked),
              ch != channel || SessionManager.shared.config.updateChannel != picked
        else { return }
        // Into the config file, like `thurm set updates.channel tip`; the daemon reloads us.
        let resp = Core.shared.request(object: ["SetSetting": ["key": "updates.channel", "value": "\"\(picked)\""]])
        if let d = resp as? [String: Any], let e = d["error"] as? String {
            SessionManager.shared.currentController?.content.focusedView?.showToast("Update channel: \(e)", duration: 6)
            return
        }
        SessionManager.shared.reloadConfig(notifyDaemon: false)
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
