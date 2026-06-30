import SwiftUI

/// The collapsible sidebar: hosts as sections, projects as rows, with add/delete.
/// Per-project live status across *all* projects is the background poller's job;
/// the sidebar shows live status only for the selected project (via the detail).
struct SidebarView: View {
    @EnvironmentObject var state: AppState
    @Binding var showAddHost: Bool
    @Binding var addProjectForHost: Host?
    @Binding var discoverForHost: Host?

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
                Text("No hosts yet — tap + to add one.")
                    .foregroundStyle(.secondary)
            }
            ForEach(state.doc.hosts) { host in
                Section {
                    ForEach(state.projects(for: host)) { project in
                        ProjectRow(project: project,
                                   selected: state.selectedProject?.id == project.id,
                                   running: state.runningCount(for: project))
                            .tag(project.id)
                    }
                    .onDelete { offsets in
                        let projects = state.projects(for: host)
                        offsets.map { projects[$0] }.forEach(state.deleteProject)
                    }
                    Button {
                        addProjectForHost = host
                    } label: {
                        Label("Add project", systemImage: "plus.circle")
                            .font(.callout)
                    }
                } header: {
                    HStack {
                        Label(host.name, systemImage: "server.rack")
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
                            Divider()
                            Button(role: .destructive) {
                                state.deleteHost(host)
                            } label: {
                                Label("Delete host", systemImage: "trash")
                            }
                        } label: {
                            Image(systemName: "ellipsis.circle")
                                .font(.title2)
                                .imageScale(.large)
                                .contentShape(Rectangle())
                                .padding(.vertical, 6)
                                .padding(.leading, 12)
                        }
                    }
                }
            }
        }
        .listStyle(.sidebar)
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button { showAddHost = true } label: {
                    Label("Add host", systemImage: "plus")
                }
            }
        }
    }
}

private struct ProjectRow: View {
    let project: RemoteProject
    let selected: Bool
    /// Live instance count for the selected project (0 / unknown for others, since
    /// only the selected project is polled in the foreground).
    let running: Int?

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "folder")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                Text(project.name)
                    .fontWeight(selected ? .semibold : .regular)
                Text(project.remoteRoot)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.head)
            }
            Spacer()
            if let running, running > 0 {
                Text("\(running)")
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 1)
                    .background(Capsule().fill(Color.green.opacity(0.18)))
                    .accessibilityLabel("\(running) running")
            }
        }
    }
}
