//! Pure close admission and expiry checks. The app supplies current launch facts.

use crate::control::AgentState;
use crate::{PaneNode, TerminalLaunchReason, remove};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseEvidence {
    Process,
    Program,
    Hook,
    Title,
    Screen,
    Bell,
    Output,
    Quiet,
    Settling,
    ShellPrompt,
}

/// Non-agent prompt evidence, separate from the platform foreground query so
/// its policy can be exercised on every test host.
pub fn shell_close_evidence(
    program: Option<&str>,
    via_client: bool,
    foreground_idle: bool,
    osc133_prompt: bool,
) -> CloseEvidence {
    // Foreground ownership proves a prompt only for the direct default shell.
    // A client or explicit program can own that group while remote work runs.
    if osc133_prompt || program.is_none() && !via_client && foreground_idle {
        CloseEvidence::ShellPrompt
    } else {
        CloseEvidence::Quiet
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseFacts {
    pub status: AgentState,
    pub evidence: CloseEvidence,
    pub attributable: bool,
    pub awaiting_reply: bool,
    pub another_tmux_client: bool,
    /// The pane runs in a tmux session that closing it would kill, here or on
    /// its host. A dead client or failed launch says nothing about the agent.
    pub session_may_outlive: bool,
    pub launch_id: Option<Uuid>,
    pub binding_rev: u64,
    pub input_rev: u64,
    pub completion_rev: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseDecision {
    Immediate,
    Ask,
    Hold { forced: bool },
}

/// [`CloseFacts::session_may_outlive`] for a pane: a local one with a recorded
/// tmux session, or one of a project on a host (`remote_unix`: Some, and
/// whether that host runs tmux). A literal command runs no tmux. These are the
/// panes whose close kills a session, or ledgers one to kill.
pub fn session_may_outlive(
    literal_argv: bool,
    remote_unix: Option<bool>,
    local_tmux: bool,
) -> bool {
    !literal_argv && remote_unix.unwrap_or(local_tmux)
}

fn looks_dead(facts: CloseFacts) -> bool {
    matches!(
        facts.status,
        AgentState::Exited | AgentState::NotRunning | AgentState::Failed
    )
}

/// Whether the pane's process is known to be gone, so its close loses nothing.
/// A tmux pane's exited client may have detached from an agent still running.
pub fn exit_proves_closed(facts: CloseFacts) -> bool {
    facts.status == AgentState::Exited && !facts.session_may_outlive
}

pub fn decide(facts: CloseFacts, ask_busy: bool) -> CloseDecision {
    if looks_dead(facts) {
        if !facts.session_may_outlive {
            return CloseDecision::Immediate;
        }
        // The session may still hold the agent; closing would kill it.
        return if ask_busy {
            CloseDecision::Ask
        } else {
            CloseDecision::Hold { forced: true }
        };
    }
    let quiet = matches!(facts.status, AgentState::Idle | AgentState::Done)
        && facts.attributable
        && !facts.awaiting_reply
        && !facts.another_tmux_client
        && matches!(
            facts.evidence,
            CloseEvidence::Program
                | CloseEvidence::Hook
                | CloseEvidence::Title
                | CloseEvidence::Screen
                | CloseEvidence::ShellPrompt
        );
    if quiet {
        CloseDecision::Hold { forced: false }
    } else if ask_busy {
        CloseDecision::Ask
    } else {
        CloseDecision::Hold { forced: true }
    }
}

pub fn may_finalize(before: CloseFacts, now: CloseFacts, forced: bool) -> bool {
    forced || before == now && decide(now, true) == CloseDecision::Hold { forced: false }
}

/// Where a close group is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseStage {
    /// Inside its Undo window.
    CountingDown,
    /// Past its deadline, checking again before it finishes.
    Checking,
    /// The check found new work: Close or Keep is the user's call.
    AwaitingDecision,
    /// Undo is waiting for an interrupted restart to settle.
    Undoing,
}

/// Leaving the workspace (quit, logout, switch) finishes only a running
/// countdown. A group being checked, waiting for Close / Keep, or being
/// restored keeps its panes: nobody decided to close what they now hold.
pub fn departure_finalizes(stage: CloseStage) -> bool {
    stage == CloseStage::CountingDown
}

/// What a held pane's launch was doing when its close began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldLaunch {
    /// Nothing in flight: the pane kept the process it had.
    Settled,
    /// A launch or reattach was starting. The hold dropped its result.
    Starting(TerminalLaunchReason),
    /// A restart was queued. Its kill may not have run yet.
    Restart,
}

pub fn held_launch(restart_pending: bool, launching: Option<TerminalLaunchReason>) -> HeldLaunch {
    match (restart_pending, launching) {
        (true, _) => HeldLaunch::Restart,
        (false, Some(reason)) => HeldLaunch::Starting(reason),
        (false, None) => HeldLaunch::Settled,
    }
}

/// The held pane's terminal when Undo runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldView {
    Running,
    /// Exited, or a launch that failed: the pane shows that as before.
    Ended,
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoLaunch {
    /// Bring the pane back as it is.
    Resume,
    /// Run the pane's ordinary launch again. A tmux pane reattaches to its
    /// session; nothing is killed first.
    Launch(TerminalLaunchReason),
    /// Run the restart the hold interrupted, which replaces the session.
    Restart,
}

/// Only a restart the user asked for may replace a session on Undo. A launch
/// or reattach the hold cut short runs again, and never over a live terminal.
pub fn undo_launch(held: HeldLaunch, view: HeldView) -> UndoLaunch {
    match (held, view) {
        (HeldLaunch::Restart, _) => UndoLaunch::Restart,
        (_, HeldView::Running) => UndoLaunch::Resume,
        (HeldLaunch::Starting(reason), _) => UndoLaunch::Launch(reason),
        (HeldLaunch::Settled, HeldView::Missing) => {
            UndoLaunch::Launch(TerminalLaunchReason::Recovery)
        }
        (HeldLaunch::Settled, HeldView::Ended) => UndoLaunch::Resume,
    }
}

/// After a peer layout was adopted over the durable layout (`layout`, with the
/// held panes in it): take the held panes it still lists out again, keeping it
/// as their place to come back to. Returns the held panes it no longer lists.
/// The peer closed those, so the adoption already dropped them here.
pub fn rehide_held(layout: &mut Option<PaneNode>, held: &[Uuid]) -> Vec<Uuid> {
    let listed = layout
        .as_ref()
        .map(PaneNode::collect_instances)
        .unwrap_or_default();
    let mut gone = Vec::new();
    for &iid in held {
        if listed.contains(&iid) {
            remove(layout, iid);
        } else {
            gone.push(iid);
        }
    }
    gone
}

/// Reads of one project's shared layout file, numbered as they begin. A read
/// is applied once this machine has compared it with its own layout, adopting
/// a peer's newer edit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LayoutReads {
    started: u64,
    applied: u64,
}

