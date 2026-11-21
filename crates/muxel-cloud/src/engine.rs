//! One sync cycle: keys → pull → apply → push → activity.
//!
//! Runs in the single process that owns the profile's sync lock, on the
//! background executor. Pure inputs in, pure outputs out; the caller persists
//! [`EngineState`] and applies [`Output`] to settings and the task store.

use std::collections::{BTreeMap, HashMap, HashSet};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use muxel_core::Settings;
use muxel_core::cloud::CloudSettings;
use muxel_core::cloud::activity::Presence;
use muxel_core::cloud::hlc::Hlc;
use muxel_core::cloud::items::{self, Carry, Collection, LocalItem, Resolver};
use muxel_core::cloud::roles::{Grants, Permission};
use muxel_core::cloud::state::{
    ItemState, RemoteDecision, ScopeState, SyncState, content_hash, decide_remote, plan_push,
};
use muxel_core::cloud::tasks::{self, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::api::{ApiError, Client, Me, Member, NewGrant, ScopeKind, Transport};
use crate::crypto::{
    self, AccountKeys, Author, Envelope, ItemAddr, ItemKey, Key, WireGrant, pub32,
};

/// The signed-in, unlocked identity.
pub struct Session {
    pub user: Uuid,
    pub device: Uuid,
    pub keys: AccountKeys,
}

/// Persisted between cycles (`data/cloud/state.json`). Holds no secrets: team
/// items cached here are the same plaintext settings muxel keeps in config.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EngineState {
    #[serde(default)]
    pub sync: SyncState,
    /// team id → item key (`"<collection>:<id>"`) → decrypted item data.
    #[serde(default)]
    pub team_items: HashMap<Uuid, BTreeMap<String, Value>>,
    /// Scope key generations this device holds. A newly granted one means
    /// earlier items were skipped for lack of it, so the next pull starts over.
    #[serde(default)]
    pub held_keys: Vec<(Uuid, u32)>,
    /// Teams this device's last cycle published activity to. One that drops
    /// out (sharing turned off, workspace made private) gets an empty
    /// presence, so teammates stop seeing the old one.
    #[serde(default)]
    pub published_presence: Vec<Uuid>,
    /// Re-seals the server refused, as `(scope, "<collection>:<id>", version)`.
    /// Not retried until the item moves past that version.
    #[serde(default)]
    pub reseal_refused: Vec<(Uuid, String, u64)>,
    /// Whose account keys this device accepts, per team (see [`TeamTrust`]).
    #[serde(default)]
    pub trust: HashMap<Uuid, TeamTrust>,
    /// This cycle's `Input::team_setting_writes` (by index) that the server
    /// answered, stored or refused. The rest were never sent or may not have
    /// arrived. Runtime only: a cycle that stops early (paused, failed) still
    /// lets the caller drop the shares that went out, so a stale copy isn't
    /// sent again over a teammate's newer edit.
    #[serde(skip)]
    pub shares_settled: Vec<usize>,
}

/// A member's published account keys (base64, as the server lists them).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberKeys {
    pub enc_pub: String,
    pub sign_pub: String,
}

impl MemberKeys {
    fn of(m: &Member) -> Option<Self> {
        Some(Self {
            enc_pub: m.enc_pub.clone()?,
            sign_pub: m.sign_pub.clone()?,
        })
    }

    /// What people compare out loud (§3.1), or `None` for malformed keys.
    pub fn fingerprint(&self) -> Option<String> {
        Some(crypto::fingerprint(
            &pub32(&self.enc_pub, "enc_pub").ok()?,
            &pub32(&self.sign_pub, "sign_pub").ok()?,
        ))
    }
}

/// Whose keys this device accepts in one team.
///
/// The server says who is in a team, and it can't be taken at its word: a
/// compromised one could list an extra member, or swap a member's keys for its
/// own, and so be handed the team's keys. So this device hands a scope key
/// only to members in `approved`, and takes one only from them, each pinned to
/// the keys they had when they were approved.
///
/// The first time a device sees a team it approves the members it finds there
/// (trust on first use). Anyone who appears later, or whose keys change, waits
/// for the user's decision ([`TeamView::approvals`], [`MemberDecision`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTrust {
    #[serde(default)]
    pub approved: BTreeMap<Uuid, MemberKeys>,
    /// Turned down by the user: not asked about again, unless their keys change.
    #[serde(default)]
    pub declined: BTreeMap<Uuid, MemberKeys>,
    /// Already announced as waiting, so each is notified once.
    #[serde(default)]
    pub asked: BTreeMap<Uuid, MemberKeys>,
}

impl TeamTrust {
    fn approves(&self, user: Uuid, keys: &MemberKeys) -> bool {
        self.approved.get(&user) == Some(keys)
    }
}

/// The user's answer about one member, queued by the UI. It names the keys
/// the user was shown: if the member's keys are different by the time it is
/// applied, it is dropped and the member is asked about again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberDecision {
    pub team: Uuid,
    pub user: Uuid,
    pub keys: MemberKeys,
    pub approve: bool,
}

/// A queued write of a team-shared setting (from "Share with team").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TeamSettingWrite {
    pub team: Uuid,
    pub collection: Collection,
    pub id: Uuid,
    /// `None` = stop sharing (delete from the team).
    pub data: Option<Value>,
}

pub struct Input<'a> {
    pub cloud: &'a CloudSettings,
    pub settings: &'a Settings,
    /// Every local project as `(id, portable key)`, across workspaces.
    pub projects: &'a [(Uuid, String)],
    pub personal_tasks: &'a [Task],
    pub team_tasks: &'a HashMap<Uuid, Vec<Task>>,
    pub team_setting_writes: &'a [TeamSettingWrite],
    /// Members the user approved or declined since the last cycle.
    pub member_decisions: &'a [MemberDecision],
    /// This device's presence per team-bound workspace (already privacy-filtered).
    pub presence: &'a [(Uuid, Presence)],
    /// Tasks deleted here, waiting to reach the server as tombstones.
    pub task_deletions: &'a [TaskDeletion],
    pub now_ms: u64,
}

/// A task deleted on this device. Tasks are never deleted by being absent
/// (a role change or a reassignment also hides one); only these explicit
/// deletions become tombstones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDeletion {
    /// `None` = a personal task.
    pub team: Option<Uuid>,
    pub id: Uuid,
    /// The task's last assignee, sent with the tombstone so an Assignee-role
    /// member (who only receives their own tasks) receives the deletion too.
    pub assignee: Option<Uuid>,
}

/// Everything an account can decrypt, for Settings → muxel Cloud → Export.
pub struct Export {
    pub json: Value,
    pub items: usize,
    /// Items that couldn't be read (no key on this device, or failed checks).
    pub skipped: usize,
}

/// What a team looks like to the UI after a cycle.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TeamView {
    pub id: Uuid,
    pub name: String,
    pub role: String,
    pub permissions: Vec<String>,
    pub members: Vec<MemberView>,
    /// Decrypted team-shared items by collection (`runners`, `snippets`, …).
    pub items: BTreeMap<String, Vec<(Uuid, Value)>>,
    pub activity: Vec<ActivityView>,
    /// Some scope keys haven't been granted to this user yet.
    pub waiting_for_keys: bool,
    /// Members waiting for this user's decision before this device shares the
    /// team's keys with them, or accepts keys from them.
    #[serde(default)]
    pub approvals: Vec<ApprovalView>,
}

/// A member this device doesn't trust yet (see [`TeamTrust`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalView {
    pub user_id: Uuid,
    pub name: String,
    pub email: String,
    pub role: String,
    pub fingerprint: String,
    /// The keys the fingerprint stands for; sent back with the decision.
    pub keys: MemberKeys,
    /// They were approved or declined before, with different keys. Account
    /// keys never change in normal use, so this deserves a second look.
    #[serde(default)]
    pub keys_changed: bool,
    /// Declined earlier (listed so the choice can be undone).
    #[serde(default)]
    pub declined: bool,
    /// Their role can hand out the team's keys (`members.manage`).
    #[serde(default)]
    pub manager: bool,
}

