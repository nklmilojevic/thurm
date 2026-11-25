import AppKit

private final class WorkspacePickerPanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }
}

private final class WorkspacePickerContent: NSView {
    override var isFlipped: Bool { true }
}

/// The workspace button's anchored picker, shared by the titlebar and sidebar.
/// The menu bar and its shortcuts continue to use the standard workspace menu.
final class WorkspacePicker: NSObject, NSTextFieldDelegate, NSWindowDelegate {
    static let shared = WorkspacePicker()

    private var panel: WorkspacePickerPanel?
    private let search = NSTextField()
    private let searchIcon = NSImageView()
    private let clearSearch = NSButton()
    private let searchBackground = NSView()
    private let scroll = NSScrollView()
    private let list = WorkspacePickerContent()
    private var rows: [WorkspacePickerRow] = []
    private var selected = 0
    private weak var anchor: NSView?
    private weak var targetWindow: NSWindow?
    private var clickMonitor: Any?
    private var observers: [NSObjectProtocol] = []
    private let announceSelection: (String) -> Void
    private static let width: CGFloat = 248
    private static let inset: CGFloat = 8
    private static let listTop: CGFloat = 48

    var isVisible: Bool { panel?.isVisible == true }

    init(announceSelection: @escaping (String) -> Void = { title in
        NSAccessibility.post(element: NSApplication.shared, notification: .announcementRequested, userInfo: [
            .announcement: title,
            .priority: NSAccessibilityPriorityLevel.high.rawValue,
        ])
    }) {
        self.announceSelection = announceSelection
        super.init()
    }

