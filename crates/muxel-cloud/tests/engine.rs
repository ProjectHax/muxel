//! End-to-end sync through an in-memory server that implements the protocol's
//! routing and access rules (docs/cloud-sync-protocol.md), with real crypto.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use muxel_cloud::api::{ApiError, Client, Transport};
use muxel_cloud::crypto;
use muxel_cloud::engine::{Engine, EngineState, Input, Session, TaskDeletion};
use muxel_core::Settings;
use muxel_core::cloud::CloudSettings;
use muxel_core::cloud::activity::{AgentActivity, Presence, ProjectActivity};
use muxel_core::cloud::roles::{Grants, Permission};
use muxel_core::cloud::tasks::{ProgressNote, Task};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone)]
struct Item {
    scope: Uuid,
    collection: String,
    id: Uuid,
    version: u64,
    seq: u64,
    author: Uuid,
    assignee: Option<Uuid>,
    deleted: bool,
    envelope: String,
}

#[derive(Default)]
struct World {
    seq: u64,
    users: HashMap<Uuid, (String, String)>, // user → (enc_pub, sign_pub)
    tokens: HashMap<String, (Uuid, Uuid)>,  // token → (user, device)
    teams: HashMap<Uuid, (String, Vec<(Uuid, String)>)>, // team → (name, [(user, role)])
    scopes: Vec<(Uuid, String, Option<Uuid>, u32, bool, Uuid)>, // id, kind, team, gen, rotation_required, creator
    grants: Vec<(Uuid, u32, Uuid, String, String, Uuid)>, // scope, gen, user, wrapped, sig, granter
    items: Vec<Item>,
    activity: HashMap<(Uuid, Uuid, Uuid), String>, // (team, user, device) → envelope
    /// A misbehaving server that lets anyone tombstone a team task.
    allow_any_delete: bool,
    /// A misbehaving server that skips the team write rules.
    allow_any_write: bool,
    /// Item writes received (accepted or not).
    puts: usize,
}

impl World {
    fn role(&self, team: Uuid, user: Uuid) -> Option<String> {
        self.teams
            .get(&team)?
            .1
            .iter()
            .find(|(u, _)| *u == user)
            .map(|(_, r)| r.clone())
    }
    fn grants_of(&self, team: Uuid, user: Uuid) -> Grants {
        self.role(team, user)
            .map(|r| Grants::from_wire(&r, &[]))
            .unwrap_or_default()
    }
    fn can_read(&self, user: Uuid, it: &Item) -> bool {
        let Some(s) = self.scopes.iter().find(|s| s.0 == it.scope) else {
            return false;
        };
        match (s.1.as_str(), s.2) {
            ("personal", _) => s.5 == user,
            ("team_settings", Some(t)) => self.grants_of(t, user).has(Permission::SettingsRead),
            ("team_tasks", Some(t)) => self
                .grants_of(t, user)
                .can_see_task(it.assignee == Some(user)),
            _ => false,
        }
    }
    fn entitled(&self, kind: &str, team: Uuid, user: Uuid) -> bool {
        let g = self.grants_of(team, user);
        match kind {
            "team_settings" => g.has(Permission::SettingsRead),
            "team_tasks" => g.has(Permission::TasksReadAll) || g.has(Permission::TasksReadAssigned),
            "team_activity" => g.has(Permission::ActivityRead),
            _ => false,
        }
    }
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<World>>);

fn ok(v: Value) -> muxel_cloud::api::Result<(u16, Value)> {
    Ok((200, v))
}
fn status(code: u16) -> muxel_cloud::api::Result<(u16, Value)> {
    Ok((code, json!({"error": "x"})))
}

