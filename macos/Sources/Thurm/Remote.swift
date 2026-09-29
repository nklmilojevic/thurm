import AppKit
import Network
import CThurm

// Remote workspaces (specs/remote-workspaces.md): `thurmd` on other machines, reached through
// a supervised `ssh -L` tunnel per `[[remote]]` host (Rust: crates/thurm-remote). Each host
// connects as its own client; its panes live in remote workspaces next to local ones. While a
// host is unreachable its tabs keep their last frame under a "reconnecting" overlay.

/// A host's tunnel state, as `thurm_remotes_start` reports it.
struct RemoteStatus {
    var name: String
    var host: String
    /// connecting, connected, reconnecting, needs_attention, not_installed, upgrade_needed,
    /// disabled, stopped.
    var phase: String
    var message: String?
    var socket: String
    var retryAt: Date?
    var remoteBuild: String?
    var upgradeAvailable: Bool
    var os: String?
    var arch: String?
    var linger: String?

    init?(json: Any?) {
        guard let d = json as? [String: Any], let name = jsonString(d["name"]) else { return nil }
        self.name = name
        host = jsonString(d["host"]) ?? name
        phase = jsonString(d["phase"]) ?? "connecting"
        message = jsonString(d["message"])
        socket = jsonString(d["socket"]) ?? ""
        retryAt = jsonDouble(d["retry_at"]).map { Date(timeIntervalSince1970: $0) }
        remoteBuild = jsonString(d["remote_build"])
        upgradeAvailable = jsonBool(d["upgrade_available"]) ?? false
        os = jsonString(d["os"])
        arch = jsonString(d["arch"])
        linger = jsonString(d["linger"])
    }

    var label: String {
        switch phase {
        case "needs_attention": return "needs attention"
        case "not_installed": return "not installed"
        case "upgrade_needed": return "upgrade needed"
        default: return phase
        }
    }
}

/// A handoff (`thurm handoff`), as the registry keeps it.
struct HandoffInfo {
    var id: String
    var host: String
    /// The local repository.
    var repo: String
    var branch: String
    var worktree: String
    var pane: UInt64?
    var fetched: String?
    var fetchError: String?
    var pendingCleanup: Bool

    init?(json: Any?) {
        guard let d = json as? [String: Any], let id = jsonString(d["id"]),
              let host = jsonString(d["host"]), let repo = jsonString(d["repo"]) else { return nil }
        self.id = id
        self.host = host
        self.repo = repo
        branch = jsonString(d["branch"]) ?? ""
        worktree = jsonString(d["worktree"]) ?? ""
        pane = jsonUInt64(d["pane"])
        fetched = jsonString(d["fetched"])
        fetchError = jsonString(d["fetch_error"])
        pendingCleanup = jsonBool(d["pending_cleanup"]) ?? false
    }
}

private let remoteStatusCallback: thurm_remote_status_cb = { _, json in
    guard let json else { return }
    let text = String(cString: json)
    DispatchQueue.main.async { Remotes.shared.statusChanged(JSON.decode(text)) }
}

/// Every remote host's state in the app. Main thread only.
final class Remotes {
    static let shared = Remotes()

    private(set) var statuses: [HostId: RemoteStatus] = [:]
    /// Agent presets of each connected host (its own config decides what runs there).
    var presets: [HostId: [AgentPreset]] = [:]
    /// Panes closed while their host was offline: closed there once it is back. Kept across
    /// launches, or a pane closed before quitting would come back as an unknown tab.
    var pendingCloses: [HostId: Set<UInt64>] = Remotes.loadPendingCloses() {
        didSet {
            let plist = pendingCloses.filter { !$0.value.isEmpty }.mapValues { $0.map(String.init).sorted() }
            UserDefaults.standard.set(plist, forKey: Remotes.pendingClosesKey)
        }
    }
    private static let pendingClosesKey = "PendingRemotePaneCloses"

    private static func loadPendingCloses() -> [HostId: Set<UInt64>] {
        let saved = UserDefaults.standard.dictionary(forKey: pendingClosesKey) as? [String: [String]] ?? [:]
        return saved.mapValues { Set($0.compactMap { UInt64($0) }) }
    }
    private(set) var handoffs: [String: HandoffInfo] = [:]
    /// The last fetch of a handoff failed (shown on its tab until the next one succeeds).
    var handoffErrors: [String: String] = [:]

    private var countdown: Timer?
    private var pathMonitor: NWPathMonitor?
    private var started = false

    private init() {}

