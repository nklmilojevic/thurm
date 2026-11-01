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

    init(pane: PaneKey, frame: CGRect, zoomed: Bool = false, cells: CardCells? = nil) {
        self.pane = pane
        self.frame = frame
        self.zoomed = zoomed
        self.cells = cells
    }

    var minSize: CGSize { cells == nil ? FloatingGeometry.minSize : FloatingGeometry.exactMinSize }

    init(_ saved: FloatingPane) {
        let f = saved.frame.count == 4 ? saved.frame.map { CGFloat($0) } : {
            let d = FloatingGeometry.defaultFraction(index: 0)
            return [d.minX, d.minY, d.width, d.height]
        }()
        self.init(pane: saved.pane, frame: CGRect(x: f[0], y: f[1], width: f[2], height: f[3]), zoomed: saved.zoomed,
                  cells: saved.cells)
    }

    var saved: FloatingPane {
        FloatingPane(pane: pane, frame: [frame.minX, frame.minY, frame.width, frame.height].map(Double.init),
                     zoomed: zoomed, cells: cells)
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
