//! The skill manager's I/O: scanning a machine's skills folders, fetching skills
//! from a git repository, and carrying out the placement planner's operations,
//! on this computer or an SSH host. Blocking — run it off the UI thread.
//!
//! The decisions (which folders, which operations) are made by
//! [`muxel_core::skills`]; this module only does what they say, refusing any
//! operation outside a known skills folder or on a synced/bundled skill.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use muxel_core::RemoteOs;
use muxel_core::library::{GIT_TIMEOUT_CLONE_SECS, GitFailure};
use muxel_core::skills::agents::{HOME_ENV_VARS, parse_hermes_manifest};
use muxel_core::skills::discover::{FoundSkill, Skipped, discover};
use muxel_core::skills::frontmatter::install_name;
use muxel_core::skills::inventory::{ScanEntry, SkillRow, build};
use muxel_core::skills::lock::{
    LockEntry, LockError, SkillLock, parse_lock, upsert, write_lock, write_sidecar,
};
use muxel_core::skills::placement::{
    DiffError, LinkMode, PlanError, SkillOp, diff, plan, remove_all, update_ops,
};
use muxel_core::skills::remote::{self as rops, ScanRequest, keep_skill_folders};
use muxel_core::skills::source::{self, SkillSource, SourceError, SourceKind};
use muxel_core::skills::tar::{TarFile, ustar};
use muxel_core::skills::{
    COPY_EXCLUDES, MAX_SKILL_BYTES, MAX_SKILL_FILES, MAX_SKILL_MD_BYTES, SCAN_DEPTH, SIDECAR_FILE,
    SKILL_FILE, Scope, SkillAgent, SkillHome, file_name, is_within, join, norm, relative_path,
    resolve_link,
};

use crate::i18n::{t, tf};
use crate::integrations::{
    GitEnv, RemoteConn, pipe_to_remote, remote_ssh_command, remove_dir_force, run_git, temp_suffix,
};

/// Most files listed from a fetched repository.
const MAX_REPO_FILES: usize = 100_000;
/// Fetch folders older than this are leftovers of an earlier run.
const STALE_FETCH: Duration = Duration::from_secs(60 * 60);

// --- Machines ---

/// Where skills are managed.
pub enum Machine {
    Local,
    Remote(Box<RemoteConn>),
}

/// A machine, connected: its folders and the agent programs on its `PATH`.
pub struct Host {
    pub machine: Machine,
    pub home: SkillHome,
    programs: BTreeSet<SkillAgent>,
}

impl Host {
    fn link_mode(&self) -> LinkMode {
        if self.home.os.is_windows() {
            LinkMode::Copy
        } else {
            LinkMode::Symlink
        }
    }
}

/// Connect: locally read the environment; remotely probe the host.
pub fn connect(machine: Machine) -> Result<Host, String> {
    match machine {
        Machine::Local => {
            let home =
                local_home().ok_or_else(|| t("Couldn't find your home folder.").to_string())?;
            Ok(connect_local_with(home, &crate::app::program_on_path))
        }
        Machine::Remote(c) => {
            let programs: Vec<&str> = SkillAgent::ALL.iter().map(|a| a.program()).collect();
            let out = run_remote(&c, rops::probe(c.host.os, &programs))?;
            let probe = rops::parse_probe(&out)
                .ok_or_else(|| t("The host didn't report its home folder.").to_string())?;
            let mut home = SkillHome::new(probe.home, c.host.os);
            home.env = probe.env;
            let programs = probe
                .programs
                .iter()
                .filter_map(|p| SkillAgent::from_program(p))
                .collect();
            Ok(Host {
                machine: Machine::Remote(c),
                home,
                programs,
            })
        }
    }
}

/// A local host with a given home and `PATH` check (tests use fake ones).
pub fn connect_local_with(home: SkillHome, on_path: &dyn Fn(&str) -> bool) -> Host {
    let programs = SkillAgent::ALL
        .into_iter()
        .filter(|a| on_path(a.program()))
        .collect();
    Host {
        machine: Machine::Local,
        home,
        programs,
    }
}

fn local_home() -> Option<SkillHome> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())?;
    let os = if cfg!(windows) {
        RemoteOs::Windows
    } else {
        RemoteOs::Unix
    };
    let mut h = SkillHome::new(home.to_string_lossy(), os);
    for v in HOME_ENV_VARS {
        if let Ok(val) = std::env::var(v) {
            h = h.with_env(v, val);
        }
    }
    Some(h)
}

fn run_remote(c: &RemoteConn, cmd: String) -> Result<String, String> {
    let out = remote_ssh_command(c, rops::for_host(c.host.os, cmd))
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| tf("Couldn't run ssh: {detail}", &[("detail", &e.to_string())]))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if msg.is_empty() {
            t("The command on the host failed.").to_string()
        } else {
            clean(&msg)
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pipe_remote(c: &RemoteConn, cmd: String, input: &[u8]) -> Result<(), String> {
    pipe_to_remote(c, rops::for_host(c.host.os, cmd), input, "failed")
        .map(|_| ())
        .map_err(|e| clean(&e.to_string()))
}

fn clean(s: &str) -> String {
    muxel_core::library::sanitize::sanitize(s)
}

// --- Scanning ---

/// Everything the page shows for one machine and scope.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub home: SkillHome,
    pub scope: Scope,
    /// Installed agents that load skills, in [`SkillAgent::ALL`] order.
    pub detected: Vec<SkillAgent>,
    pub rows: Vec<SkillRow>,
    /// The `skills` CLI's lock, or why it can't be used.
    pub lock: Result<SkillLock, LockError>,
    pub hermes_bundled: Vec<String>,
}

impl Snapshot {
    pub fn row(&self, key: &str) -> Option<&SkillRow> {
        self.rows.iter().find(|r| r.key == key)
    }
}

