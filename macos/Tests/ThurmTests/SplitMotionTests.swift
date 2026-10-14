import XCTest
@testable import Thurm

final class SplitMotionTests: XCTestCase {
    private let a = PaneKey(localHost, 1)
    private let b = PaneKey(localHost, 2)
    private let c = PaneKey(localHost, 3)

    func testSideFindsTheDirectChildSplit() {
        let tree = SplitNode.split(axis: .horizontal, ratio: 0.5, first: .leaf(a),
                                   second: .split(axis: .vertical, ratio: 0.5, first: .leaf(b), second: .leaf(c)))
        XCTAssertEqual(tree.side(of: a)?.axis, .horizontal)
        XCTAssertEqual(tree.side(of: a)?.second, false)
        XCTAssertEqual(tree.side(of: c)?.axis, .vertical)
        XCTAssertEqual(tree.side(of: c)?.second, true)
        XCTAssertNil(SplitNode.leaf(a).side(of: a))
    }

    /// A pane opened to the right grows out of the far right edge; one opened above, out of the top.
    func testCollapsedFlattensOntoTheOuterEdge() {
        let rect = CGRect(x: 100, y: 20, width: 300, height: 200)
        XCTAssertEqual(SplitNode.collapsed(rect, axis: .horizontal, second: true),
                       CGRect(x: 400, y: 20, width: 0, height: 200))
        XCTAssertEqual(SplitNode.collapsed(rect, axis: .horizontal, second: false),
                       CGRect(x: 100, y: 20, width: 0, height: 200))
        XCTAssertEqual(SplitNode.collapsed(rect, axis: .vertical, second: true),
                       CGRect(x: 100, y: 220, width: 300, height: 0))
        XCTAssertEqual(SplitNode.collapsed(rect, axis: .vertical, second: false),
                       CGRect(x: 100, y: 20, width: 300, height: 0))
    }

    /// Leaving content is gone by the halfway point, before arriving content starts to show.
    func testCrossFadeNeverOverlaps() {
        XCTAssertEqual(Motion.crossFade(0, appearing: false), 1)
        XCTAssertEqual(Motion.crossFade(0.5, appearing: false), 0)
        XCTAssertEqual(Motion.crossFade(1, appearing: false), 0)
        XCTAssertEqual(Motion.crossFade(0, appearing: true), 0)
        XCTAssertEqual(Motion.crossFade(0.5, appearing: true), 0)
        XCTAssertEqual(Motion.crossFade(1, appearing: true), 1)
        XCTAssertEqual(Motion.crossFade(0.25, appearing: false), 0.5, accuracy: 1e-9)
        XCTAssertEqual(Motion.crossFade(0.75, appearing: true), 0.5, accuracy: 1e-9)
    }

    /// The pane curve settles without overshoot, so a growing pane never passes its place.
    func testPaneCurveDoesNotOvershoot() {
        var last = 0.0
        for i in 0...100 {
            let y = Motion.paneCurve.value(at: Double(i) / 100)
            XCTAssertGreaterThanOrEqual(y, last - 1e-9)
            XCTAssertLessThanOrEqual(y, 1 + 1e-9)
            last = y
        }
        XCTAssertGreaterThan(Motion.paneCurve.value(at: 0.3), 0.7)
    }

    func testTweenUsesItsCurve() {
        var tween = Tween(0, curve: Motion.paneCurve)
        tween.move(to: 1, duration: 1, at: 0)
        if Motion.reduced { return }
        XCTAssertEqual(tween.value(at: 0.3), Motion.paneCurve.value(at: 0.3), accuracy: 1e-9)
    }
}
