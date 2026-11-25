import AppKit
import CThurm

/// Owns the connection lifecycle, every window/tab/split, pane metadata and layout
/// persistence. All methods run on the main thread.
final class SessionManager: NSObject, CoreDelegate {
    static let shared = SessionManager()

    var config = AppConfig() {
        didSet { Motion.speed = config.animationSpeed }
    }
    /// Cmd+Plus / Cmd+Minus override of `config.fontSize` (not persisted).
    private(set) var fontSizeOverride: CGFloat?
    /// View > Tabs in Sidebar when config.toml cannot record it (read-only, e.g. managed by Nix
    /// or Home Manager): the choice holds for this session, over reloads.
    private var sidebarTabsOverride: Bool?
    private var activePaneBorderOverride: Bool?
    /// View > Group Tabs by Repository, Tabs from All Workspaces and Only Active Tabs, as
    /// `sidebarTabsOverride`.
    private var sidebarListingOverride: [WritableKeyPath<AppConfig, Bool>: Bool] = [:]
    /// `window.sidebar_host` chosen when config.toml cannot record it ("" for every host).
    private var sidebarHostOverride: String?
    private var sidebarUsageOverride: [UsageProvider: Bool] = [:]
    /// A theme chosen when config.toml cannot record it is in use (see `setTheme`).
    private var sessionTheme = false
    var effectiveFontSize: CGFloat { fontSizeOverride ?? config.fontSize }

    private(set) var controllers: [TerminalWindowController] = []
    /// Every workspace (see Workspaces.swift).
    var workspaces: [Workspace] = []
    var tabHistory = TabHistory()
    /// Every pane of every connected daemon (a disconnected host's keep their last state).
    var panes: [PaneKey: PaneInfo] = [:]
    /// Panes waiting for input that the user has looked at since they started waiting; they
    /// don't count toward the Dock badge until the agent asks again.
    /// Panes whose current agent state is on the Dock badge but the user has seen.
    private var seenAlerts: Set<PaneKey> = []
    private weak var lastKeyController: TerminalWindowController?
    private weak var lastRegularKeyController: TerminalWindowController?

    private(set) var isTerminating = false
    /// Monotonically incremented whenever theme/font/config changes are applied to the UI, so
    /// cached background `TabItem`s can refresh lazily when mounted.
    private(set) var configGeneration: UInt64 = 0
    /// Re-entrant guard suppressing native `NSWindowTabGroup` KVO storms during batch switches.
    private var workspaceSwitchDepth = 0
    var isSwitchingWorkspace: Bool { workspaceSwitchDepth > 0 }

    func withWorkspaceSwitchBatch<T>(_ body: () -> T) -> T {
        workspaceSwitchDepth += 1
        defer { workspaceSwitchDepth -= 1 }
        return body()
    }

    /// Drops `id` from the Rust core's warm pane LRU cache when a pane closes.
    func evictWarmPane(_ id: PaneKey) {
        Core.shared.evictPane(id)
    }

    /// Prewarms background tabs' visible panes into the Rust core's warm LRU cache (bounded per
    /// host by `Core.warmPaneLimit`) so switching tabs paints on frame 0 without a daemon round-trip.
    func prewarmWorkspaceTabs(_ tabs: [TabItem], excluding selected: TabItem?) {
        let budget = Int(Core.warmPaneLimit)
        guard budget > 0 else { return }
        var remaining: [HostId: Int] = [:]
        for tab in tabs where tab !== selected && !tab.isClosed {
            for pane in tab.content.visiblePaneIds {
                guard Core.shared.isConnected(pane.host) else { continue }
                let left = remaining[pane.host] ?? budget
                guard left > 0 else { continue }
                Core.shared.prewarm(pane)
                remaining[pane.host] = left - 1
            }
        }
    }

    /// Prewarms hidden workspaces' tabs (selected tab of each workspace first, ordered by recency)
    /// for `host` (or all connected hosts when nil) so even the first switch to a local or remote
    /// workspace paints immediately with zero RTT.
    func prewarmHiddenWorkspaces(for host: HostId? = nil) {
        let budget = Int(Core.warmPaneLimit)
        guard budget > 0 else { return }
        var remaining: [HostId: Int] = [:]
        func warm(_ pane: PaneKey) {
            guard Core.shared.isConnected(pane.host) else { return }
            let left = remaining[pane.host] ?? budget
            guard left > 0 else { return }
            Core.shared.prewarm(pane)
            remaining[pane.host] = left - 1
        }
        let targets = workspacesByRecency.filter { ws in
            !isShown(ws) && (host == nil || ws.host == host)
        }
        // Pass 1: each hidden workspace's selected tab first so switching to any workspace is instant.
        for ws in targets {
            let tabs = ws.hiddenTabs
            guard !tabs.isEmpty else { continue }
            let idx = min(max(0, ws.hiddenSelectedTab), tabs.count - 1)
            for pane in tabs[idx].panes { warm(pane) }
        }
        // Pass 2: remaining hidden tabs while budget allows.
        for ws in targets {
            let tabs = ws.hiddenTabs
            let selectedIdx = min(max(0, ws.hiddenSelectedTab), max(0, tabs.count - 1))
            for (i, tab) in tabs.enumerated() where i != selectedIdx {
                for pane in tab.panes { warm(pane) }
            }
        }
    }
    /// Layout saving starts only after the initial restore, so a failed start never
    /// overwrites the stored layout with an empty one.
    private var sessionReady = false
    /// Called after each successful restore, once the session is usable.
    var onSessionReady: (() -> Void)?
    /// Restores in a row whose ListPanes or GetLayout failed.
    private var restoreFailures = 0
    private var lastLayoutJSON: String?
    private var periodicTimer: Timer?
    private var reconnectAttempt = 0
    private var appearanceObservation: NSKeyValueObservation?
    /// The last Hello was refused because the daemon speaks another protocol version.
    private var helloRefusedForVersion = false

    private override init() {
        super.init()
    }

    // MARK: - Startup

    func start() {
        config = AppConfig.load()
        config.loadWarnings += applyKeys()
        if let err = config.loadError { tlog("config: \(err)") }
        Core.shared.delegate = self
        NotificationCenter.default.addObserver(self, selector: #selector(secureInputChanged(_:)),
                                               name: SecureInput.didChangeNotification, object: nil)
        guard connectOrAsk() else { return }
        appearanceObservation = NSApp.observe(\.effectiveAppearance) { _, _ in
            DispatchQueue.main.async { SessionManager.shared.appearanceChanged() }
        }
        // The reconnect timer keeps running during connectOrAsk's alerts: it may have restored
        // the session already (restoring twice opens every window twice).
        if !sessionReady { restoreSession() }
        // Remote hosts connect in the background; their tabs show "reconnecting" until then.
        Remotes.shared.start()
        UsageLimits.shared.configChanged()
        // Safety net only: tab changes save through the tab group's KVO.
        periodicTimer = Timer.scheduledTimer(timeInterval: 15, target: self, selector: #selector(periodicSave),
                                             userInfo: nil, repeats: true)
        if let err = config.loadError {
            currentController?.content.focusedView?.showToast("Config error: \(err)", duration: 8)
        } else if let err = QuickTerminal.shared.configChanged() {
            currentController?.content.focusedView?.showToast(err, duration: 8)
        } else if let warning = config.warningSummary {
            currentController?.content.focusedView?.showToast(warning, duration: 8)
        }
    }

    /// Connects, asking the user to retry or quit when the daemon is unreachable.
    private func connectOrAsk() -> Bool {
        while true {
            if connectAndHello() { return true }
            if helloRefusedForVersion {
                helloRefusedForVersion = false
                let alert = NSAlert()
                alert.messageText = "Restart the session daemon?"
                alert.informativeText = "Thurm was updated, but the session daemon still runs the previous "
                    + "version. Restarting it closes the programs running in your tabs; tabs, splits, "
                    + "scrollback and working directories come back."
                alert.addButton(withTitle: "Restart Daemon")
                alert.addButton(withTitle: "Quit")
                let restart = alert.runModal() == .alertFirstButtonReturn
                // Reconnect timers keep running during the alert and may have started a
                // compatible daemon already (if the old one went away meanwhile).
                if Core.shared.isConnected || connectAndHello() { return true }
                if restart {
                    if !thurm_terminate_daemon() { tlog("could not stop the old daemon") }
                    continue
                }
                NSApp.terminate(nil)
                return false
            }
            let alert = NSAlert()
            alert.messageText = "Cannot connect to the Thurm session daemon"
            alert.informativeText = Core.shared.lastError ?? "thurmd could not be started."
            alert.alertStyle = .critical
            alert.addButton(withTitle: "Retry")
            alert.addButton(withTitle: "Quit")
            if alert.runModal() != .alertFirstButtonReturn {
                NSApp.terminate(nil)
                return false
            }
        }
    }

    private func connectAndHello() -> Bool {
        guard Core.shared.connect() else {
            // The client says Hello while connecting, so a daemon on another protocol fails here.
            if let e = Core.shared.lastError, e.contains("protocol mismatch") { return helloRefused(e) }
            return false
        }
        let resp = Core.shared.request(object: ["Hello": ["client": "Thurm.app", "version": thurm_protocol_version(), "ui": true]])
        if let d = resp as? [String: Any], let e = d["error"] as? String {
            Core.shared.disconnect()
            return helloRefused(e)
        }
        guard resp != nil else { return false }
        if let hello = (resp as? [String: Any])?["Hello"] as? [String: Any],
           let build = hello["build"] as? String, build != Core.buildId
        {
            tlog("thurmd runs build \(build), this app \(Core.buildId)")
            Core.shared.disconnect()
            if upgradeDaemonInPlace() { return connectAndHello() }
            // Compatible: carry on with it, and say so when it is known to be older.
            guard Core.shared.connect() else { return false }
            Core.shared.request(object: ["Hello": ["client": "Thurm.app", "version": thurm_protocol_version(), "ui": true]])
            Notices.shared.localDaemonBuild = Notices.isNewer(Core.buildId, than: build) ? build : nil
        } else {
            Notices.shared.localDaemonBuild = nil
        }
        sendAppearance()
        Extensions.shared.connected()
        return true
    }

    /// The daemon refused our Hello. After an update it still runs the previous build: replace
    /// it in place, or (too old for that) have `connectOrAsk` offer a restart.
    private func helloRefused(_ e: String) -> Bool {
        tlog("Hello rejected: \(e)")
        if e.contains("protocol mismatch") {
            if upgradeDaemonInPlace() { return connectAndHello() }
            helloRefusedForVersion = true
        }
        Core.shared.lastError = "The running thurmd refused the connection: \(e). Quit Thurm and run `thurm daemon stop` (the layout is restored), then open Thurm again."
        return false
    }

    /// Tried once per launch: a daemon that did not upgrade won't on a second try.
    private var daemonUpgradeTried = false

    /// Replaces the running daemon with the bundled one; every pane keeps its processes.
    /// Blocks until the new daemon answers (normally well under a second).
    private func upgradeDaemonInPlace() -> Bool {
        guard !daemonUpgradeTried, let path = Core.daemonPath else { return false }
        daemonUpgradeTried = true
        let rc = path.withCString { thurm_upgrade_daemon($0) }
        tlog(rc == 0 ? "thurmd upgraded in place" : "in-place daemon upgrade not possible (\(rc))")
        return rc == 0
    }

