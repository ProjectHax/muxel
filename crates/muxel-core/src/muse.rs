//! Muse's root-session hook contract. No transcript contents are retained.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[cfg(test)]
mod tests {
    use super::{
        HookDocumentError, HookLaunch, MuseEvent, MuseState, has_hook_override, hook_command,
        hook_launch, merge_hook_document, metadata, session_log, title_state,
    };
    use serde_json::json;
    use std::path::PathBuf;

    const A: &str = "01a14039-60e8-7532-9187-0cd6f75f15a0";
    const B: &str = "01a1403e-b457-73d0-854d-cbda1483df9c";

    #[test]
    fn two_muse_panes_in_one_folder_bind_exact_ids() {
        for id in [A, B] {
            let event: MuseEvent = serde_json::from_value(json!({
                "session_id": id, "cwd": "C:/same-folder",
                "hook_event_name": "SessionStart", "prompt": "never retain this"
            }))
            .unwrap();
            let record = metadata(event, id).unwrap();
            assert_eq!(record.session_id, id);
            assert!(!serde_json::to_string(&record).unwrap().contains("prompt"));
        }
    }

    #[test]
    fn muse_observer_hooks_cannot_rebind_the_root_session() {
        let event: MuseEvent = serde_json::from_value(json!({
            "session_id": B, "cwd": "C:/same-folder", "hook_event_name": "Stop"
        }))
        .unwrap();
        assert!(metadata(event, A).is_none());
    }

    #[test]
    fn muse_session_discovery_reads_only_fixture_names_and_metadata() {
        let files = [
            PathBuf::from(format!("2026/10/15/{B}/session.jsonl")),
            PathBuf::from(format!("2026/10/14/{A}/session.jsonl")),
            PathBuf::from(format!(".msp-view-v1/{A}/session.jsonl")),
        ];
        assert_eq!(session_log(A, &files), Some(files[1].clone()));
        assert_eq!(session_log(B, &files), Some(files[0].clone()));
        assert!(session_log("../../elsewhere", &files).is_none());
        assert!(session_log(A, &files[..1]).is_none());
    }

    #[test]
    fn muse_hook_overlay_preserves_existing_handlers() {
        let prior = json!({"version":1,"hooks":{
            "SessionStart":[{"matcher":"resume","hooks":[{"type":"command","command":"user-hook"}]}],
            "PreCompact":[{"hooks":[{"type":"command","command":"compact-hook"}]}]
        }});
        let merged = merge_hook_document(prior.clone(), "muxel-hook").unwrap();
        assert_eq!(merged["version"], prior["version"]);
        assert_eq!(merged["hooks"]["PreCompact"], prior["hooks"]["PreCompact"]);
        assert_eq!(
            merged["hooks"]["SessionStart"][0],
            prior["hooks"]["SessionStart"][0]
        );
        assert_eq!(merged["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        assert_eq!(
            merged["hooks"]["Notification"][0]["matcher"],
            "permission_prompt"
        );
        assert!(merged["hooks"]["Stop"][0].get("matcher").is_none());
        assert!(merge_hook_document(json!({"hooks":[]}), "muxel-hook").is_err());
        assert!(merge_hook_document(json!({"hooks":{"Stop":{}}}), "muxel-hook").is_err());
    }

    #[test]
    fn muse_hook_overlay_declines_strict_managed_schema() {
        for marker in [json!({"name":"policy"}), json!({"timeout_ms":5000})] {
            let mut handler = json!({"type":"command","command":"policy-hook"});
            handler
                .as_object_mut()
                .unwrap()
                .extend(marker.as_object().unwrap().clone());
            let prior = json!({"hooks":{"SessionStart":[{"hooks":[handler]}]}});
            assert_eq!(
                merge_hook_document(prior, "muxel-hook"),
                Err(HookDocumentError::StrictSchema)
            );
        }
    }

    #[test]
    fn muse_config_overrides_cover_attached_and_separated_forms() {
        for key in [
            "managed_hooks_path=other.json",
            "hooks={}",
            "hooks.SessionStart=[]",
        ] {
            for args in [
                vec!["-c".into(), key.into()],
                vec![format!("-c{key}")],
                vec![format!("-c={key}")],
                vec!["--config".into(), key.into()],
                vec![format!("--config={key}")],
            ] {
                assert!(has_hook_override(&args), "{args:?}");
            }
        }
        assert!(has_hook_override(&["-c".into(), " hooks = {} ".into()]));
        for args in [
            vec!["--config".into(), "model=default".into()],
            vec!["-cmanaged_hooks_path_extra=1".into()],
            vec!["hooks={}".into()],
            vec!["--".into(), "-chooks={}".into()],
        ] {
            assert!(!has_hook_override(&args), "{args:?}");
        }
    }

    #[test]
    fn muse_tmux_reattach_preserves_the_surviving_launch() {
        let nonce = uuid::Uuid::parse_str(A).unwrap();
        assert_eq!(hook_launch(true, Some(nonce)), HookLaunch::Reattach(nonce));
        assert_eq!(hook_launch(false, Some(nonce)), HookLaunch::Start);
        assert_eq!(hook_launch(false, None), HookLaunch::Start);
        assert_eq!(hook_launch(true, None), HookLaunch::Unmanaged);
        assert_eq!(
            hook_launch(true, Some(uuid::Uuid::nil())),
            HookLaunch::Unmanaged
        );
    }

    #[test]
    fn muse_hook_command_quotes_exact_arguments_on_both_platforms() {
        use base64::Engine as _;

        let pane = uuid::Uuid::parse_str(A).unwrap();
        let nonce = uuid::Uuid::parse_str(B).unwrap();
        let executable = "C:/O’Brien/Muxel's app.exe";
        let data = "C:/data/$env:PATH; `literal`";
        let command = hook_command(executable, data, pane, nonce, true);
        let encoded = command
            .strip_prefix("powershell.exe -NoProfile -NonInteractive -EncodedCommand ")
            .unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(
            String::from_utf16(&units).unwrap(),
            format!(
                "& 'C:/O’’Brien/Muxel''s app.exe' '--muse-session-hook' '{pane}' 'C:/data/$env:PATH; `literal`' '{nonce}'; exit $LASTEXITCODE"
            )
        );
        assert_eq!(
            hook_command("/tmp/Muxel's app", "/tmp/data space", pane, nonce, false),
            format!("'/tmp/Muxel'\\''s app' --muse-session-hook {pane} '/tmp/data space' {nonce}")
        );
    }

    #[test]
    fn muse_recorded_titles_distinguish_idle_and_working() {
        assert_eq!(
            title_state("same-folder", "same-folder"),
            Some(MuseState::Idle)
        );
        for spinner in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
            assert_eq!(
                title_state(&format!("{spinner} same-folder"), "same-folder"),
                Some(MuseState::Working)
            );
        }
        assert_eq!(title_state("C:/muse-bin.exe", "same-folder"), None);
        assert_eq!(title_state("⠋ a different pane", "same-folder"), None);
        assert_eq!(title_state("same-folder", ""), None);
    }

