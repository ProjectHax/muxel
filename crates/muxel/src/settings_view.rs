//! State for the in-app settings page.
//!
//! The page edits one preset / project at a time, reusing a fixed set of
//! [`InputState`] widgets that are repopulated on selection and read back on
//! Save. The rendering + handlers live as `MuxelApp` methods in `app.rs`.

use crate::i18n::t;
use gpui::*;
use gpui_component::input::InputState;
use muxel_core::{EnvVar, InjectionMode, PresetKind, SshAuth};
use uuid::Uuid;

/// Inline result of a "Test connection" in the SSH host editor.
#[derive(Clone)]
pub enum RemoteTestState {
    Idle,
    Testing,
    Ok(String),
    Failed(String),
}

/// Configurable actions: `(action, default keystroke, key context)`. The names
/// are matched in `app::keybinding_for` to the corresponding gpui actions; the
/// context (e.g. `Some("Terminal")`) scopes a binding to a focus region.
///
/// **Terminal vs app chords.** Plain `ctrl-<letter>` is claimed by agents as C0
/// (Claude Ctrl+S stash, shells Ctrl+R, …). `install_keybindings` auto-scopes
/// those to `!Terminal` unless the action is in [`KEEP_GLOBAL_WHILE_TERMINAL`].
/// Prefer `ctrl-shift-*` for muxel chrome.
pub const DEFAULT_KEYBINDINGS: &[(&str, &str, Option<&str>)] = &[
    ("NewPane", "ctrl-shift-t", None),
    // Keep global while a terminal is focused (see KEEP_GLOBAL_WHILE_TERMINAL).
    ("NewTab", "ctrl-t", None),
    ("TabNext", "ctrl-tab", None),
    ("TabPrev", "ctrl-shift-tab", None),
    ("SplitRight", "ctrl-shift-l", None),
    ("SplitDown", "ctrl-shift-j", None),
    ("ClosePane", "ctrl-shift-w", None),
    ("UndoClose", "ctrl-alt-z", None),
    ("FocusNext", "ctrl-shift-]", None),
    ("FocusPrev", "ctrl-shift-[", None),
    ("FocusLeft", "ctrl-alt-left", None),
    ("FocusRight", "ctrl-alt-right", None),
    ("FocusUp", "ctrl-alt-up", None),
    ("FocusDown", "ctrl-alt-down", None),
    ("ZoomIn", "ctrl-shift-up", None),
    ("ZoomOut", "ctrl-shift-down", None),
    ("ToggleSidebar", "ctrl-shift-b", None),
    ("ToggleDashboard", "ctrl-shift-e", None),
    ("ToggleSettings", "ctrl-shift-,", None),
    // Ctrl+P also opens it when no terminal is focused (see install_keybindings),
    // leaving Ctrl+P free for the terminal agent (e.g. opencode's commands).
    ("GlobalSearch", "ctrl-shift-p", None),
    ("FindInProject", "ctrl-shift-f", None),
    // Same key as FindInProject but scoped to a focused terminal (searches it).
    ("SearchTerminal", "ctrl-shift-f", Some("Terminal")),
    ("ClearTerminal", "ctrl-shift-k", None),
    ("FocusAttention", "ctrl-shift-a", None),
    ("ShowKeys", "ctrl-shift-/", None),
    ("ToggleBroadcast", "ctrl-shift-i", None),
    ("ToggleSpeechToText", "ctrl-shift-m", None),
    ("HoldSpeechToText", "ctrl-shift-h", None),
    // Read the focused agent's last reply aloud; again pauses, again resumes.
    ("ReadAloud", "ctrl-shift-r", None),
    ("ReadAloudRestart", "ctrl-alt-r", None),
    ("ReadAloudStop", "ctrl-alt-s", None),
    // Toggle the toolbar's "new agents get a git worktree" switch.
    ("ToggleWorktree", "ctrl-shift-g", None),
    // OS fullscreen; the sidebar hides until revealed or fullscreen exits.
    ("ToggleFullScreen", "f11", None),
    ("ToggleDevConsole", "f12", None),
    // Another muxel window, on the workspace selector.
    ("NewWindow", "ctrl-shift-n", None),
    // Editor save — auto !Terminal so Claude stash (Ctrl+S) reaches the PTY.
    ("SaveFile", "ctrl-s", None),
    ("SaveFileAs", "ctrl-shift-s", None),
    ("JumpToTab1", "alt-1", None),
    ("JumpToTab2", "alt-2", None),
    ("JumpToTab3", "alt-3", None),
    ("JumpToTab4", "alt-4", None),
    ("JumpToTab5", "alt-5", None),
    ("JumpToTab6", "alt-6", None),
    ("JumpToTab7", "alt-7", None),
    ("JumpToTab8", "alt-8", None),
    ("JumpToTab9", "alt-9", None),
    ("JumpToProject1", "ctrl-1", None),
    ("JumpToProject2", "ctrl-2", None),
    ("JumpToProject3", "ctrl-3", None),
    ("JumpToProject4", "ctrl-4", None),
    ("JumpToProject5", "ctrl-5", None),
    ("JumpToProject6", "ctrl-6", None),
    ("JumpToProject7", "ctrl-7", None),
    ("JumpToProject8", "ctrl-8", None),
    ("JumpToProject9", "ctrl-9", None),
    // New tab in the focused pane running the Nth agent preset (in the
    // preset-list order).
    ("NewAgent1", "ctrl-alt-1", None),
    ("NewAgent2", "ctrl-alt-2", None),
    ("NewAgent3", "ctrl-alt-3", None),
    ("NewAgent4", "ctrl-alt-4", None),
    ("NewAgent5", "ctrl-alt-5", None),
    ("NewAgent6", "ctrl-alt-6", None),
    ("NewAgent7", "ctrl-alt-7", None),
    ("NewAgent8", "ctrl-alt-8", None),
    ("NewAgent9", "ctrl-alt-9", None),
];