impl Transport for Fake {
    fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> muxel_cloud::api::Result<(u16, Value)> {
        let mut w = self.0.lock().unwrap();
        let Some(&(user, device)) = token.and_then(|t| w.tokens.get(t)) else {
            return status(401);
        };
        let body = body.cloned().unwrap_or(Value::Null);
        let parts: Vec<&str> = path
            .split('?')
            .next()
            .unwrap()
            .trim_matches('/')
            .split('/')
            .collect();
        let s = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
        let uid = |v: &Value, k: &str| Uuid::parse_str(v[k].as_str().unwrap_or_default()).ok();
        match (method, parts.as_slice()) {
            ("GET", ["me"]) => {
                let teams: Vec<Value> = w
                    .teams
                    .iter()
                    .filter_map(|(id, (name, _))| {
                        Some(json!({"id": id, "name": name, "role": w.role(*id, user)?}))
                    })
                    .collect();
                ok(json!({"user": {"id": user, "email": format!("{user}@x")},
                          "device": {"id": device},
                          "entitlements": {"personal_sync": true, "read_only": false, "plan": "team"},
                          "teams": teams}))
            }
            ("GET", ["scopes"]) => {
                let v: Vec<Value> = w
                    .scopes
                    .iter()
                    .filter(|s| match s.2 {
                        None => s.5 == user,
                        Some(t) => w.role(t, user).is_some(),
                    })
                    .map(|s| {
                        json!({"id": s.0, "kind": s.1, "team_id": s.2, "generation": s.3,
                                    "rotation_required": s.4})
                    })
                    .collect();
                ok(json!(v))
            }
            ("POST", ["scopes"]) => {
                let kind = s(&body, "kind");
                let team = uid(&body, "team_id");
                let id = uid(&body, "id").unwrap();
                if let Some(t) = team {
                    if !w.grants_of(t, user).has(Permission::MembersManage) {
                        return status(403);
                    }
                    if w.scopes.iter().any(|x| x.2 == Some(t) && x.1 == kind) {
                        return status(409);
                    }
                } else if w.scopes.iter().any(|x| x.2.is_none() && x.5 == user) {
                    return status(409);
                }
                let grants = body["grants"].as_array().cloned().unwrap_or_default();
                if !grants.iter().any(|g| uid(g, "user_id") == Some(user)) {
                    return status(400);
                }
                w.scopes.push((id, kind, team, 1, false, user));
                for g in grants {
                    let entry = (
                        id,
                        1,
                        uid(&g, "user_id").unwrap(),
                        s(&g, "wrapped_key"),
                        s(&g, "signature"),
                        user,
                    );
                    w.grants.push(entry);
                }
                ok(json!({"id": id}))
            }
            ("GET", ["keys"]) => {
                let v: Vec<Value> = w
                    .grants
                    .iter()
                    .filter(|g| g.2 == user)
                    .map(|g| {
                        json!({"scope_id": g.0, "generation": g.1, "wrapped_key": g.3,
                                    "signature": g.4, "granter_user_id": g.5})
                    })
                    .collect();
                ok(json!(v))
            }
            ("POST", ["grants"]) => {
                let scope = uid(&body, "scope_id").unwrap();
                let sc = w.scopes.iter().find(|x| x.0 == scope).cloned().unwrap();
                let allowed = sc.5 == user
                    || sc
                        .2
                        .is_some_and(|t| w.grants_of(t, user).has(Permission::MembersManage));
                if !allowed {
                    return status(403);
                }
                let g = (
                    scope,
                    body["generation"].as_u64().unwrap() as u32,
                    uid(&body, "user_id").unwrap(),
                    s(&body, "wrapped_key"),
                    s(&body, "signature"),
                    user,
                );
                w.grants.push(g);
                ok(json!({}))
            }
            ("GET", ["grants", "pending"]) => {
                let mut out = Vec::new();
                for sc in &w.scopes {
                    let Some(t) = sc.2 else { continue };
                    if !w.grants_of(t, user).has(Permission::MembersManage) {
                        continue;
                    }
                    for (m, _) in &w.teams[&t].1 {
                        let has = w
                            .grants
                            .iter()
                            .any(|g| g.0 == sc.0 && g.1 == sc.3 && g.2 == *m);
                        if !has
                            && w.entitled(&sc.1, t, *m)
                            && let Some((e, sg)) = w.users.get(m)
                        {
                            out.push(json!({"scope_id": sc.0, "generation": sc.3, "user_id": m,
                                            "enc_pub": e, "sign_pub": sg}));
                        }
                    }
                }
                ok(json!(out))
            }
            ("POST", ["scopes", id, "rotate"]) => {
                let id = Uuid::parse_str(id).unwrap();
                let gen_ = body["generation"].as_u64().unwrap() as u32;
                for g in body["grants"].as_array().unwrap() {
                    let entry = (
                        id,
                        gen_,
                        uid(g, "user_id").unwrap(),
                        s(g, "wrapped_key"),
                        s(g, "signature"),
                        user,
                    );
                    w.grants.push(entry);
                }
                let sc = w.scopes.iter_mut().find(|x| x.0 == id).unwrap();
                sc.3 = gen_;
                sc.4 = false;
                ok(json!({}))
            }
            ("GET", ["changes"]) => {
                let cursor: u64 = path
                    .split("cursor=")
                    .nth(1)
                    .and_then(|r| r.split('&').next())
                    .and_then(|c| c.parse().ok())
                    .unwrap_or(0);
                let mut items: Vec<&Item> = w
                    .items
                    .iter()
                    .filter(|i| i.seq > cursor && w.can_read(user, i))
                    .collect();
                items.sort_by_key(|i| i.seq);
                let next = w.seq;
                let v: Vec<Value> = items
                    .iter()
                    .map(|i| {
                        json!({"scope_id": i.scope, "collection": i.collection, "item_id": i.id,
                                    "version": i.version, "seq": i.seq, "author_user_id": i.author,
                                    "assignee_user_id": i.assignee, "deleted": i.deleted,
                                    "envelope": i.envelope})
                    })
                    .collect();
                ok(json!({"items": v, "cursor": next, "more": false}))
            }
            ("PUT", ["scopes", scope, "items", collection, id]) => {
                w.puts += 1;
                let scope = Uuid::parse_str(scope).unwrap();
                let id = Uuid::parse_str(id).unwrap();
                let base = body["base_version"].as_u64().unwrap();
                let current = w
                    .items
                    .iter()
                    .find(|i| i.scope == scope && i.id == id && i.collection == *collection)
                    .cloned();
                if current.as_ref().map_or(0, |c| c.version) != base {
                    return Ok((409, json!({"error": "version_conflict"})));
                }
                let deleting = body["deleted"].as_bool().unwrap_or(false);
                // The real server's write rules for team scopes (access.check_write).
                if let Some((kind, Some(team))) = w
                    .scopes
                    .iter()
                    .find(|x| x.0 == scope)
                    .map(|x| (x.1.clone(), x.2))
                {
                    let g = w.grants_of(team, user);
                    let allowed = match (kind.as_str(), &current) {
                        ("team_settings", _) => g.has(Permission::SettingsWrite),
                        ("team_tasks", None) => g.has(Permission::TasksCreate),
                        ("team_tasks", Some(c)) => {
                            g.can_see_task(c.assignee == Some(user))
                                && g.has(Permission::TasksUpdate)
                        }
                        _ => true,
                    };
                    if !allowed && !w.allow_any_write {
                        return status(403);
                    }
                }
                if let Some(team) = w
                    .scopes
                    .iter()
                    .find(|x| x.0 == scope && x.1 == "team_tasks")
                    .and_then(|x| x.2)
                    && deleting
                    && !w.allow_any_delete
                    && !w.grants_of(team, user).has(Permission::TasksDelete)
                {
                    return status(403);
                }
                w.seq += 1;
                let seq = w.seq;
                let item = Item {
                    scope,
                    collection: collection.to_string(),
                    id,
                    version: base + 1,
                    seq,
                    author: user,
                    assignee: uid(&body, "assignee_user_id"),
                    deleted: body["deleted"].as_bool().unwrap_or(false),
                    envelope: s(&body, "envelope"),
                };
                w.items
                    .retain(|i| !(i.scope == scope && i.id == id && i.collection == *collection));
                w.items.push(item);
                ok(json!({"version": base + 1, "seq": seq}))
            }
            ("GET", ["teams", t, "members"]) => {
                let t = Uuid::parse_str(t).unwrap();
                let v: Vec<Value> = w.teams[&t]
                    .1
                    .iter()
                    .map(|(u, r)| {
                        let (e, sg) = w.users.get(u).cloned().unwrap_or_default();
                        json!({"user_id": u, "email": format!("{u}@x"), "role": r,
                               "enc_pub": e, "sign_pub": sg})
                    })
                    .collect();
                ok(json!(v))
            }
            ("PUT", ["teams", t, "activity"]) => {
                let t = Uuid::parse_str(t).unwrap();
                let env = s(&body, "envelope");
                w.activity.insert((t, user, device), env);
                ok(json!({}))
            }
            ("GET", ["teams", t, "activity"]) => {
                let t = Uuid::parse_str(t).unwrap();
                let v: Vec<Value> = w
                    .activity
                    .iter()
                    .filter(|((tt, _, _), _)| *tt == t)
                    .map(|((_, u, d), e)| json!({"user_id": u, "device_id": d, "envelope": e}))
                    .collect();
                ok(json!(v))
            }
            _ => status(404),
        }
    }
}

