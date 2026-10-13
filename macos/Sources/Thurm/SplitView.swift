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

    /// Splits `target`, putting `newPane` on the given side.
    func inserting(_ newPane: PaneKey, at target: PaneKey, direction: SplitDirName) -> SplitNode {
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
    private let chrome = CardChrome()
    private(set) var focusedPane: PaneKey
    private(set) var zoomedPane: PaneKey?

    private(set) var findBar: FindBar?
    private var updateBadge: UpdateBadge?
    private var dragging: SplitDivider?
    /// Pane rectangles where a zoom change started; `zoomProgress` eases them to the layout.
    private var zoomFrom: [PaneKey: CGRect] = [:]
    private var zoomProgress = Tween(1)
    private let zoomMotion = MotionDriver()
    /// Path of the divider under the pointer or being dragged, and how far its highlight has
    /// eased in.
    private var highlightedPath: [Bool]?
    private var dividerEmphasis = Tween(0)
    private let dividerMotion = MotionDriver()

    static let gap: CGFloat = 1
    /// With pane headers, splits are rounded cards this far apart (and from the window edges):
    /// `window.pane_gap`.
    private var cardGap: CGFloat { SessionManager.shared.config.paneGap }
    static let cardRadius: CGFloat = 8

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
            && (hasMultipleVisiblePanes || fullHeight || (window as? TerminalWindow)?.tabStrip != nil)
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

    /// Each visible pane's rectangle: its card with headers, else its whole view. During a zoom
    /// change, the rectangles on the way from where the panes were.
    private var visibleFrames: [(PaneKey, CGRect)] {
        let targets = targetFrames
        let now = CACurrentMediaTime()
        guard !zoomProgress.isFinished(at: now) else { return targets }
        let t = CGFloat(zoomProgress.value(at: now))
        return targets.map { id, rect in
            (id, Self.blend(zoomFrom[id] ?? zoomFrom.values.first ?? rect, rect, t))
        }
    }

    private var targetFrames: [(PaneKey, CGRect)] {
        if let z = zoomedPane { return [(z, showsHeaders ? splitArea : paneArea)] }
        return root.frames(in: splitArea, gap: splitGap)
    }

    private static func blend(_ a: CGRect, _ b: CGRect, _ t: CGFloat) -> CGRect {
        CGRect(x: a.minX + (b.minX - a.minX) * t, y: a.minY + (b.minY - a.minY) * t,
               width: max(0, a.width + (b.width - a.width) * t),
               height: max(0, a.height + (b.height - a.height) * t))
    }

    init(root: SplitNode, focused: PaneKey, zoomed: PaneKey?) {
        self.root = root
        let panes = root.panes
        self.focusedPane = panes.contains(focused) ? focused : (panes.first ?? focused)
        if let z = zoomed, panes.contains(z) { self.zoomedPane = z } else { self.zoomedPane = nil }
        super.init(frame: NSRect(x: 0, y: 0, width: 800, height: 600))
        autoresizingMask = [.width, .height]
        addSubview(chrome)
        syncViews()
        setUpdateBadge(Updater.shared.badgeText)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    var paneIds: [PaneKey] { root.panes }
    var isEmpty: Bool { views.isEmpty }
    var focusedView: TerminalView? { views[focusedPane] }

    /// Panes currently on screen (all panes, or only the zoomed one).
    var visiblePaneIds: [PaneKey] {
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
            headers.removeValue(forKey: id)?.removeFromSuperview()
        }
        for id in root.panes where views[id] == nil {
            let view = TerminalView(pane: id)
            views[id] = view
            addSubview(view, positioned: .below, relativeTo: chrome)
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
        updateDimming()
        for (id, rect) in frames {
            guard let view = views[id] else { continue }
            guard cards else {
                view.frame = rect
                continue
            }
            // Inside the card's 1pt border: the header, then the terminal.
            let inner = rect.insetBy(dx: 1, dy: 1)
            let header = headers[id] ?? {
                let h = PaneHeader(pane: id)
                h.content = self
                headers[id] = h
                addSubview(h, positioned: .below, relativeTo: chrome)
                return h
            }()
            header.isHidden = false
            header.frame = NSRect(x: inner.minX, y: inner.minY, width: inner.width, height: PaneHeader.height)
            // A top pane at the window's leading edge (sidebar collapsed) keeps its icon and
            // title clear of the traffic lights and toolbar buttons.
            if let controls, inner.minX < controls, rect.minY <= splitArea.minY + 1 {
                header.leadingInset = controls - inner.minX + 10
            } else {
                header.leadingInset = PaneHeader.leading
            }
            view.frame = NSRect(x: inner.minX, y: inner.minY + PaneHeader.height, width: inner.width,
                                height: max(0, inner.height - PaneHeader.height))
        }
        chrome.isHidden = !cards
        if controls != settledControls {
            settledControls = controls
            recheckTitlebarControls()
        }
        if cards {
            chrome.frame = bounds
            chrome.area = full
            chrome.cards = frames.map {
                (rect: $0.1, focused: $0.0 == focusedPane && frames.count > 1)
            }
            chrome.opacity = controller?.opacity ?? SessionManager.shared.config.opacity
            chrome.windowRadius = (window as? TerminalWindow)?.cornerRadius ?? 0
            // The window's own corners (above the titlebar with native tabs, where this view
            // starts below it), from its frame in window coordinates.
            chrome.windowCorners = window.map { w in
                let s = w.frame.size
                return [NSPoint(x: 0, y: 0), NSPoint(x: s.width, y: 0), NSPoint(x: 0, y: s.height),
                        NSPoint(x: s.width, y: s.height)].map { convert($0, from: nil) }
            } ?? []
            chrome.needsDisplay = true
            syncDividerHighlight()
            for (id, header) in headers {
                header.update(focused: id == focusedPane)
            }
        } else {
            for header in headers.values { header.removeFromSuperview() }
            headers.removeAll()
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

    /// `TerminalWindow.leadingControlsMaxX` in this view, when the panes run under the titlebar.
    private var titlebarControlsMaxX: CGFloat? {
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
        chrome.needsDisplay = true
    }

    /// Highlights the divider at `path` (nil: none) and eases the highlight in or out.
    private func setDividerHighlight(_ path: [Bool]?) {
        if let path { highlightedPath = path }
        let target = path == nil ? 0.0 : 1.0
        guard dividerEmphasis.to != target else { return }
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
    /// Their terminals hold their grid until it settles, so the text does not reflow per frame.
    private func transitionZoom(_ change: () -> Void) {
        let duration = Motion.duration(Motion.standard)
        guard duration > 0 else {
            change()
            layoutPanes()
            return
        }
        zoomFrom = Dictionary(uniqueKeysWithValues: visibleFrames)
        change()
        zoomProgress = Tween(0)
        zoomProgress.move(to: 1, duration: duration, at: CACurrentMediaTime())
        views.values.forEach { $0.holdGridSize = true }
        zoomMotion.run(on: self, for: duration) { [weak self] time in
            self?.zoomTick(time)
        }
        layoutPanes()
    }

    private func zoomTick(_ now: CFTimeInterval) {
        guard zoomProgress.isFinished(at: now) else {
            needsLayout = true
            return
        }
        zoomFrom = [:]
        layoutPanes()
        views.values.forEach { $0.holdGridSize = false }
    }

    override func draw(_ dirtyRect: NSRect) {
        // Theme background under the titlebar and tab bar.
        let cfg = SessionManager.shared.config
        let area = paneArea
        colorFromRGB(cfg.theme.background, alpha: controller?.opacity ?? cfg.opacity).setFill()
        NSRect(x: bounds.minX, y: bounds.minY, width: bounds.width, height: area.minY - bounds.minY).fill()
        guard showsHeaders else {
            guard zoomedPane == nil else { return }
            cfg.dividerColor.setFill()
            for d in currentDividers {
                d.rect.fill()
            }
            return
        }
        // Cards: the header strips in the terminal's color, under the headers. The gutters and
        // outlines are `CardChrome`'s, drawn over the terminals' square corners.
        let bg = colorFromRGB(cfg.theme.background, alpha: controller?.opacity ?? cfg.opacity)
        bg.setFill()
        for (_, rect) in visibleFrames {
            NSRect(x: rect.minX, y: rect.minY, width: rect.width, height: 1 + PaneHeader.height).fill()
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
        dragging = root.dividers(in: splitArea, gap: splitGap).first { $0.path == d.path }
        needsLayout = true
    }

    override func mouseUp(with event: NSEvent) {
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
        for d in currentDividers {
            addCursorRect(d.hitRect, cursor: d.axis == .horizontal ? .resizeLeftRight : .resizeUpDown)
        }
    }

    // MARK: Tree operations

    /// Inserts `newPane` next to `target` (default: focused pane) and focuses it.
    func split(target: PaneKey? = nil, newPane: PaneKey, direction: SplitDirName) {
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
    func remove(pane: PaneKey) -> Bool {
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
            let frames = newRoot.frames(in: splitArea, gap: splitGap)
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
    func focus(_ pane: PaneKey) {
        guard let view = views[pane] else { return }
        if let z = zoomedPane, z != pane {
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
        for view in views.values { view.needsRender = true }
        if changed { refreshHeaders() }
        if changed { updateDimming() }
        if changed && showsHeaders { layoutPanes() }
        if changed {
            controller?.focusedPaneChanged()
        }
    }

    func moveFocus(_ direction: FocusDirection) {
        guard zoomedPane == nil, let current = views[focusedPane] else { return }
        let cur = current.frame
        var best: (PaneKey, CGFloat)?
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
        transitionZoom { zoomedPane = zoomedPane == nil ? focusedPane : nil }
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

    // MARK: Pane headers

    /// Re-reads titles and focus into the headers (pane info changed, focus moved, theme).
    func refreshHeaders() {
        for (id, header) in headers { header.update(focused: id == focusedPane) }
        if showsHeaders { needsDisplay = true }
    }

    /// After a config reload: headers turned on or off, or the theme changed.
    func applyConfig() {
        applyUpdateBadgeTheme()
        layoutPanes()
        window?.invalidateCursorRects(for: self)
    }

    // MARK: Persistence

    func tabLayout(title: String?) -> TabLayout {
        TabLayout(title: title, root: root.layoutNode, focused: focusedPane.id, zoomed: zoomedPane?.id,
                  handoff: controller?.handoffID)
    }

    /// The host of this tab's panes.
    var host: HostId { root.panes.first?.host ?? localHost }
}

/// Split cards' gutters (in the sidebar's color) and faint outlines, over the panes so the rounded
/// corners cover the terminals' square ones. Clicks pass through. A card corner near a window
/// corner follows the window's curve, so the two stay concentric.
private final class CardChrome: NSView {
    var area: NSRect = .zero
    var cards: [(rect: NSRect, focused: Bool)] = []
    var opacity: CGFloat = 1
    var windowRadius: CGFloat = 0
    /// The window's corners, in this view.
    var windowCorners: [NSPoint] = []
    /// The divider being hovered or dragged, and how far its highlight has eased in (0...1).
    var highlight: (rect: NSRect, axis: SplitAxis, amount: CGFloat)?

    override var isFlipped: Bool { true }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    override func draw(_ dirtyRect: NSRect) {
        let cfg = SessionManager.shared.config
        let base = colorFromRGB(cfg.theme.background)
        let fg = colorFromRGB(cfg.theme.foreground)
        let gutter = NSBezierPath(rect: area)
        gutter.windingRule = .evenOdd
        let radii = cards.map { corners(of: $0.rect) }
        for (card, r) in zip(cards, radii) {
            gutter.append(Self.path(card.rect, radii: r))
        }
        let bg = base.withAlphaComponent(opacity)
        (bg.shadow(withLevel: cfg.theme.isDark ? 0.18 : 0.05) ?? bg).setFill()
        gutter.fill()
        // A hairline a step off the card, a touch brighter around the focused pane.
        for (card, r) in zip(cards, radii) {
            let outline = Self.path(card.rect.insetBy(dx: 0.5, dy: 0.5), radii: r.map { $0 - 0.5 })
            let color = base.blended(withFraction: card.focused ? 0.1 : 0.06, of: fg) ?? fg
            color.withAlphaComponent(max(opacity, 0.6)).setStroke()
            outline.lineWidth = 1
            outline.stroke()
        }
        if let highlight, highlight.amount > 0 {
            let r = highlight.rect
            let width = 1 + 2 * highlight.amount
            let bar = highlight.axis == .horizontal
                ? NSRect(x: r.midX - width / 2, y: r.minY, width: width, height: r.height)
                : NSRect(x: r.minX, y: r.midY - width / 2, width: r.width, height: width)
            (base.blended(withFraction: 0.35 * highlight.amount, of: fg) ?? fg).setFill()
            NSBezierPath(rect: bar).fill()
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

    /// A rounded rectangle with its own radius per corner, clockwise from the top left (flipped).
    private static func path(_ r: NSRect, radii: [CGFloat]) -> NSBezierPath {
        let limit = min(r.width, r.height) / 2
        let (tl, tr, br, bl) = (min(radii[0], limit), min(radii[1], limit), min(radii[2], limit), min(radii[3], limit))
        let p = NSBezierPath()
        p.move(to: NSPoint(x: r.minX + tl, y: r.minY))
        p.line(to: NSPoint(x: r.maxX - tr, y: r.minY))
        p.appendArc(from: NSPoint(x: r.maxX, y: r.minY), to: NSPoint(x: r.maxX, y: r.minY + tr), radius: tr)
        p.line(to: NSPoint(x: r.maxX, y: r.maxY - br))
        p.appendArc(from: NSPoint(x: r.maxX, y: r.maxY), to: NSPoint(x: r.maxX - br, y: r.maxY), radius: br)
        p.line(to: NSPoint(x: r.minX + bl, y: r.maxY))
        p.appendArc(from: NSPoint(x: r.minX, y: r.maxY), to: NSPoint(x: r.minX, y: r.maxY - bl), radius: bl)
        p.line(to: NSPoint(x: r.minX, y: r.minY + tl))
        p.appendArc(from: NSPoint(x: r.minX, y: r.minY), to: NSPoint(x: r.minX + tl, y: r.minY), radius: tl)
        p.close()
        return p
    }
}
