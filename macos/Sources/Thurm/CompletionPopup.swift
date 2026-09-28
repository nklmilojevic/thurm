import AppKit

/// One tab-completion candidate (`thurm_proto::CompletionItem`).
struct CompletionCandidate {
    let text: String
    let kind: String
    let description: String?

    var isDirectory: Bool { kind == "Directory" || text.hasSuffix("/") }

    var symbol: String {
        switch kind {
        case "Command": return "terminal"
        case "Subcommand": return "chevron.right.square"
        case "Flag": return "flag"
        case "Directory": return "folder"
        case "File": return "doc"
        default: return "text.alignleft"
        }
    }
}

/// Colors and font of the popup: the terminal's theme and font, so it reads as part of the grid.
struct PopupStyle {
    var font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
    var detailFont = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
    var background = NSColor.windowBackgroundColor
    var text = NSColor.labelColor
    var secondary = NSColor.secondaryLabelColor
    var selection = NSColor.selectedContentBackgroundColor
    var selectionText = NSColor.white
    var border = NSColor.separatorColor

    init() {}

    init(config: AppConfig) {
        let t = config.theme
        font = FontShaper.terminalFont(config: config, size: config.fontSize)
        detailFont = FontShaper.terminalFont(config: config, size: max(8, config.fontSize - 2))
        background = colorFromRGB(t.background)
        text = colorFromRGB(t.foreground)
        secondary = colorFromRGB(t.foreground, alpha: 0.55)
        selection = colorFromRGB(t.selectionBackground)
        selectionText = colorFromRGB(t.selectionForeground)
        border = colorFromRGB(t.foreground, alpha: 0.14)
    }
}

/// Selected rows in the theme's selection color instead of the system accent.
final class ThemedRowView: NSTableRowView {
    var style = PopupStyle()

    override func drawSelection(in dirtyRect: NSRect) {
        style.selection.setFill()
        NSBezierPath(roundedRect: bounds.insetBy(dx: 2, dy: 1), xRadius: 5, yRadius: 5).fill()
    }

    override var interiorBackgroundStyle: NSView.BackgroundStyle { .normal }
}

/// The completion list shown under the cursor, after tty7's completion menu. It never takes
/// key focus: the terminal view forwards navigation keys (see `TerminalView.keyDown`).
final class CompletionPopup: NSObject, NSTableViewDataSource, NSTableViewDelegate {
    private let panel: NSPanel
    private let table = NSTableView()
    private let scroll = NSScrollView()
    private let background = NSView(frame: NSRect(x: 0, y: 0, width: 300, height: 100))
    private(set) var items: [CompletionCandidate] = []
    /// The word the items complete (as typed).
    private(set) var word = ""
    var onAccept: ((CompletionCandidate) -> Void)?
    private var style = PopupStyle()

    static let maxRows = 10
    private var rowHeight: CGFloat { ceil(style.font.ascender - style.font.descender + style.font.leading) + 8 }

