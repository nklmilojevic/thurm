import AppKit

/// Orientation of a split: `.horizontal` places children side by side (vertical divider),
/// `.vertical` stacks them (horizontal divider).
enum SplitAxis {
    case horizontal
    case vertical
}

enum FocusDirection {
    case left, right, up, down
}

/// A divider between the two children of a split.
struct SplitDivider {
    /// Visible 1pt line.
    let rect: CGRect
    /// Wider area that accepts drags.
    let hitRect: CGRect
    let axis: SplitAxis
    /// Path from the root to the split (false = first child, true = second child).
    let path: [Bool]
    /// Rectangle occupied by that split.
    let container: CGRect
}

/// Binary split tree of pane ids, mirroring `LayoutNode`.
indirect enum SplitNode {
    case leaf(PaneKey)
    case split(axis: SplitAxis, ratio: CGFloat, first: SplitNode, second: SplitNode)

    var panes: [PaneKey] {
        switch self {
        case .leaf(let id):
            return [id]
        case .split(_, _, let first, let second):
            return first.panes + second.panes
        }
    }

    func contains(_ id: PaneKey) -> Bool {
        switch self {
        case .leaf(let leaf):
            return leaf == id
        case .split(_, _, let first, let second):
            return first.contains(id) || second.contains(id)
        }
    }

    /// Splits `target`, putting `newPane` on the given side with `share` of the space.
    func inserting(_ newPane: PaneKey, at target: PaneKey, direction: SplitDirName,
                   share: CGFloat = 0.5) -> SplitNode {
        switch self {
        case .leaf(let id):
            guard id == target else { return self }
            switch direction {
            case .right: return .split(axis: .horizontal, ratio: 1 - share, first: self, second: .leaf(newPane))
            case .left: return .split(axis: .horizontal, ratio: share, first: .leaf(newPane), second: self)
            case .down: return .split(axis: .vertical, ratio: 1 - share, first: self, second: .leaf(newPane))
            case .up: return .split(axis: .vertical, ratio: share, first: .leaf(newPane), second: self)
            }
        case .split(let axis, let ratio, let first, let second):
            return .split(axis: axis, ratio: ratio,
                          first: first.inserting(newPane, at: target, direction: direction, share: share),
                          second: second.inserting(newPane, at: target, direction: direction, share: share))
        }
    }

    /// Exchanges the places of two panes (their splits stay where they are).
    func swapping(_ a: PaneKey, _ b: PaneKey) -> SplitNode {
        switch self {
        case .leaf(let id):
            if id == a { return .leaf(b) }
            if id == b { return .leaf(a) }
            return self
        case .split(let axis, let ratio, let first, let second):
            return .split(axis: axis, ratio: ratio, first: first.swapping(a, b), second: second.swapping(a, b))
        }
    }

    /// Removes a pane; nil when the tree becomes empty.
    func removing(_ id: PaneKey) -> SplitNode? {
        switch self {
        case .leaf(let leaf):
            return leaf == id ? nil : self
        case .split(let axis, let ratio, let first, let second):
            let a = first.removing(id)
            let b = second.removing(id)
            if let a = a, let b = b { return .split(axis: axis, ratio: ratio, first: a, second: b) }
            return a ?? b
        }
    }

    /// The split that holds `id` as a direct child: its axis, and whether `id` is the second
    /// (right or bottom) child. Nil for a lone pane or one not in the tree.
    func side(of id: PaneKey) -> (axis: SplitAxis, second: Bool)? {
        guard case .split(let axis, _, let first, let second) = self else { return nil }
        if case .leaf(let leaf) = first, leaf == id { return (axis, false) }
        if case .leaf(let leaf) = second, leaf == id { return (axis, true) }
        return first.side(of: id) ?? second.side(of: id)
    }

    /// `rect` flattened onto its outer edge along `axis`: the far edge for a second child, the
    /// near one for a first. A pane opens out of this edge and closes into it.
    static func collapsed(_ rect: CGRect, axis: SplitAxis, second: Bool) -> CGRect {
        switch axis {
        case .horizontal:
            return CGRect(x: second ? rect.maxX : rect.minX, y: rect.minY, width: 0, height: rect.height)
        case .vertical:
            return CGRect(x: rect.minX, y: second ? rect.maxY : rect.minY, width: rect.width, height: 0)
        }
    }

    /// Frames of every pane inside `rect`, leaving `gap` points for each divider.
    func frames(in rect: CGRect, gap: CGFloat) -> [(PaneKey, CGRect)] {
        switch self {
        case .leaf(let id):
            return [(id, rect)]
        case .split(let axis, let ratio, let first, let second):
            let (a, b, _) = SplitNode.divide(rect, axis: axis, ratio: ratio, gap: gap)
            return first.frames(in: a, gap: gap) + second.frames(in: b, gap: gap)
        }
    }

    func dividers(in rect: CGRect, gap: CGFloat, path: [Bool] = []) -> [SplitDivider] {
        guard case .split(let axis, let ratio, let first, let second) = self else { return [] }
        let (a, b, line) = SplitNode.divide(rect, axis: axis, ratio: ratio, gap: gap)
        let hit = axis == .horizontal ? line.insetBy(dx: -3, dy: 0) : line.insetBy(dx: 0, dy: -3)
        var out = [SplitDivider(rect: line, hitRect: hit, axis: axis, path: path, container: rect)]
        out += first.dividers(in: a, gap: gap, path: path + [false])
        out += second.dividers(in: b, gap: gap, path: path + [true])
        return out
    }

    /// Splits `rect` into (first, second, divider line).
    static func divide(_ rect: CGRect, axis: SplitAxis, ratio: CGFloat, gap: CGFloat) -> (CGRect, CGRect, CGRect) {
        let r = min(0.95, max(0.05, ratio))
        switch axis {
        case .horizontal:
            let avail = max(0, rect.width - gap)
            let w1 = (avail * r).rounded()
            let a = CGRect(x: rect.minX, y: rect.minY, width: w1, height: rect.height)
            let line = CGRect(x: rect.minX + w1, y: rect.minY, width: gap, height: rect.height)
            let b = CGRect(x: rect.minX + w1 + gap, y: rect.minY, width: avail - w1, height: rect.height)
            return (a, b, line)
        case .vertical:
            let avail = max(0, rect.height - gap)
            let h1 = (avail * r).rounded()
            let a = CGRect(x: rect.minX, y: rect.minY, width: rect.width, height: h1)
            let line = CGRect(x: rect.minX, y: rect.minY + h1, width: rect.width, height: gap)
            let b = CGRect(x: rect.minX, y: rect.minY + h1 + gap, width: rect.width, height: avail - h1)
            return (a, b, line)
        }
    }

    func settingRatio(at path: [Bool], to newRatio: CGFloat) -> SplitNode {
        guard case .split(let axis, let ratio, let first, let second) = self else { return self }
        if path.isEmpty {
            return .split(axis: axis, ratio: min(0.95, max(0.05, newRatio)), first: first, second: second)
        }
        let rest = Array(path.dropFirst())
        if path[0] {
            return .split(axis: axis, ratio: ratio, first: first, second: second.settingRatio(at: rest, to: newRatio))
        }
        return .split(axis: axis, ratio: ratio, first: first.settingRatio(at: rest, to: newRatio), second: second)
    }

    /// Number of panes along `axis` (for equalizing).
    func weight(along axis: SplitAxis) -> Int {
        switch self {
        case .leaf:
            return 1
        case .split(let a, _, let first, let second):
            if a == axis {
                return first.weight(along: axis) + second.weight(along: axis)
            }
            return max(first.weight(along: axis), second.weight(along: axis))
        }
    }

    func equalized() -> SplitNode {
        guard case .split(let axis, _, let first, let second) = self else { return self }
        let w1 = CGFloat(first.weight(along: axis))
        let w2 = CGFloat(second.weight(along: axis))
        return .split(axis: axis, ratio: w1 / max(1, w1 + w2), first: first.equalized(), second: second.equalized())
    }

    /// Moves the nearest divider of `axis` around `pane` by `delta` (fraction of the split).
    func resizing(_ pane: PaneKey, axis: SplitAxis, delta: CGFloat) -> (SplitNode, Bool) {
        guard case .split(let a, let ratio, let first, let second) = self else { return (self, false) }
        if first.contains(pane) {
            let (node, done) = first.resizing(pane, axis: axis, delta: delta)
            if done { return (.split(axis: a, ratio: ratio, first: node, second: second), true) }
        } else if second.contains(pane) {
            let (node, done) = second.resizing(pane, axis: axis, delta: delta)
            if done { return (.split(axis: a, ratio: ratio, first: first, second: node), true) }
        } else {
            return (self, false)
        }
        if a == axis {
            return (.split(axis: a, ratio: min(0.95, max(0.05, ratio + delta)), first: first, second: second), true)
        }
        return (self, false)
    }

    // MARK: Layout conversion

    init(layout: LayoutNode) {
        switch layout {
        case .pane(let id):
            self = .leaf(id)
        case .split(let dir, let ratio, let first, let second):
            let axis: SplitAxis = (dir == .right || dir == .left) ? .horizontal : .vertical
            // `left`/`up` put `second` before `first`; the tree stores panes in screen order
            // (left/top child first), with `ratio` the share of the left/top child.
            if dir == .left || dir == .up {
                self = .split(axis: axis, ratio: CGFloat(1 - ratio), first: SplitNode(layout: second),
                              second: SplitNode(layout: first))
            } else {
                self = .split(axis: axis, ratio: CGFloat(ratio), first: SplitNode(layout: first),
                              second: SplitNode(layout: second))
            }
        }
    }

    var layoutNode: LayoutNode {
        switch self {
        case .leaf(let id):
            return .pane(id)
        case .split(let axis, let ratio, let first, let second):
            return .split(dir: axis == .horizontal ? .right : .down, ratio: Double(ratio),
                          first: first.layoutNode, second: second.layoutNode)
        }
    }
}

