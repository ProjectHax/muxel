//! The app's side of the accomplishment log (`.muxel/ACCOMPLISHMENTS.md`):
//! loading and maintaining each project's log into a cache the views share,
//! keeping it fresh while a view is open, the docked per-project panel, and
//! opening the all-projects overview — embedded in the main window or popped out
//! (both are `crate::accomplishments_view::AccomplishmentsOverview`).
//!
//! muxel maintains every log-enabled project on its own, with or without a view
//! open: a background pass (every [`BACKGROUND_CHECK`]) loads — and so prunes —
//! each project not yet maintained today, which covers app start and every day
//! rollover after it. A remote project joins in once its host has been reached
//! this session (pruning never opens a connection by itself), and a failed load
//! backs off for [`RETRY_AFTER_FAILURE`]. The same pass `stat`s local logs and
//! reloads those that changed, so an undated line is stamped with the day it was
//! appended.
//!
//! With a view open, local logs are `stat`ed every [`LOCAL_POLL`] instead, and
//! remote logs reload when one of the project's agents finishes a turn — which is
//! when agents append — so there is no SSH polling.

use super::upkeep::{BACKGROUND_CHECK, LogCheck, LogState, RETRY_AFTER_FAILURE, plan_log_check};
use super::*;
use crate::accomplishments_view::{
    AccomplishmentsOverview, OverviewProject, OverviewSnapshot, day_heading, entry_row,
    kind_counts_row, local_now_secs,
};

/// How often open views check their local log files for changes.
const LOCAL_POLL: Duration = Duration::from_secs(2);
impl MuxelApp {
    /// The main window, for overview actions that navigate it from a pop-out.
    pub(crate) fn main_window_handle(&self) -> Option<AnyWindowHandle> {
        self.main_window
    }

    fn accomplishments_overview_open(&self) -> bool {
        self.main_view == super::hotbar::MainView::Accomplishments
            || self.accomplishments_window.is_some()
    }

    fn accomplishment_projects(&self) -> impl Iterator<Item = &Project> {
        self.workspace
            .projects
            .iter()
            .filter(|p| p.accomplishments_enabled)
    }

    /// Projects whose log an open view is showing.
    fn accomplishments_shown_pids(&self) -> Vec<Uuid> {
        let mut pids: Vec<Uuid> = if self.accomplishments_overview_open() {
            self.accomplishment_projects().map(|p| p.id).collect()
        } else {
            Vec::new()
        };
        if self.show_accomplishments
            && let Some(pid) = self.accomplishments_pid
            && !pids.contains(&pid)
        {
            pids.push(pid);
        }
        pids
    }