    func start() {
        guard !started else { return }
        started = true
        reloadHandoffs()
        thurm_remotes_start(remoteStatusCallback, nil)
        // After sleep or on another network, a tunnel may look alive and be dead: check now
        // rather than after ssh's keepalive gives up.
        NSWorkspace.shared.notificationCenter.addObserver(forName: NSWorkspace.didWakeNotification,
                                                          object: nil, queue: .main) { _ in
            thurm_remote_kick(nil, true)
        }
        let monitor = NWPathMonitor()
        var first = true
        monitor.pathUpdateHandler = { _ in
            DispatchQueue.main.async {
                if first { first = false; return }
                thurm_remote_kick(nil, true)
            }
        }
        monitor.start(queue: DispatchQueue.global(qos: .utility))
        pathMonitor = monitor
        countdown = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { _ in
            Remotes.shared.tick()
        }
    }

    func stop() {
        pathMonitor?.cancel()
        countdown?.invalidate()
        thurm_remotes_stop()
    }

    func configChanged() {
        guard started else { return }
        thurm_remotes_sync()
        let names = Set(SessionManager.shared.config.remoteNames)
        for host in statuses.keys where !names.contains(host) {
            statuses.removeValue(forKey: host)
            Core.shared.disconnect(host)
        }
        RemotesWindow.shared.reload()
    }

    func phaseLabel(_ host: HostId) -> String {
        if Core.shared.isConnected(host) { return "connected" }
        return statuses[host]?.label ?? "connecting"
    }

    // MARK: Tunnel state

    fileprivate func statusChanged(_ json: Any?) {
        guard let s = RemoteStatus(json: json) else { return }
        let before = statuses[s.name]
        statuses[s.name] = s
        let host = s.name
        if s.phase == "connected" {
            if !Core.shared.isConnected(host) {
                if let err = Core.shared.connectRemote(host, socket: s.socket) {
                    tlog("connecting to \(host) through its tunnel: \(err)")
                    thurm_remote_kick(host, true)
                } else {
                    SessionManager.shared.remoteConnected(host)
                }
            }
        } else if Core.shared.isConnected(host) {
            connectionLost(host)
        }
        if before?.phase != s.phase, ["needs_attention", "upgrade_needed", "not_installed"].contains(s.phase) {
            tlog("\(host): \(s.label): \(s.message ?? "")")
        }
        updateOverlays(host)
        RemotesWindow.shared.reload()
        SessionManager.shared.refreshSidebars()
    }

    /// `host`'s connection dropped (the daemon went away or the tunnel did).
    func connectionLost(_ host: HostId) {
        Core.shared.disconnect(host)
        updateOverlays(host)
        SessionManager.shared.refreshSidebars()
        // The tunnel may still look alive; have it checked and replaced now.
        thurm_remote_kick(host, true)
    }

    private func tick() {
        for host in statuses.keys where !Core.shared.isConnected(host) {
            updateOverlays(host)
        }
    }

    private func updateOverlays(_ host: HostId) {
        let message = offlineMessage(host)
        for view in SessionManager.shared.views(of: host) {
            view.setOffline(message)
        }
    }

    /// What a disconnected host's panes show; nil while connected.
    func offlineMessage(_ host: HostId) -> String? {
        if Core.shared.isConnected(host) { return nil }
        guard let s = statuses[host] else { return "Connecting to \(host)…" }
        let detail = s.message.map { "\n" + ($0.split(separator: "\n").first.map(String.init) ?? $0) } ?? ""
        switch s.phase {
        case "connecting":
            return "Connecting to \(host)…"
        case "needs_attention":
            return "\(host) needs attention\(s.message.map { "\n" + $0 } ?? "")\nThurm › Remotes… to retry"
        case "not_installed":
            return "Thurm is not installed on \(host).\nThurm › Remotes… to install it."
        case "upgrade_needed":
            return "\(host) runs another version of Thurm\(detail)\nThurm › Remotes… to upgrade it."
        case "disabled":
            return "\(host) is disabled (enabled = false in its [[remote]])."
        default:
            var line = "Disconnected — reconnecting"
            if let at = s.retryAt {
                let secs = Int(at.timeIntervalSinceNow.rounded(.up))
                line += secs > 0 ? " in \(secs) s…" : "…"
            } else {
                line += "…"
            }
            return line + detail
        }
    }

    // MARK: Policy (clipboard)

