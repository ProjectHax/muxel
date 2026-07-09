//! [`TerminalView`] — a gpui entity that owns a [`TerminalSession`], drains its
//! output into the grid, renders it via [`TerminalElement`], and forwards
//! keyboard input.

use crate::colors::TerminalPalette;
use crate::element::TerminalElement;
use crate::keymap::{KeyModifiers, key_to_bytes};
use crate::session::{CommandSpec, PtyChunk, TerminalSession};
use alacritty_terminal::term::ClipboardType;
use anyhow::Context as _;
use gpui::*;
use std::sync::Arc;
use std::time::Duration;

/// Stop draining after this many bytes in a single turn so one noisy terminal
/// can't starve the UI; the rest stays buffered for the next turn.
const MAX_BYTES_PER_TURN: usize = 256 * 1024;

/// A small margin between the terminal grid and the pane edge. The grid (and so
/// the reported size) is computed from the inset area, giving a TUI that renders
/// wider than expected some breathing room from the border/scrollbar instead of
/// jamming against it.
const TERM_INSET: Pixels = px(6.0);

/// Open a link the user ctrl+clicked in a terminal — an `http(s)://` URL or a
/// `file://` URI for an existing local file. Dispatched by the terminal element
/// and handled by the app, which routes URLs to the built-in browser (when
/// enabled) or the OS.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = terminal, no_json)]
pub struct OpenLink(pub String);

/// Lifecycle state of a terminal/agent, shown as a badge. Inferred from the
/// agent's TUI (per-agent markers), the bell, output activity, and process exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// Actively generating / running tools (a working marker, or recent output).
    Working,
    /// Alive but quiet — nothing pending.
    Idle,
    /// Waiting on the user — a permission/approval prompt is on screen.
    Blocked,
    /// Finished a turn (rang the bell) or the process exited.
    Done,
}

/// Decide an agent's lifecycle state from its signals. Pure (unit-testable):
/// exit wins; then on-screen markers (working spinner, blocked prompt); then a
/// rung bell means a finished turn; then recent output is the activity fallback.
fn classify(
    exited: bool,
    screen: &str,
    working: &[String],
    blocked: &[String],
    bell: bool,
    idle: Duration,
) -> AgentStatus {
    if exited {
        return AgentStatus::Done;
    }
    if working.iter().any(|m| screen.contains(m)) {
        return AgentStatus::Working;
    }
    if blocked.iter().any(|m| screen.contains(m)) {
        return AgentStatus::Blocked;
    }
    if bell {
        return AgentStatus::Done;
    }
    // Output-activity fallback ONLY for agents without a working marker. With a
    // marker configured (e.g. Claude), "working" comes solely from the marker —
    // otherwise just typing (echoed output) would flip it to "working".
    if working.is_empty() && idle < Duration::from_secs(2) {
        return AgentStatus::Working;
    }
    AgentStatus::Idle
}

/// Promote a working→idle transition to `Done`, latching it until the agent works
/// again or the pane is attended. Returns `(displayed status, new latch state)`.
/// Pure half of [`TerminalView::status`]'s done-latch, so a finished turn shows
/// Done even when the agent never rang the bell.
///
/// `can_latch` gates the whole mechanism to agents whose `Working` state comes
/// from a reliable on-screen marker. For marker-less terminals (plain shells, or
/// agents with no configured markers) `Working` is inferred from recent output
/// alone — and incidental output (a focus-change redraw when you click the pane,
/// a prompt repaint, …) would otherwise flip them Working→Idle and latch a bogus
/// `Done`. Those terminals report `Done` only from the bell or process exit.
fn latch_done(
    prev_raw: Option<AgentStatus>,
    raw: AgentStatus,
    latched: bool,
    can_latch: bool,
) -> (AgentStatus, bool) {
    match raw {
        // Active again, blocked, or already Done (bell/exit) — no latch needed.
        AgentStatus::Working | AgentStatus::Blocked | AgentStatus::Done => (raw, false),
        AgentStatus::Idle => {
            if can_latch && (latched || prev_raw == Some(AgentStatus::Working)) {
                (AgentStatus::Done, true)
            } else {
                (AgentStatus::Idle, false)
            }
        }
    }
}

