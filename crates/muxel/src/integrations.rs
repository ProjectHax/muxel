//! Side-effecting wrappers around the `git` and `tmux` CLIs — the I/O half of
//! muxel-core's pure tmux/worktree helpers.

use crate::i18n::{t, tf};
#[cfg(test)]
use crate::test_support::{GitStopReason, GitTimingReceipt, GitTimings};
use anyhow::{Context, Result, bail};
use muxel_core::accomplishments::{self, Accomplishment};
use muxel_core::library::GitFailure;
use muxel_core::library::resync::LocalChanges;
use muxel_core::memory::{self, MemoryEntry};
use muxel_core::tmux::{ServerOrigin, SessionTarget, Socket};
use muxel_core::{
    ACCOMPLISHMENTS_FILE, MEMORY_DIR, MEMORY_FILE, RemoteHost, RemoteOs, SshAuth,
    accomplishments_header, memory_header, remote_ops, ssh,
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
/// owns (see `muxel_core::tmux::orphan_sessions`) — from muxel's own server and the
/// default one alike: a pane from before muxel had its own server is still running
/// on the default one, as is the user's own tmux, which Import offers. `None` when
/// the host can't be reached or tmux isn't there; an empty list means "reached it,
/// no sessions", and the two must not be confused — a blip would otherwise read as
/// "nothing running".
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
    // `tmux` is not on sshd's bare default PATH when it came from Homebrew — see
    // `ssh::tmux_path_prelude`. Unresolved, this reads as "no sessions on the host"
    // and every running agent there stays stranded.
    let sockets = local_tmux_sockets();
    let out = remote_ssh_command(c, ssh::list_sessions_command_on(&sockets))
        .output()
        .ok()?;
    ssh::parse_session_listings_on(&String::from_utf8_lossy(&out.stdout), &sockets)
}

/// Every tmux session on this machine, as [`list_remote_tmux_sessions`] reports a
/// host's: muxel's own server's, then the default one's. Isolated profiles list
/// only their own server. An empty list when no tmux server is running; `None`
/// when tmux couldn't be run at all.
pub fn list_local_tmux_sessions() -> Option<Vec<muxel_core::tmux::RemoteSession>> {
    let mut panes = Vec::new();
    for socket in local_tmux_sockets() {
        let out = tmux_on(&socket)
            .args(muxel_core::tmux::list_sessions_args())
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        // No server (or no socket yet) is a perfectly good answer: nothing running.
        if out.status.success() {
            panes.extend(muxel_core::tmux::parse_sessions_on(
                &String::from_utf8_lossy(&out.stdout),
                socket,
            ));
        } else if !muxel_core::tmux::no_server(&String::from_utf8_lossy(&out.stderr)) {
            return None;
        }
    }
    Some(panes)
}

/// Write `content` to a remote file (overwriting), piping it over SSH stdin.
pub fn write_remote_file(loc: &RepoLoc, abs_path: &str, content: &str) -> Result<()> {
    let RepoLoc::Remote(c) = loc else {
        bail!("not a remote file");
    };
    let cmd = remote_ops::write_file(c.host.os, abs_path);
    pipe_to_remote(c, cmd, content.as_bytes(), "write failed")?;
    Ok(())
}

/// Copy a pasted file to the host — see [`remote_ops::upload_paste`] — and return
/// its path there, ready to paste into a pane on that host.
pub fn upload_paste(loc: &RepoLoc, name: &str, bytes: &[u8]) -> Result<String> {
    let RepoLoc::Remote(c) = loc else {
        bail!("not a remote project");
    };
    let cmd = remote_ops::upload_paste(c.host.os, name);
    let out = pipe_to_remote(c, cmd, bytes, "upload failed")?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        bail!("the host didn't say where it saved “{name}”");
    }
    Ok(path)
}

/// Run `cmd` on the host with `input` piped to its stdin. `Err` with ssh's stderr
/// (or `failed` when it said nothing) unless the command succeeds.
fn pipe_to_remote(
    c: &RemoteConn,
    cmd: String,
    input: &[u8],
    failed: &str,
) -> Result<std::process::Output> {
    use std::io::Write;
    let mut command = remote_ssh_command(c, cmd);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| ssh_spawn_error(c.host.auth, e))?;
    // Written from its own thread: ssh may block writing stdout/stderr while this
    // is still writing a large input, and both sides would wait on each other.
    let mut stdin = child.stdin.take().context("ssh stdin")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().context("waiting for ssh")?;
    let written = writer
        .join()
        .map_err(|_| anyhow::anyhow!("writing to ssh panicked"))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        let msg = msg.trim();
        bail!("{}", if msg.is_empty() { failed } else { msg });
    }
    written.context("sending to ssh")?;
    Ok(out)
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
    ensure_muxel_file(loc, MEMORY_FILE, memory_header(), "ensure memory failed")
}

/// [`ensure_memory_file`] for the project's `.muxel/ACCOMPLISHMENTS.md`.
pub fn ensure_accomplishments_file(loc: &RepoLoc) -> Result<()> {
    ensure_muxel_file(
        loc,
        ACCOMPLISHMENTS_FILE,
        accomplishments_header(),
        "ensure accomplishments failed",
    )
}

/// Create `<root>/.muxel/`, seed `<file>` in it with `header` if absent, and make
/// sure `.muxel/` is git-ignored. `failed` is the error when a remote host says
/// nothing about why it couldn't.
fn ensure_muxel_file(loc: &RepoLoc, file: &str, header: &str, failed: &str) -> Result<()> {
    let ignore_line = format!("{MEMORY_DIR}/");
    match loc {
        RepoLoc::Local(root) => {
            let dir = root.join(MEMORY_DIR);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            let file = dir.join(file);
            if !file.exists() {
                std::fs::write(&file, header)
                    .with_context(|| format!("writing {}", file.display()))?;
            }
            ensure_gitignored(root, &ignore_line)
        }
        RepoLoc::Remote(c) => {
            let root = c.remote_path.trim_end_matches('/');
            let file = format!("{MEMORY_DIR}/{file}");
            let cmd = remote_ops::ensure_seeded_file(
                c.host.os,
                root,
                MEMORY_DIR,
                &file,
                header,
                &ignore_line,
            );
            let out = remote_ssh_command(c, cmd)
                .output()
                .map_err(|e| ssh_spawn_error(c.host.auth, e))?;
            if !out.status.success() {
                let msg = String::from_utf8_lossy(&out.stderr);
                let msg = msg.trim();
                bail!("{}", if msg.is_empty() { failed } else { msg });
            }
            Ok(())
        }
    }
}

/// Absolute path of a project's `.muxel/MEMORY.md`, local or remote.
fn memory_abs(loc: &RepoLoc) -> String {
    muxel_file_abs(loc, MEMORY_FILE)
}

/// Absolute path of `file` in a project's `.muxel/`, local or remote.
fn muxel_file_abs(loc: &RepoLoc, file: &str) -> String {
    match loc {
        RepoLoc::Local(root) => root.join(MEMORY_DIR).join(file).display().to_string(),
        RepoLoc::Remote(c) => format!(
            "{}/{MEMORY_DIR}/{file}",
            c.remote_path.trim_end_matches('/')
        ),
    }
}

/// Load a project's memory, maintained (new sections stamped, ordered, purged; see
/// [`memory::maintain`]). `now` is unix seconds. A missing file is an empty memory;
/// a file that can't be read — or a host that can't be reached — is an error, never
/// an empty list, so nothing is ever saved over entries that merely failed to load.
///
/// Agents add sections while muxel may be rewriting the file, so muxel writes only
/// when maintenance changes something, re-reading just before it writes so a
/// section added in between is kept.
pub fn maintain_memory(loc: &RepoLoc, now: i64) -> Result<Vec<MemoryEntry>> {
    let text = read_muxel_text(loc, MEMORY_FILE)?;
    let m = memory::maintain(memory::parse_document(&text), now);
    if !m.changed {
        return Ok(m.kept);
    }
    let fresh = read_muxel_text(loc, MEMORY_FILE)?;
    let m = if fresh == text {
        m
    } else {
        memory::maintain(memory::parse_document(&fresh), now)
    };
    if m.changed {
        write_memory_text(loc, &memory::render_document(&m.kept))?;
    }
    Ok(m.kept)
}

/// Apply a panel edit to the memory as it is on disk *now* — not to the copy the
/// panel showed, which may predate an agent's new section — then write the
/// maintained result and return it.
pub fn edit_memory(loc: &RepoLoc, now: i64, edit: &memory::Edit) -> Result<Vec<MemoryEntry>> {
    let text = read_muxel_text(loc, MEMORY_FILE)?;
    let mut entries = memory::maintain(memory::parse_document(&text), now).kept;
    edit.apply(&mut entries);
    let kept = memory::maintain(entries, now).kept;
    write_memory_text(loc, &memory::render_document(&kept))?;
    Ok(kept)
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

/// Overwrite the project's `.muxel/MEMORY.md` with `text`, creating `.muxel/`
/// (git-ignored) first if needed. Works local or remote.
fn write_memory_text(loc: &RepoLoc, text: &str) -> Result<()> {
    ensure_memory_file(loc)?; // dir + .gitignore (+ seed if absent)
    let path = memory_abs(loc);
    match loc {
        RepoLoc::Local(_) => std::fs::write(&path, text).with_context(|| format!("writing {path}")),
        RepoLoc::Remote(_) => write_remote_file(loc, &path, text),
    }
}

/// A project's accomplishment log as text (see [`read_muxel_text`]).
fn read_accomplishments_text(loc: &RepoLoc) -> Result<String> {
    read_muxel_text(loc, ACCOMPLISHMENTS_FILE)
}

/// One of a project's `.muxel/` files as text. A missing file reads as empty (it
/// simply has nothing yet); an unreadable one, or a host that can't be reached, is
/// an error, so a caller can say it couldn't load rather than show — or save over
/// — nothing.
fn read_muxel_text(loc: &RepoLoc, file: &str) -> Result<String> {
    let path = muxel_file_abs(loc, file);
    match loc {
        RepoLoc::Local(_) => match std::fs::read(&path) {
            // Lossy: one badly encoded append must not hide the rest of the file.
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("reading {path}")),
        },
        RepoLoc::Remote(_) => {
            read_remote_file(loc, &path).with_context(|| format!("couldn't read {path}"))
        }
    }
}

/// Overwrite the accomplishment log with `text`, creating `.muxel/` (git-ignored)
/// first if needed.
fn write_accomplishments_text(loc: &RepoLoc, text: &str) -> Result<()> {
    ensure_accomplishments_file(loc)?;
    let path = muxel_file_abs(loc, ACCOMPLISHMENTS_FILE);
    match loc {
        RepoLoc::Local(_) => std::fs::write(&path, text).with_context(|| format!("writing {path}")),
        RepoLoc::Remote(_) => write_remote_file(loc, &path, text),
    }
}

