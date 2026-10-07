//! Remote reconnects, grouped by host for the notification feed.
//!
//! When a laptop wakes, every remote pane's SSH relay has died, and each pane
//! drops and reattaches on its own schedule. Reported pane by pane, that is two
//! feed entries for every running agent. A host's panes share one fate, so they
//! share one entry: it counts the panes still reattaching and turns into
//! "reconnected" once they're all back.
//!
//! Pure and I/O-free: the app reports drops, reattaches and closed panes, and
//! renders the [`OutageUpdate`] each one returns.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// A drop this soon after a host's outage ended continues that outage instead of
/// starting a new one. After a wake the relays don't all notice they're dead at
/// once (each finds out on its own keepalive probe), so the first panes can be
/// back before the last ones have even dropped.
pub const OUTAGE_MERGE_WINDOW: Duration = Duration::from_secs(120);

/// Every host's current (or most recent) outage.
#[derive(Default)]
pub struct HostOutages {
    hosts: HashMap<Uuid, Outage>,
}

struct Outage {
    /// The feed entry this host reports through, reused across its outages so the
    /// feed holds one line per host.
    entry: Uuid,
    /// Panes still reattaching.
    waiting: HashSet<Uuid>,
    /// Panes that dropped during this outage and have since reattached.
    back: HashSet<Uuid>,
    /// When the last waiting pane came back; `None` while any are waiting.
    ended: Option<Instant>,
}

/// What a host's feed entry should now say.
#[derive(Debug, PartialEq, Eq)]
pub struct OutageUpdate {
    pub host: Uuid,
    /// The entry to show it in: the same one for every update about this host.
    pub entry: Uuid,
    /// This update starts a new outage. That is news even if the user dismissed
    /// the host's previous entry; any other update to a dismissed entry isn't.
    pub fresh: bool,
    pub state: OutageState,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OutageState {
    /// `waiting` panes are still reattaching; `back` already have.
    Reconnecting { waiting: usize, back: usize },
    /// All `panes` that dropped have reattached.
    Reconnected { panes: usize },
    /// Every pane that dropped was closed before it came back: nothing is left to
    /// report, so the entry goes.
    Cleared,
}

impl HostOutages {
    /// `pane`, on `host`, lost its connection.
    pub fn dropped(&mut self, host: Uuid, pane: Uuid, now: Instant) -> OutageUpdate {
        let outage = self.hosts.entry(host).or_insert_with(|| Outage {
            entry: Uuid::new_v4(),
            waiting: HashSet::new(),
            back: HashSet::new(),
            ended: None,
        });
        let fresh = match outage.ended.take() {
            Some(ended) => {
                let fresh = now.saturating_duration_since(ended) >= OUTAGE_MERGE_WINDOW;
                if fresh {
                    outage.back.clear();
                }
                fresh
            }
            None => outage.waiting.is_empty(),
        };
        outage.back.remove(&pane);
        outage.waiting.insert(pane);
        OutageUpdate {
            host,
            entry: outage.entry,
            fresh,
            state: outage.state(),
        }
    }

    /// `pane` reattached. `None` when it wasn't part of a host outage.
    pub fn reconnected(&mut self, pane: Uuid, now: Instant) -> Option<OutageUpdate> {
        let (&host, outage) = self
            .hosts
            .iter_mut()
            .find(|(_, outage)| outage.waiting.contains(&pane))?;
        outage.waiting.remove(&pane);
        outage.back.insert(pane);
        if outage.waiting.is_empty() {
            outage.ended = Some(now);
        }
        Some(OutageUpdate {
            host,
            entry: outage.entry,
            fresh: false,
            state: outage.state(),
        })
    }