struct Dev {
    session: Session,
    client: Client<Fake>,
    state: EngineState,
    settings: Settings,
    personal_tasks: Vec<Task>,
    team_tasks: HashMap<Uuid, Vec<Task>>,
    deletions: Vec<TaskDeletion>,
}

/// A user with a fresh account; returns what any of their devices needs to
/// unlock it (the bundle the server keeps, plus the AUK).
struct Account {
    id: Uuid,
    bundle: crypto::AccountBundle,
    auk: crypto::Key,
}

fn user(fake: &Fake) -> Account {
    let acct = crypto::create_account().unwrap();
    let id = Uuid::new_v4();
    fake.0.lock().unwrap().users.insert(
        id,
        (acct.bundle.enc_pub.clone(), acct.bundle.sign_pub.clone()),
    );
    Account {
        id,
        bundle: acct.bundle,
        auk: acct.auk,
    }
}

/// A signed-in device of `acct`, unlocked with the AUK.
fn device(fake: &Fake, acct: &Account) -> Dev {
    let token = format!("mxd_{}", Uuid::new_v4());
    let dev = Uuid::new_v4();
    fake.0
        .lock()
        .unwrap()
        .tokens
        .insert(token.clone(), (acct.id, dev));
    Dev {
        session: Session {
            user: acct.id,
            device: dev,
            keys: crypto::unlock(&acct.bundle, &acct.auk).unwrap(),
        },
        client: Client::new(fake.clone(), Some(token)),
        state: EngineState::default(),
        settings: Settings::default(),
        personal_tasks: Vec::new(),
        team_tasks: HashMap::new(),
        deletions: Vec::new(),
    }
}

fn cycle(d: &mut Dev, presence: &[(Uuid, Presence)]) -> muxel_cloud::engine::Output {
    let out = cycle_any(d, presence);
    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    out
}

/// [`cycle`] that leaves per-item errors to the caller.
fn cycle_any(d: &mut Dev, presence: &[(Uuid, Presence)]) -> muxel_cloud::engine::Output {
    let cloud = CloudSettings {
        enabled: true,
        ..Default::default()
    };
    let projects = vec![(Uuid::from_u128(77), "git:github.com/o/r".to_string())];
    let engine = Engine {
        client: &d.client,
        session: &d.session,
    };
    let input = Input {
        cloud: &cloud,
        settings: &d.settings,
        projects: &projects,
        personal_tasks: &d.personal_tasks,
        team_tasks: &d.team_tasks,
        team_setting_writes: &[],
        presence,
        task_deletions: &d.deletions,
        now_ms: 1_700_000_000_000,
    };
    let out = engine.cycle(&input, &mut d.state).expect("cycle");
    // Replay onto the live settings, as the app does on its UI thread.
    muxel_cloud::engine::apply_deltas(&mut d.settings, &out.settings_changes, &projects, &d.state);
    if let Some(t) = out.personal_tasks.clone() {
        d.personal_tasks = t;
    }
    for (team, list) in &out.team_tasks {
        d.team_tasks.insert(*team, list.clone());
    }
    d.deletions.retain(|x| !out.deletions_done.contains(&x.id));
    out
}

/// Delete a task on `d` the way the app does: drop it and queue a tombstone.
fn delete_task(d: &mut Dev, team: Option<Uuid>, id: Uuid) {
    let list = match team {
        None => &mut d.personal_tasks,
        Some(t) => d.team_tasks.get_mut(&t).unwrap(),
    };
    let i = list.iter().position(|t| t.id == id).unwrap();
    let task = list.remove(i);
    d.deletions.push(TaskDeletion {
        team,
        id,
        assignee: task.assignee,
    });
}

#[test]
fn a_personal_task_deleted_on_one_device_is_deleted_on_the_other() {
    let fake = Fake::default();
    let acct = user(&fake);
    let mut a = device(&fake, &acct);
    let mut b = device(&fake, &acct);
    let keep = Task::new("keep", Some(acct.id), 1);
    let drop = Task::new("drop", Some(acct.id), 2);
    a.personal_tasks = vec![keep.clone(), drop.clone()];
    cycle(&mut a, &[]);
    cycle(&mut b, &[]);
    assert_eq!(b.personal_tasks.len(), 2);

    delete_task(&mut a, None, drop.id);
    let out = cycle(&mut a, &[]);
    assert_eq!(out.deletions_done, vec![drop.id]);
    assert!(a.deletions.is_empty());
    cycle(&mut b, &[]);
    let left: Vec<_> = b.personal_tasks.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(left, vec!["keep"]);
    assert!(
        fake.0
            .lock()
            .unwrap()
            .items
            .iter()
            .any(|i| i.id == drop.id && i.deleted),
        "the server keeps only a tombstone"
    );

    // A task that never reached the server just settles.
    let local = Task::new("never synced", Some(acct.id), 3);
    b.personal_tasks.push(local.clone());
    delete_task(&mut b, None, local.id);
    let out = cycle(&mut b, &[]);
    assert_eq!(out.deletions_done, vec![local.id]);
}

