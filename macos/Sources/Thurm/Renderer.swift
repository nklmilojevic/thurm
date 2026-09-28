import AppKit
import Metal
import QuartzCore
import CThurm

// MARK: - GPU instance format

/// One instanced quad. Must match `struct Quad` in the shader (64 bytes).
struct QuadInstance {
    /// x, y, width, height in pixels (origin top-left).
    var rect: SIMD4<Float>
    /// Texture rectangle in texels (u0, v0, u1, v1), or kind specific parameters.
    var uv: SIMD4<Float>
    /// Straight (non premultiplied) RGBA.
    var color: SIMD4<Float>
    var kind: UInt32
    var pad0: UInt32 = 0
    var pad1: UInt32 = 0
    var pad2: UInt32 = 0

    init(rect: SIMD4<Float>, uv: SIMD4<Float> = SIMD4<Float>(0, 0, 0, 0), color: SIMD4<Float>, kind: UInt32) {
        self.rect = rect
        self.uv = uv
        self.color = color
        self.kind = kind
    }
}

enum QuadKind {
    static let solid: UInt32 = 0
    static let grayGlyph: UInt32 = 1
    static let colorGlyph: UInt32 = 2
    static let undercurl: UInt32 = 3
    static let image: UInt32 = 4
    static let dotted: UInt32 = 5
    static let dashed: UInt32 = 6
    static let arc: UInt32 = 7
}

/// Converts 0xRRGGBB to a float color.
@inline(__always)
func rgba(_ rgb: UInt32, _ alpha: Float = 1) -> SIMD4<Float> {
    SIMD4<Float>(Float((rgb >> 16) & 0xFF) / 255, Float((rgb >> 8) & 0xFF) / 255, Float(rgb & 0xFF) / 255, alpha)
}

// MARK: - Shader (compiled at runtime; SwiftPM cannot build .metal files)

private let shaderSource = """
#include <metal_stdlib>
using namespace metal;

struct Quad {
    float4 rect;
    float4 uv;
    float4 color;
    uint kind;
    uint pad0;
    uint pad1;
    uint pad2;
};

struct Uniforms {
    float2 viewport;
};

struct VOut {
    float4 position [[position]];
    float2 uv;
    float2 local;
    float2 size;
    float4 color;
    float4 params;
    uint kind [[flat]];
};

vertex VOut vs_main(uint vid [[vertex_id]],
                    uint iid [[instance_id]],
                    const device Quad *quads [[buffer(0)]],
                    constant Uniforms &u [[buffer(1)]]) {
    Quad q = quads[iid];
    float2 corner = float2(float(vid & 1u), float(vid >> 1u));
    float2 pos = q.rect.xy + corner * q.rect.zw;
    VOut o;
    o.position = float4(pos.x / u.viewport.x * 2.0 - 1.0, 1.0 - pos.y / u.viewport.y * 2.0, 0.0, 1.0);
    o.uv = mix(q.uv.xy, q.uv.zw, corner);
    o.local = corner * q.rect.zw;
    o.size = q.rect.zw;
    o.color = q.color;
    o.params = q.uv;
    o.kind = q.kind;
    return o;
}

fragment float4 fs_main(VOut in [[stage_in]],
                        texture2d<float> grayAtlas [[texture(0)]],
                        texture2d<float> colorAtlas [[texture(1)]],
                        texture2d<float> image [[texture(2)]]) {
    constexpr sampler nearestPx(coord::pixel, filter::nearest, address::clamp_to_edge);
    constexpr sampler linearPx(coord::pixel, filter::linear, address::clamp_to_edge);
    float4 c = in.color;
    switch (in.kind) {
    case 0u: {
        return float4(c.rgb * c.a, c.a);
    }
    case 1u: {
        float a = grayAtlas.sample(nearestPx, in.uv).r * c.a;
        return float4(c.rgb * a, a);
    }
    case 2u: {
        float4 s = colorAtlas.sample(nearestPx, in.uv);
        return s * c.a;
    }
    case 3u: {
        // Undercurl: params.x = thickness, params.y = wavelength, params.z = phase (quad x).
        float t = max(in.params.x, 1.0);
        float period = max(in.params.y, 2.0);
        float amp = max((in.size.y - t) * 0.5, 0.0);
        float k = 6.2831853 / period;
        float x = in.local.x + in.params.z;
        float y = in.size.y * 0.5 + amp * sin(x * k);
        float slope = amp * k * cos(x * k);
        float d = abs(in.local.y - y) / sqrt(1.0 + slope * slope);
        float a = clamp(t * 0.5 - d + 0.5, 0.0, 1.0) * c.a;
        return float4(c.rgb * a, a);
    }
    case 4u: {
        float4 s = image.sample(linearPx, in.uv);
        float a = s.a * c.a;
        return float4(s.rgb * a, a);
    }
    case 5u: {
        // Dotted: params.x = dot size, params.z = phase.
        float t = max(in.params.x, 1.0);
        float x = in.local.x + in.params.z;
        float a = (fmod(x, 2.0 * t) < t) ? c.a : 0.0;
        return float4(c.rgb * a, a);
    }
    case 6u: {
        // Dashed: params.y = dash length, params.z = phase.
        float dash = max(in.params.y, 2.0);
        float x = in.local.x + in.params.z;
        float a = (fmod(x, 2.0 * dash) < dash) ? c.a : 0.0;
        return float4(c.rgb * a, a);
    }
    case 7u: {
        // Quarter-circle stroke: params = (radius, thickness, center x, center y) in the quad.
        float d = abs(length(in.local - in.params.zw) - in.params.x);
        float a = clamp(in.params.y * 0.5 - d + 0.5, 0.0, 1.0) * c.a;
        return float4(c.rgb * a, a);
    }
    default:
        return float4(0.0);
    }
}
"""

