//! A code-editor pane backed by gpui-component's `InputState` code editor
//! (syntax highlighting, line numbers, indent guides, folding, Ctrl+F find,
//! tab indent, undo — all provided by the widget). This module adds file
//! load/save plumbing, language detection, and a dirty flag on top.

use crate::i18n::{t, tf};
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState, Position, TabSize};
use gpui_component::text::markdown;
use gpui_component::{ActiveTheme, Sizable};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Don't try to open files larger than this (treated as non-text).
pub const MAX_EDITOR_BYTES: u64 = 8 * 1024 * 1024;

/// User-configurable editor appearance/behavior (from `Settings`).
#[derive(Clone)]
pub struct EditorConfig {
    /// Empty = the theme's monospace font.
    pub font_family: String,
    pub font_size: f32,
    pub tab_size: usize,
    pub soft_wrap: bool,
    pub line_numbers: bool,
    pub indent_guides: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            font_family: String::new(),
            font_size: 13.0,
            tab_size: 4,
            soft_wrap: false,
            line_numbers: true,
            indent_guides: true,
        }
    }
}

/// One editor pane: an open file (or an unsaved "Untitled" buffer), or — when
/// `diff_dir` is set — a read-only git-diff view of that directory.
pub struct EditorView {
    input: Entity<InputState>,
    path: Option<PathBuf>,
    dirty: bool,
    config: EditorConfig,
    /// Set for read-only diff panes: the directory whose `git diff` is shown.
    diff_dir: Option<PathBuf>,
    /// The file is an image — offer a rendered (`img`) view.
    is_image: bool,
    /// The file is markdown (`.md`/`.markdown`) — offer a rendered view.
    is_markdown: bool,
    /// Show the rendered view (image / markdown) vs the raw text editor.
    show_rendered: bool,
    /// Latest external change that could not be applied without discarding edits.
    external_change: Option<ExternalChange>,
    /// Last observed on-disk revision; polled off the UI thread.
    disk_stamp: Option<DiskStamp>,
}

#[derive(Clone)]
pub(crate) enum ExternalChange {
    Modified(String),
    Deleted,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiskStamp {
    modified: Option<SystemTime>,
    len: u64,
}

pub(crate) enum DiskChange {
    Content { stamp: DiskStamp, content: String },
    Deleted,
    Unavailable { stamp: DiskStamp },
}

fn disk_stamp(path: &Path) -> Option<DiskStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(DiskStamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

/// Poll one text file. Metadata is cheap; content is read only after its stamp changes.
pub(crate) fn read_disk_change(path: &Path, known: Option<DiskStamp>) -> Option<DiskChange> {
    let Some(stamp) = disk_stamp(path) else {
        return known.is_some().then_some(DiskChange::Deleted);
    };
    if known == Some(stamp) {
        return None;
    }
    if stamp.len > MAX_EDITOR_BYTES {
        return Some(DiskChange::Unavailable { stamp });
    }
    match std::fs::read_to_string(path) {
        Ok(content) => Some(DiskChange::Content { stamp, content }),
        Err(_) => Some(DiskChange::Unavailable { stamp }),
    }
}

/// Whether `path` is a markdown file by extension.
fn is_markdown_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// Whether `path` is a renderable image by extension (`img()` decodes these,
/// incl. SVG via rasterization).
fn is_image_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| {
            matches!(
                e.as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg"
            )
        })
}

impl EditorView {
    /// Open `path` (or a blank Untitled buffer when `None`).
    pub fn open(
        path: Option<PathBuf>,
        config: EditorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = path.as_deref().and_then(read_text_file).unwrap_or_default();
        let language = path
            .as_deref()
            .map(language_for_path)
            .unwrap_or("text")
            .to_string();
        Self::build(path, content, language, None, false, config, window, cx)
    }

