//! `LibraryHub`: library configuration, runtime state, update schedule and
//! per-library lock.
//!
//! Pure: the clock, presets, git results and file reads come in as
//! parameters. The app owns one hub, moved out of `Settings` with
//! [`LibraryHub::take_from`] and written back with [`LibraryHub::write_into`]
//! before every save.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use super::config::{AddError, is_duplicate, normalize_name, validate_new};
use super::resolve::{CopyError, local_loop_copy, local_runner_copy, local_snippet_copy};
use super::resync::{LocalChanges, ResyncConfirm, confirmation_for};
use super::state::{
    FireCheck, FireMode, find_loop, find_runner, find_shared_loop, loop_fire_check,
    reconcile_loops, turn_off,
};
use super::{
    FileError, GitFailure, LibItemKey, LibKind, LibLoop, LibRunner, LibSnippet, LibraryConfig,
    LoopContent, LoopOffReason, PULL_INTERVAL_SECS, ParsedLibrary, SharedLoopState,
    SharedRunnerConfirmation,
};
use crate::{AgentPreset, Loop, Runner, Settings, Snippet};

/// The git operation running (or requested) on a library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// Pull fast-forward-only, or clone if the clone is missing.
    Update,
    /// Re-sync from repository.
    Resync,
    /// The local-changes check before a re-sync, started only by
    /// [`LibraryHub::request_resync`]. Not an update attempt.
    Check,
}

/// Runtime (not persisted) state of one library.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LibRuntime {
    pub busy: Option<JobKind>,
    /// Unix seconds when the last update attempt started.
    pub last_attempt: Option<u64>,
    /// `None` = not read yet.
    pub items: Option<ParsedLibrary>,
    pub file_error: Option<FileError>,
    pub git_error: Option<GitFailure>,
}

/// What a background job must do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobSpec {
    pub id: Uuid,
    pub kind: JobKind,
    pub url: String,
    pub branch: String,
}

/// The result of a background job: the git result and a fresh read of the
/// library file, always attempted.
#[derive(Clone, Debug, PartialEq)]
pub struct JobOutcome {
    pub id: Uuid,
    pub kind: JobKind,
    pub git: Result<(), GitFailure>,
    pub read: Result<ParsedLibrary, FileError>,
}

/// Side effects of [`LibraryHub::finish`] the app must report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// Shared loops switched off because their content changed.
    pub switched_off: Vec<String>,
}

/// Which actions of a library's Settings row are enabled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LibActions {
    /// Show the "operation in progress" text.
    pub busy: bool,
    pub pull_now: bool,
    pub resync: bool,
    pub remove: bool,
}

/// Answer to a click on "Re-sync from repository".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResyncRequest {
    /// The library is now busy with the local-changes check: run it off the
    /// UI thread and pass its result to [`LibraryHub::finish_check`].
    Check,
    /// Unknown, busy or held library: ignore without effect.
    Ignored,
}

/// What a finished local-changes check leads to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckDecision {
    /// No clone or no changes: the re-sync has started (the library is busy
    /// with it); run `spec` as a job.
    Start(JobSpec),
    Confirm(ResyncConfirm),
    /// No check was running for this library: nothing to do.
    Ignored,
}

/// Why a library was not removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveError {
    /// An operation is in progress, or the startup cleanup holds it.
    Busy,
    NotFound,
}

/// The app's private lists a local copy is appended to.
pub struct LocalLists<'a> {
    pub snippets: &'a mut Vec<Snippet>,
    pub runners: &'a mut Vec<Runner>,
    pub loops: &'a mut Vec<Loop>,
}

/// A shared loop run the app must start.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedFire {
    /// Stable id used in `running_loops`, so runs do not stack.
    pub run_id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    /// The content checked against the approved one; the agent is built from it.
    pub content: LoopContent,
    /// The resolved local preset: never the toolbar's.
    pub preset_id: Uuid,
}

/// What [`LibraryHub::prepare_shared_fire`] decided.
#[derive(Clone, Debug, PartialEq)]
pub enum SharedFireOutcome {
    /// Run it: `last_run` was recorded.
    Fire(SharedFire),
    /// It would have run, but its agent does not resolve: its state was
    /// dropped as by "Turn off". Report it; spawn nothing.
    SwitchedOff(LoopOffReason),
    /// Nothing to do; nothing changed.
    Skip,
}

impl SharedFireOutcome {
    pub fn fire(self) -> Option<SharedFire> {
        match self {
            Self::Fire(fire) => Some(fire),
            Self::SwitchedOff(_) | Self::Skip => None,
        }
    }
}

impl SharedFire {
    /// The transient `Loop` used to spawn this run (`Loop::build_instance`)
    /// and to apply its `post_run`. It is never stored in `Settings.loops`.
    pub fn to_runtime_loop(&self) -> Loop {
        Loop {
            id: self.run_id,
            name: self.name.clone(),
            preset_id: Some(self.preset_id),
            project_id: self.project_id,
            auto_mode_presses: self.content.auto_mode_presses,
            prompt: self.content.prompt.clone(),
            schedule: self.content.schedule,
            post_run: self.content.post_run,
            enabled: true,
            last_run: None,
        }
    }
}

/// One library's section of a drop-down.
#[derive(Clone, Copy, Debug)]
pub struct MenuSection<'a> {
    pub config: &'a LibraryConfig,
    pub items: &'a ParsedLibrary,
}

/// Configured libraries, their persisted shared state and their runtime state.
#[derive(Clone, Debug, Default)]
pub struct LibraryHub {
    pub configs: Vec<LibraryConfig>,
    pub shared_loops: Vec<SharedLoopState>,
    pub runner_confirmations: Vec<SharedRunnerConfirmation>,
    runtime: HashMap<Uuid, LibRuntime>,
    startup_hold: bool,
}

impl LibraryHub {
    // --- Persistence ---

    /// Move the persisted library fields out of `settings` (left empty).
    pub fn take_from(settings: &mut Settings) -> Self {
        Self {
            configs: std::mem::take(&mut settings.libraries),
            shared_loops: std::mem::take(&mut settings.shared_loops),
            runner_confirmations: std::mem::take(&mut settings.shared_runner_confirmations),
            runtime: HashMap::new(),
            startup_hold: false,
        }
    }

    pub fn write_into(&self, settings: &mut Settings) {
        settings.libraries = self.configs.clone();
        settings.shared_loops = self.shared_loops.clone();
        settings.shared_runner_confirmations = self.runner_confirmations.clone();
    }

    // --- Queries ---

    pub fn config(&self, id: Uuid) -> Option<&LibraryConfig> {
        self.configs.iter().find(|c| c.id == id)
    }

    pub fn runtime(&self, id: Uuid) -> Option<&LibRuntime> {
        self.config(id)?;
        self.runtime.get(&id)
    }

    pub fn items(&self, id: Uuid) -> Option<&ParsedLibrary> {
        self.runtime(id).and_then(|r| r.items.as_ref())
    }

    pub fn file_ok(&self, id: Uuid) -> bool {
        self.runtime(id).is_none_or(|r| r.file_error.is_none())
    }

    /// Busy for the schedule and the row: an operation in progress, or the
    /// startup cleanup still running.
    fn is_busy(&self, id: Uuid) -> bool {
        self.startup_hold || self.runtime.get(&id).is_some_and(|r| r.busy.is_some())
    }

    pub fn loaded_snippet(&self, key: &LibItemKey) -> Option<&LibSnippet> {
        if key.kind != LibKind::Snippet {
            return None;
        }
        self.items(key.library)?
            .snippets
            .iter()
            .find(|s| s.name == key.name)
    }

    pub fn loaded_runner(&self, key: &LibItemKey) -> Option<&LibRunner> {
        if key.kind != LibKind::Runner {
            return None;
        }
        find_runner(self.items(key.library)?, &key.name)
    }

    pub fn loaded_loop(&self, key: &LibItemKey) -> Option<&LibLoop> {
        if key.kind != LibKind::Loop {
            return None;
        }
        find_loop(self.items(key.library)?, &key.name)
    }

