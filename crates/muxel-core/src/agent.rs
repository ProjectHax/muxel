//! Agent presets and system-prompt injection.
//!
//! A [`AgentPreset`] is a template for launching an agent (Claude, opencode, a
//! plain shell, …). [`resolve_launch`] turns an [`Instance`] into the concrete
//! program/args plus any text to type in at startup, applying the configured
//! [`InjectionMode`] for the system prompt.

use crate::Instance;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// What a preset opens: a terminal agent or a web-browser pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PresetKind {
    /// A terminal running an agent/shell (`program`, `args`, … apply).
    #[default]
    Terminal,
    /// A web-browser pane that opens `url` (the terminal fields are unused).
    Browser,
}

/// How an instance's system prompt is delivered to the agent.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum InjectionMode {
    /// Don't inject a system prompt.
    #[default]
    None,
    /// Pass it as a CLI flag, e.g. `claude --append-system-prompt <prompt>`.
    CliFlag { flag: String },
    /// Type it into the terminal and press Enter shortly after the agent starts.
    TypeIn,
}

/// An environment variable applied to an agent's process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

fn default_model_flag() -> Option<String> {
    Some("--model".to_string())
}

/// A launch template for an agent. Editable + persisted; `compose_args` turns
/// the structured fields (model/effort/extra) into a concrete argument list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPreset {
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    pub name: String,
    /// Program to run; `None` = the user's default shell.
    #[serde(default)]
    pub program: Option<String>,
    /// Model name, passed via `model_flag` when both are set.
    #[serde(default)]
    pub model: Option<String>,
    /// Flag used to pass the model (e.g. `--model`).
    #[serde(default)]
    pub model_flag: Option<String>,
    /// Reasoning-effort value, passed via `effort_flag` when both are set.
    #[serde(default)]
    pub effort: Option<String>,
    /// Flag used to pass the effort (tool-specific; often unset).
    #[serde(default)]
    pub effort_flag: Option<String>,
    /// Extra arguments appended after model/effort.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub injection: InjectionMode,
    /// Environment variables to set for the process.
    #[serde(default)]
    pub env: Vec<EnvVar>,
    /// Override on-screen markers that mean the agent is actively working (its
    /// spinner). Empty → use the built-in defaults for the program, else the
    /// output-activity heuristic.
    #[serde(default)]
    pub working_markers: Vec<String>,
    /// Override on-screen markers that mean the agent is blocked on the user (a
    /// permission/approval prompt). Empty → built-in defaults, else none.
    #[serde(default)]
    pub blocked_markers: Vec<String>,
    /// Fixed delay (ms) after the agent first produces output before runner
    /// automation types into it — for agents that keep loading after their first
    /// draw (e.g. opencode). 0 = auto: wait until output goes quiet instead.
    #[serde(default)]
    pub startup_delay_ms: u32,
    /// CLI flag that starts a conversation with a chosen session ID (e.g. Claude's
    /// `--session-id <uuid>`). When set with [`Self::resume_flag`], muxel mints a
    /// stable id per pane and passes it on first launch. When `None` but
    /// `resume_flag` is set (e.g. Codex), the agent mints its own id and muxel
    /// captures it from disk before the next resume. `None` + no `resume_flag`
    /// = no resume support.
    #[serde(default)]
    pub session_id_flag: Option<String>,
    /// CLI flag or subcommand that resumes a conversation by session ID (e.g.
    /// Claude's `--resume`, Codex's `resume`). Required for resume support.
    #[serde(default)]
    pub resume_flag: Option<String>,
    /// Whether this preset opens a terminal agent or a browser pane.
    #[serde(default)]
    pub kind: PresetKind,
    /// Homepage for a `Browser`-kind preset (ignored for terminals).
    #[serde(default)]
    pub url: String,
}

