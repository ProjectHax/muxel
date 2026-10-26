//! Pure scheduling and result guards for provider session discovery.

use crate::tmux::Socket;
use crate::{CodexHintMiss, Workspace};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionProvider {
    Codex,
    Claude,
    Grok,
    Muse,
}

/// Captured without I/O on the UI thread. Binding generations distinguish an
/// intervening rebind even if the UUID later returns to its original value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDiscoveryKey {
    pub pane: Uuid,
    pub provider: SessionProvider,
    pub launch: Instant,
    pub child_pid: Option<u32>,
    pub binding_generation: u64,
    pub binding: Option<String>,
    pub binding_path: Option<PathBuf>,
    pub cwd: PathBuf,
    /// Canonical provider identity hint, excluding title activity/spinners.
    pub hint: Option<String>,
    /// Local tmux owner for a Codex pane whose OSC title cannot reach the view.
    pub tmux_session: Option<String>,
    pub tmux_socket: Option<Socket>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDiscoveryRequest {
    pub key: SessionDiscoveryKey,
    pub codex_miss: Option<CodexHintMiss>,
    /// A sibling-owned tmux title still gets polled. If the title has not
    /// changed, the worker can reuse the validated identity without a walk.
    pub codex_rejected_hint: Option<(String, String)>,
    generation: u64,
    sequence: u64,
}

#[derive(Default)]
pub struct CodexLookupEvidence {
    pub miss: Option<CodexHintMiss>,
    pub rejected_owner: Option<(Uuid, String)>,
    pub observed_hint: Option<String>,
    /// A previously validated sibling-owned hint was reused without a rollout
    /// read. It can never be adopted as a new binding.
    pub cached_rejection: bool,
}

#[derive(Debug)]
struct CodexOwnershipRejection {
    owner: Uuid,
    session_id: String,
    observed_hint: Option<String>,
    binding_generation: u64,
}

/// A pane keeps its obsolete worker until it finishes, so rapid identity
/// changes cannot pile up scans. Only a matching completion releases it.
#[derive(Debug, Default)]
pub struct SessionDiscovery {
    key: Option<SessionDiscoveryKey>,
    generation: u64,
    sequence: u64,
    in_flight: Option<SessionDiscoveryRequest>,
    misses: u32,
    retry_at: Option<Instant>,
    codex_miss: Option<CodexHintMiss>,
    codex_owner: Option<CodexOwnershipRejection>,
}

/// Discovery belongs to a workspace pane, independent of which window owns its
/// view. Retain outstanding requests until the pane metadata is really removed.
#[derive(Debug, Default)]
pub struct SessionDiscoveries {
    panes: HashMap<Uuid, SessionDiscovery>,
    binding_generations: HashMap<Uuid, u64>,
    bindings: HashMap<Uuid, String>,
}

impl SessionDiscoveries {
    pub fn reconcile(&mut self, workspace: &Workspace) {
        let panes: HashSet<_> = workspace.instances.iter().map(|i| i.id).collect();
        self.panes.retain(|pane, _| panes.contains(pane));
        self.binding_generations
            .retain(|pane, _| panes.contains(pane));
        self.bindings.retain(|pane, _| panes.contains(pane));
        for instance in &workspace.instances {
            if let Some(id) = &instance.session_id {
                if self.bindings.get(&instance.id) != Some(id) {
                    self.bindings.insert(instance.id, id.clone());
                }
            } else {
                self.bindings.remove(&instance.id);
            }
        }
    }

    pub fn binding_generation(&self, pane: Uuid) -> u64 {
        self.binding_generations.get(&pane).copied().unwrap_or(0)
    }

    pub fn binding_changed(&mut self, pane: Uuid) {
        let generation = self.binding_generations.entry(pane).or_default();
        *generation = generation.wrapping_add(1);
    }

    pub fn project_directory_changed(&mut self, workspace: &Workspace, project: Uuid) {
        // Project::instances() describes the layout, excluding popped-out panes.
        for instance in workspace
            .instances
            .iter()
            .filter(|i| i.project_id == project)
        {
            self.binding_changed(instance.id);
        }
    }