    /// Load the given projects' logs off the UI thread — maintained, so a stale
    /// entry is pruned the first time anything looks — into the shared cache. A
    /// project already loading is reloaded once that load lands instead, so a
    /// change that arrives mid-load is never lost.
    pub(crate) fn reload_accomplishments(&mut self, pids: Vec<Uuid>, cx: &mut Context<Self>) {
        let now = local_now_secs();
        let days = self.settings.accomplishment_retention_days;
        for pid in pids {
            if !self.accomplishments_loading.insert(pid) {
                self.accomplishments_stale.insert(pid);
                continue;
            }
            let Some(loc) = self.repo_loc(pid) else {
                self.accomplishments_loading.remove(&pid);
                self.accomplishments_retry_at
                    .insert(pid, Instant::now() + RETRY_AFTER_FAILURE);
                let log = self.accomplishment_logs.entry(pid).or_default();
                log.loaded = true;
                log.error = Some(t("its SSH host isn't configured").to_string());
                self.accomplishments_generation += 1;
                continue;
            };
            let local_file = match &loc {
                integrations::RepoLoc::Local(root) => {
                    Some(root.join(MEMORY_DIR).join(ACCOMPLISHMENTS_FILE))
                }
                integrations::RepoLoc::Remote(_) => None,
            };
            cx.spawn(async move |this, cx| {
                let (stamp, result) = cx
                    .background_executor()
                    .spawn(async move {
                        // Stamp before reading: an append that lands during the
                        // load then still reads as a change on the next poll.
                        let stamp = local_file.as_deref().and_then(crate::editor::disk_stamp);
                        let result = integrations::maintain_accomplishments(&loc, now, days)
                            .map_err(|e| format!("{e:#}"));
                        (stamp, result)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.accomplishments_loading.remove(&pid);
                    let log = this.accomplishment_logs.entry(pid).or_default();
                    log.loaded = true;
                    log.stamp = stamp;
                    match result {
                        Ok(entries) => {
                            log.entries = entries;
                            log.error = None;
                            this.accomplishments_maintained
                                .insert(pid, accomplishments::day_start(now));
                            this.accomplishments_retry_at.remove(&pid);
                        }
                        Err(e) => {
                            log.error = Some(e);
                            this.accomplishments_retry_at
                                .insert(pid, Instant::now() + RETRY_AFTER_FAILURE);
                        }
                    }
                    this.accomplishments_generation += 1;
                    if this.accomplishments_stale.remove(&pid) {
                        this.reload_accomplishments(vec![pid], cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// Reload every log-enabled project (the overview's Refresh).
    pub(crate) fn refresh_all_accomplishments(&mut self, cx: &mut Context<Self>) {
        let pids = self.accomplishment_projects().map(|p| p.id).collect();
        self.reload_accomplishments(pids, cx);
    }

    /// Called from `tick`. Every [`BACKGROUND_CHECK`], maintain each log-enabled
    /// project that's due (see the module docs) and reload local logs that changed;
    /// while a view is open, also check the logs it shows every [`LOCAL_POLL`].
    pub(super) fn poll_accomplishments(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut candidates = Vec::new();
        if self
            .accomplishments_polled_at
            .is_none_or(|at| now.duration_since(at) >= LOCAL_POLL)
        {
            candidates = self.accomplishments_shown_pids();
            if !candidates.is_empty() {
                self.accomplishments_polled_at = Some(now);
            }
        }
        if self
            .accomplishments_checked_at
            .is_none_or(|at| now.duration_since(at) >= BACKGROUND_CHECK)
        {
            self.accomplishments_checked_at = Some(now);
            for pid in self.accomplishment_projects().map(|p| p.id) {
                if !candidates.contains(&pid) {
                    candidates.push(pid);
                }
            }
        }
        if candidates.is_empty() {
            return;
        }

        let today = accomplishments::day_start(local_now_secs());
        let mut due = Vec::new();
        let mut jobs: Vec<(Uuid, PathBuf, Option<DiskStamp>)> = Vec::new();
        for pid in candidates {
            let Some(p) = self.workspace.project(pid) else {
                continue;
            };
            let local = p.remote.is_none();
            let log = LogState {
                maintained_day: self.accomplishments_maintained.get(&pid).copied(),
                loading: self.accomplishments_loading.contains(&pid),
                backing_off: self
                    .accomplishments_retry_at
                    .get(&pid)
                    .is_some_and(|at| *at > now),
                reachable: local
                    || (self.remote_synced.contains(&pid)
                        && !self.remote_connect_failed.contains_key(&pid)),
                local,
                // Remote logs refresh on agent turns while a view shows them.
                changed_remotely: false,
            };
            match plan_log_check(today, log) {
                LogCheck::Maintain => due.push(pid),
                LogCheck::Stat => jobs.push((
                    pid,
                    p.root_path.join(MEMORY_DIR).join(ACCOMPLISHMENTS_FILE),
                    self.accomplishment_logs.get(&pid).and_then(|l| l.stamp),
                )),
                LogCheck::Skip => {}
            }
        }
        if !due.is_empty() {
            self.reload_accomplishments(due, cx);
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
                let _ = this.update(cx, |this, cx| this.reload_accomplishments(changed, cx));
            }
        })
        .detach();
    }

    /// Called from `tick` with the projects whose agents just left `Working`:
    /// reload any remote one an open view shows (local ones are polled).
    pub(super) fn refresh_accomplishments_after_turns(
        &mut self,
        mut pids: Vec<Uuid>,
        cx: &mut Context<Self>,
    ) {
        if pids.is_empty() {
            return;
        }
        let shown = self.accomplishments_shown_pids();
        pids.sort();
        pids.dedup();
        pids.retain(|pid| {
            shown.contains(pid)
                && self
                    .workspace
                    .project(*pid)
                    .is_some_and(|p| p.remote.is_some())
        });
        if !pids.is_empty() {
            self.reload_accomplishments(pids, cx);
        }
    }

    /// Apply a panel edit to the log on disk (see
    /// [`integrations::edit_accomplishments`]) and cache the result.
    fn edit_accomplishments(
        &mut self,
        pid: Uuid,
        edit: impl FnOnce(&mut Vec<Accomplishment>) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(loc) = self.repo_loc(pid) else {
            return;
        };
        let now = local_now_secs();
        let days = self.settings.accomplishment_retention_days;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { integrations::edit_accomplishments(&loc, now, days, edit) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(entries) => {
                        let log = this.accomplishment_logs.entry(pid).or_default();
                        log.entries = entries;
                        log.loaded = true;
                        log.error = None;
                        this.clear_save_error(SaveTarget::Accomplishments);
                    }
                    Err(e) => this.report_save_error(SaveTarget::Accomplishments, format!("{e:#}")),
                }
                // A load that read the file before this write would land stale.
                if this.accomplishments_loading.contains(&pid) {
                    this.accomplishments_stale.insert(pid);
                }
                this.accomplishments_generation += 1;
                cx.notify();
            });
        })
        .detach();
    }

    /// Log an accomplishment from the panel's quick-add box.
    pub(super) fn add_accomplishment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.accomplishments_pid else {
            return;
        };
        let input = self.accomplishments_add_input.read(cx).value().to_string();
        let Some((kind, summary)) = accomplishments::parse_user_input(&input) else {
            return;
        };
        let entry = Accomplishment::new(kind, summary, None, local_now_secs());
        self.accomplishments_add_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.edit_accomplishments(pid, move |entries| entries.push(entry), cx);
    }

    fn delete_accomplishment(&mut self, pid: Uuid, target: Accomplishment, cx: &mut Context<Self>) {
        self.accomplishments_confirm_delete = None;
        self.edit_accomplishments(
            pid,
            move |entries| {
                if let Some(i) = entries.iter().position(|e| *e == target) {
                    entries.remove(i);
                }
            },
            cx,
        );
        cx.notify();
    }

    /// Toggle the docked panel for `pid` (hides it if it's already showing that
    /// project). Mirrors [`Self::toggle_memory_panel`].
    pub(super) fn toggle_accomplishments_panel(
        &mut self,
        pid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.show_accomplishments && self.accomplishments_pid == Some(pid) {
            self.show_accomplishments = false;
            cx.notify();
            return;
        }
        self.open_accomplishments_panel(pid, window, cx);
    }

    /// Show `pid`'s log in the docked panel. Idempotent.
    pub(super) fn open_accomplishments_panel(
        &mut self,
        pid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_accomplishments = true;
        // The docked panels share one slot.
        self.show_file_browser = false;
        self.show_memory = false;
        if self.accomplishments_pid != Some(pid) {
            for input in [
                &self.accomplishments_search,
                &self.accomplishments_add_input,
            ] {
                input.update(cx, |s, cx| s.set_value("", window, cx));
            }
        }
        self.accomplishments_pid = Some(pid);
        self.accomplishments_confirm_delete = None;
        self.reload_accomplishments(vec![pid], cx);
    }

    /// From the overview: go to `pid` and open its panel.
    pub(crate) fn show_project_accomplishments(
        &mut self,
        pid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_accomplishments_overview(cx);
        self.select_project(pid, window, cx);
        self.open_accomplishments_panel(pid, window, cx);
    }

    /// Show/hide the all-projects overview in place of the pane area.
    pub(super) fn toggle_accomplishments_overview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_main_view(super::hotbar::MainView::Accomplishments, window, cx);
    }

