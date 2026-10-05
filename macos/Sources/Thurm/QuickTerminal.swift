import AppKit
import Carbon

/// Borderless panel of the quick terminal: above other apps' windows, on every Space (full
/// screen ones too), and never part of a tab group.
final class QuickTerminalWindow: NSPanel {
    init(contentRect: NSRect) {
        super.init(contentRect: contentRect, styleMask: [.borderless, .closable, .resizable],
                   backing: .buffered, defer: false)
        isFloatingPanel = true
        level = .floating
        // QuickTerminal hides it (autohide), not AppKit on every app switch.
        hidesOnDeactivate = false
        becomesKeyOnlyIfNeeded = false
        collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .ignoresCycle]
        animationBehavior = .none
        isExcludedFromWindowsMenu = true
    }

    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }

    /// Ordering front would pull the panel onto the screen, so the slide would start where it
    /// ends. QuickTerminal places it on the screen itself.
    override func constrainFrameRect(_ frameRect: NSRect, to screen: NSScreen?) -> NSRect { frameRect }

    /// There is no close button, which makes AppKit's performClose beep: Close Tab (⌘⇧W).
    override func performClose(_ sender: Any?) {
        if delegate?.windowShouldClose?(self) ?? true { close() }
    }

    /// Rounds the corners along the screen edge it hangs from, where the display's own corners
    /// are round (all four in the center).
    /// The frame view holds the terminal and the blur, so clipping it clips both.
    func roundCorners(for position: QuickTerminalPosition) {
        guard let frameView = contentView?.superview else { return }
        frameView.wantsLayer = true
        guard let layer = frameView.layer else { return }
        let flipped = frameView.isFlipped
        let top: CACornerMask = flipped ? [.layerMinXMinYCorner, .layerMaxXMinYCorner]
            : [.layerMinXMaxYCorner, .layerMaxXMaxYCorner]
        let bottom: CACornerMask = flipped ? [.layerMinXMaxYCorner, .layerMaxXMaxYCorner]
            : [.layerMinXMinYCorner, .layerMaxXMinYCorner]
        let left: CACornerMask = [.layerMinXMinYCorner, .layerMinXMaxYCorner]
        let right: CACornerMask = [.layerMaxXMinYCorner, .layerMaxXMaxYCorner]
        switch position {
        case .top: layer.maskedCorners = top
        case .bottom: layer.maskedCorners = bottom
        case .left: layer.maskedCorners = left
        case .right: layer.maskedCorners = right
        case .center: layer.maskedCorners = top.union(bottom)
        }
        layer.cornerRadius = 16
        layer.cornerCurve = .continuous
        layer.masksToBounds = true
        invalidateShadow()
    }

    override func toggleFullScreen(_ sender: Any?) {}
    override func mergeAllWindows(_ sender: Any?) {}
    override func moveTabToNewWindow(_ sender: Any?) {}
}

/// A terminal that slides in from a screen edge on a global hotkey (`[quick_terminal]`), like
/// Ghostty's quick terminal. Hiding only orders the panel out: its panes keep running, and
/// the layout keeps its tab (`Layout.quick`) so it comes back after a restart.
final class QuickTerminal {
    static let shared = QuickTerminal()

    private(set) var controller: TerminalWindowController?
    /// The app that was in front when the hotkey showed the panel; it gets focus back on hide.
    private var previousApp: NSRunningApplication?
    /// Bumped by every show and hide, so a finished animation of an earlier one is ignored.
    private var generation = 0
    private var hiding = false
    /// Fractions of the screen (width, height) after the user resized the panel; kept until
    /// the position or size changes in the config.
    private var resizedFraction: CGSize?
    private var configured: (position: QuickTerminalPosition, size: CGFloat)?

    private var hotKeyRef: EventHotKeyRef?
    private var hotKeySpec = ""
    private var handlerInstalled = false

    private var config: AppConfig { SessionManager.shared.config }

    var isShown: Bool {
        guard let w = controller?.window else { return false }
        return w.isVisible && !hiding
    }

    /// The quick terminal's tab for the saved layout.
    var tabLayout: TabLayout? {
        guard let c = controller, !c.isClosed, !c.content.isEmpty else { return nil }
        return c.content.tabLayout(title: c.titleOverride)
    }

    // MARK: Showing and hiding

