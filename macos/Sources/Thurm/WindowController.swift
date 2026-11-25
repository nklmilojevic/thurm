import AppKit
import QuartzCore

/// Small colored dot shown in the native tab (window.tab.accessoryView) for agent status:
/// Working = blue, Idle = gray, NeedsInput = orange, Done (unseen) = green, Error (unseen) = red.
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

    /// Repaints in the current theme's status colors, after a config reload.
    func applyTheme() {
        refresh()
    }

    private func refresh() {
        guard let layer = layer else { return }
        layer.backgroundColor = status?.color.cgColor ?? NSColor.clear.cgColor
        switch status {
        case .some(.working): toolTip = "Agent working"
        case .some(.needsInput): toolTip = "Agent needs input"
        case .some(.done): toolTip = "Agent finished"
        case .some(.error): toolTip = "Agent failed"
        case .some(.idle): toolTip = "Agent idle"
        case .none: toolTip = nil
        }
    }
}

/// One tab's split tree and metadata, independent of any NSWindow.
///
/// In sidebar mode (`config.sidebarTabs == true`), a single regular `TerminalWindowController`
/// hosts all tabs of the active workspace as `TabItem`s and swaps the selected tab's
/// `TabContentView` inside `TabSplitViewController` in place — eliminating per-tab `NSWindow`,
/// `NSToolbar`, and `TabSidebarViewController` churn on tab and workspace switches. Background
/// workspaces cache their live `[TabItem]`s as well.
///
/// In native-tab mode (`config.sidebarTabs == false`) and for the quick terminal (`isQuick`),
/// each `TerminalWindowController` hosts a single `TabItem`.
final class TabItem {
    private static var nextDragID: UInt64 = 1

    /// Stable process-local identifier used for sidebar drag-and-drop.
    let dragID: UInt64
    let content: TabContentView
    /// The window currently hosting this tab (nil while parked in a background workspace or closed).
    weak var windowController: TerminalWindowController?
    /// Set once this tab has closed and its views have been detached.
    var isClosed = false
    /// `SessionManager.configGeneration` last applied to `content`.
    var appliedConfigGeneration: UInt64

    /// User-supplied title (layout `title`); nil follows the focused pane.
    var titleOverride: String? {
        didSet {
            guard titleOverride != oldValue else { return }
            if let c = windowController {
                if c.selectedTab === self {
                    c.updateTitle()
                } else {
                    SessionManager.shared.refreshSidebars()
                }
            }
        }
    }

    /// A handoff tab (a remote agent working on a local repository): the handoff's id.
    var handoffID: String? {
        didSet {
            if handoffID != oldValue { SessionManager.shared.refreshSidebars() }
        }
    }

    /// The agent wall (see AgentWall.swift): the tabs its agents came from.
    var wall: [WallOrigin]? {
        didSet { SessionManager.shared.refreshSidebars() }
    }

    var pinned = false {
        didSet {
            if pinned != oldValue { SessionManager.shared.refreshSidebars() }
        }
    }

    var focusedPane: PaneKey { content.focusedPane }
    var host: HostId { content.host }
    var workspaceID: UInt64 { windowController?.workspaceID ?? 0 }
    var isQuick: Bool { windowController?.isQuick ?? false }
    var isSelected: Bool { windowController?.selectedTab === self }

    init(content: TabContentView, title: String? = nil) {
        dragID = Self.nextDragID
        Self.nextDragID &+= 1
        self.content = content
        self.titleOverride = title
        self.appliedConfigGeneration = SessionManager.shared.configGeneration
        content.tab = self
    }

    convenience init(root: SplitNode, focused: PaneKey, zoomed: PaneKey?,
                     floating: [FloatingPane] = [], floatingHidden: Bool = false,
                     title: String? = nil) {
        let view = TabContentView(root: root, focused: focused, zoomed: zoomed,
                                  floating: floating, floatingHidden: floatingHidden)
        self.init(content: view, title: title)
    }

    convenience init(layout tab: TabLayout) {
        self.init(root: SplitNode(layout: tab.root), focused: tab.focusedKey, zoomed: tab.zoomedKey,
                  floating: tab.floating ?? [], floatingHidden: tab.floatingHidden ?? false,
                  title: tab.title)
        adopt(tab)
    }

