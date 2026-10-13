import AppKit
import SwiftUI

/// The settings window's content: the sections in a sidebar, and the form of the chosen one (or
/// the settings matching the search).
struct SettingsView: View {
    @ObservedObject var store: SettingsStore

    var body: some View {
        NavigationSplitView {
            List(store.sections, selection: $store.selectedSection) { section in
                Label {
                    Text(section.label)
                } icon: {
                    SectionTile(section: section.id)
                }
                .tag(section.id)
            }
            .listStyle(.sidebar)
            .navigationSplitViewColumnWidth(min: 190, ideal: 210, max: 250)
        } detail: {
            DetailPane(store: store)
                .navigationSplitViewColumnWidth(min: 420, ideal: 540)
        }
        .searchable(text: $store.query, placement: .sidebar, prompt: "Search")
        .toolbar {
            ToolbarItem {
                Menu {
                    Button("Show Files in Finder") { store.revealFiles() }
                    Button("Open settings.toml") { store.openOverlay() }
                } label: {
                    Label("Files", systemImage: "folder")
                }
                .help("Show config.toml and settings.toml")
            }
        }
    }
}

/// A section's icon in the sidebar: a white symbol on a coloured rounded square.
private struct SectionTile: View {
    let section: String

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: 12, weight: .semibold))
            .foregroundStyle(.white)
            .frame(width: 22, height: 22)
            .background(tint, in: RoundedRectangle(cornerRadius: 6, style: .continuous))
    }

    private var symbol: String {
        let name = SettingsSectionIcons.symbol(for: section)
        return NSImage(systemSymbolName: name, accessibilityDescription: nil) == nil ? "brain" : name
    }

    private var tint: Color {
        switch section {
        case "appearance": return .purple
        case "font": return .blue
        case "window": return .teal
        case "terminal": return .gray
        case "session": return .orange
        case "security": return .indigo
        case "notifications": return .red
        case "agents": return .pink
        case "ai": return .mint
        case "programs": return .green
        case "updates": return .cyan
        case "quick_terminal": return .blue
        case "keys": return .gray
        case "remotes": return .teal
        default: return .gray
        }
    }
}

/// One group of rows: a section's rows, or the search results of one of its subgroups.
private struct FormGroup: Identifiable {
    let id: String
    let title: String?
    let group: SettingGroup
}

private struct DetailPane: View {
    @ObservedObject var store: SettingsStore

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(title)
                .font(.largeTitle.weight(.bold))
                .padding(.horizontal, 24)
                .padding(.top, 8)
                .padding(.bottom, 10)
            if let notice = store.doc?.notice {
                NoticeBar(notice: notice, store: store)
            }
            if store.doc == nil {
                ContentUnavailableView("Settings could not be read", systemImage: "exclamationmark.triangle")
            } else if groups.isEmpty {
                if store.query.isEmpty {
                    ContentUnavailableView("No settings", systemImage: "gearshape")
                } else {
                    ContentUnavailableView.search(text: store.query)
                }
            } else {
                Form {
                    ForEach(groups) { formGroup in
                        Section {
                            if formGroup.group.isMatrix {
                                NotificationMatrix(items: formGroup.group.items, store: store)
                            } else {
                                ForEach(formGroup.group.items, id: \.key) { item in
                                    if item.kind == .theme {
                                        ThemeRow(item: item, store: store, slot: .light)
                                        ThemeRow(item: item, store: store, slot: .dark)
                                    } else {
                                        SettingRow(item: item, store: store)
                                    }
                                }
                            }
                        } header: {
                            if let title = formGroup.title { Text(title) }
                        }
                    }
                }
                .formStyle(.grouped)
            }
        }
    }

    private var title: String {
        if !store.query.isEmpty { return "Search" }
        guard let id = store.selectedSection else { return "" }
        return store.doc?.label(ofSection: id) ?? id
    }

    private var groups: [FormGroup] {
        guard let doc = store.doc else { return [] }
        if store.query.isEmpty {
            guard let id = store.selectedSection else { return [] }
            return SettingGroups.split(doc.settings.filter { $0.section == id }).map { group in
                FormGroup(id: "\(id)/\(group.heading ?? "")", title: group.heading, group: group)
            }
        }
        return doc.groups(matching: store.query).flatMap { entry in
            SettingGroups.split(entry.items).map { group in
                let title = [doc.label(ofSection: entry.section.id), group.heading].compactMap { $0 }
                    .joined(separator: " \u{00B7} ")
                return FormGroup(id: "\(entry.section.id)/\(group.heading ?? "")", title: title, group: group)
            }
        }
    }
}

