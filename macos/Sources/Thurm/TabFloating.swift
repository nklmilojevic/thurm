import AppKit

typealias TabGap = (tab: TabItem, after: Bool)

extension SessionManager {
    func floatSources(into c: TerminalWindowController) -> [TerminalWindowController] {
        group(of: c).filter { canFloat($0, into: c) }
    }

    func floatTabSources(into dst: TabItem) -> [TabItem] {
        guard let c = dst.windowController else { return [] }
        return windowTabs(of: c).filter { canFloat($0, into: dst) }
    }

    func canFloat(_ src: TabItem, into dst: TabItem) -> Bool {
        guard let dstController = dst.windowController, let srcController = src.windowController else { return false }
        return src !== dst && canMoveCards(src) && canMoveCards(dst) && src.content.root.panes.count == 1
            && src.content.host == dst.content.host
            && windowTabs(of: dstController).contains { $0 === src }
            && (srcController === dstController || group(of: dstController).contains { $0 === srcController })
    }

    func canFloat(_ src: TerminalWindowController, into c: TerminalWindowController) -> Bool {
        canFloat(src.selectedTab, into: c.selectedTab)
    }

    func canMoveCards(_ tab: TabItem) -> Bool {
        guard let c = tab.windowController else { return false }
        return !c.isClosed && !tab.isClosed && !c.isQuick && tab.wall == nil && tab.handoffID == nil
            && !isWallOrigin(tab)
    }

    func canMoveCards(_ c: TerminalWindowController) -> Bool {
        canMoveCards(c.selectedTab)
    }

    func floatTab(_ src: TabItem, into dst: TabItem, at point: CGPoint? = nil) {
        guard canFloat(src, into: dst), let pane = src.content.root.panes.first,
              let srcController = src.windowController, let dstController = dst.windowController else { return }
        let frame = point.map { dst.content.centeredCardFrame(at: $0) }
            ?? FloatingGeometry.defaultFraction(index: dst.content.floating.count)
        let cards = src.content.floating.map(\.saved) + [FloatingCard(pane: pane, frame: frame).saved]
        for card in cards { src.content.remove(pane: card.pane) }
        dst.content.addFloating(cards, hidden: false)
        if srcController === dstController && srcController.usesSingleWindowTabs {
            _ = srcController.removeTab(src, detach: true)
            dstController.selectTab(dst, focus: false)
        } else {
            srcController.closingWithoutConfirmation = true
            srcController.window?.close()
        }
        dstController.window?.makeKeyAndOrderFront(nil)
        dst.content.focus(pane)
        dstController.updateTitle()
        focusChanged()
        scheduleLayoutSave()
    }

    func floatTab(_ src: TerminalWindowController, into c: TerminalWindowController, at point: CGPoint? = nil) {
        floatTab(src.selectedTab, into: c.selectedTab, at: point)
    }

    func moveCardToNewTab(_ pane: PaneKey, from tab: TabItem, next: TabGap? = nil) {
        guard canMoveCards(tab), tab.content.isFloating(pane), let c = tab.windowController else { return }
        tab.content.remove(pane: pane)
        if tab.isSelected { c.updateTitle() } else { refreshSidebars() }
        let anchor = next?.tab ?? tab
        let after = next?.after ?? true
        if c.usesSingleWindowTabs && group(of: c).count == 1 {
            let newItem = TabItem(root: .leaf(pane), focused: pane, zoomed: nil)
            let anchorIdx = c.tabs.firstIndex { $0 === anchor } ?? c.selectedTabIndex
            let insertAt = after ? anchorIdx + 1 : anchorIdx
            c.insertTab(newItem, at: insertAt, select: true)
            c.window?.makeKeyAndOrderFront(nil)
            newItem.content.focus(pane)
            focusChanged()
            scheduleLayoutSave()
            return
        }
        newTab(from: anchor.windowController ?? c, pane: pane, ordered: after ? .above : .below)
        self.tab(for: pane)?.content.focus(pane)
        focusChanged()
    }

    func moveCardToNewTab(_ pane: PaneKey, from c: TerminalWindowController,
                          next: (tab: TerminalWindowController, after: Bool)? = nil) {
        moveCardToNewTab(pane, from: c.selectedTab, next: next.map { ($0.tab.selectedTab, $0.after) })
    }
}
