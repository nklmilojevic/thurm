import AppKit

/// A pane lifted out of a tab's splits: a card above them that the user moves and resizes.
/// Its frame is kept as fractions of the tab's area, so cards scale with the window.
struct FloatingCard: Equatable {
    var pane: PaneKey
    var frame: CGRect
    /// Shown over the whole tab.
    var zoomed = false
    /// Placed in cells: its terminal keeps this size, below the size of a card made by hand.
    var cells: CardCells?
    var minimized = false
    /// Where its chip sits while minimized, dragged out of the row of chips: the chip's top left
    /// corner as fractions of the tab's area. `nil` in the row.
    var chip: CGPoint?

    init(pane: PaneKey, frame: CGRect, zoomed: Bool = false, cells: CardCells? = nil, minimized: Bool = false,
         chip: CGPoint? = nil) {
        self.pane = pane
        self.frame = frame
        self.zoomed = zoomed
        self.cells = cells
        self.minimized = minimized
        self.chip = chip
    }

    var minSize: CGSize { cells == nil ? FloatingGeometry.minSize : FloatingGeometry.exactMinSize }

    init(_ saved: FloatingPane) {
        let f = saved.frame.count == 4 ? saved.frame.map { CGFloat($0) } : {
            let d = FloatingGeometry.defaultFraction(index: 0)
            return [d.minX, d.minY, d.width, d.height]
        }()
        self.init(pane: saved.pane, frame: CGRect(x: f[0], y: f[1], width: f[2], height: f[3]), zoomed: saved.zoomed,
                  cells: saved.cells, minimized: saved.minimized,
                  chip: saved.chip.map { CGPoint(x: $0[0], y: $0[1]) })
    }

    var saved: FloatingPane {
        FloatingPane(pane: pane, frame: [frame.minX, frame.minY, frame.width, frame.height].map(Double.init),
                     zoomed: zoomed, cells: cells, minimized: minimized,
                     chip: chip.map { [Double($0.x), Double($0.y)] })
    }
}

/// The edges of a card a drag resizes.
struct FloatingEdges: OptionSet, Hashable {
    let rawValue: Int
    static let left = FloatingEdges(rawValue: 1)
    static let right = FloatingEdges(rawValue: 2)
    static let top = FloatingEdges(rawValue: 4)
    static let bottom = FloatingEdges(rawValue: 8)
}

/// Where cards go, in a flipped view (y grows down). Pure, for tests.
enum FloatingGeometry {
    /// A card is never smaller than this, nor larger than its area.
    static let minSize = CGSize(width: 280, height: 160)
    /// The least a card placed in cells keeps to before its terminal's metrics are known: its
    /// header and a row of a few cells. Once known, see `cellMinSize`.
    static let exactMinSize = CGSize(width: 32, height: PaneHeader.height + 20)

    /// The least a card placed in cells keeps to: a terminal of 2 columns by 1 row.
    static func cellMinSize(cell: CGSize, padding: CGPoint) -> CGSize {
        cardSize(CardCells(cols: 2, rows: 1)!, cell: cell, padding: padding)
    }
    /// How far either side of a card's edge a drag resizes it.
    static let band: CGFloat = 5

    /// The card for `fraction` of `area`, at least `minSize` and wholly inside the area.
    static func rect(_ fraction: CGRect, in area: CGRect, minSize: CGSize = FloatingGeometry.minSize) -> CGRect {
        let w = min(max(fraction.width * area.width, minSize.width), area.width)
        let h = min(max(fraction.height * area.height, minSize.height), area.height)
        let x = min(max(area.minX + fraction.minX * area.width, area.minX), area.maxX - w)
        let y = min(max(area.minY + fraction.minY * area.height, area.minY), area.maxY - h)
        return CGRect(x: x, y: y, width: w, height: h).integral
    }

    /// How far a chip dragged out of the row keeps from the tab's edges.
    static let chipInset: CGFloat = 8

