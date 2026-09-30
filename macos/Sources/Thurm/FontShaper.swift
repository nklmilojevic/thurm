import AppKit
import CoreText
import Metal
import CThurm

/// One positioned glyph of a shaped row.
struct ShapedGlyph {
    /// Grid column of the glyph's cluster.
    var col: Int
    /// Pixel offset from the column's left edge (non zero for ligature pieces and marks).
    var x: Float
    /// Pixel offset from the baseline, positive upwards.
    var y: Float
    var font: Int
    var glyph: CGGlyph
    var color: Bool
    /// Nerd Font icon from the symbols fallback: fit into this many cells (0 = draw as is).
    var span: UInt8 = 0
}

/// Cache key for shaped rows: per cell scalar + style bits, plus grapheme clusters.
struct ShapeKey: Hashable {
    var cells: [UInt32]
    var clusters: [Int: String]
}

/// Fonts, cell metrics, shaping (CoreText, with ligatures and fallback) and the glyph atlas for
/// one backing scale factor. All metrics are in device pixels.
final class FontShaper {
    // MARK: Shared instances (one per backing scale)

    private static var instances: [CGFloat: FontShaper] = [:]

    static func shared(scale: CGFloat) -> FontShaper? {
        let s = max(1, scale)
        if let existing = instances[s] { return existing }
        guard let ctx = MetalContext.shared else { return nil }
        let manager = SessionManager.shared
        guard let shaper = FontShaper(config: manager.config, fontSize: manager.effectiveFontSize,
                                      scale: s, device: ctx.device)
        else { return nil }
        instances[s] = shaper
        return shaper
    }

    /// Drops every instance (font or config change); views fetch new ones.
    static func invalidateAll() {
        instances.removeAll()
    }

    /// Family of the bundled icon font (Resources/fonts/SymbolsNerdFontMono-Regular.ttf).
    static let symbolsFamily = "Symbols Nerd Font Mono"

    /// Makes the fonts shipped in the app bundle available to this process.
    static func registerBundledFonts() {
        guard let dir = Bundle.main.resourceURL?.appendingPathComponent("fonts"),
              let files = try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
        else { return }
        for url in files where ["ttf", "otf"].contains(url.pathExtension.lowercased()) {
            var error: Unmanaged<CFError>?
            if !CTFontManagerRegisterFontsForURL(url as CFURL, .process, &error) {
                tlog("could not register \(url.lastPathComponent): \(String(describing: error?.takeRetainedValue()))")
            }
        }
    }

    // MARK: Properties

    let scale: CGFloat
    /// regular, bold, italic, bold-italic.
    let styleFonts: [CTFont]
    let cellWidth: Int
    let cellHeight: Int
    /// Distance from the cell top to the baseline.
    let baseline: Int
    /// Underline offset below the baseline and thickness.
    let underlineOffset: Int
    let lineThickness: Int
    /// Strikeout offset above the baseline.
    let strikeOffset: Int
    let glyphs: GlyphCache

    private var fontList: [CTFont] = []
    private var fontIndex: [String: Int] = [:]
    /// Font ids of the Nerd Font symbols fallback (their glyphs are fitted to the cell).
    private var symbolFonts: Set<Int> = []
    /// Shaped rows, two generations: when the current one fills up (by rows or by bytes) it
    /// becomes the old one, and rows still in use move back on their next hit (an LRU without
    /// the bookkeeping).
    private var rowCache: [ShapeKey: [ShapedGlyph]] = [:]
    private var oldRowCache: [ShapeKey: [ShapedGlyph]] = [:]
    private var rowCacheBytes = 0
    private static let rowCacheGeneration = 4096
    private static let rowCacheGenerationBytes = 6 << 20
    private var boxCache: [UInt32: [BoxRect]] = [:]
    private let fontAttributeKey = NSAttributedString.Key(rawValue: kCTFontAttributeName as String)

