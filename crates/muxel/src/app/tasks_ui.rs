//! The Tasks panel: a right-hand panel (sharing the slot with Git diff) that
//! lists your personal tasks and, in a workspace bound to a team, the team's.
//! Personal tasks work without an account; with muxel Cloud they sync
//! between your computers, and team tasks are shared with your team.
//!
//! Every window reads and writes `data/cloud/tasks.json` (read, change one
//! task, write, bump the stamp), so windows and the sync coordinator see each
//! other's edits.

use super::*;
use muxel_cloud::engine::TeamView;
use muxel_cloud::store::TaskHome;
use muxel_core::cloud::roles::{Grants, Permission};
use muxel_core::cloud::tasks::{
    self as mtasks, Actor, PanelGroups, PanelTask, ProgressNote, Task, TaskStatus,
};
use muxel_core::control as ctl_types;
use serde_json::{Value, json};

/// Finished tasks the panel keeps listing.
const DONE_SHOWN: usize = 15;

pub(super) struct TasksUi {
    /// The expanded task.
    pub(super) selected: Option<Uuid>,
    /// The new-task row is open.
    pub(super) adding: bool,
    /// The new task goes to the bound team (else personal).
    pub(super) add_to_team: bool,
    /// Showing the team's activity instead of tasks.
    pub(super) show_team: bool,
    /// The task whose assignee picker is open.
    pub(super) assigning: Option<Uuid>,
    /// The task asking "delete for good?".
    pub(super) confirm_delete: Option<Uuid>,
    pub(super) new_title: Entity<InputState>,
    pub(super) note: Entity<InputState>,
    /// Per task-linked pane: the status and OSC 7501 report revision last
    /// noted, so only changes become progress notes.
    seen: HashMap<Uuid, (muxel_terminal::AgentStatus, u64)>,
}

