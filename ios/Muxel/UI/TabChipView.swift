import SwiftUI
import UIKit

/// Actions a tab chip's context menu can offer. `nil` closures hide their item —
/// e.g. non-terminal panes have no Duplicate, and the compact (iPhone) layout has no
/// split actions.
struct TabMenuActions {
    var onRename: () -> Void
    var onDuplicate: (() -> Void)?
    var onSplitRight: (() -> Void)?
    var onSplitDown: (() -> Void)?
    var onClose: () -> Void
    var onCloseOthers: (() -> Void)?
}

/// A single tab chip (status dot / kind glyph + name), shared by the compact tab bar
/// and each split leaf's strip so both look and behave identically.
struct TabChipView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    let instance: Instance
    let isActive: Bool
    let onTap: () -> Void
    let menu: TabMenuActions
    /// The shared drag coordinator + the drop resolver. nil in the compact (iPhone)
    /// layout, where there are no panes to drag between (the chip is tap-only there).
    var dragCoord: PaneDragCoordinator? = nil
    var onChipDrop: ((_ leaf: LeafFrameInfo, _ zone: PaneDropZone) -> Void)? = nil

    /// Solid accent for the active tab, the theme surface otherwise. The label color is
    /// derived from THIS background's luminance, so it's readable on any palette (the
    /// theme's own `fg`/`muted` can be too close to `surface` on some ported themes).
    private var chipBackgroundHex: String { isActive ? theme.accent : theme.surface }
    private var chipLabelColor: Color { theme.readableText(on: chipBackgroundHex) }

    var body: some View {
        HStack(spacing: 6) {
            chipIcon
            Text(instance.displayName)
                .font(.mono(.footnote, weight: isActive ? .semibold : .regular))
                .foregroundStyle(chipLabelColor)
                .lineLimit(1)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(Color(hex: chipBackgroundHex))
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(isActive ? theme.accentColor : theme.borderColor,
                              lineWidth: isActive ? 1.3 : 1))
        // A plain tappable view, NOT a Button (a Button steals the long-press the drag
        // needs). Tap = select; long-press-hold = context menu; press-then-move = drag.
        .contentShape(Rectangle())
        .onTapGesture { onTap() }
        .modifier(ChipDragModifier(instanceId: instance.id, coord: dragCoord, onDrop: onChipDrop))
        .contextMenu { TabContextMenu(actions: menu) }
    }

    @ViewBuilder private var chipIcon: some View {
        switch instance.kind {
        case .terminal:
            StatusDot(status: state.status(instance.id), running: state.isRunning(instance.id))
        case .editor:
            Image(systemName: "doc.text").font(.caption2).foregroundStyle(chipLabelColor)
        case .diff:
            Image(systemName: "plus.forwardslash.minus").font(.caption2).foregroundStyle(chipLabelColor)
        case .browser, .other:
            Image(systemName: "globe").font(.caption2).foregroundStyle(chipLabelColor)
        }
    }
}

/// Applies the custom chip-drag gesture only when a coordinator is present (iPad split
/// view). A `DragGesture` with a minimum distance — so a quick tap still selects and a
/// long-press-hold still opens the context menu — via `highPriorityGesture` so it wins
/// over the enclosing horizontal ScrollView's scroll (which is why the built-in
/// `.onDrag`/`.draggable` couldn't lift a single chip).
private struct ChipDragModifier: ViewModifier {
    let instanceId: String
    let coord: PaneDragCoordinator?
    let onDrop: ((LeafFrameInfo, PaneDropZone) -> Void)?

    func body(content: Content) -> some View {
        if let coord, let onDrop {
            content.highPriorityGesture(
                DragGesture(minimumDistance: 10, coordinateSpace: .named(paneDragSpace))
                    .onChanged { value in
                        if coord.draggingId == nil { coord.begin(instanceId, at: value.location) }
                        else { coord.update(to: value.location) }
                    }
                    .onEnded { value in
                        coord.update(to: value.location)
                        if let drop = coord.end() { onDrop(drop.leaf, drop.zone) }
                    }
            )
        } else {
            content
        }
    }
}

/// The chip's long-press menu, built from whichever `TabMenuActions` are non-nil.
struct TabContextMenu: View {
    let actions: TabMenuActions

    var body: some View {
        Button { run(actions.onRename) } label: { Label("Rename", systemImage: "pencil") }
        if let dup = actions.onDuplicate {
            Button { run(dup) } label: { Label("Duplicate", systemImage: "plus.square.on.square") }
        }
        if actions.onSplitRight != nil || actions.onSplitDown != nil {
            Divider()
            if let right = actions.onSplitRight {
                Button { run(right) } label: { Label("Open in split right", systemImage: "rectangle.righthalf.inset.filled") }
            }
            if let down = actions.onSplitDown {
                Button { run(down) } label: { Label("Open in split down", systemImage: "rectangle.bottomhalf.inset.filled") }
            }
        }
        Divider()
        Button(role: .destructive) { run(actions.onClose) } label: { Label("Close", systemImage: "xmark.circle") }
        if let closeOthers = actions.onCloseOthers {
            Button(role: .destructive) { run(closeOthers) } label: { Label("Close others", systemImage: "xmark.square") }
        }
    }

    private func run(_ action: () -> Void) {
        UISelectionFeedbackGenerator().selectionChanged()
        action()
    }
}
