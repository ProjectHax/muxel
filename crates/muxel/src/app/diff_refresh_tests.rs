//! Diff panes run `git diff` only while on screen. Draws real frames with no
//! PTY. Each window is a gpui-component `Root`, as muxel opens it, since an
//! editor needs one to paint; on macOS that `Root` asks gpui's test window for
//! an NSView it can't give, so these run on Windows and Linux.
use super::{MuxelApp, Project, ResourceOrigin, SecondaryWindow, WorkspaceWindow};
use crate::editor::TEST_RENDERS;
use crate::integrations::git_launches;
use crate::test_support::TestRepo;
use crate::theme;
use gpui::{AnyWindowHandle, AppContext, Context, Entity, TestAppContext, Window, WindowHandle};
use gpui_component::Root;
use std::time::Duration;
use uuid::Uuid;

/// Longer than one tick of the diff timer.
const DIFF_TICK: Duration = Duration::from_millis(1600);

struct Fixture {
    pid: Uuid,
    a: TestRepo,
    b: TestRepo,
}

/// The main window as muxel opens it: the app inside a `Root`.
struct Main {
    window: WindowHandle<Root>,
    app: Entity<MuxelApp>,
}

impl Main {
    fn update<R>(
        &self,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut MuxelApp, &mut Window, &mut Context<MuxelApp>) -> R,
    ) -> R {
        let app = self.app.clone();
        self.window
            .update(cx, |_, window, cx| {
                app.update(cx, |app, cx| f(app, window, cx))
            })
            .unwrap()
    }
}

fn committed_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.commit(&[("notes.txt", "first\n")], "seed");
    repo
}

fn add_line(repo: &TestRepo) {
    std::fs::write(repo.path().join("notes.txt"), "first\nsecond\n").unwrap();
}