    pub fn begin(
        &mut self,
        key: SessionDiscoveryKey,
        now: Instant,
    ) -> Option<SessionDiscoveryRequest> {
        let state = self.panes.entry(key.pane).or_default();
        state.observe(Some(&key));
        if let Some(owner) = &state.codex_owner {
            let still_owned = self.bindings.get(&owner.owner) == Some(&owner.session_id)
                && self
                    .binding_generations
                    .get(&owner.owner)
                    .copied()
                    .unwrap_or(0)
                    == owner.binding_generation;
            if still_owned {
                key.tmux_session.as_ref()?;
            } else {
                // Ownership changed. Revalidate immediately, even when the
                // previous failed lookup had reached its maximum retry delay.
                state.codex_owner = None;
                state.codex_miss = None;
                state.misses = 0;
                state.retry_at = None;
            }
        }
        state.begin(key, now)
    }

    pub fn finish(
        &mut self,
        request: &SessionDiscoveryRequest,
        current: Option<&SessionDiscoveryKey>,
        now: Instant,
        found: bool,
    ) -> bool {
        self.panes
            .get_mut(&request.key.pane)
            .is_some_and(|state| state.finish(request, current, now, found))
    }

    pub fn finish_with_codex_miss(
        &mut self,
        request: &SessionDiscoveryRequest,
        current: Option<&SessionDiscoveryKey>,
        now: Instant,
        found: bool,
        miss: Option<CodexHintMiss>,
    ) -> bool {
        self.finish_with_codex_evidence(
            request,
            current,
            now,
            found,
            CodexLookupEvidence {
                miss,
                rejected_owner: None,
                observed_hint: None,
                cached_rejection: false,
            },
        )
    }

    pub fn finish_with_codex_evidence(
        &mut self,
        request: &SessionDiscoveryRequest,
        current: Option<&SessionDiscoveryKey>,
        now: Instant,
        found: bool,
        evidence: CodexLookupEvidence,
    ) -> bool {
        let owner = evidence
            .rejected_owner
            .map(|(owner, session_id)| CodexOwnershipRejection {
                owner,
                session_id,
                observed_hint: evidence.observed_hint,
                binding_generation: self.binding_generation(owner),
            });
        let revalidate_released_owner = evidence.cached_rejection && owner.is_none();
        let Some(state) = self.panes.get_mut(&request.key.pane) else {
            return false;
        };
        if !state.finish(request, current, now, found) {
            return false;
        }
        state.codex_miss = evidence.miss;
        if !found && request.key.provider == SessionProvider::Codex {
            state.codex_owner = owner;
        }
        if revalidate_released_owner {
            // The cached ID was not checked on disk in this request. When its
            // sibling releases it mid-flight, the next poll must validate it
            // afresh before the target may bind it.
            state.codex_miss = None;
            state.misses = 0;
            state.retry_at = None;
        }
        true
    }
}

/// One pre-launch Codex history lookup per pane per run: confirming a saved id
/// that has no saved rollout path, or recovering an unbound started pane's
/// conversation. A launch waits for its pane's pending lookup for at most the
/// lookup's budget, and a result applies only while no launch or rebind has
/// happened since the lookup began.
#[derive(Debug, Default)]
pub struct CodexRestoreLookups {
    panes: HashMap<Uuid, CodexRestoreLookup>,
}

#[derive(Debug)]
struct CodexRestoreLookup {
    started: Instant,
    binding_generation: u64,
    pending: bool,
}

impl CodexRestoreLookups {
    /// Whether to start this pane's lookup now. A pane never gets a second one,
    /// whatever the first found, so restarts do not keep walking history.
    pub fn begin(&mut self, pane: Uuid, binding_generation: u64, now: Instant) -> bool {
        if self.panes.contains_key(&pane) {
            return false;
        }
        self.panes.insert(
            pane,
            CodexRestoreLookup {
                started: now,
                binding_generation,
                pending: true,
            },
        );
        true
    }

    /// Whether the pane's launch should keep waiting for its lookup.
    pub fn holds_launch(&self, pane: Uuid, now: Instant, budget: Duration) -> bool {
        self.panes.get(&pane).is_some_and(|lookup| {
            lookup.pending && now.saturating_duration_since(lookup.started) < budget
        })
    }

    /// Record a landed lookup. Returns whether its result may still be applied:
    /// the pane has not been prepared for launch or rebound since it began.
    pub fn finish(&mut self, pane: Uuid, binding_generation: u64) -> bool {
        self.panes.get_mut(&pane).is_some_and(|lookup| {
            std::mem::take(&mut lookup.pending) && lookup.binding_generation == binding_generation
        })
    }
}