    /// Create the embedded overview's view the first time it's shown.
    pub(super) fn ensure_accomplishments_overview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.accomplishments_overview.is_none() {
            let app = cx.weak_entity();
            let snapshot = self.accomplishments_snapshot();
            self.accomplishments_overview =
                Some(cx.new(|cx| AccomplishmentsOverview::new(app, snapshot, false, window, cx)));
        }
    }

    pub(crate) fn close_accomplishments_overview(&mut self, cx: &mut Context<Self>) {
        if self.main_view == super::hotbar::MainView::Accomplishments {
            self.main_view = super::hotbar::MainView::Panes;
        }
        cx.notify();
    }

    /// Open the overview in its own window (or bring the open one forward).
    pub(crate) fn pop_out_accomplishments(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.accomplishments_window {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.accomplishments_window = None;
        }
        let app = cx.weak_entity();
        let snapshot = self.accomplishments_snapshot();
        let bounds = Bounds::centered(None, size(px(560.0), px(720.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(360.0), px(320.0))),
                ..crate::window_options()
            },
            move |window, cx| {
                ui_profile::register_profile_window(
                    ui_profile::ProfileWindowKind::Auxiliary,
                    window,
                );
                window.set_window_title("muxel — accomplishments");
                // Renders from `snapshot`; it must not read the app, whose update
                // this runs inside.
                let view =
                    cx.new(|cx| AccomplishmentsOverview::new(app, snapshot, true, window, cx));
                cx.new(|cx| gpui_component::Root::new(view, window, cx).bg(cx.theme().background))
            },
        );
        if let Ok(handle) = opened {
            self.accomplishments_window = Some(handle);
            self.refresh_all_accomplishments(cx);
        }
    }

    /// What the overview shows, copied out of the cache.
    pub(crate) fn accomplishments_snapshot(&self) -> OverviewSnapshot {
        OverviewSnapshot {
            generation: self.accomplishments_generation,
            retention_days: accomplishments::clamp_retention(
                self.settings.accomplishment_retention_days,
            ),
            projects: self
                .accomplishment_projects()
                .map(|p| {
                    let log = self.accomplishment_logs.get(&p.id);
                    OverviewProject {
                        id: p.id,
                        name: p.name.clone(),
                        entries: log.map(|l| l.entries.clone()).unwrap_or_default(),
                        error: log.and_then(|l| l.error.clone()),
                        loading: !log.is_some_and(|l| l.loaded),
                    }
                })
                .collect(),
        }
    }

    /// Whether `snap` still matches the app: no new log data, same retention, and
    /// the same log-enabled projects under the same names. Cheap (no cloning), so
    /// overviews can ask on every app notify.
    pub(crate) fn accomplishments_snapshot_is_current(&self, snap: &OverviewSnapshot) -> bool {
        snap.generation == self.accomplishments_generation
            && snap.retention_days
                == accomplishments::clamp_retention(self.settings.accomplishment_retention_days)
            && snap
                .projects
                .iter()
                .map(|p| (p.id, p.name.as_str()))
                .eq(self
                    .accomplishment_projects()
                    .map(|p| (p.id, p.name.as_str())))
    }

    /// Toggle the project's accomplishment log; on enable, seed the file and
    /// git-ignore `.muxel/`. New agents in the project then get the instruction.
    pub(super) fn toggle_project_accomplishments(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        let Some(p) = self.workspace.project_mut(pid) else {
            return;
        };
        p.accomplishments_enabled = !p.accomplishments_enabled;
        if p.accomplishments_enabled {
            self.accomplishments_ensured.remove(&pid);
            self.ensure_project_accomplishments(pid, cx);
            self.accomplishments_checked_at = None; // pick it up on the next tick
        }
        self.accomplishments_generation += 1;
        self.persist();
        cx.notify();
    }

    /// Ensure the project's `.muxel/ACCOMPLISHMENTS.md` and `.gitignore` entry
    /// exist, off the UI thread (remote is an SSH round-trip).
    pub(super) fn ensure_project_accomplishments(&self, pid: Uuid, cx: &mut Context<Self>) {
        let Some(loc) = self.repo_loc(pid) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { integrations::ensure_accomplishments_file(&loc) })
                .await;
            if let Err(e) = res {
                let _ = this.update(cx, |this, cx| {
                    let msg = format!("{e}");
                    if !this.handle_ssh_error(&msg, None, SshRetry::None, cx) {
                        this.add_event(NotifKind::Error, t("Accomplishment log"), msg);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Open the raw `ACCOMPLISHMENTS.md` in the editor, creating it if needed.
    fn open_accomplishments_file(
        &mut self,
        pid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(p) = self.workspace.project(pid) else {
            return;
        };
        let is_remote = p.remote.is_some();
        let root = match &p.remote {
            Some(r) => r.remote_root.clone(),
            None => p.root_path.display().to_string(),
        };
        let path = PathBuf::from(format!("{root}/{MEMORY_DIR}/{ACCOMPLISHMENTS_FILE}"));
        let target = self.active_instance;
        if is_remote {
            self.ensure_project_accomplishments(pid, cx);
        } else if let Some(loc) = self.repo_loc(pid)
            && let Err(e) = integrations::ensure_accomplishments_file(&loc)
        {
            self.add_event(NotifKind::Error, t("Accomplishment log"), format!("{e}"));
        }
        self.open_editor_at(pid, Some(path), target, window, cx);
    }

    /// Step the retention window by `delta` days (Settings), pruning open logs.
    pub(super) fn adjust_accomplishment_retention(&mut self, delta: i64, cx: &mut Context<Self>) {
        let current = i64::from(self.settings.accomplishment_retention_days);
        let next = u32::try_from((current + delta).max(1)).unwrap_or(1);
        self.set_accomplishment_retention(next, cx);
    }

    pub(super) fn set_accomplishment_retention(&mut self, days: u32, cx: &mut Context<Self>) {
        let days = accomplishments::clamp_retention(days);
        if days == self.settings.accomplishment_retention_days {
            return;
        }
        self.settings.accomplishment_retention_days = days;
        self.persist_settings();
        self.accomplishments_generation += 1;
        // Every log is due again under the new window, starting on the next tick.
        self.accomplishments_maintained.clear();
        self.accomplishments_checked_at = None;
        cx.notify();
    }

    /// The docked accomplishment panel: header, kind totals, search, the log
    /// grouped by day (newest first) with delete, and the quick-add box.
    pub(super) fn render_accomplishments_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(pid) = self.accomplishments_pid else {
            return div().into_any_element();
        };
        let pname = self
            .workspace
            .project(pid)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let enabled = self
            .workspace
            .project(pid)
            .is_some_and(|p| p.accomplishments_enabled);
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let query = self
            .accomplishments_search
            .read(cx)
            .value()
            .trim()
            .to_lowercase();
        let today = local_now_secs();
        let log = self.accomplishment_logs.get(&pid);
        let entries: &[Accomplishment] = log.map(|l| l.entries.as_slice()).unwrap_or_default();
        let visible: Vec<&Accomplishment> = entries.iter().filter(|e| e.matches(&query)).collect();

        let mut list = div()
            .id("acc-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2()
            .py_1()
            .flex()
            .flex_col()
            .gap_3();
        if let Some(err) = log.and_then(|l| l.error.as_ref()) {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(tf("Couldn't load the log: {error}", &[("error", err)])),
            );
        }
        if !enabled {
            list = list.child(div().text_xs().text_color(muted).child(t(
                "The log is off for this project, so agents aren't adding to it. Turn it \
                 on from the project's right-click menu or Settings → Projects.",
            )));
        }
        if !log.is_some_and(|l| l.loaded) {
            list = list.child(div().text_xs().text_color(muted).child(t("Loading…")));
        } else if entries.is_empty() {
            list = list.child(div().text_xs().text_color(muted).child(t(
                "Nothing logged yet. Agents add a line here each time they finish a \
                 feature, fix, or other piece of work.",
            )));
        } else if visible.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t("No entries match your search.")),
            );
        }
        for (day, items) in accomplishments::group_by_day(visible, |e| e.date) {
            let mut day_col = div().flex().flex_col().gap_1().child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .pb_1()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .child(day_heading(day, today)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(items.len().to_string()),
                    ),
            );
            for (i, e) in items.into_iter().enumerate() {
                let confirming = self.accomplishments_confirm_delete.as_ref() == Some(e);
                let key = format!("{day}-{i}");
                let actions = if confirming {
                    let target = e.clone();
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(div().text_xs().text_color(muted).child(t("Delete?")))
                        .child(
                            Button::new(SharedString::from(format!("acc-del-yes-{key}")))
                                .danger()
                                .xsmall()
                                .icon(IconName::Check)
                                .tooltip(t("Confirm delete"))
                                .on_click(cx.listener(move |this, _e, _w, cx| {
                                    this.delete_accomplishment(pid, target.clone(), cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("acc-del-no-{key}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip(t("Cancel"))
                                .on_click(cx.listener(|this, _e, _w, cx| {
                                    this.accomplishments_confirm_delete = None;
                                    cx.notify();
                                })),
                        )
                } else {
                    let target = e.clone();
                    div().flex_none().child(
                        Button::new(SharedString::from(format!("acc-del-{key}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Delete)
                            .tooltip(t("Delete"))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.accomplishments_confirm_delete = Some(target.clone());
                                cx.notify();
                            })),
                    )
                };
                day_col = day_col.child(
                    div()
                        .flex()
                        .items_start()
                        .gap_1()
                        .p_1()
                        .rounded(cx.theme().radius)
                        .hover(|s| s.bg(cx.theme().secondary))
                        .child(entry_row(e, muted).flex_1().min_w_0())
                        .child(actions),
                );
            }
            list = list.child(day_col);
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
                    .child(tf(
                        "ACCOMPLISHMENTS · {proj_name}",
                        &[("proj_name", &pname)],
                    )),
            )
            .child(
                Button::new("acc-refresh")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/refresh.svg"))
                    .tooltip(t("Refresh"))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.reload_accomplishments(vec![pid], cx)
                    })),
            )
            .child(
                Button::new("acc-open-file")
                    .ghost()
                    .xsmall()
                    .icon(IconName::File)
                    .tooltip(t("Open ACCOMPLISHMENTS.md in editor"))
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.open_accomplishments_file(pid, window, cx)
                    })),
            )
            .child(
                Button::new("acc-expand")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Maximize)
                    .tooltip(t("Show all projects"))
                    .on_click(cx.listener(|this, _e, window, cx| {
                        this.set_main_view(super::hotbar::MainView::Accomplishments, window, cx)
                    })),
            )
            .child(
                Button::new("acc-popout")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ExternalLink)
                    .tooltip(t("Pop out all projects into a window"))
                    .on_click(cx.listener(|this, _e, _w, cx| this.pop_out_accomplishments(cx))),
            )
            .child(
                Button::new("acc-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(t("Close accomplishments panel"))
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.show_accomplishments = false;
                        cx.notify();
                    })),
            );

        let totals = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .pt_2()
            .child(
                div().text_xs().text_color(muted).child(tf(
                    "Last {days} days · {count} logged",
                    &[
                        (
                            "days",
                            &accomplishments::clamp_retention(
                                self.settings.accomplishment_retention_days,
                            )
                            .to_string(),
                        ),
                        ("count", &entries.len().to_string()),
                    ],
                )),
            )
            .child(kind_counts_row(entries));

        let add_form = div()
            .flex()
            .items_center()
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.accomplishments_add_input).w_full()),
            )
            .child(
                Button::new("acc-add")
                    .primary()
                    .xsmall()
                    .icon(IconName::Plus)
                    .tooltip(t("Log it"))
                    .on_click(
                        cx.listener(|this, _e, window, cx| this.add_accomplishment(window, cx)),
                    ),
            );

        v_flex()
            .size_full()
            .min_w_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(border)
            .child(header)
            .child(totals)
            .child(
                div()
                    .px_2()
                    .py_1()
                    .child(Input::new(&self.accomplishments_search).w_full()),
            )
            .child(list)
            .child(add_form)
            .into_any_element()
    }
}