// MARK: - Tab content

/// The content of one tab: terminal views laid out according to a split tree, with draggable
/// dividers. Also hosts the find bar overlay.
final class TabContentView: NSView {
    weak var controller: TerminalWindowController?

    private(set) var root: SplitNode
    private(set) var views: [PaneKey: TerminalView] = [:]
    private var headers: [PaneKey: PaneHeader] = [:]
    private let backdrop = SplitBackdrop()
    private let chrome = CardChrome()
    /// `window.active_pane_border` without pane headers: the ring around the focused pane (the
    /// cards draw their own).
    private let focusRing = FocusRing()
    private(set) var focusedPane: PaneKey
    private(set) var zoomedPane: PaneKey?
    /// Panes floating above the splits, back to front (see FloatingPanes.swift).
    private(set) var floating: [FloatingCard] = []
    /// The cards are hidden (their programs keep running) until shown again.
    private(set) var floatingHidden = false
    /// The cards on screen: none while hidden.
    private var shownCards: [FloatingCard] { floatingHidden ? [] : floating }
    /// Each card's fill and outline, by pane.
    private var cardParts: [PaneKey: (back: FloatingCardBack, rim: FloatingCardRim)] = [:]
    /// The pane of the splits focused last: where they split while a card has the focus.
    private var lastTiledFocus: PaneKey?
    /// Where docked panes floated, for when they float again.
    private var floatedFrames: [PaneKey: CGRect] = [:]
    /// Closing panes that were cards: they shrink in place instead of into a split edge.
    private var leavingCards: Set<PaneKey> = []
    private let dockIndicator = DockIndicator()
    /// A card whose edges a drag is moving: the edges, and its rectangle and the pointer at the
    /// start.
    private var resizing: (pane: PaneKey, edges: FloatingEdges, start: CGRect, from: NSPoint)?

    private(set) var findBar: FindBar?
    private var noticeBadges: [NoticeBadge] = []
    private var dragging: SplitDivider?
    /// Pane rectangles where a layout move (zoom, a pane opening or closing) started;
    /// `layoutProgress` eases them to the layout.
    private var motionFrom: [PaneKey: CGRect] = [:]
    private var layoutProgress = Tween(1)
    /// The time `layoutPanes` lays out for while a move is recorded (see `playMove`).
    private var layoutClock: CFTimeInterval?
    /// Layers playing the move's frames, and which move that is.
    private var movingLayers: [CALayer] = []
    private var moveGeneration = 0
    /// Panes opening in the current move, from the split edge they grow out of.
    private var entering: [PaneKey: CGRect] = [:]
    /// Panes closing in the current move: their views (detached, showing their last frame) and
    /// headers, and the split edge they shrink into. They go once the move settles.
    private var leaving: [PaneKey: (view: TerminalView, header: PaneHeader?, to: CGRect)] = [:]
    /// Path of the divider under the pointer or being dragged, and how far its highlight has
    /// eased in.
    private var highlightedPath: [Bool]?
    private var dividerEmphasis = Tween(0)
    private let dividerMotion = MotionDriver()

    static let gap: CGFloat = 1
    /// With pane headers, splits are rounded cards this far apart (and from the window edges):
    /// `window.pane_gap`.
    private var cardGap: CGFloat { SessionManager.shared.config.paneGap }
    static let cardRadius: CGFloat = ChromeRadius.pane

    /// With vertical tabs and pane headers the panes run the full height of the window, under
    /// the (empty) titlebar: each pane's header takes the place of the window title.
    var fullHeight: Bool {
        let cfg = SessionManager.shared.config
        return cfg.paneHeaders && controller?.tabSplit != nil
    }

    /// Pane headers show while more than one pane is on screen (`window.pane_headers`), and
    /// always in `fullHeight` mode or under the horizontal tab strip.
    var showsHeaders: Bool {
        SessionManager.shared.config.paneHeaders
            && ((zoomedPane == nil && root.panes.count > 1) || fullHeight
                || (window as? TerminalWindow)?.tabStrip != nil)
    }

    /// Where the split tree lays out, and the space between its panes.
    private var splitArea: NSRect {
        guard showsHeaders else { return paneArea }
        var area = paneArea.insetBy(dx: cardGap, dy: cardGap)
        // The top headers' text lines up with the traffic lights.
        if fullHeight, let close = window?.standardWindowButton(.closeButton), close.superview != nil {
            let lights = convert(close.bounds, from: close).midY
            let top = max(paneArea.minY + cardGap, lights - 1 - PaneHeader.height / 2)
            area.size.height += area.minY - top
            area.origin.y = top
        }
        return area
    }
    private var splitGap: CGFloat { showsHeaders ? cardGap : Self.gap }

    private var isMoving: Bool { !layoutProgress.isFinished(at: CACurrentMediaTime()) }

    /// The moment the layout is for: a frame of a move being recorded, else the end of the move
    /// in progress (Core Animation shows the frames on the way, and the layers hold where they
    /// end up), else now.
    private var layoutTime: CFTimeInterval {
        if let layoutClock { return layoutClock }
        let now = CACurrentMediaTime()
        return layoutProgress.isFinished(at: now) ? now : layoutProgress.end
    }

    private var visibleFrames: [(PaneKey, CGRect)] { frames(at: layoutTime) }

    /// Each visible pane's rectangle at `now`: its card with headers, else its whole view. During
    /// a layout move, the rectangles on the way from where the panes were.
    private func frames(at now: CFTimeInterval) -> [(PaneKey, CGRect)] {
        let targets = targetFrames
        guard !layoutProgress.isFinished(at: now) else { return targets }
        let t = CGFloat(layoutProgress.value(at: now))
        let moving = targets.map { id, rect in
            (id, Self.blend(motionFrom[id] ?? entering[id] ?? motionFrom.values.first ?? rect, rect, t))
        }
        // Panes hidden by a zoom hold their old rectangles until it settles; closing ones
        // shrink into their split edge.
        let held = motionFrom.filter { id, _ in leaving[id] == nil && !targets.contains { $0.0 == id } }
        let closing = leaving.compactMap { id, pane in
            motionFrom[id].map { (id, Self.blend($0, pane.to, t)) }
        }
        return moving + held.map { ($0.key, $0.value) } + closing
    }

    /// How much of a pane's content shows during a layout move: opening panes fade in, closing
    /// ones fade out (see `Motion.crossFade`).
    private func contentOpacity(_ id: PaneKey, at now: CFTimeInterval) -> CGFloat {
        guard !layoutProgress.isFinished(at: now) else { return 1 }
        let t = layoutProgress.value(at: now)
        if entering[id] != nil { return CGFloat(Motion.crossFade(t, appearing: true)) }
        if leaving[id] != nil { return CGFloat(Motion.crossFade(t, appearing: false)) }
        return 1
    }

    private var targetFrames: [(PaneKey, CGRect)] {
        let tiled = zoomedPane.map { [($0, showsHeaders ? splitArea : paneArea)] }
            ?? root.frames(in: splitArea, gap: splitGap)
        return tiled + shownCards.map { ($0.pane, cardRect($0)) }
    }

    /// Where cards go: the tab below the titlebar, clear of its edges.
    private var floatArea: NSRect {
        let inset = safeAreaInsets
        return NSRect(x: bounds.minX + inset.left, y: bounds.minY + inset.top,
                      width: max(0, bounds.width - inset.left - inset.right),
                      height: max(0, bounds.height - inset.top - inset.bottom)).insetBy(dx: 8, dy: 8)
    }

    /// A card's rectangle; zoomed, the space a zoomed pane of the splits takes.
    private func cardRect(_ card: FloatingCard) -> CGRect {
        card.zoomed ? (showsHeaders ? splitArea : paneArea) : FloatingGeometry.rect(card.frame, in: floatArea)
    }

    /// The cards' rectangles as laid out now, back to front.
    private var cardFrames: [(PaneKey, CGRect)] {
        let frames = Dictionary(visibleFrames, uniquingKeysWith: { first, _ in first })
        return shownCards.compactMap { card in frames[card.pane].map { (card.pane, $0) } }
    }

    private static func blend(_ a: CGRect, _ b: CGRect, _ t: CGFloat) -> CGRect {
        CGRect(x: a.minX + (b.minX - a.minX) * t, y: a.minY + (b.minY - a.minY) * t,
               width: max(0, a.width + (b.width - a.width) * t),
               height: max(0, a.height + (b.height - a.height) * t))
    }

