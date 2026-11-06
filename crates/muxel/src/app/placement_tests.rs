//! Where links, files and new agents land in the running app: beside the pane
//! a link was clicked in, in the window a toolbar click or shortcut came from,
//! and never behind a maximized pane.

use super::popout_lifecycle_tests::with_app;
use super::{
    AgentPreset, Instance, MuxelApp, PaneNode, Project, ResourceOrigin, SecondaryWindow,
    WorkspaceWindow,
};
use gpui::{AnyWindowHandle, Context, TestAppContext, Window, WindowHandle};
use muxel_core::{SplitDirection, split};
use std::path::PathBuf;
use uuid::Uuid;

/// An inert command for every pane a test starts: it waits on input until killed.
const WAIT: [&str; 4] = ["/d", "/q", "/c", "more"];

/// A project with an empty layout. The first one made is the main window's.
fn project(cx: &mut TestAppContext, main: WindowHandle<MuxelApp>, name: &str) -> Uuid {
    main.update(cx, |app, _, _| {
        let root = muxel_store::data_dir()
            .unwrap()
            .join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&root).unwrap();
        let pid = app.workspace.add_project(Project::new(name, root));
        app.workspace.active_project.get_or_insert(pid);
        app.current_workspace.get_or_insert_with(Uuid::new_v4);
        app.main_window = Some(main.into());
        app.show_terms = false;
        app.show_workspace_selector = false;
        app.use_tmux = false;
        app.use_worktree = false;
        app.presets = vec![AgentPreset {
            program: Some("cmd.exe".into()),
            args: WAIT.map(String::from).into(),
            ..AgentPreset::shell()
        }];
        app.current_preset = 0;
        pid
    })
    .unwrap()
}

/// A terminal pane in `pid` to the right of `beside` (or the layout's first),
/// with no process behind it.
fn pane(app: &mut MuxelApp, pid: Uuid, beside: Option<Uuid>) -> Uuid {
    let instance = Instance::shell(pid);
    let iid = instance.id;
    app.workspace.add_instance(instance);
    let layout = &mut app.workspace.project_mut(pid).unwrap().layout;
    match beside {
        Some(beside) => assert!(split(layout, beside, SplitDirection::Horizontal, iid)),
        None => *layout = Some(PaneNode::leaf(iid)),
    }
    iid
}

fn group(app: &MuxelApp, pid: Uuid, iid: Uuid) -> Vec<Uuid> {
    let layout = app.workspace.project(pid).unwrap().layout.as_ref();
    layout.unwrap().tab_group(iid).unwrap().to_vec()
}

