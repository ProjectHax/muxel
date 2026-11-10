//! App wiring without a real PTY: runs on every desktop test host.
use super::MuxelApp;
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::recoverable_close::{ExpiredCloseSync, LayoutWait, expired_close_sync};
use muxel_core::{Instance, PaneNode, Project};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// `test_support::set_root` doesn't move tmux's socket, so `Socket::Private`
/// here would be the developer's real server.
fn fixture_socket() -> muxel_core::tmux::Socket {
    muxel_core::tmux::Socket::Named(format!("muxel-close-test-{}", Uuid::new_v4()))
}

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
                    Some(fixture_socket()),
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
                inst.tmux_socket = Some(fixture_socket());
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
                        Some(fixture_socket()),
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
                let target =
                    muxel_core::tmux::SessionTarget::new("recreated-pane", Some(fixture_socket()));
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

/// `kill-server` can return while the old server is still exiting, and a
/// client that connects in that gap fails with "server exited unexpectedly".
/// So wait until tmux says no server is running, as `Server::stop` in
/// integrations.rs does, before anything starts another.
#[cfg(unix)]
fn kill_tmux_server(socket: &str) {
    tmux_fixture(socket, &["kill-server"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let out = std::process::Command::new("tmux")
            .args(["-L", socket, "list-sessions"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if muxel_core::tmux::confirmed_no_server(&stderr) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "tmux server still answering 5 s after kill-server: {stderr}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
            kill_tmux_server(&socket);
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

#[test]
fn restore_shows_unused_registered_worktrees_and_leaves_used_ones() {
    with_app(|cx, window, _, _| {
        window
            .update(cx, |app, window, cx| {
                let root = muxel_store::data_dir().unwrap();
                let mut workspace = muxel_core::Workspace::default();
                let empty = workspace.add_project(Project::new("empty", root.join("empty")));
                let used_project = workspace.add_project(Project::new("used", root.join("used")));
                let orphan = Uuid::new_v4();
                let used = Uuid::new_v4();
                for (id, pid, name) in [(orphan, empty, "orphan"), (used, used_project, "used")] {
                    let path = root.join(format!("load-{name}-worktree"));
                    std::fs::create_dir_all(&path).unwrap();
                    std::fs::write(path.join("draft.txt"), "keep registered files").unwrap();
                    workspace.add_worktree(muxel_core::Worktree {
                        id,
                        project_id: pid,
                        name: name.into(),
                        path,
                        branch: format!("muxel/{name}"),
                        color: 0,
                        detached: false,
                    });
                }
                let mut pane = Instance::shell(used_project);
                pane.worktree_id = Some(used);
                pane.worktree_path = Some(workspace.worktree(used).unwrap().path.clone());
                pane.worktree_branch = Some("muxel/used".into());
                workspace.project_mut(used_project).unwrap().layout = Some(PaneNode::leaf(pane.id));
                workspace.add_instance(pane);
                workspace.active_project = Some(empty);
                let document =
                    muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                muxel_store::save_workspace_to(&document, &workspace).unwrap();
                app.restore(
                    muxel_store::load_workspace_from(&document).unwrap(),
                    window,
                    cx,
                );
                assert!(
                    app.workspace.worktree(orphan).unwrap().detached,
                    "load hid a registered worktree with no panes"
                );
                assert!(!app.workspace.worktree(used).unwrap().detached);
                let saved = muxel_store::load_workspace_from(&document).unwrap();
                assert!(saved.worktree(orphan).unwrap().detached);
                assert!(!saved.worktree(used).unwrap().detached);
                for tree in &saved.worktrees {
                    assert_eq!(
                        std::fs::read_to_string(tree.path.join("draft.txt")).unwrap(),
                        "keep registered files"
                    );
                }
            })
            .unwrap();
    });
}

#[test]
fn an_unconfirmed_remote_close_cannot_stop_a_project_being_removed() {
    with_app(|cx, window, pid, iid| {
        window
            .update(cx, |app, _, cx| {
                app.workspace.remove_instance_meta(iid);
                let project = app.workspace.project_mut(pid).unwrap();
                project.layout = None;
                // A host that no longer exists can never confirm this.
                project.remote = Some(muxel_core::RemoteRef {
                    host_id: Uuid::new_v4(),
                    remote_root: "/gone".into(),
                });
                project.remember_closed_session(
                    muxel_core::tmux::SessionTarget::new("gone-session", Some(fixture_socket())),
                    iid,
                );
                app.delete_project(pid, cx);
                assert!(
                    app.workspace.project(pid).is_none(),
                    "a dead host made the project impossible to remove"
                );
                assert!(
                    app.notifications
                        .iter()
                        .any(|notice| notice.title.contains("may still be running")),
                    "the removal has to say the session may have survived"
                );
            })
            .unwrap();
    });
}

#[cfg(unix)]
fn start_tmux_session(socket: &str, session: &str) {
    let launched = std::process::Command::new("tmux")
        .args([
            "-L",
            socket,
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
}

#[cfg(unix)]
fn tmux_session_alive(socket: &str, session: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["-L", socket, "has-session", "-t", &format!("={session}")])
        .output()
        .unwrap()
        .status
        .success()
}

#[cfg(unix)]
#[test]
fn removing_a_project_still_kills_its_tmux_sessions() {
    let socket = format!("muxel-close-test-{}", Uuid::new_v4());
    let _server = OwnedTmux(socket.clone());
    let session = "close-removal-fixture";
    start_tmux_session(&socket, session);
    with_app(|cx, window, pid, iid| {
        window
            .update(cx, |app, _, cx| {
                let inst = app.workspace.instance_mut(iid).unwrap();
                inst.tmux_session = Some(session.into());
                inst.tmux_socket = Some(muxel_core::tmux::Socket::Named(socket.clone()));
                app.delete_project(pid, cx);
                assert!(
                    app.workspace.project(pid).is_none(),
                    "removal failed the first time"
                );
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            !tmux_session_alive(&socket, session),
            "removal lost the kill, and the agent kept running"
        );
    });
}

#[cfg(unix)]
fn git_fixture(root: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Muxel test",
            "-c",
            "user.email=muxel-test@example.invalid",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("run git inside disposable fixture");
    assert!(
        output.status.success(),
        "fixture git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Ryan's report, on real tmux: the agent and shell of a clean worktree close
/// together. Whichever kill confirms first sees the other's entry.
#[cfg(unix)]
#[test]
fn closing_both_tmux_panes_of_a_clean_worktree_removes_it() {
    let socket = format!("muxel-close-test-{}", Uuid::new_v4());
    let _server = OwnedTmux(socket.clone());
    start_tmux_session(&socket, "close-worktree-agent");
    start_tmux_session(&socket, "close-worktree-shell");
    with_app(|cx, window, pid, iid| {
        let (wid, path) = window
            .update(cx, |app, _, cx| {
                let root = app.workspace.project(pid).unwrap().root_path.clone();
                git_fixture(&root, &["init", "--initial-branch=main"]);
                git_fixture(&root, &["commit", "--allow-empty", "-m", "fixture"]);
                let path = root.join("close-worktree");
                git_fixture(
                    &root,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        "muxel/close-both",
                        path.to_str().unwrap(),
                    ],
                );
                let wid = Uuid::new_v4();
                app.workspace.add_worktree(muxel_core::Worktree {
                    id: wid,
                    project_id: pid,
                    name: "close both".into(),
                    path: path.clone(),
                    branch: "muxel/close-both".into(),
                    color: 0,
                    detached: false,
                });
                let mut second = Instance::shell(pid);
                second.tmux_session = Some("close-worktree-shell".into());
                second.tmux_socket = Some(muxel_core::tmux::Socket::Named(socket.clone()));
                second.worktree_id = Some(wid);
                second.worktree_path = Some(path.clone());
                let second_id = second.id;
                app.workspace.add_instance(second);
                let agent = app.workspace.instance_mut(iid).unwrap();
                agent.tmux_session = Some("close-worktree-agent".into());
                agent.tmux_socket = Some(muxel_core::tmux::Socket::Named(socket.clone()));
                agent.worktree_id = Some(wid);
                agent.worktree_path = Some(path.clone());
                app.notifications.clear();
                app.close_instance_inner(iid, "close (agent)", cx);
                app.close_instance_inner(second_id, "close (shell)", cx);
                (wid, path)
            })
            .unwrap();
        cx.run_until_parked();
        assert!(!tmux_session_alive(&socket, "close-worktree-agent"));
        assert!(!tmux_session_alive(&socket, "close-worktree-shell"));
        window
            .update(cx, |app, _, _| {
                assert!(
                    app.notifications.is_empty(),
                    "a normal close showed a false error"
                );
                assert!(
                    app.workspace.worktree(wid).is_none(),
                    "a clean worktree was kept"
                );
            })
            .unwrap();
        assert!(!path.exists(), "a clean worktree stayed on disk");
    });
}
