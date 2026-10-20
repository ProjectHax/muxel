//! Runtime ownership of the first task supplied by outside pane creation.
//!
//! Creation precedes PTY admission. Keep that interval distinct from the reply
//! grace period. Enter admission is not provider acceptance.

use super::{AgentState, REPLY_GRACE_MS, awaiting_reply};
use crate::AgentActivity;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupTurnState {
    Pending,
    AwaitingAcceptance(i64),
    Submitted(i64),
    Failed(StartupFailure),
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupFailure {
    NotReady,
    NotAccepted,
    WriteFailed,
    UnsupportedProvider,
}

impl StartupFailure {
    pub fn message(self) -> &'static str {
        match self {
            Self::NotReady => {
                "Startup task was not sent: the provider's editable composer did not become ready."
            }
            Self::NotAccepted => {
                "Startup task submission was not confirmed by the provider after at most one Enter retry. Inspect the pane before sending another task."
            }
            Self::WriteFailed => "Startup task could not be written to its terminal.",
            Self::UnsupportedProvider => {
                "Startup task was not sent: this provider has no verified input and acceptance protocol. Open the pane and submit the task directly."
            }
        }
    }
}

impl StartupTurnState {
    fn is_pending(self) -> bool {
        matches!(self, Self::Pending | Self::AwaitingAcceptance(_))
    }
}

#[derive(Debug)]
struct Tracked {
    state: StartupTurnState,
    claimed: bool,
}

/// The controller's observation of a single startup task. Never persisted or
/// reused for a later task, even when the pane retains the same UUID.
#[derive(Clone, Debug)]
pub struct StartupTurn(Arc<Mutex<Tracked>>);

impl Default for StartupTurn {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Tracked {
            state: StartupTurnState::Pending,
            claimed: false,
        })))
    }
}

impl StartupTurn {
    fn lock(&self) -> MutexGuard<'_, Tracked> {
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn state(&self) -> StartupTurnState {
        self.lock().state
    }

    pub fn cancel(&self) {
        let mut tracked = self.lock();
        if tracked.state.is_pending() {
            tracked.state = StartupTurnState::Cancelled;
        }
    }

    /// One launch generation may own this task. Its reservation, command and
    /// automation can clone the lease; losing all owners cancels pending work.
    /// A second claim supersedes the old task rather than replaying its input.
    pub fn claim(&self) -> Option<StartupTurnOwner> {
        let mut tracked = self.lock();
        if tracked.claimed || tracked.state != StartupTurnState::Pending {
            if tracked.state.is_pending() {
                tracked.state = StartupTurnState::Cancelled;
            }
            return None;
        }
        tracked.claimed = true;
        Some(StartupTurnOwner(Arc::new(Lease(self.clone()))))
    }

    pub fn awaiting_reply(
        &self,
        state: Option<AgentState>,
        activity: &AgentActivity,
        now: i64,
    ) -> bool {
        match self.state() {
            StartupTurnState::Pending | StartupTurnState::AwaitingAcceptance(_) => matches!(
                state,
                Some(
                    AgentState::Starting
                        | AgentState::Working
                        | AgentState::Idle
                        | AgentState::Done
                        | AgentState::Blocked
                )
            ),
            StartupTurnState::Submitted(at) => {
                awaiting_reply(Some(at), REPLY_GRACE_MS, state, activity, now)
            }
            StartupTurnState::Cancelled | StartupTurnState::Failed(_) => false,
        }
    }