    /// The chip of `size` whose top left corner is at `fraction` of `area`, kept inside it.
    static func chipRect(_ fraction: CGPoint, size: CGSize, in area: CGRect) -> CGRect {
        let inner = area.insetBy(dx: chipInset, dy: chipInset)
        let w = min(size.width, max(0, inner.width))
        let x = min(max(area.minX + fraction.x * area.width, inner.minX), max(inner.minX, inner.maxX - w))
        let y = min(max(area.minY + fraction.y * area.height, inner.minY), max(inner.minY, inner.maxY - size.height))
        return CGRect(x: x, y: y, width: w, height: size.height).integral
    }

    /// The top left corner of `rect` as fractions of `area`.
    static func chipFraction(of rect: CGRect, in area: CGRect) -> CGPoint {
        guard area.width > 0, area.height > 0 else { return .zero }
        return CGPoint(x: (rect.minX - area.minX) / area.width, y: (rect.minY - area.minY) / area.height)
    }

    /// `rect` as fractions of `area`.
    static func fraction(of rect: CGRect, in area: CGRect) -> CGRect {
        guard area.width > 0, area.height > 0 else { return rect }
        return CGRect(x: (rect.minX - area.minX) / area.width, y: (rect.minY - area.minY) / area.height,
                      width: rect.width / area.width, height: rect.height / area.height)
    }

    /// The size of a card whose terminal is `cells` (with `cell` and `padding`). The inverse of
    /// `TabContentView.terminalFrame(in:cards:)`: a 1pt border, and the header on top.
    static func cardSize(_ cells: CardCells, cell: CGSize, padding: CGPoint) -> CGSize {
        CGSize(width: CGFloat(cells.cols) * cell.width + 2 * padding.x + 2,
               height: CGFloat(cells.rows) * cell.height + 2 * padding.y + PaneHeader.height + 2)
    }

    /// The cells of a card's terminal `size` (its rectangle with `cardSize`'s border and header).
    static func cells(of size: CGSize, cell: CGSize, padding: CGPoint) -> CardCells? {
        guard cell.width > 0, cell.height > 0 else { return nil }
        return CardCells(cols: max(2, Int(((size.width - 2 - 2 * padding.x) / cell.width).rounded())),
                         rows: max(1, Int(((size.height - 2 - PaneHeader.height - 2 * padding.y) / cell.height).rounded())))
    }

    /// The card whose terminal is `cells`, its first cell (`col`, `row`) cells from the first
    /// cell of a terminal view at `anchor` (both with `cell` and `padding`).
    static func cardRect(anchor: CGPoint, cell: CGSize, padding: CGPoint,
                         col: Int, row: Int, cells: CardCells) -> CGRect {
        let size = cardSize(cells, cell: cell, padding: padding)
        return CGRect(x: anchor.x + CGFloat(col) * cell.width - 1,
                      y: anchor.y + CGFloat(row) * cell.height - PaneHeader.height - 1,
                      width: size.width, height: size.height)
    }

    /// A card placed in cells, in `area`: at `fraction`'s position, sized to `cells` whatever the
    /// area's size, and wholly inside the area.
    static func rect(_ fraction: CGRect, cells: CardCells, cell: CGSize, padding: CGPoint, in area: CGRect) -> CGRect {
        guard area.width > 0, area.height > 0 else { return rect(fraction, in: area, minSize: exactMinSize) }
        let size = cardSize(cells, cell: cell, padding: padding)
        let sized = CGRect(x: fraction.minX, y: fraction.minY,
                           width: size.width / area.width, height: size.height / area.height)
        // Its size comes from its cells (at least 2x1): no minimum on top of that.
        return rect(sized, in: area, minSize: .zero)
    }

    /// The fraction of a new card: centered, cascading by `index` so several stay apart.
    static func defaultFraction(index: Int) -> CGRect {
        let step = CGFloat(index % 6) * 0.03
        return CGRect(x: 0.22 + step, y: 0.18 + step, width: 0.56, height: 0.6)
    }