impl SessionDiscovery {
    fn observe(&mut self, key: Option<&SessionDiscoveryKey>) {
        if self.key.as_ref() != key {
            self.key = key.cloned();
            self.generation = self.generation.wrapping_add(1);
            self.misses = 0;
            self.retry_at = None;
            self.codex_miss = None;
            self.codex_owner = None;
        }
    }

    pub fn begin(
        &mut self,
        key: SessionDiscoveryKey,
        now: Instant,
    ) -> Option<SessionDiscoveryRequest> {
        self.observe(Some(&key));
        if self.in_flight.is_some() || self.retry_at.is_some_and(|at| now < at) {
            return None;
        }
        self.sequence = self.sequence.wrapping_add(1);
        let request = SessionDiscoveryRequest {
            key,
            codex_miss: self.codex_miss.clone(),
            codex_rejected_hint: self.codex_owner.as_ref().and_then(|owner| {
                owner
                    .observed_hint
                    .as_ref()
                    .map(|hint| (hint.clone(), owner.session_id.clone()))
            }),
            generation: self.generation,
            sequence: self.sequence,
        };
        self.in_flight = Some(request.clone());
        Some(request)
    }

    /// Returns whether this result may be applied. `found` controls the next
    /// poll: Codex misses back off from one to thirty seconds, independently of
    /// directory stamp changes. Claude, Grok and Muse
    /// cap misses at two seconds so new in-process switches stay timely.
    /// A hit polls in one second; a bound Codex hint stops polling altogether.
    pub fn finish(
        &mut self,
        request: &SessionDiscoveryRequest,
        current: Option<&SessionDiscoveryKey>,
        now: Instant,
        found: bool,
    ) -> bool {
        if self.in_flight.as_ref() != Some(request) {
            return false;
        }
        self.in_flight = None;
        self.observe(current);
        if current != Some(&request.key) || self.generation != request.generation {
            return false;
        }
        self.codex_owner = None;
        let delay = if found {
            self.misses = 0;
            Duration::from_secs(1)
        } else {
            self.misses = self.misses.saturating_add(1);
            let cap = match request.key.provider {
                SessionProvider::Codex => 30,
                SessionProvider::Claude | SessionProvider::Grok | SessionProvider::Muse => 2,
            };
            Duration::from_secs((1_u64 << (self.misses - 1).min(5)).min(cap))
        };
        self.retry_at = Some(now + delay);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CodexRestoreLookups, SessionDiscoveries, SessionDiscovery, SessionDiscoveryKey,
        SessionProvider,
    };
    use crate::{AgentPreset, Instance, LeafData, PaneNode, Project, Workspace};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    fn key() -> SessionDiscoveryKey {
        SessionDiscoveryKey {
            pane: Uuid::new_v4(),
            provider: SessionProvider::Codex,
            launch: Instant::now(),
            child_pid: Some(41),
            binding_generation: 7,
            binding: None,
            binding_path: None,
            cwd: PathBuf::from("project"),
            hint: Some("thread hint".to_string()),
            tmux_session: None,
            tmux_socket: None,
        }
    }

    #[test]
    fn b9_codex_missing_hint_keeps_bounded_availability_checks_running() {
        let key = key();
        let now = Instant::now();
        let mut discovery = SessionDiscovery::default();
        let request = discovery.begin(key.clone(), now).unwrap();
        assert!(discovery.finish(&request, Some(&key), now, false));
        let later = now + Duration::from_secs(3600);
        assert!(
            discovery.begin(key.clone(), later).is_some(),
            "unchanged missing hint never retried"
        );
    }

