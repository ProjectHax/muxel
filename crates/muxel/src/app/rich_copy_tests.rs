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
            app.editors[&markdown].update(cx, |editor, cx| editor.toggle_rendered(cx));
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
    let notes = window
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
            let editor = app.editors[&notes].read(cx);
            assert!(editor.rich_copy_converting() && !editor.rich_text_copied());

            // A second click while it converts is ignored, even after an edit.
            app.editors[&notes].update(cx, |editor, cx| {
                editor.set_content("# Changed\n".into(), window, cx)
            });
            app.copy_rich_text(notes, cx);

            // A pane that closes while it converts drops its copy.
            let closing = open(app, pid, &project.join("closing.md"), window, cx);
            app.copy_rich_text(closing, cx);
            app.close_instance_inner(closing, "rich copy test", cx);
            notes
        })
        .unwrap();
    assert!(writes.0.borrow().is_empty(), "written before converting");

    // Run every task that can run, without advancing the clock, so the check's
    // 1.5 s timer is still pending.
    while cx.executor().tick() {}
    // Both panes converted; the ignored click didn't.
    assert_eq!(CONVERSIONS.load(Ordering::SeqCst), 2);
    let written = writes.0.borrow().clone();
    assert_eq!(written.len(), 1, "{written:?}");
    let (html, text) = &written[0];
    assert!(html.contains("<h1>Draft</h1>") && html.contains("<strong>there</strong>"));
    assert_eq!(text, draft, "the first click's document");
    window
        .update(&mut cx, |app, _, cx| {
            let editor = app.editors[&notes].read(cx);
            assert!(!editor.rich_copy_converting() && editor.rich_text_copied());
        })
        .unwrap();
    drop(cx);
    let _ = std::fs::remove_dir_all(&root);
}
