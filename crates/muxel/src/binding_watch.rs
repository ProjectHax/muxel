//! Wakes Claude session discovery when the session hook writes a pane's record.
//!
//! Without it each Claude pane opens its record path every two seconds, and
//! almost always finds nothing: the hook writes only after `/clear`, `/resume`
//! or `/fork`. Discovery still polls slowly while this runs
//! ([`muxel_core::session_discovery::WATCHED_HOOK_FALLBACK`]), because a
//! watcher can lose events, and at the usual cadence when it can't start.

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

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
    /// Add one watcher callback. Only the record the hook installs counts:
    /// `<pane>.json`. Its temporary file is renamed onto that name, and the
    /// `.claimed` name is muxel's own. Access events are skipped so that a
    /// platform reporting reads could never turn a lookup into another wake.
    pub fn record(&mut self, event: notify::Result<notify::Event>) {
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
        self.panes.extend(event.paths.iter().filter_map(|path| {
            let stem = path.file_name()?.to_str()?.strip_suffix(".json")?;
            Uuid::parse_str(stem).ok()
        }));
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
    /// stand for many events.
    pub fn start(dir: &Path) -> anyhow::Result<(Self, async_channel::Receiver<()>)> {
        std::fs::create_dir_all(dir)?;
        let changes = Arc::new(Mutex::new(BindingChanges::default()));
        let (signal, receiver) = async_channel::bounded(1);
        let pending = Arc::clone(&changes);
        let mut watcher = notify::recommended_watcher(move |event| {
            let mut changes = pending.lock().unwrap_or_else(|poison| poison.into_inner());
            changes.record(event);
            if !changes.is_empty() {
                // Full means a signal is already waiting for these changes.
                let _ = signal.try_send(());
            }
        })?;
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
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
    use super::{BindingChanges, ClaudeBindingWatch};
    use notify::event::{AccessKind, CreateKind, Flag, ModifyKind, RenameMode};
    use notify::{Event, EventKind};
    use std::collections::HashSet;
    use std::path::Path;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    fn event(kind: EventKind, name: &str) -> notify::Result<Event> {
        Ok(Event::new(kind).add_path(Path::new("bindings").join(name)))
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
            changes.record(event(kind, &file));
        }
        assert_eq!(changes, BindingChanges::default());
        changes.record(event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            &name,
        ));
        assert_eq!(changes.panes, HashSet::from([pane]));
        assert!(!changes.rescan && !changes.failed);
    }

    #[test]
    fn lost_events_and_errors_are_reported_separately() {
        let mut changes = BindingChanges::default();
        changes.record(Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)));
        assert!(changes.rescan && !changes.failed && changes.panes.is_empty());
        changes.record(Err(notify::Error::generic("queue closed")));
        assert!(changes.failed);
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
