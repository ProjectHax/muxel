//! App wiring without a real PTY: runs on every desktop test host.
use super::MuxelApp;
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::recoverable_close::{ExpiredCloseSync, LayoutWait, expired_close_sync};
use muxel_core::{Instance, PaneNode, Project};
use std::time::{Duration, Instant};
use uuid::Uuid;

fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>, Uuid, Uuid)) {
    let root = std::env::temp_dir().join(format!("muxel-close-layout-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let _storage = muxel_store::test_support::set_root(root.clone());
    let mut cx = TestAppContext::single();
    cx.skip_drawing();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let window = cx.add_window(MuxelApp::new);
    let (pid, iid) = window
        .update(&mut cx, |app, _, _| {
            app._status_timer = gpui::Task::ready(());
            let pid = app
                .workspace
                .add_project(Project::new("layout fixture", root.clone()));
            let inst = Instance::shell(pid);
            let iid = inst.id;
            app.workspace.add_instance(inst);
            app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(iid));
            app.workspace.active_project = Some(pid);
            app.current_workspace = Some(Uuid::new_v4());
            app.failed_launches
                .insert(iid, "fixture has no child".into());
            app.use_tmux = true;
            app.remote_synced.insert(pid);
            (pid, iid)
        })
        .unwrap();
    run(&mut cx, window, pid, iid);
    drop(cx);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn hold_undo_and_departure_preserve_the_durable_layout_without_spawning() {
    with_app(|cx, window, pid, iid| {
        window
            .update(cx, |app, window, cx| {
                let gid = app.hold_closes(vec![(iid, true)], None, cx).unwrap();
                assert!(app.workspace.project(pid).unwrap().instances().is_empty());
                assert_eq!(
                    app.workspace_with_closes_restored()
                        .project(pid)
                        .unwrap()
                        .instances(),
                    vec![iid]
                );
                app.undo_close(gid, window, cx);
                assert_eq!(app.workspace.project(pid).unwrap().instances(), vec![iid]);
                assert!(!app.terminal_launching.contains_key(&iid));
                app.hold_closes(vec![(iid, true)], None, cx).unwrap();
                app.expire_closes(Instant::now() + Duration::from_secs(16), cx);
                assert!(app.is_closing(iid));
                app.finalize_all_closes(cx);
                assert!(!app.is_closing(iid));
                assert!(app.workspace.instance(iid).is_some());
                assert_eq!(app.workspace.project(pid).unwrap().instances(), vec![iid]);
                // A countdown still inside its grace period also cannot
                // publish removal without a read after departure was requested.
                app.hold_closes(vec![(iid, true)], None, cx).unwrap();
                app.finalize_all_closes(cx);
                assert!(app.workspace.instance(iid).is_some());
                assert_eq!(app.workspace.project(pid).unwrap().instances(), vec![iid]);
            })
            .unwrap();
    });
}

#[test]
fn explicit_close_waits_for_a_read_started_after_the_click() {
    with_app(|cx, window, pid, iid| {
        window
            .update(cx, |app, _, cx| {
                let old = app.layout_reads.entry(pid).or_default().begin();
                app.layout_reads.get_mut(&pid).unwrap().applied(old);
                let gid = app.hold_closes(vec![(iid, true)], None, cx).unwrap();
                app.close_groups[0].needs_decision = true;
                app.finalize_close_group(gid, cx);
                app.expire_closes(Instant::now(), cx);
                assert!(app.workspace.instance(iid).is_some());
                let fresh = app.layout_reads.entry(pid).or_default().begin();
                app.layout_reads.get_mut(&pid).unwrap().applied(fresh);
                app.expire_closes(Instant::now(), cx);
                assert!(app.workspace.instance(iid).is_none());
            })
            .unwrap();
    });
}

fn poll(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) {
    window
        .update(cx, |app, window, cx| app.fetch_remote_layouts(window, cx))
        .unwrap();
    cx.run_until_parked();
    assert!(
        !window
            .read_with(cx, |app, _| app.remote_layout_poll_inflight)
            .unwrap()
    );
}

#[test]
fn polls_release_close_only_after_a_comparison_and_reject_push_races() {
    with_app(|cx, window, pid, _| {
        let path = window
            .read_with(cx, |app, _| {
                app.workspace
                    .project(pid)
                    .unwrap()
                    .root_path
                    .join(".muxel/workspace.json")
            })
            .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "unfinished peer JSON").unwrap();
        poll(cx, window);
        window
            .read_with(cx, |app, _| {
                assert_eq!(
                    expired_close_sync(
                        [LayoutWait {
                            mark: 0,
                            reads: app.layout_reads[&pid],
                            readable: true
                        }],
                        Duration::ZERO
                    ),
                    ExpiredCloseSync::Wait
                );
            })
            .unwrap();
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        poll(cx, window);
        window
            .read_with(cx, |app, _| {
                assert_eq!(
                    expired_close_sync(
                        [LayoutWait {
                            mark: 0,
                            reads: app.layout_reads[&pid],
                            readable: true
                        }],
                        Duration::ZERO
                    ),
                    ExpiredCloseSync::Wait
                );
            })
            .unwrap();
        // An in-flight read must not satisfy a close after a local push begins.
        std::fs::remove_file(&path).unwrap();
        window
            .update(cx, |app, window, cx| {
                app.fetch_remote_layouts(window, cx);
                *app.layout_push_epoch.entry(pid).or_default() += 1;
            })
            .unwrap();
        cx.run_until_parked();
        window
            .read_with(cx, |app, _| {
                assert_eq!(
                    expired_close_sync(
                        [LayoutWait {
                            mark: 0,
                            reads: app.layout_reads[&pid],
                            readable: true
                        }],
                        Duration::ZERO
                    ),
                    ExpiredCloseSync::Wait
                );
            })
            .unwrap();
        poll(cx, window);
        window
            .read_with(cx, |app, _| {
                assert_eq!(
                    expired_close_sync(
                        [LayoutWait {
                            mark: 0,
                            reads: app.layout_reads[&pid],
                            readable: true
                        }],
                        Duration::ZERO
                    ),
                    ExpiredCloseSync::Finish
                );
            })
            .unwrap();
    });
}

#[test]
fn poll_clears_inflight_even_if_its_window_closes() {
    with_app(|cx, window, pid, _| {
        let app = window.entity(cx).unwrap();
        window
            .update(cx, |app, window, cx| {
                app.fetch_remote_layouts(window, cx);
                assert!(app.remote_layout_poll_inflight);
                window.remove_window();
            })
            .unwrap();
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(!app.remote_layout_poll_inflight);
            // The read couldn't be compared inside the removed window.
            assert_eq!(
                expired_close_sync(
                    [LayoutWait {
                        mark: 0,
                        reads: app.layout_reads[&pid],
                        readable: true,
                    }],
                    Duration::ZERO
                ),
                ExpiredCloseSync::Wait
            );
        });
    });
}

