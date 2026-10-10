//! Watched-field comparison, reconciliation and fire / launch / confirm
//! decisions for shared loops and runners.
//!
//! Watched content is compared with the derived `==`: the parser already
//! stores sanitized values with defaults applied and `preset` as literal text.
//! Every lookup is by identity (library, kind, `name`), never by position.

use uuid::Uuid;

use super::resolve::resolve_preset;
use super::{
    LibItemKey, LibLoop, LibRunner, LoopContent, LoopOffReason, ParsedLibrary, PresetResolution,
    RunnerContent, SharedLoopState, SharedRunnerConfirmation,
};
use crate::{AgentPreset, SavedAgent, saved_agent};

// --- Lookups by identity ---

pub fn find_loop<'a>(parsed: &'a ParsedLibrary, name: &str) -> Option<&'a LibLoop> {
    parsed.loops.iter().find(|l| l.name == name)
}

pub fn find_runner<'a>(parsed: &'a ParsedLibrary, name: &str) -> Option<&'a LibRunner> {
    parsed.runners.iter().find(|r| r.name == name)
}

pub fn find_shared_loop<'a>(
    states: &'a [SharedLoopState],
    library: Uuid,
    name: &str,
) -> Option<&'a SharedLoopState> {
    states
        .iter()
        .find(|s| s.library == library && s.name == name)
}

pub fn find_confirmation<'a>(
    confirmations: &'a [SharedRunnerConfirmation],
    library: Uuid,
    name: &str,
) -> Option<&'a SharedRunnerConfirmation> {
    confirmations
        .iter()
        .find(|c| c.library == library && c.name == name)
}

/// A runner's `preset` → the agent to launch: `Ok(None)` = the toolbar's
/// preset, `Err(literal)` = unresolved.
fn preset_id_of(preset: Option<&str>, presets: &[AgentPreset]) -> Result<Option<Uuid>, String> {
    match resolve_preset(preset, presets) {
        PresetResolution::Unnamed => Ok(None),
        PresetResolution::Preset(id) => Ok(Some(id)),
        PresetResolution::NotFound(p) => Err(p),
    }
}

/// The agent a shared loop runs with: the preset its `preset` names or,
/// without one, the preset pinned when it was switched on. Never the
/// toolbar's preset or any other stand-in: an unresolved agent switches the
/// loop off.
pub fn loop_agent(
    preset: Option<&str>,
    pinned: Option<Uuid>,
    presets: &[AgentPreset],
) -> Result<Uuid, LoopOffReason> {
    match resolve_preset(preset, presets) {
        PresetResolution::Preset(id) => Ok(id),
        PresetResolution::NotFound(p) => Err(LoopOffReason::PresetNotFound(p)),
        PresetResolution::Unnamed => match saved_agent(pinned, presets) {
            SavedAgent::Preset(p) => Ok(p.id),
            SavedAgent::Missing => Err(LoopOffReason::PinnedPresetDeleted),
            SavedAgent::Current => Err(LoopOffReason::NoAgentPinned),
        },
    }
}

// --- Reconciliation after a successful read ---

/// Reconcile the switched-on loops of `library` with a successful read of its
/// file (never after a `FileError`). A loop no longer loaded loses its state
/// silently; one whose content changed loses it and its name is returned.
pub fn reconcile_loops(
    library: Uuid,
    parsed: &ParsedLibrary,
    states: &mut Vec<SharedLoopState>,
) -> Vec<String> {
    let mut switched_off = Vec::new();
    states.retain(|s| {
        if s.library != library {
            return true;
        }
        match find_loop(parsed, &s.name) {
            None => false,
            Some(l) if l.content != s.approved => {
                switched_off.push(s.name.clone());
                false
            }
            Some(_) => true,
        }
    });
    switched_off
}

// --- Fire decision ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FireMode {
    /// `now_tod` is seconds since local midnight.
    Scheduled { now: u64, now_tod: u32 },
    /// A click on the row: ignores the schedule.
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    Off,
    FileError,
    Gone,
    /// The loaded content differs from the approved one.
    Changed,
    NotDue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FireCheck {
    /// Run it, building the agent from the compared `content`.
    Fire {
        content: LoopContent,
        preset_id: Uuid,
    },
    /// Due (or clicked) with the approved content, but its agent does not
    /// resolve: switch it off with this reason and run nothing.
    SwitchOff(LoopOffReason),
    Skip(SkipReason),
}

/// Decide, at fire time, whether a shared loop runs. The agent is checked
/// last, so a loop is switched off for it only when it would otherwise run;
/// the caller still checks a run in flight and the project.
pub fn loop_fire_check(
    state: Option<&SharedLoopState>,
    loaded: Option<&LibLoop>,
    file_ok: bool,
    presets: &[AgentPreset],
    mode: FireMode,
) -> FireCheck {
    let Some(state) = state else {
        return FireCheck::Skip(SkipReason::Off);
    };
    if !file_ok {
        return FireCheck::Skip(SkipReason::FileError);
    }
    let Some(loaded) = loaded else {
        return FireCheck::Skip(SkipReason::Gone);
    };
    if loaded.content != state.approved {
        return FireCheck::Skip(SkipReason::Changed);
    }
    if let FireMode::Scheduled { now, now_tod } = mode
        && !loaded.content.schedule.is_due(state.last_run, now, now_tod)
    {
        return FireCheck::Skip(SkipReason::NotDue);
    }
    match loop_agent(
        loaded.content.preset.as_deref(),
        state.pinned_preset_id,
        presets,
    ) {
        Ok(preset_id) => FireCheck::Fire {
            content: loaded.content.clone(),
            preset_id,
        },
        Err(reason) => FireCheck::SwitchOff(reason),
    }
}

// --- Runner launch decision ---

#[derive(Clone, Debug, PartialEq)]
pub enum RunnerLaunch {
    Gone,
    Unavailable(String),
    /// Show the confirmation dialog with this loaded content.
    NeedsConfirm(RunnerContent),
    /// Go ahead, building the prompt from the compared `content`.
    Proceed {
        content: RunnerContent,
        preset_id: Option<Uuid>,
    },
}

/// Decide, at launch time, how a shared runner runs.
pub fn runner_launch(
    confirm: Option<&SharedRunnerConfirmation>,
    loaded: Option<&LibRunner>,
    presets: &[AgentPreset],
) -> RunnerLaunch {
    let Some(loaded) = loaded else {
        return RunnerLaunch::Gone;
    };
    let preset_id = match preset_id_of(loaded.content.preset.as_deref(), presets) {
        Ok(id) => id,
        Err(p) => return RunnerLaunch::Unavailable(p),
    };
    match confirm {
        Some(c) if c.confirmed == loaded.content => RunnerLaunch::Proceed {
            content: loaded.content.clone(),
            preset_id,
        },
        _ => RunnerLaunch::NeedsConfirm(loaded.content.clone()),
    }
}

// --- Confirm dialogs ---

/// What confirming a dialog that shows `shown` does.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfirmCheck<C> {
    Commit,
    /// The loaded content changed: show the dialog again with it.
    Reshow(C),
    Close,
}

