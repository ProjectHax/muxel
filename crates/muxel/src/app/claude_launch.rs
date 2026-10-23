//! Reserve on the UI thread, resolve transcripts once per batch on a worker,
//! then build the command only if the reservation and binding still match.

use super::{
    MuxelApp, TerminalSpawnMeta, home_dir, is_claude_program, launch_token_current,
    release_launch_token, session_id_bound_elsewhere,
};
use crate::{session_binding, ui_profile};
use gpui::{AsyncApp, BackgroundExecutor, Context, WeakEntity};
use muxel_core::{
    AgentPreset, ClaudeSession, ClaudeSessionRequest, InstanceKind, TerminalLaunchReason,
};
use muxel_terminal::{CommandSpec, TerminalLaunch};
use std::collections::HashSet;
use std::path::PathBuf;
use uuid::Uuid;

type Ready = (TerminalSpawnMeta, CommandSpec, (u16, u16));

#[derive(Clone, PartialEq, Eq)]
struct LaunchKey {
    workspace: Option<Uuid>,
    pane: Uuid,
    project: Uuid,
    token: u64,
    reason: TerminalLaunchReason,
    generation: u64,
    id: Option<String>,
    path: Option<PathBuf>,
    started: bool,
    consumed_hook: Option<String>,
    record: session_binding::ClaudeHookIdentity,
    cwd: PathBuf,
    preset: AgentPreset,
    program: Option<String>,
    args: Vec<String>,
}

#[derive(Clone)]
struct Request {
    key: LaunchKey,
    home: Option<PathBuf>,
    data: Option<PathBuf>,
    siblings: HashSet<String>,
}

enum Probe {
    Found(ClaudeSession),
    Missing,
    Unknown,
    Refused(String),
}

struct Resolution {
    id: Option<String>,
    hook: bool,
    record: session_binding::ClaudeHookIdentity,
    probe: Probe,
}

struct Pending {
    request: Request,
    sender: Option<async_channel::Sender<Resolution>>,
    receiver: async_channel::Receiver<Resolution>,
}

pub(super) struct TerminalPreparation {
    pub meta: TerminalSpawnMeta,
    ready: Option<(CommandSpec, (u16, u16))>,
    pending: Option<Pending>,
}

impl TerminalPreparation {
    pub(super) fn ready((meta, spec, size): Ready) -> Self {
        Self {
            meta,
            ready: Some((spec, size)),
            pending: None,
        }
    }
    #[cfg(all(test, windows))]
    pub(super) fn ready_parts(self) -> Option<Ready> {
        self.ready.map(|(spec, size)| (self.meta, spec, size))
    }
}

pub(super) fn start_batch(preparations: &mut [TerminalPreparation], executor: &BackgroundExecutor) {
    let jobs: Vec<_> = preparations
        .iter_mut()
        .filter_map(|prepared| {
            let pending = prepared.pending.as_mut()?;
            Some((pending.request.clone(), pending.sender.take()?))
        })
        .collect();
    if !jobs.is_empty() {
        executor
            .spawn(async move {
                resolve_batch(jobs);
            })
            .detach();
    }
}

fn request_record(request: &Request) -> session_binding::ClaudeHookIdentity {
    request
        .data
        .as_deref()
        .map(|data| session_binding::claude_hook_identity(data, request.key.pane))
        .unwrap_or(Ok(None))
}

fn resolve_batch(jobs: Vec<(Request, async_channel::Sender<Resolution>)>) {
    let Some(home) = jobs.first().and_then(|(request, _)| request.home.as_ref()) else {
        for (request, sender) in jobs {
            let record = request_record(&request);
            let _ = sender.try_send(Resolution {
                id: request.key.id,
                hook: false,
                record,
                probe: Probe::Unknown,
            });
        }
        return;
    };
    let panes = jobs.iter().map(|(request, _)| request.key.pane).collect();
    let bindings = jobs
        .first()
        .and_then(|(request, _)| request.data.as_deref())
        .map(|data| session_binding::claude_bindings_for_batch(data, &panes, home))
        .transpose();
    let bindings = match bindings {
        Ok(bindings) => bindings.unwrap_or_default(),
        Err(error) => {
            for (request, sender) in jobs {
                let record = request_record(&request);
                let _ = sender.try_send(Resolution {
                    id: request.key.id,
                    hook: false,
                    record,
                    probe: Probe::Refused(error.to_string()),
                });
            }
            return;
        }
    };
    let mut lookups = Vec::new();
    let mut pending = Vec::new();
    for (request, sender) in &jobs {
        let snapshot = bindings.get(&request.key.pane);
        let record = snapshot
            .map(|snapshot| snapshot.record.clone())
            .unwrap_or(Ok(None));
        let consumed = record
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|record| Some(record) == request.key.consumed_hook.as_ref());
        let binding = if consumed {
            None
        } else {
            match snapshot.map(|snapshot| &snapshot.binding) {
                Some(Err(error)) => {
                    let _ = sender.try_send(Resolution {
                        id: request.key.id.clone(),
                        hook: false,
                        record,
                        probe: Probe::Refused(error.clone()),
                    });
                    continue;
                }
                Some(Ok(binding)) => binding.as_ref(),
                None => None,
            }
        };
        let hook = binding.filter(|binding| {
            !request.siblings.contains(&binding.id)
                && record
                    .as_ref()
                    .ok()
                    .and_then(Option::as_ref)
                    .is_some_and(|record| Some(record) != request.key.consumed_hook.as_ref())
        });
        let id = hook
            .map(|binding| binding.id.clone())
            .or_else(|| request.key.id.clone());
        // A legacy announcement matching a saved conversation has already been
        // adopted. Discover its real transcript and consume the receipt rather
        // than promising another first write. New publications remain eligible.
        if let Some(binding) = hook.filter(|binding| {
            binding.pending_write
                && !(binding.legacy
                    && request.key.consumed_hook.is_none()
                    && request.key.started
                    && request.key.id.as_deref() == Some(binding.id.as_str()))
        }) {
            let _ = sender.try_send(Resolution {
                id,
                hook: true,
                record,
                probe: Probe::Found(ClaudeSession {
                    path: binding.path.clone(),
                    cwd: binding.cwd.is_dir().then(|| binding.cwd.clone()),
                }),
            });
            continue;
        }
        let Some(session_id) = id.clone().filter(|_| request.key.started || hook.is_some()) else {
            let _ = sender.try_send(Resolution {
                id,
                hook: false,
                record,
                probe: Probe::Unknown,
            });
            continue;
        };
        #[cfg(test)]
        if let Some(probe) = super::CLAUDE_LAUNCH_TEST_LOOKUP.lock().unwrap().clone() {
            probe();
            let _ = sender.try_send(Resolution {
                id,
                hook: hook.is_some(),
                record,
                probe: Probe::Unknown,
            });
            continue;
        }
        lookups.push(ClaudeSessionRequest {
            id: session_id,
            path: hook
                .map(|binding| binding.path.clone())
                .or_else(|| request.key.path.clone()),
            cwd: Some(request.key.cwd.clone()),
        });
        pending.push((id, hook.is_some(), record, sender));
    }
    let _phase = ui_profile::phase("background-discovery", "claude-launch-batch", None);
    muxel_core::discover_claude_sessions(home, &lookups, |index, result| {
        let (id, hook, record, sender) = &pending[index];
        let probe = match result {
            Ok(Some(session)) => Probe::Found(session),
            Ok(None) => Probe::Missing,
            Err(error) => {
                log::warn!("could not check Claude transcript before resume: {error}");
                Probe::Unknown
            }
        };
        let _ = sender.try_send(Resolution {
            id: id.clone(),
            hook: *hook,
            record: record.clone(),
            probe,
        });
    });
}

