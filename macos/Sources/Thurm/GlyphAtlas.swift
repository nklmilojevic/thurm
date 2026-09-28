import CoreGraphics
import CoreText
import Metal

/// A texture atlas filled with a simple shelf packer. Glyph coordinates are stored in
/// texel units and sampled with `coord::pixel`, so growing the atlas (which only adds rows at
/// the bottom) never invalidates existing entries.
final class GlyphAtlas {
    let device: MTLDevice
    let pixelFormat: MTLPixelFormat
    let bytesPerPixel: Int

    private(set) var texture: MTLTexture
    private(set) var width: Int
    private(set) var height: Int
    /// CPU copy of the texture contents, needed to re-upload after growing.
    private var pixels: [UInt8]

    private var cursorX = 1
    private var cursorY = 1
    private var shelfHeight = 0

    /// Incremented whenever the atlas had to be wiped; cached entries become invalid.
    private(set) var epoch = 0

    static let maxHeight = 8192

    init?(device: MTLDevice, pixelFormat: MTLPixelFormat, bytesPerPixel: Int, size: Int = 1024) {
        guard let tex = GlyphAtlas.makeTexture(device: device, format: pixelFormat, width: size, height: size) else {
            return nil
        }
        self.device = device
        self.pixelFormat = pixelFormat
        self.bytesPerPixel = bytesPerPixel
        self.texture = tex
        self.width = size
        self.height = size
        self.pixels = [UInt8](repeating: 0, count: size * size * bytesPerPixel)
    }

    private static func makeTexture(device: MTLDevice, format: MTLPixelFormat, width: Int, height: Int) -> MTLTexture? {
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: format, width: width, height: height,
                                                            mipmapped: false)
        desc.usage = [.shaderRead]
        return device.makeTexture(descriptor: desc)
    }

    /// Reserves a `w`×`h` region (with a 1 texel gutter). Nil when it can never fit.
    func reserve(width w: Int, height h: Int) -> (x: Int, y: Int)? {
        guard w > 0, h > 0, w + 2 <= width, h + 2 <= GlyphAtlas.maxHeight else { return nil }
        if cursorX + w + 1 > width {
            cursorX = 1
            cursorY += shelfHeight + 1
            shelfHeight = 0
        }
        if cursorY + h + 1 > height {
            if !grow(toAtLeast: cursorY + h + 1) {
                wipe()
                guard h + 2 <= height else { return nil }
            }
        }
        let origin = (x: cursorX, y: cursorY)
        cursorX += w + 1
        shelfHeight = max(shelfHeight, h)
        return origin
    }

    /// Copies `w`×`h` pixels (tightly packed rows) into the atlas at (x, y).
    func upload(x: Int, y: Int, width w: Int, height h: Int, bytes: [UInt8]) {
        let rowBytes = w * bytesPerPixel
        guard bytes.count >= rowBytes * h else { return }
        let atlasRowBytes = width * bytesPerPixel
        bytes.withUnsafeBufferPointer { src in
            pixels.withUnsafeMutableBufferPointer { dst in
                guard let s = src.baseAddress, let d = dst.baseAddress else { return }
                for row in 0..<h {
                    let dstOffset = (y + row) * atlasRowBytes + x * bytesPerPixel
                    (d + dstOffset).update(from: s + row * rowBytes, count: rowBytes)
                }
            }
        }
        // Frames in flight may still read this region (a wiped atlas reuses space).
        MetalContext.shared?.waitForGPU()
        bytes.withUnsafeBytes { raw in
            guard let base = raw.baseAddress else { return }
            texture.replace(region: MTLRegionMake2D(x, y, w, h), mipmapLevel: 0,
                            withBytes: base, bytesPerRow: rowBytes)
        }
    }

    private func grow(toAtLeast needed: Int) -> Bool {
        var newHeight = height
        while newHeight < needed { newHeight *= 2 }
        newHeight = min(newHeight, GlyphAtlas.maxHeight)
        guard newHeight >= needed, newHeight > height,
              let tex = GlyphAtlas.makeTexture(device: device, format: pixelFormat, width: width, height: newHeight)
        else { return false }
        // Rows are appended at the bottom, so the CPU copy keeps its layout.
        pixels.append(contentsOf: [UInt8](repeating: 0, count: (newHeight - height) * width * bytesPerPixel))
        height = newHeight
        texture = tex
        let rowBytes = width * bytesPerPixel
        pixels.withUnsafeBytes { raw in
            guard let base = raw.baseAddress else { return }
            tex.replace(region: MTLRegionMake2D(0, 0, width, height), mipmapLevel: 0,
                        withBytes: base, bytesPerRow: rowBytes)
        }
        return true
    }

    /// Clears everything (atlas full at max size). Callers drop their caches via `epoch`.
    private func wipe() {
        pixels = [UInt8](repeating: 0, count: pixels.count)
        cursorX = 1
        cursorY = 1
        shelfHeight = 0
        epoch += 1
    }
}

// MARK: - Glyph cache

struct GlyphKey: Hashable {
    let font: Int
    let glyph: CGGlyph
    /// Fit into this many cells (Nerd Font icons), 0 = natural size.
    var span: UInt8 = 0
}

/// Box a constrained glyph is fitted into: `span` cells, with the baseline at `baseline`
/// pixels from the cell top.
struct CellBox {
    let width: Int
    let height: Int
    let baseline: Int
}

/// Location of a rasterized glyph. `left`/`top` place the bitmap relative to the pen position
/// on the baseline (top is measured upwards), in pixels.
struct GlyphEntry {
    let x: Float
    let y: Float
    let w: Float
    let h: Float
    let left: Float
    let top: Float
    let color: Bool

    var isEmpty: Bool { w <= 0 || h <= 0 }