    init(root: SplitNode, focused: PaneKey, zoomed: PaneKey?, floating: [FloatingPane] = [],
         floatingHidden: Bool = false) {
        self.root = root
        let panes = root.panes
        var seen = Set(panes)
        self.floating = floating.filter { seen.insert($0.pane).inserted }.map(FloatingCard.init)
        self.floatingHidden = floatingHidden && !self.floating.isEmpty
        // Hidden cards don't take the focus.
        if self.floatingHidden { seen = Set(panes) }
        self.focusedPane = seen.contains(focused) ? focused : (panes.first ?? focused)
        self.lastTiledFocus = panes.contains(focused) ? focused : panes.first
        if let z = zoomed, panes.contains(z) { self.zoomedPane = z } else { self.zoomedPane = nil }
        super.init(frame: NSRect(x: 0, y: 0, width: 800, height: 600))
        autoresizingMask = [.width, .height]
        addSubview(backdrop)
        addSubview(chrome)
        addSubview(focusRing)
        addSubview(dockIndicator)
        syncViews()
        setNotices(Notices.shared.current)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    /// The tab's panes: those of the splits, then the floating ones.
    var paneIds: [PaneKey] { root.panes + floating.map(\.pane) }
    var isEmpty: Bool { views.isEmpty }
    var focusedView: TerminalView? { views[focusedPane] }

    func isFloating(_ pane: PaneKey) -> Bool { floating.contains { $0.pane == pane } }
    func contains(_ pane: PaneKey) -> Bool { root.contains(pane) || isFloating(pane) }
    /// Whether `pane` can float: a pane of the splits, which keep another.
    func canFloat(_ pane: PaneKey) -> Bool { root.contains(pane) && root.panes.count > 1 }

    /// Panes currently on screen (all panes, or only the zoomed one and the floating ones).
    var visiblePaneIds: [PaneKey] {
        (zoomedPane.map { [$0] } ?? root.panes) + shownCards.map(\.pane)
    }

    var hasMultipleVisiblePanes: Bool { visiblePaneIds.count > 1 }

    // MARK: View management

    /// Creates views for new panes and removes views of panes no longer in the tree. With
    /// `keepingClosed`, the views of removed panes stay on screen in `leaving` for a layout move
    /// to close (the move fills in where they shrink to).
    private func syncViews(keepingClosed: Bool = false) {
        let ids = Set(paneIds)
        for (id, view) in views where !ids.contains(id) {
            view.detach()
            views.removeValue(forKey: id)
            let header = headers.removeValue(forKey: id)
            if keepingClosed {
                leaving[id] = (view, header, .zero)
            } else {
                view.removeFromSuperview()
                header?.removeFromSuperview()
            }
        }
        for id in paneIds where views[id] == nil {
            let view = TerminalView(pane: id)
            view.onDimDrawn = { [weak self] in
                guard let self else { return }
                if self.showsHeaders || self.isMoving || !self.floating.isEmpty { self.updateBackdrop() }
            }
            views[id] = view
            addSubview(view, positioned: .below, relativeTo: chrome)
        }
        for (id, parts) in cardParts where !isFloating(id) && !(keepingClosed && leaving[id] != nil) {
            parts.back.removeFromSuperview()
            parts.rim.removeFromSuperview()
            cardParts.removeValue(forKey: id)
        }
        for card in floating where cardParts[card.pane] == nil {
            let parts = (back: FloatingCardBack(), rim: FloatingCardRim())
            addSubview(parts.back)
            addSubview(parts.rim)
            cardParts[card.pane] = parts
        }
        restack()
        needsLayout = true
        updateBackdrop()
    }

    /// Orders the subviews: the splits under their chrome, then the cards back to front (each
    /// its fill, terminal, header and outline), the dock indicator, and overlays (find bar,
    /// notices) on top. Sorted in place: a view being clicked is never taken out of the window.
    private func restack() {
        var rank: [ObjectIdentifier: Int] = [:]
        func set(_ view: NSView?, _ r: Int) { if let view { rank[ObjectIdentifier(view)] = r } }
        for (i, view) in subviews.enumerated() { rank[ObjectIdentifier(view)] = 100_000 + i }
        set(backdrop, 0)
        for (i, view) in subviews.enumerated() where view is TerminalView || view is PaneHeader {
            set(view, 100 + i)
        }
        set(chrome, 10_000)
        set(focusRing, 10_001)
        let cards = floating.map(\.pane) + leavingCards.sorted { $0.id < $1.id }
        for (i, id) in cards.enumerated() {
            let base = 20_000 + i * 4
            set(cardParts[id]?.back, base)
            set(views[id] ?? leaving[id]?.view, base + 1)
            set(headers[id] ?? leaving[id]?.header, base + 2)
            set(cardParts[id]?.rim, base + 3)
        }
        set(dockIndicator, 50_000)
        withUnsafeMutablePointer(to: &rank) { ranks in
            sortSubviews({ a, b, context in
                let ranks = context!.assumingMemoryBound(to: [ObjectIdentifier: Int].self).pointee
                let (x, y) = (ranks[ObjectIdentifier(a)] ?? 0, ranks[ObjectIdentifier(b)] ?? 0)
                return x < y ? .orderedAscending : x > y ? .orderedDescending : .orderedSame
            }, context: ranks)
        }
    }

    override func layout() {
        super.layout()
        layoutPanes()
    }

    override func setFrameSize(_ newSize: NSSize) {
        let resized = newSize != frame.size
        super.setFrameSize(newSize)
        // A move's frames were laid out for the old size: settle it at the new one.
        if resized && isMoving { finishLayoutMove() }
        layoutPanes()
    }

    /// `bounds` minus the safe-area insets (the titlebar and tab bar sit on top of the rest).
    var paneArea: NSRect {
        var inset = safeAreaInsets
        if fullHeight { inset.top = 0 }
        return NSRect(x: bounds.minX + inset.left, y: bounds.minY + inset.top,
                      width: max(0, bounds.width - inset.left - inset.right),
                      height: max(0, bounds.height - inset.top - inset.bottom))
    }

    func layoutPanes() {
        let full = paneArea
        let cards = showsHeaders
        let frames = visibleFrames
        let controls = titlebarControlsMaxX
        let visible = Set(frames.map(\.0))
        for (id, view) in views {
            view.isHidden = !visible.contains(id)
            headers[id]?.isHidden = !visible.contains(id)
        }
        for (id, parts) in cardParts {
            parts.back.isHidden = !visible.contains(id)
            parts.rim.isHidden = !visible.contains(id)
        }
        updateDimming()
        let now = layoutTime
        for (id, rect) in frames {
            guard let view = views[id] ?? leaving[id]?.view else { continue }
            let opacity = contentOpacity(id, at: now)
            view.alphaValue = opacity
            view.layer?.cornerRadius = cardParts[id] == nil ? 0 : Self.cardRadius - 1
            if let parts = cardParts[id] {
                layoutCard(id, rect: rect, view: view, parts: parts, opacity: opacity)
                continue
            }
            guard cards else {
                leaving[id]?.header?.isHidden = true
                view.frame = rect
                continue
            }
            // Inside the card's 1pt border: the header, then the terminal. Not `insetBy`: a
            // card opening or closing gets thinner than its border, and that inset is null.
            let inner = CGRect(x: rect.minX + 1, y: rect.minY + 1, width: max(0, rect.width - 2),
                               height: max(0, rect.height - 2))
            let header = header(for: id)
            header.isHidden = false
            header.alphaValue = opacity
            header.frame = NSRect(x: inner.minX, y: inner.minY, width: inner.width, height: PaneHeader.height)
            header.leadingInset = headerInset(inner: inner, rect: rect, controls: controls)
            view.frame = terminalFrame(in: rect, cards: true)
        }
        chrome.isHidden = !cards
        if controls != settledControls {
            settledControls = controls
            recheckTitlebarControls()
        }
        let tiled = frames.filter { cardParts[$0.0] == nil }
        if cards {
            chrome.frame = bounds
            chrome.scale = window?.backingScaleFactor ?? 2
            chrome.area = full
            chrome.cards = tiled.map {
                (rect: $0.1, focused: $0.0 == focusedPane && tiled.count > 1)
            }
            chrome.accentsFocused = SessionManager.shared.config.activePaneBorder
            chrome.opacity = controller?.opacity ?? SessionManager.shared.config.opacity
            chrome.windowRadius = (window as? TerminalWindow)?.cornerRadius ?? 0
            // The window's own corners (above the titlebar with native tabs, where this view
            // starts below it), from its frame in window coordinates.
            chrome.windowCorners = window.map { w in
                let s = w.frame.size
                return [NSPoint(x: 0, y: 0), NSPoint(x: s.width, y: 0), NSPoint(x: 0, y: s.height),
                        NSPoint(x: s.width, y: s.height)].map { convert($0, from: nil) }
            } ?? []
            syncDividerHighlight()
            for (id, header) in headers {
                header.update(focused: id == focusedPane)
            }
        } else {
            for (id, header) in headers where cardParts[id] == nil {
                header.removeFromSuperview()
                headers.removeValue(forKey: id)
            }
        }
        updateFocusRing(frames: tiled, cards: cards)
        for view in views.values {
            view.visibilityChanged()
        }
        if let bar = findBar, !bar.isHidden {
            let size = bar.fittingSize
            let width = min(max(size.width, 320), max(200, full.width - 24))
            bar.frame = NSRect(x: full.maxX - width - 12, y: full.minY + 8, width: width,
                               height: max(size.height, 34))
        }
        var bottom = full.maxY - 10
        for badge in noticeBadges {
            let size = badge.fittingSize
            let width = min(size.width, max(0, full.width - 24))
            badge.frame = NSRect(x: full.maxX - width - 12, y: bottom - size.height,
                                 width: width, height: size.height)
            bottom -= size.height + 6
        }
        updateBackdrop()
    }

    /// A top pane at the window's leading edge (sidebar collapsed) keeps its header's icon and
    /// title clear of the traffic lights and toolbar buttons.
    private func headerInset(inner: CGRect, rect: CGRect, controls: CGFloat?) -> CGFloat {
        if let controls, inner.minX < controls, rect.minY <= splitArea.minY + 1 {
            return controls - inner.minX + 10
        }
        return PaneHeader.leading
    }

    /// The header of `id`, made on first use.
    private func header(for id: PaneKey) -> PaneHeader {
        if let header = headers[id] ?? leaving[id]?.header { return header }
        let h = PaneHeader(pane: id)
        h.content = self
        headers[id] = h
        addSubview(h, positioned: .below, relativeTo: chrome)
        if cardParts[id] != nil { restack() }
        return h
    }

    /// A card in `rect`: its fill and shadow, the header across the top, the terminal below,
    /// and the outline over both.
    private func layoutCard(_ id: PaneKey, rect: CGRect, view: TerminalView,
                            parts: (back: FloatingCardBack, rim: FloatingCardRim), opacity: CGFloat) {
        parts.back.frame = rect
        parts.back.alphaValue = opacity
        parts.rim.frame = rect
        parts.rim.alphaValue = opacity
        parts.rim.update(focused: id == focusedPane)
        let header = header(for: id)
        header.isHidden = false
        header.alphaValue = opacity
        let inner = CGRect(x: rect.minX + 1, y: rect.minY + 1, width: max(0, rect.width - 2),
                           height: max(0, rect.height - 2))
        header.leadingInset = headerInset(inner: inner, rect: rect, controls: titlebarControlsMaxX)
        header.frame = NSRect(x: inner.minX, y: inner.minY, width: inner.width, height: PaneHeader.height)
        header.update(focused: id == focusedPane)
        view.frame = terminalFrame(in: rect, cards: true)
    }

    /// A pane's terminal in its rectangle: all of it, or with cards inside the card's 1pt
    /// border and below the header.
    private func terminalFrame(in rect: CGRect, cards: Bool) -> CGRect {
        guard cards else { return rect }
        let inner = CGRect(x: rect.minX + 1, y: rect.minY + 1, width: max(0, rect.width - 2),
                           height: max(0, rect.height - 2))
        return NSRect(x: inner.minX, y: inner.minY + PaneHeader.height, width: inner.width,
                      height: max(0, inner.height - PaneHeader.height))
    }

    /// `titlebarControlsMaxX` while a move is recorded: the titlebar holds still, and finding its
    /// buttons walks the window's views, too slow for every frame.
    private var recordingControls: CGFloat?

    /// `TerminalWindow.leadingControlsMaxX` in this view, when the panes run under the titlebar.
    private var titlebarControlsMaxX: CGFloat? {
        if layoutClock != nil { return recordingControls }
        guard fullHeight, let window = window as? TerminalWindow else { return nil }
        let maxX = window.leadingControlsMaxX
        return maxX > 0 ? convert(NSPoint(x: maxX, y: 0), from: nil).x : nil
    }

    /// AppKit lays the toolbar out after the content (sidebar collapsed or shown): check again
    /// once it has, and move the headers if its buttons moved.
    private var settledControls: CGFloat?
    private func recheckTitlebarControls() {
        DispatchQueue.main.async { [weak self] in
            guard let self, self.titlebarControlsMaxX != self.settledControls else { return }
            self.settledControls = self.titlebarControlsMaxX
            self.layoutPanes()
        }
    }

    private var currentDividers: [SplitDivider] {
        if zoomedPane != nil { return [] }
        return root.dividers(in: splitArea, gap: splitGap)
    }

    /// Places the ring around the focused pane, shown with `window.active_pane_border` while the
    /// tab shows several panes and no cards (which outline the focused one themselves).
    private func updateFocusRing(frames: [(PaneKey, CGRect)], cards: Bool) {
        let rect = frames.first { $0.0 == focusedPane }?.1
        let on = SessionManager.shared.config.activePaneBorder && !cards && frames.count > 1
        focusRing.frame = bounds
        focusRing.ring = on ? rect : nil
        focusRing.refresh()
    }

    /// Dims every pane but the focused one while the tab shows several.
    private func updateDimming() {
        let multiple = hasMultipleVisiblePanes
        for (id, view) in views {
            view.setDimmed(multiple && id != focusedPane)
        }
    }

    /// Hands the highlighted divider and its emphasis to the card chrome, which draws it.
    private func syncDividerHighlight(at now: CFTimeInterval = CACurrentMediaTime()) {
        let divider = highlightedPath.flatMap { path in currentDividers.first { $0.path == path } }
        chrome.highlight = divider.map { ($0.rect, $0.axis, CGFloat(dividerEmphasis.value(at: now))) }
        chrome.refresh()
    }

    /// Highlights the divider at `path` (nil: none) and eases the highlight in or out.
    private func setDividerHighlight(_ path: [Bool]?) {
        let pathChanged = path != nil && path != highlightedPath
        if let path { highlightedPath = path }
        let target = path == nil ? 0.0 : 1.0
        guard dividerEmphasis.to != target else {
            if pathChanged { syncDividerHighlight() }
            return
        }
        let now = CACurrentMediaTime()
        dividerEmphasis.move(to: target, duration: Motion.fast, at: now)
        syncDividerHighlight(at: now)
        if !dividerEmphasis.isFinished(at: now) {
            dividerMotion.run(on: self, for: Motion.fast) { [weak self] time in
                self?.syncDividerHighlight(at: time)
            }
        }
    }

    /// Runs a zoom change as a move: the panes glide from where they were to the new layout.
    private func transitionZoom(_ change: () -> Void) {
        transitionLayout {
            change()
            if let z = zoomedPane, let view = views[z] {
                addSubview(view, positioned: .below, relativeTo: chrome)
            }
        }
    }

    /// Runs a layout change as a move: the panes glide from where they were to the new layout,
    /// new panes grow out of their split edge and fade in, and closed ones shrink into theirs
    /// and fade out. Terminals hold their grid until it settles, so the text does not reflow
    /// per frame; a new pane sizes its grid for its place first, so it opens at its final size.
    private func transitionLayout(_ change: () -> Void) {
        let duration = Motion.duration(Motion.paneSettle)
        let from = Dictionary(frames(at: CACurrentMediaTime()), uniquingKeysWith: { first, _ in first })
        let oldRoot = root
        let oldCards = Set(floating.map(\.pane))
        let before = Set(views.keys)
        finishLayoutMove()
        guard duration > 0 else {
            change()
            syncViews()
            layoutPanes()
            return
        }
        views.values.forEach { $0.holdGridSize = true }
        change()
        syncViews(keepingClosed: true)
        let targets = Dictionary(targetFrames, uniquingKeysWith: { first, _ in first })
        let cards = showsHeaders
        for id in Set(views.keys).subtracting(before) {
            guard let rect = targets[id] else { continue }
            if isFloating(id) {
                // A new card grows a little where it lands as it fades in.
                entering[id] = rect.insetBy(dx: rect.width * 0.06, dy: rect.height * 0.06)
            } else {
                guard let side = root.side(of: id) else { continue }
                entering[id] = SplitNode.collapsed(rect, axis: side.axis, second: side.second)
            }
            // The new terminal takes its final grid size, unseen, before the move starts.
            if let view = views[id] {
                view.alphaValue = 0
                view.frame = terminalFrame(in: rect, cards: cards || isFloating(id))
            }
        }
        leavingCards = Set(leaving.keys).intersection(oldCards)
        for id in leaving.keys {
            // A card shrinks a little where it is as it fades.
            if leavingCards.contains(id), let rect = from[id] {
                leaving[id]?.to = rect.insetBy(dx: rect.width * 0.06, dy: rect.height * 0.06)
                continue
            }
            guard let side = oldRoot.side(of: id), let rect = from[id] else { continue }
            leaving[id]?.to = SplitNode.collapsed(rect, axis: side.axis, second: side.second)
        }
        if !leavingCards.isEmpty { restack() }
        motionFrom = from
        views.values.forEach { $0.holdGridSize = true }
        layoutProgress = Tween(0, curve: Motion.paneSpring)
        playMove(duration: duration)
    }

    /// Lays the move out once per display frame and hands the frames to Core Animation (see
    /// `BakedMotion`), which plays them with no work from the app; the layers themselves take
    /// where the move ends. It settles when the frames run out.
    private func playMove(duration: TimeInterval) {
        let fps = Double(window?.screen?.maximumFramesPerSecond ?? 60)
        let count = max(1, Int((duration * fps).rounded(.up)))
        let base = CACurrentMediaTime()
        layoutProgress.move(to: 1, duration: duration, at: base)
        var motion = BakedMotion()
        recordingControls = titlebarControlsMaxX
        // Views made for this move (a new pane's header) get their layers from AppKit at the
        // next display, and a layer that does not exist yet records nothing: lay out the first
        // frame and display it.
        layoutClock = base
        needsLayout = true
        layoutSubtreeIfNeeded()
        window?.displayIfNeeded()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        if let layer {
            for frame in 0..<count {
                layoutClock = base + duration * Double(frame) / Double(count)
                // `layout()` lays the panes out, then the headers' contents.
                needsLayout = true
                layoutSubtreeIfNeeded()
                motion.record(layer)
            }
        }
        layoutClock = nil
        // Laying the frames out took a moment: the move starts now, from its first frame.
        let start = CACurrentMediaTime()
        layoutProgress.restart(at: start)
        needsLayout = true
        layoutSubtreeIfNeeded()
        movingLayers = motion.play(begin: start, duration: duration, fps: Float(fps))
        CATransaction.commit()
        moveGeneration += 1
        let generation = moveGeneration
        DispatchQueue.main.asyncAfter(deadline: .now() + duration) { [weak self] in
            guard let self, self.moveGeneration == generation else { return }
            self.finishLayoutMove()
        }
    }

    /// Settles the layout move in progress, if any: closed panes go, terminals take their size.
    private func finishLayoutMove() {
        moveGeneration += 1
        BakedMotion.stop(movingLayers)
        movingLayers = []
        layoutProgress = Tween(1)
        let moved = !motionFrom.isEmpty || !leaving.isEmpty
        motionFrom = [:]
        entering = [:]
        for pane in leaving.values {
            pane.view.removeFromSuperview()
            pane.header?.removeFromSuperview()
        }
        leaving = [:]
        leavingCards = []
        for (id, parts) in cardParts where !isFloating(id) {
            parts.back.removeFromSuperview()
            parts.rim.removeFromSuperview()
            cardParts.removeValue(forKey: id)
        }
        guard moved else { return }
        layoutPanes()
        views.values.forEach { $0.holdGridSize = false }
    }

    /// The fills under the terminals, as `backdrop` layers: a layout move then changes layer
    /// frames and colors each frame instead of repainting a window-sized bitmap.
    private func updateBackdrop() {
        backdrop.frame = bounds
        var fills: [(NSRect, NSColor)] = []
        defer { backdrop.show(fills) }
        // Theme background under the titlebar and tab bar.
        let cfg = SessionManager.shared.config
        let area = paneArea
        let bg = colorFromRGB(cfg.theme.background, alpha: controller?.opacity ?? cfg.opacity)
        fills.append((NSRect(x: bounds.minX, y: bounds.minY, width: bounds.width, height: area.minY - bounds.minY), bg))
        func surface(_ id: PaneKey) -> NSColor {
            let scale = ChromePalette.dimmedBackgroundScale(drawn: (views[id] ?? leaving[id]?.view)?.dimDrawn ?? 0,
                                                            dim: cfg.unfocusedSplitDim,
                                                            isDark: cfg.theme.isDark)
            let c = [bg.redComponent * scale, bg.greenComponent * scale, bg.blueComponent * scale, bg.alphaComponent]
            return NSColor(colorSpace: bg.colorSpace, components: c, count: 4)
        }
        // Cards are filled by their own back, which hides the splits under them.
        for (id, parts) in cardParts { parts.back.fill = surface(id) }
        // During a layout move, each pane's surface under its terminal: what shows where a held
        // grid does not reach and where content fades in or out.
        let frames = visibleFrames.filter { cardParts[$0.0] == nil }
        if isMoving {
            for (id, rect) in frames {
                fills.append((rect, surface(id)))
            }
        }
        guard showsHeaders else {
            guard zoomedPane == nil, !isMoving else { return }
            for d in currentDividers {
                fills.append((d.rect, cfg.dividerColor))
            }
            return
        }
        // Cards: the header strips in the terminal's color, under the headers, shaded with the
        // pane's dim. The gutters and outlines are `CardChrome`'s, over the terminals' square
        // corners.
        for (id, rect) in frames {
            fills.append((NSRect(x: rect.minX, y: rect.minY, width: rect.width, height: 1 + PaneHeader.height), surface(id)))
        }
    }

    func dividerAt(_ point: NSPoint) -> SplitDivider? {
        let band = FloatingGeometry.band
        guard !cardFrames.contains(where: { $0.1.insetBy(dx: -band, dy: -band).contains(point) }) else { return nil }
        return currentDividers.first { $0.hitRect.contains(point) }
    }

    /// The card whose edge is at `point`, and which edges: the frontmost card there, as a
    /// card covers the edges of those behind it.
    private func cardEdges(at point: NSPoint) -> (pane: PaneKey, edges: FloatingEdges, rect: CGRect)? {
        for (id, rect) in cardFrames.reversed() {
            let edges = FloatingGeometry.edges(at: point, of: rect)
            if !edges.isEmpty {
                return floating.first { $0.pane == id }?.zoomed == true ? nil : (id, edges, rect)
            }
            if rect.contains(point) { return nil }
        }
        return nil
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        let local = convert(point, from: superview)
        if let bar = findBar, !bar.isHidden, bar.frame.contains(local) {
            return super.hitTest(point)
        }
        if cardEdges(at: local) != nil || dividerAt(local) != nil {
            return self
        }
        return super.hitTest(point)
    }

    // MARK: Divider dragging

    override func mouseDown(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        if let hit = cardEdges(at: p) {
            focus(hit.pane)
            resizing = (hit.pane, hit.edges, hit.rect, p)
            return
        }
        dragging = dividerAt(p)
        if let path = dragging?.path {
            setDividerHighlight(path)
        } else {
            super.mouseDown(with: event)
        }
    }

    override func mouseDragged(with event: NSEvent) {
        if let r = resizing, let i = floating.firstIndex(where: { $0.pane == r.pane }) {
            let p = convert(event.locationInWindow, from: nil)
            let area = floatArea
            let rect = FloatingGeometry.resized(r.start, edges: r.edges,
                                                by: CGVector(dx: p.x - r.from.x, dy: p.y - r.from.y), in: area)
            floating[i].frame = FloatingGeometry.fraction(of: rect, in: area)
            needsLayout = true
            return
        }
        guard let d = dragging else {
            super.mouseDragged(with: event)
            return
        }
        let p = convert(event.locationInWindow, from: nil)
        let ratio: CGFloat
        switch d.axis {
        case .horizontal:
            ratio = (p.x - d.container.minX) / max(1, d.container.width)
        case .vertical:
            ratio = (p.y - d.container.minY) / max(1, d.container.height)
        }
        root = root.settingRatio(at: d.path, to: ratio)
        // Refresh the stored divider geometry so later drags use the new container rects.
        dragging = root.dividers(in: splitArea, gap: splitGap).first { $0.path == d.path }
        needsLayout = true
    }

    override func mouseUp(with event: NSEvent) {
        if resizing != nil {
            resizing = nil
            window?.invalidateCursorRects(for: self)
            SessionManager.shared.scheduleLayoutSave()
            return
        }
        if dragging != nil {
            dragging = nil
            setDividerHighlight(dividerAt(convert(event.locationInWindow, from: nil))?.path)
            SessionManager.shared.scheduleLayoutSave()
        } else {
            super.mouseUp(with: event)
        }
    }

    override func mouseMoved(with event: NSEvent) {
        setDividerHighlight(dividerAt(convert(event.locationInWindow, from: nil))?.path)
    }

    override func mouseExited(with event: NSEvent) {
        if dragging == nil { setDividerHighlight(nil) }
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(NSTrackingArea(rect: bounds,
                                       options: [.mouseMoved, .mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                       owner: self))
    }

    override func resetCursorRects() {
        let band = FloatingGeometry.band
        let cards = cardFrames
        let holes = cards.map { $0.1.insetBy(dx: -band, dy: -band) }
        for d in currentDividers {
            for rect in FloatingGeometry.uncovered(d.hitRect, axis: d.axis, by: holes) {
                addCursorRect(rect, cursor: d.axis == .horizontal ? .resizeLeftRight : .resizeUpDown)
            }
        }
        for (i, (id, r)) in cards.enumerated() where floating.first(where: { $0.pane == id })?.zoomed == false {
            // A card's edges under the cards in front of it don't resize it (see `cardEdges`).
            let above = holes[(i + 1)...]
            let b2 = band * 2
            let parts: [(CGRect, FloatingEdges)] = [
                (CGRect(x: r.minX - band, y: r.minY + band, width: b2, height: r.height - b2), .left),
                (CGRect(x: r.maxX - band, y: r.minY + band, width: b2, height: r.height - b2), .right),
                (CGRect(x: r.minX + band, y: r.minY - band, width: r.width - b2, height: b2), .top),
                (CGRect(x: r.minX + band, y: r.maxY - band, width: r.width - b2, height: b2), .bottom),
                (CGRect(x: r.minX - band, y: r.minY - band, width: b2, height: b2), [.top, .left]),
                (CGRect(x: r.maxX - band, y: r.minY - band, width: b2, height: b2), [.top, .right]),
                (CGRect(x: r.minX - band, y: r.maxY - band, width: b2, height: b2), [.bottom, .left]),
                (CGRect(x: r.maxX - band, y: r.maxY - band, width: b2, height: b2), [.bottom, .right]),
            ]
            for (rect, edges) in parts {
                let pieces: [CGRect]
                switch edges {
                case .left, .right: pieces = FloatingGeometry.uncovered(rect, axis: .horizontal, by: Array(above))
                case .top, .bottom: pieces = FloatingGeometry.uncovered(rect, axis: .vertical, by: Array(above))
                default: pieces = above.contains { $0.intersects(rect) } ? [] : [rect]
                }
                for piece in pieces { addCursorRect(piece, cursor: FloatingGeometry.cursor(for: edges)) }
            }
        }
    }

    // MARK: Tree operations

    /// Inserts `newPane` next to `target` (default: focused pane) and focuses it. `share` is its
    /// part of the split (default: half, or even with auto-balance).
    func split(target: PaneKey? = nil, newPane: PaneKey, direction: SplitDirName, share: CGFloat? = nil,
               takeFocus: Bool = true) {
        let t = target.flatMap { root.contains($0) ? $0 : nil } ?? tiledFocus
        transitionLayout {
            zoomedPane = nil
            root = root.inserting(newPane, at: t, direction: direction, share: share ?? 0.5)
            if share == nil && SessionManager.shared.config.autoBalance { root = root.equalized() }
        }
        if takeFocus { focus(newPane) }
        window?.invalidateCursorRects(for: self)
    }

    /// The pane of the splits the splits act on: the focused one, or while a card has the focus,
    /// the one focused before it.
    private var tiledFocus: PaneKey {
        if root.contains(focusedPane) { return focusedPane }
        return lastTiledFocus.flatMap { root.contains($0) ? $0 : nil } ?? root.panes[0]
    }

    /// Moves `pane` of this tab next to `target`, keeping its view.
    func move(_ pane: PaneKey, nextTo target: PaneKey, direction: SplitDirName, share: CGFloat?) {
        if isFloating(pane) {
            dock(pane, nextTo: target, direction: direction, share: share)
            return
        }
        guard pane != target, root.contains(target), let rest = root.removing(pane), rest.contains(target) else {
            return
        }
        transitionLayout {
            zoomedPane = nil
            root = rest.inserting(pane, at: target, direction: direction, share: share ?? 0.5)
            if share == nil && SessionManager.shared.config.autoBalance { root = root.equalized() }
        }
        window?.invalidateCursorRects(for: self)
    }

    /// Lays the tab out as `newRoot`, keeping the views of the panes it already shows. Panes of
    /// other tabs must have left them first.
    func setRoot(_ newRoot: SplitNode) {
        transitionLayout {
            zoomedPane = nil
            root = newRoot
            floating.removeAll { newRoot.contains($0.pane) }
        }
        if !contains(focusedPane), let first = root.panes.first { setFocusedPane(first) }
        window?.invalidateCursorRects(for: self)
    }

    /// Exchanges the places of two panes of this tab.
    func swap(_ a: PaneKey, _ b: PaneKey) {
        guard contains(a), contains(b) else { return }
        root = root.swapping(a, b)
        // A card trades places too: with the pane of the splits, or with the other card.
        for i in floating.indices {
            if floating[i].pane == a { floating[i].pane = b } else if floating[i].pane == b { floating[i].pane = a }
        }
        syncViews()
        layoutPanes()
        window?.invalidateCursorRects(for: self)
    }

    /// Puts `new`, a pane of another tab, where `old` is; `old` leaves this tab.
    func replace(_ old: PaneKey, with new: PaneKey) {
        guard contains(old), !contains(new) else { return }
        let wasFocused = focusedPane == old
        if zoomedPane == old { zoomedPane = new }
        root = root.swapping(old, new)
        for i in floating.indices where floating[i].pane == old { floating[i].pane = new }
        syncViews()
        if wasFocused {
            focusedPane = new
            if window?.isKeyWindow == true, let view = views[new] { window?.makeFirstResponder(view) }
            refreshHeaders()
        }
        layoutPanes()
        window?.invalidateCursorRects(for: self)
    }

    /// Removes a pane's view. Returns true when the tab became empty.
    @discardableResult
    func remove(pane: PaneKey) -> Bool {
        guard contains(pane) else { return views.isEmpty }
        let wasFocused = pane == focusedPane
        if let i = floating.firstIndex(where: { $0.pane == pane }) {
            floatedFrames[pane] = nil
            transitionLayout {
                floating.remove(at: i)
                if floating.isEmpty { floatingHidden = false }
            }
            window?.invalidateCursorRects(for: self)
            if wasFocused { focus(shownCards.last?.pane ?? tiledFocus) }
            return false
        }
        if zoomedPane == pane { zoomedPane = nil }
        // The splits' last pane closing: the frontmost card takes their place.
        let promoted = root.panes == [pane] ? floating.last : nil
        guard var newRoot = root.removing(pane) ?? promoted.map({ .leaf($0.pane) }) else {
            finishLayoutMove()
            views[pane]?.detach()
            views[pane]?.removeFromSuperview()
            views.removeValue(forKey: pane)
            return true
        }
        if SessionManager.shared.config.autoBalance { newRoot = newRoot.equalized() }
        // Pick the spatially nearest pane as the new focus.
        var nextFocus = focusedPane
        if wasFocused {
            let oldFrame = views[pane]?.frame ?? .zero
            let center = CGPoint(x: oldFrame.midX, y: oldFrame.midY)
            let frames = newRoot.frames(in: splitArea, gap: splitGap)
            nextFocus = frames.min(by: { a, b in
                hypot(a.1.midX - center.x, a.1.midY - center.y) < hypot(b.1.midX - center.x, b.1.midY - center.y)
            })?.0 ?? (newRoot.panes.first ?? pane)
        }
        transitionLayout {
            root = newRoot
            if promoted != nil { floating.removeLast() }
            if floating.isEmpty { floatingHidden = false }
        }
        window?.invalidateCursorRects(for: self)
        if wasFocused { focus(nextFocus) }
        return false
    }

    /// Makes `pane` the focused pane (and first responder when the window is visible).
    func focus(_ pane: PaneKey) {
        guard let view = views[pane] else { return }
        if floatingHidden && isFloating(pane) { setFloatingHidden(false, focusing: false) }
        if let z = zoomedPane, z != pane, root.contains(pane) {
            transitionZoom { zoomedPane = nil }
        }
        setFocusedPane(pane)
        window?.makeFirstResponder(view)
    }

    /// Records the focused pane without touching the responder chain (called by the view
    /// itself when it becomes first responder).
    func setFocusedPane(_ pane: PaneKey) {
        guard views[pane] != nil else { return }
        let changed = pane != focusedPane
        focusedPane = pane
        if root.contains(pane) {
            lastTiledFocus = pane
        } else if let i = floating.firstIndex(where: { $0.pane == pane }), i != floating.count - 1 {
            // A card with the focus comes to the front.
            floating.append(floating.remove(at: i))
            restack()
            window?.invalidateCursorRects(for: self)
            SessionManager.shared.scheduleLayoutSave()
        }
        for view in views.values { view.needsRender = true }
        if changed { refreshHeaders() }
        if changed { updateDimming() }
        // Cards draw their focus in their outline, with or without the splits' headers.
        if changed && (showsHeaders || !floating.isEmpty) { layoutPanes() }
        if changed && !showsHeaders { updateFocusRing(frames: visibleFrames, cards: false) }
        if changed {
            controller?.focusedPaneChanged()
        }
    }

    func moveFocus(_ direction: FocusDirection) {
        guard zoomedPane == nil, let target = neighbor(of: focusedPane, direction, cards: true) else { return }
        focus(target)
    }

    /// The pane next to `pane` in `direction`, among the splits and, with `cards`, the cards.
    private func neighbor(of pane: PaneKey, _ direction: FocusDirection, cards: Bool = false) -> PaneKey? {
        guard let current = views[pane] else { return nil }
        let candidates = views.filter { id, _ in id != pane && (root.contains(id) || (cards && !floatingHidden)) }
        return Self.neighbor(of: current.frame, among: candidates.map { ($0.key, $0.value.frame) }, direction)
    }

    /// The frame next to `cur` in `direction`: overlapping ones first, then the closest. A card
    /// and a pane it lies over go by their centers.
    static func neighbor(of cur: CGRect, among frames: [(PaneKey, CGRect)], _ direction: FocusDirection) -> PaneKey? {
        var best: (PaneKey, CGFloat)?
        for (id, f) in frames {
            let overlap: Bool
            let distance: CGFloat
            if f.intersects(cur.insetBy(dx: 2, dy: 2)) {
                let ahead: CGFloat
                switch direction {
                case .left: ahead = cur.midX - f.midX
                case .right: ahead = f.midX - cur.midX
                case .up: ahead = cur.midY - f.midY
                case .down: ahead = f.midY - cur.midY
                }
                guard ahead > 0 else { continue }
                overlap = true
                distance = 0
            } else {
                switch direction {
                case .left:
                    guard f.maxX <= cur.minX + 2 else { continue }
                    overlap = f.maxY > cur.minY && f.minY < cur.maxY
                    distance = cur.minX - f.maxX
                case .right:
                    guard f.minX >= cur.maxX - 2 else { continue }
                    overlap = f.maxY > cur.minY && f.minY < cur.maxY
                    distance = f.minX - cur.maxX
                case .up:
                    guard f.maxY <= cur.minY + 2 else { continue }
                    overlap = f.maxX > cur.minX && f.minX < cur.maxX
                    distance = cur.minY - f.maxY
                case .down:
                    guard f.minY >= cur.maxY - 2 else { continue }
                    overlap = f.maxX > cur.minX && f.minX < cur.maxX
                    distance = f.minY - cur.maxY
                }
            }
            // Prefer overlapping neighbours, then the closest center.
            let centerDistance = hypot(f.midX - cur.midX, f.midY - cur.midY)
            let score = (overlap ? 0 : 100_000) + distance * 10 + centerDistance
            if best == nil || score < best!.1 {
                best = (id, score)
            }
        }
        return best?.0
    }

    /// Swaps the focused pane with its neighbour in `direction`; the focus stays on it.
    func movePane(_ direction: FocusDirection) {
        guard zoomedPane == nil, root.contains(focusedPane), let target = neighbor(of: focusedPane, direction) else {
            return
        }
        root = root.swapping(focusedPane, target)
        layoutPanes()
        window?.invalidateCursorRects(for: self)
        SessionManager.shared.scheduleLayoutSave()
    }

    /// Zooms `pane` (true), restores the splits (false), or toggles (nil).
    func zoom(_ pane: PaneKey, _ on: Bool?) {
        if isFloating(pane) {
            zoomCard(pane, on)
            return
        }
        guard root.contains(pane) else { return }
        if on ?? (zoomedPane != pane) {
            guard root.panes.count > 1 else { return }
            if focusedPane != pane { focus(pane) }
            transitionZoom { zoomedPane = pane }
        } else {
            guard zoomedPane != nil else { return }
            transitionZoom { zoomedPane = nil }
        }
        window?.invalidateCursorRects(for: self)
        SessionManager.shared.scheduleLayoutSave()
    }

    func toggleZoom() {
        if isFloating(focusedPane) {
            zoomCard(focusedPane, nil)
            return
        }
        guard root.panes.count > 1 || zoomedPane != nil else { return }
        transitionZoom { zoomedPane = zoomedPane == nil ? focusedPane : nil }
        window?.invalidateCursorRects(for: self)
        if let view = views[focusedPane] { window?.makeFirstResponder(view) }
        SessionManager.shared.scheduleLayoutSave()
    }

    // MARK: Floating panes

    /// Floats `pane` above the splits (true), docks it back into them (false), or toggles
    /// (nil). False when it cannot float: the splits keep at least one pane.
    @discardableResult
    func setFloating(_ pane: PaneKey, _ on: Bool?) -> Bool {
        let floats = isFloating(pane)
        guard on ?? !floats else {
            if floats {
                let anchor = dockAnchor(for: pane)
                dock(pane, nextTo: anchor.pane, direction: anchor.side, share: nil)
                focus(pane)
            }
            return true
        }
        guard !floats else { return true }
        guard canFloat(pane), let rest = root.removing(pane) else { return false }
        let frame = floatedFrames[pane] ?? FloatingGeometry.defaultFraction(index: floating.count)
        transitionLayout {
            if zoomedPane == pane { zoomedPane = nil }
            root = SessionManager.shared.config.autoBalance ? rest.equalized() : rest
            floating.append(FloatingCard(pane: pane, frame: frame))
            floatingHidden = false
        }
        focus(pane)
        window?.invalidateCursorRects(for: self)
        SessionManager.shared.scheduleLayoutSave()
        return true
    }

    /// Floats `cards`, panes of other tabs that left them first, above the splits.
    func addFloating(_ cards: [FloatingPane]) {
        let new = cards.filter { !contains($0.pane) }
        guard !new.isEmpty else { return }
        transitionLayout {
            floating += new.map(FloatingCard.init)
            floatingHidden = false
        }
        window?.invalidateCursorRects(for: self)
    }

    /// Floats `pane`, a new pane of this tab, above the splits and focuses it (unless
    /// `takeFocus` is false). Hidden cards show again.
    func addNewFloating(_ pane: PaneKey, takeFocus: Bool = true) {
        guard !contains(pane) else { return }
        let frame = FloatingGeometry.defaultFraction(index: floating.count)
        transitionLayout {
            floating.append(FloatingCard(pane: pane, frame: frame))
            floatingHidden = false
        }
        if takeFocus { focus(pane) }
        window?.invalidateCursorRects(for: self)
        SessionManager.shared.scheduleLayoutSave()
    }

    /// The rectangle a new card gets, for sizing its terminal before it opens.
    var newCardRect: CGRect {
        let rect = FloatingGeometry.rect(FloatingGeometry.defaultFraction(index: floating.count), in: floatArea)
        return terminalFrame(in: rect, cards: true)
    }

    /// Hides the cards (their programs keep running), focusing the splits, or shows them again,
    /// focusing the frontmost (with `focusing`).
    func setFloatingHidden(_ hidden: Bool, focusing: Bool = true) {
        guard hidden != floatingHidden, !floating.isEmpty else { return }
        let refocus = hidden && isFloating(focusedPane)
        floatingHidden = hidden
        layoutPanes()
        window?.invalidateCursorRects(for: self)
        if refocus {
            focus(tiledFocus)
        } else if !hidden && focusing, let front = floating.last {
            focus(front.pane)
        }
        SessionManager.shared.scheduleLayoutSave()
    }

    /// Puts floating `pane` back into the splits, on `direction`'s side of `target`.
    func dock(_ pane: PaneKey, nextTo target: PaneKey, direction: SplitDirName, share: CGFloat?) {
        guard let i = floating.firstIndex(where: { $0.pane == pane }), root.contains(target) else { return }
        floatedFrames[pane] = floating[i].frame
        transitionLayout {
            zoomedPane = nil
            floating.remove(at: i)
            root = root.inserting(pane, at: target, direction: direction, share: share ?? 0.5)
            if share == nil && SessionManager.shared.config.autoBalance { root = root.equalized() }
        }
        window?.invalidateCursorRects(for: self)
        SessionManager.shared.scheduleLayoutSave()
    }

    /// Where Dock Pane puts a card: next to the pane of the splits focused last (else the one
    /// nearest the card), on the side the card is over.
    private func dockAnchor(for pane: PaneKey) -> (pane: PaneKey, side: SplitDirName) {
        let card = cardFrames.first { $0.0 == pane }?.1 ?? floatArea
        let center = CGPoint(x: card.midX, y: card.midY)
        let tiles = root.frames(in: splitArea, gap: splitGap)
        let anchor = tiles.first { $0.0 == tiledFocus }
            ?? tiles.min { hypot($0.1.midX - center.x, $0.1.midY - center.y) < hypot($1.1.midX - center.x, $1.1.midY - center.y) }
        guard let anchor else { return (root.panes[0], .right) }
        return (anchor.0, FloatingGeometry.dockSide(center, in: anchor.1))
    }

    /// Zooms a card over the whole tab (true), back to its place (false), or toggles (nil).
    private func zoomCard(_ pane: PaneKey, _ on: Bool?) {
        guard let i = floating.firstIndex(where: { $0.pane == pane }) else { return }
        let zoomed = on ?? !floating[i].zoomed
        guard zoomed != floating[i].zoomed else { return }
        transitionLayout { floating[i].zoomed = zoomed }
        if focusedPane != pane { focus(pane) }
        window?.invalidateCursorRects(for: self)
        SessionManager.shared.scheduleLayoutSave()
    }

    /// The pane of the splits under `point` a card dropped there docks next to, its side, and
    /// the part of it the card would take.
    private func dockTarget(at point: NSPoint, excluding pane: PaneKey) -> (pane: PaneKey, side: SplitDirName, rect: CGRect)? {
        let tiles = visibleFrames.filter { root.contains($0.0) && $0.0 != pane }
        guard let (id, rect) = tiles.first(where: { $0.1.contains(point) }) else { return nil }
        let side = FloatingGeometry.dockSide(point, in: rect)
        return (id, side, FloatingGeometry.dockRect(rect, side: side, gap: splitGap))
    }

    /// Moves a card by its header, from a mouse down until the button comes up. Dropped with ⌘
    /// held over a pane of the splits, it docks there (where the indicator shows); Esc puts it
    /// back where it was.
    func trackCardDrag(_ pane: PaneKey, from event: NSEvent) {
        guard let window, let start = floating.first(where: { $0.pane == pane }), !start.zoomed,
              let startRect = cardFrames.first(where: { $0.0 == pane })?.1 else { return }
        let origin = convert(event.locationInWindow, from: nil)
        var moved = false
        var cancelled = false
        var target: (pane: PaneKey, side: SplitDirName, rect: CGRect)?
        window.trackEvents(matching: [.leftMouseDragged, .leftMouseUp, .keyDown, .flagsChanged],
                           timeout: NSEvent.foreverDuration, mode: .eventTracking) { [weak self] e, stop in
            guard let self, let e, let i = self.floating.firstIndex(where: { $0.pane == pane }) else {
                stop.pointee = true
                return
            }
            switch e.type {
            case .leftMouseUp:
                stop.pointee = true
            case .keyDown:
                guard e.keyCode == 53 else { return }
                cancelled = true
                self.floating[i].frame = start.frame
                stop.pointee = true
            default:
                let p = self.convert(e.type == .flagsChanged ? window.mouseLocationOutsideOfEventStream
                                     : e.locationInWindow, from: nil)
                if !moved && hypot(p.x - origin.x, p.y - origin.y) < 3 { return }
                moved = true
                let area = self.floatArea
                let rect = startRect.offsetBy(dx: p.x - origin.x, dy: p.y - origin.y)
                self.floating[i].frame = FloatingGeometry.fraction(of: FloatingGeometry.rect(
                    FloatingGeometry.fraction(of: rect, in: area), in: area), in: area)
                target = e.modifierFlags.contains(.command) ? self.dockTarget(at: p, excluding: pane) : nil
                self.dockIndicator.show(target?.rect)
                self.needsLayout = true
            }
        }
        dockIndicator.show(nil)
        if let target, !cancelled {
            dock(pane, nextTo: target.pane, direction: target.side, share: nil)
            focus(pane)
            return
        }
        needsLayout = true
        window.invalidateCursorRects(for: self)
        if moved && !cancelled { SessionManager.shared.scheduleLayoutSave() }
    }

    func equalize() {
        root = root.equalized()
        layoutPanes()
        window?.invalidateCursorRects(for: self)
        SessionManager.shared.scheduleLayoutSave()
    }

    func resizeFocused(axis: SplitAxis, delta: CGFloat) {
        let (node, done) = root.resizing(focusedPane, axis: axis, delta: delta)
        guard done else { return }
        root = node
        layoutPanes()
        window?.invalidateCursorRects(for: self)
        SessionManager.shared.scheduleLayoutSave()
    }

    /// Detaches every view (tab closing).
    func detachAll() {
        for view in views.values {
            view.detach()
        }
    }

    // MARK: Find bar

    func showFindBar() {
        if findBar == nil {
            let bar = FindBar()
            bar.content = self
            addSubview(bar, positioned: .above, relativeTo: nil)
            findBar = bar
        }
        guard let bar = findBar else { return }
        bar.isHidden = false
        layoutPanes()
        bar.activate()
    }

    func hideFindBar() {
        guard let bar = findBar, !bar.isHidden else { return }
        bar.isHidden = true
        if let view = views[focusedPane] {
            window?.makeFirstResponder(view)
        }
    }

    // MARK: Notices

    /// Shows `notices` stacked in the bottom right (see `Notices`).
    func setNotices(_ notices: [Notice]) {
        for badge in noticeBadges { badge.removeFromSuperview() }
        noticeBadges = notices.map { NoticeBadge($0) }
        for badge in noticeBadges {
            addSubview(badge, positioned: .above, relativeTo: nil)
        }
        layoutPanes()
        for badge in noticeBadges { window?.invalidateCursorRects(for: badge) }
    }

    // MARK: Pane headers

    /// Re-reads titles and focus into the headers (pane info changed, focus moved, theme).
    func refreshHeaders() {
        for (id, header) in headers { header.update(focused: id == focusedPane) }
        if showsHeaders { updateBackdrop() }
    }

    /// After a config reload: headers turned on or off, or the theme changed.
    func applyConfig() {
        for badge in noticeBadges { badge.applyTheme() }
        findBar?.applyTheme()
        layoutPanes()
        window?.invalidateCursorRects(for: self)
    }

    // MARK: Persistence

    func tabLayout(title: String?) -> TabLayout {
        TabLayout(title: title, root: root.layoutNode, focused: focusedPane.id, zoomed: zoomedPane?.id,
                  handoff: controller?.handoffID, wall: controller?.wall,
                  floating: floating.isEmpty ? nil : floating.map(\.saved),
                  floatingHidden: floatingHidden ? true : nil)
    }

    /// The host of this tab's panes.
    var host: HostId { root.panes.first?.host ?? localHost }
}

/// Flat fills under the terminals (see `TabContentView.updateBackdrop`), one layer each.
/// Clicks pass through.
private final class SplitBackdrop: NSView {
    private var fills: [CALayer] = []

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    func show(_ rects: [(NSRect, NSColor)]) {
        guard let layer else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        while fills.count < rects.count {
            let fill = CALayer()
            layer.addSublayer(fill)
            fills.append(fill)
        }
        for (i, fill) in fills.enumerated() {
            guard i < rects.count else {
                fill.isHidden = true
                continue
            }
            fill.isHidden = false
            fill.frame = rects[i].0
            fill.backgroundColor = rects[i].1.cgColor
        }
        CATransaction.commit()
    }
}

/// The accent ring around the focused pane in splits without cards (`window.active_pane_border`),
/// over the terminals, inside the pane's rectangle. Clicks pass through.
private final class FocusRing: NSView {
    static let width: CGFloat = 1.5

    var ring: NSRect?

    private let shape = CAShapeLayer()

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        shape.fillColor = nil
        layer?.addSublayer(shape)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        shape.contentsScale = window?.backingScaleFactor ?? 2
    }

    func refresh() {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        guard let ring, ring.width > Self.width, ring.height > Self.width else {
            shape.isHidden = true
            return
        }
        shape.isHidden = false
        shape.frame = bounds
        shape.path = CGPath(rect: ring.insetBy(dx: Self.width / 2, dy: Self.width / 2), transform: nil)
        shape.strokeColor = SessionManager.shared.config.chrome.accent.cgColor
        shape.lineWidth = Self.width
    }
}

/// Split cards' gutters (in the sidebar's color) and faint outlines, over the panes so the rounded
/// corners cover the terminals' square ones. Clicks pass through. A card corner near a window
/// corner follows the window's curve, so the two stay concentric. Shape layers, not a drawn
/// view: a layout move changes their paths each frame, which the render server rasterizes,
/// instead of the app repainting and uploading a window-sized bitmap.
private final class CardChrome: NSView {
    var area: NSRect = .zero
    var cards: [(rect: NSRect, focused: Bool)] = []
    /// `window.active_pane_border`: the focused card's outline in the accent color.
    var accentsFocused = false
    var opacity: CGFloat = 1
    var windowRadius: CGFloat = 0
    /// The window's corners, in this view.
    var windowCorners: [NSPoint] = []
    /// The divider being hovered or dragged, and how far its highlight has eased in (0...1).
    var highlight: (rect: NSRect, axis: SplitAxis, amount: CGFloat)?
    var scale: CGFloat = 2

