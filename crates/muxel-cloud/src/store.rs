//! muxel Cloud's local files, under `<data dir>/cloud/`.
//!
//! Every window of a profile is its own process. Exactly one, the holder of
//! `sync.lock`, is the **coordinator**: it runs sync cycles and is the only
//! writer of `state.json`, `tasks.json` and `teams.json`. Other windows never
//! read-modify-write those; they drop small files into `outbox/` (task edits,
//! team shares) and `presence/` (what their workspace's agents are doing), which
//! the coordinator consumes. After writing, the coordinator bumps `stamp`; every
//! window re-reads the shared files when its mtime changes.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use muxel_core::cloud::activity::Presence;
use muxel_core::cloud::tasks::Task;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::Me;
use crate::engine::{EngineState, MemberDecision, TaskDeletion, TeamSettingWrite, TeamView};

/// Tasks for this profile: personal ones (work without an account) and each
/// team's (synced only in a team-bound workspace).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskStore {
    #[serde(default)]
    pub personal: Vec<Task>,
    #[serde(default)]
    pub teams: HashMap<Uuid, Vec<Task>>,
    /// Deleted here; sent to muxel Cloud as tombstones by the next cycle.
    #[serde(default)]
    pub deleted: Vec<TaskDeletion>,
}

fn merge_list(live: &[Task], snapshot: &[Task], out: Vec<Task>) -> Vec<Task> {
    let find = |list: &[Task], id: Uuid| list.iter().find(|t| t.id == id).cloned();
    let mut result: Vec<Task> = Vec::with_capacity(out.len());
    for o in out {
        match find(live, o.id) {
            // Edited here during the cycle: keep both sides' changes.
            Some(l) if find(snapshot, o.id).as_ref() != Some(&l) => {
                let remote_newer = o.updated_at > l.updated_at;
                result.push(muxel_core::cloud::tasks::merge(&l, &o, remote_newer));
            }
            _ => result.push(o),
        }
    }
    for l in live {
        // Created during the cycle (a task the cycle saw but dropped, such as
        // one no longer visible to this user, stays dropped).
        if !result.iter().any(|t| t.id == l.id) && find(snapshot, l.id).is_none() {
            result.push(l.clone());
        }
    }
    result
}

/// Where a task lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "team", rename_all = "snake_case")]
pub enum TaskHome {
    Personal,
    Team(Uuid),
}

impl TaskStore {
    pub fn list(&self, home: TaskHome) -> &[Task] {
        match home {
            TaskHome::Personal => &self.personal,
            TaskHome::Team(t) => self.teams.get(&t).map_or(&[], Vec::as_slice),
        }
    }

    pub fn find(&self, id: Uuid) -> Option<(TaskHome, &Task)> {
        if let Some(t) = self.personal.iter().find(|t| t.id == id) {
            return Some((TaskHome::Personal, t));
        }
        self.teams.iter().find_map(|(team, l)| {
            l.iter()
                .find(|t| t.id == id)
                .map(|t| (TaskHome::Team(*team), t))
        })
    }

    /// Insert or replace a task (an outbox op).
    /// Fold a sync cycle's results into the live store. The cycle ran on
    /// `snapshot`; a task edited here meanwhile is merged with its result
    /// instead of replaced, and a task created meanwhile is kept. Returns
    /// whether anything changed.
    pub fn merge_cycle(
        &mut self,
        snapshot: &TaskStore,
        personal: Option<Vec<Task>>,
        teams: HashMap<Uuid, Vec<Task>>,
    ) -> bool {
        let before = self.clone();
        if let Some(out) = personal {
            self.personal = merge_list(&self.personal, &snapshot.personal, out);
        }
        for (team, out) in teams {
            let snap = snapshot.teams.get(&team).map_or(&[][..], Vec::as_slice);
            let live = self.teams.get(&team).map_or(&[][..], Vec::as_slice);
            let merged = merge_list(live, snap, out);
            self.teams.insert(team, merged);
        }
        // A task deleted here while the cycle ran stays deleted.
        let gone: Vec<Uuid> = self.deleted.iter().map(|d| d.id).collect();
        self.personal.retain(|t| !gone.contains(&t.id));
        for list in self.teams.values_mut() {
            list.retain(|t| !gone.contains(&t.id));
        }
        *self != before
    }

