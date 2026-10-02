import AppKit
import Sparkle

/// Automatic updates with Sparkle, from the appcast in `SUFeedURL` (set by release builds).
///
/// Two channels: *release* gets tagged releases; *tip* also gets the nightly build of main.
/// `[updates]` in the config picks one (the menu writes it there); `auto`, the default, is the
/// channel of the running build, so a tip build keeps getting tip builds. The appcast
/// marks tip builds with `<sparkle:channel>tip</sparkle:channel>`, so the channel only changes
/// which items Sparkle may pick. Development builds carry no feed or key, and never update.
///
/// Scheduled checks don't open Sparkle's window over the terminal: an `UpdateBadge` in the bottom
/// right of each window says an update is waiting, and clicking it opens the window. Checks
/// from the menu open the window right away.
///
/// After an update the new app finds the daemon still running the old build and replaces it
/// in place (see `SessionManager.connectAndHello`), so the shells keep running.
final class Updater: NSObject, SPUUpdaterDelegate, SPUStandardUserDriverDelegate, NSMenuItemValidation {
    static let shared = Updater()

    enum Channel: String, CaseIterable {
        case release, tip

        var title: String {
            switch self {
            case .release: "Release"
            case .tip: "Tip (nightly builds of main)"
            }
        }
    }

    private var controller: SPUStandardUpdaterController?

    /// Text of the update badge while a scheduled update waits for the user, else nil.
    private(set) var badgeText: String?

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
                                                  userDriverDelegate: self)
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

    /// Also brings a waiting scheduled update into focus (the badge's action).
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

    /// Sparkle always adds default-channel (release) items to the allowed channels. A release
    /// is published before the nightly tip build of its commit, so it would briefly be the
    /// newest item and move a tip app onto the release build. On tip, pick only tip items;
    /// Sparkle has already dropped the ones this system can't run.
    func bestValidUpdate(in appcast: SUAppcast, for updater: SPUUpdater) -> SUAppcastItem? {
        guard channel == .tip else { return nil }
        let comparator = SUStandardVersionComparator.default
        let tips = appcast.items.filter { $0.channel == Channel.tip.rawValue }
        return tips.max { comparator.compareVersion($0.versionString, toVersion: $1.versionString) == .orderedAscending }
    }

    // MARK: SPUStandardUserDriverDelegate

    var supportsGentleScheduledUpdateReminders: Bool { true }

    @objc(standardUserDriverShouldHandleShowingScheduledUpdate:andInImmediateFocus:)
    func standardUserDriverShouldHandleShowingScheduledUpdate(_ update: SUAppcastItem,
                                                              andInImmediateFocus immediateFocus: Bool) -> Bool {
        false
    }

    @objc(standardUserDriverWillHandleShowingUpdate:forUpdate:state:)
    func standardUserDriverWillHandleShowingUpdate(_ handleShowingUpdate: Bool, forUpdate update: SUAppcastItem,
                                                   state: SPUUserUpdateState) {
        guard !handleShowingUpdate else { return }
        let version = update.displayVersionString
        setBadge(state.stage == .notDownloaded ? "Update available: \(version)" : "Update ready: \(version)")
    }

    @objc(standardUserDriverDidReceiveUserAttentionForUpdate:)
    func standardUserDriverDidReceiveUserAttention(forUpdate update: SUAppcastItem) {
        setBadge(nil)
    }

    @objc(standardUserDriverWillFinishUpdateSession)
    func standardUserDriverWillFinishUpdateSession() {
        setBadge(nil)
    }

    private func setBadge(_ text: String?) {
        badgeText = text
        if let text { tlog("update badge: \(text)") }
        for c in SessionManager.shared.liveControllers {
            c.content.setUpdateBadge(text)
        }
    }
}

/// "Update available: 1.2.0" pill in the bottom right of a window; a click opens Sparkle's
/// update window. Colors follow the terminal theme.
final class UpdateBadge: NSView {
    private let dot = NSView()
    private let label = NSTextField(labelWithString: "")

    var text: String {
        get { label.stringValue }
        set {
            label.stringValue = newValue
            toolTip = "\(newValue). Click to see what's new and install."
            needsLayout = true
        }
    }

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.cornerRadius = 11
        layer?.borderWidth = 1
        dot.wantsLayer = true
        dot.layer?.cornerRadius = 3
        label.font = .systemFont(ofSize: 11, weight: .medium)
        addSubview(dot)
        addSubview(label)
        applyTheme()
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    func applyTheme() {
        let theme = SessionManager.shared.config.theme
        let bg = colorFromRGB(theme.background)
        let fg = colorFromRGB(theme.foreground)
        layer?.backgroundColor = (theme.isDark ? bg.highlight(withLevel: 0.08) : bg.shadow(withLevel: 0.04))?.cgColor
        layer?.borderColor = fg.withAlphaComponent(0.18).cgColor
        label.textColor = fg
        // ANSI green, like a finished agent.
        let green = theme.palette.count > 2 ? colorFromRGB(theme.palette[2]) : NSColor.systemGreen
        dot.layer?.backgroundColor = green.cgColor
    }

    override var fittingSize: NSSize {
        let l = label.fittingSize
        return NSSize(width: ceil(l.width) + 32, height: 22)
    }

    override func layout() {
        super.layout()
        let l = label.fittingSize
        dot.frame = NSRect(x: 11, y: (bounds.height - 6) / 2, width: 6, height: 6)
        label.frame = NSRect(x: 22, y: (bounds.height - l.height) / 2, width: ceil(l.width), height: l.height)
    }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }

    override func mouseDown(with event: NSEvent) {
        Updater.shared.checkForUpdates(self)
    }
}
