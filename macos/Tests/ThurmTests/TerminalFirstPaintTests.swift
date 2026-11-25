import AppKit
import Metal
import QuartzCore
import XCTest
@testable import Thurm

/// Offscreen Metal drawable for exercising `TerminalView.renderNow()` in unit tests without
/// requiring a visible screen or window server presentation.
private final class FirstPaintDrawable: NSObject, CAMetalDrawable {
    let texture: MTLTexture
    unowned let layer: CAMetalLayer
    var presentedTime: CFTimeInterval = 0
    let drawableID: Int = 0
    private let onPresent: () -> Void

    init(texture: MTLTexture, layer: CAMetalLayer, onPresent: @escaping () -> Void) {
        self.texture = texture
        self.layer = layer
        self.onPresent = onPresent
    }

    func present() {
        presentedTime = CACurrentMediaTime()
        onPresent()
    }
    func present(at presentationTime: CFTimeInterval) { present() }
    func present(afterMinimumDuration duration: CFTimeInterval) { present() }
    func addPresentedHandler(_ block: @escaping MTLDrawablePresentedHandler) {}
}

private final class FirstPaintMetalLayer: CAMetalLayer {
    var presentCount = 0

    override func nextDrawable() -> (any CAMetalDrawable)? {
        guard let device, drawableSize.width >= 1, drawableSize.height >= 1 else { return nil }
        let descriptor = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .bgra8Unorm,
            width: Int(drawableSize.width),
            height: Int(drawableSize.height),
            mipmapped: false
        )
        descriptor.usage = .renderTarget
        descriptor.storageMode = .shared
        guard let texture = device.makeTexture(descriptor: descriptor) else { return nil }
        return FirstPaintDrawable(texture: texture, layer: self) { [weak self] in
            self?.presentCount += 1
        }
    }
}

/// Window whose occlusion state is controlled deterministically by tests.
private final class OcclusionControllableWindow: NSWindow {
    var mockOcclusionState: NSWindow.OcclusionState = [.visible]

    override var occlusionState: NSWindow.OcclusionState {
        mockOcclusionState
    }

    convenience init(size: NSSize = NSSize(width: 800, height: 400)) {
        self.init(
            contentRect: NSRect(origin: .zero, size: size),
            styleMask: [.titled, .closable, .resizable],
            backing: .buffered,
            defer: false
        )
        isReleasedWhenClosed = false
    }
}

final class TerminalFirstPaintTests: XCTestCase {
    private var savedConfig: AppConfig!
    private var savedFontSettings: (() -> (any FontSettings, CGFloat))?
    private var windows: [NSWindow] = []

    override func setUp() {
        super.setUp()
        _ = NSApplication.shared
        savedConfig = SessionManager.shared.config
        let config = AppConfig()
        SessionManager.shared.config = config
        savedFontSettings = FontShaper.settings
        FontShaper.settings = { (config, 14) }
        FontShaper.invalidateAll()
    }

    override func tearDown() {
        MetalContext.shared?.waitForGPU()
        for window in windows {
            window.close()
        }
        windows = []
        FontShaper.settings = savedFontSettings
        FontShaper.invalidateAll()
        SessionManager.shared.config = savedConfig
        super.tearDown()
    }

    private func requireMetal() throws {
        try XCTSkipIf(MTLCreateSystemDefaultDevice() == nil, "no Metal device")
        _ = try XCTUnwrap(MetalContext.shared, "Metal pipeline initialization failed")
    }

    private func makeWindow(size: NSSize = NSSize(width: 800, height: 400),
                            visible: Bool = true) -> OcclusionControllableWindow {
        let window = OcclusionControllableWindow(size: size)
        window.mockOcclusionState = visible ? [.visible] : []
        windows.append(window)
        return window
    }

    private func installLayer(on view: TerminalView) throws -> FirstPaintMetalLayer {
        let device = try XCTUnwrap(MetalContext.shared?.device)
        let layer = FirstPaintMetalLayer()
        layer.device = device
        layer.isOpaque = false
        layer.presentsWithTransaction = true
        layer.contentsScale = 2
        view.layer = layer
        return layer
    }

    // MARK: Decision helper tests

    func testShouldSuppressRenderFirstPaintWithValidSnapshotDrawsImmediately() {
        // First paint (!hasDrawn) with a valid snapshot at a mismatched size must paint
        // immediately rather than blank-stalling behind waitingForResizeFrame.
        XCTAssertFalse(TerminalView.shouldSuppressRender(
            hasDrawn: false,
            snapshotValid: true,
            sized: false,
            waitingForResizeFrame: true,
            resizeTimedOut: false,
            holdGridSize: false
        ))
    }

