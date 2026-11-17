//! The app's side of reminders (`.muxel/REMINDERS.md`): loading each
//! reminders-enabled project's file into a cache, noticing when one comes due,
//! alerting (a popup or a desktop notification, plus a sound that repeats until
//! silenced), and the docked per-project panel.
//!
//! Watching costs almost nothing. The 1 s status tick only compares the cached
//! due times against the clock. Local files are `stat`ed off the UI thread every
//! [`REMINDER_CHECK`] and re-read only when their stamp changed. Remote files are
//! re-read when one of the project's agents finishes a turn (which is when agents
//! append), on toggle-on, and once a day while the host is connected; muxel never
//! opens an SSH connection just to look at reminders.
//!
//! Before a due reminder rings, its file is re-read once, so a reminder that was
//! cancelled or dismissed elsewhere (an agent, another machine) doesn't go off.
//! Dismissing removes the line from the file; snoozing moves its time.

use super::upkeep::{LogCheck, LogState, RETRY_AFTER_FAILURE, plan_log_check};
use super::*;
use crate::sound;
use muxel_core::reminders::{self, LocalZone, Reminder};
use muxel_core::sound_alert::volume_gain;
use std::sync::atomic::{AtomicBool, Ordering};

/// How often local reminder files are checked for changes.
const REMINDER_CHECK: Duration = Duration::from_secs(15);
/// Silence between repeats of the reminder sound.
const SOUND_GAP: Duration = Duration::from_secs(3);
/// The snooze choices, in minutes.
pub(super) const SNOOZE_MINUTES: [i64; 3] = [5, 15, 60];

/// One project's reminders, as last loaded.
#[derive(Default)]
pub(super) struct ProjectReminders {
    pub entries: Vec<Reminder>,
    pub error: Option<String>,
    pub loaded: bool,
    pub stamp: Option<DiskStamp>,
}

/// A reminder that has come due and is waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Ringing {
    pub pid: Uuid,
    pub id: String,
    pub due: i64,
    pub message: String,
    pub agent: Option<String>,
}

/// Everything the app keeps about reminders. Runtime-only.
pub(super) struct ReminderState {
    /// The docked panel (shares the file-browser/memory/accomplishments slot).
    pub show_panel: bool,
    pub pid: Option<Uuid>,
    pub when_input: Entity<InputState>,
    pub message_input: Entity<InputState>,
    /// Why the quick-add box couldn't add, if it couldn't.
    pub add_error: Option<SharedString>,
    /// The reminder (project, id) showing an inline "delete?" confirm, if any.
    pub confirm_delete: Option<(Uuid, String)>,
    pub lists: HashMap<Uuid, ProjectReminders>,
    loading: HashSet<Uuid>,
    /// Projects that changed again while loading (reloaded once that load lands).
    stale: HashSet<Uuid>,
    /// Projects whose file is being re-read before a due reminder rings.
    confirming: HashSet<Uuid>,
    checked_at: Option<Instant>,
    /// UTC day each project was last loaded, and when a failed one may retry.
    maintained_day: HashMap<Uuid, i64>,
    retry_at: HashMap<Uuid, Instant>,
    /// Projects whose `.muxel/REMINDERS.md` was ensured this session.
    pub ensured: HashSet<Uuid>,
    /// (project, id, due) of every reminder that has rung, so each rings once
    /// per due time. A snoozed reminder has a new due time, so it rings again.
    fired: HashSet<(Uuid, String, i64)>,
    pub ringing: Vec<Ringing>,
    /// Raised to stop the repeating sound; `None` while it's quiet.
    playback: Option<Arc<AtomicBool>>,
    /// Whether the reminder popup is showing.
    pub popup: bool,
    /// Bring the window forward on the next tick (it needs the `Window`).
    raise_pending: bool,
}

impl ReminderState {
    pub(super) fn new(when_input: Entity<InputState>, message_input: Entity<InputState>) -> Self {
        Self {
            show_panel: false,
            pid: None,
            when_input,
            message_input,
            add_error: None,
            confirm_delete: None,
            lists: HashMap::new(),
            loading: HashSet::new(),
            stale: HashSet::new(),
            confirming: HashSet::new(),
            checked_at: None,
            maintained_day: HashMap::new(),
            retry_at: HashMap::new(),
            ensured: HashSet::new(),
            fired: HashSet::new(),
            ringing: Vec::new(),
            playback: None,
            popup: false,
            raise_pending: false,
        }
    }

    /// Forget every project's reminders (switching workspaces), keeping the inputs.
    pub(super) fn reset(&mut self) {
        self.silence();
        let when_input = self.when_input.clone();
        let message_input = self.message_input.clone();
        *self = Self::new(when_input, message_input);
    }