/// The one-line note on config.toml and settings.toml, above the form.
private struct NoticeBar: View {
    let notice: SettingsNotice
    let store: SettingsStore

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: notice.isError ? "exclamationmark.triangle.fill" : "info.circle.fill")
                .foregroundStyle(notice.isError ? Color.red : Color.secondary)
            Text(notice.text)
                .font(.callout)
                .lineLimit(1)
                .truncationMode(.tail)
                .help(notice.text)
            Spacer(minLength: 12)
            Button("Show in Finder") { store.revealFiles() }
                .buttonStyle(.link)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(notice.isError ? Color.red.opacity(0.12) : Color.primary.opacity(0.06),
                    in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .padding(.horizontal, 20)
        .padding(.bottom, 12)
    }
}

/// A row: the label with its description on the left, the control in a column of its own on the
/// right, and Reset (when settings.toml sets the value) after it.
private struct SettingRow: View {
    let item: SettingItem
    let store: SettingsStore

    var body: some View {
        LabeledContent {
            HStack(spacing: 8) {
                SettingControl(item: item, store: store)
                    .frame(width: 230, alignment: .trailing)
                ResetSlot(item: item, store: store)
            }
        } label: {
            RowLabel(item: item)
        }
    }
}

/// The Reset button of a row, or the same space left empty.
private struct ResetSlot: View {
    let item: SettingItem
    let store: SettingsStore

    var body: some View {
        Group {
            if item.isOverridden {
                Button {
                    store.commit(item.key, NSNull())
                } label: {
                    Image(systemName: "arrow.uturn.backward")
                }
                .buttonStyle(.borderless)
                .help("Reset to the config.toml or default value")
            } else {
                Color.clear
            }
        }
        .frame(width: 16)
    }
}

private struct RowLabel: View {
    let item: SettingItem
    var title: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(title ?? item.label)
                if item.isOverridden {
                    Circle()
                        .fill(Color.accentColor)
                        .frame(width: 6, height: 6)
                        .help("Set in settings.toml")
                }
                if let warning = item.warning {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.caption)
                        .foregroundStyle(.orange)
                        .help("settings.toml value ignored: \(warning)")
                }
            }
            let summary = SettingText.summary(item.doc)
            if !summary.isEmpty {
                Text(summary)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .help(item.doc)
    }
}

/// The control of one setting, by its kind.
private struct SettingControl: View {
    let item: SettingItem
    let store: SettingsStore

    var body: some View {
        switch item.kind {
        case .bool:
            Toggle("", isOn: Binding(
                get: { (item.value as? NSNumber)?.boolValue == true },
                set: { store.commit(item.key, $0) }
            ))
            .labelsHidden()
            .toggleStyle(.switch)
        case .int, .float:
            if let lo = item.min, let hi = item.max {
                RangedNumber(item: item, store: store, range: lo...hi)
            } else {
                TextValue(item: item, store: store, isNumber: true)
            }
        case .enum:
            Picker("", selection: Binding(
                get: { item.text },
                set: { store.commit(item.key, $0) }
            )) {
                ForEach(item.options, id: \.self) { option in
                    Text(SettingText.optionTitle(option)).tag(option)
                }
            }
            .labelsHidden()
            .fixedSize()
        case .theme:
            EmptyView()
        case .font:
            FontPicker(item: item, store: store)
        case .color:
            ColorControl(item: item, store: store)
        case .text, .list:
            TextValue(item: item, store: store, isNumber: false)
        case .table:
            TableControl(item: item, store: store)
        }
    }
}

