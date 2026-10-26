//! muxel maintaining a project's `.muxel/` files on its own: deciding when each
//! one is due, and the background pass for shared memory (`.muxel/MEMORY.md`).
//! The accomplishment log's pass lives with the rest of it in `accomplishments_ui`
//! and shares [`plan_log_check`].
//!
//! Agents add to these files directly, so muxel can't stamp, order, or prune what
//! they write unless it looks. Every [`BACKGROUND_CHECK`] each enabled project is
//! maintained if it hasn't been today (app start, every day rollover), and a local
//! file whose stamp moved is maintained right then — so a new memory section is
//! dated within a minute of being written. A remote file is maintained once its
//! host has been reached this session (upkeep never opens a connection itself),
//! and again after an agent there finishes a turn, at most every
//! [`REMOTE_REFRESH`]. A failed load backs off for [`RETRY_AFTER_FAILURE`].

use super::*;

/// How often the background pass looks for files due for maintenance.
pub(super) const BACKGROUND_CHECK: Duration = Duration::from_secs(60);
/// How long a project whose file failed to load waits before the pass retries it.
pub(super) const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(30 * 60);
/// Least time between maintenance runs a remote project's agent turns can trigger.
const REMOTE_REFRESH: Duration = Duration::from_secs(10 * 60);

/// What a poll does with one project's file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LogCheck {
    /// Leave it: loading now, backing off after a failure, or its host isn't
    /// reachable yet.
    Skip,
    /// Load it — which maintains (stamps, orders, prunes) the file.
    Maintain,
    /// Already maintained today and local: reload only if its file stamp moved.
    Stat,
}

/// What a poll knows about one project's file.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LogState {
    /// Day (seconds at its start) the file was last maintained.
    pub maintained_day: Option<i64>,
    pub loading: bool,
    pub backing_off: bool,
    /// Local projects always; remote ones once their host was reached this session.
    pub reachable: bool,
    pub local: bool,
    /// A remote file agents have likely written to since it was last maintained.
    pub changed_remotely: bool,
}

/// Decide a poll's action for one file. `today` is the day's start.
pub(super) fn plan_log_check(today: i64, log: LogState) -> LogCheck {
    if log.loading || log.backing_off || !log.reachable {
        LogCheck::Skip
    } else if log.maintained_day != Some(today) || log.changed_remotely {
        LogCheck::Maintain
    } else if log.local {
        LogCheck::Stat
    } else {
        LogCheck::Skip
    }
}

/// Shared-memory upkeep bookkeeping, per project. Runtime-only.
#[derive(Default)]
pub(super) struct Upkeep {
    /// UTC day each project's memory was last maintained, and when.
    maintained_day: HashMap<Uuid, i64>,
    maintained_at: HashMap<Uuid, Instant>,
    /// When a project whose memory failed to load may be retried.
    retry_at: HashMap<Uuid, Instant>,
    /// The local file's stamp at the last load, for cheap change polling.
    stamps: HashMap<Uuid, Option<DiskStamp>>,
    loading: HashSet<Uuid>,
    /// Remote projects where an agent finished a turn since the last maintenance.
    turned: HashSet<Uuid>,
    /// When the pass last ran (`None` = run on the next tick).
    checked_at: Option<Instant>,
}

impl Upkeep {
    /// Run the pass on the next tick (a project was just enabled).
    pub(super) fn check_soon(&mut self) {
        self.checked_at = None;
    }
}

