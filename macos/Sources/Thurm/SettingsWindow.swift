import AppKit
import CThurm
import SwiftUI

/// Thurm > Settings… (⌘,): every option of config.toml. Changes are saved to settings.toml
/// beside it (config.toml may be a read-only link), and apply at once. The window is SwiftUI
/// (`SettingsView`) in an NSWindow; rules and values come from `SettingsModel`, and the Rust side
/// is `thurm_settings_json` and `thurm_settings_set`.
final class SettingsWindow: NSObject {
    static let shared = SettingsWindow()

    private var window: NSWindow?
    private let store = SettingsStore()

    func show() {
        if window == nil { build() }
        store.reload()
        window?.makeKeyAndOrderFront(nil)
    }

    /// Shows the window on `section` (leaving a search).
    func show(section: String) {
        show()
        store.selectedSection = section
    }

    /// Re-reads the settings while the window is open (a host added or changed elsewhere).
    func reload() {
        if window?.isVisible == true { store.reload() }
    }

    private func build() {
        let controller = NSHostingController(rootView: SettingsView(store: store))
        controller.sizingOptions = []
        let w = NSWindow(contentViewController: controller)
        w.title = "Settings"
        w.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        w.isReleasedWhenClosed = false
        w.toolbarStyle = .unified
        w.setContentSize(NSSize(width: 780, height: 560))
        w.contentMinSize = NSSize(width: 640, height: 420)
        // A frame saved by an earlier launch wins; only a first open is centered.
        if !w.setFrameUsingName("SettingsWindow") { w.center() }
        w.setFrameAutosaveName("SettingsWindow")
        store.window = w
        window = w
    }
}

/// The settings the window shows, and the saves that go back to Rust.
final class SettingsStore: ObservableObject {
    @Published private(set) var doc: SettingsDocument?
    /// The section shown, or `nil` while searching, so that clicking any section (even the one
    /// shown before the search) leaves the results for it.
    @Published var selectedSection: String? {
        didSet {
            if selectedSection != nil && !query.isEmpty { query = "" }
        }
    }
    @Published var query = "" {
        didSet {
            if query.isEmpty {
                if selectedSection == nil { selectedSection = sectionBeforeSearch }
            } else if let shown = selectedSection {
                sectionBeforeSearch = shown
                selectedSection = nil
            }
        }
    }
    private var sectionBeforeSearch: String?
    weak var window: NSWindow?

    var sections: [SettingSection] { doc?.populatedSections ?? [] }

    /// Re-reads the settings from Rust and shows them again.
    func reload() {
        let raw = thurm_settings_json()
        let text = raw.map { String(cString: $0) } ?? ""
        if let raw { thurm_string_free(raw) }
        doc = SettingsDocument(json: text)
        let ids = sections.map(\.id)
        if !query.isEmpty {
            if sectionBeforeSearch.map(ids.contains) != true { sectionBeforeSearch = ids.first }
            return
        }
        if let id = selectedSection, ids.contains(id) { return }
        selectedSection = ids.first
    }

    func setting(_ key: String) -> SettingItem? {
        doc?.settings.first { $0.key == key }
    }

    /// The setting `key` as the control holds it: `value`, or NSNull to reset it.
    func commit(_ key: String, _ value: Any) {
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

    func revealFiles() {
        guard let doc else { return }
        var urls = [URL(fileURLWithPath: doc.configPath)]
        if doc.overlayExists { urls.append(URL(fileURLWithPath: doc.overlayPath)) }
        NSWorkspace.shared.activateFileViewerSelecting(urls.filter { FileManager.default.fileExists(atPath: $0.path) })
    }

    func openOverlay() {
        let raw = thurm_settings_overlay_path()
        let path = raw.map { String(cString: $0) } ?? ""
        if let raw { thurm_string_free(raw) }
        guard !path.isEmpty else { return }
        NSWorkspace.shared.open(URL(fileURLWithPath: path))
        reload()
    }

    /// Opens the file a value the window can't edit is set in (see `SettingsDocument.editPath`).
    func editTable(_ item: SettingItem) {
        guard let doc else { return }
        let path = doc.editPath(for: item)
        if path == doc.overlayPath {
            openOverlay()
        } else {
            NSWorkspace.shared.open(URL(fileURLWithPath: path))
        }
    }

    /// A theme file, chosen in an open panel. Nil when the panel is cancelled.
    func chooseThemeFile() -> String? {
        let panel = NSOpenPanel()
        panel.message = "Choose a theme file (Thurm's TOML, or an iTerm2-style palette)."
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        return url.path
    }

    /// The color a theme gives `key`, for a color that is not set.
    func themeColor(for key: String) -> NSColor? {
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
}
