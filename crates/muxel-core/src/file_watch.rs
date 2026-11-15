//! Routing for muxel's one file watcher.
//!
//! Each registration names the paths it needs watched and how an event under
//! them becomes a change for its target. Registrations that need the same
//! path share one OS watch. Nothing here touches the file system: the app owns
//! the watcher backend, feeds its events in, and applies the watch and unwatch
//! calls this hands back.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

/// One registration with the router.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WatchId(u64);

/// How far an OS watch reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recursion {
    /// The directory's own entries.
    Dir,
    /// Everything beneath the directory.
    Tree,
}

/// One directory under every name a backend may report it by: as given, and
/// canonical, because FSEvents names `/private/var` for `/var`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirNames(Vec<PathBuf>);

impl DirNames {
    /// `given`, plus `canonical` when the caller resolved one that differs.
    pub fn new(given: PathBuf, canonical: Option<PathBuf>) -> Self {
        let mut names = vec![given];
        if let Some(canonical) = canonical
            && canonical != names[0]
        {
            names.push(canonical);
        }
        Self(names)
    }

    /// These names and `name` too, when it's another.
    pub fn also(mut self, name: PathBuf) -> Self {
        if !self.is(&name) {
            self.0.push(name);
        }
        self
    }

    pub fn is(&self, path: &Path) -> bool {
        self.0.iter().any(|name| name == path)
    }

    /// `path` relative to this directory, under whichever name it's below.
    pub fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        self.0.iter().find_map(|name| path.strip_prefix(name).ok())
    }
}

/// What a diff pane's text depends on: `git diff HEAD -- .` shows tracked
/// files only, so only those and the git state behind HEAD can change it.
#[derive(Debug)]
pub struct TrackedFiles {
    /// The diff's directory.
    pub root: DirNames,
    /// Tracked files under `root`, relative to it, as `git ls-files` lists them.
    pub files: HashSet<PathBuf>,
    /// The directories holding them and their ancestors below `root`.
    pub dirs: HashSet<PathBuf>,
    /// HEAD's target: the current branch's loose ref, `packed-refs`, reftable's
    /// `tables.list`. A change there re-runs the diff.
    pub refs: Vec<DirNames>,
    /// HEAD and the index. A change there re-runs the diff and re-lists the
    /// tracked files: an add, a removal, a branch switch.
    pub relist: Vec<DirNames>,
}

impl TrackedFiles {
    pub fn new(
        root: DirNames,
        files: impl IntoIterator<Item = PathBuf>,
        refs: Vec<DirNames>,
        relist: Vec<DirNames>,
    ) -> Self {
        let files: HashSet<PathBuf> = files.into_iter().collect();
        let mut dirs = HashSet::new();
        for file in &files {
            for dir in file.ancestors().skip(1) {
                if dir.as_os_str().is_empty() || !dirs.insert(dir.to_path_buf()) {
                    break;
                }
            }
        }
        Self {
            root,
            files,
            dirs,
            refs,
            relist,
        }
    }

    /// Route one event's paths. A tracked file counts for any change. A
    /// tracked directory counts when it's created, removed or renamed, and
    /// re-lists too, so a watch per directory follows it; its write time
    /// moving only means an entry inside changed, which a tracked file's own
    /// event reports.
    fn route(&self, event: &WatchEvent, change: &mut Change) {
        let moved = matches!(
            event.kind,
            EventKind::Create | EventKind::Remove | EventKind::Rename
        );
        for path in &event.paths {
            if self.relist.iter().any(|name| name.is(path)) {
                change.touched = true;
                change.relist = true;
            } else if self.refs.iter().any(|name| name.is(path)) {
                change.touched = true;
            } else if let Some(rel) = self.root.relative(path) {
                if self.files.contains(rel) {
                    change.touched = true;
                } else if moved && self.dirs.contains(rel) {
                    change.touched = true;
                    change.relist = true;
                }
            }
        }
    }
}

/// How an event under a registration's paths becomes a change.
#[derive(Clone, Debug)]
pub enum WatchFilter {
    /// `<dir>/<pane>.json`, the extension in any ASCII case: that pane. A
    /// session hook installs its record under that name.
    RecordFiles { dir: DirNames },
    /// A diff pane's sources.
    Tracked(Arc<TrackedFiles>),
}

/// What a registration asks for.
#[derive(Clone, Debug)]
pub struct WatchSpec {
    /// The OS watches it needs.
    pub paths: Vec<(PathBuf, Recursion)>,
    pub filter: WatchFilter,
    /// Directories whose own removal or rename leaves it blind. inotify's
    /// watches follow a moved directory or drop a deleted one, and Windows
    /// reports nothing on a deleted directory's own watch, so writes to a
    /// recreated path would go unseen.
    pub lose_on: Vec<DirNames>,
}

