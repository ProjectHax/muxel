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
    var systemPrompt: String?
    var injection: InjectionMode = .none
    var preset: String = ""
    var presetId: String?
    var env: [EnvVar] = []
    var useTmux: Bool = false
    var useWorktree: Bool = false
    var tmuxSession: String?
    var worktreePath: String?
    var worktreeBranch: String?
    var autoModePresses: Int = 0
    var isRunner: Bool = false
    var autoSubmit: Bool = false
    var pinned: Bool = false
    var worktreeId: String?
    var sessionId: String?
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
        case systemPrompt = "system_prompt"
        case injection, preset
        case presetId = "preset_id"
        case env
        case useTmux = "use_tmux"
        case useWorktree = "use_worktree"
        case tmuxSession = "tmux_session"
        case worktreePath = "worktree_path"
        case worktreeBranch = "worktree_branch"
        case autoModePresses = "auto_mode_presses"
        case isRunner = "is_runner"
        case autoSubmit = "auto_submit"
        case pinned
        case worktreeId = "worktree_id"
        case sessionId = "session_id"
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
        systemPrompt = try c.decodeIfPresent(String.self, forKey: .systemPrompt)
        injection = (try c.decodeIfPresent(InjectionMode.self, forKey: .injection)) ?? .none
        preset = (try c.decodeIfPresent(String.self, forKey: .preset)) ?? ""
        presetId = try c.decodeIfPresent(String.self, forKey: .presetId)
        env = (try c.decodeIfPresent([EnvVar].self, forKey: .env)) ?? []
        useTmux = (try c.decodeIfPresent(Bool.self, forKey: .useTmux)) ?? false
        useWorktree = (try c.decodeIfPresent(Bool.self, forKey: .useWorktree)) ?? false
        tmuxSession = try c.decodeIfPresent(String.self, forKey: .tmuxSession)
        worktreePath = try c.decodeIfPresent(String.self, forKey: .worktreePath)
        worktreeBranch = try c.decodeIfPresent(String.self, forKey: .worktreeBranch)
        autoModePresses = (try c.decodeIfPresent(Int.self, forKey: .autoModePresses)) ?? 0
        isRunner = (try c.decodeIfPresent(Bool.self, forKey: .isRunner)) ?? false
        autoSubmit = (try c.decodeIfPresent(Bool.self, forKey: .autoSubmit)) ?? false
        pinned = (try c.decodeIfPresent(Bool.self, forKey: .pinned)) ?? false
        worktreeId = try c.decodeIfPresent(String.self, forKey: .worktreeId)
        sessionId = try c.decodeIfPresent(String.self, forKey: .sessionId)
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
        try c.encode(systemPrompt, forKey: .systemPrompt)
        try c.encode(injection, forKey: .injection)
        try c.encode(preset, forKey: .preset)
        try c.encode(presetId, forKey: .presetId)
        try c.encode(env, forKey: .env)
        try c.encode(useTmux, forKey: .useTmux)
        try c.encode(useWorktree, forKey: .useWorktree)
        try c.encode(tmuxSession, forKey: .tmuxSession)
        try c.encode(worktreePath, forKey: .worktreePath)
        try c.encode(worktreeBranch, forKey: .worktreeBranch)
        try c.encode(autoModePresses, forKey: .autoModePresses)
        try c.encode(isRunner, forKey: .isRunner)
        try c.encode(autoSubmit, forKey: .autoSubmit)
        try c.encode(pinned, forKey: .pinned)
        try c.encode(worktreeId, forKey: .worktreeId)
        try c.encode(sessionId, forKey: .sessionId)
        try c.encode(sessionStarted, forKey: .sessionStarted)
    }

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
        sessionStarted = false
        autoName = nil
    }
}
