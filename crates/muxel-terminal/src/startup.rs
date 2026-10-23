//! Guards for a startup turn. PTY silence is not an application's input-ready signal.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Interrupted,
    NotReady,
    NotAccepted,
    UnsupportedProvider,
    WriteFailed,
    PermissionRevoked,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AcceptanceAction {
    Observe,
    RetryEnter,
    Accepted,
    Failed,
}

/// How long after its Enter a task has to show provider evidence of acceptance.
pub(crate) const ACCEPTANCE_WINDOW: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(crate) struct Acceptance {
    considered_retry: bool,
}

impl Acceptance {
    /// A positive provider receipt wins. Missing evidence never becomes success,
    /// and the one retry requires a provider-specific retained-editor proof.
    pub(crate) fn observe(
        &mut self,
        elapsed: Duration,
        accepted: bool,
        retryable: bool,
    ) -> AcceptanceAction {
        if accepted {
            return AcceptanceAction::Accepted;
        }
        if elapsed >= ACCEPTANCE_WINDOW {
            return AcceptanceAction::Failed;
        }
        if elapsed >= Duration::from_secs(5) && !self.considered_retry {
            self.considered_retry = true;
            if retryable {
                return AcceptanceAction::RetryEnter;
            }
        }
        AcceptanceAction::Observe
    }
}

pub(crate) struct Guard {
    expected_revision: u64,
    deadline: Option<Duration>,
    ready_since: Option<Duration>,
    stopped: Option<Stop>,
    control_turn: Option<muxel_core::control::StartupTurnOwner>,
    shell_grant: Option<Arc<AtomicBool>>,
}

impl Guard {
    pub(crate) fn new(revision: u64, deadline: Option<Duration>) -> Self {
        Self {
            expected_revision: revision,
            deadline,
            ready_since: None,
            stopped: None,
            control_turn: None,
            shell_grant: None,
        }
    }

    pub(crate) fn with_control_turn(
        mut self,
        owner: Option<muxel_core::control::StartupTurnOwner>,
    ) -> Self {
        self.control_turn = owner;
        self
    }

    pub(crate) fn with_shell_grant(mut self, grant: Option<Arc<AtomicBool>>) -> Self {
        self.shell_grant = grant;
        self
    }

    pub(crate) fn submitted(&self, at: i64) {
        if let Some(owner) = &self.control_turn {
            owner.submitted(at);
        }
    }

    pub(crate) fn enter_admitted(&mut self, at: i64) {
        // Readiness has its own deadline; acceptance gets a separate bounded
        // observation window after the actual Enter, even after a slow boot.
        self.deadline = None;
        if let Some(owner) = &self.control_turn {
            owner.enter_admitted(at);
        }
    }

    pub(crate) fn fail(&mut self, reason: Stop) {
        self.stopped = Some(reason);
        if let Some(owner) = &self.control_turn {
            use muxel_core::control::StartupFailure;
            let failure = match reason {
                Stop::NotReady => StartupFailure::NotReady,
                Stop::NotAccepted => StartupFailure::NotAccepted,
                Stop::WriteFailed => StartupFailure::WriteFailed,
                Stop::UnsupportedProvider => StartupFailure::UnsupportedProvider,
                Stop::PermissionRevoked => StartupFailure::PermissionRevoked,
                Stop::Interrupted => return,
            };
            owner.fail(failure);
        }
    }

    pub(crate) fn check(&mut self, elapsed: Duration, revision: u64) -> Result<(), Stop> {
        if let Some(reason) = self.stopped {
            return Err(reason);
        }
        let reason = if self
            .control_turn
            .as_ref()
            .is_some_and(|owner| !owner.is_pending())
            || revision != self.expected_revision
        {
            Some(Stop::Interrupted)
        } else if self.deadline.is_some_and(|deadline| elapsed >= deadline) {
            Some(Stop::NotReady)
        } else {
            None
        };
        if let Some(reason) = reason {
            self.fail(reason);
            return Err(reason);
        }
        Ok(())
    }