#[test]
fn team_task_deletion_follows_roles_and_reaches_the_assignee() {
    let fake = Fake::default();
    let (oa, ma, aa) = (user(&fake), user(&fake), user(&fake));
    let (owner, member, assignee) = (oa.id, ma.id, aa.id);
    let team = Uuid::new_v4();
    fake.0.lock().unwrap().teams.insert(
        team,
        (
            "Acme".into(),
            vec![
                (owner, "owner".into()),
                (member, "member".into()),
                (assignee, "assignee".into()),
            ],
        ),
    );
    let mut o = device(&fake, &oa);
    let mut m = device(&fake, &ma);
    let mut a = device(&fake, &aa);
    cycle(&mut o, &[]);
    let mut theirs = Task::new("for the assignee", Some(owner), 1);
    theirs.assignee = Some(assignee);
    let open = Task::new("unassigned", Some(owner), 2);
    o.team_tasks
        .insert(team, vec![theirs.clone(), open.clone()]);
    cycle(&mut o, &[]);
    cycle(&mut m, &[]);
    cycle(&mut a, &[]);
    assert_eq!(m.team_tasks[&team].len(), 2);
    assert_eq!(a.team_tasks[&team].len(), 1);

    // A Member may not delete: the server refuses, and the task survives elsewhere.
    delete_task(&mut m, Some(team), open.id);
    let out = cycle_any(&mut m, &[]);
    assert!(
        out.errors.iter().any(|e| e.contains("refused")),
        "{:?}",
        out.errors
    );
    assert!(
        m.deletions.is_empty(),
        "a refused deletion isn't retried forever"
    );
    cycle(&mut m, &[]);
    assert!(
        m.team_tasks[&team].iter().any(|t| t.id == open.id),
        "and the task comes back"
    );
    cycle(&mut o, &[]);
    assert!(o.team_tasks[&team].iter().any(|t| t.id == open.id));

    // A task merely missing locally (hidden by a role change, say) is never tombstoned.
    o.team_tasks
        .get_mut(&team)
        .unwrap()
        .retain(|t| t.id != open.id);
    cycle(&mut o, &[]);
    cycle(&mut m, &[]);
    assert!(m.team_tasks[&team].iter().any(|t| t.id == open.id));

    // The owner deletes the assignee's task; everyone, the assignee included, loses it.
    delete_task(&mut o, Some(team), theirs.id);
    cycle(&mut o, &[]);
    cycle(&mut m, &[]);
    cycle(&mut a, &[]);
    assert!(!m.team_tasks[&team].iter().any(|t| t.id == theirs.id));
    assert!(
        a.team_tasks[&team].is_empty(),
        "the assignee gets the tombstone"
    );
}

#[test]
fn a_task_deletion_by_a_member_who_may_not_delete_is_rejected_on_pull() {
    let fake = Fake::default();
    let (oa, ma) = (user(&fake), user(&fake));
    let team = Uuid::new_v4();
    fake.0.lock().unwrap().teams.insert(
        team,
        (
            "Acme".into(),
            vec![(oa.id, "owner".into()), (ma.id, "member".into())],
        ),
    );
    let mut o = device(&fake, &oa);
    let mut m = device(&fake, &ma);
    cycle(&mut o, &[]);
    let t = Task::new("keep me", Some(oa.id), 1);
    o.team_tasks.insert(team, vec![t.clone()]);
    cycle(&mut o, &[]);
    cycle(&mut m, &[]);

    fake.0.lock().unwrap().allow_any_delete = true;
    delete_task(&mut m, Some(team), t.id);
    cycle(&mut m, &[]);
    let out = cycle_any(&mut o, &[]);
    assert!(
        out.errors.iter().any(|e| e.contains("may not delete")),
        "{:?}",
        out.errors
    );
    assert!(o.team_tasks[&team].iter().any(|x| x.id == t.id), "kept");
}

#[test]
fn export_decrypts_everything_the_account_can_read() {
    let fake = Fake::default();
    let acct = user(&fake);
    let mut a = device(&fake, &acct);
    a.personal_tasks = vec![Task::new("exported", Some(acct.id), 1)];
    cycle(&mut a, &[]);
    let engine = Engine {
        client: &a.client,
        session: &a.session,
    };
    let export = engine.export(42).expect("export");
    assert_eq!(export.skipped, 0);
    assert!(export.items >= 1);
    assert_eq!(export.json["format"], "muxel-cloud-export");
    assert_eq!(export.json["exported_at_ms"], 42);
    assert_eq!(export.json["personal"]["tasks"][0]["title"], "exported");
    // Nothing in the export is ciphertext.
    assert!(!export.json.to_string().contains("envelope"));
}

#[test]
fn two_devices_converge_and_server_sees_only_ciphertext() {
    let fake = Fake::default();
    let acct = user(&fake);
    let u = acct.id;
    let mut a = device(&fake, &acct);
    let mut b = device(&fake, &acct);

    a.settings.theme = "Nord".into();
    a.settings.snippets[0].text = "keep going".into();
    let mut l = muxel_core::Loop::new("nightly", Uuid::from_u128(77));
    l.prompt = "run the nightly checks".into();
    a.settings.loops.push(l.clone());
    let mut t = Task::new("write docs", Some(u), 1);
    t.add_note(ProgressNote {
        at_ms: 5,
        by_user: Some(u),
        by_agent: None,
        text: "outline done".into(),
        percent: Some(20),
    });
    a.personal_tasks.push(t.clone());
    cycle(&mut a, &[]);
    cycle(&mut b, &[]);

    assert_eq!(b.settings.theme, "Nord");
    // Identical seeded snippets are adopted; A's customized "Continue" arrives
    // beside B's untouched one rather than replacing it.
    assert!(b.settings.snippets.iter().any(|s| s.text == "keep going"));
    assert_eq!(b.settings.snippets.len(), a.settings.snippets.len() + 1);
    assert_eq!(
        b.settings
            .snippets
            .iter()
            .filter(|s| s.name == "Yes")
            .count(),
        1
    );
    let pulled = b
        .settings
        .loops
        .iter()
        .find(|x| x.id == l.id)
        .expect("loop synced");
    assert!(!pulled.enabled, "a pulled loop arrives off");
    assert_eq!(pulled.project_id, Uuid::from_u128(77));
    assert_eq!(b.personal_tasks[0].progress[0].text, "outline done");

    // B edits A's snippet; A picks it up; nothing bounces back.
    let shared = a.settings.snippets[0].id;
    b.settings
        .snippets
        .iter_mut()
        .find(|s| s.id == shared)
        .unwrap()
        .text = "edited on B".into();
    cycle(&mut b, &[]);
    let out = cycle(&mut a, &[]);
    assert_eq!(a.settings.snippets[0].text, "edited on B");
    assert!(!out.settings_changes.is_empty());
    let seq_before = fake.0.lock().unwrap().seq;
    cycle(&mut a, &[]);
    cycle(&mut b, &[]);
    assert_eq!(
        fake.0.lock().unwrap().seq,
        seq_before,
        "steady state writes nothing"
    );

    // The server holds nothing readable.
    let w = fake.0.lock().unwrap();
    for i in &w.items {
        let raw = String::from_utf8(B64.decode(&i.envelope).unwrap()).unwrap();
        for leak in ["Nord", "keep going", "nightly", "write docs", "outline"] {
            assert!(!raw.contains(leak), "{leak} visible to the server");
        }
    }
}

