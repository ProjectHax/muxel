//! Team libraries in the app: startup cleanup, periodic updates, applying
//! finished git jobs, the toolbar drop-down sections and Settings → Libraries.
//!
//! Git runs on its own threads (`crate::libraries::run_job`), never on the UI
//! thread or the gpui executor; results come back over a channel.

use super::*;
use crate::libraries::{
    LibraryListMarker, add_error_text, branch_text, copy_error_text, copy_tooltip_text,
    delete_error_text, delete_library_files, library_list_marker, library_list_marker_tooltip,
    library_row_status, preset_not_found_text, read_library_file, resync_prompt_text, run_check,
    run_job, startup_cleanup, switched_off_text,
};
use muxel_core::Settings;
use muxel_core::library::config::display_name;
use muxel_core::library::hub::LocalLists;
use muxel_core::library::hub::{
    CheckDecision, JobKind, JobOutcome, JobSpec, LibraryHub, ResyncRequest,
};
use muxel_core::library::resolve::{CopyError, SnippetStep, resolve_preset, snippet_send_action};
use muxel_core::library::resync::LocalChanges;
use muxel_core::library::{FileError, GitFailure, LibItemKey, LibKind, PresetResolution};

/// Settings read at startup, plus whether `config.toml` existed and parsed:
/// the only case in which `LIB_DIR` may be cleaned up. A missing or invalid
/// file falls back to the defaults.
pub(super) fn startup_settings(loaded: anyhow::Result<Option<Settings>>) -> (Settings, bool) {
    match loaded {
        Ok(Some(settings)) => (settings, true),
        Ok(None) => (Settings::default(), false),
        Err(e) => {
            log::warn!("ignoring invalid config: {e:#}");
            (Settings::default(), false)
        }
    }
}

/// `base` with the hub's library state written back. The library fields live in
/// `MuxelApp::library_hub`, not `self.settings`, so every save goes through here.
pub(super) fn settings_for_save(mut base: Settings, hub: &LibraryHub) -> Settings {
    hub.write_into(&mut base);
    base
}

/// What an automatic save (finished library job, scheduled shared loop) writes:
/// the on-disk `config.toml` with only the library fields replaced by the hub's,
/// so edits another muxel process or the user made to the rest survive. A
/// missing, invalid or empty file falls back to [`settings_for_save`] of `base`.
pub(super) fn settings_for_background_save(
    on_disk: anyhow::Result<Option<Settings>>,
    base: Settings,
    hub: &LibraryHub,
) -> Settings {
    match on_disk {
        Ok(Some(disk)) => settings_for_save(disk, hub),
        Ok(None) | Err(_) => settings_for_save(base, hub),
    }
}

/// Read `config.toml` at `path` and write it back with only the library fields
/// replaced ([`settings_for_background_save`]).
pub(super) fn save_library_state_to(
    path: &std::path::Path,
    base: Settings,
    hub: &LibraryHub,
) -> anyhow::Result<()> {
    let settings = settings_for_background_save(load_for_background_save(path), base, hub);
    muxel_store::save_settings_to(path, &settings)
}

/// `config.toml` as the background save reads it: `Ok(None)` if missing, `Err` if
/// unreadable, invalid or blank. A blank file is more likely caught mid-write than
/// truly empty, and merging into its defaults would wipe the user's settings.
fn load_for_background_save(path: &std::path::Path) -> anyhow::Result<Option<Settings>> {
    use anyhow::Context as _;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if text.trim().is_empty() {
        anyhow::bail!("{} is empty", path.display());
    }
    muxel_store::parse_settings(&text)
        .with_context(|| format!("parsing {}", path.display()))
        .map(Some)
}

// --- Library sections of the toolbar drop-downs ---

/// The agent of a library runner or loop row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RowAgent {
    /// Runnable with the resolved preset; `None` = no `preset` (a runner then uses
    /// the toolbar's agent, a loop its pinned one).
    Ready(Option<Uuid>),
    /// The `preset` does not resolve: the row is disabled with this reason.
    Unavailable(String),
}

/// One row of a drop-down's library part, in display order.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum LibMenuRow {
    /// A section header: the library's display name.
    Header(String),
    /// Shown like a private snippet: name and `↵`, never the text.
    Snippet {
        key: LibItemKey,
        name: String,
        submit: bool,
    },
    Runner {
        key: LibItemKey,
        name: String,
        agent: RowAgent,
    },
    Loop {
        key: LibItemKey,
        name: String,
        schedule: LoopSchedule,
        /// The file's `preset` resolved.
        agent: RowAgent,
    },
}

fn row_agent(preset: Option<&str>, presets: &[AgentPreset]) -> RowAgent {
    match resolve_preset(preset, presets) {
        PresetResolution::Unnamed => RowAgent::Ready(None),
        PresetResolution::Preset(id) => RowAgent::Ready(Some(id)),
        PresetResolution::NotFound(p) => RowAgent::Unavailable(preset_not_found_text(&p)),
    }
}

/// The library rows of the `kind` drop-down: per library (add order) a header and
/// its items in file order. Empty when no library has items of that kind.
pub(super) fn library_menu_rows(
    hub: &LibraryHub,
    kind: LibKind,
    presets: &[AgentPreset],
) -> Vec<LibMenuRow> {
    let mut rows = Vec::new();
    for section in hub.library_menu_sections(kind) {
        let library = section.config.id;
        let key = |name: &str| LibItemKey {
            library,
            kind,
            name: name.to_string(),
        };
        rows.push(LibMenuRow::Header(display_name(section.config)));
        match kind {
            LibKind::Snippet => {
                rows.extend(section.items.snippets.iter().map(|s| LibMenuRow::Snippet {
                    key: key(&s.name),
                    name: s.name.clone(),
                    submit: s.submit,
                }))
            }
            LibKind::Runner => {
                rows.extend(section.items.runners.iter().map(|r| LibMenuRow::Runner {
                    key: key(&r.name),
                    name: r.name.clone(),
                    agent: row_agent(r.content.preset.as_deref(), presets),
                }))
            }
            LibKind::Loop => rows.extend(section.items.loops.iter().map(|l| LibMenuRow::Loop {
                key: key(&l.name),
                name: l.name.clone(),
                schedule: l.content.schedule,
                agent: row_agent(l.content.preset.as_deref(), presets),
            })),
        }
    }
    rows
}

/// Number of item rows (headers excluded).
pub(super) fn library_item_count(rows: &[LibMenuRow]) -> usize {
    rows.iter()
        .filter(|r| !matches!(r, LibMenuRow::Header(_)))
        .count()
}

pub(super) fn show_empty_message(private: usize, library: usize) -> bool {
    private == 0 && library == 0
}

/// Whether the Snippets drop-down shows the "focus a terminal pane" hint.
pub(super) fn show_focus_hint(private: usize, library: usize, has_target: bool) -> bool {
    !show_empty_message(private, library) && !has_target
}

