use super::{
    Instance, MuxelApp, PaneNode, Project, RemoteHost, RemoteRestartCompletion, SecondaryWindow,
    WorkspaceWindow,
};
use crate::theme;
use gpui::{AnyWindowHandle, TestAppContext, WindowHandle};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

struct StopChild {
    session: Arc<muxel_terminal::TerminalSession>,
    process: HANDLE,
}

impl StopChild {
    fn new(session: Arc<muxel_terminal::TerminalSession>) -> Self {
        // SAFETY: this PID belongs to the fixture just spawned. Retain its
        // process handle so PID reuse cannot prove liveness.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                session.child_pid().expect("replacement child PID"),
            )
        }
        .expect("open owned replacement process");
        Self { session, process }
    }

    fn assert_running(&self) {
        // SAFETY: the fixture's retained handle stays open until this guard drops.
        assert_eq!(
            unsafe { WaitForSingleObject(self.process, 0) },
            WAIT_TIMEOUT
        );
    }
}

impl Drop for StopChild {
    fn drop(&mut self) {
        self.session.kill();
        // SAFETY: this guard exclusively owns the retained process handle.
        let _ = unsafe { CloseHandle(self.process) };
    }
}

fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>)) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.wip/restart-completion-tests")
        .join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let _storage = muxel_store::test_support::set_root(root.clone());
    assert_eq!(muxel_store::data_dir(), Some(root.join("data")));
    assert_eq!(muxel_store::config_dir(), Some(root.join("config")));
    let mut cx = TestAppContext::single();
    cx.skip_drawing();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let main = cx.add_window(MuxelApp::new);
    let app = main.entity(&cx).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut cx, main)));
    // A failing regression must stop any replacement it created before the
    // scoped root disappears, including a failure before a StopChild is retained.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.update(&mut cx, |app, cx| {
            for view in app.terminals.values() {
                view.read(cx).session().kill();
            }
        });
    }));
    drop(app);
    drop(cx);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Pause immediately after remote I/O has started and the old PTY was removed.
/// Completion is injected through the production continuation, without an SSH
/// server; its replacement runs only an inert local PTY fixture.
fn pending_secondary_restart(
    cx: &mut TestAppContext,
    main: WindowHandle<MuxelApp>,
) -> (RemoteRestartCompletion, WindowHandle<WorkspaceWindow>) {
    let completion = main
        .update(cx, |app, _, _| {
            let root = muxel_store::data_dir().unwrap().join("project");
            std::fs::create_dir_all(&root).unwrap();
            let project = app.workspace.add_project(Project::new("restart", &root));
            let other = app.workspace.add_project(Project::new("other", &root));
            let mut instance = Instance::shell(project);
            instance.program = Some("cmd.exe".into());
            instance.args = ["/d", "/q", "/c", "more"].map(String::from).into();
            let iid = instance.id;
            let token = app.terminal_restarts.begin(&instance).unwrap();
            app.workspace.add_instance(instance);
            app.workspace.project_mut(project).unwrap().layout = Some(PaneNode::leaf(iid));
            app.workspace.active_project = Some(other);
            app.main_window = Some(main.into());
            app.use_tmux = false;
            app.use_worktree = false;
            RemoteRestartCompletion {
                instance: iid,
                project,
                workspace: app.current_workspace,
                token,
                owner_generation: app.terminal_restarts.owner_generation(iid),
                host: "test host".into(),
                session: "test session".into(),
                killed: Ok(()),
            }
        })
        .unwrap();
    let app = main.entity(cx).unwrap();
    let weak = app.downgrade();
    let display_uuid = Uuid::new_v4();
    let project = completion.project;
    let secondary =
        cx.add_window(|window, cx| WorkspaceWindow::new(weak, project, display_uuid, window, cx));
    let view = secondary.entity(cx).unwrap();
    let handle: AnyWindowHandle = secondary.into();
    app.update(cx, |app, _| {
        app.secondary_windows.push(SecondaryWindow {
            pid: project,
            display_uuid,
            window_id: handle.window_id(),
            handle,
            view,
            active: true,
        });
        assert_eq!(app.window_showing(project), Some(handle));
    });
    (completion, secondary)
}

