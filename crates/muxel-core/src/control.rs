//! Outside control: the protocol behind `muxel ctl`, which lets another program on
//! this computer — an orchestrating agent such as Grok Bot — list muxel's projects,
//! panes and agents, see what each agent is doing, read what it was last asked and
//! what it answered, and type into it.
//!
//! This is the pure half. The running app (`muxel` crate, `control.rs`) listens on
//! a loopback port named in [`ENDPOINT_FILE`] and answers each [`Request`] from its
//! live state; the `muxel ctl` command line is the client. Parsing the command line,
//! resolving "which agent", naming keys, finding the question an agent is blocked
//! on and deciding when a turn is over all live here, deterministic and tested.

use crate::pane::PaneNode;
use crate::{AgentActivity, SplitDirection};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

mod new;
pub use new::{MAX_LABEL_CHARS, MAX_PROMPT_CHARS, NewOptions, NewPlacement, place_new_pane};
mod startup;
pub use startup::{StartupFailure, StartupTurn, StartupTurnOwner, StartupTurnState};
mod card;
pub use card::{CardPrompt, card_lines, card_prompt, card_value, is_hidden_char};
mod approval;
pub use approval::{
    APPROVAL_ARM_DELAY_MS, APPROVAL_TIMEOUT_MS, ApprovalQueue, CreationDecision, CreationPolicy,
    LaunchClass, PendingApproval, creation_decision, creation_refusal,
};

/// Endpoint and plain-request version. Guards and plain creation require v2.
pub const PROTOCOL_VERSION: u32 = 1;
/// An older v1 host ignores unknown fields but rejects this request version
/// before forwarding a guarded send to the app.
pub const GUARDED_SEND_VERSION: u32 = 2;
/// Model/effort fields must never be silently ignored by older creation hosts.
pub const PARAMETERIZED_NEW_VERSION: u32 = 3;

/// The file, in muxel's data directory, where the running app says how to reach
/// it. Owner-only: the token in it is the whole of the authorization.
pub const ENDPOINT_FILE: &str = "control.json";

/// The environment variable that tells a program muxel starts in a pane which
/// pane it runs in: the pane's `uuid`, which works as AGENT. Only a pane on this
/// computer has one; a remote pane is kept from taking one (see `ssh`).
pub const INSTANCE_ID_ENV: &str = "MUXEL_INSTANCE_ID";

/// How long after a prompt is sent an agent may take to visibly start working
/// before `wait` stops expecting it to.
pub const REPLY_GRACE_MS: i64 = 30_000;

/// The same for an answer to a question: an agent takes one in at once, and one
/// that goes straight back to idle ("No, and tell me what to do instead") never
/// shows a turn at all, so `wait` gives up expecting one sooner.
pub const ANSWER_GRACE_MS: i64 = 5_000;

/// Lines `screen` returns by default, and at most.
pub const SCREEN_LINES: usize = 60;
pub const SCREEN_LINES_MAX: usize = 2000;

/// How to reach the running app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub version: u32,
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// The running muxel binary — how an agent on a computer whose muxel lives
    /// somewhere unexpected finds the command to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
}

/// One request: one line of JSON on a fresh connection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub token: String,
    /// The calling pane, if this client runs inside one. A declared label,
    /// not an authenticated identity; older hosts ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_instance_id: Option<Uuid>,
    pub command: Command,
}

/// What a request asks for. The app answers each from its live state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Every project, with a count of its agents by status.
    Projects,
    /// The pane layout and every agent, of one project or all of them.
    Panes {
        #[serde(default)]
        project: Option<String>,
    },
    /// Open a terminal in a project's active tab group.
    New {
        project: String,
        #[serde(flatten)]
        options: NewOptions,
    },
    /// Poll a creation approval without keeping a connection open.
    NewStatus { request_id: Uuid },
    /// One agent's state, or every agent's.
    Status {
        #[serde(default)]
        agent: Option<String>,
    },
    /// An agent's last prompt, last reply and pending question.
    Show {
        agent: String,
        /// The whole last turn rather than only its final message.
        #[serde(default)]
        full: bool,
    },
    /// The raw text of an agent's terminal.
    Screen {
        agent: String,
        #[serde(default)]
        lines: Option<usize>,
    },
    /// Type a prompt and press Enter.
    Send {
        agent: String,
        text: String,
        /// Type even though the agent is working or blocked.
        #[serde(default)]
        force: bool,
        /// Refuse if the pane no longer has this terminal launch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_id: Option<Uuid>,
    },
    /// Pick a numbered option of the question the agent is blocked on.
    Answer {
        agent: String,
        option: String,
        /// Override the caller's own-pane guard only.
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    /// Press keys (see [`key_bytes`]).
    Keys {
        agent: String,
        keys: Vec<String>,
        /// Override the caller's own-pane guard only.
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Require explicit versions so older hosts refuse fields they cannot honor.
pub fn request_version(command: &Command) -> u32 {
    if matches!(command, Command::New { options, .. } if options.model.is_some() || options.effort.is_some())
    {
        return PARAMETERIZED_NEW_VERSION;
    }
    if matches!(
        command,
        Command::Send {
            launch_id: Some(_),
            ..
        } | Command::New { .. }
            | Command::NewStatus { .. }
    ) {
        GUARDED_SEND_VERSION
    } else {
        PROTOCOL_VERSION
    }
}

/// The reply to a [`Request`]: one line of JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Why it failed, when that is one of the [`ErrorCode`]s.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "known_code"
    )]
    pub code: Option<ErrorCode>,
    /// A stable detail of a refusal, when its code alone is too broad.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Present and true after a paste attempt. A writer error may leave partial input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pasted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            code: None,
            reason: None,
            pasted: None,
            error: None,
        }
    }

    pub fn err(error: impl Into<Error>) -> Self {
        let error = error.into();
        Self {
            ok: false,
            result: None,
            code: error.code,
            reason: error.reason,
            pasted: error.pasted,
            error: Some(error.message),
        }
    }

    /// The result, or why there is none.
    pub fn into_result(self) -> Result<Value, Error> {
        if self.ok {
            return Ok(self.result.unwrap_or(Value::Null));
        }
        Err(Error {
            code: self.code,
            reason: self.reason,
            pasted: self.pasted,
            message: self
                .error
                .unwrap_or_else(|| "muxel refused the request".into()),
        })
    }
}

/// A code from a newer muxel, which this one doesn't know, reads as none rather
/// than making the whole reply unreadable.
fn known_code<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<ErrorCode>, D::Error> {
    Ok(Option::<Value>::deserialize(de)?.and_then(|code| serde_json::from_value(code).ok()))
}

/// Why `muxel ctl` didn't type into an agent, or couldn't reach muxel, as a word a
/// script can act on; `error` says the same for people and may be reworded. Other
/// failures (a bad command line, no agent by that name) carry only `error`.
///
/// These refusals type nothing, except `input_changed`: its prompt was pasted,
/// but Enter was withheld. A lost reply after sending has an uncertain outcome;
/// inspect the agent before retrying.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Outside control was revoked before the mutation.
    ControlDisabled,
    /// Outside terminal creation is Off.
    CreationDisabled,
    /// Literal argv or a shell launch needs the shell/command grant.
    CommandPermissionRequired,
    /// The user denied this creation request.
    ApprovalDenied,
    /// No approval arrived within 120 seconds of admission.
    ApprovalTimeout,
    /// Only configured non-shell terminal presets accept user tasks.
    PromptUnsupported,
    /// A preset cannot safely express the requested model or effort override.
    ParameterUnsupported,
    /// The agent is working: `wait` for it (or `send --force`).
    Working,
    /// The agent is waiting on a question: `answer` it, or press `keys`.
    Blocked,
    /// The agent is still starting: try again in a moment.
    Starting,
    /// The pane isn't running this session, or its program failed to start.
    NotRunning,
    /// The agent's program has exited.
    Exited,
    /// Another request holds the shared tmux claim.
    ClaimedElsewhere,
    /// The pane's terminal was replaced since the caller inspected it.
    LaunchMismatch,
    /// The prompt was pasted but its delayed Enter was withheld.
    InputChanged,
    /// The caller selected its own pane without explicitly forcing the action.
    SelfTarget,
    /// `muxel ctl` found no muxel to ask: it isn't running, outside control is
    /// off, its `control.json` can't be read or reached, or a read-only creation
    /// status cannot be answered.
    HostUnavailable,
    /// The endpoint's owner process did not pass the liveness check.
    StaleEndpoint,
}

/// A failed request: what to tell the user, and its [`ErrorCode`] when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub code: Option<ErrorCode>,
    pub reason: Option<String>,
    pub pasted: Option<bool>,
    pub message: String,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            reason: None,
            pasted: None,
            message: message.into(),
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn after_paste(mut self) -> Self {
        self.pasted = Some(true);
        self
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self {
            code: None,
            reason: None,
            pasted: None,
            message,
        }
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Check immediately before pasting, including after asynchronous claim work.
/// `--force` only overrides busy state; it never overrides a launch mismatch.
pub fn check_launch_id(expected: Option<Uuid>, current: Option<Uuid>) -> Result<(), Error> {
    if let Some(expected) = expected
        && current != Some(expected)
    {
        return Err(Error::new(
            ErrorCode::LaunchMismatch,
            format!(
                "the agent's terminal was replaced since you saw launch_id {expected}: check `status` before sending again"
            ),
        ));
    }
    Ok(())
}

/// A pane cannot direct input at itself by accident. This uses the caller's
/// declared environment value for convenience, not as an authorization check.
pub fn check_self_target(caller: Option<Uuid>, target: Uuid, force: bool) -> Result<(), Error> {
    if caller == Some(target) && !force {
        return Err(Error::new(
            ErrorCode::SelfTarget,
            "this command targets your own pane; pass --force if you mean to type into it",
        ));
    }
    Ok(())
}

/// A successful send receipt, created only after the paste was admitted to the
/// PTY writer. Admission does not confirm delivery to or consumption by the agent.
pub fn pasted_prompt_receipt(pane: Uuid, launch: Uuid) -> Value {
    json!({ "sent": true, "pasted": true, "agent": short_id(pane), "launch_id": launch })
}

/// What an agent pane is doing, as `muxel ctl` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Generating or running tools.
    Working,
    /// Alive and waiting for a prompt.
    Idle,
    /// Waiting on a permission or choice prompt.
    Blocked,
    /// Finished a turn.
    Done,
    /// The program ended.
    Exited,
    /// Launching, or reconnecting to its host.
    Starting,
    /// In the layout, but not started this session.
    NotRunning,
    /// Could not be launched at all.
    Failed,
}

/// One pane's entry in `panes` / `status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Short id: the first eight hex digits of `uuid`.
    pub id: String,
    pub uuid: Uuid,
    pub name: String,
    pub project: String,
    pub project_id: String,
    /// `terminal`, `editor` or `browser`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    pub preset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A coding agent rather than a shell (or an editor/browser pane).
    pub is_agent: bool,
    /// `None` for editors and browsers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentState>,
    /// When the agent last started working, finished or blocked (unix ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_since: Option<i64>,
    /// This run of the pane's terminal: a new id each time muxel opens a terminal
    /// for the pane (starting or restarting its program, reconnecting or
    /// recovering its session, reopening its workspace), so a caller can tell
    /// that what it saw may be out of date. An agent kept in tmux can still be in
    /// the same conversation under a new id, and an agent that changes
    /// conversation by itself (Claude's `/resume`) keeps its id. `None` while it
    /// isn't running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<Uuid>,
    /// A prompt sent through `muxel ctl` whose turn hasn't ended yet.
    pub awaiting_reply: bool,
    /// A startup task that failed before provider acceptance. The terminal itself
    /// may still be running, for example at a folder-trust menu.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_error: Option<String>,
    /// The pane the user has focused.
    pub focused: bool,
    /// The pane (tab group) it is a tab of, numbered in layout order from 1.
    /// `None` when it is popped out into its own window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Runs on a remote host over SSH.
    pub remote: bool,
}