    /// `pane` was closed. `None` when that changes nothing on screen: it wasn't
    /// waiting on a host.
    pub fn forget(&mut self, pane: Uuid, now: Instant) -> Option<OutageUpdate> {
        for outage in self.hosts.values_mut() {
            outage.back.remove(&pane);
        }
        let (&host, outage) = self
            .hosts
            .iter_mut()
            .find(|(_, outage)| outage.waiting.contains(&pane))?;
        outage.waiting.remove(&pane);
        let update = OutageUpdate {
            host,
            entry: outage.entry,
            fresh: false,
            state: outage.state(),
        };
        if update.state == OutageState::Cleared {
            self.hosts.remove(&host);
        } else if outage.waiting.is_empty() {
            outage.ended = Some(now);
        }
        Some(update)
    }
}

impl Outage {
    fn state(&self) -> OutageState {
        match (self.waiting.len(), self.back.len()) {
            (0, 0) => OutageState::Cleared,
            (0, panes) => OutageState::Reconnected { panes },
            (waiting, back) => OutageState::Reconnecting { waiting, back },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids<const N: usize>() -> [Uuid; N] {
        std::array::from_fn(|_| Uuid::new_v4())
    }

    /// A wake that drops twelve panes is one entry counting up, then one
    /// "reconnected" — not twenty-four lines.
    #[test]
    fn a_hosts_panes_share_one_entry() {
        let now = Instant::now();
        let mut outages = HostOutages::default();
        let host = Uuid::new_v4();
        let [a, b, c] = ids();

        let first = outages.dropped(host, a, now);
        assert!(first.fresh);
        assert_eq!(
            first.state,
            OutageState::Reconnecting {
                waiting: 1,
                back: 0
            }
        );
        for (pane, waiting) in [(b, 2), (c, 3)] {
            let update = outages.dropped(host, pane, now);
            assert_eq!(update.entry, first.entry);
            assert!(!update.fresh);
            assert_eq!(update.state, OutageState::Reconnecting { waiting, back: 0 });
        }

        let update = outages.reconnected(a, now).unwrap();
        assert_eq!(update.entry, first.entry);
        assert_eq!(
            update.state,
            OutageState::Reconnecting {
                waiting: 2,
                back: 1
            }
        );
        outages.reconnected(b, now);
        let done = outages.reconnected(c, now).unwrap();
        assert_eq!(done.state, OutageState::Reconnected { panes: 3 });
        assert!(!done.fresh);
    }

    #[test]
    fn each_host_has_its_own_entry() {
        let now = Instant::now();
        let mut outages = HostOutages::default();
        let [dev, cloud, a, b] = ids();
        let on_dev = outages.dropped(dev, a, now);
        let on_cloud = outages.dropped(cloud, b, now);
        assert_ne!(on_dev.entry, on_cloud.entry);
        assert!(on_cloud.fresh);
        assert_eq!(
            outages.reconnected(b, now).unwrap().state,
            OutageState::Reconnected { panes: 1 }
        );
        assert_eq!(
            outages.dropped(dev, b, now).state,
            OutageState::Reconnecting {
                waiting: 2,
                back: 0
            }
        );
    }

    /// The first panes back can finish before the last relay notices it's dead.
    /// That late drop continues the same outage, keeping the count.
    #[test]
    fn a_late_drop_continues_the_outage() {
        let now = Instant::now();
        let mut outages = HostOutages::default();
        let host = Uuid::new_v4();
        let [a, b] = ids();
        let first = outages.dropped(host, a, now);
        outages.reconnected(a, now);

        let late = outages.dropped(host, b, now + Duration::from_secs(30));
        assert_eq!(late.entry, first.entry);
        assert!(!late.fresh);
        assert_eq!(
            late.state,
            OutageState::Reconnecting {
                waiting: 1,
                back: 1
            }
        );
        assert_eq!(
            outages.reconnected(b, now).unwrap().state,
            OutageState::Reconnected { panes: 2 }
        );
    }

    /// A drop long after the last outage is a new one: news again, counted from
    /// zero, in the same entry so the feed keeps one line per host.
    #[test]
    fn a_later_drop_starts_a_new_outage_in_the_same_entry() {
        let now = Instant::now();
        let mut outages = HostOutages::default();
        let host = Uuid::new_v4();
        let [a, b] = ids();
        let first = outages.dropped(host, a, now);
        outages.dropped(host, b, now);
        outages.reconnected(a, now);
        outages.reconnected(b, now);

        let next = outages.dropped(host, a, now + OUTAGE_MERGE_WINDOW);
        assert_eq!(next.entry, first.entry);
        assert!(next.fresh);
        assert_eq!(
            next.state,
            OutageState::Reconnecting {
                waiting: 1,
                back: 0
            }
        );
    }

    /// A pane that comes back and drops again is counted once, not twice.
    #[test]
    fn a_flapping_pane_counts_once() {
        let now = Instant::now();
        let mut outages = HostOutages::default();
        let host = Uuid::new_v4();
        let [a, b] = ids();
        outages.dropped(host, a, now);
        outages.dropped(host, b, now);
        outages.reconnected(a, now);
        assert_eq!(
            outages.dropped(host, a, now).state,
            OutageState::Reconnecting {
                waiting: 2,
                back: 0
            }
        );
        outages.reconnected(a, now);
        assert_eq!(
            outages.reconnected(b, now).unwrap().state,
            OutageState::Reconnected { panes: 2 }
        );
    }

    #[test]
    fn closing_waiting_panes_settles_or_clears_the_entry() {
        let now = Instant::now();
        let mut outages = HostOutages::default();
        let host = Uuid::new_v4();
        let [a, b, c] = ids();
        outages.dropped(host, a, now);
        outages.dropped(host, b, now);
        outages.reconnected(a, now);
        // The last pane still waiting is closed: the one that came back is the story.
        assert_eq!(
            outages.forget(b, now).unwrap().state,
            OutageState::Reconnected { panes: 1 }
        );
        // Closing a pane that isn't waiting changes nothing on screen.
        assert_eq!(outages.forget(a, now), None);

        // Nobody came back and the only dropped pane closed: drop the entry.
        let gone = Uuid::new_v4();
        let first = outages.dropped(gone, c, now);
        let cleared = outages.forget(c, now).unwrap();
        assert_eq!(cleared.entry, first.entry);
        assert_eq!(cleared.state, OutageState::Cleared);
        assert!(outages.dropped(gone, c, now).fresh);
    }

    #[test]
    fn panes_outside_any_outage_are_ignored() {
        let mut outages = HostOutages::default();
        let pane = Uuid::new_v4();
        assert_eq!(outages.reconnected(pane, Instant::now()), None);
        assert_eq!(outages.forget(pane, Instant::now()), None);
    }
}
