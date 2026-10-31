import AppKit

/// Terminal window with macOS 26+ "tabs in the titlebar": the native NSTabBar is moved into the
/// unified-compact toolbar row next to the traffic lights (glass tab pills, round "+" button),
/// and the titlebar is tinted with the terminal background. Without tabs, a centered title
/// toolbar item is shown instead.
///
/// AppKit has no public API for this. The approach follows Ghostty's
/// `TitlebarTabsTahoeTerminalWindow`: intercept the tab bar's titlebar accessory, then re-anchor
/// its private clip view inside `NSToolbarView` with constraints. Every lookup is by class name
/// and fails soft: if AppKit's hierarchy changes, the stock tab bar below the titlebar remains.
///
/// On macOS 27 the tab strip also paints its own grey glass over the tinted titlebar; see
/// `syncTabBarBackground`.
final class TerminalWindow: NSWindow, NSToolbarDelegate {
    private static let tabBarIdentifier = NSUserInterfaceItemIdentifier("thurmTabBar")
    private static let titleItem = NSToolbarItem.Identifier("thurmTitle")
    private static let sidebarItem = NSToolbarItem.Identifier("thurmSidebar")
    private static let newTabItem = NSToolbarItem.Identifier("thurmNewTab")
    private static let activeOnlyItem = NSToolbarItem.Identifier("thurmActiveOnly")

    /// A collapsed sidebar leaves just its toggle and New Tab beside the traffic lights.
    var sidebarCollapsed = false {
        didSet {
            guard sidebarCollapsed != oldValue else { return }
            syncSidebarItem()
        }
    }

    /// `window.sidebar_active_only`, which the button next to the sidebar button shows.
    var sidebarActiveOnly = false {
        didSet {
            guard sidebarActiveOnly != oldValue else { return }
            syncSidebarItem()
        }
    }

    /// Vertical tabs are on: AppKit's tab bar stays installed (so switching back is instant and
    /// AppKit's own bookkeeping is untouched) but hidden. The sidebar then runs up under a clear
    /// titlebar, and a toolbar button toggles it.
    var suppressTabBar = false {
        didSet {
            guard suppressTabBar != oldValue else { return }
            for vc in titlebarAccessoryViewControllers where isTabBar(vc) {
                vc.isHidden = hidesTabBar
            }
            applyTabBarVisibility()
            syncTitlebar()
            syncSidebarItem()
        }
    }

    /// Horizontal tabs: the strip takes AppKit's tab bar's place in the titlebar row.
    var tabStrip: TabStrip? {
        didSet {
            guard tabStrip !== oldValue else { return }
            oldValue?.removeFromSuperview()
            for vc in titlebarAccessoryViewControllers where isTabBar(vc) {
                vc.isHidden = hidesTabBar
            }
            installTabStrip()
            applyTabBarVisibility()
        }
    }

    /// AppKit's tab bar stays installed but hidden under vertical tabs or the strip.
    private var hidesTabBar: Bool { suppressTabBar || tabStrip != nil }

    /// The strip goes into the titlebar itself, after the traffic lights.
    private func installTabStrip() {
        guard let strip = tabStrip, let bar = titlebarView else { return }
        guard strip.superview !== bar else { return }
        strip.removeFromSuperview()
        strip.translatesAutoresizingMaskIntoConstraints = false
        bar.addSubview(strip)
        let lights = standardWindowButton(.zoomButton).map { $0.convert($0.bounds, to: bar).maxX } ?? 70
        let centerY = standardWindowButton(.closeButton).map { strip.centerYAnchor.constraint(equalTo: $0.centerYAnchor) }
            ?? strip.centerYAnchor.constraint(equalTo: bar.centerYAnchor)
        NSLayoutConstraint.activate([
            strip.leadingAnchor.constraint(equalTo: bar.leadingAnchor, constant: lights + 16),
            strip.trailingAnchor.constraint(equalTo: bar.trailingAnchor, constant: -10),
            strip.heightAnchor.constraint(equalToConstant: TabStrip.height),
            centerY,
        ])
        strip.reload()
    }

