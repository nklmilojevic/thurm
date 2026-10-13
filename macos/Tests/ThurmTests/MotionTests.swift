import XCTest
@testable import Thurm

final class MotionTests: XCTestCase {
    private let curve = Motion.curve

    func testEndpoints() {
        XCTAssertEqual(curve.value(at: 0), 0)
        XCTAssertEqual(curve.value(at: 1), 1)
    }

    func testOvershootIsSlight() {
        let peak = stride(from: 0.0, through: 1.0, by: 0.001).map { curve.value(at: $0) }.max() ?? 0
        XCTAssertGreaterThan(peak, 1)
        XCTAssertLessThan(peak, 1.02)
    }

    func testMatchesBisectionReference() {
        func x(_ t: Double) -> Double { 3 * (1 - t) * (1 - t) * t * curve.x1 + 3 * (1 - t) * t * t * curve.x2 + t * t * t }
        func y(_ t: Double) -> Double { 3 * (1 - t) * (1 - t) * t * curve.y1 + 3 * (1 - t) * t * t * curve.y2 + t * t * t }
        for step in 1..<100 {
            let target = Double(step) / 100
            var low = 0.0
            var high = 1.0
            for _ in 0..<100 {
                let mid = (low + high) / 2
                if x(mid) < target { low = mid } else { high = mid }
            }
            XCTAssertEqual(curve.value(at: target), y((low + high) / 2), accuracy: 1e-4)
        }
    }

    func testTweenRetargetsFromValueOnScreen() {
        var tween = Tween(0)
        tween.move(to: 1, duration: 0.2, at: 0)
        let midway = tween.value(at: 0.1)
        tween.move(to: 0, duration: 0.2, at: 0.1)
        XCTAssertEqual(tween.value(at: 0.1), midway, accuracy: 1e-9)
        XCTAssertEqual(tween.value(at: 0.3), 0)
    }
}
