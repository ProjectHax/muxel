import Foundation

/// An environment variable for an agent process. Port of `EnvVar` (agent.rs).
struct EnvVar: Codable, Equatable {
    var key: String
    var value: String
}

/// What a pane runs. Port of `InstanceKind` (`crates/muxel-core/src/lib.rs`) —
/// serialized as the bare variant name. A kind the iOS build doesn't know (e.g. a
/// newer desktop adds one) decodes to `.other(raw)` and re-encodes verbatim, so a
/// pane iOS can't render can't corrupt the layout or break the whole decode.
enum InstanceKind: Equatable {
    case terminal
    case editor
    case diff
    case browser
    case other(String)

    init(rawValue: String) {
        switch rawValue {
        case "Terminal": self = .terminal
        case "Editor": self = .editor
        case "Diff": self = .diff
        case "Browser": self = .browser
        default: self = .other(rawValue)
        }
    }

    var rawValue: String {
        switch self {
        case .terminal: return "Terminal"
        case .editor: return "Editor"
        case .diff: return "Diff"
        case .browser: return "Browser"
        case .other(let raw): return raw
        }
    }
}

extension InstanceKind: Codable {
    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        self.init(rawValue: try c.decode(String.self))
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        try c.encode(rawValue)
    }
}

/// Persisted desktop activity snapshot decoded from `.muxel/workspace.json`
/// (`AgentActivity` in `crates/muxel-core/src/lib.rs`). Read-only on iOS: never
/// re-encoded on write-back so desktop's `ios_write_back_is_not_a_layout_change`
/// contract stays intact.
struct PersistedAgentActivity: Equatable {
    var workStartedAt: Int64?
    var completedAt: Int64?
    var blockedAt: Int64?
    var lastAttendedAt: Int64?
    var lastState: AgentStatus?

    /// Mirrors `AgentActivity::current_state`: explicit `last_state`, else inferred
    /// from the latest transition timestamp.
    var currentState: AgentStatus? {
        if let lastState { return lastState }
        let candidates: [(Int64, Int, AgentStatus)] = [
            workStartedAt.map { ($0, 0, .working) },
            completedAt.map { ($0, 1, .done) },
            blockedAt.map { ($0, 2, .blocked) },
        ].compactMap { $0 }
        return candidates.max { a, b in
            a.0 != b.0 ? a.0 < b.0 : a.1 < b.1
        }?.2
    }

    /// Mirrors `AgentActivity::has_unattended_completion`: whether a completed turn
    /// is newer than the last time the pane was attended on desktop.
    var hasUnattendedCompletion: Bool {
        guard let completedAt else { return false }
        guard let lastAttendedAt else { return true }
        return completedAt > lastAttendedAt
    }

    /// Live-display / notification state derived from desktop's persisted `activity`:
    /// - downgrades already-attended `done`/`blocked` states (`last_attended_at` at or
    ///   after the completion/block timestamp) to `.idle`, because desktop keeps
    ///   `last_state == "done"` even after the user read the pane;
    /// - downgrades ancient `done` states whose `completed_at` is older than 5 minutes
    ///   (and `blocked` older than 30 minutes) to `.idle`, so a tmux resize/attach
    ///   bumping `window_activity` can never resurrect an hours-old completion.
    func effectiveLiveState(now: TimeInterval = Date().timeIntervalSince1970) -> AgentStatus? {
        guard let state = currentState else { return nil }
        switch state {
        case .done:
            if let completedAt {
                if let lastAttendedAt, lastAttendedAt >= completedAt {
                    return .idle
                }
                let completedSecs = TimeInterval(completedAt) / 1000.0
                if now - completedSecs > 300 {
                    return .idle
                }
            }
            return .done
        case .blocked:
            if let blockedAt {
                if let lastAttendedAt, lastAttendedAt >= blockedAt {
                    return .idle
                }
                let blockedSecs = TimeInterval(blockedAt) / 1000.0
                if now - blockedSecs > 1800 {
                    return .idle
                }
            }
            return .blocked
        case .working, .idle:
            return state
        }
    }