    static func centered(at point: CGPoint, in area: CGRect, index: Int) -> CGRect {
        let d = defaultFraction(index: index)
        guard area.width > 0, area.height > 0 else { return d }
        let x = (point.x - area.minX) / area.width - d.width / 2
        let y = (point.y - area.minY) / area.height - d.height / 2
        return CGRect(x: min(max(0, x), 1 - d.width), y: min(max(0, y), 1 - d.height),
                      width: d.width, height: d.height)
    }

    /// The edges of `rect` within `band` of `point` (empty: not on its edge).
    static func edges(at point: CGPoint, of rect: CGRect) -> FloatingEdges {
        let outer = rect.insetBy(dx: -band, dy: -band)
        guard outer.contains(point), !rect.insetBy(dx: band, dy: band).contains(point) else { return [] }
        var edges: FloatingEdges = []
        if point.x < rect.minX + band { edges.insert(.left) } else if point.x > rect.maxX - band { edges.insert(.right) }
        if point.y < rect.minY + band { edges.insert(.top) } else if point.y > rect.maxY - band { edges.insert(.bottom) }
        return edges
    }

    /// `rect` with `edges` moved by `delta`, kept at least `minSize` and inside `area`; the
    /// opposite edges hold still.
    static func resized(_ rect: CGRect, edges: FloatingEdges, by delta: CGVector, in area: CGRect,
                        minSize: CGSize = FloatingGeometry.minSize) -> CGRect {
        var (minX, minY, maxX, maxY) = (rect.minX, rect.minY, rect.maxX, rect.maxY)
        let minW = min(minSize.width, area.width), minH = min(minSize.height, area.height)
        if edges.contains(.left) { minX = min(max(area.minX, minX + delta.dx), maxX - minW) }
        if edges.contains(.right) { maxX = max(min(area.maxX, maxX + delta.dx), minX + minW) }
        if edges.contains(.top) { minY = min(max(area.minY, minY + delta.dy), maxY - minH) }
        if edges.contains(.bottom) { maxY = max(min(area.maxY, maxY + delta.dy), minY + minH) }
        return CGRect(x: minX, y: minY, width: maxX - minX, height: maxY - minY)
    }

    static let edgeReach: CGFloat = 32

    /// The edge of the splits' `area` (flipped) a card dragged to `point` docks at: the nearest
    /// one within `edgeReach`, and none outside the area.
    static func edge(at point: CGPoint, in area: CGRect) -> SplitDirName? {
        guard area.contains(point) else { return nil }
        let distances: [(SplitDirName, CGFloat)] = [
            (.left, point.x - area.minX), (.right, area.maxX - point.x),
            (.up, point.y - area.minY), (.down, area.maxY - point.y),
        ]
        guard let nearest = distances.min(by: { $0.1 < $1.1 }), nearest.1 <= edgeReach else { return nil }
        return nearest.0
    }

    /// The half of `area` a card docked at `edge` takes.
    static func edgeRect(_ edge: SplitDirName, in area: CGRect) -> CGRect {
        switch edge {
        case .left: return CGRect(x: area.minX, y: area.minY, width: area.width / 2, height: area.height)
        case .right: return CGRect(x: area.midX, y: area.minY, width: area.width / 2, height: area.height)
        case .up: return CGRect(x: area.minX, y: area.minY, width: area.width, height: area.height / 2)
        case .down: return CGRect(x: area.minX, y: area.midY, width: area.width, height: area.height / 2)
        }
    }

    /// The side of `rect` a pane dropped at `point` docks on: the one whose triangle (between
    /// the rectangle's diagonals) holds the point.
    static func dockSide(_ point: CGPoint, in rect: CGRect) -> SplitDirName {
        let dx = (point.x - rect.midX) / max(1, rect.width)
        let dy = (point.y - rect.midY) / max(1, rect.height)
        if abs(dx) > abs(dy) { return dx < 0 ? .left : .right }
        return dy < 0 ? .up : .down
    }

