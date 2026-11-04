//! Worker-only ancestry reuse. Registry observations themselves are never cached.

use super::super::GrokActiveSession;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_AGE: Duration = Duration::from_secs(30);

struct Snapshot {
    roots: HashMap<(u32, Instant), Instant>,
    active: Vec<GrokActiveSession>,
    at: Instant,
    parents: Arc<HashMap<u32, u32>>,
}

#[derive(Default)]
struct Cache(Option<Snapshot>);

impl Cache {
    fn parents(
        &mut self,
        roots: &[(u32, Instant)],
        active: &[GrokActiveSession],
        now: Instant,
        snapshot: impl FnOnce() -> HashMap<u32, u32>,
    ) -> Arc<HashMap<u32, u32>> {
        if roots.is_empty() || active.is_empty() {
            self.0 = None;
            return Arc::default();
        }
        if let Some(cached) = &mut self.0
            && roots.iter().all(|root| cached.roots.contains_key(root))
            && cached.active == active
            && now.saturating_duration_since(cached.at) < MAX_AGE
        {
            for root in roots {
                cached.roots.entry(*root).and_modify(|seen| *seen = now);
            }
            return Arc::clone(&cached.parents);
        }
        let parents = Arc::new(snapshot());
        // Batches can contain different subsets of stable panes. Remember
        // recently observed roots, rather than treating omission from one poll
        // as evidence that a process changed. Inactive observations expire.
        let mut known_roots = self
            .0
            .take()
            .map_or_else(HashMap::new, |cached| cached.roots);
        known_roots.retain(|_, seen| now.saturating_duration_since(*seen) < MAX_AGE);
        known_roots.extend(roots.iter().map(|&root| (root, now)));
        // An empty ToolHelp read is a failure, not a reusable observation of an
        // empty machine. Retry it on the next registry poll.
        self.0 = (!parents.is_empty()).then(|| Snapshot {
            roots: known_roots,
            active: active.to_vec(),
            at: now,
            parents: Arc::clone(&parents),
        });
        parents
    }
}

#[cfg(windows)]
pub(super) fn process_snapshot(
    roots: &[(u32, Instant)],
    active: &[GrokActiveSession],
) -> Arc<HashMap<u32, u32>> {
    static CACHE: std::sync::Mutex<Cache> = std::sync::Mutex::new(Cache(None));
    CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .parents(
            roots,
            active,
            Instant::now(),
            super::super::process_parent_map,
        )
}

#[cfg(test)]
mod tests {
    use super::super::super::{
        GrokRegistryRead, active_grok_session_for_process, grok_switch_interval,
    };
    use super::*;
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn registration(id: &str, pid: u32, seconds: i64) -> GrokActiveSession {
        GrokActiveSession {
            session_id: id.into(),
            pid,
            opened_at: chrono::DateTime::from_timestamp(seconds, 0).unwrap(),
        }
    }

    #[test]
    fn stable_polls_reuse_one_population_until_the_freshness_boundary() {
        let mut cache = Cache::default();
        let calls = Cell::new(0);
        let now = Instant::now();
        let roots = [(10, now)];
        let active = [registration("a", 12, 1)];
        let load = || {
            calls.set(calls.get() + 1);
            HashMap::from([(12, 11), (11, 10)])
        };
        cache.parents(&roots, &active, now, load);
        for seconds in 1..30 {
            let parents = cache.parents(&roots, &active, now + Duration::from_secs(seconds), load);
            assert_eq!(
                active_grok_session_for_process(10, &active, &parents)
                    .unwrap()
                    .session_id,
                "a"
            );
        }
        assert_eq!(
            calls.get(),
            1,
            "stable polls must not enumerate the process population again"
        );
        let refreshed = cache.parents(&roots, &active, now + MAX_AGE, || {
            calls.set(calls.get() + 1);
            HashMap::from([(11, 10)])
        });
        assert_eq!(calls.get(), 2);
        assert!(
            active_grok_session_for_process(10, &active, &refreshed).is_none(),
            "stale process ancestry must age out"
        );
    }

