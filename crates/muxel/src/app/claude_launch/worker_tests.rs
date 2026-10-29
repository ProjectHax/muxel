use super::{LaunchKey, Probe, Request, Resolution, request_record, resolve_batch};
use crate::session_binding;
use muxel_core::{AgentPreset, TerminalLaunchReason};
use std::collections::HashSet;
use std::path::PathBuf;
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    data: PathBuf,
    cwd: PathBuf,
    // Shares the Windows app fixtures' lock, including the global lookup hook.
    _storage: muxel_store::test_support::TestRoot,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("muxel-pr108-worker-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let storage = muxel_store::test_support::set_root(root.clone());
        let home = root.join("home");
        let data = root.join("data");
        let cwd = root.join("project");
        for path in [&home, &data, &cwd] {
            std::fs::create_dir_all(path).unwrap();
        }
        Self {
            root,
            home,
            data,
            cwd,
            _storage: storage,
        }
    }

    fn request(&self) -> Request {
        let preset = AgentPreset::claude();
        Request {
            key: LaunchKey {
                workspace: Some(Uuid::new_v4()),
                pane: Uuid::new_v4(),
                project: Uuid::new_v4(),
                token: 1,
                reason: TerminalLaunchReason::Restore,
                generation: 0,
                id: None,
                path: None,
                started: false,
                consumed_hook: None,
                record: Ok(None),
                cwd: self.cwd.clone(),
                program: preset.program.clone(),
                preset,
                args: Vec::new(),
            },
            home: Some(self.home.clone()),
            data: Some(self.data.clone()),
            siblings: HashSet::new(),
        }
    }

    fn hook(&self, pane: Uuid, id: &str, source: &str, transcript_exists: bool) -> PathBuf {
        let path = muxel_core::claude_session_path(&self.home, &self.cwd, id);
        if transcript_exists {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::json!({"cwd": self.cwd}).to_string()).unwrap();
        }
        let event = serde_json::json!({
            "hook_event_name": "SessionStart",
            "source": source,
            "session_id": id,
            "transcript_path": path,
            "cwd": self.cwd,
        });
        session_binding::write_claude_binding_from_reader(
            &self.data,
            pane,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn resolve(request: Request) -> Resolution {
    let (sender, receiver) = async_channel::bounded(1);
    resolve_batch(vec![(request, sender)]);
    let resolution = receiver.recv_blocking().unwrap();
    assert!(
        receiver.recv_blocking().is_err(),
        "worker sent more than one result"
    );
    resolution
}

#[test]
fn pr108_worker_consumed_receipt_is_ignored_but_new_stamped_same_uuid_is_adopted() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    let announced = Uuid::new_v4().to_string();
    let path = fixture.hook(request.key.pane, &announced, "resume", true);
    let consumed = request_record(&request).unwrap().unwrap();
    request.key.consumed_hook = Some(consumed.clone());
    let ignored = resolve(request.clone());
    assert!(!ignored.hook);
    assert_eq!(ignored.id, None);
    assert!(matches!(ignored.probe, Probe::Unknown));

    fixture.hook(request.key.pane, &announced, "resume", true);
    let current = request_record(&request).unwrap().unwrap();
    assert_ne!(current, consumed);
    let adopted = resolve(request);
    assert!(adopted.hook);
    assert_eq!(adopted.id.as_deref(), Some(announced.as_str()));
    assert_eq!(adopted.record, Ok(Some(current)));
    let Probe::Found(session) = adopted.probe else {
        panic!("new receipt was not resolved");
    };
    assert_eq!(session.path, path);
    assert_eq!(session.cwd, Some(fixture.cwd.clone()));
}

#[test]
fn pr108_worker_unconsumed_clear_keeps_first_write_path_before_transcript_exists() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let announced = Uuid::new_v4().to_string();
    let path = fixture.hook(request.key.pane, &announced, "clear", false);
    // As in the app fixture, the provider's project directory exists before
    // its first transcript write. Canonical validation still needs that parent.
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    assert!(!path.exists());
    let found = resolve(request);
    assert!(found.hook);
    assert_eq!(found.id.as_deref(), Some(announced.as_str()));
    let Probe::Found(session) = found.probe else {
        panic!("clear lost its first-write allowance");
    };
    assert_eq!(session.path, path);
    assert_eq!(session.cwd, Some(fixture.cwd.clone()));
    assert!(!path.exists(), "lookup must not create the transcript");
}

#[test]
fn pr108_worker_same_mtime_replacement_changes_the_landed_receipt_identity() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let first = Uuid::new_v4().to_string();
    fixture.hook(request.key.pane, &first, "resume", true);
    let record = session_binding::claude_binding_path(&fixture.data, request.key.pane);
    let modified = std::fs::metadata(&record).unwrap().modified().unwrap();
    let landed = resolve(request.clone());
    assert_eq!(landed.id.as_deref(), Some(first.as_str()));
    let second = Uuid::new_v4().to_string();
    fixture.hook(request.key.pane, &second, "resume", true);
    std::fs::File::options()
        .write(true)
        .open(&record)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert_eq!(
        std::fs::metadata(&record).unwrap().modified().unwrap(),
        modified
    );
    let current = request_record(&request);
    assert_ne!(
        landed.record, current,
        "mtime equality hid a new publication"
    );
    let refreshed = resolve(request);
    assert_eq!(refreshed.record, current);
    assert_eq!(refreshed.id.as_deref(), Some(second.as_str()));
    assert!(matches!(refreshed.probe, Probe::Found(_)));
}

#[test]
fn pr108_worker_missing_store_does_not_refuse_an_unrelated_fresh_batch_job() {
    let fixture = Fixture::new();
    let old = fixture.request();
    fixture.hook(old.key.pane, &Uuid::new_v4().to_string(), "clear", false);
    let fresh = fixture.request();
    assert!(!fixture.home.join(".claude/projects").exists());
    let (old_sender, old_receiver) = async_channel::bounded(1);
    let (fresh_sender, fresh_receiver) = async_channel::bounded(1);
    resolve_batch(vec![(old, old_sender), (fresh, fresh_sender)]);
    let old = old_receiver.recv_blocking().unwrap();
    // A missing store leaves this hook unadoptable; it must not turn the
    // unrelated fresh job's answer into a batch-wide refusal.
    assert!(!old.hook);
    assert_eq!(old.id, None);
    assert!(matches!(old.probe, Probe::Unknown));
    let fresh = fresh_receiver.recv_blocking().unwrap();
    assert!(!fresh.hook);
    assert_eq!(fresh.id, None);
    assert!(matches!(fresh.probe, Probe::Unknown));
}
