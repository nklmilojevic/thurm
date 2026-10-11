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
        let ws = workspace.fittingSize
        workspace.frame = NSRect(x: 0, y: (h - ws.height) / 2, width: min(ws.width, 180), height: ws.height)
        let button: CGFloat = 28
        newTab.frame = NSRect(x: bounds.maxX - button, y: (h - button) / 2, width: button, height: button)
        palette.frame = newTab.frame.offsetBy(dx: -button - 4, dy: 0)
        let left = workspace.frame.maxX + 14
        track.frame = NSRect(x: left, y: 0, width: max(0, palette.frame.minX - 12 - left), height: h)
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
/// separators between the others. A pill dragged sideways moves its tab.
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

    override var isFlipped: Bool { true }
    override var mouseDownCanMoveWindow: Bool { true }

    /// The pill being dragged, where it started, and the slot it would drop into.
    private var drag: (pill: TabPill, from: Int, grabX: CGFloat, to: Int)?

    private func sync() {
        if let d = drag, !pills.contains(where: { $0 === d.pill }) { drag = nil }
        while pills.count > tabs.count { pills.removeLast().removeFromSuperview() }
        while pills.count < tabs.count {
            let p = TabPill()
            addSubview(p)
            pills.append(p)
        }
        for (p, t) in zip(pills, tabs) { p.tab = t }
        needsLayout = true
        needsDisplay = true
    }

    private var pillWidth: CGFloat {
        guard !tabs.isEmpty else { return 0 }
        return min(260, max(90, (bounds.width - 6) / CGFloat(tabs.count)))
    }

    override func layout() {
        super.layout()
        let w = pillWidth
        let h = max(0, bounds.height - 6)
        guard let d = drag else {
            for (i, p) in pills.enumerated() {
                p.frame = NSRect(x: 3 + CGFloat(i) * w, y: 3, width: w, height: h)
            }
            return
        }
        // The others close up around the slot the dragged pill would drop into.
        var slot = 0
        for p in pills where p !== d.pill {
            if slot == d.to { slot += 1 }
            p.frame = NSRect(x: 3 + CGFloat(slot) * w, y: 3, width: w, height: h)
            slot += 1
        }
    }

    // MARK: Dragging a tab

    /// Mouse moved with the button down on `pill`: past a few points, it follows the pointer.
    func dragged(_ pill: TabPill, event: NSEvent, downAt start: NSPoint) {
        let p = convert(event.locationInWindow, from: nil)
        if drag == nil {
            guard abs(p.x - start.x) > 4, let from = pills.firstIndex(where: { $0 === pill }) else { return }
            drag = (pill, from, start.x - pill.frame.minX, from)
            addSubview(pill, positioned: .above, relativeTo: nil)
            needsDisplay = true
        }
        guard var d = drag else { return }
        let w = pillWidth
        let x = min(max(p.x - d.grabX, 3), 3 + CGFloat(pills.count - 1) * w)
        pill.frame.origin.x = x
        let to = min(max(Int(((x - 3) / w).rounded()), 0), pills.count - 1)
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

    /// Button released: a dragged pill's tab moves to its slot in the window's tab group.
    func dropped(_ pill: TabPill) {
        guard let d = drag, d.pill === pill else { return }
        drag = nil
        needsDisplay = true
        if d.to != d.from, let w = pill.tab?.window, let group = w.tabGroup {
            group.insertWindow(w, at: d.to)
        }
        NSAnimationContext.runAnimationGroup { ctx in
            ctx.duration = 0.15
            ctx.allowsImplicitAnimation = true
            needsLayout = true
            layoutSubtreeIfNeeded()
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        let cfg = SessionManager.shared.config
        let fg = colorFromRGB(cfg.theme.foreground)
        // The track.
        let r = bounds.height / 2
        fg.withAlphaComponent(0.035).setFill()
        NSBezierPath(roundedRect: bounds, xRadius: r, yRadius: r).fill()
        // Separators between neighbours that are both unselected (none while dragging).
        guard drag == nil else { return }
        fg.withAlphaComponent(0.14).setFill()
        for i in 1..<max(1, pills.count) where !tabs[i].selected && !tabs[i - 1].selected {
            let x = pills[i].frame.minX
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
            icon.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 10),
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

    private func update() {
        guard let tab else { return }
        let fg = colorFromRGB(SessionManager.shared.config.theme.foreground)
        icon.image = tab.icon.tile(size: 20, stacked: tab.stacked, badge: tab.status == .idle ? nil : tab.status)
        if title.stringValue != tab.title { title.stringValue = tab.title }
        title.textColor = fg.withAlphaComponent(tab.selected ? 0.95 : 0.55)
        close.contentTintColor = fg.withAlphaComponent(0.55)
        close.isHidden = !hovering
        toolTip = tab.title
        needsDisplay = true
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let tab, tab.selected || hovering else { return }
        let cfg = SessionManager.shared.config
        let fg = colorFromRGB(cfg.theme.foreground)
        let r = bounds.height / 2
        let path = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5), xRadius: r - 0.5, yRadius: r - 0.5)
        fg.withAlphaComponent(tab.selected ? 0.07 : 0.035).setFill()
        path.fill()
        if tab.selected {
            fg.withAlphaComponent(0.08).setStroke()
            path.lineWidth = 1
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
