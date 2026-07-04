//! A single terminal session: a PTY child process plus the `alacritty_terminal`
//! emulator state that interprets its output.
//!
//! Threading model: a dedicated OS thread blocks reading the PTY and ships
//! bytes over an async channel. The GPUI thread drains that channel and feeds
//! the bytes through the VTE `Processor` into the `Term` (see `process_output`),
//! so the `Term` is only ever touched from the GPUI thread.

use crate::colors::TerminalPalette;
use crate::listener::{MuxelListener, SharedWriter};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{ClipboardType, Config as TermConfig, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// An item produced by the PTY reader thread.
pub enum PtyChunk {
    /// Raw bytes read from the PTY.
    Output(Vec<u8>),
    /// The child exited (or the PTY closed). Carries an exit code when known.
    Exit(Option<i32>),
}

/// What to run in a terminal.
#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// Text to type into the terminal shortly after start (system-prompt
    /// "type-in" injection). Handled by the view, not the session.
    pub startup_input: Option<String>,
    /// Shift+Tab presses to send at startup before typing (runner auto mode).
    pub auto_mode_presses: u8,
    /// Press Enter to submit after typing `startup_input` (false = leave it in
    /// the input unsubmitted).
    pub submit: bool,
    /// On-screen strings that mean the agent is actively working (e.g. its
    /// spinner footer). Empty → fall back to the output-activity heuristic.
    pub working_markers: Vec<String>,
    /// On-screen strings that mean the agent is blocked on the user (e.g. a
    /// permission/approval prompt). Empty → no marker-based blocked detection.
    pub blocked_markers: Vec<String>,
    /// Fixed delay (ms) after the agent's first output before startup automation
    /// types into it. 0 = auto (wait for output to go quiet instead).
    pub startup_delay_ms: u32,
}

impl CommandSpec {
    /// Run the user's default shell. On Windows that's PowerShell (`cmd.exe` is
    /// available as a separate preset); elsewhere it's `$SHELL`.
    pub fn shell() -> Self {
        #[cfg(windows)]
        let program = "powershell.exe".to_string();
        #[cfg(not(windows))]
        let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        Self {
            program,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            startup_input: None,
            auto_mode_presses: 0,
            submit: true,
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
        }
    }

    /// Run an arbitrary program with arguments.
    pub fn program(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: None,
            env: Vec::new(),
            startup_input: None,
            auto_mode_presses: 0,
            submit: true,
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
        }
    }

    /// Set the working/blocked status markers.
    pub fn with_markers(mut self, working: Vec<String>, blocked: Vec<String>) -> Self {
        self.working_markers = working;
        self.blocked_markers = blocked;
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_startup_input(mut self, input: impl Into<String>) -> Self {
        self.startup_input = Some(input.into());
        self
    }

    pub fn with_auto_mode(mut self, presses: u8) -> Self {
        self.auto_mode_presses = presses;
        self
    }

    pub fn with_submit(mut self, submit: bool) -> Self {
        self.submit = submit;
        self
    }

    pub fn with_startup_delay(mut self, ms: u32) -> Self {
        self.startup_delay_ms = ms;
        self
    }
}

/// How long the requested size must hold steady before we actually resize. A
/// pane close / divider drag produces a burst of size changes; coalescing them
/// into one resize avoids repeated SIGWINCHes that make TUIs redraw (and can
/// leave duplicated static output, e.g. a reprinted banner).
const RESIZE_SETTLE: Duration = Duration::from_millis(60);

/// Debounce state for resizing: the last-applied size, the size currently being
/// requested, and when that request first appeared.
struct ResizeState {
    applied: (u16, u16),
    target: (u16, u16),
    since: Instant,
}

/// A clickable link the pointer is ctrl-hovering: a span of columns on one
/// buffer line (negative = history, so the underline scrolls with the content)
/// plus the URI a click would open.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HoveredLink {
    /// Buffer line (alacritty `Line` coordinate; negative = scrollback).
    pub line: i32,
    /// Column span `[start, end)` on that line.
    pub start: usize,
    pub end: usize,
    /// What a click opens: an `http(s)://` URL or a `file://` URI.
    pub url: String,
}

