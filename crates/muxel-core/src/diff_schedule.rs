//! When each diff pane runs `git diff`.
//!
//! A hidden pane never refreshes; a change while it's hidden marks it dirty.
//! A visible pane the file watcher can't cover refreshes on a fixed cadence. A
//! watched one refreshes after its files change and settle, and on a slow
//! backstop for events the watcher loses.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// A visible pane without a watch refreshes this long after its last refresh.
pub const POLL: Duration = Duration::from_millis(1500);
/// A watched pane refreshes once its files have been quiet this long…
pub const QUIET: Duration = Duration::from_millis(300);
/// …or this long after the first change of a burst that keeps going…
pub const MAX_WAIT: Duration = Duration::from_secs(2);
/// …but no sooner than this after its last refresh.
pub const MIN_GAP: Duration = Duration::from_secs(1);
/// A watched, visible pane refreshes at least this often anyway: Windows
/// drops a watcher overflow silently.
pub const BACKSTOP: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct Pane {
    visible: bool,
    watched: bool,
    /// Changed since its last refresh started.
    dirty: bool,
    /// Refresh at the first chance: just shown while it may be stale, or
    /// asked for (the refresh button, reopening the pane).
    urgent: bool,
    in_flight: bool,
    /// When its last refresh landed; `None` before the first.
    landed: Option<Instant>,
    /// The first and latest change of the current burst.
    burst: Option<(Instant, Instant)>,
}

impl Pane {
    /// When this pane is next due, if anything would make it due.
    fn due_at(&self, now: Instant) -> Option<Instant> {
        if !self.visible || self.in_flight {
            return None;
        }
        let Some(landed) = self.landed else {
            return Some(now);
        };
        if self.urgent {
            return Some(now);
        }
        if !self.watched {
            return Some(landed + POLL);
        }
        let backstop = landed + BACKSTOP;
        if !self.dirty {
            return Some(backstop);
        }
        // A change with no burst on record is a rescan: treat it as one change
        // the moment the pane went dirty, which settles at once.
        let settled = self
            .burst
            .map_or(now, |(first, last)| (last + QUIET).min(first + MAX_WAIT));
        Some(settled.max(landed + MIN_GAP).min(backstop))
    }
}

/// Refresh state for every open diff pane, keyed by pane.
#[derive(Debug, Default)]
pub struct DiffSchedule {
    panes: HashMap<Uuid, Pane>,
}

impl DiffSchedule {
    fn pane(&mut self, pane: Uuid) -> &mut Pane {
        self.panes.entry(pane).or_default()
    }

    /// Forget a closed pane.
    pub fn remove(&mut self, pane: Uuid) {
        self.panes.remove(&pane);
    }

    /// The panes on record, for reconciling against the open ones.
    pub fn panes(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.panes.keys().copied()
    }

    /// Whether `pane` is on screen. Coming into view makes it due at once if
    /// it may be stale: dirty, never refreshed, or unwatched (so nothing told
    /// it about changes). A clean watched pane shows what it has, unless its
    /// backstop came due while it was hidden.
    pub fn set_visible(&mut self, pane: Uuid, visible: bool) {
        let state = self.pane(pane);
        if visible && !state.visible && (state.dirty || !state.watched || state.landed.is_none()) {
            state.urgent = true;
        }
        state.visible = visible;
    }

    /// Whether the file watcher covers `pane`. Losing the watch makes it due
    /// at once, since changes may have gone unreported.
    pub fn set_watched(&mut self, pane: Uuid, watched: bool) {
        let state = self.pane(pane);
        if state.watched && !watched {
            state.urgent = true;
        }
        state.watched = watched;
    }

    /// The watcher saw `pane`'s files change, or may have lost events (a
    /// rescan, with `at` `None`).
    pub fn changed(&mut self, pane: Uuid, at: Option<Instant>) {
        let state = self.pane(pane);
        state.dirty = true;
        if let Some(at) = at {
            state.burst = Some(state.burst.map_or((at, at), |(first, _)| (first, at)));
        }
    }

    /// Refresh `pane` at the first chance.
    pub fn request(&mut self, pane: Uuid) {
        self.pane(pane).urgent = true;
    }

    /// The panes due at `now`, now in flight. A change that arrives while a
    /// refresh runs marks the pane dirty again, so it's due once that lands.
    pub fn take_due(&mut self, now: Instant) -> Vec<Uuid> {
        let mut due = Vec::new();
        for (id, pane) in &mut self.panes {
            if pane.due_at(now).is_some_and(|at| at <= now) {
                pane.in_flight = true;
                pane.dirty = false;
                pane.urgent = false;
                pane.burst = None;
                due.push(*id);
            }
        }
        due
    }

    /// `pane`'s refresh finished at `now`, applied or not.
    pub fn landed(&mut self, pane: Uuid, now: Instant) {
        if let Some(state) = self.panes.get_mut(&pane) {
            state.in_flight = false;
            state.landed = Some(now);
        }
    }

