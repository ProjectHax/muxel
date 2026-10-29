//! One restart owns a pane until its remote owner lookup and kill finish.

use muxel_core::tmux::{ClosedSession, SessionTarget, Socket};
use muxel_core::{Instance, Workspace};
use std::collections::{HashMap, hash_map::Entry};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Default)]
struct CommandState {
    cancelled: bool,
    in_flight: usize,
}

/// Shared with workers before dispatch. Cancellation never revives this lease;
/// a newer restart gets a different state even when the pane ID is unchanged.
#[derive(Clone, Default)]
pub(super) struct RestartLease {
    state: Arc<Mutex<CommandState>>,
}

/// Tracks admitted work without retaining a mutex guard across I/O.
pub(super) struct CommandGuard {
    state: Arc<Mutex<CommandState>>,
}

impl RestartLease {
    pub fn begin_command(&self) -> Option<CommandGuard> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.cancelled {
            return None;
        }
        state.in_flight = state.in_flight.checked_add(1)?;
        Some(CommandGuard {
            state: Arc::clone(&self.state),
        })
    }

    pub fn is_in_flight(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .in_flight
            > 0
    }

    fn cancel(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancelled = true;
    }
}

impl Drop for CommandGuard {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.in_flight -= 1;
    }
}

struct Restart {
    token: Uuid,
    project: Uuid,
    recorded: Option<SessionTarget>,
    lease: RestartLease,
}

#[derive(Default)]
pub(super) struct PendingRestarts {
    pending: HashMap<Uuid, Restart>,
    /// Changes only when a peer changes this pane's tmux owner. Discovery's
    /// binding generation also changes for unrelated layout adoption.
    owner_generations: HashMap<Uuid, u64>,
}

pub(super) struct RemoteRestartCompletion {
    pub instance: Uuid,
    pub project: Uuid,
    pub workspace: Option<Uuid>,
    pub token: Uuid,
    /// Tmux owner generation observed before remote resolution and kill began.
    pub owner_generation: u64,
    pub host: String,
    pub session: String,
    pub killed: anyhow::Result<()>,
}

impl PendingRestarts {
    pub fn begin(&mut self, instance: &Instance) -> Option<Uuid> {
        let Entry::Vacant(entry) = self.pending.entry(instance.id) else {
            return None;
        };
        let token = Uuid::new_v4();
        entry.insert(Restart {
            token,
            project: instance.project_id,
            recorded: target(instance),
            lease: RestartLease::default(),
        });
        Some(token)
    }

    pub fn contains(&self, iid: Uuid) -> bool {
        self.pending.contains_key(&iid)
    }

    pub fn lease(&self, iid: Uuid) -> Option<RestartLease> {
        self.pending.get(&iid).map(|restart| restart.lease.clone())
    }

    pub fn current(&self, iid: Uuid, token: Uuid) -> bool {
        self.pending
            .get(&iid)
            .is_some_and(|restart| restart.token == token)
    }

    pub fn owner_generation(&self, iid: Uuid) -> u64 {
        self.owner_generations.get(&iid).copied().unwrap_or(0)
    }

    /// A peer took this pane's tmux session or socket elsewhere. Release the
    /// old restart lease before attaching to the peer's new owner.
    pub fn owner_changed(&mut self, iid: Uuid) {
        self.cancel(iid);
        let generation = self.owner_generations.entry(iid).or_default();
        *generation = generation.wrapping_add(1);
    }

    /// Validate the asynchronous result before changing the durable owner. A
    /// peer may have rebound this pane while discovery was running; only an
    /// unchanged binding or the same resolved target can accept the result.
    pub fn record_owner(
        &self,
        workspace: &mut Workspace,
        iid: Uuid,
        token: Uuid,
        closed: &ClosedSession,
    ) -> bool {
        let Some(restart) = self
            .pending
            .get(&iid)
            .filter(|restart| restart.token == token)
        else {
            return false;
        };
        let Some(instance) = workspace.instance_mut(iid) else {
            return false;
        };
        let current = target(instance);
        if closed.instance != iid
            || closed.socket.is_none()
            || instance.project_id != restart.project
            || (current != restart.recorded && current.as_ref() != Some(&closed.target()))
        {
            return false;
        }
        instance.tmux_session = Some(closed.name.clone());
        instance.tmux_socket = closed.socket.clone();
        true
    }

    /// After confirmed teardown, replace only the expected Default binding of
    /// this restart generation. A peer rebind and a newer restart both win.
    pub fn migrate_default(
        &self,
        workspace: &mut Workspace,
        iid: Uuid,
        token: Uuid,
        expected: &SessionTarget,
        replacement: &SessionTarget,
    ) -> bool {
        let Some(restart) = self
            .pending
            .get(&iid)
            .filter(|restart| restart.token == token)
        else {
            return false;
        };
        let Some(instance) = workspace.instance_mut(iid) else {
            return false;
        };
        if instance.project_id != restart.project
            || target(instance).as_ref() != Some(expected)
            || expected.socket != Some(Socket::Default)
            || replacement.socket != Some(Socket::Private)
            || replacement.name != expected.name
        {
            return false;
        }
        instance.tmux_socket = replacement.socket.clone();
        true
    }

