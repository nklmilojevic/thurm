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

    /// The pane spring settles without overshoot, so a growing pane never passes its place.
    func testPaneSpringDoesNotOvershoot() {
        var last = 0.0
        for i in 0...100 {
            let y = Motion.paneSpring.value(at: Double(i) / 100)
            XCTAssertGreaterThanOrEqual(y, last - 1e-9)
            XCTAssertLessThanOrEqual(y, 1 + 1e-9)
            last = y
        }
        XCTAssertEqual(Motion.paneSpring.value(at: 1), 1)
    }

    /// Rex's "Snappy": half the way at 74 ms, 90% at 171 ms, settled by Core Animation at 0.5 s.
    func testPaneSpringMatchesCoreAnimation() {
        let settle = Motion.paneSettle
        XCTAssertEqual(settle, 0.5, accuracy: 0.01)
        XCTAssertEqual(Motion.paneSpring.value(at: 0.074 / settle), 0.5, accuracy: 0.01)
        XCTAssertEqual(Motion.paneSpring.value(at: 0.171 / settle), 0.9, accuracy: 0.01)
    }

    func testTweenUsesItsCurve() {
        var tween = Tween(0, curve: Motion.paneSpring)
        tween.move(to: 1, duration: 1, at: 0)
        if Motion.reduced { return }
        XCTAssertEqual(tween.value(at: 0.3), Motion.paneSpring.value(at: 0.3), accuracy: 1e-9)
    }

    func testTweenRestartKeepsItsMove() {
        var tween = Tween(0, curve: Motion.paneSpring)
        tween.move(to: 1, duration: 0.5, at: 10)
        if Motion.reduced { return }
        tween.restart(at: 20)
        XCTAssertEqual(tween.value(at: 20), 0)
        XCTAssertEqual(tween.end, 20.5)
        XCTAssertFalse(tween.isFinished(at: 20.4))
        XCTAssertTrue(tween.isFinished(at: 20.5))
    }

    /// Each recorded frame becomes a discrete keyframe; what holds still is not animated.
    func testBakedMotionKeyframesWhatChanges() throws {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        let root = CALayer()
        let pane = CALayer()
        root.addSublayer(pane)
        var motion = BakedMotion()
        for x in [0.0, 10, 20] {
            pane.frame = CGRect(x: x, y: 0, width: 100, height: 50)
            motion.record(root)
        }
        XCTAssertEqual(motion.frames, 3)
        let animated = motion.play(begin: CACurrentMediaTime(), duration: 0.5)
        XCTAssertEqual(animated.map(ObjectIdentifier.init), [ObjectIdentifier(pane)])
        XCTAssertEqual(pane.animationKeys(), [BakedMotion.keyPrefix + "position"])
        let position = try XCTUnwrap(pane.animation(forKey: BakedMotion.keyPrefix + "position") as? CAKeyframeAnimation)
        XCTAssertEqual(position.calculationMode, .discrete)
        let keyTimes = try XCTUnwrap(position.keyTimes).map(\.doubleValue)
        XCTAssertEqual(keyTimes.count, 4)
        for (time, expected) in zip(keyTimes, [0, 1.0 / 3, 2.0 / 3, 1]) {
            XCTAssertEqual(time, expected, accuracy: 1e-6)
        }
        XCTAssertEqual((position.values as? [NSValue])?.map(\.pointValue.x), [50, 60, 70])
        BakedMotion.stop(animated)
        XCTAssertNil(pane.animationKeys())
    }

    /// A layer added during the move (a new card's outline) is hidden in the frames before it.
    func testBakedMotionHidesLayersBeforeTheyAppear() throws {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        let root = CALayer()
        var motion = BakedMotion()
        motion.record(root)
        let outline = CAShapeLayer()
        root.addSublayer(outline)
        motion.record(root)
        _ = motion.play(begin: CACurrentMediaTime(), duration: 0.5)
        let hidden = try XCTUnwrap(outline.animation(forKey: BakedMotion.keyPrefix + "hidden") as? CAKeyframeAnimation)
        XCTAssertEqual(hidden.values as? [NSNumber], [true, false].map(NSNumber.init))
    }
}
