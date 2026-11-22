//! Diff panes run `git diff` only while on screen, and a watched one only
//! when its files change. Draws real frames with no PTY. Each window is a
//! gpui-component `Root`, as muxel opens it, since an editor needs one to
//! paint; on macOS that `Root` asks gpui's test window for an NSView it can't
//! give, so these run on Windows and Linux.
use super::{GitSource, MuxelApp, Project, ResourceOrigin, SecondaryWindow, WorkspaceWindow};
use crate::editor::TEST_RENDERS;
use crate::file_watch::FileWatch;
use crate::file_watch::test_backend::TestBackend;
use crate::integrations::git_launches;
use crate::test_support::TestRepo;
use crate::theme;
use gpui::{AnyWindowHandle, AppContext, Context, Entity, TestAppContext, Window, WindowHandle};
use gpui_component::Root;
use muxel_core::diff_schedule::{BACKSTOP, MIN_GAP, POLL};
use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode};
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

/// Longer than the poll of a pane without a watch.
const DIFF_TICK: Duration = Duration::from_millis(1600);
/// Past a watched pane's quiet period and its gap after the last refresh,
/// short of the poll: only a watch's wake refreshes the pane within it.
const SOON: Duration = MIN_GAP.saturating_add(Duration::from_millis(100));
const _: () = assert!(SOON.as_millis() < POLL.as_millis());

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
        // No watcher, so panes poll while visible, unless a test hands
        // the app one (`watch_with`).
        app.file_watch = Some(None);
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
    wait(cx, DIFF_TICK);
}

