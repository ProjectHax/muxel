import SwiftUI

/// Sheet to launch a new instance into a project: pick a built-in preset or type a
/// custom command. Creates a detached tmux session (muxel's naming) and records the
/// new pane in the shared `.muxel/workspace.json` so desktop sees it too.
struct LaunchInstanceView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.dismiss) private var dismiss
    let project: RemoteProject

    @State private var selected: Preset = Preset.builtins.first!
    @State private var useCustom = false
    @State private var customCommand = ""

    var body: some View {
        NavigationStack {
            Form {
                Section("Agent") {
                    Picker("Preset", selection: $selected) {
                        ForEach(Preset.builtins) { preset in
                            Text(preset.name).tag(preset)
                        }
                    }
                    .disabled(useCustom)

                    Toggle("Custom command", isOn: $useCustom)
                    if useCustom {
                        TextField("e.g. claude --model opus", text: $customCommand)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                    }
                }
                Section {
                    Text(previewCommand)
                        .font(.system(.footnote, design: .monospaced))
                        .foregroundStyle(.secondary)
                } header: {
                    Text("Will run in \(project.remoteRoot)")
                }
            }
            .navigationTitle("New instance")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Launch") {
                        Task {
                            await state.launch(
                                preset: selected,
                                customCommand: useCustom ? customCommand : nil,
                                into: project
                            )
                            dismiss()
                        }
                    }
                    .disabled(useCustom && customCommand.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
        }
    }

    private var previewCommand: String {
        if useCustom {
            return customCommand.isEmpty ? "(shell)" : customCommand
        }
        guard let program = selected.program else { return "(login shell)" }
        return ([program] + selected.args).joined(separator: " ")
    }
}
