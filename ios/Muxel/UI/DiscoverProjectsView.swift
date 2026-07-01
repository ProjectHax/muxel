import SwiftUI

/// "Scan for projects" sheet: connects to the host, scans for `.muxel/` markers, and
/// lets you import the discovered project roots in one tap. The remote-dev-box path
/// to bringing in projects without typing each absolute path.
struct DiscoverProjectsView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    @Environment(\.dismiss) private var dismiss
    let host: Host

    @State private var found: [ProjectDiscovery.Found] = []
    @State private var selected: Set<String> = []
    @State private var scanning = true
    @State private var error: String?

    var body: some View {
        NavigationStack {
            content
                .navigationTitle("Scan \(host.name)")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Import\(selected.isEmpty ? "" : " (\(selected.count))")") {
                            state.importDiscovered(found.filter { selected.contains($0.id) }, on: host)
                            dismiss()
                        }
                        .disabled(selected.isEmpty)
                    }
                }
        }
        .task { await scan() }
    }

    @ViewBuilder
    private var content: some View {
        Group {
            if scanning {
                CenteredState(spinner: true,
                              title: "scanning \(host.name)…",
                              message: "looking for .muxel/ markers under $HOME",
                              showsGrid: true)
            } else if let error {
                CenteredState(icon: "exclamationmark.triangle",
                              iconColor: theme.blockedColor,
                              title: "Couldn't scan \(host.name)",
                              message: error,
                              showsGrid: true) {
                    Button("Try again") { Task { await scan() } }
                        .buttonStyle(.bordered)
                }
            } else if found.isEmpty {
                CenteredState(icon: "folder.badge.questionmark",
                              title: "No projects found",
                              message: "Nothing under $HOME on \(host.name) has a .muxel/ "
                                + "folder yet. Open the project once in desktop muxel, "
                                + "or add it by path.",
                              showsGrid: true)
            } else {
                List {
                    MuxelSection("\(found.count) project\(found.count == 1 ? "" : "s") found") {
                        ForEach(found) { item in
                            row(item)
                        }
                    } footer: {
                        Text("Each is a directory on \(host.name) containing a .muxel/ folder.")
                    }
                }
            }
        }
        .muxelSheet()
    }

    private func row(_ item: ProjectDiscovery.Found) -> some View {
        Button {
            if selected.contains(item.id) { selected.remove(item.id) } else { selected.insert(item.id) }
        } label: {
            HStack(spacing: 12) {
                Image(systemName: selected.contains(item.id) ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(selected.contains(item.id) ? theme.accentColor : theme.mutedColor)
                VStack(alignment: .leading, spacing: 1) {
                    Text(item.name)
                        .font(.mono(.callout))
                    Text(item.remoteRoot)
                        .font(.mono(.caption2))
                        .foregroundStyle(theme.mutedColor)
                        .lineLimit(1)
                        .truncationMode(.head)
                }
            }
            .contentShape(Rectangle())
        }
        .tint(theme.textColor)
    }

    private func scan() async {
        scanning = true
        error = nil
        do {
            let results = try await state.scanProjects(on: host)
            found = results
            selected = Set(results.map(\.id)) // default: import everything found
        } catch {
            self.error = error.localizedDescription
        }
        scanning = false
    }
}
