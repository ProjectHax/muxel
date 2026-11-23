import SwiftUI
import UIKit

/// Persisted home-screen display preferences shared by `SidebarView` and `SettingsView`.
enum SidebarPreferences {
    static let showAgentsKey = "muxel.home.showAgents"
}

/// The collapsible sidebar: hosts as sections, projects as rows, with add/delete.
/// Each project row shows running / needs-input badges: live for the selected
/// project, and from the foreground cross-project sweep (over already-connected
/// hosts) for the rest.
struct SidebarView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    @Environment(\.openURL) private var openURL
    @Binding var showAddHost: Bool
    @Binding var addProjectForHost: Host?
    @Binding var discoverForHost: Host?
    @Binding var editHost: Host?
    @AppStorage(SidebarPreferences.showAgentsKey) private var showAgentsOnHome = true
    @State private var showSettings = false
    @State private var deleteHostTarget: Host?
    @State private var deleteProjectTarget: RemoteProject?

    /// Drives `NavigationSplitView` navigation: binding the `List` selection to the
    /// selected project's id is what pushes the detail column on iPhone (a plain
    /// `.onTapGesture` updates state but doesn't navigate in compact width).
    private var selection: Binding<UUID?> {
        Binding(
            get: { state.selectedProject?.id },
            set: { id in
                if let id, let project = state.doc.projects.first(where: { $0.id == id }) {
                    state.select(project)
                } else {
                    // Detail popped (iPhone back) → framework sets nil. Must clear the
                    // selection, otherwise it stays pinned to this id and re-tapping the
                    // row is a no-op, so you can't navigate back in.
                    state.deselect()
                }
            }
        )
    }

    var body: some View {
        List(selection: selection) {
            if state.isScanningActiveProjects {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text(state.scanStatusText ?? "Scanning active agents…")
                        .font(.mono(.caption))
                        .foregroundStyle(theme.mutedColor)
                        .lineLimit(1)
                }
                .listRowBackground(Color.clear)
            }
            if state.doc.hosts.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    PromptLabel(text: "no hosts yet", style: .footnote)
                    Button { showAddHost = true } label: {
                        HStack(spacing: 6) {
                            Image(systemName: "plus")
                            Text("Add a host")
                        }
                        .font(.mono(.footnote, weight: .semibold))
                        .foregroundStyle(theme.readableText(on: theme.accent))
                        .padding(.horizontal, 14)
                        .padding(.vertical, 8)
                        .background(Capsule().fill(theme.accentColor))
                    }
                    .buttonStyle(.plain)
                }
                .padding(.vertical, 4)
                .listRowBackground(Color.clear)
            }
            ForEach(state.doc.hosts) { host in
                let collapsed = state.isHostCollapsed(host.id)
                Section {
                    if !collapsed {
                        if state.projects(for: host).isEmpty {
                            emptyProjectsHint(host)
                        } else {
                            ForEach(state.projects(for: host)) { project in
                                let watched = state.isProjectWatched(project.id)
                                let autoWatched = state.isProjectAutoWatched(project.id)
                                ProjectRow(project: project,
                                           selected: state.selectedProject?.id == project.id,
                                           watched: watched,
                                           autoWatched: autoWatched,
                                           activity: state.activity(for: project),
                                           showAgents: showAgentsOnHome,
                                           onSelectAgent: { agentId in
                                               state.select(project, focusingInstance: agentId)
                                           })
                                    .tag(project.id)
                                    .listRowBackground(Color.clear)
                                    .swipeActions(edge: .leading, allowsFullSwipe: true) {
                                        Button {
                                            UISelectionFeedbackGenerator().selectionChanged()
                                            state.toggleProjectWatched(project.id)
                                        } label: {
                                            Label(watched ? "Unwatch" : "Watch",
                                                  systemImage: watched ? "eye.slash" : "eye")
                                        }
                                        .tint(watched ? theme.mutedColor : theme.accentColor)
                                    }
                                    // Omit `role: .destructive` here — UIKit collapses a
                                    // destructive swipe row immediately, making it vanish
                                    // and snap back while the confirmation dialog is open.
                                    .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                                        Button {
                                            deleteProjectTarget = project
                                        } label: {
                                            Label("Delete", systemImage: "trash")
                                        }
                                        .tint(.red)
                                    }
                                    .contextMenu {
                                        Button {
                                            state.toggleProjectWatched(project.id)
                                        } label: {
                                            Label(watched ? "Stop watching" : "Watch project",
                                                  systemImage: watched ? "eye.slash" : "eye")
                                        }
                                        Button {
                                            state.toggleProjectAutoWatched(project.id)
                                        } label: {
                                            Label(autoWatched ? "Remove from auto-watch on launch" : "Auto-watch on launch",
                                                  systemImage: autoWatched ? "pin.slash" : "pin")
                                        }
                                        Divider()
                                        Button(role: .destructive) {
                                            deleteProjectTarget = project
                                        } label: {
                                            Label("Remove project", systemImage: "trash")
                                        }
                                    }
                                    // Attach the confirmationDialog directly to this project
                                    // row so the iOS 26/27 / iPadOS popover anchors to the
                                    // exact list item being removed.
                                    .confirmationDialog(
                                        ConfirmationCopy.deleteProjects([project]).title,
                                        isPresented: Binding(
                                            get: { deleteProjectTarget?.id == project.id },
                                            set: { if !$0, deleteProjectTarget?.id == project.id { deleteProjectTarget = nil } }
                                        ),
                                        titleVisibility: .visible
                                    ) {
                                        Button("Remove project", role: .destructive) {
                                            state.deleteProject(project)
                                            deleteProjectTarget = nil
                                        }
                                        Button("Cancel", role: .cancel) {
                                            deleteProjectTarget = nil
                                        }
                                    } message: {
                                        Text(ConfirmationCopy.deleteProjects([project]).message)
                                    }
                            }
                            Button {
                                addProjectForHost = host
                            } label: {
                                Label("Add project", systemImage: "plus.circle")
                                    .font(.mono(.footnote))
                                    .foregroundStyle(theme.mutedColor)
                            }
                            .listRowBackground(Color.clear)
                        }
                    }
                } header: {
                    hostHeader(host, collapsed: collapsed)
                }
            }
            if state.notificationsDenied {
                notificationsDeniedRow
            }
        }
        .listStyle(.sidebar)
        .scrollContentBackground(.hidden)
        .muxelBackground()
        // Keep an opaque themed bar behind "muxel" and the status bar so long
        // project lists never bleed underneath the title when scrolling (iOS 26/27).
        .toolbarBackground(theme.background, for: .navigationBar)
        .toolbarBackground(.visible, for: .navigationBar)
        .onAppear { state.ensureSidebarPolling() }
        .refreshable { await state.refreshAll() }
        .toolbar {
            ToolbarItem(placement: .principal) {
                Text("muxel")
                    .font(.mono(.headline, weight: .bold))
                    .foregroundStyle(theme.textColor)
                    // Never wrap to "mux/el" when the sidebar column is narrow.
                    .lineLimit(1)
                    .fixedSize(horizontal: true, vertical: false)
            }
            ToolbarItem(placement: .topBarLeading) {
                Button { showSettings = true } label: {
                    Image(systemName: "gearshape")
                }
                .accessibilityLabel("Settings")
            }
            ToolbarItem(placement: .primaryAction) {
                Button { showAddHost = true } label: {
                    Label("Add host", systemImage: "plus")
                }
            }
        }
        .sheet(isPresented: $showSettings) { SettingsView() }
        .onChange(of: state.selectedProject?.id) { _, id in
            if id != nil, showSettings { showSettings = false }
        }
        // An alert, not a confirmationDialog: this is triggered from the host `…` menu,
        // and a menu-triggered confirmationDialog presents as a stray anchored popover.
        .alert(
            deleteHostTarget.map {
                ConfirmationCopy.deleteHost($0, projectCount: state.projects(for: $0).count).title
            } ?? "Delete host?",
            isPresented: Binding(
                get: { deleteHostTarget != nil },
                set: { if !$0 { deleteHostTarget = nil } }
            ),
            presenting: deleteHostTarget
        ) { host in
            Button("Delete host", role: .destructive) {
                state.deleteHost(host)
                deleteHostTarget = nil
            }
            Button("Cancel", role: .cancel) { deleteHostTarget = nil }
        } message: { host in
            Text(ConfirmationCopy.deleteHost(host, projectCount: state.projects(for: host).count).message)
        }
    }

    /// Quiet pointer shown only when the user has *denied* notifications — the
    /// blocked/done alerts the background poller posts silently can't arrive.
    private var notificationsDeniedRow: some View {
        Section {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: "bell.slash")
                    .foregroundStyle(theme.mutedColor)
                VStack(alignment: .leading, spacing: 3) {
                    Text("notifications off — blocked/done alerts won't arrive")
                        .font(.mono(.caption2))
                        .foregroundStyle(theme.mutedColor)
                    Button("Open Settings") {
                        if let url = URL(string: UIApplication.openNotificationSettingsURLString) {
                            openURL(url)
                        }
                    }
                    .font(.mono(.caption2, weight: .semibold))
                    .tint(theme.accentColor)
                }
            }
            .listRowBackground(Color.clear)
        }
    }

    /// Shown under a host that has no projects yet — a prominent scan CTA (the fastest
    /// onboarding path) plus the add-by-path fallback.
    private func emptyProjectsHint(_ host: Host) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            PromptLabel(text: "no projects yet", style: .footnote)
            Button { discoverForHost = host } label: {
                HStack(spacing: 6) {
                    Image(systemName: "sparkle.magnifyingglass")
                    Text("Scan for projects")
                }
                .font(.mono(.footnote, weight: .semibold))
                .foregroundStyle(theme.readableText(on: theme.accent))
                .padding(.horizontal, 14)
                .padding(.vertical, 8)
                .background(Capsule().fill(theme.accentColor))
            }
            .buttonStyle(.plain)
            Button { addProjectForHost = host } label: {
                Label("Add by path", systemImage: "plus.circle")
                    .font(.mono(.footnote))
                    .foregroundStyle(theme.mutedColor)
            }
        }
        .padding(.vertical, 4)
        .listRowBackground(Color.clear)
    }

    private func hostHeader(_ host: Host, collapsed: Bool) -> some View {
        let projectCount = state.projects(for: host).count
        let rolledUp = collapsed ? state.hostActivity(for: host) : nil
        return HStack(spacing: 6) {
            Button {
                UISelectionFeedbackGenerator().selectionChanged()
                withAnimation(.snappy(duration: 0.2)) {
                    state.toggleHostCollapsed(host.id)
                }
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(theme.mutedColor)
                        .rotationEffect(.degrees(collapsed ? 0 : 90))
                        .frame(width: 12)
                    Text("❯")
                        .font(.mono(.caption, weight: .bold))
                        .foregroundStyle(theme.accentColor)
                    Text(host.name)
                        .font(.mono(.subheadline, weight: .semibold))
                        .foregroundStyle(theme.textColor)
                        .lineLimit(1)
                    if collapsed, projectCount > 0 {
                        Text("\(projectCount)")
                            .font(.mono(.caption2))
                            .foregroundStyle(theme.mutedColor)
                    }
                    Spacer(minLength: 4)
                    if let statuses = rolledUp?.agentStatuses, !statuses.isEmpty {
                        hostStatusDots(statuses)
                    }
                }
                .contentShape(Rectangle())
                .padding(.vertical, 4)
            }
            .buttonStyle(.plain)
            .accessibilityLabel("\(host.name), \(collapsed ? "collapsed" : "expanded")")
            .accessibilityHint(collapsed ? "Expands host projects" : "Collapses host projects")
            Menu {
                Button {
                    withAnimation(.snappy(duration: 0.2)) {
                        state.toggleHostCollapsed(host.id)
                    }
                } label: {
                    Label(collapsed ? "Expand host" : "Collapse host",
                          systemImage: collapsed ? "chevron.down.circle" : "chevron.right.circle")
                }
                Divider()
                Button {
                    Task { await state.testConnection(host) }
                } label: {
                    Label("Test connection", systemImage: "bolt.horizontal.circle")
                }
                Button {
                    discoverForHost = host
                } label: {
                    Label("Scan for projects", systemImage: "sparkle.magnifyingglass")
                }
                Button {
                    Task { _ = await state.scanAndWatchWorkingProjects(announce: true) }
                } label: {
                    Label("Watch active projects", systemImage: "eye")
                }
                Button {
                    addProjectForHost = host
                } label: {
                    Label("Add project by path", systemImage: "plus")
                }
                Button {
                    editHost = host
                } label: {
                    Label("Edit host", systemImage: "pencil")
                }
                Divider()
                Button(role: .destructive) {
                    deleteHostTarget = host
                } label: {
                    Label("Delete host", systemImage: "trash")
                }
            } label: {
                Image(systemName: "ellipsis.circle")
                    .font(.title2)
                    .imageScale(.large)
                    .foregroundStyle(theme.mutedColor)
                    .contentShape(Rectangle())
                    .padding(.vertical, 6)
                    .padding(.leading, 8)
            }
        }
    }

    private func hostStatusDots(_ statuses: [AgentStatus]) -> some View {
        let shown = Array(statuses.prefix(5))
        let extra = statuses.count - shown.count
        return HStack(spacing: 4) {
            ForEach(Array(shown.enumerated()), id: \.offset) { _, status in
                StatusDot(status: status, running: true)
            }
            if extra > 0 {
                Text("+\(extra)")
                    .font(.mono(.caption2, weight: .semibold))
                    .foregroundStyle(theme.mutedColor)
            }
        }
        .fixedSize(horizontal: true, vertical: false)
    }
}