    init?(config: AppConfig, fontSize: CGFloat, scale: CGFloat, device: MTLDevice) {
        guard let cache = GlyphCache(device: device,
                                     thicken: config.fontThicken ? config.thickenStrength : nil)
        else { return nil }
        self.glyphs = cache
        self.scale = scale
        let pixelSize = max(4, fontSize) * scale

        let (regular, descriptor) = FontShaper.makeBaseFont(config: config, pixelSize: pixelSize)
        let extras = FontShaper.extraAttributes(config: config)
        func styled(_ name: String?, _ traits: CTFontSymbolicTraits) -> CTFont {
            if let name, let font = FontShaper.resolve(name, size: pixelSize, extra: extras) { return font }
            return FontShaper.variant(of: regular, descriptor: descriptor, traits: traits, size: pixelSize)
        }
        let bold = styled(config.fontFamilyBold, .traitBold)
        let italic = styled(config.fontFamilyItalic, .traitItalic)
        let boldItalic = styled(config.fontFamilyBoldItalic ?? config.fontFamilyBold, [.traitBold, .traitItalic])
        styleFonts = [regular, bold, italic, boldItalic]

        // Cell metrics from the primary font.
        let ascent = CTFontGetAscent(regular)
        let descent = CTFontGetDescent(regular)
        let leading = CTFontGetLeading(regular)
        var chars: [UniChar] = [0x4D] // "M"
        var mGlyph: [CGGlyph] = [0]
        var advance: CGFloat = pixelSize * 0.6
        if CTFontGetGlyphsForCharacters(regular, &chars, &mGlyph, 1) {
            var size = CGSize.zero
            advance = CGFloat(CTFontGetAdvancesForGlyphs(regular, .horizontal, &mGlyph, &size, 1))
        }
        cellWidth = max(1, Int((advance + config.letterSpacing * scale).rounded()))
        let natural = ascent + descent + leading
        cellHeight = max(1, Int(ceil(ceil(natural) * max(0.5, config.lineHeight))))
        let topPad = (CGFloat(cellHeight) - natural) / 2
        baseline = max(1, min(cellHeight - 1, Int((topPad + leading / 2 + ascent).rounded())))

        let thickness = max(1, Int(CTFontGetUnderlineThickness(regular).rounded()))
        lineThickness = thickness
        var ulOffset = max(1, Int((-CTFontGetUnderlinePosition(regular)).rounded()))
        if baseline + ulOffset + thickness > cellHeight {
            ulOffset = max(0, cellHeight - baseline - thickness)
        }
        underlineOffset = ulOffset
        strikeOffset = max(1, Int((CTFontGetXHeight(regular) / 2).rounded()))
    }

    // MARK: Font construction

    /// OpenType feature settings: "calt" / "+calt" enable, "-calt" disable, "cv01=2" value.
    static func featureSettings(_ list: [String]) -> [[String: Any]] {
        var out: [[String: Any]] = []
        for raw in list {
            var tag = raw.trimmingCharacters(in: .whitespaces)
            var value = 1
            if tag.hasPrefix("-") {
                value = 0
                tag.removeFirst()
            } else if tag.hasPrefix("+") {
                tag.removeFirst()
            }
            if let eq = tag.firstIndex(of: "=") {
                value = Int(tag[tag.index(after: eq)...]) ?? 1
                tag = String(tag[..<eq])
            }
            guard tag.count == 4 else { continue }
            out.append([
                kCTFontOpenTypeFeatureTag as String: tag,
                kCTFontOpenTypeFeatureValue as String: value,
            ])
        }
        return out
    }

    /// OpenType features and the fallback cascade (user fallbacks, then the bundled icons).
    private static func extraAttributes(config: AppConfig) -> [String: Any] {
        var attrs: [String: Any] = [:]
        let features = featureSettings(config.fontFeatures)
        if !features.isEmpty { attrs[kCTFontFeatureSettingsAttribute as String] = features }
        var families = config.fontFallback
        // Icons before the system cascade, which has no Nerd Font glyphs.
        if config.nerdFontSymbols { families.append(symbolsFamily) }
        let cascade: [CTFontDescriptor] = families.map { family in
            CTFontDescriptorCreateWithAttributes([kCTFontFamilyNameAttribute as String: family] as CFDictionary)
        }
        if !cascade.isEmpty { attrs[kCTFontCascadeListAttribute as String] = cascade }
        return attrs
    }

    /// A family ("TX02 Nerd Font") or a single face by its full or PostScript name
    /// ("TX-02 Retina ExtraCondensed"), like Ghostty's font-family. Nil when not installed.
    private static func resolve(_ name: String, size: CGFloat, extra: [String: Any]) -> CTFont? {
        return resolveWithDescriptor(name, size: size, extra: extra)?.0
    }

    private static func resolveWithDescriptor(_ name: String, size: CGFloat,
                                              extra: [String: Any]) -> (CTFont, CTFontDescriptor)? {
        let same = { (a: String, b: String) in a.caseInsensitiveCompare(b) == .orderedSame }
        var byFamily = extra
        byFamily[kCTFontFamilyNameAttribute as String] = name
        let familyDesc = CTFontDescriptorCreateWithAttributes(byFamily as CFDictionary)
        let familyFont = CTFontCreateWithFontDescriptor(familyDesc, size, nil)
        if same(CTFontCopyFamilyName(familyFont) as String, name) { return (familyFont, familyDesc) }
        var byName = extra
        byName[kCTFontNameAttribute as String] = name
        let nameDesc = CTFontDescriptorCreateWithAttributes(byName as CFDictionary)
        let nameFont = CTFontCreateWithFontDescriptor(nameDesc, size, nil)
        let names = [CTFontCopyFullName(nameFont), CTFontCopyPostScriptName(nameFont),
                     CTFontCopyDisplayName(nameFont)].map { $0 as String }
        if names.contains(where: { same($0, name) }) { return (nameFont, nameDesc) }
        return nil
    }

