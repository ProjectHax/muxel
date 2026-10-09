//! Team libraries orchestration without GPUI: library jobs, adding and
//! removing libraries, and the user-facing texts of the core's results.
//!
//! Every text quoting library-file, parser or git content passes it through
//! [`sanitize`], so a library can't inject control or bidi characters.

use std::path::Path;
use std::time::{Duration, Instant};

use muxel_core::library::config::AddError;
use muxel_core::library::hub::{
    JobKind, JobOutcome, JobSpec, LibActions, LibRuntime, LibraryHub, stale_lib_dir_entries,
};
use muxel_core::library::parse::parse_library;
use muxel_core::library::resolve::CopyError;
use muxel_core::library::resync::{LocalChanges, Plural, ResyncConfirm, ResyncLoss, resync_loss};
use muxel_core::library::sanitize::sanitize;
use muxel_core::library::{
    FileError, GIT_TIMEOUT_CLONE_SECS, GIT_TIMEOUT_PULL_SECS, GitFailure, IssueKind, ItemIssue,
    LIBRARY_FILE, LibKind, LibWarning, LibraryConfig, LoopOffReason, ParsedLibrary,
    RESYNC_CHECK_TIMEOUT_SECS,
};
use uuid::Uuid;

use crate::i18n::{t, tf};
use crate::integrations::{
    GitEnv, library_clone, library_local_changes, library_pull, library_resync, remove_dir_force,
};

// --- Jobs ---

/// Time limits of one library job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobLimits {
    /// Clone, and the whole re-sync.
    pub clone: Duration,
    pub pull: Duration,
}

impl JobLimits {
    pub const STANDARD: JobLimits = JobLimits {
        clone: Duration::from_secs(GIT_TIMEOUT_CLONE_SECS),
        pull: Duration::from_secs(GIT_TIMEOUT_PULL_SECS),
    };
}

/// Run `spec` with the standard time limits. Blocking: call it off the UI
/// thread.
pub fn run_job(env: &GitEnv, lib_dir: &Path, spec: &JobSpec) -> JobOutcome {
    run_job_with_limits(env, lib_dir, spec, JobLimits::STANDARD)
}

/// `Update` pulls `LIB_DIR/<id>`, or clones it if missing (a folder without
/// `.git` is `GitFailure::NotAClone`); `Resync` re-syncs it. The library file
/// is always re-read, so the items on disk stay loaded after a failed update.
pub fn run_job_with_limits(
    env: &GitEnv,
    lib_dir: &Path,
    spec: &JobSpec,
    limits: JobLimits,
) -> JobOutcome {
    let clone_dir = lib_dir.join(spec.id.to_string());
    let start = Instant::now();
    let git = match spec.kind {
        JobKind::Update if clone_dir.exists() => {
            library_pull(env, &spec.url, &clone_dir, start + limits.pull)
        }
        JobKind::Update => library_clone(
            env,
            &spec.url,
            &spec.branch,
            &clone_dir,
            lib_dir,
            start + limits.clone,
        ),
        JobKind::Resync => library_resync(
            env,
            &spec.url,
            &spec.branch,
            lib_dir,
            spec.id,
            start + limits.clone,
        ),
        JobKind::Check => {
            unreachable!("the local-changes check is not a job: LibraryHub::begin refuses it")
        }
    };
    let read = read_library_file(&clone_dir);
    JobOutcome {
        id: spec.id,
        kind: spec.kind,
        git,
        read,
    }
}

/// The local-changes check of library `id` before a re-sync, within
/// `RESYNC_CHECK_TIMEOUT_SECS`. Blocking: call it off the UI thread.
pub fn run_check(env: &GitEnv, lib_dir: &Path, id: Uuid) -> LocalChanges {
    run_check_with_limit(
        env,
        lib_dir,
        id,
        Duration::from_secs(RESYNC_CHECK_TIMEOUT_SECS),
    )
}

/// [`run_check`] with another total time limit (tests: 0 s).
pub fn run_check_with_limit(
    env: &GitEnv,
    lib_dir: &Path,
    id: Uuid,
    limit: Duration,
) -> LocalChanges {
    library_local_changes(env, &lib_dir.join(id.to_string()), Instant::now() + limit)
}

/// Read and parse `<clone>/muxel-library.toml`.
pub fn read_library_file(clone_dir: &Path) -> Result<ParsedLibrary, FileError> {
    let bytes = match std::fs::read(clone_dir.join(LIBRARY_FILE)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(FileError::Missing),
        Err(e) => return Err(FileError::Unreadable(e.to_string())),
    };
    let text = String::from_utf8(bytes).map_err(|_| FileError::NotUtf8)?;
    parse_library(&text)
}

// --- Adding a library ---

#[derive(Debug)]
pub enum AddLibraryError {
    /// Invalid or duplicate URL/branch: nothing changed.
    Rejected(AddError),
    /// The settings could not be saved: the add was rolled back.
    SaveFailed(String),
}

/// Add a library and persist it before anything is cloned; a failed save
/// rolls the add back. On `Ok(id)` the caller starts the clone.
pub fn add_library(
    hub: &mut LibraryHub,
    url: &str,
    branch: &str,
    name: &str,
    persist: impl FnOnce(&LibraryHub) -> anyhow::Result<()>,
) -> Result<Uuid, AddLibraryError> {
    let id = hub
        .add(url, branch, name)
        .map_err(AddLibraryError::Rejected)?;
    if let Err(e) = persist(hub) {
        hub.rollback_add(id);
        return Err(AddLibraryError::SaveFailed(format!("{e:#}")));
    }
    Ok(id)
}

// --- Removing a library and the startup cleanup ---

/// Delete the clone `LIB_DIR/<id>` of a removed library, without asking.
/// A failed delete is retried once after clearing read-only attributes; on
/// `Err` the folder stays and the next startup cleanup removes it.
pub fn delete_library_files(env: &GitEnv, lib_dir: &Path, id: Uuid) -> Result<(), ()> {
    let clone_dir = lib_dir.join(id.to_string());
    if std::fs::symlink_metadata(&clone_dir).is_err() {
        return Ok(());
    }
    remove_dir_force(env, &clone_dir).map_err(|_| ())
}

/// Delete every first-level entry of `lib_dir` not named after a configured
/// library (old clones, `<id>.c-…`/`.r-…`/`.o-…` leftovers). Only when
/// `config.toml` existed and parsed, before any update. Best effort.
pub fn startup_cleanup(lib_dir: &Path, ids: &[Uuid], env: &GitEnv) {
    let Ok(read) = std::fs::read_dir(lib_dir) else {
        return;
    };
    let mut names = Vec::new();
    // A name that is not UTF-8 can't be a library id: it goes too.
    let mut stale = Vec::new();
    for entry in read.flatten() {
        match entry.file_name().into_string() {
            Ok(name) => names.push(name),
            Err(_) => stale.push(entry.path()),
        }
    }
    stale.extend(
        stale_lib_dir_entries(&names, ids)
            .into_iter()
            .map(|name| lib_dir.join(name)),
    );
    for path in stale {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            let _ = remove_dir_force(env, &path);
        } else if std::fs::remove_file(&path).is_err() {
            // A directory symlink on Windows is removed with remove_dir.
            let _ = std::fs::remove_dir(&path);
        }
    }
}

// --- Texts ---

/// Make a value read from a library file, git or the parser safe to show.
fn clean(raw: &str) -> String {
    sanitize(raw)
}

fn table_key(kind: LibKind) -> &'static str {
    match kind {
        LibKind::Snippet => "snippets",
        LibKind::Runner => "runners",
        LibKind::Loop => "loops",
    }
}

/// Message of an add that was refused or rolled back.
pub fn add_error_text(err: &AddLibraryError) -> String {
    match err {
        AddLibraryError::Rejected(AddError::EmptyUrl) => {
            t("Enter the URL of the library's git repository.").to_string()
        }
        AddLibraryError::Rejected(AddError::UrlStartsWithDash) => {
            t("The URL can't start with \"-\".").to_string()
        }
        AddLibraryError::Rejected(AddError::BranchStartsWithDash) => {
            t("The branch can't start with \"-\".").to_string()
        }
        AddLibraryError::Rejected(AddError::Duplicate) => {
            t("A library with this URL and branch is already added.").to_string()
        }
        AddLibraryError::SaveFailed(detail) => tf(
            "Couldn't save the settings, so the library was not added: {detail}",
            &[("detail", detail)],
        ),
    }
}

/// Message of a failed git operation.
pub fn git_failure_text(failure: &GitFailure) -> String {
    match failure {
        GitFailure::NotFound => {
            t("git not found. Install git and make sure it is on your PATH.").to_string()
        }
        GitFailure::TimedOut { secs } => tf(
            "git did not finish within {secs} seconds and was stopped.",
            &[("secs", &secs.to_string())],
        ),
        GitFailure::Failed { detail } => tf("git failed: {detail}", &[("detail", &clean(detail))]),
        GitFailure::Io { detail } => tf(
            "File system error in the library folder: {detail}",
            &[("detail", &clean(detail))],
        ),
        GitFailure::NotAClone => t(
            "The library folder is not a git clone. Use \"Re-sync from repository\" to clone it again.",
        )
        .to_string(),
        GitFailure::ResyncRestoreFailed {
            replace,
            restore,
            leftover,
        } => tf(
            "Re-sync failed ({replace}) and the previous clone could not be restored ({restore}). The library will be cloned again on the next update; the leftover folder {leftover} may be removed the next time muxel starts.",
            &[
                ("replace", &clean(replace)),
                ("restore", &clean(restore)),
                ("leftover", &clean(leftover)),
            ],
        ),
    }
}

/// Message of a library file that could not be read: names
/// `muxel-library.toml` and, for a syntax error, the line.
pub fn file_error_text(err: &FileError) -> String {
    match err {
        FileError::Missing => tf(
            "{file} was not found in the library repository.",
            &[("file", LIBRARY_FILE)],
        ),
        FileError::NotUtf8 => tf("{file} is not valid UTF-8 text.", &[("file", LIBRARY_FILE)]),
        FileError::Unreadable(detail) => tf(
            "Couldn't read {file}: {detail}",
            &[("file", LIBRARY_FILE), ("detail", &clean(detail))],
        ),
        FileError::Syntax {
            line: Some(line),
            detail,
        } => tf(
            "{file} is not valid TOML (line {line}): {detail}",
            &[
                ("file", LIBRARY_FILE),
                ("line", &line.to_string()),
                ("detail", &clean(detail)),
            ],
        ),
        FileError::Syntax { line: None, detail } => tf(
            "{file} is not valid TOML: {detail}",
            &[("file", LIBRARY_FILE), ("detail", &clean(detail))],
        ),
    }
}

/// `snippets #2 "Name"` (or `snippets #2` without a usable name).
fn item_label(issue: &ItemIssue) -> String {
    let table = table_key(issue.table);
    let n = issue.position.to_string();
    match &issue.name {
        Some(name) => tf(
            "{table} #{n} \"{name}\"",
            &[("table", table), ("n", &n), ("name", &clean(name))],
        ),
        None => tf("{table} #{n}", &[("table", table), ("n", &n)]),
    }
}

/// Message of the first discarded item. A whole key that is not an
/// array of tables names that key.
pub fn issue_text(issue: &ItemIssue) -> String {
    let item = item_label(issue);
    match &issue.problem {
        IssueKind::TableNotArray => tf(
            "\"{key}\" is not an array of tables; no items of this type were loaded",
            &[("key", table_key(issue.table))],
        ),
        IssueKind::MissingName => tf("{item} discarded: it has no name", &[("item", &item)]),
        IssueKind::WrongType { field, expected } => tf(
            "{item} discarded: {field} must be a {expected}",
            &[
                ("item", &item),
                ("field", &clean(field)),
                ("expected", *expected),
            ],
        ),
        IssueKind::OutOfRange { field } => tf(
            "{item} discarded: {field} is out of range",
            &[("item", &item), ("field", &clean(field))],
        ),
        IssueKind::UnknownScheduleKind(kind) => tf(
            "{item} discarded: unknown schedule kind \"{kind}\"",
            &[("item", &item), ("kind", &clean(kind))],
        ),
        IssueKind::UnknownPostRun(value) => tf(
            "{item} discarded: unknown post_run \"{value}\"",
            &[("item", &item), ("value", &clean(value))],
        ),
        IssueKind::DuplicateName => tf(
            "{item} discarded: an earlier item has the same name",
            &[("item", &item)],
        ),
    }
}

/// Message of the first warning.
pub fn warning_text(warning: &LibWarning) -> String {
    match warning {
        LibWarning::UnknownField {
            table,
            position,
            field,
        } => tf(
            "{table} #{n}: unknown field \"{field}\" ignored",
            &[
                ("table", table_key(*table)),
                ("n", &position.to_string()),
                ("field", &clean(field)),
            ],
        ),
        LibWarning::UnknownTable(name) => tf(
            "Unknown table \"{table}\" ignored",
            &[("table", &clean(name))],
        ),
    }
}

pub fn preset_not_found_text(preset: &str) -> String {
    tf(
        "Agent preset \"{preset}\" not found",
        &[("preset", &clean(preset))],
    )
}

/// The error event of a library whose files could not be deleted, with the
/// library's display name.
pub fn delete_error_text(name: &str) -> String {
    tf(
        "Couldn't delete the files of library \"{name}\". They will be removed the next time muxel starts.",
        &[("name", name)],
    )
}

/// The counted re-sync confirmation. One whole-sentence template per form so
/// translations can reorder it; both counts 0 → the generic text. `{name}` is
/// substituted last so a name containing `{files}` is not expanded.
pub fn resync_confirm_text(name: &str, files: usize, commits: usize) -> String {
    let (f, c) = (files.to_string(), commits.to_string());
    let args: &[(&str, &str)] = &[("files", &f), ("commits", &c), ("name", name)];
    let Some(loss) = resync_loss(files, commits) else {
        return resync_generic_text(name);
    };
    match loss {
        ResyncLoss::Files(Plural::One) => tf(
            "{files} modified or untracked file in muxel's copy of \"{name}\" will be lost.",
            args,
        ),
        ResyncLoss::Files(Plural::Other) => tf(
            "{files} modified or untracked files in muxel's copy of \"{name}\" will be lost.",
            args,
        ),
        ResyncLoss::Commits(Plural::One) => tf(
            "{commits} local commit in muxel's copy of \"{name}\" will be lost.",
            args,
        ),
        ResyncLoss::Commits(Plural::Other) => tf(
            "{commits} local commits in muxel's copy of \"{name}\" will be lost.",
            args,
        ),
        ResyncLoss::Both(Plural::One, Plural::One) => tf(
            "{files} modified or untracked file and {commits} local commit in muxel's copy of \"{name}\" will be lost.",
            args,
        ),
        ResyncLoss::Both(Plural::One, Plural::Other) => tf(
            "{files} modified or untracked file and {commits} local commits in muxel's copy of \"{name}\" will be lost.",
            args,
        ),
        ResyncLoss::Both(Plural::Other, Plural::One) => tf(
            "{files} modified or untracked files and {commits} local commit in muxel's copy of \"{name}\" will be lost.",
            args,
        ),
        ResyncLoss::Both(Plural::Other, Plural::Other) => tf(
            "{files} modified or untracked files and {commits} local commits in muxel's copy of \"{name}\" will be lost.",
            args,
        ),
    }
}

/// The generic re-sync confirmation: the check could not tell what would
/// be lost.
pub fn resync_generic_text(name: &str) -> String {
    tf(
        "Re-sync \"{name}\" from its repository? Local changes in muxel's copy may be lost.",
        &[("name", name)],
    )
}

pub fn resync_prompt_text(name: &str, confirm: ResyncConfirm) -> String {
    match confirm {
        ResyncConfirm::Counted { files, commits } => resync_confirm_text(name, files, commits),
        ResyncConfirm::Generic => resync_generic_text(name),
    }
}

/// The event of a shared loop switched off by a content change.
pub fn switched_off_text(loop_name: &str) -> String {
    tf(
        "Shared loop \"{name}\" was switched off because it changed in its library",
        &[("name", &clean(loop_name))],
    )
}

/// Title of the event of a shared loop switched off because its agent does
/// not resolve: the same string `fire_loop` uses for a private loop.
pub fn shared_loop_off_title(loop_name: &str) -> String {
    tf("Loop “{name}” turned off", &[("name", &clean(loop_name))])
}

/// Why a shared loop's agent does not resolve, as the event's detail and on its row.
pub fn loop_off_reason_text(reason: &LoopOffReason) -> String {
    match reason {
        LoopOffReason::PresetNotFound(preset) => preset_not_found_text(preset),
        LoopOffReason::PinnedPresetDeleted => t(
            "Its agent preset no longer exists. Turn it back on from the Loops menu and pick an agent.",
        )
        .to_string(),
        LoopOffReason::NoAgentPinned => {
            t("It doesn't name an agent. Turn it back on from the Loops menu and pick one.")
                .to_string()
        }
    }
}

/// The hint shown in the switch-on dialog of a loop without `preset` while
/// no agent is picked.
pub fn agent_pick_hint_text() -> String {
    t("Pick the agent this loop runs with").to_string()
}

/// The tooltip of an enabled "Make a local copy".
pub fn copy_tooltip_text() -> String {
    t("Creates a private copy you can edit. It is not synced with the library.").to_string()
}

/// Title and body of the error event of a failed "Make a local copy";
/// `None` (no event) when the item is gone.
pub fn copy_error_text(err: &CopyError) -> Option<(String, String)> {
    match err {
        CopyError::Gone => None,
        CopyError::NoProject => Some((
            t("Can't add a loop").to_string(),
            t("Open a project first — a loop runs in a specific project.").to_string(),
        )),
        CopyError::PresetNotFound(preset) => Some((
            t("Can't make a local copy").to_string(),
            preset_not_found_text(preset),
        )),
    }
}

// --- Settings → Libraries row ---

/// The text of a library with a git operation in progress.
pub fn busy_text() -> String {
    t("Git operation in progress…").to_string()
}

pub fn branch_text(branch: &str) -> String {
    if branch.is_empty() {
        t("default").to_string()
    } else {
        branch.to_string()
    }
}

/// The last successful pull as `YYYY-MM-DD HH:MM` in `tz` (the app passes
/// `chrono::Local`); empty when there was none.
pub fn pull_time_text<Tz: chrono::TimeZone>(secs: Option<u64>, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    secs.and_then(|s| i64::try_from(s).ok())
        .and_then(|s| tz.timestamp_opt(s, 0).single())
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// What a library's Settings row shows besides its name, URL and branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryRowStatus {
    pub busy: Option<String>,
    /// Last successful pull, local time; empty if none.
    pub last_pull: String,
    /// The last git error, then the file error.
    pub errors: Vec<String>,
    /// Loaded snippets, runners and loops; `None` until the file is read.
    pub counts: Option<String>,
    /// Discarded items with the first one's message.
    pub discarded: Option<String>,
    /// Warnings with the first one's message.
    pub warnings: Option<String>,
}

pub fn library_row_status<Tz: chrono::TimeZone>(
    config: &LibraryConfig,
    runtime: Option<&LibRuntime>,
    actions: LibActions,
    tz: &Tz,
) -> LibraryRowStatus
where
    Tz::Offset: std::fmt::Display,
{
    let mut row = LibraryRowStatus {
        busy: actions.busy.then(busy_text),
        last_pull: pull_time_text(config.last_pull_ok, tz),
        ..LibraryRowStatus::default()
    };
    let Some(rt) = runtime else {
        return row;
    };
    if let Some(e) = &rt.git_error {
        row.errors.push(git_failure_text(e));
    }
    if let Some(e) = &rt.file_error {
        row.errors.push(file_error_text(e));
    }
    if let Some(items) = &rt.items {
        row.counts = Some(tf(
            "Snippets: {snippets} · Runners: {runners} · Loops: {loops}",
            &[
                ("snippets", &items.snippets.len().to_string()),
                ("runners", &items.runners.len().to_string()),
                ("loops", &items.loops.len().to_string()),
            ],
        ));
        if items.discarded > 0 {
            let first = items.first_discard.as_ref().map(issue_text);
            row.discarded = Some(tf(
                "Discarded items: {n} ({first})",
                &[
                    ("n", &items.discarded.to_string()),
                    ("first", first.as_deref().unwrap_or_default()),
                ],
            ));
        }
        if items.warnings > 0 {
            let first = items.first_warning.as_ref().map(warning_text);
            row.warnings = Some(tf(
                "Warnings: {n} ({first})",
                &[
                    ("n", &items.warnings.to_string()),
                    ("first", first.as_deref().unwrap_or_default()),
                ],
            ));
        }
    }
    row
}

