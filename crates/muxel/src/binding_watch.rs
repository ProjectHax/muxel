//! Wakes Claude session discovery when the session hook writes a pane's record.
//!
//! Without it each Claude pane opens its record path every two seconds, and
//! almost always finds nothing: the hook writes only after `/clear`, `/resume`
//! or `/fork`. Discovery still polls slowly while this runs
//! ([`muxel_core::session_discovery::WATCHED_HOOK_FALLBACK`]), because a
//! watcher can lose events, and at the usual cadence when it can't start.

use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use uuid::Uuid;

/// A directory as given and as a backend may report it: FSEvents names
/// canonical paths (`/private/var` for `/var` on macOS).
#[derive(Clone, Debug)]
struct DirNames(Vec<PathBuf>);

impl DirNames {
    fn new(dir: &Path) -> Self {
        let mut names = vec![dir.to_path_buf()];
        if let Ok(canonical) = dir.canonicalize()
            && canonical != dir
        {
            names.push(canonical);
        }
        Self(names)
    }

    fn is(&self, path: &Path) -> bool {
        self.0.iter().any(|name| name == path)
    }
}

/// The hook directory and its parent, both watched.
#[derive(Clone, Debug)]
pub(crate) struct WatchedDirs {
    root: DirNames,
    parent: Option<DirNames>,
}

impl WatchedDirs {
    pub fn new(root: &Path) -> Self {
        Self {
            root: DirNames::new(root),
            parent: root.parent().map(DirNames::new),
        }
    }

    /// Whether `path` is the hook directory or its parent: losing either
    /// detaches the watch from where the hook writes.
    fn is_watched(&self, path: &Path) -> bool {
        self.root.is(path) || self.parent.as_ref().is_some_and(|parent| parent.is(path))
    }
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

/// What a watcher callback reported, gathered until the UI takes it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct BindingChanges {
    /// Panes whose hook record was written.
    pub panes: HashSet<Uuid>,
    /// Events may have been lost: every Claude pane is due.
    pub rescan: bool,
    /// The watcher reported an error; stop relying on it.
    pub failed: bool,
}

impl BindingChanges {
    /// Add one watcher callback. Only the record the hook installs in the hook
    /// directory counts: `<pane>.json`. Its temporary file is renamed onto
    /// that name, and the `.claimed` name is muxel's own. Access events are
    /// skipped so that a platform reporting reads could never turn a lookup
    /// into another wake. Removing or renaming the hook directory or its
    /// parent fails the watch: inotify's watches follow the moved directory
    /// or drop a deleted one, and Windows reports nothing on a deleted
    /// directory's own watch, so writes to a recreated path would go unseen.
    pub fn record(&mut self, dirs: &WatchedDirs, event: notify::Result<notify::Event>) {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                log::warn!("Claude session hook watcher failed: {error}");
                self.failed = true;
                return;
            }
        };
        if event.need_rescan() {
            self.rescan = true;
        }
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if matches!(
            event.kind,
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
        ) && event.paths.iter().any(|path| dirs.is_watched(path))
        {
            log::warn!("Claude session hook directory was removed or renamed; polling instead");
            self.failed = true;
            return;
        }
        self.panes.extend(
            event
                .paths
                .iter()
                .filter(|path| path.parent().is_some_and(|parent| dirs.root.is(parent)))
                .filter_map(|path| record_pane(path)),
        );
    }

    fn is_empty(&self) -> bool {
        self.panes.is_empty() && !self.rescan && !self.failed
    }
}

/// The handler a backend calls with each event: it records the event and
/// signals the app. The backend owns it until it stops, so the signal closes,
/// and `alive` drops, when the backend's worker goes away.
pub(crate) struct RecordChanges {
    dirs: WatchedDirs,
    changes: Arc<Mutex<BindingChanges>>,
    signal: async_channel::Sender<()>,
    _alive: Arc<()>,
}

impl notify::EventHandler for RecordChanges {
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        let mut changes = self
            .changes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        changes.record(&self.dirs, event);
        if !changes.is_empty() {
            // Full means a signal is already waiting for these changes.
            let _ = self.signal.try_send(());
        }
    }
}

/// A running watcher on the Claude hook directory. Dropping it stops it.
pub(crate) struct ClaudeBindingWatch {
    watcher: Option<Box<dyn Watcher>>,
    changes: Arc<Mutex<BindingChanges>>,
    /// Upgradable while the backend still holds its handler.
    backend: Weak<()>,
}

