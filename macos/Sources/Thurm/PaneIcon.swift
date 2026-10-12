import AppKit

/// What a pane runs, as its icon shows it: pane headers draw a monochrome glyph, sidebar rows
/// a small app-style tile.
enum PaneIcon: Equatable {
    case shell
    /// A program other than the shell in the foreground: one the daemon recognized (Neovim,
    /// htop), or any other (nil).
    case program(ProgramMatch?)
    case claude
    case codex
    /// Any other coding agent.
    case agent

    init(_ info: PaneInfo?) {
        switch info?.agent?.kind.lowercased() {
        case "claude"?: self = .claude
        case "codex"?: self = .codex
        case .some: self = .agent
        case .none: self = info?.hasRunningProcess == true ? .program(info?.program) : .shell
        }
    }

    /// A template image (tinted by the view) for pane headers.
    func glyph(size: CGFloat) -> NSImage {
        let key = "\(self)-\(size)"
        if let cached = Self.cache[key] { return cached }
        let image: NSImage
        switch self {
        case .shell:
            image = Self.drawn(size) { rect in
                Self.prompt(in: rect, lineWidth: max(1.2, size / 9)).stroke()
            }
        case .claude, .codex:
            image = Self.drawn(size) { rect in Self.logo(self, in: rect).fill() }
        case .program(let p?) where Self.mark(p) != nil:
            image = Self.drawn(size) { rect in Self.mark(p)?.fill(in: rect) }
        case .program, .agent:
            let config = NSImage.SymbolConfiguration(pointSize: size * 0.85, weight: .medium)
            image = NSImage(systemSymbolName: symbolName, accessibilityDescription: nil)?
                .withSymbolConfiguration(config) ?? NSImage()
        }
        image.isTemplate = true
        Self.cache[key] = image
        return image
    }

    /// A rounded tile with the glyph on it, for sidebar rows.
    func tile(size: CGFloat) -> NSImage {
        let key = "tile-\(self)-\(size)"
        if let cached = Self.cache[key] { return cached }
        let image = NSImage(size: NSSize(width: size, height: size), flipped: true) { rect in
            let radius = size * 0.26
            let shape = NSBezierPath(roundedRect: rect.insetBy(dx: 0.5, dy: 0.5), xRadius: radius, yRadius: radius)
            let (fill, ink): (NSColor, NSColor)
            switch self {
            case .claude: (fill, ink) = (NSColor(srgbRed: 0.85, green: 0.47, blue: 0.34, alpha: 1), .white)
            case .codex: (fill, ink) = (.white, .black)
            case .agent: (fill, ink) = (NSColor(srgbRed: 0.35, green: 0.38, blue: 0.9, alpha: 1), .white)
            case .program(let p?):
                (fill, ink) = (p.color.map { colorFromRGB($0) } ?? NSColor(white: 0.1, alpha: 1),
                               p.ink.map { colorFromRGB($0) } ?? NSColor(white: 0.88, alpha: 1))
            case .shell, .program: (fill, ink) = (NSColor(white: 0.1, alpha: 1), NSColor(srgbRed: 0.35, green: 0.85, blue: 0.4, alpha: 1))
            }
            fill.setFill()
            shape.fill()
            NSColor(white: 1, alpha: self == .codex ? 0 : 0.14).setStroke()
            shape.lineWidth = 1
            shape.stroke()
            let inner = rect.insetBy(dx: size * 0.22, dy: size * 0.22)
            ink.set()
            switch self {
            case .program(let p?):
                if let mark = Self.mark(p) {
                    mark.fill(in: inner)
                } else {
                    Self.symbol(symbolName, in: inner, color: ink)
                }
            case .shell, .program: Self.prompt(in: inner, lineWidth: max(1, size / 14)).stroke()
            case .claude, .codex: Self.logo(self, in: inner).fill()
            case .agent: Self.symbol("sparkle", in: inner, color: ink)
            }
            return true
        }
        Self.cache[key] = image
        return image
    }

