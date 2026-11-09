use super::popout_lifecycle_tests::with_app;
use super::{Instance, NotifKind, PaneNode, Project};
use gpui::AppContext;
use muxel_core::library::resolve::snippet_send_action;
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalView};
use std::time::Instant;

fn rejected_paste_keeps_draft(transcript: bool) {
    with_app(|cx, window| {
        let (before, rejected, accepted) = window
            .update(cx, |app, window, cx| {
                // A local batch fixture exercises the unverified Codex paste
                // profile without launching a provider.
                let root = muxel_store::data_dir().unwrap().join("paste-target");
                std::fs::create_dir_all(&root).unwrap();
                let program = root.join("codex.cmd");
                std::fs::write(&program, "@echo off\r\nmore\r\n").unwrap();
                let project = app.workspace.add_project(Project::new("paste", &root));
                let mut instance = Instance::shell(project);
                instance.program = Some(program.to_string_lossy().into_owned());
                instance.args = vec!["--config".into(), "disable_paste_burst=true".into()];
                let pane = instance.id;
                let spec =
                    CommandSpec::program(instance.program.clone().unwrap(), instance.args.clone());
                let launch = TerminalLaunch::spawn(spec, (80, 24)).unwrap();
                let view = cx.new(|cx| TerminalView::new(launch, pane, Instant::now(), window, cx));
                let session = view.read(cx).session().clone();
                app.workspace.add_instance(instance);
                app.workspace.project_mut(project).unwrap().layout = Some(PaneNode::leaf(pane));
                app.workspace.active_project = Some(project);
                app.active_instance = Some(pane);
                app.use_tmux = false;
                app.use_worktree = false;
                app.terminals.insert(pane, view);

                session.try_paste("existing draft").unwrap();
                let before = session.input_snapshot();
                assert!(before.1, "fixture must have a draft");
                assert!(!session.has_submitted_turn());
                assert_eq!(
                    session.try_paste("prefix\tsuffix").unwrap_err().kind(),
                    std::io::ErrorKind::InvalidInput,
                    "fixture must reject tabs before writer admission"
                );
                let mut rejected = Vec::new();
                for submit in [true, false] {
                    app.notifications.clear();
                    if transcript {
                        let admission = app.terminal_input_admission(pane, cx);
                        app.insert_transcript(admission, "prefix\tsuffix", submit, window, cx);
                    } else {
                        app.send_snippet_steps(
                            pane,
                            &snippet_send_action("prefix\tsuffix", submit),
                            window,
                            cx,
                        );
                    }
                    rejected.push((
                        session.input_snapshot(),
                        session.has_submitted_turn(),
                        app.notifications.iter().any(|notification| {
                            matches!(notification.kind, NotifKind::Error)
                                && notification.subtitle.contains("U+0009")
                        }),
                    ));
                }

                // The same admitted target must still accept a valid action.
                if transcript {
                    let admission = app.terminal_input_admission(pane, cx);
                    app.insert_transcript(admission, "accepted text", true, window, cx);
                } else {
                    app.send_snippet_steps(
                        pane,
                        &snippet_send_action("accepted text", true),
                        window,
                        cx,
                    );
                }
                (
                    before,
                    rejected,
                    (session.input_rev(), session.has_submitted_turn()),
                )
            })
            .unwrap();
        // Assert after releasing the app update so a failed regression can
        // still run the fixture's normal process cleanup.
        for (snapshot, submitted, reported) in rejected {
            assert_eq!(
                snapshot, before,
                "a rejected paste must not write Enter or change the existing draft"
            );
            assert!(
                !submitted,
                "a rejected paste must not mark a turn submitted"
            );
            assert!(reported, "a rejected paste must appear in the error feed");
        }
        assert!(accepted.0 > before.0);
        assert!(accepted.1);
    });
}

#[test]
fn snippet_paste_rejection_never_submits_the_existing_draft() {
    rejected_paste_keeps_draft(false);
}

#[test]
fn transcript_paste_rejection_never_submits_the_existing_draft() {
    rejected_paste_keeps_draft(true);
}
