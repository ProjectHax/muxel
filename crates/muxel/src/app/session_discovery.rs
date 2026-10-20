//! Recurring provider reads and process enumeration run on workers. The UI
//! claims matching validated snapshots and acknowledges only after persistence.

#[cfg(windows)]
use super::{
    GrokRegistryRead, active_grok_session_for_process, grok_switch_interval, process_parent_map,
    read_grok_active_sessions,
};
use super::{
    MuxelApp, home_dir, is_claude_program, is_codex_program, is_grok_program,
    validated_codex_session_id,
};
use crate::{session_binding, ui_profile};
use gpui::{App, Context};
use muxel_core::AgentPreset;
use muxel_core::session_discovery::{
    SessionDiscoveryKey, SessionDiscoveryRequest, SessionProvider,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Instant, SystemTime};
use uuid::Uuid;

enum DiscoveryCandidate {
    Unresolved,
    Session {
        session_id: String,
        switched: Option<(SystemTime, SystemTime)>,
    },
    Claude {
        snapshot: session_binding::ClaudeBindingSnapshot,
        binding: Option<session_binding::ClaudeBinding>,
    },
}

impl DiscoveryCandidate {
    fn session_id(&self) -> Option<&str> {
        match self {
            Self::Unresolved => None,
            Self::Session { session_id, .. } => Some(session_id),
            Self::Claude { binding, .. } => {
                binding.as_ref().map(|binding| binding.session_id.as_str())
            }
        }
    }
}

struct DiscoveryBatch {
    results: Vec<(SessionDiscoveryRequest, DiscoveryCandidate)>,
    grok_read: Option<(SystemTime, SystemTime)>,
}

#[cfg(windows)]
pub(super) struct GrokObservation {
    launch: Instant,
    child_pid: Option<u32>,
    session_id: Option<String>,
    read: GrokRegistryRead,
}

#[cfg(windows)]
impl GrokObservation {
    fn new(key: &SessionDiscoveryKey, read: GrokRegistryRead) -> Self {
        Self {
            launch: key.launch,
            child_pid: key.child_pid,
            session_id: key.binding.clone(),
            read,
        }
    }

    fn matches(&self, key: &SessionDiscoveryKey) -> bool {
        key.provider == SessionProvider::Grok
            && self.launch == key.launch
            && self.child_pid == key.child_pid
    }
}

struct ClaudeCommit {
    record: session_binding::ClaimedClaudeRecord,
    launch: Uuid,
    stamp: Option<session_binding::ClaudeHookStamp>,
}

struct SessionBindingUpdate {
    pane: Uuid,
    previous_id: Option<String>,
    previous_started: bool,
    previous_auto_name: Option<String>,
    switched: Option<(SystemTime, SystemTime)>,
    claude: Option<ClaudeCommit>,
}

/// A pane can reserve its saved ID and an accepted ID whose save is pending.
/// Construct once per batch, then update claims as each result is staged.
#[derive(Default)]
struct SessionOwnership {
    identities: HashMap<String, HashSet<Uuid>>,
    positions: HashMap<Uuid, usize>,
}

impl SessionOwnership {
    fn claim(&mut self, pane: Uuid, id: &str) {
        if let Some(owners) = self.identities.get_mut(id) {
            owners.insert(pane);
        } else {
            self.identities.insert(id.to_owned(), HashSet::from([pane]));
        }
    }

    fn bound_elsewhere(&self, pane: Uuid, id: &str) -> bool {
        self.identities
            .get(id)
            .is_some_and(|owners| owners.len() > 1 || !owners.contains(&pane))
    }

    fn replace(&mut self, pane: Uuid, old: [Option<&str>; 2], new: [Option<&str>; 2]) {
        for id in old
            .into_iter()
            .flatten()
            .filter(|id| !new.contains(&Some(id)))
        {
            if let Some(owners) = self.identities.get_mut(id) {
                owners.remove(&pane);
                if owners.is_empty() {
                    self.identities.remove(id);
                }
            }
        }
        for id in new
            .into_iter()
            .flatten()
            .filter(|id| !old.contains(&Some(id)))
        {
            self.claim(pane, id);
        }
    }
}

impl MuxelApp {
    pub(super) fn bump_session_binding_generation(&mut self, iid: Uuid) {
        self.session_discoveries.binding_changed(iid);
    }

