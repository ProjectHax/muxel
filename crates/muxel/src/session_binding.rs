//! Exact provider session bindings that must survive an in-process `/resume`.

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::{Hash as _, Hasher as _};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

const CLAUDE_BINDING_DIR: &str = "provider-session-bindings/claude";
const CLAUDE_SETTINGS_DIR: &str = "provider-session-bindings/claude/settings";
const CLAUDE_HOOK_FLAG: &str = "--claude-session-hook";
const MAX_HOOK_INPUT_BYTES: u64 = 64 * 1024;
/// The field the hook adds to Claude's event: see [`ClaudeHookStamp`].
const HOOK_STAMP_FIELD: &str = "muxel_hook";
pub(crate) const MUXEL_INSTANCE_ID_ENV: &str = muxel_core::control::INSTANCE_ID_ENV;

/// The process muxel starts for a pane, for [`pane_env`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneProcess {
    /// The pane's program itself, on this computer.
    Program,
    /// A tmux client; the program runs in a tmux session on this computer.
    TmuxClient,
    /// An ssh client; the program runs on another computer.
    SshClient,
}

/// The environment a pane's process starts with, on top of muxel's own: the
/// variables to set (what the preset and instance set, then `launch`, what the
/// launch itself needs, such as ssh's password), and the ones to remove.
///
/// The pane's own program also gets `MUXEL_INSTANCE_ID`, the id of the pane it
/// runs in, which `muxel ctl` takes as AGENT and Claude's session hook reads to
/// say which pane switched conversations. It replaces any value set for it,
/// since only muxel knows which pane that is.
///
/// A tmux or ssh client has none at all, not even one muxel itself inherited
/// (from a pane it was started in) or one set for it, since each can pass its
/// environment on to the program. A tmux pane's program starts from the tmux
/// server's environment and gets its id through [`tmux_pane_args`], but tmux
/// copies the variables its `update-environment` option names from the client
/// into a new session, so a user who names this one would hand the session the
/// client's. An ssh `SendEnv` naming this variable could forward the client's
/// local control id to the remote program, where it does not identify that
/// program.
///
/// Under an isolated root, configured profile variables are never trusted,
/// including case variants on Windows. Direct programs and ssh clients inherit
/// the exact startup root (which need not be Unicode) and have no control
/// override. Only the tmux launch supplies its constructed matching endpoint.
pub(crate) fn pane_env(
    configured: &[(String, String)],
    launch: Vec<(String, String)>,
    instance_id: Uuid,
    process: PaneProcess,
    isolated: bool,
) -> (Vec<(String, String)>, Vec<String>) {
    let profile_key = |key: &str| {
        key.eq_ignore_ascii_case(muxel_store::USER_DATA_DIR_ENV)
            || key.eq_ignore_ascii_case(crate::control::CONTROL_ENV)
    };
    let mut env: Vec<_> = configured
        .iter()
        .filter(|(key, _)| !isolated || !profile_key(key))
        .cloned()
        .collect();
    env.extend(launch);
    let mut removed = Vec::new();
    if isolated && process != PaneProcess::TmuxClient {
        env.retain(|(key, _)| !profile_key(key));
        removed.push(crate::control::CONTROL_ENV.to_string());
    }
    let not_the_id = |(key, _): &(String, String)| !key.eq_ignore_ascii_case(MUXEL_INSTANCE_ID_ENV);
    match process {
        PaneProcess::Program => {
            env.retain(not_the_id);
            env.push((MUXEL_INSTANCE_ID_ENV.to_string(), instance_id.to_string()));
            (env, removed)
        }
        PaneProcess::TmuxClient | PaneProcess::SshClient => {
            env.retain(not_the_id);
            removed.push(MUXEL_INSTANCE_ID_ENV.to_string());
            (env, removed)
        }
    }
}

/// The tmux arguments for a local pane kept in tmux session `session` on the
/// server `socket` (see `integrations::local_tmux_socket`), running `program`
/// (with none, the user's shell).
///
/// The program starts from the tmux server's environment, not this client's, so
/// the pane's id and isolated profile are handed over with `new-session -e`
/// (`session_env`: tmux 3.2 and later). Older tmux cannot launch isolated panes.
/// No preset environment or secrets go into argv. First, on every tmux, the id
/// is cleared from the server: one started
/// from inside another pane (by a `tmux` typed in a shell pane, or by a muxel run
/// there) has that pane's id, which a program started without `-e` would take as
/// its own.
pub(crate) struct LocalTmuxLaunch {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub(crate) fn tmux_pane_args(
    socket: muxel_core::tmux::Socket,
    session: &str,
    program: Option<&str>,
    args: &[String],
    instance_id: Uuid,
    session_env: bool,
    user_data_root: Option<&Path>,
) -> Result<LocalTmuxLaunch, muxel_core::isolated_root::TmuxEnvironmentError> {
    let env = muxel_core::isolated_root::local_tmux_environment(
        user_data_root,
        instance_id,
        session_env,
    )?;
    // No `-c`: the client is spawned with `cwd` already, which is what tmux uses
    // for a new session anyway. Passing it would put the project's path in this
    // client's argv for no gain.
    let socket = user_data_root
        .filter(|root| !root.as_os_str().is_empty())
        .map(muxel_core::tmux::Socket::isolated)
        .unwrap_or(socket);
    let args = socket.argv(muxel_core::tmux::launch_session_args_with_env(
        session,
        None,
        program,
        args,
        &env,
        &[MUXEL_INSTANCE_ID_ENV],
    ));
    // The same profile values on the client cover user-defined
    // update-environment options during attach. pane_env strips the pane id.
    Ok(LocalTmuxLaunch { args, env })
}

/// What the hook adds to the record it writes: a nonce naming this write, and
/// when it ran. A record with a nonce muxel hasn't seen means a SessionStart ran
/// since the last one, even when it names the conversation already bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeHookStamp {
    pub nonce: Uuid,
    pub recorded_at: SystemTime,
}

/// A pane's binding record, validated, from one read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeBinding {
    pub session_id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    /// `None` for a record written by an older muxel's hook.
    pub stamp: Option<ClaudeHookStamp>,
}

/// Whether a pane's binding record reports a switch, given the nonce of the
/// last record this launch read (`seen`) and the bound conversation. A stamped
/// record is known by its nonce: one this launch hasn't seen is a switch even
/// when it names the bound conversation, as after `/resume B` and `/resume A`
/// between two reads. The launch's first stamped record, and a record from an
/// older muxel's hook, switch only when they name another conversation: a
/// resumed launch's own `SessionStart` names the conversation muxel launched.
pub(crate) fn claude_record_switches(
    binding: &ClaudeBinding,
    seen: Option<Uuid>,
    bound: Option<&str>,
) -> bool {
    match (binding.stamp, seen) {
        (Some(stamp), Some(seen)) => stamp.nonce != seen,
        _ => bound != Some(binding.session_id.as_str()),
    }
}

pub(crate) fn hook_instance_from_args(
    args: impl IntoIterator<Item = OsString>,
    env_instance_id: Option<OsString>,
) -> std::result::Result<Option<Uuid>, String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == CLAUDE_HOOK_FLAG {
            // Old settings files passed the UUID as argv. Keep accepting that form so
            // hooks from an already-running pane survive a Muxel upgrade. New settings
            // route the pane through inherited environment instead.
            let raw = args.next().or(env_instance_id).ok_or_else(|| {
                format!(
                    "{CLAUDE_HOOK_FLAG} requires an instance UUID argument or {MUXEL_INSTANCE_ID_ENV}"
                )
            })?;
            let raw = raw
                .to_str()
                .ok_or_else(|| format!("{CLAUDE_HOOK_FLAG} requires a UTF-8 instance UUID"))?;
            let id = Uuid::parse_str(raw)
                .map_err(|_| format!("{CLAUDE_HOOK_FLAG} requires a valid instance UUID"))?;
            return Ok(Some(id));
        }
    }
    Ok(None)
}

pub(crate) fn run_claude_session_hook(instance_id: Uuid) -> Result<()> {
    let data_dir = muxel_store::data_dir().context("could not determine muxel data directory")?;
    write_claude_binding_from_reader(&data_dir, instance_id, std::io::stdin().lock())
}

