import AppKit

/// The sidebar's hosts button: list this workspace's tabs as before, every host's tabs under
/// the host, or one host's (`window.sidebar_by_host`, `window.sidebar_host`).
final class SidebarHostsPopover: NSObject, NSPopoverDelegate {
    static let shared = SidebarHostsPopover()

    /// What a row sets: the list by host on or off, and the one host it lists (nil: every host).
    struct Choice: Equatable {
        let byHost: Bool
        let host: HostId?
    }

    struct Row: Equatable {
        let title: String
        let symbol: String
        var detail: String?
        let choice: Choice
        var checked: Bool
    }

    /// The list as it was first ("This Workspace", or "All Workspaces" with that setting on),
    /// then every host, then each host, This Mac first. The current choice is checked; a filter
    /// naming a host that is gone counts as every host.
    static func rows(byHost: Bool, filter: HostId?, allWorkspaces: Bool, hosts: [HostId],
                     state: (HostId) -> String?) -> [Row] {
        let current = SidebarHosts.effectiveFilter(filter, hosts: hosts)
        var rows = [Row(title: allWorkspaces ? "All Workspaces" : "This Workspace", symbol: "macwindow",
                        choice: Choice(byHost: false, host: filter), checked: !byHost),
                    Row(title: "All Hosts", symbol: "square.stack.3d.up", choice: Choice(byHost: true, host: nil),
                        checked: byHost && current == nil)]
        for h in hosts {
            rows.append(Row(title: SidebarHosts.title(h), symbol: h == localHost ? "laptopcomputer" : "globe",
                            detail: state(h), choice: Choice(byHost: true, host: h), checked: byHost && current == h))
        }
        return rows
    }

    /// A host's state on its row: the sidebar header's ("connecting", "not configured", …), or
    /// "connected"; none for This Mac.
    static func state(_ host: HostId) -> String? {
        host == localHost ? nil : TabSidebarViewController.hostState(host) ?? "connected"
    }

    private var popover: NSPopover?

    func toggle(relativeTo view: NSView) {
        if let p = popover, p.isShown {
            p.performClose(nil)
            return
        }
        let sm = SessionManager.shared
        let config = sm.config
        let hosts = SidebarHosts.hosts(remotes: config.remoteNames, workspaceHosts: sm.workspaces.map(\.host))
        let rows = Self.rows(byHost: config.sidebarByHost, filter: config.sidebarHost,
                             allWorkspaces: config.sidebarAllWorkspaces, hosts: hosts, state: Self.state)
        let p = NSPopover()
        p.behavior = .transient
        p.animates = false
        p.delegate = self
        p.appearance = NSAppearance(named: config.chrome.isDark ? .darkAqua : .aqua)
        p.contentViewController = HostsListController(rows: rows) { [weak p] choice in
            p?.performClose(nil)
            SessionManager.shared.setSidebarHosts(byHost: choice.byHost, host: choice.host)
        }
        popover = p
        p.show(relativeTo: view.bounds, of: view, preferredEdge: .minY)
    }

    func popoverDidClose(_ notification: Notification) {
        popover = nil
    }
}

/// The popover's list: as tall as its rows, but no taller than `maxHeight`, past which it
/// scrolls, so every host stays within reach however many there are.
final class HostsListController: NSViewController {
    private let rows: [SidebarHostsPopover.Row]
    private let choose: (SidebarHostsPopover.Choice) -> Void
    private let maxHeight: CGFloat

    /// Most of the screen's height, leaving room for the window's top above the popover.
    static var screenMaxHeight: CGFloat {
        ((NSScreen.main?.visibleFrame.height ?? 800) * 0.6).rounded(.down)
    }