impl AgentPreset {
    /// The default-shell preset: `program: None` flows through
    /// [`CommandSpec::shell`], the OS default shell. Named "PowerShell" on Windows
    /// (where that's the default), "Shell" elsewhere.
    pub fn shell() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: if cfg!(windows) { "PowerShell" } else { "Shell" }.to_string(),
            program: None,
            model: None,
            model_flag: None,
            effort: None,
            effort_flag: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::None,
            env: Vec::new(),
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            session_id_flag: None,
            resume_flag: None,
        }
    }

    /// The Windows `cmd.exe` shell, offered alongside PowerShell. Runs `cmd.exe`
    /// explicitly (PowerShell is the `program: None` default). Only seeded on
    /// Windows (see [`AgentPreset::defaults`]).
    pub fn cmd() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "Cmd".to_string(),
            program: Some("cmd.exe".to_string()),
            model: None,
            model_flag: None,
            effort: None,
            effort_flag: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::None,
            env: Vec::new(),
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            session_id_flag: None,
            resume_flag: None,
        }
    }

    /// Git for Windows' bash, offered alongside PowerShell and cmd. Only seeded
    /// on Windows (see [`AgentPreset::defaults`]).
    ///
    /// `-i -l` matches what Windows Terminal and VS Code launch: the login shell
    /// is what sets up the MSYS `PATH`, so without `-l` the pane starts without
    /// the Unix tools that are the reason to use Git Bash at all. `CHERE_INVOKING`
    /// is what keeps the pane in its project — Git Bash's `/etc/profile` `cd`s to
    /// `$HOME` on every login shell unless it is set, which would drop every pane
    /// out of the worktree muxel just opened it in.
    pub fn git_bash() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "Git Bash".to_string(),
            program: Some(git_bash_program()),
            model: None,
            model_flag: None,
            effort: None,
            effort_flag: None,
            args: vec!["-i".to_string(), "-l".to_string()],
            system_prompt: None,
            injection: InjectionMode::None,
            env: vec![EnvVar {
                key: "CHERE_INVOKING".to_string(),
                value: "1".to_string(),
            }],
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            session_id_flag: None,
            resume_flag: None,
        }
    }

    pub fn claude() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "Claude".to_string(),
            program: Some("claude".to_string()),
            model: None,
            model_flag: default_model_flag(),
            effort: None,
            effort_flag: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::CliFlag {
                flag: "--append-system-prompt".to_string(),
            },
            env: Vec::new(),
            // Claude prints "esc to interrupt" on its status line for the whole
            // duration of a turn, so it's a reliable "working" signal — far more so
            // than the output-activity timer, which the long "Computing…" phase
            // (quiet output / a stalled spinner) trips into a false "idle".
            working_markers: vec!["esc to interrupt".to_string()],
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            session_id_flag: Some("--session-id".to_string()),
            resume_flag: Some("--resume".to_string()),
        }
    }

    pub fn opencode() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "opencode".to_string(),
            program: Some("opencode".to_string()),
            model: None,
            model_flag: default_model_flag(),
            effort: None,
            effort_flag: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::TypeIn,
            env: Vec::new(),
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            // opencode keeps loading well after its first draw; wait before typing.
            startup_delay_ms: 6000,
            session_id_flag: None,
            resume_flag: None,
        }
    }

    /// The built-in presets, in display order.
    pub fn hermes() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "Hermes".to_string(),
            program: Some("hermes".to_string()),
            model: None,
            model_flag: default_model_flag(),
            effort: None,
            effort_flag: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::TypeIn,
            env: Vec::new(),
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            session_id_flag: None,
            resume_flag: None,
        }
    }

    pub fn ollama() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "Ollama".to_string(),
            program: Some("ollama".to_string()),
            model: None,
            model_flag: None,
            effort: None,
            effort_flag: None,
            // `ollama run <model>` — change the model in the preset's args.
            args: vec!["run".to_string(), "llama3.2".to_string()],
            system_prompt: None,
            injection: InjectionMode::TypeIn,
            env: Vec::new(),
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            session_id_flag: None,
            resume_flag: None,
        }
    }

    /// Run a coding agent backed by an Ollama model via `ollama launch <agent>
    /// --model <model>` (e.g. `ollama launch opencode --model glm-5.2:cloud`). The
    /// whole launch line lives in `args` because the `--model` flag has to follow
    /// the `launch` subcommand and its agent — change the agent or model there.
    /// Markers default to opencode's TUI (the seeded agent); adjust them if you
    /// point it at a different agent.
    pub fn ollama_code() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "Ollama Code".to_string(),
            program: Some("ollama".to_string()),
            model: None,
            model_flag: None,
            effort: None,
            effort_flag: None,
            args: vec![
                "launch".to_string(),
                "opencode".to_string(),
                "--model".to_string(),
                "glm-5.2:cloud".to_string(),
            ],
            system_prompt: None,
            injection: InjectionMode::TypeIn,
            env: Vec::new(),
            working_markers: vec!["esc interrupt".to_string()],
            blocked_markers: vec!["Permission required".to_string()],
            // The launched agent (opencode) keeps loading after its first draw, on
            // top of ollama's own connect — wait before any runner types into it.
            startup_delay_ms: 6000,
            session_id_flag: None,
            resume_flag: None,
        }
    }

    pub fn pi() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "Pi".to_string(),
            program: Some("pi".to_string()),
            model: None,
            model_flag: default_model_flag(),
            effort: None,
            effort_flag: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::TypeIn,
            env: Vec::new(),
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            session_id_flag: None,
            resume_flag: None,
        }
    }

    /// Sourcegraph's Amp (https://ampcode.com) — the `amp` CLI.
    pub fn amp() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "Amp".to_string(),
            program: Some("amp".to_string()),
            model: None,
            model_flag: None,
            effort: None,
            effort_flag: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::TypeIn,
            env: Vec::new(),
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            session_id_flag: None,
            resume_flag: None,
        }
    }

    /// xAI's Grok CLI (https://x.ai/cli) — the `grok` command.
    ///
    /// Grok speaks the same session flags as Claude (`--session-id` / `--resume`),
    /// so panes reopen their prior conversation after a muxel restart.
    pub fn grok() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "Grok".to_string(),
            program: Some("grok".to_string()),
            model: None,
            model_flag: default_model_flag(),
            effort: None,
            effort_flag: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::CliFlag {
                flag: "--rules".to_string(),
            },
            env: Vec::new(),
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            session_id_flag: Some("--session-id".to_string()),
            resume_flag: Some("--resume".to_string()),
        }
    }

    /// OpenAI's Codex CLI (`codex`). Codex mints its own session UUID (no
    /// `--session-id` on create); resume is the subcommand `codex resume <id>`.
    /// muxel captures the real id from `~/.codex/sessions` before restarting.
    pub fn codex() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Terminal,
            url: String::new(),
            name: "Codex".to_string(),
            program: Some("codex".to_string()),
            model: None,
            model_flag: default_model_flag(),
            effort: None,
            effort_flag: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::TypeIn,
            env: Vec::new(),
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            // Agent-owned id: leave session_id_flag unset; resume is a subcommand.
            session_id_flag: None,
            resume_flag: Some("resume".to_string()),
        }
    }

    /// A built-in web-browser preset. Picked like an agent; opens a browser pane
    /// (embedded on macOS/Windows, a separate window on Linux) at `url`.
    pub fn browser() -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: PresetKind::Browser,
            url: "https://duckduckgo.com".to_string(),
            name: "Browser".to_string(),
            program: None,
            model: None,
            model_flag: None,
            effort: None,
            effort_flag: None,
            args: Vec::new(),
            system_prompt: None,
            injection: InjectionMode::None,
            env: Vec::new(),
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            session_id_flag: None,
            resume_flag: None,
        }
    }

    pub fn defaults() -> Vec<AgentPreset> {
        let mut presets = vec![Self::shell()];
        // On Windows, offer cmd.exe and Git Bash alongside the PowerShell default.
        #[cfg(windows)]
        {
            presets.push(Self::cmd());
            presets.push(Self::git_bash());
        }
        presets.extend([
            Self::claude(),
            Self::opencode(),
            Self::amp(),
            Self::grok(),
            Self::codex(),
            Self::hermes(),
            Self::ollama(),
            Self::ollama_code(),
            Self::pi(),
            Self::browser(),
        ]);
        presets
    }

    /// Compose the full argument list: `model_flag model`, then
    /// `effort_flag effort`, then the extra args. Pairs are skipped unless both
    /// the flag and the value are set (and non-empty).
    pub fn compose_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let (Some(flag), Some(model)) = (
            &self.model_flag,
            self.model.as_ref().filter(|m| !m.is_empty()),
        ) {
            out.push(flag.clone());
            out.push(model.clone());
        }
        if let (Some(flag), Some(effort)) = (
            &self.effort_flag,
            self.effort.as_ref().filter(|e| !e.is_empty()),
        ) {
            out.push(flag.clone());
            out.push(effort.clone());
        }
        out.extend(self.args.iter().cloned());
        out
    }
}