    /// Keep the leading pair in place. Its second button becomes New Tab when collapsed, and
    /// the separate New Tab at the sidebar's far edge is then unnecessary.
    private func syncSidebarItem() {
        guard let toolbar else { return }
        // These controls change once per toggle; they must not inherit the split view's
        // animation and briefly fade or move through an intermediate toolbar layout.
        NSAnimationContext.beginGrouping()
        NSAnimationContext.current.duration = 0
        NSAnimationContext.current.allowsImplicitAnimation = false
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer {
            CATransaction.commit()
            NSAnimationContext.endGrouping()
        }
        for item in toolbar.items where item.itemIdentifier == Self.activeOnlyItem {
            styleActiveOnlyItem(item)
        }
        let separator = toolbar.items.firstIndex { $0.itemIdentifier == .sidebarTrackingSeparator }
        let leading = [Self.sidebarItem, Self.activeOnlyItem]
        let expanded: [NSToolbarItem.Identifier] = [.flexibleSpace, Self.newTabItem]
        if !suppressTabBar {
            // Switching to native tabs removes the entire sidebar section.
            if let separator {
                for i in (0...separator).reversed() { toolbar.removeItem(at: i) }
            }
        } else if let separator {
            // The separator anchors this section throughout a toggle. Change only the
            // flexible space and far-edge New Tab between the leading pair and that anchor.
            if sidebarCollapsed {
                for i in (leading.count..<separator).reversed() { toolbar.removeItem(at: i) }
            } else if separator == leading.count {
                for (offset, id) in expanded.enumerated() {
                    toolbar.insertItem(withItemIdentifier: id, at: separator + offset)
                }
            }
        } else {
            let items = leading + (sidebarCollapsed ? [] : expanded) + [.sidebarTrackingSeparator]
            for (i, id) in items.enumerated() {
                toolbar.insertItem(withItemIdentifier: id, at: i)
            }
        }
        titlebarView?.layoutSubtreeIfNeeded()
    }

    /// The tab bar lives in the toolbar row (see `setupTabBar`), outside the accessory's
    /// visibility: hide its container directly, and show the title instead.
    private func applyTabBarVisibility() {
        guard let tabBar = titlebarView?.firstDescendant(className: "NSTabBar") else {
            titleLabel.isHidden = hidesTitle || tabStrip != nil
            return
        }
        let clip = tabBar.firstAncestor(className: "NSTitlebarAccessoryClipView")
            ?? tabBar.firstAncestor(className: "NSTitlebarAccessoryContainerView")
        (clip ?? tabBar).isHidden = hidesTabBar
        titleLabel.isHidden = !suppressTabBar || hidesTitle || tabStrip != nil
        if !hidesTabBar { scheduleTabBarBackgroundSync() }
    }

    /// Vertical tabs with pane headers: each header names its pane, so the title stays empty.
    var hidesTitle = false {
        didSet { if hidesTitle != oldValue { applyTabBarVisibility() } }
    }

    /// Right edge, in window coordinates, of the titlebar's leading controls: the traffic lights
    /// and, with vertical tabs, the sidebar and New Tab buttons (which follow the traffic lights
    /// when the sidebar is collapsed). Pane headers under the titlebar start past it.
    var leadingControlsMaxX: CGFloat {
        guard let bar = titlebarView else { return 0 }
        var maxX: CGFloat = 0
        func walk(_ view: NSView) {
            for sub in view.subviews where !sub.isHidden && sub.alphaValue > 0 {
                if sub is NSControl, sub !== titleLabel {
                    let frame = sub.convert(sub.bounds, to: nil)
                    if frame.minX < self.frame.width / 2 { maxX = max(maxX, frame.maxX) }
                } else {
                    walk(sub)
                }
            }
        }
        walk(bar)
        return maxX
    }

    /// The window's corner radius: AppKit's own (private `_cornerRadius`) when it answers, else
    /// what macOS 27 draws (about 18pt); square in full screen.
    var cornerRadius: CGFloat {
        if styleMask.contains(.fullScreen) { return 0 }
        if responds(to: NSSelectorFromString("_cornerRadius")),
           let r = value(forKey: "_cornerRadius") as? CGFloat, r > 0 {
            return r
        }
        return 18
    }

    /// Terminal background used to tint the titlebar.
    var titlebarColor: NSColor? {
        didSet { syncTitlebar() }
    }

    private let titleLabel: NSTextField = {
        let label = NSTextField(labelWithString: "")
        label.font = .titleBarFont(ofSize: NSFont.systemFontSize)
        label.alignment = .center
        label.lineBreakMode = .byTruncatingTail
        label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        label.translatesAutoresizingMaskIntoConstraints = false
        return label
    }()

