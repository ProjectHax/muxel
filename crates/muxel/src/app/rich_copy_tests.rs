//! "Copy as rich text" belongs to rendered Markdown only: not the Raw view, not
//! other text files, and not images (the other kind with a rendered view).

use super::{MuxelApp, PaletteCommand};
use crate::theme;
use gpui::{App, Context, TestAppContext, Window};
use muxel_core::Project;
use std::path::Path;
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

#[test]
fn copy_as_rich_text_is_offered_for_rendered_markdown_only() {
    let root = std::env::temp_dir().join(format!("muxel-rich-copy-{}", Uuid::new_v4()));
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let root = root.canonicalize().unwrap();
    let project = project.canonicalize().unwrap();
    std::fs::write(project.join("notes.md"), "# Draft\n\nHello **there**.\n").unwrap();
    std::fs::write(project.join("notes.txt"), "plain\n").unwrap();
    std::fs::write(project.join("shot.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let _storage = muxel_store::test_support::set_root(root.clone());

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