/// What happened, as far as routing cares; the app maps its backend's kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Access,
    Create,
    Modify,
    Rename,
    Remove,
    Other,
}

/// One backend event.
#[derive(Clone, Debug)]
pub struct WatchEvent {
    pub kind: EventKind,
    pub paths: Vec<PathBuf>,
    /// The backend may have dropped events.
    pub rescan: bool,
}

/// A registration's news since the last drain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    /// Panes the events named.
    pub hits: HashSet<Uuid>,
    /// Something the registration's target depends on changed.
    pub touched: bool,
    /// What it depends on may have changed too (see [`TrackedFiles::relist`]).
    pub relist: bool,
    /// Events may have been lost: every pane of the target is due.
    pub rescan: bool,
    /// Blind from now on. It routes nothing more; its owner goes back to
    /// polling and unregisters it.
    pub lost: bool,
}

impl Change {
    fn is_empty(&self) -> bool {
        self.hits.is_empty() && !self.touched && !self.rescan && !self.lost
    }

    fn merge(&mut self, other: Change) {
        self.hits.extend(other.hits);
        self.touched |= other.touched;
        self.relist |= other.relist;
        self.rescan |= other.rescan;
        self.lost |= other.lost;
    }
}

/// An OS watch for the app to add or remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OsWatch {
    Watch(PathBuf, Recursion),
    Unwatch(PathBuf),
}

/// Everything the router gathered since the last drain.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drained {
    pub changes: Vec<(WatchId, Change)>,
    /// The watcher itself failed: every registration is blind, and the app
    /// should stop it.
    pub failed: bool,
}

struct Registration {
    spec: WatchSpec,
    lost: bool,
}

/// The routing table behind the app's one watcher.
pub struct WatchRouter {
    next: u64,
    registrations: BTreeMap<WatchId, Registration>,
    /// Each watched path and the registrations that need it, with the depth
    /// each asked for. The OS watch takes the deepest.
    os: HashMap<PathBuf, Vec<(WatchId, Recursion)>>,
    /// Watched paths whose OS watch the backend may have dropped (see
    /// [`Self::mark_stale`]), or a failed apply may have taken (see
    /// [`Self::failed`]). The next registration or respec that wants one
    /// adds its watch again, though its depth stays.
    stale: HashSet<PathBuf>,
    pending: BTreeMap<WatchId, Change>,
    failed: bool,
    /// Changing the watched set restarts the backend's event stream, and
    /// events in the gap are lost (notify's FSEvents backend).
    restarts_stream: bool,
}

fn depth(users: &[(WatchId, Recursion)]) -> Option<Recursion> {
    if users.is_empty() {
        None
    } else if users.iter().any(|(_, depth)| *depth == Recursion::Tree) {
        Some(Recursion::Tree)
    } else {
        Some(Recursion::Dir)
    }
}

/// The calls that take `path`'s OS watch from `was` to `now`. `again` adds
/// a watch that stays afresh, because the backend dropped it.
fn retarget(
    path: &Path,
    was: Option<Recursion>,
    now: Option<Recursion>,
    again: bool,
    ops: &mut Vec<OsWatch>,
) {
    if was == now && !(again && now.is_some()) {
        return;
    }
    if was.is_some() {
        ops.push(OsWatch::Unwatch(path.to_path_buf()));
    }
    if let Some(now) = now {
        ops.push(OsWatch::Watch(path.to_path_buf(), now));
    }
}

/// Each path either list names, once and in order, with the depth `new` asks
/// for there (the deepest, where it names a path twice), or `None` where only
/// `old` does. Generic so a test can count the comparisons it makes.
fn respec_depths<'a, K: Eq + Hash>(
    old: &'a [(K, Recursion)],
    new: &'a [(K, Recursion)],
) -> Vec<(&'a K, Option<Recursion>)> {
    let mut wanted: HashMap<&K, Recursion> = HashMap::with_capacity(new.len());
    for (path, recursion) in new {
        let depth = wanted.entry(path).or_insert(*recursion);
        if *recursion == Recursion::Tree {
            *depth = Recursion::Tree;
        }
    }
    let mut listed = HashSet::with_capacity(old.len() + new.len());
    let mut paths = Vec::with_capacity(old.len() + new.len());
    for (path, _) in old.iter().chain(new) {
        if listed.insert(path) {
            paths.push((path, wanted.get(path).copied()));
        }
    }
    paths
}

/// The pane a hook record's path names: `<pane>.json`, the extension in any
/// case. Windows and macOS (by default) open a `.JSON` record at the path
/// discovery reads; elsewhere such a name costs one lookup that finds nothing.
fn record_pane(path: &Path) -> Option<Uuid> {
    let name = path.file_name()?.to_str()?;
    let split = name.len().checked_sub(".json".len())?;
    if !name.get(split..)?.eq_ignore_ascii_case(".json") {
        return None;
    }
    Uuid::parse_str(name.get(..split)?).ok()
}