    /// The hotkey: hide when it has focus, else bring it (back) in front.
    func toggle() {
        if isShown, NSApp.isActive, controller?.window?.isKeyWindow == true {
            hide(restoreFocus: true)
        } else {
            show()
        }
    }

    func show() {
        guard let screen = targetScreen() else { return }
        let frames = frames(on: screen)
        guard let c = controller ?? makeController(size: frames.shown.size), let w = c.window else { return }
        if !NSApp.isActive {
            let front = NSWorkspace.shared.frontmostApplication
            previousApp = front?.processIdentifier == ProcessInfo.processInfo.processIdentifier ? nil : front
        }
        generation += 1
        hiding = false
        let animate = config.quickAnimationDuration > 0
        let slides = config.quickPosition != .center
        (w as? QuickTerminalWindow)?.roundCorners(for: config.quickPosition)
        if !w.isVisible {
            w.setFrame(animate && slides ? frames.hidden : frames.shown, display: false)
            w.alphaValue = animate && !slides ? 0 : 1
        }
        w.makeKeyAndOrderFront(nil)
        w.makeFirstResponder(c.content.focusedView)
        NSApp.activate()
        animateTo(w, frame: frames.shown, alpha: 1, completion: nil)
    }

    /// `restoreFocus`: give focus back to the app the hotkey was pressed in.
    func hide(restoreFocus: Bool) {
        guard let w = controller?.window, w.isVisible, !hiding else { return }
        hiding = true
        generation += 1
        let current = generation
        let target = w.screen.map { frames(on: $0) } ?? (shown: w.frame, hidden: w.frame)
        let slides = config.quickPosition != .center
        animateTo(w, frame: slides ? target.hidden : w.frame, alpha: slides ? 1 : 0) { [weak self] in
            guard let self, current == self.generation else { return }
            self.hiding = false
            w.orderOut(nil)
            w.alphaValue = 1
            if restoreFocus, let app = self.previousApp, NSApp.isActive {
                app.activate()
            }
            self.previousApp = nil
        }
    }

    private func animateTo(_ w: NSWindow, frame: NSRect, alpha: CGFloat, completion: (() -> Void)?) {
        let duration = config.quickAnimationDuration
        guard duration > 0 else {
            w.setFrame(frame, display: true)
            w.alphaValue = alpha
            completion?()
            return
        }
        NSAnimationContext.runAnimationGroup({ ctx in
            ctx.duration = duration
            ctx.timingFunction = CAMediaTimingFunction(name: .easeOut)
            w.animator().setFrame(frame, display: true)
            w.animator().alphaValue = alpha
        }, completionHandler: completion)
    }

    /// Autohide: another app or another Thurm window took focus. Sheets, alerts and the
    /// command palette leave it up.
    func didResignKey() {
        guard config.quickAutohide, isShown else { return }
        DispatchQueue.main.async { [weak self] in
            guard let self, self.isShown else { return }
            if !NSApp.isActive {
                self.hide(restoreFocus: false)
            } else if let c = NSApp.keyWindow?.windowController as? TerminalWindowController, !c.isQuick {
                self.hide(restoreFocus: false)
            }
        }
    }

    // MARK: Controller

    private func makeController(size: NSSize) -> TerminalWindowController? {
        let manager = SessionManager.shared
        let grid = manager.gridSize(forPoints: size)
        guard let pane = manager.createPane(cols: grid.cols, rows: grid.rows, inheritFrom: nil) else { return nil }
        let c = manager.makeController(root: .leaf(pane), focused: pane, zoomed: nil, title: nil, frame: nil,
                                       quick: true)
        controller = c
        manager.scheduleLayoutSave()
        return c
    }

    /// From the saved layout, hidden until the hotkey shows it.
    func restore(_ tab: TabLayout) {
        guard controller == nil else { return }
        controller = SessionManager.shared.makeController(root: SplitNode(layout: tab.root), focused: tab.focusedKey,
                                                          zoomed: tab.zoomedKey, title: tab.title, frame: nil,
                                                          quick: true)
    }

    /// Its last pane ended (or it was closed): the next show starts a new shell.
    func controllerClosed(_ c: TerminalWindowController) {
        guard c === controller else { return }
        controller = nil
        hiding = false
        if let app = previousApp, NSApp.isActive { app.activate() }
        previousApp = nil
    }

