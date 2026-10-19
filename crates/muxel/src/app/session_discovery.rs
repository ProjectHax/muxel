//! The UI half captures identities and applies guarded results. All provider
//! files and process enumeration are read by the background task below.

use super::{
    MuxelApp, PaneView, home_dir, is_claude_program, is_codex_program, is_grok_program,
    session_id_bound_elsewhere, validated_codex_session_id,
};
#[cfg(windows)]
use super::{active_grok_session_for_process, process_parent_map, read_grok_active_sessions};
use crate::{session_binding, ui_profile};
use gpui::{App, Context};
use muxel_core::AgentPreset;
use muxel_core::session_discovery::{
    SessionDiscoveryKey, SessionDiscoveryRequest, SessionProvider,
};
use std::path::Path;
use std::time::Instant;
use uuid::Uuid;

struct SessionBindingUpdate {
    pane: Uuid,
    provider: SessionProvider,
    previous_id: Option<String>,
    previous_started: bool,
    previous_auto_name: Option<String>,
}

impl MuxelApp {
    /// Bump on each binding mutation, including launch-time changes and rollback.
    /// A rebind away and back must still invalidate an older worker.
    pub(super) fn bump_session_binding_generation(&mut self, iid: Uuid) {
        self.session_discoveries.binding_changed(iid);
    }

    fn discovery_terminal(&self, iid: Uuid) -> Option<&gpui::Entity<muxel_terminal::TerminalView>> {
        self.terminals.get(&iid).or_else(|| {
            self.popouts
                .get(&iid)
                .and_then(|popout| match &popout.view {
                    PaneView::Terminal(view) => Some(view),
                    _ => None,
                })
        })
    }

    fn session_discovery_key(&self, iid: Uuid, cx: &App) -> Option<SessionDiscoveryKey> {
        let instance = self.workspace.instance(iid)?;
        self.session_discovery_key_for_instance(instance, cx)
    }

    fn session_discovery_key_for_instance(
        &self,
        instance: &muxel_core::Instance,
        cx: &App,
    ) -> Option<SessionDiscoveryKey> {
        let iid = instance.id;
        let project = self.workspace.project(instance.project_id)?;
        if instance.literal_argv
            || project.remote.is_some()
            || self.terminal_launching.contains_key(&iid)
        {
            return None;
        }
        let view = self.discovery_terminal(iid)?.read(cx);
        if view.exited() {
            return None;
        }
        let preset = instance
            .preset_id
            .and_then(|id| self.presets.iter().find(|preset| preset.id == id))
            .or_else(|| {
                self.presets
                    .iter()
                    .find(|preset| preset.name == instance.preset)
            });
        let (provider, hint) = if let Some(preset) =
            preset.filter(|preset| is_codex_program(preset.program.as_deref()))
        {
            let hint = muxel_core::codex_session_hint_from_title(preset, &view.session_id_hint()?)?;
            let prefix = hint.strip_suffix("...").unwrap_or(&hint);
            if instance
                .session_id
                .as_deref()
                .is_some_and(|id| id.starts_with(prefix))
            {
                return None;
            }
            (SessionProvider::Codex, Some(hint))
        } else if is_claude_program(instance.program.as_deref()) {
            (SessionProvider::Claude, None)
        } else if cfg!(windows)
            && is_grok_program(instance.program.as_deref())
            && instance.tmux_session.is_none()
            && view.child_pid().is_some()
        {
            (SessionProvider::Grok, None)
        } else {
            return None;
        };
        Some(SessionDiscoveryKey {
            pane: iid,
            provider,
            launch: self.terminal_launches.get(&iid)?.0,
            child_pid: view.child_pid(),
            binding_generation: self.session_discoveries.binding_generation(iid),
            binding: instance.session_id.clone(),
            cwd: instance
                .working_directory()
                .unwrap_or(&project.root_path)
                .to_path_buf(),
            hint,
        })
    }