    /// Re-create an editor from captured state (used when popping a pane out to
    /// its own window — gpui-component binds text-input focus to the creating
    /// window, so the view must be rebuilt rather than moved). Undo history is
    /// not preserved.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_state(
        text: String,
        path: Option<PathBuf>,
        language: String,
        cursor: Option<Position>,
        dirty: bool,
        external_change: Option<ExternalChange>,
        disk_stamp: Option<DiskStamp>,
        show_rendered: bool,
        config: EditorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editor = Self::build(path, text, language, cursor, dirty, config, window, cx);
        editor.external_change = external_change;
        editor.disk_stamp = disk_stamp;
        if editor.is_renderable() {
            editor.show_rendered = show_rendered;
        }
        editor
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        path: Option<PathBuf>,
        content: String,
        language: String,
        cursor: Option<Position>,
        dirty: bool,
        config: EditorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let cfg = config.clone();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .line_number(cfg.line_numbers)
                .indent_guides(cfg.indent_guides)
                .soft_wrap(cfg.soft_wrap)
                .tab_size(TabSize {
                    tab_size: cfg.tab_size,
                    hard_tabs: false,
                })
                .default_value(content)
        });
        // Mark dirty on edit; refresh the pane title's dirty dot once.
        cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) && !this.dirty {
                    this.dirty = true;
                    cx.notify();
                }
            },
        )
        .detach();
        if let Some(pos) = cursor {
            input.update(cx, |s, cx| s.set_cursor_position(pos, window, cx));
        }
        let stamp = path.as_deref().and_then(disk_stamp);
        let is_markdown = path.as_deref().is_some_and(is_markdown_path);
        let is_image = path.as_deref().is_some_and(is_image_path);
        Self {
            input,
            path,
            dirty,
            config,
            diff_dir: None,
            is_image,
            is_markdown,
            show_rendered: is_markdown || is_image,
            external_change: None,
            disk_stamp: stamp,
        }
    }

    /// Create a read-only git-diff view of `dir`, syntax-highlighted via the
    /// "diff" grammar (green additions / red deletions / `@@` hunks). The buffer
    /// is never saved; `refresh_diff` re-runs `git diff`.
    pub fn diff(
        dir: PathBuf,
        config: EditorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = crate::integrations::git_diff(&dir);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("diff")
                .line_number(false)
                .indent_guides(false)
                .soft_wrap(config.soft_wrap)
                .default_value(content)
        });
        Self {
            input,
            path: None,
            dirty: false,
            config,
            diff_dir: Some(dir),
            is_image: false,
            is_markdown: false,
            show_rendered: false,
            external_change: None,
            disk_stamp: None,
        }
    }

    /// Whether this is a read-only diff pane.
    pub fn is_diff(&self) -> bool {
        self.diff_dir.is_some()
    }

    /// Whether this file has a rendered view (image or markdown).
    pub fn is_renderable(&self) -> bool {
        self.is_markdown || self.is_image
    }

    pub(crate) fn is_pollable_text(&self) -> bool {
        self.path.is_some() && self.diff_dir.is_none() && !self.is_image
    }
    pub fn is_html(&self) -> bool {
        self.path
            .as_deref()
            .and_then(Path::extension)
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm"))
    }

    /// Whether the rendered view (vs raw editor) is currently shown.
    pub fn show_rendered(&self) -> bool {
        self.show_rendered
    }

    /// Toggle between the rendered view (image/markdown) and the raw text editor.
    pub fn toggle_rendered(&mut self, cx: &mut Context<Self>) {
        if self.is_renderable() {
            self.show_rendered = !self.show_rendered;
            cx.notify();
        }
    }

    /// The directory a diff pane is showing (for dedup), if any.
    pub fn diff_dir(&self) -> Option<&Path> {
        self.diff_dir.as_deref()
    }

    /// Re-run `git diff` for a diff pane and replace the shown text (synchronous;
    /// used by the manual refresh button and pop-out/restore).
    pub fn refresh_diff(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.diff_dir.clone() else {
            return;
        };
        let content = crate::integrations::git_diff(&dir);
        self.apply_diff_text(content, window, cx);
    }

    /// Replace a diff pane's text with already-computed `content` (used by the
    /// periodic background refresh so `git diff` doesn't run on the UI thread).
    pub fn set_diff_content(&self, content: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.diff_dir.is_some() {
            self.apply_diff_text(content, window, cx);
        }
    }

    /// Swap in new diff text while keeping the user's scroll position. Does
    /// nothing when the text is unchanged (so an idle diff never jumps).
    fn apply_diff_text(&self, content: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| {
            if &*s.value() == content.as_str() {
                return;
            }
            // `set_value` snaps scroll to the top; re-apply the saved offset
            // (clamped after the next layout) so reading isn't interrupted.
            let offset = s.scroll_offset();
            s.set_value(content, window, cx);
            s.set_scroll_offset(offset, cx);
        });
    }

    /// Apply updated editor settings live (font/wrap/line-numbers/indent guides;
    /// tab width applies to newly-opened editors).
    pub fn set_config(
        &mut self,
        config: EditorConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Diff panes keep their fixed layout (no line numbers / indent guides);
        // only the font (read in `render`) follows the new config.
        if self.diff_dir.is_none() {
            self.input.update(cx, |s, cx| {
                s.set_line_number(config.line_numbers, window, cx);
                s.set_soft_wrap(config.soft_wrap, window, cx);
                s.set_indent_guides(config.indent_guides, window, cx);
            });
        }
        self.config = config;
        cx.notify();
    }

    /// Move the cursor to a 0-based line (used by find-in-project navigation).
    pub fn goto_line(&self, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.goto_position(line, 0, window, cx);
    }

    /// Move the cursor to a zero-based source position.
    pub fn goto_position(
        &self,
        line: u32,
        character: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input.update(cx, |s, cx| {
            s.set_cursor_position(Position { line, character }, window, cx);
        });
    }

    /// Reload an unchanged local buffer from disk. Dirty source always wins.
    pub fn reload_if_clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty {
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        let Some(content) = read_text_file(&path) else {
            return;
        };
        self.disk_stamp = disk_stamp(&path);
        self.replace_disk_content(content, window, cx);
    }

    /// Apply a poll result, or retain the disk version beside dirty local edits.
    /// Keeping this state on the editor makes the same conflict UI work in main,
    /// secondary, and popped-out windows without app-level pane bookkeeping.
    pub(crate) fn apply_disk_change(
        &mut self,
        change: DiskChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match change {
            DiskChange::Content { stamp, content } => {
                self.disk_stamp = Some(stamp);
                if self.dirty {
                    self.external_change = Some(ExternalChange::Modified(content));
                } else {
                    self.external_change = None;
                    self.replace_disk_content(content, window, cx);
                }
            }
            DiskChange::Deleted => {
                self.disk_stamp = None;
                self.external_change = Some(ExternalChange::Deleted);
            }
            DiskChange::Unavailable { stamp } => {
                self.disk_stamp = Some(stamp);
                self.external_change = Some(ExternalChange::Unavailable);
            }
        }
        cx.notify();
    }

    fn replace_disk_content(
        &mut self,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if &*self.input.read(cx).value() == content.as_str() {
            return;
        }
        self.input.update(cx, |s, cx| {
            let offset = s.scroll_offset();
            let cursor = s.cursor_position();
            s.set_value(content, window, cx);
            s.set_scroll_offset(offset, cx);
            s.set_cursor_position(cursor, window, cx);
        });
        self.dirty = false;
        cx.notify();
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub(crate) fn external_change(&self) -> Option<ExternalChange> {
        self.external_change.clone()
    }
    pub(crate) fn disk_stamp(&self) -> Option<DiskStamp> {
        self.disk_stamp
    }
    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }
    pub fn cursor(&self, cx: &App) -> Position {
        self.input.read(cx).cursor_position()
    }
    /// Language name for the current path (for pop-out re-creation).
    pub fn language(&self) -> String {
        self.path
            .as_deref()
            .map(language_for_path)
            .unwrap_or("text")
            .to_string()
    }

    /// The pane header title: "Diff · <dir>" for diff panes, else the file name
    /// (with a "●" dirty marker) or "Untitled".
    pub fn title(&self) -> String {
        if let Some(dir) = &self.diff_dir {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "diff".to_string());
            return tf("Diff · {name}", &[("name", &name)]);
        }
        let name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| t("Untitled").to_string());
        if self.dirty {
            format!("● {name}")
        } else {
            name
        }
    }

    /// Mark the buffer as saved (clears the dirty flag).
    pub fn mark_saved(&mut self, cx: &mut Context<Self>) {
        let was_dirty = self.dirty;
        let changed = self.external_change.take().is_some();
        self.disk_stamp = self.path.as_deref().and_then(disk_stamp);
        if self.dirty {
            self.dirty = false;
        }
        if was_dirty || changed {
            cx.notify();
        }
    }

    /// Replace the buffer contents (used after an async remote read), keeping the
    /// path/language, and clear the dirty flag.
    pub fn set_content(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| s.set_value(text, window, cx));
        self.dirty = false;
        self.external_change = None;
        self.disk_stamp = self.path.as_deref().and_then(disk_stamp);
        cx.notify();
    }

    /// Point the editor at a new on-disk path (after Save As), re-detecting the
    /// syntax language, and clear the dirty flag.
    pub fn set_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let lang = language_for_path(&path).to_string();
        let stamp = disk_stamp(&path);
        self.input.update(cx, |s, cx| s.set_highlighter(lang, cx));
        self.path = Some(path);
        self.dirty = false;
        self.external_change = None;
        self.disk_stamp = stamp;
        cx.notify();
    }

    fn render_external_change(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let no_disk_version = matches!(
            self.external_change,
            Some(ExternalChange::Deleted | ExternalChange::Unavailable)
        );
        let label = match self.external_change {
            Some(ExternalChange::Deleted) => t("Moved, deleted, or inaccessible on disk"),
            Some(ExternalChange::Unavailable) => t("Couldn't read file from disk"),
            _ => t("File changed on disk"),
        };
        let mut actions = div().flex().items_center().gap_1();
        if !no_disk_version {
            actions = actions.child(
                Button::new("editor-external-reload")
                    .xsmall()
                    .label(t("Reload"))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        let Some(ExternalChange::Modified(content)) = this.external_change.take()
                        else {
                            return;
                        };
                        this.replace_disk_content(content, window, cx);
                        this.dirty = false;
                        cx.notify();
                    })),
            );
        }
        actions = actions.child(
            Button::new("editor-external-keep")
                .ghost()
                .xsmall()
                .label(if no_disk_version {
                    t("Keep open")
                } else {
                    t("Keep mine")
                })
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.external_change = None;
                    cx.notify();
                })),
        );

        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .px_2()
                .py_1()
                .bg(cx.theme().warning.opacity(0.14))
                .text_color(cx.theme().warning)
                .text_xs()
                .child(label)
                .child(actions)
                .into_any_element(),
        )
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let notice = self
            .external_change
            .as_ref()
            .and_then(|_| self.render_external_change(cx));
        // Rendered image view (incl. SVG, which `img` rasterizes).
        let content = if self.is_image
            && self.show_rendered
            && let Some(path) = self.path.clone()
        {
            div()
                .id("img-view")
                .size_full()
                .overflow_scroll()
                .flex()
                .justify_center()
                .bg(cx.theme().background)
                .p_4()
                .child(img(path).max_w_full())
                .into_any_element()
        // Rendered markdown view: a fixed-size scrollable container holding the
        // formatted markdown (the InputState text stays the source of truth, so
        // edits made in raw mode show here when toggled back).
        } else if self.is_markdown && self.show_rendered {
            let src = self.input.read(cx).value().to_string();
            div()
                .id("md-view")
                .size_full()
                .bg(cx.theme().background)
                .child(markdown(src).selectable(true).scrollable(true).p_4())
                .into_any_element()
        } else {
            let family: SharedString = if self.config.font_family.trim().is_empty() {
                cx.theme().mono_font_family.clone()
            } else {
                self.config.font_family.clone().into()
            };
            Input::new(&self.input)
                .h_full()
                .bordered(false)
                .focus_bordered(false)
                .font_family(family)
                .text_size(px(self.config.font_size))
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(notice)
            .child(div().flex_1().min_h_0().child(content))
            .into_any_element()
    }
}

