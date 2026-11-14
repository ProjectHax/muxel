//! "Copy as rich text" belongs to rendered Markdown only: not the Raw view, not
//! other text files, and not images (the other kind with a rendered view). It
//! converts in the background and writes the clipboard afterwards.

use super::{MuxelApp, PaletteCommand, RichClipboard};
use crate::theme;
use gpui::{App, Context, TestAppContext, Window};
use muxel_core::Project;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

fn offers_copy(app: &MuxelApp, cx: &App) -> bool {
    app.palette_commands(cx)
        .iter()
        .any(|cmd| matches!(cmd, PaletteCommand::CopyRichText))
}

/// Open `path` in an editor pane of project `pid`, the way the file browser does.
fn open(
    app: &mut MuxelApp,
    pid: Uuid,
    path: &Path,
    window: &mut Window,
    cx: &mut Context<MuxelApp>,
) -> Uuid {
    app.file_browser_pid = Some(pid);
    app.open_browser_file(path.to_path_buf(), window, cx);
    let iid = app.active_instance.expect("an editor opened");
    assert_eq!(
        app.workspace
            .instance(iid)
            .and_then(|i| i.editor_path.as_deref()),
        Some(path),
        "{path:?} is focused"
    );
    iid
}

/// A temporary root holding a `project` directory with `files`, returned as
/// (root, project).
fn project_with(files: &[(&str, &[u8])]) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("muxel-rich-copy-{}", Uuid::new_v4()));
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    for (name, bytes) in files {
        std::fs::write(project.join(name), bytes).unwrap();
    }
    (
        root.canonicalize().unwrap(),
        project.canonicalize().unwrap(),
    )
}

fn test_cx() -> TestAppContext {
    let mut cx = TestAppContext::single();
    cx.skip_drawing();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    cx
}

#[test]
fn copy_as_rich_text_is_offered_for_rendered_markdown_only() {
    let (root, project) = project_with(&[
        ("notes.md", b"# Draft\n\nHello **there**.\n"),
        ("notes.txt", b"plain\n"),
        ("shot.png", b"\x89PNG\r\n\x1a\n"),
    ]);
    let _storage = muxel_store::test_support::set_root(root.clone());
    let mut cx = test_cx();
    let window = cx.add_window(MuxelApp::new);
    window
        .update(&mut cx, |app, window, cx| {
            let pid = app
                .workspace
                .add_project(Project::new("rich-copy", project.clone()));
            let markdown = open(app, pid, &project.join("notes.md"), window, cx);
            assert!(offers_copy(app, cx), "rendered Markdown");
            app.editors[&markdown].update(cx, |editor, cx| editor.toggle_rendered(window, cx));
            assert!(!offers_copy(app, cx), "Markdown in Raw view");

            open(app, pid, &project.join("notes.txt"), window, cx);
            assert!(!offers_copy(app, cx), "a plain text file");

            let image = open(app, pid, &project.join("shot.png"), window, cx);
            assert!(app.editors[&image].read(cx).show_rendered());
            assert!(!offers_copy(app, cx), "a rendered image");
        })
        .unwrap();
    drop(cx);
    let _ = std::fs::remove_dir_all(&root);
}

/// Records what "Copy as rich text" writes, in place of the system clipboard.
#[derive(Clone, Default)]
struct Recorder(Rc<RefCell<Vec<(String, String)>>>);

impl RichClipboard for Recorder {
    fn write_html(&mut self, html: String, text: String) -> Result<(), String> {
        self.0.borrow_mut().push((html, text));
        Ok(())
    }
}

/// Conversions run by [`counted`]; only the test below uses it.
static CONVERSIONS: AtomicUsize = AtomicUsize::new(0);

fn counted(markdown: &str, image_label: &str) -> String {
    CONVERSIONS.fetch_add(1, Ordering::SeqCst);
    muxel_core::rich_copy::markdown_to_html(markdown, image_label)
}

