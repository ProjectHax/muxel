use super::{Instance, MuxelApp, Project, Workspace, WorkspaceMeta};
use crate::theme;
use gpui::{AppContext, Entity, TestAppContext, WindowHandle};
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalSession, TerminalView};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

struct StopChild {
    session: Arc<TerminalSession>,
    process: HANDLE,
}

impl StopChild {
    fn new(session: Arc<TerminalSession>) -> Self {
        // SAFETY: this PID belongs to the PTY fixture we just spawned. Retain
        // the handle before restart/close so PID reuse cannot satisfy the test.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                session.child_pid().expect("fixture child PID"),
            )
        }
        .expect("open owned fixture process");
        Self { session, process }
    }

    fn exited(&self) -> bool {
        // SAFETY: the owned handle remains open until this guard drops.
        let status = unsafe { WaitForSingleObject(self.process, 0) };
        if status == WAIT_OBJECT_0 {
            true
        } else {
            assert_eq!(
                status, WAIT_TIMEOUT,
                "could not query owned fixture process"
            );
            false
        }
    }
}

impl Drop for StopChild {
    fn drop(&mut self) {
        self.session.kill();
        // SAFETY: this guard exclusively owns the process handle.
        let _ = unsafe { CloseHandle(self.process) };
    }
}

pub(super) fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>)) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.wip/lifecycle-tests")
        .join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let _storage = muxel_store::test_support::set_root(root.clone());
    assert_eq!(muxel_store::data_dir(), Some(root.join("data")));
    assert_eq!(muxel_store::config_dir(), Some(root.join("config")));
    // Drop the app and all of its tasks before restoring normal storage paths.
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
    let app = window.entity(&cx).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut cx, window)));
    // Failed regressions must stop every process they created before the app
    // and its isolated storage scope disappear.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.update(&mut cx, |app, cx| {
            for view in app.terminals.values() {
                view.read(cx).session().kill();
            }
            for popout in app.popouts.values() {
                if let Some(view) = popout.terminal_view() {
                    view.read(cx).session().kill();
                }
            }
        });
    }));
    drop(app);
    drop(cx);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn popout_fixture(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
) -> (Uuid, Entity<TerminalView>, StopChild) {
    window
        .update(cx, |app, window, cx| {
            let project_root = muxel_store::data_dir().unwrap().join("project");
            std::fs::create_dir_all(&project_root).unwrap();
            let project = app
                .workspace
                .add_project(Project::new("lifecycle", project_root));
            let mut instance = Instance::shell(project);
            #[cfg(windows)]
            let (program, args) = ("cmd.exe", vec!["/d", "/q", "/c", "more"]);
            #[cfg(unix)]
            let (program, args) = ("/bin/cat", vec![]);
            instance.program = Some(program.into());
            instance.args = args.iter().map(|arg| (*arg).into()).collect();
            let pane = instance.id;
            let spec = CommandSpec::program(program, instance.args.clone());
            app.workspace.add_instance(instance);
            app.use_tmux = false;
            app.use_worktree = false;
            let launch = TerminalLaunch::spawn(spec, (80, 24)).expect("spawn fixture");
            let view = cx.new(|cx| TerminalView::new(launch, pane, Instant::now(), window, cx));
            let child = StopChild::new(view.read(cx).session().clone());
            app.terminals.insert(pane, view.clone());
            app.pop_out_instance(pane, window, cx);
            assert!(!app.terminals.contains_key(&pane));
            assert!(app.popouts.contains_key(&pane));
            (pane, view, child)
        })
        .unwrap()
}

