import AppKit
import QuartzCore
import CThurm

/// Grid size last reported to the daemon.
private struct GridSize: Equatable {
    var cols: Int
    var rows: Int
    var cellWidth: Int
    var cellHeight: Int
}

/// Mouse position in grid terms.
private struct CellPosition {
    var col: Int
    var row: Int
    var rightHalf: Bool
    var x: Int
    var y: Int
}

/// One terminal pane: a CAMetalLayer backed view that renders the daemon's grid and forwards
/// keyboard (NSTextInputClient, IME), mouse and scroll input.
final class TerminalView: NSView, NSTextInputClient {
    let pane: PaneKey

    private let renderer = TerminalRenderer()
    private var shaper: FontShaper?
    private var frameLink: CADisplayLink?
    /// Wakes the paused display link for the next cursor blink phase.
    private var blinkTimer: Timer?
    /// Force a redraw on the next display refresh.
    var needsRender = true {
        didSet { if needsRender { wakeLink() } }
    }
    private var subscribed = false
    private var reportedSize: GridSize?
    private var reportedFocus: Bool?
    private var waitingForResizeFrame = false
    /// The daemon didn't answer a resize in time: draw the frame we have at the new size.
    private var resizeTimedOut = false
    private var resizeFrameTimeout: DispatchWorkItem?
    private(set) var cols = 0
    private(set) var rows = 0

    // Keyboard / IME state.
    private var markedText = NSMutableAttributedString()
    /// Non-nil while `interpretKeyEvents` runs inside keyDown; collects committed text.
    private var keyTextAccumulator: [String]?
    /// Set when the input method passed the key on as a command (`doCommand`) during keyDown.
    private var keyCommandIssued = false

    // Mouse state.
    private var scrollAccumulator: CGFloat = 0
    /// Whole lines scrolled since the last display tick, sent once per tick so each vsync
    /// gets at most one daemon round trip (and the frame is back well before the next one).
    private var pendingScroll: (lines: Int, col: Int, row: Int, mods: UInt8)?

    // Smooth scrolling (trackpad, primary screen): the scrollback position in fractional
    // lines. The daemon is asked for its whole part once per display tick; the fraction is
    // drawn as a pixel offset from whatever offset the latest frame actually has, so a late
    // frame never jumps.
    private var smoothActive = false
    private var smoothPos: CGFloat = 0
    private var sentOffset = 0
    private var lastSmoothInput: CFTimeInterval = 0
    private var pressedButton: UInt8?
    private var rightPressReported = false
    private var trackingArea: NSTrackingArea?
    private var lastMouseLocation: NSPoint?
    private var hoverLink: UInt16 = 0
    private var hoverSpan: (row: Int, start: Int, end: Int)?

    // Animation state.
    private var blinkEpoch = CACurrentMediaTime()
    private var lastBlinkOn = true
    private var flashStart: CFTimeInterval = 0

    // Overlays.
    private var lockBadge: NSImageView?
    private var markedLabel: NSTextField?
    private var toastView: NSView?
    private var restoredToastShown = false
    private var progressBar: ProgressBarView?
    private var offlineOverlay: OfflineOverlay?

    private static let urlRegex: NSRegularExpression? = try? NSRegularExpression(
        pattern: "(?:https?|ftp|file)://[^\\s<>\"'`]+|mailto:[^\\s<>\"'`]+", options: [])

    init(pane: PaneKey) {
        self.pane = pane
        super.init(frame: NSRect(x: 0, y: 0, width: 400, height: 300))
        wantsLayer = true
        layerContentsPlacement = .topLeft
        registerForDraggedTypes([.fileURL, .URL, .png, .tiff, .string])
        setProgress(SessionManager.shared.panes[pane]?.progress)
        if pane.isRemote { setOffline(SessionManager.shared.offlineMessage(for: pane.host)) }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    deinit {
        frameLink?.invalidate()
        blinkTimer?.invalidate()
        NotificationCenter.default.removeObserver(self)
    }

    // MARK: Layer

    override func makeBackingLayer() -> CALayer {
        let layer = CAMetalLayer()
        layer.device = MetalContext.shared?.device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        // Present as soon as a frame is drawn instead of on the next vsync (Alacritty's
        // `SwapInterval::DontWait`); pacing comes from `scheduleRender`.
        layer.displaySyncEnabled = !Perf.tickRendering
        // Opaque (no WindowServer blending) whenever the window's opacity is 1; see `renderNow`.
        layer.isOpaque = false
        layer.colorspace = CGColorSpace(name: CGColorSpace.sRGB)
        layer.contentsScale = NSScreen.main?.backingScaleFactor ?? 2
        // On a size change (splits, window resize) keep the last frame unscaled at the top
        // left until the next one is drawn, instead of stretching it to the new bounds.
        layer.contentsGravity = .topLeft
        // A held frame larger than a shrinking view must not spill over its neighbours.
        layer.masksToBounds = true
        layer.needsDisplayOnBoundsChange = false
        return layer
    }

    private var metalLayer: CAMetalLayer? { layer as? CAMetalLayer }

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { metalLayer?.isOpaque ?? false }
    override var acceptsFirstResponder: Bool { true }

    private var container: TabContentView? { superview as? TabContentView }
    private var config: AppConfig { SessionManager.shared.config }

    private var backingScale: CGFloat {
        window?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 2
    }

    private var padXPixels: Int { Int((config.paddingX * backingScale).rounded()) }
    private var padYPixels: Int { Int((config.paddingY * backingScale).rounded()) }

    /// Cell size in points (for layout / IME).
    private var cellSizePoints: CGSize {
        guard let s = shaper else { return CGSize(width: 8, height: 16) }
        return CGSize(width: CGFloat(s.cellWidth) / backingScale, height: CGFloat(s.cellHeight) / backingScale)
    }

    /// True when this view receives keyboard input right now.
    var isKeyFocused: Bool {
        guard let window = window else { return false }
        return NSApp.isActive && window.isKeyWindow && window.firstResponder === self
    }

    var snapshotModes: UInt32 { renderer.snapshot.info.modes }

    // MARK: Window attachment

    override func viewWillMove(toWindow newWindow: NSWindow?) {
        super.viewWillMove(toWindow: newWindow)
        if let old = window {
            let nc = NotificationCenter.default
            nc.removeObserver(self, name: NSWindow.didBecomeKeyNotification, object: old)
            nc.removeObserver(self, name: NSWindow.didResignKeyNotification, object: old)
            nc.removeObserver(self, name: NSWindow.didChangeOcclusionStateNotification, object: old)
            nc.removeObserver(self, name: NSWindow.didChangeScreenNotification, object: old)
        }
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        frameLink?.invalidate()
        frameLink = nil
        guard let window = window else {
            updateSubscription()
            return
        }
        let nc = NotificationCenter.default
        nc.addObserver(self, selector: #selector(windowKeyStateChanged(_:)),
                       name: NSWindow.didBecomeKeyNotification, object: window)
        nc.addObserver(self, selector: #selector(windowKeyStateChanged(_:)),
                       name: NSWindow.didResignKeyNotification, object: window)
        nc.addObserver(self, selector: #selector(windowOcclusionChanged(_:)),
                       name: NSWindow.didChangeOcclusionStateNotification, object: window)
        nc.addObserver(self, selector: #selector(windowScreenChanged(_:)),
                       name: NSWindow.didChangeScreenNotification, object: window)

        let link = displayLink(target: self, selector: #selector(displayTick(_:)))
        link.add(to: .main, forMode: .common)
        frameLink = link
        updateFrameRate()

        updateScale()
        updateSubscription()
        needsRender = true
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        updateScale()
    }

    override func setFrameSize(_ newSize: NSSize) {
        let changed = newSize != frame.size
        if !holdGridSize && !waitingForResizeFrame {
            metalLayer?.contentsGravity = .topLeft
        }
        super.setFrameSize(newSize)
        if holdGridSize {
            // Keep the current frame until the sidebar layout reaches its final size.
            layoutOverlays()
            return
        }
        updateGridSize()
        updateDrawableSize()
        layoutOverlays()
        needsRender = true
        // Draw only when the grid matches the new size. Keep the last image while waiting.
        if changed, window != nil, subscribed, !isHiddenOrHasHiddenAncestor { renderNow() }
    }

    /// Stops rendering and unsubscribes (pane closed or tab closing).
    func detach() {
        if subscribed {
            subscribed = false
            Core.shared.unsubscribe(pane)
        }
        frameLink?.invalidate()
        frameLink = nil
        blinkTimer?.invalidate()
        blinkTimer = nil
        NSObject.cancelPreviousPerformRequests(withTarget: self)
    }

    /// Called by the container after layout (zoom changes hide panes).
    func visibilityChanged() {
        updateSubscription()
        wakeLink()
    }

    /// After reconnecting to the daemon: subscribe and report the size again.
    func resubscribe() {
        subscribed = false
        reportedSize = nil
        reportedFocus = nil
        renderer.resetImages()
        updateSubscription()
        reportFocus()
    }

    /// Font or config change: new metrics, new grid size.
    func metricsChanged() {
        shaper = nil
        updateScale()
        updateMarkedTextOverlay()
    }

    private func updateScale() {
        let scale = backingScale
        metalLayer?.contentsScale = scale
        shaper = FontShaper.shared(scale: scale)
        updateGridSize()
        updateDrawableSize()
        needsRender = true
    }

    private func updateDrawableSize() {
        guard !waitingForResizeFrame else { return }
        guard let layer = metalLayer else { return }
        let scale = backingScale
        let size = CGSize(width: max(1, (bounds.width * scale).rounded()),
                          height: max(1, (bounds.height * scale).rounded()))
        if layer.drawableSize != size {
            layer.drawableSize = size
        }
    }

    /// Computes cols/rows from the view size and tells the daemon when they change.
    /// Hold the grid size during a sidebar toggle. Resize once after layout is complete.
    var holdGridSize = false {
        didSet {
            if holdGridSize {
                waitingForResizeFrame = true
                // Pin the held frame to the left edge so the text slides with the sidebar.
                metalLayer?.contentsGravity = .topLeft
            }
            guard oldValue, !holdGridSize else { return }
            updateGridSize()
            updateDrawableSize()
            needsRender = true
        }
    }

    private func updateGridSize() {
        guard !holdGridSize else { return }
        guard let s = shaper, bounds.width > 1, bounds.height > 1, window != nil, !isHidden else { return }
        let scale = backingScale
        let availW = bounds.width * scale - CGFloat(2 * padXPixels)
        let availH = bounds.height * scale - CGFloat(2 * padYPixels)
        let newCols = max(2, Int(availW / CGFloat(s.cellWidth)))
        let newRows = max(1, Int(availH / CGFloat(s.cellHeight)))
        cols = newCols
        rows = newRows
        let size = GridSize(cols: newCols, rows: newRows, cellWidth: s.cellWidth, cellHeight: s.cellHeight)
        if size != reportedSize && subscribed {
            reportedSize = size
            waitingForResizeFrame = true
            Core.shared.resize(pane, cols: newCols, rows: newRows, cellWidth: s.cellWidth, cellHeight: s.cellHeight)
            scheduleResizeFrameTimeout()
        }
    }

    /// A resize the daemon never answers (busy, or gone) must not leave the old frame on
    /// screen for good.
    private func scheduleResizeFrameTimeout() {
        resizeFrameTimeout?.cancel()
        resizeTimedOut = false
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.waitingForResizeFrame, !self.holdGridSize else { return }
            self.resizeTimedOut = true
            self.waitingForResizeFrame = false
            self.updateDrawableSize()
            self.needsRender = true
        }
        resizeFrameTimeout = work
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(250), execute: work)
    }