impl MuxelApp {
    fn claude_launch_key(
        &self,
        pane: Uuid,
        token: u64,
        reason: TerminalLaunchReason,
    ) -> Option<LaunchKey> {
        let instance = self.workspace.instance(pane)?;
        let project = self.workspace.project(instance.project_id)?;
        if project.remote.is_some() || instance.kind != InstanceKind::Terminal {
            return None;
        }
        let preset = instance
            .preset_id
            .and_then(|id| self.presets.iter().find(|preset| preset.id == id))
            .or_else(|| {
                self.presets
                    .iter()
                    .find(|preset| preset.name == instance.preset)
            })?;
        if !is_claude_program(preset.program.as_deref()) || preset.resume_flag.is_none() {
            return None;
        }
        Some(LaunchKey {
            workspace: self.current_workspace,
            pane,
            project: project.id,
            token,
            reason,
            generation: self.session_discoveries.binding_generation(pane),
            id: instance.session_id.clone(),
            path: instance.session_path.clone(),
            started: instance.session_started,
            consumed_hook: instance.claude_hook_record.clone(),
            // The worker fills the receipt portion of the resolved key. Launch
            // reservation itself performs no hook or transcript filesystem I/O.
            record: Ok(None),
            cwd: instance
                .worktree_path
                .clone()
                .unwrap_or_else(|| project.root_path.clone()),
            preset: preset.clone(),
            program: instance.program.clone(),
            args: instance.args.clone(),
        })
    }

    pub(super) fn needs_claude_preflight(&self, pane: Uuid) -> bool {
        self.claude_launch_key(pane, 0, TerminalLaunchReason::Restore)
            .is_some()
    }

    #[cfg(all(test, windows))]
    pub(super) fn prepare_claude_launch(
        &self,
        pane: Uuid,
        token: u64,
    ) -> Option<TerminalPreparation> {
        self.prepare_claude_launch_with_reason(pane, token, TerminalLaunchReason::Restore)
    }

    pub(super) fn prepare_claude_launch_with_reason(
        &self,
        pane: Uuid,
        token: u64,
        reason: TerminalLaunchReason,
    ) -> Option<TerminalPreparation> {
        self.prepare_claude_launch_for(pane, token, reason, home_dir(), muxel_store::data_dir())
    }

    fn prepare_claude_launch_for(
        &self,
        pane: Uuid,
        token: u64,
        reason: TerminalLaunchReason,
        home: Option<PathBuf>,
        data: Option<PathBuf>,
    ) -> Option<TerminalPreparation> {
        let key = self.claude_launch_key(pane, token, reason)?;
        let (sender, receiver) = async_channel::bounded(1);
        let request = Request {
            key,
            home,
            data,
            siblings: self
                .workspace
                .instances
                .iter()
                .filter(|instance| instance.id != pane)
                .filter_map(|instance| instance.session_id.clone())
                .collect(),
        };
        let meta = TerminalSpawnMeta {
            instance_id: pane,
            token,
            program: request.key.preset.program.clone().unwrap_or_default(),
            cwd: request.key.cwd.display().to_string(),
            was_resume: request.key.started,
            started: self.terminal_launching.get(&pane)?.since,
        };
        Some(TerminalPreparation {
            meta,
            ready: None,
            pending: Some(Pending {
                request,
                sender: Some(sender),
                receiver,
            }),
        })
    }

