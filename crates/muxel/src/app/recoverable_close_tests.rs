#![cfg(windows)]

use super::popout_lifecycle_tests::with_app;
use super::{
    ConfirmAction, EditorSnapshot, Instance, MuxelApp, NotifKind, Project, SaveTarget, Workspace,
    WorkspaceMeta,
};
use gpui::{AppContext, Entity, Focusable, TestAppContext, VisualTestContext, WindowHandle};
use muxel_core::control::ErrorCode;
use muxel_core::{PaneNode, Worktree, add_tab};
use muxel_terminal::{
    AgentStatus, CommandSpec, TerminalLaunch, TerminalSession, TerminalStatusReason, TerminalView,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    TerminateProcess, WaitForSingleObject,
};

/// Keep the original process handle: an unrelated process reusing its PID must
/// never count as either survival or successful teardown of this fixture.
struct OwnedChild {
    session: Arc<TerminalSession>,
    process: HANDLE,
}

impl OwnedChild {
    fn new(session: Arc<TerminalSession>) -> Self {
        // SAFETY: the session owns the process just spawned by this fixture.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                session.child_pid().expect("owned fixture child PID"),
            )
        }
        .expect("retain owned fixture process handle");
        Self { session, process }
    }

    fn exited(&self) -> bool {
        // SAFETY: this handle stays open until the guard is dropped.
        let status = unsafe { WaitForSingleObject(self.process, 0) };
        if status == WAIT_OBJECT_0 {
            true
        } else {
            assert_eq!(status, WAIT_TIMEOUT, "query owned fixture process");
            false
        }
    }

    fn terminate(&self, exit_code: u32) {
        // SAFETY: this is the retained handle of this fixture's own child.
        // Selecting its actual OS exit code exercises both clean and abnormal
        // exit paths without changing TerminalView's runtime fields.
        unsafe { TerminateProcess(self.process, exit_code) }.expect("terminate owned fixture");
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.session.kill();
        // SAFETY: this guard exclusively owns this retained process handle.
        let _ = unsafe { CloseHandle(self.process) };
    }
}

fn project(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Uuid {
    window
        .update(cx, |app, _, _| {
            let root = muxel_store::data_dir()
                .unwrap()
                .join(Uuid::new_v4().to_string());
            std::fs::create_dir_all(&root).unwrap();
            let pid = app
                .workspace
                .add_project(Project::new("close lifecycle", root));
            app.workspace.active_project = Some(pid);
            app.current_workspace = Some(Uuid::new_v4());
            app.use_tmux = false;
            app.use_worktree = false;
            app.settings.control_enabled = true;
            app.settings.control_allow_shells = true;
            app.settings.confirm_close_terminal = true;
            pid
        })
        .unwrap()
}

fn terminal(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pid: Uuid,
) -> (Uuid, Entity<TerminalView>, OwnedChild) {
    window
        .update(cx, |app, window, cx| {
            let mut instance = Instance::shell(pid);
            instance.program = Some("cmd.exe".into());
            instance.args = ["/d", "/q", "/c", "more"]
                .into_iter()
                .map(str::to_string)
                .collect();
            let iid = instance.id;
            let launch = TerminalLaunch::spawn(
                CommandSpec::program("cmd.exe", instance.args.clone()),
                (80, 24),
            )
            .expect("spawn owned waiting command");
            let view = cx.new(|cx| TerminalView::new(launch, iid, Instant::now(), window, cx));
            let child = OwnedChild::new(view.read(cx).session().clone());
            app.workspace.add_instance(instance);
            let project = app.workspace.project_mut(pid).unwrap();
            if let Some(anchor) = project.first_instance() {
                assert!(add_tab(&mut project.layout, anchor, iid));
            } else {
                project.layout = Some(PaneNode::leaf(iid));
            }
            app.terminals.insert(iid, view.clone());
            app.active_instance = Some(iid);
            assert!(!child.exited(), "fixture must start alive");
            (iid, view, child)
        })
        .unwrap()
}

fn wait_for_exit(cx: &mut TestAppContext, child: &OwnedChild) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if child.exited() {
            return;
        }
        assert!(Instant::now() < deadline, "held process was not finalized");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn idle_agent_close_holds_process_and_undo_restores_tab_order() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (a, _, first) = terminal(cx, window, pid);
        let (b, _, second) = terminal(cx, window, pid);
        let (c, _, survivor) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                // Admission is covered separately. Exercise the actual hold
                // operation with a selection ordered differently from its tabs.
                let group = app
                    .hold_closes(vec![(b, false), (a, false)], None, cx)
                    .unwrap();
                assert_eq!(app.workspace.project(pid).unwrap().instances(), vec![c]);
                for (iid, child) in [(a, &first), (b, &second)] {
                    assert!(app.is_closing(iid));
                    assert!(app.closing_until(iid).is_some());
                    let session = app.terminal_view(iid).unwrap().read(cx).session();
                    assert!(Arc::ptr_eq(session, &child.session));
                    assert!(!child.exited(), "hold killed the process");
                }
                app.undo_close(group, window, cx);
                assert_eq!(
                    app.workspace.project(pid).unwrap().instances(),
                    vec![a, b, c]
                );
                for (iid, child) in [(a, &first), (b, &second), (c, &survivor)] {
                    assert!(!app.is_closing(iid));
                    assert!(app.closing_until(iid).is_none());
                    assert!(Arc::ptr_eq(
                        app.terminal_view(iid).unwrap().read(cx).session(),
                        &child.session,
                    ));
                    assert!(!child.exited());
                }
            })
            .unwrap();
    });
}

#[test]
fn held_panes_stay_out_of_ctl_listings_and_refuse_closing() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (held, _, child) = terminal(cx, window, pid);
        let (visible, _, _survivor) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                assert!(app.control_writable(held, cx).is_ok());
                let group = app.hold_closes(vec![(held, false)], None, cx).unwrap();
                let deadline = app.closing_until(held).unwrap();
                let listed: Vec<_> = app.control_instances().iter().map(|i| i.id).collect();
                assert!(!listed.contains(&held));
                assert!(listed.contains(&visible));
                let error = app
                    .control_writable(held, cx)
                    .err()
                    .expect("held write refused");
                assert_eq!(error.code, Some(ErrorCode::Closing));
                assert_eq!(error.undo_until, Some(deadline));
                let error = app.control_resolve(&held.to_string()).unwrap_err();
                assert_eq!(error.code, Some(ErrorCode::Closing));
                assert_eq!(error.undo_until, Some(deadline));
                assert!(!child.exited());
                app.undo_close(group, window, cx);
                assert!(app.control_writable(held, cx).is_ok());
                assert_eq!(app.control_resolve(&held.to_string()).unwrap(), held);
            })
            .unwrap();
    });
}

#[test]
fn save_during_countdown_restores_held_panes_to_layout() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (a, _, child) = terminal(cx, window, pid);
        let (b, _, _survivor) = terminal(cx, window, pid);
        window
            .update(cx, |app, _, cx| {
                app.hold_closes(vec![(a, false)], None, cx).unwrap();
                let restored = app.workspace_with_closes_restored();
                let bytes = serde_json::to_vec(&restored).unwrap();
                let durable: Workspace = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(durable.project(pid).unwrap().instances(), vec![a, b]);
                assert!(durable.instance(a).is_some());
                assert!(app.try_persist());
                let path = muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                let saved =
                    muxel_store::load_workspace_from(&path).expect("saved countdown workspace");
                assert_eq!(saved.project(pid).unwrap().instances(), vec![a, b]);
                assert_eq!(app.workspace.project(pid).unwrap().instances(), vec![b]);
                assert!(app.is_closing(a), "saving ended the countdown");
                assert!(!child.exited());
            })
            .unwrap();
    });
}

