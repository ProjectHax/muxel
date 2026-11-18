//! Which folders each coding agent reads skills from.
//!
//! The table follows each agent's own loader (checked against the installed
//! claude, opencode, amp, grok, pi and hermes builds) and the `skills` CLI's
//! agent table for the rest. The shared folder `~/.agents/skills` (and
//! `<project>/.agents/skills`) is read by most agents; Claude Code and Hermes
//! read only their own.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{is_within, join, norm};
use crate::RemoteOs;

/// A coding agent that loads Agent Skills.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillAgent {
    Claude,
    Codex,
    Opencode,
    Amp,
    Grok,
    Pi,
    Hermes,
    Gemini,
    Cursor,
    Copilot,
    Droid,
}

impl SkillAgent {
    /// Every agent, in the order the UI lists them (and the planner prefers
    /// their folders).
    pub const ALL: [SkillAgent; 11] = [
        SkillAgent::Claude,
        SkillAgent::Codex,
        SkillAgent::Opencode,
        SkillAgent::Amp,
        SkillAgent::Grok,
        SkillAgent::Pi,
        SkillAgent::Hermes,
        SkillAgent::Gemini,
        SkillAgent::Cursor,
        SkillAgent::Copilot,
        SkillAgent::Droid,
    ];

    /// The product name (not translated).
    pub fn label(self) -> &'static str {
        match self {
            SkillAgent::Claude => "Claude Code",
            SkillAgent::Codex => "Codex",
            SkillAgent::Opencode => "opencode",
            SkillAgent::Amp => "Amp",
            SkillAgent::Grok => "Grok",
            SkillAgent::Pi => "pi",
            SkillAgent::Hermes => "Hermes",
            SkillAgent::Gemini => "Gemini CLI",
            SkillAgent::Cursor => "Cursor",
            SkillAgent::Copilot => "Copilot CLI",
            SkillAgent::Droid => "Factory Droid",
        }
    }

    /// The command that runs it, used to detect it on `PATH`.
    pub fn program(self) -> &'static str {
        match self {
            SkillAgent::Claude => "claude",
            SkillAgent::Codex => "codex",
            SkillAgent::Opencode => "opencode",
            SkillAgent::Amp => "amp",
            SkillAgent::Grok => "grok",
            SkillAgent::Pi => "pi",
            SkillAgent::Hermes => "hermes",
            SkillAgent::Gemini => "gemini",
            SkillAgent::Cursor => "cursor-agent",
            SkillAgent::Copilot => "copilot",
            SkillAgent::Droid => "droid",
        }
    }

    /// The agent a program (by basename, without `.exe`) belongs to.
    pub fn from_program(program: &str) -> Option<SkillAgent> {
        let base = super::file_name(program);
        let base = base.strip_suffix(".exe").unwrap_or(base);
        SkillAgent::ALL.into_iter().find(|a| a.program() == base)
    }
}

/// User-level skills, or one project's.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    User,
    /// The project's root folder.
    Project(String),
}

/// Environment variables that move an agent's folders. A remote probe reads
/// these on the host; locally they come from muxel's own environment.
pub const HOME_ENV_VARS: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "GROK_HOME",
    "PI_CODING_AGENT_DIR",
    "HERMES_HOME",
];

/// Where one machine's agents keep their skills: its home folder, the
/// environment overrides that apply there, and its OS family.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillHome {
    pub home: String,
    /// Only [`HOME_ENV_VARS`] are consulted; empty values count as unset.
    pub env: BTreeMap<String, String>,
    pub os: RemoteOs,
}

impl SkillHome {
    pub fn new(home: impl Into<String>, os: RemoteOs) -> Self {
        SkillHome {
            home: norm(&home.into()),
            env: BTreeMap::new(),
            os,
        }
    }