    /// OSC 52 read: this Mac's panes as before; a remote pane asks (per `clipboard_read`).
    func clipboardRequest(_ key: PaneKey) {
        let reply = { (text: String) in
            Core.shared.send(object: ["ClipboardReply": ["pane": key.number, "text": text]], host: key.host)
        }
        let clipboard = { NSPasteboard.general.string(forType: .string) ?? "" }
        guard key.isRemote else {
            reply(clipboard())
            return
        }
        let decision = Core.shared.remoteCall(["op": "clipboard_read", "host": key.host]) as? String ?? "deny"
        switch decision {
        case "allow":
            reply(clipboard())
        case "ask":
            let alert = NSAlert()
            alert.messageText = "\(key.host) wants to read your clipboard"
            alert.informativeText = "A program in a pane on \(key.host) asked for the contents of this Mac's "
                + "clipboard (OSC 52)."
            alert.addButton(withTitle: "Allow Once")
            alert.addButton(withTitle: "Always for \(key.host)")
            alert.addButton(withTitle: "Deny")
            switch alert.runModal() {
            case .alertFirstButtonReturn:
                reply(clipboard())
            case .alertSecondButtonReturn:
                if let e = (Core.shared.remoteCall(["op": "allow_clipboard", "name": key.host]) as? [String: Any])?["error"] {
                    tlog("could not save the clipboard permission: \(e)")
                }
                reply(clipboard())
            default:
                reply("")
            }
        default:
            reply("")
        }
    }

    /// OSC 52 write from `host`: follows this Mac's `terminal.osc52`.
    func clipboardWriteAllowed(_ host: HostId) -> Bool {
        guard host != localHost else { return true }
        return Core.shared.remoteCall(["op": "clipboard_write", "host": host]) as? String == "allow"
    }

    // MARK: Handoffs

    func reloadHandoffs() {
        let list = Core.shared.remoteCall(["op": "handoff_list"]) as? [Any] ?? []
        handoffs = Dictionary(list.compactMap { HandoffInfo(json: $0) }.map { ($0.id, $0) }) { a, _ in a }
        for (id, h) in handoffs {
            if let e = h.fetchError { handoffErrors[id] = e } else { handoffErrors.removeValue(forKey: id) }
        }
    }

    /// The handoff running in `host`'s `pane` (re-read: `thurm handoff` records it).
    func handoff(host: HostId, pane: UInt64) -> HandoffInfo? {
        reloadHandoffs()
        return handoffs.values.first { $0.host == host && $0.pane == pane }
    }

    /// An agent reached Done or NeedsInput: a handoff's committed work comes back.
    func agentSettled(_ key: PaneKey) {
        guard key.isRemote else { return }
        let id = SessionManager.shared.controller(for: key)?.handoffID
            ?? handoffs.values.first { $0.host == key.host && $0.pane == key.id }?.id
        if let id { SessionManager.shared.fetchHandoff(id, quiet: true) }
    }
}

extension SessionManager {
    /// The daemon new tabs of the front window run on.
    var currentHost: HostId {
        guard let c = currentController, !c.isQuick else { return localHost }
        return workspace(c.workspaceID)?.host ?? c.host
    }

    /// Launch presets for `host`: this Mac's config, or the host's own.
    func presets(for host: HostId) -> [AgentPreset] {
        host == localHost ? config.agentPresets : Remotes.shared.presets[host] ?? []
    }

    func offlineMessage(for host: HostId) -> String? {
        Remotes.shared.offlineMessage(host)
    }

    // MARK: Connecting

