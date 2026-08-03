//! [`TerminalView`] — a gpui entity that owns a [`TerminalSession`], drains its
//! output into the grid, renders it via [`TerminalElement`], and forwards
//! keyboard input.

use crate::colors::TerminalPalette;
use crate::element::TerminalElement;
use crate::keymap::{KeyModifiers, key_to_bytes};
use crate::profile;
use crate::session::{CommandSpec, PtyChunk, TerminalSession};
use alacritty_terminal::term::ClipboardType;
use anyhow::Context as _;
use gpui::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Stop draining after this many bytes in a single turn so one noisy terminal
/// can't starve the UI; the rest stays buffered for the next turn.
const MAX_BYTES_PER_TURN: usize = 256 * 1024;

/// Unfocused terminals still parse PTY output (status badges need a warm grid)
/// but only repaint at this rate. Multi-agent streams at 10 Hz still starved the
/// UI thread under load; 4 Hz is enough for status dots and idle panes.
const BACKGROUND_PAINT_INTERVAL: Duration = Duration::from_millis(250);

/// Focused stream output outside the recent-input window: ~30 Hz.
/// Typing-while-Claude-streams used to notify every batch (~full submit thrash).
const FOCUSED_STREAM_INTERVAL: Duration = Duration::from_millis(33);

/// Focused output shortly after user input: keep TUI feedback crisp.
const FOCUSED_INTERACTION_INTERVAL: Duration = Duration::from_millis(8);

