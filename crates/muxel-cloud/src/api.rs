//! The muxel Cloud HTTP API (docs/cloud-sync-protocol.md §§2–6).
//!
//! Blocking by design: callers run it on the background executor, as the
//! update check does. [`Transport`] abstracts the wire so the engine can be
//! tested against an in-memory server.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::crypto::{AccountBundle, WireGrant};

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum ApiError {
    #[error("can't reach muxel Cloud: {0}")]
    Network(String),
    #[error("signed out (the device token was revoked or expired)")]
    Unauthorized,
    #[error("this needs an active muxel Cloud plan")]
    PaymentRequired,
    #[error("your team role doesn't allow that")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("changed elsewhere; retrying")]
    Conflict(Box<Value>),
    #[error("that item is too large to sync")]
    TooLarge,
    #[error("sync is read-only until the payment issue is fixed")]
    ReadOnly,
    #[error("rate limited")]
    RateLimited(Option<u64>),
    #[error("device login: {0}")]
    DeviceFlow(String),
    #[error("server error {0}: {1}")]
    Server(u16, String),
    #[error("unexpected response: {0}")]
    Decode(String),
    /// The caller cancelled (sync was paused): nothing was sent.
    #[error("sync was paused")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, ApiError>;

/// One HTTP exchange. Returns the status and the parsed JSON body (or `Null`).
pub trait Transport: Send + Sync {
    fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<(u16, Value)>;
}

/// The real transport, over `ureq`.
pub struct Http {
    base: String,
    agent: ureq::Agent,
    cancel: Option<Arc<AtomicBool>>,
}

impl Http {
    /// `base` is the server root (e.g. `https://cloud.muxel.sh`).
    pub fn new(base: &str) -> Self {
        Self {
            base: format!("{}/api/v1", base.trim_end_matches('/')),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("muxel/", env!("CARGO_PKG_VERSION")))
                .build(),
            cancel: None,
        }
    }

    /// Refuse every request once `flag` is set, before anything is sent. A
    /// sync cycle carries one so pausing sync stops it between requests.
    pub fn with_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }
}

impl Transport for Http {
    fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::SeqCst))
        {
            return Err(ApiError::Cancelled);
        }
        let mut req = self
            .agent
            .request(method, &format!("{}{}", self.base, path));
        if let Some(t) = token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let resp = match body {
            Some(b) => req.send_json(b.clone()),
            None => req.call(),
        };
        let (status, resp) = match resp {
            Ok(r) => (r.status(), r),
            Err(ureq::Error::Status(code, r)) => {
                if code == 429 {
                    let after = r.header("Retry-After").and_then(|v| v.parse().ok());
                    return Err(ApiError::RateLimited(after));
                }
                (code, r)
            }
            Err(e) => return Err(ApiError::Network(e.to_string())),
        };
        let text = resp
            .into_string()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Ok((status, value))
    }
}

fn err_code(v: &Value) -> &str {
    v.get("error").and_then(Value::as_str).unwrap_or("")
}

/// Map a non-2xx response to an error.
fn check(status: u16, v: Value) -> Result<Value> {
    match status {
        200..=299 => Ok(v),
        401 => Err(ApiError::Unauthorized),
        402 => Err(ApiError::PaymentRequired),
        403 => Err(ApiError::Forbidden),
        404 => Err(ApiError::NotFound),
        409 => Err(ApiError::Conflict(Box::new(
            v.get("current").cloned().unwrap_or(Value::Null),
        ))),
        413 => Err(ApiError::TooLarge),
        423 => Err(ApiError::ReadOnly),
        429 => Err(ApiError::RateLimited(None)),
        _ => {
            let detail = v
                .get("detail")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| err_code(&v).to_string());
            Err(ApiError::Server(status, detail))
        }
    }
}

fn decode<T: DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| ApiError::Decode(e.to_string()))
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
pub struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    #[serde(default = "five")]
    pub interval: u64,
    #[serde(default)]
    pub expires_in: u64,
}

