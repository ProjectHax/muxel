//! muxel-core — the pure domain model for muxel: projects, agent instances, and
//! the pane layout tree. No UI, no I/O; everything here is serializable and
//! unit-tested so the GPUI app and persistence layer can build on it.

mod agent;
pub mod diff;
mod gui_path;
pub mod memory;
mod pane;
mod shell;
pub mod ssh;
pub mod tmux;
pub mod worktree;

pub use agent::{
    AgentPreset, EnvVar, InjectionMode, MEMORY_DIR, MEMORY_FILE, ResolvedLaunch,
    claude_session_path, memory_header, memory_instruction, resolve_launch, session_resume_args,
};
pub use diff::{SplitRow, split_diff};
pub use gui_path::{augmented_linux_path, augmented_macos_path};
pub use pane::{
    FocusDir, LeafData, PaneNode, SplitDirection, add_tab, add_tab_at, focus_in_direction,
    move_into_split, move_into_tabs, move_pane_beside, move_tab_to, remove, set_active_tab,
    set_split_sizes, set_tab_order, split, split_beside, swap_instances, swap_panes,
};
pub use shell::{join_words, split_words};
pub use worktree::Worktree;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
pub use uuid::Uuid;

/// What a pane holds. Defaults to `Terminal` so instances persisted before
/// editors existed deserialize correctly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstanceKind {
    /// A terminal/agent pane (a PTY process).
    #[default]
    Terminal,
    /// A text-editor pane (a file open on disk; see `editor_path`).
    Editor,
    /// A read-only git-diff pane. `editor_path` holds the directory to diff
    /// (the agent's worktree, or the project root).
    Diff,
    /// An embedded web-browser pane (`browser_url` holds the current URL).
    /// Only macOS/Windows render these embedded; Linux opens links in a
    /// separate browser window instead.
    Browser,
}

/// A single agent/terminal instance's persistent metadata. The live terminal
/// (PTY + view) is owned by the app, keyed by `id`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instance {
    pub id: Uuid,
    pub project_id: Uuid,
    pub title: String,
    /// Whether this pane is a terminal or a code editor.
    #[serde(default)]
    pub kind: InstanceKind,
    /// For editor panes: the file on disk (None = an unsaved "Untitled" buffer).
    #[serde(default)]
    pub editor_path: Option<PathBuf>,
    /// For browser panes: the current URL (kept fresh so a restart restores it).
    #[serde(default)]
    pub browser_url: Option<String>,
    /// User-assigned name; when set it fully replaces the agent's own title.
    #[serde(default)]
    pub custom_name: Option<String>,
    /// Program to run; `None` means the user's default shell.
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional system prompt delivered to the agent on startup.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// How the system prompt is delivered.
    #[serde(default)]
    pub injection: InjectionMode,
    /// Name of the preset this instance was created from (for display).
    #[serde(default)]
    pub preset: String,
    /// Id of the preset this instance was created from.
    #[serde(default)]
    pub preset_id: Option<Uuid>,
    /// Environment variables for the process.
    #[serde(default)]
    pub env: Vec<EnvVar>,
    /// Run inside a persistent tmux session (Unix only).
    #[serde(default)]
    pub use_tmux: bool,
    /// Run in a dedicated git worktree.
    #[serde(default)]
    pub use_worktree: bool,
    /// tmux session name, once created.
    #[serde(default)]
    pub tmux_session: Option<String>,
    /// Worktree directory, once created (becomes this instance's working dir).
    #[serde(default)]
    pub worktree_path: Option<std::path::PathBuf>,
    /// Branch checked out in the worktree.
    #[serde(default)]
    pub worktree_branch: Option<String>,
    /// Shift+Tab presses to send at startup (runner "auto mode"); 0 = none.
    #[serde(default)]
    pub auto_mode_presses: u8,
    /// Created by a runner (one-off task launcher).
    #[serde(default)]
    pub is_runner: bool,
    /// Press Enter to submit after typing the startup prompt. New instances set
    /// this true; a runner clears it after its first launch so reopening the app
    /// re-types the prompt but does NOT auto-submit it (defaults false for
    /// already-persisted instances).
    #[serde(default)]
    pub auto_submit: bool,
    /// Tab is pinned: kept in the leftmost block of its pane. App-enforced order.
    #[serde(default)]
    pub pinned: bool,
    /// Registry id of the [`Worktree`] this instance runs in (None = none, or a
    /// legacy instance before migration).
    #[serde(default)]
    pub worktree_id: Option<Uuid>,
    /// Stable session ID for a resume-capable agent (e.g. Claude). muxel launches
    /// the first time with `--session-id <this>` and resumes with `--resume <this>`
    /// on restart. `None` until first launch / for agents without resume support.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Whether this instance's session has been launched at least once, so a
    /// respawn resumes it instead of starting a new conversation.
    #[serde(default)]
    pub session_started: bool,
}

impl Instance {
    /// A plain shell instance.
    pub fn shell(project_id: Uuid) -> Self {
        Self::from_preset(project_id, &AgentPreset::shell())
    }

    /// Create an instance from a preset.
    pub fn from_preset(project_id: Uuid, preset: &AgentPreset) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id,
            title: preset.name.clone(),
            kind: InstanceKind::Terminal,
            editor_path: None,
            browser_url: None,
            custom_name: None,
            program: preset.program.clone(),
            args: preset.compose_args(),
            system_prompt: preset.system_prompt.clone(),
            injection: preset.injection.clone(),
            preset: preset.name.clone(),
            preset_id: Some(preset.id),
            env: preset.env.clone(),
            use_tmux: false,
            use_worktree: false,
            tmux_session: None,
            worktree_path: None,
            worktree_branch: None,
            auto_mode_presses: 0,
            is_runner: false,
            // New instances submit their startup prompt; persisted ones default
            // to false (so restored runners re-type without auto-submitting).
            auto_submit: true,
            pinned: false,
            worktree_id: None,
            session_id: None,
            session_started: false,
        }
    }

    /// Create a read-only git-diff instance for `dir` (the directory to diff).
    pub fn diff(project_id: Uuid, dir: PathBuf) -> Self {
        Self {
            kind: InstanceKind::Diff,
            editor_path: Some(dir),
            title: "Diff".to_string(),
            ..Self::editor(project_id, None)
        }
    }

    /// Create an embedded-browser instance showing `url`.
    pub fn browser(project_id: Uuid, url: String) -> Self {
        Self {
            kind: InstanceKind::Browser,
            browser_url: Some(url),
            title: "Browser".to_string(),
            ..Self::editor(project_id, None)
        }
    }

    /// Create an editor instance for an optional file path (None = Untitled).
    pub fn editor(project_id: Uuid, path: Option<PathBuf>) -> Self {
        let title = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_string());
        Self {
            id: Uuid::new_v4(),
            project_id,
            title,
            kind: InstanceKind::Editor,
            editor_path: path,
            browser_url: None,
            custom_name: None,
            program: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::default(),
            preset: String::new(),
            preset_id: None,
            env: Vec::new(),
            use_tmux: false,
            use_worktree: false,
            tmux_session: None,
            worktree_path: None,
            worktree_branch: None,
            auto_mode_presses: 0,
            is_runner: false,
            auto_submit: false,
            pinned: false,
            worktree_id: None,
            session_id: None,
            session_started: false,
        }
    }
}

