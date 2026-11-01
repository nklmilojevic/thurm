import AppKit
import QuartzCore

/// The motion the chrome shares: one easing curve, a few duration tokens and the reduce-motion
/// switch. Nothing here runs a free timer: a `MotionDriver` lives only as long as its animation.
enum Motion {
    /// `window.animation_speed`, kept in step with the config: it scales `fast`, `standard` and
    /// `paneSettle`. The attention pulse keeps its time, so it still catches the eye.
    static var speed = AnimationSpeed.normal

    /// Small state changes: selection, focus dimming, toasts, divider highlight.
    static var fast: TimeInterval { 0.048 * speed.scale }
    /// Short layout moves, such as tab drag.
    static var standard: TimeInterval { 0.072 * speed.scale }
    /// A sidebar moves a whole window of text; accelerate and settle gently at both ends.
    static var sidebar: TimeInterval { 0.34 * speed.scale }
    static let sidebarTimingFunction = CAMediaTimingFunction(controlPoints: 0.42, 0, 0.58, 1)
    /// Blend a program's old and new line wrapping while the sidebar is moving.
    static var sidebarReflow: TimeInterval { 0.20 * speed.scale }
    /// The one-shot attention ring on a tab.
    static let pulse: TimeInterval = 0.6

    /// cubic-bezier(0.4, -0.4, 0.2, 1.2): a quick start that settles with a 1.7% overshoot.
    static let curve = CubicBezier(x1: 0.4, y1: -0.4, x2: 0.2, y2: 1.2)
    static let timingFunction = CAMediaTimingFunction(controlPoints: 0.4, -0.4, 0.2, 1.2)
    /// Split panes opening, closing, moving and zooming: a critically damped spring (stiffness
    /// 3250, mass 1), which settles without overshoot, so a growing pane never pokes past its
    /// place. Half the way in 30 ms, 90% in 68 ms, settled at `paneSettle`. A slower `speed`
    /// plays the same curve over more time: a softer spring, still critically damped.
    static let paneSpring = CriticalSpring(stiffness: 3250)
    static var paneSettle: TimeInterval { paneSpring.settlingDuration * speed.scale }

    /// Whether the user asked for less motion (Accessibility > Display > Reduce motion).
    static var reduced: Bool { NSWorkspace.shared.accessibilityDisplayShouldReduceMotion }

    /// `duration`, or 0 under reduce motion: callers then apply the end state at once.
    static func duration(_ duration: TimeInterval) -> TimeInterval { reduced ? 0 : duration }

    /// The opacity of content that appears or disappears with a layout move at eased progress
    /// `t`: leaving content smoothsteps out over the first half, arriving content waits out the
    /// first half and smoothsteps in over the second, so the two never show at once.
    static func crossFade(_ t: Double, appearing: Bool) -> Double {
        func smoothstep(_ u: Double) -> Double {
            let u = min(1, max(0, u))
            return u * u * (3 - 2 * u)
        }
        return appearing ? smoothstep(2 * t - 1) : 1 - smoothstep(2 * t)
    }
}

/// `window.animation_speed`.
enum AnimationSpeed: String {
    case normal, slow, slower

    /// How many times the normal duration an animation takes.
    var scale: Double {
        switch self {
        case .normal: return 1
        case .slow: return 1.6
        case .slower: return 2.5
        }
    }
}

/// Maps a time fraction (0...1) to eased progress, 0 at the start and 1 at the end.
protocol Easing {
    func value(at x: Double) -> Double
}

/// A critically damped spring with mass 1 over the time Core Animation takes to settle it:
/// `1 - (1 + ωt)·e^(-ωt)` with ω = √stiffness, ending at exactly 1.
struct CriticalSpring: Easing {
    let stiffness: Double
    /// `CASpringAnimation.settlingDuration` for this spring.
    let settlingDuration: TimeInterval

    init(stiffness: Double) {
        self.stiffness = stiffness
        let spring = CASpringAnimation()
        spring.mass = 1
        spring.stiffness = stiffness
        spring.damping = 2 * stiffness.squareRoot()
        settlingDuration = spring.settlingDuration
    }

    func value(at x: Double) -> Double {
        if x <= 0 { return 0 }
        if x >= 1 { return 1 }
        let wt = stiffness.squareRoot() * x * settlingDuration
        return 1 - (1 + wt) * exp(-wt)
    }
}

/// A CSS-style cubic-bezier timing curve. `value(at:)` maps a time fraction to eased progress: it
/// solves the x curve for the curve parameter (Newton's method, bisection as a fallback), then
/// reads the y curve at that parameter.
struct CubicBezier: Easing {
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
    private let curve: any Easing

    init(_ value: Double, curve: any Easing = Motion.curve) {
        from = value
        to = value
        self.curve = curve
    }

    func value(at now: CFTimeInterval) -> Double {
        guard duration > 0 else { return to }
        let t = min(1, max(0, (now - start) / duration))
        return from + (to - from) * curve.value(at: t)
    }

    func isFinished(at now: CFTimeInterval) -> Bool {
        now - start >= duration
    }

    /// When the move in progress ends.
    var end: CFTimeInterval { start + duration }

    /// Runs the move in progress from `start` instead, keeping where it goes and how long it takes.
    mutating func restart(at start: CFTimeInterval) {
        self.start = start
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
