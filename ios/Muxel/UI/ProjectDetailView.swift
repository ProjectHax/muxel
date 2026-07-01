import SwiftUI
import UIKit

/// The active project: a horizontal tab bar of its panes (with status dots) and the
/// selected pane's terminal. MVP renders the active leaf's tabs; full split-tree
/// rendering is a later phase.
struct ProjectDetailView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.verticalSizeClass) private var vSizeClass
    @Environment(\.theme) private var theme
    let project: RemoteProject
    @State private var selectedTab: String?
    @State private var showLaunch = false
    @State private var renameTarget: Instance?
    @State private var renameText = ""
    @State private var closeTarget: Instance?
    @State private var keyboardUp = false

    /// Landscape on iPhone (compact height) with the keyboard up leaves almost no room
    /// for the terminal — reclaim the nav bar + tab bar for the terminal in that case.
    private var hideChrome: Bool { keyboardUp && vSizeClass == .compact }

    private var instances: [Instance] { state.layout?.orderedTerminalInstances ?? [] }

    private var current: Instance? {
        instances.first { $0.id == selectedTab } ?? instances.first
    }

    var body: some View {
        VStack(spacing: 0) {
            if !hideChrome {
                tabBar
                Divider()
            }
            if let inst = current, let host = state.host(for: project) {
                TerminalPaneView(host: host, project: project, instance: inst)
                    .id(inst.id)
            } else {
                emptyState
            }
        }
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
        .sheet(isPresented: $showLaunch) { LaunchInstanceView(project: project) }
        .onChange(of: state.layout?.orderedTerminalInstances.map(\.id) ?? []) { ids in
            if selectedTab == nil || !ids.contains(selectedTab!) {
                selectedTab = ids.first
            }
        }
        .onChange(of: state.lastLaunched) { id in
            // Auto-open a freshly launched instance's terminal.
            if let id, instances.contains(where: { $0.id == id }) { selectedTab = id }
        }
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
        .confirmationDialog(
            closeTarget.map { "Close “\($0.displayName)”?" } ?? "Close pane?",
            isPresented: Binding(
                get: { closeTarget != nil },
                set: { if !$0 { closeTarget = nil } }
            ),
            titleVisibility: .visible,
            presenting: closeTarget
        ) { inst in
            Button("Close pane", role: .destructive) {
                Task { await state.close(inst, in: project) }
                closeTarget = nil
            }
            Button("Cancel", role: .cancel) { closeTarget = nil }
        } message: { _ in
            Text("This ends its tmux session on \(project.name). This can't be undone.")
        }
    }

    private var tabBar: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(instances) { inst in
                    tabChip(inst)
                }
            }
            .padding(.horizontal)
            .padding(.vertical, 8)
        }
    }

    @ViewBuilder
    private func tabChip(_ inst: Instance) -> some View {
        let isActive = inst.id == current?.id
        Button {
            selectedTab = inst.id
            state.attend(inst.id)
        } label: {
            HStack(spacing: 6) {
                StatusDot(status: state.status(inst.id), running: state.isRunning(inst.id))
                Text(inst.displayName)
                    .font(.mono(.footnote, weight: isActive ? .semibold : .regular))
                    .foregroundStyle(isActive ? theme.textColor : theme.mutedColor)
                    .lineLimit(1)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 7)
            .paneCard(theme, active: isActive, radius: 8)
        }
        .buttonStyle(.plain)
        .contextMenu { tabMenu(inst) }
    }

    @ViewBuilder
    private func tabMenu(_ inst: Instance) -> some View {
        Button {
            UISelectionFeedbackGenerator().selectionChanged()
            renameText = inst.displayName
            renameTarget = inst
        } label: {
            Label("Rename", systemImage: "pencil")
        }
        Button {
            UISelectionFeedbackGenerator().selectionChanged()
            Task { await state.duplicate(inst, in: project) }
        } label: {
            Label("Duplicate", systemImage: "plus.square.on.square")
        }
        Divider()
        Button(role: .destructive) {
            UISelectionFeedbackGenerator().selectionChanged()
            closeTarget = inst
        } label: {
            Label("Close", systemImage: "xmark.circle")
        }
    }

    /// The no-terminal states, keyed by the layout's load state so a failed
    /// connection is distinguishable from a genuinely empty project.
    @ViewBuilder private var emptyState: some View {
        switch state.layoutLoad {
        case .idle, .loading:
            CenteredState(spinner: true, title: "connecting…", showsGrid: true)
        case let .failed(message):
            CenteredState(icon: "wifi.exclamationmark",
                          iconColor: theme.blockedColor,
                          title: "can't reach \(state.host(for: project)?.name ?? "the host")",
                          message: message,
                          showsGrid: true) {
                Button {
                    Task { await state.refreshLayout() }
                } label: {
                    Label("Try again", systemImage: "arrow.clockwise")
                        .font(.mono(.footnote, weight: .semibold))
                }
                .buttonStyle(.borderedProminent)
            }
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