/// How muxel authenticates an SSH connection. For `Password`, the secret lives
/// in the OS keychain (fed to ssh via sshpass), never in the persisted config.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshAuth {
    /// Use the running ssh-agent / the user's default keys.
    #[default]
    Agent,
    /// Use a specific identity file (see [`RemoteHost::identity_file`]).
    Key,
    /// Password auth; the password is stored in the OS keychain.
    Password,
}

/// A saved SSH remote host: connection settings for remote development. The
/// password (when `auth == Password`) is stored in the OS keychain keyed by `id`,
/// never in this struct.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteHost {
    pub id: Uuid,
    /// Display label.
    pub name: String,
    /// Host name / IP, or a `~/.ssh/config` alias (ssh resolves it).
    pub hostname: String,
    /// SSH port (`None` = ssh default).
    #[serde(default)]
    pub port: Option<u16>,
    /// Login user (empty = ssh/config default).
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub auth: SshAuth,
    /// Identity file for [`SshAuth::Key`].
    #[serde(default)]
    pub identity_file: Option<PathBuf>,
    /// When set, this host's credential fields (`user`, `auth`, `identity_file`)
    /// come from the shared [`Identity`] with this id instead of the inline fields.
    #[serde(default)]
    pub identity_id: Option<Uuid>,
    /// ProxyJump host (`-J`), if any.
    #[serde(default)]
    pub jump_host: Option<String>,
    /// Forward the ssh-agent (`-A`).
    #[serde(default)]
    pub forward_agent: bool,
    /// `StrictHostKeyChecking` value ("" = `accept-new`).
    #[serde(default)]
    pub strict_host_key: String,
    /// `ServerAliveInterval` seconds, if set.
    #[serde(default)]
    pub keepalive_secs: Option<u32>,
    /// Enable SSH compression (`-o Compression=yes`) — worth it on slow / high-
    /// latency links, a waste of CPU on a fast LAN.
    #[serde(default)]
    pub compression: bool,
    /// Extra raw `-o KEY=VALUE` ssh options.
    #[serde(default)]
    pub extra_options: Vec<String>,
    /// Default new remote panes here to a persistent tmux session.
    #[serde(default = "default_true")]
    pub default_use_tmux: bool,
}

impl RemoteHost {
    pub fn new(name: impl Into<String>, hostname: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            hostname: hostname.into(),
            port: None,
            user: String::new(),
            auth: SshAuth::Agent,
            identity_file: None,
            identity_id: None,
            jump_host: None,
            forward_agent: false,
            strict_host_key: String::new(),
            keepalive_secs: None,
            compression: false,
            extra_options: Vec::new(),
            default_use_tmux: true,
        }
    }

    /// Effective connection settings: when this host references an [`Identity`] that
    /// exists in `identities`, its credential fields (`user`, `auth`,
    /// `identity_file`) are overlaid from that identity; otherwise the host's inline
    /// fields are used unchanged. `id`, `identity_id`, and every transport field
    /// (port, jump, keepalive, extra options, …) are preserved — so the argv builders
    /// in [`crate::ssh`] and the per-host ControlMaster socket keep working verbatim.
    pub fn effective(&self, identities: &[Identity]) -> RemoteHost {
        let mut h = self.clone();
        if let Some(iid) = self.identity_id
            && let Some(id) = identities.iter().find(|i| i.id == iid)
        {
            h.user = id.user.clone();
            h.auth = id.auth;
            h.identity_file = id.identity_file.clone();
        }
        h
    }

    /// The keychain / session-cache owner of this host's password: the referenced
    /// identity (only while it still exists), else the host itself. Lets several
    /// hosts share one stored secret via a common identity.
    pub fn secret_owner(&self, identities: &[Identity]) -> Uuid {
        self.identity_id
            .filter(|iid| identities.iter().any(|i| i.id == *iid))
            .unwrap_or(self.id)
    }
}

/// A reusable, named SSH login identity: the credential half of a host — login
/// user, auth method, and an optional key file — that many hosts can share via
/// [`RemoteHost::identity_id`]. The password (for [`SshAuth::Password`]) lives in
/// the OS keychain keyed by `id`, never in this struct — mirroring [`RemoteHost`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub id: Uuid,
    /// Display label.
    pub name: String,
    /// Login user (empty = ssh/config default).
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub auth: SshAuth,
    /// Identity file for [`SshAuth::Key`].
    #[serde(default)]
    pub identity_file: Option<PathBuf>,
}

impl Identity {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            user: String::new(),
            auth: SshAuth::Agent,
            identity_file: None,
        }
    }
}

/// A project's link to a remote host: which host, and the project's root
/// directory on that host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRef {
    pub host_id: Uuid,
    /// Absolute path of the project root on the remote host.
    pub remote_root: String,
}

/// A project: a named workspace rooted at a directory, with a pane layout.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub root_path: PathBuf,
    /// The pane tree; `None` when the project has no panes.
    pub layout: Option<PaneNode>,
    /// Default preset id for new panes in this project (None = use the global default).
    #[serde(default)]
    pub default_preset: Option<Uuid>,
    /// Saved agents to (re)launch into this project on demand.
    #[serde(default)]
    pub startup: Vec<StartupAgent>,
    /// When set, this project runs on a remote host over SSH; `root_path` is then
    /// cosmetic and the working directory comes from [`RemoteRef::remote_root`].
    #[serde(default)]
    pub remote: Option<RemoteRef>,
    /// Inject a shared-memory instruction into this project's agents, pointing them
    /// at a `.muxel/MEMORY.md` file they read + append lessons to across runs.
    #[serde(default)]
    pub memory_enabled: bool,
    /// Unix-seconds version stamp for this project's pane layout, advanced when the
    /// layout/instances change. Drives newer-wins sync of a remote project's layout
    /// to/from `<remote_root>/.muxel/workspace.json`. `None` until first change/sync.
    #[serde(default)]
    pub layout_updated_at: Option<u64>,
}

/// One agent in a project's saved startup set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StartupAgent {
    /// Preset to launch (None = the default shell).
    #[serde(default)]
    pub preset_id: Option<Uuid>,
    /// Launch it in its own git worktree.
    #[serde(default)]
    pub use_worktree: bool,
}

impl Project {
    pub fn new(name: impl Into<String>, root_path: impl Into<PathBuf>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            root_path: root_path.into(),
            layout: None,
            default_preset: None,
            startup: Vec::new(),
            remote: None,
            memory_enabled: false,
            layout_updated_at: None,
        }
    }

    /// Whether this project runs on a remote host over SSH.
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// All instance ids referenced by this project's layout.
    pub fn instances(&self) -> Vec<Uuid> {
        self.layout
            .as_ref()
            .map(|l| l.collect_instances())
            .unwrap_or_default()
    }

    pub fn first_instance(&self) -> Option<Uuid> {
        self.layout.as_ref().and_then(|l| l.first_instance())
    }

    pub fn is_empty(&self) -> bool {
        self.layout.is_none()
    }
}

/// The full set of projects + instance metadata. Serializable for persistence.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Workspace {
    pub projects: Vec<Project>,
    pub active_project: Option<Uuid>,
    pub instances: Vec<Instance>,
    /// Persisted project-sidebar width in pixels (None = default).
    #[serde(default)]
    pub sidebar_width: Option<f32>,
    /// Persisted file-browser sidebar width in pixels (None = default).
    #[serde(default)]
    pub file_browser_width: Option<f32>,
    /// Persisted memory-panel sidebar width in pixels (None = default).
    #[serde(default)]
    pub memory_panel_width: Option<f32>,
    /// Persisted git-diff panel width in pixels (None = default).
    #[serde(default)]
    pub gitdiff_panel_width: Option<f32>,
    /// Named git worktrees, referenced by instances via `Instance.worktree_id`.
    #[serde(default)]
    pub worktrees: Vec<Worktree>,
    /// Projects pinned to their own full muxel window (project id → monitor +
    /// last geometry): reopened there when the workspace loads. A missing
    /// monitor leaves the project in the main window (pin kept).
    #[serde(default)]
    pub project_windows: std::collections::HashMap<Uuid, ProjectWindow>,
}

