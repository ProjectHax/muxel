//! A terminal no window displays must not redraw the window for its screen,
//! and must show its current screen when it's displayed again.

use super::popout_lifecycle_tests::with_app_drawing;
use super::{Instance, MuxelApp, Project};
use gpui::{AppContext, Entity, TestAppContext, WindowHandle};
use muxel_core::{PaneNode, SplitDirection, add_tab, set_active_tab, split};
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalView};
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use uuid::Uuid;

struct Panes {
    /// Shown in the focused group.
    focused: (Uuid, Entity<TerminalView>),
    /// Shown in the other group, which caches its terminal's paint.
    shown: (Uuid, Entity<TerminalView>),
    /// The other group's second tab, never shown.
    hidden: (Uuid, Entity<TerminalView>),
}

/// Prints `ready`, then `out <line>` for each line it reads. Its title never
/// changes, so only its screen does.
const ECHO: &str = "[Console]::WriteLine('ready'); while ($null -ne ($l = [Console]::ReadLine())) { [Console]::WriteLine('out ' + $l) }";

/// `focused | [shown, hidden]`, each running [`ECHO`].
fn panes(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Panes {
    let powershell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe")
        .to_string_lossy()
        .into_owned();
    let panes = window
        .update(cx, |app, window, cx| {
            let root = muxel_store::data_dir()
                .unwrap()
                .join(Uuid::new_v4().to_string());
            std::fs::create_dir_all(&root).unwrap();
            let pid = app
                .workspace
                .add_project(Project::new("hidden paint", root));
            app.workspace.active_project = Some(pid);
            app.current_workspace = Some(Uuid::new_v4());
            app.show_terms = false;
            app.show_workspace_selector = false;
            app.use_tmux = false;
            app.use_worktree = false;
            let mut terminal = || {
                let mut instance = Instance::shell(pid);
                instance.program = Some(powershell.clone());
                instance.args = vec!["-NoProfile".into(), "-Command".into(), ECHO.into()];
                let iid = instance.id;
                // A marker that never shows keeps output from reading as Working,
                // so status, and the sidebar's 1 Hz Working clock, stay put.
                let spec = CommandSpec::program(&powershell, instance.args.clone())
                    .with_markers(vec!["never shown".into()], Vec::new());
                let launch = TerminalLaunch::spawn(spec, (80, 24)).expect("spawn PowerShell");
                let view = cx.new(|cx| TerminalView::new(launch, iid, Instant::now(), window, cx));
                app.workspace.add_instance(instance);
                app.terminals.insert(iid, view.clone());
                (iid, view)
            };
            let panes = Panes {
                focused: terminal(),
                shown: terminal(),
                hidden: terminal(),
            };
            let layout = &mut app.workspace.project_mut(pid).unwrap().layout;
            *layout = Some(PaneNode::leaf(panes.focused.0));
            assert!(split(
                layout,
                panes.focused.0,
                SplitDirection::Horizontal,
                panes.shown.0
            ));
            assert!(add_tab(layout, panes.shown.0, panes.hidden.0));
            assert!(set_active_tab(layout, panes.shown.0));
            app.active_instance = Some(panes.focused.0);
            cx.notify();
            panes
        })
        .unwrap();
    // A pane shows its loading screen, not the terminal, until it prints.
    for (_, view) in [&panes.focused, &panes.shown, &panes.hidden] {
        wait_for_screen(cx, window, view, "ready");
    }
    settle(cx);
    panes
}

/// Send `line` and wait until the terminal prints it back.
fn print(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    view: &Entity<TerminalView>,
    line: &str,
) {
    view.read_with(cx, |view, _| {
        view.session().write_input(format!("{line}\r").as_bytes())
    });
    wait_for_screen(cx, window, view, &format!("out {line}"));
}

fn wait_for_screen(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    view: &Entity<TerminalView>,
    text: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !window
        .read_with(cx, |_, cx| view.read(cx).visible_text().contains(text))
        .unwrap()
    {
        assert!(
            Instant::now() < deadline,
            "{text:?} never reached the screen"
        );
        // The PTY reader is a real thread; the drain's coalescing timer is
        // GPUI's virtual clock.
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Let pending repaint timers fire.
fn settle(cx: &mut TestAppContext) {
    cx.executor().advance_clock(Duration::from_millis(500));
    cx.run_until_parked();
}

fn main_renders() -> usize {
    super::MAIN_TEST_RENDERS.with(|count| count.get())
}

fn displayed(cx: &mut TestAppContext, view: &Entity<TerminalView>) -> bool {
    view.read_with(cx, |view, _| view.is_displayed())
}

fn painted_current_screen(cx: &mut TestAppContext, view: &Entity<TerminalView>) -> bool {
    view.read_with(cx, |view, _| view.session().painted_current_screen())
}

/// How many times `entity` notifies. A terminal dirties a window only that way.
fn notifies<T: 'static>(cx: &mut TestAppContext, entity: &Entity<T>) -> Rc<Cell<usize>> {
    let count = Rc::<Cell<usize>>::default();
    let counter = count.clone();
    cx.update(|cx| {
        cx.observe(entity, move |_, _| counter.set(counter.get() + 1))
            .detach()
    });
    count
}

#[test]
fn a_streaming_hidden_terminal_leaves_the_main_window_clean() {
    with_app_drawing(true, |cx, window| {
        let Panes { shown, hidden, .. } = panes(cx, window);
        assert!(displayed(cx, &shown.1));
        assert!(!displayed(cx, &hidden.1));
        let shown_notifies = notifies(cx, &shown.1);
        let hidden_notifies = notifies(cx, &hidden.1);

        // The harness sees a repaint from the same kind of pane when it's shown.
        let before = main_renders();
        for n in 0..4 {
            print(cx, window, &shown.1, &format!("shown {n}"));
        }
        settle(cx);
        assert!(shown_notifies.get() > 0);
        assert!(
            main_renders() > before,
            "a shown terminal's output didn't redraw"
        );

        // Its screen changes eight times without one notify, so it can't dirty
        // the window. (The app's own periodic work still redraws now and then.)
        for n in 0..8 {
            print(cx, window, &hidden.1, &format!("hidden {n}"));
        }
        settle(cx);
        assert_eq!(hidden_notifies.get(), 0, "a hidden terminal repainted");
    });
}

#[test]
fn a_hidden_tab_shows_its_current_screen_when_selected() {
    with_app_drawing(true, |cx, window| {
        let Panes { shown, hidden, .. } = panes(cx, window);
        assert!(painted_current_screen(cx, &shown.1));

        // Hide the cached tab, then let it print.
        window
            .update(cx, |app, _, cx| {
                let pid = app.workspace.active_project.unwrap();
                let layout = &mut app.workspace.project_mut(pid).unwrap().layout;
                assert!(set_active_tab(layout, hidden.0));
                cx.notify();
            })
            .unwrap();
        settle(cx);
        assert!(!displayed(cx, &shown.1));
        print(cx, window, &shown.1, "while hidden");
        settle(cx);
        assert!(
            !painted_current_screen(cx, &shown.1),
            "a hidden terminal painted"
        );

        // Selecting it again draws the screen as it is now, without focusing it.
        window
            .update(cx, |app, _, cx| {
                let pid = app.workspace.active_project.unwrap();
                let layout = &mut app.workspace.project_mut(pid).unwrap().layout;
                assert!(set_active_tab(layout, shown.0));
                cx.notify();
            })
            .unwrap();
        settle(cx);
        assert!(displayed(cx, &shown.1));
        assert!(
            painted_current_screen(cx, &shown.1),
            "showed a stale screen"
        );

        // Shown again, its output repaints.
        print(cx, window, &shown.1, "shown again");
        settle(cx);
        assert!(painted_current_screen(cx, &shown.1));
    });
}