/// An instance's short id, as tmux session names and `muxel ctl` show it.
pub fn short_id(id: Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

/// The latest lifecycle transition an agent has recorded (unix ms).
pub fn status_since(activity: &AgentActivity) -> Option<i64> {
    [
        activity.work_started_at,
        activity.completed_at,
        activity.blocked_at,
    ]
    .into_iter()
    .flatten()
    .max()
}

/// The [`AgentInfo::launch_id`] of an agent in `state` whose pane's terminal has
/// session id `terminal`: that id while the program runs, none while it starts
/// and none once it has exited, though its pane keeps the terminal open.
pub fn launch_id(state: Option<AgentState>, terminal: Option<Uuid>) -> Option<Uuid> {
    match state? {
        AgentState::Working | AgentState::Idle | AgentState::Blocked | AgentState::Done => terminal,
        AgentState::Starting | AgentState::Exited | AgentState::NotRunning | AgentState::Failed => {
            None
        }
    }
}

/// The value given to `flag` in `args` (`--model x` or `--model=x`).
pub fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let flag = flag.trim();
    if flag.is_empty() {
        return None;
    }
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == flag {
            return it.next().cloned();
        }
        if let Some(value) = arg.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            return Some(value.to_string());
        }
    }
    None
}

// --- Which agent ---------------------------------------------------------------

/// What an agent reference is matched against.
#[derive(Clone, Copy, Debug)]
pub struct AgentEntry<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub project: &'a str,
}

/// A name as it is matched: lowercase letters, digits and single spaces, so the
/// status glyphs agents put in their titles (`✳ Fix the pager`) don't count.
fn normalize(name: &str) -> String {
    let kept: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `query` can be an id: a full UUID, or at least four hex digits.
fn id_like(query: &str) -> bool {
    query.len() >= 4 && query.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn id_matches(id: Uuid, query: &str) -> bool {
    let query = query.replace('-', "").to_ascii_lowercase();
    id.simple().to_string().starts_with(&query)
}

/// Resolve an agent reference: `focused`, a UUID or short id (any unique prefix of
/// four or more hex digits), a name, or `project/name` when names repeat.
pub fn resolve_agent(
    query: &str,
    agents: &[AgentEntry],
    focused: Option<Uuid>,
) -> Result<Uuid, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("no agent given".into());
    }
    if matches!(query, "focused" | "active" | ".") {
        return focused
            .filter(|id| agents.iter().any(|a| a.id == *id))
            .ok_or_else(|| "no pane is focused".into());
    }
    if id_like(query) {
        let hits: Vec<&AgentEntry> = agents.iter().filter(|a| id_matches(a.id, query)).collect();
        match hits.as_slice() {
            [one] => return Ok(one.id),
            [] => {}
            many => return Err(ambiguous(query, many)),
        }
    }
    let (project, name) = match query.split_once('/') {
        Some((project, name)) => (Some(normalize(project)), normalize(name)),
        None => (None, normalize(query)),
    };
    let hits: Vec<&AgentEntry> = agents
        .iter()
        .filter(|a| normalize(a.name) == name)
        .filter(|a| project.as_ref().is_none_or(|p| normalize(a.project) == *p))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.id),
        [] => Err(format!(
            "no agent matches '{query}' — run `muxel ctl panes` to list them"
        )),
        many => Err(ambiguous(query, many)),
    }
}

fn ambiguous(query: &str, hits: &[&AgentEntry]) -> String {
    let list: Vec<String> = hits
        .iter()
        .map(|a| format!("{} ({} in {})", short_id(a.id), a.name, a.project))
        .collect();
    format!(
        "'{query}' matches more than one agent: {} — use an id",
        list.join(", ")
    )
}

/// Resolve a project reference: `active`, an id (unique prefix), or a name.
pub fn resolve_project(
    query: &str,
    projects: &[(Uuid, &str)],
    active: Option<Uuid>,
) -> Result<Uuid, String> {
    let query = query.trim();
    if matches!(query, "active" | "focused" | ".") {
        return active.ok_or_else(|| "no project is active".into());
    }
    if id_like(query) {
        let hits: Vec<Uuid> = projects
            .iter()
            .filter(|(id, _)| id_matches(*id, query))
            .map(|(id, _)| *id)
            .collect();
        if let [one] = hits.as_slice() {
            return Ok(*one);
        }
    }
    let name = normalize(query);
    let hits: Vec<Uuid> = projects
        .iter()
        .filter(|(_, n)| normalize(n) == name)
        .map(|(id, _)| *id)
        .collect();
    match hits.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!(
            "no project matches '{query}' — run `muxel ctl projects` to list them"
        )),
        _ => Err(format!(
            "'{query}' matches more than one project — use its id"
        )),
    }
}

// --- The layout ------------------------------------------------------------------

/// A project's pane tree as JSON, and the pane (numbered in layout order from 1)
/// each instance is a tab of.
pub fn describe_layout(node: &PaneNode) -> (Value, HashMap<Uuid, usize>) {
    fn walk(node: &PaneNode, next: &mut usize, panes: &mut HashMap<Uuid, usize>) -> Value {
        match node {
            PaneNode::Leaf(leaf) => {
                *next += 1;
                for tab in &leaf.tabs {
                    panes.insert(*tab, *next);
                }
                json!({
                    "pane": *next,
                    "tabs": leaf.tabs.iter().map(|t| short_id(*t)).collect::<Vec<_>>(),
                    "active_tab": leaf.tabs.get(leaf.active).map(|t| short_id(*t)),
                })
            }
            PaneNode::Split {
                direction,
                sizes,
                children,
            } => json!({
                "split": match direction {
                    SplitDirection::Horizontal => "side_by_side",
                    SplitDirection::Vertical => "stacked",
                },
                "sizes": sizes,
                "children": children
                    .iter()
                    .map(|child| walk(child, next, panes))
                    .collect::<Vec<_>>(),
            }),
        }
    }
    let mut panes = HashMap::new();
    let value = walk(node, &mut 0, &mut panes);
    (value, panes)
}

// --- Keys --------------------------------------------------------------------------

/// The bytes a key name sends: `enter`, `esc`, `tab`, `shift-tab`, the arrows,
/// `backspace`, `delete`, `space`, `home`, `end`, `pageup`, `pagedown`,
/// `ctrl-<letter>`, or any single character. Arrows (and home/end) follow the
/// terminal's cursor-key mode, as a real keypress would.
pub fn key_bytes(key: &str, app_cursor: bool) -> Option<Vec<u8>> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(c.to_string().into_bytes());
    }
    let arrow = |c: char| {
        let lead = if app_cursor { "\x1bO" } else { "\x1b[" };
        format!("{lead}{c}").into_bytes()
    };
    let name = key.trim().to_ascii_lowercase().replace('_', "-");
    let bytes = match name.as_str() {
        "enter" | "return" => b"\r".to_vec(),
        "esc" | "escape" => b"\x1b".to_vec(),
        "tab" => b"\t".to_vec(),
        "shift-tab" | "backtab" => b"\x1b[Z".to_vec(),
        "up" => arrow('A'),
        "down" => arrow('B'),
        "right" => arrow('C'),
        "left" => arrow('D'),
        "home" => arrow('H'),
        "end" => arrow('F'),
        "backspace" => b"\x7f".to_vec(),
        "delete" | "del" => b"\x1b[3~".to_vec(),
        "pageup" | "page-up" => b"\x1b[5~".to_vec(),
        "pagedown" | "page-down" => b"\x1b[6~".to_vec(),
        "space" => b" ".to_vec(),
        _ => {
            let letter = name.strip_prefix("ctrl-")?;
            let mut it = letter.chars();
            match (it.next(), it.next()) {
                (Some(c), None) if c.is_ascii_lowercase() => vec![c as u8 - b'a' + 1],
                _ => return None,
            }
        }
    };
    Some(bytes)
}

/// Whether pressing `key` submits what's typed.
pub fn key_submits(key: &str) -> bool {
    matches!(key.trim().to_ascii_lowercase().as_str(), "enter" | "return")
}

// --- The question an agent is blocked on ------------------------------------------

/// A numbered choice in an agent's prompt (`❯ 1. Yes`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub key: String,
    pub label: String,
    /// The option the prompt's cursor is on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub selected: bool,
}

/// What a blocked agent is asking, read off its screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub text: String,
    pub options: Vec<QuestionOption>,
}

/// Glyphs a prompt puts in front of the option its cursor is on.
const SELECT_GLYPHS: &[char] = &['❯', '>', '›', '▶', '→', '➜'];
/// How far up from the bottom of the screen a prompt's options are looked for.
const QUESTION_REACH: usize = 30;
/// Lines of question text kept above the options (or of screen, with none).
const QUESTION_LINES: usize = 14;

fn is_box_char(c: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&c)
}

/// `line` without the box frame some prompts are drawn in, or its indent.
fn unbox(line: &str) -> &str {
    line.trim()
        .trim_start_matches(['│', '┃', '║'])
        .trim_end_matches(['│', '┃', '║'])
        .trim()
}

/// A horizontal rule or a frame's top or bottom edge (`────`, `╭──╮`) — not an
/// empty line inside a frame (`│    │`), which is only its sides.
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars().all(|c| is_box_char(c) || c.is_whitespace())
        && t.chars()
            .any(|c| is_box_char(c) && !matches!(c, '│' | '┃' | '║'))
}

/// `❯ 1. Yes` → ("1", "Yes", selected).
fn option_line(line: &str) -> Option<QuestionOption> {
    let s = unbox(line);
    let (selected, s) = match s.strip_prefix(SELECT_GLYPHS) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, s),
    };
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let rest = s[digits.len()..].strip_prefix(['.', ')'])?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let label = rest.trim();
    (!label.is_empty()).then(|| QuestionOption {
        key: digits,
        label: label.to_string(),
        selected,
    })
}

/// The question at the bottom of a blocked agent's screen: its numbered options,
/// counted back from the last one to `1`, and the text above them up to the rule
/// the prompt is drawn under. A prompt with no numbered options (`Allow? (y/n)`)
/// comes back as the last lines of the screen, `None` when there are none.
pub fn pending_question(screen: &str) -> Option<Question> {
    let lines: Vec<&str> = screen.lines().collect();
    let bottom = lines.iter().rposition(|l| !l.trim().is_empty())?;
    let reach = bottom.saturating_sub(QUESTION_REACH);

    let last = (reach..=bottom)
        .rev()
        .find(|&i| option_line(lines[i]).is_some());
    let mut options = Vec::new();
    let mut first = None;
    if let Some(last) = last {
        let mut gap = 0;
        let mut expect: Option<u32> = None;
        for i in (reach..=last).rev() {
            match option_line(lines[i]) {
                Some(opt) => {
                    let n: u32 = opt.key.parse().unwrap_or(0);
                    if expect.is_some_and(|e| n != e) {
                        break;
                    }
                    options.push(opt);
                    first = Some(i);
                    gap = 0;
                    if n <= 1 {
                        break;
                    }
                    expect = Some(n - 1);
                }
                // A label wrapped onto the next line, or a hint under an option.
                None if gap < 2 && !lines[i].trim().is_empty() && !is_rule(lines[i]) => gap += 1,
                None => break,
            }
        }
        options.reverse();
    }

    let text_end = first.unwrap_or(bottom + 1);
    let mut text: Vec<&str> = Vec::new();
    let mut blanks = 0;
    for i in (0..text_end).rev() {
        let line = lines[i];
        if is_rule(line) || text.len() >= QUESTION_LINES {
            break;
        }
        if unbox(line).is_empty() {
            blanks += 1;
            if blanks >= 2 && !text.is_empty() {
                break;
            }
            continue;
        }
        if blanks > 0 && !text.is_empty() {
            text.push("");
        }
        blanks = 0;
        text.push(unbox(line));
    }
    text.reverse();
    if text.is_empty() && options.is_empty() {
        return None;
    }
    Some(Question {
        text: text.join("\n"),
        options,
    })
}