    /// Libraries, in add order, with at least one loaded item of `kind`, so a
    /// header is never drawn without rows.
    pub fn library_menu_sections(&self, kind: LibKind) -> Vec<MenuSection<'_>> {
        self.configs
            .iter()
            .filter_map(|config| {
                let items = self.items(config.id)?;
                let has_items = match kind {
                    LibKind::Snippet => !items.snippets.is_empty(),
                    LibKind::Runner => !items.runners.is_empty(),
                    LibKind::Loop => !items.loops.is_empty(),
                };
                has_items.then_some(MenuSection { config, items })
            })
            .collect()
    }

    /// Enabled actions of library `id`'s row; all disabled while busy.
    pub fn actions(&self, id: Uuid) -> LibActions {
        if self.config(id).is_none() {
            return LibActions::default();
        }
        let free = !self.is_busy(id);
        LibActions {
            busy: !free,
            pull_now: free,
            resync: free,
            remove: free,
        }
    }

    // --- Configuration ---

    /// Add a library. The caller persists right after, and calls
    /// [`Self::rollback_add`] if saving fails.
    pub fn add(&mut self, url: &str, branch: &str, name: &str) -> Result<Uuid, AddError> {
        let (url, branch) = validate_new(url, branch)?;
        if is_duplicate(&self.configs, &url, &branch) {
            return Err(AddError::Duplicate);
        }
        let id = Uuid::new_v4();
        self.configs.push(LibraryConfig {
            id,
            url,
            branch,
            name: normalize_name(name),
            last_pull_ok: None,
        });
        Ok(id)
    }

    /// Undo [`Self::add`] when the settings could not be saved.
    pub fn rollback_add(&mut self, id: Uuid) {
        self.configs.retain(|c| c.id != id);
        self.runtime.remove(&id);
    }

    /// Change the display name (trimmed). Returns whether the library exists.
    pub fn rename(&mut self, id: Uuid, name: &str) -> bool {
        match self.configs.iter_mut().find(|c| c.id == id) {
            Some(c) => {
                c.name = normalize_name(name);
                true
            }
            None => false,
        }
    }

    /// Remove a library with its runtime state, switched-on loops and runner
    /// confirmations. Refused while busy; the caller deletes the clone.
    pub fn remove(&mut self, id: Uuid) -> Result<LibraryConfig, RemoveError> {
        let pos = self
            .configs
            .iter()
            .position(|c| c.id == id)
            .ok_or(RemoveError::NotFound)?;
        if self.is_busy(id) {
            return Err(RemoveError::Busy);
        }
        let config = self.configs.remove(pos);
        self.runtime.remove(&id);
        self.shared_loops.retain(|s| s.library != id);
        self.runner_confirmations.retain(|c| c.library != id);
        Ok(config)
    }

    // --- Schedule and lock ---

    /// Hold every update until [`Self::release_startup`], while the startup
    /// cleanup of the libraries folder runs.
    pub fn hold_startup(&mut self) {
        self.startup_hold = true;
    }

    /// End the startup hold; returns every configured library, to update now.
    pub fn release_startup(&mut self) -> Vec<Uuid> {
        self.startup_hold = false;
        self.configs.iter().map(|c| c.id).collect()
    }

    /// Free libraries never attempted, or last attempted at least
    /// `PULL_INTERVAL_SECS` before `now`.
    pub fn due_updates(&self, now: u64) -> Vec<Uuid> {
        if self.startup_hold {
            return Vec::new();
        }
        self.configs
            .iter()
            .filter(|c| {
                self.runtime.get(&c.id).is_none_or(|r| {
                    r.busy.is_none()
                        && r.last_attempt
                            .is_none_or(|t| now >= t.saturating_add(PULL_INTERVAL_SECS))
                })
            })
            .map(|c| c.id)
            .collect()
    }

    /// Start operation `kind` on library `id`: marks it busy and records the
    /// attempt. Unknown, busy or held → `None`. `Check` is never begun here.
    pub fn begin(&mut self, id: Uuid, kind: JobKind, now: u64) -> Option<JobSpec> {
        if kind == JobKind::Check || self.is_busy(id) {
            return None;
        }
        let config = self.config(id)?;
        let spec = JobSpec {
            id,
            kind,
            url: config.url.clone(),
            branch: config.branch.clone(),
        };
        let rt = self.runtime.entry(id).or_default();
        rt.busy = Some(kind);
        rt.last_attempt = Some(now);
        Some(spec)
    }

    /// Ask for a re-sync: a free library becomes busy with the local-changes
    /// check. Does not touch `last_attempt`.
    pub fn request_resync(&mut self, id: Uuid) -> ResyncRequest {
        if self.is_busy(id) || self.config(id).is_none() {
            return ResyncRequest::Ignored;
        }
        self.runtime.entry(id).or_default().busy = Some(JobKind::Check);
        ResyncRequest::Check
    }

    /// End the local-changes check of library `id`. No clone or no changes →
    /// the re-sync begins at once; otherwise the library is freed, unchanged,
    /// to ask for confirmation.
    pub fn finish_check(&mut self, id: Uuid, changes: LocalChanges, now: u64) -> CheckDecision {
        let checking = self.config(id).is_some()
            && self
                .runtime
                .get(&id)
                .is_some_and(|r| r.busy == Some(JobKind::Check));
        if !checking {
            return CheckDecision::Ignored;
        }
        if let Some(rt) = self.runtime.get_mut(&id) {
            rt.busy = None;
        }
        match confirmation_for(changes) {
            Some(confirm) => CheckDecision::Confirm(confirm),
            None => match self.begin(id, JobKind::Resync, now) {
                Some(spec) => CheckDecision::Start(spec),
                None => CheckDecision::Ignored,
            },
        }
    }

    /// Apply a finished job at `now`. A failed re-sync only records its git
    /// error and keeps everything else. Otherwise a successful read replaces
    /// the items and reconciles the shared loops; a failed read leaves no
    /// items and does not reconcile.
    pub fn finish(&mut self, outcome: JobOutcome, now: u64) -> Effects {
        let id = outcome.id;
        let Some(config) = self.configs.iter_mut().find(|c| c.id == id) else {
            return Effects::default();
        };
        let rt = self.runtime.entry(id).or_default();
        rt.busy = None;
        if outcome.kind == JobKind::Resync
            && let Err(e) = outcome.git
        {
            rt.git_error = Some(e);
            return Effects::default();
        }
        match outcome.git {
            Ok(()) => {
                rt.git_error = None;
                config.last_pull_ok = Some(now);
            }
            Err(e) => rt.git_error = Some(e),
        }
        match outcome.read {
            Ok(parsed) => {
                let switched_off = reconcile_loops(id, &parsed, &mut self.shared_loops);
                rt.items = Some(parsed);
                rt.file_error = None;
                Effects { switched_off }
            }
            Err(e) => {
                rt.items = Some(ParsedLibrary::default());
                rt.file_error = Some(e);
                Effects::default()
            }
        }
    }

    // --- Local copies ---

    /// Append a private copy of the loaded item `key` to its list and return
    /// its index. `toolbar_preset` is the agent of a copied loop without
    /// `preset`.
    pub fn make_local_copy(
        &self,
        key: &LibItemKey,
        presets: &[AgentPreset],
        toolbar_preset: Option<Uuid>,
        active_project: Option<Uuid>,
        now: u64,
        lists: LocalLists<'_>,
    ) -> Result<usize, CopyError> {
        match key.kind {
            LibKind::Snippet => {
                let item = self.loaded_snippet(key).ok_or(CopyError::Gone)?;
                lists.snippets.push(local_snippet_copy(item));
                Ok(lists.snippets.len() - 1)
            }
            LibKind::Runner => {
                let item = self.loaded_runner(key).ok_or(CopyError::Gone)?;
                lists.runners.push(local_runner_copy(item, presets)?);
                Ok(lists.runners.len() - 1)
            }
            LibKind::Loop => {
                let item = self.loaded_loop(key).ok_or(CopyError::Gone)?;
                let copy = local_loop_copy(item, presets, toolbar_preset, active_project, now)?;
                lists.loops.push(copy);
                Ok(lists.loops.len() - 1)
            }
        }
    }

    // --- Shared loop fire ---

    /// Decide whether shared loop `key` runs now: [`loop_fire_check`], not
    /// already running, and its project exists. When it would run but its
    /// agent does not resolve, it is switched off instead.
    pub fn prepare_shared_fire(
        &mut self,
        key: &LibItemKey,
        presets: &[AgentPreset],
        mode: FireMode,
        now: u64,
        active_runs: &HashSet<Uuid>,
        project_exists: &dyn Fn(Uuid) -> bool,
    ) -> SharedFireOutcome {
        if key.kind != LibKind::Loop {
            return SharedFireOutcome::Skip;
        }
        let check = loop_fire_check(
            find_shared_loop(&self.shared_loops, key.library, &key.name),
            self.loaded_loop(key),
            self.file_ok(key.library),
            presets,
            mode,
        );
        if let FireCheck::Skip(_) = check {
            return SharedFireOutcome::Skip;
        }
        let Some(state) = self
            .shared_loops
            .iter_mut()
            .find(|s| s.library == key.library && s.name == key.name)
        else {
            return SharedFireOutcome::Skip;
        };
        if active_runs.contains(&state.run_id) || !project_exists(state.project_id) {
            return SharedFireOutcome::Skip;
        }
        match check {
            FireCheck::Fire { content, preset_id } => {
                state.last_run = Some(now);
                SharedFireOutcome::Fire(SharedFire {
                    run_id: state.run_id,
                    project_id: state.project_id,
                    name: state.name.clone(),
                    content,
                    preset_id,
                })
            }
            FireCheck::SwitchOff(reason) => {
                turn_off(&mut self.shared_loops, key.library, &key.name);
                SharedFireOutcome::SwitchedOff(reason)
            }
            FireCheck::Skip(_) => SharedFireOutcome::Skip,
        }
    }
}