    pub fn failure(&self) -> Option<StartupFailure> {
        match self.state() {
            StartupTurnState::Failed(reason) => Some(reason),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Lease(StartupTurn);

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Owning launch/automation lease. The signal does not keep this lease alive.
#[derive(Clone, Debug)]
pub struct StartupTurnOwner(Arc<Lease>);

impl StartupTurnOwner {
    pub fn signal(&self) -> StartupTurn {
        self.0.0.clone()
    }

    pub fn is_pending(&self) -> bool {
        self.0.0.state().is_pending()
    }

    pub fn cancel(&self) {
        self.0.0.cancel();
    }

    /// A successful writer admission only starts the bounded acceptance check.
    pub fn enter_admitted(&self, at: i64) {
        let mut tracked = self.0.0.lock();
        if tracked.state == StartupTurnState::Pending {
            tracked.state = StartupTurnState::AwaitingAcceptance(at);
        }
    }

    /// Fresh provider-owned turn evidence, observed by this launch's automation.
    /// Keep the first Enter timestamp so a fast reply predating this observation
    /// still belongs to the submitted task.
    pub fn submitted(&self, at: i64) {
        let mut tracked = self.0.0.lock();
        if tracked.state.is_pending() {
            let entered = match tracked.state {
                StartupTurnState::AwaitingAcceptance(entered) => entered,
                _ => at,
            };
            tracked.state = StartupTurnState::Submitted(entered);
        }
    }

    pub fn fail(&self, reason: StartupFailure) {
        let mut tracked = self.0.0.lock();
        if tracked.state.is_pending() {
            tracked.state = StartupTurnState::Failed(reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StartupFailure, StartupTurn, StartupTurnState};
    use crate::AgentActivity;
    use crate::control::{AgentState, WaitOutcome, wait_outcome};

    #[test]
    fn startup_wait_covers_creation_paste_and_the_actual_submitted_turn() {
        let turn = StartupTurn::default();
        let owner = turn.claim().unwrap();
        let mut activity = AgentActivity {
            completed_at: Some(5),
            ..Default::default()
        };
        for state in [AgentState::Idle, AgentState::Done, AgentState::Blocked] {
            // Startup/readiness/paste may take longer than the reply grace.
            let awaiting = turn.awaiting_reply(Some(state), &activity, 90_000);
            assert!(awaiting);
            assert_eq!(wait_outcome(Some(state), awaiting), None);
        }
        // Pasting and our Enter do not settle ownership.
        assert_eq!(turn.state(), StartupTurnState::Pending);
        owner.enter_admitted(90_001);
        assert_eq!(turn.state(), StartupTurnState::AwaitingAcceptance(90_001));
        assert!(turn.awaiting_reply(Some(AgentState::Idle), &activity, 120_000));
        // Even after the ordinary grace, our CR cannot finish an unaccepted task.
        owner.submitted(90_001);
        assert!(turn.awaiting_reply(Some(AgentState::Idle), &activity, 90_002));
        activity.completed_at = Some(90_010);
        let awaiting = turn.awaiting_reply(Some(AgentState::Done), &activity, 90_011);
        assert!(!awaiting);
        assert_eq!(
            wait_outcome(Some(AgentState::Done), awaiting),
            Some(WaitOutcome::Finished)
        );
    }

    #[test]
    fn fast_reply_before_acceptance_poll_still_belongs_to_first_enter() {
        let turn = StartupTurn::default();
        let owner = turn.claim().unwrap();
        owner.enter_admitted(10);
        owner.enter_admitted(20);
        let activity = AgentActivity {
            completed_at: Some(25),
            ..Default::default()
        };
        owner.submitted(30);
        assert_eq!(turn.state(), StartupTurnState::Submitted(10));
        assert!(!turn.awaiting_reply(Some(AgentState::Done), &activity, 31));
    }

    #[test]
    fn acceptance_failure_survives_owner_drop_and_late_provider_evidence() {
        let turn = StartupTurn::default();
        let owner = turn.claim().unwrap();
        owner.enter_admitted(10);
        owner.enter_admitted(20); // The one retry does not replace the first CR.
        owner.fail(StartupFailure::NotAccepted);
        owner.submitted(30);
        drop(owner);
        assert_eq!(turn.failure(), Some(StartupFailure::NotAccepted));
        assert!(!turn.awaiting_reply(Some(AgentState::Idle), &AgentActivity::default(), 40));
        assert!(turn.claim().is_none());
    }

    #[test]
    fn dropping_the_last_launch_owner_cancels_without_waiting_for_grace() {
        let turn = StartupTurn::default();
        let reservation = turn.claim().unwrap();
        let command = reservation.clone();
        drop(reservation);
        assert_eq!(turn.state(), StartupTurnState::Pending);
        drop(command);
        assert_eq!(turn.state(), StartupTurnState::Cancelled);
        assert!(!turn.awaiting_reply(Some(AgentState::Idle), &AgentActivity::default(), 1));
    }

    #[test]
    fn cancellation_and_old_completion_cannot_replay_or_settle_a_new_task() {
        let old = StartupTurn::default();
        let owner = old.claim().unwrap();
        assert!(old.claim().is_none());
        assert!(!owner.is_pending());
        let new = StartupTurn::default();
        let new_owner = new.claim().unwrap();
        owner.submitted(10);
        assert_eq!(old.state(), StartupTurnState::Cancelled);
        assert_eq!(new.state(), StartupTurnState::Pending);
        new_owner.submitted(20);
        new.cancel();
        drop(new_owner);
        assert_eq!(new.state(), StartupTurnState::Submitted(20));
    }

    #[test]
    fn failed_exited_and_stopped_panes_do_not_keep_waiting_on_pending_input() {
        let turn = StartupTurn::default();
        for state in [
            AgentState::Failed,
            AgentState::Exited,
            AgentState::NotRunning,
        ] {
            assert!(!turn.awaiting_reply(Some(state), &AgentActivity::default(), 10));
            assert!(wait_outcome(Some(state), false).is_some());
        }
        turn.cancel();
        assert!(turn.claim().is_none());
    }
}
