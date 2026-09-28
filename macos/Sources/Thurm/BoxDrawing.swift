import Foundation

/// A solid rectangle inside a cell, in pixels relative to the cell's top-left corner.
struct BoxRect {
    var x: Int
    var y: Int
    var w: Int
    var h: Int
    var alpha: Float
    /// Non-nil: a quarter-circle stroke inside this rect (rounded corners), as
    /// (radius, thickness, center x, center y) in pixels relative to the rect.
    var arc: SIMD4<Float>? = nil
}

/// Procedural rendering of box drawing (U+2500–U+257F) and block elements (U+2580–U+259F),
/// so that lines connect seamlessly across cells regardless of the font.
enum BoxDrawing {
    /// Arm weights per box drawing character, as four digits "URDL":
    /// 0 = none, 1 = light, 2 = heavy, 3 = double. "-" = leave it to the font
    /// (dashed lines and diagonals).
    private static let table: [String] = """
    0101 0202 1010 2020 - - - - - - - - \
    0110 0210 0120 0220 0011 0012 0021 0022 1100 1200 2100 2200 1001 1002 2001 2002 \
    1110 1210 2110 1120 2120 2210 1220 2220 1011 1012 2011 1021 2021 2012 1022 2022 \
    0111 0112 0211 0212 0121 0122 0221 0222 1101 1102 1201 1202 2101 2102 2201 2202 \
    1111 1112 1211 1212 2111 1121 2121 2112 2211 1122 1221 2212 1222 2122 2221 2222 \
    - - - - \
    0303 3030 0310 0130 0330 0013 0031 0033 1300 3100 3300 1003 3001 3003 1310 3130 \
    3330 1013 3031 3033 0313 0131 0333 1303 3101 3303 1313 3131 3333 \
    0110 0011 1001 1100 - - - \
    0001 1000 0100 0010 0002 2000 0200 0020 0201 1020 0102 2010
    """.split(separator: " ").map { String($0) }

    /// Decoded arms (up, right, down, left) or nil.
    private static let arms: [(Int, Int, Int, Int)?] = table.map { entry in
        let digits = entry.compactMap { $0.wholeNumberValue }
        guard digits.count == 4 else { return nil }
        return (digits[0], digits[1], digits[2], digits[3])
    }

    /// Dashed lines: (horizontal, heavy, dashes).
    private static func dashed(_ ch: UInt32) -> (Bool, Bool, Int)? {
        switch ch {
        case 0x2504: return (true, false, 3)
        case 0x2505: return (true, true, 3)
        case 0x2506: return (false, false, 3)
        case 0x2507: return (false, true, 3)
        case 0x2508: return (true, false, 4)
        case 0x2509: return (true, true, 4)
        case 0x250A: return (false, false, 4)
        case 0x250B: return (false, true, 4)
        case 0x254C: return (true, false, 2)
        case 0x254D: return (true, true, 2)
        case 0x254E: return (false, false, 2)
        case 0x254F: return (false, true, 2)
        default: return nil
        }
    }

    /// True when `ch` is drawn procedurally (and must therefore be excluded from shaping).
    static func isProcedural(_ ch: UInt32) -> Bool {
        if ch >= 0x2580 && ch <= 0x259F { return true }
        if dashed(ch) != nil || (ch >= 0x256D && ch <= 0x2570) { return true }
        if ch >= 0x2500 && ch <= 0x257F {
            let i = Int(ch - 0x2500)
            return i < arms.count && arms[i] != nil
        }
        return false
    }

    /// Rectangles for `ch` in a `width`×`height` cell. `light` is the light line thickness.
    static func rects(for ch: UInt32, width: Int, height: Int, light: Int) -> [BoxRect] {
        if ch >= 0x2580 && ch <= 0x259F {
            return blockRects(ch, width, height)
        }
        guard ch >= 0x2500 && ch <= 0x257F else { return [] }
        if let (horizontal, heavy, n) = dashed(ch) {
            return dashRects(horizontal: horizontal, heavy: heavy, count: n, width, height, light)
        }
        if ch >= 0x256D && ch <= 0x2570 {
            return roundedCorner(ch, width, height, light)
        }
        let i = Int(ch - 0x2500)
        guard i < arms.count, let a = arms[i] else { return [] }
        return lineRects(up: a.0, right: a.1, down: a.2, left: a.3, width, height, light)
    }

    // MARK: Lines

