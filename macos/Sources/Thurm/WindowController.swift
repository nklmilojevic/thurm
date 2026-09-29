import AppKit
import QuartzCore

/// Small colored dot shown in the native tab (window.tab.accessoryView) for agent status:
/// Working = blue, Idle = gray, NeedsInput = orange, Done (unseen) = green.
final class AgentDotView: NSView {
    var status: AgentStatus? {
        didSet {
            if status != oldValue { refresh() }
        }
    }

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.cornerRadius = frameRect.width / 2
        translatesAutoresizingMaskIntoConstraints = false
        widthAnchor.constraint(equalToConstant: frameRect.width).isActive = true
        heightAnchor.constraint(equalToConstant: frameRect.height).isActive = true
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var intrinsicContentSize: NSSize { frame.size }

    private func refresh() {
        guard let layer = layer else { return }
        switch status {
        case .some(.working):
            layer.backgroundColor = NSColor.systemBlue.cgColor
            toolTip = "Agent working"
        case .some(.needsInput):
            layer.backgroundColor = NSColor.systemOrange.cgColor
            toolTip = "Agent needs input"
        case .some(.done):
            layer.backgroundColor = NSColor.systemGreen.cgColor
            toolTip = "Agent finished"
        case .some(.idle):
            layer.backgroundColor = NSColor.systemGray.cgColor
            toolTip = "Agent idle"
        case .none:
            layer.backgroundColor = NSColor.clear.cgColor
            toolTip = nil
        }
    }
}

/// "42s", "3m 05s", "1h 02m" (same as the daemon's notifications).
func formatDuration(_ ms: UInt64) -> String {
    let s = ms / 1000
    if s < 60 { return "\(s)s" }
    if s < 3600 { return String(format: "%dm %02ds", s / 60, s % 60) }
    return String(format: "%dh %02dm", s / 3600, (s % 3600) / 60)
}

/// One native tab = one NSWindow. Owns the tab's split tree (TabContentView).
final class TerminalWindowController: NSWindowController, NSWindowDelegate {
    let content: TabContentView

    /// User supplied title (layout `title`); nil follows the focused pane.
    var titleOverride: String? {
        didSet { updateTitle() }
    }

    private let dot = AgentDotView(frame: NSRect(x: 0, y: 0, width: 8, height: 8))
    /// "⌘1" … "⌘9" in the native tab, like Ghostty.
    private let shortcutHint: NSTextField = {
        let label = NSTextField(labelWithString: "")
        label.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        label.textColor = .tertiaryLabelColor
        return label
    }()
    private lazy var tabAccessory: NSStackView = {
        let stack = NSStackView(views: [dot, shortcutHint])
        stack.orientation = .horizontal
        stack.spacing = 5
        stack.alignment = .centerY
        // Sized by its content: an autoresizing 0×0 frame conflicts with the dot's fixed size,
        // and AppKit then breaks the tab bar's constraints (the selected tab fills the bar).
        stack.translatesAutoresizingMaskIntoConstraints = false
        stack.setHuggingPriority(.required, for: .horizontal)
        stack.setContentCompressionResistancePriority(.required, for: .horizontal)
        return stack
    }()
    /// Set when the window is closed because its panes are gone (no confirmation, no ClosePane).
    var closingWithoutConfirmation = false
    /// The workspace this tab belongs to (every tab of a window shows the same one).
    var workspaceID: UInt64 = 0
    /// A handoff tab (a remote agent working on a local repository): the handoff's id.
    var handoffID: String? {
        didSet { SessionManager.shared.refreshSidebars() }
    }
    /// The daemon of this tab's panes.
    var host: HostId { content.host }
    /// Set once the window closed; the controller is about to be released.
    var isClosed = false
    /// The quick terminal (see QuickTerminal.swift): one tab in a borderless panel, outside
    /// the tab groups, workspaces and saved windows.
    let isQuick: Bool

