//! Where links, files and new agents land in the running app: beside the pane
//! a link was clicked in, in the window a toolbar click or shortcut came from,
//! and never behind a maximized pane. Focus stays with the window that shows
//! the pane holding it.

use super::popout_lifecycle_tests::{with_app, with_app_drawing};
use super::{
    AgentPreset, Instance, MuxelApp, OpenFocus, PaneNode, Project, ResourceOrigin, SecondaryWindow,
    WorkspaceWindow,
};
use gpui::{
    AnyWindowHandle, AppContext, Context, FocusHandle, Focusable, TestAppContext, Window,
    WindowHandle,
};
use muxel_core::{SplitDirection, split};
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalView};
use std::path::{Path, PathBuf};
use std::time::Instant;
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
/// with no process behind it. One started later runs the inert command.
fn pane(app: &mut MuxelApp, pid: Uuid, beside: Option<Uuid>) -> Uuid {
    let mut instance = Instance::shell(pid);
    instance.program = Some("cmd.exe".into());
    instance.args = WAIT.map(String::from).into();
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

/// The tab at the front of `iid`'s group.
fn front_tab(app: &MuxelApp, pid: Uuid, iid: Uuid) -> Uuid {
    let layout = app.workspace.project(pid).unwrap().layout.as_ref();
    match layout.unwrap().leaf_containing(iid) {
        Some(PaneNode::Leaf(leaf)) => leaf.tabs[leaf.active],
        _ => panic!("{iid} is in no group"),
    }
}

/// What `window` has focused.
fn focused(cx: &mut TestAppContext, window: impl Into<AnyWindowHandle>) -> Option<FocusHandle> {
    cx.update_window(window.into(), |_, window, cx| window.focused(cx))
        .unwrap()
}

fn editor_focus(cx: &mut TestAppContext, main: WindowHandle<MuxelApp>, iid: Uuid) -> FocusHandle {
    main.read_with(cx, |app, cx| app.editors[&iid].read(cx).focus_handle(cx))
        .unwrap()
}

/// Open `path` in project `pid` from window `window`, as the file browser does.
fn open_file(
    cx: &mut TestAppContext,
    main: WindowHandle<MuxelApp>,
    window: impl Into<AnyWindowHandle>,
    pid: Uuid,
    path: &Path,
    origin: ResourceOrigin,
) -> Uuid {
    let app = main.entity(cx).unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        app.update(cx, |app, cx| {
            let path = Some(path.to_path_buf());
            app.open_editor_at(pid, path, origin, OpenFocus::Focus, window, cx)
        })
    })
    .unwrap()
    .unwrap()
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
                let path = Some(path.clone());
                app.open_editor_at(pid, path, origin, OpenFocus::Focus, window, cx)
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

#[test]
fn a_background_open_leaves_each_windows_focus_its_own() {
    with_app(|cx, main| {
        let shown = project(cx, main, "main window");
        let other = project(cx, main, "second window");
        let (a_path, c_path, d_path) = main
            .update(cx, |app, _, _| {
                (
                    text_file(app, shown, "a.txt"),
                    text_file(app, other, "c.txt"),
                    text_file(app, other, "d.txt"),
                )
            })
            .unwrap();
        let near = ResourceOrigin::Chrome { near: None };
        let a = open_file(cx, main, main, shown, &a_path, near);
        let secondary = secondary_window(cx, main, other);
        let b = main.update(cx, |app, _, _| pane(app, other, None)).unwrap();
        let c = open_file(cx, main, secondary, other, &c_path, ResourceOrigin::Pane(b));
        // The main window's pane has the app's focus; the second window keeps C.
        main.update(cx, |app, window, cx| app.focus_instance(a, window, cx))
            .unwrap();
        let (a_focus, c_focus) = (editor_focus(cx, main, a), editor_focus(cx, main, c));
        assert_eq!(focused(cx, secondary), Some(c_focus.clone()));

        // A middle-click on a path in B, in the second window.
        let app = main.entity(cx).unwrap();
        let link = muxel_terminal::OpenLinkBackground {
            url: muxel_terminal::file_uri(&d_path),
            pane: b,
        };
        secondary
            .update(cx, |_, window, cx| {
                app.update(cx, |app, cx| app.on_open_link_background(&link, window, cx))
            })
            .unwrap();

        assert_eq!(
            focused(cx, secondary),
            Some(c_focus.clone()),
            "the second window lost its own focus"
        );
        assert_eq!(focused(cx, main), Some(a_focus.clone()));
        main.read_with(cx, |app, _| {
            assert_eq!(app.active_instance, Some(a));
            assert_eq!(app.editors.len(), 3, "the link opened no editor");
            let tabs = group(app, other, c);
            assert_eq!(tabs.len(), 2, "beside B, in C's group");
            assert_eq!(
                front_tab(app, other, c),
                c,
                "the focused tab stays in front"
            );
        })
        .unwrap();

        // The same path middle-clicked in the main window reuses that file,
        // and the second window's focused tab still stays in front of it.
        let caller = main
            .update(cx, |app, _, _| pane(app, shown, Some(a)))
            .unwrap();
        main.update(cx, |app, window, cx| {
            let again = muxel_terminal::OpenLinkBackground {
                url: muxel_terminal::file_uri(&d_path),
                pane: caller,
            };
            app.on_open_link_background(&again, window, cx);
        })
        .unwrap();
        assert_eq!(focused(cx, secondary), Some(c_focus));
        assert_eq!(focused(cx, main), Some(a_focus));
        main.read_with(cx, |app, _| {
            assert_eq!(app.editors.len(), 3, "the file opened twice");
            assert_eq!(
                front_tab(app, other, c),
                c,
                "the second window's focused tab was hidden"
            );
        })
        .unwrap();
    });
}