#[test]
fn project_remove_finalizes_its_held_panes_first() {
    with_app(|cx, window| {
        let removed = project(cx, window);
        let (held, _, child) = terminal(cx, window, removed);
        let kept = project(cx, window);
        let (visible, _, survivor) = terminal(cx, window, kept);
        window
            .update(cx, |app, _, cx| {
                app.hold_closes(vec![(held, false)], None, cx).unwrap();
                app.delete_project(removed, cx);
                assert!(app.workspace.project(removed).is_none());
                assert!(app.workspace.instance(held).is_none());
                assert!(!app.is_closing(held));
                assert!(app.terminal_view(held).is_none());
                assert!(app.workspace.project(kept).is_some());
                assert!(app.workspace.instance(visible).is_some());
                assert!(!survivor.exited());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn workspace_switch_finalizes_before_drain() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (held, _, child) = terminal(cx, window, pid);
        let next = Uuid::new_v4();
        let target_project = window
            .update(cx, |app, window, cx| {
                let previous = app.current_workspace.unwrap();
                app.workspaces.workspaces.extend([
                    WorkspaceMeta {
                        id: previous,
                        name: "previous".into(),
                    },
                    WorkspaceMeta {
                        id: next,
                        name: "target".into(),
                    },
                ]);
                let root = muxel_store::data_dir().unwrap().join("target-project");
                std::fs::create_dir_all(&root).unwrap();
                let mut target = Workspace::default();
                let target_project = target.add_project(Project::new("target", root));
                muxel_store::save_workspace_to(
                    &muxel_store::workspace_doc_path(next).unwrap(),
                    &target,
                )
                .unwrap();
                app.hold_closes(vec![(held, false)], None, cx).unwrap();
                app.enter_workspace(next, window, cx);
                assert_eq!(app.current_workspace, Some(next));
                assert!(!app.is_closing(held));
                assert!(app.terminal_view(held).is_none());
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(previous).unwrap(),
                )
                .expect("old workspace persisted before switch");
                assert!(
                    saved.instance(held).is_none(),
                    "switch persisted a held pane as live"
                );
                target_project
            })
            .unwrap();
        cx.run_until_parked();
        window
            .read_with(cx, |app, _| {
                assert!(app.workspace.project(target_project).is_some());
                assert!(app.workspace.instance(held).is_none());
                assert!(!app.is_closing(held));
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn popout_os_close_follows_the_close_rule() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        let popup = window
            .update(cx, |app, window, cx| {
                app.pop_out_instance(pane, window, cx);
                app.popouts[&pane].window
            })
            .unwrap();
        let mut visual = VisualTestContext::from_window(popup.into(), cx);
        // This fixture is a running Windows command with no OSC 133 prompt
        // evidence. Native close must ask, rather than treating output quiet as idle.
        assert!(!visual.simulate_close(), "busy native close must be vetoed");
        cx.run_until_parked();
        window
            .update(cx, |app, _, cx| {
                assert!(
                    app.confirm.is_some(),
                    "OS close did not reach central confirmation"
                );
                assert!(!app.is_closing(pane));
                assert!(!child.exited());
                app.cancel_confirm(cx);
            })
            .unwrap();
        assert!(popup.read_with(cx, |_, _| ()).is_ok());
        assert!(!visual.simulate_close());
        cx.run_until_parked();
        window
            .update(cx, |app, window, cx| {
                assert!(app.confirm.is_some());
                app.run_confirm(window, cx);
                assert!(app.is_closing(pane));
                assert!(
                    !child.exited(),
                    "confirmed busy close must still count down"
                );
            })
            .unwrap();
        cx.run_until_parked();
        assert!(popup.read_with(cx, |_, _| ()).is_err());
        window
            .update(cx, |app, window, cx| {
                assert!(!app.popouts.contains_key(&pane));
                assert!(Arc::ptr_eq(
                    app.terminal_view(pane).unwrap().read(cx).session(),
                    &child.session,
                ));
                let group = app
                    .close_groups
                    .last()
                    .expect("one confirmed close group")
                    .id;
                app.undo_close(group, window, cx);
                assert_eq!(app.workspace.project(pid).unwrap().instances(), vec![pane]);
                assert!(app.terminals.contains_key(&pane));
                assert!(!app.is_closing(pane));
                assert!(Arc::ptr_eq(
                    app.terminal_view(pane).unwrap().read(cx).session(),
                    &child.session,
                ));
                assert!(!child.exited());
            })
            .unwrap();
    });
}

#[test]
fn forced_close_skips_expiry_recheck() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        window
            .update(cx, |app, _, cx| {
                app.hold_closes(vec![(pane, true)], Some("lifecycle fixture".into()), cx)
                    .unwrap();
                assert!(!child.exited());
                app.expire_closes(Instant::now() + Duration::from_secs(16), cx);
                assert!(app.workspace.instance(pane).is_none());
                assert!(app.terminal_view(pane).is_none());
                assert!(!app.is_closing(pane));
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn finalizing_all_held_closes_removes_processes_before_persist() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        window
            .update(cx, |app, _, cx| {
                app.hold_closes(vec![(pane, false)], None, cx).unwrap();
                assert!(app.finalize_all_closes(cx));
                assert!(app.try_persist());
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                )
                .unwrap();
                assert!(saved.instance(pane).is_none());
                assert!(!app.is_closing(pane));
                assert!(app.terminal_view(pane).is_none());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

fn wait_for_view(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    view: &Entity<TerminalView>,
    condition: impl Fn(&TerminalView) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // The real PTY reader and GPUI's virtual output-coalescing timers must
        // both advance before an actual child exit/title reaches its view.
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        if window
            .read_with(cx, |_, cx| condition(view.read(cx)))
            .unwrap()
        {
            return;
        }
        if Instant::now() >= deadline {
            let seen = window
                .read_with(cx, |_, cx| {
                    let view = view.read(cx);
                    format!(
                        "reason {:?}, status {:?}, title {:?}, exited {}, screen {:?}",
                        view.runtime_snapshot().reason,
                        view.status(),
                        view.title(),
                        view.exited(),
                        view.visible_text().trim(),
                    )
                })
                .unwrap();
            panic!("owned terminal did not reach expected state: {seen}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn held_panes_skip_tick_attention_and_restart() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (held_clean, held_clean_view, held_clean_child) = terminal(cx, window, pid);
        let (held_bad, held_bad_view, held_bad_child) = terminal(cx, window, pid);
        let (live_clean, live_clean_view, live_clean_child) = terminal(cx, window, pid);
        let (live_bad, live_bad_view, live_bad_child) = terminal(cx, window, pid);
        let activities = window
            .update(cx, |app, window, cx| {
                app.settings.close_on_exit = true;
                app.notifications_enabled = false;
                for (iid, name) in [
                    (held_clean, "held clean fixture"),
                    (held_bad, "held abnormal fixture"),
                    (live_clean, "live clean fixture"),
                    (live_bad, "live abnormal fixture"),
                ] {
                    app.workspace.instance_mut(iid).unwrap().custom_name = Some(name.into());
                }
                app.tick(window, cx);
                let restart = app
                    .terminal_restarts
                    .begin(app.workspace.instance(held_clean).unwrap())
                    .unwrap();
                app.hold_closes(vec![(held_clean, false), (held_bad, false)], None, cx)
                    .unwrap();
                assert!(!app.terminal_restarts.current(held_clean, restart));
                app.restart_agent(held_clean, window, cx);
                assert!(!app.terminal_restarts.contains(held_clean));
                assert!(Arc::ptr_eq(
                    app.terminal_view(held_clean).unwrap().read(cx).session(),
                    &held_clean_child.session
                ));
                app.notifications.clear();
                [held_clean, held_bad]
                    .map(|iid| app.workspace.instance(iid).unwrap().activity.clone())
            })
            .unwrap();
        held_clean_child.terminate(0);
        held_bad_child.terminate(7);
        live_clean_child.terminate(0);
        live_bad_child.terminate(7);
        for view in [
            &held_clean_view,
            &held_bad_view,
            &live_clean_view,
            &live_bad_view,
        ] {
            wait_for_view(cx, window, view, TerminalView::exited);
        }
        window
            .update(cx, |app, window, cx| {
                assert_eq!(held_clean_view.read(cx).exit_code(), Some(0));
                assert_eq!(live_clean_view.read(cx).exit_code(), Some(0));
                app.tick(window, cx);
                for (iid, activity) in [held_clean, held_bad].into_iter().zip(activities) {
                    assert!(app.is_closing(iid));
                    assert_eq!(app.workspace.instance(iid).unwrap().activity, activity);
                    assert!(app.terminal_view(iid).is_some());
                    assert!(!app.exit_logged.contains(&iid));
                    assert!(!app.terminal_restarts.contains(iid));
                    assert!(!app.terminal_launching.contains_key(&iid));
                    assert!(
                        !app.notifications
                            .iter()
                            .any(|notice| notice.instance == Some(iid))
                    );
                    assert!(!app.readings.queue.contains(&iid));
                }
                assert!(
                    app.notifications
                        .iter()
                        .all(|notice| !notice.title.contains("held clean fixture")
                            && !notice.title.contains("held abnormal fixture"))
                );
                assert!(
                    app.workspace.instance(live_clean).is_none(),
                    "live clean control did not auto-close"
                );
                assert!(
                    app.workspace.instance(live_bad).is_some(),
                    "abnormal control should remain a tombstone"
                );
                assert!(
                    app.notifications
                        .iter()
                        .any(|notice| notice.title.contains("live clean fixture"))
                );
                assert!(
                    app.notifications
                        .iter()
                        .any(|notice| notice.title.contains("live abnormal fixture"))
                );
                assert!(app.exit_logged.contains(&live_bad));
            })
            .unwrap();
    });
}

/// The owned command emits real Codex OSC-title protocol bytes through its PTY.
/// No provider binary, injected runtime status or fake title-source callback is used.
fn title_terminal(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pid: Uuid,
    name: &str,
    state: &str,
) -> (Uuid, Entity<TerminalView>, OwnedChild) {
    window
        .update(cx, |app, window, cx| {
            let root = app.workspace.project(pid).unwrap().root_path.clone();
            let filename = format!("title-{}.cmd", Uuid::new_v4());
            let script = root.join(&filename);
            std::fs::write(
                &script,
                format!("@echo \x1b]0;{name} ^| {state}\x07\r\n@more\r\n"),
            )
            .unwrap();
            // cmd rejects canonical verbatim paths. Pass a local filename and
            // the same normal cwd used by the verified B2 title fixture. The
            // `.\` keeps it found when NoDefaultCurrentDirectoryInExePath is set
            // (Claude Code exports it), which stops cmd searching the cwd.
            let directory = root.to_string_lossy();
            let directory = directory
                .strip_prefix(r"\\?\")
                .unwrap_or(&directory)
                .to_string();
            let args = vec![
                "/d".into(),
                "/q".into(),
                "/c".into(),
                format!(r".\{filename}"),
            ];
            let mut instance = Instance::shell(pid);
            instance.program = Some("codex".into());
            instance.custom_name = Some(name.into());
            let iid = instance.id;
            let launch = TerminalLaunch::spawn(
                CommandSpec::program("cmd.exe", args)
                    .with_cwd(directory)
                    .with_status_program(Some("codex".into())),
                (80, 24),
            )
            .unwrap();
            let view = cx.new(|cx| TerminalView::new(launch, iid, Instant::now(), window, cx));
            let child = OwnedChild::new(view.read(cx).session().clone());
            app.workspace.add_instance(instance);
            let project = app.workspace.project_mut(pid).unwrap();
            if let Some(anchor) = project.first_instance() {
                assert!(add_tab(&mut project.layout, anchor, iid));
            } else {
                project.layout = Some(PaneNode::leaf(iid));
            }
            app.terminals.insert(iid, view.clone());
            (iid, view, child)
        })
        .unwrap()
}

#[test]
fn batch_close_splits_quiet_and_active_members() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (anchor, _, _anchor_child) = terminal(cx, window, pid);
        let (quiet, quiet_view, quiet_child) =
            title_terminal(cx, window, pid, "quiet fixture", "Ready");
        let (busy, busy_view, busy_child) =
            title_terminal(cx, window, pid, "busy fixture", "Working");
        wait_for_view(cx, window, &quiet_view, |view| {
            view.runtime_snapshot().reason == TerminalStatusReason::ProviderTitle
                && view.status() == AgentStatus::Idle
        });
        wait_for_view(cx, window, &busy_view, |view| {
            view.runtime_snapshot().reason == TerminalStatusReason::ProviderTitle
                && view.status() == AgentStatus::Working
        });
        window.update(cx, |app, window, cx| {
            app.close_other_tabs(anchor, window, cx);
            let pending = app.confirm.as_ref().expect("mixed batch prompts once");
            assert!(matches!(&pending.action, ConfirmAction::CloseBatch(ids) if ids.as_slice() == [quiet, busy]));
            assert!(pending.message.contains("busy fixture"));
            assert!(!pending.message.contains("quiet fixture"));
            assert!(!app.is_closing(quiet));
            assert!(!app.is_closing(busy));
        }).unwrap();
        let (new_tab, _, new_child) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                app.run_close_alternative(cx);
                assert!(app.is_closing(quiet));
                assert!(!app.closing[&quiet].forced);
                assert!(!app.is_closing(busy));
                assert!(!app.is_closing(new_tab));
                assert_eq!(app.close_groups.len(), 1);
                assert_eq!(app.close_groups[0].members, vec![quiet]);
                app.undo_latest_close(window, cx);
                // Repeat the original captured selection and confirm all. The tab
                // created while the first prompt was open remains excluded.
                app.request_recoverable_close(vec![quiet, busy], cx);
                app.run_confirm(window, cx);
                assert!(app.is_closing(quiet));
                assert!(app.is_closing(busy));
                assert!(!app.closing[&quiet].forced);
                assert!(app.closing[&busy].forced);
                assert_eq!(app.close_groups.len(), 1);
                assert_eq!(app.close_groups[0].members, vec![quiet, busy]);
                assert_eq!(
                    app.workspace.project(pid).unwrap().instances(),
                    vec![anchor, new_tab]
                );
                assert!(!quiet_child.exited());
                assert!(!busy_child.exited());
                assert!(!new_child.exited());
            })
            .unwrap();
    });
}

// Git's worktree argv rejects the verbatim prefix returned by canonicalizing
// the isolated test root. Keep actual fixture metadata on a normal drive path.
fn git_fixture_path(path: PathBuf) -> PathBuf {
    let value = path.to_string_lossy();
    PathBuf::from(value.strip_prefix(r"\\?\").unwrap_or(&value))
}

fn git_fixture(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "core.hooksPath=NUL",
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

#[test]
fn worktree_last_pane_dispose_answer_is_final() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        window
            .update(cx, |app, _, cx| {
                let root = app.workspace.project(pid).unwrap().root_path.clone();
                git_fixture(&root, &["init", "--initial-branch=main"]);
                git_fixture(&root, &["commit", "--allow-empty", "-m", "fixture"]);
                let worktree_path =
                    git_fixture_path(muxel_store::data_dir().unwrap().join("owned-worktree"));
                git_fixture(
                    &root,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        "muxel/close-fixture",
                        worktree_path.to_str().unwrap(),
                    ],
                );
                let draft = worktree_path.join("draft.txt");
                std::fs::write(&draft, "keep this work\n").unwrap();
                let wid = Uuid::new_v4();
                app.workspace.add_worktree(Worktree {
                    id: wid,
                    project_id: pid,
                    name: "close fixture".into(),
                    path: worktree_path.clone(),
                    branch: "muxel/close-fixture".into(),
                    color: 0,
                    detached: false,
                });
                let instance = app.workspace.instance_mut(pane).unwrap();
                instance.worktree_id = Some(wid);
                instance.worktree_path = Some(worktree_path.clone());
                instance.worktree_branch = Some("muxel/close-fixture".into());
                app.request_close_instance(pane, cx);
                let pending = app
                    .pending_worktree_dispose
                    .front()
                    .expect("worktree decision must precede close");
                assert_eq!(pending.wid, wid);
                assert_eq!(pending.close_after, vec![pane]);
                assert!(pending.changed > 0);
                assert!(app.confirm.is_none(), "worktree must be the sole prompt");
                assert!(!app.is_closing(pane));
                assert!(
                    !child.exited(),
                    "preflight killed the terminal before the choice"
                );
                app.dispose_worktree_keep(cx);
                assert!(app.workspace.instance(pane).is_none());
                assert!(app.terminal_view(pane).is_none());
                assert!(!app.is_closing(pane));
                assert!(
                    app.close_groups.is_empty(),
                    "worktree choice must not count down"
                );
                assert!(app.pending_worktree_dispose.is_empty());
                assert!(app.workspace.worktree(wid).unwrap().detached);
                assert_eq!(std::fs::read_to_string(&draft).unwrap(), "keep this work\n");
                app.expire_closes(Instant::now() + Duration::from_secs(60), cx);
                assert!(
                    app.pending_worktree_dispose.is_empty(),
                    "a second dispose question appeared at expiry"
                );
                assert!(worktree_path.is_dir());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn dirty_editor_cancel_and_save_failure_preserve_the_draft_until_discarded() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (iid, editor) = window.update(cx, |app, window, cx| {
            // A directory is a deterministic unwritable file target on Windows;
            // this does not rely on ACLs or the account's privileges.
            let path = app.workspace.project(pid).unwrap().root_path.clone();
            let instance = Instance::editor(pid, Some(path.clone()));
            let iid = instance.id;
            let snapshot = EditorSnapshot { text: "unsaved draft".into(), path: Some(path), language: "text".into(), cursor: None, dirty: true, external_change: None, disk_stamp: None, show_rendered: false, diff_dir: None };
            let editor = snapshot.build(app.editor_config(), window, cx);
            app.workspace.add_instance(instance);
            app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(iid));
            app.editors.insert(iid, editor.clone());
            app.request_close_instance(iid, cx);
            assert!(matches!(app.confirm.as_ref().map(|p| &p.action), Some(ConfirmAction::SaveEditorBeforeClose { iid: pending, .. }) if *pending == iid));
            app.cancel_confirm(cx);
            assert!(app.workspace.instance(iid).is_some());
            assert!(editor.read(cx).is_dirty());
            assert_eq!(editor.read(cx).text(cx), "unsaved draft");
            app.request_close_instance(iid, cx);
            app.run_confirm(window, cx);
            (iid, editor)
        }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let failed = window
                .read_with(cx, |app, _| {
                    app.notifications.iter().any(|notice| {
                        matches!(notice.kind, NotifKind::Error)
                            && notice.title == super::t("Save failed").as_ref()
                    })
                })
                .unwrap();
            if failed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "actual editor save failure was not reported"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        window
            .update(cx, |app, _, cx| {
                assert!(app.workspace.instance(iid).is_some());
                assert_eq!(app.editors.get(&iid), Some(&editor));
                assert!(editor.read(cx).is_dirty());
                assert_eq!(editor.read(cx).text(cx), "unsaved draft");
                assert!(!app.is_closing(iid));
                app.request_close_instance(iid, cx);
                app.run_close_alternative(cx);
                assert!(app.workspace.instance(iid).is_none());
                assert!(!app.editors.contains_key(&iid));
                assert!(app.close_groups.is_empty());
            })
            .unwrap();
    });
}

#[test]
fn workspace_switch_retries_a_failed_close_flush_before_leaving() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                let previous = app.current_workspace.unwrap();
                let next = Uuid::new_v4();
                app.workspaces.workspaces.extend([
                    WorkspaceMeta {
                        id: previous,
                        name: "previous".into(),
                    },
                    WorkspaceMeta {
                        id: next,
                        name: "target".into(),
                    },
                ]);
                let target_root = muxel_store::data_dir().unwrap().join("retry-target");
                std::fs::create_dir_all(&target_root).unwrap();
                let mut target = Workspace::default();
                let target_project = target.add_project(Project::new("target", target_root));
                muxel_store::save_workspace_to(
                    &muxel_store::workspace_doc_path(next).unwrap(),
                    &target,
                )
                .unwrap();
                app.hold_closes(vec![(pane, false)], None, cx).unwrap();
                let old_path = muxel_store::workspace_doc_path(previous).unwrap();
                let saved_countdown = old_path.with_extension("before-close.json");
                let saved = muxel_store::load_workspace_from(&old_path).unwrap();
                assert!(
                    saved.instance(pane).is_some(),
                    "fixture must retain a recoverable countdown document"
                );
                // Replace only this isolated fixture file with an empty directory.
                // The real store's atomic rename must fail regardless of privileges.
                std::fs::rename(&old_path, &saved_countdown).unwrap();
                std::fs::create_dir(&old_path).unwrap();
                app.enter_workspace(next, window, cx);
                assert_eq!(
                    app.current_workspace,
                    Some(previous),
                    "failed flush must veto switch"
                );
                assert!(app.save_errors.contains_key(&SaveTarget::Workspace));
                assert!(
                    app.workspace.instance(pane).is_none(),
                    "first attempt should finalize the close"
                );
                assert!(!app.is_closing(pane));
                std::fs::remove_dir(&old_path).unwrap();
                std::fs::rename(&saved_countdown, &old_path).unwrap();
                assert!(
                    muxel_store::load_workspace_from(&old_path)
                        .unwrap()
                        .instance(pane)
                        .is_some()
                );
                // No held group remains to trigger the original finalize-only save
                // condition. A second attempt must still flush the pending removal.
                app.enter_workspace(next, window, cx);
                assert_eq!(app.current_workspace, Some(next));
                assert!(app.workspace.project(target_project).is_some());
                let saved = muxel_store::load_workspace_from(&old_path).unwrap();
                assert!(
                    saved.instance(pane).is_none(),
                    "retry left a finalized pane in the old durable document"
                );
                assert!(!app.save_errors.contains_key(&SaveTarget::Workspace));
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn project_finalization_preserves_other_projects_in_a_shared_close_group() {
    with_app(|cx, window| {
        let first_project = project(cx, window);
        let (first, _, first_child) = terminal(cx, window, first_project);
        let second_project = project(cx, window);
        let (second, _, second_child) = terminal(cx, window, second_project);
        window
            .update(cx, |app, window, cx| {
                let group = app
                    .hold_closes(vec![(first, false), (second, false)], None, cx)
                    .unwrap();
                let deadline = app.closing_until(second).unwrap();
                app.finalize_project_closes(first_project, cx);
                assert!(app.workspace.instance(first).is_none());
                assert!(app.terminal_view(first).is_none());
                assert!(!app.is_closing(first));
                assert!(
                    app.workspace.instance(second).is_some(),
                    "project finalization closed another project's pane"
                );
                assert!(app.is_closing(second));
                assert_eq!(app.closing_until(second), Some(deadline));
                let remaining = app
                    .close_groups
                    .iter()
                    .find(|remaining| remaining.id == group)
                    .expect("other project keeps its close group");
                assert_eq!(remaining.members, vec![second]);
                assert!(Arc::ptr_eq(
                    app.terminal_view(second).unwrap().read(cx).session(),
                    &second_child.session
                ));
                assert!(!second_child.exited());
                assert!(app.try_persist());
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                )
                .unwrap();
                assert!(saved.instance(first).is_none());
                assert_eq!(
                    saved.project(second_project).unwrap().instances(),
                    vec![second]
                );
                assert!(
                    app.workspace
                        .project(second_project)
                        .unwrap()
                        .instances()
                        .is_empty()
                );
                app.undo_close(group, window, cx);
                assert_eq!(
                    app.workspace.project(second_project).unwrap().instances(),
                    vec![second]
                );
                assert!(!app.is_closing(second));
                assert!(Arc::ptr_eq(
                    app.terminal_view(second).unwrap().read(cx).session(),
                    &second_child.session
                ));
                assert!(!second_child.exited());
            })
            .unwrap();
        wait_for_exit(cx, &first_child);
    });
}

fn clean_worktree(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pid: Uuid,
    pane: Uuid,
) -> (Uuid, PathBuf) {
    window
        .update(cx, |app, _, _| {
            let root = app.workspace.project(pid).unwrap().root_path.clone();
            git_fixture(&root, &["init", "--initial-branch=main"]);
            git_fixture(&root, &["commit", "--allow-empty", "-m", "fixture"]);
            let path = git_fixture_path(
                muxel_store::data_dir()
                    .unwrap()
                    .join("review-r2-clean-worktree"),
            );
            git_fixture(
                &root,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "muxel/clean-close-fixture",
                    path.to_str().unwrap(),
                ],
            );
            let wid = Uuid::new_v4();
            app.workspace.add_worktree(Worktree {
                id: wid,
                project_id: pid,
                name: "clean close fixture".into(),
                path: path.clone(),
                branch: "muxel/clean-close-fixture".into(),
                color: 0,
                detached: false,
            });
            let instance = app.workspace.instance_mut(pane).unwrap();
            instance.worktree_id = Some(wid);
            instance.worktree_path = Some(path.clone());
            instance.worktree_branch = Some("muxel/clean-close-fixture".into());
            assert_eq!(super::integrations::worktree_change_count(&path), 0);
            let base = super::integrations::repo_head(&root).expect("fixture base HEAD");
            assert_eq!(
                super::integrations::worktree_unmerged_count(&path, &base),
                0
            );
            (wid, path)
        })
        .unwrap()
}

#[test]
fn working_clean_last_worktree_asks_and_keeps_undo_until_expiry() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, view, child) =
            title_terminal(cx, window, pid, "working worktree fixture", "Working");
        wait_for_view(cx, window, &view, |view| {
            view.runtime_snapshot().reason == TerminalStatusReason::ProviderTitle
                && view.status() == AgentStatus::Working
        });
        let (wid, path) = clean_worktree(cx, window, pid, pane);
        window
            .update(cx, |app, window, cx| {
                app.request_close_instance(pane, cx);
                assert!(
                    app.confirm.is_some(),
                    "Working agent in a clean worktree must ask"
                );
                assert!(!child.exited(), "close request killed the Working agent");
                assert!(app.workspace.instance(pane).is_some());
                assert!(!app.is_closing(pane));
                assert!(path.is_dir());
                app.run_confirm(window, cx);
                assert!(app.is_closing(pane));
                assert!(app.closing[&pane].forced);
                assert!(
                    !child.exited(),
                    "confirmed close must retain its Undo window"
                );
                assert!(path.is_dir());
                app.undo_latest_close(window, cx);
                assert!(!app.is_closing(pane));
                assert!(Arc::ptr_eq(
                    app.terminal_view(pane).unwrap().read(cx).session(),
                    &child.session
                ));
                assert!(!child.exited());
                assert!(path.is_dir());
                assert!(app.workspace.worktree(wid).is_some());
                app.request_close_instance(pane, cx);
                assert!(app.confirm.is_some());
                app.run_confirm(window, cx);
                assert!(app.is_closing(pane));
                assert!(path.is_dir());
                app.expire_closes(Instant::now() + Duration::from_secs(16), cx);
                assert!(
                    app.pending_worktree_dispose.is_empty(),
                    "fully landed worktree should not show a dispose question"
                );
                assert!(app.confirm.is_none());
                assert!(!app.is_closing(pane));
                assert!(app.close_groups.is_empty());
                assert!(app.workspace.instance(pane).is_none());
                assert!(app.terminal_view(pane).is_none());
                assert!(app.workspace.worktree(wid).is_none());
                assert!(
                    !path.exists(),
                    "clean worktree should be removed only at expiry"
                );
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn quiet_clean_last_worktree_counts_down_without_a_prompt() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, view, child) =
            title_terminal(cx, window, pid, "quiet worktree fixture", "Ready");
        wait_for_view(cx, window, &view, |view| {
            view.runtime_snapshot().reason == TerminalStatusReason::ProviderTitle
                && view.status() == AgentStatus::Idle
        });
        let (wid, path) = clean_worktree(cx, window, pid, pane);
        window
            .update(cx, |app, _, cx| {
                app.request_close_instance(pane, cx);
                assert!(app.confirm.is_none());
                assert!(app.pending_worktree_dispose.is_empty());
                assert!(
                    app.is_closing(pane),
                    "quiet clean worktree close must remain recoverable"
                );
                assert!(!app.closing[&pane].forced);
                assert!(!child.exited());
                assert!(path.is_dir());
                assert!(app.workspace.worktree(wid).is_some());
                app.expire_closes(Instant::now() + Duration::from_secs(16), cx);
                assert!(app.workspace.instance(pane).is_none());
                assert!(app.workspace.worktree(wid).is_none());
                assert!(!path.exists());
                assert!(app.pending_worktree_dispose.is_empty());
                assert!(app.close_groups.is_empty());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn process_exiting_during_a_quiet_hold_finalizes_at_expiry() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, view, child) = title_terminal(cx, window, pid, "exiting held fixture", "Ready");
        wait_for_view(cx, window, &view, |view| {
            view.runtime_snapshot().reason == TerminalStatusReason::ProviderTitle
                && view.status() == AgentStatus::Idle
        });
        window
            .update(cx, |app, _, cx| {
                app.request_close_instance(pane, cx);
                assert!(app.is_closing(pane));
                assert!(!app.closing[&pane].forced);
            })
            .unwrap();
        child.terminate(0);
        wait_for_view(cx, window, &view, TerminalView::exited);
        window
            .update(cx, |app, _, cx| {
                assert_eq!(view.read(cx).exit_code(), Some(0));
                app.expire_closes(Instant::now() + Duration::from_secs(16), cx);
                assert!(
                    app.workspace.instance(pane).is_none(),
                    "exited held pane must not become a started-working decision"
                );
                assert!(!app.is_closing(pane));
                assert!(app.terminal_view(pane).is_none());
                assert!(app.close_groups.is_empty());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn quit_queues_departure_even_when_workspace_persist_fails() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        let (release, waiting) = async_channel::bounded::<()>(1);
        let (queued, pending, failed, finalized) = window
            .update(cx, |app, _, cx| {
                app._status_timer = gpui::Task::ready(());
                app.hold_closes(vec![(pane, false)], None, cx).unwrap();
                let path = muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
                app.close_tmux_kills
                    .push(cx.background_executor().spawn(async move {
                        let _ = waiting.recv().await;
                    }));
                app.quit_after_closes(cx);
                let queued = matches!(
                    app.close_departure,
                    Some(super::recoverable_close::CloseDeparture::Quit)
                );
                let pending = app.close_departure_task.is_some();
                let failed = app.save_errors.contains_key(&SaveTarget::Workspace);
                let finalized = app.workspace.instance(pane).is_none();
                // Clear both pieces before any assertion or channel release. Even
                // an expected-red unwind must never call application-global Quit.
                app.close_departure = None;
                app.close_departure_task = None;
                std::fs::remove_dir(&path).unwrap();
                app.try_persist();
                (queued, pending, failed, finalized)
            })
            .unwrap();
        release.try_send(()).unwrap();
        cx.run_until_parked();
        assert!(
            failed,
            "fixture did not force a real workspace save failure"
        );
        assert!(finalized);
        assert!(
            queued,
            "save failure silently vetoed Quit instead of queuing departure"
        );
        assert!(pending);
        wait_for_exit(cx, &child);
    });
}

#[test]
fn ordinary_workspace_switch_is_not_blocked_by_failed_persist() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        let (entered, next) = window
            .update(cx, |app, window, cx| {
                app._status_timer = gpui::Task::ready(());
                assert!(app.close_groups.is_empty());
                assert!(app.auto_name_save_due.is_none());
                let previous = app.current_workspace.unwrap();
                let next = Uuid::new_v4();
                app.workspaces.workspaces.extend([
                    WorkspaceMeta {
                        id: previous,
                        name: "previous".into(),
                    },
                    WorkspaceMeta {
                        id: next,
                        name: "target".into(),
                    },
                ]);
                let root = muxel_store::data_dir()
                    .unwrap()
                    .join("ordinary-switch-target");
                std::fs::create_dir_all(&root).unwrap();
                let mut target = Workspace::default();
                target.add_project(Project::new("target", root));
                muxel_store::save_workspace_to(
                    &muxel_store::workspace_doc_path(next).unwrap(),
                    &target,
                )
                .unwrap();
                assert!(app.try_persist());
                let path = muxel_store::workspace_doc_path(previous).unwrap();
                let backup = path.with_extension("ordinary-before-switch.json");
                std::fs::rename(&path, &backup).unwrap();
                std::fs::create_dir(&path).unwrap();
                app.enter_workspace(next, window, cx);
                let entered = app.current_workspace == Some(next);
                std::fs::remove_dir(&path).unwrap();
                std::fs::rename(&backup, &path).unwrap();
                (entered, next)
            })
            .unwrap();
        assert!(
            entered,
            "ordinary switch acquired a new save-success dependency"
        );
        window
            .read_with(cx, |app, _| {
                assert_eq!(app.current_workspace, Some(next));
                assert!(app.workspace.instance(pane).is_none());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn pending_dispose_merges_unrelated_close_requests_without_duplicates() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        let (first, _, first_child) = terminal(cx, window, pid);
        let (second, _, second_child) = terminal(cx, window, pid);
        let (wid, path) = clean_worktree(cx, window, pid, pane);
        std::fs::write(path.join("draft.txt"), "keep this work\n").unwrap();
        window
            .update(cx, |app, _, cx| {
                app.request_recoverable_close(vec![pane, first], cx);
                assert_eq!(app.pending_worktree_dispose.len(), 1);
                assert_eq!(app.pending_worktree_dispose[0].wid, wid);
                assert_eq!(app.pending_worktree_dispose[0].close_rest, vec![first]);
                app.request_recoverable_close(vec![pane, second, first, second], cx);
                assert_eq!(app.pending_worktree_dispose.len(), 1);
                let pending = &app.pending_worktree_dispose[0];
                assert_eq!(pending.close_after, vec![pane]);
                assert_eq!(
                    pending.close_rest.len(),
                    2,
                    "pending dispose dropped or duplicated requested panes"
                );
                assert!(pending.close_rest.contains(&first));
                assert!(pending.close_rest.contains(&second));
                assert!(app.close_groups.is_empty());
                assert!(!child.exited());
                assert!(!first_child.exited());
                assert!(!second_child.exited());
            })
            .unwrap();
    });
}

