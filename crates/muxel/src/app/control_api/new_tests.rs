//! Compile the production mutation boundary with inert UI/launch adapters.
//! Domain selection, validation and policy are real; the only replacements are
//! window identity and the final methods that would focus, persist and spawn.

use super::super::{ConfirmAction, PlacementMode, WorktreeChoice};
use super::{ControlTurn, CtlResult, control_launch_spec, control_program_writable};
use muxel_core::control::{self as ctl, CreationPolicy, ErrorCode};
use muxel_core::readaloud;
use muxel_core::{AgentPreset, Instance, PaneNode, Project, Settings, SplitDirection, Workspace};
use muxel_terminal::CommandSpec;
use serde_json::{Value, json};
use std::marker::PhantomData;
use std::path::Path;
use uuid::Uuid;

#[path = "new.rs"]
#[allow(
    clippy::duplicate_mod,
    reason = "exercise the production method with inert UI and spawn adapters"
)]
mod production;

#[derive(Clone, Copy)]
struct WindowHandle(u8);
impl WindowHandle {
    fn window_id(self) -> u8 {
        self.0
    }
}

#[derive(Default)]
struct Window {
    activations: usize,
}
impl Window {
    fn window_handle(&self) -> WindowHandle {
        WindowHandle(1)
    }
    fn activate_window(&mut self) {
        self.activations += 1;
    }
}

struct Context<T>(PhantomData<T>);
struct SecondaryWindow {
    pid: Uuid,
    handle: WindowHandle,
    active: bool,
}
struct PendingConfirm {
    action: ConfirmAction,
}

struct MuxelApp {
    settings: Settings,
    workspace: Workspace,
    presets: Vec<AgentPreset>,
    current_preset: usize,
    current_workspace: Option<Uuid>,
    workspace_busy: Option<Uuid>,
    confirm_quit: bool,
    show_quit_confirm: bool,
    confirm: Option<PendingConfirm>,
    secondary_windows: Vec<SecondaryWindow>,
    main_window: Option<WindowHandle>,
    active_instance: Option<Uuid>,
    selections: usize,
    placements: usize,
    persists: usize,
    launches: usize,
    control_turns: std::collections::HashMap<Uuid, ControlTurn>,
    reject_placement: bool,
}

impl MuxelApp {
    fn fixture() -> Self {
        let mut project = Project::new("repo", std::env::current_dir().unwrap());
        let pid = project.id;
        let existing = Instance::shell(pid);
        let active_instance = Some(existing.id);
        project.layout = Some(PaneNode::leaf(existing.id));
        Self {
            settings: Settings {
                control_enabled: true,
                control_allow_new: CreationPolicy::Allow,
                ..Settings::default()
            },
            workspace: Workspace {
                projects: vec![project],
                instances: vec![existing],
                active_project: Some(pid),
                ..Workspace::default()
            },
            presets: vec![AgentPreset::shell(), AgentPreset::codex()],
            current_preset: 0,
            current_workspace: Some(Uuid::new_v4()),
            workspace_busy: None,
            confirm_quit: false,
            show_quit_confirm: false,
            confirm: None,
            secondary_windows: vec![],
            main_window: Some(WindowHandle(1)),
            active_instance,
            selections: 0,
            placements: 0,
            persists: 0,
            launches: 0,
            control_turns: Default::default(),
            reject_placement: false,
        }
    }

    fn select_project(&mut self, pid: Uuid, _: &mut Window, _: &mut Context<Self>) {
        self.selections += 1;
        self.workspace.active_project = Some(pid);
    }