/// Where a project's dedicated window lives: which monitor (stable UUID) and
/// the exact window geometry to restore it with.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ProjectWindow {
    /// The display's stable UUID.
    pub display: Uuid,
    /// The window's last position/size on that display (global coordinates).
    #[serde(default)]
    pub geom: Option<WindowGeom>,
}

impl Workspace {
    pub fn project(&self, id: Uuid) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn project_mut(&mut self, id: Uuid) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.id == id)
    }

    pub fn active(&self) -> Option<&Project> {
        self.active_project.and_then(|id| self.project(id))
    }

    pub fn instance(&self, id: Uuid) -> Option<&Instance> {
        self.instances.iter().find(|i| i.id == id)
    }

    pub fn instance_mut(&mut self, id: Uuid) -> Option<&mut Instance> {
        self.instances.iter_mut().find(|i| i.id == id)
    }

    /// Add a project; it becomes active if none was.
    pub fn add_project(&mut self, project: Project) -> Uuid {
        let id = project.id;
        self.projects.push(project);
        if self.active_project.is_none() {
            self.active_project = Some(id);
        }
        id
    }

    /// Move project `id` one slot toward the front (`up`) or back of the sidebar
    /// order. Returns whether anything moved (false if `id` is unknown or already
    /// at the relevant end). Pure index math, so it's unit-testable.
    pub fn move_project(&mut self, id: Uuid, up: bool) -> bool {
        let Some(i) = self.projects.iter().position(|p| p.id == id) else {
            return false;
        };
        let j = if up {
            i.checked_sub(1)
        } else {
            (i + 1 < self.projects.len()).then_some(i + 1)
        };
        match j {
            Some(j) => {
                self.projects.swap(i, j);
                true
            }
            None => false,
        }
    }

    pub fn add_instance(&mut self, instance: Instance) {
        self.instances.push(instance);
    }

    pub fn remove_instance_meta(&mut self, id: Uuid) {
        self.instances.retain(|i| i.id != id);
    }

    pub fn worktree(&self, id: Uuid) -> Option<&Worktree> {
        self.worktrees.iter().find(|w| w.id == id)
    }

    pub fn worktree_mut(&mut self, id: Uuid) -> Option<&mut Worktree> {
        self.worktrees.iter_mut().find(|w| w.id == id)
    }

    pub fn add_worktree(&mut self, worktree: Worktree) {
        self.worktrees.push(worktree);
    }

    /// Drop the registry entry (does not touch the filesystem).
    pub fn remove_worktree_meta(&mut self, id: Uuid) {
        self.worktrees.retain(|w| w.id != id);
    }

    /// Instance ids currently referencing `worktree_id`.
    pub fn instances_using(&self, worktree_id: Uuid) -> Vec<Uuid> {
        self.instances
            .iter()
            .filter(|i| i.worktree_id == Some(worktree_id))
            .map(|i| i.id)
            .collect()
    }

    /// Next color index for a new worktree in `project_id`: the lowest unused
    /// slot 0..8 among the project's live (non-detached) worktrees, wrapping.
    pub fn next_worktree_color(&self, project_id: Uuid) -> u8 {
        let used: Vec<u8> = self
            .worktrees
            .iter()
            .filter(|w| w.project_id == project_id && !w.detached)
            .map(|w| w.color)
            .collect();
        (0u8..8).find(|c| !used.contains(c)).unwrap_or(0)
    }
}

/// On-disk format version for [`RemoteLayout`]; bump on incompatible changes.
pub const REMOTE_LAYOUT_VERSION: u32 = 1;

/// A self-contained snapshot of one remote project's pane layout, stored on the
/// remote host at `<remote_root>/.muxel/workspace.json` so another machine can
/// restore the session without recreating panes. Holds the layout tree plus the
/// instances it references and the worktrees those instances use; `updated_at`
/// (unix secs) drives newer-wins resolution between the local and remote copies.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteLayout {
    pub version: u32,
    pub updated_at: u64,
    /// The project's identity root: its path on the remote host, or — for a local
    /// project synced so a remote peer (the iOS app) can attach over SSH — its local
    /// root path. A mismatch on load means the doc was captured for a different
    /// project and is ignored.
    pub remote_root: String,
    pub layout: Option<PaneNode>,
    #[serde(default)]
    pub instances: Vec<Instance>,
    #[serde(default)]
    pub worktrees: Vec<Worktree>,
}

impl RemoteLayout {
    /// Snapshot `project`'s layout plus the instances it references and the
    /// worktrees those instances use, in a canonical (id-sorted) order so two
    /// captures of identical content compare equal. `updated_at` is the project's
    /// `layout_updated_at`, falling back to `now` when it has never been stamped.
    pub fn capture(project: &Project, workspace: &Workspace, now: u64) -> Self {
        // A remote project is identified by its path on the host; a local project
        // (synced for an SSH peer like the iOS app) by its local root path.
        let remote_root = project
            .remote
            .as_ref()
            .map(|r| r.remote_root.clone())
            .unwrap_or_else(|| project.root_path.display().to_string());

        let mut instances: Vec<Instance> = project
            .instances()
            .iter()
            .filter_map(|id| workspace.instance(*id).cloned())
            .collect();
        instances.sort_by_key(|i| i.id);

        let mut wt_ids: Vec<Uuid> = instances.iter().filter_map(|i| i.worktree_id).collect();
        wt_ids.sort();
        wt_ids.dedup();
        let mut worktrees: Vec<Worktree> = wt_ids
            .iter()
            .filter_map(|id| workspace.worktree(*id).cloned())
            .collect();
        worktrees.sort_by_key(|w| w.id);

        Self {
            version: REMOTE_LAYOUT_VERSION,
            updated_at: project.layout_updated_at.unwrap_or(now),
            remote_root,
            layout: project.layout.clone(),
            instances,
            worktrees,
        }
    }

    /// Parse a stored document, rejecting a wrong format version or one captured
    /// for a different `remote_root` (both treated as "no usable remote doc").
    pub fn parse(json: &str, expect_root: &str) -> Option<Self> {
        let doc: Self = serde_json::from_str(json).ok()?;
        (doc.version == REMOTE_LAYOUT_VERSION && doc.remote_root == expect_root).then_some(doc)
    }

    /// Pretty JSON for storage on the remote.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// A stable key over the *content* (layout + instances + worktrees) that
    /// ignores `version`/`updated_at`, for "did anything change" and "are these in
    /// sync" checks. Relies on [`capture`](Self::capture)'s canonical ordering and
    /// serde_json's deterministic (sorted-key) object encoding.
    pub fn content_key(&self) -> String {
        serde_json::json!({
            "layout": self.layout,
            "instances": self.instances,
            "worktrees": self.worktrees,
        })
        .to_string()
    }
}

