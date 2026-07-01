import SwiftUI

/// The collapsible sidebar: hosts as sections, projects as rows, with add/delete.
/// Per-project live status across *all* projects is the background poller's job;
/// the sidebar shows live status only for the selected project (via the detail).
struct SidebarView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    @Binding var showAddHost: Bool
    @Binding var addProjectForHost: Host?
    @Binding var discoverForHost: Host?
    @State private var showThemePicker = false
    @State private var showIdentities = false

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
                HStack(spacing: 6) {
                    Text("❯").foregroundStyle(theme.accentColor)
                    Text("no hosts yet — tap + to add one")
                        .foregroundStyle(theme.mutedColor)
                }
                .font(.mono(.footnote))
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
                        offsets.map { projects[$0] }.forEach(state.deleteProject)
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
        }
        .listStyle(.sidebar)
        .scrollContentBackground(.hidden)
        .background(theme.background.ignoresSafeArea())
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
