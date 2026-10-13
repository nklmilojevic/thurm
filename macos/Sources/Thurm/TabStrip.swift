import AppKit

/// Horizontal tabs (`window.tab_style = "native"`): a strip in the titlebar row in place of
/// AppKit's tab bar. The workspace on the left, then the window's tabs as pills in a track (icon
/// tile, title, close on hover), the command palette and New Tab on the right. The tabs are
/// still the window's native tab group, so Cmd+1…9, the Window menu and restoring work as before.
final class TabStrip: NSView {
    weak var controller: TerminalWindowController?

    private let workspace = WorkspaceBlock()
    private let track = TabTrack()
    private let palette = TabStrip.button("command", "Command Palette (⇧⌘P)", #selector(AppDelegate.showCommandPalette(_:)))
    private let newTab = TabStrip.button("plus", "New Tab (⌘T)", #selector(AppDelegate.newTab(_:)))

    static let height: CGFloat = 32

    init() {
        super.init(frame: .zero)
        for v in [workspace, track, palette, newTab] as [NSView] {
            addSubview(v)
        }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }
    /// Empty space drags the window, and a double-click there zooms it, as in a titlebar.
    override var mouseDownCanMoveWindow: Bool { true }

    private static func button(_ symbol: String, _ tip: String, _ action: Selector) -> NSButton {
        let b = NSButton(image: NSImage(systemSymbolName: symbol, accessibilityDescription: tip)!, target: nil,
                         action: action)
        b.isBordered = false
        b.symbolConfiguration = .init(pointSize: 14, weight: .regular)
        b.toolTip = tip
        return b
    }

    override func layout() {
        super.layout()
        let h = bounds.height
        let button: CGFloat = 28
        newTab.frame = NSRect(x: bounds.maxX - button, y: (h - button) / 2, width: button, height: button)
        palette.frame = newTab.frame.offsetBy(dx: -button - 4, dy: 0)
        // The buttons keep their room; the workspace takes at most a third of the rest, shows
        // just its icon when that is tight, and gives way to the tabs when there is no room.
        let room = max(0, palette.frame.minX - 12)
        workspace.compact = room < 360
        let ws = workspace.fittingSize
        let width = room < 160 ? 0 : workspace.compact ? 28 : min(ws.width, 180, room / 3)
        workspace.isHidden = width == 0
        workspace.frame = NSRect(x: 0, y: (h - ws.height) / 2, width: width, height: ws.height)
        let left = width > 0 ? width + 14 : 0
        track.frame = NSRect(x: left, y: 0, width: max(0, room - left), height: h)
    }

    /// Re-reads the window's tabs, the workspace and the theme.
    func reload() {
        guard let controller, let window = controller.window else { return }
        let sm = SessionManager.shared
        let fg = colorFromRGB(sm.config.theme.foreground)
        for b in [palette, newTab] { b.contentTintColor = fg.withAlphaComponent(0.6) }
        workspace.update(name: sm.workspace(controller.workspaceID)?.name ?? "Workspace",
                         host: controller.host, fg: fg)
        let selected = window.tabGroup?.selectedWindow ?? window
        let panes = sm.panes
        track.tabs = controller.orderedTabs.compactMap { w in
            guard let c = w.windowController as? TerminalWindowController, !c.isClosed else { return nil }
            let info = panes[c.focusedPane]
            var status: AgentStatus?
            for id in c.content.paneIds {
                if let a = panes[id]?.agent, status == nil || a.status.urgency > status!.urgency { status = a.status }
            }
            let base = c.titleOverride ?? info?.displayTitle ?? "Thurm"
            return TabTrack.Tab(window: w,
                                title: c.host == localHost ? base : "\(c.host) · \(base)",
                                icon: PaneIcon(info),
                                stacked: c.content.paneIds.count > 1,
                                status: status,
                                selected: w === selected)
        }
        needsLayout = true
    }
}

/// The workspace, as Rex shows the session: an icon over its name and where it runs. Opens the
/// workspace menu.
private final class WorkspaceBlock: NSView {
    private let icon = NSImageView()
    private let name = NSTextField(labelWithString: "")
    private let detail = NSTextField(labelWithString: "")
    private let workspaceMenu = NSMenu()

    init() {
        super.init(frame: .zero)
        icon.symbolConfiguration = .init(pointSize: 15, weight: .regular)
        name.font = .systemFont(ofSize: 13, weight: .semibold)
        detail.font = .systemFont(ofSize: 11)
        for f in [name, detail] {
            f.lineBreakMode = .byTruncatingTail
            f.maximumNumberOfLines = 1
            f.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        }
        for v in [icon, name, detail] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        NSLayoutConstraint.activate([
            icon.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 4),
            icon.centerYAnchor.constraint(equalTo: centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: 20),
            name.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 8),
            name.topAnchor.constraint(equalTo: topAnchor),
            name.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor),
            detail.leadingAnchor.constraint(equalTo: name.leadingAnchor),
            detail.topAnchor.constraint(equalTo: name.bottomAnchor, constant: -1),
            detail.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor),
            detail.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
        toolTip = "Workspaces"
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    /// Just the icon, when the strip is short of room.
    var compact = false {
        didSet {
            name.isHidden = compact
            detail.isHidden = compact
        }
    }

    func update(name text: String, host: HostId, fg: NSColor) {
        let remote = host != localHost
        let symbol = remote ? "globe" : "laptopcomputer"
        if icon.image?.accessibilityDescription != symbol {
            icon.image = NSImage(systemSymbolName: symbol, accessibilityDescription: symbol)
        }
        icon.contentTintColor = fg.withAlphaComponent(0.55)
        if name.stringValue != text { name.stringValue = text }
        let where_ = remote ? host : "This Mac"
        if detail.stringValue != where_ { detail.stringValue = where_ }
        name.textColor = fg.withAlphaComponent(0.92)
        detail.textColor = fg.withAlphaComponent(0.5)
    }

    override func mouseDown(with event: NSEvent) {
        SessionManager.shared.fillWorkspaceMenu(workspaceMenu)
        workspaceMenu.popUp(positioning: nil, at: NSPoint(x: 0, y: bounds.maxY + 6), in: self)
    }
}

/// The tabs: equal-width pills in a faint rounded track, the selected one raised, thin
/// separators between the others. A pill dragged sideways moves its tab. Pills shrink to fit
/// (to just the icon when narrow); tabs that still do not fit go into an overflow menu, the
/// selected one always staying in view.
private final class TabTrack: NSView {
    struct Tab {
        weak var window: NSWindow?
        var title: String
        var icon: PaneIcon
        var stacked: Bool
        var status: AgentStatus?
        var selected: Bool
    }

