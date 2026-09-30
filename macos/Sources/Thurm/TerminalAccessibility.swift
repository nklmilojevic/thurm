import AppKit

/// The visible screen as VoiceOver reads it: a text area with one line per row, the cursor as
/// the insertion point, and new output announced.
final class TerminalAccessibility {
    /// Rows joined by newlines, trailing blanks trimmed.
    private(set) var text = ""
    /// UTF-16 offset where each row starts in `text`.
    private(set) var lineStarts: [Int] = []
    /// Column of every UTF-16 unit of each row, and the column after its glyph (a wide one
    /// takes two).
    private var columns: [[Int]] = []
    private var ends: [[Int]] = []
    private var lines: [String] = []
    private var generation: UInt64 = .max
    /// When a key was last typed: its echo is not announced (VoiceOver speaks typed keys).
    var lastKey: CFTimeInterval = 0
    private var pending: [String] = []
    private var announceScheduled = false

    var length: Int { (text as NSString).length }

    /// Rebuilds the model from the grid when it changed. Returns the rows with new text.
    @discardableResult
    func update(_ snapshot: GridSnapshot) -> [String] {
        guard snapshot.valid, snapshot.info.generation != generation || lines.count != snapshot.rows else {
            return []
        }
        generation = snapshot.info.generation
        var newLines: [String] = []
        var newColumns: [[Int]] = []
        var newEnds: [[Int]] = []
        for r in 0..<snapshot.rows {
            let (row, map) = snapshot.rowText(r)
            let trimmed = row.replacingOccurrences(of: "\\s+$", with: "", options: .regularExpression)
            let units = (trimmed as NSString).length
            newLines.append(trimmed)
            newColumns.append(Array(map.prefix(units)))
            // The next glyph's column (spacers aren't in the map), or the row's end.
            var end = Array(repeating: snapshot.cols, count: units)
            var next = snapshot.cols
            for k in stride(from: map.count - 1, through: 0, by: -1) {
                if k + 1 < map.count, map[k + 1] != map[k] { next = map[k + 1] }
                if k < units { end[k] = next }
            }
            newEnds.append(end)
        }
        let added = TerminalAccessibility.newRows(old: lines, new: newLines)
        lines = newLines
        columns = newColumns
        ends = newEnds
        var starts: [Int] = []
        var offset = 0
        for l in lines {
            starts.append(offset)
            offset += (l as NSString).length + 1
        }
        lineStarts = starts
        text = lines.joined(separator: "\n")
        return added
    }

    /// Rows of `new` that aren't in `old`, after lining the two up for a scroll.
    static func newRows(old: [String], new: [String]) -> [String] {
        guard !old.isEmpty, old.count == new.count else { return new.filter { !$0.isEmpty } }
        let n = new.count
        // The scroll that keeps the most rows in place, either way (row i of the new screen
        // was row i + shift), the smallest on a tie.
        var best = 0
        var bestSame = -1
        for step in 0..<(2 * n - 1) {
            let shift = step % 2 == 0 ? step / 2 : -(step + 1) / 2
            var same = 0
            for i in max(0, -shift)..<min(n, n - shift) where new[i] == old[i + shift] { same += 1 }
            if same > bestSame {
                bestSame = same
                best = shift
            }
        }
        var out: [String] = []
        for i in 0..<n {
            let j = i + best
            if (j < 0 || j >= n || new[i] != old[j]) && !new[i].isEmpty { out.append(new[i]) }
        }
        return out
    }

    func line(for index: Int) -> Int {
        guard !lineStarts.isEmpty else { return 0 }
        var lo = 0
        var hi = lineStarts.count - 1
        while lo < hi {
            let mid = (lo + hi + 1) / 2
            if lineStarts[mid] <= index { lo = mid } else { hi = mid - 1 }
        }
        return lo
    }

    func range(forLine line: Int) -> NSRange {
        guard line >= 0, line < lines.count else { return NSRange(location: NSNotFound, length: 0) }
        return NSRange(location: lineStarts[line], length: (lines[line] as NSString).length)
    }

    /// Offset of a cell: its first unit, or the row's end when the row stops before it.
    func index(col: Int, row: Int) -> Int {
        guard row >= 0, row < lines.count else { return length }
        let cols = columns[row]
        let within = cols.firstIndex { $0 >= col } ?? cols.count
        return lineStarts[row] + within
    }

    /// Cells of an offset's glyph (`end` exclusive), for the frame of a range.
    func cell(at index: Int) -> (col: Int, end: Int, row: Int) {
        let row = line(for: index)
        guard row < columns.count else { return (0, 1, 0) }
        let i = index - lineStarts[row]
        let cols = columns[row]
        if i >= 0, i < cols.count { return (cols[i], ends[row][i], row) }
        // The newline, or past the text: the cell after it.
        let col = ends[row].last ?? 0
        return (col, col + 1, row)
    }

