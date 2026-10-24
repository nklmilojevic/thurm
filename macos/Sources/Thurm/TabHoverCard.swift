import AppKit

// A card about a tab, shown while the pointer rests on it in the sidebar or the tab strip: a
// map of its splits, tinted by what each pane's agent is doing, and a line or three per pane
// (what runs there, its status in words, its progress, where it is). It covers the tabs you
// are not looking at; the selected tab is on screen already. The card never takes the
// keyboard, and updates while it is up. The pointer can move onto it, and a click on a pane in
// the map or the list goes to that pane.

/// One pane as the card shows it.
struct HoverCardPane: Equatable {
    let key: PaneKey
    var icon: PaneIcon
    var title: String
    var status: AgentStatus?
    /// The agent's status in words, or the program running in the pane.
    var detail: String?
    var progress: ProgressReport?
    /// The git branch, else the working directory.
    var place: String?
    var added: UInt32 = 0
    var removed: UInt32 = 0
    var focused: Bool
}

/// A pane's rectangle in the tab's layout, in the unit square (origin top left).
struct HoverCardCell: Equatable {
    let key: PaneKey
    let rect: CGRect
}

/// What the card shows for one tab.
struct HoverCardModel: Equatable {
    var title: String
    var subtitle: String?
    /// Every pane in layout order: the map shows them all.
    var all: [HoverCardPane]
    /// The panes listed, in layout order: at most `maxPanes`, and fewer when the card would not
    /// fit the screen (see `limited(to:)`).
    var panes: [HoverCardPane]
    /// The layout; empty for a tab with one pane.
    var cells: [HoverCardCell]

    /// Panes in the tab the card doesn't list.
    var more: Int { all.count - panes.count }

    static let maxPanes = 6

    static func make(root: SplitNode, focused: PaneKey, zoomed: PaneKey?, titleOverride: String?, host: HostId,
                     panes: [PaneKey: PaneInfo]) -> HoverCardModel {
        let keys = root.panes
        let front = panes[focused]
        let rows = keys.map { key -> HoverCardPane in
            pane(key, info: panes[key], focused: key == focused, remote: host != localHost)
        }
        var parts = [keys.count > 1 ? "\(keys.count) panes" : nil, host == localHost ? nil : host]
        if let z = zoomed, keys.count > 1, let row = rows.first(where: { $0.key == z }) {
            parts.append("\(row.title) zoomed")
        }
        let subtitle = parts.compactMap { $0 }.joined(separator: " · ")
        return HoverCardModel(
            title: titleOverride ?? front?.displayTitle ?? "Thurm",
            subtitle: subtitle.isEmpty ? nil : subtitle,
            all: rows,
            panes: rows,
            cells: keys.count > 1 ? cells(root) : []
        ).limited(to: maxPanes)
    }

    /// The same card listing at most `count` panes: the most urgent, in layout order, so a
    /// waiting agent never falls off the end of the list.
    func limited(to count: Int) -> HoverCardModel {
        let keep = all.enumerated().sorted {
            let (a, b) = ($0.element.status?.urgency ?? -1, $1.element.status?.urgency ?? -1)
            return a != b ? a > b : $0.offset < $1.offset
        }.prefix(max(0, count)).map(\.offset)
        var model = self
        model.panes = all.enumerated().filter { keep.contains($0.offset) }.map(\.element)
        return model
    }

    /// The layout in the unit square. `SplitNode.frames` rounds to whole points, so it lays
    /// out a large square that is then scaled down.
    private static func cells(_ root: SplitNode) -> [HoverCardCell] {
        let side: CGFloat = 1000
        return root.frames(in: CGRect(x: 0, y: 0, width: side, height: side), gap: 0).map { key, r in
            HoverCardCell(key: key, rect: CGRect(x: r.minX / side, y: r.minY / side,
                                                 width: r.width / side, height: r.height / side))
        }
    }

