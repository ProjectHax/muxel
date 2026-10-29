use super::ControlTurn;
use crate::app::LaunchTicket;
use crate::app::restart_completion_tests::with_app;
use muxel_core::control::StartupTurnState;
use muxel_core::{AgentPreset, Instance, Project, TerminalLaunchReason};
use std::time::Instant;
use uuid::Uuid;

#[test]
fn retained_tmux_reservation_claims_the_first_task_once_for_its_generation() {
    with_app(|cx, main| {
        main.update(cx, |app, _, _| {
            let iid = Uuid::new_v4();
            let turn = ControlTurn::startup("exact first task".into());
            let signal = turn.startup.as_ref().unwrap().clone();
            app.control_turns.insert(iid, turn);
            app.terminal_launching.insert(
                iid,
                LaunchTicket {
                    token: 17,
                    reason: TerminalLaunchReason::NewPane,
                    since: Instant::now(),
                    startup: None,
                },
            );
            app.claim_reserved_control_startup(iid, 16);
            assert!(app.terminal_launching[&iid].startup.is_none());
            assert_eq!(signal.state(), StartupTurnState::Pending);

            app.claim_reserved_control_startup(iid, 17);
            assert!(
                app.terminal_launching[&iid]
                    .startup
                    .as_ref()
                    .unwrap()
                    .is_pending()
            );
            app.claim_reserved_control_startup(iid, 17);
            assert_eq!(
                signal.state(),
                StartupTurnState::Pending,
                "claiming an already owned task again would cancel it"
            );
            assert_eq!(app.terminal_launching[&iid].token, 17);

            app.terminal_launching.remove(&iid);
            assert_eq!(signal.state(), StartupTurnState::Cancelled);
        })
        .unwrap();
    });
}

#[test]
fn retained_reservation_refusal_cancels_its_unclaimed_task_before_preparation() {
    with_app(|cx, main| {
        main.update(cx, |app, _, cx| {
            let project = app.workspace.add_project(Project::new(
                "literal conversion",
                muxel_store::data_dir().unwrap(),
            ));
            let mut instance = Instance::from_preset(project, &AgentPreset::codex());
            instance.literal_argv = true;
            let iid = instance.id;
            app.workspace.add_instance(instance);
            let turn = ControlTurn::startup("exact first task".into());
            let signal = turn.startup.as_ref().unwrap().clone();
            app.control_turns.insert(iid, turn);
            app.terminal_launching.insert(
                iid,
                LaunchTicket {
                    token: 17,
                    reason: TerminalLaunchReason::Restore,
                    since: Instant::now(),
                    startup: None,
                },
            );

            assert!(
                app.prepare_reserved_terminal_spawn(iid, 17, TerminalLaunchReason::Restore, cx)
                    .is_none()
            );
            assert_eq!(signal.state(), StartupTurnState::Cancelled);
            assert!(!app.terminal_launching.contains_key(&iid));
            assert!(app.terminals.is_empty());
        })
        .unwrap();
    });
}

#[test]
fn tmux_preparation_failure_cancels_a_task_before_or_after_claiming_it() {
    with_app(|cx, main| {
        main.update(cx, |app, _, _| {
            for claimed in [false, true] {
                let iid = Uuid::new_v4();
                let turn = ControlTurn::startup("exact first task".into());
                let signal = turn.startup.as_ref().unwrap().clone();
                app.control_turns.insert(iid, turn);
                let startup = claimed.then(|| app.claim_control_startup(iid).unwrap());
                app.terminal_launching.insert(
                    iid,
                    LaunchTicket {
                        token: 17,
                        reason: TerminalLaunchReason::NewPane,
                        since: Instant::now(),
                        startup,
                    },
                );
                app.fail_tmux_launch(iid, "owner preparation refused".into());
                assert_eq!(signal.state(), StartupTurnState::Cancelled);
                assert!(!app.terminal_launching.contains_key(&iid));
                assert_eq!(app.failed_launches[&iid], "owner preparation refused");
                assert!(app.terminals.is_empty());
            }
        })
        .unwrap();
    });
}