#[test]
fn copy_converts_in_the_background_and_writes_afterwards() {
    let draft = "# Draft\n\nHello **there**.\n";
    let (root, project) =
        project_with(&[("notes.md", draft.as_bytes()), ("closing.md", b"# Gone\n")]);
    let _storage = muxel_store::test_support::set_root(root.clone());
    let mut cx = test_cx();
    let window = cx.add_window(MuxelApp::new);
    let writes = Recorder::default();
    let (notes, held) = window
        .update(&mut cx, |app, window, cx| {
            app.rich_clipboard = Some(Box::new(writes.clone()));
            app.rich_copy_convert = counted;
            let pid = app
                .workspace
                .add_project(Project::new("rich-copy", project.clone()));
            let notes = open(app, pid, &project.join("notes.md"), window, cx);
            app.copy_rich_text(notes, cx);
            // GPUI's test executor runs no task until the test steps it, so a
            // conversion counted here ran inside the click handler.
            assert_eq!(
                CONVERSIONS.load(Ordering::SeqCst),
                0,
                "converted in the handler"
            );
            assert!(!app.editors[&notes].read(cx).rich_text_copied());

            // A second click while it converts is ignored, even after an edit.
            app.editors[&notes].update(cx, |editor, cx| {
                editor.set_content("# Changed\n".into(), window, cx)
            });
            app.copy_rich_text(notes, cx);

            // A pane that closes while it converts drops its copy, even while
            // something else holds its editor, as a remote file's load does.
            let closing = open(app, pid, &project.join("closing.md"), window, cx);
            app.copy_rich_text(closing, cx);
            let held = app.editors[&closing].clone();
            app.close_instance_inner(closing, "rich copy test", cx);
            (notes, held)
        })
        .unwrap();
    assert!(writes.0.borrow().is_empty(), "written before converting");

    // Background tasks alone: both panes convert (the ignored click doesn't),
    // and nothing is written until the UI thread runs. A conversion on the UI
    // thread would wait here too.
    while cx.dispatcher.tick(true) {}
    assert_eq!(
        CONVERSIONS.load(Ordering::SeqCst),
        2,
        "converted on the UI thread"
    );
    assert!(writes.0.borrow().is_empty(), "written off the UI thread");
    // Then every task that can run, without advancing the clock, so the
    // check's 1.5 s timer is still pending.
    while cx.executor().tick() {}
    let written = writes.0.borrow().clone();
    assert_eq!(written.len(), 1, "{written:?}");
    let (html, text) = &written[0];
    assert!(html.contains("<h1>Draft</h1>") && html.contains("<strong>there</strong>"));
    assert_eq!(text, draft, "the first click's document");

    // The check shows, and the pane is no longer converting: a click now
    // copies again, with the edit.
    window
        .update(&mut cx, |app, _, cx| {
            assert!(app.editors[&notes].read(cx).rich_text_copied());
            app.copy_rich_text(notes, cx);
        })
        .unwrap();
    while cx.executor().tick() {}
    assert_eq!(CONVERSIONS.load(Ordering::SeqCst), 3);
    let written = writes.0.borrow().clone();
    assert_eq!(written.len(), 2, "{written:?}");
    assert_eq!(written[1].1, "# Changed\n");
    drop(held);
    drop(cx);
    let _ = std::fs::remove_dir_all(&root);
}

/// Fails every write, as when another process keeps the clipboard open.
struct Busy;

impl RichClipboard for Busy {
    fn write_html(&mut self, _html: String, _text: String) -> Result<(), String> {
        Err("clipboard busy".into())
    }
}

#[test]
fn a_failed_copy_does_not_show_the_last_ones_check() {
    let (root, project) = project_with(&[("notes.md", b"# Draft\n")]);
    let _storage = muxel_store::test_support::set_root(root.clone());
    let mut cx = test_cx();
    let window = cx.add_window(MuxelApp::new);
    let notes = window
        .update(&mut cx, |app, window, cx| {
            app.rich_clipboard = Some(Box::new(Recorder::default()));
            let pid = app
                .workspace
                .add_project(Project::new("rich-copy", project.clone()));
            let notes = open(app, pid, &project.join("notes.md"), window, cx);
            app.copy_rich_text(notes, cx);
            notes
        })
        .unwrap();
    // The clock stays put, so the check's 1.5 s never runs out.
    while cx.executor().tick() {}
    window
        .update(&mut cx, |app, _, cx| {
            assert!(app.editors[&notes].read(cx).rich_text_copied());
            app.rich_clipboard = Some(Box::new(Busy));
            app.copy_rich_text(notes, cx);
            assert!(
                !app.editors[&notes].read(cx).rich_text_copied(),
                "the last copy's check shows during this one"
            );
        })
        .unwrap();
    while cx.executor().tick() {}
    window
        .update(&mut cx, |app, _, cx| {
            assert!(!app.editors[&notes].read(cx).rich_text_copied());
            assert!(
                app.notifications
                    .iter()
                    .any(|n| n.subtitle == "clipboard busy"),
                "no error shown"
            );
        })
        .unwrap();
    drop(cx);
    let _ = std::fs::remove_dir_all(&root);
}