pub(crate) fn claude_hook_settings() -> Option<String> {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            log::warn!("could not resolve Muxel executable for Claude session hook: {error}");
            return None;
        }
    };
    let Some(data_dir) = muxel_store::data_dir() else {
        log::warn!("could not resolve Muxel data directory for Claude session hook");
        return None;
    };
    let path = match write_claude_hook_settings_file(&data_dir, &executable) {
        Ok(path) => path,
        Err(error) => {
            log::warn!("could not write Claude session hook settings: {error:#}");
            return None;
        }
    };
    let argument = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    if !windows_batch_arg_is_safe(&argument) {
        log::warn!(
            "Claude hook settings path contains cmd.exe metacharacters; session rebinding is disabled for this pane"
        );
        return None;
    }
    Some(argument)
}

fn claude_hook_settings_for(executable: &Path) -> String {
    json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "resume|clear|fork",
                "hooks": [{
                    "type": "command",
                    "command": executable.to_string_lossy(),
                    "args": [CLAUDE_HOOK_FLAG],
                    "timeout": 5
                }]
            }]
        }
    })
    .to_string()
}

fn executable_identity(executable: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    executable.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn claude_hook_settings_path(data_dir: &Path, executable: &Path) -> PathBuf {
    data_dir
        .join(CLAUDE_SETTINGS_DIR)
        .join(format!("{}.json", executable_identity(executable)))
}

fn write_claude_hook_settings_file(data_dir: &Path, executable: &Path) -> Result<PathBuf> {
    let path = claude_hook_settings_path(data_dir, executable);
    let parent = path
        .parent()
        .context("Claude hook settings path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "creating Claude hook settings directory {}",
            parent.display()
        )
    })?;
    let desired = claude_hook_settings_for(executable);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(desired.as_str()) {
        return Ok(path);
    }

    let temp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    std::fs::write(&temp, &desired)
        .with_context(|| format!("writing Claude hook settings {}", temp.display()))?;
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    if let Err(error) = std::fs::rename(&temp, &path) {
        // Two pane launches can race. Identical content at the target is success;
        // only remove the uniquely-owned temporary file.
        if std::fs::read_to_string(&path).ok().as_deref() == Some(desired.as_str()) {
            let _ = std::fs::remove_file(&temp);
        } else {
            let _ = std::fs::remove_file(&temp);
            return Err(error)
                .with_context(|| format!("installing Claude hook settings {}", path.display()));
        }
    }
    Ok(path)
}

#[cfg(any(windows, test))]
fn windows_batch_arg_is_safe(value: &str) -> bool {
    !value
        .chars()
        .any(|ch| matches!(ch, '"' | '%' | '&' | '|' | '<' | '>' | '^' | '\r' | '\n'))
}

pub(crate) fn claude_binding_path(data_dir: &Path, instance_id: Uuid) -> PathBuf {
    data_dir
        .join(CLAUDE_BINDING_DIR)
        .join(format!("{instance_id}.json"))
}

pub(crate) fn write_claude_binding_from_reader(
    data_dir: &Path,
    instance_id: Uuid,
    reader: impl std::io::Read,
) -> Result<()> {
    let mut input = String::new();
    reader
        .take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_string(&mut input)
        .context("reading Claude SessionStart input")?;
    if input.len() as u64 > MAX_HOOK_INPUT_BYTES {
        bail!("Claude SessionStart input exceeds {MAX_HOOK_INPUT_BYTES} bytes");
    }
    let mut event: Value =
        serde_json::from_str(&input).context("parsing Claude SessionStart input")?;
    validate_claude_event(&event)?;
    stamp_claude_event(&mut event, Uuid::new_v4(), SystemTime::now());

    let path = claude_binding_path(data_dir, instance_id);
    let parent = path.parent().context("Claude binding path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating Claude binding directory {}", parent.display()))?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temp, event.to_string())
        .with_context(|| format!("writing Claude binding {}", temp.display()))?;
    // muxel may claim the previous record (rename it away) at any moment.
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error)
                .with_context(|| format!("replacing Claude binding {}", path.display()));
        }
        _ => {}
    }
    std::fs::rename(&temp, &path)
        .with_context(|| format!("installing Claude binding {}", path.display()))?;
    Ok(())
}

fn validate_claude_event(event: &Value) -> Result<()> {
    if event.get("hook_event_name").and_then(Value::as_str) != Some("SessionStart") {
        bail!("expected a Claude SessionStart event");
    }
    let source = event
        .get("source")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no source")?;
    if !matches!(source, "resume" | "clear" | "fork") {
        bail!("unexpected Claude SessionStart source {source:?}");
    }
    let session_id = event
        .get("session_id")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no session_id")?;
    Uuid::parse_str(session_id).context("Claude SessionStart session_id is not a UUID")?;
    let transcript = event
        .get("transcript_path")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no transcript_path")?;
    let expected_name = format!("{session_id}.jsonl");
    if Path::new(transcript)
        .file_name()
        .and_then(|name| name.to_str())
        != Some(expected_name.as_str())
    {
        bail!("Claude transcript filename does not match session_id");
    }
    let cwd = event
        .get("cwd")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no cwd")?;
    if !Path::new(cwd).is_absolute() || !Path::new(transcript).is_absolute() {
        bail!("Claude SessionStart paths must be absolute");
    }
    Ok(())
}

fn stamp_claude_event(event: &mut Value, nonce: Uuid, recorded_at: SystemTime) {
    let micros = recorded_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros() as u64);
    if let Some(event) = event.as_object_mut() {
        event.insert(
            HOOK_STAMP_FIELD.to_string(),
            json!({ "nonce": nonce.to_string(), "recorded_at_us": micros }),
        );
    }
}

fn claude_hook_stamp(event: &Value) -> Option<ClaudeHookStamp> {
    let stamp = event.get(HOOK_STAMP_FIELD)?;
    let nonce = Uuid::parse_str(stamp.get("nonce")?.as_str()?).ok()?;
    let micros = stamp.get("recorded_at_us")?.as_u64()?;
    Some(ClaudeHookStamp {
        nonce,
        recorded_at: SystemTime::UNIX_EPOCH + Duration::from_micros(micros),
    })
}

/// The pane's binding record, if one is present and valid.
#[cfg(test)]
fn read_claude_binding(data_dir: &Path, instance_id: Uuid, home: &Path) -> Option<ClaudeBinding> {
    claude_binding_from_record(
        &std::fs::read(claude_binding_path(data_dir, instance_id)).ok()?,
        home,
    )
}

/// Whether a binding record isn't a Claude `SessionStart` event the hook would
/// write, so no later read can validate it. A well-formed record that fails
/// [`claude_binding_from_record`] may validate later (`/clear` before Claude
/// creates the project directory), or never: Claude's store lives elsewhere
/// (`CLAUDE_CONFIG_DIR`), or a resumed transcript was deleted. Such a record is
/// read again on each poll until a newer one supersedes it.
pub(crate) fn claude_record_is_malformed(record: &[u8]) -> bool {
    serde_json::from_slice::<Value>(record)
        .ok()
        .is_none_or(|event| validate_claude_event(&event).is_err())
}

/// Parse and validate one read of a binding record. The session id, its
/// transcript and the hook's stamp all come from the same bytes.
pub(crate) fn claude_binding_from_record(record: &[u8], home: &Path) -> Option<ClaudeBinding> {
    let event: Value = serde_json::from_slice(record).ok()?;
    validate_claude_event(&event).ok()?;
    let session_id = event.get("session_id")?.as_str()?;
    let source = event.get("source")?.as_str()?;
    let transcript = Path::new(event.get("transcript_path")?.as_str()?);
    let projects = home.join(".claude").join("projects").canonicalize().ok()?;
    let expected_name = format!("{session_id}.jsonl");
    if transcript.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return None;
    }

    // `/clear` and `/fork` may announce the new UUID before Claude writes its
    // first transcript record. Validate the existing project directory in that
    // case; resumed sessions must already have a real transcript.
    let project = transcript.parent()?.canonicalize().ok()?;
    if project.parent()? != projects {
        return None;
    }
    match transcript.canonicalize() {
        Ok(resolved) if resolved.is_file() && resolved.parent() == Some(project.as_path()) => {}
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && matches!(source, "clear" | "fork") => {}
        _ => return None,
    }
    Some(ClaudeBinding {
        session_id: session_id.to_string(),
        path: transcript.to_path_buf(),
        cwd: PathBuf::from(event.get("cwd")?.as_str()?),
        stamp: claude_hook_stamp(&event),
    })
}