    #[test]
    fn muse_permission_notification_is_the_blocked_signal() {
        let mut value = json!({"session_id":A,"cwd":"C:/project",
            "hook_event_name":"Notification","notification_type":"permission_prompt"});
        assert_eq!(
            metadata(serde_json::from_value(value.clone()).unwrap(), A)
                .unwrap()
                .state,
            MuseState::Blocked
        );
        value["notification_type"] = json!("unrelated");
        assert!(metadata(serde_json::from_value(value).unwrap(), A).is_none());
    }

    #[test]
    fn muse_native_selection_uses_only_the_recorded_version() {
        assert_eq!(
            super::native_binary_name("1.4.8-R6256.1").as_deref(),
            Some("muse-bin-1.4.8-R6256.1.exe")
        );
        assert_eq!(
            super::native_binary_name("1.4.8-R6256").as_deref(),
            Some("muse-bin-1.4.8-R6256.exe")
        );
        for invalid in [
            "../muse",
            "1.4.8-R6256.1.exe",
            "1.4.8-R6256.1/evil",
            "1.4-R6256.1",
            "",
        ] {
            assert!(super::native_binary_name(invalid).is_none());
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MuseState {
    #[default]
    Idle,
    Working,
    Blocked,
    Done,
}

/// Deserialize only metadata; serde skips prompts, tool inputs and replies.
#[derive(Deserialize)]
pub struct MuseEvent {
    session_id: String,
    cwd: PathBuf,
    hook_event_name: String,
    #[serde(default)]
    notification_type: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuseMetadata {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub cwd: PathBuf,
    #[serde(default)]
    pub state: MuseState,
}

pub fn metadata(event: MuseEvent, root_session_id: &str) -> Option<MuseMetadata> {
    Uuid::parse_str(root_session_id).ok()?;
    if event.session_id != root_session_id {
        return None;
    }
    let state = match event.hook_event_name.as_str() {
        "SessionStart" => MuseState::Idle,
        "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" => MuseState::Working,
        "Notification" if event.notification_type.as_deref() == Some("permission_prompt") => {
            MuseState::Blocked
        }
        "Stop" | "StopFailure" | "SessionEnd" => MuseState::Done,
        _ => return None,
    };
    Some(MuseMetadata {
        session_id: event.session_id,
        cwd: event.cwd,
        state,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookDocumentError {
    StrictSchema,
    Malformed,
}

/// Strict managed schemas validate the whole file and permit fewer events.
/// Decline instead of converting their handlers or appending ordinary ones.
pub fn merge_hook_document(mut document: Value, command: &str) -> Result<Value, HookDocumentError> {
    fn strict_marker(value: &Value) -> bool {
        match value {
            Value::Object(fields) => {
                fields.contains_key("name")
                    || fields.contains_key("timeout_ms")
                    || fields.values().any(strict_marker)
            }
            Value::Array(values) => values.iter().any(strict_marker),
            _ => false,
        }
    }
    if document.get("hooks").is_some_and(strict_marker) {
        return Err(HookDocumentError::StrictSchema);
    }
    let root = document
        .as_object_mut()
        .ok_or(HookDocumentError::Malformed)?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or(HookDocumentError::Malformed)?;
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PostToolUse",
        "PostToolUseFailure",
        "Notification",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ] {
        let mut group = json!({
            "hooks":[{"type":"command","command":command,"timeout":5}]
        });
        if event == "Notification" {
            group["matcher"] = json!("permission_prompt");
        }
        hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or(HookDocumentError::Malformed)?
            .push(group);
    }
    Ok(document)
}

/// Respect Muse's short/long config value syntax, including attached values.
pub fn has_hook_override(args: &[String]) -> bool {
    fn owns_hook_source(value: &str) -> bool {
        let key = value.split_once('=').map_or(value, |(key, _)| key).trim();
        key == "managed_hooks_path" || key == "hooks" || key.starts_with("hooks.")
    }
    let mut values = args.iter();
    while let Some(arg) = values.next() {
        if arg == "--" {
            break;
        }
        let value = if matches!(arg.as_str(), "-c" | "--config") {
            values.next().map(String::as_str)
        } else if let Some(value) = arg.strip_prefix("--config=") {
            Some(value)
        } else {
            arg.strip_prefix("-c")
                .map(|value| value.strip_prefix('=').unwrap_or(value))
        };
        if value.is_some_and(owns_hook_source) {
            return true;
        }
    }
    false
}

/// Keep generated options before the delimiter that starts positional input.
pub fn insert_hook_config(args: &mut Vec<String>, config: String) {
    let index = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    args.splice(index..index, ["--config".into(), config]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookLaunch {
    Start,
    Reattach(Uuid),
    Unmanaged,
}

/// A surviving tmux process keeps its old command/nonce; it cannot accept new hooks.
pub fn hook_launch(reattaching: bool, previous_nonce: Option<Uuid>) -> HookLaunch {
    if !reattaching {
        HookLaunch::Start
    } else {
        previous_nonce
            .filter(|nonce| !nonce.is_nil())
            .map_or(HookLaunch::Unmanaged, HookLaunch::Reattach)
    }
}

pub const HOOK_FLAG: &str = "--muse-session-hook";

/// Construct provider hook commands without filesystem or process side effects.
pub fn hook_command(
    executable: &str,
    data: &str,
    pane: Uuid,
    nonce: Uuid,
    windows: bool,
) -> String {
    if windows {
        let quote = crate::winshell::ps_quote;
        crate::winshell::script_command(&format!(
            "& {} {} {} {} {}; exit $LASTEXITCODE",
            quote(executable),
            quote(HOOK_FLAG),
            quote(&pane.to_string()),
            quote(data),
            quote(&nonce.to_string())
        ))
    } else {
        let quote = crate::ssh::sh_quote;
        format!(
            "{} {HOOK_FLAG} {pane} {} {nonce}",
            quote(executable),
            quote(data)
        )
    }
}

/// Find an exact UUID in a metadata-only fixture/listing, never by cwd or age.
pub fn session_log(session_id: &str, files: &[PathBuf]) -> Option<PathBuf> {
    Uuid::parse_str(session_id).ok()?;
    files.iter().find(|path| {
        let parts = path.components().map(|part| part.as_os_str().to_str()).collect::<Vec<_>>();
        matches!(parts.as_slice(), [Some(year), Some(month), Some(day), Some(id), Some("session.jsonl")]
            if year.len()==4 && month.len()==2 && day.len()==2
                && [*year,*month,*day].iter().all(|s|s.bytes().all(|b|b.is_ascii_digit()))
                && *id == session_id)
    }).cloned()
}

pub fn is_program(program: Option<&str>) -> bool {
    program
        .and_then(|program| program.rsplit(['/', '\\']).next())
        .is_some_and(|leaf| {
            matches!(
                leaf.to_ascii_lowercase().as_str(),
                "muse" | "muse.exe" | "muse.cmd" | "muse.bat"
            )
        })
}

/// Observed OSC0 contract: the cwd basename, optionally prefixed by a spinner.
pub fn title_state(title: &str, cwd_label: &str) -> Option<MuseState> {
    if cwd_label.is_empty() {
        return None;
    }
    if title == cwd_label {
        return Some(MuseState::Idle);
    }
    let mut chars = title.chars();
    let spinner = chars.next()?;
    (matches!(
        spinner,
        '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧' | '⠇' | '⠏'
    ) && chars.as_str().strip_prefix(' ') == Some(cwd_label))
    .then_some(MuseState::Working)
}

pub fn cwd_label(cwd: &Path) -> Option<&str> {
    cwd.to_str()?
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|label| !label.is_empty())
}

/// The installed Windows launcher's exact .muse-version selection grammar.
pub fn native_binary_name(version: &str) -> Option<String> {
    let (version_part, release) = version.split_once("-R")?;
    let numeric = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let parts = version_part.split('.').collect::<Vec<_>>();
    let release_parts = release.split('.').collect::<Vec<_>>();
    (parts.len() == 3
        && parts.iter().all(|part| numeric(part))
        && (1..=2).contains(&release_parts.len())
        && release_parts.iter().all(|part| numeric(part)))
    .then(|| format!("muse-bin-{version}.exe"))
}