/// Marker next to a library's name in Settings → Libraries, since the details
/// pane shows only the selected library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryListMarker {
    /// An operation is in progress, or the startup hold is on.
    Busy,
    /// The last git operation failed or the library file could not be read.
    Error,
}

/// `Busy` wins over `Error`; discarded items and warnings get no marker.
pub fn library_list_marker(
    actions: LibActions,
    runtime: Option<&LibRuntime>,
) -> Option<LibraryListMarker> {
    if actions.busy {
        return Some(LibraryListMarker::Busy);
    }
    runtime
        .filter(|rt| rt.git_error.is_some() || rt.file_error.is_some())
        .map(|_| LibraryListMarker::Error)
}

pub fn library_list_marker_tooltip(marker: LibraryListMarker) -> String {
    match marker {
        LibraryListMarker::Busy => busy_text(),
        LibraryListMarker::Error => t("This library has an error").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AddLibraryError, JobLimits, LibraryListMarker, add_error_text, add_library,
        copy_error_text, copy_tooltip_text, delete_error_text, delete_library_files,
        file_error_text, git_failure_text, issue_text, library_list_marker,
        library_list_marker_tooltip, preset_not_found_text, read_library_file, resync_confirm_text,
        resync_generic_text, resync_prompt_text, run_check, run_check_with_limit, run_job,
        run_job_with_limits, startup_cleanup, switched_off_text, warning_text,
    };
    use crate::integrations::{GitEnv, RenameFn, remove_dir_force};
    use crate::test_support::{TestRepo, TestServer, askpass_script, file_url, git_out};
    use muxel_core::library::config::{AddError, display_name};
    use muxel_core::library::hub::{
        CheckDecision, Effects, JobKind, LibActions, LibRuntime, LibraryHub, LocalLists,
        RemoveError, ResyncRequest,
    };
    use muxel_core::library::parse::parse_library;
    use muxel_core::library::resolve::CopyError;
    use muxel_core::library::resync::{LocalChanges, ResyncConfirm};
    use muxel_core::library::{
        FileError, GitFailure, IssueKind, ItemIssue, LibItemKey, LibKind, LibWarning, LoopContent,
        PULL_INTERVAL_SECS, RunnerContent, SharedLoopState, SharedRunnerConfirmation,
    };
    use muxel_core::{LoopSchedule, PostRunAction, Settings};
    use muxel_store::{save_settings_to, try_load_settings_from};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    const LIB_FILE: &str = "muxel-library.toml";
    const FILE_A: &str = "[[snippets]]\nname = \"A\"\ntext = \"one\"\n";
    const FILE_OLD: &str = "[[snippets]]\nname = \"Old\"\ntext = \"old\"\n";
    const FILE_A_NEW: &str = "[[snippets]]\nname = \"A\"\ntext = \"one\"\n\n[[snippets]]\nname = \"New\"\ntext = \"two\"\n";
    const T0: u64 = 1_700_000_000;

    /// A temporary directory path (not created), removed on drop.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new() -> Self {
            Self(crate::test_support::short_temp_path())
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            if self.0.exists() {
                let _ = remove_dir_force(&GitEnv::production(), &self.0);
            }
        }
    }

    /// `R` with `muxel-library.toml` = `file` on `main`.
    fn remote(file: &str) -> TestRepo {
        let repo = TestRepo::init();
        repo.commit(&[(LIB_FILE, file), ("README.md", "readme\n")], "init");
        repo
    }

    /// `begin` + `run_job` + `finish` on the calling thread.
    fn run_sync(
        hub: &mut LibraryHub,
        id: Uuid,
        kind: JobKind,
        env: &GitEnv,
        lib_dir: &Path,
        now: u64,
    ) -> Effects {
        let spec = hub.begin(id, kind, now).expect("library free to start");
        let outcome = run_job(env, lib_dir, &spec);
        hub.finish(outcome, now)
    }

    /// Round trip through the production persistence path (`write_into` → save
    /// → load → `take_from`); returns the reloaded hub and settings.
    fn persist_roundtrip(
        hub: &LibraryHub,
        settings: &mut Settings,
        path: &Path,
    ) -> (LibraryHub, Settings) {
        hub.write_into(settings);
        save_settings_to(path, settings).expect("save settings");
        let mut loaded = try_load_settings_from(path)
            .expect("settings parse")
            .expect("settings exist");
        let reloaded = LibraryHub::take_from(&mut loaded);
        (reloaded, loaded)
    }

    fn save_to(path: &Path) -> impl FnOnce(&LibraryHub) -> anyhow::Result<()> + '_ {
        move |hub: &LibraryHub| {
            let mut settings = Settings::default();
            hub.write_into(&mut settings);
            save_settings_to(path, &settings)
        }
    }

    /// Top-level entries of `dir`, sorted; empty if it does not exist.
    fn entries(dir: &Path) -> Vec<String> {
        let Ok(read) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = read
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn snippet_names(hub: &LibraryHub, id: Uuid) -> Vec<String> {
        hub.items(id)
            .map(|p| p.snippets.iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default()
    }

    fn head(dir: &Path) -> String {
        git_out(dir, &["log", "-1", "--format=%H"])
    }

    // ---- Adding a library ----

    #[test]
    fn add_persists_then_clones_and_settings_hold_the_url() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let cfg_dir = TmpDir::new();
        let cfg = cfg_dir.path().join("config.toml");
        let env = GitEnv::for_tests();
        let url = file_url(repo.path());

        let mut hub = LibraryHub::default();
        let id = add_library(&mut hub, &url, "", "", save_to(&cfg)).expect("added");
        // Persisted before any clone: config has it, LIB_DIR untouched.
        assert!(std::fs::read_to_string(&cfg).unwrap().contains(&url));
        assert!(entries(lib_dir.path()).is_empty());
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);

        let mut settings = Settings::default();
        let (mut reloaded, _) = persist_roundtrip(&hub, &mut settings, &cfg);
        assert_eq!(reloaded.configs.len(), 1);
        assert_eq!(reloaded.configs[0].id, id);
        assert_eq!(reloaded.configs[0].url, url);

        let spec = reloaded.begin(id, JobKind::Update, T0).expect("begin");
        let outcome = run_job(&env, lib_dir.path(), &spec);
        assert_eq!(outcome.git, Ok(()));
        reloaded.finish(outcome, T0);

        let cloned = std::fs::read(lib_dir.path().join(id.to_string()).join(LIB_FILE)).unwrap();
        assert_eq!(cloned, std::fs::read(repo.path().join(LIB_FILE)).unwrap());
        assert_eq!(snippet_names(&reloaded, id), vec!["A".to_string()]);
        let saved = std::fs::read_to_string(&cfg).unwrap();
        assert!(saved.contains(&url), "{saved}");
    }

    #[test]
    fn branch_clone_has_that_branch_file() {
        let repo = remote(FILE_A);
        repo.git(&["checkout", "-q", "-b", "team"]);
        let team_file = "[[snippets]]\nname = \"Team\"\ntext = \"t\"\n";
        repo.commit(&[(LIB_FILE, team_file)], "team");
        repo.git(&["checkout", "-q", "main"]);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();

        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "team", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);

        let cloned =
            std::fs::read_to_string(lib_dir.path().join(id.to_string()).join(LIB_FILE)).unwrap();
        assert_eq!(cloned, team_file);
        assert_eq!(snippet_names(&hub, id), vec!["Team".to_string()]);
    }

    #[test]
    fn two_libraries_have_own_dirs_and_items() {
        let r1 = remote("[[snippets]]\nname = \"One\"\n");
        let r2 = remote("[[snippets]]\nname = \"Two\"\n");
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();

        let mut hub = LibraryHub::default();
        let id1 = hub.add(&file_url(r1.path()), "", "").unwrap();
        let id2 = hub.add(&file_url(r2.path()), "", "").unwrap();
        run_sync(&mut hub, id1, JobKind::Update, &env, lib_dir.path(), T0);
        run_sync(&mut hub, id2, JobKind::Update, &env, lib_dir.path(), T0);

        let mut expected = vec![id1.to_string(), id2.to_string()];
        expected.sort();
        assert_eq!(entries(lib_dir.path()), expected);
        assert_eq!(snippet_names(&hub, id1), vec!["One".to_string()]);
        assert_eq!(snippet_names(&hub, id2), vec!["Two".to_string()]);
    }

    // ---- Automatic updates ----

    #[test]
    fn startup_update_pulls_new_snippet() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        assert_eq!(snippet_names(&hub, id), vec!["A".to_string()]);

        repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");
        let mut settings = Settings::default();
        hub.write_into(&mut settings);
        let mut started = LibraryHub::take_from(&mut settings);
        started.hold_startup();
        assert_eq!(started.begin(id, JobKind::Update, T0 + 10), None);
        for lib in started.release_startup() {
            run_sync(
                &mut started,
                lib,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 10,
            );
        }
        assert_eq!(
            snippet_names(&started, id),
            vec!["A".to_string(), "New".to_string()]
        );
        assert_eq!(started.runtime(id).unwrap().git_error, None);
    }

    #[test]
    fn startup_update_reclones_a_deleted_clone() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        let clone = lib_dir.path().join(id.to_string());
        remove_dir_force(&env, &clone).unwrap();
        assert!(!clone.exists());

        let mut settings = Settings::default();
        hub.write_into(&mut settings);
        let mut started = LibraryHub::take_from(&mut settings);
        run_sync(
            &mut started,
            id,
            JobKind::Update,
            &env,
            lib_dir.path(),
            T0 + 10,
        );

        assert!(clone.join(LIB_FILE).is_file());
        assert_eq!(snippet_names(&started, id), vec!["A".to_string()]);
        assert_eq!(started.runtime(id).unwrap().git_error, None);
    }

    /// Here `LIB_DIR` is inside another git repository: the update must neither
    /// pull the outer repository nor delete the folder.
    #[test]
    fn clone_folder_without_git_is_an_error_not_a_pull() {
        let repo = remote(FILE_A);
        let outer = TestRepo::init();
        let outer_head = outer.commit(&[("outer.txt", "outer\n")], "outer");
        let lib_dir = outer.path().join("libs");
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        let clone = lib_dir.join(id.to_string());
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();

        run_sync(&mut hub, id, JobKind::Update, &env, &lib_dir, T0);

        assert_eq!(
            hub.runtime(id).unwrap().git_error,
            Some(GitFailure::NotAClone)
        );
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0, "git was run");
        assert_eq!(
            std::fs::read_to_string(clone.join("notes.txt")).unwrap(),
            "mine\n"
        );
        assert!(!clone.join(".git").exists());
        assert_eq!(outer.git(&["rev-parse", "HEAD"]), outer_head);
        assert!(
            git_failure_text(&GitFailure::NotAClone).contains("Re-sync from repository"),
            "the message names the recovery"
        );

        run_sync(&mut hub, id, JobKind::Resync, &env, &lib_dir, T0 + 10);
        assert_eq!(hub.runtime(id).unwrap().git_error, None);
        assert!(clone.join(".git").exists());
        assert_eq!(snippet_names(&hub, id), vec!["A".to_string()]);
    }

    #[test]
    fn list_marker_error_for_a_git_or_file_error() {
        let git = LibRuntime {
            git_error: Some(GitFailure::NotFound),
            ..LibRuntime::default()
        };
        let file = LibRuntime {
            file_error: Some(FileError::Missing),
            ..LibRuntime::default()
        };
        assert_eq!(
            library_list_marker(FREE, Some(&git)),
            Some(LibraryListMarker::Error)
        );
        assert_eq!(
            library_list_marker(FREE, Some(&file)),
            Some(LibraryListMarker::Error)
        );
    }

    #[test]
    fn list_marker_none_without_errors() {
        assert_eq!(library_list_marker(FREE, None), None);
        assert_eq!(
            library_list_marker(FREE, Some(&LibRuntime::default())),
            None
        );
        // Discarded items and warnings alone are no error.
        let parsed = parse_library(
            "[[snippets]]\nname = \"A\"\ntext = \"a\"\nbogus = 1\n\n[[snippets]]\nname = \"B\"\n",
        )
        .unwrap();
        assert!(parsed.discarded > 0 || parsed.warnings > 0);
        let loaded = LibRuntime {
            items: Some(parsed),
            ..LibRuntime::default()
        };
        assert_eq!(library_list_marker(FREE, Some(&loaded)), None);
    }

    #[test]
    fn list_marker_busy_while_busy_or_held_even_with_an_error() {
        let failed = LibRuntime {
            git_error: Some(GitFailure::NotFound),
            ..LibRuntime::default()
        };
        assert_eq!(
            library_list_marker(BUSY, None),
            Some(LibraryListMarker::Busy)
        );
        assert_eq!(
            library_list_marker(BUSY, Some(&failed)),
            Some(LibraryListMarker::Busy)
        );

        let mut hub = LibraryHub::default();
        let id = hub.add("https://example.com/team.git", "", "").unwrap();
        hub.hold_startup();
        assert_eq!(
            library_list_marker(hub.actions(id), hub.runtime(id)),
            Some(LibraryListMarker::Busy)
        );
        hub.release_startup();
        assert_eq!(library_list_marker(hub.actions(id), hub.runtime(id)), None);
    }

    #[test]
    fn list_marker_tooltips() {
        assert_eq!(
            library_list_marker_tooltip(LibraryListMarker::Busy),
            "Git operation in progress…"
        );
        assert_eq!(
            library_list_marker_tooltip(LibraryListMarker::Error),
            "This library has an error"
        );
    }

    // ---- List marker and actions ----

    #[test]
    fn list_marker_and_actions_through_startup_hold_and_finish() {
        let env = GitEnv::for_tests();
        let lib_dir = TmpDir::new();
        let good = remote(FILE_A);
        let pulled = remote(FILE_A);
        let no_file = TestRepo::init();
        no_file.commit(&[("README.md", "readme\n")], "init");
        let mut hub = LibraryHub::default();
        let a = hub.add(&file_url(good.path()), "", "").unwrap();
        let b = hub.add(&file_url(pulled.path()), "", "").unwrap();
        let c = hub.add(&file_url(no_file.path()), "", "").unwrap();
        // `B`'s remote is gone, so its next pull fails.
        run_sync(&mut hub, b, JobKind::Update, &env, lib_dir.path(), T0);
        assert_eq!(hub.runtime(b).unwrap().git_error, None);
        git_out(
            &lib_dir.path().join(b.to_string()),
            &[
                "remote",
                "set-url",
                "origin",
                "file:///nonexistent/muxel-missing-library",
            ],
        );

        // Startup hold: all three busy, the three actions disabled.
        hub.hold_startup();
        for id in [a, b, c] {
            assert_eq!(hub.actions(id), BUSY, "{id}");
            assert_eq!(
                library_list_marker(hub.actions(id), hub.runtime(id)),
                Some(LibraryListMarker::Busy)
            );
        }

        let released = hub.release_startup();
        assert_eq!(released.len(), 3);
        for id in [a, b, c] {
            run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0 + 5);
        }
        assert_eq!(hub.runtime(a).unwrap().git_error, None);
        assert_eq!(hub.runtime(a).unwrap().file_error, None);
        assert!(matches!(
            hub.runtime(b).unwrap().git_error,
            Some(GitFailure::Failed { .. })
        ));
        assert_eq!(hub.runtime(c).unwrap().git_error, None);
        assert_eq!(hub.runtime(c).unwrap().file_error, Some(FileError::Missing));
        assert_eq!(library_list_marker(hub.actions(a), hub.runtime(a)), None);
        for id in [b, c] {
            assert_eq!(
                library_list_marker(hub.actions(id), hub.runtime(id)),
                Some(LibraryListMarker::Error),
                "{id}"
            );
        }
        for id in [a, b, c] {
            assert_eq!(hub.actions(id), FREE, "{id}");
        }

        // `B` still in error, with a new update in progress: busy, not error.
        let _spec = hub.begin(b, JobKind::Update, T0 + 9).expect("begin");
        assert!(hub.runtime(b).unwrap().git_error.is_some());
        assert_eq!(
            library_list_marker(hub.actions(b), hub.runtime(b)),
            Some(LibraryListMarker::Busy)
        );
        assert_eq!(hub.actions(b), BUSY);
    }

    // ---- No askpass on clone, pull or re-sync ----

    /// Every askpass source is set (`GIT_ASKPASS`, `SSH_ASKPASS`, `core.askPass`);
    /// `credential.interactive=true` after the production `-c` options stands for
    /// git < 2.46, so only `base_command`'s askpass environment stops the prompt.
    #[test]
    fn clone_pull_and_resync_never_run_an_askpass_program() {
        use muxel_core::library::{GIT_TIMEOUT_CLONE_SECS, GIT_TIMEOUT_PULL_SECS};
        use std::ffi::OsString;

        let server = TestServer::unauthorized();
        let url = server.url("team-lib.git");
        let work = TmpDir::new();
        std::fs::create_dir_all(work.path()).unwrap();
        let marker = work.path().join("askpass.called");
        let script = askpass_script(work.path(), &marker);
        let global = work.path().join("gitconfig");
        let script_cfg = script.display().to_string().replace('\\', "/");
        std::fs::write(&global, format!("[core]\n\taskPass = \"{script_cfg}\"\n")).unwrap();

        let mut env = GitEnv::for_tests();
        env.extra_env.retain(|(k, _)| k != "GIT_CONFIG_GLOBAL");
        let mut vars: Vec<(&str, OsString)> = vec![
            ("GIT_CONFIG_GLOBAL", global.clone().into_os_string()),
            ("GIT_ASKPASS", script.clone().into_os_string()),
            ("SSH_ASKPASS", script.clone().into_os_string()),
            ("SSH_ASKPASS_REQUIRE", OsString::from("force")),
        ];
        if cfg!(not(windows)) {
            vars.push(("DISPLAY", OsString::from(":0")));
        }
        for (k, v) in vars {
            env.extra_env.push((OsString::from(k), v));
        }
        env.extra_config
            .push("credential.interactive=true".to_string());

        // Control: plain git with this environment does run the askpass
        // program, so the setup can catch a regression.
        let _ = std::process::Command::new("git")
            .args(["-c", "credential.interactive=true", "ls-remote", "--", &url])
            .envs(env.extra_env.iter().map(|(k, v)| (k, v)))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run git");
        assert!(marker.exists(), "control: the askpass program was not run");
        std::fs::remove_file(&marker).unwrap();

        let lib_dir = work.path().join("libs");
        std::fs::create_dir_all(&lib_dir).unwrap();
        let mut hub = LibraryHub::default();
        let failure = |hub: &LibraryHub, id: Uuid| {
            let err = hub.runtime(id).unwrap().git_error.clone();
            let err = err.expect("the operation failed");
            assert!(!git_failure_text(&err).trim().is_empty(), "{err:?}");
            err
        };

        let cloned = hub.add(&url, "", "").unwrap();
        let start = Instant::now();
        run_sync(&mut hub, cloned, JobKind::Update, &env, &lib_dir, T0);
        assert!(start.elapsed() <= Duration::from_secs(GIT_TIMEOUT_CLONE_SECS));
        let err = failure(&hub, cloned);
        assert!(matches!(err, GitFailure::Failed { .. }), "clone: {err:?}");
        assert!(!marker.exists(), "askpass ran on clone");

        // Pull: a clone of a local repo whose `origin` now is the server.
        let local = remote(FILE_A);
        let pull_url = server.url("team-lib-2.git");
        let pulled = hub.add(&pull_url, "", "").unwrap();
        let clone = lib_dir.join(pulled.to_string());
        git_out(
            &lib_dir,
            &["clone", "-q", &file_url(local.path()), &pulled.to_string()],
        );
        git_out(&clone, &["remote", "set-url", "origin", &pull_url]);
        let clone_head = head(&clone);
        let start = Instant::now();
        run_sync(&mut hub, pulled, JobKind::Update, &env, &lib_dir, T0 + 1);
        assert!(start.elapsed() <= Duration::from_secs(GIT_TIMEOUT_PULL_SECS));
        let err = failure(&hub, pulled);
        assert!(matches!(err, GitFailure::Failed { .. }), "pull: {err:?}");
        assert!(!marker.exists(), "askpass ran on pull");

        // Confirmed re-sync of that library.
        let start = Instant::now();
        run_sync(&mut hub, pulled, JobKind::Resync, &env, &lib_dir, T0 + 2);
        assert!(start.elapsed() <= Duration::from_secs(GIT_TIMEOUT_CLONE_SECS));
        failure(&hub, pulled);
        assert!(!marker.exists(), "askpass ran on re-sync");
        assert_eq!(head(&clone), clone_head);

        // Every git child was waited for.
        let reaped = env.reaped.as_ref().expect("for_tests keeps children");
        let mut reaped = reaped.lock().unwrap();
        assert!(reaped.len() >= 3, "{} children", reaped.len());
        assert_eq!(reaped.len(), env.spawned.load(Ordering::SeqCst));
        for child in reaped.iter_mut() {
            assert!(
                matches!(child.try_wait(), Ok(Some(_))),
                "child still running"
            );
        }
    }

    // ---- Broken clone inside another repo ----

    /// Every file under `dir` (relative path → bytes).
    fn files_of(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(base, &path, out);
                } else {
                    let rel = path.strip_prefix(base).unwrap().to_path_buf();
                    out.insert(rel, std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    #[test]
    fn clone_dir_without_git_inside_another_repo_touches_neither() {
        let q = TestRepo::init();
        q.commit(&[("q.txt", "q\n")], "q1");
        let p = TestRepo::init();
        p.commit(&[("p.txt", "p\n")], "p1");
        p.git(&["remote", "add", "origin", &file_url(q.path())]);
        let repo = remote(FILE_A);
        let lib_dir = p.path().join("data").join("libraries");
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        let clone = lib_dir.join(id.to_string());
        std::fs::create_dir_all(clone.join("sub")).unwrap();
        std::fs::write(clone.join(LIB_FILE), FILE_A).unwrap();
        std::fs::write(clone.join("sub").join("blob.bin"), [0u8, 1, 2, 0xff, 0x80]).unwrap();

        let p_head = p.git(&["log", "-1", "--format=%H"]);
        let p_refs = p.git(&["for-each-ref"]);
        let p_status = p.git(&["status", "--porcelain"]);
        let files = files_of(&clone);
        assert_eq!(files.len(), 2);
        let fetch_head = p.path().join(".git").join("FETCH_HEAD");
        assert!(!fetch_head.exists());

        hub.hold_startup();
        assert_eq!(hub.release_startup(), vec![id]);
        for (step, now) in [("startup", T0), ("pull now", T0 + 60)] {
            run_sync(&mut hub, id, JobKind::Update, &env, &lib_dir, now);
            let err = hub.runtime(id).unwrap().git_error.clone();
            assert_eq!(err, Some(GitFailure::NotAClone), "{step}");
            assert!(!git_failure_text(&GitFailure::NotAClone).trim().is_empty());
            assert!(!clone.join(".git").exists(), "{step}");
            assert_eq!(files_of(&clone), files, "{step}");
            assert_eq!(p.git(&["log", "-1", "--format=%H"]), p_head, "{step}");
            assert_eq!(p.git(&["for-each-ref"]), p_refs, "{step}");
            assert_eq!(p.git(&["status", "--porcelain"]), p_status, "{step}");
            assert!(!fetch_head.exists(), "{step}");
        }
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0, "git was run");
    }

    // ---- Rename saved to config.toml, no git ----

    #[test]
    fn rename_is_saved_to_config_and_runs_no_git() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let cfg_dir = TmpDir::new();
        std::fs::create_dir_all(cfg_dir.path()).unwrap();
        let cfg = cfg_dir.path().join("config.toml");
        let env = GitEnv::for_tests();
        let url = file_url(repo.path());
        let mut hub = LibraryHub::default();
        let id = hub.add(&url, "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        let clone = lib_dir.path().join(id.to_string());
        let clone_head = head(&clone);
        let spawned = env.spawned.load(Ordering::SeqCst);
        let mut settings = Settings::default();

        assert!(hub.rename(id, "  Team  "));
        let (reloaded, _) = persist_roundtrip(&hub, &mut settings, &cfg);
        let saved = std::fs::read_to_string(&cfg).unwrap();
        assert!(saved.contains("name = \"Team\""), "{saved}");
        let config = reloaded.config(id).expect("configured");
        assert_eq!(display_name(config), "Team");
        assert_eq!(config.url, url);
        assert_eq!(config.branch, "");
        assert_eq!(hub.runtime(id).unwrap().last_attempt, Some(T0));
        assert_eq!(head(&clone), clone_head);
        assert_eq!(env.spawned.load(Ordering::SeqCst), spawned, "git was run");

        assert!(hub.rename(id, "   "));
        let (reloaded, _) = persist_roundtrip(&hub, &mut settings, &cfg);
        let repo_name = repo.path().file_name().unwrap().to_string_lossy();
        assert_eq!(display_name(reloaded.config(id).unwrap()), repo_name);
        assert!(!std::fs::read_to_string(&cfg).unwrap().contains("Team"));
        assert_eq!(head(&clone), clone_head);
        assert_eq!(env.spawned.load(Ordering::SeqCst), spawned, "git was run");
    }

    #[test]
    fn update_reflects_removed_and_added_items() {
        let repo = remote(FILE_OLD);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        assert_eq!(snippet_names(&hub, id), vec!["Old".to_string()]);

        repo.commit(&[(LIB_FILE, "")], "remove Old");
        repo.commit(&[(LIB_FILE, "[[snippets]]\nname = \"New\"\n")], "add New");
        run_sync(
            &mut hub,
            id,
            JobKind::Update,
            &env,
            lib_dir.path(),
            T0 + 400,
        );

        let names = snippet_names(&hub, id);
        assert!(names.contains(&"New".to_string()), "{names:?}");
        assert!(!names.contains(&"Old".to_string()), "{names:?}");
    }

    #[test]
    fn three_updates_leave_clone_clean_and_remote_untouched() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        let remote_commits_before = repo.git(&["log", "--all", "--format=%H"]);

        for i in 1..=3 {
            run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + i * PULL_INTERVAL_SECS,
            );
            assert_eq!(hub.runtime(id).unwrap().git_error, None);
        }

        let clone = lib_dir.path().join(id.to_string());
        assert_eq!(git_out(&clone, &["status", "--porcelain"]), "");
        assert_eq!(head(&clone), repo.git(&["rev-parse", "main"]));
        assert_eq!(
            repo.git(&["log", "--all", "--format=%H"]),
            remote_commits_before
        );
    }

    // ---- git missing ----

    #[test]
    fn git_missing_from_path_is_an_error_with_git_not_found_text() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let empty_path = TmpDir::new();
        std::fs::create_dir_all(empty_path.path()).unwrap();
        let env = GitEnv {
            path_override: Some(empty_path.path().as_os_str().to_os_string()),
            ..GitEnv::for_tests()
        };
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);

        let err = hub.runtime(id).unwrap().git_error.clone().expect("error");
        assert_eq!(err, GitFailure::NotFound);
        let text = git_failure_text(&err);
        assert!(
            text.starts_with("git not found") || text.starts_with("git is not found"),
            "{text}"
        );
        assert_eq!(hub.config(id).unwrap().last_pull_ok, None);
    }

    // ---- Refused adds end to end, rollback of a failed save ----

    #[test]
    fn duplicate_add_is_rejected_and_clones_nothing() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let cfg_dir = TmpDir::new();
        let cfg = cfg_dir.path().join("config.toml");
        let env = GitEnv::for_tests();
        let url = file_url(repo.path());

        // Contrast: a valid add + its update spawns exactly one git.
        let mut hub = LibraryHub::default();
        let id = add_library(&mut hub, &url, "", "", save_to(&cfg)).expect("added");
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        assert_eq!(env.spawned.load(Ordering::SeqCst), 1);
        let before = entries(lib_dir.path());
        assert_eq!(before, vec![id.to_string()]);

        let err = add_library(&mut hub, &url, "", "", save_to(&cfg)).expect_err("duplicate");
        assert!(matches!(
            err,
            AddLibraryError::Rejected(AddError::Duplicate)
        ));
        assert!(!add_error_text(&err).is_empty());
        // No id to dispatch: nothing ran, LIB_DIR unchanged, one library.
        assert_eq!(env.spawned.load(Ordering::SeqCst), 1);
        assert_eq!(entries(lib_dir.path()), before);
        assert_eq!(hub.configs.len(), 1);
        let (reloaded, _) = persist_roundtrip(&hub, &mut Settings::default(), &cfg);
        assert_eq!(reloaded.configs.len(), 1);
    }

    #[test]
    fn invalid_adds_are_rejected_without_git_or_lib_dir() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let cfg_dir = TmpDir::new();
        let cfg = cfg_dir.path().join("config.toml");
        let env = GitEnv::for_tests();
        let url = file_url(repo.path());
        let mut hub = LibraryHub::default();

        let cases: [(&str, &str); 5] = [
            ("", ""),
            ("   ", ""),
            ("-uhack", ""),
            (" --upload-pack=x", ""),
            (url.as_str(), "--orphan"),
        ];
        for (u, b) in cases {
            let err = add_library(&mut hub, u, b, "", save_to(&cfg)).expect_err("rejected");
            assert!(matches!(err, AddLibraryError::Rejected(_)), "{u:?} {b:?}");
            assert!(!add_error_text(&err).trim().is_empty(), "{u:?} {b:?}");
        }
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);
        assert!(entries(lib_dir.path()).is_empty());
        assert!(hub.configs.is_empty());
        assert!(!cfg.exists(), "a rejected add never saves");
        let (reloaded, _) = persist_roundtrip(&hub, &mut Settings::default(), &cfg);
        assert!(reloaded.configs.is_empty());

        // Contrast: the same URL without the bad branch is accepted and cloned.
        let id = add_library(&mut hub, &url, "", "", save_to(&cfg)).expect("valid add");
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        assert_eq!(env.spawned.load(Ordering::SeqCst), 1);
        assert_eq!(entries(lib_dir.path()), vec![id.to_string()]);
    }

    #[test]
    fn failed_save_rolls_back_the_add_and_clones_nothing() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();

        let err = add_library(&mut hub, &file_url(repo.path()), "", "", |_| {
            Err(anyhow::anyhow!("disk full"))
        })
        .expect_err("save failed");
        assert!(matches!(&err, AddLibraryError::SaveFailed(d) if d.contains("disk full")));
        let text = add_error_text(&err);
        assert!(text.contains("disk full"), "{text}");
        assert!(hub.configs.is_empty(), "the add is rolled back");
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);
        assert!(entries(lib_dir.path()).is_empty());
    }

    // ---- Persistence round trip ----

    #[test]
    fn persist_roundtrip_keeps_libraries_loops_and_confirmations() {
        let cfg_dir = TmpDir::new();
        let cfg = cfg_dir.path().join("config.toml");
        let mut hub = LibraryHub::default();
        let a = hub.add("file:///tmp/a", "", "Alpha").unwrap();
        let b = hub
            .add("https://example.invalid/b.git", "team", "")
            .unwrap();
        hub.shared_loops.push(SharedLoopState {
            library: a,
            name: "Nightly".to_string(),
            run_id: Uuid::from_u128(7),
            project_id: Uuid::from_u128(8),
            last_run: Some(T0),
            approved: LoopContent {
                prompt: "go".to_string(),
                preset: Some(String::new()),
                auto_mode_presses: 2,
                schedule: LoopSchedule::EveryMinutes { minutes: 5 },
                post_run: PostRunAction::Exit,
            },
            pinned_preset_id: None,
        });
        hub.runner_confirmations.push(SharedRunnerConfirmation {
            library: b,
            name: "Review".to_string(),
            confirmed: RunnerContent {
                prompt: "review".to_string(),
                preset: None,
                auto_mode_presses: 0,
            },
        });

        let mut settings = Settings::default();
        let (reloaded, _) = persist_roundtrip(&hub, &mut settings, &cfg);
        assert_eq!(reloaded.configs, hub.configs);
        assert_eq!(reloaded.shared_loops, hub.shared_loops);
        assert_eq!(reloaded.runner_confirmations, hub.runner_confirmations);
        assert_eq!(reloaded.configs.len(), 2);
    }

    // ---- Pull now and the last successful pull ----

    #[test]
    fn pull_now_loads_new_counts_as_attempt_and_runs_once() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");
        let spawned_before = env.spawned.load(Ordering::SeqCst);

        let pull_at = T0 + 60;
        let spec = hub.begin(id, JobKind::Update, pull_at).expect("pull now");
        // A second Pull now while the first runs is ignored.
        assert_eq!(hub.begin(id, JobKind::Update, pull_at), None);
        let outcome = run_job(&env, lib_dir.path(), &spec);
        hub.finish(outcome, pull_at);

        assert_eq!(env.spawned.load(Ordering::SeqCst), spawned_before + 1);
        assert!(snippet_names(&hub, id).contains(&"New".to_string()));
        assert!(
            !hub.due_updates(pull_at + PULL_INTERVAL_SECS - 1)
                .contains(&id)
        );
        assert!(hub.due_updates(pull_at + PULL_INTERVAL_SECS).contains(&id));
    }

    #[test]
    fn last_pull_ok_survives_reload_and_a_failed_update() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let cfg_dir = TmpDir::new();
        let cfg = cfg_dir.path().join("config.toml");
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        assert_eq!(hub.config(id).unwrap().last_pull_ok, Some(T0));

        let (mut reloaded, _) = persist_roundtrip(&hub, &mut Settings::default(), &cfg);
        assert_eq!(reloaded.runtime(id), None, "no update yet");
        assert_eq!(reloaded.config(id).unwrap().last_pull_ok, Some(T0));

        remove_dir_force(&env, repo.path()).expect("delete R");
        assert!(!repo.path().exists());
        run_sync(
            &mut reloaded,
            id,
            JobKind::Update,
            &env,
            lib_dir.path(),
            T0 + 400,
        );
        let rt = reloaded.runtime(id).unwrap();
        assert!(rt.git_error.is_some());
        assert!(!git_failure_text(rt.git_error.as_ref().unwrap()).is_empty());
        assert_eq!(reloaded.config(id).unwrap().last_pull_ok, Some(T0));
        // The items readable on disk are still loaded.
        assert_eq!(snippet_names(&reloaded, id), vec!["A".to_string()]);
    }

    // ---- File errors ----

    #[test]
    fn missing_file_is_an_error_naming_the_file() {
        let repo = TestRepo::init();
        repo.commit(&[("README.md", "no library file\n")], "init");
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);

        let rt = hub.runtime(id).unwrap();
        assert_eq!(rt.git_error, None);
        assert_eq!(rt.file_error, Some(FileError::Missing));
        let items = hub.items(id).unwrap();
        assert_eq!(
            items.snippets.len() + items.runners.len() + items.loops.len(),
            0
        );
        let text = file_error_text(rt.file_error.as_ref().unwrap());
        assert!(text.contains("muxel-library.toml"), "{text}");
    }

    #[test]
    fn syntax_error_names_the_file_and_line() {
        let repo = remote("[[snippets]]\nname = \"a\"\n[[snippets\n");
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);

        let rt = hub.runtime(id).unwrap();
        let err = rt.file_error.clone().expect("file error");
        assert!(
            matches!(err, FileError::Syntax { line: Some(3), .. }),
            "{err:?}"
        );
        assert_eq!(hub.items(id).unwrap().snippets.len(), 0);
        let text = file_error_text(&err);
        assert!(text.contains("muxel-library.toml"), "{text}");
        assert!(text.contains('3'), "{text}");
    }

    #[test]
    fn read_library_file_reports_not_utf8_and_missing() {
        let dir = TmpDir::new();
        std::fs::create_dir_all(dir.path()).unwrap();
        assert_eq!(read_library_file(dir.path()), Err(FileError::Missing));
        std::fs::write(dir.path().join(LIB_FILE), [0xff, 0xfe, 0x00, b'x']).unwrap();
        assert_eq!(read_library_file(dir.path()), Err(FileError::NotUtf8));
        assert!(file_error_text(&FileError::NotUtf8).contains("muxel-library.toml"));
        std::fs::write(dir.path().join(LIB_FILE), FILE_A).unwrap();
        let parsed = read_library_file(dir.path()).expect("valid file");
        assert_eq!(parsed.snippets.len(), 1);
    }

    #[test]
    fn run_job_with_limits_times_out_a_pull_that_never_ends() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);

        let server = crate::test_support::TestServer::silent();
        let clone = lib_dir.path().join(id.to_string());
        git_out(
            &clone,
            &["remote", "set-url", "origin", &server.url("r.git")],
        );
        let spec = hub.begin(id, JobKind::Update, T0 + 400).unwrap();
        let limits = JobLimits {
            clone: Duration::from_secs(2),
            pull: Duration::from_secs(2),
        };
        let started = std::time::Instant::now();
        let outcome = run_job_with_limits(&env, lib_dir.path(), &spec, limits);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            matches!(outcome.git, Err(GitFailure::TimedOut { .. })),
            "{:?}",
            outcome.git
        );
        assert_eq!(outcome.read.expect("read").snippets.len(), 1);
        assert_eq!(JobLimits::STANDARD.clone, Duration::from_secs(120));
        assert_eq!(JobLimits::STANDARD.pull, Duration::from_secs(60));
    }

    // ---- Texts ----

    #[test]
    fn out_of_range_text_names_minutes_and_item() {
        let parsed = parse_library(
            "[[loops]]\nname = \"Bad\"\nschedule = { kind = \"every_minutes\", minutes = 0 }\n\n\
             [[loops]]\nprompt = \"x\"\n\n[[loops]]\nname = \"Ok\"\n",
        )
        .unwrap();
        assert_eq!(parsed.discarded, 2);
        let text = issue_text(parsed.first_discard.as_ref().unwrap());
        assert!(text.contains("minutes"), "{text}");
        assert!(text.contains("Bad"), "{text}");
    }

    #[test]
    fn table_not_array_text_names_the_key() {
        let first = parse_library(
            "snippets = \"x\"\n\n[[runners]]\nname = \"R1\"\n\n[[loops]]\nname = \"L1\"\n",
        )
        .unwrap();
        let text = issue_text(first.first_discard.as_ref().unwrap());
        assert!(
            text.contains("\"snippets\" is not an array of tables"),
            "{text}"
        );

        let second =
            parse_library("runners = 3\nloops = [1, 2]\n\n[[snippets]]\nname = \"S\"\n").unwrap();
        let text = issue_text(second.first_discard.as_ref().unwrap());
        assert!(
            text.contains("\"runners\" is not an array of tables"),
            "{text}"
        );
        assert!(!text.contains("snippets"), "{text}");
    }

    #[test]
    fn issue_texts_are_non_empty_for_every_kind() {
        let issue = |problem: IssueKind, name: Option<&str>| ItemIssue {
            table: LibKind::Runner,
            position: 2,
            name: name.map(str::to_string),
            problem,
        };
        let cases = [
            issue(IssueKind::MissingName, None),
            issue(
                IssueKind::WrongType {
                    field: "prompt".to_string(),
                    expected: "string",
                },
                Some("R"),
            ),
            issue(IssueKind::UnknownPostRun("stay".to_string()), Some("R")),
            issue(IssueKind::DuplicateName, Some("R")),
        ];
        for c in &cases {
            let text = issue_text(c);
            assert!(text.contains("runners #2"), "{text}");
        }
        assert!(issue_text(&cases[1]).contains("prompt"));
        assert!(issue_text(&cases[2]).contains("stay"));
    }

    #[test]
    fn raw_file_strings_are_sanitized_in_texts() {
        // A bidi override (U+202E) and an isolate (U+2066) in names read raw
        // from the file: the texts keep the visible letters, drop the controls.
        let parsed = parse_library(
            "[[snippets]]\nname = \"S\"\n\"co\\u202Elor\" = 1\n\n\
             [[loops]]\nname = \"L\"\nschedule = { kind = \"week\\u202Ely\" }\n\n\
             [[loops]]\nname = \"P\"\npost_run = \"st\\u2066ay\"\n\n\
             [\"ev\\u2066il\"]\nx = 1\n",
        )
        .unwrap();
        let warning = warning_text(parsed.first_warning.as_ref().unwrap());
        assert!(warning.contains("\"color\""), "{warning}");
        assert!(!warning.contains('\u{202E}'), "{warning:?}");

        let kind = issue_text(parsed.first_discard.as_ref().unwrap());
        assert!(kind.contains("\"weekly\""), "{kind}");
        assert!(!kind.contains('\u{202E}'), "{kind:?}");

        let post_run = issue_text(&ItemIssue {
            table: LibKind::Loop,
            position: 2,
            name: Some("P".to_string()),
            problem: IssueKind::UnknownPostRun("st\u{2066}ay".to_string()),
        });
        assert!(post_run.contains("\"stay\""), "{post_run}");

        let table = warning_text(&LibWarning::UnknownTable("ev\u{2066}il".to_string()));
        assert!(table.contains("\"evil\""), "{table}");
        assert!(!table.contains('\u{2066}'), "{table:?}");

        let git = git_failure_text(&GitFailure::Failed {
            detail: "remote: \u{1b}[31mdenied\u{202E}".to_string(),
        });
        assert!(git.contains("[31mdenied"), "{git}");
        assert!(
            !git.contains('\u{1b}') && !git.contains('\u{202E}'),
            "{git:?}"
        );
    }

    #[test]
    fn resync_restore_failed_text_names_cause_and_leftover() {
        let leftover = "0f0e0d0c-0000-4000-8000-000000000001.o-1a2b3c4d";
        let text = git_failure_text(&GitFailure::ResyncRestoreFailed {
            replace: "replace-cause-xyz".to_string(),
            restore: "restore-cause-abc".to_string(),
            leftover: leftover.to_string(),
        });
        assert!(!text.is_empty());
        assert!(text.contains(leftover), "{text}");
        assert!(text.contains("replace-cause-xyz"), "{text}");
        assert!(text.contains("restore-cause-abc"), "{text}");
        assert!(text.contains("cloned again on the next update"), "{text}");
        assert!(
            text.contains("may be removed the next time muxel starts"),
            "{text}"
        );
    }

    #[test]
    fn git_failure_texts_are_non_empty() {
        let timed = git_failure_text(&GitFailure::TimedOut { secs: 60 });
        assert!(timed.contains("60"), "{timed}");
        let io = git_failure_text(&GitFailure::Io {
            detail: "denied-io".to_string(),
        });
        assert!(io.contains("denied-io"), "{io}");
        let failed = git_failure_text(&GitFailure::Failed {
            detail: "not a fast-forward".to_string(),
        });
        assert!(failed.contains("not a fast-forward"), "{failed}");
    }

    #[test]
    fn file_error_texts_name_the_file() {
        for err in [
            FileError::Missing,
            FileError::NotUtf8,
            FileError::Unreadable("busy".to_string()),
            FileError::Syntax {
                line: None,
                detail: "bad".to_string(),
            },
        ] {
            let text = file_error_text(&err);
            assert!(text.contains("muxel-library.toml"), "{text}");
        }
    }

    #[test]
    fn preset_not_found_text_is_exact() {
        assert_eq!(
            preset_not_found_text("NoSuchAgent"),
            "Agent preset \"NoSuchAgent\" not found"
        );
        assert_eq!(preset_not_found_text(""), "Agent preset \"\" not found");
    }

    #[test]
    fn user_texts_are_exact() {
        assert_eq!(
            delete_error_text("Team"),
            "Couldn't delete the files of library \"Team\". They will be removed the next time muxel starts."
        );
        assert_eq!(
            resync_generic_text("Team"),
            "Re-sync \"Team\" from its repository? Local changes in muxel's copy may be lost."
        );
        assert_eq!(
            switched_off_text("Nightly"),
            "Shared loop \"Nightly\" was switched off because it changed in its library"
        );
    }

    #[test]
    fn copy_tooltip_text_is_exact() {
        assert_eq!(
            copy_tooltip_text(),
            "Creates a private copy you can edit. It is not synced with the library."
        );
    }

    #[test]
    fn copy_error_texts() {
        assert_eq!(copy_error_text(&CopyError::Gone), None);
        assert_eq!(
            copy_error_text(&CopyError::NoProject),
            Some((
                "Can't add a loop".to_string(),
                "Open a project first — a loop runs in a specific project.".to_string()
            ))
        );
        let (title, body) =
            copy_error_text(&CopyError::PresetNotFound("Ghost".to_string())).unwrap();
        assert!(!title.is_empty());
        assert_eq!(body, "Agent preset \"Ghost\" not found");
    }

    #[test]
    fn add_error_texts_are_distinct_and_non_empty() {
        let texts: Vec<String> = [
            AddError::EmptyUrl,
            AddError::UrlStartsWithDash,
            AddError::BranchStartsWithDash,
            AddError::Duplicate,
        ]
        .into_iter()
        .map(|e| add_error_text(&AddLibraryError::Rejected(e)))
        .collect();
        for (i, a) in texts.iter().enumerate() {
            assert!(!a.is_empty());
            for b in &texts[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    // ---- Removing a library ----

    const FILE_TEAM: &str = "[[snippets]]\nname = \"Greet\"\ntext = \"hello\"\n\n[[runners]]\nname = \"Review\"\n\n[[loops]]\nname = \"Nightly\"\n";

    /// Switch on the shared loop `Nightly` and confirm the runner `Review`
    /// of library `id`.
    fn switch_on_and_confirm(hub: &mut LibraryHub, id: Uuid) {
        hub.shared_loops.push(SharedLoopState {
            library: id,
            name: "Nightly".to_string(),
            run_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            last_run: Some(T0),
            approved: LoopContent {
                prompt: String::new(),
                preset: None,
                auto_mode_presses: 0,
                schedule: LoopSchedule::EveryMinutes { minutes: 5 },
                post_run: PostRunAction::Exit,
            },
            pinned_preset_id: Some(Uuid::from_u128(0xC1)),
        });
        hub.runner_confirmations.push(SharedRunnerConfirmation {
            library: id,
            name: "Review".to_string(),
            confirmed: RunnerContent {
                prompt: String::new(),
                preset: None,
                auto_mode_presses: 0,
            },
        });
    }

    /// Mark every file under `dir` (`.git/objects` included) read-only.
    fn make_files_read_only(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                make_files_read_only(&path);
            } else {
                let mut perms = std::fs::metadata(&path).unwrap().permissions();
                perms.set_readonly(true);
                std::fs::set_permissions(&path, perms).unwrap();
            }
        }
    }

    /// The first file under `<dir>/.git/objects`.
    fn some_object_file(dir: &Path) -> PathBuf {
        fn find(dir: &Path) -> Option<PathBuf> {
            for entry in std::fs::read_dir(dir).ok()? {
                let path = entry.ok()?.path();
                if path.is_dir() {
                    if let Some(found) = find(&path) {
                        return Some(found);
                    }
                } else {
                    return Some(path);
                }
            }
            None
        }
        find(&dir.join(".git").join("objects")).expect("an object file")
    }

    #[test]
    fn delete_removes_clone_and_shared_state_and_keeps_the_local_copy() {
        let repo = remote(FILE_TEAM);
        let lib_dir = TmpDir::new();
        let cfg_dir = TmpDir::new();
        let cfg = cfg_dir.path().join("config.toml");
        let env = GitEnv::for_tests();
        let url = file_url(repo.path());
        let mut hub = LibraryHub::default();
        let id = hub.add(&url, "", "Team").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        switch_on_and_confirm(&mut hub, id);

        let mut settings = Settings::default();
        let key = LibItemKey {
            library: id,
            kind: LibKind::Snippet,
            name: "Greet".to_string(),
        };
        let idx = hub
            .make_local_copy(
                &key,
                &[],
                None,
                None,
                T0,
                LocalLists {
                    snippets: &mut settings.snippets,
                    runners: &mut settings.runners,
                    loops: &mut settings.loops,
                },
            )
            .expect("local copy");
        let copy = settings.snippets[idx].clone();
        assert_eq!((copy.name.as_str(), copy.text.as_str()), ("Greet", "hello"));
        let fields = |list: &[muxel_core::Snippet]| -> Vec<(Uuid, String, String, bool)> {
            list.iter()
                .map(|s| (s.id, s.name.clone(), s.text.clone(), s.submit))
                .collect()
        };
        let snippets_before = fields(&settings.snippets);
        let clone = lib_dir.path().join(id.to_string());
        std::fs::write(clone.join("README.md"), "edited by hand\n").unwrap();
        assert!(!git_out(&clone, &["status", "--porcelain"]).is_empty());

        assert_eq!(hub.shared_loops.len(), 1);
        assert_eq!(hub.runner_confirmations.len(), 1);
        for kind in [LibKind::Snippet, LibKind::Runner, LibKind::Loop] {
            assert_eq!(hub.library_menu_sections(kind).len(), 1);
        }

        // No confirmation about the local changes, just removed.
        let removed = hub.remove(id).expect("removed");
        assert_eq!(removed.id, id);
        let (reloaded, loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
        assert_eq!(delete_library_files(&env, lib_dir.path(), id), Ok(()));

        assert!(!clone.exists());
        assert!(entries(lib_dir.path()).is_empty());
        assert!(reloaded.configs.is_empty());
        assert!(reloaded.items(id).is_none());
        for kind in [LibKind::Snippet, LibKind::Runner, LibKind::Loop] {
            assert!(hub.library_menu_sections(kind).is_empty());
            assert!(reloaded.library_menu_sections(kind).is_empty());
        }
        assert!(reloaded.shared_loops.is_empty());
        assert!(reloaded.runner_confirmations.is_empty());
        let saved = std::fs::read_to_string(&cfg).unwrap();
        assert!(!saved.contains(&url), "{saved}");
        assert!(!saved.contains("Nightly"), "{saved}");
        // The private snippets, the local copy included, are untouched.
        assert_eq!(fields(&loaded.snippets), snippets_before);
        assert!(
            loaded
                .snippets
                .iter()
                .any(|s| s.id == copy.id && s.name == "Greet" && s.text == "hello")
        );
    }

    #[test]
    fn delete_read_only_clone_is_deleted_without_error() {
        let repo = remote(FILE_TEAM);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        let clone = lib_dir.path().join(id.to_string());
        make_files_read_only(&clone);
        let object = some_object_file(&clone);
        assert!(std::fs::metadata(&object).unwrap().permissions().readonly());
        assert!(
            std::fs::metadata(clone.join(LIB_FILE))
                .unwrap()
                .permissions()
                .readonly()
        );

        hub.remove(id).expect("removed");
        assert_eq!(delete_library_files(&env, lib_dir.path(), id), Ok(()));
        assert!(!clone.exists());
    }

    #[test]
    fn delete_failed_delete_still_removes_the_library_and_startup_cleans_it() {
        let repo = remote(FILE_TEAM);
        let lib_dir = TmpDir::new();
        let cfg_dir = TmpDir::new();
        let cfg = cfg_dir.path().join("config.toml");
        let env = GitEnv::for_tests();
        let url = file_url(repo.path());
        let mut hub = LibraryHub::default();
        let id = hub.add(&url, "", "Team").unwrap();
        run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0);
        switch_on_and_confirm(&mut hub, id);
        let clone = lib_dir.path().join(id.to_string());
        let marker = clone.join("README.md");
        let mut perms = std::fs::metadata(&marker).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&marker, perms).unwrap();

        // remove_dir always fails; record whether the marker was read-only
        // at each attempt.
        let attempts: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let failing = GitEnv {
            remove_dir: {
                let attempts = attempts.clone();
                let marker = marker.clone();
                Arc::new(move |_: &Path| {
                    let read_only = std::fs::metadata(&marker).unwrap().permissions().readonly();
                    attempts.lock().unwrap().push(read_only);
                    Err(std::io::Error::other("injected delete failure"))
                })
            },
            ..GitEnv::for_tests()
        };

        let removed = hub.remove(id).expect("removed");
        let event = match delete_library_files(&failing, lib_dir.path(), id) {
            Ok(()) => None,
            Err(()) => Some(delete_error_text(&display_name(&removed))),
        };
        assert_eq!(*attempts.lock().unwrap(), vec![true, false]);
        assert_eq!(
            event.as_deref(),
            Some(
                "Couldn't delete the files of library \"Team\". They will be removed the next time muxel starts."
            )
        );
        assert!(clone.exists());
        let (reloaded, _) = persist_roundtrip(&hub, &mut Settings::default(), &cfg);
        assert!(reloaded.configs.is_empty());
        assert!(reloaded.items(id).is_none());
        assert!(hub.library_menu_sections(LibKind::Snippet).is_empty());
        assert!(reloaded.shared_loops.is_empty());
        assert!(reloaded.runner_confirmations.is_empty());
        let saved = std::fs::read_to_string(&cfg).unwrap();
        assert!(!saved.contains(&url), "{saved}");

        let ids: Vec<Uuid> = reloaded.configs.iter().map(|c| c.id).collect();
        startup_cleanup(lib_dir.path(), &ids, &GitEnv::for_tests());
        assert!(!clone.exists());
        assert!(entries(lib_dir.path()).is_empty());
    }

    // ---- Startup cleanup ----

    /// The app's start up to just before the updates: the cleanup
    /// runs only when `config.toml` existed and parsed (`Ok(Some(_))`).
    fn start_until_updates(cfg: &Path, lib_dir: &Path, env: &GitEnv) -> LibraryHub {
        match try_load_settings_from(cfg) {
            Ok(Some(mut settings)) => {
                let hub = LibraryHub::take_from(&mut settings);
                let ids: Vec<Uuid> = hub.configs.iter().map(|c| c.id).collect();
                startup_cleanup(lib_dir, &ids, env);
                hub
            }
            Ok(None) | Err(_) => LibraryHub::default(),
        }
    }

    /// Add `B` (a UUID dir with a file), `<a>.tmp` and `stray.txt` to
    /// `lib_dir`, which already holds `<a>`. Returns the four names, sorted.
    fn add_strays(lib_dir: &Path, a: Uuid) -> Vec<String> {
        let b = Uuid::new_v4().to_string();
        std::fs::create_dir_all(lib_dir.join(&b)).unwrap();
        std::fs::write(lib_dir.join(&b).join(LIB_FILE), FILE_A).unwrap();
        std::fs::create_dir_all(lib_dir.join(format!("{a}.tmp"))).unwrap();
        std::fs::write(lib_dir.join("stray.txt"), "stray\n").unwrap();
        let mut names = vec![
            a.to_string(),
            b,
            format!("{a}.tmp"),
            "stray.txt".to_string(),
        ];
        names.sort();
        names
    }

    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    out.insert(rel, std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    #[test]
    fn startup_cleanup_valid_config_keeps_only_configured_clones() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let cfg_dir = TmpDir::new();
        let cfg = cfg_dir.path().join("config.toml");
        let env = GitEnv::for_tests();
        let mut hub = LibraryHub::default();
        let a = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, a, JobKind::Update, &env, lib_dir.path(), T0);
        persist_roundtrip(&hub, &mut Settings::default(), &cfg);
        let clone = lib_dir.path().join(a.to_string());
        let files_before = snapshot(&clone);
        let head_before = head(&clone);
        assert_eq!(add_strays(lib_dir.path(), a).len(), 4);
        assert_eq!(entries(lib_dir.path()).len(), 4);

        let started = start_until_updates(&cfg, lib_dir.path(), &env);
        assert_eq!(started.configs.len(), 1);
        assert_eq!(entries(lib_dir.path()), vec![a.to_string()]);
        assert_eq!(snapshot(&clone), files_before);
        assert_eq!(head(&clone), head_before);
    }

    #[test]
    fn startup_cleanup_invalid_config_deletes_nothing() {
        let lib_dir = TmpDir::new();
        let cfg_dir = TmpDir::new();
        let cfg = cfg_dir.path().join("config.toml");
        std::fs::create_dir_all(cfg_dir.path()).unwrap();
        std::fs::write(&cfg, "libraries = [ this is not toml\n").unwrap();
        assert!(try_load_settings_from(&cfg).is_err());
        let a = Uuid::new_v4();
        std::fs::create_dir_all(lib_dir.path().join(a.to_string())).unwrap();
        let all = add_strays(lib_dir.path(), a);

        start_until_updates(&cfg, lib_dir.path(), &GitEnv::for_tests());
        assert_eq!(entries(lib_dir.path()), all);
    }

    #[test]
    fn startup_cleanup_missing_config_deletes_nothing() {
        let lib_dir = TmpDir::new();
        let cfg_dir = TmpDir::new();
        let cfg = cfg_dir.path().join("config.toml");
        assert!(matches!(try_load_settings_from(&cfg), Ok(None)));
        let a = Uuid::new_v4();
        std::fs::create_dir_all(lib_dir.path().join(a.to_string())).unwrap();
        let all = add_strays(lib_dir.path(), a);

        start_until_updates(&cfg, lib_dir.path(), &GitEnv::for_tests());
        assert_eq!(entries(lib_dir.path()), all);
    }

    #[test]
    fn startup_cleanup_removes_resync_old_and_clone_leftovers() {
        let lib_dir = TmpDir::new();
        let a = Uuid::new_v4();
        let other = Uuid::new_v4();
        let mut keep = vec![a.to_string(), other.to_string()];
        keep.sort();
        for name in &keep {
            std::fs::create_dir_all(lib_dir.path().join(name)).unwrap();
            std::fs::write(lib_dir.path().join(name).join(LIB_FILE), FILE_A).unwrap();
        }
        for suffix in ["old", "resync", "clone"] {
            let dir = lib_dir
                .path()
                .join(format!("{a}.{suffix}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(dir.join(".git").join("objects")).unwrap();
            std::fs::write(dir.join(".git").join("objects").join("pack"), "x").unwrap();
            make_files_read_only(&dir);
        }
        std::fs::write(lib_dir.path().join("stray.txt"), "stray\n").unwrap();
        assert_eq!(entries(lib_dir.path()).len(), 6);

        startup_cleanup(lib_dir.path(), &[a, other], &GitEnv::for_tests());
        assert_eq!(entries(lib_dir.path()), keep);
        assert_eq!(
            std::fs::read_to_string(lib_dir.path().join(a.to_string()).join(LIB_FILE)).unwrap(),
            FILE_A
        );
        // No configured library: every entry goes, LIB_DIR itself stays.
        startup_cleanup(lib_dir.path(), &[], &GitEnv::for_tests());
        assert!(entries(lib_dir.path()).is_empty());
        assert!(lib_dir.path().exists());
    }

    #[test]
    fn startup_cleanup_without_lib_dir_does_nothing() {
        let lib_dir = TmpDir::new();
        startup_cleanup(lib_dir.path(), &[Uuid::new_v4()], &GitEnv::for_tests());
        assert!(!lib_dir.path().exists());
    }

    #[test]
    fn delete_library_files_without_clone_is_ok() {
        let lib_dir = TmpDir::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counting = GitEnv {
            remove_dir: {
                let calls = calls.clone();
                Arc::new(move |dir: &Path| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::fs::remove_dir_all(dir)
                })
            },
            ..GitEnv::for_tests()
        };
        assert_eq!(
            delete_library_files(&counting, lib_dir.path(), Uuid::new_v4()),
            Ok(())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    // ---- Re-sync and concurrency ----

    const FILE_FORCED: &str = "[[snippets]]\nname = \"Forced\"\ntext = \"rewritten\"\n";
    const FREE: LibActions = LibActions {
        busy: false,
        pull_now: true,
        resync: true,
        remove: true,
    };
    const BUSY: LibActions = LibActions {
        busy: true,
        pull_now: false,
        resync: false,
        remove: false,
    };

    /// A library on `repo` (`R`) whose clone was made by an update at `T0`.
    fn cloned_library(
        repo: &TestRepo,
        lib_dir: &Path,
        env: &GitEnv,
    ) -> (LibraryHub, Uuid, PathBuf) {
        let mut hub = LibraryHub::default();
        let id = hub.add(&file_url(repo.path()), "", "").unwrap();
        run_sync(&mut hub, id, JobKind::Update, env, lib_dir, T0);
        assert_eq!(hub.runtime(id).unwrap().git_error, None);
        (hub, id, lib_dir.join(id.to_string()))
    }

    /// Delete `R` for good (git objects are read-only on Windows, so a plain
    /// `remove_dir_all` could leave a usable repository behind).
    fn delete_remote(repo: TestRepo) {
        remove_dir_force(&GitEnv::production(), repo.path()).expect("delete R");
        assert!(!repo.path().exists());
    }

    fn assert_git_error(hub: &LibraryHub, id: Uuid) -> GitFailure {
        let err = hub
            .runtime(id)
            .unwrap()
            .git_error
            .clone()
            .expect("library in error");
        assert!(!git_failure_text(&err).trim().is_empty(), "{err:?}");
        err
    }

    /// Everything a failed re-sync must leave as it was: the clone's files
    /// (`.git` included) and HEAD, and the library's items, file error and
    /// `last_pull_ok`.
    #[derive(Debug, PartialEq)]
    struct Before {
        files: BTreeMap<PathBuf, Vec<u8>>,
        head: String,
        items: Option<muxel_core::library::ParsedLibrary>,
        file_error: Option<FileError>,
        last_pull_ok: Option<u64>,
    }

    fn before(hub: &LibraryHub, id: Uuid, clone: &Path) -> Before {
        Before {
            files: snapshot(clone),
            head: head(clone),
            items: hub.items(id).cloned(),
            file_error: hub.runtime(id).and_then(|r| r.file_error.clone()),
            last_pull_ok: hub.config(id).unwrap().last_pull_ok,
        }
    }

    /// Rename hook failing when the SOURCE name contains any of `bad`; every
    /// other rename is `std::fs::rename`.
    fn failing_rename(bad: &'static [&'static str]) -> RenameFn {
        Arc::new(move |src: &Path, dst: &Path| {
            let name = src.file_name().unwrap().to_string_lossy().into_owned();
            if bad.iter().any(|b| name.contains(b)) {
                Err(std::io::Error::other(format!(
                    "injected rename failure: {name}"
                )))
            } else {
                std::fs::rename(src, dst)
            }
        })
    }

    /// Rename hook that signals `entered`, blocks its first call on `release`
    /// (at most 60 s) and then renames normally.
    fn held_rename(entered: mpsc::Sender<()>, release: mpsc::Receiver<()>) -> RenameFn {
        let first = Mutex::new(Some((entered, release)));
        Arc::new(move |src: &Path, dst: &Path| {
            let hold = first.lock().unwrap().take();
            if let Some((entered, release)) = hold {
                let _ = entered.send(());
                let _ = release.recv_timeout(Duration::from_secs(60));
            }
            std::fs::rename(src, dst)
        })
    }

    fn wait_until(cond: impl Fn() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "timed out waiting"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// What the app does on Pull now or a confirmed re-sync: run a job only
    /// if `begin` lets it start. Returns whether one ran.
    fn request_job(
        hub: &mut LibraryHub,
        id: Uuid,
        kind: JobKind,
        env: &GitEnv,
        lib_dir: &Path,
        now: u64,
    ) -> bool {
        match hub.begin(id, kind, now) {
            Some(spec) => {
                let outcome = run_job(env, lib_dir, &spec);
                hub.finish(outcome, now);
                true
            }
            None => false,
        }
    }

    /// A click on "Re-sync from repository" as the app handles it: `request_resync`
    /// → `run_check` → `finish_check`. A `Start` has begun the re-sync, not run it.
    fn click_resync(
        hub: &mut LibraryHub,
        id: Uuid,
        env: &GitEnv,
        lib_dir: &Path,
        now: u64,
    ) -> (LocalChanges, CheckDecision) {
        assert_eq!(hub.request_resync(id), ResyncRequest::Check);
        assert_eq!(hub.actions(id), BUSY);
        let changes = run_check(env, lib_dir, id);
        (changes, hub.finish_check(id, changes, now))
    }

    /// "Re-sync and confirm": click; a started re-sync runs, and a
    /// confirmation is accepted like `confirm_library_resync` does.
    fn resync_confirmed(
        hub: &mut LibraryHub,
        id: Uuid,
        env: &GitEnv,
        lib_dir: &Path,
        now: u64,
    ) -> (LocalChanges, Effects) {
        let (changes, decision) = click_resync(hub, id, env, lib_dir, now);
        let effects = match decision {
            CheckDecision::Start(spec) => {
                let outcome = run_job(env, lib_dir, &spec);
                hub.finish(outcome, now)
            }
            CheckDecision::Confirm(_) => run_sync(hub, id, JobKind::Resync, env, lib_dir, now),
            CheckDecision::Ignored => panic!("re-sync request ignored"),
        };
        (changes, effects)
    }

    fn prompt(hub: &LibraryHub, id: Uuid, decision: &CheckDecision) -> String {
        let CheckDecision::Confirm(confirm) = decision else {
            panic!("no confirmation: {decision:?}");
        };
        resync_prompt_text(&display_name(hub.config(id).unwrap()), *confirm)
    }

    /// While an operation runs: busy actions, no due update, Pull now,
    /// Re-sync and Remove ignored, library and clone still there.
    fn assert_busy_and_requests_ignored(
        hub: &mut LibraryHub,
        id: Uuid,
        env: &GitEnv,
        lib_dir: &Path,
        now: u64,
    ) {
        assert_eq!(hub.actions(id), BUSY);
        assert!(!hub.due_updates(now + 10 * PULL_INTERVAL_SECS).contains(&id));
        assert!(!request_job(hub, id, JobKind::Update, env, lib_dir, now));
        // Re-sync from repository: no dialog, and a confirmed one sent from
        // code does not start either.
        assert_eq!(hub.request_resync(id), ResyncRequest::Ignored);
        assert!(!request_job(hub, id, JobKind::Resync, env, lib_dir, now));
        assert_eq!(hub.remove(id).map(|_| ()), Err(RemoveError::Busy));
        assert!(hub.config(id).is_some());
        assert!(lib_dir.join(id.to_string()).is_dir());
        assert_eq!(hub.actions(id), BUSY);
    }

    #[test]
    fn resync_dirty_clone_becomes_identical_to_the_remote() {
        let repo = TestRepo::init();
        repo.commit(&[(LIB_FILE, FILE_A), (".gitignore", "build.log\n")], "init");
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);

        // Local commit, hand-edited library file, untracked and ignored files.
        std::fs::write(clone.join("local.txt"), "local\n").unwrap();
        git_out(&clone, &["add", "local.txt"]);
        git_out(&clone, &["commit", "-q", "-m", "local commit"]);
        std::fs::write(clone.join(LIB_FILE), "[[snippets]]\nname = \"Hand\"\n").unwrap();
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        std::fs::write(clone.join("build.log"), "log\n").unwrap();
        assert_eq!(git_out(&clone, &["check-ignore", "build.log"]), "build.log");
        let r_head = repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");

        let (changes, _) = resync_confirmed(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(
            changes,
            LocalChanges::Changes {
                files: 2,
                commits: 1
            }
        );

        let rt = hub.runtime(id).unwrap();
        assert_eq!(rt.git_error, None);
        assert_eq!(rt.file_error, None);
        assert_eq!(git_out(&clone, &["status", "--porcelain", "--ignored"]), "");
        assert_eq!(head(&clone), r_head);
        assert!(!clone.join("notes.txt").exists());
        assert!(!clone.join("build.log").exists());
        assert!(!clone.join("local.txt").exists());
        assert_eq!(
            std::fs::read(clone.join(LIB_FILE)).unwrap(),
            std::fs::read(repo.path().join(LIB_FILE)).unwrap()
        );
        assert_eq!(
            snippet_names(&hub, id),
            vec!["A".to_string(), "New".to_string()]
        );
        assert_eq!(hub.config(id).unwrap().last_pull_ok, Some(T0 + 400));
        // The old clone is gone and no temporary entry is left.
        assert_eq!(entries(lib_dir.path()), vec![id.to_string()]);
    }

    #[test]
    fn resync_recovers_from_a_force_push() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
        let old_head = head(&clone);
        let forced = repo.force_push(&[(LIB_FILE, FILE_FORCED)], "rewrite");

        // The update fails; no reclone, no reset.
        run_sync(
            &mut hub,
            id,
            JobKind::Update,
            &env,
            lib_dir.path(),
            T0 + 400,
        );
        assert_git_error(&hub, id);
        assert_eq!(head(&clone), old_head);
        assert_eq!(snippet_names(&hub, id), vec!["A".to_string()]);

        // The failed pull fetched the rewritten branch: the old tip is now a
        // local commit, so the re-sync asks first.
        let (changes, _) = resync_confirmed(&mut hub, id, &env, lib_dir.path(), T0 + 500);
        assert_eq!(
            changes,
            LocalChanges::Changes {
                files: 0,
                commits: 1
            }
        );

        assert_eq!(hub.runtime(id).unwrap().git_error, None);
        assert_eq!(head(&clone), forced);
        assert_eq!(snippet_names(&hub, id), vec!["Forced".to_string()]);
    }

    #[test]
    fn resync_cancelled_confirmation_changes_nothing() {
        let repo = TestRepo::init();
        repo.commit(&[(LIB_FILE, FILE_TEAM), (".gitignore", "*.log\n")], "init");
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
        assert!(hub.rename(id, "Example"));
        switch_on_and_confirm(&mut hub, id);
        // Only the snippet's `text` changes: not a field the loop watches.
        std::fs::write(
            clone.join(LIB_FILE),
            FILE_TEAM.replace("text = \"hello\"", "text = \"hand\""),
        )
        .unwrap();
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        std::fs::write(clone.join("build.log"), "log\n").unwrap();
        std::fs::write(clone.join("local.txt"), "local\n").unwrap();
        git_out(&clone, &["add", "local.txt"]);
        git_out(&clone, &["commit", "-q", "-m", "local commit"]);
        assert_eq!(git_out(&clone, &["check-ignore", "build.log"]), "build.log");

        let state = before(&hub, id, &clone);
        let loops = hub.shared_loops.clone();
        let confirmations = hub.runner_confirmations.clone();
        let runtime = hub.runtime(id).cloned();

        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(
            changes,
            LocalChanges::Changes {
                files: 2,
                commits: 1
            }
        );
        assert_eq!(
            prompt(&hub, id, &decision),
            "2 modified or untracked files and 1 local commit in muxel's copy of \"Example\" will be lost."
        );

        // Cancel: the app never calls `begin`, so no git runs after it.
        assert_eq!(hub.actions(id), FREE);
        assert_eq!(hub.runtime(id).unwrap().busy, None);
        // The read-only check left every file (`.git` included) as it was.
        assert_eq!(before(&hub, id, &clone), state);
        assert!(clone.join("notes.txt").is_file());
        assert!(clone.join("local.txt").is_file());
        assert!(clone.join("build.log").is_file());
        assert_eq!(hub.shared_loops, loops);
        assert_eq!(hub.shared_loops[0].last_run, Some(T0));
        assert_eq!(hub.runner_confirmations, confirmations);
        assert_eq!(hub.runtime(id).cloned(), runtime);
    }

    // ---- Local-changes check before a re-sync ----
    // Conventions of these tests: display name `Example`; `R` tracks
    // `muxel-library.toml`, `a.txt` and `b.txt` and ignores `*.log`.

    fn check_remote() -> TestRepo {
        let repo = TestRepo::init();
        repo.commit(
            &[
                (LIB_FILE, FILE_A),
                ("a.txt", "a\n"),
                ("b.txt", "b\n"),
                (".gitignore", "*.log\n"),
            ],
            "init",
        );
        repo
    }

    /// A clean clone of `repo` made by muxel, library named `Example`.
    fn example_library(
        repo: &TestRepo,
        lib_dir: &Path,
        env: &GitEnv,
    ) -> (LibraryHub, Uuid, PathBuf) {
        let (mut hub, id, clone) = cloned_library(repo, lib_dir, env);
        assert!(hub.rename(id, "Example"));
        (hub, id, clone)
    }

    fn counted(files: usize, commits: usize) -> LocalChanges {
        LocalChanges::Changes { files, commits }
    }

    fn local_commit(clone: &Path, name: &str) {
        std::fs::write(clone.join(name), "local\n").unwrap();
        git_out(clone, &["add", name]);
        git_out(clone, &["commit", "-q", "-m", name]);
    }

    fn run_started(
        hub: &mut LibraryHub,
        decision: CheckDecision,
        env: &GitEnv,
        lib_dir: &Path,
        now: u64,
    ) {
        let CheckDecision::Start(spec) = decision else {
            panic!("expected a re-sync without confirmation, got {decision:?}");
        };
        assert_eq!(hub.runtime(spec.id).unwrap().busy, Some(JobKind::Resync));
        let outcome = run_job(env, lib_dir, &spec);
        hub.finish(outcome, now);
    }

    /// The clone after a re-sync: HEAD of `R`, nothing untracked or ignored.
    fn assert_resync_result(clone: &Path, repo: &TestRepo) {
        assert_eq!(git_out(clone, &["status", "--porcelain", "--ignored"]), "");
        assert_eq!(head(clone), repo.git(&["rev-parse", "HEAD"]));
    }

    const GENERIC_EXAMPLE: &str =
        "Re-sync \"Example\" from its repository? Local changes in muxel's copy may be lost.";

    #[test]
    fn resync_clean_clone_starts_without_confirmation() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
        let r_head = repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");

        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(changes, LocalChanges::None);
        run_started(&mut hub, decision, &env, lib_dir.path(), T0 + 400);

        assert_eq!(hub.runtime(id).unwrap().git_error, None);
        assert_eq!(head(&clone), r_head);
        assert!(snippet_names(&hub, id).contains(&"New".to_string()));
        assert_eq!(hub.actions(id), FREE);
    }

    #[test]
    fn resync_missing_or_empty_clone_starts_without_confirmation() {
        let repo = check_remote();
        let env = GitEnv::for_tests();
        for empty in [false, true] {
            let lib_dir = TmpDir::new();
            let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
            remove_dir_force(&GitEnv::production(), &clone).unwrap();
            if empty {
                std::fs::create_dir_all(&clone).unwrap();
            }
            let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
            assert_eq!(changes, LocalChanges::NoClone, "empty dir: {empty}");
            run_started(&mut hub, decision, &env, lib_dir.path(), T0 + 400);
            assert_eq!(hub.runtime(id).unwrap().git_error, None);
            assert_eq!(head(&clone), repo.git(&["rev-parse", "HEAD"]));
        }
    }

    #[test]
    fn resync_changed_files_ask_with_the_count_and_wait_for_confirmation() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
        std::fs::write(clone.join("a.txt"), "changed\n").unwrap();
        std::fs::remove_file(clone.join("b.txt")).unwrap();
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        std::fs::create_dir_all(clone.join("tmp")).unwrap();
        std::fs::write(clone.join("tmp/x.txt"), "x\n").unwrap();
        std::fs::write(clone.join("tmp/y.txt"), "y\n").unwrap();
        let files = snapshot(&clone);

        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(changes, counted(5, 0));
        assert_eq!(
            prompt(&hub, id, &decision),
            "5 modified or untracked files in muxel's copy of \"Example\" will be lost."
        );
        // Until confirmed: no re-sync in progress, the clone is untouched.
        assert_eq!(hub.actions(id), FREE);
        assert_eq!(hub.runtime(id).unwrap().busy, None);
        assert_eq!(snapshot(&clone), files);

        run_sync(
            &mut hub,
            id,
            JobKind::Resync,
            &env,
            lib_dir.path(),
            T0 + 410,
        );
        assert!(!clone.join("notes.txt").exists());
        assert!(!clone.join("tmp").exists());
        assert_resync_result(&clone, &repo);
    }

    #[test]
    fn resync_one_untracked_file_is_singular() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(changes, counted(1, 0));
        assert_eq!(
            prompt(&hub, id, &decision),
            "1 modified or untracked file in muxel's copy of \"Example\" will be lost."
        );
    }

    #[test]
    fn resync_local_commits_are_counted_in_the_text() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();

        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
        local_commit(&clone, "local.txt");
        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(changes, counted(0, 1));
        assert_eq!(
            prompt(&hub, id, &decision),
            "1 local commit in muxel's copy of \"Example\" will be lost."
        );

        for name in ["u1.txt", "u2.txt", "u3.txt"] {
            std::fs::write(clone.join(name), "u\n").unwrap();
        }
        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 401);
        assert_eq!(changes, counted(3, 1));
        assert_eq!(
            prompt(&hub, id, &decision),
            "3 modified or untracked files and 1 local commit in muxel's copy of \"Example\" will be lost."
        );

        local_commit(&clone, "local2.txt");
        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 402);
        assert_eq!(changes, counted(3, 2));
        assert_eq!(
            prompt(&hub, id, &decision),
            "3 modified or untracked files and 2 local commits in muxel's copy of \"Example\" will be lost."
        );
        assert_eq!(hub.actions(id), FREE);
    }

    #[test]
    fn resync_no_upstream_or_detached_head_asks_with_the_generic_text() {
        let repo = check_remote();
        let env = GitEnv::for_tests();
        for args in [
            ["branch", "--unset-upstream"].as_slice(),
            ["checkout", "-q", "--detach"].as_slice(),
        ] {
            let lib_dir = TmpDir::new();
            let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
            git_out(&clone, args);
            let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
            assert_eq!(changes, LocalChanges::Unknown, "{args:?}");
            assert_eq!(decision, CheckDecision::Confirm(ResyncConfirm::Generic));
            assert_eq!(prompt(&hub, id, &decision), GENERIC_EXAMPLE);
            assert_eq!(hub.runtime(id).unwrap().busy, None);
            assert_eq!(hub.actions(id), FREE);
        }
    }

    #[test]
    fn resync_folder_without_git_asks_with_its_file_count() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
        remove_dir_force(&GitEnv::production(), &clone).unwrap();
        std::fs::create_dir_all(clone.join("docs")).unwrap();
        std::fs::write(clone.join(LIB_FILE), FILE_A).unwrap();
        std::fs::write(clone.join("docs/notes.md"), "notes\n").unwrap();
        let files = snapshot(&clone);

        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(changes, counted(2, 0));
        assert_eq!(
            prompt(&hub, id, &decision),
            "2 modified or untracked files in muxel's copy of \"Example\" will be lost."
        );
        assert!(!clone.join(".git").exists());
        assert_eq!(snapshot(&clone), files);

        run_sync(
            &mut hub,
            id,
            JobKind::Resync,
            &env,
            lib_dir.path(),
            T0 + 410,
        );
        assert!(!clone.join("docs/notes.md").exists());
        assert_resync_result(&clone, &repo);
    }

    #[test]
    fn resync_only_ignored_files_start_without_confirmation() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
        std::fs::write(clone.join("build.log"), "log\n").unwrap();
        assert_eq!(git_out(&clone, &["check-ignore", "build.log"]), "build.log");

        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(changes, LocalChanges::None);
        run_started(&mut hub, decision, &env, lib_dir.path(), T0 + 400);
        assert!(!clone.join("build.log").exists());
        assert_eq!(git_out(&clone, &["status", "--porcelain", "--ignored"]), "");
    }

    /// An unknown result asks with the generic text and keeps the error state;
    /// cancelling then leaves nothing in progress and no file changed.
    fn assert_unknown_then_cancel(
        hub: &mut LibraryHub,
        id: Uuid,
        clone: &Path,
        changes: LocalChanges,
        decision: CheckDecision,
        runtime_before: Option<LibRuntime>,
        files_before: &BTreeMap<PathBuf, Vec<u8>>,
    ) {
        assert_eq!(changes, LocalChanges::Unknown);
        assert_eq!(prompt(hub, id, &decision), GENERIC_EXAMPLE);
        assert_eq!(hub.runtime(id).cloned(), runtime_before);
        assert_eq!(hub.actions(id), FREE);
        assert_eq!(hub.runtime(id).unwrap().busy, None);
        assert_eq!(&snapshot(clone), files_before);
    }

    #[test]
    fn resync_git_missing_from_path_is_unknown() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &GitEnv::for_tests());
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        let no_git = TmpDir::new();
        std::fs::create_dir_all(no_git.path()).unwrap();
        let env = GitEnv {
            path_override: Some(no_git.path().as_os_str().to_owned()),
            ..GitEnv::for_tests()
        };
        let runtime = hub.runtime(id).cloned();
        let files = snapshot(&clone);
        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);
        assert_unknown_then_cancel(&mut hub, id, &clone, changes, decision, runtime, &files);
    }

    #[test]
    fn resync_broken_head_is_unknown_and_keeps_the_error_state() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
        std::fs::write(clone.join(".git/HEAD"), "garbage").unwrap();
        // The update fails on the broken clone: the library is in error.
        run_sync(
            &mut hub,
            id,
            JobKind::Update,
            &env,
            lib_dir.path(),
            T0 + 300,
        );
        let err = assert_git_error(&hub, id);
        let runtime = hub.runtime(id).cloned();
        let files = snapshot(&clone);

        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_unknown_then_cancel(&mut hub, id, &clone, changes, decision, runtime, &files);
        assert_eq!(hub.runtime(id).unwrap().git_error, Some(err));
        assert_eq!(std::fs::read(clone.join(".git/HEAD")).unwrap(), b"garbage");
    }

    #[test]
    fn resync_zero_time_limit_is_unknown() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
        let runtime = hub.runtime(id).cloned();
        let files = snapshot(&clone);
        let spawned = env.spawned.load(Ordering::SeqCst);

        assert_eq!(hub.request_resync(id), ResyncRequest::Check);
        let changes = run_check_with_limit(&env, lib_dir.path(), id, Duration::ZERO);
        let decision = hub.finish_check(id, changes, T0 + 400);
        assert_eq!(env.spawned.load(Ordering::SeqCst), spawned);
        assert_unknown_then_cancel(&mut hub, id, &clone, changes, decision, runtime, &files);
    }

    #[test]
    fn resync_held_check_keeps_the_library_busy_and_is_read_only() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let hold = Mutex::new(Some((entered_tx, release_rx)));
        let env = GitEnv {
            check_hook: Some(Arc::new(move || {
                if let Some((entered, release)) = hold.lock().unwrap().take() {
                    let _ = entered.send(());
                    let _ = release.recv_timeout(Duration::from_secs(60));
                }
            })),
            ..GitEnv::for_tests()
        };
        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
        local_commit(&clone, "local.txt");
        std::fs::write(clone.join("a.txt"), "changed\n").unwrap();
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        delete_remote(repo);

        let files = snapshot(&clone);
        let head_before = head(&clone);
        let refs = git_out(&clone, &["for-each-ref"]);

        assert_eq!(hub.request_resync(id), ResyncRequest::Check);
        let changes = std::thread::scope(|s| {
            let check = s.spawn(|| run_check(&env, lib_dir.path(), id));
            entered_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the check started");
            assert!(!check.is_finished());
            let spawned = env.spawned.load(Ordering::SeqCst);
            assert_busy_and_requests_ignored(&mut hub, id, &env, lib_dir.path(), T0 + 401);
            assert_eq!(env.spawned.load(Ordering::SeqCst), spawned);
            release_tx.send(()).unwrap();
            check.join().unwrap()
        });
        assert_eq!(changes, counted(2, 1));
        let decision = hub.finish_check(id, changes, T0 + 410);
        assert_eq!(
            decision,
            CheckDecision::Confirm(ResyncConfirm::Counted {
                files: 2,
                commits: 1
            })
        );
        assert_eq!(hub.actions(id), FREE);
        assert_eq!(snapshot(&clone), files);
        assert_eq!(head(&clone), head_before);
        assert_eq!(git_out(&clone, &["for-each-ref"]), refs);
        assert!(!clone.join(".git/index.lock").exists());
        assert!(!clone.join(".git/FETCH_HEAD").exists());
    }

    #[test]
    fn resync_check_runs_no_askpass_program() {
        let repo = check_remote();
        let lib_dir = TmpDir::new();
        let marker_dir = TmpDir::new();
        std::fs::create_dir_all(marker_dir.path()).unwrap();
        let marker = marker_dir.path().join("M");
        let script = askpass_script(marker_dir.path(), &marker);
        let mut env = GitEnv::for_tests();
        for var in ["GIT_ASKPASS", "SSH_ASKPASS"] {
            env.extra_env
                .push((var.into(), script.clone().into_os_string()));
        }
        env.extra_env
            .push(("SSH_ASKPASS_REQUIRE".into(), "force".into()));
        let (mut hub, id, clone) = example_library(&repo, lib_dir.path(), &env);
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        let (changes, _) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(changes, counted(1, 0));
        assert!(!marker.exists());
    }

    #[test]
    fn resync_folder_without_git_inside_another_repo_touches_neither() {
        let parent = TestRepo::init();
        parent.commit(&[("p.txt", "p\n")], "init");
        std::fs::write(parent.path().join("dirty.txt"), "dirty\n").unwrap();
        let lib_dir = parent.path().join("libraries");
        let mut hub = LibraryHub::default();
        let id = hub.add("file:///nowhere/r.git", "", "Example").unwrap();
        let dir = lib_dir.join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LIB_FILE), FILE_A).unwrap();
        let status = parent.git(&["status", "--porcelain"]);
        let log = parent.git(&["log", "-1", "--format=%H"]);
        let env = GitEnv::for_tests();

        let (changes, decision) = click_resync(&mut hub, id, &env, &lib_dir, T0 + 400);
        assert_eq!(changes, counted(1, 0));
        assert_eq!(
            prompt(&hub, id, &decision),
            "1 modified or untracked file in muxel's copy of \"Example\" will be lost."
        );
        assert_eq!(parent.git(&["status", "--porcelain"]), status);
        assert_eq!(parent.git(&["log", "-1", "--format=%H"]), log);
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);
        remove_dir_force(&GitEnv::production(), &lib_dir).unwrap();
    }

    #[test]
    fn resync_texts_are_exact() {
        let cases = [
            (
                1,
                0,
                "1 modified or untracked file in muxel's copy of \"Example\" will be lost.",
            ),
            (
                5,
                0,
                "5 modified or untracked files in muxel's copy of \"Example\" will be lost.",
            ),
            (
                0,
                1,
                "1 local commit in muxel's copy of \"Example\" will be lost.",
            ),
            (
                0,
                2,
                "2 local commits in muxel's copy of \"Example\" will be lost.",
            ),
            (
                1,
                1,
                "1 modified or untracked file and 1 local commit in muxel's copy of \"Example\" will be lost.",
            ),
            (
                1,
                3,
                "1 modified or untracked file and 3 local commits in muxel's copy of \"Example\" will be lost.",
            ),
            (
                3,
                1,
                "3 modified or untracked files and 1 local commit in muxel's copy of \"Example\" will be lost.",
            ),
            (
                3,
                2,
                "3 modified or untracked files and 2 local commits in muxel's copy of \"Example\" will be lost.",
            ),
        ];
        for (files, commits, text) in cases {
            assert_eq!(resync_confirm_text("Example", files, commits), text);
            assert_eq!(
                resync_prompt_text("Example", ResyncConfirm::Counted { files, commits }),
                text
            );
        }
        assert_eq!(resync_generic_text("Example"), GENERIC_EXAMPLE);
        assert_eq!(
            resync_prompt_text("Example", ResyncConfirm::Generic),
            GENERIC_EXAMPLE
        );
        // A name with a placeholder is not expanded.
        assert_eq!(
            resync_confirm_text("{files}", 2, 0),
            "2 modified or untracked files in muxel's copy of \"{files}\" will be lost."
        );
    }

    #[test]
    fn resync_remote_gone_keeps_the_clone_and_items() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        let state = before(&hub, id, &clone);
        delete_remote(repo);

        let effects = run_sync(
            &mut hub,
            id,
            JobKind::Resync,
            &env,
            lib_dir.path(),
            T0 + 400,
        );

        assert_eq!(effects, Effects::default());
        assert_git_error(&hub, id);
        assert!(clone.join("notes.txt").is_file());
        assert_eq!(before(&hub, id, &clone), state);
        assert_eq!(entries(lib_dir.path()), vec![id.to_string()]);
        assert_eq!(hub.actions(id), FREE);
    }

    #[test]
    fn resync_orphan_index_lock_is_cleared() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
        let lock = clone.join(".git").join("index.lock");
        std::fs::write(&lock, "").unwrap();
        let r_head = repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");
        // The update has failed on that file.
        run_sync(
            &mut hub,
            id,
            JobKind::Update,
            &env,
            lib_dir.path(),
            T0 + 400,
        );
        assert_git_error(&hub, id);
        assert!(lock.is_file());

        run_sync(
            &mut hub,
            id,
            JobKind::Resync,
            &env,
            lib_dir.path(),
            T0 + 500,
        );

        assert_eq!(hub.runtime(id).unwrap().git_error, None);
        assert!(!lock.exists());
        assert_eq!(git_out(&clone, &["status", "--porcelain", "--ignored"]), "");
        assert_eq!(head(&clone), r_head);
        assert!(snippet_names(&hub, id).contains(&"New".to_string()));
    }

    // The library URL is a server that never answers; the clone came from `R`.
    #[test]
    fn resync_timeout_keeps_the_clone_and_items() {
        let repo = remote(FILE_A);
        let server = TestServer::silent();
        let lib_dir = TmpDir::new();
        std::fs::create_dir_all(lib_dir.path()).unwrap();
        let mut hub = LibraryHub::default();
        let id = hub.add(&server.url("r.git"), "", "").unwrap();
        let clone = lib_dir.path().join(id.to_string());
        git_out(
            lib_dir.path(),
            &["clone", "-q", &file_url(repo.path()), &id.to_string()],
        );
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        // Load the items: an update pulls the clone's own origin (`R`).
        let setup = GitEnv::for_tests();
        run_sync(&mut hub, id, JobKind::Update, &setup, lib_dir.path(), T0);
        assert_eq!(hub.runtime(id).unwrap().git_error, None);
        assert_eq!(snippet_names(&hub, id), vec!["A".to_string()]);
        let state = before(&hub, id, &clone);

        let env = GitEnv::for_tests();
        let limits = JobLimits {
            clone: Duration::from_secs(2),
            pull: Duration::from_secs(2),
        };
        let spec = hub.begin(id, JobKind::Resync, T0 + 400).unwrap();
        let start = Instant::now();
        let outcome = run_job_with_limits(&env, lib_dir.path(), &spec, limits);
        let elapsed = start.elapsed();
        assert!(elapsed <= Duration::from_secs(7), "took {elapsed:?}");
        {
            let mut reaped = env.reaped.as_ref().unwrap().lock().unwrap();
            assert!(!reaped.is_empty());
            for child in reaped.iter_mut() {
                assert!(matches!(child.try_wait(), Ok(Some(_))));
            }
        }
        hub.finish(outcome, T0 + 400);

        assert!(matches!(
            assert_git_error(&hub, id),
            GitFailure::TimedOut { .. }
        ));
        assert!(clone.join("notes.txt").is_file());
        assert_eq!(before(&hub, id, &clone), state);
        assert_eq!(entries(lib_dir.path()), vec![id.to_string()]);
    }

    #[test]
    fn resync_replace_failure_keeps_the_clone_and_items() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &GitEnv::for_tests());
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");
        let state = before(&hub, id, &clone);
        let env = GitEnv {
            rename: failing_rename(&[".r-"]),
            ..GitEnv::for_tests()
        };

        run_sync(
            &mut hub,
            id,
            JobKind::Resync,
            &env,
            lib_dir.path(),
            T0 + 400,
        );

        assert_git_error(&hub, id);
        assert!(clone.join("notes.txt").is_file());
        assert_eq!(before(&hub, id, &clone), state);
        assert_eq!(snippet_names(&hub, id), vec!["A".to_string()]);
        assert_eq!(entries(lib_dir.path()), vec![id.to_string()]);
    }

    #[test]
    fn resync_without_clone_and_remote_fails() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
        remove_dir_force(&env, &clone).unwrap();
        delete_remote(repo);
        let items = hub.items(id).cloned();
        assert!(items.is_some());

        run_sync(
            &mut hub,
            id,
            JobKind::Resync,
            &env,
            lib_dir.path(),
            T0 + 400,
        );

        assert_git_error(&hub, id);
        assert!(!clone.exists());
        assert!(entries(lib_dir.path()).is_empty());
        assert_eq!(hub.items(id).cloned(), items);
    }

    #[test]
    fn resync_double_failure_then_update_clones_again() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &GitEnv::for_tests());
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");
        let items = hub.items(id).cloned();
        let env = GitEnv {
            rename: failing_rename(&[".r-", ".o-"]),
            ..GitEnv::for_tests()
        };

        run_sync(
            &mut hub,
            id,
            JobKind::Resync,
            &env,
            lib_dir.path(),
            T0 + 400,
        );

        assert!(matches!(
            assert_git_error(&hub, id),
            GitFailure::ResyncRestoreFailed { .. }
        ));
        assert_eq!(hub.items(id).cloned(), items);
        assert_eq!(snippet_names(&hub, id), vec!["A".to_string()]);
        assert!(!clone.exists());
        let old_prefix = format!("{id}.o-");
        let left = entries(lib_dir.path());
        assert_eq!(
            left.iter().filter(|n| n.starts_with(&old_prefix)).count(),
            1,
            "{left:?}"
        );

        // A fresh environment without hooks clones the missing clone again.
        let fresh = GitEnv::for_tests();
        run_sync(
            &mut hub,
            id,
            JobKind::Update,
            &fresh,
            lib_dir.path(),
            T0 + 500,
        );

        assert!(clone.is_dir());
        assert_eq!(head(&clone), repo.git(&["rev-parse", "main"]));
        let rt = hub.runtime(id).unwrap();
        assert_eq!(rt.git_error, None);
        assert_eq!(rt.file_error, None);
        assert!(snippet_names(&hub, id).contains(&"New".to_string()));
    }

    // The update runs on a thread against a silent server (limit 3 s).
    #[test]
    fn busy_requests_during_a_hung_update_are_ignored_then_error() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &GitEnv::for_tests());
        let server = TestServer::silent();
        git_out(
            &clone,
            &["remote", "set-url", "origin", &server.url("r.git")],
        );
        let items = hub.items(id).cloned();
        let env = GitEnv::for_tests();
        let limits = JobLimits {
            clone: Duration::from_secs(3),
            pull: Duration::from_secs(3),
        };

        let spec = hub.begin(id, JobKind::Update, T0 + 400).expect("free");
        let outcome = std::thread::scope(|s| {
            let job = s.spawn(|| run_job_with_limits(&env, lib_dir.path(), &spec, limits));
            wait_until(|| env.spawned.load(Ordering::SeqCst) >= 1);
            assert!(!job.is_finished());
            assert_busy_and_requests_ignored(&mut hub, id, &env, lib_dir.path(), T0 + 401);
            job.join().unwrap()
        });

        assert_eq!(env.spawned.load(Ordering::SeqCst), 1);
        assert!(
            matches!(outcome.git, Err(GitFailure::TimedOut { .. })),
            "{:?}",
            outcome.git
        );
        hub.finish(outcome, T0 + 410);
        assert_git_error(&hub, id);
        assert_eq!(hub.actions(id), FREE);
        assert_eq!(hub.request_resync(id), ResyncRequest::Check);
        assert_eq!(hub.items(id).cloned(), items);
        assert!(hub.config(id).is_some());
    }

    #[test]
    fn busy_actions_disabled_during_a_held_resync_then_success() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &GitEnv::for_tests());
        let r_head = repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let env = GitEnv {
            rename: held_rename(entered_tx, release_rx),
            ..GitEnv::for_tests()
        };

        // The clone is clean: the click starts the re-sync straight away.
        let (changes, decision) = click_resync(&mut hub, id, &env, lib_dir.path(), T0 + 400);
        assert_eq!(changes, LocalChanges::None);
        let CheckDecision::Start(spec) = decision else {
            panic!("{decision:?}");
        };
        let spawned_by_check = env.spawned.load(Ordering::SeqCst);
        let outcome = std::thread::scope(|s| {
            let job = s.spawn(|| run_job(&env, lib_dir.path(), &spec));
            entered_rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the re-sync reached its rename");
            assert!(!job.is_finished());
            let spawned = env.spawned.load(Ordering::SeqCst);
            assert_busy_and_requests_ignored(&mut hub, id, &env, lib_dir.path(), T0 + 401);
            assert_eq!(env.spawned.load(Ordering::SeqCst), spawned);
            release_tx.send(()).unwrap();
            job.join().unwrap()
        });

        assert_eq!(env.spawned.load(Ordering::SeqCst), spawned_by_check + 1);
        assert_eq!(outcome.git, Ok(()));
        hub.finish(outcome, T0 + 410);
        assert_eq!(hub.runtime(id).unwrap().git_error, None);
        assert_eq!(hub.actions(id), FREE);
        assert_eq!(head(&clone), r_head);
        assert!(snippet_names(&hub, id).contains(&"New".to_string()));
    }

    #[test]
    fn busy_orphan_index_lock_fails_the_update_and_stays() {
        let repo = remote(FILE_A);
        let lib_dir = TmpDir::new();
        let env = GitEnv::for_tests();
        let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
        let lock = clone.join(".git").join("index.lock");
        std::fs::write(&lock, "").unwrap();
        let old_head = head(&clone);
        repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");

        run_sync(
            &mut hub,
            id,
            JobKind::Update,
            &env,
            lib_dir.path(),
            T0 + 400,
        );

        assert_git_error(&hub, id);
        assert!(lock.is_file());
        assert_eq!(head(&clone), old_head);
        assert_eq!(hub.actions(id), FREE);
    }

    // ---- Safety rules end to end ----

    mod safety {
        use super::{
            FILE_A, FILE_A_NEW, FILE_FORCED, FILE_OLD, LIB_FILE, T0, TmpDir, assert_git_error,
            cloned_library, copy_error_text, delete_library_files, head, persist_roundtrip, remote,
            resync_confirmed, run_sync, snippet_names, switched_off_text,
        };
        use crate::integrations::GitEnv;
        use crate::test_support::{TestRepo, file_url, git_out};
        use muxel_core::library::hub::{Effects, JobKind, LibraryHub, LocalLists};
        use muxel_core::library::resolve::CopyError;
        use muxel_core::library::state::{
            FireMode, RunnerConfirm, RunnerLaunch, TurnOn, TurnOnRequest, confirm_runner,
            find_confirmation, find_shared_loop, request_turn_on, runner_launch, turn_on,
        };
        use muxel_core::library::{
            LibItemKey, LibKind, LoopContent, RunnerContent, SharedLoopState,
        };
        use muxel_core::{
            AgentPreset, Loop, LoopSchedule, PostRunAction, Runner, Settings, Snippet,
        };
        use muxel_store::{save_settings_to, try_load_settings_from};
        use std::collections::HashSet;
        use std::path::{Path, PathBuf};
        use uuid::Uuid;

        const PROJECT: Uuid = Uuid::from_u128(0x50);
        const CLAUDE_ID: Uuid = Uuid::from_u128(0xC1);
        const CODEX_ID: Uuid = Uuid::from_u128(0xC2);
        /// Ten years: any schedule is overdue.
        const FAR: u64 = 10 * 365 * 24 * 3600;
        const LOOP: &str = "Nightly";
        const RUNNER: &str = "Review";

        /// The runner `Review` and the loop `Nightly`, both without `preset`: the
        /// runner uses the toolbar's agent, the loop the one pinned at switch-on.
        fn team_file(loop_prompt: &str, runner_prompt: &str, minutes: u32) -> String {
            format!(
                "[[runners]]\nname = \"{RUNNER}\"\nprompt = \"{runner_prompt}\"\n\n\
                 [[loops]]\nname = \"{LOOP}\"\nprompt = \"{loop_prompt}\"\n\
                 schedule = {{ kind = \"every_minutes\", minutes = {minutes} }}\n"
            )
        }

        fn loop_key(id: Uuid) -> LibItemKey {
            LibItemKey {
                library: id,
                kind: LibKind::Loop,
                name: LOOP.to_string(),
            }
        }

        fn runner_key(id: Uuid) -> LibItemKey {
            LibItemKey {
                library: id,
                kind: LibKind::Runner,
                name: RUNNER.to_string(),
            }
        }

        fn claude_presets() -> Vec<AgentPreset> {
            let mut p = AgentPreset::shell();
            p.name = "Claude".to_string();
            p.id = CLAUDE_ID;
            vec![p]
        }

        fn cfg_file() -> (TmpDir, PathBuf) {
            let dir = TmpDir::new();
            std::fs::create_dir_all(dir.path()).unwrap();
            let path = dir.path().join("config.toml");
            (dir, path)
        }

        /// Switch on `Nightly` in `PROJECT` through the turn-on dialog, picking
        /// the agent `Claude` (the loop has no `preset`).
        fn switch_on(hub: &mut LibraryHub, id: Uuid, now: u64) {
            let key = loop_key(id);
            let loaded = hub.loaded_loop(&key).cloned().expect("loop loaded");
            let result = turn_on(
                &mut hub.shared_loops,
                &key,
                &loaded.content,
                Some(&loaded),
                PROJECT,
                Some(CLAUDE_ID),
                &claude_presets(),
                now,
            );
            assert_eq!(result, TurnOn::On);
        }

        fn confirm(hub: &mut LibraryHub, id: Uuid) {
            let key = runner_key(id);
            let loaded = hub.loaded_runner(&key).cloned().expect("runner loaded");
            let result = confirm_runner(
                &mut hub.runner_confirmations,
                &key,
                &loaded.content,
                Some(&loaded),
                &[],
            );
            assert_eq!(result, RunnerConfirm::OpenDetails);
        }

        fn is_on(hub: &LibraryHub, id: Uuid) -> bool {
            find_shared_loop(&hub.shared_loops, id, LOOP).is_some()
        }

        /// `prepare_shared_fire` of `Nightly` with no run in progress and
        /// `PROJECT` existing: `Some` = it would run.
        fn fires(hub: &mut LibraryHub, id: Uuid, mode: FireMode, now: u64) -> bool {
            hub.prepare_shared_fire(
                &loop_key(id),
                &claude_presets(),
                mode,
                now,
                &HashSet::new(),
                &|p| p == PROJECT,
            )
            .fire()
            .is_some()
        }

        fn scheduled(now: u64) -> FireMode {
            FireMode::Scheduled { now, now_tod: 0 }
        }

        /// What pressing Run on `Review` does.
        fn launch(hub: &LibraryHub, id: Uuid) -> RunnerLaunch {
            runner_launch(
                find_confirmation(&hub.runner_confirmations, id, RUNNER),
                hub.loaded_runner(&runner_key(id)),
                &[],
            )
        }

        fn loaded_loop_prompt(hub: &LibraryHub, id: Uuid) -> String {
            hub.loaded_loop(&loop_key(id))
                .expect("loop loaded")
                .content
                .prompt
                .clone()
        }

        fn lists(settings: &mut Settings) -> LocalLists<'_> {
            LocalLists {
                snippets: &mut settings.snippets,
                runners: &mut settings.runners,
                loops: &mut settings.loops,
            }
        }

        #[test]
        fn force_push_fails_the_update_and_keeps_head_and_items() {
            let repo = remote(FILE_A);
            let lib_dir = TmpDir::new();
            let env = GitEnv::for_tests();
            let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
            let old_head = head(&clone);
            repo.force_push(&[(LIB_FILE, FILE_FORCED)], "rewrite");

            let effects = run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 400,
            );

            assert_git_error(&hub, id);
            assert_eq!(effects, Effects::default());
            assert_eq!(head(&clone), old_head);
            assert_eq!(
                std::fs::read_to_string(clone.join(LIB_FILE)).unwrap(),
                FILE_A
            );
            assert_eq!(snippet_names(&hub, id), vec!["A".to_string()]);
            assert_eq!(hub.runtime(id).unwrap().file_error, None);
        }

        #[test]
        fn conflicting_hand_edit_is_kept_and_the_update_fails() {
            let repo = remote(FILE_A);
            let lib_dir = TmpDir::new();
            let env = GitEnv::for_tests();
            let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
            let edited = "[[snippets]]\nname = \"A\"\ntext = \"local\"\n";
            std::fs::write(clone.join(LIB_FILE), edited).unwrap();
            std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
            let old_head = head(&clone);
            repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");

            run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 400,
            );

            assert_git_error(&hub, id);
            assert_eq!(
                std::fs::read(clone.join(LIB_FILE)).unwrap(),
                edited.as_bytes()
            );
            assert_eq!(std::fs::read(clone.join("notes.txt")).unwrap(), b"mine\n");
            assert_eq!(head(&clone), old_head);
            let a = &hub.items(id).unwrap().snippets;
            assert_eq!(a.len(), 1);
            assert_eq!((a[0].name.as_str(), a[0].text.as_str()), ("A", "local"));
        }

        #[test]
        fn dirty_tracked_file_is_kept_by_a_successful_update() {
            let repo = remote(FILE_A);
            let lib_dir = TmpDir::new();
            let env = GitEnv::for_tests();
            let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
            std::fs::write(clone.join("README.md"), "edited by hand\n").unwrap();
            let r_head = repo.commit(&[(LIB_FILE, FILE_A_NEW)], "add New");

            run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 400,
            );

            assert_eq!(hub.runtime(id).unwrap().git_error, None);
            assert_eq!(head(&clone), r_head);
            assert_eq!(
                std::fs::read_to_string(clone.join(LIB_FILE)).unwrap(),
                FILE_A_NEW
            );
            assert_eq!(
                std::fs::read(clone.join("README.md")).unwrap(),
                b"edited by hand\n"
            );
            assert_eq!(
                snippet_names(&hub, id),
                vec!["A".to_string(), "New".to_string()]
            );
        }

        const FILE_RUNNERS: &str = "[[runners]]\nname = \"Review\"\npreset = \"Claude\"\n\
            auto_mode_presses = 3\nprompt = \"P {{input}}\"\n\n\
            [[runners]]\nname = \"Plain\"\nprompt = \"P {{input}}\"\n";

        fn private_runner() -> Runner {
            Runner {
                id: Uuid::new_v4(),
                name: "Mine".to_string(),
                preset_id: None,
                auto_mode_presses: 0,
                prompt: "mine".to_string(),
            }
        }

        fn copy_runner(hub: &LibraryHub, id: Uuid, name: &str, settings: &mut Settings) -> Uuid {
            let key = LibItemKey {
                library: id,
                kind: LibKind::Runner,
                name: name.to_string(),
            };
            let idx = hub
                .make_local_copy(&key, &claude_presets(), None, None, T0, lists(settings))
                .expect("local copy");
            settings.runners[idx].id
        }

        #[test]
        fn runner_copy_resolves_the_preset_and_is_saved() {
            let repo = remote(FILE_RUNNERS);
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (hub, id, _clone) = cloned_library(&repo, lib_dir.path(), &env);
            let mut settings = Settings {
                runners: vec![private_runner()],
                ..Settings::default()
            };

            let review = copy_runner(&hub, id, "Review", &mut settings);
            let plain = copy_runner(&hub, id, "Plain", &mut settings);
            let (_reloaded, loaded) = persist_roundtrip(&hub, &mut settings, &cfg);

            assert_eq!(loaded.runners.len(), 3);
            let ids: HashSet<Uuid> = loaded.runners.iter().map(|r| r.id).collect();
            assert_eq!(ids.len(), 3);
            let review = loaded.runners.iter().find(|r| r.id == review).unwrap();
            assert_eq!(review.name, "Review");
            assert_eq!(review.preset_id, Some(CLAUDE_ID));
            assert_eq!(review.auto_mode_presses, 3);
            assert_eq!(review.prompt, "P {{input}}");
            let plain = loaded.runners.iter().find(|r| r.id == plain).unwrap();
            assert_eq!(plain.name, "Plain");
            assert_eq!(plain.preset_id, None);
            assert_eq!(plain.prompt, "P {{input}}");
        }

        #[test]
        fn copy_is_independent_of_updates_resyncs_and_its_deletion() {
            let repo = remote(FILE_RUNNERS);
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
            let mut settings = Settings::default();
            let copy = copy_runner(&hub, id, "Review", &mut settings);
            let copy_prompt = |s: &Settings| -> String {
                s.runners
                    .iter()
                    .find(|r| r.id == copy)
                    .unwrap()
                    .prompt
                    .clone()
            };
            let review_prompt = |hub: &LibraryHub| -> String {
                hub.loaded_runner(&runner_key(id))
                    .unwrap()
                    .content
                    .prompt
                    .clone()
            };

            let changed = FILE_RUNNERS.replacen("P {{input}}", "Q {{input}}", 1);
            assert_ne!(changed, FILE_RUNNERS);
            repo.commit(&[(LIB_FILE, &changed)], "change Review");
            run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 400,
            );
            assert_eq!(review_prompt(&hub), "Q {{input}}");
            let (_r, loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert_eq!(copy_prompt(&settings), "P {{input}}");
            assert_eq!(copy_prompt(&loaded), "P {{input}}");

            resync_confirmed(&mut hub, id, &env, lib_dir.path(), T0 + 500);
            assert_eq!(hub.runtime(id).unwrap().git_error, None);
            let (_r, loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert_eq!(copy_prompt(&settings), "P {{input}}");
            assert_eq!(copy_prompt(&loaded), "P {{input}}");

            settings.runners.retain(|r| r.id != copy);
            let (_r, loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert!(loaded.runners.iter().all(|r| r.id != copy));
            assert_eq!(git_out(&clone, &["status", "--porcelain"]), "");
            assert_eq!(
                std::fs::read_to_string(clone.join(LIB_FILE)).unwrap(),
                changed
            );
            assert_eq!(review_prompt(&hub), "Q {{input}}");
        }

        const FILE_LOOP: &str = "[[loops]]\nname = \"Nightly\"\nprompt = \"p\"\n\
            preset = \"Claude\"\nauto_mode_presses = 2\npost_run = \"exit\"\n\
            schedule = { kind = \"every_minutes\", minutes = 5 }\n";

        // `preset = "Claude"` with `Codex` in the toolbar: the copy gets `Claude`.
        #[test]
        fn loop_copy_is_off_in_the_active_project_and_needs_one() {
            let mut presets = claude_presets();
            let mut codex = AgentPreset::shell();
            codex.name = "Codex".to_string();
            codex.id = CODEX_ID;
            presets.push(codex);
            let repo = remote(FILE_LOOP);
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (hub, id, _clone) = cloned_library(&repo, lib_dir.path(), &env);
            let mut settings = Settings::default();
            let key = loop_key(id);

            // No active project: nothing is created, `add_loop`'s error.
            let before = format!(
                "{:?}",
                (&settings.snippets, &settings.runners, &settings.loops)
            );
            let err = hub
                .make_local_copy(
                    &key,
                    &presets,
                    Some(CODEX_ID),
                    None,
                    T0 + 77,
                    lists(&mut settings),
                )
                .unwrap_err();
            assert_eq!(err, CopyError::NoProject);
            assert_eq!(
                format!(
                    "{:?}",
                    (&settings.snippets, &settings.runners, &settings.loops)
                ),
                before
            );
            assert_eq!(
                copy_error_text(&err),
                Some((
                    "Can't add a loop".to_string(),
                    "Open a project first — a loop runs in a specific project.".to_string()
                ))
            );
            let (_r, loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert_eq!(loaded.loops.len(), settings.loops.len());

            // Active project `PROJECT`, clock `T0 + 77`.
            let n = settings.loops.len();
            let idx = hub
                .make_local_copy(
                    &key,
                    &presets,
                    Some(CODEX_ID),
                    Some(PROJECT),
                    T0 + 77,
                    lists(&mut settings),
                )
                .expect("loop copy");
            let copy_id = settings.loops[idx].id;
            let (_r, loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert_eq!(loaded.loops.len(), n + 1);
            let copy: &Loop = loaded.loops.iter().find(|l| l.id == copy_id).unwrap();
            assert_eq!(copy.name, LOOP);
            assert!(!copy.enabled);
            assert_eq!(copy.project_id, PROJECT);
            assert_eq!(copy.last_run, Some(T0 + 77));
            assert_eq!(copy.prompt, "p");
            assert_eq!(copy.auto_mode_presses, 2);
            assert_eq!(copy.post_run, PostRunAction::Exit);
            assert_eq!(copy.schedule, LoopSchedule::EveryMinutes { minutes: 5 });
            assert_eq!(copy.preset_id, Some(CLAUDE_ID));
        }

        /// Every private item, field by field (`Debug` prints every field).
        fn private_items(s: &Settings) -> String {
            format!("{:#?}\n{:#?}\n{:#?}", s.snippets, s.runners, s.loops)
        }

        /// The private lists, in memory and in the saved
        /// `config.toml`, are those of `baseline`.
        fn assert_private_untouched(
            step: &str,
            hub: &LibraryHub,
            settings: &mut Settings,
            cfg: &Path,
            baseline: &str,
        ) {
            assert_eq!(private_items(settings), baseline, "in memory after {step}");
            let (_r, loaded) = persist_roundtrip(hub, settings, cfg);
            assert_eq!(private_items(&loaded), baseline, "config.toml after {step}");
        }

        #[test]
        fn private_items_and_copies_survive_every_library_operation() {
            let repo = remote(FILE_A);
            let repo2 = remote(FILE_OLD);
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, _clone) = cloned_library(&repo, lib_dir.path(), &env);

            let mut settings = Settings {
                snippets: vec![
                    Snippet {
                        id: Uuid::new_v4(),
                        name: "S1".to_string(),
                        text: "one".to_string(),
                        submit: true,
                    },
                    Snippet {
                        id: Uuid::new_v4(),
                        name: "S2".to_string(),
                        text: "two".to_string(),
                        submit: false,
                    },
                ],
                runners: vec![private_runner(), {
                    let mut r = private_runner();
                    r.name = "Mine 2".to_string();
                    r.preset_id = Some(CLAUDE_ID);
                    r.auto_mode_presses = 2;
                    r
                }],
                loops: vec![Loop::new("L1", PROJECT), {
                    let mut l = Loop::new("L2", PROJECT);
                    l.prompt = "two".to_string();
                    l.enabled = false;
                    l.last_run = Some(T0);
                    l
                }],
                ..Settings::default()
            };
            let key = LibItemKey {
                library: id,
                kind: LibKind::Snippet,
                name: "A".to_string(),
            };
            hub.make_local_copy(&key, &[], None, None, T0, lists(&mut settings))
                .expect("snippet copy");
            assert_eq!(settings.snippets.len(), 3);
            let copy = &settings.snippets[2];
            assert_eq!((copy.name.as_str(), copy.text.as_str()), ("A", "one"));
            let baseline = private_items(&settings);
            assert_private_untouched("the copy", &hub, &mut settings, &cfg, &baseline);

            let id2 = hub.add(&file_url(repo2.path()), "", "").unwrap();
            run_sync(
                &mut hub,
                id2,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 10,
            );
            assert_eq!(snippet_names(&hub, id2), vec!["Old".to_string()]);
            assert_private_untouched("add R2", &hub, &mut settings, &cfg, &baseline);

            // A commit in `R` changes and deletes the copied snippet.
            repo.commit(&[(LIB_FILE, FILE_OLD)], "drop A");
            run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 400,
            );
            assert_eq!(snippet_names(&hub, id), vec!["Old".to_string()]);
            assert_private_untouched("update", &hub, &mut settings, &cfg, &baseline);

            // A force-push in `R` fails the next update.
            repo.force_push(&[(LIB_FILE, FILE_FORCED)], "rewrite");
            run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 800,
            );
            assert_git_error(&hub, id);
            assert_private_untouched("failed update", &hub, &mut settings, &cfg, &baseline);

            resync_confirmed(&mut hub, id, &env, lib_dir.path(), T0 + 900);
            assert_eq!(hub.runtime(id).unwrap().git_error, None);
            assert_eq!(snippet_names(&hub, id), vec!["Forced".to_string()]);
            assert_private_untouched("re-sync", &hub, &mut settings, &cfg, &baseline);

            assert!(hub.rename(id2, "Second"));
            assert_private_untouched("rename", &hub, &mut settings, &cfg, &baseline);

            hub.remove(id).expect("remove R");
            assert_eq!(delete_library_files(&env, lib_dir.path(), id), Ok(()));
            assert_private_untouched("remove R", &hub, &mut settings, &cfg, &baseline);

            hub.remove(id2).expect("remove R2");
            assert_eq!(delete_library_files(&env, lib_dir.path(), id2), Ok(()));
            assert_private_untouched("remove R2", &hub, &mut settings, &cfg, &baseline);
            assert!(hub.configs.is_empty());
        }

        #[test]
        fn hand_edited_loop_prompt_switches_the_loop_off() {
            let repo = remote(&team_file("go", "go", 5));
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
            switch_on(&mut hub, id, T0);
            let mut settings = Settings::default();

            // Unchanged content: an update keeps it on.
            let effects = run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0 + 30);
            assert_eq!(effects, Effects::default());
            assert!(is_on(&hub, id));

            let edited = team_file("hand", "go", 5);
            std::fs::write(clone.join(LIB_FILE), &edited).unwrap();
            let old_head = head(&clone);
            let effects = run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0 + 60);

            assert_eq!(hub.runtime(id).unwrap().git_error, None);
            assert_eq!(head(&clone), old_head);
            assert_eq!(
                std::fs::read_to_string(clone.join(LIB_FILE)).unwrap(),
                edited
            );
            assert_eq!(loaded_loop_prompt(&hub, id), "hand");
            assert_eq!(effects.switched_off, vec![LOOP.to_string()]);
            assert_eq!(
                switched_off_text(&effects.switched_off[0]),
                "Shared loop \"Nightly\" was switched off because it changed in its library"
            );
            assert!(!is_on(&hub, id));
            assert!(!fires(&mut hub, id, scheduled(T0 + FAR), T0 + FAR));
            assert!(!fires(&mut hub, id, FireMode::Manual, T0 + FAR));
            let (reloaded, _loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert!(reloaded.shared_loops.is_empty());
        }

        #[test]
        fn hand_edited_runner_prompt_needs_confirmation_again() {
            let repo = remote(&team_file("go", "go", 5));
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
            assert!(matches!(launch(&hub, id), RunnerLaunch::NeedsConfirm(_)));
            confirm(&mut hub, id);
            assert!(matches!(launch(&hub, id), RunnerLaunch::Proceed { .. }));
            let mut settings = Settings::default();

            std::fs::write(clone.join(LIB_FILE), team_file("go", "hand", 5)).unwrap();
            let old_head = head(&clone);
            run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0 + 60);

            assert_eq!(hub.runtime(id).unwrap().git_error, None);
            assert_eq!(head(&clone), old_head);
            let expected = RunnerContent {
                prompt: "hand".to_string(),
                preset: None,
                auto_mode_presses: 0,
            };
            assert_eq!(
                launch(&hub, id),
                RunnerLaunch::NeedsConfirm(expected.clone())
            );

            // Also after a restart: the saved confirmation is the old one.
            let (mut reloaded, _loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert_eq!(reloaded.runner_confirmations.len(), 1);
            run_sync(
                &mut reloaded,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 120,
            );
            assert_eq!(launch(&reloaded, id), RunnerLaunch::NeedsConfirm(expected));
        }

        #[test]
        fn confirmed_runner_round_trip_leaves_the_clone_clean() {
            let repo = remote(&team_file("go", "review this", 5));
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, clone) = cloned_library(&repo, lib_dir.path(), &env);
            confirm(&mut hub, id);
            let confirmed = hub.runner_confirmations.clone();
            assert_eq!(confirmed.len(), 1);
            assert_eq!(confirmed[0].confirmed.prompt, "review this");
            let mut settings = Settings::default();

            let (reloaded, _loaded) = persist_roundtrip(&hub, &mut settings, &cfg);

            assert_eq!(reloaded.runner_confirmations, confirmed);
            let saved = std::fs::read_to_string(&cfg).unwrap();
            assert!(saved.contains("shared_runner_confirmations"), "{saved}");
            assert!(saved.contains("review this"), "{saved}");
            assert_eq!(git_out(&clone, &["status", "--porcelain"]), "");
        }

        #[test]
        fn startup_with_changed_approved_content_never_fires() {
            let repo = remote(&team_file("new", "go", 5));
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut first, id, _clone) = cloned_library(&repo, lib_dir.path(), &env);
            first.shared_loops.push(SharedLoopState {
                library: id,
                name: LOOP.to_string(),
                run_id: Uuid::new_v4(),
                project_id: PROJECT,
                last_run: Some(0),
                approved: LoopContent {
                    prompt: "old".to_string(),
                    preset: None,
                    auto_mode_presses: 0,
                    schedule: LoopSchedule::EveryMinutes { minutes: 5 },
                    post_run: PostRunAction::Leave,
                },
                pinned_preset_id: Some(CLAUDE_ID),
            });
            let mut saved = Settings::default();
            first.write_into(&mut saved);
            save_settings_to(&cfg, &saved).unwrap();

            // Startup: config.toml → hub, then the library is loaded.
            let mut settings = try_load_settings_from(&cfg).unwrap().unwrap();
            let mut hub = LibraryHub::take_from(&mut settings);
            let persisted = hub.shared_loops.clone();
            assert_eq!(persisted.len(), 1);
            assert_eq!(persisted[0].approved.prompt, "old");
            for i in 0..10 {
                let now = T0 + FAR + i * 600;
                assert!(!fires(&mut hub, id, scheduled(now), now), "tick {i}");
            }
            let effects = run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + FAR,
            );
            assert_eq!(loaded_loop_prompt(&hub, id), "new");
            assert_eq!(effects.switched_off, vec![LOOP.to_string()]);
            for i in 0..10 {
                let now = T0 + FAR + i * 600;
                assert!(!fires(&mut hub, id, scheduled(now), now), "tick {i}");
            }
            assert!(!is_on(&hub, id));
            let (reloaded, _loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert!(reloaded.shared_loops.is_empty());

            // The fire decision alone (before reconciliation): on, approved
            // "old", loaded "new" → no run, in both modes, nothing changes.
            hub.shared_loops = persisted.clone();
            assert!(!fires(&mut hub, id, scheduled(T0 + FAR), T0 + FAR));
            assert!(!fires(&mut hub, id, FireMode::Manual, T0 + FAR));
            assert_eq!(hub.shared_loops, persisted);

            // Contrast: with "new" approved it does run, built from "new".
            hub.shared_loops[0].approved.prompt = "new".to_string();
            let fire = hub
                .prepare_shared_fire(
                    &loop_key(id),
                    &claude_presets(),
                    scheduled(T0 + FAR),
                    T0 + FAR,
                    &HashSet::new(),
                    &|p| p == PROJECT,
                )
                .fire()
                .expect("fires");
            assert_eq!(fire.content.prompt, "new");
            assert!(fires(&mut hub, id, FireMode::Manual, T0 + FAR + 1));
        }

        /// A library on `team_file("go", "go", 5)` with `Nightly` on and
        /// `Review` confirmed.
        fn armed_library(
            repo: &TestRepo,
            lib_dir: &Path,
            env: &GitEnv,
        ) -> (LibraryHub, Uuid, PathBuf) {
            let (mut hub, id, clone) = cloned_library(repo, lib_dir, env);
            switch_on(&mut hub, id, T0);
            confirm(&mut hub, id);
            assert!(is_on(&hub, id));
            assert!(matches!(launch(&hub, id), RunnerLaunch::Proceed { .. }));
            (hub, id, clone)
        }

        #[test]
        fn resync_undoing_hand_edits_keeps_approvals() {
            let repo = remote(&team_file("go", "go", 5));
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, clone) = armed_library(&repo, lib_dir.path(), &env);
            std::fs::write(clone.join(LIB_FILE), team_file("hand", "hand", 5)).unwrap();

            let (_, effects) = resync_confirmed(&mut hub, id, &env, lib_dir.path(), T0 + 60);

            assert_eq!(hub.runtime(id).unwrap().git_error, None);
            assert_eq!(effects, Effects::default());
            assert_eq!(loaded_loop_prompt(&hub, id), "go");
            assert!(is_on(&hub, id));
            assert!(matches!(launch(&hub, id), RunnerLaunch::Proceed { .. }));
            let (reloaded, _loaded) = persist_roundtrip(&hub, &mut Settings::default(), &cfg);
            assert_eq!(reloaded.shared_loops.len(), 1);
            assert_eq!(reloaded.runner_confirmations.len(), 1);
        }

        #[test]
        fn resync_with_changed_prompts_switches_off_and_needs_confirmation() {
            let repo = remote(&team_file("go", "go", 5));
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, _clone) = armed_library(&repo, lib_dir.path(), &env);
            repo.commit(&[(LIB_FILE, &team_file("new", "new", 5))], "change prompts");

            let (_, effects) = resync_confirmed(&mut hub, id, &env, lib_dir.path(), T0 + 60);

            assert_eq!(hub.runtime(id).unwrap().git_error, None);
            assert_eq!(effects.switched_off, vec![LOOP.to_string()]);
            assert!(!is_on(&hub, id));
            assert!(!fires(&mut hub, id, scheduled(T0 + FAR), T0 + FAR));
            assert_eq!(
                launch(&hub, id),
                RunnerLaunch::NeedsConfirm(RunnerContent {
                    prompt: "new".to_string(),
                    preset: None,
                    auto_mode_presses: 0,
                })
            );
            let (reloaded, _loaded) = persist_roundtrip(&hub, &mut Settings::default(), &cfg);
            assert!(reloaded.shared_loops.is_empty());
        }

        #[test]
        fn a_control_char_added_by_a_pull_changes_nothing() {
            let repo = remote(&team_file("go", "go", 5));
            let lib_dir = TmpDir::new();
            let env = GitEnv::for_tests();
            let (mut hub, id, clone) = armed_library(&repo, lib_dir.path(), &env);
            let r_head = repo.commit(
                &[(LIB_FILE, &team_file("g\\u0007o", "g\\u0007o", 5))],
                "bell",
            );

            let effects = run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0 + 60);

            assert_eq!(head(&clone), r_head);
            assert!(
                std::fs::read_to_string(clone.join(LIB_FILE))
                    .unwrap()
                    .contains("g\\u0007o")
            );
            assert_eq!(effects, Effects::default());
            let state = find_shared_loop(&hub.shared_loops, id, LOOP).expect("still on");
            assert_eq!(state.last_run, Some(T0));
            let go = RunnerContent {
                prompt: "go".to_string(),
                preset: None,
                auto_mode_presses: 0,
            };
            assert_eq!(
                launch(&hub, id),
                RunnerLaunch::Proceed {
                    content: go.clone(),
                    preset_id: None
                }
            );
            let loaded_runner = hub.loaded_runner(&runner_key(id)).unwrap();
            assert_eq!(loaded_runner.content, go);
            match request_turn_on(hub.loaded_loop(&loop_key(id)), &[]) {
                TurnOnRequest::NeedsProjectAndConfirm(c) => assert_eq!(c.prompt, "go"),
                other => panic!("{other:?}"),
            }
            let mut settings = Settings::default();
            let r = hub
                .make_local_copy(&runner_key(id), &[], None, None, T0, lists(&mut settings))
                .unwrap();
            assert_eq!(settings.runners[r].prompt, "go");
            let l = hub
                .make_local_copy(
                    &loop_key(id),
                    &[],
                    None,
                    Some(PROJECT),
                    T0,
                    lists(&mut settings),
                )
                .unwrap();
            assert_eq!(settings.loops[l].prompt, "go");
        }

        #[test]
        fn deleted_loop_loses_its_state_and_comes_back_off() {
            let repo = remote(&team_file("go", "go", 5));
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, _clone) = armed_library(&repo, lib_dir.path(), &env);
            let mut settings = Settings::default();

            let only_runner = "[[runners]]\nname = \"Review\"\nprompt = \"go\"\n";
            repo.commit(&[(LIB_FILE, only_runner)], "delete Nightly");
            let effects = run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0 + 60);
            assert!(hub.loaded_loop(&loop_key(id)).is_none());
            assert_eq!(effects, Effects::default());
            let (reloaded, _loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert!(reloaded.shared_loops.is_empty());

            repo.commit(&[(LIB_FILE, &team_file("go", "go", 5))], "restore Nightly");
            let effects = run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 120,
            );
            assert_eq!(effects, Effects::default());
            assert_eq!(loaded_loop_prompt(&hub, id), "go");
            assert!(!is_on(&hub, id));
            assert!(!fires(&mut hub, id, FireMode::Manual, T0 + FAR));
            let (reloaded, _loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert!(reloaded.shared_loops.is_empty());
        }

        #[test]
        fn discarded_loop_counts_as_absent_without_event() {
            let repo = remote(&team_file("go", "go", 5));
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, _clone) = armed_library(&repo, lib_dir.path(), &env);
            let mut settings = Settings::default();

            repo.commit(&[(LIB_FILE, &team_file("go", "go", 0))], "minutes = 0");
            let effects = run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0 + 60);
            assert!(hub.loaded_loop(&loop_key(id)).is_none());
            assert_eq!(hub.items(id).unwrap().discarded, 1);
            assert_eq!(effects.switched_off, Vec::<String>::new());
            assert!(!is_on(&hub, id));
            let (reloaded, _loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert!(reloaded.shared_loops.is_empty());

            repo.commit(&[(LIB_FILE, &team_file("go", "go", 5))], "minutes = 5");
            let effects = run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 120,
            );
            assert_eq!(effects, Effects::default());
            assert_eq!(loaded_loop_prompt(&hub, id), "go");
            assert!(!is_on(&hub, id));
        }

        #[test]
        fn same_name_loop_inserted_first_is_loaded_off() {
            let repo = remote(&team_file("go", "go", 5));
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, _clone) = armed_library(&repo, lib_dir.path(), &env);
            let mut settings = Settings::default();

            let inserted = format!(
                "[[loops]]\nname = \"{LOOP}\"\nprompt = \"first\"\n\n{}",
                team_file("go", "go", 5)
            );
            repo.commit(&[(LIB_FILE, &inserted)], "insert Nightly first");
            run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0 + 60);

            assert_eq!(loaded_loop_prompt(&hub, id), "first");
            assert_eq!(hub.items(id).unwrap().loops.len(), 1);
            assert!(!is_on(&hub, id));
            assert!(!fires(&mut hub, id, FireMode::Manual, T0 + FAR));
            let (reloaded, _loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert!(reloaded.shared_loops.is_empty());
        }

        #[test]
        fn invalid_file_stops_firing_but_keeps_the_loop_on() {
            let repo = remote(&team_file("go", "go", 5));
            let lib_dir = TmpDir::new();
            let (_cfg_dir, cfg) = cfg_file();
            let env = GitEnv::for_tests();
            let (mut hub, id, _clone) = armed_library(&repo, lib_dir.path(), &env);
            let mut settings = Settings::default();
            let state = find_shared_loop(&hub.shared_loops, id, LOOP)
                .unwrap()
                .clone();

            repo.commit(&[(LIB_FILE, "[[loops]\nname = \n")], "break the file");
            let effects = run_sync(&mut hub, id, JobKind::Update, &env, lib_dir.path(), T0 + 60);
            assert_eq!(hub.runtime(id).unwrap().git_error, None);
            assert!(hub.runtime(id).unwrap().file_error.is_some());
            assert_eq!(effects, Effects::default());
            assert!(!fires(&mut hub, id, scheduled(T0 + FAR), T0 + FAR));
            assert!(!fires(&mut hub, id, FireMode::Manual, T0 + FAR));
            assert_eq!(hub.shared_loops, vec![state.clone()]);
            let (reloaded, _loaded) = persist_roundtrip(&hub, &mut settings, &cfg);
            assert_eq!(reloaded.shared_loops, vec![state.clone()]);

            repo.commit(&[(LIB_FILE, &team_file("go", "go", 5))], "restore the file");
            let effects = run_sync(
                &mut hub,
                id,
                JobKind::Update,
                &env,
                lib_dir.path(),
                T0 + 120,
            );
            assert_eq!(hub.runtime(id).unwrap().file_error, None);
            assert_eq!(effects, Effects::default());
            assert_eq!(hub.shared_loops, vec![state.clone()]);
            assert_eq!(hub.shared_loops[0].project_id, PROJECT);
            assert!(fires(&mut hub, id, scheduled(T0 + FAR), T0 + FAR));
        }
    }
}

