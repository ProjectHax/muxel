//! Shared test helpers for `integrations.rs` and `libraries.rs`. No
//! `use gpui::*` here, so `#[test]` in the modules that use them stays the built-in one.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitStopReason {
    Deadline,
    Exited,
    PollError,
}

#[derive(Clone, Copy, Debug)]
pub struct GitTimingReceipt {
    pub pid: u32,
    pub deadline: Instant,
    pub spawn_started: Instant,
    pub spawn_finished: Instant,
    pub wait_entered: Instant,
    pub stop: GitStopReason,
    pub stop_observed: Instant,
    pub kill_started: Option<Instant>,
    pub kill_finished: Option<Instant>,
    pub wait_finished: Instant,
    pub wait_succeeded: bool,
}

#[derive(Default)]
pub struct GitTimings {
    pub job_starts: Vec<Instant>,
    pub children: Vec<GitTimingReceipt>,
}

// Process creation and tree termination can each take seconds on a loaded
// Windows host. They are measured separately; application delay is still
// charged against these fixed allowances, rather than a larger wall ceiling.
const TIMEOUT_RECOGNITION_ALLOWANCE: Duration = Duration::from_secs(1);
const TIMEOUT_CLEANUP_ALLOWANCE: Duration = Duration::from_secs(1);

fn validate_git_timeout(
    receipts: &[GitTimingReceipt],
    expected_deadline: Instant,
    operation_finished: Instant,
    spawned: usize,
    reaped_ids: &[u32],
) -> Result<(), String> {
    if spawned == 0 || receipts.len() != spawned || reaped_ids.len() != spawned {
        return Err("every spawned child must have a receipt and be reaped".into());
    }
    let mut remaining: std::collections::HashSet<_> = reaped_ids.iter().copied().collect();
    if remaining.len() != spawned {
        return Err("reaped child IDs must be unique".into());
    }
    let mut os_time = Duration::ZERO;
    for receipt in receipts {
        if !remaining.remove(&receipt.pid) {
            return Err("receipt must identify its reaped child exactly once".into());
        }
        if receipt.deadline != expected_deadline {
            return Err("the requested absolute deadline changed".into());
        }
        if receipt.stop != GitStopReason::Deadline {
            return Err("the deadline branch must cause termination".into());
        }
        if !receipt.wait_succeeded {
            return Err("the explicit child wait must succeed".into());
        }
        let (Some(kill_started), Some(kill_finished)) =
            (receipt.kill_started, receipt.kill_finished)
        else {
            return Err("deadline termination must have a measured OS kill interval".into());
        };
        if !(receipt.spawn_started <= receipt.spawn_finished
            && receipt.spawn_finished <= receipt.wait_entered
            && receipt.wait_entered <= receipt.stop_observed
            && expected_deadline <= receipt.stop_observed
            && receipt.stop_observed <= kill_started
            && kill_started <= kill_finished
            && kill_finished <= receipt.wait_finished
            && receipt.wait_finished <= operation_finished)
        {
            return Err("timeout receipt timestamps are out of order".into());
        }
        // A blocked spawn cannot be interrupted by the polling loop. Once it
        // returns, neither stderr setup nor late loop entry gets a free pass.
        let recognition = receipt
            .stop_observed
            .duration_since(expected_deadline.max(receipt.spawn_finished));
        if recognition > TIMEOUT_RECOGNITION_ALLOWANCE {
            return Err(format!("deadline recognition was late by {recognition:?}"));
        }
        os_time += receipt
            .spawn_finished
            .saturating_duration_since(expected_deadline.max(receipt.spawn_started));
        os_time += kill_finished.duration_since(kill_started);
    }
    // Exclude only actual overdue process creation and OS termination calls.
    // Child waiting, pre-spawn work, temp cleanup and library rereads remain
    // visible, including when startup itself outlasted the deadline.
    let charged_lateness = operation_finished
        .saturating_duration_since(expected_deadline)
        .saturating_sub(os_time);
    if charged_lateness > TIMEOUT_RECOGNITION_ALLOWANCE + TIMEOUT_CLEANUP_ALLOWANCE {
        return Err(format!(
            "non-OS timeout work was late by {charged_lateness:?}"
        ));
    }
    Ok(())
}

