import AppKit

/// A pill in the bottom right of every window (`NoticeBadge`).
struct Notice: Equatable {
    enum Kind: Equatable {
        /// An app update is waiting (`Updater`).
        case update
        /// The local daemon still runs an older build (`Notices.localDaemonBuild`).
        case localDaemon
        /// These remote hosts run an older Thurm than this app.
        case remotes([HostId])
    }

    var kind: Kind
    var text: String
    var tooltip: String
    /// What dismissing it hides (see `Notices.dismiss`); empty when it can't be dismissed.
    var keys: [String] = []

    var dismissible: Bool { !keys.isEmpty }
}

/// The notices every window shows. Daemons are normally replaced in place without asking (the
/// local one in `SessionManager.connectAndHello`, a remote one from Settings › Remotes); these
/// say when one still runs an older build, so features that need it don't work, and a click
/// offers the upgrade.
final class Notices {
    static let shared = Notices()

    /// The local daemon's build while it is older than this app's (its in-place upgrade did not
    /// happen), else nil.
    var localDaemonBuild: String? {
        didSet { if localDaemonBuild != oldValue { refresh() } }
    }

    /// Dismissed for this launch, by what they were about: a later build there shows again.
    private var dismissed: Set<String> = []
    private(set) var current: [Notice] = []

    func refresh() {
        let remotes = Remotes.shared.statuses.values.sorted { $0.name < $1.name }
        let next = Self.notices(update: Updater.shared.badgeText, localDaemon: localDaemonBuild,
                                remotes: remotes, ours: Core.buildId, dismissed: dismissed)
        guard next != current else { return }
        current = next
        for c in SessionManager.shared.liveControllers {
            c.content.setNotices(next)
        }
    }

    func dismiss(_ notice: Notice) {
        dismissed.formUnion(notice.keys)
        refresh()
    }

    func activate(_ notice: Notice, from view: NSView) {
        switch notice.kind {
        case .update:
            Updater.shared.checkForUpdates(view)
        case .localDaemon:
            SessionManager.shared.offerDaemonRestart()
        case .remotes(let hosts) where hosts.count == 1:
            SessionManager.shared.installOnRemote(hosts[0])
        case .remotes(let hosts):
            let menu = NSMenu()
            for host in hosts {
                let item = NSMenuItem(title: "Update Thurm on \(host)…", action: #selector(updateHost(_:)),
                                      keyEquivalent: "")
                item.target = self
                item.representedObject = host
                menu.addItem(item)
            }
            menu.popUp(positioning: nil, at: NSPoint(x: 0, y: view.bounds.height + 4), in: view)
        }
    }

    @objc private func updateHost(_ sender: NSMenuItem) {
        guard let host = sender.representedObject as? HostId else { return }
        SessionManager.shared.installOnRemote(host)
    }

    static func notices(update: String?, localDaemon: String?, remotes: [RemoteStatus], ours: String,
                        dismissed: Set<String>) -> [Notice] {
        var out: [Notice] = []
        if let update {
            out.append(Notice(kind: .update, text: update,
                              tooltip: "\(update). Click to see what's new and install."))
        }
        if let build = localDaemon, !dismissed.contains("local:\(build)") {
            out.append(Notice(kind: .localDaemon, text: "Restart the session daemon to use new features",
                              tooltip: "It still runs Thurm \(build), this app \(ours).",
                              keys: ["local:\(build)"]))
        }
        let stale = remotes.filter { s in
            guard s.upgradeAvailable || s.phase == "upgrade_needed" else { return false }
            // A host ahead of this Mac is not downgraded; this app's own update catches up.
            if let b = s.remoteBuild, isNewer(b, than: ours) { return false }
            return !dismissed.contains(remoteKey(s))
        }
        if let first = stale.first {
            let one = stale.count == 1
            out.append(Notice(
                kind: .remotes(stale.map(\.name)),
                text: one ? "Update Thurm on \(first.name)" : "Update Thurm on \(stale.count) hosts",
                tooltip: one
                    ? "\(first.name) runs \(first.remoteBuild.map { "Thurm \($0)" } ?? "an older Thurm"), this Mac \(ours)."
                    : "\(stale.map(\.name).joined(separator: ", ")) run an older Thurm than this Mac (\(ours)).",
                keys: stale.map(remoteKey)))
        }
        return out
    }