    var effectiveLiveState: AgentStatus? {
        effectiveLiveState()
    }
}

extension PersistedAgentActivity: Decodable {
    private enum CodingKeys: String, CodingKey {
        case workStartedAt = "work_started_at"
        case completedAt = "completed_at"
        case blockedAt = "blocked_at"
        case lastAttendedAt = "last_attended_at"
        case lastState = "last_state"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        workStartedAt = try c.decodeIfPresent(Int64.self, forKey: .workStartedAt)
        completedAt = try c.decodeIfPresent(Int64.self, forKey: .completedAt)
        blockedAt = try c.decodeIfPresent(Int64.self, forKey: .blockedAt)
        lastAttendedAt = try c.decodeIfPresent(Int64.self, forKey: .lastAttendedAt)
        if let raw = try c.decodeIfPresent(String.self, forKey: .lastState) {
            lastState = AgentStatus(rawValue: raw)
        }
    }
}

/// Persisted metadata for one pane. Port of `Instance`
/// (`crates/muxel-core/src/lib.rs`). Every field except `id`/`projectId`/`title`
/// is `#[serde(default)]` in Rust, so decoding tolerates older documents missing
/// them. Encoding writes all fields (Rust serializes all fields).
struct Instance: Codable, Equatable, Identifiable {
    var id: String
    var projectId: String
    var title: String
    var kind: InstanceKind = .terminal
    var editorPath: String?
    /// For browser panes: the current URL. iOS never renders browser panes, but
    /// preserves this so an iOS layout write-back doesn't strip it from the peer.
    var browserUrl: String?
    var customName: String?
    /// Last program-supplied title. Preserved across remote-layout write-back;
    /// a manual custom name still wins.
    var autoName: String?
    var program: String?
    var args: [String] = []
    var literalArgv: Bool = false
    var systemPrompt: String?
    var injection: InjectionMode = .none
    var instructionInjection: InjectionMode?
    var preset: String = ""
    var presetId: String?
    var env: [EnvVar] = []
    /// Desktop's persisted activity state (read-only; omitted on encode).
    var activity: PersistedAgentActivity? = nil
    var useTmux: Bool = false
    var useWorktree: Bool = false
    var tmuxSession: String?
    var launchDir: String?
    var tmuxSocket: String?
    var worktreePath: String?
    var worktreeBranch: String?
    var autoModePresses: Int = 0
    /// The pane's own auto-continue message (desktop Auto button, right-click).
    /// iOS doesn't auto-continue, but preserves this so an iOS layout write-back
    /// doesn't strip it from the peer.
    var autoContinueMessage: String?
    var isRunner: Bool = false
    var controlPrompt: Bool = false
    var controlCreated: Bool = false
    var autoSubmit: Bool = false
    var pinned: Bool = false
    var worktreeId: String?
    var sessionId: String?
    /// The desktop's validated provider transcript/rollout path for `sessionId`
    /// (a path on the machine running the pane). iOS never reads it, but
    /// preserves it so an iOS layout write-back doesn't strip it from the peer.
    var sessionPath: String?
    var sessionStarted: Bool = false

    private enum CodingKeys: String, CodingKey {
        case id
        case projectId = "project_id"
        case title, kind
        case editorPath = "editor_path"
        case browserUrl = "browser_url"
        case customName = "custom_name"
        case autoName = "auto_name"
        case program, args
        case literalArgv = "literal_argv"
        case systemPrompt = "system_prompt"
        case injection, preset
        case instructionInjection = "instruction_injection"
        case presetId = "preset_id"
        case env, activity
        case useTmux = "use_tmux"
        case useWorktree = "use_worktree"
        case tmuxSession = "tmux_session"
        case launchDir = "launch_dir"
        case tmuxSocket = "tmux_socket"
        case worktreePath = "worktree_path"
        case worktreeBranch = "worktree_branch"
        case autoModePresses = "auto_mode_presses"
        case autoContinueMessage = "auto_continue_message"
        case isRunner = "is_runner"
        case controlPrompt = "control_prompt"
        case controlCreated = "control_created"
        case autoSubmit = "auto_submit"
        case pinned
        case worktreeId = "worktree_id"
        case sessionId = "session_id"
        case sessionPath = "session_path"
        case sessionStarted = "session_started"
    }

