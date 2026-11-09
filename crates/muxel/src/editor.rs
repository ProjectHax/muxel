//! A code-editor pane backed by gpui-component's `InputState` code editor
//! (syntax highlighting, line numbers, indent guides, folding, Ctrl+F find,
//! tab indent, undo — all provided by the widget). This module adds file
//! load/save plumbing, language detection, and a dirty flag on top.

use crate::i18n::{t, tf};
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState, Position, Rope, RopeExt as _, TabSize};
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
    /// Last clean buffer, for detecting edits accepted without InputEvent::Change.
    clean_text: Rope,
    config: EditorConfig,
    /// Set for read-only diff panes: the directory whose `git diff` is shown.
    diff_dir: Option<PathBuf>,
    /// The file is an image — offer a rendered (`img`) view.
    is_image: bool,
    /// The file is markdown (`.md`/`.markdown`) — offer a rendered view.
    is_markdown: bool,
    /// Show the rendered view (image / markdown) vs the raw text editor.
    show_rendered: bool,
    /// "Copy as rich text" just succeeded: the header shows a check briefly.
    rich_copied: bool,
    /// Counts copies, so an earlier copy's timer can't clear a later one's check.
    rich_copy_count: u64,
    /// A rich-text copy is converting in the background.
    rich_copy_converting: bool,
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

