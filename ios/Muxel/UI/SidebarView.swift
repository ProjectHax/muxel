import SwiftUI
import UIKit

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
    @State private var showSettings = false
    @State private var deleteHostTarget: Host?
    @State private var deleteProjectsTarget: [RemoteProject] = []

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
            if state.doc.hosts.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    PromptLabel(text: "no hosts yet", style: .footnote)
                    Button { showAddHost = true } label: {
                        Label("Add a host", systemImage: "plus")
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                }
                .padding(.vertical, 4)
                .listRowBackground(Color.clear)
            }
            ForEach(state.doc.hosts) { host in
                Section {
                    if state.projects(for: host).isEmpty {
                        emptyProjectsHint(host)
                    } else {
                        ForEach(state.projects(for: host)) { project in
                            ProjectRow(project: project,
                                       selected: state.selectedProject?.id == project.id,
                                       activity: state.activity(for: project))
                                .tag(project.id)
                                .listRowBackground(Color.clear)
                        }
                        .onDelete { offsets in
                            let projects = state.projects(for: host)
                            deleteProjectsTarget = offsets.map { projects[$0] }
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
                } header: {
                    hostHeader(host)
                }
            }
            if state.notificationsDenied {
                notificationsDeniedRow
            }
        }
        .listStyle(.sidebar)
        .scrollContentBackground(.hidden)
        .muxelBackground()
        .refreshable { await state.refreshAll() }
        .toolbar {
            ToolbarItem(placement: .principal) {
                HStack(spacing: 6) {
                    Image("MuxelMark")
                        .resizable()
                        .scaledToFit()
                        .frame(width: 20, height: 20)
                    Text("muxel")
                        .font(.mono(.headline, weight: .bold))
                        .foregroundStyle(theme.textColor)
                        // Never wrap to "mux/el" when the sidebar column is narrow.
                        .lineLimit(1)
                        .fixedSize(horizontal: true, vertical: false)
                }
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
        .confirmationDialog(
            deleteProjectsTarget.isEmpty
                ? "Remove project?"
                : ConfirmationCopy.deleteProjects(deleteProjectsTarget).title,
            isPresented: Binding(
                get: { !deleteProjectsTarget.isEmpty },
                set: { if !$0 { deleteProjectsTarget = [] } }
            ),
            titleVisibility: .visible
        ) {
            Button(deleteProjectsTarget.count == 1 ? "Remove project" : "Remove projects",
                   role: .destructive) {
                deleteProjectsTarget.forEach(state.deleteProject)
                deleteProjectsTarget = []
            }
            Button("Cancel", role: .cancel) { deleteProjectsTarget = [] }
        } message: {
            if !deleteProjectsTarget.isEmpty {
                Text(ConfirmationCopy.deleteProjects(deleteProjectsTarget).message)
            }
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
                Label("Scan for projects", systemImage: "sparkle.magnifyingglass")
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.small)
            Button { addProjectForHost = host } label: {
                Label("Add by path", systemImage: "plus.circle")
                    .font(.mono(.footnote))
                    .foregroundStyle(theme.mutedColor)
            }
        }
        .padding(.vertical, 4)
        .listRowBackground(Color.clear)
    }

    private func hostHeader(_ host: Host) -> some View {
        HStack {
            HStack(spacing: 6) {
                Text("❯")
                    .font(.mono(.caption, weight: .bold))
                    .foregroundStyle(theme.accentColor)
                Text(host.name)
                    .font(.mono(.subheadline, weight: .semibold))
                    .foregroundStyle(theme.textColor)
            }
            Spacer()
            Menu {
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
                    .padding(.leading, 12)
            }
        }
    }
}

private struct ProjectRow: View {
    @Environment(\.theme) private var theme
    let project: RemoteProject
    let selected: Bool
    /// Live agent counts for this project. The selected project is derived live; other
    /// projects come from the cross-project sweep (`nil` = unknown → no badge).
    let activity: AppState.ProjectActivity?

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "folder")
                .foregroundStyle(selected ? theme.accentColor : theme.mutedColor)
            VStack(alignment: .leading, spacing: 1) {
                Text(project.name)
                    .font(.mono(.callout, weight: selected ? .semibold : .regular))
                    .foregroundStyle(theme.textColor)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Text(project.remoteRoot)
                    .font(.mono(.caption2))
                    .foregroundStyle(theme.mutedColor)
                    .lineLimit(1)
                    .truncationMode(.head)
            }
            Spacer()
            // Red "needs input" badge first (most urgent), then the running count.
            if let blocked = activity?.blocked, blocked > 0 {
                countBadge(blocked, color: theme.blockedColor, label: "\(blocked) need input")
            }
            if let running = activity?.running, running > 0 {
                countBadge(running, color: theme.runningColor, label: "\(running) running")
            }
        }
        .padding(.vertical, 2)
    }

    private func countBadge(_ n: Int, color: Color, label: String) -> some View {
        Text("\(n)")
            .font(.mono(.caption2, weight: .semibold))
            .foregroundStyle(color)
            .padding(.horizontal, 6)
            .padding(.vertical, 1)
            .background(Capsule().fill(color.opacity(0.18)))
            .accessibilityLabel(label)
    }
}