    /// `host` connected (again): its panes attach, vanished ones close with a notice, unknown
    /// ones open as new tabs in its workspace, and the current agent state shows (no missed
    /// notifications are replayed).
    func remoteConnected(_ host: HostId) {
        sendAppearance(to: host)
        for id in Remotes.shared.pendingCloses.removeValue(forKey: host) ?? [] {
            Core.shared.send(object: ["ClosePane": ["pane": NSNumber(value: id)]], host: host)
        }
        guard let infos = fetchPanes(host: host) else {
            Remotes.shared.connectionLost(host)
            return
        }
        let alive = Set(infos.filter { $0.alive }.map { $0.key })
        for key in panes.keys where key.host == host && !alive.contains(key) {
            panes.removeValue(forKey: key)
        }
        // Tabs of panes the host no longer has close, with a notice.
        var gone = 0
        for c in liveControllers {
            for id in c.content.paneIds where id.host == host && !alive.contains(id) {
                removePaneFromUI(id)
                gone += 1
            }
        }
        for ws in workspaces where !isShown(ws) {
            let before = ws.hiddenTabs.flatMap { $0.root.panes }.count
            ws.hiddenTabs = retainTabs(ws.hiddenTabs) { $0.host != host || alive.contains($0) }
            gone += before - ws.hiddenTabs.flatMap { $0.root.panes }.count
            ws.hiddenSelectedTab = min(ws.hiddenSelectedTab, max(0, ws.hiddenTabs.count - 1))
        }
        for info in infos where info.alive { paneInfoUpdated(info) }
        for view in views(of: host) {
            view.setOffline(nil)
            view.resubscribe()
        }
        // Panes the layout doesn't know about: new tabs in the host's workspace.
        var known = Set(liveControllers.flatMap { $0.content.paneIds })
        known.formUnion(workspaces.flatMap { $0.hiddenTabs.flatMap { $0.root.panes } })
        let unknown = infos.filter { $0.alive && !known.contains($0.key) }.map(\.key).sorted { $0.id < $1.id }
        if !unknown.isEmpty { adoptRemotePanes(unknown, host: host) }

        if let resp = JSON.variant(Core.shared.request("\"ListAgentPresets\"", host: host)),
           resp.name == "AgentPresets", let list = resp.payload as? [[String: Any]] {
            Remotes.shared.presets[host] = list.compactMap { p in
                jsonString(p["name"]).map { AgentPreset(name: $0, command: (p["command"] as? [String]) ?? []) }
            }
        }
        if gone > 0 {
            currentController?.content.focusedView?.showToast(
                "\(gone) \(gone == 1 ? "pane" : "panes") on \(host) ended while it was away", duration: 5)
        }
        runPendingHandoffCleanups(host)
        refreshSidebars()
        updateDockBadge()
        for c in liveControllers { c.updateTitle() }
        scheduleLayoutSave()
    }

    /// Opens `keys` (all on `host`) as tabs of the host's workspace, shown or not.
    private func adoptRemotePanes(_ keys: [PaneKey], host: HostId) {
        let ws = workspaces.first { $0.host == host && isShown($0) }
            ?? workspaces.first { $0.host == host }
            ?? makeWorkspace(host: host)
        let handoffs = Remotes.shared.handoffs.values
        if let shownIn = controllerShowing(ws) {
            for key in keys {
                let c = makeController(root: .leaf(key), focused: key, zoomed: nil, title: nil, frame: nil)
                c.handoffID = handoffs.first { $0.host == host && $0.pane == key.id }?.id
                attachAsTab(c, to: shownIn)
            }
        } else {
            for key in keys {
                ws.hiddenTabs.append(TabLayout(title: nil, root: .pane(key), focused: key.id, zoomed: nil,
                                               handoff: handoffs.first { $0.host == host && $0.pane == key.id }?.id))
            }
        }
    }

    // MARK: Links

    /// Cmd-click on a link: this Mac's panes open it; a remote pane's http(s) links open, other
    /// schemes ask, and file:// paths (the host's) are offered for copying, never opened here.
    func openLink(_ url: URL, from pane: PaneKey, in window: NSWindow?) {
        guard pane.isRemote else {
            NSWorkspace.shared.open(url)
            return
        }
        let v = Core.shared.remoteCall(["op": "link", "host": pane.host, "url": url.absoluteString]) as? [String: Any]
        switch jsonString(v?["action"]) {
        case "open":
            NSWorkspace.shared.open(url)
        case "ask":
            let alert = NSAlert()
            alert.messageText = "Open this link from \(pane.host)?"
            alert.informativeText = url.absoluteString
            alert.addButton(withTitle: "Open")
            alert.addButton(withTitle: "Cancel")
            if alert.runModal() == .alertFirstButtonReturn { NSWorkspace.shared.open(url) }
        case "copy_path":
            let path = jsonString(v?["path"]) ?? url.path
            let alert = NSAlert()
            alert.messageText = "This is a file on \(pane.host)"
            alert.informativeText = "\(path)\n\nFiles on a remote host are not opened on this Mac."
            alert.addButton(withTitle: "Copy Path")
            alert.addButton(withTitle: "Cancel")
            if alert.runModal() == .alertFirstButtonReturn {
                let pb = NSPasteboard.general
                pb.clearContents()
                pb.setString(path, forType: .string)
            }
        default:
            break
        }
    }

    // MARK: Command palette

