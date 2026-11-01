import AppKit
import Metal
import QuartzCore
import XCTest
@testable import Thurm

/// A real render target without a visible window; presentation records the calling transaction.
private final class BackdropDrawable: NSObject, CAMetalDrawable {
    let texture: MTLTexture
    unowned let layer: CAMetalLayer
    var presentedTime: CFTimeInterval = 0
    let drawableID: Int = 0
    var presentedOnMain = false
    var transactionValue: String?

    init(texture: MTLTexture, layer: CAMetalLayer) {
        self.texture = texture
        self.layer = layer
    }

    func present() {
        presentedOnMain = Thread.isMainThread
        transactionValue = CATransaction.value(forKey: "thurm.test.transaction") as? String
        presentedTime = CACurrentMediaTime()
    }
    func present(at presentationTime: CFTimeInterval) { present() }
    func present(afterMinimumDuration duration: CFTimeInterval) { present() }
    func addPresentedHandler(_ block: @escaping MTLDrawablePresentedHandler) {}
}

private final class BackdropMetalLayer: CAMetalLayer {
    var available = true
    var lastDrawable: BackdropDrawable?

    override func nextDrawable() -> (any CAMetalDrawable)? {
        guard available, let device else { return nil }
        let descriptor = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm,
            width: Int(drawableSize.width), height: Int(drawableSize.height), mipmapped: false)
        descriptor.usage = .renderTarget
        descriptor.storageMode = .shared
        guard let texture = device.makeTexture(descriptor: descriptor) else { return nil }
        let drawable = BackdropDrawable(texture: texture, layer: self)
        lastDrawable = drawable
        return drawable
    }
}

final class TerminalBackdropTests: XCTestCase {
    private var savedConfig: AppConfig!
    private var savedFontSettings: (() -> (any FontSettings, CGFloat))?

    override func setUp() {
        super.setUp()
        _ = NSApplication.shared
        savedConfig = SessionManager.shared.config
        let config = AppConfig()
        config.opacity = 0.6
        SessionManager.shared.config = config
        savedFontSettings = FontShaper.settings
        FontShaper.settings = { (config, 14) }
        FontShaper.invalidateAll()
    }

    override func tearDown() {
        MetalContext.shared?.waitForGPU()
        FontShaper.settings = savedFontSettings
        FontShaper.invalidateAll()
        SessionManager.shared.config = savedConfig
        super.tearDown()
    }

    private func requireMetal() throws {
        try XCTSkipIf(MTLCreateSystemDefaultDevice() == nil, "no Metal device")
        // A device without working render pipelines is a failure, not a reason to skip.
        _ = try XCTUnwrap(MetalContext.shared, "Metal pipeline initialization failed")
    }

    private func pane(in content: TabContentView, key: PaneKey? = nil) throws -> (TerminalView, BackdropMetalLayer) {
        let view = try XCTUnwrap(key.flatMap { content.views[$0] } ?? content.focusedView)
        let device = try XCTUnwrap(MetalContext.shared?.device)
        let layer = BackdropMetalLayer()
        layer.device = device
        layer.isOpaque = false
        layer.presentsWithTransaction = true
        layer.contentsScale = 2
        view.layer = layer
        view.metricsChanged()
        view.renderNow()
        XCTAssertNotNil(layer.backgroundColor)
        return (view, layer)
    }