/// Actions whose plain `ctrl-<letter>` binding stays live while a terminal is
/// focused. Everything else with that shape is scoped to `!Terminal`.
pub const KEEP_GLOBAL_WHILE_TERMINAL: &[&str] = &[
    "NewTab", // ctrl-t — open a tab without leaving the agent
];

/// Which settings section is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSection {
    Appearance,
    Editor,
    Behavior,
    /// Alert sounds for agents that finish or get blocked.
    Sounds,
    Speech,
    ReadAloud,
    Agents,
    /// Outside control: letting another agent (Grok Bot, a script, …) drive muxel
    /// through `muxel ctl`.
    OutsideControl,
    Runners,
    Snippets,
    Loops,
    /// Team libraries: shared snippets, runners and loops from git.
    Libraries,
    Remotes,
    Identities,
    Projects,
    Keybindings,
}

/// Editable widgets + selection state for the settings page.
pub struct SettingsUi {
    pub section: SettingsSection,

    // Agent preset editor.
    pub selected_preset: Option<usize>,
    pub p_injection: InjectionMode,
    /// Terminal-agent vs browser preset (drives which editor fields show).
    pub p_kind: PresetKind,
    pub p_name: Entity<InputState>,
    /// Homepage for a browser preset.
    pub p_url: Entity<InputState>,
    pub p_program: Entity<InputState>,
    pub p_model: Entity<InputState>,
    pub p_model_flag: Entity<InputState>,
    pub p_effort: Entity<InputState>,
    pub p_effort_flag: Entity<InputState>,
    pub p_args: Entity<InputState>,
    pub p_prompt: Entity<InputState>,
    pub p_inj_flag: Entity<InputState>,
    pub p_env: Entity<InputState>,
    pub p_working_markers: Entity<InputState>,
    pub p_blocked_markers: Entity<InputState>,
    pub p_startup_delay: Entity<InputState>,

    // Runner editor.
    pub selected_runner: Option<usize>,
    pub r_preset_id: Option<Uuid>,
    pub r_presses: u8,
    pub r_name: Entity<InputState>,
    pub r_prompt: Entity<InputState>,

    // Snippet editor.
    pub selected_snippet: Option<usize>,
    pub sn_submit: bool,
    pub sn_name: Entity<InputState>,
    pub sn_text: Entity<InputState>,

    // Loop editor.
    pub selected_loop: Option<usize>,
    pub l_preset_id: Option<Uuid>,
    pub l_project_id: Option<Uuid>,
    pub l_presses: u8,
    /// Schedule kind: 0 = every N minutes, 1 = every N hours, 2 = daily at.
    pub l_sched_kind: u8,
    pub l_exit: bool,
    pub l_enabled: bool,
    pub l_name: Entity<InputState>,
    pub l_prompt: Entity<InputState>,
    pub l_interval: Entity<InputState>,
    pub l_hour: Entity<InputState>,
    pub l_minute: Entity<InputState>,