    /// The tile for a tab: `stacked` peeks a second tile out behind it (a split tab), and `badge`
    /// puts the agent status on its corner.
    func tile(size: CGFloat, stacked: Bool, badge: AgentStatus?) -> NSImage {
        let front = tile(size: size)
        let offset: CGFloat = stacked ? 4 : 0
        let extra: CGFloat = 2
        let ring = SessionManager.shared.config.sidebarColor
        return NSImage(size: NSSize(width: size + offset + extra, height: size + extra), flipped: true) { _ in
            if stacked {
                front.draw(in: NSRect(x: offset, y: extra, width: size, height: size), from: .zero,
                           operation: .sourceOver, fraction: 0.55)
            }
            front.draw(in: NSRect(x: 0, y: extra, width: size, height: size))
            if let badge {
                let c = NSPoint(x: size - 1.5, y: extra + 1.5)
                ring.setFill()
                NSBezierPath(ovalIn: NSRect(x: c.x - 5, y: c.y - 5, width: 10, height: 10)).fill()
                badge.color.setFill()
                NSBezierPath(ovalIn: NSRect(x: c.x - 3.5, y: c.y - 3.5, width: 7, height: 7)).fill()
            }
            return true
        }
    }

    private static var cache: [String: NSImage] = [:]

    /// SF Symbols for the kinds of program in `programs.rs`' `CATEGORIES`.
    private static let categorySymbols: [String: String] = [
        "editor": "square.and.pencil",
        "runtime": "cube.fill",
        "git": "plusminus.circle",
        "build": "hammer",
        "container": "shippingbox",
        "cluster": "helm",
        "database": "cylinder.split.1x2",
        "monitor": "gauge.with.dots.needle.33percent",
        "multiplexer": "rectangle.split.2x2",
        "remote": "network",
        "files": "folder",
        "docs": "book",
    ]

    /// The SF Symbol drawn for programs (their kind's, or one the config names) and agents.
    private var symbolName: String {
        switch self {
        case .program(let p?):
            let name = Self.categorySymbols[p.icon] ?? p.icon
            return NSImage(systemSymbolName: name, accessibilityDescription: nil) == nil
                ? "gauge.with.dots.needle.33percent" : name
        case .agent: return "sparkle"
        default: return "gauge.with.dots.needle.33percent"
        }
    }

    /// The program's own mark (Neovim's, Sofka's), if it has one.
    private static func mark(_ p: ProgramMatch) -> NSBezierPath? {
        SessionManager.shared.config.programMarks[p.icon].map(parse)
    }

    /// `name` in `color`, as large as fits `rect` without stretching.
    private static func symbol(_ name: String, in rect: NSRect, color: NSColor) {
        let config = NSImage.SymbolConfiguration(pointSize: rect.width, weight: .semibold)
            .applying(.init(paletteColors: [color]))
        guard let image = NSImage(systemSymbolName: name, accessibilityDescription: nil)?
            .withSymbolConfiguration(config) else { return }
        let scale = min(rect.width / image.size.width, rect.height / image.size.height)
        let size = NSSize(width: image.size.width * scale, height: image.size.height * scale)
        image.draw(in: NSRect(x: rect.midX - size.width / 2, y: rect.midY - size.height / 2,
                              width: size.width, height: size.height))
    }

    private static func drawn(_ size: CGFloat, _ draw: @escaping (NSRect) -> Void) -> NSImage {
        NSImage(size: NSSize(width: size, height: size), flipped: true) { rect in
            NSColor.black.set()
            draw(rect)
            return true
        }
    }

    /// `>_`, the shell prompt.
    private static func prompt(in rect: NSRect, lineWidth: CGFloat) -> NSBezierPath {
        let p = NSBezierPath()
        let w = rect.width, h = rect.height
        p.move(to: NSPoint(x: rect.minX + w * 0.1, y: rect.minY + h * 0.2))
        p.line(to: NSPoint(x: rect.minX + w * 0.45, y: rect.minY + h * 0.5))
        p.line(to: NSPoint(x: rect.minX + w * 0.1, y: rect.minY + h * 0.8))
        p.move(to: NSPoint(x: rect.minX + w * 0.55, y: rect.minY + h * 0.82))
        p.line(to: NSPoint(x: rect.minX + w * 0.95, y: rect.minY + h * 0.82))
        p.lineWidth = lineWidth
        p.lineCapStyle = .round
        p.lineJoinStyle = .round
        return p
    }

    /// The agent's mark (24x24 SVG paths), scaled into `rect`.
    private static func logo(_ icon: PaneIcon, in rect: NSRect) -> NSBezierPath {
        let path = parse(icon == .claude ? claudePath : openAIPath)
        path.windingRule = .evenOdd
        var t = AffineTransform(translationByX: rect.minX, byY: rect.minY)
        t.scale(rect.width / 24)
        path.transform(using: t)
        return path
    }