    fn advance_claude_launch(
        &mut self,
        request: Request,
        resolution: Resolution,
        cx: &mut Context<Self>,
    ) -> Option<TerminalPreparation> {
        let mut key = request.key.clone();
        if !launch_token_current(&self.terminal_launching, key.pane, key.token)
            || self.current_workspace != key.workspace
            || self.workspace.instance(key.pane).is_none_or(|instance| {
                instance.project_id != key.project || instance.kind != InstanceKind::Terminal
            })
            || self
                .workspace
                .project(key.project)
                .is_none_or(|project| project.remote.is_some())
        {
            return None;
        }
        let current = self.claude_launch_key(key.pane, key.token, key.reason);
        if current.as_ref() != Some(&key) {
            // Refresh a still-owned local launch instead of leaving its caller
            // without a terminal. Reuse the captured store roots and deadline.
            return if current.is_some() {
                self.prepare_claude_launch_for(
                    key.pane,
                    key.token,
                    key.reason,
                    request.home,
                    request.data,
                )
            } else {
                self.prepare_reserved_terminal_spawn(key.pane, key.token, key.reason, cx)
            };
        }
        // Attach the worker's actual receipt identity to its captured context,
        // then compare it with the latest own-data receipt before applying it.
        key.record = resolution.record.clone();
        if request_record(&request) != key.record {
            return self.prepare_claude_launch_for(
                key.pane,
                key.token,
                key.reason,
                request.home,
                request.data,
            );
        }
        let refused = match &resolution.probe {
            Probe::Refused(error) => Some(error.clone()),
            _ => key.record.as_ref().err().cloned(),
        };
        if let Some(error) = refused {
            self.failed_launches.insert(
                key.pane,
                crate::i18n::tf(
                    "Could not recover this conversation before launch: {error}",
                    &[("error", &error)],
                ),
            );
            cx.notify();
            return None;
        }
        if resolution.hook
            && resolution.id.as_deref().is_some_and(|id| {
                session_id_bound_elsewhere(&self.workspace.instances, key.pane, id)
            })
        {
            // A sibling may adopt the hook after this pane's worker snapshot.
            // Refresh ownership before resolving the pane's saved binding.
            return self.prepare_claude_launch_for(
                key.pane,
                key.token,
                key.reason,
                request.home,
                request.data,
            );
        }
        let previous = self.workspace.instance(key.pane)?.clone();
        let instance = self.workspace.instance_mut(key.pane)?;
        let consume = request.home.is_some()
            && (resolution.hook || matches!(&resolution.probe, Probe::Missing) || !key.started);
        if consume && let Ok(Some(record)) = &key.record {
            instance.claude_hook_record = Some(record.clone());
        }
        let mut cwd = None;
        if resolution.hook {
            if instance.session_id != resolution.id {
                instance.auto_name = None;
            }
            instance.session_id = resolution.id;
            instance.session_started = true;
        }
        match resolution.probe {
            Probe::Found(session) => {
                instance.session_path = Some(session.path);
                cwd = session.cwd;
            }
            Probe::Missing => {
                instance.session_id = None;
                instance.session_path = None;
                instance.session_started = false;
            }
            Probe::Unknown => {}
            Probe::Refused(_) => unreachable!(),
        }
        // Persist a fresh host-owned identity before command construction too.
        // There is no provider discovery for a conversation we have yet to start.
        if instance.session_id.is_none() && key.preset.session_id_flag.is_some() {
            instance.session_id = Some(Uuid::new_v4().to_string());
            instance.session_started = false;
            instance.session_path = None;
        }
        if !instance.session_started
            && instance.session_path.is_none()
            && let (Some(home), Some(id)) =
                (request.home.as_deref(), instance.session_id.as_deref())
        {
            instance.session_path = Some(muxel_core::claude_session_path(home, &key.cwd, id));
        }
        if instance.session_id != previous.session_id
            || instance.session_path != previous.session_path
            || instance.session_started != previous.session_started
            || instance.claude_hook_record != previous.claude_hook_record
        {
            self.bump_session_binding_generation(key.pane);
            if !self.try_persist() {
                *self.workspace.instance_mut(key.pane)? = previous;
                self.bump_session_binding_generation(key.pane);
                self.failed_launches.insert(
                    key.pane,
                    crate::i18n::t("Could not save this conversation before launch.").to_string(),
                );
                cx.notify();
                return None;
            }
        }
        // Saving can yield filesystem time to another hook publisher too.
        // Re-probe immediately before constructing the command under this token.
        if request_record(&request) != key.record {
            return self.prepare_claude_launch_for(
                key.pane,
                key.token,
                key.reason,
                request.home,
                request.data,
            );
        }
        self.prepare_resolved_terminal_spawn(key.pane, key.token, key.reason, cwd, cx)
            .map(TerminalPreparation::ready)
    }

    #[cfg(all(test, windows))]
    fn apply_claude_launch(
        &mut self,
        request: Request,
        resolution: Resolution,
        cx: &mut Context<Self>,
    ) -> Option<Ready> {
        self.advance_claude_launch(request, resolution, cx)
            .and_then(TerminalPreparation::ready_parts)
    }

