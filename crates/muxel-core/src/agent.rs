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
}

impl AgentPreset {
    /// The default-shell preset: `program: None` flows through
    /// [`CommandSpec::shell`], the OS default shell. Named "PowerShell" on Windows
    /// (where that's the default), "Shell" elsewhere.
    pub fn shell() -> Self {
        Self {
            id: Uuid::new_v4(),
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

    pub fn claude() -> Self {
        Self {
            id: Uuid::new_v4(),
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
            name: "Grok".to_string(),
            program: Some("grok".to_string()),
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

    pub fn defaults() -> Vec<AgentPreset> {
        let mut presets = vec![Self::shell()];
        // On Windows, offer cmd.exe alongside the PowerShell default.
        #[cfg(windows)]
        presets.push(Self::cmd());
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
    let mut args = instance.args.clone();
    let mut startup_input = None;

    if let Some(prompt) = instance.system_prompt.as_ref().filter(|p| !p.is_empty()) {
        match &instance.injection {
            InjectionMode::CliFlag { flag } => {
                args.push(flag.clone());
                args.push(prompt.clone());
            }
            InjectionMode::TypeIn => startup_input = Some(prompt.clone()),
            InjectionMode::None => {}
        }
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
        if found {
            return;
        }
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains(session_id))
        {
            found = true;
            return;
        }
        if let Some((id, _)) = codex_session_meta(path) {
            if id == session_id {
                found = true;
            }
        }
    });
    found
}

/// Most recently modified Codex session id whose `session_meta.cwd` matches `cwd`.
///
/// Codex mints its own UUID on first launch (no host-side `--session-id`), so on
/// restart muxel adopts the latest rollout for this working directory. Multiple
/// concurrent Codex panes in the *same* cwd may collide on that heuristic — keep
/// one Codex pane per project for reliable autoresume.
pub fn codex_latest_session_id(home: &Path, cwd: &Path) -> Option<String> {
    let root = home.join(".codex").join("sessions");
    if !root.is_dir() {
        return None;
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    walk_jsonl(&root, &mut |path| {
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        let Ok(mtime) = meta.modified() else {
            return;
        };
        let Some((id, session_cwd)) = codex_session_meta(path) else {
            return;
        };
        if !paths_loosely_equal(Path::new(&session_cwd), cwd) {
            return;
        }
        if best.as_ref().is_none_or(|(t, _)| mtime >= *t) {
            best = Some((mtime, id));
        }
    });
    best.map(|(_, id)| id)
}

fn walk_jsonl(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, visit);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            visit(&path);
        }
    }
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
    fn norm(p: &Path) -> String {
        let s = p.to_string_lossy();
        let s = s.replace('/', "\\");
        s.trim_end_matches(['\\', '/']).to_ascii_lowercase()
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
            assert!(!names.contains(&"Shell"));
        } else {
            assert_eq!(AgentPreset::shell().name, "Shell");
            assert!(names.contains(&"Shell"));
            assert!(!names.contains(&"Cmd"));
        }
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
    fn codex_latest_session_id_picks_matching_cwd() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join(format!("muxel-codex-test-{}", Uuid::new_v4()));
        let day = tmp.join(".codex").join("sessions").join("2026").join("07").join("09");
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