    func adopt(_ tab: TabLayout) {
        handoffID = tab.handoff
        wall = tab.wall
        pinned = tab.pinned ?? false
    }

    func tabLayout() -> TabLayout {
        content.tabLayout(title: titleOverride, handoff: handoffID, wall: wall, pinned: pinned)
    }

    func displayTitle(panes: [PaneKey: PaneInfo] = SessionManager.shared.panes) -> String {
        let info = panes[focusedPane]
        let base = titleOverride ?? info?.displayTitle ?? "Thurm"
        return host == localHost ? base : "\(host) · \(base)"
    }

    /// Reapplies theme/font metrics if the config changed while this tab was parked or unselected.
    func applyConfigIfNeeded() {
        let gen = SessionManager.shared.configGeneration
        guard appliedConfigGeneration != gen else { return }
        appliedConfigGeneration = gen
        content.applyConfig()
        for view in content.views.values { view.metricsChanged() }
        content.needsDisplay = true
    }
}

/// Hosts one NSWindow (and one TabSplitViewController / TabSidebarViewController in sidebar mode).
///
/// - In sidebar mode (`config.sidebarTabs == true`, regular window): hosts 1..N `TabItem`s of the
///   active workspace and swaps `selectedTab.content` in place.
/// - In native mode (`config.sidebarTabs == false`) and the quick terminal: hosts 1 `TabItem`.
final class TerminalWindowController: NSWindowController, NSWindowDelegate {
    /// The tabs hosted by this window, in tab order. Never empty while the window is alive.
    private(set) var tabs: [TabItem]
    private(set) var selectedTabIndex: Int = 0

    var selectedTab: TabItem { tabs[selectedTabIndex] }
    var content: TabContentView { selectedTab.content }

