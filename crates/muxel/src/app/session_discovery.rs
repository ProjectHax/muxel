//! The UI half captures identities and applies guarded results. All provider
//! files and process enumeration are read by the background task below.

use super::{
    MuxelApp, PaneView, home_dir, is_claude_program, is_codex_program, is_grok_program,
    validated_codex_session_id,
};
#[cfg(windows)]
use super::{active_grok_session_for_process, process_parent_map, read_grok_active_sessions};
use crate::{session_binding, ui_profile};
use gpui::{App, AsyncApp, Context, WeakEntity};
use muxel_core::session_discovery::{
    CodexLookupEvidence, SessionDiscoveryKey, SessionDiscoveryRequest, SessionProvider,
};
use muxel_core::tmux::{SessionTarget, Socket};
use muxel_core::{AgentPreset, CodexHintMiss, Workspace};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use uuid::Uuid;

// Explicit Import searches all history. Restoration uses the bounded,
// ownership-aware `codex_restore_lookup` instead, on a worker.
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
    std::sync::Arc<dyn Fn(&str, Socket) -> Option<String> + Send + Sync>;
#[cfg(test)]
static CODEX_TMUX_TITLE_TEST_READER: std::sync::Mutex<Option<CodexTmuxTitleTestReader>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static CODEX_TMUX_TITLE_FIXTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
struct CodexTmuxTitleTestGuard {
    _fixture_lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for CodexTmuxTitleTestGuard {
    fn drop(&mut self) {
        *CODEX_TMUX_TITLE_TEST_READER.lock().unwrap() = None;
    }
}

#[cfg(test)]
fn inject_codex_tmux_title_reader(reader: CodexTmuxTitleTestReader) -> CodexTmuxTitleTestGuard {
    let fixture_lock = CODEX_TMUX_TITLE_FIXTURE_LOCK.lock().unwrap();
    *CODEX_TMUX_TITLE_TEST_READER.lock().unwrap() = Some(reader);
    CodexTmuxTitleTestGuard {
        _fixture_lock: fixture_lock,
    }
}

/// Bound the discovery worker across process creation, output and reap. The
/// command worker kills/reaps a normally stalled child at the same deadline;
/// the caller can move on even if an OS operation itself stalls during cleanup.
fn bounded_title_command(command: Command, budget: Duration) -> Option<String> {
    let (status, output) = bounded_command(command, budget)?;
    if !status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output?).trim().to_string())
}

/// Whether `target`'s session runs on this profile's tmux servers, asked with
/// the same bound as a title read. `None` when tmux could not answer in time.
pub(super) fn tmux_session_running(target: &SessionTarget) -> Option<bool> {
    let target = crate::integrations::profile_tmux_target(target);
    let sockets = target
        .socket
        .map_or_else(|| Socket::BOTH.to_vec(), |socket| vec![socket]);
    let mut answered = true;
    for socket in sockets {
        let mut command = Command::new("tmux");
        command
            .args(socket.args())
            .args(muxel_core::tmux::has_session_args(&target.name));
        match bounded_command(command, Duration::from_secs(2)) {
            Some((status, _)) if status.success() => return Some(true),
            Some(_) => {}
            None => answered = false,
        }
    }
    answered.then_some(false)
}

/// `command`'s exit status, and its output (at most 4096 bytes) when it
/// succeeded, or `None` when it could not start or finish within `budget`.
fn bounded_command(
    mut command: Command,
    budget: Duration,
) -> Option<(std::process::ExitStatus, Option<Vec<u8>>)> {
    let deadline = Instant::now() + budget;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let result = (|| {
            let mut child = command.spawn().ok()?;
            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            };
            // A title can exceed the pipe capacity, and a child can keep its
            // pipe open after exit. Neither case may block the command worker.
            let (output_sender, output_receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = stdout.take(4097).read_to_end(&mut bytes).map(|_| bytes);
                let _ = output_sender.send(result);
            });
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                }
            };
            if !status.success() {
                return Some((status, None));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let output = match output_receiver.recv_timeout(remaining) {
                Ok(Ok(bytes)) if bytes.len() <= 4096 => Some(bytes),
                _ => None,
            };
            Some((status, output))
        })();
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .flatten()
}

fn tmux_title_on(session: &str, socket: Socket) -> Option<String> {
    let mut command = Command::new("tmux");
    command.args(socket.args()).args([
        "display-message",
        "-p",
        "-t",
        &format!("={session}:"),
        "#{pane_title}",
    ]);
    bounded_title_command(command, Duration::from_secs(2))
}

/// This runs only on the discovery worker. The recorded socket prevents a
/// namesake session on the other server from supplying the wrong title.
fn codex_tmux_title(session: &str, socket: Socket) -> Option<String> {
    #[cfg(test)]
    if let Some(reader) = CODEX_TMUX_TITLE_TEST_READER.lock().unwrap().clone() {
        return reader(session, socket);
    }
    tmux_title_on(session, socket)
}

fn codex_tmux_capture_session(
    instance: &muxel_core::Instance,
    osc_hint: Option<&str>,
) -> Option<String> {
    (osc_hint.is_none()
        && instance.use_tmux
        && instance.session_started
        && instance.session_id.is_none()
        && instance.tmux_socket.is_some())
    .then(|| instance.tmux_session.clone())
    .flatten()
}

/// How long a launch waits for its pane's pre-launch Codex lookup, and how long
/// that lookup may walk the rollout store. The same bound as a tmux title read.
pub(super) const CODEX_RESTORE_LOOKUP_BUDGET: Duration = Duration::from_secs(2);
const CODEX_RESTORE_LOOKUP_POLL: Duration = Duration::from_millis(20);

