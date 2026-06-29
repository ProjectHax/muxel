import Foundation

/// An environment variable for an agent process. Port of `EnvVar` (agent.rs).
struct EnvVar: Codable, Equatable {
    var key: String
    var value: String
}

/// What a pane runs. Port of `InstanceKind` — serialized as the bare variant name.
/// The iOS app only attaches to `terminal` instances (editor/diff are desktop-only).
enum InstanceKind: String, Codable, Equatable {
    case terminal = "Terminal"
    case editor = "Editor"
    case diff = "Diff"
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
    var customName: String?
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
        case customName = "custom_name"
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

    /// Build a fresh terminal instance the iOS app is about to launch.
    init(id: String, projectId: String, title: String, program: String?, args: [String]) {
        self.id = id
        self.projectId = projectId
        self.title = title
        self.program = program
        self.args = args
        self.sessionStarted = true
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        projectId = try c.decode(String.self, forKey: .projectId)
        title = try c.decode(String.self, forKey: .title)
        kind = (try c.decodeIfPresent(InstanceKind.self, forKey: .kind)) ?? .terminal
        editorPath = try c.decodeIfPresent(String.self, forKey: .editorPath)
        customName = try c.decodeIfPresent(String.self, forKey: .customName)
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
        try c.encode(customName, forKey: .customName)
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

    /// The display label shown in the tab bar (custom name overrides the title).
    var displayName: String {
        if let customName, !customName.isEmpty { return customName }
        return title
    }
}