    /// Covers the actual sidebar-sized expansion, including the gap between animation
    /// completion and the replacement image. The background must never double the opacity.
    func testHeldPaneKeepsItsBackgroundUntilTheResizedFrame() throws {
        try requireMetal()
        for headers in [false, true] {
            SessionManager.shared.config.paneHeaders = headers
            let key = PaneKey.local(1)
            let content = TabContentView(root: .leaf(key), focused: key, zoomed: nil)
            content.frame = CGRect(x: 0, y: 0, width: 600, height: 400)
            let (view, layer) = try pane(in: content)
            defer { content.detachAll() }
            let originalSize = layer.drawableSize
            view.holdGridSize = true
            content.setFrameSize(CGSize(width: 840, height: 400))
            XCTAssertEqual(layer.drawableSize, originalSize)
            let background = try XCTUnwrap(layer.backgroundColor)
            let point = content.convert(CGPoint(x: view.bounds.maxX - 10, y: 100), from: view)
            XCTAssertTrue(fills(in: content, at: point).isEmpty, "the pane layer owns the background")
            XCTAssertEqual(background.alpha, 0.6, accuracy: 0.001)
            XCTAssertFalse(layer.isOpaque, "Metal alpha must composite over the background")
            let covered = content.convert(CGPoint(x: 10, y: 10), from: view)
            XCTAssertTrue(fills(in: content, at: covered).isEmpty)

            view.holdGridSize = false
            content.layoutPanes()
            XCTAssertEqual(layer.backgroundColor, background, "keep the fill while waiting")
            layer.available = false
            view.renderNow()
            XCTAssertEqual(layer.backgroundColor, background, "a failed draw must keep the fill")
            XCTAssertTrue(view.needsRender)
            layer.available = true
            view.renderNow()
            XCTAssertNotNil(layer.backgroundColor)
            XCTAssertTrue(fills(in: content, at: point).isEmpty, "never add a second background under a translucent pane")
            XCTAssertFalse(view.needsRender)
        }
    }

    func testMacFramePresentsInTheCallingTransaction() throws {
        try requireMetal()
        let key = PaneKey.local(1)
        let content = TabContentView(root: .leaf(key), focused: key, zoomed: nil)
        defer { content.detachAll() }
        let view = try XCTUnwrap(content.focusedView)
        XCTAssertTrue(try XCTUnwrap(view.layer as? CAMetalLayer).presentsWithTransaction)
        let (_, layer) = try pane(in: content)
        CATransaction.begin()
        CATransaction.setValue("resized", forKey: "thurm.test.transaction")
        defer { CATransaction.commit() }
        view.renderNow()
        let drawable = try XCTUnwrap(layer.lastDrawable)
        XCTAssertTrue(drawable.presentedOnMain)
        XCTAssertEqual(drawable.transactionValue, "resized")
    }

    func testSidebarResizeWaitsForCaptureWhenDrawablesAreUnavailable() throws {
        try requireMetal()
        try XCTSkipIf(Motion.reduced, "sidebar reflow is disabled by Reduce Motion")
        for finishBeforeCapture in [false, true] {
            let key = PaneKey.local(1)
            let content = TabContentView(root: .leaf(key), focused: key, zoomed: nil)
            let window = NSWindow(contentRect: CGRect(x: 0, y: 0, width: 600, height: 400),
                                  styleMask: .borderless, backing: .buffered, defer: false)
            window.contentView = content
            defer {
                content.detachAll()
                window.contentView = nil
            }
            let (view, layer) = try pane(in: content)
            let oldCols = view.cols
            let oldRows = view.rows
            let oldDrawable = try XCTUnwrap(layer.lastDrawable)
            XCTAssertGreaterThan(oldCols, 0)

            layer.available = false
            let destination = CGSize(width: view.bounds.width + 240, height: view.bounds.height)
            view.beginSidebarResize(to: destination)
            XCTAssertEqual(view.cols, oldCols, "a failed capture must not request the new grid")
            XCTAssertEqual(view.rows, oldRows)
            view.renderNow()
            XCTAssertEqual(view.cols, oldCols, "retries must keep the old grid until submission succeeds")
            XCTAssertTrue(layer.lastDrawable === oldDrawable)
            XCTAssertTrue(view.needsRender)

            // Layout completion also reports a size. It must defer that report and keep the
            // latest bounds, which may differ from the original projection.
            let settled = CGSize(width: destination.width + 40, height: destination.height + 40)
            if finishBeforeCapture {
                view.setFrameSize(settled)
                view.endSidebarResize()
                XCTAssertEqual(view.cols, oldCols, "ending the slide must not bypass capture")
                XCTAssertEqual(view.rows, oldRows)
                XCTAssertTrue(view.needsRender)
            }

            layer.available = true
            view.renderNow()
            XCTAssertFalse(layer.lastDrawable === oldDrawable)
            XCTAssertTrue(try XCTUnwrap(layer.lastDrawable).presentedOnMain)
            XCTAssertGreaterThan(view.cols, oldCols, "a successful capture must release the pending resize")
            if finishBeforeCapture {
                XCTAssertGreaterThan(view.rows, oldRows, "use the settled size instead of the stale projection")
            } else {
                XCTAssertEqual(view.rows, oldRows)
                view.setFrameSize(destination)
                view.endSidebarResize()
            }
            let resizedCols = view.cols
            let resizedRows = view.rows
            view.renderNow()
            XCTAssertEqual(view.cols, resizedCols)
            XCTAssertEqual(view.rows, resizedRows)
        }
    }