/// A number with a range: a slider that saves on release (or on each key press), and a field
/// that saves when it is left or submitted.
private struct RangedNumber: View {
    let item: SettingItem
    let store: SettingsStore
    let range: ClosedRange<Double>
    @State private var value = 0.0
    @State private var text = ""
    @State private var dragging = false
    @FocusState private var focused: Bool

    /// A whole-number range too wide to drag through usefully (columns, scrollback) gets a
    /// stepper instead of a slider.
    private var usesSlider: Bool { item.kind != .int || range.upperBound - range.lowerBound <= 200 }

    var body: some View {
        HStack(spacing: 10) {
            if usesSlider {
                slider
                    .frame(width: 150)
            }
            TextField("", text: $text)
                .textFieldStyle(.roundedBorder)
                .multilineTextAlignment(.trailing)
                .frame(width: 64)
                .focused($focused)
                .onSubmit { applyText() }
                .onChange(of: focused) { _, isFocused in
                    if !isFocused { applyText() }
                }
            if !usesSlider {
                Stepper("", value: $value, in: range, step: 1)
                    .labelsHidden()
            }
        }
        .onAppear(perform: sync)
        .onChange(of: item.text) { _, _ in sync() }
        .onChange(of: value) { _, new in
            text = displayText(new)
            if !dragging { save() }
        }
    }

    @ViewBuilder private var slider: some View {
        // No `step:` even for whole numbers: SwiftUI then lays out a tick mark per step on every
        // update, which made wide ranges take most of a second to show. The value is rounded
        // instead.
        if item.kind == .int {
            Slider(value: Binding(get: { value }, set: { value = $0.rounded() }), in: range,
                   onEditingChanged: editingChanged)
        } else {
            Slider(value: $value, in: range, onEditingChanged: editingChanged)
        }
    }

    private func editingChanged(_ editing: Bool) {
        dragging = editing
        if !editing { save() }
    }

    private func sync() {
        value = numericValue(item.value) ?? range.lowerBound
        text = displayText(value)
    }

    /// Saves the slider's value unless it is the saved one, so a click that changes nothing
    /// does not turn the value into an override.
    private func save() {
        guard let chosen = SettingNumber.value(value, kind: item.kind) else {
            sync()
            return
        }
        if let current = numericValue(item.value), current == numericValue(chosen) { return }
        store.commit(item.key, chosen)
    }

    private func applyText() {
        guard text != item.text else { return }
        guard let number = SettingNumber.parse(text), range.contains(number),
              let chosen = SettingNumber.value(number, kind: item.kind)
        else {
            NSSound.beep()
            sync()
            return
        }
        if let current = numericValue(item.value), current == numericValue(chosen) {
            sync()
            return
        }
        store.commit(item.key, chosen)
    }
}

/// A text or number field that saves when it is submitted or left.
private struct TextValue: View {
    let item: SettingItem
    let store: SettingsStore
    let isNumber: Bool
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        if item.kind == .list && !SettingList.isEditableAsText(item.value) {
            HStack(spacing: 8) {
                Text("\(SettingList.elements(item.value).count) items")
                    .foregroundStyle(.secondary)
                Button("Edit in File\u{2026}") { store.editTable(item) }
                    .controlSize(.small)
                    .help("An item contains a comma, so the list is edited in the file.")
            }
        } else {
            field
        }
    }

    private var field: some View {
        TextField(item.kind == .list ? "Comma-separated" : "", text: $text)
            .textFieldStyle(.roundedBorder)
            .multilineTextAlignment(isNumber ? .trailing : .leading)
            .frame(width: isNumber ? 80 : 230)
            .focused($focused)
            .onSubmit { apply() }
            .onChange(of: focused) { _, isFocused in
                if !isFocused { apply() }
            }
            .onAppear { text = item.text }
            .onChange(of: item.text) { _, new in text = new }
    }

    private func apply() {
        guard text != item.text else { return }
        guard isNumber else {
            store.commit(item.key, item.kind == .list ? SettingList.parse(text) as Any : text)
            return
        }
        guard let number = SettingNumber.parse(text), let value = SettingNumber.value(number, kind: item.kind)
        else {
            NSSound.beep()
            text = item.text
            return
        }
        store.commit(item.key, value)
    }
}