/// The Settings → Libraries row texts.
#[cfg(test)]
mod row_tests {
    use super::{LibraryRowStatus, branch_text, busy_text, library_row_status, pull_time_text};
    use chrono::{FixedOffset, Utc};
    use muxel_core::library::hub::{LibActions, LibRuntime};
    use muxel_core::library::{
        FileError, GitFailure, IssueKind, ItemIssue, LibKind, LibSnippet, LibWarning,
        LibraryConfig, ParsedLibrary,
    };
    use uuid::Uuid;

    // 2023-11-14 22:13:20 UTC.
    const TS: u64 = 1_700_000_000;

    fn config(branch: &str, last_pull_ok: Option<u64>) -> LibraryConfig {
        LibraryConfig {
            id: Uuid::from_u128(1),
            url: "file:///r".to_string(),
            branch: branch.to_string(),
            name: String::new(),
            last_pull_ok,
        }
    }

    const FREE: LibActions = LibActions {
        busy: false,
        pull_now: true,
        resync: true,
        remove: true,
    };
    const BUSY: LibActions = LibActions {
        busy: true,
        pull_now: false,
        resync: false,
        remove: false,
    };

    fn snippet(name: &str) -> LibSnippet {
        LibSnippet {
            name: name.to_string(),
            text: "x".to_string(),
            submit: false,
        }
    }

