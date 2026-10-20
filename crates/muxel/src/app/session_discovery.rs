//! The UI half captures identities and applies guarded results. All provider
//! files and process enumeration are read by the background task below.

use super::{
    MuxelApp, PaneView, home_dir, is_claude_program, is_codex_program, is_grok_program,
    validated_codex_session_id,
};
#[cfg(windows)]
use super::{active_grok_session_for_process, process_parent_map, read_grok_active_sessions};
use crate::{session_binding, ui_profile};
use gpui::{App, Context};
use muxel_core::session_discovery::{
    CodexLookupEvidence, SessionDiscoveryKey, SessionDiscoveryRequest, SessionProvider,
};
use muxel_core::tmux::Socket;
use muxel_core::{AgentPreset, CodexHintMiss};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use uuid::Uuid;

// Explicit Import may search history. Restoration must never call this path.
pub(super) fn capture_codex_session_id(home: &Path, cwd: &Path) -> Option<String> {
    #[cfg(all(test, windows))]
    let injected = CODEX_LAUNCH_TEST_LOOKUP.lock().unwrap().clone();
    #[cfg(all(test, windows))]
    if let Some(discover) = injected {
        return discover(home, cwd);
    }
    muxel_core::codex_latest_session_id(home, cwd)
}

pub(super) fn resume_codex_session(
    home: &Path,
    id: &str,
    path: Option<&Path>,
) -> std::io::Result<muxel_core::CodexResumePath> {
    #[cfg(all(test, windows))]
    let injected = CODEX_RESUME_TEST_LOOKUP.lock().unwrap().clone();
    #[cfg(all(test, windows))]
    if let Some(discover) = injected {
        return discover(home, id, path);
    }
    muxel_core::codex_resume_path(home, id, path)
}

#[cfg(all(test, windows))]
type CodexResumeTestLookup = std::sync::Arc<
    dyn Fn(&Path, &str, Option<&Path>) -> std::io::Result<muxel_core::CodexResumePath>
        + Send
        + Sync,
>;
#[cfg(all(test, windows))]
static CODEX_RESUME_TEST_LOOKUP: std::sync::Mutex<Option<CodexResumeTestLookup>> =
    std::sync::Mutex::new(None);
#[cfg(all(test, windows))]
pub(super) fn inject_codex_resume_lookup(discover: CodexResumeTestLookup) -> CodexLaunchTestGuard {
    *CODEX_RESUME_TEST_LOOKUP.lock().unwrap() = Some(discover);
    CodexLaunchTestGuard
}

#[cfg(all(test, windows))]
type CodexLaunchTestLookup = std::sync::Arc<dyn Fn(&Path, &Path) -> Option<String> + Send + Sync>;
#[cfg(all(test, windows))]
static CODEX_LAUNCH_TEST_LOOKUP: std::sync::Mutex<Option<CodexLaunchTestLookup>> =
    std::sync::Mutex::new(None);

#[cfg(all(test, windows))]
pub(super) struct CodexLaunchTestGuard;
#[cfg(all(test, windows))]
impl Drop for CodexLaunchTestGuard {
    fn drop(&mut self) {
        *CODEX_LAUNCH_TEST_LOOKUP.lock().unwrap() = None;
        *CODEX_RESUME_TEST_LOOKUP.lock().unwrap() = None;
    }
}
#[cfg(all(test, windows))]
pub(super) fn inject_codex_launch_lookup(discover: CodexLaunchTestLookup) -> CodexLaunchTestGuard {
    *CODEX_LAUNCH_TEST_LOOKUP.lock().unwrap() = Some(discover);
    CodexLaunchTestGuard
}

#[cfg(test)]
type CodexTmuxTitleTestReader =
    std::sync::Arc<dyn Fn(&str, Option<Socket>) -> Option<String> + Send + Sync>;
#[cfg(test)]
static CODEX_TMUX_TITLE_TEST_READER: std::sync::Mutex<Option<CodexTmuxTitleTestReader>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
struct CodexTmuxTitleTestGuard;

#[cfg(test)]
impl Drop for CodexTmuxTitleTestGuard {
    fn drop(&mut self) {
        *CODEX_TMUX_TITLE_TEST_READER.lock().unwrap() = None;
    }
}