pub fn confirm_check<C: PartialEq + Clone>(shown: &C, loaded: Option<&C>) -> ConfirmCheck<C> {
    match loaded {
        None => ConfirmCheck::Close,
        Some(l) if l != shown => ConfirmCheck::Reshow(l.clone()),
        Some(_) => ConfirmCheck::Commit,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnOnRequest {
    /// Open the turn-on dialog with this content and nothing picked.
    NeedsProjectAndConfirm(LoopContent),
    Gone,
    Unavailable(String),
}

/// Ask to switch a shared loop on: it always needs a project and a
/// confirmation of the loaded content, and an agent when it has no `preset`.
pub fn request_turn_on(loaded: Option<&LibLoop>, presets: &[AgentPreset]) -> TurnOnRequest {
    let Some(loaded) = loaded else {
        return TurnOnRequest::Gone;
    };
    match preset_id_of(loaded.content.preset.as_deref(), presets) {
        Err(p) => TurnOnRequest::Unavailable(p),
        Ok(_) => TurnOnRequest::NeedsProjectAndConfirm(loaded.content.clone()),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnOn {
    On,
    /// No `preset` and no agent picked.
    NeedsAgent,
    /// The content changed, or the picked agent was deleted: show the dialog
    /// again with this content and nothing picked.
    Reshow(LoopContent),
    Close,
    Unavailable(String),
}

/// Confirm the turn-on dialog of `key`, which showed `shown`. On success the
/// dialog's snapshot is approved and the loop is armed at `now`, like a new
/// local loop. Without `preset` the `agent` pick is required and pinned; with
/// one it is ignored (the name is resolved at fire time).
#[allow(clippy::too_many_arguments)]
pub fn turn_on(
    states: &mut Vec<SharedLoopState>,
    key: &LibItemKey,
    shown: &LoopContent,
    loaded: Option<&LibLoop>,
    project_id: Uuid,
    agent: Option<Uuid>,
    presets: &[AgentPreset],
    now: u64,
) -> TurnOn {
    match confirm_check(shown, loaded.map(|l| &l.content)) {
        ConfirmCheck::Close => return TurnOn::Close,
        ConfirmCheck::Reshow(c) => return TurnOn::Reshow(c),
        ConfirmCheck::Commit => {}
    }
    if let Err(p) = preset_id_of(shown.preset.as_deref(), presets) {
        return TurnOn::Unavailable(p);
    }
    let pinned_preset_id = if shown.preset.is_some() {
        None
    } else {
        let Some(id) = agent else {
            return TurnOn::NeedsAgent;
        };
        if !presets.iter().any(|p| p.id == id) {
            return TurnOn::Reshow(shown.clone());
        }
        Some(id)
    };
    turn_off(states, key.library, &key.name);
    states.push(SharedLoopState {
        library: key.library,
        name: key.name.clone(),
        run_id: Uuid::new_v4(),
        project_id,
        last_run: Some(now),
        approved: shown.clone(),
        pinned_preset_id,
    });
    TurnOn::On
}

/// Switch a shared loop off, forgetting its approved content and pin. Always
/// allowed. Returns whether it was on.
pub fn turn_off(states: &mut Vec<SharedLoopState>, library: Uuid, name: &str) -> bool {
    let before = states.len();
    states.retain(|s| !(s.library == library && s.name == name));
    states.len() != before
}

#[derive(Clone, Debug, PartialEq)]
pub enum RunnerConfirm {
    /// Confirmation recorded: open the normal "Run task" details dialog.
    OpenDetails,
    Reshow(RunnerContent),
    Close,
    Unavailable(String),
}

/// Confirm the confirmation dialog of runner `key`, which showed `shown`; the
/// snapshot replaces any earlier confirmation of that runner.
pub fn confirm_runner(
    confirmations: &mut Vec<SharedRunnerConfirmation>,
    key: &LibItemKey,
    shown: &RunnerContent,
    loaded: Option<&LibRunner>,
    presets: &[AgentPreset],
) -> RunnerConfirm {
    match confirm_check(shown, loaded.map(|r| &r.content)) {
        ConfirmCheck::Close => return RunnerConfirm::Close,
        ConfirmCheck::Reshow(c) => return RunnerConfirm::Reshow(c),
        ConfirmCheck::Commit => {}
    }
    if let Err(p) = preset_id_of(shown.preset.as_deref(), presets) {
        return RunnerConfirm::Unavailable(p);
    }
    confirmations.retain(|c| !(c.library == key.library && c.name == key.name));
    confirmations.push(SharedRunnerConfirmation {
        library: key.library,
        name: key.name.clone(),
        confirmed: shown.clone(),
    });
    RunnerConfirm::OpenDetails
}

// --- Loop row ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopToggle {
    TurnOn,
    TurnOff,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopRow {
    pub on: bool,
    pub toggle: LoopToggle,
    /// "Turn off" is always enabled; "Turn on…" only with a resolvable preset.
    pub toggle_enabled: bool,
    /// An on row is always clickable: the click is a run, which switches an
    /// unresolved loop off.
    pub clickable: bool,
    pub agent: Option<Uuid>,
    pub unavailable: Option<LoopOffReason>,
}

pub fn loop_row(
    state: Option<&SharedLoopState>,
    loaded: &LibLoop,
    presets: &[AgentPreset],
) -> LoopRow {
    let preset = loaded.content.preset.as_deref();
    let agent = match state {
        Some(s) => loop_agent(preset, s.pinned_preset_id, presets).map(Some),
        None => match resolve_preset(preset, presets) {
            PresetResolution::Preset(id) => Ok(Some(id)),
            PresetResolution::Unnamed => Ok(None),
            PresetResolution::NotFound(p) => Err(LoopOffReason::PresetNotFound(p)),
        },
    };
    let on = state.is_some();
    let unavailable = agent.as_ref().err().cloned();
    LoopRow {
        on,
        toggle: if on {
            LoopToggle::TurnOff
        } else {
            LoopToggle::TurnOn
        },
        toggle_enabled: on || unavailable.is_none(),
        clickable: on || unavailable.is_none(),
        agent: agent.ok().flatten(),
        unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfirmCheck, FireCheck, FireMode, LoopToggle, RunnerConfirm, RunnerLaunch, SkipReason,
        TurnOn, TurnOnRequest, confirm_check, confirm_runner, find_confirmation, find_loop,
        find_runner, find_shared_loop, loop_fire_check, loop_row, reconcile_loops, request_turn_on,
        runner_launch, turn_off, turn_on,
    };
    use crate::library::parse::parse_library;
    use crate::library::{
        LibItemKey, LibKind, LibLoop, LibRunner, LoopContent, LoopOffReason, ParsedLibrary,
        RunnerContent, SharedLoopState, SharedRunnerConfirmation,
    };
    use crate::{AgentPreset, LoopSchedule, PostRunAction, Runner, Settings};
    use uuid::Uuid;

    const LIB: Uuid = Uuid::from_u128(0xA1);
    const OTHER_LIB: Uuid = Uuid::from_u128(0xA2);
    const CLAUDE_ID: Uuid = Uuid::from_u128(0xC1);
    const CODEX_ID: Uuid = Uuid::from_u128(0xC2);
    const PROJECT: Uuid = Uuid::from_u128(0x50);
    const T0: u64 = 1_700_000_000;

    fn preset(name: &str, id: Uuid) -> AgentPreset {
        let mut p = AgentPreset::shell();
        p.name = name.to_string();
        p.id = id;
        p
    }

    fn claude() -> Vec<AgentPreset> {
        vec![preset("Claude", CLAUDE_ID)]
    }

    fn parse(text: &str) -> ParsedLibrary {
        parse_library(text).expect("library file parses")
    }

    fn loop_key(name: &str) -> LibItemKey {
        LibItemKey {
            library: LIB,
            kind: LibKind::Loop,
            name: name.to_string(),
        }
    }

    fn runner_key(name: &str) -> LibItemKey {
        LibItemKey {
            library: LIB,
            kind: LibKind::Runner,
            name: name.to_string(),
        }
    }

    fn loop_content(prompt: &str) -> LoopContent {
        LoopContent {
            prompt: prompt.to_string(),
            preset: Some("Claude".to_string()),
            auto_mode_presses: 0,
            schedule: LoopSchedule::EveryHours { hours: 1 },
            post_run: PostRunAction::Leave,
        }
    }

    fn lib_loop(name: &str, content: LoopContent) -> LibLoop {
        LibLoop {
            name: name.to_string(),
            content,
        }
    }

    fn runner_content(prompt: &str) -> RunnerContent {
        RunnerContent {
            prompt: prompt.to_string(),
            preset: Some("Claude".to_string()),
            auto_mode_presses: 3,
        }
    }

    fn lib_runner(name: &str, content: RunnerContent) -> LibRunner {
        LibRunner {
            name: name.to_string(),
            content,
        }
    }

    fn on_state(name: &str, approved: LoopContent, last_run: u64) -> SharedLoopState {
        SharedLoopState {
            library: LIB,
            name: name.to_string(),
            run_id: Uuid::from_u128(0x77),
            project_id: PROJECT,
            last_run: Some(last_run),
            approved,
            pinned_preset_id: None,
        }
    }

    fn confirmation(name: &str, confirmed: RunnerContent) -> SharedRunnerConfirmation {
        SharedRunnerConfirmation {
            library: LIB,
            name: name.to_string(),
            confirmed,
        }
    }

    fn scheduled(now: u64) -> FireMode {
        FireMode::Scheduled { now, now_tod: 0 }
    }

    const WATCHED_BASE: &str = r#"
[[loops]]
name = "L"
prompt = "go"
preset = "Claude"
auto_mode_presses = 1
schedule = { kind = "every_minutes", minutes = 5 }
post_run = "leave"
"#;

    /// Switch `L` on from `approved_file`, then reconcile with `new_file`.
    fn reload(approved_file: &str, new_file: &str) -> (Vec<String>, Vec<SharedLoopState>) {
        let approved = find_loop(&parse(approved_file), "L")
            .expect("L is loaded")
            .content
            .clone();
        let mut states = vec![on_state("L", approved, T0)];
        let new = parse(new_file);
        let off = reconcile_loops(LIB, &new, &mut states);
        (off, states)
    }

    fn assert_switched_off(new_file: &str) {
        assert_switched_off_from(WATCHED_BASE, new_file);
    }

    fn assert_switched_off_from(approved_file: &str, new_file: &str) {
        let (off, states) = reload(approved_file, new_file);
        assert_eq!(off, vec!["L".to_string()], "{new_file}");
        assert!(states.is_empty(), "{new_file}");
        let new = parse(new_file);
        assert_eq!(
            loop_fire_check(
                find_shared_loop(&states, LIB, "L"),
                find_loop(&new, "L"),
                true,
                &claude(),
                scheduled(T0 + 10_000_000),
            ),
            FireCheck::Skip(SkipReason::Off)
        );
    }

    fn assert_still_on(new_file: &str) {
        let (off, states) = reload(WATCHED_BASE, new_file);
        assert!(off.is_empty(), "{new_file}");
        assert_eq!(states.len(), 1, "{new_file}");
        assert_eq!(states[0].last_run, Some(T0), "{new_file}");
    }

    // --- Every watched loop field switches the loop off ---

    #[test]
    fn prompt_change_switches_off() {
        assert_switched_off(&WATCHED_BASE.replace("prompt = \"go\"", "prompt = \"stop\""));
    }

    #[test]
    fn preset_case_change_switches_off() {
        assert_switched_off(&WATCHED_BASE.replace("\"Claude\"", "\"claude\""));
    }

    #[test]
    fn preset_removed_switches_off() {
        assert_switched_off(&WATCHED_BASE.replace("preset = \"Claude\"\n", ""));
    }

    #[test]
    fn preset_added_switches_off() {
        let without = WATCHED_BASE.replace("preset = \"Claude\"\n", "");
        assert_switched_off_from(&without, WATCHED_BASE);
    }

    /// The added `preset` resolves to the same `C` as the pin; still off.
    #[test]
    fn preset_added_to_a_pinned_loop_switches_off_and_drops_the_pin() {
        let without = WATCHED_BASE.replace("preset = \"Claude\"\n", "");
        let approved = find_loop(&parse(&without), "L").unwrap().content.clone();
        let mut state = on_state("L", approved, T0);
        state.pinned_preset_id = Some(CLAUDE_ID);
        let mut states = vec![state];
        let new = parse(WATCHED_BASE);
        assert_eq!(
            reconcile_loops(LIB, &new, &mut states),
            vec!["L".to_string()]
        );
        assert!(states.is_empty());
        assert_eq!(
            loop_fire_check(
                find_shared_loop(&states, LIB, "L"),
                find_loop(&new, "L"),
                true,
                &claude(),
                scheduled(T0 + 10_000_000),
            ),
            FireCheck::Skip(SkipReason::Off)
        );
        let loaded = find_loop(&new, "L").unwrap();
        assert_eq!(
            request_turn_on(Some(loaded), &claude()),
            TurnOnRequest::NeedsProjectAndConfirm(loaded.content.clone())
        );
        assert_eq!(loaded.content.preset.as_deref(), Some("Claude"));
    }

    #[test]
    fn auto_mode_presses_change_switches_off() {
        assert_switched_off(
            &WATCHED_BASE.replace("auto_mode_presses = 1", "auto_mode_presses = 2"),
        );
    }

    #[test]
    fn schedule_change_switches_off() {
        assert_switched_off(&WATCHED_BASE.replace("minutes = 5", "minutes = 6"));
        assert_switched_off(&WATCHED_BASE.replace(
            "{ kind = \"every_minutes\", minutes = 5 }",
            "{ kind = \"every_hours\", hours = 5 }",
        ));
    }

    #[test]
    fn post_run_change_switches_off() {
        assert_switched_off(&WATCHED_BASE.replace("post_run = \"leave\"", "post_run = \"exit\""));
    }

    #[test]
    fn watched_compare_missing_post_run_equals_leave() {
        assert_still_on(&WATCHED_BASE.replace("post_run = \"leave\"\n", ""));
    }

    #[test]
    fn watched_compare_empty_preset_differs_from_absent() {
        let without = WATCHED_BASE.replace("preset = \"Claude\"\n", "");
        let empty = WATCHED_BASE.replace("preset = \"Claude\"", "preset = \"\"");
        assert_switched_off_from(&without, &empty);
    }

    #[test]
    fn switch_off_reports_the_loop_name_only_once() {
        let file = format!("{WATCHED_BASE}\n[[loops]]\nname = \"M\"\nprompt = \"m\"\n");
        let new = file.replace("prompt = \"go\"", "prompt = \"stop\"");
        let parsed_old = parse(&file);
        let mut states = vec![
            on_state(
                "L",
                find_loop(&parsed_old, "L").unwrap().content.clone(),
                T0,
            ),
            on_state(
                "M",
                find_loop(&parsed_old, "M").unwrap().content.clone(),
                T0,
            ),
        ];
        let off = reconcile_loops(LIB, &parse(&new), &mut states);
        assert_eq!(off, vec!["L".to_string()]);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].name, "M");
    }

    // --- Unrelated changes and ignored fields keep the loop on ---

    #[test]
    fn change_to_another_item_keeps_loop_on() {
        let file = format!(
            "{WATCHED_BASE}\n[[loops]]\nname = \"M\"\nprompt = \"m\"\n[[runners]]\nname = \"R\"\nprompt = \"r\"\n"
        );
        let (off, states) = reload(
            &file,
            &file
                .replace("prompt = \"m\"", "prompt = \"m2\"")
                .replace("prompt = \"r\"", "prompt = \"r2\""),
        );
        assert!(off.is_empty());
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].last_run, Some(T0));
    }

    #[test]
    fn ignored_fields_never_switch_off() {
        assert_still_on(&format!("{WATCHED_BASE}enabled = false\n"));
        assert_still_on(&format!(
            "{WATCHED_BASE}project_id = \"00000000-0000-0000-0000-000000000009\"\nlast_run = 5\nid = 3\npreset_id = [1]\n"
        ));
    }

    #[test]
    fn reconcile_leaves_other_libraries_alone() {
        let approved = loop_content("go");
        let mut other = on_state("L", approved.clone(), T0);
        other.library = OTHER_LIB;
        let mut states = vec![on_state("L", approved, T0), other.clone()];
        let off = reconcile_loops(LIB, &ParsedLibrary::default(), &mut states);
        assert!(off.is_empty());
        assert_eq!(states, vec![other]);
    }

    // --- A loaded loop is off whatever the file says ---

    #[test]
    fn enabled_in_file_is_still_off_and_never_fires() {
        let parsed = parse(
            r#"
[[loops]]
name = "L"
enabled = true
project_id = "00000000-0000-0000-0000-000000000050"
last_run = 0
schedule = { kind = "every_minutes", minutes = 1 }
"#,
        );
        let mut states: Vec<SharedLoopState> = Vec::new();
        assert!(reconcile_loops(LIB, &parsed, &mut states).is_empty());
        assert!(states.is_empty());
        let loaded = find_loop(&parsed, "L");
        assert!(loaded.is_some());
        assert_eq!(
            loop_fire_check(None, loaded, true, &claude(), scheduled(10_000_000)),
            FireCheck::Skip(SkipReason::Off)
        );
        assert!(!loop_row(None, loaded.unwrap(), &claude()).on);
    }

    #[test]
    fn freshly_loaded_loop_never_fires_over_ten_hourly_ticks() {
        let parsed = parse(
            "[[loops]]\nname = \"L\"\nschedule = { kind = \"every_minutes\", minutes = 1 }\n",
        );
        let loaded = find_loop(&parsed, "L");
        let fires = (1..=10)
            .filter(|i| {
                matches!(
                    loop_fire_check(None, loaded, true, &claude(), scheduled(T0 + i * 3600)),
                    FireCheck::Fire { .. }
                )
            })
            .count();
        assert_eq!(fires, 0);
    }

    // --- Switching on arms the loop like a new local one ---

    #[test]
    fn switched_on_at_t0_fires_after_one_full_interval_in_p() {
        let loaded = lib_loop("L", loop_content("go"));
        let mut states = Vec::new();
        let shown = loaded.content.clone();
        assert_eq!(
            turn_on(
                &mut states,
                &loop_key("L"),
                &shown,
                Some(&loaded),
                PROJECT,
                None,
                &claude(),
                T0
            ),
            TurnOn::On
        );
        let state = find_shared_loop(&states, LIB, "L").expect("switched on");
        assert_eq!(state.project_id, PROJECT);
        assert_eq!(state.last_run, Some(T0));
        assert_eq!(state.approved, shown);

        assert_eq!(
            loop_fire_check(
                Some(state),
                Some(&loaded),
                true,
                &claude(),
                scheduled(T0 + 3599)
            ),
            FireCheck::Skip(SkipReason::NotDue)
        );
        assert_eq!(
            loop_fire_check(
                Some(state),
                Some(&loaded),
                true,
                &claude(),
                scheduled(T0 + 3600)
            ),
            FireCheck::Fire {
                content: loop_content("go"),
                preset_id: CLAUDE_ID,
            }
        );
    }

    #[test]
    fn daily_at_already_passed_waits_for_next_day() {
        let mut content = loop_content("go");
        content.schedule = LoopSchedule::DailyAt { hour: 9, minute: 0 };
        let loaded = lib_loop("L", content.clone());
        let tod = 10 * 3600;
        let mut states = Vec::new();
        turn_on(
            &mut states,
            &loop_key("L"),
            &content,
            Some(&loaded),
            PROJECT,
            None,
            &claude(),
            T0,
        );
        let state = find_shared_loop(&states, LIB, "L");
        let at = |now: u64, now_tod: u32| {
            loop_fire_check(
                state,
                Some(&loaded),
                true,
                &claude(),
                FireMode::Scheduled { now, now_tod },
            )
        };
        assert_eq!(at(T0 + 60, tod + 60), FireCheck::Skip(SkipReason::NotDue));
        assert!(matches!(
            at(T0 + 23 * 3600, 9 * 3600),
            FireCheck::Fire { .. }
        ));
    }

    // --- Turning off ---

    #[test]
    fn turned_off_never_fires_and_turning_on_needs_project_and_confirm() {
        let loaded = lib_loop("L", loop_content("go"));
        let l = T0;
        let mut states = vec![on_state("L", loaded.content.clone(), l)];
        assert!(matches!(
            loop_fire_check(
                states.first(),
                Some(&loaded),
                true,
                &claude(),
                scheduled(l + 3600)
            ),
            FireCheck::Fire { .. }
        ));

        assert!(turn_off(&mut states, LIB, "L"));
        assert!(states.is_empty());
        for now in [l + 3600, l + 36_000] {
            assert_eq!(
                loop_fire_check(
                    find_shared_loop(&states, LIB, "L"),
                    Some(&loaded),
                    true,
                    &claude(),
                    scheduled(now)
                ),
                FireCheck::Skip(SkipReason::Off)
            );
        }
        assert_eq!(
            request_turn_on(Some(&loaded), &claude()),
            TurnOnRequest::NeedsProjectAndConfirm(loop_content("go"))
        );
        let row = loop_row(None, &loaded, &claude());
        assert_eq!(row.toggle, LoopToggle::TurnOn);
        assert!(row.toggle_enabled);
        assert!(!turn_off(&mut states, LIB, "L"));
    }

    #[test]
    fn renamed_local_preset_keeps_it_on_until_due_and_turn_off_works() {
        let loaded = lib_loop("L", every_minute(loop_content("go")));
        let mut states = vec![on_state("L", loaded.content.clone(), T0)];
        let renamed = vec![preset("Claude2", CLAUDE_ID)];

        assert_eq!(
            loop_fire_check(
                states.first(),
                Some(&loaded),
                true,
                &renamed,
                scheduled(T0 + 30)
            ),
            FireCheck::Skip(SkipReason::NotDue)
        );
        let row = loop_row(states.first(), &loaded, &renamed);
        assert!(row.on);
        assert_eq!(row.toggle, LoopToggle::TurnOff);
        assert!(row.toggle_enabled);
        assert!(row.clickable);
        assert_eq!(
            row.unavailable,
            Some(LoopOffReason::PresetNotFound("Claude".to_string()))
        );
        assert_eq!(
            loop_row(states.first(), &loaded, &claude()).unavailable,
            None
        );

        assert!(turn_off(&mut states, LIB, "L"));
        assert!(states.is_empty());
    }

    fn every_minute(mut content: LoopContent) -> LoopContent {
        content.schedule = LoopSchedule::EveryMinutes { minutes: 1 };
        content
    }

    fn claude_codex() -> Vec<AgentPreset> {
        vec![preset("Claude", CLAUDE_ID), preset("Codex", CODEX_ID)]
    }

    fn no_preset(prompt: &str) -> LoopContent {
        LoopContent {
            preset: None,
            ..every_minute(loop_content(prompt))
        }
    }

    // --- The agent pick of a loop without `preset` ---

    #[test]
    fn turn_on_without_preset_needs_a_pick_and_pins_it() {
        let loaded = lib_loop("L", no_preset("go"));
        let presets = claude_codex();
        assert_eq!(
            request_turn_on(Some(&loaded), &presets),
            TurnOnRequest::NeedsProjectAndConfirm(no_preset("go"))
        );
        let mut states = Vec::new();
        assert_eq!(
            turn_on(
                &mut states,
                &loop_key("L"),
                &loaded.content,
                Some(&loaded),
                PROJECT,
                None,
                &presets,
                T0
            ),
            TurnOn::NeedsAgent
        );
        assert!(states.is_empty());
        assert_eq!(
            turn_on(
                &mut states,
                &loop_key("L"),
                &loaded.content,
                Some(&loaded),
                PROJECT,
                Some(CLAUDE_ID),
                &presets,
                T0
            ),
            TurnOn::On
        );
        let state = find_shared_loop(&states, LIB, "L").expect("on");
        assert_eq!(state.project_id, PROJECT);
        assert_eq!(state.pinned_preset_id, Some(CLAUDE_ID));
        assert_eq!(state.approved, no_preset("go"));
    }

    #[test]
    fn loop_with_preset_pins_nothing_and_needs_no_pick() {
        let mut content = loop_content("go");
        content.preset = Some("claude".to_string());
        let loaded = lib_loop("M", content.clone());
        let mut states = Vec::new();
        assert_eq!(
            turn_on(
                &mut states,
                &loop_key("M"),
                &content,
                Some(&loaded),
                PROJECT,
                None,
                &claude_codex(),
                T0
            ),
            TurnOn::On
        );
        assert_eq!(states[0].pinned_preset_id, None);
        turn_off(&mut states, LIB, "M");
        turn_on(
            &mut states,
            &loop_key("M"),
            &content,
            Some(&loaded),
            PROJECT,
            Some(CODEX_ID),
            &claude_codex(),
            T0,
        );
        assert_eq!(states[0].pinned_preset_id, None);
    }

    #[test]
    fn deleted_pick_reshows_and_switches_nothing_on() {
        let loaded = lib_loop("L", no_preset("go"));
        let without_codex = vec![preset("Claude", CLAUDE_ID)];
        let mut states = Vec::new();
        assert_eq!(
            turn_on(
                &mut states,
                &loop_key("L"),
                &loaded.content,
                Some(&loaded),
                PROJECT,
                Some(CODEX_ID),
                &without_codex,
                T0
            ),
            TurnOn::Reshow(no_preset("go"))
        );
        assert!(states.is_empty());
    }

    // --- A pinned preset that was deleted, or no pin at all ---

    #[test]
    fn deleted_pin_shows_the_reason_then_switches_off_when_due() {
        let loaded = lib_loop("L", no_preset("go"));
        let mut state = on_state("L", no_preset("go"), T0);
        state.pinned_preset_id = Some(CLAUDE_ID);
        let states = [state];
        let codex_only = vec![preset("Codex", CODEX_ID)];

        let row = loop_row(states.first(), &loaded, &codex_only);
        assert!(row.on && row.toggle_enabled && row.clickable);
        assert_eq!(row.unavailable, Some(LoopOffReason::PinnedPresetDeleted));
        let check = |mode| loop_fire_check(states.first(), Some(&loaded), true, &codex_only, mode);
        assert_eq!(
            check(scheduled(T0 + 30)),
            FireCheck::Skip(SkipReason::NotDue)
        );
        assert_eq!(
            check(scheduled(T0 + 60)),
            FireCheck::SwitchOff(LoopOffReason::PinnedPresetDeleted)
        );
        assert_eq!(
            check(FireMode::Manual),
            FireCheck::SwitchOff(LoopOffReason::PinnedPresetDeleted)
        );
        assert_eq!(
            loop_fire_check(
                states.first(),
                Some(&loaded),
                true,
                &claude_codex(),
                scheduled(T0 + 60)
            ),
            FireCheck::Fire {
                content: no_preset("go"),
                preset_id: CLAUDE_ID
            }
        );
        assert_eq!(
            loop_row(states.first(), &loaded, &claude_codex()).agent,
            Some(CLAUDE_ID)
        );
    }

    #[test]
    fn no_pin_shows_the_reason_and_switches_off_when_due() {
        let loaded = lib_loop("L", no_preset("go"));
        let states = [on_state("L", no_preset("go"), T0)];
        let row = loop_row(states.first(), &loaded, &claude_codex());
        assert!(row.on && row.toggle_enabled && row.clickable);
        assert_eq!(row.unavailable, Some(LoopOffReason::NoAgentPinned));
        assert_eq!(
            loop_fire_check(
                states.first(),
                Some(&loaded),
                true,
                &claude_codex(),
                scheduled(T0 + 10_000)
            ),
            FireCheck::SwitchOff(LoopOffReason::NoAgentPinned)
        );
    }

    // --- The file's preset no longer resolves ---

    #[test]
    fn renamed_preset_switches_off_when_due_or_clicked() {
        let loaded = lib_loop("N", every_minute(loop_content("go")));
        let states = [on_state("N", loaded.content.clone(), T0)];
        let renamed = vec![preset("Claude2", CLAUDE_ID)];
        let check = |mode| loop_fire_check(states.first(), Some(&loaded), true, &renamed, mode);
        let off = FireCheck::SwitchOff(LoopOffReason::PresetNotFound("Claude".to_string()));
        assert_eq!(check(scheduled(T0 + 60)), off);
        assert_eq!(check(FireMode::Manual), off);
        let row = loop_row(None, &loaded, &renamed);
        assert!(!row.on && !row.toggle_enabled && !row.clickable);
        assert_eq!(
            row.unavailable,
            Some(LoopOffReason::PresetNotFound("Claude".to_string()))
        );
        assert_eq!(
            request_turn_on(Some(&loaded), &renamed),
            TurnOnRequest::Unavailable("Claude".to_string())
        );
    }

    #[test]
    fn unresolved_agent_is_reported_only_when_it_would_run() {
        let loaded = lib_loop("L", no_preset("go"));
        let states = [on_state("L", no_preset("go"), T0)];
        assert_eq!(
            loop_fire_check(None, Some(&loaded), true, &[], FireMode::Manual),
            FireCheck::Skip(SkipReason::Off)
        );
        assert_eq!(
            loop_fire_check(states.first(), None, false, &[], FireMode::Manual),
            FireCheck::Skip(SkipReason::FileError)
        );
        let changed = lib_loop("L", no_preset("other"));
        assert_eq!(
            loop_fire_check(states.first(), Some(&changed), true, &[], FireMode::Manual),
            FireCheck::Skip(SkipReason::Changed)
        );
        assert_eq!(
            loop_fire_check(states.first(), Some(&loaded), true, &[], scheduled(T0 + 59)),
            FireCheck::Skip(SkipReason::NotDue)
        );
    }

    // --- The agent comes from the file or the pin, never elsewhere ---

    #[test]
    fn agent_is_the_named_or_pinned_preset_and_never_another() {
        let shell = Uuid::from_u128(0x5);
        let presets = vec![
            preset("Claude", CLAUDE_ID),
            preset("Codex", CODEX_ID),
            preset("Shell", shell),
        ];
        let pinned = |pin: Option<Uuid>| {
            let mut s = on_state("L", no_preset("go"), T0);
            s.pinned_preset_id = pin;
            s
        };
        let named = |name: &str| {
            let mut c = every_minute(loop_content("go"));
            c.preset = Some(name.to_string());
            c
        };
        let cases = [
            (named("Claude"), on_state("L", named("Claude"), T0)),
            (no_preset("go"), pinned(Some(CLAUDE_ID))),
            (no_preset("go"), pinned(Some(Uuid::from_u128(0xDEAD)))),
            (no_preset("go"), pinned(None)),
            (named("NoSuch"), on_state("L", named("NoSuch"), T0)),
        ];
        let got: Vec<FireCheck> = cases
            .iter()
            .map(|(content, state)| {
                let loaded = lib_loop("L", content.clone());
                loop_fire_check(Some(state), Some(&loaded), true, &presets, FireMode::Manual)
            })
            .collect();
        assert_eq!(
            got,
            [
                FireCheck::Fire {
                    content: named("Claude"),
                    preset_id: CLAUDE_ID
                },
                FireCheck::Fire {
                    content: no_preset("go"),
                    preset_id: CLAUDE_ID
                },
                FireCheck::SwitchOff(LoopOffReason::PinnedPresetDeleted),
                FireCheck::SwitchOff(LoopOffReason::NoAgentPinned),
                FireCheck::SwitchOff(LoopOffReason::PresetNotFound("NoSuch".to_string())),
            ]
        );
    }

    #[test]
    fn unresolved_preset_disables_turn_on() {
        let loaded = lib_loop("L", loop_content("go"));
        let none: Vec<AgentPreset> = Vec::new();
        let row = loop_row(None, &loaded, &none);
        assert_eq!(row.toggle, LoopToggle::TurnOn);
        assert!(!row.toggle_enabled);
        assert!(!row.clickable);
        assert_eq!(
            row.unavailable,
            Some(LoopOffReason::PresetNotFound("Claude".to_string()))
        );
        assert_eq!(
            request_turn_on(Some(&loaded), &none),
            TurnOnRequest::Unavailable("Claude".to_string())
        );
        let mut states = Vec::new();
        assert_eq!(
            turn_on(
                &mut states,
                &loop_key("L"),
                &loaded.content,
                Some(&loaded),
                PROJECT,
                None,
                &none,
                T0
            ),
            TurnOn::Unavailable("Claude".to_string())
        );
        assert!(states.is_empty());
    }

    // --- The check at fire time ---

    #[test]
    fn file_error_blocks_fire_but_keeps_state() {
        let loaded = lib_loop("L", loop_content("go"));
        let states = [on_state("L", loaded.content.clone(), T0)];
        assert_eq!(
            loop_fire_check(
                states.first(),
                None,
                false,
                &claude(),
                scheduled(T0 + 99_999)
            ),
            FireCheck::Skip(SkipReason::FileError)
        );
        assert_eq!(
            loop_fire_check(states.first(), None, false, &claude(), FireMode::Manual),
            FireCheck::Skip(SkipReason::FileError)
        );
    }

    #[test]
    fn loop_not_loaded_is_not_fired() {
        let states = [on_state("L", loop_content("go"), T0)];
        assert_eq!(
            loop_fire_check(states.first(), None, true, &claude(), FireMode::Manual),
            FireCheck::Skip(SkipReason::Gone)
        );
    }

    #[test]
    fn changed_content_is_not_run_by_schedule_or_click() {
        let states = [on_state("L", loop_content("old"), 0)];
        let loaded = lib_loop("L", loop_content("new"));
        for mode in [scheduled(10_000_000), FireMode::Manual] {
            assert_eq!(
                loop_fire_check(states.first(), Some(&loaded), true, &claude(), mode),
                FireCheck::Skip(SkipReason::Changed)
            );
        }
        let same = lib_loop("L", loop_content("old"));
        for mode in [scheduled(10_000_000), FireMode::Manual] {
            assert!(matches!(
                loop_fire_check(states.first(), Some(&same), true, &claude(), mode),
                FireCheck::Fire { .. }
            ));
        }
    }

    #[test]
    fn startup_reload_switches_off_and_never_fires() {
        let mut states = vec![on_state("L", loop_content("old"), 0)];
        let parsed = ParsedLibrary {
            loops: vec![lib_loop("L", loop_content("new"))],
            ..Default::default()
        };
        assert_eq!(
            reconcile_loops(LIB, &parsed, &mut states),
            vec!["L".to_string()]
        );
        let fires = (1..=10)
            .filter(|i| {
                matches!(
                    loop_fire_check(
                        find_shared_loop(&states, LIB, "L"),
                        find_loop(&parsed, "L"),
                        true,
                        &claude(),
                        scheduled(10_000_000 + i * 3600),
                    ),
                    FireCheck::Fire { .. }
                )
            })
            .count();
        assert_eq!(fires, 0);
        assert!(states.is_empty());
    }

    #[test]
    fn fire_is_found_by_identity_and_built_from_compared_content() {
        // A new "K" is inserted before "L".
        let parsed = ParsedLibrary {
            loops: vec![
                lib_loop("K", loop_content("other")),
                lib_loop("L", loop_content("go")),
            ],
            ..Default::default()
        };
        let states = [on_state("L", loop_content("go"), T0)];
        assert_eq!(
            loop_fire_check(
                find_shared_loop(&states, LIB, "L"),
                find_loop(&parsed, "L"),
                true,
                &claude(),
                FireMode::Manual
            ),
            FireCheck::Fire {
                content: loop_content("go"),
                preset_id: CLAUDE_ID
            }
        );
    }

    // --- Sanitized content compares equal ---

    #[test]
    fn control_char_added_by_pull_keeps_loop_on_and_runner_confirmed() {
        let file = |prompt: &str| {
            format!(
                "[[runners]]\nname = \"R\"\nprompt = \"{prompt}\"\n[[loops]]\nname = \"L\"\nprompt = \"{prompt}\"\n"
            )
        };
        let before = parse(&file("go"));
        let mut states = vec![on_state(
            "L",
            find_loop(&before, "L").unwrap().content.clone(),
            T0,
        )];
        let confirmations = vec![confirmation(
            "R",
            find_runner(&before, "R").unwrap().content.clone(),
        )];

        let after = parse(&file("g\\u0007o"));
        assert!(reconcile_loops(LIB, &after, &mut states).is_empty());
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].last_run, Some(T0));

        let launch = runner_launch(
            find_confirmation(&confirmations, LIB, "R"),
            find_runner(&after, "R"),
            &claude(),
        );
        match launch {
            RunnerLaunch::Proceed { content, .. } => assert_eq!(content.prompt, "go"),
            other => panic!("expected Proceed, got {other:?}"),
        }
        match request_turn_on(find_loop(&after, "L"), &claude()) {
            TurnOnRequest::NeedsProjectAndConfirm(c) => assert_eq!(c.prompt, "go"),
            other => panic!("expected the dialog, got {other:?}"),
        }
        assert_eq!(find_runner(&after, "R").unwrap().content.prompt, "go");
        assert_eq!(find_loop(&after, "L").unwrap().content.prompt, "go");
    }

    #[test]
    fn zero_width_space_added_to_preset_keeps_loop_on() {
        let file = |preset: &str| {
            format!("[[loops]]\nname = \"L\"\npreset = \"{preset}\"\nprompt = \"go\"\n")
        };
        let before = parse(&file("Claude"));
        let mut states = vec![on_state(
            "L",
            find_loop(&before, "L").unwrap().content.clone(),
            T0,
        )];
        let after = parse(&file("Claude\\u200B"));
        assert!(reconcile_loops(LIB, &after, &mut states).is_empty());
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].last_run, Some(T0));
    }

    // --- A discarded item counts as absent ---

    #[test]
    fn invalid_schedule_drops_state_without_event_and_returns_off() {
        let approved = find_loop(&parse(WATCHED_BASE), "L")
            .unwrap()
            .content
            .clone();
        let mut states = vec![on_state("L", approved, T0)];

        let invalid = parse(&WATCHED_BASE.replace("minutes = 5", "minutes = 0"));
        assert!(find_loop(&invalid, "L").is_none());
        let off = reconcile_loops(LIB, &invalid, &mut states);
        assert!(
            off.is_empty(),
            "no \"switched off\" event for a vanished loop"
        );
        assert!(states.is_empty());

        let restored = parse(WATCHED_BASE);
        assert!(reconcile_loops(LIB, &restored, &mut states).is_empty());
        let loaded = find_loop(&restored, "L").expect("L is back");
        assert!(find_shared_loop(&states, LIB, "L").is_none());
        assert!(!loop_row(None, loaded, &claude()).on);
        assert_eq!(
            loop_fire_check(None, Some(loaded), true, &claude(), scheduled(T0 + 99_999)),
            FireCheck::Skip(SkipReason::Off)
        );
    }

    #[test]
    fn duplicate_inserted_before_loads_the_new_one_off() {
        let approved = find_loop(&parse(WATCHED_BASE), "L")
            .unwrap()
            .content
            .clone();
        let mut states = vec![on_state("L", approved, T0)];
        let file = format!("[[loops]]\nname = \"L\"\nprompt = \"inserted\"\n{WATCHED_BASE}");
        let parsed = parse(&file);
        assert_eq!(find_loop(&parsed, "L").unwrap().content.prompt, "inserted");
        reconcile_loops(LIB, &parsed, &mut states);
        assert!(states.is_empty());
    }

    // --- Confirm re-checks the loaded content ---

    #[test]
    fn loop_changed_while_dialog_open_reshows_with_new_content() {
        let shown = loop_content("A");
        let loaded = lib_loop("L", loop_content("B"));
        let mut states = Vec::new();
        assert_eq!(
            turn_on(
                &mut states,
                &loop_key("L"),
                &shown,
                Some(&loaded),
                PROJECT,
                None,
                &claude(),
                T0
            ),
            TurnOn::Reshow(loop_content("B"))
        );
        assert!(states.is_empty());
    }

    #[test]
    fn loop_deleted_while_dialog_open_closes() {
        let mut states = Vec::new();
        assert_eq!(
            turn_on(
                &mut states,
                &loop_key("L"),
                &loop_content("A"),
                None,
                PROJECT,
                None,
                &claude(),
                T0
            ),
            TurnOn::Close
        );
        assert!(states.is_empty());
    }

    #[test]
    fn loop_unchanged_is_switched_on_with_shown_content() {
        let loaded = lib_loop("L", loop_content("A"));
        let mut states = Vec::new();
        assert_eq!(
            turn_on(
                &mut states,
                &loop_key("L"),
                &loop_content("A"),
                Some(&loaded),
                PROJECT,
                None,
                &claude(),
                T0
            ),
            TurnOn::On
        );
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].approved.prompt, "A");
        assert_eq!(states[0].library, LIB);
        assert_eq!(states[0].name, "L");
        assert_eq!(states[0].approved.preset, Some("Claude".to_string()));
    }

    #[test]
    fn runner_changed_while_dialog_open_reshows_and_still_needs_confirm() {
        let loaded = lib_runner("X", runner_content("B"));
        let mut confirmations = Vec::new();
        assert_eq!(
            confirm_runner(
                &mut confirmations,
                &runner_key("X"),
                &runner_content("A"),
                Some(&loaded),
                &claude()
            ),
            RunnerConfirm::Reshow(runner_content("B"))
        );
        assert!(confirmations.is_empty());
        assert_eq!(
            runner_launch(
                find_confirmation(&confirmations, LIB, "X"),
                Some(&loaded),
                &claude()
            ),
            RunnerLaunch::NeedsConfirm(runner_content("B"))
        );
    }

    #[test]
    fn confirm_check_decides_commit_reshow_close() {
        let a = runner_content("A");
        assert_eq!(confirm_check(&a, Some(&a)), ConfirmCheck::Commit);
        assert_eq!(
            confirm_check(&a, Some(&runner_content("B"))),
            ConfirmCheck::Reshow(runner_content("B"))
        );
        assert_eq!(
            confirm_check::<RunnerContent>(&a, None),
            ConfirmCheck::Close
        );
    }

    // --- Runners ---

    #[test]
    fn unconfirmed_runner_needs_confirmation_then_opens_details() {
        let loaded = lib_runner("X", runner_content("A"));
        let mut confirmations: Vec<SharedRunnerConfirmation> = Vec::new();
        assert_eq!(
            runner_launch(None, Some(&loaded), &claude()),
            RunnerLaunch::NeedsConfirm(runner_content("A"))
        );
        assert_eq!(
            runner_launch(
                find_confirmation(&confirmations, LIB, "X"),
                Some(&loaded),
                &claude()
            ),
            RunnerLaunch::NeedsConfirm(runner_content("A"))
        );
        assert_eq!(
            confirm_runner(
                &mut confirmations,
                &runner_key("X"),
                &runner_content("A"),
                Some(&loaded),
                &claude()
            ),
            RunnerConfirm::OpenDetails
        );
        assert_eq!(confirmations, vec![confirmation("X", runner_content("A"))]);
        assert_eq!(
            runner_launch(
                find_confirmation(&confirmations, LIB, "X"),
                Some(&loaded),
                &claude()
            ),
            RunnerLaunch::Proceed {
                content: runner_content("A"),
                preset_id: Some(CLAUDE_ID)
            }
        );
    }

    #[test]
    fn reconfirming_replaces_the_previous_confirmation() {
        let mut confirmations = vec![confirmation("X", runner_content("old"))];
        let loaded = lib_runner("X", runner_content("new"));
        assert_eq!(
            confirm_runner(
                &mut confirmations,
                &runner_key("X"),
                &runner_content("new"),
                Some(&loaded),
                &claude()
            ),
            RunnerConfirm::OpenDetails
        );
        assert_eq!(
            confirmations,
            vec![confirmation("X", runner_content("new"))]
        );
    }

    #[test]
    fn confirmed_runner_proceeds_also_after_reload() {
        let loaded = lib_runner("X", runner_content("A"));
        let settings = Settings {
            shared_runner_confirmations: vec![confirmation("X", runner_content("A"))],
            ..Default::default()
        };
        let text = toml::to_string(&settings).expect("settings serialize");
        let reloaded: Settings = toml::from_str(&text).expect("settings reload");
        for confirmations in [
            &settings.shared_runner_confirmations,
            &reloaded.shared_runner_confirmations,
        ] {
            assert_eq!(
                runner_launch(
                    find_confirmation(confirmations, LIB, "X"),
                    Some(&loaded),
                    &claude()
                ),
                RunnerLaunch::Proceed {
                    content: runner_content("A"),
                    preset_id: Some(CLAUDE_ID)
                }
            );
        }
    }

    #[test]
    fn each_watched_runner_field_requires_confirmation_again() {
        let confirmed = runner_content("A");
        let confirmations = [confirmation("X", confirmed.clone())];
        let mut changes = Vec::new();
        changes.push(RunnerContent {
            prompt: "B".to_string(),
            ..confirmed.clone()
        });
        for preset in [Some("claude"), None, Some("")] {
            changes.push(RunnerContent {
                preset: preset.map(str::to_string),
                ..confirmed.clone()
            });
        }
        changes.push(RunnerContent {
            auto_mode_presses: 4,
            ..confirmed.clone()
        });
        let presets = vec![preset("Claude", CLAUDE_ID), preset("", Uuid::from_u128(9))];
        for changed in changes {
            let loaded = lib_runner("X", changed.clone());
            assert_eq!(
                runner_launch(confirmations.first(), Some(&loaded), &presets),
                // `Some("")` never resolves and is reported first.
                if changed.preset.as_deref() == Some("") {
                    RunnerLaunch::Unavailable(String::new())
                } else {
                    RunnerLaunch::NeedsConfirm(changed.clone())
                },
                "{changed:?}"
            );
        }
        let same = lib_runner("X", confirmed.clone());
        assert!(matches!(
            runner_launch(confirmations.first(), Some(&same), &presets),
            RunnerLaunch::Proceed { .. }
        ));
    }

    #[test]
    fn absent_to_present_preset_requires_confirmation() {
        let confirmed = RunnerContent {
            preset: None,
            ..runner_content("A")
        };
        let confirmations = [confirmation("X", confirmed)];
        let loaded = lib_runner("X", runner_content("A"));
        assert_eq!(
            runner_launch(confirmations.first(), Some(&loaded), &claude()),
            RunnerLaunch::NeedsConfirm(runner_content("A"))
        );
    }

    #[test]
    fn launch_time_check_needs_confirmation_for_new_content() {
        let confirmations = [confirmation("X", runner_content("old"))];
        let loaded = lib_runner("X", runner_content("new"));
        assert_eq!(
            runner_launch(confirmations.first(), Some(&loaded), &claude()),
            RunnerLaunch::NeedsConfirm(runner_content("new"))
        );
    }

    #[test]
    fn changed_before_run_needs_confirmation_and_keeps_old_confirmation() {
        let confirmations = vec![confirmation("X", runner_content("old"))];
        let parsed = ParsedLibrary {
            runners: vec![lib_runner("X", runner_content("new"))],
            ..Default::default()
        };
        assert_eq!(
            runner_launch(
                find_confirmation(&confirmations, LIB, "X"),
                find_runner(&parsed, "X"),
                &claude()
            ),
            RunnerLaunch::NeedsConfirm(runner_content("new"))
        );
        assert_eq!(confirmations[0].confirmed.prompt, "old");
    }

    #[test]
    fn runner_inserted_before_is_ignored_by_identity() {
        let confirmations = vec![confirmation("X", runner_content("x prompt"))];
        let parsed = ParsedLibrary {
            runners: vec![
                lib_runner("W", runner_content("w prompt")),
                lib_runner("X", runner_content("x prompt")),
            ],
            ..Default::default()
        };
        let RunnerLaunch::Proceed { content, preset_id } = runner_launch(
            find_confirmation(&confirmations, LIB, "X"),
            find_runner(&parsed, "X"),
            &claude(),
        ) else {
            panic!("expected Proceed");
        };
        assert_eq!(content.prompt, "x prompt");
        let runner = Runner {
            id: Uuid::from_u128(0x99),
            name: "X".to_string(),
            preset_id,
            auto_mode_presses: content.auto_mode_presses,
            prompt: content.prompt.clone(),
        };
        let instance = runner.build_instance(PROJECT, &claude()[0], "");
        assert_eq!(instance.custom_name, Some("X".to_string()));
        assert_eq!(instance.system_prompt, Some("x prompt".to_string()));
    }

    #[test]
    fn runner_deleted_before_run_is_gone() {
        let confirmations = vec![confirmation("X", runner_content("old"))];
        let parsed = ParsedLibrary::default();
        assert_eq!(
            runner_launch(
                find_confirmation(&confirmations, LIB, "X"),
                find_runner(&parsed, "X"),
                &claude()
            ),
            RunnerLaunch::Gone
        );
    }

    #[test]
    fn unresolved_runner_preset_is_unavailable_even_when_confirmed() {
        let confirmations = [confirmation("X", runner_content("A"))];
        let loaded = lib_runner("X", runner_content("A"));
        let renamed = vec![preset("Claude2", CLAUDE_ID)];
        assert_eq!(
            runner_launch(confirmations.first(), Some(&loaded), &renamed),
            RunnerLaunch::Unavailable("Claude".to_string())
        );
        let mut fresh = Vec::new();
        assert_eq!(
            confirm_runner(
                &mut fresh,
                &runner_key("X"),
                &runner_content("A"),
                Some(&loaded),
                &renamed
            ),
            RunnerConfirm::Unavailable("Claude".to_string())
        );
        assert!(fresh.is_empty());
    }

    #[test]
    fn confirm_runner_closes_when_runner_is_gone() {
        let mut confirmations = Vec::new();
        assert_eq!(
            confirm_runner(
                &mut confirmations,
                &runner_key("X"),
                &runner_content("A"),
                None,
                &claude()
            ),
            RunnerConfirm::Close
        );
        assert!(confirmations.is_empty());
    }

    #[test]
    fn absent_runner_preset_proceeds_with_current_agent() {
        let content = RunnerContent {
            preset: None,
            ..runner_content("A")
        };
        let confirmations = [confirmation("X", content.clone())];
        let loaded = lib_runner("X", content.clone());
        assert_eq!(
            runner_launch(confirmations.first(), Some(&loaded), &[]),
            RunnerLaunch::Proceed {
                content,
                preset_id: None
            }
        );
    }

    #[test]
    fn lookups_match_library_and_name_exactly() {
        let mut other = on_state("L", loop_content("go"), T0);
        other.library = OTHER_LIB;
        let states = [other];
        assert!(find_shared_loop(&states, LIB, "L").is_none());
        assert!(find_shared_loop(&states, OTHER_LIB, "L").is_some());
        assert!(find_shared_loop(&states, OTHER_LIB, "l").is_none());
        let confirmations = [SharedRunnerConfirmation {
            library: OTHER_LIB,
            ..confirmation("X", runner_content("A"))
        }];
        assert!(find_confirmation(&confirmations, LIB, "X").is_none());
        assert!(find_confirmation(&confirmations, OTHER_LIB, "X").is_some());
    }
}
