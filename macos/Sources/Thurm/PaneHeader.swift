import AppKit

extension AgentStatus {
    /// The status dot's color (sidebar rows, pane headers, tabs).
    var color: NSColor {
        let chrome = SessionManager.shared.config.chrome
        switch self {
        case .working: return chrome.info
        case .needsInput: return chrome.warning
        case .done: return chrome.success
        case .error: return chrome.danger
        case .idle: return chrome.contentTertiary
        }
    }
}

/// The strip above a split pane (`window.pane_headers`): what runs in it, and buttons to split,
/// float, zoom or close it. Clicking it focuses the pane; double-clicking zooms it. On a floating
/// pane it is always shown, and dragging it moves the pane.
final class PaneHeader: NSView {
    static let height: CGFloat = 30

    let pane: PaneKey
    weak var content: TabContentView?

    private let icon = NSImageView()
    private let dot = NSView()
    private let title = NSTextField(labelWithString: "")
    private let buttons = NSStackView()
    private var splitButtons: [NSButton] = []
    private var floatButton: NSButton?
    private var focused = false
    private var iconKind: PaneIcon?
    private lazy var iconLeading = icon.leadingAnchor.constraint(equalTo: leadingAnchor, constant: Self.leading)

    static let leading: CGFloat = 10

    /// Where the icon starts: past the titlebar's controls when they overlap the header.
    var leadingInset: CGFloat {
        get { iconLeading.constant }
        set { if iconLeading.constant != newValue { iconLeading.constant = newValue } }
    }

