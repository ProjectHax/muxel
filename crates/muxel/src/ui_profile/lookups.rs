//! Worker-only discovery noise filtering; callers still report every lookup.

use super::{SessionLookupKind, SessionLookupOutcome, SessionLookupRecord};
use muxel_core::session_discovery::SessionProvider;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use uuid::Uuid;

const SLOW_US: u64 = 5_000;
const MAX_PANES: usize = 4096;
const FORGET_AFTER: Duration = Duration::from_secs(3600);

#[derive(Default)]
struct Totals {
    calls: u64,
    found: u64,
    slow: u64,
    emitted: u64,
    elapsed_us: u64,
    max_us: u64,
}

#[derive(Default)]
pub(super) struct Lookups {
    panes: HashMap<Uuid, (Option<SessionProvider>, SessionLookupOutcome, Instant)>,
    order: VecDeque<Uuid>,
    totals: [Totals; 5],
}

impl Lookups {
    pub fn observe(&mut self, record: SessionLookupRecord, now: Instant) -> bool {
        if matches!(record.kind, SessionLookupKind::Launch) {
            return true;
        }
        let changed = self
            .panes
            .get(&record.pane)
            .is_some_and(|&(provider, outcome, _)| {
                provider != record.provider || outcome != record.outcome
            });
        if !self.panes.contains_key(&record.pane) {
            if self.panes.len() == MAX_PANES
                && let Some(oldest) = self.order.pop_front()
            {
                self.panes.remove(&oldest);
            }
            self.order.push_back(record.pane);
        }
        self.panes
            .insert(record.pane, (record.provider, record.outcome, now));
        let found = record.outcome == SessionLookupOutcome::Found;
        let slow = record.elapsed_us >= SLOW_US;
        let emit = found || changed || slow;
        let slot = match record.provider {
            Some(SessionProvider::Codex) => 0,
            Some(SessionProvider::Claude) => 1,
            Some(SessionProvider::Grok) => 2,
            Some(SessionProvider::Muse) => 3,
            None => 4,
        };
        let totals = &mut self.totals[slot];
        totals.calls += 1;
        totals.found += u64::from(found);
        totals.slow += u64::from(slow);
        totals.emitted += u64::from(emit);
        totals.elapsed_us += record.elapsed_us;
        totals.max_us = totals.max_us.max(record.elapsed_us);
        emit
    }

    pub fn summaries(&mut self, now: Instant) -> Vec<String> {
        self.panes
            .retain(|_, (_, _, at)| now.saturating_duration_since(*at) < FORGET_AFTER);
        self.order.retain(|pane| self.panes.contains_key(pane));
        ["codex", "claude", "grok", "muse", "other"].into_iter()
            .zip(std::mem::take(&mut self.totals))
            .filter(|(_, totals)| totals.calls > 0)
            .map(|(provider, t)| format!(
                "ui-prof[session-lookups v1] provider={provider} calls={} found={} slow={} emitted={} suppressed={} elapsed_us={} max_us={}",
                t.calls, t.found, t.slow, t.emitted, t.calls - t.emitted, t.elapsed_us, t.max_us,
            )).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(outcome: SessionLookupOutcome, elapsed_us: u64) -> SessionLookupRecord {
        SessionLookupRecord {
            kind: SessionLookupKind::Discovery,
            provider: Some(SessionProvider::Claude),
            pane: Uuid::nil(),
            outcome,
            elapsed_us,
        }
    }

    #[test]
    fn steady_misses_are_counted_without_lines_but_transitions_hits_and_slow_calls_log() {
        let mut lookups = Lookups::default();
        let now = Instant::now();
        for _ in 0..100 {
            assert!(!lookups.observe(record(SessionLookupOutcome::Unresolved, 41), now));
        }
        assert!(lookups.observe(record(SessionLookupOutcome::Found, 80), now));
        assert!(lookups.observe(record(SessionLookupOutcome::Found, 90), now));
        assert!(lookups.observe(record(SessionLookupOutcome::Unresolved, 40), now));
        assert!(!lookups.observe(record(SessionLookupOutcome::Unresolved, 41), now));
        assert!(lookups.observe(record(SessionLookupOutcome::Unresolved, 5_000), now));
        let summaries = lookups.summaries(now);
        assert_eq!(
            summaries,
            [
                "ui-prof[session-lookups v1] provider=claude calls=105 found=2 slow=1 emitted=4 suppressed=101 elapsed_us=9351 max_us=5000"
            ]
        );
        assert!(lookups.summaries(now).is_empty());
    }

    #[test]
    fn launch_lookups_always_log_without_changing_discovery_state() {
        let mut lookups = Lookups::default();
        let now = Instant::now();
        let mut launch = record(SessionLookupOutcome::Unresolved, 1);
        launch.kind = SessionLookupKind::Launch;
        assert!(lookups.observe(launch, now));
        assert!(lookups.observe(launch, now));
        assert!(!lookups.observe(record(SessionLookupOutcome::Unresolved, 1), now));
    }

    #[test]
    fn outcome_memory_is_pane_and_provider_scoped_bounded_and_expires() {
        let mut lookups = Lookups::default();
        let now = Instant::now();
        assert!(lookups.observe(record(SessionLookupOutcome::Found, 1), now));
        let mut other = record(SessionLookupOutcome::Unresolved, 1);
        other.pane = Uuid::max();
        assert!(!lookups.observe(other, now));
        other.provider = Some(SessionProvider::Grok);
        assert!(lookups.observe(other, now));
        for n in 1..=MAX_PANES {
            other.pane = Uuid::from_u128(n as u128);
            lookups.observe(other, now);
        }
        assert_eq!(lookups.panes.len(), MAX_PANES);
        assert_eq!(lookups.order.len(), MAX_PANES);
        lookups.summaries(now + FORGET_AFTER);
        assert!(lookups.panes.is_empty());
        assert!(lookups.order.is_empty());
    }
}
