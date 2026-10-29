//! muxel-core — the pure domain model for muxel: projects, agent instances, and
//! the pane layout tree. No UI, no I/O; everything here is serializable and
//! unit-tested so the GPUI app and persistence layer can build on it.

pub mod accomplishments;
mod agent;
mod appimage;
pub mod audio;
pub mod autopilot;
pub mod control;
pub mod diff;
pub mod geometry;
mod gui_path;
pub mod import;
pub mod isolated_root;
pub mod library;
pub mod locale;
pub mod memory;
pub mod muse;
pub mod outage;
mod pane;
pub mod path_paste;
pub mod readaloud;
pub mod recoverable_close;
pub mod remote_ops;
pub mod session_discovery;
mod shell;
pub mod ssh;
pub mod startup_flags;
pub mod stt;
pub mod tmux;
pub mod tmux_install;
pub mod tts;
pub mod url;
pub mod winshell;
pub mod worktree;

pub use agent::{
    ACCOMPLISHMENTS_FILE, AgentPreset, ClaudeSession, ClaudeSessionRequest, CodexHintMiss,
    CodexResumePath, CodexSession, CodexSessionNames, EnvVar, InjectionMode, MEMORY_DIR,
    MEMORY_FILE, PresetKind, ResolvedLaunch, accomplishments_header, accomplishments_instruction,
    append_agent_instruction, claude_session_cwd, claude_session_cwd_with_len, claude_session_path,
    codex_developer_instructions_override, codex_latest_session_id, codex_latest_sessions,
    codex_resume_path, codex_rollout_by_walk, codex_session_from_hint,
    codex_session_hint_from_title, codex_session_id_from_hint, codex_session_names,
    codex_terminal_title_override, discover_claude_sessions, file_link_instruction,
    find_claude_session_path, is_known_agent_program, launches_known_agent, memory_header,
    memory_instruction, memory_reference, muxel_file_reference, resolve_launch,
    resolve_launch_for_session, session_resume_args, stored_claude_session_path,
};
pub use appimage::{foreign_muxel_appimage_mounts, own_appimage};
pub use diff::{SplitRow, split_diff};
pub use gui_path::{augmented_linux_path, augmented_macos_path};
pub use pane::{
    FocusDir, LeafData, PaneNode, SplitDirection, add_tab, add_tab_at, focus_in_direction,
    move_into_split, move_into_tabs, move_pane_beside, move_tab_to, place_peer_panes, remove,
    set_active_tab, set_split_sizes, set_tab_order, split, split_beside, swap_instances,
    swap_panes,
};
pub use readaloud::{ReadAloudAuto, ReadAloudScope};
pub use shell::{join_words, split_words};
pub use stt::SttEngine;
pub use tts::TtsEngine;
pub use url::{normalize_url, same_resource_url};
pub use worktree::Worktree;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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

/// Persisted lifecycle timestamps for an agent pane.
///
/// These are Unix milliseconds. The live terminal owns status detection; the core
/// model owns the durable lifecycle and notification-attendance facts needed to
/// retain completion and derive compact age labels after a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivity {
    #[serde(default)]
    pub work_started_at: Option<i64>,
    #[serde(default)]
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub blocked_at: Option<i64>,
    #[serde(default)]
    pub last_attended_at: Option<i64>,
    /// Latest lifecycle state, independent from whether its notification was read.
    /// Older workspaces infer it from their existing transition timestamps.
    #[serde(default)]
    pub last_state: Option<AgentActivityState>,
}

/// Provider-neutral lifecycle state used by the persisted activity reducer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivityState {
    Working,
    Idle,
    Blocked,
    Done,
}

impl AgentActivity {
    fn last_lifecycle_at(&self) -> Option<i64> {
        [self.work_started_at, self.completed_at, self.blocked_at]
            .into_iter()
            .flatten()
            .max()
    }

    pub fn current_state(&self) -> Option<AgentActivityState> {
        self.last_state.or_else(|| {
            [
                self.work_started_at
                    .map(|at| (at, 0, AgentActivityState::Working)),
                self.completed_at
                    .map(|at| (at, 1, AgentActivityState::Done)),
                self.blocked_at
                    .map(|at| (at, 2, AgentActivityState::Blocked)),
            ]
            .into_iter()
            .flatten()
            .max_by_key(|(at, priority, _)| (*at, *priority))
            .map(|(_, _, state)| state)
        })
    }

    /// Whether a completed turn is newer than the last time the pane was
    /// attended. A missing attendance timestamp means the completion is unseen.
    pub fn has_unattended_completion(&self) -> bool {
        self.current_state() == Some(AgentActivityState::Done)
            && self
                .completed_at
                .is_some_and(|done| self.last_attended_at.is_none_or(|seen| done > seen))
    }

    /// Apply one displayed-status transition. Returns whether durable state
    /// changed, so callers can avoid rewriting the workspace on idle ticks.
    pub fn observe(
        &mut self,
        previous: Option<AgentActivityState>,
        status: AgentActivityState,
        now: i64,
    ) -> bool {
        let now = now.max(0);
        let before = self.clone();
        let previous_persisted = self.current_state();
        match status {
            AgentActivityState::Working => {
                if previous != Some(AgentActivityState::Working) {
                    self.work_started_at = Some(now);
                }
                self.blocked_at = None;
            }
            AgentActivityState::Blocked => {
                if previous != Some(AgentActivityState::Blocked)
                    && !(previous.is_none() && self.blocked_at.is_some())
                {
                    self.blocked_at = Some(now);
                }
            }
            AgentActivityState::Done => {
                // A restored completion already has its correct timestamp.
                // Its first live sample is not a new event, even when focus marked it
                // attended while the terminal was loading.
                if previous != Some(AgentActivityState::Done)
                    && !(previous.is_none() && previous_persisted == Some(AgentActivityState::Done))
                {
                    self.completed_at = Some(now);
                }
                self.blocked_at = None;
            }
            AgentActivityState::Idle => {
                if previous == Some(AgentActivityState::Blocked) {
                    self.blocked_at = None;
                }
            }
        }
        if self.last_state.is_some() || previous_persisted != Some(status) {
            self.last_state = Some(status);
        }
        *self != before
    }

    /// Record deliberate attendance. This clears unattended completion by
    /// ordering rather than by maintaining a second persisted boolean.
    pub fn attend(&mut self, now: i64) -> bool {
        let now = now.max(0);
        if self.last_attended_at == Some(now) {
            return false;
        }
        self.last_attended_at = Some(now);
        true
    }
}

/// Compact sidebar copy derived from lifecycle state and durable timestamps.
pub fn agent_activity_label(
    status: AgentActivityState,
    activity: &AgentActivity,
    now: i64,
) -> String {
    match status {
        AgentActivityState::Working => "working".to_string(),
        AgentActivityState::Done => state_with_age("done", activity.completed_at, now),
        AgentActivityState::Blocked => state_with_age("blocked", activity.blocked_at, now),
        AgentActivityState::Idle => {
            let Some(age) = age_millis(activity.last_lifecycle_at(), now) else {
                return "idle".to_string();
            };
            if age < MINUTE_MS {
                "idle".to_string()
            } else if age < HOUR_MS {
                format!("idle · {}m", age / MINUTE_MS)
            } else if age < 6 * HOUR_MS {
                format!("idle · {}h", age / HOUR_MS)
            } else if age < 3 * DAY_MS {
                "idle".to_string()
            } else {
                format!("stale · {}d", age / DAY_MS)
            }
        }
    }
}

fn state_with_age(state: &str, at: Option<i64>, now: i64) -> String {
    let Some(age) = age_millis(at, now) else {
        return state.to_string();
    };
    if age < MINUTE_MS {
        state.to_string()
    } else if age < HOUR_MS {
        format!("{state} · {}m", age / MINUTE_MS)
    } else if age < DAY_MS {
        format!("{state} · {}h", age / HOUR_MS)
    } else {
        format!("{state} · {}d", age / DAY_MS)
    }
}

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;

fn age_millis(at: Option<i64>, now: i64) -> Option<i64> {
    at.map(|at| now.saturating_sub(at).max(0))
}

#[cfg(test)]
mod agent_activity_tests {
    use super::{
        AgentActivity, AgentActivityState, DAY_MS, HOUR_MS, Instance, MINUTE_MS,
        agent_activity_label,
    };

    #[test]
    fn old_instance_json_defaults_activity() {
        let mut value = serde_json::to_value(Instance::shell(uuid::Uuid::new_v4())).unwrap();
        value.as_object_mut().unwrap().remove("activity");
        let restored: Instance = serde_json::from_value(value).unwrap();
        assert_eq!(restored.activity, AgentActivity::default());
    }

    #[test]
    fn old_activity_json_infers_current_state_from_timestamps() {
        let activity: AgentActivity = serde_json::from_value(serde_json::json!({
            "work_started_at": 100,
            "completed_at": 200,
            "blocked_at": null,
            "last_attended_at": 300
        }))
        .unwrap();

        assert_eq!(activity.last_state, None);
        assert_eq!(activity.current_state(), Some(AgentActivityState::Done));
        assert!(!activity.has_unattended_completion());
    }

    #[test]
    fn completion_remains_unattended_until_attendance() {
        let mut activity = AgentActivity::default();
        assert!(activity.observe(None, AgentActivityState::Working, 100_000));
        assert!(activity.observe(
            Some(AgentActivityState::Working),
            AgentActivityState::Done,
            200_000
        ));
        assert!(activity.has_unattended_completion());
        assert!(!activity.observe(None, AgentActivityState::Done, 300_000));
        assert_eq!(activity.completed_at, Some(200_000));

        assert!(activity.attend(400_000));
        assert!(!activity.has_unattended_completion());
        assert_eq!(activity.current_state(), Some(AgentActivityState::Done));
        let restored: AgentActivity =
            serde_json::from_value(serde_json::to_value(&activity).unwrap()).unwrap();
        assert_eq!(restored.current_state(), Some(AgentActivityState::Done));
        assert!(!restored.has_unattended_completion());
        assert_eq!(
            agent_activity_label(AgentActivityState::Idle, &activity, 460_000),
            "idle · 4m"
        );
    }

    #[test]
    fn new_work_supersedes_an_unattended_completion() {
        let mut activity = AgentActivity::default();
        assert!(activity.observe(None, AgentActivityState::Working, 50));
        assert!(activity.observe(
            Some(AgentActivityState::Working),
            AgentActivityState::Done,
            60
        ));
        assert!(activity.has_unattended_completion());
        assert!(activity.observe(
            Some(AgentActivityState::Done),
            AgentActivityState::Working,
            70
        ));
        assert_eq!(activity.current_state(), Some(AgentActivityState::Working));
        assert!(!activity.has_unattended_completion());
        assert!(activity.observe(
            Some(AgentActivityState::Working),
            AgentActivityState::Done,
            80
        ));
        assert_eq!(activity.completed_at, Some(80));
        assert_eq!(activity.current_state(), Some(AgentActivityState::Done));
        assert!(activity.has_unattended_completion());
    }

    #[test]
    fn first_runtime_done_after_restored_work_gets_a_new_timestamp() {
        let mut activity = AgentActivity {
            work_started_at: Some(200),
            completed_at: Some(100),
            last_state: Some(AgentActivityState::Working),
            ..AgentActivity::default()
        };

        assert!(activity.observe(None, AgentActivityState::Done, 300));
        assert_eq!(activity.completed_at, Some(300));
        assert_eq!(activity.current_state(), Some(AgentActivityState::Done));
    }

    #[test]
    fn completion_one_millisecond_after_attendance_is_unseen() {
        let mut activity = AgentActivity {
            last_attended_at: Some(100_000),
            ..AgentActivity::default()
        };
        assert!(activity.observe(
            Some(AgentActivityState::Working),
            AgentActivityState::Done,
            100_001
        ));
        assert!(activity.has_unattended_completion());
    }

    #[test]
    fn completion_attended_at_the_transition_does_not_survive_as_unseen() {
        let mut activity = AgentActivity::default();
        assert!(activity.observe(None, AgentActivityState::Working, 100));
        assert!(activity.observe(
            Some(AgentActivityState::Working),
            AgentActivityState::Done,
            200
        ));
        assert!(activity.attend(200));
        assert!(!activity.has_unattended_completion());
    }

    #[test]
    fn attended_restored_completion_keeps_its_age_on_first_live_sample() {
        let mut activity = AgentActivity {
            completed_at: Some(0),
            ..AgentActivity::default()
        };
        assert!(activity.attend(2 * HOUR_MS));
        assert!(!activity.has_unattended_completion());
        assert!(!activity.observe(None, AgentActivityState::Done, 2 * HOUR_MS));
        assert_eq!(activity.completed_at, Some(0));
        assert_eq!(
            agent_activity_label(AgentActivityState::Done, &activity, 2 * HOUR_MS),
            "done · 2h"
        );
    }

    #[test]
    fn restored_blocked_timestamp_is_not_replaced_by_first_live_sample() {
        let mut activity = AgentActivity {
            blocked_at: Some(100),
            ..AgentActivity::default()
        };
        assert!(!activity.observe(None, AgentActivityState::Blocked, 500));
        assert_eq!(activity.blocked_at, Some(100));
    }

    #[test]
    fn label_boundaries_keep_the_middle_quiet() {
        let activity = AgentActivity {
            work_started_at: Some(0),
            last_attended_at: Some(58 * MINUTE_MS),
            ..AgentActivity::default()
        };
        assert_eq!(
            agent_activity_label(AgentActivityState::Idle, &activity, 59 * MINUTE_MS),
            "idle · 59m"
        );
        assert_eq!(
            agent_activity_label(AgentActivityState::Idle, &activity, HOUR_MS),
            "idle · 1h"
        );
        assert_eq!(
            agent_activity_label(AgentActivityState::Idle, &activity, 6 * HOUR_MS),
            "idle"
        );
        assert_eq!(
            agent_activity_label(AgentActivityState::Idle, &activity, 3 * DAY_MS),
            "stale · 3d"
        );
    }

    #[test]
    fn attendance_does_not_reset_idle_age() {
        let mut activity = AgentActivity {
            completed_at: Some(0),
            ..AgentActivity::default()
        };
        assert!(activity.attend(4 * DAY_MS));
        assert_eq!(
            agent_activity_label(AgentActivityState::Idle, &activity, 4 * DAY_MS),
            "stale · 4d"
        );
    }

    #[test]
    fn done_and_blocked_age_without_expiring() {
        let activity = AgentActivity {
            completed_at: Some(100_000),
            blocked_at: Some(200_000),
            ..AgentActivity::default()
        };
        let now = 20 * DAY_MS;
        assert_eq!(
            agent_activity_label(AgentActivityState::Done, &activity, now),
            "done · 19d"
        );
        assert_eq!(
            agent_activity_label(AgentActivityState::Blocked, &activity, now),
            "blocked · 19d"
        );
    }

