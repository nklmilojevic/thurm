import AppKit

/// Borderless panels cannot become key by default.
private final class PalettePanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }
    var onKeyEquivalent: ((NSEvent) -> Bool)?

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if onKeyEquivalent?(event) == true { return true }
        return super.performKeyEquivalent(with: event)
    }
}

/// Cmd+Shift+P command palette: a search field over a fuzzy filtered list of actions.
final class CommandPalette: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSSearchFieldDelegate,
    NSWindowDelegate {
    static let shared = CommandPalette()

    struct Item {
        let title: String
        let detail: String
        /// Shown right-aligned ("⇧⌘D").
        let shortcut: String
        /// ⌘R on the item (the workspace switcher: rename).
        let rename: (() -> Void)?
        /// Runs when the item becomes the selected row (the theme picker: show it live).
        let preview: (() -> Void)?
        let action: () -> Void

        init(title: String, detail: String, shortcut: String = "", rename: (() -> Void)? = nil,
             preview: (() -> Void)? = nil, action: @escaping () -> Void) {
            self.title = title
            self.detail = detail
            self.shortcut = shortcut
            self.rename = rename
            self.preview = preview
            self.action = action
        }
    }

    /// The main menu's commands: submenus become a title prefix ("Theme: Nord"), the detail
    /// is the shortcut and the top-level menu. Validated first, so disabled ones are left out
    /// and toggles carry their current title ("Show Sidebar").
    static func menuCommands(_ menu: NSMenu?) -> [Item] {
        guard let menu else { return [] }
        var out: [Item] = []
        func walk(_ m: NSMenu, path: [String], top: String) {
            m.delegate?.menuNeedsUpdate?(m)
            m.update()
            for item in m.items where !item.isSeparatorItem && !item.isHidden {
                if let sub = item.submenu {
                    // Services and the window list are the system's, not ours.
                    if sub === NSApp.servicesMenu || item.title == "Services" { continue }
                    walk(sub, path: top.isEmpty ? [] : path + [item.title], top: top.isEmpty ? item.title : top)
                    continue
                }
                guard let action = item.action, item.isEnabled,
                      action != #selector(AppDelegate.showCommandPalette(_:)),
                      action != #selector(NSWindow.makeKeyAndOrderFront(_:))
                else { continue }
                let title = (path + [item.title]).joined(separator: ": ")
                out.append(Item(title: title, detail: top, shortcut: shortcut(item)) { [weak item] in
                    guard let item else { return }
                    NSApp.sendAction(action, to: item.target, from: item)
                })
            }
        }
        // The app menu (About, Hide, Quit) last: rarely what one is looking for.
        for top in menu.items.dropFirst() + menu.items.prefix(1) {
            if let sub = top.submenu { walk(sub, path: [], top: top.title.isEmpty ? "Thurm" : top.title) }
        }
        return out
    }

    /// "⇧⌘D", as the menu shows it.
    static func shortcut(_ item: NSMenuItem) -> String {
        let key = item.keyEquivalent
        guard !key.isEmpty else { return "" }
        let mods = item.keyEquivalentModifierMask
        var s = ""
        if mods.contains(.control) { s += "⌃" }
        if mods.contains(.option) { s += "⌥" }
        if mods.contains(.shift) || (key.count == 1 && key != key.lowercased()) { s += "⇧" }
        if mods.contains(.command) { s += "⌘" }
        let named: [String: String] = [
            "\r": "↩", "\t": "⇥", " ": "Space", "\u{1b}": "⎋", "\u{8}": "⌫", "\u{7f}": "⌫",
            String(UnicodeScalar(NSUpArrowFunctionKey)!): "↑", String(UnicodeScalar(NSDownArrowFunctionKey)!): "↓",
            String(UnicodeScalar(NSLeftArrowFunctionKey)!): "←", String(UnicodeScalar(NSRightArrowFunctionKey)!): "→",
        ]
        return s + (named[key] ?? key.uppercased())
    }

    private var panel: PalettePanel?
    private var field: NSTextField?
    private var background: NSView?
    private var separator: NSView?
    private var footer: NSTextField?
    private var style = PopupStyle()
    private var table: NSTableView?
    private var allItems: [Item] = []
    /// Called when the palette closes without running an item (Escape, clicking away).
    private var onCancel: (() -> Void)?
    /// The item whose preview ran last, so selecting it again doesn't rerun it.
    private var previewed: String?
    private var filtered: [Item] = []
    private weak var targetWindow: NSWindow?

    var isVisible: Bool { panel?.isVisible ?? false }

    // MARK: Showing

    func show(items: [Item], over window: NSWindow?, placeholder: String = "Type a command…",
              footer hint: String? = nil, initialRow: Int = 0, onCancel: (() -> Void)? = nil) {
        cancelIfOpen()
        allItems = items
        filtered = items
        targetWindow = window
        self.onCancel = onCancel
        previewed = items.indices.contains(initialRow) ? items[initialRow].title : nil
        let panel = self.panel ?? buildPanel()
        applyStyle(placeholder: placeholder, footer: hint)
        field?.stringValue = ""
        table?.reloadData()
        selectRow(initialRow)

        let size = panel.frame.size
        var origin = NSPoint(x: 200, y: 400)
        if let w = window {
            origin = NSPoint(x: w.frame.midX - size.width / 2, y: w.frame.maxY - size.height - 80)
        } else if let screen = NSScreen.main {
            origin = NSPoint(x: screen.visibleFrame.midX - size.width / 2, y: screen.visibleFrame.maxY - size.height - 120)
        }
        panel.setFrameOrigin(origin)
        panel.makeKeyAndOrderFront(nil)
        if let field = field { panel.makeFirstResponder(field) }
    }

    func close() {
        panel?.orderOut(nil)
    }

    /// Closed without choosing: let the caller undo its previews.
    private func cancelIfOpen() {
        let cancel = onCancel
        onCancel = nil
        cancel?()
    }

    private func buildPanel() -> PalettePanel {
        let rect = NSRect(x: 0, y: 0, width: 560, height: 340)
        let panel = PalettePanel(contentRect: rect, styleMask: [.borderless, .fullSizeContentView],
                                 backing: .buffered, defer: false)
        panel.isFloatingPanel = true
        panel.level = .floating
        panel.hasShadow = true
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.delegate = self
        panel.hidesOnDeactivate = true
        panel.isReleasedWhenClosed = false

        // Themed like the terminal (see `applyStyle`).
        let effect = NSView(frame: rect)
        effect.wantsLayer = true
        effect.layer?.cornerRadius = 10
        effect.layer?.borderWidth = 1
        effect.layer?.masksToBounds = true
        effect.autoresizingMask = [.width, .height]
        panel.contentView = effect
        background = effect

        let field = NSTextField(frame: NSRect(x: 16, y: rect.height - 42, width: rect.width - 32, height: 26))
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.delegate = self
        field.autoresizingMask = [.width, .minYMargin]
        effect.addSubview(field)

        let line = NSView(frame: NSRect(x: 0, y: rect.height - 52, width: rect.width, height: 1))
        line.wantsLayer = true
        line.autoresizingMask = [.width, .minYMargin]
        effect.addSubview(line)
        separator = line

        let hint = NSTextField(labelWithString: "")
        hint.frame = NSRect(x: 16, y: 6, width: rect.width - 32, height: 16)
        hint.autoresizingMask = [.width, .maxYMargin]
        effect.addSubview(hint)
        footer = hint

        let scroll = NSScrollView(frame: NSRect(x: 0, y: 8, width: rect.width, height: rect.height - 64))
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        scroll.autoresizingMask = [.width, .height]

        let table = NSTableView(frame: scroll.bounds)
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("command"))
        column.width = rect.width - 20
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)
        table.headerView = nil
        table.rowHeight = 26
        table.backgroundColor = .clear
        table.selectionHighlightStyle = .regular
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.doubleAction = #selector(runSelected)
        table.action = #selector(tableClicked)
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        scroll.documentView = table
        effect.addSubview(scroll)

        panel.onKeyEquivalent = { [weak self] event in
            guard let self, event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command,
                  event.charactersIgnoringModifiers == "r", let table = self.table,
                  table.selectedRow >= 0, table.selectedRow < self.filtered.count,
                  let rename = self.filtered[table.selectedRow].rename
            else { return false }
            self.close()
            self.targetWindow?.makeKeyAndOrderFront(nil)
            rename()
            return true
        }
        self.panel = panel
        self.field = field
        self.table = table
        return panel
    }

    /// Terminal theme and font, re-read on every show (themes and fonts change).
    private func applyStyle(placeholder: String, footer hint: String?) {
        style = PopupStyle(config: SessionManager.shared.config)
        panel?.appearance = NSAppearance(named: SessionManager.shared.config.theme.isDark ? .darkAqua : .aqua)
        background?.layer?.backgroundColor = style.background.cgColor
        background?.layer?.borderColor = style.border.cgColor
        separator?.layer?.backgroundColor = style.border.cgColor
        let big = NSFont(descriptor: style.font.fontDescriptor, size: style.font.pointSize + 2) ?? style.font
        field?.font = big
        field?.textColor = style.text
        (field?.cell as? NSTextFieldCell)?.placeholderAttributedString = NSAttributedString(
            string: placeholder, attributes: [.font: big, .foregroundColor: style.secondary])
        footer?.stringValue = hint ?? ""
        footer?.font = style.detailFont
        footer?.textColor = style.secondary
        footer?.isHidden = hint == nil
        if let scroll = table?.enclosingScrollView, let bounds = background?.bounds {
            let bottom: CGFloat = hint == nil ? 8 : 26
            scroll.frame = NSRect(x: 0, y: bottom, width: bounds.width, height: bounds.height - 56 - bottom)
        }
        table?.rowHeight = ceil(style.font.ascender - style.font.descender + style.font.leading) + 12
    }

    // MARK: Filtering

    /// Subsequence match; substring hits rank first.
    private static func score(_ title: String, _ query: String) -> Int? {
        if query.isEmpty { return 0 }
        let t = title.lowercased()
        let q = query.lowercased()
        if let r = t.range(of: q) {
            return t.distance(from: t.startIndex, to: r.lowerBound)
        }
        var ti = t.startIndex
        var gaps = 0
        for qc in q {
            guard let found = t[ti...].firstIndex(of: qc) else { return nil }
            gaps += t.distance(from: ti, to: found)
            ti = t.index(after: found)
        }
        return 1000 + gaps
    }

    private func applyFilter() {
        let query = field?.stringValue.trimmingCharacters(in: .whitespaces) ?? ""
        let scored: [(Int, Int, Item)] = allItems.enumerated().compactMap { entry in
            guard let s = CommandPalette.score(entry.element.title, query) else { return nil }
            return (s, entry.offset, entry.element)
        }
        filtered = scored.sorted { a, b in a.0 != b.0 ? a.0 < b.0 : a.1 < b.1 }.map { $0.2 }
        table?.reloadData()
        selectRow(0)
    }

    private func selectRow(_ row: Int) {
        guard let table = table, !filtered.isEmpty else { return }
        let r = max(0, min(filtered.count - 1, row))
        table.selectRowIndexes(IndexSet(integer: r), byExtendingSelection: false)
        table.scrollRowToVisible(r)
    }

    @objc private func tableClicked() {
        // Single click only selects; double click / Enter runs.
    }

    @objc private func runSelected() {
        guard let table = table else { return }
        let row = table.selectedRow
        guard row >= 0, row < filtered.count else { return }
        let item = filtered[row]
        onCancel = nil
        close()
        if let w = targetWindow {
            w.makeKeyAndOrderFront(nil)
        }
        item.action()
    }

    // MARK: NSTableViewDataSource / Delegate

    func numberOfRows(in tableView: NSTableView) -> Int {
        filtered.count
    }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        let v = ThemedRowView()
        v.style = style
        return v
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        guard let table else { return }
        let row = table.selectedRow
        if row >= 0, row < filtered.count, filtered[row].title != previewed {
            previewed = filtered[row].title
            filtered[row].preview?()
        }
        table.enumerateAvailableRowViews { _, row in
            if let cell = table.view(atColumn: 0, row: row, makeIfNecessary: false) as? PaletteCell {
                cell.apply(style, selected: row == table.selectedRow)
            }
        }
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard row >= 0, row < filtered.count else { return nil }
        let item = filtered[row]
        let id = NSUserInterfaceItemIdentifier("PaletteCell")
        let cell = tableView.makeView(withIdentifier: id, owner: self) as? PaletteCell ?? PaletteCell()
        cell.identifier = id
        cell.title.stringValue = item.title
        cell.detail.stringValue = item.detail
        cell.shortcut.stringValue = item.shortcut
        cell.apply(style, selected: row == tableView.selectedRow)
        return cell
    }

    // MARK: Field delegate

    func controlTextDidChange(_ obj: Notification) {
        applyFilter()
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        guard let table = table else { return false }
        switch commandSelector {
        case #selector(NSResponder.moveDown(_:)):
            selectRow(table.selectedRow + 1)
            return true
        case #selector(NSResponder.moveUp(_:)):
            selectRow(table.selectedRow - 1)
            return true
        case #selector(NSResponder.insertNewline(_:)):
            runSelected()
            return true
        case #selector(NSResponder.cancelOperation(_:)):
            close()
            cancelIfOpen()
            targetWindow?.makeKeyAndOrderFront(nil)
            return true
        default:
            return false
        }
    }

    // MARK: NSWindowDelegate

    func windowDidResignKey(_ notification: Notification) {
        close()
        cancelIfOpen()
    }
}

