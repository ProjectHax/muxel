//! [`TerminalView`] — a gpui entity that owns a [`TerminalSession`], drains its
//! output into the grid, renders it via [`TerminalElement`], and forwards
//! keyboard input.

use crate::colors::TerminalPalette;
use crate::element::TerminalElement;
use crate::keymap::{KeyModifiers, key_to_bytes};
use crate::profile;
use crate::program_status::{ProgramState, ProgramStatusKind, ProgramStatusSnapshot, RootStatus};
use crate::session::{
    CommandSpec, ProfiledPtyChunk, ProfiledPtyReceiver, PtyOutput, TerminalSession,
};
use alacritty_terminal::term::ClipboardType;
use anyhow::Context as _;
use gpui::*;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
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

/// How long a paint waits when the batch just *hid* the cursor.
///
/// A TUI hides the cursor to redraw and shows it again immediately after, and
/// `coalesce_pending` only merges chunks that have already arrived — so when that
/// pair straddles two PTY reads, the paint between them draws a frame with no
/// cursor. Through tmux, or on a machine busy enough to split the reads, that
/// happens often enough to look like the cursor is blinking at random.
///
/// Waiting briefly lets the matching show arrive and coalesce, so the missing
/// cursor is never drawn at all. Nothing is faked: if the program really meant to
/// hide the cursor, no show arrives and the deferred paint hides it, only this
/// much later. The cost is bounded to batches that end with a *newly* hidden
/// cursor, which are mid-redraw frames whose content the next batch supersedes
/// anyway — a program that keeps its cursor hidden never pays it.
const CURSOR_HIDE_HOLD: Duration = Duration::from_millis(80);

/// Grok redraws its foreground background-work summary. Keep one exact positive
/// observation through a brief missing frame so a repaint cannot forge a
/// Working→Done→Working lifecycle and duplicate completion notifications.
const GROK_SCREEN_WORKING_HOLD: Duration = Duration::from_secs(2);

/// Visible lifecycle parsing is independent from paint cadence, but it still
/// must not allocate a full grid string for every PTY read under stream load.
const STATUS_SCREEN_SCAN_INTERVAL: Duration = Duration::from_millis(50);
const SILENT_SWITCH_SETTLE: Duration = Duration::from_secs(5);

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

/// `defer` holds the paint off for at least that long from *now*, which the
/// throttle alone cannot express: its interval is measured from the last paint, so
/// after a quiet gap `now` is already past the deadline and the batch paints
/// immediately however large the interval is. That is exactly the case a
/// cursor-hide has to delay.
fn next_paint_schedule(
    last_notify: Instant,
    pending: Option<Instant>,
    now: Instant,
    min_interval: Duration,
    defer: Duration,
) -> PaintSchedule {
    let deadline = (last_notify + min_interval).max(now + defer);
    if now >= deadline {
        PaintSchedule::Now
    } else if pending.is_some_and(|pending| pending <= deadline) {
        PaintSchedule::KeepPending
    } else {
        PaintSchedule::At(deadline)
    }
}

/// The `(min_interval, defer)` an output batch asks [`next_paint_schedule`] for.
/// A runtime status change drops the throttle so it paints ahead of it, but
/// waits out a cursor-hide hold: this batch's own, or (`held_for`) an earlier
/// batch's still pending while the cursor stays hidden. The facts are already
/// current for readers (`ctl`, the sidebar's next read), and painting now would
/// draw the cursorless mid-redraw frame that [`CURSOR_HIDE_HOLD`] exists to skip.
fn paint_request(
    runtime_changed: bool,
    throttle: Duration,
    batch_hold: Duration,
    held_for: Duration,
) -> (Duration, Duration) {
    if runtime_changed {
        (Duration::ZERO, batch_hold.max(held_for))
    } else {
        (throttle, batch_hold)
    }
}

/// The cursor-hide hold left armed once a paint is scheduled: the deadline of a
/// pending paint that a hold set, or `None` when no such paint is pending. A
/// later batch that replaces the held paint ends the hold, as does any paint.
fn hold_after(schedule: PaintSchedule, defer: Duration, held: Option<Instant>) -> Option<Instant> {
    match schedule {
        PaintSchedule::Now => None,
        PaintSchedule::At(deadline) => (!defer.is_zero()).then_some(deadline),
        PaintSchedule::KeepPending => held,
    }
}

/// How long a status repaint found off the output path (the runtime deadline
/// timer, a runtime event) must wait: until the held paint, which then draws
/// the change, or not at all.
fn status_paint_defer(held: Option<Instant>, now: Instant) -> Duration {
    held.map_or(Duration::ZERO, |until| until.saturating_duration_since(now))
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

/// Open a terminal link without moving focus away from the terminal.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = terminal, no_json)]
pub struct OpenLinkBackground(pub String);

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

/// Strongest fact behind the current lifecycle badge. Program reports supply
/// provider-owned state; fallback sources cover only visible work and activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalStatusReason {
    ProcessExit,
    /// Current root OSC 7501 report. The optional kind belongs to Blocked.
    ProgramStatus(Option<ProgramStatusKind>),
    /// OSC 7501 `error` maps to the existing Done badge and control state.
    ProgramStatusError,
    BlockedMarker,
    WorkingMarker,
    ProviderTitle,
    ProviderScreen,
    Bell,
    /// A provider's own lifecycle hook (Muse's root-session hooks).
    ProviderHook,
    RecentOutput,
    Quiet,
}

/// Scope of the evidence used by the runtime reducer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalStatusCoverage {
    /// A current root OSC 7501 report supplies provider-owned lifecycle state.
    ProgramStatus,
    /// Only PTY activity/bell/exit is available; quiet does not prove no work.
    ActivityOnly,
    /// Configured markers add visible-grid semantics, but not hidden-work proof.
    VisibleMarkers,
    /// A provider-owned title/screen contract is active, still visible-only.
    ProviderVisible,
}

/// Immutable per-launch facts. Reads never scan the grid or advance lifecycle
/// state; output/input/resize/deadline events reconcile the owning view first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalRuntimeSnapshot {
    pub status: AgentStatus,
    pub attributable: bool,
    pub reason: TerminalStatusReason,
    pub coverage: TerminalStatusCoverage,
    pub launch_id: Uuid,
    pub binding_rev: u64,
    pub snapshot_seq: u64,
    pub input_rev: u64,
    pub draft_latched: bool,
    pub completion_ready: bool,
    /// New provider-owned completions in this launch and binding. Consumers
    /// compare this value even when two turns end at the same Done badge.
    pub completion_revision: u64,
}

impl TerminalRuntimeSnapshot {
    /// A switch invalidates the cached facts before its queued reconciliation.
    pub fn is_attributable_to(&self, binding_rev: u64) -> bool {
        self.attributable && self.binding_rev == binding_rev
    }
}

struct RuntimeStatusState {
    status: AgentStatus,
    reason: TerminalStatusReason,
    /// See [`status_is_attributable`].
    attributable: bool,
    /// See [`screen_hold_after`].
    screen_held: bool,
    coverage: TerminalStatusCoverage,
    fallback_coverage: TerminalStatusCoverage,
    program_status_revision: u64,
    program_status_active: bool,
    program_explicit_completions: u64,
    program_idle_completions: u64,
    completion_revision: u64,
    codex_idle_done: bool,
    snapshot_seq: u64,
    binding_rev: u64,
    /// Last title edge read from the session; `None` replays the current
    /// binding's edges from its start.
    title_cursor: Option<u64>,
    content_generation: u64,
    last_screen_capture: Option<Instant>,
    working_marker: bool,
    blocked_marker: bool,
    provider_screen: Option<AgentStatus>,
    prev_raw: Option<AgentStatus>,
    done_latch: bool,
    completion_armed: bool,
    provider_settled: bool,
    completion_ready: bool,
    semantic_title_status: Option<AgentStatus>,
    grok_blocked_at: Option<Instant>,
    grok_screen_working_at: Option<Instant>,
    /// Muse titles name the pane's working directory; a hook can report a new one.
    muse_cwd_label: Option<String>,
    /// Latest state reported by Muse's root-session hooks.
    muse_hook_state: Option<muxel_core::muse::MuseState>,
    deadline: Option<Instant>,
    /// Independent of the evidence window, which can settle before an old
    /// Working title has had enough quiet time to retire.
    silent_switch_until: Option<Instant>,
    timer_generation: u64,
}

impl RuntimeStatusState {
    fn new(coverage: TerminalStatusCoverage) -> Self {
        Self {
            status: AgentStatus::Idle,
            reason: TerminalStatusReason::Quiet,
            attributable: true,
            screen_held: false,
            coverage,
            fallback_coverage: coverage,
            program_status_revision: 0,
            program_status_active: false,
            program_explicit_completions: 0,
            program_idle_completions: 0,
            completion_revision: 0,
            codex_idle_done: false,
            snapshot_seq: 0,
            binding_rev: 0,
            title_cursor: None,
            content_generation: 0,
            last_screen_capture: None,
            working_marker: false,
            blocked_marker: false,
            provider_screen: None,
            prev_raw: None,
            done_latch: false,
            completion_armed: false,
            provider_settled: false,
            completion_ready: false,
            semantic_title_status: None,
            grok_blocked_at: None,
            grok_screen_working_at: None,
            muse_cwd_label: None,
            muse_hook_state: None,
            deadline: None,
            silent_switch_until: None,
            timer_generation: 0,
        }
    }

    fn snapshot(&self, session: &TerminalSession) -> TerminalRuntimeSnapshot {
        let (input_rev, draft_latched) = session.input_snapshot();
        let current_binding = self.binding_rev == session.binding_revision();
        TerminalRuntimeSnapshot {
            status: self.status,
            attributable: self.attributable && current_binding,
            reason: self.reason,
            coverage: self.coverage,
            launch_id: session.id,
            binding_rev: self.binding_rev,
            snapshot_seq: self.snapshot_seq,
            input_rev,
            draft_latched,
            completion_ready: self.completion_ready && current_binding,
            completion_revision: if current_binding {
                self.completion_revision
            } else {
                0
            },
        }
    }

    /// Reconcile parser evidence and visible state; the view schedules the returned deadline.
    fn reconcile(
        &mut self,
        session: &TerminalSession,
        title_provider: TitleProvider,
        working_markers: &[String],
        blocked_markers: &[String],
        exited: bool,
        now: Instant,
    ) -> (bool, Option<Instant>) {
        let snapshot = session.program_status_snapshot();
        // Reading PSP may flush an expired synchronized update and deliver
        // its deferred UUID title through the #91 listener, so it is read on
        // every pane.
        let program_status = if title_provider.reports_program_status() {
            snapshot
        } else {
            ProgramStatusSnapshot::default()
        };
        let binding_rev = session.binding_revision();
        let binding_changed = binding_rev != self.binding_rev;
        let program_root = (!exited).then_some(program_status.root.as_ref()).flatten();
        let program_status_cleared = program_status.clear_revision > self.program_status_revision;
        let program_status_changed_source =
            program_root.is_some() != self.program_status_active || program_status_cleared;
        self.program_status_revision = program_status.revision;
        self.program_status_active = program_root.is_some();
        if binding_changed {
            // A new provider conversation cannot inherit lifecycle evidence or
            // visible facts from the retired binding. The current generation is
            // recaptured below; the current semantic title is handled separately.
            self.semantic_title_status = None;
            self.prev_raw = None;
            self.done_latch = false;
            self.completion_armed = false;
            self.provider_settled = false;
            self.grok_blocked_at = None;
            self.grok_screen_working_at = None;
            self.last_screen_capture = None;
            self.working_marker = false;
            self.blocked_marker = false;
            self.provider_screen = None;
            self.program_explicit_completions = 0;
            self.program_idle_completions = 0;
            self.completion_revision = 0;
            self.codex_idle_done = false;
            // Read the new binding's title edges from its start: from the
            // title that began it, or after everything recorded before a
            // switch reported out of band (all of it ambiguous).
            self.title_cursor = None;
            self.silent_switch_until = None;
            // The retired conversation's bell was cleared where the binding
            // advanced (listener title bump or `provider_switched`), in order
            // with any bell the new binding has rung since; one read inside a
            // switch window never rang.
        }
        if program_status_changed_source {
            // A fallback title/screen Working edge cannot complete a new PSP
            // Idle report, and a cleared PSP edge cannot complete fallback Idle.
            self.prev_raw = None;
            self.done_latch = false;
            if program_status_cleared {
                self.codex_idle_done = false;
            }
        }

        // Evidence is timed on the wall clock. Read it at the time `now` stands
        // for, so a deadline the caller's clock has reached has passed there too.
        let wall = wall_time_at(now);
        let evidence = session.runtime_evidence_at(self.title_cursor, wall);
        self.title_cursor = Some(evidence.cursor);
        // An uncertain level state settles at a deadline of its own, in case
        // no fresh evidence ever comes; just after it, so it has passed.
        let settle_deadline = evidence
            .settles_at
            .map(|at| now + at.duration_since(wall).unwrap_or_default() + Duration::from_millis(1));
        let (title_events, title_overflowed) = (evidence.titles, evidence.overflowed);
        let (_, title_text) = session.title_snapshot();
        let muse_cwd_label = self.muse_cwd_label.as_deref();
        let observed_title = if title_provider == TitleProvider::Muse {
            muse_title_status(title_text.as_deref(), muse_cwd_label)
        } else {
            title_status(title_provider, title_text.as_deref(), session.title_age())
        };
        // Only a submission in the current binding arms its completion: the
        // Enter that picked a conversation in `/resume` ran no turn in it.
        let submitted_turn = evidence.submitted;
        let settling = evidence.settling;
        if binding_changed {
            // A switch reported out of band starts the screen hold; the
            // capture below, and later ones, decide when it ends.
            self.screen_held = evidence.windowed;
            if evidence.windowed {
                self.silent_switch_until = Some(now + SILENT_SWITCH_SETTLE);
            }
        }
        if submitted_turn || evidence.title_fresh {
            // New work and a title actually read after the switch window own
            // their state, even if they subsequently remain quiet.
            self.silent_switch_until = None;
        }
        if title_overflowed {
            // Missing an ordered title edge invalidates this source. Rebuild from
            // the retained final title without manufacturing a completion edge.
            self.semantic_title_status = None;
            self.prev_raw = None;
        }

        let mut working_to_nonworking = false;
        if !title_overflowed {
            let statuses = title_status_trace(title_provider, muse_cwd_label, &title_events);
            working_to_nonworking = has_working_to_nonworking_edge(&statuses);
            for status in statuses {
                self.provider_settled = provider_settled_after_title(
                    self.provider_settled,
                    Some(status),
                    submitted_turn,
                );
                self.semantic_title_status = Some(status);
            }
        }
        let fresh_title = observed_title.filter(|_| !settling);
        self.provider_settled =
            provider_settled_after_title(self.provider_settled, fresh_title, submitted_turn);
        if let Some(status) = fresh_title {
            self.semantic_title_status = Some(status);
        }
        if working_to_nonworking {
            // The parser may receive several OSC title frames in one coalesced
            // PTY batch. Retain the semantic Working edge so the final Idle can
            // complete an armed turn instead of disappearing into the batch.
            self.prev_raw = Some(AgentStatus::Working);
            self.done_latch = false;
        }
        let mut title = sticky_title_status(self.semantic_title_status, observed_title);

        let needs_provider_screen_scan =
            matches!(title_provider, TitleProvider::Claude | TitleProvider::Grok);
        let content_generation = session.content_generation();
        let has_visible_sources = !working_markers.is_empty()
            || !blocked_markers.is_empty()
            || needs_provider_screen_scan;
        let mut next_deadline = session.program_status_sync_deadline();
        match status_screen_capture_plan(
            self.content_generation,
            content_generation,
            has_visible_sources,
            self.last_screen_capture,
            now,
        ) {
            StatusScreenCapture::Now => {
                self.content_generation = content_generation;
                self.last_screen_capture = Some(now);
                let screen = session.visible_text();
                self.working_marker = working_markers.iter().any(|marker| screen.contains(marker));
                self.blocked_marker = blocked_markers.iter().any(|marker| screen.contains(marker));
                self.provider_screen = provider_screen_status(title_provider, &screen);
            }
            StatusScreenCapture::At(deadline) => next_deadline = Some(deadline),
            StatusScreenCapture::None if !has_visible_sources => {
                // Keep the generation current without allocating a screen snapshot;
                // this prevents a later unrelated event from treating it as new.
                self.content_generation = content_generation;
                self.working_marker = false;
                self.blocked_marker = false;
                self.provider_screen = None;
            }
            StatusScreenCapture::None => {}
        }
        let bell = session.has_bell();
        let idle = session.idle_for();
        // Until Muse's own title is read (never, on a remote or tmux pane), it
        // is judged by output activity like an unknown terminal. Its hooks
        // still report blocked and finished turns.
        let activity_fallback = working_markers.is_empty()
            && match title_provider {
                TitleProvider::Other => true,
                TitleProvider::Muse => self.semantic_title_status.is_none(),
                _ => false,
            };
        let base = classify_facts(
            exited,
            self.working_marker,
            self.blocked_marker,
            bell,
            activity_fallback,
            idle,
        );
        let provider_screen = self.provider_screen;
        let working_marker = self.working_marker;
        let blocked_marker = self.blocked_marker;
        let mut screen_working = working_marker || provider_screen == Some(AgentStatus::Working);
        let screen_marker = provider_screen
            .or(blocked_marker.then_some(AgentStatus::Blocked))
            .or(working_marker.then_some(AgentStatus::Working));
        // Only a title read after the window can confirm a held marker; one
        // the settle deadline accepted may be the old conversation's too.
        self.screen_held = screen_hold_after(
            self.screen_held,
            screen_marker,
            fresh_title.filter(|_| evidence.title_fresh),
        );
        // The two-second evidence settle accepts uncertain titles as level
        // state; it does not make an unchanged Working title new work. Give
        // that title five seconds from observing the switch and five seconds
        // of PTY quiet, without releasing the newer policy's screen hold.
        let hook = (title_provider == TitleProvider::Muse && !exited)
            .then(|| muse_hook_status(self.muse_hook_state, title, screen_working))
            .flatten();
        let stale_busy_title = matches!(title, Some(AgentStatus::Working | AgentStatus::Blocked));
        let silent_switch_settled = !settling
            && hook.is_none()
            && self.silent_switch_until.is_some_and(|until| now >= until)
            && stale_busy_title
            && idle >= SILENT_SWITCH_SETTLE
            && screen_marker.is_none()
            && !exited
            && !bell;
        if !silent_switch_settled
            && stale_busy_title
            && !exited
            && !bell
            && screen_marker.is_none()
            && let Some(until) = self.silent_switch_until
        {
            let deadline = until
                .max(now + SILENT_SWITCH_SETTLE.saturating_sub(idle))
                .max(settle_deadline.unwrap_or(now));
            next_deadline = Some(next_deadline.map_or(deadline, |old| old.min(deadline)));
        }

        if title_provider == TitleProvider::Grok {
            // Debounces retain only attributable source evidence. The current
            // uncertain title/row can still show as level state, without a hold.
            if fresh_title == Some(AgentStatus::Blocked) {
                self.grok_blocked_at = Some(now);
            } else {
                let blocked_age = self
                    .grok_blocked_at
                    .map(|at| now.saturating_duration_since(at));
                title = hold_grok_blocked(title, blocked_age);
                if title == Some(AgentStatus::Blocked)
                    && let Some(at) = self.grok_blocked_at
                {
                    let deadline = at + Duration::from_millis(751);
                    next_deadline = Some(next_deadline.map_or(deadline, |old| old.min(deadline)));
                }
                if matches!(title, Some(AgentStatus::Idle))
                    || blocked_age.is_some_and(|age| age > Duration::from_millis(750))
                {
                    self.grok_blocked_at = None;
                }
            }

            let (working, last_seen, hold_deadline) = grok_screen_working_state(
                screen_working,
                provider_screen == Some(AgentStatus::Working) && !settling && !self.screen_held,
                self.grok_screen_working_at,
                now,
            );
            screen_working = working;
            self.grok_screen_working_at = last_seen;
            if let Some(deadline) = hold_deadline {
                next_deadline = Some(next_deadline.map_or(deadline, |old| old.min(deadline)));
            }
        }
        if title != Some(AgentStatus::Blocked) && provider_screen == Some(AgentStatus::Blocked) {
            title = provider_screen;
        }
        // A reporter can miss a prompt it is waiting on. Blocked read now from
        // the provider's screen or title, or a configured marker, beats a
        // Working root so the Blocked notification still fires. A title from
        // before an unsettled silent switch, or a held screen, is not read now.
        let visible_blocked = [
            (provider_screen == Some(AgentStatus::Blocked))
                .then_some(TerminalStatusReason::ProviderScreen),
            (fresh_title == Some(AgentStatus::Blocked) && self.silent_switch_until.is_none())
                .then_some(TerminalStatusReason::ProviderTitle),
            blocked_marker.then_some(TerminalStatusReason::BlockedMarker),
        ]
        .into_iter()
        .flatten()
        .find(|reason| status_is_attributable(settling, self.screen_held, *reason));

        let fallback_raw = hook.unwrap_or_else(|| {
            if silent_switch_settled {
                AgentStatus::Idle
            } else {
                combine_title_status(exited, base, title, screen_working)
            }
        });
        let new_explicit = program_status
            .explicit_completions
            .saturating_sub(self.program_explicit_completions);
        let new_idle = program_status
            .idle_completions
            .saturating_sub(self.program_idle_completions);
        self.program_explicit_completions = program_status.explicit_completions;
        self.program_idle_completions = program_status.idle_completions;
        // Codex can receive an automated startup prompt before its first UUID
        // title arrives. A PSP Working edge in that first binding confirms the
        // work; later binding switches still require a new submission.
        let codex_submitted_turn = submitted_turn || session.initial_unbound_submission();
        let codex_idle_completions =
            if title_provider == TitleProvider::Codex && codex_submitted_turn {
                new_idle
            } else {
                0
            };
        let codex_idle_completion = codex_idle_completions > 0
            && program_root.is_some_and(|root| root.state == ProgramState::Idle)
            && program_status.working_to_idle_revision > program_status.clear_revision;
        if codex_idle_completion {
            self.codex_idle_done = true;
        } else if !program_root.is_some_and(|root| root.state == ProgramState::Idle)
            || title_provider != TitleProvider::Codex
        {
            self.codex_idle_done = false;
        }
        let new_completions = new_explicit + codex_idle_completions;
        let previous_completion_revision = self.completion_revision;
        self.completion_revision = self.completion_revision.wrapping_add(new_completions);
        let masked_blocked = program_root
            .filter(|root| root.state == ProgramState::Working)
            .and(visible_blocked);
        let raw = program_root.map_or(fallback_raw, |root| {
            if masked_blocked.is_some() {
                AgentStatus::Blocked
            } else if root.state == ProgramState::Idle && self.codex_idle_done {
                AgentStatus::Done
            } else {
                program_status_state(root)
            }
        });
        let raw_reason = masked_blocked.unwrap_or_else(|| {
            program_root.map_or_else(
                || {
                    if silent_switch_settled && !exited && !bell {
                        TerminalStatusReason::Quiet
                    } else if hook == Some(raw) {
                        TerminalStatusReason::ProviderHook
                    } else {
                        status_reason(
                            raw,
                            exited,
                            bell,
                            title,
                            provider_screen,
                            working_marker,
                            blocked_marker,
                        )
                    }
                },
                program_status_reason,
            )
        });
        let raw_attributable = status_is_attributable(settling, self.screen_held, raw_reason);
        let (status, latch, armed) = if program_root.is_some() {
            // Authoritative protocol states bypass the visible-source latch.
            // Only the Codex transition above derives Done from PSP Idle.
            (raw, false, submitted_turn)
        } else if silent_switch_settled {
            // Retiring stale level state is neither a finish nor readiness
            // evidence for a turn in the replacement conversation.
            self.provider_settled = false;
            (raw, false, false)
        } else if raw_attributable {
            latch_done_after_readiness(
                self.prev_raw,
                raw,
                self.done_latch,
                can_latch_completion(
                    !working_markers.is_empty(),
                    self.semantic_title_status.is_some(),
                ),
                self.completion_armed,
                submitted_turn,
            )
        } else {
            // An old screen marker must not start an edge which completes when
            // it disappears. Keep the new binding's explicit submission only.
            (raw, false, submitted_turn)
        };
        self.prev_raw =
            (program_root.is_none() && raw_attributable && !silent_switch_settled).then_some(raw);
        self.done_latch = latch;
        self.completion_armed = armed;

        if program_root.is_none()
            && let Some(deadline) = fallback_status_deadline(now, idle, status, activity_fallback)
                .into_iter()
                .chain(settle_deadline)
                .min()
        {
            next_deadline = Some(next_deadline.map_or(deadline, |old| old.min(deadline)));
        }

        let reason = masked_blocked.unwrap_or_else(|| {
            program_root.map_or_else(
                || {
                    if silent_switch_settled {
                        TerminalStatusReason::Quiet
                    } else if hook == Some(status) {
                        TerminalStatusReason::ProviderHook
                    } else {
                        status_reason(
                            status,
                            exited,
                            bell,
                            title,
                            provider_screen,
                            working_marker,
                            blocked_marker,
                        )
                    }
                },
                program_status_reason,
            )
        });
        let attributable = status_is_attributable(settling, self.screen_held, reason);
        if exited {
            next_deadline = None;
        }
        // A report that the provider isn't working settles it, as an Idle
        // title does. A Working report alone is not readiness: Codex reports
        // Working while it boots a resumed session, with no prompt.
        if program_root.is_some_and(|root| root.state != ProgramState::Working) {
            self.provider_settled = true;
        }
        let completion_ready = self.provider_settled || submitted_turn;
        let coverage = if program_root.is_some() {
            TerminalStatusCoverage::ProgramStatus
        } else {
            self.fallback_coverage
        };
        let public_changed = status != self.status
            || reason != self.reason
            || coverage != self.coverage
            || attributable != self.attributable
            || binding_changed
            || completion_ready != self.completion_ready
            || self.completion_revision != previous_completion_revision;
        if public_changed {
            self.snapshot_seq = self.snapshot_seq.wrapping_add(1);
        }
        self.status = status;
        self.reason = reason;
        self.coverage = coverage;
        self.attributable = attributable;
        self.binding_rev = binding_rev;
        self.completion_ready = completion_ready;
        if public_changed && let Some(root) = program_root {
            log::debug!(
                "program status source=osc7501 launch={} state={:?} kind={:?} badge={:?}",
                session.id,
                root.state,
                root.kind,
                status
            );
        }
        (public_changed, next_deadline)
    }
}

fn program_status_state(root: &RootStatus) -> AgentStatus {
    match root.state {
        ProgramState::Idle => AgentStatus::Idle,
        ProgramState::Working => AgentStatus::Working,
        ProgramState::Blocked => AgentStatus::Blocked,
        // The four-badge model has no Error badge. A failed, stopped turn is
        // finished for ctl wait and attention, with the error source retained.
        ProgramState::Done | ProgramState::Error => AgentStatus::Done,
        ProgramState::Clear => AgentStatus::Idle, // clear never creates a root record
    }
}

