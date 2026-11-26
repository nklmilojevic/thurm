import XCTest
@testable import Thurm

final class ImageDownsampleTests: XCTestCase {
    /// Overlapping footprints read edge pixels twice; the budget counts every read. 4097×4000 to
    /// 4096×3999: each output pixel reads 2 columns and 2 rows, 16 384 reads a row.
    func testRowCostCountsRepeatedReads() {
        let h = TerminalRenderer.horizontalVisits(srcWidth: 4097, dstWidth: 4096)
        XCTAssertEqual(h, 8192)
        let (rows, cost) = TerminalRenderer.rowsWithin(budget: TerminalRenderer.downsampleBudget, from: 0,
                                                       maxRows: 3999, srcHeight: 4000, dstHeight: 3999,
                                                       horizontalVisits: h)
        XCTAssertEqual(rows, 122)
        XCTAssertEqual(cost, 122 * 16384)
        XCTAssertLessThanOrEqual(cost, TerminalRenderer.downsampleBudget)
        // Partly spent budget: only rows that fit.
        XCTAssertEqual(TerminalRenderer.rowsWithin(budget: 16383, from: 0, maxRows: 10, srcHeight: 4000,
                                                   dstHeight: 3999, horizontalVisits: h).rows, 0)
    }

    /// A single row never exceeds a whole draw's budget, so a fresh draw always makes progress.
    func testOneRowAlwaysFitsAFullBudget() {
        for (sw, sh, dw, dh) in [(16384, 16384, 4096, 4096), (16384, 4097, 4096, 1024),
                                 (4097, 16384, 1024, 4096), (16383, 16381, 4095, 4095)] {
            let h = TerminalRenderer.horizontalVisits(srcWidth: sw, dstWidth: dw)
            for y in [0, 1, dh / 2, dh - 1] {
                XCTAssertEqual(TerminalRenderer.rowsWithin(budget: TerminalRenderer.downsampleBudget, from: y,
                                                           maxRows: 1, srcHeight: sh, dstHeight: dh,
                                                           horizontalVisits: h).rows, 1, "\(sw)x\(sh) row \(y)")
            }
        }
    }

    /// Room for a new texture comes from the least recently drawn ones, never from one this draw
    /// shows; when that isn't enough, nothing is evicted and it doesn't fit.
    func testTextureEvictionsSpareThisDraw() {
        let now = 10.0
        let resident: [(key: Int, bytes: Int, lastUsed: Double)] = [
            (key: 1, bytes: 40, lastUsed: 5), (key: 2, bytes: 40, lastUsed: 3), (key: 3, bytes: 40, lastUsed: now),
        ]
        XCTAssertEqual(TerminalRenderer.evictions(adding: 20, resident: resident, cap: 200, now: now), [])
        XCTAssertEqual(TerminalRenderer.evictions(adding: 40, resident: resident, cap: 120, now: now), [2])
        XCTAssertEqual(TerminalRenderer.evictions(adding: 60, resident: resident, cap: 100, now: now), [2, 1])
        XCTAssertNil(TerminalRenderer.evictions(adding: 80, resident: resident, cap: 100, now: now))
    }

    private func downsample(_ src: [UInt8], _ w: Int, _ h: Int, to dw: Int, _ dh: Int,
                            bandRows: Int? = nil) -> [UInt8] {
        var out = [UInt8](repeating: 0, count: dw * dh * 4)
        let band = bandRows ?? dh
        src.withUnsafeBufferPointer { s in
            out.withUnsafeMutableBufferPointer { d in
                var y0 = 0
                while y0 < dh {
                    let rows = min(band, dh - y0)
                    TerminalRenderer.downsample(s.baseAddress!, width: w, height: h, toWidth: dw, height: dh,
                                                rows: y0..<(y0 + rows), into: d.baseAddress! + y0 * dw * 4)
                    y0 += rows
                }
            }
        }
        return out
    }

    /// One-pixel black/white stripes halved keep both colours' contribution (grey), where a
    /// point sample of the even columns would have turned them solid.
    func testHalvingStripesAveragesBothColors() {
        let w = 8, h = 4
        var src = [UInt8](repeating: 0, count: w * h * 4)
        for y in 0..<h {
            for x in 0..<w {
                let v: UInt8 = x % 2 == 0 ? 0 : 255
                let i = (y * w + x) * 4
                src[i] = v; src[i + 1] = v; src[i + 2] = v; src[i + 3] = 255
            }
        }
        let out = downsample(src, w, h, to: 4, 2)
        for p in stride(from: 0, to: out.count, by: 4) {
            XCTAssertEqual(Int(out[p]), 128, accuracy: 1)
            XCTAssertEqual(out[p + 3], 255)
        }
    }

