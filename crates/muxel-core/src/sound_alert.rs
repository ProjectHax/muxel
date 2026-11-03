//! Sound alerts: a chime when an agent finishes a turn or is blocked on you.
//!
//! [`AlertGate`] decides when a chime actually plays. The status tick arms it
//! when an agent pane nobody is watching enters Done or Blocked, and asks it
//! once per tick what is due. Its rules are what keep an agent that flips
//! between working and done from ringing over and over:
//!
//! - **Settle** — an alert plays only once its state has held for [`SETTLE`];
//!   a Done that flickers back to Working first never rings.
//! - **Once per pane until you look** — after a pane rings for Done (or for
//!   Blocked), it doesn't ring for that again until you've focused it, or
//!   [`PANE_COOLDOWN`] has passed.
//! - **One at a time** — alerts due together play as one chime (Blocked wins),
//!   and chimes are at least [`MIN_GAP`] apart; an alert held back by the gap
//!   plays after it rather than being lost.

use crate::Settings;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a Done or Blocked must hold before it rings.
pub const SETTLE: Duration = Duration::from_millis(900);
/// How long a pane that rang stays quiet for the same alert, unless you look.
pub const PANE_COOLDOWN: Duration = Duration::from_secs(5 * 60);
/// The least time between two chimes, whichever panes they're for.
pub const MIN_GAP: Duration = Duration::from_secs(2);
/// Alert volume for a new install, in percent.
pub const DEFAULT_VOLUME: u8 = 80;

/// Which alert to play.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AlertSound {
    /// An agent finished its turn.
    Done,
    /// An agent is waiting on you (a permission prompt, a question).
    Blocked,
}

impl AlertSound {
    /// Whether the user wants this alert to play.
    pub fn enabled(self, settings: &Settings) -> bool {
        match self {
            Self::Done => settings.sound_on_done,
            Self::Blocked => settings.sound_on_blocked,
        }
    }

    /// The user's own sound file for this alert, if they chose one.
    pub fn file(self, settings: &Settings) -> Option<&Path> {
        match self {
            Self::Done => settings.sound_done_file.as_deref(),
            Self::Blocked => settings.sound_blocked_file.as_deref(),
        }
    }
}

/// Linear gain for a volume percentage. Squared, so the slider's steps sound
/// roughly even: half way is about a quarter of the amplitude.
pub fn volume_gain(percent: u8) -> f32 {
    let v = f32::from(percent.min(100)) / 100.0;
    v * v
}

/// Which alerts are waiting to settle, and which panes rang recently.
#[derive(Debug, Default)]
pub struct AlertGate {
    pending: HashMap<Uuid, (AlertSound, Instant)>,
    rung: HashMap<(Uuid, AlertSound), Instant>,
    last_chime: Option<Instant>,
}

impl AlertGate {
    /// An agent pane nobody is watching just entered `sound`'s state.
    pub fn arm(&mut self, pane: Uuid, sound: AlertSound, now: Instant) {
        if self
            .rung
            .get(&(pane, sound))
            .is_some_and(|at| now.saturating_duration_since(*at) < PANE_COOLDOWN)
        {
            return;
        }
        // Re-arming the same alert keeps its original settle start.
        if self
            .pending
            .get(&pane)
            .is_none_or(|(armed, _)| *armed != sound)
        {
            self.pending.insert(pane, (sound, now));
        }
    }

    /// The user is looking at `pane`: what it does next is news again.
    pub fn attended(&mut self, pane: Uuid) {
        self.pending.remove(&pane);
        self.rung.retain(|(rang, _), _| *rang != pane);
    }