#[cfg(test)]
fn inject_codex_tmux_title_reader(reader: CodexTmuxTitleTestReader) -> CodexTmuxTitleTestGuard {
    *CODEX_TMUX_TITLE_TEST_READER.lock().unwrap() = Some(reader);
    CodexTmuxTitleTestGuard
}

enum TmuxTitleProbe {
    Found(String),
    Failed,
    TimedOut,
}

/// A title probe has the same two-second deadline as tmux claim cleanup. Read
/// its bounded output concurrently so a long or stalled title cannot fill a
/// pipe and hold the discovery batch.
fn tmux_title_on(session: &str, socket: Socket) -> TmuxTitleProbe {
    let mut command = Command::new("tmux");
    command
        .args(socket.args())
        .args([
            "display-message",
            "-p",
            "-t",
            &format!("={session}:"),
            "#{pane_title}",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return TmuxTitleProbe::Failed;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return TmuxTitleProbe::Failed;
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(4097).read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return TmuxTitleProbe::TimedOut;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return TmuxTitleProbe::Failed;
            }
        }
    };
    if !status.success() {
        return TmuxTitleProbe::Failed;
    }
    match receiver.recv_timeout(Duration::from_millis(250)) {
        Ok(Ok(bytes)) if bytes.len() <= 4096 => {
            TmuxTitleProbe::Found(String::from_utf8_lossy(&bytes).trim().to_string())
        }
        _ => TmuxTitleProbe::Failed,
    }
}

/// This runs only on the discovery worker. The recorded socket prevents a
/// namesake session on the other server from supplying the wrong title.
fn codex_tmux_title(session: &str, socket: Option<Socket>) -> Option<String> {
    #[cfg(test)]
    if let Some(reader) = CODEX_TMUX_TITLE_TEST_READER.lock().unwrap().clone() {
        return reader(session, socket);
    }
    if let Some(socket) = socket {
        return match tmux_title_on(session, socket) {
            TmuxTitleProbe::Found(title) => Some(title),
            _ => None,
        };
    }
    // Legacy panes did not record their socket. Query the private server first
    // and only try the default server when that probe fails. A timeout is
    // inconclusive and must not select a namesake elsewhere.
    match tmux_title_on(session, Socket::Private) {
        TmuxTitleProbe::Found(title) => Some(title),
        TmuxTitleProbe::Failed => match tmux_title_on(session, Socket::Default) {
            TmuxTitleProbe::Found(title) => Some(title),
            _ => None,
        },
        TmuxTitleProbe::TimedOut => None,
    }
}

