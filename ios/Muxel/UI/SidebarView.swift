import SwiftUI
import UIKit

/// The collapsible sidebar: hosts as sections, projects as rows, with add/delete.
/// Per-project live status across *all* projects is the background poller's job;
/// the sidebar shows live status only for the selected project (via the detail).
struct SidebarView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    @Environment(\.openURL) private var openURL
    @Binding var showAddHost: Bool
    @Binding var addProjectForHost: Host?
    @Binding var discoverForHost: Host?
    @Binding var editHost: Host?
    @State private var showThemePicker = false
    @State private var showIdentities = false
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
                PromptLabel(text: "no hosts yet — tap + to add one", style: .footnote)
                    .listRowBackground(Color.clear)
            }
            ForEach(state.doc.hosts) { host in
                Section {
                    ForEach(state.projects(for: host)) { project in
                        ProjectRow(project: project,
                                   selected: state.selectedProject?.id == project.id,
                                   running: state.runningCount(for: project))
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
                }
            }
            ToolbarItem(placement: .topBarLeading) {
                Button { showThemePicker = true } label: {
                    Image(systemName: "paintpalette")
                }
                .accessibilityLabel("Theme")
            }
            ToolbarItem(placement: .topBarLeading) {
                Button { showIdentities = true } label: {
                    Image(systemName: "person.badge.key")
                }
                .accessibilityLabel("Login identities")
            }
            ToolbarItem(placement: .primaryAction) {
                Button { showAddHost = true } label: {
                    Label("Add host", systemImage: "plus")
                }
            }
        }
        .sheet(isPresented: $showThemePicker) { ThemePickerView() }
        .sheet(isPresented: $showIdentities) { IdentitiesView() }
        .confirmationDialog(
            deleteHostTarget.map {
                ConfirmationCopy.deleteHost($0, projectCount: state.projects(for: $0).count).title
            } ?? "Delete host?",
            isPresented: Binding(
                get: { deleteHostTarget != nil },
                set: { if !$0 { deleteHostTarget = nil } }
            ),
            titleVisibility: .visible,
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
    /// Live instance count for the selected project (0 / unknown for others, since
    /// only the selected project is polled in the foreground).
    let running: Int?

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "folder")
                .foregroundStyle(selected ? theme.accentColor : theme.mutedColor)
            VStack(alignment: .leading, spacing: 1) {
                Text(project.name)
                    .font(.mono(.callout, weight: selected ? .semibold : .regular))
                    .foregroundStyle(theme.textColor)
                Text(project.remoteRoot)
                    .font(.mono(.caption2))
                    .foregroundStyle(theme.mutedColor)
                    .lineLimit(1)
                    .truncationMode(.head)
            }
            Spacer()
            if let running, running > 0 {
                Text("\(running)")
                    .font(.mono(.caption2, weight: .semibold))
                    .foregroundStyle(theme.runningColor)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 1)
                    .background(Capsule().fill(theme.runningColor.opacity(0.18)))
                    .accessibilityLabel("\(running) running")
            }
        }
        .padding(.vertical, 2)
    }
}