// MARK: - Shared Metal state

final class MetalContext {
    static let shared: MetalContext? = MetalContext()

    let device: MTLDevice
    let queue: MTLCommandQueue
    let pipeline: MTLRenderPipelineState
    /// 1×1 transparent texture bound when a slot has nothing to show.
    let emptyTexture: MTLTexture
    /// The most recently committed frame (all panes share this serial queue).
    var lastCommitted: MTLCommandBuffer?

    /// Blocks until the GPU finished every committed frame, before the CPU rewrites a texture
    /// region those frames may still sample (glyph atlas uploads, which are rare).
    func waitForGPU() {
        if let cb = lastCommitted, cb.status != .completed, cb.status != .error {
            cb.waitUntilCompleted()
        }
    }

    private init?() {
        guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue() else {
            tlog("Metal is not available")
            return nil
        }
        self.device = device
        self.queue = queue
        do {
            let library = try device.makeLibrary(source: shaderSource, options: nil)
            guard let vfn = library.makeFunction(name: "vs_main"),
                  let ffn = library.makeFunction(name: "fs_main")
            else {
                tlog("shader functions missing")
                return nil
            }
            let desc = MTLRenderPipelineDescriptor()
            desc.label = "Thurm quads"
            desc.vertexFunction = vfn
            desc.fragmentFunction = ffn
            // Explicit type: compiles whether the SDK imports the subscript as `T!` or `T`.
            let att: MTLRenderPipelineColorAttachmentDescriptor = desc.colorAttachments[0]
            att.pixelFormat = .bgra8Unorm
            att.isBlendingEnabled = true
            att.rgbBlendOperation = .add
            att.alphaBlendOperation = .add
            att.sourceRGBBlendFactor = .one
            att.sourceAlphaBlendFactor = .one
            att.destinationRGBBlendFactor = .oneMinusSourceAlpha
            att.destinationAlphaBlendFactor = .oneMinusSourceAlpha
            pipeline = try device.makeRenderPipelineState(descriptor: desc)
        } catch {
            tlog("could not build the Metal pipeline: \(error)")
            return nil
        }
        let texDesc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .rgba8Unorm, width: 1, height: 1,
                                                               mipmapped: false)
        texDesc.usage = [.shaderRead]
        guard let empty = device.makeTexture(descriptor: texDesc) else { return nil }
        var zero: [UInt8] = [0, 0, 0, 0]
        empty.replace(region: MTLRegionMake2D(0, 0, 1, 1), mipmapLevel: 0, withBytes: &zero, bytesPerRow: 4)
        emptyTexture = empty
    }
}

// MARK: - Grid snapshot

/// A copy of a pane's grid, taken under `thurm_grid_lock` and released immediately.
final class GridSnapshot {
    private(set) var info = thurm_grid_info()
    private(set) var cells: [thurm_cell] = []
    /// Grapheme clusters per row: column → full cluster text.
    private(set) var rowClusters: [[Int: String]] = []
    private(set) var links: [String] = []
    private(set) var images: [thurm_image_placement] = []
    private(set) var valid = false
    /// History line above the viewport (smooth scrolling), empty when there is none.
    private(set) var peekCells: [thurm_cell] = []
    private(set) var peekClusters: [Int: String] = [:]

