import QuartzCore

/// Frame timing, logged once a second when `THURM_PERF=1` is set in the app's environment:
/// display ticks, frames rendered, new daemon frames shown, and render time (avg / max).
/// Written to `$THURM_PERF_LOG` (default /tmp/thurm-perf.log).
final class Perf {
    static let enabled = ProcessInfo.processInfo.environment["THURM_PERF"] == "1"
    static let shared = Perf()
    /// `THURM_RENDER=tick`: the old scheduling (draw on display-link ticks, present on vsync),
    /// for A/B comparisons.
    /// `THURM_PERF_IGNORE_OCCLUSION=1`: keep drawing while covered (measuring with the screen locked).
    static let ignoreOcclusion = ProcessInfo.processInfo.environment["THURM_PERF_IGNORE_OCCLUSION"] == "1"
    static let tickRendering = ProcessInfo.processInfo.environment["THURM_RENDER"] == "tick"

    private struct Stats {
        var ticks = 0
        var renders = 0
        var newFrames = 0
        var renderTotal: Double = 0
        var renderMax: Double = 0
        var lastGeneration: UInt64 = 0
        var linkInterval: Double = 0
        var scrollEvents = 0
        var scrollLines = 0
        var wheelSends = 0
        var updateTotal: Double = 0
        var drawTotal: Double = 0
        var keys = 0
        /// Key press → first new frame drawn after it.
        var keyLatencyTotal: Double = 0
        var keyLatencyMax: Double = 0
        var keyLatencyCount = 0
        var pendingKey: CFTimeInterval = 0
        /// Gaps between consecutive new frames, in display ticks: 1, 2, 3+.
        var gaps = [0, 0, 0]
        var lastNewFrameTick = 0
        var tickCount = 0
        var arrivals = 0
        var multiArrivalRenders = 0
        var keyToArrivalTotal: Double = 0
        var keyToArrivalMax: Double = 0
        var arrivalToRenderTotal: Double = 0
        var arrivalToRenderMax: Double = 0
        var arrivalSamples = 0
        var lastNewFrameTime: CFTimeInterval = 0
        var intervals: [Double] = []
    }

    private var stats: [PaneKey: Stats] = [:]
    private var windowStart = CACurrentMediaTime()

    func key(pane: PaneKey) {
        stats[pane, default: Stats()].keys += 1
        if stats[pane]?.pendingKey == 0 { stats[pane]?.pendingKey = CACurrentMediaTime() }
    }

    func tick(pane: PaneKey, link: CADisplayLink) {
        stats[pane, default: Stats()].ticks += 1
        stats[pane]?.tickCount += 1
        stats[pane]?.linkInterval = link.targetTimestamp - link.timestamp
        flushIfDue()
    }

    func scrollEvent(pane: PaneKey, lines: Int) {
        stats[pane, default: Stats()].scrollEvents += 1
        stats[pane]?.scrollLines += abs(lines)
    }

    func wheelSent(pane: PaneKey) {
        stats[pane, default: Stats()].wheelSends += 1
    }

    /// Time spent pulling the grid (`update`) and building + encoding (`draw`) in one render.
    func split(pane: PaneKey, update: Double, draw: Double) {
        stats[pane]?.updateTotal += update * 1000
        stats[pane]?.drawTotal += draw * 1000
    }

    func rendered(pane: PaneKey, start: CFTimeInterval, generation: UInt64) {
        let ms = (CACurrentMediaTime() - start) * 1000
        var s = stats[pane, default: Stats()]
        s.renders += 1
        s.renderTotal += ms
        s.renderMax = max(s.renderMax, ms)
        if let a = Core.shared.takeArrivals(pane) {
            s.arrivals += a.count
            if a.count > 1 { s.multiArrivalRenders += 1 }
            let wait = (CACurrentMediaTime() - a.last) * 1000
            s.arrivalToRenderTotal += wait
            s.arrivalToRenderMax = max(s.arrivalToRenderMax, wait)
            s.arrivalSamples += 1
            if s.pendingKey > 0 {
                let k = (a.first - s.pendingKey) * 1000
                s.keyToArrivalTotal += k
                s.keyToArrivalMax = max(s.keyToArrivalMax, k)
            }
        }
        if generation != s.lastGeneration {
            let t = CACurrentMediaTime()
            if s.lastNewFrameTime > 0, t - s.lastNewFrameTime < 0.1 { s.intervals.append((t - s.lastNewFrameTime) * 1000) }
            s.lastNewFrameTime = t
            s.newFrames += 1
            s.lastGeneration = generation
            let gap = s.tickCount - s.lastNewFrameTick
            if gap >= 1 && gap <= 6 { s.gaps[min(gap, 3) - 1] += 1 }
            s.lastNewFrameTick = s.tickCount
            if s.pendingKey > 0 {
                let ms = (CACurrentMediaTime() - s.pendingKey) * 1000
                s.keyLatencyTotal += ms
                s.keyLatencyMax = max(s.keyLatencyMax, ms)
                s.keyLatencyCount += 1
                s.pendingKey = 0
            }
        }
        stats[pane] = s
    }

