import SwiftUI

/// The collapsible sidebar: hosts as sections, projects as rows, with add/delete.
/// Per-project live status across *all* projects is the background poller's job;
/// the sidebar shows live status only for the selected project (via the detail).
struct SidebarView: View {
    @EnvironmentObject var state: AppState
    @Binding var showAddHost: Bool
    @Binding var addProjectForHost: Host?

    var body: some View {
        List {
            if state.doc.hosts.isEmpty {
                Text("No hosts yet — tap + to add one.")
                    .foregroundStyle(.secondary)
            }
            ForEach(state.doc.hosts) { host in
                Section {
                    ForEach(state.projects(for: host)) { project in
                        ProjectRow(project: project,
                                   selected: state.selectedProject?.id == project.id)
                            .contentShape(Rectangle())
                            .onTapGesture { state.select(project) }
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
                            Button(role: .destructive) {
                                state.deleteHost(host)
                            } label: {
                                Label("Delete host", systemImage: "trash")
                            }
                        } label: {
                            Image(systemName: "ellipsis.circle").font(.caption)
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
        }
    }
}
