//! Preset resolution, local copies and snippet send action.

use uuid::Uuid;

use super::{LibLoop, LibRunner, LibSnippet, PresetResolution};
use crate::{AgentPreset, Loop, Runner, Snippet};

/// Resolve a library item's literal `preset` against the local presets: the
/// first whose name matches ignoring case and surrounding whitespace. No match
/// (even for an empty `preset`) is `NotFound`, never a fallback to a default.
pub fn resolve_preset(preset: Option<&str>, presets: &[AgentPreset]) -> PresetResolution {
    let Some(wanted) = preset else {
        return PresetResolution::Unnamed;
    };
    let key = wanted.trim().to_lowercase();
    if !key.is_empty()
        && let Some(found) = presets.iter().find(|p| p.name.trim().to_lowercase() == key)
    {
        return PresetResolution::Preset(found.id);
    }
    PresetResolution::NotFound(wanted.to_string())
}

/// Why "Make a local copy" could not create the copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyError {
    /// The item is no longer loaded from its library.
    Gone,
    /// A loop copy needs an active project.
    NoProject,
    PresetNotFound(String),
}

fn copy_preset_id(
    preset: Option<&str>,
    presets: &[AgentPreset],
) -> Result<Option<Uuid>, CopyError> {
    match resolve_preset(preset, presets) {
        PresetResolution::Unnamed => Ok(None),
        PresetResolution::Preset(id) => Ok(Some(id)),
        PresetResolution::NotFound(p) => Err(CopyError::PresetNotFound(p)),
    }
}

/// A private copy of a library snippet: new id, same (sanitized) fields.
pub fn local_snippet_copy(item: &LibSnippet) -> Snippet {
    Snippet {
        id: Uuid::new_v4(),
        name: item.name.clone(),
        text: item.text.clone(),
        submit: item.submit,
    }
}

/// A private copy of a library runner, with `preset` resolved to `preset_id`.
pub fn local_runner_copy(item: &LibRunner, presets: &[AgentPreset]) -> Result<Runner, CopyError> {
    let preset_id = copy_preset_id(item.content.preset.as_deref(), presets)?;
    Ok(Runner {
        id: Uuid::new_v4(),
        name: item.name.clone(),
        preset_id,
        auto_mode_presses: item.content.auto_mode_presses,
        prompt: item.content.prompt.clone(),
    })
}

/// A private copy of a library loop: switched off, in the active project and
/// armed at `now`, as `add_loop` does. Its agent is the one `preset` names or,
/// without one, `toolbar_preset`. The preset is checked before the project.
pub fn local_loop_copy(
    item: &LibLoop,
    presets: &[AgentPreset],
    toolbar_preset: Option<Uuid>,
    active_project: Option<Uuid>,
    now: u64,
) -> Result<Loop, CopyError> {
    let preset_id = copy_preset_id(item.content.preset.as_deref(), presets)?.or(toolbar_preset);
    let project_id = active_project.ok_or(CopyError::NoProject)?;
    Ok(Loop {
        id: Uuid::new_v4(),
        name: item.name.clone(),
        preset_id,
        project_id,
        auto_mode_presses: item.content.auto_mode_presses,
        prompt: item.content.prompt.clone(),
        schedule: item.content.schedule,
        post_run: item.content.post_run,
        enabled: false,
        last_run: Some(now),
    })
}

/// One step of sending a snippet to a terminal pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnippetStep {
    /// `TerminalSession::paste` of this text.
    Paste(String),
    /// `TerminalSession::write_input` of these bytes.
    Write(&'static [u8]),
}