impl LayoutReads {
    /// Number a read as it begins.
    pub fn begin(&mut self) -> u64 {
        self.started += 1;
        self.started
    }

    /// Read `ticket`'s result has been applied.
    pub fn applied(&mut self, ticket: u64) {
        self.applied = self.applied.max(ticket);
    }

    /// The reads begun so far. Only a later one can show what peers wrote
    /// after this moment.
    pub fn mark(&self) -> u64 {
        self.started
    }
}

/// Whether a close whose countdown ran out may finish, given each project it
/// touches that shares its layout as (its [`LayoutReads::mark`] when the
/// deadline was first seen passed, its reads now). A machine that slept
/// through the countdown wakes with peers' edits unread, and the final close
/// is stamped with the wall clock: finished first, it would outrank and
/// overwrite them. So it waits for a read begun after the deadline.
pub fn expired_close_may_finish(projects: impl IntoIterator<Item = (u64, LayoutReads)>) -> bool {
    projects
        .into_iter()
        .all(|(mark, reads)| reads.applied > mark)
}

#[cfg(test)]
mod recoverable_close_policy_tests {
    use super::{
        CloseDecision, CloseEvidence, CloseFacts, CloseStage, HeldLaunch, HeldView, LayoutReads,
        UndoLaunch, decide, departure_finalizes, exit_proves_closed, expired_close_may_finish,
        held_launch, may_finalize, rehide_held, session_may_outlive, shell_close_evidence,
        undo_launch,
    };
    use crate::control::AgentState;
    use crate::{PaneNode, SplitDirection, TerminalLaunchReason, split};
    use uuid::Uuid;

