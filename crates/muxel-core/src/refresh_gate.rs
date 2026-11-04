//! Change-gated polling. The status tick runs every second, but the probes it
//! feeds (a `git` subprocess per project, a `$PATH` scan per agent) are far too
//! costly to repeat that often — on Windows every `git` run also starts a
//! console host, and both are scanned by antivirus. A [`RefreshGate`] re-runs a
//! probe only when a cheap stamp of its inputs moved, or when a backstop
//! interval has passed (for inputs the stamp can't see, e.g. git config).

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// When each keyed probe last ran, and against which input stamp.
#[derive(Debug, Clone)]
pub struct RefreshGate<K, S> {
    backstop: Duration,
    seen: HashMap<K, (S, Instant)>,
}

impl<K: Eq + Hash, S: PartialEq> RefreshGate<K, S> {
    /// A gate that re-runs an unchanged probe after `backstop`.
    pub fn new(backstop: Duration) -> Self {
        Self {
            backstop,
            seen: HashMap::new(),
        }
    }

    /// Whether `key`'s probe needs to run: it never has, its stamp moved since,
    /// or the backstop has passed.
    pub fn due(&self, key: &K, stamp: &S, now: Instant) -> bool {
        self.seen.get(key).is_none_or(|(seen, at)| {
            seen != stamp || now.saturating_duration_since(*at) >= self.backstop
        })
    }

    /// `key`'s probe just ran against `stamp`.
    pub fn record(&mut self, key: K, stamp: S, now: Instant) {
        self.seen.insert(key, (stamp, now));
    }

    /// Forget keys `keep` rejects (e.g. removed projects).
    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.seen.retain(|key, _| keep(key));
    }
}

#[cfg(test)]
mod tests {
    use super::RefreshGate;
    use std::time::{Duration, Instant};

    const BACKSTOP: Duration = Duration::from_secs(60);

    #[test]
    fn runs_first_then_only_on_a_new_stamp_or_after_the_backstop() {
        let t0 = Instant::now();
        let mut gate = RefreshGate::new(BACKSTOP);
        assert!(gate.due(&1, &"a", t0), "never ran");
        gate.record(1, "a", t0);
        assert!(!gate.due(&1, &"a", t0 + Duration::from_secs(1)));
        assert!(!gate.due(&1, &"a", t0 + BACKSTOP - Duration::from_millis(1)));
        assert!(
            gate.due(&1, &"b", t0 + Duration::from_secs(1)),
            "stamp moved"
        );
        assert!(gate.due(&1, &"a", t0 + BACKSTOP), "backstop");
        assert!(gate.due(&2, &"a", t0), "other keys are independent");
    }

    #[test]
    fn recording_restarts_the_backstop() {
        let t0 = Instant::now();
        let mut gate = RefreshGate::new(BACKSTOP);
        gate.record((), 7, t0);
        gate.record((), 7, t0 + BACKSTOP);
        assert!(!gate.due(&(), &7, t0 + BACKSTOP + Duration::from_secs(1)));
    }

    #[test]
    fn a_clock_behind_the_record_is_not_due() {
        let t0 = Instant::now() + Duration::from_secs(10);
        let mut gate = RefreshGate::new(BACKSTOP);
        gate.record((), 0, t0);
        assert!(!gate.due(&(), &0, t0 - Duration::from_secs(5)));
    }

    #[test]
    fn retain_forgets_dropped_keys() {
        let t0 = Instant::now();
        let mut gate = RefreshGate::new(BACKSTOP);
        gate.record(1, 0, t0);
        gate.record(2, 0, t0);
        gate.retain(|k| *k == 2);
        assert!(gate.due(&1, &0, t0));
        assert!(!gate.due(&2, &0, t0));
    }
}