/// What a restored, started Codex pane needs from local history before its
/// launch. Captured on the UI thread without I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CodexRestoreQuery {
    pub(super) kind: CodexRestoreKind,
    /// The pane's tmux session. One that still runs keeps its conversation:
    /// the pane reattaches and title capture binds the exact id.
    pub(super) tmux: Option<SessionTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CodexRestoreKind {
    /// A saved id with no saved rollout path: confirm it before `resume <id>`.
    Resume { id: String },
    /// No saved id: the newest rollout for `cwd` that no other pane owns.
    Latest {
        cwd: PathBuf,
        owned: HashSet<String>,
        /// Unbound panes sharing `cwd`, so concurrent lookups can each find one.
        limit: usize,
        /// Those siblings' tmux sessions. A running one is still writing its
        /// own unbound conversation here, which would look like this pane's.
        sibling_sessions: Vec<SessionTarget>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CodexRestoreFound {
    Resume(muxel_core::CodexResumePath),
    Latest(Vec<muxel_core::CodexSession>),
    /// Nothing to apply: a running tmux session, or a walk cut short.
    Skip,
}

fn restore_preset<'a>(
    presets: &'a [AgentPreset],
    instance: &muxel_core::Instance,
) -> Option<&'a AgentPreset> {
    instance
        .preset_id
        .and_then(|id| presets.iter().find(|preset| preset.id == id))
        .or_else(|| presets.iter().find(|preset| preset.name == instance.preset))
}

fn restore_codex_pane<'a>(
    workspace: &'a Workspace,
    presets: &[AgentPreset],
    instance: &'a muxel_core::Instance,
) -> Option<&'a Path> {
    let project = workspace.project(instance.project_id)?;
    let preset = restore_preset(presets, instance)?;
    // A literal pane runs its saved argv, never a resume: no history lookup.
    (!instance.literal_argv
        && project.remote.is_none()
        && instance.session_started
        && is_codex_program(preset.program.as_deref())
        && preset.resume_flag.is_some()
        && preset.session_id_flag.is_none())
    .then(|| {
        instance
            .worktree_path
            .as_deref()
            .unwrap_or(&project.root_path)
    })
}

/// The lookup a pane needs before its launch, if any: only local, started
/// Codex panes that either lack a saved rollout path or lack an id entirely.
/// `running_here` says whether a pane's terminal runs in this app now: an
/// unbound sibling in the same directory that does is writing the newest
/// rollout there, so a pane without an id then gets no lookup at all.
pub(super) fn codex_restore_query(
    workspace: &Workspace,
    presets: &[AgentPreset],
    iid: Uuid,
    running_here: &dyn Fn(Uuid) -> bool,
) -> Option<CodexRestoreQuery> {
    let instance = workspace.instance(iid)?;
    let cwd = restore_codex_pane(workspace, presets, instance)?;
    let kind = match &instance.session_id {
        Some(id) if instance.session_path.is_none() => CodexRestoreKind::Resume { id: id.clone() },
        Some(_) => return None,
        None => {
            let siblings: Vec<_> = workspace
                .instances
                .iter()
                .filter(|other| {
                    other.id != iid
                        && other.session_id.is_none()
                        && restore_codex_pane(workspace, presets, other) == Some(cwd)
                })
                .collect();
            if siblings.iter().any(|other| running_here(other.id)) {
                return None;
            }
            CodexRestoreKind::Latest {
                cwd: cwd.to_path_buf(),
                owned: workspace
                    .instances
                    .iter()
                    .filter(|other| other.id != iid)
                    .filter_map(|other| other.session_id.clone())
                    .collect(),
                limit: siblings.len() + 1,
                sibling_sessions: siblings
                    .iter()
                    .filter_map(|other| {
                        other
                            .tmux_session
                            .as_ref()
                            .map(|name| SessionTarget::new(name.clone(), other.tmux_socket.clone()))
                    })
                    .collect(),
            }
        }
    };
    Some(CodexRestoreQuery {
        kind,
        tmux: instance
            .tmux_session
            .as_ref()
            .map(|name| SessionTarget::new(name.clone(), instance.tmux_socket.clone())),
    })
}

/// Runs only on a worker: a dated miss and a cwd recovery both walk the store,
/// bounded by `deadline`.
pub(super) fn codex_restore_lookup(
    query: &CodexRestoreQuery,
    home: &Path,
    deadline: Instant,
    tmux_alive: &dyn Fn(&SessionTarget) -> bool,
) -> CodexRestoreFound {
    let running = || query.tmux.as_ref().is_some_and(tmux_alive);
    match &query.kind {
        CodexRestoreKind::Resume { id } => {
            use muxel_core::CodexResumePath;
            let found = match muxel_core::codex_resume_path(home, id, None) {
                Ok(CodexResumePath::Unknown) => {
                    muxel_core::codex_rollout_by_walk(home, id, deadline)
                }
                Ok(found) => found,
                Err(_) => CodexResumePath::Unknown,
            };
            // A session still running its conversation is not abandoned
            // because the store lacks the rollout.
            if found == CodexResumePath::Missing && running() {
                return CodexRestoreFound::Skip;
            }
            CodexRestoreFound::Resume(found)
        }
        CodexRestoreKind::Latest {
            cwd,
            owned,
            limit,
            sibling_sessions,
        } => {
            if running() || sibling_sessions.iter().any(tmux_alive) {
                return CodexRestoreFound::Skip;
            }
            muxel_core::codex_latest_sessions(home, cwd, owned, *limit, deadline)
                .map_or(CodexRestoreFound::Skip, CodexRestoreFound::Latest)
        }
    }
}