    fn quiet() -> CloseFacts {
        CloseFacts {
            status: AgentState::Idle,
            evidence: CloseEvidence::Title,
            attributable: true,
            awaiting_reply: false,
            another_tmux_client: false,
            session_may_outlive: false,
            launch_id: Some(Uuid::new_v4()),
            binding_rev: 3,
            input_rev: 7,
            completion_rev: 2,
        }
    }

    #[test]
    fn non_agent_foreground_ownership_does_not_prove_a_shell_prompt() {
        for program in ["ssh", "python", "node", "wrapper.sh", "bash"] {
            let evidence = shell_close_evidence(Some(program), false, true, false);
            assert_eq!(evidence, CloseEvidence::Quiet, "explicit program {program}");
            assert_eq!(
                decide(
                    CloseFacts {
                        evidence,
                        ..quiet()
                    },
                    true
                ),
                CloseDecision::Ask
            );
        }
    }

    #[test]
    fn default_shell_foreground_proof_excludes_tmux_and_remote_clients() {
        assert_eq!(
            shell_close_evidence(None, false, true, false),
            CloseEvidence::ShellPrompt
        );
        assert_eq!(
            shell_close_evidence(None, false, false, false),
            CloseEvidence::Quiet
        );
        // Both tmux and an SSH connection can own the child foreground group
        // while a command is running beyond that client.
        let evidence = shell_close_evidence(None, true, true, false);
        assert_eq!(evidence, CloseEvidence::Quiet);
        assert_eq!(
            decide(
                CloseFacts {
                    evidence,
                    ..quiet()
                },
                true
            ),
            CloseDecision::Ask
        );
    }

    #[test]
    fn explicit_osc133_prompt_remains_protocol_evidence() {
        for program in [None, Some("custom-shell"), Some("wrapper.sh")] {
            for via_client in [false, true] {
                let evidence = shell_close_evidence(program, via_client, false, true);
                assert_eq!(evidence, CloseEvidence::ShellPrompt);
                assert_eq!(
                    decide(
                        CloseFacts {
                            evidence,
                            ..quiet()
                        },
                        true
                    ),
                    CloseDecision::Hold { forced: false }
                );
            }
        }
    }

    #[test]
    fn idle_agent_close_uses_rechecked_countdown() {
        for evidence in [
            CloseEvidence::Title,
            CloseEvidence::Screen,
            CloseEvidence::Hook,
            CloseEvidence::Program,
        ] {
            for status in [AgentState::Idle, AgentState::Done] {
                let facts = CloseFacts {
                    status,
                    evidence,
                    ..quiet()
                };
                assert_eq!(decide(facts, true), CloseDecision::Hold { forced: false });
                assert!(may_finalize(facts, facts, false));
            }
        }
    }

    #[test]
    fn working_agent_close_asks_and_confirm_counts_down_without_recheck() {
        let facts = CloseFacts {
            status: AgentState::Working,
            ..quiet()
        };
        assert_eq!(decide(facts, true), CloseDecision::Ask);
        assert!(may_finalize(facts, facts, true));
    }

    #[test]
    fn settling_quiet_or_bell_only_agent_asks() {
        for evidence in [
            CloseEvidence::Settling,
            CloseEvidence::Quiet,
            CloseEvidence::Bell,
            CloseEvidence::Output,
        ] {
            let facts = CloseFacts {
                evidence,
                ..quiet()
            };
            assert_eq!(decide(facts, true), CloseDecision::Ask, "{evidence:?}");
        }
        assert_eq!(
            decide(
                CloseFacts {
                    attributable: false,
                    ..quiet()
                },
                true
            ),
            CloseDecision::Ask
        );
        assert_eq!(
            decide(
                CloseFacts {
                    status: AgentState::Starting,
                    ..quiet()
                },
                true
            ),
            CloseDecision::Ask
        );
        assert_eq!(
            decide(
                CloseFacts {
                    status: AgentState::Blocked,
                    ..quiet()
                },
                true
            ),
            CloseDecision::Ask
        );
    }

    #[test]
    fn busy_close_with_setting_off_counts_down_without_asking() {
        assert_eq!(
            decide(
                CloseFacts {
                    status: AgentState::Working,
                    ..quiet()
                },
                false
            ),
            CloseDecision::Hold { forced: true }
        );
        assert_eq!(
            decide(quiet(), false),
            CloseDecision::Hold { forced: false }
        );
    }