    /// A sidebar reveals the prepared image without redrawing idle panes at every width.
    /// Check both endpoints' pixels, including dimming and transparency.
    func testSidebarViewportKeepsBackgroundPixelsUniform() throws {
        try requireMetal()
        let config = SessionManager.shared.config
        for background: UInt32 in [0x1E1E2E, 0xEFF1F5] {
            config.theme.background = background
            for opacity: CGFloat in [1, 0.6] {
                config.opacity = opacity
                let a = PaneKey.local(1), b = PaneKey.local(2)
                let content = TabContentView(root: .split(axis: .horizontal, ratio: 0.5,
                                                        first: .leaf(a), second: .leaf(b)),
                                             focused: a, zoomed: nil)
                defer { content.detachAll() }
                content.frame = CGRect(x: 0, y: 0, width: 600, height: 400)
                let panes = try [a, b].map { try pane(in: content, key: $0) }
                RunLoop.main.run(until: Date(timeIntervalSinceNow: Motion.fast + 0.02))
                var original: [[UInt8]] = []
                for (view, layer) in panes {
                    view.renderNow()
                    original.append(try pixel(in: layer, x: 10))
                }
                for widths: [CGFloat] in [[640, 720, 840], [800, 700, 600]] {
                    content.beginSidebarResize(contentWidth: try XCTUnwrap(widths.last))
                    let prepared = try panes.map { try XCTUnwrap($0.1.lastDrawable) }
                    for width in widths {
                        content.setFrameSize(CGSize(width: width, height: 400))
                        for (i, pair) in panes.enumerated() {
                            let (view, layer) = pair
                            XCTAssertTrue(view.holdGridSize)
                            XCTAssertTrue(layer.lastDrawable === prepared[i], "layout alone must not redraw an idle pane")
                            XCTAssertEqual(Int(layer.drawableSize.width), prepared[i].texture.width)
                            XCTAssertGreaterThanOrEqual(layer.drawableSize.width, (view.bounds.width * 2).rounded())
                            XCTAssertNotNil(layer.backgroundColor)
                            let right = Int((view.bounds.width * 2).rounded()) - 10
                            XCTAssertEqual(try pixel(in: layer, x: 10), original[i])
                            XCTAssertEqual(try pixel(in: layer, x: right), original[i],
                                           "exposed background differs: \(background), opacity \(opacity), width \(width)")
                        }
                    }
                    for (view, layer) in panes {
                        view.endSidebarResize()
                        XCTAssertEqual(layer.drawableSize.width, (view.bounds.width * 2).rounded())
                    }
                }
            }
        }
    }