/// Scan every agent's skills folders at `scope`.
pub fn scan(host: &Host, scope: &Scope) -> Result<Snapshot, String> {
    let home = &host.home;
    let roots = home.scan_dirs(&SkillAgent::ALL, scope);
    let agent_homes: Vec<String> = SkillAgent::ALL
        .iter()
        .map(|&a| home.agent_home(a))
        .collect();
    let lock_path = home.lock_path();
    let manifest = home.hermes_manifest();
    let (entries, existing, files) = match &host.machine {
        Machine::Local => {
            let mut entries = Vec::new();
            for root in &roots {
                walk_local(root, &mut entries);
            }
            let existing: Vec<String> = agent_homes
                .iter()
                .filter(|d| Path::new(d).is_dir())
                .cloned()
                .collect();
            let mut files = BTreeMap::new();
            for f in [&lock_path, &manifest] {
                if let Some(text) = read_capped(Path::new(f), MAX_SKILL_MD_BYTES) {
                    files.insert(f.clone(), text);
                }
            }
            (keep_skill_folders(entries), existing, files)
        }
        Machine::Remote(c) => {
            let nonce = temp_suffix();
            let req = ScanRequest {
                roots: roots.clone(),
                test_dirs: agent_homes.clone(),
                files: vec![lock_path.clone(), manifest.clone()],
            };
            let out = run_remote(c, rops::scan(c.host.os, &req, &nonce))?;
            let res = rops::parse_scan(&out, &nonce);
            (res.entries, res.existing, res.files)
        }
    };
    let detected: Vec<SkillAgent> = SkillAgent::ALL
        .into_iter()
        .filter(|&a| host.programs.contains(&a) || existing.contains(&norm(&home.agent_home(a))))
        // An agent with no skills folder at this scope can't be offered.
        .filter(|&a| home.own_dir(a, scope).is_some())
        .collect();
    let lock = parse_lock(files.get(&lock_path).map(String::as_str));
    let hermes_bundled = files
        .get(&manifest)
        .map(|m| parse_hermes_manifest(m))
        .unwrap_or_default();
    let rows = build(
        entries,
        home,
        scope,
        &hermes_bundled,
        lock.as_ref().unwrap_or(&SkillLock::default()),
    );
    Ok(Snapshot {
        home: home.clone(),
        scope: scope.clone(),
        detected,
        rows,
        lock,
        hermes_bundled,
    })
}

/// Find skill folders below a local skills folder, as the remote scan does.
fn walk_local(root: &str, out: &mut Vec<ScanEntry>) {
    fn visit(root: &str, dir: &Path, rel: &str, depth: usize, out: &mut Vec<ScanEntry>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let is_link = meta.file_type().is_symlink();
            let link_target = is_link.then(|| {
                let raw = std::fs::read_link(&path).unwrap_or_default();
                resolve_link(&norm(&dir.to_string_lossy()), &raw.to_string_lossy())
            });
            let Ok(target_meta) = std::fs::metadata(&path) else {
                if is_link && depth == 1 {
                    out.push(ScanEntry {
                        root: norm(root),
                        rel: child_rel,
                        link_target,
                        broken: true,
                        ..Default::default()
                    });
                }
                continue;
            };
            if !target_meta.is_dir() {
                continue;
            }
            let md = path.join(SKILL_FILE);
            if md.is_file() {
                out.push(ScanEntry {
                    root: norm(root),
                    rel: child_rel,
                    link_target,
                    broken: false,
                    skill_md: read_capped(&md, MAX_SKILL_MD_BYTES),
                    sidecar: read_capped(&path.join(SIDECAR_FILE), 64 * 1024),
                });
            } else if depth < SCAN_DEPTH && !is_link {
                visit(root, &path, &child_rel, depth + 1, out);
            }
        }
    }
    visit(root, Path::new(root), "", 1, out);
}

fn read_capped(path: &Path, max: u64) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

// --- Fetching ---

/// Why skills couldn't be fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchError {
    Source(SourceError),
    Git(GitFailure),
    Io(String),
}

pub fn fetch_error_text(e: &FetchError) -> String {
    match e {
        FetchError::Source(SourceError::Empty) => {
            t("Enter a GitHub repository, like owner/repo.").to_string()
        }
        FetchError::Source(SourceError::Unrecognized) => t(
            "That isn't a repository muxel can fetch. Use owner/repo, a GitHub URL, or a git URL.",
        )
        .to_string(),
        FetchError::Source(SourceError::UnsafePath) => {
            t("The folder in that address leaves the repository.").to_string()
        }
        FetchError::Git(GitFailure::NotFound) => {
            t("git not found. Install git and make sure it is on your PATH.").to_string()
        }
        FetchError::Git(GitFailure::TimedOut { secs }) => tf(
            "git did not finish within {secs} seconds and was stopped.",
            &[("secs", &secs.to_string())],
        ),
        FetchError::Git(GitFailure::Failed { detail }) => {
            tf("git failed: {detail}", &[("detail", &clean(detail))])
        }
        FetchError::Git(other) => tf("git failed: {detail}", &[("detail", &format!("{other:?}"))]),
        FetchError::Io(detail) => tf(
            "Couldn't prepare the download folder: {detail}",
            &[("detail", &clean(detail))],
        ),
    }
}

/// One skill in a fetched repository.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub found: FoundSkill,
    /// The folder it installs under.
    pub name: String,
    /// The git tree SHA of its folder (the lock's `skillFolderHash`).
    pub tree: String,
}

/// A repository cloned into a temporary folder, removed on drop.
pub struct Fetched {
    pub source: SkillSource,
    pub commit: String,
    pub skills: Vec<Candidate>,
    pub skipped: Vec<Skipped>,
    clone: PathBuf,
    env: Arc<GitEnv>,
}

impl Drop for Fetched {
    fn drop(&mut self) {
        let _ = remove_dir_force(&self.env, &self.clone);
    }
}

impl std::fmt::Debug for Fetched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetched")
            .field("source", &self.source.display)
            .field("skills", &self.skills.len())
            .finish()
    }
}

impl Fetched {
    /// The lock/sidecar record for one of its skills, stamped `now`.
    pub fn record(&self, c: &Candidate, now: &str) -> LockEntry {
        LockEntry {
            source: self.source.display.clone(),
            source_type: self.source.kind.lock_name().to_string(),
            source_url: self.source.clone_url.clone(),
            git_ref: self.source.git_ref.clone(),
            skill_path: c.found.skill_path(),
            skill_folder_hash: c.tree.clone(),
            installed_at: now.to_string(),
            updated_at: now.to_string(),
            extra: Default::default(),
        }
    }

    fn files(&self, c: &Candidate) -> Result<Vec<TarFile>, String> {
        collect_files(&self.clone.join(&c.found.dir))
    }
}

/// Clone what the user typed and find its skills. `work` holds the clones.
pub fn fetch(env: &Arc<GitEnv>, work: &Path, input: &str) -> Result<Fetched, FetchError> {
    let src = source::parse(input).map_err(FetchError::Source)?;
    fetch_source(env, work, src)
}