fn close_secondary_while_pending(
    cx: &mut TestAppContext,
    main: WindowHandle<MuxelApp>,
    secondary: WindowHandle<WorkspaceWindow>,
    completion: &RemoteRestartCompletion,
) {
    secondary
        .update(cx, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(
        super::RESTORE_FIRST_WAVE_DEBOUNCE_MS + 1,
    ));
    cx.run_until_parked();
    assert!(secondary.read_with(cx, |_, _| ()).is_err());
    main.read_with(cx, |app, _| {
        // The actual close observer returned the project to main. Its automatic
        // restore ran while the pending generation blocked launch admission.
        assert!(app.secondary_windows.is_empty());
        assert_eq!(app.workspace.active_project, Some(completion.project));
        assert!(
            app.deferred_activation_generation
                .contains_key(&completion.project)
        );
        assert!(app.terminal_restarts.contains(completion.instance));
        assert!(!app.terminals.contains_key(&completion.instance));
        assert!(!app.terminal_launching.contains_key(&completion.instance));
    })
    .unwrap();
}

#[test]
fn remote_restart_completion_follows_closed_secondary_to_main() {
    with_app(|cx, main| {
        let (completion, secondary) = pending_secondary_restart(cx, main);
        let iid = completion.instance;
        close_secondary_while_pending(cx, main, secondary, &completion);
        let app = main.entity(cx).unwrap();
        MuxelApp::complete_remote_restart(&app.downgrade(), completion, cx);
        cx.run_until_parked();
        let session = main
            .read_with(cx, |app, cx| {
                assert!(!app.terminal_restarts.contains(iid));
                assert_eq!(app.terminals.len(), 1);
                app.terminals
                    .get(&iid)
                    .expect(
                        "restart must install its replacement in main without another user action",
                    )
                    .read(cx)
                    .session()
                    .clone()
            })
            .unwrap();
        let child = StopChild::new(session);
        child.assert_running();
    });
}

#[test]
fn stale_remote_restart_completion_leaves_new_generation_pending() {
    with_app(|cx, main| {
        let (completion, secondary) = pending_secondary_restart(cx, main);
        let iid = completion.instance;
        close_secondary_while_pending(cx, main, secondary, &completion);
        let app = main.entity(cx).unwrap();
        let next = app.update(cx, |app, _| {
            app.terminal_restarts.cancel(iid);
            app.terminal_restarts
                .begin(app.workspace.instance(iid).unwrap())
                .unwrap()
        });
        MuxelApp::complete_remote_restart(&app.downgrade(), completion, cx);
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(app.terminal_restarts.current(iid, next));
            assert!(app.terminals.is_empty());
        });
    });
}

#[test]
fn push_move_cancels_restart_and_discards_its_late_kill_failure() {
    use muxel_core::tmux::Socket;

    with_app(|cx, main| {
        let (mut completion, _secondary) = pending_secondary_restart(cx, main);
        let iid = completion.instance;
        let name = "peer-owned".to_string();
        let (socket, failed, restarting) = main
            .update(cx, |app, window, cx| {
                let instance = app.workspace.instance_mut(iid).unwrap();
                instance.tmux_session = Some(name.clone());
                instance.tmux_socket = Some(Socket::Default);
                app.terminal_restarts.cancel(iid);
                completion.token = app
                    .terminal_restarts
                    .begin(app.workspace.instance(iid).unwrap())
                    .unwrap();
                app.failed_launches.insert(iid, "old binding failed".into());

                app.take_moves_off_default(completion.project, &[(iid, name)], window, cx);

                (
                    app.workspace.instance(iid).unwrap().tmux_socket.clone(),
                    app.failed_launches.contains_key(&iid),
                    app.terminal_restarts.contains(iid),
                )
            })
            .unwrap();
        assert_eq!(socket, Some(Socket::Private));
        assert!(!failed);
        assert!(
            !restarting,
            "the peer's new binding needs launch admission immediately"
        );

        completion.killed = Err(anyhow::anyhow!("old Default kill failed"));
        let app = main.entity(cx).unwrap();
        MuxelApp::complete_remote_restart(&app.downgrade(), completion, cx);
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.workspace.instance(iid).unwrap().tmux_socket,
                Some(Socket::Private)
            );
            assert!(!app.failed_launches.contains_key(&iid));
        });
    });
}