    /// Set while the daemon is replaced from `offerDaemonRestart`: the connection it drops is
    /// taken up again once that is done, not raced against the old daemon.
    private var replacingDaemon = false

    /// The daemon stayed on an older build (see `Notices`): restart it, or try the in-place
    /// upgrade again.
    func offerDaemonRestart() {
        guard let build = Notices.shared.localDaemonBuild, !replacingDaemon, let path = Core.daemonPath else { return }
        let alert = NSAlert()
        alert.messageText = "Restart the session daemon?"
        alert.informativeText = "It still runs Thurm \(build) and could not be replaced in place, so features of "
            + "\(Core.buildId) that need it don't work yet. Restarting it closes the programs running in your "
            + "tabs; tabs, splits, scrollback and working directories come back."
        alert.addButton(withTitle: "Restart Daemon")
        alert.addButton(withTitle: "Try in Place Again")
        alert.addButton(withTitle: "Cancel")
        let choice = alert.runModal()
        guard choice == .alertFirstButtonReturn || choice == .alertSecondButtonReturn else { return }
        let restart = choice == .alertFirstButtonReturn
        replacingDaemon = true
        DispatchQueue.global(qos: .userInitiated).async {
            let ok = restart ? thurm_terminate_daemon() : path.withCString { thurm_upgrade_daemon($0) } == 0
            DispatchQueue.main.async {
                tlog(restart ? "restarting thurmd: \(ok ? "stopped" : "could not stop it")"
                             : "thurmd upgrade retried: \(ok ? "upgraded" : "not possible")")
                self.replacingDaemon = false
                if !ok {
                    let what = restart ? "The daemon could not be stopped" : "The daemon could not be replaced in place"
                    self.currentController?.content.focusedView?.showToast("\(what) (see thurmd.log)", duration: 6)
                }
                // A replaced daemon dropped the connection; a new one starts if none runs.
                if !Core.shared.isConnected {
                    self.reconnectAttempt = 0
                    self.attemptReconnect()
                }
            }
        }
    }

    // MARK: - Appearance and themes

    private var lastSentDark: Bool?

    private func sendAppearance() {
        let dark = systemIsDark
        lastSentDark = dark
        Core.shared.send(object: ["SetAppearance": ["dark": dark]])
        for host in Core.shared.connectedRemotes { sendAppearance(to: host) }
    }

    /// Remote panes are drawn in this Mac's appearance too.
    func sendAppearance(to host: HostId) {
        Core.shared.send(object: ["SetAppearance": ["dark": systemIsDark]], host: host)
    }

    /// System light/dark switch: the daemon re-resolves colors (and broadcasts
    /// ConfigReloaded) when the theme follows the appearance.
    func appearanceChanged() {
        guard systemIsDark != lastSentDark else { return }
        sendAppearance()
        if config.followsAppearance {
            reloadConfig(notifyDaemon: false)
        }
    }

    /// Writes `colors.theme` through the daemon, which reloads every client. When the config
    /// cannot be written, the theme switches for this session only.
    @discardableResult
    func setTheme(_ spec: String) -> Bool {
        let resp = Core.shared.request(object: ["SetTheme": ["spec": spec]])
        // Saved: the reload that follows carries it. An error, no answer or anything else:
        // switch for this session only.
        if JSON.variant(resp)?.name == "Ok" {
            // The daemon's reload may have come first and still used the session's theme.
            if sessionTheme {
                sessionTheme = false
                _ = Core.shared.setSessionTheme(nil)
                reloadConfig(notifyDaemon: false)
            }
            return true
        }
        if let e = Core.shared.setSessionTheme(spec) {
            currentController?.content.focusedView?.showToast("Theme: \(e)", duration: 6)
            return false
        }
        sessionTheme = true
        reloadConfig(notifyDaemon: false)
        return true
    }

    // MARK: Theme picker

    /// Shows `name` in every window without saving it (nil: back to the configured theme).
    /// Only colors change, so this is cheap enough to follow the arrow keys.
    func previewTheme(_ name: String?) {
        guard let json = Core.shared.previewTheme(name),
              let t = JSON.decode(json) as? [String: Any] else { return }
        config.theme = Theme(json: t)
        configGeneration &+= 1
        for c in liveControllers {
            c.applyConfig()
            for tab in c.tabs {
                tab.appliedConfigGeneration = configGeneration
                for view in tab.content.views.values { view.needsRender = true }
            }
        }
    }

    /// View > Theme > Browse Themes…: every theme in a filterable list. Moving through it shows
    /// each one live; Enter keeps it (saved like choosing it from the menu), Escape goes back.
    func showThemePicker() {
        let current = config.followsAppearance ? (config.theme.isDark ? config.themeDark : config.themeLight)
            : config.theme.name
        let items = config.themes.map { t in
            CommandPalette.Item(title: t.name,
                                detail: (t.dark ? "Dark" : "Light") + (t.own ? " · Thurm" : ""),
                                shortcut: t.name == current ? "current" : "",
                                preview: { [weak self] in self?.previewTheme(t.name) },
                                action: { [weak self] in
                                    guard let self else { return }
                                    // Saving reloads the config, which ends the preview.
                                    if !self.chooseTheme(t.name) { self.previewTheme(nil) }
                                })
        }
        let row = config.themes.firstIndex { $0.name == current } ?? 0
        CommandPalette.shared.show(items: items, over: currentController?.window,
                                   placeholder: "Search \(items.count) themes…",
                                   footer: "↑↓ preview · ↩ keep · esc cancel", initialRow: row,
                                   onCancel: { [weak self] in self?.previewTheme(nil) })
    }

    /// Picks `name`. When following the appearance, it replaces the half it belongs to.
    @discardableResult
    func chooseTheme(_ name: String) -> Bool {
        guard config.followsAppearance else {
            return setTheme(name)
        }
        let dark = config.themes.first { $0.name == name }?.dark ?? true
        let light = dark ? config.themeLight : name
        let darkName = dark ? name : config.themeDark
        return setTheme("light:\(light),dark:\(darkName)")
    }

    func toggleFollowAppearance() {
        if config.followsAppearance {
            setTheme(config.theme.name)
            return
        }
        let current = config.themeDark
        let isDark = config.themes.first { $0.name == current }?.dark ?? true
        let partner = SessionManager.themePartner(current, wantDark: !isDark)
        let light = isDark ? partner : current
        let dark = isDark ? current : partner
        setTheme("light:\(light),dark:\(dark)")
    }

    /// The light (or dark) sibling of a theme, for turning on "Match System Appearance".
    static func themePartner(_ name: String, wantDark: Bool) -> String {
        let pairs = [("thurm-light", "thurm"), ("catppuccin-latte", "catppuccin-mocha")]
        for (light, dark) in pairs {
            if name == light || name == dark { return wantDark ? dark : light }
        }
        return wantDark ? "thurm" : "thurm-light"
    }

    // MARK: - Daemon queries

    /// `host`'s panes; nil when it did not answer.
    func fetchPanes(host: HostId = localHost) -> [PaneInfo]? {
        guard let v = JSON.variant(Core.shared.request("\"ListPanes\"", host: host)), ["Panes", "PanesWithActivity"].contains(v.name),
              let list = v.payload as? [Any]
        else { return nil }
        return list.compactMap { PaneInfo(json: $0, host: host) }
    }

    func fetchPaneInfo(_ key: PaneKey) -> PaneInfo? {
        guard let v = JSON.variant(Core.shared.request(object: ["PaneInfo": ["pane": key.number]], host: key.host)),
              ["PaneInfo", "PaneActivity"].contains(v.name)
        else { return nil }
        return PaneInfo(json: v.payload, host: key.host)
    }

    /// nil when the daemon did not answer (or its layout does not parse); an empty layout when
    /// it has none saved.
    func fetchLayout() -> Layout? {
        guard let v = JSON.variant(Core.shared.request("\"GetLayout\"")), v.name == "Layout" else { return nil }
        guard let json = v.payload as? String else { return v.payload is NSNull ? Layout(windows: []) : nil }
        return Layout.from(json: json)
    }

    /// Creates a pane in `host`'s daemon (default: the host of the pane it inherits from).
    /// Returns its key.
    func createPane(cols: Int, rows: Int, inheritFrom: PaneKey?, preset: String? = nil,
                    forkFrom: PaneKey? = nil, host: HostId? = nil, cwd: String? = nil,
                    command: [String]? = nil, hold: Bool = false) -> PaneKey? {
        let host = host ?? forkFrom?.host ?? inheritFrom?.host ?? localHost
        guard Core.shared.isConnected(host) else {
            currentController?.content.focusedView?.showToast(
                "\(host) is not connected (\(Remotes.shared.phaseLabel(host)))", duration: 4)
            return nil
        }
        // Only a pane of the same daemon can pass on its directory or session.
        let inherit = inheritFrom.flatMap { $0.host == host ? $0 : nil }
        let fork = forkFrom.flatMap { $0.host == host ? $0 : nil }
        let request: [String: Any] = ["CreatePane": [
            "fork_from": orNull(fork?.number),
            "command": orNull(command),
            "cwd": orNull(cwd),
            "env": [Any](),
            "size": paneSizeObject(cols: cols, rows: rows),
            "agent_preset": orNull(preset),
            "inherit_cwd_from": orNull(inherit?.number),
            "hold": hold,
        ] as [String: Any]]
        let resp = Core.shared.request(object: request, host: host)
        guard let v = JSON.variant(resp), v.name == "PaneCreated",
              let payload = v.payload as? [String: Any], let id = jsonUInt64(payload["pane"])
        else {
            showError(forkFrom != nil ? "Could not fork the agent session" : "Could not start a new shell",
                      response: resp)
            return nil
        }
        let key = PaneKey(host, id)
        if let info = fetchPaneInfo(key) { panes[key] = info }
        return key
    }

    /// The backing scale of the window new panes open in (not the main screen's: the window
    /// may be on another display).
    private var newPaneScale: CGFloat {
        currentController?.window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 2
    }

    private func paneSizeObject(cols: Int, rows: Int) -> [String: Any] {
        let scale = newPaneScale
        let shaper = FontShaper.shared(scale: scale)
        return [
            "cols": NSNumber(value: max(2, min(1000, cols))),
            "rows": NSNumber(value: max(1, min(1000, rows))),
            "cell_width": NSNumber(value: shaper?.cellWidth ?? 8),
            "cell_height": NSNumber(value: shaper?.cellHeight ?? 16),
        ]
    }

    /// Grid size that fits in `size` points.
    func gridSize(forPoints size: NSSize) -> (cols: Int, rows: Int) {
        let scale = newPaneScale
        guard let s = FontShaper.shared(scale: scale), size.width > 0, size.height > 0 else {
            return (config.columns, config.rows)
        }
        let w = size.width * scale - 2 * (config.paddingX * scale).rounded()
        let h = size.height * scale - 2 * (config.paddingY * scale).rounded()
        return (max(2, Int(w / CGFloat(s.cellWidth))), max(1, Int(h / CGFloat(s.cellHeight))))
    }