fn fetch_source(env: &Arc<GitEnv>, work: &Path, src: SkillSource) -> Result<Fetched, FetchError> {
    std::fs::create_dir_all(work).map_err(|e| FetchError::Io(e.to_string()))?;
    sweep_stale(env, work);
    let dest = work.join(format!("f-{}", temp_suffix()));
    let deadline = Instant::now() + Duration::from_secs(GIT_TIMEOUT_CLONE_SECS);
    let sha: Option<String> = src
        .git_ref
        .clone()
        .filter(|r| r.len() == 40 && r.chars().all(|c| c.is_ascii_hexdigit()));
    let mut args: Vec<&OsStr> = vec![OsStr::new("clone"), OsStr::new("--quiet")];
    if sha.is_none() {
        args.extend([
            OsStr::new("--depth"),
            OsStr::new("1"),
            OsStr::new("--single-branch"),
        ]);
        if let Some(r) = &src.git_ref {
            args.extend([OsStr::new("--branch"), OsStr::new(r.as_str())]);
        }
    }
    args.extend([
        OsStr::new("--"),
        OsStr::new(src.clone_url.as_str()),
        dest.as_os_str(),
    ]);
    let ceiling = work.parent().unwrap_or(work);
    if let Err(e) = run_git(
        env,
        Some(work),
        Some(ceiling),
        &src.clone_url,
        &args,
        deadline,
    ) {
        let _ = remove_dir_force(env, &dest);
        return Err(FetchError::Git(e));
    }
    // From here on the folder is removed by `Fetched`'s drop.
    let mut fetched = Fetched {
        source: src,
        commit: String::new(),
        skills: Vec::new(),
        skipped: Vec::new(),
        clone: dest,
        env: env.clone(),
    };
    if let Some(sha) = &sha {
        let co = [
            OsStr::new("-C"),
            fetched.clone.as_os_str(),
            OsStr::new("-c"),
            OsStr::new("advice.detachedHead=false"),
            OsStr::new("checkout"),
            OsStr::new("--quiet"),
            OsStr::new(sha),
        ];
        run_git(
            env,
            Some(&fetched.clone),
            Some(work),
            &fetched.source.clone_url,
            &co,
            deadline,
        )
        .map_err(FetchError::Git)?;
    }
    fetched.commit = git_stdout(env, &fetched.clone, &["rev-parse", "HEAD"])?;
    let files = list_files(&fetched.clone);
    let clone = fetched.clone.clone();
    let read = |rel: &str| -> Option<String> {
        let p = clone.join(rel);
        // Never through a link: a repository could point one at a local file.
        let meta = std::fs::symlink_metadata(&p).ok()?;
        if !meta.is_file() {
            return None;
        }
        read_capped(&p, MAX_SKILL_MD_BYTES)
    };
    let d = discover(
        &files,
        &read,
        fetched.source.subpath.as_deref(),
        fetched.source.skill.as_deref(),
    );
    fetched.skipped = d.skipped;
    for found in d.skills {
        let Some(name) =
            install_name(&found.frontmatter.name).or_else(|| install_name(file_name(&found.dir)))
        else {
            continue;
        };
        let tree = tree_hash(env, &fetched.clone, &found.dir).unwrap_or_default();
        fetched.skills.push(Candidate { found, name, tree });
    }
    Ok(fetched)
}

/// `git rev-parse HEAD:<dir>` (the root's tree for `""`).
fn tree_hash(env: &GitEnv, repo: &Path, dir: &str) -> Result<String, FetchError> {
    let spec = if dir.is_empty() {
        "HEAD^{tree}".to_string()
    } else {
        format!("HEAD:{dir}")
    };
    git_stdout(env, repo, &["rev-parse", &spec])
}

/// A local, read-only git query's output (no network, so no time limit).
fn git_stdout(env: &GitEnv, repo: &Path, args: &[&str]) -> Result<String, FetchError> {
    let mut cmd = crate::integrations::command(&env.program);
    cmd.arg("-C").arg(repo).args(args);
    cmd.envs(env.extra_env.iter().map(|(k, v)| (k, v)));
    if let Some(p) = &env.path_override {
        cmd.env("PATH", p);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null());
    let out = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FetchError::Git(GitFailure::NotFound)
        } else {
            FetchError::Io(e.to_string())
        }
    })?;
    if !out.status.success() {
        return Err(FetchError::Git(GitFailure::Failed {
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Remove fetch folders left behind by a crash.
fn sweep_stale(env: &GitEnv, work: &Path) {
    let Ok(read) = std::fs::read_dir(work) else {
        return;
    };
    for e in read.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .is_some_and(|age| age > STALE_FETCH);
        if old && e.file_name().to_string_lossy().starts_with("f-") {
            let _ = remove_dir_force(env, &e.path());
        }
    }
}

/// Every regular file in a clone, relative and `/`-separated, without `.git`.
fn list_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if rel.is_empty() && name == ".git" {
                continue;
            }
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push((e.path(), child));
            } else if ft.is_file() {
                out.push(child);
                if out.len() >= MAX_REPO_FILES {
                    return out;
                }
            }
        }
    }
    out
}

