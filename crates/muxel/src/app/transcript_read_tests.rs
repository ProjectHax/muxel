//! Exercise the production transcript-selection and tail-read boundaries.
//! Forbidden operations are recorded and denied before touching special files
//! or network paths, so the vulnerable baseline fails without opening them.

use super::{claude_reply_transcript, read_file_tail};
use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Default)]
struct IoSpy {
    operations: Vec<(&'static str, PathBuf)>,
    deny_all: bool,
    deny_open: bool,
    append_before_read: Option<Vec<u8>>,
}

thread_local! {
    static IO: RefCell<IoSpy> = RefCell::new(IoSpy::default());
}

pub(super) fn observe(kind: &'static str, path: &Path) -> bool {
    let (allowed, append) = IO.with_borrow_mut(|spy| {
        spy.operations.push((kind, path.to_path_buf()));
        let allowed = !(spy.deny_all || spy.deny_open && kind == "open");
        let append = (kind == "read")
            .then(|| spy.append_before_read.take())
            .flatten();
        (allowed, append)
    });
    if let Some(bytes) = append {
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
    }
    allowed
}

struct Fixture {
    home: PathBuf,
    id: String,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        IO.with_borrow_mut(|spy| *spy = IoSpy::default());
        let home = std::env::temp_dir().join(format!("muxel-pr108-read-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        // TEMP may use an 8.3 alias on hosted Windows. This fixture tests the
        // verbatim namespace prefix, not filesystem alias resolution: use one
        // physical spelling for both its trusted home and transcript paths.
        let home = std::fs::canonicalize(home).unwrap();
        #[cfg(windows)]
        let home = PathBuf::from(home.to_str().unwrap().strip_prefix(r"\\?\").unwrap());
        let id = Uuid::new_v4().to_string();
        let path = home
            .join(".claude/projects/project")
            .join(format!("{id}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        Self { home, id, path }
    }

    fn read(&self, path: &Path, max: u64) -> Option<String> {
        claude_reply_transcript(&self.home, Some(&self.id), Some(path))
            .as_deref()
            .and_then(|path| read_file_tail(path, max))
    }

    fn rejects_before_any_io(&self, path: &Path) {
        IO.with_borrow_mut(|spy| {
            *spy = IoSpy {
                deny_all: true,
                ..IoSpy::default()
            }
        });
        assert!(self.read(path, 16).is_none());
        assert_eq!(
            IO.with_borrow(|spy| spy.operations.clone()),
            [],
            "untrusted path reached filesystem boundary: {path:?}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn pr108_outside_and_wrong_session_paths_are_rejected_before_any_io() {
    let fixture = Fixture::new();
    let outside = fixture.home.join(format!("{}.jsonl", fixture.id));
    std::fs::write(&outside, "peer-controlled transcript").unwrap();
    fixture.rejects_before_any_io(&outside);
    fixture.rejects_before_any_io(
        &fixture
            .path
            .with_file_name(format!("{}.jsonl", Uuid::new_v4())),
    );
    fixture.rejects_before_any_io(&fixture.path.with_extension("txt"));
    fixture.rejects_before_any_io(
        &fixture
            .path
            .parent()
            .unwrap()
            .join("..")
            .join(format!("{}.jsonl", fixture.id)),
    );
}

#[test]
fn pr108_transcript_needs_a_valid_session_identity_before_any_io() {
    let fixture = Fixture::new();
    for id in [None, Some("not-a-session-id")] {
        IO.with_borrow_mut(|spy| {
            *spy = IoSpy {
                deny_all: true,
                ..IoSpy::default()
            }
        });
        let selected = claude_reply_transcript(&fixture.home, id, Some(&fixture.path));
        let read = selected
            .as_deref()
            .and_then(|path| read_file_tail(path, 16));
        assert!(read.is_none());
        assert!(IO.with_borrow(|spy| spy.operations.is_empty()));
    }
}

#[test]
fn pr108_valid_regular_transcript_reads_its_bounded_tail() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.path, "0123456789").unwrap();
    assert_eq!(fixture.read(&fixture.path, 4).as_deref(), Some("6789"));
    assert!(IO.with_borrow(|spy| {
        spy.operations
            .iter()
            .any(|(kind, path)| *kind == "open" && path == &fixture.path)
    }));
}

#[test]
fn pr108_transcript_growth_after_stat_cannot_exceed_the_read_budget() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.path, "0123456789").unwrap();
    IO.with_borrow_mut(|spy| spy.append_before_read = Some(b"abcdefghij".to_vec()));
    assert_eq!(fixture.read(&fixture.path, 4).as_deref(), Some("6789"));
}

#[cfg(unix)]
#[test]
fn pr108_dev_zero_is_rejected_without_stat_or_open() {
    let fixture = Fixture::new();
    fixture.rejects_before_any_io(Path::new("/dev/zero"));
}

#[cfg(unix)]
#[test]
fn pr108_fifo_with_valid_transcript_name_is_rejected_before_open() {
    use std::os::unix::ffi::OsStrExt;
    let fixture = Fixture::new();
    let path = std::ffi::CString::new(fixture.path.as_os_str().as_bytes()).unwrap();
    // SAFETY: the NUL-terminated pathname belongs exclusively to this fixture.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    IO.with_borrow_mut(|spy| spy.deny_open = true);
    assert!(fixture.read(&fixture.path, 16).is_none());
    assert!(IO.with_borrow(|spy| spy.operations.iter().all(|(kind, _)| *kind != "open")));
}

#[cfg(unix)]
#[test]
fn pr108_symlink_transcript_or_project_cannot_escape_the_store() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let outside = fixture.home.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join(format!("{}.jsonl", fixture.id));
    std::fs::write(&target, "outside transcript").unwrap();
    symlink(&target, &fixture.path).unwrap();
    IO.with_borrow_mut(|spy| spy.deny_open = true);
    assert!(fixture.read(&fixture.path, 16).is_none());
    assert!(IO.with_borrow(|spy| spy.operations.iter().all(|(kind, _)| *kind != "open")));
    std::fs::remove_file(&fixture.path).unwrap();
    let alias = fixture.home.join(".claude/projects/alias");
    symlink(&outside, &alias).unwrap();
    IO.with_borrow_mut(|spy| spy.operations.clear());
    assert!(
        fixture
            .read(&alias.join(format!("{}.jsonl", fixture.id)), 16)
            .is_none()
    );
    assert!(IO.with_borrow(|spy| spy.operations.iter().all(|(kind, _)| *kind != "open")));
}

#[cfg(windows)]
#[test]
fn pr108_unc_and_device_paths_are_rejected_without_network_or_filesystem_io() {
    let fixture = Fixture::new();
    fixture.rejects_before_any_io(&PathBuf::from(format!(
        r"\\untrusted.invalid\share\{}.jsonl",
        fixture.id
    )));
    fixture.rejects_before_any_io(Path::new(r"\\.\NUL"));
    fixture.rejects_before_any_io(&PathBuf::from(format!(
        r"\\?\UNC\untrusted.invalid\share\{}.jsonl",
        fixture.id
    )));
}

#[cfg(windows)]
#[test]
fn pr108_trusted_local_verbatim_drive_transcript_remains_readable() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.path, "0123456789").unwrap();
    let verbatim = std::fs::canonicalize(&fixture.path).unwrap();
    assert_eq!(fixture.read(&verbatim, 4).as_deref(), Some("6789"));
    let canonical_home = std::fs::canonicalize(&fixture.home).unwrap();
    let selected = claude_reply_transcript(&canonical_home, Some(&fixture.id), Some(&fixture.path));
    assert_eq!(selected.as_deref(), Some(fixture.path.as_path()));
}