    #[test]
    fn branch_text_default_when_empty() {
        assert_eq!(branch_text(""), "default");
        assert_eq!(branch_text("team"), "team");
    }

    #[test]
    fn pull_time_in_the_given_time_zone() {
        assert_eq!(pull_time_text(Some(TS), &Utc), "2023-11-14 22:13");
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(pull_time_text(Some(TS), &plus2), "2023-11-15 00:13");
        assert_eq!(pull_time_text(None, &Utc), "");
    }

    #[test]
    fn busy_row_shows_the_busy_text() {
        assert_eq!(busy_text(), "Git operation in progress…");
        let c = config("", None);
        let busy = library_row_status(&c, None, BUSY, &Utc);
        assert_eq!(busy.busy.as_deref(), Some("Git operation in progress…"));
        let free = library_row_status(&c, None, FREE, &Utc);
        assert_eq!(free.busy, None);
    }

    #[test]
    fn correct_library_row() {
        let c = config("team", Some(TS));
        let rt = LibRuntime {
            items: Some(ParsedLibrary {
                snippets: vec![snippet("a"), snippet("b")],
                ..ParsedLibrary::default()
            }),
            ..LibRuntime::default()
        };
        let row = library_row_status(&c, Some(&rt), FREE, &Utc);
        assert_eq!(
            row,
            LibraryRowStatus {
                busy: None,
                last_pull: "2023-11-14 22:13".to_string(),
                errors: Vec::new(),
                counts: Some("Snippets: 2 · Runners: 0 · Loops: 0".to_string()),
                discarded: None,
                warnings: None,
            }
        );
    }