/// Read a file as UTF-8 text, skipping oversized/unreadable files.
fn read_text_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_EDITOR_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Map a file's extension to a gpui-component highlighter language name.
/// Unknown extensions return `"text"` (the highlighter falls back gracefully).
pub fn language_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" | "pyw" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" => "markdown",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "sh" | "bash" | "zsh" => "bash",
        "rb" => "ruby",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "scala" | "sc" => "scala",
        "sql" => "sql",
        "swift" => "swift",
        "ex" | "exs" => "elixir",
        "php" => "php",
        "svelte" => "svelte",
        "zig" => "zig",
        "graphql" | "gql" => "graphql",
        "proto" => "proto",
        "diff" | "patch" => "diff",
        "astro" => "astro",
        "cmake" => "cmake",
        "mk" | "mak" => "make",
        _ => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::{DiskChange, disk_stamp, language_for_path, read_disk_change};
    use std::path::Path;

    #[test]
    fn detects_languages_by_extension() {
        assert_eq!(language_for_path(Path::new("a/b/main.rs")), "rust");
        assert_eq!(language_for_path(Path::new("x.PY")), "python"); // case-insensitive
        assert_eq!(language_for_path(Path::new("comp.tsx")), "tsx");
        assert_eq!(language_for_path(Path::new("s.unknownext")), "text");
        assert_eq!(language_for_path(Path::new("Makefile")), "text"); // no extension
        assert_eq!(language_for_path(Path::new("notes.md")), "markdown");
    }

    #[test]
    fn disk_poll_reads_only_changed_text_and_reports_deletion() {
        let root = std::env::temp_dir().join(format!(
            "muxel-editor-poll-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("notes.md");
        std::fs::write(&path, "one").unwrap();
        let initial = disk_stamp(&path).unwrap();
        assert!(read_disk_change(&path, Some(initial)).is_none());

        std::fs::write(&path, "two lines").unwrap();
        let changed = read_disk_change(&path, Some(initial)).unwrap();
        let stamp = match changed {
            DiskChange::Content { stamp, content } => {
                assert_eq!(content, "two lines");
                stamp
            }
            _ => panic!("changed UTF-8 text was not read"),
        };

        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            read_disk_change(&path, Some(stamp)),
            Some(DiskChange::Deleted)
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn disk_poll_reports_changed_non_utf8_content_as_unavailable() {
        let root = std::env::temp_dir().join(format!(
            "muxel-editor-poll-binary-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("notes.md");
        std::fs::write(&path, "text").unwrap();
        let initial = disk_stamp(&path).unwrap();
        std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();

        assert!(matches!(
            read_disk_change(&path, Some(initial)),
            Some(DiskChange::Unavailable { .. })
        ));
        let _ = std::fs::remove_dir_all(root);
    }
}