// --- The last prompt -----------------------------------------------------------------

/// The last prompt the user sent in a Claude session transcript
/// (`~/.claude/projects/<slug>/<session>.jsonl`, or just its tail). Tool results,
/// meta entries, subagents' prompts and interruption notes are skipped; a slash
/// command comes back as `/name args`.
pub fn prompt_from_claude_transcript(jsonl: &str) -> Option<String> {
    let mut last = None;
    for line in jsonl.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("user")
            || entry.get("isMeta").and_then(Value::as_bool) == Some(true)
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let content = &entry["message"]["content"];
        let text = match content {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        if let Some(prompt) = user_prompt(&text) {
            last = Some(prompt);
        }
    }
    last
}

/// A user entry's text as the prompt it was, or `None` when it is bookkeeping.
fn user_prompt(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty()
        || text.starts_with("[Request interrupted")
        || text.starts_with("<local-command-")
        || text.starts_with("Caveat:")
    {
        return None;
    }
    if let Some(name) = between(text, "<command-name>", "</command-name>") {
        let args = between(text, "<command-args>", "</command-args>").unwrap_or("");
        let name = name.trim();
        let name = if name.starts_with('/') {
            name.to_string()
        } else {
            format!("/{name}")
        };
        return Some(format!("{name} {}", args.trim()).trim().to_string());
    }
    Some(text.to_string())
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

// --- Waiting for a turn -----------------------------------------------------------------

/// Whether a prompt (or answer) `muxel ctl` sent at `sent_at` (unix ms) is still
/// waiting on its turn to end. A working agent is mid-turn. A quiet one is done
/// with it once it has worked, finished or blocked since the prompt — or, having
/// shown no sign of starting, once `grace_ms` ([`REPLY_GRACE_MS`] or
/// [`ANSWER_GRACE_MS`]) has passed.
///
/// A blocked agent is asking something new only if it blocked after the prompt: a
/// block from before it is the question just answered, still on screen until the
/// agent redraws (or until the once-a-second status refresh catches up).
pub fn awaiting_reply(
    sent_at: Option<i64>,
    grace_ms: i64,
    state: Option<AgentState>,
    activity: &AgentActivity,
    now: i64,
) -> bool {
    let Some(sent) = sent_at else {
        return false;
    };
    let since = |at: Option<i64>| at.is_some_and(|at| at >= sent);
    match state {
        Some(AgentState::Working | AgentState::Starting) => true,
        Some(AgentState::Idle | AgentState::Done | AgentState::Blocked) => {
            !(since(activity.work_started_at)
                || since(activity.completed_at)
                || since(activity.blocked_at))
                && now - sent < grace_ms
        }
        _ => false,
    }
}

/// Why `wait` stopped waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitOutcome {
    /// The startup task never obtained provider-owned acceptance.
    SubmissionFailed,
    /// The turn ended; the reply is ready.
    Finished,
    /// The agent is asking something (see `question`).
    Blocked,
    /// The program ended or failed to start.
    Exited,
    /// The pane isn't running this session.
    NotRunning,
    /// `--timeout` passed with the agent still working.
    TimedOut,
}

/// Whether `wait` is over for an agent in `state`, and why. `None` keeps waiting.
pub fn wait_outcome(state: Option<AgentState>, awaiting: bool) -> Option<WaitOutcome> {
    match state? {
        AgentState::Blocked => (!awaiting).then_some(WaitOutcome::Blocked),
        AgentState::Exited | AgentState::Failed => Some(WaitOutcome::Exited),
        AgentState::NotRunning => Some(WaitOutcome::NotRunning),
        AgentState::Working | AgentState::Starting => None,
        AgentState::Idle | AgentState::Done => (!awaiting).then_some(WaitOutcome::Finished),
    }
}

// --- Sharing an agent with muxel on other computers -----------------------------------

/// The tmux session option in which a muxel records that it has just typed a
/// prompt or answer into the agent. The session lives wherever the agent runs, so
/// every muxel attached to it — a remote project opened on several computers, or a
/// local one a peer attached to over SSH — reads the same value.
pub const CLAIM_OPTION: &str = "@muxel-ctl";

/// Which muxel last typed into an agent through `muxel ctl`, and when: stored on
/// the agent's tmux session as `v1|owner|host|at|grace[|nonce]`. Older readers
/// ignore the optional suffix; their claims remain readable without it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// A random id for the muxel process (not secret; it only tells muxels apart).
    pub owner: String,
    /// The computer that muxel runs on, for the message another one shows.
    pub host: String,
    /// When the prompt or answer was submitted (unix ms, the owner's clock).
    pub at: i64,
    /// [`REPLY_GRACE_MS`] or [`ANSWER_GRACE_MS`], as for [`awaiting_reply`].
    pub grace_ms: i64,
    /// Unique per send, including concurrent sends from the same process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<Uuid>,
}

fn claim_field(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '|' || c.is_whitespace() || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect()
}

impl Claim {
    pub fn encode(&self) -> String {
        let mut value = format!(
            "v1|{}|{}|{}|{}",
            claim_field(&self.owner),
            claim_field(&self.host),
            self.at,
            self.grace_ms
        );
        if let Some(nonce) = self.nonce {
            value.push('|');
            value.push_str(&nonce.to_string());
        }
        value
    }

    /// Both admission and cleanup must distinguish same-process, same-ms sends.
    pub fn same_turn(&self, other: &Self) -> bool {
        self.owner == other.owner && self.nonce.is_some() && self.nonce == other.nonce
    }

    pub fn pattern(owner: &str, nonce: Uuid) -> String {
        format!("v1|{}|*|*|*|{nonce}", claim_field(owner))
    }

    /// A safe tmux format pattern for comparing the claim before replacing it.
    /// A nonce identifies modern sends even when a hostname needs quoting;
    /// legacy claims need every encoded byte to be literal and format-safe.
    pub fn compare_pattern(&self) -> Option<String> {
        if let Some(nonce) = self.nonce {
            let owner = claim_field(&self.owner);
            owner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                .then(|| Self::pattern(&self.owner, nonce))
        } else {
            let value = self.encode();
            value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "|-_.".contains(c))
                .then_some(value)
        }
    }

    /// `None` for an unset option, or a value this version doesn't understand.
    pub fn decode(value: &str) -> Option<Self> {
        let mut parts = value.trim().split('|');
        if parts.next()? != "v1" {
            return None;
        }
        let owner = parts.next()?.to_string();
        let host = parts.next()?.to_string();
        let at = parts.next()?.parse().ok()?;
        let grace_ms = parts.next()?.parse().ok()?;
        let nonce = parts.next().map(Uuid::parse_str).transpose().ok()?;
        (!owner.is_empty()).then_some(Self {
            owner,
            host,
            at,
            grace_ms,
            nonce,
        })
    }
}

/// Whether another muxel's claim still holds the agent: its turn hasn't visibly
/// ended, judged from what this muxel sees by the same rule as [`awaiting_reply`].
/// A claim left by a muxel that quit or crashed lapses the same way — when the
/// agent is seen to finish, or when its grace runs out without a turn starting.
/// (The claim's time is the other computer's clock; they're assumed to agree to
/// within a second or so, as NTP keeps them.)
pub fn claim_holds(
    claim: &Claim,
    me: &str,
    state: Option<AgentState>,
    activity: &AgentActivity,
    now: i64,
) -> bool {
    claim.owner != me && awaiting_reply(Some(claim.at), claim.grace_ms, state, activity, now)
}

// --- The command line --------------------------------------------------------------

/// What `muxel ctl …` asked for.
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "only one command is parsed per CLI invocation; boxing adds an allocation without reducing a collection"
)]
pub enum CliCommand {
    /// A request the app answers directly.
    Request(Command),
    /// Poll an agent until its turn ends, then show it.
    Wait {
        agent: String,
        timeout_secs: u64,
        full: bool,
    },
    /// Print the instructions for an orchestrating agent.
    Skill,
    Help(Option<String>),
}

/// `wait`'s default patience.
pub const WAIT_TIMEOUT_SECS: u64 = 600;

/// Usage text for `muxel ctl help`.
pub const USAGE: &str = "\
usage: muxel ctl <command> [args]

  projects                       list projects
  panes [PROJECT]                pane layout and agents (all projects, or one)
  new PROJECT [--cwd DIR] [--name TEXT] [--tab | --split right|down]
              [--focus] [--prompt TEXT] [--from LABEL] [--no-wait]
              [--preset NAME] [--model MODEL] [--effort LEVEL] [-- PROGRAM [ARGS...]]
                                 open in background; --focus selects and raises;
                                 --background is a compatibility no-op; --prompt - reads stdin
  new-status REQUEST_ID          poll a pending creation's final reply
  status [AGENT]                 an agent's status (or every agent's)
  show AGENT [--full]            last prompt, last reply, pending question
  screen AGENT [--lines N]       raw terminal text (default 60 lines)
  send AGENT TEXT [--force] [--launch-id ID]
                                 type TEXT and press Enter ('-' reads stdin);
                                 --launch-id refuses a replaced terminal
  wait AGENT [--timeout SECS] [--full]
                                 wait for the turn to end, then show it
  answer AGENT OPTION [--force]  pick a numbered option of a pending question
  keys AGENT KEY... [--force]    press keys: enter esc tab shift-tab up down
                                 left right backspace space ctrl-c, or a character
  skill                          instructions for an orchestrating agent
  help [COMMAND]                 show general or command-specific help

AGENT is an id from `panes`, a name, project/name, or `focused`.
send, answer and keys refuse the caller's own pane unless --force is explicit.
Everything after `--` is taken literally (for text that starts with -).
For `new` without --prompt, presets/defaults keep toolbar injection, resume and hooks; raw argv bypasses them.
ALL `new` requests require outside control AND Settings > Grok Bot > Outside terminal creation set to Ask or Allow.
Ask requires a click for every request. Allow covers configured non-shell terminal presets.
Literal argv and shell presets always Ask; the click card never takes keyboard focus.
Each card disables Allow and Deny for 750 ms after appearing and ignores double-clicks.
Both mouse-down and release must belong to that same armed card.
`--prompt` submits a user task through the one-shot runner path, once on first launch; only non-shell terminal presets support it.
It replaces the preset's configured system prompt with the task; preset arguments are kept.
The typed task stays exact; automatic instructions use a separate flag/config channel when available.
In Git Bash, set MSYS_NO_PATHCONV=1 for leading-slash prompts, or use --prompt - with stdin.
Its task text is discarded after the first launch and is not re-typed on a later launch.
Ask returns a pending request id immediately; the CLI polls once per second until the final reply.
Connection and host-availability failures retry up to three times against the same id; creation is never reissued.
Uncoded status errors also name the id to query with `new-status`; do not repeat `new` while the original may still wait.
`--no-wait` returns that id for scripts; poll with `new-status REQUEST_ID`. Requests expire after 120 seconds.
`--from` and the caller's MUXEL_INSTANCE_ID are declared labels, not verified identities.
All raw argv (even agent argv) and shell presets/defaults ALSO require \"Also allow typing into shell panes\".
All three grants default off; upgrades do not infer creation permission from the older grants.
Creation opt-out or outside control off refuses all pending requests. Existing panes, admitted launches and typing permissions are unchanged; toolbar creation is unaffected.
Outside control off stops the server; pollers report host_unavailable after their retries.
Commands run as the host muxel process, outside the caller's sandbox; agents/presets are not a sandbox.
Turning \"Also allow typing into shell panes\" off blocks subsequent control writes to all raw-argv panes and shell-classified panes.
Configured non-literal agents remain controllable while outside control is on. Revocation does not stop processes or undo input.
Raw argv panes restore stopped; only Run again replays their saved command.
`new` launch directories are never owned, registered or deleted by muxel.
On Windows, raw argv requires a native executable; use --preset for batch wrappers.
`new` opens terminal panes; browser presets (including a browser default) are not supported yet.
muxel must be running with Settings > Grok Bot > \"Allow outside tools to control muxel\" on.
To reach a muxel started with --user-data-dir DIR, run `muxel --user-data-dir DIR ctl ...`;
its own panes reach it without the flag.

