import AppKit

/// Scrollback search overlay (Cmd+F). Enter searches backwards (older output), Shift+Enter
/// forwards, Esc closes and clears the search.
final class FindBar: NSView, NSSearchFieldDelegate {
    weak var content: TabContentView?

    private let field = NSSearchField()
    private let status = NSTextField(labelWithString: "")
    private var lastQuery = ""

    init() {
        super.init(frame: NSRect(x: 0, y: 0, width: 340, height: 34))
        wantsLayer = true
        layer?.backgroundColor = NSColor.windowBackgroundColor.withAlphaComponent(0.96).cgColor
        layer?.cornerRadius = 8
        layer?.borderWidth = 1
        layer?.borderColor = NSColor.separatorColor.cgColor
        shadow = NSShadow()
        layer?.shadowOpacity = 0.25
        layer?.shadowRadius = 6
        layer?.shadowOffset = CGSize(width: 0, height: -2)

        field.placeholderString = "Find"
        field.delegate = self
        field.sendsWholeSearchString = true
        field.target = self
        field.action = #selector(fieldAction(_:))
        field.translatesAutoresizingMaskIntoConstraints = false

        status.textColor = NSColor.secondaryLabelColor
        status.font = NSFont.systemFont(ofSize: 11)
        status.translatesAutoresizingMaskIntoConstraints = false
        status.setContentHuggingPriority(.required, for: .horizontal)

        let up = FindBar.button(symbol: "chevron.up", fallback: "▲", target: self, action: #selector(findPrevious(_:)))
        up.toolTip = "Find Previous (Enter)"
        let down = FindBar.button(symbol: "chevron.down", fallback: "▼", target: self, action: #selector(findNext(_:)))
        down.toolTip = "Find Next (Shift+Enter)"
        let close = FindBar.button(symbol: "xmark", fallback: "✕", target: self, action: #selector(closeBar(_:)))
        close.toolTip = "Close (Esc)"

        let stack = NSStackView(views: [field, status, up, down, close])
        stack.orientation = .horizontal
        stack.spacing = 4
        stack.edgeInsets = NSEdgeInsets(top: 4, left: 6, bottom: 4, right: 6)
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor),
            stack.topAnchor.constraint(equalTo: topAnchor),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor),
            field.widthAnchor.constraint(greaterThanOrEqualToConstant: 180),
        ])
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    private static func button(symbol: String, fallback: String, target: AnyObject, action: Selector) -> NSButton {
        let button: NSButton
        if let image = NSImage(systemSymbolName: symbol, accessibilityDescription: nil) {
            button = NSButton(image: image, target: target, action: action)
        } else {
            button = NSButton(title: fallback, target: target, action: action)
        }
        button.bezelStyle = .inline
        button.isBordered = false
        button.translatesAutoresizingMaskIntoConstraints = false
        button.widthAnchor.constraint(equalToConstant: 22).isActive = true
        return button
    }

    func activate() {
        window?.makeFirstResponder(field)
        field.selectText(nil)
    }

    // MARK: Actions

    @objc private func fieldAction(_ sender: Any?) {
        // Handled in doCommandBy (Enter / Shift+Enter); typing does not search incrementally
        // to avoid flooding the daemon.
    }

    @objc func findPrevious(_ sender: Any?) {
        search(direction: "Backward")
    }

    @objc func findNext(_ sender: Any?) {
        search(direction: "Forward")
    }

    @objc func closeBar(_ sender: Any?) {
        if let pane = content?.focusedPane {
            Core.shared.send(object: [
                "Search": ["pane": NSNumber(value: pane), "query": NSNull(), "direction": "Backward"],
            ])
        }
        lastQuery = ""
        status.stringValue = ""
        content?.hideFindBar()
    }

    private func search(direction: String) {
        let query = field.stringValue
        guard !query.isEmpty, let pane = content?.focusedPane else { return }
        lastQuery = query
        let resp = Core.shared.request(object: [
            "Search": ["pane": NSNumber(value: pane), "query": query, "direction": direction],
        ])
        if let v = JSON.variant(resp), v.name == "Search",
           let payload = v.payload as? [String: Any], let found = jsonBool(payload["found"]) {
            status.stringValue = found ? "" : "No matches"
        }
        content?.views[pane]?.needsRender = true
    }

    // MARK: NSSearchFieldDelegate / NSControlTextEditingDelegate

    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        if commandSelector == #selector(NSResponder.insertNewline(_:)) {
            let shift = NSApp.currentEvent?.modifierFlags.contains(.shift) ?? false
            search(direction: shift ? "Forward" : "Backward")
            return true
        }
        if commandSelector == #selector(NSResponder.cancelOperation(_:)) {
            closeBar(nil)
            return true
        }
        return false
    }
}
