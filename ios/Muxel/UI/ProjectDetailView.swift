import SwiftUI
import UIKit

/// The active project. On iPhone (compact width) a flat tab bar of its panes + the
/// selected pane's terminal; on iPad (regular width) the full `PaneNode` split tree
/// rendered with live terminals side by side, plus basic split editing.
struct ProjectDetailView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.verticalSizeClass) private var vSizeClass
    @Environment(\.horizontalSizeClass) private var hSizeClass
    @Environment(\.theme) private var theme
    let project: RemoteProject
    @State private var selectedTab: String?
    /// The focused leaf's anchor (regular width) — target for launch + keyboard.
    @State private var focusedInstance: String?
    @State private var showLaunch = false
    @State private var renameTarget: Instance?
    @State private var renameText = ""
    @State private var closeTarget: Instance?
    @State private var closeOthersTarget: Instance?
    @State private var keyboardUp = false
    /// Live state of an in-progress chip drag (iPad).
    @StateObject private var dragCoord = PaneDragCoordinator()

    /// Landscape on iPhone (compact height) with the keyboard up leaves almost no room
    /// for the terminal — reclaim the nav bar + tab bar for the terminal in that case.
    private var hideChrome: Bool { keyboardUp && vSizeClass == .compact }
    private var isRegular: Bool { hSizeClass == .regular }

    private var instances: [Instance] { state.layout?.orderedPaneInstances ?? [] }

    private var current: Instance? {
        instances.first { $0.id == selectedTab } ?? instances.first
    }

    var body: some View {
        decorated
            .alert(
                "Rename pane",
                isPresented: Binding(
                    get: { renameTarget != nil },
                    set: { if !$0 { renameTarget = nil } }
                ),
                presenting: renameTarget
            ) { inst in
                TextField("Name", text: $renameText)
                Button("Cancel", role: .cancel) { renameTarget = nil }
                Button("Rename") {
                    let name = renameText
                    Task { await state.rename(inst, to: name, in: project) }
                    renameTarget = nil
                }
            } message: { _ in
                Text("Leave blank to reset to the default name.")
            }
            // Alerts (not confirmationDialogs): a confirmationDialog triggered from a
            // tab's context menu presents as a stray popover anchored to the chip; an
            // alert is a proper centered modal, like the rename prompt above.
            .alert(
                closeTarget.map { "Close “\($0.displayName)”?" } ?? "Close pane?",
                isPresented: Binding(
                    get: { closeTarget != nil },
                    set: { if !$0 { closeTarget = nil } }
                ),
                presenting: closeTarget
            ) { inst in
                Button("Close pane", role: .destructive) {
                    Task { await state.close(inst, in: project) }
                    closeTarget = nil
                }
                Button("Cancel", role: .cancel) { closeTarget = nil }
            } message: { inst in
                Text(inst.kind == .terminal
                     ? "This ends its tmux session on \(project.name). This can't be undone."
                     : "This removes the pane from \(project.name).")
            }
            .alert(
                closeOthersTarget.map { "Close all panes except “\($0.displayName)”?" } ?? "Close others?",
                isPresented: Binding(
                    get: { closeOthersTarget != nil },
                    set: { if !$0 { closeOthersTarget = nil } }
                ),
                presenting: closeOthersTarget
            ) { inst in
                Button("Close others", role: .destructive) {
                    Task { await state.closeOthers(keeping: inst.id, in: project) }
                    closeOthersTarget = nil
                }
                Button("Cancel", role: .cancel) { closeOthersTarget = nil }
            } message: { _ in
                Text("Ends the other panes' tmux sessions on \(project.name). This can't be undone.")
            }
    }

    /// The content + chrome (nav, toolbar, keyboard tracking, sheet, onChange). Split
    /// from `body`'s dialog modifiers so the type-checker doesn't time out on one giant
    /// expression.
    private var decorated: some View {
        content
            .muxelBackground()
            .navigationTitle(project.name)
            .navigationBarTitleDisplayMode(.inline)
            .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { _ in
                withAnimation(.easeOut(duration: 0.2)) { keyboardUp = true }
            }
            .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
                withAnimation(.easeOut(duration: 0.2)) { keyboardUp = false }
            }
            .toolbar {
                ToolbarItem(placement: .primaryAction) {
                    Button { showLaunch = true } label: {
                        Label("New instance", systemImage: "plus.rectangle.on.rectangle")
                    }
                }
                ToolbarItem(placement: .navigationBarTrailing) {
                    Button { Task { await state.refreshLayout() } } label: {
                        Image(systemName: "arrow.clockwise")
                    }
                }
            }
            .sheet(isPresented: $showLaunch) {
                // Launch into the focused leaf on iPad; into the main pane on iPhone.
                LaunchInstanceView(project: project,
                                   targetLeafAnchor: isRegular ? focusedInstance : nil)
            }
            .onChange(of: state.layout?.orderedPaneInstances.map(\.id) ?? []) { ids in
                if selectedTab == nil || !ids.contains(selectedTab!) { selectedTab = ids.first }
                if focusedInstance == nil || !ids.contains(focusedInstance!) { focusedInstance = ids.first }
            }
            .onChange(of: state.lastLaunched) { id in
                // Auto-open a freshly launched instance's terminal + focus its leaf.
                if let id, instances.contains(where: { $0.id == id }) {
                    selectedTab = id
                    focusedInstance = id
                }
            }
    }

    @ViewBuilder private var content: some View {
        if isRegular {
            if let host = state.host(for: project), let root = state.layout?.layout, !instances.isEmpty {
                PaneTreeView(node: root, context: renderContext(host: host))
                    .padding(6)
                    .coordinateSpace(name: paneDragSpace)
                    .onPreferenceChange(LeafFramesKey.self) { dragCoord.frames = $0 }
                    .overlay { dragOverlay }
            } else {
                emptyState
            }
        } else {
            VStack(spacing: 0) {
                if !hideChrome {
                    tabBar
                    Divider()
                }
                if let inst = current, let host = state.host(for: project) {
                    PaneContentView(host: host, project: project, instance: inst)
                        .id(inst.id)
                } else {
                    emptyState
                }
            }
        }
    }

    private func renderContext(host: Host) -> PaneRenderContext {
        let all = state.layout?.instances ?? []
        let byId = Dictionary(all.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        // First occurrence of each id (left-to-right) is the only renderable one — a
        // second mount of the same instance would steal its single TerminalView.
        var firstOccurrence = Set<String>()
        var visited = Set<String>()
        for id in state.layout?.layout?.allTabs ?? [] where visited.insert(id).inserted {
            firstOccurrence.insert(id)
        }
        return PaneRenderContext(
            project: project,
            host: host,
            instancesById: byId,
            firstOccurrence: firstOccurrence,
            focusedInstance: $focusedInstance,
            onRename: { startRename($0) },
            onClose: { closeTarget = $0 },
            onCloseOthers: { closeOthersTarget = $0 },
            onDuplicate: { inst in Task { await state.duplicate(inst, in: project) } },
            onSplit: { inst, dir in state.openInSplit(inst, direction: dir, in: project) },
            hasMultiplePanes: instances.count > 1,
            dragCoord: dragCoord,
            onChipDrop: { draggedId, leaf, zone in resolveChipDrop(draggedId, leaf, zone) },
            onResizeSplit: { key, sizes in state.setSplitSizes(key: key, sizes: sizes, in: project) })
    }

    /// Resolve a finished chip drag over pane `leaf`. On its OWN pane: the tab strip is
    /// a no-op (already a tab there); a body edge pulls the agent out into a new split.
    /// On ANOTHER pane: the tab strip joins its tabs, a body edge splits out that side.
    private func resolveChipDrop(_ draggedId: String, _ leaf: LeafFrameInfo, _ zone: PaneDropZone) {
        let sameLeaf = leaf.tabs.contains(draggedId)
        switch zone {
        case .tabs:
            guard !sameLeaf else { return }  // dropped back on its own tabs
            state.moveTabIntoPane(dragged: draggedId, targetAnchor: leaf.anchor, in: project)
        case let .split(dir, before):
            if sameLeaf {
                // Pull the agent out beside its siblings (needs a sibling to split against).
                guard let sibling = leaf.tabs.first(where: { $0 != draggedId }) else { return }
                state.moveTabIntoSplit(dragged: draggedId, targetAnchor: sibling,
                                       direction: dir, before: before, in: project)
            } else {
                state.moveTabIntoSplit(dragged: draggedId, targetAnchor: leaf.anchor,
                                       direction: dir, before: before, in: project)
            }
        }
    }

    /// While a chip is dragged: a preview of the exact region the drop will occupy
    /// (whole pane = join as a tab; a half = split on that side), plus a floating chip
    /// following the finger. Mirrors the desktop drop preview.
    @ViewBuilder private var dragOverlay: some View {
        if let id = dragCoord.draggingId,
           let inst = state.layout?.instances.first(where: { $0.id == id }) {
            if let drop = dragCoord.currentDrop {
                let rect = drop.zone.previewRect(in: drop.leaf.rect)
                RoundedRectangle(cornerRadius: 6)
                    .fill(theme.accentColor.opacity(0.22))
                    .overlay(RoundedRectangle(cornerRadius: 6)
                        .strokeBorder(theme.accentColor, lineWidth: 2))
                    .frame(width: rect.width, height: rect.height)
                    .position(x: rect.midX, y: rect.midY)
                    .allowsHitTesting(false)
            }
            Text(inst.displayName)
                .font(.mono(.footnote, weight: .semibold))
                .padding(.horizontal, 12).padding(.vertical, 7)
                .paneCard(theme, active: true, radius: 8)
                .position(dragCoord.point)
                .allowsHitTesting(false)
        }
    }

    private var tabBar: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(instances) { inst in
                    TabChipView(
                        instance: inst,
                        isActive: inst.id == current?.id,
                        onTap: {
                            selectedTab = inst.id
                            state.attend(inst.id)
                        },
                        menu: compactMenu(for: inst))
                }
                addTabChip
            }
            .padding(.horizontal)
            .padding(.vertical, 8)
        }
    }

    /// Trailing `+` chip — the natural terminal-tabs affordance for launching a pane
    /// (mirrors the toolbar button).
    private var addTabChip: some View {
        Button { showLaunch = true } label: {
            Image(systemName: "plus")
                .font(.mono(.footnote, weight: .semibold))
                .foregroundStyle(theme.mutedColor)
                .padding(.horizontal, 12)
                .padding(.vertical, 7)
                .paneCard(theme, active: false, radius: 8)
        }
        .buttonStyle(.plain)
        .accessibilityLabel("New instance")
    }

    private func compactMenu(for inst: Instance) -> TabMenuActions {
        TabMenuActions(
            onRename: { startRename(inst) },
            onDuplicate: inst.kind == .terminal ? { Task { await state.duplicate(inst, in: project) } } : nil,
            onSplitRight: nil,   // splits are iPad-only
            onSplitDown: nil,
            onClose: { closeTarget = inst },
            onCloseOthers: instances.count > 1 ? { closeOthersTarget = inst } : nil)
    }

    private func startRename(_ inst: Instance) {
        UISelectionFeedbackGenerator().selectionChanged()
        renameText = inst.displayName
        renameTarget = inst
    }

    /// The no-terminal states, keyed by the layout's load state so a failed
    /// connection is distinguishable from a genuinely empty project.
    @ViewBuilder private var emptyState: some View {
        switch state.layoutLoad {
        case .idle, .loading:
            CenteredState(spinner: true, title: "connecting…", showsGrid: true)
        case let .failed(message):
            HostReachabilityState(hostName: state.host(for: project)?.name ?? "the host",
                                  mode: .unreachable(message: message),
                                  onRetry: { Task { await state.refreshLayout() } })
        case .loaded:
            CenteredState(title: "no panes yet", prompt: true, showsGrid: true) {
                Button { showLaunch = true } label: {
                    Label("Launch one", systemImage: "plus")
                        .font(.mono(.footnote, weight: .semibold))
                }
                .buttonStyle(.borderedProminent)
            }
        }
    }
}