/// Pure paint-priority policy (see `docs/terminal-paint-architecture.md`).
/// Extracted so we can unit-test without a full GPUI window.
pub(crate) fn paint_min_interval(focused: bool, interactive: bool, stop: bool) -> Duration {
    if stop {
        Duration::ZERO
    } else if !focused {
        BACKGROUND_PAINT_INTERVAL
    } else if interactive {
        FOCUSED_INTERACTION_INTERVAL
    } else {
        FOCUSED_STREAM_INTERVAL
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaintSchedule {
    Now,
    At(Instant),
    KeepPending,
}

fn next_paint_schedule(
    last_notify: Instant,
    pending: Option<Instant>,
    now: Instant,
    min_interval: Duration,
) -> PaintSchedule {
    let deadline = last_notify + min_interval;
    if now >= deadline {
        PaintSchedule::Now
    } else if pending.is_some_and(|pending| pending <= deadline) {
        PaintSchedule::KeepPending
    } else {
        PaintSchedule::At(deadline)
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TitleProvider {
    Claude,
    Codex,
    Grok,
    Other,
}

impl TitleProvider {
    fn from_program(program: &str) -> Self {
        let leaf = program
            .trim()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let stem = leaf
            .strip_suffix(".exe")
            .or_else(|| leaf.strip_suffix(".cmd"))
            .or_else(|| leaf.strip_suffix(".bat"))
            .unwrap_or(&leaf);
        match stem {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "grok" => Self::Grok,
            _ => Self::Other,
        }
    }
}

/// Parse only provider-owned, structurally constrained title state. Unknown
/// shapes are no evidence; they must never turn an arbitrary pane title into a
/// lifecycle claim.
fn title_status(
    provider: TitleProvider,
    title: Option<&str>,
    age: Option<Duration>,
) -> Option<AgentStatus> {
    let title = title?.trim_start();
    match provider {
        TitleProvider::Claude => match title.chars().next()? {
            // Claude owns this title protocol: the braille frames mean Working
            // and the star means Idle. Long-running tools can leave one frame
            // unchanged for minutes, so title age is not completion evidence.
            '⠂' | '⠐' => Some(AgentStatus::Working),
            '✳' => Some(AgentStatus::Idle),
            _ => None,
        },
        TitleProvider::Codex => {
            // Muxel forces the title to run-state + activity only, so these words
            // cannot come from a project/session name. Activity supplies a fresh
            // spinner heartbeat and the action-required phrase.
            let lower = title.to_ascii_lowercase();
            if lower.contains("action required") {
                Some(AgentStatus::Blocked)
            } else if lower
                .split(|c: char| !c.is_alphabetic())
                .any(|part| matches!(part, "starting" | "working" | "thinking"))
                && age.is_some_and(|age| age <= Duration::from_secs(5))
            {
                Some(AgentStatus::Working)
            } else if lower
                .split(|c: char| !c.is_alphabetic())
                .any(|part| part == "ready")
            {
                // Ready is authoritative until Codex publishes the next state.
                // This keeps a background server's PTY repaint from claiming the
                // agent itself is still working.
                Some(AgentStatus::Idle)
            } else {
                None
            }
        }
        TitleProvider::Grok => {
            let parts = title.split(" - ").map(str::trim).collect::<Vec<_>>();
            if title.contains("⚠ Action Required") {
                Some(AgentStatus::Blocked)
            } else if parts
                .iter()
                .any(|part| is_grok_spinner(part) || is_grok_activity(part))
            {
                // Grok removes these provider-owned items when it returns to
                // Idle. A long tool or response can leave the same title in
                // place for minutes, so age is not completion evidence.
                Some(AgentStatus::Working)
            } else if title == "grok" || parts.last() == Some(&"grok") {
                // Grok removes the spinner/activity items when AgentState returns
                // to Idle, leaving the generated session title and `grok` item.
                Some(AgentStatus::Idle)
            } else {
                None
            }
        }
        TitleProvider::Other => None,
    }
}

/// Provider-owned screen text that proves work continues even if the title has
/// already moved to its idle shape.
fn continuing_screen_status(provider: TitleProvider, screen: &str) -> Option<AgentStatus> {
    match provider {
        TitleProvider::Claude => screen
            .lines()
            .any(is_claude_background_command_row)
            .then_some(AgentStatus::Working),
        _ => None,
    }
}

fn is_claude_background_command_row(line: &str) -> bool {
    let words: Vec<_> = line.split_whitespace().collect();
    if words.len() != 11
        || words[0] != "·"
        || words[4..] != ["running", "·", "send", "a", "message", "to", "interrupt"]
    {
        return false;
    }
    let Ok(count) = words[1].parse::<usize>() else {
        return false;
    };
    count > 0 && words[2] == if count == 1 { "command" } else { "commands" } && words[3] == "still"
}

fn is_grok_spinner(part: &str) -> bool {
    matches!(part, "⠋" | "⠙" | "⠹" | "⠸" | "⠼" | "⠴" | "⠦" | "⠧")
}

fn is_grok_activity(part: &str) -> bool {
    matches!(
        part,
        "Thinking" | "Responding" | "Running tool" | "Compacting"
    ) || part.starts_with("Waiting")
        || part.starts_with("Running:")
        || part.starts_with("Retrying (")
}

fn combine_title_status(
    exited: bool,
    base: AgentStatus,
    title: Option<AgentStatus>,
) -> AgentStatus {
    if exited {
        return AgentStatus::Done;
    }
    if base == AgentStatus::Blocked {
        return AgentStatus::Blocked;
    }
    match title {
        Some(AgentStatus::Blocked) => AgentStatus::Blocked,
        Some(AgentStatus::Working) => AgentStatus::Working,
        // A bell is a stronger completion edge than an idle title.
        Some(AgentStatus::Idle) if base != AgentStatus::Done => AgentStatus::Idle,
        _ => base,
    }
}

/// Grok blinks its action-required title item off for roughly half of each
/// second while unfocused. Hold Blocked through that documented off-frame; a
/// stable Idle title clears it immediately, while approval may leave at most a
/// short tail before Working becomes visible.
fn hold_grok_blocked(
    status: Option<AgentStatus>,
    blocked_age: Option<Duration>,
) -> Option<AgentStatus> {
    match status {
        Some(AgentStatus::Working)
            if blocked_age.is_some_and(|age| age <= Duration::from_millis(750)) =>
        {
            Some(AgentStatus::Blocked)
        }
        other => other,
    }
}

/// Remove provider lifecycle decoration before a title is considered for the
/// pane's persisted automatic name. A title containing only state is not a name.
pub fn clean_agent_title(program: &str, title: &str) -> Option<String> {
    match TitleProvider::from_program(program) {
        TitleProvider::Claude => {
            let title = title.trim_start();
            let first = title.chars().next()?;
            let cleaned = if matches!(first, '⠂' | '⠐' | '✳') {
                title[first.len_utf8()..].trim_start()
            } else {
                title
            };
            (!cleaned.is_empty()).then(|| cleaned.to_string())
        }
        TitleProvider::Codex
            if title_status(TitleProvider::Codex, Some(title), Some(Duration::ZERO)).is_some() =>
        {
            None
        }
        TitleProvider::Grok => {
            let mut parts = title
                .split(" - ")
                .map(str::trim)
                .filter(|part| {
                    !part.is_empty()
                        && *part != "grok"
                        && *part != "⚠ Action Required"
                        && !is_grok_spinner(part)
                        && !is_grok_activity(part)
                })
                .peekable();
            parts.peek()?;
            Some(parts.collect::<Vec<_>>().join(" - "))
        }
        _ => Some(title.to_string()),
    }
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
    if blocked.iter().any(|m| screen.contains(m)) {
        return AgentStatus::Blocked;
    }
    // User-actionable input wins when a provider leaves its working marker on
    // screen behind an approval prompt.
    if working.iter().any(|m| screen.contains(m)) {
        return AgentStatus::Working;
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
/// again. Returns `(displayed status, new latch state)`.
/// Pure half of [`TerminalView::status`]'s done-latch, so a finished turn shows
/// Done even when the agent never rang the bell.
///
/// `can_latch` gates the mechanism to agents whose `Working` state has reliable
/// marker or semantic-title evidence. Other terminals infer Working from recent
/// output alone, where incidental repaint output would create a bogus Done.
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

fn latch_done_after_readiness(
    prev_raw: Option<AgentStatus>,
    raw: AgentStatus,
    latched: bool,
    can_latch: bool,
    armed: bool,
    submitted: bool,
) -> (AgentStatus, bool, bool) {
    let armed_before_edge = armed || submitted;
    let (status, latched) = latch_done(prev_raw, raw, latched, can_latch && armed_before_edge);
    (
        status,
        latched,
        armed_before_edge || raw == AgentStatus::Idle,
    )
}

fn can_latch_completion(has_working_markers: bool, semantic_title_seen: bool) -> bool {
    has_working_markers || semantic_title_seen
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

/// Drain whatever is already buffered on the channel into `output`, stopping
/// at an Exit event or once [`MAX_BYTES_PER_TURN`] is buffered (the rest stays
/// queued for the next drain turn).
fn coalesce_pending(
    rx: &async_channel::Receiver<PtyChunk>,
    output: &mut Vec<u8>,
    exit: &mut Option<ExitInfo>,
) {
    while let Ok(more) = rx.try_recv() {
        match more {
            PtyChunk::Output(b) => output.extend_from_slice(&b),
            PtyChunk::Exit {
                code,
                signal,
                read_error,
            } => {
                *exit = Some(ExitInfo {
                    code,
                    signal,
                    read_error,
                });
                return;
            }
        }
        if output.len() >= MAX_BYTES_PER_TURN {
            return;
        }
    }
}

pub struct TerminalView {
    instance_id: Uuid,
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
    /// Latched when the child first draws non-whitespace content. Kept here so
    /// an animated loading overlay does not rescan the terminal grid every frame.
    has_visible_content: bool,
    /// On-screen markers that classify the agent's status (per-agent).
    working_markers: Vec<String>,
    blocked_markers: Vec<String>,
    title_provider: TitleProvider,
    /// Latches `Done` from a working→finished transition so a completed turn shows
    /// Done (and notifies) even when the agent didn't ring the bell. `prev_raw` is
    /// the previous *raw* classification; the latch clears when the agent works
    /// again.
    prev_raw: std::cell::Cell<Option<AgentStatus>>,
    done_latch: std::cell::Cell<bool>,
    /// Ready/Idle must be observed once, or a turn submitted, before a
    /// Working→Idle edge can mean Done. This rejects startup spinners.
    completion_armed: std::cell::Cell<bool>,
    /// A provider-specific semantic title has been recognized at least once.
    /// Until then Codex/Grok without title support retain the marker-only
    /// fallback and may not turn incidental PTY activity into Done.
    semantic_title_seen: std::cell::Cell<bool>,
    /// Grok's action-required title item intentionally blinks when unfocused.
    /// Retain the last positive edge briefly so the sidebar does not blink too.
    grok_blocked_at: std::cell::Cell<Option<std::time::Instant>>,
    /// Last time we `cx.notify()`'d a paint from the drain loop (background throttle).
    last_paint_notify: std::cell::Cell<std::time::Instant>,
    /// A throttled batch must still paint if output stops before the next batch.
    /// The generation invalidates an older, later timer when interactive output
    /// brings the deadline forward.
    pending_paint_deadline: std::cell::Cell<Option<std::time::Instant>>,
    paint_timer_generation: std::cell::Cell<u64>,
    /// Cached agent status for the current grid and OSC-title generations.
    status_cache: std::cell::Cell<Option<(u64, u64, AgentStatus)>>,
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
    /// Spawn `spec` at `size` (`(cols, rows)`); if it can't be launched (e.g. the
    /// agent isn't installed), fall back to a shell that prints the error. `Err` only
    /// when even the fallback shell can't spawn (bogus `$SHELL` and no `/bin/bash`,
    /// fd exhaustion, …).
    ///
    /// `size` should be the grid the pane will actually render at — see
    /// [`TerminalSession::size`]. Getting it right up front is what keeps a
    /// `tmux attach` from painting its first frame at the wrong size.
    pub fn spawn(spec: CommandSpec, size: (u16, u16)) -> anyhow::Result<Self> {
        Self::spawn_with_fallback(spec, CommandSpec::shell(), size, None)
    }

    /// Spawn while attributing PTY sub-phase timings to one pane.
    pub fn spawn_for_instance(
        spec: CommandSpec,
        size: (u16, u16),
        instance_id: Uuid,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_fallback(spec, CommandSpec::shell(), size, Some(instance_id))
    }

    /// Testable inner half of [`Self::spawn`]: the fallback spec is injectable.
    fn spawn_with_fallback(
        spec: CommandSpec,
        fallback: CommandSpec,
        (cols, rows): (u16, u16),
        instance_id: Option<Uuid>,
    ) -> anyhow::Result<Self> {
        let spawn = |spec| match instance_id {
            Some(instance_id) => TerminalSession::spawn_profiled(spec, cols, rows, instance_id),
            None => TerminalSession::spawn(spec, cols, rows),
        };
        match spawn(spec.clone()) {
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
                let (session, rx) = spawn(shell.clone())
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
    /// Notify now or arm one trailing-edge notify. A leading-edge throttle alone
    /// can leave the final output batch stale forever when output stops inside
    /// the interval.
    ///
    /// Every path that calls `cx.notify()` here must also arm the Windows
    /// present pump ([`crate::present_flag`]): gpui-on-Windows can draw without
    /// presenting, so a frame we schedule but never mark stays off-screen.
    fn schedule_paint(&mut self, min_interval: Duration, cx: &mut Context<Self>) {
        let now = Instant::now();
        let deadline = match next_paint_schedule(
            self.last_paint_notify.get(),
            self.pending_paint_deadline.get(),
            now,
            min_interval,
        ) {
            PaintSchedule::Now => {
                self.pending_paint_deadline.set(None);
                self.paint_timer_generation
                    .set(self.paint_timer_generation.get().wrapping_add(1));
                self.last_paint_notify.set(now);
                cx.notify();
                profile::notify_scheduled(
                    self.instance_id,
                    profile::PaintRequest::Now,
                    min_interval,
                );
                crate::present_flag::mark_present_needed();
                return;
            }
            PaintSchedule::KeepPending => return,
            PaintSchedule::At(deadline) => deadline,
        };

        self.pending_paint_deadline.set(Some(deadline));
        let generation = self.paint_timer_generation.get().wrapping_add(1);
        self.paint_timer_generation.set(generation);
        let delay = deadline.saturating_duration_since(now);
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            cx.background_executor().timer(delay).await;
            let _ = view.update(cx, |view, cx| {
                if view.paint_timer_generation.get() != generation {
                    return;
                }
                view.pending_paint_deadline.set(None);
                view.last_paint_notify.set(Instant::now());
                cx.notify();
                profile::notify_scheduled(
                    view.instance_id,
                    profile::PaintRequest::Timer,
                    min_interval,
                );
                crate::present_flag::mark_present_needed();
            });
        })
        .detach();
    }

    /// Wrap a spawned terminal in a view and wire up its output drain.
    pub fn new(
        launch: TerminalLaunch,
        instance_id: Uuid,
        startup_started: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
        let startup_program = spec.program.clone();
        let title_provider =
            TitleProvider::from_program(spec.status_program.as_deref().unwrap_or(&startup_program));
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
                    session.paste(&input);
                    // On restore, leave the prompt typed but unsubmitted.
                    if submit {
                        timer(SUBMIT_DELAY_MS).await;
                        session.mark_turn_submitted();
                        session.write_input(b"\r");
                    }
                }
            })
            .detach();
        }

        let drain_session = session.clone();
        let drain = cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let session = drain_session;
            let mut first_output = true;
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
                coalesce_pending(&rx, &mut output, &mut exit);

                // Coalesce before taking the UI lock: bg agents always; focused
                // stream bursts too (interaction priority is decided after parsing).
                let focused_hint = session.is_focused();
                if exit.is_none() && output.len() < MAX_BYTES_PER_TURN {
                    let wait = if !focused_hint {
                        Some(BACKGROUND_PAINT_INTERVAL)
                    } else if output.len() >= 4096 {
                        Some(FOCUSED_STREAM_INTERVAL)
                    } else {
                        None
                    };
                    if let Some(d) = wait {
                        cx.background_executor().timer(d).await;
                        coalesce_pending(&rx, &mut output, &mut exit);
                    }
                }

                let batch_len = output.len();
                if first_output && batch_len > 0 {
                    profile::startup_event(
                        instance_id,
                        &startup_program,
                        "first-output",
                        startup_started.elapsed(),
                        batch_len,
                    );
                    first_output = false;
                }
                let stop = view
                    .update(cx, |view, cx| {
                        let focused = view.session.is_focused();
                        if !output.is_empty() {
                            let t0 = Instant::now();
                            view.session.process_output(&output);
                            if !view.has_visible_content
                                && !view.session.visible_text().trim().is_empty()
                            {
                                view.has_visible_content = true;
                                profile::startup_event(
                                    instance_id,
                                    &startup_program,
                                    "first-screen",
                                    startup_started.elapsed(),
                                    batch_len,
                                );
                            }
                            profile::process_output(instance_id, batch_len, t0.elapsed(), focused);
                            if focused && profile::is_enabled() {
                                let (col, row, text) = view.session.cursor_probe();
                                profile::screen_probe_update(col, row, text);
                            }
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
                        // Priority scheduler (see docs/terminal-paint-architecture.md):
                        // Recent input ≫ stream; every throttled batch gets one
                        // trailing-edge notify if no later output arrives.
                        let interactive = view.session.is_interactive();
                        let min_interval = paint_min_interval(focused, interactive, stop);
                        view.schedule_paint(min_interval, cx);
                        stop
                    })
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
        });

        Self {
            instance_id,
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
            has_visible_content: false,
            working_markers,
            blocked_markers,
            title_provider,
            prev_raw: std::cell::Cell::new(None),
            done_latch: std::cell::Cell::new(false),
            completion_armed: std::cell::Cell::new(false),
            semantic_title_seen: std::cell::Cell::new(false),
            grok_blocked_at: std::cell::Cell::new(None),
            last_paint_notify: std::cell::Cell::new(std::time::Instant::now()),
            pending_paint_deadline: std::cell::Cell::new(None),
            paint_timer_generation: std::cell::Cell::new(0),
            status_cache: std::cell::Cell::new(None),
            _drain: drain,
        }
    }

    pub fn session(&self) -> &Arc<TerminalSession> {
        &self.session
    }

    pub fn exited(&self) -> bool {
        self.exited
    }

    /// Whether the child has drawn anything a person can see. Agent TUIs often
    /// emit control sequences immediately, then spend seconds starting before
    /// their first text; those bytes must not dismiss loading UI.
    pub fn has_visible_content(&self) -> bool {
        self.has_visible_content
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
        // Cache against content gen so sidebar/tick don't re-scan the grid every
        // frame while the agent is idle. Bell and idle-time still force a recheck.
        let content_gen = self.session.content_generation();
        let (title_gen, title_text) = self.session.title_snapshot();
        let bell = self.session.has_bell();
        if !bell
            && let Some((cached_gen, cached_title_gen, cached)) = self.status_cache.get()
            && cached_gen == content_gen
            && cached_title_gen == title_gen
            && !self.exited
        {
            // Idle duration can advance Working→Idle without a content gen bump
            // (no new output). Recompute when the activity heuristic might flip.
            if !matches!(cached, AgentStatus::Working) {
                return cached;
            }
        }

        let mut title = title_status(
            self.title_provider,
            title_text.as_deref(),
            self.session.title_age(),
        );
        if title.is_some() {
            self.semantic_title_seen.set(true);
        }
        // Once a known provider has proved its title protocol, that protocol owns
        // Working/Idle. Keep screen markers only as a compatibility fallback for
        // older/custom clients; blocked prompts remain independent evidence.
        let title_protocol_live = self.semantic_title_seen.get();
        let working_markers = if title_protocol_live {
            &[][..]
        } else {
            self.working_markers.as_slice()
        };
        // Claude may declare its title idle while a background command remains
        // live. Scan its visible grid for that provider-owned continuation row.
        let needs_continuation_scan = self.title_provider == TitleProvider::Claude
            && matches!(title, None | Some(AgentStatus::Idle));
        let screen = if working_markers.is_empty()
            && self.blocked_markers.is_empty()
            && !needs_continuation_scan
        {
            String::new()
        } else {
            self.session.visible_text()
        };
        let base = classify(
            self.exited,
            &screen,
            working_markers,
            &self.blocked_markers,
            bell,
            self.session.idle_for(),
        );
        if self.title_provider == TitleProvider::Grok {
            if title == Some(AgentStatus::Blocked) {
                self.grok_blocked_at.set(Some(std::time::Instant::now()));
            } else {
                let blocked_age = self.grok_blocked_at.get().map(|at| at.elapsed());
                title = hold_grok_blocked(title, blocked_age);
                if matches!(title, Some(AgentStatus::Idle))
                    || blocked_age.is_some_and(|age| age > Duration::from_millis(750))
                {
                    self.grok_blocked_at.set(None);
                }
            }
        }
        if title != Some(AgentStatus::Blocked)
            && let Some(continuing) = continuing_screen_status(self.title_provider, &screen)
        {
            title = Some(continuing);
        }
        let raw = combine_title_status(self.exited, base, title);
        // Marker-less providers may latch only after proving that their semantic
        // title protocol is live. Older/custom CLIs otherwise infer Working from
        // typing echo and turn two seconds of quiet into a false completion.
        let can_latch = can_latch_completion(
            !self.working_markers.is_empty(),
            self.semantic_title_seen.get(),
        );
        let (status, latch, armed) = latch_done_after_readiness(
            self.prev_raw.replace(Some(raw)),
            raw,
            self.done_latch.get(),
            can_latch,
            self.completion_armed.get(),
            self.session.has_submitted_turn(),
        );
        self.done_latch.set(latch);
        self.completion_armed.set(armed);
        self.status_cache
            .set(Some((content_gen, title_gen, status)));
        status
    }

    /// Whether `needle` appears in the current visible grid — used by the app to
    /// spot an agent's "session not found" error for resume recovery.
    pub fn screen_has(&self, needle: &str) -> bool {
        self.session.visible_text().contains(needle)
    }

    /// The current visible grid as text (rows joined by newlines). Used by the
    /// app's auto-continue to scan an agent's todo list.
    pub fn visible_text(&self) -> String {
        self.session.visible_text()
    }

    pub fn title(&self) -> Option<String> {
        self.session.title()
    }

    pub fn clear_title(&self) {
        self.session.clear_title();
    }

    pub fn session_id_hint(&self) -> Option<String> {
        self.session.session_id_hint()
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.session.child_pid()
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
        let t0 = Instant::now();
        let held = event.is_held;
        let m = &event.keystroke.modifiers;

        // Copy / paste. On macOS the platform shortcut is ⌘C / ⌘V; elsewhere
        // ctrl-shift-c / ctrl-shift-v (plain ctrl-c must stay SIGINT).
        //
        // Plain Ctrl+V is host-side smart paste (Windows Terminal / VS Code
        // style): text and file paths are injected into the PTY; an image
        // forwards raw 0x16 so agents that read the OS clipboard (Grok) can
        // attach it. Leaving Ctrl+V as bare 0x16 broke text paste in Claude —
        // it does not host-paste on 0x16. Claude's image chord is Alt+V
        // (ESC v via the keymap); we never intercept that.
        // Classic Insert: Ctrl+Insert = copy, Shift+Insert = paste.
        //
        // Always `stop_propagation` when we consume a key. On Windows, Alt+letter
        // is `WM_SYSKEYDOWN`; if gpui returns unhandled, DefWindowProc rings the
        // system ding (Windows Terminal never does — it eats the message).
        let key = event.keystroke.key.as_str();
        if key == "insert" || key == "ins" {
            if m.control && !m.alt && !m.shift {
                if let Some(text) = self.session.selection_to_string()
                    && !text.is_empty()
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                cx.stop_propagation();
                return;
            }
            if m.shift && !m.control && !m.alt {
                paste_clipboard_into_session(&self.session, cx);
                self.session.clear_selection();
                cx.notify();
                cx.stop_propagation();
                return;
            }
        }
        // Plain Ctrl+V — smart paste (see above). Must run before key_to_bytes
        // would turn it into C0 0x16 unconditionally.
        if key == "v" && m.control && !m.shift && !m.alt && !m.platform {
            paste_clipboard_into_session(&self.session, cx);
            self.session.clear_selection();
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let copy_paste = (m.control && m.shift && !m.alt)
            || (cfg!(target_os = "macos") && m.platform && !m.control && !m.shift && !m.alt);
        if copy_paste {
            match key {
                "c" => {
                    if let Some(text) = self.session.selection_to_string()
                        && !text.is_empty()
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    cx.stop_propagation();
                    return;
                }
                "v" => {
                    paste_clipboard_into_session(&self.session, cx);
                    self.session.clear_selection();
                    cx.notify();
                    cx.stop_propagation();
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
            if event.keystroke.key.eq_ignore_ascii_case("enter") {
                self.session.mark_turn_submitted();
            }
            self.session.write_input(&bytes);
            if cleared {
                cx.notify();
            }
            // Key path: gpui may sync-draw without presenting — arm the pump.
            crate::present_flag::mark_present_needed();
            cx.stop_propagation();
            profile::key_handled(self.instance_id, held, t0.elapsed());
        }
    }
}

/// Paste from the system clipboard into the PTY.
/// Image → forward Ctrl+V (0x16) so the agent reads the OS clipboard.
/// File paths → shell-quoted paths. Text → bracketed paste.
pub fn paste_clipboard_into_session(session: &TerminalSession, cx: &App) {
    let Some(item) = cx.read_from_clipboard() else {
        return;
    };
    for entry in item.entries() {
        match entry {
            ClipboardEntry::Image(image) if !image.bytes.is_empty() => {
                session.write_input(&[0x16]);
                return;
            }
            ClipboardEntry::ExternalPaths(paths) => {
                let paths: Vec<_> = paths.paths().to_vec();
                if !paths.is_empty() {
                    session.paste_paths(&paths);
                    return;
                }
            }
            _ => {}
        }
    }
    if let Some(text) = item.text() {
        session.paste(&text);
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
            // OS file drops (Explorer → pane) arrive as an internal gpui drag of
            // ExternalPaths, not as FileDropEvent listeners. Same path Zed uses.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.session.paste_paths(paths.paths());
                this.session.clear_selection();
                cx.notify();
            }))
            .size_full()
            // Fill the inset margin with the terminal background and inset the
            // element so the grid (sized from the inner area) never butts against
            // the pane edge — a too-wide TUI truncates inside the margin.
            .bg(self.palette.background_hsla())
            .p(TERM_INSET)
            .child(TerminalElement::new(
                self.instance_id,
                self.session.clone(),
                cx.entity_id(),
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
        let launch = TerminalLaunch::spawn(
            CommandSpec::program("/definitely/not/here-muxel", vec![]),
            (80, 24),
        )
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
        let result = TerminalLaunch::spawn_with_fallback(bogus.clone(), bogus, (80, 24), None);
        assert!(result.is_err(), "total failure must surface as Err");
    }

    #[test]
    fn good_program_has_no_launch_error() {
        let launch = TerminalLaunch::spawn(CommandSpec::program("/bin/cat", vec![]), (80, 24))
            .expect("spawn cat");
        assert!(launch.launch_error().is_none());
        launch.session.kill();
    }
}

#[cfg(test)]
mod tests {
    // Import specifically (not `super::*`) so `#[test]` resolves to the built-in
    // macro, not gpui's glob-imported `test` attribute.
    use super::{
        AgentStatus, BACKGROUND_PAINT_INTERVAL, FOCUSED_INTERACTION_INTERVAL,
        FOCUSED_STREAM_INTERVAL, PaintSchedule, TerminalMouseMode, TitleProvider,
        can_latch_completion, classify, clean_agent_title, combine_title_status,
        continuing_screen_status, hold_grok_blocked, latch_done, latch_done_after_readiness,
        next_paint_schedule, paint_min_interval, title_status,
    };
    use std::time::Duration;

    fn m(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn markerless_provider_must_prove_semantic_titles_before_latching() {
        assert!(!can_latch_completion(false, false));
        assert!(can_latch_completion(false, true));
        assert!(can_latch_completion(true, false));
    }

    #[test]
    fn claude_title_frames_remain_working_until_explicit_idle() {
        assert_eq!(
            title_status(
                TitleProvider::Claude,
                Some("⠂ Review changes"),
                Some(Duration::from_millis(200))
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            title_status(
                TitleProvider::Claude,
                Some("⠂ Review changes"),
                Some(Duration::from_secs(4))
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            title_status(
                TitleProvider::Claude,
                Some("✳ Review changes"),
                Some(Duration::from_secs(30))
            ),
            Some(AgentStatus::Idle)
        );
    }

    #[test]
    fn codex_forced_title_contract_separates_agent_from_background_output() {
        assert_eq!(
            title_status(
                TitleProvider::Codex,
                Some("Ready ·"),
                Some(Duration::from_secs(20))
            ),
            Some(AgentStatus::Idle)
        );
        assert_eq!(
            combine_title_status(false, AgentStatus::Working, Some(AgentStatus::Idle)),
            AgentStatus::Idle
        );
        assert_eq!(
            title_status(
                TitleProvider::Codex,
                Some("Starting ⠦"),
                Some(Duration::from_millis(500))
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            title_status(
                TitleProvider::Codex,
                Some("Working · ⠋"),
                Some(Duration::from_millis(500))
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            title_status(
                TitleProvider::Codex,
                Some("Ready · action required"),
                Some(Duration::from_secs(30))
            ),
            Some(AgentStatus::Blocked)
        );
    }

    #[test]
    fn grok_title_contract_has_working_blocked_and_idle_edges() {
        assert_eq!(
            title_status(
                TitleProvider::Grok,
                Some("⠦ - Responding - Review title - grok"),
                Some(Duration::from_secs(30))
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            title_status(
                TitleProvider::Grok,
                Some("⠦ - Waiting for response… - Review title - grok"),
                Some(Duration::from_millis(300))
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            title_status(
                TitleProvider::Grok,
                Some("⚠ Action Required - ⠋ - Running tool - Review title - grok"),
                Some(Duration::from_millis(300))
            ),
            Some(AgentStatus::Blocked)
        );
        assert_eq!(
            title_status(
                TitleProvider::Grok,
                Some("Review title - grok"),
                Some(Duration::from_secs(30))
            ),
            Some(AgentStatus::Idle)
        );
        assert_eq!(
            title_status(
                TitleProvider::Grok,
                Some("Custom title with no provider state"),
                Some(Duration::ZERO)
            ),
            None
        );
    }

    #[test]
    fn claude_background_command_overrides_idle_title() {
        assert_eq!(
            continuing_screen_status(
                TitleProvider::Claude,
                "· 1 command still running · send a message to interrupt"
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            continuing_screen_status(TitleProvider::Claude, "Ready for another prompt"),
            None
        );
        assert_eq!(
            continuing_screen_status(
                TitleProvider::Claude,
                "The log says: 1 command still running; investigate it."
            ),
            None
        );
        assert_eq!(
            continuing_screen_status(
                TitleProvider::Claude,
                "quoted: · 1 command still running · send a message to interrupt later"
            ),
            None
        );
        assert_eq!(
            continuing_screen_status(
                TitleProvider::Claude,
                "  · 2 commands still running · send a message to interrupt  "
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            continuing_screen_status(
                TitleProvider::Grok,
                "· 1 command still running · send a message to interrupt"
            ),
            None
        );
    }

    #[test]
    fn grok_action_required_blink_does_not_flicker_status() {
        assert_eq!(
            hold_grok_blocked(Some(AgentStatus::Working), Some(Duration::from_millis(500))),
            Some(AgentStatus::Blocked)
        );
        assert_eq!(
            hold_grok_blocked(Some(AgentStatus::Working), Some(Duration::from_millis(751))),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            hold_grok_blocked(Some(AgentStatus::Idle), Some(Duration::from_millis(100))),
            Some(AgentStatus::Idle)
        );
    }

    #[test]
    fn traced_title_edges_override_pty_noise_and_latch_done() {
        let grok_working = title_status(
            TitleProvider::Grok,
            Some("⠹ - Responding - Review title - grok"),
            Some(Duration::from_millis(300)),
        );
        let raw_working = combine_title_status(false, AgentStatus::Idle, grok_working);
        assert_eq!(raw_working, AgentStatus::Working);

        let grok_idle = title_status(
            TitleProvider::Grok,
            Some("Review title - grok"),
            Some(Duration::from_millis(300)),
        );
        let raw_idle = combine_title_status(
            false,
            AgentStatus::Working, // incidental PTY redraw/background output
            grok_idle,
        );
        assert_eq!(raw_idle, AgentStatus::Idle);
        assert_eq!(
            latch_done(Some(AgentStatus::Working), raw_idle, false, true),
            (AgentStatus::Done, true)
        );

        // Typing while an already-idle title is present cannot manufacture a
        // Working→Done edge from the PTY activity heuristic.
        let typed = combine_title_status(false, AgentStatus::Working, grok_idle);
        assert_eq!(
            latch_done(Some(AgentStatus::Idle), typed, false, true),
            (AgentStatus::Idle, false)
        );
    }

    #[test]
    fn foreign_titles_never_become_lifecycle_state() {
        assert_eq!(
            title_status(TitleProvider::Other, Some("Thinking"), Some(Duration::ZERO)),
            None
        );
    }

    #[test]
    fn provider_detection_requires_an_exact_executable_name() {
        assert_eq!(
            TitleProvider::from_program("C:\\tools\\codex.cmd"),
            TitleProvider::Codex
        );
        assert_eq!(
            TitleProvider::from_program("claude.exe"),
            TitleProvider::Claude
        );
        assert_eq!(TitleProvider::from_program("grok"), TitleProvider::Grok);
        assert_eq!(
            TitleProvider::from_program("codex-title-proxy.exe"),
            TitleProvider::Other
        );
    }

    #[test]
    fn lifecycle_decoration_does_not_leak_into_pane_names() {
        assert_eq!(
            clean_agent_title("claude", "✳ Review changes").as_deref(),
            Some("Review changes")
        );
        assert_eq!(clean_agent_title("codex", "Ready · ⠋"), None);
        assert_eq!(
            clean_agent_title(
                "grok",
                "⠦ - Waiting for response… - User requests exact OK reply - grok"
            )
            .as_deref(),
            Some("User requests exact OK reply")
        );
        assert_eq!(clean_agent_title("grok", "grok"), None);
        assert_eq!(
            clean_agent_title("pwsh", "D:\\dev\\muxel").as_deref(),
            Some("D:\\dev\\muxel")
        );
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
        // Working marker beats a stale bell when no input request is present.
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
        // A permission prompt is actionable even if the provider leaves its
        // ordinary working footer visible behind the modal.
        assert_eq!(
            classify(
                false,
                "esc to interrupt\nDo you want to proceed?",
                &working,
                &blocked,
                false,
                busy
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

    #[test]
    fn startup_working_to_idle_arms_without_claiming_done() {
        use AgentStatus::{Done, Idle, Working};
        let (status, latch, armed) =
            latch_done_after_readiness(None, Working, false, true, false, false);
        assert_eq!((status, latch, armed), (Working, false, false));
        let (status, latch, armed) =
            latch_done_after_readiness(Some(Working), Idle, latch, true, armed, false);
        assert_eq!((status, latch, armed), (Idle, false, true));
        let (_, latch, armed) =
            latch_done_after_readiness(Some(Idle), Working, latch, true, armed, false);
        assert_eq!(
            latch_done_after_readiness(Some(Working), Idle, latch, true, armed, false),
            (Done, true, true)
        );
    }

    #[test]
    fn immediate_submission_arms_a_turn_during_startup() {
        use AgentStatus::{Done, Idle, Working};
        let (_, latch, armed) = latch_done_after_readiness(None, Working, false, true, false, true);
        assert_eq!(
            latch_done_after_readiness(Some(Working), Idle, latch, true, armed, true),
            (Done, true, true)
        );
    }

    #[test]
    fn paint_priority_interaction_beats_stream() {
        assert_eq!(
            paint_min_interval(true, true, false),
            FOCUSED_INTERACTION_INTERVAL
        );
        assert_eq!(
            paint_min_interval(true, false, false),
            FOCUSED_STREAM_INTERVAL
        );
        assert_eq!(
            paint_min_interval(false, false, false),
            BACKGROUND_PAINT_INTERVAL
        );
        assert_eq!(
            paint_min_interval(false, true, false),
            BACKGROUND_PAINT_INTERVAL
        );
        assert_eq!(paint_min_interval(true, false, true), Duration::ZERO);
        assert!(FOCUSED_INTERACTION_INTERVAL < FOCUSED_STREAM_INTERVAL);
        assert!(FOCUSED_STREAM_INTERVAL < BACKGROUND_PAINT_INTERVAL);
    }

    #[test]
    fn throttled_paint_gets_one_trailing_deadline() {
        let last = std::time::Instant::now();
        let now = last + Duration::from_millis(10);
        let deadline = last + FOCUSED_STREAM_INTERVAL;
        assert_eq!(
            next_paint_schedule(last, None, now, FOCUSED_STREAM_INTERVAL),
            PaintSchedule::At(deadline)
        );
        assert_eq!(
            next_paint_schedule(
                last,
                Some(deadline),
                now + Duration::from_millis(1),
                FOCUSED_STREAM_INTERVAL,
            ),
            PaintSchedule::KeepPending
        );
        assert_eq!(
            next_paint_schedule(last, Some(deadline), deadline, FOCUSED_STREAM_INTERVAL),
            PaintSchedule::Now
        );
    }
}