    /// User supplied title (layout `title`) of the selected tab; nil follows the focused pane.
    var titleOverride: String? {
        get { selectedTab.titleOverride }
        set { selectedTab.titleOverride = newValue }
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
    /// Set when this window's tabs were transferred to another window (e.g. native -> sidebar)
    /// so `windowWillClose` leaves their `TabContentView`s alive and attached elsewhere.
    private var tabsTransferred = false
    /// Deferred sidebar match in native-tab mode: applied lazily if this hidden tab is later shown.
    weak var pendingSidebarMatch: TabSplitViewController?
    /// The workspace this window shows (every hosted tab belongs to it).
    var workspaceID: UInt64 = 0
    /// A handoff tab (a remote agent working on a local repository): the selected tab's handoff id.
    var handoffID: String? {
        get { selectedTab.handoffID }
        set { selectedTab.handoffID = newValue }
    }
    /// The agent wall (see AgentWall.swift): the selected tab's wall origins.
    var wall: [WallOrigin]? {
        get { selectedTab.wall }
        set { selectedTab.wall = newValue }
    }
    var pinned: Bool {
        get { selectedTab.pinned }
        set { selectedTab.pinned = newValue }
    }
    /// The daemon of the selected tab's panes.
    var host: HostId { selectedTab.host }
    /// Set once the window closed; the controller is about to be released.
    var isClosed = false
    /// The quick terminal (see QuickTerminal.swift): one tab in a borderless panel, outside
    /// the tab groups, workspaces and saved windows.
    let isQuick: Bool

    /// True while this regular window hosts tabs in-process inside a single `TabSplitViewController`
    /// rather than using one `NSWindow` per tab in an `NSWindowTabGroup`.
    var usesSingleWindowTabs: Bool { !isQuick && tabSplit != nil }

    func adopt(_ tab: TabLayout) {
        selectedTab.adopt(tab)
    }

    init(tabs initialTabs: [TabItem], selectedIndex: Int = 0, contentSize: NSSize, quick: Bool = false) {
        precondition(!initialTabs.isEmpty, "TerminalWindowController requires at least one TabItem")
        tabs = initialTabs
        selectedTabIndex = min(max(0, selectedIndex), initialTabs.count - 1)
        isQuick = quick
        let rect = NSRect(origin: .zero, size: contentSize)
        // Tabs live in the titlebar, which is tinted with the theme (see TerminalWindow).
        let window: NSWindow = quick ? QuickTerminalWindow(contentRect: rect)
            : TerminalWindow(contentRect: rect, styleMask: [.titled, .closable, .miniaturizable, .resizable],
                             backing: .buffered, defer: false)
        super.init(window: window)
        shouldCascadeWindows = false

        for tab in tabs {
            tab.windowController = self
            tab.content.controller = self
            tab.isClosed = false
        }

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

    convenience init(root: SplitNode, focused: PaneKey, zoomed: PaneKey?, floating: [FloatingPane] = [],
                     floatingHidden: Bool = false, title: String?, contentSize: NSSize, quick: Bool = false) {
        let tab = TabItem(root: root, focused: focused, zoomed: zoomed, floating: floating,
                          floatingHidden: floatingHidden, title: title)
        self.init(tabs: [tab], selectedIndex: 0, contentSize: contentSize, quick: quick)
    }

    private var layoutObservation: NSKeyValueObservation?

    /// The sidebar split when `tab_style = "sidebar"`, else nil (content is the window's view).
    private(set) var tabSplit: TabSplitViewController?
    var sidebar: TabSidebarViewController? { tabSplit?.sidebar }

    // MARK: Multi-tab hosting (single-window sidebar mode)

    /// Selects a hosted `TabItem` in place, swapping its `TabContentView` into `tabSplit`.
    func selectTab(_ tab: TabItem, focus: Bool = true) {
        guard let index = tabs.firstIndex(where: { $0 === tab }) else { return }
        let previous = selectedTab
        let changed = index != selectedTabIndex
        selectedTabIndex = index

        tab.applyConfigIfNeeded()
        syncRemoteOverlays(in: tab)
        tabSplit?.setContent(tab.content)

        if focus, let window, let view = tab.content.focusedView,
           !((window.firstResponder as? NSView)?.isDescendant(of: tab.content) ?? false) {
            window.makeFirstResponder(view)
        }
        if changed {
            previous.content.focusedView?.reportFocus()
        }
        tab.content.focusedView?.reportFocus()
        updateTitle()
        if changed {
            SessionManager.shared.tabSelected(tab, in: self)
        }
        sidebar?.reload()
    }

    /// Cycles to the previous (`delta == -1`) or next (`delta == 1`) tab in display order.
    func selectAdjacentTab(delta: Int) {
        let ordered = orderedTabItems
        guard ordered.count > 1,
              let current = ordered.firstIndex(where: { $0 === selectedTab }) else { return }
        let next = (current + delta + ordered.count) % ordered.count
        selectTab(ordered[next], focus: true)
        window?.makeKeyAndOrderFront(nil)
    }

    /// Inserts a new `TabItem` into this window without creating any new `NSWindow`.
    func insertTab(_ tab: TabItem, at index: Int? = nil, select: Bool = true) {
        let pos = min(max(0, index ?? tabs.count), tabs.count)
        tab.windowController = self
        tab.content.controller = self
        tab.isClosed = false
        tab.applyConfigIfNeeded()
        tabs.insert(tab, at: pos)
        if pos <= selectedTabIndex { selectedTabIndex += 1 }
        if select {
            selectTab(tab, focus: true)
        } else {
            updateTitle()
            sidebar?.reload()
            SessionManager.shared.scheduleLayoutSave()
        }
    }

    /// Removes `tab` from this window. Returns `true` without mutating when `tab` is the sole
    /// tab left (the caller then switches to another workspace or closes the window).
    @discardableResult
    func removeTab(_ tab: TabItem, detach: Bool = true) -> Bool {
        guard let index = tabs.firstIndex(where: { $0 === tab }) else { return false }
        if tabs.count == 1 { return true }
        let wasSelected = index == selectedTabIndex
        tabs.remove(at: index)
        tab.windowController = nil
        tab.content.controller = nil
        tab.content.removeFromSuperview()
        if detach {
            tab.isClosed = true
            tab.content.detachAll()
        }
        if index < selectedTabIndex {
            selectedTabIndex -= 1
            updateTitle()
            sidebar?.reload()
        } else if wasSelected {
            selectedTabIndex = min(index, tabs.count - 1)
            let next = selectedTab
            next.applyConfigIfNeeded()
            syncRemoteOverlays(in: next)
            tabSplit?.setContent(next.content)
            if let window, let view = next.content.focusedView,
               !((window.firstResponder as? NSView)?.isDescendant(of: next.content) ?? false) {
                window.makeFirstResponder(view)
            }
            next.content.focusedView?.reportFocus()
            updateTitle()
            SessionManager.shared.tabSelected(next, in: self)
            sidebar?.reload()
        } else {
            updateTitle()
            sidebar?.reload()
        }
        SessionManager.shared.focusChanged()
        SessionManager.shared.scheduleLayoutSave()
        return false
    }

    /// Reorders `tab` within `tabs` (used by sidebar drag-to-reorder in single-window mode).
    func moveHostedTab(_ tab: TabItem, toIndex targetIndex: Int) {
        guard let from = tabs.firstIndex(where: { $0 === tab }) else { return }
        let selected = selectedTab
        tabs.remove(at: from)
        let clamped = min(max(0, targetIndex), tabs.count)
        tabs.insert(tab, at: clamped)
        selectedTabIndex = tabs.firstIndex(where: { $0 === selected }) ?? 0
    }

    /// Replaces the entire tab list in place for a single-window workspace switch.
    /// Does not create or close any `NSWindow`, `NSToolbar`, or `TabSidebarViewController`.
    func replaceTabs(_ newTabs: [TabItem], selectedIndex: Int, workspaceID newWorkspaceID: UInt64)
        -> (oldTabs: [TabItem], oldSelectedIndex: Int) {
        precondition(!newTabs.isEmpty, "replaceTabs requires at least one TabItem")
        let oldTabs = tabs
        let oldSelected = selectedTabIndex
        let oldActive = selectedTab

        // Remove the previously visible content from the window hierarchy before wiring the new
        // tabs so its TerminalViews transition cleanly through viewDidMoveToWindow() -> unsubscribe
        // (parking into the warm LRU cache rather than tearing down).
        oldActive.content.removeFromSuperview()
        oldActive.content.focusedView?.reportFocus()
        let newIDs = Set(newTabs.map(ObjectIdentifier.init))
        for tab in oldTabs where !newIDs.contains(ObjectIdentifier(tab)) {
            tab.windowController = nil
            tab.content.controller = nil
        }

        workspaceID = newWorkspaceID
        tabs = newTabs
        selectedTabIndex = min(max(0, selectedIndex), newTabs.count - 1)
        for tab in tabs {
            tab.windowController = self
            tab.content.controller = self
            tab.isClosed = false
        }

        let active = selectedTab
        active.applyConfigIfNeeded()
        syncRemoteOverlays(in: active)
        tabSplit?.setContent(active.content)
        if let window, let view = active.content.focusedView,
           !((window.firstResponder as? NSView)?.isDescendant(of: active.content) ?? false) {
            window.makeFirstResponder(view)
        }
        active.content.focusedView?.reportFocus()
        updateTitle()
        return (oldTabs, oldSelected)
    }

    /// Adopts tabs folded from a native tab group when switching `native` → `sidebar`.
    func adoptHostedTabs(_ newTabs: [TabItem], selectedIndex: Int) {
        guard !newTabs.isEmpty else { return }
        tabs = newTabs
        selectedTabIndex = min(max(0, selectedIndex), newTabs.count - 1)
        for tab in tabs {
            tab.windowController = self
            tab.content.controller = self
            tab.isClosed = false
            if tab !== selectedTab {
                tab.content.removeFromSuperview()
            }
        }
        selectedTab.applyConfigIfNeeded()
        tabSplit?.setContent(selectedTab.content)
        if let window, let view = selectedTab.content.focusedView,
           !((window.firstResponder as? NSView)?.isDescendant(of: selectedTab.content) ?? false) {
            window.makeFirstResponder(view)
        }
        updateTitle()
        sidebar?.reload()
    }

    /// Detaches non-selected tabs when switching `sidebar` → `native`, returning the full ordered
    /// list and the selected index so the caller can wrap siblings in native windows.
    func keepOnlySelectedTab() -> (all: [TabItem], selectedIndex: Int) {
        let all = tabs
        let selIdx = selectedTabIndex
        let kept = selectedTab
        for tab in all where tab !== kept {
            tab.windowController = nil
            tab.content.controller = nil
            tab.content.removeFromSuperview()
        }
        tabs = [kept]
        selectedTabIndex = 0
        return (all, selIdx)
    }

    /// Marks this window as transferring its tabs elsewhere so closing it does not detach them.
    func markTabsTransferred() {
        tabsTransferred = true
        isClosed = true
        closingWithoutConfirmation = true
    }

    private func syncRemoteOverlays(in tab: TabItem) {
        for view in tab.content.views.values where view.pane.isRemote {
            view.setOffline(Remotes.shared.offlineMessage(view.pane.host))
        }
    }

    /// Native tab bar, or the vertical sidebar hosting `content`.
    func applyTabStyle() {
        guard let window = window as? TerminalWindow else { return }
        let cfg = SessionManager.shared.config
        if cfg.sidebarTabs {
            window.tabStrip = nil
            if let tabSplit {
                tabSplit.applyConfiguredWidth(cfg.sidebarWidth)
                tabSplit.setContent(content)
                return
            }
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
            window.sidebarCollapsed = split.sidebarCollapsed
            split.sidebar.reload()
        } else {
            let hadSplit = tabSplit != nil
            let frame = window.frame
            // Drop the sidebar's toolbar section while its split view still backs it: the
            // titlebar must never have more sections than the window.
            window.suppressTabBar = false
            if hadSplit {
                tabSplit?.detachContent()
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
            if !isQuick, window.tabStrip == nil {
                let strip = TabStrip()
                strip.controller = self
                window.tabStrip = strip
            }
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
        (window as? TerminalWindow)?.applyTitleColor()
        dot.applyTheme()
        applyTabStyle()
        (window as? TerminalWindow)?.hidesTitle = cfg.sidebarTabs && cfg.paneHeaders
        (window as? TerminalWindow)?.sidebarActiveOnly = cfg.sidebarActiveOnly
        sidebar?.backgroundColor = cfg.sidebarColor
        // The quick terminal's rounded corners need a transparent window.
        if opacity < 1 || isQuick {
            window.isOpaque = false
            window.backgroundColor = colorFromRGB(cfg.theme.background, alpha: 0.001)
        } else {
            window.isOpaque = true
            window.backgroundColor = colorFromRGB(cfg.theme.background)
        }
        refreshBlur()
        let gen = SessionManager.shared.configGeneration
        for tab in tabs {
            tab.appliedConfigGeneration = gen
            tab.content.applyConfig()
            tab.content.needsDisplay = true
        }
    }

    /// Behind-window blur for translucent windows. With `window.blur` it is an untinted visual
    /// effect view at the back of the window's frame view, under the (translucent) terminal
    /// content. A `window.material` other than `theme` backs only the chrome (the sidebar, or the
    /// titlebar holding the tabs), so its tint never reaches the terminal's transparent cells.
    private var blurView: NSVisualEffectView?
    private var chromeBackdrop: NSVisualEffectView?

    private func applyBlur(_ on: Bool) {
        guard let window = window, let frameView = window.contentView?.superview else { return }
        let cfg = SessionManager.shared.config
        let full = on && cfg.blur > 0
        let chrome = on && cfg.material != .theme
        blurView = Self.backdrop(blurView, in: full ? frameView : nil, tint: 0,
                                 background: cfg.theme.background)
        let chromeHost = sidebar?.view ?? (window as? TerminalWindow)?.titlebarView
        chromeBackdrop = Self.backdrop(chromeBackdrop, in: chrome ? chromeHost : nil,
                                       tint: cfg.material.tint, background: cfg.theme.background)
    }

    /// AppKit swaps the titlebar into another window in full screen (and back).
    func refreshBlur() {
        let cfg = SessionManager.shared.config
        applyBlur(opacity < 1 && (cfg.blur > 0 || cfg.material != .theme))
    }

    /// The blur behind `host`'s content, with the terminal background laid over it at `tint`.
    /// A nil `host` removes it.
    private static func backdrop(_ existing: NSVisualEffectView?, in host: NSView?, tint alpha: CGFloat,
                                 background: UInt32) -> NSVisualEffectView? {
        guard let host else {
            existing?.removeFromSuperview()
            return nil
        }
        let v = existing ?? NSVisualEffectView()
        v.material = .underWindowBackground
        v.blendingMode = .behindWindow
        v.state = .active
        v.frame = host.bounds
        v.autoresizingMask = [.width, .height]
        if v.superview !== host {
            host.addSubview(v, positioned: .below, relativeTo: host.subviews.first)
        }
        let tintID = NSUserInterfaceItemIdentifier("thurmBlurTint")
        let tint: NSView
        if let existing = v.subviews.first(where: { $0.identifier == tintID }) {
            tint = existing
        } else {
            tint = NSView(frame: v.bounds)
            tint.identifier = tintID
            tint.wantsLayer = true
            tint.autoresizingMask = [.width, .height]
            v.addSubview(tint)
        }
        tint.isHidden = alpha == 0
        tint.layer?.backgroundColor = colorFromRGB(background, alpha: alpha).cgColor
        return v
    }

    // MARK: Title and agent status

    func updateTitle() {
        guard let window = window else { return }
        let panes = SessionManager.shared.panes
        let config = SessionManager.shared.config
        let info = panes[content.focusedPane]
        let base = titleOverride ?? info?.displayTitle ?? "Thurm"

        // Most urgent agent status among the tab's panes whose state shows on the tab.
        var status: AgentStatus?
        var agentName: String?
        var detail: String?
        for id in content.paneIds {
            guard let agent = panes[id]?.agent, config.notice(for: agent).tabIndicator else { continue }
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
            case .error: prefix = "✗ "
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
        var hint = ""
        if !usesSingleWindowTabs {
            let tabs = orderedTabs
            if tabs.count > 1, let i = tabs.firstIndex(of: window), let n = tabShortcut(index: i, count: tabs.count) {
                hint = "⌘\(n)"
            }
        }
        shortcutHint.stringValue = hint
        shortcutHint.isHidden = hint.isEmpty
        let accessory = (status == nil && hint.isEmpty) ? nil : tabAccessory
        if window.tab.accessoryView !== accessory { window.tab.accessoryView = accessory }
        if !SessionManager.shared.isSwitchingWorkspace {
            SessionManager.shared.refreshSidebars()
        }
    }

    /// Called by a hosted content view when its focused pane changes.
    func focusedPaneChanged(in view: TabContentView) {
        if view === content {
            updateTitle()
            SessionManager.shared.focusChanged()
        } else {
            SessionManager.shared.refreshSidebars()
        }
        SessionManager.shared.scheduleLayoutSave()
    }

    /// Called by the active content view when the focused pane changes.
    func focusedPaneChanged() {
        focusedPaneChanged(in: content)
    }

    // MARK: Tab close confirmation

    /// Runs the handoff or busy-process confirmation for closing `tab`.
    func confirmClose(tab: TabItem) -> (confirmed: Bool, handoffRemove: String?) {
        let manager = SessionManager.shared
        if manager.isTerminating || closingWithoutConfirmation { return (true, nil) }
        let busy = tab.content.paneIds.compactMap { manager.panes[$0] }.filter { $0.hasRunningProcess }
        var handoffChoice = SessionManager.HandoffClose.keepWorktree
        if let handoff = tab.handoffID, Remotes.shared.handoffs[handoff] != nil {
            // Before any pane is closed: that stops the agent.
            handoffChoice = manager.askHandoffTabClose(handoff)
            if handoffChoice == .cancel { return (false, nil) }
        } else if manager.config.confirmClose && !busy.isEmpty {
            let names = busy.compactMap { $0.foregroundName }.joined(separator: ", ")
            let alert = NSAlert()
            alert.messageText = "Close this tab?"
            alert.informativeText = "Processes are still running: \(names). Closing the tab terminates them."
            alert.alertStyle = .warning
            alert.addButton(withTitle: "Close")
            alert.addButton(withTitle: "Cancel")
            guard alert.runModal() == .alertFirstButtonReturn else { return (false, nil) }
        }
        return (true, handoffChoice == .removeWorktree ? tab.handoffID : nil)
    }

    // MARK: NSWindowDelegate

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        let manager = SessionManager.shared
        if manager.isTerminating || closingWithoutConfirmation { return true }
        let target = selectedTab
        let (confirmed, handoffRemove) = confirmClose(tab: target)
        guard confirmed else { return false }
        for id in target.content.paneIds {
            manager.sendClosePane(id)
        }
        if let handoffRemove {
            DispatchQueue.main.async { manager.removeHandoffWorktree(handoffRemove) }
        }
        // In single-window sidebar mode, closing one of several tabs switches to a neighbor tab
        // in place without closing the window.
        if usesSingleWindowTabs && tabs.count > 1 {
            _ = removeTab(target, detach: true)
            return false
        }
        // The window shows another workspace now, and this tab is gone with it.
        return !manager.showNextWorkspace(closing: self)
    }

    func windowDidEndLiveResize(_ notification: Notification) {
        if isQuick { QuickTerminal.shared.userResized() }
    }

    func windowWillClose(_ notification: Notification) {
        if !tabsTransferred {
            for tab in tabs {
                tab.isClosed = true
                tab.content.detachAll()
            }
        }
        SessionManager.shared.controllerDidClose(self)
    }

    func windowDidBecomeKey(_ notification: Notification) {
        if let pending = pendingSidebarMatch {
            pendingSidebarMatch = nil
            tabSplit?.match(pending)
        }
        // A tab AppKit brings forward (the neighbour of one that closed) keeps the responder
        // it was left with: its sidebar, clicked to switch away, or none if never shown.
        // The terminal takes it back; the find bar's field, inside `content`, keeps it.
        if let window, let view = content.focusedView,
           !((window.firstResponder as? NSView)?.isDescendant(of: content) ?? false) {
            window.makeFirstResponder(view)
        }
        SessionManager.shared.controllerBecameKey(self)
        content.focusedView?.reportFocus()
    }

    func windowDidResignKey(_ notification: Notification) {
        if !tabsTransferred {
            content.focusedView?.reportFocus()
        }
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
        for tab in tabs {
            for view in tab.content.views.values { view.metricsChanged() }
        }
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

    @objc func copyPaneID(_ sender: Any?) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(focusedPane.copyText, forType: .string)
    }

    @objc func toggleZoom(_ sender: Any?) {
        content.toggleZoom()
    }

    /// Floats the focused pane above the splits, or docks it back.
    @objc func toggleFloat(_ sender: Any?) {
        let pane = content.focusedPane
        if !content.setFloating(pane, nil) {
            content.focusedView?.showToast("The only pane of the splits can't float")
        }
    }

    /// Opens a shell floating above the splits when the tab has none, else hides the floating
    /// panes or shows them again.
    @objc func toggleFloatingPanes(_ sender: Any?) {
        if content.floating.isEmpty {
            SessionManager.shared.newFloatingPane(in: self)
        } else {
            content.setFloatingHidden(!content.floatingHidden)
        }
    }

    static let edgeMoves: [(edge: SplitDirName, title: String, name: String)] = [
        (.left, "Left Edge", "move_pane_to_left_edge"), (.right, "Right Edge", "move_pane_to_right_edge"),
        (.up, "Top Edge", "move_pane_to_top_edge"), (.down, "Bottom Edge", "move_pane_to_bottom_edge"),
    ]

    @objc func movePaneToEdge(_ sender: Any?) {
        guard let tag = (sender as? NSMenuItem)?.tag, Self.edgeMoves.indices.contains(tag) else { return }
        let pane = content.focusedPane
        content.moveToEdge(pane, Self.edgeMoves[tag].edge)
        content.focus(pane)
    }

    @objc func minimizeFloatingPane(_ sender: Any?) {
        content.setMinimized(content.focusedPane, true)
    }

    @objc func newFloatingPane(_ sender: Any?) {
        SessionManager.shared.newFloatingPane(in: self)
    }

    @objc func moveFloatingPaneToNewTab(_ sender: Any?) {
        SessionManager.shared.moveCardToNewTab(content.focusedPane, from: selectedTab)
    }

    @objc func toggleAgentWall(_ sender: Any?) {
        SessionManager.shared.toggleAgentWall(in: self)
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

    @objc func movePaneLeft(_ sender: Any?) { content.movePane(.left) }
    @objc func movePaneRight(_ sender: Any?) { content.movePane(.right) }
    @objc func movePaneUp(_ sender: Any?) { content.movePane(.up) }
    @objc func movePaneDown(_ sender: Any?) { content.movePane(.down) }

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

    /// All tabs of this window (hosted in sidebar mode, or across its native tab group), in
    /// display order.
    var orderedTabItems: [TabItem] {
        let all = SessionManager.shared.windowTabs(of: self)
        return SessionManager.shared.config.sidebarTabs ? TabSidebarViewController.displayOrder(all) : all
    }

    /// Cmd+1...9 (tag = number; 9 selects the last tab).
    @objc func selectTabByNumber(_ sender: Any?) {
        guard let item = sender as? NSMenuItem, window != nil else { return }
        let config = SessionManager.shared.config
        if !isQuick, config.sidebarTabs, config.sidebarByHost || config.sidebarAllWorkspaces {
            let targets = TabSidebarViewController.shortcutTargets(for: self)
            guard !targets.isEmpty else { return }
            let index = item.tag >= 9 ? targets.count - 1 : item.tag - 1
            guard index >= 0, index < targets.count else { return }
            switch targets[index] {
            case .hidden(let hidden):
                SessionManager.shared.showHiddenTab(hidden, from: self)
            case .live(let target):
                if let host = target.windowController {
                    if host.usesSingleWindowTabs && SessionManager.shared.group(of: host).count == 1 {
                        host.selectTab(target, focus: true)
                        host.window?.makeKeyAndOrderFront(nil)
                    } else if let w = host.window {
                        w.makeKeyAndOrderFront(nil)
                        if let view = target.content.focusedView {
                            w.makeFirstResponder(view)
                        }
                    }
                }
            }
            return
        }
        if usesSingleWindowTabs && SessionManager.shared.group(of: self).count == 1 {
            let ordered = orderedTabItems
            guard !ordered.isEmpty else { return }
            let index = item.tag >= 9 ? ordered.count - 1 : item.tag - 1
            guard index >= 0, index < ordered.count else { return }
            selectTab(ordered[index], focus: true)
            window?.makeKeyAndOrderFront(nil)
            return
        }
        let windows = orderedTabs
        guard !windows.isEmpty else { return }
        let index = item.tag >= 9 ? windows.count - 1 : item.tag - 1
        guard index >= 0, index < windows.count else { return }
        let target = windows[index]
        target.makeKeyAndOrderFront(nil)
        // Restore terminal focus if a sidebar or search field kept it.
        if let controller = target.windowController as? TerminalWindowController,
           let view = controller.content.focusedView {
            target.makeFirstResponder(view)
        }
    }

    @objc func togglePinTab(_ sender: Any?) {
        SessionManager.shared.togglePin(selectedTab)
    }

    /// The "+" button of the native tab bar.
    @objc override func newWindowForTab(_ sender: Any?) {
        SessionManager.shared.newTab(from: self)
    }
}

extension TerminalWindowController: NSMenuItemValidation {
    /// Float Pane reads Dock Pane while the focused pane floats, and Show Floating Panes reads
    /// Hide Floating Panes while they show. The quick terminal floats none.
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case #selector(toggleFloat(_:)):
            let floats = content.isFloating(content.focusedPane)
            menuItem.title = floats ? "Dock Pane" : "Float Pane"
            return floats || !isQuick
        case #selector(toggleFloatingPanes(_:)):
            let shown = !content.floating.isEmpty && !content.floatingHidden
            menuItem.title = shown ? "Hide Floating Panes" : "Show Floating Panes"
            return !isQuick
        case #selector(newFloatingPane(_:)):
            return !isQuick
        case #selector(togglePinTab(_:)):
            menuItem.title = pinned ? "Unpin Tab" : "Pin Tab"
            return !isQuick && SessionManager.shared.config.sidebarTabs
        case #selector(movePaneToEdge(_:)):
            return content.isFloating(content.focusedPane) || content.canMoveSplit(content.focusedPane)
        case #selector(minimizeFloatingPane(_:)):
            return content.isFloating(content.focusedPane)
        case #selector(moveFloatingPaneToNewTab(_:)):
            return SessionManager.shared.canMoveCards(selectedTab) && content.isFloating(content.focusedPane)
        default:
            return true
        }
    }
}
