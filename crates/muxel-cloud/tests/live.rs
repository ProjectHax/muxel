//! The engine against a real server implementing the protocol. Ignored by
//! default; run with a server and device tokens:
//!
//! ```sh
//! MUXEL_CLOUD_LIVE='{"url":"http://localhost:8000","owner_a":"mxd_…","owner_b":"mxd_…",
//!                   "member":"mxd_…","assignee":"mxd_…"}' \
//!   cargo test -p muxel-cloud --test live -- --ignored --nocapture
//! ```
//!
//! `owner_a`/`owner_b` are two devices of one user who owns a paid team that
//! `member` and `assignee` belong to (with those roles). Accounts must be fresh.

use std::collections::HashMap;

use muxel_cloud::account;
use muxel_cloud::api::{Client, Http};
use muxel_cloud::crypto;
use muxel_cloud::engine::{Engine, EngineState, Input, Output, Session, apply_deltas};
use muxel_core::Settings;
use muxel_core::cloud::CloudSettings;
use muxel_core::cloud::activity::{AgentActivity, Presence, ProjectActivity};
use muxel_core::cloud::tasks::{ProgressNote, Task};
use serde_json::Value;
use uuid::Uuid;

struct Dev {
    session: Session,
    client: Client<Http>,
    state: EngineState,
    settings: Settings,
    personal: Vec<Task>,
    team: HashMap<Uuid, Vec<Task>>,
}

/// Sign in a device the way muxel does: `setup` with whatever AUK this device
/// has; a second device of an account unlocks with the Recovery Key.
fn dev(url: &str, token: &str, recovery: Option<&str>) -> (Dev, Option<String>) {
    let client = Client::new(Http::new(url), Some(token.to_string()));
    let (account, auk, shown) = match account::setup(&client, None).expect("setup") {
        account::Setup::Ready {
            account,
            auk,
            created,
        } => (account, auk, created.map(|rk| rk.display())),
        account::Setup::NeedsRecovery { account } => {
            let typed = recovery.expect("an existing account needs its Recovery Key");
            assert!(account::recover(&account, "MXL-0000-0000-0000-0000-0000-0000-00").is_err());
            let auk = account::recover(&account, typed).expect("recover");
            (account, auk, None)
        }
    };
    // A new Recovery Key replaces the old one.
    if let Some(old) = &shown {
        let (rk, bundle) = account::new_recovery_key(&client, &account, &auk).expect("rotate");
        let fetched = client.account_keys().unwrap().unwrap();
        assert_eq!(fetched, bundle);
        let rotated = account::AccountFile {
            bundle,
            ..account.clone()
        };
        assert!(account::recover(&rotated, old).is_err());
        account::recover(&rotated, &rk.display()).expect("new key opens it");
        return finish(client, rotated, auk, Some(rk.display()));
    }
    finish(client, account, auk, shown)
}

fn finish(
    client: Client<Http>,
    account: account::AccountFile,
    auk: muxel_cloud::crypto::Key,
    shown: Option<String>,
) -> (Dev, Option<String>) {
    // With the AUK stored, setup unlocks straight away.
    let again = account::setup(&client, account::decode_auk(&account::encode_auk(&auk)));
    assert!(matches!(
        again,
        Ok(account::Setup::Ready { created: None, .. })
    ));
    let d = Dev {
        session: account::session(&account, &auk).unwrap(),
        client,
        state: EngineState::default(),
        settings: Settings::default(),
        personal: Vec::new(),
        team: HashMap::new(),
    };
    (d, shown)
}