pub struct TerminalSession {
    pub id: Uuid,
    term: Arc<Mutex<Term<MuxelListener>>>,
    processor: Mutex<Processor>,
    writer: SharedWriter,
    master: Box<dyn MasterPty + Send>,
    /// Kill handle for the child. The `Child` itself lives in the reader thread,
    /// which harvests the exit code after EOF (see `read_loop`).
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// The PTY child's pid (the shell/agent), captured at spawn. Compared against
    /// the terminal's foreground process group to tell whether the child is idle
    /// at its prompt vs. running a foreground command (see `is_idle_foreground`).
    child_pid: Option<u32>,
    title: Arc<Mutex<Option<String>>>,
    bell: Arc<AtomicBool>,
    /// OSC-52 copies parsed from output, pending pickup by the view (which owns
    /// the gpui context a clipboard write needs).
    clipboard_store: Arc<Mutex<Vec<(ClipboardType, String)>>>,
    /// The palette color queries are answered from (see `MuxelListener`); kept
    /// current with the app theme via [`Self::set_palette`].
    palette: Arc<Mutex<TerminalPalette>>,
    /// True while a left-drag text selection started in this terminal.
    selecting: AtomicBool,
    /// Sub-line scroll-wheel remainder, carried across wheel events.
    scroll_accum: Mutex<f32>,
    /// While dragging the scrollbar thumb: the grab offset within the thumb.
    scrollbar_drag: Mutex<Option<f32>>,
    /// Resize debounce state (skips redundant resizes + coalesces bursts).
    resize: Mutex<ResizeState>,
    /// Active search needle (lowercased not required — matching is ASCII-insensitive).
    /// Empty = no search; the element highlights matches of this each paint.
    search: Mutex<Vec<char>>,
    /// The local working directory the child was spawned in — the base for
    /// resolving relative file paths on ctrl+click. Remote panes run `ssh`
    /// locally with no cwd set, so their relative paths stay unresolvable.
    cwd: Option<std::path::PathBuf>,
    /// The link span under a ctrl+hover, if any; the element paints an underline
    /// over it and shows a pointing-hand cursor (mirrors the `search` pattern).
    hovered_link: Mutex<Option<HoveredLink>>,
    /// When output was last processed (for idle/status detection).
    last_output: Mutex<Instant>,
    /// Whether the child has produced any output yet (vs. still starting up).
    output_seen: AtomicBool,
    _reader: JoinHandle<()>,
}

impl TerminalSession {
    /// Spawn a PTY running `spec` at the given initial grid size. Returns the
    /// session plus the receiver the UI drains for output/exit events.
    pub fn spawn(
        spec: CommandSpec,
        cols: u16,
        rows: u16,
    ) -> Result<(Arc<Self>, async_channel::Receiver<PtyChunk>)> {
        let cols = cols.max(1);
        let rows = rows.max(1);

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open pty")?;

        let mut builder = CommandBuilder::new(&spec.program);
        for arg in &spec.args {
            builder.arg(arg);
        }
        // Remembered for resolving relative file paths on ctrl+click.
        let cwd = spec.cwd.as_ref().map(std::path::PathBuf::from);
        if let Some(cwd) = &spec.cwd {
            builder.cwd(cwd);
        }
        // When muxel itself runs from an AppImage, its process environment
        // carries the AppImage runtime's leakage — APPDIR/APPIMAGE/ARGV0/OWD, a
        // `MAKE` pointing back at the AppImage, and AppImage-mount entries in
        // PATH/LD_LIBRARY_PATH. Strip it so spawned shells/agents/build tools get
        // a clean system environment (otherwise e.g. cmake caches `$(MAKE)` as the
        // AppImage and `make` relaunches muxel instead of building).
        sanitize_appimage_env(&mut builder);
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");
        for (k, v) in &spec.env {
            builder.env(k, v);
        }

        let child = pair.slave.spawn_command(builder).context("spawn command")?;
        let child_pid = child.process_id();
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let writer: SharedWriter = Arc::new(Mutex::new(
            pair.master.take_writer().context("take pty writer")?,
        ));
        // `pair.slave` is dropped at the end of this function, closing the
        // parent's copy so the reader sees EOF when the child exits.

        let (tx, rx) = async_channel::unbounded::<PtyChunk>();
        let reader_handle = std::thread::Builder::new()
            .name("muxel-pty-reader".to_string())
            .spawn(move || read_loop(reader, child, tx))
            .context("spawn reader thread")?;

        let title = Arc::new(Mutex::new(None));
        let bell = Arc::new(AtomicBool::new(false));
        let clipboard_store = Arc::new(Mutex::new(Vec::new()));
        let palette = Arc::new(Mutex::new(TerminalPalette::default()));
        let listener = MuxelListener {
            writer: writer.clone(),
            title: title.clone(),
            bell: bell.clone(),
            clipboard_store: clipboard_store.clone(),
            palette: palette.clone(),
        };

        let term = Term::new(
            // Allow OSC-52 *reads* to reach the listener too — it answers them
            // with an empty reply (see `MuxelListener`) instead of alacritty's
            // default silent deny, so probing TUIs don't hang.
            TermConfig {
                osc52: Osc52::CopyPaste,
                ..TermConfig::default()
            },
            &TermSize::new(cols as usize, rows as usize),
            listener,
        );

        let session = Arc::new(Self {
            id: Uuid::new_v4(),
            term: Arc::new(Mutex::new(term)),
            processor: Mutex::new(Processor::new()),
            writer,
            master: pair.master,
            killer: Mutex::new(killer),
            child_pid,
            title,
            bell,
            clipboard_store,
            palette,
            selecting: AtomicBool::new(false),
            scroll_accum: Mutex::new(0.0),
            scrollbar_drag: Mutex::new(None),
            resize: Mutex::new(ResizeState {
                applied: (cols, rows),
                target: (cols, rows),
                since: Instant::now(),
            }),
            search: Mutex::new(Vec::new()),
            cwd,
            hovered_link: Mutex::new(None),
            last_output: Mutex::new(Instant::now()),
            output_seen: AtomicBool::new(false),
            _reader: reader_handle,
        });

        Ok((session, rx))
    }