    func toggle(relativeTo view: NSView) {
        if isVisible { close(); return }
        guard let window = view.window else { return }
        anchor = view
        targetWindow = window
        let panel = self.panel ?? buildPanel()
        let chrome = SessionManager.shared.config.chrome
        panel.appearance = NSAppearance(named: chrome.isDark ? .darkAqua : .aqua)
        panel.contentView?.layer?.backgroundColor = chrome.surfaceRaised.cgColor
        searchBackground.layer?.backgroundColor = chrome.control.cgColor
        search.textColor = chrome.contentPrimary
        searchIcon.contentTintColor = chrome.contentSecondary
        clearSearch.contentTintColor = chrome.contentSecondary
        search.placeholderAttributedString = NSAttributedString(string: "Filter workspaces…", attributes: [
            .font: NSFont.systemFont(ofSize: 13), .foregroundColor: chrome.contentSecondary,
        ])
        search.stringValue = ""
        reload()
        window.addChildWindow(panel, ordered: .above)
        panel.makeKeyAndOrderFront(nil)
        panel.makeFirstResponder(search)

        // Consume the dismissing click, just as a menu does; it must not reach the terminal
        // or reopen the picker when the user clicks its button a second time.
        clickMonitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown, .otherMouseDown]) {
            [weak self] event in
            guard let self, self.isVisible, event.window !== self.panel else { return event }
            self.close(restoreFocus: event.window === self.targetWindow)
            return nil
        }
        for name in [NSWindow.willCloseNotification, NSWindow.willMoveNotification, NSWindow.didResizeNotification] {
            observers.append(NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) {
                [weak self] _ in self?.close(restoreFocus: false)
            })
        }
        observers.append(NotificationCenter.default.addObserver(
            forName: NSApplication.didResignActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in self?.close(restoreFocus: false) })
    }

    func close(restoreFocus: Bool = true) {
        if let clickMonitor { NSEvent.removeMonitor(clickMonitor) }
        clickMonitor = nil
        for observer in observers { NotificationCenter.default.removeObserver(observer) }
        observers.removeAll()
        guard let panel, panel.isVisible else { return }
        panel.parent?.removeChildWindow(panel)
        panel.orderOut(nil)
        if restoreFocus { targetWindow?.makeKeyAndOrderFront(nil) }
    }

    private func buildPanel() -> WorkspacePickerPanel {
        let panel = WorkspacePickerPanel(contentRect: NSRect(x: 0, y: 0, width: Self.width, height: 240),
                                         styleMask: [.borderless], backing: .buffered, defer: false)
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.isReleasedWhenClosed = false
        // The deactivation observer dismisses it permanently, including the child-window
        // relationship, rather than letting AppKit hide and later reshow it.
        panel.hidesOnDeactivate = false
        panel.delegate = self
        panel.setAccessibilityLabel("Workspaces")
        let content = WorkspacePickerContent(frame: panel.frame)
        content.wantsLayer = true
        content.layer?.cornerRadius = 12
        content.layer?.masksToBounds = true
        panel.contentView = content

        searchBackground.wantsLayer = true
        searchBackground.layer?.cornerRadius = 8
        content.addSubview(searchBackground)
        search.font = .systemFont(ofSize: 13)
        search.isBordered = false
        search.drawsBackground = false
        search.focusRingType = .none
        search.delegate = self
        search.setAccessibilityLabel("Filter workspaces")
        searchBackground.addSubview(search)
        searchIcon.image = NSImage(systemSymbolName: "magnifyingglass", accessibilityDescription: nil)
        searchIcon.symbolConfiguration = .init(pointSize: 12, weight: .regular)
        searchBackground.addSubview(searchIcon)
        clearSearch.image = NSImage(systemSymbolName: "xmark.circle.fill", accessibilityDescription: "Clear filter")
        clearSearch.isBordered = false
        clearSearch.symbolConfiguration = .init(pointSize: 12, weight: .regular)
        clearSearch.target = self
        clearSearch.action = #selector(clearFilter)
        clearSearch.setAccessibilityLabel("Clear filter")
        searchBackground.addSubview(clearSearch)

        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.scrollerStyle = .overlay
        scroll.documentView = list
        content.addSubview(scroll)
        self.panel = panel
        return panel
    }

    /// Keep shortcut numbers tied to the full list, even while filtering.
    static func matching(_ workspaces: [Workspace], query: String) -> [(workspace: Workspace, shortcut: String)] {
        let query = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return workspaces.enumerated().compactMap { index, ws in
            guard query.isEmpty || ws.name.localizedCaseInsensitiveContains(query)
                || (ws.isRemote && ws.host.localizedCaseInsensitiveContains(query)) else { return nil }
            return (ws, index < 9 ? "⌃⌘\(index + 1)" : "")
        }
    }

    private func reload() {
        guard let panel, let anchor, let targetWindow else { return }
        let sm = SessionManager.shared
        let chrome = sm.config.chrome
        let current = (targetWindow.windowController as? TerminalWindowController)?.workspaceID
        for view in list.subviews { view.removeFromSuperview() }
        rows.removeAll()
        var y: CGFloat = 0
        let width = Self.width - Self.inset * 2
        func row(_ title: String, symbol: String, shortcut: String = "", current: Bool = false,
                 action: @escaping () -> Void) {
            let button = WorkspacePickerRow(title: title, symbol: symbol, shortcut: shortcut,
                                            current: current, chrome: chrome)
            button.frame = NSRect(x: 0, y: y, width: width, height: 28)
            button.tag = rows.count
            button.onHover = { [weak self, weak button] in
                guard let button else { return }
                self?.select(button.tag, scrollToVisible: false)
            }
            button.onSelect = { [weak self] in
                self?.close()
                action()
            }
            list.addSubview(button)
            rows.append(button)
            y += 30
        }
        func separator() {
            let line = NSView(frame: NSRect(x: 8, y: y + 4, width: width - 16, height: ChromeStroke.width))
            line.wantsLayer = true
            line.layer?.backgroundColor = chrome.strokeStrong.cgColor
            list.addSubview(line)
            y += 10
        }

        let matches = Self.matching(sm.workspacesByRecency, query: search.stringValue)
        for (ws, shortcut) in matches {
            row(ws.name,
                symbol: ws.id == current ? "checkmark" : ws.isRemote ? sm.config.hostSymbol(for: ws.host) : "rectangle.stack",
                shortcut: shortcut, current: ws.id == current) { [weak targetWindow] in
                guard let workspace = sm.workspace(ws.id) else { return }
                if let c = targetWindow?.windowController as? TerminalWindowController, !c.isClosed {
                    sm.switchWorkspace(in: c, to: workspace)
                } else {
                    sm.openWorkspaceInNewWindow(workspace)
                }
            }
            rows.last?.toolTip = ws.isRemote ? "\(ws.name) · \(ws.host)" : ws.name
        }
        if matches.isEmpty {
            let empty = NSTextField(labelWithString: "No matching workspaces")
            empty.font = .systemFont(ofSize: 12)
            empty.textColor = chrome.contentSecondary
            empty.alignment = .center
            empty.frame = NSRect(x: 8, y: y + 6, width: width - 16, height: 18)
            list.addSubview(empty)
            y += 30
        }
        separator()
        row("New Workspace", symbol: "plus.rectangle.on.rectangle", shortcut: newWorkspaceShortcut()) {
            sm.newWorkspace()
        }
        row("Add Remote Host…", symbol: "globe") {
            RemoteHosts.shared.show()
            RemoteHosts.shared.sheet = .add
        }
        if let id = current, let ws = sm.workspace(id) {
            separator()
            row("Rename Workspace…", symbol: "pencil") {
                guard sm.workspace(id) === ws else { return }
                sm.renameWorkspace(ws)
            }
            row("Close Workspace…", symbol: "xmark.rectangle") {
                guard sm.workspace(id) === ws else { return }
                sm.closeWorkspace(ws)
            }
        }

        let anchorRect = targetWindow.convertToScreen(anchor.convert(anchor.bounds, to: nil))
        let screen = targetWindow.screen?.visibleFrame ?? NSScreen.main?.visibleFrame ?? targetWindow.frame
        let frame = Self.frame(anchor: anchorRect, contentHeight: y + Self.listTop + Self.inset, screen: screen)
        panel.setFrame(frame, display: true)
        searchBackground.frame = NSRect(x: Self.inset, y: Self.inset, width: width, height: 30)
        searchIcon.frame = NSRect(x: 10, y: 8, width: 14, height: 14)
        search.frame = NSRect(x: 30, y: 5, width: width - 54, height: 20)
        clearSearch.frame = NSRect(x: width - 22, y: 8, width: 14, height: 14)
        clearSearch.isHidden = search.stringValue.isEmpty
        scroll.frame = NSRect(x: Self.inset, y: Self.listTop, width: width,
                              height: max(0, frame.height - Self.listTop - Self.inset))
        list.frame = NSRect(x: 0, y: 0, width: width, height: y)
        select(search.stringValue.isEmpty ? (rows.firstIndex(where: \.isCurrent) ?? 0) : 0)
    }

    /// Prefer below when the content fits. Otherwise use above, or the larger side with a
    /// shorter scroll area when neither side fits, so the picker never covers its button.
    static func frame(anchor: NSRect, contentHeight: CGFloat, screen: NSRect) -> NSRect {
        let visible = screen.insetBy(dx: 8, dy: 8)
        let desiredHeight = min(contentHeight, 460, visible.height)
        let belowTop = min(max(anchor.minY - 6, visible.minY), visible.maxY)
        let aboveBottom = min(max(anchor.maxY + 6, visible.minY), visible.maxY)
        let belowHeight = belowTop - visible.minY
        let aboveHeight = visible.maxY - aboveBottom
        let below = belowHeight >= desiredHeight || (aboveHeight < desiredHeight && belowHeight >= aboveHeight)
        let height = min(desiredHeight, below ? belowHeight : aboveHeight)
        let x = min(max(anchor.minX, visible.minX), visible.maxX - width)
        let y = below ? belowTop - height : aboveBottom
        return NSRect(x: x, y: y, width: width, height: height)
    }

    private func newWorkspaceShortcut() -> String {
        func find(_ menu: NSMenu) -> NSMenuItem? {
            for item in menu.items where !item.isHidden {
                if item.identifier == MainMenu.actionIdentifier("new_workspace") { return item }
                if let submenu = item.submenu, let found = find(submenu) { return found }
            }
            return nil
        }
        return NSApp.mainMenu.flatMap(find).map(CommandPalette.shortcut) ?? "⌘N"
    }

    private func select(_ index: Int, scrollToVisible: Bool = true, announce: Bool = false) {
        guard !rows.isEmpty else { return }
        let previous = selected
        selected = min(max(index, 0), rows.count - 1)
        for (i, row) in rows.enumerated() { row.isSelected = i == selected }
        if scrollToVisible { list.scrollToVisible(rows[selected].frame) }
        // Keyboard focus stays in the filter. Speak arrow-key changes so VoiceOver users
        // know what Return will activate; hover, reloads and boundary presses stay silent.
        if announce, selected != previous {
            announceSelection(rows[selected].accessibilityLabel() ?? rows[selected].title)
        }
    }

    func controlTextDidChange(_ obj: Notification) { reload() }

    @objc private func clearFilter() {
        search.stringValue = ""
        reload()
        panel?.makeFirstResponder(search)
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        switch selector {
        case #selector(NSResponder.moveDown(_:)):
            select(selected + 1, announce: true)
        case #selector(NSResponder.moveUp(_:)):
            select(selected - 1, announce: true)
        case #selector(NSResponder.insertNewline(_:)):
            if rows.indices.contains(selected) { rows[selected].performClick(nil) }
        case #selector(NSResponder.cancelOperation(_:)):
            close()
        default:
            return false
        }
        return true
    }

    func windowDidResignKey(_ notification: Notification) { close(restoreFocus: false) }
}

