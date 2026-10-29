//! A hidden terminal keeps its launch alive until Undo or explicit finalization.

use super::*;
use muxel_core::control::AgentState;
use muxel_core::recoverable_close::{
    CloseDecision, CloseEvidence, CloseFacts, CloseStage, HeldLaunch, HeldView, UndoLaunch, decide,
    departure_finalizes, exit_proves_closed, expired_close_may_finish, held_launch, may_finalize,
    rehide_held, session_may_outlive, shell_close_evidence, undo_launch,
};
use muxel_terminal::TerminalStatusReason;
use std::borrow::Cow;

pub(super) const CLOSE_GRACE: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
pub(super) enum CloseDeparture {
    Quit,
    Workspace(Uuid),
}

pub(super) struct HeldClose {
    pub group: Uuid,
    pub project: Uuid,
    pub facts: CloseFacts,
    pub forced: bool,
    pub launch: HeldLaunch,
    pub restart_lease: Option<RestartLease>,
}

pub(super) struct CloseGroup {
    pub id: Uuid,
    pub members: Vec<Uuid>,
    pub deadline: Instant,
    pub undo_until: i64,
    pub needs_decision: bool,
    pub checking: bool,
    pub undoing: bool,
    /// Past the deadline: each layout-shared project's read mark then. The
    /// close waits for a later read of each (`expired_close_may_finish`).
    pub sync_marks: Option<Vec<(Uuid, u64)>>,
    pub layouts: HashMap<Uuid, Option<PaneNode>>,
    pub focus: Option<Uuid>,
    pub caller: Option<String>,
}

impl CloseGroup {
    /// Past its deadline, waiting to read what peers did to the shared
    /// layout. Leaving the workspace still finishes it as a running countdown.
    pub fn syncing(&self) -> bool {
        self.sync_marks.is_some() && !self.checking && !self.needs_decision && !self.undoing
    }

    pub fn stage(&self) -> CloseStage {
        if self.undoing {
            CloseStage::Undoing
        } else if self.needs_decision {
            CloseStage::AwaitingDecision
        } else if self.checking {
            CloseStage::Checking
        } else {
            CloseStage::CountingDown
        }
    }
}

/// The close evidence behind an agent's status badge. A program's own report
/// is provider-owned, like a hook; `decide` still treats only an Idle or Done
/// report as quiet. An error report ends the turn without saying the program
/// is idle, so it asks.
fn reason_close_evidence(reason: TerminalStatusReason) -> CloseEvidence {
    match reason {
        TerminalStatusReason::ProcessExit => CloseEvidence::Process,
        TerminalStatusReason::ProgramStatus(_) => CloseEvidence::Program,
        TerminalStatusReason::ProgramStatusError => CloseEvidence::Quiet,
        TerminalStatusReason::ProviderHook => CloseEvidence::Hook,
        TerminalStatusReason::ProviderTitle => CloseEvidence::Title,
        TerminalStatusReason::ProviderScreen => CloseEvidence::Screen,
        TerminalStatusReason::BlockedMarker | TerminalStatusReason::WorkingMarker => {
            CloseEvidence::Output
        }
        TerminalStatusReason::Bell => CloseEvidence::Bell,
        TerminalStatusReason::RecentOutput => CloseEvidence::Output,
        TerminalStatusReason::Quiet => CloseEvidence::Quiet,
    }
}

pub(super) struct ClientCount {
    target: SessionTarget,
    count: Option<usize>,
}

impl MuxelApp {
    pub(super) fn is_closing(&self, iid: Uuid) -> bool {
        self.closing.contains_key(&iid)
    }

    pub(super) fn closing_until(&self, iid: Uuid) -> Option<i64> {
        let held = self.closing.get(&iid)?;
        self.close_groups
            .iter()
            .find(|g| g.id == held.group)
            .map(|g| g.undo_until)
    }

    pub(super) fn project_has_closes(&self, pid: Uuid) -> bool {
        self.closing.values().any(|held| held.project == pid)
    }

    fn close_tmux_site(&self, iid: Uuid) -> Option<(integrations::RepoLoc, SessionTarget)> {
        let inst = self.workspace.instance(iid)?;
        if inst.literal_argv {
            return None;
        }
        let name = inst.tmux_session.as_ref()?;
        let project = self.workspace.project(inst.project_id)?;
        let loc = self.repo_loc(project.id)?;
        Some((
            loc,
            SessionTarget::new(name.clone(), inst.tmux_socket.clone()),
        ))
    }

    /// Whether closing this pane kills a tmux session, here or on its host,
    /// which may still be running the agent whatever the local client shows.
    /// The same panes `teardown_closed_instance` kills or ledgers a session for.
    fn close_session_may_outlive(&self, inst: &Instance) -> bool {
        let remote_unix = match self.workspace.project(inst.project_id) {
            Some(project) if project.is_remote() => Some(
                self.remote_host_for_instance(inst.id)
                    .is_some_and(|host| !host.os.is_windows()),
            ),
            Some(_) => None,
            None => return false,
        };
        session_may_outlive(inst.literal_argv, remote_unix, inst.tmux_session.is_some())
    }