    fn discovery_terminal(&self, iid: Uuid) -> Option<&gpui::Entity<muxel_terminal::TerminalView>> {
        self.terminal_view(iid)
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
        #[cfg(windows)]
        if !self.grok_observations.is_empty() {
            let panes: std::collections::HashSet<_> = self
                .workspace
                .instances
                .iter()
                .map(|instance| instance.id)
                .collect();
            self.grok_observations
                .retain(|pane, _| panes.contains(pane));
        }
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
            #[cfg(windows)]
            let previous_grok_reads = requests
                .iter()
                .filter_map(|request| {
                    self.grok_read_for(&request.key)
                        .map(|read| (request.key.pane, (read.started, read.finished)))
                })
                .collect();
            #[cfg(not(windows))]
            let previous_grok_reads = HashMap::new();
            cx.spawn(async move |this, cx| {
                let (batch, data_dir) = cx
                    .background_executor()
                    .spawn(async move {
                        let batch = discover_sessions(
                            requests,
                            &home,
                            data_dir.as_deref(),
                            previous_grok_reads,
                        );
                        (batch, data_dir)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.apply_discovery_batch(batch, data_dir.as_deref(), cx);
                });
            })
            .detach();
        }
    }

    fn apply_discovery_batch(
        &mut self,
        batch: DiscoveryBatch,
        data_dir: Option<&Path>,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(windows))]
        let _ = batch.grok_read;
        let mut ownership = self.session_ownership(cx);
        let mut updates = Vec::new();
        for (request, candidate) in batch.results {
            let iid = request.key.pane;
            let current = self.session_discovery_key(iid, cx);
            let unowned = candidate
                .session_id()
                .is_some_and(|id| !ownership.bound_elsewhere(iid, id));
            let valid = self.session_discoveries.finish(
                &request,
                current.as_ref(),
                Instant::now(),
                unowned,
            );
            if !valid {
                // Stale workers never claim or acknowledge a receipt.
                continue;
            }
            let previous_id = current.as_ref().and_then(|key| key.binding.as_deref());
            #[cfg(windows)]
            let previous_grok = current
                .as_ref()
                .filter(|key| key.provider == SessionProvider::Grok)
                .and_then(|_| self.live_grok_session_id(iid, cx))
                .map(str::to_owned);
            #[cfg(not(windows))]
            let previous_grok: Option<String> = None;
            #[cfg(windows)]
            if request.key.provider == SessionProvider::Grok
                && unowned
                && let Some((started, finished)) = batch.grok_read
            {
                self.record_grok_read(&request.key, GrokRegistryRead { started, finished });
            }
            let update = match candidate {
                DiscoveryCandidate::Claude { snapshot, binding } => {
                    // Only this UI owner and prelaunch recovery may claim/ack.
                    // A newer receipt must wait for its own validation.
                    let Some(record) = data_dir.and_then(|data_dir| {
                        claim_validated_claude_record(data_dir, iid, &snapshot)
                    }) else {
                        continue;
                    };
                    if unowned {
                        self.stage_discovered_claude(iid, record, binding, cx)
                    } else {
                        session_binding::acknowledge_claude_binding(record);
                        None
                    }
                }
                DiscoveryCandidate::Session {
                    session_id,
                    switched,
                } if unowned => {
                    #[cfg(windows)]
                    if request.key.provider == SessionProvider::Grok {
                        self.stage_grok_session(
                            &request.key,
                            session_id,
                            switched.expect("Grok result carries its observation interval"),
                            cx,
                        )
                    } else {
                        self.stage_discovered_session(iid, session_id, false)
                    }
                    #[cfg(not(windows))]
                    {
                        let _ = switched;
                        self.stage_discovered_session(iid, session_id, false)
                    }
                }
                _ => None,
            };
            let current_id = ownership
                .positions
                .get(&iid)
                .and_then(|index| self.workspace.instances.get(*index))
                .and_then(|instance| instance.session_id.as_deref());
            #[cfg(windows)]
            let current_grok = current
                .as_ref()
                .filter(|key| key.provider == SessionProvider::Grok)
                .and_then(|_| self.live_grok_session_id(iid, cx));
            #[cfg(not(windows))]
            let current_grok = None;
            ownership.replace(
                iid,
                [previous_id, previous_grok.as_deref()],
                [current_id, current_grok],
            );
            if let Some(update) = update {
                updates.push(update);
            }
        }
        self.finish_discovered_sessions(updates, cx);
    }

    fn session_ownership(&self, cx: &App) -> SessionOwnership {
        let mut ownership = SessionOwnership::default();
        #[cfg(windows)]
        let local_projects: HashSet<_> = self
            .workspace
            .projects
            .iter()
            .filter(|project| project.remote.is_none())
            .map(|project| project.id)
            .collect();
        #[cfg(not(windows))]
        let _ = cx;
        for (index, instance) in self.workspace.instances.iter().enumerate() {
            ownership.positions.entry(instance.id).or_insert(index);
            if let Some(id) = &instance.session_id {
                ownership.claim(instance.id, id);
            }
            #[cfg(windows)]
            if instance.kind == muxel_core::InstanceKind::Terminal
                && is_grok_program(instance.program.as_deref())
                && instance.tmux_session.is_none()
                && local_projects.contains(&instance.project_id)
                && let Some(id) = self.live_grok_session_id(instance.id, cx)
            {
                ownership.claim(instance.id, id);
            }
        }
        ownership
    }

    #[cfg(windows)]
    fn live_grok_session_id(&self, iid: Uuid, cx: &App) -> Option<&str> {
        let observation = self.grok_observations.get(&iid)?;
        if self.terminal_launches.get(&iid)?.0 != observation.launch {
            return None;
        }
        let pid = self.discovery_terminal(iid)?.read(cx).child_pid()?;
        if observation.child_pid != Some(pid) {
            return None;
        }
        observation.session_id.as_deref()
    }

    #[cfg(windows)]
    pub(super) fn reconcile_grok_before_restart(
        &mut self,
        iid: Uuid,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(instance) = self.workspace.instance(iid) else {
            return Ok(());
        };
        if instance.kind != muxel_core::InstanceKind::Terminal
            || !is_grok_program(instance.program.as_deref())
            || instance.tmux_session.is_some()
            || self
                .workspace
                .project(instance.project_id)
                .is_none_or(|project| project.remote.is_some())
        {
            return Ok(());
        }
        let Some(identity) = self.live_grok_session_id(iid, cx).map(str::to_owned) else {
            return Ok(());
        };
        if instance.session_id.as_deref() == Some(identity.as_str()) {
            return Ok(());
        }
        if self.session_ownership(cx).bound_elsewhere(iid, &identity) {
            anyhow::bail!(
                crate::i18n::t("Another pane owns the current conversation.").to_string()
            );
        }
        let Some(update) = self.stage_discovered_session(iid, identity, false) else {
            return Ok(());
        };
        if !self.finish_discovered_sessions(vec![update], cx) {
            anyhow::bail!(
                crate::i18n::t("Could not save the current conversation before restart.")
                    .to_string()
            );
        }
        Ok(())
    }

    #[cfg(windows)]
    fn grok_read_for(&self, key: &SessionDiscoveryKey) -> Option<GrokRegistryRead> {
        self.grok_observations
            .get(&key.pane)
            .filter(|observation| observation.matches(key))
            .map(|observation| observation.read)
    }

    #[cfg(windows)]
    fn record_grok_read(&mut self, key: &SessionDiscoveryKey, read: GrokRegistryRead) {
        let observation = self
            .grok_observations
            .entry(key.pane)
            .or_insert_with(|| GrokObservation::new(key, read));
        if !observation.matches(key) {
            *observation = GrokObservation::new(key, read);
        } else if read.finished >= observation.read.finished {
            observation.read = read;
        }
    }

    #[cfg(windows)]
    fn stage_grok_session(
        &mut self,
        key: &SessionDiscoveryKey,
        session_id: String,
        interval: (SystemTime, SystemTime),
        cx: &App,
    ) -> Option<SessionBindingUpdate> {
        // Called only for an accepted worker result after its read is recorded.
        // Durable metadata can still say A after a failed save of live B.
        let observation = self
            .grok_observations
            .get_mut(&key.pane)
            .filter(|observation| observation.matches(key))?;
        let changed = observation.session_id.as_deref() != Some(session_id.as_str());
        if changed {
            observation.session_id = Some(session_id.clone());
        }
        if changed && let Some(view) = self.discovery_terminal(key.pane) {
            view.read(cx).provider_switched(interval.0, interval.1);
        }
        // Retrying persistence must not create another runtime boundary.
        self.stage_discovered_session(key.pane, session_id, false)
    }

    fn stage_discovered_claude(
        &mut self,
        iid: Uuid,
        record: session_binding::ClaimedClaudeRecord,
        binding: Option<session_binding::ClaudeBinding>,
        cx: &App,
    ) -> Option<SessionBindingUpdate> {
        let Some(binding) = binding else {
            session_binding::acknowledge_claude_binding(record);
            return None;
        };
        let launch = self.discovery_terminal(iid)?.read(cx).session().id;
        let seen = self
            .claude_hook_seen
            .get(&iid)
            .filter(|(seen_launch, _)| *seen_launch == launch)
            .map(|(_, nonce)| *nonce);
        let bound = self.workspace.instance(iid)?.session_id.as_deref();
        if !session_binding::claude_record_switches(&binding, seen, bound) {
            self.remember_claude_hook(iid, Some(launch), binding.stamp);
            session_binding::acknowledge_claude_binding(record);
            return None;
        }
        let switched_at = binding
            .stamp
            .map(|stamp| stamp.recorded_at)
            .or(record.modified)
            .unwrap_or_else(SystemTime::now);
        // A new nonce can report A -> B -> A between reads even when the final
        // UUID equals the saved one. It still needs a runtime boundary.
        let mut update = self.stage_discovered_session(iid, binding.session_id, true)?;
        update.switched = Some((switched_at, switched_at));
        update.claude = Some(ClaudeCommit {
            record,
            launch,
            stamp: binding.stamp,
        });
        Some(update)
    }

    fn stage_discovered_session(
        &mut self,
        iid: Uuid,
        session_id: String,
        force_boundary: bool,
    ) -> Option<SessionBindingUpdate> {
        let instance = self.workspace.instance_mut(iid)?;
        if !force_boundary && instance.session_id.as_deref() == Some(session_id.as_str()) {
            return None;
        }
        let update = SessionBindingUpdate {
            pane: iid,
            previous_id: instance.session_id.clone(),
            previous_started: instance.session_started,
            previous_auto_name: instance.auto_name.clone(),
            switched: None,
            claude: None,
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
    ) -> bool {
        if updates.is_empty() {
            return true;
        }
        if self.try_persist() {
            for update in updates {
                if let Some(claude) = update.claude {
                    self.remember_claude_hook(update.pane, Some(claude.launch), claude.stamp);
                    session_binding::acknowledge_claude_binding(claude.record);
                }
                if let Some((from, to)) = update.switched
                    && let Some(view) = self.discovery_terminal(update.pane)
                {
                    view.read(cx).provider_switched(from, to);
                }
            }
            cx.notify();
            true
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
                // Claimed Claude files are intentionally not acknowledged, and
                // their nonce is not remembered until the save succeeds.
            }
            false
        }
    }
}