    pub(crate) fn ready(
        &mut self,
        elapsed: Duration,
        revision: u64,
        ready: bool,
    ) -> Result<bool, Stop> {
        self.check(elapsed, revision)?;
        if !ready {
            self.ready_since = None;
            return Ok(false);
        }
        let since = *self.ready_since.get_or_insert(elapsed);
        Ok(elapsed.saturating_sub(since) >= Duration::from_millis(100))
    }

    pub(crate) fn reset_ready(&mut self) {
        self.ready_since = None;
    }

    /// Update only for our own admitted write, never adopt an observed revision.
    pub(crate) fn write(
        &mut self,
        elapsed: Duration,
        revision: u64,
        write: impl FnOnce() -> io::Result<()>,
    ) -> Result<(), Stop> {
        self.check(elapsed, revision)?;
        if self
            .shell_grant
            .as_ref()
            .is_some_and(|grant| !grant.load(Ordering::Acquire))
        {
            self.fail(Stop::PermissionRevoked);
            return Err(Stop::PermissionRevoked);
        }
        if write().is_err() {
            self.fail(Stop::WriteFailed);
            return Err(Stop::WriteFailed);
        }
        self.expected_revision = self.expected_revision.wrapping_add(1);
        Ok(())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(owner) = &self.control_turn {
            owner.cancel();
        }
    }
}

/// Polls a [`TitleSource`](crate::TitleSource) on its own thread while one
/// controller task waits for readiness and acceptance, so a blocking read
/// (a tmux client) never runs on the UI thread. Each change of the read title
/// bumps a generation, the way an OSC title event bumps the session's. It
/// stops when dropped, or after `lifetime` if its owner never drops it.
pub(crate) struct TitleProbe {
    shared: std::sync::Arc<ProbeShared>,
    /// Never joined in production: a blocked read must not stall its owner.
    #[cfg_attr(not(test), allow(dead_code))]
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct ProbeShared {
    state: parking_lot::Mutex<ProbeState>,
    wake: parking_lot::Condvar,
}

#[derive(Default)]
struct ProbeState {
    generation: u64,
    title: Option<String>,
    stopped: bool,
}

impl TitleProbe {
    pub(crate) fn spawn(
        source: crate::TitleSource,
        interval: Duration,
        lifetime: Duration,
    ) -> Self {
        let shared = std::sync::Arc::new(ProbeShared::default());
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("muxel-title-probe".into())
            .spawn(move || {
                let started = std::time::Instant::now();
                while started.elapsed() < lifetime && !worker.state.lock().stopped {
                    let title = source.read();
                    let mut state = worker.state.lock();
                    if state.stopped {
                        break;
                    }
                    if state.title != title {
                        state.title = title;
                        state.generation += 1;
                    }
                    let _ = worker.wake.wait_for(&mut state, interval);
                }
            });
        if spawned.is_err() {
            log::warn!("could not start the startup title probe");
        }
        Self {
            shared,
            worker: spawned.ok(),
        }
    }

