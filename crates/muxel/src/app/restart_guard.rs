//! One restart owns a pane until its remote owner lookup and kill finish.

use muxel_core::tmux::{ClosedSession, SessionTarget, Socket};
use muxel_core::{Instance, Workspace};
use std::collections::{HashMap, hash_map::Entry};
use uuid::Uuid;

struct Restart {
    token: Uuid,
    project: Uuid,
    recorded: Option<SessionTarget>,
}

#[derive(Default)]
pub(super) struct PendingRestarts(HashMap<Uuid, Restart>);

pub(super) struct RemoteRestartCompletion {
    pub instance: Uuid,
    pub project: Uuid,
    pub workspace: Option<Uuid>,
    pub token: Uuid,
    pub host: String,
    pub session: String,
    pub killed: anyhow::Result<()>,
}

impl PendingRestarts {
    pub fn begin(&mut self, instance: &Instance) -> Option<Uuid> {
        let Entry::Vacant(entry) = self.0.entry(instance.id) else {
            return None;
        };
        let token = Uuid::new_v4();
        entry.insert(Restart {
            token,
            project: instance.project_id,
            recorded: target(instance),
        });
        Some(token)
    }

    pub fn contains(&self, iid: Uuid) -> bool {
        self.0.contains_key(&iid)
    }

    pub fn current(&self, iid: Uuid, token: Uuid) -> bool {
        self.0
            .get(&iid)
            .is_some_and(|restart| restart.token == token)
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
        let Some(restart) = self.0.get(&iid).filter(|restart| restart.token == token) else {
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
        let Some(restart) = self.0.get(&iid).filter(|restart| restart.token == token) else {
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
        self.current(iid, token) && self.0.remove(&iid).is_some()
    }

    pub fn cancel(&mut self, iid: Uuid) {
        self.0.remove(&iid);
    }

    pub fn clear(&mut self) {
        self.0.clear();
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