    private func updateSubscription() {
        var visible = false
        if let window = window, !isHiddenOrHasHiddenAncestor {
            visible = window.occlusionState.contains(.visible) || Perf.ignoreOcclusion
        }
        if visible && !subscribed {
            subscribed = true
            Core.shared.subscribe(pane)
            reportedSize = nil
            updateGridSize()
            needsRender = true
            maybeShowRestoredToast()
        } else if !visible && subscribed {
            subscribed = false
            Core.shared.unsubscribe(pane)
        }
    }

    @objc private func windowKeyStateChanged(_ note: Notification) {
        reportFocus()
        needsRender = true
    }

    @objc private func windowOcclusionChanged(_ note: Notification) {
        updateSubscription()
        wakeLink()
    }

    @objc private func windowScreenChanged(_ note: Notification) {
        updateFrameRate()
    }

    // MARK: Focus

    override func becomeFirstResponder() -> Bool {
        let ok = super.becomeFirstResponder()
        if ok {
            container?.setFocusedPane(pane)
            reportFocus(hasResponder: true)
            resetBlink()
            needsRender = true
        }
        return ok
    }

    override func resignFirstResponder() -> Bool {
        let ok = super.resignFirstResponder()
        if ok {
            reportFocus(hasResponder: false)
            needsRender = true
        }
        return ok
    }

    /// Tells the daemon about focus changes (for focus reporting mode and agent detection).
    func reportFocus(hasResponder: Bool? = nil) {
        let responder = hasResponder ?? (window?.firstResponder === self)
        let focused = responder && NSApp.isActive && (window?.isKeyWindow ?? false)
        if focused != reportedFocus {
            reportedFocus = focused
            Core.shared.focus(pane, focused: focused)
        }
    }

    private func resetBlink() {
        blinkEpoch = CACurrentMediaTime()
        if !lastBlinkOn {
            lastBlinkOn = true
            needsRender = true
        }
    }

    // MARK: Rendering

    /// Asks for the screen's full rate (120 Hz on ProMotion) while the link runs. The link is
    /// paused whenever nothing animates (`updateLinkState`), so the floor only matters for
    /// how far the system may throttle an active pane.
    private func updateFrameRate() {
        guard let link = frameLink else { return }
        let maxFPS = Float(window?.screen?.maximumFramesPerSecond ?? 60)
        link.preferredFrameRateRange = CAFrameRateRange(minimum: min(10, maxFPS), maximum: maxFPS,
                                                        preferred: maxFPS)
    }

    /// Resumes the display link for at least one tick.
    private func wakeLink() {
        guard let link = frameLink, link.isPaused else { return }
        link.isPaused = false
    }

    /// True while the next refresh has work: a pending draw, scroll or animation.
    private var wantsTick: Bool {
        needsRender || pendingScroll != nil || smoothActive || flashStart > 0 || renderer.wantsAnotherFrame
    }

    /// Pauses the display link when nothing animates, so an idle pane costs no wakeups. New
    /// output is drawn by `frameArrived`; input, focus and visibility changes wake the link;
    /// a one-shot timer wakes it for the next cursor blink.
    private func updateLinkState() {
        guard let link = frameLink else { return }
        if Perf.tickRendering || wantsTick {
            link.isPaused = false
            blinkTimer?.invalidate()
            blinkTimer = nil
            return
        }
        link.isPaused = true
        let blinking = config.cursorBlink || renderer.snapshot.info.cursor_blinking
        guard blinking, isKeyFocused, window != nil, subscribed, !isHiddenOrHasHiddenAncestor else {
            blinkTimer?.invalidate()
            blinkTimer = nil
            return
        }
        // An early wake is harmless: the tick finds no phase change and schedules again.
        if blinkTimer?.isValid == true { return }
        let elapsed = CACurrentMediaTime() - blinkEpoch
        let next = (floor(elapsed / 0.53) + 1) * 0.53 - elapsed
        let timer = Timer(timeInterval: next + 0.001, repeats: false) { [weak self] _ in
            self?.wakeLink()
        }
        RunLoop.main.add(timer, forMode: .common)
        blinkTimer = timer
    }