    /// Keeps a size the user dragged the panel to for the next shows.
    func userResized() {
        guard let w = controller?.window, let screen = w.screen else { return }
        let vf = screen.visibleFrame
        resizedFraction = CGSize(width: min(1, w.frame.width / vf.width), height: min(1, w.frame.height / vf.height))
    }

    // MARK: Geometry

    private func targetScreen() -> NSScreen? {
        if config.quickOnMainScreen { return NSScreen.screens.first }
        let mouse = NSEvent.mouseLocation
        return NSScreen.screens.first { NSMouseInRect(mouse, $0.frame, false) } ?? NSScreen.main
    }

    /// Where it shows on `screen`, and where it slides in from (just past the edge).
    private func frames(on screen: NSScreen) -> (shown: NSRect, hidden: NSRect) {
        let vf = screen.visibleFrame
        let size = config.quickSize
        let position = config.quickPosition
        var fraction: CGSize
        switch position {
        case .top, .bottom: fraction = CGSize(width: 1, height: size)
        case .left, .right: fraction = CGSize(width: size, height: 1)
        case .center: fraction = CGSize(width: size, height: size)
        }
        if let r = resizedFraction {
            switch position {
            case .top, .bottom: fraction.height = r.height
            case .left, .right: fraction.width = r.width
            case .center: fraction = r
            }
        }
        let w = (vf.width * fraction.width).rounded()
        let h = (vf.height * fraction.height).rounded()
        var shown = NSRect(x: vf.minX, y: vf.minY, width: w, height: h)
        var hidden = shown
        switch position {
        case .top:
            shown.origin.y = vf.maxY - h
            hidden.origin.y = vf.maxY
        case .bottom:
            hidden.origin.y = vf.minY - h
        case .left:
            hidden.origin.x = vf.minX - w
        case .right:
            shown.origin.x = vf.maxX - w
            hidden.origin.x = vf.maxX
        case .center:
            shown.origin = NSPoint(x: vf.midX - w / 2, y: vf.midY - h / 2)
            hidden = shown
        }
        return (shown, hidden)
    }

    // MARK: Config and hotkey

    /// After (re)loading the config: registers the hotkey and applies position and size.
    /// Returns a message when the hotkey can't be used.
    @discardableResult
    func configChanged() -> String? {
        let now = (config.quickPosition, config.quickSize)
        if let old = configured, old != now { resizedFraction = nil }
        configured = now
        if isShown, let w = controller?.window, let screen = w.screen {
            w.setFrame(frames(on: screen).shown, display: true)
        }
        return registerHotkey(config.quickHotkey.trimmingCharacters(in: .whitespaces))
    }

    private func registerHotkey(_ spec: String) -> String? {
        if spec == hotKeySpec, spec.isEmpty || hotKeyRef != nil { return nil }
        if let ref = hotKeyRef {
            UnregisterEventHotKey(ref)
            hotKeyRef = nil
        }
        hotKeySpec = spec
        guard !spec.isEmpty else { return nil }
        guard let key = Self.parseHotkey(spec) else {
            return "Quick terminal: cannot use \"\(spec)\" as the hotkey (use a modifier, like ctrl+grave, or F1–F20)"
        }
        installHandler()
        var ref: EventHotKeyRef?
        let id = EventHotKeyID(signature: OSType(0x5448_524D), id: 1) // "THRM"
        let status = RegisterEventHotKey(key.code, key.modifiers, id, GetApplicationEventTarget(), 0, &ref)
        guard status == noErr, let ref else {
            tlog("RegisterEventHotKey(\(spec)) failed: \(status)")
            return status == eventHotKeyExistsErr
                ? "Quick terminal: \(spec) is already used by another app"
                : "Quick terminal: could not register \(spec) (\(status))"
        }
        hotKeyRef = ref
        return nil
    }

    private func installHandler() {
        guard !handlerInstalled else { return }
        var type = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        let status = InstallEventHandler(GetApplicationEventTarget(), { _, _, _ in
            DispatchQueue.main.async { QuickTerminal.shared.toggle() }
            return noErr
        }, 1, &type, nil, nil)
        handlerInstalled = status == noErr
        if status != noErr { tlog("InstallEventHandler failed: \(status)") }
    }