    /// Stop the repeating sound, if it's playing.
    pub(super) fn silence(&mut self) {
        if let Some(stop) = self.playback.take() {
            stop.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn sounding(&self) -> bool {
        self.playback.is_some()
    }

    pub(super) fn is_ringing(&self, pid: Uuid, id: &str) -> bool {
        self.ringing.iter().any(|r| r.pid == pid && r.id == id)
    }
}

/// Unix seconds now.
fn utc_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `due` as the user reads it: the time alone today, else the day and time.
pub(super) fn fmt_due(due: i64, now: i64) -> String {
    use chrono::TimeZone;
    let (Some(at), Some(today)) = (
        chrono::Local.timestamp_opt(due, 0).earliest(),
        chrono::Local.timestamp_opt(now, 0).earliest(),
    ) else {
        return String::new();
    };
    if at.date_naive() == today.date_naive() {
        at.format("%H:%M").to_string()
    } else if (at.date_naive() - today.date_naive()).num_days().abs() < 7 {
        at.format("%a %H:%M").to_string()
    } else {
        at.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// How far off `due` is: "in 5 min", "in 2 h", "overdue".
pub(super) fn relative_due(due: i64, now: i64) -> String {
    let diff = due - now;
    if diff <= 0 {
        return if diff > -60 {
            t("due now").to_string()
        } else {
            t("overdue").to_string()
        };
    }
    let mins = (diff + 59) / 60;
    if mins < 60 {
        tf("in {n} min", &[("n", &mins.to_string())])
    } else if mins < 48 * 60 {
        tf("in {n} h", &[("n", &((mins + 30) / 60).to_string())])
    } else {
        tf("in {n} d", &[("n", &((mins + 720) / 1440).to_string())])
    }
}

impl MuxelApp {
    fn reminder_projects(&self) -> impl Iterator<Item = &Project> {
        self.workspace
            .projects
            .iter()
            .filter(|p| p.reminders_enabled)
    }

    fn remote_reachable(&self, pid: Uuid) -> bool {
        self.remote_synced.contains(&pid) && !self.remote_connect_failed.contains_key(&pid)
    }

    /// Load the given projects' reminders off the UI thread, maintained, into the
    /// cache, then ring any that are due. A project already loading is reloaded
    /// once that load lands instead.
    pub(super) fn reload_reminders(&mut self, pids: Vec<Uuid>, cx: &mut Context<Self>) {
        for pid in pids {
            if !self.reminders.loading.insert(pid) {
                self.reminders.stale.insert(pid);
                continue;
            }
            let Some(loc) = self.repo_loc(pid) else {
                self.reminders.loading.remove(&pid);
                self.reminders
                    .retry_at
                    .insert(pid, Instant::now() + RETRY_AFTER_FAILURE);
                let list = self.reminders.lists.entry(pid).or_default();
                list.loaded = true;
                list.error = Some(t("its SSH host isn't configured").to_string());
                self.fire_due_reminders(pid, cx);
                continue;
            };
            let local_file = match &loc {
                integrations::RepoLoc::Local(root) => {
                    Some(root.join(MEMORY_DIR).join(REMINDERS_FILE))
                }
                integrations::RepoLoc::Remote(_) => None,
            };
            cx.spawn(async move |this, cx| {
                let (stamp, result) = cx
                    .background_executor()
                    .spawn(async move {
                        // Stamp before reading: an append during the load still
                        // reads as a change on the next check.
                        let stamp = local_file.as_deref().and_then(crate::editor::disk_stamp);
                        let result = integrations::maintain_reminders(&loc, &LocalZone)
                            .map_err(|e| format!("{e:#}"));
                        (stamp, result)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.reminders.loading.remove(&pid);
                    let list = this.reminders.lists.entry(pid).or_default();
                    list.loaded = true;
                    list.stamp = stamp;
                    match result {
                        Ok(entries) => {
                            list.entries = entries;
                            list.error = None;
                            this.reminders
                                .maintained_day
                                .insert(pid, utc_now().div_euclid(86_400));
                            this.reminders.retry_at.remove(&pid);
                        }
                        Err(e) => {
                            // Keep the last good list: a host that can't be read
                            // right now mustn't make a due reminder vanish.
                            list.error = Some(e);
                            this.reminders
                                .retry_at
                                .insert(pid, Instant::now() + RETRY_AFTER_FAILURE);
                        }
                    }
                    if this.reminders.stale.remove(&pid) {
                        this.reload_reminders(vec![pid], cx);
                    } else {
                        this.reminders.confirming.remove(&pid);
                        this.drop_vanished_ringing(pid);
                        this.fire_due_reminders(pid, cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// Called from `tick`: raise the window for a reminder that just rang, check
    /// local files every [`REMINDER_CHECK`], and send due reminders through a
    /// confirming re-read before they ring.
    pub(super) fn poll_reminders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.reminders.raise_pending) {
            window.activate_window();
            window.request_attention();
        }
        if self.reminder_projects().next().is_none() {
            return;
        }
        let now = Instant::now();
        let today = utc_now().div_euclid(86_400);

        // Due reminders: confirm against the file, then ring.
        let utc = utc_now();
        let mut confirm = Vec::new();
        for p in self.reminder_projects() {
            let pid = p.id;
            let due = self.reminders.lists.get(&pid).is_some_and(|l| {
                l.entries.iter().any(|r| {
                    r.due.is_some_and(|d| {
                        d <= utc && !self.reminders.fired.contains(&(pid, r.id(), d))
                    })
                })
            });
            if !due || self.reminders.confirming.contains(&pid) {
                continue;
            }
            if p.remote.is_some() && !self.remote_reachable(pid) {
                // No connection to confirm over: ring from what we have.
                confirm.push((pid, false));
            } else {
                confirm.push((pid, true));
            }
        }
        for (pid, reread) in confirm {
            if reread {
                self.reminders.confirming.insert(pid);
                self.reload_reminders(vec![pid], cx);
            } else {
                self.fire_due_reminders(pid, cx);
            }
        }

        if self
            .reminders
            .checked_at
            .is_some_and(|at| now.duration_since(at) < REMINDER_CHECK)
        {
            return;
        }
        self.reminders.checked_at = Some(now);
        let mut reload = Vec::new();
        let mut jobs: Vec<(Uuid, PathBuf, Option<DiskStamp>)> = Vec::new();
        for p in self.reminder_projects() {
            let pid = p.id;
            let local = p.remote.is_none();
            let state = LogState {
                maintained_day: self.reminders.maintained_day.get(&pid).copied(),
                loading: self.reminders.loading.contains(&pid),
                backing_off: self
                    .reminders
                    .retry_at
                    .get(&pid)
                    .is_some_and(|at| *at > now),
                reachable: local || self.remote_reachable(pid),
                local,
                changed_remotely: false,
            };
            match plan_log_check(today, state) {
                LogCheck::Maintain => reload.push(pid),
                LogCheck::Stat => jobs.push((
                    pid,
                    p.root_path.join(MEMORY_DIR).join(REMINDERS_FILE),
                    self.reminders.lists.get(&pid).and_then(|l| l.stamp),
                )),
                LogCheck::Skip => {}
            }
        }
        if !reload.is_empty() {
            self.reload_reminders(reload, cx);
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
            if !changed.is_empty() {
                let _ = this.update(cx, |this, cx| this.reload_reminders(changed, cx));
            }
        })
        .detach();
    }

    /// Called from `tick` with the projects whose agents just finished a turn:
    /// re-read the remote ones (local files are `stat`ed), since a turn is when an
    /// agent appends a reminder.
    pub(super) fn refresh_reminders_after_turns(&mut self, pids: &[Uuid], cx: &mut Context<Self>) {
        let mut remote: Vec<Uuid> = pids
            .iter()
            .copied()
            .filter(|pid| {
                self.workspace
                    .project(*pid)
                    .is_some_and(|p| p.reminders_enabled && p.remote.is_some())
                    && self.remote_reachable(*pid)
            })
            .collect();
        remote.sort();
        remote.dedup();
        if !remote.is_empty() {
            self.reload_reminders(remote, cx);
        }
    }

    /// A ringing reminder whose line is gone (dismissed or cancelled elsewhere) or
    /// was moved to a later time stops ringing.
    fn drop_vanished_ringing(&mut self, pid: Uuid) {
        let Some(list) = self.reminders.lists.get(&pid) else {
            return;
        };
        let before = self.reminders.ringing.len();
        self.reminders.ringing.retain(|r| {
            r.pid != pid
                || list
                    .entries
                    .iter()
                    .any(|e| e.id() == r.id && e.due == Some(r.due))
        });
        if self.reminders.ringing.len() != before {
            self.after_ringing_changed();
        }
    }

    /// Ring every due reminder in `pid`'s cached list that hasn't rung yet.
    fn fire_due_reminders(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        let now = utc_now();
        let Some(project) = self.workspace.project(pid).filter(|p| p.reminders_enabled) else {
            return;
        };
        let project_name = project.name.clone();
        let Some(list) = self.reminders.lists.get(&pid) else {
            return;
        };
        let mut new = Vec::new();
        for r in &list.entries {
            let Some(due) = r.due.filter(|&d| d <= now) else {
                continue;
            };
            if self.reminders.fired.insert((pid, r.id(), due)) {
                new.push(Ringing {
                    pid,
                    id: r.id(),
                    due,
                    message: r.message.clone(),
                    agent: r.agent.clone(),
                });
            }
        }
        if new.is_empty() {
            return;
        }
        let notifies = self.settings.reminder_notifies();
        let with_sound = self.settings.reminder_sound;
        for r in &new {
            self.add_event(NotifKind::Blocked, r.message.clone(), project_name.clone());
            if notifies {
                notify_with(
                    r.message.clone(),
                    tf("Reminder · {project}", &[("project", &project_name)]),
                    None,
                    with_sound,
                );
            }
        }
        self.reminders.ringing.extend(new);
        if !notifies {
            self.reminders.popup = true;
            self.reminders.raise_pending = true;
        }
        if with_sound && !self.reminders.sounding() {
            self.start_reminder_sound();
        }
        cx.notify();
    }

    fn start_reminder_sound(&mut self) {
        let gain = volume_gain(self.settings.sound_volume);
        if gain <= 0.0 {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        self.reminders.silence();
        self.reminders.playback = Some(stop.clone());
        let file = AlertSound::Reminder
            .file(&self.settings)
            .map(Path::to_path_buf);
        sound::ring_repeating(AlertSound::Reminder, file, gain, SOUND_GAP, stop);
    }

    /// Once nothing is ringing, the sound stops and the popup closes.
    fn after_ringing_changed(&mut self) {
        if self.reminders.ringing.is_empty() {
            self.reminders.silence();
            self.reminders.popup = false;
        }
    }

    /// Stop the sound; the reminders stay until dismissed or snoozed.
    pub(super) fn silence_reminders(&mut self, cx: &mut Context<Self>) {
        self.reminders.silence();
        cx.notify();
    }

    /// Hide the popup (and stop the sound). Ringing reminders stay in the title
    /// bar and the panel.
    pub(super) fn close_reminder_popup(&mut self, cx: &mut Context<Self>) {
        self.reminders.popup = false;
        self.reminders.silence();
        cx.notify();
    }

    /// Show the popup for what's ringing (the title-bar chip).
    pub(super) fn open_reminder_popup(&mut self, cx: &mut Context<Self>) {
        if !self.reminders.ringing.is_empty() {
            self.reminders.popup = true;
            cx.notify();
        }
    }

    /// Dismiss: stop ringing and remove the line from the file.
    pub(super) fn dismiss_reminder(&mut self, pid: Uuid, id: String, cx: &mut Context<Self>) {
        self.reminders
            .ringing
            .retain(|r| !(r.pid == pid && r.id == id));
        self.after_ringing_changed();
        self.edit_reminders(
            pid,
            move |list| {
                reminders::remove(list, &id);
            },
            cx,
        );
    }

    pub(super) fn dismiss_all_reminders(&mut self, cx: &mut Context<Self>) {
        let all = std::mem::take(&mut self.reminders.ringing);
        self.after_ringing_changed();
        let mut by_project: HashMap<Uuid, Vec<String>> = HashMap::new();
        for r in all {
            by_project.entry(r.pid).or_default().push(r.id);
        }
        for (pid, ids) in by_project {
            self.edit_reminders(
                pid,
                move |list| {
                    for id in &ids {
                        reminders::remove(list, id);
                    }
                },
                cx,
            );
        }
    }

    /// Snooze: stop ringing and move the reminder `minutes` from now.
    pub(super) fn snooze_reminder(
        &mut self,
        pid: Uuid,
        id: String,
        minutes: i64,
        cx: &mut Context<Self>,
    ) {
        self.reminders
            .ringing
            .retain(|r| !(r.pid == pid && r.id == id));
        self.after_ringing_changed();
        let due = utc_now() + minutes * 60;
        self.edit_reminders(
            pid,
            move |list| {
                reminders::reschedule(list, &id, due);
            },
            cx,
        );
    }

    fn delete_reminder(&mut self, pid: Uuid, id: String, cx: &mut Context<Self>) {
        self.reminders.confirm_delete = None;
        self.dismiss_reminder(pid, id, cx);
    }

    /// Apply an edit to the file as it is on disk now (see
    /// [`integrations::edit_reminders`]) and cache the result.
    fn edit_reminders(
        &mut self,
        pid: Uuid,
        edit: impl FnOnce(&mut Vec<Reminder>) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(loc) = self.repo_loc(pid) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { integrations::edit_reminders(&loc, &LocalZone, edit) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(entries) => {
                        let list = this.reminders.lists.entry(pid).or_default();
                        list.entries = entries;
                        list.loaded = true;
                        list.error = None;
                        this.clear_save_error(SaveTarget::Reminders);
                        this.drop_vanished_ringing(pid);
                    }
                    Err(e) => this.report_save_error(SaveTarget::Reminders, format!("{e:#}")),
                }
                // A load that read the file before this write would land stale.
                if this.reminders.loading.contains(&pid) {
                    this.reminders.stale.insert(pid);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Add a reminder from the quick-add boxes (panel or all-projects view).
    pub(super) fn add_reminder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.reminder_add_target() else {
            return;
        };
        let when = self.reminders.when_input.read(cx).value().to_string();
        let message = self
            .reminders
            .message_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if message.is_empty() {
            self.reminders.add_error = Some(t("Say what to remind you of."));
            cx.notify();
            return;
        }
        let now = utc_now();
        let Some(due) = reminders::parse_when(&when, now, &LocalZone) else {
            self.reminders.add_error = Some(t(
                "Couldn't read the time. Try 15m, 2h, 14:30, 3pm, tomorrow 9am, or 2026-12-01 09:00.",
            ));
            cx.notify();
            return;
        };
        self.reminders.add_error = None;
        for input in [&self.reminders.when_input, &self.reminders.message_input] {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.edit_reminders(
            pid,
            move |list| {
                let key = reminders::fresh_key(list, &message);
                list.push(Reminder::new(due, &message, Some(key)));
            },
            cx,
        );
    }

    /// Toggle the docked panel for `pid` (hides it if it's showing that project).
    pub(super) fn toggle_reminders_panel(
        &mut self,
        pid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.reminders.show_panel && self.reminders.pid == Some(pid) {
            self.reminders.show_panel = false;
            cx.notify();
            return;
        }
        self.open_reminders_panel(pid, window, cx);
    }

    /// Show `pid`'s reminders in the docked panel. Idempotent.
    pub(super) fn open_reminders_panel(
        &mut self,
        pid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reminders.show_panel = true;
        // The docked panels share one slot.
        self.show_file_browser = false;
        self.show_memory = false;
        self.show_accomplishments = false;
        if self.reminders.pid != Some(pid) {
            for input in [&self.reminders.when_input, &self.reminders.message_input] {
                input.update(cx, |s, cx| s.set_value("", window, cx));
            }
            self.reminders.add_error = None;
        }
        self.reminders.pid = Some(pid);
        self.reminders.confirm_delete = None;
        let reachable = self
            .workspace
            .project(pid)
            .is_some_and(|p| p.remote.is_none() || self.remote_reachable(pid));
        if reachable {
            self.reload_reminders(vec![pid], cx);
        }
        cx.notify();
    }

    /// Toggle the project's reminders; on enable, seed the file and git-ignore
    /// `.muxel/`. New agents in the project then get the instruction.
    pub(super) fn toggle_project_reminders(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        let Some(p) = self.workspace.project_mut(pid) else {
            return;
        };
        p.reminders_enabled = !p.reminders_enabled;
        if p.reminders_enabled {
            self.reminders.ensured.remove(&pid);
            self.ensure_project_reminders(pid, cx);
            self.reminders.maintained_day.remove(&pid);
            self.reminders.checked_at = None; // pick it up on the next tick
        } else {
            self.reminders.ringing.retain(|r| r.pid != pid);
            self.after_ringing_changed();
        }
        self.persist();
        cx.notify();
    }

    /// Ensure the project's `.muxel/REMINDERS.md` and `.gitignore` entry exist,
    /// off the UI thread (remote is an SSH round-trip).
    pub(super) fn ensure_project_reminders(&self, pid: Uuid, cx: &mut Context<Self>) {
        let Some(loc) = self.repo_loc(pid) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { integrations::ensure_reminders_file(&loc) })
                .await;
            if let Err(e) = res {
                let _ = this.update(cx, |this, cx| {
                    let msg = format!("{e}");
                    if !this.handle_ssh_error(&msg, None, SshRetry::None, cx) {
                        this.add_event(NotifKind::Error, t("Reminders"), msg);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Open the raw `REMINDERS.md` in the editor, creating it if needed.
    fn open_reminders_file(&mut self, pid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.workspace.project(pid) else {
            return;
        };
        let is_remote = p.remote.is_some();
        let root = match &p.remote {
            Some(r) => r.remote_root.clone(),
            None => p.root_path.display().to_string(),
        };
        let path = PathBuf::from(format!("{root}/{MEMORY_DIR}/{REMINDERS_FILE}"));
        let origin = ResourceOrigin::Chrome {
            near: self.active_instance,
        };
        if is_remote {
            self.ensure_project_reminders(pid, cx);
        } else if let Some(loc) = self.repo_loc(pid)
            && let Err(e) = integrations::ensure_reminders_file(&loc)
        {
            self.add_event(NotifKind::Error, t("Reminders"), format!("{e}"));
        }
        self.open_editor_at(pid, Some(path), origin, OpenFocus::Focus, window, cx);
    }

    fn project_name(&self, pid: Uuid) -> String {
        self.workspace
            .project(pid)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    /// Dismiss + snooze buttons for a ringing reminder.
    fn ringing_actions(&self, pid: Uuid, id: &str, key: &str, cx: &mut Context<Self>) -> Div {
        let mut row = div().flex().flex_wrap().items_center().gap_1();
        for minutes in SNOOZE_MINUTES {
            let id = id.to_string();
            let label = if minutes < 60 {
                tf("{n} min", &[("n", &minutes.to_string())])
            } else {
                tf("{n} h", &[("n", &(minutes / 60).to_string())])
            };
            row = row.child(
                Button::new(SharedString::from(format!("rem-snooze-{minutes}-{key}")))
                    .ghost()
                    .xsmall()
                    .label(label)
                    .tooltip(t("Snooze"))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.snooze_reminder(pid, id.clone(), minutes, cx)
                    })),
            );
        }
        let id = id.to_string();
        row.child(
            Button::new(SharedString::from(format!("rem-dismiss-{key}")))
                .primary()
                .xsmall()
                .icon(IconName::Check)
                .label(t("Dismiss"))
                .on_click(
                    cx.listener(move |this, _e, _w, cx| this.dismiss_reminder(pid, id.clone(), cx)),
                ),
        )
    }

    /// The title bar's Reminders button, shown while any project has reminders on
    /// (or something is ringing). Normally it toggles the all-projects Reminders
    /// view; while reminders are due it turns into an alarm showing how many,
    /// which opens the popup, with a button beside it to silence the sound.
    pub(super) fn render_reminders_titlebar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ringing = self.reminders.ringing.len();
        if ringing == 0 && self.reminder_projects().next().is_none() {
            return None;
        }
        let showing = self.main_view == super::hotbar::MainView::Reminders;
        let button = if ringing > 0 {
            Button::new("reminders-titlebar")
                .warning()
                .small()
                .icon(IconName::Bell)
                .label(ringing.to_string())
                .tooltip(t("Reminders due"))
                .on_click(cx.listener(|this, _e, _w, cx| this.open_reminder_popup(cx)))
        } else {
            Button::new("reminders-titlebar")
                .ghost()
                .icon(IconName::Bell)
                .selected(showing)
                .tooltip(t("Reminders (all projects)"))
                .on_click(cx.listener(|this, _e, window, cx| {
                    this.toggle_main_view(super::hotbar::MainView::Reminders, window, cx)
                }))
        };
        let mut chip = div().flex().items_center().gap_1().child(button);
        if self.reminders.sounding() {
            chip = chip.child(
                Button::new("reminders-silence")
                    .ghost()
                    .small()
                    .icon(Icon::empty().path("icons/volume-2.svg"))
                    .tooltip(t("Silence"))
                    .on_click(cx.listener(|this, _e, _w, cx| this.silence_reminders(cx))),
            );
        }
        Some(chip.into_any_element())
    }

    /// The popup listing every ringing reminder.
    pub(super) fn render_reminder_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let now = utc_now();
        let muted = cx.theme().muted_foreground;
        let mut list = div()
            .id("rem-popup-list")
            .flex()
            .flex_col()
            .gap_3()
            .max_h(px(420.0))
            .overflow_y_scroll();
        for (i, r) in self.reminders.ringing.iter().enumerate() {
            let mut meta = vec![self.project_name(r.pid), fmt_due(r.due, now)];
            if let Some(agent) = &r.agent {
                meta.push(agent.clone());
            }
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().secondary)
                    .child(div().text_base().font_semibold().child(r.message.clone()))
                    .child(div().text_xs().text_color(muted).child(meta.join(" · ")))
                    .child(self.ringing_actions(r.pid, &r.id, &format!("pop-{i}"), cx)),
            );
        }
        let sounding = self.reminders.sounding();
        modal_backdrop()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.close_reminder_popup(cx)),
            )
            .child(
                div()
                    .w(px(460.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                        if ev.keystroke.key == "escape" {
                            this.close_reminder_popup(cx);
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Icon::new(IconName::Bell).text_color(cx.theme().warning))
                            .child(div().text_lg().font_semibold().child(t("Reminder"))),
                    )
                    .child(list)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .children(sounding.then(|| {
                                Button::new("rem-popup-silence")
                                    .ghost()
                                    .icon(Icon::empty().path("icons/volume-2.svg"))
                                    .label(t("Silence"))
                                    .on_click(
                                        cx.listener(|this, _e, _w, cx| this.silence_reminders(cx)),
                                    )
                            }))
                            .child(
                                Button::new("rem-popup-close")
                                    .ghost()
                                    .label(t("Later"))
                                    .tooltip(t("Hide this; the reminders stay due"))
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.close_reminder_popup(cx)
                                    })),
                            )
                            .children((self.reminders.ringing.len() > 1).then(|| {
                                Button::new("rem-popup-dismiss-all")
                                    .primary()
                                    .label(t("Dismiss all"))
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.dismiss_all_reminders(cx)
                                    }))
                            })),
                    ),
            )
            .into_any_element()
    }

    /// One reminder as a list row: message, when (and how far off), agent, the
    /// dismiss/snooze actions while it's ringing, and delete with a confirm.
    fn reminder_row(
        &self,
        pid: Uuid,
        index: usize,
        r: &Reminder,
        now: i64,
        cx: &mut Context<Self>,
    ) -> Div {
        let muted = cx.theme().muted_foreground;
        let id = r.id();
        let key = format!("{}-{index}", pid.simple());
        let ringing = self.reminders.is_ringing(pid, &id);
        let confirming = self
            .reminders
            .confirm_delete
            .as_ref()
            .is_some_and(|(p, i)| *p == pid && *i == id);
        let when: SharedString = match r.due {
            Some(due) => format!("{} · {}", fmt_due(due, now), relative_due(due, now)).into(),
            None => t("Can't read this time; fix it in the file"),
        };
        let when_color = if r.due.is_none() {
            cx.theme().danger
        } else if ringing || r.is_due(now) {
            cx.theme().warning
        } else {
            muted
        };
        let delete = if confirming {
            let id = id.clone();
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(div().text_xs().text_color(muted).child(t("Delete?")))
                .child(
                    Button::new(SharedString::from(format!("rem-del-yes-{key}")))
                        .danger()
                        .xsmall()
                        .icon(IconName::Check)
                        .tooltip(t("Confirm delete"))
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.delete_reminder(pid, id.clone(), cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("rem-del-no-{key}")))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip(t("Cancel"))
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            this.reminders.confirm_delete = None;
                            cx.notify();
                        })),
                )
        } else {
            let id = id.clone();
            div().flex_none().child(
                Button::new(SharedString::from(format!("rem-del-{key}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Delete)
                    .tooltip(t("Delete"))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.reminders.confirm_delete = Some((pid, id.clone()));
                        cx.notify();
                    })),
            )
        };
        let mut body = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(div().text_sm().child(r.message.clone()))
            .child(div().text_xs().text_color(when_color).child(when))
            .children(
                r.agent
                    .clone()
                    .map(|a| div().text_xs().text_color(muted).child(a)),
            );
        if ringing {
            body = body.child(self.ringing_actions(pid, &id, &format!("row-{key}"), cx));
        }
        let mut row = div()
            .flex()
            .items_start()
            .gap_1()
            .p_1()
            .rounded(cx.theme().radius)
            .hover(|s| s.bg(cx.theme().secondary))
            .child(body)
            .child(delete);
        if ringing {
            row = row.border_1().border_color(cx.theme().warning);
        }
        row
    }

    /// The quick-add boxes (message, then when) and their error, if any.
    fn reminder_add_form(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(Input::new(&self.reminders.message_input).w_full())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.reminders.when_input).w_full()),
                    )
                    .child(
                        Button::new("rem-add")
                            .primary()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip(t("Add reminder"))
                            .on_click(
                                cx.listener(|this, _e, window, cx| this.add_reminder(window, cx)),
                            ),
                    ),
            )
            .children(
                self.reminders
                    .add_error
                    .clone()
                    .map(|e| div().text_xs().text_color(cx.theme().danger).child(e)),
            )
    }

    /// The docked reminders panel: header, the project's reminders soonest first
    /// (ringing ones with dismiss/snooze), and the quick-add boxes.
    pub(super) fn render_reminders_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(pid) = self.reminders.pid else {
            return div().into_any_element();
        };
        let pname = self.project_name(pid);
        let enabled = self
            .workspace
            .project(pid)
            .is_some_and(|p| p.reminders_enabled);
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let now = utc_now();
        let list_state = self.reminders.lists.get(&pid);
        let entries: &[Reminder] = list_state.map(|l| l.entries.as_slice()).unwrap_or_default();

        let mut list = div()
            .id("rem-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2()
            .py_1()
            .flex()
            .flex_col()
            .gap_1();
        if let Some(err) = list_state.and_then(|l| l.error.as_ref()) {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(tf("Couldn't load reminders: {error}", &[("error", err)])),
            );
        }
        if !enabled {
            list = list.child(div().text_xs().text_color(muted).child(t(
                "Reminders are off for this project, so agents aren't adding them and \
                 none will go off. Turn them on in Settings → Projects.",
            )));
        }
        if !list_state.is_some_and(|l| l.loaded) {
            list = list.child(div().text_xs().text_color(muted).child(t("Loading…")));
        } else if entries.is_empty() {
            list = list.child(div().text_xs().text_color(muted).child(t(
                "No reminders. Ask an agent to remind you of something, or add one below.",
            )));
        }
        for (i, r) in entries.iter().enumerate() {
            list = list.child(self.reminder_row(pid, i, r, now, cx));
        }

        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py(px(6.0))
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .font_semibold()
                    .text_color(muted)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(tf("REMINDERS · {proj_name}", &[("proj_name", &pname)])),
            )
            .child(
                Button::new("rem-refresh")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/refresh.svg"))
                    .tooltip(t("Refresh"))
                    .on_click(
                        cx.listener(move |this, _e, _w, cx| this.reload_reminders(vec![pid], cx)),
                    ),
            )
            .child(
                Button::new("rem-open-file")
                    .ghost()
                    .xsmall()
                    .icon(IconName::File)
                    .tooltip(t("Open REMINDERS.md in editor"))
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.open_reminders_file(pid, window, cx)
                    })),
            )
            .child(
                Button::new("rem-expand")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Maximize)
                    .tooltip(t("Show all projects"))
                    .on_click(cx.listener(|this, _e, window, cx| {
                        this.set_main_view(super::hotbar::MainView::Reminders, window, cx)
                    })),
            )
            .child(
                Button::new("rem-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(t("Close reminders panel"))
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.reminders.show_panel = false;
                        cx.notify();
                    })),
            );

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(border)
            .child(header)
            .child(list)
            .child(
                div()
                    .p_2()
                    .border_t_1()
                    .border_color(border)
                    .child(self.reminder_add_form(cx)),
            )
            .into_any_element()
    }

    /// Where the quick-add box adds a reminder: the panel's project, or in the
    /// all-projects view, the active project when it has reminders on (else the
    /// first project that does).
    fn reminder_add_target(&self) -> Option<Uuid> {
        if self.main_view == super::hotbar::MainView::Reminders {
            self.workspace
                .active_project
                .filter(|pid| {
                    self.workspace
                        .project(*pid)
                        .is_some_and(|p| p.reminders_enabled)
                })
                .or_else(|| self.reminder_projects().next().map(|p| p.id))
        } else {
            self.reminders.pid
        }
    }

    /// Reload every reminders-enabled project that can be read without opening a
    /// connection (the all-projects view).
    pub(super) fn refresh_all_reminders(&mut self, cx: &mut Context<Self>) {
        let pids: Vec<Uuid> = self
            .reminder_projects()
            .filter(|p| p.remote.is_none() || self.remote_reachable(p.id))
            .map(|p| p.id)
            .collect();
        self.reload_reminders(pids, cx);
    }

    /// The hotbar's Reminders view: every reminders-enabled project's reminders,
    /// what's due first, grouped by project, with the quick-add box.
    pub(super) fn render_reminders_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let now = utc_now();
        let projects: Vec<(Uuid, String)> = self
            .reminder_projects()
            .map(|p| (p.id, p.name.clone()))
            .collect();
        let total: usize = projects
            .iter()
            .filter_map(|(pid, _)| self.reminders.lists.get(pid))
            .map(|l| l.entries.len())
            .sum();

        let mut body = div()
            .id("rem-view")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_4();
        if projects.is_empty() {
            body = body.child(div().text_sm().text_color(muted).child(t(
                "No project has reminders turned on. Enable them per project in \
                 Settings → Projects.",
            )));
        }
        for (pid, name) in &projects {
            let pid = *pid;
            let list_state = self.reminders.lists.get(&pid);
            let entries: &[Reminder] = list_state.map(|l| l.entries.as_slice()).unwrap_or_default();
            let mut section = div().flex().flex_col().gap_1().child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pb_1()
                    .border_b_1()
                    .border_color(border)
                    .child(div().text_sm().font_semibold().child(name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(entries.len().to_string()),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new(SharedString::from(format!(
                            "rem-view-open-{}",
                            pid.simple()
                        )))
                        .ghost()
                        .xsmall()
                        .icon(IconName::File)
                        .tooltip(t("Open REMINDERS.md in editor"))
                        .on_click(cx.listener(
                            move |this, _e, window, cx| this.open_reminders_file(pid, window, cx),
                        )),
                    ),
            );
            if let Some(err) = list_state.and_then(|l| l.error.as_ref()) {
                section = section.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(tf("Couldn't load reminders: {error}", &[("error", err)])),
                );
            }
            if !list_state.is_some_and(|l| l.loaded) {
                section = section.child(div().text_xs().text_color(muted).child(t("Loading…")));
            } else if entries.is_empty() {
                section =
                    section.child(div().text_xs().text_color(muted).child(t("No reminders.")));
            }
            for (i, r) in entries.iter().enumerate() {
                section = section.child(self.reminder_row(pid, i, r, now, cx));
            }
            body = body.child(section);
        }

        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(border)
            .child(Icon::new(IconName::Bell).text_color(muted))
            .child(div().text_base().font_semibold().child(t("Reminders")))
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(muted)
                    .child(tf("{count} pending", &[("count", &total.to_string())])),
            )
            .child(
                Button::new("rem-view-refresh")
                    .ghost()
                    .small()
                    .icon(Icon::empty().path("icons/refresh.svg"))
                    .tooltip(t("Refresh"))
                    .on_click(cx.listener(|this, _e, _w, cx| this.refresh_all_reminders(cx))),
            );

        let target = self.reminder_add_target();
        let footer = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_4()
            .py_2()
            .border_t_1()
            .border_color(border)
            .children(target.map(|pid| {
                div().text_xs().text_color(muted).child(tf(
                    "Add to {project}",
                    &[("project", &self.project_name(pid))],
                ))
            }))
            .child(div().max_w(px(640.0)).child(self.reminder_add_form(cx)));

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().background)
            .child(header)
            .child(body)
            .children(target.is_some().then_some(footer))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::relative_due;

    #[test]
    fn relative_due_rounds_up_to_the_next_unit() {
        assert_eq!(relative_due(1000, 1000), "due now");
        assert_eq!(relative_due(1000, 1000 + 3600), "overdue");
        assert_eq!(relative_due(1000 + 30, 1000), "in 1 min");
        assert_eq!(relative_due(1000 + 59 * 60, 1000), "in 59 min");
        assert_eq!(relative_due(1000 + 2 * 3600, 1000), "in 2 h");
        assert_eq!(relative_due(1000 + 3 * 86_400, 1000), "in 3 d");
    }
}
