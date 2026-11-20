//! muxel Cloud: the pure side of settings sync, team tasks and activity.
//!
//! Everything here is data and decisions — no I/O, no crypto. The wire protocol
//! is specified in `docs/cloud-sync-protocol.md`; encryption and HTTP live in the
//! `muxel-cloud` crate, and the app wires the two together.

pub mod activity;
pub mod hlc;
pub mod items;
pub mod merge3;
pub mod project_key;
pub mod roles;
pub mod state;
pub mod tasks;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The production service. Overridden by `MUXEL_CLOUD_URL` (development) or
/// [`CloudSettings::server_url`].
pub const DEFAULT_SERVER_URL: &str = "https://cloud.muxel.sh";

/// Environment variable that points muxel at another server (a local
/// development backend, or a self-hosted implementation of the protocol).
pub const SERVER_URL_ENV: &str = "MUXEL_CLOUD_URL";

/// Which way one category of data flows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Not synced.
    #[default]
    Off,
    /// Upload local changes; never apply remote ones.
    Push,
    /// Apply remote changes; never upload local ones.
    Pull,
    /// Both directions.
    TwoWay,
}

impl SyncMode {
    pub fn pushes(self) -> bool {
        matches!(self, SyncMode::Push | SyncMode::TwoWay)
    }
    pub fn pulls(self) -> bool {
        matches!(self, SyncMode::Pull | SyncMode::TwoWay)
    }
    pub const ALL: [SyncMode; 4] = [
        SyncMode::Off,
        SyncMode::Push,
        SyncMode::Pull,
        SyncMode::TwoWay,
    ];
}

/// Per-category sync directions for the personal scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCategories {
    #[serde(default = "two_way")]
    pub runners: SyncMode,
    #[serde(default = "two_way")]
    pub loops: SyncMode,
    #[serde(default = "two_way")]
    pub snippets: SyncMode,
    /// SSH hosts and login identities (never passwords or key material).
    #[serde(default = "two_way")]
    pub hosts: SyncMode,
    #[serde(default = "two_way")]
    pub appearance: SyncMode,
    /// Keybindings are separate from appearance: they differ between
    /// keyboards and operating systems more often than themes do.
    #[serde(default)]
    pub keybindings: SyncMode,
    /// Personal tasks (team tasks follow the workspace's team binding).
    #[serde(default = "two_way")]
    pub tasks: SyncMode,
}

fn two_way() -> SyncMode {
    SyncMode::TwoWay
}

impl Default for SyncCategories {
    fn default() -> Self {
        Self {
            runners: SyncMode::TwoWay,
            loops: SyncMode::TwoWay,
            snippets: SyncMode::TwoWay,
            hosts: SyncMode::TwoWay,
            appearance: SyncMode::TwoWay,
            keybindings: SyncMode::Off,
            tasks: SyncMode::TwoWay,
        }
    }
}

impl SyncCategories {
    /// The direction configured for a personal-scope collection.
    pub fn mode(&self, collection: items::Collection) -> SyncMode {
        use items::Collection as C;
        match collection {
            C::Runners => self.runners,
            C::Loops => self.loops,
            C::Snippets => self.snippets,
            C::Hosts | C::Identities => self.hosts,
            C::Appearance => self.appearance,
            C::Keybindings => self.keybindings,
            C::Tasks => self.tasks,
        }
    }
}

/// What a user's activity heartbeat may include. Everything beyond the agent's
/// status is opt-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityPrivacy {
    /// Program name and working/blocked/idle per agent.
    #[serde(default = "yes")]
    pub agent_status: bool,
    /// Branch / worktree name of each agent.
    #[serde(default = "yes")]
    pub branch: bool,
    /// Title of the task an agent is working on.
    #[serde(default = "yes")]
    pub task_titles: bool,
    /// A one-line summary of the agent's last prompt.
    #[serde(default)]
    pub prompts: bool,
}

fn yes() -> bool {
    true
}

impl Default for ActivityPrivacy {
    fn default() -> Self {
        Self {
            agent_status: true,
            branch: true,
            task_titles: true,
            prompts: false,
        }
    }
}

/// Global cloud preferences, stored in `config.toml` as `[cloud]`. Credentials
/// (device token, account unlock key) live in the OS keychain, never here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloudSettings {
    /// Master switch: off = muxel never contacts the service.
    #[serde(default)]
    pub enabled: bool,
    /// Signed-in account's email, for display only.
    #[serde(default)]
    pub account_label: String,
    /// Server base URL; empty = [`DEFAULT_SERVER_URL`].
    #[serde(default)]
    pub server_url: String,
    /// Seconds between sync cycles (clamped to 15..=3600).
    #[serde(default = "default_interval")]
    pub interval_secs: u32,
    #[serde(default)]
    pub categories: SyncCategories,
    #[serde(default)]
    pub privacy: ActivityPrivacy,
}