    #[test]
    fn w8lm_codex_miss_is_invalidated_by_launch_cwd_or_binding_changes() {
        for change in ["launch", "cwd", "binding", "hint"] {
            let mut key = key();
            let now = Instant::now();
            let mut discoveries = SessionDiscoveries::default();
            let request = discoveries.begin(key.clone(), now).unwrap();
            let miss =
                crate::CodexHintMiss::capture(std::path::Path::new("unused"), "invalid hint", now);
            assert!(discoveries.finish_with_codex_miss(
                &request,
                Some(&key),
                now,
                false,
                Some(miss)
            ));
            let later = now + Duration::from_secs(1);
            let cached = discoveries.begin(key.clone(), later).unwrap();
            assert!(cached.codex_miss.is_some());
            assert!(discoveries.finish_with_codex_miss(
                &cached,
                Some(&key),
                later,
                false,
                cached.codex_miss.clone()
            ));
            match change {
                "launch" => key.launch += Duration::from_secs(1),
                "cwd" => key.cwd = PathBuf::from("changed"),
                "binding" => key.binding_generation += 1,
                "hint" => key.hint = Some("another hint".into()),
                _ => unreachable!(),
            }
            assert!(
                discoveries.begin(key, later).unwrap().codex_miss.is_none(),
                "{change}"
            );
        }
    }

    fn workspace(pane: Uuid) -> Workspace {
        let project = Project::new("project", "project");
        let mut instance = Instance::from_preset(project.id, &AgentPreset::claude());
        instance.id = pane;
        Workspace {
            projects: vec![project],
            instances: vec![instance],
            ..Workspace::default()
        }
    }

    #[test]
    fn b9_r2_codex_ownership_cache_resets_when_the_target_context_changes() {
        use super::CodexLookupEvidence;
        for change in ["launch", "cwd", "binding", "hint"] {
            let mut key = key();
            let mut workspace = workspace(key.pane);
            let mut owner = workspace.instances[0].clone();
            owner.id = Uuid::new_v4();
            owner.session_id = Some("owned hint".into());
            let owner_id = owner.id;
            workspace.instances.push(owner);
            let mut discoveries = SessionDiscoveries::default();
            discoveries.reconcile(&workspace);
            let now = Instant::now();
            let request = discoveries.begin(key.clone(), now).unwrap();
            assert!(discoveries.finish_with_codex_evidence(
                &request,
                Some(&key),
                now,
                false,
                CodexLookupEvidence {
                    miss: None,
                    rejected_owner: Some((owner_id, "owned hint".into())),
                    observed_hint: key.hint.clone(),
                    cached_rejection: false,
                }
            ));
            assert!(
                discoveries
                    .begin(key.clone(), now + Duration::from_secs(60))
                    .is_none()
            );
            match change {
                "launch" => key.launch += Duration::from_secs(1),
                "cwd" => key.cwd = PathBuf::from("changed"),
                "binding" => key.binding_generation += 1,
                "hint" => key.hint = Some("another hint".into()),
                _ => unreachable!(),
            }
            assert!(discoveries.begin(key, now).is_some(), "{change}");
        }
    }

    #[test]
    fn popout_tick_and_redock_preserve_the_outstanding_request_and_generation() {
        let mut key = key();
        let now = Instant::now();
        let mut workspace = workspace(key.pane);
        let mut discoveries = SessionDiscoveries::default();
        discoveries.binding_changed(key.pane);
        key.binding_generation = discoveries.binding_generation(key.pane);
        let old = discoveries.begin(key.clone(), now).unwrap();
        // A popout has metadata and a live view, but no main-window layout entry.
        discoveries.reconcile(&workspace);
        assert_eq!(discoveries.binding_generation(key.pane), 1);
        assert!(discoveries.begin(key.clone(), now).is_none());
        workspace.projects[0].layout = Some(PaneNode::Leaf(LeafData {
            pane_id: Uuid::new_v4(),
            tabs: vec![key.pane],
            active: 0,
        }));
        discoveries.reconcile(&workspace);
        assert!(discoveries.begin(key.clone(), now).is_none());
        assert!(discoveries.finish(&old, Some(&key), now, true));
        let later = now + Duration::from_secs(1);
        let new = discoveries.begin(key.clone(), later).unwrap();
        assert_ne!(old, new);
        assert!(!discoveries.finish(&old, Some(&key), later, true));
        assert!(discoveries.begin(key.clone(), later).is_none());
        assert!(discoveries.finish(&new, Some(&key), later, true));
        workspace.instances.clear();
        discoveries.reconcile(&workspace);
        assert_eq!(discoveries.binding_generation(key.pane), 0);
        assert!(!discoveries.finish(&new, Some(&key), later, true));
    }