    /// Absolute M/L/C/Z path data, as the constants below and the config's marks hold it.
    private static func parse(_ d: String) -> NSBezierPath {
        let path = NSBezierPath()
        let scanner = Scanner(string: d)
        scanner.charactersToBeSkipped = CharacterSet(charactersIn: " ,")
        func point() -> NSPoint? {
            guard let x = scanner.scanDouble(), let y = scanner.scanDouble() else { return nil }
            return NSPoint(x: x, y: y)
        }
        while let command = scanner.scanCharacter() {
            switch command {
            case "M": if let p = point() { path.move(to: p) }
            case "L": if let p = point() { path.line(to: p) }
            case "C":
                if let c1 = point(), let c2 = point(), let p = point() {
                    path.curve(to: p, controlPoint1: c1, controlPoint2: c2)
                }
            case "Z": path.close()
            default: return path
            }
        }
        return path
    }

    /// Claude's mark, from Simple Icons (CC0), arcs flattened to curves.
    private static let claudePath = "M4.714 15.956L9.432 13.308L9.511 13.078L9.432 12.95L9.201 12.95L8.412 12.902L5.716 12.829L3.379 12.732L1.114 12.61L0.543 12.489L0.009 11.785L0.064 11.432L0.543 11.111L1.229 11.171L2.747 11.275L5.024 11.432L6.675 11.53L9.122 11.785L9.511 11.785L9.565 11.627L9.432 11.53L9.329 11.432L6.973 9.836L4.423 8.148L3.087 7.176L2.365 6.684L2.001 6.223L1.843 5.215L2.498 4.493L3.379 4.553L3.603 4.614L4.496 5.3L6.402 6.776L8.892 8.609L9.256 8.913L9.402 8.809L9.42 8.737L9.256 8.463L7.902 6.017L6.457 3.527L5.813 2.495L5.643 1.876C5.583 1.621 5.54 1.409 5.54 1.147L6.287 0.134L6.7 0L7.695 0.134L8.114 0.498L8.734 1.913L9.735 4.141L11.29 7.17L11.745 8.069L11.988 8.901L12.079 9.156L12.237 9.156L12.237 9.01L12.364 7.304L12.601 5.209L12.832 2.514L12.911 1.755L13.287 0.844L14.034 0.352L14.617 0.631L15.096 1.317L15.03 1.761L14.744 3.612L14.186 6.515L13.821 8.457L14.034 8.457L14.277 8.215L15.26 6.909L16.912 4.845L17.64 4.025L18.49 3.121L19.037 2.69L20.069 2.69L20.828 3.819L20.488 4.985L19.425 6.332L18.545 7.474L17.282 9.174L16.493 10.534L16.566 10.643L16.754 10.625L19.607 10.018L21.149 9.738L22.989 9.423L23.821 9.811L23.912 10.206L23.584 11.013L21.617 11.499L19.31 11.961L15.874 12.774L15.831 12.805L15.88 12.865L17.428 13.011L18.09 13.047L19.711 13.047L22.728 13.272L23.517 13.794L23.991 14.432L23.912 14.917L22.698 15.537L21.058 15.148L17.233 14.237L15.922 13.909L15.74 13.909L15.74 14.019L16.833 15.087L18.836 16.897L21.344 19.228L21.471 19.805L21.15 20.26L20.81 20.212L18.606 18.554L17.756 17.807L15.831 16.186L15.704 16.186L15.704 16.356L16.147 17.006L18.49 20.527L18.612 21.608L18.442 21.96L17.835 22.173L17.167 22.051L15.795 20.127L14.38 17.959L13.239 16.016L13.099 16.095L12.425 23.35L12.109 23.721L11.381 24L10.774 23.539L10.452 22.792L10.774 21.316L11.162 19.392L11.478 17.862L11.763 15.961L11.933 15.33L11.921 15.288L11.781 15.306L10.349 17.273L8.169 20.218L6.445 22.063L6.032 22.227L5.316 21.857L5.382 21.195L5.783 20.606L8.169 17.57L9.608 15.688L10.537 14.602L10.531 14.444L10.476 14.444L4.138 18.56L3.008 18.706L2.523 18.25L2.583 17.504L2.814 17.261L4.721 15.949Z"