impl WatchRouter {
    /// `restarts_stream`: the backend restarts its event stream whenever the
    /// watched set changes (macOS).
    pub fn new(restarts_stream: bool) -> Self {
        Self {
            next: 0,
            registrations: BTreeMap::new(),
            os: HashMap::new(),
            stale: HashSet::new(),
            pending: BTreeMap::new(),
            failed: false,
            restarts_stream,
        }
    }

    /// Add a registration. Returns its id and the OS watch calls to apply. If
    /// one fails, the caller unregisters it again.
    pub fn register(&mut self, spec: WatchSpec) -> (WatchId, Vec<OsWatch>) {
        let id = WatchId(self.next);
        self.next += 1;
        let mut ops = Vec::new();
        for (path, recursion) in &spec.paths {
            let users = self.os.entry(path.clone()).or_default();
            let was = depth(users);
            users.push((id, *recursion));
            let now = depth(users);
            retarget(path, was, now, self.stale.remove(path), &mut ops);
        }
        self.rescan_others_if_restarted(id, &ops);
        self.registrations
            .insert(id, Registration { spec, lost: false });
        (id, ops)
    }

    /// Drop a registration and anything pending for it. Returns the OS watch
    /// calls to apply: the paths no other registration needs any more.
    pub fn unregister(&mut self, id: WatchId) -> Vec<OsWatch> {
        self.pending.remove(&id);
        let Some(registration) = self.registrations.remove(&id) else {
            return Vec::new();
        };
        let mut ops = Vec::new();
        for (path, _) in &registration.spec.paths {
            let Some(users) = self.os.get_mut(path) else {
                continue;
            };
            let was = depth(users);
            users.retain(|(user, _)| *user != id);
            let now = depth(users);
            if now.is_none() {
                self.os.remove(path);
                self.stale.remove(path);
            }
            retarget(path, was, now, false, &mut ops);
        }
        self.rescan_others_if_restarted(id, &ops);
        ops
    }

    /// Give a registration a new spec, keeping its id and anything pending.
    /// Returns the OS watch calls to apply: only for the paths whose watch
    /// changes, so events on a path both specs share keep arriving.
    pub fn respec(&mut self, id: WatchId, spec: WatchSpec) -> Vec<OsWatch> {
        let Some(registration) = self.registrations.get_mut(&id) else {
            return Vec::new();
        };
        let old = std::mem::replace(&mut registration.spec, spec);
        let new = &registration.spec;
        // A re-plan that lists the same paths, none of them dropped by the
        // backend, changes no watch.
        let stale = &self.stale;
        if old.paths == new.paths
            && (stale.is_empty() || !new.paths.iter().any(|(path, _)| stale.contains(path)))
        {
            return Vec::new();
        }
        let mut ops = Vec::new();
        for (path, wanted) in respec_depths(&old.paths, &new.paths) {
            let users = self.os.entry(path.clone()).or_default();
            let was = depth(users);
            users.retain(|(user, _)| *user != id);
            users.extend(wanted.map(|recursion| (id, recursion)));
            let now = depth(users);
            // A dropped watch is added again by a registration that still
            // wants it. One only others want waits for their respec, so a
            // directory that isn't back yet can't fail this one's.
            let again = wanted.is_some() && self.stale.remove(path);
            if now.is_none() {
                self.os.remove(path);
                self.stale.remove(path);
            }
            retarget(path, was, now, again, &mut ops);
        }
        self.rescan_others_if_restarted(id, &ops);
        ops
    }

    fn rescan_others_if_restarted(&mut self, changed: WatchId, ops: &[OsWatch]) {
        if !self.restarts_stream || ops.is_empty() {
            return;
        }
        for (id, registration) in &self.registrations {
            if *id != changed && !registration.lost {
                self.pending.entry(*id).or_default().rescan = true;
            }
        }
    }

    /// Route one backend event. Access events are skipped so that a platform
    /// reporting reads could never turn a lookup into another wake.
    pub fn event(&mut self, event: &WatchEvent) {
        for (id, registration) in &mut self.registrations {
            if registration.lost {
                continue;
            }
            let mut change = Change {
                rescan: event.rescan,
                ..Change::default()
            };
            let lost = matches!(event.kind, EventKind::Remove | EventKind::Rename)
                && event
                    .paths
                    .iter()
                    .any(|path| registration.spec.lose_on.iter().any(|dir| dir.is(path)));
            if lost {
                change.lost = true;
                registration.lost = true;
            } else if event.kind != EventKind::Access {
                match &registration.spec.filter {
                    WatchFilter::RecordFiles { dir } => change.hits.extend(
                        event
                            .paths
                            .iter()
                            .filter(|path| path.parent().is_some_and(|parent| dir.is(parent)))
                            .filter_map(|path| record_pane(path)),
                    ),
                    WatchFilter::Tracked(tracked) => tracked.route(event, &mut change),
                }
            }
            if !change.is_empty() {
                self.pending.entry(*id).or_default().merge(change);
            }
        }
        self.mark_stale(event);
    }