    func remotePaletteItems() -> [CommandPalette.Item] {
        var items: [CommandPalette.Item] = []
        let c = currentController
        if let c, !c.isQuick, c.host == localHost, let root = panes[c.focusedPane]?.git?.root {
            for host in Core.shared.connectedRemotes {
                items.append(CommandPalette.Item(title: "Hand Off to \(host)…",
                                                 detail: (root as NSString).lastPathComponent) {
                    SessionManager.shared.showHandoffPicker(host: host, repo: root)
                })
            }
        }
        if let id = c?.handoffID {
            let branch = Remotes.shared.handoffs[id]?.branch ?? ""
            items.append(CommandPalette.Item(title: "Fetch Handoff Result", detail: branch) {
                SessionManager.shared.fetchHandoff(id, quiet: false)
            })
        }
        for host in config.remoteNames where !Core.shared.isConnected(host) {
            items.append(CommandPalette.Item(title: "Reconnect to \(host)", detail: Remotes.shared.phaseLabel(host)) {
                thurm_remote_kick(host, true)
            })
        }
        if !config.remoteNames.isEmpty {
            items.append(CommandPalette.Item(title: "Remotes…", detail: "connection state, install, upgrade") {
                RemotesWindow.shared.show()
            })
        }
        return items
    }

    /// Picks what runs in the handoff tab: one of the host's presets, or its shell.
    func showHandoffPicker(host: HostId, repo: String) {
        var items = presets(for: host).map { p in
            CommandPalette.Item(title: p.name, detail: p.command.joined(separator: " ")) {
                SessionManager.shared.startHandoff(host: host, repo: repo, preset: p.name)
            }
        }
        items.append(CommandPalette.Item(title: "Shell", detail: "type the command yourself") {
            SessionManager.shared.startHandoff(host: host, repo: repo, preset: nil)
        })
        CommandPalette.shared.show(items: items, over: currentController?.window,
                                   placeholder: "Hand off \((repo as NSString).lastPathComponent) to \(host) with…",
                                   footer: "↩ hand off")
    }

    // MARK: Handoffs

    /// Pushes a snapshot of `repo` to `host`, makes its worktree and opens a tab there running
    /// `preset` (no task is typed for the agent).
    func startHandoff(host: HostId, repo: String, preset: String?) {
        currentController?.content.focusedView?.showToast("Handing off to \(host)…", duration: 20)
        DispatchQueue.global(qos: .userInitiated).async {
            let v = Core.shared.remoteCall(["op": "handoff_prepare", "host": host, "path": repo, "branch": NSNull()])
            DispatchQueue.main.async {
                let sm = SessionManager.shared
                guard let h = HandoffInfo(json: v) else {
                    sm.remoteError("Could not hand off to \(host)", v)
                    return
                }
                Remotes.shared.reloadHandoffs()
                guard let c = sm.newTab(from: sm.currentController, preset: preset, cwd: h.worktree, host: host)
                else { return }
                c.handoffID = h.id
                let pane = c.focusedPane
                DispatchQueue.global(qos: .utility).async {
                    _ = Core.shared.remoteCall(["op": "handoff_set_pane", "id": h.id, "pane": pane.number])
                    DispatchQueue.main.async { Remotes.shared.reloadHandoffs() }
                }
                c.content.focusedView?.showToast("\(h.branch) on \(host): results come back as thurm-\(host)/\(h.branch)",
                                                 duration: 5)
                sm.scheduleLayoutSave()
            }
        }
    }

    /// `git fetch thurm-<host> agent/<slug>`; failures show on the tab and retry next time.
    func fetchHandoff(_ id: String, quiet: Bool) {
        let before = Remotes.shared.handoffs[id]?.fetched
        DispatchQueue.global(qos: .utility).async {
            let v = Core.shared.remoteCall(["op": "handoff_fetch", "id": id])
            DispatchQueue.main.async {
                let sm = SessionManager.shared
                let view = sm.liveControllers.first { $0.handoffID == id }?.content.focusedView
                if let e = (v as? [String: Any])?["error"] as? String {
                    Remotes.shared.handoffErrors[id] = e
                    view?.showToast("Fetching the handoff failed: \(e.split(separator: "\n").first ?? "")", duration: 5)
                } else if let h = HandoffInfo(json: v) {
                    Remotes.shared.handoffErrors.removeValue(forKey: id)
                    if !quiet || (h.fetched != nil && h.fetched != before) {
                        view?.showToast("Fetched \(h.branch) → thurm-\(h.host)/\(h.branch)")
                    }
                }
                Remotes.shared.reloadHandoffs()
                sm.refreshSidebars()
            }
        }
    }

    /// A handoff tab closed: offer to remove its worktree on the host.
    enum HandoffClose { case cancel, removeWorktree, keepWorktree }