    private static func remoteKey(_ s: RemoteStatus) -> String {
        "remote:\(s.name):\(s.remoteBuild ?? s.phase)"
    }

    /// Whether build `a` is known to be newer than `b`. Builds look like `0.12.1+830` (a
    /// release), `0.12.1-tip.3fa1c2e+830` (tip), `0.12.1+830.3fa1c2e` (built locally),
    /// `0.12.1+nix.3fa1c2e` or `0.12.1`. The commit count after `+` orders them when both have
    /// one, else the version does.
    static func isNewer(_ a: String, than b: String) -> Bool {
        func parse(_ s: String) -> (version: [Int], count: Int?) {
            let parts = s.split(separator: "+", maxSplits: 1)
            let core = (parts.first ?? "").prefix { $0 != "-" }
            let version = core.split(separator: ".").map { Int($0) ?? 0 }
            let count = parts.count > 1 ? parts[1].split(separator: ".").first.flatMap { Int($0) } : nil
            return (version, count)
        }
        let (va, ca) = parse(a)
        let (vb, cb) = parse(b)
        if let ca, let cb { return ca > cb }
        for i in 0..<max(va.count, vb.count) {
            let x = i < va.count ? va[i] : 0
            let y = i < vb.count ? vb[i] : 0
            if x != y { return x > y }
        }
        return false
    }
}

/// A notice's pill (see `Notices`): a click acts on it, the cross dismisses it. Colors follow
/// the terminal theme.
final class NoticeBadge: NSView {
    let notice: Notice
    private let dot = NSView()
    private let label = NSTextField(labelWithString: "")
    private var close: NSButton?

    init(_ notice: Notice) {
        self.notice = notice
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 11
        layer?.borderWidth = ChromeStroke.width
        dot.wantsLayer = true
        dot.layer?.cornerRadius = 3
        label.font = .systemFont(ofSize: 11, weight: .medium)
        label.stringValue = notice.text
        toolTip = notice.tooltip
        addSubview(dot)
        addSubview(label)
        if notice.dismissible {
            let image = NSImage(systemSymbolName: "xmark", accessibilityDescription: "Dismiss")?
                .withSymbolConfiguration(.init(pointSize: 8, weight: .semibold))
            let button = NSButton(image: image ?? NSImage(), target: self, action: #selector(dismiss))
            button.isBordered = false
            button.toolTip = "Dismiss"
            addSubview(button)
            close = button
        }
        applyTheme()
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    func applyTheme() {
        let chrome = SessionManager.shared.config.chrome
        layer?.backgroundColor = chrome.surfaceRaised.cgColor
        layer?.borderColor = chrome.strokeStrong.cgColor
        label.textColor = chrome.contentPrimary
        dot.layer?.backgroundColor = (notice.kind == .update ? chrome.success : chrome.warning).cgColor
        close?.contentTintColor = chrome.contentSecondary
    }

    override var fittingSize: NSSize {
        let l = label.fittingSize
        return NSSize(width: ceil(l.width) + 32 + (close == nil ? 0 : 16), height: 22)
    }

    override func layout() {
        super.layout()
        let l = label.fittingSize
        dot.frame = NSRect(x: 11, y: (bounds.height - 6) / 2, width: 6, height: 6)
        label.frame = NSRect(x: 22, y: (bounds.height - l.height) / 2, width: ceil(l.width), height: l.height)
        close?.frame = NSRect(x: label.frame.maxX + 4, y: (bounds.height - 16) / 2, width: 16, height: 16)
    }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }

    override func mouseDown(with event: NSEvent) {
        Notices.shared.activate(notice, from: self)
    }

    @objc private func dismiss() {
        Notices.shared.dismiss(notice)
    }
}