/// A real button for accessibility, with menu-like hover and a rounded theme accent.
private final class WorkspacePickerRow: NSButton {
    let isCurrent: Bool
    private let symbol: String
    private let shortcut: String
    private let chrome: ChromePalette
    var onHover: (() -> Void)?
    var onSelect: (() -> Void)?
    var isSelected = false { didSet { needsDisplay = true } }

    init(title: String, symbol: String, shortcut: String, current: Bool, chrome: ChromePalette) {
        self.symbol = symbol
        self.shortcut = shortcut
        self.isCurrent = current
        self.chrome = chrome
        super.init(frame: .zero)
        self.title = title
        isBordered = false
        target = self
        action = #selector(run)
        setAccessibilityLabel(current ? "\(title), current workspace" : title)
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
                                       owner: self))
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }
    override var isFlipped: Bool { true }
    // Keep typing in the filter after navigating with the pointer.
    override var acceptsFirstResponder: Bool { false }
    override func isAccessibilitySelected() -> Bool { isSelected }
    override func mouseEntered(with event: NSEvent) { onHover?() }
    @objc private func run() { onSelect?() }

    override func draw(_ dirtyRect: NSRect) {
        let foreground: NSColor
        if isSelected {
            chrome.accent.setFill()
            NSBezierPath(roundedRect: bounds, xRadius: 7, yRadius: 7).fill()
            // Choose the more readable foreground even for bright accents in light themes.
            let rgb = chrome.accent.usingColorSpace(.sRGB) ?? chrome.accent
            func linear(_ c: CGFloat) -> CGFloat { c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4) }
            let luma = 0.2126 * linear(rgb.redComponent) + 0.7152 * linear(rgb.greenComponent)
                + 0.0722 * linear(rgb.blueComponent)
            foreground = luma > 0.179 ? .black : .white
        } else {
            foreground = chrome.contentPrimary
        }
        let configuration = NSImage.SymbolConfiguration(pointSize: 13, weight: isCurrent ? .semibold : .regular)
            .applying(.init(paletteColors: [foreground]))
        NSImage(systemSymbolName: symbol, accessibilityDescription: nil)?
            .withSymbolConfiguration(configuration)?
            .draw(in: NSRect(x: 10, y: 7, width: 16, height: 14), from: .zero, operation: .sourceOver,
                  fraction: 1, respectFlipped: true, hints: nil)

        let keyAttributes: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: 11, weight: .medium),
            .foregroundColor: isSelected ? foreground.withAlphaComponent(0.75) : chrome.contentSecondary,
        ]
        let keyWidth = (shortcut as NSString).size(withAttributes: keyAttributes).width
        (shortcut as NSString).draw(at: NSPoint(x: bounds.width - 10 - keyWidth, y: 7), withAttributes: keyAttributes)
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineBreakMode = .byTruncatingTail
        (title as NSString).draw(in: NSRect(x: 34, y: 5, width: bounds.width - 52 - keyWidth, height: 18),
                                 withAttributes: [.font: NSFont.systemFont(ofSize: 13, weight: isCurrent ? .semibold : .regular),
                                                  .foregroundColor: foreground, .paragraphStyle: paragraph])
    }
}
