//! Team libraries in the app: the library state held
//! outside `self.settings`, the startup cleanup and first updates, the
//! periodic updates and applying a finished git job.
//!
//! Git runs on its own threads (`crate::libraries::run_job`), never on the UI
//! thread or the gpui executor; results come back over a channel.

use super::*;
use crate::libraries::{read_library_file, run_job, startup_cleanup, switched_off_text};
use muxel_core::Settings;
use muxel_core::library::config::display_name;
use muxel_core::library::hub::{JobKind, JobOutcome, JobSpec, LibraryHub};
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
