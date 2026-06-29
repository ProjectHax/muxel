import SwiftUI

/// Top-level split layout: a collapsible sidebar (hosts → projects) and the active
/// project detail. `NavigationSplitView` gives the collapsible sidebar on iPad /
/// landscape and a push navigation on iPhone for free.
struct RootView: View {
    @EnvironmentObject var state: AppState
    @State private var showAddHost = false
    @State private var addProjectForHost: Host?

    var body: some View {
        NavigationSplitView {
            SidebarView(showAddHost: $showAddHost, addProjectForHost: $addProjectForHost)
                .navigationTitle("muxel")
        } detail: {
            if let project = state.selectedProject {
                ProjectDetailView(project: project)
            } else {
                placeholder
            }
        }
        .sheet(isPresented: $showAddHost) { AddHostView() }
        .sheet(item: $addProjectForHost) { host in AddProjectView(host: host) }
        .alert(
            "Something went wrong",
            isPresented: Binding(
                get: { state.errorMessage != nil },
                set: { if !$0 { state.errorMessage = nil } }
            )
        ) {
            Button("OK", role: .cancel) { state.errorMessage = nil }
        } message: {
            Text(state.errorMessage ?? "")
        }
    }

    private var placeholder: some View {
        VStack(spacing: 12) {
            Image(systemName: "terminal")
                .font(.system(size: 44))
                .foregroundStyle(.secondary)
            Text("Select a project")
                .font(.headline)
            Text("Add a host and a project from the sidebar to get started.")
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding()
    }
}