    /// A watched directory removed or renamed. inotify drops its watch and,
    /// for a moved one, every watch under it, reporting nothing for those.
    /// A respec whose paths stay the same would leave them all unwatched
    /// once the directories are back, so they're marked to be added again,
    /// and every registration holding one re-plans: one rooted under a
    /// moved directory sees no event of its own. A watch's own removal or
    /// move is reported through that watch, so a Create needs no mark. The
    /// pass over the watched paths runs only for an event naming one,
    /// which is when notify drops any.
    fn mark_stale(&mut self, event: &WatchEvent) {
        if !matches!(event.kind, EventKind::Remove | EventKind::Rename) {
            return;
        }
        for gone in &event.paths {
            if !self.os.contains_key(gone) {
                continue;
            }
            let under: Vec<PathBuf> = self
                .os
                .keys()
                .filter(|path| path.starts_with(gone))
                .cloned()
                .collect();
            for path in &under {
                self.mark(path);
            }
        }
    }

    /// The app couldn't apply all of `ops` and dropped the registration
    /// they were for. A path among them that another registration still
    /// holds may have lost its OS watch on the way, as when an unwatch went
    /// through and the watch after it failed at the inotify limit. Mark it
    /// to be added again, and have its holders re-plan.
    pub fn failed(&mut self, ops: &[OsWatch]) {
        for op in ops {
            let (OsWatch::Watch(path, _) | OsWatch::Unwatch(path)) = op;
            self.mark(path);
        }
    }

    /// Mark `path`, if it's watched, to be added again (see `stale`), and
    /// tell every live registration holding it to rescan.
    fn mark(&mut self, path: &Path) {
        let Some(users) = self.os.get(path) else {
            return;
        };
        self.stale.insert(path.to_path_buf());
        for (id, _) in users {
            if self.registrations.get(id).is_some_and(|r| !r.lost) {
                self.pending.entry(*id).or_default().rescan = true;
            }
        }
    }

    /// The backend reported an error. One naming paths blinds the
    /// registrations whose watches cover them; one without paths blinds the
    /// watcher.
    pub fn error(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() {
            self.failed = true;
        }
        for (id, registration) in &mut self.registrations {
            let named = paths.iter().any(|error| {
                registration
                    .spec
                    .paths
                    .iter()
                    .any(|(watched, recursion)| match recursion {
                        Recursion::Dir => error == watched || error.parent() == Some(watched),
                        Recursion::Tree => error.starts_with(watched),
                    })
            });
            if !registration.lost && (paths.is_empty() || named) {
                registration.lost = true;
                self.pending.entry(*id).or_default().lost = true;
            }
        }
    }

    /// Whether a drain would find anything.
    pub fn has_news(&self) -> bool {
        self.failed || !self.pending.is_empty()
    }