    #[test]
    fn expiry_rechecks_and_turns_started_work_into_a_decision() {
        let before = quiet();
        for now in [
            CloseFacts {
                status: AgentState::Working,
                ..before
            },
            CloseFacts {
                status: AgentState::Done,
                ..before
            },
            CloseFacts {
                binding_rev: 4,
                ..before
            },
            CloseFacts {
                input_rev: 8,
                ..before
            },
            CloseFacts {
                completion_rev: 3,
                ..before
            },
            CloseFacts {
                launch_id: Some(Uuid::new_v4()),
                ..before
            },
            CloseFacts {
                attributable: false,
                ..before
            },
            CloseFacts {
                awaiting_reply: true,
                ..before
            },
            CloseFacts {
                another_tmux_client: true,
                ..before
            },
        ] {
            assert!(!may_finalize(before, now, false), "{now:?}");
        }
    }

    #[test]
    fn forced_close_skips_expiry_recheck() {
        assert!(may_finalize(
            quiet(),
            CloseFacts {
                status: AgentState::Working,
                attributable: false,
                ..quiet()
            },
            true
        ));
    }

    #[test]
    fn pending_turn_or_second_tmux_client_never_proves_quiet() {
        for facts in [
            CloseFacts {
                awaiting_reply: true,
                ..quiet()
            },
            CloseFacts {
                another_tmux_client: true,
                ..quiet()
            },
        ] {
            assert_eq!(decide(facts, true), CloseDecision::Ask);
        }
    }

    #[test]
    fn shell_requires_current_prompt_evidence_and_exit_is_immediate() {
        assert_eq!(
            decide(
                CloseFacts {
                    evidence: CloseEvidence::ShellPrompt,
                    ..quiet()
                },
                true
            ),
            CloseDecision::Hold { forced: false }
        );
        assert_eq!(
            decide(
                CloseFacts {
                    status: AgentState::Exited,
                    evidence: CloseEvidence::Process,
                    ..quiet()
                },
                true
            ),
            CloseDecision::Immediate
        );
    }

    #[test]
    fn undo_restarts_only_a_queued_restart() {
        let reason = TerminalLaunchReason::Recovery;
        assert_eq!(held_launch(true, Some(reason)), HeldLaunch::Restart);
        assert_eq!(
            held_launch(false, Some(reason)),
            HeldLaunch::Starting(reason)
        );
        assert_eq!(held_launch(false, None), HeldLaunch::Settled);
        for view in [HeldView::Running, HeldView::Ended, HeldView::Missing] {
            assert_eq!(undo_launch(HeldLaunch::Restart, view), UndoLaunch::Restart);
        }
        // A reconnect in flight over the old, exited client, and a restore that
        // had no terminal yet: both attach again, never restart.
        for reason in [
            TerminalLaunchReason::Recovery,
            TerminalLaunchReason::Restore,
            TerminalLaunchReason::NewPane,
        ] {
            for view in [HeldView::Ended, HeldView::Missing] {
                assert_eq!(
                    undo_launch(HeldLaunch::Starting(reason), view),
                    UndoLaunch::Launch(reason)
                );
            }
            assert_eq!(
                undo_launch(HeldLaunch::Starting(reason), HeldView::Running),
                UndoLaunch::Resume,
                "a live terminal is never replaced"
            );
        }
        assert_eq!(
            undo_launch(HeldLaunch::Settled, HeldView::Running),
            UndoLaunch::Resume
        );
        assert_eq!(
            undo_launch(HeldLaunch::Settled, HeldView::Ended),
            UndoLaunch::Resume
        );
        assert_eq!(
            undo_launch(HeldLaunch::Settled, HeldView::Missing),
            UndoLaunch::Launch(TerminalLaunchReason::Recovery)
        );
    }

    #[test]
    fn only_panes_whose_close_kills_a_tmux_session_may_outlive_it() {
        // Local: only with a recorded session.
        assert!(session_may_outlive(false, None, true));
        assert!(!session_may_outlive(false, None, false));
        // Remote: a Unix host always runs the pane in tmux; Windows never.
        assert!(session_may_outlive(false, Some(true), false));
        assert!(!session_may_outlive(false, Some(false), true));
        // A literal command runs no tmux, wherever it is.
        for remote_unix in [None, Some(true), Some(false)] {
            assert!(!session_may_outlive(true, remote_unix, true));
        }
    }

