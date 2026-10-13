import AppKit
import CThurm

/// Thurm > Settings… (⌘,): every option of config.toml. Changes are saved to settings.toml
/// beside it (config.toml may be a read-only link), and apply at once. Rules and values come
/// from `SettingsModel`; the Rust side is `thurm_settings_json` and `thurm_settings_set`.
final class SettingsWindow: NSObject, NSWindowDelegate, NSTableViewDataSource, NSTableViewDelegate,
    NSTextFieldDelegate {
    static let shared = SettingsWindow()

    private var window: NSWindow?
    private let search = NSSearchField()
    private let sectionList = NSTableView()
    private let notice = NSTextField(wrappingLabelWithString: "")
    private var noticeView: NSView?
    private let formStack = NSStackView()
    private static let sidebarWidth: CGFloat = 200
    private let formScroll = NSScrollView()
    private var doc: SettingsDocument?
    private var sections: [SettingSection] = []
    private var selectedSection: String?
    private var query = ""
    private var selectingInSidebar = false
    /// The light and dark popups of each `colors.theme` control.
    private var themePopups: [String: (light: NSPopUpButton, dark: NSPopUpButton)] = [:]

    /// The label of a font popup's entry that leaves a face to the family.
    private static let sameAsFamily = "Same as family"
    /// The last entry of a theme popup: choose a theme file instead.
    private static let otherTheme = "Other\u{2026}"

    func show() {
        if window == nil { build() }
        reload()
        window?.makeKeyAndOrderFront(nil)
    }

    // MARK: Layout

    private func build() {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 920, height: 600),
                         styleMask: [.titled, .closable, .miniaturizable, .resizable],
                         backing: .buffered, defer: false)
        w.title = "Settings"
        w.isReleasedWhenClosed = false
        w.delegate = self
        w.contentMinSize = NSSize(width: 700, height: 420)
        w.center()

        search.placeholderString = "Search settings"
        search.target = self
        search.action = #selector(queryChanged(_:))
        search.sendsSearchStringImmediately = true
        search.translatesAutoresizingMaskIntoConstraints = false

        sectionList.addTableColumn(NSTableColumn(identifier: NSUserInterfaceItemIdentifier("section")))
        sectionList.headerView = nil
        sectionList.style = .sourceList
        sectionList.rowHeight = 26
        sectionList.dataSource = self
        sectionList.delegate = self
        let sideScroll = NSScrollView()
        sideScroll.documentView = sectionList
        sideScroll.hasVerticalScroller = true
        sideScroll.drawsBackground = false
        sideScroll.borderType = .noBorder
        sideScroll.translatesAutoresizingMaskIntoConstraints = false

        let sidebar = NSView()
        sidebar.addSubview(search)
        sidebar.addSubview(sideScroll)
        NSLayoutConstraint.activate([
            search.topAnchor.constraint(equalTo: sidebar.topAnchor, constant: 12),
            search.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 12),
            search.trailingAnchor.constraint(equalTo: sidebar.trailingAnchor, constant: -12),
            sideScroll.topAnchor.constraint(equalTo: search.bottomAnchor, constant: 10),
            sideScroll.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor),
            sideScroll.trailingAnchor.constraint(equalTo: sidebar.trailingAnchor),
            sideScroll.bottomAnchor.constraint(equalTo: sidebar.bottomAnchor),
            sidebar.widthAnchor.constraint(equalToConstant: Self.sidebarWidth),
        ])

        notice.font = .systemFont(ofSize: 12)
        notice.textColor = .secondaryLabelColor
        notice.maximumNumberOfLines = 0
        let noticeBox = NSStackView(views: [notice])
        noticeBox.edgeInsets = NSEdgeInsets(top: 0, left: 24, bottom: 0, right: 24)
        noticeBox.isHidden = true

        formStack.orientation = .vertical
        formStack.alignment = .leading
        formStack.spacing = 22
        formStack.translatesAutoresizingMaskIntoConstraints = false
        let flipped = FlippedView()
        flipped.translatesAutoresizingMaskIntoConstraints = false
        flipped.addSubview(formStack)
        NSLayoutConstraint.activate([
            formStack.topAnchor.constraint(equalTo: flipped.topAnchor, constant: 20),
            formStack.leadingAnchor.constraint(equalTo: flipped.leadingAnchor, constant: 24),
            formStack.trailingAnchor.constraint(lessThanOrEqualTo: flipped.trailingAnchor, constant: -24),
            formStack.bottomAnchor.constraint(equalTo: flipped.bottomAnchor, constant: -20),
        ])
        formScroll.documentView = flipped
        formScroll.hasVerticalScroller = true
        formScroll.drawsBackground = false
        formScroll.borderType = .noBorder
        formScroll.setContentHuggingPriority(.defaultLow, for: .vertical)
        NSLayoutConstraint.activate([
            flipped.leadingAnchor.constraint(equalTo: formScroll.contentView.leadingAnchor),
            flipped.trailingAnchor.constraint(equalTo: formScroll.contentView.trailingAnchor),
            flipped.topAnchor.constraint(equalTo: formScroll.contentView.topAnchor),
        ])

        let revealButton = NSButton(title: "Show Files in Finder", target: self, action: #selector(revealFiles(_:)))
        let editButton = NSButton(title: "Open settings.toml", target: self, action: #selector(openOverlay(_:)))
        let footer = NSStackView(views: [revealButton, editButton])
        footer.orientation = .horizontal
        footer.spacing = 10
        footer.edgeInsets = NSEdgeInsets(top: 8, left: 24, bottom: 12, right: 24)

        let main = NSStackView(views: [noticeBox, formScroll, footer])
        main.orientation = .vertical
        main.alignment = .leading
        main.spacing = 0
        main.edgeInsets = NSEdgeInsets(top: 12, left: 0, bottom: 0, right: 0)
        main.translatesAutoresizingMaskIntoConstraints = false
        formScroll.widthAnchor.constraint(equalTo: main.widthAnchor).isActive = true
        noticeBox.widthAnchor.constraint(equalTo: main.widthAnchor).isActive = true

        let divider = NSView()
        divider.wantsLayer = true
        divider.layer?.backgroundColor = NSColor.separatorColor.cgColor
        divider.translatesAutoresizingMaskIntoConstraints = false
        let content = NSView()
        for view in [sidebar, divider, main] {
            view.translatesAutoresizingMaskIntoConstraints = false
            content.addSubview(view)
        }
        NSLayoutConstraint.activate([
            sidebar.topAnchor.constraint(equalTo: content.topAnchor),
            sidebar.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            sidebar.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            divider.topAnchor.constraint(equalTo: content.topAnchor),
            divider.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            divider.leadingAnchor.constraint(equalTo: sidebar.trailingAnchor),
            divider.widthAnchor.constraint(equalToConstant: 1),
            main.topAnchor.constraint(equalTo: content.topAnchor),
            main.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            main.leadingAnchor.constraint(equalTo: divider.trailingAnchor),
            main.trailingAnchor.constraint(equalTo: content.trailingAnchor),
        ])
        w.contentView = content
        window = w
        noticeView = noticeBox
    }

    // MARK: Model

    /// Re-reads the settings from Rust and shows them again.
    private func reload() {
        let raw = thurm_settings_json()
        let text = raw.map { String(cString: $0) } ?? ""
        if let raw { thurm_string_free(raw) }
        doc = SettingsDocument(json: text)
        guard let doc else { return }
        sections = doc.populatedSections
        if selectedSection == nil || !sections.contains(where: { $0.id == selectedSection }) {
            selectedSection = sections.first?.id
        }
        sectionList.reloadData()
        selectInSidebar(selectedSection)
        updateNotice(doc)
        rebuildForm()
    }

    private func updateNotice(_ doc: SettingsDocument) {
        var lines: [String] = []
        if let error = doc.error {
            lines.append("config.toml could not be loaded (\(error)). Thurm runs on the defaults until it is fixed.")
        }
        if doc.configReadOnly {
            lines.append("config.toml is read-only (another tool, such as Home Manager, manages it). "
                + "Changes made here are saved to settings.toml, which takes precedence over it.")
        }
        if let first = doc.warnings.first {
            let more = doc.warnings.count > 1 ? " (+\(doc.warnings.count - 1) more)" : ""
            lines.append("Ignored in settings.toml: \(first)\(more)")
        }
        notice.stringValue = lines.joined(separator: "\n")
        notice.textColor = doc.error == nil ? .secondaryLabelColor : .systemRed
        noticeView?.isHidden = lines.isEmpty
    }

    private func rebuildForm() {
        for view in formStack.arrangedSubviews {
            formStack.removeArrangedSubview(view)
            view.removeFromSuperview()
        }
        themePopups = [:]
        guard let doc else { return }
        let groups: [(section: SettingSection, items: [SettingItem])]
        if query.isEmpty {
            if let id = selectedSection, let section = sections.first(where: { $0.id == id }) {
                groups = [(section: section, items: doc.settings.filter { $0.section == id })]
            } else {
                groups = []
            }
        } else {
            groups = doc.groups(matching: query)
        }
        for group in groups {
            if !query.isEmpty {
                let heading = NSTextField(labelWithString: group.section.label)
                heading.font = .systemFont(ofSize: 15, weight: .semibold)
                formStack.addArrangedSubview(heading)
            }
            formStack.addArrangedSubview(sectionView(group.items))
        }
        if groups.isEmpty {
            let empty = NSTextField(labelWithString: "No settings match \u{201C}\(query)\u{201D}.")
            empty.textColor = .secondaryLabelColor
            formStack.addArrangedSubview(empty)
        }
        fitWidth()
    }

    /// Keeps the window wide enough for the form shown: its rows have fixed-width labels and
    /// controls, so a narrower window would clip them.
    private func fitWidth() {
        guard let window, let content = window.contentView else { return }
        let scroller = NSScroller.scrollerWidth(for: .regular, scrollerStyle: formScroll.scrollerStyle)
        let needed = Self.sidebarWidth + 1 + 24 + formStack.fittingSize.width + 24 + scroller
        let minimum = max(700, ceil(needed))
        window.contentMinSize = NSSize(width: minimum, height: window.contentMinSize.height)
        if content.frame.width < minimum {
            var frame = window.frame
            frame.size.width += minimum - content.frame.width
            window.setFrame(frame, display: true, animate: false)
        }
    }

    /// The rows of a section: one per option, and the notification matrix as one grid at the end.
    private func sectionView(_ items: [SettingItem]) -> NSView {
        let stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 22
        let form = NSGridView(numberOfColumns: 2, rows: 0)
        form.columnSpacing = 28
        form.rowSpacing = 16
        form.column(at: 0).xPlacement = .leading
        form.column(at: 1).xPlacement = .trailing
        for item in items where item.row == nil {
            form.addRow(with: [labelColumn(item), controlRow(item)])
        }
        if form.numberOfRows > 0 { stack.addArrangedSubview(form) }
        let matrix = items.filter { $0.row != nil }
        if !matrix.isEmpty { stack.addArrangedSubview(matrixView(matrix)) }
        return stack
    }

    private func labelColumn(_ item: SettingItem) -> NSView {
        let title = NSTextField(labelWithString: item.label)
        title.font = .systemFont(ofSize: 13)
        var views: [NSView] = [title]
        if !item.doc.isEmpty {
            let doc = NSTextField(wrappingLabelWithString: item.doc)
            doc.font = .systemFont(ofSize: 11)
            doc.textColor = .secondaryLabelColor
            doc.preferredMaxLayoutWidth = 380
            views.append(doc)
        }
        if let warning = item.warning {
            let note = NSTextField(wrappingLabelWithString: "settings.toml value ignored: \(warning)")
            note.font = .systemFont(ofSize: 11)
            note.textColor = .systemRed
            note.preferredMaxLayoutWidth = 380
            views.append(note)
        }
        let stack = NSStackView(views: views)
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 3
        stack.widthAnchor.constraint(equalToConstant: 380).isActive = true
        return stack
    }

    /// The control, the source of its value and Reset (when settings.toml sets it).
    private func controlRow(_ item: SettingItem) -> NSView {
        var views: [NSView] = [control(for: item)]
        let badge = NSTextField(labelWithString: item.source.label)
        badge.font = .systemFont(ofSize: 11)
        badge.textColor = .tertiaryLabelColor
        views.append(badge)
        if item.isOverridden {
            views.append(resetButton(key: item.key))
        }
        let stack = NSStackView(views: views)
        stack.orientation = .horizontal
        stack.alignment = .centerY
        stack.spacing = 10
        return stack
    }

    private func resetButton(key: String) -> NSButton {
        let button = NSButton(title: "Reset", target: self, action: #selector(resetClicked(_:)))
        button.bezelStyle = .rounded
        button.controlSize = .small
        button.identifier = NSUserInterfaceItemIdentifier(key)
        return button
    }

    /// The notification matrix: a row per state, a column per field.
    private func matrixView(_ items: [SettingItem]) -> NSView {
        var states: [String] = []
        var columns: [String] = []
        for item in items {
            if let row = item.row, !states.contains(row) { states.append(row) }
            if let column = item.column, !columns.contains(column) { columns.append(column) }
        }
        let grid = NSGridView(numberOfColumns: columns.count + 2, rows: states.count + 1)
        grid.columnSpacing = 18
        grid.rowSpacing = 10
        for (index, column) in columns.enumerated() {
            let title = NSTextField(labelWithString: columnTitle(column))
            title.font = .systemFont(ofSize: 11)
            title.textColor = .secondaryLabelColor
            grid.cell(atColumnIndex: index + 1, rowIndex: 0).contentView = title
        }
        for (r, state) in states.enumerated() {
            let name = NSTextField(labelWithString: stateTitle(state))
            name.font = .systemFont(ofSize: 13)
            grid.cell(atColumnIndex: 0, rowIndex: r + 1).contentView = name
            for (index, column) in columns.enumerated() {
                if let cell = items.first(where: { $0.row == state && $0.column == column }) {
                    grid.cell(atColumnIndex: index + 1, rowIndex: r + 1).contentView = control(for: cell)
                }
            }
            // Reset is offered only when settings.toml sets a cell of this row.
            if items.contains(where: { $0.row == state && $0.isOverridden }) {
                grid.cell(atColumnIndex: columns.count + 1, rowIndex: r + 1).contentView =
                    resetButton(key: "notifications.\(state)")
            }
        }
        return grid
    }

    private func columnTitle(_ column: String) -> String {
        switch column {
        case "tab_indicator": return "Tab"
        case "notify": return "Notify"
        case "badge": return "Badge"
        case "sound": return "Sound"
        default: return column
        }
    }

    private func stateTitle(_ state: String) -> String {
        state.prefix(1).uppercased() + state.dropFirst()
    }

    // MARK: Controls

    private func control(for item: SettingItem) -> NSView {
        let key = NSUserInterfaceItemIdentifier(item.key)
        switch item.kind {
        case .bool:
            let toggle = NSSwitch()
            toggle.state = (item.value as? NSNumber)?.boolValue == true ? .on : .off
            toggle.target = self
            toggle.action = #selector(toggled(_:))
            toggle.identifier = key
            return toggle
        case .int, .float:
            return numberControl(item)
        case .enum:
            let popup = NSPopUpButton(frame: .zero, pullsDown: false)
            popup.addItems(withTitles: item.options)
            popup.selectItem(withTitle: item.text)
            popup.target = self
            popup.action = #selector(popupChanged(_:))
            popup.identifier = key
            return popup
        case .theme:
            return themeControl(item)
        case .font:
            return fontPopup(item)
        case .color:
            let well = NSColorWell()
            well.isBordered = true
            well.supportsAlpha = false
            if let text = item.value as? String, let rgb = HexColor.parse(text) {
                well.color = NSColor(srgbRed: CGFloat(rgb.red) / 255, green: CGFloat(rgb.green) / 255,
                                     blue: CGFloat(rgb.blue) / 255, alpha: 1)
            } else {
                // Unset: the theme's color, faded, so a chosen color reads as the override.
                well.color = themeColor(for: item.key) ?? .gray
                well.alphaValue = 0.4
                well.toolTip = "The theme's color. Choose a color to override it."
            }
            well.target = self
            well.action = #selector(colorChanged(_:))
            well.identifier = key
            well.widthAnchor.constraint(equalToConstant: 44).isActive = true
            well.heightAnchor.constraint(equalToConstant: 24).isActive = true
            if item.value is String { return well }
            let theme = NSTextField(labelWithString: "Theme")
            theme.font = .systemFont(ofSize: 11)
            theme.textColor = .secondaryLabelColor
            return NSStackView(views: [well, theme])
        case .list, .text:
            let field = NSTextField(string: item.text)
            field.placeholderString = item.kind == .list ? "Comma-separated" : nil
            field.delegate = self
            field.identifier = key
            field.widthAnchor.constraint(equalToConstant: item.kind == .list ? 260 : 150).isActive = true
            return field
        case .table:
            let summary = NSTextField(labelWithString: tableSummary(item.value))
            summary.font = .systemFont(ofSize: 12)
            summary.textColor = .secondaryLabelColor
            // Edited where it is set: a list in settings.toml would hide config.toml's whole.
            let inConfig = item.source == .file
            let edit = NSButton(title: inConfig ? "Open config.toml\u{2026}" : "Edit in settings.toml\u{2026}",
                                target: self, action: #selector(editTable(_:)))
            edit.bezelStyle = .rounded
            edit.controlSize = .small
            edit.identifier = key
            if inConfig, doc?.configReadOnly == true {
                edit.toolTip = "config.toml is managed by another tool: change these entries there. "
                    + "Entries in settings.toml would replace them all."
            } else if !inConfig {
                edit.toolTip = "Entries in settings.toml replace any in config.toml."
            }
            return NSStackView(views: [summary, edit])
        }
    }

    private func numberControl(_ item: SettingItem) -> NSView {
        let key = NSUserInterfaceItemIdentifier(item.key)
        let number = (item.value as? NSNumber)?.doubleValue ?? 0
        if let lo = item.min, let hi = item.max {
            let slider = ReleaseSlider(value: number, minValue: lo, maxValue: hi, target: self,
                                       action: #selector(sliderMoved(_:)))
            slider.identifier = key
            slider.onRelease = { [weak self] slider in self?.saveSlider(slider) }
            slider.widthAnchor.constraint(equalToConstant: 180).isActive = true
            let readout = NSTextField(labelWithString: item.text)
            readout.alignment = .right
            readout.identifier = NSUserInterfaceItemIdentifier("readout")
            readout.widthAnchor.constraint(equalToConstant: 56).isActive = true
            return NSStackView(views: [slider, readout])
        }
        let field = NSTextField(string: item.text)
        field.alignment = .right
        field.identifier = key
        field.delegate = self
        field.widthAnchor.constraint(equalToConstant: 72).isActive = true
        let stepper = NSStepper()
        stepper.minValue = item.min ?? 0
        stepper.maxValue = item.max ?? 1_000_000_000
        stepper.increment = item.kind == .int ? 1 : 0.1
        stepper.doubleValue = number
        stepper.target = self
        stepper.action = #selector(stepped(_:))
        stepper.identifier = key
        return NSStackView(views: [field, stepper])
    }

    private func themeControl(_ item: SettingItem) -> NSView {
        let names = SessionManager.shared.config.themes.map(\.name)
        let spec = ColorThemeSpec.parse(item.text)
        func popup(_ selected: String) -> NSPopUpButton {
            let popup = NSPopUpButton(frame: .zero, pullsDown: false)
            var all = names
            if !all.contains(selected) { all.insert(selected, at: 0) }
            popup.addItems(withTitles: all)
            popup.menu?.addItem(.separator())
            popup.addItem(withTitle: SettingsWindow.otherTheme)
            popup.selectItem(withTitle: selected)
            popup.target = self
            popup.action = #selector(themeChanged(_:))
            popup.identifier = NSUserInterfaceItemIdentifier(item.key)
            return popup
        }
        let dark = popup(spec.dark)
        let light = popup(spec.light)
        themePopups[item.key] = (light, dark)
        let darkLabel = NSTextField(labelWithString: "Dark")
        let lightLabel = NSTextField(labelWithString: "Light")
        for label in [darkLabel, lightLabel] {
            label.font = .systemFont(ofSize: 11)
            label.textColor = .secondaryLabelColor
        }
        // Light above dark, so the pair is no wider than any other control.
        func line(_ label: NSTextField, _ popup: NSPopUpButton) -> NSStackView {
            label.alignment = .right
            label.widthAnchor.constraint(equalToConstant: 32).isActive = true
            popup.widthAnchor.constraint(equalToConstant: 208).isActive = true
            let line = NSStackView(views: [label, popup])
            line.orientation = .horizontal
            line.spacing = 6
            return line
        }
        let column = NSStackView(views: [line(lightLabel, light), line(darkLabel, dark)])
        column.orientation = .vertical
        column.alignment = .trailing
        column.spacing = 6
        return column
    }

    private func fontPopup(_ item: SettingItem) -> NSPopUpButton {
        let popup = NSPopUpButton(frame: .zero, pullsDown: false)
        if item.isNullable { popup.addItem(withTitle: SettingsWindow.sameAsFamily) }
        let families = FontFamilies.all
        var current = item.text
        if !item.isNullable && current.isEmpty { current = families.monospaced.first ?? "" }
        popup.addItems(withTitles: families.monospaced)
        if !families.monospaced.isEmpty {
            popup.menu?.addItem(.separator())
        }
        popup.addItems(withTitles: families.other)
        if !current.isEmpty && popup.itemTitles.contains(current) == false {
            popup.insertItem(withTitle: current, at: 0)
        }
        popup.selectItem(withTitle: current.isEmpty ? SettingsWindow.sameAsFamily : current)
        popup.target = self
        popup.action = #selector(fontChanged(_:))
        popup.identifier = NSUserInterfaceItemIdentifier(item.key)
        popup.widthAnchor.constraint(equalToConstant: 240).isActive = true
        return popup
    }

    private func tableSummary(_ value: Any?) -> String {
        if let list = value as? [Any] { return list.isEmpty ? "None" : "\(list.count) items" }
        if let map = value as? [String: Any] { return map.isEmpty ? "None" : "\(map.count) entries" }
        return "None"
    }

    // MARK: Changes

    /// The setting `key` as the control holds it: `value`, or NSNull to reset it.
    private func commit(_ key: String, _ value: Any) {
        let raw = thurm_settings_set(key, JSON.encode(value))
        var failure: String?
        if let raw {
            failure = String(cString: raw)
            thurm_string_free(raw)
        }
        if let failure {
            let alert = NSAlert()
            alert.messageText = "The setting was not saved"
            alert.informativeText = failure
            if let window { alert.beginSheetModal(for: window) } else { alert.runModal() }
            reload()
            return
        }
        SessionManager.shared.reloadConfig(notifyDaemon: true)
        reload()
    }

    /// Saves a number typed into a field. Input that is not a finite number, or not one the
    /// setting can take, is refused: the field goes back to the saved value.
    private func setNumber(_ key: String, _ text: String) {
        guard let kind = doc?.settings.first(where: { $0.key == key })?.kind,
              let number = SettingNumber.parse(text),
              let value = SettingNumber.value(number, kind: kind)
        else {
            NSSound.beep()
            reload()
            return
        }
        commit(key, value)
    }

    private func identifierKey(_ sender: Any?) -> String? {
        (sender as? NSView)?.identifier?.rawValue
    }

    @objc private func toggled(_ sender: NSSwitch) {
        guard let key = identifierKey(sender) else { return }
        commit(key, sender.state == .on)
    }

    @objc private func popupChanged(_ sender: NSPopUpButton) {
        guard let key = identifierKey(sender) else { return }
        commit(key, sender.titleOfSelectedItem ?? "")
    }

    @objc private func themeChanged(_ sender: NSPopUpButton) {
        guard let key = identifierKey(sender), let popups = themePopups[key] else { return }
        var light = popups.light.titleOfSelectedItem ?? ""
        var dark = popups.dark.titleOfSelectedItem ?? ""
        if sender.titleOfSelectedItem == SettingsWindow.otherTheme {
            // A theme file: saved only if it loads (commit reports the error otherwise).
            guard let path = chooseThemeFile() else { reload(); return }
            if sender === popups.light { light = path } else { dark = path }
        }
        commit(key, ColorThemeSpec.encode(light: light, dark: dark))
    }

    private func chooseThemeFile() -> String? {
        let panel = NSOpenPanel()
        panel.message = "Choose a theme file (Thurm's TOML, or an iTerm2-style palette)."
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        return url.path
    }

    /// The color a theme gives `key`, for an unset color control.
    private func themeColor(for key: String) -> NSColor? {
        let theme = SessionManager.shared.config.theme
        switch key {
        case "colors.foreground": return colorFromRGB(theme.foreground)
        case "colors.background": return colorFromRGB(theme.background)
        case "colors.cursor": return colorFromRGB(theme.cursor)
        case "colors.cursor_text": return colorFromRGB(theme.cursorText)
        case "colors.selection_foreground": return colorFromRGB(theme.selectionForeground)
        case "colors.selection_background": return colorFromRGB(theme.selectionBackground)
        default: return nil
        }
    }

    @objc private func fontChanged(_ sender: NSPopUpButton) {
        guard let key = identifierKey(sender) else { return }
        let title = sender.titleOfSelectedItem ?? ""
        let value: Any = title == SettingsWindow.sameAsFamily ? NSNull() : title
        commit(key, value)
    }

    @objc private func colorChanged(_ sender: NSColorWell) {
        guard let key = identifierKey(sender) else { return }
        let color = sender.color.usingColorSpace(.sRGB) ?? sender.color
        let hex = HexColor.format(red: Int(round(color.redComponent * 255)),
                                  green: Int(round(color.greenComponent * 255)),
                                  blue: Int(round(color.blueComponent * 255)))
        commit(key, hex)
    }

    private func setting(_ key: String) -> SettingItem? {
        doc?.settings.first { $0.key == key }
    }

    @objc private func stepped(_ sender: NSStepper) {
        guard let key = identifierKey(sender), let item = setting(key),
              let value = SettingNumber.value(sender.doubleValue, kind: item.kind)
        else {
            NSSound.beep()
            return
        }
        commit(key, value)
    }

    /// A slider saves when the mouse is released (`ReleaseSlider`) or a key moves it. Committing
    /// rebuilds the controls, so nothing is saved while the mouse is down: the drag only moves
    /// the readout.
    @objc private func sliderMoved(_ sender: NSSlider) {
        guard let key = identifierKey(sender), let item = setting(key),
              let value = SettingNumber.value(sender.doubleValue, kind: item.kind)
        else { return }
        let readout = (sender.superview as? NSStackView)?.arrangedSubviews
            .compactMap { $0 as? NSTextField }
            .first { $0.identifier?.rawValue == "readout" }
        if let whole = value as? Int {
            readout?.stringValue = String(whole)
        } else {
            readout?.stringValue = String(format: "%g", (value as? Double) ?? 0)
        }
        switch NSApp.currentEvent?.type {
        case .leftMouseDown?, .leftMouseDragged?, .leftMouseUp?:
            return
        default:
            commit(key, value)
        }
    }

    private func saveSlider(_ slider: NSSlider) {
        guard let key = identifierKey(slider), let item = setting(key),
              let value = SettingNumber.value(slider.doubleValue, kind: item.kind)
        else { return }
        // A click that leaves the value as it was must not turn it into an override.
        if let current = (item.value as? NSNumber)?.doubleValue,
           let chosen = (value as? NSNumber)?.doubleValue, current == chosen {
            return
        }
        commit(key, value)
    }

    @objc private func resetClicked(_ sender: NSButton) {
        guard let key = identifierKey(sender) else { return }
        commit(key, NSNull())
    }

    @objc private func queryChanged(_ sender: NSSearchField) {
        query = sender.stringValue.trimmingCharacters(in: .whitespaces)
        selectInSidebar(query.isEmpty ? selectedSection : nil)
        rebuildForm()
    }

    @objc private func revealFiles(_ sender: Any?) {
        guard let doc else { return }
        var urls = [URL(fileURLWithPath: doc.configPath)]
        if doc.overlayExists { urls.append(URL(fileURLWithPath: doc.overlayPath)) }
        NSWorkspace.shared.activateFileViewerSelecting(urls.filter { FileManager.default.fileExists(atPath: $0.path) })
    }

    @objc private func editTable(_ sender: NSButton) {
        guard let key = identifierKey(sender), let item = setting(key), let doc else { return }
        let path = doc.editPath(for: item)
        if path == doc.overlayPath {
            openOverlay(sender)
        } else {
            NSWorkspace.shared.open(URL(fileURLWithPath: path))
        }
    }

    @objc private func openOverlay(_ sender: Any?) {
        let raw = thurm_settings_overlay_path()
        let path = raw.map { String(cString: $0) } ?? ""
        if let raw { thurm_string_free(raw) }
        guard !path.isEmpty else { return }
        NSWorkspace.shared.open(URL(fileURLWithPath: path))
        reload()
    }

    // MARK: Delegates

    func numberOfRows(in tableView: NSTableView) -> Int {
        sections.count
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let label = NSTextField(labelWithString: sections[row].label)
        label.font = .systemFont(ofSize: 13)
        return label
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        guard !selectingInSidebar else { return }
        let row = sectionList.selectedRow
        guard sections.indices.contains(row) else { return }
        selectedSection = sections[row].id
        query = ""
        search.stringValue = ""
        rebuildForm()
    }

    /// Shows `id` as the sidebar's selection, without the selection change that follows.
    private func selectInSidebar(_ id: String?) {
        selectingInSidebar = true
        defer { selectingInSidebar = false }
        if let index = sections.firstIndex(where: { $0.id == id }) {
            sectionList.selectRowIndexes(IndexSet(integer: index), byExtendingSelection: false)
        } else {
            sectionList.deselectAll(nil)
        }
    }

    /// Text fields commit when editing ends, unless nothing changed (a rebuild ends editing too).
    func controlTextDidEndEditing(_ obj: Notification) {
        guard let field = obj.object as? NSTextField, let key = identifierKey(field),
              let item = doc?.settings.first(where: { $0.key == key }) else { return }
        if item.kind == .list || item.kind == .text, field.stringValue != item.text {
            commit(key, field.stringValue)
        } else if item.kind == .int || item.kind == .float, field.stringValue != item.text {
            setNumber(key, field.stringValue)
        }
    }

}

/// A container whose origin is its top-left corner, so the form starts at the top.
private final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

/// The font families of this Mac: the monospaced ones first.
private enum FontFamilies {
    static let all: (monospaced: [String], other: [String]) = {
        var monospaced: [String] = []
        var other: [String] = []
        for family in NSFontManager.shared.availableFontFamilies {
            let face = NSFontManager.shared.availableMembers(ofFontFamily: family)?.first?.first as? String
            if let face, NSFont(name: face, size: 12)?.isFixedPitch == true {
                monospaced.append(family)
            } else {
                other.append(family)
            }
        }
        return (monospaced, other)
    }()
}

/// A slider that reports the end of a mouse interaction: `NSSlider` tracks the mouse inside
/// `mouseDown(with:)` and returns once it is released, whether or not a last action was sent.
private final class ReleaseSlider: NSSlider {
    var onRelease: ((NSSlider) -> Void)?

    override func mouseDown(with event: NSEvent) {
        super.mouseDown(with: event)
        onRelease?(self)
    }
}