pub(crate) fn disk_stamp(path: &Path) -> Option<DiskStamp> {
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
        // IME cancellation can emit Change after restoring the clean text.
        // Mark actual edits dirty; refresh the pane title's dirty dot once.
        cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change)
                    && !this.dirty
                    && input.read(cx).text() != &this.clean_text
                {
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
        let clean_text = input.read(cx).text().clone();
        Self {
            input,
            path,
            dirty,
            clean_text,
            config,
            diff_dir: None,
            is_image,
            is_markdown,
            show_rendered: is_markdown || is_image,
            rich_copied: false,
            rich_copy_count: 0,
            rich_copy_converting: false,
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
            // Read-only diffs never reload from a file or need a clean snapshot.
            clean_text: Rope::new(),
            config,
            diff_dir: Some(dir),
            is_image: false,
            is_markdown: false,
            show_rendered: false,
            rich_copied: false,
            rich_copy_count: 0,
            rich_copy_converting: false,
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

    /// Whether "Copy as rich text" is offered: rendered Markdown only.
    pub fn can_copy_rich_text(&self) -> bool {
        self.is_markdown && self.show_rendered
    }

    /// Whether a rich-text copy just succeeded (the header shows a check).
    pub fn rich_text_copied(&self) -> bool {
        self.rich_copied
    }

    /// Start a rich-text copy: the document to convert, or `None` while an
    /// earlier copy is still converting, so a repeated click is ignored. An
    /// earlier copy's check goes, so it can't sit beside this one's error.
    pub fn begin_rich_copy(&mut self, cx: &mut Context<Self>) -> Option<String> {
        if self.rich_copy_converting {
            return None;
        }
        self.rich_copy_converting = true;
        if self.rich_copied {
            self.rich_copied = false;
            cx.notify();
        }
        Some(self.text(cx))
    }

    /// The conversion `begin_rich_copy` started has finished.
    pub fn end_rich_copy(&mut self) {
        self.rich_copy_converting = false;
    }

    /// Show the copied check for a moment.
    pub fn flash_rich_text_copied(&mut self, cx: &mut Context<Self>) {
        self.rich_copy_count += 1;
        let count = self.rich_copy_count;
        self.rich_copied = true;
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.rich_copy_count == count {
                    this.rich_copied = false;
                    cx.notify();
                }
            });
        })
        .detach();
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

    fn is_composing(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.input.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        })
    }

    /// Unmarking IME text can commit it without emitting InputEvent::Change.
    /// Compare only when deciding whether a disk reload may discard the buffer.
    fn check_unreported_edits(&mut self, cx: &mut Context<Self>) {
        if !self.dirty && self.is_dirty(cx) {
            self.dirty = true;
            cx.notify();
        }
    }

    /// Reload an unchanged local buffer from disk. Dirty source always wins,
    /// and in-progress IME text stays in place until composition ends.
    pub fn reload_if_clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty || self.is_composing(window, cx) {
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        self.check_unreported_edits(cx);
        if self.dirty {
            return;
        }
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
                if self.is_composing(window, cx) {
                    // Leave the old stamp so the next poll retries after the
                    // composition commits or cancels, using current disk text.
                    return;
                }
                self.check_unreported_edits(cx);
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

    /// Swap in the file's contents from disk, keeping the cursor and scroll
    /// position, and without moving focus. The cursor goes back with
    /// `set_selected_range`, which only moves it. gpui-component's
    /// `set_cursor_position` also focuses the input, and then a file an agent
    /// rewrites while it's open in a clean editor would take the keys from
    /// whatever had them, the agent's own terminal included.
    fn replace_disk_content(
        &mut self,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if &*self.input.read(cx).value() == content.as_str() {
            self.clean_text = self.input.read(cx).text().clone();
            return;
        }
        self.input.update(cx, |s, cx| {
            let offset = s.scroll_offset();
            let cursor = s.cursor_position();
            s.set_value(content, window, cx);
            let at = s.text().position_to_offset(&cursor);
            s.set_selected_range(at..at, cx);
            // Restoring the cursor scrolls it into view once layout exists.
            s.set_scroll_offset(offset, cx);
        });
        self.clean_text = self.input.read(cx).text().clone();
        self.dirty = false;
        cx.notify();
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn is_dirty(&self, cx: &App) -> bool {
        self.dirty || (self.diff_dir.is_none() && self.input.read(cx).text() != &self.clean_text)
    }
    #[cfg(all(test, windows))]
    pub(crate) fn input_for_test(&self) -> Entity<InputState> {
        self.input.clone()
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
        self.clean_text = self.input.read(cx).text().clone();
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
        self.clean_text = self.input.read(cx).text().clone();
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
        self.clean_text = self.input.read(cx).text().clone();
        self.path = Some(path);
        self.dirty = false;
        self.external_change = None;
        self.disk_stamp = stamp;
        cx.notify();
    }

    /// Explicit Reload discards all local text, including an active composition.
    fn reload_external_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ExternalChange::Modified(content)) = self.external_change.take() else {
            return;
        };
        // End composition even if the chosen disk text already equals the input.
        self.input
            .update(cx, |input, cx| input.unmark_text(window, cx));
        self.replace_disk_content(content, window, cx);
        self.dirty = false;
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
                        this.reload_external_change(window, cx);
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
    use super::{
        DiskChange, EditorConfig, EditorView, Position, disk_stamp, language_for_path,
        read_disk_change,
    };
    use gpui::{
        AppContext, Context, Entity, FocusHandle, IntoElement, Render, TestAppContext, Window,
        WindowHandle,
    };
    use std::path::{Path, PathBuf};

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

    /// A test window's root. The editor under test isn't drawn.
    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::div()
        }
    }

    /// A clean editor on `notes.txt`, in a test window with nothing focused.
    /// Fields drop in order, and `cx` checks for leaked entities when it
    /// drops, so it comes last.
    struct Reload {
        editor: Entity<EditorView>,
        window: WindowHandle<Blank>,
        root: PathBuf,
        path: PathBuf,
        cx: TestAppContext,
    }

    impl Reload {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "muxel-editor-reload-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("notes.txt");
            std::fs::write(&path, "one\n").unwrap();
            let mut cx = TestAppContext::single();
            cx.skip_drawing();
            cx.update(gpui_component::init);
            let window = cx.add_window(|_, _| Blank);
            let open = Some(path.clone());
            let editor = window
                .update(&mut cx, |_, window, cx| {
                    cx.new(|cx| EditorView::open(open, EditorConfig::default(), window, cx))
                })
                .unwrap();
            Self {
                editor,
                window,
                root,
                path,
                cx,
            }
        }

        fn focused(&mut self) -> Option<FocusHandle> {
            self.window
                .update(&mut self.cx, |_, window, cx| window.focused(cx))
                .unwrap()
        }

        fn focus(&mut self, handle: &FocusHandle) {
            self.window
                .update(&mut self.cx, |_, window, cx| window.focus(handle, cx))
                .unwrap();
        }

        fn cursor(&self) -> Position {
            self.editor
                .read_with(&self.cx, |editor, cx| editor.cursor(cx))
        }

        fn goto(&mut self, line: u32, character: u32) {
            let editor = self.editor.clone();
            self.window
                .update(&mut self.cx, |_, window, cx| {
                    editor.update(cx, |editor, cx| {
                        editor.goto_position(line, character, window, cx)
                    })
                })
                .unwrap();
        }

        /// The disk poll finding the file rewritten as `text`, by an agent say.
        /// The poll compares modified time and length, and a quick rewrite can
        /// keep the time, so `text` is a different length from what's there.
        fn poll(&mut self, text: &str) {
            let editor = self.editor.clone();
            let stamp = editor.read_with(&self.cx, |editor, _| editor.disk_stamp());
            std::fs::write(&self.path, text).unwrap();
            let change = read_disk_change(&self.path, stamp).expect("the poll saw no change");
            self.window
                .update(&mut self.cx, |_, window, cx| {
                    editor.update(cx, |editor, cx| {
                        editor.apply_disk_change(change, window, cx)
                    })
                })
                .unwrap();
            assert_eq!(
                editor.read_with(&self.cx, |editor, cx| editor.text(cx)),
                text
            );
        }

        /// Reopening the file after it was rewritten as `text`.
        fn reopen(&mut self, text: &str) {
            let editor = self.editor.clone();
            std::fs::write(&self.path, text).unwrap();
            self.window
                .update(&mut self.cx, |_, window, cx| {
                    editor.update(cx, |editor, cx| editor.reload_if_clean(window, cx))
                })
                .unwrap();
            assert_eq!(
                editor.read_with(&self.cx, |editor, cx| editor.text(cx)),
                text
            );
        }
    }

    impl Drop for Reload {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_reload_from_disk_leaves_focus_with_the_pane_that_has_it() {
        let mut reload = Reload::new("elsewhere");
        // Another pane has focus: the terminal of the agent rewriting the file, say.
        let elsewhere = reload.cx.update(|cx| cx.focus_handle());
        reload.focus(&elsewhere);
        reload.poll("two lines\n");
        assert_eq!(
            reload.focused(),
            Some(elsewhere.clone()),
            "the poll's reload took focus"
        );
        reload.reopen("three lines now\n");
        assert_eq!(
            reload.focused(),
            Some(elsewhere),
            "reopening the file took focus"
        );
    }

    #[test]
    fn a_reload_from_disk_with_nothing_focused_focuses_nothing() {
        let mut reload = Reload::new("unfocused");
        assert_eq!(reload.focused(), None);
        reload.poll("two lines\n");
        assert_eq!(reload.focused(), None, "the poll's reload took focus");
    }

    #[test]
    fn a_reload_from_disk_keeps_the_cursor_on_its_line() {
        let mut reload = Reload::new("cursor");
        reload.poll("one\nsecond line\n");
        reload.goto(1, 5);
        // The line above the cursor grows.
        reload.poll("one more\nsecond line\n");
        assert_eq!(reload.cursor(), Position::new(1, 5));
        // The cursor's own line shrinks past it, leaving it at the line's end.
        reload.poll("one more\nsec\n");
        assert_eq!(reload.cursor(), Position::new(1, 3));
    }

    // On macOS, gpui-component's Root installs native accessibility hooks;
    // GPUI test windows have no native window handle for that constructor.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_drawn_editor_keeps_its_scroll_and_cursor_after_disk_reload() {
        let mut cx = TestAppContext::single();
        cx.update(gpui_component::init);
        let original = (0..200).map(|i| format!("line {i}\n")).collect::<String>();
        let mut editor = None;
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|cx| {
                EditorView::build(
                    None,
                    original.clone(),
                    "text".into(),
                    Some(Position::new(2, 3)),
                    false,
                    EditorConfig::default(),
                    window,
                    cx,
                )
            });
            editor = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let editor = editor.unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &cx);
        visual.run_until_parked();
        // Scroll away from the cursor, as when reading further down the file.
        visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.input.update(cx, |input, cx| {
                    input.set_scroll_offset(gpui::point(gpui::px(0.), gpui::px(-500.)), cx)
                })
            })
        });
        visual.run_until_parked();
        let before = visual.update(|_, cx| {
            let editor = editor.read(cx);
            (editor.input.read(cx).scroll_offset(), editor.cursor(cx))
        });
        assert!(
            before.0.y < gpui::px(0.),
            "the drawn editor must be scrolled"
        );
        assert_eq!(before.1, Position::new(2, 3));

        visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.replace_disk_content(format!("{original}extra\n"), window, cx)
            })
        });
        visual.run_until_parked();
        let after = visual.update(|_, cx| {
            let editor = editor.read(cx);
            (editor.input.read(cx).scroll_offset(), editor.cursor(cx))
        });
        assert_eq!(after, before, "disk reload moved the viewport or cursor");
    }
}