    /// Content size of a new window: `columns`×`rows` cells plus padding.
    func defaultContentSize() -> NSSize {
        let scale = NSScreen.main?.backingScaleFactor ?? 2
        guard let s = FontShaper.shared(scale: scale) else { return NSSize(width: 900, height: 600) }
        let w = CGFloat(config.columns * s.cellWidth) / scale + 2 * config.paddingX + 1
        let h = CGFloat(config.rows * s.cellHeight) / scale + 2 * config.paddingY + 1
        return NSSize(width: ceil(w), height: ceil(h))
    }

    // MARK: - Restore

    private func restoreSession() {
        // Nothing is changed (or saved over the daemon's layout) unless both answered: a failed
        // request is not an empty session.
        guard let infos = fetchPanes(), let saved = fetchLayout() else {
            restoreFailures += 1
            tlog("restoring the session failed (\(restoreFailures))")
            if restoreFailures < 5 {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.25 * Double(1 << restoreFailures)) { [weak self] in
                    guard let self, !self.sessionReady else { return }
                    self.restoreSession()
                }
            } else {
                let alert = NSAlert()
                alert.messageText = "Could not read the saved session"
                alert.informativeText = "thurmd did not answer. Your tabs are left as they were: quit and reopen Thurm to try again."
                alert.alertStyle = .warning
                alert.runModal()
            }
            return
        }
        restoreFailures = 0
        panes = panes.filter { $0.key.isRemote }
        for info in infos { panes[info.key] = info }
        let listed = Set(infos.map(\.key))
        // Remote tabs come back as they were and attach once their host connects (tabs of a
        // host no longer configured go).
        let remotes = Set(config.remoteNames)

        // A pane shows in one place. Skip the ones a window already shows, and the ones an
        // earlier window of the layout claimed (a layout saved after a double restore lists
        // them twice).
        var placed = Set(liveTabs.flatMap { $0.content.paneIds })
        var layout = saved
        layout.retainPanes { key in
            !placed.contains(key) && (key.isRemote ? remotes.contains(key.host) : listed.contains(key))
        }
        workspaces = layout.workspaces.map { Workspace(layout: $0) }
        if let quick = layout.quick {
            QuickTerminal.shared.restore(quick)
            placed.formUnion(quick.panes)
        }

        // Hidden workspaces' panes are in use too (they run until a window shows them), and
        // the quick terminal's.
        var used = Set(layout.workspaces.flatMap { $0.tabs.flatMap { $0.panes } })
        used.formUnion(layout.quick?.panes ?? [])
        for var w in layout.windows {
            w.tabs = w.tabs.filter { tab in tab.panes.allSatisfy { !placed.contains($0) } }
            placed.formUnion(w.tabs.flatMap { $0.panes })
            // Thurm has one window: the front one of the layout. Older layouts' other windows
            // come back as workspaces in the background.
            if regularControllers.isEmpty {
                restoreWindow(w)
            } else {
                hideWindowLayout(w)
            }
            used.formUnion(w.tabs.flatMap { $0.panes })
        }

        // Panes that exist but are not in the layout: tabs of a workspace in the background.
        let orphans = infos.filter { !used.contains($0.key) }.map { $0.key }
            .sorted { $0.id < $1.id }
        if !orphans.isEmpty {
            makeWorkspace().hiddenTabs = orphans.map { TabLayout(title: nil, root: .pane($0), focused: $0.id, zoomed: nil) }
        }