    func testShouldSuppressRenderFirstPaintWithInvalidSnapshotWaitsCleanly() {
        // Without a valid snapshot yet, waitingForResizeFrame still holds cleanly for the
        // initial grid or timeout.
        XCTAssertTrue(TerminalView.shouldSuppressRender(
            hasDrawn: false,
            snapshotValid: false,
            sized: false,
            waitingForResizeFrame: true,
            resizeTimedOut: false,
            holdGridSize: false
        ))
    }

    func testShouldSuppressRenderSubsequentResizeHoldsUntilSizedOrTimedOut() {
        // Once a frame is already on screen, mismatched frames are suppressed while a resize
        // is in flight (whether the snapshot is valid or invalid).
        for snapshotValid in [false, true] {
            XCTAssertTrue(TerminalView.shouldSuppressRender(
                hasDrawn: true,
                snapshotValid: snapshotValid,
                sized: false,
                waitingForResizeFrame: true,
                resizeTimedOut: false,
                holdGridSize: false
            ))
            // Clears as soon as the resized snapshot arrives.
            XCTAssertFalse(TerminalView.shouldSuppressRender(
                hasDrawn: true,
                snapshotValid: snapshotValid,
                sized: true,
                waitingForResizeFrame: true,
                resizeTimedOut: false,
                holdGridSize: false
            ))
            // Clears if the daemon did not answer in time.
            XCTAssertFalse(TerminalView.shouldSuppressRender(
                hasDrawn: true,
                snapshotValid: snapshotValid,
                sized: false,
                waitingForResizeFrame: true,
                resizeTimedOut: true,
                holdGridSize: false
            ))
        }
    }

    func testShouldSuppressRenderHoldGridSizeAlwaysDraws() {
        for hasDrawn in [false, true] {
            for snapshotValid in [false, true] {
                for sized in [false, true] {
                    for waiting in [false, true] {
                        for timedOut in [false, true] {
                            XCTAssertFalse(TerminalView.shouldSuppressRender(
                                hasDrawn: hasDrawn,
                                snapshotValid: snapshotValid,
                                sized: sized,
                                waitingForResizeFrame: waiting,
                                resizeTimedOut: timedOut,
                                holdGridSize: true
                            ))
                        }
                    }
                }
            }
        }
    }

    func testShouldSuppressRenderWhenNotWaitingAlwaysDraws() {
        for hasDrawn in [false, true] {
            for snapshotValid in [false, true] {
                for sized in [false, true] {
                    for timedOut in [false, true] {
                        XCTAssertFalse(TerminalView.shouldSuppressRender(
                            hasDrawn: hasDrawn,
                            snapshotValid: snapshotValid,
                            sized: sized,
                            waitingForResizeFrame: false,
                            resizeTimedOut: timedOut,
                            holdGridSize: false
                        ))
                    }
                }
            }
        }
    }

    // MARK: End-to-end TerminalView first paint & warm-hit tests

    func testFirstPaintDrawsValidMismatchedSnapshotImmediatelyAndSnapsOnResize() throws {
        try requireMetal()
        let window = makeWindow(size: NSSize(width: 800, height: 400), visible: false)
        let view = TerminalView(pane: .local(101))
        defer { view.detach() }
        view.frame = NSRect(x: 0, y: 0, width: 800, height: 400)
        let layer = try installLayer(on: view)
        window.contentView?.addSubview(view)

        // Pre-populate a valid warm snapshot whose dimensions differ from the view's grid.
        view.accessibilitySnapshot.show(lines: ["prompt$ echo warm"], cols: 40)
        XCTAssertTrue(view.accessibilitySnapshot.valid)
        XCTAssertNotEqual(view.cols, 40)
        XCTAssertFalse(view.hasDrawn)

        // Become visible: subscribes and enters waitingForResizeFrame because 40x1 != view grid.
        window.mockOcclusionState = [.visible]
        view.refreshSubscription()
        XCTAssertTrue(view.subscribed)
        XCTAssertTrue(view.waitingForResizeFrame)

        // First paint must NOT stall for 250 ms: it paints immediately pinned top-left and keeps
        // waitingForResizeFrame armed so subsequent mismatched frames hold this image.
        view.renderNow()
        XCTAssertEqual(layer.presentCount, 1)
        XCTAssertTrue(view.hasDrawn)
        XCTAssertTrue(view.waitingForResizeFrame)
        XCTAssertFalse(view.needsRender)
        let expectedGravity: CALayerContentsGravity = layer.contentsAreFlipped() ? .bottomLeft : .topLeft
        XCTAssertEqual(layer.contentsGravity, expectedGravity)
        let scale = window.backingScaleFactor
        XCTAssertEqual(layer.drawableSize, CGSize(width: (800 * scale).rounded(), height: (400 * scale).rounded()))

        // A follow-up render while still mismatched must hold the first-painted image.
        view.renderNow()
        XCTAssertEqual(layer.presentCount, 1)
        XCTAssertTrue(view.needsRender)
        XCTAssertTrue(view.waitingForResizeFrame)

        // Resizing again before the daemon answers must keep the held drawable size unstretched.
        let heldDrawableSize = layer.drawableSize
        view.setFrameSize(NSSize(width: 960, height: 480))
        XCTAssertEqual(layer.presentCount, 1)
        XCTAssertEqual(layer.drawableSize, heldDrawableSize)
        XCTAssertTrue(view.waitingForResizeFrame)

        // Once the daemon's resized snapshot matches (cols, rows), drawableSize and image snap cleanly.
        view.accessibilitySnapshot.show(lines: Array(repeating: "resized line", count: view.rows), cols: view.cols)
        view.renderNow()
        XCTAssertEqual(layer.presentCount, 2)
        XCTAssertFalse(view.waitingForResizeFrame)
        XCTAssertFalse(view.needsRender)
        XCTAssertEqual(layer.drawableSize, CGSize(width: (960 * scale).rounded(), height: (480 * scale).rounded()))
    }

