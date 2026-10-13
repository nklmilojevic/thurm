import AppKit
import QuartzCore

/// The motion the chrome shares: one easing curve, a few duration tokens and the reduce-motion
/// switch. Nothing here runs a free timer: a `MotionDriver` lives only as long as its animation.
enum Motion {
    /// Small state changes: selection, focus dimming, toasts, divider highlight.
    static let fast: TimeInterval = 0.12
    /// Layout moves: sidebar, zoom, tab drag and the quick terminal.
    static let standard: TimeInterval = 0.18
    /// The one-shot attention ring on a tab.
    static let pulse: TimeInterval = 0.6

    /// cubic-bezier(0.4, -0.4, 0.2, 1.2): a quick start that settles with a 1.7% overshoot.
    static let curve = CubicBezier(x1: 0.4, y1: -0.4, x2: 0.2, y2: 1.2)
    static let timingFunction = CAMediaTimingFunction(controlPoints: 0.4, -0.4, 0.2, 1.2)

    /// Whether the user asked for less motion (Accessibility > Display > Reduce motion).
    static var reduced: Bool { NSWorkspace.shared.accessibilityDisplayShouldReduceMotion }

    /// `duration`, or 0 under reduce motion: callers then apply the end state at once.
    static func duration(_ duration: TimeInterval) -> TimeInterval { reduced ? 0 : duration }
}

/// A CSS-style cubic-bezier timing curve. `value(at:)` maps a time fraction to eased progress: it
/// solves the x curve for the curve parameter (Newton's method, bisection as a fallback), then
/// reads the y curve at that parameter.
struct CubicBezier {
    let x1: Double
    let y1: Double
    let x2: Double
    let y2: Double

    func value(at x: Double) -> Double {
        if x <= 0 { return 0 }
        if x >= 1 { return 1 }
        var t = x
        for _ in 0..<8 {
            let error = Self.point(t, x1, x2) - x
            if abs(error) < 1e-6 { return Self.point(t, y1, y2) }
            let slope = Self.slope(t, x1, x2)
            guard abs(slope) > 1e-6 else { break }
            t = min(1, max(0, t - error / slope))
        }
        var low = 0.0
        var high = 1.0
        t = x
        for _ in 0..<60 {
            let error = Self.point(t, x1, x2) - x
            if abs(error) < 1e-6 { break }
            if error < 0 { low = t } else { high = t }
            t = (low + high) / 2
        }
        return Self.point(t, y1, y2)
    }

    /// One coordinate of the curve at parameter `t`, with end points 0 and 1.
    private static func point(_ t: Double, _ a: Double, _ b: Double) -> Double {
        let u = 1 - t
        return 3 * u * u * t * a + 3 * u * t * t * b + t * t * t
    }

    private static func slope(_ t: Double, _ a: Double, _ b: Double) -> Double {
        let u = 1 - t
        return 3 * u * u * a + 6 * u * t * (b - a) + 3 * t * t * (1 - b)
    }
}

/// A value that eases from where it is toward a target over a time box. Moving the target
/// mid-flight starts from the value on screen, so a change of mind never snaps.
struct Tween {
    private(set) var from: Double
    private(set) var to: Double
    private var start: CFTimeInterval = 0
    private var duration: TimeInterval = 0

    init(_ value: Double) {
        from = value
        to = value
    }

    func value(at now: CFTimeInterval) -> Double {
        guard duration > 0 else { return to }
        let t = min(1, max(0, (now - start) / duration))
        return from + (to - from) * Motion.curve.value(at: t)
    }

    func isFinished(at now: CFTimeInterval) -> Bool {
        now - start >= duration
    }

    /// Eases toward `target` over `duration` seconds from the value at `now`. A zero duration
    /// (reduce motion) jumps to the target.
    mutating func move(to target: Double, duration: TimeInterval, at now: CFTimeInterval) {
        guard target != to else { return }
        from = value(at: now)
        to = target
        start = now
        self.duration = Motion.duration(duration)
        if self.duration == 0 { from = target }
    }
}

/// Calls `frame` on each display refresh of one short animation, then stops. The display link
/// exists only while the animation runs: once its deadline passes it is invalidated, so an idle
/// window keeps no timer. The last call comes at or after the deadline.
final class MotionDriver: NSObject {
    private var link: CADisplayLink?
    private var deadline: CFTimeInterval = 0
    private var frame: ((CFTimeInterval) -> Void)?

    /// Starts the animation on `view`'s display, replacing any run in progress.
    func run(on view: NSView, for duration: TimeInterval, frame: @escaping (CFTimeInterval) -> Void) {
        stop()
        self.frame = frame
        deadline = CACurrentMediaTime() + duration
        let link = view.displayLink(target: self, selector: #selector(tick(_:)))
        link.add(to: .main, forMode: .common)
        self.link = link
    }

    func stop() {
        link?.invalidate()
        link = nil
        frame = nil
    }

    @objc private func tick(_ link: CADisplayLink) {
        let now = CACurrentMediaTime()
        frame?(now)
        if now >= deadline { stop() }
    }

    deinit {
        link?.invalidate()
    }
}