    /// Speaks new output a moment after it stops arriving, so a burst is one announcement.
    func announce(_ rows: [String], from element: NSView) {
        guard !rows.isEmpty else { return }
        pending.append(contentsOf: rows)
        if pending.count > 20 { pending.removeFirst(pending.count - 20) }
        guard !announceScheduled else { return }
        announceScheduled = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self, weak element] in
            guard let self, let element else { return }
            self.announceScheduled = false
            let message = self.pending.joined(separator: "\n")
            self.pending.removeAll()
            guard !message.isEmpty else { return }
            NSAccessibility.post(element: element, notification: .announcementRequested, userInfo: [
                .announcement: String(message.prefix(1000)),
                .priority: NSAccessibilityPriorityLevel.medium.rawValue,
            ])
        }
    }
}

extension TerminalView {
    /// After the grid changed: keeps VoiceOver's copy current and announces new output of the
    /// focused pane. Nothing is done while VoiceOver is off.
    func accessibilityGridChanged() {
        guard NSWorkspace.shared.isVoiceOverEnabled else { return }
        let before = accessibilityModel.length
        let added = accessibilityModel.update(accessibilitySnapshot)
        NSAccessibility.post(element: self, notification: .valueChanged)
        NSAccessibility.post(element: self, notification: .selectedTextChanged)
        if before != accessibilityModel.length {
            NSAccessibility.post(element: self, notification: .layoutChanged)
        }
        guard window?.isKeyWindow == true, window?.firstResponder === self else { return }
        let info = accessibilitySnapshot.info
        // The echo of what was just typed: only the cursor's row changed.
        let cursorLine = accessibilityModel.range(forLine: Int(info.cursor_row))
        let cursorText = cursorLine.location == NSNotFound
            ? "" : (accessibilityModel.text as NSString).substring(with: cursorLine)
        if CACurrentMediaTime() - accessibilityModel.lastKey < 0.5, added.count <= 1,
           added.first.map({ $0 == cursorText }) ?? true {
            return
        }
        accessibilityModel.announce(added, from: self)
    }

    private var a11y: TerminalAccessibility {
        accessibilityModel.update(accessibilitySnapshot)
        return accessibilityModel
    }

    override func isAccessibilityElement() -> Bool { true }

    override func accessibilityRole() -> NSAccessibility.Role? { .textArea }

    override func accessibilityRoleDescription() -> String? { "terminal" }

    override func accessibilityLabel() -> String? {
        let title = window?.title ?? ""
        return title.isEmpty ? "Terminal" : title
    }

    override func accessibilityValue() -> Any? { a11y.text }

    override func accessibilityNumberOfCharacters() -> Int { a11y.length }

    override func accessibilityVisibleCharacterRange() -> NSRange {
        NSRange(location: 0, length: a11y.length)
    }

    override func accessibilitySelectedText() -> String? { "" }

    override func accessibilitySelectedTextRange() -> NSRange {
        let info = accessibilitySnapshot.info
        return NSRange(location: a11y.index(col: Int(info.cursor_col), row: Int(info.cursor_row)), length: 0)
    }

    override func accessibilityInsertionPointLineNumber() -> Int {
        Int(accessibilitySnapshot.info.cursor_row)
    }

    override func accessibilityLine(for index: Int) -> Int { a11y.line(for: index) }

    override func accessibilityRange(forLine line: Int) -> NSRange { a11y.range(forLine: line) }

    override func accessibilityString(for range: NSRange) -> String? {
        let text = a11y.text as NSString
        guard range.location != NSNotFound, NSMaxRange(range) <= text.length else { return nil }
        return text.substring(with: range)
    }

    override func accessibilityAttributedString(for range: NSRange) -> NSAttributedString? {
        accessibilityString(for: range).map { NSAttributedString(string: $0) }
    }

    override func accessibilityFrame(for range: NSRange) -> NSRect {
        guard let window, range.location != NSNotFound else { return .zero }
        let model = a11y
        let start = model.cell(at: range.location)
        let end = model.cell(at: max(range.location, NSMaxRange(range) - 1))
        let cell = accessibilityCellSize
        let pad = accessibilityPadding
        let rect: NSRect
        if start.row == end.row {
            rect = NSRect(x: pad.x + CGFloat(start.col) * cell.width, y: pad.y + CGFloat(start.row) * cell.height,
                          width: CGFloat(max(1, end.end - start.col)) * cell.width, height: cell.height)
        } else {
            // Across rows: the full width of every row in it.
            rect = NSRect(x: pad.x, y: pad.y + CGFloat(start.row) * cell.height,
                          width: CGFloat(accessibilitySnapshot.cols) * cell.width,
                          height: CGFloat(end.row - start.row + 1) * cell.height)
        }
        return window.convertToScreen(convert(rect, to: nil))
    }
}