/// Concrete launch parameters resolved from an instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedLaunch {
    /// Program to run; `None` = default shell.
    pub program: Option<String>,
    pub args: Vec<String>,
    /// Text to type into the terminal once the agent is ready (TypeIn injection).
    pub startup_input: Option<String>,
    /// Number of Shift+Tab presses to send before typing (runner "auto mode").
    pub auto_mode_presses: u8,
    /// Press Enter to submit after typing `startup_input`.
    pub submit: bool,
    /// Environment variables to set for the process.
    pub env: Vec<(String, String)>,
}

/// Resolve an instance into program/args (+ any startup input + env), applying
/// its system-prompt injection mode.
pub fn resolve_launch(instance: &Instance) -> ResolvedLaunch {
    resolve_launch_for_session(instance, false)
}

/// Resolve a launch while respecting whether it resumes an existing conversation.
///
/// Type-in injection is a real user turn. It belongs only to a new conversation;
/// submitting it again on process restart mutates the resumed transcript. CLI
/// flags remain launch configuration and may need to be applied on every process.
pub fn resolve_launch_for_session(instance: &Instance, resuming: bool) -> ResolvedLaunch {
    let mut args = instance.args.clone();
    let mut startup_input = None;

    if let Some(prompt) = instance.system_prompt.as_ref().filter(|p| !p.is_empty()) {
        match &instance.injection {
            InjectionMode::CliFlag { flag } => {
                args.push(flag.clone());
                args.push(prompt.split_whitespace().collect::<Vec<_>>().join(" "));
            }
            InjectionMode::TypeIn if !resuming => {
                // A raw newline is Enter in TUIs without bracketed-paste mode.
                // Keep the instruction bundle to one startup turn everywhere.
                startup_input = Some(prompt.split_whitespace().collect::<Vec<_>>().join(" "));
            }
            InjectionMode::TypeIn => {}
            InjectionMode::None => {}
        }
    }

    if instance.program.as_deref().is_some_and(is_codex_program) {
        args.push("--config".to_string());
        args.push(codex_terminal_title_override().to_string());
    }

    ResolvedLaunch {
        program: instance.program.clone(),
        args,
        startup_input,
        auto_mode_presses: instance.auto_mode_presses,
        submit: instance.auto_submit,
        env: instance
            .env
            .iter()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect(),
    }
}

/// The CLI arguments to start or resume a session for a resume-capable agent.
///
/// Resume support requires [`AgentPreset::resume_flag`]. Two shapes:
///
/// - **Host-minted** (`session_id_flag` set, e.g. Claude): first launch returns
///   `[session_id_flag, id]`; later launches return `[resume_flag, id]`.
/// - **Agent-minted** (`session_id_flag` unset, e.g. Codex): first launch returns
///   `None` (bare start — the agent creates its own id); later launches return
///   `[resume_flag, id]` once the caller has captured the real id.
///
/// Keying off `session_started` rather than probing the agent's on-disk session
/// avoids a flush race for host-minted agents. When a session was genuinely
/// deleted, the caller probes the disk and restarts cleanly.
pub fn session_resume_args(preset: &AgentPreset, instance: &Instance) -> Option<Vec<String>> {
    let resume_flag = preset.resume_flag.as_deref()?;
    if instance.session_started {
        let id = instance.session_id.as_deref()?;
        return Some(vec![resume_flag.to_string(), id.to_string()]);
    }
    // First launch: only host-minted agents pass an id flag.
    let id_flag = preset.session_id_flag.as_deref()?;
    let id = instance.session_id.as_deref()?;
    Some(vec![id_flag.to_string(), id.to_string()])
}

/// Path to Claude's on-disk session transcript for an agent running in `cwd`:
/// `<home>/.claude/projects/<slug>/<session_id>.jsonl`, where `slug` is `cwd` with
/// every non-ASCII-alphanumeric character replaced by `-` — Claude's project-dir
/// encoding (e.g. `/home/u/Proj` → `-home-u-Proj`, `/home/u/.local` →
/// `-home-u--local`). Pure path-building; the caller does the existence check. The
/// caller must start a *fresh* session id when the file is missing, never reuse the
/// old one (that would collide with a still-live session — see `session_resume_args`).
pub fn claude_session_path(home: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    let slug: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    home.join(".claude")
        .join("projects")
        .join(slug)
        .join(format!("{session_id}.jsonl"))
}

/// Whether any Codex rollout under `~/.codex/sessions` carries `session_id`.
/// Used to decide if a stored id is still resumable before `codex resume <id>`.
pub fn codex_session_exists(home: &Path, session_id: &str) -> bool {
    let root = home.join(".codex").join("sessions");
    if !root.is_dir() {
        return false;
    }
    let mut found = false;
    walk_jsonl(&root, &mut |path| {
        // The rollout filename embeds the session id, so a name match settles it
        // without opening the file (and works for compressed `.jsonl.zst` too).
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(session_id))
        {
            found = true;
            return true; // stop the walk
        }
        if let Some((id, _)) = codex_session_meta(path)
            && id == session_id
        {
            found = true;
            return true;
        }
        false
    });
    found
}

/// Latest saved display name for each Codex session id.
///
/// Codex appends an entry to `~/.codex/session_index.jsonl` when `/rename`
/// changes a thread name. Session ids make this a Codex-owned name source:
/// commands running inside the terminal cannot replace it with their own OSC
/// title. Later rows win because the index is append-only.
pub fn codex_session_names(
    home: &Path,
) -> std::io::Result<std::collections::HashMap<String, String>> {
    use std::io::{BufRead, BufReader};

    let mut names = std::collections::HashMap::new();
    let path = home.join(".codex").join("session_index.jsonl");
    let file = std::fs::File::open(path)?;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(name) = value
            .get("thread_name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        names.insert(id.to_string(), name.to_string());
    }
    Ok(names)
}

/// A Codex terminal title that directly identifies its session.
///
/// Codex publishes its agent-minted UUID as an OSC terminal title.
/// Capturing that title binds each pane to its own rollout even when several
/// Codex panes share a working directory. The normal pre-resume existence check
/// still rejects a stale or missing UUID before it reaches the CLI.
pub fn codex_session_id_from_title(preset: &AgentPreset, title: &str) -> Option<String> {
    if !preset.program.as_deref().is_some_and(is_codex_program) {
        return None;
    }
    let title = title.trim();
    if Uuid::parse_str(title).is_ok() {
        return Some(title.to_string());
    }

    // The invocation-local Codex contract is `thread | run-state · activity`.
    // Accept a UUID only when it owns the complete thread field. A UUID merely
    // mentioned inside a renamed thread must never rebind the pane.
    let (thread, state) = title.rsplit_once(" | ")?;
    let (run_state, activity) = state.split_once('·')?;
    if activity.contains('·') {
        return None;
    }
    let run_state = run_state.trim().to_ascii_lowercase();
    let activity = activity.trim();
    let activity_lower = activity.to_ascii_lowercase();
    let valid_state = (run_state == "ready"
        && matches!(activity_lower.as_str(), "" | "action required"))
        || (matches!(run_state.as_str(), "starting" | "working" | "thinking")
            && !activity.is_empty());
    if !valid_state {
        return None;
    }
    let thread = thread.trim();
    Uuid::parse_str(thread).ok().map(|_| thread.to_string())
}