    // Team libraries; the list itself lives in `MuxelApp::library_hub`.
    /// The library whose details are shown; `None` = the add form.
    pub selected_library: Option<Uuid>,
    pub lib_url: Entity<InputState>,
    pub lib_branch: Entity<InputState>,
    pub lib_name: Entity<InputState>,
    /// Display name of the selected library.
    pub lib_rename: Entity<InputState>,
    /// Inline message of the last refused or rolled-back add.
    pub lib_add_error: Option<String>,

    // Outside control tab.
    /// Inline result of the last "Test" (running `muxel ctl` as the agent would).
    pub control_test: RemoteTestState,
    /// Whether the skill's full text is shown below its Copy button.
    pub control_skill_preview: bool,
    /// The skill has been copied to the clipboard (shown beside the button).
    pub control_skill_copied: bool,

    // SSH remote-host editor.
    pub selected_remote: Option<usize>,
    pub s_auth: SshAuth,
    /// Inline result of the last "Test connection".
    pub s_test: RemoteTestState,
    /// Cached "a password is stored in the keychain" flag for the open host
    /// (refreshed on open/save, so render doesn't hit the keychain every frame).
    pub s_has_password: bool,
    pub s_forward_agent: bool,
    pub s_compression: bool,
    pub s_use_tmux: bool,
    /// Which OS the selected host runs — decides the whole remote command
    /// vocabulary, and whether the tmux row is offered at all.
    pub s_remote_os: muxel_core::RemoteOs,
    /// Shell an interactive pane runs on a Windows host.
    pub s_windows_shell: muxel_core::winshell::WindowsShell,
    pub s_name: Entity<InputState>,
    pub s_host: Entity<InputState>,
    pub s_port: Entity<InputState>,
    pub s_user: Entity<InputState>,
    pub s_identity: Entity<InputState>,
    pub s_password: Entity<InputState>,
    pub s_jump: Entity<InputState>,
    pub s_keepalive: Entity<InputState>,
    pub s_strict: Entity<InputState>,
    pub s_extra: Entity<InputState>,
    /// Shared login identity chosen for the open host (None = inline credentials).
    pub s_identity_id: Option<Uuid>,

    // Shared login-identity editor.
    pub selected_identity: Option<usize>,
    pub id_auth: SshAuth,
    pub id_has_password: bool,
    pub id_name: Entity<InputState>,
    pub id_user: Entity<InputState>,
    pub id_identity: Entity<InputState>,
    pub id_password: Entity<InputState>,

    // Project editor.
    pub selected_project: Option<Uuid>,
    pub proj_name: Entity<InputState>,

    // Appearance.
    pub font_family: Entity<InputState>,

    // Behavior.
    /// What a pane's Auto toggle types to resume a stalled agent.
    pub auto_continue_message: Entity<InputState>,

    // Editor.
    pub editor_font_family: Entity<InputState>,

    // Speech-to-text.
    pub stt_provider_url: Entity<InputState>,
    pub stt_provider_model: Entity<InputState>,
    pub stt_language: Entity<InputState>,
    pub stt_api_key: Entity<InputState>,
    /// The spoken phrase that triggers the wake command.
    pub stt_wake_phrase: Entity<InputState>,
    /// Whether a provider API key is stored (cached so render doesn't hit the
    /// keychain every frame).
    pub stt_has_key: bool,

    // Read aloud (text-to-speech).
    /// OS voice name, typed — for systems muxel can't list voices on (Linux).
    pub tts_system_voice: Entity<InputState>,
    pub tts_provider_voice: Entity<InputState>,
    pub tts_provider_model: Entity<InputState>,
    /// The OS voices, `(name, locale)`, listed once in the background the first
    /// time Settings opens (`None` until then).
    pub tts_system_voices: Option<Vec<(String, String)>>,

    // Keybindings.
    /// Narrows the shortcut list by action name or key.
    pub kb_filter: Entity<InputState>,
    /// The row waiting for its new shortcut to be pressed: an action name, or
    /// the shared prefix of a folded 1–9 range.
    pub kb_recording: Option<String>,
    /// Comma/space-separated chords passed through to a focused terminal.
    pub passthrough_keys: Entity<InputState>,

