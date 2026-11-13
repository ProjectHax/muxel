//! Ctrl+C and Ctrl+Insert copy what's selected in a rendered Markdown pane.
//! The keys take the route a person's do: the window is muxel's in a
//! gpui-component `Root`, muxel's own keymap is installed, a press on the pane
//! body focuses the pane, and a drag or a double or triple click selects.

use super::MuxelApp;
use crate::theme;
use gpui::{
    AppContext as _, Bounds, ClipboardItem, Entity, Modifiers, MouseButton, MouseDownEvent,
    MouseUpEvent, Pixels, Point, TestAppContext, VisualTestContext, point, px,
};
use muxel_core::{PaneNode, Project, SplitDirection, split};
use std::path::PathBuf;
use uuid::Uuid;

const LINE: &str = "Copy this line from the rendered view.";

/// Muxel's window with `notes.md` open in the rendered view and drawn. With
/// `beside`, a plain-text editor sits in a pane to its left and has focus, as
/// a terminal would when a person turns to the Markdown.
///
/// Fields drop in order: the app's handle before the context checks for
/// leaks, the app before its storage root, and the root's isolation before
/// the directory goes.
struct MdPane {
    app: Entity<MuxelApp>,
    visual: VisualTestContext,
    md: Uuid,
    _storage: muxel_store::test_support::TestRoot,
    _root: RemoveDir,
}

struct RemoveDir(PathBuf);

impl Drop for RemoveDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl MdPane {
    fn new(beside: bool) -> Self {
        let root = std::env::temp_dir().join(format!("muxel-md-copy-{}", Uuid::new_v4()));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("notes.md"), format!("{LINE}\n")).unwrap();
        std::fs::write(project.join("other.txt"), "other\n").unwrap();
        let (root, project) = (
            root.canonicalize().unwrap(),
            project.canonicalize().unwrap(),
        );
        let storage = muxel_store::test_support::set_root(root.clone());