/// Most recently modified Codex session id whose `session_meta.cwd` matches `cwd`.
///
/// This is a legacy recovery path for a pane that was already started before exact
/// title binding existed but has no saved session id. New and already-bound panes
/// use their exact captured id and never replace it with this cwd heuristic.
pub fn codex_latest_session_id(home: &Path, cwd: &Path) -> Option<String> {
    let root = home.join(".codex").join("sessions");
    if !root.is_dir() {
        return None;
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    walk_jsonl(&root, &mut |path| {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        let Ok(mtime) = meta.modified() else {
            return false;
        };
        let Some((id, session_cwd)) = codex_session_meta(path) else {
            return false;
        };
        if !paths_loosely_equal(Path::new(&session_cwd), cwd) {
            return false;
        }
        if best.as_ref().is_none_or(|(t, _)| mtime >= *t) {
            best = Some((mtime, id));
        }
        false // scan every rollout to find the newest
    });
    best.map(|(_, id)| id)
}

/// Walk Codex rollout files (`*.jsonl` and compressed `*.jsonl.zst`) under `dir`,
/// calling `visit` on each. `visit` returns `true` to stop the walk early (used by
/// existence checks); returns whether the walk was stopped.
fn walk_jsonl(dir: &Path, visit: &mut dyn FnMut(&Path) -> bool) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if walk_jsonl(&path, visit) {
                return true;
            }
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".jsonl") || n.ends_with(".jsonl.zst"))
            && visit(&path)
        {
            return true;
        }
    }
    false
}

/// First `session_meta` line in a Codex rollout → `(session_id, cwd)`.
fn codex_session_meta(path: &Path) -> Option<(String, String)> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(8) {
        let line = line.ok()?;
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v.get("type")?.as_str()? != "session_meta" {
            continue;
        }
        let payload = v.get("payload")?;
        let id = payload
            .get("session_id")
            .or_else(|| payload.get("id"))?
            .as_str()?
            .to_string();
        let cwd = payload.get("cwd")?.as_str()?.to_string();
        return Some((id, cwd));
    }
    None
}

fn paths_loosely_equal(a: &Path, b: &Path) -> bool {
    if let (Ok(ca), Ok(cb)) = (a.canonicalize(), b.canonicalize()) {
        return ca == cb;
    }
    // Fallback when a recorded cwd no longer exists on disk (canonicalize fails).
    // Normalize per the platform's own path semantics — Windows is case-
    // insensitive and separator-agnostic; Unix is case-sensitive with `/`. The
    // previous unconditional Windows-shaped normalization wrongly treated
    // `/x/ProjA` and `/x/proja` as equal on Linux.
    fn norm(p: &Path) -> String {
        let s = p.to_string_lossy();
        #[cfg(windows)]
        {
            s.replace('/', "\\")
                .trim_end_matches('\\')
                .to_ascii_lowercase()
        }
        #[cfg(not(windows))]
        {
            s.trim_end_matches('/').to_string()
        }
    }
    norm(a) == norm(b)
}

/// Directory (under the project root) holding muxel's per-project files.
pub const MEMORY_DIR: &str = ".muxel";
/// The shared per-project agent memory file, inside [`MEMORY_DIR`].
pub const MEMORY_FILE: &str = "MEMORY.md";

/// How to refer to a project's `.muxel/MEMORY.md` from inside an agent's system
/// prompt, given the project `root` and the `cwd` the agent will run in.
///
/// This lands in the agent's **argv** (via `--append-system-prompt`), and argv is
/// what `pkill -f <pattern>` matches. An absolute path puts the project's name in
/// every one of its agents' command lines, so an agent running a routine cleanup
/// like `pkill -f myproject` SIGKILLs every pane in the project — including its
/// own. Prefer a path relative to the agent's cwd, which names nothing.
///
/// Falls back to the absolute path when the memory file isn't under the cwd (an
/// instance running in a worktree), where a relative path simply wouldn't resolve.
pub fn memory_reference(root: &str, cwd: Option<&str>) -> String {
    let trimmed = root.trim_end_matches('/');
    let relative = format!("{MEMORY_DIR}/{MEMORY_FILE}");
    match cwd {
        Some(cwd) if cwd.trim_end_matches('/') != trimmed => {
            format!("{trimmed}/{relative}")
        }
        _ => relative,
    }
}

/// The system-prompt snippet appended to an agent's prompt when a project has
/// shared memory enabled. `path` is how the agent should refer to its project's
/// `.muxel/MEMORY.md` — see [`memory_reference`].
pub fn memory_instruction(path: &str) -> String {
    format!(
        "This project has a shared, muxel-maintained memory file at `{path}`, \
persisted across every agent and run here. At the start of a task, `grep -i` it for \
prior lessons, decisions, and gotchas relevant to what you're doing (each entry is a \
`##` section with a `tags=` line, so one grep finds it), then read that section. \
Whenever you learn something durable — a fix, a convention, a pitfall, an important \
detail — record it by adding a new `## Short Title` section with a concise note (a \
few keywords help future greps). muxel timestamps, orders (most-recently-used \
first), de-dupes, and prunes the file automatically, so don't renumber, reorder, or \
delete other entries, and don't repeat what's already there."
    )
}

/// System-prompt guidance for file citations muxel can open directly.
pub fn file_link_instruction() -> &'static str {
    "When citing a local file, use a Markdown link with an absolute file:/// URI \
and an optional source fragment such as #L12C4, for example \
[browser.rs:112](file:///D:/dev/muxel/crates/muxel/src/browser.rs#L112). Keep the \
label readable and never invent a target."
}

/// Add one capability instruction to the prompt bundle delivered by the
/// preset's configured transport.
pub fn append_agent_instruction(prompt: &mut Option<String>, instruction: String) {
    *prompt = Some(match prompt.take() {
        Some(base) if !base.is_empty() => format!("{base}\n\n{instruction}"),
        _ => instruction,
    });
}