    @objc private func displayTick(_ link: CADisplayLink) {
        guard window != nil, subscribed, !isHiddenOrHasHiddenAncestor else {
            // Nothing to draw until the pane is shown again, which wakes the link.
            link.isPaused = true
            blinkTimer?.invalidate()
            blinkTimer = nil
            return
        }
        defer { updateLinkState() }
        if Perf.enabled { Perf.shared.tick(pane: pane, link: link) }
        if let s = pendingScroll {
            pendingScroll = nil
            Core.shared.wheel(pane, lines: s.lines, col: s.col, row: s.row, mods: s.mods)
            if Perf.enabled { Perf.shared.wheelSent(pane: pane) }
        }
        var render = needsRender
        if smoothActive {
            advanceSmoothScroll(CACurrentMediaTime())
            render = true
        }
        if Core.shared.takeDirty(pane) { render = true }

        let now = CACurrentMediaTime()
        let blinking = config.cursorBlink || renderer.snapshot.info.cursor_blinking
        if blinking && isKeyFocused {
            let on = Int((now - blinkEpoch) / 0.53) % 2 == 0
            if on != lastBlinkOn {
                lastBlinkOn = on
                render = true
            }
        } else if !lastBlinkOn {
            lastBlinkOn = true
            render = true
        }
        if flashStart > 0 { render = true }
        if renderer.wantsAnotherFrame { render = true }
        if render { renderNow() }
    }

    private func advanceSmoothScroll(_ now: CFTimeInterval) {
        let info = renderer.snapshot.info
        let idle = now - lastSmoothInput
        if idle > 0.08 {
            // Gesture (and momentum) over: ease onto the nearest whole line.
            let target = smoothPos.rounded()
            smoothPos += (target - smoothPos) * 0.35
            if abs(target - smoothPos) < 0.01 { smoothPos = target }
        }
        let want = max(0, Int(floor(smoothPos + 0.0001)))
        if want != sentOffset {
            sentOffset = want
            Core.shared.send(object: ["Scroll": ["pane": pane.number, "scroll": ["Offset": want]]], host: pane.host)
        }
        let settled = smoothPos == smoothPos.rounded() && Int(info.display_offset) == sentOffset
        // Something else moved the scrollback (typing, new output while scrolled back).
        let overridden = idle > 0.3 && Int(info.display_offset) != sentOffset
        if idle > 0.08 && (settled || overridden) {
            smoothActive = false
        }
    }

    /// Pixels the grid is drawn below its rows while smooth scrolling.
    private var smoothScrollY: Float {
        guard smoothActive, let s = shaper else { return 0 }
        let info = renderer.snapshot.info
        var frac = smoothPos - CGFloat(info.display_offset)
        if !info.has_peek { frac = min(frac, 0) }
        frac = min(max(frac, 0), 0.999)
        return Float((frac * CGFloat(s.cellHeight)).rounded())
    }

    private var lastRenderTime: CFTimeInterval = 0
    private var renderScheduled = false

    /// A new frame arrived from the daemon. Draw it now if the last draw was at least one
    /// refresh interval ago, else at the end of that interval (Alacritty's `FrameTimer`), so
    /// output shows up without waiting for the next display-link tick.
    func frameArrived() {
        guard !Perf.tickRendering else {
            wakeLink()
            return
        }
        guard window != nil, subscribed, !isHiddenOrHasHiddenAncestor, !renderScheduled else { return }
        let fps = Double(window?.screen?.maximumFramesPerSecond ?? 60)
        let wait = lastRenderTime + 1 / max(30, fps) - CACurrentMediaTime()
        if wait <= 0.0005 {
            if Core.shared.takeDirty(pane) { renderNow() }
            return
        }
        renderScheduled = true
        DispatchQueue.main.asyncAfter(deadline: .now() + wait) { [weak self] in
            guard let self else { return }
            self.renderScheduled = false
            if Core.shared.takeDirty(self.pane) { self.renderNow() }
        }
    }

    private func renderNow() {
        guard !holdGridSize else { return }
        lastRenderTime = CACurrentMediaTime()
        let perfStart = Perf.enabled ? CACurrentMediaTime() : 0
        defer { if Perf.enabled { Perf.shared.rendered(pane: pane, start: perfStart, generation: renderer.snapshot.info.generation) } }
        needsRender = false
        guard let layer = metalLayer else { return }
        // A blinking cursor or an animation may have started with this frame.
        defer { updateLinkState() }
        if shaper == nil { updateScale() }
        guard let s = shaper else { return }
        let oldCursor = (renderer.snapshot.info.cursor_col, renderer.snapshot.info.cursor_row)
        let perfGrid = Perf.enabled ? CACurrentMediaTime() : 0
        _ = renderer.snapshot.update(pane: pane)
        // A previous resize can still be in flight when another layout change occurs.
        let sized = renderer.snapshot.cols == cols && renderer.snapshot.rows == rows
        if sized { resizeTimedOut = false }
        guard sized || resizeTimedOut else {
            needsRender = true
            return
        }
        if waitingForResizeFrame {
            waitingForResizeFrame = false
            updateDrawableSize()
        }
        let perfDraw = Perf.enabled ? CACurrentMediaTime() : 0

        var flash: Float = 0
        if flashStart > 0 {
            let t = Float((CACurrentMediaTime() - flashStart) / 0.25)
            if t >= 1 {
                flashStart = 0
            } else {
                flash = 1 - t
            }
        }
        var dim: Float = 0
        if let c = container, c.hasMultipleVisiblePanes, c.focusedPane != pane {
            dim = Float(config.unfocusedSplitDim)
        }
        let opacity = Float((window?.windowController as? TerminalWindowController)?.opacity ?? config.opacity)
        if layer.isOpaque != (opacity >= 1) { layer.isOpaque = opacity >= 1 }
        let params = RenderParams(pane: pane,
                                  shaper: s,
                                  padX: padXPixels,
                                  padY: padYPixels,
                                  viewportWidth: Int(layer.drawableSize.width),
                                  viewportHeight: Int(layer.drawableSize.height),
                                  focused: isKeyFocused,
                                  cursorOn: lastBlinkOn,
                                  cursorThickness: max(1, Int((config.cursorThickness * backingScale).rounded())),
                                  dim: dim,
                                  flash: flash,
                                  opacity: opacity,
                                  themeBackground: config.theme.background,
                                  themeForeground: config.theme.foreground,
                                  hoverLink: hoverLink,
                                  hoverSpan: hoverSpan,
                                  scrollY: smoothScrollY)
        renderer.draw(layer: layer, params: params)
        if Perf.enabled {
            Perf.shared.split(pane: pane, update: perfDraw - perfGrid, draw: CACurrentMediaTime() - perfDraw)
        }

        let newCursor = (renderer.snapshot.info.cursor_col, renderer.snapshot.info.cursor_row)
        if hasMarkedText() && (oldCursor.0 != newCursor.0 || oldCursor.1 != newCursor.1) {
            updateMarkedTextOverlay()
        }
    }

    /// Visual bell.
    func flash() {
        flashStart = CACurrentMediaTime()
        wakeLink()
    }

    // MARK: Overlays

    private func layoutOverlays() {
        progressBar?.frame = NSRect(x: 0, y: 0, width: bounds.width, height: ProgressBarView.height)
        if let badge = lockBadge {
            badge.frame = NSRect(x: bounds.width - 26, y: 6, width: 18, height: 18)
        }
        offlineOverlay?.frame = bounds
        if let toast = toastView {
            let size = toast.frame.size
            toast.setFrameOrigin(NSPoint(x: (bounds.width - size.width) / 2, y: bounds.height - size.height - 12))
        }
    }

    func setLockBadgeVisible(_ visible: Bool) {
        if visible && lockBadge == nil {
            let badge = NSImageView()
            badge.image = NSImage(systemSymbolName: "lock.fill", accessibilityDescription: "Secure Keyboard Entry")
            badge.symbolConfiguration = NSImage.SymbolConfiguration(pointSize: 13, weight: .semibold)
            badge.contentTintColor = NSColor.systemOrange
            badge.toolTip = "Secure Keyboard Entry is on"
            addSubview(badge)
            lockBadge = badge
        }
        lockBadge?.isHidden = !visible
        layoutOverlays()
    }