    private let gutter = CAShapeLayer()
    private var outlines: [CAShapeLayer] = []
    private let bar = CALayer()

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.addSublayer(gutter)
        layer?.addSublayer(bar)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    /// Applies the properties above to the layers.
    func refresh() {
        guard let layer else { return }
        let chrome = SessionManager.shared.config.chrome
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        // A card opening or closing can be flatter than its outline; it has nothing to show.
        let cards = cards.filter { $0.rect.width >= 1 && $0.rect.height >= 1 }
        let radii = cards.map { corners(of: $0.rect) }
        gutter.frame = bounds
        gutter.contentsScale = scale
        gutter.path = CardPath.gutter(area, cards: zip(cards, radii).map { ($0.rect, $1) })
        gutter.fillColor = chrome.surface.withAlphaComponent(opacity).cgColor
        // A hairline around each card, a touch brighter around the focused pane. Light on dark
        // themes, so it reads as an edge. Each outline layer covers only its card, which bounds
        // the blend.
        while outlines.count < cards.count {
            let outline = CAShapeLayer()
            outline.fillColor = nil
            layer.insertSublayer(outline, below: bar)
            outlines.append(outline)
        }
        let blend = chrome.isDark ? CIFilter(name: "CIAdditionCompositing") : nil
        for (i, outline) in outlines.enumerated() {
            guard i < cards.count else {
                outline.isHidden = true
                continue
            }
            let (card, r) = (cards[i], radii[i])
            outline.isHidden = false
            outline.frame = card.rect
            outline.contentsScale = scale
            let accent = card.focused && accentsFocused
            let inset = accent ? FocusRing.width / 2 : 0.5
            let local = CGRect(origin: .zero, size: card.rect.size).insetBy(dx: inset, dy: inset)
            outline.path = CardPath.rounded(local, radii: r.map { $0 - inset })
            let color = card.focused ? chrome.strokeStrong : chrome.stroke
            outline.strokeColor = accent ? chrome.accent.cgColor
                : color.withAlphaComponent(color.alphaComponent * max(opacity, 0.6)).cgColor
            outline.lineWidth = accent ? FocusRing.width : ChromeStroke.width
            let filter = accent ? nil : blend
            if (outline.compositingFilter == nil) != (filter == nil) { outline.compositingFilter = filter }
        }
        if let highlight, highlight.amount > 0 {
            let r = highlight.rect
            let width = 1 + 2 * highlight.amount
            bar.isHidden = false
            bar.frame = highlight.axis == .horizontal
                ? NSRect(x: r.midX - width / 2, y: r.minY, width: width, height: r.height)
                : NSRect(x: r.minX, y: r.midY - width / 2, width: r.width, height: width)
            bar.backgroundColor = (chrome.surface.blended(withFraction: 0.35 * highlight.amount, of: chrome.contentPrimary)
                ?? chrome.contentPrimary).cgColor
        } else {
            bar.isHidden = true
        }
    }

