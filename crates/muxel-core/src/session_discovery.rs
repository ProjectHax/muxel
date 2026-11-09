//! Pure scheduling and result guards for provider session discovery.

use crate::tmux::Socket;
use crate::{CodexHintMiss, Workspace};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a pane waits between lookups for a hook record that isn't there,
/// while a watcher on its provider's hook directory wakes the pane when its
/// record is written. The poll is for events a watcher loses: some filesystems
/// never deliver them, and on Windows a full change buffer is dropped silently.
pub const WATCHED_HOOK_FALLBACK: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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
pub struct LookupEvidence {
    pub miss: Option<CodexHintMiss>,
    pub rejected_owner: Option<(Uuid, String)>,
    pub observed_hint: Option<String>,
    /// A previously validated sibling-owned hint was reused without a rollout
    /// read. It can never be adopted as a new binding.
    pub cached_rejection: bool,
    /// The lookup read a hook record, adopted or not. One that waits (its
    /// transcript isn't there yet, or another pane owns it) is retried at the
    /// unwatched cadence, since no new event announces that it can be adopted.
    pub hook_record: bool,
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
    /// A watcher event arrived while a lookup was in flight. That lookup may
    /// have read before the write, so the next one is due when it finishes.
    woken: bool,
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
    /// Providers whose hook directory a running watcher reports changes in.
    watched: HashSet<SessionProvider>,
    /// A finished lookup spent a wake that arrived while it ran, so its pane
    /// is due now. See [`Self::take_redispatch`].
    redispatch: bool,
}

impl SessionDiscoveries {
    /// Record whether `provider`'s hook directory is watched. While it is, a
    /// pane whose lookup found no record waits for [`Self::wake`] or
    /// [`WATCHED_HOOK_FALLBACK`]. Either change wakes the provider's panes:
    /// a new watcher may have missed writes made before it started, and
    /// without one the quiet panes must go back to polling now.
    pub fn set_watched(&mut self, provider: SessionProvider, watched: bool) {
        let changed = if watched {
            self.watched.insert(provider)
        } else {
            self.watched.remove(&provider)
        };
        if changed {
            self.wake_provider(provider);
        }
    }

    pub fn watched(&self, provider: SessionProvider) -> bool {
        self.watched.contains(&provider)
    }

    /// `provider`'s hook record for the pane changed: its next lookup is due
    /// now, or as soon as the one in flight finishes. A pane now running
    /// another provider is left alone, and unknown panes start due anyway.
    pub fn wake(&mut self, provider: SessionProvider, pane: Uuid) {
        if let Some(state) = self.panes.get_mut(&pane)
            && state
                .key
                .as_ref()
                .is_some_and(|key| key.provider == provider)
        {
            state.wake();
        }
    }

    /// Whether a lookup finished since the last call left its pane due at
    /// once: a wake arrived while it ran, or the pane's key changed under it
    /// (its result then doesn't apply, but the new key's lookup is waiting).
    /// Nothing else asks for a lookup before the next tick, so the caller
    /// should.
    pub fn take_redispatch(&mut self) -> bool {
        std::mem::take(&mut self.redispatch)
    }