fn five() -> u64 {
    5
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevicePoll {
    Pending,
    SlowDown,
    Denied,
    Expired,
    Approved { token: String, device_id: Uuid },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeUser {
    pub id: Uuid,
    pub email: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeDevice {
    pub id: Uuid,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Entitlements {
    #[serde(default)]
    pub personal_sync: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub interval: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TeamRef {
    pub id: Uuid,
    pub name: String,
    #[serde(default, alias = "my_role")]
    pub role: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Me {
    pub user: MeUser,
    pub device: MeDevice,
    #[serde(default)]
    pub entitlements: Entitlements,
    #[serde(default)]
    pub teams: Vec<TeamRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Personal,
    TeamSettings,
    TeamTasks,
    TeamActivity,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Scope {
    pub id: Uuid,
    pub kind: ScopeKind,
    #[serde(default)]
    pub team_id: Option<Uuid>,
    #[serde(default)]
    pub generation: u32,
    #[serde(default)]
    pub rotation_required: bool,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub collections: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct KeyGrant {
    pub scope_id: Uuid,
    pub generation: u32,
    pub wrapped_key: String,
    pub granter_user_id: Uuid,
    pub signature: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct NewGrant {
    pub user_id: Uuid,
    pub generation: u32,
    pub wrapped_key: String,
    pub signature: String,
}

impl NewGrant {
    pub fn new(user_id: Uuid, generation: u32, g: WireGrant) -> Self {
        Self {
            user_id,
            generation,
            wrapped_key: g.wrapped_key,
            signature: g.signature,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct PendingGrant {
    pub scope_id: Uuid,
    pub generation: u32,
    pub user_id: Uuid,
    pub enc_pub: String,
    pub sign_pub: String,
    #[serde(default)]
    pub fingerprint: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RemoteItem {
    pub scope_id: Uuid,
    pub collection: String,
    pub item_id: Uuid,
    pub version: u64,
    #[serde(default)]
    pub seq: u64,
    pub author_user_id: Uuid,
    #[serde(default)]
    pub author_device_id: Option<Uuid>,
    #[serde(default)]
    pub assignee_user_id: Option<Uuid>,
    #[serde(default)]
    pub deleted: bool,
    /// base64 of the envelope JSON.
    #[serde(default)]
    pub envelope: String,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Changes {
    pub items: Vec<RemoteItem>,
    pub cursor: u64,
    #[serde(default)]
    pub more: bool,
}

/// The body of an item write (§4.2); `envelope` is base64.
pub struct ItemWrite<'a> {
    pub base_version: u64,
    pub envelope: &'a str,
    pub deleted: bool,
    pub assignee: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct PutResult {
    pub version: u64,
    #[serde(default)]
    pub seq: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Member {
    pub user_id: Uuid,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub enc_pub: Option<String>,
    #[serde(default)]
    pub sign_pub: Option<String>,
    #[serde(default)]
    pub fingerprint: Option<String>,
}

impl Member {
    pub fn display(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.email
        } else {
            &self.name
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Team {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub my_role: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub seats: u32,
    #[serde(default)]
    pub seats_used: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ActivityRow {
    pub user_id: Uuid,
    pub device_id: Uuid,
    pub envelope: String,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Approval {
    pub approval_id: Uuid,
    #[serde(default)]
    pub device_id: Option<Uuid>,
    #[serde(default)]
    pub device_name: String,
    pub device_pub: String,
    #[serde(default)]
    pub created_at: u64,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Client<T: Transport> {
    t: T,
    token: Option<String>,
}

impl<T: Transport> Client<T> {
    pub fn new(t: T, token: Option<String>) -> Self {
        Self { t, token }
    }

    pub fn set_token(&mut self, token: Option<String>) {
        self.token = token;
    }

    fn req(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let (status, v) = self
            .t
            .call(method, path, self.token.as_deref(), body.as_ref())?;
        check(status, v)
    }

    // --- device login (§2) ---

    pub fn device_start(&self, device_name: &str, platform: &str) -> Result<DeviceStart> {
        let (s, v) = self.t.call(
            "POST",
            "/device/start",
            None,
            Some(&json!({"device_name": device_name, "platform": platform})),
        )?;
        decode(check(s, v)?)
    }

    pub fn device_poll(&self, device_code: &str) -> Result<DevicePoll> {
        let (s, v) = self.t.call(
            "POST",
            "/device/token",
            None,
            Some(&json!({"device_code": device_code})),
        )?;
        if (200..300).contains(&s) {
            let token = v
                .get("access_token")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::Decode("no access_token".into()))?;
            let device_id = v
                .get("device_id")
                .and_then(Value::as_str)
                .and_then(|d| Uuid::parse_str(d).ok())
                .ok_or_else(|| ApiError::Decode("no device_id".into()))?;
            return Ok(DevicePoll::Approved {
                token: token.to_string(),
                device_id,
            });
        }
        match err_code(&v) {
            "authorization_pending" => Ok(DevicePoll::Pending),
            "slow_down" => Ok(DevicePoll::SlowDown),
            "access_denied" => Ok(DevicePoll::Denied),
            "expired_token" => Ok(DevicePoll::Expired),
            other => Err(ApiError::DeviceFlow(other.to_string())),
        }
    }

    // --- account (§3.1, §6) ---

    pub fn me(&self) -> Result<Me> {
        decode(self.req("GET", "/me", None)?)
    }

    pub fn account_keys(&self) -> Result<Option<AccountBundle>> {
        match self.req("GET", "/account/keys", None) {
            Ok(v) => Ok(Some(decode(v)?)),
            Err(ApiError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn put_account_keys(&self, b: &AccountBundle) -> Result<()> {
        self.req("PUT", "/account/keys", Some(json!(b))).map(|_| ())
    }

    pub fn rotate_recovery(&self, auk_recovery: &str, recovery_salt: &str) -> Result<()> {
        self.req(
            "POST",
            "/account/keys/rotate-recovery",
            Some(json!({"auk_recovery": auk_recovery, "recovery_salt": recovery_salt})),
        )
        .map(|_| ())
    }

    pub fn request_approval(&self, device_pub_b64: &str) -> Result<Uuid> {
        let v = self.req(
            "POST",
            "/account/device-approvals",
            Some(json!({"device_pub": device_pub_b64})),
        )?;
        v.get("approval_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| ApiError::Decode("no approval_id".into()))
    }

    pub fn pending_approvals(&self) -> Result<Vec<Approval>> {
        decode(self.req("GET", "/account/device-approvals?pending=1", None)?)
    }

    pub fn approve(&self, approval: Uuid, sealed_auk: &str) -> Result<()> {
        self.req(
            "POST",
            &format!("/account/device-approvals/{approval}"),
            Some(json!({"sealed_auk": sealed_auk})),
        )
        .map(|_| ())
    }

    /// `Ok(None)` while still pending.
    pub fn approval_result(&self, approval: Uuid) -> Result<Option<String>> {
        let (s, v) = self.t.call(
            "GET",
            &format!("/account/device-approvals/{approval}"),
            self.token.as_deref(),
            None,
        )?;
        if s == 202 {
            return Ok(None);
        }
        let v = check(s, v)?;
        Ok(v.get("sealed_auk")
            .and_then(Value::as_str)
            .map(String::from))
    }

    // --- scopes & keys (§3.2) ---

    pub fn scopes(&self) -> Result<Vec<Scope>> {
        decode(self.req("GET", "/scopes", None)?)
    }

    pub fn keys(&self) -> Result<Vec<KeyGrant>> {
        decode(self.req("GET", "/keys", None)?)
    }

    /// Create a scope atomically with its grants (§3.2). 409 = it exists.
    pub fn create_scope(
        &self,
        id: Uuid,
        kind: ScopeKind,
        team_id: Option<Uuid>,
        grants: &[NewGrant],
    ) -> Result<()> {
        self.req(
            "POST",
            "/scopes",
            Some(json!({"id": id, "kind": kind, "team_id": team_id, "grants": grants})),
        )
        .map(|_| ())
    }

    pub fn pending_grants(&self) -> Result<Vec<PendingGrant>> {
        decode(self.req("GET", "/grants/pending", None)?)
    }

    pub fn post_grant(&self, scope: Uuid, g: &NewGrant) -> Result<()> {
        self.req(
            "POST",
            "/grants",
            Some(
                json!({"scope_id": scope, "generation": g.generation, "user_id": g.user_id,
                         "wrapped_key": g.wrapped_key, "signature": g.signature}),
            ),
        )
        .map(|_| ())
    }

    pub fn rotate(&self, scope: Uuid, generation: u32, grants: &[NewGrant]) -> Result<()> {
        self.req(
            "POST",
            &format!("/scopes/{scope}/rotate"),
            Some(json!({"generation": generation, "grants": grants})),
        )
        .map(|_| ())
    }

    // --- items (§4) ---

    pub fn changes(&self, cursor: u64, limit: u32) -> Result<Changes> {
        decode(self.req(
            "GET",
            &format!("/changes?cursor={cursor}&limit={limit}"),
            None,
        )?)
    }

    /// [`Self::changes`] for an export: also returns scopes whose plan has
    /// lapsed (read-only access to your own data). A server without export
    /// support ignores the flag.
    pub fn export_changes(&self, cursor: u64, limit: u32) -> Result<Changes> {
        decode(self.req(
            "GET",
            &format!("/changes?cursor={cursor}&limit={limit}&export=1"),
            None,
        )?)
    }

    pub fn put_item(
        &self,
        scope: Uuid,
        collection: &str,
        item: Uuid,
        w: &ItemWrite,
    ) -> Result<PutResult> {
        decode(self.req(
            "PUT",
            &format!("/scopes/{scope}/items/{collection}/{item}"),
            Some(
                json!({"base_version": w.base_version, "envelope": w.envelope,
                         "deleted": w.deleted, "assignee_user_id": w.assignee}),
            ),
        )?)
    }

    // --- teams (§5) ---

    pub fn teams(&self) -> Result<Vec<Team>> {
        decode(self.req("GET", "/teams", None)?)
    }

    pub fn members(&self, team: Uuid) -> Result<Vec<Member>> {
        decode(self.req("GET", &format!("/teams/{team}/members"), None)?)
    }

    pub fn put_activity(&self, team: Uuid, envelope_b64: &str) -> Result<()> {
        self.req(
            "PUT",
            &format!("/teams/{team}/activity"),
            Some(json!({"envelope": envelope_b64})),
        )
        .map(|_| ())
    }

    pub fn activity(&self, team: Uuid) -> Result<Vec<ActivityRow>> {
        decode(self.req("GET", &format!("/teams/{team}/activity"), None)?)
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiError, Http, Transport};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn a_cancelled_transport_sends_nothing() {
        // Nothing listens on port 1: a request that went out would fail with
        // a network error, not `Cancelled`.
        let flag = Arc::new(AtomicBool::new(false));
        let http = Http::new("http://127.0.0.1:1").with_cancel(flag.clone());
        assert!(matches!(
            http.call("GET", "/me", None, None),
            Err(ApiError::Network(_))
        ));
        flag.store(true, Ordering::SeqCst);
        assert_eq!(
            http.call("GET", "/me", None, None),
            Err(ApiError::Cancelled)
        );
    }
}