fn new_worktree_member_survives_dispose_choice(choice: &str) {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (captured, _, captured_child) = terminal(cx, window, pid);
        let (wid, path) = clean_worktree(cx, window, pid, captured);
        let draft = path.join("draft.txt");
        std::fs::write(&draft, "keep this work\n").unwrap();
        window
            .update(cx, |app, _, cx| {
                // If an unfixed handler reaches git, keep every operation inside
                // this disposable fixture without user hooks, identity or signing.
                let root = &app.workspace.project(pid).unwrap().root_path;
                for (key, value) in [
                    ("core.hooksPath", "NUL"),
                    ("commit.gpgsign", "false"),
                    ("user.name", "Muxel test"),
                    ("user.email", "muxel-test@example.invalid"),
                ] {
                    git_fixture(root, &["config", "--local", key, value]);
                }
                app.request_close_instance(captured, cx);
                assert_eq!(app.pending_worktree_dispose.len(), 1);
                assert_eq!(app.pending_worktree_dispose[0].close_after, vec![captured]);
            })
            .unwrap();
        let (uncaptured, new_view, new_child) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                let instance = app.workspace.instance_mut(uncaptured).unwrap();
                instance.worktree_id = Some(wid);
                instance.worktree_path = Some(path.clone());
                instance.worktree_branch = Some("muxel/clean-close-fixture".into());
                match choice {
                    "discard" => app.dispose_worktree_discard(cx),
                    "keep" => app.dispose_worktree_keep(cx),
                    "commit" => app.dispose_worktree_commit(cx),
                    "merge" => app.dispose_worktree_merge(window, cx),
                    _ => panic!("unknown fixture dispose choice"),
                }
                assert!(
                    app.workspace.instance(uncaptured).is_some(),
                    "{choice} closed a pane outside the captured request"
                );
                assert!(Arc::ptr_eq(
                    app.terminal_view(uncaptured).unwrap().read(cx).session(),
                    &new_child.session
                ));
                assert_eq!(app.terminal_view(uncaptured), Some(&new_view));
                assert!(!new_child.exited());
                assert!(
                    path.is_dir(),
                    "{choice} removed a worktree with a new live pane"
                );
                assert_eq!(
                    std::fs::read_to_string(&draft).unwrap(),
                    "keep this work\n",
                    "{choice} changed uncaptured live work"
                );
                let worktree = app
                    .workspace
                    .worktree(wid)
                    .expect("new pane's worktree stays registered");
                assert!(
                    !worktree.detached,
                    "{choice} detached a worktree still used by the new pane"
                );
                assert!(!app.is_closing(uncaptured));
                if let Some(pending) = app.confirm.as_ref()
                    && let ConfirmAction::CloseBatch(ids) = &pending.action
                {
                    assert!(!ids.contains(&uncaptured));
                }
                assert!(
                    !captured_child.exited(),
                    "stale disposal should replan the captured close request"
                );
            })
            .unwrap();
    });
}