/// Sending loaded library snippet `key` is exactly a private snippet's action;
/// `None` when it is no longer loaded.
pub(super) fn library_snippet_steps(
    hub: &LibraryHub,
    key: &LibItemKey,
) -> Option<Vec<SnippetStep>> {
    hub.loaded_snippet(key)
        .map(|s| snippet_send_action(&s.text, s.submit))
}

/// The "Make a local copy" action of a library row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CopyAction {
    pub tooltip: String,
    pub enabled: bool,
}

/// Enabled with the copy tooltip, or disabled with the reason as tooltip while the
/// preset does not resolve. `agent` is `None` for a snippet.
pub(super) fn copy_action(agent: Option<&RowAgent>) -> CopyAction {
    match agent {
        Some(RowAgent::Unavailable(reason)) => CopyAction {
            tooltip: reason.clone(),
            enabled: false,
        },
        _ => CopyAction {
            tooltip: copy_tooltip_text(),
            enabled: true,
        },
    }
}

/// The editor opened for a new local copy at this index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CopyEditor {
    Runner(usize),
    Loop(usize),
}

/// What the app does after `LibraryHub::make_local_copy`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CopyOutcome {
    /// Copied: persist, then open this editor (none for a snippet).
    Copied(Option<CopyEditor>),
    /// Failed: an error event with this title and body; no list changed.
    Event(String, String),
    /// The item is no longer loaded: nothing happens.
    Ignore,
}

pub(super) fn copy_outcome(kind: LibKind, result: Result<usize, CopyError>) -> CopyOutcome {
    match result {
        Ok(idx) => CopyOutcome::Copied(match kind {
            LibKind::Snippet => None,
            LibKind::Runner => Some(CopyEditor::Runner(idx)),
            LibKind::Loop => Some(CopyEditor::Loop(idx)),
        }),
        Err(e) => match copy_error_text(&e) {
            Some((title, body)) => CopyOutcome::Event(title, body),
            None => CopyOutcome::Ignore,
        },
    }
}

/// Run `spec`, turning a panic into a failed git operation, so the job always
/// reports back and the library never stays busy.
fn run_job_guarded(
    env: &crate::integrations::GitEnv,
    lib_dir: &Path,
    spec: &muxel_core::library::hub::JobSpec,
) -> JobOutcome {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(env, lib_dir, spec))) {
        Ok(outcome) => outcome,
        Err(_) => JobOutcome {
            id: spec.id,
            kind: spec.kind,
            git: Err(GitFailure::Io {
                detail: t("the library job stopped unexpectedly").to_string(),
            }),
            read: read_library_file(&lib_dir.join(spec.id.to_string())).or(Err(FileError::Missing)),
        },
    }
}

/// As [`run_job_guarded`], for the local-changes check: a panic yields `Unknown`,
/// which always asks.
fn run_check_guarded(env: &crate::integrations::GitEnv, lib_dir: &Path, id: Uuid) -> LocalChanges {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_check(env, lib_dir, id)))
        .unwrap_or(LocalChanges::Unknown)
}

impl MuxelApp {
    /// Startup: hold every update, clean stale entries of `LIB_DIR` on a thread (only
    /// when `config_ok`), then release the hold and update every library.
    pub(super) fn start_library_startup(&mut self, config_ok: bool, cx: &mut Context<Self>) {
        self.library_hub.hold_startup();
        let lib_dir = muxel_store::libraries_dir();
        let ids: Vec<Uuid> = self.library_hub.configs.iter().map(|c| c.id).collect();
        let env = self.git_env.clone();
        let (tx, rx) = async_channel::bounded::<()>(1);
        std::thread::spawn(move || {
            if config_ok && let Some(dir) = lib_dir {
                startup_cleanup(&dir, &ids, &env);
            }
            let _ = tx.send_blocking(());
        });
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            // A panicked cleanup drops the sender: release the hold anyway.
            let _ = rx.recv().await;
            let _ = view.update(cx, |this, cx| {
                let now = unix_now();
                for id in this.library_hub.release_startup() {
                    this.dispatch_library_job(id, JobKind::Update, now, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The 1 s timer: start every update that is due.
    pub(super) fn tick_libraries(&mut self, cx: &mut Context<Self>) {
        let now = unix_now();
        for id in self.library_hub.due_updates(now) {
            self.dispatch_library_job(id, JobKind::Update, now, cx);
        }
    }

    /// Start job `kind` on library `id` on its own thread; busy, held or unknown →
    /// ignored. Returns whether a job started.
    pub(super) fn dispatch_library_job(
        &mut self,
        id: Uuid,
        kind: JobKind,
        now: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(lib_dir) = muxel_store::libraries_dir() else {
            return false;
        };
        let Some(spec) = self.library_hub.begin(id, kind, now) else {
            return false;
        };
        self.spawn_library_job(spec, lib_dir, cx);
        true
    }

    /// Run an already begun job on its own thread and apply the result when it arrives.
    fn spawn_library_job(&mut self, spec: JobSpec, lib_dir: PathBuf, cx: &mut Context<Self>) {
        let env = self.git_env.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(run_job_guarded(&env, &lib_dir, &spec));
        });
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            if let Ok(outcome) = rx.recv().await {
                let _ = view.update(cx, |this, cx| this.apply_library_outcome(outcome, cx));
            }
        })
        .detach();
        cx.notify();
    }

    /// Apply a finished job: update state, post one event per shared loop switched
    /// off, persist.
    pub(super) fn apply_library_outcome(&mut self, outcome: JobOutcome, cx: &mut Context<Self>) {
        let library = self
            .library_hub
            .config(outcome.id)
            .map(display_name)
            .unwrap_or_default();
        let effects = self.library_hub.finish(outcome, unix_now());
        for name in &effects.switched_off {
            self.add_event(NotifKind::Error, switched_off_text(name), library.clone());
        }
        self.persist_library_state();
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// Settings → Libraries. Every change applies to `library_hub` and is
// persisted at once; the Settings Cancel never reverts it.
// ---------------------------------------------------------------------------

impl MuxelApp {
    pub(super) fn render_settings_libraries(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = v_flex().w(rems(10.0)).flex_none().gap_1();
        for config in &self.library_hub.configs {
            let id = config.id;
            let selected = self.settings_ui.selected_library == Some(id);
            let fg = if selected {
                cx.theme().sidebar_accent_foreground
            } else {
                cx.theme().foreground
            };
            let mut row = div()
                .id(SharedString::from(format!("library-row-{id}")))
                .flex()
                .items_center()
                .gap_2()
                .w_full()
                .px_2()
                .py_1()
                .rounded(cx.theme().radius)
                .cursor_pointer()
                .text_color(fg)
                .on_click(cx.listener(move |this, _e, window, cx| {
                    this.open_library_details(id, window, cx)
                }))
                .child(Icon::new(IconName::BookOpen).small())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .child(display_name(config)),
                );
            let marker =
                library_list_marker(self.library_hub.actions(id), self.library_hub.runtime(id));
            if let Some(marker) = marker {
                let shape = match marker {
                    LibraryListMarker::Busy => div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("…"),
                    LibraryListMarker::Error => {
                        div().size(px(7.0)).rounded_full().bg(cx.theme().danger)
                    }
                };
                let tip = SharedString::from(library_list_marker_tooltip(marker));
                row = row.child(
                    div()
                        .id(SharedString::from(format!("library-marker-{id}")))
                        .flex_none()
                        .child(shape)
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                        }),
                );
            }
            if selected {
                row = row.bg(cx.theme().sidebar_accent);
            } else {
                row = row.hover(|s| s.bg(cx.theme().accent));
            }
            list = list.child(row);
        }
        list = list.child(
            Button::new("add-library")
                .ghost()
                .icon(IconName::Plus)
                .label(t("Add library"))
                .on_click(cx.listener(|this, _e, _w, cx| this.show_library_add_form(cx))),
        );

        let details = match self.settings_ui.selected_library {
            Some(id) if self.library_hub.config(id).is_some() => {
                self.render_library_details(id, cx)
            }
            _ => self.render_library_add_form(cx),
        };

        div()
            .flex()
            .flex_row()
            .gap_4()
            .child(list)
            .child(div().flex_1().min_w_0().child(details))
            .into_any_element()
    }