    /// The earliest moment any pane is due, for the one timer that waits for
    /// it. `None` while nothing could come due without news.
    pub fn next_due(&self, now: Instant) -> Option<Instant> {
        self.panes
            .values()
            .filter_map(|pane| pane.due_at(now))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::{BACKSTOP, DiffSchedule, MAX_WAIT, MIN_GAP, POLL, QUIET};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    const MS: Duration = Duration::from_millis(1);

    /// A visible, watched pane that refreshed at `t0`.
    fn watched_pane() -> (DiffSchedule, Uuid, Instant) {
        let mut schedule = DiffSchedule::default();
        let pane = Uuid::from_u128(1);
        let t0 = Instant::now();
        schedule.set_watched(pane, true);
        schedule.set_visible(pane, true);
        assert_eq!(schedule.take_due(t0), vec![pane], "first refresh");
        schedule.landed(pane, t0);
        (schedule, pane, t0)
    }

    #[test]
    fn a_change_during_a_refresh_runs_another_when_it_lands() {
        let (mut schedule, pane, t0) = watched_pane();
        let t1 = t0 + 5 * MIN_GAP;
        schedule.changed(pane, Some(t1));
        let t2 = t1 + QUIET;
        assert_eq!(schedule.take_due(t2), vec![pane]);
        // The agent writes again while that refresh runs.
        schedule.changed(pane, Some(t2 + 10 * MS));
        assert!(schedule.take_due(t2 + 20 * MS).is_empty(), "one at a time");
        let t3 = t2 + 50 * MS;
        schedule.landed(pane, t3);
        let next = schedule.next_due(t3).unwrap();
        assert!(next < t3 + BACKSTOP, "not left for the backstop");
        assert_eq!(next, t3 + MIN_GAP);
        assert_eq!(schedule.take_due(next), vec![pane]);
    }

    #[test]
    fn a_burst_refreshes_once_after_it_settles() {
        let (mut schedule, pane, t0) = watched_pane();
        let start = t0 + 5 * MIN_GAP;
        let mut refreshes = 0;
        // A write every 100 ms for 900 ms, then quiet.
        let mut now = start;
        while now <= start + 900 * MS {
            schedule.changed(pane, Some(now));
            refreshes += schedule.take_due(now).len();
            now += 50 * MS;
            refreshes += schedule.take_due(now).len();
            now += 50 * MS;
        }
        assert_eq!(refreshes, 0, "nothing runs while it keeps changing");
        let last = start + 900 * MS;
        assert_eq!(schedule.next_due(now), Some(last + QUIET));
        assert_eq!(schedule.take_due(last + QUIET), vec![pane]);
        schedule.landed(pane, last + QUIET);
        assert_eq!(
            schedule.next_due(last + QUIET),
            Some(last + QUIET + BACKSTOP)
        );

        // A burst that never stops still refreshes after MAX_WAIT.
        let start = last + QUIET + 5 * MIN_GAP;
        let mut now = start;
        let mut ran = None;
        while ran.is_none() && now < start + 3 * MAX_WAIT {
            schedule.changed(pane, Some(now));
            if !schedule.take_due(now).is_empty() {
                ran = Some(now);
            }
            now += 100 * MS;
        }
        assert_eq!(ran, Some(start + MAX_WAIT));
    }

    #[test]
    fn hidden_panes_wait_and_shown_ones_catch_up() {
        let (mut schedule, pane, t0) = watched_pane();
        schedule.set_visible(pane, false);
        // Hidden for a while, though less than the backstop, which would
        // make it due on its own.
        let t1 = t0 + 5 * MIN_GAP;
        assert!(t1 < t0 + BACKSTOP);
        // A write while hidden, just before it's shown again.
        schedule.changed(pane, Some(t1 - MS));
        assert_eq!(schedule.next_due(t1 + BACKSTOP), None, "hidden: never due");
        // Shown: at once, not after the quiet period.
        schedule.set_visible(pane, true);
        assert_eq!(schedule.take_due(t1), vec![pane]);
        schedule.landed(pane, t1);
        // Hidden and shown again clean: it shows what it has.
        schedule.set_visible(pane, false);
        schedule.set_visible(pane, true);
        assert!(schedule.take_due(t1 + MIN_GAP).is_empty());
    }

    #[test]
    fn an_unwatched_pane_polls_while_visible() {
        let mut schedule = DiffSchedule::default();
        let pane = Uuid::from_u128(2);
        let t0 = Instant::now();
        schedule.set_visible(pane, true);
        assert_eq!(schedule.take_due(t0), vec![pane]);
        schedule.landed(pane, t0);
        assert_eq!(schedule.next_due(t0), Some(t0 + POLL));
        assert_eq!(schedule.take_due(t0 + POLL), vec![pane]);
        schedule.landed(pane, t0 + POLL);
        // A watch that's lost makes it due at once and then poll.
        schedule.set_watched(pane, true);
        assert_eq!(schedule.next_due(t0 + POLL), Some(t0 + POLL + BACKSTOP));
        schedule.set_watched(pane, false);
        assert_eq!(schedule.take_due(t0 + POLL + MS), vec![pane]);
    }
}