#[test]
fn review_close_retry_forgets_a_session_imported_into_another_pane() {
    with_app(|cx, window, pid, iid| {
        window
            .update(cx, |app, window, cx| {
                let target = muxel_core::tmux::SessionTarget::new(
                    "imported-session",
                    Some(muxel_core::tmux::Socket::Private),
                );
                // Import creates a fresh pane id bound to the still-running target.
                app.workspace
                    .project_mut(pid)
                    .unwrap()
                    .remember_closed_session(target.clone(), Uuid::new_v4());
                app.workspace.project_mut(pid).unwrap().closed_sessions[0].local_close_token =
                    Some(Uuid::new_v4());
                let inst = app.workspace.instance_mut(iid).unwrap();
                inst.tmux_session = Some(target.name);
                inst.tmux_socket = target.socket;
                app.ensure_project_terminals(pid, window, cx);
                assert!(
                    app.local_closes_inflight.is_empty(),
                    "retry started under an imported pane"
                );
                assert!(
                    app.workspace
                        .project(pid)
                        .unwrap()
                        .closed_sessions
                        .is_empty()
                );
            })
            .unwrap();
    });
}

#[test]
fn review_close_reconciles_the_peer_layout_before_replaying_a_kill() {
    with_app(|cx, window, pid, iid| {
        window
            .update(cx, |app, window, cx| {
                let inst = app.workspace.instance_mut(iid).unwrap();
                inst.tmux_session = Some("peer-kept-session".into());
                inst.tmux_socket = Some(muxel_core::tmux::Socket::Private);
                let mut peer = app.sync_capture(pid, 1).unwrap();
                peer.updated_at = u64::MAX / 2;
                let root = app.workspace.project(pid).unwrap().root_path.clone();
                std::fs::create_dir_all(root.join(".muxel")).unwrap();
                std::fs::write(root.join(".muxel/workspace.json"), peer.to_json()).unwrap();
                app.workspace.remove_instance_meta(iid);
                let project = app.workspace.project_mut(pid).unwrap();
                project.layout = None;
                project.remember_closed_session(
                    muxel_core::tmux::SessionTarget::new(
                        "peer-kept-session",
                        Some(muxel_core::tmux::Socket::Private),
                    ),
                    iid,
                );
                project.closed_sessions[0].local_close_token = Some(Uuid::new_v4());
                app.remote_synced.remove(&pid);
                // Keep the fixture headless if reconciliation restores the pane.
                app.ensure_project_terminals_deferred(pid, window, cx);
                assert!(
                    app.local_closes_inflight.is_empty(),
                    "retry ran before the peer pane was restored"
                );
                assert!(app.workspace.instance(iid).is_some());
                assert!(
                    app.workspace
                        .project(pid)
                        .unwrap()
                        .closed_sessions
                        .is_empty()
                );
            })
            .unwrap();
    });
}