/// A skill folder's files, ready to write anywhere: regular files only; a
/// symlink is followed when it stays inside the folder and dropped otherwise.
/// Refuses folders over [`MAX_SKILL_FILES`] files or [`MAX_SKILL_BYTES`].
fn collect_files(src: &Path) -> Result<Vec<TarFile>, String> {
    let base = std::fs::canonicalize(src).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut total: u64 = 0;
    let mut stack = vec![(base.clone(), String::new(), 0usize)];
    while let Some((dir, rel, depth)) = stack.pop() {
        let read = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if COPY_EXCLUDES.contains(&name.as_str()) || (rel.is_empty() && name == SIDECAR_FILE) {
                continue;
            }
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let Ok(real) = std::fs::canonicalize(e.path()) else {
                continue;
            };
            if !real.starts_with(&base) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&real) else {
                continue;
            };
            if meta.is_dir() {
                // A link back up the tree would loop; depth bounds it.
                if depth < 16 {
                    stack.push((real, child, depth + 1));
                }
            } else if meta.is_file() {
                total += meta.len();
                if out.len() >= MAX_SKILL_FILES || total > MAX_SKILL_BYTES {
                    return Err(
                        t("The skill is too large to install (over 1000 files or 25 MB).")
                            .to_string(),
                    );
                }
                let data = std::fs::read(&real).map_err(|e| e.to_string())?;
                out.push(TarFile {
                    path: child,
                    data,
                    executable: is_executable(&meta),
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

// --- Changing skills ---

/// What a change did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeReport {
    pub done: Vec<String>,
    /// Skill name, message.
    pub failed: Vec<(String, String)>,
    /// The `skills` CLI's lock couldn't be updated.
    pub lock_warning: Option<String>,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn plan_error_text(e: &PlanError) -> String {
    match e {
        PlanError::NoAgents => t("Choose at least one agent.").to_string(),
        PlanError::Unreachable(a) => tf(
            "{agent} has no skills folder here.",
            &[("agent", a.label())],
        ),
    }
}

fn diff_error_text(e: &DiffError) -> String {
    match e {
        DiffError::NoSource => {
            t("There's no copy of this skill that muxel can move or copy.").to_string()
        }
    }
}

/// Install `picks` (indexes into `fetched.skills`) for `agents`.
pub fn install(
    host: &Host,
    snap: &Snapshot,
    fetched: &Fetched,
    picks: &[usize],
    agents: &BTreeSet<SkillAgent>,
) -> ChangeReport {
    let mut report = ChangeReport::default();
    let now = now_iso();
    for &i in picks {
        let Some(c) = fetched.skills.get(i) else {
            continue;
        };
        let res = (|| -> Result<(), String> {
            let record = fetched.record(c, &now);
            let mut files = fetched.files(c)?;
            files.push(sidecar_file(&record));
            let existing = snap.row(&c.name);
            let current = existing.map(|r| r.current(false)).unwrap_or_default();
            let prefer = existing.and_then(|r| r.canonical()).map(|l| l.dir.clone());
            let placement = plan(
                &snap.home,
                &snap.scope,
                &snap.detected,
                agents,
                prefer.as_deref(),
            )
            .map_err(|e| plan_error_text(&e))?;
            let ops = diff(&placement, &current, &c.name, host.link_mode(), true)
                .map_err(|e| diff_error_text(&e))?;
            exec(host, snap, &c.name, &ops, Some(&files))?;
            let in_shared = placement.canonical == snap.home.shared_dir(&snap.scope);
            sync_lock(
                host,
                snap,
                &c.name,
                in_shared.then_some(record),
                &mut report,
            );
            Ok(())
        })();
        match res {
            Ok(()) => report.done.push(c.name.clone()),
            Err(e) => report.failed.push((c.name.clone(), e)),
        }
    }
    report
}

/// Make the skill `key` available to exactly `agents` (as nearly as shared
/// folders allow).
pub fn set_agents(
    host: &Host,
    snap: &Snapshot,
    key: &str,
    agents: &BTreeSet<SkillAgent>,
) -> ChangeReport {
    let mut report = ChangeReport::default();
    let res = (|| -> Result<(), String> {
        let row = snap
            .row(key)
            .ok_or_else(|| t("The skill is gone.").to_string())?;
        let prefer = row.canonical().map(|l| l.dir.clone());
        let placement = plan(
            &snap.home,
            &snap.scope,
            &snap.detected,
            agents,
            prefer.as_deref(),
        )
        .map_err(|e| plan_error_text(&e))?;
        let ops = diff(
            &placement,
            &row.current(false),
            key,
            host.link_mode(),
            false,
        )
        .map_err(|e| diff_error_text(&e))?;
        exec(host, snap, key, &ops, None)?;
        if let Some(record) = &row.record {
            // Keep the record with the skill wherever its copy now lives.
            let sidecar = join(&join(&placement.canonical, key), SIDECAR_FILE);
            write_text(host, &sidecar, &write_sidecar(record))?;
            let in_shared = placement.canonical == snap.home.shared_dir(&snap.scope);
            sync_lock(
                host,
                snap,
                key,
                in_shared.then(|| record.clone()),
                &mut report,
            );
        }
        Ok(())
    })();
    match res {
        Ok(()) => report.done.push(key.to_string()),
        Err(e) => report.failed.push((key.to_string(), e)),
    }
    report
}

/// Remove the skill `key` from every folder it can be removed from.
pub fn remove(host: &Host, snap: &Snapshot, key: &str) -> ChangeReport {
    let mut report = ChangeReport::default();
    let res = (|| -> Result<(), String> {
        let row = snap
            .row(key)
            .ok_or_else(|| t("The skill is gone.").to_string())?;
        exec(host, snap, key, &remove_all(&row.current(true)), None)?;
        sync_lock(host, snap, key, None, &mut report);
        Ok(())
    })();
    match res {
        Ok(()) => report.done.push(key.to_string()),
        Err(e) => report.failed.push((key.to_string(), e)),
    }
    report
}

/// Whether an installed skill's source has changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateState {
    UpToDate,
    /// The new tree SHA.
    Available(String),
    /// The skill's folder is no longer in the repository.
    Gone,
    /// The source couldn't be checked.
    Failed(String),
}

/// The source to fetch to update a skill with this record.
fn record_source(record: &LockEntry) -> Option<SkillSource> {
    let url = if record.source_url.is_empty() {
        &record.source
    } else {
        &record.source_url
    };
    let mut src = source::parse(url).ok()?;
    if src.kind == SourceKind::Git
        && !matches!(record.source_type.as_str(), "git" | "github" | "gitlab")
    {
        return None;
    }
    src.git_ref = record.git_ref.clone();
    src.subpath = None;
    src.skill = None;
    Some(src)
}

/// Check every recorded skill against its repository, one clone per source.
pub fn check_updates(
    env: &Arc<GitEnv>,
    work: &Path,
    snap: &Snapshot,
) -> BTreeMap<String, UpdateState> {
    let mut groups: BTreeMap<(String, Option<String>), Vec<&SkillRow>> = BTreeMap::new();
    for row in &snap.rows {
        if row.read_only().is_some() {
            continue;
        }
        let Some(rec) = &row.record else { continue };
        if record_source(rec).is_none() {
            continue;
        }
        groups
            .entry((rec.source_url.clone(), rec.git_ref.clone()))
            .or_default()
            .push(row);
    }
    let mut out = BTreeMap::new();
    for rows in groups.values() {
        let Some(src) = rows[0].record.as_ref().and_then(record_source) else {
            continue;
        };
        match fetch_source(env, work, src) {
            Ok(f) => {
                for row in rows {
                    let rec = row.record.as_ref().expect("grouped rows have records");
                    let state = match tree_hash(env, &f.clone, &rec.skill_dir()) {
                        Ok(h) if h == rec.skill_folder_hash => UpdateState::UpToDate,
                        Ok(h) => UpdateState::Available(h),
                        Err(_) => UpdateState::Gone,
                    };
                    out.insert(row.key.clone(), state);
                }
            }
            Err(e) => {
                let msg = fetch_error_text(&e);
                for row in rows {
                    out.insert(row.key.clone(), UpdateState::Failed(msg.clone()));
                }
            }
        }
    }
    out
}

/// Update the skill `key` from its recorded source.
pub fn update(
    env: &Arc<GitEnv>,
    work: &Path,
    host: &Host,
    snap: &Snapshot,
    key: &str,
) -> ChangeReport {
    let mut report = ChangeReport::default();
    let res = (|| -> Result<(), String> {
        let row = snap
            .row(key)
            .ok_or_else(|| t("The skill is gone.").to_string())?;
        let rec = row
            .record
            .as_ref()
            .ok_or_else(|| t("muxel doesn't know where this skill came from.").to_string())?;
        let src = record_source(rec)
            .ok_or_else(|| t("muxel can't update skills from this kind of source.").to_string())?;
        let fetched = fetch_source(env, work, src).map_err(|e| fetch_error_text(&e))?;
        let dir = rec.skill_dir();
        let c = fetched
            .skills
            .iter()
            .find(|c| c.found.dir == dir)
            .or_else(|| fetched.skills.iter().find(|c| c.name == key))
            .ok_or_else(|| t("The skill is no longer in its repository.").to_string())?;
        let mut record = fetched.record(c, &now_iso());
        record.installed_at = rec.installed_at.clone();
        let mut files = fetched.files(c)?;
        files.push(sidecar_file(&record));
        exec(
            host,
            snap,
            key,
            &update_ops(&row.current(false)),
            Some(&files),
        )?;
        let in_shared = row
            .canonical()
            .is_some_and(|l| l.dir == snap.home.shared_dir(&snap.scope));
        sync_lock(host, snap, key, in_shared.then_some(record), &mut report);
        Ok(())
    })();
    match res {
        Ok(()) => report.done.push(key.to_string()),
        Err(e) => report.failed.push((key.to_string(), e)),
    }
    report
}

fn sidecar_file(record: &LockEntry) -> TarFile {
    TarFile {
        path: SIDECAR_FILE.to_string(),
        data: write_sidecar(record).into_bytes(),
        executable: false,
    }
}

/// Bring the `skills` CLI's lock in line for `name`: an entry when its
/// canonical copy is in `~/.agents/skills`, none otherwise. User scope only;
/// a lock muxel can't read is left alone (with a warning).
fn sync_lock(
    host: &Host,
    snap: &Snapshot,
    name: &str,
    entry: Option<LockEntry>,
    report: &mut ChangeReport,
) {
    if snap.scope != Scope::User {
        return;
    }
    let path = snap.home.lock_path();
    let current = match read_text(host, &path) {
        Ok(text) => parse_lock(text.as_deref()),
        Err(e) => {
            report.lock_warning = Some(e);
            return;
        }
    };
    let mut lock = match current {
        Ok(lock) => lock,
        Err(LockError::Malformed) => {
            report.lock_warning = Some(
                t("The skills CLI's lock file couldn't be read, so it wasn't changed.").to_string(),
            );
            return;
        }
        Err(LockError::Newer(_)) => {
            report.lock_warning = Some(
                t("The skills CLI's lock file is from a newer version, so it wasn't changed.")
                    .to_string(),
            );
            return;
        }
    };
    let changed = match entry {
        Some(e) => {
            upsert(&mut lock, name, e);
            true
        }
        None => lock.skills.remove(name).is_some(),
    };
    if changed && let Err(e) = write_text(host, &path, &write_lock(&lock)) {
        report.lock_warning = Some(e);
    }
}

/// Refuse anything but a plain skill folder name directly inside a known,
/// changeable skills folder.
fn guard(snap: &Snapshot, dir: &str, name: &str) -> Result<(), String> {
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && name != "..";
    let roots = snap.home.scan_dirs(&SkillAgent::ALL, &snap.scope);
    let known = roots.iter().any(|r| is_within(dir, r));
    let protected = snap
        .home
        .protection(&join(dir, name), &snap.hermes_bundled)
        .is_some();
    if plain && known && !protected {
        Ok(())
    } else {
        Err(tf(
            "Refused to change {path}: it isn't a skill folder muxel manages.",
            &[("path", &join(dir, name))],
        ))
    }
}

fn exec(
    host: &Host,
    snap: &Snapshot,
    name: &str,
    ops: &[SkillOp],
    files: Option<&[TarFile]>,
) -> Result<(), String> {
    for op in ops {
        match op {
            SkillOp::Write { dir } => {
                guard(snap, dir, name)?;
                let files = files.ok_or_else(|| t("Nothing to install.").to_string())?;
                write_skill(host, dir, name, files)?;
            }
            SkillOp::Move { from, to } => {
                guard(snap, from, name)?;
                guard(snap, to, name)?;
                move_skill(host, from, to, name)?;
            }
            SkillOp::Link { dir, target_dir } => {
                guard(snap, dir, name)?;
                let target = join(target_dir, name);
                match host.link_mode() {
                    LinkMode::Symlink => {
                        symlink_skill(host, dir, name, &relative_path(dir, &target))?
                    }
                    LinkMode::Copy => copy_skill(host, &target, dir, name)?,
                }
            }
            SkillOp::Remove { dir } => {
                guard(snap, dir, name)?;
                remove_skill(host, dir, name)?;
            }
        }
    }
    Ok(())
}

fn io_text(path: &str, e: impl std::fmt::Display) -> String {
    tf(
        "Couldn't change {path}: {detail}",
        &[("path", path), ("detail", &clean(&e.to_string()))],
    )
}

fn write_skill(host: &Host, dir: &str, name: &str, files: &[TarFile]) -> Result<(), String> {
    match &host.machine {
        Machine::Local => {
            local_write(Path::new(dir), name, files).map_err(|e| io_text(&join(dir, name), e))
        }
        Machine::Remote(c) => {
            let mtime = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let tar =
                ustar(files, mtime).map_err(|e| io_text(&join(dir, name), format!("{e:?}")))?;
            pipe_remote(
                c,
                rops::untar_into(c.host.os, dir, name, &temp_suffix()),
                &tar,
            )
            .map_err(|e| io_text(&join(dir, name), e))
        }
    }
}

fn symlink_skill(host: &Host, dir: &str, name: &str, target: &str) -> Result<(), String> {
    match &host.machine {
        Machine::Local => {
            local_symlink(Path::new(dir), name, target).map_err(|e| io_text(&join(dir, name), e))
        }
        Machine::Remote(c) => run_remote(c, rops::symlink(dir, name, target))
            .map(|_| ())
            .map_err(|e| io_text(&join(dir, name), e)),
    }
}

fn copy_skill(host: &Host, from: &str, dir: &str, name: &str) -> Result<(), String> {
    match &host.machine {
        Machine::Local => {
            let files = collect_files(Path::new(from))?;
            local_write(Path::new(dir), name, &files).map_err(|e| io_text(&join(dir, name), e))
        }
        Machine::Remote(c) => run_remote(
            c,
            rops::copy_into(c.host.os, from, dir, name, &temp_suffix()),
        )
        .map(|_| ())
        .map_err(|e| io_text(&join(dir, name), e)),
    }
}

fn move_skill(host: &Host, from: &str, to: &str, name: &str) -> Result<(), String> {
    match &host.machine {
        Machine::Local => local_move(Path::new(from), Path::new(to), name)
            .map_err(|e| io_text(&join(to, name), e)),
        Machine::Remote(c) => {
            run_remote(c, rops::move_into(c.host.os, &join(from, name), to, name))
                .map(|_| ())
                .map_err(|e| io_text(&join(to, name), e))
        }
    }
}

fn remove_skill(host: &Host, dir: &str, name: &str) -> Result<(), String> {
    match &host.machine {
        Machine::Local => {
            remove_entry(&Path::new(dir).join(name)).map_err(|e| io_text(&join(dir, name), e))
        }
        Machine::Remote(c) => run_remote(c, rops::remove(c.host.os, &join(dir, name)))
            .map(|_| ())
            .map_err(|e| io_text(&join(dir, name), e)),
    }
}

/// A text file's contents; `None` when it doesn't exist.
fn read_text(host: &Host, path: &str) -> Result<Option<String>, String> {
    match &host.machine {
        Machine::Local => match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_text(path, e)),
        },
        Machine::Remote(c) => {
            let out = run_remote(
                c,
                muxel_core::remote_ops::read_file(c.host.os, path, MAX_SKILL_MD_BYTES),
            )?;
            Ok((!out.is_empty()).then_some(out))
        }
    }
}

fn write_text(host: &Host, path: &str, text: &str) -> Result<(), String> {
    match &host.machine {
        Machine::Local => {
            let p = Path::new(path);
            let res = (|| -> std::io::Result<()> {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let tmp = p.with_file_name(format!(".{}.muxel-{}", file_name(path), temp_suffix()));
                std::fs::write(&tmp, text)?;
                std::fs::rename(&tmp, p).inspect_err(|_| {
                    let _ = std::fs::remove_file(&tmp);
                })
            })();
            res.map_err(|e| io_text(path, e))
        }
        Machine::Remote(c) => pipe_remote(c, rops::write_file(c.host.os, path), text.as_bytes())
            .map_err(|e| io_text(path, e)),
    }
}

// --- Local file operations ---

/// Remove a path without following it: a link (to a file or folder) itself, or
/// a real folder and its contents.
fn remove_entry(p: &Path) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        // A directory symlink on Windows is removed with remove_dir.
        std::fs::remove_file(p).or_else(|_| std::fs::remove_dir(p))
    } else if meta.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

