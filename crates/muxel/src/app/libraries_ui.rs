//! Team libraries in the app: startup cleanup, periodic updates, applying
//! finished git jobs, the toolbar drop-down sections and Settings → Libraries.
//!
//! Git runs on its own threads (`crate::libraries::run_job`), never on the UI
//! thread or the gpui executor; results come back over a channel.

use super::*;
use crate::libraries::{
    LibraryListMarker, add_error_text, branch_text, delete_error_text, delete_library_files,
    library_list_marker, library_list_marker_tooltip, library_row_status, read_library_file,
    resync_prompt_text, run_check, run_job, startup_cleanup, switched_off_text,
};
use muxel_core::Settings;
use muxel_core::library::config::display_name;
use muxel_core::library::hub::{
    CheckDecision, JobKind, JobOutcome, JobSpec, LibraryHub, ResyncRequest,
};
use muxel_core::library::resync::LocalChanges;
use muxel_core::library::{FileError, GitFailure};

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