#[test]
fn discard_preserves_a_new_uncaptured_worktree_pane() {
    new_worktree_member_survives_dispose_choice("discard");
}

#[test]
fn keep_preserves_a_new_uncaptured_worktree_pane() {
    new_worktree_member_survives_dispose_choice("keep");
}

#[test]
fn commit_preserves_a_new_uncaptured_worktree_pane() {
    new_worktree_member_survives_dispose_choice("commit");
}

#[test]
fn merge_preserves_a_new_uncaptured_worktree_pane() {
    new_worktree_member_survives_dispose_choice("merge");
}

#[test]
fn workspace_departure_waits_for_local_tmux_close_tasks_before_draining_processes() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (held, _, held_child) = terminal(cx, window, pid);
        let (survivor, _, survivor_child) = terminal(cx, window, pid);
        let (release, waiting) = async_channel::bounded::<()>(1);
        let next = Uuid::new_v4();
        let (previous, target_project) = window
            .update(cx, |app, window, cx| {
                app._status_timer = gpui::Task::ready(());
                let previous = app.current_workspace.unwrap();
                app.workspaces.workspaces.extend([
                    WorkspaceMeta {
                        id: previous,
                        name: "previous".into(),
                    },
                    WorkspaceMeta {
                        id: next,
                        name: "target".into(),
                    },
                ]);
                let root = muxel_store::data_dir().unwrap().join("departure-target");
                std::fs::create_dir_all(&root).unwrap();
                let mut target = Workspace::default();
                let target_project = target.add_project(Project::new("target", root));
                muxel_store::save_workspace_to(
                    &muxel_store::workspace_doc_path(next).unwrap(),
                    &target,
                )
                .unwrap();
                app.hold_closes(vec![(held, false)], None, cx).unwrap();
                // Use the same real GPUI task collection as local tmux teardown,
                // with a controlled completion instead of launching tmux.
                app.close_tmux_kills
                    .push(cx.background_executor().spawn(async move {
                        waiting.recv().await.expect("release owned departure task");
                    }));
                app.enter_workspace(next, window, cx);
                assert_eq!(app.current_workspace, Some(previous));
                assert!(app.close_departure_task.is_some());
                assert!(matches!(
                    app.close_departure,
                    Some(super::recoverable_close::CloseDeparture::Workspace(id)) if id == next
                ));
                assert!(app.workspace.instance(held).is_none());
                assert!(app.workspace.instance(survivor).is_some());
                assert!(Arc::ptr_eq(
                    app.terminal_view(survivor).unwrap().read(cx).session(),
                    &survivor_child.session,
                ));
                assert!(
                    !survivor_child.exited(),
                    "departure drained the old process early"
                );
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(previous).unwrap(),
                )
                .unwrap();
                assert!(saved.instance(held).is_none());
                assert!(saved.instance(survivor).is_some());
                (previous, target_project)
            })
            .unwrap();
        // Allow several production departure polls while the task is blocked.
        for _ in 0..3 {
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
        }
        window
            .read_with(cx, |app, _| {
                assert_eq!(app.current_workspace, Some(previous));
                assert!(app.close_departure_task.is_some());
                assert!(!app.close_tmux_kills.is_empty());
                assert!(app.workspace.instance(survivor).is_some());
                assert!(!survivor_child.exited());
            })
            .unwrap();
        release.try_send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            let entered = window
                .read_with(cx, |app, _| app.current_workspace == Some(next))
                .unwrap();
            if entered {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "departure did not resume after teardown completed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        window
            .read_with(cx, |app, _| {
                assert!(app.close_departure_task.is_none());
                assert!(app.close_departure.is_none());
                assert!(app.close_tmux_kills.is_empty());
                assert!(app.workspace.project(target_project).is_some());
                assert!(app.workspace.instance(survivor).is_none());
                assert!(app.terminal_view(survivor).is_none());
            })
            .unwrap();
        wait_for_exit(cx, &held_child);
        wait_for_exit(cx, &survivor_child);
    });
}