    private static func pane(_ key: PaneKey, info: PaneInfo?, focused: Bool, remote: Bool) -> HoverCardPane {
        var row = HoverCardPane(key: key, icon: PaneIcon(info), title: info?.displayTitle ?? "Thurm", focused: focused)
        guard let info else { return row }
        if let agent = info.agent {
            let a = AgentPane(pane: key, info: info, agent: agent, controller: nil, hidden: nil)
            row.title = a.title
            row.status = agent.status
            row.detail = a.statusDetail
        } else if !info.alive {
            row.detail = "Exited"
        } else if info.passwordInput {
            row.detail = "Waiting for a password"
        } else if info.hasRunningProcess, let name = info.foregroundName {
            let program = info.program?.name ?? name
            row.detail = row.title.localizedCaseInsensitiveContains(program) ? nil : "Running \(program)"
        }
        row.progress = info.progress
        if let git = info.git {
            row.place = git.branch
            row.added = git.added
            row.removed = git.removed
        } else if let cwd = info.cwd {
            let dir = remote ? cwd : abbreviatePath(cwd)
            // The directory, unless the title already says the same.
            row.place = dir == row.title || row.title.hasSuffix(dir) ? nil : dir
        }
        return row
    }
}

/// The tab's splits in miniature: each pane a rounded cell with its icon, the cell tinted by
/// its agent's status, the focused one outlined.
final class HoverCardMinimap: NSView {
    var model: HoverCardModel? {
        didSet { if model != oldValue { invalidateIntrinsicContentSize(); needsDisplay = true } }
    }
    /// The tab's content size, for the map's proportions.
    var aspect: CGFloat = 0.6 {
        didSet { if aspect != oldValue { invalidateIntrinsicContentSize() } }
    }
    /// The pane under the pointer, in the map or the list: its cell is lit.
    var hovered: PaneKey? {
        didSet { if hovered != oldValue { needsDisplay = true } }
    }

    static let width: CGFloat = 276
    static let gap: CGFloat = 3

    override var isFlipped: Bool { true }

    override var intrinsicContentSize: NSSize {
        NSSize(width: Self.width, height: (Self.width * aspect).rounded().clamped(to: 56...120))
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let model else { return }
        let chrome = SessionManager.shared.config.chrome
        let statuses = Dictionary(model.all.map { ($0.key, $0) }, uniquingKeysWith: { a, _ in a })
        for cell in model.cells {
            let r = rect(of: cell)
            let path = NSBezierPath(roundedRect: r, xRadius: 4, yRadius: 4)
            let pane = statuses[cell.key]
            let tint = pane?.status.flatMap { $0 == .idle ? nil : $0.color }
            let lit = cell.key == hovered
            (tint?.withAlphaComponent(lit ? 0.35 : 0.2) ?? (lit ? chrome.strokeStrong : chrome.control)).setFill()
            path.fill()
            if pane?.focused == true || tint != nil {
                (tint ?? chrome.strokeStrong).withAlphaComponent(tint == nil ? 1 : 0.7).setStroke()
                path.lineWidth = 1
                path.stroke()
            }
            // The icon, when the cell has room for it.
            let side: CGFloat = 13
            guard let pane, r.width > side + 6, r.height > side + 6 else { continue }
            let glyph = pane.icon.glyph(size: side)
            let iconRect = CGRect(x: r.midX - side / 2, y: r.midY - side / 2, width: side, height: side)
            let tinted = NSImage(size: glyph.size, flipped: false) { rect in
                glyph.draw(in: rect)
                (tint ?? chrome.contentSecondary).set()
                rect.fill(using: .sourceAtop)
                return true
            }
            tinted.draw(in: iconRect, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
        }
    }

    /// The cell as drawn: its share of the map, less half the gap on each side.
    private func rect(of cell: HoverCardCell) -> CGRect {
        CGRect(x: bounds.minX + cell.rect.minX * bounds.width, y: bounds.minY + cell.rect.minY * bounds.height,
               width: cell.rect.width * bounds.width, height: cell.rect.height * bounds.height)
            .insetBy(dx: Self.gap / 2, dy: Self.gap / 2)
    }

    /// The pane whose drawn cell holds `point`, in the map's coordinates; none in the gaps.
    func pane(at point: NSPoint) -> PaneKey? {
        model?.cells.first { rect(of: $0).contains(point) }?.key
    }
}

/// The card's content: the header, the minimap, and a section per pane.
final class HoverCardView: NSView {
    private let stack = NSStackView()
    let minimap = HoverCardMinimap()
    /// Each listed pane's section.
    private var sections: [(key: PaneKey, view: NSView)] = []
    /// The pane under the button when it went down.
    private var pressed: PaneKey?
    /// A pane was clicked.
    var onSelect: ((PaneKey) -> Void)?
    /// The pointer entered or left the card.
    var onHover: ((Bool) -> Void)?

