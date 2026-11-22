//! Tasks: assignable units of work with progress notes from people and agents.
//!
//! Tasks work locally without an account (personal tasks), sync between a
//! user's devices with a plan, and are shared in a team's `tasks` scope. The
//! permission checks here mirror the server's (protocol §5) so the UI only
//! offers what will be accepted.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::roles::{Grants, Permission};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Todo,
    InProgress,
    Blocked,
    Done,
    Cancelled,
}

impl TaskStatus {
    pub fn is_open(self) -> bool {
        matches!(
            self,
            TaskStatus::Todo | TaskStatus::InProgress | TaskStatus::Blocked
        )
    }

    pub fn parse(s: &str) -> Option<TaskStatus> {
        Some(
            match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
                "todo" | "open" | "not_started" => TaskStatus::Todo,
                "in_progress" | "working" | "started" => TaskStatus::InProgress,
                "blocked" => TaskStatus::Blocked,
                "done" | "complete" | "completed" => TaskStatus::Done,
                "cancelled" | "canceled" => TaskStatus::Cancelled,
                _ => return None,
            },
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Todo => "todo",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
            TaskStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
    Urgent,
}

/// One progress entry. Agent notes name the agent program; people's name the
/// user. Notes are merged by `(at_ms, author)` so concurrent notes from two
/// devices both survive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressNote {
    pub at_ms: u64,
    #[serde(default)]
    pub by_user: Option<Uuid>,
    /// Agent program (e.g. "claude") when an agent or muxel reported it.
    #[serde(default)]
    pub by_agent: Option<String>,
    pub text: String,
    #[serde(default)]
    pub percent: Option<u8>,
}

/// Notes kept per task (oldest dropped first).
pub const MAX_NOTES: usize = 50;
/// Agent notes closer together than this replace each other.
pub const COALESCE_MS: u64 = 30_000;
/// Longest note text kept.
pub const MAX_NOTE_CHARS: usize = 2_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Portable project key (see `project_key`); `None` = not tied to one.
    #[serde(default)]
    pub project_key: Option<String>,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub assignee: Option<Uuid>,
    #[serde(default)]
    pub created_by: Option<Uuid>,
    #[serde(default)]
    pub priority: Priority,
    /// Latest reported completion, 0–100.
    #[serde(default)]
    pub percent: Option<u8>,
    #[serde(default)]
    pub progress: Vec<ProgressNote>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

impl Task {
    pub fn new(title: impl Into<String>, created_by: Option<Uuid>, now_ms: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            body: String::new(),
            project_key: None,
            status: TaskStatus::Todo,
            assignee: None,
            created_by,
            priority: Priority::Normal,
            percent: None,
            progress: Vec::new(),
            created_at: now_ms,
            updated_at: now_ms,
        }
    }

    /// Append a note, coalescing rapid agent notes and capping the history.
    pub fn add_note(&mut self, mut note: ProgressNote) {
        note.text = note.text.trim().chars().take(MAX_NOTE_CHARS).collect();
        note.percent = note.percent.map(|p| p.min(100));
        if let Some(p) = note.percent {
            self.percent = Some(p);
        }
        if let Some(last) = self.progress.last_mut()
            && note.by_agent.is_some()
            && last.by_agent == note.by_agent
            && last.by_user == note.by_user
            && note.at_ms.saturating_sub(last.at_ms) < COALESCE_MS
        {
            last.text = note.text;
            last.percent = note.percent.or(last.percent);
        } else {
            self.progress.push(note);
        }
        if self.progress.len() > MAX_NOTES {
            let drop = self.progress.len() - MAX_NOTES;
            self.progress.drain(..drop);
        }
        self.updated_at = self
            .updated_at
            .max(self.progress.last().map_or(0, |n| n.at_ms));
    }
}

/// Why an action was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denied {
    NotAllowed,
    AlreadyAssigned(Uuid),
    Closed,
}

/// Who is acting. `me = None` is a local-only (signed-out) user who may do
/// anything with personal tasks.
#[derive(Clone, Debug)]
pub struct Actor<'a> {
    pub me: Option<Uuid>,
    /// `None` for personal tasks (no team roles apply).
    pub grants: Option<&'a Grants>,
}