    var tabs: [Tab] = [] {
        didSet { sync() }
    }
    private var pills: [TabPill] = []
    /// Indices into `tabs` of the pills in view, in order; the rest are in the overflow menu.
    private var shown: [Int] = []
    private let overflow = NSButton()
    private let overflowMenu = NSMenu()

    static let minPill: CGFloat = 40
    static let maxPill: CGFloat = 260
    static let overflowWidth: CGFloat = 28

    override var isFlipped: Bool { true }
    override var mouseDownCanMoveWindow: Bool { true }

    /// The pill being dragged, its slot among the shown pills, where the pointer holds it, and
    /// the slot it would drop into.
    private var drag: (pill: TabPill, from: Int, grabX: CGFloat, to: Int)?

    init() {
        super.init(frame: .zero)
        overflow.image = NSImage(systemSymbolName: "chevron.down", accessibilityDescription: "More Tabs")
        overflow.isBordered = false
        overflow.symbolConfiguration = .init(pointSize: 10, weight: .semibold)
        overflow.toolTip = "More Tabs"
        overflow.target = self
        overflow.action = #selector(showOverflow(_:))
        overflow.isHidden = true
        addSubview(overflow)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    private func sync() {
        if let d = drag, !pills.contains(where: { $0 === d.pill }) { drag = nil }
        while pills.count > tabs.count { pills.removeLast().removeFromSuperview() }
        while pills.count < tabs.count {
            let p = TabPill()
            addSubview(p, positioned: .below, relativeTo: overflow)
            pills.append(p)
        }
        for (p, t) in zip(pills, tabs) { p.tab = t }
        needsLayout = true
        needsDisplay = true
    }

    /// Which tabs are in view at this width, and the width of their pills.
    private func fit() -> (shown: [Int], width: CGFloat, overflows: Bool) {
        let avail = bounds.width - 6
        guard !tabs.isEmpty, avail > 0 else { return ([], 0, false) }
        let all = Array(tabs.indices)
        if CGFloat(tabs.count) * Self.minPill <= avail {
            return (all, min(Self.maxPill, avail / CGFloat(tabs.count)), false)
        }
        let count = max(1, Int((avail - Self.overflowWidth) / Self.minPill))
        var picked = Array(all.prefix(count))
        if let sel = tabs.firstIndex(where: \.selected), !picked.contains(sel) {
            picked[picked.count - 1] = sel
        }
        return (picked, max(1, (avail - Self.overflowWidth) / CGFloat(picked.count)), true)
    }

    private var pillWidth: CGFloat = 0

    override func layout() {
        super.layout()
        let (picked, w, overflows) = fit()
        shown = picked
        pillWidth = w
        let h = max(0, bounds.height - 6)
        for (i, p) in pills.enumerated() where !picked.contains(i) {
            p.isHidden = true
        }
        let order = shown.map { pills[$0] }
        if let d = drag {
            // The others close up around the slot the dragged pill would drop into.
            var slot = 0
            for p in order where p !== d.pill {
                if slot == d.to { slot += 1 }
                p.isHidden = false
                p.frame = NSRect(x: 3 + CGFloat(slot) * w, y: 3, width: w, height: h)
                slot += 1
            }
        } else {
            for (slot, p) in order.enumerated() {
                p.isHidden = false
                p.frame = NSRect(x: 3 + CGFloat(slot) * w, y: 3, width: w, height: h)
            }
        }
        overflow.isHidden = !overflows
        overflow.frame = NSRect(x: 3 + CGFloat(order.count) * w, y: 3, width: Self.overflowWidth, height: h)
        let fg = colorFromRGB(SessionManager.shared.config.theme.foreground)
        overflow.contentTintColor = fg.withAlphaComponent(0.6)
    }

    @objc private func showOverflow(_ sender: NSButton) {
        overflowMenu.removeAllItems()
        for (i, t) in tabs.enumerated() where !shown.contains(i) {
            let item = NSMenuItem(title: t.title, action: #selector(selectFromMenu(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = t.window
            item.image = t.icon.tile(size: 16, stacked: t.stacked, badge: t.status == .idle ? nil : t.status)
            overflowMenu.addItem(item)
        }
        overflowMenu.popUp(positioning: nil, at: NSPoint(x: 0, y: sender.bounds.maxY + 6), in: sender)
    }

    @objc private func selectFromMenu(_ item: NSMenuItem) {
        guard let target = item.representedObject as? NSWindow else { return }
        target.makeKeyAndOrderFront(nil)
        if let c = target.windowController as? TerminalWindowController, let view = c.content.focusedView {
            target.makeFirstResponder(view)
        }
    }

    // MARK: Dragging a tab

    /// Mouse moved with the button down on `pill`: past a few points, it follows the pointer.
    func dragged(_ pill: TabPill, event: NSEvent, downAt start: NSPoint) {
        let p = convert(event.locationInWindow, from: nil)
        if drag == nil {
            guard abs(p.x - start.x) > 4,
                  let from = shown.firstIndex(where: { $0 < pills.count && pills[$0] === pill }) else { return }
            drag = (pill, from, start.x - pill.frame.minX, from)
            addSubview(pill, positioned: .below, relativeTo: overflow)
            needsDisplay = true
        }
        guard var d = drag, pillWidth > 0 else { return }
        let w = pillWidth
        let x = min(max(p.x - d.grabX, 3), 3 + CGFloat(shown.count - 1) * w)
        pill.frame.origin.x = x
        let to = min(max(Int(((x - 3) / w).rounded()), 0), shown.count - 1)
        if to != d.to {
            d.to = to
            drag = d
            NSAnimationContext.runAnimationGroup { ctx in
                ctx.duration = 0.15
                ctx.allowsImplicitAnimation = true
                layoutSubtreeIfNeeded()
                needsLayout = true
                layoutSubtreeIfNeeded()
            }
        }
    }

    /// Button released: a dragged pill's tab moves to the place in the window's tab group of the
    /// tab whose slot it took.
    func dropped(_ pill: TabPill) {
        guard let d = drag, d.pill === pill else { return }
        drag = nil
        needsDisplay = true
        if d.to != d.from, d.to < shown.count, let w = pill.tab?.window, let group = w.tabGroup {
            group.insertWindow(w, at: shown[d.to])
        }
        NSAnimationContext.runAnimationGroup { ctx in
            ctx.duration = 0.15
            ctx.allowsImplicitAnimation = true
            needsLayout = true
            layoutSubtreeIfNeeded()
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        let chrome = SessionManager.shared.config.chrome
        // The track.
        let r = bounds.height / 2
        chrome.control.setFill()
        NSBezierPath(roundedRect: bounds, xRadius: r, yRadius: r).fill()
        // Separators between neighbours that are both unselected (none while dragging, nor
        // between a change of tabs and the layout that follows it).
        guard drag == nil, shown.allSatisfy({ $0 < tabs.count && $0 < pills.count }) else { return }
        chrome.strokeStrong.setFill()
        for slot in 1..<max(1, shown.count) where !tabs[shown[slot]].selected && !tabs[shown[slot - 1]].selected {
            let x = pills[shown[slot]].frame.minX
            NSRect(x: x - 0.5, y: bounds.midY - 8, width: 1, height: 16).fill()
        }
    }
}

/// One tab: icon tile (stacked for a split tab, with the agent status as a badge), title, and a
/// close button on hover. Click selects it, a middle click closes it.
private final class TabPill: NSView {
    var tab: TabTrack.Tab? {
        didSet { update() }
    }
    private let icon = NSImageView()
    private let title = NSTextField(labelWithString: "")
    private let close = NSButton()
    private lazy var iconLeading = icon.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 10)
    /// Too narrow for a title: just the icon, centered.
    private var compact: Bool { bounds.width < 80 }
    private var hovering = false {
        didSet { if hovering != oldValue { update() } }
    }

    init() {
        super.init(frame: .zero)
        title.font = .systemFont(ofSize: 13)
        title.lineBreakMode = .byTruncatingTail
        title.maximumNumberOfLines = 1
        close.image = NSImage(systemSymbolName: "xmark", accessibilityDescription: "Close Tab")
        close.isBordered = false
        close.symbolConfiguration = .init(pointSize: 9, weight: .semibold)
        close.target = self
        close.action = #selector(closeTab)
        close.toolTip = "Close Tab"
        for v in [icon, title, close] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        title.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        NSLayoutConstraint.activate([
            iconLeading,
            icon.centerYAnchor.constraint(equalTo: centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: 26),
            icon.heightAnchor.constraint(equalToConstant: 22),
            title.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 6),
            title.centerYAnchor.constraint(equalTo: centerYAnchor),
            title.trailingAnchor.constraint(lessThanOrEqualTo: close.leadingAnchor, constant: -4),
            close.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            close.centerYAnchor.constraint(equalTo: centerYAnchor),
            close.widthAnchor.constraint(equalToConstant: 16),
        ])
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect],
                                       owner: self))
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    override func setFrameSize(_ newSize: NSSize) {
        let was = compact
        super.setFrameSize(newSize)
        if compact != was || compact { update() }
    }