/// Write `files` as `<dir>/<name>` through a staging folder swapped in once
/// complete, so a failure never leaves a half-written skill.
fn local_write(dir: &Path, name: &str, files: &[TarFile]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let stage = dir.join(format!(".{name}.muxel-new-{}", temp_suffix()));
    remove_entry(&stage)?;
    std::fs::create_dir(&stage)?;
    let written = (|| -> std::io::Result<()> {
        for f in files {
            let p = stage.join(&f.path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&p, &f.data)?;
            #[cfg(unix)]
            if f.executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        Ok(())
    })();
    if let Err(e) = written {
        let _ = remove_entry(&stage);
        return Err(e);
    }
    swap_in(dir, name, &stage)
}

/// Put `stage` at `<dir>/<name>`, replacing a link or folder there.
fn swap_in(dir: &Path, name: &str, stage: &Path) -> std::io::Result<()> {
    let dest = dir.join(name);
    let mut old = None;
    match std::fs::symlink_metadata(&dest) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => remove_entry(&dest)?,
        Ok(_) => {
            let o = dir.join(format!(".{name}.muxel-old-{}", temp_suffix()));
            std::fs::rename(&dest, &o)?;
            old = Some(o);
        }
        Err(_) => {}
    }
    if let Err(e) = std::fs::rename(stage, &dest) {
        if let Some(o) = &old {
            let _ = std::fs::rename(o, &dest);
        }
        let _ = remove_entry(stage);
        return Err(e);
    }
    if let Some(o) = old {
        let _ = remove_entry(&o);
    }
    Ok(())
}