impl Actor<'_> {
    fn has(&self, p: Permission) -> bool {
        self.grants.is_none_or(|g| g.has(p))
    }

    pub fn can_see(&self, t: &Task) -> bool {
        match self.grants {
            None => true,
            Some(g) => g.can_see_task(t.assignee.is_some() && t.assignee == self.me),
        }
    }

    pub fn can_create(&self) -> bool {
        self.has(Permission::TasksCreate)
    }

    pub fn can_update(&self, t: &Task) -> bool {
        self.can_see(t) && self.has(Permission::TasksUpdate)
    }

    pub fn can_assign_others(&self) -> bool {
        self.has(Permission::TasksAssign)
    }

    /// Permanently delete a task (personal tasks: always).
    pub fn can_delete(&self, t: &Task) -> bool {
        self.can_see(t) && self.has(Permission::TasksDelete)
    }

    /// Whether [`take`] would succeed.
    pub fn can_take(&self, t: &Task) -> bool {
        t.status.is_open()
            && self.me.is_some()
            && t.assignee != self.me
            && (self.can_assign_others()
                || (t.assignee.is_none() && self.has(Permission::TasksTake)))
    }
}

/// Take a task: assign it to the actor and start it.
pub fn take(t: &mut Task, actor: &Actor, now_ms: u64) -> Result<(), Denied> {
    if !t.status.is_open() {
        return Err(Denied::Closed);
    }
    match t.assignee {
        Some(a) if Some(a) == actor.me => {}
        Some(a) if !actor.can_assign_others() => return Err(Denied::AlreadyAssigned(a)),
        _ if !(actor.has(Permission::TasksTake) || actor.can_assign_others()) => {
            return Err(Denied::NotAllowed);
        }
        _ => {}
    }
    t.assignee = actor.me;
    if t.status == TaskStatus::Todo {
        t.status = TaskStatus::InProgress;
    }
    t.updated_at = now_ms;
    Ok(())
}

/// Assign (or unassign) a task.
pub fn assign(t: &mut Task, to: Option<Uuid>, actor: &Actor, now_ms: u64) -> Result<(), Denied> {
    let self_take = to.is_some() && to == actor.me && t.assignee.is_none();
    if !(actor.can_assign_others() || (self_take && actor.has(Permission::TasksTake))) {
        return Err(Denied::NotAllowed);
    }
    t.assignee = to;
    t.updated_at = now_ms;
    Ok(())
}

pub fn set_status(
    t: &mut Task,
    status: TaskStatus,
    actor: &Actor,
    now_ms: u64,
) -> Result<(), Denied> {
    if !actor.can_update(t) {
        return Err(Denied::NotAllowed);
    }
    t.status = status;
    if status == TaskStatus::Done {
        t.percent = Some(100);
    }
    t.updated_at = now_ms;
    Ok(())
}

/// Merge a remote copy into a local one. Scalar fields come from whichever
/// side `remote_newer` says won (HLC order); progress notes are unioned so
/// notes added concurrently on two devices both survive.
pub fn merge(local: &Task, remote: &Task, remote_newer: bool) -> Task {
    let (mut out, other) = if remote_newer {
        (remote.clone(), local)
    } else {
        (local.clone(), remote)
    };
    for n in &other.progress {
        if !out
            .progress
            .iter()
            .any(|m| m.at_ms == n.at_ms && m.by_user == n.by_user && m.by_agent == n.by_agent)
        {
            out.progress.push(n.clone());
        }
    }
    out.progress.sort_by_key(|n| n.at_ms);
    if out.progress.len() > MAX_NOTES {
        let drop = out.progress.len() - MAX_NOTES;
        out.progress.drain(..drop);
    }
    out
}

/// The Tasks button's badge: open tasks assigned to me.
pub fn badge_count(tasks: &[Task], me: Option<Uuid>) -> usize {
    tasks
        .iter()
        .filter(|t| t.status.is_open() && t.assignee.is_some() && t.assignee == me)
        .count()
}

/// The panel's groups, each sorted by priority (high first) then age.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Groups {
    pub mine: Vec<Task>,
    pub in_progress: Vec<Task>,
    pub not_started: Vec<Task>,
    pub done: Vec<Task>,
}