    /// Delete a task for good: drop it here and queue its tombstone.
    pub fn delete(&mut self, home: TaskHome, id: Uuid) -> bool {
        let list = match home {
            TaskHome::Personal => &mut self.personal,
            TaskHome::Team(t) => match self.teams.get_mut(&t) {
                Some(l) => l,
                None => return false,
            },
        };
        let Some(i) = list.iter().position(|t| t.id == id) else {
            return false;
        };
        let task = list.remove(i);
        if !self.deleted.iter().any(|d| d.id == id) {
            self.deleted.push(TaskDeletion {
                team: match home {
                    TaskHome::Personal => None,
                    TaskHome::Team(t) => Some(t),
                },
                id,
                assignee: task.assignee,
            });
        }
        true
    }

    /// Forget deletions a cycle settled. Returns whether any were.
    pub fn settle_deletions(&mut self, done: &[Uuid]) -> bool {
        let before = self.deleted.len();
        self.deleted.retain(|d| !done.contains(&d.id));
        self.deleted.len() != before
    }

    /// Tasks here that `before` didn't have: what a sync cycle brought in.
    /// A team `before` had no list for is skipped — its first pull (a workspace
    /// just bound to it) is the backlog, not news.
    pub fn arrivals<'a>(&'a self, before: &TaskStore) -> Vec<(TaskHome, &'a Task)> {
        let new_in = |live: &'a [Task], old: &[Task], home: TaskHome| {
            live.iter()
                .filter(|t| !old.iter().any(|o| o.id == t.id))
                .map(move |t| (home, t))
                .collect::<Vec<_>>()
        };
        let mut out = new_in(&self.personal, &before.personal, TaskHome::Personal);
        let mut teams: Vec<_> = self.teams.iter().collect();
        teams.sort_by_key(|(id, _)| **id);
        for (team, live) in teams {
            if let Some(old) = before.teams.get(team) {
                out.extend(new_in(live, old, TaskHome::Team(*team)));
            }
        }
        out
    }

    pub fn upsert(&mut self, home: TaskHome, task: Task) {
        let list = match home {
            TaskHome::Personal => &mut self.personal,
            TaskHome::Team(t) => self.teams.entry(t).or_default(),
        };
        match list.iter_mut().find(|x| x.id == task.id) {
            Some(slot) => *slot = task,
            None => list.push(task),
        }
    }
}

/// A change queued by any window for the coordinator.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// The full new state of a task (edits are applied in the window first,
    /// with permission checks, then queued as a snapshot).
    Task { home: TaskHome, task: Task },
    /// Share (or stop sharing) a setting with a team.
    Share(TeamSettingWrite),
    /// The user approved or declined a team member's keys.
    Trust(MemberDecision),
    /// Run a sync cycle now (e.g. after signing in or changing a toggle).
    SyncNow,
}

/// What one window publishes about its workspace.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowPresence {
    pub team: Uuid,
    pub presence: Presence,
}

/// How long a window's presence file counts as live.
pub const PRESENCE_TTL_SECS: u64 = 120;

pub struct CloudDir {
    root: PathBuf,
}

impl CloudDir {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("cloud"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Become the coordinator. Keep the file alive for as long as this process
    /// should coordinate; the OS releases it on exit or crash.
    pub fn try_lock(&self) -> Option<File> {
        let _ = fs::create_dir_all(&self.root);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path("sync.lock"))
            .ok()?;
        match file.try_lock() {
            Ok(()) => Some(file),
            Err(fs::TryLockError::WouldBlock) => None,
            Err(fs::TryLockError::Error(_)) => Some(file),
        }
    }

