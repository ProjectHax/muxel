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
use gpui::{App, AsyncApp, Context, WeakEntity};
use muxel_core::session_discovery::{
    CodexLookupEvidence, SessionDiscoveryKey, SessionDiscoveryRequest, SessionProvider,
};
use muxel_core::tmux::{SessionTarget, Socket};
use muxel_core::{AgentPreset, CodexHintMiss, Workspace};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};
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

/// One worker result. A Codex result also carries the evidence its backoff
/// needs: a reusable miss and the hint the worker observed.
type DiscoveryResult = (
    SessionDiscoveryRequest,
    DiscoveryCandidate,
    Option<CodexHintMiss>,
    Option<String>,
);

enum DiscoveryCandidate {
    Unresolved,
    /// A sibling still owns this exact Codex title's conversation, validated
    /// earlier; it can never be adopted as a new binding.
    CachedSiblingRejection(String),
    Session {
        candidate: SessionCandidate,
        switched: Option<(SystemTime, SystemTime)>,
    },
    Claude {
        snapshot: session_binding::ClaudeBindingSnapshot,
        binding: Option<session_binding::ClaudeBinding>,
    },
    /// The current launch's latest root-session hook, and when it was written.
    Muse {
        metadata: muxel_core::muse::MuseMetadata,
        recorded: Option<SystemTime>,
    },
}

impl DiscoveryCandidate {
    fn session_id(&self) -> Option<&str> {
        match self {
            Self::Unresolved | Self::CachedSiblingRejection(_) => None,
            Self::Session { candidate, .. } => Some(&candidate.id),
            Self::Claude { binding, .. } => {
                binding.as_ref().map(|binding| binding.session_id.as_str())
            }
            Self::Muse { metadata, .. } => Some(&metadata.session_id),
        }
    }
}

