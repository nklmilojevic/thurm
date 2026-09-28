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
    case leaf(UInt64)
    case split(axis: SplitAxis, ratio: CGFloat, first: SplitNode, second: SplitNode)

    var panes: [UInt64] {
        switch self {
        case .leaf(let id):
            return [id]
        case .split(_, _, let first, let second):
            return first.panes + second.panes
        }
    }

    func contains(_ id: UInt64) -> Bool {
        switch self {
        case .leaf(let leaf):
            return leaf == id
        case .split(_, _, let first, let second):
            return first.contains(id) || second.contains(id)
        }
    }

    /// Splits `target`, putting `newPane` on the given side.
    func inserting(_ newPane: UInt64, at target: UInt64, direction: SplitDirName) -> SplitNode {
        switch self {
        case .leaf(let id):
            guard id == target else { return self }
            switch direction {
            case .right: return .split(axis: .horizontal, ratio: 0.5, first: self, second: .leaf(newPane))
            case .left: return .split(axis: .horizontal, ratio: 0.5, first: .leaf(newPane), second: self)
            case .down: return .split(axis: .vertical, ratio: 0.5, first: self, second: .leaf(newPane))
            case .up: return .split(axis: .vertical, ratio: 0.5, first: .leaf(newPane), second: self)
            }
        case .split(let axis, let ratio, let first, let second):
            return .split(axis: axis, ratio: ratio,
                          first: first.inserting(newPane, at: target, direction: direction),
                          second: second.inserting(newPane, at: target, direction: direction))
        }
    }

    /// Removes a pane; nil when the tree becomes empty.
    func removing(_ id: UInt64) -> SplitNode? {
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

    /// Frames of every pane inside `rect`, leaving `gap` points for each divider.
    func frames(in rect: CGRect, gap: CGFloat) -> [(UInt64, CGRect)] {
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
    func resizing(_ pane: UInt64, axis: SplitAxis, delta: CGFloat) -> (SplitNode, Bool) {
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
            return .pane(id: id)
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
    private(set) var views: [UInt64: TerminalView] = [:]
    private(set) var focusedPane: UInt64
    private(set) var zoomedPane: UInt64?

    private(set) var findBar: FindBar?
    private var updateBadge: UpdateBadge?
    private var dragging: SplitDivider?

    static let gap: CGFloat = 1

    init(root: SplitNode, focused: UInt64, zoomed: UInt64?) {
        self.root = root
        let panes = root.panes
        self.focusedPane = panes.contains(focused) ? focused : (panes.first ?? focused)
        if let z = zoomed, panes.contains(z) { self.zoomedPane = z } else { self.zoomedPane = nil }
        super.init(frame: NSRect(x: 0, y: 0, width: 800, height: 600))
        autoresizingMask = [.width, .height]
        syncViews()
        setUpdateBadge(Updater.shared.badgeText)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    var paneIds: [UInt64] { root.panes }
    var isEmpty: Bool { views.isEmpty }
    var focusedView: TerminalView? { views[focusedPane] }

    /// Panes currently on screen (all panes, or only the zoomed one).
    var visiblePaneIds: [UInt64] {
        if let z = zoomedPane { return [z] }
        return root.panes
    }

    var hasMultipleVisiblePanes: Bool { visiblePaneIds.count > 1 }

    // MARK: View management

    /// Creates views for new panes and removes views of panes no longer in the tree.
    private func syncViews() {
        let ids = Set(root.panes)
        for (id, view) in views where !ids.contains(id) {
            view.detach()
            view.removeFromSuperview()
            views.removeValue(forKey: id)
        }
        for id in root.panes where views[id] == nil {
            let view = TerminalView(pane: id)
            views[id] = view
            addSubview(view, positioned: .below, relativeTo: findBar)
        }
        needsLayout = true
        needsDisplay = true
    }

    override func layout() {
        super.layout()
        layoutPanes()
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        layoutPanes()
    }

    /// `bounds` minus the safe-area insets (the titlebar and tab bar sit on top of the rest).
    var paneArea: NSRect {
        let inset = safeAreaInsets
        return NSRect(x: bounds.minX + inset.left, y: bounds.minY + inset.top,
                      width: max(0, bounds.width - inset.left - inset.right),
                      height: max(0, bounds.height - inset.top - inset.bottom))
    }

    func layoutPanes() {
        let full = paneArea
        if let z = zoomedPane, let zv = views[z] {
            for (id, view) in views {
                view.isHidden = id != z
            }
            zv.frame = full
        } else {
            for (id, rect) in root.frames(in: full, gap: TabContentView.gap) {
                guard let view = views[id] else { continue }
                view.isHidden = false
                view.frame = rect
            }
        }
        for view in views.values {
            view.visibilityChanged()
        }
        if let bar = findBar, !bar.isHidden {
            let size = bar.fittingSize
            let width = min(max(size.width, 320), max(200, full.width - 24))
            bar.frame = NSRect(x: full.maxX - width - 12, y: full.minY + 8, width: width,
                               height: max(size.height, 34))
        }
        if let badge = updateBadge {
            let size = badge.fittingSize
            badge.frame = NSRect(x: full.maxX - size.width - 12, y: full.maxY - size.height - 10,
                                 width: size.width, height: size.height)
        }
        needsDisplay = true
    }

    private var currentDividers: [SplitDivider] {
        if zoomedPane != nil { return [] }
        return root.dividers(in: paneArea, gap: TabContentView.gap)
    }

    override func draw(_ dirtyRect: NSRect) {
        // Theme background under the titlebar and tab bar.
        let cfg = SessionManager.shared.config
        let area = paneArea
        colorFromRGB(cfg.theme.background, alpha: cfg.opacity).setFill()
        NSRect(x: bounds.minX, y: bounds.minY, width: bounds.width, height: area.minY - bounds.minY).fill()
        guard zoomedPane == nil else { return }
        SessionManager.shared.config.dividerColor.setFill()
        for d in currentDividers {
            d.rect.fill()
        }
    }

    func dividerAt(_ point: NSPoint) -> SplitDivider? {
        currentDividers.first { $0.hitRect.contains(point) }
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        let local = convert(point, from: superview)
        if let bar = findBar, !bar.isHidden, bar.frame.contains(local) {
            return super.hitTest(point)
        }
        if dividerAt(local) != nil {
            return self
        }
        return super.hitTest(point)
    }

    // MARK: Divider dragging

    override func mouseDown(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        dragging = dividerAt(p)
        if dragging == nil { super.mouseDown(with: event) }
    }

    override func mouseDragged(with event: NSEvent) {
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
        dragging = root.dividers(in: paneArea, gap: TabContentView.gap).first { $0.path == d.path }
        needsLayout = true
    }

    override func mouseUp(with event: NSEvent) {
        if dragging != nil {
            dragging = nil
            SessionManager.shared.scheduleLayoutSave()
        } else {
            super.mouseUp(with: event)
        }
    }

    override func resetCursorRects() {
        for d in currentDividers {
            addCursorRect(d.hitRect, cursor: d.axis == .horizontal ? .resizeLeftRight : .resizeUpDown)
        }
    }

    // MARK: Tree operations

    /// Inserts `newPane` next to `target` (default: focused pane) and focuses it.
    func split(target: UInt64? = nil, newPane: UInt64, direction: SplitDirName) {
        let t = target.flatMap { root.contains($0) ? $0 : nil } ?? focusedPane
        zoomedPane = nil
        root = root.inserting(newPane, at: t, direction: direction)
        syncViews()
        layoutPanes()
        focus(newPane)
        window?.invalidateCursorRects(for: self)
    }

    /// Removes a pane's view. Returns true when the tab became empty.
    @discardableResult
    func remove(pane: UInt64) -> Bool {
        guard root.contains(pane) else { return views.isEmpty }
        let wasFocused = pane == focusedPane
        if zoomedPane == pane { zoomedPane = nil }
        guard let newRoot = root.removing(pane) else {
            views[pane]?.detach()
            views[pane]?.removeFromSuperview()
            views.removeValue(forKey: pane)
            return true
        }
        // Pick the spatially nearest pane as the new focus.
        var nextFocus = focusedPane
        if wasFocused {
            let oldFrame = views[pane]?.frame ?? .zero
            let center = CGPoint(x: oldFrame.midX, y: oldFrame.midY)
            let frames = newRoot.frames(in: paneArea, gap: TabContentView.gap)
            nextFocus = frames.min(by: { a, b in
                hypot(a.1.midX - center.x, a.1.midY - center.y) < hypot(b.1.midX - center.x, b.1.midY - center.y)
            })?.0 ?? (newRoot.panes.first ?? pane)
        }
        root = newRoot
        syncViews()
        layoutPanes()
        window?.invalidateCursorRects(for: self)
        if wasFocused { focus(nextFocus) }
        return false
    }

    /// Makes `pane` the focused pane (and first responder when the window is visible).
    func focus(_ pane: UInt64) {
        guard let view = views[pane] else { return }
        if let z = zoomedPane, z != pane {
            zoomedPane = nil
            layoutPanes()
        }
        setFocusedPane(pane)
        window?.makeFirstResponder(view)
    }

    /// Records the focused pane without touching the responder chain (called by the view
    /// itself when it becomes first responder).
    func setFocusedPane(_ pane: UInt64) {
        guard views[pane] != nil else { return }
        let changed = pane != focusedPane
        focusedPane = pane
        for view in views.values { view.needsRender = true }
        if changed {
            controller?.focusedPaneChanged()
        }
    }

    func moveFocus(_ direction: FocusDirection) {
        guard zoomedPane == nil, let current = views[focusedPane] else { return }
        let cur = current.frame
        var best: (UInt64, CGFloat)?
        for (id, view) in views where id != focusedPane {
            let f = view.frame
            let overlap: Bool
            let distance: CGFloat
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
            // Prefer overlapping neighbours, then the closest center.
            let centerDistance = hypot(f.midX - cur.midX, f.midY - cur.midY)
            let score = (overlap ? 0 : 100_000) + distance * 10 + centerDistance
            if best == nil || score < best!.1 {
                best = (id, score)
            }
        }
        if let target = best?.0 {
            focus(target)
        }
    }

    func toggleZoom() {
        guard root.panes.count > 1 || zoomedPane != nil else { return }
        zoomedPane = zoomedPane == nil ? focusedPane : nil
        layoutPanes()
        window?.invalidateCursorRects(for: self)
        if let view = views[focusedPane] { window?.makeFirstResponder(view) }
        SessionManager.shared.scheduleLayoutSave()
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

    // MARK: Update badge

    /// Shows `text` in the bottom right (see `Updater`), or removes the badge when nil.
    func setUpdateBadge(_ text: String?) {
        guard let text else {
            updateBadge?.removeFromSuperview()
            updateBadge = nil
            return
        }
        let badge = updateBadge ?? UpdateBadge()
        badge.text = text
        if badge.superview !== self {
            addSubview(badge, positioned: .above, relativeTo: nil)
        }
        updateBadge = badge
        layoutPanes()
        window?.invalidateCursorRects(for: badge)
    }

    func applyUpdateBadgeTheme() {
        updateBadge?.applyTheme()
    }

    // MARK: Persistence

    func tabLayout(title: String?) -> TabLayout {
        TabLayout(title: title, root: root.layoutNode, focused: focusedPane, zoomed: zoomedPane)
    }
}
