import SwiftUI

/// Sheet to launch a new instance into a project: pick a built-in preset or type a
/// custom command. Creates a detached tmux session (muxel's naming) and records the
/// new pane in the shared `.muxel/workspace.json` so desktop sees it too.
struct LaunchInstanceView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    @Environment(\.dismiss) private var dismiss
    let project: RemoteProject
    /// On iPad, the focused leaf to launch into; nil launches into the main pane.
    var targetLeafAnchor: String? = nil

    @State private var selected: Preset = Preset.builtins.first!
    @State private var useCustom = false
    @State private var customCommand = ""
    @State private var systemPrompt = ""
    @State private var model = ""
    @State private var newWorktree = false
    @State private var worktreeBranch = ""

    /// The picked preset with the sheet's system-prompt / model overrides applied.
    private var effectivePreset: Preset {
        var p = selected
        p.systemPrompt = systemPrompt.isEmpty ? nil : systemPrompt
        if selected.modelFlag != nil { p.model = model.isEmpty ? nil : model }
        return p
    }

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
                if !useCustom {
                    MuxelSection("Options") {
                        DisclosureGroup("advanced") {
                            VStack(alignment: .leading, spacing: 4) {
                                TextField("system prompt", text: $systemPrompt, axis: .vertical)
                                    .lineLimit(1...4)
                                Text(injectionFooter)
                                    .font(.mono(.caption2))
                                    .foregroundStyle(theme.mutedColor)
                            }
                            if selected.modelFlag != nil {
                                TextField("model (e.g. opus)", text: $model)
                                    .textInputAutocapitalization(.never)
                                    .autocorrectionDisabled()
                            }
                            if selected.sessionIdFlag != nil {
                                Text("session id assigned automatically; reopening resumes it")
                                    .font(.mono(.caption2))
                                    .foregroundStyle(theme.mutedColor)
                            }
                        }
                    }
                }
                MuxelSection("Worktree") {
                    Toggle("Run in a new worktree", isOn: $newWorktree)
                    if newWorktree {
                        TextField("branch (default muxel/…)", text: $worktreeBranch)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                    }
                } footer: {
                    Text(newWorktree
                         ? "Creates a git worktree under the host's data dir and runs the agent there."
                         : "Runs in the project root, sharing your working tree.")
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
                                preset: effectivePreset,
                                customCommand: useCustom ? customCommand : nil,
                                into: project,
                                targetLeafAnchor: targetLeafAnchor,
                                newWorktree: newWorktree,
                                worktreeBranch: worktreeBranch.isEmpty ? nil : worktreeBranch
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
        guard let program = effectivePreset.program else { return "(login shell)" }
        var parts = [program] + AgentLaunch.composeArgs(effectivePreset)
        // Show a CliFlag prompt (Claude) inline; TypeIn prompts are typed after launch.
        if !systemPrompt.isEmpty, case let .cliFlag(flag) = selected.injection {
            parts.append(flag)
            parts.append("“\(systemPrompt)”")
        }
        return parts.joined(separator: " ")
    }

    /// How the entered system prompt will reach the agent, per its injection mode.
    private var injectionFooter: String {
        guard !systemPrompt.isEmpty else { return "delivered per the agent's injection mode" }
        switch selected.injection {
        case let .cliFlag(flag): return "passed via \(flag)"
        case .typeIn: return "typed in after the agent starts"
        case .none: return "this preset ignores a system prompt"
        }
    }
}