/// How the mouse copies/pastes in a terminal pane (a global setting parsed from
/// `Settings.terminal_mouse`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalMouseMode {
    /// Right-click copies the selection, or pastes when nothing is selected.
    #[default]
    CopyPaste,
    /// Right-click opens a Copy/Paste menu; selection stays manual.
    RightClickMenu,
    /// Selecting text copies it immediately; right-click pastes.
    CopyOnSelect,
}

impl TerminalMouseMode {
    /// Parse the persisted setting string; unknown values fall back to the default.
    pub fn from_setting(s: &str) -> Self {
        match s {
            "menu" => Self::RightClickMenu,
            "copy_on_select" => Self::CopyOnSelect,
            _ => Self::CopyPaste,
        }
    }
}

/// How a child ended, carried from `PtyChunk::Exit` to the view in one piece so
/// the three same-typed optionals can't be transposed at a call site.
struct ExitInfo {
    code: Option<i32>,
    signal: Option<String>,
    read_error: Option<String>,
}

pub struct TerminalView {
    session: Arc<TerminalSession>,
    focus_handle: FocusHandle,
    palette: TerminalPalette,
    font_family: SharedString,
    font_size: f32,
    mouse_mode: TerminalMouseMode,
    exited: bool,
    /// The child's exit code once it has exited (`None` = still running or the
    /// code wasn't reported by the OS/PTY). `Some(1)` may mean a signal — see
    /// `exit_signal`.
    exit_code: Option<i32>,
    /// The signal that killed the child, when one did (see `PtyChunk::Exit`).
    exit_signal: Option<String>,
    /// Set when the session ended on a PTY read error rather than a clean EOF —
    /// the child may still have been healthy (see `PtyChunk::Exit`).
    exit_read_error: Option<String>,
    /// Error from a failed launch (e.g. the agent program isn't on PATH), captured
    /// for the dev console. `None` when the program launched fine.
    launch_error: Option<String>,
    /// On-screen markers that classify the agent's status (per-agent).
    working_markers: Vec<String>,
    blocked_markers: Vec<String>,
    /// Latches `Done` from a working→finished transition so a completed turn shows
    /// Done (and notifies) even when the agent didn't ring the bell. `prev_raw` is
    /// the previous *raw* classification; the latch clears when the agent works
    /// again or the pane is attended (see `clear_done`).
    prev_raw: std::cell::Cell<Option<AgentStatus>>,
    done_latch: std::cell::Cell<bool>,
    _drain: Task<()>,
}

/// A spawned terminal not yet wrapped in a view: the spec that actually ran
/// (the requested one, or the fallback shell), the live session + its output
/// receiver, and the launch error when the requested program failed to start.
/// Splitting the fallible spawn from the (infallible) gpui entity construction
/// is what lets a total launch failure surface as an error instead of a panic.
pub struct TerminalLaunch {
    spec: CommandSpec,
    session: Arc<TerminalSession>,
    rx: async_channel::Receiver<PtyChunk>,
    launch_error: Option<String>,
}

impl TerminalLaunch {
    /// Spawn `spec`; if it can't be launched (e.g. the agent isn't installed),
    /// fall back to a shell that prints the error. `Err` only when even the
    /// fallback shell can't spawn (bogus `$SHELL` and no `/bin/bash`, fd
    /// exhaustion, …).
    pub fn spawn(spec: CommandSpec) -> anyhow::Result<Self> {
        Self::spawn_with_fallback(spec, CommandSpec::shell())
    }