#[test]
fn delayed_transcript_refuses_retired_epoch_and_replaced_session_without_changing_focus() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, view, child) = terminal(cx, window, pid);
        let (other, other_view, other_child) = terminal(cx, window, pid);
        let (replacement_id, replacement_view, replacement_child) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                app._status_timer = gpui::Task::ready(());
                let retired = app
                    .terminal_input_admission(pane, cx)
                    .expect("initial admission");
                let group = app.hold_closes(vec![(pane, false)], None, cx).unwrap();
                app.undo_close(group, window, cx);
                assert!(Arc::ptr_eq(
                    app.terminal_view(pane).unwrap().read(cx).session(),
                    &child.session,
                ));
                app.focus_instance(other, window, cx);
                assert!(other_view.read(cx).focus_handle(cx).is_focused(window));
                let original_rev = child.session.input_rev();
                let other_rev = other_child.session.input_rev();
                app.insert_transcript(Some(retired), "retired transcript", true, window, cx);
                assert_eq!(child.session.input_rev(), original_rev);
                assert_eq!(other_child.session.input_rev(), other_rev);
                assert_eq!(app.active_instance, Some(other));
                assert!(other_view.read(cx).focus_handle(cx).is_focused(window));
                let fresh = app
                    .terminal_input_admission(pane, cx)
                    .expect("fresh admission after Undo");
                app.insert_transcript(Some(fresh), "fresh transcript", true, window, cx);
                assert!(
                    child.session.input_rev() > original_rev,
                    "fresh production callback did not type"
                );
                assert_eq!(app.active_instance, Some(pane));
                assert!(view.read(cx).focus_handle(cx).is_focused(window));

                // Both sessions have epoch zero: an epoch-only comparison would
                // wrongly admit this callback into a replacement with the same iid.
                assert_eq!(other_child.session.input_epoch(), 0);
                assert_eq!(replacement_child.session.input_epoch(), 0);
                let replaced = app
                    .terminal_input_admission(other, cx)
                    .expect("pre-replacement admission");
                app.terminals.remove(&replacement_id);
                app.terminals.insert(other, replacement_view.clone());
                muxel_core::remove(
                    &mut app.workspace.project_mut(pid).unwrap().layout,
                    replacement_id,
                );
                app.workspace.remove_instance_meta(replacement_id);
                assert!(!Arc::ptr_eq(
                    app.terminal_view(other).unwrap().read(cx).session(),
                    &other_child.session,
                ));
                let current_rev = child.session.input_rev();
                let abandoned_rev = other_child.session.input_rev();
                let replacement_rev = replacement_child.session.input_rev();
                app.insert_transcript(Some(replaced), "wrong launch transcript", true, window, cx);
                assert_eq!(child.session.input_rev(), current_rev);
                assert_eq!(other_child.session.input_rev(), abandoned_rev);
                assert_eq!(replacement_child.session.input_rev(), replacement_rev);
                assert_eq!(app.active_instance, Some(pane));
                assert!(view.read(cx).focus_handle(cx).is_focused(window));
                assert!(!child.exited());
                assert!(!other_child.exited());
                assert!(!replacement_child.exited());
            })
            .unwrap();
    });
}

