//! When diff panes run `git diff`, and how they learn their files changed.
//!
//! [`DiffSchedule`] decides when each pane is due. This keeps it fed: which
//! panes are on screen (checked every frame), what the file watcher saw, and
//! when a refresh lands. A pane's first refresh resolves its repository's
//! [`DiffPlan`] and watches what [`diff_sources`] builds from it, so a
//! visible pane runs git when its tracked files or HEAD's branch change. One
//! that can't be watched refreshes every 1.5 s while visible, as before.
//! Each refresh of a watched pane first checks HEAD and the index against
//! its plan's [`PlanInputs`], and a watcher overflow re-plans too, so a lost
//! event can't leave the plan filtering by stale tracked files.

use super::MuxelApp;
use super::watching::WatchTarget;
use crate::file_watch::diff_sources;
use crate::integrations::{self, DiffPlan, PlanInputs};
use crate::ui_profile;
use gpui::{Context, Task, WeakEntity};
use muxel_core::diff_schedule::DiffSchedule;
use muxel_core::file_watch::{WatchId, WatchSpec};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;
use uuid::Uuid;

/// Refresh state for every open diff pane.
#[derive(Default)]
pub(super) struct DiffPanes {
    pub(super) schedule: DiffSchedule,
    pub(super) sources: HashMap<Uuid, DiffSource>,
    /// The diff panes open, and those on screen, at the last frame.
    open: HashSet<Uuid>,
    visible: HashSet<Uuid>,
    /// When the timer below wakes; `None` while nothing is due without news.
    armed: Option<Instant>,
    timer: Option<Task<()>>,
}

/// What one pane's refreshes read.
pub(super) struct DiffSource {
    dir: PathBuf,
    /// The repository's top level from the last [`DiffPlan`]. `None` before
    /// the first refresh, or where `dir` isn't in a work tree.
    toplevel: Option<String>,
    /// What the last plan was read from (see [`PlanInputs`]).
    inputs: PlanInputs,
    /// Resolve the plan again at the next refresh: none yet, HEAD or the
    /// index changed, a tracked directory came or went, the watcher lost
    /// events, or `dir` was gone.
    pub(super) replan: bool,
    pub(super) watch: Option<WatchId>,
}

/// One refresh, run off the UI thread.
struct DiffJob {
    pane: Uuid,
    dir: PathBuf,
    toplevel: Option<String>,
    /// The watched plan's inputs, checked before reusing it; `None` for a
    /// pane that polls, which doesn't need its plan.
    inputs: Option<PlanInputs>,
    replan: bool,
}

struct DiffResult {
    pane: Uuid,
    text: String,
    /// The top level this refresh resolved, what its plan was read from,
    /// and the watch built from it; `None` when it reused the pane's.
    planned: Option<(Option<String>, PlanInputs, Option<WatchSpec>)>,
    /// `dir` was gone: resolve the plan once it's back.
    gone: bool,
}

impl DiffJob {
    fn run(self) -> DiffResult {
        // A folder that's gone runs no git while it stays gone.
        if !self.dir.is_dir() {
            return DiffResult {
                pane: self.pane,
                text: integrations::not_a_git_repo(&self.dir),
                planned: Some((None, PlanInputs::default(), None)),
                gone: true,
            };
        }
        // A watched plan whose HEAD or index moved unreported (a lost
        // event) would keep filtering by the old tracked files.
        let stale = self.inputs.as_ref().is_some_and(|inputs| !inputs.current());
        let (toplevel, planned) = if self.replan || stale {
            let plan: Option<DiffPlan> = integrations::diff_plan(&self.dir);
            let spec = plan.as_ref().and_then(|plan| diff_sources(&self.dir, plan));
            let inputs = plan
                .as_ref()
                .map(|plan| plan.inputs.clone())
                .unwrap_or_default();
            let toplevel = plan.map(|plan| plan.toplevel);
            (toplevel.clone(), Some((toplevel, inputs, spec)))
        } else {
            (self.toplevel, None)
        };
        DiffResult {
            pane: self.pane,
            text: integrations::git_diff(&self.dir, toplevel.as_deref()),
            planned,
            gone: false,
        }
    }
}