Replies are JSON. A failure is {\"ok\": false, \"error\": ...}, with a \"code\" when a
script can act on it (see `muxel ctl skill`). Exit status: 0 success,
1 request failure (including no running muxel), 2 invalid command line or stdin.";

pub fn command_help(command: &str) -> Option<&'static str> {
    Some(match command {
        "projects" => "usage: muxel ctl projects\nList every project and its agent counts.",
        "panes" => {
            "usage: muxel ctl panes [PROJECT]\nShow pane layout and agents in all projects or one project."
        }
        "new" => {
            "usage: muxel ctl new PROJECT [--cwd DIR] [--name TEXT] [--tab | --split right|down] [--focus] [--prompt TEXT] [--from LABEL] [--no-wait] [--preset NAME] [--model MODEL] [--effort LEVEL] [-- PROGRAM [ARGS...]]\nOpen a terminal. --prompt - reads stdin; -- starts literal program arguments. Creation permissions apply."
        }
        "new-status" => {
            "usage: muxel ctl new-status REQUEST_ID\nPoll a pending creation by its full request UUID."
        }
        "status" => {
            "usage: muxel ctl status [AGENT]\nShow one agent's status or every agent's status."
        }
        "show" => {
            "usage: muxel ctl show AGENT [--full]\nShow the last prompt, reply and pending question; --full includes the whole reply."
        }
        "screen" => {
            "usage: muxel ctl screen AGENT [--lines N]\nShow raw terminal text; default 60 lines, maximum 2000."
        }
        "send" => {
            "usage: muxel ctl send AGENT TEXT [--force] [--launch-id ID]\nPaste TEXT and press Enter. '-' reads stdin; `--` makes following text literal, including -h and --help. --force overrides busy and own-pane refusal, but never a launch mismatch."
        }
        "wait" => {
            "usage: muxel ctl wait AGENT [--timeout SECS] [--full]\nWait for the turn to finish, block, exit or time out, then show its result."
        }
        "answer" => {
            "usage: muxel ctl answer AGENT OPTION [--force]\nPick a numbered option of a pending question. --force permits your own pane."
        }
        "keys" => {
            "usage: muxel ctl keys AGENT KEY... [--force]\nPress keys such as enter, esc, tab, arrows, backspace and ctrl-c. --force permits your own pane."
        }
        "skill" => "usage: muxel ctl skill\nPrint instructions for an orchestrating agent.",
        "help" => {
            "usage: muxel ctl help [COMMAND]\nShow general or command-specific help. Use `--` before literal prompt text that starts with -."
        }
        _ => return None,
    })
}

fn unknown_command_error(command: &str) -> String {
    let suggestion = if command == "list" {
        Some("panes")
    } else {
        const VERBS: &[&str] = &[
            "projects",
            "panes",
            "new",
            "new-status",
            "status",
            "show",
            "screen",
            "send",
            "wait",
            "answer",
            "keys",
            "skill",
            "help",
        ];
        VERBS
            .iter()
            .map(|verb| (*verb, edit_distance(command, verb)))
            .min_by_key(|(_, distance)| *distance)
            .filter(|(_, distance)| *distance <= 3)
            .map(|(verb, _)| verb)
    };
    let hint = suggestion.map_or(String::new(), |verb| format!("\ndid you mean `{verb}`?"));
    format!("unknown command '{command}'{hint}\n\n{USAGE}")
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (row, ch) in left.chars().enumerate() {
        let mut current = vec![row + 1; right.len() + 1];
        for (col, other) in right.iter().enumerate() {
            current[col + 1] = (previous[col + 1] + 1)
                .min(current[col] + 1)
                .min(previous[col] + usize::from(ch != *other));
        }
        previous = current;
    }
    previous[right.len()]
}

/// Parse the arguments after `muxel ctl`.
pub fn parse_cli(args: &[String]) -> Result<CliCommand, String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Ok(CliCommand::Help(None));
    };
    if matches!(cmd.as_str(), "help" | "-h" | "--help") {
        return match rest {
            [] => Ok(CliCommand::Help(None)),
            [target] if command_help(target).is_some() => {
                Ok(CliCommand::Help(Some(target.clone())))
            }
            [target] => Err(unknown_command_error(target)),
            _ => Err("usage: muxel ctl help [COMMAND]".into()),
        };
    }
    if rest
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        return if command_help(cmd).is_some() {
            Err(format!(
                "{cmd} was not run: use `muxel ctl help {cmd}` for help, or `--` before literal -h/--help text"
            ))
        } else {
            Err(unknown_command_error(cmd))
        };
    }
    if cmd == "new" {
        return new::parse(rest);
    }
    if cmd == "new-status" {
        return match rest {
            [id] => Ok(CliCommand::Request(Command::NewStatus {
                request_id: id
                    .parse()
                    .map_err(|_| "new-status needs a full request UUID")?,
            })),
            _ => Err("usage: muxel ctl new-status REQUEST_ID".into()),
        };
    }
    let mut words: Vec<String> = Vec::new();
    let mut full = false;
    let mut force = false;
    let mut launch_id: Option<Uuid> = None;
    let mut lines: Option<usize> = None;
    let mut timeout: Option<u64> = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--" => {
                words.extend(it.by_ref().cloned());
                break;
            }
            "--full" => full = true,
            "--force" => force = true,
            "--launch-id" => {
                let value = it.next().ok_or("--launch-id needs an ID")?;
                launch_id = Some(Uuid::parse_str(value).map_err(|_| "--launch-id needs a UUID")?);
            }
            "--lines" => lines = Some(number(it.next(), "--lines")?),
            "--timeout" => timeout = Some(number(it.next(), "--timeout")?),
            flag if flag.starts_with("--") && flag.len() > 2 => {
                return Err(format!("unknown option {flag}"));
            }
            _ => words.push(arg.clone()),
        }
    }
    if launch_id.is_some() && cmd != "send" {
        return Err("--launch-id is only valid with send".into());
    }
    let agent = |words: &[String]| -> Result<String, String> {
        words
            .first()
            .cloned()
            .ok_or_else(|| format!("`{cmd}` needs an AGENT (an id from `muxel ctl panes`)"))
    };
    let at_most = |n: usize| -> Result<(), String> {
        if words.len() > n {
            Err(format!("unexpected argument '{}'", words[n]))
        } else {
            Ok(())
        }
    };
    let command = match cmd.as_str() {
        "projects" => {
            at_most(0)?;
            Command::Projects
        }
        "panes" => {
            at_most(1)?;
            Command::Panes {
                project: words.first().cloned(),
            }
        }
        "status" => {
            at_most(1)?;
            Command::Status {
                agent: words.first().cloned(),
            }
        }
        "show" => {
            at_most(1)?;
            Command::Show {
                agent: agent(&words)?,
                full,
            }
        }
        "screen" => {
            at_most(1)?;
            Command::Screen {
                agent: agent(&words)?,
                lines,
            }
        }
        "send" => {
            let agent = agent(&words)?;
            let text = words[1..].join(" ");
            if text.is_empty() {
                return Err("`send` needs the TEXT to type ('-' reads it from stdin)".into());
            }
            Command::Send {
                agent,
                text,
                force,
                launch_id,
            }
        }
        "answer" => {
            at_most(2)?;
            let agent = agent(&words)?;
            let option = words
                .get(1)
                .cloned()
                .ok_or("`answer` needs the OPTION number to pick")?;
            Command::Answer {
                agent,
                option,
                force,
            }
        }
        "keys" => {
            let agent = agent(&words)?;
            let keys = words[1..].to_vec();
            if keys.is_empty() {
                return Err("`keys` needs at least one KEY".into());
            }
            if let Some(bad) = keys.iter().find(|k| key_bytes(k, false).is_none()) {
                return Err(format!("unknown key '{bad}'"));
            }
            Command::Keys { agent, keys, force }
        }
        "wait" => {
            at_most(1)?;
            return Ok(CliCommand::Wait {
                agent: agent(&words)?,
                timeout_secs: timeout.unwrap_or(WAIT_TIMEOUT_SECS),
                full,
            });
        }
        "skill" => return Ok(CliCommand::Skill),
        other => return Err(unknown_command_error(other)),
    };
    Ok(CliCommand::Request(command))
}

fn number<T: std::str::FromStr>(value: Option<&String>, flag: &str) -> Result<T, String> {
    value
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("{flag} needs a number"))
}

// --- Instructions for an orchestrating agent -----------------------------------------

/// The skill an orchestrating agent (Grok Bot, …) is given so it can drive muxel:
/// how to find muxel on whichever computer it is working on, the commands, what the
/// statuses mean, and the rules for using them. `exe` is the muxel binary on the
/// computer it was made on — the first place to look, not the only one, since one
/// skill may serve agents on several computers.
pub fn skill(exe: &str) -> String {
    SKILL.replace("MUXEL_PATH", &crate::ssh::sh_quote(exe))
}

/// `markdown` with its hard line breaks inside paragraphs and list items joined,
/// for showing in a box of any width: text wrapped at 80 columns and shown in a
/// narrower one breaks raggedly, a short line after every long one. Headings, list
/// items, rules, front matter and indented code keep their own lines.
pub fn reflow_for_display(markdown: &str) -> String {
    #[derive(PartialEq)]
    enum Last {
        Blank,
        Text,
        Fixed,
        Code,
    }
    let mut out: Vec<String> = Vec::new();
    let mut last = Last::Blank;
    let mut front_matter = false;
    for (i, line) in markdown.lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let starts_item = trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed
                .split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if trimmed.is_empty() {
            out.push(String::new());
            last = Last::Blank;
        } else if trimmed == "---" {
            // Front matter opens with a rule on the first line and ends at the next.
            front_matter = i == 0;
            out.push(line.to_string());
            last = Last::Fixed;
        } else if front_matter || trimmed.starts_with('#') {
            out.push(line.to_string());
            last = Last::Fixed;
        } else if indent >= 4 && matches!(last, Last::Blank | Last::Code) {
            out.push(line.to_string());
            last = Last::Code;
        } else if last == Last::Text && !starts_item {
            let joined = out.last_mut().expect("text follows a line");
            joined.push(' ');
            joined.push_str(trimmed);
        } else {
            out.push(line.to_string());
            last = Last::Text;
        }
    }
    out.join("\n")
}

const SKILL: &str = r#"---
name: muxel
description: Drive the coding agents running in muxel on this computer - list its projects, panes and agents, see which are working, done or waiting on a question, read an agent's last prompt and reply, send it a prompt, and answer its questions.
---

# muxel

muxel is a desktop app on the user's computer that runs coding agents (Claude,
Codex, opencode, Grok, ...) and shells side by side, grouped into projects. You
control it with its `ctl` command, run on the **local computer** (not a cloud
computer). The user may run muxel on several computers; each one's `muxel ctl`
only reaches the muxel on that computer.

## Find muxel on this computer

Do this once on each computer. Below, `muxel` means the command you found.

1. `MUXEL_PATH` - where muxel was when these instructions were made. Use it if
   it exists on this computer.
2. Otherwise read `exe` from muxel's control file, which muxel writes while it is
   running with outside control on:
   - macOS: `~/Library/Application Support/dev.muxel.muxel/control.json`
   - Linux: `$XDG_DATA_HOME/muxel/control.json` (usually
     `~/.local/share/muxel/control.json`)
   - Windows: `%APPDATA%\muxel\muxel\data\control.json`