    var cols: Int { Int(info.cols) }
    var rows: Int { Int(info.rows) }

    /// Pulls the latest grid. Returns true when anything changed.
    func update(pane: UInt64) -> Bool {
        guard let client = Core.shared.client else { return false }
        var newInfo = thurm_grid_info()
        var ptr: UnsafePointer<thurm_cell>? = nil
        guard thurm_grid_lock(client, pane, &newInfo, &ptr) else { return false }
        defer { thurm_grid_unlock(client, pane) }
        if valid && newInfo.generation == info.generation && newInfo.cols == info.cols && newInfo.rows == info.rows {
            return false
        }
        let cols = Int(newInfo.cols)
        let rows = Int(newInfo.rows)
        let count = cols * rows
        let sizeChanged = !valid || newInfo.cols != info.cols || newInfo.rows != info.rows

        if cells.count != count {
            cells = [thurm_cell](repeating: thurm_cell(), count: count)
        }
        if let src = ptr, count > 0 {
            cells.withUnsafeMutableBufferPointer { dst in
                if let base = dst.baseAddress { base.update(from: src, count: count) }
            }
        }

        // Clusters: only re-query dirty rows.
        if sizeChanged || rowClusters.count != rows {
            rowClusters = [[Int: String]](repeating: [:], count: rows)
        } else if newInfo.shift != 0 {
            // Scrollback moved: keep the clusters of rows that only changed position.
            let shift = Int(newInfo.shift)
            let old = rowClusters
            rowClusters = (0..<rows).map { r in
                let src = r - shift
                return src >= 0 && src < rows ? old[src] : [:]
            }
        }
        let dirty: UInt64 = sizeChanged ? UInt64.max : newInfo.dirty_rows
        func isDirty(_ r: Int) -> Bool { dirty & (UInt64(1) << UInt64(min(r, 63))) != 0 }
        if dirty != 0 {
            for r in 0..<rows where isDirty(r) { rowClusters[r] = [:] }
            // All clusters in one call; keep the ones of dirty rows (clusters are rare).
            let total = thurm_grid_clusters(client, pane, nil, 0)
            if total > 0 {
                var list = [thurm_cluster](repeating: thurm_cluster(), count: total)
                let n = list.withUnsafeMutableBufferPointer { thurm_grid_clusters(client, pane, $0.baseAddress, total) }
                for item in list.prefix(min(n, total)) {
                    let r = Int(item.row)
                    guard r < rows, isDirty(r), let text = item.text else { continue }
                    rowClusters[r][Int(item.col)] = String(cString: text)
                }
            }
        }

        peekCells.removeAll(keepingCapacity: true)
        peekClusters = [:]
        if newInfo.has_peek {
            var peekPtr: UnsafePointer<thurm_cell>? = nil
            let n = thurm_grid_peek(client, pane, &peekPtr)
            if let peekPtr, n == cols {
                peekCells.append(contentsOf: UnsafeBufferPointer(start: peekPtr, count: n))
                for c in 0..<n where peekCells[c].ch > 32 {
                    if let s = thurm_grid_peek_cluster(client, pane, UInt16(c)) {
                        peekClusters[c] = String(cString: s)
                    }
                }
            }
        }

        // Hyperlinks.
        links.removeAll(keepingCapacity: true)
        if newInfo.link_count > 0 {
            for i in 0..<newInfo.link_count {
                if let s = thurm_grid_link(client, pane, UInt16(clamping: i)) {
                    links.append(String(cString: s))
                } else {
                    links.append("")
                }
            }
        }

        // Kitty image placements.
        if newInfo.image_count > 0 {
            var placements = [thurm_image_placement](repeating: thurm_image_placement(),
                                                     count: newInfo.image_count)
            let n = placements.withUnsafeMutableBufferPointer { buf -> Int in
                thurm_grid_images(client, pane, buf.baseAddress, buf.count)
            }
            if n < placements.count { placements.removeLast(placements.count - n) }
            images = placements
        } else {
            images.removeAll()
        }

        info = newInfo
        valid = true
        return true
    }