#[test]
fn deleted_on_one_device_is_deleted_on_the_other() {
    let fake = Fake::default();
    let acct = user(&fake);
    let mut a = device(&fake, &acct);
    let mut b = device(&fake, &acct);
    cycle(&mut a, &[]);
    cycle(&mut b, &[]);
    let gone = a.settings.snippets.remove(0).id;
    cycle(&mut a, &[]);
    cycle(&mut b, &[]);
    assert!(!b.settings.snippets.iter().any(|s| s.id == gone));
}

#[test]
fn team_tasks_roles_grants_and_activity() {
    let fake = Fake::default();
    let (oa, ma, aa, sa) = (user(&fake), user(&fake), user(&fake), user(&fake));
    let (owner, member, assignee, settings_only) = (oa.id, ma.id, aa.id, sa.id);
    let team = Uuid::new_v4();
    fake.0.lock().unwrap().teams.insert(
        team,
        (
            "Acme".into(),
            vec![
                (owner, "owner".into()),
                (member, "member".into()),
                (assignee, "assignee".into()),
                (settings_only, "settings_only".into()),
            ],
        ),
    );
    let mut o = device(&fake, &oa);
    let mut m = device(&fake, &ma);
    let mut a = device(&fake, &aa);
    let mut s = device(&fake, &sa);

    // The owner's first cycle creates the team scopes and grants keys.
    let out = cycle(&mut o, &[]);
    assert!(!out.teams[0].waiting_for_keys);

    let mut for_a = Task::new("fix login", Some(owner), 1);
    for_a.assignee = Some(assignee);
    let backlog = Task::new("backlog item", Some(owner), 2);
    o.team_tasks
        .insert(team, vec![for_a.clone(), backlog.clone()]);
    cycle(&mut o, &[]);

    cycle(&mut m, &[]);
    assert_eq!(m.team_tasks[&team].len(), 2, "member sees all tasks");
    cycle(&mut a, &[]);
    let seen: Vec<_> = a.team_tasks[&team]
        .iter()
        .map(|t| t.title.clone())
        .collect();
    assert_eq!(seen, vec!["fix login"], "assignee sees only their task");
    let out = cycle(&mut s, &[]);
    assert!(
        s.team_tasks.get(&team).is_none_or(|t| t.is_empty()),
        "settings-only sees no tasks"
    );
    assert!(!out.teams[0].waiting_for_keys);

    // The assignee reports progress; the owner and member see it.
    let t = a.team_tasks.get_mut(&team).unwrap().first_mut().unwrap();
    t.add_note(ProgressNote {
        at_ms: 10,
        by_user: Some(assignee),
        by_agent: Some("claude".into()),
        text: "tests pass".into(),
        percent: Some(80),
    });
    cycle(&mut a, &[]);
    cycle(&mut o, &[]);
    let got = o.team_tasks[&team]
        .iter()
        .find(|t| t.id == for_a.id)
        .unwrap();
    assert_eq!(got.percent, Some(80));
    assert_eq!(got.progress.last().unwrap().text, "tests pass");

    // Activity: the member publishes, the owner reads it; the assignee can't.
    let presence = Presence {
        at_ms: 1,
        device_name: "m-laptop".into(),
        projects: vec![ProjectActivity {
            project_key: "git:github.com/o/r".into(),
            project_name: "r".into(),
            agents: vec![AgentActivity {
                program: "codex".into(),
                status: "working".into(),
                ..Default::default()
            }],
        }],
    };
    cycle(&mut m, &[(team, presence.clone())]);
    let out = cycle(&mut o, &[]);
    let act = &out.teams[0].activity;
    assert_eq!(act.len(), 1);
    assert_eq!(act[0].presence.projects[0].agents[0].program, "codex");
    let out = cycle(&mut a, &[]);
    assert!(out.teams[0].activity.is_empty());
}

#[test]
fn late_joiner_gets_keys_from_an_online_admin() {
    let fake = Fake::default();
    let oa = user(&fake);
    let owner = oa.id;
    let team = Uuid::new_v4();
    fake.0
        .lock()
        .unwrap()
        .teams
        .insert(team, ("T".into(), vec![(owner, "owner".into())]));
    let mut o = device(&fake, &oa);
    cycle(&mut o, &[]);
    o.team_tasks
        .insert(team, vec![Task::new("existing", Some(owner), 1)]);
    cycle(&mut o, &[]);

    let la = user(&fake);
    fake.0
        .lock()
        .unwrap()
        .teams
        .get_mut(&team)
        .unwrap()
        .1
        .push((la.id, "member".into()));
    let mut l = device(&fake, &la);
    let out = cycle(&mut l, &[]);
    assert!(out.teams[0].waiting_for_keys, "no admin online yet");

    let out = cycle(&mut o, &[]);
    assert!(out.notes.iter().any(|n| n.starts_with("Shared team keys")));
    // Keys arrive; items skipped before (no key yet) are fetched again.
    let out = cycle(&mut l, &[]);
    assert!(!out.teams[0].waiting_for_keys);
    assert_eq!(l.team_tasks[&team][0].title, "existing");
}

