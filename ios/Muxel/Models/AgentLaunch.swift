import Foundation

/// Concrete launch parameters resolved from an instance. Port of `ResolvedLaunch`
/// (`crates/muxel-core/src/agent.rs`) — the fields the iOS PTY launch uses.
struct ResolvedLaunch: Equatable {
    var program: String?
    var args: [String]
    /// Text to type into the terminal once the agent is ready (TypeIn injection).
    var startupInput: String?
    /// Press Enter to submit after typing `startupInput`.
    var submit: Bool
}

/// Pure ports of agent.rs launch resolution. These decide the exact program/args/
/// startup-input for a launch, so the iOS command matches what desktop would run.
enum AgentLaunch {
    /// Compose `modelFlag model`, then `effortFlag effort`, then the extra args. Each
    /// pair is skipped unless both flag and value are set + non-empty. Port of
    /// `compose_args`.
    static func composeArgs(_ preset: Preset) -> [String] {
        var out: [String] = []
        if let flag = preset.modelFlag, let model = preset.model, !model.isEmpty {
            out.append(flag); out.append(model)
        }
        if let flag = preset.effortFlag, let effort = preset.effort, !effort.isEmpty {
            out.append(flag); out.append(effort)
        }
        out.append(contentsOf: preset.args)
        return out
    }

    /// Resolve an instance into program/args + any TypeIn startup input, applying its
    /// system-prompt injection mode. Port of `resolve_launch`.
    static func resolveLaunch(_ instance: Instance) -> ResolvedLaunch {
        var args = instance.args
        var startupInput: String?
        if let prompt = instance.systemPrompt, !prompt.isEmpty {
            switch instance.injection {
            case let .cliFlag(flag):
                args.append(flag)
                args.append(prompt)
            case .typeIn:
                startupInput = prompt
            case .none:
                break
            }
        }
        return ResolvedLaunch(program: instance.program, args: args,
                              startupInput: startupInput, submit: instance.autoSubmit)
    }

    /// CLI args to start or resume a session for a resume-capable agent. `nil` when the
    /// preset lacks resume flags or the instance has no session id. `[idFlag, id]` on
    /// the first launch (creating the session with the chosen id), `[resumeFlag, id]`
    /// once started. Port of `session_resume_args`.
    static func sessionResumeArgs(preset: Preset, instance: Instance) -> [String]? {
        guard let idFlag = preset.sessionIdFlag,
              let resumeFlag = preset.resumeFlag,
              let id = instance.sessionId else { return nil }
        return [instance.sessionStarted ? resumeFlag : idFlag, id]
    }

    /// Recover the builtin preset an instance was launched from — by stored preset
    /// name, then by program basename — so a re-attach knows its resume/model flags.
    /// (iOS builtin ids are per-process random, so we match on name/program, not id.)
    static func builtinPreset(for instance: Instance) -> Preset? {
        if !instance.preset.isEmpty,
           let byName = Preset.builtins.first(where: { $0.name == instance.preset }) {
            return byName
        }
        guard let program = instance.program else {
            return Preset.builtins.first { $0.program == nil }  // Shell
        }
        let base = basename(program)
        return Preset.builtins.first { preset in
            guard let pp = preset.program else { return false }
            return basename(pp) == base
        }
    }

    private static func basename(_ path: String) -> String {
        path.split(whereSeparator: { $0 == "/" || $0 == "\\" }).last.map(String.init) ?? path
    }
}