    fn read<T: DeserializeOwned + Default>(&self, name: &str) -> T {
        match fs::read_to_string(self.path(name)) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("cloud: ignoring unreadable {name}: {e}");
                T::default()
            }),
            Err(_) => T::default(),
        }
    }

    fn write<T: Serialize>(&self, name: &str, value: &T) -> std::io::Result<()> {
        fs::create_dir_all(&self.root)?;
        write_atomic(&self.path(name), &serde_json::to_vec_pretty(value)?)
    }

    pub fn load_state(&self) -> EngineState {
        let mut state: EngineState = self.read("state.json");
        if state.trust.is_empty() {
            state.trust = self.read("trust.json");
        }
        state
    }
    /// Whose keys the user approved is also kept in a file of its own, which
    /// [`Self::clear`] leaves alone: signing out and in again (which a server
    /// can force by refusing the token) must not start trust over, or members
    /// the server added meanwhile would be trusted on sight.
    pub fn save_state(&self, s: &EngineState) -> std::io::Result<()> {
        self.write("trust.json", &s.trust)?;
        self.write("state.json", s)
    }

    pub fn load_tasks(&self) -> TaskStore {
        self.read("tasks.json")
    }
    pub fn save_tasks(&self, t: &TaskStore) -> std::io::Result<()> {
        self.write("tasks.json", t)
    }

    pub fn load_teams(&self) -> Vec<TeamView> {
        self.read("teams.json")
    }
    pub fn save_teams(&self, t: &[TeamView]) -> std::io::Result<()> {
        self.write("teams.json", &t)
    }

    /// The account as of the last cycle (plan, teams), so every window — not
    /// just the one that syncs — knows whether Tasks is available.
    pub fn load_me(&self) -> Option<Me> {
        let text = fs::read_to_string(self.path("me.json")).ok()?;
        serde_json::from_str(&text).ok()
    }
    pub fn save_me(&self, me: &Me) -> std::io::Result<()> {
        self.write("me.json", me)
    }

    /// Tell every window the shared files changed. Returns the new stamp, so
    /// the caller can tell its own bump from another window's.
    pub fn bump_stamp(&self) -> Option<String> {
        let _ = fs::create_dir_all(&self.root);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let value = format!("{nanos}-{}", Uuid::new_v4().simple());
        write_atomic(&self.path("stamp"), value.as_bytes()).ok()?;
        Some(value)
    }

    /// Whether this profile is signed in (any window may have done it).
    pub fn has_account(&self) -> bool {
        self.path("account.json").exists()
    }

    /// The cached account (no secrets).
    pub fn load_account(&self) -> Option<crate::account::AccountFile> {
        let text = fs::read_to_string(self.path("account.json")).ok()?;
        serde_json::from_str(&text).ok()
    }
    pub fn save_account(&self, a: &crate::account::AccountFile) -> std::io::Result<()> {
        self.write("account.json", a)
    }

    /// Forget everything this device knows about the account (sign out).
    /// The lock file stays: another process may hold it. So does `trust.json`
    /// (see [`Self::save_state`]).
    pub fn clear(&self) {
        for name in [
            "account.json",
            "state.json",
            "teams.json",
            "me.json",
            "stamp",
        ] {
            let _ = fs::remove_file(self.path(name));
        }
        let _ = fs::remove_dir_all(self.path("outbox"));
        let _ = fs::remove_dir_all(self.path("presence"));
    }

    /// The stamp's content (changes on every bump; cheap to poll each second).
    pub fn stamp(&self) -> Option<String> {
        fs::read_to_string(self.path("stamp")).ok()
    }

    /// Queue an op for the coordinator (any window, including the coordinator).
    pub fn enqueue(&self, op: &Op) -> std::io::Result<()> {
        let dir = self.path("outbox");
        fs::create_dir_all(&dir)?;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!("{nanos:032}-{}.json", Uuid::new_v4().simple());
        write_atomic(&dir.join(name), &serde_json::to_vec(op)?)
    }

    /// The queued ops' files, in order (cheap enough to poll each second).
    pub fn outbox_files(&self) -> Vec<PathBuf> {
        let Ok(rd) = fs::read_dir(self.path("outbox")) else {
            return Vec::new();
        };
        let mut names: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        names.sort();
        names
    }

    /// Every queued op with its file, in order, left in place: the caller
    /// removes them with [`Self::remove_ops`] once they've been sent, so a
    /// failed or interrupted cycle sends them again. Unreadable files are
    /// dropped so one bad file can't wedge the queue.
    pub fn peek(&self) -> Vec<(PathBuf, Op)> {
        let mut ops = Vec::new();
        for p in self.outbox_files() {
            match fs::read_to_string(&p)
                .ok()
                .and_then(|text| serde_json::from_str::<Op>(&text).ok())
            {
                Some(op) => ops.push((p, op)),
                None => {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        ops
    }

    pub fn remove_ops(&self, files: &[PathBuf]) {
        for f in files {
            let _ = fs::remove_file(f);
        }
    }

    /// Take every queued op in order (coordinator only). Unreadable files are
    /// dropped so one bad file can't wedge the queue.
    pub fn drain(&self) -> Vec<Op> {
        let dir = self.path("outbox");
        let Ok(rd) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut names: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        names.sort();
        let mut ops = Vec::new();
        for p in names {
            if let Ok(text) = fs::read_to_string(&p)
                && let Ok(op) = serde_json::from_str::<Op>(&text)
            {
                ops.push(op);
            }
            let _ = fs::remove_file(&p);
        }
        ops
    }

    /// Publish this window's presence (overwrites its previous one).
    pub fn put_presence(&self, window: &str, p: &[WindowPresence]) -> std::io::Result<()> {
        let dir = self.path("presence");
        fs::create_dir_all(&dir)?;
        write_atomic(&dir.join(format!("{window}.json")), &serde_json::to_vec(p)?)
    }

    pub fn clear_presence(&self, window: &str) {
        let _ = fs::remove_file(self.path("presence").join(format!("{window}.json")));
    }

    /// Every live window's presence, merged per team.
    pub fn collect_presence(&self, device_name: &str, now_ms: u64) -> Vec<(Uuid, Presence)> {
        let mut by_team: HashMap<Uuid, Presence> = HashMap::new();
        let Ok(rd) = fs::read_dir(self.path("presence")) else {
            return Vec::new();
        };
        for e in rd.filter_map(Result::ok) {
            let fresh = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age.as_secs() <= PRESENCE_TTL_SECS);
            if !fresh {
                let _ = fs::remove_file(e.path());
                continue;
            }
            let Ok(text) = fs::read_to_string(e.path()) else {
                continue;
            };
            let Ok(list) = serde_json::from_str::<Vec<WindowPresence>>(&text) else {
                continue;
            };
            for wp in list {
                let slot = by_team.entry(wp.team).or_insert_with(|| Presence {
                    at_ms: now_ms,
                    device_name: device_name.to_string(),
                    projects: Vec::new(),
                });
                slot.projects.extend(wp.presence.projects);
            }
        }
        let mut v: Vec<_> = by_team.into_iter().collect();
        v.sort_by_key(|(t, _)| *t);
        v
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", Uuid::new_v4().simple()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::{CloudDir, Op, TaskHome, TaskStore, WindowPresence};
    use crate::engine::{EngineState, MemberKeys, TeamTrust};
    use muxel_core::cloud::activity::{Presence, ProjectActivity};
    use muxel_core::cloud::tasks::Task;
    use uuid::Uuid;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("muxel-cloud-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn approvals_survive_signing_out() {
        let d = tmp();
        let dir = CloudDir::new(&d);
        let (team, member) = (Uuid::new_v4(), Uuid::new_v4());
        let keys = MemberKeys {
            enc_pub: "e".into(),
            sign_pub: "s".into(),
        };
        let mut state = EngineState::default();
        state.sync.cursor = 7;
        let mut trust = TeamTrust::default();
        trust.approved.insert(member, keys.clone());
        state.trust.insert(team, trust);
        dir.save_state(&state).unwrap();

        dir.clear();
        let back = dir.load_state();
        assert_eq!(back.sync.cursor, 0, "the sync state starts over");
        assert_eq!(back.trust[&team].approved.get(&member), Some(&keys));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn one_coordinator_at_a_time() {
        let d = tmp();
        let dir = CloudDir::new(&d);
        let held = dir.try_lock().expect("first takes it");
        assert!(CloudDir::new(&d).try_lock().is_none(), "second is refused");
        drop(held);
        assert!(CloudDir::new(&d).try_lock().is_some(), "released on drop");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn outbox_drains_in_order_and_skips_junk() {
        let d = tmp();
        let dir = CloudDir::new(&d);
        let a = Task::new("a", None, 1);
        let b = Task::new("b", None, 2);
        dir.enqueue(&Op::Task {
            home: TaskHome::Personal,
            task: a.clone(),
        })
        .unwrap();
        std::fs::write(dir.root().join("outbox").join("0-junk.json"), "{nope").unwrap();
        dir.enqueue(&Op::Task {
            home: TaskHome::Personal,
            task: b.clone(),
        })
        .unwrap();
        let peeked = dir.peek();
        assert_eq!(peeked.len(), 2, "junk dropped, ops kept");
        assert_eq!(dir.outbox_files().len(), 2, "still queued until removed");
        let ops = dir.drain();
        assert!(dir.outbox_files().is_empty());
        let titles: Vec<String> = ops
            .iter()
            .filter_map(|o| match o {
                Op::Task { task, .. } => Some(task.title.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(titles, vec!["a", "b"]);
        assert!(dir.drain().is_empty());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn tasks_and_stamp_roundtrip() {
        let d = tmp();
        let dir = CloudDir::new(&d);
        let mut s = TaskStore::default();
        let team = Uuid::new_v4();
        let t = Task::new("x", None, 1);
        s.upsert(TaskHome::Team(team), t.clone());
        dir.save_tasks(&s).unwrap();
        assert_eq!(dir.load_tasks(), s);
        assert_eq!(dir.load_tasks().find(t.id).unwrap().0, TaskHome::Team(team));
        assert!(dir.stamp().is_none());
        dir.bump_stamp();
        let one = dir.stamp();
        std::thread::sleep(std::time::Duration::from_millis(2));
        dir.bump_stamp();
        assert_ne!(one, dir.stamp());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn presence_merges_windows_per_team() {
        let d = tmp();
        let dir = CloudDir::new(&d);
        let team = Uuid::new_v4();
        let proj = |n: &str| Presence {
            at_ms: 0,
            device_name: String::new(),
            projects: vec![ProjectActivity {
                project_key: n.into(),
                project_name: n.into(),
                agents: vec![],
            }],
        };
        dir.put_presence(
            "w1",
            &[WindowPresence {
                team,
                presence: proj("a"),
            }],
        )
        .unwrap();
        dir.put_presence(
            "w2",
            &[WindowPresence {
                team,
                presence: proj("b"),
            }],
        )
        .unwrap();
        let all = dir.collect_presence("laptop", 5);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.projects.len(), 2);
        assert_eq!(all[0].1.device_name, "laptop");
        dir.clear_presence("w1");
        assert_eq!(dir.collect_presence("laptop", 5)[0].1.projects.len(), 1);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_cycle_keeps_task_edits_made_while_it_ran() {
        use muxel_core::cloud::tasks::Task;
        let a = Task::new("a", None, 1);
        let b = Task::new("b", None, 1);
        let snapshot = TaskStore {
            personal: vec![a.clone(), b.clone()],
            ..Default::default()
        };
        // Meanwhile here: `a` renamed, `c` created.
        let mut live = snapshot.clone();
        live.personal[0].title = "a edited here".into();
        live.personal[0].updated_at = 5;
        let c = Task::new("c", None, 4);
        live.personal.push(c.clone());
        // The cycle's result: `b` changed remotely.
        let mut b2 = b.clone();
        b2.title = "b from the server".into();
        b2.updated_at = 3;
        let changed = live.merge_cycle(&snapshot, Some(vec![a, b2]), Default::default());
        assert!(changed);
        let titles: Vec<_> = live.personal.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["a edited here", "b from the server", "c"]);
    }

    #[test]
    fn arrivals_are_the_tasks_a_cycle_added() {
        let team = Uuid::new_v4();
        let fresh_team = Uuid::new_v4();
        let mine = Task::new("mine", None, 1);
        let mut before = TaskStore::default();
        before.personal.push(mine.clone());
        before.teams.insert(team, Vec::new());

        let mut after = before.clone();
        after.personal[0].title = "mine, edited elsewhere".into();
        after.personal.push(Task::new("from my laptop", None, 2));
        after
            .teams
            .get_mut(&team)
            .unwrap()
            .push(Task::new("from a teammate", None, 3));
        // A team's first pull is its backlog, not news.
        after
            .teams
            .insert(fresh_team, vec![Task::new("old team task", None, 0)]);

        let got: Vec<_> = after
            .arrivals(&before)
            .into_iter()
            .map(|(home, t)| (home, t.title.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (TaskHome::Personal, "from my laptop"),
                (TaskHome::Team(team), "from a teammate"),
            ]
        );
        assert!(before.arrivals(&before).is_empty());
    }

    #[test]
    fn a_deleted_task_stays_deleted_through_a_cycle_that_still_has_it() {
        let a = Task::new("a", None, 1);
        let mut b = Task::new("b", None, 2);
        b.assignee = Some(Uuid::from_u128(9));
        let team = Uuid::new_v4();
        let mut live = TaskStore::default();
        live.personal.push(a.clone());
        live.teams.insert(team, vec![b.clone()]);
        let snapshot = live.clone();

        assert!(live.delete(TaskHome::Personal, a.id));
        assert!(live.delete(TaskHome::Team(team), b.id));
        assert!(!live.delete(TaskHome::Personal, a.id), "already gone");
        assert_eq!(live.deleted.len(), 2);
        assert_eq!(
            live.deleted[1].assignee, b.assignee,
            "tombstone keeps the assignee"
        );

        // The cycle ran on the snapshot and returns both tasks unchanged.
        let mut teams = std::collections::HashMap::new();
        teams.insert(team, vec![b.clone()]);
        live.merge_cycle(&snapshot, Some(vec![a.clone()]), teams);
        assert!(live.personal.is_empty() && live.teams[&team].is_empty());

        assert!(live.settle_deletions(&[a.id]));
        assert_eq!(live.deleted.len(), 1);
        assert!(!live.settle_deletions(&[a.id]));
    }
}