    /// The part of `rect` a pane docked on `side` would take: half of it.
    static func dockRect(_ rect: CGRect, side: SplitDirName, gap: CGFloat) -> CGRect {
        let axis: SplitAxis = side == .left || side == .right ? .horizontal : .vertical
        let (a, b, _) = SplitNode.divide(rect, axis: axis, ratio: 0.5, gap: gap)
        return side == .left || side == .up ? a : b
    }

    /// The parts of the strip `rect` (a divider's hit area) outside `holes`, cut along its
    /// length: the divider's cursor stays off the cards above it.
    static func uncovered(_ rect: CGRect, axis: SplitAxis, by holes: [CGRect]) -> [CGRect] {
        var parts = [rect]
        for hole in holes {
            parts = parts.flatMap { part -> [CGRect] in
                guard part.intersects(hole) else { return [part] }
                switch axis {
                // A vertical divider (side by side panes) runs top to bottom.
                case .horizontal:
                    return [CGRect(x: part.minX, y: part.minY, width: part.width, height: hole.minY - part.minY),
                            CGRect(x: part.minX, y: hole.maxY, width: part.width, height: part.maxY - hole.maxY)]
                        .filter { $0.height > 0 }
                case .vertical:
                    return [CGRect(x: part.minX, y: part.minY, width: hole.minX - part.minX, height: part.height),
                            CGRect(x: hole.maxX, y: part.minY, width: part.maxX - hole.maxX, height: part.height)]
                        .filter { $0.width > 0 }
                }
            }
        }
        return parts
    }

    /// The cursor for resizing along `edges`.
    static func cursor(for edges: FloatingEdges) -> NSCursor {
        if #available(macOS 15, *) {
            let position: NSCursor.FrameResizePosition
            switch edges {
            case [.top, .left]: position = .topLeft
            case [.top, .right]: position = .topRight
            case [.bottom, .left]: position = .bottomLeft
            case [.bottom, .right]: position = .bottomRight
            case [.left]: position = .left
            case [.right]: position = .right
            case [.top]: position = .top
            default: position = .bottom
            }
            return .frameResize(position: position, directions: .all)
        }
        return edges.isDisjoint(with: [.left, .right]) ? .resizeUpDown : .resizeLeftRight
    }
}

/// Under a card: its fill and shadow. The terminal's background color, opaque, so the panes
/// below never show through (even in a translucent window), and the header sits on it.
final class FloatingCardBack: NSView {
    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        guard let layer else { return }
        layer.cornerRadius = TabContentView.cardRadius
        layer.cornerCurve = .continuous
        layer.shadowColor = NSColor.black.cgColor
        layer.shadowOpacity = 0.32
        layer.shadowRadius = 14
        layer.shadowOffset = CGSize(width: 0, height: -5)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    var fill: NSColor = .black {
        didSet { layer?.backgroundColor = fill.withAlphaComponent(1).cgColor }
    }
}

/// Over a card: its outline, in the accent color while it has the focus. Clicks pass through.
final class FloatingCardRim: NSView {
    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.cornerRadius = TabContentView.cardRadius
        layer?.cornerCurve = .continuous
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    func update(focused: Bool) {
        let chrome = SessionManager.shared.config.chrome
        let accent = focused && SessionManager.shared.config.activePaneBorder
        layer?.borderWidth = accent ? 1.5 : ChromeStroke.width
        layer?.borderColor = (accent ? chrome.accent : focused ? chrome.strokeStrong : chrome.stroke).cgColor
    }
}

/// Where a card dragged with ⌘ would dock: a tinted rectangle over that part of the pane.
final class DockIndicator: NSView {
    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.cornerRadius = TabContentView.cardRadius
        layer?.cornerCurve = .continuous
        layer?.borderWidth = 2
        isHidden = true
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    func show(_ rect: CGRect?) {
        guard let rect else {
            isHidden = true
            return
        }
        let accent = SessionManager.shared.config.chrome.accent
        layer?.backgroundColor = accent.withAlphaComponent(0.18).cgColor
        layer?.borderColor = accent.withAlphaComponent(0.7).cgColor
        frame = rect
        isHidden = false
    }
}

final class MinimizedCardsBar: NSView {
    static let spacing: CGFloat = 6