    private static func lineRects(up: Int, right: Int, down: Int, left: Int,
                                  _ W: Int, _ H: Int, _ lightIn: Int) -> [BoxRect] {
        let l = max(1, min(lightIn, max(1, min(W, H) / 5)))
        let heavy = max(l + 1, l * 2)
        func span(_ w: Int) -> Int {
            switch w {
            case 1: return l
            case 2: return heavy
            case 3: return 3 * l
            default: return 0
            }
        }
        let cx = W / 2
        let cy = H / 2
        func start(_ c: Int, _ w: Int) -> Int { c - span(w) / 2 }

        let vSpan = max(span(up), span(down))
        let hSpan = max(span(left), span(right))
        let vStart = cx - vSpan / 2
        let hStart = cy - hSpan / 2

        // Where horizontal arms end/start near the center.
        let fullEndX = vSpan > 0 ? vStart + vSpan : cx + 1
        let fullStartX = vSpan > 0 ? vStart : cx
        let fullEndY = hSpan > 0 ? hStart + hSpan : cy + 1
        let fullStartY = hSpan > 0 ? hStart : cy
        func innerEndX(_ p: Int) -> Int { p == 3 ? start(cx, 3) + l : start(cx, p) + span(p) }
        func innerStartX(_ p: Int) -> Int { p == 3 ? start(cx, 3) + 2 * l : start(cx, p) }
        func innerEndY(_ p: Int) -> Int { p == 3 ? start(cy, 3) + l : start(cy, p) + span(p) }
        func innerStartY(_ p: Int) -> Int { p == 3 ? start(cy, 3) + 2 * l : start(cy, p) }

        var out: [BoxRect] = []
        func add(_ x0: Int, _ y0: Int, _ x1: Int, _ y1: Int) {
            let x = max(0, min(x0, x1)), y = max(0, min(y0, y1))
            let xe = min(W, max(x0, x1)), ye = min(H, max(y0, y1))
            if xe > x && ye > y { out.append(BoxRect(x: x, y: y, w: xe - x, h: ye - y, alpha: 1)) }
        }

        // Left arm.
        if left == 1 || left == 2 {
            let end = (up == 3 && down == 3 && right == 0) ? vStart + l : fullEndX
            let y = start(cy, left)
            add(0, y, end, y + span(left))
        } else if left == 3 {
            let s = start(cy, 3)
            add(0, s, up > 0 ? innerEndX(up) : fullEndX, s + l)
            add(0, s + 2 * l, down > 0 ? innerEndX(down) : fullEndX, s + 3 * l)
        }
        // Right arm.
        if right == 1 || right == 2 {
            let begin = (up == 3 && down == 3 && left == 0) ? vStart + 2 * l : fullStartX
            let y = start(cy, right)
            add(begin, y, W, y + span(right))
        } else if right == 3 {
            let s = start(cy, 3)
            add(up > 0 ? innerStartX(up) : fullStartX, s, W, s + l)
            add(down > 0 ? innerStartX(down) : fullStartX, s + 2 * l, W, s + 3 * l)
        }
        // Up arm.
        if up == 1 || up == 2 {
            let end = (left == 3 && right == 3 && down == 0) ? hStart + l : fullEndY
            let x = start(cx, up)
            add(x, 0, x + span(up), end)
        } else if up == 3 {
            let s = start(cx, 3)
            add(s, 0, s + l, left > 0 ? innerEndY(left) : fullEndY)
            add(s + 2 * l, 0, s + 3 * l, right > 0 ? innerEndY(right) : fullEndY)
        }
        // Down arm.
        if down == 1 || down == 2 {
            let begin = (left == 3 && right == 3 && up == 0) ? hStart + 2 * l : fullStartY
            let x = start(cx, down)
            add(x, begin, x + span(down), H)
        } else if down == 3 {
            let s = start(cx, 3)
            add(s, left > 0 ? innerStartY(left) : fullStartY, s + l, H)
            add(s + 2 * l, right > 0 ? innerStartY(right) : fullStartY, s + 3 * l, H)
        }
        return out
    }

    private static func lineWidth(_ W: Int, _ H: Int, _ light: Int, heavy: Bool) -> Int {
        let l = max(1, min(light, max(1, min(W, H) / 5)))
        return heavy ? max(l + 1, l * 2) : l
    }

    /// `count` dashes per cell, the gap split across both ends so neighbouring cells tile
    /// into an evenly dashed line.
    private static func dashRects(horizontal: Bool, heavy: Bool, count: Int,
                                  _ W: Int, _ H: Int, _ light: Int) -> [BoxRect] {
        let t = lineWidth(W, H, light, heavy: heavy)
        let length = horizontal ? W : H
        var out: [BoxRect] = []
        for i in 0..<count {
            let a = length * i / count
            let b = length * (i + 1) / count
            let gap = max(1, (b - a) / 3)
            let s = a + gap / 2
            let e = b - (gap - gap / 2)
            guard e > s else { continue }
            if horizontal {
                out.append(BoxRect(x: s, y: H / 2 - t / 2, w: e - s, h: t, alpha: 1))
            } else {
                out.append(BoxRect(x: W / 2 - t / 2, y: s, w: t, h: e - s, alpha: 1))
            }
        }
        return out
    }

