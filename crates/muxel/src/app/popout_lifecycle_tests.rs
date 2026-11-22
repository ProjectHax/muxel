use super::codex_capture_tests::{TempRoot, stop_app_processes};
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
    with_app_drawing(false, run);
}

pub(super) fn with_app_drawing(
    draw: bool,
    run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>),
) {
    in_a_test_app(draw, |cx| cx.add_window(MuxelApp::new), run);
}

/// The test's main window: the app as its root view, or inside a
/// gpui-component `Root`.
trait MainWindow: Clone {
    fn handle(&self) -> gpui::AnyWindowHandle;

    /// The app, while the window holds it.
    fn app(&self, cx: &TestAppContext) -> Option<Entity<MuxelApp>>;

    fn update_app<R>(
        &self,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut MuxelApp, &mut gpui::Window, &mut gpui::Context<MuxelApp>) -> R,
    ) -> R {
        let app = self.app(cx).expect("the main window holds the app");
        cx.update_window(self.handle(), |_, window, cx| {
            app.update(cx, |app, cx| f(app, window, cx))
        })
        .unwrap()
    }

    fn read_app<R>(&self, cx: &TestAppContext, f: impl FnOnce(&MuxelApp, &gpui::App) -> R) -> R {
        let app = self.app(cx).expect("the main window holds the app");
        app.read_with(cx, f)
    }
}

impl MainWindow for WindowHandle<MuxelApp> {
    fn handle(&self) -> gpui::AnyWindowHandle {
        (*self).into()
    }

    fn app(&self, cx: &TestAppContext) -> Option<Entity<MuxelApp>> {
        self.entity(cx).ok()
    }
}

/// The main window as muxel opens it: the app inside a gpui-component
/// `Root`, which a window needs to draw an input.
#[derive(Clone)]
struct InRoot {
    window: WindowHandle<gpui_component::Root>,
    app: Entity<MuxelApp>,
}

impl MainWindow for InRoot {
    fn handle(&self) -> gpui::AnyWindowHandle {
        self.window.into()
    }

    fn app(&self, _: &TestAppContext) -> Option<Entity<MuxelApp>> {
        Some(self.app.clone())
    }
}

/// [`with_app_drawing`], drawing, with the main window inside a `Root`.
fn with_app_in_root(run: impl FnOnce(&mut TestAppContext, InRoot)) {
    let open = |cx: &mut TestAppContext| {
        let mut app = None;
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|cx| MuxelApp::new(window, cx));
            app = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        InRoot {
            window,
            app: app.unwrap(),
        }
    };
    in_a_test_app(true, open, run);
}