#[test]
fn tampered_or_unauthorized_items_are_rejected() {
    let fake = Fake::default();
    let (oa, ma) = (user(&fake), user(&fake));
    let (owner, member) = (oa.id, ma.id);
    let team = Uuid::new_v4();
    fake.0.lock().unwrap().teams.insert(
        team,
        (
            "T".into(),
            vec![(owner, "owner".into()), (member, "member".into())],
        ),
    );
    let mut o = device(&fake, &oa);
    let mut m = device(&fake, &ma);
    cycle(&mut o, &[]);
    cycle(&mut m, &[]);
    // A member may not write team settings (no settings.write): their loop
    // must be ignored by others even if the server accepted it.
    let w = muxel_cloud::engine::TeamSettingWrite {
        team,
        collection: muxel_core::cloud::items::Collection::Loops,
        id: Uuid::new_v4(),
        data: Some(json!({"name": "evil", "prompt": "rm -rf ~"})),
    };
    let cloud = CloudSettings::default();
    let input = Input {
        cloud: &cloud,
        settings: &m.settings,
        projects: &[],
        personal_tasks: &[],
        team_tasks: &HashMap::new(),
        team_setting_writes: std::slice::from_ref(&w),
        presence: &[],
        task_deletions: &[],
        now_ms: 1,
    };
    let engine = Engine {
        client: &m.client,
        session: &m.session,
    };
    fake.0.lock().unwrap().allow_any_write = true;
    let _ = engine.cycle(&input, &mut m.state).unwrap();
    fake.0.lock().unwrap().allow_any_write = false;
    let cloud_on = CloudSettings {
        enabled: true,
        ..Default::default()
    };
    let engine = Engine {
        client: &o.client,
        session: &o.session,
    };
    let input = Input {
        cloud: &cloud_on,
        settings: &o.settings,
        projects: &[],
        personal_tasks: &[],
        team_tasks: &HashMap::new(),
        team_setting_writes: &[],
        presence: &[],
        task_deletions: &[],
        now_ms: 1,
    };
    let out = engine.cycle(&input, &mut o.state).unwrap();
    assert!(
        out.teams[0].items.get("loops").is_none_or(|v| v.is_empty()),
        "unauthorized loop was accepted"
    );
    assert!(out.errors.iter().any(|e| e.contains("may not write")));

    // The server swapping an envelope between collections is caught.
    {
        let mut w = fake.0.lock().unwrap();
        let it = w
            .items
            .iter_mut()
            .find(|i| i.collection == "snippets")
            .unwrap();
        it.collection = "runners".into();
        it.seq += 1000;
        w.seq += 1000;
    }
    let out = engine.cycle(&input, &mut o.state).unwrap();
    assert!(
        out.errors.iter().any(|e| e.contains("runners")),
        "{:?}",
        out.errors
    );
    let _ = ApiError::NotFound;
}

#[test]
fn two_first_sign_ins_race_for_the_personal_scope() {
    let fake = Fake::default();
    let acct = user(&fake);
    let mut a = device(&fake, &acct);
    let mut b = device(&fake, &acct);
    // Sequential here: B finds A's scope and must not create a second one.
    // (A true simultaneous create gets 409 from the server and reloads.)
    cycle(&mut a, &[]);
    cycle(&mut b, &[]);
    let w = fake.0.lock().unwrap();
    assert_eq!(w.scopes.iter().filter(|s| s.2.is_none()).count(), 1);
}

#[test]
fn an_edit_made_during_a_cycle_is_not_overwritten() {
    let fake = Fake::default();
    let acct = user(&fake);
    let mut a = device(&fake, &acct);
    let mut b = device(&fake, &acct);
    cycle(&mut a, &[]);
    cycle(&mut b, &[]);
    a.settings.snippets[0].text = "from A".into();
    cycle(&mut a, &[]);

    // B runs a cycle on a snapshot; meanwhile the user edits the same snippet.
    let cloud = CloudSettings {
        enabled: true,
        ..Default::default()
    };
    let snapshot = b.settings.clone();
    let engine = Engine {
        client: &b.client,
        session: &b.session,
    };
    let input = Input {
        cloud: &cloud,
        settings: &snapshot,
        projects: &[],
        personal_tasks: &[],
        team_tasks: &HashMap::new(),
        team_setting_writes: &[],
        presence: &[],
        task_deletions: &[],
        now_ms: 1_700_000_000_000,
    };
    let out = engine.cycle(&input, &mut b.state).unwrap();
    b.settings.snippets[0].text = "typed on B mid-cycle".into();
    muxel_cloud::engine::apply_deltas(&mut b.settings, &out.settings_changes, &[], &b.state);
    assert_eq!(b.settings.snippets[0].text, "typed on B mid-cycle");
    // …and B's edit wins everywhere on the next round.
    cycle(&mut b, &[]);
    cycle(&mut a, &[]);
    assert_eq!(a.settings.snippets[0].text, "typed on B mid-cycle");
}

/// A cycle that also shares `writes` with the team.
fn cycle_writing(
    d: &mut Dev,
    writes: &[muxel_cloud::engine::TeamSettingWrite],
) -> muxel_cloud::engine::Output {
    let cloud = CloudSettings {
        enabled: true,
        ..Default::default()
    };
    let engine = Engine {
        client: &d.client,
        session: &d.session,
    };
    let input = Input {
        cloud: &cloud,
        settings: &d.settings,
        projects: &[],
        personal_tasks: &d.personal_tasks,
        team_tasks: &d.team_tasks,
        team_setting_writes: writes,
        presence: &[],
        task_deletions: &[],
        now_ms: 1_700_000_000_000,
    };
    let out = engine.cycle(&input, &mut d.state).expect("cycle");
    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    out
}