    override init() {
        panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 300, height: 100),
                        styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: true)
        super.init()
        panel.isFloatingPanel = true
        panel.hasShadow = true
        panel.backgroundColor = .clear
        panel.isOpaque = false
        panel.level = .popUpMenu

        // Real frames from the start: autoresizing from a zero-sized view leaves the scroll
        // view (and the table column in it) with no width, so rows render empty.
        let effect = background
        effect.wantsLayer = true
        effect.layer?.cornerRadius = 8
        effect.layer?.borderWidth = 1
        effect.layer?.masksToBounds = true

        let col = NSTableColumn(identifier: .init("c"))
        col.resizingMask = .autoresizingMask
        table.addTableColumn(col)
        table.columnAutoresizingStyle = .firstColumnOnlyAutoresizingStyle
        table.headerView = nil
        table.rowHeight = 22
        table.selectionHighlightStyle = .regular
        table.intercellSpacing = NSSize(width: 0, height: 0)
        table.backgroundColor = .clear
        table.style = .plain
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.doubleAction = #selector(clicked(_:))
        table.action = #selector(clicked(_:))
        scroll.documentView = table
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.frame = effect.bounds.insetBy(dx: 4, dy: 4)
        scroll.autoresizingMask = [.width, .height]
        effect.addSubview(scroll)
        panel.contentView = effect
    }

    var isVisible: Bool { panel.isVisible }

    var selected: CompletionCandidate? {
        let r = table.selectedRow
        return r >= 0 && r < items.count ? items[r] : nil
    }

    /// Shows `items` under `anchor` (a screen rect: the cursor cell).
    func show(_ items: [CompletionCandidate], word: String, anchor: NSRect, parent: NSWindow) {
        self.items = items
        self.word = word
        style = PopupStyle(config: SessionManager.shared.config)
        background.layer?.backgroundColor = style.background.cgColor
        background.layer?.borderColor = style.border.cgColor
        panel.appearance = NSAppearance(named: SessionManager.shared.config.theme.isDark ? .darkAqua : .aqua)
        table.rowHeight = rowHeight
        table.reloadData()
        table.selectRowIndexes([0], byExtendingSelection: false)
        table.scrollRowToVisible(0)
        let widest = items.prefix(200).map { c -> CGFloat in
            let t = (c.text as NSString).size(withAttributes: [.font: style.font]).width
            let d = c.description.map { ($0 as NSString).size(withAttributes: [.font: style.detailFont]).width } ?? 0
            return 34 + t + (d > 0 ? 16 + min(d, 260) : 0)
        }.max() ?? 200
        let width = min(560, max(220, widest + 16))
        let height = CGFloat(min(items.count, Self.maxRows)) * rowHeight + 8
        // Below the cursor, or above it when there is no room.
        let screen = parent.screen?.visibleFrame ?? .infinite
        var origin = NSPoint(x: anchor.minX - 8, y: anchor.minY - height - 2)
        if origin.y < screen.minY { origin.y = anchor.maxY + 2 }
        origin.x = min(origin.x, screen.maxX - width)
        panel.setFrame(NSRect(origin: origin, size: NSSize(width: width, height: height)), display: false)
        table.tableColumns.first?.width = scroll.contentSize.width
        table.reloadData()
        table.selectRowIndexes([0], byExtendingSelection: false)
        panel.display()
        if panel.parent !== parent {
            panel.parent?.removeChildWindow(panel)
            parent.addChildWindow(panel, ordered: .above)
        }
        panel.orderFront(nil)
    }

    func close() {
        panel.parent?.removeChildWindow(panel)
        panel.orderOut(nil)
        items = []
    }

    func move(_ delta: Int) {
        guard !items.isEmpty else { return }
        let r = (max(0, table.selectedRow) + delta + items.count) % items.count
        table.selectRowIndexes([r], byExtendingSelection: false)
        table.scrollRowToVisible(r)
    }

    @objc private func clicked(_ sender: Any?) {
        let r = table.clickedRow
        guard r >= 0, r < items.count else { return }
        onAccept?(items[r])
    }

    // MARK: Table

    func numberOfRows(in tableView: NSTableView) -> Int { items.count }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        let v = ThemedRowView()
        v.style = style
        return v
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        // Row text follows the selection color.
        table.enumerateAvailableRowViews { rowView, row in
            guard let cell = rowView.view(atColumn: 0) as? NSView else { return }
            let selected = row == table.selectedRow
            for case let label as NSTextField in cell.subviews {
                label.textColor = label.tag == 1 ? (selected ? style.selectionText.withAlphaComponent(0.7) : style.secondary)
                    : (selected ? style.selectionText : style.text)
            }
            for case let icon as NSImageView in cell.subviews {
                icon.contentTintColor = selected ? style.selectionText : style.secondary
            }
        }
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard row < items.count else { return nil }
        let c = items[row]
        let cell = NSView()
        let icon = NSImageView(image: NSImage(systemSymbolName: c.symbol, accessibilityDescription: c.kind) ?? NSImage())
        let selected = row == tableView.selectedRow
        icon.contentTintColor = selected ? style.selectionText : style.secondary
        icon.symbolConfiguration = .init(pointSize: max(9, style.font.pointSize - 2), weight: .regular)
        let name = NSTextField(labelWithString: c.text)
        name.font = style.font
        name.textColor = selected ? style.selectionText : style.text
        name.lineBreakMode = .byTruncatingMiddle
        let desc = NSTextField(labelWithString: c.description ?? "")
        desc.tag = 1
        desc.font = style.detailFont
        desc.textColor = selected ? style.selectionText.withAlphaComponent(0.7) : style.secondary
        desc.lineBreakMode = .byTruncatingTail
        for v in [icon, name, desc] {
            v.translatesAutoresizingMaskIntoConstraints = false
            cell.addSubview(v)
        }
        name.setContentCompressionResistancePriority(.defaultHigh, for: .horizontal)
        desc.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        NSLayoutConstraint.activate([
            icon.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 6),
            icon.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: 16),
            name.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 6),
            name.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
            desc.leadingAnchor.constraint(greaterThanOrEqualTo: name.trailingAnchor, constant: 16),
            desc.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -8),
            desc.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
        ])
        return cell
    }
}