fn wait(cx: &mut TestAppContext, time: Duration) {
    cx.executor().advance_clock(time);
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

/// Give the app a watcher whose events the test plays.
fn watch_with(cx: &mut TestAppContext, main: &Main) -> TestBackend {
    let backend = TestBackend::default();
    let (watch, signal) = FileWatch::start_with(|handler| backend.build(handler)).unwrap();
    main.update(cx, |app, _, cx| app.follow_file_watch(watch, signal, cx));
    backend
}

fn watched(cx: &mut TestAppContext, main: &Main, iid: Uuid) -> bool {
    main.update(cx, |app, _, _| app.diff_panes.sources[&iid].watch.is_some())
}

/// A's diff pane, watched through a test backend, past its first refresh
/// and the check after the watch began, so nothing is due but the backstop.
fn watched_diff(cx: &mut TestAppContext, main: &Main, fx: &Fixture) -> (TestBackend, Uuid) {
    let backend = watch_with(cx, main);
    let iid = open_diff(cx, main, fx, &fx.a);
    assert!(watched(cx, main, iid), "the pane watches its repository");
    wait(cx, 2 * MIN_GAP);
    (backend, iid)
}

fn send(backend: &TestBackend, kind: notify::EventKind, paths: &[PathBuf]) {
    let mut event = notify::Event::new(kind);
    for path in paths {
        event = event.add_path(path.clone());
    }
    backend.send(Ok(event));
}

fn modified(backend: &TestBackend, path: PathBuf) {
    send(
        backend,
        notify::EventKind::Modify(ModifyKind::Data(DataChange::Content)),
        &[path],
    );
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_tracked_edit_refreshes_a_visible_diff_without_a_tick() {
    with_app(|cx, main, fx| {
        let (backend, iid) = watched_diff(cx, main, fx);
        add_line(&fx.a);
        modified(&backend, fx.a.path().join("notes.txt"));
        wait(cx, SOON);
        assert!(diff_text(cx, main, iid).contains("+second"));
    });
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn writes_under_an_untracked_directory_start_no_git() {
    with_app(|cx, main, fx| {
        let (backend, iid) = watched_diff(cx, main, fx);
        // The tracked edit goes unreported, so only a refresh started by
        // the other writes, or a poll, would show it.
        add_line(&fx.a);
        let root = fx.a.path();
        send(
            &backend,
            notify::EventKind::Create(CreateKind::File),
            &[root.join("target/debug/out.o"), root.join("scratch.txt")],
        );
        modified(&backend, root.join("target"));
        modified(&backend, root.to_path_buf());
        wait(cx, 3 * POLL);
        assert!(!diff_text(cx, main, iid).contains("+second"), "no git ran");
        // The pane is live: its tracked file's event refreshes it.
        modified(&backend, root.join("notes.txt"));
        wait(cx, SOON);
        assert!(diff_text(cx, main, iid).contains("+second"));
    });
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_commit_on_the_branch_refreshes_the_diff() {
    with_app(|cx, main, fx| {
        let (backend, iid) = watched_diff(cx, main, fx);
        add_line(&fx.a);
        modified(&backend, fx.a.path().join("notes.txt"));
        wait(cx, SOON);
        assert!(diff_text(cx, main, iid).contains("+second"));
        fx.a.commit(&[], "second");
        // Only the branch's ref is reported moving, as git renames it into
        // place.
        let heads = fx.a.path().join(".git").join("refs").join("heads");
        send(
            &backend,
            notify::EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[heads.join("main.lock"), heads.join("main")],
        );
        wait(cx, SOON);
        assert!(!diff_text(cx, main, iid).contains("+second"));
    });
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_lost_diff_watch_refreshes_the_pane_and_watches_again() {
    with_app(|cx, main, fx| {
        let (backend, iid) = watched_diff(cx, main, fx);
        add_line(&fx.a);
        // As a checkout that moves the folder away and back reports it.
        send(
            &backend,
            notify::EventKind::Remove(RemoveKind::Folder),
            &[fx.a.path().to_path_buf()],
        );
        cx.run_until_parked();
        assert!(diff_text(cx, main, iid).contains("+second"), "at once");
        assert!(watched(cx, main, iid), "it planned again and watches");
        let notes = fx.a.path().join("notes.txt");
        std::fs::write(&notes, "first\nsecond\nthird\n").unwrap();
        modified(&backend, notes);
        wait(cx, SOON);
        assert!(
            diff_text(cx, main, iid).contains("+third"),
            "through the new watch"
        );
    });
}

/// `git add` of a new file whose index event the watcher lost: the file is
/// now tracked, but the pane's plan still lists the old files.
fn add_unreported(repo: &TestRepo) -> PathBuf {
    let added = repo.path().join("added.txt");
    std::fs::write(&added, "one\n").unwrap();
    repo.git(&["add", "added.txt"]);
    added
}

/// After the plan caught up, an edit to the newly tracked file wakes the
/// pane by itself.
fn edit_wakes(
    cx: &mut TestAppContext,
    main: &Main,
    backend: &TestBackend,
    iid: Uuid,
    added: PathBuf,
) {
    std::fs::write(&added, "one\nmore\n").unwrap();
    modified(backend, added);
    wait(cx, SOON);
    assert!(
        diff_text(cx, main, iid).contains("+more"),
        "the newly tracked file's edit refreshed the pane"
    );
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_watcher_overflow_re_plans_the_diff() {
    with_app(|cx, main, fx| {
        let (backend, iid) = watched_diff(cx, main, fx);
        let added = add_unreported(&fx.a);
        backend.send(Ok(
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan)
        ));
        wait(cx, SOON);
        assert!(
            diff_text(cx, main, iid).contains("+one"),
            "the rescan refreshed"
        );
        edit_wakes(cx, main, &backend, iid, added);
    });
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn the_backstop_re_plans_after_a_lost_index_event() {
    with_app(|cx, main, fx| {
        let (backend, iid) = watched_diff(cx, main, fx);
        let added = add_unreported(&fx.a);
        wait(cx, BACKSTOP);
        assert!(
            diff_text(cx, main, iid).contains("+one"),
            "the backstop refreshed"
        );
        edit_wakes(cx, main, &backend, iid, added);
    });
}

/// `git add` of a new file, reported: the re-plan that follows watches the
/// file from then on, and refreshes once more, since a write to it between
/// the re-plan's `git diff` and the new watch went unrouted.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_re_plan_that_changes_the_watch_refreshes_once_more() {
    with_app(|cx, main, fx| {
        let (backend, iid) = watched_diff(cx, main, fx);
        let added = add_unreported(&fx.a);
        let git = fx.a.path().join(".git");
        send(
            &backend,
            notify::EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[git.join("index.lock"), git.join("index")],
        );
        wait(cx, SOON);
        assert!(
            diff_text(cx, main, iid).contains("+one"),
            "the re-plan refreshed"
        );
        // Unreported, as a write before the new watch would be.
        std::fs::write(&added, "one\nmore\n").unwrap();
        wait(cx, SOON);
        assert!(
            diff_text(cx, main, iid).contains("+more"),
            "it refreshed once more"
        );
    });
}

/// A tracked folder reported removed and made again, where each folder is
/// its own watch (inotify). The re-plan finds the same files but watches
/// the folder afresh, so it refreshes once more for a write before then.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_re_plan_that_moves_only_the_watch_refreshes_once_more() {
    crate::file_watch::PER_DIR.set(Some(true));
    with_app(|cx, main, fx| {
        fx.a.commit(&[("src/lib.rs", "")], "src");
        let (backend, iid) = watched_diff(cx, main, fx);
        let src = fx.a.path().join("src");
        let folder = std::slice::from_ref(&src);
        send(
            &backend,
            notify::EventKind::Remove(RemoveKind::Folder),
            folder,
        );
        send(
            &backend,
            notify::EventKind::Create(CreateKind::Folder),
            folder,
        );
        wait(cx, SOON);
        // Unreported, as a write before the new watch would be.
        std::fs::write(src.join("lib.rs"), "// edited\n").unwrap();
        wait(cx, SOON);
        assert!(
            diff_text(cx, main, iid).contains("+// edited"),
            "it refreshed once more"
        );
    });
}

/// An edit inside a submodule changes the diff (`-dirty`) without touching a
/// file the plan tracks, so a repository with one polls instead of watching.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_repository_with_a_submodule_polls() {
    with_app(|cx, main, fx| {
        fx.a.git(&[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &fx.b.path().to_string_lossy(),
            "sub",
        ]);
        fx.a.commit(&[], "add sub");
        let backend = watch_with(cx, main);
        let iid = open_diff(cx, main, fx, &fx.a);
        assert!(!watched(cx, main, iid), "the pane polls");
        let inside = fx.a.path().join("sub").join("notes.txt");
        std::fs::write(&inside, "first\nsecond\n").unwrap();
        modified(&backend, inside);
        tick(cx);
        assert!(diff_text(cx, main, iid).contains("-dirty"));
    });
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
        // One pane's poll, as B now stands: its top level is already known.
        let toplevel = crate::integrations::diff_plan(fx.b.path(), None)
            .expect("a work tree")
            .toplevel;
        let one =
            launches_while(|| crate::integrations::git_diff(fx.b.path(), Some(&toplevel), None));
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

// The Changes view: its lists are read off the UI thread, and the project they
// were read for may no longer be the one shown when they land.

/// A second project, on repository B, beside the fixture's on A.
fn project_on_b(cx: &mut TestAppContext, main: &Main, fx: &Fixture) -> Uuid {
    main.update(cx, |app, _, _| {
        app.workspace
            .add_project(Project::new("second fixture", fx.b.path().to_path_buf()))
    })
}

/// The paths the Changes view lists, or `None` while it shows none.
fn changes_shown(cx: &mut TestAppContext, main: &Main) -> Option<Vec<String>> {
    main.update(cx, |app, _, _| {
        app.shown_git_diff_files()
            .map(|files| files.iter().map(|file| file.path.clone()).collect())
    })
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn changes_read_for_one_project_dont_land_in_the_next() {
    with_app(|cx, main, fx| {
        add_line(&fx.a);
        let b = project_on_b(cx, main, fx);
        // A's list is still being read when B is chosen.
        main.update(cx, |app, window, cx| {
            app.refresh_git_diff_panel(cx);
            app.select_project(b, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(changes_shown(cx, main), None, "A's changes landed in B");
        // B's own read lists B's.
        main.update(cx, |app, _, cx| app.refresh_git_diff_panel(cx));
        cx.run_until_parked();
        assert_eq!(changes_shown(cx, main), Some(Vec::new()));
    });
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn changes_read_before_the_folder_moved_dont_land() {
    with_app(|cx, main, fx| {
        add_line(&fx.a);
        // As Settings > Projects > Change folder does, while A is read.
        main.update(cx, |app, _, cx| {
            app.refresh_git_diff_panel(cx);
            app.workspace.project_mut(fx.pid).unwrap().root_path = fx.b.path().to_path_buf();
        });
        cx.run_until_parked();
        assert_eq!(
            changes_shown(cx, main),
            None,
            "the old folder's changes landed"
        );
    });
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn switching_projects_stops_showing_the_last_ones_changes() {
    with_app(|cx, main, fx| {
        add_line(&fx.a);
        let b = project_on_b(cx, main, fx);
        main.update(cx, |app, _, cx| app.refresh_git_diff_panel(cx));
        cx.run_until_parked();
        assert_eq!(changes_shown(cx, main), Some(vec!["notes.txt".to_string()]));
        // Until B's own read lands, B has nothing to show.
        main.update(cx, |app, window, cx| app.select_project(b, window, cx));
        assert_eq!(changes_shown(cx, main), None, "B shows A's changes");
    });
}

/// A project can be opened in a folder below its repository's root, and git
/// lists its changes from the root, files outside the folder included. Open
/// file finds each listed file, and the file browser marks the folder's.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_project_below_its_repositorys_root_finds_its_listed_files() {
    with_app(|cx, main, fx| {
        let repo = fx.a.path();
        fx.a.commit(
            &[("app/src/main.rs", "one\n"), ("docs/notes.md", "one\n")],
            "layout",
        );
        let (in_folder, outside) = (repo.join("app/src/main.rs"), repo.join("docs/notes.md"));
        std::fs::write(&in_folder, "two\n").unwrap();
        std::fs::write(&outside, "two\n").unwrap();
        let folder = repo.join("app");
        main.update(cx, |app, _, cx| {
            app.workspace.project_mut(fx.pid).unwrap().root_path = folder.clone();
            app.refresh_git_diff_panel(cx);
            app.load_file_browser(fx.pid, cx);
        });
        cx.run_until_parked();
        let mut listed = changes_shown(cx, main).expect("the folder's changes are read");
        listed.sort();
        assert_eq!(listed, ["app/src/main.rs", "docs/notes.md"]);
        let (opened, marked) = main.update(cx, |app, _, _| {
            (
                ["app/src/main.rs", "docs/notes.md"].map(|rel| {
                    app.changes_file(GitSource::Project(fx.pid), rel)
                        .map(|(_, path)| path)
                }),
                app.file_browser_status
                    .get(&folder.join("src").join("main.rs"))
                    .cloned(),
            )
        });
        let resolved =
            |path: Option<&PathBuf>| path.and_then(|path| std::fs::canonicalize(path).ok());
        assert_eq!(
            opened.each_ref().map(|path| resolved(path.as_ref())),
            [&in_folder, &outside].map(|path| resolved(Some(path))),
            "Open file: {opened:?}"
        );
        assert_eq!(marked.as_deref(), Some(" M"), "the browser's mark");
    });
}

/// Initializing a new repository inside a project folder that previously sat
/// below an outer repository's root must not leave Open file using the outer
/// repository's cached prefix after a refresh.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn refreshing_changes_rereads_the_folders_repository_place() {
    with_app(|cx, main, fx| {
        let repo = fx.a.path();
        fx.a.commit(&[("app/src/main.rs", "one\n")], "layout");
        let folder = repo.join("app");
        let file = folder.join("src").join("main.rs");
        std::fs::write(&file, "two\n").unwrap();
        main.update(cx, |app, _, cx| {
            app.workspace.project_mut(fx.pid).unwrap().root_path = folder.clone();
            app.refresh_git_diff_panel(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            changes_shown(cx, main),
            Some(vec!["app/src/main.rs".to_string()])
        );

        // Now initialize an inner repository in the same project folder and
        // refresh Changes without changing project id or folder path.
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "test@muxel"],
            &["config", "user.name", "muxel test"],
            &["config", "core.autocrlf", "false"],
            &["add", "."],
            &["commit", "-q", "-m", "inner"],
        ] {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&folder)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::fs::write(&file, "three\n").unwrap();
        main.update(cx, |app, _, cx| app.refresh_git_diff_panel(cx));
        cx.run_until_parked();

        assert_eq!(
            changes_shown(cx, main),
            Some(vec!["src/main.rs".to_string()])
        );
        let opened = main.update(cx, |app, _, _| {
            app.changes_file(GitSource::Project(fx.pid), "src/main.rs")
                .map(|(_, path)| path)
        });
        assert_eq!(
            opened
                .as_ref()
                .and_then(|path| std::fs::canonicalize(path).ok()),
            std::fs::canonicalize(&file).ok(),
            "Open file resolved against stale outer prefix: {opened:?}"
        );
    });
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn worktree_lists_read_for_one_project_dont_land_in_the_next() {
    with_app(|cx, main, fx| {
        fx.a.git(&["branch", "only-in-a"]);
        let b = project_on_b(cx, main, fx);
        main.update(cx, |app, window, cx| {
            for (pid, repo) in [(fx.pid, &fx.a), (b, &fx.b)] {
                app.workspace.add_worktree(muxel_core::Worktree {
                    id: Uuid::new_v4(),
                    project_id: pid,
                    name: "fixture".into(),
                    path: repo.path().to_path_buf(),
                    branch: "main".into(),
                    color: 0,
                    detached: false,
                    base: None,
                });
            }
            // A's worktrees and branches are still being read when B is
            // chosen.
            app.refresh_git_diff_worktrees(cx);
            app.select_project(b, window, cx);
        });
        cx.run_until_parked();
        main.update(cx, |app, _, _| {
            assert!(
                app.git_diff_branches.is_empty(),
                "A's branches landed in B's Merge into menu: {:?}",
                app.git_diff_branches
            );
            assert!(app.git_diff_worktree_files.is_empty());
        });
    });
}

/// The Worktrees tab's "Merge into…" offers the branches read for the project
/// it shows: until B's own read lands, B offers none of A's.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn switching_projects_stops_offering_the_last_ones_branches() {
    with_app(|cx, main, fx| {
        fx.a.git(&["branch", "only-in-a"]);
        let b = project_on_b(cx, main, fx);
        main.update(cx, |app, _, cx| {
            for (pid, repo) in [(fx.pid, &fx.a), (b, &fx.b)] {
                app.workspace.add_worktree(muxel_core::Worktree {
                    id: Uuid::new_v4(),
                    project_id: pid,
                    name: "fixture".into(),
                    path: repo.path().to_path_buf(),
                    branch: "main".into(),
                    color: 0,
                    detached: false,
                    base: None,
                });
            }
            app.refresh_git_diff_worktrees(cx);
        });
        cx.run_until_parked();
        let offered = |cx: &mut TestAppContext| {
            main.update(cx, |app, _, _| app.shown_git_diff_branches().to_vec())
        };
        assert!(
            offered(cx).iter().any(|branch| branch == "only-in-a"),
            "A's read offers its branches: {:?}",
            offered(cx)
        );
        main.update(cx, |app, window, cx| app.select_project(b, window, cx));
        assert_eq!(offered(cx), Vec::<String>::new(), "B offers A's branches");
    });
}

/// A remote project's Changes are read in its folder on the host, so they go
/// when that folder changes, and stay when only its local label does.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_remote_projects_changes_follow_its_folder_on_the_host() {
    with_app(|cx, main, fx| {
        main.update(cx, |app, _, _| {
            app.workspace.project_mut(fx.pid).unwrap().remote = Some(muxel_core::RemoteRef {
                host_id: Uuid::new_v4(),
                remote_root: "/srv/one".into(),
            });
            let remote_root = |app: &mut MuxelApp, root: &str| {
                let project = app.workspace.project_mut(fx.pid).unwrap();
                project.remote.as_mut().unwrap().remote_root = root.into();
            };
            // As a read for the project landing.
            app.git_diff_files = Some(Vec::new());
            app.git_diff_files_from = app.changes_source();
            assert!(app.shown_git_diff_files().is_some());
            remote_root(app, "/srv/two");
            assert!(
                app.shown_git_diff_files().is_none(),
                "the old folder's changes still show"
            );
            remote_root(app, "/srv/one");
            app.workspace.project_mut(fx.pid).unwrap().root_path = fx.b.path().to_path_buf();
            assert!(
                app.shown_git_diff_files().is_some(),
                "a new label hid the changes of a folder that didn't move"
            );
        });
    });
}

/// A Changes row's Open file opens the listed file where git listed it: on
/// the host for a remote project, whatever its local label, with the host's
/// `/` between the folder and the path.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_remote_projects_changed_file_opens_on_the_host() {
    with_app(|cx, main, fx| {
        main.update(cx, |app, _, _| {
            let project = app.workspace.project_mut(fx.pid).unwrap();
            project.remote = Some(muxel_core::RemoteRef {
                host_id: Uuid::new_v4(),
                remote_root: "/srv/app".into(),
            });
            app.git_diff_files_from = app.changes_source();
            app.git_diff_files_place = Some(crate::integrations::RepoPlace {
                top: "/srv/app".into(),
                prefix: String::new(),
            });
            let opened = |app: &MuxelApp| {
                app.changes_file(GitSource::Project(fx.pid), "src/main.rs")
                    .map(|(pid, path)| (pid, path.to_string_lossy().into_owned()))
            };
            let on_the_host = Some((fx.pid, "/srv/app/src/main.rs".to_string()));
            // As created, the label is the host's folder.
            app.workspace.project_mut(fx.pid).unwrap().root_path = PathBuf::from("/srv/app");
            assert_eq!(opened(app), on_the_host);
            app.workspace.project_mut(fx.pid).unwrap().root_path = fx.b.path().to_path_buf();
            assert_eq!(opened(app), on_the_host, "it opened the label's folder");
        });
    });
}

/// When a Changes list is shown without a known repository place (for example,
/// `git_place` failed during a refresh), Open file must decline to guess a
/// path from the project folder rather than opening a wrong location.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn changes_file_returns_none_when_place_is_unknown() {
    with_app(|cx, main, fx| {
        let repo = fx.a.path();
        fx.a.commit(&[("app/src/main.rs", "one\n")], "layout");
        let folder = repo.join("app");
        std::fs::write(folder.join("src").join("main.rs"), "two\n").unwrap();
        main.update(cx, |app, _, cx| {
            app.workspace.project_mut(fx.pid).unwrap().root_path = folder.clone();
            app.refresh_git_diff_panel(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            changes_shown(cx, main),
            Some(vec!["app/src/main.rs".to_string()])
        );

        let opened = main.update(cx, |app, _, _| {
            app.git_diff_files_place = None;
            assert!(app.shown_git_diff_files().is_some());
            app.changes_file(GitSource::Project(fx.pid), "app/src/main.rs")
        });
        assert_eq!(
            opened, None,
            "Open file guessed a path without a known place"
        );
    });
}

/// A muxel worktree of A on its own branch, made from `main` and registered
/// with `base`, holding one commit `main` doesn't have. Removed on drop.
struct WorktreeOfA {
    path: PathBuf,
    wid: Uuid,
    repo: PathBuf,
}

impl Drop for WorktreeOfA {
    fn drop(&mut self) {
        let path = self.path.to_string_lossy().into_owned();
        crate::test_support::git_out(&self.repo, &["worktree", "remove", "--force", &path]);
    }
}

fn worktree_of_a(
    cx: &mut TestAppContext,
    main: &Main,
    fx: &Fixture,
    base: Option<&str>,
) -> WorktreeOfA {
    let path = crate::test_support::short_temp_path();
    let shown = path.to_string_lossy().into_owned();
    fx.a.git(&["worktree", "add", "-q", "-b", "muxel/wt", &shown]);
    let worktree = WorktreeOfA {
        path: path.clone(),
        wid: Uuid::new_v4(),
        repo: fx.a.path().to_path_buf(),
    };
    std::fs::write(path.join("notes.txt"), "first\nsecond\n").unwrap();
    crate::test_support::git_out(&path, &["commit", "-q", "-am", "worktree work"]);
    let (wid, pid, base) = (worktree.wid, fx.pid, base.map(str::to_string));
    main.update(cx, |app, _, _| {
        app.workspace.add_worktree(muxel_core::Worktree {
            id: wid,
            project_id: pid,
            name: "fixture".into(),
            path,
            branch: "muxel/wt".into(),
            color: 0,
            detached: false,
            base,
        });
    });
    worktree
}

fn open_diff_at(cx: &mut TestAppContext, main: &Main, fx: &Fixture, dir: PathBuf) -> Uuid {
    let iid = main.update(cx, |app, window, cx| {
        let origin = ResourceOrigin::Chrome {
            near: app.active_instance,
        };
        app.open_diff_for_dir(fx.pid, dir, origin, window, cx);
        app.active_instance.expect("the new diff pane is focused")
    });
    cx.run_until_parked();
    iid
}

/// A worktree's diff pane compares it with the branch it came from, so the
/// worktree's commits show, not only what it hasn't committed. One made
/// before muxel recorded its base compares with the project's branch.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_worktree_diff_pane_shows_its_commits_against_its_base() {
    with_app(|cx, main, fx| {
        let wt = worktree_of_a(cx, main, fx, Some("main"));
        let iid = open_diff_at(cx, main, fx, wt.path.clone());
        tick(cx);
        let text = diff_text(cx, main, iid);
        assert!(text.contains("compared with main"), "{text}");
        assert!(text.contains("+second"), "{text}");

        // Made before bases were recorded: the project's branch stands in.
        let wid = wt.wid;
        main.update(cx, |app, _, cx| {
            app.workspace.worktree_mut(wid).unwrap().base = None;
            app.project_branches.insert(fx.pid, Some("main".into()));
            app.request_diff(iid, cx);
        });
        tick(cx);
        let text = diff_text(cx, main, iid);
        assert!(text.contains("compared with main"), "{text}");
        assert!(text.contains("+second"), "{text}");

        // With neither, it's HEAD's diff, as for any folder.
        main.update(cx, |app, _, cx| {
            app.project_branches.insert(fx.pid, None);
            app.request_diff(iid, cx);
        });
        tick(cx);
        let text = diff_text(cx, main, iid);
        assert!(!text.contains("+second"), "{text}");
    });
}

/// Merging a worktree into its base moves only the base's ref, and the
/// watched pane follows it: nothing is left to show.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn merging_a_worktree_into_its_base_refreshes_its_diff() {
    with_app(|cx, main, fx| {
        let backend = watch_with(cx, main);
        let wt = worktree_of_a(cx, main, fx, Some("main"));
        let iid = open_diff_at(cx, main, fx, wt.path.clone());
        assert!(watched(cx, main, iid), "the pane watches its worktree");
        wait(cx, 2 * MIN_GAP);
        assert!(diff_text(cx, main, iid).contains("+second"));
        fx.a.git(&["merge", "-q", "--ff-only", "muxel/wt"]);
        let heads = fx.a.path().join(".git").join("refs").join("heads");
        send(
            &backend,
            notify::EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[heads.join("main.lock"), heads.join("main")],
        );
        wait(cx, SOON);
        let text = diff_text(cx, main, iid);
        assert!(!text.contains("+second"), "{text}");
    });
}

/// The Worktrees tab lists what a worktree committed since its base beside
/// what it hasn't committed, and a committed file's diff window shows its
/// commits. One made before bases were recorded compares with the project's
/// branch; with neither, only its uncommitted files show.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn the_worktrees_tab_lists_a_worktrees_commits() {
    with_app(|cx, main, fx| {
        let wt = worktree_of_a(cx, main, fx, Some("main"));
        let wid = wt.wid;
        std::fs::write(wt.path.join("notes.txt"), "first\nsecond\nthird\n").unwrap();
        let read = |cx: &mut TestAppContext| {
            main.update(cx, |app, _, cx| app.refresh_git_diff_worktrees(cx));
            cx.run_until_parked();
            main.update(cx, |app, _, _| {
                app.git_diff_worktree_files
                    .get(&wid)
                    .cloned()
                    .expect("the worktree's lists were read")
            })
        };
        let listed = |list: &[crate::integrations::GitChange]| {
            list.iter()
                .map(|change| format!("{}{}", change.status, change.path))
                .collect::<Vec<_>>()
        };

        let changes = read(cx);
        assert_eq!(changes.base.as_deref(), Some("main"));
        assert_eq!(listed(&changes.committed), ["M notes.txt"]);
        assert_eq!(listed(&changes.uncommitted), [" Mnotes.txt"]);
        assert_eq!(changes.file_count(), 1, "a file in both lists counts once");

        // Its committed diff is the commit, without the edit on top, in a
        // window of its own beside the uncommitted one.
        let committed = GitSource::WorktreeCommits(wid);
        let window_text = |cx: &mut TestAppContext, src: GitSource| {
            let handle = main.update(cx, |app, _, cx| {
                app.open_file_diff_window(src, "notes.txt".into(), cx);
                app.diff_file_windows
                    .get(&(src, "notes.txt".to_string()))
                    .copied()
                    .expect("a diff window opened")
            });
            handle
                .update(cx, |root, _, cx| {
                    let view = root.view().clone().downcast::<super::FileDiffView>();
                    view.expect("a file diff").read(cx).unified_src.to_string()
                })
                .unwrap()
        };
        let text = window_text(cx, committed);
        assert!(
            text.contains("+second") && !text.contains("+third"),
            "{text}"
        );
        let text = window_text(cx, GitSource::Worktree(wid));
        assert!(
            text.contains("+third") && !text.contains("+second"),
            "{text}"
        );

        main.update(cx, |app, _, _| {
            app.workspace.worktree_mut(wid).unwrap().base = None;
            app.project_branches.insert(fx.pid, Some("main".into()));
        });
        assert_eq!(listed(&read(cx).committed), ["M notes.txt"]);

        main.update(cx, |app, _, _| {
            app.project_branches.insert(fx.pid, None);
        });
        let changes = read(cx);
        assert_eq!(changes.base, None);
        assert!(changes.committed.is_empty(), "{:?}", changes.committed);
        assert_eq!(listed(&changes.uncommitted), [" Mnotes.txt"]);
    });
}