    #[test]
    fn claude_popout_keeps_polling_and_project_directory_changes_invalidate_it() {
        let mut key = key();
        key.provider = SessionProvider::Claude;
        key.hint = None;
        key.binding_generation = 0;
        let workspace = workspace(key.pane);
        assert!(workspace.projects[0].instances().is_empty());
        let mut discoveries = SessionDiscoveries::default();
        let now = Instant::now();
        discoveries.reconcile(&workspace);
        let first = discoveries.begin(key.clone(), now).unwrap();
        assert!(discoveries.finish(&first, Some(&key), now, true));
        let later = now + Duration::from_secs(1);
        let second = discoveries.begin(key.clone(), later).unwrap();
        let other = Uuid::new_v4();
        discoveries.project_directory_changed(&workspace, other);
        assert_eq!(discoveries.binding_generation(key.pane), 0);
        discoveries.project_directory_changed(&workspace, workspace.projects[0].id);
        // A directory edit away and back still invalidates the worker.
        discoveries.project_directory_changed(&workspace, workspace.projects[0].id);
        key.binding_generation = discoveries.binding_generation(key.pane);
        assert_eq!(key.binding_generation, 2);
        assert!(!discoveries.finish(&second, Some(&key), later, true));
        assert!(discoveries.begin(key, later).is_some());
    }

    #[test]
    fn hook_discovery_coalesces_unchanged_requests_and_backs_off_missing_files() {
        let mut key = key();
        key.provider = SessionProvider::Claude;
        let mut discovery = SessionDiscovery::default();
        let mut now = Instant::now();
        for delay in [1, 2, 2, 2, 2] {
            let request = discovery.begin(key.clone(), now).unwrap();
            assert!(discovery.begin(key.clone(), now).is_none());
            assert!(discovery.finish(&request, Some(&key), now, false));
            now += Duration::from_secs(delay);
            assert!(
                discovery
                    .begin(key.clone(), now - Duration::from_millis(1))
                    .is_none()
            );
        }
        assert!(discovery.begin(key, now).is_some());
    }

    #[test]
    fn changed_hint_resets_backoff_but_waits_for_the_obsolete_worker() {
        let key = key();
        let now = Instant::now();
        let mut discovery = SessionDiscovery::default();
        let old = discovery.begin(key.clone(), now).unwrap();
        let mut changed = key.clone();
        changed.hint = Some("new hint".to_string());
        assert!(discovery.begin(changed.clone(), now).is_none());
        assert!(!discovery.finish(&old, Some(&changed), now, false));
        let new = discovery.begin(changed.clone(), now).unwrap();
        // A duplicate old completion must not release the current request.
        assert!(!discovery.finish(&old, Some(&changed), now, true));
        assert!(discovery.begin(changed.clone(), now).is_none());
        assert!(discovery.finish(&new, Some(&changed), now, true));
    }

    fn rejects_changed(change: impl FnOnce(&mut SessionDiscoveryKey)) {
        let key = key();
        let now = Instant::now();
        let mut discovery = SessionDiscovery::default();
        let request = discovery.begin(key.clone(), now).unwrap();
        let mut changed = key;
        change(&mut changed);
        assert!(!discovery.finish(&request, Some(&changed), now, true));
        assert!(discovery.begin(changed, now).is_some());
    }

    #[test]
    fn stale_discovery_rejects_a_changed_launch() {
        rejects_changed(|key| key.launch += Duration::from_secs(1));
    }

    #[test]
    fn stale_discovery_rejects_a_changed_binding_generation() {
        // Even a rebind away and back to the same UUID invalidates the result.
        rejects_changed(|key| key.binding_generation += 1);
    }

    #[test]
    fn stale_discovery_rejects_a_changed_binding_path() {
        rejects_changed(|key| key.binding_path = Some(PathBuf::from("new-rollout.jsonl")));
    }

    #[test]
    fn stale_discovery_rejects_a_changed_cwd() {
        rejects_changed(|key| key.cwd = PathBuf::from("other-project"));
    }

    #[test]
    fn stale_discovery_rejects_a_closed_pane() {
        let key = key();
        let now = Instant::now();
        let mut discovery = SessionDiscovery::default();
        let request = discovery.begin(key, now).unwrap();
        assert!(!discovery.finish(&request, None, now, true));
    }

    #[test]
    fn stale_discovery_rejects_a_changed_pane_process_or_provider() {
        rejects_changed(|key| key.pane = Uuid::new_v4());
        rejects_changed(|key| key.child_pid = Some(99));
        rejects_changed(|key| key.provider = SessionProvider::Claude);
    }