/// A binding record muxel has claimed: its bytes, read once, and its write
/// time (an older muxel's hook didn't stamp its record).
/// Immutable worker input. Reading it does not claim or acknowledge the hook.
pub(crate) struct ClaudeBindingSnapshot {
    pub bytes: Vec<u8>,
    pub modified: Option<SystemTime>,
}

/// Validate a pending claim before a newer shared hook, without changing either.
/// Read bytes and modification time from the same open file.
pub(crate) fn read_pending_claude_binding(
    data_dir: &Path,
    instance_id: Uuid,
) -> Option<ClaudeBindingSnapshot> {
    let mut file = match std::fs::File::open(claude_claimed_binding_path(data_dir, instance_id)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::File::open(claude_binding_path(data_dir, instance_id)).ok()?
        }
        Err(_) => return None,
    };
    let modified = file
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(ClaudeBindingSnapshot { bytes, modified })
}

pub(crate) struct ClaimedClaudeRecord {
    path: PathBuf,
    /// The shared path, where the hook writes the pane's next record.
    shared: PathBuf,
    pub bytes: Vec<u8>,
    pub modified: Option<SystemTime>,
}

impl ClaimedClaudeRecord {
    /// Whether the hook has written a newer record since this one was claimed.
    fn superseded(&self) -> Result<bool> {
        self.shared
            .try_exists()
            .context("check for a newer Claude conversation binding")
    }
}

/// Where a pane's binding record moves while muxel handles it. The hook never
/// writes this name, so a record it writes meanwhile lands at the shared path.
fn claude_claimed_binding_path(data_dir: &Path, instance_id: Uuid) -> PathBuf {
    data_dir
        .join(CLAUDE_BINDING_DIR)
        .join(format!("{instance_id}.claimed.json"))
}

/// Claim the pane's next binding record: one claimed before whose handling
/// failed, else the shared record, moved to the claimed name by an atomic
/// rename. Renaming keeps its write time.
pub(crate) fn claim_claude_binding(
    data_dir: &Path,
    instance_id: Uuid,
) -> Option<ClaimedClaudeRecord> {
    try_claim_claude_binding(data_dir, instance_id)
        .ok()
        .flatten()
}

fn try_claim_claude_binding(
    data_dir: &Path,
    instance_id: Uuid,
) -> Result<Option<ClaimedClaudeRecord>> {
    let path = claude_claimed_binding_path(data_dir, instance_id);
    let shared = claude_binding_path(data_dir, instance_id);
    if !path.try_exists()? {
        match std::fs::rename(&shared, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("claim Claude conversation binding"),
        }
    }
    let bytes = std::fs::read(&path).context("read claimed Claude conversation binding")?;
    let modified = std::fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok();
    Ok(Some(ClaimedClaudeRecord {
        path,
        shared,
        bytes,
        modified,
    }))
}

/// What prelaunch recovery's handler did with one claimed record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveredClaudeRecord {
    /// Adopted and saved, or nothing to adopt: it names the saved
    /// conversation. Acknowledge it.
    Handled,
    /// It isn't adopted now (see [`defer_claude_binding`]). Recovery keeps it
    /// only while the process that wrote it survives the launch.
    NotYet,
}

