import AppKit
import ImageIO
import SwiftUI

/// A node of a plugin's view, as `ViewNode` in thurm-proto has it.
struct PluginNode: Equatable {
    var type = ""
    var children: [PluginNode] = []
    var text = ""
    var symbol = ""
    var image: Picture?
    var color = ""
    var size = 0.0
    var weight = ""
    var width = 0.0
    var height = 0.0
    var spacing = 0.0
    var padding = 0.0
    var align = ""
    var fill = ""
    var corner = 0.0
    var border = ""
    var tooltip = ""
    var action: Action?
    var popover: Popover?
    var menu: [MenuItem] = []

    struct MenuItem: Equatable {
        var title: String
        var action: Action?
        var checked: Bool
    }

    struct Action: Equatable {
        var name: String
        /// Its arguments as JSON.
        var args: String?
    }

    final class Popover: Equatable {
        let node: PluginNode
        init(_ node: PluginNode) { self.node = node }
        static func == (a: Popover, b: Popover) -> Bool { a.node == b.node }
    }

    /// A PNG, decoded once when the view comes.
    final class Picture: Equatable {
        let base64: String
        let image: NSImage?
        init(_ base64: String) {
            self.base64 = base64
            image = PluginNodeView.image(base64)
        }
        static func == (a: Picture, b: Picture) -> Bool { a.base64 == b.base64 }
    }

    init(_ d: [String: Any]) {
        func str(_ k: String) -> String { jsonString(d[k]) ?? "" }
        func num(_ k: String) -> Double { jsonDouble(d[k]) ?? 0 }
        type = str("type")
        children = (d["children"] as? [[String: Any]] ?? []).map(PluginNode.init)
        text = str("text")
        symbol = str("symbol")
        if let png = jsonString(d["image"]), !png.isEmpty { image = Picture(png) }
        color = str("color")
        size = num("size")
        weight = str("weight")
        width = num("width")
        height = num("height")
        spacing = num("spacing")
        padding = num("padding")
        align = str("align")
        fill = str("fill")
        corner = num("corner")
        border = str("border")
        tooltip = str("tooltip")
        action = Self.action(d["action"])
        menu = (d["menu"] as? [[String: Any]] ?? []).map {
            MenuItem(title: jsonString($0["title"]) ?? "", action: Self.action($0["action"]),
                     checked: $0["checked"] as? Bool ?? false)
        }
        if let p = d["popover"] as? [String: Any] { popover = Popover(PluginNode(p)) }
    }

    private static func action(_ value: Any?) -> Action? {
        guard let a = value as? [String: Any], let name = jsonString(a["name"]) else { return nil }
        return Action(name: name, args: a["args"].map { JSON.encode($0) })
    }
}

/// A view a plugin mounted, as `MountedView` in thurm-proto has it.
struct MountedPluginView: Equatable {
    var client: UInt64
    var id: String
    var slot: String
    var view: PluginNode

    var key: String { "\(client)/\(id)" }

    init?(_ d: [String: Any]) {
        guard let client = jsonUInt64(d["client"]), let id = jsonString(d["id"]),
              let slot = jsonString(d["slot"]), let view = d["view"] as? [String: Any] else { return nil }
        (self.client, self.id, self.slot, self.view) = (client, id, slot, PluginNode(view))
    }
}

/// The places in a tab plugins draw in, as `SLOTS` in thurm-proto has them.
enum PluginSlot: String, CaseIterable {
    case bottomTrailing = "window.bottom_trailing"
    case bottomLeading = "window.bottom_leading"
    case topTrailing = "window.top_trailing"
}

/// The views plugins mounted in the window, as this Mac's daemon sends them (`ui.views.changed`).
final class PluginViews: ObservableObject {
    static let shared = PluginViews()

    @Published private(set) var views: [MountedPluginView] = []

    func apply(_ list: [MountedPluginView]) {
        guard list != views else { return }
        views = list
        NotificationCenter.default.post(name: .pluginViewsChanged, object: nil)
    }

    static func decode(_ value: Any?) -> [MountedPluginView] {
        (value as? [[String: Any]] ?? []).compactMap(MountedPluginView.init)
    }

    func views(in slot: PluginSlot) -> [MountedPluginView] {
        views.filter { $0.slot == slot.rawValue }
    }
}

extension Notification.Name {
    static let pluginViewsChanged = Notification.Name("ThurmPluginViewsChanged")
}

/// The views of one slot, side by side.
struct PluginSlotView: View {
    @ObservedObject var store = PluginViews.shared
    let slot: PluginSlot
    let run: (String, String?) -> Void

    var body: some View {
        HStack(spacing: 8) {
            ForEach(store.views(in: slot), id: \.key) { PluginNodeView(node: $0.view, run: run) }
        }
        .fixedSize()
    }
}

/// One node and what is under it.
struct PluginNodeView: View {
    let node: PluginNode
    let run: (String, String?) -> Void
    @State private var hovering = false