/// One-time migration: give legacy per-instance worktrees a registry entry.
/// If any instance already has a `worktree_id` this is a no-op. Instances are
/// grouped by `worktree_path`, so any that share a path share one [`Worktree`].
pub fn migrate_worktrees(workspace: &mut Workspace) {
    if workspace.instances.iter().any(|i| i.worktree_id.is_some()) {
        return;
    }
    use std::collections::HashMap;
    let legacy: Vec<(Uuid, Uuid, PathBuf, String)> = workspace
        .instances
        .iter()
        .filter_map(|i| {
            let path = i.worktree_path.clone()?;
            let branch = i.worktree_branch.clone().unwrap_or_default();
            Some((i.id, i.project_id, path, branch))
        })
        .collect();
    let mut by_path: HashMap<PathBuf, Uuid> = HashMap::new();
    for (iid, pid, path, branch) in legacy {
        let wid = *by_path.entry(path.clone()).or_insert_with(|| {
            let color = workspace.next_worktree_color(pid);
            let id = Uuid::new_v4();
            workspace.worktrees.push(Worktree {
                id,
                project_id: pid,
                name: worktree::random_name(),
                path,
                branch,
                color,
                detached: false,
            });
            id
        });
        if let Some(inst) = workspace.instances.iter_mut().find(|i| i.id == iid) {
            inst.worktree_id = Some(wid);
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_presets() -> Vec<AgentPreset> {
    AgentPreset::defaults()
}

fn default_font_size() -> f32 {
    14.0
}

fn default_ui_font_size() -> f32 {
    16.0
}

fn default_zoom() -> f32 {
    1.0
}

fn default_pane_border() -> String {
    "subtle".to_string()
}

fn default_terminal_mouse() -> String {
    "copy_paste".to_string()
}

/// A user keybinding override: an action name bound to a keystroke string.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyBindingCfg {
    pub action: String,
    pub keystroke: String,
}

/// A reusable task launcher: spawn an agent, flip it into auto-accept mode
/// (send Shift+Tab `auto_mode_presses` times), then type `prompt` (with the
/// user's run-time details substituted for `{{input}}`, else appended) + Enter.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Runner {
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    pub name: String,
    /// Agent preset to launch; `None` = the current/default preset.
    #[serde(default)]
    pub preset_id: Option<Uuid>,
    /// Shift+Tab presses to send after the agent starts (to reach auto mode).
    #[serde(default)]
    pub auto_mode_presses: u8,
    /// Base task prompt. `{{input}}` is replaced with the user's run-time text.
    #[serde(default)]
    pub prompt: String,
}

impl Runner {
    fn new(name: &str, prompt: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.to_string(),
            preset_id: None,
            auto_mode_presses: 3,
            prompt: prompt.to_string(),
        }
    }

    /// Built-in example runners.
    pub fn defaults() -> Vec<Runner> {
        vec![
            Runner::new(
                "Review",
                "Review the current changes (git diff) for correctness, bugs, and \
                 code quality. Call out specific issues with file:line references and \
                 suggest concrete fixes.\n\n{{input}}",
            ),
            Runner::new(
                "Security Review",
                "Perform a security review of the current changes (git diff). Look for \
                 injection, auth/authorization gaps, unsafe input handling, leaked \
                 secrets, and risky dependencies. Report findings with severity and \
                 file:line.\n\n{{input}}",
            ),
        ]
    }
}

/// A reusable snippet of text typed into an **already-running** pane on demand
/// (unlike a [`Runner`], which spawns a new agent). `submit` decides whether
/// Enter is pressed after the text — so a snippet can either run immediately or
/// just be dropped into the input for you to review and send.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snippet {
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    pub name: String,
    /// The text typed into the pane.
    #[serde(default)]
    pub text: String,
    /// Press Enter after typing (run it) vs. leave it unsubmitted in the input.
    #[serde(default)]
    pub submit: bool,
}

impl Snippet {
    fn new(name: &str, text: &str, submit: bool) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.to_string(),
            text: text.to_string(),
            submit,
        }
    }

    /// Built-in example snippets (seeded once; users can edit or delete them).
    pub fn defaults() -> Vec<Snippet> {
        vec![
            Snippet::new("Continue", "continue", true),
            Snippet::new("Yes", "yes", true),
            Snippet::new(
                "Plan first",
                "Before you start, outline your plan and wait for my confirmation.",
                false,
            ),
        ]
    }
}

/// When a [`Loop`] fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopSchedule {
    /// Every N minutes since the last run.
    EveryMinutes { minutes: u32 },
    /// Every N hours since the last run.
    EveryHours { hours: u32 },
    /// Once per day at a local time of day.
    DailyAt { hour: u8, minute: u8 },
}

impl Default for LoopSchedule {
    fn default() -> Self {
        LoopSchedule::EveryHours { hours: 1 }
    }
}

impl LoopSchedule {
    /// Whether the loop is due now. `now` = unix seconds, `now_tod` = seconds since
    /// local midnight, `last_run` = unix seconds of the previous fire (loops are
    /// "armed" with `last_run = now` at creation, so an interval first fires after
    /// one interval and a daily-at created after its time waits until the next day).
    pub fn is_due(&self, last_run: Option<u64>, now: u64, now_tod: u32) -> bool {
        match self {
            LoopSchedule::EveryMinutes { minutes } => {
                last_run.is_some_and(|t| now.saturating_sub(t) >= (*minutes as u64) * 60)
            }
            LoopSchedule::EveryHours { hours } => {
                last_run.is_some_and(|t| now.saturating_sub(t) >= (*hours as u64) * 3600)
            }
            LoopSchedule::DailyAt { hour, minute } => {
                let target = (*hour as u32) * 3600 + (*minute as u32) * 60;
                if now_tod < target {
                    return false;
                }
                // The unix second of today's target time of day.
                let today_target = now - now_tod as u64 + target as u64;
                last_run.is_none_or(|t| t < today_target)
            }
        }
    }
}

/// What happens to a loop's agent once it finishes a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostRunAction {
    /// Leave the agent running.
    #[default]
    Leave,
    /// Close the pane once the agent finishes its turn.
    Exit,
}

/// A scheduled task launcher: run a saved prompt on an agent, in a project, on a
/// timer. A [`Runner`] plus a schedule, a target project, and a post-run policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Loop {
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    pub name: String,
    /// Agent preset to launch; `None` = the current/default preset.
    #[serde(default)]
    pub preset_id: Option<Uuid>,
    /// Project to spawn the agent into.
    pub project_id: Uuid,
    /// Shift+Tab presses to send after the agent starts (to reach auto mode).
    #[serde(default)]
    pub auto_mode_presses: u8,
    /// The prompt to type each run.
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub schedule: LoopSchedule,
    #[serde(default)]
    pub post_run: PostRunAction,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Unix seconds of the last fire (persisted so schedules survive restarts).
    #[serde(default)]
    pub last_run: Option<u64>,
}

impl Loop {
    pub fn new(name: impl Into<String>, project_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            preset_id: None,
            project_id,
            auto_mode_presses: 0,
            prompt: String::new(),
            schedule: LoopSchedule::default(),
            post_run: PostRunAction::Leave,
            enabled: true,
            last_run: None,
        }
    }
}

/// Persisted main-window geometry (plain pixels; converted to/from gpui bounds
/// in the app).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct WindowGeom {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

/// Metadata for one workspace: a named workspace (its own projects + layouts).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub id: Uuid,
    pub name: String,
}

/// The list of workspaces plus the most recently used one (for pre-selection).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkspacesIndex {
    // `alias = "profiles"` reads the pre-rename index (`{"profiles": [...]}`) so
    // existing setups aren't lost; muxel rewrites it as `workspaces` on next save.
    #[serde(default, alias = "profiles")]
    pub workspaces: Vec<WorkspaceMeta>,
    #[serde(default)]
    pub current: Option<Uuid>,
}

