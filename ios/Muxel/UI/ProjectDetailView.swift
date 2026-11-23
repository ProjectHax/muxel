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
    @AppStorage("muxel.lastPresetName") private var lastPresetName = ""
    @State private var showLaunch = false
    @State private var quickLaunching = false
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

    private var lastUsedPreset: Preset {
        if let saved = Preset.builtins.first(where: { $0.name == lastPresetName }) {
            return saved
        }
        if let inferred = state.layout?.orderedTerminalInstances.last
            .flatMap(AgentLaunch.builtinPreset(for:)) {
            return inferred
        }
        return Preset.builtins.first!
    }

    /// On iPhone (flat tab strip), anchor new launches to the last prior agent tab's
    /// leaf so the new tab always appends at the end of the prior agent tabs (even when
    /// desktop saved a split layout). On iPad, launch into the focused leaf.
    private var launchTargetAnchor: String? {
        if isRegular { return focusedInstance }
        return state.layout?.orderedTerminalInstances.last?.id
            ?? state.layout?.layout?.allTabs.last
    }

    private func quickLaunch(_ preset: Preset) {
        guard !quickLaunching else { return }
        quickLaunching = true
        lastPresetName = preset.name
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        let anchor = launchTargetAnchor
        Task {
            await state.launch(
                preset: preset,
                customCommand: nil,
                into: project,
                targetLeafAnchor: anchor)
            quickLaunching = false
        }
    }

    @ViewBuilder private var launchPickerMenu: some View {
        Button {
            showLaunch = true
        } label: {
            Label("New instance…", systemImage: "slider.horizontal.3")
        }
        Divider()
        ForEach(Preset.builtins) { preset in
            Button {
                quickLaunch(preset)
            } label: {
                if preset.name == lastUsedPreset.name {
                    Label(preset.name, systemImage: "checkmark")
                } else {
                    Text(preset.name)
                }
            }
        }
    }

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
    /// Other projects' running/active agents for 1-tap cross-project jumping from the
    /// navigation title menu (attention-first: needs input → working → done → idle).
    private var quickJumpAgents: [(project: RemoteProject, agent: AppState.AgentSummary)] {
        var list: [(RemoteProject, AppState.AgentSummary)] = []
        for host in state.doc.hosts {
            for proj in state.projects(for: host) where proj.id != project.id {
                guard let agents = state.activity(for: proj)?.agents else { continue }
                for agent in agents where agent.running {
                    list.append((proj, agent))
                }
            }
        }
        func rank(_ s: AgentStatus) -> Int {
            switch s {
            case .blocked: return 0
            case .working: return 1
            case .done: return 2
            case .idle: return 3
            }
        }
        return list.sorted { rank($0.1.status) < rank($1.1.status) }
    }

    /// True if any agent in another project currently needs attention (blocked or done).
    private var otherProjectNeedsAttention: Bool {
        quickJumpAgents.contains { $0.agent.needsAttention }
    }

    private func statusBadgeLabel(for agent: AppState.AgentSummary) -> String {
        guard agent.running else { return "stopped" }
        switch agent.status {
        case .blocked: return "needs input"
        case .working: return "working"
        case .done: return "done"
        case .idle: return "idle"
        }
    }

    private func statusSymbol(for agent: AppState.AgentSummary) -> String {
        guard agent.running else { return "circle" }
        switch agent.status {
        case .blocked: return "exclamationmark.circle.fill"
        case .working: return "bolt.circle.fill"
        case .done: return "checkmark.circle.fill"
        case .idle: return "circle.fill"
        }
    }

    /// 1-tap switcher menu anchored to the navigation title so the user can jump
    /// straight between projects or directly into any running agent across projects.
    @ViewBuilder private var projectSwitcherMenu: some View {
        let activeOthers = Array(quickJumpAgents.prefix(8))
        if !activeOthers.isEmpty {
            Section("Active agents") {
                ForEach(activeOthers, id: \.agent.id) { item in
                    Button {
                        UISelectionFeedbackGenerator().selectionChanged()
                        state.select(item.project, focusingInstance: item.agent.id)
                    } label: {
                        Label(
                            "\(item.project.name) · \(item.agent.name) (\(statusBadgeLabel(for: item.agent)))",
                            systemImage: statusSymbol(for: item.agent))
                    }
                }
            }
        }
        let multiHost = state.doc.hosts.count > 1
        ForEach(state.doc.hosts) { host in
            let hostProjects = state.projects(for: host)
            if !hostProjects.isEmpty {
                Section(multiHost ? host.name : "Projects") {
                    ForEach(hostProjects) { proj in
                        let isCurrent = proj.id == project.id
                        let agents = state.activity(for: proj)?.agents ?? []
                        let runningCount = agents.filter(\.running).count
                        let suffix = runningCount > 0 ? " (\(runningCount) active)" : ""
                        if !isCurrent, agents.count > 1 {
                            Menu {
                                Button {
                                    UISelectionFeedbackGenerator().selectionChanged()
                                    state.select(proj)
                                } label: {
                                    Label("Open \(proj.name)", systemImage: "folder")
                                }
                                Divider()
                                ForEach(agents) { agent in
                                    Button {
                                        UISelectionFeedbackGenerator().selectionChanged()
                                        state.select(proj, focusingInstance: agent.id)
                                    } label: {
                                        Label(
                                            "\(agent.name) — \(statusBadgeLabel(for: agent))",
                                            systemImage: statusSymbol(for: agent))
                                    }
                                }
                            } label: {
                                Label(proj.name + suffix,
                                      systemImage: state.isProjectWatched(proj.id) ? "folder.fill" : "folder")
                            }
                        } else {
                            Button {
                                guard !isCurrent else { return }
                                UISelectionFeedbackGenerator().selectionChanged()
                                state.select(proj)
                            } label: {
                                if isCurrent {
                                    Label(proj.name, systemImage: "checkmark")
                                } else {
                                    Label(proj.name + suffix,
                                          systemImage: state.isProjectWatched(proj.id) ? "folder.fill" : "folder")
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    private func syncSelectedTabForCurrentProject() {
        let ids = state.layout?.orderedPaneInstances.map(\.id) ?? []
        if let target = state.lastLaunched, ids.contains(target) {
            selectedTab = target
            focusedInstance = target
            state.lastLaunched = nil
        } else if selectedTab == nil || !ids.contains(selectedTab!) {
            selectedTab = ids.first
            focusedInstance = ids.first
        }
    }

    private var decorated: some View {
        content
            .muxelBackground()
            .navigationTitle(project.name)
            .navigationBarTitleDisplayMode(.inline)
            .toolbarBackground(theme.background, for: .navigationBar)
            .toolbarBackground(.visible, for: .navigationBar)
            .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { _ in
                withAnimation(.easeOut(duration: 0.2)) { keyboardUp = true }
            }
            .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
                withAnimation(.easeOut(duration: 0.2)) { keyboardUp = false }
            }
            .toolbar {
                ToolbarItem(placement: .principal) {
                    Menu {
                        projectSwitcherMenu
                    } label: {
                        HStack(spacing: 5) {
                            if otherProjectNeedsAttention {
                                Circle()
                                    .fill(theme.accentColor)
                                    .frame(width: 7, height: 7)
                            }
                            Text(project.name)
                                .font(.mono(.subheadline, weight: .semibold))
                                .foregroundStyle(theme.textColor)
                                .lineLimit(1)
                            Image(systemName: "chevron.down")
                                .font(.system(size: 10, weight: .semibold))
                                .foregroundStyle(theme.mutedColor)
                        }
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                    }
                    .accessibilityLabel("Switch project or agent, current \(project.name)")
                }
                ToolbarItem(placement: .primaryAction) {
                    Menu {
                        launchPickerMenu
                    } label: {
                        if quickLaunching {
                            ProgressView().controlSize(.small)
                        } else {
                            Label("New \(lastUsedPreset.name)", systemImage: "plus")
                        }
                    } primaryAction: {
                        quickLaunch(lastUsedPreset)
                    }
                    .disabled(quickLaunching)
                    .accessibilityLabel("New \(lastUsedPreset.name)")
                    .accessibilityHint("Tap to launch \(lastUsedPreset.name), or hold to pick another agent")
                }
                ToolbarItem(placement: .navigationBarTrailing) {
                    Button { Task { await state.refreshLayout() } } label: {
                        if state.isRefreshingLayout {
                            ProgressView().controlSize(.small)
                        } else {
                            Image(systemName: "arrow.clockwise")
                        }
                    }
                    .disabled(state.isRefreshingLayout)
                    .accessibilityLabel("Refresh layout")
                }
            }
            .sheet(isPresented: $showLaunch) {
                // Launch into the focused leaf on iPad; at the end of prior agent tabs on iPhone.
                LaunchInstanceView(project: project,
                                   targetLeafAnchor: launchTargetAnchor)
            }
            .onAppear {
                syncSelectedTabForCurrentProject()
                state.activeViewedInstanceId = current?.id
            }
            .onDisappear {
                state.activeViewedInstanceId = nil
            }
            .onChange(of: current?.id) { _, id in
                state.activeViewedInstanceId = id
            }
            .onChange(of: project.id) { _, _ in
                selectedTab = nil
                focusedInstance = nil
                syncSelectedTabForCurrentProject()
            }
            .onChange(of: state.layout?.orderedPaneInstances.map(\.id) ?? []) { _, ids in
                if let target = state.lastLaunched, ids.contains(target) {
                    selectedTab = target
                    focusedInstance = target
                    state.lastLaunched = nil
                } else {
                    if selectedTab == nil || !ids.contains(selectedTab!) { selectedTab = ids.first }
                    if focusedInstance == nil || !ids.contains(focusedInstance!) { focusedInstance = ids.first }
                }
            }
            .onChange(of: state.lastLaunched) { _, id in
                // Auto-open a freshly launched or sidebar-tapped instance's terminal + focus its leaf.
                if let id, instances.contains(where: { $0.id == id }) {
                    selectedTab = id
                    focusedInstance = id
                    state.lastLaunched = nil
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
        ScrollViewReader { proxy in
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
                        .id(inst.id)
                    }
                    addTabChip
                }
                .padding(.horizontal)
                .padding(.vertical, 8)
            }
            .onChange(of: current?.id) { _, id in
                guard let id else { return }
                withAnimation(.easeOut(duration: 0.2)) {
                    proxy.scrollTo(id, anchor: .center)
                }
            }
        }
    }

    /// Trailing `+` chip — tap spawns the last-used agent directly; long-press
    /// picks another preset or opens the full New Instance sheet.
    private var addTabChip: some View {
        Menu {
            launchPickerMenu
        } label: {
            Group {
                if quickLaunching {
                    ProgressView().controlSize(.small)
                } else {
                    Image(systemName: "plus")
                        .font(.mono(.footnote, weight: .semibold))
                        .foregroundStyle(theme.mutedColor)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 7)
            .paneCard(theme, active: false, radius: 8)
        } primaryAction: {
            quickLaunch(lastUsedPreset)
        }
        .disabled(quickLaunching)
        .accessibilityLabel("New \(lastUsedPreset.name)")
        .accessibilityHint("Tap to launch \(lastUsedPreset.name), or hold to pick another agent")
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
                VStack(spacing: 8) {
                    Button { quickLaunch(lastUsedPreset) } label: {
                        Label("Launch \(lastUsedPreset.name)", systemImage: "plus")
                            .font(.mono(.footnote, weight: .semibold))
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(quickLaunching)
                    Button("More options…") { showLaunch = true }
                        .font(.mono(.caption))
                        .foregroundStyle(theme.mutedColor)
                }
            }
        }
    }
}