    /// The terminal font as an NSFont (point size), for UI that should match the grid.
    static func terminalFont(config: AppConfig, size: CGFloat) -> NSFont {
        let (font, _) = makeBaseFont(config: config, pixelSize: size)
        return font as NSFont
    }

    /// Resolves the configured font (then the fallbacks, then Menlo), with features and the
    /// fallback cascade list applied.
    private static func makeBaseFont(config: AppConfig, pixelSize: CGFloat) -> (CTFont, CTFontDescriptor) {
        let extra = extraAttributes(config: config)
        var candidates = [config.fontFamily]
        candidates.append(contentsOf: config.fontFallback)
        candidates.append("Menlo")
        for name in candidates where !name.isEmpty {
            if let found = resolveWithDescriptor(name, size: pixelSize, extra: extra) { return found }
            tlog("font \"\(name)\" not available")
        }
        let system = NSFont.monospacedSystemFont(ofSize: pixelSize, weight: .regular) as CTFont
        return (system, CTFontCopyFontDescriptor(system))
    }

    private static func variant(of base: CTFont, descriptor: CTFontDescriptor,
                                traits: CTFontSymbolicTraits, size: CGFloat) -> CTFont {
        if let desc = CTFontDescriptorCreateCopyWithSymbolicTraits(descriptor, traits, traits) {
            let font = CTFontCreateWithFontDescriptor(desc, size, nil)
            if CTFontGetSymbolicTraits(font).contains(traits) { return font }
        }
        if let font = CTFontCreateCopyWithSymbolicTraits(base, size, nil, traits, traits) {
            return font
        }
        return base
    }

    // MARK: Font registry

    func fontId(_ font: CTFont) -> Int {
        let name = CTFontCopyPostScriptName(font) as String
        let key = "\(name)|\(CTFontGetSize(font))"
        if let id = fontIndex[key] { return id }
        fontList.append(font)
        let id = fontList.count - 1
        fontIndex[key] = id
        if (CTFontCopyFamilyName(font) as String) == FontShaper.symbolsFamily { symbolFonts.insert(id) }
        return id
    }

    func font(_ id: Int) -> CTFont {
        (id >= 0 && id < fontList.count) ? fontList[id] : styleFonts[0]
    }

    // MARK: Box drawing

    func boxRects(for ch: UInt32) -> [BoxRect] {
        if let cached = boxCache[ch] { return cached }
        let rects = BoxDrawing.rects(for: ch, width: cellWidth, height: cellHeight, light: lineThickness)
        boxCache[ch] = rects
        return rects
    }

    // MARK: Shaping

    /// The scalar that takes part in shaping for `cell`; 32 for anything drawn as blank
    /// (empty, hidden, control characters, procedural box drawing).
    static func shapedScalar(_ cell: thurm_cell) -> UInt32 {
        let ch = cell.ch
        if ch <= 32 || ch == 0x7F || cell.flags & CellFlag.hidden != 0 { return 32 }
        if ch > 0x10FFFF || (ch >= 0xD800 && ch <= 0xDFFF) { return 32 }
        if BoxDrawing.isProcedural(ch) { return 32 }
        return ch
    }

    /// Shapes one grid row. Cached by content, so unchanged rows are not reshaped.
    func shapeRow(_ cells: UnsafeBufferPointer<thurm_cell>, clusters: [Int: String]) -> [ShapedGlyph] {
        let cols = cells.count
        var key: [UInt32] = []
        key.reserveCapacity(cols)
        var anyText = !clusters.isEmpty
        for c in 0..<cols {
            let cell = cells[c]
            if cell.flags & CellFlag.wideSpacer != 0 {
                key.append(0xFFFF_FFFF)
                continue
            }
            let scalar = FontShaper.shapedScalar(cell)
            if scalar != 32 { anyText = true }
            let style = UInt32(cell.flags & 3)
            let wide: UInt32 = (cell.flags & CellFlag.wide) != 0 ? (1 << 23) : 0
            key.append(scalar | (style << 21) | wide)
        }
        if !anyText { return [] }
        let shapeKey = ShapeKey(cells: key, clusters: clusters)
        if let hit = rowCache[shapeKey] { return hit }
        if let hit = oldRowCache.removeValue(forKey: shapeKey) {
            storeRow(shapeKey, hit)
            return hit
        }

        var out: [ShapedGlyph] = []
        var units: [UniChar] = []
        var map: [Int] = []
        var runStyle = -1
        var blankAfter = [Bool](repeating: false, count: cols)
        for c in 0..<max(0, cols - 1) {
            let next = cells[c + 1]
            blankAfter[c] = next.flags & CellFlag.wideSpacer == 0 && FontShaper.shapedScalar(next) == 32
                && clusters[c + 1] == nil
        }
        units.reserveCapacity(cols + 8)
        map.reserveCapacity(cols + 8)

        for c in 0..<cols {
            let cell = cells[c]
            if cell.flags & CellFlag.wideSpacer != 0 { continue }
            let scalar = FontShaper.shapedScalar(cell)
            let cluster = clusters[c]
            let blank = scalar == 32 && cluster == nil
            if blank {
                // Spaces never break a style run.
                units.append(32)
                map.append(c)
                continue
            }
            let style = Int(cell.flags & 3)
            if style != runStyle {
                if runStyle >= 0 { shapeRun(units, map, style: runStyle, blankAfter: blankAfter, into: &out) }
                units.removeAll(keepingCapacity: true)
                map.removeAll(keepingCapacity: true)
                runStyle = style
            }
            if let cluster = cluster {
                for u in cluster.utf16 {
                    units.append(u)
                    map.append(c)
                }
            } else if let us = Unicode.Scalar(scalar) {
                for u in String(Character(us)).utf16 {
                    units.append(u)
                    map.append(c)
                }
            }
        }
        if runStyle >= 0 { shapeRun(units, map, style: runStyle, blankAfter: blankAfter, into: &out) }

        storeRow(shapeKey, out)
        return out
    }