    /// Asked before a handoff's tab closes: closing stops the agent, so the only way to keep
    /// it running is not to close.
    func askHandoffTabClose(_ id: String) -> HandoffClose {
        guard let h = Remotes.shared.handoffs[id] else { return .keepWorktree }
        let alert = NSAlert()
        alert.messageText = "Close the handoff on \(h.host)?"
        alert.informativeText = "Closing the tab stops the agent. You can also remove its worktree, "
            + "\(h.branch) in \(h.worktree): committed work is fetched first, and the branch stays on "
            + "\(h.host) unless your default branch contains it."
        alert.addButton(withTitle: "Close and Remove Worktree")
        alert.addButton(withTitle: "Close, Keep Worktree")
        alert.addButton(withTitle: "Cancel")
        switch alert.runModal() {
        case .alertFirstButtonReturn: return .removeWorktree
        case .alertSecondButtonReturn: return .keepWorktree
        default: return .cancel
        }
    }

    /// The handoff's tab closed and its worktree is to go.
    func removeHandoffWorktree(_ id: String) {
        guard let h = Remotes.shared.handoffs[id] else { return }
        guard Core.shared.isConnected(h.host) else {
            _ = Core.shared.remoteCall(["op": "handoff_defer", "id": id])
            currentController?.content.focusedView?.showToast(
                "\(h.host) is offline: the worktree is removed once it is back", duration: 5)
            Remotes.shared.reloadHandoffs()
            return
        }
        cleanUpHandoff(id, confirmLoss: true)
    }

    /// Final fetch, then removal; with `confirmLoss`, uncommitted or unfetched work asks first
    /// (otherwise it is kept and reported).
    private func cleanUpHandoff(_ id: String, confirmLoss: Bool) {
        DispatchQueue.global(qos: .userInitiated).async {
            let first = Core.shared.remoteCall(["op": "handoff_cleanup", "id": id, "force": false])
            DispatchQueue.main.async {
                let sm = SessionManager.shared
                if let e = (first as? [String: Any])?["error"] as? String {
                    guard confirmLoss else {
                        sm.currentController?.content.focusedView?.showToast("Handoff kept: \(e)", duration: 6)
                        return
                    }
                    let alert = NSAlert()
                    alert.alertStyle = .warning
                    alert.messageText = "Remove it anyway?"
                    alert.informativeText = e
                    alert.addButton(withTitle: "Remove")
                    alert.addButton(withTitle: "Keep")
                    guard alert.runModal() == .alertFirstButtonReturn else { return }
                    DispatchQueue.global(qos: .userInitiated).async {
                        let forced = Core.shared.remoteCall(["op": "handoff_cleanup", "id": id, "force": true])
                        DispatchQueue.main.async { sm.handoffCleanedUp(forced) }
                    }
                    return
                }
                sm.handoffCleanedUp(first)
            }
        }
    }

    private func handoffCleanedUp(_ result: Any?) {
        Remotes.shared.reloadHandoffs()
        if let d = result as? [String: Any], let message = jsonString(d["message"]) {
            currentController?.content.focusedView?.showToast(message, duration: 5)
        } else {
            remoteError("Could not remove the handoff", result)
        }
    }

    /// Handoff tabs closed while their host was offline.
    private func runPendingHandoffCleanups(_ host: HostId) {
        Remotes.shared.reloadHandoffs()
        for h in Remotes.shared.handoffs.values where h.host == host && h.pendingCleanup {
            cleanUpHandoff(h.id, confirmLoss: false)
        }
    }

    func remoteError(_ title: String, _ response: Any?) {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = ((response as? [String: Any])?["error"] as? String) ?? "Unexpected answer."
        alert.alertStyle = .warning
        alert.runModal()
    }

    // MARK: Install and upgrade

    /// Installs this build on `host` and upgrades its daemon in place, asking first.
    func installOnRemote(_ host: HostId) {
        let bins: Any = Core.helpersPath ?? NSNull()
        DispatchQueue.global(qos: .userInitiated).async {
            let plan = Core.shared.remoteCall(["op": "plan", "name": host, "bins": bins])
            DispatchQueue.main.async { SessionManager.shared.installWithPlan(host, plan: plan) }
        }
    }