    #[test]
    fn registry_and_launch_changes_refresh_immediately_and_preserve_switch_evidence() {
        let mut cache = Cache::default();
        let now = Instant::now();
        let calls = Cell::new(0);
        let load = || {
            calls.set(calls.get() + 1);
            HashMap::from([(12, 10), (13, 10)])
        };
        let roots = [(10, now)];
        let old = registration("a", 12, 1);
        let same_pid_new = registration("b", 12, 2);
        let newer = registration("b", 12, 3);
        let new_pid = registration("b", 13, 3);
        for (index, rows) in [
            vec![old.clone()],
            vec![old.clone(), same_pid_new],
            vec![old.clone(), newer],
            vec![old, new_pid],
        ]
        .into_iter()
        .enumerate()
        {
            let parents = cache.parents(&roots, &rows, now, load);
            let selected = active_grok_session_for_process(10, &rows, &parents).unwrap();
            assert_eq!(selected, rows.last().unwrap());
            let interval = grok_switch_interval(
                selected,
                None,
                GrokRegistryRead {
                    started: UNIX_EPOCH,
                    finished: SystemTime::now(),
                },
            );
            assert_eq!(interval.0, SystemTime::from(selected.opened_at));
            assert_eq!(
                calls.get(),
                index + 1,
                "registry changes must bypass the age limit"
            );
        }
        let active = [registration("a", 12, 1), registration("b", 13, 3)];
        for roots in [[(10, now + Duration::from_secs(1))], [(11, now)]] {
            cache.parents(&roots, &active, now, load);
        }
        assert_eq!(
            calls.get(),
            6,
            "launch or child PID changes must bypass the age limit"
        );
    }

    #[test]
    fn empty_inputs_skip_enumeration_and_failed_snapshots_retry() {
        let mut cache = Cache::default();
        let now = Instant::now();
        let roots = [(10, now)];
        let active = [registration("a", 12, 1)];
        for (roots, active) in [(&roots[..], &[][..]), (&[][..], &active[..])] {
            assert!(
                cache
                    .parents(roots, active, now, || panic!("nothing to match"))
                    .is_empty()
            );
        }
        assert!(cache.parents(&roots, &active, now, HashMap::new).is_empty());
        let recovered = cache.parents(&roots, &active, now, || HashMap::from([(12, 10)]));
        assert!(active_grok_session_for_process(10, &active, &recovered).is_some());
        cache.parents(&roots, &[], now, || panic!("empty registry"));
        let replaced = cache.parents(&roots, &active, now, || HashMap::from([(12, 11)]));
        assert!(
            active_grok_session_for_process(10, &active, &replaced).is_none(),
            "a registry gap must discard old ancestry before matching again"
        );
    }

    #[test]
    fn staggered_stable_panes_do_not_turn_batch_subsets_into_process_changes() {
        let mut cache = Cache::default();
        let now = Instant::now();
        let calls = Cell::new(0);
        let active = [registration("a", 21, 1), registration("b", 22, 1)];
        for seconds in 0..30 {
            let root = if seconds % 2 == 0 { 10 } else { 11 };
            let parents = cache.parents(
                &[(root, now)],
                &active,
                now + Duration::from_secs(seconds),
                || {
                    calls.set(calls.get() + 1);
                    HashMap::from([(21, 10), (22, 11)])
                },
            );
            assert!(active_grok_session_for_process(root, &active, &parents).is_some());
        }
        assert_eq!(
            calls.get(),
            2,
            "each newly observed root needs one snapshot, not every alternating batch"
        );
        cache.parents(
            &[(11, now), (10, now)],
            &active,
            now + Duration::from_secs(29),
            || panic!("a combined batch of known roots is still the same population"),
        );
    }

    #[test]
    fn root_observations_expire_during_process_churn() {
        let mut cache = Cache::default();
        let start = Instant::now();
        let active = [registration("a", 12, 1)];
        for second in 0..120 {
            let now = start + Duration::from_secs(second);
            cache.parents(&[(100 + second as u32, now)], &active, now, || {
                HashMap::from([(12, 10)])
            });
            assert!(
                cache.0.as_ref().is_none_or(|cache| cache.roots.len() <= 30),
                "inactive root history must not grow with the process lifetime"
            );
        }
    }
}