    fn render_library_add_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.settings_ui;
        v_flex()
            .gap_2()
            .max_w(px(560.0))
            .child(self.settings_label(&t("Repository URL"), cx))
            .child(Self::wide_input(Input::new(&ui.lib_url)))
            .child(self.settings_label(&t("Branch"), cx))
            .child(Self::wide_input(Input::new(&ui.lib_branch)))
            .child(self.settings_label(&t("Display name"), cx))
            .child(Self::wide_input(Input::new(&ui.lib_name)))
            .children(
                ui.lib_add_error
                    .clone()
                    .map(|msg| div().text_sm().text_color(cx.theme().danger).child(msg)),
            )
            .child(
                div().flex().gap_2().pt_2().child(
                    Button::new("save-library")
                        .primary()
                        .label(t("Add library"))
                        .on_click(cx.listener(|this, _e, window, cx| this.add_library(window, cx))),
                ),
            )
            .into_any_element()
    }

    /// Only the display name is editable; actions are disabled while busy or held.
    fn render_library_details(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let Some(config) = self.library_hub.config(id) else {
            return div().into_any_element();
        };
        let actions = self.library_hub.actions(id);
        let status = library_row_status(
            config,
            self.library_hub.runtime(id),
            actions,
            &chrono::Local,
        );
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let warning = cx.theme().warning;
        let value = |text: String| div().text_sm().min_w_0().child(text);
        v_flex()
            .gap_2()
            .max_w(px(560.0))
            .child(self.settings_label(&t("Display name"), cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.settings_ui.lib_rename)),
                    )
                    .child(
                        Button::new("rename-library")
                            .ghost()
                            .label(t("Save name"))
                            .on_click(
                                cx.listener(move |this, _e, _w, cx| this.rename_library(id, cx)),
                            ),
                    ),
            )
            .child(self.settings_label(&t("Repository URL"), cx))
            .child(value(config.url.clone()))
            .child(self.settings_label(&t("Branch"), cx))
            .child(value(branch_text(&config.branch)))
            .child(self.settings_label(&t("Last successful pull"), cx))
            .child(value(status.last_pull))
            .children(
                status
                    .busy
                    .map(|busy| div().text_sm().text_color(muted).child(busy)),
            )
            .children(
                status
                    .errors
                    .into_iter()
                    .map(|e| div().text_sm().text_color(danger).child(e)),
            )
            .children(status.counts.map(value))
            .children(
                status
                    .discarded
                    .map(|d| div().text_sm().text_color(warning).child(d)),
            )
            .children(
                status
                    .warnings
                    .map(|w| div().text_sm().text_color(muted).child(w)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("pull-library")
                            .primary()
                            .label(t("Pull now"))
                            .disabled(!actions.pull_now)
                            .on_click(
                                cx.listener(move |this, _e, _w, cx| this.pull_library_now(id, cx)),
                            ),
                    )
                    .child(
                        Button::new("resync-library")
                            .ghost()
                            .label(t("Re-sync from repository"))
                            .disabled(!actions.resync)
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.request_library_resync(id, cx)
                            })),
                    )
                    .child(
                        Button::new("remove-library")
                            .ghost()
                            .label(t("Remove"))
                            .disabled(!actions.remove)
                            .on_click(
                                cx.listener(move |this, _e, _w, cx| this.remove_library(id, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn open_library_details(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.library_hub.config(id).map(|c| c.name.clone()) else {
            return;
        };
        self.settings_ui.selected_library = Some(id);
        self.settings_ui
            .lib_rename
            .update(cx, |s, cx| s.set_value(name, window, cx));
        cx.notify();
    }

    fn show_library_add_form(&mut self, cx: &mut Context<Self>) {
        self.settings_ui.selected_library = None;
        cx.notify();
    }

    /// Saved before anything is cloned and rolled back if the save fails; the first
    /// update then clones it. During the startup hold no job starts here: the release
    /// updates every library.
    fn add_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.settings_ui.lib_url.read(cx).value().to_string();
        let branch = self.settings_ui.lib_branch.read(cx).value().to_string();
        let name = self.settings_ui.lib_name.read(cx).value().to_string();
        let base = self.settings_base();
        let added =
            crate::libraries::add_library(&mut self.library_hub, &url, &branch, &name, |hub| {
                muxel_store::save_settings(&settings_for_save(base, hub))
            });
        match added {
            Ok(id) => {
                self.clear_save_error(SaveTarget::Settings);
                self.settings_ui.lib_add_error = None;
                for input in [
                    self.settings_ui.lib_url.clone(),
                    self.settings_ui.lib_branch.clone(),
                    self.settings_ui.lib_name.clone(),
                ] {
                    input.update(cx, |s, cx| s.set_value("", window, cx));
                }
                self.dispatch_library_job(id, JobKind::Update, unix_now(), cx);
                self.open_library_details(id, window, cx);
            }
            Err(e) => self.settings_ui.lib_add_error = Some(add_error_text(&e)),
        }
        cx.notify();
    }

    /// Change the display name: persisted, no git operation.
    fn rename_library(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let name = self.settings_ui.lib_rename.read(cx).value().to_string();
        if self.library_hub.rename(id, &name) {
            self.persist_settings();
        }
        cx.notify();
    }

    /// An immediate update, counted as the last attempt. Busy or held → ignored.
    fn pull_library_now(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_library_job(id, JobKind::Update, unix_now(), cx);
    }

    /// Check the clone for local changes on its own thread (library busy meanwhile);
    /// the result goes to `apply_library_check`. Busy, held or unknown → ignored.
    fn request_library_resync(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(lib_dir) = muxel_store::libraries_dir() else {
            return;
        };
        if self.library_hub.request_resync(id) != ResyncRequest::Check {
            return;
        }
        let env = self.git_env.clone();
        let (tx, rx) = async_channel::bounded(1);
        let check_dir = lib_dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(run_check_guarded(&env, &check_dir, id));
        });
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            // A lost result is a failed check: ask.
            let changes = rx.recv().await.unwrap_or(LocalChanges::Unknown);
            let _ = view.update(cx, |this, cx| {
                this.apply_library_check(id, changes, lib_dir, cx)
            });
        })
        .detach();
        cx.notify();
    }

    /// No clone or no changes → re-sync at once; otherwise ask. Only a confirmation
    /// (`confirm_library_resync`) starts it.
    fn apply_library_check(
        &mut self,
        id: Uuid,
        changes: LocalChanges,
        lib_dir: PathBuf,
        cx: &mut Context<Self>,
    ) {
        match self.library_hub.finish_check(id, changes, unix_now()) {
            CheckDecision::Start(spec) => self.spawn_library_job(spec, lib_dir, cx),
            CheckDecision::Confirm(confirm) => {
                let name = self
                    .library_hub
                    .config(id)
                    .map(display_name)
                    .unwrap_or_default();
                self.request_confirm(
                    t("Re-sync library?"),
                    resync_prompt_text(&name, confirm),
                    t("Re-sync"),
                    ConfirmAction::ResyncLibrary(id),
                    cx,
                );
            }
            CheckDecision::Ignored => {}
        }
        cx.notify();
    }

    /// The confirmed re-sync. Busy meanwhile → ignored.
    pub(super) fn confirm_library_resync(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_library_job(id, JobKind::Resync, unix_now(), cx);
    }

    /// Remove at once (settings, menus, loop and runner state) and persist; then delete
    /// the clone on a thread without asking about local changes. Busy → nothing.
    fn remove_library(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Ok(config) = self.library_hub.remove(id) else {
            return;
        };
        if self.settings_ui.selected_library == Some(id) {
            self.settings_ui.selected_library = None;
        }
        self.persist_settings();
        cx.notify();
        let Some(lib_dir) = muxel_store::libraries_dir() else {
            return;
        };
        let name = display_name(&config);
        let env = self.git_env.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(delete_library_files(&env, &lib_dir, id));
        });
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            // A panicked delete drops the sender: report it as failed too.
            let deleted = rx.recv().await.unwrap_or(Err(()));
            if deleted.is_err() {
                let _ = view.update(cx, |this, cx| {
                    this.add_event(NotifKind::Error, delete_error_text(&name), String::new());
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl MuxelApp {
    /// Send library snippet `key` exactly as a private one: no preview or confirmation.
    fn send_library_snippet_to_active(
        &mut self,
        key: &LibItemKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(iid) = self.active_instance else {
            return;
        };
        let Some(steps) = library_snippet_steps(&self.library_hub, key) else {
            return;
        };
        self.send_snippet_steps(iid, &steps, window, cx);
    }

    /// A loop without `preset` gets the toolbar's preset, as `add_loop` does (none if
    /// the selection is not a real preset).
    fn make_local_copy(&mut self, key: &LibItemKey, window: &mut Window, cx: &mut Context<Self>) {
        let toolbar_preset = self.current_preset_id();
        let result = self.library_hub.make_local_copy(
            key,
            &self.presets,
            toolbar_preset,
            self.workspace.active_project,
            unix_now(),
            LocalLists {
                snippets: &mut self.snippets,
                runners: &mut self.runners,
                loops: &mut self.loops,
            },
        );
        match copy_outcome(key.kind, result) {
            CopyOutcome::Copied(editor) => {
                self.persist_settings();
                match editor {
                    Some(CopyEditor::Runner(idx)) => self.open_runner_settings(idx, window, cx),
                    Some(CopyEditor::Loop(idx)) => self.open_loop_settings(idx, window, cx),
                    None => {}
                }
            }
            CopyOutcome::Event(title, body) => self.add_event(NotifKind::Error, title, body),
            CopyOutcome::Ignore => {}
        }
        cx.notify();
    }

    /// The "Make a local copy" button, in place of a private row's edit pencil.
    fn copy_button(
        &self,
        id: String,
        key: LibItemKey,
        agent: Option<&RowAgent>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let action = copy_action(agent);
        div()
            .flex_none()
            .mr_1()
            .child(
                Button::new(SharedString::from(id))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Copy)
                    .tooltip(action.tooltip)
                    .disabled(!action.enabled)
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        // Only the copy, never the row's own action.
                        cx.stop_propagation();
                        this.make_local_copy(&key, window, cx);
                    })),
            )
            .into_any_element()
    }

    /// Append library `rows` after the private items. Library rows are read-only (no
    /// edit pencil); `has_target` = a terminal pane is focused (Snippets only).
    pub(super) fn push_library_menu_rows(
        &self,
        mut list: Div,
        rows: Vec<LibMenuRow>,
        has_target: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let muted = cx.theme().muted_foreground;
        for (i, row) in rows.into_iter().enumerate() {
            let row_el = match row {
                LibMenuRow::Header(title) => div()
                    .mt_1()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(muted)
                    .child(title)
                    .into_any_element(),
                LibMenuRow::Snippet { key, name, submit } => {
                    // Never shows the text; inert without a focused pane.
                    let copy =
                        self.copy_button(format!("lib-snippet-copy-{i}"), key.clone(), None, cx);
                    let fg = if has_target {
                        cx.theme().foreground
                    } else {
                        muted
                    };
                    let mut item = div()
                        .id(SharedString::from(format!("lib-snippet-item-{i}")))
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .text_color(fg)
                        .child(Icon::new(IconName::SquareTerminal).small())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_sm()
                                .child(name),
                        )
                        .children(
                            submit
                                .then(|| div().flex_none().text_xs().text_color(muted).child("↵")),
                        );
                    if has_target {
                        item = item
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().accent))
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.send_library_snippet_to_active(&key, window, cx)
                            }));
                    }
                    div()
                        .flex()
                        .items_center()
                        .w_full()
                        .child(item)
                        .child(copy)
                        .into_any_element()
                }
                LibMenuRow::Runner { key, name, agent } => {
                    let copy = self.copy_button(
                        format!("lib-runner-copy-{i}"),
                        key.clone(),
                        Some(&agent),
                        cx,
                    );
                    let item = self.library_agent_item(
                        format!("lib-runner-item-{i}"),
                        name,
                        None,
                        &agent,
                        true,
                        cx,
                    );
                    div()
                        .flex()
                        .items_center()
                        .w_full()
                        .child(item)
                        .child(copy)
                        .into_any_element()
                }
                LibMenuRow::Loop {
                    key,
                    name,
                    schedule,
                    agent,
                } => {
                    let copy = self.copy_button(
                        format!("lib-loop-copy-{i}"),
                        key.clone(),
                        Some(&agent),
                        cx,
                    );
                    let item = self.library_agent_item(
                        format!("lib-loop-item-{i}"),
                        name,
                        Some(loop_schedule_summary(&schedule)),
                        &agent,
                        false,
                        cx,
                    );
                    div()
                        .flex()
                        .items_center()
                        .w_full()
                        .child(item)
                        .child(copy)
                        .into_any_element()
                }
            };
            list = list.child(row_el);
        }
        list
    }

    /// A library runner or loop row: agent icon, name, (loops) schedule, and the
    /// reason under the name when the preset does not resolve. `lit` = normal colour.
    fn library_agent_item(
        &self,
        id: String,
        name: String,
        schedule: Option<String>,
        agent: &RowAgent,
        lit: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let muted = cx.theme().muted_foreground;
        let (icon, reason) = match agent {
            RowAgent::Ready(preset_id) => {
                let program = preset_id
                    .and_then(|id| self.presets.iter().find(|p| p.id == id))
                    .and_then(|p| p.program.clone());
                let fg = if lit { cx.theme().foreground } else { muted };
                (
                    agent_icon(program.as_deref(), px(15.0), fg).into_any_element(),
                    None,
                )
            }
            RowAgent::Unavailable(reason) => (
                Icon::new(IconName::CircleX)
                    .size(px(15.0))
                    .text_color(muted)
                    .into_any_element(),
                Some(reason.clone()),
            ),
        };
        let fg = if lit && reason.is_none() {
            cx.theme().foreground
        } else {
            muted
        };
        div()
            .id(SharedString::from(id))
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .text_color(fg)
            .child(icon)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .w_full()
                            // One line: the name truncates with an ellipsis,
                            // the schedule never shrinks.
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_sm()
                                    .child(name),
                            )
                            .children(schedule.map(|s| {
                                div()
                                    .flex_none()
                                    .ml_1()
                                    .whitespace_nowrap()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(s)
                            })),
                    )
                    .children(reason.map(|r| {
                        div()
                            .text_xs()
                            .line_height(relative(1.2))
                            .text_color(muted)
                            .child(r)
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LibraryHub, Settings, save_library_state_to, settings_for_background_save,
        settings_for_save, startup_settings,
    };
    use muxel_core::library::{
        LibraryConfig, LoopContent, RunnerContent, SharedLoopState, SharedRunnerConfirmation,
    };
    use muxel_core::{LoopSchedule, PostRunAction, Snippet};
    use muxel_store::{save_settings_to, try_load_settings_from};
    use std::path::PathBuf;
    use uuid::Uuid;

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let p = crate::test_support::short_temp_path();
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn library(n: u128, url: &str) -> LibraryConfig {
        LibraryConfig {
            id: Uuid::from_u128(n),
            url: url.to_string(),
            branch: String::new(),
            name: String::new(),
            last_pull_ok: Some(1_700_000_000),
        }
    }

    #[test]
    fn startup_settings_only_a_parsed_config_allows_the_cleanup() {
        let mut s = Settings::default();
        s.libraries.push(library(1, "file:///r"));
        let (got, ok) = startup_settings(Ok(Some(s)));
        assert!(ok);
        assert_eq!(got.libraries.len(), 1);

        let (got, ok) = startup_settings(Ok(None));
        assert!(!ok);
        assert!(got.libraries.is_empty());

        let (got, ok) = startup_settings(Err(anyhow::anyhow!("bad toml")));
        assert!(!ok);
        assert!(got.libraries.is_empty());
    }

    #[test]
    fn save_writes_back_the_hub_fields() {
        let dir = TmpDir::new();
        let path = dir.0.join("config.toml");
        let mut loaded = Settings::default();
        loaded.libraries.push(library(1, "file:///r"));
        loaded.shared_loops.push(SharedLoopState {
            library: Uuid::from_u128(1),
            name: "Nightly".to_string(),
            run_id: Uuid::from_u128(2),
            project_id: Uuid::from_u128(3),
            last_run: Some(42),
            approved: LoopContent {
                prompt: "go".to_string(),
                preset: None,
                auto_mode_presses: 0,
                schedule: LoopSchedule::EveryMinutes { minutes: 5 },
                post_run: PostRunAction::Leave,
            },
            pinned_preset_id: Some(Uuid::from_u128(0xC1)),
        });
        let hub = LibraryHub::take_from(&mut loaded);
        assert!(loaded.libraries.is_empty());
        assert!(loaded.shared_loops.is_empty());

        save_settings_to(&path, &settings_for_save(loaded.clone(), &hub)).unwrap();
        let reloaded = try_load_settings_from(&path).unwrap().unwrap();
        assert_eq!(reloaded.libraries.len(), 1);
        assert_eq!(reloaded.libraries[0].id, Uuid::from_u128(1));
        assert_eq!(reloaded.libraries[0].last_pull_ok, Some(1_700_000_000));
        assert_eq!(reloaded.shared_loops.len(), 1);
        assert_eq!(reloaded.shared_loops[0].last_run, Some(42));

        // Without the hub, a save loses them.
        save_settings_to(&path, &loaded).unwrap();
        let lost = try_load_settings_from(&path).unwrap().unwrap();
        assert!(lost.libraries.is_empty());
        assert!(lost.shared_loops.is_empty());
    }

    fn snippet(name: &str) -> Snippet {
        Snippet {
            id: Uuid::from_u128(77),
            name: name.to_string(),
            text: "echo".to_string(),
            submit: false,
        }
    }

    /// A hub with library 1, a switched-on loop and a runner confirmation.
    fn hub_state() -> LibraryHub {
        let mut s = Settings::default();
        s.libraries.push(library(1, "file:///r"));
        s.shared_loops.push(SharedLoopState {
            library: Uuid::from_u128(1),
            name: "Nightly".to_string(),
            run_id: Uuid::from_u128(2),
            project_id: Uuid::from_u128(3),
            last_run: Some(42),
            approved: LoopContent {
                prompt: "go".to_string(),
                preset: None,
                auto_mode_presses: 0,
                schedule: LoopSchedule::EveryMinutes { minutes: 5 },
                post_run: PostRunAction::Leave,
            },
            pinned_preset_id: Some(Uuid::from_u128(0xC1)),
        });
        s.shared_runner_confirmations
            .push(SharedRunnerConfirmation {
                library: Uuid::from_u128(1),
                name: "Review".to_string(),
                confirmed: RunnerContent {
                    prompt: "review".to_string(),
                    preset: None,
                    auto_mode_presses: 0,
                },
            });
        LibraryHub::take_from(&mut s)
    }

    #[test]
    fn background_save_keeps_other_processes_changes() {
        let dir = TmpDir::new();
        let path = dir.0.join("config.toml");
        let hub = hub_state();
        let base = Settings {
            snippets: Vec::new(),
            theme: "Ours".to_string(),
            ..Settings::default()
        };
        // Written meanwhile by another process.
        let disk = Settings {
            snippets: vec![snippet("From the other process")],
            theme: "Theirs".to_string(),
            libraries: vec![library(9, "file:///old")],
            ..Settings::default()
        };
        save_settings_to(&path, &disk).unwrap();

        let merged =
            settings_for_background_save(try_load_settings_from(&path), base.clone(), &hub);
        save_settings_to(&path, &merged).unwrap();
        let saved = try_load_settings_from(&path).unwrap().unwrap();
        let names: Vec<&str> = saved.snippets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["From the other process"]);
        assert_eq!(saved.theme, "Theirs");
        let ids: Vec<Uuid> = saved.libraries.iter().map(|l| l.id).collect();
        assert_eq!(ids, vec![Uuid::from_u128(1)]);
        assert_eq!(saved.shared_loops.len(), 1);
        assert_eq!(saved.shared_loops[0].last_run, Some(42));
        assert_eq!(saved.shared_runner_confirmations.len(), 1);
        assert_eq!(saved.shared_runner_confirmations[0].name, "Review");

        // A full (user) save drops the other process's snippet.
        let full = settings_for_save(base, &hub);
        assert!(full.snippets.is_empty());
        assert_eq!(full.theme, "Ours");
    }

    #[test]
    fn background_save_without_a_parsed_file_is_the_full_save() {
        let hub = hub_state();
        let base = Settings {
            snippets: vec![snippet("Ours")],
            theme: "Ours".to_string(),
            ..Settings::default()
        };
        for on_disk in [Ok(None), Err(anyhow::anyhow!("bad toml"))] {
            let got = settings_for_background_save(on_disk, base.clone(), &hub);
            assert_eq!(got.theme, "Ours");
            assert_eq!(got.snippets.len(), 1);
            assert_eq!(got.snippets[0].name, "Ours");
            assert_eq!(got.libraries.len(), 1);
            assert_eq!(got.shared_loops.len(), 1);
            assert_eq!(got.shared_runner_confirmations.len(), 1);
        }
    }

    #[test]
    fn background_save_over_an_empty_file_is_the_full_save() {
        let hub = hub_state();
        let base = Settings {
            snippets: vec![snippet("Ours")],
            theme: "Ours".to_string(),
            ..Settings::default()
        };
        for content in ["", "  \n\t\n"] {
            let dir = TmpDir::new();
            let path = dir.0.join("config.toml");
            std::fs::write(&path, content).unwrap();
            // Parsed as is, the blank file is the defaults.
            let parsed = try_load_settings_from(&path).unwrap().unwrap();
            let merged = settings_for_background_save(Ok(Some(parsed)), base.clone(), &hub);
            assert_ne!(merged.theme, "Ours", "{content:?}");
            assert!(
                merged.snippets.iter().all(|s| s.name != "Ours"),
                "{content:?}"
            );

            save_library_state_to(&path, base.clone(), &hub).unwrap();
            let saved = try_load_settings_from(&path).unwrap().unwrap();
            assert_eq!(saved.theme, "Ours", "{content:?}");
            let names: Vec<&str> = saved.snippets.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, vec!["Ours"], "{content:?}");
            assert_eq!(saved.libraries.len(), 1, "{content:?}");
            assert_eq!(saved.shared_loops.len(), 1, "{content:?}");
        }
    }

    /// `config.toml` is rewritten, deleted or made invalid while an update of `R` runs.
    #[test]
    fn background_save_keeps_a_snippet_changed_on_disk_meanwhile() {
        use crate::integrations::GitEnv;
        use crate::libraries::run_job;
        use crate::test_support::{TestRepo, file_url};
        use muxel_core::library::hub::JobKind;

        const LIB_FILE: &str = "muxel-library.toml";
        const T0: u64 = 1_700_000_000;
        const T1: u64 = T0 + 3600;
        let s_id = Uuid::from_u128(0x5);
        let private = |text: &str| Snippet {
            id: s_id,
            name: "S".to_string(),
            text: text.to_string(),
            submit: false,
        };
        let s_text = |settings: &Settings| -> Vec<String> {
            settings
                .snippets
                .iter()
                .filter(|s| s.name == "S")
                .map(|s| s.text.clone())
                .collect()
        };

        for case in ["rewritten", "deleted", "invalid"] {
            let dir = TmpDir::new();
            let path = dir.0.join("config.toml");
            let lib_dir = dir.0.join("libs");
            let env = GitEnv::for_tests();
            let repo = TestRepo::init();
            repo.commit(
                &[(LIB_FILE, "[[snippets]]\nname = \"T\"\ntext = \"t\"\n")],
                "1",
            );

            let mut hub = LibraryHub::default();
            let id = hub.add(&file_url(repo.path()), "", "").unwrap();
            let spec = hub.begin(id, JobKind::Update, T0).unwrap();
            hub.finish(run_job(&env, &lib_dir, &spec), T0);
            assert_eq!(hub.config(id).unwrap().last_pull_ok, Some(T0));
            let base = Settings {
                snippets: vec![private("a")],
                ..Settings::default()
            };
            save_settings_to(&path, &settings_for_save(base.clone(), &hub)).unwrap();
            repo.commit(
                &[(LIB_FILE, "[[snippets]]\nname = \"T\"\ntext = \"t2\"\n")],
                "2",
            );

            // The update starts; while git runs, the file changes on disk.
            let spec = hub.begin(id, JobKind::Update, T1).unwrap();
            match case {
                "rewritten" => {
                    let mut other = try_load_settings_from(&path).unwrap().unwrap();
                    other.snippets = vec![private("b")];
                    save_settings_to(&path, &other).unwrap();
                }
                "deleted" => std::fs::remove_file(&path).unwrap(),
                _ => std::fs::write(&path, "snippets = [ not toml").unwrap(),
            }
            let outcome = run_job(&env, &lib_dir, &spec);
            assert_eq!(outcome.git, Ok(()), "{case}");
            hub.finish(outcome, T1);
            save_library_state_to(&path, base.clone(), &hub).unwrap();

            let saved = try_load_settings_from(&path)
                .expect("parses")
                .expect("exists");
            let expected = if case == "rewritten" { "b" } else { "a" };
            assert_eq!(s_text(&saved), vec![expected.to_string()], "{case}");
            assert_eq!(saved.libraries.len(), 1, "{case}");
            assert_eq!(saved.libraries[0].id, id, "{case}");
            assert_eq!(saved.libraries[0].last_pull_ok, Some(T1), "{case}");

            if case == "rewritten" {
                // A user-initiated save afterwards: last writer wins.
                assert!(hub.rename(id, "Team"));
                save_settings_to(&path, &settings_for_save(base.clone(), &hub)).unwrap();
                let saved = try_load_settings_from(&path).unwrap().unwrap();
                assert_eq!(s_text(&saved), vec!["a".to_string()]);
                assert_eq!(saved.libraries[0].name, "Team");
                assert_eq!(saved.libraries[0].last_pull_ok, Some(T1));
            }
        }
    }
}

