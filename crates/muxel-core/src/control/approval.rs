//! Creation admission and click approvals. The host supplies elapsed milliseconds;
//! no wall clock, socket, persistence or UI belongs here.

use super::{Error, ErrorCode, Response};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CreationPolicy {
    #[default]
    Off,
    Ask,
    Allow,
}

impl<'de> Deserialize<'de> for CreationPolicy {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Named {
            Off,
            Ask,
            Allow,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Saved {
            Legacy(bool),
            Named(Named),
        }
        // No release stored a bool, but a development build did: keep such a
        // settings file loadable, and never let it grant Allow.
        Ok(match Saved::deserialize(de)? {
            Saved::Legacy(false) | Saved::Named(Named::Off) => Self::Off,
            Saved::Legacy(true) | Saved::Named(Named::Ask) => Self::Ask,
            Saved::Named(Named::Allow) => Self::Allow,
        })
    }
}

/// What an outside creation would run, for its admission policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchClass {
    /// A preset running a known agent provider (`is_known_agent_program`):
    /// the only class Allow opens without a click.
    Agent,
    /// A preset running any other non-shell program (`ssh`, `wsl.exe`,
    /// `python`, …). It may give shell access, so it always Asks.
    Program,
    /// A shell preset or the default shell: also needs the command grant.
    Shell,
    /// Literal argv: also needs the command grant.
    Literal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationDecision {
    Refuse(ErrorCode),
    Ask,
    Allow,
}

pub fn creation_decision(
    enabled: bool,
    policy: CreationPolicy,
    commands: bool,
    class: LaunchClass,
) -> CreationDecision {
    if !enabled {
        return CreationDecision::Refuse(ErrorCode::ControlDisabled);
    }
    if policy == CreationPolicy::Off {
        return CreationDecision::Refuse(ErrorCode::CreationDisabled);
    }
    if matches!(class, LaunchClass::Shell | LaunchClass::Literal) && !commands {
        return CreationDecision::Refuse(ErrorCode::CommandPermissionRequired);
    }
    if policy == CreationPolicy::Allow && class == LaunchClass::Agent {
        CreationDecision::Allow
    } else {
        CreationDecision::Ask
    }
}

pub fn creation_refusal(code: ErrorCode) -> Error {
    Error::new(
        code,
        match code {
            ErrorCode::ControlDisabled => "outside control is disabled in Settings > Grok Bot",
            ErrorCode::CreationDisabled => {
                "outside terminal creation is Off; choose Ask or Allow in Settings > Grok Bot > Outside terminal creation"
            }
            ErrorCode::CommandPermissionRequired => {
                "opening shell panes or running literal commands requires Settings > Grok Bot > \"Also allow typing into shell panes\"; commands run as the host muxel process, outside the caller's sandbox"
            }
            ErrorCode::ApprovalDenied => "terminal creation was denied in muxel",
            ErrorCode::ApprovalTimeout => "terminal creation approval expired after 120 seconds",
            ErrorCode::PromptUnsupported => {
                "--prompt is supported only for configured non-shell terminal presets"
            }
            _ => "terminal creation was refused",
        },
    )
}

pub const APPROVAL_TIMEOUT_MS: u64 = 120_000;
pub const APPROVAL_ARM_DELAY_MS: u64 = 750;
const RESULT_RETENTION_MS: u64 = 600_000;
const MAX_PENDING: usize = 64;
const MAX_FINISHED: usize = 256;

pub struct PendingApproval<T> {
    pub id: Uuid,
    pub admitted_at: u64,
    pub payload: T,
}

/// Runtime-only. Polling is repeatable, including after a lost final reply.
/// Finished replies live for ten minutes, bounded to the latest 256.
pub struct ApprovalQueue<T> {
    pending: VecDeque<PendingApproval<T>>,
    replies: HashMap<Uuid, Option<Response>>,
    finished: VecDeque<(Uuid, u64)>,
    visible: Option<(Uuid, u64)>,
    pressed: Option<Uuid>,
}

impl<T> Default for ApprovalQueue<T> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            replies: HashMap::new(),
            finished: VecDeque::new(),
            visible: None,
            pressed: None,
        }
    }
}