#[test]
fn a_link_in_a_popped_out_terminal_opens_in_its_projects_window() {
    with_app_drawing(true, |cx, main| {
        let shown = project(cx, main, "main window");
        let pid = project(cx, main, "popped out");
        let (term, view) = main
            .update(cx, |app, window, cx| {
                let mut instance = Instance::shell(pid);
                instance.program = Some("cmd.exe".into());
                instance.args = WAIT.map(String::from).into();
                let iid = instance.id;
                let spec = CommandSpec::program("cmd.exe", instance.args.clone());
                app.workspace.add_instance(instance);
                let launch = TerminalLaunch::spawn(spec, (80, 24)).expect("spawn the terminal");
                let view = cx.new(|cx| TerminalView::new(launch, iid, Instant::now(), window, cx));
                app.terminals.insert(iid, view.clone());
                app.pop_out_instance(iid, window, cx);
                let display = cx.displays().into_iter().next().expect("a test display");
                app.open_project_on_monitor(pid, display.id(), display.uuid().unwrap(), window, cx);
                (iid, view)
            })
            .unwrap();
        cx.run_until_parked();
        let (popout, secondary) = main
            .read_with(cx, |app, _| {
                let own = app.secondary_windows.iter().find(|s| s.pid == pid);
                (
                    app.popouts[&term].window,
                    own.expect("the project's window").handle,
                )
            })
            .unwrap();
        let drawn = super::POPOUT_TEST_RENDERS.with(|r| r.borrow().get(&term).copied());
        assert!(drawn.is_some(), "the popout never drew its terminal");
        let term_focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        let (first, second) = main
            .read_with(cx, |app, _| {
                (
                    text_file(app, pid, "first.txt"),
                    text_file(app, pid, "second.txt"),
                )
            })
            .unwrap();
        // A click in the terminal raises the link from its own focus node.
        let click = |cx: &mut TestAppContext, action: &dyn gpui::Action| {
            popout
                .update(cx, |_, window, cx| {
                    term_focus.dispatch_action(action, window, cx)
                })
                .unwrap();
            cx.run_until_parked();
        };

        let link = muxel_terminal::OpenLink {
            url: muxel_terminal::file_uri(&first),
            pane: term,
        };
        click(cx, &link);
        let opened = main
            .read_with(cx, |app, _| {
                assert_eq!(app.editors.len(), 1, "the link opened nothing");
                let opened = *app.editors.keys().next().unwrap();
                assert_eq!(group(app, pid, opened), [opened]);
                assert!(app.workspace.project(shown).unwrap().layout.is_none());
                opened
            })
            .unwrap();
        let opened_focus = editor_focus(cx, main, opened);
        assert_eq!(focused(cx, secondary), Some(opened_focus.clone()));
        assert_eq!(focused(cx, popout), Some(term_focus.clone()));

        // A middle-click opens beside it and moves no focus.
        let link = muxel_terminal::OpenLinkBackground {
            url: muxel_terminal::file_uri(&second),
            pane: term,
        };
        click(cx, &link);
        main.read_with(cx, |app, _| {
            assert_eq!(app.editors.len(), 2, "the link opened nothing");
            assert_eq!(group(app, pid, opened).len(), 2);
        })
        .unwrap();
        assert_eq!(focused(cx, secondary), Some(opened_focus));
        assert_eq!(focused(cx, popout), Some(term_focus.clone()));
    });
}