#[cfg(test)]
mod menu_tests {
    use super::{
        CopyAction, CopyEditor, CopyOutcome, LibMenuRow, RowAgent, copy_action, copy_outcome,
        library_item_count, library_menu_rows, library_snippet_steps, show_empty_message,
        show_focus_hint,
    };
    use muxel_core::AgentPreset;
    use muxel_core::library::hub::LocalLists;
    use muxel_core::library::hub::{JobKind, JobOutcome, LibraryHub};
    use muxel_core::library::resolve::CopyError;
    use muxel_core::library::resolve::SnippetStep;
    use muxel_core::library::{
        FileError, LibItemKey, LibKind, LibLoop, LibRunner, LibSnippet, LoopContent, ParsedLibrary,
        RunnerContent,
    };
    use muxel_core::{Loop, Runner, Snippet};
    use muxel_core::{LoopSchedule, PostRunAction};
    use uuid::Uuid;

    fn snippet(name: &str, text: &str, submit: bool) -> LibSnippet {
        LibSnippet {
            name: name.to_string(),
            text: text.to_string(),
            submit,
        }
    }

    fn runner(name: &str, preset: Option<&str>) -> LibRunner {
        LibRunner {
            name: name.to_string(),
            content: RunnerContent {
                prompt: "p".to_string(),
                preset: preset.map(str::to_string),
                auto_mode_presses: 0,
            },
        }
    }