    /// Tests: stop (or, with `stop` false, let the lifetime end) and wait for
    /// the worker, so nothing can read after this returns.
    #[cfg(test)]
    fn join(mut self, stop: bool) {
        if stop {
            self.shared.state.lock().stopped = true;
            self.shared.wake.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }

    /// The latest title read and its change generation.
    pub(crate) fn snapshot(&self) -> (u64, Option<String>) {
        let state = self.shared.state.lock();
        (state.generation, state.title.clone())
    }
}

impl Drop for TitleProbe {
    fn drop(&mut self) {
        self.shared.state.lock().stopped = true;
        self.shared.wake.notify_all();
    }
}

pub(crate) struct Screen {
    pub text: String,
    pub cursor_column: usize,
    pub cursor_row: i32,
    pub cursor_visible: bool,
    pub bracketed_paste: bool,
}

impl Screen {
    /// Codex's unframed editor. A passive model/cwd status line can replace the
    /// shortcut hint even with empty input; a draft hides that hint as well.
    /// A trust/choice menu also uses `›`, but has no composer status footer.
    pub(crate) fn codex_composer_context(&self, empty: bool) -> Option<String> {
        let rows: Vec<_> = self.text.lines().collect();
        let region = self.codex_composer_rows(&rows, empty)?;
        Some(
            rows[..region.start]
                .iter()
                .chain(rows[region.end..].iter())
                .copied()
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    /// Retry only a completely visible single-row ASCII task with its cursor at
    /// the exact end. Reflow, tabs, newlines and Unicode cell widths cannot be
    /// reconstructed losslessly from this snapshot, so ambiguity refuses retry.
    pub(crate) fn codex_exact_retry_draft(&self, input: &str) -> bool {
        if input.is_empty()
            || !input.is_ascii()
            || input.trim() != input
            || input.chars().any(char::is_control)
        {
            return false;
        }
        let rows: Vec<_> = self.text.lines().collect();
        let Some(region) = self.codex_composer_rows(&rows, false) else {
            return false;
        };
        let top = region.start;
        if usize::try_from(self.cursor_row).ok() != Some(top)
            || region.end <= top + 1
            // A blank line can also be part of a longer draft. Every row up to
            // the independently recognized model/cwd footer must be blank.
            || rows[top + 1..region.end]
                .iter()
                .any(|row| !row.trim().is_empty())
        {
            return false;
        }
        let prefix = rows[top]
            .chars()
            .take_while(|ch| ch.is_whitespace())
            .count()
            + 2;
        let end = prefix + input.len();
        self.cursor_column == end
            && rows[top]
                .chars()
                .skip(prefix)
                .take(input.len())
                .collect::<String>()
                == input
            && rows[top].chars().skip(end).all(char::is_whitespace)
    }

    fn codex_composer_rows(&self, rows: &[&str], empty: bool) -> Option<std::ops::Range<usize>> {
        if !self.cursor_visible || !self.bracketed_paste {
            return None;
        }
        let cursor = usize::try_from(self.cursor_row).ok()?;
        let bottom = rows.iter().rposition(|row| codex_footer(row))?;
        // Use the final prompt in the whole snapshot: a later menu cannot
        // borrow an earlier editor's footer or cursor position.
        let top = rows.iter().rposition(|row| {
            row.trim_start()
                .strip_prefix('›')
                .is_some_and(|after| after.is_empty() || after.starts_with(char::is_whitespace))
        })?;
        if top >= bottom || cursor < top || cursor >= bottom {
            return None;
        }
        let prefix = rows[top]
            .chars()
            .take_while(|ch| ch.is_whitespace())
            .count();
        if (cursor == top && self.cursor_column == prefix + 2) != empty {
            return None;
        }
        Some(top..bottom)
    }

    pub(crate) fn codex_draft_matches(&self, input: &str) -> bool {
        if input.len() > self.text.len() {
            return false;
        }
        let rows: Vec<_> = self.text.lines().collect();
        let Some(region) = self.codex_composer_rows(&rows, false) else {
            return false;
        };
        let top = region.start;
        let cursor = usize::try_from(self.cursor_row).unwrap_or(top);
        if cursor < top || cursor >= rows.len() {
            return false;
        }
        let text = std::iter::once(rows[top].trim_start().trim_start_matches('›').trim())
            .chain(rows[top + 1..=cursor].iter().map(|row| row.trim()))
            .collect::<Vec<_>>()
            .join(" ");
        normalized(&text) == normalized(input)
    }

    pub(crate) fn claude_draft_matches(&self, input: &str) -> bool {
        if input.len() > self.text.len() {
            return false;
        }
        let Some(region) = self.composer_rows(false) else {
            return false;
        };
        let rows: Vec<_> = self.text.lines().collect();
        let text = std::iter::once(
            rows[region.start]
                .trim_start()
                .trim_start_matches('❯')
                .trim(),
        )
        .chain(
            rows[region.start + 1..region.end]
                .iter()
                .map(|row| row.trim()),
        )
        .collect::<Vec<_>>()
        .join(" ");
        normalized(&text) == normalized(input)
    }

    /// The cursor must belong to the live input region between two rules. A menu
    /// choice or a quoted prompt elsewhere in the viewport is not a composer.
    /// `empty` uses cursor position, allowing Claude's placeholder text to remain.
    #[cfg(test)]
    pub(crate) fn claude_composer(&self, empty: bool) -> bool {
        self.composer_rows(empty).is_some()
    }

    /// Permission chrome belongs outside the live composer. A task quoting a
    /// permission question must not be mistaken for that question after paste.
    pub(crate) fn claude_composer_context(&self, empty: bool) -> Option<String> {
        let composer = self.composer_rows(empty)?;
        Some(
            self.text
                .lines()
                .enumerate()
                .filter(|(index, _)| !composer.contains(index))
                .map(|(_, row)| row)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    fn composer_rows(&self, empty: bool) -> Option<std::ops::Range<usize>> {
        if !self.bracketed_paste || !self.cursor_visible {
            return None;
        }
        let Ok(cursor) = usize::try_from(self.cursor_row) else {
            return None;
        };
        let rows: Vec<_> = self.text.lines().collect();
        if cursor >= rows.len() {
            return None;
        }
        let rule = |row: &str| {
            let row = row.trim();
            row.chars().count() >= 8 && row.chars().all(|ch| ch == '─' || ch == '━')
        };
        let top = rows[..cursor].iter().rposition(|row| rule(row))?;
        let bottom = rows[cursor + 1..]
            .iter()
            .position(|row| rule(row))
            .map(|offset| cursor + 1 + offset)?;
        // The final framed region belongs to the composer, not an earlier turn.
        if rows[bottom + 1..].iter().any(|row| rule(row)) {
            return None;
        }
        let prompt = rows[top + 1];
        let prefix = prompt.chars().take_while(|ch| ch.is_whitespace()).count();
        let after = prompt.trim_start().strip_prefix('❯')?;
        if !after.starts_with(char::is_whitespace) {
            return None;
        }
        let cursor_at_start = cursor == top + 1 && self.cursor_column == prefix + 2;
        (cursor_at_start == empty).then_some(top + 1..bottom)
    }
}

/// A row Codex draws below its live editor: its own shortcut hint or context
/// meter (`? for shortcuts … 98% context left`), or a passive status line that
/// opens with a model id (`gpt-5.4 high fast · scratch`, `o4-mini · …`, a custom
/// provider's `llama3.1:8b · …`). A model name with no digit and no `gpt-` or
/// `codex-` prefix is not recognised without one of the other two footers.
/// Menus separate key hints with ` · ` as well (`enter continue · esc quit`),
/// so a status line must open with a model-like token and no later segment may
/// read as a key hint. A segment that merely is such a word (a project folder
/// named `continue`) is not one.
fn codex_footer(row: &str) -> bool {
    let row = row.trim();
    if row.starts_with("? for shortcuts") || row.contains("% context left") {
        return true;
    }
    let mut segments = row.split(" · ");
    let Some(id) = segments
        .next()
        .and_then(|model| model.split_whitespace().next())
    else {
        return false;
    };
    let openai = ["gpt-", "codex-"].iter().any(|prefix| {
        id.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    });
    let model_like = id.len() >= 2
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/@+-".contains(&b))
        && (openai || id.bytes().any(|b| b.is_ascii_digit()));
    let mut rest = segments.peekable();
    model_like && rest.peek().is_some() && !rest.any(key_hint)
}

/// `enter to confirm`, `esc quit`, `press enter …`, `ctrl+t transcript`,
/// `↑/↓ select`: a key (or `press`) followed by what it does.
fn key_hint(segment: &str) -> bool {
    const KEYS: &[&str] = &[
        "enter", "esc", "escape", "tab", "space", "ctrl", "shift", "alt", "press", "⏎", "↵", "↑",
        "↓", "←", "→", "↑/↓",
    ];
    let mut words = segment.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    let first = first.to_ascii_lowercase();
    let key = KEYS.contains(&first.as_str())
        || ["ctrl+", "shift+", "alt+", "ctrl-", "^"]
            .iter()
            .any(|prefix| first.starts_with(prefix) && first.len() > prefix.len());
    key && words.next().is_some()
}

pub(crate) fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{Guard, Screen, Stop};
    use std::{
        cell::Cell,
        io,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn screen(text: &str, row: i32, column: usize) -> Screen {
        Screen {
            text: text.into(),
            cursor_column: column,
            cursor_row: row,
            cursor_visible: true,
            bracketed_paste: true,
        }
    }

    #[test]
    fn startup_control_turn_tracks_guarded_paste_submit_and_every_stop_tail() {
        use muxel_core::control::{StartupTurn, StartupTurnState};
        let turn = StartupTurn::default();
        let mut guard = Guard::new(0, None).with_control_turn(turn.claim());
        guard.write(ms(1), 0, || Ok(())).unwrap();
        assert_eq!(turn.state(), StartupTurnState::Pending);
        guard.write(ms(2), 1, || Ok(())).unwrap();
        guard.enter_admitted(123);
        assert_eq!(turn.state(), StartupTurnState::AwaitingAcceptance(123));
        guard.submitted(123);
        drop(guard);
        assert_eq!(turn.state(), StartupTurnState::Submitted(123));

        for stop in [
            None,
            Some(Stop::Interrupted),
            Some(Stop::NotReady),
            Some(Stop::NotAccepted),
            Some(Stop::UnsupportedProvider),
            Some(Stop::WriteFailed),
            Some(Stop::PermissionRevoked),
        ] {
            let turn = StartupTurn::default();
            let owner = turn.claim().unwrap();
            let extra_command_clone = owner.clone();
            let mut guard = Guard::new(0, Some(ms(10))).with_control_turn(Some(owner));
            match stop {
                None => {} // no-input, submit=false, or future dropped before execution
                Some(Stop::Interrupted) => {
                    assert_eq!(guard.check(ms(1), 1), Err(Stop::Interrupted))
                }
                Some(Stop::NotReady) => assert_eq!(guard.check(ms(10), 0), Err(Stop::NotReady)),
                Some(
                    reason @ (Stop::NotAccepted
                    | Stop::UnsupportedProvider
                    | Stop::PermissionRevoked),
                ) => guard.fail(reason),
                Some(Stop::WriteFailed) => assert_eq!(
                    guard.write(ms(1), 0, || Err(io::Error::other("closed"))),
                    Err(Stop::WriteFailed)
                ),
            }
            drop(guard);
            let expected = match stop {
                Some(Stop::NotReady) => {
                    StartupTurnState::Failed(muxel_core::control::StartupFailure::NotReady)
                }
                Some(Stop::WriteFailed) => {
                    StartupTurnState::Failed(muxel_core::control::StartupFailure::WriteFailed)
                }
                Some(Stop::NotAccepted) => {
                    StartupTurnState::Failed(muxel_core::control::StartupFailure::NotAccepted)
                }
                Some(Stop::UnsupportedProvider) => StartupTurnState::Failed(
                    muxel_core::control::StartupFailure::UnsupportedProvider,
                ),
                Some(Stop::PermissionRevoked) => {
                    StartupTurnState::Failed(muxel_core::control::StartupFailure::PermissionRevoked)
                }
                _ => StartupTurnState::Cancelled,
            };
            assert_eq!(turn.state(), expected);
            assert!(!extra_command_clone.is_pending());
        }
        let turn = StartupTurn::default();
        let mut guard = Guard::new(0, None).with_control_turn(turn.claim());
        turn.cancel();
        assert_eq!(
            guard.write(ms(1), 0, || panic!("cancelled automation must not write")),
            Err(Stop::Interrupted)
        );
    }
    #[test]
    fn revoked_tmux_grant_refuses_queued_paste_submit_and_retry_writes() {
        use muxel_core::control::{StartupFailure, StartupTurn};

        for admitted_writes in [0, 1, 2] {
            let grant = Arc::new(AtomicBool::new(true));
            let turn = StartupTurn::default();
            let mut guard = Guard::new(0, None)
                .with_control_turn(turn.claim())
                .with_shell_grant(Some(grant.clone()));
            let writes = Cell::new(0);
            for revision in 0..admitted_writes {
                guard
                    .write(ms(revision + 1), revision, || {
                        writes.set(writes.get() + 1);
                        Ok(())
                    })
                    .unwrap();
            }
            if admitted_writes == 2 {
                guard.enter_admitted(123);
            }
            grant.store(false, Ordering::Release);
            assert_eq!(
                guard.write(ms(3), admitted_writes, || {
                    writes.set(writes.get() + 1);
                    Ok(())
                }),
                Err(Stop::PermissionRevoked)
            );
            assert_eq!(writes.get(), admitted_writes);
            assert_eq!(turn.failure(), Some(StartupFailure::PermissionRevoked));
        }
    }
    #[test]
    fn quiet_boot_does_not_authorize_input_and_deadline_never_falls_through() {
        let mut guard = Guard::new(0, Some(ms(30_000)));
        assert!(!guard.ready(ms(2500), 0, false).unwrap());
        assert!(!guard.ready(ms(7000), 0, true).unwrap());
        assert!(guard.ready(ms(7100), 0, true).unwrap());
        let mut late = Guard::new(0, Some(ms(30_000)));
        assert_eq!(late.ready(ms(30_000), 0, true), Err(Stop::NotReady));
        let writes = Cell::new(0);
        assert_eq!(
            late.write(ms(30_001), 0, || {
                writes.set(1);
                Ok(())
            }),
            Err(Stop::NotReady)
        );
        assert_eq!(writes.get(), 0);
    }

    #[test]
    fn enter_without_a_provider_receipt_can_only_retry_once_then_fail() {
        use super::{Acceptance, AcceptanceAction::*};
        let mut acceptance = Acceptance::default();
        assert_eq!(acceptance.observe(ms(4999), false, true), Observe);
        assert_eq!(acceptance.observe(ms(5000), false, true), RetryEnter);
        assert_eq!(acceptance.observe(ms(5100), false, true), Observe);
        assert_eq!(acceptance.observe(ms(9999), false, true), Observe);
        assert_eq!(acceptance.observe(ms(10_000), false, true), Failed);
        let mut unsupported_retry = Acceptance::default();
        assert_eq!(unsupported_retry.observe(ms(5000), false, false), Observe);
        assert_eq!(unsupported_retry.observe(ms(6000), false, true), Observe);
        assert_eq!(unsupported_retry.observe(ms(10_000), false, false), Failed);
        let mut accepted = Acceptance::default();
        assert_eq!(accepted.observe(ms(5000), true, true), Accepted);
    }

    #[test]
    fn codex_requires_a_live_editor_and_never_treats_folder_trust_as_input() {
        let trust = screen(
            "Folder access\nTrust this folder?\n› 1. Trust and continue\n  2. Quit\nenter continue · esc quit",
            2,
            2,
        );
        assert!(trust.codex_composer_context(true).is_none());
        let mut ready = screen(
            "OpenAI Codex\n› Ask Codex to do anything\n\n  GPT-6-Sol · scratch\n  ⚠ 1 · f2",
            1,
            2,
        );
        assert!(ready.codex_composer_context(true).is_some());
        ready.text =
            "OpenAI Codex\n› Answer exactly\n  B8_OK\n\n  gpt-5.4 high fast · scratch · Context 0% used\n  ⚠ 1 · f2"
                .into();
        ready.cursor_row = 2;
        ready.cursor_column = 7;
        assert!(ready.codex_composer_context(false).is_some());
        assert!(ready.codex_draft_matches("Answer exactly B8_OK"));
        assert!(!ready.codex_draft_matches("A different task"));
        ready.cursor_visible = false;
        assert!(!ready.codex_draft_matches("Answer exactly B8_OK"));
        let residual_menu = screen(
            "OpenAI Codex\n› task\n\n  GPT-6-Sol · scratch\n  ? for shortcuts\n› 1. menu",
            1,
            6,
        );
        assert!(!residual_menu.codex_draft_matches("task"));
    }
    #[test]
    fn codex_editor_accepts_non_gpt_models_and_the_shortcut_footer_but_no_menu() {
        for footer in [
            "  o4-mini medium · scratch · Context 0% used",
            "  o3 · scratch",
            "  llama3.1:8b · ~/src/webapp",
            "  codex-mini-latest · scratch",
            "  ? for shortcuts                                   98% context left",
            "  ? for shortcuts",
            "                                                    98% context left",
        ] {
            let empty = screen(
                &format!("OpenAI Codex\n› Ask Codex to do anything\n\n{footer}"),
                1,
                2,
            );
            assert!(empty.codex_composer_context(true).is_some(), "{footer}");
            assert!(empty.codex_composer_context(false).is_none(), "{footer}");
            let draft = screen(&format!("OpenAI Codex\n› run the tests\n\n{footer}"), 1, 15);
            assert!(draft.codex_draft_matches("run the tests"), "{footer}");
            assert!(draft.codex_exact_retry_draft("run the tests"), "{footer}");
        }
        // A project folder named like a key word is still a status line.
        for footer in [
            "  gpt-5 · continue",
            "  gpt-5.4 high · select · Context 0% used",
            "  o3 · tab",
            "  gpt-5 · cancel · enter",
        ] {
            let empty = screen(
                &format!("OpenAI Codex\n› Ask Codex to do anything\n\n{footer}"),
                1,
                2,
            );
            assert!(empty.codex_composer_context(true).is_some(), "{footer}");
        }
        // Key-hint rows are not editor footers, whatever precedes them; nor is
        // a row that only shares the separator or a model-less word.
        for footer in [
            "enter continue · esc quit",
            "  ↑/↓ select · enter confirm",
            "  gpt-5.4 · press enter to continue",
            "  gpt-5.4 · enter to confirm · esc to cancel",
            "  o3 · Esc quit",
            "  gpt-5 · ctrl+c exit",
            "  gpt-5.4",
            "  ⚠ 1 · f2",
            "  scratch · Context 0% used",
            "  sonnet · scratch",
            "  Press enter to confirm or esc to cancel",
        ] {
            let menu = screen(
                &format!("Pick one\n› 1. First\n  2. Second\n{footer}"),
                1,
                2,
            );
            assert!(menu.codex_composer_context(true).is_none(), "{footer}");
            assert!(menu.codex_composer_context(false).is_none(), "{footer}");
        }
    }

    #[test]
    fn title_probe_counts_changes_only_and_stops_when_dropped() {
        use super::TitleProbe;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = reads.clone();
        let probe = TitleProbe::spawn(
            crate::TitleSource::new(move || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                Some(
                    if n < 3 {
                        "scratch | Starting"
                    } else {
                        "scratch | Ready"
                    }
                    .into(),
                )
            }),
            ms(5),
            Duration::from_secs(30),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while probe.snapshot().1.as_deref() != Some("scratch | Ready") {
            assert!(std::time::Instant::now() < deadline, "probe never updated");
            std::thread::sleep(ms(5));
        }
        // Repeated identical reads are not new title events.
        while reads.load(Ordering::SeqCst) < 8 {
            assert!(
                std::time::Instant::now() < deadline,
                "probe stopped polling"
            );
            std::thread::sleep(ms(5));
        }
        assert_eq!(probe.snapshot().0, 2);
        probe.join(true);
        let stopped = reads.load(Ordering::SeqCst);
        std::thread::sleep(ms(50));
        assert_eq!(
            reads.load(Ordering::SeqCst),
            stopped,
            "stopped probe kept reading"
        );

        // Its own lifetime bounds a probe whose owner never drops it.
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = reads.clone();
        let forgotten = TitleProbe::spawn(
            crate::TitleSource::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                None
            }),
            ms(5),
            ms(40),
        );
        forgotten.join(false);
        let ended = reads.load(Ordering::SeqCst);
        assert!(ended > 0);
        std::thread::sleep(ms(50));
        assert_eq!(reads.load(Ordering::SeqCst), ended);
    }

    #[test]
    fn external_input_before_paste_or_after_our_paste_stops_automation() {
        let mut before = Guard::new(0, None);
        assert_eq!(before.check(ms(10), 1), Err(Stop::Interrupted));
        let mut after = Guard::new(0, None);
        after.write(ms(10), 0, || Ok(())).unwrap();
        assert!(after.check(ms(20), 1).is_ok());
        assert_eq!(
            after.write(ms(30), 2, || panic!("must not submit edited input")),
            Err(Stop::Interrupted)
        );
    }
    #[test]
    fn failed_paste_or_submit_stops_without_another_write() {
        for admitted_paste in [false, true] {
            let mut guard = Guard::new(0, None);
            if admitted_paste {
                guard.write(ms(1), 0, || Ok(())).unwrap();
            }
            let revision = u64::from(admitted_paste);
            assert_eq!(
                guard.write(ms(2), revision, || Err(io::Error::other("writer closed"))),
                Err(Stop::WriteFailed)
            );
            assert_eq!(
                guard.write(ms(3), revision, || panic!("no retry after failure")),
                Err(Stop::WriteFailed)
            );
        }
    }
    #[test]
    fn real_composer_allows_placeholder_and_wrapped_or_multiline_draft() {
        let empty = screen(
            "Claude Code\n────────────────\n❯ Try a task\n────────────────\nmode",
            2,
            2,
        );
        assert!(empty.claude_composer(true));
        assert!(!empty.claude_composer(false));
        let draft = screen(
            "Claude Code\n────────────────\n❯ /b12-ready\n  wrapped continuation\n  second line\n────────────────\nmode",
            4,
            7,
        );
        assert!(!draft.claude_composer(true));
        assert!(draft.claude_composer(false));
    }
    #[test]
    fn trust_choice_quoted_transcript_and_wrong_terminal_modes_are_not_ready() {
        for (text, row, column) in [
            (
                "────────────────\nAccessing workspace\n❯ No, exit\n  Yes, I trust this folder\nEnter to confirm · Esc to cancel",
                2,
                2,
            ),
            (
                "❯ /b12-ready\n● quoted answer\n────────────────\n❯ Try a task\n────────────────\nmode",
                0,
                2,
            ),
            (
                "────────────────\nDo you trust this?\n❯ Yes\n────────────────\nEsc to cancel",
                2,
                2,
            ),
        ] {
            assert!(!screen(text, row, column).claude_composer(true));
        }
        let mut sample = screen("────────────────\n❯ Try a task\n────────────────", 1, 2);
        sample.bracketed_paste = false;
        assert!(!sample.claude_composer(true));
        sample.bracketed_paste = true;
        sample.cursor_visible = false;
        assert!(!sample.claude_composer(true));
    }
}
