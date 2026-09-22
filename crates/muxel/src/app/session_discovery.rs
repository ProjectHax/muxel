//! Recurring provider reads and process enumeration run on workers. The UI
//! claims matching validated snapshots and acknowledges only after persistence.

#[cfg(windows)]
use super::{
    GrokRegistryRead, active_grok_session_for_process, grok_switch_interval, process_parent_map,
    read_grok_active_sessions,
};
use super::{
    MuxelApp, home_dir, is_claude_program, is_codex_program, is_grok_program,
    session_id_bound_elsewhere, validated_codex_session_id,
};
use crate::{session_binding, ui_profile};
use gpui::{App, Context};
use muxel_core::AgentPreset;
use muxel_core::session_discovery::{
    SessionDiscoveryKey, SessionDiscoveryRequest, SessionProvider,
};
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
            let previous_grok_read = self
                .grok_registry_read
                .map(|read| (read.started, read.finished));
            #[cfg(not(windows))]
            let previous_grok_read = None;
            cx.spawn(async move |this, cx| {
                let (batch, data_dir) = cx
                    .background_executor()
                    .spawn(async move {
                        let batch = discover_sessions(
                            requests,
                            &home,
                            data_dir.as_deref(),
                            previous_grok_read,
                        );
                        (batch, data_dir)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    #[cfg(windows)]
                    if let Some((started, finished)) = batch.grok_read
                        && this
                            .grok_registry_read
                            .is_none_or(|old| finished >= old.finished)
                    {
                        this.grok_registry_read = Some(GrokRegistryRead { started, finished });
                    }
                    #[cfg(not(windows))]
                    let _ = batch.grok_read;
                    let mut updates = Vec::new();
                    for (request, candidate) in batch.results {
                        let iid = request.key.pane;
                        let current = this.session_discovery_key(iid, cx);
                        let unowned = candidate.session_id().is_some_and(|id| {
                            !session_id_bound_elsewhere(&this.workspace.instances, iid, id)
                        });
                        let valid = this.session_discoveries.finish(
                            &request,
                            current.as_ref(),
                            Instant::now(),
                            unowned,
                        );
                        if !valid {
                            // Stale workers never claim or acknowledge a receipt.
                            continue;
                        }
                        let update = match candidate {
                            DiscoveryCandidate::Claude { snapshot, binding } => {
                                // Only this UI owner and prelaunch recovery may claim/ack.
                                // A newer receipt must wait for its own validation.
                                let Some(record) = data_dir.as_deref().and_then(|data_dir| {
                                    claim_validated_claude_record(data_dir, iid, &snapshot)
                                }) else {
                                    continue;
                                };
                                if unowned {
                                    this.stage_discovered_claude(iid, record, binding, cx)
                                } else {
                                    session_binding::acknowledge_claude_binding(record);
                                    None
                                }
                            }
                            DiscoveryCandidate::Session {
                                session_id,
                                switched,
                            } if unowned => this
                                .stage_discovered_session(iid, session_id, false)
                                .map(|mut update| {
                                    update.switched = switched;
                                    update
                                }),
                            _ => None,
                        };
                        if let Some(update) = update {
                            updates.push(update);
                        }
                    }
                    this.finish_discovered_sessions(updates, cx);
                });
            })
            .detach();
        }
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
    ) {
        if updates.is_empty() {
            return;
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
        }
    }
}

fn discover_sessions(
    requests: Vec<SessionDiscoveryRequest>,
    home: &Path,
    data_dir: Option<&Path>,
    previous_grok_read: Option<(SystemTime, SystemTime)>,
) -> DiscoveryBatch {
    #[cfg(not(windows))]
    let _ = previous_grok_read;
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
                        let previous = previous_grok_read
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

#[cfg(test)]
mod tests {
    use super::{DiscoveryCandidate, claim_validated_claude_record, discover_sessions};
    use crate::session_binding;
    use muxel_core::session_discovery::{SessionDiscovery, SessionDiscoveryKey, SessionProvider};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, SystemTime};
    use uuid::Uuid;

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
                discover_sessions(vec![request], &home, Some(&data), None)
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