/// User-facing settings, persisted as hand-editable TOML.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub default_use_tmux: bool,
    #[serde(default)]
    pub default_use_worktree: bool,
    #[serde(default = "default_true")]
    pub notifications_enabled: bool,
    /// Open ctrl+clicked terminal links in the built-in browser (an embedded
    /// pane on macOS/Windows, a separate muxel-managed window on Linux). Off →
    /// links open in the system default browser.
    #[serde(default = "default_true")]
    pub browser_enabled: bool,
    /// Close a pane automatically when its process exits.
    #[serde(default = "default_true")]
    pub close_on_exit: bool,
    /// Ask for confirmation before closing a pane, per kind. Terminals default
    /// on (they hold a running process); editors/diffs default off (cheap to
    /// reopen, nothing is lost).
    #[serde(default = "default_true")]
    pub confirm_close_terminal: bool,
    #[serde(default)]
    pub confirm_close_editor: bool,
    #[serde(default)]
    pub confirm_close_diff: bool,
    /// Highest Terms/Privacy version the user has accepted (0 = never). When this
    /// is below [`CURRENT_TERMS_VERSION`] the app shows the first-run acceptance
    /// screen.
    #[serde(default)]
    pub accepted_terms_version: u32,
    /// Version of the built-in preset set already merged in (so new built-ins
    /// like Hermes/Ollama reach existing users once, without resurrecting ones
    /// they deleted).
    #[serde(default)]
    pub preset_seed_version: u32,
    /// Id (or, for back-compat, name) of the default preset for new agents.
    #[serde(default)]
    pub default_preset: String,
    /// The agent preset library.
    #[serde(default = "default_presets")]
    pub presets: Vec<AgentPreset>,
    /// Active theme name (empty = default dark theme).
    #[serde(default)]
    pub theme: String,
    /// Theme mode override: "dark" | "light" | "" (use the theme's own mode).
    #[serde(default)]
    pub theme_mode: String,
    /// UI language override (BCP-47, e.g. "fr", "zh-CN"). None/empty = auto-detect
    /// from the OS locale at startup.
    #[serde(default)]
    pub language: Option<String>,
    /// Terminal font family (empty = built-in per-OS monospace default).
    #[serde(default)]
    pub font_family: String,
    /// Terminal font size (independent of the interface font size).
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// Interface (non-terminal) base font size; scales all UI text + spacing.
    #[serde(default = "default_ui_font_size")]
    pub ui_font_size: f32,
    #[serde(default = "default_zoom")]
    pub zoom: f32,
    /// Pane border intensity: "off" | "subtle" | "bold".
    #[serde(default = "default_pane_border")]
    pub pane_border: String,
    /// Terminal mouse copy/paste mode: "copy_paste" | "menu" | "copy_on_select".
    #[serde(default = "default_terminal_mouse")]
    pub terminal_mouse: String,
    /// Diff window layout: `true` = side-by-side split, `false` = unified.
    #[serde(default)]
    pub diff_split_view: bool,
    /// Hide muxel to a system-tray icon (instead of quitting) when the window is
    /// closed, minimized, or exited. Restore from the tray menu.
    #[serde(default)]
    pub minimize_to_tray: bool,
    /// Enable the developer console (a popped-out error log toggled with F12).
    #[serde(default)]
    pub dev_console_enabled: bool,
    /// Keybinding overrides.
    #[serde(default)]
    pub keybindings: Vec<KeyBindingCfg>,
    /// Reusable task launchers.
    #[serde(default = "default_runners")]
    pub runners: Vec<Runner>,
    /// Reusable text snippets typed into an existing pane on demand.
    #[serde(default = "default_snippets")]
    pub snippets: Vec<Snippet>,
    /// Scheduled task launchers (run a prompt on a timer).
    #[serde(default)]
    pub loops: Vec<Loop>,
    /// Key chords (e.g. `ctrl-p`) that, while a terminal is focused, are sent to
    /// the PTY instead of triggering muxel's shortcut — so agents like opencode
    /// (Ctrl+P for commands) receive them.
    #[serde(default = "default_passthrough_keys")]
    pub terminal_passthrough_keys: Vec<String>,
    /// Saved SSH remote hosts for remote development.
    #[serde(default)]
    pub remotes: Vec<RemoteHost>,
    /// Reusable SSH login identities that hosts can reference (shared credentials).
    #[serde(default)]
    pub identities: Vec<Identity>,
    // --- Code-editor pane settings ---
    /// Editor font family (empty = the theme's monospace font).
    #[serde(default)]
    pub editor_font_family: String,
    /// Editor font size.
    #[serde(default = "default_editor_font_size")]
    pub editor_font_size: f32,
    /// Editor indent width (spaces).
    #[serde(default = "default_editor_tab_size")]
    pub editor_tab_size: u32,
    /// Wrap long lines in the editor.
    #[serde(default)]
    pub editor_soft_wrap: bool,
    /// Show line numbers in the editor gutter.
    #[serde(default = "default_true")]
    pub editor_line_numbers: bool,
    /// Show indentation guides in the editor.
    #[serde(default = "default_true")]
    pub editor_indent_guides: bool,
}

fn default_editor_font_size() -> f32 {
    13.0
}
fn default_editor_tab_size() -> u32 {
    4
}

fn default_runners() -> Vec<Runner> {
    Runner::defaults()
}

fn default_snippets() -> Vec<Snippet> {
    Snippet::defaults()
}

fn default_passthrough_keys() -> Vec<String> {
    // Ctrl+P is handled directly (palette only when no terminal is focused), so the
    // general pass-through list starts empty; add other conflicting chords as needed.
    Vec::new()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_use_tmux: false,
            default_use_worktree: false,
            notifications_enabled: true,
            browser_enabled: true,
            close_on_exit: true,
            confirm_close_terminal: true,
            confirm_close_editor: false,
            confirm_close_diff: false,
            accepted_terms_version: 0,
            preset_seed_version: PRESET_SEED_VERSION,
            default_preset: String::new(),
            presets: AgentPreset::defaults(),
            theme: String::new(),
            theme_mode: String::new(),
            language: None,
            font_family: String::new(),
            font_size: 14.0,
            ui_font_size: 16.0,
            zoom: 1.0,
            pane_border: "subtle".to_string(),
            terminal_mouse: "copy_paste".to_string(),
            diff_split_view: false,
            minimize_to_tray: false,
            dev_console_enabled: false,
            keybindings: Vec::new(),
            runners: Runner::defaults(),
            snippets: Snippet::defaults(),
            loops: Vec::new(),
            terminal_passthrough_keys: default_passthrough_keys(),
            remotes: Vec::new(),
            identities: Vec::new(),
            editor_font_family: String::new(),
            editor_font_size: default_editor_font_size(),
            editor_tab_size: default_editor_tab_size(),
            editor_soft_wrap: false,
            editor_line_numbers: true,
            editor_indent_guides: true,
        }
    }
}

/// Current version of the bundled built-in preset + runner set. Bump when adding
/// new built-ins so existing users get them merged once (see
/// [`Settings::seed_builtin_presets`]).
/// v4: added the Amp (ampcode) preset.
/// v5: added the Grok (x.ai) preset.
/// v6: opencode default runner startup delay.
/// v10: added the "Ollama Code" preset (`ollama launch <agent> --model <model>`).
pub const PRESET_SEED_VERSION: u32 = 11;

