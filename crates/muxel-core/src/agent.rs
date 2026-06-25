//! Agent presets and system-prompt injection.
//!
//! A [`AgentPreset`] is a template for launching an agent (Claude, opencode, a
//! plain shell, …). [`resolve_launch`] turns an [`Instance`] into the concrete
//! program/args plus any text to type in at startup, applying the configured
//! [`InjectionMode`] for the system prompt.

use crate::Instance;
use serde::{Deserialize, Serialize};
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
    /// `--session-id <uuid>`). Paired with `resume_flag`, it lets muxel give each
    /// pane a stable session and resume it on restart. `None` = no resume support.
    #[serde(default)]
    pub session_id_flag: Option<String>,
    /// CLI flag that resumes a conversation by session ID (e.g. Claude's
    /// `--resume <uuid>`). Only meaningful alongside `session_id_flag`.
    #[serde(default)]
    pub resume_flag: Option<String>,
}

impl AgentPreset {
    pub fn shell() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: "Shell".to_string(),
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
            working_markers: Vec::new(),
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
            session_id_flag: None,
            resume_flag: None,
        }
    }

    pub fn defaults() -> Vec<AgentPreset> {
        vec![
            Self::shell(),
            Self::claude(),
            Self::opencode(),
            Self::amp(),
            Self::grok(),
            Self::hermes(),
            Self::ollama(),
            Self::pi(),
        ]
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
/// `None` when the preset has no resume support (`session_id_flag` / `resume_flag`
/// unset) or the instance has no session id yet. Otherwise `[resume_flag, id]` when
/// the session was already started and still exists on disk, else
/// `[session_id_flag, id]` — so a fresh or vanished session starts anew while
/// keeping the same id (the pane's stable slot).
pub fn session_resume_args(
    preset: &AgentPreset,
    instance: &Instance,
    session_exists: bool,
) -> Option<Vec<String>> {
    let id_flag = preset.session_id_flag.as_deref()?;
    let resume_flag = preset.resume_flag.as_deref()?;
    let id = instance.session_id.as_deref()?;
    let flag = if instance.session_started && session_exists {
        resume_flag
    } else {
        id_flag
    };
    Some(vec![flag.to_string(), id.to_string()])
}

/// Directory (under the project root) holding muxel's per-project files.
pub const MEMORY_DIR: &str = ".muxel";
/// The shared per-project agent memory file, inside [`MEMORY_DIR`].
pub const MEMORY_FILE: &str = "MEMORY.md";

/// The system-prompt snippet appended to an agent's prompt when a project has
/// shared memory enabled. `path` is the absolute path to the project's
/// `.muxel/MEMORY.md` on whichever host the agent runs.
pub fn memory_instruction(path: &str) -> String {
    format!(
        "This project has a shared memory file at `{path}`, persisted across every \
agent and run here. At the start of a task, read it for prior lessons, decisions, \
and gotchas about this project. Whenever you learn something durable — a fix, a \
convention, a pitfall, an important detail — append a short entry to it. Keep \
entries concise and don't repeat what's already there."
    )
}

/// Seed contents written when a project's `MEMORY.md` is first created.
pub fn memory_header() -> &'static str {
    "# Project memory\n\nShared notes for agents working on this project. Append \
durable lessons, decisions, and gotchas below.\n"
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
    fn session_resume_args_session_id_then_resume() {
        let preset = AgentPreset::claude();
        let mut inst = instance(&preset, None);
        // No session id yet → nothing to add.
        assert_eq!(session_resume_args(&preset, &inst, true), None);
        // First launch (not started): start the session with a chosen id.
        inst.session_id = Some("abc".to_string());
        assert_eq!(
            session_resume_args(&preset, &inst, true),
            Some(vec!["--session-id".to_string(), "abc".to_string()])
        );
        // Started + still on disk: resume it.
        inst.session_started = true;
        assert_eq!(
            session_resume_args(&preset, &inst, true),
            Some(vec!["--resume".to_string(), "abc".to_string()])
        );
        // Started but the session vanished: start fresh with the same id.
        assert_eq!(
            session_resume_args(&preset, &inst, false),
            Some(vec!["--session-id".to_string(), "abc".to_string()])
        );
        // A non-resume agent (shell) never gets resume args.
        let shell = AgentPreset::shell();
        let mut s = instance(&shell, None);
        s.session_id = Some("abc".to_string());
        s.session_started = true;
        assert_eq!(session_resume_args(&shell, &s, true), None);
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
        assert!(s.contains("read it"));
        assert!(s.contains("append"));
    }
}