    /// The title sits in the titlebar itself, centered on the window. As a toolbar item it
    /// would be centered in the toolbar's section beside the sidebar instead.
    private func installTitleLabel() {
        guard let bar = titlebarView, titleLabel.superview !== bar else { return }
        titleLabel.removeFromSuperview()
        bar.addSubview(titleLabel)
        let centerY = standardWindowButton(.closeButton).map { titleLabel.centerYAnchor.constraint(equalTo: $0.centerYAnchor) }
            ?? titleLabel.centerYAnchor.constraint(equalTo: bar.centerYAnchor)
        let center = titleLabel.centerXAnchor.constraint(equalTo: bar.centerXAnchor)
        center.priority = .defaultHigh
        NSLayoutConstraint.activate([
            center, centerY,
            // Clear of the traffic lights; truncates rather than overlapping them.
            titleLabel.leadingAnchor.constraint(greaterThanOrEqualTo: bar.leadingAnchor, constant: 84),
            titleLabel.trailingAnchor.constraint(lessThanOrEqualTo: bar.trailingAnchor, constant: -12),
        ])
    }

    private var tabBarObserver: NSObjectProtocol? {
        didSet {
            if let old = oldValue { NotificationCenter.default.removeObserver(old) }
        }
    }
    private var tabGroupObservation: NSKeyValueObservation?
    private var tabSelectionObservation: NSKeyValueObservation?
    private weak var observedTabGroup: NSWindowTabGroup?

    override init(contentRect: NSRect, styleMask style: NSWindow.StyleMask,
                  backing backingStoreType: NSWindow.BackingStoreType, defer flag: Bool) {
        super.init(contentRect: contentRect, styleMask: style, backing: backingStoreType, defer: flag)
        titleVisibility = .hidden
        // AppKit synchronizes toolbars with the same identifier, but tabs can have different
        // sidebar states (including a hidden tab or workspace that has yet to be shown).
        let toolbar = NSToolbar(identifier: "Thurm-\(UUID().uuidString)")
        toolbar.delegate = self
        self.toolbar = toolbar
        toolbarStyle = .unifiedCompact
        let nc = NotificationCenter.default
        nc.addObserver(self, selector: #selector(fullScreenChanged(_:)), name: NSWindow.didEnterFullScreenNotification,
                       object: self)
        nc.addObserver(self, selector: #selector(fullScreenChanged(_:)), name: NSWindow.didExitFullScreenNotification,
                       object: self)
    }

    deinit {
        tabBarObserver = nil
        NotificationCenter.default.removeObserver(self)
    }