fn in_a_test_app<W: MainWindow>(
    draw: bool,
    open: impl FnOnce(&mut TestAppContext) -> W,
    run: impl FnOnce(&mut TestAppContext, W),
) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.wip/lifecycle-tests")
        .join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    // Declared first, dropped last: the lease is held until the root is gone,
    // so the next app test can't start while it is still being removed.
    let _storage = muxel_store::test_support::set_root(root.clone());
    let _temp = TempRoot(root.clone());
    assert_eq!(muxel_store::data_dir(), Some(root.join("data")));
    assert_eq!(muxel_store::config_dir(), Some(root.join("config")));
    // Drop the app and all of its tasks before restoring normal storage paths.
    let mut cx = TestAppContext::single();
    if !draw {
        cx.skip_drawing();
    }
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let window = open(&mut cx);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut cx, window.clone())
    }));
    // Failed regressions must stop every process they created before the app
    // and its isolated storage scope disappear, and a pane's process must be
    // gone before its folder under the root can be removed.
    let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(app) = window.app(&cx) {
            stop_app_processes(&mut cx, &app);
        }
    }));
    drop(window);
    drop(cx);
    if let Err(panic) = result.and(stopped) {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn with_app_drawing_removes_its_root() {
    let mut data = None;
    with_app_drawing(false, |_, _| data = muxel_store::data_dir());
    let root = data.unwrap().parent().unwrap().to_path_buf();
    assert!(
        !root.try_exists().unwrap(),
        "{} is still there",
        root.display()
    );
}

fn popout_fixture(
    cx: &mut TestAppContext,
    window: impl MainWindow,
) -> (Uuid, Entity<TerminalView>, StopChild) {
    popout_fixture_with_status(cx, window, None)
}

fn popout_fixture_with_status(
    cx: &mut TestAppContext,
    window: impl MainWindow,
    status_program: Option<&str>,
) -> (Uuid, Entity<TerminalView>, StopChild) {
    #[cfg(windows)]
    let args = ["/d", "/q", "/c", "more"].as_slice();
    #[cfg(unix)]
    let args: &[&str] = &[];
    popout_fixture_with_args(cx, window, status_program, args)
}

fn popout_fixture_with_args(
    cx: &mut TestAppContext,
    window: impl MainWindow,
    status_program: Option<&str>,
    args: &[&str],
) -> (Uuid, Entity<TerminalView>, StopChild) {
    window.update_app(cx, |app, window, cx| {
        let project_root = muxel_store::data_dir().unwrap().join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let project = app
            .workspace
            .add_project(Project::new("lifecycle", project_root));
        let mut instance = Instance::shell(project);
        #[cfg(windows)]
        let program = "cmd.exe";
        #[cfg(unix)]
        let program = "/bin/cat";
        instance.program = Some(program.into());
        instance.args = args.iter().map(|arg| (*arg).into()).collect();
        let pane = instance.id;
        let spec = CommandSpec::program(program, instance.args.clone())
            .with_status_program(status_program.map(str::to_string));
        app.workspace.add_instance(instance);
        app.use_tmux = false;
        app.use_worktree = false;
        let launch = TerminalLaunch::spawn(spec, (80, 24)).expect("spawn fixture");
        let view = cx.new(|cx| TerminalView::new(launch, pane, Instant::now(), window, cx));
        let palette = theme::palette_from_theme(cx);
        view.update(cx, |view, _| view.set_palette(palette));
        let child = StopChild::new(view.read(cx).session().clone());
        app.terminals.insert(pane, view.clone());
        app.pop_out_instance(pane, window, cx);
        assert!(!app.terminals.contains_key(&pane));
        assert!(app.popouts.contains_key(&pane));
        (pane, view, child)
    })
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

#[test]
fn unrelated_app_updates_do_not_render_three_quiet_popouts() {
    with_app_drawing(true, |cx, window| {
        let fixtures: Vec<_> = (0..3).map(|_| popout_fixture(cx, window)).collect();
        cx.run_until_parked();
        super::POPOUT_TEST_RENDERS.with(|counts| counts.borrow_mut().clear());
        // Sixty independently flushed app updates, a one-minute 1 Hz workload.
        // GPUI's test effect loop renders only dirty windows. Count Render
        // calls there rather than forcing otherwise clean windows to draw.
        for tick in 0..60 {
            window
                .update(cx, |app, _, cx| {
                    app.sidebar_collapsed = tick % 2 == 0;
                    cx.notify();
                })
                .unwrap();
            cx.run_until_parked();
        }
        let counts: Vec<_> = fixtures
            .iter()
            .map(|(pane, _, _)| {
                super::POPOUT_TEST_RENDERS
                    .with(|counts| counts.borrow().get(pane).copied().unwrap_or(0))
            })
            .collect();
        eprintln!("popout renders for 60 unrelated updates (1 Hz equivalent): {counts:?}");
        assert_eq!(counts, [0, 0, 0]);
    });
}

#[test]
fn popout_theme_updates_the_terminal_palette() {
    with_app(|cx, window| {
        let (_, view, _child) = popout_fixture(cx, window);
        assert_ne!(
            view.read_with(cx, |view, _| view.palette().background),
            0xff00ff
        );
        cx.update_global::<gpui_component::Theme, _>(|theme, _| {
            theme.colors.background = gpui::rgb(0xff00ff).into()
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.palette().background),
            0xff00ff
        );
    });
}

#[test]
fn popout_renders_its_title_status_close_confirmation_and_theme_updates() {
    with_app_drawing(true, |cx, window| {
        let (pane, view, _child) = popout_fixture_with_status(cx, window, Some("muse"));
        let (other, _, _other_child) = popout_fixture(cx, window);
        let draw = |cx: &mut TestAppContext| {
            cx.run_until_parked();
        };
        let reset = || super::POPOUT_TEST_RENDERS.with(|counts| counts.borrow_mut().clear());
        let count = |iid| {
            super::POPOUT_TEST_RENDERS
                .with(|counts| counts.borrow().get(&iid).copied().unwrap_or(0))
        };
        draw(cx);
        reset();
        view.update(cx, |view, cx| {
            view.session()
                .process_probe_output(b"\x1b]0;Updated popout title\x07");
            cx.notify();
        });
        draw(cx);
        assert_eq!(
            view.read_with(cx, |view, _| view.title()).as_deref(),
            Some("Updated popout title")
        );
        assert!(count(pane) > 0);
        assert_eq!(count(other), 0);

        for state in [
            muxel_core::muse::MuseState::Blocked,
            muxel_core::muse::MuseState::Working,
            muxel_core::muse::MuseState::Done,
        ] {
            // Recent title output already makes the pane Working. Start with
            // Blocked so each hook changes the public state; Working itself
            // is activity-derived rather than a sticky hook assertion.
            let expected = match state {
                muxel_core::muse::MuseState::Blocked => muxel_terminal::AgentStatus::Blocked,
                muxel_core::muse::MuseState::Working => muxel_terminal::AgentStatus::Working,
                muxel_core::muse::MuseState::Done => muxel_terminal::AgentStatus::Done,
                muxel_core::muse::MuseState::Idle => unreachable!(),
            };
            assert_ne!(view.read_with(cx, |view, _| view.status()), expected);
            reset();
            view.update(cx, |view, cx| {
                view.set_muse_hook_state(
                    &muxel_core::muse::MuseMetadata {
                        state,
                        ..Default::default()
                    },
                    cx,
                )
            });
            draw(cx);
            assert_eq!(view.read_with(cx, |view, _| view.status()), expected);
            assert!(count(pane) > 0, "missing status update {state:?}");
            assert_eq!(count(other), 0);
        }

        reset();
        window
            .update(cx, |app, _, cx| {
                app.confirm = Some(super::PendingConfirm {
                    title: "Close pane?".into(),
                    message: "Running".into(),
                    confirm_label: "Close".into(),
                    details: Vec::new(),
                    action: super::ConfirmAction::CloseBatch(vec![pane]),
                });
                cx.notify();
            })
            .unwrap();
        draw(cx);
        assert!(count(pane) > 0);
        assert_eq!(count(other), 0);
        reset();
        window
            .update(cx, |app, _, cx| {
                app.confirm.as_mut().unwrap().message = "Updated close reason".into();
                cx.notify();
            })
            .unwrap();
        draw(cx);
        assert!(count(pane) > 0);
        assert_eq!(count(other), 0);
        for action in [
            super::ConfirmAction::CloseBatch(vec![pane, other]),
            super::ConfirmAction::SaveEditorBeforeClose {
                iid: pane,
                rest: vec![other],
            },
        ] {
            reset();
            window
                .update(cx, |app, _, cx| {
                    app.confirm.as_mut().unwrap().action = action;
                    cx.notify();
                })
                .unwrap();
            draw(cx);
            assert!(count(pane) > 0, "changed alternative button must render");
            assert_eq!(count(other), 0);
        }
        reset();
        window
            .update(cx, |app, _, cx| app.cancel_confirm(cx))
            .unwrap();
        draw(cx);
        assert!(count(pane) > 0);
        assert_eq!(count(other), 0);

        reset();
        cx.update_global::<gpui_component::Theme, _>(|theme, _| theme.radius_lg = gpui::px(9.0));
        draw(cx);
        assert!(
            count(pane) > 0 && count(other) > 0,
            "theme changes must reach every popout"
        );
    });
}

fn secondary_fixture(
    cx: &mut TestAppContext,
    window: impl MainWindow,
) -> (Uuid, gpui::AnyWindowHandle) {
    let project = window.update_app(cx, |app, window, cx| {
        let root = muxel_store::data_dir().unwrap().join("secondary-project");
        std::fs::create_dir_all(&root).unwrap();
        let project = app.workspace.add_project(Project::new("secondary", root));
        let display = cx.displays().into_iter().next().expect("test display");
        app.open_project_on_monitor(project, display.id(), display.uuid().unwrap(), window, cx);
        project
    });
    cx.run_until_parked();
    let handle = window.read_app(cx, |app, _| {
        assert!(app.workspace.project_windows.contains_key(&project));
        app.secondary_windows
            .iter()
            .find(|secondary| secondary.pid == project)
            .expect("production open installed secondary window")
            .handle
    });
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

/// A pane in `project`'s layout that prints `ready` once its shell is up and
/// then waits on `more`, its terminal made and focused in `window`.
fn focused_pane(
    app: &Entity<MuxelApp>,
    project: Uuid,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) -> (Uuid, Entity<TerminalView>) {
    let mut instance = Instance::shell(project);
    instance.program = Some("cmd.exe".into());
    instance.args = ["/d", "/q", "/c", "echo ready&more"]
        .map(String::from)
        .to_vec();
    let pane = instance.id;
    let spec = CommandSpec::program("cmd.exe", instance.args.clone());
    let launch = TerminalLaunch::spawn(spec, (80, 24)).expect("spawn fixture");
    let view = cx.new(|cx| TerminalView::new(launch, pane, Instant::now(), window, cx));
    app.update(cx, |app, _| {
        app.workspace.project_mut(project).unwrap().layout = Some(muxel_core::PaneNode::leaf(pane));
        app.workspace.add_instance(instance);
        app.terminals.insert(pane, view.clone());
        app.active_instance = Some(pane);
    });
    let focus = gpui::Focusable::focus_handle(view.read(cx), cx);
    window.focus(&focus, cx);
    (pane, view)
}

/// Keys a dialog keeps from the terminal under it: a letter, and Tab and
/// Shift+Tab, which the app binds as actions that run ahead of any key
/// listener.
const DIALOG_KEYS: [&str; 3] = ["x", "tab", "shift-tab"];

/// The app's own keymap, so Tab and Shift+Tab are the `Terminal` context's
/// SendTab and SendBackTab, as they are in the app.
fn app_keymap(cx: &mut TestAppContext) {
    cx.update(|cx| super::install_keybindings(&muxel_core::Settings::default(), cx));
}

/// Which of [`DIALOG_KEYS`], each typed into `window`, reached `view`'s
/// terminal.
fn types_into(
    cx: &mut TestAppContext,
    window: gpui::AnyWindowHandle,
    view: &Entity<TerminalView>,
) -> Vec<&'static str> {
    let session = view.read_with(cx, |view, _| view.session().clone());
    let mut typed = Vec::new();
    for keys in DIALOG_KEYS {
        let before = session.input_rev();
        cx.simulate_keystrokes(window, keys);
        cx.run_until_parked();
        if session.input_rev() != before {
            typed.push(keys);
        }
    }
    typed
}

/// Past the first-run screens, so the main window draws the active project.
fn showing_a_project(cx: &mut TestAppContext, window: impl MainWindow) -> Uuid {
    window.update_app(cx, |app, _, _| {
        app.show_terms = false;
        app.show_workspace_selector = false;
        app.current_workspace = Some(Uuid::new_v4());
        let root = muxel_store::data_dir().unwrap().join("dialog-project");
        std::fs::create_dir_all(&root).unwrap();
        let project = app.workspace.add_project(Project::new("dialogs", root));
        app.workspace.active_project = Some(project);
        project
    })
}

/// A confirm or the Quit question over the focused terminal takes the keys:
/// none reaches the terminal, and Escape cancels the dialog.
#[test]
fn a_dialog_keeps_keys_from_the_terminal_under_it() {
    with_app_drawing(true, |cx, window| {
        app_keymap(cx);
        let app = window.entity(&*cx).unwrap();
        let project = showing_a_project(cx, window);
        let main: gpui::AnyWindowHandle = window.into();
        let (_, view) = cx
            .update_window(main, |_, window, cx| {
                focused_pane(&app, project, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            types_into(cx, main, &view),
            DIALOG_KEYS,
            "the focused terminal takes keys"
        );

        window
            .update(cx, |app, _, cx| {
                let action = super::ConfirmAction::DeleteProject(project);
                app.request_confirm("Remove project?", "", "Remove", action, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            types_into(cx, main, &view),
            Vec::<&str>::new(),
            "keys reached the terminal under the confirm"
        );
        cx.simulate_keystrokes(main, "escape");
        assert!(
            window
                .read_with(cx, |app, _| app.confirm.is_none())
                .unwrap()
        );

        window
            .update(cx, |app, _, cx| {
                app.show_quit_confirm = true;
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            types_into(cx, main, &view),
            Vec::<&str>::new(),
            "keys reached the terminal under the Quit question"
        );
        cx.simulate_keystrokes(main, "escape");
        assert!(
            !window
                .read_with(cx, |app, _| app.show_quit_confirm)
                .unwrap()
        );
        assert_eq!(
            types_into(cx, main, &view),
            DIALOG_KEYS,
            "the terminal takes keys again"
        );
    });
}

/// The confirm about a pane in a project's own window shows there, and keeps
/// that window's keys from the pane.
#[test]
fn a_dialog_in_a_project_window_keeps_keys_from_its_terminal() {
    with_app_drawing(true, |cx, window| {
        app_keymap(cx);
        let app = window.entity(&*cx).unwrap();
        showing_a_project(cx, window);
        let (project, secondary) = secondary_fixture(cx, window);
        let (pane, view) = cx
            .update_window(secondary, |_, window, cx| {
                focused_pane(&app, project, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            types_into(cx, secondary, &view),
            DIALOG_KEYS,
            "the focused terminal takes keys"
        );

        window
            .update(cx, |app, _, cx| {
                let action = super::ConfirmAction::CloseBatch(vec![pane]);
                app.request_confirm("Close pane?", "", "Close", action, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            types_into(cx, secondary, &view),
            Vec::<&str>::new(),
            "keys reached the terminal under the confirm"
        );
        cx.simulate_keystrokes(secondary, "escape");
        assert!(
            window
                .read_with(cx, |app, _| app.confirm.is_none())
                .unwrap()
        );
    });
}

/// A popped-out pane's close confirm keeps the pop-out's keys from the pane.
#[test]
fn a_popouts_close_confirm_keeps_keys_from_its_terminal() {
    with_app_drawing(true, |cx, window| {
        app_keymap(cx);
        let (pane, view, _child) = popout_fixture(cx, window);
        let popout: gpui::AnyWindowHandle = window
            .read_with(cx, |app, _| app.popouts[&pane].window)
            .unwrap()
            .into();
        cx.update_window(popout, |_, window, cx| {
            let focus = gpui::Focusable::focus_handle(view.read(cx), cx);
            window.focus(&focus, cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            types_into(cx, popout, &view),
            DIALOG_KEYS,
            "the focused terminal takes keys"
        );

        window
            .update(cx, |app, _, cx| {
                let action = super::ConfirmAction::CloseBatch(vec![pane]);
                app.request_confirm("Close pane?", "", "Close", action, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            types_into(cx, popout, &view),
            Vec::<&str>::new(),
            "keys reached the terminal under the confirm"
        );
        cx.simulate_keystrokes(popout, "escape");
        cx.run_until_parked();
        assert!(
            window
                .read_with(cx, |app, _| app.confirm.is_none())
                .unwrap()
        );
    });
}

/// What the walk below leaves to the dialog rule's own answer instead of
/// pressing, and whether the rule keeps it. Pressed for real, each would
/// reach outside the test (the microphone or speaker). Quit and NewWindow are
/// pressed through real dispatch with harmless global handlers registered.
fn left_to_the_rule(action: &dyn gpui::Action) -> Option<bool> {
    let outside: [&dyn gpui::Action; 4] = [
        &super::ToggleSpeechToText,
        &super::HoldSpeechToText,
        &super::ReadAloud,
        &super::ReadAloudRestart,
    ];
    outside
        .iter()
        .any(|other| action.partial_eq(*other))
        .then_some(true)
}

/// A key the app binds, as `simulate_keystrokes` types it: the actions it runs
/// in one context or another and, for a key the walk doesn't press, whether
/// the rule keeps it.
type BoundKeys = std::collections::BTreeMap<String, (Vec<&'static str>, Option<bool>)>;

fn bound_keys(cx: &mut TestAppContext) -> BoundKeys {
    cx.update(|cx| {
        let mut keys = BoundKeys::new();
        for binding in cx.key_bindings().borrow().bindings() {
            let typed = binding
                .keystrokes()
                .iter()
                .map(|keystroke| keystroke.inner().unparse())
                .collect::<Vec<_>>()
                .join(" ");
            let (actions, left) = keys.entry(typed).or_default();
            actions.push(binding.action().name());
            *left = left.or(left_to_the_rule(binding.action()));
        }
        keys
    })
}

/// The first line pushed into each pane's scrollback before the walk.
const FIRST_LINE: &str = "scrollback line 01";

/// What a key pressed under a dialog must leave as it was.
#[derive(PartialEq)]
struct Untouched {
    /// Input written to the pane under the dialog.
    input: u64,
    /// Its scrollback still holds [`FIRST_LINE`].
    scrollback: bool,
    /// Every project's panes, tabs and splits, the pop-outs, and the windows.
    panes: String,
    /// Overlay flags, toggles, zoom and global-action counters a bound action
    /// can change.
    overlays: String,
    active: Option<Uuid>,
    focus: Option<gpui::FocusHandle>,
    dialog: bool,
}

fn untouched(
    cx: &mut TestAppContext,
    app: &Entity<MuxelApp>,
    window: gpui::AnyWindowHandle,
    view: &Entity<TerminalView>,
    new_window_fired: &Cell<usize>,
) -> Untouched {
    cx.update_window(window, |_, window, cx| {
        let view = view.read(cx);
        let app = app.read(cx);
        let layouts: Vec<_> = app
            .workspace
            .projects
            .iter()
            .map(|project| (project.id, project.layout.clone()))
            .collect();
        let instances: Vec<Uuid> = app.workspace.instances.iter().map(|i| i.id).collect();
        let mut shown: Vec<Uuid> = app
            .terminals
            .keys()
            .chain(app.editors.keys())
            .chain(app.popouts.keys())
            .copied()
            .collect();
        shown.sort();
        let mut sec_sidebars: Vec<Uuid> = app.secondary_sidebar_shown.iter().copied().collect();
        sec_sidebars.sort();
        Untouched {
            input: view.session().input_rev(),
            scrollback: view.recent_text(1000).contains(FIRST_LINE),
            panes: format!(
                "{layouts:?} {instances:?} {shown:?} {} windows",
                cx.windows().len()
            ),
            overlays: format!(
                "keys={} settings={} palette={} find={} term_search={} broadcast={} view={:?} sidebar={} sec_sidebars={sec_sidebars:?} wt={} dev={} zoom={} new_win={}",
                app.show_keys,
                app.show_settings,
                app.show_search_palette,
                app.show_find_panel,
                app.term_search.is_some(),
                app.broadcasting,
                app.main_view,
                app.sidebar_collapsed,
                app.use_worktree,
                app.dev_console_window.is_some(),
                app.settings.zoom.to_bits(),
                new_window_fired.get(),
            ),
            active: app.active_instance,
            focus: window.focused(cx),
            dialog: app.dialog_shown_in(window, cx),
        }
    })
    .unwrap()
}

fn what_changed(before: &Untouched, after: &Untouched) -> Vec<&'static str> {
    [
        ("input reached the pane", before.input != after.input),
        ("scrollback cleared", before.scrollback != after.scrollback),
        (
            "a pane or window opened, closed or moved",
            before.panes != after.panes,
        ),
        (
            "an overlay or app state changed",
            before.overlays != after.overlays,
        ),
        ("another pane became active", before.active != after.active),
        ("the keyboard moved", before.focus != after.focus),
        ("the dialog went", before.dialog != after.dialog),
    ]
    .into_iter()
    .filter_map(|(what, changed)| changed.then_some(what))
    .collect()
}

/// Names what was in hand if it panics. On code that lets a key act under a
/// dialog, what the key runs can panic, and the panic alone wouldn't say
/// which key, or which dialog, it was.
struct Pressing(String);

impl Drop for Pressing {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("panicked pressing {}", self.0);
        }
    }
}

/// Press every key the app binds in `window` while it shows a dialog over
/// `view`'s pane; `restore` puts the dialog and keyboard back after a key
/// that moved them. What each key did, as `keys (actions): what`.
fn walk_the_keymap(
    cx: &mut TestAppContext,
    app: &Entity<MuxelApp>,
    window: gpui::AnyWindowHandle,
    view: &Entity<TerminalView>,
    quit_fired: &Cell<usize>,
    new_window_fired: &Cell<usize>,
    restore: &dyn Fn(&mut TestAppContext),
) -> Vec<String> {
    let quit_name = gpui::Action::name(&super::Quit);
    let mut acted = Vec::new();
    for (keys, (actions, left)) in bound_keys(cx) {
        // Escape cancels the dialog, as the tests above see.
        if keys == "escape" {
            continue;
        }
        let what = match left {
            Some(kept) => {
                let held = cx
                    .update_window(window, |_, window, cx| {
                        let event = gpui::KeystrokeEvent {
                            keystroke: gpui::Keystroke::parse(&keys).unwrap(),
                            action: None,
                            context_stack: window.context_stack(),
                        };
                        app.update(cx, |app, cx| app.dialog_keystroke(&event, window, cx))
                    })
                    .unwrap();
                if held == kept {
                    continue;
                }
                vec![if kept { "not kept" } else { "kept" }]
            }
            None => {
                let before = untouched(cx, app, window, view, new_window_fired);
                let quits_before = quit_fired.get();
                let is_quit = actions.contains(&quit_name);
                let pressing = Pressing(format!("{keys} ({})", actions.join(", ")));
                cx.simulate_keystrokes(window, &keys);
                cx.run_until_parked();
                drop(pressing);
                let mut what =
                    what_changed(&before, &untouched(cx, app, window, view, new_window_fired));
                let quits_after = quit_fired.get();
                if is_quit && quits_after != quits_before + 1 {
                    what.push("Quit did not dispatch");
                } else if !is_quit && quits_after != quits_before {
                    what.push("Quit dispatched unexpectedly");
                }
                if what.is_empty() {
                    continue;
                }
                restore(cx);
                what
            }
        };
        acted.push(format!(
            "{keys} ({}): {}",
            actions.join(", "),
            what.join(", ")
        ));
    }
    acted
}

/// An agent preset that runs `more`, so an agent a key opens is one more
/// pane the walk sees, not a real agent.
fn more_preset() -> muxel_core::AgentPreset {
    muxel_core::AgentPreset {
        name: "more".into(),
        program: Some("cmd.exe".into()),
        args: ["/d", "/q", "/c", "more"].map(String::from).to_vec(),
        ..muxel_core::AgentPreset::cmd()
    }
}

/// Give `view` a screen of scrollback, once its shell has printed its `ready`
/// marker and the terminal view has drained it onto the grid.
fn with_scrollback(cx: &mut TestAppContext, view: &Entity<TerminalView>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        if view.read_with(cx, |view, _| view.recent_text(100).contains("ready")) {
            break;
        }
        assert!(Instant::now() < deadline, "terminal PTY never became ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    let lines: String = (1..=60)
        .map(|n| format!("scrollback line {n:02}\r\n"))
        .collect();
    view.update(cx, |view, _| {
        view.session().process_probe_output(lines.as_bytes())
    });
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.recent_text(1000).contains(FIRST_LINE)));
}

/// While a window shows a dialog, no key the app binds does anything but the
/// two it means to: Escape cancels the dialog and the key bound to Quit
/// passes. Every binding is pressed under each dialog in each kind of window,
/// over a focused pane and with nothing focused, and must leave the pane's
/// input and scrollback, every pane and window, the keyboard and the dialog
/// as they were. The main window holds the app inside a `Root`, as muxel
/// opens it, so what a key opens there draws as it would in the app.
#[test]
fn no_key_binding_acts_under_a_dialog() {
    with_app_in_root(|cx, window| {
        app_keymap(cx);
        let quit_fired = Rc::new(Cell::new(0usize));
        let new_window_fired = Rc::new(Cell::new(0usize));
        cx.update(|cx| {
            let quit_fired = quit_fired.clone();
            cx.on_action(move |_: &super::Quit, _| {
                quit_fired.set(quit_fired.get() + 1);
            });
            let new_window_fired = new_window_fired.clone();
            cx.on_action(move |_: &super::NewWindow, _| {
                new_window_fired.set(new_window_fired.get() + 1);
            });
        });
        let app = window.app.clone();
        let project = showing_a_project(cx, window.clone());
        window.update_app(cx, |app, _, _| {
            app.presets = vec![more_preset()];
            app.current_preset = 0;
        });
        let main = window.handle();
        let (main_pane, main_view) = cx
            .update_window(main, |_, window, cx| {
                focused_pane(&app, project, window, cx)
            })
            .unwrap();
        let (other, secondary) = secondary_fixture(cx, window.clone());
        let (other_pane, other_view) = cx
            .update_window(secondary, |_, window, cx| {
                focused_pane(&app, other, window, cx)
            })
            .unwrap();
        let (popped, popped_view, _child) = popout_fixture_with_args(
            cx,
            window.clone(),
            None,
            &["/d", "/q", "/c", "echo ready&more"],
        );
        let popout: gpui::AnyWindowHandle = app
            .read_with(cx, |app, _| app.popouts[&popped].window)
            .into();
        for view in [&main_view, &other_view, &popped_view] {
            with_scrollback(cx, view);
        }
        let confirm = |about: super::ConfirmAction| {
            move |app: &mut MuxelApp, cx: &mut gpui::Context<MuxelApp>| {
                app.request_confirm("Dialog", "", "Go", about.clone(), cx)
            }
        };
        let quit = |app: &mut MuxelApp, cx: &mut gpui::Context<MuxelApp>| {
            app.show_quit_confirm = true;
            cx.notify();
        };
        type Show = Box<dyn Fn(&mut MuxelApp, &mut gpui::Context<MuxelApp>)>;
        /// A dialog to show over `window`, and the pane its keys would reach.
        struct Case<'a> {
            name: &'a str,
            window: gpui::AnyWindowHandle,
            pane: Uuid,
            view: &'a Entity<TerminalView>,
            show: Show,
            focused: bool,
        }
        let cases = [
            Case {
                name: "main, confirm",
                window: main,
                pane: main_pane,
                view: &main_view,
                show: Box::new(confirm(super::ConfirmAction::DeleteProject(project))),
                focused: true,
            },
            Case {
                name: "main, confirm, nothing focused",
                window: main,
                pane: main_pane,
                view: &main_view,
                show: Box::new(confirm(super::ConfirmAction::DeleteProject(project))),
                focused: false,
            },
            Case {
                name: "main, Quit question",
                window: main,
                pane: main_pane,
                view: &main_view,
                show: Box::new(quit),
                focused: true,
            },
            Case {
                name: "project window, confirm",
                window: secondary,
                pane: other_pane,
                view: &other_view,
                show: Box::new(confirm(super::ConfirmAction::CloseBatch(vec![other_pane]))),
                focused: true,
            },
            Case {
                name: "pop-out, confirm",
                window: popout,
                pane: popped,
                view: &popped_view,
                show: Box::new(confirm(super::ConfirmAction::CloseBatch(vec![popped]))),
                focused: true,
            },
        ];
        let mut acted = Vec::new();
        for Case {
            name: case,
            window: handle,
            pane,
            view,
            show,
            focused,
        } in &cases
        {
            let restore = |cx: &mut TestAppContext| {
                window.update_app(cx, |app, _, cx| {
                    app.active_instance = Some(*pane);
                    if app.confirm.is_none() && !app.show_quit_confirm {
                        show(app, cx);
                    }
                });
                cx.run_until_parked();
                cx.update_window(*handle, |_, window, cx| {
                    if *focused {
                        let focus = gpui::Focusable::focus_handle(view.read(cx), cx);
                        window.focus(&focus, cx);
                    } else {
                        window.blur();
                    }
                })
                .unwrap();
                cx.run_until_parked();
            };
            restore(cx);
            let _case = Pressing(format!("every key under the dialog of \"{case}\""));
            acted.extend(
                walk_the_keymap(
                    cx,
                    &app,
                    *handle,
                    view,
                    &quit_fired,
                    &new_window_fired,
                    &restore,
                )
                .into_iter()
                .map(|line| format!("{case}: {line}")),
            );
            if !focused {
                cx.simulate_keystrokes(*handle, "escape");
                cx.run_until_parked();
                assert!(
                    app.read_with(cx, |app, _| app.confirm.is_none()),
                    "{case}: Escape did not cancel the blurred dialog"
                );
                let rooted = cx
                    .update_window(*handle, |_, window, cx| {
                        app.read(cx).focus_handle.contains_focused(window, cx)
                    })
                    .unwrap();
                assert!(rooted, "{case}: dismissing a blurred dialog left no focus");
            } else {
                window.update_app(cx, |app, _, cx| {
                    app.show_quit_confirm = false;
                    app.cancel_confirm(cx);
                });
                cx.run_until_parked();
            }
        }
        assert!(
            acted.is_empty(),
            "keys acted under a dialog:\n{}",
            acted.join("\n")
        );
    });
}

/// The first key of a two-key binding, typed on a focused terminal, waits for
/// the second. With none, gpui replays it to the terminal once its one-second
/// wait runs out, without calling keystroke interceptors; under a dialog
/// raised meanwhile, it must not reach the terminal.
#[test]
fn a_replayed_pending_key_does_not_reach_the_pane_under_a_dialog() {
    with_app_drawing(true, |cx, window| {
        app_keymap(cx);
        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new("a b", super::ToggleSidebar, None)]);
        });
        let app = window.entity(&*cx).unwrap();
        let project = showing_a_project(cx, window);
        let main: gpui::AnyWindowHandle = window.into();
        let (_, view) = cx
            .update_window(main, |_, window, cx| {
                focused_pane(&app, project, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        let session = view.read_with(cx, |view, _| view.session().clone());
        let replayed = |cx: &mut TestAppContext, dialog: bool| {
            let before = session.input_rev();
            cx.simulate_keystrokes(main, "a");
            assert_eq!(
                session.input_rev(),
                before,
                "`a` waits for the `b` of `a b`"
            );
            if dialog {
                window
                    .update(cx, |app, _, cx| {
                        app.show_quit_confirm = true;
                        cx.notify();
                    })
                    .unwrap();
            }
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_secs(2));
            cx.run_until_parked();
            session.input_rev() != before
        };
        assert!(
            replayed(cx, false),
            "with no dialog the waiting key reaches the terminal"
        );
        assert!(
            !replayed(cx, true),
            "a replayed pending key reached the terminal under the dialog"
        );
    });
}

/// The same on the workspace selector: the first key of a two-key binding,
/// typed into the new workspace's name field, lands there once its wait runs
/// out, but not under "Delete workspace?" raised meanwhile.
#[test]
fn a_replayed_pending_key_does_not_reach_the_selector_under_its_confirm() {
    with_app_in_root(|cx, window| {
        app_keymap(cx);
        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new("a b", super::ToggleSidebar, None)]);
        });
        let app = window.app.clone();
        let main = window.handle();
        window.update_app(cx, |app, window, cx| {
            app.show_terms = false;
            app.show_workspace_selector = true;
            let field = gpui::Focusable::focus_handle(app.workspace_name_input.read(cx), cx);
            window.focus(&field, cx);
            cx.notify();
        });
        cx.run_until_parked();
        let name = |cx: &mut TestAppContext| {
            app.read_with(cx, |app, cx| {
                app.workspace_name_input.read(cx).value().to_string()
            })
        };
        cx.simulate_keystrokes(main, "z");
        cx.run_until_parked();
        assert_eq!(name(cx), "z", "the name field takes typing");
        let replayed = |cx: &mut TestAppContext, dialog: bool| {
            let before = name(cx);
            cx.simulate_keystrokes(main, "a");
            assert_eq!(name(cx), before, "`a` waits for the `b` of `a b`");
            if dialog {
                window.update_app(cx, |app, _, cx| {
                    let action = super::ConfirmAction::DeleteWorkspace(Uuid::new_v4());
                    app.request_confirm("Delete workspace?", "", "Delete", action, cx);
                });
            }
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_secs(2));
            cx.run_until_parked();
            name(cx) != before
        };
        assert!(
            replayed(cx, false),
            "with no dialog the waiting key reaches the name field"
        );
        assert!(
            !replayed(cx, true),
            "a replayed pending key reached the name field under the confirm"
        );
    });
}

/// Clicking a confirm's backdrop cancels it and leaves the keyboard where it
/// was, so the terminal under it takes the next key.
#[test]
fn clicking_a_dialogs_backdrop_leaves_the_terminal_its_keys() {
    with_app_drawing(true, |cx, window| {
        app_keymap(cx);
        let app = window.entity(&*cx).unwrap();
        let project = showing_a_project(cx, window);
        let main: gpui::AnyWindowHandle = window.into();
        let (_, view) = cx
            .update_window(main, |_, window, cx| {
                focused_pane(&app, project, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |app, _, cx| {
                let action = super::ConfirmAction::DeleteProject(project);
                app.request_confirm("Remove project?", "", "Remove", action, cx);
            })
            .unwrap();
        cx.run_until_parked();
        // Beside the card, which sits in the middle of the window.
        let mut visual = gpui::VisualTestContext::from_window(main, &*cx);
        visual.simulate_click(
            gpui::point(gpui::px(8.0), gpui::px(200.0)),
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();
        assert!(
            window
                .read_with(cx, |app, _| app.confirm.is_none())
                .unwrap(),
            "the click missed the backdrop"
        );
        assert_eq!(
            types_into(cx, main, &view),
            DIALOG_KEYS,
            "the terminal lost its keys to the backdrop"
        );
    });
}

/// A window's keys go to what it draws on top. The first-run screens draw no
/// Quit question, so they keep no keys for one. An input drawn above a dialog
/// takes them while it holds the keyboard, and leaves them with the dialog
/// when it doesn't.
#[test]
fn keys_go_to_what_the_window_draws_on_top() {
    with_app_in_root(|cx, window| {
        app_keymap(cx);
        let app = window.app.clone();
        let main = window.handle();
        let shows_dialog = |cx: &mut TestAppContext| {
            cx.update_window(main, |_, window, cx| {
                app.read(cx).dialog_shown_in(window, cx)
            })
            .unwrap()
        };

        // An OS close on the Terms screen or the workspace selector asks the
        // Quit question, which neither draws.
        window.update_app(cx, |app, _, cx| {
            app.show_terms = true;
            app.show_quit_confirm = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!shows_dialog(cx), "the Terms screen kept keys for Quit");
        window.update_app(cx, |app, _, cx| {
            app.show_terms = false;
            app.show_workspace_selector = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            !shows_dialog(cx),
            "the workspace selector kept keys for Quit"
        );
        window.update_app(cx, |app, _, cx| {
            app.show_quit_confirm = false;
            cx.notify();
        });

        let project = showing_a_project(cx, window.clone());
        let (pane, view) = cx
            .update_window(main, |_, window, cx| {
                focused_pane(&app, project, window, cx)
            })
            .unwrap();
        cx.run_until_parked();

        // The auto-message editor draws over a confirm: Escape closes the
        // editor, Enter saves its message, and the confirm stays.
        window.update_app(cx, |app, window, cx| {
            let action = super::ConfirmAction::DeleteProject(project);
            app.request_confirm("Remove?", "", "Remove", action, cx);
            app.open_auto_message_popup(pane, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(main, "escape");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(app.auto_message_popup.is_none(), "Escape closes the editor");
            assert!(app.confirm.is_some(), "the confirm under it stays");
        });
        window.update_app(cx, |app, window, cx| {
            app.open_auto_message_popup(pane, window, cx);
            app.auto_message_input
                .update(cx, |s, cx| s.set_value("keep going", window, cx));
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(main, "enter");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.workspace
                    .instance(pane)
                    .and_then(|i| i.auto_continue_message.as_deref()),
                Some("keep going"),
                "Enter saves the message"
            );
            assert!(app.confirm.is_some(), "the confirm stays after Enter");
        });
        cx.simulate_keystrokes(main, "escape");
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.confirm.is_none()));

        // The SSH password prompt draws over the Quit question and takes what
        // is typed into it.
        window.update_app(cx, |app, window, cx| {
            let action = super::PasswordAction::Connect(project);
            app.prompt_password(Uuid::new_v4(), action, window, cx);
            app.show_quit_confirm = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(main, "p w");
        cx.run_until_parked();
        app.read_with(cx, |app, cx| {
            assert_eq!(app.password_prompt_input.read(cx).value().to_string(), "pw");
            assert!(app.show_quit_confirm, "the Quit question under it stays");
        });
        // With the keyboard back on the terminal, the prompt still shows, and
        // the Quit question keeps the keys from the terminal.
        cx.update_window(main, |_, window, cx| {
            let focus = gpui::Focusable::focus_handle(view.read(cx), cx);
            window.focus(&focus, cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            types_into(cx, main, &view),
            Vec::<&str>::new(),
            "keys reached the terminal under the Quit question"
        );
        // A confirm draws over the prompt, and keeps the keys from it.
        window.update_app(cx, |app, window, cx| {
            let focus = gpui::Focusable::focus_handle(app.password_prompt_input.read(cx), cx);
            window.focus(&focus, cx);
            let action = super::ConfirmAction::DeleteProject(project);
            app.request_confirm("Remove?", "", "Remove", action, cx);
        });
        cx.run_until_parked();
        assert!(
            shows_dialog(cx),
            "the prompt took keys from the confirm over it"
        );
    });
}

#[test]
fn escape_cancels_quit_after_the_run_details_field_disappears() {
    with_app_in_root(|cx, window| {
        app_keymap(cx);
        showing_a_project(cx, window.clone());
        window.update_app(cx, |app, _, cx| {
            app.runners = vec![muxel_core::Runner {
                id: Uuid::new_v4(),
                name: "Runner".into(),
                prompt: "Run with {{input}}".into(),
                preset_id: None,
                auto_mode_presses: 0,
            }];
            app.open_run_dialog(0, cx);
        });
        cx.run_until_parked();
        let details = window.update_app(cx, |app, window, cx| {
            let focus = gpui::Focusable::focus_handle(app.runner_input.read(cx), cx);
            window.focus(&focus, cx);
            focus
        });
        cx.run_until_parked();
        let main = window.handle();
        cx.simulate_keystrokes(main, "x");
        window.read_app(cx, |app, cx| {
            assert_eq!(app.runner_input.read(cx).value().as_str(), "x");
        });

        window.update_app(cx, |app, _, cx| app.delete_runner(0, cx));
        cx.run_until_parked();
        window.update_app(cx, |app, window, cx| {
            assert!(app.runners.is_empty() && app.show_run_dialog);
            assert!(details.is_focused(window), "the details field lost focus");
            app.show_quit_confirm = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(main, "escape");
        cx.run_until_parked();
        assert!(
            !window.read_app(cx, |app, _| app.show_quit_confirm),
            "Escape left the Quit question up after the Run details field disappeared"
        );
    });
}

/// A Keybindings row waiting for a chord binds nothing while a question is
/// open: the Quit question over docked Settings, a confirm floating Settings
/// draws in its own window (where Escape cancels it), or the Quit question
/// with a field drawn above it taking the typing. The row waits on and takes
/// the next chord once the question goes.
#[test]
fn the_shortcut_recorder_waits_while_a_question_is_open() {
    with_app_in_root(|cx, window| {
        app_keymap(cx);
        let app = window.app.clone();
        let main = window.handle();
        let project = showing_a_project(cx, window.clone());
        let bound = |cx: &mut TestAppContext| {
            app.read_with(cx, |app, _| app.effective_keystroke("ToggleSidebar"))
        };
        let recording = |cx: &mut TestAppContext| {
            app.read_with(cx, |app, _| app.settings_ui.kb_recording.clone())
        };
        let before = bound(cx);

        window.update_app(cx, |app, _, cx| {
            app.settings.settings_floating = false;
            app.show_settings = true;
            app.start_key_recording("ToggleSidebar".into(), cx);
            app.show_quit_confirm = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(main, "ctrl-alt-y");
        cx.simulate_keystrokes(main, "escape");
        cx.run_until_parked();
        assert!(!app.read_with(cx, |app, _| app.show_quit_confirm));
        assert_eq!(
            bound(cx),
            before,
            "a chord was bound under the Quit question"
        );
        assert_eq!(recording(cx).as_deref(), Some("ToggleSidebar"));
        cx.simulate_keystrokes(main, "ctrl-alt-u");
        cx.run_until_parked();
        assert_eq!(
            bound(cx),
            "ctrl-alt-u",
            "the row took no chord after the question"
        );
        assert_eq!(recording(cx), None);

        window.update_app(cx, |app, _, cx| {
            app.settings.settings_floating = true;
            app.float_settings(cx);
        });
        cx.run_until_parked();
        let settings: gpui::AnyWindowHandle = app
            .read_with(cx, |app, _| app.settings_window)
            .expect("Settings opened its own window")
            .into();
        window.update_app(cx, |app, _, cx| {
            app.start_key_recording("ToggleSidebar".into(), cx);
            let action = super::ConfirmAction::DeleteProject(project);
            app.request_confirm("Remove?", "", "Remove", action, cx);
        });
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.confirm_in_settings()));
        cx.simulate_keystrokes(settings, "ctrl-alt-i");
        cx.run_until_parked();
        assert_eq!(
            bound(cx),
            "ctrl-alt-u",
            "a chord was bound under Settings' own confirm"
        );
        assert_eq!(recording(cx).as_deref(), Some("ToggleSidebar"));
        cx.simulate_keystrokes(settings, "escape");
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.confirm.is_none()),
            "Escape left Settings' own confirm up"
        );
        cx.simulate_keystrokes(settings, "ctrl-alt-i");
        cx.run_until_parked();
        assert_eq!(
            bound(cx),
            "ctrl-alt-i",
            "the row took no chord after the confirm"
        );

        // The password prompt draws above the Quit question and keeps what is
        // typed into it; the row armed meanwhile binds none of it.
        window.update_app(cx, |app, window, cx| {
            app.start_key_recording("ToggleSidebar".into(), cx);
            let action = super::PasswordAction::Connect(project);
            app.prompt_password(Uuid::new_v4(), action, window, cx);
            app.show_quit_confirm = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(main, "p");
        cx.run_until_parked();
        assert_eq!(
            bound(cx),
            "ctrl-alt-i",
            "a key typed into the password prompt over Quit was bound"
        );
        assert_eq!(recording(cx).as_deref(), Some("ToggleSidebar"));
        app.read_with(cx, |app, cx| {
            let typed = app.password_prompt_input.read(cx).value().to_string();
            assert_eq!(typed, "p", "the password prompt lost its key");
        });
    });
}

#[test]
fn a_shortcut_row_records_the_quit_chord_when_the_selector_draws_no_question() {
    with_app_in_root(|cx, window| {
        app_keymap(cx);
        let quit_fired = Rc::new(Cell::new(0));
        cx.update(|cx| {
            let quit_fired = quit_fired.clone();
            cx.on_action(move |_: &super::Quit, _| {
                quit_fired.set(quit_fired.get() + 1);
            });
        });
        window.update_app(cx, |app, _, cx| {
            app.show_terms = false;
            app.show_workspace_selector = true;
            app.settings.settings_floating = true;
            app.show_settings = true;
            app.float_settings(cx);
        });
        cx.run_until_parked();
        let settings: gpui::AnyWindowHandle = window.update_app(cx, |app, _, cx| {
            // The OS close hook asks to quit even on the workspace selector.
            app.show_quit_confirm = true;
            app.start_key_recording("ToggleSidebar".into(), cx);
            app.settings_window
                .expect("Settings opened its own window")
                .into()
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(settings, "ctrl-q");
        cx.run_until_parked();
        assert_eq!(quit_fired.get(), 0, "the Quit chord dispatched Quit");
        assert_eq!(
            window.read_app(cx, |app, _| app.effective_keystroke("ToggleSidebar")),
            "ctrl-q",
            "the row did not record the Quit chord"
        );
    });
}
