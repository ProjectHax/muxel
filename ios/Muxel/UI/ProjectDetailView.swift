import SwiftUI

/// The active project: a horizontal tab bar of its panes (with status dots) and the
/// selected pane's terminal. MVP renders the active leaf's tabs; full split-tree
/// rendering is a later phase.
struct ProjectDetailView: View {
    @EnvironmentObject var state: AppState
    let project: RemoteProject
    @State private var selectedTab: String?
    @State private var showLaunch = false

    private var instances: [Instance] { state.layout?.orderedTerminalInstances ?? [] }

    private var current: Instance? {
        instances.first { $0.id == selectedTab } ?? instances.first
    }

    var body: some View {
        VStack(spacing: 0) {
            tabBar
            Divider()
            if let inst = current, let host = state.host(for: project) {
                TerminalPaneView(host: host, project: project, instance: inst)
                    .id(inst.id)
            } else {
                emptyState
            }
        }
        .navigationTitle(project.name)
        .navigationBarTitleDisplayMode(.inline)
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
        .onChange(of: state.layout?.orderedTerminalInstances.map(\.id) ?? []) { _, ids in
            if selectedTab == nil || !ids.contains(selectedTab!) {
                selectedTab = ids.first
            }
        }
    }

    private var tabBar: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(instances) { inst in
                    let isActive = inst.id == current?.id
                    Button {
                        selectedTab = inst.id
                        state.attend(inst.id)
                    } label: {
                        HStack(spacing: 6) {
                            StatusDot(status: state.status(inst.id))
                            Text(inst.displayName).lineLimit(1)
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 7)
                        .background(isActive ? Color.accentColor.opacity(0.18) : Color(.secondarySystemBackground))
                        .clipShape(Capsule())
                    }
                    .buttonStyle(.plain)
                    .contextMenu {
                        Button(role: .destructive) {
                            Task { await state.close(inst, in: project) }
                        } label: {
                            Label("Close instance", systemImage: "xmark.circle")
                        }
                    }
                }
            }
            .padding(.horizontal)
            .padding(.vertical, 8)
        }
    }

    private var emptyState: some View {
        VStack(spacing: 10) {
            if state.isBusy {
                ProgressView()
            } else {
                Image(systemName: "rectangle.dashed").font(.largeTitle).foregroundStyle(.secondary)
                Text("No instances yet").font(.headline)
                Button { showLaunch = true } label: {
                    Label("Launch one", systemImage: "plus")
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
