import SwiftUI

/// One leaf of the split tree: its own compact tab strip + the selected tab's live
/// terminal, in a focus-bordered card. Tab selection is local view state (seeded from
/// the layout's `active` index); it is deliberately NOT written back, so switching
/// tabs on the phone neither churns the shared file nor fights desktop's focus.
struct LeafPaneView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    let tabs: [String]
    let active: Int
    let context: PaneRenderContext
    @State private var selected: String?

    private var resolved: [Instance] { tabs.compactMap { context.instancesById[$0] } }

    private var current: Instance? {
        if let sel = selected, let inst = resolved.first(where: { $0.id == sel }) { return inst }
        if active >= 0, active < tabs.count, let inst = context.instancesById[tabs[active]] { return inst }
        return resolved.first
    }

    private var isFocused: Bool {
        guard let focused = context.focusedInstance.wrappedValue else { return false }
        return tabs.contains(focused)
    }

    var body: some View {
        VStack(spacing: 0) {
            strip
            Divider()
            terminalArea
        }
        .background(theme.terminalBackground)
        // Report this leaf's frame (in the shared drag space) so the drag can hit-test
        // which pane the finger is over.
        .background(
            GeometryReader { geo in
                Color.clear.preference(
                    key: LeafFramesKey.self,
                    value: [LeafFrameInfo(anchor: tabs.first ?? "", tabs: tabs,
                                          rect: geo.frame(in: .named(paneDragSpace)))])
            }
        )
        .overlay(
            RoundedRectangle(cornerRadius: 6)
                .strokeBorder(isFocused ? theme.accentColor : .clear, lineWidth: 1.5)
        )
        .onChange(of: tabs) { newTabs in
            // Desktop moved the selected tab to another leaf → fall back to this leaf's
            // active tab (recomputed from the fresh layout).
            if let sel = selected, !newTabs.contains(sel) { selected = nil }
        }
    }

    private var strip: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                ForEach(resolved) { inst in
                    TabChipView(
                        instance: inst,
                        isActive: inst.id == current?.id,
                        onTap: {
                            selected = inst.id
                            context.focusedInstance.wrappedValue = inst.id
                            state.attend(inst.id)
                        },
                        menu: menu(for: inst),
                        dragCoord: context.dragCoord,
                        onChipDrop: { leaf, zone in context.onChipDrop(inst.id, leaf, zone) })
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
        }
    }

    @ViewBuilder private var terminalArea: some View {
        if let inst = current {
            if context.firstOccurrence.contains(inst.id) {
                PaneContentView(host: context.host, project: context.project, instance: inst,
                                onFocus: { context.focusedInstance.wrappedValue = inst.id })
                    .id(inst.id)
            } else {
                // Guard: a malformed tree listing the same instance twice must not mount
                // its single TerminalView in two places.
                CenteredState(title: "already open in another pane", prompt: true)
            }
        } else {
            CenteredState(title: "pane unavailable", prompt: true)
        }
    }

    private func menu(for inst: Instance) -> TabMenuActions {
        // "Open in split" needs a sibling to split beside — a sole-tab leaf is already
        // its own pane.
        let canSplit = tabs.count > 1
        return TabMenuActions(
            onRename: { context.onRename(inst) },
            onDuplicate: inst.kind == .terminal ? { context.onDuplicate(inst) } : nil,
            onSplitRight: canSplit ? { context.onSplit(inst, .horizontal) } : nil,
            onSplitDown: canSplit ? { context.onSplit(inst, .vertical) } : nil,
            onClose: { context.onClose(inst) },
            onCloseOthers: context.hasMultiplePanes ? { context.onCloseOthers(inst) } : nil)
    }
}