#[test]
fn a_downgraded_member_gets_no_key_for_new_team_settings() {
    let fake = Fake::default();
    let (oa, ma) = (user(&fake), user(&fake));
    let (owner, member) = (oa.id, ma.id);
    let team = Uuid::new_v4();
    fake.0.lock().unwrap().teams.insert(
        team,
        (
            "T".into(),
            vec![(owner, "owner".into()), (member, "member".into())],
        ),
    );
    let mut o = device(&fake, &oa);
    let mut m = device(&fake, &ma);
    cycle(&mut o, &[]);
    cycle(&mut m, &[]);
    cycle(&mut o, &[]); // grants the member's keys
    let share = |name: &str| muxel_cloud::engine::TeamSettingWrite {
        team,
        collection: muxel_core::cloud::items::Collection::Snippets,
        id: Uuid::new_v4(),
        data: Some(json!({"name": name, "text": "echo hi"})),
    };
    let first = share("before");
    cycle_writing(&mut o, std::slice::from_ref(&first));
    let out = cycle(&mut m, &[]);
    assert_eq!(out.teams[0].items["snippets"].len(), 1, "member reads it");

    // Downgraded to Assignee: the server flags the settings scope for rotation.
    let settings_scope = {
        let mut w = fake.0.lock().unwrap();
        w.teams.get_mut(&team).unwrap().1[1].1 = "assignee".into();
        let sc = w
            .scopes
            .iter_mut()
            .find(|s| s.1 == "team_settings" && s.2 == Some(team))
            .unwrap();
        sc.4 = true;
        sc.0
    };
    let out = cycle(&mut o, &[]);
    assert!(
        out.notes
            .iter()
            .any(|n| n.starts_with("Rotated a team key")),
        "{:?}",
        out.notes
    );
    let second = share("after");
    cycle_writing(&mut o, std::slice::from_ref(&second));

    let w = fake.0.lock().unwrap();
    let item = w.items.iter().find(|i| i.id == second.id).unwrap();
    let env: Value = serde_json::from_slice(&B64.decode(&item.envelope).unwrap()).unwrap();
    assert_eq!(env["generation"], 2, "new items use the new key");
    assert!(
        !w.grants
            .iter()
            .any(|g| g.0 == settings_scope && g.1 == 2 && g.2 == member),
        "the downgraded member has no key for generation 2"
    );
    assert!(
        w.grants
            .iter()
            .any(|g| g.0 == settings_scope && g.1 == 2 && g.2 == owner)
    );
    drop(w);
    let out = cycle(&mut o, &[]);
    assert_eq!(out.teams[0].items["snippets"].len(), 2, "owner reads both");
    let out = cycle(&mut m, &[]);
    assert!(
        out.teams[0]
            .items
            .get("snippets")
            .is_none_or(|v| v.is_empty()),
        "an Assignee no longer sees team settings: {:?}",
        out.teams[0].items.get("snippets")
    );
}

#[test]
fn turning_sharing_off_retracts_activity() {
    let fake = Fake::default();
    let (oa, ma) = (user(&fake), user(&fake));
    let team = Uuid::new_v4();
    fake.0.lock().unwrap().teams.insert(
        team,
        (
            "T".into(),
            vec![(oa.id, "owner".into()), (ma.id, "member".into())],
        ),
    );
    let mut o = device(&fake, &oa);
    let mut m = device(&fake, &ma);
    cycle(&mut o, &[]);
    cycle(&mut m, &[]);
    cycle(&mut o, &[]);
    let presence = Presence {
        at_ms: 1,
        device_name: "m".into(),
        projects: vec![ProjectActivity {
            project_key: "name:r".into(),
            project_name: "r".into(),
            agents: vec![AgentActivity {
                program: "claude".into(),
                status: "working".into(),
                ..Default::default()
            }],
        }],
    };
    cycle(&mut m, &[(team, presence)]);
    let out = cycle(&mut o, &[]);
    assert_eq!(out.teams[0].activity[0].presence.projects.len(), 1);
    // The member turns sharing off: the next cycle publishes nothing for the
    // team, so it retracts what it published.
    cycle(&mut m, &[]);
    let out = cycle(&mut o, &[]);
    assert!(
        out.teams[0]
            .activity
            .iter()
            .all(|a| a.presence.projects.is_empty()),
        "{:?}",
        out.teams[0].activity
    );
    assert!(m.state.published_presence.is_empty());
}

#[test]
fn team_data_outlives_the_member_who_wrote_and_granted_it() {
    let fake = Fake::default();
    let (oa, aa, ma) = (user(&fake), user(&fake), user(&fake));
    let (founder, admin, member) = (oa.id, aa.id, ma.id);
    let team = Uuid::new_v4();
    fake.0.lock().unwrap().teams.insert(
        team,
        (
            "T".into(),
            vec![
                (founder, "owner".into()),
                (admin, "admin".into()),
                (member, "member".into()),
            ],
        ),
    );
    let mut o = device(&fake, &oa);
    let mut a = device(&fake, &aa);
    let mut m = device(&fake, &ma);
    cycle(&mut o, &[]); // creates the scopes
    cycle(&mut a, &[]);
    cycle(&mut m, &[]);
    cycle(&mut o, &[]); // grants everyone's keys, signed by the founder
    let snippet = share_snippet(team, "deploy");
    cycle_writing(&mut o, std::slice::from_ref(&snippet));
    let task = Task::new("ship it", Some(founder), 1);
    o.team_tasks.insert(team, vec![task.clone()]);
    cycle(&mut o, &[]);
    cycle(&mut a, &[]);
    let out = cycle(&mut m, &[]);
    assert_eq!(out.teams[0].items["snippets"].len(), 1, "member reads it");

    // The founder hands the team to the admin and leaves; the server flags
    // every scope for rotation.
    {
        let mut w = fake.0.lock().unwrap();
        let roster = &mut w.teams.get_mut(&team).unwrap().1;
        roster.retain(|(u, _)| *u != founder);
        roster.iter_mut().find(|(u, _)| *u == admin).unwrap().1 = "owner".into();
        for s in w.scopes.iter_mut().filter(|s| s.2 == Some(team)) {
            s.4 = true;
        }
    }
    let out = cycle_any(&mut a, &[]); // rotates, then re-seals what it verified
    assert!(
        out.notes
            .iter()
            .any(|n| n == "Re-sealed 2 team items under the current key"),
        "{:?}",
        out.notes
    );
    {
        let w = fake.0.lock().unwrap();
        for id in [snippet.id, task.id] {
            let item = w.items.iter().find(|i| i.id == id).unwrap();
            assert_eq!(item.author, admin, "re-signed by the new owner");
            let env: Value = serde_json::from_slice(&B64.decode(&item.envelope).unwrap()).unwrap();
            assert_eq!(env["generation"], 2, "under the new key");
        }
    }
    // Nothing left to re-seal: the next cycle is quiet.
    let out = cycle(&mut a, &[]);
    assert!(!out.notes.iter().any(|n| n.starts_with("Re-sealed")));

    // The member's existing device and a brand-new one both read everything,
    // without errors about the departed founder.
    let out = cycle(&mut m, &[]);
    assert_eq!(out.teams[0].items["snippets"].len(), 1);
    let mut m2 = device(&fake, &ma);
    let out = cycle(&mut m2, &[]);
    assert_eq!(
        out.teams[0].items["snippets"].len(),
        1,
        "the snippet survives"
    );
    assert_eq!(m2.team_tasks[&team][0].title, "ship it", "so does the task");
}