/// Split visible tasks into the panel's groups. "Mine" takes precedence; the
/// done list keeps the most recent `done_limit`.
pub fn group(tasks: &[Task], actor: &Actor, done_limit: usize) -> Groups {
    let mut g = Groups::default();
    for t in tasks.iter().filter(|t| actor.can_see(t)) {
        let mine = t.assignee.is_some() && t.assignee == actor.me;
        match t.status {
            TaskStatus::Done | TaskStatus::Cancelled => g.done.push(t.clone()),
            _ if mine => g.mine.push(t.clone()),
            TaskStatus::InProgress | TaskStatus::Blocked => g.in_progress.push(t.clone()),
            TaskStatus::Todo if t.assignee.is_some() => g.in_progress.push(t.clone()),
            TaskStatus::Todo => g.not_started.push(t.clone()),
        }
    }
    let by_priority = |a: &Task, b: &Task| {
        b.priority
            .cmp(&a.priority)
            .then(a.created_at.cmp(&b.created_at))
    };
    g.mine.sort_by(by_priority);
    g.in_progress.sort_by(by_priority);
    g.not_started.sort_by(by_priority);
    g.done.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
    g.done.truncate(done_limit);
    g
}

/// One row of the Tasks panel: a task and where it lives (`None` = personal).
#[derive(Clone, Debug, PartialEq)]
pub struct PanelTask {
    pub team: Option<Uuid>,
    pub task: Task,
}

/// The Tasks panel's sections.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelGroups {
    /// Personal tasks and team tasks assigned to me, still open.
    pub mine: Vec<PanelTask>,
    /// The team's open tasks someone else has.
    pub in_progress: Vec<PanelTask>,
    /// The team's unassigned open tasks.
    pub not_started: Vec<PanelTask>,
    /// Recently finished or cancelled, newest first.
    pub done: Vec<PanelTask>,
}

impl PanelGroups {
    /// The Tasks button's badge.
    pub fn badge(&self) -> usize {
        self.mine.len()
    }
}

/// Group personal tasks (always mine) and one team's tasks (as `actor` may
/// see them) for the panel.
pub fn panel_groups(
    personal: &[Task],
    team: Option<(Uuid, &[Task], &Actor)>,
    done_limit: usize,
) -> PanelGroups {
    let mut g = PanelGroups::default();
    let row = |team: Option<Uuid>, t: &Task| PanelTask {
        team,
        task: t.clone(),
    };
    for t in personal {
        if t.status.is_open() {
            g.mine.push(row(None, t));
        } else {
            g.done.push(row(None, t));
        }
    }
    if let Some((id, tasks, actor)) = team {
        let tg = group(tasks, actor, usize::MAX);
        g.mine.extend(tg.mine.iter().map(|t| row(Some(id), t)));
        g.in_progress
            .extend(tg.in_progress.iter().map(|t| row(Some(id), t)));
        g.not_started
            .extend(tg.not_started.iter().map(|t| row(Some(id), t)));
        g.done.extend(tg.done.iter().map(|t| row(Some(id), t)));
    }
    let by_priority = |a: &PanelTask, b: &PanelTask| {
        b.task
            .priority
            .cmp(&a.task.priority)
            .then(a.task.created_at.cmp(&b.task.created_at))
    };
    g.mine.sort_by(by_priority);
    g.done.sort_by_key(|t| std::cmp::Reverse(t.task.updated_at));
    g.done.truncate(done_limit);
    g
}

/// Which tasks a `muxel ctl task` caller may see and change.
///
/// With outside control on, every task the app can show. With it off, the
/// control server accepts task commands only from authenticated local
/// launches of task panes, and each may reach only its own task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtlTaskScope {
    All,
    Only(Uuid),
    Refused,
}

impl CtlTaskScope {
    /// `linked` is the task linked to the caller's authenticated live launch.
    pub fn of(outside_control: bool, linked: Option<Uuid>) -> Self {
        match (outside_control, linked) {
            (true, _) => Self::All,
            (false, Some(task)) => Self::Only(task),
            (false, None) => Self::Refused,
        }
    }

    pub fn allows(self, task: Uuid) -> bool {
        match self {
            Self::All => true,
            Self::Only(own) => own == task,
            Self::Refused => false,
        }
    }
}

impl crate::Workspace {
    pub fn authenticated_task_link(
        &self,
        pane: Uuid,
        presented_hash: Option<&crate::control::TaskLaunchHash>,
    ) -> Option<Uuid> {
        let expected = self.task_launch_hashes.get(&pane)?;
        if task_hash_matches(expected, presented_hash?) {
            self.task_links.get(&pane).copied()
        } else {
            None
        }
    }