#[test]
fn rebound_adoption_cancels_restart_before_retrying_launch() {
    use muxel_core::tmux::Socket;

    with_app(|cx, main| {
        let (completion, _secondary) = pending_secondary_restart(cx, main);
        let iid = completion.instance;
        let (failed, restarting) = main
            .update(cx, |app, _, _| {
                let mut before = app.workspace.instance(iid).unwrap().clone();
                before.tmux_session = Some("peer-owned".into());
                before.tmux_socket = Some(Socket::Default);
                let mut after = before.clone();
                after.tmux_socket = Some(Socket::Private);
                app.failed_launches.insert(iid, "old binding failed".into());

                app.retry_rebound_launches(&[before], &[after]);

                (
                    app.failed_launches.contains_key(&iid),
                    app.terminal_restarts.contains(iid),
                )
            })
            .unwrap();
        assert!(!failed);
        assert!(!restarting);
    });
}

#[test]
fn connect_inventory_rebinding_cancels_delayed_restart_failure() {
    use muxel_core::tmux::{RemoteSession, Socket};

    with_app(|cx, main| {
        let (mut completion, _secondary) = pending_secondary_restart(cx, main);
        let iid = completion.instance;
        let (socket, restarting, unchanged_restart, unchanged_failure) = main
            .update(cx, |app, _, _| {
                let instance = app.workspace.instance_mut(iid).unwrap();
                instance.tmux_session = Some("legacy".into());
                instance.tmux_socket = None;
                instance.use_tmux = true;
                app.terminal_restarts.cancel(iid);
                completion.token = app
                    .terminal_restarts
                    .begin(app.workspace.instance(iid).unwrap())
                    .unwrap();
                completion.owner_generation = app.terminal_restarts.owner_generation(iid);
                app.failed_launches
                    .insert(iid, "old Default lookup failed".into());

                let mut unchanged = Instance::shell(completion.project);
                unchanged.tmux_session = Some("unchanged".into());
                unchanged.tmux_socket = Some(Socket::Private);
                unchanged.use_tmux = true;
                let unchanged_id = unchanged.id;
                let unchanged_token = app.terminal_restarts.begin(&unchanged).unwrap();
                app.workspace.add_instance(unchanged);
                app.failed_launches
                    .insert(unchanged_id, "existing failure".into());

                let host = RemoteHost::new("remote", "example.test");
                let sessions = ["legacy", "unchanged"].map(|name| RemoteSession {
                    name: name.into(),
                    socket: Socket::Private,
                    path: "/remote".into(),
                    command: "claude".into(),
                    title: String::new(),
                });
                app.bind_connect_session_owners(completion.project, &host, &sessions);
                (
                    app.workspace.instance(iid).unwrap().tmux_socket.clone(),
                    app.terminal_restarts.contains(iid),
                    app.terminal_restarts.current(unchanged_id, unchanged_token),
                    app.failed_launches.contains_key(&unchanged_id),
                )
            })
            .unwrap();
        assert_eq!(socket, Some(Socket::Private));
        assert!(unchanged_restart);
        assert!(unchanged_failure);
        assert!(
            !restarting,
            "the new owner must not inherit the old restart lease"
        );

        completion.killed = Err(anyhow::anyhow!("delayed Default owner lookup failed"));
        let app = main.entity(cx).unwrap();
        MuxelApp::complete_remote_restart(&app.downgrade(), completion, cx);
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.workspace.instance(iid).unwrap().tmux_socket,
                Some(Socket::Private)
            );
            assert!(!app.failed_launches.contains_key(&iid));
        });
    });
}

