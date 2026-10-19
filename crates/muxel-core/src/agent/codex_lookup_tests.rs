use super::{CodexSessionNames, codex_resume_path, codex_session_id_from_hint};
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
    let root = std::env::temp_dir().join(format!("muxel-codex-bounded-{}", Uuid::new_v4()));
    let cwd = root.join("project");
    let day = root.join(".codex/sessions/2026/10/19");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&day).unwrap();
    for unrelated in ["2025/01/01", "2026/10/18", "2026/09/30"] {
        std::fs::create_dir_all(root.join(".codex/sessions").join(unrelated)).unwrap();
    }
    let id = "01a1543c-9700-7000-8000-000000000001";
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
        Some(id)
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
        Some(path.clone())
    );
    assert_eq!(
        OPERATIONS.with_borrow_mut(std::mem::take),
        vec![Operation::Stat(path.clone())]
    );
    assert_eq!(
        codex_resume_path(&root, "01a1543c-9700-7000-8000-000000000002", Some(&path)).unwrap(),
        None
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
    assert_eq!(codex_resume_path(&root, id, Some(&path)).unwrap(), None);
    assert!(OPERATIONS.with_borrow(Vec::is_empty));
    assert_eq!(codex_resume_path(&root, id, None).unwrap(), None);
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
        Some(compressed.clone())
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
        Some(plain.clone())
    );
    assert_eq!(
        OPERATIONS.with_borrow_mut(std::mem::take),
        vec![Operation::Stat(compressed), Operation::Stat(plain)]
    );
}

#[test]
fn b9_codex_name_index_reads_only_changes_and_resets_after_truncation() {
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
            offset: first.len() as u64,
            bytes: appended.len()
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