    /// Progress bar along the top edge (OSC 9;4), like Ghostty's.
    func setProgress(_ report: ProgressReport?) {
        guard let report else {
            progressBar?.removeFromSuperview()
            progressBar = nil
            return
        }
        if progressBar == nil {
            let bar = ProgressBarView()
            addSubview(bar)
            progressBar = bar
            layoutOverlays()
        }
        progressBar?.report = report
    }

    /// The pane's host is not connected: the last frame stays, under "Disconnected —
    /// reconnecting…" (with the retry countdown), and input is blocked. nil: connected.
    func setOffline(_ message: String?) {
        guard let message else {
            offlineOverlay?.removeFromSuperview()
            offlineOverlay = nil
            return
        }
        let overlay = offlineOverlay ?? OfflineOverlay()
        overlay.message = message
        if overlay.superview !== self {
            addSubview(overlay, positioned: .above, relativeTo: nil)
            offlineOverlay = overlay
        }
        layoutOverlays()
    }

    /// Input goes nowhere while the pane's host is disconnected.
    var isOffline: Bool { offlineOverlay != nil }

    /// Small transient message at the bottom of the pane.
    func showToast(_ text: String, duration: TimeInterval = 2.5) {
        toastView?.removeFromSuperview()
        let label = NSTextField(labelWithString: text)
        label.font = NSFont.systemFont(ofSize: 11, weight: .medium)
        label.textColor = NSColor.white
        label.sizeToFit()
        let box = NSView(frame: NSRect(x: 0, y: 0, width: label.frame.width + 20, height: label.frame.height + 8))
        box.wantsLayer = true
        box.layer?.backgroundColor = NSColor(white: 0.1, alpha: 0.75).cgColor
        box.layer?.cornerRadius = 7
        label.frame.origin = NSPoint(x: 10, y: 4)
        box.addSubview(label)
        addSubview(box)
        toastView = box
        layoutOverlays()
        NSObject.cancelPreviousPerformRequests(withTarget: self, selector: #selector(fadeToast), object: nil)
        perform(#selector(fadeToast), with: nil, afterDelay: duration)
    }

    @objc private func fadeToast() {
        toastView?.animator().alphaValue = 0
        perform(#selector(removeToast), with: nil, afterDelay: 0.35)
    }

    @objc private func removeToast() {
        toastView?.removeFromSuperview()
        toastView = nil
    }

    /// "Restored" toast for panes that survived a daemon restart / reboot.
    func maybeShowRestoredToast() {
        guard !restoredToastShown, subscribed, let info = SessionManager.shared.panes[pane], info.restored else { return }
        restoredToastShown = true
        showToast("Session restored")
    }

    // MARK: Tab completion

    private lazy var completion: CompletionPopup = {
        let p = CompletionPopup()
        p.onAccept = { [weak self] item in self?.acceptCompletion(item) }
        return p
    }()
    private var completionRefresh: DispatchWorkItem?
    /// Numbers completion refreshes, so only the answer to the latest one is shown.
    private var completionQuery = 0

    /// Tab and the menu's navigation keys. Returns true when the key was consumed.
    private func handleCompletionKey(_ event: NSEvent) -> Bool {
        let mods = event.modifierFlags.intersection([.command, .control, .option, .shift])
        if completion.isVisible {
            switch event.keyCode {
            case 48: // Tab / Shift-Tab
                completion.move(mods.contains(.shift) ? -1 : 1)
                return true
            case 125: completion.move(1); return true   // down
            case 126: completion.move(-1); return true  // up
            case 36, 76, 124: // return, enter, right: accept
                if let item = completion.selected { acceptCompletion(item) }
                return true
            case 53: // escape
                completion.close()
                return true
            default:
                // Typing goes to the shell; the menu follows what is typed.
                scheduleCompletionRefresh()
                return false
            }
        }
        guard event.keyCode == 48, mods.isEmpty, !hasMarkedText() else { return false }
        return startCompletion()
    }

    /// Asks the daemon for candidates. False (Tab goes to the shell) when there are none, when
    /// not at a prompt, or in full-screen apps.
    private func startCompletion() -> Bool {
        guard config.tabCompletion, (snapshotModes & TermMode.altScreen) == 0,
              SessionManager.shared.panes[pane]?.atPrompt == true,
              let (word, items) = fetchCompletions(), !items.isEmpty
        else { return false }
        if items.count == 1 {
            insertCompletion(items[0].text, replacing: word, final: !items[0].isDirectory)
            return true
        }
        // Complete the common prefix, then offer the rest.
        let common = items.map(\.text).reduce(items[0].text) { $0.commonPrefix(with: $1) }
        var typed = word
        if common.count > word.count {
            insertCompletion(common, replacing: word, final: false)
            typed = common
        }
        showCompletions(items, word: typed)
        return true
    }

    /// Tab has to know right away whether it completes or goes to the shell: a short timeout,
    /// and nothing while the host is offline.
    private func fetchCompletions() -> (String, [CompletionCandidate])? {
        guard !isOffline,
              let resp = Core.shared.request(object: ["Complete": ["pane": pane.number]], host: pane.host,
                                             timeout: 1)
        else { return nil }
        return parseCompletions(resp)
    }

    private func parseCompletions(_ resp: Any?) -> (String, [CompletionCandidate])? {
        guard let v = JSON.variant(resp), v.name == "Completions", let d = v.payload as? [String: Any]
        else { return nil }
        let word = jsonString(d["word"]) ?? ""
        let items = (d["items"] as? [[String: Any]] ?? []).compactMap { i -> CompletionCandidate? in
            guard let text = jsonString(i["text"]) else { return nil }
            return CompletionCandidate(text: text, kind: jsonString(i["kind"]) ?? "", description: jsonString(i["description"]))
        }
        return (word, items)
    }

    private func showCompletions(_ items: [CompletionCandidate], word: String) {
        guard let window = window else { return }
        let rect = window.convertToScreen(convert(cursorRect(), to: nil))
        completion.show(items, word: word, anchor: rect, parent: window)
    }

    /// After a typed key reaches the shell (and it echoed), re-query; close when nothing is left.
    private func scheduleCompletionRefresh() {
        completionRefresh?.cancel()
        // Every key makes answers to earlier queries stale, also the one still on its way.
        completionQuery += 1
        let query = completionQuery
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.completion.isVisible, !self.isOffline else { return }
            Core.shared.requestAsync(object: ["Complete": ["pane": self.pane.number]], host: self.pane.host,
                                     timeout: 2) { [weak self] resp in
                guard let self, query == self.completionQuery, self.completion.isVisible else { return }
                guard let (word, items) = self.parseCompletions(resp), !items.isEmpty,
                      SessionManager.shared.panes[self.pane]?.atPrompt == true
                else {
                    self.completion.close()
                    return
                }
                self.showCompletions(items, word: word)
            }
        }
        completionRefresh = work
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(60), execute: work)
    }

    private func acceptCompletion(_ item: CompletionCandidate) {
        let word = completion.word
        completion.close()
        insertCompletion(item.text, replacing: word, final: !item.isDirectory)
    }

    /// Types the completion into the shell: only the missing part when `text` extends `word`,
    /// else backspaces over the word first. Special characters are escaped.
    private func insertCompletion(_ text: String, replacing word: String, final: Bool) {
        let escaped = shellEscape(text)
        let typed = shellEscape(word)
        var out = ""
        if escaped.hasPrefix(typed) {
            out = String(escaped.dropFirst(typed.count))
        } else {
            // Erase what is on the line: the word as typed, escapes included.
            out = String(repeating: "\u{7f}", count: typed.count) + escaped
        }
        if final { out += " " }
        Core.shared.input(pane, text: out)
    }

    private func shellEscape(_ s: String) -> String {
        var out = ""
        for c in s {
            if " \t'\"\\$`!&;|()<>*?[]{}#~".contains(c) && !(c == "~" && out.isEmpty) { out.append("\\") }
            out.append(c)
        }
        return out
    }

    /// Cursor cell rectangle in view coordinates.
    private func cursorRect() -> NSRect {
        let info = renderer.snapshot.info
        let cell = cellSizePoints
        let scale = backingScale
        let x = CGFloat(padXPixels) / scale + CGFloat(info.cursor_col) * cell.width
        let y = CGFloat(padYPixels) / scale + CGFloat(info.cursor_row) * cell.height
        return NSRect(x: x, y: y, width: cell.width, height: cell.height)
    }

    private func updateMarkedTextOverlay() {
        guard markedText.length > 0 else {
            markedLabel?.removeFromSuperview()
            markedLabel = nil
            return
        }
        let label: NSTextField
        if let existing = markedLabel {
            label = existing
        } else {
            label = NSTextField(labelWithString: "")
            label.drawsBackground = true
            label.isBordered = false
            addSubview(label)
            markedLabel = label
        }
        let fontSize = SessionManager.shared.effectiveFontSize
        let font = NSFont(name: config.fontFamily, size: fontSize)
            ?? NSFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        let attrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: colorFromRGB(config.theme.foreground),
            .underlineStyle: NSUnderlineStyle.single.rawValue,
        ]
        label.attributedStringValue = NSAttributedString(string: markedText.string, attributes: attrs)
        label.backgroundColor = colorFromRGB(config.theme.background)
        label.sizeToFit()
        let rect = cursorRect()
        label.frame = NSRect(x: rect.minX, y: rect.minY, width: max(label.frame.width, rect.width),
                             height: max(label.frame.height, rect.height))
    }