    /// Full screen swaps the titlebar into another window (and back): redo tint and tabs.
    @objc private func fullScreenChanged(_ note: Notification) {
        tabBarObserver = nil
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(60)) { [weak self] in
            self?.syncTitlebar()
            self?.installTitleLabel()
            self?.installTabStrip()
            self?.setupTabBar()
            self?.scheduleTabBarBackgroundSync()
        }
    }

    override var title: String {
        didSet { titleLabel.stringValue = title.isEmpty ? " " : title }
    }

    /// Thurm has one window: a tab moved out goes to a new workspace, and there is nothing
    /// to merge.
    override func moveTabToNewWindow(_ sender: Any?) {
        guard let c = windowController as? TerminalWindowController else { return }
        SessionManager.shared.moveTabToNewWorkspace(c)
    }

    override func mergeAllWindows(_ sender: Any?) {}

    /// The title's color follows the theme, and whether the window is the main one.
    func applyTitleColor() {
        let chrome = SessionManager.shared.config.chrome
        titleLabel.textColor = isMainWindow ? chrome.contentPrimary : chrome.contentSecondary
    }

    override func sendEvent(_ event: NSEvent) {
        // Before super: the click must not make this window key first.
        if CommandPalette.shared.dismissOnClick(event) {
            makeKeyAndOrderFront(nil)
            return
        }
        super.sendEvent(event)
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        super.performKeyEquivalent(with: event) || MainMenu.physicalFallback(event)
    }

    override func becomeMain() {
        super.becomeMain()
        applyTitleColor()
        installTitleLabel()
        observeTabGroup()
        syncTitlebar()
        setupTabBar()
        scheduleTabBarBackgroundSync()
    }

    override func resignMain() {
        super.resignMain()
        applyTitleColor()
        scheduleTabBarBackgroundSync()
    }

    override func becomeKey() {
        super.becomeKey()
        scheduleTabBarBackgroundSync()
    }

    override func resignKey() {
        super.resignKey()
        scheduleTabBarBackgroundSync()
    }

    override func update() {
        super.update()
        // Adding a tab rebuilds the bar lazily, after the KVO callbacks ran. This runs once per
        // event loop pass before display: the earliest point to fix a new bar up. It runs for
        // every key and mouse event, so walk the bar only for a few passes after the tabs,
        // selection, size or tint changed, and otherwise at most twice a second.
        let key = TabBarSyncKey(tabs: tabGroup?.windows.count ?? 1,
                                selected: tabGroup?.selectedWindow.map(ObjectIdentifier.init),
                                fullScreen: styleMask.contains(.fullScreen),
                                width: frame.width,
                                color: titlebarColor,
                                appearance: effectiveAppearance.name,
                                suppressed: hidesTabBar)
        if key != tabBarSyncKey {
            tabBarSyncKey = key
            tabBarSyncPasses = 3
        }
        let now = CACurrentMediaTime()
        guard tabBarSyncPasses > 0 || now - lastTabBarSync > 0.5 else { return }
        tabBarSyncPasses = max(0, tabBarSyncPasses - 1)
        lastTabBarSync = now
        syncTabBarBackground()
        if hidesTabBar { applyTabBarVisibility() }
    }

    /// What decides whether `update()` walks the tab bar.
    private struct TabBarSyncKey: Equatable {
        var tabs: Int
        var selected: ObjectIdentifier?
        var fullScreen: Bool
        var width: CGFloat
        var color: NSColor?
        var appearance: NSAppearance.Name
        var suppressed: Bool
    }

    private var tabBarSyncKey: TabBarSyncKey?
    private var tabBarSyncPasses = 0
    private var lastTabBarSync: CFTimeInterval = 0

    // MARK: Tab bar

    override func addTitlebarAccessoryViewController(_ vc: NSTitlebarAccessoryViewController) {
        guard isTabBar(vc) else {
            super.addTitlebarAccessoryViewController(vc)
            return
        }
        vc.identifier = Self.tabBarIdentifier
        tabBarObserver = nil
        // Must be set before adding, or AppKit asserts during layout.
        vc.layoutAttribute = .right
        vc.isHidden = hidesTabBar
        super.addTitlebarAccessoryViewController(vc)
        // Wait a tick: with several restored tabs the bar is not fully populated yet.
        DispatchQueue.main.async { [weak self] in self?.setupTabBar() }
    }

    override func removeTitlebarAccessoryViewController(at index: Int) {
        let isBar = titlebarAccessoryViewControllers.indices.contains(index)
            && isTabBar(titlebarAccessoryViewControllers[index])
        super.removeTitlebarAccessoryViewController(at: index)
        if isBar {
            tabBarObserver = nil
            titleLabel.isHidden = false
        }
    }

    private func isTabBar(_ vc: NSTitlebarAccessoryViewController) -> Bool {
        if vc.identifier == Self.tabBarIdentifier { return true }
        guard vc.identifier == nil else { return false }
        if vc.view.firstDescendant(className: "NSTabBar") != nil { return true }
        // A window joining an existing group first gets an empty placeholder view.
        return vc.layoutAttribute == .bottom && vc.view.className == "NSView" && vc.view.subviews.isEmpty
    }

    /// Idempotent: only the main window of a tab group owns the NSTabBar, so this runs again
    /// whenever the window becomes main or the bar is re-laid out.
    private func setupTabBar() {
        guard tabBarObserver == nil,
              let titlebarView,
              let tabBar = titlebarView.firstDescendant(className: "NSTabBar"),
              // macOS 26: NSTitlebarAccessoryClipView; macOS 27: NSTitlebarAccessoryContainerView.
              let clip = tabBar.firstAncestor(className: "NSTitlebarAccessoryClipView")
                ?? tabBar.firstAncestor(className: "NSTitlebarAccessoryContainerView"),
              let accessory = clip.subviews.first,
              let toolbarView = titlebarView.firstDescendant(className: "NSToolbarView"),
              let newTabButton = titlebarView.firstDescendant(className: "NSTabBarNewTabButton")
        else { return }

        tabBar.frame.size.height = newTabButton.frame.width
        clip.translatesAutoresizingMaskIntoConstraints = false
        accessory.translatesAutoresizingMaskIntoConstraints = false
        // 70pt clears the traffic lights.
        NSLayoutConstraint.activate([
            clip.leftAnchor.constraint(equalTo: toolbarView.leftAnchor, constant: 70),
            clip.rightAnchor.constraint(equalTo: toolbarView.rightAnchor),
            clip.topAnchor.constraint(equalTo: toolbarView.topAnchor, constant: 2),
            clip.heightAnchor.constraint(equalTo: toolbarView.heightAnchor),
            accessory.leftAnchor.constraint(equalTo: clip.leftAnchor),
            accessory.rightAnchor.constraint(equalTo: clip.rightAnchor),
            accessory.topAnchor.constraint(equalTo: clip.topAnchor),
            accessory.heightAnchor.constraint(equalTo: clip.heightAnchor),
        ])
        clip.needsLayout = true
        accessory.needsLayout = true
        applyTabBarVisibility()
        // AppKit may have rebuilt the accessory container with its own material.
        syncTabBarBackground()

        // Appearance changes and tab moves resize the bar and drop our constraints; redo them.
        tabBar.postsFrameChangedNotifications = true
        tabBarObserver = NotificationCenter.default.addObserver(
            forName: NSView.frameDidChangeNotification, object: tabBar, queue: .main
        ) { [weak self] _ in
            self?.tabBarObserver = nil
            DispatchQueue.main.async { self?.setupTabBar() }
        }
    }

    /// macOS rebuilds the tab bar and titlebar when tabs are added, removed or moved.
    private func observeTabGroup() {
        guard let group = tabGroup, group !== observedTabGroup else { return }
        observedTabGroup = group
        tabGroupObservation = group.observe(\.windows) { [weak self] _, _ in
            DispatchQueue.main.async {
                self?.syncTitlebar()
                self?.setupTabBar()
                // Tabs were added, closed or reordered: save the layout now (was a 3 s poll)
                // and refresh the Cmd+N hints and sidebars.
                SessionManager.shared.scheduleLayoutSave()
                SessionManager.shared.refreshSidebars()
                for c in SessionManager.shared.liveControllers where c.window?.tabGroup === group {
                    c.updateTitle()
                }
            }
        }
        // Selecting a tab restyles the buttons over a short animation, recreating the glass
        // layers we neutralised; re-apply across it.
        tabSelectionObservation = group.observe(\.selectedWindow) { [weak self] _, _ in
            DispatchQueue.main.async {
                SessionManager.shared.refreshSidebars()
                self?.syncTabBarBackground()
                for ms in [50, 150, 300, 600] {
                    DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(ms)) {
                        self?.syncTabBarBackground()
                    }
                }
            }
        }
    }

    // MARK: Titlebar tint

    /// In full screen the titlebar lives in a separate `NSToolbarFullScreenWindow` whose parent
    /// is this window (as in Ghostty's `titlebarContainer`).
    var titlebarView: NSView? {
        if styleMask.contains(.fullScreen) {
            for w in NSApp.windows where w.className == "NSToolbarFullScreenWindow" && w.parent === self {
                if let v = w.contentView?.rootView.firstDescendant(className: "NSTitlebarView") { return v }
            }
        }
        return contentView?.rootView.firstDescendant(className: "NSTitlebarView")
    }

    private func syncTitlebar() {
        guard let titlebarView else { return }
        titlebarView.wantsLayer = true
        // With vertical tabs the titlebar stays clear: the sidebar's material shows through on
        // its side, the window background (the terminal color) on the other.
        titlebarView.layer?.backgroundColor = suppressTabBar ? nil : titlebarColor?.cgColor
        // Its subviews paint the stock titlebar material over our color.
        titlebarView.superview?.firstDescendant(className: "NSTitlebarBackgroundView")?.isHidden = true
        (windowController as? TerminalWindowController)?.refreshBlur()
        syncTabBarBackground()
    }

    // MARK: macOS 27 tab strip

    /// AppKit restyles the tab bar after key/main transitions; re-apply on the next turns.
    private func scheduleTabBarBackgroundSync() {
        syncTabBarBackground()
        DispatchQueue.main.async { [weak self] in self?.syncTabBarBackground() }
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(100)) { [weak self] in
            self?.syncTabBarBackground()
        }
    }

    /// On macOS 27 `NSTabBar` wraps its track in a glass view whose `NSTabBarTrackFilterHost`
    /// layer holds a backdrop blur plus a grey material fill, and every `NSTabButton` renders
    /// glass through a SwiftUI hosting view inside its `NSGlassEffectView`, with the title shown
    /// through a `CAPortalLayer` from that renderer. None of it honours the titlebar color, so
    /// the strip stays grey. Hide the material layers, let the portal source draw on its own,
    /// and paint the selected tab from the terminal background. Idempotent; AppKit rebuilds the
    /// bar often.
    private func syncTabBarBackground() {
        guard #available(macOS 27, *),
              let base = titlebarColor,
              let tabBar = titlebarView?.firstDescendant(className: "NSTabBar")
        else { return }

        // Raw CALayer changes animate implicitly: a 250ms grey fade on every rebuild.
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }

        // AppKit can keep several tracks around while animating: walk the whole bar. The track
        // also carries a thin rim drawn by its accessibility border view.
        for border in tabBar.descendants(className: "NSView")
        where border.identifier?.rawValue == "_tabBarAccessibilityBorderView" {
            border.isHidden = true
        }
        if let root = tabBar.layer {
            Self.forEachLayer(in: root) { layer in
                if layer.name == "NSTabBarTrackFilterHost" {
                    layer.isHidden = true
                    layer.opacity = 0
                }
            }
        }

        let selectedIndex = tabGroup.flatMap { g in g.selectedWindow.flatMap { g.windows.firstIndex(of: $0) } }
        let isLight = luminance(base.rgb) >= 0.5
        let highlight = (isLight ? base.shadow(withLevel: 0.06) : base.highlight(withLevel: 0.08))?.cgColor
            ?? base.cgColor
        let coverID = NSUserInterfaceItemIdentifier("thurmTabGlassCover")

        let buttons = tabBar.descendants(className: "NSTabButton").sorted { $0.frame.minX < $1.frame.minX }
        for (index, button) in buttons.enumerated() {
            guard let glass = button.firstDescendant(className: "NSGlassEffectView") else { continue }
            // The renderer also portals the tab content, so only its material layers go.
            for sub in glass.subviews where String(describing: type(of: sub)).hasPrefix("_NSCoreHostingView") {
                if let layer = sub.layer {
                    Self.hideGlassMaterial(in: layer)
                    Self.releasePortalSources(in: layer)
                }
                sub.isHidden = true
                sub.alphaValue = 0
            }

            // Our own selection highlight, beneath the tab content.
            let cover: NSView
            if let existing = glass.subviews.first(where: { $0.identifier == coverID }) {
                cover = existing
            } else {
                cover = NSView(frame: glass.bounds)
                cover.identifier = coverID
                cover.wantsLayer = true
                cover.translatesAutoresizingMaskIntoConstraints = false
                glass.addSubview(cover, positioned: .below, relativeTo: glass.subviews.first)
                NSLayoutConstraint.activate([
                    cover.leadingAnchor.constraint(equalTo: glass.leadingAnchor),
                    cover.trailingAnchor.constraint(equalTo: glass.trailingAnchor),
                    cover.topAnchor.constraint(equalTo: glass.topAnchor),
                    cover.bottomAnchor.constraint(equalTo: glass.bottomAnchor),
                ])
            }
            if let coverLayer = cover.layer, let host = glass.layer, host.sublayers?.first !== coverLayer {
                coverLayer.removeFromSuperlayer()
                host.insertSublayer(coverLayer, at: 0)
            }
            cover.layer?.zPosition = -1
            cover.layer?.cornerRadius = glass.bounds.height / 2
            cover.layer?.backgroundColor = index == selectedIndex ? highlight : NSColor.clear.cgColor
        }
    }

    private static func forEachLayer(in layer: CALayer, _ body: (CALayer) -> Void) {
        body(layer)
        for sub in layer.sublayers ?? [] { forEachLayer(in: sub, body) }
    }

    /// The portal hides its source; once the renderer is hidden the title would vanish with it.
    private static func releasePortalSources(in layer: CALayer) {
        if NSStringFromClass(type(of: layer)).hasSuffix("PortalLayer"),
           layer.responds(to: NSSelectorFromString("setHidesSourceLayer:")) {
            layer.setValue(false, forKey: "hidesSourceLayer")
        }
        for sub in layer.sublayers ?? [] { releasePortalSources(in: sub) }
    }

    /// Hides material layers under a glass renderer, keeping any subtree with a portal.
    /// Returns true when `layer` or a descendant is a portal layer.
    @discardableResult
    private static func hideGlassMaterial(in layer: CALayer) -> Bool {
        let name = NSStringFromClass(type(of: layer))
        if name.hasSuffix("PortalLayer") { return true }
        var hasPortal = false
        for sub in layer.sublayers ?? [] where hideGlassMaterial(in: sub) {
            hasPortal = true
        }
        if hasPortal { return true }
        if name == "CABackdropLayer" || !(layer.filters ?? []).isEmpty || name.contains("SDF") {
            layer.isHidden = true
            layer.opacity = 0
        }
        return false
    }

    // MARK: NSToolbarDelegate

    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [Self.titleItem, Self.sidebarItem, Self.activeOnlyItem, Self.newTabItem, .sidebarTrackingSeparator,
         .flexibleSpace]
    }

    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [.flexibleSpace]
    }

    func toolbar(_ toolbar: NSToolbar, itemForItemIdentifier id: NSToolbarItem.Identifier,
                 willBeInsertedIntoToolbar flag: Bool) -> NSToolbarItem? {
        let item = NSToolbarItem(itemIdentifier: id)
        if id == Self.sidebarItem {
            item.image = NSImage(systemSymbolName: "sidebar.left", accessibilityDescription: "Toggle Sidebar")
            item.label = "Sidebar"
            item.toolTip = "Show or Hide Sidebar (⌘B)"
            item.action = #selector(AppDelegate.toggleTabSidebar(_:))
            item.visibilityPriority = .high
        }
        if id == Self.activeOnlyItem {
            item.visibilityPriority = .high
            styleActiveOnlyItem(item)
        }
        if id == Self.newTabItem {
            item.image = NSImage(systemSymbolName: "plus", accessibilityDescription: "New Tab")
            item.label = "New Tab"
            item.toolTip = "New Tab (⌘T)"
            item.action = #selector(AppDelegate.newTab(_:))
            item.visibilityPriority = .high
        }
        if id == Self.titleItem {
            item.view = titleLabel
            item.visibilityPriority = .user
            item.isEnabled = false
            // No glass capsule behind the title.
            item.isBordered = false
        }
        return item
    }

    /// The second button keeps its place, swapping the filter for New Tab while collapsed.
    private func styleActiveOnlyItem(_ item: NSToolbarItem) {
        if sidebarCollapsed {
            item.image = NSImage(systemSymbolName: "plus", accessibilityDescription: "New Tab")
            item.label = "New Tab"
            item.toolTip = "New Tab (⌘T)"
            item.action = #selector(AppDelegate.newTab(_:))
            return
        }
        item.label = "Only Active Tabs"
        item.action = #selector(AppDelegate.toggleSidebarActiveOnly(_:))
        let name = sidebarActiveOnly ? "line.3.horizontal.decrease.circle.fill" : "line.3.horizontal.decrease.circle"
        item.image = NSImage(systemSymbolName: name, accessibilityDescription: "Only Active Tabs")
        item.toolTip = sidebarActiveOnly ? "Show All Tabs" : "Show Only Active Tabs"
    }
}