fn discover_sessions(
    requests: Vec<SessionDiscoveryRequest>,
    home: &Path,
    data_dir: Option<&Path>,
    previous_grok_reads: HashMap<Uuid, (SystemTime, SystemTime)>,
) -> DiscoveryBatch {
    #[cfg(not(windows))]
    let _ = previous_grok_reads;
    #[cfg(windows)]
    let mut grok_snapshot = None;
    let results = requests
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
                )
                .map_or(DiscoveryCandidate::Unresolved, |session_id| {
                    DiscoveryCandidate::Session {
                        session_id,
                        switched: None,
                    }
                }),
                SessionProvider::Claude => data_dir
                    .and_then(|data_dir| {
                        session_binding::read_pending_claude_binding(data_dir, key.pane)
                    })
                    .map_or(DiscoveryCandidate::Unresolved, |snapshot| {
                        let binding =
                            session_binding::claude_binding_from_record(&snapshot.bytes, home);
                        DiscoveryCandidate::Claude { snapshot, binding }
                    }),
                SessionProvider::Grok => {
                    #[cfg(windows)]
                    {
                        let (read, active, parents) = grok_snapshot.get_or_insert_with(|| {
                            let started = SystemTime::now();
                            let active = read_grok_active_sessions();
                            let read = GrokRegistryRead {
                                started,
                                finished: SystemTime::now(),
                            };
                            (read, active, process_parent_map())
                        });
                        let previous = previous_grok_reads
                            .get(&key.pane)
                            .copied()
                            .map(|(started, finished)| GrokRegistryRead { started, finished });
                        key.child_pid
                            .and_then(|pid| active_grok_session_for_process(pid, active, parents))
                            .map_or(DiscoveryCandidate::Unresolved, |session| {
                                DiscoveryCandidate::Session {
                                    session_id: session.session_id.clone(),
                                    switched: Some(grok_switch_interval(session, previous, *read)),
                                }
                            })
                    }
                    #[cfg(not(windows))]
                    {
                        DiscoveryCandidate::Unresolved
                    }
                }
            };
            ui_profile::session_lookup_finished(
                ui_profile::SessionLookupKind::Discovery,
                Some(key.provider),
                key.pane,
                started.elapsed(),
                if candidate.session_id().is_some() {
                    ui_profile::SessionLookupOutcome::Found
                } else {
                    ui_profile::SessionLookupOutcome::Unresolved
                },
            );
            (request, candidate)
        })
        .collect();
    #[cfg(windows)]
    let grok_read = grok_snapshot.map(|(read, _, _)| (read.started, read.finished));
    #[cfg(not(windows))]
    let grok_read = None;
    DiscoveryBatch { results, grok_read }
}

