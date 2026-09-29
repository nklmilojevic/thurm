import AppKit
import CThurm

/// Owns the connection lifecycle, every window/tab/split, pane metadata and layout
/// persistence. All methods run on the main thread.
final class SessionManager: NSObject, CoreDelegate {
    static let shared = SessionManager()

    var config = AppConfig()
    /// Cmd+Plus / Cmd+Minus override of `config.fontSize` (not persisted).
    private(set) var fontSizeOverride: CGFloat?
    var effectiveFontSize: CGFloat { fontSizeOverride ?? config.fontSize }

    private(set) var controllers: [TerminalWindowController] = []
    /// Every workspace (see Workspaces.swift).
    var workspaces: [Workspace] = []
    /// Every pane of every connected daemon (a disconnected host's keep their last state).
    var panes: [PaneKey: PaneInfo] = [:]
    /// Panes waiting for input that the user has looked at since they started waiting; they
    /// don't count toward the Dock badge until the agent asks again.
    private var seenWaiting: Set<PaneKey> = []
    private weak var lastKeyController: TerminalWindowController?
    private weak var lastRegularKeyController: TerminalWindowController?

    private(set) var isTerminating = false
    /// Layout saving starts only after the initial restore, so a failed start never
    /// overwrites the stored layout with an empty one.
    private var sessionReady = false
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
        // Safety net only: tab changes save through the tab group's KVO.
        periodicTimer = Timer.scheduledTimer(timeInterval: 15, target: self, selector: #selector(periodicSave),
                                             userInfo: nil, repeats: true)
        if let err = config.loadError {
            currentController?.content.focusedView?.showToast("Config error: \(err)", duration: 8)
        } else if let err = QuickTerminal.shared.configChanged() {
            currentController?.content.focusedView?.showToast(err, duration: 8)
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
            // Compatible, just older: carry on with it.
            guard Core.shared.connect() else { return false }
            Core.shared.request(object: ["Hello": ["client": "Thurm.app", "version": thurm_protocol_version(), "ui": true]])
        }
        sendAppearance()
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
        Core.shared.lastError = "The running thurmd refused the connection: \(e). Quit Thurm and run `thurm daemon stop` (layout and scrollback are restored), then open Thurm again."
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

    /// Writes `colors.theme` through the daemon, which reloads every client.
    @discardableResult
    func setTheme(_ spec: String) -> Bool {
        let resp = Core.shared.request(object: ["SetTheme": ["spec": spec]])
        if let d = resp as? [String: Any], let e = d["error"] as? String {
            currentController?.content.focusedView?.showToast("Theme: \(e)", duration: 6)
            return false
        }
        return resp != nil
    }

    // MARK: Theme picker