#[cfg(test)]
mod ime_tests {
    use super::{EditorConfig, EditorView, ExternalChange, read_disk_change};
    use gpui::{Context, EntityInputHandler, TestAppContext, Window, WindowHandle};
    use std::ops::Range;
    use std::path::PathBuf;

    struct ImeReload {
        window: WindowHandle<EditorView>,
        path: PathBuf,
        cx: TestAppContext,
    }

    impl ImeReload {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "muxel-editor-ime-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("notes.txt");
            std::fs::write(&path, "one\n").unwrap();
            let mut cx = TestAppContext::single();
            cx.skip_drawing();
            cx.update(gpui_component::init);
            let window = cx.add_window(|window, cx| {
                EditorView::open(Some(path.clone()), EditorConfig::default(), window, cx)
            });
            Self { window, path, cx }
        }

        fn update<R>(
            &mut self,
            run: impl FnOnce(&mut EditorView, &mut Window, &mut Context<EditorView>) -> R,
        ) -> R {
            self.window.update(&mut self.cx, run).unwrap()
        }

        fn compose(&mut self, text: &str) {
            self.update(|editor, window, cx| {
                editor.input.update(cx, |input, cx| {
                    input.replace_and_mark_text_in_range(None, text, None, window, cx)
                })
            });
        }