    static let empty = GlyphEntry(x: 0, y: 0, w: 0, h: 0, left: 0, top: 0, color: false)
}

/// Rasterizes glyphs with CoreGraphics into an R8 (coverage) atlas and a BGRA atlas for color
/// (emoji) fonts.
final class GlyphCache {
    let gray: GlyphAtlas
    let color: GlyphAtlas
    private var entries: [GlyphKey: GlyphEntry] = [:]
    private var grayEpoch = 0
    private var colorEpoch = 0
    private let graySpace = CGColorSpace(name: CGColorSpace.linearGray) ?? CGColorSpaceCreateDeviceGray()
    private let rgbSpace = CGColorSpaceCreateDeviceRGB()
    /// Font smoothing strength (0...1) when thickening, nil = off.
    private let thicken: CGFloat?

    init?(device: MTLDevice, thicken: CGFloat?) {
        self.thicken = thicken
        guard let g = GlyphAtlas(device: device, pixelFormat: .r8Unorm, bytesPerPixel: 1),
              let c = GlyphAtlas(device: device, pixelFormat: .bgra8Unorm, bytesPerPixel: 4)
        else { return nil }
        gray = g
        color = c
    }

    func entry(for key: GlyphKey, font: CTFont, isColor: Bool, cell: CellBox? = nil) -> GlyphEntry {
        if gray.epoch != grayEpoch || color.epoch != colorEpoch {
            entries.removeAll()
            grayEpoch = gray.epoch
            colorEpoch = color.epoch
        }
        if let e = entries[key] { return e }
        let box = key.span > 0 ? cell.map { CellBox(width: $0.width * Int(key.span), height: $0.height,
                                                     baseline: $0.baseline) } : nil
        let e = rasterize(glyph: key.glyph, font: font, isColor: isColor, fit: box)
        entries[key] = e
        return e
    }

    /// Rasterizes one glyph. With `fit`, the glyph is scaled to fit the box (keeping its aspect)
    /// and centered in it, and the entry is positioned relative to the cell's pen position.
    private func rasterize(glyph: CGGlyph, font: CTFont, isColor: Bool, fit: CellBox?) -> GlyphEntry {
        var g = glyph
        var rect = CGRect.zero
        CTFontGetBoundingRectsForGlyphs(font, .horizontal, &g, &rect, 1)
        if rect.width <= 0 || rect.height <= 0 || !rect.origin.x.isFinite || !rect.origin.y.isFinite {
            return .empty
        }
        // Where the (scaled) glyph goes, in pixels relative to the pen on the baseline.
        var scale: CGFloat = 1
        var target = rect
        if let box = fit {
            // A little breathing room, like Ghostty's icon padding.
            let bw = CGFloat(box.width) * 0.94
            let bh = CGFloat(box.height) * 0.88
            scale = min(bw / rect.width, bh / rect.height)
            let w = rect.width * scale
            let h = rect.height * scale
            // Center horizontally in the box, vertically in the cell (baseline coordinates).
            let x = (CGFloat(box.width) - w) / 2
            let cellBottom = -CGFloat(box.height - box.baseline)
            let y = cellBottom + (CGFloat(box.height) - h) / 2
            target = CGRect(x: x, y: y, width: w, height: h)
        }
        // Smoothing can grow a glyph by a pixel on every side.
        let pad = thicken != nil && !isColor ? 2 : 1
        let left = Int(floor(target.minX)) - pad
        let bottom = Int(floor(target.minY)) - pad
        let w = Int(ceil(target.maxX)) - left + pad
        let h = Int(ceil(target.maxY)) - bottom + pad
        guard w > 0, h > 0, w < 2048, h < 2048 else { return .empty }

        let atlas = isColor ? color : gray
        let bpp = atlas.bytesPerPixel
        var buffer = [UInt8](repeating: 0, count: w * h * bpp)
        let drew: Bool = buffer.withUnsafeMutableBytes { raw -> Bool in
            let info: UInt32
            let space: CGColorSpace
            if isColor {
                info = CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue
                space = rgbSpace
            } else {
                // Coverage only, as Ghostty does: font smoothing needs an alpha-only target.
                info = CGImageAlphaInfo.alphaOnly.rawValue
                space = graySpace
            }
            guard let ctx = CGContext(data: raw.baseAddress, width: w, height: h, bitsPerComponent: 8,
                                      bytesPerRow: w * bpp, space: space, bitmapInfo: info)
            else { return false }
            ctx.setAllowsAntialiasing(true)
            ctx.setShouldAntialias(true)
            ctx.setAllowsFontSubpixelPositioning(false)
            ctx.setShouldSubpixelPositionFonts(false)
            if !isColor {
                // "Thicken": CoreText font smoothing; the fill gray sets its strength.
                ctx.setAllowsFontSmoothing(thicken != nil)
                ctx.setShouldSmoothFonts(thicken != nil)
                ctx.setFillColor(gray: thicken ?? 1, alpha: 1)
            }
            // Map the glyph's own bounds onto the target rectangle.
            ctx.translateBy(x: target.minX - CGFloat(left), y: target.minY - CGFloat(bottom))
            ctx.scaleBy(x: scale, y: scale)
            var position = CGPoint(x: -rect.minX, y: -rect.minY)
            CTFontDrawGlyphs(font, &g, &position, 1, ctx)
            return true
        }
        guard drew, let origin = atlas.reserve(width: w, height: h) else { return .empty }
        atlas.upload(x: origin.x, y: origin.y, width: w, height: h, bytes: buffer)
        return GlyphEntry(x: Float(origin.x), y: Float(origin.y), w: Float(w), h: Float(h),
                          left: Float(left), top: Float(bottom + h), color: isColor)
    }
}