    /// Shows `name` in every window without saving it (nil: back to the configured theme).
    /// Only colors change, so this is cheap enough to follow the arrow keys.
    func previewTheme(_ name: String?) {
        guard let json = Core.shared.previewTheme(name),
              let t = JSON.decode(json) as? [String: Any] else { return }
        config.theme = Theme(json: t)
        for c in liveControllers {
            c.applyConfig()
            for view in c.content.views.values { view.needsRender = true }
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
        guard let v = JSON.variant(Core.shared.request("\"ListPanes\"", host: host)), v.name == "Panes",
              let list = v.payload as? [Any]
        else { return nil }
        return list.compactMap { PaneInfo(json: $0, host: host) }
    }

    func fetchPaneInfo(_ key: PaneKey) -> PaneInfo? {
        guard let v = JSON.variant(Core.shared.request(object: ["PaneInfo": ["pane": key.number]], host: key.host)),
              v.name == "PaneInfo"
        else { return nil }
        return PaneInfo(json: v.payload, host: key.host)
    }

    func fetchLayout() -> Layout? {
        guard let v = JSON.variant(Core.shared.request("\"GetLayout\"")), v.name == "Layout",
              let json = v.payload as? String
        else { return nil }
        return Layout.from(json: json)
    }

    /// Creates a pane in `host`'s daemon (default: the host of the pane it inherits from).
    /// Returns its key.
    func createPane(cols: Int, rows: Int, inheritFrom: PaneKey?, preset: String? = nil,
                    forkFrom: PaneKey? = nil, host: HostId? = nil, cwd: String? = nil) -> PaneKey? {
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
            "command": NSNull(),
            "cwd": orNull(cwd),
            "env": [Any](),
            "size": paneSizeObject(cols: cols, rows: rows),
            "agent_preset": orNull(preset),
            "inherit_cwd_from": orNull(inherit?.number),
            "hold": false,
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

    private func paneSizeObject(cols: Int, rows: Int) -> [String: Any] {
        let scale = NSScreen.main?.backingScaleFactor ?? 2
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
        let scale = NSScreen.main?.backingScaleFactor ?? 2
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
        let infos = fetchPanes() ?? []
        panes = panes.filter { $0.key.isRemote }
        for info in infos { panes[info.key] = info }
        let alive = Set(infos.filter { $0.alive }.map { $0.key })
        // Remote tabs come back as they were and attach once their host connects (tabs of a
        // host no longer configured go).
        let remotes = Set(config.remoteNames)

        // A pane shows in one place. Skip the ones a window already shows, and the ones an
        // earlier window of the layout claimed (a layout saved after a double restore lists
        // them twice).
        var placed = Set(liveControllers.flatMap { $0.content.paneIds })
        var layout = fetchLayout() ?? Layout(windows: [])
        layout.retainPanes { key in
            !placed.contains(key) && (key.isRemote ? remotes.contains(key.host) : alive.contains(key))
        }
        workspaces = layout.workspaces.map { Workspace(layout: $0) }
        if let quick = layout.quick {
            QuickTerminal.shared.restore(quick)
            placed.formUnion(quick.root.panes)
        }

        // Hidden workspaces' panes are in use too (they run until a window shows them), and
        // the quick terminal's.
        var used = Set(layout.workspaces.flatMap { $0.tabs.flatMap { $0.root.panes } })
        used.formUnion(layout.quick?.root.panes ?? [])
        for var w in layout.windows {
            w.tabs = w.tabs.filter { tab in tab.root.panes.allSatisfy { !placed.contains($0) } }
            placed.formUnion(w.tabs.flatMap { $0.root.panes })
            restoreWindow(w)
            used.formUnion(w.tabs.flatMap { $0.root.panes })
        }

        // Panes that exist but are not in the layout: tabs of one extra window.
        let orphans = infos.filter { $0.alive && !used.contains($0.key) }.map { $0.key }
            .sorted { $0.id < $1.id }
        var first: TerminalWindowController?
        var previous: TerminalWindowController?
        let orphanWorkspace = orphans.isEmpty ? nil : makeWorkspace()
        for id in orphans {
            let c = makeController(root: .leaf(id), focused: id, zoomed: nil, title: nil, frame: nil)
            c.workspaceID = orphanWorkspace?.id ?? 0
            if let prev = previous {
                attachAsTab(c, to: prev)
            } else {
                showAsNewWindow(c, frame: nil)
                first = c
            }
            previous = c
        }
        first?.window?.makeKeyAndOrderFront(nil)

        if regularControllers.isEmpty {
            if let ws = workspacesByRecency.first(where: { !$0.hiddenTabs.isEmpty }) {
                openWorkspaceInNewWindow(ws)
            } else {
                newWindow()
            }
        }
        sessionReady = true
        saveLayoutNow(force: true)
        focusChanged()
    }

    private func restoreWindow(_ w: WindowLayout) {
        var frame: NSRect?
        if let f = w.frame, f.count == 4, f[2] > 50, f[3] > 50 {
            frame = clampToScreens(NSRect(x: f[0], y: f[1], width: f[2], height: f[3]))
        }
        var created: [TerminalWindowController] = []
        // Older layouts have no workspaces, and two windows can't show the same one.
        var ws = workspace(w.workspace)
        if ws == nil || (ws.map { isShown($0) } ?? false) {
            ws = makeWorkspace(host: w.tabs.first?.focusedKey.host ?? localHost)
        }
        for tab in w.tabs {
            let c = makeController(root: SplitNode(layout: tab.root), focused: tab.focusedKey, zoomed: tab.zoomedKey,
                                   title: tab.title, frame: frame)
            c.workspaceID = ws?.id ?? 0
            c.handoffID = tab.handoff
            if let prev = created.last {
                attachAsTab(c, to: prev)
            } else {
                showAsNewWindow(c, frame: frame)
            }
            created.append(c)
        }
        guard !created.isEmpty else { return }
        let selected = created[min(max(0, w.selectedTab), created.count - 1)]
        selected.window?.makeKeyAndOrderFront(nil)
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

    func controller(for pane: PaneKey) -> TerminalWindowController? {
        liveControllers.first { $0.content.root.contains(pane) }
    }

    func view(for pane: PaneKey) -> TerminalView? {
        controller(for: pane)?.content.views[pane]
    }

    /// Every view of `host`'s panes.
    func views(of host: HostId) -> [TerminalView] {
        liveControllers.flatMap { $0.content.views.values.filter { $0.pane.host == host } }
    }

    func makeController(root: SplitNode, focused: PaneKey, zoomed: PaneKey?, title: String?,
                        frame: NSRect?, quick: Bool = false) -> TerminalWindowController {
        let c = TerminalWindowController(root: root, focused: focused, zoomed: zoomed, title: title,
                                         contentSize: defaultContentSize(), quick: quick)
        if let frame = frame {
            c.window?.setFrame(frame, display: false)
        }
        controllers.append(c)
        return c
    }

    func attachAsTab(_ c: TerminalWindowController, to host: TerminalWindowController) {
        c.workspaceID = host.workspaceID
        guard let hostWindow = host.window, let w = c.window else { return }
        w.setFrame(hostWindow.frame, display: false)
        hostWindow.addTabbedWindow(w, ordered: .above)
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

    /// Cmd+N: a new window with a new shell (or an existing pane), showing a new workspace.
    func newWindow(pane existing: PaneKey? = nil, preset: String? = nil, workspace ws: Workspace? = nil) {
        let host = existing?.host ?? ws?.host ?? localHost
        var id = existing
        if id == nil {
            let size = gridSize(forPoints: defaultContentSize())
            id = createPane(cols: size.cols, rows: size.rows, inheritFrom: nil, preset: preset, host: host)
        }
        guard let pane = id else { return }
        let c = makeController(root: .leaf(pane), focused: pane, zoomed: nil, title: nil, frame: nil)
        c.workspaceID = (ws ?? makeWorkspace(host: host)).id
        showAsNewWindow(c, frame: nil)
        scheduleLayoutSave()
    }

    /// Cmd+T: a new tab next to `host`, inheriting the focused pane's cwd. The pane runs on
    /// the daemon of `host`'s workspace (a remote workspace's tabs are that host's panes).
    @discardableResult
    func newTab(from hostIn: TerminalWindowController?, pane existing: PaneKey? = nil, preset: String? = nil,
                cwd: String? = nil, host daemonIn: HostId? = nil) -> TerminalWindowController? {
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
        let c = makeController(root: .leaf(pane), focused: pane, zoomed: nil, title: nil, frame: nil)
        let sameHost = host.map { (workspace($0.workspaceID)?.host ?? $0.host) == pane.host } ?? false
        if let host = host, let hostWindow = host.window, hostWindow.isVisible, sameHost {
            attachAsTab(c, to: host)
            c.window?.makeKeyAndOrderFront(nil)
        } else if let shown = workspaces.first(where: { $0.host == pane.host && isShown($0) }),
                  let other = controllerShowing(shown) {
            // Another window shows this host's workspace: the tab goes there.
            attachAsTab(c, to: other)
            c.window?.makeKeyAndOrderFront(nil)
        } else {
            c.workspaceID = (workspaces.first { $0.host == pane.host && !isShown($0) && pane.isRemote }
                ?? makeWorkspace(host: pane.host)).id
            if let ws = workspace(c.workspaceID), !ws.hiddenTabs.isEmpty {
                // Its hidden tabs come along.
                let frame = currentRegularController?.window?.frame
                showAsNewWindow(c, frame: frame)
                for tab in ws.hiddenTabs {
                    let t = makeController(root: SplitNode(layout: tab.root), focused: tab.focusedKey,
                                           zoomed: tab.zoomedKey, title: tab.title, frame: nil)
                    t.handoffID = tab.handoff
                    attachAsTab(t, to: c)
                }
                ws.hiddenTabs = []
                c.window?.makeKeyAndOrderFront(nil)
            } else {
                showAsNewWindow(c, frame: nil)
            }
        }
        scheduleLayoutSave()
        return c
    }

    /// Cmd+D / Cmd+Shift+D.
    func splitFocused(in c: TerminalWindowController, direction: SplitDirName, preset: String? = nil,
                      fork: Bool = false) {
        let target = c.focusedPane
        var size = gridSize(forPoints: c.content.focusedView?.frame.size ?? c.content.bounds.size)
        if direction == .right || direction == .left {
            size.cols = max(2, size.cols / 2)
        } else {
            size.rows = max(1, size.rows / 2)
        }
        guard let id = createPane(cols: size.cols, rows: size.rows, inheritFrom: target, preset: preset,
                                  forkFrom: fork ? target : nil) else { return }
        c.content.split(target: target, newPane: id, direction: direction)
        scheduleLayoutSave()
    }

    /// Cmd+W: close the focused pane (confirming when a program is running).
    func userClosePane(_ id: PaneKey) {
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
        if Core.shared.isConnected(id.host) {
            Core.shared.send(object: ["ClosePane": ["pane": id.number]], host: id.host)
        } else {
            // Its host is offline: close it there once it is back.
            Remotes.shared.pendingCloses[id.host, default: []].insert(id.id)
        }
        panes.removeValue(forKey: id)
    }

    /// Removes a pane's view; closes the tab when it was the last pane.
    func removePaneFromUI(_ id: PaneKey) {
        guard let c = controller(for: id) else { return }
        let empty = c.content.remove(pane: id)
        if empty {
            c.closingWithoutConfirmation = true
            c.window?.close()
        } else {
            c.updateTitle()
        }
        focusChanged()
        scheduleLayoutSave()
    }

    func controllerDidClose(_ c: TerminalWindowController) {
        c.isClosed = true
        if c.isQuick { QuickTerminal.shared.controllerClosed(c) }
        refreshSidebars()
        if lastKeyController === c { lastKeyController = nil }
        // Drop our reference after AppKit is done with the window delegate callbacks.
        DispatchQueue.main.async {
            SessionManager.shared.controllers.removeAll { $0 === c }
        }
        if !isTerminating {
            scheduleLayoutSave()
            focusChanged()
        }
    }

    func controllerBecameKey(_ c: TerminalWindowController) {
        lastKeyController = c
        if !c.isQuick { lastRegularKeyController = c }
        workspace(c.workspaceID)?.lastActive = Date()
        focusChanged()
        scheduleLayoutSave()
    }

    /// Focuses a pane, bringing its tab to the front.
    func focusPane(_ id: PaneKey, activate: Bool) {
        guard let c = controller(for: id) else { return }
        if c.isQuick {
            QuickTerminal.shared.show()
        } else {
            c.window?.makeKeyAndOrderFront(nil)
        }
        c.content.focus(id)
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
        updateDockBadge()
        refreshSidebars()
    }

    private var sidebarRefreshPending = false

    /// Reload every vertical tab sidebar on the next main-loop turn (coalesced).
    func refreshSidebars() {
        guard config.sidebarTabs, !sidebarRefreshPending else { return }
        sidebarRefreshPending = true
        DispatchQueue.main.async {
            self.sidebarRefreshPending = false
            for c in self.liveControllers { c.sidebar?.reload() }
        }
    }

    /// View > Tabs in Sidebar: switch `window.tab_style` (persisted; every window follows).
    func toggleSidebarTabs() {
        let next = config.sidebarTabs ? "native" : "sidebar"
        let resp = Core.shared.request(object: ["SetSetting": ["key": "window.tab_style", "value": "\"\(next)\""]])
        if let d = resp as? [String: Any], let e = d["error"] as? String {
            currentController?.content.focusedView?.showToast("Tabs: \(e)", duration: 6)
        }
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
        for c in liveControllers {
            for (id, view) in c.content.views {
                view.setLockBadgeVisible(show && c === key && id == c.content.focusedPane)
            }
        }
    }

    func updateDockBadge() {
        // On screen in the active window's tab counts as read.
        if NSApp.isActive, let c = NSApp.keyWindow?.windowController as? TerminalWindowController {
            for id in c.content.visiblePaneIds where panes[id]?.agent?.status == .needsInput {
                seenWaiting.insert(id)
            }
        }
        seenWaiting = seenWaiting.filter { panes[$0]?.agent?.status == .needsInput }
        let count = panes.values.filter { info in
            info.agent?.status == .needsInput && !seenWaiting.contains(info.key)
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

    // MARK: - Config and fonts

    func reloadConfig(notifyDaemon: Bool) {
        if notifyDaemon {
            Core.shared.request("\"ReloadConfig\"")
        }
        config = AppConfig.load()
        applyConfigToUI()
        Core.shared.reloadRemoteEngines()
        Remotes.shared.configChanged()
        Updater.shared.configChanged()
        let quickError = QuickTerminal.shared.configChanged()
        let view = currentController?.content.focusedView
        if let err = config.loadError {
            view?.showToast("Config error: \(err)", duration: 8)
        } else if let err = quickError {
            view?.showToast(err, duration: 8)
        } else if notifyDaemon {
            view?.showToast("Configuration reloaded")
        }
    }

    private func applyConfigToUI() {
        FontShaper.invalidateAll()
        for c in liveControllers {
            c.applyConfig()
            for view in c.content.views.values {
                view.metricsChanged()
            }
            c.content.needsDisplay = true
        }
        updateSecureInput()
    }

    func changeFontSize(by delta: CGFloat) {
        fontSizeOverride = max(6, min(72, effectiveFontSize + delta))
        applyConfigToUI()
    }

    func resetFontSize() {
        fontSizeOverride = nil
        applyConfigToUI()
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
            let tabs = g.map { $0.content.tabLayout(title: $0.titleOverride) }
            let selected = g.firstIndex { $0.window === selectedWindow } ?? 0
            let f = selectedWindow.frame
            windows.append(WindowLayout(frame: [Double(f.minX), Double(f.minY), Double(f.width), Double(f.height)],
                                        tabs: tabs, selectedTab: selected,
                                        fullscreen: selectedWindow.styleMask.contains(.fullScreen),
                                        workspace: g[0].workspaceID))
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
            Core.shared.request(object: request)
        } else {
            Core.shared.send(object: request)
        }
    }

    /// Debounced (500 ms) layout save.
    func scheduleLayoutSave() {
        guard !isTerminating else { return }
        NSObject.cancelPreviousPerformRequests(withTarget: self, selector: #selector(debouncedSave), object: nil)
        perform(#selector(debouncedSave), with: nil, afterDelay: 0.5)
    }

    @objc private func debouncedSave() {
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
            Core.shared.request(object: ["Shutdown": ["kill_panes": true]])
        }
        for c in liveControllers {
            c.content.detachAll()
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
        tlog("lost connection to thurmd; reconnecting")
        Core.shared.disconnect()
        reconnectAttempt = 0
        currentController?.content.focusedView?.showToast("Daemon connection lost, reconnecting…", duration: 3)
        attemptReconnect()
    }

    @objc private func attemptReconnect() {
        guard !isTerminating, !Core.shared.isConnected else { return }
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
        let infos = fetchPanes() ?? []
        panes = panes.filter { $0.key.isRemote }
        for info in infos { panes[info.key] = info }
        for c in liveControllers {
            for id in c.content.paneIds where !id.isRemote && panes[id] == nil {
                removePaneFromUI(id)
            }
        }
        let live = liveControllers
        if live.isEmpty {
            sessionReady = false
            restoreSession()
            return
        }
        for c in live {
            for view in c.content.views.values where !view.pane.isRemote {
                view.resubscribe()
            }
            c.updateTitle()
        }
        currentController?.content.focusedView?.showToast("Reconnected")
        saveLayoutNow(force: true)
        focusChanged()
    }

    // MARK: - Events

    func coreDidReceiveEvent(_ name: String, payload: Any?, host: HostId) {
        guard !isTerminating else { return }
        let dict = payload as? [String: Any]
        let key = jsonUInt64(dict?["pane"]).map { PaneKey(host, $0) }
        switch name {
        case "PaneInfo":
            if let info = PaneInfo(json: payload, host: host) { paneInfoUpdated(info) }
        case "PaneExited", "PaneClosed":
            if let id = key {
                panes.removeValue(forKey: id)
                removePaneFromUI(id)
                updateDockBadge()
            }
        case "Bell":
            if let id = key {
                view(for: id)?.flash()
                if !NSApp.isActive || !isPaneFocused(id) {
                    NSApp.requestUserAttention(.informationalRequest)
                }
            }
        case "Notify":
            if let id = key {
                var title = jsonString(dict?["title"]) ?? "Thurm"
                if id.isRemote { title = "\(host) · \(title)" }
                let body = jsonString(dict?["body"]) ?? ""
                if config.notificationsEnabled && (!NSApp.isActive || !isPaneFocused(id)) {
                    Notifications.shared.post(pane: id, title: title, body: body)
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
        case "ClipboardRequest":
            if let id = key { Remotes.shared.clipboardRequest(id) }
        case "Ui":
            handleUi(payload, host: host)
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
        panes[key] = info
        if let c = controller(for: key) {
            c.updateTitle()
            c.content.views[key]?.maybeShowRestoredToast()
            if old?.progress != info.progress { c.content.views[key]?.setProgress(info.progress) }
        }
        // A handoff's agent finished or asks for something: bring its commits back.
        if let status = info.agent?.status, status == .done || status == .needsInput,
           old?.agent?.status != status {
            Remotes.shared.agentSettled(key)
        }
        let wasWaiting = old?.agent?.status == .needsInput
        let waiting = info.agent?.status == .needsInput
        // A new request (or a different one) is unread again.
        if !waiting || !wasWaiting || old?.agent?.message != info.agent?.message {
            seenWaiting.remove(key)
        }
        if waiting && !wasWaiting && !isPaneFocused(key) {
            NSApp.requestUserAttention(.informationalRequest)
        }
        updateDockBadge()
        // The sidebar's agent list covers every pane, including hidden workspaces'.
        if old?.agent != info.agent { refreshSidebars() }
        if old?.passwordInput != info.passwordInput {
            updateSecureInput()
        }
    }

    /// Commands sent by the `thurm` CLI through a daemon (`host`'s: its pane ids).
    private func handleUi(_ payload: Any?, host daemon: HostId) {
        guard let v = JSON.variant(payload), let d = v.payload as? [String: Any] else { return }
        let key = { (field: String) in jsonUInt64(d[field]).map { PaneKey(daemon, $0) } }
        switch v.name {
        case "NewTab":
            guard let pane = key("pane") else { return }
            if controller(for: pane) != nil {
                focusPane(pane, activate: true)
                return
            }
            if panes[pane] == nil, let info = fetchPaneInfo(pane) { panes[pane] = info }
            if jsonBool(d["new_window"]) ?? false {
                newWindow(pane: pane)
            } else {
                let c = newTab(from: currentController, pane: pane)
                // `thurm handoff` opened it: tag and group it.
                if let h = Remotes.shared.handoff(host: daemon, pane: pane.id) { c?.handoffID = h.id }
            }
            NSApp.activate()
        case "Split":
            guard let pane = key("pane") else { return }
            if controller(for: pane) != nil {
                focusPane(pane, activate: true)
                return
            }
            if panes[pane] == nil, let info = fetchPaneInfo(pane) { panes[pane] = info }
            let dir = SplitDirName(rawValue: jsonString(d["dir"]) ?? "right") ?? .right
            let target = key("target")
            let host = target.flatMap { controller(for: $0) }
                ?? (currentController?.host == daemon ? currentController : nil)
            if let host = host {
                host.content.split(target: target, newPane: pane, direction: dir)
                scheduleLayoutSave()
            } else {
                newWindow(pane: pane)
            }
        case "Focus":
            if let pane = key("pane") { focusPane(pane, activate: true) }
        case "Scroll":
            // The viewport lives in the app's copy of the terminal.
            if let pane = key("pane"), let scroll = d["scroll"] {
                Core.shared.send(object: ["Scroll": ["pane": pane.number, "scroll": scroll]], host: daemon)
            }
        case "SetTabTitle":
            if let pane = key("pane"), let c = controller(for: pane) {
                let title = jsonString(d["title"])
                c.titleOverride = (title?.isEmpty ?? true) ? nil : title
                scheduleLayoutSave()
            }
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
