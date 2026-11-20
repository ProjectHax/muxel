//! Team presence: what each member's agents are doing, published periodically
//! (encrypted) by every device in a team-bound workspace. Everything beyond an
//! agent's status is filtered by the user's [`ActivityPrivacy`].

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ActivityPrivacy;

/// How old a presence may be before the UI greys it out (the server expires
/// entries on its own TTL too).
pub const STALE_AFTER_MS: u64 = 5 * 60_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivity {
    /// Program name ("claude", "codex", …); empty when status sharing is off.
    #[serde(default)]
    pub program: String,
    /// "working" | "blocked" | "idle" | "done" | "exited".
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub task_id: Option<Uuid>,
    #[serde(default)]
    pub task_title: Option<String>,
    /// One-line summary of the last prompt (opt-in).
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectActivity {
    pub project_key: String,
    pub project_name: String,
    #[serde(default)]
    pub agents: Vec<AgentActivity>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub at_ms: u64,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub projects: Vec<ProjectActivity>,
}

/// One line, at most 120 characters, control characters removed.
pub fn summarize(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let clean: String = line.chars().filter(|c| !c.is_control()).collect();
    if clean.chars().count() > 120 {
        let cut: String = clean.chars().take(119).collect();
        format!("{cut}…")
    } else {
        clean
    }
}

/// Apply the privacy switches to a raw snapshot before it is encrypted.
pub fn filter(mut p: Presence, privacy: &ActivityPrivacy) -> Presence {
    for proj in &mut p.projects {
        if !privacy.agent_status {
            proj.agents.clear();
            continue;
        }
        for a in &mut proj.agents {
            if !privacy.branch {
                a.branch = None;
            }
            if !privacy.task_titles {
                a.task_title = None;
            }
            a.prompt = if privacy.prompts {
                a.prompt.as_deref().map(summarize)
            } else {
                None
            };
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::{AgentActivity, Presence, ProjectActivity, filter, summarize};
    use crate::cloud::ActivityPrivacy;

    fn sample() -> Presence {
        Presence {
            at_ms: 1,
            device_name: "laptop".into(),
            projects: vec![ProjectActivity {
                project_key: "git:x/y".into(),
                project_name: "y".into(),
                agents: vec![AgentActivity {
                    program: "claude".into(),
                    status: "working".into(),
                    branch: Some("feat".into()),
                    task_id: None,
                    task_title: Some("Fix it".into()),
                    prompt: Some("\n  refactor the parser\nsecond line".into()),
                }],
            }],
        }
    }

    #[test]
    fn prompts_are_off_by_default() {
        let p = filter(sample(), &ActivityPrivacy::default());
        let a = &p.projects[0].agents[0];
        assert_eq!(a.prompt, None);
        assert_eq!(a.branch.as_deref(), Some("feat"));
    }

    #[test]
    fn opting_in_summarizes_and_status_off_hides_agents() {
        let privacy = ActivityPrivacy {
            prompts: true,
            ..Default::default()
        };
        let p = filter(sample(), &privacy);
        assert_eq!(
            p.projects[0].agents[0].prompt.as_deref(),
            Some("refactor the parser")
        );
        let privacy = ActivityPrivacy {
            agent_status: false,
            ..Default::default()
        };
        assert!(filter(sample(), &privacy).projects[0].agents.is_empty());
    }

    #[test]
    fn summarize_caps_length() {
        let long = "x".repeat(500);
        assert_eq!(summarize(&long).chars().count(), 120);
        assert_eq!(summarize("a\u{7}b"), "ab");
    }
}