/// Drain pending records before choosing launch arguments, through the newest.
/// A failed handler leaves its claim intact and stops the launch. A record that
/// can't be adopted yet names a conversation of the process that wrote it. It
/// stays claimed for a later retry, and the launch uses the saved binding, only
/// when `writer_survives` (a reattach to that process's live tmux session),
/// unless the hook has written a newer record, which supersedes it. Any other
/// launch starts a new process, which is in none of the old one's
/// conversations, so the record goes, as one that can never validate does.
///
/// A handled record's binding is already saved, so failing to acknowledge it is
/// logged and the launch goes ahead; the claim stays for a later retry, where it
/// names the saved conversation. Only a newer record waiting behind it stops
/// the launch: it can't be claimed past the stuck one, and launching would
/// resume the conversation it replaced. So do claim and read failures, which
/// leave a pending record unread.
pub(crate) fn recover_claude_bindings_before_resume(
    data_dir: &Path,
    instance_id: Uuid,
    writer_survives: impl FnOnce() -> bool,
    mut adopt: impl FnMut(&ClaimedClaudeRecord) -> Result<RecoveredClaudeRecord>,
) -> Result<()> {
    // Asked only for a record that can't be adopted yet, and at most once: an
    // answer of yes keeps that record and ends recovery.
    let mut writer_survives = Some(writer_survives);
    while let Some(record) = try_claim_claude_binding(data_dir, instance_id)? {
        if adopt(&record)? == RecoveredClaudeRecord::NotYet
            && !claude_record_is_malformed(&record.bytes)
            && !record.superseded()?
            && writer_survives.take().is_some_and(|survives| survives())
        {
            return Ok(());
        }
        match std::fs::remove_file(&record.path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                if record.superseded()? {
                    return Err(error).context("acknowledge recovered Claude conversation");
                }
                log::warn!(
                    "could not acknowledge a recovered Claude conversation; launching with the saved binding: {error}"
                );
                return Ok(());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Delete a claimed record once it has been handled. Only the claimed file
/// goes; a record the hook has written since waits for the next claim.
pub(crate) fn acknowledge_claude_binding(record: ClaimedClaudeRecord) {
    let _ = std::fs::remove_file(record.path);
}

/// Keep a claimed record muxel can't adopt yet: its transcript doesn't
/// validate yet (`/clear` can run before Claude creates the project
/// directory), or another pane owns its conversation. The next claim retries
/// it, as a later read retried an unclaimed record, until the hook writes a
/// newer one: that supersedes it, so a record that never validates can't hold
/// back the next. A record that can never validate goes now.
pub(crate) fn defer_claude_binding(record: ClaimedClaudeRecord) {
    if claude_record_is_malformed(&record.bytes) || record.superseded().unwrap_or(false) {
        acknowledge_claude_binding(record);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ClaudeLookupBinding {
    pub id: String,
    pub path: PathBuf,
    pub pending_write: bool,
    /// Old producers did not assign each publication a distinct record id.
    pub legacy: bool,
    pub cwd: PathBuf,
}

pub(crate) type ClaudeHookIdentity = std::result::Result<Option<String>, String>;

pub(crate) struct ClaudeLookupSnapshot {
    pub record: ClaudeHookIdentity,
    pub binding: std::result::Result<Option<ClaudeLookupBinding>, String>,
}

struct ClaudeRecord {
    identity: String,
    bytes: Vec<u8>,
}

fn read_claude_record(path: &Path) -> std::result::Result<Option<ClaudeRecord>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let modified = file
        .metadata()
        .map_err(|error| error.to_string())?
        .modified()
        .ok();
    let mut bytes = Vec::new();
    file.take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_HOOK_INPUT_BYTES {
        return Err(format!(
            "Claude hook record exceeds {MAX_HOOK_INPUT_BYTES} bytes"
        ));
    }
    let (sign, time) = match modified.map(|time| time.duration_since(std::time::UNIX_EPOCH)) {
        None => (0_u8, std::time::Duration::ZERO),
        Some(Ok(time)) => (1, time),
        Some(Err(error)) => (2, error.duration()),
    };
    let mut hash = Sha256::new();
    hash.update(b"muxel-claude-hook-v1\0");
    hash.update([sign]);
    hash.update(time.as_secs().to_le_bytes());
    hash.update(time.subsec_nanos().to_le_bytes());
    hash.update(&bytes);
    Ok(Some(ClaudeRecord {
        identity: format!("v1:{:x}", hash.finalize()),
        bytes,
    }))
}

/// A bounded own-data receipt check. Transcript validation stays on the worker.
pub(crate) fn claude_hook_identity(data: &Path, pane: Uuid) -> ClaudeHookIdentity {
    let record = match read_claude_record(&claude_claimed_binding_path(data, pane))? {
        Some(record) => Some(record),
        None => read_claude_record(&claude_binding_path(data, pane))?,
    };
    Ok(record.map(|record| record.identity))
}

/// One directory snapshot, including no file opens for panes without a record.
/// Keep records after async reads: deleting by name could erase a newer hook.
pub(crate) fn claude_bindings_for_batch(
    data_dir: &Path,
    panes: &std::collections::HashSet<Uuid>,
    home: &Path,
) -> std::io::Result<std::collections::HashMap<Uuid, ClaudeLookupSnapshot>> {
    let mut result = std::collections::HashMap::new();
    #[cfg(test)]
    BINDING_BATCH_OPERATIONS.with_borrow_mut(|ops| ops.push("directory"));
    let entries = match std::fs::read_dir(data_dir.join(CLAUDE_BINDING_DIR)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(error) => return Err(error),
    };
    let mut selected = std::collections::HashMap::<Uuid, (bool, PathBuf)>::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some((id, claimed)) = name.to_str().and_then(|name| {
            let stem = name.strip_suffix(".json")?;
            let claimed = stem.ends_with(".claimed");
            let id = if claimed {
                stem.strip_suffix(".claimed")?
            } else {
                stem
            };
            Some((Uuid::parse_str(id).ok()?, claimed))
        }) else {
            continue;
        };
        if !panes.contains(&id) {
            continue;
        }
        if claimed || !selected.contains_key(&id) {
            selected.insert(id, (claimed, entry.path()));
        }
    }
    let mut projects = None;
    for (id, (_, path)) in selected {
        #[cfg(test)]
        BINDING_BATCH_OPERATIONS.with_borrow_mut(|ops| ops.push("read"));
        let snapshot = match read_claude_record(&path) {
            Ok(None) => continue,
            Err(error) => ClaudeLookupSnapshot {
                record: Err(error.clone()),
                binding: Err(error),
            },
            Ok(Some(record)) => {
                let binding = match serde_json::from_slice(&record.bytes) {
                    Err(_) => Ok(None),
                    Ok(event) => {
                        let projects = projects
                            .get_or_insert_with(|| home.join(".claude/projects").canonicalize());
                        match projects {
                            Ok(projects) => Ok(validated_claude_binding(event, projects)),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                            Err(error) => Err(error.to_string()),
                        }
                    }
                };
                ClaudeLookupSnapshot {
                    record: Ok(Some(record.identity)),
                    binding,
                }
            }
        };
        result.insert(id, snapshot);
    }
    Ok(result)
}

fn validated_claude_binding(event: Value, projects: &Path) -> Option<ClaudeLookupBinding> {
    validate_claude_event(&event).ok()?;
    let session_id = event.get("session_id")?.as_str()?;
    let source = event.get("source")?.as_str()?;
    let transcript = Path::new(event.get("transcript_path")?.as_str()?);
    let expected_name = format!("{session_id}.jsonl");
    if transcript.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return None;
    }

    // `/clear` and `/fork` may announce the new UUID before Claude writes its
    // first transcript record. Validate the existing project directory in that
    // case; resumed sessions must already have a real transcript.
    let project = transcript.parent()?.canonicalize().ok()?;
    if project.parent()? != projects {
        return None;
    }
    let pending_write = match transcript.canonicalize() {
        Ok(resolved) if resolved.is_file() && resolved.parent() == Some(project.as_path()) => false,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && matches!(source, "clear" | "fork") =>
        {
            true
        }
        _ => return None,
    };
    Some(ClaudeLookupBinding {
        id: session_id.to_string(),
        path: transcript.to_path_buf(),
        pending_write,
        legacy: claude_hook_stamp(&event).is_none()
            && event
                .get("muxel_record_id")
                .and_then(Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok())
                .is_none(),
        cwd: PathBuf::from(event.get("cwd")?.as_str()?),
    })
}

#[cfg(test)]
thread_local! {
    static BINDING_BATCH_OPERATIONS: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxel_core::tmux::Socket;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxel-claude-binding-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Run the hook as Claude does on `/resume <session_id>` in `cwd`.
    fn resume_hook(data: &Path, home: &Path, cwd: &Path, instance_id: Uuid, session_id: &str) {
        let transcript = muxel_core::claude_session_path(home, cwd, session_id);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let event = json!({
            "session_id": session_id,
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        write_claude_binding_from_reader(
            data,
            instance_id,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();
    }

    #[test]
    fn claimed_hook_precedes_new_shared_record_in_launch_batch() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let pane = Uuid::new_v4();
        let first = Uuid::new_v4().to_string();
        let second = Uuid::new_v4().to_string();
        resume_hook(&data, &home, &cwd, pane, &first);
        let first_receipt = claude_hook_identity(&data, pane).unwrap();
        let claimed = claim_claude_binding(&data, pane).unwrap();
        resume_hook(&data, &home, &cwd, pane, &second);

        let snapshots =
            claude_bindings_for_batch(&data, &std::collections::HashSet::from([pane]), &home)
                .unwrap();
        let snapshot = snapshots.get(&pane).unwrap();
        assert_eq!(snapshot.record, Ok(first_receipt.clone()));
        let binding = snapshot.binding.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(binding.id, first);
        assert!(!binding.legacy);
        assert_eq!(claude_hook_identity(&data, pane), Ok(first_receipt));
        drop(claimed);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn claimed_session(record: &ClaimedClaudeRecord, home: &Path) -> String {
        claude_binding_from_record(&record.bytes, home)
            .unwrap()
            .session_id
    }

    struct PendingResume {
        root: PathBuf,
        data: PathBuf,
        home: PathBuf,
        workspace_path: PathBuf,
        instance_id: Uuid,
        old_id: String,
        new_id: String,
    }

    impl PendingResume {
        fn new() -> Self {
            let root = temp_dir();
            let data = root.join("data");
            let home = root.join("home");
            let cwd = root.join("project");
            let mut instance = muxel_core::Instance::from_preset(
                Uuid::new_v4(),
                &muxel_core::AgentPreset::claude(),
            );
            let instance_id = instance.id;
            let old_id = Uuid::new_v4().to_string();
            let new_id = Uuid::new_v4().to_string();
            instance.session_id = Some(old_id.clone());
            instance.session_started = true;
            let mut workspace = muxel_core::Workspace::default();
            workspace.instances.push(instance);
            let workspace_path = root.join("workspace.json");
            muxel_store::save_workspace_to(&workspace_path, &workspace).unwrap();
            resume_hook(&data, &home, &cwd, instance_id, &old_id);
            drop(claim_claude_binding(&data, instance_id).unwrap());
            resume_hook(&data, &home, &cwd, instance_id, &new_id);
            Self {
                root,
                data,
                home,
                workspace_path,
                instance_id,
                old_id,
                new_id,
            }
        }

        fn workspace(&self) -> muxel_core::Workspace {
            muxel_store::load_workspace_from(&self.workspace_path).unwrap()
        }

        /// The production handler's outcomes: a record that doesn't validate
        /// yet isn't adopted yet; any other is saved, then handled.
        fn persist_record(
            &self,
            workspace: &mut muxel_core::Workspace,
            record: &ClaimedClaudeRecord,
        ) -> Result<RecoveredClaudeRecord> {
            let Some(binding) = claude_binding_from_record(&record.bytes, &self.home) else {
                return Ok(RecoveredClaudeRecord::NotYet);
            };
            let instance = workspace.instance_mut(self.instance_id).unwrap();
            instance.session_id = Some(binding.session_id);
            instance.session_started = true;
            muxel_store::save_workspace_to(&self.workspace_path, workspace)?;
            Ok(RecoveredClaudeRecord::Handled)
        }

        fn saved_id(&self) -> Option<String> {
            self.workspace()
                .instance(self.instance_id)
                .unwrap()
                .session_id
                .clone()
        }

        /// Run the hook as Claude does on `/clear`, which can announce the new
        /// conversation before Claude creates its project directory.
        fn clear_hook(&self, session_id: &str, cwd: &Path) {
            let transcript = muxel_core::claude_session_path(&self.home, cwd, session_id);
            let event = json!({
                "session_id": session_id,
                "transcript_path": transcript,
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": "clear"
            });
            write_claude_binding_from_reader(
                &self.data,
                self.instance_id,
                std::io::Cursor::new(event.to_string()),
            )
            .unwrap();
        }

        /// Drain the old record, leaving nothing pending.
        fn drain(&self) {
            let mut workspace = self.workspace();
            recover_claude_bindings_before_resume(
                &self.data,
                self.instance_id,
                || false,
                |record| self.persist_record(&mut workspace, record),
            )
            .unwrap();
            assert!(claim_claude_binding(&self.data, self.instance_id).is_none());
        }

        fn resume_args(&self, workspace: &muxel_core::Workspace) -> Vec<String> {
            muxel_core::session_resume_args(
                &muxel_core::AgentPreset::claude(),
                workspace.instance(self.instance_id).unwrap(),
            )
            .unwrap()
        }
    }

    impl Drop for PendingResume {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn prelaunch_recovery_selects_newer_binding_after_a_claimed_record() {
        let pending = PendingResume::new();
        let mut workspace = pending.workspace();
        let adoptable = || unreachable!("asked whether a record it adopts survives");
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            adoptable,
            |record| pending.persist_record(&mut workspace, record),
        )
        .unwrap();
        assert_eq!(
            pending.resume_args(&workspace),
            vec!["--resume".to_string(), pending.new_id.clone()]
        );
        assert_eq!(
            pending.resume_args(&pending.workspace()),
            pending.resume_args(&workspace)
        );
        assert!(claim_claude_binding(&pending.data, pending.instance_id).is_none());
    }

    #[test]
    fn prelaunch_recovery_stops_on_a_partial_persistence_failure() {
        let pending = PendingResume::new();
        let mut workspace = pending.workspace();
        let mut handled = Vec::new();
        let result = recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| {
                let session = claimed_session(record, &pending.home);
                handled.push(session.clone());
                if session == pending.new_id {
                    bail!("simulated workspace save failure");
                }
                pending.persist_record(&mut workspace, record)
            },
        );
        assert!(
            result.is_err(),
            "failed recovery must prevent launch argument selection"
        );
        assert_eq!(
            handled,
            vec![pending.old_id.clone(), pending.new_id.clone()]
        );
        assert_eq!(
            pending
                .workspace()
                .instance(pending.instance_id)
                .unwrap()
                .session_id
                .as_deref(),
            Some(pending.old_id.as_str())
        );
        let retry = claim_claude_binding(&pending.data, pending.instance_id).unwrap();
        assert_eq!(claimed_session(&retry, &pending.home), pending.new_id);
        drop(retry);
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| pending.persist_record(&mut workspace, record),
        )
        .unwrap();
        assert_eq!(
            pending.resume_args(&workspace),
            vec!["--resume".to_string(), pending.new_id.clone()]
        );
    }

    /// Make a claimed record impossible to acknowledge: a directory takes its
    /// place, which `remove_file` refuses on every platform.
    fn block_acknowledgement(record: &ClaimedClaudeRecord) -> Result<()> {
        std::fs::remove_file(&record.path)?;
        std::fs::create_dir(&record.path)?;
        Ok(())
    }

    #[test]
    fn prelaunch_recovery_stops_if_a_newer_record_waits_behind_a_failed_acknowledgement() {
        let pending = PendingResume::new();
        let mut workspace = pending.workspace();
        let mut handled = 0;
        let result = recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| {
                handled += 1;
                let outcome = pending.persist_record(&mut workspace, record)?;
                block_acknowledgement(record)?;
                Ok(outcome)
            },
        );
        // The newer record can't be claimed past the stuck one, and the saved
        // binding is the conversation it replaced.
        assert!(result.unwrap_err().to_string().contains("acknowledge"));
        assert_eq!(handled, 1);
        assert_eq!(pending.saved_id(), Some(pending.old_id.clone()));
        assert!(claude_binding_path(&pending.data, pending.instance_id).exists());
    }

    #[test]
    fn prelaunch_recovery_launches_after_failing_to_acknowledge_a_saved_binding() {
        let pending = PendingResume::new();
        pending.drain();
        let cwd = pending.root.join("project");
        let newest = Uuid::new_v4().to_string();
        resume_hook(
            &pending.data,
            &pending.home,
            &cwd,
            pending.instance_id,
            &newest,
        );
        let mut workspace = pending.workspace();
        let mut handled = 0;
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| {
                handled += 1;
                let outcome = pending.persist_record(&mut workspace, record)?;
                block_acknowledgement(record)?;
                Ok(outcome)
            },
        )
        .expect("a saved binding whose record can't be removed still launches");
        assert_eq!(handled, 1, "the stuck claim isn't retried in a loop");
        assert_eq!(
            pending.resume_args(&pending.workspace()),
            vec!["--resume".to_string(), newest]
        );
    }

    #[test]
    fn prelaunch_recovery_keeps_an_unadoptable_record_only_for_its_live_writer() {
        let pending = PendingResume::new();
        pending.drain();
        // `/clear` before Claude has created the cwd's project directory.
        let cwd = pending.root.join("fresh-project");
        let cleared = Uuid::new_v4().to_string();
        pending.clear_hook(&cleared, &cwd);
        let recover = |writer_survives: bool| {
            let mut workspace = pending.workspace();
            recover_claude_bindings_before_resume(
                &pending.data,
                pending.instance_id,
                || writer_survives,
                |record| pending.persist_record(&mut workspace, record),
            )
        };
        // Reattaching the live tmux session that wrote it keeps it for a retry.
        recover(true).expect("launch goes ahead with the saved binding");
        assert_eq!(pending.saved_id(), Some(pending.new_id.clone()));
        let kept = claim_claude_binding(&pending.data, pending.instance_id)
            .expect("the record is kept for a retry");
        assert_eq!(claude_binding_from_record(&kept.bytes, &pending.home), None);
        drop(kept);
        // Once the directory exists, that process's record validates.
        let transcript = muxel_core::claude_session_path(&pending.home, &cwd, &cleared);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        recover(true).unwrap();
        assert_eq!(pending.saved_id(), Some(cleared));
        assert!(claim_claude_binding(&pending.data, pending.instance_id).is_none());
    }

    #[test]
    fn a_fresh_process_never_inherits_its_predecessors_unadoptable_record() {
        let pending = PendingResume::new();
        pending.drain();
        // A direct pane in a new directory: `/clear` names C before Claude
        // creates the project directory, then the pane restarts untouched.
        let cwd = pending.root.join("fresh-project");
        let cleared = Uuid::new_v4().to_string();
        pending.clear_hook(&cleared, &cwd);
        let mut workspace = pending.workspace();
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| pending.persist_record(&mut workspace, record),
        )
        .unwrap();
        assert_eq!(pending.saved_id(), Some(pending.new_id.clone()));
        // The new process writes its own transcript in that directory, which
        // would make C's record validate. Nothing is left to adopt it.
        let transcript = muxel_core::claude_session_path(&pending.home, &cwd, &cleared);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        assert!(read_pending_claude_binding(&pending.data, pending.instance_id).is_none());

        // `/resume X` while a sibling owns X: not adoptable yet, and gone once
        // the pane relaunches, so a later release of X never rebinds it.
        let owned = Uuid::new_v4().to_string();
        resume_hook(
            &pending.data,
            &pending.home,
            &pending.root.join("project"),
            pending.instance_id,
            &owned,
        );
        let sibling_owns = |_: &ClaimedClaudeRecord| Ok(RecoveredClaudeRecord::NotYet);
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || true,
            sibling_owns,
        )
        .unwrap();
        assert!(
            claim_claude_binding(&pending.data, pending.instance_id).is_some(),
            "a live writer keeps it"
        );
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            sibling_owns,
        )
        .unwrap();
        assert!(read_pending_claude_binding(&pending.data, pending.instance_id).is_none());
        assert_eq!(pending.saved_id(), Some(pending.new_id.clone()));
    }

    #[test]
    fn a_kept_record_gives_way_to_a_newer_one_and_a_malformed_one_goes() {
        let pending = PendingResume::new();
        pending.drain();
        let cwd = pending.root.join("fresh-project");
        pending.clear_hook(&Uuid::new_v4().to_string(), &cwd);
        // The recurring reader claims it and can't adopt it yet.
        defer_claude_binding(claim_claude_binding(&pending.data, pending.instance_id).unwrap());
        assert!(claim_claude_binding(&pending.data, pending.instance_id).is_some());
        // `/resume` before the cleared conversation ever validated: the newer
        // record supersedes the kept one.
        let resumed = Uuid::new_v4().to_string();
        resume_hook(
            &pending.data,
            &pending.home,
            &pending.root.join("project"),
            pending.instance_id,
            &resumed,
        );
        defer_claude_binding(claim_claude_binding(&pending.data, pending.instance_id).unwrap());
        let next = claim_claude_binding(&pending.data, pending.instance_id).unwrap();
        assert_eq!(claimed_session(&next, &pending.home), resumed);
        acknowledge_claude_binding(next);

        // Prelaunch recovery reaches the newer record the same way.
        pending.clear_hook(&Uuid::new_v4().to_string(), &cwd);
        drop(claim_claude_binding(&pending.data, pending.instance_id).unwrap());
        let newest = Uuid::new_v4().to_string();
        resume_hook(
            &pending.data,
            &pending.home,
            &pending.root.join("project"),
            pending.instance_id,
            &newest,
        );
        let mut workspace = pending.workspace();
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| pending.persist_record(&mut workspace, record),
        )
        .unwrap();
        assert_eq!(pending.saved_id(), Some(newest));
        assert!(claim_claude_binding(&pending.data, pending.instance_id).is_none());

        // A record that isn't a SessionStart event never will be.
        let shared = claude_binding_path(&pending.data, pending.instance_id);
        std::fs::write(&shared, b"not json").unwrap();
        defer_claude_binding(claim_claude_binding(&pending.data, pending.instance_id).unwrap());
        assert!(claim_claude_binding(&pending.data, pending.instance_id).is_none());
        std::fs::write(&shared, b"not json").unwrap();
        pending.drain();
    }

    #[test]
    fn hook_settings_use_exec_form_and_resume_sources() {
        let settings = claude_hook_settings_for(Path::new("C:/Program Files/muxel.exe"));
        let value: Value = serde_json::from_str(&settings).unwrap();
        let handler = &value["hooks"]["SessionStart"][0];
        assert_eq!(handler["matcher"], "resume|clear|fork");
        assert_eq!(handler["hooks"][0]["command"], "C:/Program Files/muxel.exe");
        assert_eq!(handler["hooks"][0]["args"][0], CLAUDE_HOOK_FLAG);
        assert_eq!(handler["hooks"][0]["args"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn hook_settings_are_passed_by_batch_safe_file() {
        let root = temp_dir();
        let executable = Path::new("C:/Program Files/muxel.exe");
        let path = write_claude_hook_settings_file(&root, executable).unwrap();
        let same_path = write_claude_hook_settings_file(&root, executable).unwrap();
        let argument = path.to_string_lossy();
        assert!(windows_batch_arg_is_safe(&argument));
        assert!(!argument.contains("SessionStart"));
        assert_eq!(path, same_path);
        assert_ne!(
            path,
            claude_hook_settings_path(&root, Path::new("C:/Other/muxel.exe"))
        );

        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let hook = &value["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(hook["command"], "C:/Program Files/muxel.exe");
        assert_eq!(hook["args"][0], CLAUDE_HOOK_FLAG);
        assert_eq!(hook["args"].as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn valid_resume_event_accepts_a_transcript_from_another_claude_project() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4().to_string();
        let transcript_cwd = root.join("moved-worktree");
        let transcript = muxel_core::claude_session_path(&home, &transcript_cwd, &session_id);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let event = json!({
            "session_id": session_id,
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        write_claude_binding_from_reader(
            &data,
            instance_id,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();

        assert_eq!(
            read_claude_binding(&data, instance_id, &home)
                .map(|binding| binding.session_id)
                .as_deref(),
            Some(session_id.as_str())
        );

        let outside = root.join(format!("{session_id}.jsonl"));
        std::fs::write(&outside, "{}\n").unwrap();
        let outside_event = json!({
            "session_id": session_id,
            "transcript_path": outside,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        write_claude_binding_from_reader(
            &data,
            instance_id,
            std::io::Cursor::new(outside_event.to_string()),
        )
        .unwrap();
        assert_eq!(
            read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
            None
        );

        write_claude_binding_from_reader(
            &data,
            instance_id,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();
        std::fs::remove_file(&transcript).unwrap();
        assert_eq!(
            read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
            None
        );

        for source in ["clear", "fork"] {
            let pending_event = json!({
                "session_id": session_id,
                "transcript_path": transcript,
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": source
            });
            write_claude_binding_from_reader(
                &data,
                instance_id,
                std::io::Cursor::new(pending_event.to_string()),
            )
            .unwrap();
            assert_eq!(
                read_claude_binding(&data, instance_id, &home)
                    .map(|binding| binding.session_id)
                    .as_deref(),
                Some(session_id.as_str()),
                "{source} should bind before Claude writes the first transcript record"
            );
        }
        let record = claim_claude_binding(&data, instance_id).unwrap();
        acknowledge_claude_binding(record);
        assert!(!claude_binding_path(&data, instance_id).exists());
        assert!(claim_claude_binding(&data, instance_id).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_to_b_to_a_between_polls_reads_as_a_switch() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();
        let resume = |session_id: &str| resume_hook(&data, &home, &cwd, instance_id, session_id);

        // `claude --resume A` runs the hook as it launches, naming A.
        let before = SystemTime::now();
        resume(&a);
        let first = read_claude_binding(&data, instance_id, &home).unwrap();
        let stamp = first.stamp.expect("the hook stamps its record");
        assert!(before <= stamp.recorded_at && stamp.recorded_at <= SystemTime::now());
        assert!(!claude_record_switches(&first, None, Some(a.as_str())));

        // `/resume B` and then `/resume A`, both before muxel's next read.
        resume(&b);
        resume(&a);
        let last = read_claude_binding(&data, instance_id, &home).unwrap();
        assert_eq!(last.session_id, a);
        assert!(
            claude_record_switches(&last, Some(stamp.nonce), Some(a.as_str())),
            "a record muxel hasn't seen is a switch, whatever it names"
        );
        // The same record read again, because removing it failed, is not.
        let nonce = last.stamp.map(|stamp| stamp.nonce);
        assert!(!claude_record_switches(&last, nonce, Some(a.as_str())));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_older_hooks_record_switches_only_to_another_conversation() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4().to_string();
        let transcript = muxel_core::claude_session_path(&home, &cwd, &session_id);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        // An older muxel's hook wrote Claude's event as it came.
        let path = claude_binding_path(&data, instance_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let event = json!({
            "session_id": session_id,
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        std::fs::write(&path, event.to_string()).unwrap();
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();

        let record = claim_claude_binding(&data, instance_id).unwrap();
        // Its write time stands in for the stamp; claiming it keeps that.
        assert_eq!(record.modified, Some(written));
        let binding = claude_binding_from_record(&record.bytes, &home).unwrap();
        assert_eq!(binding.stamp, None);
        let seen = Some(Uuid::new_v4());
        assert!(!claude_record_switches(
            &binding,
            seen,
            Some(session_id.as_str())
        ));
        let other = Uuid::new_v4().to_string();
        assert!(claude_record_switches(&binding, seen, Some(other.as_str())));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_record_written_while_muxel_handles_the_last_survives_its_acknowledgement() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();

        resume_hook(&data, &home, &cwd, instance_id, &a);
        let claimed = claim_claude_binding(&data, instance_id).unwrap();
        assert_eq!(claimed_session(&claimed, &home), a);
        // The hook runs again between muxel's read and its acknowledgement.
        resume_hook(&data, &home, &cwd, instance_id, &b);
        acknowledge_claude_binding(claimed);

        let next = claim_claude_binding(&data, instance_id).expect("B wasn't deleted unread");
        assert_eq!(claimed_session(&next, &home), b);
        acknowledge_claude_binding(next);
        assert!(claim_claude_binding(&data, instance_id).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_record_whose_handling_failed_is_claimed_again_before_a_newer_one() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();

        resume_hook(&data, &home, &cwd, instance_id, &a);
        let claimed = claim_claude_binding(&data, instance_id).unwrap();
        // Saving the binding failed, so the record isn't acknowledged; the
        // hook runs again before the next tick.
        drop(claimed);
        resume_hook(&data, &home, &cwd, instance_id, &b);

        let retry = claim_claude_binding(&data, instance_id).unwrap();
        assert_eq!(claimed_session(&retry, &home), a);
        acknowledge_claude_binding(retry);
        let next = claim_claude_binding(&data, instance_id).unwrap();
        assert_eq!(claimed_session(&next, &home), b);
        acknowledge_claude_binding(next);
        assert!(claim_claude_binding(&data, instance_id).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_non_resume_events_and_mismatched_transcript_names() {
        let root = temp_dir();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        for event in [
            json!({
                "session_id": session_id,
                "transcript_path": root.join(format!("{session_id}.jsonl")),
                "cwd": root,
                "hook_event_name": "SessionStart",
                "source": "startup"
            }),
            json!({
                "session_id": session_id,
                "transcript_path": root.join("other.jsonl"),
                "cwd": root,
                "hook_event_name": "SessionStart",
                "source": "resume"
            }),
        ] {
            assert!(
                write_claude_binding_from_reader(
                    &root,
                    instance_id,
                    std::io::Cursor::new(event.to_string()),
                )
                .is_err()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_transcript_paths_that_escape_or_nest_below_the_project_store() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let projects = home.join(".claude/projects");
        std::fs::create_dir_all(&projects).unwrap();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4().to_string();
        let cwd = root.join("project");

        let outside = home.join(".claude").join(format!("{session_id}.jsonl"));
        std::fs::write(&outside, "{}\n").unwrap();
        let traversal = projects.join("..").join(format!("{session_id}.jsonl"));

        let nested = projects
            .join("project")
            .join("nested")
            .join(format!("{session_id}.jsonl"));
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "{}\n").unwrap();

        for transcript in [traversal, nested] {
            let event = json!({
                "session_id": session_id,
                "transcript_path": transcript,
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": "resume"
            });
            write_claude_binding_from_reader(
                &data,
                instance_id,
                std::io::Cursor::new(event.to_string()),
            )
            .unwrap();
            assert_eq!(
                read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
                None
            );
        }

        #[cfg(unix)]
        {
            let escaped = root.join("escaped-project");
            std::fs::create_dir_all(&escaped).unwrap();
            std::fs::write(escaped.join(format!("{session_id}.jsonl")), "{}\n").unwrap();
            let linked = projects.join("linked-project");
            std::os::unix::fs::symlink(&escaped, &linked).unwrap();
            let event = json!({
                "session_id": session_id,
                "transcript_path": linked.join(format!("{session_id}.jsonl")),
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": "resume"
            });
            write_claude_binding_from_reader(
                &data,
                instance_id,
                std::io::Cursor::new(event.to_string()),
            )
            .unwrap();
            assert_eq!(
                read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
                None
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pane_muxel_starts_itself_is_told_its_own_instance_id_and_no_other_carries_one() {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        let id = Uuid::new_v4();
        let configured = vec![
            pair("FOO", "bar"),
            pair("muxel_instance_id", "stale"),
            pair(MUXEL_INSTANCE_ID_ENV, "also-stale"),
        ];

        let (local, removed) = pane_env(&configured, Vec::new(), id, PaneProcess::Program, false);
        assert_eq!(
            local,
            vec![
                pair("FOO", "bar"),
                pair(MUXEL_INSTANCE_ID_ENV, &id.to_string())
            ]
        );
        assert!(removed.is_empty());
        // It is what the Claude hook reads back.
        assert_eq!(
            hook_instance_from_args(
                [OsString::from(CLAUDE_HOOK_FLAG)],
                Some(local[1].1.clone().into())
            ),
            Ok(Some(id))
        );

        // A tmux client has no id, set or inherited (tmux hands the id over).
        let (client, removed) =
            pane_env(&configured, Vec::new(), id, PaneProcess::TmuxClient, false);
        assert_eq!(client, vec![pair("FOO", "bar")]);
        assert_eq!(removed, [MUXEL_INSTANCE_ID_ENV]);

        // Nor has an ssh client, which an ssh `SendEnv` could forward it from. What
        // the launch needs, such as ssh's password, is kept.
        let launch = vec![pair("SSHPASS", "pw")];
        let (remote, removed) = pane_env(&configured, launch, id, PaneProcess::SshClient, false);
        assert_eq!(remote, vec![pair("FOO", "bar"), pair("SSHPASS", "pw")]);
        assert_eq!(removed, [MUXEL_INSTANCE_ID_ENV]);
    }

    #[test]
    fn a_tmux_panes_program_gets_its_own_id_or_none_never_the_servers() {
        let id = Uuid::new_v4();
        let own = format!("{MUXEL_INSTANCE_ID_ENV}={id}");
        let clear = ["set-environment", "-g", "-u", MUXEL_INSTANCE_ID_ENV, ";"];
        // After `-u set -g mouse on ;`, before the session is created.
        let args = tmux_pane_args(Socket::Default, "s", Some("claude"), &[], id, true, None)
            .unwrap()
            .args;
        assert_eq!(args[6..11], clear);
        assert_eq!(
            args[11..],
            ["new-session", "-A", "-s", "s", "-e", &own, "--", "claude"]
        );
        // Older tmux has no `-e`: the program starts without an id rather than
        // with whichever pane's the server had.
        let args = tmux_pane_args(Socket::Default, "s", None, &[], id, false, None)
            .unwrap()
            .args;
        assert_eq!(args[6..11], clear);
        assert_eq!(args[11..], ["new-session", "-A", "-s", "s"]);
    }

    /// muxel's own server is named ahead of everything else the client is told; a
    /// pane from before it had one reattaches on the default server with exactly
    /// the argv it always had.
    #[test]
    fn a_tmux_pane_names_the_server_its_session_is_on() {
        let id = Uuid::new_v4();
        let private = tmux_pane_args(Socket::Private, "s", Some("claude"), &[], id, true, None)
            .unwrap()
            .args;
        let default = tmux_pane_args(Socket::Default, "s", Some("claude"), &[], id, true, None)
            .unwrap()
            .args;
        assert_eq!(private[..2], ["-L", "mxl"]);
        assert_eq!(private[2..], default[..]);
        assert_eq!(default[0], "-u");
    }

    #[test]
    fn isolated_tmux_sets_profile_only_on_its_session_and_client() {
        let id = Uuid::new_v4();
        let root = Path::new("/isolated root/quote'and;equals=dir");
        let argv = vec!["two words".into(), "".into(), "--flag".into()];
        let launch = tmux_pane_args(
            Socket::Default,
            "s",
            Some("agent"),
            &argv,
            id,
            true,
            Some(root),
        )
        .unwrap();
        assert_eq!(&launch.args[..2], Socket::isolated(root).args());
        let new = launch
            .args
            .iter()
            .position(|arg| arg == "new-session")
            .unwrap();
        assert_eq!(&launch.args[new..new + 4], ["new-session", "-A", "-s", "s"]);
        for (i, (key, value)) in launch.env.iter().enumerate() {
            assert_eq!(launch.args[new + 4 + i * 2], "-e");
            assert_eq!(launch.args[new + 5 + i * 2], format!("{key}={value}"));
        }
        assert_eq!(launch.env.len(), 3);
        assert_eq!(
            &launch.args[new + 10..],
            ["--", "agent", "two words", "", "--flag"]
        );
        // No profile values are installed globally on this profile's server.
        assert_eq!(
            &launch.args[2..new],
            [
                "-u",
                "set",
                "-g",
                "mouse",
                "on",
                ";",
                "set-environment",
                "-g",
                "-u",
                MUXEL_INSTANCE_ID_ENV,
                ";"
            ]
        );
        let configured = vec![
            ("MUXEL_CONTROL".into(), "foreign.json".into()),
            ("muxel_control".into(), "also-foreign.json".into()),
            ("muxel_user_data_dir".into(), "".into()),
        ];
        let (client, removed) =
            pane_env(&configured, launch.env, id, PaneProcess::TmuxClient, true);
        assert_eq!(removed, [MUXEL_INSTANCE_ID_ENV]);
        assert!(!client.iter().any(|(key, _)| key == MUXEL_INSTANCE_ID_ENV));
        for key in [muxel_store::USER_DATA_DIR_ENV, crate::control::CONTROL_ENV] {
            assert_eq!(
                client
                    .iter()
                    .filter(|(name, _)| name.eq_ignore_ascii_case(key))
                    .count(),
                1
            );
        }
        assert_eq!(
            client.last().unwrap(),
            &(
                "MUXEL_CONTROL".into(),
                root.join("data")
                    .join(muxel_core::control::ENDPOINT_FILE)
                    .to_str()
                    .unwrap()
                    .into()
            )
        );
        assert!(tmux_pane_args(Socket::Default, "s", None, &[], id, false, Some(root)).is_err());
    }

    #[test]
    fn isolated_tmux_client_keeps_the_root_while_argv_escapes_its_separator() {
        let root = Path::new("/isolated;");
        let launch = tmux_pane_args(
            Socket::Default,
            "s",
            None,
            &[],
            Uuid::new_v4(),
            true,
            Some(root),
        )
        .unwrap();
        assert!(
            launch
                .args
                .contains(&r"MUXEL_USER_DATA_DIR=/isolated\;".to_string())
        );
        assert!(
            launch
                .env
                .contains(&("MUXEL_USER_DATA_DIR".into(), "/isolated;".into()))
        );
        let endpoint = root.join("data").join(muxel_core::control::ENDPOINT_FILE);
        assert!(
            launch
                .args
                .contains(&format!("MUXEL_CONTROL={}", endpoint.display()))
        );
    }

    #[test]
    fn profile_overrides_remain_configurable_without_isolation() {
        let id = Uuid::new_v4();
        let configured = vec![
            ("muxel_user_data_dir".into(), "configured-root".into()),
            ("MUXEL_CONTROL".into(), "configured-control".into()),
            ("FOO".into(), "bar".into()),
        ];
        for process in [
            PaneProcess::Program,
            PaneProcess::TmuxClient,
            PaneProcess::SshClient,
        ] {
            let (env, removed) = pane_env(&configured, vec![], id, process, false);
            assert_eq!(&env[..configured.len()], &configured);
            assert!(!removed.iter().any(|key| key == crate::control::CONTROL_ENV));
        }
        for process in [PaneProcess::Program, PaneProcess::SshClient] {
            let (env, removed) = pane_env(
                &configured,
                vec![("muxel_control".into(), "untrusted-launch".into())],
                id,
                process,
                true,
            );
            assert!(!env.iter().any(|(key, _)| {
                key.eq_ignore_ascii_case(muxel_store::USER_DATA_DIR_ENV)
                    || key.eq_ignore_ascii_case(crate::control::CONTROL_ENV)
            }));
            assert!(env.contains(&("FOO".into(), "bar".into())));
            assert!(removed.contains(&crate::control::CONTROL_ENV.to_string()));
            assert_eq!(
                removed.contains(&MUXEL_INSTANCE_ID_ENV.to_string()),
                process == PaneProcess::SshClient
            );
        }
    }

    #[test]
    fn isolated_direct_preset_overrides_cannot_escape_in_spawned_pty_children() {
        let root = temp_dir();
        for value in ["", "foreign-profile"] {
            for lowercase in [false, true] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "session_binding::tests::isolated_direct_child_probe",
                        "--nocapture",
                    ])
                    .env("MUXEL_B3_PROBE_STAGE", "launch")
                    .env("MUXEL_B3_PROBE_ROOT", &root)
                    .env("MUXEL_B3_PRESET_VALUE", value)
                    .env("MUXEL_B3_LOWERCASE", lowercase.to_string())
                    .env(muxel_store::USER_DATA_DIR_ENV, &root)
                    .env(crate::control::CONTROL_ENV, "inherited-foreign.json")
                    .env(MUXEL_INSTANCE_ID_ENV, "inherited-wrong-id")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "value={value:?}, lowercase={lowercase}: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn isolated_direct_child_probe() {
        let Ok(stage) = std::env::var("MUXEL_B3_PROBE_STAGE") else {
            return;
        };
        let root = std::env::var_os("MUXEL_B3_PROBE_ROOT").unwrap();
        if stage == "observe" {
            assert_eq!(
                std::env::var_os(muxel_store::USER_DATA_DIR_ENV),
                Some(root.clone())
            );
            assert_eq!(
                muxel_store::data_dir(),
                Some(PathBuf::from(root).join("data"))
            );
            assert!(std::env::var_os(crate::control::CONTROL_ENV).is_none());
            assert_eq!(
                std::env::var(MUXEL_INSTANCE_ID_ENV).unwrap(),
                std::env::var("MUXEL_B3_EXPECTED_ID").unwrap()
            );
            println!("B3_ISOLATED_CHILD_OK");
            return;
        }
        let lowercase = std::env::var("MUXEL_B3_LOWERCASE").unwrap() == "true";
        let value = std::env::var("MUXEL_B3_PRESET_VALUE").unwrap();
        let keys = [muxel_store::USER_DATA_DIR_ENV, crate::control::CONTROL_ENV];
        let configured: Vec<_> = keys
            .into_iter()
            .map(|key| {
                (
                    if lowercase {
                        key.to_ascii_lowercase()
                    } else {
                        key.to_string()
                    },
                    value.clone(),
                )
            })
            .collect();
        let id = Uuid::new_v4();
        let (env, env_remove) = pane_env(&configured, Vec::new(), id, PaneProcess::Program, true);
        let mut spec = muxel_terminal::CommandSpec::program(
            std::env::current_exe().unwrap().to_str().unwrap(),
            vec![
                "--exact".into(),
                "session_binding::tests::isolated_direct_child_probe".into(),
                "--nocapture".into(),
            ],
        );
        spec.env = env;
        spec.env.extend([
            ("MUXEL_B3_PROBE_STAGE".into(), "observe".into()),
            ("MUXEL_B3_EXPECTED_ID".into(), id.to_string()),
        ]);
        spec.env_remove = env_remove;
        let (_session, rx) = muxel_terminal::TerminalSession::spawn(spec, 100, 30).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut output = Vec::new();
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "child timed out: {}",
                String::from_utf8_lossy(&output)
            );
            match rx.try_recv() {
                Ok(muxel_terminal::PtyChunk::Output(bytes)) => output.extend(bytes),
                Ok(muxel_terminal::PtyChunk::Exit {
                    code, read_error, ..
                }) => {
                    assert_eq!(code, Some(0), "{}", String::from_utf8_lossy(&output));
                    assert!(read_error.is_none());
                    assert!(String::from_utf8_lossy(&output).contains("B3_ISOLATED_CHILD_OK"));
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }

    #[test]
    fn hook_mode_requires_one_valid_instance_uuid() {
        let id = Uuid::new_v4();
        assert_eq!(
            hook_instance_from_args(
                [OsString::from(CLAUDE_HOOK_FLAG), id.to_string().into()],
                None
            ),
            Ok(Some(id))
        );
        assert_eq!(
            hook_instance_from_args(
                [OsString::from(CLAUDE_HOOK_FLAG)],
                Some(id.to_string().into())
            ),
            Ok(Some(id))
        );
        assert!(hook_instance_from_args([OsString::from(CLAUDE_HOOK_FLAG)], None).is_err());
        assert_eq!(
            hook_instance_from_args([OsString::from("--other")], Some(id.to_string().into())),
            Ok(None)
        );
    }
}