    func cell(col: Int, row: Int) -> thurm_cell? {
        guard valid, col >= 0, row >= 0, col < cols, row < rows else { return nil }
        return cells[row * cols + col]
    }

    /// Row text (spacers skipped) plus the column of every UTF-16 unit.
    func rowText(_ row: Int) -> (String, [Int]) {
        guard valid, row >= 0, row < rows else { return ("", []) }
        var units: [UniChar] = []
        var map: [Int] = []
        let clusters = row < rowClusters.count ? rowClusters[row] : [:]
        for c in 0..<cols {
            let cell = cells[row * cols + c]
            if cell.flags & CellFlag.wideSpacer != 0 { continue }
            var text = " "
            if let cl = clusters[c] {
                text = cl
            } else if cell.ch > 32, let us = Unicode.Scalar(cell.ch) {
                text = String(Character(us))
            }
            for u in text.utf16 {
                units.append(u)
                map.append(c)
            }
        }
        return (String(utf16CodeUnits: units, count: units.count), map)
    }
}

// MARK: - Renderer

/// Everything besides the grid that affects a frame.
struct RenderParams {
    var pane: UInt64
    var shaper: FontShaper
    var padX: Int
    var padY: Int
    var viewportWidth: Int
    var viewportHeight: Int
    /// View is first responder of the key window (solid cursor); hollow otherwise.
    var focused: Bool
    /// Blink phase.
    var cursorOn: Bool
    var cursorThickness: Int
    /// Unfocused split dimming (0...1).
    var dim: Float
    /// Visual bell flash (0...1).
    var flash: Float
    var opacity: Float
    var themeBackground: UInt32
    var themeForeground: UInt32
    /// Hyperlink id (cell.link) hovered with Cmd, 0 = none.
    var hoverLink: UInt16
    /// Plain URL hovered with Cmd: row and column range (inclusive).
    var hoverSpan: (row: Int, start: Int, end: Int)?
    /// Smooth scrolling: content moved down by this many pixels (0 ..< cell height), with the
    /// peek row drawn above row 0.
    var scrollY: Float = 0
}

private struct DrawSegment {
    var start: Int
    var count: Int
    var texture: MTLTexture?
}

final class TerminalRenderer {
    let snapshot = GridSnapshot()

    private var backgrounds: [QuadInstance] = []
    private var cursorUnder: [QuadInstance] = []
    private var glyphQuads: [QuadInstance] = []
    private var decorations: [QuadInstance] = []
    private var cursorOver: [QuadInstance] = []
    private var overlay: [QuadInstance] = []
    private var imageTextures: [UInt32: MTLTexture] = [:]
    /// Set when an image was referenced but its pixels were not available yet.
    private(set) var wantsAnotherFrame = false

    private let searchMatchBg: UInt32 = 0xD7BA5A
    private let searchFocusBg: UInt32 = 0xF0883E
    private let searchFg: UInt32 = 0x1B1B1B

    func draw(layer: CAMetalLayer, params p: RenderParams) {
        guard let ctx = MetalContext.shared else { return }
        guard p.viewportWidth > 0, p.viewportHeight > 0 else { return }
        wantsAnotherFrame = false
        buildQuads(p, device: ctx.device)

        guard let drawable = layer.nextDrawable() else { return }
        let bgColor = snapshot.valid ? snapshot.info.background : p.themeBackground
        let clear = rgba(bgColor, p.opacity)
        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = drawable.texture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].storeAction = .store
        pass.colorAttachments[0].clearColor = MTLClearColor(red: Double(clear.x * clear.w),
                                                            green: Double(clear.y * clear.w),
                                                            blue: Double(clear.z * clear.w),
                                                            alpha: Double(clear.w))
        guard let commands = ctx.queue.makeCommandBuffer(),
              let encoder = commands.makeRenderCommandEncoder(descriptor: pass)
        else { return }

        // Assemble all quads into one buffer, in paint order.
        var all: [QuadInstance] = []
        all.reserveCapacity(backgrounds.count + cursorUnder.count + glyphQuads.count + decorations.count
                            + cursorOver.count + overlay.count + snapshot.images.count)
        var segments: [DrawSegment] = []
        func append(_ quads: [QuadInstance], texture: MTLTexture? = nil) {
            guard !quads.isEmpty else { return }
            segments.append(DrawSegment(start: all.count, count: quads.count, texture: texture))
            all.append(contentsOf: quads)
        }
        append(backgrounds)
        append(cursorUnder)
        let (below, above) = imageQuads(p, device: ctx.device)
        for (quad, tex) in below { append([quad], texture: tex) }
        append(glyphQuads)
        append(decorations)
        for (quad, tex) in above { append([quad], texture: tex) }
        append(cursorOver)
        let overlayStart = segments.count
        append(overlay)