#[test]
fn unrelated_peer_adoption_keeps_restarting_panes_replacement() {
    use muxel_core::RemoteLayout;
    use muxel_core::tmux::Socket;

    with_app(|cx, main| {
        let (mut completion, _secondary) = pending_secondary_restart(cx, main);
        let iid = completion.instance;
        let pid = completion.project;
        let peer = main
            .update(cx, |app, _, _| {
                let restarting = app.workspace.instance_mut(iid).unwrap();
                restarting.tmux_session = Some("restarting".into());
                restarting.tmux_socket = Some(Socket::Private);
                app.terminal_restarts.cancel(iid);
                completion.token = app
                    .terminal_restarts
                    .begin(app.workspace.instance(iid).unwrap())
                    .unwrap();

                let mut other = Instance::shell(pid);
                other.program = Some("cmd.exe".into());
                other.args = ["/d", "/q", "/c", "more"].map(String::from).into();
                other.tmux_session = Some("other".into());
                other.tmux_socket = Some(Socket::Default);
                let other_id = other.id;
                app.workspace.add_instance(other);
                let project = app.workspace.project_mut(pid).unwrap();
                assert!(muxel_core::add_tab(&mut project.layout, iid, other_id));
                project.layout_updated_at = Some(1);

                let mut peer =
                    RemoteLayout::capture(app.workspace.project(pid).unwrap(), &app.workspace, 2);
                peer.updated_at = 2;
                peer.instances
                    .iter_mut()
                    .find(|instance| instance.id == other_id)
                    .unwrap()
                    .tmux_socket = Some(Socket::Private);
                peer
            })
            .unwrap();

        main.update(cx, |app, window, cx| {
            app.adopt_peer_layout(pid, peer, window, cx);
            assert_eq!(
                app.workspace.instance(iid).unwrap().tmux_socket,
                Some(Socket::Private)
            );
            assert!(app.terminal_restarts.current(iid, completion.token));
        })
        .unwrap();

        completion.killed = Ok(());
        let app = main.entity(cx).unwrap();
        MuxelApp::complete_remote_restart(&app.downgrade(), completion, cx);
        cx.run_until_parked();
        let session = main
            .read_with(cx, |app, cx| {
                app.terminals
                    .get(&iid)
                    .expect("unchanged owner needs its restarted replacement")
                    .read(cx)
                    .session()
                    .clone()
            })
            .unwrap();
        let replacement = StopChild::new(session);
        replacement.assert_running();
    });
}

#[test]
fn local_restart_failed_owner_save_never_kills_or_respawns() {
    with_app(|cx, main| {
        main.update(cx, |app, window, cx| {
            let root = muxel_store::data_dir().unwrap().join("project");
            std::fs::create_dir_all(&root).unwrap();
            let project = app.workspace.add_project(Project::new("local", &root));
            let mut instance = Instance::shell(project);
            instance.tmux_session = Some("must-not-be-killed".into());
            instance.tmux_socket = Some(muxel_core::tmux::Socket::Private);
            let iid = instance.id;
            app.workspace.add_instance(instance);
            app.workspace.project_mut(project).unwrap().layout = Some(PaneNode::leaf(iid));
            app.workspace.active_project = Some(project);
            app.current_workspace = Some(Uuid::new_v4());
            // A directory at the destination makes the actual atomic save fail.
            let path = muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
            std::fs::create_dir_all(&path).unwrap();

            app.restart_agent(iid, window, cx);

            assert!(!app.terminal_restarts.contains(iid));
            assert!(app.terminals.is_empty());
            assert!(app.terminal_launching.is_empty());
            assert!(app.terminal_launches.is_empty());
            assert!(
                app.failed_launches[&iid]
                    .contains("couldn't save the tmux session owner before restart")
            );
            assert!(
                app.reserve_terminal_spawn(
                    iid,
                    muxel_core::TerminalLaunchReason::Restore,
                    window,
                    cx,
                )
                .is_none(),
                "automatic restore cannot retry a failed restart"
            );
        })
        .unwrap();
    });
}