    static let width: CGFloat = 300
    static let inset: CGFloat = 12

    init() {
        super.init(frame: NSRect(x: 0, y: 0, width: Self.width, height: 100))
        wantsLayer = true
        layer?.cornerRadius = ChromeRadius.window
        layer?.borderWidth = ChromeStroke.width
        layer?.masksToBounds = true
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            widthAnchor.constraint(equalToConstant: Self.width),
            stack.topAnchor.constraint(equalTo: topAnchor, constant: Self.inset),
            stack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: Self.inset),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -Self.inset),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -Self.inset),
        ])
        // The card's window is never key: track the pointer always.
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.activeAlways, .inVisibleRect, .mouseEnteredAndExited,
                                                              .mouseMoved], owner: self))
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    /// The pane under `point`, in the card's coordinates: a cell in the map or a section in the list.
    func pane(at point: NSPoint) -> PaneKey? {
        if minimap.superview != nil, let key = minimap.pane(at: minimap.convert(point, from: self)) { return key }
        return sections.first { $0.view.convert($0.view.bounds, to: self).contains(point) }?.key
    }

    private func track(_ event: NSEvent) {
        let key = pane(at: convert(event.locationInWindow, from: nil))
        minimap.hovered = key
        (key == nil ? NSCursor.arrow : NSCursor.pointingHand).set()
    }

    override func mouseEntered(with event: NSEvent) {
        track(event)
        onHover?(true)
    }

    override func mouseMoved(with event: NSEvent) {
        track(event)
    }

    override func mouseExited(with event: NSEvent) {
        minimap.hovered = nil
        NSCursor.arrow.set()
        onHover?(false)
    }

    override func mouseDown(with event: NSEvent) {
        pressed = pane(at: convert(event.locationInWindow, from: nil))
    }

    override func mouseUp(with event: NSEvent) {
        defer { pressed = nil }
        guard let pressed, pane(at: convert(event.locationInWindow, from: nil)) == pressed else { return }
        onSelect?(pressed)
    }

    func show(_ model: HoverCardModel, aspect: CGFloat) {
        let chrome = SessionManager.shared.config.chrome
        layer?.backgroundColor = chrome.surfaceRaised.cgColor
        layer?.borderColor = chrome.strokeStrong.cgColor
        for v in stack.arrangedSubviews {
            stack.removeArrangedSubview(v)
            v.removeFromSuperview()
        }
        sections = []
        let contentWidth = Self.width - 2 * Self.inset

        let header = NSStackView()
        header.orientation = .vertical
        header.alignment = .leading
        header.spacing = 1
        header.addArrangedSubview(label(model.title, size: 13, weight: .semibold, color: chrome.contentPrimary,
                                        width: contentWidth))
        if let subtitle = model.subtitle {
            header.addArrangedSubview(label(subtitle, size: 11, color: chrome.contentSecondary, width: contentWidth))
        }
        stack.addArrangedSubview(header)

        if !model.cells.isEmpty {
            minimap.aspect = aspect
            minimap.model = model
            if let key = minimap.hovered, !model.all.contains(where: { $0.key == key }) { minimap.hovered = nil }
            minimap.needsDisplay = true
            stack.addArrangedSubview(minimap)
        }

        for pane in model.panes {
            let view = section(pane, chrome: chrome, width: contentWidth)
            sections.append((pane.key, view))
            stack.addArrangedSubview(view)
        }
        if model.more > 0 {
            stack.addArrangedSubview(label("\(model.more) more \(model.more == 1 ? "pane" : "panes")", size: 11,
                                           color: chrome.contentTertiary, width: contentWidth))
        }
        layoutSubtreeIfNeeded()
    }

    private func section(_ pane: HoverCardPane, chrome: ChromePalette, width: CGFloat) -> NSView {
        let iconSide: CGFloat = 15
        let textWidth = width - iconSide - 8
        let icon = NSImageView(image: pane.icon.glyph(size: iconSide))
        icon.contentTintColor = pane.status.flatMap { $0 == .idle ? nil : $0.color } ?? chrome.contentSecondary
        icon.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            icon.widthAnchor.constraint(equalToConstant: iconSide),
            icon.heightAnchor.constraint(equalToConstant: iconSide),
        ])

        let lines = NSStackView()
        lines.orientation = .vertical
        lines.alignment = .leading
        lines.spacing = 2
        lines.addArrangedSubview(label(pane.title, size: 13, weight: pane.focused ? .medium : .regular,
                                       color: chrome.contentPrimary, width: textWidth))
        if let detail = pane.detail {
            let urgent = pane.status == .needsInput || pane.status == .error
            lines.addArrangedSubview(label(detail, size: 12,
                                           color: urgent ? pane.status!.color : chrome.contentSecondary,
                                           width: textWidth, lines: 3))
        }
        if let progress = pane.progress {
            lines.addArrangedSubview(ProgressLine(progress, width: textWidth))
        }
        if let place = pane.place {
            let para = NSMutableParagraphStyle()
            para.lineBreakMode = .byTruncatingMiddle
            let font = NSFont.monospacedDigitSystemFont(ofSize: 11, weight: .regular)
            let base: [NSAttributedString.Key: Any] = [.foregroundColor: chrome.contentSecondary, .font: font,
                                                      .paragraphStyle: para]
            let s = NSMutableAttributedString(string: place, attributes: base)
            if pane.added > 0 {
                s.append(NSAttributedString(string: "  +\(pane.added)",
                                            attributes: base.merging([.foregroundColor: chrome.success]) { $1 }))
            }
            if pane.removed > 0 {
                s.append(NSAttributedString(string: " −\(pane.removed)",
                                            attributes: base.merging([.foregroundColor: chrome.danger]) { $1 }))
            }
            let field = NSTextField(labelWithAttributedString: s)
            field.maximumNumberOfLines = 1
            field.preferredMaxLayoutWidth = textWidth
            field.widthAnchor.constraint(lessThanOrEqualToConstant: textWidth).isActive = true
            lines.addArrangedSubview(field)
        }

        let row = NSStackView(views: [icon, lines])
        row.orientation = .horizontal
        row.alignment = .top
        row.spacing = 8
        // The whole width, so a click beside a short title still lands on the pane.
        row.widthAnchor.constraint(equalToConstant: width).isActive = true
        return row
    }

    private func label(_ text: String, size: CGFloat, weight: NSFont.Weight = .regular, color: NSColor,
                       width: CGFloat, lines: Int = 1) -> NSTextField {
        let field = NSTextField(wrappingLabelWithString: text)
        field.font = .systemFont(ofSize: size, weight: weight)
        field.textColor = color
        field.maximumNumberOfLines = lines
        field.lineBreakMode = .byTruncatingTail
        field.cell?.truncatesLastVisibleLine = true
        field.preferredMaxLayoutWidth = width
        field.widthAnchor.constraint(lessThanOrEqualToConstant: width).isActive = true
        return field
    }
}

