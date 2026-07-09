import SwiftUI

/// Everything a rendered leaf needs, threaded down the recursive tree. Actions are
/// closures back to `ProjectDetailView`, which hoists the dialogs so they stay
/// single-instanced regardless of how many leaves are on screen.
struct PaneRenderContext {
    let project: RemoteProject
    let host: Host
    let instancesById: [String: Instance]
    /// Instance ids that are the FIRST occurrence in the tree — the only ones a leaf
    /// may mount. A malformed tree with the same instance twice would otherwise steal
    /// its single `TerminalView` into the second mount.
    let firstOccurrence: Set<String>
    let focusedInstance: Binding<String?>
    let onRename: (Instance) -> Void
    let onClose: (Instance) -> Void
    let onCloseOthers: (Instance) -> Void
    let onDuplicate: (Instance) -> Void
    let onSplit: (Instance, SplitDirection) -> Void
    /// Whether "Close others" applies (more than one pane in the project).
    let hasMultiplePanes: Bool
    /// The live chip-drag coordinator (finger position, target pane). Chips update it
    /// during a drag; leaves read `targetAnchor` for the highlight.
    let dragCoord: PaneDragCoordinator
    /// Called when a chip drag ends over a pane — resolve into a move/split.
    let onChipDrop: (_ draggedId: String, _ leaf: LeafFrameInfo, _ zone: PaneDropZone) -> Void
    /// Called when a split divider is dragged — persist the split's new sizes (keyed by
    /// the split's `stableKey`).
    let onResizeSplit: (_ splitKey: String, _ sizes: [Double]) -> Void
}

/// Recursive renderer over the shared `PaneNode` split tree (iPad regular width). A
/// split becomes a `ResizableSplit` (children sized by `sizes`, with draggable
/// dividers); a leaf becomes a `LeafPaneView`. The geometry is the same tree desktop
/// persists, so both peers show the same layout.
struct PaneTreeView: View {
    let node: PaneNode
    let context: PaneRenderContext

    var body: some View {
        switch node {
        case let .leaf(tabs, active):
            LeafPaneView(tabs: tabs, active: active, context: context)
        case let .split(direction, sizes, children):
            ResizableSplit(splitKey: node.stableKey, direction: direction,
                           sizes: sizes, children: children, context: context)
        }
    }
}

/// Lays a split's children along its axis, each getting a share of the space
/// proportional to `sizes`, with a draggable divider between them. Dragging a divider
/// transfers space between the two adjacent panes (never below `minPane`), live, and
/// persists the new sizes on release. Weights are treated as relative (never assumed
/// to sum to a constant).
struct ResizableSplit: View {
    @Environment(\.theme) private var theme
    let splitKey: String
    let direction: SplitDirection
    let sizes: [Double]
    let children: [PaneNode]
    let context: PaneRenderContext
    /// The divider being dragged + its clamped offset. The pane frames stay at the
    /// persisted `sizes` during the drag — only a ghost indicator moves — so the
    /// terminals never re-layout/reflow mid-drag; they resize once on release.
    @State private var dragIndex: Int?
    @State private var dragOffset: CGFloat = 0

    private let dividerThickness: CGFloat = 8
    private let minPane: CGFloat = 140

    private var horizontal: Bool { direction == .horizontal }

    var body: some View {
        GeometryReader { geo in
            let n = children.count
            let total = horizontal ? geo.size.width : geo.size.height
            let available = max(0, total - dividerThickness * CGFloat(n - 1))
            let lengths: [CGFloat] = normalized(sizes, count: n).map { CGFloat($0) * available }
            ZStack(alignment: .topLeading) {
                HStackOrVStack(horizontal: horizontal) {
                    ForEach(Array(children.enumerated()), id: \.element.stableKey) { i, child in
                        PaneTreeView(node: child, context: context)
                            .frame(width: horizontal ? lengths[i] : nil,
                                   height: horizontal ? nil : lengths[i])
                        if i < n - 1 {
                            divider(index: i, lengths: lengths, available: available)
                        }
                    }
                }
                // Ghost divider showing where the release will land.
                if let i = dragIndex {
                    ghost(index: i, lengths: lengths, size: geo.size)
                }
            }
        }
    }

    private func divider(index i: Int, lengths: [CGFloat], available: CGFloat) -> some View {
        Rectangle()
            .fill(theme.borderColor)
            .frame(width: horizontal ? dividerThickness : nil,
                   height: horizontal ? nil : dividerThickness)
            .overlay(  // a subtle grip
                Capsule().fill(theme.mutedColor.opacity(0.6))
                    .frame(width: horizontal ? 2 : 24, height: horizontal ? 24 : 2))
            .contentShape(Rectangle())
            .gesture(
                DragGesture(minimumDistance: 1)
                    .onChanged { value in
                        dragIndex = i
                        let raw = horizontal ? value.translation.width : value.translation.height
                        dragOffset = clampedOffset(raw, index: i, lengths: lengths)
                    }
                    .onEnded { _ in
                        let offset = dragOffset
                        dragIndex = nil
                        dragOffset = 0
                        context.onResizeSplit(splitKey, resized(divider: i, offset: offset,
                                                                lengths: lengths, available: available))
                    }
            )
    }

    /// The moving accent line spanning the cross axis, at the divider's resting center
    /// plus the current drag offset.
    private func ghost(index i: Int, lengths: [CGFloat], size: CGSize) -> some View {
        let center = lengths.prefix(i + 1).reduce(0, +) + (CGFloat(i) + 0.5) * dividerThickness + dragOffset
        return Rectangle()
            .fill(theme.accentColor)
            .frame(width: horizontal ? 2 : size.width, height: horizontal ? size.height : 2)
            .position(x: horizontal ? center : size.width / 2,
                      y: horizontal ? size.height / 2 : center)
            .allowsHitTesting(false)
    }

    /// Clamp the raw drag so neither adjacent pane would shrink below `minPane`.
    private func clampedOffset(_ raw: CGFloat, index i: Int, lengths: [CGFloat]) -> CGFloat {
        let li = lengths[i], lj = lengths[i + 1]
        guard li + lj >= minPane * 2 else { return 0 }
        return min(max(raw, minPane - li), lj - minPane)
    }

    /// New normalized weights after moving divider `i` by `offset` points.
    private func resized(divider i: Int, offset: CGFloat, lengths: [CGFloat], available: CGFloat) -> [Double] {
        guard available > 0 else { return sizes }
        var w = normalized(sizes, count: children.count)
        let li = lengths[i], lj = lengths[i + 1]
        w[i] = (li + offset) / available
        w[i + 1] = (lj - offset) / available
        return w
    }

    private func normalized(_ raw: [Double], count n: Int) -> [Double] {
        let vals = (0..<n).map { $0 < raw.count ? max(0, raw[$0]) : 0 }
        let sum = vals.reduce(0, +)
        return sum > 0 ? vals.map { $0 / sum } : Array(repeating: 1.0 / Double(n), count: n)
    }
}

/// An HStack or VStack chosen at runtime, spacing 0 (dividers are explicit children).
struct HStackOrVStack<Content: View>: View {
    let horizontal: Bool
    @ViewBuilder let content: Content
    var body: some View {
        if horizontal { HStack(spacing: 0) { content } }
        else { VStack(spacing: 0) { content } }
    }
}