#[test]
fn failed_restart_completion_does_not_reattach_the_old_agent() {
    with_app(|cx, main| {
        let (mut completion, secondary) = pending_secondary_restart(cx, main);
        let iid = completion.instance;
        close_secondary_while_pending(cx, main, secondary, &completion);
        completion.killed = Err(anyhow::anyhow!("owner save failed"));
        let app = main.entity(cx).unwrap();
        MuxelApp::complete_remote_restart(&app.downgrade(), completion, cx);
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(!app.terminal_restarts.contains(iid));
            assert!(app.terminals.is_empty());
            assert!(app.terminal_launching.is_empty());
            assert!(app.failed_launches[&iid].contains("owner save failed"));
        });
    });
}

#[test]
fn peer_migration_collision_stops_restart_without_changing_owner() {
    use muxel_core::tmux::{SessionTarget, Socket};

    with_app(|cx, main| {
        let (mut completion, secondary) = pending_secondary_restart(cx, main);
        let iid = completion.instance;
        let target = SessionTarget::new("owned", Some(Socket::Default));
        let app = main.entity(cx).unwrap();
        app.update(cx, |app, _| {
            let instance = app.workspace.instance_mut(iid).unwrap();
            instance.tmux_session = Some(target.name.clone());
            instance.tmux_socket = target.socket.clone();
            assert!(app.terminal_restarts.current(iid, completion.token));
        });
        // The first peer has already replaced Default on Private. Exercise the
        // production conversion from checked inventory to restart failure.
        completion.killed = crate::integrations::confirmed_tmux_respawn_target(&target, |socket| {
            Ok(socket == &Socket::Private)
        })
        .map(|_| ());
        assert!(completion.killed.is_err());
        close_secondary_while_pending(cx, main, secondary, &completion);
        MuxelApp::complete_remote_restart(&app.downgrade(), completion, cx);
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(!app.terminal_restarts.contains(iid));
            assert!(app.terminals.is_empty());
            assert!(app.terminal_launching.is_empty());
            assert!(app.terminal_launches.is_empty());
            let instance = app.workspace.instance(iid).unwrap();
            assert_eq!(instance.tmux_socket, Some(Socket::Default));
            assert_eq!(instance.tmux_session.as_deref(), Some("owned"));
            assert!(app.failed_launches[&iid].contains("restart the pane"));
        });
    });
}

#[test]
fn failed_migration_save_keeps_the_expected_default_owner_and_restart_lease() {
    with_app(|cx, main| {
        main.update(cx, |app, _, _| {
            let project = app
                .workspace
                .add_project(Project::new("migration", muxel_store::data_dir().unwrap()));
            let mut instance = Instance::shell(project);
            instance.tmux_session = Some("owned".into());
            instance.tmux_socket = Some(muxel_core::tmux::Socket::Default);
            let iid = instance.id;
            let token = app.terminal_restarts.begin(&instance).unwrap();
            app.workspace.add_instance(instance);
            app.current_workspace = Some(Uuid::new_v4());
            std::fs::create_dir_all(
                muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
            )
            .unwrap();
            let old = muxel_core::tmux::SessionTarget::new(
                "owned",
                Some(muxel_core::tmux::Socket::Default),
            );
            let next = muxel_core::tmux::SessionTarget::new(
                "owned",
                Some(muxel_core::tmux::Socket::Private),
            );
            assert!(app.save_restart_migration(iid, token, &old, &next).is_err());
            assert_eq!(app.workspace.instance(iid).unwrap().tmux_socket, old.socket);
            assert!(app.terminal_restarts.current(iid, token));
            assert!(app.terminals.is_empty());
        })
        .unwrap();
    });
}