fn run_codex_restore_lookup(
    query: &CodexRestoreQuery,
    home: &Path,
    deadline: Instant,
) -> CodexRestoreFound {
    #[cfg(all(test, windows))]
    let injected = CODEX_RESTORE_TEST_LOOKUP.lock().unwrap().clone();
    #[cfg(all(test, windows))]
    if let Some(lookup) = injected {
        return lookup(query);
    }
    // A session tmux cannot answer for in time is treated as running.
    codex_restore_lookup(query, home, deadline, &|target| {
        tmux_session_running(target) != Some(false)
    })
}

#[cfg(all(test, windows))]
type CodexRestoreTestLookup =
    std::sync::Arc<dyn Fn(&CodexRestoreQuery) -> CodexRestoreFound + Send + Sync>;
#[cfg(all(test, windows))]
static CODEX_RESTORE_TEST_LOOKUP: std::sync::Mutex<Option<CodexRestoreTestLookup>> =
    std::sync::Mutex::new(None);
#[cfg(all(test, windows))]
pub(super) struct CodexRestoreTestGuard;
#[cfg(all(test, windows))]
impl Drop for CodexRestoreTestGuard {
    fn drop(&mut self) {
        *CODEX_RESTORE_TEST_LOOKUP.lock().unwrap() = None;
    }
}
#[cfg(all(test, windows))]
pub(super) fn inject_codex_restore_lookup(lookup: CodexRestoreTestLookup) -> CodexRestoreTestGuard {
    *CODEX_RESTORE_TEST_LOOKUP.lock().unwrap() = Some(lookup);
    CodexRestoreTestGuard
}

