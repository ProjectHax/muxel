import SwiftUI

/// "Scan for projects" sheet: connects to the host, scans for `.muxel/` markers, and
/// lets you import the discovered project roots in one tap. The remote-dev-box path
/// to bringing in projects without typing each absolute path.
struct DiscoverProjectsView: View {
    @EnvironmentObject var state: AppState
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
        if scanning {
            VStack(spacing: 12) {
                ProgressView()
                Text("Scanning \(host.name) for muxel projects…")
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if let error {
            errorState(error)
        } else if found.isEmpty {
            emptyState
        } else {
            List {
                Section {
                    ForEach(found) { item in
                        row(item)
                    }
                } header: {
                    Text("\(found.count) project\(found.count == 1 ? "" : "s") found")
                } footer: {
                    Text("Each is a directory on \(host.name) containing a .muxel/ folder.")
                }
            }
        }
    }

    private func row(_ item: ProjectDiscovery.Found) -> some View {
        Button {
            if selected.contains(item.id) { selected.remove(item.id) } else { selected.insert(item.id) }
        } label: {
            HStack(spacing: 12) {
                Image(systemName: selected.contains(item.id) ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(selected.contains(item.id) ? Color.accentColor : Color.secondary)
                VStack(alignment: .leading, spacing: 1) {
                    Text(item.name)
                    Text(item.remoteRoot)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.head)
                }
            }
            .contentShape(Rectangle())
        }
        .tint(.primary)
    }

    private var emptyState: some View {
        VStack(spacing: 12) {
            Image(systemName: "folder.badge.questionmark")
                .font(.system(size: 40))
                .foregroundStyle(.secondary)
            Text("No projects found")
                .font(.headline)
            Text("Nothing under $HOME on \(host.name) has a .muxel/ folder yet. Open the project once in desktop muxel, or add it by path.")
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func errorState(_ message: String) -> some View {
        VStack(spacing: 12) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 40))
                .foregroundStyle(.orange)
            Text("Couldn't scan \(host.name)")
                .font(.headline)
            Text(message)
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            Button("Try again") { Task { await scan() } }
                .buttonStyle(.bordered)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
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