        if regularControllers.isEmpty {
            if let ws = workspacesByRecency.first(where: { !$0.hiddenTabs.isEmpty }) {
                openWorkspaceInNewWindow(ws)
            } else {
                newWindow()
            }
        }
        sessionReady = true
        prewarmHiddenWorkspaces()
        TailscaleService.shared.refresh()
        saveLayoutNow(force: true)
        focusChanged()
        onSessionReady?()
    }

    /// A saved window's tabs as a workspace in the background.
    private func hideWindowLayout(_ w: WindowLayout) {
        guard !w.tabs.isEmpty else { return }
        var ws = workspace(w.workspace)
        if ws == nil || (ws.map { isShown($0) || !$0.hiddenTabs.isEmpty } ?? false) {
            ws = makeWorkspace(host: w.tabs.first?.focusedKey.host ?? localHost)
        }
        ws?.hiddenTabs = w.tabs
        ws?.hiddenSelectedTab = min(max(0, w.selectedTab), w.tabs.count - 1)
    }

    private func restoreWindow(_ w: WindowLayout) {
        var frame: NSRect?
        if let f = w.frame, f.count == 4, f[2] > 50, f[3] > 50 {
            frame = clampToScreens(NSRect(x: f[0], y: f[1], width: f[2], height: f[3]))
        }
        // Older layouts have no workspaces, and two windows can't show the same one.
        var ws = workspace(w.workspace)
        if ws == nil || (ws.map { isShown($0) } ?? false) {
            ws = makeWorkspace(host: w.tabs.first?.focusedKey.host ?? localHost)
        }
        if config.sidebarTabs {
            let items = w.tabs.map { TabItem(layout: $0) }
            guard !items.isEmpty else { return }
            let selectedIndex = min(max(0, w.selectedTab), items.count - 1)
            let c = makeController(tabs: items, selectedIndex: selectedIndex, frame: frame)
            c.workspaceID = ws?.id ?? 0
            showAsNewWindow(c, frame: frame)
            prewarmWorkspaceTabs(items, excluding: c.selectedTab)
            if w.fullscreen, let win = c.window, !win.styleMask.contains(.fullScreen) {
                win.toggleFullScreen(nil)
            }
            return
        }
        var created: [TerminalWindowController] = []
        withWorkspaceSwitchBatch {
            for tab in w.tabs {
                let c = makeController(root: SplitNode(layout: tab.root), focused: tab.focusedKey, zoomed: tab.zoomedKey,
                                       title: tab.title, frame: frame, floating: tab.floating,
                                       floatingHidden: tab.floatingHidden)
                c.workspaceID = ws?.id ?? 0
                c.adopt(tab)
                if let prev = created.last {
                    attachAsTab(c, to: prev)
                } else {
                    showAsNewWindow(c, frame: frame)
                }
                created.append(c)
            }
        }
        guard !created.isEmpty else { return }
        let selected = created[min(max(0, w.selectedTab), created.count - 1)]
        selected.window?.makeKeyAndOrderFront(nil)
        for c in created { c.updateTitle() }
        (selected.window as? TerminalWindow)?.finishWorkspaceSwitchSync()
        if w.fullscreen, let win = selected.window, !win.styleMask.contains(.fullScreen) {
            win.toggleFullScreen(nil)
        }
    }

    /// Keeps restored windows on a connected screen.
    private func clampToScreens(_ frame: NSRect) -> NSRect {
        let screens = NSScreen.screens
        if screens.contains(where: { $0.visibleFrame.intersects(frame) }) { return frame }
        guard let screen = NSScreen.main ?? screens.first else { return frame }
        let vf = screen.visibleFrame
        let w = min(frame.width, vf.width)
        let h = min(frame.height, vf.height)
        return NSRect(x: vf.midX - w / 2, y: vf.midY - h / 2, width: w, height: h)
    }

    // MARK: - Windows and tabs

    var liveControllers: [TerminalWindowController] {
        controllers.filter { !$0.isClosed }
    }

    /// Every live controller but the quick terminal's.
    var regularControllers: [TerminalWindowController] {
        liveControllers.filter { !$0.isQuick }
    }

    /// Every live tab across all open windows (including hosted tabs in single-window sidebar mode).
    var liveTabs: [TabItem] {
        liveControllers.flatMap(\.tabs).filter { !$0.isClosed }
    }

    /// Every live regular tab (excluding the quick terminal).
    var regularTabs: [TabItem] {
        regularControllers.flatMap(\.tabs).filter { !$0.isClosed }
    }

    /// The controller of the key window, else the last key one (the quick terminal only while
    /// it is on screen).
    var currentController: TerminalWindowController? {
        if let c = NSApp.keyWindow?.windowController as? TerminalWindowController, !c.isClosed { return c }
        if let c = NSApp.mainWindow?.windowController as? TerminalWindowController, !c.isClosed { return c }
        if let c = lastKeyController, !c.isClosed, !c.isQuick || QuickTerminal.shared.isShown { return c }
        return currentRegularController
    }

    /// `currentController`, or the last regular window's when that is the quick terminal: where
    /// new tabs and workspaces go.
    var currentRegularController: TerminalWindowController? {
        if let c = NSApp.keyWindow?.windowController as? TerminalWindowController, !c.isClosed, !c.isQuick {
            return c
        }
        if let c = lastRegularKeyController, !c.isClosed { return c }
        return regularControllers.last
    }

    func tab(for pane: PaneKey) -> TabItem? {
        liveTabs.first { $0.content.contains(pane) }
    }

    func controller(for pane: PaneKey) -> TerminalWindowController? {
        tab(for: pane)?.windowController
    }

    func view(for pane: PaneKey) -> TerminalView? {
        tab(for: pane)?.content.views[pane]
    }

    /// Every view of `host`'s panes (including cached background workspace tabs).
    func views(of host: HostId) -> [TerminalView] {
        let live = liveTabs.flatMap { $0.content.views.values.filter { $0.pane.host == host } }
        let cached = workspaces.compactMap(\.cachedTabs).joined()
            .flatMap { $0.content.views.values.filter { $0.pane.host == host } }
        return live + cached
    }

    func makeController(tabs: [TabItem], selectedIndex: Int = 0, frame: NSRect?,
                        quick: Bool = false) -> TerminalWindowController {
        let c = TerminalWindowController(tabs: tabs, selectedIndex: selectedIndex,
                                         contentSize: defaultContentSize(), quick: quick)
        if let frame = frame {
            c.window?.setFrame(frame, display: false)
        }
        controllers.append(c)
        return c
    }

    func makeController(root: SplitNode, focused: PaneKey, zoomed: PaneKey?, title: String?,
                        frame: NSRect?, quick: Bool = false, floating: [FloatingPane]? = nil,
                        floatingHidden: Bool? = nil) -> TerminalWindowController {
        let c = TerminalWindowController(root: root, focused: focused, zoomed: zoomed, floating: floating ?? [],
                                         floatingHidden: floatingHidden ?? false, title: title,
                                         contentSize: defaultContentSize(), quick: quick)
        if let frame = frame {
            c.window?.setFrame(frame, display: false)
        }
        controllers.append(c)
        return c
    }

    func attachAsTab(_ c: TerminalWindowController, to host: TerminalWindowController,
                     ordered: NSWindow.OrderingMode = .above) {
        c.workspaceID = host.workspaceID
        guard let hostWindow = host.window, let w = c.window else { return }
        w.setFrame(hostWindow.frame, display: false)
        hostWindow.addTabbedWindow(w, ordered: ordered)
        // Its sidebar lists the whole tab group from its first frame, not just itself until the
        // next refresh.
        c.tabSplit?.match(host.tabSplit)
    }

    /// Shows a window as its own window (never auto-tabbed into another one).
    func showAsNewWindow(_ c: TerminalWindowController, frame: NSRect?) {
        guard let w = c.window else { return }
        if let frame = frame {
            w.setFrame(frame, display: false)
        } else if let key = currentController?.window, key !== w, key.isVisible {
            w.setFrameTopLeftPoint(NSPoint(x: key.frame.minX + 26, y: key.frame.maxY - 26))
        } else {
            w.center()
        }
        w.tabbingMode = .disallowed
        w.makeKeyAndOrderFront(nil)
        w.tabbingMode = .preferred
    }

    /// A new shell (or an existing pane) in a new workspace (or `ws`). Thurm has one window: it
    /// switches to that workspace, or with `background` keeps showing its own; the window is
    /// made here when there is none.
    func newWindow(pane existing: PaneKey? = nil, preset: String? = nil, workspace ws: Workspace? = nil,
                   background: Bool = false) {
        let host = existing?.host ?? ws?.host ?? localHost
        let window = currentRegularController.flatMap { $0.window == nil ? nil : $0 }
        var id = existing
        if id == nil {
            let size = gridSize(forPoints: window?.content.bounds.size ?? defaultContentSize())
            id = createPane(cols: size.cols, rows: size.rows, inheritFrom: nil, preset: preset, host: host)
        }
        guard let pane = id else { return }
        if let window {
            let target = ws ?? makeWorkspace(host: host)
            addHiddenTab(pane, handoff: nil, to: target, in: window, reveal: !background)
            return
        }
        let c = makeController(root: .leaf(pane), focused: pane, zoomed: nil, title: nil, frame: nil)
        c.workspaceID = (ws ?? makeWorkspace(host: host)).id
        showAsNewWindow(c, frame: nil)
        scheduleLayoutSave()
    }

    /// Cmd+T: a new tab next to `host`, inheriting the focused pane's cwd. The pane runs on
    /// the daemon of `host`'s workspace (a remote workspace's tabs are that host's panes). A pane
    /// of another host goes to a workspace of that host: the window switches to it with
    /// `reveal`, else it waits in the background. Returns the pane.
    @discardableResult
    func newTab(from hostIn: TerminalWindowController?, pane existing: PaneKey? = nil, preset: String? = nil,
                cwd: String? = nil, host daemonIn: HostId? = nil, handoff: String? = nil,
                reveal: Bool = false, background: Bool = false, append: Bool = false,
                ordered: NSWindow.OrderingMode = .above) -> PaneKey? {
        // The quick terminal has one tab: new ones go to the last regular window.
        var host = hostIn ?? currentController
        if host?.isQuick ?? false { host = currentRegularController }
        let daemon = daemonIn ?? existing?.host ?? host.flatMap { workspace($0.workspaceID)?.host } ?? host?.host
            ?? localHost
        var id = existing
        if id == nil {
            let size = gridSize(forPoints: host?.content.bounds.size ?? defaultContentSize())
            id = createPane(cols: size.cols, rows: size.rows, inheritFrom: host?.focusedPane, preset: preset,
                            host: daemon, cwd: cwd)
        }
        guard let pane = id else { return nil }
        if let host, host.window != nil {
            if (workspace(host.workspaceID)?.host ?? host.host) == pane.host {
                if host.usesSingleWindowTabs && group(of: host).count == 1 {
                    let tab = TabItem(root: .leaf(pane), focused: pane, zoomed: nil)
                    tab.handoffID = handoff
                    let insertIndex: Int
                    if append || background {
                        insertIndex = host.tabs.count
                    } else {
                        insertIndex = ordered == .below ? host.selectedTabIndex : host.selectedTabIndex + 1
                    }
                    host.insertTab(tab, at: insertIndex, select: !background)
                    if !background { host.window?.makeKeyAndOrderFront(nil) }
                } else {
                    let c = makeController(root: .leaf(pane), focused: pane, zoomed: nil, title: nil, frame: nil)
                    c.handoffID = handoff
                    let anchor = append || background ? group(of: host).last ?? host : host
                    if background {
                        // Append in creation order and keep the selected tab.
                        let selected = host.window?.tabGroup?.selectedWindow ?? host.window
                        attachAsTab(c, to: anchor)
                        if let selected { selected.tabGroup?.selectedWindow = selected }
                    } else {
                        attachAsTab(c, to: anchor, ordered: ordered)
                        c.window?.makeKeyAndOrderFront(nil)
                    }
                }
            } else {
                let ws = workspacesByRecency.first { $0.host == pane.host && !isShown($0) }
                    ?? makeWorkspace(host: pane.host)
                addHiddenTab(pane, handoff: handoff, to: ws, in: host, reveal: reveal)
            }
        } else {
            // No window yet: this tab's workspace opens in the one window.
            let ws = workspacesByRecency.first { $0.host == pane.host && !isShown($0) }
                ?? makeWorkspace(host: pane.host)
            if config.sidebarTabs {
                let firstTab = TabItem(root: .leaf(pane), focused: pane, zoomed: nil)
                firstTab.handoffID = handoff
                let rest: [TabItem]
                if let cached = ws.takeCachedTabs() {
                    rest = cached.tabs
                } else {
                    rest = ws.hiddenTabs.map { TabItem(layout: $0) }
                    ws.hiddenTabs = []
                    ws.hiddenSelectedTab = 0
                }
                let frame = rest.isEmpty ? nil : currentRegularController?.window?.frame
                let c = makeController(tabs: [firstTab] + rest, selectedIndex: 0, frame: frame)
                c.workspaceID = ws.id
                ws.lastActive = Date()
                showAsNewWindow(c, frame: frame)
            } else {
                let c = makeController(root: .leaf(pane), focused: pane, zoomed: nil, title: nil, frame: nil)
                c.handoffID = handoff
                c.workspaceID = ws.id
                if !ws.hiddenTabs.isEmpty {
                    // Its hidden tabs come along.
                    let frame = currentRegularController?.window?.frame
                    withWorkspaceSwitchBatch {
                        showAsNewWindow(c, frame: frame)
                        var previous = c
                        for tab in ws.hiddenTabs {
                            let t = makeController(root: SplitNode(layout: tab.root), focused: tab.focusedKey,
                                                   zoomed: tab.zoomedKey, title: tab.title, frame: nil, floating: tab.floating)
                            t.adopt(tab)
                            attachAsTab(t, to: previous)
                            previous = t
                        }
                        ws.hiddenTabs = []
                        c.window?.makeKeyAndOrderFront(nil)
                    }
                    (c.window as? TerminalWindow)?.finishWorkspaceSwitchSync()
                    refreshSidebars()
                } else {
                    showAsNewWindow(c, frame: nil)
                }
            }
        }
        scheduleLayoutSave()
        return pane
    }

    /// CLI tabs follow the source pane's workspace and keep creation order.
    func newCommandTab(_ pane: PaneKey, target: PaneKey?, background: Bool) {
        if let target, let ws = workspace(containing: target), !isShown(ws) {
            addHiddenTab(pane, handoff: nil, to: ws, in: currentRegularController, reveal: !background)
            return
        }
        newTab(from: target.flatMap { controller(for: $0) } ?? currentController,
               pane: pane, background: background, append: true)
    }

    /// Cmd+D / Cmd+Shift+D.
    func splitFocused(in c: TerminalWindowController, direction: SplitDirName, preset: String? = nil,
                      fork: Bool = false, cwd: String? = nil) {
        let target = c.focusedPane
        var size = gridSize(forPoints: c.content.focusedView?.frame.size ?? c.content.bounds.size)
        if direction == .right || direction == .left {
            size.cols = max(2, size.cols / 2)
        } else {
            size.rows = max(1, size.rows / 2)
        }
        guard let id = createPane(cols: size.cols, rows: size.rows, inheritFrom: target, preset: preset,
                                  forkFrom: fork ? target : nil, cwd: cwd) else { return }
        c.content.split(target: target, newPane: id, direction: direction)
        scheduleLayoutSave()
    }

    /// A new shell floating above the splits of `c`'s tab, where its focused pane is.
    func newFloatingPane(in c: TerminalWindowController) {
        guard !c.isQuick else { return }
        let size = gridSize(forPoints: c.content.newCardRect.size)
        guard let id = createPane(cols: size.cols, rows: size.rows, inheritFrom: c.focusedPane) else { return }
        c.content.addNewFloating(id)
    }

    /// Closes a specific tab (selected or background hosted tab), confirming when needed.
    func closeTab(_ tab: TabItem) {
        guard let c = tab.windowController, !c.isClosed, !tab.isClosed else { return }
        if c.usesSingleWindowTabs && group(of: c).count == 1 {
            if tab === c.selectedTab {
                c.window?.performClose(nil)
                return
            }
            let (confirmed, handoffRemove) = c.confirmClose(tab: tab)
            guard confirmed else { return }
            for id in tab.content.paneIds { sendClosePane(id) }
            if let handoffRemove {
                DispatchQueue.main.async { self.removeHandoffWorktree(handoffRemove) }
            }
            _ = c.removeTab(tab, detach: true)
            return
        }
        c.window?.performClose(nil)
    }

    /// Cmd+W: close the focused pane (confirming when a program is running).
    func userClosePane(_ id: PaneKey) {
        // A handoff tab's last pane: close the tab, which asks about the agent and its worktree.
        if let t = tab(for: id), t.handoffID != nil, t.content.paneIds == [id] {
            closeTab(t)
            return
        }
        if config.confirmClose, let info = panes[id], info.hasRunningProcess {
            let alert = NSAlert()
            alert.messageText = "Close this pane?"
            alert.informativeText = "\(info.foregroundName ?? "A process") is still running and will be terminated."
            alert.alertStyle = .warning
            alert.addButton(withTitle: "Close")
            alert.addButton(withTitle: "Cancel")
            guard alert.runModal() == .alertFirstButtonReturn else { return }
        }
        sendClosePane(id)
        removePaneFromUI(id)
    }

    func sendClosePane(_ id: PaneKey) {
        evictWarmPane(id)
        if Core.shared.isConnected(id.host) {
            Core.shared.send(object: ["ClosePane": ["pane": id.number]], host: id.host)
        } else {
            // Its host is offline: close it there once it is back.
            Remotes.shared.pendingCloses[id.host, default: [:]][id.id] = panes[id]?.pid ?? 0
        }
        panes.removeValue(forKey: id)
    }

    /// Removes a pane's view; closes the tab when it was the last pane.
    func removePaneFromUI(_ id: PaneKey) {
        guard let t = tab(for: id), let c = t.windowController else {
            removePaneFromBackgroundWorkspaces(id)
            return
        }
        let empty = t.content.remove(pane: id)
        if empty {
            if c.usesSingleWindowTabs && c.tabs.count > 1 {
                _ = c.removeTab(t, detach: true)
            } else {
                guard !showNextWorkspace(closing: c) else { return }
                c.closingWithoutConfirmation = true
                c.window?.close()
            }
        } else if t.isSelected {
            c.updateTitle()
        } else {
            refreshSidebars()
        }
        focusChanged()
        scheduleLayoutSave()
    }

    private func removePaneFromBackgroundWorkspaces(_ id: PaneKey) {
        var changed = false
        for ws in workspaces where !isShown(ws) {
            let before = ws.hiddenTabs
            ws.retainHiddenPanes { $0 != id }
            if ws.hiddenTabs != before { changed = true }
        }
        if changed {
            workspaces.removeAll { !isShown($0) && $0.hiddenTabs.isEmpty }
            refreshSidebars()
            scheduleLayoutSave()
        }
    }

    func controllerDidClose(_ c: TerminalWindowController) {
        c.isClosed = true
        if c.isQuick { QuickTerminal.shared.controllerClosed(c) }
        refreshSidebars()
        if lastKeyController === c { lastKeyController = nil }
        if lastRegularKeyController === c { lastRegularKeyController = nil }
        // Drop our reference after AppKit is done with the window delegate callbacks.
        DispatchQueue.main.async {
            SessionManager.shared.controllers.removeAll { $0 === c }
        }
        if !isTerminating && !isSwitchingWorkspace {
            scheduleLayoutSave()
            focusChanged()
        }
    }

    func controllerBecameKey(_ c: TerminalWindowController) {
        lastKeyController = c
        if !c.isQuick { lastRegularKeyController = c }
        workspace(c.workspaceID)?.lastActive = Date()
        recordTabVisit(c.selectedTab)
        focusChanged()
        scheduleLayoutSave()
        // A tab dragged out into its own window: after AppKit is done with the drag.
        if !c.isQuick { DispatchQueue.main.async { SessionManager.shared.foldExtraWindows() } }
    }

    /// Called when a hosted tab becomes active inside a single-window controller.
    func tabSelected(_ tab: TabItem, in c: TerminalWindowController) {
        lastKeyController = c
        if !c.isQuick { lastRegularKeyController = c }
        workspace(c.workspaceID)?.lastActive = Date()
        recordTabVisit(tab)
        focusChanged()
        scheduleLayoutSave()
    }

    /// Focuses a pane, bringing its tab to the front.
    func focusPane(_ id: PaneKey, activate: Bool) {
        guard let t = tab(for: id), let c = t.windowController else { return }
        if c.isQuick {
            QuickTerminal.shared.show()
        } else {
            if c.usesSingleWindowTabs && !t.isSelected {
                c.selectTab(t, focus: false)
            }
            c.window?.makeKeyAndOrderFront(nil)
        }
        t.content.focus(id)
        if activate { NSApp.activate() }
    }

    func isPaneFocused(_ id: PaneKey) -> Bool {
        guard NSApp.isActive, let c = NSApp.keyWindow?.windowController as? TerminalWindowController else {
            return false
        }
        return c.content.focusedPane == id
    }

    // MARK: - Focus side effects (secure input, badges)

    func focusChanged() {
        updateSecureInput()
        withdrawNotificationsOnScreen()
        updateDockBadge()
        refreshSidebars()
    }

    /// On screen in the active window's tab counts as read, as for the Dock badge: those
    /// panes' notifications leave Notification Center. Here and not in `updateDockBadge`,
    /// which runs on every pane update: a notification of a pane that is on screen without
    /// the keyboard focus stays until the user next moves the focus. Also runs when a zoom
    /// changes which panes are on screen.
    func withdrawNotificationsOnScreen() {
        guard NSApp.isActive, let c = NSApp.keyWindow?.windowController as? TerminalWindowController else {
            return
        }
        Notifications.shared.withdraw(panes: Set(c.content.visiblePaneIds))
    }

    private var sidebarRefreshPending = false

    /// Reload every vertical tab sidebar and horizontal tab strip on the next main-loop turn
    /// (coalesced).
    func refreshSidebars() {
        guard !isSwitchingWorkspace, !sidebarRefreshPending else { return }
        sidebarRefreshPending = true
        DispatchQueue.main.async {
            self.sidebarRefreshPending = false
            guard !self.isSwitchingWorkspace else { return }
            for c in self.liveControllers {
                c.sidebar?.reload()
                (c.window as? TerminalWindow)?.tabStrip?.reload()
            }
            TabHoverCard.shared.refresh()
        }
    }

    /// View > Tabs in Sidebar: switch `window.tab_style` (every window follows). Saved to the
    /// config when it can be written; otherwise it switches for this session only.
    func toggleSidebarTabs() {
        let sidebar = !config.sidebarTabs
        let resp = Core.shared.request(object: ["SetSetting": ["key": "window.tab_style",
                                                               "value": sidebar ? "\"sidebar\"" : "\"native\""]])
        // Saved, the daemon answers with the config's path; the reload that follows carries it.
        // An error, no answer or anything else: switch for this session only.
        if JSON.variant(resp)?.name == "Text" {
            sidebarTabsOverride = nil
        } else {
            sidebarTabsOverride = sidebar
            config.sidebarTabs = sidebar
            applyConfigToUI()
        }
    }

    /// How the sidebar lists tabs (View > Group Tabs by Repository, Tabs from All Workspaces,
    /// Only Active Tabs, and the filter button next to the sidebar button): switch the setting `key` that `path`
    /// reads. Saved to the config when it can be written; otherwise it switches for
    /// this session only.
    func toggleSidebarListing(_ key: String, _ path: WritableKeyPath<AppConfig, Bool>) {
        let on = !config[keyPath: path]
        sidebarListingOverride[path] = toggleSetting(key, to: on)
        if sidebarListingOverride[path] != nil {
            config[keyPath: path] = on
            applyConfigToUI()
        }
    }

    /// View > Show Active Pane Border: switch `window.active_pane_border`, as
    /// `toggleSidebarListing`.
    func toggleActivePaneBorder() {
        let on = !config.activePaneBorder
        activePaneBorderOverride = toggleSetting("window.active_pane_border", to: on)
        if activePaneBorderOverride != nil {
            config.activePaneBorder = on
            applyConfigToUI()
        }
    }

    /// View > Usage Limits in Sidebar: switch `window.sidebar_usage_<provider>`, as
    /// `toggleSidebarListing`.
    func toggleSidebarUsage(_ provider: UsageProvider) {
        let on = !config.sidebarUsage.contains(provider)
        sidebarUsageOverride[provider] = toggleSetting("window.sidebar_usage_\(provider.rawValue)", to: on)
        if sidebarUsageOverride[provider] != nil {
            if on { config.sidebarUsage.insert(provider) } else { config.sidebarUsage.remove(provider) }
            applyConfigToUI()
            UsageLimits.shared.configChanged()
        }
    }

    /// Saves a switched setting. Saved, the daemon answers with the config's path and the reload
    /// that follows carries it (nil). An error, no answer or anything else: the value to keep for
    /// this session only.
    private func toggleSetting(_ key: String, to value: Bool) -> Bool? {
        let resp = Core.shared.request(object: ["SetSetting": ["key": key, "value": value ? "true" : "false"]])
        return JSON.variant(resp)?.name == "Text" ? nil : value
    }

    func updateSecureInput() {
        var requested = false
        if config.autoSecureInput, let c = currentController, let info = panes[c.focusedPane] {
            requested = info.passwordInput
        }
        SecureInput.shared.setScoped(requested)
        refreshLockBadges()
    }

    @objc private func secureInputChanged(_ note: Notification) {
        refreshLockBadges()
    }

    func refreshLockBadges() {
        let show = SecureInput.shared.isActive && config.secureInputIndicator
        let key = NSApp.keyWindow?.windowController as? TerminalWindowController
        for t in liveTabs {
            for (id, view) in t.content.views {
                view.setLockBadgeVisible(show && t.windowController === key && t.isSelected
                                         && id == t.content.focusedPane)
            }
        }
    }

    /// Whether the pane's agent state counts on the Dock badge.
    private func onBadge(_ id: PaneKey) -> Bool {
        // An older daemon keeps an exited held pane's agent: nothing is left there to answer.
        guard let info = panes[id], info.alive, let agent = info.agent else { return false }
        return config.notice(for: agent).badge
    }

    func updateDockBadge() {
        // On screen in the active window's tab counts as read.
        if NSApp.isActive, let c = NSApp.keyWindow?.windowController as? TerminalWindowController {
            for id in c.content.visiblePaneIds where onBadge(id) {
                seenAlerts.insert(id)
            }
        }
        seenAlerts = seenAlerts.filter { onBadge($0) }
        let count = panes.values.filter { info in
            onBadge(info.key) && !seenAlerts.contains(info.key)
        }.count
        NSApp.dockTile.badgeLabel = count > 0 ? "\(count)" : nil
    }

    func appDidBecomeActive() {
        SecureInput.shared.setAppActive(true)
        for c in liveControllers { c.content.focusedView?.reportFocus() }
        focusChanged()
    }

    func appDidResignActive() {
        SecureInput.shared.setAppActive(false)
        for c in liveControllers { c.content.focusedView?.reportFocus() }
        refreshLockBadges()
    }

    /// The sidebar's hosts button and View > Tabs by Host: whether the sidebar lists every
    /// host's tabs under the host, and the one host it narrows to (nil: every host). Saved as
    /// `toggleSidebarListing` saves.
    func setSidebarHosts(byHost: Bool, host: HostId?) {
        var changed = false
        if byHost != config.sidebarByHost {
            sidebarListingOverride[\.sidebarByHost] = toggleSetting("window.sidebar_by_host", to: byHost)
            if sidebarListingOverride[\.sidebarByHost] != nil {
                config.sidebarByHost = byHost
                changed = true
            }
        }
        if byHost, host != config.sidebarHost {
            let value = host ?? ""
            let resp = Core.shared.request(object: ["SetSetting": ["key": "window.sidebar_host",
                                                                   "value": Self.tomlString(value)]])
            sidebarHostOverride = JSON.variant(resp)?.name == "Text" ? nil : value
            if sidebarHostOverride != nil {
                config.sidebarHost = host
                changed = true
            }
        }
        if changed { applyConfigToUI() }
    }

    /// `s` as a TOML basic string, the value `SetSetting` parses: quotes, backslashes and
    /// control characters escaped, so any host name saves as itself.
    static func tomlString(_ s: String) -> String {
        var out = "\""
        for u in s.unicodeScalars {
            switch u {
            case "\"": out += "\\\""
            case "\\": out += "\\\\"
            case "\n": out += "\\n"
            case "\r": out += "\\r"
            case "\t": out += "\\t"
            case _ where u.value < 0x20 || u.value == 0x7F: out += String(format: "\\u%04X", u.value)
            default: out.unicodeScalars.append(u)
            }
        }
        return out + "\""
    }

    // MARK: - Config and fonts

    func reloadConfig(notifyDaemon: Bool) {
        if notifyDaemon {
            Core.shared.request("\"ReloadConfig\"")
        }
        config = AppConfig.load()
        config.loadWarnings += applyKeys()
        if let o = sidebarTabsOverride { config.sidebarTabs = o }
        if let o = activePaneBorderOverride { config.activePaneBorder = o }
        for (path, on) in sidebarListingOverride { config[keyPath: path] = on }
        if let h = sidebarHostOverride { config.sidebarHost = h.isEmpty ? nil : h }
        for (p, on) in sidebarUsageOverride {
            if on { config.sidebarUsage.insert(p) } else { config.sidebarUsage.remove(p) }
        }
        applyConfigToUI()
        UsageLimits.shared.configChanged()
        Core.shared.reloadRemoteEngines()
        Remotes.shared.configChanged()
        Updater.shared.configChanged()
        TailscaleService.shared.configChanged()
        let quickError = QuickTerminal.shared.configChanged()
        let view = currentController?.content.focusedView
        if let err = config.loadError {
            view?.showToast("Config error: \(err)", duration: 8)
        } else if let err = quickError {
            view?.showToast(err, duration: 8)
        } else if let warning = config.warningSummary {
            view?.showToast(warning, duration: 8)
        } else if notifyDaemon {
            view?.showToast("Configuration reloaded")
        }
    }

    func applyConfigToUI() {
        FontShaper.invalidateAll()
        configGeneration &+= 1
        reconcileTabStyleMode()
        for c in liveControllers {
            c.applyConfig()
            for tab in c.tabs {
                tab.appliedConfigGeneration = configGeneration
                for view in tab.content.views.values {
                    view.metricsChanged()
                }
                tab.content.needsDisplay = true
            }
            c.updateTitle()
        }
        refreshSidebars()
        updateDockBadge()
        updateSecureInput()
    }

    /// Converts live windows cleanly between single-window hosted tabs (`sidebarTabs == true`)
    /// and native `NSWindowTabGroup` tabs (`sidebarTabs == false`) without losing any tabs,
    /// splits, floating panes, titles, pins, or selection.
    func reconcileTabStyleMode() {
        if config.sidebarTabs {
            var groups: [[TerminalWindowController]] = []
            for c in regularControllers where c.window != nil && !groups.contains(where: { $0.contains { $0 === c } }) {
                let g = group(of: c)
                if g.count > 1 { groups.append(g) }
            }
            guard !groups.isEmpty else { return }
            withWorkspaceSwitchBatch {
                for g in groups {
                    let selected = g.first { $0.window?.tabGroup?.selectedWindow === $0.window } ?? g[0]
                    let allTabs = g.flatMap(\.tabs).filter { !$0.isClosed }
                    let selIdx = allTabs.firstIndex { $0 === selected.selectedTab } ?? 0
                    for other in g where other !== selected {
                        other.markTabsTransferred()
                        other.window?.close()
                    }
                    selected.applyTabStyle()
                    selected.adoptHostedTabs(allTabs, selectedIndex: selIdx)
                    selected.window?.makeKeyAndOrderFront(nil)
                }
            }
        } else {
            for ws in workspaces { ws.invalidateCachedTabs() }
            let multi = regularControllers.filter { $0.tabs.count > 1 }
            guard !multi.isEmpty else { return }
            var syncedWindows: [TerminalWindow] = []
            withWorkspaceSwitchBatch {
                for c in multi {
                    let frame = c.window?.frame
                    let wsID = c.workspaceID
                    let (allTabs, selIdx) = c.keepOnlySelectedTab()
                    c.applyTabStyle()
                    guard let hostWindow = c.window else { continue }
                    // Tabs before `selIdx`: inserting each `.below` `hostWindow` preserves order
                    // `[t0, t1, ..., t_{selIdx-1}, sel]`.
                    for tab in allTabs[0..<selIdx] {
                        let nc = makeController(tabs: [tab], selectedIndex: 0, frame: frame)
                        nc.workspaceID = wsID
                        if let w = nc.window {
                            w.setFrame(hostWindow.frame, display: false)
                            hostWindow.addTabbedWindow(w, ordered: .below)
                        }
                    }
                    var anchor = hostWindow
                    if selIdx + 1 < allTabs.count {
                        for tab in allTabs[(selIdx + 1)...] {
                            let nc = makeController(tabs: [tab], selectedIndex: 0, frame: frame)
                            nc.workspaceID = wsID
                            if let w = nc.window {
                                w.setFrame(hostWindow.frame, display: false)
                                anchor.addTabbedWindow(w, ordered: .above)
                                anchor = w
                            }
                        }
                    }
                    hostWindow.tabGroup?.selectedWindow = hostWindow
                    hostWindow.makeKeyAndOrderFront(nil)
                    if let tw = hostWindow as? TerminalWindow { syncedWindows.append(tw) }
                }
            }
            for tw in syncedWindows { tw.finishWorkspaceSwitchSync() }
        }
    }

    func changeFontSize(by delta: CGFloat) {
        fontSizeOverride = max(6, min(72, effectiveFontSize + delta))
        applyConfigToUI()
    }

    func resetFontSize() {
        fontSizeOverride = nil
        applyConfigToUI()
    }

    /// Asks this Mac's daemon to run the `[[commands]]` entry `name`, for the focused pane when
    /// it is one of this Mac's (a remote pane's id means nothing to it). A failure to start shows
    /// in the pane; one later comes as a notification.
    func runUserCommand(_ name: String, args: String? = nil) {
        let view = currentController?.content.focusedView
        var pane: Any = NSNull()
        if let focused = currentController?.focusedPane, focused.host == localHost { pane = focused.id }
        if Extensions.shared.available {
            let focused = currentController?.focusedPane
            let remote = focused.map { $0.host != localHost } ?? false
            // What the action learns of where it runs: another host's pane by its host's name;
            // in one of this Mac's, the selection and the file under the pointer.
            var context: [String: Any] = [:]
            if remote, let host = focused?.host {
                context["remote"] = host
            } else if let view {
                if let text = view.selectedText() { context["selection"] = text }
                if let file = view.pointedFile() { context["file"] = file }
            }
            Extensions.shared.run(name, args: args, pane: pane as? UInt64, noPane: remote,
                                  context: context) { error in
                if let error { view?.showToast("\(name): \(error)", duration: 5) }
            }
            return
        }
        Core.shared.requestAsync(object: ["RunCommand": ["name": name, "pane": pane]], timeout: 10) { resp in
            let error = (resp as? [String: Any]).flatMap { jsonString($0["error"]) }
                ?? (resp == nil ? "Thurm's daemon is not connected" : nil)
            if let error { view?.showToast("\(name): \(error)", duration: 5) }
        }
    }

    /// Applies `[keybindings]` and the Commands menu (the daemon's actions, or `[[commands]]`
    /// from an older daemon) to the menus; returns what could not be used.
    @discardableResult
    func applyKeys() -> [String] {
        let (keys, commands) = Extensions.shared.menuInput(keybindings: config.keybindings, commands: config.commands)
        return MainMenu.applyKeys(keys, commands: commands)
    }

    func openConfigFile() {
        var path = config.configPath
        if path.isEmpty, let raw = thurm_config_path() {
            path = String(cString: raw)
            thurm_string_free(raw)
        }
        guard !path.isEmpty else { return }
        if !FileManager.default.fileExists(atPath: path) {
            _ = AppConfig.load() // writes the commented default config
        }
        let url = URL(fileURLWithPath: path)
        if !NSWorkspace.shared.open(url) {
            let textEdit = URL(fileURLWithPath: "/System/Applications/TextEdit.app")
            NSWorkspace.shared.open([url], withApplicationAt: textEdit,
                                    configuration: NSWorkspace.OpenConfiguration(), completionHandler: nil)
        }
    }

    // MARK: - Command palette

    /// ⌘⇧P: every command of the main menu (as enabled for the front window, with its
    /// shortcut), plus agent launch presets and forking the focused agent's session.
    func showCommandPalette() {
        var items = CommandPalette.menuCommands(NSApp.mainMenu)
        if let c = currentController, let agent = panes[c.focusedPane]?.agent, agent.sessionId != nil {
            items.append(CommandPalette.Item(title: "Fork \(agent.name) Session", detail: "in a split") {
                SessionManager.shared.splitFocused(in: c, direction: .right, fork: true)
            })
        }
        items += remotePaletteItems()
        for preset in presets(for: currentHost) {
            let name = preset.name
            let detail = preset.command.joined(separator: " ")
            items.append(CommandPalette.Item(title: "Launch \(name)", detail: detail) {
                SessionManager.shared.newTab(from: SessionManager.shared.currentController, preset: name)
            })
            items.append(CommandPalette.Item(title: "Launch \(name) in Split", detail: detail) {
                if let c = SessionManager.shared.currentController {
                    SessionManager.shared.splitFocused(in: c, direction: .right, preset: name)
                } else {
                    SessionManager.shared.newWindow(preset: name)
                }
            })
        }
        CommandPalette.shared.show(items: items, over: currentController?.window)
    }

    // MARK: - Layout persistence

    func buildLayout() -> Layout {
        let live = liveControllers
        // Front-to-back window order first, then the rest.
        var ordered: [TerminalWindowController] = []
        for w in NSApp.orderedWindows {
            if let c = w.windowController as? TerminalWindowController, !c.isClosed, !c.isQuick,
               !ordered.contains(where: { $0 === c }) {
                ordered.append(c)
            }
        }
        for c in live where !c.isQuick && !ordered.contains(where: { $0 === c }) {
            ordered.append(c)
        }

        // Window groups (tab groups), front to back.
        var groups: [[TerminalWindowController]] = []
        var selectedWindows: [NSWindow] = []
        var seen = Set<ObjectIdentifier>()
        for c in ordered {
            guard let w = c.window, !seen.contains(ObjectIdentifier(w)) else { continue }
            let group: [NSWindow] = w.tabGroup?.windows ?? [w]
            group.forEach { seen.insert(ObjectIdentifier($0)) }
            seen.insert(ObjectIdentifier(w))
            let tcs = group.compactMap { $0.windowController as? TerminalWindowController }
                .filter { !$0.isClosed && !$0.content.isEmpty }
            guard !tcs.isEmpty else { continue }
            groups.append(tcs)
            selectedWindows.append(w.tabGroup?.selectedWindow ?? w)
        }
        let selectedControllers = zip(groups, selectedWindows).map { g, sw in
            g.first { $0.window === sw } ?? g[0]
        }
        normalizeWorkspaces(groups: groups, selected: selectedControllers)

        var windows: [WindowLayout] = []
        for (g, selectedWindow) in zip(groups, selectedWindows) {
            let tabItems = g.flatMap(\.tabs).filter { !$0.isClosed && !$0.content.isEmpty }
            guard !tabItems.isEmpty else { continue }
            let selectedController = g.first { $0.window === selectedWindow } ?? g[0]
            let selectedTab = selectedController.selectedTab
            let selected = tabItems.firstIndex { $0 === selectedTab } ?? 0
            let tabs = tabItems.map { $0.tabLayout() }
            let f = selectedWindow.frame
            windows.append(WindowLayout(frame: [Double(f.minX), Double(f.minY), Double(f.width), Double(f.height)],
                                        tabs: tabs, selectedTab: selected,
                                        fullscreen: selectedWindow.styleMask.contains(.fullScreen),
                                        workspace: selectedController.workspaceID))
        }
        return Layout(windows: windows, workspaces: workspaceLayouts(), quick: QuickTerminal.shared.tabLayout)
    }

    /// Sends the layout to the daemon when it changed (or always with `force`).
    func saveLayoutNow(force: Bool = false, blocking: Bool = false) {
        guard sessionReady, Core.shared.isConnected else { return }
        guard let json = buildLayout().jsonString() else { return }
        if !force && json == lastLayoutJSON { return }
        lastLayoutJSON = json
        let request: [String: Any] = ["SetLayout": ["json": json]]
        if blocking {
            // At quit: a daemon that stopped answering must not hold up Quit (or an update).
            Core.shared.request(object: request, timeout: 3)
        } else {
            Core.shared.send(object: request)
        }
    }

    /// Debounced (500 ms) layout save.
    func scheduleLayoutSave() {
        guard !isTerminating, !isSwitchingWorkspace else { return }
        NSObject.cancelPreviousPerformRequests(withTarget: self, selector: #selector(debouncedSave), object: nil)
        perform(#selector(debouncedSave), with: nil, afterDelay: 0.5)
    }

    @objc private func debouncedSave() {
        foldExtraWindows()
        saveLayoutNow()
    }

    /// Catches changes without a notification (e.g. tabs reordered by dragging).
    @objc private func periodicSave() {
        if !isTerminating { saveLayoutNow() }
    }

    // MARK: - Termination

    func prepareForTermination() {
        guard !isTerminating else { return }
        NSObject.cancelPreviousPerformRequests(withTarget: self)
        saveLayoutNow(force: true, blocking: true)
        isTerminating = true
        periodicTimer?.invalidate()
        periodicTimer = nil
        if config.quitTerminates {
            Core.shared.request(object: ["Shutdown": ["kill_panes": true]], timeout: 3)
        }
        for t in liveTabs {
            t.content.detachAll()
        }
        for ws in workspaces {
            ws.invalidateCachedTabs()
        }
        // Remote daemons keep their panes; only the tunnels close.
        Remotes.shared.stop()
        Core.shared.disconnectAll()
        SecureInput.shared.releaseAll()
    }

    // MARK: - Reconnect

    private func handleDisconnect(host: HostId) {
        guard !isTerminating else { return }
        if host != localHost {
            Remotes.shared.connectionLost(host)
            return
        }
        Extensions.shared.disconnected()
        Core.shared.disconnect()
        if replacingDaemon {
            tlog("lost connection to thurmd while replacing it")
            return
        }
        tlog("lost connection to thurmd; reconnecting")
        reconnectAttempt = 0
        currentController?.content.focusedView?.showToast("Daemon connection lost, reconnecting…", duration: 3)
        attemptReconnect()
    }

    @objc private func attemptReconnect() {
        // `offerDaemonRestart` reconnects once the daemon is replaced.
        guard !isTerminating, !replacingDaemon, !Core.shared.isConnected else { return }
        if connectAndHello() {
            resync()
            return
        }
        reconnectAttempt += 1
        let delay = min(5.0, 0.25 * pow(2.0, Double(reconnectAttempt)))
        perform(#selector(attemptReconnect), with: nil, afterDelay: delay)
    }

    /// After reconnecting: drop views of vanished panes, resubscribe the rest.
    private func resync() {
        // Without an answer, keep every view: an empty list would drop them all.
        guard let infos = fetchPanes() else {
            tlog("resync: ListPanes failed")
            return
        }
        panes = panes.filter { $0.key.isRemote }
        for info in infos { panes[info.key] = info }
        for t in liveTabs {
            for id in t.content.paneIds where !id.isRemote && panes[id] == nil {
                removePaneFromUI(id)
            }
        }
        for ws in workspaces where !isShown(ws) {
            ws.retainHiddenPanes { $0.isRemote || panes[$0] != nil }
        }
        let live = liveControllers
        if live.isEmpty {
            sessionReady = false
            restoreSession()
            return
        }
        for t in liveTabs {
            for view in t.content.views.values where !view.pane.isRemote {
                view.resubscribe()
            }
        }
        for ws in workspaces where !isShown(ws) {
            for tab in ws.cachedTabs ?? [] {
                for view in tab.content.views.values where !view.pane.isRemote {
                    view.resubscribe()
                }
            }
        }
        for c in live { c.updateTitle() }
        currentController?.content.focusedView?.showToast("Reconnected")
        TailscaleService.shared.refresh()
        saveLayoutNow(force: true)
        focusChanged()
    }

    // MARK: - Events

    func coreDidReceiveEvent(_ name: String, payload: Any?, host: HostId) {
        guard !isTerminating else { return }
        let dict = payload as? [String: Any]
        let key = jsonUInt64(dict?["pane"]).map { PaneKey(host, $0) }
        switch name {
        case "PaneInfo", "PaneActivity":
            if let info = PaneInfo(json: payload, host: host) { paneInfoUpdated(info) }
        case "PaneExited":
            if let id = key {
                Notifications.shared.withdraw(pane: id)
                panes[id]?.alive = false
                updateDockBadge()
            }
        case "PaneClosed":
            if let id = key {
                evictWarmPane(id)
                Notifications.shared.withdraw(pane: id)
                panes.removeValue(forKey: id)
                removePaneFromUI(id)
                updateDockBadge()
            }
        case "TailscaleStatusChanged":
            if host == localHost { TailscaleService.shared.update(payload) }
        case "PaneRead":
            // Read here or on a phone: off the Dock badge and out of Notification Center.
            if let id = key {
                Notifications.shared.withdraw(pane: id)
                seenAlerts.insert(id)
                updateDockBadge()
            }
        case "Bell":
            if let id = key {
                view(for: id)?.flash()
                if !NSApp.isActive || !isPaneFocused(id) {
                    NSApp.requestUserAttention(.informationalRequest)
                }
                if !isPaneFocused(id) {
                    NotificationCenter.default.post(name: .tabAttention, object: controller(for: id)?.window)
                }
            }
        case "Notify":
            if let id = key {
                var title = jsonString(dict?["title"]) ?? "Thurm"
                if id.isRemote { title = "\(host) · \(title)" }
                let body = jsonString(dict?["body"]) ?? ""
                if config.notificationsEnabled && (!NSApp.isActive || !isPaneFocused(id)) {
                    Notifications.shared.post(pane: id, title: title, body: body,
                                              sound: jsonString(dict?["sound"]) ?? "default",
                                              permission: jsonUInt64(dict?["permission"]))
                }
            }
        case "ClipboardStore":
            // The user's own selection always; a program's OSC 52 write per the Mac's policy.
            if let text = jsonString(dict?["text"]),
               jsonBool(dict?["user"]) == true || Remotes.shared.clipboardWriteAllowed(host) {
                let pb = NSPasteboard.general
                pb.clearContents()
                pb.setString(text, forType: .string)
            }
        case "ClipboardWrite":
            if let key, let id = jsonUInt64(dict?["id"]) {
                Remotes.shared.clipboardWrite(key, id: id, text: jsonString(dict?["text"]))
            }
        case "ClipboardRequest":
            if let id = key { Remotes.shared.clipboardRequest(id) }
        case "PortsChanged":
            remotePortsChanged(dict, host: host)
        case "ClipboardQuery":
            if let key, let id = jsonUInt64(dict?["id"]), let mime = jsonString(dict?["mime"]) {
                Remotes.shared.clipboardQuery(key, id: id, mime: mime)
            }
        case "Ui":
            handleUi(payload, host: host)
        case "Ext":
            // A remote daemon's plugins ask its own users.
            if host == localHost, let dict { Extensions.shared.handle(dict) }
        case "ConfigReloaded":
            // A remote daemon's config is its own business.
            if host == localHost { reloadConfig(notifyDaemon: false) }
        case "Disconnected":
            handleDisconnect(host: host)
        default:
            // Attach / Output / Resized are consumed by the Rust core.
            break
        }
    }

    func paneInfoUpdated(_ info: PaneInfo) {
        let key = info.key
        let old = panes[key]
        // A dead pane we no longer know: an older daemon's last word after PaneClosed. Kept,
        // its agent would hold the Dock badge with no tab to read it in.
        if old == nil && !info.alive { return }
        panes[key] = info
        if let t = tab(for: key) {
            if t.isSelected {
                t.windowController?.updateTitle()
            } else {
                refreshSidebars()
            }
            t.content.refreshHeaders()
            t.content.views[key]?.maybeShowRestoredToast()
            let progress = TerminalView.shownProgress(info)
            if TerminalView.shownProgress(old) != progress { t.content.views[key]?.setProgress(progress) }
        }
        // A handoff's agent finished or asks for something: bring its commits back.
        if let status = info.agent?.status, status == .done || status == .error || status == .needsInput,
           old?.agent?.status != status {
            Remotes.shared.agentSettled(key)
        }
        if info.agent?.status == .done, old?.agent?.status != .done, !isPaneFocused(key) {
            NotificationCenter.default.post(name: .tabAttention, object: controller(for: key)?.window)
        }
        let wasWaiting = old?.agent?.status == .needsInput
        let waiting = info.agent?.status == .needsInput
        // A new state, message, or permission prompt is unread again.
        let prompt = info.agent?.permission
        let newPrompt = prompt != nil && prompt != old?.agent?.permission
        let newState = old?.agent?.noticeState != info.agent?.noticeState
        let newMessage = old?.agent?.message != info.agent?.message
        if newState || newMessage || newPrompt {
            seenAlerts.remove(key)
        }
        // Answered, or retired by keys typed in the pane: its buttons would do nothing.
        if old?.agent?.permission != nil && old?.agent?.permission != info.agent?.permission {
            Notifications.shared.withdrawPermission(pane: key)
        }
        if waiting && !wasWaiting && !isPaneFocused(key) {
            NSApp.requestUserAttention(.informationalRequest)
        }
        updateDockBadge()
        // The sidebar's agent list covers every pane, including hidden workspaces'; its tiles
        // follow the program even when the title stays.
        if old?.agent != info.agent || PaneIcon(old) != PaneIcon(info) { refreshSidebars() }
        // Progress, git, the directory or the program: only the card shows those.
        TabHoverCard.shared.refresh()
        if old?.passwordInput != info.passwordInput {
            updateSecureInput()
        }
        // Not at launch (no `old`): those agents were where they are when the app quit.
        if old != nil, old?.agent == nil, info.agent != nil { agentStarted(key) }
    }

    /// Commands sent by the `thurm` CLI through a daemon (`host`'s: its pane ids).
    func swapPanes(_ a: PaneKey, _ b: PaneKey) {
        guard a != b, let ta = tab(for: a), let tb = tab(for: b) else { return }
        if ta === tb {
            ta.content.swap(a, b)
        } else {
            let history = tabHistory
            ta.content.replace(a, with: b)
            tb.content.replace(b, with: a)
            tabHistory = history
            tabHistory.swapPanes(a, b)
            // Each pane's old view let go of it after its new one took it.
            ta.content.views[b]?.resubscribe()
            tb.content.views[a]?.resubscribe()
            if ta.isSelected { ta.windowController?.updateTitle() }
            if tb.isSelected { tb.windowController?.updateTitle() }
            focusChanged()
        }
        scheduleLayoutSave()
    }

    private func handleUi(_ payload: Any?, host daemon: HostId) {
        guard let v = JSON.variant(payload), let d = v.payload as? [String: Any] else { return }
        let key = { (field: String) in jsonUInt64(d[field]).map { PaneKey(daemon, $0) } }
        switch v.name {
        case "OpenLayout":
            guard let requestID = jsonUInt64(d["request_id"]) else { return }
            var failure: String? = "the desktop could not install the layout"
            defer {
                Core.shared.send(object: ["LayoutApplied": [
                    "request_id": requestID,
                    "error": failure.map { $0 as Any } ?? NSNull()
                ]], host: daemon)
            }
            guard let json = jsonString(d["json"]), let data = json.data(using: .utf8),
                  var tab = try? JSONDecoder().decode(TabLayout.self, from: data) else { return }
            func onHost(_ node: LayoutNode) -> LayoutNode {
                switch node {
                case .pane(let pane): return .pane(PaneKey(daemon, pane.id))
                case .split(let dir, let ratio, let first, let second):
                    return .split(dir: dir, ratio: ratio, first: onHost(first), second: onHost(second))
                }
            }
            tab.root = onHost(tab.root)
            let moved = Set(tab.root.panes)
            for pane in tab.root.panes {
                guard let info = fetchPaneInfo(pane) else {
                    failure = "a layout pane no longer exists"
                    return
                }
                panes[pane] = info
            }
            guard JSON.variant(Core.shared.request(object: ["CommitLayout": [
                "request_id": requestID
            ]], host: daemon))?.name == "Ok" else {
                failure = "layout was cancelled before installation"
                return
            }
            var emptied: [TabItem] = []
            for pane in tab.root.panes {
                if let t = self.tab(for: pane) {
                    if t.content.remove(pane: pane) {
                        emptied.append(t)
                    } else if t.isSelected {
                        t.windowController?.updateTitle()
                    }
                }
            }
            for ws in workspaces {
                ws.retainHiddenPanes { !moved.contains($0) }
            }
            let ws = makeWorkspace(name: tab.title, host: daemon)
            ws.hiddenTabs = [tab]
            openWorkspaceInNewWindow(ws)
            // Open the new window before closing an empty last window.
            for t in emptied {
                guard let c = t.windowController else { continue }
                if c.usesSingleWindowTabs && c.tabs.count > 1 {
                    _ = c.removeTab(t, detach: true)
                } else {
                    c.closingWithoutConfirmation = true
                    c.window?.close()
                }
            }
            scheduleLayoutSave()
            if tab.root.panes.allSatisfy({ self.tab(for: $0)?.workspaceID == ws.id }) {
                failure = nil
            }
        case "NewTab":
            guard let pane = key("pane") else { return }
            let ownWorkspace = jsonBool(d["new_window"]) ?? false
            if let t = tab(for: pane), let c = t.windowController {
                // Adopted while the app started (this command launched it): `--window` still
                // gets its own workspace, unless that would leave the window without a tab.
                if ownWorkspace, t.content.paneIds.count == 1, windowTabs(of: c).count > 1 {
                    moveTabToNewWorkspace(t)
                } else {
                    focusPane(pane, activate: true)
                }
                return
            }
            if panes[pane] == nil, let info = fetchPaneInfo(pane) { panes[pane] = info }
            // Launched by this command, the app may have adopted the pane as an orphan already:
            // it moves to where the command says.
            takeHiddenTab(pane)
            if ownWorkspace {
                // `thurm new-tab --window`: its own workspace, in the background (Thurm stays
                // where it is too).
                newWindow(pane: pane, background: true)
            } else {
                // `thurm handoff` opened it: tag and group it.
                newTab(from: currentController, pane: pane,
                       handoff: Remotes.shared.handoff(host: daemon, pane: pane.id)?.id)
                NSApp.activate()
            }
        case "Split":
            guard let pane = key("pane") else { return }
            if tab(for: pane) != nil {
                focusPane(pane, activate: true)
                return
            }
            if panes[pane] == nil, let info = fetchPaneInfo(pane) { panes[pane] = info }
            let dir = SplitDirName(rawValue: jsonString(d["dir"]) ?? "right") ?? .right
            let target = key("target")
            let host = target.flatMap { tab(for: $0) }
                ?? (currentController?.host == daemon ? currentController?.selectedTab : nil)
            if let host = host {
                host.content.split(target: target, newPane: pane, direction: dir)
                scheduleLayoutSave()
            } else {
                newTab(from: currentController, pane: pane)
            }
        case "Focus":
            if let pane = key("pane") { focusPane(pane, activate: true) }
        case "Equalize":
            if let pane = key("pane") { tab(for: pane)?.content.equalize() }
        case "Scroll":
            // The viewport lives in the app's copy of the terminal.
            if let pane = key("pane"), let scroll = d["scroll"] {
                Core.shared.send(object: ["Scroll": ["pane": pane.number, "scroll": scroll]], host: daemon)
            }
        case "SetTabTitle":
            if let pane = key("pane"), let t = tab(for: pane) {
                let title = jsonString(d["title"])
                t.titleOverride = (title?.isEmpty ?? true) ? nil : title
                scheduleLayoutSave()
            }
        case "RenameWorkspace":
            let name = (jsonString(d["name"]) ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if let pane = key("pane"), !name.isEmpty, let ws = workspace(containing: pane) {
                ws.name = name
                refreshSidebars()
                scheduleLayoutSave()
            }
        case "Open":
            // `thurm new-tab` / `split` with options (`--background`, `--size`).
            guard let pane = key("pane") else { return }
            let background = jsonBool(d["background"]) ?? false
            if tab(for: pane) != nil {
                if !background { focusPane(pane, activate: true) }
                return
            }
            if panes[pane] == nil, let info = fetchPaneInfo(pane) { panes[pane] = info }
            takeHiddenTab(pane)
            let share = jsonUInt64(d["percent"]).map { CGFloat(min(99, max(1, $0))) / 100 }
            if let dir = jsonString(d["split"]).flatMap(SplitDirName.init(rawValue:)) {
                let target = key("target")
                let host = target.flatMap { tab(for: $0) }
                    ?? (currentController?.host == daemon ? currentController?.selectedTab : nil)
                if let host {
                    host.content.split(target: target, newPane: pane, direction: dir, share: share,
                                       takeFocus: !background)
                    scheduleLayoutSave()
                } else {
                    newTab(from: currentController, pane: pane, background: background)
                }
            } else if jsonBool(d["new_window"]) ?? false {
                newWindow(pane: pane, background: true)
            } else {
                newCommandTab(pane, target: key("target"), background: background)
            }
            if !background { NSApp.activate() }
        case "Zoom":
            if let pane = key("pane") { tab(for: pane)?.content.zoom(pane, jsonBool(d["zoom"])) }
        case "MovePane":
            guard let pane = key("pane"), let target = key("target"), pane != target,
                  let from = tab(for: pane), let to = tab(for: target),
                  !from.isQuick, !to.isQuick,
                  let dir = SplitDirName(rawValue: jsonString(d["dir"]) ?? "right") else { return }
            let share = jsonUInt64(d["percent"]).map { CGFloat(min(99, max(1, $0))) / 100 }
            if from === to {
                to.content.move(pane, nextTo: target, direction: dir, share: share)
            } else {
                // Out of its tab first, so its view there lets go of the pane before the new one
                // takes it; an emptied tab closes once the pane is in its new place.
                let emptied = from.content.remove(pane: pane)
                to.content.split(target: target, newPane: pane, direction: dir, share: share, takeFocus: false)
                if to.isSelected { to.windowController?.updateTitle() }
                if emptied, let fc = from.windowController {
                    if fc.usesSingleWindowTabs && fc.tabs.count > 1 {
                        _ = fc.removeTab(from, detach: true)
                    } else {
                        fc.closingWithoutConfirmation = true
                        fc.window?.close()
                    }
                } else if from.isSelected {
                    from.windowController?.updateTitle()
                }
                focusChanged()
            }
            scheduleLayoutSave()
        case "AgentWall":
            if let pane = key("pane"), let c = controller(for: pane) { toggleAgentWall(in: c, on: jsonBool(d["on"])) }
        case "Float":
            guard let pane = key("pane"), let t = tab(for: pane), !t.isQuick || t.content.isFloating(pane) else { return }
            t.content.setFloating(pane, jsonBool(d["on"]))
        case "OpenFloating", "OpenFloatingAt":
            // `thurm new-float`, with `--frame` or `--cells` for `OpenFloatingAt`.
            guard let pane = key("pane") else { return }
            let background = jsonBool(d["background"]) ?? false
            let place = FloatPlace(json: d["place"])
            if let t = tab(for: pane) {
                if let place, !t.isQuick { t.content.placeFloating(pane, at: place) }
                if !background { focusPane(pane, activate: true) }
                return
            }
            if panes[pane] == nil, let info = fetchPaneInfo(pane) { panes[pane] = info }
            takeHiddenTab(pane)
            // Placed in cells, it goes on the tab of the pane it counts from.
            var anchor: UInt64? = nil
            if case let .cells(a, _, _, _) = place { anchor = a }
            let host = anchor.flatMap { tab(for: PaneKey(daemon, $0)) }
                ?? key("target").flatMap { tab(for: $0) }
                ?? (currentController?.host == daemon ? currentController?.selectedTab : nil)
            if let host, !host.isQuick, let hc = host.windowController {
                host.content.addNewFloating(pane, place: place, takeFocus: !background)
                if host.isSelected { hc.updateTitle() } else { refreshSidebars() }
                if !background {
                    if hc.usesSingleWindowTabs && !host.isSelected { hc.selectTab(host, focus: false) }
                    hc.window?.makeKeyAndOrderFront(nil)
                }
            } else {
                newTab(from: currentController, pane: pane, background: background)
            }
            if !background { NSApp.activate() }
        case "OpenFloatingMinimized":
            // `thurm new-float --minimized`: a chip from the start, that never takes the focus.
            guard let pane = key("pane") else { return }
            let place = FloatPlace(json: d["place"])
            if let t = tab(for: pane) {
                if !t.isQuick { t.content.minimize(pane, true, at: place) }
                return
            }
            if panes[pane] == nil, let info = fetchPaneInfo(pane) { panes[pane] = info }
            takeHiddenTab(pane)
            var anchor: UInt64? = nil
            if case let .cells(a, _, _, _) = place { anchor = a }
            let host = anchor.flatMap { tab(for: PaneKey(daemon, $0)) }
                ?? key("target").flatMap { tab(for: $0) }
                ?? (currentController?.host == daemon ? currentController?.selectedTab : nil)
            if let host, !host.isQuick {
                host.content.addNewFloating(pane, place: place, takeFocus: false, minimized: true)
                if host.isSelected { host.windowController?.updateTitle() } else { refreshSidebars() }
            } else {
                newTab(from: currentController, pane: pane, background: true)
            }
        case "MinimizeFloating":
            // `thurm float --minimize/--restore`.
            guard let pane = key("pane"), let t = tab(for: pane),
                  !t.isQuick || t.content.isFloating(pane) else { return }
            t.content.minimize(pane, jsonBool(d["minimized"]) ?? true, at: FloatPlace(json: d["place"]))
        case "PlaceFloating":
            // `thurm float --frame/--cells`.
            guard let pane = key("pane"), let place = FloatPlace(json: d["place"]),
                  let t = tab(for: pane), !t.isQuick else { return }
            t.content.placeFloating(pane, at: place)
        case "SwapPanes":
            guard let a = key("a"), let b = key("b") else { return }
            swapPanes(a, b)
        default:
            tlog("unknown UI command \(v.name)")
        }
    }

    // MARK: - Errors

    private func showError(_ message: String, response: Any?) {
        let alert = NSAlert()
        alert.messageText = message
        if let d = response as? [String: Any], let e = d["error"] as? String {
            alert.informativeText = e
        } else if response == nil {
            alert.informativeText = "Not connected to the Thurm daemon."
        }
        alert.alertStyle = .warning
        alert.runModal()
    }
}