    pub fn with_env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.insert(key.to_string(), value.into());
        self
    }

    fn var(&self, key: &str) -> Option<String> {
        self.env
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(norm)
    }

    fn home_join(&self, rel: &str) -> String {
        join(&self.home, rel)
    }

    /// `$XDG_CONFIG_HOME`, else `~/.config` (agents use it on every OS).
    fn config_home(&self) -> String {
        self.var("XDG_CONFIG_HOME")
            .unwrap_or_else(|| self.home_join(".config"))
    }

    fn claude_home(&self) -> String {
        self.var("CLAUDE_CONFIG_DIR")
            .unwrap_or_else(|| self.home_join(".claude"))
    }

    fn codex_home(&self) -> String {
        self.var("CODEX_HOME")
            .unwrap_or_else(|| self.home_join(".codex"))
    }

    fn grok_home(&self) -> String {
        self.var("GROK_HOME")
            .unwrap_or_else(|| self.home_join(".grok"))
    }

    fn pi_home(&self) -> String {
        self.var("PI_CODING_AGENT_DIR")
            .unwrap_or_else(|| self.home_join(".pi/agent"))
    }

    fn hermes_home(&self) -> String {
        self.var("HERMES_HOME")
            .unwrap_or_else(|| self.home_join(".hermes"))
    }

    /// The folder whose existence means the agent is set up on this machine.
    pub fn agent_home(&self, agent: SkillAgent) -> String {
        match agent {
            SkillAgent::Claude => self.claude_home(),
            SkillAgent::Codex => self.codex_home(),
            SkillAgent::Opencode => join(&self.config_home(), "opencode"),
            SkillAgent::Amp => join(&self.config_home(), "amp"),
            SkillAgent::Grok => self.grok_home(),
            SkillAgent::Pi => self.pi_home(),
            SkillAgent::Hermes => self.hermes_home(),
            SkillAgent::Gemini => self.home_join(".gemini"),
            SkillAgent::Cursor => self.home_join(".cursor"),
            SkillAgent::Copilot => self.home_join(".copilot"),
            SkillAgent::Droid => self.home_join(".factory"),
        }
    }

    /// The shared folder most agents read: `~/.agents/skills` or
    /// `<project>/.agents/skills`. The `skills` CLI keeps its canonical copies
    /// here.
    pub fn shared_dir(&self, scope: &Scope) -> String {
        match scope {
            Scope::User => self.home_join(".agents/skills"),
            Scope::Project(root) => join(root, ".agents/skills"),
        }
    }

    /// Every folder the agent reads skills from, its own first. Empty when the
    /// agent has no skills at that scope (Hermes has no project skills).
    pub fn read_dirs(&self, agent: SkillAgent, scope: &Scope) -> Vec<String> {
        let shared = self.shared_dir(scope);
        let dirs: Vec<String> = match scope {
            Scope::User => {
                // The literal `~/.claude/skills`, which other agents read for
                // compatibility regardless of `CLAUDE_CONFIG_DIR`.
                let claude_compat = self.home_join(".claude/skills");
                match agent {
                    SkillAgent::Claude => vec![join(&self.claude_home(), "skills")],
                    SkillAgent::Codex => vec![shared, join(&self.codex_home(), "skills")],
                    SkillAgent::Opencode => {
                        let oc = join(&self.config_home(), "opencode");
                        vec![
                            join(&oc, "skills"),
                            join(&oc, "skill"),
                            claude_compat,
                            shared,
                        ]
                    }
                    SkillAgent::Amp => vec![
                        join(&self.config_home(), "amp/skills"),
                        join(&self.config_home(), "agents/skills"),
                        shared,
                        claude_compat,
                    ],
                    SkillAgent::Grok => vec![
                        join(&self.grok_home(), "skills"),
                        shared,
                        claude_compat,
                        self.home_join(".cursor/skills"),
                    ],
                    SkillAgent::Pi => vec![join(&self.pi_home(), "skills"), shared],
                    SkillAgent::Hermes => vec![join(&self.hermes_home(), "skills")],
                    SkillAgent::Gemini => vec![self.home_join(".gemini/skills"), shared],
                    SkillAgent::Cursor => vec![self.home_join(".cursor/skills"), shared],
                    SkillAgent::Copilot => vec![self.home_join(".copilot/skills"), shared],
                    SkillAgent::Droid => vec![self.home_join(".factory/skills"), shared],
                }
            }
            Scope::Project(root) => {
                let p = |rel: &str| join(root, rel);
                match agent {
                    SkillAgent::Claude => vec![p(".claude/skills")],
                    SkillAgent::Codex => vec![shared],
                    SkillAgent::Opencode => vec![
                        p(".opencode/skills"),
                        p(".opencode/skill"),
                        p(".claude/skills"),
                        shared,
                    ],
                    SkillAgent::Amp => vec![shared, p(".claude/skills")],
                    SkillAgent::Grok => vec![
                        p(".grok/skills"),
                        shared,
                        p(".claude/skills"),
                        p(".cursor/skills"),
                    ],
                    SkillAgent::Pi => vec![p(".pi/skills"), shared],
                    SkillAgent::Hermes => Vec::new(),
                    SkillAgent::Gemini => vec![p(".gemini/skills"), shared],
                    SkillAgent::Cursor => vec![p(".cursor/skills"), shared],
                    SkillAgent::Copilot => vec![p(".github/skills"), shared],
                    SkillAgent::Droid => vec![p(".factory/skills"), shared],
                }
            }
        };
        dedup(dirs)
    }

    /// The folder muxel writes into to make a skill available to `agent`: its
    /// first read folder. `None` when the agent has none at that scope.
    pub fn own_dir(&self, agent: SkillAgent, scope: &Scope) -> Option<String> {
        self.read_dirs(agent, scope).into_iter().next()
    }

    /// Whether `agent` reads skills from `dir`.
    pub fn reads(&self, agent: SkillAgent, scope: &Scope, dir: &str) -> bool {
        let d = norm(dir);
        self.read_dirs(agent, scope).contains(&d)
    }

    /// Folders holding skills an agent ships with. They're listed so the page
    /// shows everything an agent can use, but never changed.
    pub fn bundled_dirs(&self, agent: SkillAgent, scope: &Scope) -> Vec<String> {
        if *scope != Scope::User {
            return Vec::new();
        }
        match agent {
            SkillAgent::Codex => vec![join(&self.codex_home(), "skills/.system")],
            SkillAgent::Grok => vec![join(&self.grok_home(), "bundled/skills")],
            _ => Vec::new(),
        }
    }

    /// Every folder to scan for `agents`, read folders then bundled ones,
    /// without duplicates.
    pub fn scan_dirs(&self, agents: &[SkillAgent], scope: &Scope) -> Vec<String> {
        let mut dirs = Vec::new();
        for &a in agents {
            dirs.extend(self.read_dirs(a, scope));
        }
        for &a in agents {
            dirs.extend(self.bundled_dirs(a, scope));
        }
        dedup(dirs)
    }

    /// The `skills` CLI's lock file: `$XDG_STATE_HOME/skills/.skill-lock.json`,
    /// else `~/.agents/.skill-lock.json`.
    pub fn lock_path(&self) -> String {
        match self.var("XDG_STATE_HOME") {
            Some(state) => join(&state, "skills/.skill-lock.json"),
            None => self.home_join(".agents/.skill-lock.json"),
        }
    }

    /// Hermes's list of the skills it ships (`name:hash` per line), which it
    /// re-seeds and muxel therefore treats as read-only.
    pub fn hermes_manifest(&self) -> String {
        join(&self.hermes_home(), "skills/.bundled_manifest")
    }

    /// Whether `dir` is a Hermes skills folder, whose skills sit in category
    /// subfolders.
    pub fn is_hermes_dir(&self, dir: &str) -> bool {
        norm(dir) == join(&self.hermes_home(), "skills")
    }

    /// Why a skill folder must not be changed, if it must not: it's synced from
    /// claude.ai, or ships with an agent. `hermes_bundled` is the parsed
    /// [`Self::hermes_manifest`].
    pub fn protection(&self, skill_path: &str, hermes_bundled: &[String]) -> Option<Protection> {
        let claude_synced = join(&self.claude_home(), "skills/synced");
        if is_within(skill_path, &claude_synced) {
            return Some(Protection::Synced);
        }
        for agent in [SkillAgent::Codex, SkillAgent::Grok] {
            for dir in self.bundled_dirs(agent, &Scope::User) {
                if is_within(skill_path, &dir) {
                    return Some(Protection::Bundled(agent));
                }
            }
        }
        let hermes = join(&self.hermes_home(), "skills");
        if is_within(skill_path, &hermes) {
            let name = super::file_name(skill_path);
            if hermes_bundled.iter().any(|b| b == name) {
                return Some(Protection::Bundled(SkillAgent::Hermes));
            }
        }
        None
    }
}

