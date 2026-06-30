import SwiftUI

/// Top-level split layout: a collapsible sidebar (hosts → projects) and the active
/// project detail. `NavigationSplitView` gives the collapsible sidebar on iPad /
/// landscape and a push navigation on iPhone for free.
struct RootView: View {
    @EnvironmentObject var state: AppState
    @State private var showAddHost = false
    @State private var addProjectForHost: Host?
    @State private var discoverForHost: Host?

    var body: some View {
        NavigationSplitView {
            SidebarView(showAddHost: $showAddHost,
                        addProjectForHost: $addProjectForHost,
                        discoverForHost: $discoverForHost)
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
        .sheet(item: $discoverForHost) { host in DiscoverProjectsView(host: host) }
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
        .alert(
            (state.testResult?.ok == true) ? "Connection OK" : "Connection failed",
            isPresented: Binding(
                get: { state.testResult != nil },
                set: { if !$0 { state.testResult = nil } }
            ),
            presenting: state.testResult
        ) { _ in
            Button("OK", role: .cancel) { state.testResult = nil }
        } message: { result in
            Text("\(result.hostName): \(result.message)")
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
