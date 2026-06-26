//! The muxel application shell: a sidebar of projects (with per-agent status),
//! a toolbar with a preset selector + split/close/restart, and a main area that
//! renders the active project's pane tree. Live terminals are kept in
//! `terminals`, keyed by instance id.

use crate::editor::{EditorConfig, EditorView};
use crate::i18n::{t, tf, tn};
use crate::integrations;
use crate::settings_view::{self, RemoteTestState, SettingsSection, SettingsUi};
use crate::theme;
use gpui::*;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Position};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_component::scroll::{Scrollbar, ScrollbarAxis};
use gpui_component::tag::Tag;
use gpui_component::{button::*, *};
use muxel_core::{
    AgentPreset, FocusDir, InjectionMode, Instance, InstanceKind, Loop, LoopSchedule, MEMORY_DIR,
    MEMORY_FILE, PaneNode, PostRunAction, Project, RemoteHost, RemoteLayout, RemoteRef,
    ResolvedLaunch, Runner, SplitDirection, SshAuth, StartupAgent, Workspace, WorkspaceMeta,
    WorkspacesIndex, Worktree, add_tab, add_tab_at, focus_in_direction, memory_instruction,
    migrate_worktrees, move_into_split, move_into_tabs, move_pane_beside, move_tab_to, remove,
    resolve_launch, set_active_tab, set_split_sizes, set_tab_order, split, split_beside,
    swap_instances, swap_panes,
};
use muxel_terminal::{AgentStatus, CommandSpec, TerminalMouseMode, TerminalSession, TerminalView};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Minimum width a horizontal split's pane can shrink to (~40 cols), so agent
/// TUIs (Claude/opencode/…) don't get squished narrow enough to overflow.
const MIN_PANE_WIDTH: Pixels = px(340.0);
/// Minimum height a vertical split's pane can shrink to (a few rows).
const MIN_PANE_HEIGHT: Pixels = px(120.0);

/// Status indicator color, taken from the active theme.
fn status_hsla(status: AgentStatus, cx: &App) -> Hsla {
    let t = cx.theme();
    match status {
        AgentStatus::Working => t.primary,
        AgentStatus::Idle => t.muted_foreground,
        AgentStatus::Blocked => t.warning,
        AgentStatus::Done => t.success,
    }
}

/// Asset path for an agent's icon, chosen from its program name.
fn agent_icon_path(program: Option<&str>) -> SharedString {
    let base = program.map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p));
    match base {
        Some(p) if p.contains("claude") => "icons/agent-claude.svg",
        Some(p) if p.contains("opencode") => "icons/agent-opencode.svg",
        Some(p) if p == "amp" || p.contains("ampcode") => "icons/agent-ampcode.svg",
        Some(p) if p.contains("grok") => "icons/agent-grok.svg",
        Some(p) if p.contains("hermes") => "icons/agent-hermes.svg",
        Some(p) if p.contains("ollama") => "icons/agent-ollama.svg",
        Some(p) if p == "pi" || p.contains("pi.dev") || p.contains("pidev") => "icons/agent-pi.svg",
        Some(_) => "icons/agent-generic.svg",
        None => "icons/agent-shell.svg",
    }
    .into()
}

/// Whether `program` resolves to an executable: a path is checked directly,
/// otherwise each `PATH` entry is searched (so the agent picker can hide agents
/// whose binary isn't installed).
fn program_on_path(program: &str) -> bool {
    fn is_exec(p: &std::path::Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            p.is_file()
        }
    }
    // Does `p` (or, on Windows, `p` + a PATHEXT suffix) resolve to an executable?
    // A bare program name like "claude" is installed as claude.exe / claude.cmd on
    // Windows, so the extension-less join would otherwise never match.
    fn exec_with_ext(p: &std::path::Path) -> bool {
        if is_exec(p) {
            return true;
        }
        #[cfg(windows)]
        {
            let exts =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            exts.split(';').filter(|e| !e.is_empty()).any(|ext| {
                let mut cand = p.as_os_str().to_owned();
                cand.push(ext);
                is_exec(std::path::Path::new(&cand))
            })
        }
        #[cfg(not(windows))]
        false
    }
    if program.contains('/') || program.contains('\\') {
        return exec_with_ext(std::path::Path::new(program));
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| exec_with_ext(&dir.join(program)))
    })
}

/// The distinct preset programs that are currently installed (for filtering the
/// new-agent menus). A preset with no program is a shell — always available.
fn installed_programs(presets: &[AgentPreset]) -> HashSet<String> {
    presets
        .iter()
        .filter_map(|p| p.program.clone())
        .filter(|prog| program_on_path(prog))
        .collect()
}

/// Built-in status markers (working spinner / blocked prompt) for a known agent,
/// keyed by program basename. The status badge scans the screen for these; a
/// preset's own markers override them, and an empty result falls back to the
/// bell/output-activity heuristic. (See also `agent_icon_path`.)
fn default_markers(program: Option<&str>) -> (Vec<String>, Vec<String>) {
    let base = program.map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p));
    let to = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match base {
        Some(p) if p.contains("claude") => (
            to(&["esc to interrupt"]),
            to(&["❯ 1.", "Do you want to proceed"]),
        ),
        Some(p) if p.contains("opencode") => (to(&["esc interrupt"]), to(&["Permission required"])),
        // hermes / ollama / pi: no recognized TUI markers — heuristic only
        // (users can add markers per preset in the agent editor).
        _ => (Vec::new(), Vec::new()),
    }
}

/// An agent's icon (per-program SVG), tinted `color` and sized `size`.
fn agent_icon(program: Option<&str>, size: Pixels, color: Hsla) -> Svg {
    svg()
        .path(agent_icon_path(program))
        .size(size)
        .flex_none()
        .text_color(color)
}

/// An agent's icon as a gpui-component [`Icon`], for use on `Button`s and menu
/// items (which take `impl Into<Icon>`).
fn agent_icon_obj(program: Option<&str>) -> Icon {
    Icon::empty().path(agent_icon_path(program))
}

/// A small status pill for an agent.
fn status_tag(status: AgentStatus) -> Tag {
    match status {
        AgentStatus::Working => Tag::primary().small().child(t("working")),
        AgentStatus::Idle => Tag::new().small().child(t("idle")),
        AgentStatus::Blocked => Tag::warning().small().child(t("blocked")),
        AgentStatus::Done => Tag::success().small().child(t("done")),
    }
}

/// Fixed worktree color palette (Tailwind 400s). Deliberately avoids blue/green/
/// red, which collide with the theme's primary/success/danger. `Worktree.color`
/// indexes into this round-robin.
const WORKTREE_PALETTE: [fn() -> Hsla; 10] = [
    violet_400,
    teal_400,
    rose_400,
    amber_400,
    cyan_400,
    indigo_400,
    emerald_400,
    pink_400,
    orange_400,
    lime_400,
];

fn worktree_color(idx: u8) -> Hsla {
    WORKTREE_PALETTE[idx as usize % WORKTREE_PALETTE.len()]()
}

fn status_label(status: Option<AgentStatus>) -> &'static str {
    match status {
        Some(AgentStatus::Working) => "working",
        Some(AgentStatus::Idle) => "idle",
        Some(AgentStatus::Blocked) => "blocked",
        Some(AgentStatus::Done) => "done",
        None => "not started",
    }
}

/// Detail line(s) for a git-op success notification: the first couple of
/// non-empty lines of git's output (e.g. `[main a1b2c3] message`, `main -> main`,
/// `Already up to date.`), trimmed.
fn git_notify_detail(out: &str) -> String {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(2)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Display title for a shell pane. A shell's OSC title is usually
/// `user@host:dir`; strip the `user@host:` so only the directory shows. Titles
/// without that shape (no `@` before the first `:`) are returned unchanged.
fn shell_dir_title(osc: &str) -> &str {
    match osc.split_once(':') {
        Some((prefix, path)) if prefix.contains('@') && !path.is_empty() => path,
        _ => osc,
    }
}

/// Render a terminal pane. In `RightClickMenu` mouse mode it wraps the view in a
/// right-click Copy/Paste context menu (the menu component lives in this crate, not
/// in muxel-terminal); the other modes handle the mouse inside the terminal element
/// itself. Shared by the main pane and pop-out windows.
fn terminal_pane_element(view: &Entity<TerminalView>, cx: &App) -> AnyElement {
    if view.read(cx).mouse_mode() != TerminalMouseMode::RightClickMenu {
        return view.clone().into_any_element();
    }
    let view = view.clone();
    div()
        .size_full()
        .child(view.clone())
        .context_menu(move |menu, window, _cx| {
            menu.item(PopupMenuItem::new(t("Copy")).icon(IconName::Copy).on_click(
                window.listener_for(&view, |this, _e, _w, cx| {
                    if let Some(text) = this
                        .session()
                        .selection_to_string()
                        .filter(|t| !t.is_empty())
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }),
            ))
            .item(PopupMenuItem::new(t("Paste")).on_click(window.listener_for(
                &view,
                |this, _e, _w, cx| {
                    if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                        this.session().paste(&text);
                    }
                },
            )))
        })
        .into_any_element()
}

/// Instance a clicked desktop notification wants muxel to jump to. Set from the
/// notification's D-Bus action thread (off the UI thread); drained on the UI
/// thread by `handle_notification_click` each tick.
static PENDING_NOTIFICATION_FOCUS: std::sync::Mutex<Option<Uuid>> = std::sync::Mutex::new(None);

/// Fire a desktop notification off the UI thread (best-effort). When `focus` is
/// set, clicking the notification raises muxel and switches to that instance's
/// project + pane.
fn notify(summary: String, body: String, focus: Option<Uuid>) {
    std::thread::spawn(move || {
        let mut builder = notify_rust::Notification::new();
        builder
            .appname("muxel")
            .icon("muxel")
            .summary(&summary)
            .body(&body)
            // Tie the notification to muxel's window: the desktop-entry hint names
            // the .desktop ("muxel"), whose StartupWMClass matches the window's
            // app_id ("muxel"). GNOME Shell then raises the existing window itself
            // when the notification is clicked — with its own privilege, so it
            // doesn't trip focus-stealing prevention (and the "muxel is ready"
            // hand-off notification) the way an app self-raise does.
            .hint(notify_rust::Hint::DesktopEntry("muxel".to_string()))
            .timeout(notify_rust::Timeout::Milliseconds(10_000));
        // "default" is the action GNOME invokes when the notification *body* is
        // clicked; only register it when there's a pane to jump to.
        if focus.is_some() {
            builder.action("default", &t("Open"));
        }
        if let Ok(handle) = builder.show() {
            // Blocks running the D-Bus loop until the notification is clicked or
            // closed — which also keeps the sending connection alive (GNOME
            // withdraws the popup when it closes), replacing the old keep-alive
            // sleep. On a body click we record the target for the UI thread.
            handle.wait_for_action(|action| {
                if action == "default"
                    && let Some(iid) = focus
                {
                    *PENDING_NOTIFICATION_FOCUS.lock().unwrap() = Some(iid);
                }
            });
        }
    });
}

/// Global handle so menu-dispatched actions (which run outside the view's
/// dispatch tree) can reach the running app.
struct MuxelHandle(WeakEntity<MuxelApp>);
impl Global for MuxelHandle {}

/// Set the active preset (by id) for new panes.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = muxel, no_json)]
struct SetPreset(Uuid);

/// Set the active project's default preset (by id).
#[derive(Action, Clone, PartialEq)]
#[action(namespace = muxel, no_json)]
struct SetDefaultPreset(Uuid);

/// Register global action handlers that route to the running app. Called once
/// at startup (the app installs [`MuxelHandle`] when it is created).
pub fn register_actions(cx: &mut App) {
    cx.on_action(|a: &SetPreset, cx| {
        let Some(weak) = cx.try_global::<MuxelHandle>().map(|h| h.0.clone()) else {
            return;
        };
        if let Some(app) = weak.upgrade() {
            app.update(cx, |this, cx| this.set_preset_by_id(a.0, cx));
        }
    });
    cx.on_action(|a: &SetDefaultPreset, cx| {
        let Some(weak) = cx.try_global::<MuxelHandle>().map(|h| h.0.clone()) else {
            return;
        };
        if let Some(app) = weak.upgrade() {
            app.update(cx, |this, cx| this.set_default_preset(a.0, cx));
        }
    });
    // Theme picks come from the settings dropdown (an overlay menu → global
    // action). Route to the app so it records + persists the choice itself.
    cx.on_action(|a: &crate::theme::SwitchTheme, cx| {
        let Some(weak) = cx.try_global::<MuxelHandle>().map(|h| h.0.clone()) else {
            return;
        };
        if let Some(app) = weak.upgrade() {
            app.update(cx, |this, cx| this.set_theme(a.0.clone(), cx));
        }
    });
    // Language picks come from the settings dropdown (overlay menu → global
    // action), same routing as the theme picker.
    cx.on_action(|a: &crate::i18n::SetLanguage, cx| {
        let Some(weak) = cx.try_global::<MuxelHandle>().map(|h| h.0.clone()) else {
            return;
        };
        if let Some(app) = weak.upgrade() {
            app.update(cx, |this, cx| this.set_language(a.0.clone(), cx));
        }
    });
    // Cmd+Q (macOS) / Ctrl+Q: route to the same confirm flow as the title-bar
    // close button. Registered globally (not on the main view) so it also fires
    // on the first-run screens, whose render path omits the main element.
    cx.on_action(|_: &Quit, cx| {
        let Some(weak) = cx.try_global::<MuxelHandle>().map(|h| h.0.clone()) else {
            return;
        };
        if let Some(app) = weak.upgrade() {
            app.update(cx, |this, cx| {
                // Quit outright when there's nothing to confirm: the first-run
                // screens (nothing running yet), or a second Cmd+Q while the
                // confirm modal is already up — same as clicking its Quit button.
                if this.show_terms || this.show_workspace_selector || this.show_quit_confirm {
                    this.confirm_quit = true;
                    cx.quit();
                } else {
                    this.show_quit_confirm = true;
                    cx.notify();
                }
            });
        }
    });
}

// Keyboard-driven actions, handled by the root view (so they have `&mut Window`)
// and bound in [`install_keybindings`].
actions!(
    muxel,
    [
        // Cmd+Q (macOS) / Ctrl+Q (elsewhere): ask to quit. Bound globally in
        // [`install_keybindings`]; not in the rebindable table since it mirrors
        // the platform-standard quit shortcut.
        Quit,
        NewPane,
        NewTab,
        TabNext,
        TabPrev,
        SplitRight,
        SplitDown,
        ClosePane,
        FocusNext,
        FocusPrev,
        ZoomIn,
        ZoomOut,
        ToggleSidebar,
        ToggleDashboard,
        ToggleSettings,
        GlobalSearch,
        FindInProject,
        SaveFile,
        SaveFileAs,
        // Clear the active terminal's scrollback.
        ClearTerminal,
        // Focus the next agent that needs attention (blocked, then done).
        FocusAttention,
        // Spatial pane focus: move to the pane in a direction.
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        // Show the keyboard-shortcut cheat sheet.
        ShowKeys,
        // Toggle the broadcast bar (send one line to every agent in the project).
        ToggleBroadcast,
        // Search the active terminal's scrollback (Terminal context only).
        SearchTerminal,
        // Tab / Shift+Tab while a terminal is focused: send to the PTY instead
        // of letting gpui-component's Root move keyboard focus.
        SendTab,
        SendBackTab,
    ]
);

/// Select the Nth tab (1-based) of the active pane. Bound to Alt+1..9.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = muxel, no_json)]
struct JumpToTab(usize);

fn keybinding_for(action: &str, keystroke: &str, context: Option<&str>) -> Option<KeyBinding> {
    Some(match action {
        "NewPane" => KeyBinding::new(keystroke, NewPane, context),
        "NewTab" => KeyBinding::new(keystroke, NewTab, context),
        "TabNext" => KeyBinding::new(keystroke, TabNext, context),
        "TabPrev" => KeyBinding::new(keystroke, TabPrev, context),
        "SplitRight" => KeyBinding::new(keystroke, SplitRight, context),
        "SplitDown" => KeyBinding::new(keystroke, SplitDown, context),
        "ClosePane" => KeyBinding::new(keystroke, ClosePane, context),
        "FocusNext" => KeyBinding::new(keystroke, FocusNext, context),
        "FocusPrev" => KeyBinding::new(keystroke, FocusPrev, context),
        "ZoomIn" => KeyBinding::new(keystroke, ZoomIn, context),
        "ZoomOut" => KeyBinding::new(keystroke, ZoomOut, context),
        "ToggleSidebar" => KeyBinding::new(keystroke, ToggleSidebar, context),
        "ToggleDashboard" => KeyBinding::new(keystroke, ToggleDashboard, context),
        "ToggleSettings" => KeyBinding::new(keystroke, ToggleSettings, context),
        "GlobalSearch" => KeyBinding::new(keystroke, GlobalSearch, context),
        "FindInProject" => KeyBinding::new(keystroke, FindInProject, context),
        "SearchTerminal" => KeyBinding::new(keystroke, SearchTerminal, context),
        "SaveFile" => KeyBinding::new(keystroke, SaveFile, context),
        "SaveFileAs" => KeyBinding::new(keystroke, SaveFileAs, context),
        "ClearTerminal" => KeyBinding::new(keystroke, ClearTerminal, context),
        "FocusAttention" => KeyBinding::new(keystroke, FocusAttention, context),
        "FocusLeft" => KeyBinding::new(keystroke, FocusLeft, context),
        "FocusRight" => KeyBinding::new(keystroke, FocusRight, context),
        "FocusUp" => KeyBinding::new(keystroke, FocusUp, context),
        "FocusDown" => KeyBinding::new(keystroke, FocusDown, context),
        "ShowKeys" => KeyBinding::new(keystroke, ShowKeys, context),
        "ToggleBroadcast" => KeyBinding::new(keystroke, ToggleBroadcast, context),
        // JumpToTab1..9 — the trailing digit is the tab index.
        a if a.starts_with("JumpToTab") => {
            match a
                .strip_prefix("JumpToTab")
                .and_then(|n| n.parse::<usize>().ok())
            {
                Some(n) => KeyBinding::new(keystroke, JumpToTab(n), context),
                None => return None,
            }
        }
        _ => return None,
    })
}

/// Bind default keybindings, applying any overrides from settings.
pub fn install_keybindings(settings: &muxel_core::Settings, cx: &mut App) {
    use std::collections::HashMap;
    let overrides: HashMap<&str, &str> = settings
        .keybindings
        .iter()
        .map(|k| (k.action.as_str(), k.keystroke.as_str()))
        .collect();
    // Globally-bound chords the user wants the focused terminal to receive (e.g.
    // ctrl-p for opencode): scope them out of the "Terminal" context so they fall
    // through to the PTY's key handler instead of firing muxel's shortcut.
    let passthrough: Vec<&str> = settings
        .terminal_passthrough_keys
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let mut bindings: Vec<KeyBinding> = settings_view::DEFAULT_KEYBINDINGS
        .iter()
        .filter_map(|(name, default, context)| {
            let ks = overrides.get(name).copied().unwrap_or(default);
            let ctx: Option<&str> = if context.is_none() && passthrough.contains(&ks) {
                Some("!Terminal")
            } else {
                *context
            };
            keybinding_for(name, ks, ctx)
        })
        .collect();
    // Tab / Shift+Tab go to the focused terminal (the "Terminal" key context is
    // deeper than gpui-component Root's, so these shadow Root's focus-nav). These
    // are fixed PTY routing, not rebindable commands.
    bindings.push(KeyBinding::new("tab", SendTab, Some("Terminal")));
    bindings.push(KeyBinding::new("shift-tab", SendBackTab, Some("Terminal")));
    // Ctrl+P opens the command palette too, but ONLY when no terminal is focused —
    // so a focused agent (e.g. opencode, which uses Ctrl+P) receives it, while
    // deselecting a pane (or focusing the sidebar/editor) routes Ctrl+P to muxel.
    bindings.push(KeyBinding::new("ctrl-p", GlobalSearch, Some("!Terminal")));
    // Cmd+Q (macOS) / Ctrl+Q (elsewhere) quits from any focus, including a
    // focused terminal — `secondary` resolves to the platform's quit modifier.
    bindings.push(KeyBinding::new("secondary-q", Quit, None));
    cx.bind_keys(bindings);
}

/// "NewPane" -> "New Pane" for the shortcut cheat-sheet.
fn humanize_action(name: &str) -> String {
    let mut out = String::new();
    for (i, ch) in name.char_indices() {
        if i > 0 && ch.is_uppercase() {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// "ctrl-shift-t" -> "Ctrl+Shift+T" for the shortcut cheat-sheet.
fn prettify_keys(ks: &str) -> String {
    ks.split('-')
        .map(|p| {
            let mut c = p.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// What the inline rename editor is currently targeting.
#[derive(Clone, PartialEq)]
enum RenameTarget {
    Instance(Uuid),
    Project(Uuid),
    Worktree(Uuid),
    /// Rename a file/dir on disk (file browser).
    File(PathBuf),
}

/// Drag payload for reordering projects in the sidebar.
#[derive(Clone)]
struct DragProject {
    from: usize,
}

/// Runtime state for one in-flight Loop run (its spawned pane).
struct LoopRun {
    loop_id: Uuid,
    /// True once the agent has been seen Working (so we don't close it during the
    /// brief idle window before its prompt is typed).
    seen_working: bool,
    /// When the run was spawned (for the max-runtime safety cap).
    started: std::time::Instant,
    post_run: PostRunAction,
}

/// Current wall-clock time in unix seconds (local clock; never negative here).
fn unix_now() -> u64 {
    chrono::Local::now().timestamp().max(0) as u64
}

/// A short human summary of a loop's schedule (for the settings list).
fn loop_schedule_summary(s: &LoopSchedule) -> String {
    match s {
        LoopSchedule::EveryMinutes { minutes } => {
            tf("every {minutes} min", &[("minutes", &minutes.to_string())])
        }
        LoopSchedule::EveryHours { hours } => {
            tf("every {hours} h", &[("hours", &hours.to_string())])
        }
        LoopSchedule::DailyAt { hour, minute } => tf(
            "daily {time}",
            &[("time", &format!("{hour:02}:{minute:02}"))],
        ),
    }
}

/// Safety cap: force-close a Loop's `Exit` agent if it's still running this long
/// after launch (so a wedged run never leaves a pane open forever).
const MAX_LOOP_RUNTIME: Duration = Duration::from_secs(30 * 60);

/// Drag payload for moving a single tab (tabifies into the pane it's dropped on).
#[derive(Clone)]
struct DragInstance {
    iid: Uuid,
}

/// Drag payload for moving a whole pane by its title bar (swaps panes on drop).
/// `anchor` is any instance in the dragged pane (identifies its leaf).
#[derive(Clone)]
struct DragPane {
    anchor: Uuid,
}

/// How a freshly-spawned agent joins the active project's layout.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PlacementMode {
    /// Split the target pane, creating a new pane beside it.
    Split(SplitDirection),
    /// Add the agent as a new tab in the target pane.
    Tab,
}

/// Which region of a pane body a drag is hovering — drives the drop highlight
/// and whether a drop tabifies/swaps (center) or splits (an edge).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DropZone {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

impl DropZone {
    /// The split (direction, before) an edge maps to; `None` for center.
    fn to_split(self) -> Option<(SplitDirection, bool)> {
        match self {
            DropZone::Left => Some((SplitDirection::Horizontal, true)),
            DropZone::Right => Some((SplitDirection::Horizontal, false)),
            DropZone::Top => Some((SplitDirection::Vertical, true)),
            DropZone::Bottom => Some((SplitDirection::Vertical, false)),
            DropZone::Center => None,
        }
    }
}

/// Classify a cursor `pos` within a pane body's `bounds` into a drop zone: the
/// inner 35–65% on both axes is Center; otherwise the nearer edge wins.
fn drop_zone(bounds: Bounds<Pixels>, pos: Point<Pixels>) -> DropZone {
    let w = bounds.size.width.max(px(1.0));
    let h = bounds.size.height.max(px(1.0));
    let x0 = bounds.origin.x;
    let y0 = bounds.origin.y;
    // Center deadzone: inner 35–65% on both axes (pos is already within bounds).
    let in_x = pos.x >= x0 + w * 0.35 && pos.x <= x0 + w * 0.65;
    let in_y = pos.y >= y0 + h * 0.35 && pos.y <= y0 + h * 0.65;
    if in_x && in_y {
        return DropZone::Center;
    }
    // Pixel distance to each edge; the nearer axis (then side) wins.
    let left = pos.x - x0;
    let right = (x0 + w) - pos.x;
    let top = pos.y - y0;
    let bottom = (y0 + h) - pos.y;
    let hdist = left.min(right);
    let vdist = top.min(bottom);
    if hdist <= vdist {
        if left < right {
            DropZone::Left
        } else {
            DropZone::Right
        }
    } else if top < bottom {
        DropZone::Top
    } else {
        DropZone::Bottom
    }
}

/// A translucent highlight covering the region a drop would occupy: the whole
/// body for Center, the corresponding half for an edge. Absolute + non-occluding
/// so the card's `on_drop` still fires through it.
fn drop_zone_overlay(zone: DropZone, accent: Hsla) -> impl IntoElement {
    let base = div().absolute();
    let panel = match zone {
        DropZone::Center => base.inset_0(),
        DropZone::Left => base.top_0().bottom_0().left_0().w(relative(0.5)),
        DropZone::Right => base.top_0().bottom_0().right_0().w(relative(0.5)),
        DropZone::Top => base.left_0().right_0().top_0().h(relative(0.5)),
        DropZone::Bottom => base.left_0().right_0().bottom_0().h(relative(0.5)),
    };
    panel
        .bg(accent.opacity(0.22))
        .border_2()
        .border_color(accent.opacity(0.7))
}

/// How a freshly-spawned agent acquires its git worktree.
#[derive(Clone, Copy)]
enum WorktreeChoice {
    /// Create a fresh worktree + registry entry.
    New,
    /// Share the worktree of an existing instance (tab inherit / duplicate).
    Inherit(Uuid),
    /// Re-attach to an existing (kept/detached) worktree by id.
    Resume(Uuid),
    /// No worktree.
    None,
}

/// Live state captured when the settings modal opens, so Cancel can revert.
struct SettingsSnapshot {
    settings: muxel_core::Settings,
    presets: Vec<AgentPreset>,
    theme: String,
    theme_mode: String,
    use_tmux: bool,
    use_worktree: bool,
    notifications: bool,
}

/// State of the in-app updater (drives the title-bar button + update modal).
enum UpdateState {
    /// No check has run yet this session.
    Idle,
    /// A check is in flight.
    Checking,
    /// Checked and already on the latest release.
    UpToDate,
    /// A newer release is available.
    Available(crate::update::UpdateInfo),
    /// The new version is downloading/being applied.
    Downloading,
    /// The update is staged; restart to finish.
    Ready(crate::update::RelaunchPlan),
    /// The last check or download failed.
    Error(String),
}

/// The small label shown under the cursor while dragging a project row.
struct DragGhost {
    label: SharedString,
    /// Cursor's grab offset within the source element. GPUI paints the ghost at
    /// `cursor - offset`, so we pad by `offset` to put the label at the cursor.
    offset: Point<Pixels>,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .pl(self.offset.x.max(px(0.0)))
            .pt(self.offset.y.max(px(0.0)))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().sidebar_accent)
                    .text_color(cx.theme().sidebar_accent_foreground)
                    .text_sm()
                    .child(self.label.clone()),
            )
    }
}

pub struct MuxelApp {
    workspace: Workspace,
    /// Live terminals, keyed by instance id.
    terminals: HashMap<Uuid, Entity<TerminalView>>,
    /// Live code editors, keyed by instance id (parallel to `terminals`).
    editors: HashMap<Uuid, Entity<EditorView>>,
    /// The pane the toolbar actions target.
    active_instance: Option<Uuid>,
    /// The agent preset library and the one currently selected for new panes.
    presets: Vec<AgentPreset>,
    current_preset: usize,
    /// Agent programs whose binary is currently installed (refreshed each tick).
    /// New-agent menus hide presets whose program isn't here.
    available_programs: HashSet<String>,
    /// Cached git status (refreshed each tick) so the sidebar doesn't shell out
    /// per render: each project's current branch, and each worktree's change count.
    project_branches: HashMap<Uuid, Option<String>>,
    worktree_changes: HashMap<Uuid, usize>,
    /// Whether the GitHub CLI (`gh`) is installed (gates worktree PR actions).
    gh_available: bool,
    /// Whether `sshpass` is installed (needed for saved-password SSH auth).
    sshpass_available: bool,
    /// Tick counter throttling remote branch-label polling (every 5th tick).
    remote_poll_count: u32,
    /// Full persisted settings (source of truth for config not mirrored above).
    settings: muxel_core::Settings,
    /// Active theme name + mode override (persisted).
    theme: String,
    theme_mode: String,
    /// Toolbar toggles applied to newly-created agents.
    use_tmux: bool,
    use_worktree: bool,
    /// Whether the OS window is focused (affects notifications + focus reporting).
    window_active: bool,
    /// Whether to show the cross-project dashboard instead of the pane tree.
    show_dashboard: bool,
    /// Whether the project sidebar is collapsed.
    sidebar_collapsed: bool,
    /// File-browser (second sidebar) state.
    show_file_browser: bool,
    file_browser_pid: Option<Uuid>,
    file_browser_files: Vec<PathBuf>,
    file_browser_expanded: HashSet<PathBuf>,
    file_browser_input: Entity<InputState>,
    /// Cached browser rows (recomputed only on change, not per render).
    file_browser_rows: Arc<Vec<crate::filetree::Row>>,
    /// Whether the settings page is shown.
    show_settings: bool,
    /// Settings-page widgets + selection state.
    settings_ui: SettingsUi,
    /// Whether desktop notifications are enabled.
    notifications_enabled: bool,
    /// In-app notification feed shown in the sidebar (collected regardless of the
    /// desktop toggle). Session-only; newest pushed last. One entry per instance.
    notifications: Vec<Notification>,
    /// Last seen status per instance, to fire notifications on transitions.
    last_status: HashMap<Uuid, AgentStatus>,
    /// Per-split id nonce, bumped to reset a split's resizable state when its
    /// panes are evened out (double-click a divider).
    split_even_nonce: HashMap<String, u32>,
    /// Projects whose `.muxel/MEMORY.md` we've ensured this session (once each).
    memory_ensured: HashSet<Uuid>,
    /// Remote projects whose layout we've reconciled with the host this session
    /// (the connect-time pull/push decision runs once each, like `memory_ensured`).
    remote_synced: HashSet<Uuid>,
    /// Last-seen layout `content_key` per remote project, to detect real changes
    /// (vs. timestamp-only churn) for the debounced push.
    layout_keys: HashMap<Uuid, String>,
    /// Pending debounced layout pushes: project id → earliest time to push.
    remote_push_due: HashMap<Uuid, Instant>,
    focus_handle: FocusHandle,
    /// Periodically refreshes status indicators + fires notifications.
    _status_timer: Task<()>,
    /// Periodically checks scheduled Loops (fire when due + post-run cleanup).
    _loop_timer: Task<()>,
    /// Periodically re-runs `git diff` for open diff panes (off the UI thread).
    _diff_timer: Task<()>,
    /// Debounce handle for persisting the window geometry on resize/move.
    bounds_save_task: Option<Task<()>>,
    /// Inline rename editor: the target being renamed + the shared input widget.
    rename: Option<RenameTarget>,
    rename_input: Entity<InputState>,
    /// Projects whose instance list is collapsed in the sidebar.
    collapsed: HashSet<Uuid>,
    /// Scroll position for the settings content area (drives the scrollbar).
    settings_scroll: ScrollHandle,
    /// Live state captured on open so the settings Cancel button can revert.
    settings_snapshot: Option<SettingsSnapshot>,
    /// The active workspace (None until one is chosen in the selector).
    current_workspace: Option<Uuid>,
    /// All workspaces + the last-used one (for pre-selection).
    workspaces: WorkspacesIndex,
    /// Whether the workspace selector screen is shown (always true at launch).
    show_workspace_selector: bool,
    /// "New workspace" name editor used in the selector.
    workspace_name_input: Entity<InputState>,
    /// Settings modal size (resizable via the bottom-right corner).
    settings_size: gpui::Size<Pixels>,
    /// Cached definite inner width of the settings content pane (updated each
    /// render). Lets deep helpers (`check_row`) size wrapping labels absolutely so
    /// their multi-line height is measured correctly. See [`settings_content_w`].
    settings_pane_w: Pixels,
    /// Settings card offset from centre, set by dragging its title bar.
    settings_offset: Point<Pixels>,
    /// Active settings-resize drag: (start cursor pos, base size).
    settings_resize: Option<(Point<Pixels>, gpui::Size<Pixels>)>,
    /// Active settings-move drag: (start cursor pos, base offset).
    settings_move: Option<(Point<Pixels>, Point<Pixels>)>,
    /// Update modal card size (resizable via the bottom-right corner).
    update_modal_size: gpui::Size<Pixels>,
    /// Active update-modal-resize drag: (start cursor pos, base size).
    update_resize: Option<(Point<Pixels>, gpui::Size<Pixels>)>,
    /// A terminal shown maximized over the pane area (transient; not persisted).
    maximized: Option<Uuid>,
    /// Panes detached into their own OS windows, keyed by instance id.
    popouts: HashMap<Uuid, PopOut>,
    /// Editors awaiting re-dock into the main window (rebuilt in `render`, which
    /// has the main window — gpui-component input focus is window-bound).
    pending_editor_redock: Vec<(Uuid, EditorSnapshot, RedockAnchor)>,
    /// Whether the "Quit?" confirmation modal is shown (close was intercepted).
    show_quit_confirm: bool,
    /// Whether the keyboard-shortcut cheat-sheet overlay is shown.
    show_keys: bool,
    /// Active terminal scrollback search (None = not searching).
    term_search: Option<TermSearch>,
    /// Reused input for the terminal search bar.
    term_search_input: Entity<InputState>,
    /// Whether the broadcast bar (send one line to every agent) is shown.
    broadcasting: bool,
    /// Reused input for the broadcast bar.
    broadcast_input: Entity<InputState>,
    /// Set once the user confirms quitting, so the close hook stops vetoing.
    confirm_quit: bool,
    /// An in-progress split/new-tab button press (target pane + placement). A
    /// short release places with the current preset; holding opens the picker.
    place_pending: Option<(Uuid, PlacementMode)>,
    /// When set, the agent picker is shown: (target, placement, anchor point).
    place_menu: Option<(Uuid, PlacementMode, Point<Pixels>)>,
    /// While dragging a tab over a pane: (leaf anchor, insertion index) for the
    /// drop indicator. Cleared when no tab drag is in progress.
    tab_drop: Option<(Uuid, usize)>,
    /// While dragging a tab/pane over a pane body: (leaf anchor, zone) for the
    /// edge-split highlight. Mutually exclusive with `tab_drop` (strip vs body).
    pane_drop: Option<(Uuid, DropZone)>,
    /// A destructive action awaiting confirmation (delete workspace/agent, close).
    confirm: Option<PendingConfirm>,
    /// Dirty worktrees awaiting a Commit/Discard/Keep decision (modal shows front).
    pending_worktree_dispose: std::collections::VecDeque<WorktreeDispose>,
    /// Reused commit-message input for the worktree dispose modal.
    dispose_commit_input: Entity<InputState>,
    /// Project git modal (commit / new branch) + its reused input.
    git_modal: Option<GitModal>,
    git_action_input: Entity<InputState>,
    /// New-remote-project wizard: visible flag, chosen host, and its inputs.
    show_new_remote: bool,
    nr_host: Option<Uuid>,
    nr_dir: Entity<InputState>,
    nr_name: Entity<InputState>,
    /// Inline result of the wizard's "Verify" (shown above the buttons).
    nr_verify: RemoteTestState,
    /// Reusable task launchers.
    runners: Vec<Runner>,
    /// Scheduled task launchers (run a prompt on a timer).
    loops: Vec<Loop>,
    /// Live loop runs: spawned instance id → run state (for post-run handling).
    running_loops: HashMap<Uuid, LoopRun>,
    /// Saved SSH remote hosts (the host library; edited in settings).
    remotes: Vec<RemoteHost>,
    /// In-memory SSH passwords entered this session (host id → password), for
    /// hosts using password auth without a keychain-saved password. Never
    /// persisted; cleared on exit.
    session_passwords: HashMap<Uuid, String>,
    /// Active password prompt (host without a saved password), + its input.
    password_prompt: Option<PasswordPrompt>,
    password_prompt_input: Entity<InputState>,
    /// Anchor point for the toolbar "Run task" runner popup, when open.
    runners_menu: Option<Point<Pixels>>,
    /// Anchor point for the toolbar "Loops" popup, when open.
    loops_menu: Option<Point<Pixels>>,
    /// The runner whose run-dialog is open (index into `runners`).
    active_runner: Option<usize>,
    /// Whether the run-dialog (collect details) is shown.
    show_run_dialog: bool,
    /// Detail-text editor for the run-dialog (main window).
    runner_input: Entity<InputState>,
    /// Whether the first-run Terms acceptance screen is shown.
    show_terms: bool,
    /// How muxel was installed (decides whether updates self-apply).
    install_kind: crate::update::InstallKind,
    /// In-app updater state (title-bar button + update modal).
    update_state: UpdateState,
    /// Whether the update modal is shown.
    show_update_modal: bool,
    /// Background task: checks for updates on launch, then daily.
    _update_timer: Task<()>,
    /// Ctrl+P search palette (open files / jump to named instances).
    show_search_palette: bool,
    search_input: Entity<InputState>,
    search_query: String,
    search_selected: usize,
    /// Current filtered palette results (recomputed on query change).
    search_results: Vec<SearchItem>,
    /// Cached file list for the active project (rebuilt when the palette opens).
    search_files: Vec<PathBuf>,
    /// Ctrl+Shift+F "find in project" content-search panel.
    show_find_panel: bool,
    find_input: Entity<InputState>,
    find_selected: usize,
    find_results: Vec<FindHit>,
    /// Active project's file contents, read once when the panel opens, so typing
    /// re-searches in memory without re-reading from disk.
    find_contents: Vec<(PathBuf, String)>,
}

/// One content-search match (file + 0-based line + the matched line text).
#[derive(Clone)]
struct FindHit {
    path: PathBuf,
    line: u32,
    text: String,
}

/// One actionable row in the Ctrl+P palette.
#[derive(Clone)]
enum SearchItem {
    /// Open an existing file (path relative label, absolute path) in an editor.
    OpenFile(PathBuf),
    /// Focus an existing terminal/editor instance.
    FocusInstance(Uuid),
    /// Create + open a new file at this (absolute) path.
    CreateFile(PathBuf),
    /// Run an app command/action.
    RunCommand(PaletteCommand),
}

/// Active terminal-scrollback search: the searching pane's match lines (buffer
/// line indices, oldest→newest) and the current index.
struct TermSearch {
    matches: Vec<i32>,
    idx: usize,
}

/// An app action runnable from the Ctrl+P palette.
#[derive(Clone, Copy)]
enum PaletteCommand {
    SplitRight,
    SplitDown,
    NewTab,
    ClosePane,
    RestartAgent,
    ClearScrollback,
    ToggleWorktree,
    FocusAttention,
    ToggleSidebar,
    ToggleDashboard,
    OpenSettings,
    RunRunner(usize),
}

/// The live view backing a pane — a terminal or a code editor. Lets the shared
/// pane operations (focus, pop-out) treat both uniformly.
#[derive(Clone)]
enum PaneView {
    Terminal(Entity<TerminalView>),
    Editor(Entity<EditorView>),
}

impl PaneView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            PaneView::Terminal(v) => v.read(cx).focus_handle(cx),
            PaneView::Editor(v) => v.read(cx).focus_handle(cx),
        }
    }
}

/// Cloneable editor state, captured so an editor pane can be rebuilt in another
/// window (pop-out) — gpui-component binds text-input focus to the creating
/// window, so the view is re-created rather than moved.
#[derive(Clone)]
struct EditorSnapshot {
    text: String,
    path: Option<PathBuf>,
    language: String,
    cursor: Option<Position>,
    dirty: bool,
    /// Set when the snapshot is of a diff pane (rebuilds via `EditorView::diff`).
    diff_dir: Option<PathBuf>,
}

impl EditorSnapshot {
    fn capture(ed: &Entity<EditorView>, cx: &App) -> Self {
        let e = ed.read(cx);
        Self {
            text: e.text(cx),
            path: e.path().map(|p| p.to_path_buf()),
            language: e.language(),
            cursor: Some(e.cursor(cx)),
            dirty: e.is_dirty(),
            diff_dir: e.diff_dir().map(|p| p.to_path_buf()),
        }
    }
    fn build(self, config: EditorConfig, window: &mut Window, cx: &mut App) -> Entity<EditorView> {
        if let Some(dir) = self.diff_dir {
            return cx.new(|cx| EditorView::diff(dir, config, window, cx));
        }
        cx.new(|cx| {
            EditorView::from_state(
                self.text,
                self.path,
                self.language,
                self.cursor,
                self.dirty,
                config,
                window,
                cx,
            )
        })
    }
}

/// What was detached when a pane was popped out (so it can be rebuilt in the new
/// window, and restored if the window fails to open).
enum PopoutContent {
    Terminal(Entity<TerminalView>),
    Editor(EditorSnapshot),
}

/// A pane popped out into its own window. Closing the window terminates the
/// pane (after a confirmation), so it isn't re-docked into the main panes.
struct PopOut {
    view: PaneView,
    window: WindowHandle<gpui_component::Root>,
    /// Where to put it back if it's re-docked (the Dock button).
    redock: RedockAnchor,
}

/// Enough about a popped-out pane's original spot to re-dock it faithfully.
#[derive(Clone, Copy, Debug)]
enum RedockAnchor {
    /// It was one of several tabs: re-insert at `index` in the leaf still holding
    /// `sibling` (an instance that survived the pop-out).
    Tab { sibling: Uuid, index: usize },
    /// It was the sole tab of its pane: re-create as a split beside `anchor`.
    Split {
        anchor: Uuid,
        dir: SplitDirection,
        before: bool,
    },
    /// No usable anchor (it was the only pane): fall back to the active pane.
    Floating,
}

/// Capture a [`RedockAnchor`] for `iid` from the live layout — call this BEFORE
/// removing `iid`, while its original position is still intact.
fn compute_redock_anchor(layout: &Option<PaneNode>, iid: Uuid) -> RedockAnchor {
    let Some(root) = layout.as_ref() else {
        return RedockAnchor::Floating;
    };
    let Some(path) = root.find_path(iid) else {
        return RedockAnchor::Floating;
    };
    let Some(PaneNode::Leaf(ld)) = root.get_at_path(&path) else {
        return RedockAnchor::Floating;
    };
    if ld.tabs.len() >= 2 {
        let index = ld.tabs.iter().position(|&id| id == iid).unwrap_or(0);
        // A sibling that outlives the pop-out's remove(): the next tab, or the
        // previous one when `iid` is last.
        let sib = if index + 1 < ld.tabs.len() {
            index + 1
        } else {
            index - 1
        };
        RedockAnchor::Tab {
            sibling: ld.tabs[sib],
            index,
        }
    } else {
        match root.neighbor_of(iid) {
            Some((anchor, dir, before)) => RedockAnchor::Split {
                anchor,
                dir,
                before,
            },
            None => RedockAnchor::Floating,
        }
    }
}

/// Root view for a popped-out pane window: a title bar (with a Dock button)
/// plus the pane content. The title-bar X asks for confirmation, then closes the
/// window, which the main app observes (`on_window_closed`) and tears the pane
/// down. The Dock button re-docks it into the main panes first.
struct PopoutView {
    view: PaneView,
    iid: Uuid,
    show_close_confirm: bool,
}

impl PopoutView {
    fn new(view: PaneView, iid: Uuid, cx: &mut Context<Self>) -> Self {
        // Re-render (refresh the title) when the pane updates.
        match &view {
            PaneView::Terminal(v) => cx.observe(v, |_, _, cx| cx.notify()).detach(),
            PaneView::Editor(v) => cx.observe(v, |_, _, cx| cx.notify()).detach(),
        }
        Self {
            view,
            iid,
            show_close_confirm: false,
        }
    }

    fn title(&self, cx: &App) -> SharedString {
        match &self.view {
            PaneView::Terminal(v) => v
                .read(cx)
                .title()
                .map(|t| shell_dir_title(&t).to_string())
                .unwrap_or_else(|| "Terminal".to_string())
                .into(),
            PaneView::Editor(v) => v.read(cx).title().into(),
        }
    }

    fn content(&self, cx: &App) -> AnyElement {
        match &self.view {
            PaneView::Terminal(v) => terminal_pane_element(v, cx),
            PaneView::Editor(v) => v.clone().into_any_element(),
        }
    }

    fn is_editor(&self) -> bool {
        matches!(self.view, PaneView::Editor(_))
    }
}

/// A destructive action awaiting user confirmation.
#[derive(Clone)]
enum ConfirmAction {
    DeleteWorkspace(Uuid),
    DeletePreset(usize),
    DeleteProject(Uuid),
    DeleteRunner(usize),
    DeleteLoop(usize),
    DeleteRemote(usize),
    CloseInstance(Uuid),
    /// Close every other tab in the pane holding this instance (keeps it).
    CloseOtherTabs(Uuid),
    /// Close the tabs to one side of `anchor` (keeps it).
    CloseTabsSide {
        anchor: Uuid,
        right: bool,
    },
    /// Switch the project's git branch (warned — it touches the working tree).
    SwitchBranch {
        pid: Uuid,
        branch: String,
    },
    /// Apply + remove the latest stash (can conflict).
    StashPop(Uuid),
    /// Permanently discard the latest stash.
    StashDrop(Uuid),
    /// Reset a worktree to its base, discarding all the agent's work (keeps it).
    DiscardWorktreeChanges(Uuid),
    /// Remove a worktree entirely (close its panes, delete worktree + branch).
    DiscardWorktree(Uuid),
}

/// State for the confirmation modal (title/message + the pending action).
struct PendingConfirm {
    title: SharedString,
    message: SharedString,
    confirm_label: SharedString,
    action: ConfirmAction,
}

/// A single-input git modal (commit message / new branch name) for a project.
#[derive(Clone, Copy)]
enum GitModalKind {
    Commit,
    NewBranch,
}

struct GitModal {
    pid: Uuid,
    kind: GitModalKind,
    /// Commit only: every changed/untracked file, and whether each is checked for
    /// the commit (parallel to `files`). Empty for `NewBranch`.
    files: Vec<integrations::GitChange>,
    selected: Vec<bool>,
}

/// A prompt for an SSH password not saved in the keychain. `Connect` stores the
/// entered password in memory for the session and (re)spawns the project's panes;
/// `Verify` tests once with the password and forgets it.
struct PasswordPrompt {
    host_id: Uuid,
    action: PasswordAction,
}

enum PasswordAction {
    /// Store the password for the session, then spawn this project's terminals.
    Connect(Uuid),
    /// Test the host at this index once, without storing the password.
    Verify(usize),
}

/// A worktree whose last instance just closed with work that isn't fully landed
/// (uncommitted changes and/or unmerged commits), awaiting a Commit / Merge /
/// Discard / Keep decision (shown in the dispose modal).
struct WorktreeDispose {
    wid: Uuid,
    name: String,
    color: u8,
    path: PathBuf,
    root: PathBuf,
    /// The worktree's git branch (`muxel/<id8>`), for merge/delete.
    branch: String,
    /// Uncommitted files (`git status --porcelain`).
    changed: usize,
    /// Commits on the branch not yet in the base.
    unmerged: usize,
    /// Base branch name for display (e.g. `main`), or "base" when detached.
    base_label: String,
}

/// What an in-app notification is about (drives its color/label). `Blocked`/`Done`
/// are per-agent status notifications; `Success`/`Error`/`Info` are generic events
/// (git results, connections, save errors, …) that used to be pop-up toasts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NotifKind {
    Blocked,
    Done,
    Success,
    Error,
}

impl NotifKind {
    /// The colored dot shown beside the notification.
    fn dot(self, cx: &App) -> Hsla {
        match self {
            NotifKind::Blocked => status_hsla(AgentStatus::Blocked, cx),
            NotifKind::Done => status_hsla(AgentStatus::Done, cx),
            NotifKind::Success => cx.theme().success,
            NotifKind::Error => cx.theme().danger,
        }
    }

    /// Short label shown after the title (agent notifications only).
    fn label(self) -> SharedString {
        match self {
            NotifKind::Blocked => t("needs input"),
            NotifKind::Done => t("finished"),
            NotifKind::Success => t("success"),
            NotifKind::Error => t("error"),
        }
    }
}

/// An in-app notification shown in the sidebar's NOTIFICATIONS category. Mirrors
/// the desktop notifications (bell / exit), but collected even when desktop
/// notifications are off. Session-only; never persisted.
struct Notification {
    id: Uuid,
    /// The agent this is about (clicking navigates to it). `None` for a generic
    /// event notification (git result, connection, save error, …).
    instance: Option<Uuid>,
    kind: NotifKind,
    title: String,
    /// Secondary line: for agents "{label} · {project}"; for events, a detail.
    subtitle: String,
}

impl Render for PopoutView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.title(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &SendTab, _w, cx| {
                if let PaneView::Terminal(v) = &this.view {
                    v.read(cx).session().write_input(b"\t");
                }
            }))
            .on_action(cx.listener(|this, _: &SendBackTab, _w, cx| {
                if let PaneView::Terminal(v) = &this.view {
                    v.read(cx).session().write_input(b"\x1b[Z");
                }
            }))
            .child(
                // Intercept the title-bar X to confirm before closing (which
                // terminates the terminal).
                //
                // POSSIBLE KNOWN BUG (unconfirmed): double-clicking this bar over a
                // popped-out editor *sometimes* leaves its text selection "stuck"
                // (keeps highlighting as the mouse moves). Suspected gpui-component
                // quirk — TitleBar is the only interactive component that doesn't
                // claim its press via `GlobalState::suppress_text_selection` like
                // Button/Input do, so a title-bar press can start the window-level
                // text selection. A patch to add that was tried but the bug couldn't
                // be reliably reproduced, so this is a note rather than a fix.
                TitleBar::new()
                    .on_close_window(cx.listener(|this, _ev, _window, cx| {
                        this.show_close_confirm = true;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w_full()
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font_semibold()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .child(
                                        Button::new("dock-back")
                                            .ghost()
                                            .xsmall()
                                            .icon(IconName::PanelBottom)
                                            .tooltip(t("Dock back into the app"))
                                            .on_click(cx.listener(|this, _e, window, cx| {
                                                let iid = this.iid;
                                                if let Some(app) = cx
                                                    .try_global::<MuxelHandle>()
                                                    .and_then(|h| h.0.upgrade())
                                                {
                                                    app.update(cx, |app, cx| {
                                                        app.redock_popout(iid, cx)
                                                    });
                                                }
                                                window.remove_window();
                                            })),
                                    ),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(self.content(cx)))
            .children(self.show_close_confirm.then(|| {
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(0x0000_0099))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev, _w, cx| {
                            this.show_close_confirm = false;
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .w(px(340.0))
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_5()
                            .bg(cx.theme().background)
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded(cx.theme().radius_lg)
                            .shadow_lg()
                            .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                            .child(div().text_lg().font_semibold().child(if self.is_editor() {
                                t("Close editor?")
                            } else {
                                t("Close terminal?")
                            }))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(if self.is_editor() {
                                        t("Unsaved changes will be lost.")
                                    } else {
                                        t("This terminal will be terminated.")
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .gap_2()
                                    .pt_2()
                                    .child(
                                        Button::new("popout-close-cancel")
                                            .ghost()
                                            .label(t("Cancel"))
                                            .on_click(cx.listener(|this, _e, _w, cx| {
                                                this.show_close_confirm = false;
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("popout-close-ok")
                                            .danger()
                                            .label(t("Close"))
                                            .on_click(|_e, window, _cx| window.remove_window()),
                                    ),
                            ),
                    )
            }))
    }
}

impl MuxelApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // `spawn_in` so the closure has a window: `tick` updates agent status, and
        // a clicked desktop notification (`handle_notification_click`) has to focus
        // a pane and raise the window.
        let status_timer = cx.spawn_in(window, async move |view: WeakEntity<Self>, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1000))
                    .await;
                if view
                    .update_in(cx, |this, window, cx| {
                        this.tick(cx);
                        this.handle_notification_click(window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        // Scheduled Loops: check every ~30s whether any loop is due (fire it) and
        // do post-run cleanup. `spawn_in` so the closure has a window for spawning.
        let loop_timer = cx.spawn_in(window, async move |view: WeakEntity<Self>, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                if view
                    .update_in(cx, |this, window, cx| this.tick_loops(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        // Keep open diff panes current as agents change files. `git diff` runs on
        // a background thread; results are applied (scroll-preserving) on the UI
        // thread. Idle when no diff panes are open.
        let diff_timer = cx.spawn_in(window, async move |view: WeakEntity<Self>, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1500))
                    .await;
                let Ok(jobs) = view.update(cx, |this, cx| this.diff_refresh_jobs(cx)) else {
                    break; // view dropped
                };
                if jobs.is_empty() {
                    continue;
                }
                let results = cx
                    .background_executor()
                    .spawn(async move {
                        jobs.into_iter()
                            .map(|(iid, dir)| (iid, crate::integrations::git_diff(&dir)))
                            .collect::<Vec<(Uuid, String)>>()
                    })
                    .await;
                if view
                    .update_in(cx, |this, window, cx| {
                        this.apply_diff_refreshes(results, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        // Check GitHub for a newer release shortly after launch, then once a day.
        let update_timer = cx.spawn(async move |view: WeakEntity<Self>, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            loop {
                if view
                    .update(cx, |this, cx| this.check_for_updates(cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(24 * 60 * 60))
                    .await;
            }
        });

        // Re-skin terminals whenever the active theme changes. NOTE: this must
        // NOT write `self.theme` or persist — the Theme global is also mutated by
        // zoom (set_ui_scale) and OS dark/light switches (which re-apply a default
        // theme), so deriving the saved theme here clobbered the user's choice.
        // The saved theme is set only on an explicit pick (see `set_theme`).
        cx.observe_global::<gpui_component::Theme>(|this, cx| {
            this.refresh_terminal_palettes(cx);
            cx.notify();
        })
        .detach();

        // Install the global handle so menu-dispatched actions can reach us.
        let weak = cx.weak_entity();
        cx.set_global(MuxelHandle(weak));

        // Persist the window geometry (debounced) on resize/move.
        cx.observe_window_bounds(window, |this, window, cx| {
            if this.bounds_save_task.is_some() {
                return;
            }
            this.bounds_save_task = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let _ = this.update_in(cx, |this, window, _cx| {
                    this.save_window_geom(window);
                    this.bounds_save_task = None;
                });
            }));
        })
        .detach();

        // Track OS-window focus: gate notifications + tell the active terminal
        // (so an agent stays "focused" only while you're actually on the window).
        cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            if let Some(iid) = this.active_instance
                && let Some(view) = this.terminals.get(&iid)
            {
                view.read(cx).session().report_focus(this.window_active);
            }
            cx.notify();
        })
        .detach();

        let mut settings = muxel_store::load_settings();
        // Merge in any new built-in presets (e.g. Hermes/Ollama) once.
        if settings.seed_builtin_presets() {
            let _ = muxel_store::save_settings(&settings);
        }
        let presets = if settings.presets.is_empty() {
            AgentPreset::defaults()
        } else {
            settings.presets.clone()
        };
        let current_preset = presets
            .iter()
            .position(|p| {
                p.id.to_string() == settings.default_preset || p.name == settings.default_preset
            })
            .unwrap_or(0);
        let available_programs = installed_programs(&presets);
        let settings_ui = SettingsUi::new(window, cx);
        let rename_input = cx.new(|cx| InputState::new(window, cx));
        let dispose_commit_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("Commit message (default: worktree name)"))
        });
        cx.subscribe_in(
            &rename_input,
            window,
            |this, _input, ev: &InputEvent, _window, cx| match ev {
                InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_rename(cx),
                _ => {}
            },
        )
        .detach();

        let workspace_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("New workspace name")));
        cx.subscribe_in(
            &workspace_name_input,
            window,
            |this, _input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.create_workspace_from_input(window, cx);
                }
            },
        )
        .detach();

        let runner_input = cx
            .new(|cx| InputState::new(window, cx).placeholder(t("Additional details (optional)")));
        cx.subscribe_in(
            &runner_input,
            window,
            |this, _input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.execute_runner(window, cx);
                }
            },
        )
        .detach();

        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("Search files and terminals…")));
        cx.subscribe_in(
            &search_input,
            window,
            |this, input, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    let q = input.read(cx).value().to_string();
                    this.update_search_results(q, cx);
                }
                InputEvent::PressEnter { .. } => this.confirm_search(window, cx),
                _ => {}
            },
        )
        .detach();

        let find_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("Find in project…")));
        cx.subscribe_in(
            &find_input,
            window,
            |this, input, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    let q = input.read(cx).value().to_string();
                    this.run_find(q, cx);
                }
                InputEvent::PressEnter { .. } => this.confirm_find(window, cx),
                _ => {}
            },
        )
        .detach();

        let term_search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("Search terminal…")));
        cx.subscribe_in(
            &term_search_input,
            window,
            |this, input, ev: &InputEvent, _window, cx| match ev {
                InputEvent::Change => {
                    let q = input.read(cx).value().to_string();
                    this.refresh_term_search(&q, cx);
                }
                // Enter steps to the previous (older) match — terminal search
                // usually walks backward through recent scrollback.
                InputEvent::PressEnter { .. } => this.term_search_step(-1, cx),
                _ => {}
            },
        )
        .detach();

        let broadcast_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t("Send a line to every agent in this project…"))
        });
        cx.subscribe_in(
            &broadcast_input,
            window,
            |this, _input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.send_broadcast(window, cx);
                }
            },
        )
        .detach();

        let git_action_input = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe_in(
            &git_action_input,
            window,
            |this, _input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.confirm_git_modal(window, cx);
                }
            },
        )
        .detach();

        let nr_dir = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("/path/to/project on the remote host"))
        });
        let nr_name = cx.new(|cx| InputState::new(window, cx).placeholder(t("Project name")));

        let password_prompt_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t("SSH password"))
        });
        cx.subscribe_in(
            &password_prompt_input,
            window,
            |this, _input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.confirm_password_prompt(window, cx);
                }
            },
        )
        .detach();

        let file_browser_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("Search files…")));
        cx.subscribe_in(
            &file_browser_input,
            window,
            |this, _input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.rebuild_file_browser_rows(cx);
                    cx.notify();
                }
            },
        )
        .detach();

        let runners = settings.runners.clone();
        let mut loops = settings.loops.clone();
        // Arm interval loops on load: an interval loop with no recorded last_run
        // should fire one interval from now, not immediately. (Daily-at uses the
        // time of day, so leaving it `None` is fine for the catch-up rule.)
        let now_arm = unix_now();
        for lp in loops.iter_mut() {
            if lp.last_run.is_none() && !matches!(lp.schedule, LoopSchedule::DailyAt { .. }) {
                lp.last_run = Some(now_arm);
            }
        }
        let remotes = settings.remotes.clone();
        // Dir for per-host SSH ControlMaster sockets (created once; the path is
        // computed purely thereafter by `control_path_for`).
        if let Some(d) = muxel_store::data_dir() {
            let _ = std::fs::create_dir_all(d.join("ssh"));
        }
        let show_terms = settings.accepted_terms_version < muxel_core::CURRENT_TERMS_VERSION;
        let install_kind = crate::update::InstallKind::detect();

        // Ensure a workspaces index exists (migrating a legacy workspace once).
        let workspaces = muxel_store::migrate_to_workspaces();

        let this = Self {
            workspace: Workspace::default(),
            terminals: HashMap::new(),
            editors: HashMap::new(),
            active_instance: None,
            presets,
            current_preset,
            available_programs,
            project_branches: HashMap::new(),
            worktree_changes: HashMap::new(),
            gh_available: program_on_path("gh"),
            sshpass_available: program_on_path("sshpass"),
            remote_poll_count: 0,
            theme: settings.theme.clone(),
            theme_mode: settings.theme_mode.clone(),
            use_tmux: settings.default_use_tmux,
            use_worktree: settings.default_use_worktree,
            window_active: true,
            show_dashboard: false,
            sidebar_collapsed: false,
            show_file_browser: false,
            file_browser_pid: None,
            file_browser_files: Vec::new(),
            file_browser_expanded: HashSet::new(),
            file_browser_input,
            file_browser_rows: Arc::new(Vec::new()),
            show_settings: false,
            settings_ui,
            rename: None,
            rename_input,
            collapsed: HashSet::new(),
            settings_scroll: ScrollHandle::new(),
            settings_snapshot: None,
            current_workspace: None,
            workspaces,
            show_workspace_selector: true,
            workspace_name_input,
            settings_size: size(px(780.0), px(620.0)),
            settings_pane_w: px(560.0),
            settings_offset: point(px(0.0), px(0.0)),
            settings_resize: None,
            settings_move: None,
            update_modal_size: size(px(560.0), px(520.0)),
            update_resize: None,
            maximized: None,
            popouts: HashMap::new(),
            pending_editor_redock: Vec::new(),
            show_quit_confirm: false,
            show_keys: false,
            term_search: None,
            term_search_input,
            broadcasting: false,
            broadcast_input,
            confirm_quit: false,
            place_pending: None,
            place_menu: None,
            tab_drop: None,
            pane_drop: None,
            confirm: None,
            pending_worktree_dispose: std::collections::VecDeque::new(),
            dispose_commit_input,
            git_modal: None,
            git_action_input,
            show_new_remote: false,
            nr_host: None,
            nr_dir,
            nr_name,
            nr_verify: RemoteTestState::Idle,
            runners,
            loops,
            running_loops: HashMap::new(),
            remotes,
            session_passwords: HashMap::new(),
            password_prompt: None,
            password_prompt_input,
            runners_menu: None,
            loops_menu: None,
            active_runner: None,
            show_run_dialog: false,
            runner_input,
            notifications_enabled: settings.notifications_enabled,
            notifications: Vec::new(),
            last_status: HashMap::new(),
            split_even_nonce: HashMap::new(),
            memory_ensured: HashSet::new(),
            remote_synced: HashSet::new(),
            layout_keys: HashMap::new(),
            remote_push_due: HashMap::new(),
            focus_handle: cx.focus_handle(),
            settings,
            _status_timer: status_timer,
            _loop_timer: loop_timer,
            _diff_timer: diff_timer,
            bounds_save_task: None,
            show_terms,
            install_kind,
            update_state: UpdateState::Idle,
            show_update_modal: false,
            _update_timer: update_timer,
            show_search_palette: false,
            search_input,
            search_query: String::new(),
            search_selected: 0,
            search_results: Vec::new(),
            search_files: Vec::new(),
            show_find_panel: false,
            find_input,
            find_selected: 0,
            find_results: Vec::new(),
            find_contents: Vec::new(),
        };

        // Terminate a popped-out terminal when the user closes its window.
        let weak = cx.weak_entity();
        cx.on_window_closed(move |cx, window_id| {
            if let Some(app) = weak.upgrade() {
                app.update(cx, |this, cx| this.close_popout(window_id, cx));
            }
        })
        .detach();

        // Confirm before quitting: veto the first close request + show a modal.
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |_window, cx| {
            weak.upgrade()
                .map(|app| {
                    app.update(cx, |this, cx| {
                        if this.confirm_quit {
                            return true;
                        }
                        this.show_quit_confirm = true;
                        cx.notify();
                        false
                    })
                })
                .unwrap_or(true)
        });

        // No workspace is loaded yet — the workspace selector (shown at launch)
        // calls `enter_workspace`, which loads the chosen workspace.
        this
    }

    /// Adopt a persisted workspace and spawn terminals for the active project
    /// (other projects' terminals spawn lazily when selected).
    fn restore(&mut self, workspace: Workspace, window: &mut Window, cx: &mut Context<Self>) {
        let mut workspace = workspace;
        // Give legacy per-instance worktrees a registry entry (no-op once done).
        migrate_worktrees(&mut workspace);
        self.workspace = workspace;
        let active = self
            .workspace
            .active_project
            .filter(|id| self.workspace.project(*id).is_some())
            .or_else(|| self.workspace.projects.first().map(|p| p.id));
        self.workspace.active_project = active;
        if let Some(pid) = active {
            self.ensure_project_terminals(pid, window, cx);
            self.active_instance = self.workspace.project(pid).and_then(|p| p.first_instance());
            if let Some(iid) = self.active_instance {
                self.focus_instance(iid, window, cx);
            }
        }
    }

    /// Per-host ControlMaster socket path (its directory is created). Shared by
    /// the host's panes and its git calls so they reuse one authenticated
    /// connection — making repeated git invocations cheap and one dropped
    /// connection recoverable.
    fn control_path_for(host_id: Uuid) -> String {
        // Pure (safe to call during render). The `ssh/` dir is created once at
        // startup (see the constructor).
        muxel_store::data_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("ssh")
            .join(format!("{}.sock", &host_id.simple().to_string()[..8]))
            .display()
            .to_string()
    }

    /// An SSH password for a host: the in-memory session password first, else the
    /// one saved in the OS keychain. `None` if neither is set.
    fn remote_password(&self, host_id: Uuid) -> Option<String> {
        self.session_passwords
            .get(&host_id)
            .cloned()
            .or_else(|| crate::secrets::get_remote_password(host_id))
    }

    /// The configured remote host for an instance's project, if any.
    fn remote_host_for_instance(&self, iid: Uuid) -> Option<RemoteHost> {
        let inst = self.workspace.instance(iid)?;
        let r = self.workspace.project(inst.project_id)?.remote.as_ref()?;
        self.remotes.iter().find(|h| h.id == r.host_id).cloned()
    }

    /// Program/args (+ extra env) to run an instance's command on a remote host
    /// over SSH: `ssh [opts] host -- '…'`, or `sshpass -e ssh …` for password
    /// auth (the password — keychain or this session — is passed via `$SSHPASS`).
    /// With password auth but no password available, falls back to plain `ssh`
    /// (it can prompt in the pane). Remote panes default to a persistent tmux
    /// session for reconnect resilience.
    fn remote_program_args(
        &self,
        inst: Option<&Instance>,
        host: &RemoteHost,
        remote_cwd: &str,
        resolved: &ResolvedLaunch,
    ) -> (String, Vec<String>, Vec<(String, String)>) {
        let control_path = Self::control_path_for(host.id);
        let use_tmux = host.default_use_tmux || inst.is_some_and(|i| i.use_tmux);
        let session = inst.map(|i| muxel_core::tmux::session_name(&host.name, i.id));
        let ssh_argv = muxel_core::ssh::ssh_args(&muxel_core::ssh::SshSpec {
            host,
            control_path: &control_path,
            remote_cwd: Some(remote_cwd),
            program: resolved.program.as_deref(),
            args: &resolved.args,
            use_tmux,
            tmux_session: session.as_deref(),
        });
        // sshpass -e reads the password from $SSHPASS (kept off the command line /
        // process list). Without a password, never use `sshpass -e` (it would
        // error); plain ssh can prompt interactively in the pane instead.
        if host.auth == SshAuth::Password
            && let Some(pw) = self.remote_password(host.id)
        {
            let env = vec![("SSHPASS".to_string(), pw)];
            let mut args = vec!["-e".to_string(), "ssh".to_string()];
            args.extend(ssh_argv);
            ("sshpass".to_string(), args, env)
        } else {
            ("ssh".to_string(), ssh_argv, Vec::new())
        }
    }

    /// Open the password prompt for a host without a saved password.
    fn prompt_password(
        &mut self,
        host_id: Uuid,
        action: PasswordAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.password_prompt = Some(PasswordPrompt { host_id, action });
        self.password_prompt_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        let handle = self.password_prompt_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn close_password_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.password_prompt = None;
        // Don't leave the typed password sitting in the input widget.
        self.password_prompt_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn confirm_password_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.password_prompt.take() else {
            return;
        };
        let pw = self.password_prompt_input.read(cx).value().to_string();
        if pw.is_empty() {
            // Keep the prompt open until something is entered.
            self.password_prompt = Some(p);
            return;
        }
        self.password_prompt_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        match p.action {
            PasswordAction::Connect(pid) => {
                // Hold the password in memory for the session and spawn the panes.
                self.session_passwords.insert(p.host_id, pw);
                self.ensure_project_terminals(pid, window, cx);
            }
            PasswordAction::Verify(idx) => {
                // Test once with the entered password; do not store it.
                self.run_ssh_check(idx, Some(pw), window, cx);
            }
        }
        cx.notify();
    }

    /// Build the launch command for an instance (program/args + system-prompt
    /// injection, rooted at its project).
    /// Session-resume args for a resume-capable agent, doing the `&mut`
    /// bookkeeping: generate a stable session id on first launch, flip
    /// `session_started`, and persist. Returns the CLI args to inject, or `None`
    /// for agents/instances without resume. First launch starts the session with
    /// `--session-id <id>`; every later launch `--resume <id>`.
    fn session_resume_for(&mut self, iid: Uuid) -> Option<Vec<String>> {
        let inst = self.workspace.instance(iid)?;
        let preset = inst
            .preset_id
            .and_then(|pid| self.presets.iter().find(|p| p.id == pid))
            .or_else(|| self.presets.iter().find(|p| p.name == inst.preset))?;
        if preset.session_id_flag.is_none() || preset.resume_flag.is_none() {
            return None;
        }
        let preset = preset.clone();
        let inst = self.workspace.instance_mut(iid)?;
        if inst.session_id.is_none() {
            inst.session_id = Some(Uuid::new_v4().to_string());
        }
        let snapshot = inst.clone();
        inst.session_started = true;
        self.persist();
        muxel_core::session_resume_args(&preset, &snapshot)
    }

    fn command_for(&mut self, instance_id: Uuid) -> CommandSpec {
        // Resume-capable agents (e.g. Claude): give the pane a stable session id and
        // resolve the --session-id / --resume flag *before* anything borrows the
        // instance. Mutates + persists the instance's session bookkeeping.
        let resume_args = self.session_resume_for(instance_id);
        let inst = self.workspace.instance(instance_id);
        let project = inst.and_then(|i| self.workspace.project(i.project_id));
        // Shared project memory: for an agent in a memory-enabled project, append an
        // instruction pointing it at the project's `.muxel/MEMORY.md` (read + append
        // lessons across runs). Launch-only — done on a clone so nothing persisted is
        // touched; skipped for plain shells (`InjectionMode::None` drops the prompt).
        let inst_owned = inst.cloned().map(|mut i| {
            if let Some(p) = project
                && p.memory_enabled
                && i.injection != InjectionMode::None
            {
                let root = match &p.remote {
                    Some(r) => r.remote_root.clone(),
                    None => p.root_path.display().to_string(),
                };
                let path = format!("{root}/{MEMORY_DIR}/{MEMORY_FILE}");
                let instruction = memory_instruction(&path);
                i.system_prompt = Some(match i.system_prompt.take() {
                    Some(base) if !base.is_empty() => format!("{base}\n\n{instruction}"),
                    _ => instruction,
                });
            }
            i
        });
        let mut resolved = inst_owned
            .as_ref()
            .map(resolve_launch)
            .unwrap_or(ResolvedLaunch {
                program: None,
                args: Vec::new(),
                startup_input: None,
                auto_mode_presses: 0,
                submit: true,
                env: Vec::new(),
            });
        // The session flag goes ahead of model / system-prompt args.
        if let Some(mut resume) = resume_args {
            resume.append(&mut resolved.args);
            resolved.args = resume;
        }
        // Classify on the agent program (the matches below consume resolved.program).
        let agent_program = resolved.program.clone();

        // Remote (SSH) project? Resolve its configured host.
        let remote = project.and_then(|p| p.remote.as_ref()).and_then(|r| {
            let host = self.remotes.iter().find(|h| h.id == r.host_id)?;
            Some((host, r))
        });

        // Build program/args (and, for local, the PTY working dir). For remote the
        // command becomes `ssh … -- 'cd <dir> && exec <program>'`; the cwd lives
        // inside that remote string, so there is no local PTY cwd.
        let (mut spec, local_cwd, extra_env) = if let Some((host, rref)) = remote {
            let remote_cwd = inst
                .and_then(|i| i.worktree_path.as_ref())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| rref.remote_root.clone());
            let (program, args, env) = self.remote_program_args(inst, host, &remote_cwd, &resolved);
            (CommandSpec::program(program, args), None, env)
        } else {
            // Local: worktree path wins as the working dir; otherwise project root.
            let cwd: Option<String> = inst
                .and_then(|i| i.worktree_path.clone())
                .map(|p| p.display().to_string())
                .or_else(|| project.map(|p| p.root_path.display().to_string()));
            // If this instance uses tmux, wrap the command in `tmux new-session -A`
            // so it persists and re-attaches across restarts.
            let spec = match inst.and_then(|i| i.tmux_session.clone()) {
                Some(session) => {
                    let args = muxel_core::tmux::new_session_args(
                        &session,
                        cwd.as_deref(),
                        resolved.program.as_deref(),
                        &resolved.args,
                    );
                    CommandSpec::program("tmux", args)
                }
                None => match resolved.program.clone() {
                    Some(program) => CommandSpec::program(program, resolved.args.clone()),
                    None => CommandSpec::shell(),
                },
            };
            (spec, cwd, Vec::new())
        };
        if let Some(cwd) = local_cwd {
            spec = spec.with_cwd(cwd);
        }
        if let Some(input) = resolved.startup_input.clone() {
            spec = spec.with_startup_input(input);
        }
        spec = spec.with_auto_mode(resolved.auto_mode_presses);
        spec = spec.with_submit(resolved.submit);
        spec.env = resolved.env.clone();
        spec.env.extend(extra_env);

        // Status markers: the preset's overrides per field, else the program's
        // built-in defaults (empty → bell/activity heuristic).
        let (def_working, def_blocked) = default_markers(agent_program.as_deref());
        let preset = inst
            .and_then(|i| i.preset_id)
            .and_then(|id| self.presets.iter().find(|p| p.id == id));
        let working = preset
            .map(|p| p.working_markers.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or(def_working);
        let blocked = preset
            .map(|p| p.blocked_markers.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or(def_blocked);
        let startup_delay = preset.map(|p| p.startup_delay_ms).unwrap_or(0);
        spec.with_startup_delay(startup_delay)
            .with_markers(working, blocked)
    }

    /// Spawn (or replace) the live terminal for an instance id.
    fn spawn_terminal(&mut self, instance_id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        // A remote password host with no saved/session password: prompt for it
        // first (storing it in memory), then this spawn is retried via
        // `ensure_project_terminals`. Avoids `sshpass -e` with an empty $SSHPASS.
        if let Some(host) = self.remote_host_for_instance(instance_id)
            && host.auth == SshAuth::Password
            && self.remote_password(host.id).is_none()
            && let Some(pid) = self.workspace.instance(instance_id).map(|i| i.project_id)
        {
            self.prompt_password(host.id, PasswordAction::Connect(pid), window, cx);
            return;
        }
        let spec = self.command_for(instance_id);
        let palette = theme::palette_from_theme(cx);
        let font_family: SharedString = self.settings.font_family.clone().into();
        let font_size = self.settings.font_size * self.settings.zoom;
        let mouse_mode = TerminalMouseMode::from_setting(&self.settings.terminal_mouse);
        let view = cx.new(move |cx| {
            let mut view = TerminalView::new(spec, window, cx);
            view.set_palette(palette);
            view.set_config(font_family, font_size);
            view.set_mouse_mode(mouse_mode);
            view
        });
        self.terminals.insert(instance_id, view);
        // A runner submits only on its first launch; clear auto_submit afterward
        // so reopening the app re-types the prompt but doesn't auto-submit it.
        if let Some(inst) = self.workspace.instance_mut(instance_id)
            && inst.is_runner
            && inst.auto_submit
        {
            inst.auto_submit = false;
            self.persist();
        }
    }

    /// Re-derive the terminal palette from the active theme and apply it to all
    /// live terminals (called after a theme change).
    fn refresh_terminal_palettes(&mut self, cx: &mut Context<Self>) {
        let palette = theme::palette_from_theme(cx);
        for view in self.terminals.values() {
            view.update(cx, |view, _cx| view.set_palette(palette.clone()));
        }
    }

    /// Apply + persist an explicitly chosen theme. This is the ONLY place the
    /// saved theme is written, so background Theme-global mutations (zoom, OS
    /// appearance) can never overwrite it. The observer re-skins terminals.
    fn set_theme(&mut self, name: SharedString, cx: &mut Context<Self>) {
        theme::apply_theme(&name, cx);
        self.theme = name.to_string();
        self.theme_mode = if cx.theme().mode.is_dark() {
            "dark".to_string()
        } else {
            "light".to_string()
        };
        self.persist_settings();
        cx.notify();
    }

    /// Switch the UI language at runtime: load the catalog, persist the choice
    /// ("en"/None = follow the OS locale), and refresh every window so all `t()`
    /// strings re-render without a restart.
    fn set_language(&mut self, lang: String, cx: &mut Context<Self>) {
        crate::i18n::set_language(&lang);
        self.settings.language = if lang == "en" { None } else { Some(lang) };
        self.persist_settings();
        cx.refresh_windows();
        cx.notify();
    }

    /// Ensure every instance in a project's layout has a live terminal.
    /// Spawn live terminals/editors for a project's panes that lack one. For a
    /// remote project this prompts for a password if needed, then verifies login
    /// **before** opening the panes — telling the user what went wrong on failure
    /// instead of filling each pane with an ssh error.
    fn ensure_project_terminals(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        // Make sure the shared memory file/gitignore exist (once per session) —
        // handles fresh clones where `.muxel/` was git-ignored away.
        if self
            .workspace
            .project(pid)
            .is_some_and(|p| p.memory_enabled)
            && self.memory_ensured.insert(pid)
        {
            self.ensure_project_memory(pid, cx);
        }
        let is_remote = self.workspace.project(pid).is_some_and(|p| p.is_remote());
        // The first time we reach a remote project this session, reconcile its
        // layout with the host even if it has no local panes yet — an empty local
        // layout is exactly when another machine's session should be pulled in.
        let first_sync = is_remote && !self.remote_synced.contains(&pid);

        // Nothing to do if every pane already has a live view (and we've already
        // reconciled the remote layout this session).
        let needs = self
            .workspace
            .project(pid)
            .map(|p| p.instances())
            .unwrap_or_default()
            .into_iter()
            .any(|iid| !self.terminals.contains_key(&iid) && !self.editors.contains_key(&iid));
        if !needs && !first_sync {
            return;
        }

        if is_remote && let Some(host) = self.remote_host_for_project(pid) {
            // Need a password and don't have one → prompt; the Connect action
            // re-enters here once it's stored.
            if host.auth == SshAuth::Password && self.remote_password(host.id).is_none() {
                self.prompt_password(host.id, PasswordAction::Connect(pid), window, cx);
                return;
            }
            // Pre-flight: verify login (and warm the ControlMaster) before opening.
            let control_path = Self::control_path_for(host.id);
            let password = self.remote_password(host.id);
            let host_id = host.id;
            let name = host.name.clone();
            // On the first connect, also fetch the host's saved layout so the
            // callback can resolve newer-wins before spawning panes.
            let loc = if first_sync { self.repo_loc(pid) } else { None };
            cx.spawn_in(window, async move |this, cx| {
                let (res, fetched) = cx
                    .background_executor()
                    .spawn(async move {
                        let res =
                            integrations::ssh_check(&host, &control_path, password.as_deref());
                        let fetched = match (&res, loc) {
                            (Ok(()), Some(loc)) => integrations::fetch_remote_layout(&loc),
                            _ => None,
                        };
                        (res, fetched)
                    })
                    .await;
                let _ = this.update_in(cx, |this, window, cx| match res {
                    Ok(()) => {
                        this.add_event(
                            NotifKind::Success,
                            tf("Connected to “{name}”", &[("name", &name.to_string())]),
                            String::new(),
                        );
                        if first_sync {
                            this.apply_remote_layout_sync(pid, fetched, window, cx);
                        }
                        this.spawn_project_terminals_now(pid, window, cx);
                        // A pull may have replaced the layout (and the focused pane
                        // no longer exists) — land focus on the (new) first pane.
                        if first_sync
                            && Some(pid) == this.workspace.active_project
                            && let Some(iid) =
                                this.workspace.project(pid).and_then(|p| p.first_instance())
                        {
                            this.focus_instance(iid, window, cx);
                        }
                        cx.notify();
                    }
                    Err(e) => {
                        // Drop a possibly-wrong session password so a retry re-prompts.
                        this.session_passwords.remove(&host_id);
                        this.add_event(
                            NotifKind::Error,
                            tf(
                                "Couldn't connect to “{name}”",
                                &[("name", &name.to_string())],
                            ),
                            format!("{e}"),
                        );
                        cx.notify();
                    }
                });
            })
            .detach();
            return;
        }
        self.spawn_project_terminals_now(pid, window, cx);
    }

    /// The configured remote host for a project, if any.
    fn remote_host_for_project(&self, pid: Uuid) -> Option<RemoteHost> {
        let r = self.workspace.project(pid)?.remote.as_ref()?;
        self.remotes.iter().find(|h| h.id == r.host_id).cloned()
    }

    /// Spawn any missing terminals/editors for a project's panes (no remote
    /// pre-flight — call [`Self::ensure_project_terminals`] for that).
    fn spawn_project_terminals_now(
        &mut self,
        pid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for iid in self
            .workspace
            .project(pid)
            .map(|p| p.instances())
            .unwrap_or_default()
        {
            let kind = self
                .workspace
                .instance(iid)
                .map(|i| i.kind)
                .unwrap_or(InstanceKind::Terminal);
            match kind {
                InstanceKind::Terminal => {
                    if !self.terminals.contains_key(&iid) {
                        self.spawn_terminal(iid, window, cx);
                    }
                }
                InstanceKind::Editor => {
                    if !self.editors.contains_key(&iid) {
                        let path = self
                            .workspace
                            .instance(iid)
                            .and_then(|i| i.editor_path.clone());
                        let config = self.editor_config();
                        let ed = cx.new(|cx| EditorView::open(path, config, window, cx));
                        self.editors.insert(iid, ed);
                    }
                }
                InstanceKind::Diff => {
                    if !self.editors.contains_key(&iid) {
                        // `editor_path` holds the directory to diff; re-run it so a
                        // restored diff pane reflects the current working tree.
                        if let Some(dir) = self
                            .workspace
                            .instance(iid)
                            .and_then(|i| i.editor_path.clone())
                        {
                            let config = self.editor_config();
                            let ed = cx.new(|cx| EditorView::diff(dir, config, window, cx));
                            self.editors.insert(iid, ed);
                        }
                    }
                }
            }
        }
    }

    /// Persist the current workspace to disk (best-effort).
    fn persist(&self) {
        let Some(id) = self.current_workspace else {
            return; // no workspace chosen yet (selector still open)
        };
        let Some(path) = muxel_store::workspace_doc_path(id) else {
            return;
        };
        if let Err(e) = muxel_store::save_workspace_to(&path, &self.workspace) {
            log::warn!("failed to save workspace: {e}");
        }
    }

    /// Tear down the current workspace's terminals and load another workspace's
    /// workspace. Used at launch (from the selector) and to switch workspaces.
    fn enter_workspace(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let views: Vec<_> = self.terminals.drain().map(|(_, v)| v).collect();
        for view in views {
            view.read(cx).session().kill();
        }
        // Editors just drop (unsaved changes lost on workspace switch).
        self.editors.clear();
        self.pending_editor_redock.clear();
        // Close + tear down any popped-out panes from the previous workspace.
        for (_, popout) in self.popouts.drain() {
            if let PaneView::Terminal(view) = &popout.view {
                view.read(cx).session().kill();
            }
            let _ = popout
                .window
                .update(cx, |_, window, _| window.remove_window());
        }
        self.last_status.clear();
        self.maximized = None;
        self.workspace = Workspace::default();
        self.active_instance = None;

        self.current_workspace = Some(id);
        self.workspaces.current = Some(id);
        let _ = muxel_store::save_workspaces_index(&self.workspaces);

        let loaded = muxel_store::workspace_doc_path(id)
            .and_then(|p| muxel_store::load_workspace_from(&p))
            .filter(|w| !w.projects.is_empty());
        if let Some(ws) = loaded {
            self.restore(ws, window, cx);
        }
        // An empty workspace starts with no projects — the user adds one with the
        // sidebar's New Project button (no auto-creation in the current folder).
        self.show_workspace_selector = false;
        cx.notify();
    }

    /// Create a new (empty) workspace and enter it.
    fn create_workspace(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let id = Uuid::new_v4();
        self.workspaces.workspaces.push(WorkspaceMeta { id, name });
        let _ = muxel_store::save_workspaces_index(&self.workspaces);
        self.enter_workspace(id, window, cx);
    }

    /// Read the selector's name field and create a workspace if non-empty.
    fn create_workspace_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .workspace_name_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if name.is_empty() {
            return;
        }
        self.workspace_name_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.create_workspace(name, window, cx);
    }

    /// Reopen the selector to switch workspaces (pre-selects the current one).
    fn open_workspace_selector(&mut self, cx: &mut Context<Self>) {
        self.workspaces.current = self.current_workspace;
        self.show_workspace_selector = true;
        cx.notify();
    }

    /// Derive a project name from a folder path.
    fn project_name_from(path: &std::path::Path) -> String {
        path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "project".to_string())
    }

    /// The currently-selected preset (falls back to a plain shell).
    fn current_agent_preset(&self) -> AgentPreset {
        self.presets
            .get(self.current_preset)
            .cloned()
            .unwrap_or_else(AgentPreset::shell)
    }

    /// Create a project rooted at `root`, spawn its first pane with the current
    /// preset, and make it active.
    fn create_project_at(
        &mut self,
        root: PathBuf,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Uuid {
        let mut project = Project::new(name, root);
        let pid = project.id;
        let preset = self.current_agent_preset();
        let instance = Instance::from_preset(pid, &preset);
        let iid = instance.id;
        project.layout = Some(PaneNode::leaf(iid));

        self.workspace.add_instance(instance);
        self.workspace.add_project(project);
        self.workspace.active_project = Some(pid);
        // Point the open file browser at the new project right away.
        if self.show_file_browser {
            self.load_file_browser(pid, cx);
        }

        self.spawn_terminal(iid, window, cx);
        self.focus_instance(iid, window, cx);
        self.persist();
        cx.notify();
        pid
    }

    /// Create a project that lives on a remote host (over SSH). `root_path` is set
    /// cosmetically to the remote path; the real working dir comes from the
    /// [`RemoteRef`].
    fn create_remote_project_at(
        &mut self,
        host_id: Uuid,
        remote_dir: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Uuid {
        let mut project = Project::new(name, PathBuf::from(&remote_dir));
        project.remote = Some(RemoteRef {
            host_id,
            remote_root: remote_dir,
        });
        let pid = project.id;
        let preset = self.current_agent_preset();
        let instance = Instance::from_preset(pid, &preset);
        let iid = instance.id;
        project.layout = Some(PaneNode::leaf(iid));

        self.workspace.add_instance(instance);
        self.workspace.add_project(project);
        self.workspace.active_project = Some(pid);
        // Point the open file browser at the new project right away.
        if self.show_file_browser {
            self.load_file_browser(pid, cx);
        }

        // Goes through the remote pre-flight (password prompt + login check).
        self.ensure_project_terminals(pid, window, cx);
        self.focus_instance(iid, window, cx);
        self.persist();
        cx.notify();
        pid
    }

    /// Open the new-remote-project wizard (defaults the host to the first saved).
    fn open_remote_project_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_new_remote = true;
        self.nr_host = self.remotes.first().map(|h| h.id);
        self.nr_verify = RemoteTestState::Idle;
        self.nr_dir.update(cx, |s, cx| s.set_value("", window, cx));
        self.nr_name.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn close_remote_project_modal(&mut self, cx: &mut Context<Self>) {
        self.show_new_remote = false;
        cx.notify();
    }

    /// Verify the chosen remote directory exists (background + toast).
    fn verify_remote_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self
            .nr_host
            .and_then(|id| self.remotes.iter().find(|h| h.id == id))
            .cloned()
        else {
            return;
        };
        let dir = self.nr_dir.read(cx).value().trim().to_string();
        if dir.is_empty() {
            return;
        }
        let password = self.remote_password(host.id);
        // Can't verify a password host without a password (none saved/in session).
        if host.auth == SshAuth::Password && password.is_none() {
            self.nr_verify = RemoteTestState::Failed(
                t("Save a password for this host (or connect once) to verify.").into(),
            );
            cx.notify();
            return;
        }
        let control_path = Self::control_path_for(host.id);
        // Inline result shown above the wizard buttons (not a sidebar event).
        self.nr_verify = RemoteTestState::Testing;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    integrations::ssh_test_dir(&host, &control_path, password.as_deref(), &dir)
                        .map(|()| dir)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.nr_verify = match res {
                    Ok(dir) => RemoteTestState::Ok(tf("Found {dir}", &[("dir", &dir.to_string())])),
                    Err(e) => RemoteTestState::Failed(format!("{e}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Create the remote project from the wizard inputs.
    fn confirm_remote_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host_id) = self.nr_host else {
            return;
        };
        let dir = self.nr_dir.read(cx).value().trim().to_string();
        if dir.is_empty() {
            return;
        }
        let mut name = self.nr_name.read(cx).value().trim().to_string();
        if name.is_empty() {
            // Default to the remote directory's last component.
            name = dir
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("remote")
                .to_string();
        }
        self.show_new_remote = false;
        self.create_remote_project_at(host_id, dir, name, window, cx);
    }

    /// Open a native folder picker, then create a project rooted at the choice.
    fn new_project_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t("Open")),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = receiver.await
                && let Some(dir) = paths.pop()
            {
                let name = Self::project_name_from(&dir);
                let _ = this.update_in(cx, |this, window, cx| {
                    this.create_project_at(dir, name, window, cx);
                });
            }
        })
        .detach();
    }

    fn select_project(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        // Leaving a remote project with a pending layout change → flush it now so
        // the remote copy is current even if we don't return for a while.
        if let Some(prev) = self.workspace.active_project
            && prev != pid
            && self.remote_push_due.remove(&prev).is_some()
        {
            self.push_remote_layout_now(prev, cx);
        }
        self.workspace.active_project = Some(pid);
        // Adopt the project's default preset as the current selection.
        if let Some(def) = self.workspace.project(pid).and_then(|p| p.default_preset)
            && let Some(idx) = self.presets.iter().position(|p| p.id == def)
        {
            self.current_preset = idx;
        }
        self.ensure_project_terminals(pid, window, cx);
        self.active_instance = self.workspace.project(pid).and_then(|p| p.first_instance());
        if let Some(iid) = self.active_instance {
            self.focus_instance(iid, window, cx);
        }
        // Keep the file browser pointed at the project being shown.
        if self.show_file_browser {
            self.load_file_browser(pid, cx);
        }
        self.persist();
        cx.notify();
    }

    fn focus_instance(&mut self, iid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.active_instance = Some(iid);
        // Attending a pane (by any means) clears its pending notification.
        self.clear_notifications_for(iid);
        // Make `iid` the active tab of its pane, so the right tab is shown.
        if let Some(pid) = self.workspace.instance(iid).map(|i| i.project_id)
            && let Some(p) = self.workspace.project_mut(pid)
        {
            set_active_tab(&mut p.layout, iid);
        }
        if let Some(view) = self.terminals.get(&iid) {
            // Attending to a pane clears its "awaiting input" bell.
            view.read(cx).session().clear_bell();
            let handle = view.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        } else if let Some(ed) = self.editors.get(&iid) {
            let handle = ed.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// Move keyboard focus off the active pane onto the app root (the "muxel" key
    /// context), so muxel shortcuts — including Ctrl+P → command palette — work
    /// instead of going to the focused terminal. Triggered by clicking app chrome.
    fn deselect_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Write raw bytes to the focused terminal's PTY (used by the Tab/Shift+Tab
    /// actions so they reach the terminal instead of moving keyboard focus).
    fn send_to_active(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if let Some(iid) = self.active_instance
            && let Some(view) = self.terminals.get(&iid)
        {
            view.read(cx).session().write_input(bytes);
        }
    }

    fn set_preset_by_id(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(idx) = self.presets.iter().position(|p| p.id == id) {
            self.current_preset = idx;
            cx.notify();
        }
    }

    fn set_default_preset(&mut self, id: Uuid, cx: &mut Context<Self>) {
        // Per-project default (and adopt it as the current selection).
        if let Some(pid) = self.workspace.active_project {
            if let Some(project) = self.workspace.project_mut(pid) {
                project.default_preset = Some(id);
            }
            self.persist();
        }
        self.settings.default_preset = id.to_string();
        if let Some(idx) = self.presets.iter().position(|p| p.id == id) {
            self.current_preset = idx;
        }
        self.persist_settings();
        cx.notify();
    }

    /// The default preset id for the active project (project default → global).
    fn active_default_preset_id(&self) -> Option<Uuid> {
        self.workspace
            .active_project
            .and_then(|pid| self.workspace.project(pid))
            .and_then(|p| p.default_preset)
            .or_else(|| {
                self.presets
                    .iter()
                    .find(|p| p.id.to_string() == self.settings.default_preset)
                    .map(|p| p.id)
            })
    }

    fn toggle_tmux(&mut self, cx: &mut Context<Self>) {
        self.use_tmux = !self.use_tmux;
        self.persist_settings();
        cx.notify();
    }

    fn toggle_worktree(&mut self, cx: &mut Context<Self>) {
        self.use_worktree = !self.use_worktree;
        self.persist_settings();
        cx.notify();
    }

    fn toggle_dashboard(&mut self, cx: &mut Context<Self>) {
        self.show_dashboard = !self.show_dashboard;
        cx.notify();
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }

    fn toggle_notifications(&mut self, cx: &mut Context<Self>) {
        self.notifications_enabled = !self.notifications_enabled;
        self.persist_settings();
        cx.notify();
    }

    /// Persist the current toolbar preferences to the TOML config.
    fn persist_settings(&self) {
        let settings = muxel_core::Settings {
            default_use_tmux: self.use_tmux,
            default_use_worktree: self.use_worktree,
            notifications_enabled: self.notifications_enabled,
            presets: self.presets.clone(),
            runners: self.runners.clone(),
            loops: self.loops.clone(),
            remotes: self.remotes.clone(),
            theme: self.theme.clone(),
            theme_mode: self.theme_mode.clone(),
            ..self.settings.clone()
        };
        if let Err(e) = muxel_store::save_settings(&settings) {
            log::warn!("failed to save settings: {e}");
        }
    }

    /// Record the user's acceptance of the current Terms/Privacy version and
    /// dismiss the first-run screen.
    fn accept_terms(&mut self, cx: &mut Context<Self>) {
        self.settings.accepted_terms_version = muxel_core::CURRENT_TERMS_VERSION;
        self.persist_settings();
        self.show_terms = false;
        cx.notify();
    }

    /// Open the update modal, kicking off a check if none has run yet.
    fn open_update_modal(&mut self, cx: &mut Context<Self>) {
        self.show_update_modal = true;
        if matches!(self.update_state, UpdateState::Idle) {
            self.check_for_updates(cx);
        } else {
            cx.notify();
        }
    }

    /// Whether a newer release is available, downloading, or staged.
    fn update_pending(&self) -> bool {
        matches!(
            self.update_state,
            UpdateState::Available(_) | UpdateState::Downloading | UpdateState::Ready(_)
        )
    }

    /// Query GitHub for a newer release (off the UI thread). Fires a desktop
    /// notification when one is found.
    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.update_state,
            UpdateState::Checking | UpdateState::Downloading
        ) {
            return;
        }
        self.update_state = UpdateState::Checking;
        cx.notify();
        let notify_enabled = self.notifications_enabled;
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::update::fetch_latest() })
                .await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(Some(info)) => {
                        if notify_enabled {
                            notify(
                                tf(
                                    "muxel {version} is available",
                                    &[("version", &info.version.to_string())],
                                ),
                                t("Open muxel to install the update.").to_string(),
                                None,
                            );
                        }
                        this.update_state = UpdateState::Available(info);
                    }
                    Ok(None) => this.update_state = UpdateState::UpToDate,
                    Err(e) => {
                        log::warn!("update check failed: {e}");
                        this.update_state = UpdateState::Error(e.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Download the matching release asset and apply it (off the UI thread).
    fn start_update_download(&mut self, cx: &mut Context<Self>) {
        let kind = self.install_kind;
        let url = match &self.update_state {
            UpdateState::Available(info) => match crate::update::asset_for(kind, &info.assets) {
                Some((_, url)) => url.clone(),
                None => return,
            },
            _ => return,
        };
        self.update_state = UpdateState::Downloading;
        cx.notify();
        let notify_enabled = self.notifications_enabled;
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::update::download_and_apply(kind, &url) })
                .await;
            let _ = view.update(cx, |this, cx| {
                match result {
                    Ok(plan) => {
                        if notify_enabled {
                            notify(
                                t("muxel update ready").to_string(),
                                t("Restart to finish updating.").to_string(),
                                None,
                            );
                        }
                        this.update_state = UpdateState::Ready(plan);
                    }
                    Err(e) => {
                        log::warn!("update download failed: {e}");
                        this.update_state = UpdateState::Error(e.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Relaunch into the freshly-installed version (never returns on success).
    fn apply_update_restart(&mut self, _cx: &mut Context<Self>) {
        if let UpdateState::Ready(plan) = &self.update_state {
            crate::update::relaunch_and_exit(plan);
        }
    }

    /// Refresh program availability + cached git status (installed agents, `gh`,
    /// `sshpass`, project branches, worktree change counts) **off the UI thread**.
    /// These do `$PATH` scans and a `git` subprocess per local project/worktree —
    /// far too slow to run synchronously every tick (it stutters window drags) —
    /// so they run on the background executor and post results back. Remote
    /// project branches are handled separately by `poll_remote_branches`.
    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        let presets = self.presets.clone();
        let locals: Vec<(Uuid, PathBuf)> = self
            .workspace
            .projects
            .iter()
            .filter(|p| !p.is_remote())
            .map(|p| (p.id, p.root_path.clone()))
            .collect();
        let worktrees: Vec<(Uuid, PathBuf)> = self
            .workspace
            .worktrees
            .iter()
            .map(|w| (w.id, w.path.clone()))
            .collect();
        cx.spawn(async move |this, cx| {
            let (available, gh, sshpass, branches, changes) = cx
                .background_executor()
                .spawn(async move {
                    let available = installed_programs(&presets);
                    let gh = program_on_path("gh");
                    let sshpass = program_on_path("sshpass");
                    let branches: Vec<(Uuid, Option<String>)> = locals
                        .into_iter()
                        .map(|(id, root)| {
                            let loc = integrations::RepoLoc::Local(root);
                            (id, integrations::repo_current_branch(&loc))
                        })
                        .collect();
                    let changes: Vec<(Uuid, usize)> = worktrees
                        .into_iter()
                        .map(|(id, path)| (id, integrations::worktree_change_count(&path)))
                        .collect();
                    (available, gh, sshpass, branches, changes)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.available_programs = available;
                this.gh_available = gh;
                this.sshpass_available = sshpass;
                // Keep only current projects (drop removed ones); overwrite local
                // branches. Remote ones are maintained by `poll_remote_branches`.
                let ids: HashSet<Uuid> = this.workspace.projects.iter().map(|p| p.id).collect();
                this.project_branches.retain(|id, _| ids.contains(id));
                for (id, b) in branches {
                    this.project_branches.insert(id, b);
                }
                this.worktree_changes = changes.into_iter().collect();
                cx.notify();
            });
        })
        .detach();
    }

    /// Refresh the branch label for remote projects off the UI thread (their git
    /// runs over SSH, reusing the pane's ControlMaster — no keychain read here).
    fn poll_remote_branches(&mut self, cx: &mut Context<Self>) {
        let jobs: Vec<(Uuid, integrations::RepoLoc)> = self
            .workspace
            .projects
            .iter()
            .filter_map(|p| {
                let r = p.remote.as_ref()?;
                let host = self.remotes.iter().find(|h| h.id == r.host_id)?.clone();
                Some((
                    p.id,
                    integrations::RepoLoc::remote(
                        host,
                        r.remote_root.clone(),
                        Self::control_path_for(r.host_id),
                        None,
                    ),
                ))
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    jobs.into_iter()
                        .map(|(pid, loc)| (pid, integrations::repo_current_branch(&loc)))
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (pid, branch) in results {
                    this.project_branches.insert(pid, branch);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Status-refresh tick: re-render and fire desktop notifications for
    /// unfocused agents when they finish or ring the terminal bell. The bell is
    /// the agent's deliberate "I need you" signal (e.g. Claude on a permission
    /// prompt), so it's precise — no guessing from idle time.
    fn tick(&mut self, cx: &mut Context<Self>) {
        // Program availability (installed agents / gh / sshpass) + git status are
        // refreshed off the UI thread so they don't stutter the render loop.
        self.refresh_status(cx);
        // Throttle remote (ssh) branch polling to every ~5s.
        if self.remote_poll_count == 0 {
            self.poll_remote_branches(cx);
        }
        self.remote_poll_count = (self.remote_poll_count + 1) % 5;
        let focused = self.active_instance;
        let snapshot: Vec<(Uuid, AgentStatus, bool, String, String)> = self
            .terminals
            .iter()
            .map(|(iid, view)| {
                let v = view.read(cx);
                let status = v.status();
                let exited = v.exited();
                let inst = self.workspace.instance(*iid);
                let title = inst.map(|i| i.title.clone()).unwrap_or_default();
                let project = inst
                    .and_then(|i| self.workspace.project(i.project_id))
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                (*iid, status, exited, title, project)
            })
            .collect();

        let mut to_close = Vec::new();
        // Only re-render when something visible actually changed. Re-rendering
        // every second (rebuilding every button) is what strands gpui-component
        // tooltips: a repaint landing as the cursor leaves a button drops the
        // hover-out event, leaving the tooltip stuck until another one shows.
        let mut dirty = false;
        for (iid, status, exited, title, project) in snapshot {
            let changed = self.last_status.insert(iid, status) != Some(status);
            dirty |= changed;
            // A pane counts as attended only if it's active AND the window is
            // focused; otherwise its agent's bell/exit is worth a notification.
            let attended = self.window_active && Some(iid) == focused;
            if changed && !attended {
                let kind = match status {
                    AgentStatus::Blocked => Some(NotifKind::Blocked),
                    AgentStatus::Done => Some(NotifKind::Done),
                    _ => None,
                };
                if let Some(kind) = kind {
                    // Collect the in-app entry regardless of the desktop toggle;
                    // only the OS popup respects `notifications_enabled`.
                    self.add_notification(iid, kind, &title, &project);
                    if self.notifications_enabled {
                        notify(format!("{title} {}", kind.label()), project, Some(iid));
                    }
                }
            }
            // Close-on-exit keys off the actual process exit, not the display
            // state (Done also means "finished a turn" while still running).
            if exited && self.settings.close_on_exit {
                to_close.push(iid);
            }
        }
        for iid in to_close {
            // Auto-close on process exit: keep a remote tmux session alive (a
            // dropped SSH connection should be reconnectable, not torn down).
            self.close_instance_inner(iid, false, cx); // re-renders on its own
        }

        let live: HashSet<Uuid> = self.terminals.keys().copied().collect();
        self.last_status.retain(|iid, _| live.contains(iid));
        // Sync remote projects' layouts to their hosts (change-detect + debounce).
        self.tick_remote_sync(cx);
        if dirty {
            cx.notify();
        }
    }

    /// Scheduled-loop heartbeat (every ~30s): post-run cleanup, then fire any loop
    /// that's due. `tick()` keeps `last_status` fresh, which this reads.
    fn tick_loops(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.process_running_loops(cx);
        let now = chrono::Local::now();
        let now_epoch = now.timestamp().max(0) as u64;
        let now_tod = chrono::Timelike::num_seconds_from_midnight(&now);
        // Loop ids with a run already in flight (don't stack a second copy).
        let active: HashSet<Uuid> = self.running_loops.values().map(|r| r.loop_id).collect();
        let due: Vec<usize> = self
            .loops
            .iter()
            .enumerate()
            .filter_map(|(i, lp)| {
                (lp.enabled
                    && !active.contains(&lp.id)
                    && self.workspace.project(lp.project_id).is_some()
                    && lp.schedule.is_due(lp.last_run, now_epoch, now_tod))
                .then_some(i)
            })
            .collect();
        for i in due {
            self.fire_loop(i, now_epoch, window, cx);
        }
    }

    /// Watch in-flight loop runs: close a finished `Exit` agent (idle after working,
    /// or past the safety cap); stop tracking ones that completed or vanished.
    fn process_running_loops(&mut self, cx: &mut Context<Self>) {
        if self.running_loops.is_empty() {
            return;
        }
        let mut untrack: Vec<Uuid> = Vec::new();
        let mut close: Vec<Uuid> = Vec::new();
        for iid in self.running_loops.keys().copied().collect::<Vec<_>>() {
            // Pane gone (closed manually, or exited + auto-closed) → stop tracking.
            if !self.terminals.contains_key(&iid) {
                untrack.push(iid);
                continue;
            }
            let status = self.last_status.get(&iid).copied();
            let run = self.running_loops.get_mut(&iid).expect("present");
            if status == Some(AgentStatus::Working) {
                run.seen_working = true;
            }
            let finished = run.seen_working
                && matches!(status, Some(AgentStatus::Idle) | Some(AgentStatus::Done));
            let timed_out = run.started.elapsed() >= MAX_LOOP_RUNTIME;
            if finished || timed_out {
                untrack.push(iid);
                if run.post_run == PostRunAction::Exit {
                    close.push(iid);
                }
            }
        }
        for iid in untrack {
            self.running_loops.remove(&iid);
        }
        for iid in close {
            self.close_instance_inner(iid, true, cx);
        }
    }

    /// Fire loop `idx`: record its run time (persisted), spawn the agent, and track
    /// the run for post-run handling.
    fn fire_loop(
        &mut self,
        idx: usize,
        now_epoch: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(lp) = self.loops.get(idx).cloned() else {
            return;
        };
        if let Some(l) = self.loops.get_mut(idx) {
            l.last_run = Some(now_epoch);
        }
        self.persist_settings();
        if let Some(iid) = self.spawn_loop_agent(&lp, window, cx) {
            self.running_loops.insert(
                iid,
                LoopRun {
                    loop_id: lp.id,
                    seen_working: false,
                    started: std::time::Instant::now(),
                    post_run: lp.post_run,
                },
            );
            let project = self
                .workspace
                .project(lp.project_id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            self.add_event(
                NotifKind::Success,
                tf("Loop “{name}” started", &[("name", &lp.name.to_string())]),
                project,
            );
        }
        cx.notify();
    }

    /// Spawn a loop's agent as a brand-new pane at the END of its project's layout
    /// (mirrors `run_runner_inner`'s instance setup). The pane is visible but NOT
    /// focused — a loop firing on a timer must never steal focus from the pane the
    /// user is typing in, nor switch their active project. Returns the new id.
    fn spawn_loop_agent(
        &mut self,
        lp: &Loop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Uuid> {
        let pid = lp.project_id;
        self.workspace.project(pid)?; // must still exist
        let preset = lp
            .preset_id
            .and_then(|id| self.presets.iter().find(|p| p.id == id).cloned())
            .unwrap_or_else(|| self.current_agent_preset());
        let prompt = lp.prompt.replace("{{input}}", "").trim_end().to_string();
        let mut instance = Instance::from_preset(pid, &preset);
        instance.system_prompt = Some(prompt);
        instance.injection = InjectionMode::TypeIn;
        instance.auto_mode_presses = lp.auto_mode_presses;
        instance.custom_name = Some(lp.name.clone());
        instance.is_runner = true;
        // Background pane: no tmux session (repeated fires would orphan sessions).
        instance.use_tmux = false;
        let iid = instance.id;
        // Append as its own pane after the last leaf (an empty project seeds the
        // root). Closing it later (Exit policy) normalizes the layout back.
        let last = self
            .workspace
            .project(pid)
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.last_instance());
        if let Some(p) = self.workspace.project_mut(pid) {
            match last {
                Some(t) => {
                    split(&mut p.layout, t, SplitDirection::Horizontal, iid);
                }
                None => p.layout = Some(PaneNode::leaf(iid)),
            }
        }
        self.workspace.add_instance(instance);
        // `spawn_terminal` does NOT focus (focus is a separate step we skip here).
        self.spawn_terminal(iid, window, cx);
        self.persist();
        cx.notify();
        Some(iid)
    }

    /// Run a loop immediately (the "Run now" button / palette), ignoring its
    /// schedule but still advancing `last_run` so the scheduled fire doesn't double.
    fn run_loop_now(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let already = self
            .loops
            .get(idx)
            .is_some_and(|lp| self.running_loops.values().any(|r| r.loop_id == lp.id));
        if already {
            return;
        }
        self.fire_loop(idx, unix_now(), window, cx);
    }

    /// Add an in-app notification for `iid`, replacing any existing one for that
    /// pane so notifications don't pile up.
    fn add_notification(&mut self, iid: Uuid, kind: NotifKind, title: &str, project: &str) {
        self.notifications.retain(|n| n.instance != Some(iid));
        self.notifications.push(Notification {
            id: Uuid::new_v4(),
            instance: Some(iid),
            kind,
            title: title.to_string(),
            subtitle: format!("{} · {}", kind.label(), project),
        });
    }

    /// Add a generic event notification to the sidebar feed (replaces pop-up
    /// toasts). Newest last; the feed is capped so it can't grow unbounded.
    fn add_event(
        &mut self,
        kind: NotifKind,
        title: impl Into<String>,
        subtitle: impl Into<String>,
    ) {
        self.notifications.push(Notification {
            id: Uuid::new_v4(),
            instance: None,
            kind,
            title: title.into(),
            subtitle: subtitle.into(),
        });
        const MAX: usize = 50;
        let len = self.notifications.len();
        if len > MAX {
            self.notifications.drain(0..len - MAX);
        }
    }

    /// Remove any notification(s) targeting `iid` (attending or closing a pane).
    fn clear_notifications_for(&mut self, iid: Uuid) {
        self.notifications.retain(|n| n.instance != Some(iid));
    }

    /// Click a notification: navigate to its pane (focusing a popout window, or
    /// switching project + focusing the pane) and dismiss it.
    fn open_notification(&mut self, nid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let inst = self
            .notifications
            .iter()
            .find(|n| n.id == nid)
            .and_then(|n| n.instance);
        self.notifications.retain(|n| n.id != nid);
        // Generic events have no pane to navigate to — clicking just dismisses.
        if let Some(iid) = inst {
            if let Some(popout) = self.popouts.get(&iid) {
                let _ = popout
                    .window
                    .update(cx, |_, window, _| window.activate_window());
            } else if self.workspace.instance(iid).is_some() {
                // Switches project if needed, activates the tab, focuses, clears bell.
                self.select_instance(iid, window, cx);
            }
        }
        cx.notify();
    }

    /// Dismiss a single notification.
    fn dismiss_notification(&mut self, nid: Uuid, cx: &mut Context<Self>) {
        self.notifications.retain(|n| n.id != nid);
        cx.notify();
    }

    /// Dismiss all notifications.
    fn clear_notifications(&mut self, cx: &mut Context<Self>) {
        self.notifications.clear();
        cx.notify();
    }

    /// Create a git worktree for `instance`, filling in its worktree fields. On
    /// any problem the worktree option is turned off for this instance.
    fn setup_worktree(&self, pid: Uuid, instance: &mut Instance) {
        let Some(root) = self.workspace.project(pid).map(|p| p.root_path.clone()) else {
            instance.use_worktree = false;
            return;
        };
        let repo_name = self
            .workspace
            .project(pid)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        if !integrations::is_git_repo(&root) {
            log::warn!(
                "worktree requested but {} is not a git repo",
                root.display()
            );
            instance.use_worktree = false;
            return;
        }
        let Some(base) = muxel_store::data_dir().map(|d| d.join("worktrees")) else {
            instance.use_worktree = false;
            return;
        };
        let _ = std::fs::create_dir_all(&base);
        let path = muxel_core::worktree::worktree_path(&base, &repo_name, instance.id);
        let branch = muxel_core::worktree::branch_name(instance.id);
        match integrations::create_worktree(&root, &path, &branch) {
            Ok(()) => {
                instance.worktree_path = Some(path);
                instance.worktree_branch = Some(branch);
            }
            Err(e) => {
                log::warn!("worktree creation failed: {e}");
                instance.use_worktree = false;
            }
        }
    }

    /// Create a new agent from the current preset: split the active pane, or
    /// seed an empty project's first pane.
    fn add_agent(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_agent_at(
            self.active_instance,
            direction,
            self.current_preset,
            window,
            cx,
        );
    }

    /// Create a new agent from `preset_idx`, splitting `target` (or seeding the
    /// layout if empty).
    fn add_agent_at(
        &mut self,
        target: Option<Uuid>,
        direction: SplitDirection,
        preset_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place_with_preset(
            target,
            PlacementMode::Split(direction),
            preset_idx,
            window,
            cx,
        );
    }

    /// Spawn an agent from `preset_idx` and place it (split or tab) at `target`.
    /// The shared body behind every split / new-tab path.
    fn place_with_preset(
        &mut self,
        target: Option<Uuid>,
        placement: PlacementMode,
        preset_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pid) = self.workspace.active_project else {
            return;
        };
        let preset = self
            .presets
            .get(preset_idx)
            .cloned()
            .unwrap_or_else(AgentPreset::shell);
        let instance = Instance::from_preset(pid, &preset);
        self.place_and_spawn(pid, instance, placement, target, None, window, cx);
    }

    /// Create a new agent from the current preset as a tab in the active pane.
    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_tab_in(self.active_instance, window, cx);
    }

    /// Create a new agent as a tab in the pane holding `target` (the pane's `+`
    /// button); `None` falls back to seeding the layout for an empty project.
    fn new_tab_in(&mut self, target: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        self.place_with_preset(target, PlacementMode::Tab, self.current_preset, window, cx);
    }

    /// The preset index of the active pane's instance — what the keyboard New
    /// Tab / New Pane shortcuts clone, so a new pane matches whatever you're on
    /// rather than the toolbar's "new agent" selector. `None` when there's no
    /// active instance or its preset no longer exists.
    fn active_preset_index(&self) -> Option<usize> {
        let inst = self.workspace.instance(self.active_instance?)?;
        if let Some(pid) = inst.preset_id
            && let Some(idx) = self.presets.iter().position(|p| p.id == pid)
        {
            return Some(idx);
        }
        self.presets.iter().position(|p| p.name == inst.preset)
    }

    /// New tab / new pane from the **active pane's** preset (the keyboard
    /// shortcuts), so you get a fresh instance of whatever you're on. Falls back
    /// to the toolbar selector if the active pane has no matching preset.
    fn new_like_active(
        &mut self,
        mode: PlacementMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let preset = self.active_preset_index().unwrap_or(self.current_preset);
        self.place_with_preset(self.active_instance, mode, preset, window, cx);
    }

    /// Cycle the active pane's tabs by `delta` (wrapping), focusing the result.
    fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_instance else {
            return;
        };
        let Some(pid) = self.workspace.active_project else {
            return;
        };
        let next = self.workspace.project(pid).and_then(|p| {
            let root = p.layout.as_ref()?;
            let path = root.find_path(active)?;
            let (tabs, _) = root.get_at_path(&path)?.tabs()?;
            if tabs.len() < 2 {
                return None;
            }
            let cur = tabs.iter().position(|&t| t == active)?;
            let len = tabs.len() as isize;
            let idx = (((cur as isize + delta) % len + len) % len) as usize;
            Some(tabs[idx])
        });
        if let Some(next) = next {
            self.focus_instance(next, window, cx);
        }
    }

    /// Apply the toolbar tmux/worktree toggles to `instance`, insert it into the
    /// project layout (per `placement`, or seed if empty), spawn its terminal,
    /// focus it, and persist. Shared by `add_agent_at`, `new_tab`, and `run_runner`.
    #[allow(clippy::too_many_arguments)]
    fn place_and_spawn(
        &mut self,
        pid: Uuid,
        mut instance: Instance,
        placement: PlacementMode,
        target: Option<Uuid>,
        explicit_worktree: Option<WorktreeChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        instance.use_tmux = self.use_tmux && cfg!(unix);
        let iid = instance.id;

        // An empty project ignores the target/placement and just seeds a pane.
        let empty = self.workspace.project(pid).is_some_and(|p| p.is_empty());
        let placed_target = if empty { None } else { target };

        // Decide the worktree: explicit (duplicate/resume) wins; otherwise a new
        // tab OR split inherits the worktree of the pane it joins, if it has one;
        // only when joining a worktree-less pane (or seeding an empty project)
        // does the toggle decide whether to make a fresh worktree.
        let choice = explicit_worktree.unwrap_or_else(|| match placed_target {
            Some(t)
                if self
                    .workspace
                    .instance(t)
                    .and_then(|i| i.worktree_id)
                    .is_some() =>
            {
                WorktreeChoice::Inherit(t)
            }
            _ if self.use_worktree => WorktreeChoice::New,
            _ => WorktreeChoice::None,
        });
        self.apply_worktree_choice(pid, &mut instance, choice);

        if instance.use_tmux {
            let project_name = self
                .workspace
                .project(pid)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            instance.tmux_session = Some(muxel_core::tmux::session_name(&project_name, iid));
        }

        match (placement, placed_target) {
            (PlacementMode::Split(direction), Some(active)) => {
                let ok = self
                    .workspace
                    .project_mut(pid)
                    .is_some_and(|p| split(&mut p.layout, active, direction, iid));
                if !ok {
                    return;
                }
                self.workspace.add_instance(instance);
            }
            (PlacementMode::Tab, Some(active)) => {
                let ok = self
                    .workspace
                    .project_mut(pid)
                    .is_some_and(|p| add_tab(&mut p.layout, active, iid));
                if !ok {
                    return;
                }
                self.workspace.add_instance(instance);
            }
            (_, None) => {
                self.workspace.add_instance(instance);
                if let Some(project) = self.workspace.project_mut(pid) {
                    project.layout = Some(PaneNode::leaf(iid));
                }
            }
        }
        self.spawn_terminal(iid, window, cx);
        self.focus_instance(iid, window, cx);
        self.persist();
        cx.notify();
    }

    /// Set `instance`'s worktree fields per `choice` (and create/register a new
    /// worktree when needed).
    fn apply_worktree_choice(
        &mut self,
        pid: Uuid,
        instance: &mut Instance,
        choice: WorktreeChoice,
    ) {
        match choice {
            WorktreeChoice::New => {
                instance.use_worktree = true;
                self.setup_worktree_into_registry(pid, instance);
            }
            WorktreeChoice::Inherit(src) => {
                if let Some(s) = self.workspace.instance(src) {
                    instance.use_worktree = s.use_worktree;
                    instance.worktree_path = s.worktree_path.clone();
                    instance.worktree_branch = s.worktree_branch.clone();
                    instance.worktree_id = s.worktree_id;
                }
            }
            WorktreeChoice::Resume(wid) => {
                if let Some(w) = self.workspace.worktree(wid) {
                    instance.use_worktree = true;
                    instance.worktree_path = Some(w.path.clone());
                    instance.worktree_branch = Some(w.branch.clone());
                    instance.worktree_id = Some(wid);
                }
                if let Some(w) = self.workspace.worktree_mut(wid) {
                    w.detached = false;
                }
            }
            WorktreeChoice::None => instance.use_worktree = false,
        }
    }

    /// Create a fresh git worktree for `instance` (via `setup_worktree`) and add a
    /// named, colored registry entry, linking the instance to it.
    fn setup_worktree_into_registry(&mut self, pid: Uuid, instance: &mut Instance) {
        self.setup_worktree(pid, instance); // sets path/branch, or clears use_worktree
        if instance.use_worktree {
            let id = Uuid::new_v4();
            let color = self.workspace.next_worktree_color(pid);
            self.workspace.add_worktree(Worktree {
                id,
                project_id: pid,
                name: muxel_core::worktree::random_name(),
                path: instance.worktree_path.clone().unwrap_or_default(),
                branch: instance.worktree_branch.clone().unwrap_or_default(),
                color,
                detached: false,
            });
            instance.worktree_id = Some(id);
        }
    }

    /// Spawn a new agent into an existing (kept/detached) worktree — no new
    /// `git worktree add`. Used to resume a Kept worktree from the sidebar.
    fn spawn_into_worktree(&mut self, wid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.workspace.worktree(wid).map(|w| w.project_id) else {
            return;
        };
        let preset = self
            .presets
            .get(self.current_preset)
            .cloned()
            .unwrap_or_else(AgentPreset::shell);
        let instance = Instance::from_preset(pid, &preset);
        let target = self.active_instance;
        self.place_and_spawn(
            pid,
            instance,
            PlacementMode::Split(SplitDirection::Horizontal),
            target,
            Some(WorktreeChoice::Resume(wid)),
            window,
            cx,
        );
    }

    /// Snapshot the project's open terminal agents (preset + worktree flag) as its
    /// saved startup set.
    fn save_project_startup(&mut self, pid: Uuid, _window: &mut Window, cx: &mut Context<Self>) {
        let agents: Vec<StartupAgent> = self
            .workspace
            .project(pid)
            .map(|p| p.instances())
            .unwrap_or_default()
            .iter()
            .filter_map(|iid| self.workspace.instance(*iid))
            .filter(|i| i.kind == InstanceKind::Terminal)
            .map(|i| StartupAgent {
                preset_id: i.preset_id,
                use_worktree: i.use_worktree,
            })
            .collect();
        let n = agents.len();
        let name = self
            .workspace
            .project(pid)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        if let Some(p) = self.workspace.project_mut(pid) {
            p.startup = agents;
        }
        self.persist();
        self.add_event(
            NotifKind::Success,
            tn(
                "Saved {n} agent as startup for “{name}”",
                "Saved {n} agents as startup for “{name}”",
                n,
                &[("n", &n.to_string()), ("name", &name)],
            ),
            String::new(),
        );
        cx.notify();
    }

    /// Launch the project's saved startup agents (cascading splits).
    fn launch_project_startup(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let startup = self
            .workspace
            .project(pid)
            .map(|p| p.startup.clone())
            .unwrap_or_default();
        if startup.is_empty() {
            return;
        }
        self.select_project(pid, window, cx);
        for sa in startup {
            let preset = sa
                .preset_id
                .and_then(|id| self.presets.iter().find(|p| p.id == id).cloned())
                .unwrap_or_else(AgentPreset::shell);
            let instance = Instance::from_preset(pid, &preset);
            let target = self.active_instance;
            self.place_and_spawn(
                pid,
                instance,
                PlacementMode::Split(SplitDirection::Horizontal),
                target,
                sa.use_worktree.then_some(WorktreeChoice::New),
                window,
                cx,
            );
        }
    }

    /// Build the editor configuration from the current settings.
    fn editor_config(&self) -> EditorConfig {
        EditorConfig {
            font_family: self.settings.editor_font_family.clone(),
            font_size: self.settings.editor_font_size,
            tab_size: (self.settings.editor_tab_size.max(1)) as usize,
            soft_wrap: self.settings.editor_soft_wrap,
            line_numbers: self.settings.editor_line_numbers,
            indent_guides: self.settings.editor_indent_guides,
        }
    }

    /// Apply the current editor settings to all open editors.
    fn apply_editor_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.editor_config();
        let editors: Vec<_> = self.editors.values().cloned().collect();
        for ed in editors {
            ed.update(cx, |e, cx| e.set_config(cfg.clone(), window, cx));
        }
    }

    /// Open `path` (None = a new Untitled buffer) as an editor pane in `pid`,
    /// splitting beside `target` (or seeding if the project is empty). If the
    /// file is already open, focuses that pane. Returns the instance id.
    fn open_editor_at(
        &mut self,
        pid: Uuid,
        path: Option<PathBuf>,
        target: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Uuid> {
        // Reuse an already-open editor for this exact path.
        if let Some(p) = &path
            && let Some(iid) = self
                .editors
                .iter()
                .find_map(|(iid, ed)| (ed.read(cx).path() == Some(p.as_path())).then_some(*iid))
        {
            self.focus_instance(iid, window, cx);
            return Some(iid);
        }
        let instance = Instance::editor(pid, path.clone());
        let iid = instance.id;
        let empty = self.workspace.project(pid).is_some_and(|p| p.is_empty());
        let split_target = if empty { None } else { target };
        match split_target {
            Some(active) => {
                let ok = self
                    .workspace
                    .project_mut(pid)
                    .is_some_and(|p| split(&mut p.layout, active, SplitDirection::Horizontal, iid));
                if !ok {
                    return None;
                }
                self.workspace.add_instance(instance);
            }
            None => {
                self.workspace.add_instance(instance);
                if let Some(project) = self.workspace.project_mut(pid) {
                    project.layout = Some(PaneNode::leaf(iid));
                }
            }
        }
        let config = self.editor_config();
        let ed = cx.new(|cx| EditorView::open(path.clone(), config, window, cx));
        self.editors.insert(iid, ed.clone());
        // Remote project: the local read in `EditorView::open` finds nothing, so
        // fetch the file's contents over SSH (background) and fill the editor.
        if let Some(p) = &path
            && self.workspace.project(pid).is_some_and(|pr| pr.is_remote())
            && let Some(loc) = self.repo_loc(pid)
        {
            let abs = p.to_string_lossy().into_owned();
            let ed = ed.clone();
            cx.spawn_in(window, async move |_this, cx| {
                let content = cx
                    .background_executor()
                    .spawn(async move { integrations::read_remote_file(&loc, &abs) })
                    .await;
                if let Some(text) = content {
                    let _ = ed.update_in(cx, |e, window, cx| e.set_content(text, window, cx));
                }
            })
            .detach();
        }
        self.focus_instance(iid, window, cx);
        self.persist();
        cx.notify();
        Some(iid)
    }

    /// Open a read-only git-diff pane to the right of `source_iid`, diffing that
    /// agent's worktree (if any) or the project root. Reuses + refreshes an
    /// existing diff pane for the same directory.
    fn open_diff_for(&mut self, source_iid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(src) = self.workspace.instance(source_iid) else {
            return;
        };
        let pid = src.project_id;
        let dir = src
            .worktree_path
            .clone()
            .or_else(|| self.workspace.project(pid).map(|p| p.root_path.clone()))
            .unwrap_or_default();
        self.open_diff_for_dir(pid, dir, Some(source_iid), window, cx);
    }

    /// Open a read-only git-diff pane for the worktree `wid`, anchored beside one
    /// of its panes (or the active/first pane).
    fn open_worktree_diff(&mut self, wid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(w) = self.workspace.worktree(wid) else {
            return;
        };
        let (pid, dir) = (w.project_id, w.path.clone());
        let anchor = self
            .workspace
            .instances_using(wid)
            .into_iter()
            .next()
            .or(self.active_instance)
            .or_else(|| self.workspace.project(pid).and_then(|p| p.first_instance()));
        self.open_diff_for_dir(pid, dir, anchor, window, cx);
    }

    /// Open a read-only git-diff pane for `pid`'s repo root, split beside one of
    /// its panes (or seeding the layout if empty). Local projects only — the diff
    /// pane runs `git diff` on a local path.
    fn open_project_diff(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.workspace.project(pid).map(|p| p.root_path.clone()) else {
            return;
        };
        let anchor = self
            .workspace
            .project(pid)
            .and_then(|p| p.first_instance())
            .or(self.active_instance);
        self.open_diff_for_dir(pid, dir, anchor, window, cx);
    }

    /// Open (or refresh + focus) a read-only git-diff pane for `dir`, split beside
    /// `anchor` (or seeding the layout if the project is empty).
    fn open_diff_for_dir(
        &mut self,
        pid: Uuid,
        dir: PathBuf,
        anchor: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if dir.as_os_str().is_empty() {
            return;
        }
        // Reuse an already-open diff pane for the same directory: refresh + focus.
        if let Some(iid) = self.editors.iter().find_map(|(iid, ed)| {
            let ed = ed.read(cx);
            (ed.diff_dir() == Some(dir.as_path())).then_some(*iid)
        }) {
            if let Some(ed) = self.editors.get(&iid).cloned() {
                ed.update(cx, |e, cx| e.refresh_diff(window, cx));
            }
            self.focus_instance(iid, window, cx);
            return;
        }

        let instance = Instance::diff(pid, dir.clone());
        let iid = instance.id;
        let ok = self
            .workspace
            .project_mut(pid)
            .is_some_and(|p| match anchor {
                Some(a) => split(&mut p.layout, a, SplitDirection::Horizontal, iid),
                None => {
                    if p.is_empty() {
                        p.layout = Some(PaneNode::leaf(iid));
                        true
                    } else {
                        false
                    }
                }
            });
        if !ok {
            return;
        }
        self.workspace.add_instance(instance);
        let config = self.editor_config();
        let ed = cx.new(|cx| EditorView::diff(dir, config, window, cx));
        self.editors.insert(iid, ed);
        self.focus_instance(iid, window, cx);
        self.persist();
        cx.notify();
    }

    /// Re-run `git diff` for an open diff pane.
    fn refresh_diff_pane(&mut self, iid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ed) = self.editors.get(&iid).cloned() {
            ed.update(cx, |e, cx| e.refresh_diff(window, cx));
        }
    }

    /// (instance id, directory) for every open diff pane — the work list for the
    /// background refresh timer.
    fn diff_refresh_jobs(&self, cx: &App) -> Vec<(Uuid, PathBuf)> {
        self.editors
            .iter()
            .filter_map(|(iid, ed)| ed.read(cx).diff_dir().map(|d| (*iid, d.to_path_buf())))
            .collect()
    }

    /// Apply background-computed diff text to each pane, keeping scroll position.
    fn apply_diff_refreshes(
        &mut self,
        results: Vec<(Uuid, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (iid, content) in results {
            if let Some(ed) = self.editors.get(&iid).cloned() {
                ed.update(cx, |e, cx| e.set_diff_content(content, window, cx));
            }
        }
    }

    // ===== Ctrl+P search palette =====

    fn open_search_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_search_palette = true;
        self.search_selected = 0;
        self.search_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        // Build the file list for the active project (gitignore-aware, capped).
        self.search_files = self
            .workspace
            .active()
            .map(|p| list_project_files(&p.root_path))
            .unwrap_or_default();
        self.update_search_results(String::new(), cx);
        let handle = self.search_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn close_search_palette(&mut self, cx: &mut Context<Self>) {
        self.show_search_palette = false;
        cx.notify();
    }

    /// Commands offered in the Ctrl+P palette (plus one per runner).
    fn palette_commands(&self) -> Vec<PaletteCommand> {
        use PaletteCommand::*;
        let mut cmds = vec![
            SplitRight,
            SplitDown,
            NewTab,
            ClosePane,
            RestartAgent,
            ClearScrollback,
            ToggleWorktree,
            FocusAttention,
            ToggleSidebar,
            ToggleDashboard,
            OpenSettings,
        ];
        cmds.extend((0..self.runners.len()).map(RunRunner));
        cmds
    }

    fn palette_command_label(&self, cmd: PaletteCommand) -> String {
        match cmd {
            PaletteCommand::SplitRight => t("Split pane right").into(),
            PaletteCommand::SplitDown => t("Split pane down").into(),
            PaletteCommand::NewTab => t("New tab").into(),
            PaletteCommand::ClosePane => "Close pane".into(),
            PaletteCommand::RestartAgent => "Restart agent".into(),
            PaletteCommand::ClearScrollback => "Clear scrollback".into(),
            PaletteCommand::ToggleWorktree => t("Toggle git worktree for new agents").into(),
            PaletteCommand::FocusAttention => t("Focus next agent needing attention").into(),
            PaletteCommand::ToggleSidebar => "Toggle sidebar".into(),
            PaletteCommand::ToggleDashboard => t("Toggle dashboard (all agents)").into(),
            PaletteCommand::OpenSettings => t("Open settings").into(),
            PaletteCommand::RunRunner(i) => self
                .runners
                .get(i)
                .map(|r| tf("Run: {name}", &[("name", &r.name.to_string())]))
                .unwrap_or_default(),
        }
    }

    fn run_palette_command(
        &mut self,
        cmd: PaletteCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match cmd {
            PaletteCommand::SplitRight => self.add_agent(SplitDirection::Horizontal, window, cx),
            PaletteCommand::SplitDown => self.add_agent(SplitDirection::Vertical, window, cx),
            PaletteCommand::NewTab => self.new_tab(window, cx),
            PaletteCommand::ClosePane => self.close_active(window, cx),
            PaletteCommand::RestartAgent => self.restart_active(window, cx),
            PaletteCommand::ClearScrollback => self.clear_active_terminal(cx),
            PaletteCommand::ToggleWorktree => self.toggle_worktree(cx),
            PaletteCommand::FocusAttention => self.focus_attention(window, cx),
            PaletteCommand::ToggleSidebar => self.toggle_sidebar(cx),
            PaletteCommand::ToggleDashboard => self.toggle_dashboard(cx),
            PaletteCommand::OpenSettings => self.toggle_settings(window, cx),
            PaletteCommand::RunRunner(i) => self.run_runner(i, String::new(), window, cx),
        }
    }

    /// Recompute the filtered palette results for `query`.
    fn update_search_results(&mut self, query: String, cx: &mut Context<Self>) {
        self.search_query = query.clone();
        let q = query.trim().to_lowercase();
        let active_pid = self.workspace.active_project;
        let mut results: Vec<SearchItem> = Vec::new();

        // Named instances (active project first), matched on custom name/title.
        let mut instances: Vec<(Uuid, String, bool)> = self
            .workspace
            .instances
            .iter()
            .map(|i| {
                let label = i
                    .custom_name
                    .clone()
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| i.title.clone());
                (i.id, label, Some(i.project_id) == active_pid)
            })
            .collect();
        instances.sort_by_key(|x| !x.2); // active-project instances first
        for (iid, label, _) in &instances {
            if q.is_empty() || label.to_lowercase().contains(&q) {
                results.push(SearchItem::FocusInstance(*iid));
            }
        }

        // Runnable commands/actions.
        for cmd in self.palette_commands() {
            let label = self.palette_command_label(cmd);
            if q.is_empty() || label.to_lowercase().contains(&q) {
                results.push(SearchItem::RunCommand(cmd));
            }
        }

        // Files in the active project.
        let mut matched_file = false;
        for path in &self.search_files {
            if results.len() >= 250 {
                break;
            }
            if q.is_empty() || path.to_string_lossy().to_lowercase().contains(&q) {
                results.push(SearchItem::OpenFile(path.clone()));
                matched_file = true;
            }
        }

        // Offer to create a new file when the query looks like a path + matched none.
        if !q.is_empty()
            && !matched_file
            && looks_like_path(&query)
            && let Some(root) = self.workspace.active().map(|p| p.root_path.clone())
        {
            results.push(SearchItem::CreateFile(root.join(query.trim())));
        }

        self.search_results = results;
        self.search_selected = 0;
        cx.notify();
    }

    fn move_search_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        if self.search_results.is_empty() {
            return;
        }
        let n = self.search_results.len() as i32;
        self.search_selected = (self.search_selected as i32 + delta).rem_euclid(n) as usize;
        cx.notify();
    }

    fn confirm_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.search_results.get(self.search_selected).cloned() else {
            return;
        };
        self.activate_search_item(item, window, cx);
    }

    fn activate_search_item(
        &mut self,
        item: SearchItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_search_palette = false;
        match item {
            SearchItem::FocusInstance(iid) => {
                if let Some(pid) = self.workspace.instance(iid).map(|i| i.project_id)
                    && self.workspace.active_project != Some(pid)
                {
                    self.select_project(pid, window, cx);
                }
                self.focus_instance(iid, window, cx);
            }
            SearchItem::OpenFile(path) | SearchItem::CreateFile(path) => {
                if let Some(pid) = self.workspace.active_project {
                    let target = self.active_instance;
                    let _ = self.open_editor_at(pid, Some(path), target, window, cx);
                }
            }
            SearchItem::RunCommand(cmd) => self.run_palette_command(cmd, window, cx),
        }
        cx.notify();
    }

    // ===== Ctrl+Shift+F find in project =====

    fn open_find_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_find_panel = true;
        self.find_selected = 0;
        self.find_results.clear();
        self.find_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        // Read the project's text files once; typing then re-searches in memory.
        self.find_contents = self
            .workspace
            .active()
            .map(|p| read_project_contents(&p.root_path))
            .unwrap_or_default();
        let handle = self.find_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn close_find_panel(&mut self, cx: &mut Context<Self>) {
        self.show_find_panel = false;
        // Free the cached file contents.
        self.find_contents = Vec::new();
        cx.notify();
    }

    /// Search the cached project contents for `query` (case-insensitive
    /// substring), capped. Runs live as the user types.
    fn run_find(&mut self, query: String, cx: &mut Context<Self>) {
        self.find_results.clear();
        let q = query.trim().to_lowercase();
        if q.len() < 2 {
            self.find_selected = 0;
            cx.notify();
            return;
        }
        let mut hits = Vec::new();
        'files: for (path, content) in &self.find_contents {
            for (i, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(&q) {
                    hits.push(FindHit {
                        path: path.clone(),
                        line: i as u32,
                        text: line.trim().chars().take(200).collect(),
                    });
                    if hits.len() >= 500 {
                        break 'files;
                    }
                }
            }
        }
        self.find_results = hits;
        self.find_selected = 0;
        cx.notify();
    }

    fn move_find_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        if self.find_results.is_empty() {
            return;
        }
        let n = self.find_results.len() as i32;
        self.find_selected = (self.find_selected as i32 + delta).rem_euclid(n) as usize;
        cx.notify();
    }

    fn confirm_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.find_results.get(self.find_selected).cloned() else {
            return;
        };
        self.activate_find_hit(hit, window, cx);
    }

    fn activate_find_hit(&mut self, hit: FindHit, window: &mut Window, cx: &mut Context<Self>) {
        self.show_find_panel = false;
        let Some(pid) = self.workspace.active_project else {
            return;
        };
        let target = self.active_instance;
        if let Some(iid) = self.open_editor_at(pid, Some(hit.path.clone()), target, window, cx)
            && let Some(ed) = self.editors.get(&iid).cloned()
        {
            ed.update(cx, |e, cx| e.goto_line(hit.line, window, cx));
        }
        cx.notify();
    }

    // ===== Editor save / save-as =====

    fn active_editor(&self) -> Option<(Uuid, Entity<EditorView>)> {
        let iid = self.active_instance?;
        self.editors.get(&iid).map(|e| (iid, e.clone()))
    }

    fn save_active_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((iid, ed)) = self.active_editor() else {
            return;
        };
        if ed.read(cx).is_diff() {
            return; // diff panes are read-only
        }
        let path = ed.read(cx).path().map(|p| p.to_path_buf());
        match path {
            Some(p) => {
                let text = ed.read(cx).text(cx);
                // Remote project: write over SSH (background); mark saved + toast
                // only on success.
                let pid = self.workspace.instance(iid).map(|i| i.project_id);
                let remote_loc = pid.filter(|pid| {
                    self.workspace
                        .project(*pid)
                        .is_some_and(|pr| pr.is_remote())
                });
                if let Some(loc) = remote_loc.and_then(|pid| self.repo_loc(pid)) {
                    let abs = p.to_string_lossy().into_owned();
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    cx.spawn_in(window, async move |this, cx| {
                        let res = cx
                            .background_executor()
                            .spawn(
                                async move { integrations::write_remote_file(&loc, &abs, &text) },
                            )
                            .await;
                        let _ = this.update(cx, |this, cx| {
                            match res {
                                Ok(()) => {
                                    if let Some(ed) = this.editors.get(&iid).cloned() {
                                        ed.update(cx, |e, cx| e.mark_saved(cx));
                                    }
                                    this.add_event(
                                        NotifKind::Success,
                                        tf("Saved {name}", &[("name", &name.to_string())]),
                                        String::new(),
                                    );
                                }
                                Err(e) => this.add_event(
                                    NotifKind::Error,
                                    t("Save failed"),
                                    format!("{e}"),
                                ),
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                    return;
                }
                if let Err(e) = std::fs::write(&p, text) {
                    log::warn!("save failed for {}: {e}", p.display());
                    return;
                }
                ed.update(cx, |e, cx| e.mark_saved(cx));
                cx.notify();
            }
            None => self.save_as_active_editor(window, cx),
        }
    }

    fn save_as_active_editor(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((iid, ed)) = self.active_editor() else {
            return;
        };
        if ed.read(cx).is_diff() {
            return; // diff panes are read-only
        }
        let dir = self
            .workspace
            .active()
            .map(|p| p.root_path.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let suggested = ed
            .read(cx)
            .path()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        let rx = cx.prompt_for_new_path(&dir, suggested.as_deref());
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let _ = view.update(cx, |this, cx| {
                let Some(ed) = this.editors.get(&iid).cloned() else {
                    return;
                };
                let text = ed.read(cx).text(cx);
                if std::fs::write(&path, text).is_err() {
                    return;
                }
                ed.update(cx, |e, cx| e.set_path(path.clone(), cx));
                if let Some(inst) = this.workspace.instance_mut(iid) {
                    inst.editor_path = Some(path.clone());
                    inst.title = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Untitled".to_string());
                }
                this.persist();
                cx.notify();
            });
        })
        .detach();
    }

    /// Open the run-dialog for a runner (collect details before launching).
    fn open_run_dialog(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.runners_menu = None;
        if idx < self.runners.len() {
            self.active_runner = Some(idx);
            self.show_run_dialog = true;
            cx.notify();
        }
    }

    /// Run-dialog "Run": read the typed details and launch the active runner.
    fn execute_runner(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.active_runner else {
            return;
        };
        let details = self.runner_input.read(cx).value().trim().to_string();
        self.run_runner(idx, details, window, cx);
        self.runner_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.show_run_dialog = false;
        self.active_runner = None;
        cx.notify();
    }

    /// Launch a runner: build an instance from its preset that types the prompt
    /// (with `details` substituted for `{{input}}`, else appended) after sending
    /// the configured Shift+Tab presses, then place + spawn it.
    fn run_runner(
        &mut self,
        idx: usize,
        details: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pid) = self.workspace.active_project else {
            return;
        };
        let target = self.active_instance;
        self.run_runner_inner(idx, details, pid, target, None, window, cx);
    }

    /// Run a runner (e.g. Review) INSIDE worktree `wid`, so the agent's cwd is the
    /// worktree and it reviews that worktree's `git diff`.
    fn run_runner_in_worktree(
        &mut self,
        idx: usize,
        wid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pid) = self.workspace.worktree(wid).map(|w| w.project_id) else {
            return;
        };
        // Anchor the new pane beside one of the worktree's panes, else the active one.
        let target = self
            .workspace
            .instances_using(wid)
            .into_iter()
            .next()
            .or(self.active_instance);
        self.run_runner_inner(
            idx,
            String::new(),
            pid,
            target,
            Some(WorktreeChoice::Resume(wid)),
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn run_runner_inner(
        &mut self,
        idx: usize,
        details: String,
        pid: Uuid,
        target: Option<Uuid>,
        worktree: Option<WorktreeChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(runner) = self.runners.get(idx).cloned() else {
            return;
        };
        let preset = runner
            .preset_id
            .and_then(|id| self.presets.iter().find(|p| p.id == id).cloned())
            .unwrap_or_else(|| self.current_agent_preset());
        let prompt = if runner.prompt.contains("{{input}}") {
            runner.prompt.replace("{{input}}", &details)
        } else if details.is_empty() {
            runner.prompt.clone()
        } else {
            format!("{}\n\n{}", runner.prompt, details)
        };
        // Trim trailing blank lines (e.g. from "…{{input}}" with no details) so
        // the submit Enter lands on a clean line.
        let prompt = prompt.trim_end().to_string();
        let mut instance = Instance::from_preset(pid, &preset);
        instance.system_prompt = Some(prompt);
        instance.injection = InjectionMode::TypeIn;
        instance.auto_mode_presses = runner.auto_mode_presses;
        instance.custom_name = Some(runner.name.clone());
        // Mark as a runner so its first launch submits the prompt, but reopening
        // the app re-types it without auto-submitting (see spawn_terminal).
        instance.is_runner = true;
        self.place_and_spawn(
            pid,
            instance,
            PlacementMode::Split(SplitDirection::Horizontal),
            target,
            worktree,
            window,
            cx,
        );
    }

    /// Mouse-down on a split / new-tab button: remember the press and, after a
    /// hold, open the agent picker (anchored at `pos`) instead of placing.
    fn begin_place_press(
        &mut self,
        iid: Uuid,
        placement: PlacementMode,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.place_pending = Some((iid, placement));
        cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let _ = view.update(cx, |this, cx| {
                if this.place_pending == Some((iid, placement)) {
                    this.place_pending = None;
                    this.place_menu = Some((iid, placement, pos));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Mouse-up on a split / new-tab button: a short press (the hold timer hasn't
    /// fired) places with the current preset.
    fn end_place_press(
        &mut self,
        iid: Uuid,
        placement: PlacementMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.place_pending == Some((iid, placement)) {
            self.place_pending = None;
            self.place_with_preset(Some(iid), placement, self.current_preset, window, cx);
        }
    }

    /// Pick an agent from the picker → create the pane or tab.
    fn pick_place_agent(&mut self, preset_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((iid, placement, _)) = self.place_menu.take() {
            self.place_with_preset(Some(iid), placement, preset_idx, window, cx);
        }
        cx.notify();
    }

    /// Close a specific pane (window-free): removes it from its project layout,
    /// kills the process, tears down its tmux session + worktree, and drops the
    /// metadata. Used by auto-close-on-exit and as the core of `close_active`.
    /// Tear down a just-closed instance's tmux session (local **or** remote) and
    /// dispose its worktree. Call after `remove_instance_meta`, with the captured
    /// fields (the project meta must still exist).
    #[allow(clippy::too_many_arguments)]
    fn teardown_closed_instance(
        &mut self,
        iid: Uuid,
        project_id: Uuid,
        use_tmux: bool,
        kill_remote_session: bool,
        local_session: Option<String>,
        worktree_path: Option<PathBuf>,
        worktree_id: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        // Local tmux session.
        if let Some(session) = local_session {
            integrations::kill_tmux_session(&session);
        }
        // Remote tmux session: a remote project whose pane ran in tmux. Only on a
        // deliberate close — NOT when the process merely exited (a dropped SSH
        // connection should leave the session alive for reconnect). Killed over
        // ssh in the background (reuses the host's still-warm ControlMaster).
        let remote_host = kill_remote_session
            .then(|| {
                self.workspace
                    .project(project_id)
                    .and_then(|p| p.remote.clone())
                    .and_then(|r| self.remotes.iter().find(|h| h.id == r.host_id).cloned())
                    .filter(|host| host.default_use_tmux || use_tmux)
            })
            .flatten();
        if let Some(host) = remote_host {
            let session = muxel_core::tmux::session_name(&host.name, iid);
            let control_path = Self::control_path_for(host.id);
            let password = (host.auth == SshAuth::Password)
                .then(|| self.remote_password(host.id))
                .flatten();
            cx.background_executor()
                .spawn(async move {
                    integrations::kill_remote_tmux(
                        &host,
                        &control_path,
                        password.as_deref(),
                        &session,
                    );
                })
                .detach();
        }
        // Worktree disposed only when its last instance is gone.
        let root = self
            .workspace
            .project(project_id)
            .map(|p| p.root_path.clone());
        if let Some(wid) = worktree_id {
            self.dispose_worktree_if_orphaned(wid, cx);
        } else if let (Some(path), Some(root)) = (worktree_path, root) {
            // Legacy instance (no worktree_id): old direct-removal behavior.
            integrations::remove_worktree(&root, &path);
        }
    }

    /// Manually close an instance (kills its tmux session, local or remote).
    fn close_instance(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        self.close_instance_inner(iid, true, cx);
    }

    /// Close an instance. `kill_remote_session` is false for auto-close-on-exit,
    /// so a dropped remote connection doesn't tear down a still-running session.
    fn close_instance_inner(
        &mut self,
        iid: Uuid,
        kill_remote_session: bool,
        cx: &mut Context<Self>,
    ) {
        self.clear_notifications_for(iid);
        let pid = self.workspace.instance(iid).map(|i| i.project_id);
        // If `iid` is one of several tabs in its pane, which tab survives as
        // active (so we can re-target focus there instead of jumping panes).
        let survivor = pid.and_then(|pid| {
            self.workspace
                .project(pid)
                .and_then(|p| p.layout.as_ref())
                .and_then(|l| l.surviving_active_after_remove(iid))
        });
        if let Some(pid) = pid
            && let Some(project) = self.workspace.project_mut(pid)
        {
            remove(&mut project.layout, iid);
        }
        if let Some(view) = self.terminals.remove(&iid) {
            view.read(cx).session().kill();
        }
        // Editors just drop (their buffer is in memory); nothing to kill.
        self.editors.remove(&iid);

        // Tear down tmux (local or remote) + worktree (capture info before drop).
        let info = self.workspace.instance(iid).map(|i| {
            (
                i.tmux_session.clone(),
                i.worktree_path.clone(),
                i.worktree_id,
                i.project_id,
                i.use_tmux,
            )
        });
        self.workspace.remove_instance_meta(iid);
        if let Some((local_session, worktree_path, worktree_id, project_id, use_tmux)) = info {
            self.teardown_closed_instance(
                iid,
                project_id,
                use_tmux,
                kill_remote_session,
                local_session,
                worktree_path,
                worktree_id,
                cx,
            );
        }
        self.last_status.remove(&iid);
        if self.maximized == Some(iid) {
            self.maximized = None;
        }

        // If the closed tab was active, retarget to its pane's surviving tab if
        // the pane lives on, else the active project's first instance.
        if self.active_instance == Some(iid) {
            self.active_instance =
                survivor.or_else(|| self.workspace.active().and_then(|p| p.first_instance()));
        }
        self.persist();
        cx.notify();
    }

    /// When `wid`'s last instance has closed: silently remove a fully-landed
    /// worktree (clean tree + nothing unmerged); otherwise queue the dispose modal
    /// so uncommitted changes / unmerged commits aren't lost silently.
    fn dispose_worktree_if_orphaned(&mut self, wid: Uuid, cx: &mut Context<Self>) {
        if !self.workspace.instances_using(wid).is_empty() {
            return;
        }
        let Some(w) = self.workspace.worktree(wid) else {
            return;
        };
        let path = w.path.clone();
        let name = w.name.clone();
        let color = w.color;
        let branch = w.branch.clone();
        let root = self
            .workspace
            .project(w.project_id)
            .map(|p| p.root_path.clone())
            .unwrap_or_default();
        let changed = integrations::worktree_change_count(&path);
        let unmerged = integrations::repo_head(&root).map_or(0, |base| {
            integrations::worktree_unmerged_count(&path, &base)
        });
        if changed == 0 && unmerged == 0 {
            // Fully landed: remove the worktree and its (empty) branch.
            integrations::remove_worktree(&root, &path);
            integrations::delete_branch(&root, &branch);
            self.workspace.remove_worktree_meta(wid);
            self.persist();
            return;
        }
        let base_label =
            integrations::repo_current_branch(&integrations::RepoLoc::Local(root.clone()))
                .unwrap_or_else(|| "base".to_string());
        self.pending_worktree_dispose.push_back(WorktreeDispose {
            wid,
            name,
            color,
            path,
            root,
            branch,
            changed,
            unmerged,
            base_label,
        });
        cx.notify();
    }

    /// Manually re-open the dispose modal for an existing (kept) worktree so the
    /// user can commit / merge / discard it from the sidebar. A fully-landed
    /// worktree (clean + merged) is just removed.
    fn review_worktree(&mut self, wid: Uuid, cx: &mut Context<Self>) {
        if self.pending_worktree_dispose.iter().any(|d| d.wid == wid) {
            return; // already queued
        }
        let Some(w) = self.workspace.worktree(wid) else {
            return;
        };
        let path = w.path.clone();
        let name = w.name.clone();
        let color = w.color;
        let branch = w.branch.clone();
        let root = self
            .workspace
            .project(w.project_id)
            .map(|p| p.root_path.clone())
            .unwrap_or_default();
        let changed = integrations::worktree_change_count(&path);
        let unmerged = integrations::repo_head(&root).map_or(0, |base| {
            integrations::worktree_unmerged_count(&path, &base)
        });
        if changed == 0 && unmerged == 0 {
            integrations::remove_worktree(&root, &path);
            integrations::delete_branch(&root, &branch);
            self.workspace.remove_worktree_meta(wid);
            self.persist();
            cx.notify();
            return;
        }
        let base_label =
            integrations::repo_current_branch(&integrations::RepoLoc::Local(root.clone()))
                .unwrap_or_else(|| "base".to_string());
        self.pending_worktree_dispose.push_back(WorktreeDispose {
            wid,
            name,
            color,
            path,
            root,
            branch,
            changed,
            unmerged,
            base_label,
        });
        cx.notify();
    }

    /// Commit the front pending worktree (message = the input, or its name), then
    /// remove it. On commit failure, keep it on disk so no work is lost.
    fn dispose_worktree_commit(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.pending_worktree_dispose.pop_front() else {
            return;
        };
        let typed = self
            .dispose_commit_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        let msg = if typed.is_empty() {
            d.name.clone()
        } else {
            typed
        };
        match integrations::git_commit(&integrations::RepoLoc::Local(d.path.clone()), &msg) {
            Ok(_) => {
                integrations::remove_worktree(&d.root, &d.path);
                self.workspace.remove_worktree_meta(d.wid);
            }
            Err(e) => {
                log::warn!("worktree commit failed, keeping it: {e}");
                if let Some(w) = self.workspace.worktree_mut(d.wid) {
                    w.detached = true;
                }
            }
        }
        self.persist();
        cx.notify();
    }

    /// Discard the front pending worktree: force-remove it AND delete its branch,
    /// throwing away both uncommitted changes and any commits.
    fn dispose_worktree_discard(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.pending_worktree_dispose.pop_front() else {
            return;
        };
        integrations::remove_worktree(&d.root, &d.path);
        integrations::delete_branch(&d.root, &d.branch);
        self.workspace.remove_worktree_meta(d.wid);
        self.persist();
        cx.notify();
    }

    /// Merge the front pending worktree's branch into the base, then remove the
    /// worktree + its (now-merged) branch. Commits any uncommitted changes first.
    /// On merge failure (e.g. conflicts) keep the worktree and toast the error.
    fn dispose_worktree_merge(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.pending_worktree_dispose.pop_front() else {
            return;
        };
        // Land any uncommitted changes onto the branch first, so the merge
        // includes them. On failure, keep the worktree (no work lost).
        if d.changed > 0 {
            let typed = self
                .dispose_commit_input
                .read(cx)
                .value()
                .trim()
                .to_string();
            let msg = if typed.is_empty() {
                d.name.clone()
            } else {
                typed
            };
            if let Err(e) =
                integrations::git_commit(&integrations::RepoLoc::Local(d.path.clone()), &msg)
            {
                log::warn!("worktree commit failed, keeping it: {e}");
                if let Some(w) = self.workspace.worktree_mut(d.wid) {
                    w.detached = true;
                }
                self.persist();
                cx.notify();
                return;
            }
        }
        match integrations::merge_worktree_branch(&d.root, &d.branch) {
            Ok(()) => {
                integrations::remove_worktree(&d.root, &d.path);
                integrations::delete_branch(&d.root, &d.branch);
                self.workspace.remove_worktree_meta(d.wid);
            }
            Err(e) => {
                log::warn!("worktree merge failed, keeping it: {e}");
                if let Some(w) = self.workspace.worktree_mut(d.wid) {
                    w.detached = true;
                }
                self.add_event(
                    NotifKind::Error,
                    tf("Couldn't merge {name}", &[("name", &d.name.to_string())]),
                    tf("{e} — the worktree was kept.", &[("e", &e.to_string())]),
                );
            }
        }
        self.persist();
        cx.notify();
    }

    /// Keep the front pending worktree on disk (detached, resumable later).
    fn dispose_worktree_keep(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.pending_worktree_dispose.pop_front() else {
            return;
        };
        if let Some(w) = self.workspace.worktree_mut(d.wid) {
            w.detached = true;
        }
        self.persist();
        cx.notify();
    }

    /// Run a (possibly slow) git/gh operation off the main thread, toasting the
    /// result.
    fn run_git_task<F>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        ok: String,
        err_title: String,
        op: F,
    ) where
        F: FnOnce() -> anyhow::Result<String> + Send + 'static,
    {
        cx.spawn_in(window, async move |this: WeakEntity<Self>, cx| {
            let result = cx.background_executor().spawn(async move { op() }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(out) => this.add_event(NotifKind::Success, ok, git_notify_detail(&out)),
                    Err(e) => this.add_event(NotifKind::Error, err_title, format!("{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Push a worktree's branch to `origin`.
    fn worktree_push_branch(&mut self, wid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(w) = self.workspace.worktree(wid) else {
            return;
        };
        let (path, branch, name) = (w.path.clone(), w.branch.clone(), w.name.clone());
        self.run_git_task(
            window,
            cx,
            tf("Pushed “{name}”", &[("name", &name.to_string())]),
            tf("Couldn't push “{name}”", &[("name", &name.to_string())]),
            move || integrations::push_branch(&path, &branch).map(|()| String::new()),
        );
    }

    /// Push a worktree's branch and open the PR-create page in a browser.
    fn worktree_create_pr(&mut self, wid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(w) = self.workspace.worktree(wid) else {
            return;
        };
        let (path, branch, name) = (w.path.clone(), w.branch.clone(), w.name.clone());
        self.run_git_task(
            window,
            cx,
            tf("Opening a PR for “{name}”…", &[("name", &name.to_string())]),
            tf(
                "Couldn't create a PR for “{name}”",
                &[("name", &name.to_string())],
            ),
            move || integrations::create_pr(&path, &branch).map(|()| String::new()),
        );
    }

    /// Open a worktree branch's existing PR in a browser.
    fn worktree_open_pr(&mut self, wid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(w) = self.workspace.worktree(wid) else {
            return;
        };
        let (path, name) = (w.path.clone(), w.name.clone());
        self.run_git_task(
            window,
            cx,
            tf(
                "Opening the PR for “{name}”…",
                &[("name", &name.to_string())],
            ),
            tf("No PR found for “{name}”", &[("name", &name.to_string())]),
            move || integrations::open_pr(&path).map(|()| String::new()),
        );
    }

    /// Run a git op on a project's repo (background + toast).
    /// The git location for a project: remote (over its host's SSH, reusing the
    /// ControlMaster) when the project is remote, else local.
    fn repo_loc(&self, pid: Uuid) -> Option<integrations::RepoLoc> {
        let p = self.workspace.project(pid)?;
        match &p.remote {
            Some(r) => {
                let host = self.remotes.iter().find(|h| h.id == r.host_id)?.clone();
                let password = (host.auth == SshAuth::Password)
                    .then(|| self.remote_password(host.id))
                    .flatten();
                Some(integrations::RepoLoc::remote(
                    host,
                    r.remote_root.clone(),
                    Self::control_path_for(r.host_id),
                    password,
                ))
            }
            None => Some(integrations::RepoLoc::Local(p.root_path.clone())),
        }
    }

    /// Toggle shared project memory; on enable, create the `.muxel/MEMORY.md` +
    /// gitignore entry. Agents launched into the project then get the memory
    /// instruction (see `command_for`).
    fn toggle_project_memory(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        let Some(p) = self.workspace.project_mut(pid) else {
            return;
        };
        p.memory_enabled = !p.memory_enabled;
        let enabled = p.memory_enabled;
        if enabled {
            self.memory_ensured.remove(&pid);
            self.ensure_project_memory(pid, cx);
        }
        self.persist();
        cx.notify();
    }

    /// Open the project's shared `.muxel/MEMORY.md` in the editor (local or remote;
    /// `open_editor_at` handles fetching remote contents over SSH).
    fn open_project_memory(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.workspace.project(pid) else {
            return;
        };
        let root = match &p.remote {
            Some(r) => r.remote_root.clone(),
            None => p.root_path.display().to_string(),
        };
        let path = PathBuf::from(format!("{root}/{MEMORY_DIR}/{MEMORY_FILE}"));
        let target = self.active_instance;
        self.open_editor_at(pid, Some(path), target, window, cx);
    }

    /// Ensure the project's `.muxel/MEMORY.md` + `.gitignore` entry exist, off the
    /// UI thread (the remote variant does an SSH round-trip).
    fn ensure_project_memory(&self, pid: Uuid, cx: &mut Context<Self>) {
        let Some(loc) = self.repo_loc(pid) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { integrations::ensure_memory_file(&loc) })
                .await;
            if let Err(e) = res {
                let _ = this.update(cx, |this, cx| {
                    this.add_event(NotifKind::Error, t("Project memory"), format!("{e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Remote-layout sync heartbeat, driven from `tick()`. For every remote project
    /// already reconciled this session, detect a real layout change (by content,
    /// ignoring timestamps), stamp a new version, and schedule a debounced push;
    /// then fire any push whose debounce window has elapsed.
    fn tick_remote_sync(&mut self, cx: &mut Context<Self>) {
        if self.remote_synced.is_empty() {
            return;
        }
        let now_epoch = chrono::Local::now().timestamp().max(0) as u64;
        let now = Instant::now();
        let synced: Vec<Uuid> = self.remote_synced.iter().copied().collect();
        let mut changed = false;
        for pid in synced {
            let Some(proj) = self.workspace.project(pid) else {
                continue;
            };
            if proj.remote.is_none() {
                continue;
            }
            let key = RemoteLayout::capture(proj, &self.workspace, now_epoch).content_key();
            if self.layout_keys.get(&pid) != Some(&key) {
                self.layout_keys.insert(pid, key);
                if let Some(p) = self.workspace.project_mut(pid) {
                    p.layout_updated_at = Some(now_epoch);
                }
                // Debounce: each fresh change pushes the deadline ~2s out.
                self.remote_push_due
                    .insert(pid, now + Duration::from_secs(2));
                changed = true;
            }
        }
        if changed {
            self.persist();
        }
        let due: Vec<Uuid> = self
            .remote_push_due
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(pid, _)| *pid)
            .collect();
        for pid in due {
            self.remote_push_due.remove(&pid);
            self.push_remote_layout_now(pid, cx);
        }
    }

    /// Push a remote project's current pane layout to `<root>/.muxel/workspace.json`
    /// off the UI thread (backs up the previous remote copy first).
    fn push_remote_layout_now(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        let Some(loc) = self.repo_loc(pid) else {
            return;
        };
        if !matches!(loc, integrations::RepoLoc::Remote(_)) {
            return;
        }
        let now_epoch = chrono::Local::now().timestamp().max(0) as u64;
        let Some(proj) = self.workspace.project(pid) else {
            return;
        };
        let json = RemoteLayout::capture(proj, &self.workspace, now_epoch).to_json();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { integrations::push_remote_layout(&loc, &json) })
                .await;
            if let Err(e) = res {
                let _ = this.update(cx, |this, cx| {
                    this.add_event(NotifKind::Error, t("Layout sync"), format!("{e}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Decide the connect-time sync direction for a remote project and apply it:
    /// pull a strictly-newer remote layout (backing up the local copy), or schedule
    /// a push when the local copy is newer / the remote has none, or do nothing when
    /// they already match. Marks the project reconciled for this session.
    fn apply_remote_layout_sync(
        &mut self,
        pid: Uuid,
        fetched: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remote_synced.insert(pid);
        let now_epoch = chrono::Local::now().timestamp().max(0) as u64;
        let Some(proj) = self.workspace.project(pid) else {
            return;
        };
        let Some(remote_root) = proj.remote.as_ref().map(|r| r.remote_root.clone()) else {
            return;
        };
        let local = RemoteLayout::capture(proj, &self.workspace, now_epoch);
        let local_key = local.content_key();
        let local_rev = proj.layout_updated_at.unwrap_or(0);
        let remote = fetched
            .as_deref()
            .and_then(|j| RemoteLayout::parse(j, &remote_root));

        match remote {
            // Remote is strictly newer and actually different → adopt it.
            Some(r) if r.updated_at > local_rev && r.content_key() != local_key => {
                self.pull_remote_layout(pid, local, r, window, cx);
            }
            // Already in sync → just arm change detection.
            Some(r) if r.content_key() == local_key => {
                self.layout_keys.insert(pid, local_key);
            }
            // Local is newer, or there's no usable remote doc → push local up.
            _ => {
                self.layout_keys.insert(pid, local_key);
                self.remote_push_due.insert(pid, Instant::now());
            }
        }
    }

    /// Adopt a newer remote layout: back up the local copy, tear down the project's
    /// current local views (the remote tmux sessions / worktrees survive and are
    /// re-attached on respawn), then swap in the remote layout/instances/worktrees
    /// remapped to this project. The caller respawns terminals afterwards.
    fn pull_remote_layout(
        &mut self,
        pid: Uuid,
        local: RemoteLayout,
        remote: RemoteLayout,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.backup_local_layout(pid, &local, remote.updated_at);

        // Light teardown: drop the local views (kill the ssh client / local PTY),
        // but don't mutate the layout or dispose worktrees — we replace wholesale.
        for iid in self
            .workspace
            .project(pid)
            .map(|p| p.instances())
            .unwrap_or_default()
        {
            if let Some(view) = self.terminals.remove(&iid) {
                view.read(cx).session().kill();
            }
            self.editors.remove(&iid);
            self.last_status.remove(&iid);
            self.workspace.remove_instance_meta(iid);
        }

        let RemoteLayout {
            layout,
            mut instances,
            mut worktrees,
            updated_at,
            ..
        } = remote;
        for inst in &mut instances {
            inst.project_id = pid;
        }
        for wt in &mut worktrees {
            wt.project_id = pid;
        }
        // Replace worktrees by id (a re-pull refreshes them), then add instances.
        for wt in worktrees {
            self.workspace.remove_worktree_meta(wt.id);
            self.workspace.add_worktree(wt);
        }
        for inst in instances {
            self.workspace.add_instance(inst);
        }
        if let Some(p) = self.workspace.project_mut(pid) {
            p.layout = layout;
            p.layout_updated_at = Some(updated_at);
        }
        // Re-seed change detection so the adoption itself isn't seen as a change.
        let now_epoch = chrono::Local::now().timestamp().max(0) as u64;
        if let Some(p) = self.workspace.project(pid) {
            let key = RemoteLayout::capture(p, &self.workspace, now_epoch).content_key();
            self.layout_keys.insert(pid, key);
        }
        self.active_instance = self.workspace.project(pid).and_then(|p| p.first_instance());
        self.persist();
        self.add_event(
            NotifKind::Success,
            t("Layout restored from remote").to_string(),
            String::new(),
        );
    }

    /// Save the local layout being replaced to `<workspace>/backups/<pid>-<ts>.json`
    /// so a newer-remote pull can't silently lose work. Best-effort.
    fn backup_local_layout(&self, pid: Uuid, local: &RemoteLayout, ts: u64) {
        let Some(workspace) = self.current_workspace else {
            return;
        };
        let Some(dir) = muxel_store::workspace_doc_path(workspace)
            .and_then(|p| p.parent().map(|d| d.join("backups")))
        else {
            return;
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let _ = std::fs::write(
            dir.join(format!("{}-{}.json", pid.simple(), ts)),
            local.to_json(),
        );
    }

    fn run_project_git<F>(
        &mut self,
        pid: Uuid,
        ok: String,
        err: String,
        op: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(&integrations::RepoLoc) -> anyhow::Result<String> + Send + 'static,
    {
        let Some(loc) = self.repo_loc(pid) else {
            return;
        };
        self.run_git_task(window, cx, ok, err, move || op(&loc));
    }

    /// Check out an existing branch — warns first if the working tree is dirty
    /// (where switching can fail or move changes).
    fn switch_branch(
        &mut self,
        pid: Uuid,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dirty = self
            .workspace
            .project(pid)
            .is_some_and(|p| integrations::worktree_change_count(&p.root_path) > 0);
        if dirty {
            self.request_confirm(
                t("Switch branch?"),
                format!(
                    "You have uncommitted changes — switching to “{branch}” may fail or carry \
                     them over."
                ),
                t("Switch"),
                ConfirmAction::SwitchBranch { pid, branch },
                cx,
            );
        } else {
            self.do_switch_branch(pid, branch, window, cx);
        }
    }

    fn do_switch_branch(
        &mut self,
        pid: Uuid,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let b = branch.clone();
        self.run_project_git(
            pid,
            tf("Switched to {branch}", &[("branch", &branch.to_string())]),
            t("Couldn't switch branch").into(),
            move |root| integrations::checkout_branch(root, &b),
            window,
            cx,
        );
    }

    fn request_stash_pop(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        self.request_confirm(
            t("Pop stash?"),
            t("Applying the latest stash can conflict with your working tree."),
            t("Pop"),
            ConfirmAction::StashPop(pid),
            cx,
        );
    }

    fn do_stash_pop(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.run_project_git(
            pid,
            t("Popped stash").into(),
            t("Pop stash failed").into(),
            integrations::git_stash_pop,
            window,
            cx,
        );
    }

    fn request_stash_drop(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        self.request_confirm(
            t("Drop stash?"),
            t("The latest stash will be permanently discarded."),
            t("Drop"),
            ConfirmAction::StashDrop(pid),
            cx,
        );
    }

    fn do_stash_drop(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.run_project_git(
            pid,
            t("Dropped stash").into(),
            t("Drop stash failed").into(),
            integrations::git_stash_drop,
            window,
            cx,
        );
    }

    /// Open the single-input git modal (commit message / new branch name). For a
    /// commit it first lists every changed/untracked file (all checked by default)
    /// so the user can review and uncheck before committing; if the tree is clean
    /// it shows a toast and opens nothing.
    fn open_git_modal(
        &mut self,
        pid: Uuid,
        kind: GitModalKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (files, selected) = match kind {
            GitModalKind::Commit => {
                let files = self
                    .repo_loc(pid)
                    .map(|loc| integrations::git_status_files(&loc))
                    .unwrap_or_default();
                if files.is_empty() {
                    self.add_event(NotifKind::Success, t("Nothing to commit"), "");
                    cx.notify();
                    return;
                }
                let n = files.len();
                (files, vec![true; n])
            }
            GitModalKind::NewBranch => (Vec::new(), Vec::new()),
        };
        self.git_modal = Some(GitModal {
            pid,
            kind,
            files,
            selected,
        });
        self.git_action_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        let handle = self.git_action_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Toggle whether the file at `idx` in the open commit modal is included.
    fn toggle_git_file(&mut self, idx: usize, cx: &mut Context<Self>) {
        if let Some(sel) = self
            .git_modal
            .as_mut()
            .and_then(|m| m.selected.get_mut(idx))
        {
            *sel = !*sel;
            cx.notify();
        }
    }

    fn close_git_modal(&mut self, cx: &mut Context<Self>) {
        self.git_modal = None;
        cx.notify();
    }

    fn confirm_git_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Validate against the still-open modal first, so an empty message or an
        // empty file selection leaves it open for the user to fix.
        let Some(m) = self.git_modal.as_ref() else {
            return;
        };
        let value = self.git_action_input.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        let (pid, kind) = (m.pid, m.kind);
        let commit_paths: Vec<String> = match kind {
            GitModalKind::Commit => {
                let paths: Vec<String> = m
                    .files
                    .iter()
                    .zip(&m.selected)
                    .filter(|(_, checked)| **checked)
                    .map(|(f, _)| f.path.clone())
                    .collect();
                if paths.is_empty() {
                    return;
                }
                paths
            }
            GitModalKind::NewBranch => Vec::new(),
        };

        self.git_modal = None;
        match kind {
            GitModalKind::Commit => self.run_project_git(
                pid,
                t("Committed").into(),
                t("Commit failed").into(),
                move |root| integrations::git_commit_paths(root, &value, &commit_paths),
                window,
                cx,
            ),
            GitModalKind::NewBranch => self.run_project_git(
                pid,
                tf("Created branch {value}", &[("value", &value.to_string())]),
                t("Couldn't create branch").into(),
                move |root| integrations::create_branch(root, &value),
                window,
                cx,
            ),
        }
        cx.notify();
    }

    /// The project git modal (commit message / new branch name).
    /// The new-remote-project wizard modal: pick a host, enter the remote
    /// directory + a name, optionally verify, then create.
    fn render_remote_project_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let card = div()
            .w(px(460.0))
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius_lg)
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .child(t("New remote project")),
            );

        let card = if self.remotes.is_empty() {
            card.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(
                        "No SSH hosts yet. Add one in Settings → Remotes, then come back.",
                    )),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("nr-cancel")
                            .ghost()
                            .label(t("Cancel"))
                            .on_click(
                                cx.listener(|this, _e, _w, cx| this.close_remote_project_modal(cx)),
                            ),
                    )
                    .child(
                        Button::new("nr-open-settings")
                            .primary()
                            .label(t("Open Settings"))
                            .on_click(cx.listener(|this, _e, window, cx| {
                                this.close_remote_project_modal(cx);
                                if !this.show_settings {
                                    this.toggle_settings(window, cx);
                                }
                                this.set_section(SettingsSection::Remotes, cx);
                            })),
                    ),
            )
        } else {
            let mut hosts = div().flex().flex_wrap().gap_1();
            for h in &self.remotes {
                let id = h.id;
                let label = if h.name.is_empty() {
                    h.hostname.clone()
                } else {
                    h.name.clone()
                };
                hosts = hosts.child(
                    Button::new(SharedString::from(format!("nr-host-{}", id.simple())))
                        .ghost()
                        .selected(self.nr_host == Some(id))
                        .icon(IconName::Network)
                        .label(label)
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.nr_host = Some(id);
                            cx.notify();
                        })),
                );
            }
            card.child(self.settings_label(&t("Host"), cx))
                .child(hosts)
                .child(self.settings_label(&t("Remote directory"), cx))
                .child(Input::new(&self.nr_dir))
                .child(self.settings_label(&t("Project name (optional)"), cx))
                .child(Input::new(&self.nr_name))
                // Inline Verify result, above the buttons.
                .children(match &self.nr_verify {
                    RemoteTestState::Idle => None,
                    RemoteTestState::Testing => Some(
                        div()
                            .pt_1()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t("Verifying…"))
                            .into_any_element(),
                    ),
                    RemoteTestState::Ok(msg) => Some(
                        div()
                            .pt_1()
                            .text_xs()
                            .text_color(cx.theme().success)
                            .child(format!("✓ {msg}"))
                            .into_any_element(),
                    ),
                    RemoteTestState::Failed(msg) => Some(
                        div()
                            .pt_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(cx.theme().danger)
                            .child(format!("✗ {msg}"))
                            .into_any_element(),
                    ),
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .pt_2()
                        .child(
                            Button::new("nr-verify")
                                .ghost()
                                .label(t("Verify"))
                                .on_click(cx.listener(|this, _e, window, cx| {
                                    this.verify_remote_dir(window, cx)
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("nr-cancel")
                                .ghost()
                                .label(t("Cancel"))
                                .on_click(cx.listener(|this, _e, _w, cx| {
                                    this.close_remote_project_modal(cx)
                                })),
                        )
                        .child(
                            Button::new("nr-create")
                                .primary()
                                .label(t("Create"))
                                .on_click(cx.listener(|this, _e, window, cx| {
                                    this.confirm_remote_project(window, cx)
                                })),
                        ),
                )
        };

        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0099))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| this.close_remote_project_modal(cx)),
            )
            .child(card)
            .into_any_element()
    }

    /// The SSH password prompt (for a host with no saved password).
    fn render_password_prompt(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(p) = &self.password_prompt else {
            return div().into_any_element();
        };
        let host_name = self
            .remotes
            .iter()
            .find(|h| h.id == p.host_id)
            .map(|h| h.name.clone())
            .unwrap_or_default();
        let (confirm, hint) = match p.action {
            PasswordAction::Connect(_) => (
                t("Connect"),
                t("Kept in memory for this session only — not saved to the keychain."),
            ),
            PasswordAction::Verify(_) => (t("Test"), t("Used once to test, then forgotten.")),
        };
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0099))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, window, cx| this.close_password_prompt(window, cx)),
            )
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(div().text_lg().font_semibold().child(tf(
                        "SSH password for “{host_name}”",
                        &[("host_name", &host_name.to_string())],
                    )))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(hint),
                    )
                    .child(Input::new(&self.password_prompt_input))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(
                                Button::new("pw-cancel")
                                    .ghost()
                                    .label(t("Cancel"))
                                    .on_click(cx.listener(|this, _e, window, cx| {
                                        this.close_password_prompt(window, cx)
                                    })),
                            )
                            .child(Button::new("pw-confirm").primary().label(confirm).on_click(
                                cx.listener(|this, _e, window, cx| {
                                    this.confirm_password_prompt(window, cx)
                                }),
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_git_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(m) = &self.git_modal else {
            return div().into_any_element();
        };
        let (title, label) = match m.kind {
            GitModalKind::Commit => (t("Commit changes"), t("Commit message")),
            GitModalKind::NewBranch => (t("New branch"), t("Branch name")),
        };
        let confirm = match m.kind {
            GitModalKind::Commit => tf(
                "Commit ({count})",
                &[(
                    "count",
                    &m.selected.iter().filter(|&&s| s).count().to_string(),
                )],
            ),
            GitModalKind::NewBranch => "Create".to_string(),
        };
        // For a commit, a scrollable checklist of every changed/untracked file
        // (checked = will be committed), so nothing is staged without the user
        // seeing it.
        let file_list = matches!(m.kind, GitModalKind::Commit).then(|| {
            let mut list = div()
                .id("git-commit-files")
                .flex()
                .flex_col()
                .gap_1()
                .max_h(px(220.0))
                .overflow_y_scroll();
            for (i, f) in m.files.iter().enumerate() {
                let checked = m.selected.get(i).copied().unwrap_or(false);
                let row = match &f.orig {
                    Some(orig) => format!("{}  {} → {}", f.status.trim(), orig, f.path),
                    None => format!("{}  {}", f.status.trim(), f.path),
                };
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Checkbox::new(SharedString::from(format!("git-file-{i}")))
                                .checked(checked)
                                .on_click(cx.listener(move |this, _c: &bool, _w, cx| {
                                    this.toggle_git_file(i, cx)
                                })),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(row),
                        ),
                );
            }
            list
        });
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0099))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| this.close_git_modal(cx)),
            )
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(div().text_lg().font_semibold().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(label),
                    )
                    .child(Input::new(&self.git_action_input).w_full())
                    .children(file_list)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(
                                Button::new("git-cancel")
                                    .ghost()
                                    .label(t("Cancel"))
                                    .on_click(
                                        cx.listener(|this, _e, _w, cx| this.close_git_modal(cx)),
                                    ),
                            )
                            .child(
                                Button::new("git-confirm")
                                    .primary()
                                    .label(confirm)
                                    .on_click(cx.listener(|this, _e, window, cx| {
                                        this.confirm_git_modal(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Throw away everything an agent did in worktree `wid` (uncommitted + its
    /// commits) by resetting it to the base, keeping the worktree + its panes.
    fn discard_worktree_changes(&mut self, wid: Uuid, cx: &mut Context<Self>) {
        let Some(w) = self.workspace.worktree(wid) else {
            return;
        };
        let path = w.path.clone();
        let root = self
            .workspace
            .project(w.project_id)
            .map(|p| p.root_path.clone())
            .unwrap_or_default();
        let Some(base) = integrations::repo_head(&root) else {
            log::warn!(
                "discard changes: couldn't resolve base for {}",
                root.display()
            );
            return;
        };
        if let Err(e) = integrations::discard_worktree_changes(&path, &base) {
            log::warn!("discard worktree changes failed: {e}");
        }
        self.worktree_changes.insert(wid, 0);
        cx.notify();
    }

    /// Remove worktree `wid` entirely: close its panes, delete the worktree + its
    /// branch, drop the registry entry.
    fn discard_worktree(&mut self, wid: Uuid, cx: &mut Context<Self>) {
        let Some(w) = self.workspace.worktree(wid) else {
            return;
        };
        let path = w.path.clone();
        let branch = w.branch.clone();
        let root = self
            .workspace
            .project(w.project_id)
            .map(|p| p.root_path.clone())
            .unwrap_or_default();
        let instances = self.workspace.instances_using(wid);
        // Drop the registry entry first so each close skips the dispose prompt
        // (dispose_worktree_if_orphaned no-ops when the worktree is gone).
        self.workspace.remove_worktree_meta(wid);
        for iid in instances {
            self.close_instance(iid, cx);
        }
        integrations::remove_worktree(&root, &path);
        integrations::delete_branch(&root, &branch);
        self.worktree_changes.remove(&wid);
        self.persist();
        cx.notify();
    }

    /// Toggle a terminal filling the pane area (transient; not persisted).
    fn toggle_maximize(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        self.maximized = (self.maximized != Some(iid)).then_some(iid);
        cx.notify();
    }

    /// Detach a pane into its own OS window (kept alive). Closing that window
    /// re-docks it (see `redock_popout`).
    fn pop_out_instance(&mut self, iid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.workspace.instance(iid).map(|i| i.project_id) else {
            return;
        };
        // Remember the original spot before removing, so Dock lands it back here.
        let redock = compute_redock_anchor(
            &self.workspace.project(pid).and_then(|p| p.layout.clone()),
            iid,
        );
        // Remove from the pane tree but KEEP the instance metadata + the view.
        if let Some(project) = self.workspace.project_mut(pid) {
            remove(&mut project.layout, iid);
        }
        // Detach the live view, capturing what's needed to rebuild it elsewhere.
        let content = if let Some(view) = self.terminals.remove(&iid) {
            PopoutContent::Terminal(view)
        } else if let Some(ed) = self.editors.remove(&iid) {
            PopoutContent::Editor(EditorSnapshot::capture(&ed, cx))
        } else {
            self.redock_into_layout(iid, pid, redock, cx);
            return;
        };
        if self.maximized == Some(iid) {
            self.maximized = None;
        }
        if self.active_instance == Some(iid) {
            self.active_instance = self.workspace.active().and_then(|p| p.first_instance());
        }

        let title = self
            .workspace
            .instance(iid)
            .map(|i| i.custom_name.clone().unwrap_or_else(|| i.title.clone()))
            .unwrap_or_else(|| "muxel".to_string());

        // The PaneView is built inside the window closure (so its input focus
        // binds to the pop-out window); a slot hands it back out for storage.
        let config = self.editor_config();
        let slot: std::rc::Rc<std::cell::RefCell<Option<PaneView>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let opened = cx.open_window(
            gpui::WindowOptions {
                titlebar: Some(gpui_component::TitleBar::title_bar_options()),
                window_min_size: Some(size(px(360.0), px(240.0))),
                ..Default::default()
            },
            {
                let slot = slot.clone();
                let config = config.clone();
                // Clone the content into the closure; the original is kept for the
                // failure path below.
                let content = match &content {
                    PopoutContent::Terminal(v) => PopoutContent::Terminal(v.clone()),
                    PopoutContent::Editor(s) => PopoutContent::Editor(s.clone()),
                };
                move |window, cx| {
                    window.set_window_title(&title);
                    let pane = match content {
                        PopoutContent::Terminal(view) => PaneView::Terminal(view),
                        PopoutContent::Editor(snap) => {
                            PaneView::Editor(snap.build(config, window, cx))
                        }
                    };
                    let fh = pane.focus_handle(cx);
                    window.focus(&fh, cx);
                    *slot.borrow_mut() = Some(pane.clone());
                    let popout = cx.new(|cx| PopoutView::new(pane, iid, cx));
                    cx.new(|cx| {
                        gpui_component::Root::new(popout, window, cx).bg(cx.theme().background)
                    })
                }
            },
        );
        match opened {
            Ok(handle) => {
                if let Some(view) = slot.borrow_mut().take() {
                    self.popouts.insert(
                        iid,
                        PopOut {
                            view,
                            window: handle,
                            redock,
                        },
                    );
                }
                self.persist();
                cx.notify();
            }
            Err(e) => {
                // Could not open a window — rebuild the pane so it isn't lost.
                log::warn!("pop-out failed: {e}");
                match content {
                    PopoutContent::Terminal(view) => {
                        self.terminals.insert(iid, view);
                    }
                    PopoutContent::Editor(snap) => {
                        let ed = snap.build(config, window, cx);
                        self.editors.insert(iid, ed);
                    }
                }
                self.redock_into_layout(iid, pid, redock, cx);
            }
        }
    }

    /// Move a popped-out terminal back into the main pane area. Called from the
    /// pop-out window's Dock button BEFORE it closes its window, so the ensuing
    /// `on_window_closed` finds nothing to terminate.
    fn redock_popout(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        let Some(popout) = self.popouts.remove(&iid) else {
            return;
        };
        match popout.view {
            PaneView::Terminal(view) => {
                self.terminals.insert(iid, view);
                if let Some(pid) = self.workspace.instance(iid).map(|i| i.project_id) {
                    self.redock_into_layout(iid, pid, popout.redock, cx);
                }
            }
            PaneView::Editor(ed) => {
                // Re-create in the main window on the next render (input focus is
                // bound to the window where the InputState is built).
                let snap = EditorSnapshot::capture(&ed, cx);
                self.pending_editor_redock.push((iid, snap, popout.redock));
                cx.notify();
            }
        }
    }

    /// Rebuild any editors awaiting re-dock into the main window. Called from
    /// `render`, which holds the main window.
    fn drain_editor_redocks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_editor_redock.is_empty() {
            return;
        }
        let config = self.editor_config();
        for (iid, snap, redock) in std::mem::take(&mut self.pending_editor_redock) {
            let config = config.clone();
            let ed = cx.new(|cx| {
                EditorView::from_state(
                    snap.text,
                    snap.path,
                    snap.language,
                    snap.cursor,
                    snap.dirty,
                    config,
                    window,
                    cx,
                )
            });
            self.editors.insert(iid, ed);
            if let Some(pid) = self.workspace.instance(iid).map(|i| i.project_id) {
                self.redock_into_layout(iid, pid, redock, cx);
            }
        }
    }

    /// When a popped-out window closes, terminate its terminal and drop the
    /// instance (it's already out of the main layout). Called from the app-wide
    /// `on_window_closed` hook. (The confirmation happens in the pop-out window.)
    fn close_popout(&mut self, window_id: WindowId, cx: &mut Context<Self>) {
        let Some((&iid, _)) = self
            .popouts
            .iter()
            .find(|(_, p)| gpui::AnyWindowHandle::from(p.window).window_id() == window_id)
        else {
            return;
        };
        let Some(popout) = self.popouts.remove(&iid) else {
            return;
        };
        self.clear_notifications_for(iid);
        // Terminals must be killed; editors just drop (unsaved changes lost — the
        // pop-out window already confirmed the close).
        if let PaneView::Terminal(view) = &popout.view {
            view.read(cx).session().kill();
        }
        // Tear down tmux (local or remote) + worktree, then drop the orphan meta.
        let info = self.workspace.instance(iid).map(|i| {
            (
                i.tmux_session.clone(),
                i.worktree_path.clone(),
                i.worktree_id,
                i.project_id,
                i.use_tmux,
            )
        });
        self.workspace.remove_instance_meta(iid);
        if let Some((local_session, worktree_path, worktree_id, project_id, use_tmux)) = info {
            // Popout window closed deliberately → kill the remote session too.
            self.teardown_closed_instance(
                iid,
                project_id,
                use_tmux,
                true,
                local_session,
                worktree_path,
                worktree_id,
                cx,
            );
        }
        self.last_status.remove(&iid);
        self.persist();
        cx.notify();
    }

    /// Re-insert an instance into its project layout + persist. Prefers the
    /// remembered neighbor (`hint`) so it lands roughly where it was; otherwise
    /// splits the active/first pane. Never clobbers a non-empty project's layout;
    /// seeds the layout only when the project is empty.
    fn redock_into_layout(
        &mut self,
        iid: Uuid,
        pid: Uuid,
        redock: RedockAnchor,
        cx: &mut Context<Self>,
    ) {
        let alive = |this: &Self, id: Uuid| {
            this.workspace
                .project(pid)
                .and_then(|p| p.layout.as_ref())
                .is_some_and(|l| l.find_path(id).is_some())
        };
        match redock {
            // Re-insert as a tab at its old index in the leaf that still holds the
            // remembered sibling.
            RedockAnchor::Tab { sibling, index } if alive(self, sibling) => {
                if let Some(p) = self.workspace.project_mut(pid)
                    && add_tab_at(&mut p.layout, sibling, iid, index)
                {
                    self.persist();
                    cx.notify();
                    return;
                }
                self.redock_into_layout(iid, pid, RedockAnchor::Floating, cx);
            }
            // Re-create as a split on the side it sat, next to its old neighbor.
            RedockAnchor::Split {
                anchor,
                dir,
                before,
            } if alive(self, anchor) => {
                if let Some(p) = self.workspace.project_mut(pid) {
                    split_beside(&mut p.layout, anchor, dir, iid, before);
                }
                self.persist();
                cx.notify();
            }
            // No anchor (or it's gone): split the active/first pane, or seed.
            _ => {
                let active_in_proj = self
                    .active_instance
                    .filter(|a| self.workspace.instance(*a).map(|i| i.project_id) == Some(pid));
                if let Some(p) = self.workspace.project_mut(pid) {
                    match active_in_proj.or_else(|| p.first_instance()) {
                        Some(target) => {
                            split(&mut p.layout, target, SplitDirection::Horizontal, iid);
                        }
                        None => p.layout = Some(PaneNode::leaf(iid)),
                    }
                }
                self.persist();
                cx.notify();
            }
        }
    }

    fn close_active(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active) = self.active_instance {
            self.request_close_instance(active, cx);
        }
    }

    /// Whether closing a pane of this kind should ask for confirmation first.
    fn confirm_close_for(&self, kind: InstanceKind) -> bool {
        match kind {
            InstanceKind::Terminal => self.settings.confirm_close_terminal,
            InstanceKind::Editor => self.settings.confirm_close_editor,
            InstanceKind::Diff => self.settings.confirm_close_diff,
        }
    }

    /// Close a pane, asking first if its kind's confirm-on-close is enabled.
    fn request_close_instance(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        let kind = self
            .workspace
            .instance(iid)
            .map(|i| i.kind)
            .unwrap_or(InstanceKind::Terminal);
        // A clean editor (no unsaved changes) has nothing to lose on close, so skip
        // the confirmation regardless of the confirm-on-close setting.
        let clean_editor = kind == InstanceKind::Editor
            && self
                .editors
                .get(&iid)
                .is_none_or(|e| !e.read(cx).is_dirty());
        if self.confirm_close_for(kind) && !clean_editor {
            let (noun, verb) = match kind {
                InstanceKind::Terminal => (t("terminal"), t("terminated")),
                InstanceKind::Editor => (t("editor"), t("closed")),
                InstanceKind::Diff => (t("diff"), t("closed")),
            };
            let name = self
                .workspace
                .instance(iid)
                .map(|i| i.custom_name.clone().unwrap_or_else(|| i.title.clone()))
                .unwrap_or_else(|| tf("this {noun}", &[("noun", &noun)]));
            self.request_confirm(
                tf("Close {noun}?", &[("noun", &noun)]),
                tf(
                    "“{name}” will be {verb}.",
                    &[("name", &name), ("verb", &verb)],
                ),
                t("Close"),
                ConfirmAction::CloseInstance(iid),
                cx,
            );
        } else {
            self.close_instance(iid, cx);
        }
    }

    /// Show the confirmation modal for a destructive action.
    fn request_confirm(
        &mut self,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
        action: ConfirmAction,
        cx: &mut Context<Self>,
    ) {
        self.confirm = Some(PendingConfirm {
            title: title.into(),
            message: message.into(),
            confirm_label: confirm_label.into(),
            action,
        });
        cx.notify();
    }

    fn cancel_confirm(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        cx.notify();
    }

    /// Execute the pending confirmed action.
    fn run_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.confirm.take() else {
            return;
        };
        match pending.action {
            ConfirmAction::DeleteWorkspace(id) => self.delete_workspace(id, cx),
            ConfirmAction::DeletePreset(idx) => self.delete_preset(idx, cx),
            ConfirmAction::DeleteProject(pid) => self.delete_project(pid, cx),
            ConfirmAction::DeleteRunner(idx) => self.delete_runner(idx, cx),
            ConfirmAction::DeleteLoop(idx) => self.delete_loop(idx, cx),
            ConfirmAction::DeleteRemote(idx) => self.delete_remote(idx, cx),
            ConfirmAction::CloseInstance(iid) => {
                self.close_instance(iid, cx);
                if let Some(next) = self.active_instance {
                    self.focus_instance(next, window, cx);
                }
            }
            ConfirmAction::CloseOtherTabs(keep) => {
                self.close_other_tabs_now(keep, window, cx);
            }
            ConfirmAction::CloseTabsSide { anchor, right } => {
                self.close_tabs_side_now(anchor, right, window, cx);
            }
            ConfirmAction::SwitchBranch { pid, branch } => {
                self.do_switch_branch(pid, branch, window, cx)
            }
            ConfirmAction::StashPop(pid) => self.do_stash_pop(pid, window, cx),
            ConfirmAction::StashDrop(pid) => self.do_stash_drop(pid, window, cx),
            ConfirmAction::DiscardWorktreeChanges(wid) => self.discard_worktree_changes(wid, cx),
            ConfirmAction::DiscardWorktree(wid) => self.discard_worktree(wid, cx),
        }
        cx.notify();
    }

    /// Delete a workspace: remove it from the index + delete its workspace file.
    /// Never deletes the last remaining workspace.
    fn delete_workspace(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.workspaces.workspaces.len() <= 1 {
            return;
        }
        self.workspaces.workspaces.retain(|p| p.id != id);
        if self.workspaces.current == Some(id) {
            self.workspaces.current = self.workspaces.workspaces.first().map(|p| p.id);
        }
        if self.current_workspace == Some(id) {
            self.current_workspace = None;
        }
        let _ = muxel_store::save_workspaces_index(&self.workspaces);
        if let Some(path) = muxel_store::workspace_doc_path(id) {
            let _ = std::fs::remove_file(&path);
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
        cx.notify();
    }

    /// Whether the active pane is a code editor (not an agent/terminal). Uses
    /// the persisted instance kind (authoritative), not the live `editors` map.
    fn active_is_editor(&self) -> bool {
        self.active_instance
            .and_then(|iid| self.workspace.instance(iid))
            .map(|i| i.kind == InstanceKind::Editor)
            .unwrap_or(false)
    }

    fn restart_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_instance else {
            return;
        };
        // Editors have no process to restart (restarting would spawn a shell).
        if self.workspace.instance(active).map(|i| i.kind) == Some(InstanceKind::Editor) {
            return;
        }
        if let Some(view) = self.terminals.remove(&active) {
            view.read(cx).session().kill();
        }
        self.spawn_terminal(active, window, cx);
        self.focus_instance(active, window, cx);
        cx.notify();
    }

    /// Duplicate an instance's launch spec into a new pane split beside it.
    fn duplicate_instance(&mut self, iid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut inst) = self.workspace.instance(iid).cloned() else {
            return;
        };
        let pid = inst.project_id;
        inst.id = Uuid::new_v4();
        inst.tmux_session = None;
        inst.pinned = false; // a duplicate starts unpinned
        // A duplicate shares the original's worktree (its worktree_id/path/branch
        // came across in the clone); we do NOT create a fresh one.
        if inst.use_tmux {
            let project_name = self
                .workspace
                .project(pid)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            inst.tmux_session = Some(muxel_core::tmux::session_name(&project_name, inst.id));
        }
        let new_iid = inst.id;
        // Insert the copy as a tab right after the original.
        let after = self
            .workspace
            .project(pid)
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| l.find_path(iid).map(|path| (l, path)))
            .and_then(|(l, path)| l.get_at_path(&path)?.tabs())
            .and_then(|(tabs, _)| tabs.iter().position(|&id| id == iid))
            .map(|pos| pos + 1)
            .unwrap_or(usize::MAX);
        let ok = self
            .workspace
            .project_mut(pid)
            .is_some_and(|p| add_tab_at(&mut p.layout, iid, new_iid, after));
        if !ok {
            return;
        }
        self.workspace.add_instance(inst);
        self.spawn_terminal(new_iid, window, cx);
        self.focus_instance(new_iid, window, cx);
        self.persist();
        cx.notify();
    }

    /// Every tab in `keep`'s pane except `keep`, in order.
    fn other_tabs_in_pane(&self, keep: Uuid) -> Vec<Uuid> {
        let Some(pid) = self.workspace.instance(keep).map(|i| i.project_id) else {
            return Vec::new();
        };
        self.workspace
            .project(pid)
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| {
                let path = l.find_path(keep)?;
                let (tabs, _) = l.get_at_path(&path)?.tabs()?;
                Some(tabs.iter().copied().filter(|&id| id != keep).collect())
            })
            .unwrap_or_default()
    }

    /// Close every other tab in `keep`'s pane (pinned included), leaving only
    /// `keep`. Asks once (batch) if any closed tab's kind confirms-on-close.
    fn close_other_tabs(&mut self, keep: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let others = self.other_tabs_in_pane(keep);
        if others.is_empty() {
            return;
        }
        // Prompt if any of the tabs being closed is a kind that asks first.
        let needs_confirm = others.iter().any(|id| {
            self.workspace
                .instance(*id)
                .is_some_and(|i| self.confirm_close_for(i.kind))
        });
        if needs_confirm {
            let n = others.len();
            self.request_confirm(
                t("Close other tabs?"),
                tn(
                    "{n} other tab in this pane will be terminated.",
                    "{n} other tabs in this pane will be terminated.",
                    n,
                    &[("n", &n.to_string())],
                ),
                t("Close others"),
                ConfirmAction::CloseOtherTabs(keep),
                cx,
            );
        } else {
            self.close_other_tabs_now(keep, window, cx);
        }
    }

    /// Close the other tabs directly (no per-tab prompt), then focus `keep`.
    fn close_other_tabs_now(&mut self, keep: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        for id in self.other_tabs_in_pane(keep) {
            self.close_instance(id, cx);
        }
        self.focus_instance(keep, window, cx);
    }

    /// Tabs to the left (`right=false`) or right (`right=true`) of `anchor` in its
    /// pane, in order.
    fn tabs_to_side(&self, anchor: Uuid, right: bool) -> Vec<Uuid> {
        let Some(pid) = self.workspace.instance(anchor).map(|i| i.project_id) else {
            return Vec::new();
        };
        self.workspace
            .project(pid)
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| {
                let path = l.find_path(anchor)?;
                let (tabs, _) = l.get_at_path(&path)?.tabs()?;
                let idx = tabs.iter().position(|&id| id == anchor)?;
                let slice = if right {
                    &tabs[idx + 1..]
                } else {
                    &tabs[..idx]
                };
                Some(slice.to_vec())
            })
            .unwrap_or_default()
    }

    /// Close the tabs to one side of `anchor` (with the batch close-confirm).
    fn close_tabs_side(
        &mut self,
        anchor: Uuid,
        right: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids = self.tabs_to_side(anchor, right);
        if ids.is_empty() {
            return;
        }
        let needs_confirm = ids.iter().any(|id| {
            self.workspace
                .instance(*id)
                .is_some_and(|i| self.confirm_close_for(i.kind))
        });
        if needs_confirm {
            let n = ids.len();
            let side = if right { "right" } else { "left" };
            self.request_confirm(
                t("Close tabs?"),
                tn(
                    "{n} tab to the {side} will be terminated.",
                    "{n} tabs to the {side} will be terminated.",
                    n,
                    &[("n", &n.to_string()), ("side", side)],
                ),
                t("Close tabs"),
                ConfirmAction::CloseTabsSide { anchor, right },
                cx,
            );
        } else {
            self.close_tabs_side_now(anchor, right, window, cx);
        }
    }

    fn close_tabs_side_now(
        &mut self,
        anchor: Uuid,
        right: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for id in self.tabs_to_side(anchor, right) {
            self.close_instance(id, cx);
        }
        self.focus_instance(anchor, window, cx);
    }

    /// Clear the active terminal's scrollback history.
    fn clear_active_terminal(&mut self, cx: &mut Context<Self>) {
        if let Some(iid) = self.active_instance {
            self.clear_terminal_scrollback(iid, cx);
        }
    }

    /// Clear terminal `iid`'s scrollback history.
    fn clear_terminal_scrollback(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        if let Some(view) = self.terminals.get(&iid) {
            view.read(cx).session().clear_scrollback();
            cx.notify();
        }
    }

    /// The active terminal's session, if the active pane is a terminal.
    fn active_session(&self, cx: &App) -> Option<Arc<TerminalSession>> {
        self.active_instance
            .and_then(|iid| self.terminals.get(&iid))
            .map(|v| v.read(cx).session().clone())
    }

    /// Repaint the active terminal (so it re-reads the search needle to highlight).
    fn notify_active_terminal(&mut self, cx: &mut Context<Self>) {
        if let Some(v) = self
            .active_instance
            .and_then(|iid| self.terminals.get(&iid))
            .cloned()
        {
            v.update(cx, |_, cx| cx.notify());
        }
    }

    /// Open the search bar for the active terminal.
    fn open_term_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let is_term = self
            .active_instance
            .and_then(|iid| self.workspace.instance(iid))
            .map(|i| i.kind)
            == Some(InstanceKind::Terminal);
        if !is_term {
            return;
        }
        self.term_search = Some(TermSearch {
            matches: Vec::new(),
            idx: 0,
        });
        self.term_search_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        if let Some(session) = self.active_session(cx) {
            session.set_search("");
        }
        let handle = self.term_search_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.notify_active_terminal(cx);
        cx.notify();
    }

    /// Recompute matches for `query`, highlight, and jump to the newest match.
    fn refresh_term_search(&mut self, query: &str, cx: &mut Context<Self>) {
        let Some(session) = self.active_session(cx) else {
            return;
        };
        session.set_search(query);
        let matches = session.search_match_lines(query);
        let idx = matches.len().saturating_sub(1); // newest match
        if let Some(&line) = matches.get(idx) {
            session.scroll_to_line(line);
        }
        self.term_search = Some(TermSearch { matches, idx });
        self.notify_active_terminal(cx);
        cx.notify();
    }

    /// Step to another match (delta -1 = older/up, +1 = newer/down), scrolling it
    /// into view. Wraps.
    fn term_search_step(&mut self, delta: i32, cx: &mut Context<Self>) {
        let line = {
            let Some(ts) = self.term_search.as_mut() else {
                return;
            };
            if ts.matches.is_empty() {
                return;
            }
            let n = ts.matches.len() as i32;
            ts.idx = (ts.idx as i32 + delta).rem_euclid(n) as usize;
            ts.matches[ts.idx]
        };
        if let Some(session) = self.active_session(cx) {
            session.scroll_to_line(line);
        }
        self.notify_active_terminal(cx);
        cx.notify();
    }

    /// Close the search bar, clear the highlight, and refocus the terminal.
    fn close_term_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.active_session(cx) {
            session.set_search("");
        }
        self.term_search = None;
        if let Some(iid) = self.active_instance {
            self.focus_instance(iid, window, cx);
        }
        self.notify_active_terminal(cx);
        cx.notify();
    }

    /// Toggle the broadcast bar; focus its input when opening.
    fn toggle_broadcast(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.broadcasting = !self.broadcasting;
        if self.broadcasting {
            self.broadcast_input
                .update(cx, |s, cx| s.set_value("", window, cx));
            let handle = self.broadcast_input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// Live terminal panes in the active project (broadcast targets).
    fn broadcast_targets(&self) -> Vec<Uuid> {
        let Some(pid) = self.workspace.active_project else {
            return Vec::new();
        };
        self.workspace
            .project(pid)
            .map(|p| p.instances())
            .unwrap_or_default()
            .into_iter()
            .filter(|iid| {
                self.terminals.contains_key(iid)
                    && self.workspace.instance(*iid).map(|i| i.kind) == Some(InstanceKind::Terminal)
            })
            .collect()
    }

    /// Send the broadcast line (+ Enter) to every target agent, then clear it.
    fn send_broadcast(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let line = self.broadcast_input.read(cx).value().to_string();
        if line.is_empty() {
            return;
        }
        let payload = format!("{line}\r");
        for iid in self.broadcast_targets() {
            if let Some(view) = self.terminals.get(&iid) {
                view.read(cx).session().write_input(payload.as_bytes());
            }
        }
        self.broadcast_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// Select the Nth tab (1-based) of the active pane.
    fn jump_to_tab(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_instance else {
            return;
        };
        let Some(pid) = self.workspace.instance(active).map(|i| i.project_id) else {
            return;
        };
        let tab = self
            .workspace
            .project(pid)
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| {
                let path = l.find_path(active)?;
                let (tabs, _) = l.get_at_path(&path)?.tabs()?;
                tabs.get(n.checked_sub(1)?).copied()
            });
        if let Some(iid) = tab {
            self.focus_instance(iid, window, cx);
        }
    }

    /// Focus the next agent that needs attention — blocked panes first (they're
    /// waiting on the user), then done. Cycles past the currently active one.
    fn focus_attention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // All instances across projects, in a stable order.
        let order: Vec<Uuid> = self
            .workspace
            .projects
            .iter()
            .flat_map(|p| p.instances())
            .collect();
        let rank = |iid: &Uuid| -> Option<u8> {
            match self.terminals.get(iid).map(|v| v.read(cx).status()) {
                Some(AgentStatus::Blocked) => Some(0),
                Some(AgentStatus::Done) => Some(1),
                _ => None,
            }
        };
        // Rotate the list so the search starts just after the active instance,
        // then take blocked over done, earliest first.
        let start = self
            .active_instance
            .and_then(|a| order.iter().position(|i| *i == a))
            .map(|p| p + 1)
            .unwrap_or(0);
        let rotated = order
            .iter()
            .cycle()
            .skip(start)
            .take(order.len())
            .copied()
            .collect::<Vec<_>>();
        let target = rotated
            .iter()
            .filter_map(|i| rank(i).map(|r| (r, *i)))
            .min_by_key(|(r, _)| *r)
            .map(|(_, i)| i);
        if let Some(iid) = target {
            self.focus_instance(iid, window, cx);
        }
    }

    /// Move focus to the nearest pane in a spatial direction.
    fn focus_direction(&mut self, dir: FocusDir, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = self.active_instance else {
            return;
        };
        let target = self
            .workspace
            .instance(active)
            .map(|i| i.project_id)
            .and_then(|pid| self.workspace.project(pid))
            .and_then(|p| p.layout.as_ref())
            .and_then(|root| focus_in_direction(root, active, dir));
        if let Some(iid) = target {
            self.focus_instance(iid, window, cx);
        }
    }

    /// Toggle a tab's pinned flag, then re-order its pane so pinned tabs stay in
    /// the leftmost block (stable within each group).
    fn toggle_pin(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        if let Some(inst) = self.workspace.instance_mut(iid) {
            inst.pinned = !inst.pinned;
        }
        self.reflow_pins(iid);
        self.persist();
        cx.notify();
    }

    fn start_rename_instance(&mut self, iid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .workspace
            .instance(iid)
            .and_then(|i| i.custom_name.clone())
            .unwrap_or_default();
        self.start_rename(RenameTarget::Instance(iid), current, window, cx);
    }

    fn start_rename_project(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .workspace
            .project(pid)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        self.start_rename(RenameTarget::Project(pid), current, window, cx);
    }

    fn start_rename_worktree(&mut self, wid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .workspace
            .worktree(wid)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        self.start_rename(RenameTarget::Worktree(wid), current, window, cx);
    }

    fn start_rename(
        &mut self,
        target: RenameTarget,
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rename = Some(target);
        self.rename_input
            .update(cx, |s, cx| s.set_value(current, window, cx));
        let handle = self.rename_input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.rename.take() else {
            return;
        };
        let value = self.rename_input.read(cx).value().trim().to_string();
        match target {
            RenameTarget::Instance(iid) => {
                if let Some(inst) = self.workspace.instance_mut(iid) {
                    inst.custom_name = (!value.is_empty()).then_some(value);
                }
            }
            RenameTarget::Project(pid) => {
                if let Some(p) = self.workspace.project_mut(pid)
                    && !value.is_empty()
                {
                    p.name = value;
                }
            }
            RenameTarget::Worktree(wid) => {
                if let Some(w) = self.workspace.worktree_mut(wid)
                    && !value.is_empty()
                {
                    w.name = value;
                }
            }
            RenameTarget::File(old) => {
                if !value.is_empty() && !value.contains('/') {
                    self.rename_file_on_disk(&old, &value, cx);
                }
            }
        }
        self.persist();
        cx.notify();
    }

    /// Rename `old` to a sibling named `name` on disk, then remap the browser
    /// file list, expanded set, and any open editors under `old` (file or dir).
    fn rename_file_on_disk(&mut self, old: &Path, name: &str, cx: &mut Context<Self>) {
        let new = old.with_file_name(name);
        if let Err(e) = std::fs::rename(old, &new) {
            log::warn!("rename {} failed: {e}", old.display());
            return;
        }
        // Replace `old` (or any path under `old/`) with the new prefix.
        let remap = |p: &Path| -> Option<PathBuf> {
            if p == old {
                Some(new.clone())
            } else {
                p.strip_prefix(old).ok().map(|rest| new.join(rest))
            }
        };
        for p in self.file_browser_files.iter_mut() {
            if let Some(np) = remap(p) {
                *p = np;
            }
        }
        self.file_browser_expanded = self
            .file_browser_expanded
            .iter()
            .map(|p| remap(p).unwrap_or_else(|| p.clone()))
            .collect();
        // Repoint any open editor whose file moved.
        let editors: Vec<_> = self.editors.iter().map(|(i, e)| (*i, e.clone())).collect();
        for (_iid, ed) in editors {
            let moved = ed.read(cx).path().and_then(remap);
            if let Some(np) = moved {
                ed.update(cx, |e, cx| e.set_path(np, cx));
            }
        }
        self.rebuild_file_browser_rows(cx);
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.rename = None;
        cx.notify();
    }

    fn reorder_projects(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let n = self.workspace.projects.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let project = self.workspace.projects.remove(from);
        let to = to.min(self.workspace.projects.len());
        self.workspace.projects.insert(to, project);
        self.persist();
        cx.notify();
    }

    fn toggle_collapse(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&pid) {
            self.collapsed.insert(pid);
        }
        cx.notify();
    }

    /// Focus an instance, switching to its project first if needed.
    fn select_instance(&mut self, iid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pid) = self.workspace.instance(iid).map(|i| i.project_id)
            && self.workspace.active_project != Some(pid)
        {
            self.select_project(pid, window, cx);
        }
        self.focus_instance(iid, window, cx);
    }

    /// If a desktop notification was clicked since the last tick, raise muxel and
    /// switch to the instance it pointed at. Polled from the status timer because
    /// the click lands on a background D-Bus thread that can't touch the UI.
    fn handle_notification_click(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(iid) = PENDING_NOTIFICATION_FOCUS.lock().unwrap().take() else {
            return;
        };
        if self.workspace.instance(iid).is_some() {
            // Switch to the pane the notification pointed at. We deliberately do
            // NOT call `window.activate_window()`: a background app raising itself
            // trips GNOME/Wayland focus-stealing prevention, which posts the
            // "muxel is ready" hand-off notification. The notification's
            // desktop-entry hint (see `notify`) lets the shell raise muxel's
            // window itself on click instead.
            self.select_instance(iid, window, cx);
        }
    }

    /// Swap two terminals' positions (sidebar or pane drag). Only within one project.
    fn swap_terminals(&mut self, a: Uuid, b: Uuid, cx: &mut Context<Self>) {
        let Some(pid) = self.workspace.instance(a).map(|i| i.project_id) else {
            return;
        };
        if self.workspace.instance(b).map(|i| i.project_id) != Some(pid) {
            return;
        }
        let ok = self
            .workspace
            .project_mut(pid)
            .is_some_and(|p| swap_instances(&mut p.layout, a, b));
        if ok {
            self.persist();
            cx.notify();
        }
    }

    /// Drop a dragged pane (title bar) onto pane `tgt_anchor`: center/none → swap
    /// the two panes; an edge → move the whole source pane to a split there.
    /// `src_anchor`/`tgt_anchor` are any instance in each pane; one project only.
    fn dock_pane(&mut self, src_anchor: Uuid, tgt_anchor: Uuid, cx: &mut Context<Self>) {
        let zone = self
            .pane_drop
            .filter(|(a, _)| *a == tgt_anchor)
            .map(|(_, z)| z);
        self.pane_drop = None;

        let Some(pid) = self.workspace.instance(src_anchor).map(|i| i.project_id) else {
            return;
        };
        if self.workspace.instance(tgt_anchor).map(|i| i.project_id) != Some(pid) {
            return;
        }
        let ok = self.workspace.project_mut(pid).is_some_and(|p| {
            match zone.and_then(|z| z.to_split()) {
                Some((dir, before)) => {
                    move_pane_beside(&mut p.layout, src_anchor, tgt_anchor, dir, before)
                }
                None => swap_panes(&mut p.layout, src_anchor, tgt_anchor),
            }
        });
        if ok {
            self.persist();
            cx.notify();
        }
    }

    /// Re-order the pane holding `anchor` so pinned tabs form the leftmost block
    /// (stable within each group). Does not persist/notify — the caller does.
    fn reflow_pins(&mut self, anchor: Uuid) {
        let Some(pid) = self.workspace.instance(anchor).map(|i| i.project_id) else {
            return;
        };
        let order: Option<(Uuid, Vec<Uuid>)> = self
            .workspace
            .project(pid)
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| {
                let path = l.find_path(anchor)?;
                let (tabs, _) = l.get_at_path(&path)?.tabs()?;
                let a = tabs[0];
                let (mut pinned, unpinned): (Vec<Uuid>, Vec<Uuid>) =
                    tabs.iter().copied().partition(|&id| {
                        self.workspace
                            .instance(id)
                            .map(|i| i.pinned)
                            .unwrap_or(false)
                    });
                pinned.extend(unpinned);
                Some((a, pinned))
            });
        if let Some((a, order)) = order
            && let Some(p) = self.workspace.project_mut(pid)
        {
            set_tab_order(&mut p.layout, a, &order);
        }
    }

    /// Set the live drop-indicator slot while dragging a tab (guarded so we only
    /// re-render when it actually changes).
    fn update_tab_drop(&mut self, anchor: Uuid, index: usize, cx: &mut Context<Self>) {
        if self.tab_drop != Some((anchor, index)) {
            self.tab_drop = Some((anchor, index));
            cx.notify();
        }
    }

    /// Clear the drop indicator (drag left all panes, or ended).
    fn clear_tab_drop(&mut self, cx: &mut Context<Self>) {
        if self.tab_drop.is_some() {
            self.tab_drop = None;
            cx.notify();
        }
    }

    /// Set the pane-body drop zone (edge-split highlight).
    fn update_pane_drop(&mut self, anchor: Uuid, zone: DropZone, cx: &mut Context<Self>) {
        if self.pane_drop != Some((anchor, zone)) {
            self.pane_drop = Some((anchor, zone));
            cx.notify();
        }
    }

    /// Clear the pane-body drop zone.
    fn clear_pane_drop(&mut self, cx: &mut Context<Self>) {
        if self.pane_drop.is_some() {
            self.pane_drop = None;
            cx.notify();
        }
    }

    /// Whether `iid` is a pinned tab.
    fn tab_is_pinned(&self, iid: Uuid) -> bool {
        self.workspace
            .instance(iid)
            .map(|i| i.pinned)
            .unwrap_or(false)
    }

    /// Number of pinned tabs in the pane holding `anchor`.
    fn pinned_count_in_pane(&self, anchor: Uuid) -> usize {
        let Some(pid) = self.workspace.instance(anchor).map(|i| i.project_id) else {
            return 0;
        };
        self.workspace
            .project(pid)
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| {
                let path = l.find_path(anchor)?;
                let (tabs, _) = l.get_at_path(&path)?.tabs()?;
                Some(tabs.iter().filter(|&&id| self.tab_is_pinned(id)).count())
            })
            .unwrap_or(0)
    }

    /// Whether any tab to the left of `iid` in its pane is unpinned (i.e. `iid`
    /// has been moved out of the leftmost pinned block).
    fn has_unpinned_left_of(&self, iid: Uuid) -> bool {
        let Some(pid) = self.workspace.instance(iid).map(|i| i.project_id) else {
            return false;
        };
        self.workspace
            .project(pid)
            .and_then(|p| p.layout.as_ref())
            .and_then(|l| {
                let path = l.find_path(iid)?;
                let (tabs, _) = l.get_at_path(&path)?.tabs()?;
                let pos = tabs.iter().position(|&id| id == iid)?;
                Some(tabs[..pos].iter().any(|&id| !self.tab_is_pinned(id)))
            })
            .unwrap_or(false)
    }

    /// Drop a dragged tab onto `anchor`'s pane: the tab strip inserts at the
    /// hovered slot (`tab_drop`); the body center tabifies; a body edge
    /// (`pane_drop`) pulls the tab out into a new split. Pin rules: an unpinned
    /// tab can't be dropped inside the pinned block; a pinned tab dragged past an
    /// unpinned tab becomes unpinned (otherwise it stays pinned).
    fn dock_tab(
        &mut self,
        dragged: Uuid,
        anchor: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The strip insertion index (if over the tab bar) takes priority; else the
        // body zone (center = tabify, edge = split out).
        let drop_index = self.tab_drop.filter(|(a, _)| *a == anchor).map(|(_, i)| i);
        let zone = self.pane_drop.filter(|(a, _)| *a == anchor).map(|(_, z)| z);
        self.tab_drop = None;
        self.pane_drop = None;
        let Some(pid) = self.workspace.instance(dragged).map(|i| i.project_id) else {
            return;
        };
        if self.workspace.instance(anchor).map(|i| i.project_id) != Some(pid) {
            return;
        }
        let dragged_pinned = self.tab_is_pinned(dragged);
        let moved = if let Some(mut index) = drop_index {
            // An unpinned tab can't jump in front of the pinned block.
            if !dragged_pinned {
                index = index.max(self.pinned_count_in_pane(anchor));
            }
            self.workspace
                .project_mut(pid)
                .is_some_and(|p| move_tab_to(&mut p.layout, dragged, anchor, index))
        } else if let Some((dir, before)) = zone.and_then(|z| z.to_split()) {
            // Dropped on a pane edge → pull the tab out into a new split.
            self.workspace
                .project_mut(pid)
                .is_some_and(|p| move_into_split(&mut p.layout, dragged, anchor, dir, before))
        } else {
            // Center, or no zone recorded → tabify.
            self.workspace
                .project_mut(pid)
                .is_some_and(|p| move_into_tabs(&mut p.layout, dragged, anchor))
        };
        if moved {
            // A pinned tab dragged out past an unpinned tab loses its pin.
            if dragged_pinned
                && self.has_unpinned_left_of(dragged)
                && let Some(inst) = self.workspace.instance_mut(dragged)
            {
                inst.pinned = false;
            }
            self.focus_instance(dragged, window, cx);
            self.persist();
            cx.notify();
        }
    }

    /// Record dragged split sizes into the active project's layout + persist, so
    /// pane proportions restore on next launch. Called from `on_resize`.
    fn update_split_sizes(&mut self, key: SharedString, sizes: Vec<f32>, _cx: &mut Context<Self>) {
        let Some(pid) = self.workspace.active_project else {
            return;
        };
        let changed = self
            .workspace
            .project_mut(pid)
            .map(|p| set_split_sizes(&mut p.layout, &key, &sizes))
            .unwrap_or(false);
        if changed {
            self.persist();
        }
    }

    /// Even out a split's panes (double-click a divider): reset its sizes to
    /// equal. Bumping the per-split nonce changes the resizable group's id so its
    /// internal state restarts from the equal (flexing) layout.
    fn even_split(&mut self, key: String, n: usize, cx: &mut Context<Self>) {
        if n < 2 {
            return;
        }
        // `[1.0; n]` is the "equal" sentinel — panels then flex evenly.
        let equal = vec![1.0_f32; n];
        if let Some(pid) = self.workspace.active_project
            && let Some(p) = self.workspace.project_mut(pid)
        {
            set_split_sizes(&mut p.layout, &key, &equal);
        }
        *self.split_even_nonce.entry(key).or_insert(0) += 1;
        self.persist();
        cx.notify();
    }

    /// Record the dragged sidebar width into the workspace + persist.
    fn set_sidebar_width(&mut self, width: f32, _cx: &mut Context<Self>) {
        if self.workspace.sidebar_width != Some(width) {
            self.workspace.sidebar_width = Some(width);
            self.persist();
        }
    }

    fn set_file_browser_width(&mut self, width: f32, _cx: &mut Context<Self>) {
        if self.workspace.file_browser_width != Some(width) {
            self.workspace.file_browser_width = Some(width);
            self.persist();
        }
    }

    /// Toggle the file-browser sidebar for project `pid` (hides if already shown
    /// for that project; otherwise selects it + loads its file list in the
    /// background so a large repo doesn't freeze the UI).
    /// The directory the file browser lists for a project: the remote root for a
    /// remote project (file paths share this prefix), else the local root.
    fn browser_root(&self, pid: Uuid) -> PathBuf {
        self.workspace
            .project(pid)
            .map(|p| match &p.remote {
                Some(r) => PathBuf::from(&r.remote_root),
                None => p.root_path.clone(),
            })
            .unwrap_or_default()
    }

    fn toggle_file_browser(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_file_browser && self.file_browser_pid == Some(pid) {
            self.show_file_browser = false;
            cx.notify();
            return;
        }
        self.show_file_browser = true;
        // `select_project` re-points the open browser at `pid` (see its tail).
        self.select_project(pid, window, cx);
    }

    /// Point the file browser at `pid` and (re)list its files. Called when opening
    /// it and whenever the active project changes while it's open.
    fn load_file_browser(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        // A different project: drop the old project's expansion/files.
        if self.file_browser_pid != Some(pid) {
            self.file_browser_expanded.clear();
        }
        self.file_browser_pid = Some(pid);
        self.file_browser_files = Vec::new();
        self.file_browser_rows = Arc::new(Vec::new());
        let root = self.browser_root(pid);
        // Remote projects list over SSH (`git ls-files`/`find`); local walk otherwise.
        let remote_loc = self
            .workspace
            .project(pid)
            .is_some_and(|p| p.is_remote())
            .then(|| self.repo_loc(pid))
            .flatten();
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let files: Vec<PathBuf> = if let Some(loc) = remote_loc {
                cx.background_executor()
                    .spawn(async move {
                        integrations::list_remote_files(&loc)
                            .into_iter()
                            .map(PathBuf::from)
                            .collect::<Vec<_>>()
                    })
                    .await
            } else {
                cx.background_executor()
                    .spawn(async move { list_project_files(&root) })
                    .await
            };
            let _ = this.update(cx, |this, cx| {
                if this.file_browser_pid == Some(pid) {
                    this.file_browser_files = files;
                    this.rebuild_file_browser_rows(cx);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Recompute the cached browser rows (tree or flat search) — called only when
    /// the inputs change (files / expanded / query), never per render.
    fn rebuild_file_browser_rows(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.file_browser_pid.map(|pid| self.browser_root(pid)) else {
            self.file_browser_rows = Arc::new(Vec::new());
            return;
        };
        let query = self.file_browser_input.read(cx).value().to_string();
        let rows = if query.trim().is_empty() {
            crate::filetree::visible_rows(
                &self.file_browser_files,
                &root,
                &self.file_browser_expanded,
            )
        } else {
            crate::filetree::filter(&self.file_browser_files, &root, query.trim())
        };
        self.file_browser_rows = Arc::new(rows);
    }

    /// Open the file the browser row points at (reuses/splits an editor pane).
    fn open_browser_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pid) = self.file_browser_pid {
            let target = self.active_instance;
            self.open_editor_at(pid, Some(path), target, window, cx);
        }
    }

    /// Spawn a shell terminal with its working directory set to `dir`.
    fn open_terminal_at(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.file_browser_pid.or(self.workspace.active_project) else {
            return;
        };
        let mut instance = Instance::from_preset(pid, &AgentPreset::shell());
        // command_for uses worktree_path as the cwd (no worktree_id = plain cwd).
        instance.worktree_path = Some(dir);
        let target = self.active_instance;
        self.place_and_spawn(
            pid,
            instance,
            PlacementMode::Split(SplitDirection::Horizontal),
            target,
            None,
            window,
            cx,
        );
    }

    fn toggle_browser_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !self.file_browser_expanded.remove(&dir) {
            self.file_browser_expanded.insert(dir);
        }
        self.rebuild_file_browser_rows(cx);
        cx.notify();
    }

    /// The file-browser sidebar panel: header, search, and a virtualized file
    /// tree (or flat search). Rows are pre-computed (see `rebuild_file_browser_rows`)
    /// and only the visible range is built each frame, so large repos stay smooth.
    fn render_file_browser(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(pid) = self.file_browser_pid else {
            return div().into_any_element();
        };
        let proj_name = self
            .workspace
            .project(pid)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let rows = self.file_browser_rows.clone();
        let entity = cx.entity();
        let muted = cx.theme().muted_foreground;
        let hover_bg = cx.theme().sidebar_accent.opacity(0.5);
        let radius = cx.theme().radius;
        let root = self
            .workspace
            .project(pid)
            .map(|p| p.root_path.clone())
            .unwrap_or_default();
        let renaming = match &self.rename {
            Some(RenameTarget::File(p)) => Some(p.clone()),
            _ => None,
        };
        let rename_input = self.rename_input.clone();
        // Reveal-in-file-manager and on-disk rename are local-only; hide them for
        // remote projects (Open-in-terminal still works — it opens a remote shell).
        let is_remote = self
            .file_browser_pid
            .and_then(|pid| self.workspace.project(pid))
            .is_some_and(|p| p.is_remote());

        let list = uniform_list("fb-rows", rows.len(), move |range, window, _cx| {
            range
                .map(|i| {
                    let row = &rows[i];
                    let abs = row.abs_path.clone();
                    let is_dir = row.is_dir;
                    let indent = px(6.0 + row.depth as f32 * 12.0);
                    let base = div()
                        .id(("fb-row", i))
                        .pl(indent)
                        .pr_2()
                        .py(px(2.0))
                        .flex()
                        .items_center()
                        .gap_1()
                        .rounded(radius)
                        .text_sm();
                    // Inline rename for this row.
                    if renaming.as_deref() == Some(abs.as_path()) {
                        return base
                            .on_key_down(window.listener_for(
                                &entity,
                                |this, ev: &KeyDownEvent, _w, cx| {
                                    if ev.keystroke.key == "escape" {
                                        this.cancel_rename(cx);
                                    }
                                },
                            ))
                            .on_mouse_down_out(
                                window.listener_for(&entity, |this, _ev, _w, cx| {
                                    this.commit_rename(cx)
                                }),
                            )
                            .child(div().flex_1().child(Input::new(&rename_input)))
                            .into_any_element();
                    }
                    let icon = if is_dir {
                        Icon::new(if row.expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .small()
                        .into_any_element()
                    } else {
                        Icon::new(IconName::File).small().into_any_element()
                    };
                    // Directory to `cd` into for "Open in terminal".
                    let term_dir = if is_dir {
                        abs.clone()
                    } else {
                        abs.parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| abs.clone())
                    };
                    let rel = abs
                        .strip_prefix(&root)
                        .unwrap_or(&abs)
                        .display()
                        .to_string();
                    let abs_str = abs.display().to_string();
                    let row_name = abs
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let open_abs = abs.clone();
                    base.cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .on_mouse_down(
                            MouseButton::Left,
                            window.listener_for(&entity, move |this, _e, window, cx| {
                                if is_dir {
                                    this.toggle_browser_dir(open_abs.clone(), cx);
                                } else {
                                    this.open_browser_file(open_abs.clone(), window, cx);
                                }
                            }),
                        )
                        .child(icon)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(row.name.clone()),
                        )
                        .context_menu({
                            let entity = entity.clone();
                            move |menu, window, _cx| {
                                let abs_str = abs_str.clone();
                                let rel = rel.clone();
                                let reveal = abs.clone();
                                let term_dir = term_dir.clone();
                                let ren = abs.clone();
                                let row_name = row_name.clone();
                                let mut menu = menu
                                    .item(
                                        PopupMenuItem::new(t("Copy path"))
                                            .icon(IconName::Copy)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |_t, _e, _w, cx| {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(abs_str.clone()),
                                                    )
                                                },
                                            )),
                                    )
                                    .item(
                                        PopupMenuItem::new(t("Copy relative path"))
                                            .icon(IconName::Copy)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |_t, _e, _w, cx| {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(rel.clone()),
                                                    )
                                                },
                                            )),
                                    )
                                    .separator();
                                if !is_remote {
                                    menu = menu.item(
                                        PopupMenuItem::new(t("Reveal in file manager"))
                                            .icon(IconName::FolderOpen)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |_t, _e, _w, _cx| {
                                                    integrations::reveal_in_file_manager(&reveal)
                                                },
                                            )),
                                    );
                                }
                                menu = menu.item(
                                    PopupMenuItem::new(t("Open in terminal"))
                                        .icon(IconName::SquareTerminal)
                                        .on_click(window.listener_for(
                                            &entity,
                                            move |this, _e, window, cx| {
                                                this.open_terminal_at(term_dir.clone(), window, cx)
                                            },
                                        )),
                                );
                                if !is_remote {
                                    menu = menu.separator().item(
                                        PopupMenuItem::new(t("Rename…"))
                                            .icon(Icon::empty().path("icons/pencil.svg"))
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _e, window, cx| {
                                                    this.start_rename(
                                                        RenameTarget::File(ren.clone()),
                                                        row_name.clone(),
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            )),
                                    );
                                }
                                menu
                            }
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        });

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .font_semibold()
                            .text_color(muted)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(tf(
                                "FILES · {proj_name}",
                                &[("proj_name", &proj_name.to_string())],
                            )),
                    )
                    .child(
                        Button::new("fb-close")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t("Close file browser"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.show_file_browser = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .child(Input::new(&self.file_browser_input).w_full()),
            )
            .child(div().flex_1().min_h_0().px_1().child(list.size_full()))
            .into_any_element()
    }

    /// A tab/pane's display name: the user's custom name if set, else the
    /// editor's file name; for a **shell** the live working directory (its OSC
    /// title with any `user@host:` prefix stripped — handier than a static
    /// "Shell"); for an **agent** the static preset name, which deliberately does
    /// NOT follow the OSC title an agent rewrites as it works, so the tab keeps a
    /// stable name until renamed.
    fn instance_title(&self, iid: Uuid, cx: &App) -> SharedString {
        let inst = self.workspace.instance(iid);
        if let Some(c) = inst
            .and_then(|i| i.custom_name.clone())
            .filter(|c| !c.is_empty())
        {
            return c.into();
        }
        if let Some(ed) = self.editors.get(&iid) {
            return ed.read(cx).title().into();
        }
        // A shell (no agent program) shows its current directory from the live
        // terminal title; an agent keeps its static preset name.
        if inst.is_some_and(|i| i.program.is_none())
            && let Some(osc) = self.terminals.get(&iid).and_then(|v| v.read(cx).title())
        {
            return shell_dir_title(&osc).to_string().into();
        }
        inst.map(|i| i.title.clone()).unwrap_or_default().into()
    }

    /// The worktree shared by ALL of `tabs` (None if mixed or none) — drives the
    /// uniform-pane outline tint and name badge.
    fn pane_worktree(&self, tabs: &[Uuid]) -> Option<&Worktree> {
        let first = self.workspace.instance(*tabs.first()?)?.worktree_id?;
        for &t in &tabs[1..] {
            if self.workspace.instance(t).and_then(|i| i.worktree_id) != Some(first) {
                return None;
            }
        }
        self.workspace.worktree(first)
    }

    /// The worktree color for a single instance (its tab dot / sidebar dot).
    fn instance_worktree_color(&self, iid: Uuid) -> Option<Hsla> {
        let wid = self.workspace.instance(iid)?.worktree_id?;
        self.workspace
            .worktree(wid)
            .map(|w| worktree_color(w.color))
    }

    fn render_pane(&self, node: &PaneNode, cx: &mut Context<Self>) -> AnyElement {
        match node {
            PaneNode::Leaf(ld) => {
                // Each pane is a tab group. The focused pane shows its focused
                // tab (== active_instance); other panes show their own saved
                // active tab. Aliasing `iid`/`is_active` keeps the controls block
                // (split / maximize / pop-out / close) acting on the shown tab.
                let entity = cx.entity(); // for per-tab context menus
                let tabs = ld.tabs.clone();
                let leaf_active = ld.active.min(tabs.len().saturating_sub(1));
                let pane_has_focus = self.active_instance.is_some_and(|a| tabs.contains(&a));
                let iid = if pane_has_focus {
                    self.active_instance.unwrap()
                } else {
                    tabs[leaf_active]
                };
                let is_active = pane_has_focus;
                let content: AnyElement = if let Some(view) = self.terminals.get(&iid) {
                    terminal_pane_element(view, cx)
                } else if let Some(ed) = self.editors.get(&iid) {
                    // Clicking the editor makes it the active pane (so toolbar
                    // actions like Restart target it correctly).
                    div()
                        .size_full()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _e, window, cx| {
                                if this.active_instance != Some(iid) {
                                    this.focus_instance(iid, window, cx);
                                }
                            }),
                        )
                        .child(ed.clone())
                        .into_any_element()
                } else {
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(cx.theme().muted_foreground)
                        .child(t("(terminal exited)"))
                        .into_any_element()
                };
                // Each pane is a rounded card with a thin header that doubles as
                // the drag handle — so the terminal body stays free for text
                // selection. The active card gets an accent ring + glow.
                let accent = cx.theme().primary;
                let inactive = match self.settings.pane_border.as_str() {
                    "off" => cx.theme().background,
                    "bold" => cx.theme().muted_foreground,
                    _ => cx.theme().border,
                };
                let drop_hl = accent.opacity(0.4);

                // Worktree tint: a uniform pane (all tabs share one worktree) gets
                // that worktree's color on its outline + glow.
                let pane_wt = self.pane_worktree(&tabs);
                let wt_color: Option<Hsla> = pane_wt.map(|w| worktree_color(w.color));
                let strip_wt: Option<(Hsla, SharedString, Uuid)> =
                    pane_wt.map(|w| (worktree_color(w.color), w.name.clone().into(), w.id));
                // Per-tab worktree colors (each worktree tab shows a color dot).
                let tab_wt_colors: Vec<Option<Hsla>> = tabs
                    .iter()
                    .map(|&t| self.instance_worktree_color(t))
                    .collect();

                let inst = self.workspace.instance(iid);
                let header_bg = if is_active {
                    cx.theme().title_bar
                } else {
                    cx.theme().secondary
                };

                // Right-aligned, pane-level controls (act on the shown tab). A
                // single stop_propagation keeps a button click from starting a
                // tab drag or focus change.
                let sid = iid.simple();
                let kind = inst.map(|i| i.kind).unwrap_or_default();
                let max_icon = if self.maximized == Some(iid) {
                    IconName::Minimize
                } else {
                    IconName::Maximize
                };
                let controls = div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(1.0))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new(SharedString::from(format!("split-h-{sid}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::PanelRight)
                            .tooltip(t("Split right (hold to choose agent)"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, e: &MouseDownEvent, _w, cx| {
                                    this.begin_place_press(
                                        iid,
                                        PlacementMode::Split(SplitDirection::Horizontal),
                                        e.position,
                                        cx,
                                    )
                                }),
                            )
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(move |this, _e, window, cx| {
                                    this.end_place_press(
                                        iid,
                                        PlacementMode::Split(SplitDirection::Horizontal),
                                        window,
                                        cx,
                                    )
                                }),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("split-v-{sid}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::PanelBottom)
                            .tooltip(t("Split down (hold to choose agent)"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, e: &MouseDownEvent, _w, cx| {
                                    this.begin_place_press(
                                        iid,
                                        PlacementMode::Split(SplitDirection::Vertical),
                                        e.position,
                                        cx,
                                    )
                                }),
                            )
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(move |this, _e, window, cx| {
                                    this.end_place_press(
                                        iid,
                                        PlacementMode::Split(SplitDirection::Vertical),
                                        window,
                                        cx,
                                    )
                                }),
                            ),
                    )
                    // Agent panes get a "show git diff" button; diff panes get a
                    // "refresh" button instead.
                    .children((kind == InstanceKind::Terminal).then(|| {
                        Button::new(SharedString::from(format!("diff-{sid}")))
                            .ghost()
                            .xsmall()
                            .icon(Icon::empty().path("icons/diff.svg"))
                            .tooltip(t("Show changes (git diff)"))
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.open_diff_for(iid, window, cx)
                            }))
                    }))
                    .children((kind == InstanceKind::Diff).then(|| {
                        Button::new(SharedString::from(format!("refresh-{sid}")))
                            .ghost()
                            .xsmall()
                            .label("↻")
                            .tooltip(t("Refresh diff"))
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.refresh_diff_pane(iid, window, cx)
                            }))
                    }))
                    // Image/markdown editors get a rendered/raw toggle.
                    .children(
                        self.editors
                            .get(&iid)
                            .filter(|e| e.read(cx).is_renderable())
                            .map(|e| {
                                let rendered = e.read(cx).show_rendered();
                                Button::new(SharedString::from(format!("md-{sid}")))
                                    .ghost()
                                    .xsmall()
                                    .label(if rendered { t("Raw") } else { t("Rendered") })
                                    .tooltip(if rendered {
                                        t("Show raw text")
                                    } else {
                                        t("Show rendered markdown")
                                    })
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        if let Some(ed) = this.editors.get(&iid).cloned() {
                                            ed.update(cx, |e, cx| e.toggle_rendered(cx));
                                        }
                                    }))
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("max-{sid}")))
                            .ghost()
                            .xsmall()
                            .icon(max_icon)
                            .tooltip(t("Maximize"))
                            .on_click(
                                cx.listener(move |this, _e, _w, cx| this.toggle_maximize(iid, cx)),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("popout-{sid}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::ExternalLink)
                            .tooltip(t("Pop out"))
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.pop_out_instance(iid, window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("close-{sid}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t("Close"))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.request_close_instance(iid, cx)
                            })),
                    );

                // The tab strip is the pane's title bar: a pill per tab (click to
                // switch, ✕ / middle-click to close, drag to move that tab into
                // another pane), a `+` to add a tab, then the pane controls.
                // Dragging the strip itself drags the whole pane (→ swap on drop).
                let anchor = tabs[0];
                // The edge-split highlight zone for this pane (only while a drag is
                // active and the cursor is over this body).
                let drop_overlay_zone = cx
                    .has_active_drag()
                    .then(|| self.pane_drop.filter(|(a, _)| *a == anchor).map(|(_, z)| z))
                    .flatten();
                let pills: Vec<AnyElement> = tabs
                    .iter()
                    .enumerate()
                    .map(|(i, &tab)| {
                        let tab_title = self.instance_title(tab, cx);
                        let tab_program =
                            self.workspace.instance(tab).and_then(|i| i.program.clone());
                        let tab_active = tab == iid;
                        let tab_wt_id = self.workspace.instance(tab).and_then(|i| i.worktree_id);
                        let tab_is_terminal = self.workspace.instance(tab).map(|i| i.kind)
                            == Some(InstanceKind::Terminal);
                        let tab_pinned = self
                            .workspace
                            .instance(tab)
                            .map(|i| i.pinned)
                            .unwrap_or(false);
                        let pill_bg = if tab_active {
                            cx.theme().background
                        } else {
                            header_bg
                        };

                        // Renaming: swap the title for the shared rename input.
                        if self.rename == Some(RenameTarget::Instance(tab)) {
                            return div()
                                .id(SharedString::from(format!("tab-{}", tab.simple())))
                                .flex()
                                .items_center()
                                .gap_1()
                                .px_1()
                                .h_full()
                                .max_w(px(180.0))
                                .border_r_1()
                                .border_color(cx.theme().border)
                                .bg(pill_bg)
                                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                                    if ev.keystroke.key == "escape" {
                                        this.cancel_rename(cx);
                                    }
                                }))
                                .on_mouse_down_out(
                                    cx.listener(|this, _ev, _w, cx| this.commit_rename(cx)),
                                )
                                .child(agent_icon(
                                    tab_program.as_deref(),
                                    px(12.0),
                                    cx.theme().muted_foreground,
                                ))
                                .child(div().flex_1().child(Input::new(&self.rename_input)))
                                .into_any_element();
                        }

                        let pill_ghost = tab_title.clone();
                        div()
                            .id(SharedString::from(format!("tab-{}", tab.simple())))
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_1()
                            .h_full()
                            .max_w(px(180.0))
                            .border_r_1()
                            .border_color(cx.theme().border)
                            .bg(pill_bg)
                            .text_xs()
                            .text_color(if tab_active {
                                cx.theme().foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .cursor_pointer()
                            // Click switches to this tab.
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.focus_instance(tab, window, cx)
                            }))
                            // Middle-click closes the tab, like a browser.
                            .on_mouse_down(
                                MouseButton::Middle,
                                cx.listener(move |this, _e, _w, cx| {
                                    this.request_close_instance(tab, cx)
                                }),
                            )
                            // Drag a single tab to move it into another pane.
                            // (Innermost on_drag wins, so this beats the strip's
                            // whole-pane drag when you grab a pill.)
                            .on_drag(DragInstance { iid: tab }, move |_, offset, _, cx| {
                                let label = pill_ghost.clone();
                                cx.new(move |_| DragGhost { label, offset })
                            })
                            // While a tab is dragged over this pill, mark the
                            // insertion slot (left half = before, right half =
                            // after). on_drag_move fires for every listener, so we
                            // only act when the cursor is actually over this pill.
                            .on_drag_move::<DragInstance>(cx.listener(
                                move |this, ev: &DragMoveEvent<DragInstance>, _w, cx| {
                                    if !ev.bounds.contains(&ev.event.position) {
                                        return;
                                    }
                                    let mid = ev.bounds.origin.x + ev.bounds.size.width / 2.0;
                                    let slot = if ev.event.position.x < mid { i } else { i + 1 };
                                    this.update_tab_drop(anchor, slot, cx);
                                },
                            ))
                            // Right-click: per-tab menu.
                            .context_menu({
                                let entity = entity.clone();
                                move |menu, window, _cx| {
                                    let pin_label = if tab_pinned { t("Unpin") } else { t("Pin") };
                                    let menu = menu
                                        .item(
                                            PopupMenuItem::new(t("Rename"))
                                                .icon(Icon::empty().path("icons/pencil.svg"))
                                                .on_click(window.listener_for(
                                                    &entity,
                                                    move |this, _, window, cx| {
                                                        this.start_rename_instance(tab, window, cx)
                                                    },
                                                )),
                                        )
                                        .item(
                                            PopupMenuItem::new(t("Duplicate"))
                                                .icon(IconName::Copy)
                                                .on_click(window.listener_for(
                                                    &entity,
                                                    move |this, _, window, cx| {
                                                        this.duplicate_instance(tab, window, cx)
                                                    },
                                                )),
                                        )
                                        .separator()
                                        .item(
                                            PopupMenuItem::new(pin_label)
                                                .icon(Icon::empty().path("icons/pin.svg"))
                                                .on_click(window.listener_for(
                                                    &entity,
                                                    move |this, _, _w, cx| this.toggle_pin(tab, cx),
                                                )),
                                        );
                                    // Clear scrollback (terminals only).
                                    let menu = if tab_is_terminal {
                                        menu.item(
                                            PopupMenuItem::new(t("Clear scrollback"))
                                                .icon(IconName::Delete)
                                                .on_click(window.listener_for(
                                                    &entity,
                                                    move |this, _, _w, cx| {
                                                        this.clear_terminal_scrollback(tab, cx)
                                                    },
                                                )),
                                        )
                                    } else {
                                        menu
                                    };
                                    // Rename the tab's worktree, when it has one.
                                    let menu = if let Some(wid) = tab_wt_id {
                                        menu.item(
                                            PopupMenuItem::new(t("Rename worktree"))
                                                .icon(Icon::empty().path("icons/git-branch.svg"))
                                                .on_click(window.listener_for(
                                                    &entity,
                                                    move |this, _, window, cx| {
                                                        this.start_rename_worktree(wid, window, cx)
                                                    },
                                                )),
                                        )
                                    } else {
                                        menu
                                    };
                                    menu.separator()
                                        .item(
                                            PopupMenuItem::new(t("Close tabs to the left"))
                                                .icon(IconName::PanelLeftClose)
                                                .on_click(window.listener_for(
                                                    &entity,
                                                    move |this, _, window, cx| {
                                                        this.close_tabs_side(tab, false, window, cx)
                                                    },
                                                )),
                                        )
                                        .item(
                                            PopupMenuItem::new(t("Close tabs to the right"))
                                                .icon(IconName::PanelRightClose)
                                                .on_click(window.listener_for(
                                                    &entity,
                                                    move |this, _, window, cx| {
                                                        this.close_tabs_side(tab, true, window, cx)
                                                    },
                                                )),
                                        )
                                        .item(
                                            PopupMenuItem::new(t("Close others"))
                                                .icon(IconName::CircleX)
                                                .on_click(window.listener_for(
                                                    &entity,
                                                    move |this, _, window, cx| {
                                                        this.close_other_tabs(tab, window, cx)
                                                    },
                                                )),
                                        )
                                        .item(
                                            PopupMenuItem::new(t("Close"))
                                                .icon(IconName::Close)
                                                .on_click(window.listener_for(
                                                    &entity,
                                                    move |this, _, _w, cx| {
                                                        this.request_close_instance(tab, cx)
                                                    },
                                                )),
                                        )
                                }
                            })
                            .child(agent_icon(
                                tab_program.as_deref(),
                                px(12.0),
                                cx.theme().muted_foreground,
                            ))
                            // A tab in a worktree always shows its color dot.
                            .children(
                                tab_wt_colors[i]
                                    .map(|c| div().size(px(6.0)).rounded_full().flex_none().bg(c)),
                            )
                            // Pinned tabs show a pin glyph before the title.
                            .children(tab_pinned.then(|| {
                                svg()
                                    .path("icons/pin.svg")
                                    .size(px(10.0))
                                    .flex_none()
                                    .text_color(cx.theme().muted_foreground)
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .pl_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(tab_title),
                            )
                            .child(
                                // stop_propagation so closing a tab isn't a focus/drag.
                                div()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "tabx-{}",
                                            tab.simple()
                                        )))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Close)
                                        .tooltip(t("Close tab"))
                                        .on_click(
                                            cx.listener(move |this, _e, _w, cx| {
                                                this.request_close_instance(tab, cx)
                                            }),
                                        ),
                                    ),
                            )
                            .into_any_element()
                    })
                    .collect();

                // Interleave a thin insertion indicator at the hovered drop slot.
                let tabs_len = tabs.len();
                let drop_here = self.tab_drop.filter(|(a, _)| *a == anchor).map(|(_, i)| i);
                let line_color = cx.theme().primary;
                let mk_line = move || {
                    div()
                        .w(px(2.0))
                        .h_full()
                        .flex_none()
                        .bg(line_color)
                        .into_any_element()
                };
                let mut tab_row: Vec<AnyElement> = Vec::with_capacity(pills.len() + 1);
                for (i, pill) in pills.into_iter().enumerate() {
                    if drop_here == Some(i) {
                        tab_row.push(mk_line());
                    }
                    tab_row.push(pill);
                }
                if drop_here == Some(tabs_len) {
                    tab_row.push(mk_line());
                }

                // A quick click adds a tab with the current preset; holding opens
                // the agent picker (same as the split buttons). Wrapped so the
                // press doesn't also start the strip's pane drag.
                let plus = div()
                    .flex_none()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new(SharedString::from(format!("newtab-{sid}")))
                            .ghost()
                            .xsmall()
                            .label("+")
                            .tooltip(t("New tab (hold to choose agent)"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, e: &MouseDownEvent, _w, cx| {
                                    this.begin_place_press(
                                        anchor,
                                        PlacementMode::Tab,
                                        e.position,
                                        cx,
                                    )
                                }),
                            )
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(move |this, _e, window, cx| {
                                    this.end_place_press(anchor, PlacementMode::Tab, window, cx)
                                }),
                            ),
                    );

                // The strip is the pane's title bar: drag it to move/rearrange the
                // pane (drop on another pane's body swaps; on its strip, tabifies).
                let strip_ghost = self.instance_title(iid, cx);
                let strip = div()
                    .id(SharedString::from(format!("strip-{}", anchor.simple())))
                    .flex_none()
                    .h(rems(1.75))
                    .flex()
                    .items_center()
                    .bg(header_bg)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .cursor_pointer()
                    // Drag the title bar to move the whole pane (→ swap on drop).
                    .on_drag(DragPane { anchor }, move |_, offset, _, cx| {
                        let label = strip_ghost.clone();
                        cx.new(move |_| DragGhost { label, offset })
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("tabrow-{}", anchor.simple())))
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .overflow_hidden()
                            // Hovering the empty area past the pills = append.
                            // (Pills fire after this in capture order and override
                            // when the cursor is over one.)
                            .on_drag_move::<DragInstance>(cx.listener(
                                move |this, ev: &DragMoveEvent<DragInstance>, _w, cx| {
                                    if !ev.bounds.contains(&ev.event.position) {
                                        return;
                                    }
                                    this.update_tab_drop(anchor, tabs_len, cx);
                                },
                            ))
                            .children(tab_row)
                            .child(plus),
                    )
                    // Uniform pane: a worktree badge (dot + name) before controls.
                    // Double-click the name (or the pill menu) to rename it.
                    .children(strip_wt.clone().map(|(c, name, wid)| {
                        let renaming = self.rename == Some(RenameTarget::Worktree(wid));
                        let badge = div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .h_full()
                            .border_l_1()
                            .border_color(cx.theme().border)
                            .child(div().size(px(7.0)).rounded_full().flex_none().bg(c));
                        if renaming {
                            badge
                                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                                    if ev.keystroke.key == "escape" {
                                        this.cancel_rename(cx);
                                    }
                                }))
                                .on_mouse_down_out(
                                    cx.listener(|this, _ev, _w, cx| this.commit_rename(cx)),
                                )
                                .child(div().w(px(110.0)).child(Input::new(&self.rename_input)))
                                .into_any_element()
                        } else {
                            badge
                                .id(SharedString::from(format!("wtbadge-{}", wid.simple())))
                                .cursor_pointer()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                        if ev.click_count >= 2 {
                                            this.start_rename_worktree(wid, window, cx);
                                        }
                                    }),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .max_w(px(110.0))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_color(c)
                                        .child(name),
                                )
                                .into_any_element()
                        }
                    }))
                    .child(controls);

                let mut card = div()
                    .id(SharedString::from(format!("pane-{}", anchor.simple())))
                    .size_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(match (is_active, wt_color) {
                        (true, Some(c)) => c,
                        (false, Some(c)) => c.opacity(0.55),
                        (true, None) => accent,
                        (false, None) => inactive,
                    })
                    .bg(cx.theme().background)
                    .hover(move |s| s.border_color(wt_color.unwrap_or(accent)))
                    // Drop a dragged tab here → strip slot / center tabify / edge split.
                    .on_drop::<DragInstance>(cx.listener(move |this, p: &DragInstance, w, cx| {
                        this.dock_tab(p.iid, anchor, w, cx)
                    }))
                    .drag_over::<DragInstance>(move |s, _, _, _| s.border_color(drop_hl))
                    // Drop a dragged pane here → center swap / edge relocate.
                    .on_drop::<DragPane>(cx.listener(move |this, p: &DragPane, _w, cx| {
                        this.dock_pane(p.anchor, anchor, cx)
                    }))
                    .drag_over::<DragPane>(move |s, _, _, _| s.border_color(drop_hl))
                    .child(strip)
                    .child(
                        // Clicking the body focuses the pane (its active tab). This
                        // lives on the body, not the whole card, so it doesn't
                        // override a tab pill's click-to-switch. A drag over the body
                        // computes the drop zone for the edge-split highlight.
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _ev, window, cx| {
                                    this.focus_instance(iid, window, cx);
                                }),
                            )
                            .on_drag_move::<DragInstance>(cx.listener(
                                move |this, ev: &DragMoveEvent<DragInstance>, _w, cx| {
                                    if !ev.bounds.contains(&ev.event.position) {
                                        return;
                                    }
                                    let zone = drop_zone(ev.bounds, ev.event.position);
                                    this.update_pane_drop(anchor, zone, cx);
                                },
                            ))
                            .on_drag_move::<DragPane>(cx.listener(
                                move |this, ev: &DragMoveEvent<DragPane>, _w, cx| {
                                    if !ev.bounds.contains(&ev.event.position) {
                                        return;
                                    }
                                    let zone = drop_zone(ev.bounds, ev.event.position);
                                    this.update_pane_drop(anchor, zone, cx);
                                },
                            ))
                            .child(content)
                            .children(drop_overlay_zone.map(|z| drop_zone_overlay(z, accent))),
                    );
                if is_active {
                    let glow = wt_color.unwrap_or(cx.theme().primary).opacity(0.35);
                    card = card.shadow(vec![BoxShadow {
                        color: glow,
                        offset: point(px(0.0), px(0.0)),
                        blur_radius: px(8.0),
                        spread_radius: px(0.0),
                        inset: false,
                    }]);
                }
                div()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .p(px(3.0))
                    .child(card)
                    .into_any_element()
            }
            PaneNode::Split {
                direction,
                children,
                sizes,
            } => {
                let horizontal = *direction == SplitDirection::Horizontal;
                // Stable id from the split's instance set: the resize state is
                // keyed by it (persists across renders; resets only when the
                // split's membership changes — i.e. a pane is added/removed).
                let key = node.split_key();
                // Bumped when the split is evened out, to restart its resizable
                // state from the equal layout.
                let nonce = self.split_even_nonce.get(&key).copied().unwrap_or(0);
                let id = SharedString::from(format!("split-{key}-{nonce}"));
                let n = children.len();
                // Apply recorded pixel sizes (a [1.0; n] default means "equal" →
                // let the panels flex). The last panel always flexes so the row
                // fills the container (the sidebar pattern, which is what works).
                let use_sizes = sizes.len() == n && sizes.iter().any(|s| *s > 2.0);
                let mut group = if horizontal {
                    h_resizable(id)
                } else {
                    v_resizable(id)
                };
                // Keep panes usable for agent TUIs: a horizontal split's panes
                // can't shrink below a sane terminal width (the default 100px is
                // ~12 columns, which makes wide TUIs like Claude overflow). The
                // cross axis (height for a horizontal split) keeps the default.
                let min_extent = if horizontal {
                    MIN_PANE_WIDTH
                } else {
                    MIN_PANE_HEIGHT
                };
                for (i, child) in children.iter().enumerate() {
                    let pane = self.render_pane(child, cx);
                    let panel = if use_sizes && i + 1 < n {
                        resizable_panel().size(px(sizes[i]))
                    } else {
                        resizable_panel()
                    };
                    group = group.child(panel.size_range(min_extent..Pixels::MAX).child(pane));
                }
                let resize_key = SharedString::from(key.clone());
                group = group.on_resize(move |state, _window, cx| {
                    let sizes: Vec<f32> = state
                        .read(cx)
                        .sizes()
                        .iter()
                        .map(|p| f32::from(*p))
                        .collect();
                    let weak = cx.try_global::<MuxelHandle>().map(|h| h.0.clone());
                    if let Some(app) = weak.and_then(|w| w.upgrade()) {
                        let key = resize_key.clone();
                        app.update(cx, |app, cx| app.update_split_sizes(key, sizes, cx));
                    }
                });

                // Double-click a divider to even out the split. The gpui resize
                // handle occludes events, so we overlay thin double-click strips at
                // the divider positions (from the recorded sizes), on top of the
                // handles. They act only on a double-click and don't stop single
                // events, so dragging the handle below still works. Only shown when
                // the panes are uneven (recorded pixel sizes present).
                if !use_sizes || n < 2 {
                    return group.into_any_element();
                }
                let mut overlay = div().absolute().top_0().left_0().size_full();
                let mut cumulative = 0.0_f32;
                for (i, s) in sizes.iter().enumerate().take(n - 1) {
                    cumulative += *s;
                    let pos = cumulative;
                    let ekey = key.clone();
                    let base = div()
                        .id(SharedString::from(format!("even-{key}-{i}")))
                        .absolute();
                    let strip = if horizontal {
                        base.top_0()
                            .h_full()
                            .left(px(pos - 5.0))
                            .w(px(10.0))
                            .cursor_col_resize()
                    } else {
                        base.left_0()
                            .w_full()
                            .top(px(pos - 5.0))
                            .h(px(10.0))
                            .cursor_row_resize()
                    }
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                            if ev.click_count >= 2 {
                                this.even_split(ekey.clone(), n, cx);
                                cx.stop_propagation();
                            }
                        }),
                    );
                    overlay = overlay.child(strip);
                }
                div()
                    .relative()
                    .size_full()
                    .child(group.into_any_element())
                    .child(overlay)
                    .into_any_element()
            }
        }
    }

    /// Full-window workspace picker, shown at launch and when switching workspaces.
    /// First-run Terms of Service / Privacy acceptance, shown full-window before
    /// anything else until the current terms version is accepted.
    fn render_terms_screen(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let primary = cx.theme().primary;
        let bullet = |text: &str| {
            div()
                .flex()
                .gap_2()
                .items_start()
                .child(div().text_color(primary).child("•"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(muted)
                        .child(text.to_string()),
                )
        };

        let card = v_flex()
            .gap_3()
            .w(px(520.0))
            .p_5()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        img("muxel.svg")
                            .size(px(44.0))
                            .flex_none()
                            .rounded(cx.theme().radius_lg),
                    )
                    .child(div().text_xl().font_semibold().child(t("Welcome to muxel"))),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t("Please review and accept the terms before you start.")),
            )
            .child(
                v_flex()
                    .gap_2()
                    .py_1()
                    .child(bullet(
                        &t("muxel is free, open-source software provided “as is”, without warranty of any kind."),
                    ))
                    .child(bullet(
                        &t("To the maximum extent permitted by law, the authors accept no liability for any damages arising from its use."),
                    ))
                    .child(bullet(
                        &t("muxel runs locally on your machine and collects no personal data."),
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("terms-full")
                            .ghost()
                            .label(t("View full terms"))
                            .on_click(cx.listener(|_t, _e, _w, cx| {
                                cx.open_url("https://muxel.sh/legal.html")
                            })),
                    )
                    .child(
                        Button::new("terms-license")
                            .ghost()
                            .label(t("License (GPL-3.0)"))
                            .on_click(cx.listener(|_t, _e, _w, cx| {
                                cx.open_url(
                                    "https://github.com/projecthax/muxel/blob/master/LICENSE",
                                )
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .justify_end()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(Button::new("terms-quit").ghost().label(t("Quit")).on_click(
                        cx.listener(|this, _e, _w, cx| {
                            this.confirm_quit = true;
                            cx.quit();
                        }),
                    ))
                    .child(
                        Button::new("terms-accept")
                            .primary()
                            .label(t("I Agree & Continue"))
                            .on_click(cx.listener(|this, _e, _w, cx| this.accept_terms(cx))),
                    ),
            );

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().background)
            .child(card)
            .into_any_element()
    }

    fn render_workspace_selector(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.workspaces.current;
        let hover_bg = cx.theme().sidebar_accent.opacity(0.5);
        let can_delete = self.workspaces.workspaces.len() > 1;
        let mut list = v_flex().gap_1().w_full();
        for meta in &self.workspaces.workspaces {
            let id = meta.id;
            let name = meta.name.clone();
            let selected = current == Some(id);
            list =
                list.child(
                    div()
                        .id(SharedString::from(format!("workspace-{}", id.simple())))
                        .px_3()
                        .py_2()
                        .rounded(cx.theme().radius)
                        .flex()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .bg(if selected {
                            cx.theme().sidebar_accent
                        } else {
                            cx.theme().secondary
                        })
                        .text_color(if selected {
                            cx.theme().sidebar_accent_foreground
                        } else {
                            cx.theme().foreground
                        })
                        .hover(move |s| s.bg(hover_bg))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev, window, cx| {
                                this.enter_workspace(id, window, cx)
                            }),
                        )
                        .child(Icon::new(IconName::Folder).small())
                        .child(div().flex_1().text_sm().child(meta.name.clone()))
                        .children(can_delete.then(|| {
                            let label = name.clone();
                            // stop_propagation so the click doesn't also enter the workspace.
                            div()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "del-workspace-{}",
                                        id.simple()
                                    )))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .tooltip(t("Delete workspace"))
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        this.request_confirm(
                                        t("Delete workspace?"),
                                        tf(
                                            "Workspace “{label}” and its layout will be deleted.",
                                            &[("label", &label)],
                                        ),
                                        t("Delete"),
                                        ConfirmAction::DeleteWorkspace(id),
                                        cx,
                                    )
                                    })),
                                )
                        })),
                );
        }

        let mut card = v_flex()
            .gap_3()
            .w(px(440.0))
            .p_5()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_lg()
            .child(
                div()
                    .text_xl()
                    .font_semibold()
                    .child(t("Choose a workspace")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(
                        "Each workspace keeps its own projects and terminal layout.",
                    )),
            )
            .child(
                div()
                    .id("workspaces-list")
                    .max_h(px(320.0))
                    .overflow_y_scroll()
                    .child(list),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().child(Input::new(&self.workspace_name_input)))
                    .child(
                        Button::new("create-workspace")
                            .primary()
                            .label(t("Create"))
                            .on_click(cx.listener(|this, _e, window, cx| {
                                this.create_workspace_from_input(window, cx)
                            })),
                    ),
            );

        // Always offer a way out (otherwise first-run users with no workspace yet
        // are trapped); when switching at runtime, also allow backing out.
        card = card.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    Button::new("quit-selector")
                        .ghost()
                        .label(t("Quit"))
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            this.confirm_quit = true;
                            cx.quit();
                        })),
                )
                .children(self.current_workspace.is_some().then(|| {
                    Button::new("cancel-selector")
                        .ghost()
                        .label(t("Cancel"))
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            this.show_workspace_selector = false;
                            cx.notify();
                        }))
                })),
        );

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().background)
            .child(card)
            .into_any_element()
    }

    fn render_dashboard(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut container = div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(cx.theme().background)
            .child(
                div()
                    .text_lg()
                    .text_color(cx.theme().foreground)
                    .child(t("All agents")),
            );

        for project in &self.workspace.projects {
            let pid = project.id;
            let mut card = div()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .bg(cx.theme().sidebar)
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(project.name.clone()),
                );
            for iid in project.instances() {
                let inst = self.workspace.instance(iid);
                let title = inst.map(|i| i.title.clone()).unwrap_or_default();
                let program = inst.and_then(|i| i.program.clone());
                let status = self.terminals.get(&iid).map(|v| v.read(cx).status());
                let color = status
                    .map(|s| status_hsla(s, cx))
                    .unwrap_or(cx.theme().muted_foreground);
                card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .pl(px(10.0))
                        .py_1()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev, window, cx| {
                                this.show_dashboard = false;
                                this.select_project(pid, window, cx);
                                this.focus_instance(iid, window, cx);
                            }),
                        )
                        .child(agent_icon(program.as_deref(), px(15.0), color))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().foreground)
                                .child(title),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(status_label(status)),
                        ),
                );
            }
            container = container.child(card);
        }

        container.into_any_element()
    }

    /// A sidebar subcategory header for a worktree: color dot + name. Double-click
    /// (or the context menu) renames it; the menu can also start an agent in it.
    fn sidebar_worktree_subheader(
        &self,
        wid: Uuid,
        entity: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(w) = self.workspace.worktree(wid) else {
            return div().into_any_element();
        };
        let color = worktree_color(w.color);
        let name: SharedString = w.name.clone().into();
        let detached = w.detached;
        let changes = self.worktree_changes.get(&wid).copied().unwrap_or(0);
        // Runner list (index + name) for the per-runner "review" menu items.
        let runners: Vec<(usize, SharedString)> = self
            .runners
            .iter()
            .enumerate()
            .map(|(i, r)| (i, r.name.clone().into()))
            .collect();
        let gh = self.gh_available;
        let base = div()
            .ml_2()
            .mr_1()
            .px_2()
            .py(px(2.0))
            .flex()
            .items_center()
            .gap_2()
            .child(div().size(px(7.0)).rounded_full().flex_none().bg(color));
        if self.rename == Some(RenameTarget::Worktree(wid)) {
            return base
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                    if ev.keystroke.key == "escape" {
                        this.cancel_rename(cx);
                    }
                }))
                .on_mouse_down_out(cx.listener(|this, _ev, _w, cx| this.commit_rename(cx)))
                .child(div().flex_1().child(Input::new(&self.rename_input)))
                .into_any_element();
        }
        let entity = entity.clone();
        base.id(SharedString::from(format!("wt-hdr-{}", wid.simple())))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    if ev.click_count >= 2 {
                        this.start_rename_worktree(wid, window, cx);
                    }
                }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .font_semibold()
                    .text_color(color.opacity(0.85))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(name),
            )
            // Uncommitted-change count (refreshed each tick).
            .children((changes > 0).then(|| {
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(tf(
                        "{changes} changed",
                        &[("changes", &changes.to_string())],
                    ))
            }))
            .children(detached.then(|| Tag::new().small().child(t("kept"))))
            .context_menu(move |menu, window, _cx| {
                let mut menu = menu
                    .item(
                        PopupMenuItem::new(t("New agent here"))
                            .icon(IconName::Plus)
                            .on_click(window.listener_for(&entity, move |this, _, window, cx| {
                                this.spawn_into_worktree(wid, window, cx)
                            })),
                    )
                    .item(
                        PopupMenuItem::new(t("View changes"))
                            .icon(IconName::Eye)
                            .on_click(window.listener_for(&entity, move |this, _, window, cx| {
                                this.open_worktree_diff(wid, window, cx)
                            })),
                    )
                    .separator();
                // Spawn each runner (Review, Security Review, …) inside the worktree.
                for (i, rname) in &runners {
                    let i = *i;
                    menu = menu.item(
                        PopupMenuItem::new(rname.clone())
                            .icon(IconName::Play)
                            .on_click(window.listener_for(&entity, move |this, _, window, cx| {
                                this.run_runner_in_worktree(i, wid, window, cx)
                            })),
                    );
                }
                // GitHub: push the branch / create or open a PR (when `gh` exists).
                if gh {
                    menu = menu
                        .separator()
                        .item(
                            PopupMenuItem::new(t("Push branch"))
                                .icon(IconName::ArrowUp)
                                .on_click(window.listener_for(
                                    &entity,
                                    move |this, _, window, cx| {
                                        this.worktree_push_branch(wid, window, cx)
                                    },
                                )),
                        )
                        .item(
                            PopupMenuItem::new(t("Create PR…"))
                                .icon(IconName::ExternalLink)
                                .on_click(window.listener_for(
                                    &entity,
                                    move |this, _, window, cx| {
                                        this.worktree_create_pr(wid, window, cx)
                                    },
                                )),
                        )
                        .item(
                            PopupMenuItem::new(t("Open PR"))
                                .icon(IconName::Github)
                                .on_click(window.listener_for(
                                    &entity,
                                    move |this, _, window, cx| {
                                        this.worktree_open_pr(wid, window, cx)
                                    },
                                )),
                        );
                }
                menu = menu
                    .separator()
                    .item(
                        PopupMenuItem::new(t("Rename worktree"))
                            .icon(Icon::empty().path("icons/pencil.svg"))
                            .on_click(window.listener_for(&entity, move |this, _, window, cx| {
                                this.start_rename_worktree(wid, window, cx)
                            })),
                    )
                    .item(
                        PopupMenuItem::new(t("Discard changes…"))
                            .icon(IconName::Undo)
                            .on_click(window.listener_for(&entity, move |this, _, _w, cx| {
                                this.request_confirm(
                                    t("Discard changes?"),
                                    "Reset this worktree to its base branch, discarding all \
                                 the agent's work (uncommitted changes and commits). The \
                                 worktree is kept.",
                                    t("Discard changes"),
                                    ConfirmAction::DiscardWorktreeChanges(wid),
                                    cx,
                                )
                            })),
                    )
                    .item(
                        PopupMenuItem::new(t("Discard worktree…"))
                            .icon(IconName::Delete)
                            .on_click(window.listener_for(&entity, move |this, _, _w, cx| {
                                this.request_confirm(
                                    t("Discard worktree?"),
                                    t("Close its panes and delete the worktree and its branch."),
                                    t("Discard worktree"),
                                    ConfirmAction::DiscardWorktree(wid),
                                    cx,
                                )
                            })),
                    );
                // Kept worktrees can also be resolved (commit / merge / keep).
                if detached {
                    menu = menu.separator().item(
                        PopupMenuItem::new(t("Resolve…"))
                            .icon(IconName::Check)
                            .on_click(window.listener_for(&entity, move |this, _, _w, cx| {
                                this.review_worktree(wid, cx)
                            })),
                    );
                }
                menu
            })
            .into_any_element()
    }

    /// The NOTIFICATIONS category at the top of the sidebar: a header (with a
    /// clear-all) and one click-to-navigate, dismissable row per notification.
    fn render_notifications_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let radius = cx.theme().radius;
        let hover_col = cx.theme().sidebar_accent.opacity(0.45);
        let has = !self.notifications.is_empty();

        let header = div()
            .px_3()
            .pt_3()
            .pb_1()
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .font_semibold()
                    .text_color(muted)
                    .child(t("NOTIFICATIONS")),
            )
            .children(has.then(|| {
                // stop_propagation so the clear-all click isn't anything else.
                div()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new("notif-clear-all")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t("Clear all notifications"))
                            .on_click(cx.listener(|this, _e, _w, cx| this.clear_notifications(cx))),
                    )
            }));

        let mut section = div().flex().flex_col().child(header);

        if !has {
            return section.child(
                div()
                    .ml_2()
                    .mr_1()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child(t("No notifications")),
            );
        }

        // Newest first.
        for n in self.notifications.iter().rev() {
            let nid = n.id;
            let dot = n.kind.dot(cx);
            let title: SharedString = n.title.clone().into();
            let sub: SharedString = n.subtitle.clone().into();
            section = section.child(
                div()
                    .id(SharedString::from(format!("notif-{}", nid.simple())))
                    .ml_2()
                    .mr_1()
                    .px_2()
                    .py_1()
                    .rounded(radius)
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover_col))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, window, cx| {
                            this.open_notification(nid, window, cx)
                        }),
                    )
                    .child(div().size(px(8.0)).rounded_full().flex_none().bg(dot))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_xs()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(sub),
                            ),
                    )
                    .child(
                        // stop_propagation so dismissing isn't a navigate.
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Button::new(SharedString::from(format!(
                                    "notif-x-{}",
                                    nid.simple()
                                )))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip(t("Dismiss"))
                                .on_click(cx.listener(
                                    move |this, _e, _w, cx| this.dismiss_notification(nid, cx),
                                )),
                            ),
                    ),
            );
        }
        section
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_pid = self.workspace.active_project;
        let entity = cx.entity();
        let mut list = div()
            .id("sidebar-scroll")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .bg(cx.theme().sidebar)
            // NOTIFICATIONS sits above PROJECTS.
            .child(self.render_notifications_section(cx))
            .child(
                div()
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child(t("PROJECTS")),
            );

        for (ix, project) in self.workspace.projects.iter().enumerate() {
            let pid = project.id;
            let active = Some(pid) == active_pid;
            let collapsed = self.collapsed.contains(&pid);
            let renaming = self.rename == Some(RenameTarget::Project(pid));
            let name: SharedString = project.name.clone().into();
            let has_startup = !project.startup.is_empty();
            let memory_on = project.memory_enabled;
            // Where this project's git runs (local, or remote over its host). No
            // keychain read here — menu git ops reuse the pane's ControlMaster.
            let menu_loc: integrations::RepoLoc = match project.remote.as_ref().and_then(|r| {
                let host = self.remotes.iter().find(|h| h.id == r.host_id)?;
                Some((host, r))
            }) {
                Some((host, r)) => integrations::RepoLoc::remote(
                    host.clone(),
                    r.remote_root.clone(),
                    Self::control_path_for(r.host_id),
                    None,
                ),
                None => integrations::RepoLoc::Local(project.root_path.clone()),
            };
            let is_repo = self.project_branches.get(&pid).is_some_and(|b| b.is_some());
            let is_local = project.remote.is_none();
            let current_branch = self.project_branches.get(&pid).cloned().flatten();
            let drop_hl = cx.theme().sidebar_accent;
            let chevron = if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            };
            let base = div()
                .id(SharedString::from(format!("proj-row-{ix}")))
                .mx_1()
                .px_1()
                .py_1()
                .rounded(cx.theme().radius)
                .flex()
                .items_center()
                .gap_1()
                .text_sm()
                .bg(if active {
                    cx.theme().sidebar_accent
                } else {
                    cx.theme().sidebar
                })
                .text_color(if active {
                    cx.theme().sidebar_accent_foreground
                } else {
                    cx.theme().sidebar_foreground
                })
                .child(
                    div()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev, _w, cx| {
                                cx.stop_propagation();
                                this.toggle_collapse(pid, cx);
                            }),
                        )
                        .child(Icon::new(chevron).small()),
                )
                .child(Icon::new(IconName::Folder).small());
            let row = if renaming {
                base.on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                    if ev.keystroke.key == "escape" {
                        this.cancel_rename(cx);
                    }
                }))
                .on_mouse_down_out(cx.listener(|this, _ev, _w, cx| this.commit_rename(cx)))
                .child(div().flex_1().child(Input::new(&self.rename_input)))
                .into_any_element()
            } else {
                base.cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            if ev.click_count >= 2 {
                                this.start_rename_project(pid, window, cx);
                            } else {
                                this.select_project(pid, window, cx);
                            }
                        }),
                    )
                    .child(
                        // Name + the repo's current branch (cached) with a branch icon.
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .children(project.is_remote().then(|| {
                                // Remote (SSH) project badge.
                                Icon::new(IconName::Network)
                                    .small()
                                    .flex_none()
                                    .text_color(cx.theme().muted_foreground)
                            }))
                            .child(
                                // Name yields/truncates; the branch chip at the end
                                // stays full.
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(project.name.clone()),
                            )
                            .children(self.project_branches.get(&pid).cloned().flatten().map(
                                |b| {
                                    div()
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(
                                            // currentColor stroke → must set the color
                                            // on the svg itself, not just the parent.
                                            svg()
                                                .path("icons/git-branch.svg")
                                                .size(px(12.0))
                                                .flex_none()
                                                .text_color(cx.theme().muted_foreground),
                                        )
                                        .child(div().whitespace_nowrap().child(b))
                                },
                            )),
                    )
                    .children(memory_on.then(|| {
                        div()
                            .flex_none()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Button::new(SharedString::from(format!("memory-{ix}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::File)
                                    .tooltip(t("Project memory (.muxel/MEMORY.md)"))
                                    .on_click(cx.listener(move |this, _e, window, cx| {
                                        this.open_project_memory(pid, window, cx)
                                    })),
                            )
                    }))
                    .child(
                        div()
                            .flex_none()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Button::new(SharedString::from(format!("files-{ix}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::empty().path("icons/files.svg"))
                                    .selected(
                                        self.show_file_browser
                                            && self.file_browser_pid == Some(pid),
                                    )
                                    .tooltip(t("File browser"))
                                    .on_click(cx.listener(move |this, _e, window, cx| {
                                        this.toggle_file_browser(pid, window, cx)
                                    })),
                            ),
                    )
                    .on_drag(DragProject { from: ix }, move |_, offset, _, cx| {
                        let label = name.clone();
                        cx.new(move |_| DragGhost { label, offset })
                    })
                    .on_drop::<DragProject>(cx.listener(move |this, p: &DragProject, _w, cx| {
                        this.reorder_projects(p.from, ix, cx)
                    }))
                    .drag_over::<DragProject>(move |s, _, _, _| s.bg(drop_hl))
                    .context_menu({
                        let entity = entity.clone();
                        move |menu, window, cx| {
                            let mut menu = menu
                                .item(
                                    PopupMenuItem::new(t("Rename"))
                                        .icon(Icon::empty().path("icons/pencil.svg"))
                                        .on_click(window.listener_for(
                                            &entity,
                                            move |this, _, window, cx| {
                                                this.start_rename_project(pid, window, cx)
                                            },
                                        )),
                                )
                                .separator()
                                .item(
                                    PopupMenuItem::new(t("Save panes as startup"))
                                        .icon(IconName::Star)
                                        .on_click(window.listener_for(
                                            &entity,
                                            move |this, _, window, cx| {
                                                this.save_project_startup(pid, window, cx)
                                            },
                                        )),
                                )
                                .item(
                                    PopupMenuItem::new(if memory_on {
                                        t("Disable shared memory")
                                    } else {
                                        t("Enable shared memory")
                                    })
                                    .icon(IconName::File)
                                    .on_click(
                                        window.listener_for(
                                            &entity,
                                            move |this, _, _window, cx| {
                                                this.toggle_project_memory(pid, cx)
                                            },
                                        ),
                                    ),
                                );
                            if has_startup {
                                menu = menu.item(
                                    PopupMenuItem::new(t("Launch startup agents"))
                                        .icon(IconName::Play)
                                        .on_click(window.listener_for(
                                            &entity,
                                            move |this, _, window, cx| {
                                                this.launch_project_startup(pid, window, cx)
                                            },
                                        )),
                                );
                            }
                            // Git actions (only when the project is a repo).
                            if is_repo {
                                menu = menu.separator();
                                // Review changes — local projects only (the diff
                                // pane runs `git diff` on a local path).
                                if is_local {
                                    menu = menu.item(
                                        PopupMenuItem::new(t("Git diff"))
                                            .icon(Icon::empty().path("icons/diff.svg"))
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, window, cx| {
                                                    this.open_project_diff(pid, window, cx)
                                                },
                                            )),
                                    );
                                }
                                let branches = integrations::list_branches(&menu_loc);
                                if !branches.is_empty() {
                                    let entity_sb = entity.clone();
                                    let cur = current_branch.clone();
                                    menu = menu.submenu_with_icon(
                                        Some(Icon::empty().path("icons/git-branch.svg")),
                                        t("Switch branch"),
                                        window,
                                        cx,
                                        move |mut sm, window, _c| {
                                            for b in &branches {
                                                let is_cur = Some(b) == cur.as_ref();
                                                let bn = b.clone();
                                                let mut item = PopupMenuItem::new(b.clone());
                                                if is_cur {
                                                    item =
                                                        item.icon(IconName::Check).disabled(true);
                                                }
                                                sm = sm.item(item.on_click(window.listener_for(
                                                    &entity_sb,
                                                    move |this, _, window, cx| {
                                                        this.switch_branch(
                                                            pid,
                                                            bn.clone(),
                                                            window,
                                                            cx,
                                                        )
                                                    },
                                                )));
                                            }
                                            sm
                                        },
                                    );
                                }
                                menu = menu
                                    .item(
                                        PopupMenuItem::new(t("New branch…"))
                                            .icon(IconName::Plus)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, window, cx| {
                                                    this.open_git_modal(
                                                        pid,
                                                        GitModalKind::NewBranch,
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            )),
                                    )
                                    .item(
                                        PopupMenuItem::new(t("Commit…"))
                                            .icon(IconName::Check)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, window, cx| {
                                                    this.open_git_modal(
                                                        pid,
                                                        GitModalKind::Commit,
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            )),
                                    )
                                    .item(
                                        PopupMenuItem::new(t("Pull"))
                                            .icon(IconName::ArrowDown)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, window, cx| {
                                                    this.run_project_git(
                                                        pid,
                                                        t("Pulled").into(),
                                                        t("Pull failed").into(),
                                                        integrations::git_pull,
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            )),
                                    )
                                    .item(
                                        PopupMenuItem::new(t("Push"))
                                            .icon(IconName::ArrowUp)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, window, cx| {
                                                    this.run_project_git(
                                                        pid,
                                                        t("Pushed").into(),
                                                        t("Push failed").into(),
                                                        integrations::git_push,
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            )),
                                    )
                                    .item(
                                        PopupMenuItem::new(t("Fetch"))
                                            .icon(IconName::Replace)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, window, cx| {
                                                    this.run_project_git(
                                                        pid,
                                                        t("Fetched").into(),
                                                        t("Fetch failed").into(),
                                                        integrations::git_fetch,
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            )),
                                    )
                                    .separator()
                                    .item(
                                        PopupMenuItem::new(t("Stash changes"))
                                            .icon(IconName::Inbox)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, window, cx| {
                                                    this.run_project_git(
                                                        pid,
                                                        t("Stashed").into(),
                                                        t("Stash failed").into(),
                                                        integrations::git_stash,
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            )),
                                    )
                                    .item(
                                        PopupMenuItem::new(t("Pop stash"))
                                            .icon(IconName::Redo)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, _w, cx| {
                                                    this.request_stash_pop(pid, cx)
                                                },
                                            )),
                                    )
                                    .item(
                                        PopupMenuItem::new(t("Drop stash"))
                                            .icon(IconName::Delete)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, _w, cx| {
                                                    this.request_stash_drop(pid, cx)
                                                },
                                            )),
                                    );
                            }
                            menu.separator().item(
                                PopupMenuItem::new(t("Remove"))
                                    .icon(IconName::CircleX)
                                    .on_click(window.listener_for(
                                        &entity,
                                        move |this, _, _window, cx| {
                                            this.request_confirm(
                                                t("Remove project?"),
                                                t("The project and its panes will be removed."),
                                                t("Remove"),
                                                ConfirmAction::DeleteProject(pid),
                                                cx,
                                            )
                                        },
                                    )),
                            )
                        }
                    })
                    .into_any_element()
            };
            list = list.child(row);

            if !collapsed {
                // Group instances by worktree: plain (no-worktree) panes first,
                // then a subcategory per worktree (a colored subheader followed by
                // its instances).
                let mut ungrouped: Vec<Uuid> = Vec::new();
                let mut groups: Vec<(Uuid, Vec<Uuid>)> = Vec::new();
                for iid in project.instances() {
                    match self.workspace.instance(iid).and_then(|i| i.worktree_id) {
                        Some(wid) => match groups.iter_mut().find(|(w, _)| *w == wid) {
                            Some(g) => g.1.push(iid),
                            None => groups.push((wid, vec![iid])),
                        },
                        None => ungrouped.push(iid),
                    }
                }
                let mut ordered: Vec<(Option<Uuid>, Uuid)> =
                    ungrouped.into_iter().map(|i| (None, i)).collect();
                for (wid, members) in groups {
                    for iid in members {
                        ordered.push((Some(wid), iid));
                    }
                }
                let mut shown_wt: HashSet<Uuid> = HashSet::new();
                for (wid_opt, iid) in ordered {
                    // Emit the worktree subheader before its first instance.
                    if let Some(wid) = wid_opt
                        && shown_wt.insert(wid)
                    {
                        list = list.child(self.sidebar_worktree_subheader(wid, &entity, cx));
                    }
                    let inst = self.workspace.instance(iid);
                    let program = inst.and_then(|i| i.program.clone());
                    let custom = inst
                        .and_then(|i| i.custom_name.clone())
                        .filter(|c| !c.is_empty());
                    let meta = inst.map(|i| i.title.clone()).unwrap_or_default();
                    let (app_title, status) = if let Some(view) = self.terminals.get(&iid) {
                        let view = view.read(cx);
                        // Shells show their cwd: strip the `user@host:` OSC prefix.
                        // Agent titles have no such prefix and pass through unchanged.
                        (
                            view.title()
                                .map(|t| shell_dir_title(&t).to_string())
                                .unwrap_or(meta),
                            view.status(),
                        )
                    } else if let Some(ed) = self.editors.get(&iid) {
                        (ed.read(cx).title(), AgentStatus::Idle)
                    } else {
                        (meta, AgentStatus::Idle)
                    };
                    let display = match custom {
                        Some(c) => format!("{c} — {app_title}"),
                        None => app_title,
                    };
                    let ghost_label: SharedString = display.clone().into();
                    let is_sel = self.active_instance == Some(iid);
                    let renaming = self.rename == Some(RenameTarget::Instance(iid));
                    let hover_col = cx.theme().sidebar_accent.opacity(0.45);
                    let drop_hl = cx.theme().primary.opacity(0.3);
                    let mut base = div()
                        .id(SharedString::from(format!("inst-row-{}", iid.simple())))
                        .ml_4()
                        .mr_1()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .flex()
                        .items_center()
                        .gap_2()
                        .bg(if is_sel {
                            cx.theme().sidebar_accent
                        } else {
                            cx.theme().sidebar
                        })
                        .child(agent_icon(
                            program.as_deref(),
                            px(15.0),
                            status_hsla(status, cx),
                        ))
                        // Worktree dot (matches the pane outline color).
                        .children(
                            self.instance_worktree_color(iid)
                                .map(|c| div().size(px(8.0)).rounded_full().flex_none().bg(c)),
                        );
                    if !is_sel {
                        base = base.hover(move |s| s.bg(hover_col));
                    }
                    let row = if renaming {
                        base.on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                            if ev.keystroke.key == "escape" {
                                this.cancel_rename(cx);
                            }
                        }))
                        .on_mouse_down_out(cx.listener(|this, _ev, _w, cx| this.commit_rename(cx)))
                        .child(div().flex_1().child(Input::new(&self.rename_input)))
                        .into_any_element()
                    } else {
                        base.cursor_pointer()
                            // Focus on click (mouse-up), not mouse-down: focusing
                            // the terminal during a press on a draggable row gets
                            // reset by the drag/release handling, so the pane would
                            // only highlight and you couldn't type. Double-click
                            // renames (like before).
                            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                                if matches!(ev, ClickEvent::Mouse(e) if e.up.click_count >= 2) {
                                    this.start_rename_instance(iid, window, cx);
                                } else {
                                    this.select_instance(iid, window, cx);
                                }
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(cx.theme().sidebar_foreground)
                                    .child(display),
                            )
                            .child(status_tag(status))
                            .on_drag(DragInstance { iid }, move |_, offset, _, cx| {
                                let label = ghost_label.clone();
                                cx.new(move |_| DragGhost { label, offset })
                            })
                            .on_drop::<DragInstance>(cx.listener(
                                move |this, p: &DragInstance, _w, cx| {
                                    this.swap_terminals(p.iid, iid, cx)
                                },
                            ))
                            .drag_over::<DragInstance>(move |s, _, _, _| s.bg(drop_hl))
                            .context_menu({
                                let entity = entity.clone();
                                move |menu, window, _cx| {
                                    menu.item(
                                        PopupMenuItem::new(t("Rename"))
                                            .icon(Icon::empty().path("icons/pencil.svg"))
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, window, cx| {
                                                    this.start_rename_instance(iid, window, cx)
                                                },
                                            )),
                                    )
                                    .item(
                                        PopupMenuItem::new(t("Duplicate"))
                                            .icon(IconName::Copy)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, window, cx| {
                                                    this.duplicate_instance(iid, window, cx)
                                                },
                                            )),
                                    )
                                    .separator()
                                    .item(
                                        PopupMenuItem::new(t("Kill"))
                                            .icon(IconName::CircleX)
                                            .on_click(window.listener_for(
                                                &entity,
                                                move |this, _, _window, cx| {
                                                    this.request_close_instance(iid, cx)
                                                },
                                            )),
                                    )
                                }
                            })
                            .into_any_element()
                    };
                    list = list.child(row);
                }

                // Kept (detached) worktrees: a subheader with no instances.
                let detached: Vec<Uuid> = self
                    .workspace
                    .worktrees
                    .iter()
                    .filter(|w| w.project_id == pid && w.detached)
                    .map(|w| w.id)
                    .collect();
                for wid in detached {
                    list = list.child(self.sidebar_worktree_subheader(wid, &entity, cx));
                }
            }
        }

        list.child(div().flex_1()).child(
            div()
                .p_2()
                .flex()
                .items_center()
                .gap_1()
                .w_full()
                .min_w_0()
                // "New Project" takes the available width and may shrink; the
                // remote-SSH icon button is fixed so it's never clipped at a
                // narrow sidebar width.
                .child(
                    div().flex_1().min_w_0().overflow_hidden().child(
                        Button::new("new-project")
                            .ghost()
                            .icon(IconName::Plus)
                            .label(t("New Project"))
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.new_project_dialog(window, cx);
                            })),
                    ),
                )
                .child(
                    div().flex_none().child(
                        Button::new("new-remote-project")
                            .ghost()
                            .icon(IconName::Network)
                            .tooltip(t("New remote project (SSH)"))
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.open_remote_project_modal(window, cx);
                            })),
                    ),
                ),
        )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut bar = div()
            .flex_none()
            .h(rems(2.5))
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .bg(cx.theme().title_bar)
            // Click the toolbar chrome to deselect the active pane (so Ctrl+P and
            // other muxel shortcuts go to muxel instead of the focused terminal).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e: &MouseDownEvent, window, cx| this.deselect_pane(window, cx)),
            );

        // Preset split-button: the body opens a new pane with the current
        // preset; the caret picks the active preset / sets the default.
        let current = self.current_agent_preset();
        let current_name = current.name.clone();
        let current_program = current.program.clone();
        let current_id = current.id;
        let default_id = self.active_default_preset_id();
        let preset_items: Vec<(Uuid, String, bool, Option<String>)> = self
            .presets
            .iter()
            .filter(|p| self.agent_runnable(p))
            .map(|p| {
                (
                    p.id,
                    p.name.clone(),
                    Some(p.id) == default_id,
                    p.program.clone(),
                )
            })
            .collect();
        bar = bar.child(
            DropdownButton::new("preset-run")
                .button(
                    Button::new("preset-run-btn")
                        .ghost()
                        .small()
                        .icon(agent_icon_obj(current_program.as_deref()))
                        .label(current_name.clone())
                        .tooltip(t("New pane with the current preset"))
                        .on_click(cx.listener(|this, _ev, window, cx| {
                            this.add_agent(SplitDirection::Horizontal, window, cx)
                        })),
                )
                .dropdown_menu(move |mut menu, _window, _cx| {
                    for (id, name, is_default, program) in preset_items.iter() {
                        let label = if *is_default {
                            format!("★ {name}")
                        } else {
                            name.clone()
                        };
                        menu = menu.menu_with_icon(
                            label,
                            agent_icon_obj(program.as_deref()),
                            Box::new(SetPreset(*id)),
                        );
                    }
                    menu = menu.separator();
                    menu.menu(
                        t("Set current as default"),
                        Box::new(SetDefaultPreset(current_id)),
                    )
                }),
        );

        bar.child(div().w(px(6.0)))
            .child(
                Button::new("run-task")
                    .ghost()
                    .small()
                    .icon(IconName::Play)
                    .label(t("Run task"))
                    .tooltip(t("Run a saved task (review, security review, …)"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _w, cx| {
                            this.runners_menu = Some(e.position);
                            cx.notify();
                        }),
                    ),
            )
            .child(
                Button::new("loops-btn")
                    .ghost()
                    .small()
                    .icon(IconName::Redo)
                    .label(t("Loops"))
                    .tooltip(t("Scheduled loops — run a prompt on a timer"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _w, cx| {
                            this.loops_menu = Some(e.position);
                            cx.notify();
                        }),
                    ),
            )
            .child(div().w(px(6.0)))
            .child(
                Button::new("toggle-tmux")
                    .ghost()
                    .icon(IconName::SquareTerminal)
                    .selected(self.use_tmux)
                    .tooltip(t("Run in a tmux session"))
                    .on_click(cx.listener(|this, _ev, _window, cx| this.toggle_tmux(cx))),
            )
            .child(
                Button::new("toggle-worktree")
                    .ghost()
                    .icon(Icon::empty().path("icons/git-branch.svg"))
                    .selected(self.use_worktree)
                    .tooltip(t("Create a git worktree"))
                    .on_click(cx.listener(|this, _ev, _window, cx| this.toggle_worktree(cx))),
            )
            .child(div().w(px(6.0)))
            .child(
                Button::new("split-right")
                    .ghost()
                    .icon(IconName::PanelRight)
                    .tooltip(t("Split right"))
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.add_agent(SplitDirection::Horizontal, window, cx)
                    })),
            )
            .child(
                Button::new("split-down")
                    .ghost()
                    .icon(IconName::PanelBottom)
                    .tooltip(t("Split down"))
                    .on_click(cx.listener(|this, _ev, window, cx| {
                        this.add_agent(SplitDirection::Vertical, window, cx)
                    })),
            )
            .child(
                Button::new("restart")
                    .ghost()
                    .icon(IconName::Play)
                    .disabled(self.active_is_editor())
                    .tooltip(t("Restart agent"))
                    .on_click(cx.listener(|this, _ev, window, cx| this.restart_active(window, cx))),
            )
            .child(
                Button::new("close")
                    .ghost()
                    .icon(IconName::Close)
                    .tooltip(t("Close pane"))
                    .on_click(cx.listener(|this, _ev, window, cx| this.close_active(window, cx))),
            )
    }

    /// The Ctrl+P search palette: a filter box + a results list (files in the
    /// active project, named instances, and a "create new file" entry).
    fn render_search_palette(&self, cx: &mut Context<Self>) -> AnyElement {
        let active_root = self.workspace.active().map(|p| p.root_path.clone());
        let muted = cx.theme().muted_foreground;
        let mut list = v_flex().w_full().gap(px(1.0));
        let mut last_section: Option<SharedString> = None;
        for (i, item) in self.search_results.iter().enumerate() {
            // Group the results under "Projects & panes" vs "Files" headers.
            let section = match item {
                SearchItem::FocusInstance(_) => t("Projects & panes"),
                SearchItem::RunCommand(_) => t("Commands"),
                SearchItem::OpenFile(_) | SearchItem::CreateFile(_) => t("Files"),
            };
            if last_section.as_ref() != Some(&section) {
                last_section = Some(section.clone());
                list = list.child(
                    div()
                        .px_3()
                        .pt_2()
                        .pb_1()
                        .text_xs()
                        .font_semibold()
                        .text_color(muted)
                        .child(section),
                );
            }

            let selected = i == self.search_selected;
            let inst = if let SearchItem::FocusInstance(iid) = item {
                self.workspace.instance(*iid)
            } else {
                None
            };
            // Icon: agent logo for terminal instances, a file glyph for editor
            // instances and on-disk files.
            let icon: AnyElement = match item {
                SearchItem::FocusInstance(_)
                    if inst.map(|x| x.kind) != Some(InstanceKind::Editor) =>
                {
                    agent_icon(inst.and_then(|x| x.program.as_deref()), px(16.0), muted)
                        .into_any_element()
                }
                SearchItem::RunCommand(_) => {
                    Icon::new(IconName::ChevronRight).small().into_any_element()
                }
                _ => Icon::new(IconName::File).small().into_any_element(),
            };
            let (label, sub): (String, String) = match item {
                SearchItem::FocusInstance(iid) => {
                    let inst = self.workspace.instance(*iid);
                    let label = inst
                        .map(|i| {
                            i.custom_name
                                .clone()
                                .filter(|c| !c.is_empty())
                                .unwrap_or_else(|| i.title.clone())
                        })
                        .unwrap_or_default();
                    let proj = inst
                        .and_then(|i| self.workspace.project(i.project_id))
                        .map(|p| p.name.clone())
                        .unwrap_or_default();
                    (label, proj)
                }
                SearchItem::OpenFile(path) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let rel = active_root
                        .as_ref()
                        .and_then(|r| path.strip_prefix(r).ok())
                        .unwrap_or(path)
                        .to_string_lossy()
                        .into_owned();
                    (name, rel)
                }
                SearchItem::CreateFile(path) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    (
                        tf("＋ Create {name}", &[("name", &name.to_string())]),
                        "new file".to_string(),
                    )
                }
                SearchItem::RunCommand(cmd) => {
                    (self.palette_command_label(*cmd), "command".to_string())
                }
            };
            let item_clone = item.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("pal-{i}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .bg(if selected {
                        cx.theme().accent
                    } else {
                        cx.theme().background.opacity(0.0)
                    })
                    .hover(|s| s.bg(cx.theme().accent.opacity(0.6)))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, window, cx| {
                            this.activate_search_item(item_clone.clone(), window, cx);
                        }),
                    )
                    .child(div().flex_none().text_color(muted).child(icon))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .child(label),
                    )
                    .child(div().flex_none().text_xs().text_color(muted).child(sub)),
            );
        }

        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(80.0))
            .bg(rgba(0x0000_0066))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, _w, cx| this.close_search_palette(cx)),
            )
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" => this.close_search_palette(cx),
                    "down" => this.move_search_selection(1, cx),
                    "up" => this.move_search_selection(-1, cx),
                    "enter" => this.confirm_search(window, cx),
                    _ => {}
                }
            }))
            .child(
                div()
                    .w(px(620.0))
                    .flex()
                    .flex_col()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .child(Input::new(&self.search_input)),
                    )
                    .child(
                        div()
                            .id("palette-list")
                            .overflow_y_scroll()
                            .max_h(px(380.0))
                            .p_1()
                            .child(list),
                    ),
            )
            .into_any_element()
    }

    /// The Ctrl+Shift+F find-in-project panel: a query box + content matches
    /// (file:line + the matched line); selecting opens the file at that line.
    fn render_find_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let active_root = self.workspace.active().map(|p| p.root_path.clone());
        let muted = cx.theme().muted_foreground;
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().w_full().gap(px(2.0));
        if self.find_results.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(muted)
                    .child(t("Type to search file contents across the project.")),
            );
        }
        for (i, hit) in self.find_results.iter().enumerate() {
            let selected = i == self.find_selected;
            let rel = active_root
                .as_ref()
                .and_then(|r| hit.path.strip_prefix(r).ok())
                .unwrap_or(&hit.path)
                .to_string_lossy()
                .into_owned();
            let loc = format!("{}:{}", rel, hit.line + 1);
            let text = hit.text.clone();
            let hit_clone = hit.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("find-{i}")))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px_3()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .bg(if selected {
                        cx.theme().accent
                    } else {
                        cx.theme().background.opacity(0.0)
                    })
                    .hover(|s| s.bg(cx.theme().accent.opacity(0.6)))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, window, cx| {
                            this.activate_find_hit(hit_clone.clone(), window, cx)
                        }),
                    )
                    .child(div().text_xs().text_color(muted).child(loc))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_family(mono.clone())
                            .text_sm()
                            .child(text),
                    ),
            );
        }

        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(80.0))
            .bg(rgba(0x0000_0066))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, _w, cx| this.close_find_panel(cx)),
            )
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" => this.close_find_panel(cx),
                    "down" => this.move_find_selection(1, cx),
                    "up" => this.move_find_selection(-1, cx),
                    "enter" => this.confirm_find(window, cx),
                    _ => {}
                }
            }))
            .child(
                div()
                    .w(px(680.0))
                    .flex()
                    .flex_col()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(cx.theme().border)
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .flex_none()
                                    .child(t("Find in project")),
                            )
                            .child(div().flex_1().child(Input::new(&self.find_input))),
                    )
                    .child(
                        div()
                            .id("find-list")
                            .overflow_y_scroll()
                            .max_h(px(440.0))
                            .p_1()
                            .child(list),
                    ),
            )
            .into_any_element()
    }

    /// A minimal draggable title bar (app name + window min/max/close controls)
    /// for the first-run screens, which otherwise have no bar to move the window.
    fn render_minimal_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        TitleBar::new()
            .on_close_window(cx.listener(|this, _ev, _window, cx| {
                // Nothing is running yet — quit directly (no confirm needed).
                this.confirm_quit = true;
                cx.quit();
            }))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .flex()
                    .items_center()
                    .child(div().font_semibold().child(t("muxel"))),
            )
    }

    fn render_titlebar(&self, workspace_name: String, cx: &mut Context<Self>) -> impl IntoElement {
        // The TitleBar registers the whole bar as a window-drag region (mouse-down
        // then start_window_move on the next move). A button click with the
        // slightest movement would start a window move and swallow the click — so
        // wrap each button to stop mouse-down from reaching the bar's drag handler.
        fn nodrag(el: impl IntoElement) -> Div {
            div()
                .flex()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(el)
        }
        // Intercept the title-bar X (which otherwise calls remove_window directly,
        // bypassing on_window_should_close) so quitting asks for confirmation.
        TitleBar::new()
            .on_close_window(cx.listener(|this, _ev, _window, cx| {
                this.show_quit_confirm = true;
                cx.notify();
            }))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(nodrag(
                        Button::new("toggle-sidebar")
                            .ghost()
                            .icon(IconName::PanelLeft)
                            .tooltip(t("Toggle sidebar"))
                            .on_click(
                                cx.listener(|this, _ev, _window, cx| this.toggle_sidebar(cx)),
                            ),
                    ))
                    .child(div().font_semibold().child(t("muxel")))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(workspace_name),
                    )
                    .child(div().flex_1())
                    .child(nodrag(
                        Button::new("global-search")
                            .ghost()
                            .small()
                            .icon(IconName::Search)
                            .label(t("Search…"))
                            .tooltip(t("Search files and terminals (Ctrl+P)"))
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.open_search_palette(window, cx)
                            })),
                    ))
                    .child(div().flex_1())
                    .child(nodrag(
                        Button::new("update")
                            .ghost()
                            .icon(IconName::ArrowUp)
                            .selected(self.update_pending())
                            .tooltip(if matches!(self.update_state, UpdateState::Checking) {
                                t("Checking for updates…")
                            } else if self.update_pending() {
                                t("Update available")
                            } else {
                                t("Check for updates")
                            })
                            .on_click(
                                cx.listener(|this, _ev, _window, cx| this.open_update_modal(cx)),
                            ),
                    ))
                    .child(nodrag(
                        Button::new("workspaces")
                            .ghost()
                            .icon(IconName::CircleUser)
                            .tooltip(t("Switch workspace"))
                            .on_click(cx.listener(|this, _ev, _window, cx| {
                                this.open_workspace_selector(cx)
                            })),
                    ))
                    .child(nodrag(
                        Button::new("dashboard")
                            .ghost()
                            .icon(IconName::LayoutDashboard)
                            .selected(self.show_dashboard)
                            .tooltip(t("Dashboard"))
                            .on_click(
                                cx.listener(|this, _ev, _window, cx| this.toggle_dashboard(cx)),
                            ),
                    ))
                    .child(nodrag(
                        Button::new("settings")
                            .ghost()
                            .icon(IconName::Settings)
                            .selected(self.show_settings)
                            .tooltip(t("Settings"))
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.toggle_settings(window, cx)
                            })),
                    ))
                    .child(nodrag(
                        Button::new("notifications")
                            .ghost()
                            .icon(IconName::Bell)
                            .selected(self.notifications_enabled)
                            .tooltip(t("Notifications"))
                            .on_click(
                                cx.listener(|this, _ev, _window, cx| this.toggle_notifications(cx)),
                            ),
                    ))
                    .child(nodrag(
                        Button::new("donate")
                            .ghost()
                            .icon(IconName::Heart)
                            .tooltip(t("Support muxel"))
                            .on_click(cx.listener(|_t, _ev, _window, cx| {
                                cx.open_url("https://donate.stripe.com/bJeaEX2OVaE68Fae7X8k80X")
                            })),
                    )),
            )
    }

    // ===== Settings page =====

    fn settings_label(&self, text: &str, cx: &App) -> Div {
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text.to_string())
    }

    fn set_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.settings_ui.section = section;
        cx.notify();
    }

    /// Open Settings → Runners with runner `idx` selected for editing.
    fn open_runner_settings(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.runners_menu = None;
        if !self.show_settings {
            self.toggle_settings(window, cx);
        }
        self.set_section(SettingsSection::Runners, cx);
        self.open_runner_editor(idx, window, cx);
    }

    /// Open Settings → Loops with loop `idx` selected for editing.
    fn open_loop_settings(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.loops_menu = None;
        if !self.show_settings {
            self.toggle_settings(window, cx);
        }
        self.set_section(SettingsSection::Loops, cx);
        self.open_loop_editor(idx, window, cx);
    }

    fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_settings = !self.show_settings;
        if self.show_settings {
            // Size the modal for the current UI scale (font size × zoom) so the
            // content isn't cramped when scaled up. The corner-drag can override.
            let scale = (self.settings.ui_font_size * self.settings.zoom / 16.0).clamp(1.0, 2.5);
            self.settings_size = size(px(780.0 * scale), px(620.0 * scale));
            self.settings_offset = point(px(0.0), px(0.0));
            self.settings_snapshot = Some(SettingsSnapshot {
                settings: self.settings.clone(),
                presets: self.presets.clone(),
                theme: self.theme.clone(),
                theme_mode: self.theme_mode.clone(),
                use_tmux: self.use_tmux,
                use_worktree: self.use_worktree,
                notifications: self.notifications_enabled,
            });
            self.load_appearance_inputs(window, cx);
            self.load_keybinding_inputs(window, cx);
        }
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.show_settings = false;
        self.settings_snapshot = None;
        cx.notify();
    }

    /// Keep all edits (they applied live + persisted) and dismiss.
    fn save_settings(&mut self, cx: &mut Context<Self>) {
        self.persist_settings();
        self.close_settings(cx);
    }

    /// Revert settings to the snapshot taken when the modal opened, then dismiss.
    fn cancel_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(snap) = self.settings_snapshot.take() {
            self.settings = snap.settings;
            self.presets = snap.presets;
            self.theme = snap.theme;
            self.theme_mode = snap.theme_mode;
            self.use_tmux = snap.use_tmux;
            self.use_worktree = snap.use_worktree;
            self.notifications_enabled = snap.notifications;
            // Re-apply the live visual state that editing may have changed.
            let zoom = self.settings.zoom;
            let ui_font = self.settings.ui_font_size;
            theme::apply_initial_theme(&self.theme.clone(), cx);
            theme::set_ui_font_size(ui_font, cx);
            theme::set_ui_scale(zoom, cx);
            self.refresh_terminal_config(cx);
            self.refresh_terminal_palettes(cx);
            self.apply_keybindings(cx);
            self.persist_settings();
        }
        self.close_settings(cx);
    }

    fn load_appearance_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fam = self.settings.font_family.clone();
        self.settings_ui
            .font_family
            .update(cx, |s, cx| s.set_value(fam, window, cx));
        let ed_fam = self.settings.editor_font_family.clone();
        self.settings_ui
            .editor_font_family
            .update(cx, |s, cx| s.set_value(ed_fam, window, cx));
    }

    /// Re-derive terminal font config from settings and push it to all panes.
    /// Save the current main-window geometry (called debounced from the
    /// window-bounds observer).
    fn save_window_geom(&self, window: &Window) {
        let (bounds, maximized) = match window.inner_window_bounds() {
            WindowBounds::Windowed(b) => (b, false),
            WindowBounds::Maximized(b) => (b, true),
            WindowBounds::Fullscreen(b) => (b, true),
        };
        let geom = muxel_core::WindowGeom {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
            maximized,
        };
        let _ = muxel_store::save_window_geom(&geom);
    }

    fn refresh_terminal_config(&mut self, cx: &mut Context<Self>) {
        let font_family: SharedString = self.settings.font_family.clone().into();
        let font_size = self.settings.font_size * self.settings.zoom;
        let mouse_mode = TerminalMouseMode::from_setting(&self.settings.terminal_mouse);
        for view in self.terminals.values() {
            view.update(cx, |view, _cx| {
                view.set_config(font_family.clone(), font_size);
                view.set_mouse_mode(mouse_mode);
            });
        }
    }

    fn apply_font_family(&mut self, cx: &mut Context<Self>) {
        self.settings.font_family = self.settings_ui.font_family.read(cx).value().to_string();
        self.refresh_terminal_config(cx);
        self.persist_settings();
        cx.notify();
    }

    // ===== Editor settings handlers =====

    fn apply_editor_font_family(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.editor_font_family = self
            .settings_ui
            .editor_font_family
            .read(cx)
            .value()
            .to_string();
        self.persist_settings();
        self.apply_editor_config(window, cx);
        cx.notify();
    }

    fn adjust_editor_font(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.editor_font_size = (self.settings.editor_font_size + delta).clamp(6.0, 48.0);
        self.persist_settings();
        self.apply_editor_config(window, cx);
        cx.notify();
    }

    fn adjust_editor_tab(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        let next = (self.settings.editor_tab_size as i32 + delta).clamp(1, 16) as u32;
        self.settings.editor_tab_size = next;
        self.persist_settings();
        self.apply_editor_config(window, cx);
        cx.notify();
    }

    fn render_settings_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_3()
            .max_w(px(520.0))
            .child(self.settings_label(
                &t("Code & diff font size — code editor and git-diff panes"),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Button::new("ed-font-dec").ghost().label("−").on_click(
                        cx.listener(|this, _e, w, cx| this.adjust_editor_font(-1.0, w, cx)),
                    ))
                    .child(
                        div()
                            .w(rems(4.0))
                            .text_center()
                            .child(format!("{}", self.settings.editor_font_size.round() as i32)),
                    )
                    .child(Button::new("ed-font-inc").ghost().label("+").on_click(
                        cx.listener(|this, _e, w, cx| this.adjust_editor_font(1.0, w, cx)),
                    )),
            )
            .child(self.settings_label(&t("Editor font family (blank = theme monospace)"), cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .child(Input::new(&self.settings_ui.editor_font_family)),
                    )
                    .child(
                        Button::new("apply-ed-font")
                            .primary()
                            .label(t("Apply"))
                            .on_click(
                                cx.listener(|this, _e, w, cx| this.apply_editor_font_family(w, cx)),
                            ),
                    ),
            )
            .child(
                self.settings_label(&t("Tab width (spaces) — applies to newly opened files"), cx),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("ed-tab-dec").ghost().label("−").on_click(
                            cx.listener(|this, _e, w, cx| this.adjust_editor_tab(-1, w, cx)),
                        ),
                    )
                    .child(
                        div()
                            .w(rems(4.0))
                            .text_center()
                            .child(format!("{}", self.settings.editor_tab_size)),
                    )
                    .child(
                        Button::new("ed-tab-inc").ghost().label("+").on_click(
                            cx.listener(|this, _e, w, cx| this.adjust_editor_tab(1, w, cx)),
                        ),
                    ),
            )
            .child(
                self.check_row(
                    Checkbox::new("ed-wrap")
                        .checked(self.settings.editor_soft_wrap)
                        .on_click(cx.listener(|this, c: &bool, w, cx| {
                            this.settings.editor_soft_wrap = *c;
                            this.persist_settings();
                            this.apply_editor_config(w, cx);
                            cx.notify();
                        })),
                    &t("Soft wrap long lines"),
                ),
            )
            .child(
                self.check_row(
                    Checkbox::new("ed-lines")
                        .checked(self.settings.editor_line_numbers)
                        .on_click(cx.listener(|this, c: &bool, w, cx| {
                            this.settings.editor_line_numbers = *c;
                            this.persist_settings();
                            this.apply_editor_config(w, cx);
                            cx.notify();
                        })),
                    &t("Show line numbers"),
                ),
            )
            .child(
                self.check_row(
                    Checkbox::new("ed-guides")
                        .checked(self.settings.editor_indent_guides)
                        .on_click(cx.listener(|this, c: &bool, w, cx| {
                            this.settings.editor_indent_guides = *c;
                            this.persist_settings();
                            this.apply_editor_config(w, cx);
                            cx.notify();
                        })),
                    &t("Show indent guides"),
                ),
            )
            .into_any_element()
    }

    /// Adjust the terminal font size (independent of the UI zoom) and save.
    fn adjust_terminal_font(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.font_size = (self.settings.font_size + delta).clamp(6.0, 72.0);
        self.refresh_terminal_config(cx);
        self.persist_settings();
        cx.notify();
    }

    fn set_close_on_exit(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.close_on_exit = on;
        self.persist_settings();
        cx.notify();
    }

    fn set_pane_border(&mut self, value: &str, cx: &mut Context<Self>) {
        self.settings.pane_border = value.to_string();
        self.persist_settings();
        cx.notify();
    }

    fn pane_border_btn(&self, value: &'static str, label: &str, cx: &mut Context<Self>) -> Button {
        Button::new(SharedString::from(format!("pb-{value}")))
            .ghost()
            .small()
            .selected(self.settings.pane_border == value)
            .label(label.to_string())
            .on_click(cx.listener(move |this, _e, _w, cx| this.set_pane_border(value, cx)))
    }

    fn set_terminal_mouse(&mut self, value: &str, cx: &mut Context<Self>) {
        self.settings.terminal_mouse = value.to_string();
        // Push to live panes so the new behavior takes effect immediately.
        self.refresh_terminal_config(cx);
        self.persist_settings();
        cx.notify();
    }

    fn terminal_mouse_btn(
        &self,
        value: &'static str,
        label: &str,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(SharedString::from(format!("tm-{value}")))
            .ghost()
            .small()
            .selected(self.settings.terminal_mouse == value)
            .label(label.to_string())
            .on_click(cx.listener(move |this, _e, _w, cx| this.set_terminal_mouse(value, cx)))
    }

    fn set_global_default_preset(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.settings.default_preset = id.to_string();
        self.persist_settings();
        cx.notify();
    }

    fn set_editor_injection(&mut self, mode: InjectionMode, cx: &mut Context<Self>) {
        self.settings_ui.p_injection = mode;
        cx.notify();
    }

    fn open_preset_editor(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.presets.get(idx).cloned() else {
            return;
        };
        self.settings_ui.selected_preset = Some(idx);
        self.settings_ui.p_injection = p.injection.clone();
        let inj_flag = match &p.injection {
            InjectionMode::CliFlag { flag } => flag.clone(),
            _ => String::new(),
        };
        let ui = &self.settings_ui;
        ui.p_name
            .update(cx, |s, cx| s.set_value(p.name.clone(), window, cx));
        ui.p_program.update(cx, |s, cx| {
            s.set_value(p.program.clone().unwrap_or_default(), window, cx)
        });
        ui.p_model.update(cx, |s, cx| {
            s.set_value(p.model.clone().unwrap_or_default(), window, cx)
        });
        ui.p_model_flag.update(cx, |s, cx| {
            s.set_value(p.model_flag.clone().unwrap_or_default(), window, cx)
        });
        ui.p_effort.update(cx, |s, cx| {
            s.set_value(p.effort.clone().unwrap_or_default(), window, cx)
        });
        ui.p_effort_flag.update(cx, |s, cx| {
            s.set_value(p.effort_flag.clone().unwrap_or_default(), window, cx)
        });
        ui.p_args
            .update(cx, |s, cx| s.set_value(p.args.join(" "), window, cx));
        ui.p_prompt.update(cx, |s, cx| {
            s.set_value(p.system_prompt.clone().unwrap_or_default(), window, cx)
        });
        ui.p_inj_flag
            .update(cx, |s, cx| s.set_value(inj_flag, window, cx));
        ui.p_env.update(cx, |s, cx| {
            s.set_value(settings_view::format_env(&p.env), window, cx)
        });
        ui.p_working_markers.update(cx, |s, cx| {
            s.set_value(p.working_markers.join(", "), window, cx)
        });
        ui.p_blocked_markers.update(cx, |s, cx| {
            s.set_value(p.blocked_markers.join(", "), window, cx)
        });
        ui.p_startup_delay.update(cx, |s, cx| {
            let v = if p.startup_delay_ms > 0 {
                p.startup_delay_ms.to_string()
            } else {
                String::new()
            };
            s.set_value(v, window, cx)
        });
        // Show the program's built-in markers as placeholders (the effective
        // value when the field is left blank).
        let (def_w, def_b) = default_markers(p.program.as_deref());
        let ph = |xs: Vec<String>| {
            if xs.is_empty() {
                t("comma-separated; blank = heuristic").to_string()
            } else {
                tf("default: {value}", &[("value", &xs.join(", "))])
            }
        };
        ui.p_working_markers
            .update(cx, |s, cx| s.set_placeholder(ph(def_w), window, cx));
        ui.p_blocked_markers
            .update(cx, |s, cx| s.set_placeholder(ph(def_b), window, cx));
        cx.notify();
    }

    fn save_preset(&mut self, cx: &mut Context<Self>) {
        let Some(idx) = self.settings_ui.selected_preset else {
            return;
        };
        if idx >= self.presets.len() {
            return;
        }
        let name = self.settings_ui.p_name.read(cx).value().trim().to_string();
        let program = self
            .settings_ui
            .p_program
            .read(cx)
            .value()
            .trim()
            .to_string();
        let model = self.settings_ui.p_model.read(cx).value().trim().to_string();
        let model_flag = self
            .settings_ui
            .p_model_flag
            .read(cx)
            .value()
            .trim()
            .to_string();
        let effort = self
            .settings_ui
            .p_effort
            .read(cx)
            .value()
            .trim()
            .to_string();
        let effort_flag = self
            .settings_ui
            .p_effort_flag
            .read(cx)
            .value()
            .trim()
            .to_string();
        let args = settings_view::parse_args(&self.settings_ui.p_args.read(cx).value());
        let prompt = self.settings_ui.p_prompt.read(cx).value().to_string();
        let env = settings_view::parse_env(&self.settings_ui.p_env.read(cx).value());
        let working_markers =
            settings_view::parse_markers(&self.settings_ui.p_working_markers.read(cx).value());
        let blocked_markers =
            settings_view::parse_markers(&self.settings_ui.p_blocked_markers.read(cx).value());
        let startup_delay_ms = self
            .settings_ui
            .p_startup_delay
            .read(cx)
            .value()
            .trim()
            .parse::<u32>()
            .unwrap_or(0);
        let inj_flag = self
            .settings_ui
            .p_inj_flag
            .read(cx)
            .value()
            .trim()
            .to_string();
        let injection = match self.settings_ui.p_injection {
            InjectionMode::CliFlag { .. } => InjectionMode::CliFlag {
                flag: if inj_flag.is_empty() {
                    "--append-system-prompt".to_string()
                } else {
                    inj_flag
                },
            },
            InjectionMode::TypeIn => InjectionMode::TypeIn,
            InjectionMode::None => InjectionMode::None,
        };
        let p = &mut self.presets[idx];
        if !name.is_empty() {
            p.name = name;
        }
        p.program = (!program.is_empty()).then_some(program);
        p.model = (!model.is_empty()).then_some(model);
        p.model_flag = (!model_flag.is_empty()).then_some(model_flag);
        p.effort = (!effort.is_empty()).then_some(effort);
        p.effort_flag = (!effort_flag.is_empty()).then_some(effort_flag);
        p.args = args;
        p.system_prompt = (!prompt.trim().is_empty()).then_some(prompt);
        p.injection = injection;
        p.env = env;
        p.working_markers = working_markers;
        p.blocked_markers = blocked_markers;
        p.startup_delay_ms = startup_delay_ms;
        self.persist_settings();
        cx.notify();
    }

    fn add_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut p = AgentPreset::shell();
        p.name = t("New preset").to_string();
        self.presets.push(p);
        let idx = self.presets.len() - 1;
        self.persist_settings();
        self.open_preset_editor(idx, window, cx);
    }

    fn duplicate_preset(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut p) = self.presets.get(idx).cloned() else {
            return;
        };
        p.id = Uuid::new_v4();
        p.name = format!("{} copy", p.name);
        self.presets.push(p);
        let new_idx = self.presets.len() - 1;
        self.persist_settings();
        self.open_preset_editor(new_idx, window, cx);
    }

    fn delete_preset(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.presets.len() || self.presets.len() <= 1 {
            return;
        }
        self.presets.remove(idx);
        if self.current_preset >= self.presets.len() {
            self.current_preset = self.presets.len() - 1;
        }
        self.settings_ui.selected_preset = None;
        self.persist_settings();
        cx.notify();
    }

    fn open_runner_editor(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.runners.get(idx).cloned() else {
            return;
        };
        self.settings_ui.selected_runner = Some(idx);
        self.settings_ui.r_preset_id = r.preset_id;
        self.settings_ui.r_presses = r.auto_mode_presses;
        self.settings_ui
            .r_name
            .update(cx, |s, cx| s.set_value(r.name.clone(), window, cx));
        self.settings_ui
            .r_prompt
            .update(cx, |s, cx| s.set_value(r.prompt.clone(), window, cx));
        cx.notify();
    }

    fn save_runner(&mut self, cx: &mut Context<Self>) {
        let Some(idx) = self.settings_ui.selected_runner else {
            return;
        };
        let name = self.settings_ui.r_name.read(cx).value().trim().to_string();
        let prompt = self.settings_ui.r_prompt.read(cx).value().to_string();
        let preset_id = self.settings_ui.r_preset_id;
        let presses = self.settings_ui.r_presses;
        if let Some(r) = self.runners.get_mut(idx) {
            if !name.is_empty() {
                r.name = name;
            }
            r.prompt = prompt;
            r.preset_id = preset_id;
            r.auto_mode_presses = presses;
        }
        self.persist_settings();
        cx.notify();
    }

    fn add_runner(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.runners.push(Runner {
            id: Uuid::new_v4(),
            name: t("New runner").to_string(),
            preset_id: None,
            auto_mode_presses: 3,
            prompt: "{{input}}".to_string(),
        });
        let idx = self.runners.len() - 1;
        self.persist_settings();
        self.open_runner_editor(idx, window, cx);
    }

    fn delete_runner(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.runners.len() {
            return;
        }
        self.runners.remove(idx);
        self.settings_ui.selected_runner = None;
        self.persist_settings();
        cx.notify();
    }

    // --- Loops (scheduled task launchers) ---

    fn open_loop_editor(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(l) = self.loops.get(idx).cloned() else {
            return;
        };
        self.settings_ui.selected_loop = Some(idx);
        self.settings_ui.l_preset_id = l.preset_id;
        self.settings_ui.l_project_id = Some(l.project_id);
        self.settings_ui.l_presses = l.auto_mode_presses;
        self.settings_ui.l_exit = l.post_run == PostRunAction::Exit;
        self.settings_ui.l_enabled = l.enabled;
        let (kind, interval, hour, minute) = match l.schedule {
            LoopSchedule::EveryMinutes { minutes } => {
                (0u8, minutes.to_string(), String::new(), String::new())
            }
            LoopSchedule::EveryHours { hours } => {
                (1u8, hours.to_string(), String::new(), String::new())
            }
            LoopSchedule::DailyAt { hour, minute } => {
                (2u8, String::new(), hour.to_string(), format!("{minute:02}"))
            }
        };
        self.settings_ui.l_sched_kind = kind;
        let ui = &self.settings_ui;
        ui.l_name
            .update(cx, |s, cx| s.set_value(l.name.clone(), window, cx));
        ui.l_prompt
            .update(cx, |s, cx| s.set_value(l.prompt.clone(), window, cx));
        ui.l_interval
            .update(cx, |s, cx| s.set_value(interval, window, cx));
        ui.l_hour.update(cx, |s, cx| s.set_value(hour, window, cx));
        ui.l_minute
            .update(cx, |s, cx| s.set_value(minute, window, cx));
        cx.notify();
    }

    /// Build a `LoopSchedule` from the editor's kind + numeric inputs (clamped).
    fn read_loop_schedule(&self, cx: &Context<Self>) -> LoopSchedule {
        let num = |inp: &Entity<InputState>| inp.read(cx).value().trim().parse::<u32>().ok();
        match self.settings_ui.l_sched_kind {
            0 => LoopSchedule::EveryMinutes {
                minutes: num(&self.settings_ui.l_interval).unwrap_or(1).max(1),
            },
            2 => LoopSchedule::DailyAt {
                hour: num(&self.settings_ui.l_hour).unwrap_or(9).min(23) as u8,
                minute: num(&self.settings_ui.l_minute).unwrap_or(0).min(59) as u8,
            },
            _ => LoopSchedule::EveryHours {
                hours: num(&self.settings_ui.l_interval).unwrap_or(1).max(1),
            },
        }
    }

    fn save_loop(&mut self, cx: &mut Context<Self>) {
        let Some(idx) = self.settings_ui.selected_loop else {
            return;
        };
        let name = self.settings_ui.l_name.read(cx).value().trim().to_string();
        let prompt = self.settings_ui.l_prompt.read(cx).value().to_string();
        let preset_id = self.settings_ui.l_preset_id;
        let presses = self.settings_ui.l_presses;
        let exit = self.settings_ui.l_exit;
        let enabled = self.settings_ui.l_enabled;
        let schedule = self.read_loop_schedule(cx);
        let project_id = self
            .settings_ui
            .l_project_id
            .or(self.workspace.active_project);
        if let Some(l) = self.loops.get_mut(idx) {
            if !name.is_empty() {
                l.name = name;
            }
            l.prompt = prompt;
            l.preset_id = preset_id;
            if let Some(pid) = project_id {
                l.project_id = pid;
            }
            l.auto_mode_presses = presses;
            l.post_run = if exit {
                PostRunAction::Exit
            } else {
                PostRunAction::Leave
            };
            l.enabled = enabled;
            l.schedule = schedule;
        }
        self.persist_settings();
        cx.notify();
    }

    fn add_loop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.workspace.active_project else {
            self.add_event(
                NotifKind::Error,
                t("Can't add a loop").to_string(),
                t("Open a project first — a loop runs in a specific project.").to_string(),
            );
            cx.notify();
            return;
        };
        let mut lp = Loop::new(t("New loop"), pid);
        // Arm so the first interval fire is after one interval.
        lp.last_run = Some(unix_now());
        self.loops.push(lp);
        let idx = self.loops.len() - 1;
        self.persist_settings();
        self.open_loop_editor(idx, window, cx);
    }

    fn delete_loop(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.loops.len() {
            return;
        }
        self.loops.remove(idx);
        self.settings_ui.selected_loop = None;
        self.persist_settings();
        cx.notify();
    }

    fn set_loop_preset(&mut self, preset_id: Option<Uuid>, cx: &mut Context<Self>) {
        self.settings_ui.l_preset_id = preset_id;
        cx.notify();
    }

    fn set_loop_project(&mut self, project_id: Uuid, cx: &mut Context<Self>) {
        self.settings_ui.l_project_id = Some(project_id);
        cx.notify();
    }

    fn set_loop_sched_kind(&mut self, kind: u8, cx: &mut Context<Self>) {
        self.settings_ui.l_sched_kind = kind;
        cx.notify();
    }

    fn adjust_loop_presses(&mut self, delta: i8, cx: &mut Context<Self>) {
        let v = self.settings_ui.l_presses as i8 + delta;
        self.settings_ui.l_presses = v.clamp(0, 9) as u8;
        cx.notify();
    }

    // --- SSH remote hosts ---

    fn open_remote_editor(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(h) = self.remotes.get(idx).cloned() else {
            return;
        };
        self.settings_ui.selected_remote = Some(idx);
        self.settings_ui.s_auth = h.auth;
        self.settings_ui.s_test = RemoteTestState::Idle;
        self.settings_ui.s_has_password = crate::secrets::has_remote_password(h.id);
        self.settings_ui.s_forward_agent = h.forward_agent;
        self.settings_ui.s_use_tmux = h.default_use_tmux;
        let set =
            |inp: &Entity<InputState>, v: String, cx: &mut Context<Self>, window: &mut Window| {
                inp.update(cx, |s, cx| s.set_value(v, window, cx));
            };
        set(&self.settings_ui.s_name, h.name.clone(), cx, window);
        set(&self.settings_ui.s_host, h.hostname.clone(), cx, window);
        set(
            &self.settings_ui.s_port,
            h.port.map(|p| p.to_string()).unwrap_or_default(),
            cx,
            window,
        );
        set(&self.settings_ui.s_user, h.user.clone(), cx, window);
        set(
            &self.settings_ui.s_identity,
            h.identity_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            cx,
            window,
        );
        // Never preload the password — it stays in the keychain.
        set(&self.settings_ui.s_password, String::new(), cx, window);
        set(
            &self.settings_ui.s_jump,
            h.jump_host.clone().unwrap_or_default(),
            cx,
            window,
        );
        set(
            &self.settings_ui.s_keepalive,
            h.keepalive_secs.map(|k| k.to_string()).unwrap_or_default(),
            cx,
            window,
        );
        set(
            &self.settings_ui.s_strict,
            h.strict_host_key.clone(),
            cx,
            window,
        );
        set(
            &self.settings_ui.s_extra,
            h.extra_options.join("\n"),
            cx,
            window,
        );
        cx.notify();
    }

    fn set_remote_auth(&mut self, auth: SshAuth, cx: &mut Context<Self>) {
        self.settings_ui.s_auth = auth;
        cx.notify();
    }

    /// Pick an SSH identity file via the OS file dialog, into the host editor.
    fn browse_identity_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("Choose key")),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = receiver.await
                && let Some(path) = paths.pop()
            {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.settings_ui.s_identity.update(cx, |s, cx| {
                        s.set_value(path.display().to_string(), window, cx)
                    });
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn save_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.settings_ui.selected_remote else {
            return;
        };
        let ui = &self.settings_ui;
        let name = ui.s_name.read(cx).value().trim().to_string();
        let hostname = ui.s_host.read(cx).value().trim().to_string();
        let port = ui.s_port.read(cx).value().trim().parse::<u16>().ok();
        let user = ui.s_user.read(cx).value().trim().to_string();
        let identity = {
            let v = ui.s_identity.read(cx).value().trim().to_string();
            (!v.is_empty()).then(|| PathBuf::from(v))
        };
        let password = ui.s_password.read(cx).value().to_string();
        let jump = {
            let v = ui.s_jump.read(cx).value().trim().to_string();
            (!v.is_empty()).then_some(v)
        };
        let keepalive = ui.s_keepalive.read(cx).value().trim().parse::<u32>().ok();
        let strict = ui.s_strict.read(cx).value().trim().to_string();
        let extra: Vec<String> = ui
            .s_extra
            .read(cx)
            .value()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let auth = ui.s_auth;
        let forward_agent = ui.s_forward_agent;
        let use_tmux = ui.s_use_tmux;
        let host_id = self.remotes.get(idx).map(|h| h.id);
        if let Some(h) = self.remotes.get_mut(idx) {
            if !name.is_empty() {
                h.name = name;
            }
            h.hostname = hostname;
            h.port = port;
            h.user = user;
            h.auth = auth;
            h.identity_file = identity;
            h.jump_host = jump;
            h.forward_agent = forward_agent;
            h.strict_host_key = strict;
            h.keepalive_secs = keepalive;
            h.extra_options = extra;
            h.default_use_tmux = use_tmux;
        }
        // Store/replace the password in the OS keychain when one was entered.
        if let Some(id) = host_id
            && !password.is_empty()
        {
            // The keychain copy is now authoritative; drop any session password.
            self.session_passwords.remove(&id);
            match crate::secrets::set_remote_password(id, &password) {
                Ok(()) => self.settings_ui.s_has_password = true,
                Err(e) => self.add_event(NotifKind::Error, t("Keychain error"), format!("{e}")),
            }
            self.settings_ui
                .s_password
                .update(cx, |s, cx| s.set_value(String::new(), window, cx));
        }
        self.persist_settings();
        cx.notify();
    }

    fn add_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remotes.push(RemoteHost::new(t("New host"), ""));
        let idx = self.remotes.len() - 1;
        self.persist_settings();
        self.open_remote_editor(idx, window, cx);
    }

    fn delete_remote(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.remotes.len() {
            return;
        }
        let id = self.remotes[idx].id;
        let _ = crate::secrets::delete_remote_password(id);
        self.session_passwords.remove(&id);
        self.remotes.remove(idx);
        self.settings_ui.selected_remote = None;
        self.persist_settings();
        cx.notify();
    }

    /// Verify a host's SSH config by opening a quick connection (background +
    /// toast). Establishing the ControlMaster also makes the first pane instant.
    fn test_remote_connection(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.remotes.get(idx).cloned() else {
            return;
        };
        // Password auth: always prompt for a password to test with (forgotten
        // afterward), so a connection can be verified before saving anything.
        if host.auth == SshAuth::Password {
            self.prompt_password(host.id, PasswordAction::Verify(idx), window, cx);
        } else {
            self.run_ssh_check(idx, None, window, cx);
        }
    }

    /// Run the connection test for host `idx` with an optional password (used once
    /// for the test — not stored).
    fn run_ssh_check(
        &mut self,
        idx: usize,
        password: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.remotes.get(idx).cloned() else {
            return;
        };
        // Inline result in the editor (not a toast). A fresh, auth-forcing check
        // (`ssh_verify`) so a warm ControlMaster / a working key can't make a bad
        // password report success.
        self.settings_ui.s_test = RemoteTestState::Testing;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { integrations::ssh_verify(&host, password.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.settings_ui.s_test = match res {
                    Ok(()) => RemoteTestState::Ok(t("Connected").into()),
                    Err(e) => RemoteTestState::Failed(format!("{e}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Set the editor's selected agent for the runner being edited.
    fn set_runner_preset(&mut self, preset_id: Option<Uuid>, cx: &mut Context<Self>) {
        self.settings_ui.r_preset_id = preset_id;
        cx.notify();
    }

    /// Adjust the editor's Shift+Tab count for the runner being edited.
    fn adjust_runner_presses(&mut self, delta: i8, cx: &mut Context<Self>) {
        let v = self.settings_ui.r_presses as i8 + delta;
        self.settings_ui.r_presses = v.clamp(0, 9) as u8;
        cx.notify();
    }

    fn open_project_editor(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.workspace.project(pid).map(|p| p.name.clone()) else {
            return;
        };
        self.settings_ui.selected_project = Some(pid);
        self.settings_ui
            .proj_name
            .update(cx, |s, cx| s.set_value(name, window, cx));
        cx.notify();
    }

    fn save_project(&mut self, cx: &mut Context<Self>) {
        let Some(pid) = self.settings_ui.selected_project else {
            return;
        };
        let name = self
            .settings_ui
            .proj_name
            .read(cx)
            .value()
            .trim()
            .to_string();
        if let Some(p) = self.workspace.project_mut(pid)
            && !name.is_empty()
        {
            p.name = name;
        }
        self.persist();
        cx.notify();
    }

    fn set_project_default_preset(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(pid) = self.settings_ui.selected_project else {
            return;
        };
        if let Some(p) = self.workspace.project_mut(pid) {
            p.default_preset = Some(id);
        }
        self.persist();
        cx.notify();
    }

    fn change_project_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.settings_ui.selected_project else {
            return;
        };
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t("Open")),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = receiver.await
                && let Some(dir) = paths.pop()
            {
                let _ = this.update_in(cx, |this, _window, cx| {
                    if let Some(p) = this.workspace.project_mut(pid) {
                        p.root_path = dir;
                    }
                    this.persist();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn delete_project(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        let iids = self
            .workspace
            .project(pid)
            .map(|p| p.instances())
            .unwrap_or_default();
        for iid in iids {
            self.close_instance(iid, cx);
        }
        self.workspace.projects.retain(|p| p.id != pid);
        if self.workspace.active_project == Some(pid) {
            self.workspace.active_project = self.workspace.projects.first().map(|p| p.id);
        }
        self.settings_ui.selected_project = None;
        self.persist();
        cx.notify();
    }

    fn focus_sibling(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.workspace.active_project else {
            return;
        };
        let instances = self
            .workspace
            .project(pid)
            .map(|p| p.instances())
            .unwrap_or_default();
        if instances.is_empty() {
            return;
        }
        let cur = self
            .active_instance
            .and_then(|a| instances.iter().position(|&i| i == a))
            .unwrap_or(0);
        let len = instances.len() as isize;
        let next = (((cur as isize + delta) % len + len) % len) as usize;
        self.focus_instance(instances[next], window, cx);
    }

    fn adjust_zoom(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.zoom = (self.settings.zoom + delta).clamp(0.5, 4.0);
        theme::set_ui_scale(self.settings.zoom, cx);
        self.refresh_terminal_config(cx);
        self.persist_settings();
        cx.notify();
    }

    /// Adjust the interface (non-terminal) font size and save. Does not touch
    /// the terminal, which has its own size.
    fn adjust_ui_font(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.ui_font_size = (self.settings.ui_font_size + delta).clamp(10.0, 28.0);
        theme::set_ui_font_size(self.settings.ui_font_size, cx);
        self.persist_settings();
        cx.notify();
    }

    fn load_keybinding_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let overrides: std::collections::HashMap<String, String> = self
            .settings
            .keybindings
            .iter()
            .map(|k| (k.action.clone(), k.keystroke.clone()))
            .collect();
        for (name, input) in &self.settings_ui.keybinds {
            let ks = overrides.get(name).cloned().unwrap_or_else(|| {
                settings_view::DEFAULT_KEYBINDINGS
                    .iter()
                    .find(|(n, _, _)| n == name)
                    .map(|(_, d, _)| d.to_string())
                    .unwrap_or_default()
            });
            input.update(cx, |s, cx| s.set_value(ks, window, cx));
        }
        let pass = self.settings.terminal_passthrough_keys.join(", ");
        self.settings_ui
            .passthrough_keys
            .update(cx, |s, cx| s.set_value(pass, window, cx));
    }

    fn apply_keybindings(&mut self, cx: &mut Context<Self>) {
        let mut cfgs = Vec::new();
        for (name, input) in &self.settings_ui.keybinds {
            let ks = input.read(cx).value().trim().to_string();
            if !ks.is_empty() {
                cfgs.push(muxel_core::KeyBindingCfg {
                    action: name.clone(),
                    keystroke: ks,
                });
            }
        }
        self.settings.keybindings = cfgs;
        // Terminal pass-through chords (comma/space/newline separated).
        let pass = self.settings_ui.passthrough_keys.read(cx).value().clone();
        self.settings.terminal_passthrough_keys = pass
            .split([',', ' ', '\n'])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        let snapshot = self.settings.clone();
        install_keybindings(&snapshot, cx);
        self.persist_settings();
        cx.notify();
    }

    /// Agent picker shown when a split button is held: a dropdown anchored at the
    /// button, listing presets for the new pane.
    /// Whether a preset can actually launch: its program is installed (shells,
    /// which have no program, are always runnable).
    fn agent_runnable(&self, preset: &AgentPreset) -> bool {
        match &preset.program {
            None => true,
            Some(prog) => self.available_programs.contains(prog),
        }
    }

    fn render_place_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((_, placement, pos)) = self.place_menu else {
            return div().into_any_element();
        };
        let heading = match placement {
            PlacementMode::Tab => t("New tab agent"),
            PlacementMode::Split(_) => t("New pane agent"),
        };
        let mut list = v_flex().gap_px().w_full();
        for (idx, preset) in self.presets.iter().enumerate() {
            // Hide agents whose binary isn't installed (shell has no program).
            if !self.agent_runnable(preset) {
                continue;
            }
            let name = preset.name.clone();
            let program = preset.program.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("place-preset-{idx}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.pick_place_agent(idx, window, cx)
                    }))
                    .child(agent_icon(
                        program.as_deref(),
                        px(15.0),
                        cx.theme().foreground,
                    ))
                    .child(div().text_sm().child(name)),
            );
        }
        // Full-window catcher dismisses on an outside click; the dropdown itself
        // is deferred + anchored at the press position so it floats on top.
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.place_menu = None;
                    cx.notify();
                }),
            )
            .child(
                deferred(
                    anchored()
                        .position(pos)
                        .snap_to_window_with_margin(px(8.0))
                        .child(
                            div()
                                .occlude()
                                .w(px(220.0))
                                .flex()
                                .flex_col()
                                .gap_px()
                                .p_1()
                                .bg(cx.theme().popover)
                                .border_1()
                                .border_color(cx.theme().border)
                                .rounded(cx.theme().radius)
                                .shadow_lg()
                                .child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(heading),
                                )
                                .child(list),
                        ),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// Anchored dropdown for the toolbar "Run task" button: pick a runner.
    /// A small pencil "edit" button for the Run-task / Loops dropdown rows. The
    /// caller attaches the `.on_click`.
    fn menu_edit_button(&self, id: String, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id(SharedString::from(id))
            .flex_none()
            .p_1()
            .mr_1()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().accent))
            .child(
                svg()
                    .path("icons/pencil.svg")
                    .size(px(14.0))
                    .flex_none()
                    .text_color(cx.theme().muted_foreground),
            )
    }

    fn render_runners_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(pos) = self.runners_menu else {
            return div().into_any_element();
        };
        let mut list = v_flex().gap_px().w_full();
        if self.runners.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t("No runners — add one in Settings → Runners.")),
            );
        }
        for (idx, runner) in self.runners.iter().enumerate() {
            let name = runner.name.clone();
            let program = runner
                .preset_id
                .and_then(|id| self.presets.iter().find(|p| p.id == id))
                .and_then(|p| p.program.clone());
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .w_full()
                    .child(
                        div()
                            .id(SharedString::from(format!("runner-item-{idx}")))
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().accent))
                            .on_click(
                                cx.listener(move |this, _e, _w, cx| this.open_run_dialog(idx, cx)),
                            )
                            .child(agent_icon(
                                program.as_deref(),
                                px(15.0),
                                cx.theme().foreground,
                            ))
                            .child(div().text_sm().child(name)),
                    )
                    .child(
                        self.menu_edit_button(format!("runner-edit-{idx}"), cx)
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.open_runner_settings(idx, window, cx)
                            })),
                    ),
            );
        }
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.runners_menu = None;
                    cx.notify();
                }),
            )
            .child(
                deferred(
                    anchored()
                        .position(pos)
                        .snap_to_window_with_margin(px(8.0))
                        .child(
                            div()
                                .occlude()
                                .w(px(240.0))
                                .flex()
                                .flex_col()
                                .gap_px()
                                .p_1()
                                .bg(cx.theme().popover)
                                .border_1()
                                .border_color(cx.theme().border)
                                .rounded(cx.theme().radius)
                                .shadow_lg()
                                .child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(t("Run task")),
                                )
                                .child(list),
                        ),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    fn render_loops_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(pos) = self.loops_menu else {
            return div().into_any_element();
        };
        let mut list = v_flex().gap_px().w_full();
        if self.loops.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t("No loops yet.")),
            );
        }
        for (idx, lp) in self.loops.iter().enumerate() {
            let name = lp.name.clone();
            let sched = loop_schedule_summary(&lp.schedule);
            let program = lp
                .preset_id
                .and_then(|id| self.presets.iter().find(|p| p.id == id))
                .and_then(|p| p.program.clone());
            let fg = if lp.enabled {
                cx.theme().foreground
            } else {
                cx.theme().muted_foreground
            };
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .w_full()
                    .child(
                        div()
                            .id(SharedString::from(format!("loop-item-{idx}")))
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .text_color(fg)
                            .hover(|s| s.bg(cx.theme().accent))
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.loops_menu = None;
                                this.run_loop_now(idx, window, cx);
                            }))
                            .child(agent_icon(program.as_deref(), px(15.0), fg))
                            .child(div().min_w_0().text_sm().child(name))
                            .child(
                                div()
                                    .ml_1()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(sched),
                            ),
                    )
                    .child(
                        self.menu_edit_button(format!("loop-edit-{idx}"), cx)
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.open_loop_settings(idx, window, cx)
                            })),
                    ),
            );
        }
        // Footer: create a new loop (opens its editor in settings).
        list = list.child(
            div()
                .id("loop-new")
                .flex()
                .items_center()
                .gap_2()
                .w_full()
                .px_2()
                .py_1()
                .mt_px()
                .rounded(cx.theme().radius)
                .cursor_pointer()
                .text_color(cx.theme().muted_foreground)
                .hover(|s| s.bg(cx.theme().accent))
                .on_click(cx.listener(|this, _e, window, cx| {
                    this.loops_menu = None;
                    this.add_loop(window, cx);
                }))
                .child(Icon::new(IconName::Plus).size(px(14.0)))
                .child(div().text_sm().child(t("New loop…"))),
        );
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.loops_menu = None;
                    cx.notify();
                }),
            )
            .child(
                deferred(
                    anchored()
                        .position(pos)
                        .snap_to_window_with_margin(px(8.0))
                        .child(
                            div()
                                .occlude()
                                .w(px(260.0))
                                .flex()
                                .flex_col()
                                .gap_px()
                                .p_1()
                                .bg(cx.theme().popover)
                                .border_1()
                                .border_color(cx.theme().border)
                                .rounded(cx.theme().radius)
                                .shadow_lg()
                                .child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(t("Loops — click to run now, pencil to edit")),
                                )
                                .child(list),
                        ),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// Run-dialog: show the runner's prompt + collect extra details, then run.
    fn render_run_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(runner) = self.active_runner.and_then(|i| self.runners.get(i)) else {
            return div().into_any_element();
        };
        let title = tf("Run: {name}", &[("name", &runner.name)]);
        let preview = runner.prompt.replace("{{input}}", "…").trim().to_string();
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0099))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.show_run_dialog = false;
                    this.active_runner = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(440.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(div().text_lg().font_semibold().child(title))
                    .child(
                        div()
                            .max_h(px(140.0))
                            .overflow_hidden()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(preview),
                    )
                    .child(Input::new(&self.runner_input))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(
                                Button::new("run-cancel")
                                    .ghost()
                                    .label(t("Cancel"))
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.show_run_dialog = false;
                                        this.active_runner = None;
                                        cx.notify();
                                    })),
                            )
                            .child(Button::new("run-go").primary().label(t("Run")).on_click(
                                cx.listener(|this, _e, window, cx| this.execute_runner(window, cx)),
                            )),
                    ),
            )
            .into_any_element()
    }

    /// "Are you sure you want to quit?" confirmation over a dimmed backdrop.
    /// Confirmation modal for a destructive action (delete / close).
    fn render_confirm_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(pending) = self.confirm.as_ref() else {
            return div().into_any_element();
        };
        let title = pending.title.clone();
        let message = pending.message.clone();
        let confirm_label = pending.confirm_label.clone();
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0099))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| this.cancel_confirm(cx)),
            )
            .child(
                div()
                    .w(px(360.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(div().text_lg().font_semibold().child(title))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(message),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(
                                Button::new("confirm-cancel")
                                    .ghost()
                                    .label(t("Cancel"))
                                    .on_click(
                                        cx.listener(|this, _e, _w, cx| this.cancel_confirm(cx)),
                                    ),
                            )
                            .child(
                                Button::new("confirm-ok")
                                    .danger()
                                    .label(confirm_label)
                                    .on_click(cx.listener(|this, _e, window, cx| {
                                        this.run_confirm(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Commit / Discard / Keep for a dirty worktree whose last instance closed.
    fn render_worktree_dispose_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = self.pending_worktree_dispose.front() else {
            return div().into_any_element();
        };
        let color = worktree_color(d.color);
        let name = d.name.clone();
        let n = d.changed;
        let m = d.unmerged;
        let base_label = d.base_label.clone();
        let show_commit = n > 0;
        // Merge only makes sense when there are commits to land.
        let show_merge = m > 0;
        // The queue condition guarantees at least one part.
        let mut parts: Vec<String> = Vec::new();
        if n > 0 {
            parts.push(tn(
                "{n} uncommitted file",
                "{n} uncommitted files",
                n,
                &[("n", &n.to_string())],
            ));
        }
        if m > 0 {
            parts.push(tn(
                "{m} unmerged commit (not in {base_label})",
                "{m} unmerged commits (not in {base_label})",
                m,
                &[("m", &m.to_string()), ("base_label", &base_label)],
            ));
        }
        let body = format!("{}.", parts.join(&t(" and ")));
        let merge_tip = tf(
            "Merge into {base_label}, then remove the worktree + branch",
            &[("base_label", &base_label)],
        );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0099))
            // Clicking the backdrop keeps the worktree (safe: never destroys work).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| this.dispose_worktree_keep(cx)),
            )
            .child(
                div()
                    .w(px(480.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(color.opacity(0.6))
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(10.0)).rounded_full().bg(color))
                            .child(
                                div()
                                    .text_lg()
                                    .font_semibold()
                                    .text_color(color)
                                    .child(name),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(body),
                    )
                    // The commit message applies to uncommitted changes only.
                    .children(show_commit.then(|| Input::new(&self.dispose_commit_input)))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(
                                Button::new("wt-keep")
                                    .ghost()
                                    .label(t("Keep"))
                                    .tooltip(t("Leave the worktree on disk to resume later"))
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.dispose_worktree_keep(cx)
                                    })),
                            )
                            .child(
                                Button::new("wt-discard")
                                    .danger()
                                    .label(t("Discard"))
                                    .tooltip(t("Delete the worktree and its branch (lose the work)"))
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.dispose_worktree_discard(cx)
                                    })),
                            )
                            .children(show_commit.then(|| {
                                Button::new("wt-commit")
                                    .label(t("Commit & close"))
                                    .tooltip(t("Commit changes to its branch, then remove the worktree (branch kept, unmerged)"))
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.dispose_worktree_commit(cx)
                                    }))
                            }))
                            .children(show_merge.then(|| {
                                Button::new("wt-merge")
                                    .primary()
                                    .label(t("Merge & close"))
                                    .tooltip(merge_tip.clone())
                                    .on_click(cx.listener(|this, _e, window, cx| {
                                        this.dispose_worktree_merge(window, cx)
                                    }))
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_quit_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0099))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.show_quit_confirm = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(360.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(div().text_lg().font_semibold().child(t("Quit muxel?")))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(t("Running terminals will be closed.")),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(
                                Button::new("quit-cancel")
                                    .ghost()
                                    .label(t("Cancel"))
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.show_quit_confirm = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("quit-confirm")
                                    .danger()
                                    .label(t("Quit"))
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.confirm_quit = true;
                                        cx.quit();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The floating terminal-search bar (shown while `term_search` is Some).
    fn render_term_search_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = match &self.term_search {
            Some(ts) if !ts.matches.is_empty() => format!("{}/{}", ts.idx + 1, ts.matches.len()),
            _ => "0/0".to_string(),
        };
        div()
            .absolute()
            .top(px(52.0))
            .right(px(16.0))
            .flex()
            .items_center()
            .gap_2()
            .pl_2()
            .pr_1()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_md()
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    this.close_term_search(window, cx);
                }
            }))
            .child(
                div()
                    .w(px(180.0))
                    .child(Input::new(&self.term_search_input)),
            )
            .child(
                div()
                    .min_w(px(34.0))
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(count),
            )
            .child(
                Button::new("ts-prev")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronUp)
                    .tooltip(t("Previous match (Enter)"))
                    .on_click(cx.listener(|this, _e, _w, cx| this.term_search_step(-1, cx))),
            )
            .child(
                Button::new("ts-next")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .tooltip(t("Next match"))
                    .on_click(cx.listener(|this, _e, _w, cx| this.term_search_step(1, cx))),
            )
            .child(
                Button::new("ts-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(t("Close (Esc)"))
                    .on_click(
                        cx.listener(|this, _e, window, cx| this.close_term_search(window, cx)),
                    ),
            )
            .into_any_element()
    }

    /// The broadcast bar: type a line, Enter (or Send) writes it to every agent
    /// in the active project.
    fn render_broadcast_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let n = self.broadcast_targets().len();
        div()
            .absolute()
            .bottom(px(16.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .w(px(560.0))
                    .max_w_full()
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().warning)
                    .shadow_lg()
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                        if ev.keystroke.key == "escape" {
                            this.broadcasting = false;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(cx.theme().warning)
                            .child(t("BROADCAST")),
                    )
                    .child(div().flex_1().child(Input::new(&self.broadcast_input)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(tn(
                                "→ {n} agent",
                                "→ {n} agents",
                                n,
                                &[("n", &n.to_string())],
                            )),
                    )
                    .child(
                        Button::new("bc-send")
                            .primary()
                            .xsmall()
                            .label(t("Send"))
                            .on_click(
                                cx.listener(|this, _e, window, cx| this.send_broadcast(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("bc-close")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(t("Close (Esc)"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.broadcasting = false;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    /// The keyboard-shortcut cheat sheet (toggled by `ShowKeys`).
    fn render_keys_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        let overrides: std::collections::HashMap<&str, &str> = self
            .settings
            .keybindings
            .iter()
            .map(|k| (k.action.as_str(), k.keystroke.as_str()))
            .collect();
        let mut rows: Vec<(String, String)> = settings_view::DEFAULT_KEYBINDINGS
            .iter()
            .map(|(name, default, _ctx)| {
                let ks = overrides.get(name).copied().unwrap_or(default);
                (humanize_action(name), prettify_keys(ks))
            })
            .collect();
        rows.push((
            t("Send Tab / Shift+Tab to terminal").into(),
            t("Tab / Shift+Tab").into(),
        ));
        rows.push((
            t("Quit muxel").into(),
            if cfg!(target_os = "macos") {
                "Cmd+Q"
            } else {
                "Ctrl+Q"
            }
            .into(),
        ));

        let list = rows
            .into_iter()
            .fold(v_flex().gap_1(), |list, (label, ks)| {
                list.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .py(px(2.0))
                        .child(div().text_sm().child(label))
                        .child(
                            div()
                                .px_2()
                                .py(px(1.0))
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().secondary)
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(ks),
                        ),
                )
            });

        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0099))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.show_keys = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(460.0))
                    .max_h(px(560.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(t("Keyboard shortcuts")),
                    )
                    .child(
                        div()
                            .id("keys-list")
                            .max_h(px(480.0))
                            .overflow_y_scroll()
                            .child(list),
                    ),
            )
            .into_any_element()
    }

    /// The in-app updater modal: shows the current version + update state, and
    /// (depending on install type) a Download/Install + Restart flow, or the
    /// package-manager command for installs that can't self-update.
    fn render_update_modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let self_updatable = self.install_kind.self_updatable();

        let mut body = v_flex().gap_3().w_full().flex_1().min_h_0().child(
            div().text_sm().text_color(muted).child(tf(
                "Current version: {version}",
                &[("version", crate::update::APP_VERSION)],
            )),
        );

        match &self.update_state {
            UpdateState::Idle => {
                body = body.child(div().text_sm().child(t("Check for a newer version.")));
            }
            UpdateState::Checking => {
                body = body.child(div().text_sm().child(t("Checking for updates…")));
            }
            UpdateState::UpToDate => {
                body = body.child(div().text_sm().child(t("You’re on the latest version.")));
            }
            UpdateState::Available(info) => {
                body = body.child(div().font_semibold().child(tf(
                    "muxel {version} is available.",
                    &[("version", &info.version.to_string())],
                )));
                let notes = info.notes.trim();
                if !notes.is_empty() {
                    // The full release notes as scrollable markdown, growing to
                    // fill the (resizable) card.
                    body = body.child(
                        div().flex_1().min_h_0().child(
                            gpui_component::text::markdown(notes.to_string())
                                .selectable(true)
                                .scrollable(true),
                        ),
                    );
                }
                if !self_updatable && let Some(hint) = self.install_kind.upgrade_hint() {
                    let mut box_ = v_flex()
                        .gap_1()
                        .p_2()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().secondary)
                        .text_xs();
                    for line in hint.lines() {
                        box_ = box_.child(div().child(line.to_string()));
                    }
                    body = body
                        .child(
                            div()
                                .text_sm()
                                .child(t("Update muxel with your package manager:")),
                        )
                        .child(box_);
                }
            }
            UpdateState::Downloading => {
                body = body.child(div().text_sm().child(t("Downloading and installing…")));
            }
            UpdateState::Ready(_) => {
                body = body.child(
                    div()
                        .text_sm()
                        .child(t("Update installed. Restart muxel to finish.")),
                );
            }
            UpdateState::Error(e) => {
                body = body.child(
                    div()
                        .text_sm()
                        .child(tf("Update failed: {error}", &[("error", &e.to_string())])),
                );
            }
        }

        // Footer: Close + a state-specific primary action.
        let mut footer = div().flex().gap_2().justify_end().pt_2().child(
            Button::new("update-close")
                .ghost()
                .label(t("Close"))
                .on_click(cx.listener(|this, _e, _w, cx| {
                    this.show_update_modal = false;
                    cx.notify();
                })),
        );
        match &self.update_state {
            UpdateState::Available(_) if self_updatable => {
                footer = footer.child(
                    Button::new("update-install")
                        .primary()
                        .label(t("Download & Install"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.start_update_download(cx))),
                );
            }
            UpdateState::Available(_) => {
                footer = footer.child(
                    Button::new("update-releases")
                        .primary()
                        .label(t("Open releases page"))
                        .on_click(
                            cx.listener(|_t, _e, _w, cx| cx.open_url(crate::update::RELEASES_URL)),
                        ),
                );
            }
            UpdateState::Ready(_) => {
                footer = footer.child(
                    Button::new("update-restart")
                        .primary()
                        .label(t("Restart now"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.apply_update_restart(cx))),
                );
            }
            UpdateState::Error(_) => {
                footer = footer.child(
                    Button::new("update-retry")
                        .label(t("Check again"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.check_for_updates(cx))),
                );
            }
            UpdateState::Idle | UpdateState::UpToDate => {
                footer = footer.child(
                    Button::new("update-check")
                        .label(t("Check now"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.check_for_updates(cx))),
                );
            }
            UpdateState::Checking | UpdateState::Downloading => {}
        }

        div()
            .absolute()
            .inset_0()
            // Opaque hitbox: block clicks/scroll/hover from falling through the
            // backdrop to the terminals + sidebar painted behind the modal.
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0099))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.show_update_modal = false;
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                if let Some((start, base)) = this.update_resize {
                    let w = (f32::from(base.width) + f32::from(ev.position.x - start.x)).max(420.0);
                    let h =
                        (f32::from(base.height) + f32::from(ev.position.y - start.y)).max(320.0);
                    this.update_modal_size = size(px(w), px(h));
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, _cx| this.update_resize = None),
            )
            .child(
                div()
                    .relative()
                    .w(self.update_modal_size.width)
                    .h(self.update_modal_size.height)
                    .max_w(relative(0.95))
                    .max_h(relative(0.9))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(div().text_lg().font_semibold().child(t("Software update")))
                    .child(body)
                    .child(footer)
                    .child(
                        // Bottom-right corner: drag to resize the modal.
                        div()
                            .absolute()
                            .bottom_0()
                            .right_0()
                            .size(px(18.0))
                            .cursor(CursorStyle::ResizeUpLeftDownRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                                    cx.stop_propagation();
                                    this.update_resize =
                                        Some((ev.position, this.update_modal_size));
                                }),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The settings page as a centered modal card over a dimmed backdrop.
    fn render_settings_modal(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let editing_agent = (self.settings_ui.selected_preset.is_some()
            && self.settings_ui.section == SettingsSection::Agents)
            || (self.settings_ui.selected_runner.is_some()
                && self.settings_ui.section == SettingsSection::Runners);
        div()
            .absolute()
            .inset_0()
            // Opaque hitbox: without this, hover/scroll/clicks fall through the
            // backdrop to the terminals and sidebar painted behind the modal.
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x0000_0099))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| this.close_settings(cx)),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                if let Some((start, base)) = this.settings_resize {
                    let w = (f32::from(base.width) + f32::from(ev.position.x - start.x)).max(420.0);
                    let h =
                        (f32::from(base.height) + f32::from(ev.position.y - start.y)).max(320.0);
                    this.settings_size = size(px(w), px(h));
                    cx.notify();
                } else if let Some((start, base)) = this.settings_move {
                    this.settings_offset = point(
                        base.x + (ev.position.x - start.x),
                        base.y + (ev.position.y - start.y),
                    );
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, _cx| {
                    this.settings_resize = None;
                    this.settings_move = None;
                }),
            )
            .child(
                div()
                    .w(self.settings_size.width)
                    .h(self.settings_size.height)
                    .relative()
                    // Shift from the centred position by the drag offset.
                    .left(self.settings_offset.x)
                    .top(self.settings_offset.y)
                    .max_w(relative(0.95))
                    .max_h(relative(0.9))
                    .flex()
                    .flex_col()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .overflow_hidden()
                    // Clicks inside the card must not reach the backdrop.
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .px_4()
                            .py_2()
                            .bg(cx.theme().title_bar)
                            // Drag the title bar to move the modal around.
                            .cursor(CursorStyle::OpenHand)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                                    cx.stop_propagation();
                                    this.settings_move = Some((ev.position, this.settings_offset));
                                }),
                            )
                            .child(div().font_semibold().child(t("Settings")))
                            .child(div().flex_1())
                            .child(
                                // Pressing Close must not begin a title-bar drag.
                                div()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .child(
                                        Button::new("close-settings")
                                            .ghost()
                                            .icon(IconName::Close)
                                            .tooltip(t("Close"))
                                            .on_click(cx.listener(|this, _ev, _w, cx| {
                                                this.close_settings(cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_settings(window, cx)),
                    )
                    .children((!editing_agent).then(|| {
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .child(
                                Button::new("settings-cancel")
                                    .ghost()
                                    .label(t("Cancel"))
                                    .on_click(
                                        cx.listener(|this, _e, _w, cx| this.cancel_settings(cx)),
                                    ),
                            )
                            .child(
                                Button::new("settings-save")
                                    .primary()
                                    .label(t("Save"))
                                    .on_click(
                                        cx.listener(|this, _e, _w, cx| this.save_settings(cx)),
                                    ),
                            )
                    }))
                    .child(
                        // Bottom-right corner: drag to resize the modal.
                        div()
                            .absolute()
                            .bottom_0()
                            .right_0()
                            .size(px(18.0))
                            .cursor(CursorStyle::ResizeUpLeftDownRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                                    cx.stop_propagation();
                                    this.settings_resize = Some((ev.position, this.settings_size));
                                }),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .bottom(px(3.0))
                                    .right(px(3.0))
                                    .size(px(9.0))
                                    .border_b_2()
                                    .border_r_2()
                                    .border_color(cx.theme().muted_foreground),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Definite inner width of the settings content pane. The modal card is
    /// `w(settings_size).max_w(relative(0.95))`, so its real width is clamped to
    /// 95% of the window — using `settings_size` alone is why inputs were only right
    /// at one window size. Subtract the nav (rems 10) and a scrollbar margin. This
    /// is the width content must be sized against absolutely, since the
    /// `overflow_y_scroll` pane gives children no definite width of their own.
    fn settings_content_w(&self, window: &mut Window) -> Pixels {
        let cap = window.viewport_size().width * 0.95;
        let modal_w = if self.settings_size.width < cap {
            self.settings_size.width
        } else {
            cap
        };
        let raw = modal_w - window.rem_size() * 10.0 - px(20.0);
        if raw < px(320.0) { px(320.0) } else { raw }
    }

    fn render_settings(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let current = self.settings_ui.section;
        let nav_item = |label: SharedString, section: SettingsSection| {
            Button::new(label.clone())
                .ghost()
                .selected(section == current)
                .label(label)
        };
        let nav = v_flex()
            .w(rems(10.0))
            .flex_none()
            .p_2()
            .gap_1()
            .bg(cx.theme().sidebar)
            .child(
                nav_item(t("Appearance"), SettingsSection::Appearance).on_click(cx.listener(
                    |this, _e, _w, cx| this.set_section(SettingsSection::Appearance, cx),
                )),
            )
            .child(nav_item(t("Editor"), SettingsSection::Editor).on_click(
                cx.listener(|this, _e, _w, cx| this.set_section(SettingsSection::Editor, cx)),
            ))
            .child(nav_item(t("Behavior"), SettingsSection::Behavior).on_click(
                cx.listener(|this, _e, _w, cx| this.set_section(SettingsSection::Behavior, cx)),
            ))
            .child(nav_item(t("Agents"), SettingsSection::Agents).on_click(
                cx.listener(|this, _e, _w, cx| this.set_section(SettingsSection::Agents, cx)),
            ))
            .child(nav_item(t("Runners"), SettingsSection::Runners).on_click(
                cx.listener(|this, _e, _w, cx| this.set_section(SettingsSection::Runners, cx)),
            ))
            .child(nav_item(t("Loops"), SettingsSection::Loops).on_click(
                cx.listener(|this, _e, _w, cx| this.set_section(SettingsSection::Loops, cx)),
            ))
            .child(nav_item(t("Remotes"), SettingsSection::Remotes).on_click(
                cx.listener(|this, _e, _w, cx| this.set_section(SettingsSection::Remotes, cx)),
            ))
            .child(nav_item(t("Projects"), SettingsSection::Projects).on_click(
                cx.listener(|this, _e, _w, cx| this.set_section(SettingsSection::Projects, cx)),
            ))
            .child(
                nav_item(t("Keybindings"), SettingsSection::Keybindings).on_click(cx.listener(
                    |this, _e, _w, cx| this.set_section(SettingsSection::Keybindings, cx),
                )),
            );

        let content_w = self.settings_content_w(window);
        let content = match current {
            SettingsSection::Appearance => self.render_settings_appearance(cx),
            SettingsSection::Editor => self.render_settings_editor(cx),
            SettingsSection::Behavior => self.render_settings_behavior(cx),
            SettingsSection::Agents => self.render_settings_agents(cx),
            SettingsSection::Runners => self.render_settings_runners(cx),
            SettingsSection::Loops => self.render_settings_loops(cx),
            SettingsSection::Remotes => self.render_settings_remotes(cx),
            SettingsSection::Projects => self.render_settings_projects(cx),
            SettingsSection::Keybindings => self.render_settings_keybindings(content_w, cx),
        };

        div()
            .size_full()
            .flex()
            .flex_row()
            .child(nav)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        div()
                            .id("settings-content")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.settings_scroll)
                            // The scroll container itself doesn't give children a
                            // definite width (percentage widths collapse), so put
                            // the content in an absolute-width inner block.
                            .child(div().w(content_w).p_4().child(content)),
                    )
                    .child(
                        div().absolute().inset_0().child(
                            Scrollbar::new(&self.settings_scroll)
                                .id("settings-sb")
                                .axis(ScrollbarAxis::Vertical),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_settings_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let current_theme = cx.theme().theme_name().clone();
        v_flex()
            .gap_3()
            .max_w(px(520.0))
            .child(self.settings_label(&t("Theme"), cx))
            .child(
                DropdownButton::new("settings-theme")
                    .button(
                        Button::new("settings-theme-btn")
                            .ghost()
                            .icon(IconName::Palette)
                            .label(current_theme),
                    )
                    .dropdown_menu(move |mut menu, _window, cx| {
                        for name in theme::theme_names(cx) {
                            menu =
                                menu.menu(name.clone(), Box::new(crate::theme::SwitchTheme(name)));
                        }
                        menu.scrollable(true)
                    }),
            )
            .child(self.settings_label(&t("Language"), cx))
            .child(
                DropdownButton::new("settings-language")
                    .button(
                        Button::new("settings-language-btn")
                            .ghost()
                            .label(crate::i18n::display_name(&crate::i18n::current_language())),
                    )
                    .dropdown_menu(move |mut menu, _window, _cx| {
                        for entry in crate::i18n::available_languages() {
                            menu = menu.menu(
                                SharedString::from(entry.1),
                                Box::new(crate::i18n::SetLanguage(entry.0.to_string())),
                            );
                        }
                        menu.scrollable(true)
                    }),
            )
            .child(self.settings_label(
                &t("Interface font size — all UI text except the terminal"),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("ui-font-dec").ghost().label("−").on_click(
                            cx.listener(|this, _e, _w, cx| this.adjust_ui_font(-1.0, cx)),
                        ),
                    )
                    .child(
                        div()
                            .w(rems(4.0))
                            .text_center()
                            .child(format!("{}", self.settings.ui_font_size.round() as i32)),
                    )
                    .child(
                        Button::new("ui-font-inc")
                            .ghost()
                            .label("+")
                            .on_click(cx.listener(|this, _e, _w, cx| this.adjust_ui_font(1.0, cx))),
                    ),
            )
            .child(self.settings_label(&t("UI zoom — scales the whole app"), cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("ui-zoom-dec")
                            .ghost()
                            .label("−")
                            .on_click(cx.listener(|this, _e, _w, cx| this.adjust_zoom(-0.1, cx))),
                    )
                    .child(
                        div()
                            .w(rems(4.0))
                            .text_center()
                            .child(format!("{}%", (self.settings.zoom * 100.0).round() as i32)),
                    )
                    .child(
                        Button::new("ui-zoom-inc")
                            .ghost()
                            .label("+")
                            .on_click(cx.listener(|this, _e, _w, cx| this.adjust_zoom(0.1, cx))),
                    ),
            )
            .child(self.settings_label(&t("Terminal font size — independent of UI zoom"), cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Button::new("term-font-dec").ghost().label("−").on_click(
                        cx.listener(|this, _e, _w, cx| this.adjust_terminal_font(-1.0, cx)),
                    ))
                    .child(
                        div()
                            .w(rems(4.0))
                            .text_center()
                            .child(format!("{}", self.settings.font_size.round() as i32)),
                    )
                    .child(Button::new("term-font-inc").ghost().label("+").on_click(
                        cx.listener(|this, _e, _w, cx| this.adjust_terminal_font(1.0, cx)),
                    )),
            )
            .child(self.settings_label(&t("Terminal font family (blank = built-in default)"), cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .child(Input::new(&self.settings_ui.font_family)),
                    )
                    .child(
                        Button::new("apply-font-family")
                            .primary()
                            .label(t("Apply"))
                            .on_click(cx.listener(|this, _e, _w, cx| this.apply_font_family(cx))),
                    ),
            )
            .child(self.settings_label(&t("Pane border"), cx))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(self.pane_border_btn("off", &t("Off"), cx))
                    .child(self.pane_border_btn("subtle", &t("Subtle"), cx))
                    .child(self.pane_border_btn("bold", &t("Bold"), cx)),
            )
            .into_any_element()
    }

    /// A checkbox + label row. The label is rendered separately because
    /// gpui-component's `Checkbox` label uses line-height 1.0, which clips
    /// descenders (g/p/y).
    /// Wrap a gpui-component `Input` so it fills the full width of a column form.
    /// The input ignores flex-grow on itself, so the proven pattern (mirroring the
    /// SSH Port/User fields) is a growing `v_flex().flex_1()` wrapper inside a flex
    /// row — the wrapper takes the width and the input fills it.
    fn wide_input(inp: Input) -> impl IntoElement {
        div().flex().child(v_flex().flex_1().child(inp))
    }

    fn check_row(&self, checkbox: Checkbox, label: &str) -> impl IntoElement {
        // The label needs a DEFINITE width — with `flex_1` its width is resolved
        // only in the layout pass, so a multi-line (wrapped) label's height is
        // mis-measured and the next row overlaps it. Use the editor-safe width
        // (pane minus the list column, capped at the form max) so one absolute
        // value fits both full-width sections and the narrower list+editor panes.
        let label_w = {
            let editor = self.settings_pane_w - px(208.0); // p_4 + list (rems 10) + gap
            let cap = px(560.0);
            let base = if editor < cap { editor } else { cap };
            let w = base - px(28.0); // checkbox + gap
            if w < px(180.0) { px(180.0) } else { w }
        };
        div()
            .flex()
            // `items_start` so the box aligns with the first line when the label wraps.
            .items_start()
            .gap_2()
            .py_0p5()
            .child(checkbox)
            .child(div().w(label_w).text_sm().child(label.to_string()))
    }

    fn render_settings_behavior(&self, cx: &mut Context<Self>) -> AnyElement {
        let global_default = self
            .presets
            .iter()
            .find(|p| p.id.to_string() == self.settings.default_preset)
            .map(|p| p.id);
        let mut preset_row = div().flex().flex_wrap().gap_1();
        for p in &self.presets {
            let id = p.id;
            preset_row = preset_row.child(
                Button::new(SharedString::from(format!("bdef-{id}")))
                    .ghost()
                    .selected(Some(id) == global_default)
                    .label(p.name.clone())
                    .on_click(
                        cx.listener(move |this, _e, _w, cx| this.set_global_default_preset(id, cx)),
                    ),
            );
        }
        v_flex()
            .gap_3()
            .max_w(px(520.0))
            .child(
                self.check_row(
                    Checkbox::new("b-notif")
                        .checked(self.notifications_enabled)
                        .on_click(cx.listener(|this, c: &bool, _w, cx| {
                            this.notifications_enabled = *c;
                            this.persist_settings();
                            cx.notify();
                        })),
                    &t("Desktop notifications"),
                ),
            )
            .child(
                div().pl(rems(1.75)).child(
                    Button::new("test-notif")
                        .ghost()
                        .small()
                        .icon(IconName::Bell)
                        .label(t("Send a test notification"))
                        .tooltip(t("Check that your desktop shows muxel notifications"))
                        .on_click(cx.listener(|_this, _e, _w, _cx| {
                            notify(
                                t("muxel test notification").to_string(),
                                t("If you can see this, notifications are working.").to_string(),
                                None,
                            );
                        })),
                ),
            )
            .child(
                self.check_row(
                    Checkbox::new("b-close")
                        .checked(self.settings.close_on_exit)
                        .on_click(
                            cx.listener(|this, c: &bool, _w, cx| this.set_close_on_exit(*c, cx)),
                        ),
                    &t("Close a pane when its process exits"),
                ),
            )
            .child(
                self.check_row(
                    Checkbox::new("b-confirm-close-term")
                        .checked(self.settings.confirm_close_terminal)
                        .on_click(cx.listener(|this, c: &bool, _w, cx| {
                            this.settings.confirm_close_terminal = *c;
                            this.persist_settings();
                            cx.notify();
                        })),
                    &t("Confirm before closing a terminal"),
                ),
            )
            .child(
                self.check_row(
                    Checkbox::new("b-confirm-close-editor")
                        .checked(self.settings.confirm_close_editor)
                        .on_click(cx.listener(|this, c: &bool, _w, cx| {
                            this.settings.confirm_close_editor = *c;
                            this.persist_settings();
                            cx.notify();
                        })),
                    &t("Confirm before closing an editor"),
                ),
            )
            .child(
                self.check_row(
                    Checkbox::new("b-confirm-close-diff")
                        .checked(self.settings.confirm_close_diff)
                        .on_click(cx.listener(|this, c: &bool, _w, cx| {
                            this.settings.confirm_close_diff = *c;
                            this.persist_settings();
                            cx.notify();
                        })),
                    &t("Confirm before closing a git-diff pane"),
                ),
            )
            .child(self.settings_label(&t("Terminal copy & paste"), cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .child(self.terminal_mouse_btn("copy_paste", &t("Right-click copy/paste"), cx))
                    .child(self.terminal_mouse_btn("menu", &t("Right-click menu"), cx))
                    .child(self.terminal_mouse_btn("copy_on_select", &t("Copy on select"), cx)),
            )
            .child(
                self.check_row(
                    Checkbox::new("b-tmux")
                        .checked(self.use_tmux)
                        .on_click(cx.listener(|this, c: &bool, _w, cx| {
                            this.use_tmux = *c;
                            this.persist_settings();
                            cx.notify();
                        })),
                    &t("New agents run in a tmux session"),
                ),
            )
            .child(
                self.check_row(
                    Checkbox::new("b-worktree")
                        .checked(self.use_worktree)
                        .on_click(cx.listener(|this, c: &bool, _w, cx| {
                            this.use_worktree = *c;
                            this.persist_settings();
                            cx.notify();
                        })),
                    &t("New agents use a git worktree"),
                ),
            )
            .child(self.settings_label(&t("Default preset for new agents"), cx))
            .child(div().flex().child(preset_row.flex_1()))
            .into_any_element()
    }

    fn render_settings_agents(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = v_flex().w(rems(10.0)).flex_none().gap_1();
        for (idx, p) in self.presets.iter().enumerate() {
            let selected = self.settings_ui.selected_preset == Some(idx);
            let program = p.program.clone();
            // Flag agents whose binary isn't installed (hidden from new-agent menus).
            let not_installed = !self.agent_runnable(p);
            let fg = if selected {
                cx.theme().sidebar_accent_foreground
            } else {
                cx.theme().foreground
            };
            let mut row =
                div()
                    .id(SharedString::from(format!("preset-item-{idx}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .text_color(fg)
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.open_preset_editor(idx, window, cx)
                    }))
                    .child(agent_icon(program.as_deref(), px(15.0), fg))
                    .child(div().flex_1().text_sm().child(p.name.clone()))
                    .children(not_installed.then(|| {
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t("not installed"))
                    }));
            if selected {
                row = row.bg(cx.theme().sidebar_accent);
            } else {
                row = row.hover(|s| s.bg(cx.theme().accent));
            }
            list = list.child(row);
        }
        list = list.child(
            Button::new("add-preset")
                .ghost()
                .icon(IconName::Plus)
                .label(t("Add preset"))
                .on_click(cx.listener(|this, _e, window, cx| this.add_preset(window, cx))),
        );

        let editor = match self.settings_ui.selected_preset {
            Some(idx) if idx < self.presets.len() => self.render_preset_editor(idx, cx),
            _ => div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child(t("Select a preset to edit, or add one."))
                .into_any_element(),
        };

        div()
            .flex()
            .flex_row()
            .gap_4()
            .child(list)
            .child(div().flex_1().min_w_0().child(editor))
            .into_any_element()
    }

    fn render_settings_runners(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = v_flex().w(rems(10.0)).flex_none().gap_1();
        for (idx, r) in self.runners.iter().enumerate() {
            let selected = self.settings_ui.selected_runner == Some(idx);
            let program = r
                .preset_id
                .and_then(|id| self.presets.iter().find(|p| p.id == id))
                .and_then(|p| p.program.clone());
            let fg = if selected {
                cx.theme().sidebar_accent_foreground
            } else {
                cx.theme().foreground
            };
            let mut row =
                div()
                    .id(SharedString::from(format!("runner-row-{idx}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .text_color(fg)
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.open_runner_editor(idx, window, cx)
                    }))
                    .child(agent_icon(program.as_deref(), px(15.0), fg))
                    .child(div().text_sm().child(r.name.clone()));
            if selected {
                row = row.bg(cx.theme().sidebar_accent);
            } else {
                row = row.hover(|s| s.bg(cx.theme().accent));
            }
            list = list.child(row);
        }
        list = list.child(
            Button::new("add-runner")
                .ghost()
                .icon(IconName::Plus)
                .label(t("Add runner"))
                .on_click(cx.listener(|this, _e, window, cx| this.add_runner(window, cx))),
        );

        let editor = match self.settings_ui.selected_runner {
            Some(idx) if idx < self.runners.len() => self.render_runner_editor(idx, cx),
            _ => div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child(t("Select a runner to edit, or add one."))
                .into_any_element(),
        };

        div()
            .flex()
            .flex_row()
            .gap_4()
            .child(list)
            .child(div().flex_1().min_w_0().child(editor))
            .into_any_element()
    }

    fn render_runner_editor(&self, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.settings_ui;
        let selected_preset = ui.r_preset_id;
        // Agent picker: "Current/default" + one button per preset.
        let mut agent_row = div().flex().flex_wrap().gap_1();
        agent_row = agent_row.child(
            Button::new("runner-agent-default")
                .ghost()
                .selected(selected_preset.is_none())
                .label(t("Current/default"))
                .on_click(cx.listener(|this, _e, _w, cx| this.set_runner_preset(None, cx))),
        );
        for p in &self.presets {
            let id = p.id;
            agent_row = agent_row.child(
                Button::new(SharedString::from(format!("runner-agent-{}", id.simple())))
                    .ghost()
                    .selected(selected_preset == Some(id))
                    .icon(agent_icon_obj(p.program.as_deref()))
                    .label(p.name.clone())
                    .on_click(
                        cx.listener(move |this, _e, _w, cx| this.set_runner_preset(Some(id), cx)),
                    ),
            );
        }

        v_flex()
            .gap_2()
            .max_w(px(560.0))
            .child(self.settings_label(&t("Name"), cx))
            .child(Self::wide_input(Input::new(&ui.r_name)))
            .child(self.settings_label(&t("Agent"), cx))
            .child(div().flex().child(agent_row.flex_1()))
            .child(self.settings_label(&t("Auto mode — Shift+Tab presses at startup"), cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("runner-presses-dec")
                            .ghost()
                            .label("−")
                            .on_click(
                                cx.listener(|this, _e, _w, cx| this.adjust_runner_presses(-1, cx)),
                            ),
                    )
                    .child(
                        div()
                            .w(rems(2.0))
                            .text_center()
                            .child(format!("{}", ui.r_presses)),
                    )
                    .child(
                        Button::new("runner-presses-inc")
                            .ghost()
                            .label("+")
                            .on_click(
                                cx.listener(|this, _e, _w, cx| this.adjust_runner_presses(1, cx)),
                            ),
                    ),
            )
            .child(self.settings_label(
                &t("Prompt — {{input}} is replaced with run-time details"),
                cx,
            ))
            .child(Self::wide_input(Input::new(&ui.r_prompt).h(px(120.0))))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("save-runner")
                            .primary()
                            .label(t("Save"))
                            .on_click(cx.listener(|this, _e, _w, cx| this.save_runner(cx))),
                    )
                    .child(
                        Button::new("del-runner")
                            .ghost()
                            .label(t("Delete"))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                let name = this
                                    .runners
                                    .get(idx)
                                    .map(|r| r.name.clone())
                                    .unwrap_or_default();
                                this.request_confirm(
                                    t("Delete runner?"),
                                    tf("The “{name}” runner will be deleted.", &[("name", &name)]),
                                    t("Delete"),
                                    ConfirmAction::DeleteRunner(idx),
                                    cx,
                                )
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_settings_loops(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = v_flex().w(rems(12.0)).flex_none().gap_1();
        for (idx, l) in self.loops.iter().enumerate() {
            let selected = self.settings_ui.selected_loop == Some(idx);
            let program = l
                .preset_id
                .and_then(|id| self.presets.iter().find(|p| p.id == id))
                .and_then(|p| p.program.clone());
            let base_fg = if l.enabled {
                cx.theme().foreground
            } else {
                cx.theme().muted_foreground
            };
            let fg = if selected {
                cx.theme().sidebar_accent_foreground
            } else {
                base_fg
            };
            let sched = loop_schedule_summary(&l.schedule);
            let mut row = div()
                .id(SharedString::from(format!("loop-row-{idx}")))
                .flex()
                .items_center()
                .gap_2()
                .w_full()
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .cursor_pointer()
                .text_color(fg)
                .on_click(
                    cx.listener(move |this, _e, window, cx| this.open_loop_editor(idx, window, cx)),
                )
                .child(agent_icon(program.as_deref(), px(15.0), fg))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(div().text_sm().child(l.name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(sched),
                        ),
                );
            if selected {
                row = row.bg(cx.theme().sidebar_accent);
            } else {
                row = row.hover(|s| s.bg(cx.theme().accent));
            }
            list = list.child(row);
        }
        list = list.child(
            Button::new("add-loop")
                .ghost()
                .icon(IconName::Plus)
                .label(t("Add loop"))
                .on_click(cx.listener(|this, _e, window, cx| this.add_loop(window, cx))),
        );

        let editor = match self.settings_ui.selected_loop {
            Some(idx) if idx < self.loops.len() => self.render_loop_editor(idx, cx),
            _ => div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child(t("Select a loop to edit, or add one."))
                .into_any_element(),
        };

        div()
            .flex()
            .flex_row()
            .gap_4()
            .child(list)
            .child(div().flex_1().min_w_0().child(editor))
            .into_any_element()
    }

    fn render_loop_editor(&self, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.settings_ui;
        let kind = ui.l_sched_kind;

        // Agent picker.
        let mut agent_row = div().flex().flex_wrap().gap_1();
        agent_row = agent_row.child(
            Button::new("loop-agent-default")
                .ghost()
                .selected(ui.l_preset_id.is_none())
                .label(t("Current/default"))
                .on_click(cx.listener(|this, _e, _w, cx| this.set_loop_preset(None, cx))),
        );
        for p in &self.presets {
            let id = p.id;
            agent_row = agent_row.child(
                Button::new(SharedString::from(format!("loop-agent-{}", id.simple())))
                    .ghost()
                    .selected(ui.l_preset_id == Some(id))
                    .icon(agent_icon_obj(p.program.as_deref()))
                    .label(p.name.clone())
                    .on_click(
                        cx.listener(move |this, _e, _w, cx| this.set_loop_preset(Some(id), cx)),
                    ),
            );
        }

        // Project picker.
        let mut project_row = div().flex().flex_wrap().gap_1();
        for p in &self.workspace.projects {
            let pidp = p.id;
            project_row = project_row.child(
                Button::new(SharedString::from(format!("loop-proj-{}", pidp.simple())))
                    .ghost()
                    .selected(ui.l_project_id == Some(pidp))
                    .label(p.name.clone())
                    .on_click(cx.listener(move |this, _e, _w, cx| this.set_loop_project(pidp, cx))),
            );
        }

        // Schedule kind toggle.
        let mut sched_row = div().flex().flex_wrap().gap_1();
        for (k, label) in [
            (0u8, t("Every minutes")),
            (1, t("Every hours")),
            (2, t("Daily at")),
        ] {
            sched_row = sched_row.child(
                Button::new(SharedString::from(format!("loop-sched-{k}")))
                    .ghost()
                    .selected(kind == k)
                    .label(label)
                    .on_click(cx.listener(move |this, _e, _w, cx| this.set_loop_sched_kind(k, cx))),
            );
        }
        let value_row = if kind == 2 {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().w(px(56.0)).child(Input::new(&ui.l_hour)))
                .child(div().child(":"))
                .child(div().w(px(56.0)).child(Input::new(&ui.l_minute)))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(t("local 24h time")),
                )
        } else {
            let unit = if kind == 0 { "minutes" } else { "hours" };
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().w(px(80.0)).child(Input::new(&ui.l_interval)))
                .child(div().text_sm().child(unit))
        };

        v_flex()
            .gap_2()
            .max_w(px(560.0))
            .child(self.settings_label(&t("Name"), cx))
            .child(Self::wide_input(Input::new(&ui.l_name)))
            .child(self.settings_label(&t("Agent"), cx))
            .child(div().flex().child(agent_row.flex_1()))
            .child(self.settings_label(&t("Project"), cx))
            .child(div().flex().child(project_row.flex_1()))
            .child(self.settings_label(&t("Schedule"), cx))
            .child(div().flex().child(sched_row.flex_1()))
            .child(value_row)
            .child(self.settings_label(&t("Auto mode — Shift+Tab presses at startup"), cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("loop-presses-dec").ghost().label("−").on_click(
                            cx.listener(|this, _e, _w, cx| this.adjust_loop_presses(-1, cx)),
                        ),
                    )
                    .child(
                        div()
                            .w(rems(2.0))
                            .text_center()
                            .child(format!("{}", ui.l_presses)),
                    )
                    .child(
                        Button::new("loop-presses-inc").ghost().label("+").on_click(
                            cx.listener(|this, _e, _w, cx| this.adjust_loop_presses(1, cx)),
                        ),
                    ),
            )
            .child(self.settings_label(&t("Prompt"), cx))
            .child(Self::wide_input(Input::new(&ui.l_prompt).h(px(120.0))))
            .child(
                self.check_row(
                    Checkbox::new("loop-exit")
                        .checked(ui.l_exit)
                        .on_click(cx.listener(|this, c: &bool, _w, cx| {
                            this.settings_ui.l_exit = *c;
                            cx.notify();
                        })),
                    &t("Exit the agent after each run (close the pane once it finishes)"),
                ),
            )
            .child(
                self.check_row(
                    Checkbox::new("loop-enabled")
                        .checked(ui.l_enabled)
                        .on_click(cx.listener(|this, c: &bool, _w, cx| {
                            this.settings_ui.l_enabled = *c;
                            cx.notify();
                        })),
                    &t("Enabled"),
                ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("loop-run-now")
                            .label(t("Run now"))
                            .icon(IconName::Play)
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.run_loop_now(idx, window, cx)
                            })),
                    )
                    .child(
                        Button::new("save-loop")
                            .primary()
                            .label(t("Save"))
                            .on_click(cx.listener(|this, _e, _w, cx| this.save_loop(cx))),
                    )
                    .child(Button::new("del-loop").ghost().label(t("Delete")).on_click(
                        cx.listener(move |this, _e, _w, cx| {
                            let name = this
                                .loops
                                .get(idx)
                                .map(|l| l.name.clone())
                                .unwrap_or_default();
                            this.request_confirm(
                                t("Delete loop?"),
                                tf("The “{name}” loop will be deleted.", &[("name", &name)]),
                                t("Delete"),
                                ConfirmAction::DeleteLoop(idx),
                                cx,
                            )
                        }),
                    )),
            )
            .into_any_element()
    }

    fn render_settings_remotes(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = v_flex().w(rems(10.0)).flex_none().gap_1();
        for (idx, h) in self.remotes.iter().enumerate() {
            let selected = self.settings_ui.selected_remote == Some(idx);
            let fg = if selected {
                cx.theme().sidebar_accent_foreground
            } else {
                cx.theme().foreground
            };
            let label = if h.name.is_empty() {
                "(unnamed)".to_string()
            } else {
                h.name.clone()
            };
            let mut row =
                div()
                    .id(SharedString::from(format!("remote-row-{idx}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .text_color(fg)
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.open_remote_editor(idx, window, cx)
                    }))
                    .child(Icon::new(IconName::Network).small())
                    .child(div().text_sm().child(label));
            if selected {
                row = row.bg(cx.theme().sidebar_accent);
            } else {
                row = row.hover(|s| s.bg(cx.theme().accent));
            }
            list = list.child(row);
        }
        list = list.child(
            Button::new("add-remote")
                .ghost()
                .icon(IconName::Plus)
                .label(t("Add host"))
                .on_click(cx.listener(|this, _e, window, cx| this.add_remote(window, cx))),
        );

        let editor = match self.settings_ui.selected_remote {
            Some(idx) if idx < self.remotes.len() => self.render_remote_editor(idx, cx),
            _ => div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child(t("Select a host to edit, or add one. Hosts are used when creating remote projects."))
                .into_any_element(),
        };

        div()
            .flex()
            .flex_row()
            .gap_4()
            .child(list)
            .child(div().flex_1().min_w_0().child(editor))
            .into_any_element()
    }

    fn render_remote_editor(&self, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.settings_ui;
        let auth = ui.s_auth;
        let auth_btn = |label: &'static str, val: SshAuth, id: &'static str| {
            Button::new(id)
                .ghost()
                .selected(auth == val)
                .label(label)
                .on_click(cx.listener(move |this, _e, _w, cx| this.set_remote_auth(val, cx)))
        };

        let mut form = v_flex()
            .gap_2()
            .w_full()
            .max_w(px(560.0))
            .child(self.settings_label(&t("Name"), cx))
            .child(Self::wide_input(Input::new(&ui.s_name)))
            .child(self.settings_label(&t("Host (or ~/.ssh/config alias)"), cx))
            .child(Self::wide_input(Input::new(&ui.s_host)))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(self.settings_label(&t("Port"), cx))
                            .child(Input::new(&ui.s_port)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(self.settings_label(&t("User"), cx))
                            .child(Input::new(&ui.s_user)),
                    ),
            )
            .child(self.settings_label(&t("Authentication"), cx))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(auth_btn("ssh-agent", SshAuth::Agent, "remote-auth-agent"))
                    .child(auth_btn("Key file", SshAuth::Key, "remote-auth-key"))
                    .child(auth_btn("Password", SshAuth::Password, "remote-auth-pw")),
            );

        if auth == SshAuth::Key {
            form = form
                .child(self.settings_label(&t("Identity file"), cx))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .items_center()
                        .child(v_flex().flex_1().child(Input::new(&ui.s_identity)))
                        .child(
                            Button::new("remote-browse-key")
                                .ghost()
                                .icon(IconName::Folder)
                                .label(t("Browse"))
                                .on_click(cx.listener(|this, _e, window, cx| {
                                    this.browse_identity_file(window, cx)
                                })),
                        ),
                );
        } else if auth == SshAuth::Password {
            let hint = if ui.s_has_password {
                t("A password is saved in the OS keychain. Type a new one to replace it.")
            } else {
                t("Stored securely in the OS keychain — never in muxel's config.")
            };
            form = form
                .child(self.settings_label(&t("Password"), cx))
                .child(Self::wide_input(Input::new(&ui.s_password)))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(hint),
                );
            // Password auth feeds the secret to ssh via `sshpass`. Warn if it's
            // unavailable (Windows has no sshpass — use a key or ssh-agent there).
            if !self.sshpass_available {
                let warn = if cfg!(target_os = "windows") {
                    "Password auth needs `sshpass`, which isn't available on Windows. \
                     Use a key file or ssh-agent instead."
                } else {
                    "`sshpass` not found on PATH — install it for password auth, or use \
                     a key file / ssh-agent. (Windows can't use password auth.)"
                };
                form = form.child(div().text_xs().text_color(cx.theme().warning).child(warn));
            }
        }

        let forward = ui.s_forward_agent;
        let use_tmux = ui.s_use_tmux;
        form = form
            .child(self.settings_label(&t("Jump host (ProxyJump, optional)"), cx))
            .child(Self::wide_input(Input::new(&ui.s_jump)))
            .child(
                self.check_row(
                    Checkbox::new("remote-forward-agent")
                        .checked(forward)
                        .on_click(cx.listener(|this, c: &bool, _w, cx| {
                            this.settings_ui.s_forward_agent = *c;
                            cx.notify();
                        })),
                    &t("Forward the ssh-agent (-A)"),
                ),
            )
            .child(
                self.check_row(
                    Checkbox::new("remote-tmux")
                        .checked(use_tmux)
                        .on_click(cx.listener(|this, c: &bool, _w, cx| {
                            this.settings_ui.s_use_tmux = *c;
                            cx.notify();
                        })),
                    &t("Run remote panes in a persistent tmux session (survives disconnects)"),
                ),
            )
            .child(self.settings_label(&t("StrictHostKeyChecking (blank = accept-new)"), cx))
            .child(Self::wide_input(Input::new(&ui.s_strict)))
            .child(self.settings_label(&t("Keepalive — ServerAliveInterval secs (optional)"), cx))
            .child(Self::wide_input(Input::new(&ui.s_keepalive)))
            .child(self.settings_label(&t("Extra ssh -o options (one per line)"), cx))
            .child(Self::wide_input(Input::new(&ui.s_extra).h(px(60.0))));

        // Inline Test-connection result, above the buttons.
        form.children(match &self.settings_ui.s_test {
            RemoteTestState::Idle => None,
            RemoteTestState::Testing => Some(
                div()
                    .pt_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t("Connecting…"))
                    .into_any_element(),
            ),
            RemoteTestState::Ok(msg) => Some(
                div()
                    .pt_1()
                    .text_xs()
                    .text_color(cx.theme().success)
                    .child(format!("✓ {msg}"))
                    .into_any_element(),
            ),
            RemoteTestState::Failed(msg) => Some(
                div()
                    .pt_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(format!("✗ {msg}"))
                    .into_any_element(),
            ),
        })
        .child(
            div()
                .flex()
                .gap_2()
                .pt_2()
                .child(
                    Button::new("test-remote")
                        .ghost()
                        .icon(IconName::Network)
                        .label(t("Test connection"))
                        .on_click(cx.listener(move |this, _e, window, cx| {
                            this.save_remote(window, cx);
                            this.test_remote_connection(idx, window, cx);
                        })),
                )
                .child(
                    Button::new("save-remote")
                        .primary()
                        .label(t("Save"))
                        .on_click(cx.listener(|this, _e, window, cx| this.save_remote(window, cx))),
                )
                .child(
                    Button::new("del-remote")
                        .ghost()
                        .label(t("Delete"))
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            let name = this
                                .remotes
                                .get(idx)
                                .map(|h| h.name.clone())
                                .unwrap_or_default();
                            this.request_confirm(
                                t("Delete host?"),
                                tf(
                                    "The “{name}” SSH host and its saved password will be removed.",
                                    &[("name", &name)],
                                ),
                                t("Delete"),
                                ConfirmAction::DeleteRemote(idx),
                                cx,
                            )
                        })),
                ),
        )
        .into_any_element()
    }

    fn render_preset_editor(&self, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.settings_ui;
        let inj = ui.p_injection.clone();
        let is_flag = matches!(inj, InjectionMode::CliFlag { .. });
        v_flex()
            .gap_2()
            .max_w(px(560.0))
            .child(self.settings_label(&t("Name"), cx))
            .child(Self::wide_input(Input::new(&ui.p_name)))
            .child(self.settings_label(&t("Program (blank = default shell)"), cx))
            .child(Self::wide_input(Input::new(&ui.p_program)))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(self.settings_label(&t("Model"), cx))
                            .child(Input::new(&ui.p_model)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(self.settings_label(&t("Model flag"), cx))
                            .child(Input::new(&ui.p_model_flag)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(self.settings_label(&t("Effort"), cx))
                            .child(Input::new(&ui.p_effort)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(self.settings_label(&t("Effort flag"), cx))
                            .child(Input::new(&ui.p_effort_flag)),
                    ),
            )
            .child(self.settings_label(&t("Extra arguments"), cx))
            .child(Self::wide_input(Input::new(&ui.p_args)))
            .child(self.settings_label(&t("System prompt"), cx))
            .child(Self::wide_input(Input::new(&ui.p_prompt).h(px(72.0))))
            .child(self.settings_label(&t("System-prompt injection"), cx))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        Button::new("inj-none")
                            .ghost()
                            .selected(matches!(inj, InjectionMode::None))
                            .label(t("None"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.set_editor_injection(InjectionMode::None, cx)
                            })),
                    )
                    .child(
                        Button::new("inj-flag")
                            .ghost()
                            .selected(is_flag)
                            .label(t("CLI flag"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.set_editor_injection(
                                    InjectionMode::CliFlag {
                                        flag: String::new(),
                                    },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("inj-typein")
                            .ghost()
                            .selected(matches!(inj, InjectionMode::TypeIn))
                            .label(t("Type-in"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.set_editor_injection(InjectionMode::TypeIn, cx)
                            })),
                    ),
            )
            .children(is_flag.then(|| {
                v_flex()
                    .gap_1()
                    .child(self.settings_label(&t("Injection flag"), cx))
                    .child(Self::wide_input(Input::new(&ui.p_inj_flag)))
            }))
            .child(self.settings_label(&t("Environment (KEY=VALUE per line)"), cx))
            .child(Self::wide_input(Input::new(&ui.p_env).h(px(60.0))))
            .child(self.settings_label(&t("Status: working markers (comma-separated)"), cx))
            .child(Self::wide_input(Input::new(&ui.p_working_markers)))
            .child(self.settings_label(&t("Status: blocked markers (comma-separated)"), cx))
            .child(Self::wide_input(Input::new(&ui.p_blocked_markers)))
            .child(self.settings_label(&t("Runner startup delay (ms after first output)"), cx))
            .child(Self::wide_input(Input::new(&ui.p_startup_delay)))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("save-preset")
                            .primary()
                            .label(t("Save"))
                            .on_click(cx.listener(|this, _e, _w, cx| this.save_preset(cx))),
                    )
                    .child(
                        Button::new("dup-preset")
                            .ghost()
                            .label(t("Duplicate"))
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.duplicate_preset(idx, window, cx)
                            })),
                    )
                    .child(
                        Button::new("del-preset")
                            .ghost()
                            .label(t("Delete"))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                let name = this
                                    .presets
                                    .get(idx)
                                    .map(|p| p.name.clone())
                                    .unwrap_or_default();
                                this.request_confirm(
                                    t("Delete agent?"),
                                    tf(
                                        "The “{name}” agent preset will be deleted.",
                                        &[("name", &name)],
                                    ),
                                    t("Delete"),
                                    ConfirmAction::DeletePreset(idx),
                                    cx,
                                )
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_settings_projects(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = v_flex().w(rems(10.0)).flex_none().gap_1();
        for project in &self.workspace.projects {
            let pid = project.id;
            let selected = self.settings_ui.selected_project == Some(pid);
            list = list.child(
                Button::new(SharedString::from(format!("proj-item-{pid}")))
                    .ghost()
                    .selected(selected)
                    .label(project.name.clone())
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.open_project_editor(pid, window, cx)
                    })),
            );
        }

        let editor = match self.settings_ui.selected_project {
            Some(pid) if self.workspace.project(pid).is_some() => {
                self.render_project_editor(pid, cx)
            }
            _ => div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child(t("Select a project to edit."))
                .into_any_element(),
        };

        div()
            .flex()
            .flex_row()
            .gap_4()
            .child(list)
            .child(div().flex_1().min_w_0().child(editor))
            .into_any_element()
    }

    fn render_project_editor(&self, pid: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let root = self
            .workspace
            .project(pid)
            .map(|p| p.root_path.display().to_string())
            .unwrap_or_default();
        let default_id = self.workspace.project(pid).and_then(|p| p.default_preset);
        let mut preset_row = div().flex().flex_wrap().gap_1();
        for p in &self.presets {
            let id = p.id;
            preset_row =
                preset_row.child(
                    Button::new(SharedString::from(format!("pdef-{id}")))
                        .ghost()
                        .selected(Some(id) == default_id)
                        .label(p.name.clone())
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.set_project_default_preset(id, cx)
                        })),
                );
        }
        v_flex()
            .gap_2()
            .max_w(px(560.0))
            .child(self.settings_label(&t("Name"), cx))
            .child(Self::wide_input(Input::new(&self.settings_ui.proj_name)))
            .child(self.settings_label(&t("Folder"), cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().text_sm().child(root))
                    .child(
                        Button::new("change-folder")
                            .ghost()
                            .label(t("Change…"))
                            .on_click(cx.listener(|this, _e, window, cx| {
                                this.change_project_folder(window, cx)
                            })),
                    ),
            )
            .child(self.settings_label(&t("Default preset"), cx))
            .child(div().flex().child(preset_row.flex_1()))
            .child(self.check_row(
                Checkbox::new("proj-memory")
                    .checked(
                        self.workspace
                            .project(pid)
                            .is_some_and(|p| p.memory_enabled),
                    )
                    .on_click(cx.listener(move |this, _c: &bool, _w, cx| {
                        this.toggle_project_memory(pid, cx)
                    })),
                &t("Shared agent memory — agents read + append lessons in .muxel/MEMORY.md across runs"),
            ))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("save-project")
                            .primary()
                            .label(t("Save"))
                            .on_click(cx.listener(|this, _e, _w, cx| this.save_project(cx))),
                    )
                    .child(
                        Button::new("del-project")
                            .ghost()
                            .label(t("Delete project"))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.request_confirm(
                                    t("Delete project?"),
                                    t("The project and its panes will be removed."),
                                    t("Delete"),
                                    ConfirmAction::DeleteProject(pid),
                                    cx,
                                )
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_settings_keybindings(&self, content_w: Pixels, cx: &mut Context<Self>) -> AnyElement {
        // gpui-component `Input`s only size reliably with an ABSOLUTE width here:
        // inside the settings `overflow_y_scroll` pane, percentage/flex widths
        // collapse to a tiny square (this is exactly how the new-remote dialog's
        // `div().w(px(460))` card sizes its inputs). `content_w` is the pane's
        // definite inner width; derive the form/input widths from it.
        let form_w = {
            let r = content_w - px(32.0); // the inner block's p_4 (both sides)
            if r < px(280.0) { px(280.0) } else { r }
        };
        let name_w = px(150.0);
        let input_w = {
            let r = form_w - name_w - px(16.0);
            if r < px(120.0) { px(120.0) } else { r }
        };
        let mut form = v_flex().gap_2().w(form_w);
        for (name, input) in &self.settings_ui.keybinds {
            form = form.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().w(name_w).flex_none().text_sm().child(name.clone()))
                    .child(div().w(input_w).child(Input::new(input))),
            );
        }
        let form = form
            .child(
                div()
                    .pt_2()
                    .child(
                        self.settings_label(&t("Pass these keys through to the focused terminal"), cx),
                    )
                    .child(div().w(form_w).child(Input::new(&self.settings_ui.passthrough_keys))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "Comma-separated chords sent to the terminal instead of muxel (e.g. ctrl-p \
                 for opencode's commands). They won't trigger muxel's shortcut while a \
                 terminal is focused.",
                    ),
            )
            .child(
                div().pt_2().flex().items_center().gap_3().child(
                    Button::new("apply-keys")
                        .primary()
                        .label(t("Apply keybindings"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.apply_keybindings(cx))),
                ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        t("Examples: ctrl-shift-t, cmd-w, ctrl-shift-up. Applied immediately + saved."),
                    ),
            );
        form.into_any_element()
    }
}

/// List files under `root`, gitignore-aware, capped to keep the palette snappy.
fn list_project_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if entry.file_type().is_some_and(|t| t.is_file()) {
            out.push(entry.into_path());
            if out.len() >= 10_000 {
                break;
            }
        }
    }
    out
}

/// Read the active project's text files (gitignore-aware) into memory for live
/// content search. Skips oversized/binary files and caps total bytes so a huge
/// repo doesn't blow up memory.
fn read_project_contents(root: &std::path::Path) -> Vec<(PathBuf, String)> {
    const PER_FILE_MAX: u64 = 512 * 1024;
    const TOTAL_MAX: usize = 48 * 1024 * 1024;
    let mut out = Vec::new();
    let mut total = 0usize;
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if std::fs::metadata(&path)
            .map(|m| m.len())
            .unwrap_or(u64::MAX)
            > PER_FILE_MAX
        {
            continue;
        }
        // read_to_string fails on non-UTF-8 (binary) files, which we skip.
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        total += content.len();
        out.push((path, content));
        if total >= TOTAL_MAX || out.len() >= 10_000 {
            break;
        }
    }
    out
}

/// Heuristic: does the query look like a file name/path (vs. a search phrase)?
fn looks_like_path(q: &str) -> bool {
    let q = q.trim();
    !q.is_empty() && !q.contains(' ') && (q.contains('.') || q.contains('/'))
}

impl Focusable for MuxelApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MuxelApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Cache the settings pane width so deep helpers can size wrapping labels
        // absolutely (their multi-line height is otherwise mis-measured).
        if self.show_settings {
            self.settings_pane_w = self.settings_content_w(window);
        }
        // First-run Terms acceptance gates everything else. These screens still
        // need a draggable title bar (with window controls) to move the window.
        if self.show_terms {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(cx.theme().background)
                .child(self.render_minimal_titlebar(cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .child(self.render_terms_screen(cx)),
                )
                .into_any_element();
        }
        if self.show_workspace_selector {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(cx.theme().background)
                .child(self.render_minimal_titlebar(cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .child(self.render_workspace_selector(cx))
                        .children(
                            self.confirm
                                .is_some()
                                .then(|| self.render_confirm_modal(cx)),
                        ),
                )
                .into_any_element();
        }
        // Rebuild any editors awaiting re-dock (needs the main window).
        self.drain_editor_redocks(window, cx);

        // The title bar shows the active *workspace* name (next to "muxel"), not
        // the highlighted project.
        let active_name = self
            .current_workspace
            .and_then(|id| self.workspaces.workspaces.iter().find(|w| w.id == id))
            .map(|w| w.name.clone())
            .unwrap_or_default();
        let active_layout = self.workspace.active().and_then(|p| p.layout.clone());

        // A maximized terminal (in the active project) fills the pane area.
        let maximized_here = self.maximized.filter(|id| {
            self.workspace
                .active()
                .map(|p| p.instances().contains(id))
                .unwrap_or(false)
        });
        let main_content: AnyElement = if self.show_dashboard {
            self.render_dashboard(cx)
        } else if let Some(iid) = maximized_here {
            self.render_pane(&PaneNode::leaf(iid), cx)
        } else {
            match active_layout {
                Some(root) => self.render_pane(&root, cx),
                None => {
                    let msg = if self.workspace.projects.is_empty() {
                        t("No projects yet — click New Project in the sidebar.")
                    } else {
                        t("No terminals — pick a preset and Split.")
                    };
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(cx.theme().muted_foreground)
                        .child(msg)
                        .into_any_element()
                }
            }
        };

        // The project sidebar is resizable (a draggable splitter) when shown.
        let main_column = div()
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().child(main_content));
        // The file browser (second sidebar) nests its own resizable so its width
        // persists independently of the project sidebar.
        let center: AnyElement = if self.show_file_browser {
            let fb_half = (f32::from(window.viewport_size().width) * 0.5).max(360.0);
            let fb_saved = self
                .workspace
                .file_browser_width
                .unwrap_or(240.0)
                .clamp(180.0, fb_half);
            let fb_key = SharedString::from(format!(
                "fb-split-{}",
                self.current_workspace
                    .map(|p| p.simple().to_string())
                    .unwrap_or_default()
            ));
            h_resizable(fb_key)
                .child(
                    resizable_panel()
                        .size(px(fb_saved))
                        .size_range(px(180.0)..px(fb_half))
                        .child(self.render_file_browser(cx)),
                )
                .child(resizable_panel().child(main_column))
                .on_resize(|state, _window, cx| {
                    let width = state.read(cx).sizes().first().map(|p| f32::from(*p));
                    if let Some(width) = width
                        && let Some(app) =
                            cx.try_global::<MuxelHandle>().and_then(|h| h.0.upgrade())
                    {
                        app.update(cx, |app, cx| app.set_file_browser_width(width, cx));
                    }
                })
                .into_any_element()
        } else {
            main_column.into_any_element()
        };
        let body: AnyElement = if self.sidebar_collapsed {
            center
        } else {
            // Allow dragging the sidebar to at least half the window width.
            let half = (f32::from(window.viewport_size().width) * 0.5).max(440.0);
            let saved = self
                .workspace
                .sidebar_width
                .unwrap_or(232.0)
                .clamp(160.0, half);
            // Key the resize state by workspace so each workspace's saved width seeds.
            let key = SharedString::from(format!(
                "sidebar-split-{}",
                self.current_workspace
                    .map(|p| p.simple().to_string())
                    .unwrap_or_default()
            ));
            h_resizable(key)
                .child(
                    resizable_panel()
                        .size(px(saved))
                        .size_range(px(160.0)..px(half))
                        .child(self.render_sidebar(cx)),
                )
                .child(resizable_panel().child(center))
                .on_resize(|state, _window, cx| {
                    let width = state.read(cx).sizes().first().map(|p| f32::from(*p));
                    if let Some(width) = width
                        && let Some(app) =
                            cx.try_global::<MuxelHandle>().and_then(|h| h.0.upgrade())
                    {
                        app.update(cx, |app, cx| app.set_sidebar_width(width, cx));
                    }
                })
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .key_context("muxel")
            // Focus target for "deselect pane": focusing this (a non-Terminal
            // context) routes muxel shortcuts (incl. Ctrl+P) to the root handlers.
            .track_focus(&self.focus_handle)
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // While a tab/pane is dragged, this fires first (capture phase) each
            // move and clears the drop indicators; the element under the cursor
            // then sets the right one. So indicators vanish over no pane, and the
            // strip (tab_drop) and body (pane_drop) stay mutually exclusive.
            .on_drag_move::<DragInstance>(cx.listener(
                |this, _ev: &DragMoveEvent<DragInstance>, _w, cx| {
                    this.clear_tab_drop(cx);
                    this.clear_pane_drop(cx);
                },
            ))
            .on_drag_move::<DragPane>(
                cx.listener(|this, _ev: &DragMoveEvent<DragPane>, _w, cx| this.clear_pane_drop(cx)),
            )
            .on_action(cx.listener(|this, _: &NewPane, window, cx| {
                this.new_like_active(PlacementMode::Split(SplitDirection::Horizontal), window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| {
                this.new_like_active(PlacementMode::Tab, window, cx)
            }))
            .on_action(cx.listener(|this, _: &TabNext, window, cx| this.cycle_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &TabPrev, window, cx| this.cycle_tab(-1, window, cx)))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.add_agent(SplitDirection::Horizontal, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                this.add_agent(SplitDirection::Vertical, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ClosePane, window, cx| this.close_active(window, cx)))
            .on_action(
                cx.listener(|this, _: &FocusNext, window, cx| this.focus_sibling(1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &FocusPrev, window, cx| this.focus_sibling(-1, window, cx)),
            )
            .on_action(cx.listener(|this, _: &ZoomIn, _window, cx| this.adjust_zoom(0.1, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _window, cx| this.adjust_zoom(-0.1, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _window, cx| this.toggle_sidebar(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleDashboard, _window, cx| this.toggle_dashboard(cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleSettings, window, cx| {
                    this.toggle_settings(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &SendTab, _w, cx| this.send_to_active(b"\t", cx)))
            .on_action(
                cx.listener(|this, _: &SendBackTab, _w, cx| this.send_to_active(b"\x1b[Z", cx)),
            )
            .on_action(cx.listener(|this, _: &GlobalSearch, window, cx| {
                this.open_search_palette(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &FindInProject, window, cx| this.open_find_panel(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SaveFile, window, cx| this.save_active_editor(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SaveFileAs, window, cx| {
                this.save_as_active_editor(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ClearTerminal, _w, cx| this.clear_active_terminal(cx)),
            )
            .on_action(
                cx.listener(|this, _: &FocusAttention, window, cx| {
                    this.focus_attention(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &FocusLeft, window, cx| {
                this.focus_direction(FocusDir::Left, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusRight, window, cx| {
                this.focus_direction(FocusDir::Right, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusUp, window, cx| {
                this.focus_direction(FocusDir::Up, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusDown, window, cx| {
                this.focus_direction(FocusDir::Down, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowKeys, _w, cx| {
                this.show_keys = !this.show_keys;
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _: &SearchTerminal, window, cx| {
                    this.open_term_search(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ToggleBroadcast, window, cx| {
                this.toggle_broadcast(window, cx)
            }))
            .on_action(
                cx.listener(|this, a: &JumpToTab, window, cx| this.jump_to_tab(a.0, window, cx)),
            )
            .child(self.render_titlebar(active_name, cx))
            .child(div().flex_1().min_h_0().flex().child(body))
            .children(
                self.show_settings
                    .then(|| self.render_settings_modal(window, cx)),
            )
            .children(
                self.show_search_palette
                    .then(|| self.render_search_palette(cx)),
            )
            .children(self.show_find_panel.then(|| self.render_find_panel(cx)))
            .children(self.show_update_modal.then(|| self.render_update_modal(cx)))
            .children(self.show_quit_confirm.then(|| self.render_quit_modal(cx)))
            .children(self.git_modal.is_some().then(|| self.render_git_modal(cx)))
            .children(
                self.show_new_remote
                    .then(|| self.render_remote_project_modal(cx)),
            )
            .children(
                self.password_prompt
                    .is_some()
                    .then(|| self.render_password_prompt(cx)),
            )
            .children(self.show_keys.then(|| self.render_keys_overlay(cx)))
            .children(
                self.term_search
                    .is_some()
                    .then(|| self.render_term_search_bar(cx)),
            )
            .children(self.broadcasting.then(|| self.render_broadcast_bar(cx)))
            .children(
                (!self.pending_worktree_dispose.is_empty())
                    .then(|| self.render_worktree_dispose_modal(cx)),
            )
            .children(
                self.place_menu
                    .is_some()
                    .then(|| self.render_place_menu(cx)),
            )
            .children(
                self.runners_menu
                    .is_some()
                    .then(|| self.render_runners_menu(cx)),
            )
            .children(
                self.loops_menu
                    .is_some()
                    .then(|| self.render_loops_menu(cx)),
            )
            .children(self.show_run_dialog.then(|| self.render_run_dialog(cx)))
            .children(
                self.confirm
                    .is_some()
                    .then(|| self.render_confirm_modal(cx)),
            )
            // No toast layer: all notifications go to the sidebar feed instead.
            .into_any_element()
    }
}

#[cfg(test)]
mod shell_title_tests {
    use super::shell_dir_title;

    #[test]
    fn strips_user_host_prefix() {
        assert_eq!(
            shell_dir_title("ryan@zen-rhel:~/Projects/Bot/phBot"),
            "~/Projects/Bot/phBot"
        );
        // No `user@host` prefix → unchanged (bare path or a running command).
        assert_eq!(shell_dir_title("~/Projects"), "~/Projects");
        assert_eq!(shell_dir_title("make build"), "make build");
        // A colon but no `@` before it → unchanged.
        assert_eq!(shell_dir_title("12:34"), "12:34");
    }
}