    /// ╭ ╮ ╯ ╰: a quarter circle through the cell center with straight arms to the edges.
    private static func roundedCorner(_ ch: UInt32, _ W: Int, _ H: Int, _ light: Int) -> [BoxRect] {
        let t = lineWidth(W, H, light, heavy: false)
        let cx = W / 2
        let cy = H / 2
        // Line centers, matching the straight box lines (which start at cx - t/2).
        let lx = Float(cx - t / 2) + Float(t) / 2
        let ly = Float(cy - t / 2) + Float(t) / 2
        let r = Float(min(W, H)) / 2
        let right = ch == 0x256D || ch == 0x2570   // arms towards the right edge
        let down = ch == 0x256D || ch == 0x256E    // arms towards the bottom edge
        // Circle center, on the side of the arms.
        let ccx = right ? lx + r : lx - r
        let ccy = down ? ly + r : ly - r
        // The quadrant square between the line centers and the circle center (+ half a line).
        let half = Float(t) / 2 + 1
        let qx0 = right ? lx - half : ccx
        let qx1 = right ? ccx : lx + half
        let qy0 = down ? ly - half : ccy
        let qy1 = down ? ccy : ly + half
        var out: [BoxRect] = []
        let x0 = Int(floor(qx0)), y0 = Int(floor(qy0))
        let x1 = Int(ceil(qx1)), y1 = Int(ceil(qy1))
        out.append(BoxRect(x: x0, y: y0, w: x1 - x0, h: y1 - y0, alpha: 1,
                           arc: SIMD4<Float>(r, Float(t), ccx - Float(x0), ccy - Float(y0))))
        // Straight arms from the arc's ends to the cell edges.
        let hy = cy - t / 2
        let vx = cx - t / 2
        if right {
            let s = Int(ccx.rounded())
            if s < W { out.append(BoxRect(x: s, y: hy, w: W - s, h: t, alpha: 1)) }
        } else {
            let e = Int(ccx.rounded())
            if e > 0 { out.append(BoxRect(x: 0, y: hy, w: e, h: t, alpha: 1)) }
        }
        if down {
            let s = Int(ccy.rounded())
            if s < H { out.append(BoxRect(x: vx, y: s, w: t, h: H - s, alpha: 1)) }
        } else {
            let e = Int(ccy.rounded())
            if e > 0 { out.append(BoxRect(x: vx, y: 0, w: t, h: e, alpha: 1)) }
        }
        return out
    }

    // MARK: Block elements

    private static func blockRects(_ ch: UInt32, _ W: Int, _ H: Int) -> [BoxRect] {
        func eighthsY(_ n: Int) -> Int { (H * n + 4) / 8 }
        func eighthsX(_ n: Int) -> Int { (W * n + 4) / 8 }
        let hx = (W + 1) / 2
        let hy = (H + 1) / 2
        func r(_ x: Int, _ y: Int, _ w: Int, _ h: Int, _ a: Float = 1) -> BoxRect {
            BoxRect(x: x, y: y, w: max(0, w), h: max(0, h), alpha: a)
        }
        func quadrants(_ mask: Int) -> [BoxRect] {
            var out: [BoxRect] = []
            if mask & 1 != 0 { out.append(r(0, 0, hx, hy)) }         // upper left
            if mask & 2 != 0 { out.append(r(hx, 0, W - hx, hy)) }    // upper right
            if mask & 4 != 0 { out.append(r(0, hy, hx, H - hy)) }    // lower left
            if mask & 8 != 0 { out.append(r(hx, hy, W - hx, H - hy)) } // lower right
            return out
        }
        switch ch {
        case 0x2580: return [r(0, 0, W, hy)]
        case 0x2581...0x2587:
            let n = Int(ch - 0x2580) // 1/8 ... 7/8 from the bottom
            let h = eighthsY(n)
            return [r(0, H - h, W, h)]
        case 0x2588: return [r(0, 0, W, H)]
        case 0x2589...0x258F:
            let n = Int(0x2590 - ch) // 7/8 ... 1/8 from the left
            return [r(0, 0, eighthsX(n), H)]
        case 0x2590: return [r(hx, 0, W - hx, H)]
        case 0x2591: return [r(0, 0, W, H, 0.25)]
        case 0x2592: return [r(0, 0, W, H, 0.5)]
        case 0x2593: return [r(0, 0, W, H, 0.75)]
        case 0x2594: return [r(0, 0, W, max(1, eighthsY(1)))]
        case 0x2595:
            let w = max(1, eighthsX(1))
            return [r(W - w, 0, w, H)]
        case 0x2596: return quadrants(4)
        case 0x2597: return quadrants(8)
        case 0x2598: return quadrants(1)
        case 0x2599: return quadrants(1 | 4 | 8)
        case 0x259A: return quadrants(1 | 8)
        case 0x259B: return quadrants(1 | 2 | 4)
        case 0x259C: return quadrants(1 | 2 | 8)
        case 0x259D: return quadrants(2)
        case 0x259E: return quadrants(2 | 4)
        case 0x259F: return quadrants(2 | 4 | 8)
        default: return []
        }
    }
}