    private func update() {
        guard let tab else { return }
        let chrome = SessionManager.shared.config.chrome
        icon.image = tab.icon.tile(size: 20, stacked: tab.stacked, badge: tab.status == .idle ? nil : tab.status)
        if title.stringValue != tab.title { title.stringValue = tab.title }
        title.textColor = tab.selected ? chrome.contentPrimary : chrome.contentSecondary
        close.contentTintColor = chrome.contentSecondary
        title.isHidden = compact
        close.isHidden = !hovering || compact
        iconLeading.constant = compact ? max(0, (bounds.width - 26) / 2) : 10
        toolTip = tab.title
        needsDisplay = true
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let tab, tab.selected || hovering else { return }
        let chrome = SessionManager.shared.config.chrome
        let r = bounds.height / 2
        let path = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5), xRadius: r - 0.5, yRadius: r - 0.5)
        (tab.selected ? chrome.controlSelected : chrome.control).setFill()
        path.fill()
        if tab.selected {
            chrome.stroke.setStroke()
            path.lineWidth = ChromeStroke.width
            path.stroke()
        }
    }

    override func mouseEntered(with event: NSEvent) { hovering = true }
    override func mouseExited(with event: NSEvent) { hovering = false }

    /// Where the button went down, in the track: the start of a possible drag.
    private var downAt: NSPoint?

    /// The pill handles its own drags (moving the tab), not the window's.
    override var mouseDownCanMoveWindow: Bool { false }

    // Each tab is its own window with its own strip, so selecting one hides this strip: select
    // on release, after any drag, or the drag would lose its strip halfway.
    override func mouseDown(with event: NSEvent) {
        downAt = superview?.convert(event.locationInWindow, from: nil)
    }

    override func mouseDragged(with event: NSEvent) {
        guard let downAt, let track = superview as? TabTrack else { return }
        track.dragged(self, event: event, downAt: downAt)
    }

    override func mouseUp(with event: NSEvent) {
        guard downAt != nil else { return }
        downAt = nil
        (superview as? TabTrack)?.dropped(self)
        guard let target = tab?.window else { return }
        target.makeKeyAndOrderFront(nil)
        if let c = target.windowController as? TerminalWindowController, let view = c.content.focusedView {
            target.makeFirstResponder(view)
        }
    }

    override func otherMouseDown(with event: NSEvent) {
        if event.buttonNumber == 2 { closeTab() }
    }

    @objc private func closeTab() {
        tab?.window?.performClose(nil)
    }
}