impl ClaudeBindingWatch {
    /// Watch `dir`, creating it if needed. The receiver gets a signal whenever
    /// changes are waiting in [`Self::take`]; signals coalesce, so one may
    /// stand for many events. The receiver closes if the backend stops.
    ///
    /// The parent is watched too, without recursion, because only it reports
    /// `dir` being removed or renamed on every platform.
    pub fn start(dir: &Path) -> anyhow::Result<(Self, async_channel::Receiver<()>)> {
        Self::start_with(dir, notify::recommended_watcher)
    }

    /// As [`Self::start`], with the backend `backend` builds around the handler.
    pub fn start_with<W: Watcher + 'static>(
        dir: &Path,
        backend: impl FnOnce(RecordChanges) -> notify::Result<W>,
    ) -> anyhow::Result<(Self, async_channel::Receiver<()>)> {
        std::fs::create_dir_all(dir)?;
        let changes = Arc::new(Mutex::new(BindingChanges::default()));
        let (signal, receiver) = async_channel::bounded(1);
        let alive = Arc::new(());
        let backend_alive = Arc::downgrade(&alive);
        let mut watcher = backend(RecordChanges {
            dirs: WatchedDirs::new(dir),
            changes: Arc::clone(&changes),
            signal,
            _alive: alive,
        })?;
        for watched in [Some(dir), dir.parent()].into_iter().flatten() {
            watcher.watch(watched, RecursiveMode::NonRecursive)?;
        }
        Ok((
            Self {
                watcher: Some(Box::new(watcher)),
                changes,
                backend: backend_alive,
            },
            receiver,
        ))
    }

    pub fn take(&self) -> BindingChanges {
        std::mem::take(
            &mut *self
                .changes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }
}

impl Drop for ClaudeBindingWatch {
    /// A live backend is stopped as usual: on app exit, or after it reported
    /// an error. One that already dropped its handler has lost its worker,
    /// and notify's inotify watcher unwraps a send to that worker when
    /// dropped, which would panic on the UI thread. Its remains (a few file
    /// descriptors) are leaked instead; that happens at most once a run.
    fn drop(&mut self) {
        let Some(watcher) = self.watcher.take() else {
            return;
        };
        if self.backend.strong_count() > 0 {
            drop(watcher);
        } else {
            std::mem::forget(watcher);
        }
    }
}

/// A stand-in for notify's backend, for tests that need to play its failures.
#[cfg(test)]
pub(crate) mod test_backend {
    use super::RecordChanges;
    use notify::{Config, EventHandler, RecursiveMode, Watcher, WatcherKind};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// The test's side of a [`TestWatcher`]: it holds the event handler the
    /// watcher was built around, so the test can play an event, or the
    /// backend's worker dying by dropping that handler.
    #[derive(Clone, Default)]
    pub(crate) struct TestBackend {
        handler: Arc<Mutex<Option<RecordChanges>>>,
        dropped: Arc<AtomicBool>,
    }

    impl TestBackend {
        /// What `ClaudeBindingWatch::start_with` takes.
        pub fn build(&self, handler: RecordChanges) -> notify::Result<TestWatcher> {
            *self.handler.lock().unwrap() = Some(handler);
            Ok(TestWatcher {
                handler: Arc::clone(&self.handler),
                dropped: Arc::clone(&self.dropped),
            })
        }

        /// The backend reports `event` while alive. Only the app's watcher
        /// tests play one, and they run on Windows alone.
        #[cfg(windows)]
        pub fn send(&self, event: notify::Result<notify::Event>) {
            self.handler
                .lock()
                .unwrap()
                .as_mut()
                .expect("the backend is alive")
                .handle_event(event);
        }

        /// The worker dies, as inotify's does when its poll fails: the
        /// handler it owned, and with it the signal's sender, is dropped.
        pub fn kill(&self) {
            drop(self.handler.lock().unwrap().take());
        }

        /// Whether the watcher's destructor ran.
        pub fn dropped(&self) -> bool {
            self.dropped.load(Ordering::SeqCst)
        }
    }

    pub(crate) struct TestWatcher {
        handler: Arc<Mutex<Option<RecordChanges>>>,
        dropped: Arc<AtomicBool>,
    }