/// A font face or family: the monospaced families first, and "Same as family" where it may be
/// unset.
private struct FontPicker: View {
    let item: SettingItem
    let store: SettingsStore
    private static let sameAsFamily = "Same as family"

    /// The bold, italic and bold italic settings pick a face: the family's own faces come first.
    private var faces: [(name: String, title: String)] {
        guard item.key != "font.family",
              let family = store.doc?.settings.first(where: { $0.key == "font.family" })?.text,
              let members = NSFontManager.shared.availableMembers(ofFontFamily: family)
        else { return [] }
        return members.compactMap { member in
            guard let name = member.first as? String else { return nil }
            let style = member.count > 1 ? (member[1] as? String) ?? name : name
            return (name, "\(family) \(style)")
        }
    }

    var body: some View {
        let families = FontFamilies.all
        let faces = self.faces
        var current = item.text
        if !item.isNullable && current.isEmpty { current = families.monospaced.first ?? "" }
        let selected = current.isEmpty ? Self.sameAsFamily : current
        let known = Set(families.monospaced + families.other + faces.map(\.name) + [Self.sameAsFamily])
        return Picker("", selection: Binding(
            get: { selected },
            set: { title in store.commit(item.key, title == Self.sameAsFamily ? NSNull() : title) }
        )) {
            if item.isNullable {
                Text(Self.sameAsFamily).tag(Self.sameAsFamily)
            }
            if !known.contains(selected) {
                Text(selected).tag(selected)
            }
            if !faces.isEmpty {
                Divider()
                ForEach(faces, id: \.name) { face in
                    Text(face.title).tag(face.name)
                }
                Divider()
            }
            ForEach(families.monospaced, id: \.self) { family in
                Text(family).tag(family)
            }
            if !families.monospaced.isEmpty {
                Divider()
            }
            ForEach(families.other, id: \.self) { family in
                Text(family).tag(family)
            }
        }
        .labelsHidden()
        .fixedSize()
    }
}

/// A color: the theme's own (shown faded), config.toml's, or a custom one picked with a color
/// well. Only settings.toml can be reset here, so when config.toml sets the color the choice is
/// between that and a custom one, not the theme's.
private struct ColorControl: View {
    let item: SettingItem
    let store: SettingsStore
    @State private var color = Color.gray

    private enum Mode: Hashable { case theme, file, custom }

    private var fileColor: String? { item.fileValue as? String }

    private var mode: Mode {
        if item.source == .overlay, item.value is String { return .custom }
        return fileColor == nil ? .theme : .file
    }

    var body: some View {
        HStack(spacing: 8) {
            Picker("", selection: Binding(
                get: { mode },
                set: { choose($0) }
            )) {
                if fileColor == nil {
                    Text("Theme default").tag(Mode.theme)
                } else {
                    Text("config.toml").tag(Mode.file)
                }
                Text("Custom").tag(Mode.custom)
            }
            .labelsHidden()
            .fixedSize()
            switch mode {
            case .custom:
                ColorPicker("", selection: $color, supportsOpacity: false)
                    .labelsHidden()
            case .file:
                swatch(fileColor.flatMap(HexColor.parse).map(nsColor) ?? .gray, opacity: 1)
            case .theme:
                swatch(store.themeColor(for: item.key) ?? .gray, opacity: 0.5)
            }
        }
        .onAppear(perform: sync)
        .onChange(of: item.text) { _, _ in sync() }
        .onChange(of: color) { _, new in saveColor(new) }
    }