    /// Radii of `rect`'s corners (top left, top right, bottom right, bottom left): the window's,
    /// less the distance, where a window corner is close; else the cards' own.
    private func corners(of rect: NSRect) -> [CGFloat] {
        let base = TabContentView.cardRadius
        let points = [NSPoint(x: rect.minX, y: rect.minY), NSPoint(x: rect.maxX, y: rect.minY),
                      NSPoint(x: rect.maxX, y: rect.maxY), NSPoint(x: rect.minX, y: rect.maxY)]
        return points.map { p in
            guard windowRadius > base else { return base }
            let inset = windowCorners.map { max(abs($0.x - p.x), abs($0.y - p.y)) }.min() ?? .infinity
            return inset < windowRadius ? max(base, windowRadius - inset) : base
        }
    }
}

/// The split cards' shapes, apart from `CardChrome` for tests.
enum CardPath {
    /// The pane area with the cards cut out. Cards are cut out of their union rather than with an
    /// even-odd path: during a zoom they overlap, and the overlap would flip back to gutter over
    /// the terminals.
    static func gutter(_ area: CGRect, cards: [(rect: CGRect, radii: [CGFloat])]) -> CGPath {
        let full = CGPath(rect: area, transform: nil)
        let holes = cards.map { rounded($0.rect, radii: $0.radii) }.reduce(nil) { (union: CGPath?, card: CGPath) in
            union.map { $0.union(card) } ?? card
        }
        return holes.map { full.subtracting($0) } ?? full
    }

