//! Window ownership must not change a live terminal's background lifecycle.

use super::{AgentStatus, LoopRun, MAX_LOOP_RUNTIME, PostRunAction, terminal_view_in};
use std::collections::HashMap;
use uuid::Uuid;

// Provider capture/apply now belongs to the merged session_discovery worker.
pub(super) fn loop_heartbeat<'a, T, P>(
    runs: &mut HashMap<Uuid, LoopRun>,
    docked: &'a HashMap<Uuid, T>,
    popouts: &'a HashMap<Uuid, P>,
    terminal: impl Fn(&'a P) -> Option<&'a T>,
    status: impl Fn(Uuid, &T) -> Option<AgentStatus>,
) -> Vec<Uuid> {
    let mut close = Vec::new();
    runs.retain(|iid, run| {
        let Some(view) = terminal_view_in(*iid, docked, popouts, &terminal) else {
            return false;
        };
        let status = status(*iid, view);
        if status == Some(AgentStatus::Working) {
            run.seen_working = true;
        }
        let finished =
            run.seen_working && matches!(status, Some(AgentStatus::Idle) | Some(AgentStatus::Done));
        if finished || run.started.elapsed() >= MAX_LOOP_RUNTIME {
            if run.post_run == PostRunAction::Exit {
                close.push(*iid);
            }
            false
        } else {
            true
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
    use std::time::Instant;
    use uuid::Uuid;

    #[test]
    fn popout_loop_grok_heartbeat_retains_run_through_redock_and_exit_cleanup() {
        let pane = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let terminal = Rc::new(Cell::new(AgentStatus::Working));
        let mut docked = HashMap::from([(pane, terminal.clone())]);
        let mut popouts = HashMap::new();
        let mut runs = HashMap::from([(
            pane,
            LoopRun {
                loop_id,
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
                |_, view: &Rc<Cell<AgentStatus>>| Some(view.get()),
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
        terminal.set(AgentStatus::Done);
        assert_eq!(beat(&mut runs, &docked, &popouts), vec![pane]);
        assert!(runs.is_empty());
    }
}
