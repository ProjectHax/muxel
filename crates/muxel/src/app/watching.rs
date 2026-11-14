//! The app's side of its one file watcher ([`FileWatch`]): starting it with
//! the first registration, and handing each registration's news to what it
//! wakes. A target whose watch is lost polls for the rest of the run (a diff
//! pane, until it re-plans), as it would have without one.

use super::MuxelApp;
use crate::file_watch::FileWatch;
use crate::ui_profile;
use gpui::Context;
use muxel_core::file_watch::{WatchId, WatchSpec};
use muxel_core::session_discovery::SessionProvider;
use uuid::Uuid;

/// What a registration wakes.
#[derive(Clone, Copy, Debug)]
pub(super) enum WatchTarget {
    /// A provider's session discovery.
    Provider(SessionProvider),
    /// A diff pane's refresh schedule.
    Diff(Uuid),
}

impl MuxelApp {
    /// Register `spec` for `target`, starting the watcher with the first.
    pub(super) fn watch(
        &mut self,
        spec: WatchSpec,
        target: WatchTarget,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<WatchId> {
        if self.file_watch.is_none() {
            match FileWatch::start() {
                Ok((watch, signal)) => self.follow_file_watch(watch, signal, cx),
                Err(error) => {
                    self.file_watch = Some(None);
                    return Err(error);
                }
            }
        }
        let Some(Some(watch)) = self.file_watch.as_mut() else {
            anyhow::bail!("the file watcher stopped earlier in this run");
        };
        let id = watch.register(spec)?;
        self.watch_targets.insert(id, target);
        Ok(id)
    }

    /// Watch what `spec` builds for `provider`'s discovery, which then polls
    /// only as a fallback. Tried once a run: on failure the provider polls.
    pub(super) fn watch_provider(
        &mut self,
        provider: SessionProvider,
        spec: impl FnOnce() -> anyhow::Result<WatchSpec>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let watched = spec().and_then(|spec| self.watch(spec, WatchTarget::Provider(provider), cx));
        let id = watched.as_ref().ok().copied();
        self.provider_watches.insert(provider, id);
        if id.is_some() {
            self.session_discoveries.set_watched(provider, true);
            ui_profile::session_watch(provider, "watching");
        } else {
            ui_profile::session_watch(provider, "failed");
        }
        watched.map(|_| ())
    }

    /// Use `watch` as the app's watcher, and hand out its news each time
    /// `signal` fires.
    pub(super) fn follow_file_watch(
        &mut self,
        watch: FileWatch,
        signal: async_channel::Receiver<()>,
        cx: &mut Context<Self>,
    ) {
        self.file_watch = Some(Some(watch));
        cx.spawn(async move |this, cx| {
            while signal.recv().await.is_ok() {
                if this
                    .update(cx, |this, cx| this.apply_file_watch_news(cx))
                    .is_err()
                {
                    return;
                }
            }
            // The sender went with the backend's handler: dropped here after
            // a failure, or by a backend that stopped without reporting one.
            let _ = this.update(cx, |this, cx| {
                if matches!(this.file_watch, Some(Some(_))) {
                    log::warn!("file watcher stopped; polling instead");
                    this.stop_file_watch(cx);
                }
            });
        })
        .detach();
    }

    fn apply_file_watch_news(&mut self, cx: &mut Context<Self>) {
        let Some(Some(watch)) = &self.file_watch else {
            return;
        };
        let drained = watch.take();
        let now = cx.background_executor().now();
        let mut lost = Vec::new();
        for (id, change) in drained.changes {
            let Some(target) = self.watch_targets.get(&id).copied() else {
                continue;
            };
            match target {
                WatchTarget::Provider(provider) => {
                    for pane in change.hits {
                        self.session_discoveries.wake(provider, pane);
                    }
                    if change.lost {
                        lost.push(id);
                    } else if change.rescan {
                        self.session_discoveries.wake_provider(provider);
                        ui_profile::session_watch(provider, "rescan");
                    }
                }
                WatchTarget::Diff(pane) => {
                    if change.lost {
                        lost.push(id);
                        continue;
                    }
                    if change.touched {
                        self.diff_panes.schedule.changed(pane, Some(now));
                    }
                    if change.rescan {
                        self.diff_panes.schedule.changed(pane, None);
                    }
                    // Lost events may include HEAD's or the index's.
                    if (change.relist || change.rescan)
                        && let Some(source) = self.diff_panes.sources.get_mut(&pane)
                    {
                        source.replan = true;
                    }
                }
            }
        }
        if drained.failed {
            self.stop_file_watch(cx);
            return;
        }
        for id in lost {
            self.lose_watch(id);
        }
        self.sync_provider_session_ids(cx);
        self.pump_diffs(cx);
    }

    /// Drop `id` and forget its target, which its owner has dealt with.
    pub(super) fn drop_watch(&mut self, id: WatchId) -> Option<WatchTarget> {
        if let Some(Some(watch)) = self.file_watch.as_mut() {
            watch.unregister(id);
        }
        self.watch_targets.remove(&id)
    }

    /// `id` is blind: drop it, and its target polls for the rest of the run.
    fn lose_watch(&mut self, id: WatchId) {
        if let Some(target) = self.drop_watch(id) {
            self.unwatch_target(target);
        }
    }

    fn unwatch_target(&mut self, target: WatchTarget) {
        match target {
            WatchTarget::Provider(provider) => {
                self.provider_watches.insert(provider, None);
                self.session_discoveries.set_watched(provider, false);
                ui_profile::session_watch(provider, "failed");
            }
            WatchTarget::Diff(pane) => self.diff_unwatched(pane),
        }
    }

    /// The watcher failed or its backend stopped. Not trusted again in this
    /// run: every target goes back to polling, and every quiet pane looks now.
    fn stop_file_watch(&mut self, cx: &mut Context<Self>) {
        self.file_watch = Some(None);
        for (_, target) in std::mem::take(&mut self.watch_targets) {
            self.unwatch_target(target);
        }
        self.sync_provider_session_ids(cx);
        self.pump_diffs(cx);
    }
}