impl TasksUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<MuxelApp>) -> Self {
        Self {
            selected: None,
            adding: false,
            add_to_team: true,
            show_team: false,
            assigning: None,
            confirm_delete: None,
            new_title: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("What needs doing?").to_string())
            }),
            note: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("Add a note…").to_string())),
            seen: HashMap::new(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub(super) fn status_label(s: TaskStatus) -> SharedString {
    match s {
        TaskStatus::Todo => t("Not started"),
        TaskStatus::InProgress => t("In progress"),
        TaskStatus::Blocked => t("Blocked"),
        TaskStatus::Done => t("Done"),
        TaskStatus::Cancelled => t("Cancelled"),
    }
}

impl MuxelApp {
    /// The team this window's workspace is bound to, if any, and how this
    /// user sees it.
    pub(super) fn bound_team(&self) -> Option<&TeamView> {
        let id = self.workspace.cloud.as_ref()?.team_id;
        if !self.cloud.ready() {
            return None;
        }
        self.cloud.teams.iter().find(|t| t.id == id)
    }

    /// The team whose tasks this workspace shows: its bound team, unless the
    /// workspace turned team tasks off.
    pub(super) fn tasks_team(&self) -> Option<&TeamView> {
        let binding = self.workspace.cloud.as_ref()?;
        if !binding.tasks_enabled {
            return None;
        }
        self.bound_team()
    }

    /// The signed-in user (for "mine").
    pub(super) fn cloud_me(&self) -> Option<Uuid> {
        self.cloud.user()
    }

    pub(super) fn task_groups(&self) -> PanelGroups {
        let me = self.cloud_me();
        let team = self.tasks_team();
        let grants = team.map(TeamView::grants);
        let actor = Actor {
            me,
            grants: grants.as_ref(),
        };
        let team_tasks = team.map(|tv| {
            (
                tv.id,
                self.cloud
                    .tasks
                    .teams
                    .get(&tv.id)
                    .map_or(&[][..], Vec::as_slice),
            )
        });
        mtasks::panel_groups(
            &self.cloud.tasks.personal,
            team_tasks.map(|(id, list)| (id, list, &actor)),
            DONE_SHOWN,
        )
    }

    pub(super) fn tasks_badge(&self) -> usize {
        self.task_groups().badge()
    }

    /// Tasks are a muxel Cloud feature: signed in, unlocked, and on a plan that
    /// includes sync (one with an overdue payment still shows them, read-only).
    /// Otherwise the button, the panel and `ctl task` are all hidden.
    pub(super) fn tasks_available(&self) -> bool {
        self.cloud.ready()
            && self
                .cloud
                .me
                .as_ref()
                .is_some_and(|me| me.entitlements.personal_sync)
    }

    pub(super) fn toggle_tasks(&mut self, cx: &mut Context<Self>) {
        if !self.tasks_available() {
            self.show_tasks = false;
            return;
        }
        self.show_tasks = !self.show_tasks;
        if self.show_tasks {
            self.show_git_diff = false;
        }
        cx.notify();
    }

    pub(super) fn set_tasks_panel_width(&mut self, width: f32) {
        if self.workspace.tasks_panel_width != Some(width) {
            self.workspace.tasks_panel_width = Some(width);
            self.persist();
        }
    }

    /// Where task `id` lives, and a copy of it.
    pub(super) fn find_task(&self, id: Uuid) -> Option<(TaskHome, Task)> {
        self.cloud.tasks.find(id).map(|(home, t)| (home, t.clone()))
    }

    /// Save one changed task: re-read the shared file, replace the task, write
    /// it back, and tell the other windows and the sync.
    pub(super) fn write_task(&mut self, home: TaskHome, task: Task, cx: &mut Context<Self>) {
        match self.cloud.dir() {
            Some(dir) => {
                let mut disk = dir.load_tasks();
                disk.upsert(home, task);
                if let Err(e) = dir.save_tasks(&disk) {
                    self.add_event(
                        NotifKind::Error,
                        t("Couldn't save the task").to_string(),
                        e.to_string(),
                    );
                }
                self.cloud.tasks = disk;
                self.cloud.bump_stamp();
            }
            None => self.cloud.tasks.upsert(home, task),
        }
        self.cloud.request_sync();
        cx.notify();
    }

    /// Apply `f` to task `id` as this user, saving on success.
    fn try_edit_task(
        &mut self,
        id: Uuid,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Task, &Actor, u64) -> Result<(), mtasks::Denied>,
    ) -> Result<(), SharedString> {
        let Some((home, mut task)) = self.find_task(id) else {
            return Err(t("That task no longer exists."));
        };
        let grants: Option<Grants> = match home {
            TaskHome::Personal => None,
            TaskHome::Team(team) => match self.cloud.teams.iter().find(|t| t.id == team) {
                Some(tv) => Some(tv.grants()),
                None => return Err(t("You're no longer in that task's team.")),
            },
        };
        let actor = Actor {
            me: self.cloud_me(),
            grants: grants.as_ref(),
        };
        match f(&mut task, &actor, now_ms()) {
            Ok(()) => {
                self.write_task(home, task, cx);
                Ok(())
            }
            Err(mtasks::Denied::NotAllowed) => Err(t("Your team role doesn't allow that.")),
            Err(mtasks::Denied::AlreadyAssigned(_)) => Err(t("Someone else already has it.")),
            Err(mtasks::Denied::Closed) => Err(t("It's already finished.")),
        }
    }

    /// [`Self::try_edit_task`] from the panel: a refusal goes to the feed.
    fn edit_task(
        &mut self,
        id: Uuid,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Task, &Actor, u64) -> Result<(), mtasks::Denied>,
    ) {
        if let Err(why) = self.try_edit_task(id, cx, f) {
            self.add_event(
                NotifKind::Error,
                t("Couldn't change the task").to_string(),
                why.to_string(),
            );
            cx.notify();
        }
    }

    /// Assign task `id` to `to` (`None` = unassigned), if the role allows.
    pub(super) fn task_assign(&mut self, id: Uuid, to: Option<Uuid>, cx: &mut Context<Self>) {
        self.tasks_ui.assigning = None;
        self.edit_task(id, cx, move |t, actor, now| {
            mtasks::assign(t, to, actor, now)
        });
    }

    /// Delete task `id` for good, everywhere it syncs to, if the role allows.
    pub(super) fn task_delete(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.tasks_ui.confirm_delete = None;
        let Some((home, task)) = self.find_task(id) else {
            return;
        };
        let grants = match home {
            TaskHome::Personal => None,
            TaskHome::Team(team) => Some(
                self.cloud
                    .teams
                    .iter()
                    .find(|t| t.id == team)
                    .map(TeamView::grants)
                    .unwrap_or_default(),
            ),
        };
        let actor = Actor {
            me: self.cloud_me(),
            grants: grants.as_ref(),
        };
        if !actor.can_delete(&task) {
            self.add_event(
                NotifKind::Error,
                t("Couldn't delete the task").to_string(),
                t("Your team role doesn't allow that.").to_string(),
            );
            cx.notify();
            return;
        }
        match self.cloud.dir() {
            Some(dir) => {
                let mut disk = dir.load_tasks();
                disk.delete(home, id);
                if let Err(e) = dir.save_tasks(&disk) {
                    self.add_event(
                        NotifKind::Error,
                        t("Couldn't delete the task").to_string(),
                        e.to_string(),
                    );
                }
                self.cloud.tasks = disk;
                self.cloud.bump_stamp();
            }
            None => {
                self.cloud.tasks.delete(home, id);
            }
        }
        if self.tasks_ui.selected == Some(id) {
            self.tasks_ui.selected = None;
        }
        let before = self.workspace.task_links.len();
        self.workspace.task_links.retain(|_, task| *task != id);
        if self.workspace.task_links.len() != before {
            self.persist();
        }
        self.cloud.request_sync();
        cx.notify();
    }

    pub(super) fn task_take(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.edit_task(id, cx, mtasks::take);
    }

    pub(super) fn task_set_status(&mut self, id: Uuid, status: TaskStatus, cx: &mut Context<Self>) {
        self.edit_task(id, cx, move |t, actor, now| {
            mtasks::set_status(t, status, actor, now)
        });
    }

    /// Add a progress note (from the panel, `ctl task progress`, or an agent).
    pub(super) fn task_add_note(
        &mut self,
        id: Uuid,
        text: String,
        agent: Option<String>,
        percent: Option<u8>,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        let by_user = self.cloud_me();
        self.try_edit_task(id, cx, move |t, actor, now| {
            if !actor.can_update(t) {
                return Err(mtasks::Denied::NotAllowed);
            }
            t.add_note(ProgressNote {
                at_ms: now,
                by_user,
                by_agent: agent,
                text,
                percent,
            });
            t.updated_at = now;
            Ok(())
        })
    }

    fn task_submit_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.tasks_ui.new_title.read(cx).value().trim().to_string();
        if title.is_empty() {
            return;
        }
        let me = self.cloud_me();
        let mut task = Task::new(title, me, now_ms());
        task.project_key = self.active_project_key();
        let home = match self.tasks_team() {
            Some(team)
                if self.tasks_ui.add_to_team && team.grants().has(Permission::TasksCreate) =>
            {
                TaskHome::Team(team.id)
            }
            _ => TaskHome::Personal,
        };
        let id = task.id;
        self.tasks_ui
            .new_title
            .update(cx, |s, cx| s.set_value(String::new(), window, cx));
        self.tasks_ui.adding = false;
        self.tasks_ui.selected = Some(id);
        self.write_task(home, task, cx);
    }

    fn task_submit_note(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.tasks_ui.note.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.tasks_ui
            .note
            .update(cx, |s, cx| s.set_value(String::new(), window, cx));
        if let Err(why) = self.task_add_note(id, text, None, None, cx) {
            self.add_event(
                NotifKind::Error,
                t("Couldn't change the task").to_string(),
                why.to_string(),
            );
        }
    }

    /// The active project's portable key, from the sync's cache (no git call
    /// on the UI thread); `name:` keys otherwise.
    fn active_project_key(&self) -> Option<String> {
        let p = self.workspace.active()?;
        Some(
            self.cloud
                .project_key_cached(p.id)
                .unwrap_or_else(|| muxel_core::cloud::project_key::project_key(None, "", &p.name)),
        )
    }

    /// The local project a task's key points at, falling back to the active one.
    fn task_project(&self, task: &Task) -> Option<Uuid> {
        task.project_key
            .as_deref()
            .and_then(|key| {
                self.workspace.projects.iter().find(|p| {
                    self.cloud.project_key_cached(p.id).as_deref() == Some(key)
                        || muxel_core::cloud::project_key::project_key(None, "", &p.name) == key
                })
            })
            .map(|p| p.id)
            .or(self.workspace.active_project)
    }

    /// Open an agent on the task: the toolbar's agent, in the task's project,
    /// with the task as its prompt. The pane is linked to the task, so its
    /// progress is noted automatically.
    pub(super) fn task_start_with_agent(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, task)) = self.find_task(id) else {
            return;
        };
        let Some(pid) = self.task_project(&task) else {
            return;
        };
        let mut prompt = task.title.clone();
        if !task.body.trim().is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(task.body.trim());
        }
        prompt.push_str("\n\n");
        prompt.push_str(&muxel_core::cloud::tasks::task_instruction(
            &crate::control::exe_path(),
            id,
        ));
        let runner = muxel_core::Runner {
            id: Uuid::new_v4(),
            name: task.title.clone(),
            preset_id: None,
            auto_mode_presses: 0,
            prompt,
        };
        let preset = self.current_agent_preset();
        let instance = runner.build_instance(pid, &preset, "");
        let iid = instance.id;
        self.workspace.task_links.insert(iid, id);
        if task.status == TaskStatus::Todo || task.assignee.is_none() {
            self.task_take(id, cx);
        }
        let target = self.active_instance;
        self.place_and_spawn(
            pid,
            instance,
            PlacementMode::Split(SplitDirection::Horizontal),
            target,
            None,
            window,
            cx,
        );
        // With outside control off, this pane's link is what starts the
        // control server; start it now rather than on the next tick.
        self.sync_control(cx);
        self.persist();
    }

    fn member_name(&self, team: Option<Uuid>, user: Uuid) -> String {
        if Some(user) == self.cloud_me() {
            return t("you").to_string();
        }
        team.and_then(|id| self.cloud.teams.iter().find(|t| t.id == id))
            .and_then(|tv| tv.members.iter().find(|m| m.user_id == user))
            .map(|m| {
                if m.name.is_empty() {
                    m.email.clone()
                } else {
                    m.name.clone()
                }
            })
            .unwrap_or_else(|| t("a teammate").to_string())
    }

    // --- automatic progress -------------------------------------------------

    /// Every second: note what task-linked panes are doing. A status change
    /// (working, waiting for input, finished a turn) and each OSC 7501 report
    /// with a progress or message become notes; working and waiting also move
    /// the task to In progress or Blocked. Nothing marks a task done but the
    /// user or the agent saying so.
    pub(super) fn tick_task_links(&mut self, cx: &mut Context<Self>) {
        if self.workspace.task_links.is_empty() || !self.tasks_available() {
            return;
        }
        let live: HashSet<Uuid> = self.workspace.instances.iter().map(|i| i.id).collect();
        let before = self.workspace.task_links.len();
        self.workspace
            .task_links
            .retain(|iid, _| live.contains(iid));
        if self.workspace.task_links.len() != before {
            self.tasks_ui.seen.retain(|iid, _| live.contains(iid));
            self.persist();
        }
        let links: Vec<(Uuid, Uuid)> = self
            .workspace
            .task_links
            .iter()
            .map(|(i, t)| (*i, *t))
            .collect();
        for (iid, task_id) in links {
            let Some(view) = self.terminals.get(&iid) else {
                continue;
            };
            let view = view.read(cx);
            let status = view.status();
            let report = view.session().program_progress();
            let agent = self
                .workspace
                .instance(iid)
                .map(|i| i.program.clone().unwrap_or_else(|| i.title.clone()))
                .unwrap_or_else(|| "agent".to_string());
            let revision = report.as_ref().map_or(0, |r| r.0);
            let Some((last_status, last_rev)) = self.tasks_ui.seen.insert(iid, (status, revision))
            else {
                continue; // first sight: nothing has changed yet
            };
            let Some((_, task)) = self.find_task(task_id) else {
                continue;
            };
            if !task.status.is_open() {
                continue;
            }
            if status != last_status {
                let state = match status {
                    muxel_terminal::AgentStatus::Working => "working",
                    muxel_terminal::AgentStatus::Blocked => "blocked",
                    muxel_terminal::AgentStatus::Done => "done",
                    muxel_terminal::AgentStatus::Idle => "idle",
                };
                if let Some((text, next)) = mtasks::status_note(&agent, state) {
                    let _ = self.task_add_note(task_id, text, Some(agent.clone()), None, cx);
                    if let Some(next) = next.filter(|n| *n != task.status) {
                        let _ = self.try_edit_task(task_id, cx, move |t, actor, now| {
                            mtasks::set_status(t, next, actor, now)
                        });
                    }
                }
            }
            if revision != last_rev
                && let Some((_, percent, msg)) = report
                && (percent.is_some() || msg.is_some())
            {
                let text = msg.unwrap_or_else(|| format!("{}%", percent.unwrap_or(0)));
                let _ = self.task_add_note(task_id, text, Some(agent), percent, cx);
            }
        }
    }

    // --- muxel ctl task -----------------------------------------------------

    /// The tasks `ctl task` can see: personal ones and the bound team's.
    fn ctl_tasks(&self) -> Vec<(Option<Uuid>, &Task)> {
        let mut out: Vec<(Option<Uuid>, &Task)> = self
            .cloud
            .tasks
            .personal
            .iter()
            .map(|t| (None, t))
            .collect();
        if let Some(team) = self.tasks_team()
            && let Some(list) = self.cloud.tasks.teams.get(&team.id)
        {
            let grants = team.grants();
            let actor = Actor {
                me: self.cloud_me(),
                grants: Some(&grants),
            };
            out.extend(
                list.iter()
                    .filter(|t| actor.can_see(t))
                    .map(|t| (Some(team.id), t)),
            );
        }
        out
    }

    fn ctl_resolve_task(&self, query: &str) -> Result<Uuid, ctl_types::Error> {
        let q = query.trim().to_ascii_lowercase().replace('-', "");
        if q.len() < 4 {
            return Err("give at least the first 4 characters of the task id".into());
        }
        let hits: Vec<Uuid> = self
            .ctl_tasks()
            .iter()
            .map(|(_, t)| t.id)
            .filter(|id| id.simple().to_string().starts_with(&q))
            .collect();
        match hits.as_slice() {
            [id] => Ok(*id),
            [] => Err(format!("no task matches '{query}' (see `muxel ctl task list`)").into()),
            _ => Err(format!("'{query}' matches several tasks; give more of the id").into()),
        }
    }

    fn ctl_task_json(&self, team: Option<Uuid>, task: &Task, notes: usize) -> Value {
        let team_name = team
            .and_then(|id| self.cloud.teams.iter().find(|t| t.id == id))
            .map(|t| t.name.clone());
        let notes: Vec<Value> = task
            .progress
            .iter()
            .rev()
            .take(notes)
            .rev()
            .map(|n| {
                json!({
                    "at_ms": n.at_ms,
                    "by": n.by_agent.clone().or_else(|| n.by_user.map(|u| self.member_name(team, u))),
                    "text": n.text,
                    "percent": n.percent,
                })
            })
            .collect();
        json!({
            "id": task.id,
            "short_id": task.id.simple().to_string()[..8],
            "title": task.title,
            "body": task.body,
            "status": task.status.as_str(),
            "percent": task.percent,
            "assignee": task.assignee.map(|u| self.member_name(team, u)),
            "team": team_name,
            "project": task.project_key,
            "notes": notes,
        })
    }

    /// `muxel ctl task …`. With outside control off, only a pane started on
    /// a task may ask, and only about its own task.
    pub(super) fn control_task(
        &mut self,
        command: ctl_types::Command,
        caller: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> Result<Value, ctl_types::Error> {
        use ctl_types::Command as C;
        if !self.tasks_available() {
            return Err("tasks need muxel Cloud: sign in with a plan that includes sync".into());
        }
        let linked = caller.and_then(|c| self.workspace.task_links.get(&c).copied());
        let scope = mtasks::CtlTaskScope::of(self.settings.control_enabled, linked);
        let refuse = || -> ctl_types::Error {
            "outside control is off: a pane started on a task may only use `ctl task` on its own task".into()
        };
        if scope == mtasks::CtlTaskScope::Refused {
            return Err(refuse());
        }
        let check = |id: Uuid| {
            if scope.allows(id) {
                Ok(id)
            } else {
                Err(refuse())
            }
        };
        let shown = |this: &Self, id: Uuid| -> Value {
            this.ctl_tasks()
                .into_iter()
                .find(|(_, t)| t.id == id)
                .map_or(Value::Null, |(team, t)| this.ctl_task_json(team, t, 10))
        };
        match command {
            C::TaskList => {
                let tasks: Vec<Value> = self
                    .ctl_tasks()
                    .into_iter()
                    .filter(|(_, t)| t.status.is_open())
                    .filter(|(_, t)| scope.allows(t.id))
                    .map(|(team, t)| self.ctl_task_json(team, t, 1))
                    .collect();
                Ok(json!({ "tasks": tasks, "linked": linked }))
            }
            C::TaskShow { task } => {
                let id = check(self.ctl_resolve_task(&task)?)?;
                Ok(json!({ "task": shown(self, id) }))
            }
            C::TaskTake { task } => {
                let id = check(self.ctl_resolve_task(&task)?)?;
                self.try_edit_task(id, cx, mtasks::take)
                    .map_err(|e| ctl_types::Error::from(e.to_string()))?;
                Ok(json!({ "task": shown(self, id) }))
            }
            C::TaskProgress {
                task,
                text,
                percent,
                status,
            } => {
                let id = check(self.ctl_resolve_task(&task)?)?;
                let agent = caller
                    .and_then(|c| self.workspace.instances.iter().find(|i| i.id == c))
                    .map(|i| i.program.clone().unwrap_or_else(|| i.title.clone()));
                let status = status.as_deref().and_then(TaskStatus::parse);
                let text = if text.trim().is_empty() {
                    status
                        .map(|s| status_label(s).to_string())
                        .unwrap_or_default()
                } else {
                    text
                };
                if !text.is_empty() || percent.is_some() {
                    self.task_add_note(id, text, agent, percent, cx)
                        .map_err(|e| ctl_types::Error::from(e.to_string()))?;
                }
                if let Some(status) = status {
                    self.try_edit_task(id, cx, move |t, actor, now| {
                        mtasks::set_status(t, status, actor, now)
                    })
                    .map_err(|e| ctl_types::Error::from(e.to_string()))?;
                }
                Ok(json!({ "task": shown(self, id) }))
            }
            _ => Err("not a task command".into()),
        }
    }

    // --- rendering ----------------------------------------------------------

    pub(super) fn render_tasks_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, bg) = (theme.muted_foreground, theme.border, theme.background);
        let groups = self.task_groups();
        let team = self.tasks_team();
        let mut list = v_flex().w_full().gap_3().p_2();

        // The panel only shows with a plan; an overdue one syncs read-only, so say so where it's noticed.
        if self
            .cloud
            .me
            .as_ref()
            .is_some_and(|me| me.entitlements.read_only)
        {
            list = list.child(
                div()
                    .p_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.warning)
                    .text_xs()
                    .text_color(theme.warning)
                    .child(t("Payment overdue: tasks sync read-only until it's paid.")),
            );
        }

        if self.tasks_ui.adding {
            let mut add = v_flex()
                .gap_2()
                .p_2()
                .rounded(theme.radius)
                .border_1()
                .border_color(border)
                .child(Input::new(&self.tasks_ui.new_title));
            if let Some(tv) = team.filter(|tv| tv.grants().has(Permission::TasksCreate)) {
                let to_team = self.tasks_ui.add_to_team;
                add = add.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("task-new-team")
                                .xsmall()
                                .label(SharedString::from(tv.name.clone()))
                                .selected(to_team)
                                .on_click(cx.listener(|this, _, _w, cx| {
                                    this.tasks_ui.add_to_team = true;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("task-new-personal")
                                .xsmall()
                                .label(t("Just me"))
                                .selected(!to_team)
                                .on_click(cx.listener(|this, _, _w, cx| {
                                    this.tasks_ui.add_to_team = false;
                                    cx.notify();
                                })),
                        ),
                );
            }
            add = add.child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("task-new-add")
                            .small()
                            .primary()
                            .label(t("Add task"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.task_submit_new(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("task-new-cancel")
                            .small()
                            .ghost()
                            .label(t("Cancel"))
                            .on_click(cx.listener(|this, _, _w, cx| {
                                this.tasks_ui.adding = false;
                                cx.notify();
                            })),
                    ),
            );
            list = list.child(add);
        }

        let sections: [(SharedString, &Vec<PanelTask>); 4] = [
            (t("Mine"), &groups.mine),
            (t("In progress"), &groups.in_progress),
            (t("Not started"), &groups.not_started),
            (t("Done"), &groups.done),
        ];
        let mut any = false;
        for (title, rows) in sections {
            if rows.is_empty() {
                continue;
            }
            any = true;
            let mut section = v_flex().gap_1().child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .child(format!("{title} · {}", rows.len())),
            );
            for row in rows {
                section = section.child(self.render_task_row(row, cx));
            }
            list = list.child(section);
        }
        if !any && !self.tasks_ui.adding {
            list = list.child(div().text_sm().text_color(muted).p_2().child(t(
                "No tasks yet. Add one with +, and start an agent on it when you're ready.",
            )));
        }
        if team.is_some_and(|tv| tv.waiting_for_keys) {
            list = list.child(div().text_xs().text_color(muted).px_2().child(t(
                "Some team tasks aren't shown yet: waiting for a team admin's muxel to share the team's keys.",
            )));
        }

        // The team's activity, when this workspace works for a team whose
        // role may read it.
        let activity_team = self
            .bound_team()
            .filter(|tv| tv.grants().has(Permission::ActivityRead));
        let show_team = self.tasks_ui.show_team && activity_team.is_some();
        let body = match activity_team.filter(|_| show_team) {
            Some(tv) => self.render_team_activity(tv, cx),
            None => list.into_any_element(),
        };
        let team_toggle = activity_team.map(|_| {
            Button::new("tasks-team-view")
                .ghost()
                .xsmall()
                .label(t("Team"))
                .selected(show_team)
                .tooltip(t("What your team is working on"))
                .on_click(cx.listener(|this, _, _w, cx| {
                    this.tasks_ui.show_team = !this.tasks_ui.show_team;
                    cx.notify();
                }))
        });
        let header = h_flex()
            .w_full()
            .flex_none()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(match team {
                        Some(tv) => tf("Tasks · {team}", &[("team", &tv.name)]),
                        None => t("Tasks").to_string(),
                    }),
            )
            .child(div().flex_1())
            .children(team_toggle)
            .child(
                Button::new("task-new")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .tooltip(t("New task"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.tasks_ui.adding = true;
                        this.tasks_ui
                            .new_title
                            .update(cx, |s, cx| s.focus(window, cx));
                        cx.notify();
                    })),
            )
            .child(
                Button::new("task-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(t("Close"))
                    .on_click(cx.listener(|this, _, _w, cx| this.toggle_tasks(cx))),
            );

        v_flex()
            .size_full()
            .bg(bg)
            .border_l_1()
            .border_color(border)
            .child(header)
            .child(
                div()
                    .id("tasks-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .into_any_element()
    }

    /// The Team view: each teammate's devices, their shared projects, and
    /// what each agent there is doing. Old entries are dimmed.
    fn render_team_activity(&self, team: &TeamView, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, radius) = (theme.muted_foreground, theme.border, theme.radius);
        let (info, warning, success) = (theme.info, theme.warning, theme.success);
        let now = now_ms();
        // An entry without projects was retracted (its sharing turned off).
        let mut rows: Vec<&muxel_cloud::engine::ActivityView> = team
            .activity
            .iter()
            .filter(|a| !a.presence.projects.is_empty())
            .collect();
        rows.sort_by_key(|a| std::cmp::Reverse(a.presence.at_ms));
        let mut list = v_flex().w_full().gap_2().p_2();
        if rows.is_empty() {
            list = list.child(div().text_sm().text_color(muted).px_2().child(t(
                "Nobody on this team is sharing what they're working on right now. Each person turns it on in Settings → muxel Cloud.",
            )));
        }
        for a in rows {
            let stale =
                now.saturating_sub(a.presence.at_ms) > muxel_core::cloud::activity::STALE_AFTER_MS;
            let mut who = self.member_name(Some(team.id), a.user_id);
            if !a.presence.device_name.is_empty() {
                who = format!("{who} · {}", a.presence.device_name);
            }
            let when = chrono::DateTime::from_timestamp_millis(a.presence.at_ms as i64)
                .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
                .unwrap_or_default();
            let mut card = v_flex()
                .gap_1()
                .p_2()
                .rounded(radius)
                .border_1()
                .border_color(border)
                .opacity(if stale { 0.55 } else { 1.0 })
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(who))
                        .child(div().flex_1())
                        .child(div().text_xs().text_color(muted).child(when)),
                );
            if a.presence.projects.iter().all(|p| p.agents.is_empty()) {
                card = card.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(t("No agents running")),
                );
            }
            for p in a.presence.projects.iter().filter(|p| !p.agents.is_empty()) {
                card = card.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .pt_1()
                        .child(p.project_name.clone()),
                );
                for agent in &p.agents {
                    let color = match agent.status.as_str() {
                        "working" => info,
                        "blocked" => warning,
                        "done" => success,
                        _ => muted,
                    };
                    let mut detail = vec![agent.status.clone()];
                    detail.extend(agent.branch.clone());
                    let mut line = v_flex().child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().size(px(7.0)).rounded_full().bg(color))
                            .child(div().text_sm().child(agent.program.clone()))
                            .child(div().text_xs().text_color(muted).child(detail.join(" · "))),
                    );
                    if let Some(title) = &agent.task_title {
                        line = line.child(
                            div()
                                .pl(px(15.0))
                                .text_xs()
                                .child(tf("On: {task}", &[("task", title)])),
                        );
                    }
                    card = card.child(line);
                }
            }
            list = list.child(card);
        }
        list.into_any_element()
    }

    fn render_task_row(&self, row: &PanelTask, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, hover, radius) = (
            theme.muted_foreground,
            theme.border,
            theme.list_hover,
            theme.radius,
        );
        let task = &row.task;
        let id = task.id;
        let selected = self.tasks_ui.selected == Some(id);
        let dot = match task.status {
            TaskStatus::InProgress => theme.info,
            TaskStatus::Blocked => theme.warning,
            TaskStatus::Done => theme.success,
            TaskStatus::Cancelled | TaskStatus::Todo => muted,
        };
        let mut meta: Vec<String> = vec![status_label(task.status).to_string()];
        if let Some(p) = task.percent.filter(|_| task.status.is_open()) {
            meta.push(format!("{p}%"));
        }
        if row.team.is_some() {
            meta.push(match task.assignee {
                Some(u) => self.member_name(row.team, u),
                None => t("unassigned").to_string(),
            });
        }
        let last_note = task.progress.last().map(|n| n.text.clone());

        // Only the header toggles the row: clicks on the expanded part (the note input, the action buttons)
        // would otherwise bubble up to it and collapse the row under the cursor.
        let mut header = v_flex()
            .id(SharedString::from(format!("task-head-{id}")))
            .w_full()
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _w, cx| {
                this.tasks_ui.selected = if this.tasks_ui.selected == Some(id) {
                    None
                } else {
                    Some(id)
                };
                cx.notify();
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size(px(8.0)).flex_none().rounded_full().bg(dot))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(if task.status.is_open() {
                                theme.foreground
                            } else {
                                muted
                            })
                            .child(task.title.clone()),
                    ),
            )
            .child(div().text_xs().text_color(muted).child(meta.join(" · ")));
        if !selected && let Some(note) = last_note {
            header = header.child(div().text_xs().text_color(muted).truncate().child(note));
        }
        let mut item = v_flex()
            .id(SharedString::from(format!("task-{id}")))
            .w_full()
            .p_2()
            .gap_1()
            .rounded(radius)
            .hover(move |s| s.bg(hover))
            .child(header);
        if !selected {
            return item.into_any_element();
        }

        // Expanded: details, history, actions.
        item = item.bg(theme.list_active).border_1().border_color(border);
        if !task.body.trim().is_empty() {
            item = item.child(div().text_xs().child(task.body.clone()));
        }
        for note in task.progress.iter().rev().take(5) {
            let who = match (&note.by_agent, note.by_user) {
                (Some(agent), _) => agent.clone(),
                (None, Some(u)) => self.member_name(row.team, u),
                (None, None) => t("you").to_string(),
            };
            let when = chrono::DateTime::from_timestamp_millis(note.at_ms as i64)
                .map(|d| {
                    d.with_timezone(&chrono::Local)
                        .format("%b %-d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            item = item.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("{when} · {who}: {}", note.text)),
            );
        }
        let grants: Option<Grants> = row.team.map(|team| {
            self.cloud
                .teams
                .iter()
                .find(|t| t.id == team)
                .map(TeamView::grants)
                .unwrap_or_default()
        });
        let actor = Actor {
            me: self.cloud_me(),
            grants: grants.as_ref(),
        };
        let can_update = actor.can_update(task);
        let mut actions = h_flex().gap_1().flex_wrap();
        if row.team.is_some() && actor.can_take(task) {
            actions = actions.child(
                Button::new("task-take")
                    .xsmall()
                    .primary()
                    .label(t("Take"))
                    .on_click(cx.listener(move |this, _, _w, cx| this.task_take(id, cx))),
            );
        }
        if task.status.is_open() && can_update {
            actions = actions.child(
                Button::new("task-agent")
                    .xsmall()
                    .label(t("Start with agent"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.task_start_with_agent(id, window, cx)
                    })),
            );
            let (label, next) = if task.status == TaskStatus::Blocked {
                (t("Unblock"), TaskStatus::InProgress)
            } else {
                (t("Blocked"), TaskStatus::Blocked)
            };
            actions = actions
                .child(
                    Button::new("task-block")
                        .xsmall()
                        .ghost()
                        .label(label)
                        .on_click(
                            cx.listener(move |this, _, _w, cx| this.task_set_status(id, next, cx)),
                        ),
                )
                .child(
                    Button::new("task-done")
                        .xsmall()
                        .ghost()
                        .label(t("Done"))
                        .on_click(cx.listener(move |this, _, _w, cx| {
                            this.task_set_status(id, TaskStatus::Done, cx)
                        })),
                )
                .child(
                    Button::new("task-cancel")
                        .xsmall()
                        .ghost()
                        .label(t("Close"))
                        .tooltip(t("Close without finishing it (Cancelled)"))
                        .on_click(cx.listener(move |this, _, _w, cx| {
                            this.task_set_status(id, TaskStatus::Cancelled, cx)
                        })),
                );
        } else if can_update {
            actions = actions.child(
                Button::new("task-reopen")
                    .xsmall()
                    .ghost()
                    .label(t("Reopen"))
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        this.task_set_status(id, TaskStatus::Todo, cx)
                    })),
            );
        }
        let team_view = row
            .team
            .and_then(|team| self.cloud.teams.iter().find(|t| t.id == team));
        if let Some(tv) = team_view.filter(|_| actor.can_assign_others()) {
            let open = self.tasks_ui.assigning == Some(id);
            actions = actions.child(
                Button::new("task-assign")
                    .xsmall()
                    .ghost()
                    .label(t("Assign…"))
                    .selected(open)
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        this.tasks_ui.assigning = if this.tasks_ui.assigning == Some(id) {
                            None
                        } else {
                            Some(id)
                        };
                        this.tasks_ui.confirm_delete = None;
                        cx.notify();
                    })),
            );
            if open {
                // Only members who could see the task once it's theirs.
                let eligible = tv.members.iter().filter(|m| {
                    let g = m.grants();
                    g.has(Permission::TasksReadAll) || g.has(Permission::TasksReadAssigned)
                });
                let mut picker = h_flex().gap_1().flex_wrap().child(
                    Button::new("task-assign-none")
                        .xsmall()
                        .label(t("Unassigned"))
                        .selected(task.assignee.is_none())
                        .on_click(
                            cx.listener(move |this, _, _w, cx| this.task_assign(id, None, cx)),
                        ),
                );
                for m in eligible {
                    let user = m.user_id;
                    picker = picker.child(
                        Button::new(SharedString::from(format!("task-assign-{user}")))
                            .xsmall()
                            .label(SharedString::from(self.member_name(row.team, user)))
                            .selected(task.assignee == Some(user))
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                this.task_assign(id, Some(user), cx)
                            })),
                    );
                }
                item = item.child(
                    v_flex()
                        .gap_1()
                        .child(div().text_xs().text_color(muted).child(t("Assign to")))
                        .child(picker),
                );
            }
        }
        if actor.can_delete(task) {
            actions = actions.child(
                Button::new("task-delete")
                    .xsmall()
                    .ghost()
                    .label(t("Delete"))
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        this.tasks_ui.confirm_delete = Some(id);
                        this.tasks_ui.assigning = None;
                        cx.notify();
                    })),
            );
            if self.tasks_ui.confirm_delete == Some(id) {
                let warning = if row.team.is_some() {
                    t("Delete this task for everyone on the team? This can't be undone.")
                } else {
                    t("Delete this task from all your computers? This can't be undone.")
                };
                item = item.child(
                    v_flex()
                        .gap_1()
                        .p_2()
                        .rounded(radius)
                        .border_1()
                        .border_color(theme.danger)
                        .child(div().text_xs().child(warning))
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("task-delete-confirm")
                                        .xsmall()
                                        .danger()
                                        .label(t("Delete for good"))
                                        .on_click(cx.listener(move |this, _, _w, cx| {
                                            this.task_delete(id, cx)
                                        })),
                                )
                                .child(
                                    Button::new("task-delete-keep")
                                        .xsmall()
                                        .ghost()
                                        .label(t("Keep it"))
                                        .on_click(cx.listener(|this, _, _w, cx| {
                                            this.tasks_ui.confirm_delete = None;
                                            cx.notify();
                                        })),
                                ),
                        ),
                );
            }
        }
        if can_update {
            item = item.child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.tasks_ui.note).small()),
                    )
                    .child(
                        Button::new("task-note-add")
                            .xsmall()
                            .label(t("Note"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.task_submit_note(id, window, cx)
                            })),
                    ),
            );
        }
        item = item.child(actions);
        item.into_any_element()
    }
}
