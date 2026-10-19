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
use muxel_core::session_discovery::{
    SessionDiscoveryKey, SessionDiscoveryRequest, SessionProvider,
};
use muxel_core::{AgentPreset, CodexHintMiss};
use std::path::{Path, PathBuf};
use std::time::Instant;
use uuid::Uuid;

// Explicit Import may search history. Restoration must never call this path.
pub(super) fn capture_codex_session_id(home: &Path, cwd: &Path) -> Option<String> {
    #[cfg(test)]
    let injected = CODEX_LAUNCH_TEST_LOOKUP.lock().unwrap().clone();
    #[cfg(test)]
    if let Some(discover) = injected {
        return discover(home, cwd);
    }
    muxel_core::codex_latest_session_id(home, cwd)
}

pub(super) fn resume_codex_session(
    home: &Path,
    id: &str,
    path: Option<&Path>,
) -> std::io::Result<Option<PathBuf>> {
    #[cfg(test)]
    let injected = CODEX_RESUME_TEST_LOOKUP.lock().unwrap().clone();
    #[cfg(test)]
    if let Some(discover) = injected {
        return discover(home, id, path);
    }
    muxel_core::codex_resume_path(home, id, path)
}

#[cfg(test)]
type CodexResumeTestLookup = std::sync::Arc<
    dyn Fn(&Path, &str, Option<&Path>) -> std::io::Result<Option<PathBuf>> + Send + Sync,
>;
#[cfg(test)]
static CODEX_RESUME_TEST_LOOKUP: std::sync::Mutex<Option<CodexResumeTestLookup>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
pub(super) fn inject_codex_resume_lookup(discover: CodexResumeTestLookup) -> CodexLaunchTestGuard {
    *CODEX_RESUME_TEST_LOOKUP.lock().unwrap() = Some(discover);
    CodexLaunchTestGuard
}

#[cfg(test)]
type CodexLaunchTestLookup = std::sync::Arc<dyn Fn(&Path, &Path) -> Option<String> + Send + Sync>;
#[cfg(test)]
static CODEX_LAUNCH_TEST_LOOKUP: std::sync::Mutex<Option<CodexLaunchTestLookup>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(super) struct CodexLaunchTestGuard;
#[cfg(test)]
impl Drop for CodexLaunchTestGuard {
    fn drop(&mut self) {
        *CODEX_LAUNCH_TEST_LOOKUP.lock().unwrap() = None;
        *CODEX_RESUME_TEST_LOOKUP.lock().unwrap() = None;
    }
}
#[cfg(test)]
pub(super) fn inject_codex_launch_lookup(discover: CodexLaunchTestLookup) -> CodexLaunchTestGuard {
    *CODEX_LAUNCH_TEST_LOOKUP.lock().unwrap() = Some(discover);
    CodexLaunchTestGuard
}

struct SessionBindingUpdate {
    pane: Uuid,
    provider: SessionProvider,
    previous_id: Option<String>,
    previous_path: Option<PathBuf>,
    previous_started: bool,
    previous_auto_name: Option<String>,
}

