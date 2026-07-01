import SwiftUI

/// Top-level split layout: a collapsible sidebar (hosts → projects) and the active
/// project detail. `NavigationSplitView` gives the collapsible sidebar on iPad /
/// landscape and a push navigation on iPhone for free.
struct RootView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    @State private var showAddHost = false
    @State private var addProjectForHost: Host?
    @State private var discoverForHost: Host?

    var body: some View {
        NavigationSplitView {
            SidebarView(showAddHost: $showAddHost,
                        addProjectForHost: $addProjectForHost,
                        discoverForHost: $discoverForHost)
                .navigationTitle("muxel")
                .navigationBarTitleDisplayMode(.inline)
        } detail: {
            if let project = state.selectedProject {
                ProjectDetailView(project: project)
            } else {
                placeholder
            }
        }
        .onAppear { KeyboardPrewarmer.warmOnce() }
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
        ZStack {
            theme.background.ignoresSafeArea()
            GridBackground().opacity(0.5)
            VStack(spacing: 14) {
                Image("MuxelMark")
                    .resizable()
                    .scaledToFit()
                    .frame(width: 64, height: 64)
                Text("muxel")
                    .font(.mono(.largeTitle, weight: .bold))
                    .foregroundStyle(theme.textColor)
                HStack(spacing: 6) {
                    Text("❯").foregroundStyle(theme.accentColor)
                    Text("select a project").foregroundStyle(theme.mutedColor)
                }
                .font(.mono(.callout))
                Text("Add a host and a project from the sidebar to get started.")
                    .font(.footnote)
                    .foregroundStyle(theme.mutedColor)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 40)
            }
        }
    }
}