    #[allow(clippy::too_many_arguments)]
    fn place_and_spawn(
        &mut self,
        pid: Uuid,
        instance: Instance,
        placement: PlacementMode,
        target: Option<Uuid>,
        worktree: Option<WorktreeChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place_and_spawn_with_focus(
            pid, instance, placement, target, worktree, true, window, cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn place_and_spawn_with_focus(
        &mut self,
        pid: Uuid,
        instance: Instance,
        placement: PlacementMode,
        target: Option<Uuid>,
        worktree: Option<WorktreeChoice>,
        focus: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        self.placements += 1;
        if self.reject_placement {
            return;
        }
        if instance.control_prompt {
            assert!(
                self.control_turns.contains_key(&instance.id),
                "publish pending before placement"
            );
        }
        assert!(matches!(worktree, Some(WorktreeChoice::None)));
        let direction = match placement {
            PlacementMode::Tab => None,
            PlacementMode::Split(direction) => Some(direction),
        };
        assert!(ctl::place_new_pane(
            &mut self.workspace.project_mut(pid).unwrap().layout,
            target,
            instance.id,
            direction,
            !focus,
        ));
        self.workspace.instances.push(instance);
        self.persists += 1;
        self.launches += 1;
    }

    fn open(&mut self, options: ctl::NewOptions, window: &mut Window) -> CtlResult<Value> {
        self.control_new(
            self.workspace.projects[0].id,
            options,
            true,
            window,
            &mut Context(PhantomData),
        )
    }

    fn snapshot(&self, window: &Window) -> Value {
        json!({
            "workspace": self.workspace, "settings": self.settings,
            "active_instance": self.active_instance, "preset": self.current_preset,
            "selections": self.selections, "placements": self.placements,
            "persists": self.persists, "launches": self.launches,
            "activations": window.activations,
            "control_turns": self.control_turns.len(),
        })
    }

    fn refuses_without_mutation(&mut self, options: ctl::NewOptions, message: &str) {
        let mut window = Window::default();
        let before = self.snapshot(&window);
        let result = self.open(options, &mut window);
        assert!(
            self.snapshot(&window) == before,
            "refusal mutated state: {result:?}"
        );
        let error = result.expect_err("request should be refused");
        assert!(
            error.message.contains(message),
            "unexpected refusal: {error}"
        );
    }
}

fn argv(program: &str) -> ctl::NewOptions {
    ctl::NewOptions {
        argv: Some(vec![
            program.into(),
            "caller-selected command".into(),
            "".into(),
        ]),
        ..Default::default()
    }
}

#[test]
fn control_new_parameters_validate_before_mutation_and_preserve_allow_policy() {
    let mut app = MuxelApp::fixture();
    app.settings.control_allow_new = CreationPolicy::Allow;
    app.presets = vec![AgentPreset::claude()];
    app.current_preset = 0;
    app.refuses_without_mutation(
        ctl::NewOptions {
            model: Some("--bad".into()),
            ..Default::default()
        },
        "--model needs",
    );
    app.presets[0].effort_flag = None;
    app.refuses_without_mutation(
        ctl::NewOptions {
            effort: Some("high".into()),
            ..Default::default()
        },
        "Effort flag",
    );
    app.presets[0].effort_flag = Some("--effort".into());
    let options = ctl::NewOptions {
        model: Some("sonnet".into()),
        effort: Some("high".into()),
        ..Default::default()
    };
    let instance = app
        .prepare_control_new(app.workspace.projects[0].id, &options)
        .unwrap();
    assert_eq!(
        app.creation_decision(&instance),
        ctl::CreationDecision::Allow
    );
    let result = app.open(options, &mut Window::default()).unwrap();
    assert_eq!(
        result["args"],
        json!(["--model", "sonnet", "--effort", "high"])
    );
    assert_eq!(app.launches, 1);
}

const COMMAND_SETTING: &str = "Also allow typing into shell panes";

#[test]
fn control_new_startup_wait_is_pending_before_placement_until_actual_submission() {
    let mut app = MuxelApp::fixture();
    let result = app
        .open(
            ctl::NewOptions {
                preset: Some("Codex".into()),
                prompt: Some("task".into()),
                ..Default::default()
            },
            &mut Window::default(),
        )
        .unwrap();
    let iid: Uuid = serde_json::from_value(result["uuid"].clone()).unwrap();
    let instance = app.workspace.instance(iid).unwrap();
    assert!(instance.control_created);
    let turn = app.control_turns.get(&iid).unwrap();
    let activity = muxel_core::AgentActivity::default();
    assert!(turn.awaiting_reply(Some(ctl::AgentState::Idle), &activity, 90_000));
    let owner = turn.startup.as_ref().unwrap().claim().unwrap();
    let spec = control_launch_spec(
        Some(instance),
        Some(turn),
        Some(owner),
        CommandSpec::program("codex", vec![]).with_startup_input("task"),
    );
    assert!(!spec.allow_shell_fallback);
    spec.startup_turn.as_ref().unwrap().submitted(90_001);
    assert!(turn.awaiting_reply(Some(ctl::AgentState::Idle), &activity, 90_002));
    let completed = muxel_core::AgentActivity {
        completed_at: Some(90_003),
        ..Default::default()
    };
    assert!(!turn.awaiting_reply(Some(ctl::AgentState::Done), &completed, 90_004));
}

#[test]
fn control_new_failed_placement_rolls_back_pending_and_plain_creation_has_no_task() {
    let mut app = MuxelApp::fixture();
    app.reject_placement = true;
    assert!(
        app.open(
            ctl::NewOptions {
                preset: Some("Codex".into()),
                prompt: Some("task".into()),
                ..Default::default()
            },
            &mut Window::default()
        )
        .is_err()
    );
    assert!(app.control_turns.is_empty());
    app.reject_placement = false;
    let reply = app
        .open(
            ctl::NewOptions {
                preset: Some("Codex".into()),
                ..Default::default()
            },
            &mut Window::default(),
        )
        .unwrap();
    let iid: Uuid = serde_json::from_value(reply["uuid"].clone()).unwrap();
    assert!(app.workspace.instance(iid).unwrap().control_created);
    assert!(app.control_turns.is_empty());
}

#[test]
fn control_new_startup_lost_owner_suppresses_input_and_controller_drop_cancels_live_owner() {
    let mut instance = Instance::from_preset(Uuid::new_v4(), &AgentPreset::codex());
    instance.control_prompt = true;
    instance.system_prompt = Some("task".into());
    let turn = ControlTurn::startup("task".into());
    let signal = turn.startup.as_ref().unwrap().clone();
    let owner = signal.claim().unwrap();
    let ordinary = CommandSpec::program("codex", vec![])
        .with_startup_input("task")
        .with_auto_mode(2);
    assert!(
        control_launch_spec(Some(&instance), None, None, ordinary.clone()).allow_shell_fallback
    );
    instance.control_created = true;
    let restored = control_launch_spec(Some(&instance), None, None, ordinary.clone());
    assert!(
        restored.startup_input.is_none(),
        "cancelled startup must not replay after losing runtime state"
    );
    assert!(!restored.submit);
    let cancelled = control_launch_spec(Some(&instance), Some(&turn), None, ordinary);
    assert!(cancelled.startup_input.is_none());
    assert_eq!(cancelled.auto_mode_presses, 0);
    assert!(!cancelled.submit);
    assert!(!owner.is_pending());
    // Consumed tasks must not suppress a later fresh conversation's automatic
    // instructions merely because their old control-turn record still exists.
    instance.system_prompt = None;
    let ordinary = CommandSpec::program("codex", vec![]).with_startup_input("new rules");
    let spec = control_launch_spec(Some(&instance), Some(&turn), None, ordinary);
    assert_eq!(spec.startup_input.as_deref(), Some("new rules"));
    let next = ControlTurn::startup("next".into());
    let next_owner = next.startup.as_ref().unwrap().claim().unwrap();
    drop(next);
    assert!(!next_owner.is_pending());
}

const CREATION_SETTING: &str = "Outside terminal creation";

#[test]
fn control_new_policy_creation_opt_in_denies_all_launch_classes() {
    for current_preset in [0, 1] {
        for options in [
            ctl::NewOptions::default(),
            ctl::NewOptions {
                preset: Some("Codex".into()),
                ..Default::default()
            },
            ctl::NewOptions {
                preset: Some(AgentPreset::shell().name),
                ..Default::default()
            },
            argv(std::env::current_exe().unwrap().to_str().unwrap()),
        ] {
            for background in [false, true] {
                let mut app = MuxelApp::fixture();
                app.current_preset = current_preset;
                app.settings.control_allow_new = CreationPolicy::Off;
                app.settings.control_allow_shells = true;
                let mut options = options.clone();
                options.background = background;
                app.refuses_without_mutation(options, CREATION_SETTING);
            }
        }
    }
    let mut app = MuxelApp::fixture();
    app.settings.control_allow_new = CreationPolicy::Off;
    app.settings.control_allow_shells = true;
    app.presets.clear();
    app.refuses_without_mutation(ctl::NewOptions::default(), CREATION_SETTING);
}

#[test]
fn control_new_policy_creation_opt_in_is_checked_at_admission() {
    let source = include_str!("../control_api.rs");
    let admission = source
        .split("    fn control_command(")
        .nth(1)
        .unwrap()
        .split("    fn control_new_destination(")
        .next()
        .unwrap();
    let gate = admission
        .find("self.settings.control_allow_new == ctl::CreationPolicy::Off")
        .expect("admission must check creation opt-in");
    assert!(gate < admission.find("self.current_workspace.is_none()").unwrap());
    assert!(gate < admission.find("self.control_turns.retain(").unwrap());
    let compact = admission.split_whitespace().collect::<String>();
    assert!(
        compact.contains("matches!(&command,Command::New{..})&&self.settings.control_allow_new==ctl::CreationPolicy::Off")
    );
}

#[test]
fn control_new_policy_raw_write_revocation() {
    let mut app = MuxelApp::fixture();
    let project = &app.workspace.projects[0];
    for program in [
        "codex",
        "claude",
        "python",
        "wsl",
        "renamed-shell",
        "unknown-program",
    ] {
        let instance = argv(program).instance(project, &[], 0).unwrap();
        let before = serde_json::to_value(&instance).unwrap();
        app.settings.control_allow_shells = true;
        assert!(control_program_writable(&instance, app.settings.control_allow_shells).is_ok());
        app.settings.control_allow_shells = false;
        let error = control_program_writable(&instance, app.settings.control_allow_shells)
            .expect_err("raw-command write permission revoked");
        assert!(
            error.contains(COMMAND_SETTING),
            "unexpected refusal: {error}"
        );
        assert!(
            !error.contains("is a shell"),
            "raw program mislabeled: {error}"
        );
        assert_eq!(serde_json::to_value(&instance).unwrap(), before);
    }
}

#[test]
fn control_new_policy_configured_agent_writes_survive_shell_opt_out() {
    let mut app = MuxelApp::fixture();
    app.settings.control_allow_new = CreationPolicy::Off;
    assert!(app.settings.control_enabled);
    assert!(!app.settings.control_allow_shells);
    let instance = Instance::from_preset(app.workspace.projects[0].id, &AgentPreset::codex());
    assert!(!instance.literal_argv);
    assert!(control_program_writable(&instance, app.settings.control_allow_shells).is_ok());
    app.settings.control_allow_shells = true;
    for instance in [
        Instance::shell(app.workspace.projects[0].id),
        argv("codex")
            .instance(&app.workspace.projects[0], &[], 0)
            .unwrap(),
    ] {
        assert!(control_program_writable(&instance, app.settings.control_allow_shells).is_ok());
    }
}

#[test]
fn control_new_policy_write_entry_points_recheck_permission() {
    let source = include_str!("../control_api.rs");
    let write_boundary = source
        .split("    fn control_writable(")
        .nth(1)
        .unwrap()
        .split("    fn control_claim_site(")
        .next()
        .unwrap();
    assert!(
        write_boundary
            .find("control_program_writable(inst, self.settings.control_allow_shells)?")
            .unwrap()
            < write_boundary.find("self.control_state(").unwrap()
    );
    for (method, next) in [
        ("control_send", "control_answer"),
        ("control_answer", "control_keys"),
        ("control_keys", "control_press"),
    ] {
        let body = source
            .split(&format!("    fn {method}("))
            .nth(1)
            .unwrap()
            .split(&format!("    fn {next}("))
            .next()
            .unwrap();
        assert!(
            body.contains("self.control_writable(iid, cx)?"),
            "missing gate in {method}"
        );
    }
    let press = source
        .split("    fn control_press(")
        .nth(1)
        .unwrap()
        .split("\nfn typed(")
        .next()
        .unwrap();
    assert!(press.contains("self.control_writable(typing.iid, cx)"));
    // Retaining a session does not retain permission. Both timer callbacks must
    // recheck current app state without redirecting input to a restarted session.
    let callbacks: Vec<_> = press.split("cx.spawn(async move").skip(1).collect();
    assert_eq!(callbacks.len(), 2);
    for (index, callback) in callbacks.into_iter().enumerate() {
        let callback = callback.split_whitespace().collect::<String>();
        let timer = callback.find(".await;").unwrap();
        let update = callback
            .find("this.update(cx,")
            .expect("timer must re-enter current app state");
        let gate = callback
            .find("app.control_writable(iid,cx)")
            .expect("timer must recheck current permission");
        let (same_session, write) = if index == 0 {
            (
                callback.find("enter_verdict(live,&session,").unwrap(),
                callback.find("press_prompt_enter(&session,").unwrap(),
            )
        } else {
            (
                callback
                    .find("Arc::ptr_eq(&session,&current_session)")
                    .unwrap(),
                callback.find("session.write_input(").unwrap(),
            )
        };
        assert!(timer < update && update < gate && gate < same_session && same_session < write);
    }
}

#[test]
fn control_new_policy_shell_classification_refuses_uppercase_presets() {
    for program in [
        r"C:\Windows\System32\powershell.EXE",
        "CmD.ExE",
        "/bin/PwSh.eXe",
    ] {
        let mut app = MuxelApp::fixture();
        app.presets[0].program = Some(program.into());
        app.refuses_without_mutation(ctl::NewOptions::default(), COMMAND_SETTING);
    }
}

#[test]
fn control_new_policy_shell_classification_refuses_writes_after_opt_out() {
    for program in [
        r"C:\Windows\System32\powershell.EXE",
        "CmD.ExE",
        "/bin/PwSh.eXe",
    ] {
        let project = Project::new("repo", std::env::current_dir().unwrap());
        let preset = AgentPreset {
            program: Some(program.into()),
            ..AgentPreset::shell()
        };
        for options in [ctl::NewOptions::default(), argv(program)] {
            let instance = options
                .instance(&project, std::slice::from_ref(&preset), 0)
                .unwrap();
            // This is the program metadata a preset/literal shell retains after
            // creation. control_writable invokes this same policy on every write.
            assert!(control_program_writable(&instance, true).is_ok());
            let error =
                control_program_writable(&instance, false).expect_err("shell permission revoked");
            assert!(
                error.contains(COMMAND_SETTING),
                "unexpected refusal: {error}"
            );
        }
    }
}

#[test]
fn control_new_policy_refuses_all_literal_argv_without_mutation() {
    let native = std::env::current_exe().unwrap();
    for program in [native.to_str().unwrap(), "pwsh", "cmd", "node"] {
        for background in [false, true] {
            let mut options = argv(program);
            options.background = background;
            MuxelApp::fixture().refuses_without_mutation(options, COMMAND_SETTING);
        }
    }
}

#[test]
fn control_new_policy_refuses_agent_looking_literal_argv_without_mutation() {
    for program in ["codex", "claude"] {
        MuxelApp::fixture().refuses_without_mutation(argv(program), COMMAND_SETTING);
    }
}

#[test]
fn control_new_policy_refuses_shell_presets_and_defaults_without_mutation() {
    for program in [None, Some("cmd.exe"), Some("/usr/bin/bash")] {
        let mut app = MuxelApp::fixture();
        app.presets[0].name = "Custom terminal".into();
        app.presets[0].program = program.map(str::to_owned);
        for preset in [
            None,
            Some(app.presets[0].name.clone()),
            Some(app.presets[0].id.to_string()),
        ] {
            app.refuses_without_mutation(
                ctl::NewOptions {
                    preset,
                    ..Default::default()
                },
                COMMAND_SETTING,
            );
        }
        // Inactive projects select their saved default rather than the toolbar.
        app.workspace.active_project = None;
        app.workspace.projects[0].default_preset = Some(app.presets[0].id);
        app.current_preset = 1;
        app.refuses_without_mutation(ctl::NewOptions::default(), COMMAND_SETTING);
        app.presets.clear();
        app.refuses_without_mutation(ctl::NewOptions::default(), COMMAND_SETTING);
    }
}

#[test]
fn control_new_policy_allows_defined_agent_presets_with_creation_permission() {
    for preset in [None, Some("Codex".into())] {
        let mut app = MuxelApp::fixture();
        app.current_preset = 1;
        assert!(
            app.settings.control_enabled && app.settings.control_allow_new == CreationPolicy::Allow
        );
        assert!(!app.settings.control_allow_shells);
        app.open(
            ctl::NewOptions {
                preset,
                ..Default::default()
            },
            &mut Window::default(),
        )
        .unwrap();
        assert_eq!(app.launches, 1);
        assert_eq!(app.persists, 1);
        assert_eq!(
            app.workspace.instances.last().unwrap().preset_id,
            Some(app.presets[1].id)
        );
    }
}

#[test]
fn control_new_policy_opt_in_permits_literal_and_shell_launches() {
    for options in [
        argv(std::env::current_exe().unwrap().to_str().unwrap()),
        ctl::NewOptions::default(),
    ] {
        for background in [false, true] {
            let mut app = MuxelApp::fixture();
            app.settings.control_allow_shells = true;
            let mut options = options.clone();
            options.background = background;
            app.open(options, &mut Window::default()).unwrap();
            assert_eq!(app.placements, 1);
            assert_eq!(app.launches, 1);
            assert_eq!(app.persists, 1);
        }
    }
}

#[test]
fn control_new_policy_rechecks_current_settings_at_mutation_time() {
    let mut app = MuxelApp::fixture();
    app.current_preset = 1;
    let queued = ctl::NewOptions::default();
    app.settings.control_allow_new = CreationPolicy::Off;
    app.current_preset = 0;
    app.refuses_without_mutation(queued, CREATION_SETTING);
    let queued = ctl::NewOptions {
        preset: Some(app.presets[1].id.to_string()),
        ..Default::default()
    };
    app.presets[1].program = Some("cmd.exe".into());
    app.refuses_without_mutation(queued, CREATION_SETTING);
    // Re-enabling creation must still use the changed program and require the
    // separate command grant, not the agent classification from admission.
    app.settings.control_allow_new = CreationPolicy::Allow;
    let queued = ctl::NewOptions {
        preset: Some(app.presets[1].id.to_string()),
        ..Default::default()
    };
    app.refuses_without_mutation(queued, COMMAND_SETTING);
    app.presets[1] = AgentPreset::codex();
    app.settings.control_allow_shells = true;
    let queued = argv(std::env::current_exe().unwrap().to_str().unwrap());
    // The queue captures the request, not permission. Revoke before its callback.
    app.settings.control_allow_shells = false;
    app.refuses_without_mutation(queued, COMMAND_SETTING);
    // Default/preset selection may also change while the request is queued.
    app.current_preset = 1;
    let queued = ctl::NewOptions::default();
    app.current_preset = 0;
    app.refuses_without_mutation(queued, COMMAND_SETTING);
    let queued = ctl::NewOptions {
        preset: Some(app.presets[1].id.to_string()),
        ..Default::default()
    };
    app.presets[1].program = Some("cmd.exe".into());
    app.refuses_without_mutation(queued, COMMAND_SETTING);
    app.current_preset = 1;
    app.settings.control_allow_shells = true;
    app.settings.control_enabled = false;
    for options in [ctl::NewOptions::default(), argv("codex")] {
        app.refuses_without_mutation(options, "outside control is disabled");
    }
}

#[test]
fn control_new_policy_keeps_browser_lifecycle_and_window_refusals() {
    let mut app = MuxelApp::fixture();
    app.settings.control_allow_shells = true;
    app.presets[0].kind = muxel_core::PresetKind::Browser;
    app.refuses_without_mutation(ctl::NewOptions::default(), "browser presets");
    app.confirm_quit = true;
    app.refuses_without_mutation(ctl::NewOptions::default(), "quitting");
    app.confirm_quit = false;
    app.workspace_busy = app.current_workspace;
    app.refuses_without_mutation(ctl::NewOptions::default(), "workspace is closing");
    app.workspace_busy = None;
    app.presets[0].kind = muxel_core::PresetKind::Terminal;
    app.main_window = Some(WindowHandle(2));
    app.refuses_without_mutation(ctl::NewOptions::default(), "window changed");
}

#[test]
fn control_new_policy_production_queue_enters_the_tested_mutation_boundary() {
    let source = include_str!("../control_api.rs");
    assert!(source.contains("mod new;"));
    let queue = include_str!("approval.rs");
    let deferred = queue.split("cx.defer(move |cx|").nth(1).unwrap();
    assert!(deferred.contains("app.current_workspace != prepared.workspace"));
    let compact = deferred.split_whitespace().collect::<String>();
    assert!(compact.contains("app.control_new(prepared.pid,prepared.options,approved,window,cx)"));
    assert!(!queue.contains("place_and_spawn"));
    let write_boundary = source
        .split("    fn control_writable(")
        .nth(1)
        .unwrap()
        .split("    fn control_claim_site(")
        .next()
        .unwrap();
    assert!(
        write_boundary
            .contains("control_program_writable(inst, self.settings.control_allow_shells)?")
    );
}

#[test]
fn control_new_policy_ask_and_literal_allow_require_a_click_without_mutation() {
    for (policy, options) in [
        (
            CreationPolicy::Ask,
            ctl::NewOptions {
                preset: Some("Codex".into()),
                ..Default::default()
            },
        ),
        (
            CreationPolicy::Allow,
            argv(std::env::current_exe().unwrap().to_str().unwrap()),
        ),
        (CreationPolicy::Allow, ctl::NewOptions::default()),
    ] {
        let mut app = MuxelApp::fixture();
        app.settings.control_allow_new = policy;
        app.settings.control_allow_shells = true;
        let mut window = Window::default();
        let before = app.snapshot(&window);
        let result = app.control_new(
            app.workspace.projects[0].id,
            options,
            false,
            &mut window,
            &mut Context(PhantomData),
        );
        assert!(result.unwrap_err().message.contains("requires a click"));
        assert_eq!(app.snapshot(&window), before);
    }
}

#[test]
fn control_new_policy_approved_request_still_honors_revoked_creation_and_command_grants() {
    let mut app = MuxelApp::fixture();
    app.settings.control_allow_new = CreationPolicy::Off;
    let mut window = Window::default();
    let error = app
        .open(
            ctl::NewOptions {
                preset: Some("Codex".into()),
                ..Default::default()
            },
            &mut window,
        )
        .unwrap_err();
    assert_eq!(error.code, Some(ErrorCode::CreationDisabled));
    app.settings.control_allow_new = CreationPolicy::Ask;
    let error = app.open(argv("codex"), &mut window).unwrap_err();
    assert_eq!(error.code, Some(ErrorCode::CommandPermissionRequired));
    assert_eq!(app.launches, 0);
    assert_eq!(app.persists, 0);
}