pub fn git_job_deadline(env: &crate::integrations::GitEnv, timeout: Duration) -> Instant {
    let timings = env
        .timings
        .as_ref()
        .expect("enable timing receipts")
        .lock()
        .unwrap();
    assert_eq!(timings.job_starts.len(), 1, "one independently timed job");
    timings.job_starts[0] + timeout
}

pub fn assert_git_timeout(
    env: &crate::integrations::GitEnv,
    expected_deadline: Instant,
    operation_finished: Instant,
) {
    let timings = env
        .timings
        .as_ref()
        .expect("enable timing receipts")
        .lock()
        .unwrap();
    let reaped = env
        .reaped
        .as_ref()
        .expect("track reaped children")
        .lock()
        .unwrap();
    let reaped_ids: Vec<_> = reaped.iter().map(std::process::Child::id).collect();
    if let Err(reason) = validate_git_timeout(
        &timings.children,
        expected_deadline,
        operation_finished,
        env.spawned.load(Ordering::SeqCst),
        &reaped_ids,
    ) {
        panic!("{reason}; receipts: {:?}", timings.children);
    }
}

#[cfg(test)]
mod git_timeout_budget_tests {
    use super::{GitStopReason, GitTimingReceipt, validate_git_timeout};
    use std::time::{Duration, Instant};

    fn receipt(start: Instant) -> GitTimingReceipt {
        GitTimingReceipt {
            pid: 1,
            deadline: start + Duration::from_secs(2),
            spawn_started: start,
            spawn_finished: start + Duration::from_secs(12),
            wait_entered: start + Duration::from_millis(12_010),
            stop: GitStopReason::Deadline,
            stop_observed: start + Duration::from_millis(12_020),
            kill_started: Some(start + Duration::from_millis(12_020)),
            kill_finished: Some(start + Duration::from_millis(42_020)),
            wait_finished: start + Duration::from_millis(42_030),
            wait_succeeded: true,
        }
    }

    fn check(receipt: GitTimingReceipt, deadline: Instant) -> Result<(), String> {
        validate_git_timeout(
            &[receipt],
            deadline,
            receipt.wait_finished + Duration::from_millis(10),
            1,
            &[1],
        )
    }

    #[test]
    fn long_os_calls_do_not_hide_a_prompt_absolute_deadline() {
        let start = Instant::now();
        assert!(check(receipt(start), start + Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn restarted_deadline_or_wrong_stop_or_missing_wait_is_rejected() {
        let start = Instant::now();
        let original = receipt(start);
        for changed in [
            GitTimingReceipt {
                deadline: original.spawn_finished + Duration::from_secs(2),
                ..original
            },
            GitTimingReceipt {
                stop: GitStopReason::PollError,
                ..original
            },
            GitTimingReceipt {
                stop: GitStopReason::Exited,
                ..original
            },
            GitTimingReceipt {
                wait_succeeded: false,
                ..original
            },
        ] {
            assert!(check(changed, original.deadline).is_err());
        }
    }

    #[test]
    fn late_recognition_and_late_wait_entry_are_both_rejected() {
        let original = receipt(Instant::now());
        for late_entry in [false, true] {
            let mut changed = original;
            let delay = Duration::from_secs(2);
            if late_entry {
                changed.wait_entered += delay;
            }
            changed.stop_observed += delay;
            changed.kill_started = changed.kill_started.map(|at| at + delay);
            changed.kill_finished = changed.kill_finished.map(|at| at + delay);
            changed.wait_finished += delay;
            assert!(check(changed, original.deadline).is_err());
        }
    }

    #[test]
    fn child_wait_and_pre_spawn_delay_remain_charged() {
        let original = receipt(Instant::now());
        let mut slow_wait = original;
        slow_wait.wait_finished += Duration::from_secs(3);
        assert!(check(slow_wait, original.deadline).is_err());
        let mut slow_setup = original;
        slow_setup.spawn_started += Duration::from_secs(6);
        assert!(check(slow_setup, original.deadline).is_err());
    }

    #[test]
    fn missing_receipts_or_unreaped_children_are_rejected() {
        let original = receipt(Instant::now());
        assert!(
            validate_git_timeout(&[], original.deadline, original.wait_finished, 1, &[1]).is_err()
        );
        assert!(
            validate_git_timeout(
                &[original],
                original.deadline,
                original.wait_finished,
                1,
                &[]
            )
            .is_err()
        );
    }
}

/// How a [`TestServer`] treats each connection it accepts.
#[derive(Clone, Copy)]
enum Behavior {
    /// Answer every HTTP request with `401` + `WWW-Authenticate: Basic`.
    Unauthorized,
    /// Accept the connection and never write anything back.
    Silent,
}

/// A local HTTP server on `127.0.0.1:<random port>`, stopped on drop.
pub struct TestServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    /// A server that asks for credentials (`401`) on every request.
    pub fn unauthorized() -> Self {
        Self::start(Behavior::Unauthorized)
    }

    /// A server that accepts connections and never answers (time limits).
    pub fn silent() -> Self {
        Self::start(Behavior::Silent)
    }

    pub fn url(&self, path: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}",
            self.port,
            path.trim_start_matches('/')
        )
    }