/// A thin progress bar with its percentage (OSC 9;4), colored like the pane's own bar.
private final class ProgressLine: NSView {
    private let report: ProgressReport
    private let width: CGFloat

    init(_ report: ProgressReport, width: CGFloat) {
        self.report = report
        self.width = width
        super.init(frame: .zero)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var intrinsicContentSize: NSSize { NSSize(width: width, height: 9) }

    override func draw(_ dirtyRect: NSRect) {
        let chrome = SessionManager.shared.config.chrome
        let track = CGRect(x: 0, y: (bounds.height - 3) / 2, width: bounds.width, height: 3)
        chrome.control.setFill()
        NSBezierPath(roundedRect: track, xRadius: 1.5, yRadius: 1.5).fill()
        let color: NSColor
        switch report.state {
        case .error: color = chrome.danger
        case .paused: color = chrome.warning
        case .normal, .indeterminate: color = chrome.info
        }
        let fraction = report.state == .indeterminate ? 1 : CGFloat(report.percent ?? 100) / 100
        color.withAlphaComponent(report.state == .indeterminate ? 0.4 : 1).setFill()
        var fill = track
        fill.size.width = max(3, track.width * min(1, max(0, fraction)))
        NSBezierPath(roundedRect: fill, xRadius: 1.5, yRadius: 1.5).fill()
    }
}

/// Shows, moves and hides the one card. Tab rows and pills report the pointer entering and
/// leaving them; the card appears after the pointer rests, and while it is up (or just after)
/// moving to another tab switches it at once.
final class TabHoverCard {
    static let shared = TabHoverCard()

