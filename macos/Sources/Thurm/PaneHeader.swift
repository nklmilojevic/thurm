import AppKit

extension AgentStatus {
    /// The status dot's color (sidebar rows, pane headers).
    var color: NSColor {
        switch self {
        case .working: return .systemBlue
        case .needsInput: return .systemOrange
        case .done: return .systemGreen
        case .error: return .systemRed
        case .idle: return .systemGray
        }
    }
}

/// The strip above a split pane (`window.pane_headers`): what runs in it, and buttons to split,
/// zoom or close it. Clicking it focuses the pane; double-clicking zooms it.
final class PaneHeader: NSView {
    static let height: CGFloat = 30

    let pane: PaneKey
    weak var content: TabContentView?

    private let icon = NSImageView()
    private let dot = NSView()
    private let title = NSTextField(labelWithString: "")
    private let buttons = NSStackView()
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
        buttons.spacing = 2
        for (symbol, label, action) in [
            ("rectangle.split.2x1", "Split Right", #selector(splitRight)),
            ("rectangle.split.1x2", "Split Down", #selector(splitDown)),
            ("arrow.up.left.and.arrow.down.right", "Zoom", #selector(zoom)),
            ("xmark", "Close Pane", #selector(close)),
        ] {
            let b = NSButton(image: NSImage(systemSymbolName: symbol, accessibilityDescription: label)!,
                             target: self, action: action)
            b.isBordered = false
            b.symbolConfiguration = .init(pointSize: 12, weight: .regular)
            b.toolTip = label
            b.widthAnchor.constraint(equalToConstant: 22).isActive = true
            buttons.addArrangedSubview(b)
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
            buttons.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
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
        title.textColor = fg.withAlphaComponent(focused ? 1 : 0.6)
        icon.contentTintColor = fg.withAlphaComponent(focused ? 0.9 : 0.55)
        for case let b as NSButton in buttons.arrangedSubviews {
            b.contentTintColor = fg.withAlphaComponent(focused ? 0.45 : 0.3)
        }
        let status = info?.agent?.status
        dot.isHidden = status == nil || status == .idle
        dot.layer?.backgroundColor = status?.color.cgColor
    }

    override func mouseDown(with event: NSEvent) {
        guard let content else { return }
        if event.clickCount == 2 {
            content.focus(pane)
            content.toggleZoom()
        } else {
            content.focus(pane)
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
    @objc private func zoom() { act { $0.toggleZoom(nil) } }
    @objc private func close() { act { $0.closePane(nil) } }
}