        let mut cx = TestAppContext::single();
        cx.update(|cx| {
            gpui_component::init(cx);
            theme::register_bundled_themes(cx);
            cx.set_global(theme::UiScale(1.0));
            cx.set_global(theme::UiFontSize(14.0));
            theme::apply_initial_theme("", cx);
            super::install_keybindings(&muxel_core::Settings::default(), cx);
        });
        let mut app = None;
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|cx| MuxelApp::new(window, cx));
            app = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let app = app.unwrap();
        let mut visual = VisualTestContext::from_window(window.into(), &cx);
        let md = visual.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.show_terms = false;
                app.show_workspace_selector = false;
                app.current_workspace = Some(Uuid::new_v4());
                let pid = app
                    .workspace
                    .add_project(Project::new("md copy", project.clone()));
                app.workspace.active_project = Some(pid);
                app.file_browser_pid = Some(pid);
                app.open_browser_file(project.join("other.txt"), window, cx);
                let other = app.active_instance.expect("an editor opened");
                app.open_browser_file(project.join("notes.md"), window, cx);
                let md = app.active_instance.expect("an editor opened");
                assert!(app.editors[&md].read(cx).show_rendered());
                let layout = &mut app.workspace.project_mut(pid).unwrap().layout;
                if beside {
                    *layout = Some(PaneNode::leaf(other));
                    assert!(split(layout, other, SplitDirection::Horizontal, md));
                    app.focus_instance(other, window, cx);
                } else {
                    *layout = Some(PaneNode::leaf(md));
                    app.focus_instance(md, window, cx);
                }
                md
            })
        });
        let mut pane = Self {
            visual,
            app,
            md,
            _storage: storage,
            _root: RemoveDir(root),
        };
        pane.draw();
        pane
    }

    fn draw(&mut self) {
        self.visual.run_until_parked();
        self.visual.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    /// A point on the rendered view's first line, `x` in from its left and
    /// short of its right edge.
    fn at(&mut self, x: f32) -> Point<Pixels> {
        let md: Bounds<Pixels> = self
            .visual
            .debug_bounds("md-view")
            .expect("the rendered view is drawn");
        let (x, limit) = (px(x), md.size.width - px(20.));
        let x = if x < limit { x } else { limit };
        point(md.left() + x, md.top() + px(26.))
    }

    /// Press at the line's start, inside the view's padding, and drag across
    /// the text to past its end.
    fn drag_across_the_line(&mut self) {
        let (start, across, past_end) = (self.at(6.), self.at(60.), self.at(600.));
        self.visual
            .simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        self.draw();
        for to in [across, past_end] {
            self.visual
                .simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
            self.draw();
        }
        self.visual
            .simulate_mouse_up(past_end, MouseButton::Left, Modifiers::default());
        self.draw();
    }

    /// Click `count` times on the line's first word: twice selects the word,
    /// three times the paragraph.
    fn multi_click_the_first_word(&mut self, count: usize) {
        let position = self.at(20.);
        for click_count in 1..=count {
            self.visual.simulate_event(MouseDownEvent {
                position,
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count,
                first_mouse: false,
            });
            self.visual.simulate_event(MouseUpEvent {
                position,
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count,
            });
            self.draw();
        }
    }

    /// Pop the pane out into its own window and work there from now on.
    fn pop_out(&mut self) {
        let (app, md) = (self.app.clone(), self.md);
        let popout = self.visual.update(|window, cx| {
            app.update(cx, |app, cx| {
                app.pop_out_instance(md, window, cx);
                app.popouts[&md].window
            })
        });
        self.visual = VisualTestContext::from_window(popout.into(), &self.visual.cx);
        self.draw();
    }

    /// What `chord` puts on the clipboard, and where the keys went.
    fn copy(&mut self, chord: &str) -> (Option<String>, String) {
        let route = self.route();
        self.visual
            .write_to_clipboard(ClipboardItem::new_string("before".into()));
        self.visual.simulate_keystrokes(chord);
        let copied = self
            .visual
            .read_from_clipboard()
            .and_then(|item| item.text())
            .filter(|text| text != "before");
        (copied, route)
    }

    /// Where the keys will go, for a failure message. An empty context stack
    /// means the focused handle isn't drawn, and the keys reach no binding.
    fn route(&mut self) -> String {
        self.visual.update(|window, cx| {
            use gpui_component::WindowExt as _;
            let contexts = window
                .context_stack()
                .iter()
                .map(|context| format!("{context:?}"))
                .collect::<Vec<_>>()
                .join(" > ");
            format!(
                "key contexts: [{contexts}]; window selection: {:?}",
                window.selected_text(cx)
            )
        })
    }

    fn assert_copies(&mut self, expected: &str) {
        for chord in ["ctrl-c", "ctrl-insert"] {
            let (copied, route) = self.copy(chord);
            assert_eq!(
                copied.as_deref().map(str::trim_end),
                Some(expected),
                "{chord} didn't copy the selection ({route})"
            );
        }
    }
}

#[test]
fn a_dragged_selection_copies_after_turning_from_another_pane() {
    let mut pane = MdPane::new(true);
    pane.drag_across_the_line();
    pane.assert_copies(LINE);
}

/// The second and third presses of a multi-click focus the pane after the
/// text has taken focus, and no drag follows to take it back.
#[test]
fn a_double_or_triple_clicked_selection_copies() {
    let mut pane = MdPane::new(true);
    pane.multi_click_the_first_word(2);
    pane.assert_copies("Copy");
    pane.multi_click_the_first_word(3);
    pane.assert_copies(LINE);
}

#[test]
fn a_selection_copies_in_a_popped_out_pane() {
    let mut pane = MdPane::new(false);
    pane.pop_out();
    pane.drag_across_the_line();
    pane.assert_copies(LINE);
}