    init(rows: [SidebarHostsPopover.Row], maxHeight: CGFloat = HostsListController.screenMaxHeight,
         choose: @escaping (SidebarHostsPopover.Choice) -> Void) {
        self.rows = rows
        self.choose = choose
        self.maxHeight = maxHeight
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func loadView() {
        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 0
        stack.edgeInsets = NSEdgeInsets(top: 6, left: 6, bottom: 6, right: 6)
        for (i, row) in rows.enumerated() {
            let v = HostChoiceRow(row) { [weak self] in self?.choose(row.choice) }
            stack.addArrangedSubview(v)
            v.widthAnchor.constraint(equalToConstant: 236).isActive = true
            if i == 0 {
                let line = NSBox()
                line.boxType = .separator
                stack.addArrangedSubview(line)
                stack.setCustomSpacing(4, after: v)
                stack.setCustomSpacing(4, after: line)
                line.widthAnchor.constraint(equalToConstant: 236).isActive = true
            }
        }
        let size = stack.fittingSize
        let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: size.width, height: min(size.height, maxHeight)))
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        // Flipped, so the list opens at its first row.
        let list = TopDownView(frame: NSRect(origin: .zero, size: size))
        stack.frame = list.bounds
        stack.autoresizingMask = [.width, .height]
        list.addSubview(stack)
        scroll.documentView = list
        view = scroll
        preferredContentSize = scroll.frame.size
    }
}

private final class TopDownView: NSView {
    override var isFlipped: Bool { true }
}

/// One choice: icon, title, the host's state, and a checkmark on the current one.
private final class HostChoiceRow: NSView {
    static let height: CGFloat = 26
    private let action: () -> Void
    private let row: SidebarHostsPopover.Row
    private var hovered = false {
        didSet { if hovered != oldValue { needsDisplay = true } }
    }

    init(_ row: SidebarHostsPopover.Row, action: @escaping () -> Void) {
        self.row = row
        self.action = action
        super.init(frame: .zero)
        translatesAutoresizingMaskIntoConstraints = false
        heightAnchor.constraint(equalToConstant: Self.height).isActive = true
        let chrome = SessionManager.shared.config.chrome
        let check = NSImageView(image: NSImage(systemSymbolName: "checkmark", accessibilityDescription: nil) ?? NSImage())
        check.symbolConfiguration = .init(pointSize: 11, weight: .semibold)
        check.contentTintColor = chrome.contentPrimary
        check.isHidden = !row.checked
        let icon = NSImageView(image: NSImage(systemSymbolName: row.symbol, accessibilityDescription: nil) ?? NSImage())
        icon.symbolConfiguration = .init(pointSize: 13, weight: .regular)
        icon.contentTintColor = chrome.contentSecondary
        let title = NSTextField(labelWithString: row.title)
        title.font = .systemFont(ofSize: NSFont.systemFontSize)
        title.textColor = chrome.contentPrimary
        title.lineBreakMode = .byTruncatingTail
        title.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let detail = NSTextField(labelWithString: row.detail ?? "")
        detail.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        detail.textColor = chrome.contentSecondary
        detail.lineBreakMode = .byTruncatingTail
        for v in [check, icon, title, detail] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        NSLayoutConstraint.activate([
            check.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8),
            check.centerYAnchor.constraint(equalTo: centerYAnchor),
            check.widthAnchor.constraint(equalToConstant: 14),
            icon.leadingAnchor.constraint(equalTo: check.trailingAnchor, constant: 6),
            icon.centerYAnchor.constraint(equalTo: centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: 20),
            title.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 6),
            title.centerYAnchor.constraint(equalTo: centerYAnchor),
            detail.leadingAnchor.constraint(greaterThanOrEqualTo: title.trailingAnchor, constant: 8),
            detail.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            detail.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect],
                                       owner: self, userInfo: nil))
        setAccessibilityElement(true)
        setAccessibilityRole(.button)
        setAccessibilityLabel([row.title, row.detail].compactMap { $0 }.joined(separator: ", "))
        setAccessibilitySelected(row.checked)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func draw(_ dirtyRect: NSRect) {
        guard hovered else { return }
        SessionManager.shared.config.chrome.controlSelected.setFill()
        NSBezierPath(roundedRect: bounds, xRadius: 6, yRadius: 6).fill()
    }

    override func mouseDown(with event: NSEvent) {}
    override func mouseEntered(with event: NSEvent) { hovered = true }
    override func mouseExited(with event: NSEvent) { hovered = false }
    override func mouseUp(with event: NSEvent) {
        if bounds.contains(convert(event.locationInWindow, from: nil)) { action() }
    }

    override func accessibilityPerformPress() -> Bool {
        action()
        return true
    }
}