    /// Every pane of `provider` is due: its watcher may have lost events.
    pub fn wake_provider(&mut self, provider: SessionProvider) {
        for state in self.panes.values_mut() {
            if state
                .key
                .as_ref()
                .is_some_and(|key| key.provider == provider)
            {
                state.wake();
            }
        }
    }

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
        let quiet = !found && self.watched(request.key.provider);
        let Some(state) = self.panes.get_mut(&request.key.pane) else {
            return false;
        };
        let releases = state.in_flight.as_ref() == Some(request);
        let valid = state.finish_with_quiet(request, current, now, found, quiet);
        self.redispatch |= releases && state.due(now);
        valid
    }

    pub fn finish_with_codex_miss(
        &mut self,
        request: &SessionDiscoveryRequest,
        current: Option<&SessionDiscoveryKey>,
        now: Instant,
        found: bool,
        miss: Option<CodexHintMiss>,
    ) -> bool {
        self.finish_with_evidence(
            request,
            current,
            now,
            found,
            LookupEvidence {
                miss,
                ..LookupEvidence::default()
            },
        )
    }

    pub fn finish_with_evidence(
        &mut self,
        request: &SessionDiscoveryRequest,
        current: Option<&SessionDiscoveryKey>,
        now: Instant,
        found: bool,
        evidence: LookupEvidence,
    ) -> bool {
        // Only a miss with nothing waiting can rely on the watcher's wake.
        let quiet = !found && !evidence.hook_record && self.watched(request.key.provider);
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
        // Releasing the request can leave work due whether or not its result
        // applies: a wake that waited on it, or a key that changed under it.
        let releases = state.in_flight.as_ref() == Some(request);
        if !state.finish_with_quiet(request, current, now, found, quiet) {
            self.redispatch |= releases && state.due(now);
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
        self.redispatch |= state.due(now);
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
            self.woken = false;
            self.codex_miss = None;
            self.codex_owner = None;
        }
    }

    fn wake(&mut self) {
        if self.in_flight.is_some() {
            self.woken = true;
        } else {
            self.retry_at = None;
        }
    }

    /// Whether [`Self::begin`] would start a lookup at `now`.
    fn due(&self, now: Instant) -> bool {
        self.in_flight.is_none() && self.retry_at.is_none_or(|at| at <= now)
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
        self.finish_with_quiet(request, current, now, found, false)
    }

    /// As [`Self::finish`]. A `quiet` miss (a watcher will wake the pane)
    /// waits [`WATCHED_HOOK_FALLBACK`] instead. A wake during the lookup makes
    /// the next one due at once.
    fn finish_with_quiet(
        &mut self,
        request: &SessionDiscoveryRequest,
        current: Option<&SessionDiscoveryKey>,
        now: Instant,
        found: bool,
        quiet: bool,
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
            if quiet {
                WATCHED_HOOK_FALLBACK
            } else {
                Duration::from_secs((1_u64 << (self.misses - 1).min(5)).min(cap))
            }
        };
        let delay = if std::mem::take(&mut self.woken) {
            Duration::ZERO
        } else {
            delay
        };
        self.retry_at = Some(now + delay);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CodexRestoreLookups, LookupEvidence, SessionDiscoveries, SessionDiscovery,
        SessionDiscoveryKey, SessionProvider, WATCHED_HOOK_FALLBACK,
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
        use super::LookupEvidence;
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
            assert!(discoveries.finish_with_evidence(
                &request,
                Some(&key),
                now,
                false,
                LookupEvidence {
                    miss: None,
                    rejected_owner: Some((owner_id, "owned hint".into())),
                    observed_hint: key.hint.clone(),
                    cached_rejection: false,
                    hook_record: false,
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
            split_from: None,
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

    fn claude_key() -> SessionDiscoveryKey {
        SessionDiscoveryKey {
            provider: SessionProvider::Claude,
            hint: None,
            ..key()
        }
    }

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn watched_claude_miss_waits_for_its_own_wake_instead_of_polling() {
        let key = claude_key();
        let mut discoveries = SessionDiscoveries::default();
        discoveries.set_watched(SessionProvider::Claude, true);
        let now = Instant::now();
        let request = discoveries.begin(key.clone(), now).unwrap();
        assert!(discoveries.finish(&request, Some(&key), now, false));
        // Unwatched, the pane would look again within two seconds.
        assert!(discoveries.begin(key.clone(), now + 2 * SECOND).is_none());
        discoveries.wake(SessionProvider::Claude, Uuid::new_v4());
        assert!(
            discoveries.begin(key.clone(), now + 3 * SECOND).is_none(),
            "another pane's record"
        );
        discoveries.wake(SessionProvider::Claude, key.pane);
        let woken = now + 3 * SECOND;
        let request = discoveries.begin(key.clone(), woken).unwrap();
        assert!(discoveries.finish(&request, Some(&key), woken, false));
        let fallback = woken + WATCHED_HOOK_FALLBACK;
        assert!(
            discoveries
                .begin(key.clone(), fallback - Duration::from_millis(1))
                .is_none()
        );
        assert!(
            discoveries.begin(key, fallback).is_some(),
            "a lost event still gets polled"
        );
    }

    #[test]
    fn a_wake_during_a_lookup_makes_the_next_one_due_when_it_lands() {
        // The lookup in flight may have read before the write it announces.
        let key = claude_key();
        let mut discoveries = SessionDiscoveries::default();
        discoveries.set_watched(SessionProvider::Claude, true);
        let now = Instant::now();
        let request = discoveries.begin(key.clone(), now).unwrap();
        discoveries.wake(SessionProvider::Claude, key.pane);
        assert!(
            discoveries.begin(key.clone(), now).is_none(),
            "one at a time"
        );
        assert!(discoveries.finish(&request, Some(&key), now, false));
        assert!(discoveries.take_redispatch(), "the caller must ask now");
        assert!(!discoveries.take_redispatch(), "once");
        let request = discoveries.begin(key.clone(), now).unwrap();
        assert!(discoveries.finish(&request, Some(&key), now, false));
        assert!(!discoveries.take_redispatch(), "no wake, no extra lookup");
        assert!(
            discoveries.begin(key, now + 2 * SECOND).is_none(),
            "the wake is spent"
        );
    }

    #[test]
    fn a_lookup_whose_key_changed_while_it_ran_asks_for_a_dispatch() {
        let key = claude_key();
        let mut discoveries = SessionDiscoveries::default();
        discoveries.set_watched(SessionProvider::Claude, true);
        let now = Instant::now();
        let request = discoveries.begin(key.clone(), now).unwrap();
        // The pane's directory changes and a hook wake arrives; both find the
        // old lookup still running.
        let moved = SessionDiscoveryKey {
            cwd: PathBuf::from("elsewhere"),
            ..key.clone()
        };
        assert!(discoveries.begin(moved.clone(), now).is_none());
        discoveries.wake(SessionProvider::Claude, key.pane);
        // It lands stale, but the new key's lookup is waiting on it.
        assert!(!discoveries.finish_with_evidence(
            &request,
            Some(&moved),
            now,
            false,
            LookupEvidence::default()
        ));
        assert!(discoveries.take_redispatch());
        assert!(discoveries.begin(moved, now).is_some());
    }

    #[test]
    fn a_wake_between_lookups_needs_no_extra_dispatch() {
        // The waking caller dispatches itself; nothing is in flight to hold it.
        let key = claude_key();
        let mut discoveries = SessionDiscoveries::default();
        discoveries.set_watched(SessionProvider::Claude, true);
        let now = Instant::now();
        let request = discoveries.begin(key.clone(), now).unwrap();
        assert!(discoveries.finish(&request, Some(&key), now, false));
        discoveries.wake(SessionProvider::Claude, key.pane);
        let request = discoveries.begin(key.clone(), now + SECOND).unwrap();
        assert!(discoveries.finish(&request, Some(&key), now + SECOND, false));
        assert!(!discoveries.take_redispatch());
    }

    #[test]
    fn a_record_name_wakes_only_a_pane_of_its_provider() {
        // A stale Claude record named for a pane now running Grok.
        let grok = SessionDiscoveryKey {
            provider: SessionProvider::Grok,
            ..claude_key()
        };
        let mut discoveries = SessionDiscoveries::default();
        discoveries.set_watched(SessionProvider::Claude, true);
        let now = Instant::now();
        let request = discoveries.begin(grok.clone(), now).unwrap();
        assert!(discoveries.finish(&request, Some(&grok), now, false));
        // Grok's first miss waits one second.
        let half = now + Duration::from_millis(500);
        discoveries.wake(SessionProvider::Claude, grok.pane);
        assert!(discoveries.begin(grok.clone(), half).is_none());
        discoveries.wake(SessionProvider::Grok, grok.pane);
        assert!(discoveries.begin(grok, half).is_some());
    }

    #[test]
    fn a_waiting_hook_record_keeps_the_unwatched_cadence() {
        // Its transcript can appear with no write to the hook directory.
        let key = claude_key();
        let mut discoveries = SessionDiscoveries::default();
        discoveries.set_watched(SessionProvider::Claude, true);
        let record = || LookupEvidence {
            hook_record: true,
            ..LookupEvidence::default()
        };
        let mut now = Instant::now();
        for delay in [SECOND, 2 * SECOND, 2 * SECOND] {
            let request = discoveries.begin(key.clone(), now).unwrap();
            assert!(discoveries.finish_with_evidence(&request, Some(&key), now, false, record()));
            assert!(
                discoveries
                    .begin(key.clone(), now + delay - Duration::from_millis(1))
                    .is_none()
            );
            now += delay;
        }
        // An adopted record is confirmed a second later, as unwatched.
        let request = discoveries.begin(key.clone(), now).unwrap();
        assert!(discoveries.finish_with_evidence(&request, Some(&key), now, true, record()));
        now += SECOND;
        let request = discoveries.begin(key.clone(), now).unwrap();
        assert!(discoveries.finish(&request, Some(&key), now, false));
        assert!(
            discoveries.begin(key, now + 2 * SECOND).is_none(),
            "nothing waits now"
        );
    }

    #[test]
    fn watcher_changes_wake_only_their_provider() {
        let claude = claude_key();
        let grok = SessionDiscoveryKey {
            pane: Uuid::new_v4(),
            provider: SessionProvider::Grok,
            ..claude_key()
        };
        let mut discoveries = SessionDiscoveries::default();
        let now = Instant::now();
        let before = discoveries.begin(claude.clone(), now).unwrap();
        let request = discoveries.begin(grok.clone(), now).unwrap();
        assert!(discoveries.finish(&request, Some(&grok), now, false));
        // A lookup that began before the watcher may have missed a write.
        discoveries.set_watched(SessionProvider::Claude, true);
        assert!(discoveries.finish(&before, Some(&claude), now, false));
        let request = discoveries.begin(claude.clone(), now).unwrap();
        assert!(
            discoveries.begin(grok.clone(), now).is_none(),
            "Grok keeps its delay"
        );
        assert!(discoveries.finish(&request, Some(&claude), now, false));
        discoveries.set_watched(SessionProvider::Claude, true);
        assert!(
            discoveries
                .begin(claude.clone(), now + 2 * SECOND)
                .is_none(),
            "still watched: no wake"
        );
        // Unwatched, no event will come: poll now and at the usual cadence.
        discoveries.set_watched(SessionProvider::Claude, false);
        let later = now + 2 * SECOND;
        let request = discoveries.begin(claude.clone(), later).unwrap();
        assert!(discoveries.finish(&request, Some(&claude), later, false));
        assert!(discoveries.begin(claude, later + 2 * SECOND).is_some());
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