fn wait_for_exit(cx: &mut TestAppContext, child: &StopChild) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if child.exited() {
            return;
        }
        assert!(Instant::now() < deadline, "old PTY did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn secondary_fixture(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
) -> (Uuid, gpui::AnyWindowHandle) {
    let project = window
        .update(cx, |app, window, cx| {
            let root = muxel_store::data_dir().unwrap().join("secondary-project");
            std::fs::create_dir_all(&root).unwrap();
            let project = app.workspace.add_project(Project::new("secondary", root));
            let display = cx.displays().into_iter().next().expect("test display");
            app.open_project_on_monitor(project, display.id(), display.uuid().unwrap(), window, cx);
            project
        })
        .unwrap();
    cx.run_until_parked();
    let handle = window
        .read_with(cx, |app, _| {
            assert!(app.workspace.project_windows.contains_key(&project));
            app.secondary_windows
                .iter()
                .find(|secondary| secondary.pid == project)
                .expect("production open installed secondary window")
                .handle
        })
        .unwrap();
    (project, handle)
}

#[test]
fn actual_popout_lifecycle_workspace_switch_closes_windows_and_preserves_target_workspace() {
    with_app(|cx, window| {
        let (pane, _view, child) = popout_fixture(cx, window);
        let (_, secondary) = secondary_fixture(cx, window);
        let popup = window
            .read_with(cx, |app, _| app.popouts[&pane].window)
            .unwrap();
        let next = Uuid::new_v4();
        let next_project = window
            .update(cx, |app, window, cx| {
                let previous = Uuid::new_v4();
                app.current_workspace = Some(previous);
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
                let project = target.add_project(Project::new("target project", root));
                muxel_store::save_workspace_to(
                    &muxel_store::workspace_doc_path(next).unwrap(),
                    &target,
                )
                .unwrap();
                // The production observer must not borrow MuxelApp again while
                // enter_workspace is still tearing down the old window owners.
                app.enter_workspace(next, window, cx);
                assert_eq!(app.current_workspace, Some(next));
                project
            })
            .unwrap();
        cx.run_until_parked();
        assert!(popup.read_with(cx, |_, _| ()).is_err());
        assert!(secondary.update(cx, |_, _, _| ()).is_err());
        window
            .read_with(cx, |app, _| {
                assert_eq!(app.current_workspace, Some(next));
                assert!(app.workspace.project(next_project).is_some());
                assert!(app.workspace.instance(pane).is_none());
                assert!(app.popouts.is_empty());
                assert!(app.terminals.is_empty());
                assert!(app.secondary_windows.is_empty());
                assert!(app.workspace.project_windows.is_empty());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn actual_popout_lifecycle_dev_console_close_does_not_reenter_the_borrowed_app() {
    with_app(|cx, window| {
        let console = window
            .update(cx, |app, _, cx| {
                app.open_dev_console(cx);
                app.dev_console_window.expect("dev console opened")
            })
            .unwrap();
        window
            .update(cx, |app, _, cx| app.toggle_dev_console(cx))
            .unwrap();
        cx.run_until_parked();
        assert!(console.read_with(cx, |_, _| ()).is_err());
        window
            .read_with(cx, |app, _| assert!(app.dev_console_window.is_none()))
            .unwrap();
    });
}

#[test]
fn actual_popout_lifecycle_bring_project_to_main_cleans_up_secondary_window() {
    with_app(|cx, window| {
        let (project, secondary) = secondary_fixture(cx, window);
        window
            .update(cx, |app, _, cx| app.bring_project_to_main(project, cx))
            .unwrap();
        cx.run_until_parked();
        assert!(secondary.update(cx, |_, _, _| ()).is_err());
        window
            .read_with(cx, |app, _| {
                assert!(app.secondary_windows.is_empty());
                assert!(!app.workspace.project_windows.contains_key(&project));
                assert_eq!(app.workspace.active_project, Some(project));
                assert!(app.workspace.project(project).is_some());
            })
            .unwrap();
    });
}

#[test]
fn actual_popout_lifecycle_old_console_close_keeps_replacement_window() {
    with_app(|cx, window| {
        let (closed, replacement) = window
            .update(cx, |app, _, cx| {
                app.open_dev_console(cx);
                let closed = app.dev_console_window.unwrap();
                app.toggle_dev_console(cx);
                app.open_dev_console(cx);
                (closed, app.dev_console_window.unwrap())
            })
            .unwrap();
        cx.run_until_parked();
        assert!(closed.read_with(cx, |_, _| ()).is_err());
        assert!(replacement.read_with(cx, |_, _| ()).is_ok());
        window
            .read_with(cx, |app, _| {
                assert_eq!(app.dev_console_window, Some(replacement));
            })
            .unwrap();
    });
}

#[test]
fn actual_popout_lifecycle_window_close_still_kills_owned_terminal() {
    with_app(|cx, window| {
        let (pane, _view, child) = popout_fixture(cx, window);
        let popup = window
            .read_with(cx, |app, _| app.popouts[&pane].window)
            .unwrap();
        popup
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        window
            .read_with(cx, |app, _| {
                assert!(!app.popouts.contains_key(&pane));
                assert!(app.workspace.instance(pane).is_none());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn actual_popout_lifecycle_close_runs_observer_after_app_update_and_kills_pty() {
    with_app(|cx, window| {
        let (pane, _view, child) = popout_fixture(cx, window);
        let popup = window
            .read_with(cx, |app, _| app.popouts.get(&pane).unwrap().window)
            .unwrap();
        let closed = Rc::new(Cell::new(0));
        let returned_from_close = Rc::new(Cell::new(false));
        cx.update(|cx| {
            let closed = closed.clone();
            let returned_from_close = returned_from_close.clone();
            cx.on_window_closed(move |_, id| {
                if id == gpui::AnyWindowHandle::from(popup).window_id() {
                    assert!(
                        returned_from_close.get(),
                        "observer ran inside close_instance_inner"
                    );
                    closed.set(closed.get() + 1);
                }
            })
            .detach();
        });
        // This is the production method and production MuxelApp close observer.
        // Inline removal re-enters the borrowed app here and panics on GPUI.
        window
            .update(cx, |app, _, cx| {
                app.close_instance_inner(pane, "lifecycle test", cx);
                returned_from_close.set(true);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(closed.get(), 1);
        assert!(popup.read_with(cx, |_, _| ()).is_err());
        window
            .read_with(cx, |app, _| {
                assert!(!app.popouts.contains_key(&pane));
                assert!(!app.terminals.contains_key(&pane));
                assert!(app.workspace.instance(pane).is_none());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn actual_popout_lifecycle_restart_kills_old_pty_and_installs_one_docked_replacement() {
    with_app(|cx, window| {
        let (pane, _old_view, child) = popout_fixture(cx, window);
        let old_session = child.session.clone();
        let popup = window
            .read_with(cx, |app, _| app.popouts.get(&pane).unwrap().window)
            .unwrap();
        window
            .update(cx, |app, window, cx| app.restart_agent(pane, window, cx))
            .unwrap();
        wait_for_exit(cx, &child);
        window
            .read_with(cx, |app, _| {
                assert!(
                    !app.popouts.contains_key(&pane),
                    "restart must not leave the old popout owner"
                );
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let replacement = window
                .read_with(cx, |app, cx| {
                    let instance = app
                        .workspace
                        .instance(pane)
                        .expect("restart retained metadata");
                    assert!(
                        app.workspace
                            .project(instance.project_id)
                            .and_then(|project| project.layout.as_ref())
                            .is_some_and(|layout| layout.find_path(pane).is_some()),
                        "replacement must be docked in the project layout"
                    );
                    app.terminals
                        .get(&pane)
                        .map(|view| view.read(cx).session().clone())
                })
                .unwrap();
            if let Some(replacement) = replacement {
                let _replacement = StopChild::new(replacement.clone());
                assert_ne!(replacement.id, old_session.id);
                assert!(popup.read_with(cx, |_, _| ()).is_err());
                break;
            }
            assert!(Instant::now() < deadline, "replacement was not installed");
            std::thread::sleep(Duration::from_millis(10));
        }
    });
}