    private func installWithPlan(_ host: HostId, plan: Any?) {
        guard let p = plan as? [String: Any], p["error"] == nil else {
            remoteError("Could not reach \(host)", plan)
            return
        }
        let installed = jsonBool(p["installed"]) ?? false
        let methods = p["methods"] as? [String] ?? []
        let labels = p["labels"] as? [String] ?? methods
        let daemon = p["daemon"] as? [String: Any]
        let hostInfo = p["host"] as? [String: Any]
        let platform = [jsonString(hostInfo?["os"]), jsonString(hostInfo?["arch"])].compactMap { $0 }.joined(separator: " ")

        let upgradeDaemon = {
            guard let d = daemon, jsonBool(d["running"]) == true,
                  jsonString(d["build"]) != Core.buildId else {
                thurm_remote_kick(host, true)
                return
            }
            let hot = jsonBool(d["hot_upgrade"]) ?? false
            if !hot {
                let alert = NSAlert()
                alert.alertStyle = .warning
                alert.messageText = "Restart the daemon on \(host)?"
                alert.informativeText = "It is too old to be replaced in place: restarting it stops the programs "
                    + "running in its panes (layout and scrollback come back)."
                alert.addButton(withTitle: "Restart")
                alert.addButton(withTitle: "Cancel")
                guard alert.runModal() == .alertFirstButtonReturn else { return }
            }
            DispatchQueue.global(qos: .userInitiated).async {
                let r = Core.shared.remoteCall(["op": "upgrade_daemon", "name": host, "allow_restart": !hot])
                DispatchQueue.main.async {
                    if (r as? [String: Any])?["error"] != nil {
                        SessionManager.shared.remoteError("Could not upgrade the daemon on \(host)", r)
                    }
                    thurm_remote_kick(host, true)
                    RemotesWindow.shared.reload()
                }
            }
        }
        if installed {
            upgradeDaemon()
            return
        }
        guard !methods.isEmpty else {
            let alert = NSAlert()
            alert.messageText = "Thurm cannot be installed on \(host)"
            alert.informativeText = jsonString(p["problem"]) ?? "No build fits \(platform)."
            alert.runModal()
            return
        }
        let alert = NSAlert()
        alert.messageText = "Install Thurm \(Core.buildId) on \(host)?"
        alert.informativeText = "\(platform). Thurm goes to ~/.local/share/thurm/bin there (and ~/.local/bin/thurm "
            + "when that directory exists)."
        for label in labels.prefix(3) { alert.addButton(withTitle: label) }
        alert.addButton(withTitle: "Cancel")
        let choice = alert.runModal().rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
        guard choice >= 0, choice < min(3, methods.count) else { return }
        let method = methods[choice]
        let bins: Any = Core.helpersPath ?? NSNull()
        RemotesWindow.shared.setBusy(host, "Installing…")
        DispatchQueue.global(qos: .userInitiated).async {
            let r = Core.shared.remoteCall(["op": "install", "name": host, "method": method, "bins": bins])
            DispatchQueue.main.async {
                RemotesWindow.shared.setBusy(host, nil)
                if (r as? [String: Any])?["error"] != nil {
                    SessionManager.shared.remoteError("Could not install Thurm on \(host)", r)
                    return
                }
                upgradeDaemon()
            }
        }
    }
}

// MARK: - Remotes window

/// Thurm › Remotes…: every `[[remote]]` host with its state and build (read-only; hosts are
/// added with `thurm remote add` or in the config), with Retry and Install/Upgrade.
final class RemotesWindow: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSWindowDelegate {
    static let shared = RemotesWindow()

    private var panel: NSPanel?
    private let table = NSTableView()
    private let detail = NSTextField(wrappingLabelWithString: "")
    private let retry = NSButton(title: "Retry", target: nil, action: nil)
    private let install = NSButton(title: "Install / Upgrade…", target: nil, action: nil)
    private var names: [String] = []
    private var busy: [HostId: String] = [:]

    func show() {
        if panel == nil { build() }
        reload(force: true)
        panel?.makeKeyAndOrderFront(nil)
    }

    func setBusy(_ host: HostId, _ text: String?) {
        busy[host] = text
        reload()
    }