    private func swatch(_ fill: NSColor, opacity: Double) -> some View {
        RoundedRectangle(cornerRadius: 4, style: .continuous)
            .fill(Color(nsColor: fill).opacity(opacity))
            .frame(width: 28, height: 18)
            .overlay(RoundedRectangle(cornerRadius: 4, style: .continuous).stroke(.separator))
    }

    private func nsColor(_ rgb: (red: Int, green: Int, blue: Int)) -> NSColor {
        NSColor(srgbRed: CGFloat(rgb.red) / 255, green: CGFloat(rgb.green) / 255, blue: CGFloat(rgb.blue) / 255,
                alpha: 1)
    }

    private func sync() {
        guard let rgb = (item.value as? String).flatMap(HexColor.parse) else { return }
        color = Color(red: Double(rgb.red) / 255, green: Double(rgb.green) / 255, blue: Double(rgb.blue) / 255)
    }

    private func choose(_ new: Mode) {
        guard new != mode else { return }
        switch new {
        case .custom:
            let start = (item.value as? String).flatMap(HexColor.parse).map(nsColor)
                ?? store.themeColor(for: item.key) ?? .gray
            store.commit(item.key, hexString(start))
        case .theme, .file:
            store.commit(item.key, NSNull())
        }
    }

    private func saveColor(_ new: Color) {
        guard mode == .custom else { return }
        let hex = hexString(NSColor(new))
        guard let saved = (item.value as? String).flatMap(HexColor.parse) else { return }
        if hexString(saved) == hex { return }
        store.commit(item.key, hex)
    }
}

/// A table or list of tables: its entry count, and a button to edit it in the file that sets it.
private struct TableControl: View {
    let item: SettingItem
    let store: SettingsStore

    var body: some View {
        let inConfig = item.source == .file
        HStack(spacing: 8) {
            Text(summary)
                .foregroundStyle(.secondary)
            Button(inConfig ? "Open config.toml\u{2026}" : "Edit in settings.toml\u{2026}") {
                store.editTable(item)
            }
            .controlSize(.small)
            .help(tooltip(inConfig: inConfig))
        }
    }

    private var summary: String {
        if let list = item.value as? [Any] { return list.isEmpty ? "None" : "\(list.count) items" }
        if let map = item.value as? [String: Any] { return map.isEmpty ? "None" : "\(map.count) entries" }
        return "None"
    }

    private func tooltip(inConfig: Bool) -> String {
        if inConfig {
            return store.doc?.configReadOnly == true
                ? "config.toml is managed by another tool: change these entries there. "
                    + "Entries in settings.toml would replace them all."
                : ""
        }
        return "Entries in settings.toml replace any in config.toml."
    }
}

/// The theme's light or dark row: a menu of the installed themes, and Other… for a file.
private struct ThemeRow: View {
    enum Slot { case light, dark }

    let item: SettingItem
    let store: SettingsStore
    let slot: Slot

    private var spec: (light: String, dark: String) { ColorThemeSpec.parse(item.text) }
    private var current: String { slot == .light ? spec.light : spec.dark }
    private var names: [String] { SessionManager.shared.config.themes.map(\.name) }

    var body: some View {
        LabeledContent {
            HStack(spacing: 8) {
                Menu {
                    let choices = names.contains(current) ? names : names + [current]
                    ForEach(choices, id: \.self) { name in
                        Button {
                            choose(name)
                        } label: {
                            if name == current {
                                Label(displayName(name), systemImage: "checkmark")
                            } else {
                                Text(displayName(name))
                            }
                        }
                    }
                    Divider()
                    Button("Other\u{2026}") {
                        if let path = store.chooseThemeFile() { choose(path) }
                    }
                } label: {
                    Text(displayName(current))
                }
                .fixedSize()
            }
            .frame(width: 230, alignment: .trailing)
            if slot == .light {
                ResetSlot(item: item, store: store)
            } else {
                Color.clear.frame(width: 16)
            }
        } label: {
            if slot == .light {
                RowLabel(item: item, title: "Light")
            } else {
                Text("Dark")
            }
        }
    }