    func testFirstPaintWithInvalidSnapshotWaitsUntilSnapshotArrives() throws {
        try requireMetal()
        let window = makeWindow(size: NSSize(width: 800, height: 400), visible: true)
        let view = TerminalView(pane: .local(102))
        defer { view.detach() }
        view.frame = NSRect(x: 0, y: 0, width: 800, height: 400)
        let layer = try installLayer(on: view)
        window.contentView?.addSubview(view)

        XCTAssertTrue(view.subscribed)
        XCTAssertFalse(view.accessibilitySnapshot.valid)
        XCTAssertTrue(view.waitingForResizeFrame)
        XCTAssertFalse(view.hasDrawn)

        // Invalid snapshot still waits cleanly while resize/attach is in flight.
        view.renderNow()
        XCTAssertEqual(layer.presentCount, 0)
        XCTAssertFalse(view.hasDrawn)
        XCTAssertTrue(view.needsRender)

        // As soon as initial attach arrives (even at a mismatched size), paint immediately.
        view.accessibilitySnapshot.show(lines: ["attached"], cols: 30)
        view.renderNow()
        XCTAssertEqual(layer.presentCount, 1)
        XCTAssertTrue(view.hasDrawn)
    }

    func testWarmHitMatchingSizeSkipsResizeWaitAndPaintsImmediatelyOnSubscription() throws {
        try requireMetal()
        let window = makeWindow(size: NSSize(width: 800, height: 400), visible: false)
        let view = TerminalView(pane: .local(103))
        defer { view.detach() }
        view.frame = NSRect(x: 0, y: 0, width: 800, height: 400)
        let layer = try installLayer(on: view)
        window.contentView?.addSubview(view)
        XCTAssertGreaterThan(view.cols, 0)
        XCTAssertGreaterThan(view.rows, 0)
        XCTAssertFalse(view.subscribed)
        XCTAssertEqual(layer.presentCount, 0)

        // Warm snapshot already matches the view's exact (cols, rows).
        view.accessibilitySnapshot.show(lines: Array(repeating: "warm", count: view.rows), cols: view.cols)

        // Becoming visible must not arm waitingForResizeFrame and must paint synchronously.
        window.mockOcclusionState = [.visible]
        view.refreshSubscription()
        XCTAssertTrue(view.subscribed)
        XCTAssertFalse(view.waitingForResizeFrame)
        XCTAssertEqual(layer.presentCount, 1)
        XCTAssertTrue(view.hasDrawn)
        XCTAssertFalse(view.needsRender)

        // Hiding and unhiding (e.g. single-window tab swap) also paints immediately on warm hit.
        view.isHidden = true
        XCTAssertFalse(view.subscribed)
        XCTAssertTrue(view.isFrameLinkPaused)

        view.isHidden = false
        XCTAssertTrue(view.subscribed)
        XCTAssertFalse(view.waitingForResizeFrame)
        XCTAssertEqual(layer.presentCount, 2)
        XCTAssertFalse(view.needsRender)
    }

    // MARK: Hide/unhide and window re-attachment lifecycle tests