    #[test]
    fn future_timestamps_clamp_to_now() {
        let activity = AgentActivity {
            work_started_at: Some(500_000),
            ..AgentActivity::default()
        };
        assert_eq!(
            agent_activity_label(AgentActivityState::Idle, &activity, 100_000),
            "idle"
        );
    }
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
    /// Last title reported by the running program. This is kept separate from
    /// `custom_name` so a manual rename remains an explicit, stable override.
    #[serde(default)]
    pub auto_name: Option<String>,
    /// Program to run; `None` means the user's default shell.
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Run the saved argv unchanged, without preset injection or session rewriting.
    #[serde(default)]
    pub literal_argv: bool,
    /// Optional system prompt delivered to the agent on startup.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// How the system prompt is delivered.
    #[serde(default)]
    pub injection: InjectionMode,
    /// Frozen separate transport for automatic instructions on a ctl task.
    /// Older task records omit this and receive no extra typed instructions.
    #[serde(default)]
    pub instruction_injection: Option<InjectionMode>,
    /// Name of the preset this instance was created from (for display).
    #[serde(default)]
    pub preset: String,
    /// Id of the preset this instance was created from.
    #[serde(default)]
    pub preset_id: Option<Uuid>,
    /// Environment variables for the process.
    #[serde(default)]
    pub env: Vec<EnvVar>,
    /// Durable lifecycle/attention timestamps for terminal instances.
    #[serde(default)]
    pub activity: AgentActivity,
    /// Run inside a persistent tmux session (Unix only).
    #[serde(default)]
    pub use_tmux: bool,
    /// Run in a dedicated git worktree.
    #[serde(default)]
    pub use_worktree: bool,
    /// tmux session name, once created.
    #[serde(default)]
    pub tmux_session: Option<String>,
    /// The server that owns the session. Absent only for older saved panes (or
    /// one recorded under a name tmux can't use, see [`tmux::recorded_socket`]).
    #[serde(default, deserialize_with = "tmux::recorded_socket")]
    pub tmux_socket: Option<tmux::Socket>,
    /// The grid `(cols, rows)` this pane last rendered at, so it can respawn at the
    /// size it will actually be laid out at instead of the 80×24 default.
    ///
    /// A program that starts at the wrong size and is resized after its first paint
    /// redraws only when something prompts it to. A `tmux attach` shows this off
    /// badly: the session's agent painted its UI long ago, so the pane opens visibly
    /// mis-spaced and only rights itself when the user types. Starting at the right
    /// size means nothing has to resize at all.
    #[serde(default)]
    pub grid: Option<(u16, u16)>,
    /// Launch directory supplied by the caller; never owned or removed by muxel.
    #[serde(default)]
    pub launch_dir: Option<PathBuf>,
    /// Owned worktree directory (including legacy worktrees before migration).
    #[serde(default)]
    pub worktree_path: Option<std::path::PathBuf>,
    /// Branch checked out in the worktree.
    #[serde(default)]
    pub worktree_branch: Option<String>,
    /// Shift+Tab presses to send at startup (runner "auto mode"); 0 = none.
    #[serde(default)]
    pub auto_mode_presses: u8,
    /// This pane's own auto-continue message, set from its **Auto** button's
    /// right-click popup. `None` follows [`Settings::auto_continue_message`]. Lives
    /// as long as the pane; see [`autopilot::pane_auto_continue_message`].
    #[serde(default)]
    pub auto_continue_message: Option<String>,
    /// Created by a runner (one-off task launcher).
    #[serde(default)]
    pub is_runner: bool,
    /// A task supplied by `ctl new --prompt`. Erase its text after building the
    /// first launch plan so a non-resuming relaunch cannot re-type the old task.
    /// `instruction_injection` keeps the preset's separate rules transport;
    /// `injection` remains TypeIn for the exact typed user task.
    #[serde(default)]
    pub control_prompt: bool,
    /// Outside creation must never substitute an unapproved fallback shell.
    /// Retain the launch policy across restore and restart.
    #[serde(default)]
    pub control_created: bool,
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
    /// Validated provider transcript/rollout path for the saved session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_path: Option<PathBuf>,
    /// Last consumed Claude hook receipt. Retain it across same-pane fresh
    /// starts so a retained hook cannot restore a discarded conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_hook_record: Option<String>,
    /// Whether this instance's session has been launched at least once, so a
    /// respawn resumes it instead of starting a new conversation.
    #[serde(default)]
    pub session_started: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalLaunchReason {
    NewPane,
    RunAgain,
    Restore,
    ProjectSelection,
    Restart,
    Wake,
    EnsureRunning,
    Recovery,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeCleanup {
    None,
    Registered(Uuid),
    Legacy(PathBuf),
}

impl Instance {
    /// Called after the launch plan has captured its startup input, before the
    /// updated instance is persisted. Reusable runners keep their task text.
    pub fn consume_runner_prompt(&mut self) -> bool {
        if !self.is_runner {
            return false;
        }
        let changed = self.auto_submit || (self.control_prompt && self.system_prompt.is_some());
        self.auto_submit = false;
        if self.control_prompt {
            self.system_prompt = None;
        }
        changed
    }

    /// Literal commands may replay an opening brief, so only creation and the
    /// explicit Run again action can start them. Recovery must leave them stopped.
    pub fn permits_launch(&self, reason: TerminalLaunchReason) -> bool {
        self.kind == InstanceKind::Terminal
            && (!self.literal_argv
                || matches!(
                    reason,
                    TerminalLaunchReason::NewPane | TerminalLaunchReason::RunAgain
                ))
    }

    pub fn working_directory(&self) -> Option<&std::path::Path> {
        self.launch_dir.as_deref().or(self.worktree_path.as_deref())
    }

    /// An unspawned saved literal command is stopped, rather than an idle agent.
    /// Retained views keep their lifecycle badge, including done after exit.
    /// Do not show stopped while a user-requested launch is in flight.
    pub fn is_stopped_literal(&self, has_terminal_view: bool, launching: bool) -> bool {
        self.literal_argv && self.kind == InstanceKind::Terminal && !has_terminal_view && !launching
    }

    /// Capture before removing instance metadata. Launch directories never enter
    /// this plan, even when they happen to be git worktrees of the project repo.
    pub fn worktree_cleanup(&self) -> WorktreeCleanup {
        match (self.worktree_id, &self.worktree_path) {
            (Some(id), _) => WorktreeCleanup::Registered(id),
            (None, Some(path)) => WorktreeCleanup::Legacy(path.clone()),
            _ => WorktreeCleanup::None,
        }
    }

    pub fn needs_peer_session(&self) -> bool {
        self.kind == InstanceKind::Terminal
            && !self.literal_argv
            && self
                .tmux_session
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
    }

    /// Display label precedence. Empty persisted values do not mask the next
    /// useful source.
    pub fn display_name(&self) -> &str {
        self.custom_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                self.auto_name
                    .as_deref()
                    .filter(|name| is_useful_auto_name_for_program(name, self.program.as_deref()))
            })
            .unwrap_or(&self.title)
    }

    /// Record a useful program-supplied title. Returns whether persisted state
    /// changed, so callers can debounce disk writes without losing the last
    /// non-empty title.
    pub fn update_auto_name(&mut self, name: String) -> bool {
        if !is_useful_auto_name_for_program(&name, self.program.as_deref())
            || self.auto_name.as_ref() == Some(&name)
        {
            return false;
        }
        self.auto_name = Some(name);
        true
    }

    /// A duplicated pane copies launch settings, not the source conversation.
    pub fn reset_conversation_for_duplicate(&mut self) {
        self.session_id = None;
        self.session_path = None;
        self.claude_hook_record = None;
        self.session_started = false;
        self.auto_name = None;
        // Duplication is a new local UI action, not the outside launch approval.
        self.control_created = false;
        // An unconsumed `ctl new --prompt` task belongs to the request that was
        // approved, never to a copy of its pane.
        if self.control_prompt {
            self.system_prompt = None;
        }
    }

    /// A plain shell instance.
    pub fn shell(project_id: Uuid) -> Self {
        Self::from_preset(project_id, &AgentPreset::shell())
    }

    pub fn tmux_target(&self, slug: &str) -> tmux::SessionTarget {
        tmux::SessionTarget::new(
            tmux::session_for(self.tmux_session.as_deref(), slug, self.id),
            self.tmux_socket.clone(),
        )
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
            auto_name: None,
            program: preset.program.clone(),
            args: preset.compose_args(),
            literal_argv: false,
            system_prompt: preset.system_prompt.clone(),
            injection: preset.injection.clone(),
            preset: preset.name.clone(),
            preset_id: Some(preset.id),
            env: preset.env.clone(),
            activity: AgentActivity::default(),
            use_tmux: false,
            use_worktree: false,
            tmux_session: None,
            tmux_socket: None,
            grid: None,
            launch_dir: None,
            worktree_path: None,
            worktree_branch: None,
            auto_mode_presses: 0,
            auto_continue_message: None,
            is_runner: false,
            control_prompt: false,
            instruction_injection: None,
            control_created: false,
            // New instances submit their startup prompt; persisted ones default
            // to false (so restored runners re-type without auto-submitting).
            auto_submit: true,
            pinned: false,
            worktree_id: None,
            session_id: None,
            session_path: None,
            claude_hook_record: None,
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
            auto_name: None,
            program: None,
            args: Vec::new(),
            literal_argv: false,
            system_prompt: None,
            injection: InjectionMode::default(),
            preset: String::new(),
            preset_id: None,
            env: Vec::new(),
            activity: AgentActivity::default(),
            use_tmux: false,
            use_worktree: false,
            tmux_session: None,
            tmux_socket: None,
            grid: None,
            launch_dir: None,
            worktree_path: None,
            worktree_branch: None,
            auto_mode_presses: 0,
            auto_continue_message: None,
            is_runner: false,
            control_prompt: false,
            instruction_injection: None,
            auto_submit: false,
            control_created: false,
            pinned: false,
            worktree_id: None,
            session_id: None,
            session_path: None,
            claude_hook_record: None,
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

/// The operating system on the far side of an SSH connection.
///
/// muxel builds every remote command as a shell line, and the two families share
/// no vocabulary: `test -f` against `Test-Path`, `mkdir -p` against `New-Item`,
/// POSIX single-quoting against PowerShell's. A host therefore has to say which
/// it is — there is no probe cheap or reliable enough to do it per command, and
/// guessing wrong produces confusing half-failures rather than a clean error.
///
/// `Unix` is the default and the serde fallback, so every host saved before this
/// existed keeps its exact current behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteOs {
    /// Linux, macOS, BSD — anything with a POSIX shell.
    #[default]
    Unix,
    /// Windows reached through its OpenSSH server.
    Windows,
}

impl RemoteOs {
    pub fn is_windows(self) -> bool {
        self == RemoteOs::Windows
    }
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
    /// Default new remote panes here to a persistent tmux session. Ignored for
    /// [`RemoteOs::Windows`] hosts, which have no tmux — see [`Self::use_tmux`].
    #[serde(default = "default_true")]
    pub default_use_tmux: bool,
    /// Which OS is on the far side. Decides the entire remote command vocabulary.
    #[serde(default)]
    pub os: RemoteOs,
    /// Shell an interactive pane runs on a Windows host. Unused when `os` is Unix
    /// (that side asks the host for `$SHELL`), and never used for muxel's own
    /// commands, which always go through `powershell.exe`.
    #[serde(default)]
    pub windows_shell: crate::winshell::WindowsShell,
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
            os: RemoteOs::default(),
            windows_shell: crate::winshell::WindowsShell::default(),
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
    /// Whether a pane on this host should run inside tmux, given the instance's
    /// own preference.
    ///
    /// The single gate for the decision, because Windows has no tmux and a stray
    /// `true` there produces a pane that dies on `tmux: command not found`. A
    /// host saved as Unix with tmux on and later switched to Windows keeps that
    /// stale `true` on disk, and an `Instance` carries its own flag as well — so
    /// this answers for both rather than trusting either.
    pub fn use_tmux(&self, instance_prefers: bool) -> bool {
        !self.os.is_windows() && (self.default_use_tmux || instance_prefers)
    }

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

/// A remote root in comparable form: no trailing separator, and on Windows hosts
/// `\`-separated and lowercased (their paths are case- and separator-blind).
fn normalize_remote_root(root: &str, os: RemoteOs) -> String {
    let root = root.trim();
    match os {
        RemoteOs::Unix => root.trim_end_matches('/').to_string(),
        RemoteOs::Windows => root
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase(),
    }
}

/// How many unconfirmed closes a project keeps. See [`Project::remember_closed_session`].
const MAX_CLOSED_SESSIONS: usize = 64;

/// A project: a named workspace rooted at a directory, with a pane layout.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub root_path: PathBuf,
    /// The pane tree; `None` when the project has no panes.
    pub layout: Option<PaneNode>,
    /// Last pane deliberately focused in this project. Local navigation state:
    /// remote layout sync must validate it, not replace it.
    #[serde(default)]
    pub last_focused_instance: Option<Uuid>,
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
    /// Inject the accomplishment-log instruction into this project's agents, so they
    /// append each finished feature/fix/… to `.muxel/ACCOMPLISHMENTS.md`.
    #[serde(default)]
    pub accomplishments_enabled: bool,
    /// Unix-seconds version stamp for this project's pane layout, advanced when the
    /// layout/instances change. Drives newer-wins sync of a remote project's layout
    /// to/from `<remote_root>/.muxel/workspace.json`. `None` until first change/sync.
    #[serde(default)]
    pub layout_updated_at: Option<u64>,
    /// Panes closed on this project's host whose tmux session muxel has not yet
    /// confirmed dead. Persisted because the confirmation may have to wait for the
    /// next connect — see [`tmux::ClosedSession`]. Local bookkeeping, never part of
    /// the synced layout doc.
    #[serde(default)]
    pub closed_sessions: Vec<tmux::ClosedSession>,
    /// [`RemoteLayout::fingerprint`] of the shared layout doc as this machine last
    /// wrote or adopted it. Remembered across restarts so a connect can tell a doc
    /// a peer rewrote while this machine was away (adopt it) from one this machine
    /// has changes on top of (push them) — see [`connect_layout_action`]. Local
    /// bookkeeping, never part of the synced layout doc.
    #[serde(default)]
    pub layout_synced: Option<String>,
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
            last_focused_instance: None,
            default_preset: None,
            startup: Vec::new(),
            remote: None,
            memory_enabled: false,
            accomplishments_enabled: false,
            layout_updated_at: None,
            closed_sessions: Vec::new(),
            layout_synced: None,
        }
    }

    /// Whether this project runs on a remote host over SSH.
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// Remember that `session` was closed and must not come back as a pane until
    /// the host confirms it is gone. Idempotent per instance: closing, failing to
    /// confirm, and closing again leaves one entry.
    ///
    /// Capped, oldest first: a host that is never reachable again would otherwise
    /// grow the ledger without bound, and an entry that old has long stopped
    /// protecting anything — its instance is gone from the workspace.
    pub fn remember_closed_session(&mut self, session: tmux::SessionTarget, instance: Uuid) {
        self.closed_sessions.retain(|c| c.instance != instance);
        self.closed_sessions.push(tmux::ClosedSession {
            name: session.name,
            socket: session.socket,
            instance,
        });
        let over = self
            .closed_sessions
            .len()
            .saturating_sub(MAX_CLOSED_SESSIONS);
        self.closed_sessions.drain(..over);
    }

    /// Forget a close whose session the host has confirmed gone.
    pub fn forget_closed_session(&mut self, instance: Uuid) -> bool {
        let before = self.closed_sessions.len();
        self.closed_sessions.retain(|c| c.instance != instance);
        self.closed_sessions.len() != before
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

    /// Restore remembered focus only while that instance remains in the layout.
    pub fn preferred_instance(&self) -> Option<Uuid> {
        self.last_focused_instance
            .filter(|iid| self.instances().contains(iid))
            .or_else(|| self.first_instance())
    }

    pub fn is_empty(&self) -> bool {
        self.layout.is_none()
    }
}

#[cfg(test)]
mod project_focus_tests {
    use super::{PaneNode, Project, add_tab};
    use uuid::Uuid;

    #[test]
    fn preferred_instance_restores_valid_focus_and_rejects_stale_focus() {
        let first = Uuid::new_v4();
        let remembered = Uuid::new_v4();
        let mut project = Project::new("repo", "/repo");
        project.layout = Some(PaneNode::leaf(first));
        assert!(add_tab(&mut project.layout, first, remembered));
        project.last_focused_instance = Some(remembered);
        assert_eq!(project.preferred_instance(), Some(remembered));

        project.last_focused_instance = Some(Uuid::new_v4());
        assert_eq!(project.preferred_instance(), Some(first));
    }

    #[test]
    fn old_project_json_defaults_remembered_focus_to_none() {
        let project = Project::new("repo", "/repo");
        let mut json = serde_json::to_value(project).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("last_focused_instance");
        let restored: Project = serde_json::from_value(json).unwrap();
        assert_eq!(restored.last_focused_instance, None);
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
    /// Persisted accomplishments-panel sidebar width in pixels (None = default).
    #[serde(default)]
    pub accomplishments_panel_width: Option<f32>,
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

    /// The local project already rooted at `root`, if any, so opening the same
    /// folder twice can be refused. Symlinked / trailing-separator spellings match.
    pub fn local_project_at(&self, root: &Path) -> Option<&Project> {
        self.projects
            .iter()
            .find(|p| !p.is_remote() && agent::paths_loosely_equal(&p.root_path, root))
    }

    /// The remote project already open at `remote_root` on `host_id`, if any,
    /// compared per the host's path rules (see [`normalize_remote_root`]).
    pub fn remote_project_at(
        &self,
        host_id: Uuid,
        remote_root: &str,
        os: RemoteOs,
    ) -> Option<&Project> {
        let want = normalize_remote_root(remote_root, os);
        self.projects.iter().find(|p| {
            p.remote.as_ref().is_some_and(|r| {
                r.host_id == host_id && normalize_remote_root(&r.remote_root, os) == want
            })
        })
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
    #[serde(default, serialize_with = "serialize_shared_instances")]
    pub instances: Vec<Instance>,
    #[serde(default)]
    pub worktrees: Vec<Worktree>,
    /// Whether the project's shared memory (`.muxel/MEMORY.md`) is switched on.
    ///
    /// Shared state, not local preference: the memory file lives on the host and
    /// every agent on it — from this desktop, another machine, or the iOS app —
    /// reads and writes the same one. Left out of the doc, each client kept its own
    /// idea of whether memory was on, so a project whose host plainly *has* shared
    /// memory in use still showed the toggle as off wherever it hadn't been switched
    /// on by hand.
    ///
    /// `Option` so a doc written before this field existed says "no opinion" rather
    /// than "off" — otherwise the first sync from an older peer would quietly switch
    /// memory off for everyone.
    #[serde(default)]
    pub memory_enabled: Option<bool>,
    /// Whether the project's accomplishment log (`.muxel/ACCOMPLISHMENTS.md`) is on.
    /// Shared state for the same reason as [`Self::memory_enabled`]: every agent on
    /// the host appends to the one file. `None` = a doc from before the field.
    #[serde(default)]
    pub accomplishments_enabled: Option<bool>,
}

// Local Workspace persistence keeps receipts; only the shared document omits them.
fn serialize_shared_instances<S: serde::Serializer>(
    instances: &[Instance],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeSeq;

    let mut sequence = serializer.serialize_seq(Some(instances.len()))?;
    for instance in instances {
        if instance.claude_hook_record.is_some() {
            let mut shared = instance.clone();
            shared.claude_hook_record = None;
            sequence.serialize_element(&shared)?;
        } else {
            sequence.serialize_element(instance)?;
        }
    }
    sequence.end()
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
            memory_enabled: Some(project.memory_enabled),
            accomplishments_enabled: Some(project.accomplishments_enabled),
        }
    }

    /// Parse a stored document, rejecting a wrong format version or one captured
    /// for a different `remote_root` (both treated as "no usable remote doc").
    pub fn parse(json: &str, expect_root: &str) -> Option<Self> {
        let doc: Self = serde_json::from_str(json).ok()?;
        (doc.version == REMOTE_LAYOUT_VERSION && doc.remote_root == expect_root).then_some(doc)
    }

    /// Whether the doc has any panes. A folder opened as a new project may already
    /// be worked in from elsewhere — a remote project another machine keeps on this
    /// one, or the iOS app — and then these are the panes opening it should show.
    pub fn has_panes(&self) -> bool {
        self.layout
            .as_ref()
            .is_some_and(|layout| layout.first_instance().is_some())
    }

    /// Pretty JSON for storage on the remote.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// A stable key over the *content* (layout + instances + worktrees) that
    /// ignores `version`/`updated_at`, for "did anything change" and "are these in
    /// sync" checks. Relies on [`capture`](Self::capture)'s canonical ordering and
    /// serde_json's deterministic (sorted-key) object encoding.
    ///
    /// Per-machine state is left out, so it never reads as a layout change: the
    /// owning project's id (every machine has its own id for the same project) and
    /// what this machine observes of a pane — its program-supplied title, terminal
    /// grid, agent activity and consumed hook receipts. Counting those pushed the
    /// layout on every resize and agent turn, and made two peers look out of sync
    /// when nothing differed.
    pub fn content_key(&self) -> String {
        self.key(false)
    }

    /// [`content_key`](Self::content_key), also blind to the session bookkeeping a
    /// machine fills in by itself as it runs a pane: the tmux session it bound, the
    /// conversation id it minted or captured, whether that session has started, a
    /// runner's spent auto-submit.
    ///
    /// A change only this key misses is a machine catching up, not someone editing
    /// the layout. It is still pushed, so peers learn it, but it must not be stamped
    /// as a newer revision: then it would outrank — and overwrite — a real edit a
    /// peer made meanwhile, such as moving the pane that was just bound.
    pub fn edit_key(&self) -> String {
        self.key(true)
    }