impl TeamView {
    pub fn grants(&self) -> Grants {
        Grants::from_wire(&self.role, &self.permissions)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MemberView {
    pub user_id: Uuid,
    pub name: String,
    pub email: String,
    pub role: String,
    pub fingerprint: Option<String>,
    /// Custom-role permissions (empty = the built-in role's).
    #[serde(default)]
    pub permissions: Vec<String>,
}

impl MemberView {
    pub fn grants(&self) -> Grants {
        Grants::from_wire(&self.role, &self.permissions)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityView {
    pub user_id: Uuid,
    pub device_id: Uuid,
    pub presence: Presence,
    pub updated_at: u64,
}

#[derive(Default)]
pub struct Output {
    pub me: Option<Me>,
    /// Remote settings changes to replay on the live settings (see
    /// [`apply_deltas`]). Never a whole `Settings`: the cycle ran on a
    /// snapshot, and the user may have edited other items meanwhile.
    pub settings_changes: Vec<SettingsDelta>,
    pub personal_tasks: Option<Vec<Task>>,
    pub team_tasks: HashMap<Uuid, Vec<Task>>,
    pub teams: Vec<TeamView>,
    /// Task deletions that are settled (sent, or never on the server).
    pub deletions_done: Vec<Uuid>,
    /// Short user-facing lines for the notification feed.
    pub notes: Vec<String>,
    /// Per-item problems that didn't stop the cycle.
    pub errors: Vec<String>,
}

/// One remote settings change, with the content hash the item had locally
/// before it was applied (`None` = absent). Replayed only if the live item
/// still hashes the same; otherwise the user's newer edit wins and is pushed.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsDelta {
    pub scope: Uuid,
    pub collection: Collection,
    pub id: Uuid,
    pub data: Option<Value>,
    pub before: Option<u64>,
}

/// Replay `deltas` onto the live settings. Returns whether anything changed.
pub fn apply_deltas(
    live: &mut Settings,
    deltas: &[SettingsDelta],
    projects: &[(Uuid, String)],
    state: &EngineState,
) -> bool {
    let mut changed = false;
    for d in deltas {
        let scope = state.sync.scopes.get(&d.scope).cloned().unwrap_or_default();
        let presets = live.presets.clone();
        let resolver = Resolver {
            presets: &presets,
            projects,
            carry: &scope.carry,
        };
        let now = items::extract(live, &resolver, &[d.collection])
            .into_iter()
            .find(|l| l.id == d.id)
            .map(|l| content_hash(Some(&l.data)));
        if now != d.before {
            continue;
        }
        if let Ok(a) = items::apply(
            live,
            &resolver,
            d.collection,
            d.id,
            d.data.as_ref(),
            &|id| scope.knows(id),
        ) {
            changed |= a.changed;
        }
    }
    changed
}

#[derive(Debug, thiserror::Error)]
pub enum CycleError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Crypto(#[from] crypto::CryptoError),
}

/// One item write.
struct Write {
    collection: Collection,
    id: Uuid,
    base_version: u64,
    data: Option<Value>,
    hash: u64,
    assignee: Option<Uuid>,
    now_ms: u64,
}

/// The plaintext inside every item envelope (§3.3).
#[derive(Serialize, Deserialize)]
struct Plain {
    hlc: Hlc,
    kind: String,
    data: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Role {
    Personal,
    Settings(Uuid),
    Tasks(Uuid),
    Activity(Uuid),
}

struct ScopeInfo {
    id: Uuid,
    role: Role,
    generation: u32,
}

struct Ctx {
    scopes: Vec<ScopeInfo>,
    keys: HashMap<(Uuid, u32), Key>,
    /// user → (sign_pub, enc_pub, grants per team)
    members: HashMap<Uuid, Vec<Member>>,
    /// Who this device trusts, per team. `None` trusts everyone the server
    /// lists (an export only reads).
    trust: Option<HashMap<Uuid, TeamTrust>>,
}

impl Ctx {
    fn scope(&self, role: Role) -> Option<&ScopeInfo> {
        self.scopes.iter().find(|s| s.role == role)
    }
    fn by_id(&self, id: Uuid) -> Option<&ScopeInfo> {
        self.scopes.iter().find(|s| s.id == id)
    }
    fn current_key(&self, role: Role) -> Option<(Uuid, u32, &Key)> {
        let s = self.scope(role)?;
        self.keys
            .get(&(s.id, s.generation))
            .map(|k| (s.id, s.generation, k))
    }
    fn member(&self, team: Uuid, user: Uuid) -> Option<&Member> {
        self.members.get(&team)?.iter().find(|m| m.user_id == user)
    }
    fn member_grants(&self, team: Uuid, user: Uuid) -> Grants {
        self.member(team, user)
            .map(|m| Grants::from_wire(&m.role, &m.permissions))
            .unwrap_or_default()
    }
    /// Whether this device hands team keys to `m`, and takes them from `m`.
    fn trusts(&self, team: Uuid, m: &Member, me: Uuid) -> bool {
        if m.user_id == me {
            return true;
        }
        let Some(trust) = &self.trust else {
            return true;
        };
        match (trust.get(&team), MemberKeys::of(m)) {
            (Some(t), Some(keys)) => t.approves(m.user_id, &keys),
            _ => false,
        }
    }
}

fn manages(m: &Member) -> bool {
    Grants::from_wire(&m.role, &m.permissions).has(Permission::MembersManage)
}

/// Whether a member's role entitles them to any of a team's scope keys.
fn holds_team_keys(m: &Member) -> bool {
    let g = Grants::from_wire(&m.role, &m.permissions);
    let any = Uuid::nil(); // `entitled` looks only at the kind of scope
    [Role::Settings(any), Role::Tasks(any), Role::Activity(any)]
        .into_iter()
        .any(|scope| entitled(scope, &g))
}

fn sign_pub_of(m: &Member) -> Option<[u8; 32]> {
    pub32(m.sign_pub.as_deref()?, "sign_pub").ok()
}

fn enc_pub_of(m: &Member) -> Option<[u8; 32]> {
    pub32(m.enc_pub.as_deref()?, "enc_pub").ok()
}

/// Whether a member's role entitles them to a team scope's key (§5).
fn entitled(role: Role, g: &Grants) -> bool {
    match role {
        Role::Settings(_) => g.has(Permission::SettingsRead),
        Role::Tasks(_) => g.has(Permission::TasksReadAll) || g.has(Permission::TasksReadAssigned),
        Role::Activity(_) => g.has(Permission::ActivityRead),
        Role::Personal => false,
    }
}

pub struct Engine<'a, T: Transport> {
    pub client: &'a Client<T>,
    pub session: &'a Session,
}

impl<T: Transport> Engine<'_, T> {
    fn me_pub(&self) -> [u8; 32] {
        self.session.keys.sign_pub()
    }

    fn author(&self) -> Author {
        Author {
            user: self.session.user,
            device: self.session.device,
        }
    }

    /// Run one cycle. `state` is updated in place (persist it afterwards, even
    /// on error: cursors and versions only move after successful writes).
    pub fn cycle(&self, input: &Input, state: &mut EngineState) -> Result<Output, CycleError> {
        state.shares_settled.clear();
        let mut out = Output::default();
        let me = self.client.me()?;
        let personal_ok = me.entitlements.personal_sync;
        let read_only = me.entitlements.read_only;
        let mut ctx = self.load_context(
            &me,
            Some((input.member_decisions, &mut state.trust)),
            &mut out,
        )?;
        let mut reload = false;
        if personal_ok && ctx.scope(Role::Personal).is_none() {
            match self.create_personal(&mut ctx) {
                Ok(()) => {}
                Err(CycleError::Api(ApiError::Conflict(_))) => reload = true,
                Err(e) => return Err(e),
            }
        }
        reload |= self.bootstrap_team_scopes(&me, &mut ctx, &mut out);
        if reload {
            ctx = self.load_context(
                &me,
                Some((input.member_decisions, &mut state.trust)),
                &mut out,
            )?;
        }
        self.fulfil_and_rotate(&mut ctx, &mut out);
        let mut held: Vec<(Uuid, u32)> = ctx.keys.keys().copied().collect();
        held.sort();
        if held.iter().any(|k| !state.held_keys.contains(k)) {
            // Re-reading is safe: anything already applied is stale by HLC.
            state.sync.cursor = 0;
        }
        state.held_keys = held;

        // --- pull ---
        let mut settings = input.settings.clone();
        let mut personal_tasks = input.personal_tasks.to_vec();
        let mut personal_tasks_changed = false;
        let mut team_tasks: HashMap<Uuid, Vec<Task>> = input.team_tasks.clone();
        let mut team_tasks_changed: Vec<Uuid> = Vec::new();
        loop {
            let ch = self.client.changes(state.sync.cursor, 500)?;
            for item in &ch.items {
                if let Err(e) = self.pull_item(
                    item,
                    input,
                    &ctx,
                    state,
                    &mut settings,
                    &mut personal_tasks,
                    &mut personal_tasks_changed,
                    &mut team_tasks,
                    &mut team_tasks_changed,
                    &mut out,
                ) {
                    out.errors.push(e);
                }
            }
            state.sync.cursor = ch.cursor;
            if !ch.more || ch.items.is_empty() {
                break;
            }
        }

        // --- push ---
        if !read_only {
            if personal_ok {
                self.push_personal_settings(input, &settings, &ctx, state, &mut out);
                if input.cloud.categories.tasks.pushes() {
                    self.push_tasks(
                        Role::Personal,
                        &personal_tasks,
                        &ctx,
                        state,
                        input.now_ms,
                        &mut out,
                    );
                }
            }
            for (team, list) in &team_tasks {
                if ctx.current_key(Role::Tasks(*team)).is_some() {
                    self.push_tasks(
                        Role::Tasks(*team),
                        list,
                        &ctx,
                        state,
                        input.now_ms,
                        &mut out,
                    );
                }
            }
            if personal_ok && input.cloud.categories.tasks.pushes() {
                self.push_task_deletions(Role::Personal, None, input, &ctx, state, &mut out);
            }
            let mut deleting_teams: Vec<Uuid> =
                input.task_deletions.iter().filter_map(|d| d.team).collect();
            deleting_teams.sort();
            deleting_teams.dedup();
            for team in deleting_teams {
                if ctx.current_key(Role::Tasks(team)).is_some() {
                    self.push_task_deletions(
                        Role::Tasks(team),
                        Some(team),
                        input,
                        &ctx,
                        state,
                        &mut out,
                    );
                }
            }
            for (i, w) in input.team_setting_writes.iter().enumerate() {
                let res = self.push_team_setting(w, &ctx, state, input.now_ms);
                let unsent = matches!(
                    &res,
                    Err(CycleError::Api(
                        ApiError::Cancelled
                            | ApiError::Network(_)
                            | ApiError::RateLimited(_)
                            | ApiError::Server(..)
                    ))
                );
                if !unsent {
                    state.shares_settled.push(i);
                }
                if let Err(e) = res {
                    out.errors
                        .push(format!("share {}: {e}", w.collection.as_str()));
                }
            }
            self.reseal_team_items(&ctx, state, &team_tasks, input.now_ms, &mut out);
        }

        // --- activity ---
        let retract: Vec<(Uuid, Presence)> = state
            .published_presence
            .iter()
            .filter(|t| !input.presence.iter().any(|(p, _)| p == *t))
            .map(|t| {
                (
                    *t,
                    Presence {
                        at_ms: input.now_ms,
                        ..Presence::default()
                    },
                )
            })
            .collect();
        let mut published = Vec::new();
        for (team, presence) in input.presence.iter().chain(&retract) {
            let grants = ctx.member_grants(*team, self.session.user);
            if read_only || !grants.has(Permission::ActivityPublish) {
                continue;
            }
            if let Some((_, generation, key)) = ctx.current_key(Role::Activity(*team)) {
                let body = serde_json::to_vec(presence).unwrap_or_default();
                match crypto::seal_activity(
                    *team,
                    generation,
                    key,
                    &body,
                    &self.session.keys,
                    self.author(),
                ) {
                    Ok(env) => {
                        let env = B64.encode(serde_json::to_vec(&env).unwrap_or_default());
                        match self.client.put_activity(*team, &env) {
                            Ok(()) => published.push(*team),
                            Err(e) => out.errors.push(format!("activity: {e}")),
                        }
                    }
                    Err(e) => out.errors.push(format!("activity: {e}")),
                }
            }
        }

        // What may be live on the server now: this cycle's teams, and any
        // retraction that failed (tried again next cycle).
        state.published_presence = input
            .presence
            .iter()
            .map(|(t, _)| *t)
            .chain(
                retract
                    .iter()
                    .map(|(t, _)| *t)
                    .filter(|t| !published.contains(t)),
            )
            .collect();

        // --- views ---
        // Forget team settings this user may no longer read: a team they
        // left, or a role without settings.read. (A later grant brings a new
        // key, which re-reads everything.)
        state.team_items.retain(|team, _| {
            me.teams.iter().any(|t| {
                t.id == *team && {
                    let perms = ctx
                        .member(t.id, self.session.user)
                        .map(|m| m.permissions.clone())
                        .unwrap_or_default();
                    Grants::from_wire(&t.role, &perms).has(Permission::SettingsRead)
                }
            })
        });
        for t in &me.teams {
            out.teams
                .push(self.team_view(t.id, &t.name, &t.role, &ctx, state));
        }
        for view in &out.teams {
            announce_approvals(view, &mut state.trust, &mut out.notes);
        }
        if personal_tasks_changed {
            out.personal_tasks = Some(personal_tasks);
        }
        for t in team_tasks_changed {
            if let Some(list) = team_tasks.remove(&t) {
                out.team_tasks.insert(t, list);
            }
        }
        out.me = Some(me);
        Ok(out)
    }

    /// `trust` is the user's pending decisions and this device's trust state,
    /// brought up to date here; `None` (an export) trusts every listed member.
    fn load_context(
        &self,
        me: &Me,
        trust: Option<(&[MemberDecision], &mut HashMap<Uuid, TeamTrust>)>,
        out: &mut Output,
    ) -> Result<Ctx, CycleError> {
        let mut members = HashMap::new();
        for t in &me.teams {
            match self.client.members(t.id) {
                Ok(m) => {
                    members.insert(t.id, m);
                }
                Err(e) => out.errors.push(format!("team {}: {e}", t.name)),
            }
        }
        let scopes = self
            .client
            .scopes()?
            .into_iter()
            .map(|s| ScopeInfo {
                id: s.id,
                generation: s.generation.max(1),
                role: match (s.kind, s.team_id) {
                    (ScopeKind::Personal, _) => Role::Personal,
                    (ScopeKind::TeamSettings, Some(t)) => Role::Settings(t),
                    (ScopeKind::TeamTasks, Some(t)) => Role::Tasks(t),
                    (ScopeKind::TeamActivity, Some(t)) => Role::Activity(t),
                    _ => Role::Personal,
                },
            })
            .collect::<Vec<_>>();
        let trust = match trust {
            Some((decisions, trust)) => {
                self.settle_trust(&members, &scopes, decisions, trust, out);
                Some(trust.clone())
            }
            None => None,
        };
        let mut ctx = Ctx {
            scopes,
            keys: HashMap::new(),
            members,
            trust,
        };
        for g in self.client.keys()? {
            let Some(scope) = ctx.by_id(g.scope_id) else {
                continue;
            };
            // Who may have signed this grant (§3.2).
            let granter_pub = match scope.role {
                Role::Personal if g.granter_user_id == self.session.user => Some(self.me_pub()),
                Role::Personal => None,
                Role::Settings(t) | Role::Tasks(t) | Role::Activity(t) => {
                    if g.granter_user_id == self.session.user {
                        Some(self.me_pub())
                    } else {
                        let granter = ctx.member(t, g.granter_user_id).filter(|m| manages(m));
                        if granter.is_some_and(|m| !ctx.trusts(t, m, self.session.user)) {
                            // A manager this user hasn't approved (yet): their
                            // key isn't used until they are. They are listed
                            // in the team's approvals, so this isn't an error.
                            continue;
                        }
                        granter.and_then(sign_pub_of)
                    }
                }
            };
            let Some(granter_pub) = granter_pub else {
                // An older generation's grant from a manager who has since left
                // is expected: items under it get re-sealed under the current
                // key (see `reseal_team_items`). Only a current one is suspect.
                if g.generation >= scope.generation {
                    out.errors
                        .push("ignored a key grant from someone not allowed to give it".into());
                }
                continue;
            };
            let wire = WireGrant {
                wrapped_key: g.wrapped_key.clone(),
                signature: g.signature.clone(),
            };
            match crypto::open_grant(
                &self.session.keys,
                self.session.user,
                g.scope_id,
                g.generation,
                &wire,
                &granter_pub,
            ) {
                Ok(k) => {
                    ctx.keys.insert((g.scope_id, g.generation), k);
                }
                Err(e) => out.errors.push(format!("key grant: {e}")),
            }
        }
        Ok(ctx)
    }

    /// Bring this device's trust up to date with the member lists: start it for
    /// teams seen for the first time, forget members who left, and apply the
    /// user's decisions.
    ///
    /// A team's trust is never dropped because the server stopped listing the
    /// team: listing it again would then count as a first sight, and trust
    /// whoever the server had added meanwhile.
    fn settle_trust(
        &self,
        members: &HashMap<Uuid, Vec<Member>>,
        scopes: &[ScopeInfo],
        decisions: &[MemberDecision],
        trust: &mut HashMap<Uuid, TeamTrust>,
        out: &mut Output,
    ) {
        // Members still waiting for a key, per team. Only a manager is told
        // (and only they hand keys out), and only a first sight needs it.
        let mut waiting: Option<HashMap<Uuid, HashSet<Uuid>>> = None;
        for (team, list) in members {
            if trust.contains_key(team) {
                continue;
            }
            // First sight of this team: trust the members found here, except
            // those still waiting for a key (the user decides about them).
            let i_manage = list
                .iter()
                .any(|m| m.user_id == self.session.user && manages(m));
            if i_manage && waiting.is_none() {
                let mut by_team: HashMap<Uuid, HashSet<Uuid>> = HashMap::new();
                match self.client.pending_grants() {
                    Ok(pending) => {
                        for p in pending {
                            let team = scopes.iter().find(|s| s.id == p.scope_id).and_then(|s| {
                                match s.role {
                                    Role::Settings(t) | Role::Tasks(t) | Role::Activity(t) => {
                                        Some(t)
                                    }
                                    Role::Personal => None,
                                }
                            });
                            if let Some(team) = team {
                                by_team.entry(team).or_default().insert(p.user_id);
                            }
                        }
                    }
                    Err(ApiError::Forbidden) => {}
                    Err(e) => {
                        // Try again next cycle rather than trust blindly.
                        out.errors.push(format!("pending grants: {e}"));
                        continue;
                    }
                }
                waiting = Some(by_team);
            }
            let unkeyed = waiting.as_ref().and_then(|w| w.get(team));
            let mut t = TeamTrust::default();
            for m in list {
                if m.user_id == self.session.user
                    || (i_manage && unkeyed.is_some_and(|w| w.contains(&m.user_id)))
                {
                    continue;
                }
                if let Some(keys) = MemberKeys::of(m) {
                    t.approved.insert(m.user_id, keys);
                }
            }
            trust.insert(*team, t);
        }
        for (team, t) in trust.iter_mut() {
            // Someone who left and comes back is asked about again. (Skipped
            // when the member list didn't load this cycle.)
            let Some(list) = members.get(team) else {
                continue;
            };
            let here = |user: &Uuid| list.iter().any(|m| m.user_id == *user);
            t.approved.retain(|u, _| here(u));
            t.declined.retain(|u, _| here(u));
            t.asked.retain(|u, _| here(u));
        }
        for d in decisions {
            let (Some(t), Some(m)) = (
                trust.get_mut(&d.team),
                members
                    .get(&d.team)
                    .and_then(|l| l.iter().find(|m| m.user_id == d.user)),
            ) else {
                continue;
            };
            // Only for the keys the user was shown.
            if MemberKeys::of(m).as_ref() != Some(&d.keys) {
                continue;
            }
            if d.approve {
                t.declined.remove(&d.user);
                t.approved.insert(d.user, d.keys.clone());
            } else {
                t.approved.remove(&d.user);
                t.declined.insert(d.user, d.keys.clone());
            }
        }
    }

    fn self_grant(&self, scope: Uuid, generation: u32, key: &Key) -> Result<NewGrant, CycleError> {
        let g = crypto::make_grant(
            &self.session.keys,
            self.session.user,
            &self.session.keys.enc_pub(),
            scope,
            generation,
            key,
        )?;
        Ok(NewGrant::new(self.session.user, generation, g))
    }

    /// Create the personal scope atomically with this user's own grant. A 409
    /// means another device won the race; the caller reloads.
    fn create_personal(&self, ctx: &mut Ctx) -> Result<(), CycleError> {
        let key = crypto::random_key()?;
        let id = Uuid::new_v4();
        let g = self.self_grant(id, 1, &key)?;
        self.client
            .create_scope(id, ScopeKind::Personal, None, std::slice::from_ref(&g))?;
        ctx.scopes.push(ScopeInfo {
            id,
            role: Role::Personal,
            generation: 1,
        });
        ctx.keys.insert((id, 1), key);
        Ok(())
    }

    /// The first manager to connect creates a team's three scopes, each with
    /// grants for every entitled member in the same request. Returns whether
    /// another client won a race (the caller reloads).
    fn bootstrap_team_scopes(&self, me: &Me, ctx: &mut Ctx, out: &mut Output) -> bool {
        let mut lost_race = false;
        for t in &me.teams {
            if !ctx
                .member_grants(t.id, self.session.user)
                .has(Permission::MembersManage)
            {
                continue;
            }
            for (kind, role) in [
                (ScopeKind::TeamSettings, Role::Settings(t.id)),
                (ScopeKind::TeamTasks, Role::Tasks(t.id)),
                (ScopeKind::TeamActivity, Role::Activity(t.id)),
            ] {
                if ctx.scope(role).is_some() {
                    continue;
                }
                let r = (|| -> Result<(), CycleError> {
                    let key = crypto::random_key()?;
                    let id = Uuid::new_v4();
                    let grants = self.grant_all(ctx, t.id, role, id, 1, &key)?;
                    if !grants.iter().any(|g| g.user_id == self.session.user) {
                        // Our own published key isn't in the member list yet.
                        return Ok(());
                    }
                    self.client.create_scope(id, kind, Some(t.id), &grants)?;
                    ctx.scopes.push(ScopeInfo {
                        id,
                        role,
                        generation: 1,
                    });
                    ctx.keys.insert((id, 1), key);
                    Ok(())
                })();
                match r {
                    Ok(()) => {}
                    Err(CycleError::Api(ApiError::Conflict(_))) => lost_race = true,
                    Err(e) => out.errors.push(format!("team {} setup: {e}", t.name)),
                }
            }
        }
        lost_race
    }

    /// Grants of `key` for every entitled member this device trusts. The rest
    /// are left for the server to list as pending, until the user approves
    /// them.
    fn grant_all(
        &self,
        ctx: &Ctx,
        team: Uuid,
        role: Role,
        scope: Uuid,
        generation: u32,
        key: &Key,
    ) -> Result<Vec<NewGrant>, CycleError> {
        let mut grants = Vec::new();
        for m in ctx.members.get(&team).into_iter().flatten() {
            if !entitled(role, &Grants::from_wire(&m.role, &m.permissions))
                || !ctx.trusts(team, m, self.session.user)
            {
                continue;
            }
            let Some(enc) = enc_pub_of(m) else { continue };
            let g =
                crypto::make_grant(&self.session.keys, m.user_id, &enc, scope, generation, key)?;
            grants.push(NewGrant::new(m.user_id, generation, g));
        }
        Ok(grants)
    }

    fn fulfil_and_rotate(&self, ctx: &mut Ctx, out: &mut Output) {
        // Rotation first, so pending grants go out at the newest generation.
        let rotate: Vec<(Uuid, Role)> = ctx
            .scopes
            .iter()
            .filter_map(|s| match s.role {
                Role::Personal => None,
                Role::Settings(t) | Role::Tasks(t) | Role::Activity(t) => Some((s.id, s.role, t)),
            })
            .filter(|(_, _, t)| {
                ctx.member_grants(*t, self.session.user)
                    .has(Permission::MembersManage)
            })
            .map(|(id, role, _)| (id, role))
            .collect();
        let needs = match self.client.scopes() {
            Ok(s) => s,
            Err(e) => {
                out.errors.push(format!("scopes: {e}"));
                return;
            }
        };
        for (id, role) in rotate {
            let Some(remote) = needs.iter().find(|s| s.id == id) else {
                continue;
            };
            if !remote.rotation_required {
                continue;
            }
            let team = match role {
                Role::Settings(t) | Role::Tasks(t) | Role::Activity(t) => t,
                Role::Personal => continue,
            };
            let next = remote.generation.max(1) + 1;
            let r = (|| -> Result<(), CycleError> {
                let key = crypto::random_key()?;
                let grants = self.grant_all(ctx, team, role, id, next, &key)?;
                self.client.rotate(id, next, &grants)?;
                if let Some(s) = ctx.scopes.iter_mut().find(|s| s.id == id) {
                    s.generation = next;
                }
                ctx.keys.insert((id, next), key);
                Ok(())
            })();
            match r {
                Ok(()) => out
                    .notes
                    .push("Rotated a team key after a membership change".into()),
                Err(e) => out.errors.push(format!("key rotation: {e}")),
            }
        }

        let pending = match self.client.pending_grants() {
            Ok(p) => p,
            Err(ApiError::Forbidden) => return,
            Err(e) => {
                out.errors.push(format!("pending grants: {e}"));
                return;
            }
        };
        for p in pending {
            let Some(scope) = ctx.by_id(p.scope_id) else {
                continue;
            };
            let team = match scope.role {
                Role::Settings(t) | Role::Tasks(t) | Role::Activity(t) => t,
                Role::Personal => continue,
            };
            let Some(key) = ctx.keys.get(&(p.scope_id, p.generation)) else {
                continue;
            };
            // Only to an actual, entitled member whose published key matches
            // and whom this user approved (the others wait in the team's
            // approvals).
            let Some(m) = ctx.member(team, p.user_id) else {
                continue;
            };
            if !entitled(scope.role, &Grants::from_wire(&m.role, &m.permissions))
                || m.enc_pub.as_deref() != Some(p.enc_pub.as_str())
                || !ctx.trusts(team, m, self.session.user)
            {
                continue;
            }
            let Ok(enc) = pub32(&p.enc_pub, "enc_pub") else {
                continue;
            };
            let r = crypto::make_grant(
                &self.session.keys,
                p.user_id,
                &enc,
                p.scope_id,
                p.generation,
                key,
            )
            .map_err(CycleError::from)
            .and_then(|g| {
                self.client
                    .post_grant(p.scope_id, &NewGrant::new(p.user_id, p.generation, g))
                    .map_err(CycleError::from)
            });
            match r {
                // One note per member, not one per scope key.
                Ok(()) => {
                    let note = format!("Shared team keys with {}", m.display());
                    if !out.notes.contains(&note) {
                        out.notes.push(note);
                    }
                }
                Err(e) => out.errors.push(format!("grant: {e}")),
            }
        }
    }

    /// The author's signing key, if they are allowed to write `collection` in
    /// this scope.
    fn writer_pub(
        &self,
        ctx: &Ctx,
        role: Role,
        collection: Collection,
        author: Uuid,
    ) -> Option<[u8; 32]> {
        match role {
            Role::Personal => (author == self.session.user).then(|| self.me_pub()),
            Role::Settings(t) => {
                let m = ctx.member(t, author)?;
                Grants::from_wire(&m.role, &m.permissions)
                    .has(Permission::SettingsWrite)
                    .then(|| sign_pub_of(m))
                    .flatten()
            }
            Role::Tasks(t) => {
                if collection != Collection::Tasks {
                    return None;
                }
                let m = ctx.member(t, author)?;
                let g = Grants::from_wire(&m.role, &m.permissions);
                (g.has(Permission::TasksUpdate) || g.has(Permission::TasksCreate))
                    .then(|| sign_pub_of(m))
                    .flatten()
            }
            Role::Activity(_) => None,
        }
    }

    /// Check and decrypt one item from `/changes`, with the key generation it
    /// was sealed under. `Ok(None)`: a scope this device doesn't know, or a key
    /// it hasn't been granted (yet).
    fn open_remote(
        &self,
        item: &crate::api::RemoteItem,
        ctx: &Ctx,
    ) -> Result<Option<(Role, Collection, Plain, u32)>, String> {
        let Some(scope) = ctx.by_id(item.scope_id) else {
            return Ok(None);
        };
        let role = scope.role;
        let collection = Collection::parse(&item.collection)
            .ok_or_else(|| format!("unknown collection {}", item.collection))?;
        let raw = B64
            .decode(item.envelope.as_bytes())
            .map_err(|_| "malformed envelope".to_string())?;
        let env: Envelope =
            serde_json::from_slice(&raw).map_err(|_| "malformed envelope".to_string())?;
        if env.by.user != item.author_user_id {
            return Err("envelope author doesn't match the server's record".into());
        }
        let author_pub = self
            .writer_pub(ctx, role, collection, env.by.user)
            .ok_or_else(|| {
                format!(
                    "rejected a {} item from a member who may not write it",
                    collection.as_str()
                )
            })?;
        let Some(key) = ctx.keys.get(&(item.scope_id, env.generation)) else {
            return Ok(None);
        };
        let addr = ItemAddr {
            scope: item.scope_id,
            collection: collection.as_str(),
            item: item.item_id,
        };
        let pt = crypto::open_item(addr, &env, ItemKey::Scope(key), &author_pub)
            .map_err(|e| format!("{}: {e}", collection.as_str()))?;
        let plain: Plain = serde_json::from_slice(&pt).map_err(|e| e.to_string())?;
        if plain.kind != collection.as_str() {
            return Err("item kind doesn't match its collection".into());
        }
        Ok(Some((role, collection, plain, env.generation)))
    }

    /// Decrypt everything this account can read, for an export. Reads with the
    /// server's export flag, which also returns scopes whose plan has lapsed,
    /// and never touches the sync state.
    pub fn export(&self, now_ms: u64) -> Result<Export, CycleError> {
        let me = self.client.me()?;
        let mut scratch = Output::default();
        let ctx = self.load_context(&me, None, &mut scratch)?;
        let mut personal: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut teams: BTreeMap<Uuid, BTreeMap<String, Vec<Value>>> = BTreeMap::new();
        let (mut items, mut skipped) = (0, 0);
        let mut cursor = 0;
        loop {
            let ch = self.client.export_changes(cursor, 500)?;
            for item in ch.items.iter().filter(|i| !i.deleted) {
                match self.open_remote(item, &ctx) {
                    Ok(Some((role, collection, plain, _))) => {
                        let Some(data) = plain.data else { continue };
                        let bucket = match role {
                            Role::Personal => &mut personal,
                            Role::Settings(t) | Role::Tasks(t) | Role::Activity(t) => {
                                teams.entry(t).or_default()
                            }
                        };
                        bucket
                            .entry(collection.as_str().to_string())
                            .or_default()
                            .push(data);
                        items += 1;
                    }
                    Ok(None) | Err(_) => skipped += 1,
                }
            }
            cursor = ch.cursor;
            if !ch.more || ch.items.is_empty() {
                break;
            }
        }
        let teams: Vec<Value> = teams
            .into_iter()
            .map(|(id, items)| {
                let team = me.teams.iter().find(|t| t.id == id);
                serde_json::json!({
                    "id": id,
                    "name": team.map(|t| t.name.as_str()).unwrap_or_default(),
                    "role": team.map(|t| t.role.as_str()).unwrap_or_default(),
                    "items": items,
                })
            })
            .collect();
        let json = serde_json::json!({
            "format": "muxel-cloud-export",
            "version": 1,
            "exported_at_ms": now_ms,
            "account": {
                "user_id": me.user.id,
                "email": me.user.email,
                "name": me.user.name,
            },
            "personal": personal,
            "teams": teams,
        });
        Ok(Export {
            json,
            items,
            skipped,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn pull_item(
        &self,
        item: &crate::api::RemoteItem,
        input: &Input,
        ctx: &Ctx,
        state: &mut EngineState,
        settings: &mut Settings,
        personal_tasks: &mut Vec<Task>,
        personal_tasks_changed: &mut bool,
        team_tasks: &mut HashMap<Uuid, Vec<Task>>,
        team_tasks_changed: &mut Vec<Uuid>,
        out: &mut Output,
    ) -> Result<(), String> {
        let Some((role, collection, plain, generation)) = self.open_remote(item, ctx)? else {
            return Ok(()); // unknown scope, or key not granted yet (a resync after the grant fetches it)
        };
        // Only members whose role may delete tasks can tombstone a team task.
        if plain.data.is_none()
            && let Role::Tasks(team) = role
            && !ctx.member(team, item.author_user_id).is_some_and(|m| {
                Grants::from_wire(&m.role, &m.permissions).has(Permission::TasksDelete)
            })
        {
            return Err("rejected a task deletion from a member who may not delete tasks".into());
        }
        let pending_delete = |id: Uuid| input.task_deletions.iter().any(|d| d.id == id);
        state.sync.observe(plain.hlc);
        let scope_state = state.sync.scope(item.scope_id);
        let prior = scope_state.get(collection, item.item_id).cloned();
        let record = |s: &mut ScopeState, hash: u64| {
            s.set(
                collection,
                item.item_id,
                ItemState {
                    version: item.version,
                    hlc: plain.hlc,
                    hash,
                    deleted: plain.data.is_none(),
                    author: Some(item.author_user_id),
                    generation,
                },
            )
        };

        match (role, collection) {
            (Role::Personal, Collection::Tasks) => {
                if !input.cloud.categories.tasks.pulls() {
                    return Ok(());
                }
                merge_task_item(
                    personal_tasks,
                    item.item_id,
                    pending_delete(item.item_id),
                    plain.data.as_ref(),
                    prior.as_ref(),
                    plain.hlc,
                    personal_tasks_changed,
                )?;
                let hash = local_task_hash(personal_tasks, item.item_id);
                record(state.sync.scope(item.scope_id), hash);
            }
            (Role::Personal, c) => {
                let mode = input.cloud.categories.mode(c);
                if !mode.pulls() {
                    return Ok(());
                }
                let carry = state.sync.scope(item.scope_id).carry.clone();
                let resolver = Resolver {
                    presets: &settings.presets.clone(),
                    projects: input.projects,
                    carry: &carry,
                };
                let local = items::extract(settings, &resolver, &[c])
                    .into_iter()
                    .find(|l| l.id == item.item_id)
                    .map(|l| content_hash(Some(&l.data)));
                match decide_remote(prior.as_ref(), local, plain.hlc) {
                    RemoteDecision::Stale => return Ok(()),
                    RemoteDecision::LocalWins => {
                        // Keep ours; remember the server version so the push
                        // that follows doesn't conflict.
                        if let Some(mut p) = prior {
                            p.version = item.version;
                            p.hlc = plain.hlc;
                            state.sync.scope(item.scope_id).set(c, item.item_id, p);
                        }
                        return Ok(());
                    }
                    RemoteDecision::Apply => {}
                }
                let synced = state.sync.scope(item.scope_id).clone();
                let applied = items::apply(
                    settings,
                    &resolver,
                    c,
                    item.item_id,
                    plain.data.as_ref(),
                    &|id| synced.knows(id),
                )
                .map_err(|e| e.0)?;
                if applied.changed || applied.adopted_from.is_some() {
                    out.settings_changes.push(SettingsDelta {
                        scope: item.scope_id,
                        collection: c,
                        id: item.item_id,
                        data: plain.data.clone(),
                        before: local,
                    });
                }
                let s = state.sync.scope(item.scope_id);
                match applied.carry {
                    Some(c) => {
                        s.carry.insert(item.item_id, c);
                    }
                    None => {
                        s.carry.remove(&item.item_id);
                    }
                }
                if applied.new_loop_off
                    && let Some(name) = plain
                        .data
                        .as_ref()
                        .and_then(|d| d.get("name"))
                        .and_then(Value::as_str)
                {
                    out.notes.push(format!(
                        "Synced loop \u{201c}{name}\u{201d} arrived switched off"
                    ));
                }
                // Hash what this device now has, so it isn't pushed back.
                let carry = s.carry.clone();
                let resolver = Resolver {
                    presets: &settings.presets.clone(),
                    projects: input.projects,
                    carry: &carry,
                };
                let hash = items::extract(settings, &resolver, &[c])
                    .into_iter()
                    .find(|l| l.id == item.item_id)
                    .map_or(content_hash(None), |l| content_hash(Some(&l.data)));
                record(state.sync.scope(item.scope_id), hash);
            }
            (Role::Settings(team), c) => {
                let cache = state.team_items.entry(team).or_default();
                let key = format!("{}:{}", c.as_str(), item.item_id);
                if prior.as_ref().is_some_and(|p| plain.hlc <= p.hlc) {
                    return Ok(());
                }
                match &plain.data {
                    Some(d) => {
                        cache.insert(key, d.clone());
                    }
                    None => {
                        cache.remove(&key);
                    }
                }
                record(
                    state.sync.scope(item.scope_id),
                    content_hash(plain.data.as_ref()),
                );
            }
            (Role::Tasks(team), Collection::Tasks) => {
                let list = team_tasks.entry(team).or_default();
                let mut changed = false;
                merge_task_item(
                    list,
                    item.item_id,
                    pending_delete(item.item_id),
                    plain.data.as_ref(),
                    prior.as_ref(),
                    plain.hlc,
                    &mut changed,
                )?;
                if changed && !team_tasks_changed.contains(&team) {
                    team_tasks_changed.push(team);
                }
                let hash = local_task_hash(list, item.item_id);
                record(state.sync.scope(item.scope_id), hash);
            }
            _ => {}
        }
        Ok(())
    }

    fn put(
        &self,
        role: Role,
        ctx: &Ctx,
        state: &mut EngineState,
        w: Write,
    ) -> Result<(), CycleError> {
        let Write {
            collection,
            id,
            base_version,
            data,
            hash,
            assignee,
            now_ms,
        } = w;
        let (scope, generation, key) = ctx
            .current_key(role)
            .ok_or(CycleError::Api(ApiError::Forbidden))?;
        let hlc = state.sync.stamp(now_ms, self.session.device);
        let deleted = data.is_none();
        let plain = serde_json::to_vec(&Plain {
            hlc,
            kind: collection.as_str().to_string(),
            data,
        })
        .unwrap_or_default();
        let env = crypto::seal_item(
            ItemAddr {
                scope,
                collection: collection.as_str(),
                item: id,
            },
            generation,
            key,
            &plain,
            &self.session.keys,
            self.author(),
            None,
        )?;
        let env = B64.encode(serde_json::to_vec(&env).unwrap_or_default());
        let r = self.client.put_item(
            scope,
            collection.as_str(),
            id,
            &crate::api::ItemWrite {
                base_version,
                envelope: &env,
                deleted,
                assignee,
            },
        )?;
        state.sync.scope(scope).set(
            collection,
            id,
            ItemState {
                version: r.version,
                hlc,
                hash,
                deleted,
                author: Some(self.session.user),
                generation,
            },
        );
        Ok(())
    }

    fn push_personal_settings(
        &self,
        input: &Input,
        settings: &Settings,
        ctx: &Ctx,
        state: &mut EngineState,
        out: &mut Output,
    ) {
        let Some(scope) = ctx.scope(Role::Personal).map(|s| s.id) else {
            return;
        };
        let pushing: Vec<Collection> = Collection::SETTINGS
            .into_iter()
            .filter(|c| input.cloud.categories.mode(*c).pushes())
            .collect();
        let carry: HashMap<Uuid, Carry> = state.sync.scope(scope).carry.clone();
        let resolver = Resolver {
            presets: &settings.presets,
            projects: input.projects,
            carry: &carry,
        };
        let local = items::extract(settings, &resolver, &pushing);
        let plan = plan_push(state.sync.scope(scope), &local, &pushing);
        for p in plan {
            let collection = p.collection;
            if let Err(e) = self.put(
                Role::Personal,
                ctx,
                state,
                Write {
                    collection,
                    id: p.id,
                    base_version: p.base_version,
                    data: p.data,
                    hash: p.hash,
                    assignee: None,
                    now_ms: input.now_ms,
                },
            ) && !matches!(e, CycleError::Api(ApiError::Conflict(_)))
            {
                out.errors.push(format!("{}: {e}", collection.as_str()));
            }
        }
    }

    fn push_tasks(
        &self,
        role: Role,
        list: &[Task],
        ctx: &Ctx,
        state: &mut EngineState,
        now_ms: u64,
        out: &mut Output,
    ) {
        let Some(scope) = ctx.scope(role).map(|s| s.id) else {
            return;
        };
        let local: Vec<LocalItem> = list
            .iter()
            .map(|t| LocalItem {
                collection: Collection::Tasks,
                id: t.id,
                data: serde_json::to_value(t).unwrap_or(Value::Null),
            })
            .collect();
        let plan = plan_push(state.sync.scope(scope), &local, &[Collection::Tasks]);
        for p in plan {
            let assignee = list.iter().find(|t| t.id == p.id).and_then(|t| t.assignee);
            if let Err(e) = self.put(
                role,
                ctx,
                state,
                Write {
                    collection: Collection::Tasks,
                    id: p.id,
                    base_version: p.base_version,
                    data: p.data,
                    hash: p.hash,
                    assignee,
                    now_ms,
                },
            ) && !matches!(e, CycleError::Api(ApiError::Conflict(_)))
            {
                out.errors.push(format!("task: {e}"));
            }
        }
    }

    /// Send this scope's pending task deletions as tombstones.
    fn push_task_deletions(
        &self,
        role: Role,
        team: Option<Uuid>,
        input: &Input,
        ctx: &Ctx,
        state: &mut EngineState,
        out: &mut Output,
    ) {
        let Some(scope) = ctx.scope(role).map(|s| s.id) else {
            return;
        };
        for d in input.task_deletions.iter().filter(|d| d.team == team) {
            let known = state
                .sync
                .scope(scope)
                .get(Collection::Tasks, d.id)
                .cloned();
            let Some(st) = known.filter(|st| !st.deleted) else {
                // Never reached the server, or already gone there.
                out.deletions_done.push(d.id);
                continue;
            };
            match self.put(
                role,
                ctx,
                state,
                Write {
                    collection: Collection::Tasks,
                    id: d.id,
                    base_version: st.version,
                    data: None,
                    hash: content_hash(None),
                    assignee: d.assignee,
                    now_ms: input.now_ms,
                },
            ) {
                Ok(()) => out.deletions_done.push(d.id),
                // Changed meanwhile: the next pull learns the new version and the
                // deletion is retried.
                Err(CycleError::Api(ApiError::Conflict(_))) => {}
                Err(CycleError::Api(ApiError::Forbidden)) => {
                    out.errors.push(
                        "muxel Cloud refused to delete a task: your role doesn't allow it".into(),
                    );
                    out.deletions_done.push(d.id);
                    // Bring the task back: forget it, and re-read from the start
                    // next cycle (what's already here is stale by HLC).
                    state
                        .sync
                        .scope(scope)
                        .items
                        .remove(&muxel_core::cloud::state::item_key(Collection::Tasks, d.id));
                    state.sync.cursor = 0;
                }
                Err(e) => out.errors.push(format!("delete task: {e}")),
            }
        }
    }

    /// Re-seal team items whose signature or key is about to stop working for
    /// the rest of the team: written by someone who is no longer a writer there
    /// (they left, or their role changed), or sealed under an older key
    /// generation than the scope's current one (late joiners and new devices
    /// only get the current key, and a grant from a manager who left stops
    /// verifying). This device signs them again under the current key, so team
    /// data outlives the people who wrote it.
    ///
    /// Only this device's own synced copy is re-sealed, and only while it is
    /// unchanged since it was verified or written here; the write is
    /// conditional on the server still holding that version. The server can't
    /// use this to get anything signed that a trusted writer didn't write.
    fn reseal_team_items(
        &self,
        ctx: &Ctx,
        state: &mut EngineState,
        team_tasks: &HashMap<Uuid, Vec<Task>>,
        now_ms: u64,
        out: &mut Output,
    ) {
        let mut resealed = 0;
        // Forget refusals for items that have moved on (or gone).
        let sync = &state.sync;
        state.reseal_refused.retain(|(scope, key, v)| {
            sync.scopes
                .get(scope)
                .and_then(|s| s.items.get(key))
                .is_some_and(|st| st.version == *v)
        });
        for scope in &ctx.scopes {
            let (team, tasks) = match scope.role {
                Role::Settings(t) => (t, false),
                Role::Tasks(t) => (t, true),
                _ => continue,
            };
            if ctx.current_key(scope.role).is_none() {
                continue;
            }
            let mine = ctx.member_grants(team, self.session.user);
            let can_write = if tasks {
                mine.has(Permission::TasksUpdate)
            } else {
                mine.has(Permission::SettingsWrite)
            };
            if !can_write {
                continue;
            }
            let Some(synced) = state.sync.scopes.get(&scope.id) else {
                continue;
            };
            let mut due: Vec<(Collection, Uuid, ItemState)> = Vec::new();
            for (key, st) in &synced.items {
                let Some(author) = st.author else { continue };
                if st.generation == 0 {
                    continue; // recorded before authors were; nothing to judge by
                }
                let Some((c, id)) = key
                    .split_once(':')
                    .and_then(|(c, id)| Some((Collection::parse(c)?, Uuid::parse_str(id).ok()?)))
                else {
                    continue;
                };
                let trusted = self.writer_pub(ctx, scope.role, c, author).is_some()
                    && (!tasks
                        || !st.deleted
                        || ctx.member_grants(team, author).has(Permission::TasksDelete));
                if trusted && st.generation >= scope.generation {
                    continue;
                }
                if tasks && st.deleted && !mine.has(Permission::TasksDelete) {
                    continue;
                }
                let refused = state
                    .reseal_refused
                    .iter()
                    .any(|(s, k, v)| *s == scope.id && k == key && *v == st.version);
                if !refused {
                    due.push((c, id, st.clone()));
                }
            }
            for (c, id, st) in due {
                let (data, assignee) = if tasks {
                    let task = team_tasks
                        .get(&team)
                        .and_then(|l| l.iter().find(|t| t.id == id));
                    (
                        task.map(|t| serde_json::to_value(t).unwrap_or(Value::Null)),
                        task.and_then(|t| t.assignee),
                    )
                } else {
                    let key = format!("{}:{}", c.as_str(), id);
                    let cached = state.team_items.get(&team).and_then(|m| m.get(&key));
                    (cached.cloned(), None)
                };
                // Re-seal only the copy that was verified: an edit made here
                // since goes out as an ordinary write instead.
                if data.is_none() != st.deleted || content_hash(data.as_ref()) != st.hash {
                    continue;
                }
                match self.put(
                    scope.role,
                    ctx,
                    state,
                    Write {
                        collection: c,
                        id,
                        base_version: st.version,
                        data,
                        hash: st.hash,
                        assignee,
                        now_ms,
                    },
                ) {
                    Ok(()) => resealed += 1,
                    // Someone wrote it since (the next pull brings it), or the
                    // server's rules (a sync policy, a task no longer visible
                    // here) leave it to another writer. Don't ask again.
                    Err(CycleError::Api(ApiError::Conflict(_) | ApiError::Forbidden)) => {
                        state.reseal_refused.push((
                            scope.id,
                            format!("{}:{}", c.as_str(), id),
                            st.version,
                        ));
                    }
                    Err(e) => out.errors.push(format!("re-seal {}: {e}", c.as_str())),
                }
            }
        }
        if resealed > 0 {
            out.notes.push(format!(
                "Re-sealed {resealed} team item{} under the current key",
                if resealed == 1 { "" } else { "s" }
            ));
        }
    }

    fn push_team_setting(
        &self,
        w: &TeamSettingWrite,
        ctx: &Ctx,
        state: &mut EngineState,
        now_ms: u64,
    ) -> Result<(), CycleError> {
        if !Collection::TEAM_SETTINGS.contains(&w.collection) {
            return Err(CycleError::Api(ApiError::Forbidden));
        }
        let role = Role::Settings(w.team);
        let scope = ctx
            .scope(role)
            .map(|s| s.id)
            .ok_or(CycleError::Api(ApiError::Forbidden))?;
        let base = state
            .sync
            .scope(scope)
            .get(w.collection, w.id)
            .map_or(0, |s| s.version);
        let hash = content_hash(w.data.as_ref());
        self.put(
            role,
            ctx,
            state,
            Write {
                collection: w.collection,
                id: w.id,
                base_version: base,
                data: w.data.clone(),
                hash,
                assignee: None,
                now_ms,
            },
        )?;
        let cache = state.team_items.entry(w.team).or_default();
        let key = format!("{}:{}", w.collection.as_str(), w.id);
        match &w.data {
            Some(d) => {
                cache.insert(key, d.clone());
            }
            None => {
                cache.remove(&key);
            }
        }
        Ok(())
    }

    fn team_view(
        &self,
        team: Uuid,
        name: &str,
        role: &str,
        ctx: &Ctx,
        state: &EngineState,
    ) -> TeamView {
        let me = ctx.member(team, self.session.user);
        let permissions = me.map(|m| m.permissions.clone()).unwrap_or_default();
        let grants = Grants::from_wire(role, &permissions);
        let mut items: BTreeMap<String, Vec<(Uuid, Value)>> = BTreeMap::new();
        if let Some(cache) = state.team_items.get(&team) {
            for (k, v) in cache {
                if let Some((c, id)) = k.split_once(':')
                    && let Ok(id) = Uuid::parse_str(id)
                {
                    items
                        .entry(c.to_string())
                        .or_default()
                        .push((id, v.clone()));
                }
            }
        }
        let mut activity = Vec::new();
        if grants.has(Permission::ActivityRead)
            && let Some(scope) = ctx.scope(Role::Activity(team))
            && let Ok(rows) = self.client.activity(team)
        {
            for row in rows {
                let Some(author) = ctx.member(team, row.user_id).and_then(sign_pub_of) else {
                    continue;
                };
                let Ok(raw) = B64.decode(row.envelope.as_bytes()) else {
                    continue;
                };
                let Ok(env) = serde_json::from_slice::<crypto::ActivityEnvelope>(&raw) else {
                    continue;
                };
                if env.by.user != row.user_id {
                    continue;
                }
                let Some(key) = ctx.keys.get(&(scope.id, env.generation)) else {
                    continue;
                };
                if let Ok(pt) = crypto::open_activity(team, &env, key, &author)
                    && let Ok(presence) = serde_json::from_slice::<Presence>(&pt)
                {
                    activity.push(ActivityView {
                        user_id: row.user_id,
                        device_id: row.device_id,
                        presence,
                        updated_at: row.updated_at,
                    });
                }
            }
        }
        let waiting_for_keys = [
            Role::Settings(team),
            Role::Tasks(team),
            Role::Activity(team),
        ]
        .into_iter()
        .any(|r| entitled(r, &grants) && ctx.current_key(r).is_none());
        let approvals = self.approvals(team, &grants, ctx);
        TeamView {
            id: team,
            name: name.to_string(),
            role: role.to_string(),
            permissions,
            members: ctx
                .members
                .get(&team)
                .into_iter()
                .flatten()
                .map(|m| MemberView {
                    user_id: m.user_id,
                    name: m.display().to_string(),
                    email: m.email.clone(),
                    role: m.role.clone(),
                    fingerprint: m
                        .fingerprint
                        .clone()
                        .or_else(|| Some(crypto::fingerprint(&enc_pub_of(m)?, &sign_pub_of(m)?))),
                    permissions: m.permissions.clone(),
                })
                .collect(),
            items,
            activity,
            waiting_for_keys,
            approvals,
        }
    }

    /// The members this user still has to decide about: for someone who
    /// manages the team, everyone who would get a key from them or could give
    /// them one; for anyone else, just the managers they take keys from.
    fn approvals(&self, team: Uuid, mine: &Grants, ctx: &Ctx) -> Vec<ApprovalView> {
        let Some(trust) = ctx.trust.as_ref().and_then(|t| t.get(&team)) else {
            return Vec::new();
        };
        let i_manage = mine.has(Permission::MembersManage);
        let mut out = Vec::new();
        for m in ctx.members.get(&team).into_iter().flatten() {
            if m.user_id == self.session.user || ctx.trusts(team, m, self.session.user) {
                continue;
            }
            let manager = manages(m);
            if !(manager || (i_manage && holds_team_keys(m))) {
                continue;
            }
            let Some(keys) = MemberKeys::of(m) else {
                continue; // no account keys yet: nothing to approve
            };
            let Some(fingerprint) = keys.fingerprint() else {
                continue;
            };
            let known = trust
                .approved
                .get(&m.user_id)
                .or_else(|| trust.declined.get(&m.user_id));
            out.push(ApprovalView {
                user_id: m.user_id,
                name: m.display().to_string(),
                email: m.email.clone(),
                role: m.role.clone(),
                fingerprint,
                keys_changed: known.is_some_and(|k| *k != keys),
                declined: trust.declined.get(&m.user_id) == Some(&keys),
                manager,
                keys,
            });
        }
        out
    }
}

/// One notification for each member who starts waiting for a decision.
fn announce_approvals(
    view: &TeamView,
    trust: &mut HashMap<Uuid, TeamTrust>,
    notes: &mut Vec<String>,
) {
    let Some(t) = trust.get_mut(&view.id) else {
        return;
    };
    let i_manage = view.grants().has(Permission::MembersManage);
    for a in &view.approvals {
        if a.declined || t.asked.get(&a.user_id) == Some(&a.keys) {
            continue;
        }
        t.asked.insert(a.user_id, a.keys.clone());
        let team = &view.name;
        let who = &a.name;
        notes.push(if a.keys_changed {
            format!(
                "{who}'s encryption key in {team} changed. Check with them before you approve it in Settings → muxel Cloud."
            )
        } else if i_manage {
            format!(
                "{who} joined {team}. Approve them in Settings → muxel Cloud to share the team's keys with them."
            )
        } else {
            format!(
                "{who} now manages {team}'s keys. Approve them in Settings → muxel Cloud to keep syncing with the team."
            )
        });
    }
}

fn local_task_hash(list: &[Task], id: Uuid) -> u64 {
    match list.iter().find(|t| t.id == id) {
        Some(t) => content_hash(Some(&serde_json::to_value(t).unwrap_or(Value::Null))),
        None => content_hash(None),
    }
}

/// Merge a remote task into `list`: unions notes; scalar fields follow the
/// newer side (a local copy edited since the last sync counts as newer).
#[allow(clippy::too_many_arguments)]
fn merge_task_item(
    list: &mut Vec<Task>,
    id: Uuid,
    pending_delete: bool,
    data: Option<&Value>,
    prior: Option<&ItemState>,
    remote_hlc: Hlc,
    changed: &mut bool,
) -> Result<(), String> {
    if prior.is_some_and(|p| remote_hlc <= p.hlc) {
        return Ok(());
    }
    // A tombstone: deleted for good (it wins over local edits).
    let Some(data) = data else {
        if let Some(i) = list.iter().position(|t| t.id == id) {
            list.remove(i);
            *changed = true;
        }
        return Ok(());
    };
    // Deleted here and not yet sent: don't bring it back.
    if pending_delete {
        return Ok(());
    }
    let remote: Task = serde_json::from_value(data.clone()).map_err(|e| e.to_string())?;
    match list.iter_mut().find(|t| t.id == remote.id) {
        Some(local) => {
            let local_hash =
                content_hash(Some(&serde_json::to_value(&*local).unwrap_or(Value::Null)));
            let local_dirty = prior.is_some_and(|p| p.hash != local_hash);
            let merged = tasks::merge(local, &remote, !local_dirty);
            if merged != *local {
                *local = merged;
                *changed = true;
            }
        }
        None => {
            list.push(remote);
            *changed = true;
        }
    }
    Ok(())
}