/// A file in `pid`'s root, spelled as a terminal prints it: without the `\\?\`
/// prefix the test root's canonical path carries.
fn text_file(app: &MuxelApp, pid: Uuid, name: &str) -> PathBuf {
    let path = app.workspace.project(pid).unwrap().root_path.join(name);
    std::fs::write(&path, "notes\n").unwrap();
    PathBuf::from(path.to_string_lossy().trim_start_matches(r"\\?\"))
}

#[test]
fn a_link_opens_beside_the_pane_it_was_clicked_in_not_the_focused_one() {
    with_app(|cx, main| {
        let pid = project(cx, main, "repo");
        main.update(cx, |app, window, cx| {
            let x = pane(app, pid, None);
            let y = pane(app, pid, Some(x));
            let z = pane(app, pid, Some(y));
            let first = muxel_terminal::file_uri(&text_file(app, pid, "first.txt"));
            let second = muxel_terminal::file_uri(&text_file(app, pid, "second.txt"));

            // A Ctrl+click in Y raises the link before the click moves focus,
            // so X, the pane focused until then, is still the active one.
            app.active_instance = Some(x);
            let link = muxel_terminal::OpenLink {
                url: first,
                pane: y,
            };
            app.on_open_link(&link, window, cx);
            let opened = app.active_instance.unwrap();
            assert_eq!(app.editors.len(), 1, "the link opened no editor");
            assert_eq!(group(app, pid, opened), [z, opened], "beside Y, not X");
            assert_eq!(group(app, pid, y), [y]);

            // A middle-click lands in the same place and gives focus back to X.
            app.focus_instance(x, window, cx);
            let background = muxel_terminal::OpenLinkBackground {
                url: second,
                pane: y,
            };
            app.on_open_link_background(&background, window, cx);
            assert_eq!(app.editors.len(), 2, "the link opened no editor");
            assert_eq!(group(app, pid, z).len(), 3, "beside Y, not X");
            assert_eq!(group(app, pid, z)[..2], [z, opened]);
            assert_eq!(app.active_instance, Some(x));
        })
        .unwrap();
    });
}

/// A second window showing `pid`, as a monitor's window does.
fn secondary_window(
    cx: &mut TestAppContext,
    main: WindowHandle<MuxelApp>,
    pid: Uuid,
) -> WindowHandle<WorkspaceWindow> {
    let app = main.entity(cx).unwrap();
    let weak = app.downgrade();
    let display_uuid = Uuid::new_v4();
    let secondary =
        cx.add_window(|window, cx| WorkspaceWindow::new(weak, pid, display_uuid, window, cx));
    let view = secondary.entity(cx).unwrap();
    let handle: AnyWindowHandle = secondary.into();
    app.update(cx, |app, _| {
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
fn toolbar_and_shortcut_agents_open_in_the_window_they_came_from() {
    with_app(|cx, main| {
        let shown = project(cx, main, "main window");
        let other = project(cx, main, "second window");
        let (main_pane, other_pane) = main
            .update(cx, |app, _, _| {
                (pane(app, shown, None), pane(app, other, None))
            })
            .unwrap();
        let secondary = secondary_window(cx, main, other);
        let app = main.entity(cx).unwrap();
        let in_secondary =
            |cx: &mut TestAppContext,
             run: fn(&mut MuxelApp, &mut Window, &mut Context<MuxelApp>)| {
                secondary
                    .update(cx, |_, window, cx| {
                        app.update(cx, |app, cx| run(app, window, cx))
                    })
                    .unwrap();
            };
        let set_focus = |cx: &mut TestAppContext, iid: Uuid| {
            main.update(cx, |app, _, _| app.active_instance = Some(iid))
                .unwrap();
        };

        // Focus is in the second window: its toolbar button and Ctrl+Alt+1 open there.
        set_focus(cx, other_pane);
        in_secondary(cx, |app, window, cx| app.new_tab(window, cx));
        in_secondary(cx, |app, window, cx| app.new_agent_preset(1, window, cx));
        // Its toolbar acts on its own project even when focus is in the main window.
        set_focus(cx, main_pane);
        in_secondary(cx, |app, window, cx| app.new_tab(window, cx));
        // And the main window's shortcut acts on the main window's project.
        set_focus(cx, other_pane);
        main.update(cx, |app, window, cx| app.new_agent_preset(1, window, cx))
            .unwrap();

        main.read_with(cx, |app, _| {
            assert_eq!(group(app, other, other_pane).len(), 4);
            assert_eq!(group(app, shown, main_pane).len(), 2);
        })
        .unwrap();
    });
}

#[test]
fn a_file_opened_beside_a_maximized_pane_ends_the_maximize() {
    with_app(|cx, main| {
        let pid = project(cx, main, "repo");
        main.update(cx, |app, window, cx| {
            let opener = pane(app, pid, None);
            let beside = pane(app, pid, Some(opener));
            let notes = text_file(app, pid, "notes.txt");
            let open = |app: &mut MuxelApp,
                        window: &mut Window,
                        cx: &mut Context<MuxelApp>,
                        path: &PathBuf,
                        origin: ResourceOrigin| {
                app.open_editor_at(pid, Some(path.clone()), origin, window, cx)
                    .unwrap()
            };

            // The file joins the pane beside the opener, which the maximize hides.
            app.active_instance = Some(opener);
            app.maximized = Some(opener);
            let file = open(app, window, cx, &notes, ResourceOrigin::Pane(opener));
            assert_eq!(group(app, pid, file), [beside, file]);
            assert_eq!(app.maximized, None);

            // Opening it again finds it in that same hidden group.
            app.maximized = Some(opener);
            assert_eq!(
                open(app, window, cx, &notes, ResourceOrigin::Pane(opener)),
                file
            );
            assert_eq!(app.maximized, None);

            // A file joining the maximized group itself is already in view.
            app.maximized = Some(file);
            let todo = text_file(app, pid, "todo.txt");
            let near = ResourceOrigin::Chrome { near: Some(file) };
            let second = open(app, window, cx, &todo, near);
            assert_eq!(group(app, pid, second), [beside, file, second]);
            assert_eq!(app.maximized, Some(second));
        })
        .unwrap();
    });
}