/// Current version of the Terms of Service / Privacy notice. Bump this when the
/// terms change so users are asked to accept again on next launch (see
/// [`Settings::accepted_terms_version`]).
pub const CURRENT_TERMS_VERSION: u32 = 1;

impl Settings {
    /// Merge in any built-in presets + runners the user is missing (matched by
    /// name), once per seed version, so new built-ins reach existing configs
    /// without resurrecting ones the user deleted. Returns whether the settings
    /// changed (and should be persisted).
    pub fn seed_builtin_presets(&mut self) -> bool {
        if self.preset_seed_version >= PRESET_SEED_VERSION {
            return false;
        }
        for builtin in AgentPreset::defaults() {
            if !self.presets.iter().any(|p| p.name == builtin.name) {
                self.presets.push(builtin);
            }
        }
        for builtin in Runner::defaults() {
            if !self.runners.iter().any(|r| r.name == builtin.name) {
                self.runners.push(builtin);
            }
        }
        // Adopt the built-in runner startup delay for matching presets that don't
        // have one set. The field is new (0 = unset), so this gives e.g. existing
        // opencode users the sensible default once, without touching their edits.
        for builtin in AgentPreset::defaults() {
            if builtin.startup_delay_ms > 0 {
                for p in self.presets.iter_mut() {
                    if p.name == builtin.name && p.startup_delay_ms == 0 {
                        p.startup_delay_ms = builtin.startup_delay_ms;
                    }
                }
            }
        }
        // Give matching built-in presets the session-resume flags (new fields)
        // without overwriting user edits, so e.g. an existing Claude preset gains
        // resume-on-restart after upgrading.
        for builtin in AgentPreset::defaults() {
            for p in self.presets.iter_mut() {
                if p.name == builtin.name {
                    if p.session_id_flag.is_none() {
                        p.session_id_flag = builtin.session_id_flag.clone();
                    }
                    if p.resume_flag.is_none() {
                        p.resume_flag = builtin.resume_flag.clone();
                    }
                }
            }
        }
        // Adopt the built-in status markers (e.g. Claude's "esc to interrupt"
        // working marker) for matching presets that have none, so existing configs
        // get reliable status detection without overwriting a user's own markers.
        for builtin in AgentPreset::defaults() {
            if builtin.working_markers.is_empty() {
                continue;
            }
            for p in self.presets.iter_mut() {
                if p.name == builtin.name && p.working_markers.is_empty() {
                    p.working_markers = builtin.working_markers.clone();
                }
            }
        }
        self.preset_seed_version = PRESET_SEED_VERSION;
        true
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn seed_merges_missing_builtins_once() {
        // Simulate an old config: only the original three presets, no runners.
        let mut s = Settings {
            preset_seed_version: 0,
            presets: vec![
                AgentPreset::shell(),
                AgentPreset::claude(),
                AgentPreset::opencode(),
            ],
            runners: vec![],
            ..Settings::default()
        };
        assert!(s.seed_builtin_presets());
        assert!(s.presets.iter().any(|p| p.name == "Hermes"));
        assert!(s.presets.iter().any(|p| p.name == "Ollama"));
        assert!(s.presets.iter().any(|p| p.name == "Pi"));
        // Built-in runners are seeded too.
        assert!(s.runners.iter().any(|r| r.name == "Review"));
        assert!(s.runners.iter().any(|r| r.name == "Security Review"));
        assert_eq!(s.preset_seed_version, PRESET_SEED_VERSION);
        // Idempotent: a second pass does nothing.
        let counts = (s.presets.len(), s.runners.len());
        assert!(!s.seed_builtin_presets());
        assert_eq!((s.presets.len(), s.runners.len()), counts);
    }

    #[test]
    fn snippets_default_when_absent_but_respect_empty() {
        // An old config with no `snippets` key falls back to the defaults.
        let s: Settings = serde_json::from_str("{}").expect("parse");
        assert!(s.snippets.iter().any(|sn| sn.name == "Continue"));
        // A user who deleted every snippet keeps an empty list (serde default only
        // fills an *absent* field, not an explicitly empty one).
        let s2: Settings = serde_json::from_str(r#"{"snippets": []}"#).expect("parse");
        assert!(s2.snippets.is_empty());
    }

    #[test]
    fn seed_does_not_resurrect_deleted_builtins() {
        // User on the current version who deleted Ollama: it stays gone.
        let mut s = Settings {
            preset_seed_version: PRESET_SEED_VERSION,
            presets: vec![AgentPreset::shell()],
            ..Settings::default()
        };
        assert!(!s.seed_builtin_presets());
        assert!(!s.presets.iter().any(|p| p.name == "Ollama"));
    }

    #[test]
    fn seed_adopts_working_markers_when_missing() {
        // An old Claude preset with no status markers gains the built-in one.
        let mut bare = AgentPreset::claude();
        bare.working_markers.clear();
        let mut s = Settings {
            preset_seed_version: 0,
            presets: vec![bare],
            ..Settings::default()
        };
        assert!(s.seed_builtin_presets());
        let claude = s.presets.iter().find(|p| p.name == "Claude").unwrap();
        assert_eq!(claude.working_markers, vec!["esc to interrupt".to_string()]);

        // A user's own markers are never overwritten.
        let mut custom = AgentPreset::claude();
        custom.working_markers = vec!["mine".to_string()];
        let mut s2 = Settings {
            preset_seed_version: 0,
            presets: vec![custom],
            ..Settings::default()
        };
        s2.seed_builtin_presets();
        let claude2 = s2.presets.iter().find(|p| p.name == "Claude").unwrap();
        assert_eq!(claude2.working_markers, vec!["mine".to_string()]);
    }

    #[test]
    fn loop_interval_due_only_after_armed_interval() {
        let s = LoopSchedule::EveryMinutes { minutes: 30 };
        // Unarmed (never run) does not fire on the first check.
        assert!(!s.is_due(None, 10_000, 0));
        // Armed: not due before the interval, due after.
        assert!(!s.is_due(Some(10_000), 10_000 + 29 * 60, 0));
        assert!(s.is_due(Some(10_000), 10_000 + 30 * 60, 0));
        // Hours variant.
        let h = LoopSchedule::EveryHours { hours: 2 };
        assert!(!h.is_due(Some(0), 3600, 0));
        assert!(h.is_due(Some(0), 2 * 3600, 0));
    }

    #[test]
    fn loop_daily_at_fires_once_per_day_after_target() {
        let s = LoopSchedule::DailyAt { hour: 9, minute: 0 };
        let target_tod: u32 = 9 * 3600; // 09:00 in seconds since midnight
        let tt = target_tod as u64;
        let midnight = 1_000_000u64; // arbitrary local-midnight unix second
        // Before the target time of day: never due.
        assert!(!s.is_due(None, midnight + 8 * 3600, 8 * 3600));
        // At/after target with no prior run today: due.
        assert!(s.is_due(None, midnight + tt, target_tod));
        // Already ran at today's target: not due again today.
        let ran = midnight + tt;
        assert!(!s.is_due(Some(ran), midnight + tt + 300, target_tod + 300));
        // A run from before today's target (e.g. yesterday) → due (catch-up).
        assert!(s.is_due(Some(midnight - 3600), midnight + tt, target_tod));
    }

    #[test]
    fn loop_serde_round_trips_with_defaults() {
        let pid = Uuid::new_v4();
        let lp = Loop::new("Nightly", pid);
        let json = serde_json::to_string(&lp).unwrap();
        let back: Loop = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "Nightly");
        assert_eq!(back.project_id, pid);
        assert!(back.enabled);
        assert_eq!(back.post_run, PostRunAction::Leave);
        // `loops` defaults empty on an old config that lacks the field.
        assert!(Settings::default().loops.is_empty());
    }

    #[test]
    fn workspaces_index_reads_legacy_profiles_key() {
        // The pre-rename index used a "profiles" key; the serde alias keeps
        // existing setups loadable so a user's workspace list isn't lost.
        let json = r#"{"profiles":[{"id":"00000000-0000-0000-0000-000000000001","name":"Default"}],"current":"00000000-0000-0000-0000-000000000001"}"#;
        let idx: WorkspacesIndex = serde_json::from_str(json).unwrap();
        assert_eq!(idx.workspaces.len(), 1);
        assert_eq!(idx.workspaces[0].name, "Default");
    }

    #[test]
    fn settings_terminal_mouse_default_and_round_trip() {
        assert_eq!(Settings::default().terminal_mouse, "copy_paste");
        // An old config missing the key still loads, defaulting to copy_paste.
        let mut v = serde_json::to_value(Settings::default()).unwrap();
        v.as_object_mut().unwrap().remove("terminal_mouse");
        let s: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(s.terminal_mouse, "copy_paste");
        // A non-default value round-trips.
        let s2 = Settings {
            terminal_mouse: "copy_on_select".to_string(),
            ..Settings::default()
        };
        let json = serde_json::to_string(&s2).unwrap();
        assert_eq!(
            serde_json::from_str::<Settings>(&json)
                .unwrap()
                .terminal_mouse,
            "copy_on_select"
        );
    }
}

#[cfg(test)]
mod remote_layout_tests {
    use super::*;