fn cycle(d: &mut Dev, presence: &[(Uuid, Presence)]) -> Output {
    let cloud = CloudSettings {
        enabled: true,
        ..Default::default()
    };
    let engine = Engine {
        client: &d.client,
        session: &d.session,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let input = Input {
        cloud: &cloud,
        settings: &d.settings,
        projects: &[],
        personal_tasks: &d.personal,
        team_tasks: &d.team,
        team_setting_writes: &[],
        member_decisions: &[],
        presence,
        task_deletions: &[],
        now_ms: now,
    };
    let out = engine.cycle(&input, &mut d.state).expect("cycle");
    apply_deltas(&mut d.settings, &out.settings_changes, &[], &d.state);
    if let Some(t) = &out.personal_tasks {
        d.personal = t.clone();
    }
    for (k, v) in &out.team_tasks {
        d.team.insert(*k, v.clone());
    }
    assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
    out
}

#[test]
#[ignore = "needs a live server; see the module docs"]
fn against_a_live_server() {
    let Ok(cfg) = std::env::var("MUXEL_CLOUD_LIVE") else {
        eprintln!("MUXEL_CLOUD_LIVE not set; skipping");
        return;
    };
    let cfg: Value = serde_json::from_str(&cfg).expect("MUXEL_CLOUD_LIVE is JSON");
    let url = cfg["url"].as_str().unwrap();
    let tok = |k: &str| {
        cfg[k]
            .as_str()
            .unwrap_or_else(|| panic!("missing {k}"))
            .to_string()
    };

    let (mut a, rk) = dev(url, &tok("owner_a"), None);
    let rk = rk.expect("a fresh account shows its Recovery Key");
    let (mut b, shown) = dev(url, &tok("owner_b"), Some(&rk));
    assert!(shown.is_none());
    let (mut m, _) = dev(url, &tok("member"), None);
    let (mut s, _) = dev(url, &tok("assignee"), None);
    let assignee = s.session.user;

    // Personal sync between the owner's two devices.
    a.settings.theme = "Nord".into();
    a.personal
        .push(Task::new("personal task", Some(a.session.user), 1));
    let out = cycle(&mut a, &[]);
    let team = out.teams.first().expect("owner is in a team").id;
    cycle(&mut b, &[]);
    assert_eq!(b.settings.theme, "Nord");
    assert_eq!(b.personal.len(), 1);

    // Fingerprints computed by the server match ours.
    let members = a.client.members(team).unwrap();
    for mem in &members {
        let (Some(e), Some(sg), Some(fp)) = (&mem.enc_pub, &mem.sign_pub, &mem.fingerprint) else {
            continue;
        };
        let ours = crypto::fingerprint(
            &crypto::pub32(e, "enc").unwrap(),
            &crypto::pub32(sg, "sign").unwrap(),
        );
        assert_eq!(&ours, fp, "fingerprint format differs from the server's");
    }

    // Team: the owner (already cycled) created scopes; members get keys —
    // either at creation or as pending grants the owner fulfils now.
    cycle(&mut a, &[]);
    let mut t1 = Task::new("for the assignee", Some(a.session.user), 2);
    t1.assignee = Some(assignee);
    let t2 = Task::new("backlog", Some(a.session.user), 3);
    a.team.insert(team, vec![t1.clone(), t2]);
    cycle(&mut a, &[]);

    cycle(&mut m, &[]);
    assert_eq!(m.team.get(&team).map_or(0, Vec::len), 2, "member sees all");
    cycle(&mut s, &[]);
    let seen: Vec<_> = s.team[&team].iter().map(|t| t.title.as_str()).collect();
    assert_eq!(seen, vec!["for the assignee"], "assignee sees only theirs");

    s.team.get_mut(&team).unwrap()[0].add_note(ProgressNote {
        at_ms: 99,
        by_user: Some(assignee),
        by_agent: Some("claude".into()),
        text: "halfway".into(),
        percent: Some(50),
    });
    cycle(&mut s, &[]);
    cycle(&mut a, &[]);
    let got = a.team[&team].iter().find(|t| t.id == t1.id).unwrap();
    assert_eq!(got.percent, Some(50));

    let presence = Presence {
        at_ms: 1,
        device_name: "member laptop".into(),
        projects: vec![ProjectActivity {
            project_key: "name:demo".into(),
            project_name: "demo".into(),
            agents: vec![AgentActivity {
                program: "codex".into(),
                status: "working".into(),
                ..Default::default()
            }],
        }],
    };
    cycle(&mut m, &[(team, presence)]);
    let out = cycle(&mut a, &[]);
    let view = out.teams.iter().find(|t| t.id == team).unwrap();
    assert!(
        view.activity
            .iter()
            .any(|x| x.presence.projects[0].agents[0].program == "codex"),
        "owner sees the member's activity"
    );
    let steady = cycle(&mut a, &[]);
    assert!(steady.settings_changes.is_empty());
    eprintln!("live protocol check passed against {url}");
}
