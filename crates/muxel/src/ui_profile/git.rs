//! One record per `git` process muxel starts and waits for, so a poll that keeps
//! asking the same question shows up in the log. Paths and arguments are logged
//! only as hashes; the subcommand and a fixed caller label are the only text.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

/// Distinct commands remembered for the `same=` column.
const MAX_COMMANDS: usize = 4096;

thread_local! {
    static CALLER: Cell<&'static str> = const { Cell::new("unlabeled") };
}

/// Labels the git launches made on this thread until it is dropped. Set it
/// where a poll or action begins; the git helpers it calls pick it up.
#[must_use]
pub struct GitCaller(Option<&'static str>);

pub fn git_caller(label: &'static str) -> GitCaller {
    if !super::is_enabled() {
        return GitCaller(None);
    }
    GitCaller(Some(CALLER.with(|caller| caller.replace(label))))
}

impl Drop for GitCaller {
    fn drop(&mut self) {
        if let Some(previous) = self.0 {
            CALLER.with(|caller| caller.set(previous));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GitLaunch {
    caller: &'static str,
    subcommand: String,
    dir: Option<u64>,
    command: u64,
    elapsed_us: u64,
    /// `None` when git couldn't be started or ended without an exit code.
    status: Option<i32>,
    output: u64,
}

/// Record a finished `git` run. Called only while profiling is on; anything
/// that isn't git (a remote read goes through `ssh`) is ignored.
pub fn git_launched(cmd: &Command, elapsed: Duration, out: &std::io::Result<Output>) {
    let Some(mut launch) = describe(cmd) else {
        return;
    };
    launch.caller = CALLER.with(Cell::get);
    launch.elapsed_us = elapsed.as_micros().min(u64::MAX as u128) as u64;
    match out {
        Ok(out) => {
            launch.status = out.status.code();
            launch.output = output_hash(launch.status, &out.stdout);
        }
        Err(_) => launch.output = output_hash(None, &[]),
    }
    super::ensure_flusher();
    super::defer_record(super::DeferredRecord::GitLaunch {
        at: SystemTime::now(),
        launch: Box::new(launch),
    });
}

/// The subcommand, directory and argument hash of a git `Command`; the
/// outcome fields are left for the caller. `None` for anything but git.
fn describe(cmd: &Command) -> Option<GitLaunch> {
    let program = std::path::Path::new(cmd.get_program()).file_stem()?;
    if !program.eq_ignore_ascii_case("git") {
        return None;
    }
    let mut dir = cmd.get_current_dir().map(|d| fnv(d.as_os_str()));
    let mut subcommand = None;
    let mut args = cmd.get_args();
    let mut command = Fnv::new();
    while let Some(arg) = args.next() {
        command.write(arg);
        if subcommand.is_some() {
            continue;
        }
        if arg == "-c" {
            args.next().inspect(|value| command.write(value));
        } else if arg == "-C" {
            if let Some(value) = args.next() {
                command.write(value);
                dir = Some(fnv(value));
            }
        } else if !arg.to_string_lossy().starts_with('-') {
            subcommand = Some(subcommand_label(arg));
        }
    }
    Some(GitLaunch {
        caller: "unlabeled",
        subcommand: subcommand.unwrap_or_else(|| "none".into()),
        dir,
        command: command.finish(),
        elapsed_us: 0,
        status: None,
        output: 0,
    })
}

/// A git subcommand name as written, or `other` for anything that doesn't look
/// like one, so an argument can never leak into the log.
fn subcommand_label(arg: &OsStr) -> String {
    match arg.to_str() {
        Some(s)
            if !s.is_empty()
                && s.len() <= 24
                && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') =>
        {
            s.to_string()
        }
        _ => "other".into(),
    }
}

fn output_hash(status: Option<i32>, stdout: &[u8]) -> u64 {
    let mut hash = Fnv::new();
    hash.write_bytes(&status.unwrap_or(i32::MIN).to_le_bytes());
    hash.write_bytes(stdout);
    hash.finish()
}

fn fnv(value: &OsStr) -> u64 {
    let mut hash = Fnv::new();
    hash.write(value);
    hash.finish()
}

/// 64-bit FNV-1a: stable across builds and runs, so hashes from two binaries
/// (or a script hashing a fixture's paths) can be compared.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    /// An argument, then a separator so `["ab", "c"]` and `["a", "bc"]` differ.
    fn write(&mut self, value: &OsStr) {
        self.write_bytes(value.to_string_lossy().as_bytes());
        self.write_bytes(&[0]);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Writer-side memory of each command's last output, for the `same=` column.
#[derive(Default)]
pub(super) struct Repeats {
    last: HashMap<u64, u64>,
    order: VecDeque<u64>,
}

impl Repeats {
    /// `Some(true)` when `launch` printed what the same command printed last
    /// time, `Some(false)` when that changed, `None` the first time.
    fn observe(&mut self, launch: &GitLaunch) -> Option<bool> {
        let previous = self.last.insert(launch.command, launch.output);
        if previous.is_none() {
            if self.order.len() == MAX_COMMANDS
                && let Some(oldest) = self.order.pop_front()
            {
                self.last.remove(&oldest);
            }
            self.order.push_back(launch.command);
        }
        previous.map(|output| output == launch.output)
    }

    pub fn line(&mut self, launch: &GitLaunch) -> String {
        let same = match self.observe(launch) {
            Some(true) => "yes",
            Some(false) => "no",
            None => "first",
        };
        let dir = launch
            .dir
            .map_or_else(|| "none".to_string(), |dir| format!("{dir:016x}"));
        let status = launch
            .status
            .map_or_else(|| "none".to_string(), |code| code.to_string());
        format!(
            "ui-prof[git v1] caller={} sub={} dir={dir} cmd={:016x} status={status} elapsed={}µs out={:016x} same={same}",
            launch.caller, launch.subcommand, launch.command, launch.elapsed_us, launch.output,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Fnv, GitLaunch, Repeats, describe, subcommand_label};
    use std::ffi::OsStr;
    use std::process::Command;

    #[test]
    fn fnv_matches_the_published_test_vectors() {
        // From the FNV reference test suite (draft-eastlake-fnv).
        let hash = |s: &str| {
            let mut h = Fnv::new();
            h.write_bytes(s.as_bytes());
            h.finish()
        };
        assert_eq!(hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(hash("foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn describes_the_read_only_form_muxel_runs() {
        let mut cmd = Command::new("git");
        cmd.args(["-c", "diff.autoRefreshIndex=false", "-C", "/repo/wt"])
            .args(["--no-pager", "rev-parse", "--abbrev-ref", "HEAD"]);
        let launch = describe(&cmd).expect("git");
        assert_eq!(launch.subcommand, "rev-parse");
        let mut dir = Fnv::new();
        dir.write(OsStr::new("/repo/wt"));
        assert_eq!(launch.dir, Some(dir.finish()));

        let mut other = Command::new("git");
        other.args(["-c", "diff.autoRefreshIndex=false", "-C", "/repo/other"]);
        other.args(["--no-pager", "rev-parse", "--abbrev-ref", "HEAD"]);
        let other = describe(&other).expect("git");
        assert_ne!(other.command, launch.command, "a different worktree");
    }

    #[test]
    fn ignores_programs_other_than_git() {
        assert!(describe(&Command::new("ssh")).is_none());
        assert!(describe(Command::new("C:/Program Files/Git/cmd/git.exe").arg("status")).is_some());
    }

    #[test]
    fn subcommand_labels_never_carry_arbitrary_text() {
        assert_eq!(subcommand_label(OsStr::new("for-each-ref")), "for-each-ref");
        assert_eq!(subcommand_label(OsStr::new("src/main.rs")), "other");
        assert_eq!(subcommand_label(OsStr::new("Secret")), "other");
    }

    #[test]
    fn same_column_compares_against_the_previous_run_of_that_command() {
        let launch = |command, output| GitLaunch {
            caller: "branch-poll",
            subcommand: "rev-parse".into(),
            dir: None,
            command,
            elapsed_us: 7,
            status: Some(0),
            output,
        };
        let mut repeats = Repeats::default();
        assert!(repeats.line(&launch(1, 10)).ends_with("same=first"));
        assert!(repeats.line(&launch(1, 10)).ends_with("same=yes"));
        assert!(repeats.line(&launch(2, 10)).ends_with("same=first"));
        assert!(repeats.line(&launch(1, 11)).ends_with("same=no"));
        assert_eq!(
            repeats.line(&launch(1, 11)),
            "ui-prof[git v1] caller=branch-poll sub=rev-parse dir=none cmd=0000000000000001 status=0 elapsed=7µs out=000000000000000b same=yes"
        );
    }
}