    #[test]
    fn tmux_pane_that_only_looks_dead_asks_and_keeps_its_undo() {
        // A pane that never connected, a restart whose kill failed, and a
        // detached client all leave the session, and maybe the agent, running.
        for status in [
            AgentState::NotRunning,
            AgentState::Failed,
            AgentState::Exited,
        ] {
            let tmux = CloseFacts {
                status,
                evidence: CloseEvidence::Process,
                session_may_outlive: true,
                ..quiet()
            };
            assert_eq!(decide(tmux, true), CloseDecision::Ask, "{status:?}");
            assert_eq!(
                decide(tmux, false),
                CloseDecision::Hold { forced: true },
                "{status:?}"
            );
            assert!(!exit_proves_closed(tmux), "{status:?}");
            let direct = CloseFacts {
                session_may_outlive: false,
                ..tmux
            };
            assert_eq!(decide(direct, true), CloseDecision::Immediate, "{status:?}");
            assert_eq!(exit_proves_closed(direct), status == AgentState::Exited);
        }
    }

    #[test]
    fn tmux_client_exiting_during_a_quiet_countdown_needs_a_decision() {
        let before = CloseFacts {
            session_may_outlive: true,
            ..quiet()
        };
        assert_eq!(decide(before, true), CloseDecision::Hold { forced: false });
        let now = CloseFacts {
            status: AgentState::Exited,
            evidence: CloseEvidence::Process,
            ..before
        };
        assert!(!exit_proves_closed(now));
        assert!(!may_finalize(before, now, false));
        // Without a session behind it, the exit itself finishes the close.
        let direct = CloseFacts {
            session_may_outlive: false,
            ..now
        };
        assert!(exit_proves_closed(direct));
    }

    #[test]
    fn adopted_layout_keeps_held_panes_hidden_at_their_new_place() {
        let id = Uuid::from_u128;
        // A peer moved pane 2 and split 3 off it while 2 was held here.
        let mut adopted = Some(PaneNode::leaf(id(1)));
        split(&mut adopted, id(1), SplitDirection::Vertical, id(2));
        split(&mut adopted, id(2), SplitDirection::Horizontal, id(3));
        let durable = adopted.clone();
        let gone = rehide_held(&mut adopted, &[id(2), id(4)]);
        // Pane 4 is no longer listed: the peer closed it.
        assert_eq!(gone, vec![id(4)]);
        let shown = adopted.as_ref().unwrap().collect_instances();
        assert_eq!(shown, vec![id(1), id(3)]);
        assert!(durable.unwrap().find_path(id(2)).is_some());
    }

    #[test]
    fn leaving_finishes_only_running_countdowns() {
        assert!(departure_finalizes(CloseStage::CountingDown));
        for stage in [
            CloseStage::Checking,
            CloseStage::AwaitingDecision,
            CloseStage::Undoing,
        ] {
            assert!(!departure_finalizes(stage), "{stage:?}");
        }
    }

    #[test]
    fn an_expired_close_waits_for_a_layout_read_begun_after_its_deadline() {
        let mut reads = LayoutReads::default();
        // A read in flight across the sleep began before the deadline.
        let before = reads.begin();
        let mark = reads.mark();
        reads.applied(before);
        assert!(!expired_close_may_finish([(mark, reads)]));
        // A read the push dropped, or that failed, was never applied.
        let dropped = reads.begin();
        assert!(!expired_close_may_finish([(mark, reads)]));
        let fresh = reads.begin();
        reads.applied(fresh);
        assert!(expired_close_may_finish([(mark, reads)]));
        // An older read landing late doesn't undo it.
        reads.applied(dropped);
        assert!(expired_close_may_finish([(mark, reads)]));
        // Every shared project the close touches must have been read.
        let other = LayoutReads::default();
        assert!(!expired_close_may_finish([
            (mark, reads),
            (other.mark(), other)
        ]));
        // A close touching no shared layout finishes at its deadline.
        assert!(expired_close_may_finish([]));
    }
}