#[test]
fn reopening_a_file_from_another_project_shows_its_project() {
    with_app(|cx, main| {
        let first = project(cx, main, "first");
        let second = project(cx, main, "second");
        let notes = main
            .update(cx, |app, _, _| text_file(app, first, "notes.txt"))
            .unwrap();
        let near = ResourceOrigin::Chrome { near: None };
        let open = open_file(cx, main, main, first, &notes, near);
        let caller = main
            .update(cx, |app, window, cx| {
                app.select_project(second, window, cx);
                pane(app, second, None)
            })
            .unwrap();

        // A Ctrl+click on its path in the second project, in the same window.
        main.update(cx, |app, window, cx| {
            let link = muxel_terminal::OpenLink {
                url: muxel_terminal::file_uri(&notes),
                pane: caller,
            };
            app.on_open_link(&link, window, cx);
        })
        .unwrap();

        main.read_with(cx, |app, _| {
            assert_eq!(app.editors.len(), 1, "the file opened twice");
            assert_eq!(
                app.workspace.active_project,
                Some(first),
                "the file's project isn't shown"
            );
            assert_eq!(app.active_instance, Some(open));
        })
        .unwrap();
        assert_eq!(focused(cx, main), Some(editor_focus(cx, main, open)));
    });
}

#[test]
fn reopening_a_file_or_diff_shown_in_another_window_focuses_it_there() {
    with_app(|cx, main| {
        let first = project(cx, main, "first");
        let second = project(cx, main, "second");
        let caller = main
            .update(cx, |app, window, cx| {
                app.select_project(second, window, cx);
                pane(app, second, None)
            })
            .unwrap();
        let secondary = secondary_window(cx, main, first);
        let (notes, todo, root) = main
            .update(cx, |app, _, _| {
                let root = app.workspace.project(first).unwrap().root_path.clone();
                let notes = text_file(app, first, "notes.txt");
                (notes, text_file(app, first, "todo.txt"), root)
            })
            .unwrap();
        let seed = ResourceOrigin::Chrome { near: None };
        let notes_editor = open_file(cx, main, secondary, first, &notes, seed);
        let near = ResourceOrigin::Chrome {
            near: Some(notes_editor),
        };
        let app = main.entity(cx).unwrap();
        let diff = secondary
            .update(cx, |_, window, cx| {
                app.update(cx, |app, cx| {
                    app.open_diff_for_dir(first, root.clone(), near, window, cx);
                    app.active_instance.unwrap()
                })
            })
            .unwrap();
        // The second window's focus moves off both before they're reopened.
        open_file(cx, main, secondary, first, &todo, near);
        let main_focus = focused(cx, main);

        main.update(cx, |app, window, cx| {
            let link = muxel_terminal::OpenLink {
                url: muxel_terminal::file_uri(&notes),
                pane: caller,
            };
            app.on_open_link(&link, window, cx);
        })
        .unwrap();
        assert_eq!(
            focused(cx, main),
            main_focus,
            "the main window was handed the other window's pane"
        );
        assert_eq!(
            focused(cx, secondary),
            Some(editor_focus(cx, main, notes_editor))
        );
        main.read_with(cx, |app, _| {
            assert_eq!(app.editors.len(), 3, "the file opened twice");
            assert_eq!(app.workspace.active_project, Some(second));
            assert_eq!(app.active_instance, Some(notes_editor));
        })
        .unwrap();

        // The same for the project diff, reopened from the second project.
        let origin = ResourceOrigin::Pane(caller);
        main.update(cx, |app, window, cx| {
            app.open_diff_for_dir(second, root.clone(), origin, window, cx)
        })
        .unwrap();
        assert_eq!(focused(cx, main), main_focus);
        assert_eq!(focused(cx, secondary), Some(editor_focus(cx, main, diff)));
        main.read_with(cx, |app, _| {
            assert_eq!(app.editors.len(), 3, "the diff opened twice");
            assert_eq!(app.active_instance, Some(diff));
        })
        .unwrap();
    });
}