    impl Watcher for TestWatcher {
        fn new<F: EventHandler>(_: F, _: Config) -> notify::Result<Self> {
            Err(notify::Error::generic("built by TestBackend"))
        }

        fn watch(&mut self, _: &Path, _: RecursiveMode) -> notify::Result<()> {
            Ok(())
        }

        fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
            Ok(())
        }

        fn kind() -> WatcherKind {
            WatcherKind::NullWatcher
        }
    }

    impl Drop for TestWatcher {
        /// notify's inotify watcher sends its worker a shutdown and unwraps
        /// the result, so dropping it after the worker died panics.
        fn drop(&mut self) {
            assert!(
                self.handler.lock().unwrap().is_some(),
                "dropped a watcher whose worker died"
            );
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_backend::TestBackend;
    use super::{BindingChanges, ClaudeBindingWatch, WatchedDirs};
    use notify::event::{AccessKind, CreateKind, Flag, ModifyKind, RemoveKind, RenameMode};
    use notify::{Event, EventKind};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    fn root() -> WatchedDirs {
        WatchedDirs::new(&root_path())
    }

    fn root_path() -> PathBuf {
        Path::new("data").join("bindings").join("claude")
    }

    fn event(kind: EventKind, name: &str) -> notify::Result<Event> {
        Ok(Event::new(kind).add_path(root_path().join(name)))
    }

    #[test]
    fn only_the_installed_record_name_wakes_its_pane() {
        let pane = Uuid::from_u128(0x5eed);
        let name = format!("{pane}.json");
        let mut changes = BindingChanges::default();
        for (kind, file) in [
            (
                EventKind::Create(CreateKind::Any),
                format!("{pane}.json.4242.tmp"),
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                format!("{pane}.json.4242.tmp"),
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                format!("{pane}.claimed.json"),
            ),
            (EventKind::Access(AccessKind::Any), name.clone()),
            (EventKind::Create(CreateKind::Folder), "settings".into()),
            (EventKind::Modify(ModifyKind::Any), "not-a-pane.json".into()),
        ] {
            changes.record(&root(), event(kind, &file));
        }
        // The parent's watch names its own entries: a record-like name there
        // isn't the hook's.
        changes.record(
            &root(),
            Ok(Event::new(EventKind::Create(CreateKind::Any))
                .add_path(root_path().with_file_name(&name))),
        );
        assert_eq!(changes, BindingChanges::default());
        changes.record(
            &root(),
            event(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &name),
        );
        assert_eq!(changes.panes, HashSet::from([pane]));
        assert!(!changes.rescan && !changes.failed);
    }

    #[test]
    fn a_two_path_rename_and_any_extension_case_wake_the_pane() {
        let renamed = Uuid::from_u128(0xa11);
        let shouted = Uuid::from_u128(0xb0b);
        let mut changes = BindingChanges::default();
        changes.record(
            &root(),
            Ok(
                Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                    .add_path(root_path().join(format!("{renamed}.json.7.tmp")))
                    .add_path(root_path().join(format!("{renamed}.json"))),
            ),
        );
        changes.record(
            &root(),
            event(
                EventKind::Create(CreateKind::Any),
                &format!("{shouted}.JSON"),
            ),
        );
        assert_eq!(changes.panes, HashSet::from([renamed, shouted]));
    }

    #[test]
    fn removing_or_renaming_the_watched_directory_fails_the_watch() {
        let parent = root_path().parent().unwrap().to_path_buf();
        for path in [root_path(), parent.clone()] {
            for kind in [
                EventKind::Remove(RemoveKind::Folder),
                EventKind::Remove(RemoveKind::Any),
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            ] {
                let mut changes = BindingChanges::default();
                changes.record(&root(), Ok(Event::new(kind).add_path(path.clone())));
                assert!(changes.failed, "{kind:?} {}", path.display());
            }
        }
        // Windows reports the directory's write time through the parent's
        // watch on every record write; that isn't a loss. Nor is a sibling
        // of the parent going away.
        let mut changes = BindingChanges::default();
        for kind in [
            EventKind::Modify(ModifyKind::Any),
            EventKind::Create(CreateKind::Folder),
        ] {
            changes.record(&root(), Ok(Event::new(kind).add_path(root_path())));
        }
        changes.record(
            &root(),
            Ok(Event::new(EventKind::Remove(RemoveKind::Folder))
                .add_path(parent.with_file_name("ssh"))),
        );
        assert_eq!(changes, BindingChanges::default());
    }

    /// A backend whose worker died has dropped its handler, which closes the
    /// signal. Dropping its watcher then must not run the destructor that
    /// would talk to the dead worker; a live one is stopped as usual.
    #[test]
    fn a_watch_whose_backend_died_is_released_without_its_destructor() {
        let dir = std::env::temp_dir()
            .join(format!("muxel-binding-teardown-{}", Uuid::new_v4()))
            .join("claude");
        let dead = TestBackend::default();
        let (watch, signal) =
            ClaudeBindingWatch::start_with(&dir, |handler| dead.build(handler)).unwrap();
        dead.kill();
        assert!(signal.try_recv().unwrap_err().is_closed());
        drop(watch);
        assert!(!dead.dropped());

        let live = TestBackend::default();
        let (watch, _signal) =
            ClaudeBindingWatch::start_with(&dir, |handler| live.build(handler)).unwrap();
        drop(watch);
        assert!(live.dropped());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn lost_events_and_errors_are_reported_separately() {
        let mut changes = BindingChanges::default();
        changes.record(
            &root(),
            Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)),
        );
        assert!(changes.rescan && !changes.failed && changes.panes.is_empty());
        changes.record(&root(), Err(notify::Error::generic("queue closed")));
        assert!(changes.failed);
    }

    /// Deleting or renaming the hook directory or its parent through the real
    /// backend fails the watch, so discovery goes back to polling the
    /// recreated directory.
    #[test]
    fn losing_the_watched_directory_fails_the_watch_through_the_platform_watcher() {
        for (lost, rename) in [(0, false), (0, true), (1, false), (1, true)] {
            let root = std::env::temp_dir().join(format!("muxel-binding-loss-{}", Uuid::new_v4()));
            let dir = crate::session_binding::claude_binding_dir(&root.join("data"));
            let (watch, signal) = ClaudeBindingWatch::start(&dir).unwrap();
            let gone = dir.ancestors().nth(lost).unwrap().to_path_buf();
            if rename {
                let moved = std::fs::rename(&gone, gone.with_extension("moved"));
                if cfg!(windows) && lost == 1 {
                    // Windows refuses to rename a directory while a watched
                    // directory inside it is open, so the hook's path can't
                    // move out from under the watch there.
                    assert!(moved.is_err());
                    drop(watch);
                    let _ = std::fs::remove_dir_all(&root);
                    continue;
                }
                moved.unwrap();
            } else {
                std::fs::remove_dir_all(&gone).unwrap();
            }
            // As the next hook would.
            std::fs::create_dir_all(&dir).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut failed = false;
            while !failed && Instant::now() < deadline {
                if signal.try_recv().is_err() {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                failed = watch.take().failed;
            }
            drop(watch);
            let _ = std::fs::remove_dir_all(&root);
            assert!(failed, "{}: rename {rename}", gone.display());
        }
    }

    /// The hook's real write (temp file, then rename onto the record name)
    /// reaches the platform watcher as a wake for exactly that pane.
    #[test]
    fn hook_write_wakes_its_pane_through_the_platform_watcher() {
        let root = std::env::temp_dir().join(format!("muxel-binding-watch-{}", Uuid::new_v4()));
        let data = root.join("data");
        let (watch, signal) =
            ClaudeBindingWatch::start(&crate::session_binding::claude_binding_dir(&data)).unwrap();
        let pane = Uuid::new_v4();
        let transcript = root.join(format!("projects/p/{}.jsonl", Uuid::nil()));
        let event = serde_json::json!({
            "hook_event_name": "SessionStart",
            "source": "clear",
            "session_id": Uuid::nil().to_string(),
            "transcript_path": transcript,
            "cwd": root,
        });
        crate::session_binding::write_claude_binding_from_reader(
            &data,
            pane,
            event.to_string().as_bytes(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = BindingChanges::default();
        while !seen.panes.contains(&pane) && Instant::now() < deadline {
            if signal.try_recv().is_err() {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let changes = watch.take();
            seen.panes.extend(changes.panes);
            seen.failed |= changes.failed;
        }
        drop(watch);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(seen.panes, HashSet::from([pane]));
        assert!(!seen.failed);
    }
}