    private let path = ProcessInfo.processInfo.environment["THURM_PERF_LOG"] ?? "/tmp/thurm-perf.log"

    private func write(_ line: String) {
        guard let data = (line + "\n").data(using: .utf8) else { return }
        if let h = FileHandle(forWritingAtPath: path) {
            h.seekToEndOfFile()
            h.write(data)
            try? h.close()
        } else {
            FileManager.default.createFile(atPath: path, contents: data)
        }
    }

    private func flushIfDue() {
        let now = CACurrentMediaTime()
        guard now - windowStart >= 1 else { return }
        let secs = now - windowStart
        for (pane, s) in stats where s.renders > 0 {
            let n = Double(s.renders)
            let iv = s.intervals
            let mean = iv.isEmpty ? 0 : iv.reduce(0, +) / Double(iv.count)
            let sd = iv.isEmpty ? 0 : (iv.map { ($0 - mean) * ($0 - mean) }.reduce(0, +) / Double(iv.count)).squareRoot()
            write(String(format: "pacing pane %llu: new-frame interval mean %.2f ms sd %.2f ms (n=%d)", pane.id, mean, sd, iv.count))
            write(String(format: "perf pane %llu: ticks %.0f/s (link %.2f ms) renders %.0f/s new frames %.0f/s render avg %.2f ms (grid %.2f, draw %.2f) max %.2f ms | scroll events %.0f/s lines %.0f/s wheel sends %.0f/s | keys %.0f/s key->frame avg %.1f max %.1f ms | frame gaps 1:%d 2:%d 3+:%d | daemon frames %.0f/s (renders with >1: %d) key->arrival avg %.1f max %.1f, arrival->render avg %.1f max %.1f ms",
                        pane.id, Double(s.ticks) / secs, s.linkInterval * 1000, n / secs,
                        Double(s.newFrames) / secs, s.renderTotal / n, s.updateTotal / n, s.drawTotal / n, s.renderMax,
                        Double(s.scrollEvents) / secs, Double(s.scrollLines) / secs, Double(s.wheelSends) / secs,
                        Double(s.keys) / secs, s.keyLatencyCount > 0 ? s.keyLatencyTotal / Double(s.keyLatencyCount) : 0,
                        s.keyLatencyMax, s.gaps[0], s.gaps[1], s.gaps[2],
                        Double(s.arrivals) / secs, s.multiArrivalRenders,
                        s.keyLatencyCount > 0 ? s.keyToArrivalTotal / Double(s.keyLatencyCount) : 0, s.keyToArrivalMax,
                        s.arrivalSamples > 0 ? s.arrivalToRenderTotal / Double(s.arrivalSamples) : 0, s.arrivalToRenderMax))
        }
        for key in stats.keys {
            stats[key]?.ticks = 0
            stats[key]?.renders = 0
            stats[key]?.newFrames = 0
            stats[key]?.renderTotal = 0
            stats[key]?.renderMax = 0
            stats[key]?.scrollEvents = 0
            stats[key]?.scrollLines = 0
            stats[key]?.wheelSends = 0
            stats[key]?.updateTotal = 0
            stats[key]?.drawTotal = 0
            stats[key]?.keys = 0
            stats[key]?.keyLatencyTotal = 0
            stats[key]?.keyLatencyMax = 0
            stats[key]?.keyLatencyCount = 0
            stats[key]?.gaps = [0, 0, 0]
            stats[key]?.arrivals = 0
            stats[key]?.multiArrivalRenders = 0
            stats[key]?.keyToArrivalTotal = 0
            stats[key]?.keyToArrivalMax = 0
            stats[key]?.arrivalToRenderTotal = 0
            stats[key]?.arrivalToRenderMax = 0
            stats[key]?.arrivalSamples = 0
            stats[key]?.intervals = []
        }
        windowStart = now
    }
}