/// What sending a snippet does, private or library: paste `text`, then Enter
/// if `submit`. No preview or confirmation.
pub fn snippet_send_action(text: &str, submit: bool) -> Vec<SnippetStep> {
    let mut steps = vec![SnippetStep::Paste(text.to_string())];
    if submit {
        steps.push(SnippetStep::Write(b"\r"));
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::{
        CopyError, SnippetStep, local_loop_copy, local_runner_copy, local_snippet_copy,
        resolve_preset, snippet_send_action,
    };
    use crate::library::{
        LibLoop, LibRunner, LibSnippet, LoopContent, PresetResolution, RunnerContent,
    };
    use crate::{AgentPreset, LoopSchedule, PostRunAction, Snippet};
    use uuid::Uuid;

    fn preset(name: &str, id: u128) -> AgentPreset {
        let mut p = AgentPreset::shell();
        p.name = name.to_string();
        p.id = Uuid::from_u128(id);
        p
    }

    fn sample_presets() -> Vec<AgentPreset> {
        vec![
            preset("Claude", 0xC1),
            preset("Codex", 0xC2),
            preset("codex", 0xC3),
        ]
    }

    #[test]
    fn codex_resolves_to_first_match() {
        assert_eq!(
            resolve_preset(Some("Codex"), &sample_presets()),
            PresetResolution::Preset(Uuid::from_u128(0xC2))
        );
    }

    #[test]
    fn case_and_surrounding_spaces_are_ignored() {
        assert_eq!(
            resolve_preset(Some("  CLAUDE "), &sample_presets()),
            PresetResolution::Preset(Uuid::from_u128(0xC1))
        );
        // Spaces on the local preset's side are ignored too.
        let presets = vec![preset("  Claude  ", 0xD1)];
        assert_eq!(
            resolve_preset(Some("claude"), &presets),
            PresetResolution::Preset(Uuid::from_u128(0xD1))
        );
    }

    #[test]
    fn lowercase_codex_also_takes_the_first_match() {
        // Both `Codex` and `codex` match; the first wins.
        assert_eq!(
            resolve_preset(Some("codex"), &sample_presets()),
            PresetResolution::Preset(Uuid::from_u128(0xC2))
        );
    }

    #[test]
    fn absent_preset_is_unnamed() {
        assert_eq!(
            resolve_preset(None, &sample_presets()),
            PresetResolution::Unnamed
        );
    }

    #[test]
    fn unknown_preset_is_not_found_with_literal_name() {
        assert_eq!(
            resolve_preset(Some("NoSuchAgent"), &sample_presets()),
            PresetResolution::NotFound("NoSuchAgent".to_string())
        );
        assert_eq!(
            resolve_preset(Some("NoSuchAgent"), &[]),
            PresetResolution::NotFound("NoSuchAgent".to_string())
        );
    }

    #[test]
    fn empty_preset_is_not_found() {
        // A `preset` emptied by sanitizing is still present.
        assert_eq!(
            resolve_preset(Some(""), &sample_presets()),
            PresetResolution::NotFound(String::new())
        );
        let presets = vec![preset("   ", 0xE1)];
        assert_eq!(
            resolve_preset(Some(""), &presets),
            PresetResolution::NotFound(String::new())
        );
    }

    #[test]
    fn exact_text_is_required_apart_from_case_and_edges() {
        // An inner zero-width space does not match.
        assert_eq!(
            resolve_preset(Some("Cla\u{200B}ude"), &sample_presets()),
            PresetResolution::NotFound("Cla\u{200B}ude".to_string())
        );
    }

    fn review_runner(preset: Option<&str>) -> LibRunner {
        LibRunner {
            name: "Review".to_string(),
            content: RunnerContent {
                prompt: "P {{input}}".to_string(),
                preset: preset.map(str::to_string),
                auto_mode_presses: 3,
            },
        }
    }

    #[test]
    fn runner_copy_resolves_preset_and_keeps_fields() {
        let presets = vec![preset("Claude", 0x58)];
        let copy = local_runner_copy(&review_runner(Some("Claude")), &presets).expect("copy");
        assert_eq!(copy.name, "Review");
        assert_eq!(copy.preset_id, Some(Uuid::from_u128(0x58)));
        assert_eq!(copy.auto_mode_presses, 3);
        assert_eq!(copy.prompt, "P {{input}}");
        assert_ne!(copy.id, Uuid::from_u128(0x58));
        assert!(!copy.id.is_nil());
    }

    #[test]
    fn runner_copy_gets_a_fresh_id_each_time() {
        let presets = vec![preset("Claude", 0x58)];
        let a = local_runner_copy(&review_runner(Some("Claude")), &presets).expect("a");
        let b = local_runner_copy(&review_runner(Some("Claude")), &presets).expect("b");
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn runner_copy_without_preset_is_current() {
        let presets = vec![preset("Claude", 0x58)];
        let copy = local_runner_copy(&review_runner(None), &presets).expect("copy");
        assert_eq!(copy.preset_id, None);
        assert_eq!(copy.name, "Review");
    }

    #[test]
    fn runner_copy_with_unresolved_preset_fails() {
        let presets = vec![preset("Claude", 0x58)];
        assert_eq!(
            local_runner_copy(&review_runner(Some("NoSuchAgent")), &presets).map(|r| r.id),
            Err(CopyError::PresetNotFound("NoSuchAgent".to_string()))
        );
    }

    fn nightly_loop(preset: Option<&str>) -> LibLoop {
        LibLoop {
            name: "Nightly".to_string(),
            content: LoopContent {
                prompt: "check {{input}}".to_string(),
                preset: preset.map(str::to_string),
                auto_mode_presses: 2,
                schedule: LoopSchedule::DailyAt {
                    hour: 3,
                    minute: 15,
                },
                post_run: PostRunAction::Exit,
            },
        }
    }

    #[test]
    fn loop_copy_is_off_in_active_project_armed_now() {
        let presets = vec![preset("Claude", 0x58), preset("Codex", 0x59)];
        let codex = Some(Uuid::from_u128(0x59));
        let p = Uuid::from_u128(0x50);
        let t = 1_700_000_123;
        let copy = local_loop_copy(&nightly_loop(Some("Claude")), &presets, codex, Some(p), t)
            .expect("copy");
        assert!(!copy.enabled);
        assert_eq!(copy.project_id, p);
        assert_eq!(copy.last_run, Some(t));
        assert_eq!(copy.name, "Nightly");
        assert_eq!(copy.preset_id, Some(Uuid::from_u128(0x58)));
        assert_eq!(copy.prompt, "check {{input}}");
        assert_eq!(copy.auto_mode_presses, 2);
        assert_eq!(
            copy.schedule,
            LoopSchedule::DailyAt {
                hour: 3,
                minute: 15
            }
        );
        assert_eq!(copy.post_run, PostRunAction::Exit);
        assert!(!copy.id.is_nil());
    }

    #[test]
    fn loop_copy_without_preset_takes_the_toolbar_preset() {
        let presets = vec![preset("Claude", 0x58), preset("Codex", 0x59)];
        let codex = Uuid::from_u128(0x59);
        let p = Some(Uuid::from_u128(0x50));
        let copy = local_loop_copy(&nightly_loop(None), &presets, Some(codex), p, 7).expect("copy");
        assert_eq!(copy.preset_id, Some(codex));
        let copy = local_loop_copy(&nightly_loop(None), &presets, None, p, 7).expect("copy");
        assert_eq!(copy.preset_id, None);
    }

    #[test]
    fn loop_copy_without_active_project_fails() {
        let presets = vec![preset("Claude", 0x58)];
        assert_eq!(
            local_loop_copy(&nightly_loop(Some("Claude")), &presets, None, None, 7).map(|l| l.id),
            Err(CopyError::NoProject)
        );
    }

    #[test]
    fn loop_copy_with_unresolved_preset_fails() {
        let presets = vec![preset("Claude", 0x58)];
        assert_eq!(
            local_loop_copy(
                &nightly_loop(Some("NoSuchAgent")),
                &presets,
                Some(Uuid::from_u128(0x58)),
                Some(Uuid::from_u128(0x50)),
                7
            )
            .map(|l| l.id),
            Err(CopyError::PresetNotFound("NoSuchAgent".to_string()))
        );
    }

    #[test]
    fn snippet_copy_keeps_name_text_and_submit() {
        let lib = LibSnippet {
            name: "Go".to_string(),
            text: "go on".to_string(),
            submit: true,
        };
        let a = local_snippet_copy(&lib);
        let b = local_snippet_copy(&lib);
        assert_eq!(a.name, "Go");
        assert_eq!(a.text, "go on");
        assert!(a.submit);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn library_snippet_sends_like_a_private_one() {
        let lib = LibSnippet {
            name: "Go".to_string(),
            text: "go on".to_string(),
            submit: true,
        };
        let private = Snippet {
            id: Uuid::from_u128(9),
            name: "Mine".to_string(),
            text: "go on".to_string(),
            submit: true,
        };
        let from_lib = snippet_send_action(&lib.text, lib.submit);
        let from_private = snippet_send_action(&private.text, private.submit);
        assert_eq!(from_lib, from_private);
        assert_eq!(
            from_lib,
            vec![
                SnippetStep::Paste("go on".to_string()),
                SnippetStep::Write(b"\r"),
            ]
        );
    }

    #[test]
    fn unsubmitted_snippet_only_pastes() {
        assert_eq!(
            snippet_send_action("a\nb", false),
            vec![SnippetStep::Paste("a\nb".to_string())]
        );
    }
}