    // The window itself.
    /// Searches every setting; non-empty replaces the section with results.
    pub search: Entity<InputState>,
    /// The theme picker shows every theme rather than the first few.
    pub themes_expanded: bool,
    /// One swatch per theme, resolved once when Settings opens.
    pub theme_swatches: Vec<ThemeSwatch>,
    /// The agent editor's Advanced group is open.
    pub preset_advanced: bool,
}

/// A theme as the Appearance picker draws it.
#[derive(Clone)]
pub struct ThemeSwatch {
    pub name: SharedString,
    pub background: Hsla,
    pub border: Hsla,
    /// Foreground, then three of the theme's accents: the bars of the swatch.
    pub bars: [Hsla; 4],
}

/// An icon beside a section in the Settings list.
#[derive(Clone)]
pub enum SectionIcon {
    Named(gpui_component::IconName),
    /// An SVG from muxel's own assets.
    Path(&'static str),
}

impl SettingsSection {
    /// The sections in the order and groups the Settings list shows them.
    pub fn groups() -> [(SharedString, &'static [SettingsSection]); 5] {
        use SettingsSection::*;
        [
            (
                t("Application"),
                &[Appearance, Behavior, Editor, Keybindings],
            ),
            (t("Alerts and voice"), &[Sounds, Speech, ReadAloud]),
            (
                t("Agents and automation"),
                &[Agents, Runners, Snippets, Loops, Libraries, OutsideControl],
            ),
            (t("Remote"), &[Remotes, Identities]),
            (t("Workspace"), &[Projects]),
        ]
    }

    /// Every section, in list order.
    pub fn all() -> Vec<SettingsSection> {
        Self::groups()
            .iter()
            .flat_map(|(_, sections)| sections.iter().copied())
            .collect()
    }

    pub fn title(self) -> SharedString {
        match self {
            Self::Appearance => t("Appearance"),
            Self::Editor => t("Editor"),
            Self::Behavior => t("General"),
            Self::Sounds => t("Notifications"),
            Self::Speech => t("Dictation"),
            Self::ReadAloud => t("Read aloud"),
            Self::Agents => t("Agents"),
            Self::OutsideControl => t("Outside control"),
            Self::Runners => t("Runners"),
            Self::Snippets => t("Snippets"),
            Self::Loops => t("Loops"),
            Self::Libraries => t("Libraries"),
            Self::Remotes => t("Hosts"),
            Self::Identities => t("Identities"),
            Self::Projects => t("Projects"),
            Self::Keybindings => t("Keybindings"),
        }
    }

    /// One line under the section's title.
    pub fn description(self) -> SharedString {
        match self {
            Self::Appearance => t("Theme, text size and pane chrome."),
            Self::Editor => t("The code editor and git-diff panes."),
            Self::Behavior => t("How new agents, panes and the terminal behave."),
            Self::Sounds => t("Desktop notifications and the chimes agents ring."),
            Self::Speech => t("Speak instead of typing into the focused pane."),
            Self::ReadAloud => t("Hear an agent's reply instead of reading it."),
            Self::Agents => t("Presets for the agents, shells and browsers you launch into panes."),
            Self::OutsideControl => t("Let another agent or a script drive muxel."),
            Self::Runners => {
                t("One-click tasks: an agent, a prompt and the details you add at run time.")
            }
            Self::Snippets => t("Text you send to the focused pane with one click."),
            Self::Loops => t("Prompts an agent runs on a schedule."),
            Self::Libraries => t("Snippets, runners and loops your team shares from git."),
            Self::Remotes => t("SSH hosts for remote projects."),
            Self::Identities => t("Logins that several hosts can share."),
            Self::Projects => t("Defaults for new projects, and each project's own settings."),
            Self::Keybindings => t("Click a shortcut, then press the keys you want instead."),
        }
    }

    pub fn icon(self) -> SectionIcon {
        use gpui_component::IconName;
        match self {
            Self::Appearance => SectionIcon::Named(IconName::Palette),
            Self::Editor => SectionIcon::Path("icons/code.svg"),
            Self::Behavior => SectionIcon::Named(IconName::Settings2),
            Self::Sounds => SectionIcon::Named(IconName::Bell),
            Self::Speech => SectionIcon::Path("icons/mic.svg"),
            Self::ReadAloud => SectionIcon::Path("icons/volume-2.svg"),
            Self::Agents => SectionIcon::Named(IconName::Bot),
            Self::OutsideControl => SectionIcon::Path("icons/plug.svg"),
            Self::Runners => SectionIcon::Named(IconName::Play),
            Self::Snippets => SectionIcon::Named(IconName::SquareTerminal),
            Self::Loops => SectionIcon::Path("icons/refresh.svg"),
            Self::Libraries => SectionIcon::Named(IconName::BookOpen),
            Self::Remotes => SectionIcon::Named(IconName::Network),
            Self::Identities => SectionIcon::Named(IconName::CircleUser),
            Self::Projects => SectionIcon::Named(IconName::Folder),
            Self::Keybindings => SectionIcon::Path("icons/keyboard.svg"),
        }
    }
}

impl SettingsUi {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            section: SettingsSection::Appearance,
            selected_preset: None,
            p_injection: InjectionMode::None,
            p_kind: PresetKind::Terminal,
            p_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            p_url: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("https://duckduckgo.com"))),
            p_program: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("Program (blank = shell)"))),
            p_model: cx.new(|cx| InputState::new(window, cx).placeholder(t("Model (optional)"))),
            p_model_flag: cx.new(|cx| InputState::new(window, cx).placeholder(t("--model"))),
            p_effort: cx.new(|cx| InputState::new(window, cx).placeholder(t("Effort (optional)"))),
            p_effort_flag: cx.new(|cx| InputState::new(window, cx).placeholder(t("Effort flag"))),
            p_args: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("Extra args (quotes group words)"))
            }),
            p_prompt: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("System prompt"))
            }),
            p_inj_flag: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("--append-system-prompt"))),
            p_env: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("KEY=VALUE per line"))
            }),
            p_working_markers: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("comma-separated; blank = default"))
            }),
            p_blocked_markers: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("comma-separated; blank = default"))
            }),
            p_startup_delay: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("0 = auto (wait for quiet)"))),
            selected_runner: None,
            r_preset_id: None,
            r_presses: 3,
            r_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            r_prompt: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("Task prompt — use {{input}} for the run-time details"))
            }),
            selected_snippet: None,
            sn_submit: false,
            sn_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            sn_text: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("Text typed into the pane"))
            }),
            selected_loop: None,
            l_preset_id: None,
            l_project_id: None,
            l_presses: 0,
            l_sched_kind: 1,
            l_exit: false,
            l_enabled: true,
            l_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            l_prompt: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("Prompt to run each time"))
            }),
            l_interval: cx.new(|cx| InputState::new(window, cx).placeholder("1")),
            l_hour: cx.new(|cx| InputState::new(window, cx).placeholder("9")),
            l_minute: cx.new(|cx| InputState::new(window, cx).placeholder("00")),
            selected_library: None,
            lib_url: cx.new(|cx| {
                InputState::new(window, cx).placeholder("https://github.com/acme/team-lib.git")
            }),
            lib_branch: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("Branch (blank = default)"))),
            lib_name: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("Display name (blank = from the URL)"))
            }),
            lib_rename: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("Display name (blank = from the URL)"))
            }),
            lib_add_error: None,
            control_test: RemoteTestState::Idle,
            control_skill_preview: false,
            control_skill_copied: false,
            selected_remote: None,
            s_auth: SshAuth::Agent,
            s_test: RemoteTestState::Idle,
            s_has_password: false,
            s_forward_agent: false,
            s_compression: false,
            s_use_tmux: true,
            s_remote_os: muxel_core::RemoteOs::default(),
            s_windows_shell: muxel_core::winshell::WindowsShell::default(),
            s_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            s_host: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(t("host.example.com or ~/.ssh/config alias"))
            }),
            s_port: cx.new(|cx| InputState::new(window, cx).placeholder("22")),
            s_user: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("login user (optional)"))),
            s_identity: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("~/.ssh/id_ed25519"))),
            s_password: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(t("stored in the OS keychain"))
            }),
            s_jump: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("ProxyJump host (optional)"))),
            s_keepalive: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("ServerAliveInterval secs"))),
            s_strict: cx.new(|cx| InputState::new(window, cx).placeholder(t("accept-new"))),
            s_extra: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("extra -o options, one KEY=VALUE per line"))
            }),
            s_identity_id: None,
            selected_identity: None,
            id_auth: SshAuth::Agent,
            id_has_password: false,
            id_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            id_user: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("login user (optional)"))),
            id_identity: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("~/.ssh/id_ed25519"))),
            id_password: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(t("stored in the OS keychain"))
            }),
            selected_project: None,
            proj_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Project name"))),
            font_family: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("DejaVu Sans Mono"))),
            auto_continue_message: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(muxel_core::autopilot::AUTO_CONTINUE_MESSAGE.to_string())
            }),
            editor_font_family: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("theme monospace"))),
            stt_provider_url: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("https://api.openai.com/v1"))),
            stt_provider_model: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("whisper-1"))),
            stt_language: cx.new(|cx| InputState::new(window, cx).placeholder(t("auto-detect"))),
            stt_api_key: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(t("stored in the OS keychain"))
            }),
            stt_wake_phrase: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(muxel_core::stt::DEFAULT_WAKE_PHRASE.to_string())
            }),
            stt_has_key: false,
            tts_system_voice: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("OS default voice"))),
            tts_provider_voice: cx.new(|cx| {
                InputState::new(window, cx).placeholder(muxel_core::tts::DEFAULT_TTS_VOICE)
            }),
            tts_provider_model: cx.new(|cx| {
                InputState::new(window, cx).placeholder(muxel_core::tts::DEFAULT_TTS_PROVIDER_MODEL)
            }),
            tts_system_voices: None,
            kb_filter: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("Filter by action or key"))),
            kb_recording: None,
            passthrough_keys: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("ctrl-p, ctrl-t"))),
            search: cx.new(|cx| InputState::new(window, cx).placeholder(t("Search settings"))),
            themes_expanded: false,
            theme_swatches: Vec::new(),
            preset_advanced: false,
        }
    }
}