/// A recorded tmux session sends a close through the client-count probe. tmux
/// is absent on Windows hosts, so the probe ends quickly with an unknown count.
fn route_through_client_probe(app: &mut MuxelApp, iid: Uuid) {
    app.workspace.instance_mut(iid).unwrap().tmux_session =
        Some(format!("muxel_r3_probe_{}", Uuid::new_v4().simple()));
}

fn wait_for_close_checks(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if window
            .read_with(cx, |app, _| app.close_requests.is_empty())
            .unwrap()
        {
            return;
        }
        assert!(Instant::now() < deadline, "client probe reply never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn probe_batch_still_decides_after_one_member_closes_another_way() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (probed, _, probed_child) = terminal(cx, window, pid);
        let (other, _, other_child) = terminal(cx, window, pid);
        window
            .update(cx, |app, _, cx| {
                app._status_timer = gpui::Task::ready(());
                route_through_client_probe(app, probed);
                app.request_recoverable_close(vec![probed, other], cx);
                assert_eq!(
                    app.close_requests.len(),
                    2,
                    "fixture must wait on its client probe"
                );
                assert!(app.confirm.is_none());
                // The batch's other member goes some other way mid-probe.
                app.close_instance_inner(other, "close (fixture)", cx);
                assert!(!app.close_requests.contains_key(&other));
            })
            .unwrap();
        wait_for_close_checks(cx, window);
        window
            .update(cx, |app, _, cx| {
                let pending = app
                    .confirm
                    .as_ref()
                    .expect("the remaining member was dropped with its batch");
                assert!(matches!(
                    &pending.action,
                    ConfirmAction::CloseBatch(ids) if ids.as_slice() == [probed]
                ));
                assert!(!app.is_closing(probed));
                assert!(!probed_child.exited());
                app.cancel_confirm(cx);
            })
            .unwrap();
        wait_for_exit(cx, &other_child);
    });
}

