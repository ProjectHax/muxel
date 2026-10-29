//! Guards for a startup turn. PTY silence is not an application's input-ready signal.

use std::{io, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Interrupted,
    NotReady,
    NotAccepted,
    UnsupportedProvider,
    WriteFailed,
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
}

impl Guard {
    pub(crate) fn new(revision: u64, deadline: Option<Duration>) -> Self {
        Self {
            expected_revision: revision,
            deadline,
            ready_since: None,
            stopped: None,
            control_turn: None,
        }
    }

    pub(crate) fn with_control_turn(
        mut self,
        owner: Option<muxel_core::control::StartupTurnOwner>,
    ) -> Self {
        self.control_turn = owner;
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

/// An empty-looking, unclipped one-row origin, captured before ctl paste.
/// Character snapshots cannot distinguish spaces at Home or a typed copy of
/// the placeholder from actual empty input; this does not claim zero text.
pub(crate) struct CodexCtlOrigin {
    columns: usize,
    rows: usize,
    top: usize,
    footer: usize,
    prefix: usize,
    footer_row: String,
}

struct CodexCtlDraft {
    columns: usize,
    rows: usize,
    top: usize,
    footer: usize,
    prefix: usize,
    end_row: usize,
}

impl Screen {
    // Master can render a model row followed by a shortcuts footer. The
    // generic composer scan uses the last footer; ctl's geometry proof pins
    // the earlier model row when both are present.
    fn codex_ctl_footer(rows: &[&str], end: usize) -> Option<usize> {
        let last = rows.get(end)?.trim_start();
        if last.starts_with("? for shortcuts") && end > 0 && codex_footer(rows[end - 1]) {
            Some(end - 1)
        } else {
            Some(end)
        }
    }

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

    /// Ctl receipts must exclude the geometry-proven live composer regardless
    /// of its height. The general read-aloud scan has a bounded footer reach;
    /// a long retained draft must never become its own submitted-user receipt.
    pub(crate) fn codex_ctl_transcript_prompt(&self) -> Option<String> {
        let rows: Vec<_> = self.text.lines().collect();
        let region = self
            .codex_composer_rows(&rows, true)
            .or_else(|| self.codex_composer_rows(&rows, false))?;
        let transcript = &rows[..region.start];
        let start = transcript
            .iter()
            .rposition(|row| row.trim_start().strip_prefix("› ").is_some())?;
        let first = transcript[start].trim_start().strip_prefix("› ")?.trim();
        let mut parts = vec![first];
        for row in &transcript[start + 1..] {
            if row.trim().is_empty() || !row.starts_with(' ') {
                break;
            }
            parts.push(row.trim());
        }
        let prompt = parts.join(" ");
        (!prompt.is_empty()).then_some(prompt)
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

    pub(crate) fn codex_ctl_origin(&self) -> Option<CodexCtlOrigin> {
        if self.text.contains(['↑', '↓']) {
            return None;
        }
        let rows: Vec<_> = self.text.lines().collect();
        let region = self.codex_composer_rows(&rows, true)?;
        let footer = Self::codex_ctl_footer(&rows, region.end)?;
        let columns = rows.first()?.chars().count();
        if rows.iter().any(|row| row.chars().count() != columns)
            || region.start == 0
            || footer != region.start + 2
            || !rows[region.start - 1].bytes().all(|byte| byte == b' ')
            || !rows[region.start + 1].bytes().all(|byte| byte == b' ')
        {
            return None;
        }
        let prompt = rows[region.start];
        let indent = prompt.bytes().take_while(|byte| *byte == b' ').count();
        let content = prompt[indent..].strip_prefix("› ")?.trim_end_matches(' ');
        if !matches!(content, "" | "Ask Codex to do anything") {
            return None;
        }
        Some(CodexCtlOrigin {
            columns,
            rows: rows.len(),
            top: region.start,
            footer,
            prefix: indent + 2,
            footer_row: rows[footer].to_owned(),
        })
    }

    /// A retained end cursor is not enough: the first CR might still be queued.
    /// From a compact LF-free origin, require the complete LF-free task followed
    /// by one new blank editor line, bottom anchored at the original cursor row.
    pub(crate) fn codex_ctl_retry_draft(&self, input: &str, origin: &CodexCtlOrigin) -> bool {
        self.codex_ctl_draft_frame(input).is_some_and(|frame| {
            usize::try_from(self.cursor_row).ok() == Some(frame.end_row + 1)
                && frame.end_row + 1 == origin.top
                && self.cursor_column == origin.prefix
                && frame.columns == origin.columns
                && frame.rows == origin.rows
                && frame.footer == origin.footer
                && frame.prefix == origin.prefix
                && frame.top < origin.top
                && self.text.lines().nth(frame.footer) == Some(origin.footer_row.as_str())
        })
    }

    /// Match the entire visible ASCII task across soft wraps. Codex can split a
    /// hyphenated word without consuming a space at the row boundary.
    fn codex_ctl_draft_frame(&self, input: &str) -> Option<CodexCtlDraft> {
        if input.is_empty()
            || !input.is_ascii()
            || input.trim() != input
            || input.bytes().any(|byte| byte.is_ascii_control())
            || input.contains("  ")
            // Completion and shell syntax can give Enter a different owner,
            // even when the editor itself still contains the exact task.
            || input.starts_with('!')
            || input.contains('@')
            || input.contains('$')
            || input.split(' ').any(|word| word.starts_with('/'))
        {
            return None;
        }
        if self.text.contains(['↑', '↓']) {
            return None;
        }
        let rows: Vec<_> = self.text.lines().collect();
        let region = self.codex_composer_rows(&rows, false)?;
        let footer = Self::codex_ctl_footer(&rows, region.end)?;
        let columns = rows.first()?.chars().count();
        if rows.iter().any(|row| row.chars().count() != columns) {
            return None;
        }
        // A top-edge editor can be a vertically clipped draft. Refuse it even
        // if its remaining text happens to resemble the requested task.
        if region.start == 0
            || !rows[region.start - 1].bytes().all(|byte| byte == b' ')
            || !rows[footer - 1].bytes().all(|byte| byte == b' ')
        {
            return None;
        }
        let mut context = rows[..region.start].iter().chain(rows[region.end..].iter());
        if context.any(|row| {
            let row = row.to_ascii_lowercase();
            [
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
            .any(|hint| row.contains(hint))
        }) {
            return None;
        }
        let top = rows[region.start];
        let indent = top.bytes().take_while(|byte| *byte == b' ').count();
        let first = top[indent..].strip_prefix("› ")?;
        let prefix = indent + 2;
        let mut pieces = vec![first.trim_end_matches(' ')];
        let mut last = region.start;
        let mut ended = false;
        for (index, row) in rows.iter().enumerate().take(footer).skip(region.start + 1) {
            if row.bytes().all(|byte| byte == b' ') {
                ended = true;
                continue;
            }
            if ended
                || !row
                    .as_bytes()
                    .get(..prefix)
                    .is_some_and(|s| s.iter().all(|b| *b == b' '))
            {
                return None;
            }
            let piece = row[prefix..].trim_end_matches(' ');
            pieces.push(piece);
            last = index;
        }
        if pieces.iter().any(|piece| {
            piece.is_empty()
                || !piece.is_ascii()
                || piece.starts_with(' ')
                || piece.bytes().any(|byte| byte.is_ascii_control())
        }) {
            return None;
        }
        let mut remaining = input;
        for (index, piece) in pieces.iter().enumerate() {
            remaining = remaining.strip_prefix(*piece)?;
            if let Some(next) = pieces.get(index + 1) {
                if let Some(rest) = remaining.strip_prefix(' ') {
                    remaining = rest;
                } else {
                    // An ordinary row break consumes a space. A no-space
                    // break needs proof that the next word could not fit, or
                    // that the current row filled the editor width.
                    let used = prefix + piece.len();
                    let next_word = next.split(' ').next()?;
                    let limit = columns.saturating_sub(2);
                    if !(used >= limit || (piece.ends_with('-') && used + next_word.len() > limit))
                    {
                        return None;
                    }
                }
            }
        }
        if !remaining.is_empty() {
            return None;
        }
        let end_column = prefix + pieces.last()?.len();
        if end_column >= columns.saturating_sub(2) {
            return None;
        }
        Some(CodexCtlDraft {
            columns,
            rows: rows.len(),
            top: region.start,
            footer,
            prefix,
            end_row: last,
        })
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
        Some(self.context_without_rows(composer))
    }

    /// Status leaves out Claude's input box whether it holds a draft or the
    /// placeholder, and wherever a repaint left the cursor.
    pub(crate) fn claude_status_context(&self) -> Option<String> {
        let composer = self.composer_rows_inner(None)?;
        Some(self.context_without_rows(composer))
    }

    fn context_without_rows(&self, composer: std::ops::Range<usize>) -> String {
        self.text
            .lines()
            .enumerate()
            .filter(|(index, _)| !composer.contains(index))
            .map(|(_, row)| row)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn composer_rows(&self, empty: bool) -> Option<std::ops::Range<usize>> {
        self.composer_rows_inner(Some(empty))
    }

    /// `empty` is the startup-input test, which needs the visible cursor inside
    /// the box. `None` finds the box for status alone and never grants input.
    fn composer_rows_inner(&self, empty: Option<bool>) -> Option<std::ops::Range<usize>> {
        if !self.bracketed_paste || (empty.is_some() && !self.cursor_visible) {
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
        let (top, bottom) = if empty.is_some() {
            let top = rows[..cursor].iter().rposition(|row| rule(row))?;
            let bottom = rows[cursor + 1..]
                .iter()
                .position(|row| rule(row))
                .map(|offset| cursor + 1 + offset)?;
            (top, bottom)
        } else {
            // A repaint can leave the cursor anywhere, so take the final box.
            // Its edges are the same full-width rule. Draft rows are drawn
            // under the prompt, two columns in, so a rule typed into a draft
            // never matches an edge and can't cut the box short.
            let bottom = rows.iter().rposition(|row| rule(row))?;
            let edge = rows[bottom].trim_end();
            let top = rows[..bottom]
                .iter()
                .rposition(|row| row.trim_end() == edge)?;
            (top, bottom)
        };
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
        empty
            .is_none_or(|empty| cursor_at_start == empty)
            .then_some(top + 1..bottom)
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
    use std::{cell::Cell, io, time::Duration};
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
                Some(reason @ (Stop::NotAccepted | Stop::UnsupportedProvider)) => {
                    guard.fail(reason)
                }
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

    fn ctl_screen_with_width(draft: &[&str], consumed: bool, width: usize) -> Screen {
        let mut rows = vec![String::new(); 51];
        rows[0] = "OpenAI Codex".into();
        let top = 49 - 1 - draft.len() - usize::from(consumed);
        for (index, piece) in draft.iter().enumerate() {
            rows[top + index] = format!("{}{piece}", if index == 0 { "› " } else { "  " });
        }
        rows[49] = "  GPT-6.1-Sol · scratch".into();
        rows[50] = "  ? for shortcuts".into();
        let text = rows
            .iter()
            .map(|row| format!("{row}{}", " ".repeat(width - row.chars().count())))
            .collect::<Vec<_>>()
            .join("\n");
        screen(
            &text,
            (top + draft.len() - usize::from(!consumed)) as i32,
            if consumed {
                2
            } else {
                2 + draft.last().unwrap().chars().count()
            },
        )
    }

    fn ctl_screen(draft: &[&str], consumed: bool) -> Screen {
        ctl_screen_with_width(draft, consumed, 41)
    }

    fn ctl_origin(text: &str) -> Screen {
        let mut sample = ctl_screen(&[text], false);
        sample.cursor_column = 2;
        sample
    }

    fn ctl_set_row(sample: &mut Screen, index: usize, text: &str) {
        let mut rows = sample.text.lines().map(str::to_owned).collect::<Vec<_>>();
        rows[index] = format!("{text}{}", " ".repeat(41 - text.chars().count()));
        sample.text = rows.join("\n");
    }

    #[test]
    fn ctl_codex_origin_requires_compact_empty_looking_editor() {
        for text in ["", "Ask Codex to do anything", "   "] {
            assert!(ctl_origin(text).codex_ctl_origin().is_some(), "{text:?}");
        }
        // Character-only snapshots cannot distinguish typed placeholder text
        // or spaces at Home. Exact post-paste matching still rejects leftovers.
        for text in [
            "old task",
            "Ask Codex to do something",
            " Ask Codex to do anything",
        ] {
            assert!(ctl_origin(text).codex_ctl_origin().is_none(), "{text:?}");
        }
        for count in [2, 18] {
            let mut multiline = ctl_screen(&vec![""; count], false);
            multiline.cursor_row = (48 - count) as i32;
            multiline.cursor_column = 2;
            assert!(multiline.codex_composer_context(true).is_some());
            assert!(
                multiline.codex_ctl_origin().is_none(),
                "blank multiline at Home"
            );
        }
        for (row, text) in [
            (0, "↑ more above"),
            (48, "↓ more below"),
            (46, "not padding"),
            (48, "not padding"),
        ] {
            let mut sample = ctl_origin("");
            ctl_set_row(&mut sample, row, text);
            assert!(sample.codex_ctl_origin().is_none(), "{row}: {text}");
        }
        let mut sample = ctl_origin("");
        sample.cursor_column = 3;
        assert!(sample.codex_ctl_origin().is_none());
        sample.cursor_column = 2;
        sample.cursor_visible = false;
        assert!(sample.codex_ctl_origin().is_none());
        sample.cursor_visible = true;
        sample.bracketed_paste = false;
        assert!(sample.codex_ctl_origin().is_none());
    }

    #[test]
    fn ctl_codex_retry_requires_one_consumed_newline_from_origin() {
        let origin = ctl_origin("").codex_ctl_origin().unwrap();
        let task = "Reply with exactly copper lantern.";
        let pending = ctl_screen(&[task], false);
        assert!(
            !pending.codex_ctl_retry_draft(task, &origin),
            "first CR may still be queued"
        );
        let mut consumed = ctl_screen(&[task], true);
        assert!(consumed.codex_ctl_retry_draft(task, &origin));
        assert!(
            !consumed.codex_exact_retry_draft(task),
            "startup stays single-row strict"
        );
        ctl_set_row(&mut consumed, 50, "  draft shortcut hint");
        assert!(
            consumed.codex_ctl_retry_draft(task, &origin),
            "draft hint legitimately changes"
        );
        consumed.cursor_row += 1;
        assert!(
            !consumed.codex_ctl_retry_draft(task, &origin),
            "two added blank rows"
        );
        consumed.cursor_row -= 1;
        consumed.cursor_column = 3;
        assert!(!consumed.codex_ctl_retry_draft(task, &origin));
    }

    #[test]
    fn ctl_codex_retry_matches_complete_eighteen_row_task_from_origin() {
        let origin = ctl_origin("Ask Codex to do anything")
            .codex_ctl_origin()
            .unwrap();
        let words: Vec<_> = (0..18)
            .map(|i| format!("segment{i:02}abcdefghijklmnopqrstuv"))
            .collect();
        let pieces = words.iter().map(String::as_str).collect::<Vec<_>>();
        let task = words.join(" ");
        let retained = ctl_screen(&pieces, true);
        assert_eq!(retained.cursor_row, 47);
        assert!(retained.codex_ctl_retry_draft(&task, &origin));
        assert!(!ctl_screen(&pieces, false).codex_ctl_retry_draft(&task, &origin));
        assert!(!retained.codex_exact_retry_draft(&task));
    }

    #[test]
    fn ctl_codex_retry_matches_hyphen_wrap_in_long_prompt() {
        // Codex 0.164 wrapped the real ctl send at `force-` with no space.
        // The first Enter became a blank editor row, leaving this draft idle.
        let pieces = [
            "Kobol: B2 is briefed. Read the new 'Task B2 ... detailed 2026-10-28' section in D:/dev/muxel-desk/handoffs/2026-10-26-b8-status.md.",
            "Step 0 first: rebase feat/osc-7501-master and fix/status-user-text onto upstream master 442ac310 (origin = ProjectHax), re-gate, force-",
            "push-with-lease fork drafts #54/#56, then build combo/status-20261028 if B1 and 7501+A conflict. Then Step 1, B2 on rebased 7501+A.",
            "Cargo lock as always. Report after Step 0, then after B2.",
        ];
        let task = concat!(
            "Kobol: B2 is briefed. Read the new 'Task B2 ... detailed 2026-10-28' section in D:/dev/muxel-desk/handoffs/2026-10-26-b8-status.md. ",
            "Step 0 first: rebase feat/osc-7501-master and fix/status-user-text onto upstream master 442ac310 (origin = ProjectHax), re-gate, force-push-with-lease fork drafts #54/#56, then build combo/status-20261028 if B1 and 7501+A conflict. Then Step 1, B2 on rebased 7501+A. ",
            "Cargo lock as always. Report after Step 0, then after B2.",
        );
        assert_eq!(task.len(), 456);
        let origin = ctl_screen_with_width(&[""], false, 140)
            .codex_ctl_origin()
            .unwrap();
        let retained = ctl_screen_with_width(&pieces, true, 140);
        assert!(!ctl_screen_with_width(&pieces, false, 140).codex_ctl_retry_draft(task, &origin));
        assert!(retained.codex_ctl_retry_draft(task, &origin));
        assert!(
            !retained.codex_ctl_retry_draft(&task.replace("force-push", "force-ship"), &origin)
        );
        let narrow_origin = ctl_origin("").codex_ctl_origin().unwrap();
        assert!(
            !ctl_screen(&["Run force-", "push-with-lease now."], true)
                .codex_ctl_retry_draft("Run force-push-with-lease now.", &narrow_origin)
        );
    }

    #[test]
    fn ctl_codex_retry_refuses_content_completion_clips_and_hidden_continuation() {
        let origin = ctl_origin("").codex_ctl_origin().unwrap();
        for task in [
            "",
            " task",
            "task ",
            "two  words",
            "two\twords",
            "two\nwords",
            "café",
            "/review",
            "run /review",
            "read @file",
            "use $skill",
            "read (@file)",
            "use ($skill)",
            "mail a@b",
            "!echo task",
        ] {
            let sample = ctl_screen(&[task], true);
            assert!(!sample.codex_ctl_retry_draft(task, &origin), "{task:?}");
        }
        let task = "Reply with exactly copper lantern.";
        for pieces in [
            vec!["exactly copper lantern."],
            vec!["Reply with exactly", "copper lantern. changed"],
            vec![
                "Reply with exactly",
                "copper lantern.",
                "",
                "hidden continuation",
            ],
            vec!["Reply with exactly", "copper lantern. ↓"],
            vec!["↑ Reply with exactly", "copper lantern."],
        ] {
            assert!(
                !ctl_screen(&pieces, true).codex_ctl_retry_draft(task, &origin),
                "{pieces:?}"
            );
        }
        for (row, text) in [
            (0, "Enter to select"),
            (0, "↑/↓ to navigate"),
            (0, "Type to filter"),
            (45, "unexpected top padding"),
            (48, "hidden continuation"),
        ] {
            let mut sample = ctl_screen(&[task], true);
            ctl_set_row(&mut sample, row, text);
            assert!(
                !sample.codex_ctl_retry_draft(task, &origin),
                "{row}: {text}"
            );
        }
        let mut sample = ctl_screen(&["Reply with exactly", "copper lantern."], true);
        ctl_set_row(&mut sample, 46, "copper lantern.");
        assert!(
            !sample.codex_ctl_retry_draft(task, &origin),
            "lost continuation prefix"
        );
        let mut sample = ctl_screen(&[task], true);
        sample.cursor_visible = false;
        assert!(!sample.codex_ctl_retry_draft(task, &origin));
        sample.cursor_visible = true;
        sample.bracketed_paste = false;
        assert!(!sample.codex_ctl_retry_draft(task, &origin));
        let margin = "x".repeat(38);
        assert!(
            !ctl_screen(&[&margin], true).codex_ctl_retry_draft(&margin, &origin),
            "natural soft-wrap blank ambiguity"
        );
        // Sufficiently tall drafts touch the viewport edge and cannot prove a full prefix.
        let pieces = vec!["x"; 47];
        assert!(!ctl_screen(&pieces, true).codex_ctl_retry_draft(&pieces.join(" "), &origin));
    }

    #[test]
    fn ctl_codex_retry_refuses_changed_grid_and_model_footer() {
        let origin = ctl_origin("").codex_ctl_origin().unwrap();
        let mut sample = ctl_screen(&["task"], true);
        ctl_set_row(&mut sample, 49, "  GPT-6.1-Sol · other project");
        assert!(!sample.codex_ctl_retry_draft("task", &origin));
        let mut sample = ctl_screen(&["task"], true);
        sample.text = sample
            .text
            .lines()
            .map(|row| format!("{row} "))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !sample.codex_ctl_retry_draft("task", &origin),
            "changed column count"
        );
        let mut sample = ctl_screen(&["task"], true);
        sample.text.push_str(&format!("\n{}", " ".repeat(41)));
        assert!(
            !sample.codex_ctl_retry_draft("task", &origin),
            "changed row count"
        );
        let mut sample = ctl_screen(&["task"], true);
        ctl_set_row(&mut sample, 49, "");
        ctl_set_row(&mut sample, 50, "  GPT-6.1-Sol · scratch");
        assert!(
            !sample.codex_ctl_retry_draft("task", &origin),
            "moved footer"
        );
        let mut sample = ctl_origin("");
        sample.text.push(' ');
        assert!(sample.codex_ctl_origin().is_none(), "nonuniform grid");
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
    fn status_box_edges_ignore_a_rule_typed_into_the_draft() {
        let mut draft = screen(
            "Claude Code\n────────────\n❯ Check this:\n  ────────────\n  Esc to cancel\n────────────\nmode",
            4,
            15,
        );
        draft.cursor_visible = false;
        assert_eq!(
            draft.claude_status_context().as_deref(),
            Some("Claude Code\n────────────\n────────────\nmode")
        );
        // Neither status nor a hidden cursor grants startup input.
        assert!(!draft.claude_composer(false));
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