    /// `"ctrl+grave"`, `"cmd+shift+space"`, `"f12"`: Carbon key code and modifiers. Keys are
    /// physical positions on an ANSI keyboard (so "grave" is the key left of 1 everywhere).
    /// A key other than F1–F20 needs a modifier besides Shift: a global hotkey takes the
    /// combination from every app.
    static func parseHotkey(_ spec: String) -> (code: UInt32, modifiers: UInt32)? {
        let parts = spec.lowercased().split(separator: "+", omittingEmptySubsequences: false)
            .map { $0.trimmingCharacters(in: .whitespaces) }
        guard let keyName = parts.last, !keyName.isEmpty else { return nil }
        var modifiers: UInt32 = 0
        for m in parts.dropLast() {
            switch m {
            case "cmd", "command", "super": modifiers |= UInt32(cmdKey)
            case "ctrl", "control": modifiers |= UInt32(controlKey)
            case "alt", "opt", "option": modifiers |= UInt32(optionKey)
            case "shift": modifiers |= UInt32(shiftKey)
            default: return nil
            }
        }
        guard let code = keyCodes[keyName] else { return nil }
        let functionKey = keyName.hasPrefix("f") && Int(keyName.dropFirst()).map { (1...20).contains($0) } == true
        if modifiers & ~UInt32(shiftKey) == 0 && !functionKey { return nil }
        return (UInt32(code), modifiers)
    }

    private static let keyCodes: [String: Int] = {
        var map: [String: Int] = [
            "a": kVK_ANSI_A, "b": kVK_ANSI_B, "c": kVK_ANSI_C, "d": kVK_ANSI_D, "e": kVK_ANSI_E,
            "f": kVK_ANSI_F, "g": kVK_ANSI_G, "h": kVK_ANSI_H, "i": kVK_ANSI_I, "j": kVK_ANSI_J,
            "k": kVK_ANSI_K, "l": kVK_ANSI_L, "m": kVK_ANSI_M, "n": kVK_ANSI_N, "o": kVK_ANSI_O,
            "p": kVK_ANSI_P, "q": kVK_ANSI_Q, "r": kVK_ANSI_R, "s": kVK_ANSI_S, "t": kVK_ANSI_T,
            "u": kVK_ANSI_U, "v": kVK_ANSI_V, "w": kVK_ANSI_W, "x": kVK_ANSI_X, "y": kVK_ANSI_Y,
            "z": kVK_ANSI_Z,
            "0": kVK_ANSI_0, "1": kVK_ANSI_1, "2": kVK_ANSI_2, "3": kVK_ANSI_3, "4": kVK_ANSI_4,
            "5": kVK_ANSI_5, "6": kVK_ANSI_6, "7": kVK_ANSI_7, "8": kVK_ANSI_8, "9": kVK_ANSI_9,
            "grave": kVK_ANSI_Grave, "`": kVK_ANSI_Grave, "backtick": kVK_ANSI_Grave,
            "minus": kVK_ANSI_Minus, "-": kVK_ANSI_Minus, "equal": kVK_ANSI_Equal, "=": kVK_ANSI_Equal,
            "left_bracket": kVK_ANSI_LeftBracket, "[": kVK_ANSI_LeftBracket,
            "right_bracket": kVK_ANSI_RightBracket, "]": kVK_ANSI_RightBracket,
            "backslash": kVK_ANSI_Backslash, "\\": kVK_ANSI_Backslash,
            "semicolon": kVK_ANSI_Semicolon, ";": kVK_ANSI_Semicolon,
            "quote": kVK_ANSI_Quote, "'": kVK_ANSI_Quote,
            "comma": kVK_ANSI_Comma, ",": kVK_ANSI_Comma, "period": kVK_ANSI_Period, ".": kVK_ANSI_Period,
            "slash": kVK_ANSI_Slash, "/": kVK_ANSI_Slash,
            "space": kVK_Space, "tab": kVK_Tab, "return": kVK_Return, "enter": kVK_Return,
            "escape": kVK_Escape, "esc": kVK_Escape, "backspace": kVK_Delete,
        ]
        let functionKeys = [kVK_F1, kVK_F2, kVK_F3, kVK_F4, kVK_F5, kVK_F6, kVK_F7, kVK_F8, kVK_F9, kVK_F10,
                            kVK_F11, kVK_F12, kVK_F13, kVK_F14, kVK_F15, kVK_F16, kVK_F17, kVK_F18, kVK_F19,
                            kVK_F20]
        for (i, code) in functionKeys.enumerated() { map["f\(i + 1)"] = code }
        return map
    }()
}
