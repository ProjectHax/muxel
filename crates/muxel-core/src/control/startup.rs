//! Runtime ownership of the first task supplied by outside pane creation.
//!
//! Creation precedes PTY admission. Keep that interval distinct from the reply
//! grace period, which starts only when Enter is admitted to the writer.

use super::{AgentState, REPLY_GRACE_MS, awaiting_reply};
use crate::AgentActivity;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupTurnState {
    Pending,
    Submitted(i64),
    Cancelled,
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
        if tracked.state == StartupTurnState::Pending {
            tracked.state = StartupTurnState::Cancelled;
        }
    }

    /// One launch generation may own this task. Its reservation, command and
    /// automation can clone the lease; losing all owners cancels pending work.
    /// A second claim supersedes the old task rather than replaying its input.
    pub fn claim(&self) -> Option<StartupTurnOwner> {
        let mut tracked = self.lock();
        if tracked.claimed || tracked.state != StartupTurnState::Pending {
            if tracked.state == StartupTurnState::Pending {
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
            StartupTurnState::Pending => matches!(
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
            StartupTurnState::Cancelled => false,
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
        self.0.0.state() == StartupTurnState::Pending
    }

    pub fn cancel(&self) {
        self.0.0.cancel();
    }

    /// Call immediately after the final CR has been admitted, on the UI thread
    /// that also admits controller cancellation/replacement and startup writes.
    pub fn submitted(&self, at: i64) {
        let mut tracked = self.0.0.lock();
        if tracked.state == StartupTurnState::Pending {
            tracked.state = StartupTurnState::Submitted(at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StartupTurn, StartupTurnState};
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
        // Pasting does not settle ownership. Only admitted Enter starts grace.
        assert_eq!(turn.state(), StartupTurnState::Pending);
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