    init(root: SplitNode, focused: PaneKey, zoomed: PaneKey?, title: String?, contentSize: NSSize,
         quick: Bool = false) {
        content = TabContentView(root: root, focused: focused, zoomed: zoomed)
        isQuick = quick
        let rect = NSRect(origin: .zero, size: contentSize)
        // Tabs live in the titlebar, which is tinted with the theme (see TerminalWindow).
        let window: NSWindow = quick ? QuickTerminalWindow(contentRect: rect)
            : TerminalWindow(contentRect: rect, styleMask: [.titled, .closable, .miniaturizable, .resizable],
                             backing: .buffered, defer: false)
        super.init(window: window)
        titleOverride = title
        shouldCascadeWindows = false

        if quick {
            window.tabbingMode = .disallowed
        } else {
            window.tabbingMode = .preferred
            window.tabbingIdentifier = "Thurm"
            window.collectionBehavior.insert(.fullScreenPrimary)
        }
        window.isRestorable = false
        window.isReleasedWhenClosed = false
        window.minSize = NSSize(width: 240, height: 140)
        window.delegate = self
        content.controller = self
        content.frame = rect
        if quick { window.contentView = content }
        applyTabStyle()
        window.initialFirstResponder = content.focusedView
        // The safe area changes when the tab bar appears or on full screen.
        layoutObservation = window.observe(\.contentLayoutRect) { [weak self] _, _ in
            DispatchQueue.main.async { self?.content.needsLayout = true }
        }
        applyConfig()
        updateTitle()
    }

    private var layoutObservation: NSKeyValueObservation?

    /// The sidebar split when `tab_style = "sidebar"`, else nil (content is the window's view).
    private(set) var tabSplit: TabSplitViewController?
    var sidebar: TabSidebarViewController? { tabSplit?.sidebar }