    // MARK: Keyboard

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        guard event.type == .keyDown, window?.firstResponder === self else { return false }
        let flags = event.modifierFlags
        // Command shortcuts belong to the menu.
        if flags.contains(.command) { return false }
        // Control combinations (Ctrl+Tab, Ctrl+Return, ...) would otherwise be eaten by AppKit.
        if flags.contains(.control) {
            keyDown(with: event)
            return true
        }
        return false
    }

    override func keyDown(with event: NSEvent) {
        if isOffline { return }
        if Perf.enabled { Perf.shared.key(pane: pane) }
        NSCursor.setHiddenUntilMouseMoves(true)
        smoothActive = false
        resetBlink()
        let flags = event.modifierFlags
        if handleCompletionKey(event) { return }
        let action = event.isARepeat ? KeyActionCode.repeating : KeyActionCode.press

        if flags.contains(.command) {
            // Not a menu shortcut: a few fallbacks, otherwise forward with the Super modifier.
            if handleCommandFallback(event) { return }
            sendKey(event, action: action, text: nil, optionIsAlt: false)
            return
        }

        let optionIsAlt = KeyMapping.optionActsAsAlt(event, setting: config.optionAsAlt)
        let hadMarked = hasMarkedText()

        if optionIsAlt && !hadMarked {
            // Option is Alt: send the unmodified character with the Alt modifier and skip
            // macOS composition (so Option+e does not start a dead key).
            var text: String? = nil
            if !flags.contains(.control),
               let t = event.characters(byApplyingModifiers: flags.intersection([.shift, .capsLock])),
               KeyMapping.isPrintable(t) {
                text = t
            }
            sendKey(event, action: action, text: text, optionIsAlt: true)
            return
        }

        keyTextAccumulator = []
        keyCommandIssued = false
        interpretKeyEvents([event])
        let committed = keyTextAccumulator ?? []
        keyTextAccumulator = nil

        if !committed.isEmpty {
            let text = committed.joined()
            if hadMarked {
                // IME / dead key commit: plain text, not a key press.
                Core.shared.input(pane, text: text)
                // Some input methods (Korean) commit and pass the key on too: Return, arrows,
                // Backspace still do what they do.
                if keyCommandIssued {
                    sendKey(event, action: action, text: nil, optionIsAlt: false)
                }
            } else {
                sendKey(event, action: action, text: KeyMapping.isPrintable(text) ? text : nil, optionIsAlt: false)
            }
            return
        }
        if hadMarked || hasMarkedText() {
            // Composition ended and the input method passed the key on.
            if keyCommandIssued && !hasMarkedText() {
                sendKey(event, action: action, text: nil, optionIsAlt: false)
            }
            // Otherwise the input method consumed the key (preedit update / cancel).
            return
        }
        sendKey(event, action: action, text: nil, optionIsAlt: false)
    }

    override func keyUp(with event: NSEvent) {
        if isOffline { return }
        if event.modifierFlags.contains(.command) || hasMarkedText() { return }
        let optionIsAlt = KeyMapping.optionActsAsAlt(event, setting: config.optionAsAlt)
        sendKey(event, action: KeyActionCode.release, text: nil, optionIsAlt: optionIsAlt)
    }

    override func flagsChanged(with event: NSEvent) {
        updateHover(modifiers: event.modifierFlags)
        if isOffline { return }
        let flags = event.modifierFlags
        let optionIsAlt = config.optionAsAlt != .none
        let mods = KeyMapping.mods(flags, optionIsAlt: optionIsAlt)
        if event.keyCode == 0x39 {
            let down = flags.contains(.capsLock)
            Core.shared.key(pane, kind: KeyKind.named, code: KeyMapping.capsLock, mods: mods,
                            action: down ? KeyActionCode.press : KeyActionCode.release,
                            text: nil, shifted: 0, baseLayout: 0)
            return
        }
        guard let m = KeyMapping.modifierKeys[event.keyCode] else { return }
        let down = (flags.rawValue & m.mask) != 0
        Core.shared.key(pane, kind: KeyKind.named, code: m.key, mods: mods,
                        action: down ? KeyActionCode.press : KeyActionCode.release,
                        text: nil, shifted: 0, baseLayout: 0)
    }

    /// Converts an NSEvent into a `thurm_key_event`.
    private func sendKey(_ event: NSEvent, action: UInt8, text: String?, optionIsAlt: Bool) {
        let keyCode = event.keyCode
        let named = KeyMapping.named[keyCode]
        // Option never composes text on named keys (arrows, F-keys...), so it is always Alt there.
        let mods = KeyMapping.mods(event.modifierFlags, optionIsAlt: optionIsAlt || named != nil)
        if let named = named {
            // Keypad keys keep their text (digits, operators) for the legacy encoding.
            let keyText = KeyMapping.isKeypad(keyCode) ? text : nil
            Core.shared.key(pane, kind: KeyKind.named, code: named, mods: mods, action: action,
                            text: keyText, shifted: 0, baseLayout: 0)
            return
        }
        let base = KeyMapping.baseLayoutScalar(keyCode)
        let code = KeyMapping.lowercasedScalar(event.characters(byApplyingModifiers: []))
            ?? KeyMapping.lowercasedScalar(event.charactersIgnoringModifiers)
            ?? base
        guard code != 0 else {
            if let t = text, action != KeyActionCode.release { Core.shared.input(pane, text: t) }
            return
        }
        var shifted: UInt32 = 0
        if let s = event.characters(byApplyingModifiers: .shift), let first = s.unicodeScalars.first {
            shifted = first.value
        }
        Core.shared.key(pane, kind: KeyKind.text, code: code, mods: mods, action: action,
                        text: action == KeyActionCode.release ? nil : text,
                        shifted: shifted, baseLayout: base == code ? 0 : base)
    }

    /// Command shortcuts that the menu may not match on every layout.
    private func handleCommandFallback(_ event: NSEvent) -> Bool {
        let flags = event.modifierFlags
        guard flags.contains(.shift) else { return false }
        switch event.keyCode {
        case 0x21: // [
            window?.selectPreviousTab(nil)
            return true
        case 0x1E: // ]
            window?.selectNextTab(nil)
            return true
        case 0x2B: // ,
            SessionManager.shared.reloadConfig(notifyDaemon: true)
            return true
        default:
            return false
        }
    }

    // MARK: NSTextInputClient

    func insertText(_ string: Any, replacementRange: NSRange) {
        let text: String
        if let s = string as? NSAttributedString {
            text = s.string
        } else if let s = string as? String {
            text = s
        } else {
            return
        }
        markedText = NSMutableAttributedString()
        updateMarkedTextOverlay()
        if keyTextAccumulator != nil {
            keyTextAccumulator?.append(text)
            return
        }
        // Outside keyDown (character palette, dictation, ...): plain text.
        if isOffline { return }
        Core.shared.input(pane, text: text)
    }

    override func doCommand(by selector: Selector) {
        // Keys like Return, arrows and Ctrl combinations are sent as key events after
        // interpretKeyEvents returns (never beep); note that the input method passed it on.
        if keyTextAccumulator != nil { keyCommandIssued = true }
    }

    func setMarkedText(_ string: Any, selectedRange: NSRange, replacementRange: NSRange) {
        if let s = string as? NSAttributedString {
            markedText = NSMutableAttributedString(attributedString: s)
        } else if let s = string as? String {
            markedText = NSMutableAttributedString(string: s)
        }
        updateMarkedTextOverlay()
    }

    func unmarkText() {
        markedText = NSMutableAttributedString()
        updateMarkedTextOverlay()
    }

    func selectedRange() -> NSRange {
        NSRange(location: NSNotFound, length: 0)
    }

    func markedRange() -> NSRange {
        markedText.length > 0 ? NSRange(location: 0, length: markedText.length) : NSRange(location: NSNotFound, length: 0)
    }

    func hasMarkedText() -> Bool {
        markedText.length > 0
    }

    func attributedSubstring(forProposedRange range: NSRange, actualRange: NSRangePointer?) -> NSAttributedString? {
        nil
    }

    func validAttributesForMarkedText() -> [NSAttributedString.Key] {
        [.underlineStyle, .backgroundColor, .foregroundColor]
    }

    func firstRect(forCharacterRange range: NSRange, actualRange: NSRangePointer?) -> NSRect {
        guard let window = window else { return .zero }
        let inWindow = convert(cursorRect(), to: nil)
        return window.convertToScreen(inWindow)
    }

    func characterIndex(for point: NSPoint) -> Int {
        NSNotFound
    }

    // MARK: Edit actions

    @objc func copy(_ sender: Any?) {
        // Local panes answer from the app's own copy; a remote host gets a moment, not forever.
        guard let resp = Core.shared.request(object: ["CopySelection": ["pane": pane.number]], host: pane.host,
                                             timeout: 2),
              let v = JSON.variant(resp), v.name == "Text",
              let text = v.payload as? String, !text.isEmpty
        else { return }
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(text, forType: .string)
    }

    @objc func paste(_ sender: Any?) {
        if isOffline { return }
        let pb = NSPasteboard.general
        if pb.string(forType: .string) == nil, pb.availableType(from: [.png, .tiff]) != nil {
            // An image: its file's path (on the pane's host).
            if let path = imagePath(pb) { Core.shared.paste(pane, text: shellEscape(path) + " ") }
            return
        }
        guard let text = pb.string(forType: .string), !text.isEmpty, pasteConfirmed(text) else { return }
        Core.shared.paste(pane, text: text)
    }

    /// `confirm_multiline_paste`: several lines into a program without bracketed paste would
    /// run one by one as they arrive; ask first. For pastes and drops alike.
    private func pasteConfirmed(_ text: String) -> Bool {
        let bracketed = (snapshotModes & TermMode.bracketedPaste) != 0
        let multiline = text.unicodeScalars.contains { $0 == "\n" || $0 == "\r" }
        guard config.confirmMultilinePaste && multiline && !bracketed else { return true }
        let lines = text.components(separatedBy: .newlines).filter { !$0.isEmpty }.count
        let alert = NSAlert()
        alert.messageText = "Paste \(lines) lines?"
        alert.informativeText = "The program in this pane did not enable bracketed paste, so every line "
            + "may run as a separate command as soon as it is pasted."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Paste")
        alert.addButton(withTitle: "Cancel")
        return alert.runModal() == .alertFirstButtonReturn
    }

    // MARK: Drag and drop

    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation {
        dropText(sender.draggingPasteboard) == nil ? [] : .copy
    }

    override func draggingUpdated(_ sender: NSDraggingInfo) -> NSDragOperation {
        draggingEntered(sender)
    }

    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        if isOffline { return false }
        guard let text = dropText(sender.draggingPasteboard, materialize: true) else { return false }
        window?.makeKeyAndOrderFront(nil)
        window?.makeFirstResponder(self)
        guard pasteConfirmed(text) else { return false }
        Core.shared.paste(pane, text: text)
        return true
    }

    /// What a drop inserts: shell-escaped paths for files, the URL for links, image data saved to
    /// a temporary PNG (e.g. an image dragged out of a browser), or plain text. `materialize`
    /// writes that PNG; without it only the kind of content is checked.
    private func dropText(_ pb: NSPasteboard, materialize: Bool = false) -> String? {
        let fileOpts: [NSPasteboard.ReadingOptionKey: Any] = [.urlReadingFileURLsOnly: true]
        // Mac paths mean nothing on a remote host: files dropped on a remote pane go by name only
        // when they are images (copied over), else as their paths as text.
        if let urls = pb.readObjects(forClasses: [NSURL.self], options: fileOpts) as? [URL], !urls.isEmpty,
           !pane.isRemote || pb.availableType(from: [.png, .tiff]) == nil {
            return urls.map { shellEscape($0.path) }.joined(separator: " ") + " "
        }
        if pb.availableType(from: [.png, .tiff]) != nil {
            guard materialize else { return "" }
            if let path = imagePath(pb) { return shellEscape(path) + " " }
        }
        if let url = pb.readObjects(forClasses: [NSURL.self], options: nil)?.first as? URL {
            return url.absoluteString
        }
        if let text = pb.string(forType: .string), !text.isEmpty { return text }
        return nil
    }

    /// The pasteboard's image as PNG.
    private static func pngData(_ pb: NSPasteboard) -> Data? {
        var data = pb.data(forType: .png)
        if data == nil, let tiff = pb.data(forType: .tiff), let rep = NSBitmapImageRep(data: tiff) {
            data = rep.representation(using: .png, properties: [:])
        }
        return data
    }

    /// Where the pasted or dropped image is now: a temporary PNG here, or, for a remote pane,
    /// a private file on its host written through its daemon (only what the user pastes goes
    /// over).
    private func imagePath(_ pb: NSPasteboard) -> String? {
        guard pane.isRemote else { return Self.saveDroppedImage(pb) }
        guard let png = Self.pngData(pb) else { return nil }
        let written = Core.shared.writeTempFile(host: pane.host, name: "paste.png", data: png)
        if let e = written.error {
            showToast("Could not copy the image to \(pane.host): \(e)", duration: 5)
        }
        return written.path
    }

    private static func saveDroppedImage(_ pb: NSPasteboard) -> String? {
        guard let png = pngData(pb) else { return nil }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("thurm-drops", isDirectory: true)
        let file = dir.appendingPathComponent("image-\(UUID().uuidString.prefix(8)).png")
        do {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            pruneOldFiles(in: dir)
            try png.write(to: file)
        } catch {
            return nil
        }
        return file.path
    }

    /// Dropped images are used by path for a moment, then never again: forget old ones.
    private static func pruneOldFiles(in dir: URL, olderThan age: TimeInterval = 24 * 60 * 60) {
        let fm = FileManager.default
        guard let files = try? fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: [.contentModificationDateKey])
        else { return }
        for file in files {
            if let date = try? file.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate,
               Date().timeIntervalSince(date) > age {
                try? fm.removeItem(at: file)
            }
        }
    }

    @objc override func selectAll(_ sender: Any?) {
        Core.shared.send(object: ["Selection": ["pane": pane.number, "op": "SelectAll"]], host: pane.host)
    }

    /// Cmd+K: clear the screen and scrollback, keeping the prompt at the top.
    @objc func clearScreen(_ sender: Any?) {
        Core.shared.send(object: ["ClearScreen": ["pane": pane.number]], host: pane.host)
        needsRender = true
    }

    @objc func clearScrollback(_ sender: Any?) {
        Core.shared.send(object: ["ClearScrollback": ["pane": pane.number]], host: pane.host)
        needsRender = true
    }

    /// Asks the on-device model (`ai.explain`) what happened in the last command, and shows it.
    @objc func explainLastCommand(_ sender: Any?) {
        // Asynchronous, and the connection is resolved here on the main thread (the library
        // keeps it alive for the request even if the host disconnects meanwhile).
        Core.shared.requestAsync(object: ["Explain": ["pane": pane.number]], host: pane.host,
                                 timeout: 120) { [weak self] response in
            guard let self, let window = self.window else { return }
            let resp = response as? [String: Any]
            let alert = NSAlert()
            if let text = jsonString(resp?["Text"]) {
                alert.messageText = "Last Command"
                alert.informativeText = text
            } else {
                alert.alertStyle = .warning
                alert.messageText = "Could Not Explain the Last Command"
                alert.informativeText = jsonString(resp?["error"]) ?? "The session daemon is not connected."
            }
            alert.beginSheetModal(for: window)
        }
    }

    // MARK: Mouse

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let t = trackingArea { removeTrackingArea(t) }
        let t = NSTrackingArea(rect: bounds,
                               options: [.mouseMoved, .mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                               owner: self, userInfo: nil)
        addTrackingArea(t)
        trackingArea = t
    }

    override func resetCursorRects() {
        // Inset so the dividers' resize cursors (which extend into the panes) win.
        addCursorRect(bounds.insetBy(dx: 4, dy: 4), cursor: .iBeam)
    }

    private func cellPosition(_ point: NSPoint) -> CellPosition {
        guard let s = shaper else { return CellPosition(col: 0, row: 0, rightHalf: false, x: 0, y: 0) }
        let scale = backingScale
        let x = point.x * scale - CGFloat(padXPixels)
        let y = point.y * scale - CGFloat(padYPixels) - CGFloat(smoothScrollY)
        let fcol = x / CGFloat(s.cellWidth)
        let frow = y / CGFloat(s.cellHeight)
        let maxCol = max(0, (renderer.snapshot.valid ? renderer.snapshot.cols : cols) - 1)
        let maxRow = max(0, (renderer.snapshot.valid ? renderer.snapshot.rows : rows) - 1)
        let col = min(maxCol, max(0, Int(floor(fcol))))
        let row = min(maxRow, max(0, Int(floor(frow))))
        let rightHalf = fcol - floor(fcol) >= 0.5
        return CellPosition(col: col, row: row, rightHalf: rightHalf, x: max(0, Int(x)), y: max(0, Int(y)))
    }

    private func mouseMods(_ flags: NSEvent.ModifierFlags) -> UInt8 {
        var m: UInt8 = 0
        if flags.contains(.shift) { m |= KeyMods.shift }
        if flags.contains(.control) { m |= KeyMods.ctrl }
        if flags.contains(.option) { m |= KeyMods.alt }
        if flags.contains(.command) { m |= KeyMods.superKey }
        return m
    }

    private func sendMouse(_ event: NSEvent, kind: UInt8, button: UInt8) {
        if isOffline { return }
        let p = cellPosition(convert(event.locationInWindow, from: nil))
        let clicks = (event.type == .mouseMoved) ? 1 : max(1, event.clickCount)
        Core.shared.mouse(pane, kind: kind, button: button, mods: mouseMods(event.modifierFlags), clicks: clicks,
                          col: p.col, row: p.row, rightHalf: p.rightHalf, x: p.x, y: p.y)
    }

    override func mouseDown(with event: NSEvent) {
        if window?.firstResponder !== self {
            window?.makeFirstResponder(self)
        }
        let point = convert(event.locationInWindow, from: nil)
        if event.modifierFlags.contains(.command) {
            let p = cellPosition(point)
            if let url = linkURL(col: p.col, row: p.row) {
                SessionManager.shared.openLink(url, from: pane, in: window)
                return
            }
        }
        pressedButton = MouseButtonCode.left
        sendMouse(event, kind: MouseKindCode.press, button: MouseButtonCode.left)
    }

    override func mouseDragged(with event: NSEvent) {
        guard pressedButton != nil else { return }
        sendMouse(event, kind: MouseKindCode.move, button: MouseButtonCode.left)
    }

    override func mouseUp(with event: NSEvent) {
        guard pressedButton != nil else { return }
        pressedButton = nil
        sendMouse(event, kind: MouseKindCode.release, button: MouseButtonCode.left)
        if config.copyOnSelect && (snapshotModes & TermMode.mouseAny) == 0 {
            copy(nil)
        }
    }

    override func rightMouseDown(with event: NSEvent) {
        let reporting = (snapshotModes & TermMode.mouseAny) != 0
        if reporting && !event.modifierFlags.contains(.shift) {
            rightPressReported = true
            sendMouse(event, kind: MouseKindCode.press, button: MouseButtonCode.right)
            return
        }
        if window?.firstResponder !== self {
            window?.makeFirstResponder(self)
        }
        NSMenu.popUpContextMenu(contextMenu(), with: event, for: self)
    }

    override func rightMouseUp(with event: NSEvent) {
        if rightPressReported {
            rightPressReported = false
            sendMouse(event, kind: MouseKindCode.release, button: MouseButtonCode.right)
        }
    }

    override func rightMouseDragged(with event: NSEvent) {
        if rightPressReported {
            sendMouse(event, kind: MouseKindCode.move, button: MouseButtonCode.right)
        }
    }

    private func otherButton(_ event: NSEvent) -> UInt8 {
        switch event.buttonNumber {
        case 2: return MouseButtonCode.middle
        case 3: return MouseButtonCode.back
        case 4: return MouseButtonCode.forward
        default: return MouseButtonCode.middle
        }
    }

    override func otherMouseDown(with event: NSEvent) {
        sendMouse(event, kind: MouseKindCode.press, button: otherButton(event))
    }

    override func otherMouseUp(with event: NSEvent) {
        sendMouse(event, kind: MouseKindCode.release, button: otherButton(event))
    }

    override func otherMouseDragged(with event: NSEvent) {
        sendMouse(event, kind: MouseKindCode.move, button: otherButton(event))
    }

    override func mouseMoved(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        lastMouseLocation = point
        if let c = container, c.dividerAt(convert(point, to: c)) != nil {
            // The divider shows its own cursor.
        } else if !event.modifierFlags.contains(.command) {
            NSCursor.iBeam.set()
        }
        updateHover(modifiers: event.modifierFlags)
        if (snapshotModes & TermMode.mouseMotion) != 0 {
            sendMouse(event, kind: MouseKindCode.move, button: MouseButtonCode.none)
        }
    }

    override func mouseExited(with event: NSEvent) {
        lastMouseLocation = nil
        updateHover(modifiers: [])
    }

    override func scrollWheel(with event: NSEvent) {
        guard let s = shaper else { return }
        if event.phase == .began || event.phase == .mayBegin {
            scrollAccumulator = 0
        }
        var delta = event.scrollingDeltaY
        if event.hasPreciseScrollingDeltas {
            // Trackpad: points → lines.
            delta /= max(1, CGFloat(s.cellHeight) / backingScale)
        } else if delta != 0 {
            // Mouse wheel: whole notches.
            delta = delta < 0 ? min(-1, delta.rounded()) : max(1, delta.rounded())
        }
        // Like Alacritty: `scroll_multiplier` lines per line of movement, except for apps that
        // get the wheel as mouse reports.
        let raw = (snapshotModes & TermMode.mouseAny) != 0
        let altScreen = (snapshotModes & TermMode.altScreen) != 0
        if !raw && !altScreen && config.smoothScroll && event.hasPreciseScrollingDeltas && renderer.snapshot.valid {
            let info = renderer.snapshot.info
            if !smoothActive {
                smoothActive = true
                smoothPos = CGFloat(info.display_offset)
                sentOffset = Int(info.display_offset)
            }
            smoothPos = min(CGFloat(info.history_size), max(0, smoothPos + delta * config.scrollMultiplier))
            lastSmoothInput = CACurrentMediaTime()
            needsRender = true
            return
        }
        scrollAccumulator += delta * (raw ? 1 : config.scrollMultiplier)
        let lines = Int(scrollAccumulator.rounded(.towardZero))
        if Perf.enabled { Perf.shared.scrollEvent(pane: pane, lines: lines) }
        guard lines != 0 else { return }
        scrollAccumulator -= CGFloat(lines)
        let p = cellPosition(convert(event.locationInWindow, from: nil))
        let mods = mouseMods(event.modifierFlags)
        if let prev = pendingScroll, prev.mods == mods, (prev.lines > 0) == (lines > 0) {
            pendingScroll = (prev.lines + lines, p.col, p.row, mods)
        } else {
            if let prev = pendingScroll {
                Core.shared.wheel(pane, lines: prev.lines, col: prev.col, row: prev.row, mods: prev.mods)
            }
            pendingScroll = (lines, p.col, p.row, mods)
        }
        wakeLink()
    }

    // MARK: Hyperlinks

    /// Cmd-hover highlighting of OSC 8 links and plain URLs.
    private func updateHover(modifiers: NSEvent.ModifierFlags) {
        var newLink: UInt16 = 0
        var newSpan: (row: Int, start: Int, end: Int)?
        if modifiers.contains(.command), let point = lastMouseLocation, bounds.contains(point) {
            let p = cellPosition(point)
            if let cell = renderer.snapshot.cell(col: p.col, row: p.row), cell.link != 0 {
                newLink = cell.link
            } else if let found = plainURL(col: p.col, row: p.row) {
                newSpan = (row: p.row, start: found.start, end: found.end)
            }
        }
        let spanChanged: Bool
        switch (hoverSpan, newSpan) {
        case (nil, nil): spanChanged = false
        case let (a?, b?): spanChanged = a.row != b.row || a.start != b.start || a.end != b.end
        default: spanChanged = true
        }
        if newLink != hoverLink || spanChanged {
            hoverLink = newLink
            hoverSpan = newSpan
            needsRender = true
            if newLink != 0 || newSpan != nil {
                NSCursor.pointingHand.set()
            } else if lastMouseLocation != nil {
                NSCursor.iBeam.set()
            }
        }
    }

    private func linkURL(col: Int, row: Int) -> URL? {
        let snap = renderer.snapshot
        if let cell = snap.cell(col: col, row: row), cell.link != 0 {
            let index = Int(cell.link) - 1
            if index >= 0, index < snap.links.count, let url = URL(string: snap.links[index]) {
                return url
            }
        }
        return plainURL(col: col, row: row)?.url
    }

    /// A URL written in the row text under (col, row).
    private func plainURL(col: Int, row: Int) -> (url: URL, start: Int, end: Int)? {
        guard let regex = TerminalView.urlRegex else { return nil }
        let (text, map) = renderer.snapshot.rowText(row)
        guard !text.isEmpty, !map.isEmpty else { return nil }
        let ns = text as NSString
        let trailing = Set(".,;:!?)]}'\"".utf16)
        for match in regex.matches(in: text, options: [], range: NSRange(location: 0, length: ns.length)) {
            var range = match.range
            while range.length > 0 && trailing.contains(ns.character(at: range.location + range.length - 1)) {
                range.length -= 1
            }
            guard range.length > 0, range.location + range.length <= map.count else { continue }
            let startCol = map[range.location]
            let endCol = map[range.location + range.length - 1]
            guard col >= startCol && col <= endCol else { continue }
            if let url = URL(string: ns.substring(with: range)) {
                return (url, startCol, endCol)
            }
        }
        return nil
    }

    // MARK: Context menu

    private func contextMenu() -> NSMenu {
        let menu = NSMenu(title: "Terminal")
        let copyItem = menu.addItem(withTitle: "Copy", action: #selector(copy(_:)), keyEquivalent: "")
        copyItem.target = self
        let pasteItem = menu.addItem(withTitle: "Paste", action: #selector(paste(_:)), keyEquivalent: "")
        pasteItem.target = self
        menu.addItem(NSMenuItem.separator())
        menu.addItem(withTitle: "Split Right", action: #selector(TerminalWindowController.splitRight(_:)),
                     keyEquivalent: "")
        menu.addItem(withTitle: "Split Down", action: #selector(TerminalWindowController.splitDown(_:)),
                     keyEquivalent: "")
        menu.addItem(NSMenuItem.separator())
        let clearItem = menu.addItem(withTitle: "Clear", action: #selector(clearScreen(_:)), keyEquivalent: "")
        clearItem.target = self
        return menu
    }
}

/// "Disconnected — reconnecting…" over a remote pane whose host is not connected; the pane's
/// last frame shows through.
private final class OfflineOverlay: NSView {
    private let label = NSTextField(wrappingLabelWithString: "")
    private let box = NSView()

    var message: String = "" {
        didSet {
            label.stringValue = message
            needsLayout = true
        }
    }

    init() {
        super.init(frame: .zero)
        wantsLayer = true
        layer?.backgroundColor = NSColor(white: 0, alpha: 0.35).cgColor
        box.wantsLayer = true
        box.layer?.backgroundColor = NSColor(white: 0.1, alpha: 0.85).cgColor
        box.layer?.cornerRadius = 9
        label.font = .systemFont(ofSize: 12, weight: .medium)
        label.textColor = .white
        label.alignment = .center
        label.maximumNumberOfLines = 4
        box.addSubview(label)
        addSubview(box)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    override func layout() {
        super.layout()
        let maxWidth = min(420, bounds.width - 32)
        label.preferredMaxLayoutWidth = maxWidth - 24
        let size = label.fittingSize
        let w = min(maxWidth, size.width + 24)
        let h = size.height + 16
        box.frame = NSRect(x: (bounds.width - w) / 2, y: (bounds.height - h) / 2, width: w, height: h)
        label.frame = NSRect(x: 12, y: 8, width: w - 24, height: size.height)
    }

    // Swallows clicks (input is blocked), lets the view below keep the cursor.
    override func mouseDown(with event: NSEvent) {}
    override func rightMouseDown(with event: NSEvent) {}
    override func scrollWheel(with event: NSEvent) {}
}

/// Thin bar at the top of a pane: filled to the reported percentage, or a segment sliding
/// back and forth while the program is busy without one.
private final class ProgressBarView: NSView {
    static let height: CGFloat = 2

    private let fill = CALayer()

    var report: ProgressReport? {
        didSet { if report != oldValue { update() } }
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.masksToBounds = true
        fill.anchorPoint = .zero
        layer?.addSublayer(fill)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    // Never takes clicks from the terminal underneath.
    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    override func layout() {
        super.layout()
        update()
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        update()
    }

    private func update() {
        guard let report else { return }
        let color: NSColor
        switch report.state {
        case .normal, .indeterminate: color = .controlAccentColor
        case .error: color = .systemRed
        case .paused: color = .systemYellow
        }
        let width = bounds.width
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        effectiveAppearance.performAsCurrentDrawingAppearance {
            fill.backgroundColor = color.cgColor
        }
        fill.removeAnimation(forKey: "slide")
        if report.state == .indeterminate {
            let segment = max(width * 0.25, 40)
            fill.frame = CGRect(x: 0, y: 0, width: segment, height: bounds.height)
            let slide = CABasicAnimation(keyPath: "position.x")
            slide.fromValue = -segment
            slide.toValue = width
            slide.duration = 1.2
            slide.autoreverses = true
            slide.repeatCount = .infinity
            slide.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
            fill.add(slide, forKey: "slide")
        } else {
            // Error / paused without a percentage show a full bar, like Ghostty.
            let fraction = CGFloat(report.percent ?? 100) / 100
            fill.frame = CGRect(x: 0, y: 0, width: width * fraction, height: bounds.height)
        }
        CATransaction.commit()
    }
}

extension TerminalView: NSMenuItemValidation {
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if menuItem.action == #selector(explainLastCommand(_:)) {
            return SessionManager.shared.config.aiExplain
        }
        return true
    }
}