    /// Testable inner half of [`Self::spawn`]: the fallback spec is injectable.
    fn spawn_with_fallback(spec: CommandSpec, fallback: CommandSpec) -> anyhow::Result<Self> {
        match TerminalSession::spawn(spec.clone(), 80, 24) {
            Ok((session, rx)) => Ok(Self {
                spec,
                session,
                rx,
                launch_error: None,
            }),
            Err(e) => {
                // Capture the full error (incl. the OS code) for the dev console.
                let launch_error = format!("{e:#}");
                let prog = spec.program.replace(['\'', '"'], "");
                // `{e:#}` includes the full anyhow context chain (e.g. the real
                // OS error: "No such file or directory"), not just the top context.
                let detail = launch_error.replace(['\'', '"', '\n', '\r'], " ");
                let shell = fallback.with_startup_input(format!(
                    "printf '%s\\n' 'muxel: could not launch {prog}: {detail}'"
                ));
                let (session, rx) = TerminalSession::spawn(shell.clone(), 80, 24)
                    .with_context(|| format!("fallback shell (after `{prog}` failed: {detail})"))?;
                Ok(Self {
                    spec: shell,
                    session,
                    rx,
                    launch_error: Some(launch_error),
                })
            }
        }
    }

    /// The error from a failed launch of the requested program (the fallback
    /// shell is running instead). `None` when the program launched fine.
    pub fn launch_error(&self) -> Option<&str> {
        self.launch_error.as_deref()
    }
}