    pub(super) fn sync_provider_session_ids(&mut self, cx: &mut Context<Self>) {
        let Some(home) = home_dir() else { return };
        let data_dir = muxel_store::data_dir();
        let now = Instant::now();
        let keys: Vec<_> = self
            .workspace
            .instances
            .iter()
            .filter_map(|instance| self.session_discovery_key_for_instance(instance, cx))
            .collect();
        self.session_discoveries.reconcile(&self.workspace);
        let requests: Vec<_> = keys
            .into_iter()
            .filter_map(|key| self.session_discoveries.begin(key, now))
            .collect();
        // Keep fast hook/process polls independent of a slow rollout walk.
        let mut groups: [Vec<SessionDiscoveryRequest>; 3] = Default::default();
        for request in requests {
            let index = match request.key.provider {
                SessionProvider::Codex => 0,
                SessionProvider::Claude => 1,
                SessionProvider::Grok => 2,
            };
            groups[index].push(request);
        }
        for requests in groups.into_iter().filter(|requests| !requests.is_empty()) {
            let home = home.clone();
            let data_dir = data_dir.clone();
            cx.spawn(async move |this, cx| {
                let results = cx
                    .background_executor()
                    .spawn(async move { discover_sessions(requests, &home, data_dir.as_deref()) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    let mut updates = Vec::new();
                    for (request, candidate) in results {
                        let iid = request.key.pane;
                        let current = this.session_discovery_key(iid, cx);
                        let unowned = candidate.as_deref().is_some_and(|id| {
                            !session_id_bound_elsewhere(&this.workspace.instances, iid, id)
                        });
                        let valid = this.session_discoveries.finish(
                            &request,
                            current.as_ref(),
                            Instant::now(),
                            unowned,
                        );
                        if valid
                            && unowned
                            && let Some(session_id) = candidate
                            && let Some(update) =
                                this.stage_discovered_session(iid, request.key.provider, session_id)
                        {
                            updates.push(update);
                        }
                    }
                    this.finish_discovered_sessions(updates, cx);
                });
            })
            .detach();
        }
    }

    fn stage_discovered_session(
        &mut self,
        iid: Uuid,
        provider: SessionProvider,
        session_id: String,
    ) -> Option<SessionBindingUpdate> {
        let instance = self.workspace.instance_mut(iid)?;
        if instance.session_id.as_deref() == Some(session_id.as_str()) {
            return None;
        }
        let update = SessionBindingUpdate {
            pane: iid,
            provider,
            previous_id: instance.session_id.clone(),
            previous_started: instance.session_started,
            previous_auto_name: instance.auto_name.clone(),
        };
        instance.session_id = Some(session_id);
        instance.session_started = true;
        instance.auto_name = None;
        self.bump_session_binding_generation(iid);
        Some(update)
    }

    fn finish_discovered_sessions(
        &mut self,
        updates: Vec<SessionBindingUpdate>,
        cx: &mut Context<Self>,
    ) {
        if updates.is_empty() {
            return;
        }
        // Preserve Grok's single save per poll and avoid serial workspace writes
        // when several panes become discoverable together.
        if self.try_persist() {
            for update in updates {
                if update.provider == SessionProvider::Grok
                    && let Some(view) = self.discovery_terminal(update.pane)
                {
                    view.read(cx).clear_title();
                }
            }
            cx.notify();
        } else {
            for update in updates {
                let instance = self
                    .workspace
                    .instance_mut(update.pane)
                    .expect("discovery owner exists during synchronous persistence");
                instance.session_id = update.previous_id;
                instance.session_started = update.previous_started;
                instance.auto_name = update.previous_auto_name;
                self.bump_session_binding_generation(update.pane);
            }
        }
        // Keep Claude's hook record. Removing it after an async read could erase
        // a newer hook, and the launch-time reader needs it if Muxel quits early.
    }
}

fn discover_sessions(
    requests: Vec<SessionDiscoveryRequest>,
    home: &Path,
    data_dir: Option<&Path>,
) -> Vec<(SessionDiscoveryRequest, Option<String>)> {
    #[cfg(windows)]
    let mut grok_snapshot = None;
    requests
        .into_iter()
        .map(|request| {
            let key = &request.key;
            let started = Instant::now();
            let candidate = match key.provider {
                SessionProvider::Codex => validated_codex_session_id(
                    &AgentPreset::codex(),
                    key.binding.as_deref(),
                    key.hint.as_deref(),
                    home,
                    &key.cwd,
                    Some(key.pane),
                ),
                SessionProvider::Claude => data_dir.and_then(|data_dir| {
                    session_binding::claude_session_id_from_binding(data_dir, key.pane, home)
                }),
                SessionProvider::Grok => {
                    #[cfg(windows)]
                    {
                        let (active, parents) = grok_snapshot.get_or_insert_with(|| {
                            (read_grok_active_sessions(), process_parent_map())
                        });
                        key.child_pid
                            .and_then(|pid| active_grok_session_for_process(pid, active, parents))
                    }
                    #[cfg(not(windows))]
                    {
                        None
                    }
                }
            };
            ui_profile::session_lookup_finished(
                ui_profile::SessionLookupKind::Discovery,
                Some(key.provider),
                key.pane,
                started.elapsed(),
                if candidate.is_some() {
                    ui_profile::SessionLookupOutcome::Found
                } else {
                    ui_profile::SessionLookupOutcome::Unresolved
                },
            );
            (request, candidate)
        })
        .collect()
}