/// Called only after the UI has validated the worker key and launch owner.
/// A mismatching claim stays pending for the next worker; dropping it never acks.
fn claim_validated_claude_record(
    data_dir: &Path,
    pane: Uuid,
    snapshot: &session_binding::ClaudeBindingSnapshot,
) -> Option<session_binding::ClaimedClaudeRecord> {
    let record = session_binding::claim_claude_binding(data_dir, pane)?;
    if record.bytes != snapshot.bytes || record.modified != snapshot.modified {
        return None;
    }
    Some(record)
}

#[cfg(all(test, windows))]
mod grok_regressions {
    use super::super::{
        AgentStatus, GrokActiveSession, GrokRegistryRead, LoopCleanup, LoopRun, MuxelApp,
        PostRunAction, grok_switch_interval, popout_lifecycle_tests::with_app,
    };
    use super::{DiscoveryBatch, DiscoveryCandidate};
    use gpui::{AppContext as _, Context, TestAppContext, Window, WindowHandle};
    use muxel_core::{AgentPreset, InjectionMode, Instance, Project};
    use muxel_terminal::{
        CommandSpec, TerminalLaunch, TerminalRuntimeSnapshot, TerminalSession,
        TerminalStatusCoverage, TerminalStatusReason, TerminalView,
    };
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};
    use uuid::Uuid;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    struct StopChild(Arc<TerminalSession>);
    impl Drop for StopChild {
        fn drop(&mut self) {
            self.0.kill();
        }
    }

    struct ChildHandle(HANDLE);
    impl ChildHandle {
        fn new(session: &TerminalSession) -> Self {
            // SAFETY: this is the PTY created by this fixture. Retain the handle
            // before Restart so PID reuse cannot satisfy a liveness assertion.
            Self(unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    session.child_pid().unwrap(),
                )
                .unwrap()
            })
        }

        fn running(&self) -> bool {
            // SAFETY: the retained handle stays open until this guard drops.
            let status = unsafe { WaitForSingleObject(self.0, 0) };
            if status == WAIT_TIMEOUT {
                true
            } else {
                assert_eq!(status, WAIT_OBJECT_0);
                false
            }
        }
    }
    impl Drop for ChildHandle {
        fn drop(&mut self) {
            // SAFETY: this guard exclusively owns the retained process handle.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn checked_update<R>(
        window: WindowHandle<MuxelApp>,
        cx: &mut TestAppContext,
        run: impl FnOnce(&mut MuxelApp, &mut Window, &mut Context<MuxelApp>) -> R,
    ) -> R {
        // Restore GPUI's app lease before reporting a failed assertion. Owned
        // PTY guards still unwind inside the callback before cleanup runs.
        let result = window
            .update(cx, |app, window, cx| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(app, window, cx)))
            })
            .unwrap();
        match result {
            Ok(value) => value,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    fn pane(
        app: &mut MuxelApp,
        window: &mut Window,
        cx: &mut Context<MuxelApp>,
    ) -> (Uuid, Arc<TerminalSession>) {
        let cwd = muxel_store::data_dir()
            .unwrap()
            .join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&cwd).unwrap();
        let project = app.workspace.add_project(Project::new("Grok fixture", cwd));
        let mut instance = Instance::shell(project);
        instance.program = Some("grok.exe".into());
        instance.session_id = Some(Uuid::new_v4().to_string());
        instance.session_started = true;
        let id = instance.id;
        app.workspace.add_instance(instance);
        let launch = TerminalLaunch::spawn(
            CommandSpec::program(
                "cmd.exe",
                vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
            ),
            (80, 24),
        )
        .unwrap();
        let view = cx.new(|cx| TerminalView::new(launch, id, Instant::now(), window, cx));
        let session = view.read(cx).session().clone();
        app.terminals.insert(id, view);
        app.terminal_launches.insert(id, (Instant::now(), false));
        (id, session)
    }

    fn complete_grok(app: &mut MuxelApp, id: Uuid, identity: &str, cx: &mut Context<MuxelApp>) {
        let key = app.session_discovery_key(id, cx).unwrap();
        let request = app.session_discoveries.begin(key, Instant::now()).unwrap();
        let now = SystemTime::now();
        app.apply_discovery_batch(
            DiscoveryBatch {
                results: vec![(
                    request,
                    DiscoveryCandidate::Session {
                        session_id: identity.into(),
                        switched: Some((now, now)),
                    },
                )],
                grok_read: Some((now, now)),
            },
            muxel_store::data_dir().as_deref(),
            cx,
        );
    }

    #[test]
    fn gasq_restart_preserves_unsaved_grok_and_then_resumes_its_accepted_identity() {
        with_app(|cx, window| {
            let (old_process, replacement, report, identity) = checked_update(
                window,
                cx,
                |app, window, cx| {
                    app.current_workspace = Some(Uuid::new_v4());
                    app.use_tmux = false;
                    app.use_worktree = false;
                    let (id, session) = pane(app, window, cx);
                    let _stop_old = StopChild(session.clone());
                    let old_process = ChildHandle::new(&session);
                    let root = muxel_store::data_dir()
                        .unwrap()
                        .join("restart-grok-fixture");
                    std::fs::create_dir_all(&root).unwrap();
                    let program = root.join("grok.cmd");
                    let report = root.join("resume.txt");
                    std::fs::write(
                        &program,
                        "@echo off\r\n> \"%~dp0resume.txt\" echo %~1\r\n>> \"%~dp0resume.txt\" echo %~2\r\nmore >nul\r\n",
                    )
                    .unwrap();
                    let mut preset = AgentPreset::grok();
                    // The store's canonical test root has a Windows verbatim
                    // prefix. cmd cannot execute a batch file under that prefix.
                    let program = program.to_string_lossy();
                    preset.program =
                        Some(program.strip_prefix(r"\\?\").unwrap_or(&program).to_owned());
                    preset.injection = InjectionMode::None;
                    let instance = app.workspace.instance_mut(id).unwrap();
                    instance.preset_id = Some(preset.id);
                    instance.preset = preset.name.clone();
                    instance.program = preset.program.clone();
                    instance.injection = InjectionMode::None;
                    app.presets.push(preset);
                    let old = app.workspace.instance(id).unwrap().session_id.clone();
                    let doc =
                        muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                    std::fs::create_dir_all(&doc).unwrap();
                    let identity = Uuid::new_v4().to_string();
                    complete_grok(app, id, &identity, cx);
                    assert_eq!(app.workspace.instance(id).unwrap().session_id, old);
                    let revision = session.binding_revision();
                    app.restart_agent(id, window, cx);
                    let current = app.terminal_view(id).unwrap().read(cx).session().clone();
                    let _stop_current = StopChild(current.clone());
                    assert_eq!(
                        current.id, session.id,
                        "failed-save Restart replaced the accepted live pane"
                    );
                    assert!(
                        old_process.running(),
                        "failed-save Restart killed the accepted conversation"
                    );
                    assert_eq!(session.binding_revision(), revision);
                    assert_eq!(app.workspace.instance(id).unwrap().session_id, old);
                    assert!(
                        !report.exists(),
                        "refused Restart still launched its fixture"
                    );
                    std::fs::remove_dir(&doc).unwrap();
                    app.restart_agent(id, window, cx);
                    let replacement =
                        StopChild(app.terminal_view(id).unwrap().read(cx).session().clone());
                    assert_ne!(replacement.0.id, session.id);
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(identity.as_str())
                    );
                    let saved = muxel_store::load_workspace_from(&doc).unwrap();
                    assert_eq!(
                        saved.instance(id).unwrap().session_id.as_deref(),
                        Some(identity.as_str())
                    );
                    (old_process, replacement, report, identity)
                },
            );
            let cap = Instant::now() + Duration::from_secs(10);
            let mut cursor_reply = Instant::now() - Duration::from_secs(1);
            loop {
                let selected = std::fs::read_to_string(&report).unwrap_or_default();
                if !old_process.running()
                    && selected.lines().collect::<Vec<_>>() == ["--resume", identity.as_str()]
                {
                    break;
                }
                // Headless ConPTY can wait for a cursor-position response before
                // starting cmd. Supply the same reply as the batch-runner test
                // without pumping the app's provider-registry polling tasks.
                if selected.is_empty() && cursor_reply.elapsed() >= Duration::from_millis(100) {
                    replacement.0.write_input(b"\x1b[1;1R");
                    cursor_reply = Instant::now();
                }
                assert!(
                    Instant::now() < cap,
                    "Restart fixture did not select the accepted identity at {}: {selected:?}",
                    report.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(replacement.0.child_pid().is_some());
        });
    }

    #[test]
    fn gasq_sibling_guard_reserves_matching_unsaved_grok_identity() {
        for change in ["matching", "launch", "pid", "removed"] {
            with_app(|cx, window| {
                checked_update(window, cx, |app, window, cx| {
                    app.current_workspace = Some(Uuid::new_v4());
                    let (owner, owner_session) = pane(app, window, cx);
                    let _stop_owner = StopChild(owner_session.clone());
                    let (other, other_session) = pane(app, window, cx);
                    let _stop_other = StopChild(other_session.clone());
                    let old_owner = app.workspace.instance(owner).unwrap().session_id.clone();
                    let old_other = app.workspace.instance(other).unwrap().session_id.clone();
                    let doc =
                        muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                    std::fs::create_dir_all(&doc).unwrap();
                    let identity = Uuid::new_v4().to_string();
                    complete_grok(app, owner, &identity, cx);
                    assert_eq!(app.workspace.instance(owner).unwrap().session_id, old_owner);
                    let revision = owner_session.binding_revision();
                    std::fs::remove_dir(&doc).unwrap();
                    match change {
                        "matching" => {}
                        "launch" => {
                            app.terminal_launches.get_mut(&owner).unwrap().0 +=
                                Duration::from_secs(1)
                        }
                        "pid" => app.grok_observations.get_mut(&owner).unwrap().child_pid = Some(0),
                        "removed" => app.workspace.instances.retain(|row| row.id != owner),
                        _ => unreachable!(),
                    }
                    complete_grok(app, other, &identity, cx);
                    if change != "matching" {
                        assert_eq!(
                            app.workspace.instance(other).unwrap().session_id.as_deref(),
                            Some(identity.as_str()),
                            "stale {change} observation blocked admission"
                        );
                        return;
                    }
                    assert_eq!(
                        app.workspace.instance(other).unwrap().session_id,
                        old_other,
                        "sibling adopted an accepted but unsaved Grok identity"
                    );
                    assert!(
                        !app.grok_observations.contains_key(&other),
                        "rejected candidate advanced its read bound"
                    );
                    complete_grok(app, owner, &identity, cx);
                    assert_eq!(
                        app.workspace.instance(owner).unwrap().session_id.as_deref(),
                        Some(identity.as_str())
                    );
                    assert_eq!(
                        owner_session.binding_revision(),
                        revision,
                        "save retry repeated the accepted switch"
                    );
                    let saved = muxel_store::load_workspace_from(&doc).unwrap();
                    assert_eq!(
                        saved
                            .instances
                            .iter()
                            .filter(|row| row.session_id.as_deref() == Some(identity.as_str()))
                            .count(),
                        1
                    );
                });
            });
        }
    }

    #[test]
    fn gasq_batch_releases_unsaved_identity_when_owner_switches_back_without_metadata_change() {
        with_app(|cx, window| {
            checked_update(window, cx, |app, window, cx| {
                app.current_workspace = Some(Uuid::new_v4());
                let (owner, owner_session) = pane(app, window, cx);
                let _stop_owner = StopChild(owner_session.clone());
                let (other, other_session) = pane(app, window, cx);
                let _stop_other = StopChild(other_session);
                let original = app
                    .workspace
                    .instance(owner)
                    .unwrap()
                    .session_id
                    .clone()
                    .unwrap();
                let doc = muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                std::fs::create_dir_all(&doc).unwrap();
                let accepted = Uuid::new_v4().to_string();
                complete_grok(app, owner, &accepted, cx);
                assert_eq!(
                    app.workspace.instance(owner).unwrap().session_id.as_deref(),
                    Some(original.as_str())
                );
                let revision = owner_session.binding_revision();
                std::fs::remove_dir(&doc).unwrap();
                let owner_key = app.session_discovery_key(owner, cx).unwrap();
                let other_key = app.session_discovery_key(other, cx).unwrap();
                let now = Instant::now();
                let owner_request = app.session_discoveries.begin(owner_key, now).unwrap();
                let other_request = app.session_discoveries.begin(other_key, now).unwrap();
                let read = SystemTime::now();
                app.apply_discovery_batch(
                    DiscoveryBatch {
                        results: vec![
                            (
                                owner_request,
                                DiscoveryCandidate::Session {
                                    session_id: original.clone(),
                                    switched: Some((read, read)),
                                },
                            ),
                            (
                                other_request,
                                DiscoveryCandidate::Session {
                                    session_id: accepted.clone(),
                                    switched: Some((read, read)),
                                },
                            ),
                        ],
                        grok_read: Some((read, read)),
                    },
                    muxel_store::data_dir().as_deref(),
                    cx,
                );
                assert_ne!(owner_session.binding_revision(), revision);
                assert_eq!(
                    app.workspace.instance(owner).unwrap().session_id.as_deref(),
                    Some(original.as_str())
                );
                assert_eq!(
                    app.grok_observations[&owner].session_id.as_deref(),
                    Some(original.as_str())
                );
                assert_eq!(
                    app.workspace.instance(other).unwrap().session_id.as_deref(),
                    Some(accepted.as_str()),
                    "unchanged metadata left the old live identity reserved for the rest of the batch"
                );
            });
        });
    }

    #[test]
    fn dtz5_grok_failed_save_abandons_the_old_exit_loop_and_dedupes_retries() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    app.current_workspace = Some(Uuid::new_v4());
                    let (id, session) = pane(app, window, cx);
                    let _stop = StopChild(session.clone());
                    let old = app
                        .workspace
                        .instance(id)
                        .unwrap()
                        .session_id
                        .clone()
                        .unwrap();
                    session.process_probe_output(format!("\x1b]0;{old} | Ready\x07").as_bytes());
                    session.mark_turn_submitted();
                    let revision = session.binding_revision();
                    let cached_done = TerminalRuntimeSnapshot {
                        status: AgentStatus::Done,
                        attributable: true,
                        reason: TerminalStatusReason::Quiet,
                        coverage: TerminalStatusCoverage::ProviderVisible,
                        launch_id: session.id,
                        binding_rev: revision,
                        snapshot_seq: 1,
                        input_rev: 0,
                        draft_latched: false,
                        completion_ready: true,
                    };
                    let mut run = LoopRun {
                        loop_id: Uuid::new_v4(),
                        launch: Some(session.id),
                        seen_working: true,
                        started: Instant::now(),
                        post_run: PostRunAction::Exit,
                    };
                    let doc =
                        muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                    std::fs::create_dir_all(&doc).unwrap();
                    let new = Uuid::new_v4().to_string();
                    let accept =
                        |app: &mut MuxelApp, cx: &mut Context<MuxelApp>, identity: &str| {
                            let key = app.session_discovery_key(id, cx).unwrap();
                            let now = SystemTime::now();
                            app.record_grok_read(
                                &key,
                                GrokRegistryRead {
                                    started: now,
                                    finished: now,
                                },
                            );
                            let update = app
                                .stage_grok_session(&key, identity.to_owned(), (now, now), cx)
                                .unwrap();
                            assert_eq!(
                                app.workspace.instance(id).unwrap().session_id.as_deref(),
                                Some(identity)
                            );
                            app.finish_discovered_sessions(vec![update], cx);
                        };
                    accept(app, cx, &new);
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(old.as_str()),
                        "fixture must fail the real workspace save"
                    );
                    assert_eq!(
                        run.cleanup(
                            cached_done,
                            session.binding_revision(),
                            session.first_submission_binding(),
                            Duration::ZERO
                        ),
                        LoopCleanup::Abandon,
                        "failed persistence left A's Exit loop owning B"
                    );
                    let observed = session.binding_revision();
                    assert_ne!(observed, revision);
                    accept(app, cx, &new);
                    assert_eq!(
                        session.binding_revision(),
                        observed,
                        "retrying the same live identity invalidated it again"
                    );
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(old.as_str())
                    );
                    std::fs::remove_dir(&doc).unwrap();
                    accept(app, cx, &new);
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(new.as_str())
                    );
                    assert_eq!(
                        session.binding_revision(),
                        observed,
                        "successful save repeated the accepted switch"
                    );
                    accept(app, cx, &Uuid::new_v4().to_string());
                    assert_ne!(
                        session.binding_revision(),
                        observed,
                        "a later genuine switch was suppressed"
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn dtz5_overlapping_grok_panes_keep_the_target_legacy_title_and_read_bound() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    app.current_workspace = Some(Uuid::new_v4());
                    let (target, session) = pane(app, window, cx);
                    let _stop_target = StopChild(session.clone());
                    let (other, other_session) = pane(app, window, cx);
                    let _stop_other = StopChild(other_session);
                    let key = app.session_discovery_key(target, cx).unwrap();
                    let foreign = app.session_discovery_key(other, cx).unwrap();
                    // The target worker reads A, then stalls. B's title arrives
                    // before an unrelated pane completes a later registry read.
                    let old_read = GrokRegistryRead {
                        started: SystemTime::now(),
                        finished: SystemTime::now(),
                    };
                    let title = "Current Grok B | Ready";
                    session.process_probe_output(format!("\x1b]0;{title}\x07").as_bytes());
                    assert_eq!(session.title().as_deref(), Some(title));
                    let title_at = SystemTime::now();
                    let cap = Instant::now() + Duration::from_secs(10);
                    while SystemTime::now()
                        .duration_since(title_at)
                        .unwrap_or_default()
                        < Duration::from_secs(3)
                    {
                        assert!(
                            Instant::now() < cap,
                            "wall clock did not advance past title slack"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let foreign_read = GrokRegistryRead {
                        started: SystemTime::now(),
                        finished: SystemTime::now(),
                    };
                    app.record_grok_read(&foreign, foreign_read);
                    // The target's old A result completes after the other pane.
                    app.record_grok_read(&key, old_read);
                    let previous = app.grok_read_for(&key);
                    let read = GrokRegistryRead {
                        started: SystemTime::now(),
                        finished: SystemTime::now(),
                    };
                    let new = GrokActiveSession {
                        pid: key.child_pid.unwrap(),
                        session_id: Uuid::new_v4().to_string(),
                        opened_at: chrono::DateTime::UNIX_EPOCH,
                    };
                    let interval = grok_switch_interval(&new, previous, read);
                    app.record_grok_read(&key, read);
                    let update = app
                        .stage_grok_session(&key, new.session_id, interval, cx)
                        .unwrap();
                    app.finish_discovered_sessions(vec![update], cx);
                    assert_eq!(
                        session.title().as_deref(),
                        Some(title),
                        "a different pane's newer read cleared the target's current title"
                    );
                    assert_eq!(interval.0, old_read.started);
                    let mut replaced = key.clone();
                    replaced.launch += Duration::from_secs(1);
                    assert!(
                        app.grok_read_for(&replaced).is_none(),
                        "another launch reused the observation"
                    );
                    replaced = key;
                    replaced.child_pid = replaced.child_pid.map(|pid| pid + 1);
                    assert!(
                        app.grok_read_for(&replaced).is_none(),
                        "another process reused the observation"
                    );
                })
                .unwrap();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DiscoveryCandidate, SessionOwnership, claim_validated_claude_record, discover_sessions,
    };
    use crate::session_binding;
    use muxel_core::session_discovery::{SessionDiscovery, SessionDiscoveryKey, SessionProvider};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, SystemTime};
    use uuid::Uuid;

    #[test]
    fn gasq_ownership_index_preserves_duplicate_owners_and_both_pending_claims() {
        let owner = Uuid::new_v4();
        let sibling = Uuid::new_v4();
        let mut ownership = SessionOwnership::default();
        ownership.claim(owner, "saved");
        ownership.claim(owner, "accepted");
        ownership.claim(sibling, "accepted");
        assert!(ownership.bound_elsewhere(owner, "accepted"));
        assert!(ownership.bound_elsewhere(sibling, "accepted"));
        ownership.replace(
            owner,
            [Some("saved"), Some("accepted")],
            [Some("saved"), Some("next")],
        );
        assert!(!ownership.bound_elsewhere(sibling, "accepted"));
        assert!(ownership.bound_elsewhere(owner, "accepted"));
        assert!(ownership.bound_elsewhere(sibling, "saved"));
        assert!(ownership.bound_elsewhere(sibling, "next"));
        ownership.replace(sibling, [Some("accepted"), Some("accepted")], [None, None]);
        assert!(!ownership.bound_elsewhere(owner, "accepted"));
    }

    struct ClaudeFixture {
        home: PathBuf,
        data: PathBuf,
        pane: Uuid,
        request: muxel_core::session_discovery::SessionDiscoveryRequest,
    }

    impl ClaudeFixture {
        fn new() -> Self {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.wip/discovery-tests")
                .join(Uuid::new_v4().to_string());
            let home = root.join("home");
            let data = root.join("data");
            std::fs::create_dir_all(home.join(".claude/projects/fixture")).unwrap();
            let pane = Uuid::new_v4();
            let request = SessionDiscovery::default()
                .begin(
                    SessionDiscoveryKey {
                        pane,
                        provider: SessionProvider::Claude,
                        launch: Instant::now(),
                        child_pid: None,
                        binding_generation: 0,
                        binding: None,
                        cwd: home.join(".claude/projects/fixture"),
                        hint: None,
                    },
                    Instant::now(),
                )
                .unwrap();
            Self {
                home,
                data,
                pane,
                request,
            }
        }

        fn shared(&self) -> PathBuf {
            session_binding::claude_binding_path(&self.data, self.pane)
        }

        fn hook(&self, identity: Uuid, nonce: Uuid) -> Vec<u8> {
            let project = self.home.join(".claude/projects/fixture");
            let transcript = project.join(format!("{identity}.jsonl"));
            std::fs::write(&transcript, "{}\n").unwrap();
            let bytes = json!({
                "hook_event_name":"SessionStart", "source":"resume", "session_id":identity,
                "cwd":project, "transcript_path":transcript,
                "muxel_hook":{"nonce":nonce, "recorded_at_us":123456000_u64}
            })
            .to_string()
            .into_bytes();
            let shared = self.shared();
            std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
            std::fs::write(shared, &bytes).unwrap();
            bytes
        }

        fn candidate(
            &self,
        ) -> (
            session_binding::ClaudeBindingSnapshot,
            session_binding::ClaudeBinding,
        ) {
            let (home, data, request) =
                (self.home.clone(), self.data.clone(), self.request.clone());
            let mut batch = std::thread::spawn(move || {
                discover_sessions(vec![request], &home, Some(&data), Default::default())
            })
            .join()
            .unwrap();
            match batch.results.pop().unwrap().1 {
                DiscoveryCandidate::Claude {
                    snapshot,
                    binding: Some(binding),
                } => (snapshot, binding),
                _ => panic!("expected validated pending Claude snapshot"),
            }
        }

        fn claim(
            &self,
            snapshot: &session_binding::ClaudeBindingSnapshot,
        ) -> Option<session_binding::ClaimedClaudeRecord> {
            claim_validated_claude_record(&self.data, self.pane, snapshot)
        }
    }

    #[test]
    fn claude_worker_snapshot_preserves_nonce_and_leaves_hook_unclaimed() {
        let fixture = ClaudeFixture::new();
        let identity = Uuid::new_v4();
        let nonce = Uuid::new_v4();
        let original = fixture.hook(identity, nonce);
        let (snapshot, binding) = fixture.candidate();
        assert_eq!(snapshot.bytes, original);
        assert!(snapshot.modified.is_some());
        assert_eq!(binding.session_id, identity.to_string());
        let stamp = binding.stamp.unwrap();
        assert_eq!(stamp.nonce, nonce);
        assert_eq!(
            stamp.recorded_at,
            SystemTime::UNIX_EPOCH + Duration::from_micros(123456000)
        );
        assert_eq!(std::fs::read(fixture.shared()).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(fixture.shared().parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn claude_worker_stale_snapshot_cannot_consume_a_newer_hook() {
        let fixture = ClaudeFixture::new();
        fixture.hook(Uuid::new_v4(), Uuid::new_v4());
        let (old_snapshot, _) = fixture.candidate();
        let newer_identity = Uuid::new_v4();
        let newer = fixture.hook(newer_identity, Uuid::new_v4());
        assert!(
            fixture.claim(&old_snapshot).is_none(),
            "stale validation accepted a newer record"
        );
        assert!(!fixture.shared().exists());
        assert_eq!(
            session_binding::read_pending_claude_binding(&fixture.data, fixture.pane)
                .unwrap()
                .bytes,
            newer
        );
        let (retry, binding) = fixture.candidate();
        assert_eq!(binding.session_id, newer_identity.to_string());
        session_binding::acknowledge_claude_binding(fixture.claim(&retry).unwrap());
        assert!(
            session_binding::read_pending_claude_binding(&fixture.data, fixture.pane).is_none()
        );
    }

    #[test]
    fn claude_worker_retries_pending_claim_before_newer_shared_hook() {
        let fixture = ClaudeFixture::new();
        let first = Uuid::new_v4();
        let nonce = Uuid::new_v4();
        fixture.hook(first, nonce);
        let (snapshot, _) = fixture.candidate();
        // A stale result or failed persistence leaves the UI's claim unacked.
        drop(fixture.claim(&snapshot).unwrap());
        let second = Uuid::new_v4();
        let newer = fixture.hook(second, Uuid::new_v4());
        let (retry, binding) = fixture.candidate();
        assert_eq!(binding.session_id, first.to_string());
        assert_eq!(binding.stamp.unwrap().nonce, nonce);
        session_binding::acknowledge_claude_binding(fixture.claim(&retry).unwrap());
        assert_eq!(std::fs::read(fixture.shared()).unwrap(), newer);
        let (latest, binding) = fixture.candidate();
        assert_eq!(binding.session_id, second.to_string());
        session_binding::acknowledge_claude_binding(fixture.claim(&latest).unwrap());
        assert!(
            session_binding::read_pending_claude_binding(&fixture.data, fixture.pane).is_none()
        );
    }
}