    /// Feed PTY output through the VTE parser into the terminal grid.
    pub(crate) fn process_output(&self, data: &[u8]) {
        let mut term = self.term.lock();
        let mut processor = self.processor.lock();
        processor.advance(&mut *term, data);
        *self.last_output.lock() = Instant::now();
        self.output_seen.store(true, Ordering::Relaxed);
    }

    /// Whether the child has produced any output yet.
    pub fn has_output(&self) -> bool {
        self.output_seen.load(Ordering::Relaxed)
    }

    /// Write bytes to the PTY (user input, pastes, key sequences). Any input
    /// jumps the viewport back to the bottom, so typing while scrolled up in the
    /// history snaps you to the live prompt.
    pub fn write_input(&self, data: &[u8]) {
        self.term.lock().scroll_display(Scroll::Bottom);
        self.write_raw(data);
    }

    /// Paste text into the PTY, honoring bracketed-paste mode (so the program
    /// receives it as a paste, not as typed keystrokes). Shared by the keyboard
    /// shortcut and the mouse copy/paste modes.
    pub fn paste(&self, text: &str) {
        let payload = if self.is_bracketed_paste() {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.write_input(payload.as_bytes());
    }

    /// Write bytes to the PTY without touching the scroll position (used for
    /// non-input writes like focus reports).
    fn write_raw(&self, data: &[u8]) {
        let mut writer = self.writer.lock();
        let _ = writer.write_all(data);
        let _ = writer.flush();
    }

    /// Scroll the viewport by a mouse-wheel delta. `delta_y` is the pixel delta
    /// (positive = up / into scrollback); sub-line remainders are accumulated
    /// across events for smooth trackpad scrolling. `col`/`row` are the 0-based
    /// cell under the pointer, used only when the wheel is forwarded as a mouse
    /// report.
    ///
    /// The notches are dispatched the same three ways every terminal uses: if the
    /// app enabled mouse reporting (opencode, grok, vim-with-mouse) the wheel is
    /// forwarded as a mouse event so the app scrolls itself; on the alternate
    /// screen with alternate-scroll and no mouse reporting (e.g. plain `less`) it
    /// is emulated with cursor-key presses; otherwise it moves through our own
    /// scrollback locally.
    ///
    /// Returns whether anything changed (so the caller can request a repaint).
    pub fn scroll_wheel(&self, delta_y: f32, line_height: f32, col: usize, row: usize) -> bool {
        if line_height <= 0.0 {
            return false;
        }
        let lines = {
            let mut acc = self.scroll_accum.lock();
            *acc += delta_y;
            let lines = (*acc / line_height) as i32;
            if lines != 0 {
                *acc -= lines as f32 * line_height;
            }
            lines
        };
        if lines == 0 {
            return false;
        }

        // Decide what the wheel means, reading every relevant mode under one lock.
        enum Wheel {
            Scrolled,
            MouseReport { sgr: bool },
            Arrows { app_cursor: bool },
        }
        let action = {
            let mut term = self.term.lock();
            let mode = term.mode();
            if mode.intersects(TermMode::MOUSE_MODE) {
                Wheel::MouseReport {
                    sgr: mode.contains(TermMode::SGR_MOUSE),
                }
            } else if mode.contains(TermMode::ALT_SCREEN)
                && mode.contains(TermMode::ALTERNATE_SCROLL)
            {
                Wheel::Arrows {
                    app_cursor: mode.contains(TermMode::APP_CURSOR),
                }
            } else {
                term.scroll_display(Scroll::Delta(lines));
                Wheel::Scrolled
            }
        };

        let count = lines.unsigned_abs().min(100) as usize;
        match action {
            Wheel::Scrolled => {}
            Wheel::MouseReport { sgr } => {
                let mut buf = Vec::with_capacity(count * 16);
                for _ in 0..count {
                    push_wheel_report(&mut buf, lines > 0, col, row, sgr);
                }
                self.write_raw(&buf);
            }
            Wheel::Arrows { app_cursor } => {
                let seq: &[u8] = match (lines > 0, app_cursor) {
                    (true, true) => b"\x1bOA", // scroll up → Up arrow
                    (true, false) => b"\x1b[A",
                    (false, true) => b"\x1bOB", // scroll down → Down arrow
                    (false, false) => b"\x1b[B",
                };
                for _ in 0..count {
                    self.write_raw(seq);
                }
            }
        }
        true
    }

    /// Resize the PTY and emulator grid, **debounced**: the requested size must
    /// hold steady for [`RESIZE_SETTLE`] before it's applied, so a burst of
    /// changes from a pane close / divider drag collapses into one resize (one
    /// SIGWINCH) instead of many. Returns `true` while a resize is still
    /// settling — the caller should schedule another frame (e.g. `window.refresh`)
    /// so the pending resize eventually lands even if nothing else repaints.
    #[must_use]
    pub fn resize(&self, cols: u16, rows: u16) -> bool {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut st = self.resize.lock();
        if st.applied == (cols, rows) {
            return false; // already at this size
        }
        if st.target != (cols, rows) {
            // New target: start (or restart) the settle window.
            st.target = (cols, rows);
            st.since = Instant::now();
            return true;
        }
        if st.since.elapsed() < RESIZE_SETTLE {
            return true; // same target, still settling
        }
        // Settled — apply to the PTY (SIGWINCH) and the grid together.
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.term
            .lock()
            .resize(TermSize::new(cols as usize, rows as usize));
        st.applied = (cols, rows);
        false
    }

    /// Clear the scrollback history (keeps the current screen) and snap the view
    /// to the bottom.
    pub fn clear_scrollback(&self) {
        let mut term = self.term.lock();
        term.grid_mut().clear_history();
        term.scroll_display(Scroll::Bottom);
    }

    /// Set the search needle the element highlights (empty string clears it).
    pub fn set_search(&self, needle: &str) {
        *self.search.lock() = needle.chars().collect();
    }

    /// The current search needle (chars), for the element to highlight.
    pub(crate) fn search_needle(&self) -> Vec<char> {
        self.search.lock().clone()
    }

    /// The local working directory the child was spawned in, if any.
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    /// The ctrl-hovered link span, if any (painted as an underline).
    pub(crate) fn hovered_link(&self) -> Option<HoveredLink> {
        self.hovered_link.lock().clone()
    }

    /// Replace the hovered-link state; returns whether it actually changed (so
    /// callers only repaint on transitions, not every mouse move).
    pub(crate) fn set_hovered_link(&self, link: Option<HoveredLink>) -> bool {
        let mut cur = self.hovered_link.lock();
        if *cur == link {
            return false;
        }
        *cur = link;
        true
    }

    /// Buffer-line indices (negative = history) containing `needle`
    /// (case-insensitive), oldest to newest. Coordinates match the element's
    /// `grid[Line(n)]`, so [`Self::scroll_to_line`] brings one into view.
    pub fn search_match_lines(&self, needle: &str) -> Vec<i32> {
        use alacritty_terminal::index::{Column, Line, Point as GridPoint};
        let needle: Vec<char> = needle.chars().collect();
        if needle.is_empty() {
            return Vec::new();
        }
        let term = self.term.lock();
        let grid = term.grid();
        let cols = grid.columns();
        let screen = grid.screen_lines() as i32;
        let hist = grid.history_size() as i32;
        let mut out = Vec::new();
        for line in -hist..screen {
            let chars: Vec<char> = (0..cols)
                .map(|c| {
                    grid[GridPoint {
                        line: Line(line),
                        column: Column(c),
                    }]
                    .c
                })
                .collect();
            if crate::search::line_contains(&chars, &needle) {
                out.push(line);
            }
        }
        out
    }

    /// Scroll so buffer `line` (negative = history) is at/near the top of view.
    pub fn scroll_to_line(&self, line: i32) {
        self.set_display_offset((-line).max(0) as usize);
    }

    /// Live `(history_size, display_offset, screen_lines)` — used to lay out the
    /// scrollbar.
    pub fn grid_metrics(&self) -> (usize, usize, usize) {
        let term = self.term.lock();
        let grid = term.grid();
        (
            grid.history_size(),
            grid.display_offset(),
            grid.screen_lines(),
        )
    }

    /// Scroll so exactly `target` history lines sit above the viewport (clamped
    /// to the available history). Drives the draggable scrollbar.
    pub fn set_display_offset(&self, target: usize) {
        let mut term = self.term.lock();
        let target = target.min(term.grid().history_size());
        let cur = term.grid().display_offset();
        let delta = target as i32 - cur as i32;
        if delta != 0 {
            term.scroll_display(Scroll::Delta(delta));
        }
    }

    pub(crate) fn scrollbar_drag_start(&self, grab: f32) {
        *self.scrollbar_drag.lock() = Some(grab);
    }
    pub(crate) fn scrollbar_drag_end(&self) {
        *self.scrollbar_drag.lock() = None;
    }
    /// The grab offset within the thumb while a scrollbar drag is in progress.
    pub(crate) fn scrollbar_grab(&self) -> Option<f32> {
        *self.scrollbar_drag.lock()
    }

    /// Read the terminal grid for rendering.
    pub(crate) fn with_term<R>(&self, f: impl FnOnce(&Term<MuxelListener>) -> R) -> R {
        let term = self.term.lock();
        f(&term)
    }

    /// The visible screen as text (one row per line, newline-separated). Used for
    /// marker-based agent-status detection (e.g. scanning for "esc to interrupt").
    pub(crate) fn visible_text(&self) -> String {
        use alacritty_terminal::index::{Column, Line, Point as GridPoint};
        self.with_term(|term| {
            let grid = term.grid();
            let rows = grid.screen_lines();
            let cols = grid.columns();
            let mut s = String::with_capacity(rows * (cols + 1));
            for row in 0..rows {
                for col in 0..cols {
                    s.push(
                        grid[GridPoint {
                            line: Line(row as i32),
                            column: Column(col),
                        }]
                        .c,
                    );
                }
                s.push('\n');
            }
            s
        })
    }

    /// Mutate the terminal (e.g. to update the text selection).
    pub(crate) fn with_term_mut<R>(&self, f: impl FnOnce(&mut Term<MuxelListener>) -> R) -> R {
        let mut term = self.term.lock();
        f(&mut term)
    }

    /// The currently-selected text, if any.
    pub fn selection_to_string(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// Clear any active text selection. Returns whether there was one (so the
    /// caller can skip a repaint when nothing visually changed).
    pub fn clear_selection(&self) -> bool {
        let mut term = self.term.lock();
        let had = term.selection.is_some();
        term.selection = None;
        had
    }

    pub(crate) fn start_selecting(&self) {
        self.selecting.store(true, Ordering::Relaxed);
    }
    pub(crate) fn stop_selecting(&self) {
        self.selecting.store(false, Ordering::Relaxed);
    }
    pub(crate) fn is_selecting(&self) -> bool {
        self.selecting.load(Ordering::Relaxed)
    }

    /// Whether the app has enabled DECCKM (application cursor keys).
    pub(crate) fn is_app_cursor_mode(&self) -> bool {
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    /// Whether the app has enabled bracketed paste mode.
    pub fn is_bracketed_paste(&self) -> bool {
        self.term.lock().mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Report a focus change to the PTY (CSI I / CSI O), but only if the running
    /// program requested focus reporting (DECSET 1004). Lets agents like Claude
    /// know whether their pane is the one the user is looking at.
    pub fn report_focus(&self, focused: bool) {
        if self.term.lock().mode().contains(TermMode::FOCUS_IN_OUT) {
            // Raw write: a focus report must not yank the viewport to the bottom
            // (e.g. clicking a scrolled-up pane to read its history).
            self.write_raw(if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// The most recent OSC title, if any.
    pub fn title(&self) -> Option<String> {
        self.title.lock().clone()
    }

    /// Drain the OSC-52 copies parsed since the last call. The view lands them
    /// on the system clipboard (this crate has no gpui context of its own here).
    pub(crate) fn take_clipboard_stores(&self) -> Vec<(ClipboardType, String)> {
        std::mem::take(&mut *self.clipboard_store.lock())
    }

    /// Replace the palette color queries are answered from — pushed by the view
    /// whenever the app theme (re)applies, so answers track what's painted.
    pub(crate) fn set_palette(&self, palette: TerminalPalette) {
        *self.palette.lock() = palette;
    }

    /// Consume the "bell rang" edge.
    pub fn take_bell(&self) -> bool {
        self.bell.swap(false, Ordering::Relaxed)
    }

    /// Whether the bell has rung (non-consuming).
    pub fn has_bell(&self) -> bool {
        self.bell.load(Ordering::Relaxed)
    }

    /// Clear the bell (e.g. once the user focuses the pane).
    pub fn clear_bell(&self) {
        self.bell.store(false, Ordering::Relaxed);
    }

    /// Time since output was last processed (for idle detection).
    pub fn idle_for(&self) -> Duration {
        self.last_output.lock().elapsed()
    }

    /// Whether the child is sitting idle at its prompt with no foreground command
    /// running — i.e. the terminal's foreground process group *is* the child
    /// itself. A shell that's running `vim`/`make`/etc. puts that command in a new
    /// foreground group, so this returns `false`. Used to skip the close
    /// confirmation for an untouched shell pane.
    ///
    /// `false` when it can't be determined — no foreground group, an unknown child
    /// pid, or a platform without `tcgetpgrp` (Windows) — so callers stay safe and
    /// confirm as usual.
    pub fn is_idle_foreground(&self) -> bool {
        #[cfg(unix)]
        {
            match (self.master.process_group_leader(), self.child_pid) {
                (Some(fg), Some(pid)) => fg == pid as libc::pid_t,
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Kill the child process.
    pub fn kill(&self) {
        let _ = self.killer.lock().kill();
    }
}

/// Append one mouse-wheel report — button 64 (scroll up) / 65 (scroll down) — at
/// the 0-based cell (`col`, `row`), in SGR (1006) or legacy X10 encoding. Wheel
/// events are press-only (no release), so the SGR form always ends in `M`.
fn push_wheel_report(buf: &mut Vec<u8>, up: bool, col: usize, row: usize, sgr: bool) {
    let cb = if up { 64 } else { 65 };
    if sgr {
        // ESC [ < Cb ; Cx ; Cy M   (1-based coordinates)
        buf.extend_from_slice(format!("\x1b[<{};{};{}M", cb, col + 1, row + 1).as_bytes());
    } else {
        // ESC [ M  Cb+32  Cx+32  Cy+32   (1-based coords, classic 223-cell ceiling)
        let enc = |v: usize| -> u8 { ((v + 1).min(223) + 32) as u8 };
        buf.extend_from_slice(&[0x1b, b'[', b'M', cb + 32, enc(col), enc(row)]);
    }
}

/// Strip an AppImage runtime's environment leakage from a child command, so a
/// shell/agent muxel spawns gets a clean system environment. No-op unless muxel
/// itself is running from an AppImage (`$APPIMAGE` set).
fn sanitize_appimage_env(builder: &mut CommandBuilder) {
    let Some(appimage) = std::env::var("APPIMAGE").ok() else {
        return;
    };
    let appdir = std::env::var("APPDIR").unwrap_or_default();
    let vars: Vec<(String, String)> = std::env::vars().collect();
    let (drop, overrides) = appimage_env_fixups(&vars, &appimage, &appdir);
    for k in drop {
        builder.env_remove(&k);
    }
    for (k, v) in overrides {
        builder.env(&k, &v);
    }
}

/// Pure half of [`sanitize_appimage_env`]: given the current environment plus the
/// AppImage's binary path and mount dir, return the env keys to DROP and the
/// `(key, cleaned-value)` pairs to OVERRIDE.
///
/// - `APPDIR`/`APPIMAGE`/`ARGV0`/`OWD` (the runtime markers) are dropped.
/// - colon-separated search paths have their AppImage-mount (`$APPDIR/…`) entries
///   stripped (dropped if nothing's left).
/// - any other variable whose value is the AppImage binary or points into the
///   mount (e.g. a poisoned `MAKE`) is dropped.
fn appimage_env_fixups(
    vars: &[(String, String)],
    appimage: &str,
    appdir: &str,
) -> (Vec<String>, Vec<(String, String)>) {
    const MARKERS: [&str; 4] = ["APPDIR", "APPIMAGE", "ARGV0", "OWD"];
    const PATH_LISTS: [&str; 5] = [
        "PATH",
        "LD_LIBRARY_PATH",
        "PYTHONPATH",
        "PERLLIB",
        "XDG_DATA_DIRS",
    ];
    let in_mount = |s: &str| !appdir.is_empty() && s.starts_with(appdir);
    let mut drop = Vec::new();
    let mut overrides = Vec::new();
    for (k, v) in vars {
        if MARKERS.contains(&k.as_str()) {
            drop.push(k.clone());
        } else if PATH_LISTS.contains(&k.as_str()) {
            let kept: Vec<&str> = v
                .split(':')
                .filter(|e| !e.is_empty() && !in_mount(e))
                .collect();
            let orig = v.split(':').filter(|e| !e.is_empty()).count();
            if kept.len() != orig {
                if kept.is_empty() {
                    drop.push(k.clone());
                } else {
                    overrides.push((k.clone(), kept.join(":")));
                }
            }
        } else if v == appimage || in_mount(v) {
            drop.push(k.clone());
        }
    }
    (drop, overrides)
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Best-effort: kill the child so the reader thread sees EOF and exits.
        let _ = self.killer.lock().kill();
    }
}

/// Blocking PTY reader. Owns the `Child` so that after EOF it can harvest the
/// exit code — letting the app tell a clean `exit` from a crash (resume
/// recovery must not treat a deliberate quit as recoverable).
fn read_loop(
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn Child + Send + Sync>,
    tx: async_channel::Sender<PtyChunk>,
) {
    let mut buf = [0u8; 65536];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break, // EOF or PTY error → child is (probably) gone
            Ok(n) => {
                if tx
                    .send_blocking(PtyChunk::Output(buf[..n].to_vec()))
                    .is_err()
                {
                    return; // receiver dropped — UI is gone
                }
            }
        }
    }
    // Bounded poll for the exit status: a daemonized child that closed the PTY
    // but lives on must not wedge this thread. None after the window (or on a
    // wait error) simply reports an unknown code.
    let mut code = None;
    for _ in 0..20 {
        match child.try_wait() {
            Ok(Some(status)) => {
                code = Some(status.exit_code() as i32);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => break,
        }
    }
    let _ = tx.send_blocking(PtyChunk::Exit(code));
}

#[cfg(test)]
mod wheel_report_tests {
    use super::{appimage_env_fixups, push_wheel_report};

    #[test]
    fn appimage_env_fixups_strip_leakage_keep_the_rest() {
        let appimage = "/home/u/Apps/muxel-linux-x86_64.AppImage";
        let appdir = "/tmp/.mount_muxelXYZ";
        let s = |k: &str, v: &str| (k.to_string(), v.to_string());
        let vars = vec![
            s("APPIMAGE", appimage),
            s("APPDIR", appdir),
            s("ARGV0", appimage),
            s("OWD", "/home/u"),
            s("MAKE", appimage), // poisoned scalar
            s("PATH", &format!("{appdir}/usr/bin:/usr/bin:/bin")), // partly poisoned
            s("LD_LIBRARY_PATH", &format!("{appdir}/usr/lib")), // wholly poisoned
            s("HOME", "/home/u"), // keep
            s("EDITOR", "vim"),  // keep
        ];
        let (drop, overrides) = appimage_env_fixups(&vars, appimage, appdir);

        for k in [
            "APPIMAGE",
            "APPDIR",
            "ARGV0",
            "OWD",
            "MAKE",
            "LD_LIBRARY_PATH",
        ] {
            assert!(drop.contains(&k.to_string()), "should drop {k}");
        }
        // PATH keeps only the system entries.
        assert_eq!(
            overrides
                .iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.as_str()),
            Some("/usr/bin:/bin")
        );
        // Untouched, legitimate vars stay.
        for k in ["HOME", "EDITOR"] {
            assert!(!drop.contains(&k.to_string()));
            assert!(!overrides.iter().any(|(o, _)| o == k));
        }
    }

    #[test]
    fn sgr_encoding() {
        let mut b = Vec::new();
        push_wheel_report(&mut b, true, 0, 0, true);
        assert_eq!(b, b"\x1b[<64;1;1M"); // wheel up at top-left cell
        b.clear();
        push_wheel_report(&mut b, false, 4, 9, true);
        assert_eq!(b, b"\x1b[<65;5;10M"); // wheel down at col 5, row 10 (1-based)
    }

    #[test]
    fn legacy_encoding() {
        let mut b = Vec::new();
        push_wheel_report(&mut b, true, 0, 0, false);
        // ESC [ M, button 64+32=96, then (col+1)+32 and (row+1)+32.
        assert_eq!(b, &[0x1b, b'[', b'M', 96, 33, 33]);
        b.clear();
        push_wheel_report(&mut b, false, 1, 2, false);
        assert_eq!(b, &[0x1b, b'[', b'M', 97, 34, 35]);
    }
}

// These tests spawn `/bin/sh` and `/bin/cat`, so they are Unix-only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line, Point as GridPoint};
    use std::time::{Duration, Instant};

    /// End-to-end check of the backend: spawn a process, drain its PTY output
    /// through the VTE parser, and confirm the text lands in the emulator grid.
    #[test]
    fn output_lands_in_grid() {
        let spec = CommandSpec::program("/bin/sh", vec!["-c".into(), "printf 'MUXEL_OK'".into()]);
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.recv_blocking() {
                Ok(PtyChunk::Output(bytes)) => session.process_output(&bytes),
                Ok(PtyChunk::Exit(_)) => break,
                Err(_) => break,
            }
            if Instant::now() > deadline {
                break;
            }
        }

        let text = session.with_term(|term| {
            let grid = term.grid();
            let mut s = String::new();
            for row in 0..grid.screen_lines() {
                for col in 0..grid.columns() {
                    s.push(
                        grid[GridPoint {
                            line: Line(row as i32),
                            column: Column(col),
                        }]
                        .c,
                    );
                }
            }
            s
        });

        assert!(
            text.contains("MUXEL_OK"),
            "grid did not contain expected output; got: {:?}",
            text.trim()
        );
    }

    /// `visible_text()` returns the screen so status detection can scan it for
    /// markers like the Claude "esc to interrupt" working footer.
    #[test]
    fn visible_text_scans_screen_for_markers() {
        let spec = CommandSpec::program(
            "/bin/sh",
            vec!["-c".into(), "printf 'foo esc to interrupt bar'".into()],
        );
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.recv_blocking() {
                Ok(PtyChunk::Output(bytes)) => session.process_output(&bytes),
                Ok(PtyChunk::Exit(_)) => break,
                Err(_) => break,
            }
            if Instant::now() > deadline {
                break;
            }
        }
        let screen = session.visible_text();
        assert!(
            screen.contains("esc to interrupt"),
            "visible_text should expose the marker; got: {:?}",
            screen.trim()
        );
        assert!(
            !screen.contains("❯ 1."),
            "no permission prompt was rendered"
        );
    }

    #[test]
    fn write_input_reaches_child() {
        // `cat` echoes stdin back; feed it a line and confirm it shows up.
        let spec = CommandSpec::program("/bin/cat", vec![]);
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");
        session.write_input(b"hello-muxel\n");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while Instant::now() < deadline && !seen {
            match rx.recv_blocking() {
                Ok(PtyChunk::Output(bytes)) => {
                    session.process_output(&bytes);
                    let text = session.with_term(|term| {
                        let grid = term.grid();
                        let mut s = String::new();
                        for row in 0..grid.screen_lines() {
                            for col in 0..grid.columns() {
                                s.push(
                                    grid[GridPoint {
                                        line: Line(row as i32),
                                        column: Column(col),
                                    }]
                                    .c,
                                );
                            }
                        }
                        s
                    });
                    if text.contains("hello-muxel") {
                        seen = true;
                    }
                }
                Ok(PtyChunk::Exit(_)) | Err(_) => break,
            }
        }
        session.kill();
        assert!(seen, "child did not echo written input back into the grid");
    }

    /// OSC-52 copy: the base64 payload is decoded by alacritty and queued for
    /// the view to land on the system clipboard.
    #[test]
    fn osc52_store_lands_in_pending_queue() {
        use alacritty_terminal::term::ClipboardType;
        let (session, _rx) =
            TerminalSession::spawn(CommandSpec::program("/bin/cat", vec![]), 80, 24)
                .expect("spawn");
        session.process_output(b"\x1b]52;c;aGVsbG8=\x07"); // base64("hello")
        let stores = session.take_clipboard_stores();
        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].0, ClipboardType::Clipboard);
        assert_eq!(stores[0].1, "hello");
        assert!(
            session.take_clipboard_stores().is_empty(),
            "drained on take"
        );
        session.kill();
    }

    /// Collect raw PTY bytes until `needle` shows up (the reply written to the
    /// child's stdin is echoed back by the tty/cat). Non-blocking receive with a
    /// hard deadline — `cat` never exits, so a blocking recv would hang the test
    /// binary forever if the reply never arrives.
    fn wait_for_reply(rx: &async_channel::Receiver<PtyChunk>, needle: &[u8]) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen: Vec<u8> = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    seen.extend_from_slice(&bytes);
                    if seen.windows(needle.len()).any(|w| w == needle) {
                        return true;
                    }
                }
                Ok(PtyChunk::Exit(_)) => return false,
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(async_channel::TryRecvError::Closed) => return false,
            }
        }
        false
    }

    /// OSC-52 read probe: answered with a well-formed EMPTY reply — support is
    /// detectable, but the clipboard never leaks to the child. The needle is the
    /// reply's printable core: the tty echoes control chars in caret notation
    /// (`ESC` → `^[`, `BEL` → `^G`), so the raw bytes never appear verbatim.
    /// Emptiness is the listener's contract (`format("")`); what's asserted here
    /// is that a reply reaches the PTY at all.
    #[test]
    fn osc52_load_answers_empty() {
        let (session, rx) =
            TerminalSession::spawn(CommandSpec::program("/bin/cat", vec![]), 80, 24)
                .expect("spawn");
        session.process_output(b"\x1b]52;c;?\x07");
        assert!(
            wait_for_reply(&rx, b"]52;c;"),
            "empty OSC-52 reply should reach the PTY"
        );
        session.kill();
    }

    /// OSC 11 (default background) query: answered from the session palette so
    /// TUIs detect dark/light from what's actually painted.
    #[test]
    fn color_query_reports_palette_background() {
        let (session, rx) =
            TerminalSession::spawn(CommandSpec::program("/bin/cat", vec![]), 80, 24)
                .expect("spawn");
        session.set_palette(crate::colors::TerminalPalette {
            background: 0x112233,
            ..Default::default()
        });
        session.process_output(b"\x1b]11;?\x07");
        assert!(
            wait_for_reply(&rx, b"]11;rgb:1111/2222/3333"),
            "background query should answer with the set palette"
        );
        session.kill();
    }

    /// The reader thread harvests the child's exit code after EOF, so the app
    /// can tell a clean exit from a crash.
    #[test]
    fn exit_code_is_reported() {
        let (_session, rx) = TerminalSession::spawn(
            CommandSpec::program("/bin/sh", vec!["-c".into(), "exit 7".into()]),
            80,
            24,
        )
        .expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                Instant::now() < deadline,
                "no exit chunk within the deadline"
            );
            match rx.try_recv() {
                Ok(PtyChunk::Output(_)) => {}
                Ok(PtyChunk::Exit(code)) => {
                    assert_eq!(code, Some(7));
                    break;
                }
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("channel closed without an exit chunk")
                }
            }
        }
    }

    #[test]
    fn direct_child_reads_as_idle_foreground() {
        // The direct child (`cat`) is the terminal's foreground process group with
        // nothing running under it, so it reads as idle-foreground. (A shell running
        // a sub-command would put that command in a different group → false.)
        let spec = CommandSpec::program("/bin/cat", vec![]);
        let (session, _rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");
        // The kernel sets the foreground group as the child takes the pty; poll.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !session.is_idle_foreground() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            session.is_idle_foreground(),
            "the direct child should be the foreground process group"
        );
        session.kill();
    }
}