    func testHideUnhideAndAncestorHideManageSubscriptionAndFrameLink() throws {
        try requireMetal()
        let window = makeWindow(size: NSSize(width: 800, height: 400), visible: true)
        let container = NSView(frame: NSRect(x: 0, y: 0, width: 800, height: 400))
        let view = TerminalView(pane: .local(104))
        defer { view.detach() }
        view.frame = container.bounds
        _ = try installLayer(on: view)
        container.addSubview(view)
        window.contentView?.addSubview(container)

        XCTAssertTrue(view.subscribed)
        XCTAssertTrue(view.hasActiveFrameLink)

        // Direct hide/unhide on TerminalView.
        view.isHidden = true
        XCTAssertFalse(view.subscribed)
        XCTAssertTrue(view.isFrameLinkPaused)

        view.isHidden = false
        XCTAssertTrue(view.subscribed)
        XCTAssertFalse(view.isFrameLinkPaused)

        // Ancestor hide/unhide (as when a TabContentView is hidden in a single-window swap).
        container.isHidden = true
        XCTAssertFalse(view.subscribed)
        XCTAssertTrue(view.isFrameLinkPaused)

        container.isHidden = false
        XCTAssertTrue(view.subscribed)
        XCTAssertFalse(view.isFrameLinkPaused)

        // Occlusion changes via refreshSubscription().
        window.mockOcclusionState = []
        view.refreshSubscription()
        XCTAssertFalse(view.subscribed)
        XCTAssertTrue(view.isFrameLinkPaused)

        window.mockOcclusionState = [.visible]
        view.refreshSubscription()
        XCTAssertTrue(view.subscribed)
        XCTAssertFalse(view.isFrameLinkPaused)
    }

    func testReattachingAcrossWindowsDoesNotDuplicateObserversOrBreakFrameLink() {
        let window1 = makeWindow(visible: true)
        let window2 = makeWindow(visible: true)
        let view = TerminalView(pane: .local(105))
        defer { view.detach() }
        view.frame = NSRect(x: 0, y: 0, width: 600, height: 400)

        // Repeatedly move out of and back into window1 (simulating cached TabContentView swaps).
        for _ in 0..<3 {
            window1.contentView?.addSubview(view)
            XCTAssertTrue(view.hasActiveFrameLink)
            XCTAssertTrue(view.subscribed)

            view.removeFromSuperview()
            XCTAssertFalse(view.hasActiveFrameLink)
            XCTAssertFalse(view.subscribed)

            let before = view.windowNotificationCount
            NotificationCenter.default.post(name: NSWindow.didBecomeKeyNotification, object: window1)
            NotificationCenter.default.post(name: NSWindow.didChangeOcclusionStateNotification, object: window1)
            XCTAssertEqual(view.windowNotificationCount, before, "detached view must not observe old window")
        }

        // Re-attach to window1 and even invoke viewDidMoveToWindow() twice directly.
        window1.contentView?.addSubview(view)
        view.viewDidMoveToWindow()
        XCTAssertTrue(view.hasActiveFrameLink)
        XCTAssertTrue(view.subscribed)

        let beforeKey = view.windowNotificationCount
        NotificationCenter.default.post(name: NSWindow.didBecomeKeyNotification, object: window1)
        XCTAssertEqual(view.windowNotificationCount, beforeKey + 1, "observers must never be duplicated")

        // Move directly from window1 to window2 without an intermediate removal.
        window2.contentView?.addSubview(view)
        XCTAssertTrue(view.hasActiveFrameLink)
        XCTAssertTrue(view.subscribed)

        let beforeMoveCheck = view.windowNotificationCount
        NotificationCenter.default.post(name: NSWindow.didBecomeKeyNotification, object: window1)
        XCTAssertEqual(view.windowNotificationCount, beforeMoveCheck, "previous window must no longer be observed")
        NotificationCenter.default.post(name: NSWindow.didBecomeKeyNotification, object: window2)
        XCTAssertEqual(view.windowNotificationCount, beforeMoveCheck + 1, "new window must be observed exactly once")

        // Explicit detach() cleans up observers, frameLink, and subscription, and re-attach recovers cleanly.
        view.detach()
        XCTAssertFalse(view.subscribed)
        XCTAssertFalse(view.hasActiveFrameLink)
        let beforeDetachNote = view.windowNotificationCount
        NotificationCenter.default.post(name: NSWindow.didChangeOcclusionStateNotification, object: window2)
        XCTAssertEqual(view.windowNotificationCount, beforeDetachNote)
        XCTAssertFalse(view.subscribed)

        view.removeFromSuperview()
        window2.contentView?.addSubview(view)
        XCTAssertTrue(view.hasActiveFrameLink)
        XCTAssertTrue(view.subscribed)
        let beforeReattachNote = view.windowNotificationCount
        NotificationCenter.default.post(name: NSWindow.didBecomeKeyNotification, object: window2)
        XCTAssertEqual(view.windowNotificationCount, beforeReattachNote + 1)
    }
}