3. Otherwise look where it is usually installed:
   - Linux, from the .deb or .rpm package: `/usr/bin/muxel` (on the PATH as
     `muxel`).
   - Linux AppImage: the `.AppImage` file itself, wherever the user saved it
     (often `~/Applications` or `~/Downloads`, named like
     `muxel-linux-x86_64.AppImage`). Run it like any command:
     `~/Applications/muxel-linux-x86_64.AppImage ctl projects`.
   - Linux tarball or install script: `~/.local/bin/muxel` or
     `/usr/local/bin/muxel`.
   - macOS: `/Applications/muxel.app/Contents/MacOS/muxel`.
   - Anywhere: `muxel` on the PATH.

Then run `muxel ctl projects`. Every reply includes `host`: the name of the
computer whose muxel answered. If you work with more than one computer, check
it's the one you meant.

If muxel won't run:

- An AppImage that says it needs FUSE: put `APPIMAGE_EXTRACT_AND_RUN=1` in front
  of the command (slower, but works without FUSE).
- `error while loading shared libraries` or `symbol lookup error`: the library
  path you run commands with (often set when you are an AppImage yourself) doesn't
  suit muxel. Run it as `env -u LD_LIBRARY_PATH muxel ctl ...`.
- It says muxel isn't running when the user says it is: your commands may see a
  different home directory than muxel does. Find its control file (step 2) and
  put `MUXEL_CONTROL=/path/to/control.json` in front of the command.

Every command prints JSON. A failure exits non-zero and prints
`{"ok": false, "error": "..."}` - the error says what went wrong and what to do,
and a `code` is added when there is something to act on (see Errors below). If
it says muxel isn't running or outside control is off, ask the user to open
muxel and turn on Settings > Grok Bot > "Allow outside tools to control muxel".

## Commands

- `muxel ctl projects` - every project: id, name, folder, git branch, and how
  many agents are working / blocked / done / idle.
- `muxel ctl panes [PROJECT]` - the pane layout and every agent in it: id, name,
  program, model, status, which pane it's in, and which one the user has focused.
- `muxel ctl new PROJECT [--cwd DIR] [--name TEXT] [--tab | --split right|down]
  [--focus] [--preset NAME] [--model MODEL] [--effort LEVEL] [-- PROGRAM [ARGS...]]` - open a terminal in the
  project's active group (a new tab by default). PROJECT resolves as for `panes`.
  DIR must be an existing absolute directory; it defaults to the project root
  and may be a linked worktree or another directory. A preset matches its name
  or id; omitting both launch choices uses the toolbar's preset for that project.
  `--model` and `--effort` override the preset's values for this pane, including
  prompt launches; restarts retain the chosen arguments. Values must contain
  1–64 characters from `[A-Za-z0-9._:/@+-]` with no leading `-`. Each override
  requires its Model flag / Effort flag in Settings > Agents, and that flag must
  be absent from Extra args. A flag containing `{}` emits one argument with the
  value substituted; other flags emit flag then value. Existing presets are not
  migrated; fill in their flag fields. Literal `-- PROGRAM` cannot use overrides.
  New Codex presets use `--config=model_reasoning_effort={}`; existing `-c` or
  `--config` arguments setting that key also conflict with an effort override.
  Unsupported combinations receive `parameter_unsupported`. Parameterized
  presets remain eligible for Allow; Ask shows the requested model and effort.
  Everything after `--` is literal argv, including spaces, quotes and empty
  arguments. `--prompt TEXT` sends a user task through the existing runner path;
  `--prompt -` reads stdin. In Git Bash, set `MSYS_NO_PATHCONV=1` for leading-slash
  prompts, or use stdin. This replaces the preset's configured system prompt
  with the task; preset arguments are kept. The typed task stays exact.
  Automatic instructions use the preset's separate flag/config transport when
  available; they are omitted for TypeIn-only presets. The task is submitted
  once and its text is discarded after first launch. Restart, restore and resume neither
  re-type nor resubmit it, including launches without a resumable conversation.
  Prompts for literal, shell and browser launches receive
  `prompt_unsupported`. Creation preserves the selected tab, focus, project and
  window by default, without raising the window. `--focus` selects the new pane
  and activates its window. `--background` is accepted as a compatibility no-op,
  including alongside `--focus`. JSON callers use `focus` (default false);
  the legacy `background` field remains accepted and does not control focus.
  Every `new` request requires outside control plus Settings > Grok Bot >
  Outside terminal creation set to Ask or Allow. Ask requires a click for every
  request; Allow permits configured non-shell terminal presets. All literal argv,
  even an agent
  executable, and shell presets/defaults additionally require "Also allow typing
  into shell panes". Configured non-literal presets not classified as shells need
  only the first two grants. Literal argv and shell presets always Ask, even
  under Allow. The existing shell/non-shell classifier is a soft
  policy for trusted configured presets, not an executable allowlist.
  All three permissions default off; upgrades leave creation off even if both
  older grants are on. Ask the user to enable the required settings themselves.
  Ask returns `{ "pending": true, "request_id": "UUID" }` immediately, without
  holding a connection. The CLI polls every second and prints the final reply.
  Temporary connection and host-availability failures retry up to three times
  against that same UUID; a successful pending reply resets the retry count.
  The CLI never reissues `new`. If retries fail, its error names the UUID to
  query with `new-status`. Uncoded status errors, including an older host's
  uncoded timeout, also give that advice. Do not repeat `new` while the original
  may still wait.
  `--no-wait` prints the pending reply for scripts; use `ctl new-status UUID` to
  poll it. Requests expire 120 seconds after admission, including time behind
  another card. Each visible card disables both buttons for 750 ms and ignores
  double-clicks. Mouse-down and release must belong to the same armed card.
  The card offers click-only Allow once / Deny, keeps terminal
  keyboard focus, and shows the waiting count in FIFO order. `--from LABEL` and
  the caller's MUXEL_INSTANCE_ID are displayed as declared information, not
  authenticated identity (the token is the whole authorization).
  Final replies remain available for ten minutes, bounded to the latest 256;
  up to 64 approvals may wait. Approvals are runtime-only and do not survive a
  host restart. Plain creation and status requests use protocol v2; creation with
  model or effort uses v3 so older hosts cannot silently discard the choices.
  The host requires the exact command version, including for JSON callers.
  Turning creation or outside control off refuses all pending requests. Outside
  control off also stops the server, so pollers receive `host_unavailable`
  after their connection retries rather than the recorded `control_disabled`.
  Revocation leaves
  existing panes, already admitted launches and
  typing permissions unchanged. Deferred requests recheck current permission;
  human toolbar creation is unaffected.
  Commands run as the host muxel process with its permissions and environment,
  outside the caller's sandbox. Coding agents and configurable presets are not
  a security sandbox. Ask the user to grant authority; do not change settings.
  Turning "Also allow typing into shell panes" off also refuses later control
  writes to every raw argv pane, even agent-looking or interpreter programs, and
  shell-classified panes. Configured non-literal agents remain controllable while
  outside control is enabled. Disabling all outside control is a separate setting. Revocation
  does not stop the process or undo input already delivered; queued Enter/keys
  recheck the shell-input grant and terminal/session availability before each write.
  The JSON reply contains `id` (short), `uuid` (full), `project`, `project_id`,
  `cwd`, `program` and `args`. Without `--prompt`, presets and defaults keep toolbar
  system-prompt injection (including TypeIn), resume and hooks; raw argv bypasses those and tmux.
  Raw argv panes restore stopped. Project selection, Restart and wake never replay
  their opening brief; the pane's explicit Run again action reruns the saved argv.
  DIR is a launch directory, never an owned worktree; closing the pane does not
  remove it and worktree migration does not register it.
  On Windows raw argv requires a native executable; use `--preset` for batch
  wrappers. Installed Codex resolves to its packaged native executable.
  Remote projects are currently refused.
  `new` opens terminal panes; browser presets, including a browser default, are
  not supported yet and receive a refusal.
- `muxel ctl status [AGENT]` - one agent's status, or every agent's.
- `muxel ctl show AGENT [--full]` - the agent's last prompt, its last reply (the
  final message; `--full` for everything it wrote that turn), and, when it is
  blocked, the `question` it's asking with its numbered `options`. `controller`
  says which computer's muxel last typed into the agent through `muxel ctl`
  (`this_muxel` is true when it was this one) and whether that turn is still open.
- `muxel ctl screen AGENT [--lines N]` - the raw text of the agent's terminal.
  Use it when `show` doesn't explain what's on screen.
- `muxel ctl send AGENT TEXT` - type TEXT into the agent and press Enter. Refused
  while the agent is working, blocked, or still in the grace after a submitted
  control turn, or while muxel on another computer is mid-turn with it. For long
  or multi-line text, pass `-` and pipe the text on stdin. Use `--` before
  literal prompt text that starts with `-`; `-h` and `--help` before `--` refuse
  with exit 2 and are never typed. Use `muxel ctl help COMMAND` for help.
  The reply waits for Enter. If keys, pastes, mouse or wheel reports reach the
  pane after the paste, Enter is withheld and `send` fails with the prompt left
  in its input. A closed or restarted terminal also withholds Enter. Check
  `screen` before sending again. Focus reports, terminal replies and local scrollback do not
  count; input from another tmux client is outside this local guard.
  Add `--launch-id ID` with the full ID from `panes` or `status` to refuse a replaced
  terminal. This guard is checked again immediately before pasting after any
  asynchronous claim work, and `--force` never bypasses it. A successful reply
  includes the actual `launch_id` and `"pasted": true` for admission to the PTY
  writer, not confirmation of agent delivery. A lost reply does not make a
  retry safe; inspect the agent before typing again. A failure with `pasted: true`
  means a paste was attempted and the input may be partial. Check `screen` before
  retrying. A `working` error with `reason: just_submitted` means the prior
  control turn is still in its grace on a pane with provider status or configured
  screen markers; `wait` for its reply. Activity-only panes allow a later send.
  Codex, Claude and Grok sends also observe provider acceptance for about ten
  seconds (three more after a retry). `submission_confirmed` is true only for a fresh provider receipt and
  false when acceptance remains unknown, with `unconfirmed_reason` `timed_out`,
  `input_changed` (other input arrived; no further Enter is sent) or
  `interrupted` (the pane closed, restarted, switched conversation or lost
  permission). Once Enter is in, the reply is this receipt, never an error.
  It is null, without waiting, when the pane can't show a receipt: an
  unsupported provider, or no readable idle status title (tmux hides titles)
  with a Grok, multi-paragraph or repeated prompt.
  `enter_retries` reports zero or one additional Enter. Windows Codex alone may
  receive that retry after five seconds while its exact, fully visible ASCII
  prose remains in the idle composer and no local input intervened. Clipped,
  multiline, Unicode or completion drafts get no retry. Text is never pasted
  twice. Retry also requires a compact composer frame from before the paste
  and an added blank cursor line proving that the first Enter was consumed;
  a pending Enter never authorizes another.
  An unconfirmed receipt requires inspecting `screen` before more input.
- `muxel ctl wait AGENT [--timeout SECS] [--full]` - wait until the agent's turn
  ends (or it blocks on a question, or exits), then print what `show` prints.
  Startup tasks stay pending until fresh provider-owned acceptance, not merely
  our Enter. Codex may receive one extra Enter only while its exact task remains
  on one exact ASCII row in its live idle editor, with no new user row. Wrapped
  or ambiguous drafts get no retry; task text is never pasted again. A missing acceptance
  within ten seconds, failed readiness or unsupported provider sets
  `agent.startup_error` in show (or `startup_error` in status). Inspect the pane
  before sending another task; do not repeat creation. Muse and other providers
  without a verified startup protocol receive no task input.
  The `wait.outcome` values are finished, blocked, exited, not_running,
  submission_failed or timed_out. Default
  timeout 600 seconds.
- `muxel ctl answer AGENT OPTION` - pick numbered option OPTION of the question
  the agent is blocked on.
- `muxel ctl keys AGENT KEY...` - press keys: enter, esc, tab, shift-tab, up, down,
  left, right, backspace, space, ctrl-c, or any single character.