/// Apply a landed lookup while the pane still matches its query. A recovered
/// id must still be unowned in the live workspace. Returns whether the pane's
/// binding changed.
pub(super) fn apply_codex_restore_found(
    workspace: &mut Workspace,
    iid: Uuid,
    query: &CodexRestoreQuery,
    found: CodexRestoreFound,
) -> bool {
    use muxel_core::CodexResumePath;
    let owner_elsewhere = |workspace: &Workspace, id: &str| {
        workspace
            .instances
            .iter()
            .any(|other| other.id != iid && other.session_id.as_deref() == Some(id))
    };
    match (&query.kind, found) {
        (CodexRestoreKind::Resume { id }, CodexRestoreFound::Resume(found)) => {
            let Some(instance) = workspace.instance_mut(iid) else {
                return false;
            };
            if !instance.session_started
                || instance.session_id.as_deref() != Some(id.as_str())
                || instance.session_path.is_some()
            {
                return false;
            }
            match found {
                CodexResumePath::Found(path) => instance.session_path = Some(path),
                // The whole store was read: start fresh as for a lost saved path.
                CodexResumePath::Missing => {
                    instance.session_id = None;
                    instance.session_started = false;
                }
                CodexResumePath::Unknown => return false,
            }
            true
        }
        (CodexRestoreKind::Latest { .. }, CodexRestoreFound::Latest(sessions)) => {
            let Some(session) = sessions
                .into_iter()
                .find(|session| !owner_elsewhere(workspace, &session.id))
            else {
                return false;
            };
            let Some(instance) = workspace.instance_mut(iid) else {
                return false;
            };
            if !instance.session_started || instance.session_id.is_some() {
                return false;
            }
            instance.session_id = Some(session.id);
            instance.session_path = Some(session.path);
            true
        }
        _ => false,
    }
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

enum DiscoveryCandidate {
    Validated(SessionCandidate),
    CachedSiblingRejection(String),
    None,
}

impl MuxelApp {
    /// Bump on each binding mutation, including launch-time changes and rollback.
    /// A rebind away and back must still invalidate an older worker.
    pub(super) fn bump_session_binding_generation(&mut self, iid: Uuid) {
        self.session_discoveries.binding_changed(iid);
    }

    /// Start a restored Codex pane's one pre-launch history lookup, if it needs
    /// one, on a worker. Call it only before the pane's launch is prepared. A
    /// deferred launch then waits for it at most [`CODEX_RESTORE_LOOKUP_BUDGET`]
    /// (see `await_codex_restore_lookup`); a result that lands after the launch
    /// was prepared is dropped.
    pub(super) fn begin_codex_restore_lookup(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        // A running pane already has its conversation; only title capture may
        // rebind it.
        if self.discovery_terminal(iid).is_some() || self.failed_launches.contains_key(&iid) {
            return;
        }
        let app: &App = cx;
        let running_here = |other: Uuid| {
            self.discovery_terminal(other)
                .is_some_and(|view| !view.read(app).exited())
        };
        let Some(query) = codex_restore_query(&self.workspace, &self.presets, iid, &running_here)
        else {
            return;
        };
        let Some(home) = home_dir() else { return };
        let now = Instant::now();
        let generation = self.session_discoveries.binding_generation(iid);
        if !self.codex_restore_lookups.begin(iid, generation, now) {
            return;
        }
        let deadline = now + CODEX_RESTORE_LOOKUP_BUDGET;
        cx.spawn(async move |this, cx| {
            let worker_query = query.clone();
            let found = cx
                .background_executor()
                .spawn(async move { run_codex_restore_lookup(&worker_query, &home, deadline) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let generation = this.session_discoveries.binding_generation(iid);
                if this.codex_restore_lookups.finish(iid, generation)
                    && apply_codex_restore_found(&mut this.workspace, iid, &query, found)
                {
                    this.bump_session_binding_generation(iid);
                    this.persist();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Let a pane's pending restore lookup land before its launch is prepared,
    /// polling so the UI thread is never held. Bounded by the lookup budget.
    pub(super) async fn await_codex_restore_lookup(
        this: &WeakEntity<Self>,
        iid: Uuid,
        cx: &mut AsyncApp,
    ) {
        while this
            .read_with(cx, |this, _| {
                this.codex_restore_lookups.holds_launch(
                    iid,
                    Instant::now(),
                    CODEX_RESTORE_LOOKUP_BUDGET,
                )
            })
            .unwrap_or(false)
        {
            cx.background_executor()
                .timer(CODEX_RESTORE_LOOKUP_POLL)
                .await;
        }
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
                .working_directory()
                .unwrap_or(&project.root_path)
                .to_path_buf(),
            hint,
            tmux_socket: tmux_session.as_ref().and(instance.tmux_socket.clone()),
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
                    for (request, discovered, miss, observed_hint) in results {
                        let iid = request.key.pane;
                        let current = this.session_discovery_key(iid, cx);
                        let (candidate, cached_rejection) = match discovered {
                            DiscoveryCandidate::Validated(candidate) => (Some(candidate), None),
                            DiscoveryCandidate::CachedSiblingRejection(id) => (None, Some(id)),
                            DiscoveryCandidate::None => (None, None),
                        };
                        // Earlier results in this batch may already have
                        // acquired this conversation. Check the live workspace.
                        let candidate_id = candidate
                            .as_ref()
                            .map(|candidate| candidate.id.as_str())
                            .or(cached_rejection.as_deref());
                        let rejected_owner = candidate_id.and_then(|id| {
                            this.workspace
                                .instances
                                .iter()
                                .find(|instance| {
                                    instance.id != iid && instance.session_id.as_deref() == Some(id)
                                })
                                .map(|instance| (instance.id, id.to_string()))
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
                                cached_rejection: cached_rejection.is_some(),
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
    DiscoveryCandidate,
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
            let mut cached_rejection = None;
            let candidate = match key.provider {
                SessionProvider::Codex => {
                    let hint = key.hint.clone().or_else(|| {
                        key.tmux_session
                            .as_deref()
                            .zip(key.tmux_socket.clone())
                            .and_then(|(session, socket)| codex_tmux_title(session, socket))
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
                            cached_rejection = Some(cached_id.clone());
                            return None;
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
            let discovered = if let Some(candidate) = candidate {
                DiscoveryCandidate::Validated(candidate)
            } else if let Some(id) = cached_rejection {
                DiscoveryCandidate::CachedSiblingRejection(id)
            } else {
                DiscoveryCandidate::None
            };
            ui_profile::session_lookup_finished(
                ui_profile::SessionLookupKind::Discovery,
                Some(key.provider),
                key.pane,
                started.elapsed(),
                if matches!(&discovered, DiscoveryCandidate::Validated(_)) {
                    ui_profile::SessionLookupOutcome::Found
                } else {
                    ui_profile::SessionLookupOutcome::Unresolved
                },
            );
            (request, discovered, miss, observed_hint)
        })
        .collect()
}

#[cfg(test)]
mod w8lm_tests {
    use super::{
        DiscoveryCandidate, SessionCandidate, bounded_command, bounded_title_command,
        codex_tmux_capture_session, discover_sessions, inject_codex_tmux_title_reader,
    };
    use muxel_core::session_discovery::{
        CodexLookupEvidence, SessionDiscoveries, SessionDiscoveryKey, SessionProvider,
    };
    use muxel_core::tmux::Socket;
    use muxel_core::{AgentPreset, Instance, Project, Workspace};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    fn validated(discovered: DiscoveryCandidate) -> Option<SessionCandidate> {
        match discovered {
            DiscoveryCandidate::Validated(candidate) => Some(candidate),
            DiscoveryCandidate::None => None,
            DiscoveryCandidate::CachedSiblingRejection(id) => {
                panic!("cached sibling rejection is not a validated candidate: {id}")
            }
        }
    }

    #[test]
    #[ignore = "invoked as the child of bounded_title_command_bounds_a_stalled_process"]
    fn sleeping_title_child() {
        if let Some(path) = std::env::var_os("MUXEL_TEST_SLEEPY_TITLE_READY") {
            std::fs::write(path, "ready").unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn bounded_title_command_bounds_a_stalled_process() {
        // Relaunching the test binary can take seconds on a loaded runner, and
        // that startup is not what is bounded. Time the bound only once the
        // child is running; a child still starting at the deadline gets a
        // longer budget instead of failing the test.
        for budget in [2, 4, 8, 16].map(Duration::from_secs) {
            let ready = std::env::temp_dir().join(format!("muxel-title-ready-{}", Uuid::new_v4()));
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--ignored",
                    "--exact",
                    "app::session_discovery::w8lm_tests::sleeping_title_child",
                ])
                .env("MUXEL_TEST_SLEEPY_TITLE_READY", &ready);
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(bounded_title_command(command, budget));
            });
            let running = loop {
                if ready.exists() {
                    break Some(Instant::now());
                }
                match receiver.recv_timeout(Duration::from_millis(10)) {
                    Ok(title) => {
                        assert!(title.is_none());
                        break None;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(error) => panic!("bounded call vanished: {error}"),
                }
            };
            let Some(running) = running else {
                let _ = std::fs::remove_file(&ready);
                continue;
            };
            let title = receiver
                .recv_timeout(budget + Duration::from_secs(10))
                .expect("a stalled child held its caller past the bound");
            assert!(title.is_none());
            assert!(
                running.elapsed() < budget + Duration::from_secs(2),
                "returned {:?} after the child was running, bound {budget:?}",
                running.elapsed()
            );
            std::fs::remove_file(ready).unwrap();
            return;
        }
        panic!("the sleeping child never started inside a bound");
    }

    #[test]
    fn bounded_command_reports_how_a_finished_command_exited() {
        // A bounded `has-session` reads only the status: success means the
        // session runs, a failure that it does not.
        let run = |arg: &str| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", arg]);
            bounded_command(command, Duration::from_secs(60))
        };
        let (status, output) = run("app::session_discovery::no_such_test").unwrap();
        assert!(status.success());
        assert!(output.is_some());
        let (status, output) = run("--no-such-flag").unwrap();
        assert!(!status.success());
        assert_eq!(output, None);
        let missing = Command::new("muxel-test-no-such-program");
        assert!(bounded_command(missing, Duration::from_secs(5)).is_none());
    }

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
            assert!(validated(candidate).is_none());
            assert!(miss.is_some());
            assert!(discovery.finish_with_codex_miss(&request, Some(&key), at, false, miss));
        }
        let path = write(&root, &key, false);
        let request = discovery
            .begin(key.clone(), now + Duration::from_secs(3))
            .unwrap();
        assert!(request.codex_miss.is_some());
        let (_, candidate, miss, _) = discover_sessions(vec![request], &root, None).pop().unwrap();
        let candidate = validated(candidate).expect("new day directory must invalidate the miss");
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
        assert!(validated(candidate).is_none());
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
        key.tmux_socket = instance.tmux_socket.clone();
        assert_eq!(key.tmux_session.as_deref(), Some("muxel_test_owned"));
        let _reader = inject_codex_tmux_title_reader(Arc::new(move |session, socket| {
            assert_eq!(session, "muxel_test_owned");
            assert_eq!(socket, Socket::Default);
            Some(format!("{id} | Ready"))
        }));
        let now = Instant::now();
        let mut discovery = SessionDiscoveries::default();
        let request = discovery.begin(key.clone(), now).unwrap();
        let (request, candidate, miss, _) =
            discover_sessions(vec![request], &root, None).pop().unwrap();
        let candidate = validated(candidate).expect("tmux title must resolve the rollout");
        assert_eq!(candidate.path, Some(path));
        assert!(miss.is_none());
        assert!(discovery.finish_with_codex_miss(&request, Some(&key), now, true, miss));
        instance.session_id = Some(candidate.id);
        assert!(codex_tmux_capture_session(&instance, None).is_none());
        instance.session_id = None;
        instance.tmux_socket = None;
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
        target.tmux_socket = key.tmux_socket.clone();
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
            assert_eq!(socket, Socket::Private);
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
            let cached_rejection = seconds == 1;
            match candidate {
                DiscoveryCandidate::Validated(candidate) if !cached_rejection => {
                    assert_eq!(candidate.id, owned_id)
                }
                DiscoveryCandidate::CachedSiblingRejection(id) if cached_rejection => {
                    assert_eq!(id, owned_id)
                }
                _ => panic!("unexpected candidate state for sibling-owned title"),
            }
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
                    cached_rejection,
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
        let candidate = validated(candidate).expect("new title should be validated and captured");
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
                cached_rejection: false,
            }
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cached_owned_title_cannot_bind_after_its_owner_releases_mid_lookup() {
        let root = std::env::temp_dir().join(format!("muxel-tmux-release-{}", Uuid::new_v4()));
        let mut key = key(&root);
        let owned_id = key.hint.clone().unwrap();
        let owned_path = write(&root, &key, false);
        key.hint = None;
        key.tmux_session = Some("muxel_released".into());
        key.tmux_socket = Some(Socket::Private);

        let project = Project::new("capture", key.cwd.clone());
        let mut owner = Instance::from_preset(project.id, &AgentPreset::codex());
        owner.session_id = Some(owned_id.clone());
        let owner_id = owner.id;
        let mut target = Instance::from_preset(project.id, &AgentPreset::codex());
        target.id = key.pane;
        let mut workspace = Workspace {
            projects: vec![project],
            instances: vec![owner, target],
            ..Workspace::default()
        };
        let title_id = owned_id.clone();
        let _reader = inject_codex_tmux_title_reader(Arc::new(move |session, socket| {
            assert_eq!((session, socket), ("muxel_released", Socket::Private));
            Some(format!("{title_id} | Ready"))
        }));
        let now = Instant::now();
        let mut discoveries = SessionDiscoveries::default();
        discoveries.reconcile(&workspace);

        let first = discoveries.begin(key.clone(), now).unwrap();
        let (first, discovered, miss, observed_hint) =
            discover_sessions(vec![first], &root, None).pop().unwrap();
        assert_eq!(
            validated(discovered).unwrap().path,
            Some(owned_path.clone())
        );
        assert!(discoveries.finish_with_codex_evidence(
            &first,
            Some(&key),
            now,
            false,
            CodexLookupEvidence {
                miss,
                rejected_owner: Some((owner_id, owned_id.clone())),
                observed_hint,
                cached_rejection: false,
            }
        ));

        std::fs::remove_file(&owned_path).unwrap();
        let at = now + Duration::from_secs(1);
        let cached = discoveries.begin(key.clone(), at).unwrap();
        let (cached, discovered, miss, observed_hint) =
            discover_sessions(vec![cached], &root, None).pop().unwrap();
        assert!(matches!(
            discovered,
            DiscoveryCandidate::CachedSiblingRejection(ref id) if id == &owned_id
        ));
        // Ownership changes after the worker reused A, before completion.
        workspace.instances[0].session_id = None;
        discoveries.reconcile(&workspace);
        assert!(discoveries.finish_with_codex_evidence(
            &cached,
            Some(&key),
            at,
            false,
            CodexLookupEvidence {
                miss,
                rejected_owner: None,
                observed_hint,
                cached_rejection: true,
            }
        ));
        assert!(workspace.instances[1].session_id.is_none());
        let retry = discoveries
            .begin(key.clone(), at + Duration::from_millis(1))
            .expect("released cached owner must trigger immediate revalidation");
        assert!(retry.codex_rejected_hint.is_none());
        let (_, discovered, _, _) = discover_sessions(vec![retry], &root, None).pop().unwrap();
        assert!(validated(discovered).is_none(), "deleted A was rebound");
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod codex_restore_tests {
    use super::{
        CodexRestoreFound, CodexRestoreKind, CodexRestoreQuery, apply_codex_restore_found,
        codex_restore_lookup, codex_restore_query,
    };
    use muxel_core::tmux::{SessionTarget, Socket};
    use muxel_core::{
        AgentPreset, CodexResumePath, CodexSession, Instance, Project, RemoteRef, Workspace,
    };
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    struct TempHome(PathBuf);

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_home(label: &str) -> TempHome {
        let root = std::env::temp_dir().join(format!("muxel-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        TempHome(root)
    }

    fn rollout(home: &Path, day: &str, id: &str, cwd: &Path) -> PathBuf {
        let path = home
            .join(".codex/sessions")
            .join(day)
            .join(format!("rollout-{id}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}}).to_string(),
        )
        .unwrap();
        path
    }

    fn started(project: Uuid, preset: &AgentPreset, id: Option<&str>) -> Instance {
        let mut instance = Instance::from_preset(project, preset);
        instance.session_started = true;
        instance.session_id = id.map(str::to_string);
        instance
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn restore_query_never_looks_up_history_for_a_literal_pane() {
        let codex = AgentPreset::codex();
        let local = Project::new("webapp", "/home/me/webapp");
        // Stale peer metadata can make a literal pane look like a started Codex.
        let mut literal = started(local.id, &codex, None);
        literal.literal_argv = true;
        literal.tmux_session = Some("muxel_webapp_1a2b3c4d".into());
        literal.tmux_socket = Some(Socket::Private);
        let mut resumed = literal.clone();
        resumed.id = Uuid::new_v4();
        resumed.session_id = Some("01a1543c-9700-7000-8000-000000000001".into());
        let agent = started(local.id, &codex, None);
        let ids = [literal.id, resumed.id, agent.id];
        let workspace = Workspace {
            projects: vec![local],
            instances: vec![literal, resumed, agent],
            ..Workspace::default()
        };
        let presets = [codex];
        assert_eq!(
            codex_restore_query(&workspace, &presets, ids[0], &|_| false),
            None
        );
        assert_eq!(
            codex_restore_query(&workspace, &presets, ids[1], &|_| false),
            None
        );
        // Nor is it a sibling whose running session would block another pane.
        assert!(matches!(
            codex_restore_query(&workspace, &presets, ids[2], &|iid| iid != ids[2])
                .map(|query| query.kind),
            Some(CodexRestoreKind::Latest { limit: 1, sibling_sessions, .. })
                if sibling_sessions.is_empty()
        ));
    }

    #[test]
    fn restore_query_covers_only_started_local_codex_panes_missing_an_id_or_a_path() {
        let codex = AgentPreset::codex();
        let claude = AgentPreset::claude();
        let local = Project::new("webapp", "/home/me/webapp");
        let mut remote = Project::new("remote", "/srv/app");
        remote.remote = Some(RemoteRef {
            host_id: Uuid::new_v4(),
            remote_root: "/srv/app".into(),
        });
        let mut unbound = started(local.id, &codex, None);
        unbound.tmux_session = Some("muxel_webapp_1a2b3c4d".into());
        unbound.tmux_socket = Some(Socket::Private);
        let sibling = started(local.id, &codex, None);
        let unconfirmed = started(
            local.id,
            &codex,
            Some("01a1543c-9700-7000-8000-000000000001"),
        );
        let mut confirmed = started(
            local.id,
            &codex,
            Some("01a1543c-9700-7000-8000-000000000002"),
        );
        confirmed.session_path = Some("/home/me/.codex/sessions/rollout.jsonl".into());
        let mut fresh = Instance::from_preset(local.id, &codex);
        fresh.session_started = false;
        let claude_pane = started(local.id, &claude, None);
        let remote_pane = started(remote.id, &codex, None);
        let mut worktree = started(local.id, &codex, None);
        worktree.worktree_path = Some("/home/me/webapp-wt".into());
        let ids = [
            unbound.id,
            sibling.id,
            unconfirmed.id,
            confirmed.id,
            fresh.id,
            claude_pane.id,
            remote_pane.id,
            worktree.id,
        ];
        let workspace = Workspace {
            projects: vec![local, remote],
            instances: vec![
                unbound,
                sibling,
                unconfirmed,
                confirmed,
                fresh,
                claude_pane,
                remote_pane,
                worktree,
            ],
            ..Workspace::default()
        };
        let presets = [codex, claude];
        let query = |iid| codex_restore_query(&workspace, &presets, iid, &|_| false);
        let owned: HashSet<String> = [
            "01a1543c-9700-7000-8000-000000000001".to_string(),
            "01a1543c-9700-7000-8000-000000000002".to_string(),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            query(ids[0]),
            Some(CodexRestoreQuery {
                kind: CodexRestoreKind::Latest {
                    cwd: "/home/me/webapp".into(),
                    owned: owned.clone(),
                    limit: 2,
                    sibling_sessions: Vec::new(),
                },
                tmux: Some(SessionTarget::new(
                    "muxel_webapp_1a2b3c4d",
                    Some(Socket::Private)
                )),
            })
        );
        // The sibling's lookup checks the first pane's tmux session too.
        assert!(matches!(
            query(ids[1]).map(|query| query.kind),
            Some(CodexRestoreKind::Latest { limit: 2, sibling_sessions, .. })
                if sibling_sessions
                    == [SessionTarget::new("muxel_webapp_1a2b3c4d", Some(Socket::Private))]
        ));
        // An unbound sibling running in this app now is writing the newest
        // rollout here: the pane without an id gets no lookup.
        assert_eq!(
            codex_restore_query(&workspace, &presets, ids[0], &|iid| iid == ids[1]),
            None
        );
        assert!(
            codex_restore_query(&workspace, &presets, ids[2], &|iid| iid == ids[1]).is_some(),
            "an exact id is not a sibling's to take"
        );
        assert_eq!(
            query(ids[2]).map(|query| query.kind),
            Some(CodexRestoreKind::Resume {
                id: "01a1543c-9700-7000-8000-000000000001".into()
            })
        );
        for skipped in &ids[3..7] {
            assert_eq!(query(*skipped), None);
        }
        assert_eq!(
            query(ids[7]).map(|query| query.kind),
            Some(CodexRestoreKind::Latest {
                cwd: "/home/me/webapp-wt".into(),
                owned,
                limit: 1,
                sibling_sessions: Vec::new(),
            })
        );
    }

    #[test]
    fn dated_miss_is_walked_and_a_confirmed_missing_rollout_starts_fresh() {
        let home = temp_home("codex-restore-walk");
        let cwd = home.0.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let moved = "01a1543c-9700-7000-8000-000000000001";
        let gone = "01a1543c-9700-7000-8000-000000000002";
        // Not in the UUIDv7 timestamp's day directory.
        let path = rollout(&home.0, "2020/01/01", moved, &cwd);
        let preset = AgentPreset::codex();
        let project = Project::new("webapp", &cwd);
        let pane = started(project.id, &preset, Some(moved));
        let missing = started(project.id, &preset, Some(gone));
        let (pane_id, missing_id) = (pane.id, missing.id);
        let mut workspace = Workspace {
            projects: vec![project],
            instances: vec![pane, missing],
            ..Workspace::default()
        };
        let presets = [preset.clone()];
        let not_running = |_: &SessionTarget| panic!("no tmux session to check");

        let query = codex_restore_query(&workspace, &presets, pane_id, &|_| false).unwrap();
        let found = codex_restore_lookup(&query, &home.0, later(), &not_running);
        assert_eq!(
            found,
            CodexRestoreFound::Resume(CodexResumePath::Found(path.clone()))
        );
        assert!(apply_codex_restore_found(
            &mut workspace,
            pane_id,
            &query,
            found
        ));
        assert_eq!(
            workspace.instance(pane_id).unwrap().session_path,
            Some(path)
        );
        assert_eq!(
            muxel_core::session_resume_args(&preset, workspace.instance(pane_id).unwrap()),
            Some(vec!["resume".to_string(), moved.to_string()])
        );

        // Master's walk answer: the rollout is gone, so the pane starts fresh
        // instead of a `codex resume` that exits and closes it.
        let query = codex_restore_query(&workspace, &presets, missing_id, &|_| false).unwrap();
        let found = codex_restore_lookup(&query, &home.0, later(), &not_running);
        assert_eq!(found, CodexRestoreFound::Resume(CodexResumePath::Missing));
        assert!(apply_codex_restore_found(
            &mut workspace,
            missing_id,
            &query,
            found
        ));
        let instance = workspace.instance(missing_id).unwrap();
        assert_eq!(instance.session_id, None);
        assert!(!instance.session_started);
        assert_eq!(muxel_core::session_resume_args(&preset, instance), None);

        // A walk cut short keeps the id for `resume <id>`.
        let query = CodexRestoreQuery {
            kind: CodexRestoreKind::Resume { id: gone.into() },
            tmux: None,
        };
        assert_eq!(
            codex_restore_lookup(&query, &home.0, Instant::now(), &not_running),
            CodexRestoreFound::Resume(CodexResumePath::Unknown)
        );
    }

    #[test]
    fn running_tmux_session_keeps_its_conversation_for_title_capture() {
        let home = temp_home("codex-restore-tmux");
        let cwd = home.0.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        rollout(
            &home.0,
            "2026/10/19",
            "01a1543c-9700-7000-8000-000000000001",
            &cwd,
        );
        let target = SessionTarget::new("muxel_webapp_1a2b3c4d", Some(Socket::Private));
        let checked = Cell::new(0);
        let alive = |session: &SessionTarget| {
            assert_eq!(session, &target);
            checked.set(checked.get() + 1);
            true
        };
        let latest = CodexRestoreQuery {
            kind: CodexRestoreKind::Latest {
                cwd: cwd.clone(),
                owned: HashSet::new(),
                limit: 1,
                sibling_sessions: Vec::new(),
            },
            tmux: Some(target.clone()),
        };
        assert_eq!(
            codex_restore_lookup(&latest, &home.0, later(), &alive),
            CodexRestoreFound::Skip
        );
        // A confirmed-missing rollout does not end a session that still runs.
        let resume = CodexRestoreQuery {
            kind: CodexRestoreKind::Resume {
                id: "01a1543c-9700-7000-8000-000000000009".into(),
            },
            tmux: Some(target.clone()),
        };
        assert_eq!(
            codex_restore_lookup(&resume, &home.0, later(), &alive),
            CodexRestoreFound::Skip
        );
        assert_eq!(checked.get(), 2);
        let gone = |_: &SessionTarget| false;
        assert!(matches!(
            codex_restore_lookup(&latest, &home.0, later(), &gone),
            CodexRestoreFound::Latest(sessions) if sessions.len() == 1
        ));
        // The same holds for an unbound sibling's running session: its rollout
        // is the newest unowned one here, and is still its conversation.
        let sibling_running = CodexRestoreQuery {
            kind: CodexRestoreKind::Latest {
                cwd: cwd.clone(),
                owned: HashSet::new(),
                limit: 2,
                sibling_sessions: vec![target.clone()],
            },
            tmux: None,
        };
        assert_eq!(
            codex_restore_lookup(&sibling_running, &home.0, later(), &alive),
            CodexRestoreFound::Skip
        );
        assert_eq!(checked.get(), 3);
        assert!(matches!(
            codex_restore_lookup(&sibling_running, &home.0, later(), &gone),
            CodexRestoreFound::Latest(sessions) if sessions.len() == 1
        ));
    }

    #[test]
    fn unbound_started_pane_recovers_the_newest_rollout_no_sibling_owns() {
        let home = temp_home("codex-restore-latest");
        let cwd = home.0.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let preset = AgentPreset::codex();
        let project = Project::new("webapp", &cwd);
        let owner_id = "01a1543c-9700-7000-8000-000000000001";
        let owned_path = rollout(&home.0, "2026/10/19", owner_id, &cwd);
        let newest = "01a1543c-9700-7000-8000-000000000002";
        let newest_path = rollout(&home.0, "2026/10/19", newest, &cwd);
        let older = "01a1543c-9700-7000-8000-000000000003";
        let older_path = rollout(&home.0, "2026/10/18", older, &cwd);
        let age = |path: &Path, secs: u64| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(secs))
                .unwrap();
        };
        age(&owned_path, 1);
        age(&newest_path, 10);
        age(&older_path, 20);
        let owner = started(project.id, &preset, Some(owner_id));
        let first = started(project.id, &preset, None);
        let second = started(project.id, &preset, None);
        let (first_id, second_id) = (first.id, second.id);
        let mut workspace = Workspace {
            projects: vec![project],
            instances: vec![owner, first, second],
            ..Workspace::default()
        };
        let presets = [preset.clone()];
        let no_tmux = |_: &SessionTarget| panic!("no tmux session to check");
        // Both unbound panes look up together, before either binds.
        let first_query = codex_restore_query(&workspace, &presets, first_id, &|_| false).unwrap();
        let second_query =
            codex_restore_query(&workspace, &presets, second_id, &|_| false).unwrap();
        let first_found = codex_restore_lookup(&first_query, &home.0, later(), &no_tmux);
        let second_found = codex_restore_lookup(&second_query, &home.0, later(), &no_tmux);
        let expected = vec![
            CodexSession {
                id: newest.into(),
                path: newest_path.clone(),
            },
            CodexSession {
                id: older.into(),
                path: older_path.clone(),
            },
        ];
        assert_eq!(first_found, CodexRestoreFound::Latest(expected.clone()));
        assert_eq!(second_found, CodexRestoreFound::Latest(expected));

        assert!(apply_codex_restore_found(
            &mut workspace,
            first_id,
            &first_query,
            first_found
        ));
        // The first pane now owns the newest; the second takes the next one.
        assert!(apply_codex_restore_found(
            &mut workspace,
            second_id,
            &second_query,
            second_found
        ));
        let bound = |iid| {
            let instance = workspace.instance(iid).unwrap();
            (instance.session_id.clone(), instance.session_path.clone())
        };
        assert_eq!(bound(first_id), (Some(newest.into()), Some(newest_path)));
        assert_eq!(bound(second_id), (Some(older.into()), Some(older_path)));
        assert_eq!(
            muxel_core::session_resume_args(&preset, workspace.instance(second_id).unwrap()),
            Some(vec!["resume".to_string(), older.to_string()])
        );
    }

    #[test]
    fn landed_lookup_never_rebinds_a_pane_that_moved_on() {
        let preset = AgentPreset::codex();
        let project = Project::new("webapp", "/home/me/webapp");
        let pane = started(project.id, &preset, None);
        let sibling = started(
            project.id,
            &preset,
            Some("01a1543c-9700-7000-8000-000000000001"),
        );
        let pane_id = pane.id;
        let mut workspace = Workspace {
            projects: vec![project],
            instances: vec![pane, sibling],
            ..Workspace::default()
        };
        let latest = CodexRestoreQuery {
            kind: CodexRestoreKind::Latest {
                cwd: "/home/me/webapp".into(),
                owned: HashSet::new(),
                limit: 1,
                sibling_sessions: Vec::new(),
            },
            tmux: None,
        };
        let session = |n: u32| CodexSession {
            id: format!("01a1543c-9700-7000-8000-{n:012}"),
            path: PathBuf::from(format!("/home/me/.codex/sessions/rollout-{n}.jsonl")),
        };
        // Every candidate is owned by now: the pane starts fresh.
        assert!(!apply_codex_restore_found(
            &mut workspace,
            pane_id,
            &latest,
            CodexRestoreFound::Latest(vec![session(1)])
        ));
        assert!(!apply_codex_restore_found(
            &mut workspace,
            pane_id,
            &latest,
            CodexRestoreFound::Skip
        ));
        // A pane that captured its own id meanwhile keeps it.
        workspace.instance_mut(pane_id).unwrap().session_id = Some(session(5).id);
        assert!(!apply_codex_restore_found(
            &mut workspace,
            pane_id,
            &latest,
            CodexRestoreFound::Latest(vec![session(2)])
        ));
        // A resume answer for a different id than the pane now holds is stale.
        let resume = CodexRestoreQuery {
            kind: CodexRestoreKind::Resume { id: session(4).id },
            tmux: None,
        };
        for found in [
            CodexResumePath::Missing,
            CodexResumePath::Found(session(4).path),
        ] {
            assert!(!apply_codex_restore_found(
                &mut workspace,
                pane_id,
                &resume,
                CodexRestoreFound::Resume(found)
            ));
        }
        let instance = workspace.instance(pane_id).unwrap();
        assert_eq!(instance.session_id, Some(session(5).id));
        assert!(instance.session_started);
        assert_eq!(instance.session_path, None);
        // An inconclusive walk changes nothing.
        let resume = CodexRestoreQuery {
            kind: CodexRestoreKind::Resume { id: session(5).id },
            tmux: None,
        };
        assert!(!apply_codex_restore_found(
            &mut workspace,
            pane_id,
            &resume,
            CodexRestoreFound::Resume(CodexResumePath::Unknown)
        ));
        assert_eq!(
            workspace.instance(pane_id).unwrap().session_id,
            Some(session(5).id)
        );
    }
}