#[test]
fn review_close_a_recreated_panes_new_intent_supersedes_its_inflight_close() {
    with_app(|cx, window, pid, iid| {
        window
            .update(cx, |app, _, cx| {
                let target = muxel_core::tmux::SessionTarget::new(
                    "recreated-pane",
                    Some(muxel_core::tmux::Socket::Private),
                );
                let original = app.workspace.instance_mut(iid).unwrap();
                original.tmux_session = Some(target.name.clone());
                original.tmux_socket = target.socket.clone();
                app.close_instance_inner(iid, "close (first intent)", cx);
                let first = app.workspace.project(pid).unwrap().closed_sessions[0]
                    .local_close_token
                    .unwrap();
                // A peer recreates the same instance before the first async job runs.
                let mut recreated = Instance::shell(pid);
                recreated.id = iid;
                recreated.tmux_session = Some(target.name);
                recreated.tmux_socket = target.socket;
                app.workspace.add_instance(recreated);
                app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(iid));
                app.close_instance_inner(iid, "close (replacement intent)", cx);
                let project = app.workspace.project(pid).unwrap();
                assert_eq!(project.closed_sessions.len(), 1);
                let second = project.closed_sessions[0].local_close_token.unwrap();
                assert_ne!(
                    first, second,
                    "an old in-flight job discarded the replacement's new durable close"
                );
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    saved.project(pid).unwrap().closed_sessions[0].local_close_token,
                    Some(second)
                );
            })
            .unwrap();
    });
}

#[cfg(unix)]
struct OwnedTmux(String);

#[cfg(unix)]
impl Drop for OwnedTmux {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .output();
    }
}

#[cfg(unix)]
#[test]
fn a_saved_local_kill_finishes_after_its_app_window_is_gone() {
    let socket = format!("muxel-close-test-{}", Uuid::new_v4());
    let _server = OwnedTmux(socket.clone());
    let session = "close-departure-fixture";
    let launched = std::process::Command::new("tmux")
        .args([
            "-L",
            &socket,
            "new-session",
            "-d",
            "-s",
            session,
            "sleep 120",
        ])
        .output()
        .expect("tmux is required for Unix CI");
    assert!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    with_app(|cx, window, pid, iid| {
        let (tasks, document) = window
            .update(cx, |app, window, cx| {
                let inst = app.workspace.instance_mut(iid).unwrap();
                inst.tmux_session = Some(session.into());
                inst.tmux_socket = Some(muxel_core::tmux::Socket::Named(socket.clone()));
                app.close_instance_inner(iid, "close (departure test)", cx);
                let document =
                    muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                // Match on_app_quit: retain jobs while dropping all window roots.
                let tasks = std::mem::take(&mut app.close_tmux_kills);
                window.remove_window();
                (tasks, document)
            })
            .unwrap();
        cx.run_until_parked();
        assert!(tasks.iter().all(gpui::Task::is_ready));
        assert!(
            !std::process::Command::new("tmux")
                .args(["-L", &socket, "has-session", "-t", &format!("={session}")])
                .output()
                .unwrap()
                .status
                .success()
        );
        // No app remains to clear the confirmation. Restart can confirm again.
        let saved = muxel_store::load_workspace_from(&document).unwrap();
        assert_eq!(saved.project(pid).unwrap().closed_sessions.len(), 1);
    });
}