impl MuxelApp {
    /// Run each frame: tell the schedule which diff panes are open and on
    /// screen, docked in a shown tab or popped out.
    pub(super) fn sync_diff_visibility(&mut self, cx: &mut Context<Self>) {
        let diffs: Vec<_> = self
            .editor_views()
            .filter_map(|(iid, editor)| {
                let editor = editor.read(cx);
                editor
                    .is_diff()
                    .then(|| editor.diff_dir().map(|dir| (iid, dir.to_path_buf())))
                    .flatten()
            })
            .collect();
        // Walk the layout only when there's a diff pane to place.
        if diffs.is_empty() && self.diff_panes.open.is_empty() {
            return;
        }
        let open: HashSet<Uuid> = diffs.iter().map(|(iid, _)| *iid).collect();
        let shown: HashSet<Uuid> = self.shown_layout_tabs().into_iter().collect();
        let visible: HashSet<Uuid> = open
            .iter()
            .filter(|iid| shown.contains(iid) || self.popouts.contains_key(iid))
            .copied()
            .collect();
        if open == self.diff_panes.open && visible == self.diff_panes.visible {
            return;
        }
        for (iid, dir) in diffs {
            self.diff_panes
                .sources
                .entry(iid)
                .or_insert_with(|| DiffSource {
                    dir,
                    toplevel: None,
                    inputs: PlanInputs::default(),
                    replan: true,
                    watch: None,
                });
        }
        let closed: Vec<Uuid> = self
            .diff_panes
            .sources
            .keys()
            .filter(|iid| !open.contains(iid))
            .copied()
            .collect();
        for iid in closed {
            if let Some(id) = self.diff_panes.sources.remove(&iid).and_then(|s| s.watch) {
                self.drop_watch(id);
            }
            self.diff_panes.schedule.remove(iid);
        }
        for iid in &open {
            self.diff_panes
                .schedule
                .set_visible(*iid, visible.contains(iid));
        }
        self.diff_panes.open = open;
        self.diff_panes.visible = visible;
        self.pump_diffs(cx);
    }

    /// Refresh `pane` at the first chance: the refresh button, or reopening
    /// it. It waits for one already running.
    pub(super) fn request_diff(&mut self, pane: Uuid, cx: &mut Context<Self>) {
        self.diff_panes.schedule.request(pane);
        self.pump_diffs(cx);
    }