    pub(super) fn close_facts(&self, iid: Uuid, cx: &App) -> CloseFacts {
        let inst = self.workspace.instance(iid);
        let mut state = inst
            .and_then(|inst| self.control_state(inst, cx))
            .unwrap_or(AgentState::NotRunning);
        let view = self.terminal_view(iid).map(|view| view.read(cx));
        let snapshot = view.map(TerminalView::runtime_snapshot);
        let attributable = view.zip(snapshot).is_some_and(|(view, snapshot)| {
            snapshot.is_attributable_to(view.session().binding_revision())
        });
        let evidence = if let Some(view) = view {
            if !muxel_core::is_known_agent_program(inst.and_then(|inst| inst.program.as_deref())) {
                shell_close_evidence(
                    inst.and_then(|inst| inst.program.as_deref()),
                    inst.is_some_and(|inst| {
                        inst.tmux_session.is_some()
                            || self
                                .workspace
                                .project(inst.project_id)
                                .is_some_and(Project::is_remote)
                    }),
                    cfg!(unix) && view.session().is_idle_foreground(),
                    view.session().shell_at_prompt(),
                )
            } else if !attributable {
                CloseEvidence::Settling
            } else {
                reason_close_evidence(view.runtime_snapshot().reason)
            }
        } else {
            CloseEvidence::Settling
        };
        if evidence == CloseEvidence::ShellPrompt && state != AgentState::Exited {
            state = AgentState::Idle;
        }
        let awaiting_reply = inst.is_some_and(|inst| {
            self.control_turns.get(&iid).is_some_and(|turn| {
                turn.awaiting_reply(
                    Some(state),
                    &inst.activity,
                    chrono::Local::now().timestamp_millis(),
                )
            })
        });
        let another_tmux_client = self.close_tmux_site(iid).is_some_and(|(_, target)| {
            self.close_client_counts
                .get(&iid)
                .filter(|count| count.target == target)
                .and_then(|count| count.count)
                .is_none_or(|count| count > 1)
        });
        CloseFacts {
            status: state,
            evidence,
            attributable: attributable || evidence == CloseEvidence::ShellPrompt,
            awaiting_reply,
            another_tmux_client,
            session_may_outlive: inst.is_some_and(|inst| self.close_session_may_outlive(inst)),
            launch_id: view.map(|view| view.session().id),
            binding_rev: view.map_or(0, |view| view.session().binding_revision()),
            input_rev: view.map_or(0, |view| view.session().input_rev()),
            completion_rev: view.map_or(0, TerminalView::launch_completion_revision),
        }
    }

    /// Query real client counts off the UI thread. A failed probe is busy/unknown.
    pub(super) fn request_recoverable_close(&mut self, ids: Vec<Uuid>, cx: &mut Context<Self>) {
        let ids: Vec<_> = ids
            .into_iter()
            .filter(|id| !self.is_closing(*id) && self.workspace.instance(*id).is_some())
            .collect();
        if ids.is_empty() {
            return;
        }
        let jobs: Vec<_> = ids
            .iter()
            .filter_map(|&id| {
                self.close_tmux_site(id)
                    .map(|(loc, target)| (id, loc, target))
            })
            .collect();
        if jobs.is_empty() {
            self.plan_close_request(ids, cx);
            return;
        }
        let token = Uuid::new_v4();
        for &iid in &ids {
            self.close_requests.insert(iid, token);
        }
        cx.notify();
        let workspace = self.current_workspace;
        cx.spawn(async move |this, cx| {
            let counts = close_probes::collect(
                cx.background_executor().clone(),
                jobs,
                |(iid, loc, target)| async move {
                    let count = integrations::attached_tmux_clients(&loc, &target).ok();
                    (iid, ClientCount { target, count })
                },
            )
            .await;
            let _ = this.update(cx, |this, cx| {
                // A member closed another way, or taken over by a newer request,
                // leaves this batch; the others still get their decision. Cancel
                // and workspace switch clear every token, so nothing remains.
                let current: Vec<_> = ids
                    .into_iter()
                    .filter(|iid| this.close_requests.get(iid) == Some(&token))
                    .collect();
                for iid in &current {
                    this.close_requests.remove(iid);
                }
                cx.notify();
                if this.current_workspace != workspace || current.is_empty() {
                    return;
                }
                this.close_client_counts.extend(counts);
                this.plan_close_request(current, cx);
            });
        })
        .detach();
    }