AGENT is an agent's `id` from `panes` (like `1a2b3c4d`), its name (`Claude`),
`project/name` when names repeat, or `focused` for the pane the user is looking
at. PROJECT is a project's id or name. muxels on different computers that share
a project usually show its agents under the same ids, but each command only
reaches the muxel on the computer it runs on.

If you are running in a muxel pane yourself, the environment variable
`MUXEL_INSTANCE_ID` holds your own pane's `uuid`, which works as AGENT. `send`,
`answer`, and `keys` refuse that pane unless you pass `--force`. Older muxel
hosts ignore this caller ID, so check the target when driving one.

## Statuses

- `working` - generating or running tools. Don't type into it; `wait` for it.
- `blocked` - waiting on a permission or choice prompt; `show` has the `question`.
- `done` - finished its turn; the reply is ready to read.
- `idle` - alive and waiting for a prompt.
- `starting` - launching. `exited` - the program ended. `failed` - it couldn't
  start. `not_running` - the pane hasn't been started this session; the user has
  to open it in muxel.

Each agent also has a `launch_id`, which changes whenever muxel opens a new
terminal for it: when its program starts or restarts, when muxel reconnects to
it, and when the user reopens its workspace. A different one means the agent may
have been restarted since you last looked (one kept in tmux can still be in the
same conversation), so check `show` before relying on what you saw. It stays the
same when the agent switches conversation by itself, as with Claude's `/resume`,
and is left out while the agent is starting and once it has exited.

## Errors

Creation permission/approval refusals, typing refusals from `send`, `answer` or
`keys`, and host connection failures have a `code`:

- `working`, `blocked`, `starting` - not now: `wait`, answer its question, or try
  again in a moment.
- `not_running`, `exited` - the agent isn't running; the user has to open it.
- `claimed_elsewhere` - another request holds the shared tmux claim.
- `launch_mismatch` - the terminal was replaced since the supplied `launch_id`.
- `input_changed` - the prompt was pasted but Enter was withheld. Inspect
  `screen` before retrying; the input is still there.
- `self_target` - this command would type into its caller's own pane; select a
  different target or pass `--force` deliberately.
- `host_unavailable`, `stale_endpoint` - no running muxel to ask.
- `control_disabled` - outside control was turned off before the mutation.
- `creation_disabled` - Outside terminal creation is Off; the user must choose Ask or Allow.
- `command_permission_required` - literal argv or a shell launch needs the shell/command grant.
- `approval_denied` - the user clicked Deny; nothing was created.
- `approval_timeout` - no approval arrived within 120 seconds; nothing was created.
- `prompt_unsupported` - prompts require a configured non-shell terminal preset.
- `parameter_unsupported` - model/effort require a configured preset flag absent from Extra args; literal commands cannot use them.

A failure without a code after the request went out (no answer came back) may or
may not have been carried out: check `status`, `show` or `screen` before typing
anything again. After a paste, pane closure, restart or a writer failure can
also withhold Enter and return an error without a code; inspect the input first.

## How to work

1. Run `panes` to find the agent. Use one whose status is `idle` or `done`.
2. `send` the prompt, then `wait` on the same agent. `wait` returns when the turn
   ends and includes the reply, so you don't need to poll.
3. If `wait` comes back `blocked`, read `question`. Permission prompts (run a
   command, edit a file, fetch a URL) are the user's decision: approve only what
   the user has told you to approve; otherwise tell the user what the agent is
   asking and let them choose. Answer with `answer AGENT N`, then `wait` again.
   If it's still blocked afterwards, check `screen` and use `keys` (often
   `enter`) to confirm.
4. If the reply ends with a question for the user (status `done`, no `question`),
   pass it on to the user, or answer it with `send` if you've been told how.
5. If `wait` times out, the agent is still working: `wait` again, or report
   progress to the user.

## Rules

- One prompt at a time per agent. Never `send` to an agent that is `working` or
  `blocked`.
- Only type into shell panes (`"is_agent": false`) when the user asks; muxel
  refuses unless the user has also allowed it in its settings.
- To stop an agent mid-turn press `esc` (`keys AGENT esc`). Don't send `ctrl-c`
  to an agent unless asked - twice exits it.
- An agent can be shared with muxel on another computer (a remote project opened
  on both, or one the other attached to). If `send` or `answer` says muxel on
  another computer is using the agent, don't `--force` it: `wait` on the agent
  and try again, or tell the user. Several of you may be driving the same agents.
- Create panes only when asked, using `new` within the user's granted authority.
  This interface can't close or rearrange existing panes; the layout is the user's.
"#;

#[cfg(test)]
mod tests {
    use super::{
        AgentEntry, AgentInfo, AgentState, CliCommand, Command, Error, ErrorCode, PaneNode,
        Request, Response, WaitOutcome, awaiting_reply, describe_layout, flag_value, key_bytes,
        parse_cli, pending_question, prompt_from_claude_transcript, resolve_agent, resolve_project,
        short_id, skill, wait_outcome,
    };
    use crate::{AgentActivity, SplitDirection};
    use uuid::Uuid;

    fn words(s: &[&str]) -> Vec<String> {
        s.iter().map(|w| w.to_string()).collect()
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn claim_matches(pattern: &str, value: &str) -> bool {
        match pattern.split_once('*') {
            None => pattern == value,
            Some((head, tail)) => {
                value.starts_with(head)
                    && (head.len()..=value.len()).any(|at| claim_matches(tail, &value[at..]))
            }
        }
    }

    #[test]
    fn same_process_claims_in_the_same_millisecond_have_distinct_identity() {
        let first = super::Claim {
            owner: "same-process".into(),
            host: "desk".into(),
            at: 1234,
            grace_ms: 30_000,
            nonce: Some(id(1)),
        };
        let second = super::Claim {
            nonce: Some(id(2)),
            ..first.clone()
        };
        assert!(first.same_turn(&super::Claim::decode(&first.encode()).unwrap()));
        assert!(!first.same_turn(&second));
        let pattern = super::Claim::pattern(&first.owner, first.nonce.unwrap());
        assert!(claim_matches(&pattern, &first.encode()));
        assert!(
            !claim_matches(&pattern, &second.encode()),
            "cleanup must preserve a newer same-ms claim"
        );
        let legacy = super::Claim {
            nonce: None,
            ..first.clone()
        };
        assert!(!claim_matches(&pattern, &legacy.encode()));
        assert_eq!(super::Claim::decode(&legacy.encode()), Some(legacy));
    }

    #[test]
    fn claims_from_before_the_nonce_field_still_deserialize() {
        let legacy: super::Claim =
            serde_json::from_str(r#"{"owner":"old","host":"desk","at":1,"grace_ms":30000}"#)
                .unwrap();
        assert_eq!(legacy.nonce, None);
        assert_eq!(super::Claim::decode("v1|old|desk|1|30000"), Some(legacy));
    }

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let req = Request {
            version: 1,
            token: "t".into(),
            caller_instance_id: None,
            command: Command::Show {
                agent: "claude".into(),
                full: true,
            },
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains(r#""cmd":"show""#), "{line}");
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), req);
        let unit: Request =
            serde_json::from_str(r#"{"version":1,"token":"t","command":{"cmd":"projects"}}"#)
                .unwrap();
        assert_eq!(unit.command, Command::Projects);
        assert_eq!(unit.caller_instance_id, None);
        let defaults: Request = serde_json::from_str(
            r#"{"version":1,"token":"t","command":{"cmd":"send","agent":"a","text":"hi"}}"#,
        )
        .unwrap();
        assert_eq!(
            defaults.command,
            Command::Send {
                agent: "a".into(),
                text: "hi".into(),
                force: false,
                launch_id: None,
            }
        );
        // No launch guard: older requests retain their original shape.
        let line = serde_json::to_string(&defaults).unwrap();
        assert!(!line.contains("launch_id"), "{line}");
        assert!(!line.contains("caller_instance_id"), "{line}");
        let caller = Request {
            caller_instance_id: Some(id(7)),
            command: Command::Keys {
                agent: "self".into(),
                keys: vec!["esc".into()],
                force: true,
            },
            ..req
        };
        let line = serde_json::to_string(&caller).unwrap();
        assert!(line.contains("caller_instance_id"), "{line}");
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), caller);
    }