/// Why a skill is read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    /// Synced from the user's claude.ai account into `~/.claude/skills/synced`.
    Synced,
    /// Ships with this agent.
    Bundled(SkillAgent),
}

/// The skill names in Hermes's `.bundled_manifest` (`name:hash` lines).
pub fn parse_hermes_manifest(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split(':').next())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

fn dedup(dirs: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(dirs.len());
    for d in dirs {
        let d = norm(&d);
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Protection, Scope, SkillAgent, SkillHome, parse_hermes_manifest};
    use crate::RemoteOs;

    fn home() -> SkillHome {
        SkillHome::new("/h", RemoteOs::Unix)
    }

    #[test]
    fn claude_and_hermes_read_only_their_own_folders() {
        let h = home();
        assert_eq!(
            h.read_dirs(SkillAgent::Claude, &Scope::User),
            ["/h/.claude/skills"]
        );
        assert_eq!(
            h.read_dirs(SkillAgent::Hermes, &Scope::User),
            ["/h/.hermes/skills"]
        );
        assert!(
            h.read_dirs(SkillAgent::Hermes, &Scope::Project("/p".into()))
                .is_empty()
        );
    }

    #[test]
    fn universal_agents_read_the_shared_folder() {
        let h = home();
        for a in [
            SkillAgent::Codex,
            SkillAgent::Opencode,
            SkillAgent::Amp,
            SkillAgent::Grok,
            SkillAgent::Pi,
            SkillAgent::Gemini,
            SkillAgent::Cursor,
            SkillAgent::Copilot,
            SkillAgent::Droid,
        ] {
            assert!(h.reads(a, &Scope::User, "/h/.agents/skills"), "{a:?}");
            assert!(
                h.reads(a, &Scope::Project("/p".into()), "/p/.agents/skills/"),
                "{a:?}"
            );
        }
        assert!(!h.reads(SkillAgent::Claude, &Scope::User, "/h/.agents/skills"));
    }

    #[test]
    fn env_overrides_move_folders() {
        let h = home()
            .with_env("CLAUDE_CONFIG_DIR", "/c")
            .with_env("XDG_CONFIG_HOME", "/x/")
            .with_env("GROK_HOME", "")
            .with_env("XDG_STATE_HOME", "/s");
        assert_eq!(
            h.own_dir(SkillAgent::Claude, &Scope::User).unwrap(),
            "/c/skills"
        );
        assert_eq!(
            h.own_dir(SkillAgent::Opencode, &Scope::User).unwrap(),
            "/x/opencode/skills"
        );
        // An empty variable counts as unset.
        assert_eq!(h.agent_home(SkillAgent::Grok), "/h/.grok");
        // Other agents still read the literal ~/.claude/skills.
        assert!(h.reads(SkillAgent::Amp, &Scope::User, "/h/.claude/skills"));
        assert_eq!(h.lock_path(), "/s/skills/.skill-lock.json");
        assert_eq!(home().lock_path(), "/h/.agents/.skill-lock.json");
    }

    #[test]
    fn own_dirs_for_projects() {
        let h = home();
        let p = Scope::Project("/p".into());
        assert_eq!(
            h.own_dir(SkillAgent::Claude, &p).unwrap(),
            "/p/.claude/skills"
        );
        assert_eq!(
            h.own_dir(SkillAgent::Codex, &p).unwrap(),
            "/p/.agents/skills"
        );
        assert_eq!(
            h.own_dir(SkillAgent::Copilot, &p).unwrap(),
            "/p/.github/skills"
        );
        assert_eq!(h.own_dir(SkillAgent::Hermes, &p), None);
    }

    #[test]
    fn scan_dirs_cover_bundled_once() {
        let dirs = home().scan_dirs(&SkillAgent::ALL, &Scope::User);
        let shared = dirs.iter().filter(|d| *d == "/h/.agents/skills").count();
        assert_eq!(shared, 1);
        assert!(dirs.contains(&"/h/.grok/bundled/skills".to_string()));
        assert!(dirs.contains(&"/h/.codex/skills/.system".to_string()));
    }

    #[test]
    fn protection_covers_synced_and_bundled() {
        let h = home();
        let bundled = parse_hermes_manifest("arxiv:abc\nairtable:def\n\n");
        assert_eq!(bundled, ["arxiv", "airtable"]);
        assert_eq!(
            h.protection("/h/.claude/skills/synced/b/docx", &bundled),
            Some(Protection::Synced)
        );
        assert_eq!(
            h.protection("/h/.grok/bundled/skills/x", &bundled),
            Some(Protection::Bundled(SkillAgent::Grok))
        );
        assert_eq!(
            h.protection("/h/.hermes/skills/research/arxiv", &bundled),
            Some(Protection::Bundled(SkillAgent::Hermes))
        );
        assert_eq!(h.protection("/h/.hermes/skills/mine/tool", &bundled), None);
        assert_eq!(h.protection("/h/.claude/skills/mine", &bundled), None);
    }

    #[test]
    fn programs_map_back_to_agents() {
        assert_eq!(
            SkillAgent::from_program("/usr/bin/claude"),
            Some(SkillAgent::Claude)
        );
        assert_eq!(SkillAgent::from_program("amp.exe"), Some(SkillAgent::Amp));
        assert_eq!(SkillAgent::from_program("ollama"), None);
    }
}
