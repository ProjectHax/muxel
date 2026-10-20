//! Guards for a startup turn. PTY silence is not an application's input-ready signal.

use std::{io, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Interrupted,
    NotReady,
    WriteFailed,
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
            self.stopped = Some(reason);
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
            self.stopped = Some(Stop::WriteFailed);
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

pub(crate) struct Screen {
    pub text: String,
    pub cursor_column: usize,
    pub cursor_row: i32,
    pub cursor_visible: bool,
    pub bracketed_paste: bool,
}

impl Screen {
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
        guard.submitted(123);
        drop(guard);
        assert_eq!(turn.state(), StartupTurnState::Submitted(123));

        for stop in [
            None,
            Some(Stop::Interrupted),
            Some(Stop::NotReady),
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
                Some(Stop::WriteFailed) => assert_eq!(
                    guard.write(ms(1), 0, || Err(io::Error::other("closed"))),
                    Err(Stop::WriteFailed)
                ),
            }
            drop(guard);
            assert_eq!(turn.state(), StartupTurnState::Cancelled);
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