fn with_app(run: impl FnOnce(&mut TestAppContext, &Main, &Fixture)) {
    let root = std::env::temp_dir().join(format!("muxel-diff-refresh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let _storage = muxel_store::test_support::set_root(root.clone());
    let (a, b) = (committed_repo(), committed_repo());
    let mut cx = TestAppContext::single();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let mut app = None;
    let window = cx.add_window(|window, cx| {
        let view = cx.new(|cx| MuxelApp::new(window, cx));
        app = Some(view.clone());
        Root::new(view, window, cx)
    });
    let main = Main {
        window,
        app: app.unwrap(),
    };
    let pid = main.update(&mut cx, |app, _, cx| {
        // The diff timer stays live; the others would add unrelated work.
        app._status_timer = gpui::Task::ready(());
        app._update_timer = gpui::Task::ready(());
        // Past first run: the terms and workspace screens draw no panes.
        app.show_terms = false;
        app.show_workspace_selector = false;
        let pid = app
            .workspace
            .add_project(Project::new("diff fixture", a.path().to_path_buf()));
        app.workspace.active_project = Some(pid);
        app.current_workspace = Some(Uuid::new_v4());
        cx.notify();
        pid
    });
    cx.run_until_parked();
    run(&mut cx, &main, &Fixture { pid, a, b });
    drop(main);
    drop(cx);
    std::fs::remove_dir_all(root).unwrap();
}

fn open_diff(cx: &mut TestAppContext, main: &Main, fx: &Fixture, repo: &TestRepo) -> Uuid {
    let iid = main.update(cx, |app, window, cx| {
        // As the project menu opens it: into the nearest diff pane's group.
        let origin = ResourceOrigin::Chrome {
            near: app.active_instance,
        };
        app.open_diff_for_dir(fx.pid, repo.path().to_path_buf(), origin, window, cx);
        app.active_instance.expect("the new diff pane is focused")
    });
    cx.run_until_parked();
    iid
}

/// Two diff panes as tabs of one group, the second shown: A is hidden.
fn hidden_and_shown(cx: &mut TestAppContext, main: &Main, fx: &Fixture) -> (Uuid, Uuid) {
    let a = open_diff(cx, main, fx, &fx.a);
    let b = open_diff(cx, main, fx, &fx.b);
    main.update(cx, |app, _, _| {
        let layout = app.workspace.project(fx.pid).unwrap().layout.as_ref();
        assert_eq!(
            layout.map(|l| l.shown_tabs(app.active_instance, app.maximized)),
            Some(vec![b]),
            "A and B share one tab group with B in front"
        );
    });
    // And a frame really draws B and not A.
    TEST_RENDERS.with(|renders| renders.borrow_mut().clear());
    main.update(cx, |_, window, _| window.refresh());
    cx.run_until_parked();
    assert!(renders(cx, main, b) > 0, "the shown pane is drawn");
    assert_eq!(renders(cx, main, a), 0, "the hidden pane isn't");
    (a, b)
}

fn diff_text(cx: &mut TestAppContext, main: &Main, iid: Uuid) -> String {
    main.update(cx, |app, _, cx| {
        let (_, editor) = app
            .open_editor_resources()
            .into_iter()
            .find(|(id, _)| *id == iid)
            .expect("the diff pane is open");
        editor.read(cx).text(cx)
    })
}

/// How many times `iid`'s editor has rendered since [`TEST_RENDERS`] was
/// cleared.
fn renders(cx: &mut TestAppContext, main: &Main, iid: Uuid) -> usize {
    let editor = main
        .update(cx, |app, _, _| {
            app.open_editor_resources()
                .into_iter()
                .find(|(id, _)| *id == iid)
                .map(|(_, editor)| editor.entity_id())
        })
        .expect("the diff pane is open");
    TEST_RENDERS.with(|renders| renders.borrow().get(&editor).copied().unwrap_or(0))
}

/// git launches on this thread, the app's background work included, while
/// `run` runs.
fn launches_while<R>(run: impl FnOnce() -> R) -> usize {
    let before = git_launches();
    run();
    git_launches() - before
}

fn tick(cx: &mut TestAppContext) {
    cx.executor().advance_clock(DIFF_TICK);
    cx.run_until_parked();
}

/// A second window showing `pid`, as a monitor's window does.
fn secondary_window(cx: &mut TestAppContext, main: &Main, pid: Uuid) -> WindowHandle<Root> {
    let weak = main.app.downgrade();
    let display_uuid = Uuid::new_v4();
    let mut view = None;
    let secondary = cx.add_window(|window, cx| {
        let workspace = cx.new(|cx| WorkspaceWindow::new(weak, pid, display_uuid, window, cx));
        view = Some(workspace.clone());
        Root::new(workspace, window, cx)
    });
    let view = view.unwrap();
    let handle: AnyWindowHandle = secondary.into();
    main.app.update(cx, |app, _| {
        app.secondary_windows.push(SecondaryWindow {
            pid,
            display_uuid,
            window_id: handle.window_id(),
            handle,
            view,
            active: true,
        });
    });
    secondary
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_hidden_diff_pane_starts_no_git_until_it_is_shown() {
    with_app(|cx, main, fx| {
        let (a, b) = hidden_and_shown(cx, main, fx);
        add_line(&fx.a);
        add_line(&fx.b);
        // One pane's refresh, as B now stands.
        let one = launches_while(|| crate::integrations::git_diff(fx.b.path()));
        assert!(one > 0);
        let ticked = launches_while(|| tick(cx));
        assert_eq!(ticked, one, "only the shown pane ran git");
        assert!(
            diff_text(cx, main, b).contains("+second"),
            "the shown pane refreshes on the timer"
        );
        assert!(
            !diff_text(cx, main, a).contains("+second"),
            "the hidden pane ran no git"
        );
    });
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_diff_pane_shown_after_a_change_refreshes_at_once() {
    with_app(|cx, main, fx| {
        let (a, _) = hidden_and_shown(cx, main, fx);
        add_line(&fx.a);
        main.update(cx, |app, window, cx| app.focus_instance(a, window, cx));
        // Frames only: the timer doesn't tick.
        cx.run_until_parked();
        assert!(diff_text(cx, main, a).contains("+second"));
    });
}

/// The workspace selector and the terms screen take the place of the main
/// window's panes, so a diff pane under either runs no git, and refreshes as
/// soon as it's back.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_diff_pane_under_the_workspace_selector_or_terms_starts_no_git() {
    type Cover = fn(&mut MuxelApp, bool);
    let covers: [(&str, Cover); 2] = [
        ("workspace selector", |app, on| {
            app.show_workspace_selector = on
        }),
        ("terms", |app, on| app.show_terms = on),
    ];
    for (name, cover) in covers {
        with_app(|cx, main, fx| {
            let b = open_diff(cx, main, fx, &fx.b);
            let set = |cx: &mut TestAppContext, on| {
                main.update(cx, |app, _, cx| {
                    cover(app, on);
                    cx.notify();
                });
                cx.run_until_parked();
            };
            set(cx, true);
            add_line(&fx.b);
            assert_eq!(launches_while(|| tick(cx)), 0, "{name}: no git");
            assert!(!diff_text(cx, main, b).contains("+second"), "{name}");
            // Frames only: the timer doesn't tick.
            set(cx, false);
            assert!(
                diff_text(cx, main, b).contains("+second"),
                "{name}: back on screen, it refreshes at once"
            );
        });
    }
}

/// A diff pane a secondary window brings into view refreshes at once, even
/// while the main window draws nothing, as when it's minimized.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_secondary_window_refreshes_the_diff_it_shows_without_the_main_window() {
    with_app(|cx, main, fx| {
        let (a, _) = hidden_and_shown(cx, main, fx);
        // The fixture project moves to its own window; the main window shows
        // another.
        main.update(cx, |app, _, cx| {
            let other = app
                .workspace
                .add_project(Project::new("other", fx.b.path().to_path_buf()));
            app.workspace.active_project = Some(other);
            cx.notify();
        });
        let secondary = secondary_window(cx, main, fx.pid);
        cx.run_until_parked();
        add_line(&fx.a);
        // Bring A forward in the secondary window without notifying the app,
        // which would draw the main window too, then draw that window alone.
        main.update(cx, |app, _, _| {
            app.active_instance = Some(a);
            let project = app.workspace.project_mut(fx.pid).unwrap();
            muxel_core::set_active_tab(&mut project.layout, a);
        });
        let main_renders = super::MAIN_TEST_RENDERS.with(|count| count.get());
        secondary
            .update(cx, |_, window, _| window.refresh())
            .unwrap();
        // Frames only: the timer doesn't tick.
        cx.run_until_parked();
        assert_eq!(
            super::MAIN_TEST_RENDERS.with(|count| count.get()),
            main_renders,
            "the main window drew nothing"
        );
        assert!(
            diff_text(cx, main, a).contains("+second"),
            "the secondary window's own frame refreshed it"
        );
    });
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_popped_out_diff_pane_keeps_refreshing() {
    with_app(|cx, main, fx| {
        let a = open_diff(cx, main, fx, &fx.a);
        main.update(cx, |app, window, cx| app.pop_out_instance(a, window, cx));
        cx.run_until_parked();
        main.update(cx, |app, _, _| assert!(app.popouts.contains_key(&a)));
        add_line(&fx.a);
        tick(cx);
        assert!(diff_text(cx, main, a).contains("+second"));
    });
}

/// Docking a diff back builds it anew in the main window, from the text it
/// had in its pop-out; it refreshes in the frame that builds it.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_diff_pane_docked_back_refreshes_at_once() {
    with_app(|cx, main, fx| {
        let a = open_diff(cx, main, fx, &fx.a);
        main.update(cx, |app, window, cx| app.pop_out_instance(a, window, cx));
        cx.run_until_parked();
        add_line(&fx.a);
        // As the pop-out's Dock button does.
        main.update(cx, |app, _, cx| app.redock_popout(a, cx));
        // Frames only: the timer doesn't tick.
        cx.run_until_parked();
        main.update(cx, |app, _, _| {
            assert!(!app.popouts.contains_key(&a));
            assert!(app.editors.contains_key(&a), "rebuilt in the main window");
        });
        assert!(diff_text(cx, main, a).contains("+second"));
    });
}