impl SettingsUi {
    /// Take over what `old` was showing. A text field belongs to the window it
    /// was made in, so moving Settings between the main window and a window of
    /// its own means new fields; which section and items were open carries
    /// over, and the editors are refilled from what was last saved.
    pub fn adopt(&mut self, old: SettingsUi) {
        self.section = old.section;
        self.selected_preset = old.selected_preset;
        self.selected_runner = old.selected_runner;
        self.selected_snippet = old.selected_snippet;
        self.selected_loop = old.selected_loop;
        self.selected_library = old.selected_library;
        self.selected_remote = old.selected_remote;
        self.selected_identity = old.selected_identity;
        self.selected_project = old.selected_project;
        self.lib_add_error = old.lib_add_error;
        self.control_test = old.control_test;
        self.control_skill_preview = old.control_skill_preview;
        self.control_skill_copied = old.control_skill_copied;
        self.stt_has_key = old.stt_has_key;
        self.tts_system_voices = old.tts_system_voices;
        self.themes_expanded = old.themes_expanded;
        self.theme_swatches = old.theme_swatches;
        self.preset_advanced = old.preset_advanced;
    }
}

/// Parse a `KEY=VALUE`-per-line block into env vars (blank lines ignored).
pub fn parse_env(text: &str) -> Vec<EnvVar> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            Some(EnvVar {
                key: k.trim().to_string(),
                value: v.trim().to_string(),
            })
        })
        .collect()
}

/// Render env vars back to a `KEY=VALUE`-per-line block for editing.
pub fn format_env(env: &[EnvVar]) -> String {
    env.iter()
        .map(|e| format!("{}={}", e.key, e.value))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split an extra-args string into shell words — quote-aware
/// ([`muxel_core::split_words`], shared with the iOS companion), so
/// `--append-system-prompt "be terse"` stays one argument. Unbalanced quotes
/// degrade to a plain whitespace split (a preset must never fail to save);
/// `save_preset` warns via the feed in that case.
pub fn parse_args(text: &str) -> Vec<String> {
    muxel_core::split_words(text)
        .unwrap_or_else(|| text.split_whitespace().map(|s| s.to_string()).collect())
}

/// Split a status-marker string on commas (markers contain spaces), trimming
/// each and dropping blanks.
pub fn parse_markers(text: &str) -> Vec<String> {
    text.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}