    init(pane: PaneKey) {
        self.pane = pane
        super.init(frame: .zero)
        icon.imageScaling = .scaleProportionallyDown
        dot.wantsLayer = true
        dot.layer?.cornerRadius = 3
        title.font = .systemFont(ofSize: 13, weight: .semibold)
        title.lineBreakMode = .byTruncatingTail
        title.maximumNumberOfLines = 1
        title.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        buttons.orientation = .horizontal
        buttons.spacing = 0
        for (symbol, label, action) in [
            ("rectangle.split.2x1", "Split Right", #selector(splitRight)),
            ("rectangle.split.1x2", "Split Down", #selector(splitDown)),
            ("pip.exit", "Float Pane", #selector(toggleFloat)),
            ("arrow.up.left.and.arrow.down.right", "Zoom", #selector(zoom)),
            ("xmark", "Close Pane", #selector(close)),
        ] {
            let b = HeaderButton(image: NSImage(systemSymbolName: symbol, accessibilityDescription: label)!,
                                 target: self, action: action)
            b.isBordered = false
            b.symbolConfiguration = .init(pointSize: 12, weight: .regular)
            b.toolTip = label
            // The whole header-high cell takes the click, not just the glyph.
            b.widthAnchor.constraint(equalToConstant: 26).isActive = true
            b.heightAnchor.constraint(equalToConstant: Self.height).isActive = true
            buttons.addArrangedSubview(b)
            if action == #selector(splitRight) || action == #selector(splitDown) { splitButtons.append(b) }
            if action == #selector(toggleFloat) { floatButton = b }
        }
        for v in [icon, dot, title, buttons] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        NSLayoutConstraint.activate([
            iconLeading,
            icon.centerYAnchor.constraint(equalTo: centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: 15),
            icon.heightAnchor.constraint(equalToConstant: 15),
            dot.widthAnchor.constraint(equalToConstant: 6),
            dot.heightAnchor.constraint(equalToConstant: 6),
            dot.trailingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 2),
            dot.bottomAnchor.constraint(equalTo: icon.bottomAnchor, constant: 1),
            title.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 7),
            title.centerYAnchor.constraint(equalTo: centerYAnchor),
            title.trailingAnchor.constraint(lessThanOrEqualTo: buttons.leadingAnchor, constant: -8),
            buttons.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -6),
            buttons.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
        update(focused: false)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var isFlipped: Bool { true }

    /// Refreshes the icon, title and colors from the pane's info and the theme.
    func update(focused: Bool) {
        self.focused = focused
        let cfg = SessionManager.shared.config
        let info = SessionManager.shared.panes[pane]
        let kind = PaneIcon(info)
        if kind != iconKind {
            iconKind = kind
            icon.image = kind.glyph(size: 15)
        }
        var text = info?.displayTitle ?? ""
        if let agent = info?.agent, text.isEmpty || text == "Thurm" { text = agent.name }
        if pane.isRemote { text = "\(pane.host) · \(text)" }
        if title.stringValue != text { title.stringValue = text }
        toolTip = [text, info?.cwd].compactMap { $0 }.joined(separator: "\n")

        let fg = colorFromRGB(cfg.theme.foreground)
        title.textColor = focused ? cfg.chrome.contentPrimary : cfg.chrome.contentSecondary
        icon.contentTintColor = fg.withAlphaComponent(focused ? 0.9 : 0.55)
        for case let b as HeaderButton in buttons.arrangedSubviews {
            b.tint = fg.withAlphaComponent(focused ? 0.45 : 0.3)
            b.hoverTint = fg.withAlphaComponent(0.9)
            b.hoverFill = fg.withAlphaComponent(0.1)
        }
        let status = info?.agent?.status
        dot.isHidden = status == nil || status == .idle
        dot.layer?.backgroundColor = status?.color.cgColor
        // A floating pane docks rather than splits; the splits' only pane cannot float.
        let floats = content?.isFloating(pane) ?? false
        for b in splitButtons where b.isHidden != floats { b.isHidden = floats }
        if let b = floatButton {
            let symbol = floats ? "pip.enter" : "pip.exit"
            if b.toolTip != (floats ? "Dock Pane" : "Float Pane") {
                b.image = NSImage(systemSymbolName: symbol, accessibilityDescription: floats ? "Dock Pane" : "Float Pane")
                b.toolTip = floats ? "Dock Pane" : "Float Pane"
            }
            let hidden = !floats && !(content?.canFloat(pane) ?? false)
            if b.isHidden != hidden { b.isHidden = hidden }
        }
    }

    override func mouseDown(with event: NSEvent) {
        guard let content else { return }
        if event.clickCount == 2 {
            content.focus(pane)
            content.toggleZoom()
        } else {
            content.focus(pane)
            if content.isFloating(pane) { content.trackCardDrag(pane, from: event) }
        }
    }

    /// Header buttons act on their own pane, which the window's actions take as the focused one.
    private func act(_ action: (TerminalWindowController) -> Void) {
        guard let content, let controller = content.controller else { return }
        content.focus(pane)
        action(controller)
    }

    @objc private func splitRight() { act { $0.splitRight(nil) } }
    @objc private func splitDown() { act { $0.splitDown(nil) } }
    @objc private func toggleFloat() { act { $0.toggleFloat(nil) } }
    @objc private func zoom() { act { $0.toggleZoom(nil) } }
    @objc private func close() { act { $0.closePane(nil) } }
}

/// A pane header button: brightens and shows a rounded backdrop while the pointer is over it, so
/// its (header-high) click target is visible.
private final class HeaderButton: NSButton {
    var tint: NSColor = .secondaryLabelColor { didSet { refresh() } }
    var hoverTint: NSColor = .labelColor { didSet { refresh() } }
    var hoverFill: NSColor = .clear { didSet { refresh() } }
    private var hovering = false {
        didSet { if hovering != oldValue { refresh() } }
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeInActiveApp, .inVisibleRect],
                                       owner: self))
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func mouseEntered(with event: NSEvent) { hovering = true }
    override func mouseExited(with event: NSEvent) { hovering = false }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if window == nil { hovering = false }
    }

    override func layout() {
        super.layout()
        // The backdrop is a square around the glyph, not the full header-high cell.
        let side = min(bounds.width, 22)
        backdrop.frame = NSRect(x: (bounds.width - side) / 2, y: (bounds.height - side) / 2, width: side, height: side)
    }

    private lazy var backdrop: CALayer = {
        let l = CALayer()
        l.cornerRadius = 5
        layer?.insertSublayer(l, at: 0)
        return l
    }()

    private func refresh() {
        contentTintColor = hovering ? hoverTint : tint
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        backdrop.backgroundColor = hovering ? hoverFill.cgColor : nil
        CATransaction.commit()
    }
}
