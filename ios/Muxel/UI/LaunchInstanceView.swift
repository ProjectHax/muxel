import SwiftUI

/// Sheet to launch a new instance into a project: pick a built-in preset or type a
/// custom command. Creates a detached tmux session (muxel's naming) and records the
/// new pane in the shared `.muxel/workspace.json` so desktop sees it too.
struct LaunchInstanceView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    @Environment(\.dismiss) private var dismiss
    let project: RemoteProject

    @State private var selected: Preset = Preset.builtins.first!
    @State private var useCustom = false
    @State private var customCommand = ""

    var body: some View {
        NavigationStack {
            Form {
                MuxelSection("Agent") {
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
                        if !commandParses {
                            Label("unbalanced quote", systemImage: "exclamationmark.triangle")
                                .font(.mono(.caption))
                                .foregroundStyle(theme.blockedColor)
                        }
                    }
                }
                MuxelSection("Command") {
                    Text(previewCommand)
                        .font(.mono(.footnote))
                        .foregroundStyle(theme.mutedColor)
                } footer: {
                    Text("Runs in \(project.remoteRoot).")
                }
            }
            .muxelSheet()
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
                    .disabled(useCustom
                        && (customCommand.trimmingCharacters(in: .whitespaces).isEmpty
                            || !commandParses))
                }
            }
        }
    }

    /// Whether the custom command splits into shell words (quotes balanced) — the
    /// same parse `AppState.launch` uses, so Launch can't submit what launch rejects.
    private var commandParses: Bool {
        !useCustom || Shell.splitWords(customCommand) != nil
    }

    private var previewCommand: String {
        if useCustom {
            return customCommand.isEmpty ? "(shell)" : customCommand
        }
        guard let program = selected.program else { return "(login shell)" }
        return ([program] + selected.args).joined(separator: " ")
    }
}