/// Add instructions through Codex's one-run `--config` override.
///
/// An invalid TOML value is treated as a raw string by Codex. Keeping this
/// value unquoted matters on Windows: npm's `codex.cmd` reparses argv and turns
/// TOML's escaped inner quotes into shell syntax. A single line also avoids
/// command-script newline handling without changing the instruction.
pub fn codex_developer_instructions_override(instructions: &str) -> String {
    let flattened = instructions
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!("developer_instructions={flattened}")
}

/// Canonical Git for Windows install path, used when nothing is discoverable so
/// the preset still names a real target the user can correct in settings.
pub const GIT_BASH_DEFAULT_PATH: &str = r"C:\Program Files\Git\bin\bash.exe";

/// Absolute paths where Git for Windows' `bash.exe` may live, most preferred
/// first. Environment lookup is injected so this stays testable off Windows.
///
/// `System32\bash.exe` is deliberately unreachable from here: that name is the
/// WSL launcher, not Git Bash, so resolving a bare `bash` through `PATH` would
/// frequently start the wrong shell. Every candidate is an explicit Git layout.
pub fn git_bash_candidates(
    env: impl Fn(&str) -> Option<String>,
    git_on_path: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !out.contains(&path) {
            out.push(path);
        }
    };

    // An explicit override wins, so a portable or non-standard install is
    // reachable without editing the preset.
    if let Some(explicit) = env("MUXEL_GIT_BASH").filter(|value| !value.trim().is_empty()) {
        push(PathBuf::from(explicit.trim()));
    }

    // Git installs `git.exe` in `<root>\cmd` (and `<root>\bin`), with the bash
    // wrapper in `<root>\bin`. Deriving the root from whichever git is actually
    // on PATH covers installs in a custom directory.
    if let Some(root) = git_on_path.and_then(Path::parent).and_then(Path::parent) {
        push(root.join("bin").join("bash.exe"));
    }

    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(base) = env(var).filter(|value| !value.trim().is_empty()) {
            push(
                Path::new(base.trim())
                    .join("Git")
                    .join("bin")
                    .join("bash.exe"),
            );
        }
    }
    // Git for Windows' per-user install, which needs no administrator.
    if let Some(base) = env("LocalAppData").filter(|value| !value.trim().is_empty()) {
        push(
            Path::new(base.trim())
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    // Scoop keeps a stable `current` junction per app.
    if let Some(home) = env("USERPROFILE").filter(|value| !value.trim().is_empty()) {
        push(
            Path::new(home.trim())
                .join("scoop")
                .join("apps")
                .join("git")
                .join("current")
                .join("bin")
                .join("bash.exe"),
        );
    }
    out
}

/// First candidate that exists. `exists` is injected so the selection order is
/// testable without a Git for Windows install.
pub fn select_git_bash(
    candidates: Vec<PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates.into_iter().find(|path| exists(path))
}

/// Locate `git.exe` on `PATH`, so a Git installed outside the standard
/// directories still leads to its bundled bash.
fn git_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("git.exe"))
        .find(|candidate| candidate.is_file())
}

/// The Git Bash program a Windows shell preset should launch. Falls back to the
/// canonical install path when Git is absent, so the preset still points
/// somewhere meaningful rather than a bare `bash` that could resolve to WSL.
pub fn git_bash_program() -> String {
    select_git_bash(
        git_bash_candidates(|key| std::env::var(key).ok(), git_on_path().as_deref()),
        |path| path.is_file(),
    )
    .map(|path| path.display().to_string())
    .unwrap_or_else(|| GIT_BASH_DEFAULT_PATH.to_string())
}

fn is_codex_program(program: &str) -> bool {
    let leaf = program
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(leaf.as_str(), "codex" | "codex.exe" | "codex.cmd")
}

/// One-run Codex title contract consumed by Muxel's lifecycle parser.
/// Single-quoted TOML strings survive the npm `.cmd` wrapper on Windows.
pub fn codex_terminal_title_override() -> &'static str {
    "tui.terminal_title=['thread','run-state','activity']"
}

