//! State for the in-app settings page.
//!
//! The page edits one preset / project at a time, reusing a fixed set of
//! [`InputState`] widgets that are repopulated on selection and read back on
//! Save. The rendering + handlers live as `MuxelApp` methods in `app.rs`.

use crate::i18n::t;
use gpui::*;
use gpui_component::input::InputState;
use muxel_core::{EnvVar, InjectionMode, SshAuth};
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
pub const DEFAULT_KEYBINDINGS: &[(&str, &str, Option<&str>)] = &[
    ("NewPane", "ctrl-shift-t", None),
    ("NewTab", "ctrl-t", None),
    ("TabNext", "ctrl-tab", None),
    ("TabPrev", "ctrl-shift-tab", None),
    ("SplitRight", "ctrl-shift-l", None),
    ("SplitDown", "ctrl-shift-j", None),
    ("ClosePane", "ctrl-shift-w", None),
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
    ("ToggleDevConsole", "f12", None),
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
];

/// Which settings section is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Appearance,
    Editor,
    Behavior,
    Agents,
    Runners,
    Loops,
    Remotes,
    Projects,
    Keybindings,
}

/// Editable widgets + selection state for the settings page.
pub struct SettingsUi {
    pub section: SettingsSection,

    // Agent preset editor.
    pub selected_preset: Option<usize>,
    pub p_injection: InjectionMode,
    pub p_name: Entity<InputState>,
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

    // SSH remote-host editor.
    pub selected_remote: Option<usize>,
    pub s_auth: SshAuth,
    /// Inline result of the last "Test connection".
    pub s_test: RemoteTestState,
    /// Cached "a password is stored in the keychain" flag for the open host
    /// (refreshed on open/save, so render doesn't hit the keychain every frame).
    pub s_has_password: bool,
    pub s_forward_agent: bool,
    pub s_use_tmux: bool,
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

    // Project editor.
    pub selected_project: Option<Uuid>,
    pub proj_name: Entity<InputState>,

    // Appearance.
    pub font_family: Entity<InputState>,

    // Editor.
    pub editor_font_family: Entity<InputState>,

    // Keybindings (action name -> keystroke input).
    pub keybinds: Vec<(String, Entity<InputState>)>,
    /// Comma/space-separated chords passed through to a focused terminal.
    pub passthrough_keys: Entity<InputState>,
}

impl SettingsUi {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            section: SettingsSection::Appearance,
            selected_preset: None,
            p_injection: InjectionMode::None,
            p_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            p_program: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("Program (blank = shell)"))),
            p_model: cx.new(|cx| InputState::new(window, cx).placeholder(t("Model (optional)"))),
            p_model_flag: cx.new(|cx| InputState::new(window, cx).placeholder(t("--model"))),
            p_effort: cx.new(|cx| InputState::new(window, cx).placeholder(t("Effort (optional)"))),
            p_effort_flag: cx.new(|cx| InputState::new(window, cx).placeholder(t("Effort flag"))),
            p_args: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("Extra args (space-separated)"))
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
            selected_remote: None,
            s_auth: SshAuth::Agent,
            s_test: RemoteTestState::Idle,
            s_has_password: false,
            s_forward_agent: false,
            s_use_tmux: true,
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
            selected_project: None,
            proj_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Project name"))),
            font_family: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("DejaVu Sans Mono"))),
            editor_font_family: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("theme monospace"))),
            keybinds: DEFAULT_KEYBINDINGS
                .iter()
                .map(|(name, default, _ctx)| {
                    (
                        name.to_string(),
                        cx.new(|cx| InputState::new(window, cx).default_value(*default)),
                    )
                })
                .collect(),
            passthrough_keys: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("ctrl-p, ctrl-t"))),
        }
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

/// Split an extra-args string on whitespace (simple; no quote handling).
pub fn parse_args(text: &str) -> Vec<String> {
    text.split_whitespace().map(|s| s.to_string()).collect()
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