    /// Native tab bar, or the vertical sidebar hosting `content`.
    func applyTabStyle() {
        guard let window = window as? TerminalWindow else { return }
        let cfg = SessionManager.shared.config
        if cfg.sidebarTabs {
            guard tabSplit == nil else { return }
            let frame = window.frame
            window.suppressTabBar = true
            // The glass sidebar runs under the titlebar; the terminal keeps to the safe area.
            window.styleMask.insert(.fullSizeContentView)
            window.titlebarAppearsTransparent = true
            content.removeFromSuperview()
            let split = TabSplitViewController(content: content, width: cfg.sidebarWidth)
            split.sidebar.controller = self
            window.contentViewController = split
            window.setFrame(frame, display: true)
            tabSplit = split
            split.sidebar.reload()
        } else {
            let hadSplit = tabSplit != nil
            let frame = window.frame
            if hadSplit {
                window.contentViewController = nil
                content.removeFromSuperview()
                tabSplit = nil
                window.styleMask.remove(.fullSizeContentView)
                window.titlebarAppearsTransparent = false
            }
            if window.contentView !== content {
                content.frame = window.contentLayoutRect
                window.contentView = content
            }
            if hadSplit { window.setFrame(frame, display: true) }
            window.suppressTabBar = false
        }
        content.needsLayout = true
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    var focusedPane: PaneKey { content.focusedPane }

    /// Background opacity: `window.opacity`, or the quick terminal's own.
    var opacity: CGFloat {
        let cfg = SessionManager.shared.config
        return isQuick ? cfg.quickOpacity ?? cfg.opacity : cfg.opacity
    }

    /// Window background / opacity from the config.
    func applyConfig() {
        guard let window = window else { return }
        let cfg = SessionManager.shared.config
        // Chrome follows the terminal theme, not the system, so glass over a dark theme is dark.
        window.appearance = NSAppearance(named: cfg.theme.isDark ? .darkAqua : .aqua)
        (window as? TerminalWindow)?.titlebarColor = colorFromRGB(cfg.theme.background, alpha: opacity)
        applyTabStyle()
        // A step off the terminal background (darker, like Catppuccin's mantle): solid, so it
        // doesn't pick up the desktop through the window.
        let base = colorFromRGB(cfg.theme.background, alpha: opacity)
        sidebar?.backgroundColor = base.shadow(withLevel: cfg.theme.isDark ? 0.18 : 0.05) ?? base
        // The quick terminal's rounded corners need a transparent window.
        if opacity < 1 || isQuick {
            window.isOpaque = false
            window.backgroundColor = colorFromRGB(cfg.theme.background, alpha: 0.001)
        } else {
            window.isOpaque = true
            window.backgroundColor = colorFromRGB(cfg.theme.background)
        }
        applyBlur(opacity < 1 && cfg.blur > 0)
        content.applyUpdateBadgeTheme()
        content.needsDisplay = true
    }

    /// Behind-window blur for translucent windows: a visual effect view at the back of the
    /// window's frame view, under the (translucent) terminal content.
    private var blurView: NSVisualEffectView?

    private func applyBlur(_ on: Bool) {
        guard let window = window, let frameView = window.contentView?.superview else { return }
        if !on {
            blurView?.removeFromSuperview()
            blurView = nil
            return
        }
        let v = blurView ?? NSVisualEffectView()
        v.material = .underWindowBackground
        v.blendingMode = .behindWindow
        v.state = .active
        v.frame = frameView.bounds
        v.autoresizingMask = [.width, .height]
        if v.superview !== frameView {
            frameView.addSubview(v, positioned: .below, relativeTo: frameView.subviews.first)
        }
        blurView = v
    }

    // MARK: Title and agent status

    func updateTitle() {
        guard let window = window else { return }
        let panes = SessionManager.shared.panes
        let info = panes[content.focusedPane]
        let base = titleOverride ?? info?.displayTitle ?? "Thurm"

        // Most urgent agent status among the tab's panes.
        var status: AgentStatus?
        var agentName: String?
        var detail: String?
        for id in content.paneIds {
            guard let agent = panes[id]?.agent else { continue }
            if status == nil || agent.status.urgency > status!.urgency {
                status = agent.status
                agentName = agent.name
                detail = agent.message
                if agent.status == .done, let ms = agent.turnMs {
                    detail = "finished in \(formatDuration(ms))"
                }
            }
        }
        var prefix = ""
        if let status = status {
            switch status {
            case .needsInput: prefix = "● "
            case .working: prefix = "◌ "
            case .done: prefix = "✓ "
            case .idle: prefix = ""
            }
        }
        // Remote tabs say where they run.
        let hostPrefix = host == localHost ? "" : "\(host) · "
        window.title = prefix + hostPrefix + base
        if let name = agentName, let status = status {
            window.tab.toolTip = "\(name): \(detail ?? status.rawValue)"
        } else {
            window.tab.toolTip = nil
        }
        dot.status = status
        dot.isHidden = status == nil
        let tabs = orderedTabs
        var hint = ""
        if tabs.count > 1, let i = tabs.firstIndex(of: window), let n = tabShortcut(index: i, count: tabs.count) {
            hint = "⌘\(n)"
        }
        shortcutHint.stringValue = hint
        shortcutHint.isHidden = hint.isEmpty
        let accessory = (status == nil && hint.isEmpty) ? nil : tabAccessory
        if window.tab.accessoryView !== accessory { window.tab.accessoryView = accessory }
        SessionManager.shared.refreshSidebars()
    }

    /// Called by the content view when the focused pane changes.
    func focusedPaneChanged() {
        updateTitle()
        SessionManager.shared.focusChanged()
        SessionManager.shared.scheduleLayoutSave()
    }

    // MARK: NSWindowDelegate

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        let manager = SessionManager.shared
        if manager.isTerminating || closingWithoutConfirmation { return true }
        let busy = content.paneIds.compactMap { manager.panes[$0] }.filter { $0.hasRunningProcess }
        if manager.config.confirmClose && !busy.isEmpty {
            let names = busy.compactMap { $0.foregroundName }.joined(separator: ", ")
            let alert = NSAlert()
            alert.messageText = "Close this tab?"
            alert.informativeText = "Processes are still running: \(names). Closing the tab terminates them."
            alert.alertStyle = .warning
            alert.addButton(withTitle: "Close")
            alert.addButton(withTitle: "Cancel")
            guard alert.runModal() == .alertFirstButtonReturn else { return false }
        }
        for id in content.paneIds {
            manager.sendClosePane(id)
        }
        if let handoff = handoffID {
            // After the window is gone: offer to remove the worktree on the host.
            DispatchQueue.main.async { manager.handoffTabClosed(handoff) }
        }
        return true
    }