    pub(super) async fn run_terminal_preparation(
        this: &WeakEntity<Self>,
        mut prepared: TerminalPreparation,
        cx: &mut AsyncApp,
    ) -> Option<(TerminalSpawnMeta, anyhow::Result<TerminalLaunch>)> {
        let pane = prepared.meta.instance_id;
        let token = prepared.meta.token;
        let ready = loop {
            if let Some((spec, size)) = prepared.ready {
                break Some((prepared.meta, spec, size));
            }
            let Some(pending) = prepared.pending else {
                break None;
            };
            let Ok(resolution) = pending.receiver.recv().await else {
                break None;
            };
            let next = this
                .update(cx, |app, cx| {
                    app.advance_claude_launch(pending.request, resolution, cx)
                })
                .ok()
                .flatten();
            let Some(mut next) = next else {
                break None;
            };
            start_batch(std::slice::from_mut(&mut next), cx.background_executor());
            prepared = next;
        };
        let Some((meta, spec, size)) = ready else {
            let _ = this.update(cx, |app, _| {
                release_launch_token(&mut app.terminal_launching, pane, token)
            });
            return None;
        };
        let result = cx
            .background_executor()
            .spawn(async move { TerminalLaunch::spawn_for_instance(spec, size, pane) })
            .await;
        Some((meta, result))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{Probe, Resolution, resolve_batch};
    use crate::app::{MuxelApp, codex_capture_tests::with_app};
    use muxel_core::{AgentPreset, ClaudeSession, Instance, Project};
    use uuid::Uuid;

    fn pane(app: &mut MuxelApp) -> Uuid {
        let cwd = muxel_store::data_dir().unwrap().join("claude-project");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = app.workspace.add_project(Project::new("Claude", cwd));
        let preset = AgentPreset::claude();
        let mut instance = Instance::from_preset(project, &preset);
        instance.session_id = Some(Uuid::new_v4().to_string());
        instance.session_started = true;
        let id = instance.id;
        app.workspace.add_instance(instance);
        app.presets.push(preset);
        app.current_workspace = Some(Uuid::new_v4());
        id
    }

    fn run_preparation(
        cx: &mut gpui::TestAppContext,
        app: gpui::WeakEntity<MuxelApp>,
        prepared: super::TerminalPreparation,
    ) -> Option<(
        crate::app::TerminalSpawnMeta,
        anyhow::Result<muxel_terminal::TerminalLaunch>,
    )> {
        let (done, result) = std::sync::mpsc::channel();
        cx.spawn(move |mut cx| async move {
            let ready = MuxelApp::run_terminal_preparation(&app, prepared, &mut cx).await;
            let _ = done.send(ready);
        })
        .detach();
        let cap = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            cx.run_until_parked();
            if let Ok(result) = result.try_recv() {
                return result;
            }
            assert!(
                std::time::Instant::now() < cap,
                "preparation runner never completed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn claude_refresh_preset_edit_finishes_the_original_runner() {
        for action in ["preset", "closed", "superseded"] {
            with_app(|cx, window| {
                let home = muxel_store::data_dir()
                    .unwrap()
                    .join(format!("preset-owned-home-{}", Uuid::new_v4()));
                std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
                let (id, mut prepared) = window
                    .update(cx, |app, window, cx| {
                        let id = pane(app);
                        let preset = app
                            .presets
                            .iter_mut()
                            .find(|p| Some(p.id) == app.workspace.instance(id).unwrap().preset_id)
                            .unwrap();
                        // Keep Claude preflight but make the eventual owned child
                        // a harmless cmd-more fixture, with no provider executable.
                        preset.session_id_flag = None;
                        let row = app.workspace.instance_mut(id).unwrap();
                        row.program = Some("cmd.exe".into());
                        row.injection = muxel_core::InjectionMode::None;
                        row.args = vec!["/d".into(), "/q".into(), "/c".into(), "more".into()];
                        (id, app.prepare_terminal_spawn(id, window, cx).unwrap())
                    })
                    .unwrap();
                let original_token = prepared.meta.token;
                let original_since = prepared.meta.started;
                let pending = prepared.pending.as_mut().unwrap();
                pending.request.home = Some(home);
                pending.request.data = muxel_store::data_dir();
                let request = pending.request.clone();
                let sender = pending.sender.take().unwrap();
                let (started, observed) = std::sync::mpsc::channel();
                let (release, wait) = async_channel::bounded(1);
                *crate::app::CLAUDE_LAUNCH_TEST_LOOKUP.lock().unwrap() =
                    Some(std::sync::Arc::new(move || {
                        started.send(()).unwrap();
                        wait.recv_blocking().unwrap();
                    }));
                struct Worker {
                    release: async_channel::Sender<()>,
                    worker: Option<std::thread::JoinHandle<()>>,
                }
                impl Drop for Worker {
                    fn drop(&mut self) {
                        let _ = self.release.try_send(());
                        if let Some(worker) = self.worker.take() {
                            let _ = worker.join();
                        }
                        *crate::app::CLAUDE_LAUNCH_TEST_LOOKUP.lock().unwrap() = None;
                    }
                }
                let mut worker = Worker {
                    release,
                    worker: Some(std::thread::spawn(move || {
                        resolve_batch(vec![(request, sender)])
                    })),
                };
                observed
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap();
                window
                    .update(cx, |app, _, _| match action {
                        "preset" => {
                            let preset_id = app.workspace.instance(id).unwrap().preset_id;
                            app.presets
                                .iter_mut()
                                .find(|p| Some(p.id) == preset_id)
                                .unwrap()
                                .model = Some("edited-during-lookup".into());
                            app.persist_settings();
                        }
                        "closed" => app.workspace.instances.retain(|row| row.id != id),
                        "superseded" => app.terminal_launching.get_mut(&id).unwrap().token += 1,
                        _ => unreachable!(),
                    })
                    .unwrap();
                worker.release.try_send(()).unwrap();
                worker.worker.take().unwrap().join().unwrap();
                drop(worker);
                let app = window.entity(cx).unwrap().downgrade();
                let result = run_preparation(cx, app, prepared);
                if action != "preset" {
                    assert!(result.is_none(), "cancelled {action} launch restarted");
                    window
                        .read_with(cx, |app, _| {
                            assert!(!app.terminals.contains_key(&id));
                            if action == "superseded" {
                                assert_ne!(app.terminal_launching[&id].token, original_token);
                            } else {
                                assert!(!app.terminal_launching.contains_key(&id));
                            }
                        })
                        .unwrap();
                    return;
                }
                let (meta, launch) =
                    result.expect("preset edit stranded the valid launch reservation");
                assert_eq!(meta.token, original_token);
                assert_eq!(meta.started, original_since);
                assert!(
                    !meta.was_resume,
                    "stale result bypassed the new missing-transcript lookup"
                );
                let session = window
                    .update(cx, |app, window, cx| {
                        app.finish_terminal_spawn(meta, launch, window, cx);
                        app.terminals[&id].read(cx).session().clone()
                    })
                    .unwrap();
                struct Stop(std::sync::Arc<muxel_terminal::TerminalSession>);
                impl Drop for Stop {
                    fn drop(&mut self) {
                        self.0.kill();
                    }
                }
                let _stop = Stop(session.clone());
                assert!(
                    session.child_pid().is_some(),
                    "automatic retry never launched its owned PTY"
                );
                window
                    .read_with(cx, |app, _| {
                        assert!(!app.failed_launches.contains_key(&id));
                        assert!(!app.terminal_launching.contains_key(&id));
                    })
                    .unwrap();
            });
        }
    }

    #[test]
    fn claude_preflight_persists_path_and_builds_resume_in_recorded_cwd() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let prepared = app.prepare_terminal_spawn(id, window, cx).unwrap();
                    let started = prepared.meta.started;
                    let request = prepared.pending.unwrap().request;
                    let sid = request.key.id.clone().unwrap();
                    let path = muxel_store::data_dir()
                        .unwrap()
                        .join(format!("{sid}.jsonl"));
                    let moved = muxel_store::data_dir().unwrap().join("moved");
                    std::fs::create_dir_all(&moved).unwrap();
                    let (meta, command, _) = app
                        .apply_claude_launch(
                            request,
                            Resolution {
                                record: Ok(None),
                                id: Some(sid.clone()),
                                hook: false,
                                probe: Probe::Found(ClaudeSession {
                                    path: path.clone(),
                                    cwd: Some(moved.clone()),
                                }),
                            },
                            cx,
                        )
                        .unwrap();
                    assert_eq!(
                        meta.started, started,
                        "discovery must count toward admission deadline"
                    );
                    assert_eq!(command.cwd.as_deref(), Some(moved.to_str().unwrap()));
                    assert!(
                        command
                            .args
                            .windows(2)
                            .any(|pair| pair == ["--resume", sid.as_str()])
                    );
                    let saved = muxel_store::load_workspace_from(
                        &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        saved.instance(id).unwrap().session_path.as_ref(),
                        Some(&path)
                    );
                    assert_eq!(
                        saved.instance(id).unwrap().session_id.as_deref(),
                        Some(sid.as_str())
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn claude_preflight_rejects_stale_binding_workspace_and_launch_then_accepts_current() {
        for stale in ["binding", "workspace", "launch", "program"] {
            with_app(|cx, window| {
                window
                    .update(cx, |app, window, cx| {
                        let id = pane(app);
                        let old = app.workspace.instance(id).unwrap().session_id.clone();
                        let pending = app
                            .prepare_terminal_spawn(id, window, cx)
                            .unwrap()
                            .pending
                            .unwrap();
                        match stale {
                            "binding" => app.bump_session_binding_generation(id),
                            "workspace" => app.current_workspace = Some(Uuid::new_v4()),
                            "launch" => {
                                app.terminal_launching.get_mut(&id).unwrap().token += 1;
                            }
                            "program" => {
                                app.workspace.instance_mut(id).unwrap().program =
                                    Some("cmd.exe".into())
                            }
                            _ => unreachable!(),
                        }
                        let candidate = Uuid::new_v4().to_string();
                        assert!(
                            app.apply_claude_launch(
                                pending.request,
                                Resolution {
                                    record: Ok(None),
                                    id: Some(candidate),
                                    hook: true,
                                    probe: Probe::Unknown,
                                },
                                cx
                            )
                            .is_none()
                        );
                        assert_eq!(app.workspace.instance(id).unwrap().session_id, old);
                        app.workspace.instance_mut(id).unwrap().program = Some("claude".into());
                        let token = app.terminal_launching[&id].token;
                        let current = app
                            .prepare_claude_launch(id, token)
                            .unwrap()
                            .pending
                            .unwrap()
                            .request;
                        assert!(
                            app.apply_claude_launch(
                                current,
                                Resolution {
                                    record: Ok(None),
                                    id: old.clone(),
                                    hook: false,
                                    probe: Probe::Unknown,
                                },
                                cx
                            )
                            .is_some(),
                            "current request rejected after {stale}"
                        );
                        assert_eq!(app.workspace.instance(id).unwrap().session_id, old);
                    })
                    .unwrap();
            });
        }
    }

    #[test]
    fn claude_preflight_rechecks_sibling_ownership_at_completion() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let pending = app
                        .prepare_terminal_spawn(id, window, cx)
                        .unwrap()
                        .pending
                        .unwrap();
                    let old = app.workspace.instance(id).unwrap().session_id.clone();
                    let sibling = pane(app);
                    // Keep the same workspace; this test isolates the sibling guard.
                    app.current_workspace = pending.request.key.workspace;
                    let candidate = app.workspace.instance(sibling).unwrap().session_id.clone();
                    assert!(
                        app.apply_claude_launch(
                            pending.request,
                            Resolution {
                                record: Ok(None),
                                id: candidate,
                                hook: true,
                                probe: Probe::Unknown,
                            },
                            cx
                        )
                        .is_none()
                    );
                    assert_eq!(app.workspace.instance(id).unwrap().session_id, old);
                })
                .unwrap();
        });
    }

    #[test]
    fn claude_worker_completes_slow_probe_without_holding_the_ui_update() {
        with_app(|cx, window| {
            let (id, mut pending) = window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let pending = app
                        .prepare_terminal_spawn(id, window, cx)
                        .unwrap()
                        .pending
                        .unwrap();
                    (id, pending)
                })
                .unwrap();
            pending.request.home = Some(muxel_store::data_dir().unwrap().join("home"));
            let (started, observed) = std::sync::mpsc::channel();
            *crate::app::CLAUDE_LAUNCH_TEST_LOOKUP.lock().unwrap() =
                Some(std::sync::Arc::new(move || {
                    started.send(()).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(350));
                }));
            struct ClearProbe;
            impl Drop for ClearProbe {
                fn drop(&mut self) {
                    *crate::app::CLAUDE_LAUNCH_TEST_LOOKUP.lock().unwrap() = None;
                }
            }
            let _clear = ClearProbe;
            let request = pending.request.clone();
            let sender = pending.sender.take().unwrap();
            let worker = std::thread::spawn(move || resolve_batch(vec![(request, sender)]));
            observed
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            let clock = std::time::Instant::now();
            window
                .update(cx, |app, _, _| {
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id,
                        pending.request.key.id
                    );
                    app.active_instance = Some(id);
                })
                .unwrap();
            assert!(clock.elapsed() < std::time::Duration::from_millis(100));
            worker.join().unwrap();
            let resolution = pending.receiver.recv_blocking().unwrap();
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(pending.request, resolution, cx)
                        .unwrap();
                    assert!(command.args.iter().any(|arg| arg == "--resume"));
                })
                .unwrap();
        });
    }

    #[test]
    fn claude_preflight_save_failure_rolls_back_and_refuses_the_launch() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let old = app.workspace.instance(id).unwrap().clone();
                    let pending = app
                        .prepare_terminal_spawn(id, window, cx)
                        .unwrap()
                        .pending
                        .unwrap();
                    let document =
                        muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                    std::fs::create_dir_all(&document).unwrap();
                    assert!(
                        app.apply_claude_launch(
                            pending.request,
                            Resolution {
                                record: Ok(None),
                                id: Some(Uuid::new_v4().to_string()),
                                hook: true,
                                probe: Probe::Found(ClaudeSession {
                                    path: muxel_store::data_dir()
                                        .unwrap()
                                        .join("new-transcript.jsonl"),
                                    cwd: None
                                }),
                            },
                            cx
                        )
                        .is_none()
                    );
                    let current = app.workspace.instance(id).unwrap();
                    assert_eq!(current.session_id, old.session_id);
                    assert_eq!(current.session_path, old.session_path);
                    assert_eq!(current.session_started, old.session_started);
                    assert!(app.failed_launches.contains_key(&id));
                })
                .unwrap();
        });
    }

    #[test]
    fn confirmed_missing_claude_transcript_persists_a_fresh_identity_before_launch() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let old = app.workspace.instance(id).unwrap().session_id.clone();
                    let pending = app
                        .prepare_terminal_spawn(id, window, cx)
                        .unwrap()
                        .pending
                        .unwrap();
                    let (meta, command, _) = app
                        .apply_claude_launch(
                            pending.request,
                            Resolution {
                                record: Ok(None),
                                id: old.clone(),
                                hook: false,
                                probe: Probe::Missing,
                            },
                            cx,
                        )
                        .unwrap();
                    assert!(!meta.was_resume);
                    assert!(command.args.iter().any(|arg| arg == "--session-id"));
                    assert!(!command.args.iter().any(|arg| arg == "--resume"));
                    let saved = muxel_store::load_workspace_from(
                        &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                    )
                    .unwrap();
                    let pane = saved.instance(id).unwrap();
                    assert_ne!(pane.session_id, old);
                    assert!(pane.session_path.is_some());
                    assert!(!pane.session_started);
                })
                .unwrap();
        });
    }

    #[test]
    fn custom_agent_minted_claude_preset_starts_bare_after_a_confirmed_miss() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let preset_id = app.workspace.instance(id).unwrap().preset_id.unwrap();
                    app.presets
                        .iter_mut()
                        .find(|preset| preset.id == preset_id)
                        .unwrap()
                        .session_id_flag = None;
                    let pending = app
                        .prepare_terminal_spawn(id, window, cx)
                        .unwrap()
                        .pending
                        .unwrap();
                    let (_, command, _) = app
                        .apply_claude_launch(
                            pending.request,
                            Resolution {
                                record: Ok(None),
                                id: None,
                                hook: false,
                                probe: Probe::Missing,
                            },
                            cx,
                        )
                        .unwrap();
                    assert!(
                        !command
                            .args
                            .iter()
                            .any(|arg| arg == "--session-id" || arg == "--resume")
                    );
                    let instance = app.workspace.instance(id).unwrap();
                    assert!(instance.session_id.is_none() && instance.session_path.is_none());
                    assert!(!instance.session_started);
                })
                .unwrap();
        });
    }

    fn w8lm_hook(
        home: &std::path::Path,
        pane: Uuid,
        id: &str,
        source: &str,
        transcript_exists: bool,
    ) -> std::path::PathBuf {
        w8lm_hook_with_format(home, pane, id, source, transcript_exists, true)
    }

    fn w8lm_hook_with_format(
        home: &std::path::Path,
        pane: Uuid,
        id: &str,
        source: &str,
        transcript_exists: bool,
        stamped: bool,
    ) -> std::path::PathBuf {
        let data = muxel_store::data_dir().unwrap();
        let cwd = data.join("claude-project");
        std::fs::create_dir_all(&cwd).unwrap();
        let path = muxel_core::claude_session_path(home, &cwd, id);
        if transcript_exists {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::json!({"cwd":cwd}).to_string()).unwrap();
        }
        let event = serde_json::json!({"hook_event_name":"SessionStart","source":source,
            "session_id":id,"transcript_path":path,"cwd":cwd});
        if stamped {
            crate::session_binding::write_claude_binding_from_reader(
                &data,
                pane,
                std::io::Cursor::new(event.to_string()),
            )
            .unwrap();
        } else {
            // Old hook records without a stamp remain readable after an upgrade.
            let record = crate::session_binding::claude_binding_path(&data, pane);
            std::fs::create_dir_all(record.parent().unwrap()).unwrap();
            std::fs::write(record, event.to_string()).unwrap();
        }
        path
    }

    fn w8lm_resolve(pending: &mut super::Pending, home: &std::path::Path) -> super::Resolution {
        pending.request.home = Some(home.to_path_buf());
        resolve_batch(vec![(
            pending.request.clone(),
            pending.sender.take().unwrap(),
        )]);
        pending.receiver.recv_blocking().unwrap()
    }

    #[test]
    fn w8lm_claude_rejects_a_hook_replaced_after_the_worker_even_with_the_same_mtime() {
        with_app(|cx, window| {
            let home = muxel_store::data_dir().unwrap().join("w8lm-home");
            let (id, mut pending) = window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    w8lm_hook(&home, id, &Uuid::new_v4().to_string(), "resume", true);
                    (
                        id,
                        app.prepare_terminal_spawn(id, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                    )
                })
                .unwrap();
            let resolution = w8lm_resolve(&mut pending, &home);
            let record =
                crate::session_binding::claude_binding_path(&muxel_store::data_dir().unwrap(), id);
            let modified = std::fs::metadata(&record).unwrap().modified().unwrap();
            w8lm_hook(&home, id, &Uuid::new_v4().to_string(), "resume", true);
            std::fs::File::options()
                .write(true)
                .open(record)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
            window
                .update(cx, |app, _, cx| {
                    assert!(
                        app.apply_claude_launch(pending.request, resolution, cx)
                            .is_none(),
                        "the older hook must not construct a command after replacement"
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn w8lm_reprobe_builds_the_newer_hook_command_under_the_original_reservation() {
        with_app(|cx, window| {
            let home = muxel_store::data_dir().unwrap().join("w8lm-home");
            let (id, started, mut pending) = window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    w8lm_hook(&home, id, &Uuid::new_v4().to_string(), "resume", true);
                    let prepared = app.prepare_terminal_spawn(id, window, cx).unwrap();
                    (id, prepared.meta.started, prepared.pending.unwrap())
                })
                .unwrap();
            let token = pending.request.key.token;
            let resolution = w8lm_resolve(&mut pending, &home);
            let newest = Uuid::new_v4().to_string();
            w8lm_hook(&home, id, &newest, "resume", true);
            let mut retry = window
                .update(cx, |app, _, cx| {
                    let next = app
                        .advance_claude_launch(pending.request, resolution, cx)
                        .unwrap();
                    assert_eq!(next.meta.started, started);
                    assert_eq!(next.meta.token, token);
                    next.pending
                        .expect("newer receipt must receive a new probe")
                })
                .unwrap();
            assert_eq!(retry.request.home.as_deref(), Some(home.as_path()));
            let resolution = w8lm_resolve(&mut retry, &home);
            window
                .update(cx, |app, _, cx| {
                    let ready = app
                        .advance_claude_launch(retry.request, resolution, cx)
                        .unwrap();
                    assert_eq!(ready.meta.started, started);
                    let (_, command, _) = ready.ready_parts().unwrap();
                    assert!(
                        command
                            .args
                            .windows(2)
                            .any(|args| args == ["--resume", newest.as_str()])
                    );
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(newest.as_str())
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn w8lm_late_sibling_claim_reprepares_the_other_pane() {
        with_app(|cx, window| {
            let home = muxel_store::data_dir().unwrap().join("w8lm-sibling-home");
            let announced = Uuid::new_v4().to_string();
            let (first, second, mut first_pending, mut second_pending) = window
                .update(cx, |app, window, cx| {
                    let first = pane(app);
                    let second = pane(app);
                    w8lm_hook(&home, first, &announced, "resume", true);
                    w8lm_hook(&home, second, &announced, "resume", true);
                    (
                        first,
                        second,
                        app.prepare_terminal_spawn(first, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                        app.prepare_terminal_spawn(second, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                    )
                })
                .unwrap();
            first_pending.request.home = Some(home.clone());
            second_pending.request.home = Some(home.clone());
            resolve_batch(vec![
                (
                    first_pending.request.clone(),
                    first_pending.sender.take().unwrap(),
                ),
                (
                    second_pending.request.clone(),
                    second_pending.sender.take().unwrap(),
                ),
            ]);
            let first_resolution = first_pending.receiver.recv_blocking().unwrap();
            let second_resolution = second_pending.receiver.recv_blocking().unwrap();
            let token = second_pending.request.key.token;
            let mut refreshed = window
                .update(cx, |app, _, cx| {
                    app.apply_claude_launch(first_pending.request, first_resolution, cx)
                        .unwrap();
                    assert_eq!(
                        app.workspace.instance(first).unwrap().session_id.as_deref(),
                        Some(announced.as_str())
                    );
                    let next = app
                        .advance_claude_launch(second_pending.request, second_resolution, cx)
                        .expect("late sibling claim must refresh the other launch");
                    assert_eq!(next.meta.instance_id, second);
                    assert_eq!(next.meta.token, token);
                    next.pending.expect("sibling ownership must be re-probed")
                })
                .unwrap();
            let resolution = w8lm_resolve(&mut refreshed, &home);
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(refreshed.request, resolution, cx)
                        .expect("second pane must launch on its saved binding");
                    assert_ne!(
                        app.workspace
                            .instance(second)
                            .unwrap()
                            .session_id
                            .as_deref(),
                        Some(announced.as_str())
                    );
                    assert!(!command.args.iter().any(|arg| arg == &announced));
                })
                .unwrap();
        });
    }

    #[test]
    fn w8lm_unconsumed_clear_still_resumes_before_its_first_transcript_write() {
        with_app(|cx, window| {
            let home = muxel_store::data_dir().unwrap().join("w8lm-home");
            let newest = Uuid::new_v4().to_string();
            let (id, path, mut pending) = window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let path = w8lm_hook(&home, id, &newest, "clear", false);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    (
                        id,
                        path,
                        app.prepare_terminal_spawn(id, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                    )
                })
                .unwrap();
            let resolution = w8lm_resolve(&mut pending, &home);
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(pending.request, resolution, cx)
                        .unwrap();
                    assert!(
                        command
                            .args
                            .windows(2)
                            .any(|args| args == ["--resume", newest.as_str()])
                    );
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(newest.as_str())
                    );
                })
                .unwrap();
            assert!(!path.exists());
        });
    }

    #[test]
    fn w8lm_adopted_clear_recovers_after_its_transcript_is_deleted() {
        with_app(|cx, window| {
            let home = muxel_store::data_dir().unwrap().join("w8lm-home");
            let announced = Uuid::new_v4().to_string();
            let (id, path, mut pending) = window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let path = w8lm_hook(&home, id, &announced, "clear", true);
                    (
                        id,
                        path,
                        app.prepare_terminal_spawn(id, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                    )
                })
                .unwrap();
            let resolution = w8lm_resolve(&mut pending, &home);
            let token = pending.request.key.token;
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(pending.request, resolution, cx)
                        .unwrap();
                    assert!(
                        command
                            .args
                            .windows(2)
                            .any(|args| args == ["--resume", announced.as_str()])
                    );
                })
                .unwrap();
            std::fs::remove_file(path).unwrap();
            let mut next = window
                .update(cx, |app, _, _| {
                    app.prepare_claude_launch(id, token)
                        .unwrap()
                        .pending
                        .unwrap()
                })
                .unwrap();
            let resolution = w8lm_resolve(&mut next, &home);
            let fresh = window
                .update(cx, |app, _, cx| {
                    let (meta, command, _) = app
                        .apply_claude_launch(next.request, resolution, cx)
                        .unwrap();
                    assert!(
                        !meta.was_resume,
                        "an adopted clear hook revived a deleted transcript"
                    );
                    assert!(command.args.iter().any(|arg| arg == "--session-id"));
                    assert!(!command.args.iter().any(|arg| arg == "--resume"));
                    assert_ne!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(announced.as_str())
                    );
                    app.workspace
                        .instance(id)
                        .unwrap()
                        .session_id
                        .clone()
                        .unwrap()
                })
                .unwrap();
            let mut again = window
                .update(cx, |app, _, _| {
                    app.prepare_claude_launch(id, token)
                        .unwrap()
                        .pending
                        .unwrap()
                })
                .unwrap();
            let resolution = w8lm_resolve(&mut again, &home);
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(again.request, resolution, cx)
                        .unwrap();
                    assert!(!command.args.iter().any(|arg| arg == "--resume"));
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(fresh.as_str()),
                        "retained hook revived after fresh recovery"
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn w8lm_consumed_hook_does_not_undo_fresh_start_but_a_new_resume_to_same_id_works() {
        with_app(|cx, window| {
            let home = muxel_store::data_dir().unwrap().join("w8lm-home");
            let announced = Uuid::new_v4().to_string();
            let (id, mut pending) = window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    w8lm_hook(&home, id, &announced, "resume", true);
                    (
                        id,
                        app.prepare_terminal_spawn(id, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                    )
                })
                .unwrap();
            let record =
                crate::session_binding::claude_binding_path(&muxel_store::data_dir().unwrap(), id);
            let first_modified = std::fs::metadata(&record).unwrap().modified().unwrap();
            let first_receipt =
                crate::session_binding::claude_hook_identity(&muxel_store::data_dir().unwrap(), id)
                    .unwrap();
            let token = pending.request.key.token;
            let resolution = w8lm_resolve(&mut pending, &home);
            window
                .update(cx, |app, _, cx| {
                    app.apply_claude_launch(pending.request, resolution, cx)
                        .unwrap();
                    let row = app.workspace.instance_mut(id).unwrap();
                    assert!(row.claude_hook_record.is_some());
                    row.session_id = None;
                    row.session_path = None;
                    row.session_started = false;
                    app.bump_session_binding_generation(id);
                })
                .unwrap();
            let mut fresh = window
                .update(cx, |app, _, _| {
                    app.prepare_claude_launch(id, token)
                        .unwrap()
                        .pending
                        .unwrap()
                })
                .unwrap();
            let resolution = w8lm_resolve(&mut fresh, &home);
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(fresh.request, resolution, cx)
                        .unwrap();
                    assert!(!command.args.iter().any(|arg| arg == "--resume"));
                    assert_ne!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(announced.as_str())
                    );
                })
                .unwrap();
            assert!(
                !record.exists(),
                "master's launch recovery acknowledged the hook"
            );
            w8lm_hook(&home, id, &announced, "resume", true);
            std::fs::File::options()
                .write(true)
                .open(&record)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(first_modified))
                .unwrap();
            assert_ne!(
                crate::session_binding::claude_hook_identity(&muxel_store::data_dir().unwrap(), id)
                    .unwrap(),
                first_receipt,
                "new stamped publication must differ even at the same mtime"
            );
            let mut resumed = window
                .update(cx, |app, _, _| {
                    app.prepare_claude_launch(id, token)
                        .unwrap()
                        .pending
                        .unwrap()
                })
                .unwrap();
            let resolution = w8lm_resolve(&mut resumed, &home);
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(resumed.request, resolution, cx)
                        .unwrap();
                    assert!(
                        command
                            .args
                            .windows(2)
                            .any(|args| args == ["--resume", announced.as_str()])
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn w8lm_missing_claude_store_does_not_refuse_an_unrelated_fresh_pane() {
        with_app(|cx, window| {
            let home = muxel_store::data_dir().unwrap().join("w8lm-absent-home");
            let (mut old, mut fresh) = window
                .update(cx, |app, window, cx| {
                    let old = pane(app);
                    let fresh = pane(app);
                    let old_id = app
                        .workspace
                        .instance(old)
                        .unwrap()
                        .session_id
                        .clone()
                        .unwrap();
                    w8lm_hook(&home, old, &old_id, "clear", false);
                    let row = app.workspace.instance_mut(fresh).unwrap();
                    row.session_id = None;
                    row.session_started = false;
                    (
                        app.prepare_terminal_spawn(old, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                        app.prepare_terminal_spawn(fresh, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                    )
                })
                .unwrap();
            assert!(!home.join(".claude/projects").exists());
            old.request.home = Some(home.clone());
            fresh.request.home = Some(home);
            resolve_batch(vec![
                (old.request.clone(), old.sender.take().unwrap()),
                (fresh.request.clone(), fresh.sender.take().unwrap()),
            ]);
            let resolution = fresh.receiver.recv_blocking().unwrap();
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(fresh.request, resolution, cx)
                        .expect(
                            "a missing store and another pane's retained hook refused a fresh pane",
                        );
                    assert!(command.args.iter().any(|arg| arg == "--session-id"));
                })
                .unwrap();
        });
    }

    #[test]
    fn w8lm_h2_legacy_moved_transcript_receipt_cannot_undo_a_later_fresh_start() {
        with_app(|cx, window| {
            let home = muxel_store::data_dir().unwrap().join("h2-legacy-home");
            let (id, announced, mut pending) = window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let announced = app
                        .workspace
                        .instance(id)
                        .unwrap()
                        .session_id
                        .clone()
                        .unwrap();
                    let old = w8lm_hook_with_format(&home, id, &announced, "clear", false, false);
                    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
                    let moved = muxel_store::data_dir()
                        .unwrap()
                        .join("moved-transcript-cwd");
                    std::fs::create_dir_all(&moved).unwrap();
                    let transcript = muxel_core::claude_session_path(&home, &moved, &announced);
                    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
                    std::fs::write(transcript, serde_json::json!({"cwd":moved}).to_string())
                        .unwrap();
                    (
                        id,
                        announced,
                        app.prepare_terminal_spawn(id, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                    )
                })
                .unwrap();
            let token = pending.request.key.token;
            let resolution = w8lm_resolve(&mut pending, &home);
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(pending.request, resolution, cx)
                        .unwrap();
                    assert!(
                        command
                            .args
                            .windows(2)
                            .any(|args| args == ["--resume", announced.as_str()])
                    );
                    let row = app.workspace.instance_mut(id).unwrap();
                    row.session_id = None;
                    row.session_path = None;
                    row.session_started = false;
                    app.bump_session_binding_generation(id);
                })
                .unwrap();
            let mut fresh = window
                .update(cx, |app, _, _| {
                    app.prepare_claude_launch(id, token)
                        .unwrap()
                        .pending
                        .unwrap()
                })
                .unwrap();
            let resolution = w8lm_resolve(&mut fresh, &home);
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(fresh.request, resolution, cx)
                        .unwrap();
                    assert!(
                        !command.args.iter().any(|arg| arg == "--resume"),
                        "legacy moved receipt undid a fresh-session reset"
                    );
                    assert_ne!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(announced.as_str())
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn w8lm_h2_new_clear_record_with_current_uuid_is_unconsumed() {
        for prior_marker in [false, true] {
            with_app(|cx, window| {
                let home = muxel_store::data_dir().unwrap().join("h2-new-record-home");
                let (announced, mut pending) = window
                    .update(cx, |app, window, cx| {
                        let id = pane(app);
                        let announced = app
                            .workspace
                            .instance(id)
                            .unwrap()
                            .session_id
                            .clone()
                            .unwrap();
                        let transcript = w8lm_hook(&home, id, &announced, "resume", true);
                        if prior_marker {
                            app.workspace.instance_mut(id).unwrap().claude_hook_record =
                                crate::session_binding::claude_hook_identity(
                                    &muxel_store::data_dir().unwrap(),
                                    id,
                                )
                                .unwrap();
                        }
                        std::fs::remove_file(transcript).unwrap();
                        w8lm_hook(&home, id, &announced, "clear", false);
                        (
                            announced,
                            app.prepare_terminal_spawn(id, window, cx)
                                .unwrap()
                                .pending
                                .unwrap(),
                        )
                    })
                    .unwrap();
                let resolution = w8lm_resolve(&mut pending, &home);
                window.update(cx,|app,_,cx|{
                    let (_,command,_)=app.apply_claude_launch(pending.request,resolution,cx).unwrap();
                    assert!(command.args.windows(2).any(|args|args==["--resume",announced.as_str()]),
                        "new same-UUID clear was mistaken for an adopted receipt (prior marker={prior_marker})");
                }).unwrap();
            });
        }
    }

    #[test]
    fn w8lm_one_unreadable_hook_does_not_refuse_other_panes() {
        use std::os::windows::fs::OpenOptionsExt;
        with_app(|cx, window| {
            let home = muxel_store::data_dir().unwrap().join("w8lm-home");
            let (old_id, mut old, mut fresh) = window
                .update(cx, |app, window, cx| {
                    let old = pane(app);
                    let fresh = pane(app);
                    let id = app
                        .workspace
                        .instance(old)
                        .unwrap()
                        .session_id
                        .clone()
                        .unwrap();
                    w8lm_hook(&home, old, &id, "resume", true);
                    let row = app.workspace.instance_mut(fresh).unwrap();
                    row.session_id = None;
                    row.session_started = false;
                    (
                        old,
                        app.prepare_terminal_spawn(old, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                        app.prepare_terminal_spawn(fresh, window, cx)
                            .unwrap()
                            .pending
                            .unwrap(),
                    )
                })
                .unwrap();
            let record = crate::session_binding::claude_binding_path(
                &muxel_store::data_dir().unwrap(),
                old_id,
            );
            let _lock = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(record)
                .unwrap();
            old.request.home = Some(home.clone());
            fresh.request.home = Some(home);
            resolve_batch(vec![
                (old.request.clone(), old.sender.take().unwrap()),
                (fresh.request.clone(), fresh.sender.take().unwrap()),
            ]);
            let resolution = fresh.receiver.recv_blocking().unwrap();
            window
                .update(cx, |app, _, cx| {
                    assert!(
                        app.apply_claude_launch(fresh.request, resolution, cx)
                            .is_some(),
                        "one pane's read error refused an unrelated fresh pane"
                    );
                })
                .unwrap();
        });
    }
}
