import QuartzCore

/// A layout move the render server plays. The app lays the move out up front, once per display
/// frame, and records what each layer looks like in that frame; Core Animation then shows the
/// frames as discrete keyframes. Once committed, the move needs no work from the app per frame,
/// so it stays smooth while the main thread or the whole machine is busy (Rex moves its panes
/// with Core Animation too). Any layout code can be recorded, however it draws: the gutter's
/// path, built with boolean path operations, has no shape Core Animation could interpolate.
struct BakedMotion {
    private struct Track {
        let layer: CALayer
        var values: [String: [AnyObject]]
    }

    static let keyPrefix = "thurm.move."

    private var tracks: [ObjectIdentifier: Track] = [:]
    private var order: [ObjectIdentifier] = []
    private(set) var frames = 0

    /// Records `root` and every layer under it as the next frame.
    mutating func record(_ root: CALayer) {
        var seen = Set<ObjectIdentifier>()
        visit(root, seen: &seen)
        // A layer gone from the tree keeps its last look; it is not drawn anyway.
        for id in order where !seen.contains(id) {
            for key in tracks[id]!.values.keys {
                tracks[id]!.values[key]!.append(tracks[id]!.values[key]!.last!)
            }
        }
        frames += 1
    }

    private mutating func visit(_ layer: CALayer, seen: inout Set<ObjectIdentifier>) {
        let id = ObjectIdentifier(layer)
        seen.insert(id)
        let now = Self.properties(of: layer)
        if tracks[id] == nil {
            // A layer that appears during the move is hidden in the frames before it.
            var values = now.mapValues { value in [AnyObject](repeating: value, count: frames) }
            values["hidden"] = [AnyObject](repeating: NSNumber(value: true), count: frames)
            tracks[id] = Track(layer: layer, values: values)
            order.append(id)
        }
        for (key, value) in now {
            tracks[id]!.values[key]!.append(value)
        }
        for sublayer in layer.sublayers ?? [] {
            visit(sublayer, seen: &seen)
        }
    }

    /// The properties a layout move changes.
    static func properties(of layer: CALayer) -> [String: AnyObject] {
        var values: [String: AnyObject] = [
            "position": NSValue(point: layer.position),
            "bounds": NSValue(rect: layer.bounds),
            "opacity": NSNumber(value: layer.opacity),
            "hidden": NSNumber(value: layer.isHidden),
            "backgroundColor": layer.backgroundColor ?? CGColor.clear,
        ]
        if let shape = layer as? CAShapeLayer {
            values["path"] = shape.path ?? CGPath(rect: .zero, transform: nil)
            values["fillColor"] = shape.fillColor ?? CGColor.clear
            values["strokeColor"] = shape.strokeColor ?? CGColor.clear
            values["lineWidth"] = NSNumber(value: Double(shape.lineWidth))
        }
        return values
    }

    /// Adds the recorded frames to their layers from `begin` (in `CACurrentMediaTime`) for
    /// `duration` seconds, at up to `fps` frames a second (left to itself, Core Animation drops
    /// to 60 under load). A property that holds still gets no animation, and once the move
    /// ends each layer shows its own (model) value again. Returns the layers it animated.
    func play(begin: CFTimeInterval, duration: TimeInterval, fps: Float = 60) -> [CALayer] {
        guard frames > 0, duration > 0 else { return [] }
        let rate = CAFrameRateRange(minimum: min(60, fps), maximum: fps, preferred: fps)
        let keyTimes = (0...frames).map { NSNumber(value: Double($0) / Double(frames)) }
        var animated: [CALayer] = []
        for id in order {
            let track = tracks[id]!
            var moved = false
            for (key, values) in track.values where values.contains(where: { !CFEqual($0, values[0]) }) {
                let animation = CAKeyframeAnimation(keyPath: key)
                animation.values = values
                animation.keyTimes = keyTimes
                animation.calculationMode = .discrete
                animation.duration = duration
                animation.beginTime = track.layer.convertTime(begin, from: nil)
                animation.fillMode = .backwards
                animation.preferredFrameRateRange = rate
                track.layer.add(animation, forKey: Self.keyPrefix + key)
                moved = true
            }
            if moved { animated.append(track.layer) }
        }
        return animated
    }

    /// Takes a move's frames off `layers` (an interrupted or settled move).
    static func stop(_ layers: [CALayer]) {
        for layer in layers {
            for key in layer.animationKeys() ?? [] where key.hasPrefix(keyPrefix) {
                layer.removeAnimation(forKey: key)
            }
        }
    }
}
