//! Window ownership must not change a live terminal's background lifecycle.

use super::{LoopCleanup, LoopRun, PostRunAction, terminal_view_in};
use std::collections::HashMap;
use uuid::Uuid;

pub(super) fn loop_heartbeat<'a, T, P>(
    runs: &mut HashMap<Uuid, LoopRun>,
    docked: &'a HashMap<Uuid, T>,
    popouts: &'a HashMap<Uuid, P>,
    terminal: impl Fn(&'a P) -> Option<&'a T>,
    cleanup: impl Fn(&T, &mut LoopRun) -> LoopCleanup,
) -> Vec<Uuid> {
    let mut close = Vec::new();
    runs.retain(|iid, run| {
        let Some(view) = terminal_view_in(*iid, docked, popouts, &terminal) else {
            return false;
        };
        match cleanup(view, run) {
            LoopCleanup::Continue => true,
            LoopCleanup::Abandon => false,
            LoopCleanup::Finish => {
                if run.post_run == PostRunAction::Exit {
                    close.push(*iid);
                }
                false
            }
        }
    });
    close
}

#[cfg(test)]
mod tests {
    use super::loop_heartbeat;
    use crate::app::{AgentStatus, LoopRun, PostRunAction};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    #[test]
    fn popout_loop_grok_heartbeat_retains_owned_run_through_redock_and_exit_cleanup() {
        use muxel_terminal::{
            TerminalRuntimeSnapshot, TerminalStatusCoverage, TerminalStatusReason,
        };
        let pane = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let launch = Uuid::new_v4();
        let runtime = TerminalRuntimeSnapshot {
            status: AgentStatus::Working,
            attributable: true,
            reason: TerminalStatusReason::Quiet,
            coverage: TerminalStatusCoverage::ProviderVisible,
            launch_id: launch,
            binding_rev: 1,
            snapshot_seq: 1,
            input_rev: 0,
            draft_latched: false,
            completion_ready: false,
        };
        let terminal = Rc::new(Cell::new(runtime));
        let mut docked = HashMap::from([(pane, terminal.clone())]);
        let mut popouts = HashMap::new();
        let mut runs = HashMap::from([(
            pane,
            LoopRun {
                loop_id,
                launch: Some(launch),
                seen_working: false,
                started: Instant::now(),
                post_run: PostRunAction::Exit,
            },
        )]);
        let beat = |runs: &mut HashMap<_, _>, docked: &HashMap<_, _>, popouts: &HashMap<_, _>| {
            loop_heartbeat(
                runs,
                docked,
                popouts,
                Option::as_ref,
                |view: &Rc<Cell<TerminalRuntimeSnapshot>>, run| {
                    run.cleanup(view.get(), 1, Some(1), Duration::ZERO)
                },
            )
        };
        assert!(beat(&mut runs, &docked, &popouts).is_empty());
        popouts.insert(pane, Some(docked.remove(&pane).unwrap()));
        assert!(beat(&mut runs, &docked, &popouts).is_empty());
        assert!(
            runs.contains_key(&pane),
            "a live popout is not a closed pane"
        );
        assert!(runs.values().any(|run| run.loop_id == loop_id));
        docked.insert(pane, popouts.remove(&pane).unwrap().unwrap());
        assert!(Rc::ptr_eq(docked.get(&pane).unwrap(), &terminal));
        assert!(beat(&mut runs, &docked, &popouts).is_empty());
        terminal.set(TerminalRuntimeSnapshot {
            status: AgentStatus::Done,
            completion_ready: true,
            ..runtime
        });
        assert_eq!(beat(&mut runs, &docked, &popouts), vec![pane]);
        assert!(runs.is_empty());
    }
}