        fn cancel_by_replacing_marked_text(&mut self, explicit_range: bool) {
            self.update(|editor, window, cx| {
                editor.input.update(cx, |input, cx| {
                    let range = if explicit_range {
                        input.marked_text_range(window, cx)
                    } else {
                        None
                    };
                    input.replace_text_in_range(range, "", window, cx);
                })
            });
        }

        fn text(&mut self) -> String {
            self.update(|editor, _, cx| editor.text(cx))
        }

        fn marked_range(&mut self) -> Option<Range<usize>> {
            self.update(|editor, window, cx| {
                editor
                    .input
                    .update(cx, |input, cx| input.marked_text_range(window, cx))
            })
        }

        fn poll(&mut self) {
            let stamp = self.update(|editor, _, _| editor.disk_stamp());
            if let Some(change) = read_disk_change(&self.path, stamp) {
                self.update(|editor, window, cx| editor.apply_disk_change(change, window, cx));
            }
        }
    }

    impl Drop for ImeReload {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.path.parent().unwrap());
        }
    }

    #[test]
    fn automatic_reload_preserves_ime_text_until_composition_is_cancelled() {
        for reopen in [false, true] {
            let mut reload = ImeReload::new(if reopen { "reopen" } else { "poll" });
            reload.compose("あ");
            assert_eq!(reload.text(), "あone\n");
            assert_eq!(reload.marked_range(), Some(0..1));
            // Preedit still emits no Change event; the dirty query also checks text.
            assert!(!reload.update(|editor, _, _| editor.dirty));
            std::fs::write(&reload.path, "remote replacement\n").unwrap();
            if reopen {
                reload.update(|editor, window, cx| editor.reload_if_clean(window, cx));
            } else {
                reload.poll();
                reload.poll();
            }
            assert_eq!(
                reload.text(),
                "あone\n",
                "automatic reload discarded preedit"
            );
            assert_eq!(reload.marked_range(), Some(0..1));

            // Cancellation restores the clean buffer; the next ordinary poll
            // must still see the disk change, without another write to the file.
            reload.compose("");
            assert_eq!(reload.marked_range(), None);
            reload.poll();
            assert_eq!(reload.text(), "remote replacement\n");
        }
    }

    #[test]
    fn committed_ime_text_keeps_local_edits_and_offers_the_disk_version() {
        for unmark in [false, true] {
            let mut reload = ImeReload::new(if unmark { "unmark" } else { "commit" });
            reload.compose("あ");
            std::fs::write(&reload.path, "remote replacement\n").unwrap();
            reload.poll();
            assert_eq!(reload.text(), "あone\n", "disk poll discarded preedit");
            reload.update(|editor, window, cx| {
                editor.input.update(cx, |input, cx| {
                    if unmark {
                        input.unmark_text(window, cx);
                    } else {
                        input.replace_text_in_range(None, "あ", window, cx);
                    }
                })
            });
            reload.poll();
            assert_eq!(reload.marked_range(), None);
            assert_eq!(reload.text(), "あone\n");
            assert!(reload.update(|editor, _, cx| editor.is_dirty(cx)));
            assert!(reload.update(|editor, _, _| matches!(
                editor.external_change(),
                Some(ExternalChange::Modified(content)) if content == "remote replacement\n"
            )));

            // Saving accepts the composed text as the new clean version.
            let saved_text = reload.text();
            std::fs::write(&reload.path, saved_text).unwrap();
            reload.update(|editor, _, cx| editor.mark_saved(cx));
            std::fs::write(&reload.path, "later disk version\n").unwrap();
            reload.poll();
            assert_eq!(reload.text(), "later disk version\n");
        }
    }

    #[test]
    fn explicit_reload_discards_ime_composition_with_other_local_edits() {
        for same_text in [false, true] {
            let mut reload = ImeReload::new(if same_text {
                "explicit-equal"
            } else {
                "explicit"
            });
            reload.update(|editor, window, cx| {
                editor.input.update(cx, |input, cx| {
                    input.replace_text_in_range(None, "local ", window, cx)
                })
            });
            let disk_text = if same_text {
                "local あone\n"
            } else {
                "remote replacement\n"
            };
            std::fs::write(&reload.path, disk_text).unwrap();
            reload.poll();
            reload.compose("あ");
            assert!(reload.marked_range().is_some());
            reload.update(|editor, window, cx| editor.reload_external_change(window, cx));
            assert_eq!(reload.text(), disk_text);
            assert_eq!(reload.marked_range(), None);
            assert!(!reload.update(|editor, _, cx| editor.is_dirty(cx)));
            assert!(reload.update(|editor, _, _| editor.external_change().is_none()));

            std::fs::write(&reload.path, "next disk version\n").unwrap();
            reload.poll();
            assert_eq!(reload.text(), "next disk version\n");
        }
    }

    fn assert_cancel_retries_pending_reload(explicit_range: bool) {
        let mut reload = ImeReload::new(if explicit_range {
            "cancel-explicit-range"
        } else {
            "cancel-implicit-range"
        });
        reload.compose("あ");
        std::fs::write(&reload.path, "remote replacement\n").unwrap();
        reload.poll();
        assert_eq!(reload.text(), "あone\n");
        reload.cancel_by_replacing_marked_text(explicit_range);
        assert_eq!(reload.text(), "one\n");
        assert_eq!(reload.marked_range(), None);
        reload.poll();
        assert_eq!(
            reload.text(),
            "remote replacement\n",
            "cancelling unchanged text blocked the pending disk reload"
        );
        assert!(!reload.update(|editor, _, cx| editor.is_dirty(cx)));
        assert!(reload.update(|editor, _, _| editor.external_change().is_none()));
    }

    #[test]
    fn cancellation_with_implicit_range_retries_pending_reload() {
        // Windows' empty-composition callback replaces the implicit marked range.
        assert_cancel_retries_pending_reload(false);
    }

    #[test]
    fn cancellation_with_explicit_range_retries_pending_reload() {
        // Wayland's delete-preedit callback supplies the marked range explicitly.
        assert_cancel_retries_pending_reload(true);
    }

    #[test]
    fn cancellation_callbacks_preserve_existing_dirty_state() {
        for explicit_range in [false, true] {
            let mut reload = ImeReload::new(if explicit_range {
                "dirty-cancel-explicit"
            } else {
                "dirty-cancel-implicit"
            });
            // An already-dirty editor stays dirty even if earlier edits were
            // returned to their original text, matching ordinary edit behavior.
            reload.update(|editor, window, cx| {
                editor.input.update(cx, |input, cx| {
                    input.replace_text_in_range(None, "local ", window, cx)
                })
            });
            reload.update(|editor, window, cx| {
                editor.input.update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..6), "", window, cx)
                })
            });
            assert_eq!(reload.text(), "one\n");
            assert!(reload.update(|editor, _, cx| editor.is_dirty(cx)));
            reload.compose("あ");
            std::fs::write(&reload.path, "remote replacement\n").unwrap();
            reload.poll();
            reload.cancel_by_replacing_marked_text(explicit_range);
            reload.poll();
            assert_eq!(reload.text(), "one\n");
            assert!(reload.update(|editor, _, cx| editor.is_dirty(cx)));
            assert!(reload.update(|editor, _, _| matches!(
                editor.external_change(),
                Some(ExternalChange::Modified(content)) if content == "remote replacement\n"
            )));
        }
    }
}
