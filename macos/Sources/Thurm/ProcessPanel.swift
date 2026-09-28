import AppKit

/// Shell > Processes & Ports: what runs in every pane and which TCP ports it listens on (dev
/// servers, databases), like tty7's process panel. Click a row to focus its pane; double-click
/// one with a port to open http://localhost:PORT.
final class ProcessPanel: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSWindowDelegate {
    static let shared = ProcessPanel()

    private struct Row {
        let pane: UInt64
        let pid: UInt64
        let ports: [Int]
        let command: String
        let depth: Int
    }

    private var panel: NSPanel?
    private let table = NSTableView()
    private var rows: [Row] = []
    private var timer: Timer?
    private let portsOnly = NSButton(checkboxWithTitle: "Only processes with ports", target: nil, action: nil)

    func show() {
        if panel == nil { build() }
        refresh()
        panel?.makeKeyAndOrderFront(nil)
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in self?.refresh() }
    }

    private func build() {
        let p = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 720, height: 380),
                        styleMask: [.titled, .closable, .resizable, .utilityWindow], backing: .buffered, defer: false)
        p.title = "Processes & Ports"
        p.isFloatingPanel = true
        p.hidesOnDeactivate = false
        p.delegate = self
        p.center()

        for (id, title, width) in [("pane", "Pane", 60.0), ("pid", "PID", 70.0), ("ports", "Ports", 110.0),
                                   ("command", "Command", 460.0)] {
            let col = NSTableColumn(identifier: .init(id))
            col.title = title
            col.width = width
            table.addTableColumn(col)
        }
        table.usesAlternatingRowBackgroundColors = true
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.action = #selector(clicked(_:))
        table.doubleAction = #selector(doubleClicked(_:))

        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.translatesAutoresizingMaskIntoConstraints = false
        portsOnly.target = self
        portsOnly.action = #selector(togglePorts(_:))
        portsOnly.translatesAutoresizingMaskIntoConstraints = false

        let root = NSView()
        root.addSubview(scroll)
        root.addSubview(portsOnly)
        NSLayoutConstraint.activate([
            scroll.topAnchor.constraint(equalTo: root.topAnchor),
            scroll.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: portsOnly.topAnchor, constant: -8),
            portsOnly.leadingAnchor.constraint(equalTo: root.leadingAnchor, constant: 12),
            portsOnly.bottomAnchor.constraint(equalTo: root.bottomAnchor, constant: -10),
        ])
        p.contentView = root
        panel = p
    }

    private func refresh() {
        guard let resp = Core.shared.request(object: ["Processes": ["pane": NSNull()]]),
              let v = JSON.variant(resp), v.name == "Processes", let list = v.payload as? [[String: Any]]
        else { return }
        var out: [Row] = []
        for pane in list {
            guard let id = jsonUInt64(pane["pane"]), let procs = pane["processes"] as? [[String: Any]] else { continue }
            // Depth in the pane's tree, from the ppid chain (the list is parents first).
            var depth: [UInt64: Int] = [:]
            for p in procs {
                guard let pid = jsonUInt64(p["pid"]) else { continue }
                let ppid = jsonUInt64(p["ppid"]) ?? 0
                let d = depth[ppid].map { $0 + 1 } ?? 0
                depth[pid] = d
                let ports = (p["ports"] as? [Any])?.compactMap { jsonInt($0) } ?? []
                if portsOnly.state == .on && ports.isEmpty { continue }
                out.append(Row(pane: id, pid: pid, ports: ports, command: jsonString(p["command"]) ?? "", depth: d))
            }
        }
        rows = out
        table.reloadData()
    }

    @objc private func togglePorts(_ sender: Any?) { refresh() }

    @objc private func clicked(_ sender: Any?) {
        let r = table.clickedRow
        guard r >= 0, r < rows.count else { return }
        SessionManager.shared.focusPane(rows[r].pane, activate: true)
        panel?.makeKeyAndOrderFront(nil)
    }

    @objc private func doubleClicked(_ sender: Any?) {
        let r = table.clickedRow
        guard r >= 0, r < rows.count, let port = rows[r].ports.first,
              let url = URL(string: "http://localhost:\(port)") else { return }
        NSWorkspace.shared.open(url)
    }

    func windowWillClose(_ notification: Notification) {
        timer?.invalidate()
        timer = nil
    }

    // MARK: Table

    func numberOfRows(in tableView: NSTableView) -> Int { rows.count }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard row < rows.count, let id = tableColumn?.identifier.rawValue else { return nil }
        let r = rows[row]
        let text: String
        switch id {
        case "pane": text = "\(r.pane)"
        case "pid": text = "\(r.pid)"
        case "ports": text = r.ports.map(String.init).joined(separator: ", ")
        default: text = String(repeating: "  ", count: min(r.depth, 8)) + r.command
        }
        let label = NSTextField(labelWithString: text)
        label.font = id == "command" ? .monospacedSystemFont(ofSize: 11, weight: .regular)
            : .monospacedDigitSystemFont(ofSize: 11, weight: .regular)
        label.lineBreakMode = .byTruncatingTail
        if id == "ports" && !r.ports.isEmpty { label.textColor = .systemGreen }
        return label
    }
}
