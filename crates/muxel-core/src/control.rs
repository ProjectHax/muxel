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

/// Bumped when a request or reply changes shape incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// The file, in muxel's data directory, where the running app says how to reach
/// it. Owner-only: the token in it is the whole of the authorization.
pub const ENDPOINT_FILE: &str = "control.json";

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
    },
    /// Pick a numbered option of the question the agent is blocked on.
    Answer { agent: String, option: String },
    /// Press keys (see [`key_bytes`]).
    Keys { agent: String, keys: Vec<String> },
}

/// The reply to a [`Request`]: one line of JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
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
    /// A prompt sent through `muxel ctl` whose turn hasn't ended yet.
    pub awaiting_reply: bool,
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
/// the agent's tmux session as `v1|owner|host|at|grace`.
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
}

impl Claim {
    pub fn encode(&self) -> String {
        fn field(s: &str) -> String {
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
        format!(
            "v1|{}|{}|{}|{}",
            field(&self.owner),
            field(&self.host),
            self.at,
            self.grace_ms
        )
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
        (!owner.is_empty()).then_some(Self {
            owner,
            host,
            at,
            grace_ms,
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
    Help,
}

/// `wait`'s default patience.
pub const WAIT_TIMEOUT_SECS: u64 = 600;

/// Usage text for `muxel ctl help`.
pub const USAGE: &str = "\
usage: muxel ctl <command> [args]

  projects                       list projects
  panes [PROJECT]                pane layout and agents (all projects, or one)
  status [AGENT]                 an agent's status (or every agent's)
  show AGENT [--full]            last prompt, last reply, pending question
  screen AGENT [--lines N]       raw terminal text (default 60 lines)
  send AGENT TEXT [--force]      type TEXT and press Enter ('-' reads stdin)
  wait AGENT [--timeout SECS] [--full]
                                 wait for the turn to end, then show it
  answer AGENT OPTION            pick a numbered option of a pending question
  keys AGENT KEY...              press keys: enter esc tab shift-tab up down
                                 left right backspace space ctrl-c, or a character
  skill                          instructions for an orchestrating agent

AGENT is an id from `panes`, a name, project/name, or `focused`.
Everything after `--` is taken literally (for text that starts with --).
muxel must be running with Settings > Grok Bot > \"Allow outside tools to control muxel\" on.";

/// Parse the arguments after `muxel ctl`.
pub fn parse_cli(args: &[String]) -> Result<CliCommand, String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Ok(CliCommand::Help);
    };
    let mut words: Vec<String> = Vec::new();
    let mut full = false;
    let mut force = false;
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
            "--lines" => lines = Some(number(it.next(), "--lines")?),
            "--timeout" => timeout = Some(number(it.next(), "--timeout")?),
            flag if flag.starts_with("--") && flag.len() > 2 => {
                return Err(format!("unknown option {flag}"));
            }
            _ => words.push(arg.clone()),
        }
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
            Command::Send { agent, text, force }
        }
        "answer" => {
            at_most(2)?;
            let agent = agent(&words)?;
            let option = words
                .get(1)
                .cloned()
                .ok_or("`answer` needs the OPTION number to pick")?;
            Command::Answer { agent, option }
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
            Command::Keys { agent, keys }
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
        "help" | "-h" | "--help" => return Ok(CliCommand::Help),
        other => return Err(format!("unknown command '{other}'\n\n{USAGE}")),
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
`{"ok": false, "error": "..."}` - the error says what went wrong and what to do.
If it says muxel isn't running or outside control is off, ask the user to open
muxel and turn on Settings > Grok Bot > "Allow outside tools to control muxel".

## Commands

- `muxel ctl projects` - every project: id, name, folder, git branch, and how
  many agents are working / blocked / done / idle.
- `muxel ctl panes [PROJECT]` - the pane layout and every agent in it: id, name,
  program, model, status, which pane it's in, and which one the user has focused.
- `muxel ctl status [AGENT]` - one agent's status, or every agent's.
- `muxel ctl show AGENT [--full]` - the agent's last prompt, its last reply (the
  final message; `--full` for everything it wrote that turn), and, when it is
  blocked, the `question` it's asking with its numbered `options`. `controller`
  says which computer's muxel last typed into the agent through `muxel ctl`
  (`this_muxel` is true when it was this one) and whether that turn is still open.
- `muxel ctl screen AGENT [--lines N]` - the raw text of the agent's terminal.
  Use it when `show` doesn't explain what's on screen.
- `muxel ctl send AGENT TEXT` - type TEXT into the agent and press Enter. Refused
  while the agent is working or blocked, or while muxel on another computer is
  mid-turn with it. For long or multi-line text, pass `-` and pipe the text on
  stdin.
- `muxel ctl wait AGENT [--timeout SECS] [--full]` - wait until the agent's turn
  ends (or it blocks on a question, or exits), then print what `show` prints plus
  `wait.outcome`: finished, blocked, exited, not_running or timed_out. Default
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

## Statuses

- `working` - generating or running tools. Don't type into it; `wait` for it.
- `blocked` - waiting on a permission or choice prompt; `show` has the `question`.
- `done` - finished its turn; the reply is ready to read.
- `idle` - alive and waiting for a prompt.
- `starting` - launching. `exited` - the program ended. `failed` - it couldn't
  start. `not_running` - the pane hasn't been started this session; the user has
  to open it in muxel.

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
- This interface can't create, close or rearrange panes; the layout is the user's.
"#;

#[cfg(test)]
mod tests {
    use super::{
        AgentEntry, AgentState, CliCommand, Command, PaneNode, Request, WaitOutcome,
        awaiting_reply, describe_layout, flag_value, key_bytes, parse_cli, pending_question,
        prompt_from_claude_transcript, resolve_agent, resolve_project, short_id, skill,
        wait_outcome,
    };
    use crate::{AgentActivity, SplitDirection};
    use uuid::Uuid;

    fn words(s: &[&str]) -> Vec<String> {
        s.iter().map(|w| w.to_string()).collect()
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let req = Request {
            version: 1,
            token: "t".into(),
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
        let defaults: Request = serde_json::from_str(
            r#"{"version":1,"token":"t","command":{"cmd":"send","agent":"a","text":"hi"}}"#,
        )
        .unwrap();
        assert_eq!(
            defaults.command,
            Command::Send {
                agent: "a".into(),
                text: "hi".into(),
                force: false
            }
        );
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
        assert_eq!(parse_cli(&[]), Ok(CliCommand::Help));
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
                force: false
            }))
        );
        assert_eq!(
            parse_cli(&words(&["send", "a1b2", "--", "--help", "--force"])),
            Ok(CliCommand::Request(Command::Send {
                agent: "a1b2".into(),
                text: "--help --force".into(),
                force: false
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
}