    fn remote_project(root: &str) -> Project {
        let mut p = Project::new("proj", "/local/proj");
        p.remote = Some(RemoteRef {
            host_id: Uuid::new_v4(),
            remote_root: root.into(),
        });
        p
    }

    fn worktree(pid: Uuid, name: &str, color: u8) -> Worktree {
        Worktree {
            id: Uuid::new_v4(),
            project_id: pid,
            name: name.into(),
            path: format!("/srv/app/.wt/{name}").into(),
            branch: format!("muxel/{name}"),
            color,
            detached: false,
        }
    }

    #[test]
    fn capture_selects_only_referenced_instances_and_worktrees() {
        let mut ws = Workspace::default();
        let mut proj = remote_project("/srv/app");
        let pid = proj.id;

        let i1 = Instance::shell(pid);
        let mut i2 = Instance::shell(pid);
        let wt = worktree(pid, "swift-pine", 0);
        i2.worktree_id = Some(wt.id);
        let (id1, id2) = (i1.id, i2.id);

        // An instance + worktree not referenced by the layout — capture must drop them.
        let orphan = Instance::shell(pid);
        let wt2 = worktree(pid, "lone-reef", 1);

        proj.layout = Some(PaneNode::Leaf(LeafData {
            tabs: vec![id1, id2],
            active: 0,
        }));
        ws.worktrees = vec![wt.clone(), wt2];
        ws.instances = vec![i1, i2, orphan];
        ws.projects = vec![proj.clone()];

        let doc = RemoteLayout::capture(&proj, &ws, 123);
        assert_eq!(doc.version, REMOTE_LAYOUT_VERSION);
        assert_eq!(doc.remote_root, "/srv/app");
        // Falls back to `now` when the project was never stamped.
        assert_eq!(doc.updated_at, 123);

        let mut want = vec![id1, id2];
        want.sort();
        assert_eq!(doc.instances.iter().map(|i| i.id).collect::<Vec<_>>(), want);
        assert_eq!(doc.worktrees.len(), 1);
        assert_eq!(doc.worktrees[0].id, wt.id);
    }

    #[test]
    fn updated_at_uses_project_stamp_when_present() {
        let mut ws = Workspace::default();
        let mut proj = remote_project("/srv/app");
        proj.layout_updated_at = Some(999);
        ws.projects = vec![proj.clone()];
        assert_eq!(RemoteLayout::capture(&proj, &ws, 123).updated_at, 999);
    }

    #[test]
    fn capture_uses_local_root_for_local_project() {
        // A local project (no `remote`) is synced so an SSH peer can attach; its
        // identity root is the local path, so the doc validates on load instead of
        // carrying an empty `remote_root`.
        let mut ws = Workspace::default();
        let proj = Project::new("proj", "/local/proj");
        assert!(proj.remote.is_none());
        ws.projects = vec![proj.clone()];
        let doc = RemoteLayout::capture(&proj, &ws, 1);
        assert_eq!(doc.remote_root, "/local/proj");
        // And a doc captured for it parses back only against that same root.
        assert!(RemoteLayout::parse(&doc.to_json(), "/local/proj").is_some());
        assert!(RemoteLayout::parse(&doc.to_json(), "/other").is_none());
    }

    #[test]
    fn content_key_ignores_updated_at_but_tracks_layout() {
        let mut ws = Workspace::default();
        let mut proj = remote_project("/srv/app");
        let i1 = Instance::shell(proj.id);
        let id1 = i1.id;
        proj.layout = Some(PaneNode::Leaf(LeafData {
            tabs: vec![id1],
            active: 0,
        }));
        ws.instances = vec![i1];
        ws.projects = vec![proj.clone()];

        let a = RemoteLayout::capture(&proj, &ws, 1);
        let b = RemoteLayout::capture(&proj, &ws, 9_999);
        assert_eq!(a.content_key(), b.content_key());

        let i2 = Instance::shell(proj.id);
        let id2 = i2.id;
        proj.layout = Some(PaneNode::Leaf(LeafData {
            tabs: vec![id1, id2],
            active: 0,
        }));
        ws.instances.push(i2);
        let c = RemoteLayout::capture(&proj, &ws, 1);
        assert_ne!(a.content_key(), c.content_key());
    }

    #[test]
    fn json_round_trips_and_parse_validates() {
        let mut ws = Workspace::default();
        let mut proj = remote_project("/srv/app");
        let i1 = Instance::shell(proj.id);
        proj.layout = Some(PaneNode::Leaf(LeafData {
            tabs: vec![i1.id],
            active: 0,
        }));
        ws.instances = vec![i1];
        ws.projects = vec![proj.clone()];

        let doc = RemoteLayout::capture(&proj, &ws, 42);
        let json = doc.to_json();

        let back = RemoteLayout::parse(&json, "/srv/app").expect("matching root parses");
        assert_eq!(back.updated_at, 42);
        assert_eq!(back.content_key(), doc.content_key());

        // A doc captured for a different root is ignored.
        assert!(RemoteLayout::parse(&json, "/other").is_none());

        // A future format version is ignored.
        let mut bumped = doc.clone();
        bumped.version = REMOTE_LAYOUT_VERSION + 1;
        assert!(RemoteLayout::parse(&bumped.to_json(), "/srv/app").is_none());

        // Garbage is ignored.
        assert!(RemoteLayout::parse("not json", "/srv/app").is_none());
    }
}

#[cfg(test)]
mod worktree_registry_tests {
    use super::*;

    fn ws_with_project() -> (Workspace, Uuid) {
        let mut ws = Workspace::default();
        let pid = ws.add_project(Project::new("repo", "/tmp/repo"));
        (ws, pid)
    }

    fn wt(pid: Uuid, path: &str, color: u8) -> Worktree {
        Worktree {
            id: Uuid::new_v4(),
            project_id: pid,
            name: "x".into(),
            path: path.into(),
            branch: "muxel/00000000".into(),
            color,
            detached: false,
        }
    }