    /// Start every refresh that's due, and wake again when the next one is.
    pub(super) fn pump_diffs(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let due = self.diff_panes.schedule.take_due(now);
        if !due.is_empty() {
            self.refresh_diffs(due, cx);
        }
        let next = self.diff_panes.schedule.next_due(now);
        if next == self.diff_panes.armed {
            return;
        }
        self.diff_panes.armed = next;
        self.diff_panes.timer = next.map(|at| {
            cx.spawn(async move |this: WeakEntity<Self>, cx| {
                cx.background_executor()
                    .timer(at.saturating_duration_since(now))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.diff_panes.armed = None;
                    this.pump_diffs(cx);
                });
            })
        });
    }

    /// Run `git diff` for `panes` on a background thread, then show each
    /// result in its pane, in whichever window holds it.
    fn refresh_diffs(&mut self, panes: Vec<Uuid>, cx: &mut Context<Self>) {
        let mut jobs = Vec::with_capacity(panes.len());
        for pane in panes {
            match self.diff_panes.sources.get_mut(&pane) {
                Some(source) => jobs.push(DiffJob {
                    pane,
                    dir: source.dir.clone(),
                    toplevel: source.toplevel.clone(),
                    inputs: source.watch.is_some().then(|| source.inputs.clone()),
                    replan: std::mem::take(&mut source.replan),
                }),
                // Asked for before its first frame; it rejoins once it's on
                // screen.
                None => self.diff_panes.schedule.remove(pane),
            }
        }
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    let _git = ui_profile::git_caller("diff-pane");
                    jobs.into_iter().map(DiffJob::run).collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for result in results {
                    this.land_diff(result, cx);
                }
                this.pump_diffs(cx);
            });
        })
        .detach();
    }

    fn land_diff(&mut self, result: DiffResult, cx: &mut Context<Self>) {
        let pane = result.pane;
        let now = cx.background_executor().now();
        self.diff_panes.schedule.landed(pane, now);
        let Some(source) = self.diff_panes.sources.get_mut(&pane) else {
            return; // closed meanwhile
        };
        source.replan |= result.gone;
        if let Some((toplevel, inputs, spec)) = result.planned {
            source.toplevel = toplevel;
            source.inputs = inputs;
            self.watch_diff(pane, spec, cx);
        }
        let editor = self
            .editor_views()
            .find_map(|(iid, editor)| (iid == pane).then(|| editor.clone()));
        let (Some(editor), Some(handle)) = (
            editor,
            self.editor_secondary_window(pane).or(self.main_window),
        ) else {
            return;
        };
        // Runs from a task, never inside a window update, so updating the
        // pane's own window can't nest.
        let text = result.text;
        let _ = handle.update(cx, move |_root, window, cx| {
            editor.update(cx, |editor, cx| editor.set_diff_content(text, window, cx));
        });
    }

    /// Point `pane`'s watch at `spec`, or stop watching where there's none.
    fn watch_diff(&mut self, pane: Uuid, spec: Option<WatchSpec>, cx: &mut Context<Self>) {
        let Some(source) = self.diff_panes.sources.get_mut(&pane) else {
            return;
        };
        let (old, dir) = (source.watch.take(), source.dir.clone());
        let watch = match (old, spec) {
            (Some(id), Some(spec)) => {
                let respec = match self.file_watch.as_mut() {
                    Some(Some(watch)) => watch.respec(id, spec),
                    _ => Err(anyhow::anyhow!("the file watcher stopped")),
                };
                match respec {
                    Ok(()) => Some(id),
                    Err(error) => {
                        log::warn!("diff pane for {} stopped watching: {error}", dir.display());
                        ui_profile::diff_watch("lost");
                        self.watch_targets.remove(&id);
                        None
                    }
                }
            }
            // The watcher failed earlier in this run, and said so then.
            (None, Some(_)) if matches!(self.file_watch, Some(None)) => None,
            (None, Some(spec)) => match self.watch(spec, WatchTarget::Diff(pane), cx) {
                Ok(id) => {
                    ui_profile::diff_watch("watching");
                    // Writes between running git and this watch went unseen.
                    self.diff_panes.schedule.changed(pane, None);
                    Some(id)
                }
                Err(error) => {
                    let spent = error.downcast_ref::<notify::Error>().is_some_and(|error| {
                        matches!(error.kind, notify::ErrorKind::MaxFilesWatch)
                    });
                    log::warn!(
                        "diff pane for {} polls while visible: {error}{}",
                        dir.display(),
                        if spent {
                            " (fs.inotify.max_user_watches is spent)"
                        } else {
                            ""
                        }
                    );
                    ui_profile::diff_watch("polling");
                    None
                }
            },
            (Some(id), None) => {
                self.drop_watch(id);
                ui_profile::diff_watch("polling");
                None
            }
            (None, None) => {
                ui_profile::diff_watch("polling");
                None
            }
        };
        if let Some(source) = self.diff_panes.sources.get_mut(&pane) {
            source.watch = watch;
        }
        self.diff_panes.schedule.set_watched(pane, watch.is_some());
    }

    /// `pane`'s watch is gone: it polls while visible.
    pub(super) fn diff_unwatched(&mut self, pane: Uuid) {
        if let Some(source) = self.diff_panes.sources.get_mut(&pane) {
            source.watch = None;
        }
        ui_profile::diff_watch("lost");
        self.diff_panes.schedule.set_watched(pane, false);
    }
}