    private func storeRow(_ key: ShapeKey, _ glyphs: [ShapedGlyph]) {
        // Rough size: key cells, cluster strings, shaped glyphs and per-entry overhead.
        let bytes = key.cells.count * 4 + key.clusters.count * 32
            + glyphs.count * MemoryLayout<ShapedGlyph>.stride + 96
        if rowCache.count >= FontShaper.rowCacheGeneration
            || rowCacheBytes + bytes > FontShaper.rowCacheGenerationBytes {
            oldRowCache = rowCache
            rowCache = [:]
            rowCacheBytes = 0
        }
        rowCache[key] = glyphs
        rowCacheBytes += bytes
    }

    /// Shapes one run of identically styled text. `map[i]` is the column of UTF-16 unit `i`.
    /// `blankAfter[c]`: the cell after column `c` is empty (a Nerd Font icon may use it).
    private func shapeRun(_ units: [UniChar], _ map: [Int], style: Int, blankAfter: [Bool],
                          into out: inout [ShapedGlyph]) {
        guard !units.isEmpty, units.contains(where: { $0 != 32 }) else { return }
        let baseFont = styleFonts[max(0, min(3, style))]
        let string = String(utf16CodeUnits: units, count: units.count)
        let attributed = NSAttributedString(string: string, attributes: [fontAttributeKey: baseFont])
        let line = CTLineCreateWithAttributedString(attributed as CFAttributedString)
        let runs = CTLineGetGlyphRuns(line) as! [CTRun]

        // Pen x of the first glyph of each column's cluster, so that every glyph is positioned
        // relative to its own cell and the grid never drifts.
        var clusterStart: [Int: CGFloat] = [:]
        for run in runs {
            let count = CTRunGetGlyphCount(run)
            if count <= 0 { continue }
            var runFont = baseFont
            let attrs = CTRunGetAttributes(run) as NSDictionary
            if let value = attrs[kCTFontAttributeName as String] {
                runFont = value as! CTFont
            }
            let fid = fontId(runFont)
            let isColor = CTFontGetSymbolicTraits(runFont).contains(.traitColorGlyphs)

            var glyphIds = [CGGlyph](repeating: 0, count: count)
            var positions = [CGPoint](repeating: .zero, count: count)
            var indices = [CFIndex](repeating: 0, count: count)
            let all = CFRange(location: 0, length: 0)
            CTRunGetGlyphs(run, all, &glyphIds)
            CTRunGetPositions(run, all, &positions)
            CTRunGetStringIndices(run, all, &indices)

            for i in 0..<count {
                let si = indices[i]
                guard si >= 0, si < map.count else { continue }
                if units[si] == 32 { continue }
                let col = map[si]
                let p = positions[i]
                let startX: CGFloat
                if let s = clusterStart[col] {
                    startX = s
                } else {
                    clusterStart[col] = p.x
                    startX = p.x
                }
                // Icons are fitted to their cell (two when the next one is empty), like Ghostty.
                let span: UInt8 = symbolFonts.contains(fid)
                    ? (col < blankAfter.count && blankAfter[col] ? 2 : 1) : 0
                out.append(ShapedGlyph(col: col,
                                       x: span > 0 ? 0 : Float(p.x - startX),
                                       y: span > 0 ? 0 : Float(p.y),
                                       font: fid,
                                       glyph: glyphIds[i],
                                       color: isColor,
                                       span: span))
            }
        }
    }
}