    #[test]
    fn registry_add_lookup_remove() {
        let (mut ws, pid) = ws_with_project();
        let w = wt(pid, "/tmp/wt/a", 2);
        let wid = w.id;
        ws.add_worktree(w);
        assert_eq!(ws.worktree(wid).map(|w| w.color), Some(2));
        ws.remove_worktree_meta(wid);
        assert!(ws.worktree(wid).is_none());
    }

    #[test]
    fn instances_using_tracks_references() {
        let (mut ws, pid) = ws_with_project();
        let w = wt(pid, "/tmp/wt/a", 0);
        let wid = w.id;
        ws.add_worktree(w);
        let mut a = Instance::shell(pid);
        a.worktree_id = Some(wid);
        let aid = a.id;
        let mut b = Instance::shell(pid);
        b.worktree_id = Some(wid);
        ws.add_instance(a);
        ws.add_instance(b);
        ws.add_instance(Instance::shell(pid)); // no worktree
        assert_eq!(ws.instances_using(wid).len(), 2);
        ws.remove_instance_meta(aid);
        assert_eq!(ws.instances_using(wid).len(), 1);
    }

    #[test]
    fn next_color_picks_lowest_unused_then_wraps() {
        let (mut ws, pid) = ws_with_project();
        for i in 0u8..8 {
            assert_eq!(ws.next_worktree_color(pid), i);
            ws.add_worktree(wt(pid, &format!("/tmp/{i}"), i));
        }
        assert_eq!(ws.next_worktree_color(pid), 0); // all used → wrap
    }

    #[test]
    fn migrate_groups_by_path_and_is_idempotent() {
        let (mut ws, pid) = ws_with_project();
        let mut a = Instance::shell(pid);
        a.worktree_path = Some("/tmp/wt/one".into());
        a.worktree_branch = Some("muxel/aaaaaaaa".into());
        let aid = a.id;
        let mut b = Instance::shell(pid);
        b.worktree_path = Some("/tmp/wt/one".into()); // same path → shares
        let bid = b.id;
        let mut c = Instance::shell(pid);
        c.worktree_path = Some("/tmp/wt/two".into());
        let cid = c.id;
        let d = Instance::shell(pid); // no worktree
        let did = d.id;
        for i in [a, b, c, d] {
            ws.add_instance(i);
        }
        migrate_worktrees(&mut ws);
        assert_eq!(ws.worktrees.len(), 2);
        let wa = ws.instance(aid).unwrap().worktree_id.unwrap();
        let wb = ws.instance(bid).unwrap().worktree_id.unwrap();
        let wc = ws.instance(cid).unwrap().worktree_id.unwrap();
        assert_eq!(wa, wb);
        assert_ne!(wa, wc);
        assert!(ws.instance(did).unwrap().worktree_id.is_none());
        migrate_worktrees(&mut ws); // idempotent
        assert_eq!(ws.worktrees.len(), 2);
    }

    #[test]
    fn worktree_serde_detached_defaults_false() {
        let json = r#"{"id":"00000000-0000-0000-0000-000000000001",
            "project_id":"00000000-0000-0000-0000-000000000002",
            "name":"swift-pine","path":"/tmp/wt","branch":"muxel/00000000","color":3}"#;
        let w: Worktree = serde_json::from_str(json).unwrap();
        assert!(!w.detached);
        assert_eq!(w.color, 3);
    }
}

#[cfg(test)]
mod project_order_tests {
    use super::{Project, Workspace};

    fn ws_with(names: &[&str]) -> Workspace {
        let mut ws = Workspace::default();
        for n in names {
            ws.add_project(Project::new(*n, format!("/tmp/{n}")));
        }
        ws
    }

    fn order(ws: &Workspace) -> Vec<String> {
        ws.projects.iter().map(|p| p.name.clone()).collect()
    }

    #[test]
    fn move_project_swaps_adjacent() {
        let mut ws = ws_with(&["a", "b", "c"]);
        let b = ws.projects[1].id;
        // Down moves one slot back, not to the end.
        assert!(ws.move_project(b, false));
        assert_eq!(order(&ws), ["a", "c", "b"]);
        // Up brings it back.
        assert!(ws.move_project(b, true));
        assert_eq!(order(&ws), ["a", "b", "c"]);
    }

    #[test]
    fn move_project_is_noop_at_the_ends() {
        let mut ws = ws_with(&["a", "b", "c"]);
        let first = ws.projects[0].id;
        let last = ws.projects[2].id;
        assert!(!ws.move_project(first, true), "first can't go up");
        assert!(!ws.move_project(last, false), "last can't go down");
        assert_eq!(order(&ws), ["a", "b", "c"]);
        // Unknown id never moves anything.
        assert!(!ws.move_project(super::Uuid::new_v4(), true));
        assert_eq!(order(&ws), ["a", "b", "c"]);
    }
}

#[cfg(test)]
mod identity_tests {
    use super::{Identity, RemoteHost, SshAuth};
    use std::path::PathBuf;

    fn host_referencing(id: uuid::Uuid) -> RemoteHost {
        let mut h = RemoteHost::new("web", "example.com");
        h.user = "inline".into();
        h.auth = SshAuth::Password;
        h.port = Some(2222);
        h.keepalive_secs = Some(30);
        h.identity_id = Some(id);
        h
    }

    fn key_identity(name: &str) -> Identity {
        let mut i = Identity::new(name);
        i.user = "deploy".into();
        i.auth = SshAuth::Key;
        i.identity_file = Some(PathBuf::from("/keys/id_ed25519"));
        i
    }

    #[test]
    fn effective_overlays_identity_credentials() {
        let id = key_identity("deploy");
        let host = host_referencing(id.id);
        let eff = host.effective(std::slice::from_ref(&id));
        // Credential fields come from the identity.
        assert_eq!(eff.user, "deploy");
        assert_eq!(eff.auth, SshAuth::Key);
        assert_eq!(eff.identity_file, Some(PathBuf::from("/keys/id_ed25519")));
        // Identity + transport fields are preserved.
        assert_eq!(eff.id, host.id);
        assert_eq!(eff.identity_id, Some(id.id));
        assert_eq!(eff.port, Some(2222));
        assert_eq!(eff.keepalive_secs, Some(30));
    }

    #[test]
    fn effective_falls_back_when_identity_missing_or_unset() {
        // References an id that isn't in the list → inline fields kept.
        let host = host_referencing(uuid::Uuid::new_v4());
        let eff = host.effective(&[]);
        assert_eq!(eff.user, "inline");
        assert_eq!(eff.auth, SshAuth::Password);

        // No identity_id → identical clone.
        let mut plain = RemoteHost::new("db", "db.example.com");
        plain.user = "root".into();
        let eff = plain.effective(&[key_identity("x")]);
        assert_eq!(eff.user, "root");
        assert_eq!(eff.identity_id, None);
    }

    #[test]
    fn secret_owner_is_identity_when_present_else_host() {
        let id = key_identity("deploy");
        let host = host_referencing(id.id);
        // Identity found → its id owns the secret (shared across hosts).
        assert_eq!(host.secret_owner(std::slice::from_ref(&id)), id.id);
        // Identity missing → the host owns its own secret.
        assert_eq!(host.secret_owner(&[]), host.id);
        // No reference → host owns it.
        let plain = RemoteHost::new("db", "db.example.com");
        assert_eq!(plain.secret_owner(&[id]), plain.id);
    }
}