    fn lib_loop(name: &str, preset: Option<&str>) -> LibLoop {
        LibLoop {
            name: name.to_string(),
            content: LoopContent {
                prompt: "p".to_string(),
                preset: preset.map(str::to_string),
                auto_mode_presses: 0,
                schedule: LoopSchedule::EveryMinutes { minutes: 5 },
                post_run: PostRunAction::Leave,
            },
        }
    }

    fn preset(name: &str, id: u128) -> AgentPreset {
        let mut p = AgentPreset::shell();
        p.name = name.to_string();
        p.id = Uuid::from_u128(id);
        p
    }

    /// Add a library and apply one finished update with `read`.
    fn add_read(
        hub: &mut LibraryHub,
        url: &str,
        name: &str,
        read: Result<ParsedLibrary, FileError>,
    ) -> Uuid {
        let id = hub.add(url, "", name).unwrap();
        hub.begin(id, JobKind::Update, 100).unwrap();
        hub.finish(
            JobOutcome {
                id,
                kind: JobKind::Update,
                git: Ok(()),
                read,
            },
            100,
        );
        id
    }

    fn snippets_only(names: &[&str]) -> ParsedLibrary {
        ParsedLibrary {
            snippets: names.iter().map(|n| snippet(n, n, false)).collect(),
            ..ParsedLibrary::default()
        }
    }

