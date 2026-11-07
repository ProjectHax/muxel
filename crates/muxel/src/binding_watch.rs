//! Wakes Claude session discovery when the session hook writes a pane's record.
//!
//! Without it each Claude pane opens its record path every two seconds, and
//! almost always finds nothing: the hook writes only after `/clear`, `/resume`
//! or `/fork`. Discovery still polls slowly while this runs
//! ([`muxel_core::session_discovery::WATCHED_HOOK_FALLBACK`]), because a
//! watcher can lose events, and at the usual cadence when it can't start.

use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// The watched directory, as given and as a backend may report it: FSEvents
/// names canonical paths (`/private/var` for `/var` on macOS).
#[derive(Clone, Debug)]
pub(crate) struct WatchedRoot {
    names: Vec<PathBuf>,
}

impl WatchedRoot {
    pub fn new(dir: &Path) -> Self {
        let mut names = vec![dir.to_path_buf()];
        if let Ok(canonical) = dir.canonicalize()
            && canonical != dir
        {
            names.push(canonical);
        }
        Self { names }
    }

    fn is(&self, path: &Path) -> bool {
        self.names.iter().any(|name| name == path)
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
    /// Add one watcher callback. Only the record the hook installs in `root`
    /// counts: `<pane>.json`. Its temporary file is renamed onto that name,
    /// and the `.claimed` name is muxel's own. Access events are skipped so
    /// that a platform reporting reads could never turn a lookup into another
    /// wake. Removing or renaming `root` itself fails the watch: inotify drops
    /// a deleted directory's watch, and Windows reports nothing on it at all,
    /// so writes to a recreated directory would go unseen.
    pub fn record(&mut self, root: &WatchedRoot, event: notify::Result<notify::Event>) {
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
        ) && event.paths.iter().any(|path| root.is(path))
        {
            log::warn!("Claude session hook directory was removed or renamed; polling instead");
            self.failed = true;
            return;
        }
        self.panes.extend(
            event
                .paths
                .iter()
                .filter(|path| path.parent().is_some_and(|parent| root.is(parent)))
                .filter_map(|path| record_pane(path)),
        );
    }

    fn is_empty(&self) -> bool {
        self.panes.is_empty() && !self.rescan && !self.failed
    }
}

/// A running watcher on the Claude hook directory. Dropping it stops it.
pub(crate) struct ClaudeBindingWatch {
    _watcher: RecommendedWatcher,
    changes: Arc<Mutex<BindingChanges>>,
}

impl ClaudeBindingWatch {
    /// Watch `dir`, creating it if needed. The receiver gets a signal whenever
    /// changes are waiting in [`Self::take`]; signals coalesce, so one may
    /// stand for many events. The receiver closes if the backend stops.
    ///
    /// The parent is watched too, without recursion, because only it reports
    /// `dir` being removed or renamed on every platform.
    pub fn start(dir: &Path) -> anyhow::Result<(Self, async_channel::Receiver<()>)> {
        std::fs::create_dir_all(dir)?;
        let root = WatchedRoot::new(dir);
        let changes = Arc::new(Mutex::new(BindingChanges::default()));
        let (signal, receiver) = async_channel::bounded(1);
        let pending = Arc::clone(&changes);
        let mut watcher = notify::recommended_watcher(move |event| {
            let mut changes = pending.lock().unwrap_or_else(|poison| poison.into_inner());
            changes.record(&root, event);
            if !changes.is_empty() {
                // Full means a signal is already waiting for these changes.
                let _ = signal.try_send(());
            }
        })?;
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
        if let Some(parent) = dir.parent() {
            watcher.watch(parent, RecursiveMode::NonRecursive)?;
        }
        Ok((
            Self {
                _watcher: watcher,
                changes,
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

#[cfg(test)]
mod tests {
    use super::{BindingChanges, ClaudeBindingWatch, WatchedRoot};
    use notify::event::{AccessKind, CreateKind, Flag, ModifyKind, RemoveKind, RenameMode};
    use notify::{Event, EventKind};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    fn root() -> WatchedRoot {
        WatchedRoot::new(&root_path())
    }

    fn root_path() -> PathBuf {
        Path::new("bindings").join("claude")
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
                .add_path(Path::new("bindings").join(&name))),
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
        for kind in [
            EventKind::Remove(RemoveKind::Folder),
            EventKind::Remove(RemoveKind::Any),
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
        ] {
            let mut changes = BindingChanges::default();
            changes.record(&root(), Ok(Event::new(kind).add_path(root_path())));
            assert!(changes.failed, "{kind:?}");
        }
        // Windows reports the directory's write time through the parent's
        // watch on every record write; that isn't a loss.
        let mut changes = BindingChanges::default();
        for kind in [
            EventKind::Modify(ModifyKind::Any),
            EventKind::Create(CreateKind::Folder),
        ] {
            changes.record(&root(), Ok(Event::new(kind).add_path(root_path())));
        }
        assert_eq!(changes, BindingChanges::default());
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

    /// Deleting or renaming the hook directory through the real backend fails
    /// the watch, so discovery goes back to polling the recreated directory.
    #[test]
    fn losing_the_watched_directory_fails_the_watch_through_the_platform_watcher() {
        for rename in [false, true] {
            let root = std::env::temp_dir().join(format!("muxel-binding-loss-{}", Uuid::new_v4()));
            let dir = crate::session_binding::claude_binding_dir(&root.join("data"));
            let (watch, signal) = ClaudeBindingWatch::start(&dir).unwrap();
            if rename {
                std::fs::rename(&dir, dir.with_file_name("claude-moved")).unwrap();
            } else {
                std::fs::remove_dir_all(&dir).unwrap();
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
            assert!(failed, "rename: {rename}");
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