fn program_status_reason(root: &RootStatus) -> TerminalStatusReason {
    if root.state == ProgramState::Error {
        TerminalStatusReason::ProgramStatusError
    } else {
        TerminalStatusReason::ProgramStatus(root.kind)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TitleProvider {
    Claude,
    Codex,
    Grok,
    Muse,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentTitleFrame {
    status: Option<AgentStatus>,
    name: Option<String>,
}

impl TitleProvider {
    /// Whether a `ctl new --prompt` task may be typed: only these providers
    /// have a verified readiness and acceptance protocol.
    fn has_startup_protocol(self) -> bool {
        matches!(self, Self::Claude | Self::Codex | Self::Grok)
    }

    /// Whether the pane's program is a known OSC 7501 reporter. Only these
    /// panes accept a report as status. Anything a shell, opencode or Amp
    /// pane prints can carry one, `cat` of a file included, and a report's
    /// origin can't be checked.
    fn reports_program_status(self) -> bool {
        matches!(self, Self::Claude | Self::Codex | Self::Grok | Self::Muse)
    }

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
            "muse" => Self::Muse,
            _ => Self::Other,
        }
    }
}

/// Parse one provider-owned title frame into independent lifecycle and naming
/// observations. Unknown titles produce neither: every process sharing the PTY
/// can emit OSC titles, and those events carry no process identity.
fn parse_agent_title(provider: TitleProvider, title: Option<&str>) -> Option<AgentTitleFrame> {
    let title = title?.trim();
    match provider {
        TitleProvider::Claude => {
            let marker = title.chars().next()?;
            let status = match marker {
                // Claude owns this title protocol. Claude 2.1.228+ uses
                // half-circle busy frames; older releases used braille frames.
                // Long-running tools can leave one frame unchanged for minutes,
                // so age is not completion evidence.
                '◐' | '◑' | '◒' | '◓' => AgentStatus::Working,
                marker if ('\u{2800}'..='\u{28ff}').contains(&marker) => AgentStatus::Working,
                '✳' => AgentStatus::Idle,
                _ => return None,
            };
            let rest = &title[marker.len_utf8()..];
            if !rest.is_empty() && !rest.chars().next().is_some_and(char::is_whitespace) {
                return None;
            }
            let name = rest.trim().to_string();
            Some(AgentTitleFrame {
                status: Some(status),
                name: (!name.is_empty()).then_some(name),
            })
        }
        TitleProvider::Codex => {
            // Codex 0.147 publishes action-required before the thread field.
            // Its normal contract is `thread | run-state [spinner]`; older
            // releases used `thread | run-state · activity`.
            for prefix in ["[ ! ] Action Required", "[ . ] Action Required"] {
                if let Some(rest) = title.strip_prefix(prefix) {
                    let name = if rest.is_empty() {
                        None
                    } else {
                        let name = rest.strip_prefix(" | ")?.trim();
                        (!name.is_empty()).then_some(name.to_string())
                    };
                    return Some(AgentTitleFrame {
                        status: Some(AgentStatus::Blocked),
                        name,
                    });
                }
            }

            // Parse state only from the final segment: thread names may contain
            // words such as "working", "ready", or "action required".
            let (name, state) = title.rsplit_once(" | ")?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some(AgentTitleFrame {
                status: Some(codex_title_state(state)?),
                name: Some(name.to_string()),
            })
        }
        TitleProvider::Grok => {
            let parts = title.split(" - ").map(str::trim).collect::<Vec<_>>();
            if parts.last() != Some(&"grok") {
                return None;
            }
            let payload = &parts[..parts.len() - 1];
            let (status, name_start) = match payload {
                ["⚠ Action Required", spinner, _activity, ..] if is_grok_spinner(spinner) => {
                    (Some(AgentStatus::Blocked), 3)
                }
                [spinner, _activity, ..] if is_grok_spinner(spinner) => {
                    // The spinner is Grok's semantic busy bit. Activity prose is
                    // intentionally open-ended and changes as tools are added.
                    // Grok removes both fields when it returns to Idle.
                    (Some(AgentStatus::Working), 2)
                }
                [reserved, ..] if *reserved == "⚠ Action Required" || is_grok_spinner(reserved) =>
                {
                    // A partial reserved prefix may come from a custom title
                    // layout or a generated name. Keep the name, but make no
                    // lifecycle claim without the complete provider frame.
                    (None, 0)
                }
                // With no complete state/activity prefix, every segment before
                // the provider sentinel belongs to the generated session name.
                _ => (Some(AgentStatus::Idle), 0),
            };
            let name = payload[name_start..]
                .iter()
                .filter(|part| !part.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" - ");
            Some(AgentTitleFrame {
                status,
                name: (!name.is_empty()).then_some(name),
            })
        }
        TitleProvider::Muse => {
            let name = title
                .split_once(' ')
                .filter(|(spinner, _)| is_codex_spinner(spinner))
                .map(|(_, name)| name)
                .unwrap_or(title);
            (!name.is_empty() && !name.contains(['/', '\\', ':'])).then(|| AgentTitleFrame {
                status: None,
                name: Some(name.to_string()),
            })
        }
        TitleProvider::Other => None,
    }
}

fn is_codex_spinner(value: &str) -> bool {
    matches!(
        value,
        "⠋" | "⠙" | "⠹" | "⠸" | "⠼" | "⠴" | "⠦" | "⠧" | "⠇" | "⠏"
    )
}

fn codex_title_state(state: &str) -> Option<AgentStatus> {
    let state = state.trim();
    if let Some((run_state, activity)) = state.split_once('·') {
        if activity.contains('·') {
            return None;
        }
        let run_state = run_state.trim().to_ascii_lowercase();
        let activity = activity.trim().to_ascii_lowercase();
        return match (run_state.as_str(), activity.as_str()) {
            ("ready", "action required") => Some(AgentStatus::Blocked),
            ("ready", "") => Some(AgentStatus::Idle),
            ("starting" | "working" | "thinking" | "waiting", activity) if !activity.is_empty() => {
                Some(AgentStatus::Working)
            }
            _ => None,
        };
    }

    let mut parts = state.split_whitespace();
    let run_state = parts.next()?.to_ascii_lowercase();
    let activity = parts.next();
    if parts.next().is_some() {
        return None;
    }
    match (run_state.as_str(), activity) {
        ("ready", None) => Some(AgentStatus::Idle),
        ("starting" | "working" | "thinking" | "waiting", None) => Some(AgentStatus::Working),
        ("starting" | "working" | "thinking" | "waiting", Some(spinner))
            if is_codex_spinner(spinner) =>
        {
            Some(AgentStatus::Working)
        }
        _ => None,
    }
}

fn title_status(
    provider: TitleProvider,
    title: Option<&str>,
    _age: Option<Duration>,
) -> Option<AgentStatus> {
    parse_agent_title(provider, title).and_then(|frame| frame.status)
}

fn title_status_trace(
    provider: TitleProvider,
    muse_cwd_label: Option<&str>,
    titles: &[Option<String>],
) -> Vec<AgentStatus> {
    titles
        .iter()
        .filter_map(|title| match provider {
            TitleProvider::Muse => muse_title_status(title.as_deref(), muse_cwd_label),
            provider => title_status(provider, title.as_deref(), None),
        })
        .collect()
}

/// Muse's title carries state only as the pane's cwd label, spinner-prefixed
/// while working; any other title says nothing about this pane.
fn muse_title_status(title: Option<&str>, cwd_label: Option<&str>) -> Option<AgentStatus> {
    match muxel_core::muse::title_state(title?, cwd_label?)? {
        muxel_core::muse::MuseState::Working => Some(AgentStatus::Working),
        _ => Some(AgentStatus::Idle),
    }
}

/// Muse's root-session hooks. A permission prompt outranks Muse's
/// still-spinning title, and a finished turn stays Done until a title or the
/// screen shows new work. Hooked work adds nothing: an interrupted turn fires
/// no hook, so titles or output activity tell when work stops.
fn muse_hook_status(
    hook: Option<muxel_core::muse::MuseState>,
    title: Option<AgentStatus>,
    screen_working: bool,
) -> Option<AgentStatus> {
    use muxel_core::muse::MuseState;
    match hook? {
        MuseState::Blocked => Some(AgentStatus::Blocked),
        MuseState::Done if !screen_working && title != Some(AgentStatus::Working) => {
            Some(AgentStatus::Done)
        }
        MuseState::Idle | MuseState::Working | MuseState::Done => None,
    }
}

fn has_working_to_nonworking_edge(statuses: &[AgentStatus]) -> bool {
    let mut saw_working = false;
    for status in statuses {
        if saw_working && *status != AgentStatus::Working {
            return true;
        }
        saw_working |= *status == AgentStatus::Working;
    }
    false
}

/// Once a provider proves its semantic title protocol, an unrelated OSC title
/// must not erase that observation or reactivate the weak PTY activity heuristic.
fn sticky_title_status(
    previous: Option<AgentStatus>,
    observed: Option<AgentStatus>,
) -> Option<AgentStatus> {
    observed.or(previous)
}

/// Provider-owned visible UI that adds state not carried by the terminal title.
fn provider_screen_status(provider: TitleProvider, screen: &str) -> Option<AgentStatus> {
    match provider {
        TitleProvider::Claude if is_claude_blocked_prompt(screen) => Some(AgentStatus::Blocked),
        TitleProvider::Claude if claude_screen_has_current_work(screen) => {
            Some(AgentStatus::Working)
        }
        TitleProvider::Grok
            if screen.lines().any(|line| {
                is_live_background_row(
                    line,
                    &["◎", "·"],
                    &["command", "monitor", "loop", "subagent"],
                    false,
                )
            }) =>
        {
            Some(AgentStatus::Working)
        }
        _ => None,
    }
}

fn claude_screen_has_current_work(screen: &str) -> bool {
    let mut newer_rows = [None, None];
    for line in screen.lines().rev() {
        if is_claude_live_task_footer(line, newer_rows)
            || is_claude_live_foreground_row(line)
            || is_claude_live_background_row(line)
        {
            return true;
        }
        if is_claude_completed_foreground_row(line) {
            return false;
        }
        newer_rows[1] = newer_rows[0];
        newer_rows[0] = Some(line);
    }
    false
}

fn is_claude_live_task_footer(line: &str, newer_rows: [Option<&str>; 2]) -> bool {
    if is_claude_live_task_footer_line(line) {
        return true;
    }
    if !line.trim().starts_with("Auto mode") {
        return false;
    }

    let mut footer = line.trim().to_string();
    let mut previous = line;
    for row in newer_rows.into_iter().flatten() {
        if row.trim().is_empty() {
            return false;
        }
        if previous.ends_with(char::is_whitespace) || row.starts_with(char::is_whitespace) {
            footer.push(' ');
        }
        footer.push_str(row.trim());
        if is_claude_live_task_footer_line(&footer) {
            return true;
        }
        previous = row;
    }
    false
}

fn is_claude_live_task_footer_line(line: &str) -> bool {
    let Some(summary) = line.trim().strip_prefix("Auto mode on · ") else {
        return false;
    };
    let mut tasks = summary
        .split(" · ")
        .flat_map(|group| group.split(", "))
        .peekable();
    tasks.peek().is_some() && tasks.all(is_claude_live_task_count)
}

fn is_claude_live_task_count(task: &str) -> bool {
    let Some((count, kind)) = task.split_once(' ') else {
        return false;
    };
    let Ok(count) = count.parse::<usize>() else {
        return false;
    };
    if count == 0 {
        return false;
    }
    let base = if count == 1 {
        kind
    } else {
        let Some(base) = kind.strip_suffix('s') else {
            return false;
        };
        base
    };
    matches!(
        base,
        "shell"
            | "monitor"
            | "agent"
            | "team"
            | "local agent"
            | "cloud session"
            | "background task"
            | "background dynamic workflow"
            | "MCP task"
            | "remote dynamic workflow"
    )
}

fn is_claude_live_foreground_row(line: &str) -> bool {
    let Some(status) = line.trim().strip_prefix("✻ ") else {
        return false;
    };
    let Some((activity, metrics)) = status.rsplit_once(" (") else {
        return false;
    };
    if activity
        .trim_end()
        .strip_suffix('…')
        .is_none_or(|label| label.trim().is_empty())
    {
        return false;
    }
    let Some(metrics) = metrics.strip_suffix(')') else {
        return false;
    };
    let Some((elapsed, tokens)) = metrics.split_once(" · ") else {
        return false;
    };
    is_claude_elapsed(elapsed.trim())
        && tokens
            .trim()
            .strip_suffix(" tokens")
            .is_some_and(|count| is_claude_token_count(count.trim()))
}

fn is_claude_completed_foreground_row(line: &str) -> bool {
    let Some(status) = line.trim().strip_prefix("✻ ") else {
        return false;
    };
    let Some((summary, finished_at)) = status.rsplit_once(" · done ") else {
        return false;
    };
    let Some((activity, elapsed)) = summary.rsplit_once(" for ") else {
        return false;
    };
    !activity.trim().is_empty()
        && is_claude_elapsed(elapsed.trim())
        && is_claude_clock_time(finished_at.trim())
}

fn is_claude_clock_time(value: &str) -> bool {
    let Some((clock, meridiem)) = value.rsplit_once(' ') else {
        return false;
    };
    if !matches!(meridiem, "AM" | "PM") {
        return false;
    }
    let Some((hour, minute)) = clock.split_once(':') else {
        return false;
    };
    let Ok(hour) = hour.parse::<u8>() else {
        return false;
    };
    hour != 0
        && hour <= 12
        && minute.len() == 2
        && minute.bytes().all(|byte| byte.is_ascii_digit())
        && minute.parse::<u8>().is_ok_and(|minute| minute < 60)
}

fn is_claude_token_count(value: &str) -> bool {
    let Some(value) = value
        .strip_prefix("↑ ")
        .or_else(|| value.strip_prefix("↓ "))
    else {
        return false;
    };
    let (number, scaled) = value
        .strip_suffix('k')
        .or_else(|| value.strip_suffix('m'))
        .map_or((value, false), |number| (number, true));
    let mut parts = number.split('.');
    let Some(whole) = parts.next() else {
        return false;
    };
    let fraction = parts.next();
    !whole.is_empty()
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.is_none_or(|fraction| {
            scaled && !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
        && parts.next().is_none()
}

fn is_claude_elapsed(value: &str) -> bool {
    let mut previous_rank = 4;
    for part in value.split_whitespace() {
        let Some((unit, digits)) = part
            .char_indices()
            .next_back()
            .map(|(index, unit)| (unit, &part[..index]))
        else {
            return false;
        };
        let rank = match unit {
            'h' => 3,
            'm' => 2,
            's' => 1,
            _ => return false,
        };
        if rank >= previous_rank
            || digits.is_empty()
            || !digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            return false;
        }
        previous_rank = rank;
    }
    previous_rank != 4
}

fn is_claude_live_background_row(line: &str) -> bool {
    if is_live_background_row(line, &["·"], &["command"], true) {
        return true;
    }

    let line = line.trim();
    let Some((activity, summary)) = line.rsplit_once(" · ") else {
        return false;
    };
    if !activity.trim().starts_with("✻ ") {
        return false;
    }
    is_live_background_row(&format!("· {summary}"), &["·"], &["shell"], false)
}

fn is_claude_blocked_prompt(screen: &str) -> bool {
    // Prompt chrome is transient and lives near the bottom of the viewport.
    // Restricting the scan avoids matching an old prompt quoted in scrollback.
    let recent = screen
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(16)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    let has_cancel = recent.contains("esc to cancel");
    if !has_cancel {
        return false;
    }

    if recent.contains("run a dynamic workflow?") {
        return true;
    }

    let has_choice = recent.lines().any(|line| {
        let choice = line.trim().trim_start_matches(['┃', '❯']).trim();
        choice == "yes"
            || choice.starts_with("1. yes")
            || choice.starts_with("2. yes")
            || choice.starts_with("2. no")
            || choice.starts_with("3. no")
    });
    if recent.contains("do you want to proceed?") && has_choice {
        return true;
    }

    let has_navigation = [
        "tab/arrow keys to navigate",
        "arrow keys to navigate",
        "arrows to navigate",
        "↑/↓ to navigate",
        "↑↓ to navigate",
    ]
    .into_iter()
    .any(|hint| recent.contains(hint));
    recent.contains("enter to confirm") || (recent.contains("enter to select") && has_navigation)
}

fn is_live_background_row(
    line: &str,
    provider_markers: &[&str],
    allowed_kinds: &[&str],
    require_interrupt_hint: bool,
) -> bool {
    let words: Vec<_> = line.split_whitespace().collect();
    if words
        .first()
        .is_none_or(|word| !provider_markers.contains(word))
    {
        return false;
    }

    const INTERRUPT_HINT: [&str; 6] = ["·", "send", "a", "message", "to", "interrupt"];
    let summary_end = if words.ends_with(&INTERRUPT_HINT) {
        words.len() - INTERRUPT_HINT.len()
    } else if require_interrupt_hint {
        return false;
    } else {
        words.len()
    };
    if summary_end < 4 || words[summary_end - 2..summary_end] != ["still", "running"] {
        return false;
    }

    let summary = &words[1..summary_end - 2];
    let mut index = 0;
    while index < summary.len() {
        let Some(kind) = summary.get(index + 1) else {
            return false;
        };
        let Ok(count) = summary[index].parse::<usize>() else {
            return false;
        };
        if count == 0
            || !allowed_kinds.iter().any(|base| {
                (count == 1 && kind == base) || (count > 1 && kind.strip_suffix('s') == Some(*base))
            })
        {
            return false;
        }
        index += 2;
        if index == summary.len() {
            return true;
        }
        if summary[index] != "·" {
            return false;
        }
        index += 1;
    }
    false
}

fn is_grok_spinner(part: &str) -> bool {
    matches!(part, "⠋" | "⠙" | "⠹" | "⠸" | "⠼" | "⠴" | "⠦" | "⠧")
}

fn combine_title_status(
    exited: bool,
    base: AgentStatus,
    title: Option<AgentStatus>,
    screen_working: bool,
) -> AgentStatus {
    if exited {
        return AgentStatus::Done;
    }
    if base == AgentStatus::Blocked || title == Some(AgentStatus::Blocked) {
        return AgentStatus::Blocked;
    }
    // Provider-owned visible UI is independent strong evidence. Claude can
    // leave its semantic title in the Idle shape while a background command or
    // permission form remains visible.
    if screen_working {
        return AgentStatus::Working;
    }
    match title {
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

/// Grok's exact foreground background-work row can disappear for a repaint.
/// Debounce only a previously observed positive row; stable absence expires.
fn hold_grok_screen_working(observed: bool, last_seen_age: Option<Duration>) -> bool {
    observed || last_seen_age.is_some_and(|age| age <= GROK_SCREEN_WORKING_HOLD)
}

fn grok_screen_working_state(
    base_working: bool,
    provider_working: bool,
    last_seen: Option<Instant>,
    now: Instant,
) -> (bool, Option<Instant>, Option<Instant>) {
    if provider_working {
        return (true, Some(now), None);
    }
    let age = last_seen.map(|at| now.saturating_duration_since(at));
    if hold_grok_screen_working(false, age) {
        let deadline = last_seen.map(|at| at + GROK_SCREEN_WORKING_HOLD + Duration::from_millis(1));
        (true, last_seen, deadline)
    } else {
        (base_working, None, None)
    }
}

/// Remove provider lifecycle decoration before a title is considered for the
/// pane's persisted automatic name. A title containing only state is not a name.
pub fn clean_agent_title(program: &str, title: &str) -> Option<String> {
    match TitleProvider::from_program(program) {
        TitleProvider::Other => Some(title.to_string()),
        provider => parse_agent_title(provider, Some(title))?.name,
    }
}

fn program_auto_title(
    session: &TerminalSession,
    provider: TitleProvider,
    exited: bool,
) -> Option<String> {
    if exited || provider != TitleProvider::Claude {
        return None;
    }
    let program_status = session.program_status_snapshot();
    if let Some(name) = program_status
        .root
        .as_ref()
        .and_then(|root| root.title.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return Some(name.to_string());
    }
    if !program_status.reported {
        return None;
    }
    let title = session.title()?;
    if let Some(frame) = parse_agent_title(provider, Some(&title)) {
        return frame.name;
    }
    // Claude drops its lifecycle glyph for the rest of the launch once its
    // capability probe succeeds, which any root report shows. So a glyph-less
    // title still names the pane after `/clear`, `/resume`, RIS or a switch
    // retires the root. This exception is for naming; title_status stays strict.
    let name = title.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Decide an agent's lifecycle state from its signals. Pure (unit-testable):
/// exit wins; then on-screen markers (working spinner, blocked prompt); then a
/// rung bell means a finished turn; then recent output is an opt-in fallback
/// for unknown terminals only.
#[cfg(test)]
fn classify(
    exited: bool,
    screen: &str,
    working: &[String],
    blocked: &[String],
    bell: bool,
    activity_fallback: bool,
    idle: Duration,
) -> AgentStatus {
    classify_facts(
        exited,
        working.iter().any(|m| screen.contains(m)),
        blocked.iter().any(|m| screen.contains(m)),
        bell,
        activity_fallback && working.is_empty(),
        idle,
    )
}

fn classify_facts(
    exited: bool,
    working: bool,
    blocked: bool,
    bell: bool,
    activity_fallback: bool,
    idle: Duration,
) -> AgentStatus {
    if exited {
        return AgentStatus::Done;
    }
    if blocked {
        return AgentStatus::Blocked;
    }
    // User-actionable input wins when a provider leaves its working marker on
    // screen behind an approval prompt.
    if working {
        return AgentStatus::Working;
    }
    if bell {
        return AgentStatus::Done;
    }
    // Raw PTY churn is useful only for unknown terminals. Known providers own
    // semantic signals; using echoed typing here would forge Working.
    if activity_fallback && idle < Duration::from_secs(2) {
        return AgentStatus::Working;
    }
    AgentStatus::Idle
}

/// Promote a working→idle transition to `Done`, latching it until the agent works
/// again. Returns `(displayed status, new latch state)`.
/// Pure half of the runtime reducer's done-latch, so a finished turn shows
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
        // New activity or an input request replaces the previous completion.
        AgentStatus::Working | AgentStatus::Blocked => (raw, false),
        // Bell/exit is precise completion evidence. Latch it so acknowledging the
        // notification cannot turn lifecycle back into Idle.
        AgentStatus::Done => (AgentStatus::Done, true),
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

fn provider_settled_after_title(
    settled: bool,
    observed_title: Option<AgentStatus>,
    submitted: bool,
) -> bool {
    settled || submitted || observed_title == Some(AgentStatus::Idle)
}

fn status_reason(
    status: AgentStatus,
    exited: bool,
    bell: bool,
    title: Option<AgentStatus>,
    provider_screen: Option<AgentStatus>,
    working_marker: bool,
    blocked_marker: bool,
) -> TerminalStatusReason {
    if exited {
        return TerminalStatusReason::ProcessExit;
    }
    match status {
        AgentStatus::Blocked if provider_screen == Some(AgentStatus::Blocked) => {
            TerminalStatusReason::ProviderScreen
        }
        AgentStatus::Blocked if title == Some(AgentStatus::Blocked) => {
            TerminalStatusReason::ProviderTitle
        }
        AgentStatus::Blocked if blocked_marker => TerminalStatusReason::BlockedMarker,
        AgentStatus::Working if provider_screen == Some(AgentStatus::Working) => {
            TerminalStatusReason::ProviderScreen
        }
        AgentStatus::Working if working_marker => TerminalStatusReason::WorkingMarker,
        AgentStatus::Working if title == Some(AgentStatus::Working) => {
            TerminalStatusReason::ProviderTitle
        }
        AgentStatus::Working => TerminalStatusReason::RecentOutput,
        AgentStatus::Done if bell => TerminalStatusReason::Bell,
        AgentStatus::Done if title.is_some() => TerminalStatusReason::ProviderTitle,
        AgentStatus::Idle if title.is_some() => TerminalStatusReason::ProviderTitle,
        AgentStatus::Done | AgentStatus::Idle => TerminalStatusReason::Quiet,
        AgentStatus::Blocked => TerminalStatusReason::BlockedMarker,
    }
}

/// Whether the status can be attributed to the current provider binding, so
/// the app may notify for it and record it as the last status seen. While a
/// switch reported out of band settles, the status still shows level state,
/// but that may be the old conversation's: a blocked title or prompt read just
/// before or after the switch. A process exit is this launch's own fact, and
/// a bell rings only for output read after the switch window. A status read
/// from a screen marker stays non-attributable while `screen_held`, which
/// starts at the switch, however fresh the output around it.
fn status_is_attributable(settling: bool, screen_held: bool, reason: TerminalStatusReason) -> bool {
    match reason {
        TerminalStatusReason::ProcessExit | TerminalStatusReason::Bell => true,
        // The binding switch clears the old root, so a current root report is
        // provider-owned evidence even while an older title is settling.
        TerminalStatusReason::ProgramStatus(_) | TerminalStatusReason::ProgramStatusError => true,
        TerminalStatusReason::ProviderScreen
        | TerminalStatusReason::BlockedMarker
        | TerminalStatusReason::WorkingMarker
            if screen_held =>
        {
            false
        }
        _ => !settling,
    }
}

/// Whether status read from a screen marker is still held as possibly the old
/// conversation's. The hold starts when a binding switches out of band; the
/// screen isn't captured at that moment, so later captures decide. Fresh bytes
/// alone don't make an unchanged prompt the new conversation's; the hold ends
/// the first time a screen capture shows no marker, or when a title read after
/// the switch window reports the state the screen shows.
fn screen_hold_after(
    held: bool,
    screen: Option<AgentStatus>,
    fresh_title: Option<AgentStatus>,
) -> bool {
    held && screen.is_some() && fresh_title != screen
}

fn status_coverage(
    provider: TitleProvider,
    working_markers: &[String],
    blocked_markers: &[String],
) -> TerminalStatusCoverage {
    if provider != TitleProvider::Other {
        TerminalStatusCoverage::ProviderVisible
    } else if !working_markers.is_empty() || !blocked_markers.is_empty() {
        TerminalStatusCoverage::VisibleMarkers
    } else {
        TerminalStatusCoverage::ActivityOnly
    }
}

fn should_capture_status_screen(
    previous_generation: u64,
    current_generation: u64,
    has_visible_sources: bool,
) -> bool {
    has_visible_sources && previous_generation != current_generation
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusScreenCapture {
    None,
    Now,
    At(Instant),
}

fn status_screen_capture_plan(
    previous_generation: u64,
    current_generation: u64,
    has_visible_sources: bool,
    last_capture: Option<Instant>,
    now: Instant,
) -> StatusScreenCapture {
    if !should_capture_status_screen(previous_generation, current_generation, has_visible_sources) {
        return StatusScreenCapture::None;
    }
    if let Some(deadline) = last_capture.map(|at| at + STATUS_SCREEN_SCAN_INTERVAL)
        && now < deadline
    {
        return StatusScreenCapture::At(deadline);
    }
    StatusScreenCapture::Now
}

/// When Working read from recent output expires, for a terminal judged by
/// output activity.
fn fallback_status_deadline(
    now: Instant,
    idle: Duration,
    status: AgentStatus,
    activity_fallback: bool,
) -> Option<Instant> {
    (status == AgentStatus::Working && activity_fallback && idle < Duration::from_secs(2))
        .then(|| now + (Duration::from_secs(2) - idle))
}

/// The wall-clock time `at` stands for.
fn wall_time_at(at: Instant) -> SystemTime {
    let (now, wall) = (Instant::now(), SystemTime::now());
    if at >= now {
        wall.checked_add(at - now).unwrap_or(wall)
    } else {
        wall.checked_sub(now - at).unwrap_or(wall)
    }
}

fn runtime_timer_is_current(current: u64, callback: u64) -> bool {
    current == callback
}

/// How long to wait for a runtime deadline. One that already passed (typically
/// while the refresh that computed it was still running) fires at once rather
/// than being dropped: a pending screen capture or fallback expiry must still be
/// serviced. Every deadline is computed strictly after its refresh's `now`, so a
/// zero-delay refresh makes progress instead of rescheduling itself.
fn runtime_timer_delay(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}

fn is_submission_input(bytes: &[u8]) -> bool {
    bytes == b"\r"
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

/// How a child ended, carried from `ProfiledPtyChunk::Exit` to the view in one piece so
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
    rx: &ProfiledPtyReceiver,
    instance_id: Uuid,
    output: &mut PtyOutput,
    exit: &mut Option<ExitInfo>,
) {
    while let Ok(more) = rx.try_recv() {
        match more {
            ProfiledPtyChunk::Output {
                bytes,
                timing,
                read_at,
            } => {
                if let Some(timing) = timing {
                    profile::output_drained(instance_id, timing, rx.take_profile_peak());
                }
                output.push(&bytes, read_at);
            }
            ProfiledPtyChunk::Exit {
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
    startup_turn: Option<muxel_core::control::StartupTurn>,
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
    /// The signal that killed the child, when one did (see `ProfiledPtyChunk::Exit`).
    exit_signal: Option<String>,
    /// Set when the session ended on a PTY read error rather than a clean EOF —
    /// the child may still have been healthy (see `ProfiledPtyChunk::Exit`).
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
    runtime: RuntimeStatusState,
    /// Last time we `cx.notify()`'d a paint from the drain loop (background throttle).
    last_paint_notify: std::cell::Cell<std::time::Instant>,
    /// Profiler correlation only: advances once per output-driven `cx.notify()`.
    /// State is fixed-size per pane and no terminal content is retained.
    profile_notify_generation: std::cell::Cell<u64>,
    profile_last_notify_cause: std::cell::Cell<Option<TerminalNotifyCause>>,
    profile_focus: bool,
    /// A throttled batch must still paint if output stops before the next batch.
    /// The generation invalidates an older, later timer when interactive output
    /// brings the deadline forward.
    pending_paint_deadline: std::cell::Cell<Option<std::time::Instant>>,
    paint_timer_generation: std::cell::Cell<u64>,
    /// The deadline of a pending paint that a cursor-hide hold
    /// ([`CURSOR_HIDE_HOLD`]) set, while it is pending. A status repaint from
    /// any path waits for it too, so it can't draw the frame the hold skips.
    paint_hold_until: std::cell::Cell<Option<std::time::Instant>>,
    _drain: Task<()>,
    _runtime_events: Task<()>,
}

/// Optional app-owned diagnostic invoked at the existing GPUI terminal focus
/// edge. Muxel injects it only while the opt-in profiler is enabled, keeping
/// platform/window knowledge out of this crate without creating a dependency
/// cycle.
pub type TerminalFocusObserver = fn(Uuid, bool, &mut Window, &mut App);

/// Provider evidence for one ctl paste-and-submit operation. Capture before
/// pasting; the caller owns deadlines, input-revision guards and the one retry.
pub struct PromptSubmission {
    session: std::sync::Weak<TerminalSession>,
    binding_revision: u64,
    provider: TitleProvider,
    instance_id: Uuid,
    input: String,
    expected_input: String,
    before_title: u64,
    allow_working_title: bool,
    before_prompt: Option<String>,
    origin: Option<PromptOriginProof>,
    observable: bool,
}

impl PromptSubmission {
    /// Whether this pane can show a receipt for this prompt at all. Without
    /// one, waiting for confirmation only delays the caller.
    pub fn observable(&self) -> bool {
        self.observable
    }
}

struct PromptOriginProof {
    frame: crate::startup::CodexCtlOrigin,
    title_generation: u64,
    resize_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptSubmissionState {
    Accepted,
    Retryable,
    Unconfirmed,
    Interrupted,
}

fn with_startup_view<R>(
    view: &WeakEntity<TerminalView>,
    session: &Arc<TerminalSession>,
    cx: &mut AsyncApp,
    action: impl FnOnce(&TerminalView) -> R,
) -> Result<R, crate::startup::Stop> {
    view.update(cx, |view, _| {
        (!view.exited && Arc::ptr_eq(&view.session, session)).then(|| action(view))
    })
    .ok()
    .flatten()
    .ok_or(crate::startup::Stop::Interrupted)
}

fn claude_startup_ready(
    screen: &crate::startup::Screen,
    title: Option<&str>,
    blocked_markers: &[String],
    empty: bool,
) -> bool {
    let Some(context) = screen.claude_composer_context(empty) else {
        return false;
    };
    parse_agent_title(TitleProvider::Claude, title).and_then(|frame| frame.status)
        != Some(AgentStatus::Working)
        && provider_screen_status(TitleProvider::Claude, &context).is_none()
        && !blocked_markers
            .iter()
            .any(|marker| !marker.is_empty() && context.contains(marker))
}

fn startup_ready(
    provider: TitleProvider,
    screen: &crate::startup::Screen,
    title: Option<&str>,
    blocked_markers: &[String],
    empty: bool,
) -> bool {
    if provider == TitleProvider::Claude {
        return claude_startup_ready(screen, title, blocked_markers, empty);
    }
    let context = match provider {
        TitleProvider::Codex => screen.codex_composer_context(empty),
        // Grok supports configurable/hidden prompt prefixes and multiline
        // editors. Its semantic idle title, after its configured startup settle,
        // is the supported admission signal; do not guess a universal glyph.
        TitleProvider::Grok => Some(screen.text.clone()),
        _ => return false,
    };
    let Some(context) = context else {
        return false;
    };
    parse_agent_title(provider, title).and_then(|frame| frame.status) == Some(AgentStatus::Idle)
        && provider_screen_status(provider, &context).is_none()
        && (provider != TitleProvider::Codex
            || ![
                "Trust this folder?",
                "Folder access",
                "enter continue · esc quit",
            ]
            .iter()
            .any(|chrome| context.contains(chrome)))
        && !blocked_markers
            .iter()
            .any(|marker| !marker.is_empty() && context.contains(marker))
}

/// Starting/Waiting are useful display states but do not establish that a new
/// user task was accepted. Do not use the sticky display status or our CR marker.
fn startup_title_working(provider: TitleProvider, title: Option<&str>) -> bool {
    if parse_agent_title(provider, title).and_then(|frame| frame.status)
        != Some(AgentStatus::Working)
    {
        return false;
    }
    if provider == TitleProvider::Codex {
        let Some((_, state)) = title.and_then(|title| title.rsplit_once(" | ")) else {
            return false;
        };
        return matches!(
            state
                .split_whitespace()
                .next()
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("working" | "thinking")
        );
    }
    true
}

fn startup_draft_matches(
    provider: TitleProvider,
    screen: &crate::startup::Screen,
    input: &str,
) -> bool {
    match provider {
        TitleProvider::Claude => screen.claude_draft_matches(input),
        TitleProvider::Codex => screen.codex_draft_matches(input),
        _ => false,
    }
}

fn startup_empty_composer(provider: TitleProvider, screen: &crate::startup::Screen) -> bool {
    match provider {
        TitleProvider::Claude => screen.claude_composer_context(true).is_some(),
        TitleProvider::Codex => screen.codex_composer_context(true).is_some(),
        _ => false,
    }
}

struct StartupAttempt<'a> {
    provider: TitleProvider,
    input: &'a str,
    expected_input: String,
    before_title: u64,
    // ctl sends begun during an existing turn need matching transcript proof.
    // Startup attempts keep their existing title-based confirmation.
    allow_working_title: bool,
    before_prompt: Option<String>,
    instance_id: Uuid,
}

fn startup_has_fresh_user_row(attempt: &StartupAttempt<'_>, prompt: Option<&str>) -> bool {
    prompt.is_some_and(|prompt| {
        crate::startup::normalized(prompt) == attempt.expected_input
            && attempt.before_prompt.as_deref() != Some(attempt.expected_input.as_str())
    })
}

fn startup_retry_allowed(
    attempt: &StartupAttempt<'_>,
    screen: &crate::startup::Screen,
    title: Option<&str>,
    blocked_markers: &[String],
    prompt: Option<&str>,
) -> bool {
    attempt.provider == TitleProvider::Codex
        && !startup_has_fresh_user_row(attempt, prompt)
        && screen.codex_exact_retry_draft(attempt.input)
        && startup_ready(attempt.provider, screen, title, blocked_markers, false)
}

fn startup_provider_accepted(
    attempt: &StartupAttempt<'_>,
    screen: &crate::startup::Screen,
    title: Option<&str>,
    title_generation: u64,
    prompt: Option<&str>,
) -> bool {
    if startup_draft_matches(attempt.provider, screen, attempt.input) {
        return false;
    }
    let fresh_user_row = startup_has_fresh_user_row(attempt, prompt)
        && startup_empty_composer(attempt.provider, screen);
    fresh_user_row
        || (attempt.allow_working_title
            && title_generation != attempt.before_title
            && startup_title_working(attempt.provider, title))
}

/// The provider title a controller task's startup judges, with its change
/// generation: a tmux-wrapped pane's title read through tmux, else the PTY's own.
fn startup_title(
    session: &TerminalSession,
    probe: Option<&crate::startup::TitleProbe>,
) -> (u64, Option<String>) {
    match probe {
        Some(probe) => probe.snapshot(),
        None => session.title_snapshot(),
    }
}

fn ctl_title_can_confirm(
    provider: TitleProvider,
    screen: &crate::startup::Screen,
    title: Option<&str>,
    status: AgentStatus,
) -> bool {
    matches!(status, AgentStatus::Idle | AgentStatus::Done)
        && !matches!(
            parse_agent_title(provider, title).and_then(|frame| frame.status),
            Some(AgentStatus::Working | AgentStatus::Blocked)
        )
        && !matches!(
            provider_screen_status(provider, &screen.text),
            Some(AgentStatus::Working | AgentStatus::Blocked)
        )
}

/// A ctl receipt is either a working title after an idle one, which needs a
/// provider status title muxel can read (tmux hides OSC titles), or a fresh
/// transcript row beside an empty composer. Those readers stop at the first
/// blank line, so only a prompt without one can match; a row repeating the
/// previous prompt is never fresh; and Grok's composer is never known empty.
fn ctl_acceptance_observable(
    provider: TitleProvider,
    title: Option<&str>,
    allow_working_title: bool,
    input: &str,
    before_prompt: Option<&str>,
) -> bool {
    let title_receipt = allow_working_title
        && parse_agent_title(provider, title)
            .and_then(|frame| frame.status)
            .is_some();
    let transcript_receipt = matches!(provider, TitleProvider::Claude | TitleProvider::Codex)
        && !input.trim().lines().any(|line| line.trim().is_empty())
        && before_prompt != Some(crate::startup::normalized(input).as_str());
    title_receipt || transcript_receipt
}

fn prompt_submission_state(
    attempt: &StartupAttempt<'_>,
    screen: &crate::startup::Screen,
    title: Option<&str>,
    title_generation: u64,
    prompt: Option<&str>,
    marker_sets: [&[String]; 2],
    origin: Option<&PromptOriginProof>,
) -> PromptSubmissionState {
    if startup_provider_accepted(attempt, screen, title, title_generation, prompt) {
        PromptSubmissionState::Accepted
    } else if cfg!(windows)
        && attempt.provider == TitleProvider::Codex
        && !startup_has_fresh_user_row(attempt, prompt)
        && origin.is_some_and(|proof| {
            proof.title_generation == title_generation
                && screen.codex_ctl_retry_draft(attempt.input, &proof.frame)
        })
        && codex_ctl_ready(screen, title, marker_sets, false)
    {
        PromptSubmissionState::Retryable
    } else {
        PromptSubmissionState::Unconfirmed
    }
}

/// Existing panes may deliberately publish only their thread name. A strict
/// ctl editor proof does not require a semantic Idle title, but positive work,
/// permission and menu evidence must still veto the retry.
fn codex_ctl_ready(
    screen: &crate::startup::Screen,
    title: Option<&str>,
    marker_sets: [&[String]; 2],
    empty: bool,
) -> bool {
    let Some(context) = screen.codex_composer_context(empty) else {
        return false;
    };
    if matches!(
        parse_agent_title(TitleProvider::Codex, title).and_then(|frame| frame.status),
        Some(AgentStatus::Working | AgentStatus::Blocked)
    ) {
        return false;
    }
    if title.is_some_and(|title| {
        matches!(
            title.trim().to_ascii_lowercase().as_str(),
            "working" | "thinking" | "starting" | "waiting" | "blocked" | "action required"
        )
    }) {
        return false;
    }
    if title
        .and_then(|title| title.rsplit_once(" | "))
        .is_some_and(|(_, state)| {
            let state = state.trim().to_ascii_lowercase();
            matches!(
                state.split_whitespace().next(),
                Some("working" | "thinking" | "starting" | "waiting" | "blocked")
            ) || state.starts_with("action required")
        })
    {
        return false;
    }
    let lower = context.to_ascii_lowercase();
    ![
        "esc to interrupt",
        "esc interrupt",
        "ctrl+c to interrupt",
        "trust this folder?",
        "folder access",
        "enter continue · esc quit",
        "↑",
        "↓",
        "enter to select",
        "enter to confirm",
        "tab to select",
        "esc to cancel",
        "esc to dismiss",
        "type to filter",
    ]
    .iter()
    .any(|chrome| lower.contains(chrome))
        && !marker_sets
            .into_iter()
            .flatten()
            .any(|marker| !marker.is_empty() && context.contains(marker))
}

fn ctl_transcript_prompt(
    provider: TitleProvider,
    screen: &crate::startup::Screen,
    history: impl FnOnce() -> String,
) -> Option<String> {
    if provider == TitleProvider::Codex {
        screen.codex_ctl_transcript_prompt()
    } else {
        muxel_core::readaloud::prompt_from_screen(&history())
    }
}

async fn confirm_startup_submission(
    view: &WeakEntity<TerminalView>,
    session: &Arc<TerminalSession>,
    cx: &mut AsyncApp,
    guard: &mut crate::startup::Guard,
    armed_at: Instant,
    attempt: &StartupAttempt<'_>,
    probe: Option<&crate::startup::TitleProbe>,
) -> Result<(), crate::startup::Stop> {
    use crate::startup::{Acceptance, AcceptanceAction};
    let mut acceptance = Acceptance::default();
    let started = Instant::now();
    loop {
        let (accepted, retryable) = with_startup_view(view, session, cx, |view| {
            guard.check(armed_at.elapsed(), session.input_rev())?;
            let (generation, title) = startup_title(session, probe);
            let screen = session.startup_screen();
            let history = session.recent_text(500);
            let prompt = muxel_core::readaloud::prompt_from_screen(&history);
            let accepted = startup_provider_accepted(
                attempt,
                &screen,
                title.as_deref(),
                generation,
                prompt.as_deref(),
            );
            // Grok can restore a previously stashed draft after accepting a
            // task. Its retained text is not a safe Enter-retry predicate.
            let retryable = startup_retry_allowed(
                attempt,
                &screen,
                title.as_deref(),
                &view.blocked_markers,
                prompt.as_deref(),
            );
            Ok::<_, crate::startup::Stop>((accepted, retryable))
        })??;
        match acceptance.observe(started.elapsed(), accepted, retryable) {
            AcceptanceAction::Accepted => {
                guard.submitted(startup_now_ms());
                return Ok(());
            }
            AcceptanceAction::RetryEnter => {
                // Recheck session, ownership and input revision on admission;
                // never re-paste the task or retry an ambiguous provider.
                write_startup_input(view, session, cx, guard, armed_at, true, |session| {
                    session.try_submit_input(b"\r")
                })?;
                profile::startup_event(
                    attempt.instance_id,
                    "codex",
                    "automation-enter-retry",
                    armed_at.elapsed(),
                    1,
                );
            }
            AcceptanceAction::Failed => {
                guard.fail(crate::startup::Stop::NotAccepted);
                return Err(crate::startup::Stop::NotAccepted);
            }
            AcceptanceAction::Observe => {}
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
}

const STARTUP_POLL_MS: u64 = 100;
const STARTUP_SETTLE_MS: u128 = 2000;
const STARTUP_MAX_WAIT_MS: u64 = 30_000;
const STARTUP_KEY_GAP_MS: u64 = 150;
const STARTUP_PRE_TYPE_MS: u64 = 300;
// The prompt is typed in one burst; wait before the submit Enter so the agent
// has finished ingesting the text and treats it as a deliberate submit rather
// than a newline within a paste.
const STARTUP_SUBMIT_DELAY_MS: u64 = 400;
/// How long a controller task keeps checking for a ready composer after the
/// settle cap and its own fixed pauses, before it reports `NotReady`.
const STARTUP_READY_GRACE_MS: u64 = 5_000;
/// How often a tmux-wrapped controller task reads its pane title through tmux.
const STARTUP_TITLE_POLL_MS: u64 = 250;
const SHIFT_TAB: &[u8] = b"\x1b[Z";

/// A controller task's readiness deadline. The settle loop may use its whole
/// cap (a UI that never goes quiet) and the configured delay; the auto-mode
/// keys, the pre-type and pre-submit pauses that follow, and one readiness
/// window must still fit, or a slow UI would always be refused at its write.
fn controlled_startup_deadline(startup_delay_ms: u32, auto_mode_presses: u8) -> Duration {
    let keys = match auto_mode_presses {
        0 => 0,
        presses => (u64::from(presses) + 1) * STARTUP_KEY_GAP_MS,
    };
    Duration::from_millis(
        STARTUP_MAX_WAIT_MS
            + u64::from(startup_delay_ms)
            + keys
            + STARTUP_PRE_TYPE_MS
            + STARTUP_SUBMIT_DELAY_MS
            + STARTUP_READY_GRACE_MS,
    )
}

/// The terminal side of [`quiet_startup`], so its timing can be tested
/// without a PTY.
trait QuietStartupTarget {
    fn has_output(&self) -> bool;
    fn idle_for(&self) -> Duration;
    fn write_input(&self, data: &[u8]);
    fn paste(&self, text: &str);
    fn mark_turn_submitted(&self);
}

impl QuietStartupTarget for TerminalSession {
    fn has_output(&self) -> bool {
        TerminalSession::has_output(self)
    }
    fn idle_for(&self) -> Duration {
        TerminalSession::idle_for(self)
    }
    fn write_input(&self, data: &[u8]) {
        TerminalSession::write_input(self, data)
    }
    fn paste(&self, text: &str) {
        TerminalSession::paste(self, text)
    }
    fn mark_turn_submitted(&self) {
        TerminalSession::mark_prompt_submitted(self)
    }
}

struct QuietStartup {
    input: Option<String>,
    auto_mode_presses: u8,
    startup_delay_ms: u32,
    submit: bool,
}

/// Startup automation for runners, TypeIn presets and loops — everything but a
/// controller task, which has its own verified protocol.
///
/// "Ready" = the child has produced output AND has then been quiet for
/// SETTLE_MS. This adapts to slow starters (e.g. opencode, whose first output —
/// clearing the screen — comes early but whose input box only appears seconds
/// later, once it stops emitting): we wait for the load output to actually stop,
/// not a guessed delay. SETTLE_MS is generous so a brief pause mid-load isn't
/// mistaken for ready. Capped by MAX_WAIT in case a UI never goes quiet: the
/// input is still typed at the cap. No provider title or composer is required,
/// so this works the same under tmux, for any agent and any Codex footer.
async fn quiet_startup<T: QuietStartupTarget + ?Sized, F: std::future::Future<Output = ()>>(
    target: &T,
    mut timer: impl FnMut(u64) -> F,
    plan: QuietStartup,
    mut event: impl FnMut(&'static str, usize),
) {
    // Wait for the agent's first output (it has started up).
    let mut waited = 0u64;
    while !target.has_output() && waited < STARTUP_MAX_WAIT_MS {
        timer(STARTUP_POLL_MS).await;
        waited += STARTUP_POLL_MS;
    }
    event("automation-first-output", 0);
    if plan.startup_delay_ms > 0 {
        // Preset-configured fixed delay after first output — for agents that
        // keep loading well past their first draw (e.g. opencode).
        timer(plan.startup_delay_ms as u64).await;
    } else {
        // Auto: wait until output goes quiet (UI finished drawing).
        while waited < STARTUP_MAX_WAIT_MS && target.idle_for().as_millis() < STARTUP_SETTLE_MS {
            timer(STARTUP_POLL_MS).await;
            waited += STARTUP_POLL_MS;
        }
    }
    event("automation-settled", 0);
    for _ in 0..plan.auto_mode_presses {
        target.write_input(SHIFT_TAB);
        timer(STARTUP_KEY_GAP_MS).await;
    }
    // Confirm the mode switch with a single Enter.
    if plan.auto_mode_presses > 0 {
        target.write_input(b"\r");
        timer(STARTUP_KEY_GAP_MS).await;
    }
    if let Some(input) = plan.input {
        timer(STARTUP_PRE_TYPE_MS).await;
        target.paste(&input);
        event("automation-pasted", input.len());
        // On restore, leave the prompt typed but unsubmitted.
        if plan.submit {
            timer(STARTUP_SUBMIT_DELAY_MS).await;
            target.mark_turn_submitted();
            target.write_input(b"\r");
            event("automation-submitted", 1);
        }
    }
}

fn startup_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

async fn wait_startup_composer(
    view: &WeakEntity<TerminalView>,
    session: &Arc<TerminalSession>,
    cx: &mut AsyncApp,
    guard: &mut crate::startup::Guard,
    armed_at: Instant,
    empty: bool,
    probe: Option<&crate::startup::TitleProbe>,
) -> Result<(), crate::startup::Stop> {
    loop {
        let ready = with_startup_view(view, session, cx, |view| {
            let screen = session.startup_screen();
            let title = startup_title(session, probe).1;
            guard.ready(
                armed_at.elapsed(),
                session.input_rev(),
                startup_ready(
                    view.title_provider,
                    &screen,
                    title.as_deref(),
                    &view.blocked_markers,
                    empty,
                ),
            )
        })??;
        if ready {
            return Ok(());
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
}

fn write_startup_input(
    view: &WeakEntity<TerminalView>,
    session: &Arc<TerminalSession>,
    cx: &mut AsyncApp,
    guard: &mut crate::startup::Guard,
    armed_at: Instant,
    submit: bool,
    write: impl FnOnce(&TerminalSession) -> std::io::Result<()>,
) -> Result<(), crate::startup::Stop> {
    with_startup_view(view, session, cx, |_| {
        let admitted_at = startup_now_ms();
        guard.write(armed_at.elapsed(), session.input_rev(), || write(session))?;
        if submit {
            guard.enter_admitted(admitted_at);
        }
        Ok(())
    })?
}

/// Why the drain loop most recently notified GPUI to request a redraw.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalNotifyCause {
    Immediate,
    Timer,
}

/// Content-free state sampled only when the app-level UI profiler observes a
/// focus-path failure. These counters describe notification scheduling, not
/// completion of a paint or native presentation.
#[derive(Clone, Copy, Debug)]
pub struct TerminalFocusProfile {
    pub content_generation: u64,
    pub notify_generation: u64,
    pub last_notify_age: Option<Duration>,
    pub last_notify_cause: Option<TerminalNotifyCause>,
    pub notify_pending: bool,
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        if let Some(startup) = &self.startup_turn {
            startup.cancel();
        }
        profile::pane_closed(self.instance_id);
    }
}

/// A spawned terminal not yet wrapped in a view: the spec that actually ran
/// (the requested one, or the fallback shell), the live session + its output
/// receiver, and the launch error when the requested program failed to start.
/// Splitting the fallible spawn from the (infallible) gpui entity construction
/// is what lets a total launch failure surface as an error instead of a panic.
pub struct TerminalLaunch {
    spec: CommandSpec,
    rx: ProfiledPtyReceiver,
    session: Arc<TerminalSession>,
    launch_error: Option<String>,
}

/// Owns the two resources captured by the output-drain task in teardown order.
/// Rust drops struct fields in declaration order: closing the receiver first
/// wakes a backpressured PTY reader before the final session owner closes the
/// platform PTY.
struct DrainTransport {
    rx: ProfiledPtyReceiver,
    session: Arc<TerminalSession>,
}

impl TerminalLaunch {
    /// Spawn `spec` at `size` (`(cols, rows)`); if it can't be launched (e.g. the
    /// agent isn't installed), fall back to a shell that prints the error. Literal
    /// commands and outside-created panes surface their error without replacing
    /// the program. Other launches return `Err` only when even the fallback shell
    /// can't spawn.
    ///
    /// `size` should be the grid the pane will actually render at — see
    /// [`TerminalSession::size`]. Getting it right up front is what keeps a
    /// `tmux attach` from painting its first frame at the wrong size.
    pub fn spawn(spec: CommandSpec, size: (u16, u16)) -> anyhow::Result<Self> {
        Self::spawn_with_fallback(spec, CommandSpec::shell(), size, None)
    }

    /// Spawn for one pane, attributing PTY timings to it and giving any local
    /// fallback shell that pane's identity.
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
        let spawn = |spec| TerminalSession::spawn_internal(spec, cols, rows, instance_id);
        match spawn(spec.clone()) {
            Ok((session, rx)) => Ok(Self {
                spec,
                session,
                rx,
                launch_error: None,
            }),
            Err(e) if spec.literal_argv || !spec.allow_shell_fallback => Err(e),
            Err(e) => {
                // Capture the full error (incl. the OS code) for the dev console.
                let launch_error = format!("{e:#}");
                let prog = spec.program.replace(['\'', '"'], "");
                // `{e:#}` includes the full anyhow context chain (e.g. the real
                // OS error: "No such file or directory"), not just the top context.
                let detail = launch_error.replace(['\'', '"', '\n', '\r'], " ");
                let mut shell = fallback.with_startup_input(format!(
                    "printf '%s\\n' 'muxel: could not launch {prog}: {detail}'"
                ));
                if let Some(instance_id) = instance_id {
                    // The fallback is a local program even when the failed command
                    // was ssh or tmux. Give it only its own id, not transport env.
                    shell.env.push((
                        muxel_core::control::INSTANCE_ID_ENV.to_string(),
                        instance_id.to_string(),
                    ));
                }
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
    fn schedule_paint(&mut self, min_interval: Duration, defer: Duration, cx: &mut Context<Self>) {
        let now = Instant::now();
        let schedule = next_paint_schedule(
            self.last_paint_notify.get(),
            self.pending_paint_deadline.get(),
            now,
            min_interval,
            defer,
        );
        self.paint_hold_until
            .set(hold_after(schedule, defer, self.paint_hold_until.get()));
        let deadline = match schedule {
            PaintSchedule::Now => {
                self.pending_paint_deadline.set(None);
                self.paint_timer_generation
                    .set(self.paint_timer_generation.get().wrapping_add(1));
                self.last_paint_notify.set(now);
                if self.profile_focus {
                    self.profile_notify_generation
                        .set(self.profile_notify_generation.get().wrapping_add(1));
                    self.profile_last_notify_cause
                        .set(Some(TerminalNotifyCause::Immediate));
                }
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
                view.paint_hold_until.set(None);
                view.last_paint_notify.set(Instant::now());
                if view.profile_focus {
                    view.profile_notify_generation
                        .set(view.profile_notify_generation.get().wrapping_add(1));
                    view.profile_last_notify_cause
                        .set(Some(TerminalNotifyCause::Timer));
                }
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
        Self::new_with_focus_observer(launch, instance_id, startup_started, None, window, cx)
    }

    /// [`Self::new`] with an optional app-owned diagnostic at the exact GPUI
    /// focus edge. Kept separate so the normal public constructor remains
    /// source-compatible for other workspace consumers.
    pub fn new_with_focus_observer(
        launch: TerminalLaunch,
        instance_id: Uuid,
        startup_started: Instant,
        focus_observer: Option<TerminalFocusObserver>,
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
        let startup_owner = spec.startup_turn.clone();
        let title_source = spec.title_source.clone();
        let startup_turn = startup_owner
            .as_ref()
            .map(muxel_core::control::StartupTurnOwner::signal);
        let auto_mode_presses = spec.auto_mode_presses;
        let startup_delay_ms = spec.startup_delay_ms;
        let submit = spec.submit;
        let working_markers = spec.working_markers.clone();
        let blocked_markers = spec.blocked_markers.clone();
        let startup_program = spec.program.clone();
        let title_provider =
            TitleProvider::from_program(spec.status_program.as_deref().unwrap_or(&startup_program));
        let muse_cwd_label = spec
            .cwd
            .as_deref()
            .filter(|_| title_provider == TitleProvider::Muse)
            .and_then(|cwd| muxel_core::muse::cwd_label(std::path::Path::new(cwd)))
            .map(str::to_string);
        let focus_handle = cx.focus_handle();

        // Forward focus in/out to the PTY (DECSET 1004) so agents like Claude
        // know when their pane is the one the user is looking at — and only
        // notify when it isn't.
        //
        // These MUST be the `Context` (entity-scoped) subscriptions, never
        // `window.on_focus_in(…).detach()`. A window-scoped focus listener lives
        // as long as the *window*, and a closure capturing `session.clone()`
        // therefore pins the `TerminalSession` — and its PTY master fd, its
        // writer-thread fd dup, and that parked thread — for the rest of the
        // process. Remote panes respawn on every dropped SSH connection, so that
        // leaked ~2 fds per reconnect until the whole app hit EMFILE ("Too many
        // open files") and could no longer spawn anything. The entity-scoped
        // form holds only a `WeakEntity` and prunes itself once the view is gone.
        cx.on_focus_in(&focus_handle, window, move |view, window, cx| {
            profile::focus_changed(instance_id, true);
            if let Some(observer) = focus_observer {
                observer(instance_id, true, window, cx);
            }
            view.session.report_focus(true);
        })
        .detach();
        cx.on_focus_out(&focus_handle, window, move |view, _ev, window, cx| {
            profile::focus_changed(instance_id, false);
            if let Some(observer) = focus_observer {
                observer(instance_id, false, window, cx);
            }
            view.session.report_focus(false);
        })
        .detach();

        // Startup automation (runners + type-in injection): once the agent is
        // ready, optionally send Shift+Tab a few times to reach auto-accept mode,
        // press Enter to confirm it, then type the prompt and press Enter.
        //
        // Runners, TypeIn presets and loops keep the quiet-then-type behavior
        // ([`quiet_startup`]). A controller task (`ctl new --prompt`) instead
        // needs its provider's verified composer, then independent acceptance:
        // silence is not an editable composer.
        if let Some(input) = startup_input.as_ref() {
            profile::startup_event(
                instance_id,
                &startup_program,
                "automation-armed",
                startup_started.elapsed(),
                input.len(),
            );
        }
        if (auto_mode_presses > 0 || startup_input.is_some())
            && let Some(owner) = startup_owner.clone()
        {
            let session = session.clone();
            let automation_program = startup_program.clone();
            let known_provider = title_provider.has_startup_protocol();
            let framed_readiness =
                matches!(title_provider, TitleProvider::Claude | TitleProvider::Codex);
            let armed_at = Instant::now();
            let deadline = controlled_startup_deadline(startup_delay_ms, auto_mode_presses);
            let mut guard = crate::startup::Guard::new(session.input_rev(), Some(deadline))
                .with_control_turn(Some(owner));
            // Only a task that is still waiting pays for reading the title
            // through tmux, and only until its startup settles either way.
            let probe = title_source.map(|source| {
                crate::startup::TitleProbe::spawn(
                    source,
                    Duration::from_millis(STARTUP_TITLE_POLL_MS),
                    deadline + crate::startup::ACCEPTANCE_WINDOW + Duration::from_secs(5),
                )
            });
            cx.spawn(async move |view: WeakEntity<Self>, cx| {
                let probe = probe.as_ref();
                let result: Result<(), crate::startup::Stop> = async {
                    if !known_provider {
                        guard.fail(crate::startup::Stop::UnsupportedProvider);
                        return Err(crate::startup::Stop::UnsupportedProvider);
                    }
                    // Wait for the agent's first output (it has started up).
                    let mut waited = 0u64;
                    loop {
                        let has_output = with_startup_view(&view, &session, cx, |_| {
                            guard.check(armed_at.elapsed(), session.input_rev())?;
                            Ok::<_, crate::startup::Stop>(session.has_output())
                        })??;
                        if has_output || waited >= STARTUP_MAX_WAIT_MS {
                            break;
                        }
                        cx.background_executor()
                            .timer(Duration::from_millis(STARTUP_POLL_MS))
                            .await;
                        waited += STARTUP_POLL_MS;
                    }
                    profile::startup_event(
                        instance_id,
                        &automation_program,
                        "automation-first-output",
                        startup_started.elapsed(),
                        0,
                    );
                    if startup_delay_ms > 0 {
                        // Preset-configured fixed delay after first output — for agents
                        // that keep loading well past their first draw (e.g. opencode).
                        cx.background_executor()
                            .timer(Duration::from_millis(startup_delay_ms as u64))
                            .await;
                    } else if !framed_readiness {
                        // Auto: wait until output goes quiet (UI finished drawing).
                        while waited < STARTUP_MAX_WAIT_MS
                            && session.idle_for().as_millis() < STARTUP_SETTLE_MS
                        {
                            with_startup_view(&view, &session, cx, |_| {
                                guard.check(armed_at.elapsed(), session.input_rev())
                            })??;
                            cx.background_executor()
                                .timer(Duration::from_millis(STARTUP_POLL_MS))
                                .await;
                            waited += STARTUP_POLL_MS;
                        }
                    }
                    wait_startup_composer(&view, &session, cx, &mut guard, armed_at, true, probe)
                        .await?;
                    profile::startup_event(
                        instance_id,
                        &automation_program,
                        "automation-settled",
                        startup_started.elapsed(),
                        0,
                    );
                    for _ in 0..auto_mode_presses {
                        write_startup_input(
                            &view,
                            &session,
                            cx,
                            &mut guard,
                            armed_at,
                            false,
                            |session| session.try_write_input(SHIFT_TAB),
                        )?;
                        cx.background_executor()
                            .timer(Duration::from_millis(STARTUP_KEY_GAP_MS))
                            .await;
                    }
                    // Confirm the mode switch with a single Enter.
                    if auto_mode_presses > 0 {
                        write_startup_input(
                            &view,
                            &session,
                            cx,
                            &mut guard,
                            armed_at,
                            false,
                            |session| session.try_write_input(b"\r"),
                        )?;
                        cx.background_executor()
                            .timer(Duration::from_millis(STARTUP_KEY_GAP_MS))
                            .await;
                    }
                    if let Some(input) = startup_input {
                        cx.background_executor()
                            .timer(Duration::from_millis(STARTUP_PRE_TYPE_MS))
                            .await;
                        guard.reset_ready();
                        wait_startup_composer(
                            &view, &session, cx, &mut guard, armed_at, true, probe,
                        )
                        .await?;
                        write_startup_input(
                            &view,
                            &session,
                            cx,
                            &mut guard,
                            armed_at,
                            false,
                            |session| session.try_paste(&input),
                        )?;
                        profile::startup_event(
                            instance_id,
                            &automation_program,
                            "automation-pasted",
                            startup_started.elapsed(),
                            input.len(),
                        );
                        // On restore, leave the prompt typed but unsubmitted.
                        if submit {
                            cx.background_executor()
                                .timer(Duration::from_millis(STARTUP_SUBMIT_DELAY_MS))
                                .await;
                            guard.reset_ready();
                            wait_startup_composer(
                                &view, &session, cx, &mut guard, armed_at, false, probe,
                            )
                            .await?;
                            let attempt = StartupAttempt {
                                provider: title_provider,
                                input: &input,
                                expected_input: crate::startup::normalized(&input),
                                before_title: startup_title(&session, probe).0,
                                allow_working_title: true,
                                before_prompt: muxel_core::readaloud::prompt_from_screen(
                                    &session.recent_text(500),
                                )
                                .map(|prompt| crate::startup::normalized(&prompt)),
                                instance_id,
                            };
                            write_startup_input(
                                &view,
                                &session,
                                cx,
                                &mut guard,
                                armed_at,
                                true,
                                |session| session.try_submit_input(b"\r"),
                            )?;
                            profile::startup_event(
                                instance_id,
                                &automation_program,
                                "automation-enter-admitted",
                                startup_started.elapsed(),
                                1,
                            );
                            confirm_startup_submission(
                                &view, &session, cx, &mut guard, armed_at, &attempt, probe,
                            )
                            .await?;
                            profile::startup_event(
                                instance_id,
                                &automation_program,
                                "automation-submitted",
                                startup_started.elapsed(),
                                0,
                            );
                        }
                    }
                    Ok(())
                }
                .await;
                if let Err(reason) = result {
                    let phase = match reason {
                        crate::startup::Stop::Interrupted => "automation-interrupted",
                        crate::startup::Stop::NotReady => "automation-not-ready",
                        crate::startup::Stop::NotAccepted => "automation-not-accepted",
                        crate::startup::Stop::UnsupportedProvider => {
                            "automation-unsupported-provider"
                        }
                        crate::startup::Stop::WriteFailed => "automation-write-failed",
                    };
                    profile::startup_event(
                        instance_id,
                        &automation_program,
                        phase,
                        startup_started.elapsed(),
                        0,
                    );
                    log::warn!("terminal startup automation stopped: {reason:?}");
                }
            })
            .detach();
        } else if auto_mode_presses > 0 || startup_input.is_some() {
            let session = session.clone();
            let automation_program = startup_program.clone();
            let plan = QuietStartup {
                input: startup_input,
                auto_mode_presses,
                startup_delay_ms,
                submit,
            };
            cx.spawn(async move |_view: WeakEntity<Self>, cx| {
                let executor = cx.background_executor().clone();
                quiet_startup(
                    &*session,
                    |ms| executor.timer(Duration::from_millis(ms)),
                    plan,
                    |phase, bytes| {
                        profile::startup_event(
                            instance_id,
                            &automation_program,
                            phase,
                            startup_started.elapsed(),
                            bytes,
                        )
                    },
                )
                .await;
            })
            .detach();
        }

        let drain_transport = DrainTransport {
            rx,
            session: session.clone(),
        };
        let drain = cx.spawn(async move |view: WeakEntity<Self>, cx| {
            let transport = drain_transport;
            let mut first_output = true;
            loop {
                let chunk = match transport.rx.recv().await {
                    Ok(c) => c,
                    Err(_) => break,
                };

                let mut output = PtyOutput::default();
                let mut exit: Option<ExitInfo> = None;
                match chunk {
                    ProfiledPtyChunk::Output {
                        bytes,
                        timing,
                        read_at,
                    } => {
                        if let Some(timing) = timing {
                            profile::output_drained(
                                instance_id,
                                timing,
                                transport.rx.take_profile_peak(),
                            );
                        }
                        output.push(&bytes, read_at);
                    }
                    ProfiledPtyChunk::Exit {
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
                coalesce_pending(&transport.rx, instance_id, &mut output, &mut exit);

                // Coalesce before taking the UI lock: bg agents always; focused
                // stream bursts too (interaction priority is decided after parsing).
                let focused_hint = transport.session.is_focused();
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
                        coalesce_pending(&transport.rx, instance_id, &mut output, &mut exit);
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
                if batch_len > 0 {
                    profile::output_update_requested(instance_id);
                }
                let stop = view
                    .update(cx, |view, cx| {
                        let focused = view.session.is_focused();
                        // Sampled around `process_drained` so only the *transition*
                        // to hidden defers: a program that keeps its cursor hidden
                        // throughout is never slowed down.
                        let cursor_was_visible = view.session.cursor_visible();
                        if !output.is_empty() {
                            profile::output_update_started(instance_id);
                            let t0 = Instant::now();
                            view.session.process_drained(&output);
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
                                let (col, row) = view.session.cursor_position();
                                profile::cursor_probe_update(instance_id, col, row);
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
                        let runtime_changed = view.update_runtime(cx);
                        // Priority scheduler (see docs/terminal-paint-architecture.md):
                        // Recent input ≫ stream; every throttled batch gets one
                        // trailing-edge notify if no later output arrives.
                        let interactive = view.session.is_interactive();
                        let min_interval = paint_min_interval(focused, interactive, stop);
                        // An exit must be drawn at once; otherwise let a batch that
                        // just hid the cursor wait for the show that follows it.
                        let defer = if !stop && cursor_was_visible && !view.session.cursor_visible()
                        {
                            CURSOR_HIDE_HOLD
                        } else {
                            Duration::ZERO
                        };
                        // An earlier batch's hold still matters while the cursor
                        // it hid stays hidden; once shown, there's no frame to skip.
                        let held_for = if view.session.cursor_visible() {
                            Duration::ZERO
                        } else {
                            status_paint_defer(view.paint_hold_until.get(), Instant::now())
                        };
                        let (min_interval, defer) =
                            paint_request(runtime_changed, min_interval, defer, held_for);
                        view.schedule_paint(min_interval, defer, cx);
                        stop
                    })
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
        });

        let runtime_events = session.runtime_events();
        let runtime_event_task = cx.spawn(async move |view: WeakEntity<Self>, cx| {
            while runtime_events.recv().await.is_ok() {
                if view
                    .update(cx, |view, cx| view.refresh_runtime(cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let coverage = status_coverage(title_provider, &working_markers, &blocked_markers);
        let mut view = Self {
            instance_id,
            session,
            startup_turn,
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
            runtime: RuntimeStatusState {
                muse_cwd_label,
                ..RuntimeStatusState::new(coverage)
            },
            last_paint_notify: std::cell::Cell::new(std::time::Instant::now()),
            profile_notify_generation: std::cell::Cell::new(0),
            profile_last_notify_cause: std::cell::Cell::new(None),
            profile_focus: focus_observer.is_some(),
            pending_paint_deadline: std::cell::Cell::new(None),
            paint_timer_generation: std::cell::Cell::new(0),
            paint_hold_until: std::cell::Cell::new(None),
            _drain: drain,
            _runtime_events: runtime_event_task,
        };
        view.refresh_runtime(cx);
        view
    }

    pub fn session(&self) -> &Arc<TerminalSession> {
        &self.session
    }

    /// Snapshot evidence before the caller pastes. This never marks a turn as
    /// submitted and does not authorize any subsequent write.
    pub fn begin_prompt_submission(&self, text: &str) -> Option<PromptSubmission> {
        if self.exited
            || !matches!(
                self.title_provider,
                TitleProvider::Codex | TitleProvider::Claude | TitleProvider::Grok
            )
            || text.trim().is_empty()
        {
            return None;
        }
        let binding_revision = self.session.binding_revision();
        let resize_revision = self.session.resize_revision();
        let (before_title, title) = self.session.title_snapshot();
        let screen = self.session.startup_screen();
        let allow_working_title = ctl_title_can_confirm(
            self.title_provider,
            &screen,
            title.as_deref(),
            self.status(),
        );
        let before_prompt = ctl_transcript_prompt(self.title_provider, &screen, || {
            self.session.recent_text(500)
        })
        .map(|prompt| crate::startup::normalized(&prompt));
        let origin = if cfg!(windows)
            && self.title_provider == TitleProvider::Codex
            && codex_ctl_ready(
                &screen,
                title.as_deref(),
                [&self.working_markers, &self.blocked_markers],
                true,
            ) {
            screen
                .codex_ctl_origin()
                .filter(|_| {
                    before_title == self.session.title_snapshot().0
                        && resize_revision == self.session.resize_revision()
                        && binding_revision == self.session.binding_revision()
                })
                .map(|frame| PromptOriginProof {
                    frame,
                    title_generation: before_title,
                    resize_revision,
                })
        } else {
            None
        };
        let observable = origin.is_some()
            || ctl_acceptance_observable(
                self.title_provider,
                title.as_deref(),
                allow_working_title,
                text,
                before_prompt.as_deref(),
            );
        // Preserve this generation even if it changes while reading history.
        // Returning None would incorrectly downgrade a switch to an unsupported
        // provider and skip the caller's pre-Enter interruption check.
        Some(PromptSubmission {
            session: Arc::downgrade(&self.session),
            binding_revision,
            provider: self.title_provider,
            instance_id: self.instance_id,
            input: text.to_owned(),
            expected_input: crate::startup::normalized(text),
            before_title,
            allow_working_title,
            before_prompt,
            origin,
            observable,
        })
    }

    /// Observe real provider receipts. Retryable is only a Windows Codex draft
    /// proof; the app must still recheck lifetime, permissions and input revision.
    pub fn observe_prompt_submission(&self, snapshot: &PromptSubmission) -> PromptSubmissionState {
        let current = || {
            !self.exited
                && snapshot.instance_id == self.instance_id
                && snapshot.provider == self.title_provider
                && snapshot.session.ptr_eq(&Arc::downgrade(&self.session))
                && snapshot.binding_revision == self.session.binding_revision()
        };
        if !current() {
            return PromptSubmissionState::Interrupted;
        }
        let resize_revision = self.session.resize_revision();
        let (generation, title) = self.session.title_snapshot();
        let screen = self.session.startup_screen();
        let prompt = ctl_transcript_prompt(self.title_provider, &screen, || {
            self.session.recent_text(500)
        });
        let attempt = StartupAttempt {
            provider: snapshot.provider,
            input: &snapshot.input,
            expected_input: snapshot.expected_input.clone(),
            before_title: snapshot.before_title,
            allow_working_title: snapshot.allow_working_title,
            before_prompt: snapshot.before_prompt.clone(),
            instance_id: snapshot.instance_id,
        };
        let observed = prompt_submission_state(
            &attempt,
            &screen,
            title.as_deref(),
            generation,
            prompt.as_deref(),
            [&self.working_markers, &self.blocked_markers],
            snapshot
                .origin
                .as_ref()
                .filter(|proof| proof.resize_revision == resize_revision),
        );
        if current()
            && observed == PromptSubmissionState::Retryable
            && (resize_revision != self.session.resize_revision()
                || generation != self.session.title_snapshot().0)
        {
            PromptSubmissionState::Unconfirmed
        } else if current() {
            observed
        } else {
            PromptSubmissionState::Interrupted
        }
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

    /// Snapshot fixed-size redraw correlation state for an app-level focus-path
    /// loss. Callers keep this behind the opt-in profiler gate.
    pub fn focus_profile(&self) -> TerminalFocusProfile {
        let last_notify_cause = self.profile_last_notify_cause.get();
        TerminalFocusProfile {
            content_generation: self.session.content_generation(),
            notify_generation: self.profile_notify_generation.get(),
            last_notify_age: last_notify_cause.map(|_| self.last_paint_notify.get().elapsed()),
            last_notify_cause,
            notify_pending: self.pending_paint_deadline.get().is_some(),
        }
    }

    /// Reconcile runtime state at an actual observation edge, and repaint when
    /// the public snapshot changed: at once, ahead of the throttle, unless a
    /// cursor-hide hold is pending, whose paint then draws the change.
    fn refresh_runtime(&mut self, cx: &mut Context<Self>) {
        if self.update_runtime(cx) {
            let defer = status_paint_defer(self.paint_hold_until.get(), Instant::now());
            self.schedule_paint(Duration::ZERO, defer, cx);
        }
    }

    /// Reconcile runtime state at an actual observation edge. This is the only
    /// place that scans the visible grid or advances the lifecycle reducer. The
    /// facts are current for readers on return; the caller decides when to paint.
    /// Returns whether the public snapshot changed.
    fn update_runtime(&mut self, cx: &mut Context<Self>) -> bool {
        let previous_binding = self.runtime.binding_rev;
        let (changed, deadline) = self.runtime.reconcile(
            &self.session,
            self.title_provider,
            &self.working_markers,
            &self.blocked_markers,
            self.exited,
            Instant::now(),
        );
        self.schedule_runtime_deadline(deadline, previous_binding != self.runtime.binding_rev, cx);
        changed
    }

    fn schedule_runtime_deadline(
        &mut self,
        deadline: Option<Instant>,
        identity_changed: bool,
        cx: &mut Context<Self>,
    ) {
        if !identity_changed {
            match (self.runtime.deadline, deadline) {
                (current, next) if current == next => return,
                (Some(current), Some(next)) if current <= next => return,
                _ => {}
            }
        }
        self.runtime.timer_generation = self.runtime.timer_generation.wrapping_add(1);
        let generation = self.runtime.timer_generation;
        self.runtime.deadline = deadline;
        let Some(deadline) = deadline else {
            return;
        };
        let delay = runtime_timer_delay(deadline, Instant::now());
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            cx.background_executor().timer(delay).await;
            let _ = view.update(cx, |view, cx| {
                if !runtime_timer_is_current(view.runtime.timer_generation, generation) {
                    return;
                }
                view.runtime.deadline = None;
                view.refresh_runtime(cx);
            });
        })
        .detach();
    }

    pub fn runtime_snapshot(&self) -> TerminalRuntimeSnapshot {
        self.runtime.snapshot(&self.session)
    }

    /// Pure immutable projection used by render/sidebar callers.
    pub fn status(&self) -> AgentStatus {
        self.runtime.status
    }

    /// Whether [`Self::status`] can be attributed to the current provider
    /// conversation. It can't while a binding switched out of band settles and
    /// the status rests on evidence that may be the old conversation's; the
    /// app then neither notifies for it nor records it as the last status seen.
    pub fn status_attributable(&self) -> bool {
        self.runtime_snapshot()
            .is_attributable_to(self.session.binding_revision())
    }

    /// Whether this terminal has observed provider-owned Ready/Idle or a
    /// submitted turn. Fallback PTY quiet is not startup-readiness evidence.
    pub fn completion_ready(&self) -> bool {
        self.runtime_snapshot().completion_ready
    }

    /// Report the latest hook from this Muse pane's root session.
    pub fn set_muse_hook_state(
        &mut self,
        metadata: &muxel_core::muse::MuseMetadata,
        cx: &mut Context<Self>,
    ) {
        let label = muxel_core::muse::cwd_label(&metadata.cwd).map(str::to_string);
        let state = Some(metadata.state);
        if self.runtime.muse_cwd_label != label || self.runtime.muse_hook_state != state {
            self.runtime.muse_cwd_label = label;
            self.runtime.muse_hook_state = state;
            self.refresh_runtime(cx);
        }
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

    /// The last `max_lines` lines of scrollback + screen as text, soft wraps
    /// rejoined. Used by read-aloud to find an agent's last reply.
    pub fn recent_text(&self, max_lines: usize) -> String {
        self.session.recent_text(max_lines)
    }

    pub fn title(&self) -> Option<String> {
        self.session.title()
    }

    /// A name authorized by the current program-status root, independent of status.
    pub fn program_auto_title(&self) -> Option<String> {
        program_auto_title(&self.session, self.title_provider, self.exited)
    }

    /// See [`TerminalSession::provider_switched`].
    pub fn provider_switched(&self, from: std::time::SystemTime, to: std::time::SystemTime) {
        self.session.provider_switched(from, to);
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

    /// Link under the last pointer position, if the terminal grid can resolve one.
    pub fn link_at_pointer(&self) -> Option<String> {
        let hit = self.session.pointer_hit()?;
        crate::element::link_at(
            &self.session,
            point(px(hit.local_x), px(hit.local_y)),
            px(hit.cell_width),
            px(hit.line_height),
            hit.cols,
            hit.rows,
        )
        .map(|link| link.url)
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
            if is_submission_input(&bytes) {
                self.session.mark_turn_submitted();
            }
            profile::key_started(self.instance_id, t0);
            self.session.write_user_input(&bytes);
            if cleared {
                cx.notify();
            }
            // Key path: gpui may sync-draw without presenting — arm the pump.
            crate::present_flag::mark_present_needed();
            cx.stop_propagation();
            profile::key_finished(held, t0.elapsed());
        }
    }
}

/// A paste that names this machine's files rather than carrying text: what a
/// [`LocalPasteHandler`] receives.
#[derive(Clone, Debug)]
pub enum LocalPaste {
    /// An image on the clipboard (a screenshot, an image copied from a browser).
    Image(Image),
    /// Files copied in a file manager, or dropped on the pane.
    Paths(Vec<std::path::PathBuf>),
}

/// Takes over a pane's [`LocalPaste`]s (see
/// [`TerminalSession::set_local_paste_handler`]). Called on the UI thread; whatever
/// it pastes into the pane it does later, through the session, once it is ready.
pub type LocalPasteHandler = Arc<dyn Fn(LocalPaste, &App) + Send + Sync>;

/// Report a refused local file list without deferring successful terminal input.
pub type PathPasteErrorHandler =
    Arc<dyn Fn(muxel_core::path_paste::PastePathError, &App) + Send + Sync>;

/// Paste from the system clipboard into the PTY.
/// Image → forward Ctrl+V (0x16) so the agent reads the OS clipboard.
/// File paths → shell-quoted paths. Text → bracketed paste.
///
/// A pane with a [`LocalPasteHandler`] hands images and file paths to it instead:
/// the far side of an ssh pane can't read this machine's clipboard or its files.
pub fn paste_clipboard_into_session(session: &TerminalSession, cx: &App) {
    let Some(item) = cx.read_from_clipboard() else {
        return;
    };
    // Only ever a user's own paste (a key or a mouse button), never automation.
    session.mark_user_input();
    for entry in item.entries() {
        match entry {
            ClipboardEntry::Image(image) if !image.bytes.is_empty() => {
                match session.local_paste_handler() {
                    Some(handler) => handler(LocalPaste::Image(image.clone()), cx),
                    None => session.write_input(&[0x16]),
                }
                return;
            }
            ClipboardEntry::ExternalPaths(paths) => {
                let paths: Vec<_> = paths.paths().to_vec();
                if !paths.is_empty() {
                    paste_local_paths(session, paths, cx);
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

/// Paste local files into the pane: their quoted paths, or — when the pane has a
/// [`LocalPasteHandler`] — whatever it makes of them.
fn paste_local_paths(session: &TerminalSession, paths: Vec<std::path::PathBuf>, cx: &App) {
    match session.local_paste_handler() {
        Some(handler) => handler(LocalPaste::Paths(paths), cx),
        None => {
            if let Err(error) = session.paste_paths(&paths) {
                match session.path_paste_error_handler() {
                    Some(handler) => handler(error, cx),
                    None => log::warn!("file paste refused: {error:?}"),
                }
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
            // OS file drops (Explorer → pane) arrive as an internal gpui drag of
            // ExternalPaths, not as FileDropEvent listeners. Same path Zed uses.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.session.mark_user_input();
                paste_local_paths(&this.session, paths.paths().to_vec(), cx);
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

#[cfg(test)]
mod launch_identity_tests {
    use super::TerminalLaunch;
    use crate::session::{CommandSpec, ProfiledPtyChunk};
    use muxel_core::control::INSTANCE_ID_ENV;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    #[test]
    fn outside_startup_launch_failure_refuses_a_real_fallback_and_cancels_its_task() {
        use muxel_core::control::{StartupTurn, StartupTurnState};
        for allow_fallback in [true, false] {
            let marker = std::env::temp_dir().join(format!("muxel-fallback-{}", Uuid::new_v4()));
            #[cfg(windows)]
            let fallback = CommandSpec::program(
                "powershell.exe",
                vec![
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    format!(
                        "[System.IO.File]::WriteAllText('{}', 'fallback')",
                        marker.display().to_string().replace('\'', "''")
                    ),
                ],
            );
            #[cfg(not(windows))]
            let fallback = CommandSpec::program(
                "/bin/sh",
                vec![
                    "-c".into(),
                    "printf fallback > \"$1\"".into(),
                    "muxel-test".into(),
                    marker.display().to_string(),
                ],
            );
            let signal = StartupTurn::default();
            let mut requested = CommandSpec::program(
                std::env::temp_dir()
                    .join(format!("muxel-missing-{}.exe", Uuid::new_v4()))
                    .display()
                    .to_string(),
                vec![],
            );
            requested.allow_shell_fallback = allow_fallback;
            if !allow_fallback {
                requested.startup_turn = signal.claim();
            }
            let result = TerminalLaunch::spawn_with_fallback(requested, fallback, (80, 24), None);
            if allow_fallback {
                let launch = result.expect("ordinary UI preset retains its fallback");
                let deadline = Instant::now() + Duration::from_secs(15);
                let marker_complete = loop {
                    if std::fs::read_to_string(&marker).ok().as_deref() == Some("fallback") {
                        break true;
                    }
                    if Instant::now() >= deadline {
                        break false;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                // File creation precedes the completed write, and kill does not
                // wait for the writer to release its Windows file handle.
                launch.session.kill();
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    match launch.rx.try_recv() {
                        Ok(ProfiledPtyChunk::Exit { .. }) => break,
                        Err(async_channel::TryRecvError::Closed) => break,
                        Ok(ProfiledPtyChunk::Output { .. }) => {}
                        Err(async_channel::TryRecvError::Empty) => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                }
                let deadline = Instant::now() + Duration::from_secs(15);
                let cleanup = loop {
                    match std::fs::remove_file(&marker) {
                        Ok(()) => break Ok(()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => break Ok(()),
                        Err(error) if Instant::now() >= deadline => break Err(error),
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                };
                assert!(
                    marker_complete,
                    "the injected fallback must finish its write"
                );
                cleanup.expect("remove fallback marker after bounded retries");
            } else {
                assert!(
                    result.is_err(),
                    "outside-approved program failure must surface"
                );
                assert!(!marker.exists(), "no replacement shell may execute");
                assert_eq!(signal.state(), StartupTurnState::Cancelled);
            }
        }
    }

    #[test]
    fn failed_launch_fallback_has_own_identity() {
        const CHILD: &str = "MUXEL_FALLBACK_ID_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Isolate inherited environment without mutating a parallel test's
            // process environment. Cover both a foreign pane id and no id.
            for inherited in [Some(Uuid::from_u128(1).to_string()), None] {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                let test_name =
                    concat!(module_path!(), "::failed_launch_fallback_has_own_identity");
                let (_, test_name) = test_name
                    .split_once("::")
                    .expect("crate-qualified test name");
                child.args(["--exact", test_name, "--nocapture", "--test-threads=1"]);
                child
                    .env(CHILD, "1")
                    .env_remove("MUXEL_TEST_TRANSPORT_ONLY");
                if let Some(id) = inherited {
                    child.env(INSTANCE_ID_ENV, id);
                } else {
                    child.env_remove(INSTANCE_ID_ENV);
                }
                let result = child.output().expect("run isolated fallback regression");
                assert!(
                    result.status.success()
                        && String::from_utf8_lossy(&result.stdout).contains("1 passed;"),
                    "fallback regression failed: {}{}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            return;
        }

        let own_id = Uuid::from_u128(2);
        let mut requested = CommandSpec::program(
            std::env::temp_dir()
                .join(format!("muxel-missing-program-{}", Uuid::new_v4()))
                .to_string_lossy(),
            Vec::new(),
        );
        requested.env = vec![
            (INSTANCE_ID_ENV.into(), Uuid::from_u128(3).to_string()),
            (
                "MUXEL_TEST_TRANSPORT_ONLY".into(),
                "requested-transport-only".into(),
            ),
        ];
        // A deterministic shell prints its real environment without a user
        // profile or GUI. Only the fallback command is injected; failure and
        // fallback dispatch run through the production path.
        #[cfg(windows)]
        let fallback = CommandSpec::program(
            "powershell.exe",
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "[Console]::WriteLine('MUXEL_FALLBACK_ENV=' + $env:MUXEL_INSTANCE_ID + ':' + $env:MUXEL_TEST_TRANSPORT_ONLY)".into(),
            ],
        );
        #[cfg(not(windows))]
        let fallback = CommandSpec::program(
            "/bin/sh",
            vec![
                "-c".into(),
                "printf 'MUXEL_FALLBACK_ENV=%s:%s\\n' \"$MUXEL_INSTANCE_ID\" \"${MUXEL_TEST_TRANSPORT_ONLY-}\""
                    .into(),
            ],
        );
        let launch =
            TerminalLaunch::spawn_with_fallback(requested, fallback, (120, 24), Some(own_id))
                .expect("fallback shell should spawn");
        assert!(
            launch.launch_error().is_some(),
            "requested launch must fail"
        );
        let expected = format!("MUXEL_FALLBACK_ENV={own_id}:\r\n");
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            match launch.rx.try_recv() {
                Ok(ProfiledPtyChunk::Output { bytes, .. }) => {
                    launch.session.process_output(&bytes);
                    output.extend(bytes);
                    if output
                        .windows(expected.len())
                        .any(|w| w == expected.as_bytes())
                    {
                        break;
                    }
                }
                Ok(ProfiledPtyChunk::Exit { .. }) | Err(async_channel::TryRecvError::Closed) => {
                    break;
                }
                Err(async_channel::TryRecvError::Empty) => {}
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        launch.session.kill();
        assert!(
            output
                .windows(expected.len())
                .any(|w| w == expected.as_bytes()),
            "fallback must have its own id and no requested transport env: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

#[cfg(test)]
mod literal_launch_tests {
    use super::TerminalLaunch;
    use crate::session::CommandSpec;

    #[test]
    fn literal_failure_never_substitutes_a_fallback_program() {
        let missing =
            std::env::temp_dir().join(format!("muxel-missing-{}.exe", uuid::Uuid::new_v4()));
        let fallback = CommandSpec::program(missing.display().to_string(), vec![]);
        let literal =
            CommandSpec::literal(missing.display().to_string(), vec!["opening brief".into()]);
        let error = TerminalLaunch::spawn_with_fallback(literal, fallback, (80, 24), None)
            .err()
            .expect("literal launch must fail");
        assert!(
            !format!("{error:#}").contains("fallback shell"),
            "{error:#}"
        );
    }
}

// Existing launch smoke tests use Unix programs.
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

/// A controller task in a real tmux pane, on a throwaway socket no muxel
/// profile uses. Skipped where tmux isn't installed.
#[cfg(all(test, unix))]
mod tmux_startup_tests {
    use super::{TitleProvider, startup_ready};
    use crate::session::{CommandSpec, PtyChunk, TerminalSession};
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// A stand-in for Codex's idle editor: its Ready title, bracketed paste, a
    /// visible cursor at the empty prompt and a non-GPT model status line.
    const FAKE_CODEX: &str = r#"printf '\033]2;scratch | Ready\033\\'
printf '\033[?2004h\033[2J\033[HOpenAI Codex\r\n› Ask Codex to do anything\r\n\r\n'
printf '  o4-mini medium · scratch\033[2;3H\033[?25h'
sleep 30
"#;

    struct Server {
        socket: String,
        dir: std::path::PathBuf,
    }

    impl Server {
        fn tmux(&self) -> Command {
            let mut tmux = Command::new("tmux");
            tmux.args(["-L", &self.socket, "-f", "/dev/null"])
                .env_remove("TMUX");
            tmux
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let path = self
                .tmux()
                .args(["display-message", "-p", "#{socket_path}"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
                .filter(|path| path.ends_with(&self.socket));
            let _ = self.tmux().arg("kill-server").output();
            if let Some(path) = path {
                let _ = std::fs::remove_file(path);
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_tmux_wrapped_codex_is_ready_only_through_its_tmux_pane_title() {
        if Command::new("tmux").arg("-V").output().is_err() {
            eprintln!("tmux is not installed; skipping");
            return;
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let server = Server {
            socket: format!("mxltest-{id}"),
            dir: std::env::temp_dir().join(format!("muxel-tmux-startup-{id}")),
        };
        std::fs::create_dir_all(&server.dir).unwrap();
        let script = server.dir.join("codex.sh");
        std::fs::write(&script, FAKE_CODEX).unwrap();
        let name = format!("muxel_startup_{}", &id[..8]);
        let mut args: Vec<String> = ["-L", &server.socket, "-f", "/dev/null"]
            .map(String::from)
            .into();
        args.extend(["new-session", "-A", "-s", &name, "--", "sh"].map(String::from));
        args.push(script.display().to_string());
        let mut spec = CommandSpec::program("tmux", args);
        spec.env_remove.push("TMUX".into());
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn tmux client");

        let deadline = Instant::now() + Duration::from_secs(15);
        let screen = loop {
            while let Ok(chunk) = rx.try_recv() {
                if let PtyChunk::Output(bytes) = chunk {
                    session.process_output(&bytes);
                }
            }
            let screen = session.startup_screen();
            if screen.codex_composer_context(true).is_some() {
                break screen;
            }
            if Instant::now() >= deadline {
                session.kill();
                panic!("the tmux client never showed the editor: {:?}", screen.text);
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        // tmux forwards the editor, its cursor and bracketed paste, but not the
        // program's title: from the PTY alone, Codex could never become ready.
        assert_ne!(session.title().as_deref(), Some("scratch | Ready"));
        assert!(!startup_ready(
            TitleProvider::Codex,
            &screen,
            session.title().as_deref(),
            &[],
            true
        ));
        // Read through tmux, as a controller task's probe reads it, it is.
        let (socket, target) = (server.socket.clone(), name.clone());
        let probe = crate::startup::TitleProbe::spawn(
            crate::TitleSource::new(move || {
                let out = Command::new("tmux")
                    .args(["-L", &socket, "-f", "/dev/null"])
                    .args(muxel_core::tmux::pane_title_args(&target))
                    .env_remove("TMUX")
                    .output()
                    .ok()?;
                muxel_core::tmux::parse_pane_title(&target, &String::from_utf8_lossy(&out.stdout))
            }),
            Duration::from_millis(50),
            Duration::from_secs(20),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while probe.snapshot().1.as_deref() != Some("scratch | Ready") {
            if Instant::now() >= deadline {
                session.kill();
                panic!("tmux never reported the title: {:?}", probe.snapshot());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(startup_ready(
            TitleProvider::Codex,
            &screen,
            probe.snapshot().1.as_deref(),
            &[],
            true
        ));
        drop(probe);
        session.kill();
    }
}

#[cfg(test)]
mod startup_readiness_tests {
    use super::{
        PromptOriginProof, PromptSubmissionState, QuietStartup, QuietStartupTarget, SHIFT_TAB,
        STARTUP_KEY_GAP_MS, STARTUP_MAX_WAIT_MS, STARTUP_PRE_TYPE_MS, STARTUP_SUBMIT_DELAY_MS,
        StartupAttempt, TitleProvider, claude_startup_ready, codex_ctl_ready,
        controlled_startup_deadline, ctl_acceptance_observable, ctl_title_can_confirm,
        ctl_transcript_prompt, prompt_submission_state, quiet_startup, startup_has_fresh_user_row,
        startup_provider_accepted, startup_ready, startup_retry_allowed,
    };
    use crate::startup::Screen;

    fn codex_screen(draft: &str) -> Screen {
        Screen {
            // Codex 0.163's passive status line replaces the shortcut hint,
            // including in the empty editor; drafts hide that hint as well.
            text: format!(
                "OpenAI Codex\n› {draft}\n\n  gpt-5.4 high fast · scratch · Context 0% used\n  ⚠ 1 · f2"
            ),
            cursor_row: 1,
            cursor_column: if draft == "Ask Codex to do anything" {
                2
            } else {
                draft.chars().count() + 2
            },
            cursor_visible: true,
            bracketed_paste: true,
        }
    }

    struct FakeUi {
        clock: std::rc::Rc<std::cell::Cell<u64>>,
        output_at: Option<u64>,
        never_quiet: bool,
        writes: std::cell::RefCell<Vec<(u64, Vec<u8>)>>,
        submitted: std::cell::Cell<bool>,
    }

    impl QuietStartupTarget for FakeUi {
        fn has_output(&self) -> bool {
            self.output_at.is_some_and(|at| self.clock.get() >= at)
        }
        fn idle_for(&self) -> std::time::Duration {
            match self.output_at {
                Some(at) if !self.never_quiet => {
                    std::time::Duration::from_millis(self.clock.get().saturating_sub(at))
                }
                _ => std::time::Duration::ZERO,
            }
        }
        fn write_input(&self, data: &[u8]) {
            self.writes
                .borrow_mut()
                .push((self.clock.get(), data.to_vec()));
        }
        fn paste(&self, text: &str) {
            self.write_input(text.as_bytes());
        }
        fn mark_turn_submitted(&self) {
            self.submitted.set(true);
        }
    }

    /// Run a future whose timers are all already elapsed (the fake clock).
    fn run_now<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(output) => output,
            std::task::Poll::Pending => panic!("fake timers never wait"),
        }
    }

    /// Timed writes, whether a turn was submitted, and the profile phases.
    type QuietRun = (Vec<(u64, Vec<u8>)>, bool, Vec<&'static str>);

    fn quiet_run(output_at: Option<u64>, never_quiet: bool, presses: u8, delay: u32) -> QuietRun {
        let clock = std::rc::Rc::new(std::cell::Cell::new(0));
        let ui = FakeUi {
            clock: clock.clone(),
            output_at,
            never_quiet,
            writes: Default::default(),
            submitted: Default::default(),
        };
        let mut events = Vec::new();
        run_now(quiet_startup(
            &ui,
            |ms| {
                clock.set(clock.get() + ms);
                std::future::ready(())
            },
            QuietStartup {
                input: Some("review the diff".into()),
                auto_mode_presses: presses,
                startup_delay_ms: delay,
                submit: true,
            },
            |phase, _| events.push(phase),
        ));
        (ui.writes.into_inner(), ui.submitted.get(), events)
    }

    #[test]
    fn runner_automation_types_at_the_cap_without_any_provider_evidence() {
        // No title, no composer, no tmux pane title: runners, TypeIn presets and
        // loops never wait on provider evidence. A UI that never goes quiet, or
        // never prints, still gets its task typed and submitted at the cap.
        for output_at in [Some(500), None] {
            let (writes, submitted, events) = quiet_run(output_at, true, 0, 0);
            assert_eq!(
                writes,
                [
                    (
                        STARTUP_MAX_WAIT_MS + STARTUP_PRE_TYPE_MS,
                        b"review the diff".to_vec()
                    ),
                    (
                        STARTUP_MAX_WAIT_MS + STARTUP_PRE_TYPE_MS + STARTUP_SUBMIT_DELAY_MS,
                        b"\r".to_vec()
                    ),
                ]
            );
            assert!(submitted);
            assert_eq!(
                events,
                [
                    "automation-first-output",
                    "automation-settled",
                    "automation-pasted",
                    "automation-submitted"
                ]
            );
        }
        // A UI that goes quiet is typed into once it has been quiet long enough.
        let (writes, _, _) = quiet_run(Some(1000), false, 0, 0);
        assert_eq!(writes[0].0, 1000 + 2000 + STARTUP_PRE_TYPE_MS);
        // Auto mode, then the configured delay instead of the quiet wait.
        let (writes, _, _) = quiet_run(Some(1000), true, 2, 4000);
        let sent: Vec<_> = writes.iter().map(|(_, bytes)| bytes.as_slice()).collect();
        assert_eq!(
            sent,
            [SHIFT_TAB, SHIFT_TAB, b"\r", b"review the diff", b"\r"]
        );
        assert_eq!(writes[0].0, 1000 + 4000);
    }

    #[test]
    fn controller_deadline_leaves_room_for_the_writes_after_a_capped_settle() {
        use crate::startup::{Guard, Stop};
        for (delay, presses) in [(0u32, 0u8), (0, 3), (4000, 0), (4000, 3)] {
            let deadline = controlled_startup_deadline(delay, presses);
            let mut guard = Guard::new(0, Some(deadline));
            // The settle loop ran to its cap (plus a second of timer slack),
            // then readiness took a moment before the fixed pauses.
            let mut at = STARTUP_MAX_WAIT_MS + u64::from(delay) + 1_000 + 500;
            let mut revision = 0;
            let mut write = |guard: &mut Guard, at: u64| {
                let admitted =
                    guard.write(std::time::Duration::from_millis(at), revision, || Ok(()));
                revision += 1;
                admitted
            };
            for _ in 0..presses {
                write(&mut guard, at).unwrap();
                at += STARTUP_KEY_GAP_MS;
            }
            if presses > 0 {
                write(&mut guard, at).unwrap();
                at += STARTUP_KEY_GAP_MS;
            }
            at += STARTUP_PRE_TYPE_MS;
            write(&mut guard, at).expect("paste after a capped settle");
            at += STARTUP_SUBMIT_DELAY_MS;
            write(&mut guard, at).expect("Enter after a capped settle");
            // Still a deadline: readiness never falls through to typing.
            assert_eq!(
                Guard::new(0, Some(deadline)).check(deadline, 0),
                Err(Stop::NotReady)
            );
        }
    }

    #[test]
    fn codex_readiness_rejects_starting_titles_and_folder_trust() {
        let mut screen = codex_screen("Ask Codex to do anything");
        assert!(startup_ready(
            TitleProvider::Codex,
            &screen,
            Some("scratch | Ready"),
            &[],
            true
        ));
        for title in [
            None,
            Some("scratch | Starting"),
            Some("scratch | Waiting"),
            Some("scratch | Working"),
        ] {
            assert!(!startup_ready(
                TitleProvider::Codex,
                &screen,
                title,
                &[],
                true
            ));
        }
        screen
            .text
            .insert_str(0, "Folder access\nTrust this folder?\n");
        screen.cursor_row += 2;
        assert!(!startup_ready(
            TitleProvider::Codex,
            &screen,
            Some("scratch | Ready"),
            &[],
            true
        ));
    }

    #[test]
    fn ctl_busy_send_cannot_confirm_from_a_previous_turns_title() {
        use super::AgentStatus;

        // A paste can still be invisible while an earlier turn publishes work.
        let empty = codex_screen("Ask Codex to do anything");
        for (provider, working) in [
            (TitleProvider::Codex, "scratch | Working"),
            (TitleProvider::Claude, "◐ scratch"),
            (TitleProvider::Grok, "⠋ - Thinking - scratch - grok"),
        ] {
            for status in [AgentStatus::Working, AgentStatus::Blocked] {
                assert!(!ctl_title_can_confirm(provider, &empty, None, status));
            }
            assert!(!ctl_title_can_confirm(
                provider,
                &empty,
                Some(working),
                AgentStatus::Idle
            ));
            let attempt = StartupAttempt {
                provider,
                input: "task",
                expected_input: "task".into(),
                before_title: 8,
                allow_working_title: false,
                before_prompt: None,
                instance_id: uuid::Uuid::new_v4(),
            };
            assert_eq!(
                prompt_submission_state(&attempt, &empty, Some(working), 9, None, [&[], &[]], None),
                PromptSubmissionState::Unconfirmed
            );
            if provider == TitleProvider::Codex {
                assert_eq!(
                    prompt_submission_state(
                        &attempt,
                        &empty,
                        Some(working),
                        9,
                        Some("task"),
                        [&[], &[]],
                        None
                    ),
                    PromptSubmissionState::Accepted
                );
            }
        }
        for status in [AgentStatus::Idle, AgentStatus::Done] {
            assert!(ctl_title_can_confirm(
                TitleProvider::Codex,
                &empty,
                Some("scratch | Ready"),
                status
            ));
        }
    }

    #[test]
    fn ctl_send_waits_only_where_a_receipt_can_be_seen() {
        let multi = "first paragraph\n\nsecond paragraph";
        for (provider, idle) in [
            (TitleProvider::Codex, "scratch | Ready"),
            (TitleProvider::Claude, "✳ scratch"),
            (TitleProvider::Grok, "scratch - grok"),
        ] {
            // A readable status title confirms any prompt sent while idle.
            assert!(ctl_acceptance_observable(
                provider,
                Some(idle),
                true,
                multi,
                Some("first paragraph")
            ));
            // tmux hides the title and a busy pane can't use it either; then
            // only a fresh transcript row the readers return whole can confirm.
            for (title, allow) in [(None, true), (Some(idle), false)] {
                assert!(!ctl_acceptance_observable(
                    provider, title, allow, multi, None
                ));
                assert_eq!(
                    ctl_acceptance_observable(provider, title, allow, "one\nparagraph\n", None),
                    provider != TitleProvider::Grok
                );
                // The same prompt again leaves the newest row unchanged.
                assert!(!ctl_acceptance_observable(
                    provider,
                    title,
                    allow,
                    "continue ",
                    Some("continue")
                ));
            }
        }
        // A Codex thread-only title carries no run state to watch.
        assert!(!ctl_acceptance_observable(
            TitleProvider::Codex,
            Some("scratch"),
            true,
            multi,
            None
        ));
    }

    #[test]
    fn ctl_prompt_confirmation_requires_fresh_provider_evidence() {
        let empty = codex_screen("Ask Codex to do anything");
        for (provider, working) in [
            (TitleProvider::Codex, "scratch | Working"),
            (TitleProvider::Claude, "◐ scratch"),
            (TitleProvider::Grok, "⠋ - Thinking - scratch - grok"),
        ] {
            let attempt = StartupAttempt {
                provider,
                input: "task",
                expected_input: "task".into(),
                before_title: 8,
                allow_working_title: true,
                before_prompt: None,
                instance_id: uuid::Uuid::new_v4(),
            };
            assert_eq!(
                prompt_submission_state(&attempt, &empty, Some(working), 8, None, [&[], &[]], None),
                PromptSubmissionState::Unconfirmed
            );
            assert_eq!(
                prompt_submission_state(&attempt, &empty, Some(working), 9, None, [&[], &[]], None),
                PromptSubmissionState::Accepted
            );
        }
        let attempt = StartupAttempt {
            provider: TitleProvider::Codex,
            input: "task",
            expected_input: "task".into(),
            before_title: 8,
            allow_working_title: true,
            before_prompt: None,
            instance_id: uuid::Uuid::new_v4(),
        };
        assert_eq!(
            prompt_submission_state(
                &attempt,
                &empty,
                Some("scratch | Ready"),
                9,
                Some("task"),
                [&[], &[]],
                None
            ),
            PromptSubmissionState::Accepted
        );
        let repeated = StartupAttempt {
            before_prompt: Some("task".into()),
            ..attempt
        };
        assert_eq!(
            prompt_submission_state(
                &repeated,
                &empty,
                Some("scratch | Ready"),
                9,
                Some("task"),
                [&[], &[]],
                None
            ),
            PromptSubmissionState::Unconfirmed
        );
    }

    fn ctl_grid(draft: &[&str], consumed: bool) -> Screen {
        let mut rows = vec![String::new(); 51];
        rows[0] = "OpenAI Codex".into();
        let top = 49 - 1 - draft.len() - usize::from(consumed);
        for (index, piece) in draft.iter().enumerate() {
            rows[top + index] = format!("{}{piece}", if index == 0 { "› " } else { "  " });
        }
        rows[49] = "  GPT-6.1-Sol · scratch".into();
        rows[50] = "  ? for shortcuts".into();
        Screen {
            text: rows
                .iter()
                .map(|row| format!("{row}{}", " ".repeat(41 - row.chars().count())))
                .collect::<Vec<_>>()
                .join("\n"),
            cursor_row: (top + draft.len() - usize::from(!consumed)) as i32,
            cursor_column: if consumed {
                2
            } else {
                2 + draft.last().unwrap().chars().count()
            },
            cursor_visible: true,
            bracketed_paste: true,
        }
    }

    fn ctl_set_row(screen: &mut Screen, index: usize, text: &str) {
        let mut rows = screen.text.lines().map(str::to_owned).collect::<Vec<_>>();
        rows[index] = format!("{text}{}", " ".repeat(41 - text.chars().count()));
        screen.text = rows.join("\n");
    }

    fn ctl_origin_proof() -> PromptOriginProof {
        let mut empty = ctl_grid(&["Ask Codex to do anything"], false);
        empty.cursor_column = 2;
        assert!(codex_ctl_ready(&empty, None, [&[], &[]], true));
        PromptOriginProof {
            frame: empty.codex_ctl_origin().unwrap(),
            title_generation: 8,
            resize_revision: 0,
        }
    }

    #[test]
    fn ctl_prompt_retry_is_windows_codex_only_and_fresh_receipts_veto_it() {
        let proof = ctl_origin_proof();
        let retained = ctl_grid(&["task"], true);
        let pending = ctl_grid(&["task"], false);
        for (provider, title) in [
            (TitleProvider::Codex, "scratch | Ready"),
            (TitleProvider::Claude, "✳ scratch"),
            (TitleProvider::Grok, "scratch - grok"),
        ] {
            let attempt = StartupAttempt {
                provider,
                input: "task",
                expected_input: "task".into(),
                before_title: 8,
                allow_working_title: true,
                before_prompt: None,
                instance_id: uuid::Uuid::new_v4(),
            };
            assert_eq!(
                prompt_submission_state(
                    &attempt,
                    &pending,
                    Some(title),
                    8,
                    None,
                    [&[], &[]],
                    Some(&proof)
                ),
                PromptSubmissionState::Unconfirmed,
                "end cursor does not prove the first CR was read"
            );
            assert_eq!(
                prompt_submission_state(
                    &attempt,
                    &retained,
                    Some(title),
                    8,
                    None,
                    [&[], &[]],
                    Some(&proof)
                ),
                if cfg!(windows) && provider == TitleProvider::Codex {
                    PromptSubmissionState::Retryable
                } else {
                    PromptSubmissionState::Unconfirmed
                }
            );
            assert_eq!(
                prompt_submission_state(
                    &attempt,
                    &retained,
                    Some(title),
                    8,
                    Some("task"),
                    [&[], &[]],
                    Some(&proof)
                ),
                PromptSubmissionState::Unconfirmed,
                "fresh matching user receipt vetoes retry"
            );
            assert_eq!(
                prompt_submission_state(
                    &attempt,
                    &retained,
                    Some(title),
                    8,
                    None,
                    [&[], &[]],
                    None
                ),
                PromptSubmissionState::Unconfirmed,
                "no compact initial origin proof"
            );
            assert_eq!(
                prompt_submission_state(
                    &attempt,
                    &retained,
                    Some(title),
                    9,
                    None,
                    [&[], &[]],
                    Some(&proof)
                ),
                PromptSubmissionState::Unconfirmed,
                "any title-generation change vetoes retry"
            );
        }
    }

    fn long_codex_ctl_screen() -> (String, Screen) {
        // One 31-column word per 38-column textarea row; eighteen rows exceed
        // the general read-aloud scan's bounded composer reach.
        let words: Vec<_> = (0..18)
            .map(|index| format!("segment{index:02}abcdefghijklmnopqrstuv"))
            .collect();
        let pieces = words.iter().map(String::as_str).collect::<Vec<_>>();
        (words.join(" "), ctl_grid(&pieces, true))
    }

    #[test]
    fn ctl_codex_no_title_long_own_composer_is_not_a_transcript_receipt() {
        let (task, retained) = long_codex_ctl_screen();
        let generic = muxel_core::readaloud::prompt_from_screen(&retained.text).unwrap();
        assert_eq!(
            crate::startup::normalized(&generic),
            task,
            "reproduce generic long-composer echo"
        );
        assert_eq!(
            ctl_transcript_prompt(TitleProvider::Codex, &retained, || retained.text.clone()),
            None
        );
        let proof = ctl_origin_proof();
        let attempt = StartupAttempt {
            provider: TitleProvider::Codex,
            input: &task,
            expected_input: task.clone(),
            before_title: 8,
            allow_working_title: true,
            before_prompt: None,
            instance_id: uuid::Uuid::new_v4(),
        };
        for title in [
            None,
            Some("a thread name without semantic status"),
            Some("scratch | Ready"),
        ] {
            assert!(codex_ctl_ready(&retained, title, [&[], &[]], false));
            assert!(
                !startup_ready(TitleProvider::Codex, &retained, None, &[], false),
                "startup title admission stays unchanged"
            );
            assert_eq!(
                prompt_submission_state(
                    &attempt,
                    &retained,
                    title,
                    8,
                    None,
                    [&[], &[]],
                    Some(&proof)
                ),
                if cfg!(windows) {
                    PromptSubmissionState::Retryable
                } else {
                    PromptSubmissionState::Unconfirmed
                }
            );
        }
    }

    #[test]
    fn ctl_codex_readiness_vetoes_explicit_work_chrome_and_both_marker_sets() {
        let (_, retained) = long_codex_ctl_screen();
        for title in [
            "scratch | Working",
            "scratch | Starting",
            "scratch | Waiting",
            "scratch | Blocked",
            "scratch | Action Required",
            "[ ! ] Action Required | scratch",
            "Working",
            "Blocked",
        ] {
            assert!(
                !codex_ctl_ready(&retained, Some(title), [&[], &[]], false),
                "{title}"
            );
        }
        // Only provider chrome owns configured markers, not a quoted task.
        let quoted = vec!["segment07".into()];
        assert!(codex_ctl_ready(&retained, None, [&quoted, &quoted], false));
        let custom = vec!["OpenAI Codex".into()];
        assert!(!codex_ctl_ready(&retained, None, [&custom, &[]], false));
        assert!(!codex_ctl_ready(&retained, None, [&[], &custom], false));
        for chrome in [
            "esc to interrupt",
            "Trust this folder?",
            "Enter to select",
            "Type to filter",
        ] {
            let mut blocked = long_codex_ctl_screen().1;
            ctl_set_row(&mut blocked, 0, chrome);
            assert!(
                !codex_ctl_ready(&blocked, None, [&[], &[]], false),
                "{chrome}"
            );
            let mut origin = ctl_grid(&[""], false);
            ctl_set_row(&mut origin, 0, chrome);
            assert!(
                !codex_ctl_ready(&origin, None, [&[], &[]], true),
                "origin: {chrome}"
            );
        }
        let origin = ctl_grid(&[""], false);
        assert!(!codex_ctl_ready(&origin, Some("Working"), [&[], &[]], true));
        assert!(!codex_ctl_ready(&origin, None, [&custom, &[]], true));
        assert!(!codex_ctl_ready(&origin, None, [&[], &custom], true));
    }

    #[test]
    fn ctl_codex_fresh_rendered_transcript_vetoes_retry_and_can_confirm_empty_editor() {
        let (task, mut retained) = long_codex_ctl_screen();
        let proof = ctl_origin_proof();
        let attempt = StartupAttempt {
            provider: TitleProvider::Codex,
            input: &task,
            expected_input: task.clone(),
            before_title: 8,
            allow_working_title: true,
            before_prompt: None,
            instance_id: uuid::Uuid::new_v4(),
        };
        for (index, word) in task.split(' ').enumerate() {
            ctl_set_row(
                &mut retained,
                index + 1,
                &format!("{}{word}", if index == 0 { "› " } else { "  " }),
            );
        }
        let prompt =
            ctl_transcript_prompt(TitleProvider::Codex, &retained, || retained.text.clone());
        assert_eq!(prompt.as_deref(), Some(task.as_str()));
        assert!(startup_has_fresh_user_row(&attempt, prompt.as_deref()));
        assert_eq!(
            prompt_submission_state(
                &attempt,
                &retained,
                None,
                8,
                prompt.as_deref(),
                [&[], &[]],
                Some(&proof)
            ),
            PromptSubmissionState::Unconfirmed
        );
        let mut empty = ctl_grid(&["Ask Codex to do anything"], false);
        empty.cursor_column = 2;
        ctl_set_row(&mut empty, 1, "› task");
        ctl_set_row(&mut empty, 2, "Assistant reply");
        let prompt = ctl_transcript_prompt(TitleProvider::Codex, &empty, || empty.text.clone());
        assert_eq!(prompt.as_deref(), Some("task"));
        let small_attempt = StartupAttempt {
            provider: TitleProvider::Codex,
            input: "task",
            expected_input: "task".into(),
            before_title: 8,
            allow_working_title: true,
            before_prompt: None,
            instance_id: uuid::Uuid::new_v4(),
        };
        assert_eq!(
            prompt_submission_state(
                &small_attempt,
                &empty,
                None,
                8,
                prompt.as_deref(),
                [&[], &[]],
                None
            ),
            PromptSubmissionState::Accepted
        );
    }

    #[test]
    fn startup_acceptance_rejects_own_echo_stale_work_and_boot_activity() {
        let attempt = StartupAttempt {
            provider: TitleProvider::Codex,
            input: "task",
            expected_input: "task".into(),
            before_title: 8,
            allow_working_title: true,
            before_prompt: None,
            instance_id: uuid::Uuid::new_v4(),
        };
        let empty = codex_screen("Ask Codex to do anything");
        for (generation, title) in [
            (8, "scratch | Working"),
            (9, "scratch | Starting"),
            (9, "scratch | Waiting"),
        ] {
            assert!(!startup_provider_accepted(
                &attempt,
                &empty,
                Some(title),
                generation,
                None
            ));
        }
        assert!(!startup_provider_accepted(
            &attempt,
            &codex_screen("task"),
            Some("scratch | Working"),
            9,
            Some("task")
        ));
        assert!(startup_provider_accepted(
            &attempt,
            &empty,
            Some("scratch | Working"),
            9,
            None
        ));
        // A fast real turn can finish before a poll; its rendered user row and
        // cleared live editor still prove acceptance without a caught spinner.
        assert!(startup_provider_accepted(
            &attempt,
            &empty,
            Some("scratch | Ready"),
            10,
            Some("task")
        ));
        assert!(!startup_provider_accepted(
            &attempt,
            &empty,
            Some("scratch | Ready"),
            10,
            Some("other task")
        ));
        let old_row = StartupAttempt {
            before_prompt: Some("task".into()),
            ..attempt
        };
        assert!(!startup_provider_accepted(
            &old_row,
            &empty,
            Some("scratch | Ready"),
            10,
            Some("task")
        ));
    }

    #[test]
    fn codex_retry_refuses_any_receipt_changed_whitespace_and_unproven_editor_geometry() {
        let attempt = StartupAttempt {
            provider: TitleProvider::Codex,
            input: "a  b",
            expected_input: "a b".into(),
            before_title: 2,
            allow_working_title: true,
            before_prompt: None,
            instance_id: uuid::Uuid::new_v4(),
        };
        assert!(startup_retry_allowed(
            &attempt,
            &codex_screen("a  b"),
            Some("scratch | Ready"),
            &[],
            None
        ));
        assert!(!startup_retry_allowed(
            &attempt,
            &codex_screen("a b"),
            Some("scratch | Ready"),
            &[],
            None
        ));
        // A fresh matching user row independently forbids a second CR, even
        // when the provider has retained or restored identical draft text.
        assert!(!startup_retry_allowed(
            &attempt,
            &codex_screen("a  b"),
            Some("scratch | Ready"),
            &[],
            Some("a  b")
        ));
        let mut suffix = codex_screen("a  b changed");
        suffix.cursor_column = 6;
        assert!(!startup_retry_allowed(
            &attempt,
            &suffix,
            Some("scratch | Ready"),
            &[],
            None
        ));
        let mut continuation = codex_screen("a  b");
        continuation.text = continuation
            .text
            .replace("a  b\n\n", "a  b\n  more content\n\n");
        assert!(!startup_retry_allowed(
            &attempt,
            &continuation,
            Some("scratch | Ready"),
            &[],
            None
        ));
        continuation.text = codex_screen("a  b")
            .text
            .replace("a  b\n\n", "a  b\n\n  hidden continuation\n\n");
        assert!(!startup_retry_allowed(
            &attempt,
            &continuation,
            Some("scratch | Ready"),
            &[],
            None
        ));
        let multiline = StartupAttempt {
            input: "a\nb",
            ..attempt
        };
        assert!(!startup_retry_allowed(
            &multiline,
            &codex_screen("a b"),
            Some("scratch | Ready"),
            &[],
            None
        ));
    }

    #[test]
    fn grok_task_quoting_codex_trust_text_does_not_block_its_own_first_enter() {
        let screen = Screen { text: "Grok\n› Explain Trust this folder? and Folder access and enter continue · esc quit\n".into(),
            cursor_row: 1, cursor_column: 80, cursor_visible: true, bracketed_paste: true };
        assert!(startup_ready(
            TitleProvider::Grok,
            &screen,
            Some("scratch - grok"),
            &[],
            false
        ));
    }

    #[test]
    fn quoted_permission_text_in_the_draft_does_not_block_its_own_submit() {
        for (draft, cursor_row) in [
            ("❯ Explain the wording Do you want to proceed", 2),
            (
                "❯ Explain the wording\n  Do you want to proceed\n  and this second line",
                4,
            ),
        ] {
            let mut screen = Screen {
                text: format!("Claude Code\n────────────────\n{draft}\n────────────────\nmode"),
                cursor_column: 22,
                cursor_row,
                cursor_visible: true,
                bracketed_paste: true,
            };
            let markers = vec!["Do you want to proceed".into()];
            assert!(claude_startup_ready(&screen, None, &markers, false));
            assert!(!claude_startup_ready(&screen, None, &markers, true));
            // A real permission question outside the composer still vetoes CR.
            screen.text.insert_str(0, "Do you want to proceed\n");
            screen.cursor_row += 1;
            assert!(!claude_startup_ready(&screen, None, &markers, false));
        }
    }

    #[test]
    fn native_slash_completion_is_ready_for_submit_but_not_an_empty_startup() {
        // The controlled native reproduction's completion menu has no permission
        // footer. Its occupied composer must admit the first CR, never a retry.
        let screen = Screen {
            text: "Claude Code v2.1.304\n  ❯ /b12-ready  Reply exactly B12_READY. Do not use tools,\n                 read or write files, or call other agents…\n────────────────────────────────────────\n❯\u{a0}/b12-ready\n────────────────────────────────────────\n  Friday 0%\n  ⏵⏵ auto mode on (shift+tab to cycle)".into(),
            cursor_column: 12, cursor_row: 4, cursor_visible: true, bracketed_paste: true,
        };
        assert!(claude_startup_ready(
            &screen,
            Some("✳ Claude Code"),
            &[],
            false
        ));
        assert!(!claude_startup_ready(&screen, None, &[], true));
        assert!(!claude_startup_ready(
            &screen,
            Some("◐ Claude Code"),
            &[],
            false
        ));
        assert!(!claude_startup_ready(
            &screen,
            None,
            &["manual approval".into(), "auto mode on".into()],
            false
        ));
    }

    #[test]
    fn permission_chrome_vetoes_an_old_composer_even_with_idle_title() {
        let screen = Screen {
            text: "────────────────\n❯ task\n────────────────\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to confirm · Esc to cancel".into(),
            cursor_column: 6, cursor_row: 1, cursor_visible: true, bracketed_paste: true,
        };
        assert!(!claude_startup_ready(
            &screen,
            Some("✳ Claude Code"),
            &[],
            false
        ));
    }
}

#[cfg(test)]
mod tests {
    // Import specifically (not `super::*`) so `#[test]` resolves to the built-in
    // macro, not gpui's glob-imported `test` attribute.
    use super::{
        AgentStatus, BACKGROUND_PAINT_INTERVAL, CURSOR_HIDE_HOLD, FOCUSED_INTERACTION_INTERVAL,
        FOCUSED_STREAM_INTERVAL, GROK_SCREEN_WORKING_HOLD, PaintSchedule,
        STATUS_SCREEN_SCAN_INTERVAL, StatusScreenCapture, TerminalMouseMode,
        TerminalStatusCoverage, TerminalStatusReason, TitleProvider, can_latch_completion,
        classify, clean_agent_title, combine_title_status, fallback_status_deadline,
        grok_screen_working_state, has_working_to_nonworking_edge, hold_after, hold_grok_blocked,
        hold_grok_screen_working, is_submission_input, latch_done, latch_done_after_readiness,
        next_paint_schedule, paint_min_interval, paint_request, provider_screen_status,
        provider_settled_after_title, runtime_timer_delay, runtime_timer_is_current,
        should_capture_status_screen, status_coverage, status_paint_defer, status_reason,
        status_screen_capture_plan, sticky_title_status, title_status, title_status_trace,
    };
    use std::time::{Duration, Instant};

    fn m(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn activity_fallback_has_one_exact_deadline() {
        let now = Instant::now();
        assert_eq!(
            fallback_status_deadline(now, Duration::from_millis(1250), AgentStatus::Working, true,),
            Some(now + Duration::from_millis(750))
        );
        assert_eq!(
            fallback_status_deadline(now, Duration::from_secs(2), AgentStatus::Working, true,),
            None
        );
        assert_eq!(
            fallback_status_deadline(now, Duration::ZERO, AgentStatus::Working, false,),
            None
        );
    }

    #[test]
    fn superseded_runtime_timer_is_rejected() {
        assert!(runtime_timer_is_current(9, 9));
        assert!(!runtime_timer_is_current(10, 9));
        let now = Instant::now();
        assert_eq!(runtime_timer_delay(now, now), Duration::ZERO);
        // A deadline that passed while its refresh ran fires at once, never dropped.
        assert_eq!(
            runtime_timer_delay(now - Duration::from_millis(1), now),
            Duration::ZERO
        );
        assert_eq!(
            runtime_timer_delay(now + Duration::from_millis(1), now),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn expired_grok_provider_hold_does_not_schedule_behind_a_working_marker() {
        let now = Instant::now();
        let last_seen = now - GROK_SCREEN_WORKING_HOLD - Duration::from_millis(1);
        let (working, retained_at, deadline) =
            grok_screen_working_state(true, false, Some(last_seen), now);
        assert!(working, "the configured marker remains valid evidence");
        assert_eq!(retained_at, None, "the expired provider hold is retired");
        assert_eq!(deadline, None, "no past timer may be re-armed");
    }

    #[test]
    fn only_plain_enter_marks_a_submission() {
        assert!(is_submission_input(b"\r"));
        assert!(!is_submission_input(b"\x1b\r"));
        assert!(!is_submission_input(b"\n"));
    }

    #[test]
    fn unchanged_runtime_generation_never_recaptures_the_screen() {
        assert!(should_capture_status_screen(8, 9, true));
        assert!(!should_capture_status_screen(9, 9, true));
        assert!(!should_capture_status_screen(8, 9, false));

        let now = Instant::now();
        assert_eq!(
            status_screen_capture_plan(8, 9, true, Some(now), now),
            StatusScreenCapture::At(now + STATUS_SCREEN_SCAN_INTERVAL)
        );
        assert_eq!(
            status_screen_capture_plan(8, 9, true, Some(now - STATUS_SCREEN_SCAN_INTERVAL), now,),
            StatusScreenCapture::Now
        );
    }

    #[test]
    fn snapshot_evidence_does_not_claim_hidden_work_coverage() {
        assert_eq!(
            status_coverage(TitleProvider::Other, &[], &[]),
            TerminalStatusCoverage::ActivityOnly
        );
        assert_eq!(
            status_coverage(TitleProvider::Other, &m(&["busy"]), &[]),
            TerminalStatusCoverage::VisibleMarkers
        );
        assert_eq!(
            status_coverage(TitleProvider::Codex, &[], &[]),
            TerminalStatusCoverage::ProviderVisible
        );
        assert_eq!(
            status_reason(
                AgentStatus::Working,
                false,
                false,
                Some(AgentStatus::Working),
                None,
                false,
                false,
            ),
            TerminalStatusReason::ProviderTitle
        );
    }

    #[test]
    fn markerless_provider_must_prove_semantic_titles_before_latching() {
        assert!(!can_latch_completion(false, false));
        assert!(can_latch_completion(false, true));
        assert!(can_latch_completion(true, false));
    }

    #[test]
    fn claude_current_and_legacy_titles_have_explicit_edges() {
        for working in [
            "◐ Review changes",
            "◑ Review changes",
            "◒ Review changes",
            "◓ Review changes",
            "⠂ Review changes",
            "⠐ Review changes",
        ] {
            assert_eq!(
                title_status(
                    TitleProvider::Claude,
                    Some(working),
                    Some(Duration::from_secs(4)),
                ),
                Some(AgentStatus::Working),
                "rejected Claude busy title {working:?}"
            );
        }
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
    fn claude_visible_permission_form_overrides_idle_title() {
        let permission = "Do you want to proceed?\n❯ 1. Yes\n  2. Yes, and don't ask again\n  3. No\nEsc to cancel";
        let screen_status = provider_screen_status(TitleProvider::Claude, permission);
        assert_eq!(screen_status, Some(AgentStatus::Blocked));
        assert_eq!(
            combine_title_status(false, AgentStatus::Idle, screen_status, false),
            AgentStatus::Blocked
        );

        let selection =
            "Choose an answer\nEnter to select · Tab/Arrow keys to navigate · Esc to cancel";
        assert_eq!(
            provider_screen_status(TitleProvider::Claude, selection),
            Some(AgentStatus::Blocked)
        );

        for screen in [
            "Select model\nEnter to set as default\nEsc to cancel",
            "Claude said: Do you want to proceed?\nEsc to cancel",
            "❯ 1. Yes\nEsc to cancel",
        ] {
            assert_eq!(
                provider_screen_status(TitleProvider::Claude, screen),
                None,
                "ordinary UI forged Blocked: {screen:?}"
            );
        }
    }

    #[test]
    fn lifecycle_signal_precedence_is_explicit() {
        use AgentStatus::{Blocked, Done, Idle, Working};

        let cases = [
            ("exit", true, Working, Some(Blocked), true, Done),
            (
                "screen blocked",
                false,
                Blocked,
                Some(Working),
                true,
                Blocked,
            ),
            ("title blocked", false, Idle, Some(Blocked), true, Blocked),
            ("screen working", false, Working, Some(Idle), true, Working),
            ("title working", false, Idle, Some(Working), false, Working),
            ("title idle", false, Working, Some(Idle), false, Idle),
            ("bell done", false, Done, Some(Idle), false, Done),
            ("base fallback", false, Idle, None, false, Idle),
        ];

        for (name, exited, base, title, screen_working, expected) in cases {
            assert_eq!(
                combine_title_status(exited, base, title, screen_working),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn codex_forced_title_contract_separates_agent_from_background_output() {
        for idle in ["Review title | Ready", "Review title | Ready ·"] {
            assert_eq!(
                title_status(TitleProvider::Codex, Some(idle), Some(Duration::ZERO)),
                Some(AgentStatus::Idle),
                "rejected idle title {idle:?}"
            );
        }
        for working in [
            "Review title | Starting ⠦",
            "Review title | Working ⠋",
            "Review title | Thinking ⠙",
            "Review title | Waiting ⠹",
            "Review title | Working",
            "Review title | Starting · ⠦",
        ] {
            assert_eq!(
                title_status(TitleProvider::Codex, Some(working), Some(Duration::ZERO)),
                Some(AgentStatus::Working),
                "rejected working title {working:?}"
            );
        }
        for blocked in [
            "[ ! ] Action Required | Review title",
            "[ . ] Action Required | Review | title",
            "Review title | Ready · action required",
        ] {
            assert_eq!(
                title_status(TitleProvider::Codex, Some(blocked), Some(Duration::ZERO)),
                Some(AgentStatus::Blocked),
                "rejected blocked title {blocked:?}"
            );
        }
        assert_eq!(
            combine_title_status(false, AgentStatus::Working, Some(AgentStatus::Idle), false),
            AgentStatus::Idle
        );
        assert_eq!(
            classify(
                false,
                "typing echoed by the TUI",
                &[],
                &[],
                false,
                false,
                Duration::from_millis(10),
            ),
            AgentStatus::Idle
        );
        for foreign in [
            "Ready",
            "Ready ·",
            "Working ⠋",
            "Review title | Working on tests",
            "[ ! ] Action Requiredly | Review title",
        ] {
            assert_eq!(
                title_status(TitleProvider::Codex, Some(foreign), Some(Duration::ZERO)),
                None,
                "accepted foreign title {foreign:?}"
            );
        }
    }

    #[test]
    fn grok_spinner_is_working_independent_of_activity_prose() {
        for working in [
            "⠦ - Responding - Review title - grok",
            "⠦ - Waiting for response… - Review title - grok",
            "⠋ - Writing command… - Review title - grok",
            "⠙ - Writing file… - Review title - grok",
            "⠹ - Run harmless Get-Date command… - grok",
            "⠸ - Preparing MCP tool… - Review title - grok",
        ] {
            assert_eq!(
                title_status(
                    TitleProvider::Grok,
                    Some(working),
                    Some(Duration::from_secs(30)),
                ),
                Some(AgentStatus::Working),
                "rejected Grok busy title {working:?}"
            );
        }
        assert_eq!(
            title_status(
                TitleProvider::Grok,
                Some("⚠ Action Required - ⠋ - Preparing question… - Review title - grok"),
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
    fn claude_live_foreground_row_overrides_idle_title() {
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "⎿  Running…\n\n✻ Unravelling… (3s · ↓ 117 tokens)\n\n  esc to in…"
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "✻ Chewing on it… (1m 20s · ↓ 2.6k tokens)"
            ),
            Some(AgentStatus::Working)
        );
        for idle_or_quoted in [
            "✻ Brewed for 12s · done 11:22 AM",
            "quoted: ✻ Chewing on it… (20s · ↓ 171 tokens)",
            "> ✻ Chewing on it… (20s · ↓ 171 tokens)",
            "✻ Chewing on it… (soon · ↓ 171 tokens)",
            "✻ Chewing on it… (20s · tokens)",
            "✻ … (20s · ↓ 171 tokens)",
            "✻ Chewing on it… (3s 1h · ↓ 171 tokens)",
            "✻ Chewing on it… (1m 2m · ↓ 171 tokens)",
            "✻ Chewing on it… (1s 2m · ↓ 171 tokens)",
            "✻ Chewing on it… (20s · ↑↓171 tokens)",
            "✻ Chewing on it… (20s · ↓171 tokens)",
            "✻ Chewing on it… (20s · 1e3 tokens)",
            "✻ Chewing on it… (20s · +171 tokens)",
            "✻ Chewing on it… (20s · ↓ 2.6 tokens)",
            "✻ Chewing on it… (20s · .5k tokens)",
            "✻ Chewing on it… (20s · 2..6k tokens)",
        ] {
            assert_eq!(
                provider_screen_status(TitleProvider::Claude, idle_or_quoted),
                None,
                "accepted Claude foreground lookalike {idle_or_quoted:?}"
            );
        }

        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "✻ Chewing on it… (20s · ↓ 171 tokens)\n\nThat was the exact row under discussion.\n\n✻ Brewed for 21s · done 11:22 AM"
            ),
            None,
            "an earlier exact row must not override newer completion evidence"
        );

        let live_background_above_long_output = format!(
            "· 1 command still running · send a message to interrupt\n{}",
            (0..20)
                .map(|line| format!("ordinary output {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(
            provider_screen_status(TitleProvider::Claude, &live_background_above_long_output),
            Some(AgentStatus::Working),
            "live background work must not disappear beyond an arbitrary row window"
        );
    }

    #[test]
    fn live_background_command_overrides_idle_title() {
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "· 1 command still running · send a message to interrupt"
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            provider_screen_status(TitleProvider::Claude, "Ready for another prompt"),
            None
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "The log says: 1 command still running; investigate it."
            ),
            None
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "quoted: · 1 command still running · send a message to interrupt later"
            ),
            None
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "  · 2 commands still running · send a message to interrupt  "
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "✻ Brewed for 15s · 1 shell still running"
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "✻ Crunched for 1m 2s · 2 shells still running"
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "✻ Brewed for 15s · done 11:22 AM\nAuto mode on · 1 shell ·   \n2 agents"
            ),
            Some(AgentStatus::Working),
            "a footer wrapped at a word boundary must remain live"
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "✻ Brewed for 15s · done 11:22 AM\nAuto mode on · 1 shell · 2 ag\nents"
            ),
            Some(AgentStatus::Working),
            "a footer wrapped inside a task kind must remain live"
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "✻ Brewed for 15s · done 11:22 AM\nAuto mode on · 1 shell · 2 agents"
            ),
            Some(AgentStatus::Working),
            "Claude's current task footer must override the completed foreground row"
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "✻ Brewed for 15s · done 11:22 AM\nAuto mode on · 1 shell, 2 monitors · 1 local agent"
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Claude,
                "Auto mode on · 1 shell · 2 agents\n✻ Brewed for 15s · done 11:22 AM"
            ),
            None,
            "a stale footer must not override a newer completion row"
        );
        for false_positive in [
            "✻ Brewed for 15s · 0 shells still running",
            "✻ Brewed for 15s · 1 shells still running",
            "quoted: ✻ Brewed for 15s · 1 shell still running",
            "✻ Brewed for 15s · 1 shell still running later",
            "Auto mode on",
            "Auto mode on · 0 shells",
            "Auto mode on · 1 shells",
            "Auto mode on · 2 shell",
            "Auto mode on · 1 worker",
            "Auto mode on · 1 shell · 1 hook",
            "Auto mode on · 1 shell ·",
            "quoted: Auto mode on · 1 shell · 2 agents",
            "Auto mode off · 1 shell · 2 agents",
        ] {
            assert_eq!(
                provider_screen_status(TitleProvider::Claude, false_positive),
                None,
                "accepted Claude background lookalike {false_positive:?}"
            );
        }
        assert_eq!(
            provider_screen_status(
                TitleProvider::Grok,
                "· 1 command still running · send a message to interrupt"
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Grok,
                "◎ 2 commands still running · send a message to interrupt"
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Grok,
                "◎ 1 command · 2 monitors · 1 loop · 1 subagent still running"
            ),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            provider_screen_status(TitleProvider::Grok, "◎ 1 monitor · 2 loops still running"),
            Some(AgentStatus::Working)
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Grok,
                "◎ waiting · send a message to interrupt"
            ),
            None
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Grok,
                "◎ 0 commands still running · send a message to interrupt"
            ),
            None
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Grok,
                "quoted: ◎ 1 command still running · send a message to interrupt later"
            ),
            None
        );
        assert_eq!(
            provider_screen_status(
                TitleProvider::Grok,
                "◎ 1 commands still running · send a message to interrupt"
            ),
            None
        );
        assert_eq!(
            provider_screen_status(TitleProvider::Grok, "◎ 1 workflow still running"),
            None
        );
    }

    #[test]
    fn grok_background_row_gaps_do_not_complete_until_stable_absence() {
        use AgentStatus::{Done, Idle, Working};

        assert!(hold_grok_screen_working(true, None));
        assert!(hold_grok_screen_working(
            false,
            Some(Duration::from_millis(1999))
        ));
        assert!(!hold_grok_screen_working(
            false,
            Some(Duration::from_millis(2001))
        ));
        assert!(!hold_grok_screen_working(false, None));

        let (status, latch, armed) =
            latch_done_after_readiness(Some(Idle), Working, false, true, true, false);
        assert_eq!(status, Working);
        let (status, latch, armed) =
            latch_done_after_readiness(Some(Working), Working, latch, true, armed, false);
        assert_eq!(status, Working, "one missing row frame forged completion");
        let (status, latch, _) =
            latch_done_after_readiness(Some(Working), Idle, latch, true, armed, false);
        assert_eq!(status, Done, "stable absence did not complete the turn");
        let (status, _, _) =
            latch_done_after_readiness(Some(Idle), Idle, latch, true, armed, false);
        assert_eq!(status, Done, "completion did not remain latched");
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
        let raw_working = combine_title_status(false, AgentStatus::Idle, grok_working, false);
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
            false,
        );
        assert_eq!(raw_idle, AgentStatus::Idle);
        assert_eq!(
            latch_done(Some(AgentStatus::Working), raw_idle, false, true),
            (AgentStatus::Done, true)
        );

        // Typing while an already-idle title is present cannot manufacture a
        // Working→Done edge from the PTY activity heuristic.
        let typed = combine_title_status(false, AgentStatus::Working, grok_idle, false);
        assert_eq!(
            latch_done(Some(AgentStatus::Idle), typed, false, true),
            (AgentStatus::Idle, false)
        );
    }

    #[test]
    fn coalesced_title_frames_keep_the_working_to_idle_edge() {
        let titles = vec![
            Some("Thread | Working ⠋".to_string()),
            Some("Thread | Ready".to_string()),
        ];
        let statuses = title_status_trace(TitleProvider::Codex, None, &titles);
        assert_eq!(statuses, vec![AgentStatus::Working, AgentStatus::Idle]);
        assert!(has_working_to_nonworking_edge(&statuses));
    }

    #[test]
    fn a_submission_before_a_resume_does_not_complete_the_resumed_conversation() {
        use crate::session::{CommandSpec, TerminalSession};
        #[cfg(windows)]
        let spec = CommandSpec::program(
            "cmd.exe",
            vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
        );
        #[cfg(unix)]
        let spec = CommandSpec::program("/bin/cat", vec![]);
        let (session, _output) = TerminalSession::spawn(spec, 80, 24).expect("spawn fixture");

        // Enter picks a conversation in Codex's `/resume` list, in the old binding.
        session.mark_turn_submitted();
        session.write_input(b"\r");
        // The resumed conversation boots: Starting then Ready, in one drained batch.
        let resumed = "019f95d7-db31-7db0-904d-9e08330e0001";
        session.process_probe_output(
            format!("\x1b]0;{resumed} | Starting\x07\x1b]0;{resumed} | Ready\x07").as_bytes(),
        );

        // What `update_runtime` feeds the reducer after the binding reset.
        let evidence = session.runtime_evidence(None);
        assert!(!evidence.overflowed);
        let statuses = title_status_trace(TitleProvider::Codex, None, &evidence.titles);
        assert!(has_working_to_nonworking_edge(&statuses));
        let submitted = evidence.submitted;
        let (status, _, armed) = latch_done_after_readiness(
            Some(AgentStatus::Working),
            AgentStatus::Idle,
            false,
            true,
            false,
            submitted,
        );
        assert_eq!(
            status,
            AgentStatus::Idle,
            "no turn ran in this conversation"
        );
        assert!(armed, "its Ready title arms the next turn");
        session.kill();
    }

    #[test]
    fn foreign_titles_never_become_lifecycle_state() {
        for provider in [
            TitleProvider::Claude,
            TitleProvider::Codex,
            TitleProvider::Grok,
            TitleProvider::Other,
        ] {
            assert_eq!(
                title_status(
                    provider,
                    Some("npm run test-v8-historical"),
                    Some(Duration::ZERO)
                ),
                None
            );
        }
        assert_eq!(
            title_status(
                TitleProvider::Codex,
                Some("npm run ready"),
                Some(Duration::ZERO)
            ),
            None
        );
        assert_eq!(
            title_status(
                TitleProvider::Claude,
                Some("✳not a Claude frame"),
                Some(Duration::ZERO)
            ),
            None
        );
        assert_eq!(
            title_status(
                TitleProvider::Grok,
                Some("Thinking - Review title"),
                Some(Duration::ZERO)
            ),
            None
        );
        for name in [
            "Thinking",
            "Responding",
            "Running tool",
            "Compacting",
            "Waiting for tests",
            "Running: npm",
            "Retrying (1)",
        ] {
            let title = format!("{name} - grok");
            assert_eq!(
                title_status(TitleProvider::Grok, Some(&title), Some(Duration::ZERO)),
                Some(AgentStatus::Idle),
                "session name forged lifecycle: {name:?}"
            );
            assert_eq!(clean_agent_title("grok", &title).as_deref(), Some(name));
        }
        for title in [
            "⚠ Action Required - grok",
            "⚠ Action Required - Review - grok",
            "⚠ Action Required - ⠋ - grok",
            "⠋ - grok",
        ] {
            assert_eq!(
                title_status(TitleProvider::Grok, Some(title), Some(Duration::ZERO)),
                None,
                "incomplete reserved prefix forged lifecycle: {title:?}"
            );
            assert!(clean_agent_title("grok", title).is_some());
        }
    }

    #[test]
    fn foreign_titles_cannot_erase_a_provider_lifecycle_observation() {
        use AgentStatus::{Idle, Working};

        let working = title_status(
            TitleProvider::Codex,
            Some("Review changes | Working ⠋"),
            Some(Duration::ZERO),
        );
        assert_eq!(sticky_title_status(None, working), Some(Working));
        assert_eq!(sticky_title_status(working, None), Some(Working));
        assert_eq!(
            sticky_title_status(
                working,
                title_status(TitleProvider::Codex, Some("Ready ·"), Some(Duration::ZERO))
            ),
            Some(Working)
        );

        let idle = title_status(
            TitleProvider::Codex,
            Some("Review changes | Ready"),
            Some(Duration::ZERO),
        );
        assert_eq!(sticky_title_status(working, idle), Some(Idle));
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
        assert_eq!(clean_agent_title("codex", "Ready"), None);
        assert_eq!(
            clean_agent_title("codex", "Review changes | Ready").as_deref(),
            Some("Review changes")
        );
        assert_eq!(
            clean_agent_title(
                "grok",
                "⠦ - Waiting for response… - User requests exact OK reply - grok"
            )
            .as_deref(),
            Some("User requests exact OK reply")
        );
        assert_eq!(clean_agent_title("grok", "grok"), None);
        for program in ["claude", "codex", "grok"] {
            assert_eq!(
                clean_agent_title(program, "npm run test-v8-historical"),
                None
            );
        }
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
            classify(
                true,
                "esc to interrupt",
                &working,
                &blocked,
                true,
                false,
                busy,
            ),
            AgentStatus::Done
        );
        // Working marker beats a stale bell when no input request is present.
        assert_eq!(
            classify(
                false,
                "… esc to interrupt",
                &working,
                &blocked,
                true,
                false,
                quiet,
            ),
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
                false,
                quiet,
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
                false,
                busy,
            ),
            AgentStatus::Blocked
        );
        // Bell with no marker on screen = finished a turn.
        assert_eq!(
            classify(false, "all done", &working, &blocked, true, false, quiet,),
            AgentStatus::Done
        );
        // With a working marker configured, output activity (e.g. typing) does
        // NOT imply working — only the marker does. So no marker + recent output
        // is still Idle, not Working.
        assert_eq!(
            classify(false, "", &working, &blocked, false, false, busy),
            AgentStatus::Idle
        );
        assert_eq!(
            classify(false, "", &working, &blocked, false, false, quiet),
            AgentStatus::Idle
        );
    }

    #[test]
    fn classify_marker_less_agent_uses_heuristic_only_when_opted_in() {
        let none: Vec<String> = Vec::new();
        assert_eq!(
            classify(false, "", &none, &none, true, true, Duration::from_secs(10),),
            AgentStatus::Done
        );
        assert_eq!(
            classify(
                false,
                "",
                &none,
                &none,
                false,
                true,
                Duration::from_millis(100),
            ),
            AgentStatus::Working
        );
        assert_eq!(
            classify(
                false,
                "",
                &none,
                &none,
                false,
                true,
                Duration::from_secs(10),
            ),
            AgentStatus::Idle
        );
        assert_eq!(
            classify(
                false,
                "typing echoed by a known provider",
                &none,
                &none,
                false,
                false,
                Duration::from_millis(100),
            ),
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
        // A bell/exit Done latches through later Idle samples.
        assert_eq!(latch_done(Some(Working), Done, false, true), (Done, true));
        assert_eq!(latch_done(Some(Done), Idle, true, true), (Done, true));
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
        // Bell/exit is precise enough to latch without semantic Working evidence.
        assert_eq!(latch_done(Some(Working), Done, false, false), (Done, true));
        assert_eq!(latch_done(Some(Done), Idle, true, false), (Idle, false));
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
    fn provider_readiness_ignores_fallback_idle_before_semantic_startup() {
        use AgentStatus::{Idle, Working};
        let settled = provider_settled_after_title(false, None, false);
        assert!(!settled);
        let settled = provider_settled_after_title(settled, Some(Working), false);
        assert!(!settled);
        let settled = provider_settled_after_title(settled, Some(Idle), false);
        assert!(settled);
        assert!(provider_settled_after_title(settled, Some(Working), false));
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
        let no_defer = Duration::ZERO;
        assert_eq!(
            next_paint_schedule(last, None, now, FOCUSED_STREAM_INTERVAL, no_defer),
            PaintSchedule::At(deadline)
        );
        assert_eq!(
            next_paint_schedule(
                last,
                Some(deadline),
                now + Duration::from_millis(1),
                FOCUSED_STREAM_INTERVAL,
                no_defer,
            ),
            PaintSchedule::KeepPending
        );
        assert_eq!(
            next_paint_schedule(
                last,
                Some(deadline),
                deadline,
                FOCUSED_STREAM_INTERVAL,
                no_defer
            ),
            PaintSchedule::Now
        );
    }

    #[test]
    fn no_defer_leaves_the_throttle_exactly_as_it_was() {
        // The floor is measured from `now`, so zero must be a no-op on every arm.
        let last = std::time::Instant::now();
        let idle = last + Duration::from_secs(5);
        assert_eq!(
            next_paint_schedule(last, None, idle, FOCUSED_STREAM_INTERVAL, Duration::ZERO),
            PaintSchedule::Now
        );
    }

    #[test]
    fn a_batch_that_hid_the_cursor_waits_even_after_a_quiet_gap() {
        // The flicker case: output resumes long after the last paint, so the
        // throttle would fire immediately and draw the cursorless mid-redraw
        // frame. The defer is what holds it back.
        let last = std::time::Instant::now();
        let now = last + Duration::from_secs(5);
        assert_eq!(
            next_paint_schedule(last, None, now, FOCUSED_STREAM_INTERVAL, Duration::ZERO),
            PaintSchedule::Now,
            "without the defer this batch paints at once"
        );
        assert_eq!(
            next_paint_schedule(last, None, now, FOCUSED_STREAM_INTERVAL, CURSOR_HIDE_HOLD),
            PaintSchedule::At(now + CURSOR_HIDE_HOLD),
            "with it, the matching cursor-show has time to arrive and coalesce"
        );
    }

    #[test]
    fn the_cursor_defer_never_paints_sooner_than_the_throttle_would() {
        // A defer may only ever push a paint later, never pull it earlier.
        let last = std::time::Instant::now();
        let now = last + Duration::from_millis(1);
        let throttled =
            next_paint_schedule(last, None, now, BACKGROUND_PAINT_INTERVAL, Duration::ZERO);
        let deferred =
            next_paint_schedule(last, None, now, BACKGROUND_PAINT_INTERVAL, CURSOR_HIDE_HOLD);
        let at = |s| match s {
            PaintSchedule::At(t) => t,
            other => panic!("expected At, got {other:?}"),
        };
        assert!(at(deferred) >= at(throttled));
        // Here the throttle is the later of the two and still wins.
        assert_eq!(at(throttled), last + BACKGROUND_PAINT_INTERVAL);
        assert_eq!(at(deferred), last + BACKGROUND_PAINT_INTERVAL);
    }

    #[test]
    fn the_hold_is_long_enough_to_bridge_a_split_but_short_enough_not_to_be_seen() {
        // It has to outlast the gap between two reads of one redraw, while staying
        // under the frame budget a viewer would notice as lag.
        assert!(CURSOR_HIDE_HOLD > FOCUSED_STREAM_INTERVAL);
        assert!(CURSOR_HIDE_HOLD <= Duration::from_millis(100));
    }

    #[test]
    fn a_status_change_in_a_batch_that_hid_the_cursor_waits_for_the_held_paint() {
        // A status edge often lands in the batch where the agent starts
        // redrawing. Repainting for it at once would draw the cursorless frame
        // the hold skips; the held paint carries the change instead.
        let last = std::time::Instant::now();
        let now = last + Duration::from_secs(5);
        let none = Duration::ZERO;
        let (min, defer) = paint_request(true, FOCUSED_STREAM_INTERVAL, CURSOR_HIDE_HOLD, none);
        assert_eq!(
            next_paint_schedule(last, None, now, min, defer),
            PaintSchedule::At(now + CURSOR_HIDE_HOLD)
        );
        // Without a hold the change paints at once, even inside the throttle.
        let just_painted = now - Duration::from_millis(1);
        let (min, defer) = paint_request(true, BACKGROUND_PAINT_INTERVAL, none, none);
        assert_eq!(
            next_paint_schedule(just_painted, None, now, min, defer),
            PaintSchedule::Now
        );
        // Output alone still waits for the throttle.
        let (min, defer) = paint_request(false, BACKGROUND_PAINT_INTERVAL, none, none);
        assert_eq!(
            next_paint_schedule(just_painted, None, now, min, defer),
            PaintSchedule::At(just_painted + BACKGROUND_PAINT_INTERVAL)
        );
    }

    #[test]
    fn a_status_change_in_a_later_batch_of_a_split_waits_for_the_pending_hold() {
        // The redraw arrives in two reads: the first hid the cursor and armed the
        // hold; the second still has it hidden and carries a status edge (an OSC
        // title, say). The edge must not paint the frame the hold is skipping.
        let last = std::time::Instant::now();
        let first = last + Duration::from_secs(5);
        let hold = Some(first + CURSOR_HIDE_HOLD);
        let second = first + Duration::from_millis(10);
        let held_for = status_paint_defer(hold, second);
        let (min, defer) = paint_request(true, FOCUSED_STREAM_INTERVAL, Duration::ZERO, held_for);
        assert_eq!(
            next_paint_schedule(last, hold, second, min, defer),
            PaintSchedule::KeepPending
        );
        // Output without a status change keeps upstream's schedule.
        let (min, defer) = paint_request(false, FOCUSED_STREAM_INTERVAL, Duration::ZERO, held_for);
        assert_eq!((min, defer), (FOCUSED_STREAM_INTERVAL, Duration::ZERO));
    }

    #[test]
    fn a_status_change_found_later_still_waits_for_a_pending_hold() {
        // The scan throttle means the runtime timer, not the batch, usually finds
        // the status edge, tens of milliseconds into the hold. It must ride the
        // held paint, not draw the cursorless frame itself.
        let last = std::time::Instant::now();
        let now = last + Duration::from_secs(5);
        let held = next_paint_schedule(last, None, now, FOCUSED_STREAM_INTERVAL, CURSOR_HIDE_HOLD);
        let hold = hold_after(held, CURSOR_HIDE_HOLD, None);
        assert_eq!(hold, Some(now + CURSOR_HIDE_HOLD));

        let later = now + Duration::from_millis(30);
        let defer = status_paint_defer(hold, later);
        assert_eq!(defer, CURSOR_HIDE_HOLD - Duration::from_millis(30));
        assert_eq!(
            next_paint_schedule(last, hold, later, Duration::ZERO, defer),
            PaintSchedule::KeepPending,
            "the held paint draws the change"
        );
        assert_eq!(
            hold_after(PaintSchedule::KeepPending, defer, hold),
            hold,
            "and the hold stays armed until it does"
        );
    }

    #[test]
    fn a_batch_that_replaces_the_held_paint_ends_the_hold() {
        // A cursor-show batch reschedules the paint on the throttle alone; from
        // then on a status change has no hold to wait for.
        let now = std::time::Instant::now();
        let hold = Some(now + CURSOR_HIDE_HOLD);
        let replaced = PaintSchedule::At(now + FOCUSED_STREAM_INTERVAL);
        assert_eq!(hold_after(replaced, Duration::ZERO, hold), None);
        assert_eq!(hold_after(PaintSchedule::Now, Duration::ZERO, hold), None);
        assert_eq!(status_paint_defer(None, now), Duration::ZERO);
        // A hold that has run out never delays anything.
        assert_eq!(
            status_paint_defer(hold, now + CURSOR_HIDE_HOLD * 2),
            Duration::ZERO
        );
    }
}

#[cfg(test)]
mod provider_switch_tests {
    // Import specifically: see `tests`.
    use super::{
        AgentStatus, RuntimeStatusState, TerminalStatusCoverage, TerminalStatusReason,
        TitleProvider, muse_title_status, screen_hold_after, status_is_attributable,
        title_status_trace, wall_time_at,
    };
    use crate::listener::{SWITCH_SETTLE, SWITCH_SLACK};
    use crate::program_status::ProgramStatusKind;
    use crate::session::{CommandSpec, LaunchBinding, PtyOutput, TerminalSession};
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    fn fixture() -> (Arc<TerminalSession>, crate::session::PtyReceiver) {
        #[cfg(windows)]
        let spec = CommandSpec::program(
            "cmd.exe",
            vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
        );
        #[cfg(unix)]
        let spec = CommandSpec::program("/bin/cat", vec![]);
        TerminalSession::spawn(spec, 80, 24).expect("spawn fixture")
    }

    mod program_name_tests {
        use super::{
            AgentStatus, RuntimeStatusState, TerminalStatusCoverage, TitleProvider, fixture,
        };
        use crate::view::{clean_agent_title, program_auto_title, title_status};
        use std::time::Instant;

        #[test]
        fn claude_glyphless_title_names_pane_while_program_status_is_current() {
            let (session, _receiver) = fixture();
            session.test_program_status(b"\x1b]7501;state=working\x07\x1b]0;Review changes\x07");
            assert_eq!(
                program_auto_title(&session, TitleProvider::Claude, false).as_deref(),
                Some("Review changes")
            );
            assert_eq!(
                title_status(TitleProvider::Claude, Some("Review changes"), None),
                None
            );
            let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
            runtime.reconcile(
                &session,
                TitleProvider::Claude,
                &[],
                &[],
                false,
                Instant::now(),
            );
            assert_eq!(runtime.snapshot(&session).status, AgentStatus::Working);
            session.kill();
        }

        #[test]
        fn claude_glyphless_title_is_ignored_until_claude_reports() {
            let (session, _receiver) = fixture();
            session.test_program_status(b"\x1b]0;Review changes\x07");
            assert_eq!(
                program_auto_title(&session, TitleProvider::Claude, false),
                None
            );
            assert_eq!(clean_agent_title("claude", "Review changes"), None);
            session.test_program_status(b"\x1b]7501;state=working:id=worker\x07");
            assert_eq!(
                program_auto_title(&session, TitleProvider::Claude, false),
                None,
                "a child report does not show the program dropped its glyphs"
            );
            session.test_program_status(b"\x1b]7501;state=working\x07");
            assert_eq!(
                program_auto_title(&session, TitleProvider::Claude, true),
                None
            );
            assert_eq!(
                program_auto_title(&session, TitleProvider::Codex, false),
                None
            );
            session.kill();
        }

        /// Once Claude has reported, it sends glyph-less titles for the rest
        /// of the launch, including while no root is current.
        #[test]
        fn claude_names_the_pane_after_clear_ris_and_a_switch_retire_its_root() {
            let (session, _receiver) = fixture();
            session.test_program_status(b"\x1b]7501;state=idle\x07\x1b]0;Review changes\x07");
            let name = || program_auto_title(&session, TitleProvider::Claude, false);
            assert_eq!(name().as_deref(), Some("Review changes"));
            // `/clear` and `/resume`.
            session.test_program_status(b"\x1b]7501;state=clear\x07\x1b]0;Fix the parser\x07");
            assert_eq!(session.program_status_snapshot().root, None);
            assert_eq!(name().as_deref(), Some("Fix the parser"));
            // RIS.
            session.test_program_status(b"\x1bc\x1b]0;After reset\x07");
            assert_eq!(session.program_status_snapshot().root, None);
            assert_eq!(name().as_deref(), Some("After reset"));
            // A report inside an out-of-band switch window, wiped by the
            // switch, and the conversation's next title.
            session.test_program_status(b"\x1b]7501;state=working\x07");
            let now = std::time::SystemTime::now();
            session.provider_switched(now, now);
            assert_eq!(session.program_status_snapshot().root, None);
            session.test_program_status(b"\x1b]0;Resumed topic\x07");
            assert_eq!(name().as_deref(), Some("Resumed topic"));
            assert_eq!(
                title_status(TitleProvider::Claude, Some("Resumed topic"), None),
                None
            );
            assert_eq!(
                program_auto_title(&session, TitleProvider::Claude, true),
                None
            );
            session.kill();
        }

        #[test]
        fn program_status_title_field_names_pane() {
            let (session, _receiver) = fixture();
            session.test_program_status(b"\x1b]0;Other topic\x07\x1b]7501;state=blocked:kind=permission:title=UFNQIHBhbmU=\x07");
            assert_eq!(
                program_auto_title(&session, TitleProvider::Claude, false).as_deref(),
                Some("PSP pane")
            );
            session.test_program_status(b"\x1b]7501;state=working:id=child:title=Q2hpbGQ=\x07");
            assert_eq!(
                program_auto_title(&session, TitleProvider::Claude, false).as_deref(),
                Some("PSP pane")
            );
            let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
            runtime.reconcile(
                &session,
                TitleProvider::Claude,
                &[],
                &[],
                false,
                Instant::now(),
            );
            assert_eq!(runtime.snapshot(&session).status, AgentStatus::Blocked);
            session.test_program_status(b"\x1b]7501;state=working\x07");
            assert_eq!(
                program_auto_title(&session, TitleProvider::Claude, false).as_deref(),
                Some("Other topic")
            );
            session.test_program_status(b"\x1b]7501;state=clear\x07");
            assert_eq!(
                program_auto_title(&session, TitleProvider::Claude, false).as_deref(),
                Some("Other topic"),
                "the reported title goes with its root; the pane title remains"
            );
            session.kill();
        }

        #[test]
        fn claude_2_1_310_control_stream_names_pane_with_and_without_program_status() {
            for (wire, supported) in [
                (
                    include_bytes!("fixtures/claude-2.1.310-7501.osc").as_slice(),
                    true,
                ),
                (
                    include_bytes!("fixtures/claude-2.1.310-no-7501.osc").as_slice(),
                    false,
                ),
            ] {
                let (session, _receiver) = fixture();
                // The real stream contains both BEL and ST; split every control frame.
                for byte in wire {
                    session.test_program_status(std::slice::from_ref(byte));
                }
                assert_eq!(session.program_status_snapshot().root.is_some(), supported);
                let name = program_auto_title(&session, TitleProvider::Claude, false)
                    .or_else(|| clean_agent_title("claude", &session.title().unwrap()));
                assert_eq!(name.as_deref(), Some("Exact OK reply"));
                if supported {
                    let mut runtime =
                        RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
                    runtime.reconcile(
                        &session,
                        TitleProvider::Claude,
                        &[],
                        &[],
                        false,
                        Instant::now(),
                    );
                    assert_eq!(runtime.snapshot(&session).status, AgentStatus::Done);
                }
                session.kill();
            }
        }
    }

    #[test]
    fn known_launch_keeps_program_report_through_its_first_confirming_title() {
        let id = "019f95d7-db31-7db0-904d-9e08330e0001";
        for initial in [Some(id.to_string()), None] {
            for title_id in [id.to_string(), format!("{}...", &id[..29])] {
                #[cfg(windows)]
                let mut spec = CommandSpec::program(
                    "cmd.exe",
                    vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
                );
                #[cfg(unix)]
                let mut spec = CommandSpec::program("/bin/cat", vec![]);
                spec.binding = LaunchBinding::Known(initial.clone());
                let (session, _receiver) = TerminalSession::spawn(spec, 80, 24).unwrap();
                assert_eq!(session.binding_revision(), 1);
                let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
                runtime.reconcile(
                    &session,
                    TitleProvider::Codex,
                    &[],
                    &[],
                    false,
                    Instant::now(),
                );
                session.test_program_status(b"\x1b]7501;state=working\x07");
                assert!(session.program_status_snapshot().root.is_some());
                session.test_program_status(format!("\x1b]0;{title_id} | Working\x07").as_bytes());
                runtime.reconcile(
                    &session,
                    TitleProvider::Codex,
                    &[],
                    &[],
                    false,
                    Instant::now(),
                );
                assert_eq!(session.binding_revision(), 1);
                assert_eq!(runtime.snapshot(&session).status, AgentStatus::Working);
                assert_eq!(
                    session.program_status_snapshot().root.unwrap().state,
                    crate::program_status::ProgramState::Working
                );
                session.test_program_status(
                    b"\x1b]0;019f95d7-db31-7db0-904d-9e08330e0002 | Ready\x07",
                );
                assert_eq!(session.binding_revision(), 2);
                assert_eq!(session.program_status_snapshot().root, None);
                session.kill();
            }
        }
    }

    #[test]
    fn synchronized_second_title_consumes_known_launch_confirmation_once() {
        let id = "019f95d7-db31-7db0-904d-9e08330e0001";
        #[cfg(windows)]
        let mut spec = CommandSpec::program(
            "cmd.exe",
            vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
        );
        #[cfg(unix)]
        let mut spec = CommandSpec::program("/bin/cat", vec![]);
        spec.binding = LaunchBinding::Known(Some(id.to_string()));
        let (session, _receiver) = TerminalSession::spawn(spec, 80, 24).unwrap();
        let abbreviated = format!("{}...", &id[..29]);
        session.test_program_status(
            format!(
                "\x1b[?2026h\x1b]0;{id} | Working\x07\x1b]0;{abbreviated} | Working\x07\x1b]7501;state=blocked:kind=permission\x07\x1b[?2026l"
            )
            .as_bytes(),
        );
        assert_eq!(session.binding_revision(), 2);
        let program_status = session.program_status_snapshot();
        assert_eq!(program_status.binding_revision, 2);
        assert_eq!(
            program_status.root,
            Some(crate::program_status::RootStatus {
                state: crate::program_status::ProgramState::Blocked,
                kind: Some(crate::program_status::ProgramStatusKind::Permission),
                title: None,
            })
        );
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            Instant::now(),
        );
        assert_eq!(runtime.snapshot(&session).status, AgentStatus::Blocked);
        session.kill();
    }

    #[test]
    fn synchronized_stale_report_cannot_cross_an_out_of_band_switch() {
        let (session, _receiver) = fixture();
        let hook = SystemTime::now();
        session.process_drained(&drained(&[(
            "\x1b[?2026h",
            hook - Duration::from_millis(500),
        )]));
        session.provider_switched_at(hook, hook, hook);
        session.process_drained(&drained(&[(
            "\x1b]7501;state=working\x07\x1b[?2026l",
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        assert_eq!(session.program_status_snapshot().root, None);
        session.process_drained(&drained(&[(
            "\x1b]7501;state=working\x07",
            hook + SWITCH_SLACK + Duration::from_millis(200),
        )]));
        assert_eq!(
            session.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Working
        );
        session.kill();
    }

    #[test]
    fn fresh_program_report_after_stale_sync_end_in_one_read_is_attributable() {
        let (session, _receiver) = fixture();
        let hook = SystemTime::now();
        session.process_drained(&drained(&[(
            "\x1b[?2026h",
            hook - Duration::from_millis(500),
        )]));
        session.provider_switched_at(hook, hook, hook);
        session.process_drained(&drained(&[(
            "\x1b]7501;state=done\x07\x1b[?2026l\x1b]7501;state=working\x07",
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        assert_eq!(
            session.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Working
        );
        session.kill();
    }

    #[test]
    fn title_binding_orders_program_reports_within_one_read() {
        let title = b"\x1b]0;019f95d7-db31-7db0-904d-9e08330e0001 | Ready\x07";
        let working = b"\x1b]7501;state=working\x07";

        let (before, _receiver) = fixture();
        let mut wire = working.to_vec();
        wire.extend_from_slice(title);
        before.process_output(&wire);
        assert_eq!(before.binding_revision(), 1);
        assert_eq!(before.program_status_snapshot().root, None);
        before.kill();

        let (after, _receiver) = fixture();
        let mut wire = title.to_vec();
        wire.extend_from_slice(working);
        after.process_output(&wire);
        assert_eq!(after.binding_revision(), 1);
        assert_eq!(
            after.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Working
        );
        after.process_output(
            b"\x1b]7501;state=idle\x07\x1b]0;019f95d7-db31-7db0-904d-9e08330e0002 | Ready\x07",
        );
        let switched = after.program_status_snapshot();
        assert_eq!(after.binding_revision(), 2);
        assert_eq!(switched.root, None);
        assert_eq!(switched.idle_completions, 0);
        after.kill();

        let (synchronized, _receiver) = fixture();
        synchronized.process_output(
            b"\x1b[?2026h\x1b]0;019f95d7-db31-7db0-904d-9e08330e0003 | Ready\x07\x1b]7501;state=working\x07\x1b[?2026l",
        );
        assert_eq!(synchronized.binding_revision(), 1);
        assert_eq!(
            synchronized.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Working
        );
        synchronized.kill();

        let (retired, _receiver) = fixture();
        retired.process_output(
            b"\x1b[?2026h\x1b]7501;state=working\x07\x1b]0;019f95d7-db31-7db0-904d-9e08330e0004 | Ready\x07\x1b[?2026l",
        );
        assert_eq!(retired.binding_revision(), 1);
        assert_eq!(retired.program_status_snapshot().root, None);
        retired.kill();

        let (split, _receiver) = fixture();
        split.process_output(b"\x1b[?2026h\x1b]0;019f95d7-db31-7db0-904d-9e08330e0005 | Ready\x07");
        split.process_output(b"\x1b]7501;state=working\x07");
        assert_eq!(split.binding_revision(), 0);
        assert_eq!(split.program_status_snapshot().root, None);
        split.process_output(b"\x1b[?2026l");
        assert_eq!(split.binding_revision(), 1);
        assert_eq!(
            split.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Working
        );
        split.kill();

        let (twice, _receiver) = fixture();
        twice.process_output(
            b"\x1b[?2026h\x1b]0;019f95d7-db31-7db0-904d-9e08330e0006 | Ready\x07\x1b]7501;state=working\x07\x1b]0;019f95d7-db31-7db0-904d-9e08330e0007 | Ready\x07\x1b]7501;state=blocked\x07\x1b[?2026l",
        );
        assert_eq!(twice.binding_revision(), 2);
        assert_eq!(
            twice.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Blocked
        );
        twice.kill();

        let (st, _receiver) = fixture();
        st.process_output(
            b"\x1b[?2026h\x1b]0;019f95d7-db31-7db0-904d-9e08330e0008 | Re\x01ady\x1b",
        );
        st.process_output(b"\\\x1b]7501;state=working\x07");
        assert_eq!(st.program_status_snapshot().root, None);
        st.process_output(b"\x1b[?2026l");
        assert_eq!(st.binding_revision(), 1);
        assert_eq!(
            st.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Working
        );
        st.process_output(
            b"\x1b]0;019f95d7-db31-7db0-904d-9e08330e0009 | Ready\x1b\\\x1b]7501;state=blocked\x07",
        );
        assert_eq!(st.binding_revision(), 2);
        assert_eq!(
            st.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Blocked
        );
        st.kill();

        let (oversized, _receiver) = fixture();
        let title = format!(
            "\x1b]0;019f95d7-db31-7db0-904d-9e08330e0010 | {}\x07\x1b]7501;state=working\x07",
            "x".repeat(5000)
        );
        oversized.process_output(title.as_bytes());
        assert_eq!(oversized.binding_revision(), 1);
        assert_eq!(oversized.program_status_snapshot().root, None);
        oversized.process_output(b"\x1b]7501;state=blocked\x07");
        assert_eq!(
            oversized.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Blocked
        );
        oversized.process_output(
            b"\x1b]0;019f95d7-db31-7db0-904d-9e08330e0010 | Ready\x07\x1b]7501;state=idle\x07",
        );
        assert_eq!(oversized.binding_revision(), 1);
        assert_eq!(
            oversized.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Idle
        );
        oversized.kill();
    }

    #[test]
    fn unterminated_synchronized_title_flushes_on_runtime_deadline() {
        let (session, _receiver) = fixture();
        session.process_output(
            b"\x1b[?2026h\x1b]0;019f95d7-db31-7db0-904d-9e08330e0016 | Ready\x07\x1b]7501;state=working\x07",
        );
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let (_, deadline) = runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            Instant::now(),
        );
        if session.binding_revision() == 0 {
            let sync_deadline = session
                .program_status_sync_deadline()
                .expect("BSU has a timeout");
            assert!(deadline.is_some_and(|deadline| deadline <= sync_deadline));
            assert_eq!(runtime.snapshot(&session).status, AgentStatus::Idle);
            std::thread::sleep(
                sync_deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(10),
            );
            runtime.reconcile(
                &session,
                TitleProvider::Codex,
                &[],
                &[],
                false,
                Instant::now(),
            );
        }
        assert_eq!(session.binding_revision(), 1);
        assert_eq!(runtime.snapshot(&session).status, AgentStatus::Working);
        session.kill();
    }

    #[test]
    fn shell_prompt_retires_busy_program_status_in_the_terminal_drain() {
        let (session, _receiver) = fixture();
        session.process_output(b"\x1b]7501;state=blocked\x07\x1b]133;A\x07");
        assert_eq!(session.program_status_snapshot().root, None);
        session.process_output(b"\x1b]7501;state=done\x07\x1b]133;A\x07");
        assert_eq!(
            session.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Done
        );
        session.kill();
    }

    #[test]
    fn program_status_root_overrides_visible_fallback_until_clear_or_exit() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let now = Instant::now();
        let mut sample = |wire: &[u8], offset_ms| {
            session.test_program_status(wire);
            runtime.reconcile(
                &session,
                TitleProvider::Codex,
                &[],
                &[],
                false,
                now + Duration::from_millis(offset_ms),
            );
            runtime.snapshot(&session)
        };
        let working = sample(b"\x1b]7501;state=working\x07", 0);
        assert_eq!(working.status, AgentStatus::Working);
        assert!(
            !working.completion_ready,
            "a Working report alone is not readiness"
        );
        assert_eq!(working.coverage, TerminalStatusCoverage::ProgramStatus);
        assert_eq!(working.reason, TerminalStatusReason::ProgramStatus(None));

        let child = sample(b"\x1b]7501;state=blocked:kind=question:id=worker\x07", 1);
        assert_eq!(child.status, AgentStatus::Working);
        assert!(!child.completion_ready);
        let blocked = sample(b"\x1b]7501;state=blocked:kind=permission\x07", 2);
        assert_eq!(blocked.status, AgentStatus::Blocked);
        assert!(blocked.completion_ready);
        assert_eq!(
            blocked.reason,
            TerminalStatusReason::ProgramStatus(Some(ProgramStatusKind::Permission))
        );
        let idle = sample(b"\x1b]7501;state=idle\x07", 3);
        assert_eq!(idle.status, AgentStatus::Idle);
        let done = sample(b"\x1b]7501;state=done\x07", 4);
        assert_eq!(done.status, AgentStatus::Done);
        assert_eq!(done.reason, TerminalStatusReason::ProgramStatus(None));
        let error = sample(b"\x1b]7501;state=error\x07", 5);
        assert_eq!(error.status, AgentStatus::Done);
        assert_eq!(error.reason, TerminalStatusReason::ProgramStatusError);
        let clear = sample(b"\x1b]7501;state=clear\x07", 6);
        assert_eq!(clear.status, AgentStatus::Idle);
        assert_eq!(clear.coverage, TerminalStatusCoverage::ProviderVisible);
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            true,
            now + Duration::from_millis(7),
        );
        let exit = runtime.snapshot(&session);
        assert_eq!(exit.status, AgentStatus::Done);
        assert_eq!(exit.reason, TerminalStatusReason::ProcessExit);
        session.kill();

        let (restarted, _receiver) = fixture();
        let mut fresh = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        fresh.reconcile(
            &restarted,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            now + Duration::from_millis(8),
        );
        assert_eq!(fresh.snapshot(&restarted).status, AgentStatus::Idle);
        assert_eq!(fresh.coverage, TerminalStatusCoverage::ProviderVisible);
        restarted.kill();
    }

    /// `cat` of a file holding reports, or a reporter run inside the pane and
    /// then lost (crash, SIGKILL, ssh drop), says nothing about these panes.
    #[test]
    fn reports_in_a_shell_opencode_or_amp_pane_cannot_set_status() {
        for program in ["claude", "codex.exe", "grok", "muse.cmd"] {
            assert!(
                TitleProvider::from_program(program).reports_program_status(),
                "{program}"
            );
        }
        for program in ["bash", "pwsh.exe", "opencode", "amp"] {
            let provider = TitleProvider::from_program(program);
            assert!(!provider.reports_program_status(), "{program}");
            let (session, _receiver) = fixture();
            let mut runtime = RuntimeStatusState::new(super::status_coverage(provider, &[], &[]));
            let now = Instant::now();
            session.test_program_status(
                b"\x1b]7501;state=blocked:kind=permission\x07\x1b]7501;state=working\x07",
            );
            let (_, deadline) = runtime.reconcile(&session, provider, &[], &[], false, now);
            let snapshot = runtime.snapshot(&session);
            assert_ne!(snapshot.status, AgentStatus::Blocked, "{program}");
            assert_ne!(
                snapshot.coverage,
                TerminalStatusCoverage::ProgramStatus,
                "{program}"
            );
            assert!(
                !matches!(
                    snapshot.reason,
                    TerminalStatusReason::ProgramStatus(_)
                        | TerminalStatusReason::ProgramStatusError
                ),
                "{program}"
            );
            // The activity fallback owns the pane: recent output reads as
            // Working with a deadline to go quiet, never Working indefinitely.
            assert_eq!(
                deadline.is_some(),
                snapshot.status == AgentStatus::Working,
                "{program}"
            );
            assert!(
                matches!(
                    snapshot.reason,
                    TerminalStatusReason::RecentOutput | TerminalStatusReason::Quiet
                ),
                "{program}: {:?}",
                snapshot.reason
            );
            session.test_program_status(b"\x1b]7501;state=done\x07");
            runtime.reconcile(
                &session,
                provider,
                &[],
                &[],
                false,
                now + Duration::from_millis(1),
            );
            assert_eq!(runtime.snapshot(&session).completion_revision, 0);
            session.kill();
        }
    }

    #[test]
    fn visible_blocked_beats_a_working_report_that_missed_it() {
        // Codex: the action-required title.
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let now = Instant::now();
        session.test_program_status(b"\x1b]7501;state=working\x07\x1b]0;[ ! ] Action Required\x07");
        runtime.reconcile(&session, TitleProvider::Codex, &[], &[], false, now);
        let blocked = runtime.snapshot(&session);
        assert_eq!(blocked.status, AgentStatus::Blocked);
        assert_eq!(blocked.reason, TerminalStatusReason::ProviderTitle);
        assert_eq!(blocked.coverage, TerminalStatusCoverage::ProgramStatus);
        assert!(blocked.attributable);
        session.test_program_status(b"\x1b]0;Fix the parser | Working\x07");
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            now + Duration::from_millis(100),
        );
        let working = runtime.snapshot(&session);
        assert_eq!(working.status, AgentStatus::Working);
        assert_eq!(working.reason, TerminalStatusReason::ProgramStatus(None));
        session.kill();

        // Claude: its permission prompt on screen.
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        session.test_program_status(
            "\x1b]7501;state=working\x07Do you want to proceed?\r\n\u{276f} 1. Yes\r\n  2. No\r\n\r\nEsc to cancel\r\n"
                .as_bytes(),
        );
        runtime.reconcile(&session, TitleProvider::Claude, &[], &[], false, now);
        let blocked = runtime.snapshot(&session);
        assert_eq!(blocked.status, AgentStatus::Blocked);
        assert_eq!(blocked.reason, TerminalStatusReason::ProviderScreen);
        assert!(blocked.attributable);
        // Its own report of the prompt keeps the report's kind.
        session.test_program_status(b"\x1b]7501;state=blocked:kind=permission\x07");
        runtime.reconcile(
            &session,
            TitleProvider::Claude,
            &[],
            &[],
            false,
            now + Duration::from_millis(100),
        );
        assert_eq!(
            runtime.snapshot(&session).reason,
            TerminalStatusReason::ProgramStatus(Some(ProgramStatusKind::Permission))
        );
        session.kill();

        // A configured blocked marker beats Working only; other reports stand.
        let markers = vec!["Approve this?".to_string()];
        for (report, status) in [
            ("working", AgentStatus::Blocked),
            ("idle", AgentStatus::Idle),
            ("done", AgentStatus::Done),
        ] {
            let (session, _receiver) = fixture();
            let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
            session.test_program_status(
                format!("\x1b]7501;state={report}\x07Approve this?\r\n").as_bytes(),
            );
            runtime.reconcile(&session, TitleProvider::Grok, &[], &markers, false, now);
            let snapshot = runtime.snapshot(&session);
            assert_eq!(snapshot.status, status, "{report}");
            if status == AgentStatus::Blocked {
                assert_eq!(snapshot.reason, TerminalStatusReason::BlockedMarker);
            }
            session.kill();
        }
    }

    /// Codex 0.165.0 `codex resume <id>` with no prompt, captured from the
    /// system ConPTY (thread id replaced): it reports Working while it boots,
    /// after the Ready title that already settled the provider.
    #[test]
    fn codex_resume_reports_working_at_boot_without_a_turn() {
        let thread = "019f95d7-db31-7db0-904d-9e08330e0021";
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let now = Instant::now();
        let mut sample = |wire: String, offset_ms| {
            session.test_program_status(wire.as_bytes());
            runtime.reconcile(
                &session,
                TitleProvider::Codex,
                &[],
                &[],
                false,
                now + Duration::from_millis(offset_ms),
            );
            runtime.snapshot(&session)
        };
        // 2.25 s and 2.26 s.
        let ready = sample(format!("\x1b]0;Ready\x07\x1b]0;{thread} | Ready\x07"), 0);
        assert_eq!(ready.status, AgentStatus::Idle);
        assert!(ready.completion_ready, "the Ready title settles Codex");
        // 2.68 s.
        let booting = sample(
            format!(
                "\x1b]0;{thread} | Starting \u{283c}\x07\x1b]7501;state=working:app=codex\x1b\\"
            ),
            420,
        );
        assert_eq!(booting.status, AgentStatus::Working);
        assert_eq!(booting.reason, TerminalStatusReason::ProgramStatus(None));
        assert!(!session.has_submitted_turn());
        // 2.70 s.
        let idle = sample(
            format!("\x1b]0;{thread} | Ready\x07\x1b]7501;state=idle:app=codex\x1b\\"),
            440,
        );
        assert_eq!(idle.status, AgentStatus::Idle);
        assert_eq!(idle.completion_revision, 0, "no turn ran");
        session.kill();

        // A Working report with no title before it is not readiness.
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        session.test_program_status(b"\x1b]7501;state=working:app=codex\x1b\\");
        runtime.reconcile(&session, TitleProvider::Codex, &[], &[], false, now);
        assert!(!runtime.snapshot(&session).completion_ready);
        session.test_program_status(b"\x1b]7501;state=idle:app=codex\x1b\\");
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            now + Duration::from_millis(20),
        );
        assert!(runtime.snapshot(&session).completion_ready);
        session.kill();
    }

    #[test]
    fn incomplete_st_cannot_replace_the_current_program_status() {
        let (session, _receiver) = fixture();
        session.test_program_status(b"\x1b]7501;state=working\x07");
        session.test_program_status(b"\x1b]7501;state=done\x1b");
        assert_eq!(
            session.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Working
        );
        session.test_program_status(b"[2J");
        assert_eq!(
            session.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Working
        );
        session.test_program_status(b"\x1b]7501;state=done\x1b");
        session.test_program_status(b"\\");
        assert_eq!(
            session.program_status_snapshot().root.unwrap().state,
            crate::program_status::ProgramState::Done
        );
        session.kill();
    }

    #[test]
    fn codex_idle_after_working_finishes_only_an_armed_turn_and_resets_on_resume() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let now = Instant::now();
        session.test_program_status(b"\x1b]7501;state=working\x07\x1b]7501;state=idle\x07");
        runtime.reconcile(&session, TitleProvider::Codex, &[], &[], false, now);
        assert_eq!(runtime.snapshot(&session).status, AgentStatus::Idle);

        session.mark_turn_submitted();
        session.test_program_status(b"\x1b]7501;state=working\x07\x1b]7501;state=idle\x07");
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            now + Duration::from_millis(1),
        );
        assert_eq!(runtime.snapshot(&session).status, AgentStatus::Done);

        let switched = SystemTime::now() - Duration::from_secs(5);
        session.provider_switched_at(switched, switched, switched);
        session.test_program_status(b"\x1b]7501;state=idle\x07");
        // The test helper now uses the production output parser, so the new
        // report itself is fresh evidence after the out-of-band switch.
        assert!(!session.runtime_evidence(None).settling);
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            now + Duration::from_millis(2),
        );
        assert_eq!(runtime.snapshot(&session).status, AgentStatus::Idle);
        assert!(runtime.snapshot(&session).attributable);

        session.process_drained(&drained(&[("fresh", SystemTime::now())]));
        session.mark_turn_submitted();
        session.test_program_status(b"\x1b]7501;state=working\x07");
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            now + Duration::from_millis(3),
        );
        assert_eq!(runtime.snapshot(&session).status, AgentStatus::Working);
        session.test_program_status(b"\x1b]7501;state=blocked\x07");
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            now + Duration::from_millis(4),
        );
        assert_eq!(runtime.snapshot(&session).status, AgentStatus::Blocked);
        session.test_program_status(b"\x1b]7501;state=idle\x07");
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            now + Duration::from_millis(5),
        );
        assert_eq!(runtime.snapshot(&session).status, AgentStatus::Done);
        session.kill();
    }

    #[test]
    fn codex_initial_title_after_submission_keeps_only_its_own_protocol_completion() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let now = Instant::now();
        session.paste("A real prompt");
        session.mark_prompt_submitted();
        session.process_output(b"\x1b]0;019f95d7-db31-7db0-904d-9e08330e0011 | Ready\x07");
        assert_eq!(session.binding_revision(), 1);
        assert!(!session.runtime_evidence(None).submitted);
        assert_eq!(session.first_submission_binding(), None);
        session.test_program_status(
            b"\x1b]7501;state=working\x07\x1b]7501;state=blocked\x07\x1b]7501;state=idle\x07",
        );
        runtime.reconcile(&session, TitleProvider::Codex, &[], &[], false, now);
        let first = runtime.snapshot(&session);
        assert_eq!(first.status, AgentStatus::Done);
        assert_eq!(first.completion_revision, 1);
        assert_eq!(session.first_submission_binding(), None);

        session.process_output(b"\x1b]0;019f95d7-db31-7db0-904d-9e08330e0012 | Ready\x07");
        session.test_program_status(b"\x1b]7501;state=working\x07\x1b]7501;state=idle\x07");
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            now + Duration::from_millis(1),
        );
        let switched = runtime.snapshot(&session);
        assert_eq!(switched.status, AgentStatus::Idle);
        assert_eq!(switched.completion_revision, 0);
        session.kill();

        let (selector, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        selector.mark_turn_submitted(); // Bare Enter selecting a resumed session.
        selector.process_output(b"\x1b]0;019f95d7-db31-7db0-904d-9e08330e0013 | Ready\x07");
        selector.test_program_status(b"\x1b]7501;state=working\x07\x1b]7501;state=idle\x07");
        runtime.reconcile(&selector, TitleProvider::Codex, &[], &[], false, now);
        assert_eq!(runtime.snapshot(&selector).status, AgentStatus::Idle);
        selector.kill();

        let (navigation, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        navigation.write_input(b"\x1b[B"); // Down in the resume picker.
        assert!(navigation.input_snapshot().1);
        navigation.mark_turn_submitted();
        navigation.process_output(b"\x1b]0;019f95d7-db31-7db0-904d-9e08330e0014 | Ready\x07");
        navigation.test_program_status(b"\x1b]7501;state=working\x07\x1b]7501;state=idle\x07");
        runtime.reconcile(&navigation, TitleProvider::Codex, &[], &[], false, now);
        assert_eq!(runtime.snapshot(&navigation).status, AgentStatus::Idle);
        navigation.kill();
    }

    #[test]
    fn known_initial_prompt_keeps_working_across_first_uuid_title() {
        for title in [
            "019f95d7-db31-7db0-904d-9e08330e0015",
            "01a163d0-b2d5-7232-8ffe-fb1e6...",
        ] {
            for sync in [false, true] {
                for title_first in [false, true] {
                    let (session, _receiver) = fixture();
                    let mut runtime =
                        RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
                    session.paste("A real prompt");
                    session.mark_prompt_submitted();
                    let working = "\x1b]7501;state=working\x07";
                    let title_frame = format!("\x1b]0;{title} | Ready\x07");
                    let idle = "\x1b]7501;state=idle\x07";
                    let wire = format!(
                        "{}{}{}{}{}",
                        if sync { "\x1b[?2026h" } else { "" },
                        if title_first { &title_frame } else { working },
                        if title_first { working } else { &title_frame },
                        idle,
                        if sync { "\x1b[?2026l" } else { "" },
                    );
                    session.test_program_status(wire.as_bytes());
                    runtime.reconcile(
                        &session,
                        TitleProvider::Codex,
                        &[],
                        &[],
                        false,
                        Instant::now(),
                    );
                    let snapshot = runtime.snapshot(&session);
                    assert_eq!(
                        session.binding_revision(),
                        1,
                        "{title} sync={sync} first={title_first}"
                    );
                    assert_eq!(
                        snapshot.status,
                        AgentStatus::Done,
                        "{title} sync={sync} first={title_first}"
                    );
                    assert_eq!(
                        snapshot.completion_revision, 1,
                        "{title} sync={sync} first={title_first}"
                    );
                    session.kill();
                }
            }
        }
    }

    #[test]
    fn protocol_idle_is_authoritative_except_for_codex_current_binding_work() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let now = Instant::now();
        session.mark_turn_submitted();
        session.test_program_status(b"\x1b]7501;state=working\x07\x1b]7501;state=idle\x07");
        runtime.reconcile(&session, TitleProvider::Claude, &[], &[], false, now);
        let cancelled = runtime.snapshot(&session);
        assert_eq!(cancelled.status, AgentStatus::Idle);
        assert_eq!(cancelled.completion_revision, 0);

        // A fallback Working title is not PSP Working evidence. Even when it
        // precedes the Idle report in the same drain, it cannot finish a turn.
        session.process_output("\x1b]0;◐ Task\x07\x1b]7501;state=idle\x07".as_bytes());
        runtime.reconcile(
            &session,
            TitleProvider::Claude,
            &[],
            &[],
            false,
            now + Duration::from_millis(1),
        );
        let idle = runtime.snapshot(&session);
        assert_eq!(idle.status, AgentStatus::Idle);
        assert_eq!(idle.completion_revision, 0);
        session.kill();
    }

    #[test]
    fn consecutive_coalesced_codex_turns_have_distinct_completion_revisions() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let now = Instant::now();
        session.mark_turn_submitted();
        for expected in [1, 2] {
            session.test_program_status(b"\x1b]7501;state=working\x07\x1b]7501;state=idle\x07");
            runtime.reconcile(
                &session,
                TitleProvider::Codex,
                &[],
                &[],
                false,
                now + Duration::from_millis(expected),
            );
            let snapshot = runtime.snapshot(&session);
            assert_eq!(snapshot.status, AgentStatus::Done);
            assert_eq!(snapshot.completion_revision, expected);
        }
        session.kill();
    }

    #[test]
    fn clear_or_ris_then_idle_retires_previous_codex_completion() {
        for reset in [b"\x1b]7501;state=clear\x07".as_slice(), b"\x1bc".as_slice()] {
            let (session, _receiver) = fixture();
            let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
            session.mark_turn_submitted();
            session.test_program_status(b"\x1b]7501;state=working\x07\x1b]7501;state=idle\x07");
            runtime.reconcile(
                &session,
                TitleProvider::Codex,
                &[],
                &[],
                false,
                Instant::now(),
            );
            assert_eq!(runtime.snapshot(&session).status, AgentStatus::Done);
            let mut wire = reset.to_vec();
            wire.extend_from_slice(b"\x1b]7501;state=idle\x07");
            session.test_program_status(&wire);
            runtime.reconcile(
                &session,
                TitleProvider::Codex,
                &[],
                &[],
                false,
                Instant::now(),
            );
            let snapshot = runtime.snapshot(&session);
            assert_eq!(snapshot.status, AgentStatus::Idle, "reset={reset:?}");
            assert_eq!(snapshot.completion_revision, 1);
            session.kill();
        }
    }

    #[test]
    fn coalesced_completion_pulses_when_newer_work_is_current() {
        for completion in ["idle", "done"] {
            let (session, _receiver) = fixture();
            let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
            session.mark_turn_submitted();
            let wire = format!(
                "\x1b]7501;state=working\x07\x1b]7501;state={completion}\x07\x1b]7501;state=working\x07"
            );
            session.test_program_status(wire.as_bytes());
            runtime.reconcile(
                &session,
                TitleProvider::Codex,
                &[],
                &[],
                false,
                Instant::now(),
            );
            let snapshot = runtime.snapshot(&session);
            assert_eq!(snapshot.status, AgentStatus::Working, "{completion}");
            assert_eq!(snapshot.completion_revision, 1, "{completion}");
            session.kill();
        }
    }

    #[test]
    fn completion_revision_survives_clear_or_exit_to_fallback() {
        for exited in [false, true] {
            let (session, _receiver) = fixture();
            let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
            session.test_program_status(b"\x1b]7501;state=working\x07");
            runtime.reconcile(
                &session,
                TitleProvider::Claude,
                &[],
                &[],
                false,
                Instant::now(),
            );
            assert_eq!(runtime.snapshot(&session).completion_revision, 0);
            if exited {
                session.test_program_status(b"\x1b]7501;state=done\x07");
            } else {
                session.test_program_status(b"\x1b]7501;state=done\x07\x1b]7501;state=clear\x07");
            }
            runtime.reconcile(
                &session,
                TitleProvider::Claude,
                &[],
                &[],
                exited,
                Instant::now(),
            );
            let snapshot = runtime.snapshot(&session);
            assert_eq!(snapshot.coverage, TerminalStatusCoverage::ProviderVisible);
            assert_eq!(snapshot.completion_revision, 1);
            session.kill();
        }
    }

    #[test]
    fn program_working_blocked_done_has_one_completion_revision() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let now = Instant::now();
        session.mark_turn_submitted();
        for (wire, status, completion_revision) in [
            (
                b"\x1b]7501;state=working\x07".as_slice(),
                AgentStatus::Working,
                0,
            ),
            (
                b"\x1b]7501;state=blocked\x07".as_slice(),
                AgentStatus::Blocked,
                0,
            ),
            (b"\x1b]7501;state=done\x07".as_slice(), AgentStatus::Done, 1),
        ] {
            session.test_program_status(wire);
            runtime.reconcile(&session, TitleProvider::Claude, &[], &[], false, now);
            let snapshot = runtime.snapshot(&session);
            assert_eq!(snapshot.status, status);
            assert_eq!(snapshot.completion_revision, completion_revision);
        }
        session.kill();
    }

    fn drained(reads: &[(&str, SystemTime)]) -> PtyOutput {
        let mut output = PtyOutput::default();
        for (bytes, read_at) in reads {
            output.push(bytes.as_bytes(), *read_at);
        }
        output
    }

    #[test]
    fn only_providers_with_a_startup_protocol_receive_a_ctl_task() {
        for program in ["claude", "codex.exe", "grok"] {
            assert!(TitleProvider::from_program(program).has_startup_protocol());
        }
        for program in ["muse", "muse.cmd", "opencode", "bash"] {
            assert!(!TitleProvider::from_program(program).has_startup_protocol());
        }
    }

    #[test]
    fn muse_titles_count_only_as_this_panes_cwd_label() {
        use AgentStatus::{Idle, Working};
        assert_eq!(muse_title_status(Some("repo"), Some("repo")), Some(Idle));
        assert_eq!(
            muse_title_status(Some("\u{280b} repo"), Some("repo")),
            Some(Working)
        );
        assert_eq!(
            muse_title_status(Some("\u{280b} other"), Some("repo")),
            None
        );
        assert_eq!(
            muse_title_status(Some("C:/bin/muse.exe"), Some("repo")),
            None
        );
        assert_eq!(muse_title_status(Some("repo"), None), None);
        let trace = title_status_trace(
            TitleProvider::Muse,
            Some("repo"),
            &[
                Some("\u{280b} repo".into()),
                Some("elsewhere".into()),
                Some("repo".into()),
            ],
        );
        assert_eq!(trace, vec![Working, Idle]);
    }

    #[test]
    fn muse_hooks_mark_permission_and_completion_while_activity_tells_hidden_work() {
        use AgentStatus::{Blocked, Done, Working};
        use TerminalStatusReason::{ProviderHook, RecentOutput};
        use muxel_core::muse::MuseState;
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState {
            muse_cwd_label: Some("repo".into()),
            ..RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible)
        };
        let observe = |runtime: &mut RuntimeStatusState, hook| {
            runtime.muse_hook_state = Some(hook);
            runtime.reconcile(
                &session,
                TitleProvider::Muse,
                &[],
                &[],
                false,
                Instant::now(),
            );
            (runtime.status, runtime.reason)
        };
        // No title reaches a tmux or remote pane's view. Output activity tells
        // work, since an interrupted turn fires no hook; hooks report blocked
        // and finished turns.
        assert_eq!(
            observe(&mut runtime, MuseState::Working),
            (Working, RecentOutput)
        );
        assert_eq!(
            observe(&mut runtime, MuseState::Blocked),
            (Blocked, ProviderHook)
        );
        assert_eq!(observe(&mut runtime, MuseState::Done), (Done, ProviderHook));
        // A spinning title is new work after a finished turn, but Muse keeps
        // spinning through a permission prompt.
        session.process_drained(&drained(&[("\x1b]0;\u{280b} repo\x07", SystemTime::now())]));
        assert_eq!(observe(&mut runtime, MuseState::Done).0, Working);
        assert_eq!(
            observe(&mut runtime, MuseState::Blocked),
            (Blocked, ProviderHook)
        );
        // Once titles arrive they, not output, tell work.
        session.process_drained(&drained(&[("\x1b]0;repo\x07", SystemTime::now())]));
        assert_eq!(observe(&mut runtime, MuseState::Done), (Done, ProviderHook));
        let (_, reason) = observe(&mut runtime, MuseState::Working);
        assert!(!matches!(reason, ProviderHook | RecentOutput), "{reason:?}");
        // A process exit outranks every hook.
        runtime.reconcile(
            &session,
            TitleProvider::Muse,
            &[],
            &[],
            true,
            Instant::now(),
        );
        assert_eq!(runtime.reason, TerminalStatusReason::ProcessExit);
        session.kill();
    }

    #[test]
    fn program_status_overrides_muse_hooks_until_clear() {
        use muxel_core::muse::MuseState;
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        runtime.muse_hook_state = Some(MuseState::Blocked);
        session.test_program_status(b"\x1b]7501;state=working\x07");
        runtime.reconcile(
            &session,
            TitleProvider::Muse,
            &[],
            &[],
            false,
            Instant::now(),
        );
        assert_eq!(runtime.status, AgentStatus::Working);
        assert_eq!(runtime.reason, TerminalStatusReason::ProgramStatus(None));
        session.test_program_status(b"\x1b]7501;state=clear\x07");
        runtime.reconcile(
            &session,
            TitleProvider::Muse,
            &[],
            &[],
            false,
            Instant::now(),
        );
        assert_eq!(runtime.status, AgentStatus::Blocked);
        assert_eq!(runtime.reason, TerminalStatusReason::ProviderHook);
        session.kill();
    }

    #[test]
    fn a_current_muse_hook_survives_silent_title_retirement() {
        use muxel_core::muse::MuseState;
        let (session, _receiver) = fixture();
        let mut viewer = Viewer::new(TitleProvider::Muse);
        viewer.runtime.muse_cwd_label = Some("repo".into());
        session.process_output("\x1b]0;\u{280b} repo\x07".as_bytes());
        assert_eq!(viewer.sample(&session).0, AgentStatus::Working);
        let hook = SystemTime::now();
        session.provider_switched_at(hook, hook, hook);
        let switched_at = viewer.clock;
        viewer.sample_at(&session, switched_at);
        viewer.runtime.muse_hook_state = Some(MuseState::Blocked);
        session.set_idle_for_test(Duration::from_secs(6));
        let (snapshot, _) = viewer.sample_at(&session, switched_at + Duration::from_secs(6));
        assert_eq!(snapshot.status, AgentStatus::Blocked);
        assert_eq!(snapshot.reason, TerminalStatusReason::ProviderHook);
        assert!(snapshot.attributable);
        session.kill();
    }

    #[test]
    fn split_old_osc_title_cannot_rebind_on_a_fresh_terminator() {
        let (session, _receiver) = fixture();
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let c = uuid::Uuid::new_v4();
        let old = SystemTime::now() - Duration::from_secs(3);
        let hook = old + Duration::from_secs(1);
        let fresh = hook + Duration::from_secs(4);
        session.process_drained(&drained(&[(
            &format!("\x1b]0;{a} | Ready\x07"),
            old - Duration::from_secs(1),
        )]));
        session.process_drained(&drained(&[(&format!("\x1b]0;{b} | Ready"), old)]));
        session.provider_switched_at(hook, hook, SystemTime::now());
        let switched = session.binding_revision();
        session.process_drained(&drained(&[("\x07", fresh)]));
        assert_eq!(session.binding_revision(), switched);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&a.to_string())
        );

        // A title whose opening bytes were read after the hook still binds.
        session.process_drained(&drained(&[(
            &format!("\x1b]0;{c} | Ready\x07"),
            fresh + Duration::from_millis(1),
        )]));
        assert!(session.binding_revision() > switched);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&c.to_string())
        );
        session.kill();
    }

    #[test]
    fn synchronized_old_title_keeps_its_opening_read_time() {
        let (session, _receiver) = fixture();
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let old = SystemTime::now() - Duration::from_secs(3);
        let hook = old + Duration::from_secs(1);
        let fresh = hook + Duration::from_secs(4);
        session.process_drained(&drained(&[(
            &format!("\x1b]0;{a} | Working\x07"),
            old - Duration::from_secs(1),
        )]));
        session.process_drained(&drained(&[(
            &format!("\x1b[?2026h\x1b]0;{b} | Ready\x07"),
            old,
        )]));
        session.provider_switched_at(hook, hook, SystemTime::now());
        let switched = session.binding_revision();
        session.process_drained(&drained(&[("\x1b[?2026l", fresh)]));
        assert_eq!(session.binding_revision(), switched);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&a.to_string())
        );
        session.kill();
    }

    #[test]
    fn escape_controls_keep_an_old_title_old_but_a_fresh_bell_fresh() {
        let (session, _receiver) = fixture();
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let old = SystemTime::now() - Duration::from_secs(3);
        let hook = old + Duration::from_secs(1);
        let fresh = hook + Duration::from_secs(4);
        session.process_drained(&drained(&[(
            &format!("\x1b]0;{a} | Working\x07"),
            old - Duration::from_secs(1),
        )]));
        session.process_drained(&drained(&[("\x1b", old)]));
        session.provider_switched_at(hook, hook, SystemTime::now());
        let switched = session.binding_revision();
        session.process_drained(&drained(&[(&format!("\x00\x07]0;{b} | Ready\x07"), fresh)]));
        assert!(session.has_bell());
        assert_eq!(session.binding_revision(), switched);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&a.to_string())
        );
        session.kill();
    }

    #[test]
    fn a_switch_with_no_fresh_evidence_settles_at_its_deadline_without_completing() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let clock = Instant::now();
        let hook = SystemTime::now() - Duration::from_secs(1);
        let learned = SystemTime::now() + Duration::from_secs(1);
        session.mark_turn_submitted();
        session.provider_switched_at(hook, hook, learned);
        assert_eq!(session.first_submission_binding(), None);
        session.process_drained(&drained(&[
            (
                "\x1b]0;\u{25d0} Task\x07",
                learned - Duration::from_millis(20),
            ),
            (
                "\x1b]0;\u{2733} Task\x07",
                learned - Duration::from_millis(10),
            ),
        ]));
        // No more output arrives, as from an idle Claude after `/clear`.
        let (_, deadline) =
            runtime.reconcile(&session, TitleProvider::Claude, &[], &[], false, clock);
        let snapshot = runtime.snapshot(&session);
        assert!(
            !snapshot.attributable,
            "the final title was read in the window"
        );
        assert!(
            snapshot.completion_ready,
            "input after the hook still arms a turn"
        );
        // The reducer wakes itself when the window has been closed for
        // SWITCH_SETTLE; it doesn't wait for output that may never come.
        let deadline = deadline.expect("a settle deadline");
        let settles = learned + SWITCH_SETTLE;
        let due = Instant::now() + settles.duration_since(SystemTime::now()).unwrap();
        assert!(
            deadline + Duration::from_millis(100) >= due
                && deadline <= due + Duration::from_millis(100)
        );
        let mut earlier = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        earlier.reconcile(
            &session,
            TitleProvider::Claude,
            &[],
            &[],
            false,
            deadline - Duration::from_millis(500),
        );
        assert!(!earlier.snapshot(&session).attributable);
        for elapsed in [Duration::ZERO, Duration::from_secs(30 * 60)] {
            runtime.reconcile(
                &session,
                TitleProvider::Claude,
                &[],
                &[],
                false,
                deadline + elapsed,
            );
            let snapshot = runtime.snapshot(&session);
            assert!(snapshot.attributable, "ctl may send to an idle pane again");
            assert_eq!(snapshot.status, AgentStatus::Idle);
            assert!(snapshot.completion_ready);
            // The turn finished inside the window: settling forms no edge.
            assert_ne!(snapshot.status, AgentStatus::Done);
        }
        // A later turn in the new conversation completes as usual.
        let later = settles + Duration::from_secs(1);
        session.mark_turn_submitted();
        session.process_drained(&drained(&[("\x1b]0;\u{25d0} Next\x07", later)]));
        runtime.reconcile(
            &session,
            TitleProvider::Claude,
            &[],
            &[],
            false,
            deadline + Duration::from_secs(2),
        );
        assert_eq!(runtime.snapshot(&session).status, AgentStatus::Working);
        session.process_drained(&drained(&[(
            "\x1b]0;\u{2733} Next\x07",
            later + Duration::from_millis(10),
        )]));
        runtime.reconcile(
            &session,
            TitleProvider::Claude,
            &[],
            &[],
            false,
            deadline + Duration::from_secs(3),
        );
        let done = runtime.snapshot(&session);
        assert_eq!(done.status, AgentStatus::Done);
        assert!(done.attributable);
        session.kill();
    }

    #[test]
    fn a_silent_switch_retires_an_old_working_title_without_completing() {
        let (session, _receiver) = fixture();
        let mut viewer = Viewer::new(TitleProvider::Claude);
        session.mark_turn_submitted();
        session.process_output("\x1b]0;\u{25d0} Old task\x07".as_bytes());
        assert_eq!(viewer.sample(&session).0, AgentStatus::Working);

        let hook = SystemTime::now();
        session.provider_switched_at(hook, hook, hook);
        let switched_at = viewer.clock;
        let (snapshot, deadline) = viewer.sample_at(&session, switched_at);
        assert_eq!(snapshot.status, AgentStatus::Working);
        assert!(!snapshot.attributable);
        assert!(!snapshot.completion_ready);
        assert!(deadline.is_some(), "the silent switch schedules a check");

        // The two-second switch allowance plus two-second evidence settle
        // ends before the five-second quiet retirement. Sample after both;
        // ending that window must not discard the retirement timer.
        session.set_idle_for_test(Duration::from_secs(6));
        let (snapshot, deadline) =
            viewer.sample_at(&session, switched_at + Duration::from_millis(4500));
        assert_eq!(snapshot.status, AgentStatus::Working);
        assert_eq!(deadline, Some(switched_at + Duration::from_secs(5)));
        session.set_idle_for_test(Duration::from_secs(1));
        let (snapshot, deadline) = viewer.sample_at(&session, switched_at + Duration::from_secs(6));
        assert_eq!(
            snapshot.status,
            AgentStatus::Working,
            "PTY output delays retirement"
        );
        assert!(deadline.is_some_and(|at| at > switched_at + Duration::from_secs(9)));
        session.set_idle_for_test(Duration::from_secs(6));
        for elapsed in [11, 60] {
            let (snapshot, _) =
                viewer.sample_at(&session, switched_at + Duration::from_secs(elapsed));
            assert_eq!(snapshot.status, AgentStatus::Idle);
            assert_eq!(snapshot.reason, TerminalStatusReason::Quiet);
            assert!(snapshot.attributable);
            assert!(!snapshot.completion_ready);
            assert!(!viewer.runtime.done_latch);
            assert!(!viewer.runtime.completion_armed);
            assert_eq!(viewer.runtime.prev_raw, None);
        }
        session.kill();
    }

    #[test]
    fn a_silent_switch_keeps_new_submitted_or_fresh_titled_work() {
        for submitted in [false, true] {
            let (session, _receiver) = fixture();
            let mut viewer = Viewer::new(TitleProvider::Claude);
            session.process_output("\x1b]0;\u{25d0} Old task\x07".as_bytes());
            viewer.sample(&session);
            let hook = SystemTime::now();
            session.provider_switched_at(hook, hook, hook);
            let switched_at = viewer.clock;
            viewer.sample_at(&session, switched_at);
            if submitted {
                session.mark_turn_submitted();
            } else {
                session.process_drained(&drained(&[(
                    "\x1b]0;\u{25d0} New task\x07",
                    hook + SWITCH_SLACK + Duration::from_millis(100),
                )]));
            }
            session.set_idle_for_test(Duration::from_secs(6));
            let (snapshot, _) = viewer.sample_at(&session, switched_at + Duration::from_secs(6));
            assert_eq!(
                snapshot.status,
                AgentStatus::Working,
                "submitted={submitted}"
            );
            assert!(snapshot.attributable, "submitted={submitted}");
            assert!(!viewer.runtime.done_latch);
            assert!(viewer.runtime.silent_switch_until.is_none());
            session.kill();
        }
    }

    #[test]
    fn silent_switch_settle_does_not_retire_a_fresh_program_report() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let clock = Instant::now();
        session.process_probe_output("\x1b]0;◐ Old task\x07".as_bytes());
        runtime.reconcile(&session, TitleProvider::Claude, &[], &[], false, clock);
        let hook = SystemTime::now();
        session.provider_switched_at(hook, hook, hook);
        session.process_drained(&drained(&[(
            "\x1b]7501;state=blocked:kind=permission\x07",
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        runtime.reconcile(
            &session,
            TitleProvider::Claude,
            &[],
            &[],
            false,
            clock + Duration::from_millis(1),
        );
        let current = runtime.snapshot(&session);
        assert_eq!(current.status, AgentStatus::Blocked);
        assert!(current.attributable);
        assert_eq!(current.coverage, TerminalStatusCoverage::ProgramStatus);
        session.set_idle_for_test(Duration::from_secs(6));
        runtime.reconcile(
            &session,
            TitleProvider::Claude,
            &[],
            &[],
            false,
            clock + Duration::from_secs(6),
        );
        let settled = runtime.snapshot(&session);
        assert_eq!(settled.status, AgentStatus::Blocked);
        assert_eq!(
            settled.reason,
            TerminalStatusReason::ProgramStatus(Some(ProgramStatusKind::Permission))
        );
        assert!(settled.attributable);
        session.kill();
    }

    #[test]
    fn stale_uuid_title_cannot_advance_an_out_of_band_binding() {
        let (session, _receiver) = fixture();
        let first = uuid::Uuid::new_v4();
        let stale = uuid::Uuid::new_v4();
        let new = uuid::Uuid::new_v4();
        session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
        assert_eq!(session.binding_revision(), 1);
        let hook = SystemTime::now();
        session.provider_switched_at(hook, hook, hook);
        let switched = session.binding_revision();
        assert_eq!(switched, 2);
        session.process_drained(&drained(&[(
            &format!("\x1b]0;{stale} | Working\x07"),
            hook - Duration::from_millis(500),
        )]));
        assert_eq!(session.binding_revision(), switched);
        assert!(session.runtime_evidence(None).settling);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&first.to_string())
        );

        session.process_drained(&drained(&[(
            &format!("\x1b]0;{new} | Ready\x07"),
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        assert_eq!(session.binding_revision(), switched + 1);
        session.kill();
    }

    #[test]
    fn synchronized_stale_uuid_title_keeps_its_original_read_time() {
        let (session, _receiver) = fixture();
        let first = uuid::Uuid::new_v4();
        let stale = uuid::Uuid::new_v4();
        session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
        let hook = SystemTime::now();
        session.process_drained(&drained(&[(
            &format!("\x1b[?2026h\x1b]0;{stale} | Working\x07"),
            hook - Duration::from_millis(500),
        )]));
        assert_eq!(session.binding_revision(), 1);
        session.provider_switched_at(hook, hook, hook);
        let switched = session.binding_revision();
        session.process_drained(&drained(&[(
            "\x1b[?2026l",
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        assert_eq!(session.binding_revision(), switched);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&first.to_string())
        );
        session.kill();
    }

    #[test]
    fn expired_synchronized_title_cannot_rebind_after_a_switch() {
        let (session, _receiver) = fixture();
        let first = uuid::Uuid::new_v4();
        let stale = uuid::Uuid::new_v4();
        session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
        let hook = SystemTime::now();
        session.process_drained(&drained(&[(
            &format!("\x1b[?2026h\x1b]0;{stale} | Working\x07"),
            hook - Duration::from_millis(500),
        )]));
        session.provider_switched_at(hook, hook, hook);
        let switched = session.binding_revision();
        std::thread::sleep(Duration::from_millis(250));
        session.process_drained(&PtyOutput::default());
        assert_eq!(session.binding_revision(), switched);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&first.to_string())
        );
        session.kill();
    }

    #[test]
    fn runtime_deadline_flush_keeps_a_stale_title_in_the_old_binding() {
        let (session, _receiver) = fixture();
        let first = uuid::Uuid::new_v4();
        let stale = uuid::Uuid::new_v4();
        session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
        let hook = SystemTime::now();
        let old_read = hook - Duration::from_millis(500);
        session.process_drained(&drained(&[(
            &format!("\x1b[?2026h\x1b]0;{stale} | Working\x07"),
            old_read,
        )]));
        session.provider_switched_at(hook, hook, hook);
        let switched = session.binding_revision();
        let deadline = session.program_status_sync_deadline().unwrap();
        std::thread::sleep(
            deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(10),
        );
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            Instant::now(),
        );
        assert_eq!(session.binding_revision(), switched);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&first.to_string())
        );
        session.process_drained(&drained(&[("\x1b]7501;state=done\x07", old_read)]));
        assert_eq!(session.program_status_snapshot().root, None);
        assert!(session.runtime_evidence(None).settling);
        session.kill();
    }

    #[test]
    fn split_osc_title_keeps_the_read_time_of_its_body() {
        for split in [18, 44] {
            let (session, _receiver) = fixture();
            let first = uuid::Uuid::new_v4();
            let stale = uuid::Uuid::new_v4();
            session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
            let hook = SystemTime::now();
            let title = format!("\x1b]0;{stale} | Working\x07");
            session.process_drained(&drained(&[(
                &title[..split],
                hook - Duration::from_millis(500),
            )]));
            session.provider_switched_at(hook, hook, hook);
            let switched = session.binding_revision();
            session.process_drained(&drained(&[(
                &title[split..],
                hook + SWITCH_SLACK + Duration::from_millis(100),
            )]));
            assert_eq!(
                session.binding_revision(),
                switched,
                "split at byte {split}"
            );
            assert!(
                session
                    .session_id_hint()
                    .unwrap()
                    .starts_with(&first.to_string())
            );
            session.kill();
        }
    }

    #[test]
    fn split_osc_st_terminator_keeps_the_read_time_of_its_body() {
        let (session, _receiver) = fixture();
        let first = uuid::Uuid::new_v4();
        let stale = uuid::Uuid::new_v4();
        session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
        let hook = SystemTime::now();
        session.process_drained(&drained(&[(
            &format!("\x1b]0;{stale} | Working"),
            hook - Duration::from_millis(500),
        )]));
        session.provider_switched_at(hook, hook, hook);
        let switched = session.binding_revision();
        session.process_drained(&drained(&[(
            "\x1b",
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        session.process_drained(&drained(&[(
            "\\",
            hook + SWITCH_SLACK + Duration::from_millis(120),
        )]));
        assert_eq!(session.binding_revision(), switched);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&first.to_string())
        );
        session.kill();
    }

    #[test]
    fn split_osc_opening_escape_keeps_its_first_read_time() {
        let (session, _receiver) = fixture();
        let first = uuid::Uuid::new_v4();
        let stale = uuid::Uuid::new_v4();
        session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
        let hook = SystemTime::now();
        session.process_drained(&drained(&[("\x1b", hook - Duration::from_millis(500))]));
        session.provider_switched_at(hook, hook, hook);
        let switched = session.binding_revision();
        session.process_drained(&drained(&[(
            &format!("]0;{stale} | Working\x07"),
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        assert_eq!(session.binding_revision(), switched);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&first.to_string())
        );
        session.kill();
    }

    #[test]
    fn c0_and_del_between_escape_and_osc_keep_the_opening_read_time() {
        for control in ["\x00", "\x1c", "\x7f", "\x1b"] {
            let (session, _receiver) = fixture();
            let first = uuid::Uuid::new_v4();
            let stale = uuid::Uuid::new_v4();
            session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
            let hook = SystemTime::now();
            session.process_drained(&drained(&[(
                &format!("\x1b{control}]0;{stale} | Ready"),
                hook - Duration::from_millis(500),
            )]));
            session.provider_switched_at(hook, hook, hook);
            let switched = session.binding_revision();
            session.process_drained(&drained(&[(
                "\x07",
                hook + SWITCH_SLACK + Duration::from_millis(100),
            )]));
            assert_eq!(session.binding_revision(), switched, "control {control:?}");
            assert!(
                session
                    .session_id_hint()
                    .unwrap()
                    .starts_with(&first.to_string()),
                "control {control:?}"
            );
            session.process_drained(&drained(&[(
                "\x1b]7501;state=done\x07",
                hook - Duration::from_millis(400),
            )]));
            assert_eq!(
                session.program_status_snapshot().root,
                None,
                "control {control:?}"
            );
            session.kill();
        }
    }

    #[test]
    fn a_fresh_bell_inside_an_old_escape_keeps_its_own_read_time() {
        let (session, _receiver) = fixture();
        let first = uuid::Uuid::new_v4();
        let stale = uuid::Uuid::new_v4();
        session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
        let hook = SystemTime::now();
        session.process_drained(&drained(&[("\x1b", hook - Duration::from_millis(500))]));
        session.provider_switched_at(hook, hook, hook);
        let switched = session.binding_revision();
        session.process_drained(&drained(&[(
            &format!("\x00\x07]0;{stale} | Ready\x07"),
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        assert!(
            session.has_bell(),
            "the executed BEL was read after the switch"
        );
        assert_eq!(session.binding_revision(), switched);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&first.to_string())
        );
        session.kill();
    }

    #[test]
    fn fresh_bell_does_not_refresh_a_program_report_opened_by_an_old_escape() {
        let (session, _receiver) = fixture();
        let hook = SystemTime::now();
        session.process_drained(&drained(&[("\x1b", hook - Duration::from_millis(500))]));
        session.provider_switched_at(hook, hook, hook);
        session.process_drained(&drained(&[(
            "\x00\x07]7501;state=done\x07",
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        assert!(session.has_bell());
        assert_eq!(session.program_status_snapshot().root, None);
        session.kill();
    }

    #[test]
    fn a_fresh_title_after_a_stale_terminator_in_one_read_can_bind() {
        let (session, _receiver) = fixture();
        let first = uuid::Uuid::new_v4();
        let stale = uuid::Uuid::new_v4();
        let fresh = uuid::Uuid::new_v4();
        session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
        let hook = SystemTime::now();
        session.process_drained(&drained(&[(
            &format!("\x1b]0;{stale} | Working"),
            hook - Duration::from_millis(500),
        )]));
        session.provider_switched_at(hook, hook, hook);
        let switched = session.binding_revision();
        session.process_drained(&drained(&[(
            &format!("\x07\x1b]0;{fresh} | Ready\x07"),
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        assert_eq!(session.binding_revision(), switched + 1);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&fresh.to_string())
        );
        session.kill();
    }

    #[test]
    fn consecutive_sync_frames_use_each_frames_read_time() {
        let (session, _receiver) = fixture();
        let first = uuid::Uuid::new_v4();
        let next = uuid::Uuid::new_v4();
        session.process_probe_output(format!("\x1b]0;{first} | Working\x07").as_bytes());
        let hook = SystemTime::now();
        session.process_drained(&drained(&[(
            &format!("\x1b[?2026h\x1b]0;{first} | Working\x07"),
            hook - Duration::from_millis(500),
        )]));
        session.provider_switched_at(hook, hook, hook);
        let switched = session.binding_revision();
        let fresh = hook + SWITCH_SLACK + Duration::from_millis(100);
        session.process_drained(&drained(&[(
            &format!("\x1b[?2026l\x1b[?2026h\x1b]0;{next} | Ready\x07"),
            fresh,
        )]));
        session.process_drained(&drained(&[(
            "\x1b[?2026l",
            fresh + Duration::from_millis(20),
        )]));
        assert_eq!(session.binding_revision(), switched + 1);
        assert!(
            session
                .session_id_hint()
                .unwrap()
                .starts_with(&next.to_string())
        );
        session.kill();
    }

    #[test]
    fn replacement_before_first_uuid_cannot_complete_the_unidentified_submission() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let clock = Instant::now();
        let read_at = SystemTime::now();
        let first = uuid::Uuid::new_v4();
        session.mark_turn_submitted();
        session.process_drained(&drained(&[(
            &format!("\x1b]0;{first} | Working\x07"),
            read_at,
        )]));
        runtime.reconcile(&session, TitleProvider::Codex, &[], &[], false, clock);
        let working = runtime.snapshot(&session);
        assert_eq!(working.status, AgentStatus::Working);
        assert!(working.attributable);
        assert_eq!(session.first_submission_binding(), None);
        session.process_drained(&drained(&[(
            &format!("\x1b]0;{first} | Ready\x07"),
            read_at + Duration::from_millis(10),
        )]));
        runtime.reconcile(
            &session,
            TitleProvider::Codex,
            &[],
            &[],
            false,
            clock + Duration::from_millis(10),
        );
        let done = runtime.snapshot(&session);
        assert_eq!(done.status, AgentStatus::Idle);
        assert!(done.attributable);
        assert!(done.completion_ready);
        assert_eq!(session.first_submission_binding(), None);
        session.kill();
    }

    #[test]
    fn production_reducer_does_not_complete_when_a_held_working_marker_clears() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::VisibleMarkers);
        let markers = vec!["WORKING_MARKER".to_string()];
        let hook = SystemTime::now() - Duration::from_secs(10);
        let clock = Instant::now();
        session.process_drained(&drained(&[(
            "WORKING_MARKER",
            hook - Duration::from_secs(3),
        )]));
        runtime.reconcile(&session, TitleProvider::Other, &markers, &[], false, clock);
        assert_eq!(runtime.status, AgentStatus::Working);

        session.provider_switched_at(hook, hook, hook + Duration::from_millis(400));
        runtime.reconcile(
            &session,
            TitleProvider::Other,
            &markers,
            &[],
            false,
            clock + Duration::from_millis(60),
        );
        session.mark_turn_submitted();
        let fresh = hook + SWITCH_SLACK + Duration::from_millis(1);
        session.process_drained(&drained(&[("\r\nfresh bytes", fresh)]));
        runtime.reconcile(
            &session,
            TitleProvider::Other,
            &markers,
            &[],
            false,
            clock + Duration::from_millis(120),
        );
        assert!(!session.runtime_evidence(None).settling);
        assert!(runtime.screen_held);
        assert!(!runtime.attributable);
        assert_eq!(runtime.prev_raw, None);
        assert!(
            runtime.completion_armed,
            "the new submission survives the hold"
        );

        session.process_drained(&drained(&[(
            "\x1b[2J\x1b[H",
            fresh + Duration::from_millis(1),
        )]));
        runtime.reconcile(
            &session,
            TitleProvider::Other,
            &markers,
            &[],
            false,
            clock + Duration::from_millis(180),
        );
        assert!(!runtime.screen_held);
        assert!(runtime.attributable);
        assert_eq!(
            runtime.status,
            AgentStatus::Idle,
            "retiring old Working cannot complete a new turn"
        );
        assert!(!runtime.done_latch);
        assert!(runtime.completion_armed);

        // A later attributable Working really can complete the submitted turn.
        session.process_drained(&drained(&[(
            "WORKING_MARKER",
            fresh + Duration::from_millis(2),
        )]));
        runtime.reconcile(
            &session,
            TitleProvider::Other,
            &markers,
            &[],
            false,
            clock + Duration::from_millis(240),
        );
        assert_eq!(runtime.prev_raw, Some(AgentStatus::Working));
        session.process_drained(&drained(&[(
            "\x1b[2J\x1b[H",
            fresh + Duration::from_millis(3),
        )]));
        runtime.reconcile(
            &session,
            TitleProvider::Other,
            &markers,
            &[],
            false,
            clock + Duration::from_millis(300),
        );
        assert_eq!(runtime.status, AgentStatus::Done);
        assert!(runtime.attributable);
    }

    #[test]
    fn production_snapshot_rejects_a_switch_before_the_queued_reconcile() {
        let (session, _receiver) = fixture();
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let now = SystemTime::now();
        session.mark_turn_submitted();
        session.process_drained(&drained(&[(
            "\x1b]0;\u{280b} Old\x07\x1b]0;\u{2733} Old\x07",
            now,
        )]));
        runtime.reconcile(
            &session,
            TitleProvider::Claude,
            &[],
            &[],
            false,
            Instant::now(),
        );
        let cached = runtime.snapshot(&session);
        assert_eq!(cached.status, AgentStatus::Done);
        assert!(cached.attributable);

        session.provider_switched(now, now);
        // Do not receive the runtime event or reconcile: this is the adoption tick.
        assert!(!cached.is_attributable_to(session.binding_revision()));
        let current_read = runtime.snapshot(&session);
        assert_eq!(current_read.status, AgentStatus::Done);
        assert!(!current_read.attributable);
        assert!(!current_read.completion_ready);
    }

    #[test]
    fn grok_working_hold_uses_only_attributable_rows() {
        let (session, _receiver) = fixture();
        let mut viewer = Viewer::new(TitleProvider::Grok);
        let clock = viewer.clock;
        let hook = SystemTime::now() - Duration::from_secs(10);
        let row = "◎ 1 command still running · send a message to interrupt";
        session.process_drained(&drained(&[(row, hook - Duration::from_secs(3))]));
        session.provider_switched_at(hook, hook, hook + Duration::from_millis(400));
        let (snapshot, _) = viewer.sample_at(&session, clock);
        assert_eq!(snapshot.status, AgentStatus::Working);
        assert!(!snapshot.attributable);
        assert_eq!(viewer.runtime.grok_screen_working_at, None);

        session.mark_turn_submitted();
        let fresh = hook + SWITCH_SLACK + Duration::from_millis(100);
        session.process_drained(&drained(&[("\x1b]0;New - grok\x07", fresh)]));
        let (snapshot, _) = viewer.sample_at(&session, clock + Duration::from_millis(60));
        assert!(!session.runtime_evidence(None).settling);
        assert_eq!(snapshot.status, AgentStatus::Working);
        assert!(
            !snapshot.attributable,
            "the fresh Idle title cannot adopt the old row"
        );
        assert_eq!(viewer.runtime.grok_screen_working_at, None);

        session.process_drained(&drained(&[(
            "\x1b[2J\x1b[H",
            fresh + Duration::from_millis(1),
        )]));
        // Removal arrives before the next permitted capture. Reconcile at its
        // returned deadline, just as the view's runtime timer does.
        let (snapshot, deadline) = viewer.sample_at(&session, clock + Duration::from_millis(80));
        assert!(!snapshot.attributable);
        let deadline = deadline.expect("trailing screen capture");
        assert_eq!(deadline, clock + Duration::from_millis(110));
        let (snapshot, _) = viewer.sample_at(&session, deadline);
        assert_eq!(snapshot.status, AgentStatus::Idle);
        assert!(snapshot.attributable);
        assert!(!viewer.runtime.done_latch);
        assert!(
            viewer.runtime.completion_armed,
            "the explicit submission survives"
        );
        let (snapshot, _) = viewer.sample_at(&session, clock + Duration::from_secs(3));
        assert_eq!(
            snapshot.status,
            AgentStatus::Idle,
            "no old hold expires into Done"
        );

        // A newly drawn row is attributable and retains the original two-second
        // repaint hold; its eventual absence may complete this submitted turn.
        session.process_drained(&drained(&[(row, fresh + Duration::from_millis(2))]));
        let seen_at = clock + Duration::from_secs(4);
        let (snapshot, _) = viewer.sample_at(&session, seen_at);
        assert_eq!(snapshot.status, AgentStatus::Working);
        assert!(snapshot.attributable);
        assert_eq!(viewer.runtime.grok_screen_working_at, Some(seen_at));
        session.process_drained(&drained(&[(
            "\x1b[2J\x1b[H",
            fresh + Duration::from_millis(3),
        )]));
        let (snapshot, deadline) = viewer.sample_at(&session, seen_at + Duration::from_millis(60));
        assert_eq!(snapshot.status, AgentStatus::Working);
        assert!(snapshot.attributable);
        let deadline = deadline.expect("attributable Working hold");
        assert_eq!(
            deadline,
            seen_at + super::GROK_SCREEN_WORKING_HOLD + Duration::from_millis(1)
        );
        let (snapshot, _) = viewer.sample_at(&session, seen_at + super::GROK_SCREEN_WORKING_HOLD);
        assert_eq!(
            snapshot.status,
            AgentStatus::Working,
            "the inclusive hold boundary remains"
        );
        let (snapshot, _) = viewer.sample_at(&session, deadline);
        assert_eq!(snapshot.status, AgentStatus::Done);
        assert!(snapshot.attributable);
    }

    #[test]
    fn grok_blocked_hold_uses_only_attributable_titles() {
        let (session, _receiver) = fixture();
        let mut viewer = Viewer::new(TitleProvider::Grok);
        let clock = viewer.clock;
        // Recent enough that the window's settle deadline is still ahead.
        let hook = SystemTime::now() - Duration::from_secs(1);
        let blocked = "\x1b]0;\u{26a0} Action Required - \u{280b} - Preparing question\u{2026} - Topic - grok\x07";
        let working = "\x1b]0;\u{2839} - Responding - Topic - grok\x07";
        session.process_drained(&drained(&[(blocked, hook + Duration::from_millis(100))]));
        session.provider_switched_at(hook, hook, hook + Duration::from_millis(400));
        let (snapshot, _) = viewer.sample_at(&session, clock);
        assert_eq!(snapshot.status, AgentStatus::Blocked);
        assert!(!snapshot.attributable);
        assert_eq!(viewer.runtime.grok_blocked_at, None);

        let fresh = hook + SWITCH_SLACK + Duration::from_millis(100);
        session.process_drained(&drained(&[(working, fresh)]));
        let (snapshot, _) = viewer.sample_at(&session, clock + Duration::from_millis(60));
        assert_eq!(
            snapshot.status,
            AgentStatus::Working,
            "old Blocked cannot override fresh Working"
        );
        assert!(snapshot.attributable);
        assert_eq!(viewer.runtime.grok_blocked_at, None);

        // A fresh Blocked observation still survives the documented blink and
        // releases Working at its original 750 ms boundary.
        session.process_drained(&drained(&[(blocked, fresh + Duration::from_millis(1))]));
        let blocked_at = clock + Duration::from_millis(120);
        let (snapshot, _) = viewer.sample_at(&session, blocked_at);
        assert_eq!(snapshot.status, AgentStatus::Blocked);
        assert!(snapshot.attributable);
        assert_eq!(viewer.runtime.grok_blocked_at, Some(blocked_at));
        session.process_drained(&drained(&[(working, fresh + Duration::from_millis(2))]));
        let (snapshot, deadline) =
            viewer.sample_at(&session, blocked_at + Duration::from_millis(60));
        assert_eq!(snapshot.status, AgentStatus::Blocked);
        assert!(snapshot.attributable);
        let deadline = deadline.expect("attributable Blocked hold");
        assert_eq!(deadline, blocked_at + Duration::from_millis(751));
        let (snapshot, _) = viewer.sample_at(&session, blocked_at + Duration::from_millis(750));
        assert_eq!(
            snapshot.status,
            AgentStatus::Blocked,
            "the inclusive hold boundary remains"
        );
        let (snapshot, _) = viewer.sample_at(&session, deadline);
        assert_eq!(snapshot.status, AgentStatus::Working);
        assert!(snapshot.attributable);
        assert_eq!(viewer.runtime.grok_blocked_at, None);
    }

    #[test]
    fn uncertain_titles_do_not_seed_sticky_status_or_readiness() {
        for (old_title, old_status) in [
            ("\x1b]0;\u{280b} Old\x07", AgentStatus::Working),
            ("\x1b]0;\u{2733} Old\x07", AgentStatus::Idle),
        ] {
            let (session, _receiver) = fixture();
            let mut viewer = Viewer::new(TitleProvider::Claude);
            // Recent enough that the window's settle deadline is still ahead.
            let hook = SystemTime::now() - Duration::from_secs(1);
            session.process_drained(&drained(&[(old_title, hook + Duration::from_millis(100))]));
            session.provider_switched_at(hook, hook, hook + Duration::from_millis(400));
            assert_eq!(viewer.sample(&session), (old_status, false));
            assert_eq!(viewer.runtime.semantic_title_status, None);
            assert!(!viewer.runtime.provider_settled);
            assert!(!viewer.runtime.completion_ready);

            let fresh = hook + SWITCH_SLACK + Duration::from_millis(100);
            session.process_drained(&drained(&[("\x1b]0;shell banner\x07", fresh)]));
            assert_eq!(viewer.sample(&session), (AgentStatus::Idle, true));
            assert_eq!(viewer.runtime.semantic_title_status, None);
            assert!(!viewer.runtime.provider_settled);
            session.mark_turn_submitted();
            session.process_drained(&drained(&[(
                "\x1b]0;\u{2733} New\x07",
                fresh + Duration::from_millis(1),
            )]));
            assert_eq!(viewer.sample(&session), (AgentStatus::Idle, true));
            assert_eq!(
                viewer.runtime.semantic_title_status,
                Some(AgentStatus::Idle)
            );
            assert!(viewer.runtime.provider_settled);
            // Once observed freshly, semantic state remains sticky through an
            // unrelated title, as it does outside a provider switch.
            session.process_drained(&drained(&[(
                "\x1b]0;shell banner\x07",
                fresh + Duration::from_millis(2),
            )]));
            assert_eq!(viewer.sample(&session), (AgentStatus::Idle, true));
            assert_eq!(
                viewer.runtime.semantic_title_status,
                Some(AgentStatus::Idle)
            );
            session.process_drained(&drained(&[(
                "\x1b]0;\u{280b} New\x07",
                fresh + Duration::from_millis(3),
            )]));
            assert_eq!(viewer.sample(&session), (AgentStatus::Working, true));
            session.process_drained(&drained(&[(
                "\x1b]0;shell banner\x07",
                fresh + Duration::from_millis(4),
            )]));
            assert_eq!(viewer.sample(&session), (AgentStatus::Working, true));
            session.process_drained(&drained(&[(
                "\x1b]0;\u{2733} New\x07",
                fresh + Duration::from_millis(5),
            )]));
            assert_eq!(viewer.sample(&session), (AgentStatus::Done, true));
        }
    }

    #[test]
    fn automated_submissions_complete_once_per_turn_without_marking_human_activity() {
        let (session, _receiver) = fixture();
        let mut viewer = Viewer::new(TitleProvider::Claude);
        let mut last_status = AgentStatus::Idle;
        let mut completions = 0;
        let mut sample = || {
            let (status, _) = viewer.sample(&session);
            if status == AgentStatus::Done && last_status != AgentStatus::Done {
                completions += 1;
            }
            last_status = status;
            (status, completions)
        };

        let working = "\x1b]0;\u{280b} Topic\x07";
        let idle = "\x1b]0;\u{2733} Topic\x07";
        session.process_drained(&drained(&[(working, SystemTime::now())]));
        assert_eq!(sample(), (AgentStatus::Working, 0));

        // Auto types its configurable message, then deliberately submits it.
        // That arms even a pane which has not yet shown a startup Idle.
        session.write_input(b"please continue the remaining work");
        assert!(session.input_snapshot().1);
        assert!(!session.runtime_evidence(None).submitted);
        session.mark_turn_submitted();
        session.write_input(b"\r");
        assert!(!session.input_snapshot().1);
        assert_eq!(session.last_user_input(), None);
        session.process_drained(&drained(&[(idle, SystemTime::now())]));
        assert_eq!(sample(), (AgentStatus::Done, 1));
        assert_eq!(sample(), (AgentStatus::Done, 1));

        // A later automated turn has its own Working and completion. The
        // repeated Idle after it remains the same completion.
        session.write_input(b"continue");
        assert!(session.input_snapshot().1);
        session.mark_turn_submitted();
        session.write_input(b"\r");
        assert!(!session.input_snapshot().1);
        session.process_drained(&drained(&[(working, SystemTime::now())]));
        assert_eq!(sample(), (AgentStatus::Working, 1));
        session.process_drained(&drained(&[(idle, SystemTime::now())]));
        assert_eq!(sample(), (AgentStatus::Done, 2));
        assert_eq!(sample(), (AgentStatus::Done, 2));
        assert_eq!(session.last_user_input(), None);
        session.kill();
    }

    /// Drives the production reducer with observation times far enough apart
    /// to permit the same screen captures as distinct UI drain turns.
    struct Viewer {
        provider: TitleProvider,
        runtime: RuntimeStatusState,
        clock: Instant,
    }

    impl Viewer {
        fn new(provider: TitleProvider) -> Self {
            Self {
                provider,
                runtime: RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible),
                clock: Instant::now(),
            }
        }

        fn sample(&mut self, session: &TerminalSession) -> (AgentStatus, bool) {
            let (snapshot, _) = self.sample_at(session, self.clock);
            (snapshot.status, snapshot.attributable)
        }

        fn sample_at(
            &mut self,
            session: &TerminalSession,
            now: Instant,
        ) -> (super::TerminalRuntimeSnapshot, Option<Instant>) {
            let (_, deadline) =
                self.runtime
                    .reconcile(session, self.provider, &[], &[], false, now);
            self.clock = now + Duration::from_millis(60);
            (self.runtime.snapshot(session), deadline)
        }

        fn completion(
            &mut self,
            session: &TerminalSession,
        ) -> (AgentStatus, Option<AgentStatus>, bool) {
            let (status, _) = self.sample(session);
            (status, self.runtime.prev_raw, self.runtime.completion_armed)
        }
    }

    #[test]
    fn while_a_switch_settles_process_exit_bell_and_program_status_are_attributable() {
        use TerminalStatusReason::*;
        for reason in [
            ProcessExit,
            ProgramStatus(None),
            ProgramStatus(Some(ProgramStatusKind::Question)),
            ProgramStatusError,
            BlockedMarker,
            WorkingMarker,
            ProviderTitle,
            ProviderScreen,
            Bell,
            ProviderHook,
            RecentOutput,
            Quiet,
        ] {
            assert!(status_is_attributable(false, false, reason), "{reason:?}");
            assert_eq!(
                status_is_attributable(true, false, reason),
                matches!(
                    reason,
                    ProcessExit | Bell | ProgramStatus(_) | ProgramStatusError
                ),
                "{reason:?}"
            );
            // A held screen marker stays the old conversation's after the
            // window too.
            assert_eq!(
                status_is_attributable(false, true, reason),
                !matches!(reason, ProviderScreen | BlockedMarker | WorkingMarker),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn an_old_blocked_title_around_a_switch_shows_but_is_not_attributable() {
        let (session, _receiver) = fixture();
        let registered = SystemTime::now() - Duration::from_secs(1);
        // Grok's old conversation asked for approval just before it switched.
        session.process_drained(&drained(&[(
            "\x1b]0;\u{26a0} Action Required - \u{280b} - Preparing question\u{2026} - Old - grok\x07",
            registered - Duration::from_millis(500),
        )]));
        let mut viewer = Viewer::new(TitleProvider::Grok);
        session.provider_switched_at(registered, registered, SystemTime::now());

        assert!(session.runtime_evidence(None).settling);
        assert_eq!(
            viewer.sample(&session),
            (AgentStatus::Blocked, false),
            "shown as level state, but no notification can come of it"
        );

        // The new conversation asks for approval, read after the window.
        session.process_drained(&drained(&[(
            "\x1b]0;\u{26a0} Action Required - \u{280b} - Preparing question\u{2026} - New - grok\x07",
            registered + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        assert!(!session.runtime_evidence(None).settling);
        assert_eq!(
            viewer.sample(&session),
            (AgentStatus::Blocked, true),
            "a Blocked from fresh evidence notifies"
        );
        session.kill();
    }

    #[test]
    fn an_old_blocked_prompt_around_a_switch_shows_but_is_not_attributable() {
        let (session, _receiver) = fixture();
        let hook = SystemTime::now() - Duration::from_secs(1);
        let prompt = "Do you want to proceed?\r\n\u{276f} 1. Yes\r\n  2. Yes, and don't ask again\r\n  3. No\r\nEsc to cancel";
        // The old conversation's permission prompt, drawn just before Claude
        // switched; no title.
        session.process_drained(&drained(&[(prompt, hook - Duration::from_millis(500))]));
        let mut viewer = Viewer::new(TitleProvider::Claude);
        session.provider_switched_at(hook, hook, hook + Duration::from_millis(400));

        assert!(session.runtime_evidence(None).settling);
        assert_eq!(
            viewer.sample(&session),
            (AgentStatus::Blocked, false),
            "the prompt still shows, but no notification can come of it"
        );

        // Harmless output read after the window (the cursor hidden and shown
        // again) ends settling but leaves the old prompt on screen.
        let fresh = hook + SWITCH_SLACK + Duration::from_millis(100);
        session.process_drained(&drained(&[("\x1b[?25l\x1b[?25h", fresh)]));
        assert!(!session.runtime_evidence(None).settling);
        assert_eq!(
            viewer.sample(&session),
            (AgentStatus::Blocked, false),
            "fresh bytes don't make an unchanged prompt the new conversation's"
        );

        // The prompt goes, then the new conversation draws its own.
        session.process_drained(&drained(&[(
            "\x1b[2J\x1b[H",
            fresh + Duration::from_millis(100),
        )]));
        assert_eq!(viewer.sample(&session), (AgentStatus::Idle, true));
        session.process_drained(&drained(&[(prompt, fresh + Duration::from_millis(200))]));
        assert_eq!(
            viewer.sample(&session),
            (AgentStatus::Blocked, true),
            "a prompt drawn after the old one went notifies"
        );
        session.kill();
    }

    #[test]
    fn the_settle_deadline_does_not_release_a_held_screen_marker() {
        let (session, _receiver) = fixture();
        let mut viewer = Viewer::new(TitleProvider::Grok);
        let clock = viewer.clock;
        let hook = SystemTime::now() - Duration::from_secs(1);
        let row = "◎ 1 command still running · send a message to interrupt";
        let working = "\x1b]0;\u{2839} - Responding - Topic - grok\x07";
        // A running row and a Working title, both read inside the window.
        session.process_drained(&drained(&[
            (row, hook + Duration::from_millis(100)),
            (working, hook + Duration::from_millis(110)),
        ]));
        session.provider_switched_at(hook, hook, hook + Duration::from_millis(400));
        let (snapshot, deadline) = viewer.sample_at(&session, clock);
        assert_eq!(snapshot.status, AgentStatus::Working);
        assert!(!snapshot.attributable);
        // Past the settle deadline the title is level state, but it was read
        // inside the window: it can't confirm the old row as the new one's.
        let late = deadline.expect("a settle deadline") + Duration::from_secs(5);
        let (snapshot, _) = viewer.sample_at(&session, late);
        assert!(
            !session
                .runtime_evidence_at(None, wall_time_at(late))
                .settling
        );
        assert_eq!(snapshot.status, AgentStatus::Working);
        assert!(!snapshot.attributable, "the row is still held");
        assert!(viewer.runtime.screen_held);
        // A title read after the window that agrees ends the hold.
        let fresh = hook + SWITCH_SLACK + SWITCH_SETTLE + Duration::from_secs(1);
        session.process_drained(&drained(&[(working, fresh)]));
        let (snapshot, _) = viewer.sample_at(&session, late + Duration::from_millis(60));
        assert!(!viewer.runtime.screen_held);
        assert!(snapshot.attributable);
        session.kill();
    }

    #[test]
    fn a_split_title_opened_inside_the_window_cannot_release_a_screen_hold() {
        let (session, _receiver) = fixture();
        let mut viewer = Viewer::new(TitleProvider::Grok);
        let clock = viewer.clock;
        let hook = SystemTime::now() - Duration::from_secs(1);
        let row = "◎ 1 command still running · send a message to interrupt";
        let working_prefix = "\x1b]0;\u{2839} - Responding - Topic - grok";
        let working = "\x1b]0;\u{2839} - Responding - Topic - grok\x07";
        session.process_drained(&drained(&[
            (row, hook + Duration::from_millis(90)),
            (working_prefix, hook + Duration::from_millis(100)),
        ]));
        session.provider_switched_at(hook, hook, hook + Duration::from_millis(400));
        let (_, deadline) = viewer.sample_at(&session, clock);
        assert!(viewer.runtime.screen_held);

        // VTE dispatches the title on this later BEL, but its read time is
        // still the prefix's time inside the switch window.
        let after_window = hook + SWITCH_SLACK + Duration::from_millis(100);
        session.process_drained(&drained(&[("\x07", after_window)]));
        let late = deadline.expect("a settle deadline") + Duration::from_secs(5);
        let (snapshot, _) = viewer.sample_at(&session, late);
        assert!(
            !session
                .runtime_evidence_at(None, wall_time_at(late))
                .title_fresh
        );
        assert_eq!(snapshot.status, AgentStatus::Working);
        assert!(!snapshot.attributable);
        assert!(viewer.runtime.screen_held);

        // A newly opened title read after the window confirms the marker.
        let fresh = after_window + Duration::from_secs(1);
        session.process_drained(&drained(&[(working, fresh)]));
        let (snapshot, _) = viewer.sample_at(&session, late + Duration::from_millis(60));
        assert!(
            session
                .runtime_evidence_at(None, wall_time_at(late))
                .title_fresh
        );
        assert!(!viewer.runtime.screen_held);
        assert!(snapshot.attributable);
        session.kill();
    }

    #[test]
    fn a_screen_hold_ends_when_the_marker_goes_or_a_fresh_title_agrees() {
        use AgentStatus::*;
        assert!(screen_hold_after(true, Some(Blocked), None));
        assert!(screen_hold_after(true, Some(Blocked), Some(Idle)));
        assert!(!screen_hold_after(true, None, None), "the marker went");
        assert!(
            !screen_hold_after(true, Some(Working), Some(Working)),
            "a title read after the window agrees"
        );
        assert!(
            !screen_hold_after(false, Some(Blocked), None),
            "once ended, a marker that reappears is the new conversation's"
        );
    }

    #[test]
    fn a_fresh_bell_while_a_switch_settles_is_attributable() {
        let (session, _receiver) = fixture();
        let hook = SystemTime::now() - Duration::from_secs(1);
        session.process_drained(&drained(&[(
            "\x1b]0;\u{2733} Topic\x07",
            hook + Duration::from_millis(200),
        )]));
        let mut viewer = Viewer::new(TitleProvider::Claude);
        session.provider_switched_at(hook, hook, hook + Duration::from_millis(400));
        // The title read inside the window is still current, but the bell
        // was read after it: the new conversation rang it.
        session.process_drained(&drained(&[(
            "\x07",
            hook + SWITCH_SLACK + Duration::from_millis(100),
        )]));
        assert!(session.runtime_evidence(None).settling);
        assert!(session.has_bell());
        assert_eq!(viewer.sample(&session), (AgentStatus::Done, true));
        session.kill();
    }

    #[test]
    fn an_enter_before_a_whole_second_registration_ends_never_arms() {
        let (session, _receiver) = fixture();
        // Grok registered the resumed session in the second that began at
        // `second` and wrote that second whole; it switched later in it. The
        // old conversation's Enter came before the switch.
        let second = SystemTime::now() - Duration::from_millis(300);
        session.mark_turn_submitted();
        // muxel reports the switch as that whole second, up to its last instant.
        session.provider_switched_at(second, second + Duration::from_secs(1), SystemTime::now());
        assert!(
            !session.runtime_evidence(None).submitted,
            "the Enter went to the old conversation"
        );

        // The new conversation works and finishes, read after the window.
        let fresh = second + Duration::from_secs(1) + SWITCH_SLACK + Duration::from_millis(100);
        session.process_drained(&drained(&[
            ("\x1b]0;\u{2839} - Responding - Review - grok\x07", fresh),
            (
                "\x1b]0;Review - grok\x07",
                fresh + Duration::from_millis(10),
            ),
        ]));
        let (status, _, _) = Viewer::new(TitleProvider::Grok).completion(&session);
        assert_eq!(status, AgentStatus::Idle, "no completion for the old Enter");
        session.kill();
    }

    #[test]
    fn a_turn_queued_before_a_switch_never_completes_after_it() {
        let (session, _receiver) = fixture();
        // Claude's hook ran at `hook`; muxel read its record at `learned`.
        let hook = SystemTime::now() - Duration::from_secs(1);
        let learned = hook + Duration::from_millis(400);
        session.provider_switched_at(hook, hook, learned);
        // The old conversation's last turn ended as Claude switched: Working,
        // Idle and its bell, read just after the hook ran and drained only now.
        session.process_drained(&drained(&[
            (
                "\x1b]0;\u{25d0} Old topic\x07",
                hook + Duration::from_millis(20),
            ),
            (
                "\x1b]0;\u{2733} Old topic\x07\x07",
                hook + Duration::from_millis(30),
            ),
        ]));

        assert!(!session.has_bell(), "its bell rings for no binding");
        let evidence = session.runtime_evidence(None);
        assert!(evidence.titles.is_empty(), "no edge to complete");
        assert!(evidence.settling);
        // The current title still shows, as level state.
        assert_eq!(session.title().as_deref(), Some("\u{2733} Old topic"));
        let mut viewer = Viewer::new(TitleProvider::Claude);
        let (status, prev_raw, armed) = viewer.completion(&session);
        assert_eq!(status, AgentStatus::Idle, "no Done, so no notification");
        assert_eq!((prev_raw, armed), (None, false), "nor a Done later");
        session.kill();
    }

    #[test]
    fn a_switch_keeps_the_new_title_and_waits_for_fresh_evidence() {
        let (session, _receiver) = fixture();
        let hook = SystemTime::now() - Duration::from_secs(1);
        let learned = hook + Duration::from_millis(800);
        session.process_drained(&drained(&[
            // Long before the switch: the old conversation's title.
            (
                "\x1b]0;\u{2733} Old topic\x07",
                hook - Duration::from_secs(10),
            ),
            // After the hook ran, before muxel's tick read its record: the
            // new conversation's.
            (
                "\x1b]0;\u{2733} New topic\x07",
                hook + Duration::from_millis(200),
            ),
        ]));
        session.provider_switched_at(hook, hook, learned);
        assert_eq!(
            session.title().as_deref(),
            Some("\u{2733} New topic"),
            "new output before the tick is kept"
        );
        let evidence = session.runtime_evidence(None);
        assert!(evidence.titles.is_empty());
        assert!(evidence.settling);
        let mut viewer = Viewer::new(TitleProvider::Claude);
        let (status, prev_raw, armed) = viewer.completion(&session);
        assert_eq!((status, prev_raw, armed), (AgentStatus::Idle, None, false));

        // A turn submitted after the switch, its frames read after the window.
        session.mark_turn_submitted();
        session.write_input(b"\r");
        let fresh = hook + SWITCH_SLACK + Duration::from_millis(100);
        session.process_drained(&drained(&[
            ("\x1b]0;\u{25d0} New topic\x07", fresh),
            (
                "\x1b]0;\u{2733} New topic\x07",
                fresh + Duration::from_millis(10),
            ),
        ]));
        let evidence = session.runtime_evidence(None);
        assert!(!evidence.settling);
        assert!(evidence.submitted);
        assert_eq!(
            title_status_trace(TitleProvider::Claude, None, &evidence.titles),
            vec![AgentStatus::Working, AgentStatus::Idle]
        );
        let (status, _, _) = viewer.completion(&session);
        assert_eq!(status, AgentStatus::Done, "the new conversation's turn");
        session.kill();
    }

    #[test]
    fn a_legacy_grok_switch_keeps_new_evidence_and_completes_nothing() {
        let (session, _receiver) = fixture();
        let scanned_at = SystemTime::now();
        let previous_scan = scanned_at - Duration::from_secs(1);
        // Between two registry reads, the resumed conversation works,
        // finishes and rings.
        session.process_drained(&drained(&[
            (
                "\x1b]0;\u{2839} - Responding - Review - grok\x07",
                previous_scan + Duration::from_millis(300),
            ),
            (
                "\x1b]0;Review - grok\x07\x07",
                previous_scan + Duration::from_millis(600),
            ),
        ]));
        // The second read finds a row with no registration time.
        session.provider_switched_at(previous_scan, scanned_at, scanned_at);

        assert_eq!(
            session.title().as_deref(),
            Some("Review - grok"),
            "the new conversation's title is not retired"
        );
        assert!(!session.has_bell());
        let evidence = session.runtime_evidence(None);
        assert!(evidence.titles.is_empty());
        assert!(evidence.settling);
        let (status, prev_raw, armed) = Viewer::new(TitleProvider::Grok).completion(&session);
        assert_eq!(status, AgentStatus::Idle, "no fabricated completion");
        assert_eq!((prev_raw, armed), (None, false));
        session.kill();
    }
}
