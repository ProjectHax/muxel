use super::{CodexResumePath, CodexSessionNames, codex_resume_path, codex_session_id_from_hint};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, PartialEq)]
pub(super) enum Operation {
    Directory(PathBuf),
    Open(PathBuf),
    Stat(PathBuf),
    Read { offset: u64, bytes: usize },
}

thread_local! {
    static OPERATIONS: RefCell<Vec<Operation>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn record_directory(path: &Path) {
    OPERATIONS
        .with_borrow_mut(|operations| operations.push(Operation::Directory(path.to_path_buf())));
}

pub(super) fn record_open(path: &Path) {
    OPERATIONS.with_borrow_mut(|operations| operations.push(Operation::Open(path.to_path_buf())));
}

pub(super) fn record_stat(path: &Path) {
    OPERATIONS.with_borrow_mut(|operations| operations.push(Operation::Stat(path.to_path_buf())));
}

pub(super) fn record_read(offset: u64, bytes: usize) {
    OPERATIONS.with_borrow_mut(|operations| operations.push(Operation::Read { offset, bytes }));
}

#[test]
fn b9_codex_hint_resolution_never_enumerates_other_days() {
    use chrono::TimeZone;
    let root = std::env::temp_dir().join(format!("muxel-codex-bounded-{}", Uuid::new_v4()));
    let cwd = root.join("project");
    // Center the timestamp in the overlap of the local and UTC calendar days.
    // A fixed UTC noon is already the next day on UTC+13/UTC+14 hosts.
    let noon = chrono::Utc
        .with_ymd_and_hms(2026, 10, 19, 12, 0, 0)
        .unwrap();
    let offset = noon
        .with_timezone(&chrono::Local)
        .offset()
        .local_minus_utc();
    let created = noon - chrono::Duration::seconds(i64::from(offset) / 2);
    let local = created.with_timezone(&chrono::Local);
    assert_eq!(local.date_naive(), created.date_naive());
    let day = root
        .join(".codex/sessions")
        .join(local.format("%Y/%m/%d").to_string());
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&day).unwrap();
    for unrelated in ["2025/01/01", "2026/10/18", "2026/09/30"] {
        std::fs::create_dir_all(root.join(".codex/sessions").join(unrelated)).unwrap();
    }
    let id =
        Uuid::from_u128(((created.timestamp_millis() as u128) << 80) | (7 << 76) | (2 << 62) | 1)
            .to_string();
    let path = day.join(format!("rollout-2026-10-19T05-56-59-{id}.jsonl"));
    std::fs::write(
        &path,
        serde_json::json!({
            "type": "session_meta", "payload": {"id": id, "cwd": cwd}
        })
        .to_string(),
    )
    .unwrap();
    OPERATIONS.with_borrow_mut(Vec::clear);
    assert_eq!(
        codex_session_id_from_hint(&root, &format!("{}...", &id[..29]), &cwd).as_deref(),
        Some(id.as_str())
    );
    let operations = OPERATIONS.with_borrow_mut(std::mem::take);
    let directories: Vec<_> = operations
        .iter()
        .filter_map(|operation| match operation {
            Operation::Directory(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        directories,
        vec![day],
        "lookup enumerated unrelated directories: {operations:?}"
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| matches!(operation, Operation::Open(_)))
            .count(),
        1
    );
}

#[test]
fn b9_stored_codex_path_is_one_stat_without_enumeration_or_content_reads() {
    let root = std::env::temp_dir().join(format!("muxel-codex-stat-{}", Uuid::new_v4()));
    let id = "01a1543c-9700-7000-8000-000000000001";
    let path = root
        .join(".codex/sessions/2026/10/19")
        .join(format!("rollout-{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not rollout JSON; existence must not read contents").unwrap();
    OPERATIONS.with_borrow_mut(Vec::clear);
    assert_eq!(
        codex_resume_path(&root, id, Some(&path)).unwrap(),
        CodexResumePath::Found(path.clone())
    );
    assert_eq!(
        OPERATIONS.with_borrow_mut(std::mem::take),
        vec![Operation::Stat(path.clone())]
    );
    assert_eq!(
        codex_resume_path(&root, "01a1543c-9700-7000-8000-000000000002", Some(&path)).unwrap(),
        CodexResumePath::Unknown
    );
    assert!(OPERATIONS.with_borrow(Vec::is_empty));
}

#[test]
fn b9_archived_codex_path_is_not_resumable_and_is_never_scanned() {
    let root = std::env::temp_dir().join(format!("muxel-codex-archive-{}", Uuid::new_v4()));
    let id = "01a1543c-9700-7000-8000-000000000001";
    let path = root
        .join(".codex/archived_sessions/2026-10")
        .join(format!("rollout-{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{}").unwrap();
    OPERATIONS.with_borrow_mut(Vec::clear);
    assert_eq!(
        codex_resume_path(&root, id, Some(&path)).unwrap(),
        CodexResumePath::Unknown
    );
    assert!(OPERATIONS.with_borrow(Vec::is_empty));
    assert_eq!(
        codex_resume_path(&root, id, None).unwrap(),
        CodexResumePath::Unknown
    );
    assert!(OPERATIONS.with_borrow(|operations| operations.iter().all(|operation| {
        matches!(operation, Operation::Directory(directory) if directory.starts_with(root.join(".codex/sessions")))
    })));
}

#[test]
fn b9_compression_moves_only_probe_the_exact_saved_path_and_its_sibling() {
    let root = std::env::temp_dir().join(format!("muxel-codex-compress-{}", Uuid::new_v4()));
    let id = "01a1543c-9700-7000-8000-000000000001";
    let path = root
        .join(".codex/sessions/2026/10/19")
        .join(format!("rollout-{id}.jsonl"));
    let compressed = path.with_extension("jsonl.zst");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&compressed, "opaque compressed contents").unwrap();
    OPERATIONS.with_borrow_mut(Vec::clear);
    assert_eq!(
        codex_resume_path(&root, id, Some(&path)).unwrap(),
        CodexResumePath::Found(compressed.clone())
    );
    assert_eq!(
        OPERATIONS.with_borrow_mut(std::mem::take),
        vec![Operation::Stat(path), Operation::Stat(compressed)]
    );
    let id = "01a1543c-9700-7000-8000-000000000002";
    let plain = root
        .join(".codex/sessions/2026/10/19")
        .join(format!("rollout-{id}.jsonl"));
    let compressed = plain.with_extension("jsonl.zst");
    std::fs::write(&plain, "{}").unwrap();
    assert_eq!(
        codex_resume_path(&root, id, Some(&compressed)).unwrap(),
        CodexResumePath::Found(plain.clone())
    );
    assert_eq!(
        OPERATIONS.with_borrow_mut(std::mem::take),
        vec![Operation::Stat(compressed), Operation::Stat(plain)]
    );
}

#[test]
fn non_file_saved_codex_path_still_checks_its_compression_sibling() {
    let root = std::env::temp_dir().join(format!("muxel-codex-nonfile-{}", Uuid::new_v4()));
    let id = "01a1543c-9700-7000-8000-000000000001";
    let path = root
        .join(".codex/sessions/2026/10/19")
        .join(format!("rollout-{id}.jsonl"));
    let sibling = path.with_extension("jsonl.zst");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(&sibling, "compressed rollout").unwrap();
    OPERATIONS.with_borrow_mut(Vec::clear);
    assert_eq!(
        codex_resume_path(&root, id, Some(&path)).unwrap(),
        CodexResumePath::Found(sibling.clone())
    );
    assert_eq!(
        OPERATIONS.with_borrow_mut(std::mem::take),
        vec![
            Operation::Stat(path.clone()),
            Operation::Stat(sibling.clone())
        ]
    );
    std::fs::remove_file(&sibling).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    assert_eq!(
        codex_resume_path(&root, id, Some(&path)).unwrap(),
        CodexResumePath::Missing
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn b9_codex_name_index_rereads_changed_files_and_resets_after_truncation() {
    use std::io::Write;
    let root = std::env::temp_dir().join(format!("muxel-codex-incremental-{}", Uuid::new_v4()));
    let path = root.join(".codex/session_index.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let first = b"{\"id\":\"a\",\"thread_name\":\"First\"}\n";
    std::fs::write(&path, first).unwrap();
    let mut index = CodexSessionNames::default();
    assert!(index.refresh(&root).unwrap());
    OPERATIONS.with_borrow_mut(Vec::clear);
    assert!(!index.refresh(&root).unwrap());
    assert_eq!(
        OPERATIONS.with_borrow_mut(std::mem::take),
        vec![Operation::Stat(path.clone())]
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    let appended = b"{\"id\":\"a\",\"thread_name\":\"Renamed\"}\n";
    file.write_all(appended).unwrap();
    file.flush().unwrap();
    assert!(index.refresh(&root).unwrap());
    assert_eq!(index.names().get("a").map(String::as_str), Some("Renamed"));
    assert!(
        OPERATIONS.with_borrow(|operations| operations.contains(&Operation::Read {
            offset: 0,
            bytes: first.len() + appended.len()
        }))
    );
    file.write_all(b"{\"id\":\"b\",\"thread_name\":\"Par")
        .unwrap();
    file.flush().unwrap();
    assert!(!index.refresh(&root).unwrap());
    assert!(!index.names().contains_key("b"));
    file.write_all(b"tial\"}\n").unwrap();
    file.flush().unwrap();
    assert!(index.refresh(&root).unwrap());
    assert_eq!(index.names().get("b").map(String::as_str), Some("Partial"));
    drop(file);
    std::fs::write(&path, b"{\"id\":\"c\",\"thread_name\":\"New\"}\n").unwrap();
    assert!(index.refresh(&root).unwrap());
    assert_eq!(index.names().len(), 1);
    assert_eq!(index.names().get("c").map(String::as_str), Some("New"));
    std::fs::write(&path, b"{\"id\":\"c\",\"thread_name\":\"Old\"}\n").unwrap();
    let modified = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert!(index.refresh(&root).unwrap());
    assert_eq!(index.names().get("c").map(String::as_str), Some("Old"));
}

#[test]
fn b9_midnight_hint_checks_only_its_day_and_one_neighbor() {
    use chrono::TimeZone;
    let root = std::env::temp_dir().join(format!("muxel-codex-midnight-{}", Uuid::new_v4()));
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let local = chrono::Local
        .with_ymd_and_hms(2026, 10, 19, 0, 2, 0)
        .single()
        .unwrap();
    let id =
        Uuid::from_u128(((local.timestamp_millis() as u128) << 80) | (7 << 76) | (2 << 62) | 1)
            .to_string();
    let day = root.join(".codex/sessions/2026/10/19");
    let neighbor = root.join(".codex/sessions/2026/10/18");
    std::fs::create_dir_all(&neighbor).unwrap();
    let path = neighbor.join(format!("rollout-{id}.jsonl"));
    std::fs::write(
        &path,
        serde_json::json!({"type":"session_meta", "payload":{"id":id,"cwd":cwd}}).to_string(),
    )
    .unwrap();
    OPERATIONS.with_borrow_mut(Vec::clear);
    let found = super::codex_session_from_hint(&root, &format!("{}...", &id[..29]), &cwd).unwrap();
    assert_eq!(found.id, id);
    assert_eq!(found.path, path.clone());
    assert_eq!(
        OPERATIONS.with_borrow_mut(std::mem::take),
        vec![
            Operation::Directory(day),
            Operation::Directory(neighbor),
            Operation::Open(path)
        ]
    );
}

fn w8lm_rollout(root: &Path, id: &str, payload: serde_json::Value) -> PathBuf {
    let day = super::codex_sessions::directories(root, id).remove(0);
    std::fs::create_dir_all(&day).unwrap();
    let path = day.join(format!("rollout-{id}.jsonl"));
    std::fs::write(
        &path,
        serde_json::json!({"type":"session_meta","payload":payload}).to_string(),
    )
    .unwrap();
    path
}

fn r2_key(root: &Path) -> crate::session_discovery::SessionDiscoveryKey {
    crate::session_discovery::SessionDiscoveryKey {
        pane: Uuid::new_v4(),
        provider: crate::session_discovery::SessionProvider::Codex,
        launch: std::time::Instant::now(),
        child_pid: Some(1),
        binding_generation: 0,
        binding: None,
        binding_path: None,
        cwd: root.join("project"),
        hint: Some("01a1543c-9700-7000-8000-000000000001".into()),
        tmux_session: None,
        tmux_socket: None,
    }
}

fn r2_churn(day: &Path, tick: u64) {
    let before = std::fs::metadata(day).unwrap().modified().unwrap();
    std::fs::write(day.join(format!("unrelated-{tick}")), "").unwrap();
    // Windows may defer directory timestamps. Set this owned fixture's stamp
    // explicitly so the operation-count test always exercises invalidation.
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.access_mode(0x100).custom_flags(0x0200_0000);
    }
    options
        .open(day)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(
            std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_800_000_000 + tick),
        ))
        .unwrap();
    let after = std::fs::metadata(day).unwrap().modified().unwrap();
    assert_ne!(before, after, "fixture must change the day directory stamp");
}

#[test]
fn b9_r2_codex_sibling_rejection_stops_io_until_ownership_changes() {
    use crate::session_discovery::{CodexLookupEvidence, SessionDiscoveries};
    use crate::{AgentPreset, Instance, Project, Workspace};
    use std::time::Duration;
    for change in ["generation", "released", "removed"] {
        let root = std::env::temp_dir().join(format!("muxel-r2-owner-{}", Uuid::new_v4()));
        let key = r2_key(&root);
        let id = key.hint.as_deref().unwrap();
        let path = w8lm_rollout(&root, id, serde_json::json!({"id":id,"cwd":key.cwd}));
        let day = path.parent().unwrap();
        let project = Project::new("project", &key.cwd);
        let mut target = Instance::from_preset(project.id, &AgentPreset::codex());
        target.id = key.pane;
        target.session_id = None;
        let mut owner = Instance::from_preset(project.id, &AgentPreset::codex());
        owner.session_id = Some(id.into());
        let owner_id = owner.id;
        let mut workspace = Workspace {
            projects: vec![project],
            instances: vec![target, owner],
            ..Workspace::default()
        };
        let mut discovery = SessionDiscoveries::default();
        let now = key.launch;
        OPERATIONS.with_borrow_mut(Vec::clear);
        let mut attempts = 0;
        for tick in 0..=60 {
            r2_churn(day, tick);
            discovery.reconcile(&workspace);
            let at = now + Duration::from_secs(tick);
            if let Some(request) = discovery.begin(key.clone(), at) {
                attempts += 1;
                let candidate = super::codex_session_from_hint(&root, id, &key.cwd).unwrap();
                let owner = workspace
                    .instances
                    .iter()
                    .find(|row| {
                        row.id != key.pane && row.session_id.as_deref() == Some(&candidate.id)
                    })
                    .unwrap();
                assert!(discovery.finish_with_codex_evidence(
                    &request,
                    Some(&key),
                    at,
                    false,
                    CodexLookupEvidence {
                        miss: None,
                        rejected_owner: Some((owner.id, candidate.id)),
                        observed_hint: key.hint.clone(),
                        cached_rejection: false,
                    }
                ));
            }
        }
        assert_eq!(attempts, 1, "unchanged sibling rejection retried: {change}");
        assert_eq!(
            OPERATIONS.with_borrow(|ops| ops
                .iter()
                .filter(|op| matches!(op, Operation::Open(_)))
                .count()),
            1
        );
        match change {
            "generation" => discovery.binding_changed(owner_id),
            "released" => workspace.instance_mut(owner_id).unwrap().session_id = None,
            "removed" => workspace.instances.retain(|row| row.id != owner_id),
            _ => unreachable!(),
        }
        discovery.reconcile(&workspace);
        let at = now + Duration::from_secs(61);
        let request = discovery
            .begin(key.clone(), at)
            .expect("ownership change did not revalidate");
        let candidate = super::codex_session_from_hint(&root, id, &key.cwd).unwrap();
        let blocker = workspace
            .instances
            .iter()
            .find(|row| row.id != key.pane && row.session_id.as_deref() == Some(&candidate.id))
            .map(|row| (row.id, candidate.id.clone()));
        assert!(discovery.finish_with_codex_evidence(
            &request,
            Some(&key),
            at,
            blocker.is_none(),
            CodexLookupEvidence {
                miss: None,
                rejected_owner: blocker,
                observed_hint: key.hint.clone(),
                cached_rejection: false,
            }
        ));
        if change == "generation" {
            assert!(
                discovery
                    .begin(key.clone(), at + Duration::from_secs(1))
                    .is_none()
            );
            workspace.instance_mut(owner_id).unwrap().session_id = None;
            discovery.reconcile(&workspace);
            let at = at + Duration::from_secs(1);
            let request = discovery.begin(key.clone(), at).unwrap();
            let candidate = super::codex_session_from_hint(&root, id, &key.cwd).unwrap();
            assert_eq!(candidate.path, path);
            assert!(discovery.finish_with_codex_evidence(
                &request,
                Some(&key),
                at,
                true,
                CodexLookupEvidence::default()
            ));
        }
        assert_eq!(
            OPERATIONS.with_borrow(|ops| ops
                .iter()
                .filter(|op| matches!(op, Operation::Open(_)))
                .count()),
            if change == "generation" { 3 } else { 2 }
        );
    }
}

#[test]
fn b9_r2_codex_churning_day_respects_backoff_and_captures_later_rollout() {
    use super::CodexHintMiss;
    use crate::session_discovery::SessionDiscoveries;
    use std::time::Duration;
    for (appears, expected) in [
        (20, vec![0, 1, 3, 7, 15, 31]),
        (100, vec![0, 1, 3, 7, 15, 31, 61, 91, 121]),
    ] {
        let root = std::env::temp_dir().join(format!("muxel-r2-churn-{}", Uuid::new_v4()));
        let key = r2_key(&root);
        let id = key.hint.as_deref().unwrap();
        let directories = super::codex_sessions::directories(&root, id);
        let day = &directories[0];
        std::fs::create_dir_all(day).unwrap();
        let now = key.launch;
        let mut discovery = SessionDiscoveries::default();
        let mut attempts = Vec::new();
        let mut captured = false;
        OPERATIONS.with_borrow_mut(Vec::clear);
        for tick in 0..=121 {
            r2_churn(day, tick);
            if tick == appears {
                w8lm_rollout(&root, id, serde_json::json!({"id":id,"cwd":key.cwd}));
            }
            let at = now + Duration::from_secs(tick);
            let Some(request) = discovery.begin(key.clone(), at) else {
                continue;
            };
            attempts.push(tick);
            let stamp = CodexHintMiss::capture(&root, id, at);
            let candidate = if request
                .codex_miss
                .as_ref()
                .is_some_and(|miss| miss.reusable_for(&stamp))
            {
                None
            } else {
                super::codex_session_from_hint(&root, id, &key.cwd)
            };
            let found = candidate.is_some();
            assert!(discovery.finish_with_codex_miss(
                &request,
                Some(&key),
                at,
                found,
                (!found).then_some(stamp)
            ));
            if let Some(candidate) = candidate {
                assert_eq!(candidate.id, id);
                captured = true;
                break;
            }
        }
        assert!(captured, "available rollout was never captured");
        assert_eq!(
            attempts, expected,
            "directory churn bypassed exponential backoff"
        );
        let operations = OPERATIONS.with_borrow_mut(std::mem::take);
        assert_eq!(
            operations
                .iter()
                .filter(|op| matches!(op, Operation::Directory(_)))
                .count(),
            directories.len() * expected.len()
        );
        assert_eq!(
            operations
                .iter()
                .filter(|op| matches!(op, Operation::Open(_)))
                .count(),
            1
        );
    }
}

#[test]
fn b9_r2_codex_index_middle_rewrite_with_unchanged_samples_is_visible() {
    let root = std::env::temp_dir().join(format!("muxel-r2-index-middle-{}", Uuid::new_v4()));
    let path = root.join(".codex/session_index.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let old = (0..40)
        .map(|i| format!("{{\"id\":\"{i}\",\"thread_name\":\"Name-{i:02}\"}}\n"))
        .collect::<String>();
    std::fs::write(&path, &old).unwrap();
    let mut index = CodexSessionNames::default();
    index.refresh(&root).unwrap();
    let rewritten =
        old.replace("Name-20", "Edit-20") + "{\"id\":\"new\",\"thread_name\":\"Appended\"}\n";
    assert_eq!(&old.as_bytes()[..256], &rewritten.as_bytes()[..256]);
    assert_eq!(
        &old.as_bytes()[old.len() - 256..],
        &rewritten.as_bytes()[old.len() - 256..old.len()]
    );
    std::fs::write(&path, &rewritten).unwrap();
    OPERATIONS.with_borrow_mut(Vec::clear);
    assert!(index.refresh(&root).unwrap());
    assert_eq!(index.names().get("20").map(String::as_str), Some("Edit-20"));
    assert_eq!(
        index.names().get("new").map(String::as_str),
        Some("Appended")
    );
    assert!(OPERATIONS.with_borrow(|ops| ops.contains(&Operation::Read {
        offset: 0,
        bytes: rewritten.len()
    })));
}

#[test]
fn w8lm_codex_metadata_uses_thread_id_and_rejects_subagents() {
    let id = "01a1543c-9700-7000-8000-000000000001";
    let parent = "01a1543c-9700-7000-8000-000000000002";
    for (label, payload, accepted) in [
        (
            "top-level",
            serde_json::json!({"id":id,"session_id":parent,"source":"cli"}),
            true,
        ),
        (
            "subagent",
            serde_json::json!({"id":id,"session_id":parent,"source":{"subagent":{"thread_spawn":{"parent_thread_id":parent}}}}),
            false,
        ),
        (
            "parent-field",
            serde_json::json!({"id":id,"parent_thread_id":parent,"source":"exec"}),
            false,
        ),
        ("legacy", serde_json::json!({"session_id":id}), true),
    ] {
        let root = std::env::temp_dir().join(format!("muxel-w8lm-{label}-{}", Uuid::new_v4()));
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut payload = payload;
        payload["cwd"] = serde_json::json!(cwd);
        w8lm_rollout(&root, id, payload);
        assert_eq!(
            codex_session_id_from_hint(&root, id, &cwd),
            accepted.then(|| id.to_string()),
            "{label}"
        );
    }
}

#[test]
fn w8lm_codex_capture_retries_after_a_missing_rollout_appears() {
    use crate::session_discovery::{SessionDiscovery, SessionDiscoveryKey, SessionProvider};
    use std::time::{Duration, Instant};
    let root = std::env::temp_dir().join(format!("muxel-w8lm-appears-{}", Uuid::new_v4()));
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let id = "01a1543c-9700-7000-8000-000000000001";
    let now = Instant::now();
    let key = SessionDiscoveryKey {
        pane: Uuid::new_v4(),
        provider: SessionProvider::Codex,
        launch: now,
        child_pid: Some(1),
        binding_generation: 0,
        binding: None,
        binding_path: None,
        cwd: cwd.clone(),
        hint: Some(id.into()),
        tmux_session: None,
        tmux_socket: None,
    };
    let mut discovery = SessionDiscovery::default();
    let first = discovery.begin(key.clone(), now).unwrap();
    assert!(codex_session_id_from_hint(&root, id, &cwd).is_none());
    assert!(discovery.finish(&first, Some(&key), now, false));
    w8lm_rollout(
        &root,
        id,
        serde_json::json!({"id":id,"cwd":cwd,"source":"cli"}),
    );
    let second = discovery
        .begin(key.clone(), now + Duration::from_secs(2))
        .expect("newly available rollout must be retried");
    assert_eq!(
        codex_session_id_from_hint(&root, id, &cwd).as_deref(),
        Some(id)
    );
    assert!(discovery.finish(&second, Some(&key), now + Duration::from_secs(2), true));
}

#[test]
fn w8lm_codex_index_replacement_and_truncate_regrow_discard_old_names() {
    for replace in [false, true] {
        let root = std::env::temp_dir().join(format!("muxel-w8lm-index-{}", Uuid::new_v4()));
        let path = root.join(".codex/session_index.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"id\":\"old\",\"thread_name\":\"Old\"}\n").unwrap();
        let mut index = CodexSessionNames::default();
        assert!(index.refresh(&root).unwrap());
        if replace {
            std::fs::rename(&path, path.with_extension("old")).unwrap();
        }
        std::fs::write(&path,b"{\"id\":\"first\",\"thread_name\":\"Replacement first row\"}\n{\"id\":\"last\",\"thread_name\":\"Replacement last row\"}\n").unwrap();
        assert!(index.refresh(&root).unwrap());
        assert!(!index.names().contains_key("old"), "replace={replace}");
        assert_eq!(
            index.names().get("first").map(String::as_str),
            Some("Replacement first row")
        );
        assert_eq!(index.names().len(), 2);
        assert!(!index.refresh(&root).unwrap());
    }
}

#[test]
fn w8lm_codex_index_regrow_with_the_same_beginning_rechecks_old_tail() {
    let root = std::env::temp_dir().join(format!("muxel-w8lm-index-tail-{}", Uuid::new_v4()));
    let path = root.join(".codex/session_index.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let prefix = (0..20)
        .map(|i| format!("{{\"id\":\"{i}\",\"thread_name\":\"Unchanged prefix {i}\"}}\n"))
        .collect::<String>();
    std::fs::write(
        &path,
        format!("{prefix}{{\"id\":\"old\",\"thread_name\":\"Old\"}}\n"),
    )
    .unwrap();
    let mut index = CodexSessionNames::default();
    index.refresh(&root).unwrap();
    std::fs::write(
        &path,
        format!("{prefix}{{\"id\":\"new\",\"thread_name\":\"New longer replacement suffix\"}}\n"),
    )
    .unwrap();
    assert!(index.refresh(&root).unwrap());
    assert!(!index.names().contains_key("old"));
    assert_eq!(
        index.names().get("new").map(String::as_str),
        Some("New longer replacement suffix")
    );
    assert_eq!(index.names().len(), 21);
}

#[test]
fn w8lm_codex_miss_checks_only_day_stamps_and_expires_after_thirty_seconds() {
    use super::CodexHintMiss;
    use std::time::{Duration, Instant};
    let root = std::env::temp_dir().join(format!("muxel-w8lm-cache-{}", Uuid::new_v4()));
    let id = "01a1543c-9700-7000-8000-000000000001";
    let now = Instant::now();
    let miss = CodexHintMiss::capture(&root, id, now);
    OPERATIONS.with_borrow_mut(Vec::clear);
    let unchanged = CodexHintMiss::capture(&root, id, now + Duration::from_secs(29));
    assert!(miss.reusable_for(&unchanged));
    assert!(!miss.reusable_for(&CodexHintMiss::capture(
        &root,
        "01a1543c-9700-7000-8000-000000000002",
        now + Duration::from_secs(1)
    )));
    assert!(OPERATIONS.with_borrow(|ops| ops.iter().all(|op| matches!(op, Operation::Stat(_)))));
    assert!(!miss.reusable_for(&CodexHintMiss::capture(
        &root,
        id,
        now + Duration::from_secs(30)
    )));
    w8lm_rollout(
        &root,
        id,
        serde_json::json!({"id":id,"cwd":root,"source":"cli"}),
    );
    assert!(!miss.reusable_for(&CodexHintMiss::capture(
        &root,
        id,
        now + Duration::from_secs(1)
    )));
}

struct TempHome(PathBuf);

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_rollout_at(path: &Path, payload: serde_json::Value, age_secs: u64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        serde_json::json!({"type":"session_meta","payload":payload}).to_string(),
    )
    .unwrap();
    let modified = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

#[test]
fn latest_sessions_unreadable_rollout_does_not_select_an_older_conversation() {
    use super::codex_latest_sessions;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};
    let root = std::env::temp_dir().join(format!("muxel-codex-unreadable-{}", Uuid::new_v4()));
    let _home = TempHome(root.clone());
    let older = "01a1543c-9700-7000-8000-000000000001";
    let newer = "01a1543c-9700-7000-8000-000000000002";
    let day = super::codex_sessions::directories(&root, older)[0].clone();
    write_rollout_at(
        &day.join(format!("rollout-{older}.jsonl")),
        serde_json::json!({"id":older,"cwd":root}),
        60,
    );
    let unresolved = day.join(format!("rollout-{newer}.jsonl"));
    for bytes in [
        b"{incomplete".as_slice(),
        b"",
        b"\xff\n",
        b"{\"type\":\"session_meta\",\"payload\":{}}",
        b"{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
    ] {
        std::fs::write(&unresolved, bytes).unwrap();
        assert_eq!(
            codex_latest_sessions(
                &root,
                &root,
                &HashSet::new(),
                1,
                Instant::now() + Duration::from_secs(60)
            ),
            None,
            "an unresolved newer rollout could be this directory's latest conversation"
        );
    }
    let owned = [newer.to_string()].into_iter().collect();
    let found = codex_latest_sessions(
        &root,
        &root,
        &owned,
        1,
        Instant::now() + Duration::from_secs(60),
    )
    .unwrap();
    assert_eq!(
        found[0].id, older,
        "an owned unresolved thread cannot change the match"
    );
}

#[test]
fn hint_unreadable_rollout_keeps_a_truncated_hint_ambiguous() {
    let root = std::env::temp_dir().join(format!("muxel-codex-unreadable-hint-{}", Uuid::new_v4()));
    let _home = TempHome(root.clone());
    let first = "01a1543c-9700-7000-8000-000000000001";
    let second = "01a1543c-9700-7000-8000-000000000002";
    let day = super::codex_sessions::directories(&root, first)[0].clone();
    write_rollout_at(
        &day.join(format!("rollout-{first}.jsonl")),
        serde_json::json!({"id":first,"cwd":root}),
        60,
    );
    std::fs::write(day.join(format!("rollout-{second}.jsonl")), b"{incomplete").unwrap();
    assert_eq!(
        codex_session_id_from_hint(&root, &format!("{}...", &first[..29]), &root),
        None,
        "a matching filename with unreadable metadata may be a second matching thread"
    );
    assert_eq!(
        codex_session_id_from_hint(&root, first, &root).as_deref(),
        Some(first)
    );
    write_rollout_at(
        &day.join(format!("rollout-{second}.jsonl")),
        serde_json::json!({"id":second,"source":"subagent"}),
        0,
    );
    // A confirmed subagent is excluded even without a cwd; an unreadable
    // compression sibling cannot introduce a different thread identity.
    std::fs::write(
        day.join(format!("rollout-{first}.jsonl.zst")),
        b"compressed",
    )
    .unwrap();
    assert_eq!(
        codex_session_id_from_hint(&root, &format!("{}...", &first[..29]), &root).as_deref(),
        Some(first)
    );
}

#[test]
fn import_unreadable_rollout_does_not_select_an_older_conversation() {
    let root =
        std::env::temp_dir().join(format!("muxel-codex-unreadable-import-{}", Uuid::new_v4()));
    let _home = TempHome(root.clone());
    let day = root.join(".codex/sessions/2026/10/19");
    write_rollout_at(
        &day.join("rollout-older.jsonl"),
        serde_json::json!({"id":"older","cwd":root}),
        60,
    );
    std::fs::write(day.join("rollout-newer.jsonl"), b"{incomplete").unwrap();
    assert_eq!(
        super::codex_latest_session_id(&root, &root),
        None,
        "Import must not mistake an unreadable newer rollout for another directory"
    );
    std::fs::File::options()
        .write(true)
        .open(day.join("rollout-newer.jsonl"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120))
        .unwrap();
    assert_eq!(
        super::codex_latest_session_id(&root, &root).as_deref(),
        Some("older"),
        "an unresolved older rollout cannot replace the known newer conversation"
    );
}

#[test]
fn unreadable_rollout_removed_after_enumeration_keeps_latest_match_unresolved() {
    let root = std::env::temp_dir().join(format!("muxel-codex-removed-{}", Uuid::new_v4()));
    let _home = TempHome(root.clone());
    let id = Uuid::new_v4();
    assert_eq!(
        super::codex_sessions::newest_matches(
            std::iter::once((
                std::time::SystemTime::now(),
                Some(id),
                root.join(format!("rollout-{id}.jsonl"))
            )),
            &root,
            1,
            &mut || false,
        ),
        None
    );
}

fn review_rollout_candidates(tied: bool) -> (TempHome, Vec<(Uuid, PathBuf)>) {
    use std::time::{Duration, SystemTime};
    let root = std::env::temp_dir().join(format!("muxel-codex-review-{}", Uuid::new_v4()));
    let valid = Uuid::parse_str("01a1543c-9700-7000-8000-000000000001").unwrap();
    let unknown = Uuid::parse_str("01a1543c-9700-7000-8000-000000000002").unwrap();
    let day = root.join(".codex/sessions/2026/10/19");
    let valid_path = day.join(format!("rollout-{valid}.jsonl"));
    let unknown_path = day.join(format!("rollout-{unknown}.jsonl"));
    write_rollout_at(&valid_path, serde_json::json!({"id":valid,"cwd":root}), 0);
    std::fs::write(&unknown_path, b"{incomplete").unwrap();
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    for (path, stamp) in [
        (&valid_path, modified),
        (
            &unknown_path,
            if tied {
                modified
            } else {
                modified - Duration::from_secs(10)
            },
        ),
    ] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
    }
    (
        TempHome(root),
        vec![(valid, valid_path), (unknown, unknown_path)],
    )
}

// The reader now retains the enumeration stamps. Adapt the same on-disk red
// fixtures without changing their timestamps, contents, ordering or assertions.
fn review_stamped(
    candidates: Vec<(Uuid, PathBuf)>,
) -> impl Iterator<Item = (std::time::SystemTime, Option<Uuid>, PathBuf)> {
    candidates.into_iter().map(|(id, path)| {
        (
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            Some(id),
            path,
        )
    })
}

#[test]
fn review_rollout_equal_timestamp_unreadable_candidate_blocks_the_limit() {
    let (home, candidates) = review_rollout_candidates(true);
    assert_eq!(
        super::codex_sessions::newest_matches(review_stamped(candidates), &home.0, 1, &mut || {
            false
        }),
        None,
        "a full limit does not rule out an unreadable candidate at the same timestamp"
    );
}

#[test]
fn review_rollout_deadline_inside_a_timestamp_group_withholds_tied_matches() {
    let (home, candidates) = review_rollout_candidates(true);
    let mut opened = 0;
    assert_eq!(
        super::codex_sessions::newest_matches(review_stamped(candidates), &home.0, 1, &mut || {
            opened += 1;
            opened > 1
        }),
        None,
        "expiry before an equally new candidate is checked must withhold the tied match"
    );
}

#[test]
fn review_rollout_older_unreadable_candidate_keeps_the_verified_newer_prefix() {
    let (home, candidates) = review_rollout_candidates(false);
    let expected = super::CodexSession {
        id: candidates[0].0.to_string(),
        path: candidates[0].1.clone(),
    };
    assert_eq!(
        super::codex_sessions::newest_matches(review_stamped(candidates), &home.0, 2, &mut || {
            false
        }),
        Some(vec![expected]),
        "an older unresolved candidate cannot invalidate a strictly newer verified match"
    );
}

fn review_rollout_compression_siblings() -> (TempHome, String) {
    use std::time::Duration;
    let root = std::env::temp_dir().join(format!("muxel-codex-siblings-{}", Uuid::new_v4()));
    let id = "01a1543c-9700-7000-8000-000000000001";
    let jsonl = root
        .join(".codex/sessions/2026/10/19")
        .join(format!("rollout-{id}.jsonl"));
    write_rollout_at(&jsonl, serde_json::json!({"id":id,"cwd":root}), 60);
    std::fs::write(jsonl.with_extension("jsonl.zst"), [0x28, 0xb5, 0x2f, 0xfd]).unwrap();
    // Explicit timestamps make the compressed sibling strictly newer.
    let earlier = std::time::SystemTime::now() - Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(jsonl)
        .unwrap()
        .set_modified(earlier)
        .unwrap();
    (TempHome(root), id.to_string())
}

#[test]
fn review_rollout_restore_uses_the_readable_compression_sibling() {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};
    let (home, id) = review_rollout_compression_siblings();
    let found = super::codex_latest_sessions(
        &home.0,
        &home.0,
        &HashSet::new(),
        1,
        Instant::now() + Duration::from_secs(60),
    );
    assert_eq!(
        found.map(|sessions| sessions
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>()),
        Some(vec![id])
    );
}

#[test]
fn review_rollout_import_uses_the_readable_compression_sibling() {
    let (home, id) = review_rollout_compression_siblings();
    assert_eq!(super::codex_latest_session_id(&home.0, &home.0), Some(id));
}

fn excluded_subagent_siblings(unreadable_newer: bool) -> (TempHome, String) {
    let root = std::env::temp_dir().join(format!("muxel-codex-subagent-{}", Uuid::new_v4()));
    let conversation = "01a1543c-9700-7000-8000-000000000001";
    let subagent = "01a1543c-9700-7000-8000-000000000002";
    let day = root.join(".codex/sessions/2026/10/19");
    write_rollout_at(
        &day.join(format!("rollout-{conversation}.jsonl")),
        serde_json::json!({"id":conversation,"cwd":root}),
        60,
    );
    let jsonl = day.join(format!("rollout-{subagent}.jsonl"));
    write_rollout_at(
        &jsonl,
        serde_json::json!({"id":subagent,"cwd":root,"source":"subagent"}),
        0,
    );
    let compressed = jsonl.with_extension("jsonl.zst");
    std::fs::write(&compressed, [0x28, 0xb5, 0x2f, 0xfd]).unwrap();
    let newest = std::time::SystemTime::now();
    for (path, age) in [
        (&jsonl, if unreadable_newer { 10 } else { 0 }),
        (&compressed, if unreadable_newer { 0 } else { 10 }),
    ] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(newest - std::time::Duration::from_secs(age))
            .unwrap();
    }
    (TempHome(root), conversation.to_string())
}

fn assert_subagent_siblings_allow_restore(unreadable_newer: bool) {
    use std::collections::HashSet;
    use std::time::{Duration, Instant};
    let (home, conversation) = excluded_subagent_siblings(unreadable_newer);
    let found = super::codex_latest_sessions(
        &home.0,
        &home.0,
        &HashSet::new(),
        1,
        Instant::now() + Duration::from_secs(60),
    );
    assert_eq!(
        found.map(|sessions| sessions
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>()),
        Some(vec![conversation]),
        "a confirmed subagent excludes its unreadable same-UUID sibling"
    );
}

#[test]
fn subagent_siblings_restore_with_unreadable_copy_newer() {
    assert_subagent_siblings_allow_restore(true);
}

#[test]
fn subagent_siblings_restore_with_readable_copy_newer() {
    assert_subagent_siblings_allow_restore(false);
}

#[test]
fn subagent_siblings_import_with_unreadable_copy_newer() {
    let (home, conversation) = excluded_subagent_siblings(true);
    assert_eq!(
        super::codex_latest_session_id(&home.0, &home.0),
        Some(conversation)
    );
}

#[test]
fn subagent_siblings_import_with_readable_copy_newer() {
    let (home, conversation) = excluded_subagent_siblings(false);
    assert_eq!(
        super::codex_latest_session_id(&home.0, &home.0),
        Some(conversation)
    );
}

#[test]
fn walked_codex_id_is_found_outside_its_day_and_a_complete_miss_is_missing() {
    use super::codex_rollout_by_walk;
    use std::time::{Duration, Instant};
    let root = std::env::temp_dir().join(format!("muxel-codex-walk-{}", Uuid::new_v4()));
    let _home = TempHome(root.clone());
    let later = Instant::now() + Duration::from_secs(60);
    // No store here proves nothing: CODEX_HOME may have moved it.
    assert_eq!(
        codex_rollout_by_walk(&root, "01a1543c-9700-7000-8000-000000000001", later),
        CodexResumePath::Unknown
    );
    let v7 = "01a1543c-9700-7000-8000-000000000001";
    let pre_v7 = "6f9619ff-8b86-4011-b42d-00cf4fc964ff";
    let moved = root
        .join(".codex/sessions/2020/01/01")
        .join(format!("rollout-2020-01-01T00-00-00-{v7}.jsonl"));
    let old = root
        .join(".codex/sessions/2025/03/04")
        .join(format!("rollout-2025-03-04T00-00-00-{pre_v7}.jsonl.zst"));
    write_rollout_at(&moved, serde_json::json!({"id": v7, "cwd": "/w"}), 0);
    write_rollout_at(&old, serde_json::json!({"id": pre_v7, "cwd": "/w"}), 0);
    // The dated lookup cannot establish either; the walk can.
    assert_eq!(
        codex_resume_path(&root, v7, None).unwrap(),
        CodexResumePath::Unknown
    );
    assert_eq!(
        codex_rollout_by_walk(&root, v7, later),
        CodexResumePath::Found(moved)
    );
    assert_eq!(
        codex_rollout_by_walk(&root, &pre_v7.to_uppercase(), later),
        CodexResumePath::Found(old)
    );
    OPERATIONS.with_borrow_mut(Vec::clear);
    assert_eq!(
        codex_rollout_by_walk(&root, "01a1543c-9700-7000-8000-000000000002", later),
        CodexResumePath::Missing
    );
    assert!(
        OPERATIONS.with_borrow(|ops| !ops.iter().any(|op| matches!(op, Operation::Open(_)))),
        "an id walk matches filenames without opening rollouts"
    );
    // A walk cut short proves nothing, and neither does a non-UUID id.
    assert_eq!(
        codex_rollout_by_walk(&root, v7, Instant::now()),
        CodexResumePath::Unknown
    );
    assert_eq!(
        codex_rollout_by_walk(&root, "not-a-uuid", later),
        CodexResumePath::Unknown
    );
}

#[test]
fn latest_codex_sessions_are_newest_unowned_top_level_rollouts_for_the_directory() {
    use super::codex_latest_sessions;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};
    let root = std::env::temp_dir().join(format!("muxel-codex-latest-{}", Uuid::new_v4()));
    let _home = TempHome(root.clone());
    let cwd = root.join("project");
    let elsewhere = root.join("other");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let id = |n: u32| format!("01a1543c-9700-7000-8000-{n:012}");
    let rollout = |day: &str, n: u32| {
        root.join(".codex/sessions")
            .join(day)
            .join(format!("rollout-{}.jsonl", id(n)))
    };
    // Newest first by modification time, whatever day directory holds it.
    let owned = rollout("2026/10/19", 1);
    let subagent = rollout("2026/10/19", 2);
    let other_dir = rollout("2026/10/19", 3);
    let resumed_old = rollout("2026/01/02", 4);
    let older = rollout("2026/10/18", 5);
    let oldest = rollout("2026/10/17", 6);
    write_rollout_at(&owned, serde_json::json!({"id": id(1), "cwd": cwd}), 10);
    write_rollout_at(
        &subagent,
        serde_json::json!({"id": id(2), "cwd": cwd, "source": {"subagent": "review"}}),
        20,
    );
    write_rollout_at(
        &other_dir,
        serde_json::json!({"id": id(3), "cwd": elsewhere}),
        30,
    );
    write_rollout_at(
        &resumed_old,
        serde_json::json!({"id": id(4), "cwd": cwd}),
        40,
    );
    write_rollout_at(&older, serde_json::json!({"id": id(5), "cwd": cwd}), 50);
    write_rollout_at(&oldest, serde_json::json!({"id": id(6), "cwd": cwd}), 60);
    let owned_ids: HashSet<String> = [id(1)].into_iter().collect();
    let later = Instant::now() + Duration::from_secs(60);

    OPERATIONS.with_borrow_mut(Vec::clear);
    let found = codex_latest_sessions(&root, &cwd, &owned_ids, 2, later).unwrap();
    assert_eq!(
        found
            .iter()
            .map(|session| (session.id.clone(), session.path.clone()))
            .collect::<Vec<_>>(),
        vec![(id(4), resumed_old.clone()), (id(5), older.clone())]
    );
    let opened: Vec<_> = OPERATIONS.with_borrow(|ops| {
        ops.iter()
            .filter_map(|op| match op {
                Operation::Open(path) => Some(path.clone()),
                _ => None,
            })
            .collect()
    });
    // The owned rollout is never opened, and nothing older than the last match.
    assert_eq!(
        opened,
        vec![subagent, other_dir, resumed_old.clone(), older]
    );

    assert_eq!(
        codex_latest_sessions(&root, &cwd, &HashSet::new(), 1, later)
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
        vec![id(1)]
    );
    assert_eq!(
        codex_latest_sessions(&root, &root.join("missing"), &owned_ids, 1, later),
        Some(Vec::new())
    );
    assert_eq!(
        codex_latest_sessions(&root, &cwd, &owned_ids, 1, Instant::now()),
        None,
        "a walk cut short has no answer"
    );
}

#[test]
fn walk_through_a_linked_day_directory_is_inconclusive() {
    use super::{codex_latest_sessions, codex_rollout_by_walk};
    use std::collections::HashSet;
    use std::time::{Duration, Instant};
    let root = std::env::temp_dir().join(format!("muxel-codex-link-{}", Uuid::new_v4()));
    let _home = TempHome(root.clone());
    let elsewhere = root.join("elsewhere/2026/10/19");
    let id = "01a1543c-9700-7000-8000-000000000001";
    write_rollout_at(
        &elsewhere.join(format!("rollout-{id}.jsonl")),
        serde_json::json!({"id": id, "cwd": root}),
        0,
    );
    let month = root.join(".codex/sessions/2026/10");
    std::fs::create_dir_all(&month).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&elsewhere, month.join("19")).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_dir(&elsewhere, month.join("19")).is_err() {
        return; // Creating links needs Developer Mode or elevation.
    }
    let later = Instant::now() + Duration::from_secs(60);
    // The link is not followed, so its rollouts are unread: not a confirmed miss.
    assert_eq!(
        codex_rollout_by_walk(&root, "01a1543c-9700-7000-8000-000000000002", later),
        CodexResumePath::Unknown
    );
    assert_eq!(
        codex_latest_sessions(&root, &root, &HashSet::new(), 1, later),
        None
    );
}