    /// A rounded rectangle with its own radius per corner, clockwise from the top left (flipped).
    static func rounded(_ r: NSRect, radii: [CGFloat]) -> CGPath {
        let limit = min(r.width, r.height) / 2
        let (tl, tr, br, bl) = (min(radii[0], limit), min(radii[1], limit), min(radii[2], limit), min(radii[3], limit))
        let p = CGMutablePath()
        p.move(to: CGPoint(x: r.minX + tl, y: r.minY))
        p.addLine(to: CGPoint(x: r.maxX - tr, y: r.minY))
        p.addArc(tangent1End: CGPoint(x: r.maxX, y: r.minY), tangent2End: CGPoint(x: r.maxX, y: r.minY + tr), radius: tr)
        p.addLine(to: CGPoint(x: r.maxX, y: r.maxY - br))
        p.addArc(tangent1End: CGPoint(x: r.maxX, y: r.maxY), tangent2End: CGPoint(x: r.maxX - br, y: r.maxY), radius: br)
        p.addLine(to: CGPoint(x: r.minX + bl, y: r.maxY))
        p.addArc(tangent1End: CGPoint(x: r.minX, y: r.maxY), tangent2End: CGPoint(x: r.minX, y: r.maxY - bl), radius: bl)
        p.addLine(to: CGPoint(x: r.minX, y: r.minY + tl))
        p.addArc(tangent1End: CGPoint(x: r.minX, y: r.minY), tangent2End: CGPoint(x: r.minX + tl, y: r.minY), radius: tl)
        p.closeSubpath()
        return p
    }
}