impl<T> ApprovalQueue<T> {
    pub fn admit(&mut self, payload: T, now: u64) -> Result<Uuid, Error> {
        self.expire(now);
        if self.pending.len() >= MAX_PENDING {
            return Err(
                "too many terminal creation requests are waiting; retry after one resolves".into(),
            );
        }
        let id = Uuid::new_v4();
        self.pending.push_back(PendingApproval {
            id,
            admitted_at: now,
            payload,
        });
        self.replies.insert(id, None);
        Ok(id)
    }

    pub fn front(&self) -> Option<&PendingApproval<T>> {
        self.pending.front()
    }
    pub fn len(&self) -> usize {
        self.pending.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Start the decision delay when the host shows a new head. A waiting
    /// request's admission time must not arm its buttons before it is shown.
    pub fn mark_visible(&mut self, now: u64) -> bool {
        let head = self.front().map(|entry| entry.id);
        if self.visible.map(|(id, _)| id) != head {
            self.visible = head.map(|id| (id, now));
            self.pressed = None;
            return head.is_some();
        }
        false
    }

    pub fn is_armed(&self, id: Uuid, now: u64) -> bool {
        self.front().is_some_and(|entry| entry.id == id)
            && self.visible.is_some_and(|(visible, since)| {
                visible == id && now.saturating_sub(since) >= APPROVAL_ARM_DELAY_MS
            })
    }

    /// A click must begin on the same armed card it eventually decides. A
    /// press held while the head expires cannot transfer to its replacement.
    pub fn press(&mut self, id: Uuid, now: u64) -> bool {
        self.expire(now);
        self.pressed = self.is_armed(id, now).then_some(id);
        self.pressed.is_some()
    }

    pub fn status(&self, id: Uuid) -> Result<Value, Error> {
        match self.replies.get(&id) {
            Some(Some(response)) => response.clone().into_result(),
            Some(None) => Ok(json!({"pending":true, "request_id":id})),
            None => Err("unknown or expired creation request id".into()),
        }
    }

    /// Only a press that began on this armed visible head can decide it. The
    /// token is consumed even when a stale release is refused.
    pub fn take(&mut self, id: Uuid, now: u64) -> Option<T> {
        self.expire(now);
        if self.pressed.take() != Some(id) || !self.is_armed(id, now) {
            return None;
        }
        self.visible = None;
        self.pending.pop_front().map(|entry| entry.payload)
    }

    pub fn finish(&mut self, id: Uuid, response: Response, now: u64) {
        if let Some(reply @ None) = self.replies.get_mut(&id) {
            *reply = Some(response);
            self.finished.push_back((id, now));
        }
        self.prune(now);
    }

    pub fn deny(&mut self, id: Uuid, now: u64) {
        if self.take(id, now).is_some() {
            self.finish(
                id,
                Response::err(creation_refusal(ErrorCode::ApprovalDenied)),
                now,
            );
        }
    }

    pub fn expire(&mut self, now: u64) {
        while self
            .front()
            .is_some_and(|entry| now.saturating_sub(entry.admitted_at) >= APPROVAL_TIMEOUT_MS)
        {
            let entry = self.pending.pop_front().expect("front checked");
            self.visible = None;
            self.pressed = None;
            self.finish(
                entry.id,
                Response::err(creation_refusal(ErrorCode::ApprovalTimeout)),
                now,
            );
        }
        self.prune(now);
    }

    pub fn refuse_all(&mut self, code: ErrorCode, now: u64) {
        self.visible = None;
        self.pressed = None;
        while let Some(entry) = self.pending.pop_front() {
            self.finish(entry.id, Response::err(creation_refusal(code)), now);
        }
    }

    fn prune(&mut self, now: u64) {
        while self
            .finished
            .front()
            .is_some_and(|(_, at)| now.saturating_sub(*at) >= RESULT_RETENTION_MS)
            || self.finished.len() > MAX_FINISHED
        {
            if let Some((id, _)) = self.finished.pop_front() {
                self.replies.remove(&id);
            }
        }
    }
}