    #[test]
    fn library_in_error_row() {
        let c = config("", None);
        let rt = LibRuntime {
            items: Some(ParsedLibrary::default()),
            file_error: Some(FileError::Missing),
            git_error: Some(GitFailure::Failed {
                detail: "repository not found".to_string(),
            }),
            ..LibRuntime::default()
        };
        let row = library_row_status(&c, Some(&rt), FREE, &Utc);
        assert_eq!(row.last_pull, "");
        assert_eq!(
            row.errors,
            vec![
                "git failed: repository not found".to_string(),
                "muxel-library.toml was not found in the library repository.".to_string(),
            ]
        );
        assert_eq!(
            row.counts.as_deref(),
            Some("Snippets: 0 · Runners: 0 · Loops: 0")
        );

        let unread = library_row_status(&c, None, FREE, &Utc);
        assert!(unread.errors.is_empty());
        assert_eq!(unread.counts, None);
    }

    #[test]
    fn discarded_and_warnings_lines() {
        let c = config("", Some(TS));
        let rt = LibRuntime {
            items: Some(ParsedLibrary {
                discarded: 2,
                first_discard: Some(ItemIssue {
                    table: LibKind::Runner,
                    position: 1,
                    name: Some("R".to_string()),
                    problem: IssueKind::MissingName,
                }),
                warnings: 3,
                first_warning: Some(LibWarning::UnknownTable("macros".to_string())),
                ..ParsedLibrary::default()
            }),
            ..LibRuntime::default()
        };
        let row = library_row_status(&c, Some(&rt), FREE, &Utc);
        assert_eq!(
            row.discarded.as_deref(),
            Some("Discarded items: 2 (runners #1 \"R\" discarded: it has no name)")
        );
        assert_eq!(
            row.warnings.as_deref(),
            Some("Warnings: 3 (Unknown table \"macros\" ignored)")
        );

        let clean = LibRuntime {
            items: Some(ParsedLibrary::default()),
            ..LibRuntime::default()
        };
        let row = library_row_status(&c, Some(&clean), FREE, &Utc);
        assert_eq!(row.discarded, None);
        assert_eq!(row.warnings, None);
    }
}