    /// OpenAI's mark (Codex), from LobeHub Icons (MIT), arcs flattened to curves.
    private static let openAIPath = "M9.205 8.658L9.205 6.398C9.205 6.208 9.277 6.065 9.443 5.97L13.986 3.354C14.605 2.997 15.342 2.831 16.103 2.831C18.957 2.831 20.765 5.043 20.765 7.397C20.765 7.564 20.765 7.754 20.741 7.944L16.031 5.185C15.903 5.104 15.755 5.06 15.603 5.06C15.451 5.06 15.303 5.104 15.175 5.185L9.205 8.658ZM19.814 17.458L19.814 12.06C19.814 11.727 19.671 11.49 19.385 11.323L13.415 7.85L15.365 6.732C15.436 6.686 15.518 6.661 15.603 6.661C15.688 6.661 15.77 6.686 15.841 6.732L20.384 9.349C21.693 10.109 22.573 11.727 22.573 13.297C22.573 15.105 21.503 16.77 19.813 17.46ZM7.802 12.703L5.852 11.561C5.685 11.466 5.613 11.323 5.613 11.133L5.613 5.899C5.613 3.354 7.563 1.427 10.204 1.427C11.204 1.427 12.131 1.76 12.916 2.355L8.23 5.067C7.945 5.233 7.802 5.471 7.802 5.804L7.802 12.702ZM12 15.128L9.205 13.558L9.205 10.228L12 8.658L14.795 10.228L14.795 13.558L12 15.128ZM13.796 22.358C12.796 22.358 11.869 22.026 11.084 21.431L15.77 18.719C16.055 18.553 16.198 18.315 16.198 17.982L16.198 11.084L18.172 12.226C18.339 12.321 18.41 12.464 18.41 12.654L18.41 17.887C18.41 20.432 16.436 22.359 13.796 22.359ZM8.159 17.055L3.615 14.438C2.307 13.677 1.427 12.06 1.427 10.49C1.424 9.598 1.687 8.725 2.183 7.984C2.679 7.242 3.385 6.665 4.21 6.327L4.21 11.75C4.21 12.083 4.353 12.321 4.638 12.488L10.585 15.937L8.635 17.055C8.564 17.102 8.482 17.126 8.397 17.126C8.312 17.126 8.23 17.102 8.159 17.055ZM7.897 20.955C5.209 20.955 3.235 18.934 3.235 16.436C3.235 16.246 3.259 16.056 3.282 15.866L7.968 18.576C8.254 18.743 8.539 18.743 8.824 18.576L14.794 15.128L14.794 17.388C14.794 17.578 14.724 17.721 14.557 17.816L10.014 20.432C9.395 20.789 8.658 20.955 7.897 20.955ZM13.796 23.785C14.705 23.785 15.601 23.577 16.417 23.176C17.233 22.776 17.946 22.194 18.502 21.475C19.057 20.756 19.441 19.919 19.623 19.029C22.287 18.339 24 15.84 24 13.296C24 11.631 23.287 10.014 22.002 8.848C22.121 8.348 22.192 7.849 22.192 7.35C22.192 3.949 19.433 1.403 16.246 1.403C15.604 1.403 14.986 1.498 14.366 1.713C13.816 1.172 13.167 0.743 12.453 0.449C11.74 0.155 10.976 0.003 10.205 0C9.296 -0 8.399 0.208 7.584 0.609C6.768 1.009 6.055 1.591 5.499 2.311C4.943 3.03 4.56 3.867 4.378 4.757C1.713 5.447 0 7.945 0 10.49C0 12.156 0.713 13.773 1.998 14.938C1.879 15.438 1.808 15.938 1.808 16.437C1.808 19.838 4.567 22.383 7.754 22.383C8.396 22.383 9.014 22.288 9.634 22.074C10.184 22.615 10.834 23.045 11.547 23.338C12.261 23.632 13.024 23.784 13.796 23.787Z"
}

private extension NSBezierPath {
    /// Fills this 24x24 path scaled into `rect`.
    func fill(in rect: NSRect) {
        guard let path = copy() as? NSBezierPath else { return }
        var t = AffineTransform(translationByX: rect.minX, byY: rect.minY)
        t.scale(rect.width / 24)
        path.transform(using: t)
        path.fill()
    }
}