extension NSView {
    /// The top of this view's hierarchy (the window's theme frame).
    var rootView: NSView {
        var v = self
        while let s = v.superview { v = s }
        return v
    }

    /// Also matches private Swift classes, whose `className` is mangled
    /// (`_TtC6AppKit…24NSSubduedGlassEffectView`).
    func isClass(named name: String) -> Bool {
        className == name || String(describing: type(of: self)) == name
    }

    func firstDescendant(className: String) -> NSView? {
        for sub in subviews {
            if sub.isClass(named: className) { return sub }
            if let found = sub.firstDescendant(className: className) { return found }
        }
        return nil
    }

    func descendants(className: String) -> [NSView] {
        var out: [NSView] = []
        for sub in subviews {
            if sub.isClass(named: className) { out.append(sub) }
            out += sub.descendants(className: className)
        }
        return out
    }

    func firstAncestor(className: String) -> NSView? {
        var v = superview
        while let s = v {
            if s.isClass(named: className) { return s }
            v = s.superview
        }
        return nil
    }
}


extension NSColor {
    /// 0xRRGGBB of this color in sRGB.
    var rgb: UInt32 {
        guard let c = usingColorSpace(.sRGB) else { return 0 }
        let v = { (x: CGFloat) in UInt32(max(0, min(255, (x * 255).rounded()))) }
        return v(c.redComponent) << 16 | v(c.greenComponent) << 8 | v(c.blueComponent)
    }
}