#[test]
fn workspace_switch_stops_once_for_an_unsaved_close_then_leaves() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        let (previous, next, old_path) = window
            .update(cx, |app, window, cx| {
                app._status_timer = gpui::Task::ready(());
                let previous = app.current_workspace.unwrap();
                let next = Uuid::new_v4();
                app.workspaces.workspaces.extend([
                    WorkspaceMeta {
                        id: previous,
                        name: "previous".into(),
                    },
                    WorkspaceMeta {
                        id: next,
                        name: "target".into(),
                    },
                ]);
                let root = muxel_store::data_dir()
                    .unwrap()
                    .join("unsaved-close-target");
                std::fs::create_dir_all(&root).unwrap();
                let mut target = Workspace::default();
                target.add_project(Project::new("target", root));
                muxel_store::save_workspace_to(
                    &muxel_store::workspace_doc_path(next).unwrap(),
                    &target,
                )
                .unwrap();
                app.hold_closes(vec![(pane, false)], None, cx).unwrap();
                // Replace only this isolated fixture document with a directory,
                // so every later save fails whatever the account's privileges.
                let old_path = muxel_store::workspace_doc_path(previous).unwrap();
                std::fs::remove_file(&old_path).unwrap();
                std::fs::create_dir(&old_path).unwrap();
                app.enter_workspace(next, window, cx);
                assert_eq!(
                    app.current_workspace,
                    Some(previous),
                    "the first switch must stop for the unsaved close"
                );
                assert!(app.workspace.instance(pane).is_none());
                assert!(app.save_errors.contains_key(&SaveTarget::Workspace));
                assert!(app.notifications.iter().any(|notice| {
                    matches!(notice.kind, NotifKind::Error)
                        && notice.title == super::t("Closed terminals weren't saved").as_ref()
                }));
                (previous, next, old_path)
            })
            .unwrap();
        window
            .update(cx, |app, window, cx| {
                assert_eq!(app.current_workspace, Some(previous));
                app.enter_workspace(next, window, cx);
                assert_eq!(
                    app.current_workspace,
                    Some(next),
                    "a store that keeps failing must not trap the user"
                );
                assert!(!app.close_persist_pending);
                assert!(!app.close_flush_warned);
            })
            .unwrap();
        std::fs::remove_dir(&old_path).unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn a_new_check_beside_a_countdown_has_its_own_cancel() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (held, _, held_child) = terminal(cx, window, pid);
        let (probed, _, probed_child) = terminal(cx, window, pid);
        let group = window
            .update(cx, |app, _, cx| {
                app._status_timer = gpui::Task::ready(());
                let group = app.hold_closes(vec![(held, false)], None, cx).unwrap();
                route_through_client_probe(app, probed);
                app.request_recoverable_close(vec![probed], cx);
                assert!(app.close_requests.contains_key(&probed));
                let latest = app.close_groups.last().unwrap();
                assert_eq!(latest.id, group);
                let (label, button) = app.close_chip_text(latest);
                let name = app.workspace.instance(held).unwrap().display_name();
                assert_eq!(
                    label.as_ref(),
                    super::tf("Closed {name}", &[("name", name)]),
                    "the countdown's label must not describe the other request"
                );
                assert!(
                    button.chars().any(|c| c.is_ascii_digit()),
                    "the button must stay this group's Undo countdown: {button}"
                );
                app.cancel_close_checks(cx);
                assert!(app.close_requests.is_empty());
                assert!(app.is_closing(held), "cancelling the check undid the hold");
                assert!(app.closing_until(held).is_some());
                group
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |app, window, cx| {
                assert!(app.confirm.is_none(), "a cancelled check still asked");
                assert!(!app.is_closing(probed));
                assert!(app.workspace.instance(probed).is_some());
                assert!(!probed_child.exited());
                app.undo_close(group, window, cx);
                assert!(!app.is_closing(held));
                assert!(Arc::ptr_eq(
                    app.terminal_view(held).unwrap().read(cx).session(),
                    &held_child.session
                ));
                assert!(!held_child.exited());
            })
            .unwrap();
    });
}