    /// The chime to play now, if any. `current` is each agent pane's present
    /// alert state (`None` while it works, or once it's gone); an alert whose
    /// pane has moved on is dropped.
    pub fn due(
        &mut self,
        current: impl Fn(Uuid) -> Option<AlertSound>,
        now: Instant,
    ) -> Option<AlertSound> {
        self.pending
            .retain(|pane, (sound, _)| current(*pane) == Some(*sound));
        self.rung
            .retain(|_, at| now.saturating_duration_since(*at) < PANE_COOLDOWN);
        if self
            .last_chime
            .is_some_and(|at| now.saturating_duration_since(at) < MIN_GAP)
        {
            return None;
        }
        let ready: Vec<(Uuid, AlertSound)> = self
            .pending
            .iter()
            .filter(|(_, (_, since))| now.saturating_duration_since(*since) >= SETTLE)
            .map(|(pane, (sound, _))| (*pane, *sound))
            .collect();
        if ready.is_empty() {
            return None;
        }
        for (pane, sound) in &ready {
            self.pending.remove(pane);
            self.rung.insert((*pane, *sound), now);
        }
        self.last_chime = Some(now);
        Some(
            if ready.iter().any(|(_, sound)| *sound == AlertSound::Blocked) {
                AlertSound::Blocked
            } else {
                AlertSound::Done
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{AlertGate, AlertSound, MIN_GAP, PANE_COOLDOWN, SETTLE, volume_gain};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    use AlertSound::{Blocked, Done};

    const TICK: Duration = Duration::from_secs(1);

    /// The gate as the status tick drives it: each pane's state per tick, with
    /// `arm` on every transition into Done/Blocked, then `due`.
    struct Sim {
        gate: AlertGate,
        states: HashMap<Uuid, Option<AlertSound>>,
        now: Instant,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                gate: AlertGate::default(),
                states: HashMap::new(),
                now: Instant::now(),
            }
        }

        /// Advance one tick with `pane` in `state`; returns what chimed.
        fn tick(&mut self, changes: &[(Uuid, Option<AlertSound>)]) -> Option<AlertSound> {
            self.now += TICK;
            for &(pane, state) in changes {
                let before = self.states.insert(pane, state).flatten();
                if let Some(sound) = state
                    && before != Some(sound)
                {
                    self.gate.arm(pane, sound, self.now);
                }
            }
            let states = self.states.clone();
            self.gate
                .due(|pane| states.get(&pane).copied().flatten(), self.now)
        }

        fn idle(&mut self, ticks: usize) -> Vec<AlertSound> {
            (0..ticks).filter_map(|_| self.tick(&[])).collect()
        }
    }

    #[test]
    fn rings_once_the_state_has_settled() {
        let pane = Uuid::new_v4();
        let mut sim = Sim::new();
        assert_eq!(sim.tick(&[(pane, Some(Done))]), None, "not settled yet");
        assert_eq!(sim.tick(&[]), Some(Done));
        assert!(sim.idle(10).is_empty(), "rings once");
    }

    #[test]
    fn a_done_that_flickers_back_to_work_never_rings() {
        let pane = Uuid::new_v4();
        let mut sim = Sim::new();
        sim.tick(&[(pane, Some(Done))]);
        assert_eq!(sim.tick(&[(pane, None)]), None);
        assert!(sim.idle(5).is_empty());
    }

    #[test]
    fn flapping_between_done_and_working_rings_once_until_attended() {
        let pane = Uuid::new_v4();
        let mut sim = Sim::new();
        let mut rang = Vec::new();
        // Done, ring, then back and forth for four minutes.
        for _ in 0..40 {
            rang.extend(sim.tick(&[(pane, Some(Done))]));
            rang.extend(sim.idle(2));
            rang.extend(sim.tick(&[(pane, None)]));
            rang.extend(sim.idle(2));
        }
        assert_eq!(rang, vec![Done]);
        // Looking at the pane re-arms it.
        sim.gate.attended(pane);
        sim.tick(&[(pane, Some(Done))]);
        assert_eq!(sim.tick(&[]), Some(Done));
    }

    #[test]
    fn a_pane_never_looked_at_rings_again_after_the_cooldown() {
        let pane = Uuid::new_v4();
        let mut sim = Sim::new();
        sim.tick(&[(pane, Some(Done))]);
        assert_eq!(sim.tick(&[]), Some(Done));
        sim.tick(&[(pane, None)]);
        sim.now += PANE_COOLDOWN;
        sim.tick(&[(pane, Some(Done))]);
        assert_eq!(sim.tick(&[]), Some(Done));
    }

    #[test]
    fn blocked_after_done_still_rings() {
        let pane = Uuid::new_v4();
        let mut sim = Sim::new();
        sim.tick(&[(pane, Some(Done))]);
        assert_eq!(sim.tick(&[]), Some(Done));
        sim.tick(&[(pane, None)]);
        sim.tick(&[(pane, Some(Blocked))]);
        assert_eq!(sim.tick(&[]), Some(Blocked));
    }

    #[test]
    fn panes_due_together_chime_once_and_blocked_wins() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut sim = Sim::new();
        sim.tick(&[(a, Some(Done)), (b, Some(Blocked)), (c, Some(Done))]);
        assert_eq!(sim.tick(&[]), Some(Blocked));
        assert!(sim.idle(10).is_empty(), "all three were covered");
    }

    #[test]
    fn an_alert_inside_the_gap_waits_rather_than_being_lost() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut sim = Sim::new();
        sim.tick(&[(a, Some(Done))]);
        assert_eq!(sim.tick(&[(b, Some(Blocked))]), Some(Done));
        let after = sim.idle(MIN_GAP.as_secs() as usize + 1);
        assert_eq!(after, vec![Blocked]);
    }

    #[test]
    fn an_alert_for_a_closed_pane_is_dropped() {
        let pane = Uuid::new_v4();
        let mut sim = Sim::new();
        sim.tick(&[(pane, Some(Blocked))]);
        sim.states.remove(&pane);
        assert!(sim.idle(5).is_empty());
    }

    #[test]
    fn rearming_keeps_the_original_settle_start() {
        let pane = Uuid::new_v4();
        let t0 = Instant::now();
        let mut gate = AlertGate::default();
        gate.arm(pane, Done, t0);
        gate.arm(pane, Done, t0 + SETTLE);
        assert_eq!(gate.due(|_| Some(Done), t0 + SETTLE), Some(Done));
    }

    #[test]
    fn volume_is_squared_and_clamped() {
        assert_eq!(volume_gain(0), 0.0);
        assert_eq!(volume_gain(50), 0.25);
        assert_eq!(volume_gain(100), 1.0);
        assert_eq!(volume_gain(250), 1.0);
    }
}