private struct ProjectRow: View {
    @Environment(\.theme) private var theme
    @Environment(\.horizontalSizeClass) private var hSizeClass
    let project: RemoteProject
    let selected: Bool
    /// True when this project is actively watched/monitored (highlighted folder icon).
    let watched: Bool
    /// True when this project is pinned to start monitoring automatically on launch.
    let autoWatched: Bool
    /// Live agent activity for this project (selected project derived live; other
    /// projects from the latest snapshot/sweep).
    let activity: AppState.ProjectActivity?
    let showAgents: Bool
    let onSelectAgent: (String) -> Void

    private static let maxDots = 6
    private static let maxListedAgents = 6

    private var showActiveHighlight: Bool {
        selected && hSizeClass == .regular
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 10) {
                Image(systemName: watched || showActiveHighlight ? "folder.fill" : "folder")
                    .foregroundStyle(watched || showActiveHighlight ? theme.accentColor : theme.mutedColor)
                    .accessibilityLabel(watched ? "Watching project" : "Project")
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(project.name)
                            .font(.mono(.callout, weight: showActiveHighlight ? .semibold : .regular))
                            .foregroundStyle(theme.textColor)
                            .lineLimit(1)
                            .truncationMode(.tail)
                        if autoWatched {
                            Image(systemName: "pin.fill")
                                .font(.system(size: 9))
                                .foregroundStyle(theme.mutedColor)
                                .accessibilityLabel("Auto-watched on launch")
                        }
                        Spacer(minLength: 4)
                        if let agents = activity?.agents, !agents.isEmpty {
                            statusDots(agents)
                        }
                    }
                    Text(project.remoteRoot)
                        .font(.mono(.caption2))
                        .foregroundStyle(theme.mutedColor)
                        .lineLimit(1)
                        .truncationMode(.head)
                }
            }
            if showAgents, let agents = activity?.agents, !agents.isEmpty {
                agentList(agents)
            }
        }
        .padding(.vertical, 3)
    }

    private func agentList(_ agents: [AppState.AgentSummary]) -> some View {
        let shown = Array(agents.prefix(Self.maxListedAgents))
        let extra = agents.count - shown.count
        return VStack(alignment: .leading, spacing: 4) {
            ForEach(shown) { agent in
                Button {
                    onSelectAgent(agent.id)
                } label: {
                    HStack(spacing: 7) {
                        StatusDot(status: agent.status, running: agent.running)
                        Text(agent.name)
                            .font(.mono(.caption, weight: agent.needsAttention ? .semibold : .regular))
                            .foregroundStyle(agent.running ? theme.textColor : theme.mutedColor)
                            .lineLimit(1)
                            .truncationMode(.tail)
                        Spacer(minLength: 6)
                        Text(statusText(for: agent))
                            .font(.mono(.caption2, weight: agent.needsAttention ? .semibold : .regular))
                            .foregroundStyle(statusColor(for: agent))
                            .lineLimit(1)
                            .fixedSize(horizontal: true, vertical: false)
                    }
                    .padding(.vertical, 2)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
            if extra > 0 {
                Text("+\(extra) more")
                    .font(.mono(.caption2))
                    .foregroundStyle(theme.mutedColor)
                    .padding(.leading, 16)
            }
        }
        .padding(.leading, 26)
    }

    private func statusText(for agent: AppState.AgentSummary) -> String {
        guard agent.running else { return "stopped" }
        switch agent.status {
        case .blocked: return "needs input"
        case .working: return "working"
        case .done: return "done"
        case .idle: return "idle"
        }
    }

    private func statusColor(for agent: AppState.AgentSummary) -> Color {
        guard agent.running else { return theme.mutedColor.opacity(0.75) }
        switch agent.status {
        case .blocked: return StatusDotContrast.legibleColor(hex: theme.red, on: theme.bg, minContrast: 3.5)
        case .working: return StatusDotContrast.legibleColor(hex: theme.yellow, on: theme.bg, minContrast: 3.5)
        case .done: return StatusDotContrast.legibleColor(hex: theme.green, on: theme.bg, minContrast: 3.5)
        case .idle: return theme.mutedColor
        }
    }

    private func statusDots(_ agents: [AppState.AgentSummary]) -> some View {
        let shown = Array(agents.prefix(Self.maxDots))
        let extra = agents.count - shown.count
        return HStack(spacing: 4) {
            ForEach(shown) { agent in
                StatusDot(status: agent.status, running: agent.running)
            }
            if extra > 0 {
                Text("+\(extra)")
                    .font(.mono(.caption2, weight: .semibold))
                    .foregroundStyle(theme.mutedColor)
            }
        }
        .fixedSize(horizontal: true, vertical: false)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(dotsAccessibilityLabel(agents))
    }

    private func dotsAccessibilityLabel(_ agents: [AppState.AgentSummary]) -> String {
        let runningAgents = agents.filter(\.running)
        let stoppedCount = agents.count - runningAgents.count
        let counts = Dictionary(grouping: runningAgents.map(\.status), by: { $0 }).mapValues(\.count)
        let parts: [String] = [
            counts[.blocked].map { "\($0) needs input" },
            counts[.working].map { "\($0) working" },
            counts[.done].map { "\($0) done" },
            counts[.idle].map { "\($0) idle" },
            stoppedCount > 0 ? "\(stoppedCount) stopped" : nil,
        ].compactMap { $0 }
        return "\(agents.count) agents: " + parts.joined(separator: ", ")
    }
}
