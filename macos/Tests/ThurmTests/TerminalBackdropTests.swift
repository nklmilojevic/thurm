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

    private func pixel(in layer: BackdropMetalLayer, x: Int) throws -> [UInt8] {
        let pixel = try rawPixel(in: layer, x: x)
        let bg = try XCTUnwrap(NSColor(cgColor: try XCTUnwrap(layer.backgroundColor))?.usingColorSpace(.sRGB))
        let alpha = Double(pixel[3]) / 255
        let base = [bg.blueComponent, bg.greenComponent, bg.redComponent, CGFloat(1)]
        return (0..<4).map { i in
            UInt8(clamping: Int((Double(pixel[i]) + Double(base[i] * bg.alphaComponent) * 255 * (1 - alpha)).rounded()))
        }
    }

    /// The backdrop is the first subview. Inspect its visible fills in content coordinates.
    private func fills(in content: TabContentView, at point: CGPoint) -> [CALayer] {
        content.subviews.first?.layer?.sublayers?.filter {
            !$0.isHidden && $0.frame.contains(point)
        } ?? []
    }
}