/// Title, then a dim detail; the shortcut right-aligned.
private final class PaletteCell: NSView {
    let title = NSTextField(labelWithString: "")
    let detail = NSTextField(labelWithString: "")
    let shortcut = NSTextField(labelWithString: "")

    init() {
        super.init(frame: .zero)
        for v in [title, detail, shortcut] {
            v.translatesAutoresizingMaskIntoConstraints = false
            v.lineBreakMode = .byTruncatingTail
            addSubview(v)
        }
        title.setContentCompressionResistancePriority(.defaultHigh, for: .horizontal)
        detail.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        shortcut.setContentCompressionResistancePriority(.required, for: .horizontal)
        shortcut.alignment = .right
        NSLayoutConstraint.activate([
            title.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 14),
            title.centerYAnchor.constraint(equalTo: centerYAnchor),
            detail.leadingAnchor.constraint(equalTo: title.trailingAnchor, constant: 10),
            detail.firstBaselineAnchor.constraint(equalTo: title.firstBaselineAnchor),
            detail.trailingAnchor.constraint(lessThanOrEqualTo: shortcut.leadingAnchor, constant: -10),
            shortcut.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -14),
            shortcut.firstBaselineAnchor.constraint(equalTo: title.firstBaselineAnchor),
        ])
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    func apply(_ style: PopupStyle, selected: Bool) {
        title.font = style.font
        detail.font = style.detailFont
        shortcut.font = style.detailFont
        title.textColor = selected ? style.selectionText : style.text
        let dim = selected ? style.selectionText.withAlphaComponent(0.7) : style.secondary
        detail.textColor = dim
        shortcut.textColor = dim
    }
}