fn default_interval() -> u32 {
    15
}

impl Default for CloudSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            account_label: String::new(),
            server_url: String::new(),
            interval_secs: default_interval(),
            categories: SyncCategories::default(),
            privacy: ActivityPrivacy::default(),
        }
    }
}

impl CloudSettings {
    /// The server to talk to: `env_override` (from [`SERVER_URL_ENV`]) wins, then
    /// the configured URL, then the default. Trailing slashes are dropped.
    pub fn server(&self, env_override: Option<&str>) -> String {
        let pick = env_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or(Some(self.server_url.trim()).filter(|s| !s.is_empty()))
            .unwrap_or(DEFAULT_SERVER_URL);
        pick.trim_end_matches('/').to_string()
    }

    pub fn interval(&self) -> u32 {
        self.interval_secs.clamp(15, 3600)
    }
}

/// Three-way override used per project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tristate {
    #[default]
    Inherit,
    On,
    Off,
}

impl Tristate {
    pub fn resolve(self, inherited: bool) -> bool {
        match self {
            Tristate::Inherit => inherited,
            Tristate::On => true,
            Tristate::Off => false,
        }
    }
}

/// A workspace's cloud binding. Stored on `Workspace`; `None` there = private.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCloud {
    /// The team this workspace works for. Its shared runners, snippets, loops
    /// and hosts appear only in workspaces bound to it.
    pub team_id: Uuid,
    /// Team display name (cached for the UI; the server is authoritative).
    #[serde(default)]
    pub team_name: String,
    /// Publish this workspace's agent activity to the team. Opt-in.
    #[serde(default)]
    pub share_activity: bool,
    /// Show and sync the team's tasks here.
    #[serde(default = "yes")]
    pub tasks_enabled: bool,
}

/// A project's overrides of its workspace's binding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCloud {
    #[serde(default)]
    pub share_activity: Tristate,
    #[serde(default)]
    pub tasks: Tristate,
}

impl ProjectCloud {
    pub fn is_default(&self) -> bool {
        *self == ProjectCloud::default()
    }

    /// Whether this project's activity is published, given its workspace binding.
    pub fn shares_activity(&self, workspace: Option<&WorkspaceCloud>) -> bool {
        match workspace {
            None => false,
            Some(w) => self.share_activity.resolve(w.share_activity),
        }
    }

    /// Whether this project shows its team's tasks.
    pub fn shows_team_tasks(&self, workspace: Option<&WorkspaceCloud>) -> bool {
        match workspace {
            None => false,
            Some(w) => self.tasks.resolve(w.tasks_enabled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CloudSettings, ProjectCloud, SyncMode, Tristate, WorkspaceCloud};
    use uuid::Uuid;

    #[test]
    fn server_url_precedence() {
        let mut s = CloudSettings::default();
        assert_eq!(s.server(None), "https://cloud.muxel.sh");
        s.server_url = "https://self.example/".into();
        assert_eq!(s.server(None), "https://self.example");
        assert_eq!(
            s.server(Some("http://localhost:8000/")),
            "http://localhost:8000"
        );
        assert_eq!(s.server(Some("  ")), "https://self.example");
    }

    #[test]
    fn modes() {
        assert!(SyncMode::TwoWay.pushes() && SyncMode::TwoWay.pulls());
        assert!(SyncMode::Push.pushes() && !SyncMode::Push.pulls());
        assert!(!SyncMode::Pull.pushes() && SyncMode::Pull.pulls());
        assert!(!SyncMode::Off.pushes() && !SyncMode::Off.pulls());
    }

    #[test]
    fn project_overrides_resolve_against_workspace() {
        let w = WorkspaceCloud {
            team_id: Uuid::nil(),
            team_name: String::new(),
            share_activity: true,
            tasks_enabled: false,
        };
        let p = ProjectCloud::default();
        assert!(p.shares_activity(Some(&w)));
        assert!(!p.shows_team_tasks(Some(&w)));
        assert!(!p.shares_activity(None), "private workspace never shares");
        let p = ProjectCloud {
            share_activity: Tristate::Off,
            tasks: Tristate::On,
        };
        assert!(!p.shares_activity(Some(&w)));
        assert!(p.shows_team_tasks(Some(&w)));
    }

    #[test]
    fn old_config_without_cloud_loads_defaults() {
        let s: CloudSettings = toml::from_str("").unwrap();
        assert_eq!(s, CloudSettings::default());
        let s: CloudSettings =
            toml::from_str("enabled = true\n[categories]\nloops = \"pull\"").unwrap();
        assert!(s.enabled);
        assert_eq!(s.categories.loops, SyncMode::Pull);
        assert_eq!(s.categories.runners, SyncMode::TwoWay);
    }
}