    fn plan_close_request(&mut self, ids: Vec<Uuid>, cx: &mut Context<Self>) {
        if let Some(&iid) = ids.iter().find(|&&iid| {
            self.close_editor(iid)
                .is_some_and(|editor| editor.read(cx).is_dirty())
        }) {
            self.request_confirm(
                t("Save changes before closing?"),
                t("Save your changes, or close without saving."),
                t("Save"),
                ConfirmAction::SaveEditorBeforeClose { iid, rest: ids },
                cx,
            );
            return;
        }
        let Some(ids) = self.preflight_close_worktrees(ids, cx) else {
            return;
        };
        let busy: Vec<_> = ids
            .iter()
            .filter(|&&iid| {
                self.workspace
                    .instance(iid)
                    .is_some_and(|inst| inst.kind == InstanceKind::Terminal)
                    && decide(
                        self.close_facts(iid, cx),
                        self.settings.confirm_close_terminal,
                    ) == CloseDecision::Ask
            })
            .copied()
            .collect();
        if !busy.is_empty() {
            let message = busy
                .iter()
                .map(|&iid| {
                    let inst = self.workspace.instance(iid).unwrap();
                    let facts = self.close_facts(iid, cx);
                    if facts.session_may_outlive
                        && matches!(
                            facts.status,
                            AgentState::Exited | AgentState::NotRunning | AgentState::Failed
                        )
                    {
                        return tf(
                            "{name}'s tmux session may still be running.",
                            &[("name", inst.display_name())],
                        );
                    }
                    let state = match facts.status {
                        AgentState::Working => t("working"),
                        AgentState::Idle | AgentState::Done => {
                            return tf(
                                "Muxel cannot confirm that {name} is idle.",
                                &[("name", inst.display_name())],
                            );
                        }
                        AgentState::Blocked => t("waiting for input"),
                        AgentState::Exited => t("exited"),
                        AgentState::Starting => t("starting"),
                        AgentState::NotRunning => t("not running"),
                        AgentState::Failed => t("unable to start"),
                    };
                    tf(
                        "{name} is {status}.",
                        &[("name", inst.display_name()), ("status", state.as_ref())],
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let label = if ids.len() > 1 {
                t("Close all")
            } else {
                t("Close")
            };
            self.request_confirm(
                t("Close terminals?"),
                message,
                label,
                ConfirmAction::CloseBatch(ids),
                cx,
            );
            return;
        }
        self.accept_close_request(ids, false, cx);
    }

    pub(super) fn accept_close_request(
        &mut self,
        ids: Vec<Uuid>,
        idle_only: bool,
        cx: &mut Context<Self>,
    ) {
        let mut holds = Vec::new();
        for iid in ids {
            let Some(inst) = self.workspace.instance(iid) else {
                continue;
            };
            if self.is_closing(iid) {
                continue;
            }
            if inst.kind != InstanceKind::Terminal {
                if !self
                    .close_editor(iid)
                    .is_some_and(|editor| editor.read(cx).is_dirty())
                {
                    self.close_instance_inner(iid, "close", cx);
                }
                continue;
            }
            let facts = self.close_facts(iid, cx);
            match decide(facts, idle_only || self.settings.confirm_close_terminal) {
                CloseDecision::Immediate => self.close_instance_inner(iid, "close", cx),
                CloseDecision::Hold { forced } => holds.push((iid, forced)),
                CloseDecision::Ask if !idle_only => holds.push((iid, true)),
                CloseDecision::Ask => {}
            }
        }
        self.hold_closes(holds, None, cx);
    }

    pub(super) fn hold_closes(
        &mut self,
        ids: Vec<(Uuid, bool)>,
        caller: Option<String>,
        cx: &mut Context<Self>,
    ) -> Option<Uuid> {
        let durable = self.workspace_with_closes_restored();
        let mut group = CloseGroup {
            id: Uuid::new_v4(),
            members: Vec::new(),
            deadline: Instant::now() + CLOSE_GRACE,
            undo_until: chrono::Local::now().timestamp_millis() + CLOSE_GRACE.as_millis() as i64,
            needs_decision: false,
            checking: false,
            undoing: false,
            sync_marks: None,
            layouts: HashMap::new(),
            focus: self.active_instance,
            caller,
        };
        for (iid, forced) in ids {
            if self.is_closing(iid) {
                continue;
            }
            let Some(inst) = self.workspace.instance(iid) else {
                continue;
            };
            let pid = inst.project_id;
            let facts = self.close_facts(iid, cx);
            // An attach in flight is not a restart: Undo attaches again.
            let launch = held_launch(
                self.terminal_restarts.contains(iid),
                self.terminal_launching
                    .get(&iid)
                    .map(|ticket| ticket.reason),
            );
            let restart_lease = self.terminal_restarts.lease(iid);
            group.layouts.entry(pid).or_insert_with(|| {
                durable
                    .project(pid)
                    .and_then(|project| project.layout.clone())
            });
            if let Some(popout) = self.popouts.get(&iid) {
                Self::restore_popout_layout(
                    group.layouts.get_mut(&pid).unwrap(),
                    iid,
                    popout.redock,
                );
            }
            // Publish inert membership before removing any layout/window ownership.
            self.closing.insert(
                iid,
                HeldClose {
                    group: group.id,
                    project: pid,
                    facts,
                    forced,
                    launch,
                    restart_lease,
                },
            );
            group.members.push(iid);
        }
        if group.members.is_empty() {
            return None;
        }
        let gid = group.id;
        let members = group.members.clone();
        self.close_groups.push(group);
        for iid in members {
            self.close_requests.remove(&iid);
            self.terminal_restarts.cancel(iid);
            self.terminal_launching.remove(&iid);
            self.bump_session_binding_generation(iid);
            self.clear_notifications_for(iid);
            self.readings.queue.retain(|&queued| queued != iid);
            self.stop_reading(iid, cx);
            if let Some(view) = self.terminal_view(iid) {
                view.read(cx).session().hold_input();
            }
            self.cancel_pending_control_startup(iid);
            self.release_maximize(&[iid]);
            if let Some(held) = self.closing.get(&iid)
                && let Some(project) = self.workspace.project_mut(held.project)
            {
                remove(&mut project.layout, iid);
            }
            if let Some(popout) = self.popouts.remove(&iid) {
                if let PaneView::Terminal(view) = popout.view {
                    self.terminals.insert(iid, view);
                }
                let handle = popout.window;
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |_, window, _| window.remove_window());
                });
            }
            if self.active_instance == Some(iid) {
                self.active_instance = self.workspace.active().and_then(Project::first_instance);
            }
        }
        self.persist();
        cx.notify();
        Some(gid)
    }

    fn restore_popout_layout(layout: &mut Option<PaneNode>, iid: Uuid, anchor: RedockAnchor) {
        match anchor {
            RedockAnchor::Tab { sibling, index } if add_tab_at(layout, sibling, iid, index) => {}
            RedockAnchor::Split {
                anchor,
                dir,
                before,
            } if split_beside(layout, anchor, dir, iid, before) => {}
            _ => {
                if let Some(anchor) = layout.as_ref().and_then(PaneNode::first_instance) {
                    split_beside(layout, anchor, SplitDirection::Horizontal, iid, false);
                } else {
                    *layout = Some(PaneNode::leaf(iid));
                }
            }
        }
    }

