//! Delivery facts and read-only next commands, separate from writer admission.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptOutcome {
    Submitted,
    PendingInComposer,
    NotDelivered,
    Unconfirmed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupBlockerKind {
    FolderTrust,
    CodexUpdate,
    Login,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupBlocker {
    pub kind: StartupBlockerKind,
    pub excerpt: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptDelivery {
    pub outcome: Option<PromptOutcome>,
    pub next: Option<String>,
    pub submission_confirmed: Option<bool>,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub prompt_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<StartupBlockerKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_excerpt: Option<String>,
}

impl PromptDelivery {
    pub fn new(agent: &str, outcome: PromptOutcome) -> Self {
        Self {
            outcome: Some(outcome),
            next: match outcome {
                PromptOutcome::Submitted => None,
                PromptOutcome::PendingInComposer
                | PromptOutcome::NotDelivered
                | PromptOutcome::Unconfirmed => Some(format!("muxel ctl screen {agent}")),
            },
            submission_confirmed: Some(outcome == PromptOutcome::Submitted),
            ..Self::default()
        }
    }

    pub fn pending(agent: &str) -> Self {
        Self {
            next: Some(format!("muxel ctl status {agent}")),
            prompt_pending: true,
            ..Self::default()
        }
    }

    pub fn apply(self, reply: &mut Value) {
        reply["outcome"] = json!(self.outcome);
        reply["next"] = json!(self.next);
        reply["submission_confirmed"] = json!(self.submission_confirmed);
        if self.prompt_pending {
            reply["prompt_pending"] = json!(true);
        } else if let Some(fields) = reply.as_object_mut() {
            fields.remove("prompt_pending");
        }
        if let Some(kind) = self.blocked_by {
            reply["blocked_by"] = json!(kind);
        }
        if let Some(excerpt) = self.screen_excerpt {
            reply["screen_excerpt"] = json!(excerpt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PromptDelivery, PromptOutcome};

    #[test]
    fn pending_advice_is_read_only() {
        for (outcome, next) in [
            (PromptOutcome::Submitted, None),
            (
                PromptOutcome::PendingInComposer,
                Some("muxel ctl screen abcd"),
            ),
            (PromptOutcome::NotDelivered, Some("muxel ctl screen abcd")),
            (PromptOutcome::Unconfirmed, Some("muxel ctl screen abcd")),
        ] {
            let receipt = PromptDelivery::new("abcd", outcome);
            assert_eq!(receipt.next.as_deref(), next);
            assert_eq!(
                receipt.submission_confirmed,
                Some(outcome == PromptOutcome::Submitted)
            );
        }
        let pending = PromptDelivery::pending("abcd");
        assert_eq!(pending.outcome, None);
        assert!(pending.prompt_pending);
        assert_eq!(pending.next.as_deref(), Some("muxel ctl status abcd"));
    }
}