    enum Edge {
        /// Beside a sidebar row.
        case right
        /// Under a tab strip pill.
        case below
    }

    private let panel: NSPanel
    private let card = HoverCardView()
    private weak var anchor: NSView?
    private weak var controller: TerminalWindowController?
    private var edge = Edge.right
    private var pending: DispatchWorkItem?
    /// When the last card went away: a tab entered soon after shows its card without waiting.
    private var hiddenAt: CFTimeInterval = 0
    private var observers: [NSObjectProtocol] = []
    private var refreshPending = false

    static let delay: TimeInterval = 0.5
    static let warm: TimeInterval = 0.4
    static let grace: TimeInterval = 0.1
    static let spacing: CGFloat = 6

    private init() {
        panel = NSPanel(contentRect: card.frame, styleMask: [.borderless, .nonactivatingPanel],
                        backing: .buffered, defer: true)
        panel.isFloatingPanel = true
        panel.hasShadow = true
        panel.backgroundColor = .clear
        panel.isOpaque = false
        panel.level = .popUpMenu
        panel.acceptsMouseMovedEvents = true
        panel.hidesOnDeactivate = true
        panel.contentView = card
        card.onSelect = { [weak self] key in self?.select(key) }
        card.onHover = { [weak self] inside in
            guard let self, let anchor = self.anchor else { return }
            if inside {
                self.pending?.cancel()
                self.pending = nil
            } else {
                self.exit(anchor)
            }
        }
        let center = NotificationCenter.default
        observers.append(center.addObserver(forName: NSApplication.didResignActiveNotification, object: nil,
                                            queue: .main) { [weak self] _ in self?.dismiss() })
        observers.append(center.addObserver(forName: NSWindow.didResignKeyNotification, object: nil,
                                            queue: .main) { [weak self] note in
            if (note.object as? NSWindow) === self?.anchor?.window { self?.dismiss() }
        })
    }

    var isVisible: Bool { panel.isVisible }

    /// The pointer entered `anchor`, a row or pill for `controller`'s tab.
    func enter(_ anchor: NSView, controller: TerminalWindowController?, edge: Edge) {
        pending?.cancel()
        pending = nil
        guard let controller, !controller.isClosed, NSApp.isActive,
              // The selected tab is on screen already.
              controller.window !== anchor.window else {
            if self.anchor !== anchor { hide() }
            return
        }
        self.anchor = anchor
        self.controller = controller
        self.edge = edge
        if panel.isVisible || CACurrentMediaTime() - hiddenAt < Self.warm {
            show()
            return
        }
        let work = DispatchWorkItem { [weak self] in
            self?.pending = nil
            self?.show()
        }
        pending = work
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.delay, execute: work)
    }