    /// Publish the verifier of the installed launch, or retire the old one.
    pub fn set_task_launch_hash(
        &mut self,
        pane: Uuid,
        hash: Option<crate::control::TaskLaunchHash>,
    ) -> bool {
        if let Some(hash) = hash.filter(|_| self.task_links.contains_key(&pane)) {
            let retired = self.task_tmux_launches.remove(&pane).is_some();
            let changed = self.task_launch_hashes.insert(pane, hash) != Some(hash);
            retired || changed
        } else {
            self.task_launch_hashes.remove(&pane).is_some()
        }
    }

    pub fn retain_task_links(&mut self, mut keep: impl FnMut(Uuid, Uuid) -> bool) -> bool {
        let before = self.task_links.len();
        let hashes_before = self.task_launch_hashes.len();
        let tmux_before = self.task_tmux_launches.len();
        self.task_links.retain(|pane, task| keep(*pane, *task));
        self.task_launch_hashes
            .retain(|pane, _| self.task_links.contains_key(pane));
        self.task_tmux_launches
            .retain(|pane, _| self.task_links.contains_key(pane));
        self.task_links.len() != before
            || self.task_launch_hashes.len() != hashes_before
            || self.task_tmux_launches.len() != tmux_before
    }
}

pub fn task_hash_matches(
    expected: &crate::control::TaskLaunchHash,
    presented: &crate::control::TaskLaunchHash,
) -> bool {
    use subtle::ConstantTimeEq as _;
    bool::from(expected.ct_eq(presented))
}

#[cfg(test)]
mod launch_authentication_tests {
    use super::CtlTaskScope;
    use crate::Workspace;
    use uuid::Uuid;

    #[test]
    fn task_launch_authentication_refuses_missing_and_wrong_credentials() {
        let pane = Uuid::from_u128(1);
        let task = Uuid::from_u128(2);
        let hash = [42u8; 32];
        let workspace: Workspace = serde_json::from_value(serde_json::json!({
            "projects": [], "active_project": null, "instances": [],
            "task_links": { (pane): task },
            "task_launch_hashes": { (pane): hash },
        }))
        .unwrap();
        let scope =
            |presented| CtlTaskScope::of(false, workspace.authenticated_task_link(pane, presented));
        let wrong = [43u8; 32];
        assert_eq!(scope(Some(&hash)), CtlTaskScope::Only(task));
        assert_eq!(
            (scope(None), scope(Some(&wrong))),
            (CtlTaskScope::Refused, CtlTaskScope::Refused)
        );
    }

    #[test]
    fn task_launch_authentication_loads_legacy_links_without_authorizing_them() {
        let pane = Uuid::from_u128(1);
        let task = Uuid::from_u128(2);
        let workspace: Workspace = serde_json::from_value(serde_json::json!({
            "projects": [], "active_project": null, "instances": [],
            "task_links": { (pane): task },
        }))
        .unwrap();
        assert_eq!(workspace.task_links.get(&pane), Some(&task));
        assert_eq!(
            workspace.authenticated_task_link(pane, Some(&[42; 32])),
            None
        );
    }
}

/// Appended to the prompt of an agent started on a task (Start with agent):
/// how to report progress back to the Tasks panel. `exe` is the muxel binary.
pub fn task_instruction(exe: &str, task: Uuid) -> String {
    let exe = crate::ssh::sh_quote(exe);
    let short = &task.simple().to_string()[..8];
    format!(
        "This is muxel task {short}. Report progress to it as you work, after each meaningful step, with:\n\
         {exe} ctl task progress {short} \"what you just did\" --percent N\n\
         Add --status blocked if you're stuck waiting on someone, and --status done once the task is finished."
    )
}

