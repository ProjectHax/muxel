//! Side-effecting wrappers around the `git` and `tmux` CLIs — the I/O half of
//! muxel-core's pure tmux/worktree helpers.

use crate::i18n::{t, tf};
use anyhow::{Context, Result, bail};
use muxel_core::library::GitFailure;
use muxel_core::library::resync::LocalChanges;
use muxel_core::memory::{self, MemoryEntry};
use muxel_core::{
    MEMORY_DIR, MEMORY_FILE, RemoteHost, RemoteOs, SshAuth, memory_header, remote_ops, ssh,
};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `std::process::Command` for `program`, with the console window suppressed on
/// Windows. muxel is a GUI app, so spawning a console child (git, ssh, gh, …)
/// would otherwise flash a cmd window on every call — extremely visible because
/// muxel polls git in the background. No-op off Windows.
fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW — don't allocate/attach a console for the child.
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Reap stale muxel AppImage squashfuse mounts left in `$TMPDIR` by prior
/// instances that were SIGKILLed or crashed before the AppImage runtime could
/// unmount them. Once such a mount goes dead (`statfs` → `ENOTCONN`), any
/// filesystem scan — `df`, which some desktop system monitors run every ~60s —
/// stalls in the kernel FUSE layer on it, which on Wayland surfaces as a
/// periodic cursor stutter that worsens as more leftovers pile up. muxel can't
/// catch SIGKILL, so it cleans up on the next launch.
///
/// Best-effort and fully detached: runs on a background thread so a hung probe
/// can never block startup; unmounts only mounts that fail a liveness probe (a
/// live one belongs to another running muxel instance) and never our own.
#[cfg(target_os = "linux")]
pub fn reap_stale_appimage_mounts() {
    let _ = std::thread::Builder::new()
        .name("muxel-reap-mounts".into())
        .spawn(|| {
            let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
                return;
            };
            let self_appdir = std::env::var("APPDIR").ok();
            let candidates =
                muxel_core::foreign_muxel_appimage_mounts(&mounts, self_appdir.as_deref());

            let mut reaped = 0usize;
            for mp in candidates {
                // A live mount lists instantly; only reap the dead ones.
                if fuse_mount_is_live(&mp) {
                    continue;
                }
                if lazy_unmount_fuse(&mp) {
                    reaped += 1;
                }
            }
            if reaped > 0 {
                muxel_store::append_event_log(&format!("reaped {reaped} stale AppImage mount(s)"));
            }
        });
}

/// Probe a FUSE mountpoint for liveness. A dead squashfuse mount fails to open
/// or list (`ENOTCONN`); a live one lists instantly. Any error counts as dead —
/// the mount is unusable either way, and the caller only lazy-unmounts, which is
/// safe even if the probe is wrong.
#[cfg(target_os = "linux")]
fn fuse_mount_is_live(mountpoint: &str) -> bool {
    match std::fs::read_dir(mountpoint) {
        // `opendir` succeeded but the first `readdir` may still surface ENOTCONN.
        Ok(mut entries) => !matches!(entries.next(), Some(Err(_))),
        Err(_) => false,
    }
}