    /// Build a fresh terminal instance the iOS app is about to launch. `sessionStarted`
    /// stays false so a resume-capable agent's first launch uses `--session-id` (later
    /// re-attaches flip it to `--resume`); it's inert for non-resume presets.
    init(id: String, projectId: String, title: String, program: String?, args: [String]) {
        self.id = id
        self.projectId = projectId
        self.title = title
        self.program = program
        self.args = args
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        projectId = try c.decode(String.self, forKey: .projectId)
        title = try c.decode(String.self, forKey: .title)
        kind = (try c.decodeIfPresent(InstanceKind.self, forKey: .kind)) ?? .terminal
        editorPath = try c.decodeIfPresent(String.self, forKey: .editorPath)
        browserUrl = try c.decodeIfPresent(String.self, forKey: .browserUrl)
        customName = try c.decodeIfPresent(String.self, forKey: .customName)
        autoName = try c.decodeIfPresent(String.self, forKey: .autoName)
        program = try c.decodeIfPresent(String.self, forKey: .program)
        args = (try c.decodeIfPresent([String].self, forKey: .args)) ?? []
        literalArgv = (try c.decodeIfPresent(Bool.self, forKey: .literalArgv)) ?? false
        systemPrompt = try c.decodeIfPresent(String.self, forKey: .systemPrompt)
        injection = (try c.decodeIfPresent(InjectionMode.self, forKey: .injection)) ?? .none
        instructionInjection = try c.decodeIfPresent(InjectionMode.self, forKey: .instructionInjection)
        preset = (try c.decodeIfPresent(String.self, forKey: .preset)) ?? ""
        presetId = try c.decodeIfPresent(String.self, forKey: .presetId)
        env = (try c.decodeIfPresent([EnvVar].self, forKey: .env)) ?? []
        activity = try c.decodeIfPresent(PersistedAgentActivity.self, forKey: .activity)
        useTmux = (try c.decodeIfPresent(Bool.self, forKey: .useTmux)) ?? false
        useWorktree = (try c.decodeIfPresent(Bool.self, forKey: .useWorktree)) ?? false
        tmuxSession = try c.decodeIfPresent(String.self, forKey: .tmuxSession)
        launchDir = try c.decodeIfPresent(String.self, forKey: .launchDir)
        // An owner tmux can't use as a socket name reads as none (legacy discovery),
        // like `tmux::recorded_socket` on desktop, rather than failing the layout.
        tmuxSocket = try c.decodeIfPresent(String.self, forKey: .tmuxSocket).flatMap {
            TmuxCommands.isSocketName($0) ? $0 : nil
        }
        worktreePath = try c.decodeIfPresent(String.self, forKey: .worktreePath)
        worktreeBranch = try c.decodeIfPresent(String.self, forKey: .worktreeBranch)
        autoModePresses = (try c.decodeIfPresent(Int.self, forKey: .autoModePresses)) ?? 0
        autoContinueMessage = try c.decodeIfPresent(String.self, forKey: .autoContinueMessage)
        isRunner = (try c.decodeIfPresent(Bool.self, forKey: .isRunner)) ?? false
        controlPrompt = (try c.decodeIfPresent(Bool.self, forKey: .controlPrompt)) ?? false
        controlCreated = (try c.decodeIfPresent(Bool.self, forKey: .controlCreated)) ?? false
        autoSubmit = (try c.decodeIfPresent(Bool.self, forKey: .autoSubmit)) ?? false
        pinned = (try c.decodeIfPresent(Bool.self, forKey: .pinned)) ?? false
        worktreeId = try c.decodeIfPresent(String.self, forKey: .worktreeId)
        sessionId = try c.decodeIfPresent(String.self, forKey: .sessionId)
        sessionPath = try c.decodeIfPresent(String.self, forKey: .sessionPath)
        sessionStarted = (try c.decodeIfPresent(Bool.self, forKey: .sessionStarted)) ?? false
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(projectId, forKey: .projectId)
        try c.encode(title, forKey: .title)
        try c.encode(kind, forKey: .kind)
        try c.encode(editorPath, forKey: .editorPath)
        try c.encode(browserUrl, forKey: .browserUrl)
        try c.encode(customName, forKey: .customName)
        try c.encode(autoName, forKey: .autoName)
        try c.encode(program, forKey: .program)
        try c.encode(args, forKey: .args)
        try c.encode(literalArgv, forKey: .literalArgv)
        try c.encode(systemPrompt, forKey: .systemPrompt)
        try c.encode(injection, forKey: .injection)
        try c.encode(instructionInjection, forKey: .instructionInjection)
        try c.encode(preset, forKey: .preset)
        try c.encode(presetId, forKey: .presetId)
        try c.encode(env, forKey: .env)
        try c.encode(useTmux, forKey: .useTmux)
        try c.encode(useWorktree, forKey: .useWorktree)
        try c.encode(tmuxSession, forKey: .tmuxSession)
        try c.encode(launchDir, forKey: .launchDir)
        try c.encode(tmuxSocket, forKey: .tmuxSocket)
        try c.encode(worktreePath, forKey: .worktreePath)
        try c.encode(worktreeBranch, forKey: .worktreeBranch)
        try c.encode(autoModePresses, forKey: .autoModePresses)
        try c.encode(autoContinueMessage, forKey: .autoContinueMessage)
        try c.encode(isRunner, forKey: .isRunner)
        try c.encode(controlPrompt, forKey: .controlPrompt)
        try c.encode(controlCreated, forKey: .controlCreated)
        try c.encode(autoSubmit, forKey: .autoSubmit)
        try c.encode(pinned, forKey: .pinned)
        try c.encode(worktreeId, forKey: .worktreeId)
        try c.encode(sessionId, forKey: .sessionId)
        try c.encode(sessionPath, forKey: .sessionPath)
        try c.encode(sessionStarted, forKey: .sessionStarted)
    }