    var onRestore: ((PaneKey) -> Void)?
    /// A press on a chip (see `MinimizedCardChip.onPress`).
    var onPress: ((PaneKey, NSEvent) -> Bool)?
    var panes: [PaneKey] = [] {
        didSet { if panes != oldValue { rebuild() } }
    }
    private(set) var chips: [MinimizedCardChip] = []
    let overflow = NSButton(title: "", target: nil, action: nil)
    private(set) var overflowPanes: [PaneKey] = []

    override init(frame: NSRect) {
        super.init(frame: frame)
        isHidden = true
        overflow.bezelStyle = .inline
        overflow.font = .systemFont(ofSize: 11, weight: .medium)
        overflow.target = self
        overflow.action = #selector(showOverflow)
        overflow.isHidden = true
        addSubview(overflow)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    private func rebuild() {
        chips.forEach { $0.removeFromSuperview() }
        chips = panes.map { pane in
            let chip = MinimizedCardChip(pane: pane)
            chip.onClick = { [weak self] in self?.onRestore?(pane) }
            chip.onPress = { [weak self] event in self?.onPress?(pane, event) ?? false }
            addSubview(chip)
            return chip
        }
    }

    func refresh() {
        chips.forEach { $0.refresh() }
    }

    func fit(maxWidth: CGFloat) -> CGSize {
        let height = MinimizedCardChip.height
        let widths = chips.map { min($0.fittingSize.width, maxWidth) }
        func rowWidth(_ widths: [CGFloat]) -> CGFloat {
            widths.reduce(0, +) + Self.spacing * CGFloat(max(0, widths.count - 1))
        }
        var shown = chips.count
        var visibleWidths = widths
        while rowWidth(visibleWidths) > maxWidth && shown > 0 {
            shown -= 1
            overflow.title = "+\(chips.count - shown)"
            let more = min(overflow.fittingSize.width + 8, maxWidth)
            visibleWidths = Array(widths.prefix(shown)) + [more]
        }
        overflowPanes = chips[shown...].map(\.pane)
        overflow.isHidden = overflowPanes.isEmpty
        overflow.setAccessibilityLabel("\(overflowPanes.count) more minimized panes")
        let views: [NSView] = Array(chips.prefix(shown)) + (overflowPanes.isEmpty ? [] : [overflow])
        for chip in chips[shown...] { chip.isHidden = true }
        for chip in chips.prefix(shown) { chip.isHidden = false }
        var x: CGFloat = 0
        for (view, width) in zip(views, visibleWidths) {
            view.frame = NSRect(x: x, y: 0, width: width, height: height)
            x += width + Self.spacing
        }
        return CGSize(width: rowWidth(visibleWidths), height: views.isEmpty ? 0 : height)
    }

    func overflowMenu() -> NSMenu {
        let menu = NSMenu()
        for pane in overflowPanes {
            let item = NSMenuItem(title: PaneHeader.title(of: pane, info: SessionManager.shared.panes[pane]),
                                  action: #selector(restoreFromMenu(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = pane
            menu.addItem(item)
        }
        return menu
    }

    @objc private func showOverflow() {
        overflowMenu().popUp(positioning: nil, at: NSPoint(x: overflow.frame.minX, y: overflow.frame.maxY), in: self)
    }

    @objc func restoreFromMenu(_ sender: NSMenuItem) {
        if let pane = sender.representedObject as? PaneKey { onRestore?(pane) }
    }
}

final class MinimizedCardChip: NSView {
    static let height: CGFloat = 26
    static let maxTitleWidth: CGFloat = 180

    let pane: PaneKey
    var onClick: (() -> Void)?
    /// Tracks a press until the button is released; true when it moved the chip (or was
    /// cancelled), false for a click.
    var onPress: ((NSEvent) -> Bool)?
    private let icon = NSImageView()
    private let dot = NSView()
    private let title = NSTextField(labelWithString: "")
    private var iconKind: PaneIcon?
    private var status: AgentStatus?
    private var hovering = false {
        didSet { if hovering != oldValue { refresh() } }
    }

    init(pane: PaneKey) {
        self.pane = pane
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = Self.height / 2
        layer?.cornerCurve = .continuous
        layer?.borderWidth = ChromeStroke.width
        icon.imageScaling = .scaleProportionallyDown
        dot.wantsLayer = true
        dot.layer?.cornerRadius = 3
        title.font = .systemFont(ofSize: 11, weight: .medium)
        title.lineBreakMode = .byTruncatingTail
        title.maximumNumberOfLines = 1
        for v in [icon, dot, title] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        NSLayoutConstraint.activate([
            heightAnchor.constraint(equalToConstant: Self.height),
            icon.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 9),
            icon.centerYAnchor.constraint(equalTo: centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: 13),
            icon.heightAnchor.constraint(equalToConstant: 13),
            dot.widthAnchor.constraint(equalToConstant: 6),
            dot.heightAnchor.constraint(equalToConstant: 6),
            dot.trailingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 2),
            dot.bottomAnchor.constraint(equalTo: icon.bottomAnchor, constant: 1),
            title.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 6),
            title.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            title.centerYAnchor.constraint(equalTo: centerYAnchor),
            title.widthAnchor.constraint(lessThanOrEqualToConstant: Self.maxTitleWidth),
        ])
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeInActiveApp, .inVisibleRect],
                                       owner: self))
        refresh()
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    func refresh() {
        let chrome = SessionManager.shared.config.chrome
        let info = SessionManager.shared.panes[pane]
        let kind = PaneIcon(info)
        if kind != iconKind {
            iconKind = kind
            icon.image = kind.glyph(size: 13)
        }
        let text = PaneHeader.title(of: pane, info: info)
        if title.stringValue != text { title.stringValue = text }
        toolTip = "\(text)\nClick to restore, drag to move"
        title.textColor = hovering ? chrome.contentPrimary : chrome.contentSecondary
        icon.contentTintColor = hovering ? chrome.contentPrimary : chrome.contentSecondary
        layer?.backgroundColor = chrome.surfaceRaised.cgColor
        layer?.borderColor = (hovering ? chrome.strokeStrong : chrome.stroke).cgColor
        status = info?.agent?.status
        dot.isHidden = status == nil || status == .idle
        dot.layer?.backgroundColor = status?.color.cgColor
    }

    override func isAccessibilityElement() -> Bool { true }
    override func accessibilityRole() -> NSAccessibility.Role? { .button }
    override func accessibilityLabel() -> String? { "Restore \(title.stringValue)" }
    override func accessibilityValue() -> Any? { status?.rawValue }

    override func accessibilityPerformPress() -> Bool {
        onClick?()
        return true
    }

    override var acceptsFirstResponder: Bool { true }
    override var canBecomeKeyView: Bool { true }

    override func keyDown(with event: NSEvent) {
        if event.charactersIgnoringModifiers == " " || event.keyCode == 36 || event.keyCode == 76 {
            onClick?()
        } else {
            super.keyDown(with: event)
        }
    }

    override func drawFocusRingMask() {
        NSBezierPath(roundedRect: bounds, xRadius: Self.height / 2, yRadius: Self.height / 2).fill()
    }

    override var focusRingMaskBounds: NSRect { bounds }

    override func mouseEntered(with event: NSEvent) { hovering = true }
    override func mouseExited(with event: NSEvent) { hovering = false }

    /// A press that neither moved the chip nor was cancelled, released over the chip (`chip`,
    /// in the coordinates of `released`).
    static func isClick(moved: Bool, cancelled: Bool, released: CGPoint, chip: CGRect) -> Bool {
        !moved && !cancelled && chip.contains(released)
    }

    override func mouseDown(with event: NSEvent) {
        guard let onPress else { return }
        if !onPress(event) { onClick?() }
    }

    override func mouseUp(with event: NSEvent) {
        if onPress == nil, bounds.contains(convert(event.locationInWindow, from: nil)) { onClick?() }
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}