fn join_worktree(app: &mut MuxelApp, iid: Uuid, wid: Uuid, path: &Path) {
    let instance = app.workspace.instance_mut(iid).unwrap();
    instance.worktree_id = Some(wid);
    instance.worktree_path = Some(path.to_path_buf());
    instance.worktree_branch = Some("muxel/clean-close-fixture".into());
}

#[test]
fn pane_opened_during_a_worktree_question_gets_its_own_prompt_and_undo() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (captured, _, captured_child) = terminal(cx, window, pid);
        let (wid, path) = clean_worktree(cx, window, pid, captured);
        let draft = path.join("draft.txt");
        std::fs::write(&draft, "keep this work\n").unwrap();
        window
            .update(cx, |app, _, cx| {
                app._status_timer = gpui::Task::ready(());
                app.request_close_instance(captured, cx);
                assert_eq!(app.pending_worktree_dispose.len(), 1);
                assert_eq!(app.pending_worktree_dispose[0].close_after, vec![captured]);
            })
            .unwrap();
        let (fresh, _, fresh_child) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                join_worktree(app, fresh, wid, &path);
                app.request_close_instance(fresh, cx);
                let pending = &app.pending_worktree_dispose[0];
                assert_eq!(
                    pending.close_after,
                    vec![captured],
                    "a pane opened after the question joined its answer"
                );
                assert!(pending.close_rest.is_empty());
                let prompt = app
                    .confirm
                    .as_ref()
                    .expect("the new pane gets its own prompt");
                assert!(matches!(
                    &prompt.action,
                    ConfirmAction::CloseBatch(ids) if ids.as_slice() == [fresh]
                ));
                app.run_confirm(window, cx);
                assert!(app.is_closing(fresh));
                assert!(app.closing_until(fresh).is_some());
                assert!(!fresh_child.exited());

                // Answering the earlier question while the new pane counts down
                // asks again, now covering it.
                app.dispose_worktree_keep(cx);
                assert!(!app.workspace.worktree(wid).unwrap().detached);
                assert!(app.is_closing(fresh));
                assert!(!fresh_child.exited());
                assert!(!captured_child.exited());
                assert_eq!(app.pending_worktree_dispose.len(), 1);
                let asked = &app.pending_worktree_dispose[0].close_after;
                assert!(asked.contains(&captured) && asked.contains(&fresh));

                app.dispose_worktree_keep(cx);
                assert!(app.workspace.instance(captured).is_none());
                assert!(app.workspace.instance(fresh).is_none());
                assert!(app.pending_worktree_dispose.is_empty());
                assert!(app.workspace.worktree(wid).unwrap().detached);
                assert_eq!(std::fs::read_to_string(&draft).unwrap(), "keep this work\n");
            })
            .unwrap();
        wait_for_exit(cx, &captured_child);
        wait_for_exit(cx, &fresh_child);
    });
}

#[test]
fn a_member_closing_another_way_leaves_one_worktree_question() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (captured, _, captured_child) = terminal(cx, window, pid);
        let (wid, path) = clean_worktree(cx, window, pid, captured);
        let draft = path.join("draft.txt");
        std::fs::write(&draft, "keep this work\n").unwrap();
        window
            .update(cx, |app, _, cx| {
                app._status_timer = gpui::Task::ready(());
                app.request_close_instance(captured, cx);
                assert_eq!(app.pending_worktree_dispose.len(), 1);

                // The question's only member goes another way, which orphans the
                // worktree while its question is still showing.
                app.close_instance_inner(captured, "close (fixture)", cx);
                assert!(app.workspace.instance(captured).is_none());
                assert_eq!(
                    app.pending_worktree_dispose.len(),
                    1,
                    "the orphaned worktree queued a second question"
                );
                assert!(path.is_dir());

                app.dispose_worktree_keep(cx);
                assert!(app.pending_worktree_dispose.is_empty());
                assert!(app.workspace.worktree(wid).unwrap().detached);
                assert_eq!(std::fs::read_to_string(&draft).unwrap(), "keep this work\n");
            })
            .unwrap();
        wait_for_exit(cx, &captured_child);
    });
}

#[test]
fn undo_during_a_worktree_question_keeps_the_pane_out_of_its_answer() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (held, _, held_child) = terminal(cx, window, pid);
        let (second, _, second_child) = terminal(cx, window, pid);
        let (wid, path) = clean_worktree(cx, window, pid, held);
        let draft = path.join("draft.txt");
        std::fs::write(&draft, "keep this work\n").unwrap();
        window
            .update(cx, |app, window, cx| {
                app._status_timer = gpui::Task::ready(());
                join_worktree(app, second, wid, &path);
                let group = app.hold_closes(vec![(held, false)], None, cx).unwrap();
                app.request_close_instance(second, cx);
                assert_eq!(app.pending_worktree_dispose.len(), 1);
                assert!(app.pending_worktree_dispose[0].close_after.contains(&held));
                app.undo_close(group, window, cx);
                assert!(!app.is_closing(held));
                assert_eq!(app.pending_worktree_dispose[0].close_after, vec![second]);

                app.dispose_worktree_discard(cx);
                assert!(
                    app.workspace.instance(held).is_some(),
                    "the answer closed a pane that Undo brought back"
                );
                assert!(Arc::ptr_eq(
                    app.terminal_view(held).unwrap().read(cx).session(),
                    &held_child.session
                ));
                assert!(!held_child.exited());
                assert!(path.is_dir(), "discard removed a worktree still in use");
                assert_eq!(std::fs::read_to_string(&draft).unwrap(), "keep this work\n");
                assert!(app.workspace.worktree(wid).is_some());
                let prompt = app
                    .confirm
                    .as_ref()
                    .expect("the remaining pane is asked about normally");
                assert!(matches!(
                    &prompt.action,
                    ConfirmAction::CloseBatch(ids) if ids.as_slice() == [second]
                ));
                assert!(!second_child.exited());
                app.cancel_confirm(cx);
            })
            .unwrap();
    });
}