    private func build() {
        let p = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 760, height: 320),
                        styleMask: [.titled, .closable, .resizable, .utilityWindow], backing: .buffered, defer: false)
        p.title = "Remotes"
        p.isFloatingPanel = false
        p.hidesOnDeactivate = false
        p.delegate = self
        p.center()
        for (id, title, width) in [("name", "Name", 110.0), ("host", "Host", 170.0), ("state", "State", 130.0),
                                   ("build", "Build", 300.0)] {
            let col = NSTableColumn(identifier: .init(id))
            col.title = title
            col.width = width
            table.addTableColumn(col)
        }
        table.usesAlternatingRowBackgroundColors = true
        table.dataSource = self
        table.delegate = self
        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        detail.font = .systemFont(ofSize: 11)
        detail.textColor = .secondaryLabelColor
        detail.maximumNumberOfLines = 6
        retry.target = self
        retry.action = #selector(retryClicked(_:))
        install.target = self
        install.action = #selector(installClicked(_:))
        let note = NSTextField(labelWithString: "Add hosts with `thurm remote add NAME SSH-TARGET` or [[remote]] in the config.")
        note.font = .systemFont(ofSize: 11)
        note.textColor = .tertiaryLabelColor
        let root = NSView()
        for v in [scroll, detail, retry, install, note] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            root.addSubview(v)
        }
        NSLayoutConstraint.activate([
            scroll.topAnchor.constraint(equalTo: root.topAnchor),
            scroll.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: detail.topAnchor, constant: -8),
            detail.leadingAnchor.constraint(equalTo: root.leadingAnchor, constant: 12),
            detail.trailingAnchor.constraint(equalTo: root.trailingAnchor, constant: -12),
            detail.bottomAnchor.constraint(equalTo: retry.topAnchor, constant: -8),
            note.leadingAnchor.constraint(equalTo: root.leadingAnchor, constant: 12),
            note.centerYAnchor.constraint(equalTo: retry.centerYAnchor),
            install.trailingAnchor.constraint(equalTo: root.trailingAnchor, constant: -12),
            install.bottomAnchor.constraint(equalTo: root.bottomAnchor, constant: -10),
            retry.trailingAnchor.constraint(equalTo: install.leadingAnchor, constant: -8),
            retry.centerYAnchor.constraint(equalTo: install.centerYAnchor),
        ])
        p.contentView = root
        panel = p
    }

    /// Refreshes the list while it is on screen (or now, with `force`).
    func reload(force: Bool = false) {
        guard let panel, panel.isVisible || force else { return }
        names = SessionManager.shared.config.remoteNames
        let selected = table.selectedRow
        table.reloadData()
        if selected >= 0, selected < names.count {
            table.selectRowIndexes([selected], byExtendingSelection: false)
        } else if !names.isEmpty {
            table.selectRowIndexes([0], byExtendingSelection: false)
        }
        updateDetail()
    }

    private var selectedHost: HostId? {
        let r = table.selectedRow
        return r >= 0 && r < names.count ? names[r] : nil
    }

    private func updateDetail() {
        guard let host = selectedHost else {
            detail.stringValue = ""
            retry.isEnabled = false
            install.isEnabled = false
            return
        }
        let s = Remotes.shared.statuses[host]
        var lines: [String] = []
        if let b = busy[host] { lines.append(b) }
        if let m = s?.message { lines.append(m) }
        if let os = s?.os, let arch = s?.arch { lines.append("\(os) \(arch)") }
        if s?.linger == "no" {
            lines.append("Lingering is off: run `loginctl enable-linger $USER` on \(host) so its daemon survives logout.")
        }
        if s?.upgradeAvailable == true { lines.append("Runs another build than this Mac (\(Core.buildId)).") }
        detail.stringValue = lines.joined(separator: "\n")
        retry.isEnabled = busy[host] == nil
        install.isEnabled = busy[host] == nil
    }

    @objc private func retryClicked(_ sender: Any?) {
        guard let host = selectedHost else { return }
        thurm_remote_kick(host, true)
    }

    @objc private func installClicked(_ sender: Any?) {
        guard let host = selectedHost else { return }
        SessionManager.shared.installOnRemote(host)
    }

    func numberOfRows(in tableView: NSTableView) -> Int { names.count }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard row < names.count, let id = tableColumn?.identifier.rawValue else { return nil }
        let host = names[row]
        let s = Remotes.shared.statuses[host]
        let text: String
        switch id {
        case "name": text = host
        case "host": text = s?.host ?? ""
        case "state": text = busy[host] ?? Remotes.shared.phaseLabel(host)
        default: text = s?.remoteBuild ?? ""
        }
        let label = NSTextField(labelWithString: text)
        label.font = id == "build" ? .monospacedSystemFont(ofSize: 11, weight: .regular) : .systemFont(ofSize: 12)
        label.lineBreakMode = .byTruncatingTail
        if id == "state" {
            switch s?.phase {
            case "connected": label.textColor = .systemGreen
            case "needs_attention", "upgrade_needed", "not_installed": label.textColor = .systemOrange
            default: label.textColor = .secondaryLabelColor
            }
        }
        return label
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        updateDetail()
    }
}