impl TerminalView {
    /// Wrap a spawned terminal in a view and wire up its output drain.
    pub fn new(launch: TerminalLaunch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let TerminalLaunch {
            spec,
            session,
            rx,
            launch_error,
        } = launch;
        let startup_input = spec.startup_input.clone();
        let auto_mode_presses = spec.auto_mode_presses;
        let startup_delay_ms = spec.startup_delay_ms;
        let submit = spec.submit;
        let working_markers = spec.working_markers.clone();
        let blocked_markers = spec.blocked_markers.clone();
        let focus_handle = cx.focus_handle();

        // Forward focus in/out to the PTY (DECSET 1004) so agents like Claude
        // know when their pane is the one the user is looking at — and only
        // notify when it isn't.
        {
            let s = session.clone();
            window
                .on_focus_in(&focus_handle, cx, move |_w, _cx| s.report_focus(true))
                .detach();
        }
        {
            let s = session.clone();
            window
                .on_focus_out(&focus_handle, cx, move |_ev, _w, _cx| s.report_focus(false))
                .detach();
        }

        // Startup automation (runners + type-in injection): once the agent is
        // ready, optionally send Shift+Tab a few times to reach auto-accept mode,
        // press Enter to confirm it, then type the prompt and press Enter.
        //
        // "Ready" = the child has produced output AND has then been quiet for
        // SETTLE_MS. This adapts to slow starters (e.g. opencode, whose first
        // output — clearing the screen — comes early but whose input box only
        // appears seconds later, once it stops emitting): we wait for the load
        // output to actually stop, not a guessed delay. SETTLE_MS is generous so
        // a brief pause mid-load isn't mistaken for ready. Capped by MAX_WAIT in
        // case a UI never goes quiet.
        if auto_mode_presses > 0 || startup_input.is_some() {
            const POLL_MS: u64 = 100;
            const SETTLE_MS: u128 = 2000;
            const MAX_WAIT_MS: u64 = 30_000;
            const KEY_GAP_MS: u64 = 150;
            const PRE_TYPE_MS: u64 = 300;
            // The prompt is typed in one burst; wait before the submit Enter so
            // the agent has finished ingesting the text and treats it as a
            // deliberate submit rather than a newline within a paste.
            const SUBMIT_DELAY_MS: u64 = 400;
            const SHIFT_TAB: &[u8] = b"\x1b[Z";
            let session = session.clone();
            cx.spawn(async move |_view: WeakEntity<Self>, cx| {
                let timer = |ms| cx.background_executor().timer(Duration::from_millis(ms));
                // Wait for the agent's first output (it has started up).
                let mut waited = 0u64;
                while !session.has_output() && waited < MAX_WAIT_MS {
                    timer(POLL_MS).await;
                    waited += POLL_MS;
                }
                if startup_delay_ms > 0 {
                    // Preset-configured fixed delay after first output — for agents
                    // that keep loading well past their first draw (e.g. opencode).
                    timer(startup_delay_ms as u64).await;
                } else {
                    // Auto: wait until output goes quiet (UI finished drawing).
                    while waited < MAX_WAIT_MS && session.idle_for().as_millis() < SETTLE_MS {
                        timer(POLL_MS).await;
                        waited += POLL_MS;
                    }
                }
                for _ in 0..auto_mode_presses {
                    session.write_input(SHIFT_TAB);
                    timer(KEY_GAP_MS).await;
                }
                // Confirm the mode switch with a single Enter.
                if auto_mode_presses > 0 {
                    session.write_input(b"\r");
                    timer(KEY_GAP_MS).await;
                }
                if let Some(input) = startup_input {
                    timer(PRE_TYPE_MS).await;
                    session.write_input(input.as_bytes());
                    // On restore, leave the prompt typed but unsubmitted.
                    if submit {
                        timer(SUBMIT_DELAY_MS).await;
                        session.write_input(b"\r");
                    }
                }
            })
            .detach();
        }

        let drain = cx.spawn(async move |view: WeakEntity<Self>, cx| {
            loop {
                let chunk = match rx.recv().await {
                    Ok(c) => c,
                    Err(_) => break,
                };

                let mut output: Vec<u8> = Vec::new();
                let mut exit: Option<ExitInfo> = None;
                match chunk {
                    PtyChunk::Output(b) => output.extend_from_slice(&b),
                    PtyChunk::Exit {
                        code,
                        signal,
                        read_error,
                    } => {
                        exit = Some(ExitInfo {
                            code,
                            signal,
                            read_error,
                        });
                    }
                }
                // Coalesce whatever else is already buffered.
                while let Ok(more) = rx.try_recv() {
                    match more {
                        PtyChunk::Output(b) => output.extend_from_slice(&b),
                        PtyChunk::Exit {
                            code,
                            signal,
                            read_error,
                        } => {
                            exit = Some(ExitInfo {
                                code,
                                signal,
                                read_error,
                            });
                            break;
                        }
                    }
                    if output.len() >= MAX_BYTES_PER_TURN {
                        break;
                    }
                }

                let stop = view
                    .update(cx, |view, cx| {
                        if !output.is_empty() {
                            view.session.process_output(&output);
                            // OSC-52 copies parsed from this batch land on the
                            // system clipboard here, where a gpui cx exists.
                            for (ty, text) in view.session.take_clipboard_stores() {
                                write_clipboard(ty, text, cx);
                            }
                        }
                        let stop = exit.is_some();
                        if let Some(info) = exit.take() {
                            view.exited = true;
                            view.exit_code = info.code;
                            view.exit_signal = info.signal;
                            view.exit_read_error = info.read_error;
                        }
                        cx.notify();
                        stop
                    })
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
        });

        Self {
            session,
            focus_handle,
            palette: TerminalPalette::default(),
            font_family: SharedString::default(),
            font_size: 14.0,
            mouse_mode: TerminalMouseMode::default(),
            exited: false,
            exit_code: None,
            exit_signal: None,
            exit_read_error: None,
            launch_error,
            working_markers,
            blocked_markers,
            prev_raw: std::cell::Cell::new(None),
            done_latch: std::cell::Cell::new(false),
            _drain: drain,
        }
    }

    pub fn session(&self) -> &Arc<TerminalSession> {
        &self.session
    }

    pub fn exited(&self) -> bool {
        self.exited
    }

    /// The child's exit code if it has exited and the OS reported one. `None`
    /// while running or when the code is unknown (e.g. a bare PTY close).
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// The signal that killed the child (`"Hangup"`, `"Killed"`, …), when one
    /// did. A pane whose child was signalled reports `exit_code() == Some(1)`,
    /// so this is the only way to tell it apart from a genuine `exit(1)`.
    pub fn exit_signal(&self) -> Option<&str> {
        self.exit_signal.as_deref()
    }

    /// The PTY read error that ended the session, when it wasn't a clean EOF.
    /// The child may not have exited at all — surfaced for diagnostics.
    pub fn exit_read_error(&self) -> Option<&str> {
        self.exit_read_error.as_deref()
    }

    /// The error from a failed launch (program not on PATH, etc.), for the dev
    /// console. `None` when the program launched (a fallback shell still ran).
    pub fn launch_error(&self) -> Option<&str> {
        self.launch_error.as_deref()
    }

    /// The agent's lifecycle state, from its per-agent on-screen markers, the
    /// bell, output activity, and process exit (see [`classify`]). Agents with no
    /// markers fall back to the bell + activity heuristic.
    pub fn status(&self) -> AgentStatus {
        // Only scan the grid when there are markers to look for.
        let screen = if self.working_markers.is_empty() && self.blocked_markers.is_empty() {
            String::new()
        } else {
            self.session.visible_text()
        };
        let raw = classify(
            self.exited,
            &screen,
            &self.working_markers,
            &self.blocked_markers,
            self.session.has_bell(),
            self.session.idle_for(),
        );
        // Only agents with a real working marker may latch Done from a
        // working→idle transition; marker-less terminals infer Working from raw
        // output activity, which incidental redraws (e.g. a focus-change repaint
        // when you click the pane) would otherwise misread as a finished turn.
        let can_latch = !self.working_markers.is_empty();
        let (status, latch) = latch_done(
            self.prev_raw.replace(Some(raw)),
            raw,
            self.done_latch.get(),
            can_latch,
        );
        self.done_latch.set(latch);
        status
    }

    /// Clear the `Done` latch — called when the pane is attended, so a finished
    /// turn drops back to Idle once you've looked at it.
    pub fn clear_done(&self) {
        self.done_latch.set(false);
    }

    /// Whether `needle` appears in the current visible grid — used by the app to
    /// spot an agent's "session not found" error for resume recovery.
    pub fn screen_has(&self, needle: &str) -> bool {
        self.session.visible_text().contains(needle)
    }

    pub fn title(&self) -> Option<String> {
        self.session.title()
    }

    /// Replace the color palette used to render this terminal. Also pushed into
    /// the session so OSC color queries answer with what's actually painted.
    pub fn set_palette(&mut self, palette: TerminalPalette) {
        self.session.set_palette(palette.clone());
        self.palette = palette;
    }

    /// Replace the font family + size (already scaled by zoom) used to render.
    /// An empty family means "use the built-in per-OS monospace default".
    pub fn set_config(&mut self, font_family: SharedString, font_size: f32) {
        self.font_family = font_family;
        self.font_size = font_size;
    }

    /// The active mouse copy/paste mode.
    pub fn mouse_mode(&self) -> TerminalMouseMode {
        self.mouse_mode
    }

    /// Set the mouse copy/paste mode (pushed from settings).
    pub fn set_mouse_mode(&mut self, mode: TerminalMouseMode) {
        self.mouse_mode = mode;
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let m = &event.keystroke.modifiers;

        // Copy / paste. On macOS the platform shortcut is ⌘C / ⌘V; everywhere
        // else it's ctrl-shift-c / ctrl-shift-v (plain ctrl-c must stay SIGINT).
        // macOS accepts the ctrl-shift combo too so muscle memory carries over.
        let copy_paste = (m.control && m.shift && !m.alt)
            || (cfg!(target_os = "macos") && m.platform && !m.control && !m.shift && !m.alt);
        if copy_paste {
            match event.keystroke.key.as_str() {
                "c" => {
                    if let Some(text) = self.session.selection_to_string()
                        && !text.is_empty()
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    return;
                }
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                        self.session.paste(&text);
                    }
                    self.session.clear_selection();
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        let mods = KeyModifiers {
            control: m.control,
            shift: m.shift,
            alt: m.alt,
            platform: m.platform,
        };
        let app_cursor = self.session.is_app_cursor_mode();
        if let Some(bytes) = key_to_bytes(
            &event.keystroke.key,
            event.keystroke.key_char.as_deref(),
            &mods,
            app_cursor,
        ) {
            // Typing dismisses any selection highlight. The terminal doesn't
            // echo locally — the typed character is drawn when the PTY echoes it
            // back (which schedules its own repaint), so only repaint here when a
            // selection was actually cleared. This halves repaints when a key is
            // held down (e.g. Enter), keeping output smooth.
            let cleared = self.session.clear_selection();
            self.session.write_input(&bytes);
            if cleared {
                cx.notify();
            }
        }
    }
}

/// Land an OSC-52 copy on the system clipboard — the primary selection where
/// the platform has one, the normal clipboard otherwise.
fn write_clipboard(ty: ClipboardType, text: String, cx: &mut Context<TerminalView>) {
    if text.is_empty() {
        return;
    }
    let item = ClipboardItem::new_string(text);
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    if ty == ClipboardType::Selection {
        cx.write_to_primary(item);
        return;
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let _ = ty;
    cx.write_to_clipboard(item);
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            // Fill the inset margin with the terminal background and inset the
            // element so the grid (sized from the inner area) never butts against
            // the pane edge — a too-wide TUI truncates inside the margin.
            .bg(self.palette.background_hsla())
            .p(TERM_INSET)
            .child(TerminalElement::new(
                self.session.clone(),
                self.focus_handle.clone(),
                self.palette.clone(),
                self.font_family.clone(),
                px(self.font_size),
                self.mouse_mode,
            ))
    }
}

// These tests spawn real processes, so they are Unix-only.
#[cfg(all(test, unix))]
mod launch_tests {
    // Import specifically (not `super::*`) so `#[test]` resolves to the built-in
    // macro, not gpui's glob-imported `test` attribute.
    use super::TerminalLaunch;
    use crate::session::CommandSpec;

    #[test]
    fn bad_program_falls_back_to_shell_with_error() {
        let launch =
            TerminalLaunch::spawn(CommandSpec::program("/definitely/not/here-muxel", vec![]))
                .expect("fallback shell should spawn");
        assert!(
            launch.launch_error().is_some(),
            "the original failure is kept for the dev console"
        );
        launch.session.kill();
    }

    #[test]
    fn double_failure_is_an_error_not_a_panic() {
        let bogus = CommandSpec::program("/definitely/not/here-muxel", vec![]);
        let result = TerminalLaunch::spawn_with_fallback(bogus.clone(), bogus);
        assert!(result.is_err(), "total failure must surface as Err");
    }

    #[test]
    fn good_program_has_no_launch_error() {
        let launch =
            TerminalLaunch::spawn(CommandSpec::program("/bin/cat", vec![])).expect("spawn cat");
        assert!(launch.launch_error().is_none());
        launch.session.kill();
    }
}

#[cfg(test)]
mod tests {
    // Import specifically (not `super::*`) so `#[test]` resolves to the built-in
    // macro, not gpui's glob-imported `test` attribute.
    use super::{AgentStatus, TerminalMouseMode, classify, latch_done};
    use std::time::Duration;

    fn m(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn mouse_mode_from_setting() {
        use TerminalMouseMode::*;
        assert_eq!(TerminalMouseMode::from_setting("copy_paste"), CopyPaste);
        assert_eq!(TerminalMouseMode::from_setting("menu"), RightClickMenu);
        assert_eq!(
            TerminalMouseMode::from_setting("copy_on_select"),
            CopyOnSelect
        );
        // Unknown / empty falls back to the default.
        assert_eq!(TerminalMouseMode::from_setting(""), CopyPaste);
        assert_eq!(TerminalMouseMode::from_setting("bogus"), CopyPaste);
        assert_eq!(TerminalMouseMode::default(), CopyPaste);
    }

    #[test]
    fn classify_priority() {
        let working = m(&["esc to interrupt"]);
        let blocked = m(&["Do you want to proceed"]);
        let busy = Duration::from_millis(100);
        let quiet = Duration::from_secs(10);

        // Exit wins over everything.
        assert_eq!(
            classify(true, "esc to interrupt", &working, &blocked, true, busy),
            AgentStatus::Done
        );
        // Working marker beats a stale bell.
        assert_eq!(
            classify(false, "… esc to interrupt", &working, &blocked, true, quiet),
            AgentStatus::Working
        );
        // Blocked marker beats the bell.
        assert_eq!(
            classify(
                false,
                "Do you want to proceed?",
                &working,
                &blocked,
                true,
                quiet
            ),
            AgentStatus::Blocked
        );
        // Bell with no marker on screen = finished a turn.
        assert_eq!(
            classify(false, "all done", &working, &blocked, true, quiet),
            AgentStatus::Done
        );
        // With a working marker configured, output activity (e.g. typing) does
        // NOT imply working — only the marker does. So no marker + recent output
        // is still Idle, not Working.
        assert_eq!(
            classify(false, "", &working, &blocked, false, busy),
            AgentStatus::Idle
        );
        assert_eq!(
            classify(false, "", &working, &blocked, false, quiet),
            AgentStatus::Idle
        );
    }

    #[test]
    fn classify_marker_less_agent_uses_heuristic() {
        // No configured markers → bell = done, activity = working, quiet = idle.
        let none: Vec<String> = Vec::new();
        assert_eq!(
            classify(false, "", &none, &none, true, Duration::from_secs(10)),
            AgentStatus::Done
        );
        assert_eq!(
            classify(false, "", &none, &none, false, Duration::from_millis(100)),
            AgentStatus::Working
        );
        assert_eq!(
            classify(false, "", &none, &none, false, Duration::from_secs(10)),
            AgentStatus::Idle
        );
    }

    #[test]
    fn done_latch_holds_a_finished_turn() {
        use AgentStatus::{Blocked, Done, Idle, Working};
        // Working → idle (no bell) latches Done...
        assert_eq!(latch_done(Some(Working), Idle, false, true), (Done, true));
        // ...and holds it across later idle ticks.
        assert_eq!(latch_done(Some(Idle), Idle, true, true), (Done, true));
        // Working again clears the latch.
        assert_eq!(
            latch_done(Some(Idle), Working, true, true),
            (Working, false)
        );
        // A bell/exit Done passes straight through (no latch needed).
        assert_eq!(latch_done(Some(Working), Done, false, true), (Done, false));
        // Idle not preceded by working stays idle (a fresh pane).
        assert_eq!(latch_done(None, Idle, false, true), (Idle, false));
        // Blocked passes through and clears the latch.
        assert_eq!(
            latch_done(Some(Idle), Blocked, true, true),
            (Blocked, false)
        );
    }

    #[test]
    fn marker_less_terminals_never_latch_done() {
        use AgentStatus::{Done, Idle, Working};
        // With `can_latch` false (a shell / marker-less agent), a working→idle
        // transition stays Idle instead of latching Done — incidental output
        // (e.g. a focus-change redraw on click) must not fake a finished turn.
        assert_eq!(latch_done(Some(Working), Idle, false, false), (Idle, false));
        // A stuck latch can't survive once latching is disallowed.
        assert_eq!(latch_done(Some(Idle), Idle, true, false), (Idle, false));
        // The bell/exit `Done` still passes straight through (precise signals).
        assert_eq!(latch_done(Some(Working), Done, false, false), (Done, false));
    }
}