    /// The pointer left `anchor`, or the card. The card stays while the pointer is on either.
    func exit(_ anchor: NSView) {
        guard anchor === self.anchor else { return }
        pending?.cancel()
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.anchor === anchor, !self.pointerInside(anchor) else { return }
            self.pending = nil
            self.hide()
        }
        pending = work
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.grace, execute: work)
    }

    private func pointerInside(_ anchor: NSView) -> Bool {
        let mouse = NSEvent.mouseLocation
        if panel.isVisible, panel.frame.contains(mouse) { return true }
        guard let window = anchor.window else { return false }
        let local = anchor.convert(window.convertPoint(fromScreen: mouse), from: nil)
        return anchor.bounds.contains(local)
    }

    /// A pane in the card was clicked: go to it.
    private func select(_ key: PaneKey) {
        dismiss()
        SessionManager.shared.focusPane(key, activate: false)
    }

    /// Hides the card now (a click, a drag, a scroll), and doesn't bring it back until the
    /// pointer enters a tab again.
    func dismiss() {
        pending?.cancel()
        pending = nil
        hide()
        anchor = nil
        hiddenAt = 0
    }

    /// Something in the tabs or a pane changed: redraw the card on the next main-loop turn if
    /// it is up (coalesced; nothing to do while it is hidden).
    func refresh() {
        guard panel.isVisible, !refreshPending else { return }
        refreshPending = true
        DispatchQueue.main.async {
            self.refreshPending = false
            if self.panel.isVisible { self.show() }
        }
    }

    private func show() {
        guard let anchor, let window = anchor.window, let controller, !controller.isClosed,
              let screen = window.screen ?? NSScreen.main else {
            hide()
            return
        }
        let content = controller.content
        let model = HoverCardModel.make(root: content.root, focused: content.focusedPane, zoomed: content.zoomedPane,
                                        titleOverride: controller.titleOverride, host: controller.host,
                                        panes: SessionManager.shared.panes)
        let size = content.bounds.size
        let aspect = size.width > 0 ? size.height / size.width : 0.6
        panel.appearance = NSAppearance(named: SessionManager.shared.config.theme.isDark ? .darkAqua : .aqua)
        let rect = window.convertToScreen(anchor.convert(anchor.bounds, to: nil))
        let visible = screen.visibleFrame
        // Under a pill the card has the space below the strip; beside a row, the screen's height.
        let room = edge == .below ? rect.minY - Self.spacing - visible.minY - 4 : visible.height - 8
        var measured: Int?
        let (listed, height) = Self.fitting(listing: model.panes.count, room: room, screen: visible.height - 8) { n in
            card.show(model.limited(to: n), aspect: aspect)
            measured = n
            return card.fittingSize.height
        }
        if measured != listed { card.show(model.limited(to: listed), aspect: aspect) }
        let fit = NSSize(width: card.fittingSize.width, height: height)
        var origin: NSPoint
        switch edge {
        case .right:
            origin = NSPoint(x: rect.maxX + Self.spacing, y: rect.maxY - fit.height)
        case .below:
            origin = NSPoint(x: rect.minX, y: rect.minY - Self.spacing - fit.height)
        }
        origin.x = min(max(origin.x, visible.minX + 4), visible.maxX - fit.width - 4)
        origin.y = min(max(origin.y, visible.minY + 4), visible.maxY - fit.height - 4)
        panel.setFrame(NSRect(origin: origin, size: fit), display: true)
        // A card that moved to a tab in another window follows that window.
        if panel.parent !== window {
            panel.parent?.removeChildWindow(panel)
            window.addChildWindow(panel, ordered: .above)
        }
        guard !panel.isVisible else { return }
        panel.alphaValue = 0
        panel.orderFront(nil)
        NSAnimationContext.runAnimationGroup { ctx in
            ctx.duration = Motion.duration(Motion.fast)
            panel.animator().alphaValue = 1
        }
    }

    /// How many of `count` panes the card lists, and its height then, where `height(n)` is the
    /// card's height listing `n`. The card doesn't scroll: when it is taller
    /// than `room` it lists fewer panes (the rest are counted, and the map still shows them).
    /// It is never cut off: when even the header and map don't fit `room` (little space under a
    /// pill), it gets the `screen`'s height and may cover the strip.
    static func fitting(listing count: Int, room: CGFloat, screen: CGFloat,
                        height: (Int) -> CGFloat) -> (listed: Int, height: CGFloat) {
        let room = height(0) > room ? max(room, screen) : room
        var listed = count
        var h = height(listed)
        while h > room, listed > 0 {
            listed -= 1
            h = height(listed)
        }
        return (listed, h)
    }

    private func hide() {
        guard panel.isVisible else { return }
        hiddenAt = CACurrentMediaTime()
        panel.parent?.removeChildWindow(panel)
        panel.orderOut(nil)
    }
}

private extension Comparable {
    func clamped(to range: ClosedRange<Self>) -> Self { min(max(self, range.lowerBound), range.upperBound) }
}