/// The automatic note for an agent status change on a task-linked pane, and
/// the status the task should move to. Done is never set automatically: an
/// agent finishing a turn hasn't necessarily finished the task.
pub fn status_note(agent: &str, state: &str) -> Option<(String, Option<TaskStatus>)> {
    Some(match state {
        "working" => (format!("{agent} is working"), Some(TaskStatus::InProgress)),
        "blocked" => (
            format!("{agent} is waiting for input"),
            Some(TaskStatus::Blocked),
        ),
        "done" | "idle" => (format!("{agent} finished a turn"), None),
        "exited" => (format!("{agent} exited"), None),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Actor, CtlTaskScope, Denied, MAX_NOTES, Priority, ProgressNote, Task, TaskStatus, assign,
        badge_count, group, merge, panel_groups, set_status, take,
    };
    use crate::cloud::roles::Grants;
    use uuid::Uuid;

    fn me() -> Option<Uuid> {
        Some(Uuid::from_u128(1))
    }
    fn other() -> Option<Uuid> {
        Some(Uuid::from_u128(2))
    }
    fn note(at: u64, agent: Option<&str>, text: &str) -> ProgressNote {
        ProgressNote {
            at_ms: at,
            by_user: me(),
            by_agent: agent.map(String::from),
            text: text.into(),
            percent: None,
        }
    }

    #[test]
    fn member_can_take_unassigned_but_not_someone_elses() {
        let g = Grants::from_wire("member", &[]);
        let a = Actor {
            me: me(),
            grants: Some(&g),
        };
        let mut t = Task::new("x", other(), 0);
        take(&mut t, &a, 1).unwrap();
        assert_eq!(t.assignee, me());
        assert_eq!(t.status, TaskStatus::InProgress);
        let mut t2 = Task::new("y", other(), 0);
        t2.assignee = other();
        assert_eq!(
            take(&mut t2, &a, 1),
            Err(Denied::AlreadyAssigned(Uuid::from_u128(2)))
        );
        assert_eq!(assign(&mut t2, me(), &a, 1), Err(Denied::NotAllowed));
    }

    #[test]
    fn manager_reassigns_assignee_only_updates_own() {
        let mg = Grants::from_wire("manager", &[]);
        let m = Actor {
            me: me(),
            grants: Some(&mg),
        };
        let mut t = Task::new("x", me(), 0);
        assign(&mut t, other(), &m, 1).unwrap();
        assert_eq!(t.assignee, other());

        let ag = Grants::from_wire("assignee", &[]);
        let a = Actor {
            me: other(),
            grants: Some(&ag),
        };
        set_status(&mut t, TaskStatus::Done, &a, 2).unwrap();
        assert_eq!(t.percent, Some(100));
        let mut not_mine = Task::new("z", me(), 0);
        assert_eq!(
            set_status(&mut not_mine, TaskStatus::Done, &a, 2),
            Err(Denied::NotAllowed)
        );
        assert!(!a.can_create());
    }

    #[test]
    fn only_roles_with_tasks_delete_may_delete_and_take_follows_take() {
        let t = Task::new("x", other(), 0);
        for (role, delete) in [
            ("owner", true),
            ("admin", true),
            ("manager", true),
            ("member", false),
            ("assignee", false),
            ("observer", false),
        ] {
            let g = Grants::from_wire(role, &[]);
            let a = Actor {
                me: me(),
                grants: Some(&g),
            };
            assert_eq!(a.can_delete(&t), delete, "{role}");
        }
        let personal = Actor {
            me: me(),
            grants: None,
        };
        assert!(personal.can_delete(&t));

        let member = Grants::from_wire("member", &[]);
        let m = Actor {
            me: me(),
            grants: Some(&member),
        };
        assert!(m.can_take(&t));
        let mut theirs = t.clone();
        theirs.assignee = other();
        assert!(!m.can_take(&theirs), "a member can't take someone else's");
        let mut mine = t.clone();
        mine.assignee = me();
        assert!(!m.can_take(&mine), "already mine");
        let observer = Grants::from_wire("observer", &[]);
        let o = Actor {
            me: me(),
            grants: Some(&observer),
        };
        assert!(!o.can_take(&t) && !o.can_update(&t));
    }

    #[test]
    fn personal_tasks_have_no_role_limits() {
        let a = Actor {
            me: None,
            grants: None,
        };
        let mut t = Task::new("x", None, 0);
        assert!(a.can_create() && a.can_update(&t));
        set_status(&mut t, TaskStatus::Blocked, &a, 1).unwrap();
    }

    #[test]
    fn agent_notes_coalesce_and_cap() {
        let mut t = Task::new("x", me(), 0);
        t.add_note(note(1_000, Some("claude"), "a"));
        t.add_note(note(2_000, Some("claude"), "b"));
        assert_eq!(t.progress.len(), 1);
        assert_eq!(t.progress[0].text, "b");
        t.add_note(note(100_000, Some("claude"), "c"));
        t.add_note(note(100_500, None, "human"));
        assert_eq!(t.progress.len(), 3);
        for i in 0..100 {
            t.add_note(note(1_000_000 + i * 60_000, None, "n"));
        }
        assert_eq!(t.progress.len(), MAX_NOTES);
    }

    #[test]
    fn merge_unions_notes_and_takes_newer_fields() {
        let mut base = Task::new("x", me(), 0);
        base.add_note(note(1, None, "shared"));
        let mut local = base.clone();
        local.add_note(note(50_000, None, "local note"));
        let mut remote = base.clone();
        remote.title = "renamed".into();
        remote.add_note(note(60_000, None, "remote note"));
        let m = merge(&local, &remote, true);
        assert_eq!(m.title, "renamed");
        let texts: Vec<_> = m.progress.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(texts, vec!["shared", "local note", "remote note"]);
    }

    #[test]
    fn grouping_and_badge() {
        let a = Actor {
            me: me(),
            grants: None,
        };
        let mut mine = Task::new("mine", me(), 1);
        mine.assignee = me();
        let mut theirs = Task::new("theirs", me(), 2);
        theirs.assignee = other();
        theirs.status = TaskStatus::InProgress;
        let mut urgent = Task::new("urgent", me(), 5);
        urgent.priority = Priority::Urgent;
        let normal = Task::new("normal", me(), 3);
        let mut done = Task::new("done", me(), 4);
        done.status = TaskStatus::Done;
        done.assignee = me();
        let all = vec![mine, theirs, urgent, normal, done];
        let g = group(&all, &a, 10);
        assert_eq!(g.mine.len(), 1);
        assert_eq!(g.in_progress[0].title, "theirs");
        assert_eq!(g.not_started[0].title, "urgent");
        assert_eq!(g.done.len(), 1);
        assert_eq!(badge_count(&all, me()), 1);
    }

    #[test]
    fn status_parse() {
        assert_eq!(
            TaskStatus::parse("in-progress"),
            Some(TaskStatus::InProgress)
        );
        assert_eq!(TaskStatus::parse("DONE"), Some(TaskStatus::Done));
        assert_eq!(TaskStatus::parse("nah"), None);
        for s in [
            TaskStatus::Todo,
            TaskStatus::InProgress,
            TaskStatus::Blocked,
            TaskStatus::Done,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(TaskStatus::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn personal_tasks_are_mine_and_team_tasks_follow_the_role() {
        use crate::cloud::roles::Grants;
        let me = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let team = uuid::Uuid::new_v4();
        let mut p_open = Task::new("personal", None, 1);
        p_open.priority = Priority::High;
        let mut p_done = Task::new("personal done", None, 1);
        p_done.status = TaskStatus::Done;
        p_done.updated_at = 9;
        let mut mine = Task::new("team mine", None, 2);
        mine.assignee = Some(me);
        let mut theirs = Task::new("team theirs", None, 3);
        theirs.assignee = Some(other);
        theirs.status = TaskStatus::InProgress;
        let free = Task::new("team free", None, 4);
        let team_tasks = vec![mine, theirs, free];
        let grants = Grants::from_wire("member", &[]);
        let actor = Actor {
            me: Some(me),
            grants: Some(&grants),
        };
        let g = panel_groups(&[p_open, p_done], Some((team, &team_tasks, &actor)), 10);
        let titles = |v: &[super::PanelTask]| -> Vec<String> {
            v.iter().map(|t| t.task.title.clone()).collect()
        };
        assert_eq!(titles(&g.mine), vec!["personal", "team mine"]);
        assert_eq!(titles(&g.in_progress), vec!["team theirs"]);
        assert_eq!(titles(&g.not_started), vec!["team free"]);
        assert_eq!(titles(&g.done), vec!["personal done"]);
        assert_eq!(g.badge(), 2);
        assert_eq!(g.mine[1].team, Some(team));
        // An assignee sees only their own team task.
        let grants = Grants::from_wire("assignee", &[]);
        let actor = Actor {
            me: Some(me),
            grants: Some(&grants),
        };
        let g = panel_groups(&[], Some((team, &team_tasks, &actor)), 10);
        assert_eq!(titles(&g.mine), vec!["team mine"]);
        assert!(g.in_progress.is_empty() && g.not_started.is_empty());
    }

    #[test]
    fn with_outside_control_off_a_pane_reaches_only_its_own_task() {
        let (own, other) = (Uuid::new_v4(), Uuid::new_v4());
        let pane = CtlTaskScope::of(false, Some(own));
        assert!(pane.allows(own));
        assert!(!pane.allows(other));
        let stranger = CtlTaskScope::of(false, None);
        assert!(!stranger.allows(own) && !stranger.allows(other));
        assert_eq!(stranger, CtlTaskScope::Refused);
        let open = CtlTaskScope::of(true, None);
        assert!(open.allows(own) && open.allows(other));
        assert_eq!(CtlTaskScope::of(true, Some(own)), CtlTaskScope::All);
    }
}