struct DiscoveryBatch {
    results: Vec<DiscoveryResult>,
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
    previous_path: Option<PathBuf>,
    previous_started: bool,
    previous_auto_name: Option<String>,
    switched: Option<(SystemTime, SystemTime)>,
    claude: Option<ClaudeCommit>,
    /// Hook state shown once the binding it belongs to is saved.
    muse: Option<muxel_core::muse::MuseMetadata>,
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

struct SessionCandidate {
    id: String,
    /// Codex's validated rollout for it, when the provider has one.
    path: Option<PathBuf>,
}

impl MuxelApp {
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
        if self.is_closing(iid)
            || self.discovery_terminal(iid).is_some()
            || self.failed_launches.contains_key(&iid)
        {
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
                // Hold bumps the binding generation, so the pre-hold answer
                // stays stale even when Undo makes the pane visible again.
                if this.codex_restore_lookups.finish(iid, generation)
                    && !this.is_closing(iid)
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
                !this.is_closing(iid)
                    && this.codex_restore_lookups.holds_launch(
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
        if self.is_closing(iid)
            || instance.literal_argv
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
        } else if muxel_core::muse::is_program(instance.program.as_deref()) {
            (SessionProvider::Muse, None, None)
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
        let mut groups: [Vec<SessionDiscoveryRequest>; 4] = Default::default();
        for request in requests {
            let index = match request.key.provider {
                SessionProvider::Codex => 0,
                SessionProvider::Claude => 1,
                SessionProvider::Grok => 2,
                SessionProvider::Muse => 3,
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
        for (request, candidate, miss, observed_hint) in batch.results {
            let iid = request.key.pane;
            let current = self.session_discovery_key(iid, cx);
            let cached_rejection = match &candidate {
                DiscoveryCandidate::CachedSiblingRejection(id) => Some(id.as_str()),
                _ => None,
            };
            // Earlier results in this batch may already have acquired this
            // conversation; the ownership index follows each staged result.
            let unowned = candidate
                .session_id()
                .is_some_and(|id| !ownership.bound_elsewhere(iid, id));
            // Codex's backoff remembers which sibling owns a rejected title.
            let rejected_owner = candidate
                .session_id()
                .or(cached_rejection)
                .filter(|_| !unowned)
                .and_then(|id| {
                    self.workspace
                        .instances
                        .iter()
                        .find(|instance| {
                            instance.id != iid && instance.session_id.as_deref() == Some(id)
                        })
                        .map(|instance| (instance.id, id.to_string()))
                });
            let valid = self.session_discoveries.finish_with_codex_evidence(
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
                        // Not valid yet, or another pane's conversation.
                        session_binding::defer_claude_binding(record);
                        None
                    }
                }
                DiscoveryCandidate::Muse { metadata, recorded } if unowned => {
                    self.stage_discovered_muse(iid, metadata, recorded, cx)
                }
                DiscoveryCandidate::Session {
                    candidate,
                    switched,
                } if unowned => {
                    #[cfg(windows)]
                    if request.key.provider == SessionProvider::Grok {
                        self.stage_grok_session(
                            &request.key,
                            candidate.id,
                            switched.expect("Grok result carries its observation interval"),
                            cx,
                        )
                    } else {
                        self.stage_discovered_session(iid, candidate, false)
                    }
                    #[cfg(not(windows))]
                    {
                        let _ = switched;
                        self.stage_discovered_session(iid, candidate, false)
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
        // Hidden panes still own their saved and accepted identities. Filtering
        // held panes here would let another pane adopt the live conversation.
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
        if self.is_closing(iid) {
            anyhow::bail!(crate::i18n::t("This terminal is closing.").to_string());
        }
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
        let candidate = SessionCandidate {
            id: identity,
            path: None,
        };
        let Some(update) = self.stage_discovered_session(iid, candidate, false) else {
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
        if self.is_closing(key.pane) {
            return None;
        }
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
        let candidate = SessionCandidate {
            id: session_id,
            path: None,
        };
        self.stage_discovered_session(key.pane, candidate, false)
    }

    fn stage_discovered_claude(
        &mut self,
        iid: Uuid,
        record: session_binding::ClaimedClaudeRecord,
        binding: Option<session_binding::ClaudeBinding>,
        cx: &App,
    ) -> Option<SessionBindingUpdate> {
        if self.is_closing(iid) {
            session_binding::defer_claude_binding(record);
            return None;
        }
        let Some(binding) = binding else {
            session_binding::defer_claude_binding(record);
            return None;
        };
        let terminal = self.discovery_terminal(iid)?;
        let (launch, launch_resumed) = {
            let view = terminal.read(cx);
            let session = view.session();
            (session.id, session.launch_resumed())
        };
        let seen = self
            .claude_hook_seen
            .get(&iid)
            .filter(|(seen_launch, _)| *seen_launch == launch)
            .map(|(_, nonce)| *nonce);
        let bound = self.workspace.instance(iid)?.session_id.as_deref();
        if !session_binding::claude_record_switches(&binding, seen, bound, launch_resumed) {
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
        let candidate = SessionCandidate {
            id: binding.session_id,
            path: Some(binding.path),
        };
        let mut update = self.stage_discovered_session(iid, candidate, true)?;
        update.switched = Some((switched_at, switched_at));
        update.claude = Some(ClaudeCommit {
            record,
            launch,
            stamp: binding.stamp,
        });
        Some(update)
    }

    /// A Muse record that names another conversation than the bound one is a
    /// switch inside the running process; its hook state applies either way.
    fn stage_discovered_muse(
        &mut self,
        iid: Uuid,
        metadata: muxel_core::muse::MuseMetadata,
        recorded: Option<SystemTime>,
        cx: &mut Context<Self>,
    ) -> Option<SessionBindingUpdate> {
        if self.is_closing(iid) {
            return None;
        }
        let bound = self.workspace.instance(iid)?.session_id.as_deref();
        let switched = bound.is_some_and(|bound| bound != metadata.session_id);
        let candidate = SessionCandidate {
            id: metadata.session_id.clone(),
            path: None,
        };
        let Some(mut update) = self.stage_discovered_session(iid, candidate, false) else {
            if let Some(view) = self.discovery_terminal(iid).cloned() {
                view.update(cx, |view, cx| view.set_muse_hook_state(&metadata, cx));
            }
            return None;
        };
        if switched {
            let at = recorded.unwrap_or_else(SystemTime::now);
            update.switched = Some((at, at));
        }
        update.muse = Some(metadata);
        Some(update)
    }

    fn stage_discovered_session(
        &mut self,
        iid: Uuid,
        candidate: SessionCandidate,
        force_boundary: bool,
    ) -> Option<SessionBindingUpdate> {
        if self.is_closing(iid) {
            return None;
        }
        let instance = self.workspace.instance_mut(iid)?;
        let same_id = instance.session_id.as_deref() == Some(candidate.id.as_str());
        let path = candidate
            .path
            .or_else(|| same_id.then(|| instance.session_path.clone()).flatten());
        if !force_boundary && same_id && instance.session_path == path {
            return None;
        }
        let update = SessionBindingUpdate {
            pane: iid,
            previous_id: instance.session_id.clone(),
            previous_path: instance.session_path.clone(),
            previous_started: instance.session_started,
            previous_auto_name: instance.auto_name.clone(),
            switched: None,
            claude: None,
            muse: None,
        };
        instance.session_id = Some(candidate.id);
        instance.session_path = path;
        instance.session_started = true;
        if !same_id || force_boundary {
            instance.auto_name = None;
        }
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
                if let Some(metadata) = update.muse
                    && let Some(view) = self.discovery_terminal(update.pane).cloned()
                {
                    view.update(cx, |view, cx| view.set_muse_hook_state(&metadata, cx));
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
                instance.session_path = update.previous_path;
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
            let mut miss = None;
            let mut observed_hint = None;
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
                    match hint {
                        None => DiscoveryCandidate::Unresolved,
                        Some(hint) => {
                            if let Some((cached_hint, cached_id)) = &request.codex_rejected_hint
                                && *cached_hint == hint
                            {
                                // A sibling still owns this exact title. Read the
                                // title again next time, but do not rescan a
                                // rollout that was already validated for this hint.
                                DiscoveryCandidate::CachedSiblingRejection(cached_id.clone())
                            } else {
                                let stamp = CodexHintMiss::capture(home, &hint, started);
                                if let Some(cached) = request
                                    .codex_miss
                                    .as_ref()
                                    .filter(|cached| cached.reusable_for(&stamp))
                                {
                                    miss = Some(cached.clone());
                                    DiscoveryCandidate::Unresolved
                                } else {
                                    match validated_codex_session_id(
                                        &AgentPreset::codex(),
                                        key.binding.as_deref(),
                                        Some(&hint),
                                        home,
                                        &key.cwd,
                                        Some(key.pane),
                                    ) {
                                        Some(session) => DiscoveryCandidate::Session {
                                            candidate: SessionCandidate {
                                                id: session.id,
                                                path: Some(session.path),
                                            },
                                            switched: None,
                                        },
                                        None => {
                                            miss = Some(stamp);
                                            DiscoveryCandidate::Unresolved
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                SessionProvider::Claude => data_dir
                    .and_then(|data_dir| {
                        session_binding::read_pending_claude_binding(data_dir, key.pane)
                    })
                    .map_or(DiscoveryCandidate::Unresolved, |snapshot| {
                        let binding =
                            session_binding::claude_binding_from_record(&snapshot.bytes, home);
                        DiscoveryCandidate::Claude { snapshot, binding }
                    }),
                SessionProvider::Muse => data_dir
                    .and_then(|data_dir| {
                        session_binding::read_muse_binding(
                            data_dir,
                            key.pane,
                            key.binding.as_deref(),
                        )
                    })
                    .map_or(DiscoveryCandidate::Unresolved, |(metadata, recorded)| {
                        DiscoveryCandidate::Muse { metadata, recorded }
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
                                    candidate: SessionCandidate {
                                        id: session.session_id.clone(),
                                        path: None,
                                    },
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
            (request, candidate, miss, observed_hint)
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
mod binding_regressions {
    use super::SessionCandidate;
    use crate::app::restart_completion_tests::with_app;
    use muxel_core::{Instance, Project};
    use uuid::Uuid;

    #[test]
    fn same_id_provider_boundary_clears_name_but_path_upgrade_preserves_it() {
        with_app(|cx, window| {
            window
                .update(cx, |app, _, cx| {
                    app.current_workspace = Some(Uuid::new_v4());
                    let root = muxel_store::data_dir().unwrap();
                    let project = app
                        .workspace
                        .add_project(Project::new("Claude binding", root.clone()));
                    let mut instance = Instance::shell(project);
                    let identity = Uuid::new_v4().to_string();
                    instance.session_id = Some(identity.clone());
                    instance.auto_name = Some("Previous conversation".into());
                    let id = instance.id;
                    app.workspace.add_instance(instance);
                    let path = root.join("exact-transcript.jsonl");
                    let candidate = || SessionCandidate {
                        id: identity.clone(),
                        path: Some(path.clone()),
                    };

                    let update = app
                        .stage_discovered_session(id, candidate(), false)
                        .unwrap();
                    assert!(app.finish_discovered_sessions(vec![update], cx));
                    let instance = app.workspace.instance(id).unwrap();
                    assert_eq!(instance.auto_name.as_deref(), Some("Previous conversation"));
                    assert_eq!(instance.session_path.as_deref(), Some(path.as_path()));

                    // A new Claude nonce can report A -> B -> A between reads.
                    // Its unchanged final UUID still creates a provider boundary.
                    let update = app.stage_discovered_session(id, candidate(), true).unwrap();
                    assert_eq!(app.workspace.instance(id).unwrap().auto_name, None);
                    assert!(app.finish_discovered_sessions(vec![update], cx));
                    let doc =
                        muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                    let saved = muxel_store::load_workspace_from(&doc).unwrap();
                    assert_eq!(saved.instance(id).unwrap().auto_name, None);
                    assert_eq!(
                        saved.instance(id).unwrap().session_id.as_deref(),
                        Some(identity.as_str())
                    );
                    assert_eq!(
                        saved.instance(id).unwrap().session_path.as_deref(),
                        Some(path.as_path())
                    );
                    assert!(
                        app.stage_discovered_session(id, candidate(), false)
                            .is_none()
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn discovered_boundary_save_failure_restores_the_previous_name() {
        with_app(|cx, window| {
            window
                .update(cx, |app, _, cx| {
                    app.current_workspace = Some(Uuid::new_v4());
                    let project = app.workspace.add_project(Project::new(
                        "Claude binding",
                        muxel_store::data_dir().unwrap(),
                    ));
                    let mut instance = Instance::shell(project);
                    let identity = Uuid::new_v4().to_string();
                    instance.session_id = Some(identity.clone());
                    instance.auto_name = Some("Previous conversation".into());
                    let id = instance.id;
                    let before = serde_json::to_value(&instance).unwrap();
                    app.workspace.add_instance(instance);
                    assert!(app.try_persist());
                    let doc =
                        muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                    std::fs::remove_file(&doc).unwrap();
                    std::fs::create_dir(&doc).unwrap();

                    let candidate = || SessionCandidate {
                        id: identity.clone(),
                        path: None,
                    };
                    let update = app.stage_discovered_session(id, candidate(), true).unwrap();
                    assert_eq!(app.workspace.instance(id).unwrap().auto_name, None);
                    assert!(!app.finish_discovered_sessions(vec![update], cx));
                    assert_eq!(
                        serde_json::to_value(app.workspace.instance(id).unwrap()).unwrap(),
                        before
                    );
                    std::fs::remove_dir(&doc).unwrap();
                    let update = app.stage_discovered_session(id, candidate(), true).unwrap();
                    assert!(app.finish_discovered_sessions(vec![update], cx));
                    assert_eq!(app.workspace.instance(id).unwrap().auto_name, None);
                })
                .unwrap();
        });
    }
}

#[cfg(all(test, windows))]
mod grok_regressions {
    use super::super::{
        AgentStatus, GrokActiveSession, GrokRegistryRead, LoopCleanup, LoopRun, MuxelApp,
        PostRunAction, conpty_test_support::ChildHandle, grok_switch_interval,
        popout_lifecycle_tests::with_app,
    };
    use super::{DiscoveryBatch, DiscoveryCandidate, SessionCandidate};
    use gpui::{AppContext as _, Context, TestAppContext, Window, WindowHandle};
    use muxel_core::{AgentPreset, InjectionMode, Instance, Project};
    use muxel_terminal::{
        CommandSpec, TerminalLaunch, TerminalRuntimeSnapshot, TerminalSession,
        TerminalStatusCoverage, TerminalStatusReason, TerminalView,
    };
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};
    use uuid::Uuid;

    struct StopChild(Arc<TerminalSession>);
    impl Drop for StopChild {
        fn drop(&mut self) {
            self.0.kill();
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
                        candidate: SessionCandidate {
                            id: identity.into(),
                            path: None,
                        },
                        switched: Some((now, now)),
                    },
                    None,
                    None,
                )],
                grok_read: Some((now, now)),
            },
            muxel_store::data_dir().as_deref(),
            cx,
        );
    }

    #[test]
    fn held_discovery_keeps_ownership_and_rejects_late_results_after_undo() {
        for undo in [false, true] {
            with_app(|cx, window| {
                checked_update(window, cx, |app, window, cx| {
                    app._status_timer = gpui::Task::ready(());
                    app.current_workspace = Some(Uuid::new_v4());
                    let (id, session) = pane(app, window, cx);
                    let _stop = StopChild(session);
                    let instance = app.workspace.instance(id).unwrap();
                    let saved = instance.session_id.clone().unwrap();
                    let project = instance.project_id;
                    app.workspace.project_mut(project).unwrap().layout =
                        Some(muxel_core::PaneNode::leaf(id));
                    let key = app.session_discovery_key(id, cx).unwrap();
                    let request = app.session_discoveries.begin(key, Instant::now()).unwrap();
                    let group = app.hold_closes(vec![(id, true)], None, cx).unwrap();
                    assert!(app.session_discovery_key(id, cx).is_none());
                    assert!(app.workspace.project(project).unwrap().layout.is_none());
                    assert!(
                        app.session_ownership(cx)
                            .bound_elsewhere(Uuid::new_v4(), &saved)
                    );
                    if undo {
                        app.undo_close(group, window, cx);
                        assert!(app.session_discovery_key(id, cx).is_some());
                    }
                    let now = SystemTime::now();
                    app.apply_discovery_batch(
                        DiscoveryBatch {
                            results: vec![(
                                request,
                                DiscoveryCandidate::Session {
                                    candidate: SessionCandidate {
                                        id: Uuid::new_v4().to_string(),
                                        path: None,
                                    },
                                    switched: Some((now, now)),
                                },
                                None,
                                None,
                            )],
                            grok_read: Some((now, now)),
                        },
                        muxel_store::data_dir().as_deref(),
                        cx,
                    );
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id.as_deref(),
                        Some(saved.as_str())
                    );
                    assert!(!app.grok_observations.contains_key(&id));
                });
            });
        }
    }

    #[test]
    fn held_codex_restore_does_not_begin_a_history_lookup() {
        with_app(|cx, window| {
            checked_update(window, cx, |app, _window, cx| {
                app._status_timer = gpui::Task::ready(());
                app.current_workspace = Some(Uuid::new_v4());
                let preset = AgentPreset::codex();
                let project = app
                    .workspace
                    .add_project(Project::new("held Codex", "/held-codex"));
                let mut instance = Instance::from_preset(project, &preset);
                instance.session_started = true;
                let id = instance.id;
                app.presets.push(preset);
                app.workspace.add_instance(instance);
                app.workspace.project_mut(project).unwrap().layout =
                    Some(muxel_core::PaneNode::leaf(id));
                app.hold_closes(vec![(id, true)], None, cx).unwrap();
                app.begin_codex_restore_lookup(id, cx);
                assert!(!app.codex_restore_lookups.holds_launch(
                    id,
                    Instant::now(),
                    super::CODEX_RESTORE_LOOKUP_BUDGET,
                ));
            });
        });
    }

    #[test]
    fn gasq_restart_preserves_unsaved_grok_and_then_resumes_its_accepted_identity() {
        with_app(|cx, window| {
            let (old_process, replacement, report, identity, pane) = checked_update(
                window,
                cx,
                |app, window, cx| {
                    // Pump terminal replies below without polling real provider
                    // registries or running unrelated status ticks.
                    app._status_timer = gpui::Task::ready(());
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
                    (old_process, replacement, report, identity, id)
                },
            );
            let replacement_process = ChildHandle::new(&replacement.0);
            // PowerShell and the batch fixture are real OS processes; the
            // virtual GPUI clock cannot accelerate their startup on loaded CI.
            let cap = Instant::now() + Duration::from_secs(30);
            loop {
                cx.executor().advance_clock(Duration::from_millis(10));
                cx.run_until_parked();
                let selected = std::fs::read_to_string(&report);
                if !old_process.running()
                    && selected
                        .as_ref()
                        .is_ok_and(|selected| selected.lines().eq(["--resume", identity.as_str()]))
                {
                    break;
                }
                assert!(
                    Instant::now() < cap,
                    "Restart fixture did not select the accepted identity at {}: {selected:?}; old_running={}, replacement_running={}, output={:?}",
                    report.display(),
                    old_process.running(),
                    replacement_process.running(),
                    window
                        .update(cx, |app, _, cx| {
                            app.terminal_view(pane)
                                .map(|view| view.read(cx).visible_text())
                        })
                        .unwrap(),
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(replacement_process.running());
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
                                    candidate: SessionCandidate {
                                        id: original.clone(),
                                        path: None,
                                    },
                                    switched: Some((read, read)),
                                },
                                None,
                                None,
                            ),
                            (
                                other_request,
                                DiscoveryCandidate::Session {
                                    candidate: SessionCandidate {
                                        id: accepted.clone(),
                                        path: None,
                                    },
                                    switched: Some((read, read)),
                                },
                                None,
                                None,
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

    /// A home and data directory under a temp dir, removed on drop.
    struct ClaudeFixture {
        root: PathBuf,
        home: PathBuf,
        data: PathBuf,
        pane: Uuid,
        request: muxel_core::session_discovery::SessionDiscoveryRequest,
    }

    impl Drop for ClaudeFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl ClaudeFixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("muxel-discovery-test-{}", Uuid::new_v4()));
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
                        binding_path: None,
                        cwd: home.join(".claude/projects/fixture"),
                        hint: None,
                        tmux_session: None,
                        tmux_socket: None,
                    },
                    Instant::now(),
                )
                .unwrap();
            Self {
                root,
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
            match self.read() {
                (snapshot, Some(binding)) => (snapshot, binding),
                _ => panic!("expected validated pending Claude snapshot"),
            }
        }

        /// One worker read of the pane's pending record, validated or not.
        fn read(
            &self,
        ) -> (
            session_binding::ClaudeBindingSnapshot,
            Option<session_binding::ClaudeBinding>,
        ) {
            let (home, data, request) =
                (self.home.clone(), self.data.clone(), self.request.clone());
            let mut batch = std::thread::spawn(move || {
                discover_sessions(vec![request], &home, Some(&data), Default::default())
            })
            .join()
            .unwrap();
            match batch.results.pop().unwrap().1 {
                DiscoveryCandidate::Claude { snapshot, binding } => (snapshot, binding),
                _ => panic!("expected a pending Claude snapshot"),
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
    fn muse_worker_reads_the_current_launch_record_with_its_state_and_time() {
        let root = std::env::temp_dir().join(format!("muxel-discovery-muse-{}", Uuid::new_v4()));
        let (data, share) = (root.join("data"), root.join("share"));
        let env = [
            ("XDG_CONFIG_HOME", root.join("config")),
            ("XDG_DATA_HOME", share.clone()),
        ]
        .map(|(key, path)| (key.to_string(), path.display().to_string()));
        let pane = Uuid::new_v4();
        assert!(session_binding::prepare_muse_hooks(&data, pane, &env, &[], None, false).is_some());
        let launch = data.join(format!("provider-session-bindings/muse/{pane}/launch.json"));
        let launch: serde_json::Value =
            serde_json::from_slice(&std::fs::read(launch).unwrap()).unwrap();
        let nonce: Uuid = serde_json::from_value(launch["launch_id"].clone()).unwrap();
        let session = Uuid::new_v4().to_string();
        let log = share.join(format!("muse/sessions/2026/10/26/{session}"));
        std::fs::create_dir_all(&log).unwrap();
        std::fs::write(log.join("session.jsonl"), "").unwrap();
        let event = json!({
            "session_id": session, "cwd": root.join("repo"),
            "hook_event_name": "Notification", "notification_type": "permission_prompt"
        });
        session_binding::write_muse_binding_from_reader(
            &data,
            pane,
            nonce,
            &session,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();
        let request = SessionDiscovery::default()
            .begin(
                SessionDiscoveryKey {
                    pane,
                    provider: SessionProvider::Muse,
                    launch: Instant::now(),
                    child_pid: None,
                    binding_generation: 0,
                    binding: None,
                    binding_path: None,
                    cwd: root.join("repo"),
                    hint: None,
                    tmux_session: None,
                    tmux_socket: None,
                },
                Instant::now(),
            )
            .unwrap();
        let (home, worker_data) = (root.join("home"), data.clone());
        let mut batch = std::thread::spawn(move || {
            discover_sessions(vec![request], &home, Some(&worker_data), Default::default())
        })
        .join()
        .unwrap();
        match batch.results.pop().unwrap().1 {
            DiscoveryCandidate::Muse { metadata, recorded } => {
                assert_eq!(metadata.session_id, session);
                assert_eq!(metadata.state, muxel_core::muse::MuseState::Blocked);
                assert!(recorded.is_some());
            }
            _ => panic!("expected the pane's Muse record"),
        }
        let _ = std::fs::remove_dir_all(&root);
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
    fn claude_worker_keeps_a_clear_record_until_its_project_directory_exists() {
        let fixture = ClaudeFixture::new();
        // `/clear` in a cwd Claude hasn't written a transcript for yet.
        let identity = Uuid::new_v4();
        let project = fixture.home.join(".claude/projects/fresh");
        let bytes = json!({
            "hook_event_name":"SessionStart", "source":"clear", "session_id":identity,
            "cwd":project, "transcript_path":project.join(format!("{identity}.jsonl")),
            "muxel_hook":{"nonce":Uuid::new_v4(), "recorded_at_us":123456000_u64}
        })
        .to_string()
        .into_bytes();
        std::fs::create_dir_all(fixture.shared().parent().unwrap()).unwrap();
        std::fs::write(fixture.shared(), &bytes).unwrap();
        let (snapshot, binding) = fixture.read();
        assert!(binding.is_none());
        // What the UI does with a record it can't adopt yet.
        session_binding::defer_claude_binding(fixture.claim(&snapshot).unwrap());
        let (retry, binding) = fixture.read();
        assert_eq!(retry.bytes, bytes, "the switch isn't lost");
        assert!(binding.is_none());
        std::fs::create_dir_all(&project).unwrap();
        let (snapshot, binding) = fixture.candidate();
        assert_eq!(snapshot.bytes, bytes);
        assert_eq!(binding.session_id, identity.to_string());
        session_binding::acknowledge_claude_binding(fixture.claim(&snapshot).unwrap());
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
            DiscoveryCandidate::Session { candidate, .. } => Some(candidate),
            DiscoveryCandidate::Unresolved => None,
            DiscoveryCandidate::Claude { .. } => panic!("a Codex lookup found a Claude record"),
            DiscoveryCandidate::Muse { .. } => panic!("a Codex lookup found a Muse record"),
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
                discover_sessions(vec![request], &root, None, Default::default())
                    .results
                    .pop()
                    .unwrap();
            assert!(validated(candidate).is_none());
            assert!(miss.is_some());
            assert!(discovery.finish_with_codex_miss(&request, Some(&key), at, false, miss));
        }
        let path = write(&root, &key, false);
        let request = discovery
            .begin(key.clone(), now + Duration::from_secs(3))
            .unwrap();
        assert!(request.codex_miss.is_some());
        let (_, candidate, miss, _) =
            discover_sessions(vec![request], &root, None, Default::default())
                .results
                .pop()
                .unwrap();
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
            discover_sessions(vec![request], &root, None, Default::default())
                .results
                .pop()
                .unwrap();
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
            discover_sessions(vec![request], &root, None, Default::default())
                .results
                .pop()
                .unwrap();
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
                discover_sessions(vec![request], &root, None, Default::default())
                    .results
                    .pop()
                    .unwrap();
            let cached_rejection = seconds == 1;
            match candidate {
                DiscoveryCandidate::Session { candidate, .. } if !cached_rejection => {
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
            discover_sessions(vec![request], &root, None, Default::default())
                .results
                .pop()
                .unwrap();
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
            discover_sessions(vec![first], &root, None, Default::default())
                .results
                .pop()
                .unwrap();
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
            discover_sessions(vec![cached], &root, None, Default::default())
                .results
                .pop()
                .unwrap();
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
        let (_, discovered, _, _) = discover_sessions(vec![retry], &root, None, Default::default())
            .results
            .pop()
            .unwrap();
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
