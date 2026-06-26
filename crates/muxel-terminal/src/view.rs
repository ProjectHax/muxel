//! [`TerminalView`] — a gpui entity that owns a [`TerminalSession`], drains its
//! output into the grid, renders it via [`TerminalElement`], and forwards
//! keyboard input.

use crate::colors::TerminalPalette;
use crate::element::TerminalElement;
use crate::keymap::{KeyModifiers, key_to_bytes};
use crate::session::{CommandSpec, PtyChunk, TerminalSession};
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

pub struct TerminalView {
    session: Arc<TerminalSession>,
    focus_handle: FocusHandle,
    palette: TerminalPalette,
    font_family: SharedString,
    font_size: f32,
    mouse_mode: TerminalMouseMode,
    exited: bool,
    /// On-screen markers that classify the agent's status (per-agent).
    working_markers: Vec<String>,
    blocked_markers: Vec<String>,
    _drain: Task<()>,
}

impl TerminalView {
    /// Spawn a terminal running `spec` and wire up its output drain.
    pub fn new(spec: CommandSpec, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Try the requested program; if it can't be launched (e.g. the agent
        // isn't installed), fall back to a shell that shows the error rather than
        // crashing the whole app.
        let (spec, session, rx) = match TerminalSession::spawn(spec.clone(), 80, 24) {
            Ok((session, rx)) => (spec, session, rx),
            Err(e) => {
                let prog = spec.program.replace(['\'', '"'], "");
                // `{e:#}` includes the full anyhow context chain (e.g. the real
                // OS error: "No such file or directory"), not just the top context.
                let detail = format!("{e:#}").replace(['\'', '"', '\n', '\r'], " ");
                let shell = CommandSpec::shell().with_startup_input(format!(
                    "printf '%s\\n' 'muxel: could not launch {prog}: {detail}'"
                ));
                let (session, rx) = TerminalSession::spawn(shell.clone(), 80, 24)
                    .expect("failed to spawn fallback shell");
                (shell, session, rx)
            }
        };
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
                let mut exit: Option<Option<i32>> = None;
                match chunk {
                    PtyChunk::Output(b) => output.extend_from_slice(&b),
                    PtyChunk::Exit(c) => exit = Some(c),
                }
                // Coalesce whatever else is already buffered.
                while let Ok(more) = rx.try_recv() {
                    match more {
                        PtyChunk::Output(b) => output.extend_from_slice(&b),
                        PtyChunk::Exit(c) => {
                            exit = Some(c);
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
                        }
                        if exit.is_some() {
                            view.exited = true;
                        }
                        cx.notify();
                        exit.is_some()
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
            working_markers,
            blocked_markers,
            _drain: drain,
        }
    }

    /// Convenience: spawn the user's login shell.
    pub fn shell(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(CommandSpec::shell(), window, cx)
    }

    pub fn session(&self) -> &Arc<TerminalSession> {
        &self.session
    }

    pub fn exited(&self) -> bool {
        self.exited
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
        classify(
            self.exited,
            &screen,
            &self.working_markers,
            &self.blocked_markers,
            self.session.has_bell(),
            self.session.idle_for(),
        )
    }

    pub fn title(&self) -> Option<String> {
        self.session.title()
    }

    /// Replace the color palette used to render this terminal.
    pub fn set_palette(&mut self, palette: TerminalPalette) {
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

#[cfg(test)]
mod tests {
    // Import specifically (not `super::*`) so `#[test]` resolves to the built-in
    // macro, not gpui's glob-imported `test` attribute.
    use super::{AgentStatus, TerminalMouseMode, classify};
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
}