    /// A short fingerprint of a [`content_key`](Self::content_key), so a project can
    /// remember which doc it last synced ([`Project::layout_synced`]) without
    /// storing the whole layout a second time. FNV-1a rather than DefaultHasher,
    /// whose output may change with Rust releases and would forget every project's
    /// sync across an upgrade.
    pub fn fingerprint(key: &str) -> String {
        let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        format!("{hash:016x}")
    }

    fn key(&self, edits_only: bool) -> String {
        let instances: Vec<Instance> = self
            .instances
            .iter()
            .cloned()
            .map(|mut instance| {
                instance.project_id = Uuid::nil();
                instance.auto_name = None;
                instance.claude_hook_record = None;
                instance.grid = None;
                instance.activity = AgentActivity::default();
                if edits_only {
                    instance.tmux_session = None;
                    instance.tmux_socket = None;
                    instance.session_id = None;
                    instance.session_path = None;
                    instance.session_started = false;
                    instance.auto_submit = false;
                }
                instance
            })
            .collect();
        let worktrees: Vec<Worktree> = self
            .worktrees
            .iter()
            .cloned()
            .map(|mut worktree| {
                worktree.project_id = Uuid::nil();
                worktree
            })
            .collect();
        serde_json::json!({
            "layout": self.layout.as_ref().map(pane::SemanticPane),
            "instances": instances,
            "worktrees": worktrees,
            // In the key, so flipping the toggle counts as a change and is pushed to
            // the host rather than sitting on one machine.
            "memory_enabled": self.memory_enabled,
            "accomplishments_enabled": self.accomplishments_enabled,
        })
        .to_string()
    }
}

/// What a live poll should do with the shared `.muxel/workspace.json` it just read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerLayoutAction {
    /// Nothing new: the file is what this machine last wrote or adopted.
    Ignore,
    /// The file already matches this machine's layout.
    InSync,
    /// A peer wrote a different layout; apply it.
    Adopt,
}

/// Decide a live poll from content keys ([`RemoteLayout::content_key`]) and
/// revisions (`updated_at`). `synced` is the key of the file as this machine last
/// wrote or adopted it, so a file with any other key was written by a peer (the
/// iOS app, or muxel on another machine). The peer's layout is adopted unless this
/// machine also has a change it hasn't pushed yet (`local` differs from `synced`)
/// that is at least as new — then that pending push wins, the same newer-wins rule
/// as the connect-time sync.
pub fn peer_layout_action(
    remote_key: &str,
    remote_rev: u64,
    synced: Option<&str>,
    local_key: &str,
    local_rev: u64,
) -> PeerLayoutAction {
    if remote_key == local_key {
        PeerLayoutAction::InSync
    } else if synced == Some(remote_key) || (synced != Some(local_key) && remote_rev <= local_rev) {
        PeerLayoutAction::Ignore
    } else {
        PeerLayoutAction::Adopt
    }
}

/// Decide a connect's first sync with the shared file (`remote`) by the live-poll
/// rule ([`peer_layout_action`]), with the doc this machine last wrote or adopted
/// known only by its persisted fingerprint, `synced` ([`Project::layout_synced`]).
///
/// Without it, a connect could only compare revisions, and a tie went to this
/// machine. But a peer catching up on session bookkeeping — rebinding a pane's
/// tmux session, picking up the conversation a pane moved to — pushes without a
/// new revision (see [`RemoteLayout::edit_key`]). A machine coming back with its
/// old copy then pushed that copy's stale bookkeeping over the peer's: panes
/// pointed back at a server their session had left, and at conversations they
/// had moved on from, which the next restart would resume. Knowing it has changed
/// nothing since its last sync, this machine adopts the file instead. When the
/// fingerprint matches neither side (both changed, or a doc from before this was
/// remembered) the revisions decide, as before.
pub fn connect_layout_action(
    remote: &RemoteLayout,
    synced: Option<&str>,
    local_key: &str,
    local_rev: u64,
) -> PeerLayoutAction {
    let remote_key = remote.content_key();
    let known = |key: &str| synced == Some(RemoteLayout::fingerprint(key).as_str());
    let synced_key = if known(&remote_key) {
        Some(remote_key.as_str())
    } else if known(local_key) {
        Some(local_key)
    } else {
        None
    };
    peer_layout_action(
        &remote_key,
        remote.updated_at,
        synced_key,
        local_key,
        local_rev,
    )
}

/// Whether writing this machine's layout over the shared file as it stands now
/// would undo a peer: the file holds a write this machine hasn't seen, and under
/// [`peer_layout_action`]'s rule that write wins. The push must then adopt it and
/// push again on top, rather than overwrite it — a blind write here is how a pane
/// a peer moved snapped back to where it was created.
pub fn push_overwrites_peer(
    file: &RemoteLayout,
    synced: Option<&str>,
    local_key: &str,
    local_rev: u64,
) -> bool {
    peer_layout_action(
        &file.content_key(),
        file.updated_at,
        synced,
        local_key,
        local_rev,
    ) == PeerLayoutAction::Adopt
}

/// Panes in the shared file a peer opened that this machine has never had: neither
/// in the doc it last wrote or adopted (`synced`, a [`RemoteLayout::content_key`])
/// nor in its own layout (`local`), not even as another pane on the same tmux
/// session. In the file's reading order.
///
/// A push whose own change is newer than the file's would write over them, though
/// they were never closed: their panes would be gone on every machine while their
/// agents ran on. Without `synced` nothing is reported, since a pane this machine
/// closed then looks the same as one it never had.
pub fn unseen_peer_panes(
    file: &RemoteLayout,
    synced: Option<&str>,
    local: &RemoteLayout,
) -> Vec<Uuid> {
    let Some(seen) = synced.and_then(content_key_instances) else {
        return Vec::new();
    };
    let Some(layout) = &file.layout else {
        return Vec::new();
    };
    let shown_here = |session: &Option<String>| {
        session.is_some() && local.instances.iter().any(|i| i.tmux_session == *session)
    };
    layout
        .collect_instances()
        .into_iter()
        .filter(|id| {
            !seen.contains(id)
                && !local.instances.iter().any(|i| i.id == *id)
                && file
                    .instances
                    .iter()
                    .any(|i| i.id == *id && !shown_here(&i.tmux_session))
        })
        .collect()
}

/// The pane ids a [`RemoteLayout::content_key`] lists.
fn content_key_instances(key: &str) -> Option<std::collections::HashSet<Uuid>> {
    let key: serde_json::Value = serde_json::from_str(key).ok()?;
    Some(
        key.get("instances")?
            .as_array()?
            .iter()
            .filter_map(|instance| instance.get("id")?.as_str()?.parse().ok())
            .collect(),
    )
}

/// The revision for closing a tmux pane because its session ended (a peer closed the
/// pane, or the program in it quit), given the revision the project had.
///
/// Every machine showing the pane closes it when its client exits, but one back from
/// sleep only notices then. Stamped with that moment, its close outranked everything
/// its peers did meanwhile, and newer-wins pushed its stale copy of the layout over
/// theirs: a pane a peer opened while it slept was dropped there, its agent left
/// running with no pane. One past the revision it had orders the close after what
/// this machine has seen and behind any edit a peer made since, so the push takes
/// the peer's layout instead ([`push_overwrites_peer`]).
pub fn observed_close_revision(current: Option<u64>) -> u64 {
    current.unwrap_or(0).saturating_add(1)
}

/// A peer layout's instances as this machine should hold them: every one the peer
/// lists (a pane it closed is simply absent), re-homed to `project_id`, and — for a
/// pane this machine already has — keeping what this machine observed of it (its
/// program-supplied title, terminal grid and agent activity), which is per-machine
/// and never part of the shared content. A validated rollout path survives a peer
/// that predates `session_path` while the pane keeps the same conversation.
pub fn merge_peer_instances(
    local: &[Instance],
    peer: Vec<Instance>,
    project_id: Uuid,
) -> Vec<Instance> {
    peer.into_iter()
        .map(|mut instance| {
            instance.project_id = project_id;
            // A peer's consumed receipt says nothing about this machine, even
            // when this is the first time it has seen the pane.
            instance.claude_hook_record = None;
            if let Some(mine) = local.iter().find(|l| l.id == instance.id) {
                instance.auto_name = mine.auto_name.clone();
                instance.claude_hook_record = mine.claude_hook_record.clone();
                instance.grid = mine.grid;
                instance.activity = mine.activity.clone();
                // Older peers cannot write socket ownership. Preserve an already
                // resolved binding when their layout still describes that pane.
                if instance.tmux_socket.is_none()
                    && (instance.tmux_session.is_none()
                        || instance.tmux_session == mine.tmux_session)
                {
                    instance.tmux_session = mine.tmux_session.clone();
                    instance.tmux_socket = mine.tmux_socket.clone();
                } else if moved_off_default(&instance, mine) {
                    instance.tmux_socket = mine.tmux_socket.clone();
                }
                if instance.session_id.is_some() && instance.session_id == mine.session_id {
                    instance.session_path =
                        mine.session_path.clone().or(instance.session_path.take());
                }
                // Older peers drop the outside-creation launch policy too, which
                // would turn a literal command back into a tmux-wrapped,
                // auto-restored pane allowed a fallback shell. A missing value
                // can't be told apart from a cleared one, but nothing clears
                // these on an existing pane: they are set once at creation (a
                // duplicate is a new id), so absent always means "not known here".
                instance.literal_argv |= mine.literal_argv;
                instance.control_created |= mine.control_created;
                instance.control_prompt |= mine.control_prompt;
                if instance.launch_dir.is_none() {
                    instance.launch_dir = mine.launch_dir.clone();
                }
                if instance.instruction_injection.is_none() {
                    instance.instruction_injection = mine.instruction_injection.clone();
                }
            }
            instance
        })
        .collect()
}

/// Whether `moved` has already taken `stale`'s session off the default tmux
/// server: both name the same session, `stale` on Default and `moved` on muxel's
/// own. No client creates a session on Default any more, so that move is one-way,
/// and a copy of the pane that predates it (a peer's layout, or this machine's
/// own unpushed one) must not undo it. Undone, the pane would refuse to launch
/// (Default is gone and `mxl` has the namesake) and closing it would kill on
/// Default, orphaning the agent on `mxl`. A literal pane runs no session, so
/// its stale tmux fields never take part in a move.
pub fn moved_off_default(stale: &Instance, moved: &Instance) -> bool {
    !stale.literal_argv
        && !moved.literal_argv
        && stale.tmux_socket == Some(tmux::Socket::Default)
        && moved.tmux_socket == Some(tmux::Socket::Private)
        && stale.tmux_session.is_some()
        && stale.tmux_session == moved.tmux_session
}

/// Before this machine's layout (`local`) overwrites the shared file: keep every
/// Default→`mxl` move the file records that `local` doesn't have yet (see
/// [`moved_off_default`]). Returns the panes it moved, with their session names,
/// for this machine to make the same move.
pub fn keep_moves_off_default(local: &mut [Instance], file: &[Instance]) -> Vec<(Uuid, String)> {
    let mut moved = Vec::new();
    for instance in local {
        if let Some(theirs) = file.iter().find(|f| f.id == instance.id)
            && moved_off_default(instance, theirs)
            && let Some(name) = instance.tmux_session.clone()
        {
            instance.tmux_socket = theirs.tmux_socket.clone();
            moved.push((instance.id, name));
        }
    }
    moved
}

/// The panes in both `before` and `after` whose tmux binding (session name and
/// owner) differs between them: after a peer layout is adopted, a pane that failed
/// to launch on its old binding can try the new one. A literal pane has no tmux
/// transport, so a peer's stale binding never retries it.
pub fn rebound_instances(before: &[Instance], after: &[Instance]) -> Vec<Uuid> {
    after
        .iter()
        .filter(|now| !now.literal_argv)
        .filter(|now| {
            before.iter().any(|was| {
                was.id == now.id
                    && (was.tmux_session != now.tmux_session || was.tmux_socket != now.tmux_socket)
            })
        })
        .map(|now| now.id)
        .collect()
}

/// Drop instances that duplicate another one — the same id twice, or a second
/// instance bound to a tmux session another already owns — removing them from the
/// layout as well. The first of each wins.
///
/// Two panes on one tmux session are never right: both attach as clients to the
/// same terminal, so they mirror each other keystroke for keystroke while muxel
/// treats them as separate agents (and closing one kills the session under the
/// other). Run on load, so a workspace that got into this state is repaired rather
/// than reproducing it every launch.
pub fn dedupe_instances(workspace: &mut Workspace) {
    use std::collections::HashSet;
    let mut seen_ids: HashSet<Uuid> = HashSet::new();
    let mut seen_sessions: HashSet<(Uuid, Option<tmux::Socket>, String)> = HashSet::new();
    let mut kept: Vec<Instance> = Vec::with_capacity(workspace.instances.len());
    // Instances dropped as duplicates, whose panes must also leave the layout. An
    // instance repeated under the *same* id is not in here: the copy we keep still
    // answers to that id, so its pane stays valid.
    let mut unlink: Vec<Uuid> = Vec::new();

    for inst in std::mem::take(&mut workspace.instances) {
        if !seen_ids.insert(inst.id) {
            continue; // the same row twice — the kept copy already covers its pane
        }
        let claim = inst
            .tmux_session
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| (inst.project_id, inst.tmux_socket.clone(), s.clone()));
        // A tmux session is claimed by exactly one instance per project.
        if claim.is_some_and(|c| !seen_sessions.insert(c)) {
            unlink.push(inst.id);
            continue;
        }
        kept.push(inst);
    }
    workspace.instances = kept;
    for id in unlink {
        for project in &mut workspace.projects {
            pane::remove(&mut project.layout, id);
        }
    }

    // An instance can also appear *twice in the layout* — one pane per tab, both
    // spawning a terminal, both attaching to its session. `pane::remove` takes the
    // first occurrence it finds, so one call per surplus tab leaves exactly one.
    for project in &mut workspace.projects {
        let mut seen: HashSet<Uuid> = HashSet::new();
        let surplus: Vec<Uuid> = project
            .instances()
            .into_iter()
            .filter(|id| !seen.insert(*id))
            .collect();
        for id in surplus {
            pane::remove(&mut project.layout, id);
        }
    }

    // And the mirror image: an instance the layout never got. It is invisible — no
    // pane shows it and none can close it — while still owning its tmux session, so
    // it can't even be re-adopted (the session counts as taken). Give it a pane back.
    for project in &mut workspace.projects {
        let placed: HashSet<Uuid> = project.instances().into_iter().collect();
        let missing: Vec<Uuid> = workspace
            .instances
            .iter()
            .filter(|i| i.project_id == project.id && !placed.contains(&i.id))
            .map(|i| i.id)
            .collect();
        for id in missing {
            match project.first_instance() {
                Some(anchor) => {
                    pane::add_tab(&mut project.layout, anchor, id);
                }
                // An empty project: this instance seeds the layout.
                None => {
                    project.layout = Some(PaneNode::Leaf(LeafData {
                        pane_id: Uuid::new_v4(),
                        tabs: vec![id],
                        active: 0,
                    }))
                }
            }
        }
    }
}

/// Keep Codex approval policy in restored panes aligned with its current preset.
///
/// Instances snapshot launch arguments when created. Without this repair, changing
/// Codex from `-a never` to `-a on-request` only affects new panes; restored panes
/// silently keep the old policy.
pub fn sync_codex_approval_args(workspace: &mut Workspace, presets: &[AgentPreset]) -> bool {
    let mut changed = false;
    for instance in &mut workspace.instances {
        if !instance
            .program
            .as_deref()
            .is_some_and(|program| program.to_ascii_lowercase().contains("codex"))
        {
            continue;
        }
        let Some(preset) = instance
            .preset_id
            .and_then(|id| presets.iter().find(|preset| preset.id == id))
            .or_else(|| presets.iter().find(|preset| preset.name == instance.preset))
        else {
            continue;
        };
        let Some(policy) = preset
            .compose_args()
            .windows(2)
            .find(|pair| pair[0] == "-a" || pair[0] == "--ask-for-approval")
            .map(|pair| pair[1].clone())
        else {
            continue;
        };
        if let Some(index) = instance
            .args
            .iter()
            .position(|arg| arg == "-a" || arg == "--ask-for-approval")
            .filter(|index| index + 1 < instance.args.len())
            && instance.args[index + 1] != policy
        {
            instance.args[index + 1] = policy;
            changed = true;
        }
    }
    changed
}