fn codex_tmux_capture_session(
    instance: &muxel_core::Instance,
    osc_hint: Option<&str>,
) -> Option<String> {
    (osc_hint.is_none()
        && instance.use_tmux
        && instance.session_started
        && instance.session_id.is_none())
    .then(|| instance.tmux_session.clone())
    .flatten()
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
        let (provider, hint, tmux_session) = if let Some(preset) =
            preset.filter(|preset| is_codex_program(preset.program.as_deref()))
        {
            let hint = view
                .session_id_hint()
                .and_then(|title| muxel_core::codex_session_hint_from_title(preset, &title));
            let tmux_session = codex_tmux_capture_session(instance, hint.as_deref());
            if hint.is_none() && tmux_session.is_none() {
                return None;
            }
            if hint.as_ref().is_some_and(|hint| {
                let prefix = hint.strip_suffix("...").unwrap_or(hint);
                instance
                    .session_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with(prefix))
            }) {
                return None;
            }
            (SessionProvider::Codex, hint, tmux_session)
        } else if is_claude_program(instance.program.as_deref()) {
            (SessionProvider::Claude, None, None)
        } else if cfg!(windows)
            && is_grok_program(instance.program.as_deref())
            && instance.tmux_session.is_none()
            && view.child_pid().is_some()
        {
            (SessionProvider::Grok, None, None)
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
            tmux_socket: tmux_session.as_ref().and(instance.tmux_socket),
            tmux_session,
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
                    for (request, candidate, miss, observed_hint) in results {
                        let iid = request.key.pane;
                        let current = this.session_discovery_key(iid, cx);
                        // Earlier results in this batch may already have
                        // acquired this conversation. Check the live workspace.
                        let rejected_owner = candidate.as_ref().and_then(|candidate| {
                            this.workspace
                                .instances
                                .iter()
                                .find(|instance| {
                                    instance.id != iid
                                        && instance.session_id.as_deref()
                                            == Some(candidate.id.as_str())
                                })
                                .map(|instance| (instance.id, candidate.id.clone()))
                        });
                        let unowned = candidate.is_some() && rejected_owner.is_none();
                        let valid = this.session_discoveries.finish_with_codex_evidence(
                            &request,
                            current.as_ref(),
                            Instant::now(),
                            unowned,
                            CodexLookupEvidence {
                                miss,
                                rejected_owner,
                                observed_hint,
                            },
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

type DiscoveryResult = (
    SessionDiscoveryRequest,
    Option<SessionCandidate>,
    Option<CodexHintMiss>,
    Option<String>,
);

fn discover_sessions(
    requests: Vec<SessionDiscoveryRequest>,
    home: &Path,
    data_dir: Option<&Path>,
) -> Vec<DiscoveryResult> {
    #[cfg(windows)]
    let mut grok_snapshot = None;
    requests
        .into_iter()
        .map(|request| {
            let key = &request.key;
            let started = Instant::now();
            let mut miss = None;
            let mut observed_hint = None;
            let candidate = match key.provider {
                SessionProvider::Codex => {
                    let hint = key.hint.clone().or_else(|| {
                        key.tmux_session
                            .as_deref()
                            .and_then(|session| codex_tmux_title(session, key.tmux_socket))
                            .and_then(|title| {
                                muxel_core::codex_session_hint_from_title(
                                    &AgentPreset::codex(),
                                    &title,
                                )
                            })
                    });
                    observed_hint = hint.clone();
                    hint.and_then(|hint| {
                        if let Some((cached_hint, cached_id)) = &request.codex_rejected_hint
                            && *cached_hint == hint
                        {
                            // A sibling still owns this exact title. Read the
                            // title again next time, but do not rescan a rollout
                            // that was already validated for this hint.
                            return Some(SessionCandidate {
                                id: cached_id.clone(),
                                path: None,
                            });
                        }
                        let stamp = CodexHintMiss::capture(home, &hint, started);
                        if let Some(cached) = request
                            .codex_miss
                            .as_ref()
                            .filter(|cached| cached.reusable_for(&stamp))
                        {
                            miss = Some(cached.clone());
                            return None;
                        }
                        let candidate = validated_codex_session_id(
                            &AgentPreset::codex(),
                            key.binding.as_deref(),
                            Some(&hint),
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
                    })
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
            (request, candidate, miss, observed_hint)
        })
        .collect()
}

#[cfg(test)]
mod w8lm_tests {
    use super::{codex_tmux_capture_session, discover_sessions, inject_codex_tmux_title_reader};
    use muxel_core::session_discovery::{
        CodexLookupEvidence, SessionDiscoveries, SessionDiscoveryKey, SessionProvider,
    };
    use muxel_core::tmux::Socket;
    use muxel_core::{AgentPreset, Instance, Project, Workspace};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
            tmux_session: None,
            tmux_socket: None,
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
            let (request, candidate, miss, _) =
                discover_sessions(vec![request], &root, None).pop().unwrap();
            assert!(candidate.is_none());
            assert!(miss.is_some());
            assert!(discovery.finish_with_codex_miss(&request, Some(&key), at, false, miss));
        }
        let path = write(&root, &key, false);
        let request = discovery
            .begin(key.clone(), now + Duration::from_secs(3))
            .unwrap();
        assert!(request.codex_miss.is_some());
        let (_, candidate, miss, _) = discover_sessions(vec![request], &root, None).pop().unwrap();
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
        let (request, candidate, _, _) =
            discover_sessions(vec![request], &root, None).pop().unwrap();
        assert!(candidate.is_none());
        assert_eq!(request.key.binding, old);
    }

    #[test]
    fn tmux_codex_capture_reads_its_recorded_socket_then_stops_when_bound() {
        let root = std::env::temp_dir().join(format!("muxel-tmux-codex-{}", Uuid::new_v4()));
        let mut key = key(&root);
        let id = key.hint.clone().unwrap();
        let path = write(&root, &key, false);
        let mut instance = Instance::from_preset(Uuid::new_v4(), &AgentPreset::codex());
        instance.use_tmux = true;
        instance.session_started = true;
        instance.tmux_session = Some("muxel_test_owned".into());
        instance.tmux_socket = Some(Socket::Default);
        key.hint = None;
        key.tmux_session = codex_tmux_capture_session(&instance, key.hint.as_deref());
        key.tmux_socket = instance.tmux_socket;
        assert_eq!(key.tmux_session.as_deref(), Some("muxel_test_owned"));
        let _reader = inject_codex_tmux_title_reader(Arc::new(move |session, socket| {
            assert_eq!(session, "muxel_test_owned");
            assert_eq!(socket, Some(Socket::Default));
            Some(format!("{id} | Ready"))
        }));
        let now = Instant::now();
        let mut discovery = SessionDiscoveries::default();
        let request = discovery.begin(key.clone(), now).unwrap();
        let (request, candidate, miss, _) =
            discover_sessions(vec![request], &root, None).pop().unwrap();
        let candidate = candidate.expect("tmux title must resolve the rollout");
        assert_eq!(candidate.path, Some(path));
        assert!(miss.is_none());
        assert!(discovery.finish_with_codex_miss(&request, Some(&key), now, true, miss));
        instance.session_id = Some(candidate.id);
        assert!(codex_tmux_capture_session(&instance, None).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sibling_owned_tmux_title_keeps_polling_until_a_new_hint_is_unowned() {
        let root = std::env::temp_dir().join(format!("muxel-tmux-rebind-{}", Uuid::new_v4()));
        let mut key = key(&root);
        let owned_id = key.hint.clone().unwrap();
        let owned_path = write(&root, &key, false);
        let new_id = "01a1543c-9700-7000-8000-000000000002";
        key.hint = Some(new_id.into());
        let new_path = write(&root, &key, false);
        key.hint = None;
        key.tmux_session = Some("muxel_switching".into());
        key.tmux_socket = Some(Socket::Private);

        let project = Project::new("capture", key.cwd.clone());
        let mut owner = Instance::from_preset(project.id, &AgentPreset::codex());
        owner.session_id = Some(owned_id.clone());
        let owner_id = owner.id;
        let mut target = Instance::from_preset(project.id, &AgentPreset::codex());
        target.id = key.pane;
        target.use_tmux = true;
        target.session_started = true;
        target.tmux_session = key.tmux_session.clone();
        target.tmux_socket = key.tmux_socket;
        let workspace = Workspace {
            projects: vec![project],
            instances: vec![owner, target],
            ..Workspace::default()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let owned_title = owned_id.clone();
        let _reader = inject_codex_tmux_title_reader(Arc::new(move |session, socket| {
            assert_eq!(session, "muxel_switching");
            assert_eq!(socket, Some(Socket::Private));
            let id = if observed.fetch_add(1, Ordering::Relaxed) < 2 {
                &owned_title
            } else {
                new_id
            };
            Some(format!("{id} | Ready"))
        }));
        let now = Instant::now();
        let mut discoveries = SessionDiscoveries::default();
        discoveries.reconcile(&workspace);
        for seconds in [0, 1] {
            let at = now + Duration::from_secs(seconds);
            let request = discoveries.begin(key.clone(), at).unwrap();
            let (request, candidate, miss, observed_hint) =
                discover_sessions(vec![request], &root, None).pop().unwrap();
            assert_eq!(candidate.unwrap().id, owned_id);
            assert!(miss.is_none());
            assert!(discoveries.finish_with_codex_evidence(
                &request,
                Some(&key),
                at,
                false,
                CodexLookupEvidence {
                    miss,
                    rejected_owner: Some((owner_id, owned_id.clone())),
                    observed_hint,
                }
            ));
            if seconds == 0 {
                std::fs::remove_file(&owned_path).unwrap();
            }
        }
        let at = now + Duration::from_secs(3);
        let request = discoveries.begin(key.clone(), at).unwrap();
        let (request, candidate, miss, observed_hint) =
            discover_sessions(vec![request], &root, None).pop().unwrap();
        let candidate = candidate.expect("new title should be validated and captured");
        assert_eq!(candidate.id, new_id);
        assert_eq!(candidate.path, Some(new_path));
        assert!(discoveries.finish_with_codex_evidence(
            &request,
            Some(&key),
            at,
            true,
            CodexLookupEvidence {
                miss,
                rejected_owner: None,
                observed_hint,
            }
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        std::fs::remove_dir_all(root).unwrap();
    }
}