    fn names(rows: &[LibMenuRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                LibMenuRow::Header(h) => format!("# {h}"),
                LibMenuRow::Snippet { name, .. }
                | LibMenuRow::Runner { name, .. }
                | LibMenuRow::Loop { name, .. } => name.clone(),
            })
            .collect()
    }

    #[test]
    fn no_libraries_adds_no_rows() {
        let hub = LibraryHub::default();
        for kind in [LibKind::Snippet, LibKind::Runner, LibKind::Loop] {
            assert!(library_menu_rows(&hub, kind, &[]).is_empty());
        }
    }

    #[test]
    fn sections_in_add_order_with_display_name_headers() {
        let mut hub = LibraryHub::default();
        add_read(
            &mut hub,
            "file:///a",
            "team-a",
            Ok(snippets_only(&["A2", "A1"])),
        );
        add_read(
            &mut hub,
            "file:///b",
            "team-b",
            Ok(snippets_only(&["B1", "B2"])),
        );
        let rows = library_menu_rows(&hub, LibKind::Snippet, &[]);
        assert_eq!(
            names(&rows),
            ["# team-a", "A2", "A1", "# team-b", "B1", "B2"]
        );
        assert_eq!(library_item_count(&rows), 4);
    }

    #[test]
    fn header_falls_back_to_the_url_segment() {
        let mut hub = LibraryHub::default();
        add_read(
            &mut hub,
            "https://h/org/shared.git",
            "",
            Ok(snippets_only(&["S"])),
        );
        let rows = library_menu_rows(&hub, LibKind::Snippet, &[]);
        assert_eq!(rows[0], LibMenuRow::Header("shared".to_string()));
    }

    #[test]
    fn libraries_without_items_of_a_kind_get_no_header() {
        let mut hub = LibraryHub::default();
        add_read(
            &mut hub,
            "file:///a",
            "only-snippets",
            Ok(snippets_only(&["S"])),
        );
        add_read(
            &mut hub,
            "file:///no/existe",
            "broken",
            Err(FileError::Missing),
        );
        // Configured but never read (items = None).
        hub.add("file:///c", "", "unread").unwrap();
        assert_eq!(
            names(&library_menu_rows(&hub, LibKind::Snippet, &[])),
            ["# only-snippets", "S"]
        );
        assert!(library_menu_rows(&hub, LibKind::Runner, &[]).is_empty());
        assert!(library_menu_rows(&hub, LibKind::Loop, &[]).is_empty());
    }

    #[test]
    fn header_follows_a_rename() {
        let mut hub = LibraryHub::default();
        let id = add_read(&mut hub, "file:///a", "old", Ok(snippets_only(&["S"])));
        assert!(hub.rename(id, "  new name  "));
        let rows = library_menu_rows(&hub, LibKind::Snippet, &[]);
        assert_eq!(rows[0], LibMenuRow::Header("new name".to_string()));
    }

    #[test]
    fn rows_follow_the_last_read() {
        let mut hub = LibraryHub::default();
        let id = add_read(&mut hub, "file:///a", "lib", Ok(snippets_only(&["Old"])));
        hub.begin(id, JobKind::Update, 500).unwrap();
        hub.finish(
            JobOutcome {
                id,
                kind: JobKind::Update,
                git: Ok(()),
                read: Ok(snippets_only(&["New"])),
            },
            500,
        );
        assert_eq!(
            names(&library_menu_rows(&hub, LibKind::Snippet, &[])),
            ["# lib", "New"]
        );
    }

    #[test]
    fn snippet_row_has_name_and_submit() {
        let mut hub = LibraryHub::default();
        let lib = add_read(
            &mut hub,
            "file:///a",
            "lib",
            Ok(ParsedLibrary {
                snippets: vec![snippet("Multi", "one\ntwo\nthree", true)],
                ..ParsedLibrary::default()
            }),
        );
        let rows = library_menu_rows(&hub, LibKind::Snippet, &[]);
        assert_eq!(
            rows[1],
            LibMenuRow::Snippet {
                key: LibItemKey {
                    library: lib,
                    kind: LibKind::Snippet,
                    name: "Multi".to_string(),
                },
                name: "Multi".to_string(),
                submit: true,
            }
        );
    }

    #[test]
    fn unresolved_preset_disables_runner_and_loop_rows() {
        let presets = [preset("Claude", 0xC1)];
        let mut hub = LibraryHub::default();
        add_read(
            &mut hub,
            "file:///a",
            "lib",
            Ok(ParsedLibrary {
                runners: vec![
                    runner("Bad", Some("NoSuchAgent")),
                    runner("Good", Some(" claude ")),
                    runner("Plain", None),
                ],
                loops: vec![lib_loop("BadLoop", Some("NoSuchAgent"))],
                ..ParsedLibrary::default()
            }),
        );
        let reason = "Agent preset \"NoSuchAgent\" not found".to_string();
        let agents: Vec<RowAgent> = library_menu_rows(&hub, LibKind::Runner, &presets)
            .into_iter()
            .filter_map(|r| match r {
                LibMenuRow::Runner { agent, .. } => Some(agent),
                _ => None,
            })
            .collect();
        assert_eq!(
            agents,
            [
                RowAgent::Unavailable(reason.clone()),
                RowAgent::Ready(Some(Uuid::from_u128(0xC1))),
                RowAgent::Ready(None),
            ]
        );
        let loop_rows = library_menu_rows(&hub, LibKind::Loop, &presets);
        match &loop_rows[1] {
            LibMenuRow::Loop { agent, .. } => assert_eq!(*agent, RowAgent::Unavailable(reason)),
            other => panic!("expected a loop row, got {other:?}"),
        }
    }

    #[test]
    fn empty_message_only_without_private_and_library_items() {
        assert!(show_empty_message(0, 0));
        assert!(!show_empty_message(0, 1));
        assert!(!show_empty_message(3, 0));
        assert!(!show_empty_message(3, 2));
    }

    #[test]
    fn focus_hint_with_only_library_snippets() {
        assert!(show_focus_hint(0, 1, false));
        assert!(show_focus_hint(3, 0, false));
        assert!(!show_focus_hint(0, 1, true));
        assert!(!show_focus_hint(3, 2, true));
        // Nothing to send → the empty message instead of the hint.
        assert!(!show_focus_hint(0, 0, false));
    }

    #[test]
    fn library_snippet_sends_like_a_private_one() {
        let mut hub = LibraryHub::default();
        let lib = add_read(
            &mut hub,
            "file:///a",
            "lib",
            Ok(ParsedLibrary {
                snippets: vec![snippet("Go", "go on", true), snippet("Say", "hi", false)],
                ..ParsedLibrary::default()
            }),
        );
        let key = |name: &str| LibItemKey {
            library: lib,
            kind: LibKind::Snippet,
            name: name.to_string(),
        };
        assert_eq!(
            library_snippet_steps(&hub, &key("Go")),
            Some(vec![
                SnippetStep::Paste("go on".to_string()),
                SnippetStep::Write(b"\r"),
            ])
        );
        assert_eq!(
            library_snippet_steps(&hub, &key("Say")),
            Some(vec![SnippetStep::Paste("hi".to_string())])
        );
        assert_eq!(library_snippet_steps(&hub, &key("Gone")), None);
    }

    const COPY_TOOLTIP: &str =
        "Creates a private copy you can edit. It is not synced with the library.";

    #[test]
    fn enabled_copy_shows_the_copy_tooltip() {
        let expected = CopyAction {
            tooltip: COPY_TOOLTIP.to_string(),
            enabled: true,
        };
        assert_eq!(copy_action(None), expected);
        assert_eq!(copy_action(Some(&RowAgent::Ready(None))), expected);
        assert_eq!(
            copy_action(Some(&RowAgent::Ready(Some(Uuid::from_u128(1))))),
            expected
        );
    }

    #[test]
    fn unresolved_preset_disables_copy_with_the_reason() {
        let reason = "Agent preset \"NoSuchAgent\" not found";
        let action = copy_action(Some(&RowAgent::Unavailable(reason.to_string())));
        assert_eq!(
            action,
            CopyAction {
                tooltip: reason.to_string(),
                enabled: false,
            }
        );
        assert_ne!(action.tooltip, COPY_TOOLTIP);
    }

    #[test]
    fn copy_opens_the_editor_of_runners_and_loops_only() {
        assert_eq!(
            copy_outcome(LibKind::Snippet, Ok(4)),
            CopyOutcome::Copied(None)
        );
        assert_eq!(
            copy_outcome(LibKind::Runner, Ok(2)),
            CopyOutcome::Copied(Some(CopyEditor::Runner(2)))
        );
        assert_eq!(
            copy_outcome(LibKind::Loop, Ok(0)),
            CopyOutcome::Copied(Some(CopyEditor::Loop(0)))
        );
    }

    #[test]
    fn copy_errors_become_events() {
        assert_eq!(
            copy_outcome(LibKind::Loop, Err(CopyError::NoProject)),
            CopyOutcome::Event(
                "Can't add a loop".to_string(),
                "Open a project first — a loop runs in a specific project.".to_string()
            )
        );
        match copy_outcome(
            LibKind::Runner,
            Err(CopyError::PresetNotFound("NoSuchAgent".to_string())),
        ) {
            CopyOutcome::Event(_, body) => {
                assert_eq!(body, "Agent preset \"NoSuchAgent\" not found")
            }
            other => panic!("expected an event, got {other:?}"),
        }
        assert_eq!(
            copy_outcome(LibKind::Snippet, Err(CopyError::Gone)),
            CopyOutcome::Ignore
        );
    }

    /// The loop has no `preset`, so its copy gets the toolbar's `Codex`.
    #[test]
    fn copy_through_the_hub() {
        let presets = [preset("Claude", 0xC1), preset("Codex", 0xC2)];
        let toolbar = Some(Uuid::from_u128(0xC2));
        let mut hub = LibraryHub::default();
        let lib = add_read(
            &mut hub,
            "file:///a",
            "lib",
            Ok(ParsedLibrary {
                snippets: vec![snippet("Go", "go on", true)],
                runners: vec![runner("Review", Some("Claude"))],
                loops: vec![lib_loop("Nightly", None)],
                ..ParsedLibrary::default()
            }),
        );
        let key = |kind, name: &str| LibItemKey {
            library: lib,
            kind,
            name: name.to_string(),
        };
        let (mut snippets, mut runners, mut loops) = (
            Vec::<Snippet>::new(),
            Vec::<Runner>::new(),
            Vec::<Loop>::new(),
        );
        let project = Uuid::from_u128(0xF0);
        let mut copy = |k: &LibItemKey, active: Option<Uuid>| {
            let r = hub.make_local_copy(
                k,
                &presets,
                toolbar,
                active,
                1_000,
                LocalLists {
                    snippets: &mut snippets,
                    runners: &mut runners,
                    loops: &mut loops,
                },
            );
            copy_outcome(k.kind, r)
        };
        assert_eq!(
            copy(&key(LibKind::Loop, "Nightly"), None),
            CopyOutcome::Event(
                "Can't add a loop".to_string(),
                "Open a project first — a loop runs in a specific project.".to_string()
            )
        );
        assert_eq!(
            copy(&key(LibKind::Snippet, "Go"), Some(project)),
            CopyOutcome::Copied(None)
        );
        assert_eq!(
            copy(&key(LibKind::Runner, "Review"), Some(project)),
            CopyOutcome::Copied(Some(CopyEditor::Runner(0)))
        );
        assert_eq!(
            copy(&key(LibKind::Loop, "Nightly"), Some(project)),
            CopyOutcome::Copied(Some(CopyEditor::Loop(0)))
        );
        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].name, "Go");
        assert_eq!(runners.len(), 1);
        assert_eq!(runners[0].name, "Review");
        assert_eq!(runners[0].preset_id, Some(Uuid::from_u128(0xC1)));
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].name, "Nightly");
        assert!(!loops[0].enabled);
        assert_eq!(loops[0].project_id, project);
        assert_eq!(loops[0].last_run, Some(1_000));
        assert_eq!(loops[0].preset_id, toolbar);
    }
}