    #[test]
    fn discovery_hit_resets_the_missing_file_backoff() {
        let mut key = key();
        key.provider = SessionProvider::Claude;
        let mut discovery = SessionDiscovery::default();
        let mut now = Instant::now();
        for delay in [1, 2, 2] {
            let request = discovery.begin(key.clone(), now).unwrap();
            assert!(discovery.finish(&request, Some(&key), now, false));
            now += Duration::from_secs(delay);
        }
        let request = discovery.begin(key.clone(), now).unwrap();
        assert!(discovery.finish(&request, Some(&key), now, true));
        assert!(discovery.begin(key.clone(), now).is_none());
        assert!(discovery.begin(key, now + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn changed_key_resets_a_completed_missing_file_backoff() {
        let mut key = key();
        key.provider = SessionProvider::Claude;
        let now = Instant::now();
        let mut discovery = SessionDiscovery::default();
        let request = discovery.begin(key.clone(), now).unwrap();
        assert!(discovery.finish(&request, Some(&key), now, false));
        assert!(discovery.begin(key.clone(), now).is_none());
        let mut changed = key;
        changed.cwd = PathBuf::from("changed");
        assert!(discovery.begin(changed, now).is_some());
    }

    #[test]
    fn observed_key_change_away_and_back_still_rejects_the_old_result() {
        let key = key();
        let now = Instant::now();
        let mut discovery = SessionDiscovery::default();
        let request = discovery.begin(key.clone(), now).unwrap();
        let mut changed = key.clone();
        changed.hint = Some("other hint".to_string());
        assert!(discovery.begin(changed, now).is_none());
        assert!(discovery.begin(key.clone(), now).is_none());
        assert!(!discovery.finish(&request, Some(&key), now, true));
        assert!(discovery.begin(key, now).is_some());
    }

    #[test]
    fn hook_and_process_polls_cap_missing_backoff_to_keep_switches_timely() {
        for provider in [
            SessionProvider::Claude,
            SessionProvider::Grok,
            SessionProvider::Muse,
        ] {
            let mut key = key();
            key.provider = provider;
            let mut discovery = SessionDiscovery::default();
            let mut now = Instant::now();
            for delay in [1, 2, 2, 2] {
                let request = discovery.begin(key.clone(), now).unwrap();
                assert!(discovery.finish(&request, Some(&key), now, false));
                now += Duration::from_secs(delay);
                assert!(
                    discovery
                        .begin(key.clone(), now - Duration::from_millis(1))
                        .is_none()
                );
            }
            assert!(discovery.begin(key, now).is_some());
        }
    }

    #[test]
    fn codex_restore_lookup_runs_once_and_holds_its_launch_only_within_budget() {
        let pane = Uuid::new_v4();
        let other = Uuid::new_v4();
        let budget = Duration::from_secs(2);
        let now = Instant::now();
        let mut lookups = CodexRestoreLookups::default();
        assert!(!lookups.holds_launch(pane, now, budget), "no lookup began");
        assert!(lookups.begin(pane, 3, now));
        assert!(!lookups.begin(pane, 3, now), "a pane gets one lookup");
        assert!(lookups.holds_launch(pane, now + Duration::from_millis(1999), budget));
        assert!(!lookups.holds_launch(pane, now + budget, budget));
        assert!(!lookups.holds_launch(other, now, budget));
        assert!(lookups.finish(pane, 3));
        assert!(!lookups.holds_launch(pane, now, budget));
        assert!(!lookups.finish(pane, 3), "a result lands once");
        assert!(!lookups.begin(pane, 4, now), "not even after it finished");
    }

    #[test]
    fn codex_restore_lookup_result_is_dropped_after_a_launch_or_rebind() {
        let pane = Uuid::new_v4();
        let mut lookups = CodexRestoreLookups::default();
        assert!(lookups.begin(pane, 3, Instant::now()));
        // Preparing the launch (or any rebind) bumps the binding generation.
        assert!(!lookups.finish(pane, 4));
        assert!(!lookups.holds_launch(pane, Instant::now(), Duration::from_secs(2)));
        assert!(
            !lookups.finish(Uuid::new_v4(), 0),
            "unknown panes never apply"
        );
    }
}
