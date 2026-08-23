//! Exact provider session bindings that must survive an in-process `/resume`.

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CLAUDE_BINDING_DIR: &str = "provider-session-bindings/claude";
const CLAUDE_HOOK_FLAG: &str = "--claude-session-hook";
const MAX_HOOK_INPUT_BYTES: u64 = 64 * 1024;

pub(crate) fn hook_instance_from_args(
    args: impl IntoIterator<Item = OsString>,
) -> std::result::Result<Option<Uuid>, String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == CLAUDE_HOOK_FLAG {
            let raw = args
                .next()
                .ok_or_else(|| format!("{CLAUDE_HOOK_FLAG} requires an instance UUID"))?;
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

pub(crate) fn claude_hook_settings(instance_id: Uuid) -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    Some(claude_hook_settings_for(&executable, instance_id))
}

fn claude_hook_settings_for(executable: &Path, instance_id: Uuid) -> String {
    json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "resume|clear|fork",
                "hooks": [{
                    "type": "command",
                    "command": executable.to_string_lossy(),
                    "args": [CLAUDE_HOOK_FLAG, instance_id.to_string()],
                    "timeout": 5
                }]
            }]
        }
    })
    .to_string()
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

fn paths_loosely_equal(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    #[cfg(windows)]
    {
        let normalize = |path: &Path| {
            path.to_string_lossy()
                .replace('/', "\\")
                .trim_end_matches('\\')
                .to_ascii_lowercase()
        };
        normalize(a) == normalize(b)
    }
    #[cfg(not(windows))]
    {
        a.to_string_lossy().trim_end_matches('/') == b.to_string_lossy().trim_end_matches('/')
    }
}

pub(crate) fn claude_session_id_from_binding(
    data_dir: &Path,
    instance_id: Uuid,
    home: &Path,
    cwd: &Path,
) -> Option<String> {
    let event: Value =
        serde_json::from_slice(&std::fs::read(claude_binding_path(data_dir, instance_id)).ok()?)
            .ok()?;
    validate_claude_event(&event).ok()?;
    let session_id = event.get("session_id")?.as_str()?;
    let event_cwd = Path::new(event.get("cwd")?.as_str()?);
    let transcript = Path::new(event.get("transcript_path")?.as_str()?);
    if !paths_loosely_equal(event_cwd, cwd) {
        return None;
    }
    let expected = muxel_core::claude_session_path(home, cwd, session_id);
    if !paths_loosely_equal(transcript, &expected) || !transcript.is_file() {
        return None;
    }
    Some(session_id.to_string())
}

pub(crate) fn clear_claude_binding(data_dir: &Path, instance_id: Uuid) {
    let _ = std::fs::remove_file(claude_binding_path(data_dir, instance_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxel-claude-binding-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn hook_settings_use_exec_form_and_resume_sources() {
        let instance_id = Uuid::new_v4();
        let settings =
            claude_hook_settings_for(Path::new("C:/Program Files/muxel.exe"), instance_id);
        let value: Value = serde_json::from_str(&settings).unwrap();
        let handler = &value["hooks"]["SessionStart"][0];
        assert_eq!(handler["matcher"], "resume|clear|fork");
        assert_eq!(handler["hooks"][0]["command"], "C:/Program Files/muxel.exe");
        assert_eq!(handler["hooks"][0]["args"][0], CLAUDE_HOOK_FLAG);
        assert_eq!(handler["hooks"][0]["args"][1], instance_id.to_string());
    }

    #[test]
    fn valid_resume_event_round_trips_only_for_its_exact_cwd_and_transcript() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4().to_string();
        let transcript = muxel_core::claude_session_path(&home, &cwd, &session_id);
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
            claude_session_id_from_binding(&data, instance_id, &home, &cwd).as_deref(),
            Some(session_id.as_str())
        );
        assert_eq!(
            claude_session_id_from_binding(&data, instance_id, &home, &root.join("other")),
            None
        );
        std::fs::remove_file(&transcript).unwrap();
        assert_eq!(
            claude_session_id_from_binding(&data, instance_id, &home, &cwd),
            None
        );
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
    fn hook_mode_requires_one_valid_instance_uuid() {
        let id = Uuid::new_v4();
        assert_eq!(
            hook_instance_from_args([OsString::from(CLAUDE_HOOK_FLAG), id.to_string().into()]),
            Ok(Some(id))
        );
        assert!(hook_instance_from_args([OsString::from(CLAUDE_HOOK_FLAG)]).is_err());
        assert_eq!(
            hook_instance_from_args([OsString::from("--other")]),
            Ok(None)
        );
    }
}