#[test]
fn latest_lookup_out_of_time_while_opening_keeps_the_newest_matches_found() {
    use super::codex_sessions::newest_matches;
    let root = std::env::temp_dir().join(format!("muxel-codex-open-{}", Uuid::new_v4()));
    let _home = TempHome(root.clone());
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let id = |n: u128| Uuid::from_u128(0x01a1543c_9700_7000_8000_000000000000 | n);
    let rollouts: Vec<_> = (1..=4)
        .map(|n| {
            let path = root.join(format!("rollout-{}.jsonl", id(n)));
            let recorded = if n == 1 {
                cwd.clone()
            } else {
                root.join("other")
            };
            write_rollout_at(
                &path,
                serde_json::json!({"id": id(n).to_string(), "cwd": recorded}),
                n as u64,
            );
            (
                std::fs::metadata(&path).unwrap().modified().unwrap(),
                Some(id(n)),
                path,
            )
        })
        .collect();
    // Two panes want two matches, but only the newest rollout is this
    // directory's; time runs out among the older ones.
    let mut opened = 0;
    let found = newest_matches(rollouts.clone().into_iter(), &cwd, 2, &mut || {
        opened += 1;
        opened > 2
    })
    .expect("a match found before the deadline is still the newest");
    assert_eq!(
        found
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>(),
        vec![id(1).to_string()]
    );
    // Nothing found before the deadline is no answer at all.
    assert_eq!(
        newest_matches(rollouts.into_iter().skip(1), &cwd, 1, &mut || true),
        None
    );
}