    #[test]
    fn a_failure_carries_a_code_when_there_is_something_to_act_on() {
        let line = |reply: &Response| serde_json::to_string(reply).unwrap();
        let working = Response::err(Error::new(ErrorCode::Working, "the agent is working"));
        assert_eq!(
            line(&working),
            r#"{"ok":false,"code":"working","error":"the agent is working"}"#
        );
        // The rest keep the shape they had.
        assert_eq!(
            line(&Response::err("no agent matches 'x'")),
            r#"{"ok":false,"error":"no agent matches 'x'"}"#
        );
        assert_eq!(
            line(&Response::ok(serde_json::json!({ "sent": true }))),
            r#"{"ok":true,"result":{"sent":true}}"#
        );
        let back: Response = serde_json::from_str(&line(&working)).unwrap();
        assert_eq!(
            back.into_result(),
            Err(Error::new(ErrorCode::Working, "the agent is working"))
        );
        // A code this muxel doesn't know (from a newer one) reads as none.
        let newer: Response =
            serde_json::from_str(r#"{"ok":false,"code":"from_later","error":"x"}"#).unwrap();
        assert_eq!(newer.code, None);
        assert_eq!(newer.error.as_deref(), Some("x"));
        let uncertain = Response::err(
            Error::new(ErrorCode::Working, "check screen")
                .with_reason("just_submitted")
                .after_paste(),
        );
        let back: Response = serde_json::from_str(&line(&uncertain)).unwrap();
        assert_eq!(back.reason.as_deref(), Some("just_submitted"));
        assert_eq!(back.pasted, Some(true));
        for (code, word) in [
            (ErrorCode::Working, "working"),
            (ErrorCode::Blocked, "blocked"),
            (ErrorCode::Starting, "starting"),
            (ErrorCode::NotRunning, "not_running"),
            (ErrorCode::Exited, "exited"),
            (ErrorCode::ClaimedElsewhere, "claimed_elsewhere"),
            (ErrorCode::LaunchMismatch, "launch_mismatch"),
            (ErrorCode::InputChanged, "input_changed"),
            (ErrorCode::SelfTarget, "self_target"),
            (ErrorCode::HostUnavailable, "host_unavailable"),
            (ErrorCode::StaleEndpoint, "stale_endpoint"),
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), word);
            let response = Response::err(Error::new(code, "refused"));
            let line = serde_json::to_string(&response).unwrap();
            let decoded: Response = serde_json::from_str(&line).unwrap();
            assert_eq!(decoded.into_result(), Err(Error::new(code, "refused")));
        }
    }

    #[test]
    fn guarded_sends_use_a_version_legacy_hosts_reject() {
        assert_eq!(super::request_version(&Command::Projects), 1);
        let mut send = Command::Send {
            agent: "a".into(),
            text: "hi".into(),
            force: false,
            launch_id: None,
        };
        assert_eq!(super::request_version(&send), 1);
        if let Command::Send { launch_id, .. } = &mut send {
            *launch_id = Some(id(42));
        }
        assert_eq!(super::request_version(&send), 2);
    }

    #[test]
    fn launch_guard_accepts_only_the_expected_terminal() {
        let launch = id(42);
        assert_eq!(super::check_launch_id(None, None), Ok(()));
        assert_eq!(super::check_launch_id(None, Some(launch)), Ok(()));
        assert_eq!(super::check_launch_id(Some(launch), Some(launch)), Ok(()));
        for current in [None, Some(id(43))] {
            let error = super::check_launch_id(Some(launch), current).unwrap_err();
            assert_eq!(error.code, Some(ErrorCode::LaunchMismatch));
        }
    }

    #[test]
    fn send_receipt_identifies_the_admitted_launch_and_paste() {
        let receipt = super::pasted_prompt_receipt(id(7), id(42));
        assert_eq!(receipt["agent"], short_id(id(7)));
        assert_eq!(receipt["launch_id"], id(42).to_string());
        assert_eq!(receipt["sent"], true);
        assert_eq!(receipt["pasted"], true);
    }

    #[test]
    fn cli_keeps_the_launch_guard_even_with_force_and_rejects_invalid_ids() {
        let launch = id(42).to_string();
        assert_eq!(
            parse_cli(&words(&[
                "send",
                "a",
                "hi",
                "--force",
                "--launch-id",
                &launch
            ])),
            Ok(CliCommand::Request(Command::Send {
                agent: "a".into(),
                text: "hi".into(),
                force: true,
                launch_id: Some(id(42))
            }))
        );
        for args in [
            words(&["send", "a", "hi", "--launch-id"]),
            words(&["send", "a", "hi", "--launch-id", "invalid"]),
            words(&["status", "a", "--launch-id", &launch]),
            words(&["send", "a", "hi", "--once", "key"]),
        ] {
            assert!(parse_cli(&args).is_err(), "{args:?}");
        }
        // A literal flag belongs to the prompt and does not set a guard.
        assert_eq!(
            parse_cli(&words(&["send", "a", "--", "--launch-id", "text"])),
            Ok(CliCommand::Request(Command::Send {
                agent: "a".into(),
                text: "--launch-id text".into(),
                force: false,
                launch_id: None
            }))
        );
    }

    #[test]
    fn an_agent_shows_its_launch_id_only_while_it_runs() {
        let mut info = AgentInfo {
            id: short_id(id(7)),
            uuid: id(7),
            name: "Claude".into(),
            project: "p".into(),
            project_id: "11111111".into(),
            kind: "terminal".into(),
            program: Some("claude".into()),
            preset: "claude".into(),
            model: None,
            is_agent: true,
            status: Some(AgentState::NotRunning),
            status_since: None,
            launch_id: None,
            awaiting_reply: false,
            startup_error: None,
            focused: false,
            pane: Some(1),
            worktree_branch: None,
            cwd: None,
            remote: false,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("launch_id").is_none(), "{json}");
        info.status = Some(AgentState::Idle);
        info.launch_id = Some(id(42));
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["launch_id"], id(42).to_string());
        assert_eq!(serde_json::from_value::<AgentInfo>(json).unwrap(), info);
    }

    #[test]
    fn launch_id_is_the_terminals_only_while_its_program_runs() {
        let terminal = Some(id(42));
        for state in [
            AgentState::Working,
            AgentState::Idle,
            AgentState::Blocked,
            AgentState::Done,
        ] {
            assert_eq!(
                super::launch_id(Some(state), terminal),
                terminal,
                "{state:?}"
            );
        }
        // Its pane keeps the terminal of a program that exited, or of a session
        // being reconnected: that run is over, so it has no id to report.
        for state in [
            AgentState::Starting,
            AgentState::Exited,
            AgentState::NotRunning,
            AgentState::Failed,
        ] {
            assert_eq!(super::launch_id(Some(state), terminal), None, "{state:?}");
        }
        // Editors and browsers have no status, and no terminal either.
        assert_eq!(super::launch_id(None, terminal), None);
        assert_eq!(super::launch_id(Some(AgentState::Idle), None), None);
    }

    #[test]
    fn skill_explains_every_code() {
        let text = skill("/usr/bin/muxel");
        for code in [
            ErrorCode::Working,
            ErrorCode::Blocked,
            ErrorCode::Starting,
            ErrorCode::NotRunning,
            ErrorCode::Exited,
            ErrorCode::ClaimedElsewhere,
            ErrorCode::LaunchMismatch,
            ErrorCode::InputChanged,
            ErrorCode::HostUnavailable,
            ErrorCode::StaleEndpoint,
        ] {
            let word = serde_json::to_value(code).unwrap();
            let word = format!("`{}`", word.as_str().unwrap());
            assert!(text.contains(&word), "the skill doesn't explain {word}");
        }
        assert!(text.contains("--launch-id ID"));
        assert!(text.contains("\"pasted\": true"));
        assert!(!text.contains("--once"));
        assert!(text.contains("launch_id"));
    }

    #[test]
    fn agents_resolve_by_id_name_project_and_focus() {
        let a = id(0x1a2b3c4d_0000_0000_0000_000000000001);
        let b = id(0x9f000000_0000_0000_0000_000000000002);
        let c = id(0x9f001111_0000_0000_0000_000000000003);
        let agents = [
            AgentEntry {
                id: a,
                name: "✳ Claude",
                project: "muxel",
            },
            AgentEntry {
                id: b,
                name: "Codex",
                project: "muxel",
            },
            AgentEntry {
                id: c,
                name: "Codex",
                project: "Other Repo",
            },
        ];
        assert_eq!(resolve_agent("1a2b3c4d", &agents, None), Ok(a));
        assert_eq!(resolve_agent("1A2B", &agents, None), Ok(a));
        assert_eq!(resolve_agent(&a.to_string(), &agents, None), Ok(a));
        assert_eq!(resolve_agent("claude", &agents, None), Ok(a));
        assert_eq!(resolve_agent("other repo/codex", &agents, None), Ok(c));
        assert_eq!(resolve_agent("focused", &agents, Some(b)), Ok(b));
        assert!(resolve_agent("focused", &agents, None).is_err());
        let err = resolve_agent("codex", &agents, None).unwrap_err();
        assert!(
            err.contains("9f000000") && err.contains("9f001111"),
            "{err}"
        );
        // A shared id prefix is ambiguous rather than silently picking one.
        assert!(resolve_agent("9f00", &agents, None).is_err());
        assert_eq!(resolve_agent("9f000", &agents, None), Ok(b));
        assert!(resolve_agent("gemini", &agents, None).is_err());
    }

    #[test]
    fn projects_resolve_by_id_name_or_active() {
        let p = id(0xabcd0000_0000_0000_0000_000000000001);
        let projects = [(p, "My App")];
        assert_eq!(resolve_project("abcd", &projects, None), Ok(p));
        assert_eq!(resolve_project("my app", &projects, None), Ok(p));
        assert_eq!(resolve_project("active", &projects, Some(p)), Ok(p));
        assert!(resolve_project("nope", &projects, None).is_err());
    }

    #[test]
    fn layout_numbers_panes_in_tree_order() {
        let (a, b, c) = (id(1), id(2), id(3));
        let mut right = PaneNode::leaf(b);
        if let PaneNode::Leaf(leaf) = &mut right {
            leaf.tabs.push(c);
            leaf.active = 1;
        }
        let tree = PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![PaneNode::leaf(a), right],
        };
        let (value, panes) = describe_layout(&tree);
        assert_eq!(panes[&a], 1);
        assert_eq!(panes[&b], 2);
        assert_eq!(panes[&c], 2);
        assert_eq!(value["split"], "side_by_side");
        assert_eq!(value["children"][1]["active_tab"], short_id(c));
    }

    #[test]
    fn model_comes_from_either_flag_form() {
        let args = words(&["--model", "opus", "--x"]);
        assert_eq!(flag_value(&args, "--model"), Some("opus".into()));
        let args = words(&["--model=sonnet"]);
        assert_eq!(flag_value(&args, "--model"), Some("sonnet".into()));
        assert_eq!(flag_value(&args, ""), None);
        assert_eq!(flag_value(&words(&["--models", "x"]), "--model"), None);
    }

    #[test]
    fn keys_map_to_terminal_bytes() {
        assert_eq!(key_bytes("enter", false), Some(b"\r".to_vec()));
        assert_eq!(key_bytes("ESC", false), Some(b"\x1b".to_vec()));
        assert_eq!(key_bytes("up", false), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes("up", true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes("shift-tab", false), Some(b"\x1b[Z".to_vec()));
        assert_eq!(key_bytes("ctrl-c", false), Some(vec![3]));
        assert_eq!(key_bytes("1", false), Some(b"1".to_vec()));
        assert_eq!(key_bytes("é", false), Some("é".as_bytes().to_vec()));
        assert_eq!(key_bytes("ctrl-shift", false), None);
        assert_eq!(key_bytes("hello", false), None);
    }

    #[test]
    fn claude_permission_prompt_parses_into_question_and_options() {
        let screen = "\
⏺ I'll clean the build directory.

────────────────────────────────────────────────────────────
 Bash command

   rm -rf build/
   Remove old build output

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for rm commands in
      /Users/me/proj
   3. No, and tell Claude what to do differently (esc)

";
        let q = pending_question(screen).unwrap();
        assert_eq!(q.options.len(), 3);
        assert_eq!(q.options[0].key, "1");
        assert_eq!(q.options[0].label, "Yes");
        assert!(q.options[0].selected);
        assert!(!q.options[1].selected);
        assert!(q.options[2].label.starts_with("No, and tell Claude"));
        assert!(q.text.starts_with("Bash command"), "{}", q.text);
        assert!(q.text.contains("rm -rf build/"));
        assert!(q.text.ends_with("Do you want to proceed?"));
        assert!(!q.text.contains("clean the build"));
    }

    #[test]
    fn boxed_prompt_loses_its_frame() {
        let screen = "\
╭──────────────────────────────╮
│ Allow command?               │
│                              │
│ › 1. Yes (y)                 │
│   2. No (esc)                │
╰──────────────────────────────╯";
        let q = pending_question(screen).unwrap();
        assert_eq!(q.text, "Allow command?");
        assert_eq!(q.options.len(), 2);
        assert_eq!(q.options[1].label, "No (esc)");
        assert!(q.options[0].selected);
    }

    #[test]
    fn prompt_without_numbered_options_is_the_screen_tail() {
        let screen = "working on it\n\nOverwrite config.toml? (y/n)\n";
        let q = pending_question(screen).unwrap();
        assert!(q.options.is_empty());
        assert!(q.text.ends_with("Overwrite config.toml? (y/n)"));
        assert_eq!(pending_question("\n  \n"), None);
        // A screen that ends in a rule (a composer's frame) holds no question.
        assert_eq!(pending_question("⏺ Done.\n────────\n"), None);
    }

    #[test]
    fn numbered_list_in_a_reply_above_the_prompt_is_not_its_options() {
        let screen = "\
Steps:
1. Build
2. Test
────────
 Continue?
 ❯ 1. Yes
   2. No";
        let q = pending_question(screen).unwrap();
        assert_eq!(q.text, "Continue?");
        assert_eq!(
            q.options
                .iter()
                .map(|o| o.label.as_str())
                .collect::<Vec<_>>(),
            ["Yes", "No"]
        );
    }

    #[test]
    fn transcript_gives_the_last_real_prompt() {
        let jsonl = [
            r#"{"type":"user","message":{"content":"first prompt"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second prompt"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"meta"}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"content":"subagent task"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
            "{torn line",
        ]
        .join("\n");
        assert_eq!(
            prompt_from_claude_transcript(&jsonl),
            Some("second prompt".into())
        );
        let slash = r#"{"type":"user","message":{"content":"<command-name>/review</command-name>\n<command-args>src/app.rs</command-args>"}}"#;
        assert_eq!(
            prompt_from_claude_transcript(slash),
            Some("/review src/app.rs".into())
        );
        assert_eq!(prompt_from_claude_transcript(""), None);
    }

    #[test]
    fn a_sent_prompt_is_awaited_until_its_turn_ends() {
        let sent = 10_000;
        let quiet = AgentActivity {
            completed_at: Some(5_000),
            ..AgentActivity::default()
        };
        // Still showing the previous turn's Done: the new one hasn't started.
        assert!(awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Done),
            &quiet,
            sent + 500
        ));
        // Working on it.
        assert!(awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Working),
            &quiet,
            sent + 5_000
        ));
        // Finished after the prompt.
        let finished = AgentActivity {
            work_started_at: Some(sent + 400),
            completed_at: Some(sent + 9_000),
            ..AgentActivity::default()
        };
        assert!(!awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Done),
            &finished,
            sent + 9_500
        ));
        // Worked, then went idle without a completion mark.
        let idled = AgentActivity {
            work_started_at: Some(sent + 400),
            ..AgentActivity::default()
        };
        assert!(!awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Idle),
            &idled,
            sent + 3_000
        ));
        // Never visibly started: give up after the grace period.
        assert!(!awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Idle),
            &quiet,
            sent + super::REPLY_GRACE_MS
        ));
        // Just answered: the question is still on screen, blocked from before.
        let asked = AgentActivity {
            blocked_at: Some(sent - 2_000),
            ..AgentActivity::default()
        };
        assert!(awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Blocked),
            &asked,
            sent + 300
        ));
        // An answer that sent the agent straight back to idle: done once the
        // shorter answer grace has passed.
        assert!(awaiting_reply(
            Some(sent),
            super::ANSWER_GRACE_MS,
            Some(AgentState::Idle),
            &asked,
            sent + 1_000
        ));
        assert!(!awaiting_reply(
            Some(sent),
            super::ANSWER_GRACE_MS,
            Some(AgentState::Idle),
            &asked,
            sent + super::ANSWER_GRACE_MS
        ));
        // Blocked again after the answer: a new question.
        let asked_again = AgentActivity {
            work_started_at: Some(sent + 500),
            blocked_at: Some(sent + 4_000),
            ..AgentActivity::default()
        };
        assert!(!awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Blocked),
            &asked_again,
            sent + 4_500
        ));
        // Nothing sent through muxel: nothing to await.
        assert!(!awaiting_reply(
            None,
            super::REPLY_GRACE_MS,
            Some(AgentState::Working),
            &quiet,
            sent
        ));
    }

    #[test]
    fn wait_ends_on_finish_block_or_exit() {
        assert_eq!(wait_outcome(Some(AgentState::Working), false), None);
        assert_eq!(wait_outcome(Some(AgentState::Done), true), None);
        assert_eq!(
            wait_outcome(Some(AgentState::Done), false),
            Some(WaitOutcome::Finished)
        );
        assert_eq!(
            wait_outcome(Some(AgentState::Blocked), false),
            Some(WaitOutcome::Blocked)
        );
        // The question just answered, not yet off the screen.
        assert_eq!(wait_outcome(Some(AgentState::Blocked), true), None);
        assert_eq!(
            wait_outcome(Some(AgentState::Failed), false),
            Some(WaitOutcome::Exited)
        );
        assert_eq!(
            wait_outcome(Some(AgentState::NotRunning), false),
            Some(WaitOutcome::NotRunning)
        );
        assert_eq!(wait_outcome(None, false), None);
    }

    #[test]
    fn cli_parses_commands_flags_and_literal_text() {
        assert_eq!(parse_cli(&[]), Ok(CliCommand::Help(None)));
        assert_eq!(
            parse_cli(&words(&["panes"])),
            Ok(CliCommand::Request(Command::Panes { project: None }))
        );
        assert_eq!(
            parse_cli(&words(&["show", "claude", "--full"])),
            Ok(CliCommand::Request(Command::Show {
                agent: "claude".into(),
                full: true
            }))
        );
        assert_eq!(
            parse_cli(&words(&["send", "a1b2", "fix", "the", "tests"])),
            Ok(CliCommand::Request(Command::Send {
                agent: "a1b2".into(),
                text: "fix the tests".into(),
                force: false,
                launch_id: None,
            }))
        );
        assert_eq!(
            parse_cli(&words(&["send", "a1b2", "--", "--help", "--force"])),
            Ok(CliCommand::Request(Command::Send {
                agent: "a1b2".into(),
                text: "--help --force".into(),
                force: false,
                launch_id: None,
            }))
        );
        assert_eq!(
            parse_cli(&words(&["wait", "a1b2", "--timeout", "30"])),
            Ok(CliCommand::Wait {
                agent: "a1b2".into(),
                timeout_secs: 30,
                full: false
            })
        );
        assert_eq!(
            parse_cli(&words(&["screen", "a1b2", "--lines", "200"])),
            Ok(CliCommand::Request(Command::Screen {
                agent: "a1b2".into(),
                lines: Some(200)
            }))
        );
        assert!(parse_cli(&words(&["send", "a1b2"])).is_err());
        assert!(parse_cli(&words(&["show"])).is_err());
        assert!(parse_cli(&words(&["keys", "a1b2", "warp"])).is_err());
        assert!(parse_cli(&words(&["wait", "a", "--timeout", "soon"])).is_err());
        assert!(parse_cli(&words(&["show", "a", "--nope"])).is_err());
        assert!(parse_cli(&words(&["frobnicate"])).is_err());
        assert!(parse_cli(&words(&["status", "a", "b"])).is_err());
    }

    #[test]
    fn dash_h_after_command_refuses_without_typing() {
        for args in [
            words(&["send", "agent", "-h"]),
            words(&["send", "--help"]),
            words(&["keys", "agent", "enter", "--help"]),
            words(&["new", "project", "-h"]),
        ] {
            let error = parse_cli(&args).unwrap_err();
            assert!(error.contains("was not run"), "{args:?}: {error}");
            assert!(error.contains("muxel ctl help"), "{args:?}: {error}");
            assert!(error.contains("`--`"), "{args:?}: {error}");
        }
        assert_eq!(
            parse_cli(&words(&["help", "send"])),
            Ok(CliCommand::Help(Some("send".into())))
        );
        assert!(super::command_help("send").unwrap().contains("--force"));
        assert!(matches!(
            parse_cli(&words(&["send", "agent", "--", "-h"])),
            Ok(CliCommand::Request(Command::Send { text, .. })) if text == "-h"
        ));
    }

    #[test]
    fn unknown_command_suggests_nearest_verb() {
        let error = parse_cli(&words(&["list"])).unwrap_err();
        assert!(error.contains("did you mean `panes`?"), "{error}");
    }

    #[test]
    fn send_to_own_pane_is_refused() {
        let me = id(7);
        let error = super::check_self_target(Some(me), me, false).unwrap_err();
        assert_eq!(error.code, Some(ErrorCode::SelfTarget));
        assert_eq!(super::check_self_target(Some(me), me, true), Ok(()));
        assert_eq!(super::check_self_target(Some(me), id(8), false), Ok(()));
        assert_eq!(super::check_self_target(None, me, false), Ok(()));
    }

    #[test]
    fn skill_names_the_binary_it_was_made_with_and_how_to_find_another() {
        let text = skill("/Applications/muxel.app/Contents/MacOS/muxel");
        assert!(text.contains("`/Applications/muxel.app/Contents/MacOS/muxel` - where muxel was"));
        assert!(!text.contains("MUXEL_PATH"));
        // Commands name the muxel found on whichever computer runs them.
        assert!(text.contains("`muxel ctl send AGENT TEXT`"));
        assert!(text.contains("dev.muxel.muxel/control.json"));
        // Every Linux install: package, AppImage, tarball.
        assert!(text.contains("`/usr/bin/muxel`"));
        assert!(text.contains("muxel-linux-x86_64.AppImage ctl projects"));
        assert!(text.contains("APPIMAGE_EXTRACT_AND_RUN=1"));
        assert!(text.contains("MUXEL_CONTROL="));
        assert!(text.contains("%APPDATA%\\muxel\\muxel\\data\\control.json"));
        let spaced = skill("/Users/me/My Apps/muxel");
        assert!(spaced.contains("`'/Users/me/My Apps/muxel'`"), "{spaced}");
    }

    #[test]
    fn skill_preview_joins_wrapped_lines_but_keeps_structure() {
        let md = "---\nname: muxel\ndescription: one line\n---\n\n# Title\n\nA paragraph\nwrapped here\nand here.\n\n    code line\n    another\n\n- item one\n  continued\n- item two\n   - nested: `a` (usually\n     `b`)\n1. step one\n   still one\n2. step two\n";
        assert_eq!(
            super::reflow_for_display(md),
            "---\nname: muxel\ndescription: one line\n---\n\n# Title\n\nA paragraph wrapped here and here.\n\n    code line\n    another\n\n- item one continued\n- item two\n   - nested: `a` (usually `b`)\n1. step one still one\n2. step two"
        );
        // The real skill: no line is a stray fragment of the one before.
        let shown = super::reflow_for_display(&skill("/usr/bin/muxel"));
        assert!(shown.contains("muxel is a desktop app on the user's computer that runs coding agents (Claude, Codex, opencode, Grok, ...) and shells side by side"));
        assert!(shown.contains("\n1. `/usr/bin/muxel` - where muxel was when these instructions were made. Use it if it exists on this computer.\n"));
        assert!(shown.contains(
            "\n   - macOS: `~/Library/Application Support/dev.muxel.muxel/control.json`\n"
        ));
    }

    #[test]
    fn claims_round_trip_and_survive_odd_host_names() {
        let claim = super::Claim {
            owner: "a1b2c3d4e5f6".into(),
            host: "Sam's Mac|mini".into(),
            at: 1_790_000_000_000,
            grace_ms: super::REPLY_GRACE_MS,
            nonce: None,
        };
        let value = claim.encode();
        assert_eq!(value, "v1|a1b2c3d4e5f6|Sam's-Mac-mini|1790000000000|30000");
        let back = super::Claim::decode(&value).unwrap();
        assert_eq!(back.owner, claim.owner);
        assert_eq!(back.host, "Sam's-Mac-mini");
        assert_eq!(back.at, claim.at);
        assert_eq!(super::Claim::decode(""), None);
        assert_eq!(super::Claim::decode("v2|x|y|1|2"), None);
        assert_eq!(super::Claim::decode("v1||y|1|2"), None);
        assert_eq!(super::Claim::decode("v1|x|y|soon|2"), None);
    }

    #[test]
    fn another_muxels_claim_holds_until_its_turn_ends() {
        let at = 50_000;
        let claim = super::Claim {
            owner: "other".into(),
            host: "desk".into(),
            at,
            grace_ms: super::REPLY_GRACE_MS,
            nonce: None,
        };
        let before = AgentActivity {
            completed_at: Some(at - 10_000),
            ..AgentActivity::default()
        };
        // Sent, not visibly started yet: held.
        assert!(super::claim_holds(
            &claim,
            "me",
            Some(AgentState::Done),
            &before,
            at + 800
        ));
        // Our own claim never blocks us.
        assert!(!super::claim_holds(
            &claim,
            "other",
            Some(AgentState::Done),
            &before,
            at + 800
        ));
        // Their turn finished: free.
        let finished = AgentActivity {
            work_started_at: Some(at + 1_000),
            completed_at: Some(at + 9_000),
            ..AgentActivity::default()
        };
        assert!(!super::claim_holds(
            &claim,
            "me",
            Some(AgentState::Done),
            &finished,
            at + 9_500
        ));
        // Left by a muxel that went away before anything happened: lapses.
        assert!(!super::claim_holds(
            &claim,
            "me",
            Some(AgentState::Idle),
            &before,
            at + super::REPLY_GRACE_MS
        ));
    }

    #[test]
    fn claim_compare_patterns_keep_modern_nonce_and_refuse_unsafe_legacy_values() {
        let claim = super::Claim {
            owner: "owner".into(),
            host: "desk'name".into(),
            at: 42,
            grace_ms: super::REPLY_GRACE_MS,
            nonce: Some(uuid::Uuid::from_u128(7)),
        };
        assert_eq!(
            claim.compare_pattern(),
            Some(super::Claim::pattern("owner", uuid::Uuid::from_u128(7)))
        );
        let legacy = super::Claim {
            nonce: None,
            host: "desk".into(),
            ..claim
        };
        assert_eq!(legacy.compare_pattern(), Some(legacy.encode()));
        assert!(
            super::Claim {
                host: "desk'name".into(),
                ..legacy
            }
            .compare_pattern()
            .is_none()
        );
    }
}