    pub fn finish(&mut self, iid: Uuid, token: Uuid) -> bool {
        if !self.current(iid, token) {
            return false;
        }
        self.cancel(iid);
        true
    }

    pub fn cancel(&mut self, iid: Uuid) {
        if let Some(restart) = self.pending.remove(&iid) {
            restart.lease.cancel();
        }
    }

    pub fn clear(&mut self) {
        for (_, restart) in self.pending.drain() {
            restart.lease.cancel();
        }
        self.owner_generations.clear();
    }
}

fn target(instance: &Instance) -> Option<SessionTarget> {
    instance
        .tmux_session
        .as_ref()
        .map(|name| SessionTarget::new(name.clone(), instance.tmux_socket.clone()))
}

#[cfg(test)]
mod tests {
    use super::PendingRestarts;
    use muxel_core::tmux::{ClosedSession, SessionTarget, Socket};
    use muxel_core::{Instance, Project, Workspace};
    use uuid::Uuid;

    fn fixture() -> (Workspace, Uuid) {
        let mut workspace = Workspace::default();
        let project = workspace.add_project(Project::new("repo", "/repo"));
        let mut instance = Instance::shell(project);
        instance.tmux_session = Some("muxel_repo_namesake".into());
        let iid = instance.id;
        workspace.add_instance(instance);
        (workspace, iid)
    }

    fn resolved(iid: Uuid, socket: Socket) -> ClosedSession {
        ClosedSession {
            instance: iid,
            name: "muxel_repo_namesake".into(),
            socket: Some(socket),
        }
    }

    #[test]
    fn cancelled_command_lease_cannot_revive_for_a_new_restart_token() {
        let (workspace, iid) = fixture();
        let mut restarts = PendingRestarts::default();
        let first = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        let old = restarts.lease(iid).unwrap();
        restarts.cancel(iid);
        let second = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        let new = restarts.lease(iid).unwrap();
        assert_ne!(first, second);
        assert!(old.begin_command().is_none());
        let guard = new
            .begin_command()
            .expect("new restart owns a distinct lease");
        assert!(new.is_in_flight());
        assert!(!old.is_in_flight());
        assert!(old.begin_command().is_none());
        drop(guard);
        assert!(!new.is_in_flight());
    }

    #[test]
    fn cancelled_lease_keeps_admitted_commands_observable_until_all_guards_drop() {
        let (workspace, iid) = fixture();
        let mut restarts = PendingRestarts::default();
        restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        let lease = restarts.lease(iid).unwrap();
        let observer = lease.clone();
        let first = lease.begin_command().unwrap();
        let second = lease.begin_command().unwrap();
        restarts.cancel(iid);
        assert!(observer.is_in_flight());
        assert!(observer.begin_command().is_none());
        drop(first);
        assert!(observer.is_in_flight());
        drop(second);
        assert!(!observer.is_in_flight());
        assert!(observer.begin_command().is_none());
    }

    #[test]
    fn finish_clear_and_owner_change_permanently_cancel_captured_leases() {
        let (workspace, iid) = fixture();
        for action in 0..3 {
            let mut restarts = PendingRestarts::default();
            let token = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
            let lease = restarts.lease(iid).unwrap();
            let guard = lease.begin_command().unwrap();
            match action {
                0 => assert!(restarts.finish(iid, token)),
                1 => restarts.clear(),
                _ => restarts.owner_changed(iid),
            }
            assert!(!restarts.contains(iid));
            assert!(lease.begin_command().is_none());
            assert!(lease.is_in_flight());
            drop(guard);
            assert!(!lease.is_in_flight());
        }
    }

    #[test]
    fn stale_finish_does_not_cancel_the_new_command_lease() {
        let (workspace, iid) = fixture();
        let mut restarts = PendingRestarts::default();
        let first = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        restarts.cancel(iid);
        restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        let lease = restarts.lease(iid).unwrap();
        assert!(!restarts.finish(iid, first));
        assert!(lease.begin_command().is_some());
    }