    func windowDidEndLiveResize(_ notification: Notification) {
        if isQuick { QuickTerminal.shared.userResized() }
    }

    func windowWillClose(_ notification: Notification) {
        content.detachAll()
        SessionManager.shared.controllerDidClose(self)
    }

    func windowDidBecomeKey(_ notification: Notification) {
        SessionManager.shared.controllerBecameKey(self)
        content.focusedView?.reportFocus()
    }

    func windowDidResignKey(_ notification: Notification) {
        content.focusedView?.reportFocus()
        SessionManager.shared.focusChanged()
        if isQuick { QuickTerminal.shared.didResignKey() }
    }

    func windowDidMove(_ notification: Notification) {
        SessionManager.shared.scheduleLayoutSave()
    }

    func windowDidResize(_ notification: Notification) {
        SessionManager.shared.scheduleLayoutSave()
    }

    func windowDidEnterFullScreen(_ notification: Notification) {
        SessionManager.shared.scheduleLayoutSave()
    }

    func windowDidExitFullScreen(_ notification: Notification) {
        SessionManager.shared.scheduleLayoutSave()
    }

    func windowDidChangeBackingProperties(_ notification: Notification) {
        for view in content.views.values { view.metricsChanged() }
    }

    // MARK: Actions (reached through the responder chain)

    @objc func splitRight(_ sender: Any?) {
        SessionManager.shared.splitFocused(in: self, direction: .right)
    }

    @objc func splitDown(_ sender: Any?) {
        SessionManager.shared.splitFocused(in: self, direction: .down)
    }

    @objc func closePane(_ sender: Any?) {
        SessionManager.shared.userClosePane(content.focusedPane)
    }

    @objc func toggleZoom(_ sender: Any?) {
        content.toggleZoom()
    }

    @objc func equalizeSplits(_ sender: Any?) {
        content.equalize()
    }

    @objc func focusLeft(_ sender: Any?) { content.moveFocus(.left) }
    @objc func focusRight(_ sender: Any?) { content.moveFocus(.right) }
    @objc func focusUp(_ sender: Any?) { content.moveFocus(.up) }
    @objc func focusDown(_ sender: Any?) { content.moveFocus(.down) }

    @objc func resizeLeft(_ sender: Any?) { content.resizeFocused(axis: .horizontal, delta: -0.05) }
    @objc func resizeRight(_ sender: Any?) { content.resizeFocused(axis: .horizontal, delta: 0.05) }
    @objc func resizeUp(_ sender: Any?) { content.resizeFocused(axis: .vertical, delta: -0.05) }
    @objc func resizeDown(_ sender: Any?) { content.resizeFocused(axis: .vertical, delta: 0.05) }

    @objc func showFind(_ sender: Any?) {
        content.showFindBar()
    }

    @objc func findNext(_ sender: Any?) {
        content.showFindBar()
        content.findBar?.findNext(sender)
    }

    @objc func findPrevious(_ sender: Any?) {
        content.showFindBar()
        content.findBar?.findPrevious(sender)
    }

    /// This window's tabs in the order they're shown, which Cmd+1…9 follow: the sidebar's (by
    /// repository, see `TabSidebarViewController.displayOrder`) or the native tab bar's.
    var orderedTabs: [NSWindow] {
        guard let window else { return [] }
        let native = window.tabGroup?.windows ?? [window]
        return SessionManager.shared.config.sidebarTabs ? TabSidebarViewController.displayOrder(native) : native
    }

    /// Cmd+1...9 (tag = number; 9 selects the last tab).
    @objc func selectTabByNumber(_ sender: Any?) {
        guard let item = sender as? NSMenuItem, window != nil else { return }
        let windows = orderedTabs
        guard !windows.isEmpty else { return }
        let index = item.tag >= 9 ? windows.count - 1 : item.tag - 1
        guard index >= 0, index < windows.count else { return }
        windows[index].makeKeyAndOrderFront(nil)
    }

    /// The "+" button of the native tab bar.
    @objc override func newWindowForTab(_ sender: Any?) {
        SessionManager.shared.newTab(from: self)
    }
}
