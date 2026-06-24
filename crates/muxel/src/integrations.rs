//! Side-effecting wrappers around the `git` and `tmux` CLIs — the I/O half of
//! muxel-core's pure tmux/worktree helpers.

use anyhow::{Context, Result, bail};
use muxel_core::{MEMORY_DIR, MEMORY_FILE, RemoteHost, SshAuth, memory_header, ssh};
use std::path::{Path, PathBuf};
use std::process::Command;

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
    argv.push("-o".into());
    argv.push("ConnectTimeout=10".into());
    if c.password.is_none() {
        argv.push("-o".into());
        argv.push("BatchMode=yes".into());
    }
    argv.push(ssh::target(&c.host));
    argv.push("--".into());
    argv.push(remote_cmd);
    if c.host.auth == SshAuth::Password {
        let mut cmd = Command::new("sshpass");
        cmd.arg("-e").arg("ssh").args(&argv);
        if let Some(pw) = &c.password {
            cmd.env("SSHPASS", pw);
        }
        cmd
    } else {
        let mut cmd = Command::new("ssh");
        cmd.args(&argv);
        cmd
    }
}

/// Run `git <args>` at a [`RepoLoc`]: locally (`git -C <path>`) or on the host
/// over SSH (`ssh … -- git -C <remote_path> …`, fed as one quoted command).
fn git_output(loc: &RepoLoc, args: &[&str]) -> std::io::Result<std::process::Output> {
    match loc {
        RepoLoc::Local(path) => Command::new("git").arg("-C").arg(path).args(args).output(),
        RepoLoc::Remote(c) => {
            let mut remote = format!("git -C {}", ssh::sh_quote(&c.remote_path));
            for a in args {
                remote.push(' ');
                remote.push_str(&ssh::sh_quote(a));
            }
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
    let q = ssh::sh_quote(root);
    let cmd = format!(
        "cd {q} && (git ls-files --cached --others --exclude-standard 2>/dev/null \
         || find . -type f -not -path '*/.git/*') | head -n 10000"
    );
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
    let p = ssh::sh_quote(abs_path);
    // Only cat when it's a regular file within the size cap.
    let cmd =
        format!("if [ -f {p} ] && [ \"$(wc -c < {p})\" -le {MAX_REMOTE_BYTES} ]; then cat {p}; fi");
    let out = remote_ssh_command(c, cmd).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Write `content` to a remote file (overwriting), piping it over SSH stdin.
pub fn write_remote_file(loc: &RepoLoc, abs_path: &str, content: &str) -> Result<()> {
    let RepoLoc::Remote(c) = loc else {
        bail!("not a remote file");
    };
    use std::io::Write;
    let cmd = format!("cat > {}", ssh::sh_quote(abs_path));
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
            let gitignore = root.join(".gitignore");
            let current = std::fs::read_to_string(&gitignore).unwrap_or_default();
            let ignored = current
                .lines()
                .any(|l| l.trim() == ignore_line || l.trim() == MEMORY_DIR);
            if !ignored {
                let mut next = current;
                if !next.is_empty() && !next.ends_with('\n') {
                    next.push('\n');
                }
                next.push_str(&ignore_line);
                next.push('\n');
                std::fs::write(&gitignore, next)
                    .with_context(|| format!("updating {}", gitignore.display()))?;
            }
            Ok(())
        }
        RepoLoc::Remote(c) => {
            let root = c.remote_path.trim_end_matches('/');
            let file = format!("{MEMORY_DIR}/{MEMORY_FILE}");
            let cmd = format!(
                "cd {root} && mkdir -p {dir} && {{ test -f {file} || printf '%s' {hdr} > {file}; }} \
                 && {{ grep -qxF {ign} .gitignore 2>/dev/null || printf '%s\\n' {ign} >> .gitignore; }}",
                root = ssh::sh_quote(root),
                dir = ssh::sh_quote(MEMORY_DIR),
                file = ssh::sh_quote(&file),
                hdr = ssh::sh_quote(memory_header()),
                ign = ssh::sh_quote(&ignore_line),
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

/// The shell command that prepares the remote for a layout push: ensure
/// `<root>/.muxel/` exists, back up any current `workspace.json` to
/// `workspace.bak.json`, and git-ignore `.muxel/`. Pure (no I/O) so its shape is
/// unit-testable; mirrors `ensure_memory_file`'s remote branch.
fn remote_push_prep_cmd(root: &str) -> String {
    let rel = format!("{MEMORY_DIR}/{REMOTE_LAYOUT_FILE}");
    let bak = format!("{MEMORY_DIR}/{REMOTE_LAYOUT_BAK}");
    let ignore_line = format!("{MEMORY_DIR}/");
    format!(
        "cd {root} && mkdir -p {dir} && {{ test -f {rel} && cp -f {rel} {bak} || true; }} \
         && {{ grep -qxF {ign} .gitignore 2>/dev/null || printf '%s\\n' {ign} >> .gitignore; }}",
        root = ssh::sh_quote(root.trim_end_matches('/')),
        dir = ssh::sh_quote(MEMORY_DIR),
        rel = ssh::sh_quote(&rel),
        bak = ssh::sh_quote(&bak),
        ign = ssh::sh_quote(&ignore_line),
    )
}

/// Read the remote project's synced layout JSON (`<root>/.muxel/workspace.json`),
/// or `None` for a local loc / missing / oversized / unreachable.
pub fn fetch_remote_layout(loc: &RepoLoc) -> Option<String> {
    let RepoLoc::Remote(c) = loc else {
        return None;
    };
    read_remote_file(loc, &remote_layout_abs(&c.remote_path))
}

/// Push the project's pane-layout JSON to `<root>/.muxel/workspace.json` on the
/// remote, backing up the previous copy to `workspace.bak.json` first and
/// ensuring `.muxel/` is git-ignored. `json` is produced by
/// `muxel_core::RemoteLayout::to_json`.
pub fn push_remote_layout(loc: &RepoLoc, json: &str) -> Result<()> {
    let RepoLoc::Remote(c) = loc else {
        bail!("not a remote project");
    };
    let root = c.remote_path.trim_end_matches('/');
    let out = remote_ssh_command(c, remote_push_prep_cmd(root))
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
    argv.push("-o".into());
    argv.push("ConnectTimeout=10".into());
    if password.is_none() {
        argv.push("-o".into());
        argv.push("BatchMode=yes".into());
    }
    argv.push(ssh::target(host));
    argv.push("--".into());
    argv.push(remote_cmd.to_string());

    let mut cmd;
    if host.auth == SshAuth::Password {
        cmd = Command::new("sshpass");
        cmd.arg("-e").arg("ssh").args(&argv);
        if let Some(pw) = password {
            cmd.env("SSHPASS", pw);
        }
    } else {
        cmd = Command::new("ssh");
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
    let mut argv = ssh::base_args(host);
    argv.push("-o".into());
    argv.push("ControlPath=none".into()); // never multiplex a credential test
    argv.push("-o".into());
    argv.push("ConnectTimeout=10".into());
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
        cmd = Command::new("sshpass");
        cmd.arg("-e").arg("ssh").args(&argv);
        if let Some(pw) = password {
            cmd.env("SSHPASS", pw);
        }
    } else {
        cmd = Command::new("ssh");
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
        &format!("test -d {}", ssh::sh_quote(dir)),
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

/// Whether `path` is inside a git working tree.
pub fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a worktree at `worktree_path` on a new `branch`, based on `repo`.
pub fn create_worktree(repo: &Path, worktree_path: &Path, branch: &str) -> Result<()> {
    let output = Command::new("git")
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
    Command::new("git")
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
    let out = Command::new("git")
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
    Command::new("git")
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
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge", "--no-edit", branch])
        .output()
        .context("running `git merge`")?;
    if !out.status.success() {
        // Leave no half-finished merge behind.
        let _ = Command::new("git")
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
    let _ = Command::new("git")
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
    let out = Command::new("git")
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
    let out = Command::new("gh")
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
        Command::new("git")
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
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .output();
    let _ = Command::new("git")
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
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("--no-pager")
            .args(args)
            .output()
    };

    // Resolve the repo root (also our "is this a git repo?" check, in one call).
    let toplevel = match git(&["rev-parse", "--show-toplevel"]) {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return format!("# {}\n\nNot a git repository.\n", dir.display()),
    };

    // A header so it's always obvious which folder the diff is reading from, and
    // a heads-up when that folder is only a subdirectory of a larger repo.
    let mut header = format!("# Changes in {}\n", dir.display());
    let is_subdir = matches!(
        (dir.canonicalize(), Path::new(&toplevel).canonicalize()),
        (Ok(d), Ok(t)) if d != t
    );
    if is_subdir {
        header.push_str(&format!(
            "# (subfolder of git repo {toplevel} — showing changes under this folder only)\n"
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
        out.push_str("\n… diff truncated …\n");
    }
    if out.trim().is_empty() {
        format!("{header}No changes.")
    } else {
        format!("{header}{out}")
    }
}

/// Kill a tmux session. Best-effort.
pub fn kill_tmux_session(session: &str) {
    let _ = Command::new("tmux")
        .args(muxel_core::tmux::kill_session_args(session))
        .output();
}

/// Kill a tmux session on a remote host over SSH (best-effort; reuses the host's
/// ControlMaster, which is still alive right after the pane's ssh closed).
pub fn kill_remote_tmux(
    host: &RemoteHost,
    control_path: &str,
    password: Option<&str>,
    session: &str,
) {
    let target = format!("={session}"); // exact-match target, as in kill_session_args
    let cmd = format!("tmux kill-session -t {}", ssh::sh_quote(&target));
    let _ = ssh_exec(host, control_path, password, &cmd);
}

/// Open the OS file manager at (or selecting) `path`. Best-effort, cross-platform.
pub fn reveal_in_file_manager(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg("-R").arg(path).output();
    #[cfg(target_os = "windows")]
    let _ = Command::new("explorer")
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
        let _ = Command::new("xdg-open").arg(dir).output();
    }
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
            Command::new("git")
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
            Command::new("git")
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
            Command::new("git")
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
    fn remote_push_prep_cmd_backs_up_and_gitignores() {
        // `sh_quote` leaves quote-safe tokens (paths, `.muxel/`) bare.
        let cmd = remote_push_prep_cmd("/srv/app/");
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
            Command::new("git")
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
        let show = Command::new("git")
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
}