/// Load a project's accomplishment log, maintained (stamped, deduped, pruned to
/// `retention_days`; `now` is local wall-clock seconds), oldest first.
///
/// Agents append to this file while muxel may be rewriting it, so muxel writes only
/// when maintenance changes something (about once a day, when an entry ages out),
/// and re-reads just before writing so a line appended in between is kept.
pub fn maintain_accomplishments(
    loc: &RepoLoc,
    now: i64,
    retention_days: u32,
) -> Result<Vec<Accomplishment>> {
    let text = read_accomplishments_text(loc)?;
    if accomplishments::plan_rewrite(&text, now, retention_days).is_none() {
        return Ok(accomplishments::parse_document(&text));
    }
    let fresh = read_accomplishments_text(loc)?;
    match accomplishments::plan_rewrite(&fresh, now, retention_days) {
        Some(doc) => {
            write_accomplishments_text(loc, &doc)?;
            Ok(accomplishments::parse_document(&doc))
        }
        None => Ok(accomplishments::parse_document(&fresh)),
    }
}

/// Apply a user edit (add/delete from the panel) to the log as it is on disk *now*
/// — not to the copy the panel showed, which may predate an agent's append — then
/// write the maintained result and return it.
pub fn edit_accomplishments(
    loc: &RepoLoc,
    now: i64,
    retention_days: u32,
    edit: impl FnOnce(&mut Vec<Accomplishment>),
) -> Result<Vec<Accomplishment>> {
    let text = read_accomplishments_text(loc)?;
    let mut entries =
        accomplishments::maintain(accomplishments::parse_document(&text), now, retention_days).kept;
    edit(&mut entries);
    let kept = accomplishments::maintain(entries, now, retention_days).kept;
    write_accomplishments_text(loc, &accomplishments::render_document(&kept))?;
    Ok(kept)
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
/// Also establishes the master at `control_path`, so the panes that ride it
/// connect instantly.
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
///
/// The server is this profile's own: `mxl` for the regular profile, or the
/// isolated root's named socket. Any other owner is left alone: the default
/// server is the user's, and a named one a pane recorded belongs to another
/// muxel version or profile, which muxel didn't start, so it would read as
/// foreign below and have its environment rewritten.
///
/// The server's environment has to be muxel's too. A pane's program starts from
/// the server's global environment, which tmux copies from whatever process
/// started the server. If that wasn't muxel (say a pane killed the server and then
/// ran `tmux new-session`), muxel gives the server the environment it would have
/// started it with before opening a pane there. See
/// `muxel_core::tmux::SERVER_MARK_ENV`.
pub fn ensure_tmux_server(socket: &Socket) {
    let root = local_tmux_root();
    let socket = SessionTarget::new("", Some(socket.clone()))
        .for_profile(root.as_deref())
        .socket
        .unwrap();
    if matches!(socket, Socket::Default | Socket::Named(_)) {
        return;
    }
    ensure_tmux_server_on(&socket, root.as_deref());
}

/// [`ensure_tmux_server`] on `socket`, for the profile rooted at `root`.
fn ensure_tmux_server_on(socket: &Socket, root: Option<&Path>) {
    let mut tmux = tmux_server_command(socket.clone(), root);
    default_utf8_locale(&mut tmux);
    let started = tmux
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if started && tmux_server_origin(socket) == Some(ServerOrigin::Foreign) {
        reseed_tmux_server(socket, &command_environment(&tmux));
    }
}

/// Who started the local server on `socket`, read from the marker that
/// [`tmux_server_command`] gives a server it starts. Reading it also flags the
/// marker removed, so no pane inherits it. `None` if tmux couldn't be run.
fn tmux_server_origin(socket: &Socket) -> Option<ServerOrigin> {
    let out = tmux_on(socket)
        .args(muxel_core::tmux::server_origin_args())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    Some(ServerOrigin::from_probe(&out.stdout))
}

/// The environment `cmd` runs with: this process's, plus `cmd`'s own changes.
/// For [`tmux_server_command`] this is the global environment of a server it
/// starts, apart from the `PWD` tmux adds.
fn command_environment(cmd: &Command) -> Vec<(OsString, OsString)> {
    let mut env: std::collections::BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    for (name, value) in cmd.get_envs() {
        match value {
            Some(value) => env.insert(name.to_owned(), value.to_owned()),
            None => env.remove(name),
        };
    }
    env.into_iter().collect()
}

/// Give the local server on `socket`, which muxel didn't start, the environment
/// muxel would have started it with (`wanted`), and mark it as muxel's either way,
/// so this happens once per server. Blocking, best-effort, and logged. See
/// `muxel_core::tmux::reseed_environment_script`.
fn reseed_tmux_server(socket: &Socket, wanted: &[(OsString, OsString)]) {
    let Ok(out) = tmux_on(socket)
        .args(["show-environment", "-g"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let current = muxel_core::tmux::environment_names(&out.stdout);
    let wanted: Vec<_> = wanted
        .iter()
        .map(|(name, value)| (name.as_encoded_bytes(), value.as_encoded_bytes()))
        .collect();
    let script = muxel_core::tmux::reseed_environment_script(&current, &wanted);
    let server = String::from(socket.clone());
    if tmux_source_stdin(socket, &script) {
        let removed: Vec<_> = muxel_core::tmux::environment_extras(&current, &wanted)
            .into_iter()
            .map(String::from_utf8_lossy)
            .collect();
        let removed = if removed.is_empty() {
            "nothing".to_string()
        } else {
            removed.join(", ")
        };
        muxel_store::append_event_log(&format!(
            "tmux server {server} was not started by muxel; gave it muxel's environment (removed: {removed})"
        ));
    } else {
        // Mark it anyway: another try on this server would fail the same way, on
        // every pane.
        let _ = tmux_on(socket)
            .args(muxel_core::tmux::mark_server_args())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        muxel_store::append_event_log(&format!(
            "tmux server {server} was not started by muxel; could not give it muxel's environment (needs tmux 3.1 or newer)"
        ));
    }
}

/// Run `script` on the local server on `socket` through `tmux source-file -`, so
/// nothing in it lands in a command line. Whether tmux ran it.
fn tmux_source_stdin(socket: &Socket, script: &str) -> bool {
    use std::io::Write as _;
    let Ok(mut child) = tmux_on(socket)
        .args(["source-file", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    // Dropping stdin ends the script. A tmux too old to read one from stdin looks
    // for a file named `-` instead, and fails.
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(script.as_bytes()).is_ok());
    child.wait().is_ok_and(|status| status.success()) && written
}

fn tmux_server_command(socket: Socket, root: Option<&Path>) -> Command {
    let socket = SessionTarget::new("", Some(socket))
        .for_profile(root)
        .socket
        .unwrap();
    let mut tmux = tmux_on(socket);
    tmux.args(muxel_core::tmux::start_server_args())
        .env(muxel_core::tmux::SERVER_MARK_ENV, "1")
        .env_remove(crate::session_binding::MUXEL_INSTANCE_ID_ENV);
    if root.is_some() {
        // A server first started by an isolated GUI must not make its profile
        // the global default for unrelated sessions. Local panes get these
        // values through new-session -e instead.
        tmux.env_remove(muxel_store::USER_DATA_DIR_ENV)
            .env_remove(crate::control::CONTROL_ENV);
    }
    tmux
}

#[cfg(test)]
mod tmux_server_environment_tests {
    use super::tmux_server_command;
    use std::ffi::OsStr;

    #[test]
    fn isolated_startup_uses_its_socket_without_seeding_profile_environment() {
        for isolated in [false, true] {
            let root = isolated.then_some(std::path::Path::new("/isolated"));
            let command = tmux_server_command(muxel_core::tmux::Socket::Private, root);
            let args: Vec<_> = command.get_args().collect();
            let prefix = muxel_core::tmux::Socket::local(root)[0].args();
            assert_eq!(args[..2], prefix.iter().map(OsStr::new).collect::<Vec<_>>());
            let removed: Vec<_> = command
                .get_envs()
                .filter(|(_, value)| value.is_none())
                .map(|(key, _)| key)
                .collect();
            assert!(removed.contains(&OsStr::new(crate::session_binding::MUXEL_INSTANCE_ID_ENV)));
            for key in [muxel_store::USER_DATA_DIR_ENV, crate::control::CONTROL_ENV] {
                assert_eq!(removed.contains(&OsStr::new(key)), isolated);
            }
            assert!(command.get_envs().any(|(key, value)| {
                key == OsStr::new(muxel_core::tmux::SERVER_MARK_ENV) && value.is_some()
            }));
        }
    }
}

/// Against a real tmux, each on a socket of its own.
#[cfg(all(test, unix))]
mod tmux_server_origin_tests {
    use super::{ensure_tmux_server_on, tmux_on, tmux_source_stdin};
    use muxel_core::tmux::{SERVER_MARK_ENV, Socket};
    use std::time::{Duration, Instant};

    /// A server on a socket no other test or muxel uses, killed when dropped.
    struct Server(Socket);

    impl Server {
        fn new() -> Self {
            Self(Socket::Isolated(uuid::Uuid::new_v4().as_u128() as u64))
        }

        fn global(&self, name: &str) -> String {
            let out = tmux_on(&self.0)
                .args(["show-environment", "-g", name])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim_end().to_string()
        }

        /// `name` as a new pane's program sees it, or `None` if it isn't set.
        fn pane_sees(&self, name: &str) -> Option<String> {
            let file = std::env::temp_dir().join(format!("muxel-tmux-{}", uuid::Uuid::new_v4()));
            let done = file.with_extension("done");
            let script = r#"eval "test \${$1+set}" && eval "printf %s \"\$$1\"" > "$2"; : > "$3""#;
            let status = tmux_on(&self.0)
                .args(["new-session", "-d", "--", "sh", "-c", script, "sh", name])
                .arg(&file)
                .arg(&done)
                .status()
                .unwrap();
            assert!(status.success());
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done.exists() {
                assert!(Instant::now() < deadline, "the pane never ran");
                std::thread::sleep(Duration::from_millis(20));
            }
            let value = std::fs::read(&file).ok();
            let _ = std::fs::remove_file(&file);
            let _ = std::fs::remove_file(&done);
            value.map(|bytes| String::from_utf8(bytes).unwrap())
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = tmux_on(&self.0).arg("kill-server").output();
        }
    }

    #[test]
    #[ignore = "requires tmux 3.1 or newer"]
    fn a_server_muxel_starts_is_marked_without_panes_seeing_the_marker() {
        let server = Server::new();
        ensure_tmux_server_on(&server.0, None);
        assert_eq!(
            server.global(SERVER_MARK_ENV),
            format!("-{SERVER_MARK_ENV}")
        );
        assert_eq!(server.pane_sees(SERVER_MARK_ENV), None);
    }

    #[test]
    #[ignore = "requires tmux 3.1 or newer"]
    fn a_server_something_else_started_gets_muxels_environment() {
        let root = std::env::temp_dir().join(format!("muxel-tmux-{}", uuid::Uuid::new_v4()));
        let storage = muxel_store::test_support::set_root(root.clone());
        let server = Server::new();
        let home = std::env::var("HOME").unwrap();
        // What a pane that kills the server and runs `tmux new-session` leaves.
        let started = tmux_on(&server.0)
            .args(muxel_core::tmux::start_server_args())
            .env("HOME", "/nonexistent")
            .env("MUXEL_TEST_LEFTOVER", "1")
            .status()
            .unwrap();
        assert!(started.success());
        assert_eq!(server.pane_sees("HOME").as_deref(), Some("/nonexistent"));

        ensure_tmux_server_on(&server.0, None);
        assert_eq!(server.pane_sees("HOME"), Some(home));
        assert_eq!(server.pane_sees("MUXEL_TEST_LEFTOVER"), None);
        assert_eq!(
            server.global(SERVER_MARK_ENV),
            format!("-{SERVER_MARK_ENV}")
        );
        let log = std::fs::read_to_string(muxel_store::event_log_path().unwrap()).unwrap();
        assert!(log.contains("was not started by muxel"), "{log}");
        assert!(log.contains("MUXEL_TEST_LEFTOVER"), "{log}");
        drop(storage);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "requires tmux 3.1 or newer"]
    fn values_reach_panes_byte_for_byte() {
        let server = Server::new();
        ensure_tmux_server_on(&server.0, None);
        let value = " $HOME ${HOME} ~ \"q\" 'q' \\ ; \\; #{pid} %if {} -x\nline\ttab é ";
        let script = muxel_core::tmux::reseed_environment_script(
            &[],
            &[
                (b"MUXEL_TEST_VALUE", value.as_bytes()),
                (b"MUXEL_TEST_EMPTY", b""),
            ],
        );
        assert!(tmux_source_stdin(&server.0, &script));
        assert_eq!(server.pane_sees("MUXEL_TEST_VALUE").as_deref(), Some(value));
        assert_eq!(server.pane_sees("MUXEL_TEST_EMPTY").as_deref(), Some(""));
    }
}

/// The checked probes against a real tmux, each test on a throwaway named socket
/// of its own (never the default server or muxel's), skipped without tmux.
#[cfg(all(test, unix))]
mod tmux_probe_tests {
    use super::{
        ensure_tmux_server, kill_tmux_session_confirmed, local_tmux_has_session_confirmed,
        local_tmux_root, restart_local_tmux, run_local_tmux_input, tmux_on,
    };
    use muxel_core::tmux::{SERVER_MARK_ENV, SessionTarget, Socket};
    use std::time::{Duration, Instant};

    /// A named server no other test or muxel uses, killed when dropped. It reads no
    /// tmux config, so nothing of the user's runs in it.
    struct Server(Socket);

    impl Server {
        /// `None` (the test skips) without a tmux to run, or under an isolated
        /// profile, where `kill_tmux_session_confirmed` would aim at the profile's
        /// own server instead.
        fn new() -> Option<Self> {
            if local_tmux_root().is_some() {
                eprintln!("skipped: MUXEL_USER_DATA_DIR is set");
                return None;
            }
            if std::process::Command::new("tmux")
                .arg("-V")
                .output()
                .is_err()
            {
                eprintln!("skipped: tmux is not installed");
                return None;
            }
            let name = format!("mxltest-{}", uuid::Uuid::new_v4().simple());
            Some(Self(Socket::Named(name)))
        }

        /// Start the server with `env` in its global environment, kept running
        /// with no sessions (`exit-empty off`, as muxel's own), so a probe right
        /// after the last session is killed can't catch it shutting down.
        fn start_with(&self, env: &[(&str, &str)]) {
            let status = tmux_on(&self.0)
                .args(["-f", "/dev/null"])
                .args(muxel_core::tmux::start_server_args())
                .envs(env.iter().copied())
                .stdin(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "starting the server");
        }

        fn start(&self, sessions: &[&str]) {
            self.start_with(&[]);
            for session in sessions {
                let status = tmux_on(&self.0)
                    .args(["new-session", "-d", "-s", session, "sleep", "600"])
                    .stdin(std::process::Stdio::null())
                    .status()
                    .unwrap();
                assert!(status.success(), "starting {session}");
            }
        }

        fn has(&self, session: &str) -> bool {
            local_tmux_has_session_confirmed(&self.0, session, deadline()).unwrap()
        }

        fn target(&self, session: &str) -> SessionTarget {
            SessionTarget::new(session, Some(self.0.clone()))
        }

        /// `name` in the server's global environment, as `show-environment -g`
        /// prints it, or `None` if it isn't there.
        fn global(&self, name: &str) -> Option<String> {
            let out = tmux_on(&self.0)
                .args(["show-environment", "-g", name])
                .output()
                .unwrap();
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
        }

        /// Kill the server and wait until it has gone: while it shuts down, a
        /// client can still reach it and get "lost server". Whether it went.
        fn stop(&self) -> bool {
            let _ = tmux_on(&self.0).arg("kill-server").output();
            let until = deadline();
            loop {
                let gone = tmux_on(&self.0)
                    .arg("list-sessions")
                    .output()
                    .is_ok_and(|out| {
                        muxel_core::tmux::confirmed_no_server(&String::from_utf8_lossy(&out.stderr))
                    });
                if gone || Instant::now() >= until {
                    return gone;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop();
            // tmux leaves the socket file behind; it is this test's alone.
            let dir = std::env::var_os("TMUX_TMPDIR")
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(|| "/tmp".into());
            // SAFETY: `getuid` has no preconditions and cannot fail.
            let uid = unsafe { libc::getuid() };
            let socket = std::path::Path::new(&dir)
                .join(format!("tmux-{uid}"))
                .join(self.0.name().as_ref());
            let _ = std::fs::remove_file(socket);
        }
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    /// Every launch of a pane on Default goes through these answers, so tmux's own
    /// wording for "no such session" and "no server" must read as absent, and
    /// only the exact name as present.
    #[test]
    fn has_session_reads_a_real_servers_answers() {
        let Some(server) = Server::new() else {
            return;
        };
        assert!(!server.has("work"), "no server yet");
        server.start(&[]);
        assert!(!server.has("work"), "a server with no sessions");
        server.start(&["work", "work two"]);
        assert!(server.has("work"));
        assert!(server.has("work two"));
        for absent in ["wor", "work2", "other"] {
            assert!(!server.has(absent), "{absent}");
        }
        assert!(server.stop());
        assert!(!server.has("work"), "server gone");
    }

    #[test]
    fn a_confirmed_kill_stops_only_its_session() {
        let Some(server) = Server::new() else {
            return;
        };
        server.start(&["work", "other"]);
        let target = server.target("work");
        assert_eq!(
            kill_tmux_session_confirmed(&target, deadline()).unwrap(),
            target
        );
        assert!(!server.has("work"));
        assert!(server.has("other"));
        // Already gone, and then no server at all: both are a finished kill.
        assert_eq!(
            kill_tmux_session_confirmed(&target, deadline()).unwrap(),
            target
        );
        assert!(server.stop());
        assert_eq!(
            kill_tmux_session_confirmed(&target, deadline()).unwrap(),
            target
        );
    }

    #[test]
    fn a_local_restart_kills_only_after_saving_its_owner() {
        let Some(server) = Server::new() else {
            return;
        };
        server.start(&["work"]);
        let target = server.target("work");
        let refused = restart_local_tmux(&target, deadline(), |_| anyhow::bail!("save failed"));
        assert!(refused.is_err());
        assert!(server.has("work"), "a failed save leaves the agent running");

        let mut saved = None;
        let (owner, next) = restart_local_tmux(&target, deadline(), |owner| {
            saved = Some(owner.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(saved, Some(target.clone()));
        assert_eq!((owner, next), (target.clone(), target));
        assert!(!server.has("work"));
    }

    /// A pane's recorded named owner is another muxel version's or profile's
    /// server, which this muxel didn't start: muxel must not take it over and
    /// give it its own environment.
    #[test]
    fn a_named_owners_server_keeps_its_own_environment() {
        let Some(server) = Server::new() else {
            return;
        };
        server.start_with(&[("MUXEL_TEST_FOREIGN", "1")]);
        ensure_tmux_server(&server.0);
        assert_eq!(
            server.global("MUXEL_TEST_FOREIGN").as_deref(),
            Some("MUXEL_TEST_FOREIGN=1")
        );
        assert_eq!(
            server.global(SERVER_MARK_ENV),
            None,
            "not marked as muxel's"
        );
    }

    /// A local pane's ctl input runner, on a real server: it finds the pane,
    /// pastes text it is given on standard input (never in an argument, where
    /// tmux would split `;` into another command), and reports tmux's refusal
    /// as a failed command rather than as an error starting it.
    #[test]
    fn ctl_input_runs_on_the_panes_server_with_its_text_on_stdin() {
        use muxel_core::control::Key;
        use muxel_core::tmux;
        let Some(server) = Server::new() else {
            return;
        };
        server.start_with(&[]);
        let dir = std::env::temp_dir().join(format!("muxel-ctl-input-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let recv = dir.join("agent.bin");
        let agent = format!("stty raw -echo; exec cat > '{}'", recv.display());
        let status = tmux_on(&server.0)
            .args(["new-session", "-d", "-s", "agent", "sh", "-c", &agent])
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let until = Instant::now() + Duration::from_secs(10);
        while !recv.exists() {
            assert!(Instant::now() < until, "the agent never started");
            std::thread::sleep(Duration::from_millis(10));
        }

        let out = run_local_tmux_input(&server.0, &tmux::control_pane_args("agent"), None).unwrap();
        let pane = tmux::parse_control_pane("agent", &String::from_utf8_lossy(&out.stdout))
            .expect("the agent's pane");
        let buffer = tmux::control_buffer_name(uuid::Uuid::new_v4());
        let text = "it's; $HOME `id` \"x\" ; display-message boom";
        let out = run_local_tmux_input(
            &server.0,
            &tmux::control_paste_args(&buffer, &pane, true),
            Some(text.as_bytes()),
        )
        .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = run_local_tmux_input(
            &server.0,
            &tmux::control_keys_args(&pane, &[Key::Char(';'), Key::Enter]),
            None,
        )
        .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let expected = format!("{text};\r");
        let until = Instant::now() + Duration::from_secs(10);
        while std::fs::read(&recv).unwrap_or_default() != expected.as_bytes() {
            assert!(
                Instant::now() < until,
                "the agent got {:?}",
                String::from_utf8_lossy(&std::fs::read(&recv).unwrap_or_default())
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let out = run_local_tmux_input(
            &server.0,
            &tmux::control_keys_args("%999", &[Key::Enter]),
            None,
        )
        .unwrap();
        assert!(!out.status.success(), "a missing pane is refused");
        let _ = std::fs::remove_dir_all(&dir);
    }
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

/// Whether the local tmux can start a session's program with variables of its
/// own (`new-session -e`, tmux 3.2 and later). Cache support, but probe each
/// negative result again so restarting a pane after an upgrade can recover.
pub fn tmux_session_env_supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    cached_tmux_session_env_support(&SUPPORTED, || {
        let Ok(out) = command("tmux")
            .arg("-V")
            .stdin(std::process::Stdio::null())
            .output()
        else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        let version = String::from_utf8_lossy(&out.stdout);
        muxel_core::tmux::session_env_supported(&version)
    })
}

fn cached_tmux_session_env_support(
    supported: &std::sync::OnceLock<bool>,
    probe: impl FnOnce() -> bool,
) -> bool {
    if supported.get() == Some(&true) {
        return true;
    }
    if probe() {
        let _ = supported.set(true);
        true
    } else {
        false
    }
}

/// Undo [`ensure_tmux_server`]'s `exit-empty off` so the server goes away with
/// its last session once muxel is gone. Best-effort, fire-and-forget.
///
/// Only this profile's own server: the default one is the user's.
pub fn restore_tmux_exit_empty() {
    let _ = tmux_on(&local_tmux_sockets()[0])
        .args(muxel_core::tmux::restore_exit_empty_args())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `tmux` on `socket` — muxel's own server, or the default one (see
/// `muxel_core::tmux::Socket`).
fn tmux_on(socket: impl std::borrow::Borrow<Socket>) -> Command {
    let mut cmd = command("tmux");
    cmd.args(socket.borrow().args());
    cmd
}

fn local_tmux_root() -> Option<PathBuf> {
    std::env::var_os(muxel_store::USER_DATA_DIR_ENV)
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn local_tmux_sockets() -> Vec<Socket> {
    Socket::local(local_tmux_root().as_deref())
}

pub fn profile_tmux_target(session: &SessionTarget) -> SessionTarget {
    session.for_profile(local_tmux_root().as_deref())
}

pub fn profile_tmux_socket() -> Socket {
    local_tmux_root()
        .as_deref()
        .map(Socket::isolated)
        .unwrap_or(Socket::Private)
}

/// Whether the local server on `socket` has session `session`.
fn local_tmux_has_session(socket: Socket, session: &str) -> bool {
    tmux_on(socket)
        .args(muxel_core::tmux::has_session_args(session))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The local server holding `session`: muxel's own, unless the session is a pane
/// from before muxel had one, still running on the default server — which is then
/// reattached where it is rather than replaced. See `Socket::locate`. Blocking:
/// one or two `has-session` calls for the regular profile. An isolated profile
/// selects its own socket without probing either regular-profile server.
pub fn local_tmux_socket(session: &SessionTarget) -> Socket {
    let session = profile_tmux_target(session);
    session
        .socket
        .clone()
        .unwrap_or_else(|| Socket::locate(|socket| local_tmux_has_session(socket, &session.name)))
}

/// The launch target may move off Default only after both exact-name probes
/// answer. A Private namesake belongs to somebody else and is never adopted.
pub fn local_tmux_launch_target(
    session: &SessionTarget,
    deadline: Instant,
) -> Result<SessionTarget> {
    let session = profile_tmux_target(session);
    local_tmux_launch_target_with(&session, |socket| {
        local_tmux_has_session_confirmed(socket, &session.name, deadline)
    })
}

/// [`local_tmux_launch_target`] on `probe`, an exact-name `has-session` on one
/// server. With no tmux to run ([`tmux_missing`]) nothing can say where the
/// session lives or that a move off Default is safe, so the recorded binding is
/// kept: the launch then fails on it, as it always did without tmux.
fn local_tmux_launch_target_with(
    session: &SessionTarget,
    mut probe: impl FnMut(&Socket) -> Result<bool>,
) -> Result<SessionTarget> {
    let target = resolve_tmux_owner_with(session, &mut probe)
        .and_then(|owner| confirmed_tmux_respawn_target(&owner, &mut probe));
    match target {
        Err(error) if tmux_missing(&error) => Ok(session.clone()),
        target => target,
    }
}

/// `session` with its owner resolved: the recorded one, or for legacy metadata
/// the server `probe` finds it on. A failed probe is an error, never an empty
/// server.
fn resolve_tmux_owner_with(
    session: &SessionTarget,
    mut probe: impl FnMut(&Socket) -> Result<bool>,
) -> Result<SessionTarget> {
    let socket = match &session.socket {
        Some(socket) => socket.clone(),
        None => Socket::try_locate(|socket| probe(&socket))?,
    };
    Ok(SessionTarget::new(session.name.clone(), Some(socket)))
}

pub(crate) fn confirmed_tmux_respawn_target(
    session: &SessionTarget,
    probe: impl FnMut(&Socket) -> Result<bool>,
) -> Result<SessionTarget> {
    session.for_respawn(probe)?.with_context(|| {
        t("A session with this name is already running on another tmux server, so this pane was not started. It attaches by itself if another muxel sharing this project's layout assigns that session to it; otherwise stop that session, then restart the pane.")
    })
}

fn local_tmux_has_session_confirmed(
    socket: &Socket,
    session: &str,
    deadline: Instant,
) -> Result<bool> {
    let mut cmd = tmux_on(socket);
    cmd.args(muxel_core::tmux::has_session_args(session));
    has_session_reply(cmd, session, deadline)
}

/// Whether `error` says tmux itself couldn't be run: not installed, or not on
/// `PATH`. Then a kill has nothing to stop, but no probe has answered either: a
/// tmux muxel can't find (one outside the `PATH` it was started with) may still
/// run the session, so this never authorizes a change of owner.
fn tmux_missing(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    })
}

/// [`local_tmux_has_session_confirmed`] for a prepared `has-session` command.
/// No tmux to run is an error ([`tmux_missing`]), not an absent session.
fn has_session_reply(cmd: Command, session: &str, deadline: Instant) -> Result<bool> {
    let out = run_tmux_output_until(cmd, deadline)?;
    if out.status.success() {
        return Ok(true);
    }
    let error = String::from_utf8_lossy(&out.stderr);
    if out.status.code() == Some(1) && muxel_core::tmux::has_session_absent(&error) {
        return Ok(false);
    }
    bail!("couldn't query tmux session “{session}”: {}", error.trim())
}

/// Collect a bounded command reply within one deadline, killing and reaping an
/// overdue child. Both pipes drain concurrently; oversized replies fail closed.
fn run_tmux_output_until(cmd: Command, deadline: Instant) -> Result<std::process::Output> {
    run_tmux_output_with(cmd, deadline, &GitEnv::production())
}

fn run_tmux_output_with(
    mut cmd: Command,
    deadline: Instant,
    lifecycle: &GitEnv,
) -> Result<std::process::Output> {
    use std::io::Read;
    fn capture(
        pipe: impl Read + Send + 'static,
    ) -> std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            const MAX_REPLY: u64 = 1024 * 1024;
            let mut bytes = Vec::new();
            let reply = pipe
                .take(MAX_REPLY + 1)
                .read_to_end(&mut bytes)
                .and_then(|_| {
                    if bytes.len() as u64 > MAX_REPLY {
                        Err(std::io::Error::other("tmux probe reply is too large"))
                    } else {
                        Ok(bytes)
                    }
                });
            let _ = tx.send(reply);
        });
        rx
    }
    if Instant::now() >= deadline {
        bail!("tmux probe timed out");
    }
    cmd.env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = lifecycle.spawn(&mut cmd).context("starting tmux probe")?;
    let stdout = capture(child.child.stdout.take().context("tmux probe stdout")?);
    let stderr = capture(child.child.stderr.take().context("tmux probe stderr")?);
    let (child, status) = lifecycle.wait_until(child, deadline);
    lifecycle.reap(child);
    let status = status.context("tmux probe timed out")?;
    let stdout = stdout.recv_timeout(deadline.saturating_duration_since(Instant::now()))??;
    let stderr = stderr.recv_timeout(deadline.saturating_duration_since(Instant::now()))??;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Complete, checked inventory for a Default-owner launch. Partial output or an
/// inaccessible server cannot authorize migration to another socket.
pub fn remote_tmux_launch_target(
    loc: &RepoLoc,
    session: &SessionTarget,
    instance: uuid::Uuid,
    require_absent: bool,
    deadline: Instant,
) -> Result<SessionTarget> {
    let RepoLoc::Remote(conn) = loc else {
        bail!("not a remote project")
    };
    let (session, sockets) = tmux_launch_inventory(session, local_tmux_root().as_deref());
    let out = remote_session_names(conn, &sockets, deadline)?;
    // A session still running on Default is reattached there, on Default's answer
    // alone. Only moving it off Default needs every server's checked answer.
    if ssh::still_on_default(
        &ssh::checked_session_listings_on(&out, &sockets),
        &sockets,
        &session,
    ) {
        if require_absent {
            bail!("tmux session “{session}” is still running");
        }
        return Ok(session);
    }
    let sessions = ssh::parse_session_listings_for_migration_on(&out, &sockets)
        .context("couldn't confirm the tmux servers queried")?;
    let resolved = muxel_core::tmux::ClosedSession {
        name: session.name.clone(),
        socket: session.socket.clone(),
        instance,
    }
    .resolve_owner(&sessions, profile_tmux_socket())
    .target();
    if require_absent
        && sessions
            .iter()
            .any(|row| Some(&row.socket) == session.socket.as_ref() && row.name == resolved.name)
    {
        bail!("tmux session “{session}” is still running");
    }
    confirmed_tmux_respawn_target(&resolved, |socket| {
        Ok(sessions
            .iter()
            .any(|row| &row.socket == socket && row.name == resolved.name))
    })
}

/// [`ssh::list_session_names_command_on`] `sockets` on the host, within `deadline`.
fn remote_session_names(
    conn: &RemoteConn,
    sockets: &[Socket],
    deadline: Instant,
) -> Result<String> {
    let command = format!(
        "export LC_ALL=C; {}",
        ssh::list_session_names_command_on(sockets)
    );
    let out = run_tmux_output_until(remote_ssh_command(conn, command), deadline)?;
    if !out.status.success() {
        bail!("{}", ssh_error_message(&out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn tmux_launch_inventory(
    session: &SessionTarget,
    root: Option<&Path>,
) -> (SessionTarget, Vec<Socket>) {
    let session = session.for_profile(root);
    let sockets = match &session.socket {
        Some(Socket::Named(_) | Socket::Isolated(_)) => session.socket.as_slice().to_vec(),
        _ => Socket::local(root),
    };
    (session, sockets)
}

/// Whether a tmux session is still alive, on this profile's servers (exact-match target,
/// so `muxel_p_1` never matches `muxel_p_12`). Fast and blocking; only called for
/// a pane that just died.
pub fn tmux_session_exists(session: &SessionTarget) -> bool {
    let session = profile_tmux_target(session);
    match &session.socket {
        Some(socket) => local_tmux_has_session(socket.clone(), &session.name),
        None => Socket::BOTH
            .into_iter()
            .any(|socket| local_tmux_has_session(socket, &session.name)),
    }
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

/// Run `tmux <args>` on the server holding `session`, where its project's sessions
/// live: on this machine, or on its SSH host (reusing the host's ControlMaster).
/// `None` for a Windows host, which has no tmux.
fn tmux_at(
    loc: &RepoLoc,
    session: &SessionTarget,
    args: &[String],
) -> Option<std::process::Output> {
    tmux_command_at(loc, session, args)?.output().ok()
}

fn tmux_command_at(loc: &RepoLoc, session: &SessionTarget, args: &[String]) -> Option<Command> {
    let session = profile_tmux_target(session);
    let mut cmd = match loc {
        RepoLoc::Local(_) => {
            let mut cmd = tmux_on(local_tmux_socket(&session));
            cmd.args(args);
            cmd
        }
        RepoLoc::Remote(c) => {
            if c.host.os.is_windows() {
                return None;
            }
            remote_ssh_command(c, ssh::session_tmux_command(&session, args))
        }
    };
    cmd.stdin(Stdio::null());
    Some(cmd)
}

/// How long one outside-control tmux command may take before it counts as
/// unanswered.
const TMUX_INPUT_DEADLINE: Duration = Duration::from_secs(10);

/// One outside-control input command (`muxel_terminal::TmuxInputRoute`) on the
/// local tmux server `socket` a pane's client attached to, with `stdin` as its
/// standard input. Blocking and bounded.
pub fn run_local_tmux_input(
    socket: &Socket,
    args: &[String],
    stdin: Option<&[u8]>,
) -> std::io::Result<std::process::Output> {
    let mut cmd = tmux_on(socket.clone());
    cmd.args(args);
    run_tmux_input(cmd, stdin, false)
}

/// The same on a remote pane's host, over the host's commands master, on the
/// server holding `session` (as the shared claim is read and written). The
/// input text travels on ssh's standard input, never in the command line.
pub fn run_remote_tmux_input(
    conn: &RemoteConn,
    session: &SessionTarget,
    args: &[String],
    stdin: Option<&[u8]>,
) -> std::io::Result<std::process::Output> {
    if conn.host.os.is_windows() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "a Windows host has no tmux",
        ));
    }
    let cmd = remote_ssh_command(conn, ssh::session_tmux_command(session, args));
    run_tmux_input(cmd, stdin, true)
}

fn run_tmux_input(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    over_ssh: bool,
) -> std::io::Result<std::process::Output> {
    use std::io::{Read as _, Write as _};
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    // Written from its own thread, so a child that never reads can't hold this
    // one past the deadline. It ends when the child's end of the pipe closes.
    let feeder = stdin.zip(child.stdin.take()).map(|(data, mut pipe)| {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        })
    });
    let deadline = Instant::now() + TMUX_INPUT_DEADLINE;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("no answer within {} seconds", TMUX_INPUT_DEADLINE.as_secs()),
                ));
            }
        }
    };
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_end(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_end(&mut stderr);
    }
    // ssh exits 255 for its own failures, which can come after the remote
    // command ran.
    if over_ssh && status.code() == Some(255) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            format!("ssh failed: {}", String::from_utf8_lossy(&stderr).trim()),
        ));
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Run this muxel's own `ctl` command (`exe ctl <args>`), the way an outside
/// agent does — Settings → Outside control's Test.
pub fn run_muxel_ctl(exe: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    command(exe)
        .arg("ctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
}

/// The title of local `session`'s active pane (`#{pane_title}`): what its
/// program last set with OSC 0/2. A tmux client forwards none of those titles
/// unless `set-titles` is on, which muxel leaves to the user's own tmux config.
/// Blocking and bounded to two seconds; `None` when the session or server is
/// gone. Never `capture-pane`.
pub fn tmux_pane_title(socket: &Socket, session: &str) -> Option<String> {
    pane_title_from(tmux_on(socket), session)
}

fn pane_title_from(mut tmux: Command, session: &str) -> Option<String> {
    use std::io::Read as _;
    tmux.args(muxel_core::tmux::pane_title_args(session))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        tmux.process_group(0);
    }
    let mut child = tmux.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    status
        .success()
        .then(|| muxel_core::tmux::parse_pane_title(session, &out))
        .flatten()
}

/// Against a real tmux on a throwaway socket that no muxel profile uses.
/// Skipped where tmux isn't installed.
#[cfg(all(test, unix))]
mod pane_title_tests {
    use super::pane_title_from;
    use std::process::Command;
    use std::time::{Duration, Instant};

    struct Server(String);

    impl Server {
        fn tmux(&self) -> Command {
            let mut tmux = Command::new("tmux");
            tmux.args(["-L", &self.0, "-f", "/dev/null"])
                .env_remove("TMUX");
            tmux
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let socket = self
                .tmux()
                .args(["display-message", "-p", "#{socket_path}"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
                .filter(|path| path.ends_with(&self.0));
            let _ = self.tmux().arg("kill-server").output();
            if let Some(socket) = socket {
                let _ = std::fs::remove_file(socket);
            }
        }
    }

    #[test]
    fn a_tmux_panes_program_title_is_read_without_set_titles() {
        if Command::new("tmux").arg("-V").output().is_err() {
            eprintln!("tmux is not installed; skipping");
            return;
        }
        let server = Server(format!("mxltest-{}", uuid::Uuid::new_v4().simple()));
        let session = "muxel_title_12345678";
        let status = server
            .tmux()
            .args([
                "new-session",
                "-d",
                "-s",
                session,
                "-x",
                "80",
                "-y",
                "24",
                "--",
            ])
            .args([
                "sh",
                "-c",
                r"printf '\033]2;scratch | Ready\033\\'; sleep 30",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let title = pane_title_from(server.tmux(), session);
            if title.as_deref() == Some("scratch | Ready") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "pane title never arrived: {title:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // The exact-match target never reads a namesake, and a missing
        // session or server is no title rather than a stale one.
        assert_eq!(pane_title_from(server.tmux(), "muxel_title_1234567"), None);
        let gone = Server(format!("mxltest-{}", uuid::Uuid::new_v4().simple()));
        assert_eq!(pane_title_from(gone.tmux(), session), None);
    }
}

/// A tmux session's user option (`@name`), wherever the session lives. `None`
/// when it is unset, the session is gone, or tmux couldn't be reached.
pub fn tmux_option(loc: &RepoLoc, session: &SessionTarget, option: &str) -> Option<String> {
    let out = tmux_at(
        loc,
        session,
        &muxel_core::tmux::show_option_args(&session.name, option),
    )?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !value.is_empty()).then_some(value)
}

/// Set a tmux session's user option (`@name`), wherever the session lives.
pub fn set_tmux_option(
    loc: &RepoLoc,
    session: &SessionTarget,
    option: &str,
    value: &str,
) -> Result<()> {
    let args = muxel_core::tmux::set_option_args(&session.name, option, value);
    let Some(out) = tmux_at(loc, session, &args) else {
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

/// Give claim cleanup two seconds before terminating its command.
pub fn unset_tmux_option_if(
    loc: &RepoLoc,
    session: &SessionTarget,
    option: &str,
    pattern: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let lifecycle = GitEnv::production();
    let session = profile_tmux_target(session);
    let args = muxel_core::tmux::unset_option_if_args(&session.name, option, pattern)
        .ok_or_else(|| anyhow::anyhow!("can't quote tmux claim cleanup safely"))?;
    let cmd = match loc {
        RepoLoc::Local(_) => {
            let socket = bounded_tmux_socket(&session, deadline, &lifecycle, |socket| {
                let mut cmd = tmux_on(socket);
                cmd.args(muxel_core::tmux::has_session_args(&session.name));
                cmd
            })?;
            let mut cmd = tmux_on(socket);
            cmd.args(args);
            cmd
        }
        RepoLoc::Remote(_) => tmux_command_at(loc, &session, &args)
            .ok_or_else(|| anyhow::anyhow!("tmux is unavailable"))?,
    };
    run_claim_cleanup(cmd, deadline, &lifecycle)
}

fn bounded_tmux_socket(
    session: &SessionTarget,
    deadline: Instant,
    lifecycle: &GitEnv,
    mut probe: impl FnMut(Socket) -> Command,
) -> Result<Socket> {
    if let Some(socket) = &session.socket {
        return Ok(socket.clone());
    }
    Socket::try_locate(|socket| {
        let status = run_tmux_until(probe(socket), deadline, lifecycle)?
            .ok_or_else(|| anyhow::anyhow!("tmux socket discovery timed out"))?;
        Ok(status.success())
    })
}

fn run_claim_cleanup(cmd: Command, deadline: Instant, lifecycle: &GitEnv) -> Result<()> {
    match run_tmux_until(cmd, deadline, lifecycle)? {
        Some(status) if status.success() => Ok(()),
        Some(status) => bail!("tmux claim cleanup failed: {status}"),
        None => bail!("tmux claim cleanup timed out"),
    }
}

fn run_tmux_until(
    mut cmd: Command,
    deadline: Instant,
    lifecycle: &GitEnv,
) -> Result<Option<ExitStatus>> {
    if Instant::now() >= deadline {
        return Ok(None);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = lifecycle.spawn(&mut cmd).context("starting tmux command")?;
    let (child, status) = lifecycle.wait_until(child, deadline);
    lifecycle.reap(child);
    Ok(status)
}

/// Kill a tmux session on its owner, resolving legacy bindings first. Best-effort.
pub fn kill_tmux_session(session: &SessionTarget) {
    let _ = tmux_on(local_tmux_socket(session))
        .args(muxel_core::tmux::kill_session_args(&session.name))
        .output();
}

/// Restart requires a confirmed teardown; a failed kill must never turn into a
/// silent create-or-attach of the old process.
pub fn kill_tmux_session_confirmed(
    session: &SessionTarget,
    deadline: Instant,
) -> Result<SessionTarget> {
    let session = profile_tmux_target(session);
    let socket = session
        .socket
        .as_ref()
        .context("resolve the tmux owner before restart")?;
    let mut cmd = tmux_on(socket);
    cmd.args(muxel_core::tmux::kill_session_args(&session.name));
    kill_tmux_session_confirmed_with(
        &session,
        || kill_session_reply(cmd, deadline),
        |socket| local_tmux_has_session_confirmed(socket, &session.name, deadline),
    )
}

/// [`kill_tmux_session_confirmed`] on `kill` and `probe` (an exact-name
/// `has-session` on one server), for `session` with a resolved owner.
///
/// With no tmux to run ([`tmux_missing`]) nothing runs that muxel could stop or
/// attach to, so the restart goes on, but on the recorded owner: no probe
/// answered, so nothing says a move off Default is safe.
fn kill_tmux_session_confirmed_with(
    session: &SessionTarget,
    kill: impl FnOnce() -> Result<()>,
    mut probe: impl FnMut(&Socket) -> Result<bool>,
) -> Result<SessionTarget> {
    let owner = session
        .socket
        .as_ref()
        .context("resolve the tmux owner before restart")?;
    kill()?;
    let next = probe(owner).and_then(|running| {
        if running {
            bail!("tmux session “{session}” is still running");
        }
        confirmed_tmux_respawn_target(session, |other| {
            if other == owner {
                Ok(false) // The owned session was just confirmed gone.
            } else {
                probe(other)
            }
        })
    });
    match next {
        Err(error) if tmux_missing(&error) => Ok(session.clone()),
        next => next,
    }
}

/// Run a prepared `kill-session` within `deadline`. Its exit status says nothing
/// (a session already gone fails too); the caller confirms the session is gone.
/// No tmux to run means no session muxel could stop.
fn kill_session_reply(cmd: Command, deadline: Instant) -> Result<()> {
    match run_tmux_until(cmd, deadline, &GitEnv::production()) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => bail!("tmux restart timed out"),
        Err(error) if tmux_missing(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// The tmux half of restarting a local pane: resolve the owner of its recorded
/// `session`, record it through `save_owner` before anything is stopped, kill
/// and confirm it, and pick the server its next session starts on. Returns that
/// owner and the next target, for the caller to save before relaunching.
pub fn restart_local_tmux(
    session: &SessionTarget,
    deadline: Instant,
    save_owner: impl FnOnce(&SessionTarget) -> Result<()>,
) -> Result<(SessionTarget, SessionTarget)> {
    let session = profile_tmux_target(session);
    restart_local_tmux_with(
        &session,
        |socket| local_tmux_has_session_confirmed(socket, &session.name, deadline),
        save_owner,
        |owner| kill_tmux_session_confirmed(owner, deadline),
    )
}

/// [`restart_local_tmux`] on `probe` (an exact-name `has-session` on one server)
/// and `kill` ([`kill_tmux_session_confirmed`]).
///
/// A pane on Default whose name `mxl` already holds can't start a session after
/// the kill (see [`SessionTarget::for_respawn`]), so that is checked first and
/// its agent is left running: killed, it would leave the pane with nothing.
///
/// With no tmux to run ([`tmux_missing`]) before anything is saved, nothing is
/// stopped or saved and the pane relaunches on its recorded binding, which
/// fails as it always did without tmux.
fn restart_local_tmux_with(
    session: &SessionTarget,
    mut probe: impl FnMut(&Socket) -> Result<bool>,
    save_owner: impl FnOnce(&SessionTarget) -> Result<()>,
    kill: impl FnOnce(&SessionTarget) -> Result<SessionTarget>,
) -> Result<(SessionTarget, SessionTarget)> {
    let checked = resolve_tmux_owner_with(session, &mut probe).and_then(|owner| {
        confirmed_tmux_respawn_target(&owner, |socket| {
            if socket == &Socket::Default {
                Ok(false) // As it will be once killed.
            } else {
                probe(socket)
            }
        })?;
        Ok(owner)
    });
    let owner = match checked {
        Err(error) if tmux_missing(&error) => return Ok((session.clone(), session.clone())),
        owner => owner?,
    };
    save_owner(&owner)?;
    let next = kill(&owner)?;
    Ok((owner, next))
}

pub fn restart_remote_tmux(
    loc: &RepoLoc,
    session: &SessionTarget,
    instance: uuid::Uuid,
    deadline: Instant,
) -> Result<SessionTarget> {
    let RepoLoc::Remote(conn) = loc else {
        bail!("not a remote project")
    };
    let session = profile_tmux_target(session);
    if session.socket.is_none() {
        bail!("resolve and save the tmux owner before restart")
    }
    if session.socket == Some(Socket::Default) {
        // As a local restart does: a namesake on `mxl` would stop the restart only
        // after the kill, with nothing started in the agent's place.
        let (target, sockets) = tmux_launch_inventory(&session, local_tmux_root().as_deref());
        let rows = ssh::parse_session_listings_for_migration_on(
            &remote_session_names(conn, &sockets, deadline)?,
            &sockets,
        )
        .context("couldn't confirm the tmux servers queried")?;
        confirmed_tmux_respawn_target(&target, |socket| {
            Ok(socket != &Socket::Default
                && rows
                    .iter()
                    .any(|row| &row.socket == socket && row.name == target.name))
        })?;
    }
    let cmd = remote_ssh_command(conn, ssh::kill_and_confirm_command(&session));
    let out = run_tmux_output_until(cmd, deadline)?;
    if !out.status.success() {
        bail!("{}", ssh_error_message(&out))
    }
    if session.socket == Some(Socket::Default) {
        remote_tmux_launch_target(loc, &session, instance, true, deadline)
    } else {
        let rows = ssh::parse_session_listings_for_migration_on(
            &remote_session_names(conn, session.socket.as_slice(), deadline)?,
            session.socket.as_slice(),
        )
        .context("couldn't confirm the tmux session's owner")?;
        if rows.iter().any(|row| row.name == session.name) {
            bail!("tmux session “{session}” is still running");
        }
        Ok(session.clone())
    }
}

/// Resolve a legacy close without mutating either server. The caller must save
/// this owner before its first kill and retain it across failures and retries.
pub fn resolve_remote_tmux_close(
    host: &RemoteHost,
    control_path: &str,
    password: Option<&str>,
    closed: &muxel_core::tmux::ClosedSession,
) -> Result<muxel_core::tmux::ClosedSession> {
    let mut closed = closed.clone();
    closed.socket = profile_tmux_target(&closed.target()).socket;
    if closed.socket.is_some() {
        return Ok(closed);
    }
    let sockets = local_tmux_sockets();
    let command = format!(
        "export LC_ALL=C; {}",
        ssh::list_session_names_command_on(&sockets)
    );
    let out = ssh_run(host, control_path, password, &command)?;
    if !out.status.success() {
        bail!("{}", ssh_error_message(&out))
    }
    let sessions = ssh::parse_session_listings_for_migration_on(
        &String::from_utf8_lossy(&out.stdout),
        &sockets,
    )
    .ok_or_else(|| anyhow::anyhow!("couldn't determine the tmux session's server"))?;
    Ok(closed.resolve_owner(&sessions, profile_tmux_socket()))
}

/// Kill a tmux session on a remote host over SSH, and **confirm it is gone**
/// (over the host's commands master, which outlives any pane's connection).
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
    session: &SessionTarget,
) -> Result<()> {
    let session = profile_tmux_target(session);
    if session.socket.is_none() {
        bail!("resolve and save the tmux session owner before teardown");
    }
    let out = ssh_run(
        host,
        control_path,
        password,
        &ssh::kill_and_confirm_command(&session),
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
    session: &SessionTarget,
    tag: &str,
) -> Result<()> {
    let session = profile_tmux_target(session);
    let out = ssh_run(
        host,
        control_path,
        password,
        &ssh::claim_window_size_command(&session, tag),
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
    session: &SessionTarget,
) {
    let remote_cmd = ssh::kill_command(&profile_tmux_target(session));
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

/// Fire-and-forget kill of a local tmux session on its owner (quit-time cleanup).
pub fn kill_local_tmux_detached(session: &SessionTarget) {
    let session = profile_tmux_target(session);
    let mut cmd = match &session.socket {
        Some(socket) => {
            let mut cmd = tmux_on(socket);
            cmd.args(muxel_core::tmux::kill_session_args(&session.name));
            cmd
        }
        None => {
            let mut cmd = command("sh");
            cmd.args(["-c", &ssh::kill_command(&session)]);
            cmd
        }
    };
    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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

/// How library git operations are run: which `git`, its environment and the
/// test hooks.
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
    /// Opt-in test receipts; no collector or clocks in production builds.
    #[cfg(test)]
    pub timings: Option<Arc<Mutex<GitTimings>>>,
}

struct GitChild {
    child: Child,
    #[cfg(test)]
    spawn_started: Instant,
    #[cfg(test)]
    spawn_finished: Instant,
}

pub type RenameFn = Arc<dyn Fn(&Path, &Path) -> std::io::Result<()> + Send + Sync>;

pub type RemoveDirFn = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

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
            #[cfg(test)]
            timings: None,
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

    fn spawn(&self, command: &mut Command) -> std::io::Result<GitChild> {
        #[cfg(test)]
        let spawn_started = Instant::now();
        let child = command.spawn()?;
        #[cfg(test)]
        let spawn_finished = Instant::now();
        self.spawned.fetch_add(1, Ordering::SeqCst);
        Ok(GitChild {
            child,
            #[cfg(test)]
            spawn_started,
            #[cfg(test)]
            spawn_finished,
        })
    }

    #[cfg(test)]
    fn record_timing(&self, receipt: GitTimingReceipt) {
        if let Some(timings) = &self.timings {
            timings.lock().unwrap().children.push(receipt);
        }
    }
}

/// Whether `url` is reached over SSH: `ssh://`, `git+ssh://`, `ssh+git://`, or
/// scp-like `[user@]host:path` (not a Windows drive letter).
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

/// `git config --get core.sshCommand` in `cwd`, with the same environment and
/// deadline as the operation it precedes.
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
    let Ok(child) = env.spawn(&mut cmd) else {
        return SshConfigProbe::Failed;
    };
    let (child, status) = env.wait_until(child, deadline);
    env.reap(child);
    match status.and_then(|s| s.code()) {
        Some(0) => SshConfigProbe::Set,
        Some(1) => SshConfigProbe::Unset,
        _ => SshConfigProbe::Failed,
    }
}

/// The pause before the next exit poll in [`GitEnv::wait_until`], after one of
/// `pause`: starting at a millisecond and doubling to 50 ms. A short command
/// (a tmux probe, run on the UI thread) is seen to exit within about its own
/// run time, rather than costing a whole 50 ms step; a long one is still polled
/// only every 50 ms.
fn next_exit_poll(pause: Option<Duration>) -> Duration {
    const FIRST: Duration = Duration::from_millis(1);
    const MAX: Duration = Duration::from_millis(50);
    pause.map_or(FIRST, |pause| (pause * 2).min(MAX))
}

impl GitEnv {
    /// Poll until exit or the original deadline, then kill and wait. Test
    /// receipts distinguish deadline expiry from a poll error and OS latency.
    fn wait_until(&self, spawned: GitChild, deadline: Instant) -> (Child, Option<ExitStatus>) {
        #[cfg(test)]
        let wait_entered = Instant::now();
        let mut child = spawned.child;
        #[cfg(test)]
        let mut stop = GitStopReason::Deadline;
        let mut pause = None;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    #[cfg(test)]
                    self.record_timing(GitTimingReceipt {
                        pid: child.id(),
                        deadline,
                        spawn_started: spawned.spawn_started,
                        spawn_finished: spawned.spawn_finished,
                        wait_entered,
                        stop: GitStopReason::Exited,
                        stop_observed: Instant::now(),
                        kill_started: None,
                        kill_finished: None,
                        wait_finished: Instant::now(),
                        wait_succeeded: false,
                    });
                    return (child, Some(status));
                }
                Ok(None) => {}
                Err(_) => {
                    #[cfg(test)]
                    {
                        stop = GitStopReason::PollError;
                    }
                    break;
                }
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let next = next_exit_poll(pause);
            pause = Some(next);
            std::thread::sleep((deadline - now).min(next));
        }
        #[cfg(test)]
        let stop_observed = Instant::now();
        #[cfg(test)]
        let kill_started = Instant::now();
        kill_process_tree(&mut child);
        #[cfg(test)]
        let kill_finished = Instant::now();
        let wait_succeeded = child.wait().is_ok();
        #[cfg(not(test))]
        let _ = wait_succeeded;
        #[cfg(test)]
        self.record_timing(GitTimingReceipt {
            pid: child.id(),
            deadline,
            spawn_started: spawned.spawn_started,
            spawn_finished: spawned.spawn_finished,
            wait_entered,
            stop,
            stop_observed,
            kill_started: Some(kill_started),
            kill_finished: Some(kill_finished),
            wait_finished: Instant::now(),
            wait_succeeded,
        });
        (child, None)
    }
}

/// Kill `child` and everything it started (git spawns helpers such as
/// `git-remote-https` and `ssh`).
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

const GIT_STDERR_TAIL: usize = 4096;

/// On Windows, `core.longpaths=true` lets git create and read paths past
/// `MAX_PATH` under a deep `LIB_DIR`. The key exists only in git for Windows.
#[cfg(windows)]
const LONG_PATHS: Option<&str> = Some("core.longpaths=true");
#[cfg(not(windows))]
const LONG_PATHS: Option<&str> = None;

/// Run `git <-c…> <args>` for a library operation on `url`, blocking until it
/// ends or `deadline` passes: no stdin, prompts or credential UI, SSH in batch
/// mode per [`ssh_batch_mode_needed`], and the process tree killed on the
/// deadline. `ceiling` as in [`GitEnv::base_command`].
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

    let mut child = match env.spawn(&mut cmd) {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(GitFailure::NotFound),
        Err(e) => {
            return Err(GitFailure::Io {
                detail: e.to_string(),
            });
        }
    };
    // Drain stderr on a thread (a full pipe would block git), keeping the tail.
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut stderr) = child.child.stderr.take() {
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

    let (child, status) = env.wait_until(child, deadline);
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

fn io_failure(e: &std::io::Error) -> GitFailure {
    GitFailure::Io {
        detail: e.to_string(),
    }
}

/// `git clone --single-branch` from `lib_dir`; on failure the half-made
/// `target` is removed.
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

/// The ceiling for a clone run from `lib_dir`: its parent, since git ignores a
/// ceiling equal to the working directory.
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

/// Clone `url` (`branch`, `""` = the remote's default) into `dest` through a
/// temporary sibling renamed into place, so a failed clone never leaves a
/// half-made `dest`.
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

/// `git pull --ff-only` and nothing else: no reset, stash, clean or checkout.
/// `url` is only used for the SSH rule. A clone folder without `.git` is
/// [`GitFailure::NotAClone`] without running git: pulling there would reach a
/// repository above `LIB_DIR`, and cloning over it would delete its contents.
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

/// Re-sync `LIB_DIR/<id>` with the remote: a full clone to `<id>.r-…`, then
/// `<id>` → `<id>.o-…`, the new clone → `<id>`, and the old one removed. On any
/// failure the clone is left as it was, except `ResyncRestoreFailed`, where
/// `<id>` is missing and `<id>.o-…` remains.
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
/// `GIT_OPTIONAL_LOCKS=0` keeps `status` from rewriting the index, and
/// `core.fsmonitor=false` keeps it from starting a daemon.
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
/// read error or past `deadline`.
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

/// Entries of `git status --porcelain=v1 -z`. A rename or copy (`R`/`C`) adds
/// its source path as one more field, which is not another entry.
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

/// Run git `args` in `clone` for the local-changes check and return its stdout;
/// `None` if git is missing, fails, or `deadline` passes.
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
    let mut child = env.spawn(&mut cmd).ok()?;
    // Drain stdout on a thread so a large status never blocks git.
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut stdout) = child.child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut out = Vec::new();
            let ok = stdout.read_to_end(&mut out).is_ok();
            let _ = tx.send(ok.then_some(out));
        });
    }
    let (child, status) = env.wait_until(child, deadline);
    env.reap(child);
    if !status?.success() {
        return None;
    }
    let left = deadline.saturating_duration_since(Instant::now());
    rx.recv_timeout(left.max(Duration::from_millis(100)))
        .ok()
        .flatten()
}

/// Delete `dir`; if that fails, clear the read-only attribute of everything
/// under it (git's packfiles are read-only on Windows) and try once more.
pub fn remove_dir_force(env: &GitEnv, dir: &Path) -> std::io::Result<()> {
    if (env.remove_dir)(dir).is_ok() {
        return Ok(());
    }
    clear_read_only(dir);
    (env.remove_dir)(dir)
}

/// Make `path` and everything under it writable, without following symlinks.
/// Best effort.
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
    fn launch_inventory_pins_the_profile_and_preserves_named_owner_coverage() {
        let root = Path::new("/isolated-migration");
        let isolated = Socket::isolated(root);
        for prior in [
            None,
            Some(Socket::Private),
            Some(Socket::Default),
            Some(Socket::Isolated(1)),
            Some(Socket::Named("peer '名".into())),
        ] {
            let saved = SessionTarget::new("exact name ", prior);
            let (target, sockets) = tmux_launch_inventory(&saved, Some(root));
            assert_eq!(target.name, saved.name);
            assert_eq!(target.socket, Some(isolated.clone()));
            assert_eq!(sockets, std::slice::from_ref(&isolated));
            assert_eq!(
                confirmed_tmux_respawn_target(&target, |_| panic!("global migration probe"))
                    .unwrap(),
                target
            );
            let reply = "exact name \n:muxel-listed:0\n";
            let rows = ssh::parse_session_listings_for_migration_on(reply, &sockets).unwrap();
            assert_eq!(rows[0].target(), target);
            assert!(ssh::parse_session_listings_for_migration_on(reply, &Socket::BOTH).is_none());
        }
        let named = SessionTarget::new("work", Some(Socket::Named("peer '名".into())));
        let (target, sockets) = tmux_launch_inventory(&named, None);
        assert_eq!(target, named);
        assert_eq!(sockets, named.socket.as_slice());
        for owner in [None, Some(Socket::Private), Some(Socket::Default)] {
            let saved = SessionTarget::new("work", owner);
            assert_eq!(
                tmux_launch_inventory(&saved, None),
                (saved, Socket::BOTH.to_vec())
            );
        }
    }

    fn hanging_tmux_command() -> Command {
        if cfg!(windows) {
            let powershell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32/WindowsPowerShell/v1.0/powershell.exe");
            let mut cmd = command(powershell);
            cmd.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ]);
            cmd
        } else {
            let mut cmd = command("/bin/sh");
            cmd.args(["-c", "sleep 30"]);
            cmd
        }
    }

    fn assert_one_tmux_child_reaped(lifecycle: &GitEnv) {
        assert_eq!(lifecycle.spawned.load(Ordering::SeqCst), 1);
        let mut reaped = lifecycle.reaped.as_ref().unwrap().lock().unwrap();
        assert_eq!(reaped.len(), 1);
        assert!(reaped[0].try_wait().unwrap().is_some());
    }

    #[test]
    fn claim_cleanup_stops_a_command_that_does_not_finish() {
        let lifecycle = GitEnv::for_tests();
        let started = Instant::now();
        let err = run_claim_cleanup(
            hanging_tmux_command(),
            started + Duration::from_millis(50),
            &lifecycle,
        )
        .expect_err("a 30-second command must not finish within 50 ms");
        assert!(err.to_string().contains("timed out"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_one_tmux_child_reaped(&lifecycle);
    }

    #[test]
    fn migration_probe_timeout_reaps_the_child_and_does_not_authorize_absence() {
        let lifecycle = GitEnv::for_tests();
        let started = Instant::now();
        let error = run_tmux_output_with(
            hanging_tmux_command(),
            started + Duration::from_millis(50),
            &lifecycle,
        )
        .expect_err("a hung query cannot confirm an empty server");
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_one_tmux_child_reaped(&lifecycle);
    }

    #[test]
    fn migration_probe_expired_deadline_does_not_spawn() {
        let lifecycle = GitEnv::for_tests();
        assert!(run_tmux_output_with(hanging_tmux_command(), Instant::now(), &lifecycle).is_err());
        assert_eq!(lifecycle.spawned.load(Ordering::SeqCst), 0);
    }

    /// A probe that exits at once is seen within about its own run time, not a
    /// whole 50 ms poll step; a long wait still polls every 50 ms.
    #[test]
    fn exit_polls_start_short_and_back_off_to_fifty_ms() {
        let mut pauses = Vec::new();
        let mut pause = None;
        for _ in 0..9 {
            let next = next_exit_poll(pause);
            pauses.push(next.as_millis());
            pause = Some(next);
        }
        assert_eq!(pauses, [1, 2, 4, 8, 16, 32, 50, 50, 50]);
    }

    /// A spawn that fails with NotFound, as running a tmux muxel can't find does.
    fn tmux_not_found() -> anyhow::Error {
        anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound))
            .context("starting tmux probe")
    }

    /// No tmux to run: a kill has nothing muxel could stop, but a probe is no
    /// answer, so it never authorizes a move off Default. A tmux outside the
    /// `PATH` muxel started with (MacPorts' `/opt/local/bin` when muxel is opened
    /// from the Dock, say) may still be running the agent there.
    #[test]
    fn a_missing_tmux_never_moves_a_pane_off_default() {
        let missing = || command("muxel-test-no-such-tmux");
        let deadline = Instant::now() + Duration::from_secs(5);
        let error = has_session_reply(missing(), "work", deadline).unwrap_err();
        assert!(tmux_missing(&error), "{error:#}");
        kill_session_reply(missing(), deadline).unwrap();
        assert!(tmux_missing(&tmux_not_found()));
        assert!(!tmux_missing(&anyhow::anyhow!("tmux probe timed out")));

        let on_default = SessionTarget::new("owned", Some(Socket::Default));
        let legacy = SessionTarget::new("owned", None);
        // Launch: the recorded binding is kept, so the launch fails on it as before.
        for session in [&on_default, &legacy] {
            assert_eq!(
                local_tmux_launch_target_with(session, |_| Err(tmux_not_found())).unwrap(),
                *session
            );
        }
        // Any other probe failure still stops the launch.
        assert!(
            local_tmux_launch_target_with(&on_default, |_| Err(anyhow::anyhow!("denied"))).is_err()
        );
        // Restart, before anything is saved: nothing is saved or killed.
        for session in [&on_default, &legacy] {
            let (result, _, steps) = local_restart_on(session.clone(), |_| Err(tmux_not_found()));
            assert_eq!(result.unwrap(), (session.clone(), session.clone()));
            assert!(steps.is_empty(), "{steps:?}");
        }
        // Restart, once killed: the confirmation can't say Default is safe to leave.
        let mut killed = false;
        let next = kill_tmux_session_confirmed_with(
            &on_default,
            || {
                killed = true;
                Ok(())
            },
            |_| Err(tmux_not_found()),
        )
        .unwrap();
        assert!(killed);
        assert_eq!(next, on_default);
        // tmux there, the same confirmation moves it once both servers answer.
        let next = kill_tmux_session_confirmed_with(&on_default, || Ok(()), |_| Ok(false)).unwrap();
        assert_eq!(next.socket, Some(Socket::Private));
    }

    /// The message a namesake refusal shows points at what actually ends it.
    #[test]
    fn a_namesake_refusal_names_a_way_out_that_exists() {
        let target = SessionTarget::new("owned", Some(Socket::Default));
        let error = confirmed_tmux_respawn_target(&target, |socket| Ok(socket == &Socket::Private))
            .unwrap_err()
            .to_string();
        assert!(error.contains("sharing this project's layout"), "{error}");
        assert!(error.contains("restart the pane"), "{error}");
        assert!(!error.contains("Refresh the project layout"), "{error}");
    }

    /// The order a local restart works in, on fake servers: `probes` records each
    /// `has-session` (server, answer), `steps` the save and the kill.
    fn local_restart(
        session: SessionTarget,
        running: &[Socket],
        save_ok: bool,
    ) -> (
        Result<(SessionTarget, SessionTarget)>,
        Vec<Socket>,
        Vec<&'static str>,
    ) {
        local_restart_with(session, |socket| Ok(running.contains(socket)), save_ok)
    }

    /// [`local_restart`] on any `answer`, with the save succeeding.
    fn local_restart_on(
        session: SessionTarget,
        answer: impl Fn(&Socket) -> Result<bool>,
    ) -> (
        Result<(SessionTarget, SessionTarget)>,
        Vec<Socket>,
        Vec<&'static str>,
    ) {
        local_restart_with(session, answer, true)
    }

    fn local_restart_with(
        session: SessionTarget,
        answer: impl Fn(&Socket) -> Result<bool>,
        save_ok: bool,
    ) -> (
        Result<(SessionTarget, SessionTarget)>,
        Vec<Socket>,
        Vec<&'static str>,
    ) {
        let probes = std::cell::RefCell::new(Vec::new());
        let steps = std::cell::RefCell::new(Vec::new());
        let probe = |socket: &Socket| {
            probes.borrow_mut().push(socket.clone());
            answer(socket)
        };
        let result = restart_local_tmux_with(
            &session,
            probe,
            |_| {
                steps.borrow_mut().push("save");
                if save_ok {
                    Ok(())
                } else {
                    bail!("couldn't save the tmux session owner before restart")
                }
            },
            |owner| {
                steps.borrow_mut().push("kill");
                kill_tmux_session_confirmed_with(
                    owner,
                    || Ok(()),
                    |socket| {
                        // The owner's session is gone once killed.
                        Ok(socket != owner.socket.as_ref().unwrap() && answer(socket)?)
                    },
                )
            },
        );
        (result, probes.into_inner(), steps.into_inner())
    }

    #[test]
    fn a_local_restart_saves_the_owner_before_it_kills() {
        let named = Socket::Named("peer".into());
        for socket in [Socket::Private, named.clone()] {
            let target = SessionTarget::new("work", Some(socket.clone()));
            let (result, probes, steps) =
                local_restart(target.clone(), std::slice::from_ref(&socket), true);
            assert_eq!(result.unwrap(), (target.clone(), target.clone()));
            assert!(probes.is_empty(), "a recorded owner needs no lookup");
            assert_eq!(steps, ["save", "kill"]);

            // A failed save stops before anything is killed.
            let (result, _, steps) = local_restart(target, &[socket], false);
            assert!(format!("{:#}", result.unwrap_err()).contains("couldn't save"));
            assert_eq!(steps, ["save"]);
        }
        // A legacy binding is resolved first, and that owner is what gets saved.
        let (result, probes, steps) =
            local_restart(SessionTarget::new("work", None), &[Socket::Default], true);
        let (owner, next) = result.unwrap();
        assert_eq!(owner.socket, Some(Socket::Default));
        assert_eq!(next.socket, Some(Socket::Private));
        assert_eq!(probes[..2], [Socket::Private, Socket::Default]);
        assert_eq!(steps, ["save", "kill"]);
    }

    /// With a live Default session and a namesake on `mxl`, the kill would leave the
    /// pane with nothing to start: the restart refuses first, agent untouched.
    #[test]
    fn a_local_restart_checks_for_a_namesake_before_it_kills() {
        let target = SessionTarget::new("owned", Some(Socket::Default));
        let (result, probes, steps) =
            local_restart(target.clone(), &[Socket::Default, Socket::Private], true);
        assert!(result.unwrap_err().to_string().contains("restart the pane"));
        assert_eq!(probes, [Socket::Private]);
        assert!(steps.is_empty(), "neither saved nor killed");

        // Without one, it moves off Default once the old session is gone.
        let (result, _, steps) = local_restart(target.clone(), &[Socket::Default], true);
        let (owner, next) = result.unwrap();
        assert_eq!(owner, target);
        assert_eq!(next, SessionTarget::new("owned", Some(Socket::Private)));
        assert_eq!(steps, ["save", "kill"]);
    }

    #[test]
    fn claim_cleanup_deadline_includes_a_hung_socket_probe() {
        let lifecycle = GitEnv::for_tests();
        let started = Instant::now();
        let mut probed = Vec::new();
        let err = bounded_tmux_socket(
            &"work".into(),
            started + Duration::from_millis(50),
            &lifecycle,
            |socket| {
                probed.push(socket);
                hanging_tmux_command()
            },
        )
        .expect_err("socket discovery must obey the cleanup deadline");
        assert!(err.to_string().contains("timed out"), "{err:#}");
        assert_eq!(
            probed,
            [Socket::Private],
            "a failed probe must not fall through to another server"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_one_tmux_child_reaped(&lifecycle);
    }

    #[test]
    fn claim_cleanup_reuses_the_known_socket_without_probing() {
        let lifecycle = GitEnv::for_tests();
        let target = SessionTarget::new("work", Some(Socket::Default));
        let socket = bounded_tmux_socket(
            &target,
            Instant::now() + Duration::from_secs(2),
            &lifecycle,
            |_| panic!("a known owner must not probe either server"),
        )
        .unwrap();
        assert_eq!(socket, Socket::Default);
        assert_eq!(lifecycle.spawned.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn negative_tmux_capability_reprobes_after_upgrade_and_caches_success() {
        let supported = std::sync::OnceLock::new();
        for version in ["tmux 3.1c", "unknown", "tmux 3.2a"] {
            assert_eq!(
                cached_tmux_session_env_support(&supported, || {
                    muxel_core::tmux::session_env_supported(version)
                }),
                muxel_core::tmux::session_env_supported(version)
            );
        }
        assert!(cached_tmux_session_env_support(&supported, || panic!(
            "positive result should be cached"
        )));
    }

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

    /// The shared-memory failure modes found in real projects: agent sections never
    /// stamped (so they never aged out), a panel edit saving over sections added
    /// since it loaded, and a failed read treated as an empty memory.
    #[test]
    fn local_memory_is_stamped_kept_and_never_saved_over() {
        use muxel_core::memory::{Edit, MemoryEntry};
        use std::io::Write as _;

        const DAY: i64 = 86_400;
        let now = 1_800_057_600 + 3600;
        let root = std::env::temp_dir().join("muxel-it-memory-upkeep");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let loc = RepoLoc::Local(root.clone());
        let file = root.join(MEMORY_DIR).join(MEMORY_FILE);

        assert!(
            maintain_memory(&loc, now).unwrap().is_empty(),
            "no file: empty"
        );
        ensure_memory_file(&loc).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        writeln!(f, "\n## Agent lesson\nuse the private socket").unwrap();
        drop(f);

        // Stamped on disk the first time muxel maintains it, then left alone.
        let kept = maintain_memory(&loc, now).unwrap();
        assert_eq!(kept.len(), 1);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("created=2027-01-16; accessed=2027-01-16"),
            "{text}"
        );
        maintain_memory(&loc, now).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "stable");

        // An agent adds a section after the panel loaded; pinning from the panel
        // must not erase it.
        let lesson = kept[0].id;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(b"\n## Second lesson\nadded while the panel was open\n")
            .unwrap();
        let after = edit_memory(&loc, now, &Edit::TogglePin(lesson)).unwrap();
        let titles: Vec<_> = after.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Agent lesson", "Second lesson"]);
        assert!(after[0].pinned);

        // It ages out 30 days on; the pinned one stays.
        let later = maintain_memory(&loc, now + 31 * DAY).unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].title, "Agent lesson");

        // An unreadable memory is an error — and an edit then writes nothing.
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap(); // a directory can't be read as text
        assert!(maintain_memory(&loc, now).is_err());
        let add = Edit::Add(MemoryEntry::new("x", "y", Vec::new(), now));
        assert!(edit_memory(&loc, now, &add).is_err());
        assert!(file.is_dir(), "nothing was saved over it");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn local_accomplishments_maintain_only_rewrites_when_needed() {
        use muxel_core::accomplishments::{Accomplishment, AccomplishmentKind};
        use std::io::Write as _;

        const DAY: i64 = 86_400;
        let now = 1_800_057_600 + 3600; // 2027-01-16, local wall clock
        let root = std::env::temp_dir().join("muxel-it-accomplishments");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let loc = RepoLoc::Local(root.clone());
        let file = root.join(MEMORY_DIR).join(ACCOMPLISHMENTS_FILE);

        // No file yet: an empty log, not an error, and nothing is created.
        assert!(maintain_accomplishments(&loc, now, 14).unwrap().is_empty());
        assert!(!file.exists());

        ensure_accomplishments_file(&loc).expect("seed");
        let gi = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(gi.lines().any(|l| l.trim() == ".muxel/"), "gitignored");

        // Agents append: one dated and current, one stale, one undated.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        writeln!(f, "- 2027-01-15 | fix | Fixed the race | claude").unwrap();
        writeln!(f, "- 2026-12-01 | feature | Ancient history | codex").unwrap();
        writeln!(f, "- docs | Wrote the guide | amp").unwrap();
        drop(f);

        let kept = maintain_accomplishments(&loc, now, 14).unwrap();
        let summaries: Vec<_> = kept.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summaries, ["Fixed the race", "Wrote the guide"]);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("Ancient history"), "pruned on disk");
        assert!(
            text.contains("- 2027-01-16 | docs | Wrote the guide | amp"),
            "stamped"
        );

        // Clean now: a second load leaves the file byte-for-byte alone.
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        maintain_accomplishments(&loc, now, 14).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);

        // A panel edit applies to what's on disk, keeping a line appended since.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(b"- 2027-01-16 | perf | Faster startup | claude\n")
            .unwrap();
        let after = edit_accomplishments(&loc, now, 14, |es| {
            es.retain(|e| e.summary != "Fixed the race");
            es.push(Accomplishment::new(
                AccomplishmentKind::Test,
                "Added tests",
                None,
                now,
            ));
        })
        .unwrap();
        let summaries: Vec<_> = after.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(
            summaries,
            ["Wrote the guide", "Faster startup", "Added tests"]
        );

        // An agent corrects an entry by appending a line with its key — muxel
        // folds it into the original on the next load, without the agent reading.
        let key = after
            .iter()
            .find(|e| e.summary == "Faster startup")
            .and_then(|e| e.key.clone())
            .expect("muxel keys every entry");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(
                format!("- 2027-01-16 | perf | Faster startup and shutdown | claude | #{key}\n")
                    .as_bytes(),
            )
            .unwrap();
        let corrected = maintain_accomplishments(&loc, now, 14).unwrap();
        let summaries: Vec<_> = corrected.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(
            summaries,
            [
                "Wrote the guide",
                "Faster startup and shutdown",
                "Added tests"
            ]
        );
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            text.matches(&format!("#{key}")).count(),
            1,
            "folded on disk"
        );

        // A shorter window prunes more on the next load.
        assert_eq!(
            maintain_accomplishments(&loc, now + DAY, 1).unwrap().len(),
            0
        );

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
        let env = GitEnv {
            timings: Some(Default::default()),
            ..GitEnv::for_tests()
        };
        let start = Instant::now();
        let deadline = start + Duration::from_secs(2);
        let res = library_resync(
            &env,
            &server.url("team-lib.git"),
            "",
            &lib_dir,
            id,
            deadline,
        );
        let finished = Instant::now();
        assert!(matches!(res, Err(GitFailure::TimedOut { .. })), "{res:?}");
        assert_all_reaped(&env);
        assert_eq!(lib_snapshot(&dest), before_files);
        assert_eq!(lib_head(&dest), before_head);
        assert_eq!(lib_entries(&lib_dir), vec![id.to_string()]);
        crate::test_support::assert_git_timeout(&env, deadline, finished);
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