    fn start(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let port = listener.local_addr().expect("local addr").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = std::thread::spawn(move || {
            // Held open (silent) until the server stops.
            let mut held: Vec<TcpStream> = Vec::new();
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => match behavior {
                        Behavior::Silent => held.push(stream),
                        Behavior::Unauthorized => answer_unauthorized(stream),
                    },
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
            drop(held);
        });
        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }
}

fn answer_unauthorized(mut stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let _ = stream.write_all(
        b"HTTP/1.1 401 Unauthorized\r\n\
          WWW-Authenticate: Basic realm=\"muxel-test\"\r\n\
          Content-Length: 0\r\n\
          Connection: close\r\n\r\n",
    );
    let _ = stream.flush();
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A new, short path directly under the temp directory (not created), so clones
/// and pack files below it stay under the Windows `MAX_PATH` limit on CI.
pub fn short_temp_path() -> PathBuf {
    let mut hex = uuid::Uuid::new_v4().simple().to_string();
    hex.truncate(8);
    std::env::temp_dir().join(format!("mxl-{hex}"))
}

/// An empty `GIT_CONFIG_GLOBAL` for test git commands, so the user's global
/// config (autocrlf, signing, hooks…) never leaks in.
fn empty_git_config() -> &'static std::path::Path {
    static EMPTY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| {
        let path = short_temp_path();
        std::fs::write(&path, "").expect("write empty git config");
        path
    })
}

/// Run `git <args>` in `dir` with an isolated config and a fixed identity;
/// panics if git fails. Returns stdout, trimmed.
pub fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=muxel-test",
            "-c",
            "user.email=muxel-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_git_config())
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn file_url(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    format!("file:///{}", path.trim_start_matches('/'))
}

/// A temporary git repository (`main` branch) standing in for a library's
/// remote `R`; removed on drop.
pub struct TestRepo {
    path: PathBuf,
}

impl TestRepo {
    pub fn init() -> Self {
        let path = short_temp_path();
        std::fs::create_dir_all(&path).expect("create test repo dir");
        git_out(&path, &["init", "-q", "-b", "main"]);
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn git(&self, args: &[&str]) -> String {
        git_out(&self.path, args)
    }

    /// Write `files`, stage everything and commit. Returns the new HEAD.
    pub fn commit(&self, files: &[(&str, &str)], msg: &str) -> String {
        self.write(files);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Replace the tip commit as a force-push would, so a clone of the old tip
    /// can no longer fast-forward. Returns the new HEAD.
    pub fn force_push(&self, files: &[(&str, &str)], msg: &str) -> String {
        self.write(files);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--amend", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn write(&self, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let file = self.path.join(rel);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).expect("create parent dir");
            }
            std::fs::write(&file, content).expect("write test repo file");
        }
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// An askpass program that writes `marker` when git runs it and answers
/// with a dummy secret: a `.bat` on Windows, a `sh` script elsewhere.
pub fn askpass_script(dir: &Path, marker: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let script = dir.join("askpass.bat");
        let body = format!(
            "@echo off\r\necho called> \"{}\"\r\necho secret\r\n",
            marker.display()
        );
        std::fs::write(&script, body).unwrap();
        script
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("askpass.sh");
        let body = format!(
            "#!/bin/sh\necho called > '{}'\necho secret\n",
            marker.display()
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }
}