    /// True when this terminal pane runs an interactive shell (no explicit agent
    /// program, or a standard shell binary like `bash`, `zsh`, `fish`, `sh`).
    var isShell: Bool {
        guard kind == .terminal else { return false }
        guard let program, !program.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return true
        }
        return Self.isShellCommand(program)
    }

    /// Whether a program or tmux `#{pane_current_command}` string names a bare shell
    /// sitting at its prompt (as opposed to a foreground command running inside it).
    static func isShellCommand(_ command: String) -> Bool {
        let trimmed = command.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return false }
        let base = (trimmed as NSString).lastPathComponent
            .trimmingCharacters(in: CharacterSet(charactersIn: "-"))
            .lowercased()
        return Self.shellNames.contains(base)
    }

    private static let shellNames: Set<String> = [
        "sh", "bash", "zsh", "fish", "dash", "ash", "ksh", "mksh", "csh", "tcsh",
        "nu", "nushell", "elvish", "xonsh", "pwsh", "login",
    ]

    /// The display label shown in the tab bar.
    var displayName: String {
        if let customName, !customName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return customName
        }
        if let autoName,
           !autoName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
           UUID(uuidString: autoName.trimmingCharacters(in: .whitespacesAndNewlines)) == nil
        {
            return autoName
        }
        return title
    }

    mutating func resetConversationForDuplicate() {
        sessionId = nil
        sessionPath = nil
        sessionStarted = false
        autoName = nil
        controlCreated = false
        // An unconsumed `ctl new --prompt` task belongs to the request that was
        // approved, never to a copy of its pane. Port of Rust's
        // `reset_conversation_for_duplicate`.
        if controlPrompt {
            systemPrompt = nil
        }
    }
}