    #[test]
    fn default_migration_requires_the_same_restart_and_expected_binding() {
        let (mut workspace, iid) = fixture();
        workspace.instance_mut(iid).unwrap().tmux_socket = Some(Socket::Default);
        let mut restarts = PendingRestarts::default();
        let token = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        let expected = resolved(iid, Socket::Default).target();
        let replacement = resolved(iid, Socket::Private).target();
        assert!(!restarts.migrate_default(
            &mut workspace,
            iid,
            Uuid::new_v4(),
            &expected,
            &replacement
        ));
        let wrong_name = SessionTarget::new("another", Some(Socket::Private));
        assert!(!restarts.migrate_default(&mut workspace, iid, token, &expected, &wrong_name));
        assert!(restarts.migrate_default(&mut workspace, iid, token, &expected, &replacement));
        assert!(
            restarts.contains(iid),
            "migration retains restart admission until persistence completes"
        );
        assert_eq!(
            workspace.instance(iid).unwrap().tmux_socket,
            Some(Socket::Private)
        );
        assert!(!restarts.migrate_default(&mut workspace, iid, token, &expected, &replacement));
    }

    #[test]
    fn default_migration_does_not_overwrite_a_peer_or_reopened_pane() {
        let (mut workspace, iid) = fixture();
        workspace.instance_mut(iid).unwrap().tmux_socket = Some(Socket::Default);
        let mut restarts = PendingRestarts::default();
        let old = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        let expected = resolved(iid, Socket::Default).target();
        let replacement = resolved(iid, Socket::Private).target();
        workspace.instance_mut(iid).unwrap().tmux_session = Some("peer-rebound".into());
        assert!(!restarts.migrate_default(&mut workspace, iid, old, &expected, &replacement));
        workspace.instance_mut(iid).unwrap().tmux_session = Some(expected.name.clone());
        restarts.cancel(iid);
        let new = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        assert!(!restarts.migrate_default(&mut workspace, iid, old, &expected, &replacement));
        assert!(restarts.current(iid, new));
        assert_eq!(
            workspace.instance(iid).unwrap().tmux_socket,
            Some(Socket::Default)
        );
    }

    #[test]
    fn overlapping_legacy_restart_cannot_start_another_lookup_or_kill() {
        let (mut workspace, iid) = fixture();
        let mut restarts = PendingRestarts::default();
        let first = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        assert!(restarts.begin(workspace.instance(iid).unwrap()).is_none());
        let mxl = resolved(iid, Socket::Private);
        assert!(restarts.record_owner(&mut workspace, iid, first, &mxl));
        // Keep the lease across persistence and the remote kill, including a
        // lost kill reply. A second lookup could now find only default.
        assert!(restarts.begin(workspace.instance(iid).unwrap()).is_none());
        let persisted = serde_json::to_string(&workspace).unwrap();
        let mut workspace: Workspace = serde_json::from_str(&persisted).unwrap();
        assert!(restarts.finish(iid, first));
        let retry = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        assert_eq!(
            workspace.instance(iid).unwrap().tmux_socket,
            Some(Socket::Private)
        );
        assert!(restarts.record_owner(&mut workspace, iid, retry, &mxl));
        assert_eq!(
            workspace.instance(iid).unwrap().tmux_socket,
            Some(Socket::Private)
        );
    }

    #[test]
    fn stale_resolution_and_kill_completion_cannot_replace_a_new_restart() {
        let (mut workspace, iid) = fixture();
        let mut restarts = PendingRestarts::default();
        let old = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        restarts.cancel(iid); // close and reopen the same persisted pane id
        let new = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        assert!(!restarts.record_owner(&mut workspace, iid, old, &resolved(iid, Socket::Default)));
        assert!(!restarts.finish(iid, old));
        assert!(restarts.current(iid, new));
        assert!(restarts.record_owner(&mut workspace, iid, new, &resolved(iid, Socket::Private)));
        assert_eq!(
            workspace.instance(iid).unwrap().tmux_socket,
            Some(Socket::Private)
        );
    }

    #[test]
    fn returning_to_a_workspace_does_not_revalidate_its_old_callback() {
        let (mut workspace, iid) = fixture();
        let mut restarts = PendingRestarts::default();
        let old = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        restarts.clear();
        let new = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        assert!(!restarts.record_owner(&mut workspace, iid, old, &resolved(iid, Socket::Default)));
        assert!(!restarts.finish(iid, old));
        assert!(restarts.current(iid, new));
        assert_eq!(workspace.instance(iid).unwrap().tmux_socket, None);
    }

    #[test]
    fn changed_peer_binding_is_not_overwritten_by_a_delayed_lookup() {
        let (mut workspace, iid) = fixture();
        let mut restarts = PendingRestarts::default();
        let operation = restarts.begin(workspace.instance(iid).unwrap()).unwrap();
        workspace.instance_mut(iid).unwrap().tmux_socket = Some(Socket::Default);
        assert!(!restarts.record_owner(
            &mut workspace,
            iid,
            operation,
            &resolved(iid, Socket::Private)
        ));
        assert_eq!(
            workspace.instance(iid).unwrap().tmux_socket,
            Some(Socket::Default)
        );
    }
}