    /// At a 3:2 reduction each output pixel covers 1.5 source pixels. Repeating black-white-black
    /// columns put the same third of white in every footprint, so the result is a uniform grey
    /// (85), not alternating black and grey from rounding the footprints to whole pixels.
    func testNonIntegerReductionWeighsPartlyCoveredPixels() {
        let w = 6, h = 3
        var src = [UInt8](repeating: 0, count: w * h * 4)
        for y in 0..<h {
            for x in 0..<w {
                let v: UInt8 = x % 3 == 1 ? 255 : 0
                let i = (y * w + x) * 4
                src[i] = v; src[i + 1] = v; src[i + 2] = v; src[i + 3] = 255
            }
        }
        let out = downsample(src, w, h, to: 4, 2)
        for p in stride(from: 0, to: out.count, by: 4) {
            XCTAssertEqual(Array(out[p..<(p + 4)]), [85, 85, 85, 255], "pixel \(p / 4)")
        }
    }

    /// A transparent pixel's colour doesn't bleed into its opaque neighbour's.
    func testTransparentPixelsDoNotDarkenColor() {
        // Red opaque next to black fully transparent.
        let src: [UInt8] = [255, 0, 0, 255, 0, 0, 0, 0]
        let out = downsample(src, 2, 1, to: 1, 1)
        XCTAssertEqual(Array(out[0..<3]), [255, 0, 0])
        XCTAssertEqual(Int(out[3]), 128, accuracy: 1)
        let clear = downsample([9, 9, 9, 0, 9, 9, 9, 0], 2, 1, to: 1, 1)
        XCTAssertEqual(clear, [0, 0, 0, 0])
    }

    /// Uploading in bands gives the same pixels as one pass over the whole image.
    func testBandsMatchAWholeImagePass() {
        let w = 37, h = 29
        let src = (0..<(w * h * 4)).map { UInt8(truncatingIfNeeded: $0 &* 31 &+ 7) }
        let whole = downsample(src, w, h, to: 15, 11)
        XCTAssertEqual(downsample(src, w, h, to: 15, 11, bandRows: 3), whole)
        XCTAssertEqual(downsample(src, w, h, to: 15, 11, bandRows: 1), whole)
    }

    func testBandBufferStaysBounded() {
        let dstW = 4096
        let rows = max(1, TerminalRenderer.downsampleBandBytes / (dstW * 4))
        XCTAssertLessThanOrEqual(rows * dstW * 4, TerminalRenderer.downsampleBandBytes)
    }
}

final class RoundedCornerGeometryTests: XCTestCase {
    private let corners: [UInt32] = [0x256D, 0x256E, 0x256F, 0x2570] // ╭ ╮ ╯ ╰

    /// At odd and even cell sizes, every rect of a rounded corner stays in the cell, the arc's
    /// centre is where the cell geometry puts it (whatever origin clipping gave the quad), and
    /// the arms line up with the straight lines they join.
    func testRoundedCornersAlignAtOddAndEvenCellSizes() throws {
        for (W, H) in [(7, 15), (8, 16), (9, 17), (10, 21), (11, 22)] {
            for light in [1, 2] {
                let horizontal = try XCTUnwrap(BoxDrawing.rects(for: 0x2500, width: W, height: H, light: light).first)
                let vertical = try XCTUnwrap(BoxDrawing.rects(for: 0x2502, width: W, height: H, light: light).first)
                for ch in corners {
                    let label = String(format: "U+%04X %dx%d light %d", ch, W, H, light)
                    let rects = BoxDrawing.rects(for: ch, width: W, height: H, light: light)
                    XCTAssertFalse(rects.isEmpty, label)
                    for r in rects {
                        XCTAssertTrue(r.x >= 0 && r.y >= 0 && r.w > 0 && r.h > 0
                                      && r.x + r.w <= W && r.y + r.h <= H, "\(label): \(r) leaves the cell")
                    }
                    let arc = try XCTUnwrap(rects.first { $0.arc != nil }?.arc, label)
                    let right = ch == 0x256D || ch == 0x2570
                    let down = ch == 0x256D || ch == 0x256E
                    // Line centres of the straight glyphs this corner joins.
                    let lx = Float(vertical.x) + Float(vertical.w) / 2
                    let ly = Float(horizontal.y) + Float(horizontal.h) / 2
                    let radius = Float(min(W, H)) / 2
                    let arcRect = rects.first { $0.arc != nil }!
                    XCTAssertEqual(arc.x, radius, label)
                    XCTAssertEqual(Float(arcRect.x) + arc.z, right ? lx + radius : lx - radius, accuracy: 0.001, label)
                    XCTAssertEqual(Float(arcRect.y) + arc.w, down ? ly + radius : ly - radius, accuracy: 0.001, label)
                    // Arms: same thickness and offset as the straight lines.
                    let arms = rects.filter { $0.arc == nil }
                    // An arm may vanish when the arc already reaches the edge; when present it aligns.
                    for a in arms {
                        let isHorizontal = a.h == horizontal.h && a.y == horizontal.y
                            && (right ? a.x + a.w == W : a.x == 0)
                        let isVertical = a.w == vertical.w && a.x == vertical.x
                            && (down ? a.y + a.h == H : a.y == 0)
                        XCTAssertTrue(isHorizontal || isVertical, "\(label): arm \(a) misaligned with the straight lines")
                    }
                }
            }
        }
    }
}