/// Keep restored panes on their preset's current instruction transport.
///
/// Instances snapshot the transport when created. A built-in preset can move from
/// visible type-in to a real system-rules flag later; leaving old panes untouched
/// would keep submitting capability text as user turns on their next fresh session.
/// This updates transport metadata only. It does not inject into a resumed session.
pub fn sync_agent_injection_modes(workspace: &mut Workspace, presets: &[AgentPreset]) -> bool {
    let mut changed = false;
    for instance in &mut workspace.instances {
        let Some(preset) = instance
            .preset_id
            .and_then(|id| presets.iter().find(|preset| preset.id == id))
            .or_else(|| presets.iter().find(|preset| preset.name == instance.preset))
        else {
            continue;
        };
        // Runner prompts are one-shot tasks, not reusable capability rules.
        // Their explicit TypeIn transport must survive preset migrations.
        if !instance.is_runner && instance.injection != preset.injection {
            instance.injection = preset.injection.clone();
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod codex_approval_sync_tests {
    use super::*;

    #[test]
    fn restored_codex_pane_adopts_current_approval_policy() {
        let mut preset = AgentPreset::codex();
        preset.args = vec!["-a".into(), "on-request".into()];
        let mut instance = Instance::from_preset(Uuid::new_v4(), &preset);
        instance.args = vec!["-a".into(), "never".into()];
        let mut workspace = Workspace {
            instances: vec![instance],
            ..Workspace::default()
        };

        assert!(sync_codex_approval_args(&mut workspace, &[preset]));
        assert_eq!(workspace.instances[0].args, ["-a", "on-request"]);
        assert!(!sync_codex_approval_args(
            &mut workspace,
            &[AgentPreset::codex()]
        ));
    }
}

#[cfg(test)]
mod agent_injection_sync_tests {
    use super::*;

    #[test]
    fn restored_grok_pane_adopts_rules_transport_without_changing_its_prompt() {
        let preset = AgentPreset::grok();
        let mut instance = Instance::from_preset(Uuid::new_v4(), &preset);
        instance.injection = InjectionMode::TypeIn;
        instance.system_prompt = Some("pane-specific rules".into());
        let mut workspace = Workspace {
            instances: vec![instance],
            ..Workspace::default()
        };

        assert!(sync_agent_injection_modes(
            &mut workspace,
            std::slice::from_ref(&preset)
        ));
        assert_eq!(workspace.instances[0].injection, preset.injection);
        assert_eq!(
            workspace.instances[0].system_prompt.as_deref(),
            Some("pane-specific rules")
        );
        assert!(!sync_agent_injection_modes(&mut workspace, &[preset]));
    }

    #[test]
    fn restored_runner_keeps_one_shot_type_in_transport() {
        let preset = AgentPreset::grok();
        let mut instance = Instance::from_preset(Uuid::new_v4(), &preset);
        instance.injection = InjectionMode::TypeIn;
        instance.system_prompt = Some("one-shot task".into());
        instance.is_runner = true;
        let mut workspace = Workspace {
            instances: vec![instance],
            ..Workspace::default()
        };

        assert!(!sync_agent_injection_modes(&mut workspace, &[preset]));
        assert_eq!(workspace.instances[0].injection, InjectionMode::TypeIn);
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

    /// The instance a run of this runner spawns with the already resolved
    /// `preset`. `details` replaces every `{{input}}`, or is appended after a
    /// blank line; trailing blank lines are trimmed so the submit Enter lands
    /// on a clean line.
    pub fn build_instance(
        &self,
        project_id: Uuid,
        preset: &AgentPreset,
        details: &str,
    ) -> Instance {
        let prompt = if self.prompt.contains("{{input}}") {
            self.prompt.replace("{{input}}", details)
        } else if details.is_empty() {
            self.prompt.clone()
        } else {
            format!("{}\n\n{}", self.prompt, details)
        };
        let mut instance = Instance::from_preset(project_id, preset);
        instance.system_prompt = Some(prompt.trim_end().to_string());
        instance.injection = InjectionMode::TypeIn;
        instance.auto_mode_presses = self.auto_mode_presses;
        instance.custom_name = Some(self.name.clone());
        // Marked as a runner so its first launch submits the prompt, but
        // reopening the app re-types it without auto-submitting.
        instance.is_runner = true;
        instance
    }

    /// Built-in example runners. They run in auto mode, so each prompt forbids
    /// edits (Security Review fixes only the findings the user then picks), and
    /// the scope covers staged, untracked and branch work (a worktree's agent may
    /// already have committed it).
    pub fn defaults() -> Vec<Runner> {
        vec![
            Runner::new(
                "Review",
                "Review the current code changes for bugs and correctness problems. \
                 This is a read-only review: don't edit, stage, or commit anything; \
                 I'll decide what to fix.\n\
                 \n\
                 Scope: all uncommitted work (staged, unstaged, and new untracked \
                 files). If the working tree is clean, review the commits on this \
                 branch that aren't on its base branch (main, master, or its \
                 upstream).\n\
                 \n\
                 How to review:\n\
                 - Read the surrounding code, callers, and tests, not just the diff \
                 hunks, so you judge each change in context.\n\
                 - Look for logic errors, mishandled edge cases (empty, missing, \
                 boundary, concurrent), errors that are swallowed or misreported, \
                 resource leaks, behavior changes that break existing callers, and \
                 new behavior with missing or wrong tests.\n\
                 - Confirm each suspicion before reporting it; you may run the \
                 existing tests or a quick read-only check. Drop anything you can't \
                 back up.\n\
                 - Skip style and formatting preferences.\n\
                 \n\
                 Report findings most severe first. For each: file:line, what's \
                 wrong, a concrete scenario where it fails (input or state, then the \
                 wrong result), and the fix you'd make. If nothing needs fixing, say \
                 so plainly instead of padding the list. End with a one-line verdict: \
                 ready to merge, or what must change first.\n\
                 \n\
                 Any notes I add below take priority over these defaults.\n\
                 \n\
                 {{input}}",
            ),
            Runner::new(
                "Security Review",
                "Do a security review of the current code changes. Don't edit, \
                 stage, or commit anything until I've chosen what to fix, and don't \
                 run exploits against live systems.\n\
                 \n\
                 Scope: all uncommitted work (staged, unstaged, and new untracked \
                 files). If the working tree is clean, review the commits on this \
                 branch that aren't on its base branch (main, master, or its \
                 upstream).\n\
                 \n\
                 How to review:\n\
                 - Work out where untrusted data enters (network requests, files, \
                 command-line arguments, environment, IPC, user or third-party \
                 content) and trace it through the changed code, reading callers \
                 and helpers outside the diff as needed.\n\
                 - Look for injection (shell, SQL, path traversal, templates, XSS), \
                 missing or bypassable authentication and authorization, unsafe \
                 deserialization, SSRF, secrets in code or logs, weak crypto or \
                 randomness, unsafe file handling (permissions, symlinks, races), \
                 memory-unsafe code, unbounded resource use, and new or upgraded \
                 dependencies with known problems.\n\
                 - Report only issues with a plausible attack path. A pre-existing \
                 problem counts only if this change introduces, exposes, or \
                 worsens it.\n\
                 \n\
                 Present your findings as a table ranked from highest to lowest \
                 priority (severity first, then how likely the attack is), with the \
                 columns #, Severity (Critical, High, Medium, Low, or Info for minor \
                 hardening ideas), Issue, Location (file:line), and Recommendation \
                 (fix now, fix later, or accept the risk). Under the table, give each \
                 finding a short note by number: who the attacker is and what they \
                 control, what they gain, and the fix. If you find nothing \
                 exploitable, say so plainly; don't inflate severity to fill the \
                 table.\n\
                 \n\
                 Then recommend which findings to fix (for example, \"I'd fix 1, 2 \
                 and 4\"), ask me which ones you should fix, and stop until I answer. \
                 Fix only the ones I pick, then run the relevant tests and summarize \
                 what you changed.\n\
                 \n\
                 Any notes I add below take priority over these defaults.\n\
                 \n\
                 {{input}}",
            ),
        ]
    }

    /// The built-in runner prompts before v19, by runner name. An unedited copy
    /// of one is replaced with the current built-in prompt.
    const LEGACY_PROMPTS: [(&'static str, &'static str); 2] = [
        (
            "Review",
            "Review the current changes (git diff) for correctness, bugs, and \
             code quality. Call out specific issues with file:line references and \
             suggest concrete fixes.\n\n{{input}}",
        ),
        (
            "Security Review",
            "Perform a security review of the current changes (git diff). Look for \
             injection, auth/authorization gaps, unsafe input handling, leaked \
             secrets, and risky dependencies. Report findings with severity and \
             file:line.\n\n{{input}}",
        ),
    ];
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
    /// Agent preset to launch. A loop names its own: it runs unattended, so
    /// "whatever the toolbar has selected when it fires" could be a shell that runs
    /// the prompt as commands. `None` is only a loop saved before that, which
    /// [`saved_agent`] reports as [`SavedAgent::Current`] and never fires.
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

    /// The instance one fire of this loop spawns with the already resolved
    /// `preset` (never the toolbar's). `{{input}}` is removed. No tmux session,
    /// so repeated fires do not orphan sessions.
    pub fn build_instance(&self, preset: &AgentPreset) -> Instance {
        let prompt = self.prompt.replace("{{input}}", "").trim_end().to_string();
        let mut instance = Instance::from_preset(self.project_id, preset);
        instance.system_prompt = Some(prompt);
        instance.injection = InjectionMode::TypeIn;
        instance.auto_mode_presses = self.auto_mode_presses;
        instance.custom_name = Some(self.name.clone());
        instance.is_runner = true;
        instance.use_tmux = false;
        instance
    }
}

/// What a runner's or loop's saved agent (`preset_id`) refers to now.
#[derive(Clone, Copy, Debug)]
pub enum SavedAgent<'a> {
    /// No preset saved: "Current", whatever the toolbar has selected.
    Current,
    /// A preset that still exists.
    Preset(&'a AgentPreset),
    /// A preset that has since been deleted. Never stand another one in for it.
    Missing,
}

/// Look up a runner's or loop's saved agent among `presets`.
pub fn saved_agent(preset_id: Option<Uuid>, presets: &[AgentPreset]) -> SavedAgent<'_> {
    match preset_id {
        None => SavedAgent::Current,
        Some(id) => presets
            .iter()
            .find(|p| p.id == id)
            .map_or(SavedAgent::Missing, SavedAgent::Preset),
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
    /// New local panes run in a persistent tmux session by default (gated on
    /// tmux actually being installed — see `MuxelApp::tmux_available`).
    #[serde(default = "default_true")]
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
    /// muxel has offered to install a missing tmux (macOS/Linux) — the offer is
    /// made once, on the first launch without it; Settings can reopen it.
    #[serde(default)]
    pub tmux_install_offered: bool,
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
    /// Extra key chords that, while a terminal is focused, are sent to the PTY
    /// instead of muxel's shortcut. Plain `ctrl-<letter>` app bindings already
    /// yield to the terminal by default (see `install_keybindings`); use this
    /// for other shapes (e.g. `ctrl-shift-p`) or custom rebinds.
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
    // --- Speech-to-text (dictate into the focused agent) ---
    /// Which transcription engine dictation uses.
    #[serde(default)]
    pub stt_engine: SttEngine,
    /// whisper.cpp model for the local engine (`tiny` | `base` | `small` | …).
    #[serde(default = "default_stt_model")]
    pub stt_model: String,
    /// Spoken language hint (BCP-47, e.g. `en`); empty = auto-detect.
    #[serde(default)]
    pub stt_language: String,
    /// Base URL of the OpenAI-compatible transcription endpoint (no trailing `/`).
    #[serde(default = "default_stt_provider_url")]
    pub stt_provider_url: String,
    /// Model name sent to the provider (e.g. `whisper-1`, `whisper-large-v3`).
    #[serde(default = "default_stt_provider_model")]
    pub stt_provider_model: String,
    /// Press Enter automatically after inserting the transcript (default: leave
    /// it in the prompt unsubmitted for review).
    #[serde(default)]
    pub stt_autosubmit: bool,
    /// Treat a dictation of [`Settings::stt_wake_phrase`] as the wake command
    /// (sweep every pane and relaunch the ones that aren't running) instead of
    /// pasting it into the focused pane.
    #[serde(default)]
    pub stt_wake_command: bool,
    /// The spoken phrase that triggers the wake command.
    #[serde(default = "default_stt_wake_phrase")]
    pub stt_wake_phrase: String,
    /// What a pane's **Auto** toggle types (then Enter) to resume an agent that
    /// stalled with work left. Kept to one non-blank line by
    /// [`autopilot::auto_continue_message`].
    #[serde(default = "default_auto_continue_message")]
    pub auto_continue_message: String,
    // --- Text-to-speech (the voice read-aloud speaks in; see crates/muxel/src/tts.rs) ---
    /// Which synthesizer speaks. Defaults to the OS voice: it needs no model and
    /// no key, so speech works on a fresh install with nothing configured.
    #[serde(default)]
    pub tts_engine: TtsEngine,
    /// The OS voice by name (System engine: `say -v`, SAPI `SelectVoice`,
    /// `spd-say -y`); empty = the OS default voice.
    #[serde(default)]
    pub tts_system_voice: String,
    /// Speaking pace as a multiple of normal ([`tts::MIN_TTS_RATE`]–[`tts::MAX_TTS_RATE`]).
    #[serde(default = "default_tts_rate")]
    pub tts_rate: f32,
    /// Kokoro voice for the local engine (`bm_george`, `am_michael`, …).
    #[serde(default = "default_kokoro_voice")]
    pub tts_local_voice: String,
    /// Kokoro weights for the local engine (`model_quantized` | `model_q8f16` | `model`).
    #[serde(default = "default_kokoro_model")]
    pub tts_local_model: String,
    /// Voice name sent to the provider (`onyx`, `nova`, …).
    #[serde(default = "default_tts_voice")]
    pub tts_provider_voice: String,
    /// Model sent to the provider (`tts-1`, `gpt-4o-mini-tts`, …). The endpoint and
    /// API key are the Speech section's — one provider, both directions.
    #[serde(default = "default_tts_provider_model")]
    pub tts_provider_model: String,
    // --- Read aloud (accessibility: speak an agent's last reply; see `readaloud`) ---
    /// Show the read-aloud speaker button in the toolbar. The shortcut and the
    /// palette command work either way.
    #[serde(default = "default_true")]
    pub read_aloud_button: bool,
    /// How much of the last reply to read: the final message, or the whole turn.
    #[serde(default)]
    pub read_aloud_scope: ReadAloudScope,
    /// Read replies automatically when an agent finishes.
    #[serde(default)]
    pub read_aloud_auto: ReadAloudAuto,
    /// Say which agent is speaking before its reply ("Claude says: …").
    #[serde(default)]
    pub read_aloud_announce: bool,
    /// Say "link" instead of reading a URL out character by character.
    #[serde(default = "default_true")]
    pub read_aloud_skip_urls: bool,
    /// Say only a path's file name (`src/app.rs:120` → "app.rs").
    #[serde(default = "default_true")]
    pub read_aloud_short_paths: bool,
    /// Read tables row by row; off skips them.
    #[serde(default)]
    pub read_aloud_tables: bool,
    /// Stop after about this many characters, at a sentence (0 = read it all).
    #[serde(default)]
    pub read_aloud_max_chars: u32,
    // --- Outside control (`muxel ctl`; see `control`) ---
    /// Let other programs on this computer (an orchestrating agent such as Grok
    /// Bot) list and drive the agents through `muxel ctl`. Off by default.
    #[serde(default)]
    pub control_enabled: bool,
    /// Off / Ask / Allow for outside terminal creation. Allow opens only presets
    /// running a known agent provider; every other program Asks. Literal argv and
    /// shell presets always Ask and also require control_allow_shells. A bool
    /// from a development build loads as Off / Ask. Revocation refuses pending
    /// approvals, not existing panes.
    #[serde(default)]
    pub control_allow_new: control::CreationPolicy,
    /// Also allow typing into shell panes and `ctl new` shell presets/defaults
    /// or any literal argv. Commands run as the host muxel process, outside the
    /// caller's sandbox. Off by default; non-literal agent writes need only global
    /// outside control. Creation also requires a non-Off control_allow_new policy.
    #[serde(default)]
    pub control_allow_shells: bool,
    // --- Team libraries (`library`) ---
    /// Configured team libraries, in the order they were added.
    #[serde(default)]
    pub libraries: Vec<library::LibraryConfig>,
    /// Shared loops the user switched on, with the content they approved.
    #[serde(default)]
    pub shared_loops: Vec<library::SharedLoopState>,
    /// Shared runners the user confirmed, with the content they confirmed.
    #[serde(default)]
    pub shared_runner_confirmations: Vec<library::SharedRunnerConfirmation>,
    /// Days of accomplishment-log history kept per project (today counts as one);
    /// older entries are pruned from `.muxel/ACCOMPLISHMENTS.md`.
    #[serde(default = "default_accomplishment_retention_days")]
    pub accomplishment_retention_days: u32,
    /// Turn shared memory on for every project created from now on.
    #[serde(default)]
    pub new_project_memory: bool,
    /// Turn the accomplishment log on for every project created from now on.
    #[serde(default)]
    pub new_project_accomplishments: bool,
}

fn default_stt_model() -> String {
    "base".to_string()
}
fn default_stt_provider_url() -> String {
    "https://api.openai.com/v1".to_string()
}
fn default_stt_provider_model() -> String {
    "whisper-1".to_string()
}
fn default_stt_wake_phrase() -> String {
    stt::DEFAULT_WAKE_PHRASE.to_string()
}
fn default_auto_continue_message() -> String {
    autopilot::AUTO_CONTINUE_MESSAGE.to_string()
}
fn default_kokoro_voice() -> String {
    tts::DEFAULT_KOKORO_VOICE.to_string()
}
fn default_kokoro_model() -> String {
    tts::DEFAULT_KOKORO_MODEL.to_string()
}
fn default_tts_voice() -> String {
    tts::DEFAULT_TTS_VOICE.to_string()
}
fn default_tts_provider_model() -> String {
    tts::DEFAULT_TTS_PROVIDER_MODEL.to_string()
}
fn default_tts_rate() -> f32 {
    1.0
}
fn default_accomplishment_retention_days() -> u32 {
    accomplishments::DEFAULT_RETENTION_DAYS
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
    // Plain ctrl+letter app bindings yield to the terminal by default (see
    // install_keybindings); this list is for other shapes (ctrl-shift-*, …).
    Vec::new()
}

/// True for `ctrl-a` … `ctrl-z` with no other modifiers (gpui keystroke form).
/// Used to scope muxel shortcuts away from focused terminals so agents receive C0.
pub fn is_plain_ctrl_letter(keystroke: &str) -> bool {
    let ks = keystroke.trim().to_ascii_lowercase();
    let Some(rest) = ks.strip_prefix("ctrl-") else {
        return false;
    };
    rest.len() == 1 && rest.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
}

/// Program-supplied titles that are useful to a person. Some harnesses expose
/// their session UUID as the terminal title until the user explicitly names the
/// thread; that is identity, not a display name.
pub fn is_useful_auto_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && Uuid::parse_str(name).is_err() && !agent::is_codex_thread_id_hint(name)
}

/// Reject launcher-generated absolute paths such as Codex's packaged
/// `.../codex-win32-x64.exe`. They identify an implementation binary, not a
/// conversation. Ordinary titles and shell directory titles remain valid.
fn is_useful_auto_name_for_program(name: &str, program: Option<&str>) -> bool {
    if !is_useful_auto_name(name) {
        return false;
    }
    let Some(program) = program else {
        return true;
    };
    let name = name.trim().trim_matches('"');
    let bytes = name.as_bytes();
    let absolute = name.starts_with('/')
        || name.starts_with("\\\\")
        || bytes.get(1) == Some(&b':') && bytes.first().is_some_and(u8::is_ascii_alphabetic);
    if !absolute {
        return true;
    }
    let stem = |value: &str| {
        let leaf = value.rsplit(['/', '\\']).next().unwrap_or(value);
        leaf.strip_suffix(".exe")
            .or_else(|| leaf.strip_suffix(".cmd"))
            .or_else(|| leaf.strip_suffix(".bat"))
            .unwrap_or(leaf)
            .to_ascii_lowercase()
    };
    let program_stem = stem(program);
    let title_stem = stem(name);
    title_stem != program_stem
        && !title_stem.starts_with(&format!("{program_stem}-"))
        && !title_stem.starts_with(&format!("{program_stem}_"))
}

#[cfg(test)]
mod instance_tests {
    use super::*;

    #[test]
    fn display_name_prefers_custom_then_auto_then_title() {
        let mut instance = Instance::shell(Uuid::new_v4());
        instance.title = "Claude".into();
        assert_eq!(instance.display_name(), "Claude");

        instance.auto_name = Some("Generated title".into());
        assert_eq!(instance.display_name(), "Generated title");

        instance.custom_name = Some("My title".into());
        assert_eq!(instance.display_name(), "My title");
    }

    #[test]
    fn display_name_ignores_blank_overrides() {
        let mut instance = Instance::shell(Uuid::new_v4());
        instance.title = "Claude".into();
        instance.auto_name = Some("Generated title".into());
        instance.custom_name = Some(" \r\n".into());
        assert_eq!(instance.display_name(), "Generated title");

        instance.auto_name = Some("\t".into());
        assert_eq!(instance.display_name(), "Claude");
    }

    #[test]
    fn packaged_agent_executable_path_is_not_a_display_name() {
        let mut instance = Instance::shell(Uuid::new_v4());
        instance.title = "Codex".into();
        instance.program = Some("codex".into());
        let leaked = r"C:\tools\npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\codex-win32-x64.exe";

        instance.auto_name = Some(leaked.into());
        assert_eq!(instance.display_name(), "Codex");
        instance.auto_name = None;
        assert!(!instance.update_auto_name(leaked.into()));
        assert_eq!(instance.auto_name, None);
        assert!(instance.update_auto_name("Fix restore behavior".into()));
        assert_eq!(instance.display_name(), "Fix restore behavior");
    }

    #[test]
    fn missing_auto_name_deserializes_for_old_workspaces() {
        let instance = Instance::shell(Uuid::new_v4());
        let mut json = serde_json::to_value(&instance).unwrap();
        json.as_object_mut().unwrap().remove("auto_name");

        let decoded: Instance = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.auto_name, None);
        assert_eq!(decoded.display_name(), instance.title);
    }

    #[test]
    fn auto_name_updates_only_for_new_nonblank_titles() {
        let mut instance = Instance::shell(Uuid::new_v4());
        assert!(!instance.update_auto_name(" \r\n".into()));
        assert_eq!(instance.auto_name, None);

        assert!(instance.update_auto_name("Generated title".into()));
        assert_eq!(instance.auto_name.as_deref(), Some("Generated title"));
        assert!(!instance.update_auto_name("Generated title".into()));
        assert!(!instance.update_auto_name(String::new()));
        assert_eq!(instance.auto_name.as_deref(), Some("Generated title"));
    }

    #[test]
    fn auto_name_rejects_a_bare_session_uuid() {
        let mut instance = Instance::shell(Uuid::new_v4());
        let session_id = Uuid::new_v4().to_string();

        assert!(!instance.update_auto_name(session_id.clone()));
        assert_eq!(instance.auto_name, None);

        instance.auto_name = Some(session_id);
        assert_eq!(instance.display_name(), instance.title);
    }

    #[test]
    fn duplicate_reset_keeps_settings_but_clears_conversation_state() {
        let mut instance = Instance::shell(Uuid::new_v4());
        instance.custom_name = Some("Manual name".into());
        instance.auto_name = Some("Generated title".into());
        instance.session_id = Some(Uuid::new_v4().to_string());
        instance.claude_hook_record = Some("consumed receipt".into());
        instance.session_started = true;

        instance.reset_conversation_for_duplicate();

        assert_eq!(instance.custom_name.as_deref(), Some("Manual name"));
        assert_eq!(instance.auto_name, None);
        assert_eq!(instance.session_id, None);
        assert_eq!(instance.claude_hook_record, None);
        assert!(!instance.session_started);
    }

    #[test]
    fn duplicate_never_carries_an_unconsumed_outside_task() {
        let mut task = Instance::shell(Uuid::new_v4());
        task.is_runner = true;
        task.control_prompt = true;
        task.control_created = true;
        task.system_prompt = Some("deploy the branch".into());
        task.reset_conversation_for_duplicate();
        assert_eq!(task.system_prompt, None);
        assert!(!task.control_created);
        // An ordinary runner's prompt is its reusable launch setting.
        let mut runner = Instance::shell(Uuid::new_v4());
        runner.is_runner = true;
        runner.system_prompt = Some("Review the current changes".into());
        runner.reset_conversation_for_duplicate();
        assert_eq!(
            runner.system_prompt.as_deref(),
            Some("Review the current changes")
        );
    }
}

#[cfg(test)]
mod keystroke_tests {
    use super::is_plain_ctrl_letter;

    #[test]
    fn plain_ctrl_letter() {
        assert!(is_plain_ctrl_letter("ctrl-s"));
        assert!(is_plain_ctrl_letter("Ctrl-S"));
        assert!(is_plain_ctrl_letter(" ctrl-r "));
        assert!(!is_plain_ctrl_letter("ctrl-shift-s"));
        assert!(!is_plain_ctrl_letter("ctrl-shift-p"));
        assert!(!is_plain_ctrl_letter("ctrl-1"));
        assert!(!is_plain_ctrl_letter("ctrl-tab"));
        assert!(!is_plain_ctrl_letter("alt-s"));
        assert!(!is_plain_ctrl_letter("secondary-s"));
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_use_tmux: true,
            default_use_worktree: false,
            notifications_enabled: true,
            browser_enabled: true,
            close_on_exit: true,
            confirm_close_terminal: true,
            confirm_close_editor: false,
            confirm_close_diff: false,
            accepted_terms_version: 0,
            tmux_install_offered: false,
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
            stt_engine: SttEngine::default(),
            stt_model: default_stt_model(),
            stt_language: String::new(),
            stt_provider_url: default_stt_provider_url(),
            stt_provider_model: default_stt_provider_model(),
            stt_autosubmit: false,
            stt_wake_command: false,
            stt_wake_phrase: default_stt_wake_phrase(),
            auto_continue_message: default_auto_continue_message(),
            tts_engine: TtsEngine::default(),
            tts_system_voice: String::new(),
            tts_rate: default_tts_rate(),
            tts_local_voice: default_kokoro_voice(),
            tts_local_model: default_kokoro_model(),
            tts_provider_voice: default_tts_voice(),
            tts_provider_model: default_tts_provider_model(),
            read_aloud_button: true,
            read_aloud_scope: ReadAloudScope::default(),
            read_aloud_auto: ReadAloudAuto::default(),
            read_aloud_announce: false,
            read_aloud_skip_urls: true,
            read_aloud_short_paths: true,
            read_aloud_tables: false,
            read_aloud_max_chars: 0,
            control_enabled: false,
            control_allow_new: control::CreationPolicy::Off,
            control_allow_shells: false,
            libraries: Vec::new(),
            shared_loops: Vec::new(),
            shared_runner_confirmations: Vec::new(),
            accomplishment_retention_days: default_accomplishment_retention_days(),
            new_project_memory: false,
            new_project_accomplishments: false,
        }
    }
}

impl Settings {
    /// A project created from now on, with the features these settings turn on
    /// for new projects.
    pub fn new_project(&self, name: impl Into<String>, root_path: impl Into<PathBuf>) -> Project {
        let mut project = Project::new(name, root_path);
        project.memory_enabled = self.new_project_memory;
        project.accomplishments_enabled = self.new_project_accomplishments;
        project
    }
}

/// Current version of the bundled built-in preset + runner set. Bump when adding
/// new built-ins so existing users get them merged once (see
/// [`Settings::seed_builtin_presets`]).
/// v4: added the Amp (ampcode) preset.
/// v5: added the Grok (x.ai) preset.
/// v6: opencode default runner startup delay.
/// v10: added the "Ollama Code" preset (`ollama launch <agent> --model <model>`).
/// v11: Grok preset gains `--session-id`/`--resume` session resume.
/// v12: added the Codex preset (agent-minted session resume).
/// v13: added the built-in Browser preset (opens a browser pane).
/// v14: migrated the built-in Grok preset from delayed TypeIn to `--rules`.
/// v15: repaired Grok presets saved with the generic Claude-only prompt flag.
/// v16: added the Windows Git Bash preset.
/// v17: Grok preset gains a runner startup delay (its output never goes quiet).
/// v18: added the Muse Code preset.
/// v19: refreshed the built-in Review and Security Review runner prompts.
pub const PRESET_SEED_VERSION: u32 = 19;

/// Current version of the Terms of Service / Privacy notice. Bump this when the
/// terms change so users are asked to accept again on next launch (see
/// [`Settings::accepted_terms_version`]).
pub const CURRENT_TERMS_VERSION: u32 = 1;

impl Settings {
    /// What read-aloud keeps and drops when it turns a reply into speech.
    pub fn speak_options(&self) -> readaloud::SpeakOptions {
        readaloud::SpeakOptions {
            skip_urls: self.read_aloud_skip_urls,
            shorten_paths: self.read_aloud_short_paths,
            read_tables: self.read_aloud_tables,
            max_chars: self.read_aloud_max_chars as usize,
        }
    }

    /// Apply pending built-in migrations, matching presets by name. The v18
    /// migration only adds Muse and v19 only refreshes unedited built-in runner
    /// prompts; older catch-up migrations must not replay over choices saved at
    /// v17. Returns whether settings should be persisted.
    pub fn seed_builtin_presets(&mut self) -> bool {
        if self.preset_seed_version >= PRESET_SEED_VERSION {
            return false;
        }
        // Preserve the historical catch-up for installs that have not reached
        // v17. Later additions have their own migration below.
        if self.preset_seed_version < 17 {
            for builtin in AgentPreset::defaults()
                .into_iter()
                .filter(|p| p.name != "Muse Code")
            {
                if !self.presets.iter().any(|p| p.name == builtin.name) {
                    self.presets.push(builtin);
                }
            }
            // Old Grok presets typed capability instructions into the live prompt
            // after startup. If the user began typing first, delayed text could
            // append to and submit their question. Migrate only the exact legacy
            // built-in shape; preserve explicit None and custom flag choices.
            let grok_rules = AgentPreset::grok().injection;
            for preset in &mut self.presets {
                let is_grok_program = preset
                    .program
                    .as_deref()
                    .and_then(|program| std::path::Path::new(program).file_stem())
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.eq_ignore_ascii_case("grok"));
                let wrong_generic_flag = matches!(
                    &preset.injection,
                    InjectionMode::CliFlag { flag } if flag == "--append-system-prompt"
                );
                if preset.name == "Grok"
                    && is_grok_program
                    && (preset.injection == InjectionMode::TypeIn || wrong_generic_flag)
                {
                    preset.injection = grok_rules.clone();
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
        }
        if self.preset_seed_version < 18 {
            let muse = AgentPreset::muse();
            if !self.presets.iter().any(|preset| preset.name == muse.name) {
                self.presets.push(muse);
            }
        }
        // A built-in runner still carrying its old prompt word for word gets the
        // current one; any edit to the prompt keeps the user's text.
        if self.preset_seed_version < 19 {
            for builtin in Runner::defaults() {
                let Some((_, legacy)) = Runner::LEGACY_PROMPTS
                    .iter()
                    .find(|(name, _)| *name == builtin.name)
                else {
                    continue;
                };
                for runner in &mut self.runners {
                    if runner.name == builtin.name && runner.prompt == *legacy {
                        runner.prompt = builtin.prompt.clone();
                    }
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
        assert!(s.presets.iter().any(|p| p.name == "Muse Code"));
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
    fn muse_preset_is_seeded_only_once() {
        let mut settings = Settings {
            preset_seed_version: 17,
            presets: vec![AgentPreset::shell()],
            ..Settings::default()
        };
        assert!(settings.seed_builtin_presets());
        assert_eq!(
            settings
                .presets
                .iter()
                .filter(|preset| preset.name == "Muse Code")
                .count(),
            1
        );
        settings.presets.retain(|preset| preset.name != "Muse Code");
        assert!(
            !settings.seed_builtin_presets(),
            "a deliberate deletion stays deleted"
        );
        assert!(
            !settings
                .presets
                .iter()
                .any(|preset| preset.name == "Muse Code")
        );
    }

    #[test]
    fn muse_upgrade_preserves_v17_settings_except_addition_and_version() {
        for existing_muse in [false, true] {
            let mut settings = Settings {
                preset_seed_version: 17,
                ..Settings::default()
            };
            settings
                .presets
                .retain(|preset| preset.name != "Muse Code" && preset.name != "Ollama");
            settings.runners.retain(|runner| runner.name != "Review");
            settings.runners[0].prompt = "custom review task".to_string();
            for preset in &mut settings.presets {
                preset.session_id_flag = None;
                preset.resume_flag = None;
                preset.startup_delay_ms = 0;
                preset.working_markers.clear();
                if preset.name == "Grok" {
                    preset.injection = InjectionMode::TypeIn;
                }
            }
            if existing_muse {
                let mut custom = AgentPreset::muse();
                custom.program = Some("custom-muse".to_string());
                custom.effort = Some("medium".to_string());
                custom.injection = InjectionMode::TypeIn;
                settings.presets.push(custom);
            }
            let before = serde_json::to_value(&settings).unwrap();
            assert!(settings.seed_builtin_presets());
            assert_eq!(settings.preset_seed_version, PRESET_SEED_VERSION);
            assert_eq!(
                settings
                    .presets
                    .iter()
                    .filter(|preset| preset.name == "Muse Code")
                    .count(),
                1
            );
            if !existing_muse {
                let added = settings.presets.pop().unwrap();
                assert_eq!(added.name, "Muse Code");
                assert_eq!(added.injection, InjectionMode::SessionHook);
            }
            settings.preset_seed_version = 17;
            assert_eq!(
                serde_json::to_value(&settings).unwrap(),
                before,
                "no old preset/runner resurrection or replay of saved defaults"
            );
        }
    }

    #[test]
    fn unedited_legacy_runner_prompts_are_refreshed_once() {
        let legacy = |name: &str| {
            let (_, prompt) = Runner::LEGACY_PROMPTS
                .iter()
                .find(|(n, _)| *n == name)
                .unwrap();
            prompt.to_string()
        };
        let current = |name: &str| {
            Runner::defaults()
                .into_iter()
                .find(|r| r.name == name)
                .unwrap()
                .prompt
        };
        let mut settings = Settings {
            preset_seed_version: 18,
            ..Settings::default()
        };
        settings.runners[0].prompt = legacy("Review");
        settings.runners[0].auto_mode_presses = 1;
        settings.runners[1].prompt =
            format!("{} Also check the CI config.", legacy("Security Review"));
        let mut renamed = settings.runners[0].clone();
        renamed.name = "My Review".to_string();
        settings.runners.push(renamed);

        assert!(settings.seed_builtin_presets());
        assert_eq!(settings.runners[0].prompt, current("Review"));
        assert_eq!(
            settings.runners[0].auto_mode_presses, 1,
            "only the prompt changes"
        );
        assert!(
            settings.runners[1]
                .prompt
                .ends_with("Also check the CI config."),
            "an edited prompt is the user's own"
        );
        assert_eq!(
            settings.runners[2].prompt,
            legacy("Review"),
            "matched by name"
        );

        // Restoring the old text afterwards sticks: the migration runs once.
        settings.runners[0].prompt = legacy("Review");
        assert!(!settings.seed_builtin_presets());
        assert_eq!(settings.runners[0].prompt, legacy("Review"));
    }

    #[test]
    fn builtin_runner_prompts_take_details_and_end_cleanly_without_them() {
        let preset = AgentPreset::claude();
        for runner in Runner::defaults() {
            assert!(
                runner
                    .prompt
                    .to_lowercase()
                    .contains("don't edit, stage, or commit anything"),
                "{} starts in auto mode, so it must not edit unasked",
                runner.name
            );
            let with = runner.build_instance(Uuid::nil(), &preset, "only src/auth.rs");
            assert!(
                with.system_prompt
                    .unwrap()
                    .ends_with("take priority over these defaults.\n\nonly src/auth.rs"),
                "{}",
                runner.name
            );
            let without = runner.build_instance(Uuid::nil(), &preset, "");
            assert!(
                without
                    .system_prompt
                    .unwrap()
                    .ends_with("take priority over these defaults."),
                "{}",
                runner.name
            );
        }
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
    fn new_projects_get_the_features_settings_turn_on_for_them() {
        // Off for a config from before these settings, so nothing changes on upgrade.
        let mut s: Settings = serde_json::from_str("{}").expect("parse");
        let p = s.new_project("app", "/srv/app");
        assert!(!p.memory_enabled && !p.accomplishments_enabled);
        assert_eq!(
            (p.name.as_str(), p.root_path.as_path()),
            ("app", Path::new("/srv/app"))
        );

        s.new_project_memory = true;
        let p = s.new_project("app", "/srv/app");
        assert!(p.memory_enabled && !p.accomplishments_enabled);
        s.new_project_accomplishments = true;
        assert!(s.new_project("app", "/srv/app").accomplishments_enabled);
    }

    #[test]
    fn read_aloud_settings_default_for_old_configs() {
        // A config from before read-aloud: the button shows, nothing auto-reads,
        // URLs and paths are shortened, and the voice keeps its normal pace.
        let s: Settings = serde_json::from_str("{}").expect("parse");
        assert!(s.read_aloud_button);
        assert_eq!(s.read_aloud_scope, ReadAloudScope::FinalMessage);
        assert_eq!(s.read_aloud_auto, ReadAloudAuto::Off);
        assert!(!s.read_aloud_announce);
        assert_eq!(s.tts_rate, 1.0);
        assert_eq!(s.accomplishment_retention_days, 14);
        assert!(s.tts_system_voice.is_empty());
        assert_eq!(s.speak_options(), readaloud::SpeakOptions::default());
        // Saved choices come back.
        let s: Settings = serde_json::from_str(
            r#"{"read_aloud_scope": "WholeTurn", "read_aloud_auto": "All", "read_aloud_max_chars": 1000}"#,
        )
        .expect("parse");
        assert_eq!(s.read_aloud_scope, ReadAloudScope::WholeTurn);
        assert_eq!(s.read_aloud_auto, ReadAloudAuto::All);
        assert_eq!(s.speak_options().max_chars, 1000);
    }

    #[test]
    fn outside_control_is_off_for_old_and_new_configs() {
        // Nothing outside muxel can drive its agents until the user says so.
        let s: Settings = serde_json::from_str("{}").expect("parse");
        assert!(!s.control_enabled);
        assert_eq!(s.control_allow_new, control::CreationPolicy::Off);
        assert!(!s.control_allow_shells);
        assert!(!Settings::default().control_enabled);
        assert_eq!(
            Settings::default().control_allow_new,
            control::CreationPolicy::Off
        );
        assert!(!Settings::default().control_allow_shells);
        let s: Settings = serde_json::from_str(r#"{"control_enabled": true}"#).expect("parse");
        assert!(s.control_enabled);
        assert_eq!(s.control_allow_new, control::CreationPolicy::Off);
        assert!(!s.control_allow_shells);
    }

    #[test]
    fn outside_terminal_creation_is_an_independent_opt_in() {
        let legacy: Settings =
            serde_json::from_str(r#"{"control_enabled":true,"control_allow_shells":true}"#)
                .unwrap();
        assert!(legacy.control_enabled);
        assert!(legacy.control_allow_shells);
        assert!(
            legacy.control_allow_new == control::CreationPolicy::Off,
            "old grants must not authorize creation on upgrade"
        );
        for enabled in [false, true] {
            let settings: Settings = serde_json::from_value(serde_json::json!({
                "control_enabled": true, "control_allow_shells": true,
                "control_allow_new": enabled,
            }))
            .unwrap();
            let saved = serde_json::to_value(&settings).unwrap();
            // A development build's bool never becomes Allow.
            let policy = if enabled {
                control::CreationPolicy::Ask
            } else {
                control::CreationPolicy::Off
            };
            assert_eq!(
                saved["control_allow_new"],
                if enabled { "ask" } else { "off" }
            );
            let restored: Settings = serde_json::from_value(saved).unwrap();
            assert_eq!(restored.control_allow_new, policy);
            assert!(restored.control_enabled && restored.control_allow_shells);
        }
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
    fn seed_repairs_legacy_grok_instruction_transports() {
        let mut legacy = AgentPreset::grok();
        legacy.injection = InjectionMode::TypeIn;
        let mut generic_flag = AgentPreset::grok();
        generic_flag.injection = InjectionMode::CliFlag {
            flag: "--append-system-prompt".into(),
        };
        let mut disabled = AgentPreset::grok();
        disabled.name = "Grok disabled".into();
        disabled.injection = InjectionMode::None;
        let mut settings = Settings {
            preset_seed_version: 13,
            presets: vec![legacy, generic_flag, disabled],
            ..Settings::default()
        };

        assert!(settings.seed_builtin_presets());
        assert_eq!(
            settings.presets[0].injection,
            InjectionMode::CliFlag {
                flag: "--rules".into()
            }
        );
        assert_eq!(
            settings.presets[1].injection,
            InjectionMode::CliFlag {
                flag: "--rules".into()
            }
        );
        assert_eq!(settings.presets[2].injection, InjectionMode::None);
        assert!(!settings.seed_builtin_presets());
    }

    #[test]
    fn seed_gives_saved_grok_presets_the_startup_delay() {
        // A Grok preset saved before v17 has no delay, so a runner waited out the
        // full auto cap behind Grok's never-quiet welcome screen.
        let mut saved = AgentPreset::grok();
        saved.startup_delay_ms = 0;
        let mut tuned = AgentPreset::grok();
        tuned.startup_delay_ms = 1500;
        let mut settings = Settings {
            preset_seed_version: 16,
            presets: vec![saved, tuned],
            ..Settings::default()
        };

        assert!(settings.seed_builtin_presets());
        assert_eq!(
            settings.presets[0].startup_delay_ms,
            AgentPreset::grok().startup_delay_ms
        );
        assert!(settings.presets[0].startup_delay_ms > 0);
        // A delay the user chose is left alone.
        assert_eq!(settings.presets[1].startup_delay_ms, 1500);
    }

    #[test]
    fn a_saved_agent_is_found_current_or_missing_never_substituted() {
        let presets = vec![AgentPreset::shell(), AgentPreset::claude()];
        let claude = presets[1].id;
        assert!(matches!(saved_agent(None, &presets), SavedAgent::Current));
        assert!(matches!(
            saved_agent(Some(claude), &presets),
            SavedAgent::Preset(p) if p.id == claude
        ));
        // A deleted preset is reported, not swapped for the shell or anything else.
        assert!(matches!(
            saved_agent(Some(Uuid::new_v4()), &presets),
            SavedAgent::Missing
        ));
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
mod closed_session_tests {
    use super::{MAX_CLOSED_SESSIONS, Project};
    use uuid::Uuid;

    #[test]
    fn closing_the_same_pane_twice_leaves_one_entry() {
        let mut p = Project::new("app", "/srv/app");
        let iid = Uuid::new_v4();
        p.remember_closed_session("muxel_app_1".into(), iid);
        p.remember_closed_session("muxel_app_1".into(), iid);
        assert_eq!(p.closed_sessions.len(), 1);
        assert!(p.forget_closed_session(iid), "a confirmed kill clears it");
        assert!(p.closed_sessions.is_empty());
        assert!(
            !p.forget_closed_session(iid),
            "and clearing twice is a no-op"
        );
    }

    /// A host that never comes back must not grow the ledger without bound; the
    /// newest closes — the ones whose panes could still be adopted back — survive.
    #[test]
    fn the_ledger_is_capped_oldest_first() {
        let mut p = Project::new("app", "/srv/app");
        let ids: Vec<Uuid> = (0..MAX_CLOSED_SESSIONS + 10)
            .map(|n| {
                let iid = Uuid::new_v4();
                p.remember_closed_session(format!("muxel_app_{n}").into(), iid);
                iid
            })
            .collect();
        let last = *ids.last().unwrap();
        assert_eq!(p.closed_sessions.len(), MAX_CLOSED_SESSIONS);
        assert_eq!(p.closed_sessions.last().unwrap().instance, last);
    }

    /// Old `workspace.json` files predate the ledger and must still load.
    #[test]
    fn a_project_without_the_field_still_loads() {
        let mut v = serde_json::to_value(Project::new("app", "/srv/app")).unwrap();
        v.as_object_mut().unwrap().remove("closed_sessions");
        let p: Project = serde_json::from_value(v).unwrap();
        assert!(p.closed_sessions.is_empty());
    }
}

#[cfg(test)]
mod remote_layout_tests {
    use super::*;

    #[test]
    fn content_key_is_stable_for_legacy_and_ios_layouts() {
        let json = r#"{"version":1,"updated_at":10,"remote_root":"/srv/test","layout":{"kind":"split","direction":"horizontal","sizes":[900,300],"children":[{"kind":"leaf","tabs":["11111111-1111-4111-8111-111111111111"],"active":0},{"kind":"leaf","tabs":["22222222-2222-4222-8222-222222222222"],"active":0}]},"instances":[],"worktrees":[],"memory_enabled":false}"#;
        let first = RemoteLayout::parse(json, "/srv/test").unwrap();
        let second = RemoteLayout::parse(json, "/srv/test").unwrap();
        assert_eq!(first.layout, second.layout);
        assert_eq!(first.content_key(), second.content_key());

        // Old/iOS peers preserve content but omit desktop-only leaf identity.
        // Returning that same document must not schedule a push or a view teardown.
        let persisted = RemoteLayout::parse(&first.to_json(), "/srv/test").unwrap();
        assert_eq!(persisted.content_key(), second.content_key());
        let mut renamed = second.clone();
        if let Some(PaneNode::Split { sizes, .. }) = &mut renamed.layout {
            sizes.swap(0, 1);
        }
        assert_ne!(persisted.content_key(), renamed.content_key());
    }

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
            pane_id: Uuid::new_v4(),
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
    fn a_folder_worked_in_remotely_opens_locally_with_its_panes() {
        // Another machine's remote project wrote the host's doc for this folder.
        let mut ws = Workspace::default();
        let mut remote = remote_project("/srv/app");
        let pane = Instance::shell(remote.id);
        remote.layout = Some(PaneNode::leaf(pane.id));
        remote.layout_updated_at = Some(50);
        ws.add_instance(pane);
        ws.projects = vec![remote.clone()];
        let json = RemoteLayout::capture(&remote, &ws, 60).to_json();

        // Opened locally on the host, the folder's doc is this project's own.
        let doc = RemoteLayout::parse(&json, "/srv/app").expect("doc for this root");
        assert!(doc.has_panes());
        // A project just opened there has no panes and has never synced, so its
        // first sync takes the doc rather than pushing its empty layout over it.
        let fresh = Project::new("app", "/srv/app");
        let local = RemoteLayout::capture(&fresh, &Workspace::default(), 70);
        assert_eq!(
            connect_layout_action(&doc, None, &local.content_key(), 0),
            PeerLayoutAction::Adopt
        );

        // A doc with no panes left leaves the folder to open with a fresh one.
        let mut emptied = remote;
        emptied.layout = None;
        let json = RemoteLayout::capture(&emptied, &ws, 60).to_json();
        assert!(!RemoteLayout::parse(&json, "/srv/app").unwrap().has_panes());
    }

    #[test]
    fn content_key_ignores_updated_at_but_tracks_layout() {
        let mut ws = Workspace::default();
        let mut proj = remote_project("/srv/app");
        let i1 = Instance::shell(proj.id);
        let id1 = i1.id;
        proj.layout = Some(PaneNode::Leaf(LeafData {
            pane_id: Uuid::new_v4(),
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
            pane_id: Uuid::new_v4(),
            tabs: vec![id1, id2],
            active: 0,
        }));
        ws.instances.push(i2);
        let c = RemoteLayout::capture(&proj, &ws, 1);
        assert_ne!(a.content_key(), c.content_key());
    }

    #[test]
    fn content_key_ignores_auto_name_but_tracks_custom_name() {
        let mut ws = Workspace::default();
        let mut proj = remote_project("/srv/app");
        let instance = Instance::shell(proj.id);
        proj.layout = Some(PaneNode::Leaf(LeafData {
            pane_id: Uuid::new_v4(),
            tabs: vec![instance.id],
            active: 0,
        }));
        ws.instances = vec![instance];
        ws.projects = vec![proj.clone()];

        let original = RemoteLayout::capture(&proj, &ws, 1);
        ws.instances[0].auto_name = Some("Generated title".into());
        let retitled = RemoteLayout::capture(&proj, &ws, 1);
        assert_eq!(original.content_key(), retitled.content_key());

        ws.instances[0].custom_name = Some("My title".into());
        let renamed = RemoteLayout::capture(&proj, &ws, 1);
        assert_ne!(original.content_key(), renamed.content_key());
    }

    #[test]
    fn pr108_claude_hook_record_does_not_change_shared_content_keys() {
        let mut ws = Workspace::default();
        let mut project = remote_project("/srv/app");
        let mut instance = Instance::shell(project.id);
        instance.program = Some("claude".into());
        project.layout = Some(PaneNode::leaf(instance.id));
        ws.instances.push(instance);
        ws.projects.push(project.clone());
        let original = RemoteLayout::capture(&project, &ws, 1);
        for receipt in [
            Some("first-local-receipt"),
            Some("second-local-receipt"),
            None,
        ] {
            ws.instances[0].claude_hook_record = receipt.map(str::to_string);
            let consumed = RemoteLayout::capture(&project, &ws, 1);
            assert_eq!(
                consumed.content_key(),
                original.content_key(),
                "receipt consumption must not push shared layout content"
            );
            assert_eq!(consumed.edit_key(), original.edit_key());
        }
        ws.instances[0].custom_name = Some("Shared rename".into());
        let renamed = RemoteLayout::capture(&project, &ws, 1);
        assert_ne!(renamed.content_key(), original.content_key());
    }

    #[test]
    fn pr108_claude_hook_record_is_local_persistence_not_shared_serialization() {
        let mut ws = Workspace::default();
        let mut project = remote_project("/srv/app");
        let mut instance = Instance::shell(project.id);
        instance.program = Some("claude".into());
        instance.claude_hook_record = Some("consumed-local-receipt".into());
        project.layout = Some(PaneNode::leaf(instance.id));
        ws.instances.push(instance);
        ws.projects.push(project.clone());
        let local_json = serde_json::to_value(&ws).unwrap();
        assert_eq!(
            local_json["instances"][0]["claude_hook_record"],
            "consumed-local-receipt"
        );
        let local_back: Workspace = serde_json::from_value(local_json).unwrap();
        assert_eq!(
            local_back.instances[0].claude_hook_record,
            ws.instances[0].claude_hook_record
        );
        let mut shared = RemoteLayout::capture(&project, &ws, 1);
        // Cover reserializing a document read from an older desktop too, not
        // just a capture that could clear the field in its temporary clone.
        shared.instances[0].claude_hook_record = Some("receipt-from-older-desktop".into());
        let wire = serde_json::to_value(&shared).unwrap();
        assert!(
            wire["instances"][0].get("claude_hook_record").is_none(),
            "RemoteLayout must not publish a machine's consumed receipt"
        );
        let stored: serde_json::Value = serde_json::from_str(&shared.to_json()).unwrap();
        assert!(stored["instances"][0].get("claude_hook_record").is_none());
        assert_eq!(
            ws.instances[0].claude_hook_record.as_deref(),
            Some("consumed-local-receipt")
        );
    }

    #[test]
    fn pr108_peer_merge_keeps_this_machines_claude_hook_record() {
        let local_pid = Uuid::new_v4();
        let peer_pid = Uuid::new_v4();
        for consumed in [None, Some("consumed-local-receipt")] {
            let mut mine = Instance::shell(local_pid);
            mine.program = Some("claude".into());
            mine.session_id = Some("local-conversation".into());
            mine.claude_hook_record = consumed.map(str::to_string);
            for session in ["local-conversation", "peer-changed-conversation"] {
                for receipt_field in ["missing", "null", "foreign"] {
                    let mut incoming = serde_json::to_value(&mine).unwrap();
                    incoming["project_id"] = serde_json::json!(peer_pid);
                    incoming["session_id"] = serde_json::json!(session);
                    incoming["custom_name"] = serde_json::json!("Peer rename");
                    match receipt_field {
                        "missing" => {
                            incoming
                                .as_object_mut()
                                .unwrap()
                                .remove("claude_hook_record");
                        }
                        "null" => incoming["claude_hook_record"] = serde_json::Value::Null,
                        _ => {
                            incoming["claude_hook_record"] =
                                serde_json::json!("consumed-peer-receipt")
                        }
                    }
                    let peer: Instance = serde_json::from_value(incoming).unwrap();
                    let merged =
                        merge_peer_instances(std::slice::from_ref(&mine), vec![peer], local_pid);
                    assert_eq!(
                        merged[0].claude_hook_record, mine.claude_hook_record,
                        "peer receipt field {receipt_field} / session {session} must not change local consumption"
                    );
                    assert_eq!(merged[0].project_id, local_pid);
                    assert_eq!(merged[0].session_id.as_deref(), Some(session));
                    assert_eq!(merged[0].custom_name.as_deref(), Some("Peer rename"));
                }
            }
        }
    }

    #[test]
    fn pr108_new_peer_pane_does_not_adopt_another_machines_claude_hook_record() {
        let local_pid = Uuid::new_v4();
        let mut peer = Instance::shell(Uuid::new_v4());
        peer.program = Some("claude".into());
        peer.session_id = Some("peer-conversation".into());
        peer.claude_hook_record = Some("consumed-peer-receipt".into());
        let merged = merge_peer_instances(&[], vec![peer.clone()], local_pid);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, peer.id);
        assert_eq!(merged[0].project_id, local_pid);
        assert_eq!(merged[0].session_id, peer.session_id);
        assert_eq!(merged[0].program, peer.program);
        assert!(
            merged[0].claude_hook_record.is_none(),
            "a peer's consumed receipt does not prove this machine consumed it"
        );
    }

    #[test]
    fn content_key_ignores_per_machine_state() {
        let mut ws = Workspace::default();
        let mut proj = remote_project("/srv/app");
        let wt = worktree(proj.id, "swift-pine", 0);
        let mut instance = Instance::shell(proj.id);
        instance.worktree_id = Some(wt.id);
        proj.layout = Some(PaneNode::Leaf(LeafData {
            pane_id: Uuid::new_v4(),
            tabs: vec![instance.id],
            active: 0,
        }));
        ws.instances = vec![instance];
        ws.worktrees = vec![wt];
        ws.projects = vec![proj.clone()];
        let original = RemoteLayout::capture(&proj, &ws, 1);

        // A resize and an agent turn are this machine's observation, not layout.
        ws.instances[0].grid = Some((120, 40));
        ws.instances[0].activity.completed_at = Some(1_700_000_000);
        ws.instances[0].activity.last_state = Some(AgentActivityState::Done);
        let observed = RemoteLayout::capture(&proj, &ws, 1);
        assert_eq!(original.content_key(), observed.content_key());

        // The same project on another machine carries that machine's project id.
        let mut peer = original.clone();
        let other = Uuid::new_v4();
        peer.instances.iter_mut().for_each(|i| i.project_id = other);
        peer.worktrees.iter_mut().for_each(|w| w.project_id = other);
        assert_eq!(original.content_key(), peer.content_key());
    }

    #[test]
    fn peer_layout_action_adopts_only_peer_writes() {
        use PeerLayoutAction::{Adopt, Ignore, InSync};
        // Same content either way: nothing to apply, just record it.
        assert_eq!(peer_layout_action("a", 5, Some("x"), "a", 9), InSync);
        // Our own last write (or what we last adopted) is not news.
        assert_eq!(peer_layout_action("a", 5, Some("a"), "b", 9), Ignore);
        // A peer wrote while nothing is pending here: adopt, whatever its clock says.
        assert_eq!(peer_layout_action("p", 1, Some("a"), "a", 9), Adopt);
        // Both sides changed: the newer one wins.
        assert_eq!(peer_layout_action("p", 10, Some("a"), "b", 9), Adopt);
        assert_eq!(peer_layout_action("p", 9, Some("a"), "b", 9), Ignore);
        // Nothing recorded yet: only a strictly newer peer layout is taken.
        assert_eq!(peer_layout_action("p", 10, None, "b", 9), Adopt);
        assert_eq!(peer_layout_action("p", 8, None, "b", 9), Ignore);
    }

    /// The bug: a machine back from being away tied with the host file on revision
    /// (a peer's session bookkeeping never bumps it) and pushed its own stale copy
    /// over the peer's — panes pointing back at conversations they had left.
    #[test]
    fn a_connect_adopts_a_peers_bookkeeping_when_nothing_changed_here() {
        use PeerLayoutAction::{Adopt, Ignore, InSync};
        let mut ws = Workspace::default();
        let mut proj = remote_project("/srv/app");
        let mut pane = Instance::shell(proj.id);
        pane.session_id = Some("old-conversation".into());
        proj.layout = Some(PaneNode::Leaf(LeafData {
            pane_id: Uuid::new_v4(),
            tabs: vec![pane.id],
            active: 0,
        }));
        proj.layout_updated_at = Some(100);
        ws.instances = vec![pane];
        ws.projects = vec![proj.clone()];
        let ours = RemoteLayout::capture(&proj, &ws, 100);
        let ours_key = ours.content_key();
        let synced = RemoteLayout::fingerprint(&ours_key);

        // A peer moved the pane on to another conversation, same revision.
        let mut theirs = ours.clone();
        theirs.instances[0].session_id = Some("new-conversation".into());
        assert_eq!(theirs.updated_at, 100);
        let action = |synced: Option<&str>, local: &str, rev| {
            connect_layout_action(&theirs, synced, local, rev)
        };
        assert_eq!(action(Some(&synced), &ours_key, 100), Adopt);
        // Even when this machine's clock stamped it later: it changed nothing since.
        assert_eq!(action(Some(&synced), &ours_key, 500), Adopt);
        // Nothing remembered (a doc from before this existed): revisions decide.
        assert_eq!(action(None, &ours_key, 100), Ignore);

        // This machine has changes the file was synced before: they are pushed.
        let theirs_synced = RemoteLayout::fingerprint(&theirs.content_key());
        assert_eq!(action(Some(&theirs_synced), "edited here", 100), Ignore);
        // Both sides changed since: the newer revision wins.
        assert_eq!(action(Some("0000000000000000"), "edited here", 100), Ignore);
        assert_eq!(action(Some("0000000000000000"), "edited here", 99), Adopt);
        // Already the same.
        assert_eq!(action(None, &theirs.content_key(), 1), InSync);
    }

    #[test]
    fn a_layout_fingerprint_is_stable_across_builds() {
        assert_eq!(RemoteLayout::fingerprint(""), "cbf29ce484222325");
        assert_eq!(RemoteLayout::fingerprint("foobar"), "85944171f73967e8");
    }

    #[test]
    fn session_bookkeeping_is_not_an_edit() {
        let mut ws = Workspace::default();
        let mut proj = Project::new("proj", "/local/proj");
        let a = Instance::shell(proj.id);
        let x = Instance::shell(proj.id);
        proj.layout = Some(PaneNode::Leaf(LeafData {
            pane_id: Uuid::new_v4(),
            tabs: vec![a.id, x.id],
            active: 0,
        }));
        ws.instances = vec![a.clone(), x.clone()];
        ws.projects = vec![proj.clone()];
        let before = RemoteLayout::capture(&proj, &ws, 1);

        // A machine binding the pane's tmux session and conversation: content, not edit.
        ws.instances[1].tmux_session = Some("muxel_proj_1a2b3c4d".into());
        ws.instances[1].tmux_socket = Some(tmux::Socket::Default);
        ws.instances[1].session_id = Some(Uuid::new_v4().to_string());
        ws.instances[1].claude_hook_record = Some("consumed receipt".into());
        ws.instances[1].session_started = true;
        let bound = RemoteLayout::capture(&proj, &ws, 1);
        assert_ne!(bound.content_key(), before.content_key());
        assert_eq!(bound.edit_key(), before.edit_key());

        // Moving the pane, or renaming it, is an edit.
        assert!(pane::move_into_split(
            &mut proj.layout,
            x.id,
            a.id,
            SplitDirection::Horizontal,
            false
        ));
        let moved = RemoteLayout::capture(&proj, &ws, 1);
        assert_ne!(moved.edit_key(), bound.edit_key());
        ws.instances[0].custom_name = Some("Reviewer".into());
        let renamed = RemoteLayout::capture(&proj, &ws, 1);
        assert_ne!(renamed.edit_key(), moved.edit_key());
    }

    #[test]
    fn a_push_never_overwrites_a_peer_move_made_meanwhile() {
        // A peer creates pane X (rev 100) and this machine adopts it, binding X's
        // tmux session. The binding keeps rev 100 — bookkeeping, not an edit.
        let mut ws = Workspace::default();
        let mut proj = Project::new("proj", "/local/proj");
        let a = Instance::shell(proj.id);
        let x = Instance::shell(proj.id);
        proj.layout = Some(PaneNode::Leaf(LeafData {
            pane_id: Uuid::new_v4(),
            tabs: vec![a.id, x.id],
            active: 0,
        }));
        ws.instances = vec![a.clone(), x.clone()];
        ws.projects = vec![proj.clone()];
        let created = RemoteLayout::capture(&proj, &ws, 100);
        ws.instances[1].tmux_session = Some("muxel_proj_1a2b3c4d".into());
        let ours = RemoteLayout::capture(&proj, &ws, 100);

        // Before this machine's push lands, the peer moves X into its own split and
        // writes that. Its revision is on the peer's own clock, past its create: the
        // iOS app stamps every write `max(now, previous + 1)`, and a desktop's move
        // can only race a push if it came after this machine read the create.
        let mut peer = created.clone();
        assert!(pane::move_into_split(
            &mut peer.layout,
            x.id,
            a.id,
            SplitDirection::Horizontal,
            false
        ));
        peer.updated_at = 101;
        let synced = created.content_key();
        assert!(push_overwrites_peer(
            &peer,
            Some(&synced),
            &ours.content_key(),
            ours.updated_at,
        ));

        // The file holding only what this machine already knows: push away.
        assert!(!push_overwrites_peer(
            &created,
            Some(&synced),
            &ours.content_key(),
            ours.updated_at,
        ));
        // A real local edit newer than the peer's still wins, as before.
        assert!(!push_overwrites_peer(
            &peer,
            Some(&synced),
            &ours.content_key(),
            102
        ));
    }

    #[test]
    fn a_close_noticed_after_sleep_takes_the_peers_newer_pane() {
        // Both machines show shell S (rev 100). This one sleeps; the other closes S,
        // which ends its tmux session, and opens agent C (rev 200).
        let mut ws = Workspace::default();
        let mut proj = Project::new("proj", "/local/proj");
        let shell = Instance::shell(proj.id);
        proj.layout = Some(PaneNode::leaf(shell.id));
        proj.layout_updated_at = Some(100);
        ws.instances = vec![shell.clone()];
        ws.projects = vec![proj.clone()];
        let base = RemoteLayout::capture(&proj, &ws, 100);
        let agent = Instance::shell(proj.id);
        let mut peer = base.clone();
        peer.layout = Some(PaneNode::leaf(agent.id));
        peer.instances = vec![agent];
        peer.updated_at = 200;

        // Awake again, this machine sees S's client exit cleanly and closes S.
        let mut closed = proj;
        closed.layout = None;
        let ours = RemoteLayout::capture(&closed, &ws, 300).content_key();
        let synced = base.content_key();

        // Stamped as the session's end, the close ranks behind the peer's edit:
        // the push takes the peer's layout, with C in it.
        let rev = observed_close_revision(Some(100));
        assert!(push_overwrites_peer(&peer, Some(&synced), &ours, rev));
        // Stamped when it was noticed, it outranked the peer and wrote this
        // machine's empty layout over C.
        assert!(!push_overwrites_peer(&peer, Some(&synced), &ours, 300));
        // With nothing new from the peer, the close is pushed as it is.
        assert!(!push_overwrites_peer(&base, Some(&synced), &ours, rev));
    }

    #[test]
    fn unseen_peer_panes_are_the_peers_new_ones_not_this_machines_closes() {
        // Both machines show shells A and B (rev 100).
        let mut ws = Workspace::default();
        let mut proj = Project::new("proj", "/local/proj");
        let a = Instance::shell(proj.id);
        let b = Instance::shell(proj.id);
        proj.layout = Some(PaneNode::leaf(a.id));
        assert!(pane::split(
            &mut proj.layout,
            a.id,
            SplitDirection::Horizontal,
            b.id
        ));
        proj.layout_updated_at = Some(100);
        ws.instances = vec![a.clone(), b.clone()];
        ws.projects = vec![proj.clone()];
        let base = RemoteLayout::capture(&proj, &ws, 100);
        let synced = base.content_key();

        // Apart, the peer opens agent C beside A (rev 200), and this machine closes
        // B and opens shell D (rev 300): this push is newer and would win.
        let c = Instance::shell(proj.id);
        let mut peer = base.clone();
        assert!(pane::add_tab(&mut peer.layout, a.id, c.id));
        peer.instances.push(c.clone());
        peer.updated_at = 200;
        let d = Instance::shell(proj.id);
        assert!(pane::remove(&mut proj.layout, b.id));
        assert!(pane::add_tab(&mut proj.layout, a.id, d.id));
        ws.instances = vec![a, d.clone()];
        let ours = RemoteLayout::capture(&proj, &ws, 300);
        assert!(!push_overwrites_peer(
            &peer,
            Some(&synced),
            &ours.content_key(),
            300
        ));

        // Only C is the peer's own: B is this machine's close, D its new pane.
        assert_eq!(unseen_peer_panes(&peer, Some(&synced), &ours), vec![c.id]);
        // Unless this machine already shows C's session, in a pane of its own.
        let session = |doc: &mut RemoteLayout, id: Uuid| {
            let pane = doc.instances.iter_mut().find(|i| i.id == id).unwrap();
            pane.tmux_session = Some("muxel_proj_0c0c0c0c".into());
        };
        let (mut shown, mut ours_shown) = (peer.clone(), ours.clone());
        session(&mut shown, c.id);
        session(&mut ours_shown, d.id);
        assert!(unseen_peer_panes(&shown, Some(&synced), &ours_shown).is_empty());
        // The file as this machine left it holds nothing unseen.
        assert!(unseen_peer_panes(&base, Some(&synced), &ours).is_empty());
        // Without the doc it last synced, a close can't be told from a new pane.
        assert!(unseen_peer_panes(&peer, None, &ours).is_empty());
    }

    #[test]
    fn merge_peer_instances_keeps_local_observation() {
        let local_pid = Uuid::new_v4();
        let mut kept = Instance::shell(local_pid);
        kept.grid = Some((100, 30));
        kept.auto_name = Some("Fixing tests".into());
        kept.activity.last_state = Some(AgentActivityState::Working);
        let closed = Instance::shell(local_pid);

        let peer_pid = Uuid::new_v4();
        let mut peer_kept = kept.clone();
        peer_kept.project_id = peer_pid;
        peer_kept.grid = Some((40, 20));
        peer_kept.auto_name = None;
        peer_kept.activity = AgentActivity::default();
        peer_kept.custom_name = Some("Reviewer".into());
        let mut added = Instance::shell(peer_pid);
        added.tmux_session = Some("muxel_proj_12345678".into());

        let merged = merge_peer_instances(
            &[kept.clone(), closed.clone()],
            vec![peer_kept, added.clone()],
            local_pid,
        );
        // The pane the peer closed is gone; the rest belong to this project.
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|i| i.project_id == local_pid));
        assert!(merged.iter().all(|i| i.id != closed.id));

        let k = merged.iter().find(|i| i.id == kept.id).unwrap();
        assert_eq!(k.custom_name.as_deref(), Some("Reviewer")); // shared: the peer's
        assert_eq!(k.grid, Some((100, 30))); // per-machine: ours
        assert_eq!(k.auto_name.as_deref(), Some("Fixing tests"));
        assert_eq!(k.activity.last_state, Some(AgentActivityState::Working));

        let a = merged.iter().find(|i| i.id == added.id).unwrap();
        assert_eq!(a.tmux_session, added.tmux_session);
    }

    /// A pane this machine moved off Default (saved, not yet pushed) against a
    /// peer layout that still has it there, and the reverse.
    fn moved_and_stale() -> (Instance, Instance) {
        let mut moved = Instance::shell(Uuid::new_v4());
        moved.tmux_session = Some("muxel_proj_12345678".into());
        moved.tmux_socket = Some(tmux::Socket::Private);
        let mut stale = moved.clone();
        stale.tmux_socket = Some(tmux::Socket::Default);
        stale.custom_name = Some("Renamed by the peer".into());
        (moved, stale)
    }

    #[test]
    fn literal_panes_take_no_part_in_a_move_off_default() {
        let (moved, stale) = moved_and_stale();
        for (mut stale, mut moved, literal_moved) in
            [(stale.clone(), moved.clone(), false), (stale, moved, true)]
        {
            if literal_moved {
                moved.literal_argv = true;
            } else {
                stale.literal_argv = true;
            }
            assert!(!moved_off_default(&stale, &moved));
            let mut local = vec![stale.clone()];
            assert!(keep_moves_off_default(&mut local, std::slice::from_ref(&moved)).is_empty());
            assert_eq!(local[0].tmux_socket, stale.tmux_socket);
        }
    }

    #[test]
    fn a_peer_layout_cannot_move_a_pane_back_onto_default() {
        let (moved, stale) = moved_and_stale();
        let merged = merge_peer_instances(
            std::slice::from_ref(&moved),
            vec![stale.clone()],
            moved.project_id,
        );
        assert_eq!(merged[0].tmux_socket, Some(tmux::Socket::Private));
        assert_eq!(merged[0].tmux_session, moved.tmux_session);
        // The rest of the peer's copy still wins.
        assert_eq!(merged[0].custom_name, stale.custom_name);

        // The other way round the peer's move is adopted as usual.
        let merged = merge_peer_instances(
            std::slice::from_ref(&stale),
            vec![moved.clone()],
            moved.project_id,
        );
        assert_eq!(merged[0].tmux_socket, Some(tmux::Socket::Private));

        // Only the same session moves one way: a peer that rebound the pane to
        // another session on Default, or to another server, is adopted.
        let mut rebound = stale.clone();
        rebound.tmux_session = Some("imported".into());
        let mut named = stale.clone();
        named.tmux_socket = Some(tmux::Socket::Named("peer".into()));
        for peer in [rebound, named] {
            let merged = merge_peer_instances(
                std::slice::from_ref(&moved),
                vec![peer.clone()],
                moved.project_id,
            );
            assert_eq!(merged[0].tmux_socket, peer.tmux_socket);
            assert_eq!(merged[0].tmux_session, peer.tmux_session);
        }
    }

    #[test]
    fn a_push_keeps_a_move_off_default_the_file_already_has() {
        let (moved, stale) = moved_and_stale();
        let mut other = Instance::shell(moved.project_id);
        other.tmux_session = Some("work".into());
        other.tmux_socket = Some(tmux::Socket::Default);
        let mut local = vec![stale.clone(), other.clone()];
        let mut file_other = other.clone();
        file_other.tmux_session = Some("elsewhere".into());
        file_other.tmux_socket = Some(tmux::Socket::Private);
        let ids = keep_moves_off_default(&mut local, &[moved.clone(), file_other]);
        assert_eq!(ids, vec![(moved.id, "muxel_proj_12345678".to_string())]);
        assert_eq!(local[0].tmux_socket, Some(tmux::Socket::Private));
        assert_eq!(
            local[0].custom_name, stale.custom_name,
            "only the owner moves"
        );
        assert_eq!(
            (&local[1].tmux_session, &local[1].tmux_socket),
            (&other.tmux_session, &other.tmux_socket),
            "a different session is not this pane's move"
        );

        // A file that predates the move changes nothing.
        let mut local = vec![moved.clone()];
        assert!(keep_moves_off_default(&mut local, &[stale]).is_empty());
        assert_eq!(local[0].tmux_socket, moved.tmux_socket);
    }

    #[test]
    fn rebound_instances_are_those_whose_session_or_owner_changed() {
        let (moved, stale) = moved_and_stale();
        let mut renamed = stale.clone();
        renamed.tmux_session = Some("other".into());
        let mut titled = stale.clone();
        titled.custom_name = Some("only the title".into());
        let added = Instance::shell(moved.project_id);
        assert_eq!(
            rebound_instances(std::slice::from_ref(&stale), std::slice::from_ref(&moved)),
            vec![moved.id]
        );
        assert_eq!(
            rebound_instances(std::slice::from_ref(&stale), &[renamed]),
            vec![stale.id]
        );
        assert!(rebound_instances(std::slice::from_ref(&stale), &[titled, added]).is_empty());
        let mut literal = moved.clone();
        literal.literal_argv = true;
        assert!(rebound_instances(std::slice::from_ref(&stale), &[literal]).is_empty());
    }

    /// Reduce a desktop pane subtree to what the iOS `PaneNode` encoder writes: only
    /// its coding keys (no `pane_id`).
    fn as_ios_pane(node: &mut serde_json::Value) {
        const IOS_KEYS: [&str; 7] = [
            "kind",
            "tabs",
            "active",
            "instance",
            "direction",
            "sizes",
            "children",
        ];
        if let Some(obj) = node.as_object_mut() {
            obj.retain(|k, _| IOS_KEYS.contains(&k.as_str()));
            if let Some(children) = obj.get_mut("children").and_then(|c| c.as_array_mut()) {
                children.iter_mut().for_each(as_ios_pane);
            }
        }
    }

    #[test]
    fn ios_write_back_is_not_a_layout_change() {
        // The iOS app's models have no `grid`, `activity` or leaf `pane_id`, so every
        // layout it writes back drops them. That must read as the same layout here —
        // or each phone save would look like a peer change — and adopting it must
        // keep this machine's own grid and activity.
        let mut ws = Workspace::default();
        let mut proj = Project::new("proj", "/local/proj");
        let mut a = Instance::shell(proj.id);
        a.grid = Some((120, 40));
        a.activity.last_state = Some(AgentActivityState::Working);
        a.tmux_session = Some("muxel_studio_1a2b3c4d".into());
        a.tmux_socket = Some(tmux::Socket::Default);
        let b = Instance::shell(proj.id);
        proj.layout = Some(PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![
                PaneNode::Leaf(LeafData {
                    pane_id: Uuid::new_v4(),
                    tabs: vec![a.id],
                    active: 0,
                }),
                PaneNode::Leaf(LeafData {
                    pane_id: Uuid::new_v4(),
                    tabs: vec![b.id],
                    active: 0,
                }),
            ],
        });
        ws.instances = vec![a.clone(), b];
        ws.projects = vec![proj.clone()];
        let desktop = RemoteLayout::capture(&proj, &ws, 10);

        // The same document as the iOS encoder writes it.
        let mut doc: serde_json::Value = serde_json::from_str(&desktop.to_json()).unwrap();
        for inst in doc["instances"].as_array_mut().unwrap() {
            let inst = inst.as_object_mut().unwrap();
            inst.remove("grid");
            inst.remove("activity");
        }
        as_ios_pane(&mut doc["layout"]);
        doc["updated_at"] = 11.into();
        let ios = RemoteLayout::parse(&doc.to_string(), "/local/proj").expect("iOS doc parses");

        let key = desktop.content_key();
        assert_eq!(ios.content_key(), key);
        assert_eq!(
            peer_layout_action(&ios.content_key(), 11, Some(key.as_str()), &key, 10),
            PeerLayoutAction::InSync
        );

        let merged = merge_peer_instances(&ws.instances, ios.instances, proj.id);
        let a2 = merged.iter().find(|i| i.id == a.id).unwrap();
        assert_eq!(a2.grid, Some((120, 40)));
        assert_eq!(a2.activity.last_state, Some(AgentActivityState::Working));
        assert_eq!(a2.tmux_session, a.tmux_session);
        assert_eq!(a2.tmux_socket, a.tmux_socket);
    }

    #[test]
    fn an_older_peer_cannot_strip_outside_creation_launch_policy() {
        let pid = Uuid::new_v4();
        let mut literal = Instance::shell(pid);
        literal.program = Some("/usr/local/bin/native".into());
        literal.args = vec!["opening brief".into(), "".into()];
        literal.literal_argv = true;
        literal.control_created = true;
        literal.launch_dir = Some("/srv/external".into());
        let mut task = Instance::from_preset(pid, &AgentPreset::claude());
        task.control_created = true;
        task.control_prompt = true;
        task.instruction_injection = Some(InjectionMode::CliFlag {
            flag: "--append-system-prompt".into(),
        });
        task.launch_dir = Some("/srv/external".into());
        let local = vec![literal.clone(), task.clone()];
        // The same panes as an older build writes them back: unknown fields gone.
        let older: Vec<Instance> = local
            .iter()
            .map(|instance| {
                let mut json = serde_json::to_value(instance).unwrap();
                let fields = json.as_object_mut().unwrap();
                for field in [
                    "literal_argv",
                    "launch_dir",
                    "control_created",
                    "control_prompt",
                    "instruction_injection",
                ] {
                    fields.remove(field);
                }
                serde_json::from_value(json).unwrap()
            })
            .collect();
        assert!(!older[0].literal_argv && older[0].launch_dir.is_none());
        let merged = merge_peer_instances(&local, older, pid);
        for (merged, mine) in merged.iter().zip(&local) {
            assert_eq!(merged.literal_argv, mine.literal_argv);
            assert_eq!(merged.control_created, mine.control_created);
            assert_eq!(merged.control_prompt, mine.control_prompt);
            assert_eq!(merged.launch_dir, mine.launch_dir);
            assert_eq!(merged.instruction_injection, mine.instruction_injection);
        }
        // A pane only the peer has keeps exactly what the peer wrote.
        let added = Instance::shell(pid);
        let merged = merge_peer_instances(&local, vec![added.clone()], pid);
        assert!(!merged[0].literal_argv && !merged[0].control_created);
        assert!(merged[0].launch_dir.is_none());
    }

    #[test]
    fn peer_without_session_path_keeps_this_machines_rollout_for_the_same_conversation() {
        let pid = Uuid::new_v4();
        let mut mine = Instance::from_preset(pid, &AgentPreset::codex());
        mine.session_id = Some("01a1543c-9700-7000-8000-000000000001".into());
        mine.session_started = true;
        mine.session_path =
            Some("/home/me/.codex/sessions/2026/10/19/rollout-01a1543c.jsonl".into());
        // What a peer that predates the field (or an older iOS build) writes back.
        let mut doc = serde_json::to_value(&mine).unwrap();
        doc.as_object_mut().unwrap().remove("session_path");
        let old_peer: Instance = serde_json::from_value(doc).unwrap();
        assert_eq!(old_peer.session_path, None);
        let merged = merge_peer_instances(std::slice::from_ref(&mine), vec![old_peer], pid);
        assert_eq!(merged[0].session_path, mine.session_path);

        // An iOS write-back carries the field as `null` or its value.
        let mut doc = serde_json::to_value(&mine).unwrap();
        doc["session_path"] = serde_json::Value::Null;
        let ios: Instance = serde_json::from_value(doc).unwrap();
        let merged = merge_peer_instances(std::slice::from_ref(&mine), vec![ios], pid);
        assert_eq!(merged[0].session_path, mine.session_path);

        // A peer that switched the pane's conversation describes another rollout.
        let mut switched = mine.clone();
        switched.session_id = Some("01a1543c-9700-7000-8000-000000000002".into());
        switched.session_path = None;
        let merged = merge_peer_instances(std::slice::from_ref(&mine), vec![switched], pid);
        assert_eq!(merged[0].session_path, None);

        // This machine has none yet: the peer's copy of it stands.
        let mut unsaved = mine.clone();
        unsaved.session_path = None;
        let merged = merge_peer_instances(&[unsaved], vec![mine.clone()], pid);
        assert_eq!(merged[0].session_path, mine.session_path);
    }

    #[test]
    fn namesakes_on_different_sockets_survive_dedupe_and_old_peer_sync() {
        let mut ws = Workspace::default();
        let mut a = Instance::shell(Uuid::new_v4());
        a.tmux_session = Some("work".into());
        a.tmux_socket = Some(tmux::Socket::Private);
        let mut b = a.clone();
        b.id = Uuid::new_v4();
        b.tmux_socket = Some(tmux::Socket::Default);
        ws.instances = vec![a.clone(), b];
        dedupe_instances(&mut ws);
        assert_eq!(ws.instances.len(), 2);
        let mut legacy = a.clone();
        legacy.tmux_socket = None;
        let merged = merge_peer_instances(&ws.instances, vec![legacy], a.project_id);
        assert_eq!(merged[0].tmux_target("p"), a.tmux_target("p"));
    }

    #[test]
    fn unfamiliar_socket_owners_preserve_every_peer_pane() {
        let mut ws = Workspace::default();
        let mut project = remote_project("/srv/app");
        let instances: Vec<_> = ["mxl", "default", "mxl-profile", "   ", "owner '名; $()"]
            .into_iter()
            .map(|name| {
                let mut instance = Instance::shell(project.id);
                instance.tmux_session = Some("same-name".into());
                instance.tmux_socket = Some(tmux::Socket::try_from(name.to_string()).unwrap());
                instance
            })
            .collect();
        let ids: Vec<_> = instances.iter().map(|i| i.id).collect();
        project.layout = Some(PaneNode::Leaf(LeafData {
            pane_id: Uuid::new_v4(),
            tabs: ids.clone(),
            active: 0,
        }));
        ws.instances = instances.clone();
        ws.projects = vec![project.clone()];
        let doc = RemoteLayout::capture(&project, &ws, 42);
        let parsed = RemoteLayout::parse(&doc.to_json(), "/srv/app").unwrap();
        assert_eq!(parsed.instances.len(), instances.len());
        assert_eq!(parsed.layout.as_ref().unwrap().collect_instances(), ids);
        for expected in &instances {
            let actual = parsed
                .instances
                .iter()
                .find(|i| i.id == expected.id)
                .unwrap();
            assert_eq!(actual.tmux_target("p"), expected.tmux_target("p"));
        }
        assert_eq!(
            RemoteLayout::parse(&parsed.to_json(), "/srv/app")
                .unwrap()
                .content_key(),
            doc.content_key()
        );
        ws.instances = merge_peer_instances(&instances, parsed.instances, project.id);
        dedupe_instances(&mut ws);
        assert_eq!(
            ws.instances.len(),
            instances.len(),
            "different owners must not collapse namesakes"
        );

        // An owner tmux can't use as a socket name costs only that owner.
        let mut json: serde_json::Value = serde_json::from_str(&doc.to_json()).unwrap();
        json["instances"][2]["tmux_socket"] = serde_json::json!("../elsewhere");
        let parsed = RemoteLayout::parse(&json.to_string(), "/srv/app").unwrap();
        assert_eq!(parsed.instances.len(), instances.len());
        let invalid = &parsed.instances[2];
        assert_eq!(invalid.tmux_socket, None);
        assert_eq!(invalid.tmux_session.as_deref(), Some("same-name"));
    }

    #[test]
    fn json_round_trips_and_parse_validates() {
        let mut ws = Workspace::default();
        let mut proj = remote_project("/srv/app");
        let i1 = Instance::shell(proj.id);
        proj.layout = Some(PaneNode::Leaf(LeafData {
            pane_id: Uuid::new_v4(),
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
mod open_project_tests {
    use super::{Project, RemoteOs, RemoteRef, Uuid, Workspace};
    use std::path::Path;

    fn remote(host_id: Uuid, root: &str) -> Project {
        let mut p = Project::new("r", root);
        p.remote = Some(RemoteRef {
            host_id,
            remote_root: root.to_string(),
        });
        p
    }

    #[test]
    fn local_project_at_matches_the_same_folder() {
        let dir = std::env::temp_dir();
        let mut ws = Workspace::default();
        let id = ws.add_project(Project::new("tmp", &dir));
        assert_eq!(ws.local_project_at(&dir).map(|p| p.id), Some(id));
        // Missing-on-disk paths fall back to a trailing-separator-blind compare.
        let gone = ws.add_project(Project::new("gone", "/muxel-nope/proj"));
        assert_eq!(
            ws.local_project_at(Path::new("/muxel-nope/proj/"))
                .map(|p| p.id),
            Some(gone)
        );
        assert!(
            ws.local_project_at(Path::new("/muxel-nope/other"))
                .is_none()
        );
    }

    #[test]
    fn local_project_at_ignores_remote_projects() {
        // A remote project's root_path is cosmetic; it must not block a local open.
        let mut ws = Workspace::default();
        ws.add_project(remote(Uuid::new_v4(), "/muxel-nope/srv"));
        assert!(ws.local_project_at(Path::new("/muxel-nope/srv")).is_none());
    }

    #[test]
    fn remote_project_at_matches_host_and_root() {
        let host = Uuid::new_v4();
        let mut ws = Workspace::default();
        let id = ws.add_project(remote(host, "/home/me/app"));
        let found = |h, root: &str| ws.remote_project_at(h, root, RemoteOs::Unix).map(|p| p.id);
        assert_eq!(found(host, "/home/me/app"), Some(id));
        assert_eq!(found(host, "/home/me/app/"), Some(id));
        // Unix paths are case-sensitive; other hosts are other projects.
        assert_eq!(found(host, "/home/me/App"), None);
        assert_eq!(found(Uuid::new_v4(), "/home/me/app"), None);
    }

    #[test]
    fn remote_project_at_is_case_and_separator_blind_on_windows() {
        let host = Uuid::new_v4();
        let mut ws = Workspace::default();
        let id = ws.add_project(remote(host, r"C:\Users\me\app"));
        let found = ws
            .remote_project_at(host, "c:/users/ME/app/", RemoteOs::Windows)
            .map(|p| p.id);
        assert_eq!(found, Some(id));
    }
}

#[cfg(test)]
mod identity_tests {
    use super::{Identity, RemoteHost, RemoteOs, SshAuth};
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

    /// A host saved before Windows support existed must load as Unix and keep
    /// its exact behavior — the back-compat rule for every persisted struct.
    #[test]
    fn host_saved_without_an_os_field_loads_as_unix() {
        let json = r#"{
            "id": "11111111-1111-4111-8111-111111111111",
            "name": "dev",
            "hostname": "example.com",
            "user": "me",
            "auth": "Agent",
            "default_use_tmux": true
        }"#;
        let h: RemoteHost = serde_json::from_str(json).unwrap();
        assert_eq!(h.os, RemoteOs::Unix);
        assert_eq!(h.windows_shell, crate::winshell::WindowsShell::PowerShell);
        // And the tmux decision it had before is untouched.
        assert!(h.use_tmux(false));
    }

    #[test]
    fn host_os_round_trips() {
        let mut h = RemoteHost::new("win", "win.example.com");
        h.os = RemoteOs::Windows;
        h.windows_shell = crate::winshell::WindowsShell::Pwsh;
        let back: RemoteHost = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(back.os, RemoteOs::Windows);
        assert_eq!(back.windows_shell, crate::winshell::WindowsShell::Pwsh);
    }

    /// Switching a host to Windows must not have to also clear `default_use_tmux`
    /// — the gate answers for the stale value.
    #[test]
    fn windows_host_ignores_a_stale_saved_tmux_preference() {
        let mut h = RemoteHost::new("win", "w");
        h.default_use_tmux = true;
        h.os = RemoteOs::Windows;
        assert!(!h.use_tmux(true));
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

#[cfg(test)]
mod build_instance_tests {
    use super::{AgentPreset, InjectionMode, Loop, LoopSchedule, PostRunAction, Runner};
    use uuid::Uuid;

    fn agent() -> AgentPreset {
        let mut p = AgentPreset::shell();
        p.id = Uuid::from_u128(0xA6);
        p.name = "Claude".to_string();
        p.program = Some("claude".to_string());
        p.args = vec!["--verbose".to_string()];
        p.system_prompt = Some("preset prompt".to_string());
        p.injection = InjectionMode::CliFlag {
            flag: "--append-system-prompt".to_string(),
        };
        p
    }

    fn runner(prompt: &str) -> Runner {
        Runner {
            id: Uuid::from_u128(0x11),
            name: "Review".to_string(),
            preset_id: None,
            auto_mode_presses: 3,
            prompt: prompt.to_string(),
        }
    }

    #[test]
    fn runner_build_instance_matches_run_runner_inner() {
        let pid = Uuid::from_u128(0x50);
        let inst = runner("P {{input}}").build_instance(pid, &agent(), "fix the bug");
        assert_eq!(inst.project_id, pid);
        assert_eq!(inst.system_prompt.as_deref(), Some("P fix the bug"));
        assert_eq!(inst.injection, InjectionMode::TypeIn);
        assert_eq!(inst.auto_mode_presses, 3);
        assert_eq!(inst.custom_name.as_deref(), Some("Review"));
        assert!(inst.is_runner);
        assert!(inst.auto_submit);
        // Everything else comes from the preset, as `Instance::from_preset`.
        assert_eq!(inst.preset_id, Some(Uuid::from_u128(0xA6)));
        assert_eq!(inst.preset, "Claude");
        assert_eq!(inst.program.as_deref(), Some("claude"));
        assert_eq!(inst.args, vec!["--verbose".to_string()]);
        assert_ne!(inst.id, Uuid::from_u128(0x11));
    }

    #[test]
    fn runner_build_instance_trims_trailing_blank_lines() {
        let inst = runner("Review this.\n\n{{input}}").build_instance(Uuid::nil(), &agent(), "");
        assert_eq!(inst.system_prompt.as_deref(), Some("Review this."));
    }

    #[test]
    fn runner_build_instance_appends_details_without_placeholder() {
        let with = runner("Do it").build_instance(Uuid::nil(), &agent(), "more");
        assert_eq!(with.system_prompt.as_deref(), Some("Do it\n\nmore"));
        let without = runner("Do it").build_instance(Uuid::nil(), &agent(), "");
        assert_eq!(without.system_prompt.as_deref(), Some("Do it"));
    }

    #[test]
    fn runner_build_instance_replaces_every_placeholder() {
        let inst = runner("{{input}} and {{input}}").build_instance(Uuid::nil(), &agent(), "x");
        assert_eq!(inst.system_prompt.as_deref(), Some("x and x"));
    }

    fn lp(prompt: &str) -> Loop {
        Loop {
            id: Uuid::from_u128(0x22),
            name: "Nightly".to_string(),
            preset_id: None,
            project_id: Uuid::from_u128(0x51),
            auto_mode_presses: 2,
            prompt: prompt.to_string(),
            schedule: LoopSchedule::EveryMinutes { minutes: 5 },
            post_run: PostRunAction::Exit,
            enabled: true,
            last_run: Some(10),
        }
    }

    #[test]
    fn loop_build_instance_matches_spawn_loop_agent() {
        let inst = lp("check {{input}}\n").build_instance(&agent());
        assert_eq!(inst.project_id, Uuid::from_u128(0x51));
        assert_eq!(inst.system_prompt.as_deref(), Some("check"));
        assert_eq!(inst.injection, InjectionMode::TypeIn);
        assert_eq!(inst.auto_mode_presses, 2);
        assert_eq!(inst.custom_name.as_deref(), Some("Nightly"));
        assert!(inst.is_runner);
        assert!(!inst.use_tmux);
        assert_eq!(inst.preset_id, Some(Uuid::from_u128(0xA6)));
        assert_eq!(inst.program.as_deref(), Some("claude"));
        assert_ne!(inst.id, Uuid::from_u128(0x22));
    }

    #[test]
    fn loop_build_instance_keeps_prompt_without_placeholder() {
        let inst = lp("  run the checks").build_instance(&agent());
        assert_eq!(inst.system_prompt.as_deref(), Some("  run the checks"));
    }
}