struct SessionCandidate {
    id: String,
    path: Option<PathBuf>,
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
        if project.remote.is_some() || self.terminal_launching.contains_key(&iid) {
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
            binding_path: instance.session_path.clone(),
            cwd: instance
                .worktree_path
                .as_ref()
                .unwrap_or(&project.root_path)
                .clone(),
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
                    for (request, candidate, miss) in results {
                        let iid = request.key.pane;
                        let current = this.session_discovery_key(iid, cx);
                        let unowned = candidate.as_ref().is_some_and(|candidate| {
                            !session_id_bound_elsewhere(
                                &this.workspace.instances,
                                iid,
                                &candidate.id,
                            )
                        });
                        let valid = this.session_discoveries.finish_with_codex_miss(
                            &request,
                            current.as_ref(),
                            Instant::now(),
                            unowned,
                            miss,
                        );
                        if valid
                            && unowned
                            && let Some(candidate) = candidate
                            && let Some(update) =
                                this.stage_discovered_session(iid, request.key.provider, candidate)
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
        candidate: SessionCandidate,
    ) -> Option<SessionBindingUpdate> {
        let instance = self.workspace.instance_mut(iid)?;
        let same_id = instance.session_id.as_deref() == Some(candidate.id.as_str());
        let path = candidate
            .path
            .or_else(|| same_id.then(|| instance.session_path.clone()).flatten());
        if same_id && instance.session_path == path {
            return None;
        }
        let update = SessionBindingUpdate {
            pane: iid,
            provider,
            previous_id: instance.session_id.clone(),
            previous_path: instance.session_path.clone(),
            previous_started: instance.session_started,
            previous_auto_name: instance.auto_name.clone(),
        };
        instance.session_id = Some(candidate.id);
        instance.session_path = path;
        instance.session_started = true;
        if !same_id {
            instance.auto_name = None;
        }
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
                instance.session_path = update.previous_path;
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
) -> Vec<(
    SessionDiscoveryRequest,
    Option<SessionCandidate>,
    Option<CodexHintMiss>,
)> {
    #[cfg(windows)]
    let mut grok_snapshot = None;
    requests
        .into_iter()
        .map(|request| {
            let key = &request.key;
            let started = Instant::now();
            let mut miss = None;
            let candidate = match key.provider {
                SessionProvider::Codex => {
                    let hint = key.hint.as_deref().unwrap_or_default();
                    let stamp = CodexHintMiss::capture(home, hint, started);
                    if let Some(cached) = request
                        .codex_miss
                        .as_ref()
                        .filter(|cached| cached.reusable_for(&stamp))
                    {
                        miss = Some(cached.clone());
                        None
                    } else {
                        let candidate = validated_codex_session_id(
                            &AgentPreset::codex(),
                            key.binding.as_deref(),
                            key.hint.as_deref(),
                            home,
                            &key.cwd,
                            Some(key.pane),
                        )
                        .map(|session| SessionCandidate {
                            id: session.id,
                            path: Some(session.path),
                        });
                        if candidate.is_none() {
                            miss = Some(stamp);
                        }
                        candidate
                    }
                }
                SessionProvider::Claude => data_dir.and_then(|data_dir| {
                    session_binding::claude_session_id_from_binding(data_dir, key.pane, home)
                        .map(|id| SessionCandidate { id, path: None })
                }),
                SessionProvider::Grok => {
                    #[cfg(windows)]
                    {
                        let (active, parents) = grok_snapshot.get_or_insert_with(|| {
                            (read_grok_active_sessions(), process_parent_map())
                        });
                        key.child_pid
                            .and_then(|pid| active_grok_session_for_process(pid, active, parents))
                            .map(|id| SessionCandidate { id, path: None })
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
            (request, candidate, miss)
        })
        .collect()
}

#[cfg(test)]
mod w8lm_tests {
    use super::discover_sessions;
    use muxel_core::session_discovery::{SessionDiscoveries, SessionDiscoveryKey, SessionProvider};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    fn key(root: &std::path::Path) -> SessionDiscoveryKey {
        SessionDiscoveryKey {
            pane: Uuid::new_v4(),
            provider: SessionProvider::Codex,
            launch: Instant::now(),
            child_pid: Some(1),
            binding_generation: 0,
            binding: None,
            binding_path: None,
            cwd: root.join("project"),
            hint: Some("01a1543c-9700-7000-8000-000000000001".into()),
        }
    }

    fn write(root: &std::path::Path, key: &SessionDiscoveryKey, subagent: bool) -> PathBuf {
        std::fs::create_dir_all(&key.cwd).unwrap();
        let day = root.join(".codex/sessions/2026/10/19");
        std::fs::create_dir_all(&day).unwrap();
        let id = key.hint.as_ref().unwrap();
        let path = day.join(format!("rollout-{id}.jsonl"));
        let source = if subagent {
            serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}})
        } else {
            serde_json::json!("cli")
        };
        std::fs::write(&path,serde_json::json!({"type":"session_meta","payload":{"id":id,"session_id":id,"source":source,"cwd":key.cwd}}).to_string()).unwrap();
        path
    }

    #[test]
    fn w8lm_worker_captures_a_rollout_that_appears_after_a_cached_miss() {
        let root = std::env::temp_dir().join(format!("muxel-w8lm-worker-{}", Uuid::new_v4()));
        let key = key(&root);
        let now = Instant::now();
        let mut discovery = SessionDiscoveries::default();
        for seconds in [0, 1] {
            let at = now + Duration::from_secs(seconds);
            let request = discovery.begin(key.clone(), at).unwrap();
            let (request, candidate, miss) =
                discover_sessions(vec![request], &root, None).pop().unwrap();
            assert!(candidate.is_none());
            assert!(miss.is_some());
            assert!(discovery.finish_with_codex_miss(&request, Some(&key), at, false, miss));
        }
        let path = write(&root, &key, false);
        let request = discovery
            .begin(key.clone(), now + Duration::from_secs(2))
            .unwrap();
        assert!(request.codex_miss.is_some());
        let (_, candidate, miss) = discover_sessions(vec![request], &root, None).pop().unwrap();
        let candidate = candidate.expect("new day directory must invalidate the miss");
        assert_eq!(candidate.id, key.hint.unwrap());
        assert_eq!(candidate.path, Some(path));
        assert!(miss.is_none());
    }

    #[test]
    fn w8lm_worker_rejects_a_subagent_without_changing_the_existing_binding() {
        let root = std::env::temp_dir().join(format!("muxel-w8lm-subagent-{}", Uuid::new_v4()));
        let mut key = key(&root);
        key.binding = Some(Uuid::new_v4().to_string());
        write(&root, &key, true);
        let old = key.binding.clone();
        let request = SessionDiscoveries::default()
            .begin(key, Instant::now())
            .unwrap();
        let (request, candidate, _) = discover_sessions(vec![request], &root, None).pop().unwrap();
        assert!(candidate.is_none());
        assert_eq!(request.key.binding, old);
    }
}