impl MuxelApp {
    /// Called from `tick`: maintain each memory-enabled project that's due.
    pub(super) fn poll_memory(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if self
            .memory_upkeep
            .checked_at
            .is_some_and(|at| now.duration_since(at) < BACKGROUND_CHECK)
        {
            return;
        }
        self.memory_upkeep.checked_at = Some(now);
        let today = now_secs().div_euclid(86_400) * 86_400;
        let mut due = Vec::new();
        let mut jobs: Vec<(Uuid, PathBuf, Option<DiskStamp>)> = Vec::new();
        for p in self.workspace.projects.iter().filter(|p| p.memory_enabled) {
            let pid = p.id;
            let local = p.remote.is_none();
            let up = &self.memory_upkeep;
            let log = LogState {
                maintained_day: up.maintained_day.get(&pid).copied(),
                loading: up.loading.contains(&pid),
                backing_off: up.retry_at.get(&pid).is_some_and(|at| *at > now),
                reachable: local
                    || (self.remote_synced.contains(&pid)
                        && !self.remote_connect_failed.contains_key(&pid)),
                local,
                changed_remotely: !local
                    && up.turned.contains(&pid)
                    && up
                        .maintained_at
                        .get(&pid)
                        .is_none_or(|at| now.duration_since(*at) >= REMOTE_REFRESH),
            };
            match plan_log_check(today, log) {
                LogCheck::Maintain => due.push(pid),
                LogCheck::Stat => jobs.push((
                    pid,
                    p.root_path.join(MEMORY_DIR).join(MEMORY_FILE),
                    up.stamps.get(&pid).copied().flatten(),
                )),
                LogCheck::Skip => {}
            }
        }
        for pid in due {
            self.maintain_project_memory(pid, cx);
        }
        if jobs.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let changed: Vec<Uuid> = cx
                .background_executor()
                .spawn(async move {
                    jobs.into_iter()
                        .filter(|(_, file, stamp)| crate::editor::disk_stamp(file) != *stamp)
                        .map(|(pid, ..)| pid)
                        .collect()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for pid in changed {
                    this.maintain_project_memory(pid, cx);
                }
            });
        })
        .detach();
    }

    /// Called from `tick` with the projects whose agents just left `Working`: a
    /// remote one with shared memory gets maintained again soon (local files are
    /// polled by stamp instead).
    pub(super) fn note_memory_turns(&mut self, pids: &[Uuid]) {
        for pid in pids {
            if self
                .workspace
                .project(*pid)
                .is_some_and(|p| p.memory_enabled && p.remote.is_some())
            {
                self.memory_upkeep.turned.insert(*pid);
            }
        }
    }

    /// Load and maintain `pid`'s memory off the UI thread (see
    /// [`integrations::maintain_memory`]); the memory panel shows the result when
    /// it's showing that project. A load already in flight answers for this one.
    pub(super) fn maintain_project_memory(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        if !self.memory_upkeep.loading.insert(pid) {
            return;
        }
        let Some(loc) = self.repo_loc(pid) else {
            self.memory_upkeep.loading.remove(&pid);
            self.memory_upkeep
                .retry_at
                .insert(pid, Instant::now() + RETRY_AFTER_FAILURE);
            return;
        };
        self.memory_upkeep.turned.remove(&pid);
        let local_file = match &loc {
            integrations::RepoLoc::Local(root) => Some(root.join(MEMORY_DIR).join(MEMORY_FILE)),
            integrations::RepoLoc::Remote(_) => None,
        };
        let now = now_secs();
        cx.spawn(async move |this, cx| {
            let (stamp, result) = cx
                .background_executor()
                .spawn(async move {
                    // Stamp before reading: a section added during the load then
                    // still reads as a change on the next poll.
                    let stamp = local_file.as_deref().and_then(crate::editor::disk_stamp);
                    let result =
                        integrations::maintain_memory(&loc, now).map_err(|e| format!("{e:#}"));
                    (stamp, result)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let up = &mut this.memory_upkeep;
                up.loading.remove(&pid);
                let shown = this.memory_pid == Some(pid);
                match result {
                    Ok(kept) => {
                        up.maintained_day
                            .insert(pid, now.div_euclid(86_400) * 86_400);
                        up.maintained_at.insert(pid, Instant::now());
                        up.retry_at.remove(&pid);
                        up.stamps.insert(pid, stamp);
                        if shown {
                            this.memory_entries = kept;
                            this.memory_load_error = None;
                        }
                    }
                    Err(e) => {
                        up.retry_at
                            .insert(pid, Instant::now() + RETRY_AFTER_FAILURE);
                        if shown {
                            this.memory_load_error = Some(e);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Apply a memory-panel edit: shown at once, and saved onto the file as it is
    /// on disk (see [`integrations::edit_memory`]).
    pub(super) fn apply_memory_edit(&mut self, edit: memory::Edit, cx: &mut Context<Self>) {
        let Some(pid) = self.memory_pid else {
            return;
        };
        let now = now_secs();
        edit.apply(&mut self.memory_entries);
        let entries = std::mem::take(&mut self.memory_entries);
        self.memory_entries = memory::maintain(entries, now).kept;
        cx.notify();
        let Some(loc) = self.repo_loc(pid) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { integrations::edit_memory(&loc, now, &edit) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(kept) => {
                        if this.memory_pid == Some(pid) {
                            this.memory_entries = kept;
                        }
                        this.clear_save_error(SaveTarget::Memory);
                    }
                    Err(e) => {
                        this.report_save_error(SaveTarget::Memory, format!("{e:#}"));
                        // Show what's really on disk, not the edit that didn't land.
                        this.maintain_project_memory(pid, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::{LogCheck, LogState, plan_log_check};

    const TODAY: i64 = 1_800_057_600;
    const DAY: i64 = 86_400;

    fn local() -> LogState {
        LogState {
            reachable: true,
            local: true,
            ..LogState::default()
        }
    }

    #[test]
    fn a_file_not_maintained_today_is_maintained() {
        // Never (app start) and yesterday (a day rollover) alike.
        assert_eq!(plan_log_check(TODAY, local()), LogCheck::Maintain);
        let yesterday = LogState {
            maintained_day: Some(TODAY - DAY),
            ..local()
        };
        assert_eq!(plan_log_check(TODAY, yesterday), LogCheck::Maintain);
    }

    #[test]
    fn a_maintained_local_file_is_only_statted() {
        let done = LogState {
            maintained_day: Some(TODAY),
            ..local()
        };
        assert_eq!(plan_log_check(TODAY, done), LogCheck::Stat);
        let remote = LogState {
            local: false,
            ..done
        };
        assert_eq!(
            plan_log_check(TODAY, remote),
            LogCheck::Skip,
            "no SSH polling"
        );
        let written = LogState {
            changed_remotely: true,
            ..remote
        };
        assert_eq!(
            plan_log_check(TODAY, written),
            LogCheck::Maintain,
            "an agent turn there makes it due again"
        );
    }

    #[test]
    fn busy_failing_or_unreachable_files_are_left_alone() {
        for log in [
            LogState {
                loading: true,
                ..local()
            },
            LogState {
                backing_off: true,
                changed_remotely: true,
                ..local()
            },
            LogState {
                reachable: false,
                local: false,
                changed_remotely: true,
                ..local()
            },
        ] {
            assert_eq!(plan_log_check(TODAY, log), LogCheck::Skip, "{log:?}");
        }
    }
}