#[test]
fn a_demoted_writers_items_are_re_signed_without_a_rotation() {
    let fake = Fake::default();
    let (oa, ma) = (user(&fake), user(&fake));
    let (owner, member) = (oa.id, ma.id);
    let team = Uuid::new_v4();
    fake.0.lock().unwrap().teams.insert(
        team,
        (
            "T".into(),
            vec![(owner, "owner".into()), (member, "manager".into())],
        ),
    );
    let mut o = device(&fake, &oa);
    let mut m = device(&fake, &ma);
    cycle(&mut o, &[]);
    cycle(&mut m, &[]);
    cycle(&mut o, &[]);
    let snippet = share_snippet(team, "by the member");
    cycle_writing(&mut m, std::slice::from_ref(&snippet));
    cycle(&mut o, &[]); // the owner verifies it while the member may still write

    // Down to Member: still reads settings (so nothing rotates) but may no
    // longer write them.
    fake.0.lock().unwrap().teams.get_mut(&team).unwrap().1[1].1 = "member".into();
    let out = cycle(&mut o, &[]);
    assert!(
        out.notes
            .iter()
            .any(|n| n == "Re-sealed 1 team item under the current key"),
        "{:?}",
        out.notes
    );
    let w = fake.0.lock().unwrap();
    assert_eq!(
        w.items.iter().find(|i| i.id == snippet.id).unwrap().author,
        owner
    );
    drop(w);
    let out = cycle(&mut device(&fake, &oa), &[]);
    assert_eq!(
        out.teams[0].items["snippets"].len(),
        1,
        "a new device reads it"
    );
}

#[test]
fn a_late_joiner_after_a_rotation_reads_items_from_before_it() {
    let fake = Fake::default();
    let (oa, ma) = (user(&fake), user(&fake));
    let (owner, member) = (oa.id, ma.id);
    let team = Uuid::new_v4();
    fake.0.lock().unwrap().teams.insert(
        team,
        (
            "T".into(),
            vec![(owner, "owner".into()), (member, "member".into())],
        ),
    );
    let mut o = device(&fake, &oa);
    cycle(&mut o, &[]);
    let old = share_snippet(team, "from generation 1");
    cycle_writing(&mut o, std::slice::from_ref(&old));

    // The member leaves; the owner rotates and re-seals.
    {
        let mut w = fake.0.lock().unwrap();
        w.teams
            .get_mut(&team)
            .unwrap()
            .1
            .retain(|(u, _)| *u != member);
        for s in w.scopes.iter_mut().filter(|s| s.2 == Some(team)) {
            s.4 = true;
        }
    }
    cycle(&mut o, &[]);

    // Someone joins and only ever gets generation 2.
    let la = user(&fake);
    fake.0
        .lock()
        .unwrap()
        .teams
        .get_mut(&team)
        .unwrap()
        .1
        .push((la.id, "member".into()));
    let mut l = device(&fake, &la);
    cycle(&mut l, &[]);
    cycle(&mut o, &[]); // grants the joiner's keys
    let out = cycle(&mut l, &[]);
    assert_eq!(
        out.teams[0].items["snippets"].len(),
        1,
        "the older item is readable"
    );
}

#[test]
fn a_refused_re_seal_is_not_retried_every_cycle() {
    let fake = Fake::default();
    let (oa, aa, xa) = (user(&fake), user(&fake), user(&fake));
    let (owner, assignee, leaver) = (oa.id, aa.id, xa.id);
    let team = Uuid::new_v4();
    fake.0.lock().unwrap().teams.insert(
        team,
        (
            "T".into(),
            vec![
                (owner, "owner".into()),
                (assignee, "assignee".into()),
                (leaver, "member".into()),
            ],
        ),
    );
    let mut o = device(&fake, &oa);
    let mut a = device(&fake, &aa);
    cycle(&mut o, &[]);
    cycle(&mut a, &[]);
    cycle(&mut o, &[]);
    let mut task = Task::new("theirs for now", Some(owner), 1);
    task.assignee = Some(assignee);
    o.team_tasks.insert(team, vec![task.clone()]);
    cycle(&mut o, &[]);
    cycle(&mut a, &[]);
    assert_eq!(a.team_tasks[&team].len(), 1);

    // Reassigned away: the assignee's copy freezes at a version they can no
    // longer see. Then someone leaves and the keys rotate.
    o.team_tasks.get_mut(&team).unwrap()[0].assignee = Some(owner);
    cycle(&mut o, &[]);
    {
        let mut w = fake.0.lock().unwrap();
        w.teams
            .get_mut(&team)
            .unwrap()
            .1
            .retain(|(u, _)| *u != leaver);
        for s in w.scopes.iter_mut().filter(|s| s.2 == Some(team)) {
            s.4 = true;
        }
    }
    cycle(&mut o, &[]); // rotates, and re-seals the task it can see

    let puts = |f: &Fake| f.0.lock().unwrap().puts;
    let before = puts(&fake);
    cycle(&mut a, &[]);
    assert_eq!(
        puts(&fake),
        before + 1,
        "tries to re-seal its stale copy once"
    );
    let before = puts(&fake);
    cycle(&mut a, &[]);
    cycle(&mut a, &[]);
    assert_eq!(puts(&fake), before, "and doesn't ask again");
}

fn share_snippet(team: Uuid, name: &str) -> muxel_cloud::engine::TeamSettingWrite {
    muxel_cloud::engine::TeamSettingWrite {
        team,
        collection: muxel_core::cloud::items::Collection::Snippets,
        id: Uuid::new_v4(),
        data: Some(json!({"name": name, "text": "make deploy"})),
    }
}