    var body: some View {
        content
            .frame(width: node.width > 0 ? node.width : nil, height: node.height > 0 ? node.height : nil,
                   alignment: alignment)
            .padding(node.padding)
            .background(Self.color(node.fill).map { RoundedRectangle(cornerRadius: node.corner).fill($0) })
            .overlay(Self.color(node.border).map { RoundedRectangle(cornerRadius: node.corner).strokeBorder($0) })
            .help(node.tooltip)
            .contentShape(Rectangle())
            .onTapGesture { if let a = node.action { run(a.name, a.args) } }
            .onHover { if node.popover != nil { hovering = $0 } }
            .popover(isPresented: $hovering, arrowEdge: .top) {
                if let p = node.popover?.node { PluginNodeView(node: p, run: run).padding(12) }
            }
            .contextMenu {
                ForEach(node.menu.indices, id: \.self) { i in
                    let item = node.menu[i]
                    if item.title.isEmpty && item.action == nil {
                        Divider()
                    } else {
                        Toggle(item.title, isOn: Binding(get: { item.checked }, set: { _ in
                            if let a = item.action { run(a.name, a.args) }
                        }))
                        .disabled(item.action == nil)
                    }
                }
            }
    }

    @ViewBuilder private var content: some View {
        let spacing: CGFloat? = node.spacing > 0 ? node.spacing : nil
        switch node.type {
        case "row":
            HStack(spacing: spacing) { children }
        case "column":
            VStack(alignment: .leading, spacing: spacing) { children }
        case "stack":
            ZStack(alignment: alignment) { children }
        case "text":
            Text(node.text)
                .font(.system(size: node.size > 0 ? node.size : 12, weight: fontWeight))
                .foregroundStyle(Self.color(node.color) ?? .primary)
                .fixedSize(horizontal: node.width == 0, vertical: false)
        case "icon":
            Image(systemName: node.symbol)
                .font(.system(size: node.size > 0 ? node.size : 13, weight: fontWeight))
                .foregroundStyle(Self.color(node.color) ?? .primary)
        case "image":
            if let image = node.image?.image { Image(nsImage: image).resizable().scaledToFit() }
        case "spacer":
            Spacer(minLength: 0)
        default:
            EmptyView()
        }
    }

    private var children: some View {
        ForEach(node.children.indices, id: \.self) { PluginNodeView(node: node.children[$0], run: run) }
    }

    private var fontWeight: Font.Weight {
        switch node.weight {
        case "bold": .bold
        case "medium": .medium
        default: .regular
        }
    }

    private var alignment: Alignment {
        switch node.align {
        case "top": .top
        case "bottom": .bottom
        case "leading": .leading
        case "trailing": .trailing
        case "top_leading": .topLeading
        case "top_trailing": .topTrailing
        case "bottom_leading": .bottomLeading
        case "bottom_trailing": .bottomTrailing
        default: .center
        }
    }

    /// `primary`, `secondary`, `accent`, or `#rrggbb[aa]`.
    static func color(_ name: String) -> Color? {
        switch name {
        case "primary": return .primary
        case "secondary": return .secondary
        case "accent": return .accentColor
        default:
            guard name.hasPrefix("#"), name.count == 7 || name.count == 9,
                  let v = UInt64(name.dropFirst(), radix: 16) else { return nil }
            let rgba = name.count == 7 ? v << 8 | 0xff : v
            return Color(.sRGB, red: Double(rgba >> 24 & 0xff) / 255, green: Double(rgba >> 16 & 0xff) / 255,
                         blue: Double(rgba >> 8 & 0xff) / 255, opacity: Double(rgba & 0xff) / 255)
        }
    }

    /// A small PNG may say it is far larger: it is drawn from a thumbnail of at most 512 pixels on
    /// a side, and refused past 4096 by 4096.
    static func image(_ base64: String) -> NSImage? {
        guard let data = Data(base64Encoded: base64),
              let source = CGImageSourceCreateWithData(data as CFData, nil),
              let props = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let w = props[kCGImagePropertyPixelWidth] as? Int, let h = props[kCGImagePropertyPixelHeight] as? Int,
              w > 0, h > 0, w * h <= 4096 * 4096 else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceThumbnailMaxPixelSize: 512,
        ]
        guard let cg = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else { return nil }
        return NSImage(cgImage: cg, size: NSSize(width: cg.width, height: cg.height))
    }
}

/// One slot's views in a tab: sized to them, hidden when there are none.
final class PluginSlotHost: NSHostingView<PluginSlotView> {
    let slot: PluginSlot

    init(slot: PluginSlot, run: @escaping (String, String?) -> Void) {
        self.slot = slot
        super.init(rootView: PluginSlotView(slot: slot, run: run))
        isHidden = true
        wantsLayer = true
        layer?.masksToBounds = true
    }

    /// The menu of the view under the pointer, or nothing: never the terminal's under the slot.
    override func rightMouseDown(with event: NSEvent) {
        if let menu = menu(for: event) { NSMenu.popUpContextMenu(menu, with: event, for: self) }
    }

    @available(*, unavailable)
    required init(rootView: PluginSlotView) { fatalError("init(rootView:) is not supported") }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }
}