    private func choose(_ name: String) {
        let encoded = slot == .light
            ? ColorThemeSpec.encode(light: name, dark: spec.dark)
            : ColorThemeSpec.encode(light: spec.light, dark: name)
        store.commit(item.key, encoded)
    }

    private func displayName(_ name: String) -> String {
        name.contains("/") ? URL(fileURLWithPath: name).deletingPathExtension().lastPathComponent : name
    }
}

/// The notification states as rows and the fields as columns, with a Reset per state.
private struct NotificationMatrix: View {
    let items: [SettingItem]
    let store: SettingsStore

    private var states: [String] { unique(items.compactMap(\.row)) }
    private var columns: [String] { unique(items.compactMap(\.column)) }

    var body: some View {
        Grid(alignment: .center, horizontalSpacing: 20, verticalSpacing: 12) {
            GridRow {
                Text("")
                ForEach(columns, id: \.self) { column in
                    Text(columnTitle(column))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Color.clear.frame(width: 16)
            }
            ForEach(states, id: \.self) { state in
                GridRow {
                    Text(stateTitle(state))
                        .gridColumnAlignment(.leading)
                    ForEach(columns, id: \.self) { column in
                        cell(state: state, column: column)
                    }
                    if items.contains(where: { $0.row == state && $0.isOverridden }) {
                        ResetState(key: "notifications.\(state)", store: store)
                    } else {
                        Color.clear.frame(width: 16)
                    }
                }
            }
        }
        .padding(.vertical, 4)
    }

    @ViewBuilder private func cell(state: String, column: String) -> some View {
        if let item = items.first(where: { $0.row == state && $0.column == column }) {
            if column == "sound" {
                SoundPicker(item: item, store: store)
            } else {
                SettingControl(item: item, store: store)
            }
        } else {
            Text("")
        }
    }

    private func unique(_ values: [String]) -> [String] {
        var seen = Set<String>()
        return values.filter { seen.insert($0).inserted }
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
}

private struct ResetState: View {
    let key: String
    let store: SettingsStore

    var body: some View {
        Button {
            store.commit(key, NSNull())
        } label: {
            Image(systemName: "arrow.uturn.backward")
        }
        .buttonStyle(.borderless)
        .help("Reset this state to the config.toml or default values")
    }
}

/// A notification sound: the default, none, or a sound of the system or the user.
private struct SoundPicker: View {
    let item: SettingItem
    let store: SettingsStore
    private static let sounds = SystemSounds.names()

    var body: some View {
        let known = ["default", "none"] + Self.sounds
        return Picker("", selection: Binding(
            get: { item.text },
            set: { store.commit(item.key, $0) }
        )) {
            Text("Default").tag("default")
            Text("None").tag("none")
            if !known.contains(item.text) {
                Text(item.text).tag(item.text)
            }
            Divider()
            ForEach(Self.sounds, id: \.self) { sound in
                Text(sound).tag(sound)
            }
        }
        .labelsHidden()
        .fixedSize()
    }
}

private func numericValue(_ value: Any?) -> Double? {
    if let whole = value as? Int { return Double(whole) }
    return (value as? NSNumber)?.doubleValue
}

private func displayText(_ value: Double) -> String {
    value == value.rounded() ? String(Int(value)) : String(value)
}

private func hexString(_ color: NSColor) -> String {
    let rgb = color.usingColorSpace(.sRGB) ?? color
    return HexColor.format(red: Int(round(rgb.redComponent * 255)),
                           green: Int(round(rgb.greenComponent * 255)),
                           blue: Int(round(rgb.blueComponent * 255)))
}

private func hexString(_ rgb: (red: Int, green: Int, blue: Int)) -> String {
    HexColor.format(red: rgb.red, green: rgb.green, blue: rgb.blue)
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