/// Lazily unmount a FUSE mountpoint via the user-space `fusermount` helper: it
/// works without root for the mount's owner, and `-z` detaches even a busy or
/// unresponsive mount (a plain unmount fails `EBUSY` on a mount something still
/// references). Returns whether a helper reported success.
#[cfg(target_os = "linux")]
fn lazy_unmount_fuse(mountpoint: &str) -> bool {
    ["fusermount3", "fusermount"].into_iter().any(|bin| {
        command(bin)
            .args(["-u", "-z", mountpoint])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// A remote git location: the host + the repo path on it + the shared
/// ControlMaster socket + (optional) password for password auth.
pub struct RemoteConn {
    pub host: RemoteHost,
    pub remote_path: String,
    pub control_path: String,
    pub password: Option<String>,
}

/// Where a git command runs: a local working tree, or a remote one reached over
/// SSH (reusing the host's ControlMaster, so repeated calls are cheap). The
/// remote variant is boxed (it's much larger than the local one).
pub enum RepoLoc {
    Local(PathBuf),
    Remote(Box<RemoteConn>),
}

impl RepoLoc {
    pub fn remote(
        host: RemoteHost,
        remote_path: String,
        control_path: String,
        password: Option<String>,
    ) -> Self {
        RepoLoc::Remote(Box::new(RemoteConn {
            host,
            remote_path,
            control_path,
            password,
        }))
    }
}

/// Build the local `ssh`/`sshpass` Command that runs `remote_cmd` (one shell
/// string) on the connection's host, reusing its ControlMaster.
fn remote_ssh_command(c: &RemoteConn, remote_cmd: String) -> Command {
    let mut argv = ssh::connection_args(&c.host, &c.control_path);
    // `ConnectTimeout` comes from `base_args`; `BatchMode` makes a key/agent
    // failure fail fast instead of blocking on a password prompt to a non-tty.
    if c.password.is_none() {
        argv.push("-o".into());
        argv.push("BatchMode=yes".into());
    }
    argv.push(ssh::target(&c.host));
    argv.push("--".into());
    argv.push(remote_cmd);
    if c.host.auth == SshAuth::Password {
        let mut cmd = command("sshpass");
        cmd.arg("-e").arg("ssh").args(&argv);
        if let Some(pw) = &c.password {
            cmd.env("SSHPASS", pw);
        }
        cmd
    } else {
        let mut cmd = command("ssh");
        cmd.args(&argv);
        cmd
    }
}

/// Run `git <args>` at a [`RepoLoc`]: locally (`git -C <path>`) or on the host
/// over SSH (`ssh … -- git -C <remote_path> …`, fed as one quoted command).
fn git_output(loc: &RepoLoc, args: &[&str]) -> std::io::Result<std::process::Output> {
    match loc {
        RepoLoc::Local(path) => command("git").arg("-C").arg(path).args(args).output(),
        RepoLoc::Remote(c) => {
            let remote = remote_ops::git(c.host.os, &c.remote_path, args);
            remote_ssh_command(c, remote).output()
        }
    }
}

/// Cap on the size of a remote file muxel will read into the editor (2 MB).
const MAX_REMOTE_BYTES: u64 = 2_000_000;

/// List files under a remote project root: gitignore-aware via `git ls-files`
/// (tracked + untracked) when it's a repo, else a bounded `find`. Returns
/// absolute remote paths (capped). Empty for a local loc or on failure.
pub fn list_remote_files(loc: &RepoLoc) -> Vec<String> {
    let RepoLoc::Remote(c) = loc else {
        return Vec::new();
    };
    let root = c.remote_path.trim_end_matches('/');
    // Same cap as the local walk: the old 10k quietly cut large trees off, and a
    // folder whose files all fell past the cut just vanished from the browser.
    let cap = crate::app::MAX_PROJECT_FILES;
    let cmd = remote_ops::list_files(c.host.os, root, cap);
    let Ok(out) = remote_ssh_command(c, cmd).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_start_matches("./"))
        .filter(|l| !l.is_empty())
        .map(|rel| format!("{root}/{rel}"))
        .collect()
}

/// Read a remote text file's contents (capped at [`MAX_REMOTE_BYTES`]). `None` on
/// failure, a local loc, or if the file is too large.
pub fn read_remote_file(loc: &RepoLoc, abs_path: &str) -> Option<String> {
    let RepoLoc::Remote(c) = loc else {
        return None;
    };
    // Only read when it's a regular file within the size cap.
    let cmd = remote_ops::read_file(c.host.os, abs_path, MAX_REMOTE_BYTES);
    let out = remote_ssh_command(c, cmd).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Every tmux session on a remote host — the input to adopting the ones no instance
/// owns (see `muxel_core::tmux::orphan_sessions`). `None` when the host can't be
/// reached or tmux isn't there; an empty list means "reached it, no sessions", and
/// the two must not be confused — a blip would otherwise read as "nothing running".
pub fn list_remote_tmux_sessions(loc: &RepoLoc) -> Option<Vec<muxel_core::tmux::RemoteSession>> {
    let RepoLoc::Remote(c) = loc else {
        return None;
    };
    if c.host.os.is_windows() {
        // No tmux on Windows, so there is nothing to adopt. `Some(empty)` rather
        // than `None`: the host is reachable and genuinely has no sessions, and
        // `None` would read as "couldn't reach it" and strand the UI in a retry.
        return Some(Vec::new());
    }
    let args = muxel_core::tmux::list_sessions_args();
    // `tmux` is not on sshd's bare default PATH when it came from Homebrew — see
    // `ssh::tmux_path_prelude`. Unresolved, this reads as "no sessions on the host"
    // and every running agent there stays stranded.
    let cmd = std::iter::once("tmux".to_string())
        .chain(args.iter().map(|a| ssh::sh_quote(a)))
        .collect::<Vec<_>>()
        .join(" ");
    let cmd = format!("{}; {cmd}", ssh::tmux_path_prelude());
    let out = remote_ssh_command(c, cmd).output().ok()?;
    if !out.status.success() {
        // "no server running" is a perfectly good answer: nothing is running there.
        let err = String::from_utf8_lossy(&out.stderr);
        return err.contains("no server running").then(Vec::new);
    }
    Some(muxel_core::tmux::parse_sessions(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

/// Every tmux session on this machine, as [`list_remote_tmux_sessions`] reports a
/// host's. An empty list when no tmux server is running; `None` when tmux couldn't
/// be run at all.
pub fn list_local_tmux_sessions() -> Option<Vec<muxel_core::tmux::RemoteSession>> {
    let out = command("tmux")
        .args(muxel_core::tmux::list_sessions_args())
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        // No server (or no socket yet) is a perfectly good answer: nothing running.
        let err = String::from_utf8_lossy(&out.stderr);
        return (err.contains("no server running") || err.contains("error connecting to"))
            .then(Vec::new);
    }
    Some(muxel_core::tmux::parse_sessions(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

/// Write `content` to a remote file (overwriting), piping it over SSH stdin.
pub fn write_remote_file(loc: &RepoLoc, abs_path: &str, content: &str) -> Result<()> {
    let RepoLoc::Remote(c) = loc else {
        bail!("not a remote file");
    };
    use std::io::Write;
    let cmd = remote_ops::write_file(c.host.os, abs_path);
    let mut command = remote_ssh_command(c, cmd);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| ssh_spawn_error(c.host.auth, e))?;
    child
        .stdin
        .take()
        .context("ssh stdin")?
        .write_all(content.as_bytes())
        .context("writing remote file")?;
    let out = child.wait_with_output().context("waiting for ssh")?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        let msg = msg.trim();
        bail!("{}", if msg.is_empty() { "write failed" } else { msg });
    }
    Ok(())
}

/// Add `ignore_line` to `<root>/.gitignore` if not already present (the bare
/// `MEMORY_DIR` form counts too). Idempotent; shared by the memory-file and
/// layout-sync writers.
fn ensure_gitignored(root: &Path, ignore_line: &str) -> Result<()> {
    let gitignore = root.join(".gitignore");
    let current = std::fs::read_to_string(&gitignore).unwrap_or_default();
    let ignored = current
        .lines()
        .any(|l| l.trim() == ignore_line || l.trim() == MEMORY_DIR);
    if ignored {
        return Ok(());
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(ignore_line);
    next.push('\n');
    std::fs::write(&gitignore, next).with_context(|| format!("updating {}", gitignore.display()))
}

/// Ensure a project's shared memory file exists and is git-ignored, idempotently:
/// create `<root>/.muxel/`, seed `MEMORY.md` if absent, and add `.muxel/` to the
/// repo's `.gitignore` if not already there. Works for a local or remote `loc`.
pub fn ensure_memory_file(loc: &RepoLoc) -> Result<()> {
    let ignore_line = format!("{MEMORY_DIR}/");
    match loc {
        RepoLoc::Local(root) => {
            let dir = root.join(MEMORY_DIR);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let file = dir.join(MEMORY_FILE);
            if !file.exists() {
                std::fs::write(&file, memory_header())
                    .with_context(|| format!("writing {}", file.display()))?;
            }
            ensure_gitignored(root, &ignore_line)
        }
        RepoLoc::Remote(c) => {
            let root = c.remote_path.trim_end_matches('/');
            let file = format!("{MEMORY_DIR}/{MEMORY_FILE}");
            let cmd = remote_ops::ensure_seeded_file(
                c.host.os,
                root,
                MEMORY_DIR,
                &file,
                memory_header(),
                &ignore_line,
            );
            let out = remote_ssh_command(c, cmd)
                .output()
                .map_err(|e| ssh_spawn_error(c.host.auth, e))?;
            if !out.status.success() {
                let msg = String::from_utf8_lossy(&out.stderr);
                let msg = msg.trim();
                bail!(
                    "{}",
                    if msg.is_empty() {
                        "ensure memory failed"
                    } else {
                        msg
                    }
                );
            }
            Ok(())
        }
    }
}

/// Absolute path of a project's `.muxel/MEMORY.md`, local or remote.
fn memory_abs(loc: &RepoLoc) -> String {
    match loc {
        RepoLoc::Local(root) => root
            .join(MEMORY_DIR)
            .join(MEMORY_FILE)
            .display()
            .to_string(),
        RepoLoc::Remote(c) => format!(
            "{}/{MEMORY_DIR}/{MEMORY_FILE}",
            c.remote_path.trim_end_matches('/')
        ),
    }
}

/// Read and parse a project's memory file into entries. Missing/empty/unreadable →
/// an empty list (the file is optional and created on first save). Works local or
/// remote.
pub fn load_memory(loc: &RepoLoc) -> Vec<MemoryEntry> {
    let text = match loc {
        RepoLoc::Local(root) => {
            std::fs::read_to_string(root.join(MEMORY_DIR).join(MEMORY_FILE)).unwrap_or_default()
        }
        RepoLoc::Remote(_) => read_remote_file(loc, &memory_abs(loc)).unwrap_or_default(),
    };
    memory::parse_document(&text)
}

/// Whether the project already has a `.muxel/MEMORY.md` — i.e. shared memory is
/// plainly in use here, whoever switched it on. Local or remote.
///
/// The evidence of last resort for the shared-memory flag: a layout doc written
/// before that flag existed carries no opinion, and defaulting such a project to
/// "off" would show the toggle off for a project whose agents are demonstrably
/// sharing a memory file on the host.
pub fn memory_file_exists(loc: &RepoLoc) -> bool {
    match loc {
        RepoLoc::Local(root) => root.join(MEMORY_DIR).join(MEMORY_FILE).is_file(),
        RepoLoc::Remote(c) => {
            let cmd = remote_ops::test_file(c.host.os, &memory_abs(loc));
            remote_ssh_command(c, cmd)
                .output()
                .is_ok_and(|o| o.status.success())
        }
    }
}

/// Render `entries` to the project's `.muxel/MEMORY.md` (overwriting). Ensures the
/// `.muxel/` dir exists and is git-ignored first. Works local or remote.
pub fn save_memory(loc: &RepoLoc, entries: &[MemoryEntry]) -> Result<()> {
    let text = memory::render_document(entries);
    ensure_memory_file(loc)?; // dir + .gitignore (+ seed if absent)
    match loc {
        RepoLoc::Local(root) => {
            let file = root.join(MEMORY_DIR).join(MEMORY_FILE);
            std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))
        }
        RepoLoc::Remote(_) => write_remote_file(loc, &memory_abs(loc), &text),
    }
}

/// Filename of a remote project's synced pane layout, under `<root>/.muxel/`.
const REMOTE_LAYOUT_FILE: &str = "workspace.json";
/// One-level backup of the previous layout, written before each overwrite.
const REMOTE_LAYOUT_BAK: &str = "workspace.bak.json";

/// Absolute path of the synced layout file on the remote host.
fn remote_layout_abs(root: &str) -> String {
    format!(
        "{}/{MEMORY_DIR}/{REMOTE_LAYOUT_FILE}",
        root.trim_end_matches('/')
    )
}

/// Path of the synced layout file inside a local project root.
fn local_layout_abs(root: &Path) -> PathBuf {
    root.join(MEMORY_DIR).join(REMOTE_LAYOUT_FILE)
}

/// Write the layout JSON to `<root>/.muxel/workspace.json` on the local filesystem,
/// backing up any current copy to `workspace.bak.json` and git-ignoring `.muxel/`.
/// The local-project mirror of the remote push, so an SSH peer (the iOS app) can
/// read a local project's panes.
fn push_local_layout(root: &Path, json: &str) -> Result<()> {
    let dir = root.join(MEMORY_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = dir.join(REMOTE_LAYOUT_FILE);
    if file.exists() {
        let _ = std::fs::copy(&file, dir.join(REMOTE_LAYOUT_BAK));
    }
    let _ = ensure_gitignored(root, &format!("{MEMORY_DIR}/"));
    std::fs::write(&file, json).with_context(|| format!("writing {}", file.display()))
}

/// The shell command that prepares the remote for a layout push: ensure
/// `<root>/.muxel/` exists, back up any current `workspace.json` to
/// `workspace.bak.json`, and git-ignore `.muxel/`. Pure (no I/O) so its shape is
/// unit-testable; mirrors `ensure_memory_file`'s remote branch.
fn remote_push_prep_cmd(os: RemoteOs, root: &str) -> String {
    let rel = format!("{MEMORY_DIR}/{REMOTE_LAYOUT_FILE}");
    let bak = format!("{MEMORY_DIR}/{REMOTE_LAYOUT_BAK}");
    let ignore_line = format!("{MEMORY_DIR}/");
    remote_ops::push_prep(os, root, MEMORY_DIR, &rel, &bak, &ignore_line)
}

/// Read a project's synced layout JSON (`<root>/.muxel/workspace.json`) — over SSH
/// for a remote project, or from the local filesystem for a local one. `None` if
/// missing, oversized, or unreadable.
pub fn fetch_remote_layout(loc: &RepoLoc) -> Option<String> {
    match loc {
        RepoLoc::Remote(c) => read_remote_file(loc, &remote_layout_abs(&c.remote_path)),
        RepoLoc::Local(root) => {
            let path = local_layout_abs(root);
            if std::fs::metadata(&path).ok()?.len() > MAX_REMOTE_BYTES {
                return None;
            }
            std::fs::read_to_string(&path).ok()
        }
    }
}

/// Push the project's pane-layout JSON to `<root>/.muxel/workspace.json`, backing up
/// the previous copy to `workspace.bak.json` first and ensuring `.muxel/` is
/// git-ignored — over SSH for a remote project, on the local filesystem for a local
/// one. `json` is produced by `muxel_core::RemoteLayout::to_json`.
pub fn push_remote_layout(loc: &RepoLoc, json: &str) -> Result<()> {
    let c = match loc {
        RepoLoc::Remote(c) => c,
        RepoLoc::Local(root) => return push_local_layout(root, json),
    };
    let root = c.remote_path.trim_end_matches('/');
    let out = remote_ssh_command(c, remote_push_prep_cmd(c.host.os, root))
        .output()
        .map_err(|e| ssh_spawn_error(c.host.auth, e))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        let msg = msg.trim();
        bail!(
            "{}",
            if msg.is_empty() {
                "prepare remote layout failed"
            } else {
                msg
            }
        );
    }
    write_remote_file(loc, &remote_layout_abs(root), json)
}

/// `git <args>` at `loc`; trimmed single-line stdout on success, else `None`.
fn git_line_loc(loc: &RepoLoc, args: &[&str]) -> Option<String> {
    let out = git_output(loc, args).ok().filter(|o| o.status.success())?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `git <args>` at `loc`; `bail!`s with stderr (a useful toast) on failure.
fn git_run_loc(loc: &RepoLoc, args: &[&str]) -> Result<String> {
    let out = git_output(loc, args).with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // git writes its human summary to stdout (pull / commit / stash) or stderr
    // (push / fetch) — return whichever is non-empty so callers can surface it.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let summary = stdout.trim();
    Ok(if summary.is_empty() {
        String::from_utf8_lossy(&out.stderr).trim().to_string()
    } else {
        summary.to_string()
    })
}

/// Run a one-off command on a remote host over SSH, reusing/establishing the
/// host's ControlMaster. `ConnectTimeout` + `BatchMode` (non-password) make it
/// fail fast instead of blocking on a prompt; password auth feeds `sshpass` via
/// `$SSHPASS`. `remote_cmd` is a single shell command string (already quoted).
fn ssh_exec(
    host: &RemoteHost,
    control_path: &str,
    password: Option<&str>,
    remote_cmd: &str,
) -> Result<()> {
    let out = ssh_run(host, control_path, password, remote_cmd)?;
    if out.status.success() {
        return Ok(());
    }
    bail!("{}", ssh_error_message(&out));
}

/// Run `remote_cmd` over ssh and return its raw output. Lower level than
/// [`ssh_exec`]: callers inspect the exit status themselves (e.g. to tell an ssh
/// transport failure apart from the remote command exiting non-zero).
fn ssh_run(
    host: &RemoteHost,
    control_path: &str,
    password: Option<&str>,
    remote_cmd: &str,
) -> Result<std::process::Output> {
    let mut argv = ssh::connection_args(host, control_path);
    // `ConnectTimeout` comes from `base_args`; `BatchMode` (non-password) fails
    // fast instead of blocking on a password prompt to a non-tty.
    if password.is_none() {
        argv.push("-o".into());
        argv.push("BatchMode=yes".into());
    }
    argv.push(ssh::target(host));
    argv.push("--".into());
    argv.push(remote_cmd.to_string());

    let mut cmd;
    if host.auth == SshAuth::Password {
        cmd = command("sshpass");
        cmd.arg("-e").arg("ssh").args(&argv);
        if let Some(pw) = password {
            cmd.env("SSHPASS", pw);
        }
    } else {
        cmd = command("ssh");
        cmd.args(&argv);
    }
    cmd.output().map_err(|e| ssh_spawn_error(host.auth, e))
}

/// A human message for an ssh transport/auth failure: ssh's stderr, or a generic
/// line when ssh said nothing.
fn ssh_error_message(out: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    let msg = err.trim();
    if msg.is_empty() {
        "connection failed".to_string()
    } else {
        msg.to_string()
    }
}

/// Whether a non-zero ssh run was ssh's own *transport/auth* failure rather than
/// the remote command exiting non-zero. ssh uses exit code 255 for its own errors
/// and otherwise passes the remote command's status through; `sshpass` uses 2..=6
/// for its auth failures (a passed-through command status — e.g. 1 from `test` —
/// is the command's own code, so it must NOT be treated as a connection failure).
fn is_ssh_transport_failure(auth: SshAuth, code: Option<i32>) -> bool {
    match code {
        Some(255) => true,
        Some(c) if auth == SshAuth::Password && (2..=6).contains(&c) => true,
        _ => false,
    }
}

/// Scan a remote host for muxel projects: `find $HOME` for the
/// `.muxel/workspace.json` markers muxel writes on sync, returning the deduped,
/// sorted project roots. Depth-capped and heavy dirs pruned so it's quick over a
/// one-shot exec channel — the desktop port of the iOS `ProjectDiscovery` scan. A
/// non-zero `find` (unreadable dirs) is fine; only an ssh transport/auth failure is
/// surfaced as an error.
pub fn scan_remote_projects(
    host: &RemoteHost,
    control_path: &str,
    password: Option<&str>,
) -> Result<Vec<String>> {
    const MARKER: &str = "/.muxel/workspace.json";
    /// Directories never worth descending into, on either OS: big, and never a
    /// project root muxel put a marker in.
    const PRUNE: &[&str] = &[
        "node_modules",
        ".git",
        ".cache",
        ".cargo",
        ".rustup",
        ".npm",
        "target",
        "vendor",
        "Library",
        ".Trash",
    ];
    let cmd = remote_ops::scan_projects(host.os, MARKER, 7, PRUNE);
    let out = ssh_run(host, control_path, password, &cmd)?;
    if !out.status.success() && is_ssh_transport_failure(host.auth, out.status.code()) {
        bail!("{}", ssh_error_message(&out));
    }
    let mut roots: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| l.ends_with(MARKER))
        .map(|l| l[..l.len() - MARKER.len()].to_string())
        .filter(|r| !r.is_empty())
        .collect();
    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// Map an ssh/sshpass spawn failure to an actionable message: a missing
/// `sshpass` (saved-password auth, Unix-only) or a missing `ssh`.
fn ssh_spawn_error(auth: SshAuth, e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        if auth == SshAuth::Password {
            anyhow::anyhow!(
                "`sshpass` not found — it's required for saved-password auth and is \
                 Linux/macOS only. Install it, or use a key file / ssh-agent instead."
            )
        } else {
            anyhow::anyhow!("`ssh` not found on PATH")
        }
    } else {
        anyhow::Error::new(e).context("running ssh")
    }
}

/// Verify a host's SSH config by opening a quick connection (`ssh … -- true`).
/// Also establishes the ControlMaster so a subsequent pane connects instantly.
pub fn ssh_check(host: &RemoteHost, control_path: &str, password: Option<&str>) -> Result<()> {
    ssh_exec(host, control_path, password, "true")
}

/// Verify a host's *credentials* with a **fresh** connection. Unlike [`ssh_check`]
/// it never reuses the ControlMaster (a warm socket would otherwise let any
/// password "succeed"), and for password auth it forces password authentication
/// so a working key can't mask a wrong password. Returns the ssh error (e.g.
/// "Permission denied") on failure.
pub fn ssh_verify(host: &RemoteHost, password: Option<&str>) -> Result<()> {
    let mut argv = ssh::base_args(host); // includes ConnectTimeout
    argv.push("-o".into());
    argv.push("ControlPath=none".into()); // never multiplex a credential test
    argv.push("-o".into());
    argv.push("NumberOfPasswordPrompts=1".into());
    if host.auth == SshAuth::Password {
        // Force password auth so a working key can't make a bad password pass.
        argv.push("-o".into());
        argv.push("PreferredAuthentications=password".into());
        argv.push("-o".into());
        argv.push("PubkeyAuthentication=no".into());
    } else {
        // No password to type → fail fast instead of blocking on a prompt.
        argv.push("-o".into());
        argv.push("BatchMode=yes".into());
    }
    argv.push(ssh::target(host));
    argv.push("--".into());
    argv.push("true".into());

    let mut cmd;
    if host.auth == SshAuth::Password {
        cmd = command("sshpass");
        cmd.arg("-e").arg("ssh").args(&argv);
        if let Some(pw) = password {
            cmd.env("SSHPASS", pw);
        }
    } else {
        cmd = command("ssh");
        cmd.args(&argv);
    }
    let out = cmd.output().map_err(|e| ssh_spawn_error(host.auth, e))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err.trim();
        let msg = if msg.is_empty() {
            "connection failed"
        } else {
            msg
        };
        bail!("{msg}");
    }
    Ok(())
}

/// Check that `dir` exists (and is a directory) on the remote host.
pub fn ssh_test_dir(
    host: &RemoteHost,
    control_path: &str,
    password: Option<&str>,
    dir: &str,
) -> Result<()> {
    let out = ssh_run(
        host,
        control_path,
        password,
        &remote_ops::test_dir(host.os, dir),
    )?;
    if out.status.success() {
        return Ok(());
    }
    // ssh connected fine but `test -d` failed → it's the PATH, not the link.
    // A genuine ssh transport/auth failure keeps the connection message instead.
    if is_ssh_transport_failure(host.auth, out.status.code()) {
        bail!("{}", ssh_error_message(&out));
    }
    bail!("directory not found: {dir}");
}

/// Remove a host's stale known_hosts entry (`ssh-keygen [-f <file>] -R <entry>`)
/// — the accept path of the changed-host-key dialog. `entry` is the token ssh
/// itself reported (`example.com`, `[example.com]:2222`, or a config alias);
/// ssh-keygen handles hashed entries itself and backs the file up to
/// `known_hosts.old`. The reconnect then re-pins the new key via `accept-new`.
pub fn forget_host_key(entry: &str, file: Option<&str>) -> Result<()> {
    let out = command("ssh-keygen")
        .args(ssh::keygen_remove_args(entry, file))
        .output()
        .context("run ssh-keygen")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err.trim();
        bail!(
            "{}",
            if msg.is_empty() {
                "ssh-keygen -R failed"
            } else {
                msg
            }
        );
    }
    Ok(())
}

/// The stored known_hosts fingerprints for a host (`ssh-keygen -l -F <entry>`),
/// as `(key type, fingerprint)` pairs — shown as "Stored" in the changed-key
/// dialog. Best-effort: empty on any failure (the dialog says "not found").
pub fn stored_host_key_fingerprints(entry: &str, file: Option<&str>) -> Vec<(String, String)> {
    command("ssh-keygen")
        .args(ssh::keygen_find_args(entry, file))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| ssh::parse_keygen_lookup(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Whether `path` is inside a git working tree.
pub fn is_git_repo(path: &Path) -> bool {
    command("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a worktree at `worktree_path` on a new `branch`, based on `repo`.
pub fn create_worktree(repo: &Path, worktree_path: &Path, branch: &str) -> Result<()> {
    let output = command("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "-b", branch])
        .arg(worktree_path)
        .output()
        .context("running `git worktree add`")?;
    if !output.status.success() {
        bail!(
            "git worktree add: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Count uncommitted changes in `worktree_path` (staged, unstaged, untracked).
/// 0 if the path is gone or git fails — callers treat that as "clean".
pub fn worktree_change_count(worktree_path: &Path) -> usize {
    if !worktree_path.exists() {
        return 0;
    }
    command("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
        })
        .unwrap_or(0)
}

/// The repo's current HEAD commit SHA — the base a worktree branch is measured
/// against. `None` if git fails.
pub fn repo_head(repo: &Path) -> Option<String> {
    git_line(repo, &["rev-parse", "HEAD"])
}

/// The repo's current branch name (e.g. `main`); `None` when detached (`"HEAD"`)
/// or git fails. Used only for display. Works for local or remote repos.
pub fn repo_current_branch(loc: &RepoLoc) -> Option<String> {
    git_line_loc(loc, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD")
}

/// Run a git command in `dir` and return its trimmed single-line stdout on
/// success, else `None`.
fn git_line(dir: &Path, args: &[&str]) -> Option<String> {
    let out = command("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Count commits on the worktree's HEAD that are not reachable from `base` (the
/// main repo's HEAD) — i.e. unmerged work. 0 if the path is gone or git fails;
/// naturally 0 once the branch has been merged into `base`.
pub fn worktree_unmerged_count(worktree_path: &Path, base: &str) -> usize {
    if !worktree_path.exists() {
        return 0;
    }
    command("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["rev-list", "--count", &format!("{base}..HEAD")])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// Merge `branch` into whatever is checked out in `repo` (the base). On any
/// failure (e.g. conflicts) abort the merge so the repo isn't left mid-merge,
/// and return the error.
pub fn merge_worktree_branch(repo: &Path, branch: &str) -> Result<()> {
    let out = command("git")
        .arg("-C")
        .arg(repo)
        .args(["merge", "--no-edit", branch])
        .output()
        .context("running `git merge`")?;
    if !out.status.success() {
        // Leave no half-finished merge behind.
        let _ = command("git")
            .arg("-C")
            .arg(repo)
            .args(["merge", "--abort"])
            .output();
        bail!("git merge: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Delete `branch` from `repo` (force). Best-effort — only valid once the branch
/// is no longer checked out in any worktree.
pub fn delete_branch(repo: &Path, branch: &str) {
    let _ = command("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "-D", branch])
        .output();
}

/// Stage **all** changes (tracked, untracked, and deletions) and commit them at
/// `loc`. Used where committing an entire worktree's work is the intent (e.g.
/// disposing a worktree). For a reviewed, file-by-file commit use
/// [`git_status_files`] + [`git_commit_paths`]. Errors on git failure.
pub fn git_commit(loc: &RepoLoc, msg: &str) -> Result<String> {
    git_run_loc(loc, &["add", "-A"])?;
    git_run_loc(loc, &["commit", "-m", msg])
}

/// Stage `paths` (`git add -- <paths>`), relative to the repo root. A directory
/// stages everything under it. Works local or remote.
pub fn git_add_paths(loc: &RepoLoc, paths: &[String]) -> Result<()> {
    let mut args: Vec<&str> = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    git_run_loc(loc, &args).map(|_| ())
}

/// One entry from `git status --porcelain` — i.e. a file a blanket `git add -A`
/// would stage. `status` is the two-char XY code (e.g. " M", "??", "A ", "D ",
/// "R "); `path` is the path to stage; `orig` is the source path for a
/// rename/copy (display only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitChange {
    pub status: String,
    pub path: String,
    pub orig: Option<String>,
}

/// List every changed/untracked file at `loc` — exactly what `git add -A` would
/// stage — from `git status --porcelain=v1 -z`. Empty on git failure or a clean
/// tree. The `-z` (NUL-separated) form sidesteps the path quoting `git status`
/// otherwise applies to names with spaces or non-ASCII characters.
pub fn git_status_files(loc: &RepoLoc) -> Vec<GitChange> {
    git_output(loc, &["status", "--porcelain=v1", "-z"])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_status_z(&o.stdout))
        .unwrap_or_default()
}

/// Parse the NUL-separated records of `git status --porcelain -z`: each record is
/// `XY <path>`, and for a rename/copy (X or Y is 'R'/'C') the *next* NUL field is
/// the original path (the `-z` form lists the new path first, then the old).
fn parse_status_z(bytes: &[u8]) -> Vec<GitChange> {
    let text = String::from_utf8_lossy(bytes);
    let mut fields = text.split('\0').filter(|s| !s.is_empty());
    let mut out = Vec::new();
    while let Some(rec) = fields.next() {
        // A record needs the two status chars, the separator space, and at least
        // one path character.
        if rec.len() < 4 {
            continue;
        }
        let status = rec[..2].to_string();
        let path = rec[3..].to_string();
        let orig = if status.starts_with('R') || status.starts_with('C') {
            fields.next().map(str::to_string)
        } else {
            None
        };
        out.push(GitChange { status, path, orig });
    }
    out
}

/// Unified diff for a single file at `loc`, vs HEAD (staged + unstaged combined).
/// `path` is the repo-relative path from [`git_status_files`]. For a brand-new
/// staged file (empty `diff HEAD`) it falls back to the staged (`--cached`) diff.
/// Returns display-ready diff text, truncated at [`MAX_DIFF_BYTES`], or a short
/// message when there's nothing to show. Works for local and remote `loc`.
pub fn git_diff_for(loc: &RepoLoc, path: &str) -> String {
    let run = |args: &[&str]| {
        git_output(loc, args)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let mut out = run(&["diff", "HEAD", "--no-color", "--", path]).unwrap_or_default();
    if out.trim().is_empty() {
        // New/staged file: HEAD has nothing for it, so show the staged diff.
        out = run(&["diff", "--cached", "--no-color", "--", path]).unwrap_or_default();
    }
    if out.len() > MAX_DIFF_BYTES {
        out.truncate(MAX_DIFF_BYTES);
        out.push_str(&t("\n… diff truncated …\n"));
    }
    if out.trim().is_empty() {
        tf("# {path}\n\nNo diff available.\n", &[("path", path)])
    } else {
        out
    }
}

/// Stage a single file at `loc` (`git add -- <path>`).
pub fn git_stage_path(loc: &RepoLoc, path: &str) -> Result<String> {
    git_run_loc(loc, &["add", "--", path])
}

/// Unstage a single file at `loc` (`git restore --staged -- <path>`).
pub fn git_unstage_path(loc: &RepoLoc, path: &str) -> Result<String> {
    git_run_loc(loc, &["restore", "--staged", "--", path])
}

/// Discard all changes to a single file at `loc`: revert a tracked file's index
/// and worktree to HEAD, or delete it if untracked. DESTRUCTIVE — the caller must
/// confirm first.
pub fn git_discard_path(loc: &RepoLoc, path: &str) -> Result<String> {
    git_run_loc(
        loc,
        &[
            "restore",
            "--source=HEAD",
            "--staged",
            "--worktree",
            "--",
            path,
        ],
    )
    .or_else(|_| git_run_loc(loc, &["clean", "-fd", "--", path]))
}

/// Merge `branch` into whatever is checked out at `loc` (the base). `RepoLoc`
/// (local + remote) variant of [`merge_worktree_branch`]; aborts a failed merge
/// so the repo isn't left mid-merge.
pub fn merge_branch(loc: &RepoLoc, branch: &str) -> Result<String> {
    let out = git_output(loc, &["merge", "--no-edit", branch]).context("running `git merge`")?;
    if !out.status.success() {
        let _ = git_output(loc, &["merge", "--abort"]);
        bail!("git merge: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Remove the worktree at `worktree_path` (force) and prune stale entries, at
/// `loc`. `RepoLoc` (local + remote) variant of [`remove_worktree`].
pub fn remove_worktree_loc(loc: &RepoLoc, worktree_path: &str) -> Result<String> {
    let out = git_output(loc, &["worktree", "remove", "--force", worktree_path])
        .context("running `git worktree remove`")?;
    if !out.status.success() {
        bail!(
            "git worktree remove: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let _ = git_output(loc, &["worktree", "prune"]);
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Delete `branch` (force, `git branch -D`) at `loc`. `RepoLoc` (local + remote)
/// variant of [`delete_branch`].
pub fn delete_branch_loc(loc: &RepoLoc, branch: &str) -> Result<String> {
    git_run_loc(loc, &["branch", "-D", branch])
}

/// Stage exactly `paths` (their additions, modifications, and deletions, via
/// `git add -A -- <paths>`) and commit only those paths at `loc` (`git commit
/// --only`). Files outside `paths` are left untouched — even if already staged.
/// Errors on an empty selection or git failure.
pub fn git_commit_paths(loc: &RepoLoc, msg: &str, paths: &[String]) -> Result<String> {
    if paths.is_empty() {
        bail!("Select at least one file to commit");
    }
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();

    // Stage the selected paths first so untracked ones become known to git; then
    // commit only those paths, taking their working-tree state.
    let mut add = vec!["add", "-A", "--"];
    add.extend_from_slice(&refs);
    git_run_loc(loc, &add)?;

    let mut commit = vec!["commit", "--only", "-m", msg, "--"];
    commit.extend_from_slice(&refs);
    git_run_loc(loc, &commit)
}

/// Push the worktree's `branch` to `origin` (setting upstream). Errors on failure.
pub fn push_branch(worktree_path: &Path, branch: &str) -> Result<()> {
    let out = command("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["push", "-u", "origin", branch])
        .output()
        .context("running `git push`")?;
    if !out.status.success() {
        bail!("git push: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Push the branch, then open the PR-create page in a browser via `gh`.
pub fn create_pr(worktree_path: &Path, branch: &str) -> Result<()> {
    push_branch(worktree_path, branch)?;
    gh(worktree_path, &["pr", "create", "--web"])
}

/// Open the worktree branch's existing PR in a browser via `gh`.
pub fn open_pr(worktree_path: &Path) -> Result<()> {
    gh(worktree_path, &["pr", "view", "--web"])
}

/// Run `gh` in `dir`; bail with stderr on failure (e.g. not installed/authed).
fn gh(dir: &Path, args: &[&str]) -> Result<()> {
    let out = command("gh")
        .current_dir(dir)
        .args(args)
        .output()
        .context("running `gh` (is the GitHub CLI installed + authenticated?)")?;
    if !out.status.success() {
        bail!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Discard ALL changes in a worktree — uncommitted edits and its commits — by
/// hard-resetting to `base_ref` and removing untracked files. The worktree dir
/// stays (clean, at the base). Errors on git failure.
pub fn discard_worktree_changes(worktree_path: &Path, base_ref: &str) -> Result<()> {
    let run = |args: &[&str]| {
        command("git")
            .arg("-C")
            .arg(worktree_path)
            .args(args)
            .output()
            .context("running git")
    };
    let reset = run(&["reset", "--hard", base_ref])?;
    if !reset.status.success() {
        bail!(
            "git reset --hard: {}",
            String::from_utf8_lossy(&reset.stderr).trim()
        );
    }
    // Drop untracked files/dirs the agent created (best-effort).
    let _ = run(&["clean", "-fd"]);
    Ok(())
}

/// Remove a worktree (force) and prune stale entries. Best-effort.
pub fn remove_worktree(repo: &Path, worktree_path: &Path) {
    let _ = command("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .output();
    let _ = command("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output();
}

/// Maximum diff text we load into the viewer (bytes), so a giant diff can't
/// bloat the editor buffer.
const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

/// Working-tree changes for `dir`: tracked changes vs HEAD, scoped to the folder.
/// Returns display-ready unified-diff text, or a short human message when there's
/// nothing to show or `dir` isn't a git repo. Untracked/new files are not shown
/// (we can't tell agent-created files from pre-existing ones without a baseline).
pub fn git_diff(dir: &Path) -> String {
    let git = |args: &[&str]| {
        command("git")
            .arg("-C")
            .arg(dir)
            .arg("--no-pager")
            .args(args)
            .output()
    };

    // Resolve the repo root (also our "is this a git repo?" check, in one call).
    let toplevel = match git(&["rev-parse", "--show-toplevel"]) {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            return tf(
                "# {dir}\n\nNot a git repository.\n",
                &[("dir", &dir.display().to_string())],
            );
        }
    };

    // A header so it's always obvious which folder the diff is reading from, and
    // a heads-up when that folder is only a subdirectory of a larger repo.
    let mut header = tf(
        "# Changes in {dir}\n",
        &[("dir", &dir.display().to_string())],
    );
    let is_subdir = matches!(
        (dir.canonicalize(), Path::new(&toplevel).canonicalize()),
        (Ok(d), Ok(t)) if d != t
    );
    if is_subdir {
        header.push_str(&tf(
            "# (subfolder of git repo {toplevel} — showing changes under this folder only)\n",
            &[("toplevel", &toplevel)],
        ));
    }
    header.push('\n');

    // Tracked changes (staged + unstaged) vs HEAD, scoped to this folder (`-- .`)
    // so a parent repo's changes elsewhere never bleed in.
    let mut out = String::new();
    match git(&["diff", "HEAD", "--no-color", "--", "."]) {
        Ok(o) if o.status.success() => out.push_str(&String::from_utf8_lossy(&o.stdout)),
        // No commits yet (HEAD invalid): fall back to the worktree/index diff.
        _ => {
            if let Ok(o) = git(&["diff", "--no-color", "--", "."]) {
                out.push_str(&String::from_utf8_lossy(&o.stdout));
            }
        }
    }

    if out.len() > MAX_DIFF_BYTES {
        out.truncate(MAX_DIFF_BYTES);
        out.push_str(&t("\n… diff truncated …\n"));
    }
    if out.trim().is_empty() {
        format!("{header}{}", t("No changes."))
    } else {
        format!("{header}{out}")
    }
}

/// Start the tmux server *before* any pane creates a session, from this benign
/// command line. Blocking, best-effort, idempotent.
///
/// tmux forks its server from whichever client first needs one, and the server
/// keeps that client's command line (its `comm` becomes `tmux: server`, but its
/// argv does not change). If that first client is a pane's
/// `tmux new-session -A -s muxel_<project>_… `, the argv of the *shared* server
/// contains the project's name — and one server hosts every session. An agent
/// then running `pkill -f <project>` to clear its dev server matches the server,
/// SIGKILLs it, and takes down every muxel session and every agent inside them.
///
/// Starting the server from here keeps project names out of its argv, so such a
/// `pkill` can only reach a pane's own tmux *client*: the session survives, the
/// agent keeps running, and the pane reattaches.
///
/// `exit-empty off` is not optional — by default a server holding no sessions
/// exits at once, so `start-server` alone would evaporate and the next
/// `new-session` would re-fork the server with the project name back in its argv.
/// [`restore_tmux_exit_empty`] puts it back when muxel quits.
pub fn ensure_tmux_server() {
    let mut tmux = command("tmux");
    default_utf8_locale(&mut tmux);
    let _ = tmux
        .args(muxel_core::tmux::start_server_args())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn default_utf8_locale(cmd: &mut Command) {
    let var = |k: &str| std::env::var(k).ok();
    if muxel_core::locale::needs_utf8_locale(
        var("LC_ALL").as_deref(),
        var("LC_CTYPE").as_deref(),
        var("LANG").as_deref(),
    ) {
        cmd.env("LANG", muxel_core::locale::FALLBACK_UTF8_LOCALE);
    }
}

/// Undo [`ensure_tmux_server`]'s `exit-empty off` so the server goes away with
/// its last session once muxel is gone. Best-effort, fire-and-forget.
pub fn restore_tmux_exit_empty() {
    let _ = command("tmux")
        .args(muxel_core::tmux::restore_exit_empty_args())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Whether a tmux session is still alive (exact-match target, so `muxel_p_1` never
/// matches `muxel_p_12`). Fast and blocking; only called for a pane that just died.
pub fn tmux_session_exists(session: &str) -> bool {
    command("tmux")
        .args(["has-session", "-t", &format!("={session}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// --- import discovery ----------------------------------------------------------

/// Every process of this user where a project lives, for the Import window.
///
/// `ps` has no cwd column, so this runs `import::process_probe_command` — a shell
/// snippet, because it has to work the same over `ssh` as it does locally. `None`
/// means the listing couldn't be run at all (an unreachable host, a Windows one);
/// an empty list means it ran and found nothing.
pub fn list_processes(loc: &RepoLoc) -> Option<Vec<muxel_core::import::ProcessRow>> {
    let probe = muxel_core::import::process_probe_command();
    let out = match loc {
        RepoLoc::Local(_) => command("sh")
            .arg("-c")
            .arg(&probe)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?,
        RepoLoc::Remote(c) => {
            if c.host.os.is_windows() {
                // No `/proc`, no `ps` of this shape. Nothing to find, and `None`
                // would read as "couldn't reach the host".
                return Some(Vec::new());
            }
            remote_ssh_command(c, probe)
                .stdin(std::process::Stdio::null())
                .output()
                .ok()?
        }
    };
    // A non-zero exit still carries usable lines: the loop's last `readlink` can
    // fail on a process that exited mid-probe without invalidating the rest.
    Some(muxel_core::import::parse_processes(
        &String::from_utf8_lossy(&out.stdout),
    ))
}

/// Claude conversations recorded for `cwd`, as `(session_id, modified)` — its
/// project directory's `*.jsonl` transcripts, newest first.
///
/// Local projects only: the transcripts live on whichever machine ran the agent,
/// and a remote project's are on the host, where muxel would have to read them over
/// ssh. Missing directory or unreadable entries yield an empty list, not an error —
/// "no past conversations here" is the ordinary case.
pub fn claude_conversations(home: &Path, cwd: &Path) -> Vec<(String, i64)> {
    let dir = muxel_core::import::claude_project_dir(home, cwd);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, i64)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "jsonl" {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            let modified = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            Some((id, modified))
        })
        .collect();
    // Newest conversation first.
    out.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    out
}

/// What a Claude conversation was about, read from the head of its transcript.
///
/// Only the head: these files reach megabytes, the title is written early and
/// repeated, and the opening prompt is near the top — so there is nothing to gain
/// from reading the rest, and a lot of time to lose doing it for every row.
pub fn claude_conversation_summary(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    file.take(muxel_core::import::SUMMARY_SCAN_BYTES)
        .read_to_end(&mut head)
        .ok()?;
    // Lossy on purpose: the cut lands mid-file and can split a multi-byte
    // character, which is not a reason to give up on the whole summary.
    muxel_core::import::conversation_summary(&String::from_utf8_lossy(&head))
}

/// Run `tmux <args>` where a project's sessions live: on this machine, or on its
/// SSH host (reusing the host's ControlMaster). `None` for a Windows host, which
/// has no tmux.
fn tmux_at(loc: &RepoLoc, args: &[String]) -> Option<std::process::Output> {
    match loc {
        RepoLoc::Local(_) => command("tmux")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .ok(),
        RepoLoc::Remote(c) => {
            if c.host.os.is_windows() {
                return None;
            }
            let cmd = std::iter::once("tmux".to_string())
                .chain(args.iter().map(|a| ssh::sh_quote(a)))
                .collect::<Vec<_>>()
                .join(" ");
            remote_ssh_command(c, format!("{}; {cmd}", ssh::tmux_path_prelude()))
                .stdin(std::process::Stdio::null())
                .output()
                .ok()
        }
    }
}

/// Run this muxel's own `ctl` command (`exe ctl <args>`), the way an outside
/// agent does — Settings → Grok Bot's Test.
pub fn run_muxel_ctl(exe: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    command(exe)
        .arg("ctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
}

/// A tmux session's user option (`@name`), wherever the session lives. `None`
/// when it is unset, the session is gone, or tmux couldn't be reached.
pub fn tmux_option(loc: &RepoLoc, session: &str, option: &str) -> Option<String> {
    let out = tmux_at(loc, &muxel_core::tmux::show_option_args(session, option))?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !value.is_empty()).then_some(value)
}

/// Set a tmux session's user option (`@name`), wherever the session lives.
pub fn set_tmux_option(loc: &RepoLoc, session: &str, option: &str, value: &str) -> Result<()> {
    let args = muxel_core::tmux::set_option_args(session, option, value);
    let Some(out) = tmux_at(loc, &args) else {
        bail!("tmux isn't reachable for session “{session}”");
    };
    if !out.status.success() {
        bail!(
            "tmux set-option failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Kill a tmux session. Best-effort.
pub fn kill_tmux_session(session: &str) {
    let _ = command("tmux")
        .args(muxel_core::tmux::kill_session_args(session))
        .output();
}

/// Kill a tmux session on a remote host over SSH, and **confirm it is gone**
/// (reuses the host's ControlMaster, which is still alive right after the pane's
/// ssh closed).
///
/// `Ok(())` means the host answered and no longer has the session. An `Err` says
/// "could not confirm" — the session may well still be running — and the caller is
/// expected to try again rather than treat the teardown as finished. See
/// [`ssh::kill_and_confirm_command`] for why a bare `kill-session` can't answer
/// that question.
pub fn kill_remote_tmux(
    host: &RemoteHost,
    control_path: &str,
    password: Option<&str>,
    session: &str,
) -> Result<()> {
    let out = ssh_run(
        host,
        control_path,
        password,
        &ssh::kill_and_confirm_command(session),
    )?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(ssh::TMUX_STILL_ALIVE) => {
            bail!("tmux session “{session}” is still running on {}", host.name)
        }
        Some(ssh::TMUX_MISSING) => bail!("no tmux on {}", host.name),
        _ => bail!("{}", ssh_error_message(&out)),
    }
}

/// Give this machine's tmux client for `session` on `host` the window size again
/// (see [`ssh::claim_window_size_command`]), over the host's ControlMaster. Only a
/// failure to reach the host is an error: a client that never recorded itself, or
/// has since gone, simply has nothing to claim.
pub fn claim_tmux_window_size(
    host: &RemoteHost,
    control_path: &str,
    password: Option<&str>,
    session: &str,
    tag: &str,
) -> Result<()> {
    let out = ssh_run(
        host,
        control_path,
        password,
        &ssh::claim_window_size_command(session, tag),
    )?;
    // ssh reports its own failures as 255; anything else is the command's answer.
    if out.status.code() == Some(255) {
        bail!("{}", ssh_error_message(&out));
    }
    Ok(())
}

/// Fire-and-forget kill of a remote tmux session, for quit-time cleanup: the
/// spawned ssh child (reusing the warm ControlMaster) outlives muxel, so
/// quitting is never blocked on the network. Errors are ignored.
pub fn kill_remote_tmux_detached(
    host: &RemoteHost,
    control_path: &str,
    password: Option<&str>,
    session: &str,
) {
    let target = format!("={session}"); // exact-match target, as in kill_session_args
    let remote_cmd = format!(
        "{}; tmux kill-session -t {}",
        ssh::tmux_path_prelude(),
        ssh::sh_quote(&target)
    );
    let mut argv = ssh::connection_args(host, control_path);
    if password.is_none() {
        argv.push("-o".into());
        argv.push("BatchMode=yes".into());
    }
    argv.push(ssh::target(host));
    argv.push("--".into());
    argv.push(remote_cmd);
    let mut cmd;
    if host.auth == SshAuth::Password {
        cmd = command("sshpass");
        cmd.arg("-e").arg("ssh").args(&argv);
        if let Some(pw) = password {
            cmd.env("SSHPASS", pw);
        }
    } else {
        cmd = command("ssh");
        cmd.args(&argv);
    }
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Fire-and-forget kill of a local tmux session (quit-time cleanup).
pub fn kill_local_tmux_detached(session: &str) {
    let _ = command("tmux")
        .args(muxel_core::tmux::kill_session_args(session))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Open the OS file manager at (or selecting) `path`. Best-effort, cross-platform.
pub fn reveal_in_file_manager(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = command("open").arg("-R").arg(path).output();
    #[cfg(target_os = "windows")]
    let _ = command("explorer")
        .arg(format!("/select,{}", path.display()))
        .output();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        // No portable "select" on Linux — open the containing directory.
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        let _ = command("xdg-open").arg(dir).output();
    }
}

/// Whether this OS has a per-app microphone permission screen worth offering to
/// open. Linux has none (PipeWire/ALSA don't gate per app outside a Flatpak
/// portal), so callers hide the shortcut there rather than open something useless.
pub const HAS_MICROPHONE_SETTINGS: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// Open the OS microphone privacy settings. Best-effort; no-op where
/// [`HAS_MICROPHONE_SETTINGS`] is false.
pub fn open_microphone_settings() {
    #[cfg(target_os = "macos")]
    let _ = command("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
        .output();
    #[cfg(target_os = "windows")]
    let _ = command("cmd")
        .args(["/C", "start", "ms-settings:privacy-microphone"])
        .output();
}

/// Local branch names (e.g. `["main", "feature/x"]`) at `loc`.
pub fn list_branches(loc: &RepoLoc) -> Vec<String> {
    git_output(loc, &["branch", "--format=%(refname:short)"])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Check out an existing branch.
pub fn checkout_branch(loc: &RepoLoc, branch: &str) -> Result<String> {
    git_run_loc(loc, &["checkout", branch])
}

/// Create + switch to a new branch.
pub fn create_branch(loc: &RepoLoc, name: &str) -> Result<String> {
    git_run_loc(loc, &["checkout", "-b", name])
}

/// `git pull` at `loc`.
pub fn git_pull(loc: &RepoLoc) -> Result<String> {
    git_run_loc(loc, &["pull"])
}

/// `git push` at `loc`.
pub fn git_push(loc: &RepoLoc) -> Result<String> {
    git_run_loc(loc, &["push"])
}

/// `git fetch` at `loc`.
pub fn git_fetch(loc: &RepoLoc) -> Result<String> {
    git_run_loc(loc, &["fetch"])
}

/// Stash the working tree (incl. untracked) at `loc`.
pub fn git_stash(loc: &RepoLoc) -> Result<String> {
    git_run_loc(loc, &["stash", "push", "--include-untracked"])
}

/// Pop (apply + remove) the most recent stash at `loc`.
pub fn git_stash_pop(loc: &RepoLoc) -> Result<String> {
    git_run_loc(loc, &["stash", "pop"])
}

/// Drop (discard) the most recent stash at `loc` — destructive.
pub fn git_stash_drop(loc: &RepoLoc) -> Result<String> {
    git_run_loc(loc, &["stash", "drop"])
}

// ---------------------------------------------------------------------------
// Team libraries: non-interactive git runner with a time limit. Blocking.
// ---------------------------------------------------------------------------

// Nothing in the app calls these yet: the `allow(dead_code)` attributes go
// away once team libraries are wired in.

/// How library git operations are run: which `git`, with what environment,
/// and the test hooks (process counter, reaped children).
#[cfg_attr(not(test), allow(dead_code))]
pub struct GitEnv {
    pub program: OsString,
    /// Replaces the child's `PATH`.
    pub path_override: Option<OsString>,
    pub extra_env: Vec<(OsString, OsString)>,
    /// Tests: `-c` options passed after the production ones, so they win.
    #[cfg(test)]
    pub extra_config: Vec<String>,
    /// Number of git processes spawned, the `core.sshCommand` query included.
    pub spawned: Arc<AtomicUsize>,
    /// Tests: every spawned child, already waited for.
    pub reaped: Option<Arc<Mutex<Vec<Child>>>>,
    /// Directory rename used by clone and re-sync (tests inject failures).
    pub rename: RenameFn,
    /// Recursive delete used by [`remove_dir_force`] (tests inject failures).
    pub remove_dir: RemoveDirFn,
    /// Tests: called first thing in the local-changes check.
    #[cfg(test)]
    pub check_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// A directory rename: `(from, to)`.
#[cfg_attr(not(test), allow(dead_code))]
pub type RenameFn = Arc<dyn Fn(&Path, &Path) -> std::io::Result<()> + Send + Sync>;

/// A recursive directory delete.
#[cfg_attr(not(test), allow(dead_code))]
pub type RemoveDirFn = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

#[cfg_attr(not(test), allow(dead_code))]
impl GitEnv {
    /// Plain `git` from the user's `PATH`.
    pub fn production() -> Self {
        Self {
            program: OsString::from("git"),
            path_override: None,
            extra_env: Vec::new(),
            #[cfg(test)]
            extra_config: Vec::new(),
            spawned: Arc::new(AtomicUsize::new(0)),
            reaped: None,
            rename: Arc::new(|from: &Path, to: &Path| std::fs::rename(from, to)),
            remove_dir: Arc::new(|dir: &Path| std::fs::remove_dir_all(dir)),
            #[cfg(test)]
            check_hook: None,
        }
    }

    /// Tests: ignore the system and global git config (set on each child's
    /// `Command`, never with `set_var`) and keep every child for `try_wait`.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        static EMPTY_CONFIG: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        let empty = EMPTY_CONFIG.get_or_init(|| {
            let path = crate::test_support::short_temp_path();
            std::fs::write(&path, "").expect("write empty git config");
            path
        });
        Self {
            extra_env: vec![
                (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
                (
                    OsString::from("GIT_CONFIG_GLOBAL"),
                    empty.clone().into_os_string(),
                ),
            ],
            reaped: Some(Arc::new(Mutex::new(Vec::new()))),
            ..Self::production()
        }
    }

    fn has_env(&self, name: &str) -> bool {
        std::env::var_os(name).is_some() || self.extra_env.iter().any(|(k, _)| k == name)
    }

    /// The program to spawn. With `path_override`, it is looked up only in
    /// those directories (`None` = not there): on Windows, std would otherwise
    /// fall back to muxel's own `PATH` and find git anyway.
    fn resolve_program(&self) -> Option<OsString> {
        let Some(paths) = &self.path_override else {
            return Some(self.program.clone());
        };
        let exts: &[&str] = if cfg!(windows) {
            &["", ".exe", ".cmd", ".bat", ".com"]
        } else {
            &[""]
        };
        std::env::split_paths(paths).find_map(|dir| {
            exts.iter().find_map(|ext| {
                let mut name = self.program.clone();
                name.push(ext);
                let candidate = dir.join(name);
                candidate.is_file().then(|| candidate.into_os_string())
            })
        })
    }

    /// A git `Command` with the non-interactive environment; `None` when the
    /// program is not on the overridden `PATH`. `ceiling` becomes
    /// `GIT_CEILING_DIRECTORIES` so git never finds a repository above it; git
    /// ignores an entry equal to `cwd`, so it must be a strict ancestor of `cwd`.
    fn base_command(&self, cwd: Option<&Path>, ceiling: Option<&Path>) -> Option<Command> {
        let mut cmd = command(self.resolve_program()?);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        if let Some(dir) = ceiling {
            cmd.env("GIT_CEILING_DIRECTORIES", dir);
        }
        if let Some(path) = &self.path_override {
            cmd.env("PATH", path);
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .envs(self.extra_env.iter().map(|(k, v)| (k, v)))
            // Set after `extra_env` so nothing inherited can undo them. An empty
            // `GIT_ASKPASS` makes git skip every askpass program (`GIT_ASKPASS`,
            // `core.askPass`, `SSH_ASKPASS`), and ssh itself never runs `SSH_ASKPASS`.
            .env("GIT_ASKPASS", "")
            .env_remove("SSH_ASKPASS")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: `setsid` is async-signal-safe and the only call between
            // fork and exec. A new session has no controlling terminal, so git
            // and ssh cannot open /dev/tty to prompt, and the child leads its
            // own process group so the whole tree can be killed.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
        }
        Some(cmd)
    }

    /// Keep a waited-for child for the tests.
    fn reap(&self, child: Child) {
        if let Some(reaped) = &self.reaped {
            reaped.lock().unwrap_or_else(|e| e.into_inner()).push(child);
        }
    }
}

/// Whether `url` is reached over SSH: `ssh://`, `git+ssh://`,
/// `ssh+git://`, or scp-like `[user@]host:path` — no `://`, a `:` whose prefix
/// has no `/` or `\` and is not a single ASCII letter (a Windows drive).
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_ssh_url(url: &str) -> bool {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if ["ssh://", "git+ssh://", "ssh+git://"]
        .iter()
        .any(|p| lower.starts_with(p))
    {
        return true;
    }
    if url.contains("://") {
        return false;
    }
    let Some(colon) = url.find(':') else {
        return false;
    };
    let host = &url[..colon];
    if host.contains('/') || host.contains('\\') {
        return false;
    }
    let drive = host.len() == 1 && host.as_bytes()[0].is_ascii_alphabetic();
    !drive
}

/// Result of `git config --get core.sshCommand`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum SshConfigProbe {
    /// Exit 0: the user configured an SSH command.
    Set,
    /// Exit 1: the key is not defined.
    Unset,
    /// Any other exit code, a spawn error or the deadline ran out.
    Failed,
}

/// Whether to run git with `GIT_SSH_COMMAND="ssh -o BatchMode=yes"` so SSH never
/// prompts: only for an SSH URL when the user set none of `GIT_SSH_COMMAND`,
/// `GIT_SSH` or `core.sshCommand`. `probe` is called only when needed.
#[cfg_attr(not(test), allow(dead_code))]
pub fn ssh_batch_mode_needed(
    url: &str,
    env_git_ssh_command: bool,
    env_git_ssh: bool,
    probe: impl FnOnce() -> SshConfigProbe,
) -> bool {
    if !is_ssh_url(url) || env_git_ssh_command || env_git_ssh {
        return false;
    }
    probe() == SshConfigProbe::Unset
}

/// `git config --get core.sshCommand` run in `cwd` with the same environment
/// and deadline as the operation it precedes; counted in `env.spawned`.
#[cfg_attr(not(test), allow(dead_code))]
fn probe_ssh_command_config(
    env: &GitEnv,
    cwd: Option<&Path>,
    ceiling: Option<&Path>,
    deadline: Instant,
) -> SshConfigProbe {
    let Some(mut cmd) = env.base_command(cwd, ceiling) else {
        return SshConfigProbe::Failed;
    };
    cmd.args(["config", "--get", "core.sshCommand"])
        .stderr(Stdio::null());
    let Ok(child) = cmd.spawn() else {
        return SshConfigProbe::Failed;
    };
    env.spawned.fetch_add(1, Ordering::SeqCst);
    let (child, status) = wait_until(child, deadline);
    env.reap(child);
    match status.and_then(|s| s.code()) {
        Some(0) => SshConfigProbe::Set,
        Some(1) => SshConfigProbe::Unset,
        _ => SshConfigProbe::Failed,
    }
}

/// Poll `child` every 50 ms until it exits or `deadline` passes; on the
/// deadline, kill its whole process tree and wait for it. Returns the child
/// (always waited for) and its exit status, or `None` if it was killed.
#[cfg_attr(not(test), allow(dead_code))]
fn wait_until(mut child: Child, deadline: Instant) -> (Child, Option<ExitStatus>) {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (child, Some(status)),
            Ok(None) => {}
            Err(_) => break,
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
    kill_process_tree(&mut child);
    let _ = child.wait();
    (child, None)
}

/// Kill `child` and everything it started (git spawns helpers such as
/// `git-remote-https` and `ssh`).
#[cfg_attr(not(test), allow(dead_code))]
fn kill_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let _ = command("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        // SAFETY: plain syscall on a pid we spawned and have not reaped yet;
        // it leads its own process group (setsid in `base_command`).
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// Bytes of git's stderr kept for the error message.
#[cfg_attr(not(test), allow(dead_code))]
const GIT_STDERR_TAIL: usize = 4096;

/// On Windows, `core.longpaths=true` lets git create and read paths past
/// `MAX_PATH` under a deep `LIB_DIR`. The key exists only in git for Windows.
#[cfg(windows)]
const LONG_PATHS: Option<&str> = Some("core.longpaths=true");
#[cfg(not(windows))]
const LONG_PATHS: Option<&str> = None;

/// Run `git <-c…> <args>` for a library operation on `url`, blocking until it
/// ends or `deadline` passes: no stdin, no terminal prompts, no
/// credential UI, SSH in batch mode per [`ssh_batch_mode_needed`], and the
/// whole process tree killed on the deadline. `ceiling` stops git's
/// repository discovery and must be a strict ancestor of `cwd` (git ignores
/// an entry equal to `cwd`), so git never reaches a repository above
/// `LIB_DIR`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn run_git(
    env: &GitEnv,
    cwd: Option<&Path>,
    ceiling: Option<&Path>,
    url: &str,
    args: &[&OsStr],
    deadline: Instant,
) -> Result<(), GitFailure> {
    let start = Instant::now();
    let limit_secs = deadline
        .saturating_duration_since(start)
        .as_secs_f64()
        .ceil() as u64;
    let batch_mode = ssh_batch_mode_needed(
        url,
        env.has_env("GIT_SSH_COMMAND"),
        env.has_env("GIT_SSH"),
        || probe_ssh_command_config(env, cwd, ceiling, deadline),
    );
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(GitFailure::TimedOut { secs: limit_secs });
    }

    let Some(mut cmd) = env.base_command(cwd, ceiling) else {
        return Err(GitFailure::NotFound);
    };
    if batch_mode {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    // Whole seconds left, rounded up, plus one: curl's stall abort is only a
    // backstop and must never fire before the runner's own deadline.
    let low_speed_secs = remaining.as_secs_f64().ceil() as u64 + 1;
    let low_speed_time = format!("http.lowSpeedTime={low_speed_secs}");
    for opt in [
        "pull.rebase=false",
        "merge.autoStash=false",
        "rebase.autoStash=false",
        "submodule.recurse=false",
        "credential.interactive=false",
        "http.lowSpeedLimit=1000",
        low_speed_time.as_str(),
    ] {
        cmd.arg("-c").arg(opt);
    }
    if let Some(opt) = LONG_PATHS {
        cmd.arg("-c").arg(opt);
    }
    #[cfg(test)]
    for opt in &env.extra_config {
        cmd.arg("-c").arg(opt);
    }
    cmd.args(args).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(GitFailure::NotFound),
        Err(e) => {
            return Err(GitFailure::Io {
                detail: e.to_string(),
            });
        }
    };
    env.spawned.fetch_add(1, Ordering::SeqCst);

    // Drain stderr on a thread (a full pipe would block git), keeping the tail.
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut tail: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > GIT_STDERR_TAIL {
                    tail.drain(..tail.len() - GIT_STDERR_TAIL);
                }
            }
            let _ = tx.send(tail);
        });
    }

    let (child, status) = wait_until(child, deadline);
    env.reap(child);
    let Some(status) = status else {
        return Err(GitFailure::TimedOut { secs: limit_secs });
    };
    if status.success() {
        return Ok(());
    }
    // A grandchild may still hold the pipe: wait at most 1 s for the reader.
    let tail = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    let text = String::from_utf8_lossy(&tail).trim().to_string();
    let detail = if text.is_empty() {
        tf(
            "git exited with {status}",
            &[("status", &status.to_string())],
        )
    } else {
        text
    };
    Err(GitFailure::Failed { detail })
}

// ---------------------------------------------------------------------------
// Team libraries: clone, pull, re-sync and forced delete. Blocking.
// ---------------------------------------------------------------------------

/// `GitFailure::Io` from a filesystem error.
#[cfg_attr(not(test), allow(dead_code))]
fn io_failure(e: &std::io::Error) -> GitFailure {
    GitFailure::Io {
        detail: e.to_string(),
    }
}

/// `git clone --quiet --single-branch [--branch <branch>] -- <url> <target>`,
/// run from `lib_dir`; on any failure the half-made `target` is removed
/// (the one failure that deletes files: nothing usable was there before).
#[cfg_attr(not(test), allow(dead_code))]
fn clone_into(
    env: &GitEnv,
    url: &str,
    branch: &str,
    target: &Path,
    lib_dir: &Path,
    deadline: Instant,
) -> Result<(), GitFailure> {
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("clone"),
        OsStr::new("--quiet"),
        OsStr::new("--single-branch"),
    ];
    if !branch.is_empty() {
        args.push(OsStr::new("--branch"));
        args.push(OsStr::new(branch));
    }
    args.push(OsStr::new("--"));
    args.push(OsStr::new(url));
    args.push(target.as_os_str());
    let res = run_git(
        env,
        Some(lib_dir),
        Some(clone_ceiling(lib_dir)),
        url,
        &args,
        deadline,
    );
    if res.is_err() && std::fs::symlink_metadata(target).is_ok() {
        let _ = remove_dir_force(env, target);
    }
    res
}

/// The `GIT_CEILING_DIRECTORIES` entry for a clone run from `lib_dir`: its
/// parent, because git ignores a ceiling entry equal to the working
/// directory itself and would then find a repository above `LIB_DIR`.
/// `lib_dir` itself only when it has no parent.
#[cfg_attr(not(test), allow(dead_code))]
fn clone_ceiling(lib_dir: &Path) -> &Path {
    lib_dir.parent().unwrap_or(lib_dir)
}

/// 8 random hex digits for a clone's temporary siblings (`<id>.c-…`, `.r-…`,
/// `.o-…`). Short so git's paths below them stay under Windows `MAX_PATH`.
fn temp_suffix() -> String {
    let mut hex = uuid::Uuid::new_v4().simple().to_string();
    hex.truncate(8);
    hex
}

/// Clone `url` (`branch`, `""` = the remote's default) into `dest`
/// (`LIB_DIR/<id>`) through a temporary sibling `<id>.c-<suffix>` that is
/// renamed into place, so a failed clone never leaves a half-made `dest`.
/// Starts with `create_dir_all(lib_dir)`, before the `core.sshCommand` query.
#[cfg_attr(not(test), allow(dead_code))]
pub fn library_clone(
    env: &GitEnv,
    url: &str,
    branch: &str,
    dest: &Path,
    lib_dir: &Path,
    deadline: Instant,
) -> Result<(), GitFailure> {
    std::fs::create_dir_all(lib_dir).map_err(|e| io_failure(&e))?;
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = lib_dir.join(format!("{name}.c-{}", temp_suffix()));
    clone_into(env, url, branch, &tmp, lib_dir, deadline)?;
    if let Err(e) = (env.rename)(&tmp, dest) {
        let _ = remove_dir_force(env, &tmp);
        return Err(io_failure(&e));
    }
    Ok(())
}

/// `git -C <clone> pull --ff-only --quiet` and nothing else: no reset, stash,
/// clean or checkout. `url` is the configured URL, used
/// only for the SSH rule; the `core.sshCommand` query runs in the clone.
/// git never looks above the clone's parent (`LIB_DIR`), and a clone folder
/// without `.git` is reported as [`GitFailure::NotAClone`] without running
/// git: pulling there would reach a repository above `LIB_DIR`, and cloning
/// over it would delete what is in it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn library_pull(
    env: &GitEnv,
    url: &str,
    clone: &Path,
    deadline: Instant,
) -> Result<(), GitFailure> {
    if std::fs::symlink_metadata(clone.join(".git")).is_err() {
        return Err(GitFailure::NotAClone);
    }
    let args = [
        OsStr::new("-C"),
        clone.as_os_str(),
        OsStr::new("pull"),
        OsStr::new("--ff-only"),
        OsStr::new("--quiet"),
    ];
    run_git(env, Some(clone), clone.parent(), url, &args, deadline)
}

/// Re-sync `LIB_DIR/<id>` with the remote branch:
/// a full clone to `<id>.r-<suffix>`, then `<id>` → `<id>.o-<suffix>`,
/// temporary → `<id>`, and the old clone removed. `deadline` is one total
/// limit for every step (the clone time limit). On any failure the clone is
/// left as it was, except when both moving the new clone in and moving the
/// old one back fail (`ResyncRestoreFailed`), where `<id>` is missing and `<id>.o-<suffix>`
/// remains.
#[cfg_attr(not(test), allow(dead_code))]
pub fn library_resync(
    env: &GitEnv,
    url: &str,
    branch: &str,
    lib_dir: &Path,
    id: uuid::Uuid,
    deadline: Instant,
) -> Result<(), GitFailure> {
    let limit_secs = deadline
        .saturating_duration_since(Instant::now())
        .as_secs_f64()
        .ceil() as u64;
    std::fs::create_dir_all(lib_dir).map_err(|e| io_failure(&e))?;
    let suffix = temp_suffix();
    let dest = lib_dir.join(id.to_string());
    let tmp = lib_dir.join(format!("{id}.r-{suffix}"));
    let old_name = format!("{id}.o-{suffix}");
    let old = lib_dir.join(&old_name);

    match clone_into(env, url, branch, &tmp, lib_dir, deadline) {
        Ok(()) => {}
        Err(GitFailure::TimedOut { .. }) => return Err(GitFailure::TimedOut { secs: limit_secs }),
        Err(e) => return Err(e),
    }
    if Instant::now() >= deadline {
        let _ = remove_dir_force(env, &tmp);
        return Err(GitFailure::TimedOut { secs: limit_secs });
    }

    let moved_old = std::fs::symlink_metadata(&dest).is_ok();
    if moved_old && let Err(e) = (env.rename)(&dest, &old) {
        let _ = remove_dir_force(env, &tmp);
        return Err(io_failure(&e));
    }

    if let Err(replace) = (env.rename)(&tmp, &dest) {
        let _ = remove_dir_force(env, &tmp);
        if moved_old && let Err(restore) = (env.rename)(&old, &dest) {
            return Err(GitFailure::ResyncRestoreFailed {
                replace: replace.to_string(),
                restore: restore.to_string(),
                leftover: old_name,
            });
        }
        return Err(io_failure(&replace));
    }

    // A failure here is still a success: the startup cleanup removes the leftover.
    if moved_old {
        let _ = remove_dir_force(env, &old);
    }
    Ok(())
}

/// The local-changes check before a re-sync: read-only and offline (no fetch).
/// A clone without `.git` has its files counted without running git; otherwise
/// changed files come from `git status` and local commits from
/// `rev-list @{upstream}..HEAD`. Any failure or the deadline → `Unknown`.
///
/// - `clone` missing → `NoClone`.
/// - `clone` without `.git` → its files counted recursively without running
///   git (directories do not count, links are not followed); none →
///   `NoClone`; an unreadable entry or the deadline → `Unknown`.
/// - Otherwise `git status --porcelain=v1 -z --untracked-files=all` (files:
///   tracked modified or deleted and untracked one by one, ignored ones
///   excluded) and `git rev-list --count @{upstream}..HEAD` (local commits,
///   against the upstream as it is in the clone: no fetch). Any failure — git
///   missing, a git error, no upstream, detached HEAD, the deadline — →
///   `Unknown`.
///
/// git runs with the library environment of [`GitEnv::base_command`] (no
/// askpass, never above `LIB_DIR`) plus `GIT_OPTIONAL_LOCKS=0`, so `status`
/// never rewrites the index, and `core.fsmonitor=false`, so no daemon starts.
#[cfg_attr(not(test), allow(dead_code))]
pub fn library_local_changes(env: &GitEnv, clone: &Path, deadline: Instant) -> LocalChanges {
    #[cfg(test)]
    if let Some(hook) = &env.check_hook {
        hook();
    }
    if std::fs::symlink_metadata(clone).is_err() {
        return LocalChanges::NoClone;
    }
    if std::fs::symlink_metadata(clone.join(".git")).is_err() {
        return match count_files(clone, deadline) {
            Some(0) => LocalChanges::NoClone,
            Some(files) => LocalChanges::from_counts(files, 0),
            None => LocalChanges::Unknown,
        };
    }
    let status = [
        OsStr::new("status"),
        OsStr::new("--porcelain=v1"),
        OsStr::new("-z"),
        OsStr::new("--untracked-files=all"),
    ];
    let Some(files) =
        check_git_output(env, clone, &status, deadline).map(|out| status_entries(&out))
    else {
        return LocalChanges::Unknown;
    };
    let rev_list = [
        OsStr::new("rev-list"),
        OsStr::new("--count"),
        OsStr::new("@{upstream}..HEAD"),
    ];
    let commits = check_git_output(env, clone, &rev_list, deadline)
        .and_then(|out| String::from_utf8_lossy(&out).trim().parse::<usize>().ok());
    match commits {
        Some(commits) => LocalChanges::from_counts(files, commits),
        None => LocalChanges::Unknown,
    }
}

/// Files under `dir`, recursively, without following links; `None` on a
/// read error or once `deadline` has passed.
#[cfg_attr(not(test), allow(dead_code))]
fn count_files(dir: &Path, deadline: Instant) -> Option<usize> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut files = 0;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_dir() {
            files += count_files(&entry.path(), deadline)?;
        } else {
            files += 1;
        }
    }
    Some(files)
}

/// Entries of `git status --porcelain=v1 -z`: NUL-terminated `XY <path>`
/// fields, plus the source path as one more field after a rename or copy
/// (`R`/`C`), which is not another entry.
#[cfg_attr(not(test), allow(dead_code))]
fn status_entries(out: &[u8]) -> usize {
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut entries = 0;
    while let Some(field) = fields.next() {
        entries += 1;
        if field.len() >= 2 && field[..2].iter().any(|c| matches!(c, b'R' | b'C')) {
            fields.next();
        }
    }
    entries
}

/// Run `git -c core.fsmonitor=false <args>` in `clone` for the
/// local-changes check and return its stdout; `None` if git is missing,
/// fails, or `deadline` passes (its process tree is then killed). Counted in
/// `env.spawned`. The deadline already passed → `None` without running git.
#[cfg_attr(not(test), allow(dead_code))]
fn check_git_output(
    env: &GitEnv,
    clone: &Path,
    args: &[&OsStr],
    deadline: Instant,
) -> Option<Vec<u8>> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut cmd = env.base_command(Some(clone), clone.parent())?;
    cmd.env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "core.fsmonitor=false"]);
    if let Some(opt) = LONG_PATHS {
        cmd.arg("-c").arg(opt);
    }
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    env.spawned.fetch_add(1, Ordering::SeqCst);
    // Drain stdout on a thread so a large status never blocks git.
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut out = Vec::new();
            let ok = stdout.read_to_end(&mut out).is_ok();
            let _ = tx.send(ok.then_some(out));
        });
    }
    let (child, status) = wait_until(child, deadline);
    env.reap(child);
    if !status?.success() {
        return None;
    }
    let left = deadline.saturating_duration_since(Instant::now());
    rx.recv_timeout(left.max(Duration::from_millis(100)))
        .ok()
        .flatten()
}

/// Delete `dir` with `env.remove_dir`; if that fails, clear the read-only
/// attribute of everything under it and try exactly once more.
#[cfg_attr(not(test), allow(dead_code))]
pub fn remove_dir_force(env: &GitEnv, dir: &Path) -> std::io::Result<()> {
    if (env.remove_dir)(dir).is_ok() {
        return Ok(());
    }
    clear_read_only(dir);
    (env.remove_dir)(dir)
}

/// Make `path` and, for a directory, everything under it writable. Symlinks
/// are not followed. Best effort: errors are ignored.
#[cfg_attr(not(test), allow(dead_code))]
fn clear_read_only(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mut perms = meta.permissions();
    #[cfg(windows)]
    {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(perms.mode() | 0o200);
    }
    let _ = std::fs::set_permissions(path, perms);
    if meta.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for entry in entries.flatten() {
            clear_read_only(&entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_failure_vs_command_failure() {
        // ssh's own transport/auth error (255) is a connection failure.
        assert!(is_ssh_transport_failure(SshAuth::Agent, Some(255)));
        assert!(is_ssh_transport_failure(SshAuth::Password, Some(255)));
        // sshpass auth failure (e.g. wrong password = 5) is too.
        assert!(is_ssh_transport_failure(SshAuth::Password, Some(5)));
        // A remote command exiting non-zero (e.g. `test -d` = 1 for a missing
        // dir) is NOT a connection failure — it's a path problem.
        assert!(!is_ssh_transport_failure(SshAuth::Key, Some(1)));
        assert!(!is_ssh_transport_failure(SshAuth::Password, Some(1)));
        // sshpass codes 2..=6 only apply to password auth, not key/agent.
        assert!(!is_ssh_transport_failure(SshAuth::Key, Some(5)));
    }

    #[test]
    fn worktree_create_and_remove() {
        let repo = std::env::temp_dir().join("muxel-it-repo");
        let worktree = std::env::temp_dir().join("muxel-it-worktree");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&worktree);
        std::fs::create_dir_all(&repo).unwrap();

        let git = |args: &[&str]| {
            command("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@muxel"]);
        git(&["config", "user.name", "muxel test"]);
        std::fs::write(repo.join("file.txt"), "hello").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);

        assert!(is_git_repo(&repo));
        assert!(!is_git_repo(&std::env::temp_dir()));

        create_worktree(&repo, &worktree, "muxel/test").expect("create worktree");
        assert!(
            worktree.join("file.txt").exists(),
            "worktree should be checked out"
        );

        remove_worktree(&repo, &worktree);
        assert!(!worktree.exists(), "worktree should be removed");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn unmerged_count_and_merge() {
        let repo = std::env::temp_dir().join("muxel-it-unmerged");
        let worktree = std::env::temp_dir().join("muxel-it-unmerged-wt");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&worktree);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            command("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap()
        };
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@muxel"]);
        git(&repo, &["config", "user.name", "muxel test"]);
        std::fs::write(repo.join("file.txt"), "hello").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);

        create_worktree(&repo, &worktree, "muxel/test").expect("create worktree");
        let base = repo_head(&repo).expect("repo head");
        // A fresh worktree has nothing ahead of base.
        assert_eq!(worktree_unmerged_count(&worktree, &base), 0);

        // Commit inside the worktree → one unmerged commit, but a clean tree.
        std::fs::write(worktree.join("feature.txt"), "work").unwrap();
        git(&worktree, &["add", "."]);
        git(&worktree, &["commit", "-q", "-m", "feature"]);
        assert_eq!(worktree_change_count(&worktree), 0, "tree should be clean");
        assert_eq!(worktree_unmerged_count(&worktree, &base), 1);

        // Merge it into the repo's base branch → the work lands there.
        merge_worktree_branch(&repo, "muxel/test").expect("merge");
        assert!(
            repo.join("feature.txt").exists(),
            "merged file should appear in the base repo"
        );
        // After merging, nothing is unmerged anymore.
        let base2 = repo_head(&repo).expect("repo head");
        assert_eq!(worktree_unmerged_count(&worktree, &base2), 0);

        // Cleanup: remove the worktree, then the (now merged) branch.
        remove_worktree(&repo, &worktree);
        delete_branch(&repo, "muxel/test");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn git_diff_shows_tracked_changes_only() {
        let repo = std::env::temp_dir().join("muxel-it-diff");
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            command("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@muxel"]);
        git(&["config", "user.name", "muxel test"]);
        std::fs::write(repo.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/insub.txt"), "a\nb\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);

        // Modify tracked files (root + subfolder) and create an untracked file.
        std::fs::write(repo.join("tracked.txt"), "one\nCHANGED\n").unwrap();
        std::fs::write(repo.join("sub/insub.txt"), "a\nSUBCHANGED\n").unwrap();
        std::fs::write(repo.join("untracked_new.txt"), "nope\n").unwrap();

        let diff = git_diff(&repo);
        // The header names the exact folder being diffed.
        assert!(
            diff.contains(&repo.display().to_string()),
            "header shows the folder path:\n{diff}"
        );
        assert!(
            diff.contains("tracked.txt"),
            "tracked change shown:\n{diff}"
        );
        assert!(diff.contains("CHANGED"), "modified line shown:\n{diff}");
        // Untracked files are NOT listed.
        assert!(
            !diff.contains("untracked_new.txt") && !diff.contains("nope"),
            "untracked file must be excluded:\n{diff}"
        );

        // Diffing the subfolder is scoped to it: flags the parent repo, shows the
        // subfolder's change, and does NOT include the parent's tracked.txt change.
        let sub_diff = git_diff(&repo.join("sub"));
        assert!(
            sub_diff.contains("subfolder of git repo"),
            "subfolder note shown:\n{sub_diff}"
        );
        assert!(
            sub_diff.contains("SUBCHANGED"),
            "subfolder change shown:\n{sub_diff}"
        );
        assert!(
            !sub_diff.contains("tracked.txt"),
            "parent's change must be scoped out:\n{sub_diff}"
        );

        // A non-repo directory reports as such (and still names the folder).
        let plain = std::env::temp_dir().join("muxel-it-not-a-repo");
        let _ = std::fs::remove_dir_all(&plain);
        std::fs::create_dir_all(&plain).unwrap();
        assert!(
            git_diff(&plain).contains("Not a git repository."),
            "non-repo message"
        );

        let _ = std::fs::remove_dir_all(&plain);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn ensure_memory_file_local_creates_and_gitignores() {
        let root = std::env::temp_dir().join("muxel-it-memory");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Pre-existing .gitignore without our entry.
        std::fs::write(root.join(".gitignore"), "target\n").unwrap();

        ensure_memory_file(&RepoLoc::Local(root.clone())).expect("ensure memory");

        let mem = root.join(MEMORY_DIR).join(MEMORY_FILE);
        assert!(mem.exists(), "MEMORY.md should be created");
        let gi = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gi.lines().any(|l| l.trim() == ".muxel/"), "gitignored");
        assert!(gi.contains("target"), "kept existing entries");

        // Idempotent: a second call doesn't duplicate the gitignore line or clobber.
        std::fs::write(&mem, "kept user notes").unwrap();
        ensure_memory_file(&RepoLoc::Local(root.clone())).expect("ensure memory again");
        let gi2 = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(gi2.matches(".muxel/").count(), 1, "no duplicate ignore");
        assert_eq!(std::fs::read_to_string(&mem).unwrap(), "kept user notes");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn local_layout_push_fetch_roundtrip() {
        let root = std::env::temp_dir().join("muxel-it-local-layout");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let loc = RepoLoc::Local(root.clone());

        // Nothing synced yet.
        assert!(fetch_remote_layout(&loc).is_none());

        // Push writes <root>/.muxel/workspace.json and git-ignores .muxel/.
        push_remote_layout(&loc, "{\"v\":1}").expect("push local layout");
        assert_eq!(fetch_remote_layout(&loc).as_deref(), Some("{\"v\":1}"));
        let gi = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(
            gi.lines().any(|l| l.trim() == ".muxel/"),
            "gitignored: {gi}"
        );

        // A second push backs up the previous copy to workspace.bak.json.
        push_remote_layout(&loc, "{\"v\":2}").expect("push again");
        assert_eq!(fetch_remote_layout(&loc).as_deref(), Some("{\"v\":2}"));
        let bak =
            std::fs::read_to_string(root.join(MEMORY_DIR).join("workspace.bak.json")).unwrap();
        assert_eq!(bak, "{\"v\":1}", "previous copy backed up");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_push_prep_cmd_backs_up_and_gitignores() {
        // `sh_quote` leaves quote-safe tokens (paths, `.muxel/`) bare.
        let cmd = remote_push_prep_cmd(RemoteOs::Unix, "/srv/app/");
        // cd into the (trailing-slash-trimmed) root and create the dir.
        assert!(cmd.contains("cd /srv/app "), "cd into root: {cmd}");
        assert!(cmd.contains("mkdir -p .muxel "), "make .muxel: {cmd}");
        // Back up the previous layout before it's overwritten.
        assert!(
            cmd.contains("cp -f .muxel/workspace.json .muxel/workspace.bak.json"),
            "backup prior layout: {cmd}"
        );
        // Idempotently git-ignore .muxel/.
        assert!(
            cmd.contains("grep -qxF .muxel/ .gitignore"),
            "gitignore: {cmd}"
        );
        assert!(cmd.contains(">> .gitignore"), "appends ignore: {cmd}");
    }

    /// The same preparation on a Windows host goes out as one opaque encoded
    /// command, so the far side's `DefaultShell` cannot reinterpret any of it.
    #[test]
    fn remote_push_prep_cmd_is_encoded_for_windows() {
        let cmd = remote_push_prep_cmd(RemoteOs::Windows, "C:/src/app/");
        assert!(
            cmd.starts_with("powershell.exe -NoProfile -NonInteractive -EncodedCommand "),
            "{cmd}"
        );
        let payload = cmd.rsplit(' ').next().unwrap();
        assert!(
            payload
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='),
            "payload is not bare base64: {payload}"
        );
    }

    #[test]
    fn remote_layout_abs_joins_under_dot_muxel() {
        assert_eq!(
            remote_layout_abs("/home/me/proj/"),
            "/home/me/proj/.muxel/workspace.json"
        );
    }

    #[test]
    fn parse_status_z_handles_untracked_modified_and_rename() {
        // -z records: " M a.txt", "?? b c.txt" (space in name, unquoted),
        // and a staged rename "R  new.txt\0old.txt" (new path first, then old).
        let raw = b" M a.txt\0?? b c.txt\0R  new.txt\0old.txt\0";
        let got = parse_status_z(raw);
        assert_eq!(
            got,
            vec![
                GitChange {
                    status: " M".into(),
                    path: "a.txt".into(),
                    orig: None,
                },
                GitChange {
                    status: "??".into(),
                    path: "b c.txt".into(),
                    orig: None,
                },
                GitChange {
                    status: "R ".into(),
                    path: "new.txt".into(),
                    orig: Some("old.txt".into()),
                },
            ]
        );
    }

    #[test]
    fn status_files_lists_all_changes_and_commit_paths_is_selective() {
        let repo = std::env::temp_dir().join("muxel-it-commit-paths");
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            command("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@muxel"]);
        git(&["config", "user.name", "muxel test"]);
        std::fs::write(repo.join("keep.txt"), "v1\n").unwrap();
        std::fs::write(repo.join("gone.txt"), "bye\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);

        // Modify a tracked file, delete a tracked file, add two untracked files.
        std::fs::write(repo.join("keep.txt"), "v2\n").unwrap();
        std::fs::remove_file(repo.join("gone.txt")).unwrap();
        std::fs::write(repo.join("wanted.txt"), "new\n").unwrap();
        std::fs::write(repo.join("extra.txt"), "junk\n").unwrap();

        let loc = RepoLoc::Local(repo.clone());

        // status lists every changed + untracked file.
        let listed: std::collections::BTreeSet<String> =
            git_status_files(&loc).into_iter().map(|c| c.path).collect();
        assert_eq!(
            listed,
            ["extra.txt", "gone.txt", "keep.txt", "wanted.txt"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );

        // Commit only a subset (modify + deletion + one new file), NOT extra.txt.
        git_commit_paths(
            &loc,
            "selective",
            &["keep.txt".into(), "gone.txt".into(), "wanted.txt".into()],
        )
        .expect("selective commit");

        // The unselected untracked file is all that remains uncommitted.
        let remaining: Vec<String> = git_status_files(&loc).into_iter().map(|c| c.path).collect();
        assert_eq!(remaining, vec!["extra.txt".to_string()]);

        // HEAD recorded exactly the three selected changes.
        let show = command("git")
            .arg("-C")
            .arg(&repo)
            .args(["show", "--name-status", "--format=", "HEAD"])
            .output()
            .unwrap();
        let names = String::from_utf8_lossy(&show.stdout);
        assert!(names.contains("keep.txt"), "modify committed:\n{names}");
        assert!(names.contains("gone.txt"), "deletion committed:\n{names}");
        assert!(names.contains("wanted.txt"), "new file committed:\n{names}");
        assert!(
            !names.contains("extra.txt"),
            "unselected file must not be committed:\n{names}"
        );

        // An empty selection is rejected rather than producing an empty commit.
        assert!(git_commit_paths(&loc, "noop", &[]).is_err());

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn stage_unstage_discard_and_diff_single_file() {
        let repo = std::env::temp_dir().join("muxel-it-per-file-ops");
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            command("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@muxel"]);
        git(&["config", "user.name", "muxel test"]);
        // Keep line endings deterministic: Windows git defaults to
        // core.autocrlf=true, which would restore the file as "one\r\n".
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        let loc = RepoLoc::Local(repo.clone());

        // Modify the file: its single-file diff shows the change.
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        let diff = git_diff_for(&loc, "a.txt");
        assert!(diff.contains("-one"), "diff shows removed line:\n{diff}");
        assert!(diff.contains("+two"), "diff shows added line:\n{diff}");

        // Stage → X column M (staged-modified, worktree clean).
        git_stage_path(&loc, "a.txt").expect("stage");
        assert_eq!(git_status_files(&loc)[0].status, "M ");

        // Unstage → back to worktree-modified.
        git_unstage_path(&loc, "a.txt").expect("unstage");
        assert_eq!(git_status_files(&loc)[0].status, " M");

        // Discard reverts a tracked file to HEAD: clean tree, original content.
        git_discard_path(&loc, "a.txt").expect("discard tracked");
        assert!(
            git_status_files(&loc).is_empty(),
            "tree clean after discard"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            "one\n"
        );

        // Discard also removes an untracked file.
        std::fs::write(repo.join("junk.txt"), "x\n").unwrap();
        git_discard_path(&loc, "junk.txt").expect("discard untracked");
        assert!(!repo.join("junk.txt").exists(), "untracked file removed");

        let _ = std::fs::remove_dir_all(&repo);
    }

    // ---- Team libraries: non-interactive git runner ----

    use crate::test_support::{TestServer, askpass_script};
    use std::ffi::OsStr;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn os_args<'a>(args: &[&'a str]) -> Vec<&'a OsStr> {
        args.iter().map(|s| OsStr::new(*s)).collect()
    }

    fn lib_temp_dir() -> PathBuf {
        let dir = crate::test_support::short_temp_path();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Every child the runner spawned has been waited for.
    fn assert_all_reaped(env: &GitEnv) {
        let reaped = env
            .reaped
            .as_ref()
            .expect("for_tests keeps reaped children");
        let mut reaped = reaped.lock().unwrap();
        assert!(!reaped.is_empty(), "at least one child was spawned");
        for child in reaped.iter_mut() {
            assert!(
                matches!(child.try_wait(), Ok(Some(_))),
                "child still running"
            );
        }
    }

    #[test]
    fn library_git_missing_from_path_is_not_found() {
        let empty = lib_temp_dir();
        let mut env = GitEnv::for_tests();
        env.path_override = Some(empty.clone().into_os_string());
        let deadline = Instant::now() + Duration::from_secs(10);
        let res = run_git(
            &env,
            None,
            None,
            "https://example.invalid/r.git",
            &os_args(&["--version"]),
            deadline,
        );
        assert_eq!(res, Err(GitFailure::NotFound));
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn library_git_http_401_fails_fast_and_child_is_reaped() {
        let server = TestServer::unauthorized();
        let url = server.url("team-lib.git");
        let env = GitEnv::for_tests();
        let start = Instant::now();
        let deadline = start + Duration::from_secs(120);
        let res = run_git(
            &env,
            None,
            None,
            &url,
            &os_args(&["ls-remote", "--", &url]),
            deadline,
        );
        assert!(
            matches!(res, Err(GitFailure::Failed { ref detail }) if !detail.is_empty()),
            "expected a git failure with a message, got {res:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(120));
        let reaped = env.reaped.as_ref().unwrap();
        assert!(matches!(reaped.lock().unwrap()[0].try_wait(), Ok(Some(_))));
        assert_eq!(env.spawned.load(Ordering::SeqCst), 1);
    }

    /// `credential.interactive` is turned back on to stand for a git older than
    /// 2.46, so only `base_command`'s askpass environment can stop the prompt.
    /// git also asks HTTP credentials through `SSH_ASKPASS`, so all three
    /// sources are exercised over HTTP.
    #[test]
    fn library_git_never_runs_an_askpass_program() {
        let server = TestServer::unauthorized();
        let url = server.url("team-lib.git");
        let dir = lib_temp_dir();
        let mut ran = Vec::new();
        for source in ["GIT_ASKPASS", "core.askPass", "SSH_ASKPASS"] {
            // Positive control: unprotected git runs this source's script.
            let plain_marker = dir.join(format!("{source}.plain.called"));
            let plain_script = askpass_script(&dir, &plain_marker);
            let mut plain = command("git");
            plain
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", dir.join("empty.gitconfig"))
                .env("GIT_TERMINAL_PROMPT", "0")
                .env_remove("GIT_ASKPASS")
                .env_remove("SSH_ASKPASS")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .args(["-c", "credential.interactive=true"]);
            match source {
                "core.askPass" => {
                    plain
                        .arg("-c")
                        .arg(format!("core.askPass={}", plain_script.display()));
                }
                _ => {
                    plain.env(source, &plain_script);
                }
            }
            if source == "SSH_ASKPASS" {
                plain.env("SSH_ASKPASS_REQUIRE", "force");
            }
            let status = plain.args(["ls-remote", "--", url.as_str()]).status();
            assert!(status.is_ok(), "{source}: plain git did not start");
            assert!(
                plain_marker.exists(),
                "{source}: plain git did not run the askpass program, so the check below would prove nothing"
            );

            let marker = dir.join(format!("{source}.called"));
            let script = askpass_script(&dir, &marker);
            let mut env = GitEnv::for_tests();
            let config_arg = format!("core.askPass={}", script.display());
            let mut args = vec!["-c", "credential.interactive=true"];
            match source {
                "core.askPass" => args.extend(["-c", config_arg.as_str()]),
                _ => env
                    .extra_env
                    .push((OsString::from(source), script.clone().into_os_string())),
            }
            if source == "SSH_ASKPASS" {
                env.extra_env.push((
                    OsString::from("SSH_ASKPASS_REQUIRE"),
                    OsString::from("force"),
                ));
            }
            args.extend(["ls-remote", "--", url.as_str()]);
            let deadline = Instant::now() + Duration::from_secs(60);
            let res = run_git(&env, None, None, &url, &os_args(&args), deadline);
            assert!(
                matches!(res, Err(GitFailure::Failed { .. })),
                "{source}: {res:?}"
            );
            if marker.exists() {
                ran.push(source);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ran.is_empty(), "git ran the askpass program of {ran:?}");
    }

    #[test]
    fn library_git_timeout_kills_silent_server_operation() {
        let server = TestServer::silent();
        let url = server.url("team-lib.git");
        let env = GitEnv::for_tests();
        let start = Instant::now();
        let deadline = start + Duration::from_secs(2);
        let res = run_git(
            &env,
            None,
            None,
            &url,
            &os_args(&["ls-remote", "--", &url]),
            deadline,
        );
        let elapsed = start.elapsed();
        assert_eq!(res, Err(GitFailure::TimedOut { secs: 2 }));
        assert!(
            elapsed >= Duration::from_secs(2),
            "returned before the deadline: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(15), "took {elapsed:?}");
        assert_all_reaped(&env);
        assert_eq!(env.spawned.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn library_git_file_url_spawns_one_process() {
        let repo = lib_temp_dir();
        let init = command("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&repo)
            .status()
            .unwrap();
        assert!(init.success());
        let path = repo.to_string_lossy().replace('\\', "/");
        let url = format!("file:///{}", path.trim_start_matches('/'));
        let env = GitEnv::for_tests();
        let deadline = Instant::now() + Duration::from_secs(60);
        let res = run_git(
            &env,
            None,
            None,
            &url,
            &os_args(&["ls-remote", "--", &url]),
            deadline,
        );
        assert_eq!(res, Ok(()));
        assert_eq!(env.spawned.load(Ordering::SeqCst), 1);
        assert_all_reaped(&env);

        let missing = format!("{url}-missing");
        let res = run_git(
            &env,
            None,
            None,
            &missing,
            &os_args(&["ls-remote", "--", &missing]),
            deadline,
        );
        assert!(
            matches!(res, Err(GitFailure::Failed { ref detail }) if !detail.is_empty()),
            "{res:?}"
        );
        assert_eq!(env.spawned.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn library_git_production_env_runs_plain_git() {
        let env = GitEnv::production();
        assert_eq!(env.program, OsStr::new("git"));
        assert!(env.path_override.is_none());
        assert!(env.extra_env.is_empty());
        assert!(env.reaped.is_none());
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn library_git_is_ssh_url_cases() {
        assert!(is_ssh_url("git@github.com:org/r.git"));
        assert!(is_ssh_url("ssh://git@h/r"));
        assert!(is_ssh_url("SSH://git@h/r"));
        assert!(is_ssh_url("git+ssh://h/r"));
        assert!(is_ssh_url("ssh+git://h/r"));
        assert!(is_ssh_url("  git@h:r  "));
        assert!(is_ssh_url("host:r"));
        assert!(!is_ssh_url("https://h/r"));
        assert!(!is_ssh_url("file:///C:/r"));
        assert!(!is_ssh_url("C:/r"));
        assert!(!is_ssh_url("C:\\r"));
        assert!(!is_ssh_url("./a:b"));
        assert!(!is_ssh_url("/srv/repos/r"));
        assert!(!is_ssh_url(""));
    }

    #[test]
    fn library_git_ssh_batch_mode_needed_rule() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let probe = |answer: SshConfigProbe| {
            let calls = &calls;
            move || {
                calls.set(calls.get() + 1);
                answer
            }
        };
        assert!(!ssh_batch_mode_needed(
            "https://h/r",
            false,
            false,
            probe(SshConfigProbe::Unset)
        ));
        assert_eq!(calls.get(), 0);
        assert!(ssh_batch_mode_needed(
            "git@h:r",
            false,
            false,
            probe(SshConfigProbe::Unset)
        ));
        assert_eq!(calls.get(), 1);
        assert!(!ssh_batch_mode_needed(
            "git@h:r",
            false,
            false,
            probe(SshConfigProbe::Set)
        ));
        assert!(!ssh_batch_mode_needed(
            "git@h:r",
            false,
            false,
            probe(SshConfigProbe::Failed)
        ));
        assert_eq!(calls.get(), 3);
        assert!(!ssh_batch_mode_needed(
            "ssh://h/r",
            true,
            false,
            probe(SshConfigProbe::Unset)
        ));
        assert!(!ssh_batch_mode_needed(
            "ssh://h/r",
            false,
            true,
            probe(SshConfigProbe::Unset)
        ));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn library_git_ssh_config_probe_reads_core_ssh_command() {
        let env = GitEnv::for_tests();
        let deadline = Instant::now() + Duration::from_secs(30);
        let plain = lib_temp_dir();
        assert_eq!(
            probe_ssh_command_config(&env, Some(&plain), None, deadline),
            SshConfigProbe::Unset
        );
        let repo = lib_temp_dir();
        assert!(
            command("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            command("git")
                .arg("-C")
                .arg(&repo)
                .args(["config", "core.sshCommand", "ssh -i k"])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            probe_ssh_command_config(&env, Some(&repo), None, deadline),
            SshConfigProbe::Set
        );
        assert_eq!(
            probe_ssh_command_config(&env, Some(&plain), None, Instant::now()),
            SshConfigProbe::Failed
        );
        let missing = plain.join("does-not-exist");
        assert_eq!(
            probe_ssh_command_config(&env, Some(&missing), None, deadline),
            SshConfigProbe::Failed
        );
        let _ = std::fs::remove_dir_all(&plain);
        let _ = std::fs::remove_dir_all(&repo);
    }

    // ---- Team libraries: clone / pull / re-sync / forced delete ----

    use crate::test_support::{TestRepo, file_url, git_out};
    use std::collections::BTreeMap;

    const LIB_FILE: &str = "muxel-library.toml";
    const FILE_V1: &str = "[[snippets]]\nname = \"A\"\ntext = \"one\"\n";
    const FILE_V2: &str = "[[snippets]]\nname = \"A\"\ntext = \"one\"\n\n[[snippets]]\nname = \"New\"\ntext = \"two\"\n";

    fn lib_deadline() -> Instant {
        Instant::now() + Duration::from_secs(120)
    }

    fn lib_remote() -> TestRepo {
        let repo = TestRepo::init();
        repo.commit(&[(LIB_FILE, FILE_V1), ("README.md", "readme\n")], "init");
        repo
    }

    fn lib_cloned(repo: &TestRepo) -> (PathBuf, uuid::Uuid, PathBuf) {
        let lib_dir = lib_temp_dir();
        let id = uuid::Uuid::new_v4();
        let dest = lib_dir.join(id.to_string());
        let env = GitEnv::for_tests();
        let res = library_clone(
            &env,
            &file_url(repo.path()),
            "",
            &dest,
            &lib_dir,
            lib_deadline(),
        );
        assert_eq!(res, Ok(()));
        (lib_dir, id, dest)
    }

    fn lib_head(dir: &Path) -> String {
        git_out(dir, &["log", "-1", "--format=%H"])
    }

    fn lib_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn lib_snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_name() == ".git" {
                    continue;
                }
                if entry.file_type().unwrap().is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_string_lossy();
                    out.insert(rel.replace('\\', "/"), std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    fn assert_no_temp_entries(lib_dir: &Path) {
        for name in lib_entries(lib_dir) {
            assert!(
                !name.contains(".r-") && !name.contains(".o-") && !name.contains(".c-"),
                "leftover entry {name}"
            );
        }
    }

    /// A rename hook that fails when the source's name contains any of `bad`.
    fn lib_failing_rename(bad: &'static [&'static str]) -> RenameFn {
        Arc::new(move |src: &Path, dst: &Path| {
            let name = src.file_name().unwrap().to_string_lossy().into_owned();
            if bad.iter().any(|b| name.contains(b)) {
                Err(std::io::Error::other(format!(
                    "injected rename failure: {name}"
                )))
            } else {
                std::fs::rename(src, dst)
            }
        })
    }

    #[test]
    fn library_clone_default_branch_copies_file() {
        let repo = lib_remote();
        let (lib_dir, id, dest) = lib_cloned(&repo);
        assert_eq!(
            std::fs::read_to_string(dest.join(LIB_FILE)).unwrap(),
            FILE_V1
        );
        assert_eq!(lib_head(&dest), repo.git(&["rev-parse", "HEAD"]));
        assert_eq!(lib_entries(&lib_dir), vec![id.to_string()]);
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_clone_checks_out_the_named_branch() {
        let repo = lib_remote();
        repo.git(&["checkout", "-q", "-b", "team"]);
        let team_file = "[[snippets]]\nname = \"Team\"\ntext = \"t\"\n";
        repo.commit(&[(LIB_FILE, team_file)], "team");
        repo.git(&["checkout", "-q", "main"]);
        let lib_dir = lib_temp_dir();
        let dest = lib_dir.join(uuid::Uuid::new_v4().to_string());
        let env = GitEnv::for_tests();
        let res = library_clone(
            &env,
            &file_url(repo.path()),
            "team",
            &dest,
            &lib_dir,
            lib_deadline(),
        );
        assert_eq!(res, Ok(()));
        assert_eq!(
            std::fs::read_to_string(dest.join(LIB_FILE)).unwrap(),
            team_file
        );
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_clone_missing_branch_leaves_nothing() {
        let repo = lib_remote();
        let lib_dir = lib_temp_dir();
        let dest = lib_dir.join(uuid::Uuid::new_v4().to_string());
        let env = GitEnv::for_tests();
        let res = library_clone(
            &env,
            &file_url(repo.path()),
            "no-such-branch",
            &dest,
            &lib_dir,
            lib_deadline(),
        );
        assert!(
            matches!(res, Err(GitFailure::Failed { ref detail }) if !detail.is_empty()),
            "{res:?}"
        );
        assert!(
            lib_entries(&lib_dir).is_empty(),
            "{:?}",
            lib_entries(&lib_dir)
        );
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_clone_creates_missing_lib_dir() {
        let repo = lib_remote();
        let parent = lib_temp_dir();
        let lib_dir = parent.join("data").join("libraries");
        assert!(!lib_dir.exists());
        let dest = lib_dir.join(uuid::Uuid::new_v4().to_string());
        let env = GitEnv::for_tests();
        let res = library_clone(
            &env,
            &file_url(repo.path()),
            "",
            &dest,
            &lib_dir,
            lib_deadline(),
        );
        assert_eq!(res, Ok(()));
        assert_eq!(
            std::fs::read_to_string(dest.join(LIB_FILE)).unwrap(),
            FILE_V1
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn library_resync_creates_missing_lib_dir_and_clones() {
        let repo = lib_remote();
        let parent = lib_temp_dir();
        let lib_dir = parent.join("libraries");
        let id = uuid::Uuid::new_v4();
        let env = GitEnv::for_tests();
        let res = library_resync(
            &env,
            &file_url(repo.path()),
            "",
            &lib_dir,
            id,
            lib_deadline(),
        );
        assert_eq!(res, Ok(()));
        let dest = lib_dir.join(id.to_string());
        assert_eq!(lib_head(&dest), repo.git(&["rev-parse", "HEAD"]));
        assert_eq!(lib_entries(&lib_dir), vec![id.to_string()]);
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn library_git_ceiling_keeps_git_inside_lib_dir() {
        let outer = TestRepo::init();
        outer.commit(&[("outer.txt", "outer\n")], "outer");
        let lib_dir = outer.path().join("libs");
        let folder = lib_dir.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&folder).unwrap();
        let url = file_url(outer.path());
        let args = os_args(&["rev-parse", "--git-dir"]);
        let env = GitEnv::for_tests();

        let unbounded = run_git(&env, Some(&folder), None, &url, &args, lib_deadline());
        assert_eq!(unbounded, Ok(()), "the outer repository is found");
        let bounded = run_git(
            &env,
            Some(&folder),
            Some(&lib_dir),
            &url,
            &args,
            lib_deadline(),
        );
        assert!(
            matches!(bounded, Err(GitFailure::Failed { .. })),
            "{bounded:?}"
        );
    }

    /// `LIB_DIR` sits inside a repository that sets `core.sshCommand`.
    #[test]
    fn library_git_clone_probe_does_not_reach_repo_above_lib_dir() {
        let outer = TestRepo::init();
        outer.commit(&[("outer.txt", "outer\n")], "outer");
        outer.git(&["config", "core.sshCommand", "ssh -i outer-key"]);
        let lib_dir = outer.path().join("libs");
        std::fs::create_dir_all(&lib_dir).unwrap();
        let env = GitEnv::for_tests();
        assert_eq!(
            probe_ssh_command_config(&env, Some(&lib_dir), None, lib_deadline()),
            SshConfigProbe::Set
        );
        assert_eq!(
            probe_ssh_command_config(
                &env,
                Some(&lib_dir),
                Some(clone_ceiling(&lib_dir)),
                lib_deadline()
            ),
            SshConfigProbe::Unset
        );
        let args = os_args(&["rev-parse", "--git-dir"]);
        let res = run_git(
            &env,
            Some(&lib_dir),
            Some(clone_ceiling(&lib_dir)),
            &file_url(outer.path()),
            &args,
            lib_deadline(),
        );
        assert!(matches!(res, Err(GitFailure::Failed { .. })), "{res:?}");
    }

    #[test]
    fn library_pull_without_dot_git_is_not_a_clone_and_spawns_nothing() {
        let outer = TestRepo::init();
        let head = outer.commit(&[("outer.txt", "outer\n")], "outer");
        let clone = outer.path().join("libs").join("x");
        std::fs::create_dir_all(&clone).unwrap();
        let env = GitEnv::for_tests();
        let res = library_pull(&env, &file_url(outer.path()), &clone, lib_deadline());
        assert_eq!(res, Err(GitFailure::NotAClone));
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);
        assert_eq!(outer.git(&["rev-parse", "HEAD"]), head);
    }

    #[test]
    fn library_pull_fast_forward() {
        let repo = lib_remote();
        let (lib_dir, _, dest) = lib_cloned(&repo);
        let new_head = repo.commit(&[(LIB_FILE, FILE_V2)], "add New");
        let env = GitEnv::for_tests();
        let res = library_pull(&env, &file_url(repo.path()), &dest, lib_deadline());
        assert_eq!(res, Ok(()));
        assert_eq!(lib_head(&dest), new_head);
        assert_eq!(
            std::fs::read_to_string(dest.join(LIB_FILE)).unwrap(),
            FILE_V2
        );
        assert_eq!(git_out(&dest, &["status", "--porcelain"]), "");
        assert_eq!(env.spawned.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_pull_after_force_push_fails_and_head_unchanged() {
        let repo = lib_remote();
        let (lib_dir, _, dest) = lib_cloned(&repo);
        let before = lib_head(&dest);
        let rewritten = repo.force_push(&[(LIB_FILE, FILE_V2)], "rewritten");
        assert_ne!(rewritten, before);
        let env = GitEnv::for_tests();
        let res = library_pull(&env, &file_url(repo.path()), &dest, lib_deadline());
        assert!(
            matches!(res, Err(GitFailure::Failed { ref detail }) if !detail.is_empty()),
            "{res:?}"
        );
        assert_eq!(lib_head(&dest), before);
        assert_eq!(
            std::fs::read_to_string(dest.join(LIB_FILE)).unwrap(),
            FILE_V1
        );
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_pull_conflicting_local_change_fails_without_touching_files() {
        let repo = lib_remote();
        let (lib_dir, _, dest) = lib_cloned(&repo);
        let local = "[[snippets]]\nname = \"A\"\ntext = \"local\"\n";
        std::fs::write(dest.join(LIB_FILE), local).unwrap();
        std::fs::write(dest.join("notes.txt"), "mine\n").unwrap();
        let before_files = lib_snapshot(&dest);
        let before_head = lib_head(&dest);
        repo.commit(&[(LIB_FILE, FILE_V2)], "upstream change");
        let env = GitEnv::for_tests();
        let res = library_pull(&env, &file_url(repo.path()), &dest, lib_deadline());
        assert!(
            matches!(res, Err(GitFailure::Failed { ref detail }) if !detail.is_empty()),
            "{res:?}"
        );
        assert_eq!(lib_snapshot(&dest), before_files);
        assert_eq!(lib_head(&dest), before_head);
        assert_eq!(git_out(&dest, &["stash", "list"]), "");
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_pull_non_conflicting_local_change_is_kept() {
        let repo = lib_remote();
        let (lib_dir, _, dest) = lib_cloned(&repo);
        std::fs::write(dest.join("README.md"), "edited by hand\n").unwrap();
        let new_head = repo.commit(&[(LIB_FILE, FILE_V2)], "upstream change");
        let env = GitEnv::for_tests();
        let res = library_pull(&env, &file_url(repo.path()), &dest, lib_deadline());
        assert_eq!(res, Ok(()));
        assert_eq!(lib_head(&dest), new_head);
        assert_eq!(
            std::fs::read_to_string(dest.join(LIB_FILE)).unwrap(),
            FILE_V2
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("README.md")).unwrap(),
            "edited by hand\n"
        );
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_pull_with_index_lock_fails_and_lock_stays() {
        let repo = lib_remote();
        let (lib_dir, _, dest) = lib_cloned(&repo);
        let lock = dest.join(".git").join("index.lock");
        std::fs::write(&lock, "").unwrap();
        let before = lib_head(&dest);
        repo.commit(&[(LIB_FILE, FILE_V2)], "upstream change");
        let env = GitEnv::for_tests();
        let res = library_pull(&env, &file_url(repo.path()), &dest, lib_deadline());
        assert!(
            matches!(res, Err(GitFailure::Failed { ref detail }) if !detail.is_empty()),
            "{res:?}"
        );
        assert!(lock.exists());
        assert_eq!(lib_head(&dest), before);
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_resync_dirty_clone_matches_remote() {
        let repo = lib_remote();
        repo.commit(&[(".gitignore", "build.log\n")], "ignore build.log");
        let (lib_dir, id, dest) = lib_cloned(&repo);
        // Hand edit, untracked, ignored, a local commit and an orphan index.lock.
        std::fs::write(dest.join("README.md"), "local commit\n").unwrap();
        git_out(&dest, &["commit", "-q", "-am", "local"]);
        std::fs::write(dest.join(LIB_FILE), "edited by hand\n").unwrap();
        std::fs::write(dest.join("notes.txt"), "mine\n").unwrap();
        std::fs::write(dest.join("build.log"), "log\n").unwrap();
        std::fs::write(dest.join(".git").join("index.lock"), "").unwrap();
        let remote_head = repo.commit(&[(LIB_FILE, FILE_V2)], "add New");

        let env = GitEnv::for_tests();
        let res = library_resync(
            &env,
            &file_url(repo.path()),
            "",
            &lib_dir,
            id,
            lib_deadline(),
        );
        assert_eq!(res, Ok(()));
        assert_eq!(git_out(&dest, &["status", "--porcelain", "--ignored"]), "");
        assert_eq!(lib_head(&dest), remote_head);
        assert!(!dest.join("notes.txt").exists());
        assert!(!dest.join("build.log").exists());
        assert!(!dest.join(".git").join("index.lock").exists());
        assert_eq!(
            std::fs::read(dest.join(LIB_FILE)).unwrap(),
            std::fs::read(repo.path().join(LIB_FILE)).unwrap()
        );
        assert_eq!(lib_entries(&lib_dir), vec![id.to_string()]);
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_resync_after_force_push() {
        let repo = lib_remote();
        let (lib_dir, id, dest) = lib_cloned(&repo);
        let rewritten = repo.force_push(&[(LIB_FILE, FILE_V2)], "rewritten");
        let env = GitEnv::for_tests();
        assert!(library_pull(&env, &file_url(repo.path()), &dest, lib_deadline()).is_err());
        let res = library_resync(
            &env,
            &file_url(repo.path()),
            "",
            &lib_dir,
            id,
            lib_deadline(),
        );
        assert_eq!(res, Ok(()));
        assert_eq!(lib_head(&dest), rewritten);
        assert_eq!(
            std::fs::read_to_string(dest.join(LIB_FILE)).unwrap(),
            FILE_V2
        );
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_resync_remote_gone_keeps_clone() {
        let repo = lib_remote();
        let url = file_url(repo.path());
        let (lib_dir, id, dest) = lib_cloned(&repo);
        std::fs::write(dest.join("notes.txt"), "mine\n").unwrap();
        let before_files = lib_snapshot(&dest);
        let before_head = lib_head(&dest);
        drop(repo); // R deleted: the remote is unreachable.
        let env = GitEnv::for_tests();
        let res = library_resync(&env, &url, "", &lib_dir, id, lib_deadline());
        assert!(
            matches!(res, Err(GitFailure::Failed { ref detail }) if !detail.is_empty()),
            "{res:?}"
        );
        assert_eq!(lib_snapshot(&dest), before_files);
        assert_eq!(lib_head(&dest), before_head);
        assert_eq!(lib_entries(&lib_dir), vec![id.to_string()]);
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_resync_timeout_keeps_clone() {
        let repo = lib_remote();
        let (lib_dir, id, dest) = lib_cloned(&repo);
        std::fs::write(dest.join("notes.txt"), "mine\n").unwrap();
        let before_files = lib_snapshot(&dest);
        let before_head = lib_head(&dest);
        let server = TestServer::silent();
        let env = GitEnv::for_tests();
        let start = Instant::now();
        let res = library_resync(
            &env,
            &server.url("team-lib.git"),
            "",
            &lib_dir,
            id,
            start + Duration::from_secs(2),
        );
        let elapsed = start.elapsed();
        assert!(matches!(res, Err(GitFailure::TimedOut { .. })), "{res:?}");
        assert!(elapsed <= Duration::from_secs(7), "took {elapsed:?}");
        assert_all_reaped(&env);
        assert_eq!(lib_snapshot(&dest), before_files);
        assert_eq!(lib_head(&dest), before_head);
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_resync_replace_failure_keeps_clone() {
        let repo = lib_remote();
        let (lib_dir, id, dest) = lib_cloned(&repo);
        std::fs::write(dest.join("notes.txt"), "mine\n").unwrap();
        let before_files = lib_snapshot(&dest);
        let before_head = lib_head(&dest);
        repo.commit(&[(LIB_FILE, FILE_V2)], "add New");
        let mut env = GitEnv::for_tests();
        env.rename = lib_failing_rename(&[".r-"]);
        let res = library_resync(
            &env,
            &file_url(repo.path()),
            "",
            &lib_dir,
            id,
            lib_deadline(),
        );
        assert!(
            matches!(res, Err(GitFailure::Io { ref detail }) if detail.contains("injected")),
            "{res:?}"
        );
        assert_eq!(lib_snapshot(&dest), before_files);
        assert_eq!(lib_head(&dest), before_head);
        assert_no_temp_entries(&lib_dir);
        assert_eq!(lib_entries(&lib_dir), vec![id.to_string()]);
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_resync_without_clone_and_remote_fails() {
        let repo = lib_remote();
        let url = file_url(repo.path());
        drop(repo);
        let lib_dir = lib_temp_dir();
        let id = uuid::Uuid::new_v4();
        let env = GitEnv::for_tests();
        let res = library_resync(&env, &url, "", &lib_dir, id, lib_deadline());
        assert!(res.is_err(), "{res:?}");
        assert!(!lib_dir.join(id.to_string()).exists());
        assert!(
            lib_entries(&lib_dir).is_empty(),
            "{:?}",
            lib_entries(&lib_dir)
        );
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_resync_double_failure_leaves_old_clone() {
        let repo = lib_remote();
        let (lib_dir, id, dest) = lib_cloned(&repo);
        std::fs::write(dest.join("notes.txt"), "old clone\n").unwrap();
        repo.commit(&[(LIB_FILE, FILE_V2)], "add New");
        let mut env = GitEnv::for_tests();
        env.rename = lib_failing_rename(&[".r-", ".o-"]);
        let res = library_resync(
            &env,
            &file_url(repo.path()),
            "",
            &lib_dir,
            id,
            lib_deadline(),
        );
        let Err(GitFailure::ResyncRestoreFailed {
            replace,
            restore,
            leftover,
        }) = res
        else {
            panic!("expected ResyncRestoreFailed, got {res:?}");
        };
        assert!(replace.contains("injected"), "{replace}");
        assert!(restore.contains("injected"), "{restore}");
        assert!(!dest.exists());
        let entries = lib_entries(&lib_dir);
        let prefix = format!("{id}.o-");
        let olds: Vec<&String> = entries.iter().filter(|n| n.starts_with(&prefix)).collect();
        assert_eq!(olds.len(), 1, "{entries:?}");
        assert_eq!(&leftover, olds[0]);
        assert_eq!(
            std::fs::read_to_string(lib_dir.join(&leftover).join("notes.txt")).unwrap(),
            "old clone\n"
        );
        assert!(!entries.iter().any(|n| n.contains(".r-")), "{entries:?}");
        assert_eq!(entries.len(), 1, "{entries:?}");
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    fn lib_make_read_only(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                lib_make_read_only(&path);
            } else {
                let mut perms = std::fs::metadata(&path).unwrap().permissions();
                perms.set_readonly(true);
                std::fs::set_permissions(&path, perms).unwrap();
            }
        }
        let mut perms = std::fs::metadata(dir).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(dir, perms).unwrap();
    }

    #[test]
    fn library_remove_dir_force_read_only_clone_is_deleted() {
        let repo = lib_remote();
        let (lib_dir, _, dest) = lib_cloned(&repo);
        lib_make_read_only(&dest);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut env = GitEnv::for_tests();
        let counter = calls.clone();
        env.remove_dir = Arc::new(move |dir: &Path| {
            counter.fetch_add(1, Ordering::SeqCst);
            std::fs::remove_dir_all(dir)
        });
        assert!(remove_dir_force(&env, &dest).is_ok());
        assert!(!dest.exists());
        let n = calls.load(Ordering::SeqCst);
        assert!((1..=2).contains(&n), "{n} attempts");
        let _ = std::fs::remove_dir_all(&lib_dir);
    }

    #[test]
    fn library_remove_dir_force_failing_delete_is_tried_twice() {
        let dir = lib_temp_dir();
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        lib_make_read_only(&dir);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut env = GitEnv::for_tests();
        let counter = calls.clone();
        env.remove_dir = Arc::new(move |_: &Path| {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("injected delete failure"))
        });
        assert!(remove_dir_force(&env, &dir).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(dir.exists());
        assert!(
            !std::fs::metadata(dir.join("f.txt"))
                .unwrap()
                .permissions()
                .readonly()
        );
        assert!(remove_dir_force(&GitEnv::production(), &dir).is_ok());
        assert!(!dir.exists());
    }

    // ---- Team libraries: local-changes check before a re-sync ----

    /// Tracked `muxel-library.toml`, `a.txt`, `b.txt` and a `.gitignore` with `*.log`.
    fn check_remote() -> TestRepo {
        let repo = TestRepo::init();
        repo.commit(
            &[
                (LIB_FILE, FILE_V1),
                ("a.txt", "a\n"),
                ("b.txt", "b\n"),
                (".gitignore", "*.log\n"),
            ],
            "init",
        );
        repo
    }

    fn check(clone: &Path) -> LocalChanges {
        library_local_changes(&GitEnv::for_tests(), clone, lib_deadline())
    }

    fn changes(files: usize, commits: usize) -> LocalChanges {
        LocalChanges::Changes { files, commits }
    }

    fn local_commit(clone: &Path, name: &str) {
        std::fs::write(clone.join(name), "local\n").unwrap();
        git_out(clone, &["add", name]);
        git_out(clone, &["commit", "-q", "-m", name]);
    }

    fn lib_cleanup(dir: &Path) {
        let _ = remove_dir_force(&GitEnv::production(), dir);
    }

    #[test]
    fn library_temp_suffix_is_eight_hex_digits_and_random() {
        let a = temp_suffix();
        let b = temp_suffix();
        assert_eq!(a.len(), 8, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b);
    }

    /// git's own files under the clone pass `MAX_PATH` while the clone's `.git`
    /// stays well under it (git for Windows rejects a `.git` path of about 230
    /// characters as "'$GIT_DIR' too big", `core.longpaths` or not).
    ///
    /// The padding is sized from the actual temp dir, so the window holds for
    /// any `TEMP` length (CI's differs from a developer machine's).
    #[cfg(windows)]
    #[test]
    fn library_ops_work_past_windows_max_path() {
        use std::os::windows::ffi::OsStrExt;
        /// The `.git` path length the padding aims for.
        const GIT_DIR_TARGET: usize = 205;
        /// Kept clear of git for Windows' "'$GIT_DIR' too big".
        const GIT_DIR_LIMIT: usize = 215;
        const MAX_PATH: usize = 260;
        const SEGMENT: &str = "deep_segment_past_max_path";
        let len = |p: &Path| p.as_os_str().encode_wide().count();
        // `<lib_dir>` + `\<36>.c-<8>\.git`, and that + `\objects\pack\pack-<40>.keep`.
        let git_dir_suffix = 1 + 36 + ".c-".len() + 8 + r"\.git".len();
        let keep_suffix = r"\objects\pack\pack-".len() + 40 + ".keep".len();

        let root = lib_temp_dir();
        let mut lib_dir = root.clone();
        // Pad to exactly `GIT_DIR_TARGET - git_dir_suffix`; each component
        // costs its name plus one separator.
        let mut need = (GIT_DIR_TARGET - git_dir_suffix).saturating_sub(len(&lib_dir));
        while need > SEGMENT.len() + 2 {
            lib_dir.push(SEGMENT);
            need -= SEGMENT.len() + 1;
        }
        if need >= 2 {
            lib_dir.push("p".repeat(need - 1));
        }
        let git_dir = len(&lib_dir) + git_dir_suffix;
        let keep = git_dir + keep_suffix;
        if git_dir >= GIT_DIR_LIMIT {
            eprintln!(
                "skipping: temp dir {} is too long for a `.git` path under {GIT_DIR_LIMIT} \
                 characters (got {git_dir})",
                root.display()
            );
            lib_cleanup(&root);
            return;
        }
        assert!(
            keep > MAX_PATH,
            "precondition: `.git` path is {git_dir} characters (target {GIT_DIR_TARGET}, \
             limit {GIT_DIR_LIMIT}), pack `.keep` path is {keep} (must exceed {MAX_PATH}); \
             lib_dir {}",
            lib_dir.display()
        );
        let repo = check_remote();
        let url = file_url(repo.path());
        let id = uuid::Uuid::new_v4();
        let dest = lib_dir.join(id.to_string());
        let env = GitEnv::for_tests();
        assert_eq!(
            library_clone(&env, &url, "", &dest, &lib_dir, lib_deadline()),
            Ok(())
        );
        assert_eq!(
            std::fs::read_to_string(dest.join(LIB_FILE)).unwrap(),
            FILE_V1
        );
        repo.commit(&[(LIB_FILE, FILE_V2)], "add New");
        assert_eq!(library_pull(&env, &url, &dest, lib_deadline()), Ok(()));
        assert_eq!(
            std::fs::read_to_string(dest.join(LIB_FILE)).unwrap(),
            FILE_V2
        );
        assert_eq!(
            library_local_changes(&env, &dest, lib_deadline()),
            LocalChanges::None
        );
        assert_eq!(
            library_resync(&env, &url, "", &lib_dir, id, lib_deadline()),
            Ok(())
        );
        assert_no_temp_entries(&lib_dir);
        assert_eq!(lib_entries(&lib_dir), vec![id.to_string()]);
        lib_cleanup(&root);
    }

    #[test]
    fn library_check_clean_clone_is_none() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        // No fetch: a new commit in `R` is not looked at.
        repo.commit(&[(LIB_FILE, FILE_V2)], "add New");
        assert_eq!(check(&clone), LocalChanges::None);
        lib_cleanup(&lib_dir);
    }

    #[test]
    fn library_check_missing_or_empty_dir_is_no_clone() {
        let lib_dir = lib_temp_dir();
        let env = GitEnv::for_tests();
        let missing = lib_dir.join(uuid::Uuid::new_v4().to_string());
        assert_eq!(
            library_local_changes(&env, &missing, lib_deadline()),
            LocalChanges::NoClone
        );
        let empty = lib_dir.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(
            library_local_changes(&env, &empty, lib_deadline()),
            LocalChanges::NoClone
        );
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);
        lib_cleanup(&lib_dir);
    }

    #[test]
    fn library_check_counts_modified_deleted_and_untracked_files_one_by_one() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        std::fs::write(clone.join("a.txt"), "changed\n").unwrap();
        std::fs::remove_file(clone.join("b.txt")).unwrap();
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        std::fs::create_dir_all(clone.join("tmp")).unwrap();
        std::fs::write(clone.join("tmp/x.txt"), "x\n").unwrap();
        std::fs::write(clone.join("tmp/y.txt"), "y\n").unwrap();
        assert_eq!(check(&clone), changes(5, 0));
        lib_cleanup(&lib_dir);
    }

    #[test]
    fn library_check_one_untracked_file() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        assert_eq!(check(&clone), changes(1, 0));
        lib_cleanup(&lib_dir);
    }

    #[test]
    fn library_check_counts_a_staged_rename_once() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        git_out(&clone, &["mv", "a.txt", "renamed.txt"]);
        // `R  renamed.txt\0a.txt\0`: one entry, its source path is not another.
        assert_eq!(
            git_out(&clone, &["status", "--porcelain"]),
            "R  a.txt -> renamed.txt"
        );
        assert_eq!(check(&clone), changes(1, 0));
        lib_cleanup(&lib_dir);
    }

    #[test]
    fn library_check_local_commits_are_counted_against_the_upstream() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        local_commit(&clone, "local.txt");
        assert_eq!(check(&clone), changes(0, 1));
        local_commit(&clone, "local2.txt");
        for name in ["u1.txt", "u2.txt", "u3.txt"] {
            std::fs::write(clone.join(name), "u\n").unwrap();
        }
        assert_eq!(check(&clone), changes(3, 2));
        lib_cleanup(&lib_dir);
    }

    #[test]
    fn library_check_no_upstream_or_detached_head_is_unknown() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        git_out(&clone, &["branch", "--unset-upstream"]);
        assert_eq!(check(&clone), LocalChanges::Unknown);
        lib_cleanup(&lib_dir);

        let (lib_dir, _, clone) = lib_cloned(&repo);
        git_out(&clone, &["checkout", "-q", "--detach"]);
        assert_eq!(check(&clone), LocalChanges::Unknown);
        lib_cleanup(&lib_dir);
    }

    #[test]
    fn library_check_folder_without_git_counts_its_files_without_git() {
        let lib_dir = lib_temp_dir();
        let dir = lib_dir.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join("empty/nested")).unwrap();
        std::fs::write(dir.join(LIB_FILE), FILE_V1).unwrap();
        std::fs::write(dir.join("docs/notes.md"), "notes\n").unwrap();
        let before = lib_snapshot(&dir);
        let env = GitEnv::for_tests();
        assert_eq!(
            library_local_changes(&env, &dir, lib_deadline()),
            changes(2, 0)
        );
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);
        assert!(!dir.join(".git").exists());
        assert_eq!(lib_snapshot(&dir), before);
        lib_cleanup(&lib_dir);
    }

    #[test]
    fn library_check_ignored_files_do_not_count() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        std::fs::write(clone.join("build.log"), "log\n").unwrap();
        assert_eq!(git_out(&clone, &["check-ignore", "build.log"]), "build.log");
        assert_eq!(check(&clone), LocalChanges::None);
        std::fs::write(clone.join("build.txt"), "log\n").unwrap();
        assert_eq!(check(&clone), changes(1, 0));
        lib_cleanup(&lib_dir);
    }

    #[test]
    fn library_check_git_missing_is_unknown() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        let empty_path = lib_temp_dir();
        let env = GitEnv {
            path_override: Some(empty_path.clone().into_os_string()),
            ..GitEnv::for_tests()
        };
        assert_eq!(
            library_local_changes(&env, &clone, lib_deadline()),
            LocalChanges::Unknown
        );
        assert!(clone.join("notes.txt").is_file());
        lib_cleanup(&lib_dir);
        lib_cleanup(&empty_path);
    }

    #[test]
    fn library_check_broken_head_is_unknown_even_inside_another_repo() {
        // `LIB_DIR` inside a dirty clone `P`: if git fell back to `P` the check
        // would answer `Changes`, not `Unknown`.
        let p_origin = TestRepo::init();
        p_origin.commit(&[("p.txt", "p\n")], "init");
        let p_root = lib_temp_dir();
        git_out(&p_root, &["clone", "-q", &file_url(p_origin.path()), "p"]);
        let parent = p_root.join("p");
        std::fs::write(parent.join("dirty.txt"), "dirty\n").unwrap();
        let repo = check_remote();
        let lib_dir = parent.join("libraries");
        let id = uuid::Uuid::new_v4();
        let clone = lib_dir.join(id.to_string());
        let env = GitEnv::for_tests();
        assert_eq!(
            library_clone(
                &env,
                &file_url(repo.path()),
                "",
                &clone,
                &lib_dir,
                lib_deadline()
            ),
            Ok(())
        );
        std::fs::write(clone.join(".git/HEAD"), "garbage").unwrap();
        assert_eq!(check(&clone), LocalChanges::Unknown);
        assert_eq!(std::fs::read(clone.join(".git/HEAD")).unwrap(), b"garbage");
        lib_cleanup(&p_root);
    }

    #[test]
    fn library_check_expired_deadline_is_unknown_without_git() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        let env = GitEnv::for_tests();
        assert_eq!(
            library_local_changes(&env, &clone, Instant::now()),
            LocalChanges::Unknown
        );
        assert_eq!(env.spawned.load(Ordering::SeqCst), 0);
        lib_cleanup(&lib_dir);
    }

    fn index_state(clone: &Path) -> (Vec<u8>, std::time::SystemTime) {
        let index = clone.join(".git/index");
        (
            std::fs::read(&index).unwrap(),
            std::fs::metadata(&index).unwrap().modified().unwrap(),
        )
    }

    #[test]
    fn library_check_is_read_only_and_offline() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        local_commit(&clone, "local.txt");
        std::fs::write(clone.join("a.txt"), "changed\n").unwrap();
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        // Same bytes, new mtime: a plain `git status` would rewrite the index.
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(clone.join(LIB_FILE), FILE_V1).unwrap();
        drop(repo); // `R` gone: the check must not need the remote.

        let files = lib_snapshot(&clone);
        let head = lib_head(&clone);
        let refs = git_out(&clone, &["for-each-ref"]);
        let index = index_state(&clone);
        let env = GitEnv::for_tests();

        assert_eq!(
            library_local_changes(&env, &clone, lib_deadline()),
            changes(2, 1)
        );
        assert_eq!(env.spawned.load(Ordering::SeqCst), 2, "status + rev-list");
        assert_all_reaped(&env);
        assert_eq!(lib_snapshot(&clone), files);
        assert_eq!(lib_head(&clone), head);
        assert_eq!(git_out(&clone, &["for-each-ref"]), refs);
        assert_eq!(index_state(&clone), index, "the index was rewritten");
        assert!(!clone.join(".git/index.lock").exists());
        assert!(!clone.join(".git/FETCH_HEAD").exists());

        // Without GIT_OPTIONAL_LOCKS=0 the same status does rewrite the index.
        git_out(&clone, &["status", "--porcelain"]);
        assert_ne!(index_state(&clone), index, "control: plain status");
        lib_cleanup(&lib_dir);
    }

    #[test]
    fn library_check_runs_no_askpass_program() {
        let repo = check_remote();
        let (lib_dir, _, clone) = lib_cloned(&repo);
        std::fs::write(clone.join("notes.txt"), "mine\n").unwrap();
        let dir = lib_temp_dir();
        let marker = dir.join("M");
        let script = askpass_script(&dir, &marker);
        let mut env = GitEnv::for_tests();
        for var in ["GIT_ASKPASS", "SSH_ASKPASS"] {
            env.extra_env
                .push((OsString::from(var), script.clone().into_os_string()));
        }
        env.extra_env.push((
            OsString::from("SSH_ASKPASS_REQUIRE"),
            OsString::from("force"),
        ));
        assert_eq!(
            library_local_changes(&env, &clone, lib_deadline()),
            changes(1, 0)
        );
        assert!(!marker.exists());
        lib_cleanup(&lib_dir);
        lib_cleanup(&dir);
    }
}
