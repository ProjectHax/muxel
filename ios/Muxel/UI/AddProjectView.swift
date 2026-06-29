import SwiftUI

/// Form to add a remote project under a host: a display name + the absolute project
/// root on the remote. The live layout is read from `<root>/.muxel/workspace.json`
/// (created by desktop muxel, or seeded when you launch the first instance).
struct AddProjectView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.dismiss) private var dismiss
    let host: Host

    @State private var name = ""
    @State private var remoteRoot = ""

    private var canSave: Bool { !name.isEmpty && remoteRoot.hasPrefix("/") }

    var body: some View {
        NavigationStack {
            Form {
                Section("Project") {
                    TextField("Name", text: $name)
                    TextField("Remote path  /srv/app", text: $remoteRoot)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.asciiCapable)
                }
                Section {
                    Text("On \(host.name). The path must be the project root on the remote (where its .muxel/ lives).")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }
            }
            .navigationTitle("Add project")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        let trimmed = remoteRoot.trimmingCharacters(in: .whitespaces)
                        let project = RemoteProject(name: name, hostId: host.id, remoteRoot: trimmed)
                        state.addProject(project)
                        dismiss()
                    }
                    .disabled(!canSave)
                }
            }
        }
    }
}