#[cfg(unix)]
#[test]
fn local_activation_retries_a_persisted_kill_on_its_recorded_server() {
    let socket = format!("muxel-close-test-{}", Uuid::new_v4());
    let _server = OwnedTmux(socket.clone());
    let session = "close-retry-fixture";
    let launched = std::process::Command::new("tmux")
        .args([
            "-L",
            &socket,
            "new-session",
            "-d",
            "-s",
            session,
            "sleep 120",
        ])
        .output()
        .expect("tmux is required for Unix CI");
    assert!(
        launched.status.success(),
        "{}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let token = Uuid::new_v4();
    tmux_fixture(
        &socket,
        &[
            "set-option",
            "-t",
            &format!("={session}:"),
            muxel_core::tmux::LOCAL_CLOSE_GENERATION_OPTION,
            &token.to_string(),
        ],
    );
    with_app(|cx, window, pid, iid| {
        window
            .update(cx, |app, _, _| {
                // This is exactly the durable state after removal was saved but
                // before the earlier process received a kill confirmation.
                app.workspace.remove_instance_meta(iid);
                app.workspace.project_mut(pid).unwrap().layout = None;
                app.workspace
                    .project_mut(pid)
                    .unwrap()
                    .remember_closed_session(
                        muxel_core::tmux::SessionTarget::new(
                            session,
                            Some(muxel_core::tmux::Socket::Named(socket.clone())),
                        ),
                        iid,
                    );
                app.workspace.project_mut(pid).unwrap().closed_sessions[0].local_close_token =
                    Some(token);
                assert!(app.try_persist());
            })
            .unwrap();
        window
            .update(cx, |app, window, cx| {
                app.ensure_project_terminals(pid, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        window
            .read_with(cx, |app, _| {
                assert!(
                    app.workspace
                        .project(pid)
                        .unwrap()
                        .closed_sessions
                        .is_empty()
                );
                assert!(app.local_closes_inflight.is_empty());
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                )
                .unwrap();
                assert!(saved.project(pid).unwrap().closed_sessions.is_empty());
            })
            .unwrap();
        assert!(
            !std::process::Command::new("tmux")
                .args(["-L", &socket, "has-session", "-t", &format!("={session}")])
                .output()
                .unwrap()
                .status
                .success()
        );
    });
}

#[cfg(unix)]
fn tmux_fixture(socket: &str, args: &[&str]) -> String {
    let output = std::process::Command::new("tmux")
        .args(["-L", socket])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "tmux {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[cfg(unix)]
#[test]
fn review_close_retry_preserves_peer_replacements_including_a_restarted_server() {
    for (restart_server, session) in [
        (false, "peer-replacement-fixture"),
        (true, "peer-replacement-fixture"),
        (false, "peer-replacement-café"),
        (true, "peer-replacement-café"),
    ] {
        let socket = format!("muxel-close-test-{}", Uuid::new_v4());
        let _server = OwnedTmux(socket.clone());
        let token = Uuid::new_v4();
        tmux_fixture(&socket, &["new-session", "-d", "-s", session, "sleep 120"]);
        let original_id = tmux_fixture(
            &socket,
            &[
                "display-message",
                "-p",
                "-t",
                &format!("={session}:"),
                "#{session_id}",
            ],
        );
        tmux_fixture(
            &socket,
            &[
                "set-option",
                "-t",
                &format!("={session}:"),
                muxel_core::tmux::LOCAL_CLOSE_GENERATION_OPTION,
                &token.to_string(),
            ],
        );
        if restart_server {
            tmux_fixture(&socket, &["kill-server"]);
        } else {
            tmux_fixture(
                &socket,
                &["new-session", "-d", "-s", "sentinel", "sleep 120"],
            );
            tmux_fixture(&socket, &["kill-session", "-t", &format!("={session}")]);
        }
        tmux_fixture(&socket, &["new-session", "-d", "-s", session, "sleep 120"]);
        let replacement_id = tmux_fixture(
            &socket,
            &[
                "display-message",
                "-p",
                "-t",
                &format!("={session}:"),
                "#{session_id}",
            ],
        );
        assert_eq!(
            original_id == replacement_id,
            restart_server,
            "fixture must exercise id reuse after server restart"
        );
        with_app(|cx, window, pid, iid| {
            window
                .update(cx, |app, window, cx| {
                    app.workspace.remove_instance_meta(iid);
                    let project = app.workspace.project_mut(pid).unwrap();
                    project.layout = None;
                    project.remember_closed_session(
                        muxel_core::tmux::SessionTarget::new(
                            session,
                            Some(muxel_core::tmux::Socket::Named(socket.clone())),
                        ),
                        iid,
                    );
                    project.closed_sessions[0].local_close_token = Some(token);
                    assert!(app.try_persist());
                    app.ensure_project_terminals(pid, window, cx);
                })
                .unwrap();
            cx.run_until_parked();
            tmux_fixture(&socket, &["has-session", "-t", &format!("={session}")]);
            assert_eq!(
                tmux_fixture(
                    &socket,
                    &[
                        "display-message",
                        "-p",
                        "-t",
                        &format!("={session}:"),
                        "#{session_id}"
                    ]
                ),
                replacement_id
            );
            window
                .read_with(cx, |app, _| {
                    assert!(
                        app.workspace
                            .project(pid)
                            .unwrap()
                            .closed_sessions
                            .is_empty()
                    );
                    assert!(app.local_closes_inflight.is_empty());
                })
                .unwrap();
        });
    }
}