    /// The layer-backed path must match the existing Metal composition, including the
    /// alpha-preserving dim/bell covers. Check real GPU output to catch double opacity.
    func testLayerSurfaceMatchesMetalClearAndCovers() throws {
        try requireMetal()
        let renderer = TerminalRenderer()
        let layer = BackdropMetalLayer()
        layer.device = try XCTUnwrap(MetalContext.shared?.device)
        layer.presentsWithTransaction = true
        layer.drawableSize = CGSize(width: 100, height: 200)
        let shaper = try XCTUnwrap(FontShaper.shared(scale: 2))
        for background: UInt32 in [0x1E1E2E, 0xEFF1F5] {
            for opacity: Float in [0, 0.6, 1] {
                for dim: Float in [0, 0.25] {
                    for flash: Float in [0, 0.8] {
                        var params = RenderParams(pane: .local(1), shaper: shaper, padX: 0, padY: 0,
                            viewportWidth: 100, viewportHeight: 200, focused: false, cursorOn: false,
                            cursorThickness: 1, dim: dim, dimFill: 0.3, flash: flash, opacity: opacity,
                            themeBackground: background, themeForeground: 0xCDD6F4, hoverLink: 0, hoverSpan: nil)
                        XCTAssertTrue(renderer.draw(layer: layer, params: params))
                        let expected = try rawPixel(in: layer, x: 10)
                        XCTAssertEqual(Double(expected[3]), Double(opacity) * 255, accuracy: 1)

                        params.backgroundInLayer = true
                        let color = renderer.surfaceColor(params: params)
                        layer.backgroundColor = NSColor(srgbRed: CGFloat(color.x), green: CGFloat(color.y),
                                                        blue: CGFloat(color.z), alpha: CGFloat(color.w)).cgColor
                        XCTAssertTrue(renderer.draw(layer: layer, params: params))
                        XCTAssertEqual(try rawPixel(in: layer, x: 10), [0, 0, 0, 0], "the base is drawn only by the layer")
                        let actual = try pixel(in: layer, x: 10)
                        for channel in 0..<4 {
                            XCTAssertEqual(Double(actual[channel]), Double(expected[channel]), accuracy: 2,
                                           "surface mismatch: \(background), opacity \(opacity), dim \(dim), flash \(flash)")
                        }
                    }
                }
            }
        }
    }

    private func rawPixel(in layer: BackdropMetalLayer, x: Int) throws -> [UInt8] {
        MetalContext.shared?.waitForGPU()
        let texture = try XCTUnwrap(layer.lastDrawable?.texture)
        var pixel = [UInt8](repeating: 0, count: 4)
        texture.getBytes(&pixel, bytesPerRow: 4, from: MTLRegionMake2D(x, 100, 1, 1), mipmapLevel: 0)
        return pixel
    }