    pub fn take(&mut self) -> Drained {
        Drained {
            changes: std::mem::take(&mut self.pending).into_iter().collect(),
            failed: self.failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Change, DirNames, EventKind, OsWatch, Recursion, TrackedFiles, WatchEvent, WatchFilter,
        WatchId, WatchRouter, WatchSpec, respec_depths,
    };
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::hash::{Hash, Hasher};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use uuid::Uuid;

    thread_local! {
        static COMPARED: Cell<u64> = const { Cell::new(0) };
    }

    /// A path that counts the comparisons and hashes made of it.
    #[derive(Debug)]
    struct Counted(u32);

    impl PartialEq for Counted {
        fn eq(&self, other: &Self) -> bool {
            COMPARED.set(COMPARED.get() + 1);
            self.0 == other.0
        }
    }

    impl Eq for Counted {}

    impl Hash for Counted {
        fn hash<H: Hasher>(&self, state: &mut H) {
            COMPARED.set(COMPARED.get() + 1);
            self.0.hash(state);
        }
    }

    /// What pairing `n` old paths with `n` new ones costs, half of them
    /// shared, in comparisons and hashes.
    fn pairing_work(n: u32) -> u64 {
        let list = |from: u32| -> Vec<(Counted, Recursion)> {
            (from..from + n)
                .map(|i| (Counted(i), Recursion::Dir))
                .collect()
        };
        let (old, new) = (list(0), list(n / 2));
        COMPARED.set(0);
        let paired = respec_depths(&old, &new);
        let work = COMPARED.get();
        assert_eq!(paired.len(), (n + n / 2) as usize);
        let dropped = paired.iter().filter(|(_, wanted)| wanted.is_none());
        assert_eq!(dropped.count(), (n / 2) as usize);
        work
    }

    /// A diff pane respecs on the UI thread, holding the router, after every
    /// index write, with up to 4,096 paths on Linux. Pairing its old list
    /// with the new grows with the paths, not with their square.
    #[test]
    fn pairing_a_respecs_paths_grows_with_them_not_their_square() {
        let (small, large) = (pairing_work(1_000), pairing_work(4_000));
        assert!(
            large <= 5 * small,
            "{small} comparisons at 1,000 paths, {large} at 4,000"
        );
    }

    #[test]
    fn a_diff_wakes_for_tracked_files_and_git_state_only() {
        let root = PathBuf::from("repo");
        let git = root.join(".git");
        let tracked = TrackedFiles::new(
            names(&root),
            ["src/main.rs", "README.md"].map(PathBuf::from),
            vec![
                names(&git.join("refs/heads/main")),
                names(&git.join("packed-refs")),
            ],
            vec![names(&git.join("HEAD")), names(&git.join("index"))],
        );
        let mut router = WatchRouter::new(false);
        router.register(WatchSpec {
            paths: vec![(root.clone(), Recursion::Tree)],
            filter: WatchFilter::Tracked(Arc::new(tracked)),
            lose_on: Vec::new(),
        });
        // A build, a new file beside tracked ones, git's own writes, a tracked
        // directory's write time, a read.
        for (kind, path) in [
            (EventKind::Create, root.join("target/debug/muxel.exe")),
            (EventKind::Modify, root.join("target")),
            (EventKind::Create, root.join("src/scratch.rs")),
            (EventKind::Modify, root.join("src")),
            (EventKind::Create, git.join("objects/ab/cdef")),
            (EventKind::Modify, git.join("logs/HEAD")),
            (EventKind::Access, root.join("README.md")),
        ] {
            router.event(&event(kind, &[path]));
        }
        assert_eq!(change(&mut router), Change::default());
        let touched = Change {
            touched: true,
            ..Change::default()
        };
        router.event(&event(EventKind::Modify, &[root.join("src/main.rs")]));
        assert_eq!(change(&mut router), touched);
        // The branch moving re-runs the diff.
        router.event(&event(
            EventKind::Rename,
            &[
                git.join("refs/heads/main.lock"),
                git.join("refs/heads/main"),
            ],
        ));
        assert_eq!(change(&mut router), touched);
        // The index or HEAD changing, or a tracked directory coming or going,
        // re-lists as well.
        let relist = Change {
            relist: true,
            ..touched
        };
        router.event(&event(
            EventKind::Rename,
            &[git.join("index.lock"), git.join("index")],
        ));
        assert_eq!(change(&mut router), relist);
        for kind in [EventKind::Rename, EventKind::Remove, EventKind::Create] {
            router.event(&event(kind, &[root.join("src")]));
            assert_eq!(change(&mut router), relist, "{kind:?}");
        }
    }

    fn root_path() -> PathBuf {
        Path::new("data").join("bindings").join("claude")
    }

    fn names(path: &Path) -> DirNames {
        DirNames::new(path.to_path_buf(), None)
    }

    /// Claude's registration: the hook directory and its parent, both
    /// watched without recursion and both lost on removal or rename.
    fn records(dir: &Path) -> WatchSpec {
        let parent = dir.parent().unwrap();
        WatchSpec {
            paths: vec![
                (dir.to_path_buf(), Recursion::Dir),
                (parent.to_path_buf(), Recursion::Dir),
            ],
            filter: WatchFilter::RecordFiles { dir: names(dir) },
            lose_on: vec![names(dir), names(parent)],
        }
    }

    fn claude() -> (WatchRouter, WatchId) {
        let mut router = WatchRouter::new(false);
        let (id, _) = router.register(records(&root_path()));
        (router, id)
    }

    fn event(kind: EventKind, paths: &[PathBuf]) -> WatchEvent {
        WatchEvent {
            kind,
            paths: paths.to_vec(),
            rescan: false,
        }
    }

    fn file(name: &str) -> PathBuf {
        root_path().join(name)
    }

    /// The one registration's change, or the empty one.
    fn change(router: &mut WatchRouter) -> Change {
        let drained = router.take();
        assert!(drained.changes.len() <= 1);
        drained
            .changes
            .into_iter()
            .next()
            .map(|(_, change)| change)
            .unwrap_or_default()
    }

    #[test]
    fn only_the_installed_record_name_wakes_its_pane() {
        let pane = Uuid::from_u128(0x5eed);
        let name = format!("{pane}.json");
        let (mut router, _) = claude();
        for (kind, name) in [
            (EventKind::Create, format!("{pane}.json.4242.tmp")),
            (EventKind::Modify, format!("{pane}.json.4242.tmp")),
            (EventKind::Modify, format!("{pane}.claimed.json")),
            (EventKind::Access, name.clone()),
            (EventKind::Create, "settings".into()),
            (EventKind::Modify, "not-a-pane.json".into()),
        ] {
            router.event(&event(kind, &[file(&name)]));
        }
        // The parent's watch names its own entries: a record-like name there
        // isn't the hook's.
        router.event(&event(
            EventKind::Create,
            &[root_path().with_file_name(&name)],
        ));
        assert_eq!(change(&mut router), Change::default());
        router.event(&event(EventKind::Rename, &[file(&name)]));
        let change = change(&mut router);
        assert_eq!(change.hits, HashSet::from([pane]));
        assert!(!change.rescan && !change.lost);
    }

    #[test]
    fn a_two_path_rename_and_any_extension_case_wake_the_pane() {
        let renamed = Uuid::from_u128(0xa11);
        let shouted = Uuid::from_u128(0xb0b);
        let (mut router, _) = claude();
        router.event(&event(
            EventKind::Rename,
            &[
                file(&format!("{renamed}.json.7.tmp")),
                file(&format!("{renamed}.json")),
            ],
        ));
        router.event(&event(
            EventKind::Create,
            &[file(&format!("{shouted}.JSON"))],
        ));
        assert_eq!(change(&mut router).hits, HashSet::from([renamed, shouted]));
    }

    #[test]
    fn removing_or_renaming_the_watched_directory_fails_the_watch() {
        let parent = root_path().parent().unwrap().to_path_buf();
        for path in [root_path(), parent.clone()] {
            for kind in [EventKind::Remove, EventKind::Rename] {
                let (mut router, _) = claude();
                router.event(&event(kind, std::slice::from_ref(&path)));
                assert!(change(&mut router).lost, "{kind:?} {}", path.display());
                // Blind now: a record write is no longer reported.
                router.event(&event(
                    EventKind::Create,
                    &[file(&format!("{}.json", Uuid::nil()))],
                ));
                assert_eq!(change(&mut router), Change::default());
            }
        }
        // Windows reports the directory's write time through the parent's
        // watch on every record write; that isn't a loss. Nor is a sibling
        // of the parent going away.
        let (mut router, _) = claude();
        for kind in [EventKind::Modify, EventKind::Create] {
            router.event(&event(kind, &[root_path()]));
        }
        router.event(&event(EventKind::Remove, &[parent.with_file_name("ssh")]));
        assert_eq!(change(&mut router), Change::default());
    }

    #[test]
    fn lost_events_and_errors_are_reported_separately() {
        let (mut router, _) = claude();
        router.event(&WatchEvent {
            kind: EventKind::Other,
            paths: Vec::new(),
            rescan: true,
        });
        let drained = router.take();
        let change = &drained.changes[0].1;
        assert!(change.rescan && !change.lost && change.hits.is_empty());
        assert!(!drained.failed);
        router.error(&[]);
        let drained = router.take();
        assert!(drained.failed && drained.changes[0].1.lost);
    }

    #[test]
    fn a_loss_blinds_only_its_own_registration() {
        let mut router = WatchRouter::new(false);
        let claude_dir = root_path();
        let muse_dir = root_path().with_file_name("muse");
        let (claude, _) = router.register(records(&claude_dir));
        let (muse, _) = router.register(records(&muse_dir));
        let pane = Uuid::from_u128(7);
        let record = format!("{pane}.json");
        let lost = Change {
            lost: true,
            ..Change::default()
        };
        // An error naming a path blinds the watches covering it. Both share
        // the parent's watch, which covers only the parent's own entries.
        router.error(&[muse_dir.join(&record)]);
        router.event(&event(EventKind::Create, &[claude_dir.join(&record)]));
        let drained = router.take();
        assert!(!drained.failed);
        assert_eq!(
            drained.changes,
            vec![
                (
                    claude,
                    Change {
                        hits: HashSet::from([pane]),
                        ..Change::default()
                    }
                ),
                (muse, lost.clone()),
            ]
        );
        // Losing a registration's directory blinds it alone.
        let (muse, _) = router.register(records(&muse_dir));
        router.event(&event(EventKind::Remove, std::slice::from_ref(&claude_dir)));
        router.event(&event(EventKind::Create, &[muse_dir.join(&record)]));
        router.event(&event(EventKind::Create, &[claude_dir.join(&record)]));
        assert_eq!(
            router.take().changes,
            vec![
                (claude, lost),
                (
                    muse,
                    Change {
                        hits: HashSet::from([pane]),
                        ..Change::default()
                    }
                ),
            ]
        );
    }

    #[test]
    fn overlapping_registrations_share_one_os_watch() {
        let mut router = WatchRouter::new(false);
        let parent = root_path().parent().unwrap().to_path_buf();
        let claude_dir = root_path();
        let muse_dir = root_path().with_file_name("muse");
        let (claude, ops) = router.register(records(&claude_dir));
        assert_eq!(
            ops,
            vec![
                OsWatch::Watch(claude_dir.clone(), Recursion::Dir),
                OsWatch::Watch(parent.clone(), Recursion::Dir),
            ]
        );
        // The shared parent is already watched.
        let (muse, ops) = router.register(records(&muse_dir));
        assert_eq!(ops, vec![OsWatch::Watch(muse_dir.clone(), Recursion::Dir)]);
        // A deeper ask deepens the shared watch, and leaving undoes it.
        let (tree, ops) = router.register(WatchSpec {
            paths: vec![(parent.clone(), Recursion::Tree)],
            filter: WatchFilter::RecordFiles {
                dir: names(&parent),
            },
            lose_on: Vec::new(),
        });
        assert_eq!(
            ops,
            vec![
                OsWatch::Unwatch(parent.clone()),
                OsWatch::Watch(parent.clone(), Recursion::Tree),
            ]
        );
        assert_eq!(
            router.unregister(tree),
            vec![
                OsWatch::Unwatch(parent.clone()),
                OsWatch::Watch(parent.clone(), Recursion::Dir),
            ]
        );
        assert_eq!(
            router.unregister(claude),
            vec![OsWatch::Unwatch(claude_dir)]
        );
        assert_eq!(
            router.unregister(muse),
            vec![OsWatch::Unwatch(muse_dir), OsWatch::Unwatch(parent)]
        );
        assert!(router.unregister(muse).is_empty());
    }

    /// A diff pane re-lists after `git add`: its new spec keeps the watches it
    /// shares with the old one open, and news it gathered meanwhile stays.
    #[test]
    fn a_respec_moves_only_the_watches_that_differ() {
        let root = PathBuf::from("repo");
        let spec = |files: &[&str], dirs: &[&str]| WatchSpec {
            paths: dirs
                .iter()
                .map(|dir| (root.join(dir), Recursion::Dir))
                .collect(),
            filter: WatchFilter::Tracked(Arc::new(TrackedFiles::new(
                names(&root),
                files.iter().map(PathBuf::from),
                Vec::new(),
                Vec::new(),
            ))),
            lose_on: Vec::new(),
        };
        let touched = |router: &mut WatchRouter| -> Vec<WatchId> {
            router
                .take()
                .changes
                .into_iter()
                .filter(|(_, change)| change.touched)
                .map(|(id, _)| id)
                .collect()
        };
        let mut router = WatchRouter::new(false);
        let (id, _) = router.register(spec(&["a/old.rs", "b/kept.rs"], &["a", "b"]));
        // Another pane on the same repository shares `a`.
        let (other, _) = router.register(spec(&["a/old.rs"], &["a"]));
        router.event(&event(EventKind::Modify, &[root.join("b/kept.rs")]));
        // Its last file under `a` is gone, but the other pane still needs `a`.
        let ops = router.respec(id, spec(&["b/kept.rs", "c/new.rs"], &["b", "c"]));
        assert_eq!(ops, vec![OsWatch::Watch(root.join("c"), Recursion::Dir)]);
        assert_eq!(touched(&mut router), vec![id], "news from before stays");
        router.event(&event(EventKind::Modify, &[root.join("c/new.rs")]));
        assert_eq!(touched(&mut router), vec![id], "the new list routes");
        router.event(&event(EventKind::Modify, &[root.join("a/old.rs")]));
        assert_eq!(touched(&mut router), vec![other], "the old one doesn't");
        assert_eq!(
            router.respec(id, spec(&[], &[])),
            vec![
                OsWatch::Unwatch(root.join("b")),
                OsWatch::Unwatch(root.join("c")),
            ]
        );
    }

    /// A diff pane over `root` where each directory is its own watch (Linux).
    fn tracked(root: &Path, files: &[&str], dirs: &[&Path]) -> WatchSpec {
        WatchSpec {
            paths: dirs
                .iter()
                .map(|dir| (dir.to_path_buf(), Recursion::Dir))
                .collect(),
            filter: WatchFilter::Tracked(Arc::new(TrackedFiles::new(
                names(root),
                files.iter().map(PathBuf::from),
                Vec::new(),
                Vec::new(),
            ))),
            lose_on: vec![names(root)],
        }
    }

    /// The watches notify's inotify backend holds. It applies the router's
    /// calls, and on its own drops the watch on a directory that's removed
    /// or moved, and every watch under a moved one, reporting none of those.
    #[derive(Default)]
    struct Inotify(HashSet<PathBuf>);

    impl Inotify {
        fn apply(&mut self, ops: &[OsWatch]) {
            for op in ops {
                match op {
                    OsWatch::Watch(path, _) => self.0.insert(path.clone()),
                    OsWatch::Unwatch(path) => self.0.remove(path),
                };
            }
        }

        fn drop_under(&mut self, dir: &Path) {
            self.0.retain(|path| !path.starts_with(dir));
        }
    }

    /// `rm -r src && mkdir src` before the pane re-plans: the plan after
    /// lists the same directories as before, and the respec watches the new
    /// `src` all the same.
    #[test]
    fn a_folder_back_before_the_re_plan_is_watched_again() {
        let root = PathBuf::from("repo");
        let src = root.join("src");
        let spec = || tracked(&root, &["src/a.rs"], &[&root, &src]);
        let mut router = WatchRouter::new(false);
        let mut backend = Inotify::default();
        let (id, ops) = router.register(spec());
        backend.apply(&ops);
        backend.drop_under(&src);
        router.event(&event(EventKind::Remove, std::slice::from_ref(&src)));
        router.event(&event(EventKind::Create, std::slice::from_ref(&src)));
        assert!(change(&mut router).relist);
        backend.apply(&router.respec(id, spec()));
        assert_eq!(backend.0, HashSet::from([root.clone(), src.clone()]));
        assert!(
            router.respec(id, spec()).is_empty(),
            "added again once, not at every re-plan"
        );
    }

    /// `mv src away && mkdir -p src/sub`, where pane A over the repository
    /// watches `src` and pane B over `src/sub` watches only that. Dropping
    /// the watches under `src` reports nothing, so no event names B's
    /// directory; B is told to re-plan all the same, and the two respecs
    /// watch both new directories.
    #[test]
    fn a_moved_folder_rewatches_every_pane_under_it() {
        let root = PathBuf::from("repo");
        let src = root.join("src");
        let sub = src.join("sub");
        let a_spec = || tracked(&root, &["src/a.rs"], &[&root, &src]);
        let b_spec = || tracked(&sub, &["b.rs"], &[&sub]);
        let mut router = WatchRouter::new(false);
        let mut backend = Inotify::default();
        let (a, ops) = router.register(a_spec());
        backend.apply(&ops);
        let (b, ops) = router.register(b_spec());
        backend.apply(&ops);
        backend.drop_under(&src);
        router.event(&event(EventKind::Rename, std::slice::from_ref(&src)));
        router.event(&event(EventKind::Create, std::slice::from_ref(&src)));
        let changes = router.take().changes;
        let of = |id: WatchId| -> Change {
            let found = changes.iter().find(|(of, _)| *of == id);
            found.map(|(_, change)| change.clone()).unwrap_or_default()
        };
        assert!(of(a).relist);
        assert!(of(b).rescan && !of(b).lost, "B re-plans");
        let ops = router.respec(a, a_spec());
        assert!(
            !ops.contains(&OsWatch::Watch(sub.clone(), Recursion::Dir)),
            "A leaves B's directory to B, whose plan knows whether it's back"
        );
        backend.apply(&ops);
        backend.apply(&router.respec(b, b_spec()));
        assert_eq!(
            backend.0,
            HashSet::from([root.clone(), src.clone(), sub.clone()])
        );
    }

    #[test]
    fn a_watch_set_change_rescans_the_others_where_the_stream_restarts() {
        let muse_dir = root_path().with_file_name("muse");
        for restarts in [false, true] {
            let mut router = WatchRouter::new(restarts);
            let (claude, _) = router.register(records(&root_path()));
            assert!(!router.has_news(), "nobody else was watching");
            let (muse, _) = router.register(records(&muse_dir));
            let rescanned = |router: &mut WatchRouter| -> Vec<WatchId> {
                router
                    .take()
                    .changes
                    .into_iter()
                    .filter(|(_, change)| change.rescan)
                    .map(|(id, _)| id)
                    .collect()
            };
            let expected = |id| if restarts { vec![id] } else { Vec::new() };
            assert_eq!(rescanned(&mut router), expected(claude));
            // The same watched set again: no restart.
            let (again, ops) = router.register(records(&muse_dir));
            assert!(ops.is_empty());
            assert_eq!(rescanned(&mut router), Vec::new());
            router.unregister(again);
            assert_eq!(rescanned(&mut router), Vec::new());
            router.unregister(muse);
            assert_eq!(rescanned(&mut router), expected(claude));
        }
    }
}
