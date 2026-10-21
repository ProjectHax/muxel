//! Exact provider session bindings that must survive an in-process `/resume`.

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::{Hash as _, Hasher as _};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CLAUDE_BINDING_DIR: &str = "provider-session-bindings/claude";
const CLAUDE_SETTINGS_DIR: &str = "provider-session-bindings/claude/settings";
const CLAUDE_HOOK_FLAG: &str = "--claude-session-hook";
const MAX_HOOK_INPUT_BYTES: u64 = 64 * 1024;
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

fn write_claude_binding_from_reader(
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
    let event: Value = serde_json::from_str(&input).context("parsing Claude SessionStart input")?;
    validate_claude_event(&event)?;

    let path = claude_binding_path(data_dir, instance_id);
    let parent = path.parent().context("Claude binding path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating Claude binding directory {}", parent.display()))?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temp, input.as_bytes())
        .with_context(|| format!("writing Claude binding {}", temp.display()))?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("replacing Claude binding {}", path.display()))?;
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

pub(crate) fn claude_session_id_from_binding(
    data_dir: &Path,
    instance_id: Uuid,
    home: &Path,
) -> Option<String> {
    let event: Value =
        serde_json::from_slice(&std::fs::read(claude_binding_path(data_dir, instance_id)).ok()?)
            .ok()?;
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
    Some(session_id.to_string())
}

pub(crate) fn clear_claude_binding(data_dir: &Path, instance_id: Uuid) {
    let _ = std::fs::remove_file(claude_binding_path(data_dir, instance_id));
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
            claude_session_id_from_binding(&data, instance_id, &home).as_deref(),
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
            claude_session_id_from_binding(&data, instance_id, &home),
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
            claude_session_id_from_binding(&data, instance_id, &home),
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
                claude_session_id_from_binding(&data, instance_id, &home).as_deref(),
                Some(session_id.as_str()),
                "{source} should bind before Claude writes the first transcript record"
            );
        }
        clear_claude_binding(&data, instance_id);
        assert!(!claude_binding_path(&data, instance_id).exists());
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
                claude_session_id_from_binding(&data, instance_id, &home),
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
                claude_session_id_from_binding(&data, instance_id, &home),
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