    func testContentReflowBlendsPixelsWithoutStretchingOrDoublingAlpha() throws {
        try requireMetal()
        let renderer = TerminalRenderer()
        let layer = BackdropMetalLayer()
        layer.device = try XCTUnwrap(MetalContext.shared?.device)
        layer.presentsWithTransaction = true
        layer.drawableSize = CGSize(width: 100, height: 200)
        var params = RenderParams(pane: .local(1), shaper: try XCTUnwrap(FontShaper.shared(scale: 2)),
            padX: 0, padY: 0, viewportWidth: 100, viewportHeight: 200, focused: false, cursorOn: false,
            cursorThickness: 1, dim: 0, dimFill: 0.3, flash: 0, opacity: 0.6,
            themeBackground: 0xFF0000, themeForeground: 0xFFFFFF, hoverLink: 0, hoverSpan: nil,
            contentTransition: .capture)
        XCTAssertTrue(renderer.draw(layer: layer, params: params))
        XCTAssertEqual(try rawPixel(in: layer, x: 10), [0, 0, 153, 153])
        params.themeBackground = 0x0000FF
        params.contentTransition = .blend(0)
        XCTAssertTrue(renderer.draw(layer: layer, params: params))
        XCTAssertEqual(try rawPixel(in: layer, x: 10), [0, 0, 153, 153], "the new layout starts at the old image")

        params.contentTransition = .blend(0.5)
        params.viewportWidth = 120
        layer.drawableSize.width = 120
        XCTAssertTrue(renderer.draw(layer: layer, params: params))
        let middle = try rawPixel(in: layer, x: 10)
        for (actual, expected) in zip(middle, [76.5, 0, 76.5, 153]) {
            XCTAssertEqual(Double(actual), expected, accuracy: 1)
        }
        let exposed = try rawPixel(in: layer, x: 110)
        XCTAssertEqual(exposed[2], 0, "the old red image must not stretch into the new width")
        XCTAssertEqual(Double(exposed[3]), 76.5, accuracy: 1)

        params.viewportWidth = 80
        layer.drawableSize.width = 80
        XCTAssertTrue(renderer.draw(layer: layer, params: params))
        XCTAssertEqual(try rawPixel(in: layer, x: 70), middle, "shrinking crops the old image")
        params.contentTransition = .none
        XCTAssertTrue(renderer.draw(layer: layer, params: params))
        XCTAssertEqual(try rawPixel(in: layer, x: 10), [153, 0, 0, 153])
        params.contentTransition = .blend(0)
        XCTAssertTrue(renderer.draw(layer: layer, params: params))
        XCTAssertEqual(try rawPixel(in: layer, x: 10), [153, 0, 0, 153], "a finished transition releases its old image")
    }

    private func pixel(in layer: BackdropMetalLayer, x: Int) throws -> [UInt8] {
        let pixel = try rawPixel(in: layer, x: x)
        let bg = try XCTUnwrap(NSColor(cgColor: try XCTUnwrap(layer.backgroundColor))?.usingColorSpace(.sRGB))
        let alpha = Double(pixel[3]) / 255
        let base = [bg.blueComponent, bg.greenComponent, bg.redComponent, CGFloat(1)]
        return (0..<4).map { i in
            UInt8(clamping: Int((Double(pixel[i]) + Double(base[i] * bg.alphaComponent) * 255 * (1 - alpha)).rounded()))
        }
    }

    func testSidebarDestinationSizesMatchSettledLayout() throws {
        let a = PaneKey.local(1), b = PaneKey.local(2), c = PaneKey.local(3), d = PaneKey.local(4)
        let root = SplitNode.split(axis: .horizontal, ratio: 0.37, first: .leaf(a),
                                  second: .split(axis: .vertical, ratio: 0.6, first: .leaf(b), second: .leaf(c)))
        for headers in [false, true] {
            SessionManager.shared.config.paneHeaders = headers
            for zoom: PaneKey? in [nil, b] {
                for floatingZoom in [false, true] {
                    let floating = FloatingPane(pane: d, frame: [0.2, 0.1, 0.5, 0.6], zoomed: floatingZoom)
                    let content = TabContentView(root: root, focused: a, zoomed: zoom, floating: [floating])
                    defer { content.detachAll() }
                    content.frame = CGRect(x: 0, y: 0, width: 800, height: 600)
                    for width: CGFloat in [1040, 600, 800] {
                        let before = content.views.mapValues(\.frame)
                        let expected = content.sidebarTerminalSizes(contentWidth: width)
                        XCTAssertEqual(content.views.mapValues(\.frame), before, "projection must not move views")
                        content.setFrameSize(CGSize(width: width, height: 600))
                        for (id, size) in expected {
                            XCTAssertEqual(content.views[id]?.frame.size, size,
                                           "destination mismatch: headers \(headers), zoom \(String(describing: zoom)), width \(width)")
                        }
                    }
                }
            }
        }
    }

    /// The backdrop is the first subview. Inspect its visible fills in content coordinates.
    private func fills(in content: TabContentView, at point: CGPoint) -> [CALayer] {
        content.subviews.first?.layer?.sublayers?.filter {
            !$0.isHidden && $0.frame.contains(point)
        } ?? []
    }
}