fn local_symlink(dir: &Path, name: &str, target: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let link = dir.join(name);
    remove_entry(&link)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, &link)
    }
    #[cfg(not(unix))]
    {
        std::os::windows::fs::symlink_dir(target, &link)
    }
}

fn local_move(from_dir: &Path, to_dir: &Path, name: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(to_dir)?;
    let from = from_dir.join(name);
    let dest = to_dir.join(name);
    remove_entry(&dest)?;
    match std::fs::rename(&from, &dest) {
        Ok(()) => Ok(()),
        // Across file systems: copy, then remove the original.
        Err(_) => {
            let files = collect_files(&from).map_err(std::io::Error::other)?;
            local_write(to_dir, name, &files)?;
            remove_entry(&from)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::fetch;
    use crate::integrations::GitEnv;
    use crate::test_support::{TestRepo, file_url, short_temp_path};

    // The tests past `fetch` check symlinks (Windows installs copies), Unix
    // permissions and `sh`-run remote commands, so they're Unix-only; so is
    // what only they use, or Windows clippy fails on unused items.
    #[cfg(unix)]
    use {
        super::{
            Host, UpdateState, check_updates, collect_files, connect_local_with, install, remove,
            scan, set_agents, update,
        },
        muxel_core::RemoteOs,
        muxel_core::skills::{Scope, SkillAgent, SkillAgent::*, SkillHome},
        std::collections::BTreeSet,
        std::path::Path,
    };

    struct TmpDir(PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp() -> TmpDir {
        let p = short_temp_path();
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    /// A fake home with Claude, opencode and pi set up (by their folders).
    #[cfg(unix)]
    fn host(home: &Path) -> Host {
        for d in [".claude", ".config/opencode", ".pi/agent"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let h = SkillHome::new(home.to_string_lossy(), RemoteOs::Unix);
        connect_local_with(h, &|_| false)
    }

    fn skill_md(name: &str) -> String {
        format!("---\nname: {name}\ndescription: The {name} skill.\nlicense: MIT\n---\nBody\n")
    }

    fn repo() -> TestRepo {
        let r = TestRepo::init();
        r.commit(
            &[
                ("skills/alpha/SKILL.md", &skill_md("alpha")),
                ("skills/alpha/run.sh", "#!/bin/sh\necho hi\n"),
                ("skills/beta/SKILL.md", &skill_md("beta")),
            ],
            "skills",
        );
        r
    }

    #[cfg(unix)]
    fn agents(a: &[SkillAgent]) -> BTreeSet<SkillAgent> {
        a.iter().copied().collect()
    }

    #[test]
    fn fetch_finds_skills_with_tree_hashes() {
        let work = tmp();
        let r = repo();
        let env = Arc::new(GitEnv::for_tests());
        let f = fetch(&env, &work.0, &file_url(r.path())).unwrap();
        let names: Vec<&str> = f.skills.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert_eq!(f.skills[0].tree.len(), 40);
        assert!(f.skills[0].found.has_scripts);
        assert_eq!(f.commit.len(), 40);
        let clone = f.clone.clone();
        assert!(clone.exists());
        drop(f);
        assert!(!clone.exists(), "the clone is removed with the fetch");
    }

    #[cfg(unix)]
    #[test]
    fn install_for_all_agents_links_claude_and_records_the_source() {
        let work = tmp();
        let home = tmp();
        let r = repo();
        let env = Arc::new(GitEnv::for_tests());
        let h = host(&home.0);
        let snap = scan(&h, &Scope::User).unwrap();
        assert_eq!(snap.detected, [Claude, Opencode, Pi]);
        let f = fetch(&env, &work.0, &file_url(r.path())).unwrap();
        let rep = install(&h, &snap, &f, &[0], &agents(&snap.detected));
        assert_eq!(rep.failed, []);
        assert_eq!(rep.done, ["alpha"]);
        let shared = home.0.join(".agents/skills/alpha");
        assert!(shared.join("SKILL.md").is_file());
        assert!(shared.join(".muxel-skill.json").is_file());
        let link = home.0.join(".claude/skills/alpha");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../../.agents/skills/alpha")
        );
        assert!(shared.join("run.sh").is_file());
        let lock = std::fs::read_to_string(home.0.join(".agents/.skill-lock.json")).unwrap();
        assert!(lock.contains("\"skillPath\": \"skills/alpha/SKILL.md\""));
        assert!(lock.contains(&f.skills[0].tree));

        let snap = scan(&h, &Scope::User).unwrap();
        let row = snap.row("alpha").unwrap();
        assert_eq!(row.description, "The alpha skill.");
        assert_eq!(
            row.visible_to(&snap.home, &snap.scope, &snap.detected),
            agents(&[Claude, Opencode, Pi])
        );
        assert!(row.record.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn narrowing_and_removing() {
        let work = tmp();
        let home = tmp();
        let r = repo();
        let env = Arc::new(GitEnv::for_tests());
        let h = host(&home.0);
        let snap = scan(&h, &Scope::User).unwrap();
        let f = fetch(&env, &work.0, &file_url(r.path())).unwrap();
        install(&h, &snap, &f, &[1], &agents(&snap.detected));

        // Only pi: the copy moves out of the shared folder (opencode reads it).
        let snap = scan(&h, &Scope::User).unwrap();
        let rep = set_agents(&h, &snap, "beta", &agents(&[Pi]));
        assert_eq!(rep.failed, []);
        assert!(home.0.join(".pi/agent/skills/beta/SKILL.md").is_file());
        assert!(!home.0.join(".agents/skills/beta").exists());
        assert!(std::fs::symlink_metadata(home.0.join(".claude/skills/beta")).is_err());
        assert!(
            home.0
                .join(".pi/agent/skills/beta/.muxel-skill.json")
                .is_file()
        );
        let lock = std::fs::read_to_string(home.0.join(".agents/.skill-lock.json")).unwrap();
        assert!(!lock.contains("\"beta\""), "no longer in the CLI's folder");
        let snap = scan(&h, &Scope::User).unwrap();
        assert_eq!(
            snap.row("beta")
                .unwrap()
                .visible_to(&snap.home, &snap.scope, &snap.detected),
            agents(&[Pi])
        );

        let rep = remove(&h, &snap, "beta");
        assert_eq!(rep.failed, []);
        assert!(!home.0.join(".pi/agent/skills/beta").exists());
        assert!(scan(&h, &Scope::User).unwrap().row("beta").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn synced_skills_are_listed_but_never_changed() {
        let home = tmp();
        let h = host(&home.0);
        let synced = home.0.join(".claude/skills/synced/bucket/docx");
        std::fs::create_dir_all(&synced).unwrap();
        std::fs::write(synced.join("SKILL.md"), skill_md("docx")).unwrap();
        let snap = scan(&h, &Scope::User).unwrap();
        let row = snap.row("docx").unwrap();
        assert!(row.read_only().is_some());
        let rep = remove(&h, &snap, "docx");
        assert_eq!(rep.done, ["docx"], "nothing removable is a no-op");
        assert!(synced.join("SKILL.md").is_file());
        // A forged op on it is refused by the guard.
        let err = super::exec(
            &h,
            &snap,
            "docx",
            &[muxel_core::skills::placement::SkillOp::Remove {
                dir: muxel_core::skills::norm(&synced.parent().unwrap().to_string_lossy()),
            }],
            None,
        );
        assert!(err.is_err());
        assert!(synced.join("SKILL.md").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn links_out_of_a_skill_are_not_copied() {
        let src = tmp();
        let outside = tmp();
        std::fs::write(outside.0.join("secret"), "x").unwrap();
        std::fs::write(src.0.join("SKILL.md"), skill_md("s")).unwrap();
        std::fs::create_dir(src.0.join("refs")).unwrap();
        std::fs::write(src.0.join("refs/a.md"), "a").unwrap();
        std::os::unix::fs::symlink(outside.0.join("secret"), src.0.join("leak")).unwrap();
        std::os::unix::fs::symlink("refs/a.md", src.0.join("alias.md")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(src.0.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(src.0.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let files = collect_files(&src.0).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["SKILL.md", "alias.md", "refs/a.md", "run.sh"]);
        // Written out again, scripts stay executable.
        let dest = tmp();
        super::local_write(&dest.0, "s", &files).unwrap();
        let mode = std::fs::metadata(dest.0.join("s/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
        assert!(!dest.0.join("s/leak").exists());
    }

    #[cfg(unix)]
    #[test]
    fn updates_are_detected_and_applied() {
        let work = tmp();
        let home = tmp();
        let r = repo();
        let env = Arc::new(GitEnv::for_tests());
        let h = host(&home.0);
        let snap = scan(&h, &Scope::User).unwrap();
        let f = fetch(&env, &work.0, &file_url(r.path())).unwrap();
        install(&h, &snap, &f, &[0, 1], &agents(&snap.detected));
        drop(f);

        let snap = scan(&h, &Scope::User).unwrap();
        let states = check_updates(&env, &work.0, &snap);
        assert_eq!(states.get("alpha"), Some(&UpdateState::UpToDate));

        r.commit(&[("skills/alpha/extra.md", "new")], "more");
        let states = check_updates(&env, &work.0, &snap);
        assert!(matches!(
            states.get("alpha"),
            Some(UpdateState::Available(_))
        ));
        assert_eq!(states.get("beta"), Some(&UpdateState::UpToDate));

        let rep = update(&env, &work.0, &h, &snap, "alpha");
        assert_eq!(rep.failed, []);
        assert!(home.0.join(".agents/skills/alpha/extra.md").is_file());
        // Through the link too.
        assert!(home.0.join(".claude/skills/alpha/extra.md").is_file());
        let snap = scan(&h, &Scope::User).unwrap();
        let states = check_updates(&env, &work.0, &snap);
        assert_eq!(states.get("alpha"), Some(&UpdateState::UpToDate));
    }

    /// The remote scripts, run through a local `sh` against a fake home: what
    /// an SSH host would run, minus ssh.
    #[cfg(unix)]
    #[test]
    fn remote_scripts_work_under_a_real_shell() {
        use muxel_core::skills::remote::{self as rops, ScanRequest};
        use muxel_core::skills::tar::{TarFile, ustar};

        let home = tmp();
        let h = home.0.to_string_lossy().into_owned();
        let run = |cmd: String, stdin: Option<&[u8]>| -> String {
            use std::io::Write;
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(rops::for_host(RemoteOs::Unix, cmd))
                .env("HOME", &home.0)
                .env_remove("XDG_CONFIG_HOME")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut pipe = child.stdin.take().unwrap();
            pipe.write_all(stdin.unwrap_or_default()).unwrap();
            drop(pipe);
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        let probe = rops::parse_probe(&run(
            rops::probe(RemoteOs::Unix, &["sh", "no-such-agent"]),
            None,
        ))
        .unwrap();
        assert_eq!(probe.home, muxel_core::skills::norm(&h));
        assert_eq!(probe.programs, ["sh"]);

        // Unpack a skill made by muxel's own tar writer.
        let shared = format!("{h}/.agents/skills");
        let files = [
            TarFile {
                path: "SKILL.md".into(),
                data: skill_md("it's").into_bytes(),
                executable: false,
            },
            TarFile {
                path: "scripts/run.sh".into(),
                data: b"#!/bin/sh\n".to_vec(),
                executable: true,
            },
            TarFile {
                path: ".muxel-skill.json".into(),
                data: br#"{"source":"o/r","sourceType":"github","sourceUrl":"u"}"#.to_vec(),
                executable: false,
            },
        ];
        let tar = ustar(&files, 1).unwrap();
        run(
            rops::untar_into(RemoteOs::Unix, &shared, "x", "n1"),
            Some(&tar),
        );
        assert!(home.0.join(".agents/skills/x/scripts/run.sh").is_file());
        // Replacing it goes through the same staging.
        run(
            rops::untar_into(RemoteOs::Unix, &shared, "x", "n2"),
            Some(&tar),
        );
        let leftovers: Vec<_> = std::fs::read_dir(home.0.join(".agents/skills"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers, ["x"]);

        let claude = format!("{h}/.claude/skills");
        run(rops::symlink(&claude, "x", "../../.agents/skills/x"), None);
        std::fs::create_dir_all(home.0.join(".pi/agent/skills")).unwrap();
        std::os::unix::fs::symlink("/nowhere", home.0.join(".pi/agent/skills/gone")).unwrap();
        std::fs::write(home.0.join(".agents/.skill-lock.json"), "{\"version\":3}").unwrap();

        let req = ScanRequest {
            roots: vec![
                shared.clone(),
                claude.clone(),
                format!("{h}/.pi/agent/skills"),
            ],
            test_dirs: vec![format!("{h}/.claude"), format!("{h}/.gemini")],
            files: vec![format!("{h}/.agents/.skill-lock.json")],
        };
        let res = rops::parse_scan(
            &run(rops::scan(RemoteOs::Unix, &req, "abc123"), None),
            "abc123",
        );
        assert_eq!(res.existing, [format!("{h}/.claude")]);
        assert_eq!(res.files.values().next().unwrap(), "{\"version\":3}");
        let found: Vec<(String, bool, bool)> = res
            .entries
            .iter()
            .map(|e| (e.rel.clone(), e.link_target.is_some(), e.broken))
            .collect();
        assert_eq!(found.len(), 3, "{found:?}");
        let link = res
            .entries
            .iter()
            .find(|e| e.root.ends_with(".claude/skills"))
            .unwrap();
        assert_eq!(
            link.link_target.as_deref(),
            Some(format!("{shared}/x").as_str())
        );
        assert!(link.skill_md.as_deref().unwrap().contains("name: it's"));
        let real = res.entries.iter().find(|e| e.root == shared).unwrap();
        assert!(real.sidecar.as_deref().unwrap().contains("o/r"));
        assert!(res.entries.iter().any(|e| e.broken && e.rel == "gone"));

        let pi = format!("{h}/.pi/agent/skills");
        run(
            rops::move_into(RemoteOs::Unix, &format!("{shared}/x"), &pi, "x"),
            None,
        );
        assert!(home.0.join(".pi/agent/skills/x/SKILL.md").is_file());
        run(
            rops::copy_into(RemoteOs::Unix, &format!("{pi}/x"), &shared, "x", "n3"),
            None,
        );
        assert!(home.0.join(".agents/skills/x/SKILL.md").is_file());
        run(rops::remove(RemoteOs::Unix, &format!("{claude}/x")), None);
        assert!(std::fs::symlink_metadata(home.0.join(".claude/skills/x")).is_err());
        assert!(
            home.0.join(".agents/skills/x/SKILL.md").is_file(),
            "only the link went"
        );
        run(
            rops::write_file(RemoteOs::Unix, &format!("{h}/new/dir/f.json")),
            Some(b"{}"),
        );
        assert_eq!(
            std::fs::read_to_string(home.0.join("new/dir/f.json")).unwrap(),
            "{}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_scope_installs_into_the_project() {
        let work = tmp();
        let home = tmp();
        let proj = tmp();
        let r = repo();
        let env = Arc::new(GitEnv::for_tests());
        let h = host(&home.0);
        let scope = Scope::Project(proj.0.to_string_lossy().into_owned());
        let snap = scan(&h, &scope).unwrap();
        let f = fetch(&env, &work.0, &file_url(r.path())).unwrap();
        let rep = install(&h, &snap, &f, &[0], &agents(&[Claude]));
        assert_eq!(rep.failed, []);
        assert!(proj.0.join(".claude/skills/alpha/SKILL.md").is_file());
        assert!(
            !home.0.join(".agents/.skill-lock.json").exists(),
            "the global lock is untouched"
        );
    }
}