        encoder.setRenderPipelineState(ctx.pipeline)
        var uniforms = SIMD2<Float>(Float(p.viewportWidth), Float(p.viewportHeight))
        encoder.setVertexBytes(&uniforms, length: MemoryLayout<SIMD2<Float>>.stride, index: 1)
        encoder.setFragmentTexture(p.shaper.glyphs.gray.texture, index: 0)
        encoder.setFragmentTexture(p.shaper.glyphs.color.texture, index: 1)
        encoder.setFragmentTexture(ctx.emptyTexture, index: 2)

        if !all.isEmpty {
            let stride = MemoryLayout<QuadInstance>.stride
            let buffer: MTLBuffer? = all.withUnsafeBytes { raw -> MTLBuffer? in
                guard let base = raw.baseAddress else { return nil }
                return ctx.device.makeBuffer(bytes: base, length: raw.count, options: [.storageModeShared])
            }
            if let buffer = buffer {
                encoder.setVertexBuffer(buffer, offset: 0, index: 0)
                // While smooth scrolling, clip the shifted grid to the padded text area.
                let full = MTLScissorRect(x: 0, y: 0, width: p.viewportWidth, height: p.viewportHeight)
                var clip = full
                if p.scrollY > 0, snapshot.valid {
                    let top = min(p.padY, p.viewportHeight)
                    let height = min(snapshot.rows * p.shaper.cellHeight, p.viewportHeight - top)
                    clip = MTLScissorRect(x: 0, y: top, width: p.viewportWidth, height: max(0, height))
                }
                for (i, seg) in segments.enumerated() {
                    encoder.setScissorRect(i >= overlayStart ? full : clip)
                    encoder.setVertexBufferOffset(seg.start * stride, index: 0)
                    encoder.setFragmentTexture(seg.texture ?? ctx.emptyTexture, index: 2)
                    encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4,
                                           instanceCount: seg.count)
                }
            }
        }
        encoder.endEncoding()
        commands.present(drawable)
        commands.commit()
        ctx.lastCommitted = commands
    }

    // MARK: Quad building

    private func buildQuads(_ p: RenderParams, device: MTLDevice) {
        backgrounds.removeAll(keepingCapacity: true)
        cursorUnder.removeAll(keepingCapacity: true)
        glyphQuads.removeAll(keepingCapacity: true)
        decorations.removeAll(keepingCapacity: true)
        cursorOver.removeAll(keepingCapacity: true)
        overlay.removeAll(keepingCapacity: true)

        let vw = Float(p.viewportWidth)
        let vh = Float(p.viewportHeight)
        let defaultBg = snapshot.valid ? snapshot.info.background : p.themeBackground

        if snapshot.valid {
            buildGrid(p)
        }

        // Unfocused split dimming and bell flash.
        if p.dim > 0.001 {
            overlay.append(QuadInstance(rect: SIMD4<Float>(0, 0, vw, vh), color: rgba(defaultBg, p.dim),
                                        kind: QuadKind.solid))
        }
        if p.flash > 0.001 {
            let flashColor: UInt32 = snapshot.valid ? snapshot.info.foreground : p.themeForeground
            overlay.append(QuadInstance(rect: SIMD4<Float>(0, 0, vw, vh), color: rgba(flashColor, 0.22 * p.flash),
                                        kind: QuadKind.solid))
        }
    }

    private func backgroundColor(_ cell: thurm_cell, _ info: thurm_grid_info) -> UInt32? {
        let f = cell.flags
        if f & CellFlag.searchFocus != 0 { return searchFocusBg }
        if f & CellFlag.searchMatch != 0 { return searchMatchBg }
        if f & CellFlag.selected != 0 { return info.selection_bg }
        if f & CellFlag.defaultBackground != 0 { return nil }
        return cell.bg
    }

    private func foregroundColor(_ cell: thurm_cell, _ info: thurm_grid_info) -> UInt32 {
        let f = cell.flags
        if f & (CellFlag.searchFocus | CellFlag.searchMatch) != 0 { return searchFg }
        if f & CellFlag.selected != 0 { return info.selection_fg }
        return cell.fg
    }

    private func buildGrid(_ p: RenderParams) {
        let info = snapshot.info
        let cols = snapshot.cols
        let rows = snapshot.rows
        guard cols > 0, rows > 0, snapshot.cells.count >= cols * rows else { return }
        let shaper = p.shaper
        let cw = shaper.cellWidth
        let ch = shaper.cellHeight
        let fcw = Float(cw)
        let fch = Float(ch)
        let padX = Float(p.padX)
        let padY = Float(p.padY)

        // Cursor geometry.
        let cursorRow = Int(info.cursor_row)
        let cursorCol = Int(info.cursor_col)
        let cursorShown = (info.modes & TermMode.showCursor) != 0
            && info.cursor_shape != CursorShapeCode.hidden
            && info.display_offset == 0
            && cursorRow < rows && cursorCol < cols
            && (p.cursorOn || !p.focused)
        var shape = info.cursor_shape
        if !p.focused { shape = CursorShapeCode.hollowBlock }
        let blockCursor = cursorShown && shape == CursorShapeCode.block

        let scrollY = p.scrollY
        let firstRow = scrollY > 0 && snapshot.peekCells.count == cols ? -1 : 0
        snapshot.cells.withUnsafeBufferPointer { allCells in
          snapshot.peekCells.withUnsafeBufferPointer { peekCells in
            for r in firstRow..<rows {
                let rowCells = r < 0 ? peekCells
                    : UnsafeBufferPointer(rebasing: allCells[(r * cols)..<((r + 1) * cols)])
                let y = padY + Float(r) * fch + scrollY

                // Backgrounds, merging horizontal runs of the same color.
                var runStart = -1
                var runColor: UInt32 = 0
                for c in 0...cols {
                    let color: UInt32? = c < cols ? backgroundColor(rowCells[c], info) : nil
                    if let color = color, runStart >= 0, color == runColor { continue }
                    if runStart >= 0 {
                        backgrounds.append(QuadInstance(
                            rect: SIMD4<Float>(padX + Float(runStart) * fcw, y, Float(c - runStart) * fcw, fch),
                            color: rgba(runColor), kind: QuadKind.solid))
                        runStart = -1
                    }
                    if let color = color {
                        runStart = c
                        runColor = color
                    }
                }

                // Text.
                let clusters = r < 0 ? snapshot.peekClusters
                    : (r < snapshot.rowClusters.count ? snapshot.rowClusters[r] : [:])
                let shaped = shaper.shapeRow(rowCells, clusters: clusters)
                let baselineY = y + Float(shaper.baseline)
                for g in shaped {
                    guard g.col < cols else { continue }
                    let cell = rowCells[g.col]
                    if cell.flags & CellFlag.hidden != 0 { continue }
                    var fg = foregroundColor(cell, info)
                    if blockCursor && r == cursorRow && g.col == cursorCol {
                        fg = (info.cursor_text_color & kNoColor) != 0 ? info.background : info.cursor_text_color
                    }
                    let entry = shaper.glyphs.entry(for: GlyphKey(font: g.font, glyph: g.glyph, span: g.span),
                                                    font: shaper.font(g.font), isColor: g.color,
                                                    cell: CellBox(width: cw, height: ch, baseline: shaper.baseline))
                    if entry.isEmpty { continue }
                    let gx = (padX + Float(g.col) * fcw + g.x.rounded() + entry.left).rounded()
                    let gy = (baselineY - g.y.rounded() - entry.top).rounded()
                    glyphQuads.append(QuadInstance(
                        rect: SIMD4<Float>(gx, gy, entry.w, entry.h),
                        uv: SIMD4<Float>(entry.x, entry.y, entry.x + entry.w, entry.y + entry.h),
                        color: entry.color ? SIMD4<Float>(1, 1, 1, 1) : rgba(fg),
                        kind: entry.color ? QuadKind.colorGlyph : QuadKind.grayGlyph))
                }

                // Procedural box drawing / block elements, and decorations.
                let thickness = Float(shaper.lineThickness)
                for c in 0..<cols {
                    let cell = rowCells[c]
                    let f = cell.flags
                    if f & CellFlag.wideSpacer != 0 { continue }
                    let x = padX + Float(c) * fcw
                    var fg = foregroundColor(cell, info)
                    if blockCursor && r == cursorRow && c == cursorCol {
                        fg = (info.cursor_text_color & kNoColor) != 0 ? info.background : info.cursor_text_color
                    }
                    if f & CellFlag.hidden == 0 && BoxDrawing.isProcedural(cell.ch) && clusters[c] == nil {
                        for b in shaper.boxRects(for: cell.ch) {
                            glyphQuads.append(QuadInstance(
                                rect: SIMD4<Float>(x + Float(b.x), y + Float(b.y), Float(b.w), Float(b.h)),
                                uv: b.arc ?? SIMD4<Float>(0, 0, 0, 0),
                                color: rgba(fg, b.alpha), kind: b.arc == nil ? QuadKind.solid : QuadKind.arc))
                        }
                    }

                    let width = (f & CellFlag.wide) != 0 ? fcw * 2 : fcw
                    var style = 0
                    if f & CellFlag.undercurl != 0 { style = 3 }
                    else if f & CellFlag.doubleUnderline != 0 { style = 2 }
                    else if f & CellFlag.dottedUnderline != 0 { style = 4 }
                    else if f & CellFlag.dashedUnderline != 0 { style = 5 }
                    else if f & CellFlag.underline != 0 { style = 1 }
                    if style == 0 {
                        if p.hoverLink != 0 && cell.link == p.hoverLink { style = 1 }
                        if let span = p.hoverSpan, span.row == r, c >= span.start, c <= span.end { style = 1 }
                    }
                    if style != 0 {
                        let ulColor = (cell.ul & kNoColor) != 0 ? fg : cell.ul
                        let color = rgba(ulColor)
                        let uy = min(baselineY + Float(shaper.underlineOffset), y + fch - thickness)
                        switch style {
                        case 1:
                            decorations.append(QuadInstance(rect: SIMD4<Float>(x, uy, width, thickness),
                                                            color: color, kind: QuadKind.solid))
                        case 2:
                            let second = min(uy + thickness * 2, y + fch - thickness)
                            let first = max(y, second - thickness * 2)
                            decorations.append(QuadInstance(rect: SIMD4<Float>(x, first, width, thickness),
                                                            color: color, kind: QuadKind.solid))
                            decorations.append(QuadInstance(rect: SIMD4<Float>(x, second, width, thickness),
                                                            color: color, kind: QuadKind.solid))
                        case 3:
                            let h = max(thickness * 3, min(fch * 0.2, thickness * 5))
                            let top = min(uy - h / 2 + thickness / 2, y + fch - h)
                            decorations.append(QuadInstance(rect: SIMD4<Float>(x, top, width, h),
                                                            uv: SIMD4<Float>(thickness, fcw, x, 0),
                                                            color: color, kind: QuadKind.undercurl))
                        case 4:
                            decorations.append(QuadInstance(rect: SIMD4<Float>(x, uy, width, thickness),
                                                            uv: SIMD4<Float>(thickness, 0, x, 0),
                                                            color: color, kind: QuadKind.dotted))
                        default:
                            decorations.append(QuadInstance(rect: SIMD4<Float>(x, uy, width, thickness),
                                                            uv: SIMD4<Float>(thickness, max(2, fcw * 0.4), x, 0),
                                                            color: color, kind: QuadKind.dashed))
                        }
                    }
                    if f & CellFlag.strikeout != 0 {
                        let sy = baselineY - Float(shaper.strikeOffset) - thickness / 2
                        decorations.append(QuadInstance(rect: SIMD4<Float>(x, sy.rounded(), width, thickness),
                                                        color: rgba(fg), kind: QuadKind.solid))
                    }
                }
            }
          }
        }

        // Cursor.
        if cursorShown {
            let cell = snapshot.cell(col: cursorCol, row: cursorRow)
            let fallback = cell?.fg ?? info.foreground
            let cursorRGB = (info.cursor_color & kNoColor) != 0 ? fallback : info.cursor_color
            let color = rgba(cursorRGB)
            let x = padX + Float(cursorCol) * fcw
            let y = padY + Float(cursorRow) * fch + scrollY
            let w = info.cursor_wide ? fcw * 2 : fcw
            let t = Float(max(1, p.cursorThickness))
            switch shape {
            case CursorShapeCode.block:
                cursorUnder.append(QuadInstance(rect: SIMD4<Float>(x, y, w, fch), color: color, kind: QuadKind.solid))
            case CursorShapeCode.beam:
                cursorOver.append(QuadInstance(rect: SIMD4<Float>(x, y, t, fch), color: color, kind: QuadKind.solid))
            case CursorShapeCode.underline:
                cursorOver.append(QuadInstance(rect: SIMD4<Float>(x, y + fch - t, w, t), color: color,
                                               kind: QuadKind.solid))
            default:
                let b = max(1, (t / 2).rounded())
                cursorOver.append(QuadInstance(rect: SIMD4<Float>(x, y, w, b), color: color, kind: QuadKind.solid))
                cursorOver.append(QuadInstance(rect: SIMD4<Float>(x, y + fch - b, w, b), color: color,
                                               kind: QuadKind.solid))
                cursorOver.append(QuadInstance(rect: SIMD4<Float>(x, y, b, fch), color: color, kind: QuadKind.solid))
                cursorOver.append(QuadInstance(rect: SIMD4<Float>(x + w - b, y, b, fch), color: color,
                                               kind: QuadKind.solid))
            }
        }
    }

    // MARK: Images

    private func imageQuads(_ p: RenderParams, device: MTLDevice)
        -> ([(QuadInstance, MTLTexture)], [(QuadInstance, MTLTexture)]) {
        var below: [(QuadInstance, MTLTexture)] = []
        var above: [(QuadInstance, MTLTexture)] = []
        guard snapshot.valid, !snapshot.images.isEmpty else {
            imageTextures.removeAll()
            return (below, above)
        }
        let cw = Float(p.shaper.cellWidth)
        let ch = Float(p.shaper.cellHeight)
        var referenced = Set<UInt32>()
        for placement in snapshot.images {
            referenced.insert(placement.image)
            guard let tex = texture(for: placement.image, pane: p.pane, device: device) else {
                wantsAnotherFrame = true
                continue
            }
            let srcW = placement.src_w > 0 ? Float(placement.src_w) : Float(tex.width)
            let srcH = placement.src_h > 0 ? Float(placement.src_h) : Float(tex.height)
            let dstW = placement.dst_w > 0 ? Float(placement.dst_w)
                : placement.cols > 0 ? Float(placement.cols) * cw : srcW
            let dstH = placement.dst_h > 0 ? Float(placement.dst_h)
                : placement.rows > 0 ? Float(placement.rows) * ch : srcH
            let x = Float(p.padX) + Float(placement.col) * cw + Float(placement.x_offset)
            let y = Float(p.padY) + Float(placement.row) * ch + Float(placement.y_offset) + p.scrollY
            let u0 = Float(placement.src_x)
            let v0 = Float(placement.src_y)
            let quad = QuadInstance(rect: SIMD4<Float>(x, y, dstW, dstH),
                                    uv: SIMD4<Float>(u0, v0, u0 + srcW, v0 + srcH),
                                    color: SIMD4<Float>(1, 1, 1, 1), kind: QuadKind.image)
            if placement.z < 0 {
                below.append((quad, tex))
            } else {
                above.append((quad, tex))
            }
        }
        // Evict textures of images that are no longer shown.
        for id in Array(imageTextures.keys) where !referenced.contains(id) {
            imageTextures.removeValue(forKey: id)
        }
        return (below, above)
    }

    private func texture(for image: UInt32, pane: UInt64, device: MTLDevice) -> MTLTexture? {
        if let tex = imageTextures[image] { return tex }
        guard let client = Core.shared.client else { return nil }
        var width: UInt32 = 0
        var height: UInt32 = 0
        var pixels: UnsafePointer<UInt8>? = nil
        guard thurm_image_lock(client, pane, image, &width, &height, &pixels) else { return nil }
        defer { thurm_image_unlock(client, pane, image) }
        guard let src = pixels, width > 0, height > 0, width <= 16384, height <= 16384 else { return nil }
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .rgba8Unorm, width: Int(width),
                                                            height: Int(height), mipmapped: false)
        desc.usage = [.shaderRead]
        guard let tex = device.makeTexture(descriptor: desc) else { return nil }
        tex.replace(region: MTLRegionMake2D(0, 0, Int(width), Int(height)), mipmapLevel: 0,
                    withBytes: src, bytesPerRow: Int(width) * 4)
        imageTextures[image] = tex
        return tex
    }

    /// Forget cached image textures (e.g. after reconnecting).
    func resetImages() {
        imageTextures.removeAll()
    }
}