/// Top-level `LIB_DIR` entries to delete at startup: every name that is not
/// exactly the id of a configured library.
pub fn stale_lib_dir_entries(names: &[String], ids: &[Uuid]) -> Vec<String> {
    let keep: HashSet<String> = ids.iter().map(Uuid::to_string).collect();
    names
        .iter()
        .filter(|n| !keep.contains(n.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        CheckDecision, JobKind, JobOutcome, LibActions, LibraryHub, LocalLists, RemoveError,
        ResyncRequest, SharedFire, SharedFireOutcome, stale_lib_dir_entries,
    };
    use crate::library::config::AddError;
    use crate::library::parse::parse_library;
    use crate::library::resolve::CopyError;
    use crate::library::resync::{LocalChanges, ResyncConfirm};
    use crate::library::state::FireMode;
    use crate::library::{
        FileError, GitFailure, LibItemKey, LibKind, LoopContent, LoopOffReason, ParsedLibrary,
        RunnerContent, SharedLoopState, SharedRunnerConfirmation,
    };
    use crate::{AgentPreset, Loop, LoopSchedule, PostRunAction, Runner, Settings, Snippet};
    use uuid::Uuid;

    const T0: u64 = 1000;
    const CLAUDE_ID: Uuid = Uuid::from_u128(0xC1);
    const PROJECT: Uuid = Uuid::from_u128(0x50);

    const LIB_FILE: &str = r#"
[[snippets]]
name = "A"
text = "alpha"
submit = true

[[runners]]
name = "R"
prompt = "review"
preset = "Claude"
auto_mode_presses = 2

[[runners]]
name = "Lost"
prompt = "x"
preset = "Nope"

[[loops]]
name = "L"
prompt = "go"
preset = "Claude"
auto_mode_presses = 1
schedule = { kind = "every_hours", hours = 1 }
post_run = "exit"
"#;

    fn claude() -> Vec<AgentPreset> {
        let mut p = AgentPreset::shell();
        p.name = "Claude".to_string();
        p.id = CLAUDE_ID;
        vec![p]
    }

    fn parsed(text: &str) -> ParsedLibrary {
        parse_library(text).expect("library file parses")
    }

    fn key(library: Uuid, kind: LibKind, name: &str) -> LibItemKey {
        LibItemKey {
            library,
            kind,
            name: name.to_string(),
        }
    }

    fn approved_l() -> LoopContent {
        LoopContent {
            prompt: "go".to_string(),
            preset: Some("Claude".to_string()),
            auto_mode_presses: 1,
            schedule: LoopSchedule::EveryHours { hours: 1 },
            post_run: PostRunAction::Exit,
        }
    }

    fn outcome(
        id: Uuid,
        kind: JobKind,
        git: Result<(), GitFailure>,
        read: Result<ParsedLibrary, FileError>,
    ) -> JobOutcome {
        JobOutcome {
            id,
            kind,
            git,
            read,
        }
    }

    fn failed() -> GitFailure {
        GitFailure::Failed {
            detail: "boom".to_string(),
        }
    }

    fn loaded_hub() -> (LibraryHub, Uuid) {
        let mut hub = LibraryHub::default();
        let id = hub.add("file:///R", "", "").expect("add");
        hub.begin(id, JobKind::Update, T0).expect("begin");
        let fx = hub.finish(
            outcome(id, JobKind::Update, Ok(()), Ok(parsed(LIB_FILE))),
            T0,
        );
        assert!(fx.switched_off.is_empty());
        (hub, id)
    }

    fn hub_with_loop_on() -> (LibraryHub, Uuid, Uuid) {
        let (mut hub, id) = loaded_hub();
        let run_id = Uuid::from_u128(0xF00);
        hub.shared_loops.push(SharedLoopState {
            library: id,
            name: "L".to_string(),
            run_id,
            project_id: PROJECT,
            last_run: Some(T0),
            approved: approved_l(),
            pinned_preset_id: None,
        });
        (hub, id, run_id)
    }

    // --- add / rollback ---

    #[test]
    fn add_trims_validates_and_rejects_duplicates() {
        let mut hub = LibraryHub::default();
        let id = hub.add("  file:///R  ", " main ", "  Team ").expect("add");
        let cfg = hub.config(id).expect("configured");
        assert_eq!(cfg.url, "file:///R");
        assert_eq!(cfg.branch, "main");
        assert_eq!(cfg.name, "Team");
        assert_eq!(cfg.last_pull_ok, None);
        assert_eq!(hub.add("file:///R", "main", ""), Err(AddError::Duplicate));
        // The empty branch is its own value.
        assert!(hub.add("file:///R", "", "").is_ok());
        assert_eq!(hub.add("   ", "", ""), Err(AddError::EmptyUrl));
        assert_eq!(hub.add("-u", "", ""), Err(AddError::UrlStartsWithDash));
        assert_eq!(hub.configs.len(), 2);
    }

    #[test]
    fn rollback_add_removes_the_new_library() {
        let mut hub = LibraryHub::default();
        let keep = hub.add("file:///K", "", "").expect("add");
        let id = hub.add("file:///R", "", "").expect("add");
        hub.rollback_add(id);
        assert_eq!(hub.configs.len(), 1);
        assert_eq!(hub.configs[0].id, keep);
        assert!(hub.config(id).is_none());
        assert_eq!(hub.actions(id), LibActions::default());
    }

    // --- Schedule ---

    #[test]
    fn due_after_pull_interval_from_last_attempt() {
        let mut hub = LibraryHub::default();
        let id = hub.add("file:///R", "", "").expect("add");
        assert_eq!(hub.due_updates(0), vec![id]);
        hub.begin(id, JobKind::Update, 1000).expect("begin");
        hub.finish(outcome(id, JobKind::Update, Ok(()), Ok(parsed(""))), 1000);
        assert!(hub.due_updates(1299).is_empty());
        assert_eq!(hub.due_updates(1300), vec![id]);
    }

    #[test]
    fn due_updates_skips_busy_libraries() {
        let mut hub = LibraryHub::default();
        let a = hub.add("file:///A", "", "").expect("add");
        let b = hub.add("file:///B", "", "").expect("add");
        hub.begin(a, JobKind::Update, 0).expect("begin");
        assert_eq!(hub.due_updates(10_000), vec![b]);
    }

    #[test]
    fn pull_now_counts_as_last_attempt_and_does_not_stack() {
        let (mut hub, id) = loaded_hub();
        let spec = hub.begin(id, JobKind::Update, T0 + 60).expect("pull now");
        assert_eq!(spec.id, id);
        assert_eq!(spec.kind, JobKind::Update);
        assert_eq!(spec.url, "file:///R");
        assert_eq!(spec.branch, "");
        // A second Pull now while the first runs is ignored.
        assert!(hub.begin(id, JobKind::Update, T0 + 61).is_none());
        assert!(hub.begin(id, JobKind::Resync, T0 + 61).is_none());
        hub.finish(
            outcome(id, JobKind::Update, Ok(()), Ok(parsed(LIB_FILE))),
            T0 + 70,
        );
        assert!(hub.due_updates(T0 + 359).is_empty());
        assert_eq!(hub.due_updates(T0 + 360), vec![id]);
    }

    #[test]
    fn begin_unknown_library_is_none() {
        let mut hub = LibraryHub::default();
        assert!(hub.begin(Uuid::from_u128(9), JobKind::Update, 0).is_none());
    }

    // --- Startup hold ---

    #[test]
    fn startup_hold_blocks_begin_and_due_and_release_returns_all() {
        let mut hub = LibraryHub::default();
        let a = hub.add("file:///A", "", "").expect("add");
        hub.hold_startup();
        let b = hub.add("file:///B", "", "").expect("add during hold");
        assert!(hub.due_updates(10_000).is_empty());
        assert!(hub.begin(a, JobKind::Update, 0).is_none());
        assert!(hub.begin(b, JobKind::Update, 0).is_none());
        assert_eq!(hub.runtime(a).and_then(|r| r.last_attempt), None);
        assert_eq!(hub.release_startup(), vec![a, b]);
        assert!(hub.begin(a, JobKind::Update, 0).is_some());
    }

    // --- Actions ---

    #[test]
    fn actions_disabled_while_busy_enabled_after_finish() {
        let (mut hub, id) = loaded_hub();
        let free = LibActions {
            busy: false,
            pull_now: true,
            resync: true,
            remove: true,
        };
        let busy = LibActions {
            busy: true,
            pull_now: false,
            resync: false,
            remove: false,
        };
        assert_eq!(hub.actions(id), free);
        hub.begin(id, JobKind::Update, T0 + 1).expect("begin");
        assert_eq!(hub.actions(id), busy);
        assert_eq!(hub.remove(id), Err(RemoveError::Busy));
        assert!(hub.config(id).is_some());
        assert_eq!(hub.request_resync(id), ResyncRequest::Ignored);
        hub.finish(
            outcome(id, JobKind::Update, Err(failed()), Ok(parsed(""))),
            T0 + 2,
        );
        assert_eq!(hub.actions(id), free);
        assert_eq!(hub.request_resync(id), ResyncRequest::Check);
    }

    // --- Local-changes check before a re-sync ---

    const BUSY_ACTIONS: LibActions = LibActions {
        busy: true,
        pull_now: false,
        resync: false,
        remove: false,
    };
    const FREE_ACTIONS: LibActions = LibActions {
        busy: false,
        pull_now: true,
        resync: true,
        remove: true,
    };

    #[test]
    fn request_resync_starts_the_check_and_holds_the_library() {
        let (mut hub, id) = loaded_hub();
        let attempt = hub.runtime(id).unwrap().last_attempt;
        assert_eq!(hub.request_resync(id), ResyncRequest::Check);
        assert_eq!(hub.runtime(id).unwrap().busy, Some(JobKind::Check));
        assert_eq!(hub.actions(id), BUSY_ACTIONS);
        assert_eq!(hub.request_resync(id), ResyncRequest::Ignored);
        assert!(hub.begin(id, JobKind::Update, T0 + 1).is_none());
        assert!(hub.begin(id, JobKind::Resync, T0 + 1).is_none());
        assert_eq!(hub.remove(id), Err(RemoveError::Busy));
        assert!(
            !hub.due_updates(T0 + 10 * crate::library::PULL_INTERVAL_SECS)
                .contains(&id)
        );
        // The check is not an update attempt.
        assert_eq!(hub.runtime(id).unwrap().last_attempt, attempt);
    }

    #[test]
    fn request_resync_ignored_when_unknown_busy_or_held() {
        let (mut hub, id) = loaded_hub();
        assert_eq!(
            hub.request_resync(Uuid::from_u128(0xDEAD)),
            ResyncRequest::Ignored
        );
        hub.begin(id, JobKind::Update, T0 + 1).expect("begin");
        assert_eq!(hub.request_resync(id), ResyncRequest::Ignored);
        assert_eq!(hub.runtime(id).unwrap().busy, Some(JobKind::Update));

        let mut held = LibraryHub::default();
        let a = held.add("file:///A", "", "").expect("add");
        held.hold_startup();
        assert_eq!(held.request_resync(a), ResyncRequest::Ignored);
        assert_eq!(held.runtime(a).and_then(|r| r.busy), None);
    }

    #[test]
    fn check_is_never_started_by_begin() {
        let (mut hub, id) = loaded_hub();
        assert!(hub.begin(id, JobKind::Check, T0 + 1).is_none());
        assert_eq!(hub.actions(id), FREE_ACTIONS);
    }

    #[test]
    fn no_clone_or_no_changes_start_the_resync_at_once() {
        for changes in [
            LocalChanges::NoClone,
            LocalChanges::None,
            LocalChanges::Changes {
                files: 0,
                commits: 0,
            },
        ] {
            let (mut hub, id) = loaded_hub();
            assert_eq!(hub.request_resync(id), ResyncRequest::Check);
            let decision = hub.finish_check(id, changes, T0 + 50);
            let CheckDecision::Start(spec) = decision else {
                panic!("{changes:?} must start without a dialog, got {decision:?}");
            };
            assert_eq!(spec.id, id);
            assert_eq!(spec.kind, JobKind::Resync);
            assert_eq!(spec.url, "file:///R");
            assert_eq!(hub.runtime(id).unwrap().busy, Some(JobKind::Resync));
            assert_eq!(hub.runtime(id).unwrap().last_attempt, Some(T0 + 50));
            assert_eq!(hub.actions(id), BUSY_ACTIONS);
        }
    }

    #[test]
    fn changes_ask_with_the_counts_and_free_the_library() {
        let (mut hub, id) = loaded_hub();
        let before = hub.runtime(id).cloned();
        assert_eq!(hub.request_resync(id), ResyncRequest::Check);
        let decision = hub.finish_check(
            id,
            LocalChanges::Changes {
                files: 2,
                commits: 1,
            },
            T0 + 50,
        );
        assert_eq!(
            decision,
            CheckDecision::Confirm(ResyncConfirm::Counted {
                files: 2,
                commits: 1
            })
        );
        // No re-sync until confirmed; nothing else changed.
        assert_eq!(hub.runtime(id).cloned(), before);
        assert_eq!(hub.actions(id), FREE_ACTIONS);
    }

    #[test]
    fn unknown_asks_generic_and_keeps_the_error_state() {
        let (mut hub, id) = loaded_hub();
        hub.begin(id, JobKind::Update, T0 + 1).expect("begin");
        hub.finish(
            outcome(id, JobKind::Update, Err(failed()), Ok(parsed(LIB_FILE))),
            T0 + 1,
        );
        let before = hub.runtime(id).cloned();
        assert_eq!(before.as_ref().unwrap().git_error, Some(failed()));
        let last_pull_ok = hub.config(id).unwrap().last_pull_ok;

        assert_eq!(hub.request_resync(id), ResyncRequest::Check);
        assert_eq!(
            hub.finish_check(id, LocalChanges::Unknown, T0 + 50),
            CheckDecision::Confirm(ResyncConfirm::Generic)
        );
        assert_eq!(hub.runtime(id).cloned(), before);
        assert_eq!(hub.config(id).unwrap().last_pull_ok, last_pull_ok);
        assert_eq!(hub.actions(id), FREE_ACTIONS);
    }

    #[test]
    fn finish_check_without_a_running_check_is_ignored() {
        let (mut hub, id) = loaded_hub();
        assert_eq!(
            hub.finish_check(id, LocalChanges::None, T0 + 5),
            CheckDecision::Ignored
        );
        assert_eq!(hub.actions(id), FREE_ACTIONS);
        hub.begin(id, JobKind::Update, T0 + 6).expect("begin");
        assert_eq!(
            hub.finish_check(id, LocalChanges::None, T0 + 7),
            CheckDecision::Ignored
        );
        assert_eq!(hub.runtime(id).unwrap().busy, Some(JobKind::Update));
        assert_eq!(
            hub.finish_check(Uuid::from_u128(0xBEEF), LocalChanges::None, T0),
            CheckDecision::Ignored
        );
    }

    #[test]
    fn actions_busy_during_startup_hold() {
        let mut hub = LibraryHub::default();
        let a = hub.add("file:///A", "", "").expect("add");
        hub.hold_startup();
        let held = hub.actions(a);
        assert!(held.busy);
        assert!(!held.pull_now && !held.resync && !held.remove);
        assert_eq!(hub.request_resync(a), ResyncRequest::Ignored);
        assert_eq!(hub.remove(a), Err(RemoveError::Busy));
        hub.release_startup();
        let free = hub.actions(a);
        assert!(!free.busy);
        assert!(free.pull_now && free.resync && free.remove);
    }

    #[test]
    fn actions_of_unknown_library_are_all_false() {
        let hub = LibraryHub::default();
        assert_eq!(hub.actions(Uuid::from_u128(7)), LibActions::default());
        assert_eq!(
            LibActions::default(),
            LibActions {
                busy: false,
                pull_now: false,
                resync: false,
                remove: false
            }
        );
    }

    // --- Remove ---

    #[test]
    fn remove_drops_config_runtime_loops_and_confirmations_of_that_library_only() {
        let (mut hub, id, _) = hub_with_loop_on();
        let other = hub.add("file:///O", "", "").expect("add");
        let confirmed = RunnerContent {
            prompt: "review".to_string(),
            preset: Some("Claude".to_string()),
            auto_mode_presses: 2,
        };
        for lib in [id, other] {
            hub.runner_confirmations.push(SharedRunnerConfirmation {
                library: lib,
                name: "R".to_string(),
                confirmed: confirmed.clone(),
            });
        }
        hub.shared_loops.push(SharedLoopState {
            library: other,
            ..hub.shared_loops[0].clone()
        });

        let removed = hub.remove(id).expect("free library is removed");
        assert_eq!(removed.id, id);
        assert_eq!(removed.url, "file:///R");
        assert!(hub.config(id).is_none());
        assert!(hub.runtime(id).is_none());
        assert!(hub.items(id).is_none());
        assert_eq!(hub.shared_loops.len(), 1);
        assert_eq!(hub.shared_loops[0].library, other);
        assert_eq!(hub.runner_confirmations.len(), 1);
        assert_eq!(hub.runner_confirmations[0].library, other);
        assert_eq!(hub.configs.len(), 1);
        assert_eq!(hub.remove(id), Err(RemoveError::NotFound));
    }

    // --- Rename ---

    #[test]
    fn rename_trims_and_keeps_last_attempt() {
        let (mut hub, id) = loaded_hub();
        assert!(hub.rename(id, "  Team  "));
        let cfg = hub.config(id).expect("configured");
        assert_eq!(cfg.name, "Team");
        assert_eq!(cfg.url, "file:///R");
        assert_eq!(cfg.branch, "");
        assert_eq!(hub.runtime(id).and_then(|r| r.last_attempt), Some(T0));
        assert!(hub.runtime(id).is_some_and(|r| r.busy.is_none()));
        assert!(hub.rename(id, "   "));
        assert_eq!(hub.config(id).expect("configured").name, "");
        assert!(!hub.rename(Uuid::from_u128(5), "x"));
    }

    // --- finish ---

    #[test]
    fn finish_ok_loads_items_sets_last_pull_ok_and_reconciles() {
        let (mut hub, id, _) = hub_with_loop_on();
        hub.begin(id, JobKind::Update, T0 + 300).expect("begin");
        // `L` changed upstream → switched off.
        let changed = LIB_FILE.replace("prompt = \"go\"", "prompt = \"gone\"");
        let fx = hub.finish(
            outcome(id, JobKind::Update, Ok(()), Ok(parsed(&changed))),
            T0 + 310,
        );
        assert_eq!(fx.switched_off, vec!["L".to_string()]);
        assert!(hub.shared_loops.is_empty());
        let rt = hub.runtime(id).expect("runtime");
        assert_eq!(rt.busy, None);
        assert_eq!(rt.git_error, None);
        assert_eq!(rt.file_error, None);
        assert_eq!(
            rt.items
                .as_ref()
                .map(|p| p.loops[0].content.prompt.as_str()),
            Some("gone")
        );
        assert_eq!(hub.config(id).and_then(|c| c.last_pull_ok), Some(T0 + 310));
    }

    #[test]
    fn finish_with_file_error_empties_items_and_does_not_reconcile() {
        let (mut hub, id, _) = hub_with_loop_on();
        hub.begin(id, JobKind::Update, T0 + 300).expect("begin");
        let fx = hub.finish(
            outcome(id, JobKind::Update, Ok(()), Err(FileError::NotUtf8)),
            T0 + 310,
        );
        assert!(fx.switched_off.is_empty());
        assert_eq!(hub.shared_loops.len(), 1, "no reconciliation");
        let rt = hub.runtime(id).expect("runtime");
        assert_eq!(rt.items, Some(ParsedLibrary::default()));
        assert_eq!(rt.file_error, Some(FileError::NotUtf8));
        assert_eq!(hub.config(id).and_then(|c| c.last_pull_ok), Some(T0 + 310));
    }

    #[test]
    fn finish_update_git_error_keeps_last_pull_ok_and_records_git_error() {
        let (mut hub, id) = loaded_hub();
        hub.begin(id, JobKind::Update, T0 + 300).expect("begin");
        hub.finish(
            outcome(id, JobKind::Update, Err(failed()), Ok(parsed(LIB_FILE))),
            T0 + 310,
        );
        assert_eq!(hub.config(id).and_then(|c| c.last_pull_ok), Some(T0));
        assert_eq!(
            hub.runtime(id).and_then(|r| r.git_error.clone()),
            Some(failed())
        );
        // The (old) clone is still read and loaded.
        assert_eq!(hub.items(id).map(|p| p.snippets.len()), Some(1));
    }

    #[test]
    fn double_failure_resync_keeps_items_and_loops() {
        let (mut hub, id, _) = hub_with_loop_on();
        let items_before = hub.items(id).cloned();
        let loops_before = hub.shared_loops.clone();
        assert_eq!(hub.request_resync(id), ResyncRequest::Check);
        assert_eq!(
            hub.finish_check(id, LocalChanges::Unknown, T0 + 50),
            CheckDecision::Confirm(ResyncConfirm::Generic)
        );
        hub.begin(id, JobKind::Resync, T0 + 50)
            .expect("begin resync");
        let err = GitFailure::ResyncRestoreFailed {
            replace: "denied".to_string(),
            restore: "denied".to_string(),
            leftover: "x.o-1".to_string(),
        };
        let fx = hub.finish(
            outcome(
                id,
                JobKind::Resync,
                Err(err.clone()),
                Err(FileError::Missing),
            ),
            T0 + 60,
        );
        assert!(fx.switched_off.is_empty());
        let rt = hub.runtime(id).expect("runtime");
        assert_eq!(rt.items, items_before);
        assert_eq!(
            rt.items.as_ref().map(|p| p.snippets[0].name.as_str()),
            Some("A")
        );
        assert_eq!(rt.file_error, None);
        assert_eq!(rt.git_error, Some(err));
        assert_eq!(rt.busy, None);
        assert_eq!(hub.shared_loops, loops_before);
        assert_eq!(hub.config(id).and_then(|c| c.last_pull_ok), Some(T0));
    }

    #[test]
    fn contrast_failed_update_with_missing_file_empties_items() {
        let (mut hub, id, _) = hub_with_loop_on();
        let loops_before = hub.shared_loops.clone();
        hub.begin(id, JobKind::Update, T0 + 50).expect("begin");
        let fx = hub.finish(
            outcome(id, JobKind::Update, Err(failed()), Err(FileError::Missing)),
            T0 + 60,
        );
        assert!(fx.switched_off.is_empty());
        let rt = hub.runtime(id).expect("runtime");
        assert_eq!(rt.items, Some(ParsedLibrary::default()));
        assert_eq!(rt.file_error, Some(FileError::Missing));
        assert_eq!(hub.shared_loops, loops_before, "not reconciled");
    }

    #[test]
    fn finish_resync_ok_applies_like_update() {
        let (mut hub, id, _) = hub_with_loop_on();
        hub.begin(id, JobKind::Resync, T0 + 50).expect("begin");
        let fx = hub.finish(
            outcome(id, JobKind::Resync, Ok(()), Ok(parsed(""))),
            T0 + 60,
        );
        assert!(fx.switched_off.is_empty());
        assert!(hub.shared_loops.is_empty());
        assert_eq!(hub.items(id), Some(&ParsedLibrary::default()));
        assert_eq!(hub.config(id).and_then(|c| c.last_pull_ok), Some(T0 + 60));
    }

    #[test]
    fn finish_of_removed_library_is_ignored() {
        let (mut hub, id) = loaded_hub();
        assert!(hub.remove(id).is_ok());
        let fx = hub.finish(
            outcome(id, JobKind::Update, Ok(()), Ok(parsed(LIB_FILE))),
            T0,
        );
        assert!(fx.switched_off.is_empty());
        assert!(hub.runtime(id).is_none());
        assert!(hub.configs.is_empty());
    }

    // --- Persistence ---

    fn settings_with_libraries() -> Settings {
        let mut s = Settings::default();
        let mut hub = LibraryHub::default();
        let a = hub.add("file:///A", "", "Alpha").expect("add");
        let b = hub.add("file:///B", "dev", "").expect("add");
        hub.configs[0].last_pull_ok = Some(T0);
        hub.shared_loops.push(SharedLoopState {
            library: a,
            name: "L".to_string(),
            run_id: Uuid::from_u128(0xF00),
            project_id: PROJECT,
            last_run: Some(T0),
            approved: approved_l(),
            pinned_preset_id: Some(CLAUDE_ID),
        });
        hub.runner_confirmations.push(SharedRunnerConfirmation {
            library: b,
            name: "R".to_string(),
            confirmed: RunnerContent {
                prompt: "review".to_string(),
                preset: None,
                auto_mode_presses: 0,
            },
        });
        hub.write_into(&mut s);
        s
    }

    #[test]
    fn take_from_then_write_into_roundtrips_in_memory() {
        let mut s = settings_with_libraries();
        let libraries = s.libraries.clone();
        let loops = s.shared_loops.clone();
        let confirmations = s.shared_runner_confirmations.clone();
        assert_eq!(libraries.len(), 2);
        assert_eq!(loops.len(), 1);
        assert_eq!(confirmations.len(), 1);

        let hub = LibraryHub::take_from(&mut s);
        assert!(s.libraries.is_empty());
        assert!(s.shared_loops.is_empty());
        assert!(s.shared_runner_confirmations.is_empty());
        assert_eq!(hub.configs, libraries);
        assert!(hub.runtime(libraries[0].id).is_none());

        hub.write_into(&mut s);
        assert_eq!(s.libraries, libraries);
        assert_eq!(s.shared_loops, loops);
        assert_eq!(s.shared_runner_confirmations, confirmations);
    }

    #[test]
    fn take_from_default_settings_is_an_empty_hub() {
        let mut s = Settings::default();
        let hub = LibraryHub::take_from(&mut s);
        assert!(hub.configs.is_empty());
        assert!(hub.shared_loops.is_empty());
        assert!(hub.runner_confirmations.is_empty());
        assert!(!hub.actions(Uuid::from_u128(1)).busy);
    }

    #[test]
    fn write_into_from_empty_hub_clears_settings() {
        let mut s = settings_with_libraries();
        LibraryHub::default().write_into(&mut s);
        assert!(s.libraries.is_empty());
        assert!(s.shared_loops.is_empty());
        assert!(s.shared_runner_confirmations.is_empty());
    }

    // --- Local copies ---

    struct Lists {
        snippets: Vec<Snippet>,
        runners: Vec<Runner>,
        loops: Vec<Loop>,
    }

    impl Lists {
        fn new() -> Self {
            let lp = |n: u128| Loop {
                id: Uuid::from_u128(n),
                name: format!("loop{n}"),
                preset_id: None,
                project_id: PROJECT,
                auto_mode_presses: 0,
                prompt: "p".to_string(),
                schedule: LoopSchedule::EveryHours { hours: 1 },
                post_run: PostRunAction::Leave,
                enabled: true,
                last_run: None,
            };
            Self {
                snippets: vec![Snippet {
                    id: Uuid::from_u128(1),
                    name: "mine".to_string(),
                    text: "t".to_string(),
                    submit: false,
                }],
                runners: Vec::new(),
                loops: vec![lp(10), lp(11)],
            }
        }

        fn lists(&mut self) -> LocalLists<'_> {
            LocalLists {
                snippets: &mut self.snippets,
                runners: &mut self.runners,
                loops: &mut self.loops,
            }
        }

        fn lens(&self) -> (usize, usize, usize) {
            (self.snippets.len(), self.runners.len(), self.loops.len())
        }
    }

    #[test]
    fn copy_snippet_appends_and_returns_index() {
        let (hub, id) = loaded_hub();
        let mut l = Lists::new();
        let k = key(id, LibKind::Snippet, "A");
        assert_eq!(
            hub.make_local_copy(&k, &claude(), None, None, T0, l.lists()),
            Ok(1)
        );
        assert_eq!(l.snippets[1].name, "A");
        assert_eq!(l.snippets[1].text, "alpha");
        assert!(l.snippets[1].submit);
        assert_eq!(l.lens(), (2, 0, 2));
    }

    #[test]
    fn copy_runner_resolves_preset() {
        let (hub, id) = loaded_hub();
        let mut l = Lists::new();
        let k = key(id, LibKind::Runner, "R");
        assert_eq!(
            hub.make_local_copy(&k, &claude(), None, None, T0, l.lists()),
            Ok(0)
        );
        assert_eq!(l.runners[0].preset_id, Some(CLAUDE_ID));
        assert_eq!(l.runners[0].prompt, "review");
        assert_eq!(l.runners[0].auto_mode_presses, 2);
    }

    #[test]
    fn copy_loop_is_off_in_active_project_armed_at_now() {
        let (hub, id) = loaded_hub();
        let mut l = Lists::new();
        let p = Uuid::from_u128(0x77);
        let k = key(id, LibKind::Loop, "L");
        assert_eq!(
            hub.make_local_copy(&k, &claude(), None, Some(p), 4242, l.lists()),
            Ok(2)
        );
        let copy = &l.loops[2];
        assert!(!copy.enabled);
        assert_eq!(copy.project_id, p);
        assert_eq!(copy.last_run, Some(4242));
        assert_eq!(copy.name, "L");
        assert_eq!(copy.preset_id, Some(CLAUDE_ID));
    }

    #[test]
    fn copy_errors_leave_every_list_untouched() {
        let (hub, id) = loaded_hub();
        let mut l = Lists::new();
        let lp = key(id, LibKind::Loop, "L");
        assert_eq!(
            hub.make_local_copy(&lp, &claude(), None, None, T0, l.lists()),
            Err(CopyError::NoProject)
        );
        assert_eq!(l.lens(), (1, 0, 2));
        let lost = key(id, LibKind::Runner, "Lost");
        assert_eq!(
            hub.make_local_copy(&lost, &claude(), None, Some(PROJECT), T0, l.lists()),
            Err(CopyError::PresetNotFound("Nope".to_string()))
        );
        assert_eq!(l.lens(), (1, 0, 2));
        let gone = key(id, LibKind::Snippet, "Nope");
        assert_eq!(
            hub.make_local_copy(&gone, &claude(), None, Some(PROJECT), T0, l.lists()),
            Err(CopyError::Gone)
        );
        let other_lib = key(Uuid::from_u128(3), LibKind::Snippet, "A");
        assert_eq!(
            hub.make_local_copy(&other_lib, &claude(), None, Some(PROJECT), T0, l.lists()),
            Err(CopyError::Gone)
        );
        assert_eq!(l.lens(), (1, 0, 2));
    }

    // --- Shared loop fire ---

    fn scheduled(now: u64) -> FireMode {
        FireMode::Scheduled { now, now_tod: 0 }
    }

    #[test]
    fn scheduled_fire_when_due_and_records_last_run() {
        let (mut hub, id, run_id) = hub_with_loop_on();
        let k = key(id, LibKind::Loop, "L");
        let none = HashSet::new();
        let exists = |p: Uuid| p == PROJECT;
        assert!(
            hub.prepare_shared_fire(
                &k,
                &claude(),
                scheduled(T0 + 3599),
                T0 + 3599,
                &none,
                &exists
            )
            .fire()
            .is_none()
        );
        assert_eq!(hub.shared_loops[0].last_run, Some(T0));
        let fire = hub
            .prepare_shared_fire(
                &k,
                &claude(),
                scheduled(T0 + 3600),
                T0 + 3600,
                &none,
                &exists,
            )
            .fire()
            .expect("due");
        assert_eq!(
            fire,
            SharedFire {
                run_id,
                project_id: PROJECT,
                name: "L".to_string(),
                content: approved_l(),
                preset_id: CLAUDE_ID,
            }
        );
        assert_eq!(hub.shared_loops[0].last_run, Some(T0 + 3600));
    }

    #[test]
    fn fire_skipped_while_already_running() {
        let (mut hub, id, run_id) = hub_with_loop_on();
        let k = key(id, LibKind::Loop, "L");
        let running: HashSet<Uuid> = [run_id].into_iter().collect();
        let exists = |_: Uuid| true;
        assert!(
            hub.prepare_shared_fire(
                &k,
                &claude(),
                scheduled(T0 + 3601),
                T0 + 3601,
                &running,
                &exists
            )
            .fire()
            .is_none()
        );
        assert!(
            hub.prepare_shared_fire(&k, &claude(), FireMode::Manual, T0 + 1, &running, &exists)
                .fire()
                .is_none()
        );
        assert_eq!(hub.shared_loops[0].last_run, Some(T0));
    }

    #[test]
    fn manual_fire_ignores_schedule() {
        let (mut hub, id, _) = hub_with_loop_on();
        let k = key(id, LibKind::Loop, "L");
        let fire = hub
            .prepare_shared_fire(
                &k,
                &claude(),
                FireMode::Manual,
                T0 + 1,
                &HashSet::new(),
                &|_| true,
            )
            .fire();
        assert!(fire.is_some());
        assert_eq!(hub.shared_loops[0].last_run, Some(T0 + 1));
    }

    #[test]
    fn fire_skipped_when_project_is_missing() {
        let (mut hub, id, _) = hub_with_loop_on();
        let k = key(id, LibKind::Loop, "L");
        assert!(
            hub.prepare_shared_fire(
                &k,
                &claude(),
                FireMode::Manual,
                T0 + 1,
                &HashSet::new(),
                &|_| false
            )
            .fire()
            .is_none()
        );
        assert_eq!(hub.shared_loops[0].last_run, Some(T0));
    }

    #[test]
    fn fire_skipped_when_loaded_content_differs_from_approved() {
        let (mut hub, id, _) = hub_with_loop_on();
        hub.shared_loops[0].approved.prompt = "old".to_string();
        let k = key(id, LibKind::Loop, "L");
        for mode in [FireMode::Manual, scheduled(T0 + 99_999)] {
            assert!(
                hub.prepare_shared_fire(&k, &claude(), mode, T0 + 99_999, &HashSet::new(), &|_| {
                    true
                })
                .fire()
                .is_none()
            );
        }
        assert_eq!(hub.shared_loops[0].last_run, Some(T0));
    }

    #[test]
    fn fire_skipped_when_off() {
        let (mut hub, id) = loaded_hub();
        let k = key(id, LibKind::Loop, "L");
        assert_eq!(
            hub.prepare_shared_fire(
                &k,
                &claude(),
                FireMode::Manual,
                T0,
                &HashSet::new(),
                &|_| true
            ),
            SharedFireOutcome::Skip
        );
    }

    #[test]
    fn unresolved_preset_switches_off_when_due_or_clicked() {
        let renamed = {
            let mut p = claude();
            p[0].name = "Claude2".to_string();
            p
        };
        let off =
            SharedFireOutcome::SwitchedOff(LoopOffReason::PresetNotFound("Claude".to_string()));
        for (mode, now) in [
            (scheduled(T0 + 3600), T0 + 3600),
            (FireMode::Manual, T0 + 30),
        ] {
            let (mut hub, id, _) = hub_with_loop_on();
            let k = key(id, LibKind::Loop, "L");
            assert_eq!(
                hub.prepare_shared_fire(&k, &renamed, mode, now, &HashSet::new(), &|_| true),
                off
            );
            assert!(hub.shared_loops.is_empty());
            assert_eq!(
                hub.prepare_shared_fire(
                    &k,
                    &renamed,
                    scheduled(T0 + 7200),
                    T0 + 7200,
                    &HashSet::new(),
                    &|_| true
                ),
                SharedFireOutcome::Skip
            );
        }
    }

    #[test]
    fn unresolved_agent_kept_on_while_it_would_not_run_anyway() {
        let (mut hub, id, run_id) = hub_with_loop_on();
        let k = key(id, LibKind::Loop, "L");
        let running: HashSet<Uuid> = [run_id].into_iter().collect();
        let none = HashSet::new();
        let cases: [(FireMode, &HashSet<Uuid>, bool); 3] = [
            (scheduled(T0 + 30), &none, true),
            (FireMode::Manual, &running, true),
            (FireMode::Manual, &none, false),
        ];
        for (mode, active, project) in cases {
            assert_eq!(
                hub.prepare_shared_fire(&k, &[], mode, T0 + 30, active, &|_| project),
                SharedFireOutcome::Skip
            );
            assert_eq!(hub.shared_loops.len(), 1);
            assert_eq!(hub.shared_loops[0].last_run, Some(T0));
        }
    }

    #[test]
    fn preset_less_loop_uses_its_pin_or_is_switched_off() {
        let file = LIB_FILE.replace(
            "preset = \"Claude\"\nauto_mode_presses = 1",
            "auto_mode_presses = 1",
        );
        let without_preset = LoopContent {
            preset: None,
            ..approved_l()
        };
        let hub_with_pin = |pin: Option<Uuid>| {
            let (mut hub, id) = loaded_hub();
            hub.begin(id, JobKind::Update, T0).expect("begin");
            hub.finish(outcome(id, JobKind::Update, Ok(()), Ok(parsed(&file))), T0);
            hub.shared_loops.push(SharedLoopState {
                library: id,
                name: "L".to_string(),
                run_id: Uuid::from_u128(0xF00),
                project_id: PROJECT,
                last_run: Some(T0),
                approved: without_preset.clone(),
                pinned_preset_id: pin,
            });
            (hub, id)
        };
        let fire = |hub: &mut LibraryHub, id: Uuid| {
            hub.prepare_shared_fire(
                &key(id, LibKind::Loop, "L"),
                &claude(),
                scheduled(T0 + 3600),
                T0 + 3600,
                &HashSet::new(),
                &|_| true,
            )
        };
        let (mut hub, id) = hub_with_pin(Some(CLAUDE_ID));
        match fire(&mut hub, id) {
            SharedFireOutcome::Fire(f) => {
                assert_eq!(f.preset_id, CLAUDE_ID);
                assert_eq!(f.to_runtime_loop().preset_id, Some(CLAUDE_ID));
            }
            other => panic!("expected a fire, got {other:?}"),
        }
        let (mut hub, id) = hub_with_pin(Some(Uuid::from_u128(0xDEAD)));
        assert_eq!(
            fire(&mut hub, id),
            SharedFireOutcome::SwitchedOff(LoopOffReason::PinnedPresetDeleted)
        );
        assert!(hub.shared_loops.is_empty());
        let (mut hub, id) = hub_with_pin(None);
        assert_eq!(
            fire(&mut hub, id),
            SharedFireOutcome::SwitchedOff(LoopOffReason::NoAgentPinned)
        );
        assert!(hub.shared_loops.is_empty());
    }

    #[test]
    fn copy_of_a_preset_less_loop_takes_the_toolbar_preset() {
        let file = LIB_FILE
            .replace(
                "preset = \"Claude\"\nauto_mode_presses = 1",
                "auto_mode_presses = 1",
            )
            .replace(
                "preset = \"Claude\"\nauto_mode_presses = 2",
                "auto_mode_presses = 2",
            );
        let (mut hub, id) = loaded_hub();
        hub.begin(id, JobKind::Update, T0).expect("begin");
        hub.finish(outcome(id, JobKind::Update, Ok(()), Ok(parsed(&file))), T0);
        let codex = Uuid::from_u128(0xC2);
        hub.shared_loops.push(SharedLoopState {
            library: id,
            name: "L".to_string(),
            run_id: Uuid::from_u128(0xF00),
            project_id: PROJECT,
            last_run: Some(T0),
            approved: LoopContent {
                preset: None,
                ..approved_l()
            },
            pinned_preset_id: Some(CLAUDE_ID),
        });
        let mut l = Lists::new();
        let lp = key(id, LibKind::Loop, "L");
        assert_eq!(
            hub.make_local_copy(&lp, &claude(), Some(codex), Some(PROJECT), T0, l.lists()),
            Ok(2)
        );
        assert_eq!(l.loops[2].preset_id, Some(codex));
        assert_eq!(
            hub.make_local_copy(&lp, &claude(), None, Some(PROJECT), T0, l.lists()),
            Ok(3)
        );
        assert_eq!(l.loops[3].preset_id, None);
        let r = key(id, LibKind::Runner, "R");
        assert_eq!(
            hub.make_local_copy(&r, &claude(), Some(codex), Some(PROJECT), T0, l.lists()),
            Ok(0)
        );
        assert_eq!(l.runners[0].preset_id, None);
    }

    #[test]
    fn to_runtime_loop_uses_fire_content_and_run_id() {
        let fire = SharedFire {
            run_id: Uuid::from_u128(0xF00),
            project_id: PROJECT,
            name: "L".to_string(),
            content: LoopContent {
                prompt: "go {{input}}".to_string(),
                preset: Some("Claude".to_string()),
                auto_mode_presses: 4,
                schedule: LoopSchedule::EveryMinutes { minutes: 5 },
                post_run: PostRunAction::Exit,
            },
            preset_id: CLAUDE_ID,
        };
        let l = fire.to_runtime_loop();
        assert_eq!(l.id, Uuid::from_u128(0xF00));
        assert_eq!(l.name, "L");
        assert_eq!(l.project_id, PROJECT);
        assert_eq!(l.preset_id, Some(CLAUDE_ID));
        assert_eq!(l.prompt, "go {{input}}");
        assert_eq!(l.auto_mode_presses, 4);
        assert_eq!(l.schedule, LoopSchedule::EveryMinutes { minutes: 5 });
        assert_eq!(l.post_run, PostRunAction::Exit);
        let instance = l.build_instance(&claude()[0]);
        assert_eq!(instance.system_prompt.as_deref(), Some("go"));
        assert_eq!(instance.custom_name.as_deref(), Some("L"));
        assert_eq!(instance.auto_mode_presses, 4);
    }

    // --- Menu sections ---

    #[test]
    fn library_menu_sections_omit_libraries_without_items_of_that_kind() {
        let mut hub = LibraryHub::default();
        let a = hub.add("file:///team-a", "", "").expect("add");
        let b = hub.add("file:///team-b", "", "").expect("add");
        let _unread = hub.add("file:///team-c", "", "").expect("add");
        hub.begin(a, JobKind::Update, T0).expect("begin");
        hub.finish(
            outcome(
                a,
                JobKind::Update,
                Ok(()),
                Ok(parsed(
                    "[[snippets]]\nname=\"s1\"\n[[snippets]]\nname=\"s2\"\n",
                )),
            ),
            T0,
        );
        hub.begin(b, JobKind::Update, T0).expect("begin");
        hub.finish(
            outcome(b, JobKind::Update, Ok(()), Err(FileError::Missing)),
            T0,
        );
        let snippets = hub.library_menu_sections(LibKind::Snippet);
        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].config.id, a);
        assert_eq!(snippets[0].items.snippets.len(), 2);
        assert!(hub.library_menu_sections(LibKind::Runner).is_empty());
        assert!(hub.library_menu_sections(LibKind::Loop).is_empty());
    }

    #[test]
    fn library_menu_sections_follow_add_order() {
        let mut hub = LibraryHub::default();
        let a = hub.add("file:///a", "", "").expect("add");
        let b = hub.add("file:///b", "", "").expect("add");
        for id in [b, a] {
            hub.begin(id, JobKind::Update, T0).expect("begin");
            hub.finish(
                outcome(id, JobKind::Update, Ok(()), Ok(parsed(LIB_FILE))),
                T0,
            );
        }
        let ids: Vec<Uuid> = hub
            .library_menu_sections(LibKind::Loop)
            .iter()
            .map(|s| s.config.id)
            .collect();
        assert_eq!(ids, vec![a, b]);
    }

    // --- Lookups by identity ---

    #[test]
    fn loaded_lookups_by_identity() {
        let (hub, id) = loaded_hub();
        assert_eq!(
            hub.loaded_runner(&key(id, LibKind::Runner, "R"))
                .map(|r| r.content.prompt.as_str()),
            Some("review")
        );
        assert!(hub.loaded_runner(&key(id, LibKind::Loop, "R")).is_none());
        assert!(hub.loaded_loop(&key(id, LibKind::Loop, "L")).is_some());
        assert!(
            hub.loaded_snippet(&key(id, LibKind::Snippet, "A"))
                .is_some()
        );
        assert!(
            hub.loaded_snippet(&key(id, LibKind::Snippet, "B"))
                .is_none()
        );
        assert!(hub.file_ok(id));
    }

    // --- Startup cleanup ---

    #[test]
    fn stale_lib_dir_entries_are_everything_but_configured_ids() {
        let a = Uuid::from_u128(0xA);
        let b = Uuid::from_u128(0xB);
        let names = vec![
            a.to_string(),
            b.to_string(),
            format!("{a}.tmp"),
            "stray.txt".to_string(),
        ];
        assert_eq!(
            stale_lib_dir_entries(&names, &[a]),
            vec![b.to_string(), format!("{a}.tmp"), "stray.txt".to_string()]
        );
        assert_eq!(stale_lib_dir_entries(&names, &[]), names);
        assert!(stale_lib_dir_entries(&[], &[a]).is_empty());
    }
}