/// Seed contents written when a project's `MEMORY.md` is first created. Delegates to
/// the memory model so the seeded file matches muxel's maintained format exactly.
pub fn memory_header() -> &'static str {
    crate::memory::document_header()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn instance(preset: &AgentPreset, prompt: Option<&str>) -> Instance {
        let mut i = Instance::from_preset(Uuid::new_v4(), preset);
        i.system_prompt = prompt.map(|p| p.to_string());
        i
    }

    #[test]
    fn codex_title_override_is_an_argv_safe_toml_array() {
        assert_eq!(
            codex_terminal_title_override(),
            "tui.terminal_title=['thread','run-state','activity']"
        );
        let launch = resolve_launch(&instance(&AgentPreset::codex(), None));
        assert!(launch.args.windows(2).any(|pair| {
            pair == [
                "--config",
                "tui.terminal_title=['thread','run-state','activity']",
            ]
        }));
    }

    #[test]
    fn codex_session_id_is_extracted_from_semantic_title() {
        let id = Uuid::new_v4().to_string();
        let title = format!("{id} | Working · Responding");
        assert_eq!(
            codex_session_id_from_title(&AgentPreset::codex(), &title).as_deref(),
            Some(id.as_str())
        );

        assert_eq!(
            codex_session_id_from_title(
                &AgentPreset::codex(),
                &format!("Review {id} carefully | Ready ·")
            ),
            None
        );
        assert_eq!(
            codex_session_id_from_title(
                &AgentPreset {
                    program: Some("codex-title-proxy.exe".to_string()),
                    ..AgentPreset::codex()
                },
                &title
            ),
            None
        );
        assert_eq!(
            codex_session_id_from_title(
                &AgentPreset::codex(),
                &format!("{id} | unowned child title")
            ),
            None
        );
    }

    #[test]
    fn claude_preset_supports_resume() {
        let c = AgentPreset::claude();
        assert_eq!(c.session_id_flag.as_deref(), Some("--session-id"));
        assert_eq!(c.resume_flag.as_deref(), Some("--resume"));
        assert!(AgentPreset::shell().session_id_flag.is_none());
    }

    #[test]
    fn grok_preset_supports_resume() {
        let g = AgentPreset::grok();
        assert_eq!(g.session_id_flag.as_deref(), Some("--session-id"));
        assert_eq!(g.resume_flag.as_deref(), Some("--resume"));
        // Same flag shape as Claude, so the shared session_resume_args path applies.
        let mut inst = instance(&g, None);
        inst.session_id = Some("abc".to_string());
        assert_eq!(
            session_resume_args(&g, &inst),
            Some(vec!["--session-id".to_string(), "abc".to_string()])
        );
        inst.session_started = true;
        assert_eq!(
            session_resume_args(&g, &inst),
            Some(vec!["--resume".to_string(), "abc".to_string()])
        );
    }

    #[test]
    fn cmd_preset_runs_cmd_exe() {
        let c = AgentPreset::cmd();
        assert_eq!(c.name, "Cmd");
        assert_eq!(c.program.as_deref(), Some("cmd.exe"));
    }

    #[test]
    fn windows_shell_presets() {
        // The default-shell preset is PowerShell on Windows, Shell elsewhere; it
        // always runs via CommandSpec::shell (program: None). Cmd is seeded only
        // on Windows, where the user gets both PowerShell and Cmd.
        let defaults = AgentPreset::defaults();
        let names: Vec<&str> = defaults.iter().map(|p| p.name.as_str()).collect();
        assert!(AgentPreset::shell().program.is_none());
        if cfg!(windows) {
            assert_eq!(AgentPreset::shell().name, "PowerShell");
            assert!(names.contains(&"PowerShell"));
            assert!(names.contains(&"Cmd"));
            assert!(names.contains(&"Git Bash"));
            assert!(!names.contains(&"Shell"));
        } else {
            assert_eq!(AgentPreset::shell().name, "Shell");
            assert!(names.contains(&"Shell"));
            assert!(!names.contains(&"Cmd"));
            assert!(!names.contains(&"Git Bash"));
        }
    }

    #[test]
    fn git_bash_preset_keeps_its_pane_directory_and_gets_a_login_shell() {
        let preset = AgentPreset::git_bash();
        assert_eq!(preset.name, "Git Bash");
        // A login shell is what populates the MSYS PATH; without it the pane has
        // none of the Unix tools that are the point of Git Bash.
        assert_eq!(preset.args, vec!["-i".to_string(), "-l".to_string()]);
        // Without CHERE_INVOKING, /etc/profile cd's to $HOME and the pane leaves
        // the project or worktree muxel opened it in.
        assert_eq!(
            preset
                .env
                .iter()
                .find(|var| var.key == "CHERE_INVOKING")
                .map(|var| var.value.as_str()),
            Some("1")
        );
        // Never a bare `bash`: on Windows that resolves to the WSL launcher.
        let program = preset.program.expect("git bash preset names a program");
        assert!(
            program.contains('\\') || program.contains('/'),
            "expected an absolute path, got {program:?}"
        );
    }

    #[test]
    fn git_bash_candidates_prefer_the_override_then_the_git_on_path_root() {
        let env = |key: &str| match key {
            "MUXEL_GIT_BASH" => Some(r"D:\portable\git\bin\bash.exe".to_string()),
            "ProgramFiles" => Some(r"C:\Program Files".to_string()),
            _ => None,
        };
        // Forward slashes so `Path::parent` splits this on every host: Windows
        // accepts both separators, but a `\` is an ordinary character elsewhere
        // and the test would silently stop exercising the root derivation.
        let git = PathBuf::from("E:/tools/Git/cmd/git.exe");
        let candidates = git_bash_candidates(env, Some(&git));

        assert_eq!(
            candidates[0],
            PathBuf::from(r"D:\portable\git\bin\bash.exe")
        );
        assert_eq!(
            candidates[1],
            PathBuf::from("E:/tools/Git").join("bin").join("bash.exe")
        );
        assert!(
            candidates.contains(
                &PathBuf::from(r"C:\Program Files")
                    .join("Git")
                    .join("bin")
                    .join("bash.exe")
            )
        );
    }

    #[test]
    fn git_bash_candidates_never_offer_the_wsl_launcher() {
        // A PATH whose only `bash.exe` is System32's WSL launcher must not
        // produce it as a Git Bash candidate, whatever else is set.
        let env = |key: &str| match key {
            "ProgramFiles" => Some(r"C:\Program Files".to_string()),
            "LocalAppData" => Some(r"C:\Users\dev\AppData\Local".to_string()),
            "USERPROFILE" => Some(r"C:\Users\dev".to_string()),
            _ => None,
        };
        let candidates = git_bash_candidates(env, None);

        assert!(!candidates.is_empty());
        for candidate in &candidates {
            let lower = candidate.display().to_string().to_ascii_lowercase();
            assert!(
                !lower.contains("system32"),
                "WSL launcher offered: {candidate:?}"
            );
            assert!(lower.ends_with("bash.exe"));
        }
    }

    #[test]
    fn git_bash_candidates_dedupe_when_roots_overlap() {
        // ProgramFiles and ProgramW6432 are the same directory on 64-bit installs.
        let env = |key: &str| match key {
            "ProgramFiles" | "ProgramW6432" => Some(r"C:\Program Files".to_string()),
            _ => None,
        };
        let candidates = git_bash_candidates(env, None);
        assert_eq!(candidates.len(), 1, "expected dedupe, got {candidates:?}");
    }

    #[test]
    fn select_git_bash_takes_the_first_existing_candidate() {
        let missing = PathBuf::from(r"D:\nope\bin\bash.exe");
        let present = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        let candidates = vec![missing.clone(), present.clone()];

        assert_eq!(
            select_git_bash(candidates.clone(), |path| path == present),
            Some(present)
        );
        assert_eq!(select_git_bash(candidates, |_| false), None);
    }

    #[test]
    fn session_resume_args_session_id_then_resume() {
        let preset = AgentPreset::claude();
        let mut inst = instance(&preset, None);
        // No session id yet → nothing to add.
        assert_eq!(session_resume_args(&preset, &inst), None);
        // First launch (not started): start the session with a chosen id.
        inst.session_id = Some("abc".to_string());
        assert_eq!(
            session_resume_args(&preset, &inst),
            Some(vec!["--session-id".to_string(), "abc".to_string()])
        );
        // Any later launch (started): resume by id — no on-disk probe, so a
        // not-yet-flushed session can't be mistaken for a fresh one.
        inst.session_started = true;
        assert_eq!(
            session_resume_args(&preset, &inst),
            Some(vec!["--resume".to_string(), "abc".to_string()])
        );
        // A non-resume agent (shell) never gets resume args.
        let shell = AgentPreset::shell();
        let mut s = instance(&shell, None);
        s.session_id = Some("abc".to_string());
        s.session_started = true;
        assert_eq!(session_resume_args(&shell, &s), None);
    }

    #[test]
    fn codex_preset_is_agent_minted_resume() {
        let c = AgentPreset::codex();
        assert_eq!(c.program.as_deref(), Some("codex"));
        assert!(c.session_id_flag.is_none());
        assert_eq!(c.resume_flag.as_deref(), Some("resume"));
        let mut inst = instance(&c, None);
        // First launch: bare — Codex mints its own id.
        assert_eq!(session_resume_args(&c, &inst), None);
        // After capture: resume subcommand + id.
        inst.session_id = Some("abc".to_string());
        inst.session_started = true;
        assert_eq!(
            session_resume_args(&c, &inst),
            Some(vec!["resume".to_string(), "abc".to_string()])
        );
    }

    #[test]
    fn codex_session_id_comes_from_uuid_terminal_title() {
        let codex = AgentPreset::codex();
        let id = Uuid::new_v4().to_string();
        assert_eq!(
            codex_session_id_from_title(&codex, &format!(" {id} ")).as_deref(),
            Some(id.as_str())
        );
        assert_eq!(codex_session_id_from_title(&codex, "Review changes"), None);
        assert_eq!(
            codex_session_id_from_title(&AgentPreset::claude(), &id),
            None
        );
    }

    #[test]
    fn codex_latest_session_id_picks_matching_cwd() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("muxel-codex-test-{}", Uuid::new_v4()));
        let day = tmp
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("07")
            .join("09");
        std::fs::create_dir_all(&day).unwrap();
        let cwd = if cfg!(windows) {
            PathBuf::from(r"D:\dev\proj")
        } else {
            PathBuf::from("/home/u/proj")
        };
        let other = if cfg!(windows) {
            PathBuf::from(r"D:\other")
        } else {
            PathBuf::from("/home/u/other")
        };
        // Older matching session.
        let older = day.join("rollout-old-aaaa.jsonl");
        let mut f = std::fs::File::create(&older).unwrap();
        writeln!(
            f,
            r#"{{"type":"session_meta","payload":{{"session_id":"id-old","cwd":"{}"}}}}"#,
            cwd.display().to_string().replace('\\', "\\\\")
        )
        .unwrap();
        // Newer matching session.
        let newer = day.join("rollout-new-bbbb.jsonl");
        // Ensure newer mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut f = std::fs::File::create(&newer).unwrap();
        writeln!(
            f,
            r#"{{"type":"session_meta","payload":{{"session_id":"id-new","cwd":"{}"}}}}"#,
            cwd.display().to_string().replace('\\', "\\\\")
        )
        .unwrap();
        // Different cwd — ignored.
        let distractor = day.join("rollout-other-cccc.jsonl");
        let mut f = std::fs::File::create(&distractor).unwrap();
        writeln!(
            f,
            r#"{{"type":"session_meta","payload":{{"session_id":"id-other","cwd":"{}"}}}}"#,
            other.display().to_string().replace('\\', "\\\\")
        )
        .unwrap();

        assert_eq!(
            codex_latest_session_id(&tmp, &cwd).as_deref(),
            Some("id-new")
        );
        assert!(codex_session_exists(&tmp, "id-new"));
        assert!(!codex_session_exists(&tmp, "missing"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn codex_session_names_use_the_latest_valid_index_row() {
        use std::io::Write;

        let tmp = std::env::temp_dir().join(format!("muxel-codex-index-{}", Uuid::new_v4()));
        let index_dir = tmp.join(".codex");
        std::fs::create_dir_all(&index_dir).unwrap();
        let mut index = std::fs::File::create(index_dir.join("session_index.jsonl")).unwrap();
        writeln!(
            index,
            r#"{{"id":"session-a","thread_name":"First name","updated_at":"1"}}"#
        )
        .unwrap();
        writeln!(index, "not json").unwrap();
        writeln!(
            index,
            r#"{{"id":"session-b","thread_name":"Other name","updated_at":"2"}}"#
        )
        .unwrap();
        writeln!(
            index,
            r#"{{"id":"session-a","thread_name":"  foo  ","updated_at":"3"}}"#
        )
        .unwrap();
        writeln!(
            index,
            r#"{{"id":"session-a","thread_name":"   ","updated_at":"4"}}"#
        )
        .unwrap();

        let names = codex_session_names(&tmp).unwrap();
        assert_eq!(names.get("session-a").map(String::as_str), Some("foo"));
        assert_eq!(
            names.get("session-b").map(String::as_str),
            Some("Other name")
        );
        assert_eq!(names.len(), 2);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn paths_loosely_equal_respects_platform_case() {
        use std::path::Path;
        // A trailing separator is ignored on both platforms.
        assert!(super::paths_loosely_equal(
            Path::new("/x/proj/"),
            Path::new("/x/proj")
        ));
        // Case sensitivity follows the platform: Unix distinguishes `ProjA` from
        // `proja` (the old fallback wrongly lowercased and merged them); Windows
        // treats them as the same path. These paths don't exist, so this exercises
        // the canonicalize-failure fallback specifically.
        assert_eq!(
            super::paths_loosely_equal(Path::new("/x/ProjA"), Path::new("/x/proja")),
            cfg!(windows)
        );
    }

    #[test]
    fn claude_session_path_encodes_cwd() {
        use std::path::Path;
        let p = super::claude_session_path(
            Path::new("/home/u"),
            Path::new("/home/ryan/Projects/muxel"),
            "abc-123",
        );
        assert_eq!(
            p,
            Path::new("/home/u/.claude/projects/-home-ryan-Projects-muxel/abc-123.jsonl")
        );
        // A worktree path: '/' and '.' both collapse to '-' (so '/.' becomes '--').
        let w = super::claude_session_path(
            Path::new("/h"),
            Path::new("/home/ryan/.local/share/x"),
            "id",
        );
        assert_eq!(
            w,
            Path::new("/h/.claude/projects/-home-ryan--local-share-x/id.jsonl")
        );
    }

    #[test]
    fn cli_flag_appends_flag_and_prompt() {
        let r = resolve_launch(&instance(&AgentPreset::claude(), Some("be terse")));
        assert_eq!(r.program.as_deref(), Some("claude"));
        assert_eq!(
            r.args,
            vec!["--append-system-prompt".to_string(), "be terse".to_string()]
        );
        assert_eq!(r.startup_input, None);
    }

    #[test]
    fn type_in_sets_startup_input() {
        let r = resolve_launch(&instance(&AgentPreset::opencode(), Some("hello there")));
        assert_eq!(r.program.as_deref(), Some("opencode"));
        assert!(r.args.is_empty());
        assert_eq!(r.startup_input.as_deref(), Some("hello there"));
    }

    #[test]
    fn type_in_does_not_submit_another_user_turn_on_resume() {
        let r = resolve_launch_for_session(
            &instance(&AgentPreset::opencode(), Some("hello there")),
            true,
        );
        assert!(r.args.is_empty());
        assert_eq!(r.startup_input, None);
    }

    #[test]
    fn cli_flag_remains_launch_configuration_on_resume() {
        let r =
            resolve_launch_for_session(&instance(&AgentPreset::claude(), Some("be terse")), true);
        assert_eq!(
            r.args,
            vec!["--append-system-prompt".to_string(), "be terse".to_string()]
        );
        assert_eq!(r.startup_input, None);
    }

    #[test]
    fn prompt_transports_keep_multiline_bundles_to_one_turn() {
        let cli = resolve_launch(&instance(&AgentPreset::claude(), Some("one\n\ntwo")));
        assert_eq!(cli.args.last().map(String::as_str), Some("one two"));
        let typed = resolve_launch(&instance(&AgentPreset::opencode(), Some("one\n\ntwo")));
        assert_eq!(typed.startup_input.as_deref(), Some("one two"));
    }

    #[test]
    fn codex_developer_instructions_use_a_batch_safe_raw_config_value() {
        assert_eq!(
            codex_developer_instructions_override("open \"D:\\dev\\x\"\nnext"),
            "developer_instructions=open \"D:\\dev\\x\" next"
        );
    }

    #[test]
    fn combines_custom_prompt_and_capabilities_once_for_any_transport() {
        let mut prompt = Some("custom rules".to_string());
        append_agent_instruction(&mut prompt, "file links".to_string());
        append_agent_instruction(&mut prompt, "project memory".to_string());
        assert_eq!(
            prompt.as_deref(),
            Some("custom rules\n\nfile links\n\nproject memory")
        );
    }

    #[test]
    fn no_prompt_injects_nothing() {
        let r = resolve_launch(&instance(&AgentPreset::claude(), None));
        assert!(r.args.is_empty());
        assert_eq!(r.startup_input, None);
    }

    #[test]
    fn empty_prompt_injects_nothing() {
        let r = resolve_launch(&instance(&AgentPreset::opencode(), Some("")));
        assert_eq!(r.startup_input, None);
    }

    #[test]
    fn shell_has_no_program() {
        let r = resolve_launch(&instance(
            &AgentPreset::shell(),
            Some("ignored-no-injection"),
        ));
        assert_eq!(r.program, None);
        assert!(r.args.is_empty());
        assert_eq!(r.startup_input, None);
    }

    #[test]
    fn compose_args_orders_model_effort_extra() {
        let mut p = AgentPreset::claude();
        p.model = Some("claude-opus-4-8".into());
        p.effort = Some("high".into());
        p.effort_flag = Some("--effort".into());
        p.args = vec!["--foo".into(), "bar".into()];
        assert_eq!(
            p.compose_args(),
            vec![
                "--model",
                "claude-opus-4-8",
                "--effort",
                "high",
                "--foo",
                "bar"
            ]
        );
    }

    #[test]
    fn compose_args_skips_unset_model_and_effort() {
        // Claude has a model_flag but no model set, and no effort_flag.
        assert!(AgentPreset::claude().compose_args().is_empty());
    }

    #[test]
    fn ollama_code_runs_an_agent_with_a_model() {
        let p = AgentPreset::ollama_code();
        assert_eq!(p.program.as_deref(), Some("ollama"));
        // `--model` must follow the `launch` subcommand + agent, so the whole line
        // lives in args (the model field can't place the flag after them).
        let r = resolve_launch(&instance(&p, None));
        assert_eq!(r.program.as_deref(), Some("ollama"));
        assert_eq!(r.args, ["launch", "opencode", "--model", "glm-5.2:cloud"]);
        // It's part of the seeded defaults so existing users get it on upgrade.
        assert!(
            AgentPreset::defaults()
                .iter()
                .any(|p| p.name == "Ollama Code")
        );
    }

    #[test]
    fn resolve_launch_carries_env() {
        let mut i = Instance::from_preset(Uuid::new_v4(), &AgentPreset::shell());
        i.env = vec![EnvVar {
            key: "FOO".into(),
            value: "bar".into(),
        }];
        let r = resolve_launch(&i);
        assert_eq!(r.env, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn memory_instruction_carries_path_and_guidance() {
        let s = memory_instruction("/srv/app/.muxel/MEMORY.md");
        assert!(s.contains("/srv/app/.muxel/MEMORY.md"));
        assert!(s.contains("grep"));
        assert!(s.contains("## "));
    }

    #[test]
    fn memory_reference_is_relative_when_the_agent_starts_at_the_project_root() {
        assert_eq!(
            memory_reference("/srv/app", Some("/srv/app")),
            ".muxel/MEMORY.md"
        );
        // No cwd recorded → the agent runs at the root by default.
        assert_eq!(memory_reference("/srv/app", None), ".muxel/MEMORY.md");
        // A trailing slash on either side is still the same directory.
        assert_eq!(
            memory_reference("/srv/app/", Some("/srv/app")),
            ".muxel/MEMORY.md"
        );
    }

    #[test]
    fn memory_reference_stays_absolute_for_a_worktree_cwd() {
        // The memory file lives at the project root, outside the worktree, so a
        // relative path would not resolve from there.
        assert_eq!(
            memory_reference("/srv/app", Some("/srv/worktrees/app-feature")),
            "/srv/app/.muxel/MEMORY.md"
        );
    }

    /// Regression: the instruction goes into the agent's argv, and `pkill -f` matches
    /// argv. If the project's path is in there, an agent running `pkill -f <project>`
    /// (a routine "kill my dev server" cleanup) SIGKILLs every pane in the project,
    /// its own included — four at once, indistinguishable from four crashes.
    #[test]
    fn memory_instruction_keeps_the_project_name_out_of_argv() {
        let root = "/home/me/Projects/sro_client";
        let s = memory_instruction(&memory_reference(root, Some(root)));
        assert!(
            !s.contains("sro_client"),
            "project name leaked into the agent's argv: {s}"
        );
    }
}