    pub(super) fn workspace_with_closes_restored(&self) -> Workspace {
        let mut workspace = self.workspace.clone();
        for project in &mut workspace.projects {
            self.restore_held_layout(project.id, &mut project.layout);
        }
        workspace
    }

    /// Put every held pane of project `pid` back into `layout`, where the
    /// durable document keeps it until its close is final.
    fn restore_held_layout(&self, pid: Uuid, layout: &mut Option<PaneNode>) {
        for group in self.close_groups.iter().rev() {
            let Some(original) = group.layouts.get(&pid) else {
                continue;
            };
            let ids: HashSet<_> = group
                .members
                .iter()
                .filter(|iid| {
                    self.closing.contains_key(iid) && self.workspace.instance(**iid).is_some()
                })
                .copied()
                .collect();
            recoverable_layout::restore_layout(layout, original, &ids);
        }
    }

    /// The project as the saved and shared documents hold it: a held pane is
    /// still in its place until its close is final, so nothing a peer sees
    /// changes while it can be undone.
    pub(super) fn durable_project(&self, pid: Uuid) -> Option<Cow<'_, Project>> {
        let project = self.workspace.project(pid)?;
        if !self.project_has_closes(pid) {
            return Some(Cow::Borrowed(project));
        }
        let mut project = project.clone();
        self.restore_held_layout(pid, &mut project.layout);
        Some(Cow::Owned(project))
    }

    /// The layout document layout sync compares and pushes for `pid`.
    pub(super) fn sync_capture(&self, pid: Uuid, now: u64) -> Option<RemoteLayout> {
        let project = self.durable_project(pid)?;
        Some(RemoteLayout::capture(&project, &self.workspace, now))
    }

    fn held_in_project(&self, pid: Uuid) -> Vec<Uuid> {
        self.close_groups
            .iter()
            .flat_map(|group| &group.members)
            .copied()
            .filter(|iid| {
                self.closing
                    .get(iid)
                    .is_some_and(|held| held.project == pid)
            })
            .collect()
    }

    /// Before a peer layout is adopted over project `pid`: show its held panes
    /// in the layout, still held, so the adoption treats them as the panes the
    /// shared document lists. [`Self::rehide_held_panes`] hides them again.
    pub(super) fn show_held_panes(&mut self, pid: Uuid) {
        let Some(layout) = self
            .durable_project(pid)
            .map(|project| project.layout.clone())
        else {
            return;
        };
        if let Some(project) = self.workspace.project_mut(pid) {
            project.layout = layout;
        }
    }

    /// After an adoption: hide the held panes the adopted layout still lists,
    /// and make it their place to come back to. A held pane it dropped was
    /// closed by the peer, and the adoption already dropped it here too.
    pub(super) fn rehide_held_panes(&mut self, pid: Uuid) {
        let held = self.held_in_project(pid);
        if held.is_empty() {
            return;
        }
        let Some(project) = self.workspace.project_mut(pid) else {
            return;
        };
        let durable = project.layout.clone();
        let gone = rehide_held(&mut project.layout, &held);
        for group in &mut self.close_groups {
            if let Some(original) = group.layouts.get_mut(&pid) {
                *original = durable.clone();
            }
        }
        for iid in gone {
            muxel_store::append_event_log(&format!("close: a peer closed held pane [{iid}]"));
            self.forget_close(iid);
        }
        if self.active_instance.is_some_and(|iid| self.is_closing(iid)) {
            self.active_instance = self.workspace.active().and_then(Project::first_instance);
        }
    }

    pub(super) fn undo_close(&mut self, gid: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_main_window(window) {
            if let Some(main) = self.main_window {
                let weak = cx.weak_entity();
                cx.defer(move |cx| {
                    let _ = main.update(cx, |_, window, cx| {
                        if let Some(app) = weak.upgrade() {
                            app.update(cx, |app, cx| app.undo_close(gid, window, cx));
                        }
                    });
                });
            }
            return;
        }
        let Some(index) = self.close_groups.iter().position(|group| group.id == gid) else {
            return;
        };
        if self.close_groups[index].undoing {
            return;
        }
        let leases: Vec<_> = self.close_groups[index]
            .members
            .iter()
            .filter_map(|iid| {
                self.closing
                    .get(iid)?
                    .restart_lease
                    .clone()
                    .filter(RestartLease::is_in_flight)
            })
            .collect();
        if !leases.is_empty() {
            self.close_groups[index].undoing = true;
            cx.spawn_in(window, async move |this, cx| {
                while leases.iter().any(RestartLease::is_in_flight) {
                    if !this
                        .read_with(cx, |app, _| {
                            app.close_groups
                                .iter()
                                .any(|group| group.id == gid && group.undoing)
                        })
                        .unwrap_or(false)
                    {
                        return;
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(20))
                        .await;
                }
                let _ = this.update_in(cx, |app, window, cx| {
                    if let Some(group) = app
                        .close_groups
                        .iter_mut()
                        .find(|group| group.id == gid && group.undoing)
                    {
                        group.undoing = false;
                        app.undo_close(gid, window, cx);
                    }
                });
            })
            .detach();
            cx.notify();
            return;
        }
        let (focus, restored) = self.release_close_group(index, cx);
        self.show_close_list = false;
        if let Some(iid) = focus {
            if let Some(inst) = self.workspace.instance(iid) {
                self.workspace.active_project = Some(inst.project_id);
            }
            self.focus_instance(iid, window, cx);
        }
        for (iid, held) in restored {
            let view = match self.terminal_view(iid) {
                Some(view) if !view.read(cx).exited() => HeldView::Running,
                Some(_) => HeldView::Ended,
                None if self.failed_launches.contains_key(&iid) => HeldView::Ended,
                None => HeldView::Missing,
            };
            match undo_launch(held.launch, view) {
                UndoLaunch::Resume => {}
                UndoLaunch::Launch(reason) => {
                    self.spawn_terminal_for(iid, reason, window, cx);
                    if focus == Some(iid) {
                        self.focus_instance(iid, window, cx);
                    }
                }
                UndoLaunch::Restart => self.restart_agent(iid, window, cx),
            }
        }
        self.persist();
        cx.notify();
    }

    /// End a group's hold: its panes go back where they were, take input again,
    /// and are no longer closing. Returns the pane to focus and what each held.
    fn release_close_group(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> (Option<Uuid>, Vec<(Uuid, HeldClose)>) {
        let group = self.close_groups.remove(index);
        let ids: HashSet<_> = group
            .members
            .iter()
            .filter(|iid| self.workspace.instance(**iid).is_some())
            .copied()
            .collect();
        let focus = group
            .focus
            .filter(|iid| ids.contains(iid))
            .or_else(|| group.members.iter().copied().find(|iid| ids.contains(iid)));
        for (&pid, original) in &group.layouts {
            if let Some(project) = self.workspace.project_mut(pid) {
                recoverable_layout::restore_layout(&mut project.layout, original, &ids);
            }
        }
        let mut restored = Vec::new();
        for iid in group.members {
            if let Some(held) = self.closing.remove(&iid) {
                // A pane brought back is no longer going away. A disposal
                // question that counted it must ask again before it acts.
                for pending in &mut self.pending_worktree_dispose {
                    pending.close_after.retain(|&member| member != iid);
                }
                if let Some(view) = self.terminal_view(iid) {
                    view.read(cx).session().resume_input();
                }
                if let Some(auto) = self.auto.get_mut(&iid) {
                    auto.rebaseline();
                }
                restored.push((iid, held));
            }
        }
        if self.close_groups.is_empty() {
            self.show_close_list = false;
        }
        (focus, restored)
    }

    pub(super) fn undo_latest_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.close_groups.last() {
            self.undo_close(group.id, window, cx);
        }
    }

    pub(super) fn expire_closes(&mut self, now: Instant, cx: &mut Context<Self>) {
        let due: Vec<_> = self
            .close_groups
            .iter()
            .filter(|g| !g.needs_decision && !g.checking && !g.undoing && now >= g.deadline)
            .map(|g| g.id)
            .collect();
        for gid in due {
            if !self.close_layout_synced(gid) {
                continue;
            }
            // A confirmed close is final at its deadline. It must not wait for
            // unrelated quiet members' remote client-count probes.
            let forced: Vec<_> = self
                .close_groups
                .iter()
                .find(|g| g.id == gid)
                .into_iter()
                .flat_map(|g| &g.members)
                .copied()
                .filter(|iid| self.closing.get(iid).is_some_and(|held| held.forced))
                .collect();
            for iid in forced {
                self.close_instance_inner(iid, "close (confirmed countdown)", cx);
            }
            let jobs: Vec<_> = self
                .close_groups
                .iter()
                .find(|g| g.id == gid)
                .into_iter()
                .flat_map(|g| g.members.iter())
                .filter_map(|&iid| {
                    self.closing.get(&iid).filter(|held| !held.forced)?;
                    if exit_proves_closed(self.close_facts(iid, cx)) {
                        return None;
                    }
                    self.close_tmux_site(iid)
                        .map(|(loc, target)| (iid, loc, target))
                })
                .collect();
            if jobs.is_empty() {
                self.finish_close_expiry(gid, cx);
                continue;
            }
            if let Some(group) = self.close_groups.iter_mut().find(|g| g.id == gid) {
                group.checking = true;
            }
            cx.spawn(async move |this, cx| {
                let counts = close_probes::collect(
                    cx.background_executor().clone(),
                    jobs,
                    |(iid, loc, target)| async move {
                        let count = integrations::attached_tmux_clients(&loc, &target).ok();
                        (iid, ClientCount { target, count })
                    },
                )
                .await;
                let _ = this.update(cx, |this, cx| {
                    if !this.close_groups.iter().any(|g| g.id == gid) {
                        return;
                    }
                    this.close_client_counts.extend(counts);
                    this.finish_close_expiry(gid, cx);
                });
            })
            .detach();
        }
        if !self.close_groups.is_empty() {
            cx.notify();
        }
    }

    /// Whether an expired group has read every layout-shared project it
    /// touches since its deadline. The first call marks where that read must
    /// begin and asks for a poll now.
    fn close_layout_synced(&mut self, gid: Uuid) -> bool {
        let Some(group) = self.close_groups.iter().find(|g| g.id == gid) else {
            return false;
        };
        let Some(marks) = &group.sync_marks else {
            let mut projects: Vec<Uuid> = group
                .members
                .iter()
                .filter_map(|iid| self.closing.get(iid).map(|held| held.project))
                .filter(|&pid| self.project_syncs_layout(pid))
                .collect();
            projects.sort_unstable();
            projects.dedup();
            let marks: Vec<_> = projects
                .into_iter()
                .map(|pid| {
                    (
                        pid,
                        self.layout_reads
                            .get(&pid)
                            .copied()
                            .unwrap_or_default()
                            .mark(),
                    )
                })
                .collect();
            let unshared = marks.is_empty();
            self.close_layout_poll |= !unshared;
            if let Some(group) = self.close_groups.iter_mut().find(|g| g.id == gid) {
                group.sync_marks = Some(marks);
            }
            return unshared;
        };
        // A project gone or no longer shared has nothing to overwrite.
        expired_close_may_finish(
            marks
                .iter()
                .filter(|(pid, _)| self.project_syncs_layout(*pid))
                .map(|&(pid, mark)| {
                    (
                        mark,
                        self.layout_reads.get(&pid).copied().unwrap_or_default(),
                    )
                }),
        )
    }

    fn finish_close_expiry(&mut self, gid: Uuid, cx: &mut Context<Self>) {
        let Some(group) = self.close_groups.iter().find(|g| g.id == gid) else {
            return;
        };
        if group.undoing {
            return;
        }
        let ready: Vec<_> = group
            .members
            .iter()
            .copied()
            .filter(|&iid| {
                self.workspace.instance(iid).is_none()
                    || self.closing.get(&iid).is_none_or(|held| {
                        let now = self.close_facts(iid, cx);
                        exit_proves_closed(now) || may_finalize(held.facts, now, held.forced)
                    })
            })
            .collect();
        for iid in ready {
            self.close_instance_inner(iid, "close (countdown)", cx);
        }
        if let Some(group) = self.close_groups.iter_mut().find(|g| g.id == gid) {
            group.needs_decision = true;
            group.checking = false;
        }
        cx.notify();
    }

    pub(super) fn finalize_close_group(&mut self, gid: Uuid, cx: &mut Context<Self>) {
        let Some(index) = self.close_groups.iter().position(|group| group.id == gid) else {
            return;
        };
        let members = self.close_groups[index].members.clone();
        for iid in members {
            self.close_instance_inner(iid, "close (countdown)", cx);
        }
        if self.close_groups.is_empty() {
            self.show_close_list = false;
        }
        self.persist();
        cx.notify();
    }

    /// Leaving the workspace: finish every running countdown, and keep the
    /// panes of a group still being checked, waiting for Close / Keep, or being
    /// restored. A kept pane is back in its layout, so the save reopens it.
    pub(super) fn finalize_all_closes(&mut self, cx: &mut Context<Self>) -> bool {
        let had_closes = !self.close_groups.is_empty();
        let was_preserving = self.preserve_close_worktrees;
        self.preserve_close_worktrees = true;
        let groups: Vec<_> = self
            .close_groups
            .iter()
            .map(|group| (group.id, departure_finalizes(group.stage())))
            .collect();
        for (gid, finalize) in groups {
            if finalize {
                self.finalize_close_group(gid, cx);
            } else if let Some(index) = self.close_groups.iter().position(|g| g.id == gid) {
                for (iid, _) in self.release_close_group(index, cx).1 {
                    if let Some(inst) = self.workspace.instance(iid) {
                        muxel_store::append_event_log(&format!(
                            "close: kept \"{}\" [{iid}], its close was not decided",
                            inst.display_name()
                        ));
                    }
                }
            }
        }
        self.preserve_close_worktrees = was_preserving;
        had_closes
    }

    /// The first held pane of project `pid` whose close is not just counting
    /// down. Removing the project would kill it before anyone decided.
    pub(super) fn project_close_undecided(&self, pid: Uuid) -> Option<Uuid> {
        self.close_groups
            .iter()
            .filter(|group| !departure_finalizes(group.stage()))
            .flat_map(|group| &group.members)
            .copied()
            .find(|iid| {
                self.closing
                    .get(iid)
                    .is_some_and(|held| held.project == pid)
            })
    }

    pub(super) fn finalize_project_closes(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        let members: Vec<_> = self
            .closing
            .iter()
            .filter(|(_, held)| held.project == pid)
            .map(|(&iid, _)| iid)
            .collect();
        for iid in members {
            self.close_instance_inner(iid, "close (project)", cx);
        }
    }

    pub(super) fn quit_after_closes(&mut self, cx: &mut Context<Self>) {
        self.finalize_all_closes(cx);
        // Save failures are reported, but must not make an explicit Quit a no-op.
        self.try_persist();
        self.close_tmux_kills.retain(|task| !task.is_ready());
        if self.close_tmux_kills.is_empty() {
            cx.quit();
        } else {
            self.begin_close_departure(CloseDeparture::Quit, cx);
        }
    }

    pub(super) fn begin_close_departure(&mut self, action: CloseDeparture, cx: &mut Context<Self>) {
        if !matches!(self.close_departure, Some(CloseDeparture::Quit)) {
            self.close_departure = Some(action);
        }
        if self.close_departure_task.is_some() {
            return;
        }
        self.close_departure_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(ready) = this.update(cx, |app, _| {
                    app.close_tmux_kills.retain(|task| !task.is_ready());
                    if !app.close_tmux_kills.is_empty() {
                        return None;
                    }
                    if let Some(task) = app.close_departure_task.take() {
                        task.detach();
                    }
                    Some(app.close_departure.take())
                }) else {
                    return;
                };
                if let Some(action) = ready {
                    match action {
                        Some(CloseDeparture::Quit) => {
                            let _ = this.update(cx, |app, cx| app.quit_after_closes(cx));
                        }
                        Some(CloseDeparture::Workspace(id)) => {
                            Self::in_project_or_main(&this, None, cx, |app, window, cx| {
                                app.enter_workspace(id, window, cx)
                            });
                        }
                        None => {}
                    }
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(20))
                    .await;
            }
        }));
        cx.notify();
    }

    pub(super) fn cancel_close_departure(&mut self) {
        if matches!(self.close_departure, Some(CloseDeparture::Workspace(_))) {
            self.close_departure = None;
        }
    }

    pub(super) fn close_editor(&self, iid: Uuid) -> Option<&Entity<EditorView>> {
        self.editors
            .get(&iid)
            .or_else(|| match &self.popouts.get(&iid)?.view {
                PaneView::Editor(editor) => Some(editor),
                _ => None,
            })
    }

    pub(super) fn forget_close(&mut self, iid: Uuid) {
        self.close_requests.remove(&iid);
        self.close_client_counts.remove(&iid);
        if self.closing.remove(&iid).is_some() {
            self.close_persist_pending = true;
        }
        for group in &mut self.close_groups {
            group.members.retain(|&member| member != iid);
        }
        self.close_groups.retain(|group| !group.members.is_empty());
        if self.close_groups.is_empty() {
            self.show_close_list = false;
        }
    }

    /// A worktree choice is the sole confirmation and finalizes before touching git.
    /// Returns the ids to plan here, or `None` once a disposal question owns them.
    pub(super) fn preflight_close_worktrees(
        &mut self,
        ids: Vec<Uuid>,
        cx: &mut Context<Self>,
    ) -> Option<Vec<Uuid>> {
        // A pane opened in a worktree after its question was shown is not part
        // of that answer. It gets its own prompt and countdown instead.
        let uncaptured: HashSet<_> = ids
            .iter()
            .copied()
            .filter(|&iid| {
                self.workspace
                    .instance(iid)
                    .and_then(|instance| instance.worktree_id)
                    .is_some_and(|wid| {
                        self.pending_worktree_dispose
                            .iter()
                            .any(|d| d.wid == wid && !d.close_after.contains(&iid))
                    })
            })
            .collect();
        // Repeating a close the question already covers joins it, and the rest
        // of that request waits for the answer, as it did the first time.
        if let Some(pending) = self
            .pending_worktree_dispose
            .iter_mut()
            .find(|d| ids.iter().any(|iid| d.close_after.contains(iid)))
        {
            for &iid in &ids {
                if !uncaptured.contains(&iid)
                    && !pending.close_after.contains(&iid)
                    && !pending.close_rest.contains(&iid)
                {
                    pending.close_rest.push(iid);
                }
            }
            cx.notify();
            let now: Vec<_> = ids
                .into_iter()
                .filter(|iid| uncaptured.contains(iid))
                .collect();
            return (!now.is_empty()).then_some(now);
        }
        let selected: HashSet<_> = ids.iter().copied().collect();
        let worktrees: Vec<_> = ids
            .iter()
            .filter(|iid| !uncaptured.contains(iid))
            .filter_map(|iid| self.workspace.instance(*iid)?.worktree_id)
            .collect();
        for wid in worktrees {
            let members = self.workspace.instances_using(wid);
            let member_ids: HashSet<_> = members.iter().copied().collect();
            if !members
                .iter()
                .all(|iid| selected.contains(iid) || self.is_closing(*iid))
            {
                continue;
            }
            let Some(worktree) = self.workspace.worktree(wid) else {
                continue;
            };
            let root = self
                .workspace
                .project(worktree.project_id)
                .map(|project| project.root_path.clone())
                .unwrap_or_default();
            let path = worktree.path.clone();
            let changed = integrations::worktree_change_count(&path);
            let unmerged = integrations::repo_head(&root).map_or(0, |base| {
                integrations::worktree_unmerged_count(&path, &base)
            });
            if changed == 0 && unmerged == 0 {
                // There is no disposal question to authorize killing a busy
                // terminal. Use the usual confirmation and recoverable hold.
                continue;
            }
            self.pending_worktree_dispose.push_back(WorktreeDispose {
                wid,
                name: worktree.name.clone(),
                color: worktree.color,
                path,
                branch: worktree.branch.clone(),
                changed,
                unmerged,
                base_label: integrations::repo_current_branch(&integrations::RepoLoc::Local(
                    root.clone(),
                ))
                .unwrap_or_else(|| "base".into()),
                root,
                close_rest: ids
                    .iter()
                    .copied()
                    .filter(|iid| !member_ids.contains(iid))
                    .collect(),
                close_after: members,
            });
            if let Some(main) = self.main_window {
                let _ = main.update(cx, |_, window, _| window.activate_window());
            }
            cx.notify();
            return None;
        }
        Some(ids)
    }

    /// A pane opened after the question was shown was never part of its answer.
    /// Re-plan the captured selection rather than touching that pane's worktree.
    pub(super) fn take_worktree_dispose(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<WorktreeDispose> {
        let choice = self.pending_worktree_dispose.pop_front()?;
        let captured: HashSet<_> = choice.close_after.iter().copied().collect();
        if self
            .workspace
            .instances_using(choice.wid)
            .iter()
            .any(|iid| !captured.contains(iid))
        {
            self.request_recoverable_close(
                choice
                    .close_after
                    .into_iter()
                    .chain(choice.close_rest)
                    .collect(),
                cx,
            );
            cx.notify();
            return None;
        }
        Some(choice)
    }

    pub(super) fn finalize_worktree_choice(&mut self, ids: &[Uuid], cx: &mut Context<Self>) {
        let was_preserving = self.preserve_close_worktrees;
        self.preserve_close_worktrees = true;
        for &iid in ids {
            self.close_instance_inner(iid, "close (worktree decision)", cx);
        }
        self.preserve_close_worktrees = was_preserving;
    }

    pub(super) fn save_editor_before_close(
        &mut self,
        iid: Uuid,
        rest: Vec<Uuid>,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.close_editor(iid).cloned() else {
            return;
        };
        let Some(path) = editor.read(cx).path().map(Path::to_path_buf) else {
            let dir = self
                .workspace
                .instance(iid)
                .and_then(|inst| self.workspace.project(inst.project_id))
                .map(|project| project.root_path.clone())
                .unwrap_or_default();
            let picked = cx.prompt_for_new_path(&dir, None);
            cx.spawn(async move |this, cx| {
                let Ok(Ok(Some(path))) = picked.await else {
                    return;
                };
                let _ = this.update(cx, |this, cx| {
                    if this
                        .close_editor(iid)
                        .is_some_and(|current| current == &editor)
                    {
                        // The dialog picked a path on this machine. Write it
                        // here, as Save As does, even in a remote project.
                        this.save_close_editor_path(iid, rest, path, false, cx);
                    }
                });
            })
            .detach();
            return;
        };
        self.save_close_editor_path(iid, rest, path, true, cx);
    }

    /// `in_project`: `path` is the editor's own, in the project (over SSH for
    /// a remote one). Otherwise it is a local path picked in a save dialog.
    fn save_close_editor_path(
        &mut self,
        iid: Uuid,
        rest: Vec<Uuid>,
        path: PathBuf,
        in_project: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.close_editor(iid).cloned() else {
            return;
        };
        let text = editor.read(cx).text(cx);
        let was_untitled = editor.read(cx).path().is_none();
        let expected = text.clone();
        let saved_path = path.clone();
        let loc = self
            .workspace
            .instance(iid)
            .filter(|_| in_project)
            .and_then(|inst| self.repo_loc(inst.project_id));
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match loc {
                        Some(loc @ integrations::RepoLoc::Remote(_)) => {
                            integrations::write_remote_file(&loc, &path.to_string_lossy(), &text)
                        }
                        _ => std::fs::write(path, text).map_err(anyhow::Error::from),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .close_editor(iid)
                    .is_none_or(|current| current != &editor)
                {
                    return;
                }
                match result {
                    Ok(()) if editor.read(cx).text(cx) == expected => {
                        editor.update(cx, |editor, cx| {
                            if was_untitled {
                                editor.set_path(saved_path.clone(), cx);
                            } else {
                                editor.mark_saved(cx);
                            }
                        });
                        if let Some(inst) = this.workspace.instance_mut(iid) {
                            if was_untitled {
                                inst.title = saved_path
                                    .file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| t("Untitled").to_string());
                            }
                            inst.editor_path = Some(saved_path);
                        }
                        this.persist();
                        this.request_recoverable_close(rest, cx);
                    }
                    Ok(()) => this.add_event(
                        NotifKind::Error,
                        t("Editor changed while saving"),
                        t("The pane stays open with your newer changes."),
                    ),
                    Err(error) => {
                        this.add_event(NotifKind::Error, t("Save failed"), error.to_string())
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::{CloseGroup, reason_close_evidence};
    use muxel_core::control::AgentState;
    use muxel_core::recoverable_close::{
        CloseDecision, CloseFacts, CloseStage, decide, departure_finalizes,
    };
    use muxel_terminal::{ProgramStatusKind, TerminalStatusReason};
    use std::collections::HashMap;
    use std::time::Instant;
    use uuid::Uuid;

    fn facts(status: AgentState, reason: TerminalStatusReason) -> CloseFacts {
        CloseFacts {
            status,
            evidence: reason_close_evidence(reason),
            attributable: true,
            awaiting_reply: false,
            another_tmux_client: false,
            session_may_outlive: true,
            launch_id: None,
            binding_rev: 0,
            input_rev: 0,
            completion_rev: 0,
        }
    }

    #[test]
    fn program_reports_are_quiet_only_when_idle_or_done() {
        let report = TerminalStatusReason::ProgramStatus(None);
        for status in [AgentState::Idle, AgentState::Done] {
            assert_eq!(
                decide(facts(status, report), true),
                CloseDecision::Hold { forced: false },
                "{status:?}"
            );
        }
        let blocked = TerminalStatusReason::ProgramStatus(Some(ProgramStatusKind::Question));
        for (status, reason) in [
            (AgentState::Working, report),
            (AgentState::Blocked, blocked),
        ] {
            assert_eq!(decide(facts(status, reason), true), CloseDecision::Ask);
        }
        // An error report shows Done, but it is no proof the program is idle.
        assert_eq!(
            decide(
                facts(AgentState::Done, TerminalStatusReason::ProgramStatusError),
                true
            ),
            CloseDecision::Ask
        );
    }

    #[test]
    fn a_close_waiting_on_the_shared_layout_is_still_a_running_countdown() {
        let mut group = CloseGroup {
            id: Uuid::new_v4(),
            members: Vec::new(),
            deadline: Instant::now(),
            undo_until: 0,
            needs_decision: false,
            checking: false,
            undoing: false,
            sync_marks: None,
            layouts: HashMap::new(),
            focus: None,
            caller: None,
        };
        assert_eq!(group.stage(), CloseStage::CountingDown);
        assert!(!group.syncing());
        // Past its deadline, waiting for a read: leaving still finishes it.
        group.sync_marks = Some(vec![(Uuid::new_v4(), 3)]);
        assert!(group.syncing());
        assert!(departure_finalizes(group.stage()));
        // Once read, the check, decision and Undo keep their own stages.
        for (checking, needs_decision, undoing, stage) in [
            (true, false, false, CloseStage::Checking),
            (false, true, false, CloseStage::AwaitingDecision),
            (false, true, true, CloseStage::Undoing),
        ] {
            group.checking = checking;
            group.needs_decision = needs_decision;
            group.undoing = undoing;
            assert_eq!(group.stage(), stage);
            assert!(!group.syncing(), "{stage:?}");
            assert!(!departure_finalizes(stage), "{stage:?}");
        }
    }
}
