use muxel_core::control::{
    ApprovalQueue, Command, CreationDecision, CreationPolicy, ErrorCode, LaunchClass, NewOptions,
    Response, creation_decision, request_version,
};
use muxel_core::{AgentPreset, InjectionMode, Project, Settings, resolve_launch_for_session};
use serde_json::json;

#[test]
fn creation_policy_matrix_keeps_literal_and_shell_launches_click_only() {
    for policy in [
        CreationPolicy::Off,
        CreationPolicy::Ask,
        CreationPolicy::Allow,
    ] {
        for class in [
            LaunchClass::Preset,
            LaunchClass::Shell,
            LaunchClass::Literal,
        ] {
            for enabled in [false, true] {
                for commands in [false, true] {
                    let expected = if !enabled {
                        CreationDecision::Refuse(ErrorCode::ControlDisabled)
                    } else if policy == CreationPolicy::Off {
                        CreationDecision::Refuse(ErrorCode::CreationDisabled)
                    } else if class != LaunchClass::Preset && !commands {
                        CreationDecision::Refuse(ErrorCode::CommandPermissionRequired)
                    } else if policy == CreationPolicy::Allow && class == LaunchClass::Preset {
                        CreationDecision::Allow
                    } else {
                        CreationDecision::Ask
                    };
                    assert_eq!(
                        creation_decision(enabled, policy, commands, class),
                        expected
                    );
                }
            }
        }
    }
}

#[test]
fn creation_policy_accepts_old_bools_and_persists_named_choices() {
    assert_eq!(Settings::default().control_allow_new, CreationPolicy::Off);
    for (value, expected) in [
        (json!(false), CreationPolicy::Off),
        (json!(true), CreationPolicy::Allow),
        (json!("off"), CreationPolicy::Off),
        (json!("ask"), CreationPolicy::Ask),
        (json!("allow"), CreationPolicy::Allow),
    ] {
        let settings: Settings =
            serde_json::from_value(json!({"control_allow_new":value})).unwrap();
        assert_eq!(settings.control_allow_new, expected);
        let saved = serde_json::to_value(settings).unwrap();
        assert!(saved["control_allow_new"].is_string());
        assert_eq!(
            serde_json::from_value::<Settings>(saved)
                .unwrap()
                .control_allow_new,
            expected
        );
    }
    assert!(serde_json::from_value::<Settings>(json!({"control_allow_new":"always"})).is_err());
}

#[test]
fn approval_queue_is_fifo_and_decisions_are_at_most_once() {
    let mut queue = ApprovalQueue::default();
    let first = queue.admit("first", 100).unwrap();
    let second = queue.admit("second", 200).unwrap();
    queue.mark_visible(200);
    assert_eq!(queue.front().unwrap().id, first);
    assert!(
        queue.take(second, 1000).is_none(),
        "cannot approve a hidden request"
    );
    assert_eq!(queue.status(first).unwrap()["pending"], true);
    assert!(queue.press(first, 1000));
    assert_eq!(queue.take(first, 1000), Some("first"));
    assert!(queue.take(first, 1000).is_none());
    queue.finish(first, Response::ok(json!({"uuid":"created"})), 1000);
    assert_eq!(queue.status(first).unwrap()["uuid"], "created");
    assert_eq!(queue.front().unwrap().id, second);
    queue.mark_visible(1000);
    assert!(queue.press(second, 1750));
    queue.deny(second, 1750);
    assert_eq!(
        queue.status(second).unwrap_err().code,
        Some(ErrorCode::ApprovalDenied)
    );
    assert!(queue.front().is_none());
}

#[test]
fn each_visible_card_arms_both_decisions_after_750_ms() {
    let mut queue = ApprovalQueue::default();
    let first = queue.admit("first", 0).unwrap();
    let second = queue.admit("second", 1).unwrap();
    assert!(queue.take(first, 10_000).is_none(), "not yet visible");
    queue.mark_visible(10_000);
    assert!(!queue.is_armed(first, 10_749));
    assert!(queue.take(first, 10_749).is_none());
    queue.deny(first, 10_749);
    assert_eq!(queue.status(first).unwrap()["pending"], true);
    queue.mark_visible(10_749); // Redrawing the same card does not reset its clock.
    assert!(queue.is_armed(first, 10_750));
    assert!(queue.press(first, 10_750));
    assert_eq!(queue.take(first, 10_750), Some("first"));
    queue.finish(first, Response::ok(json!({"uuid":"first"})), 10_750);

    assert!(queue.take(second, 10_750).is_none());
    queue.mark_visible(10_750);
    assert!(
        !queue.is_armed(second, 11_499),
        "queued age cannot arm a new card"
    );
    queue.deny(second, 11_499);
    assert_eq!(queue.status(second).unwrap()["pending"], true);
    assert!(queue.is_armed(second, 11_500));
    assert!(queue.press(second, 11_500));
    queue.deny(second, 11_500);
    assert_eq!(
        queue.status(second).unwrap_err().code,
        Some(ErrorCode::ApprovalDenied)
    );
}

#[test]
fn a_decision_needs_an_armed_press_on_that_same_visible_request() {
    let mut queue = ApprovalQueue::default();
    let first = queue.admit("first", 0).unwrap();
    queue.mark_visible(0);
    assert!(!queue.press(first, 749));
    assert!(
        queue.take(first, 750).is_none(),
        "holding an early press cannot arm it"
    );
    assert_eq!(queue.status(first).unwrap()["pending"], true);
    assert!(queue.press(first, 750));
    assert_eq!(queue.take(first, 750), Some("first"));
    assert!(queue.take(first, 751).is_none(), "a press is consumed once");

    let mut queue = ApprovalQueue::default();
    let first = queue.admit("first", 0).unwrap();
    queue.mark_visible(0);
    let second = queue.admit("second", 1000).unwrap();
    assert!(queue.press(first, 119_999));
    queue.expire(120_000);
    assert_eq!(
        queue.status(first).unwrap_err().code,
        Some(ErrorCode::ApprovalTimeout)
    );
    queue.mark_visible(120_000);
    assert!(queue.is_armed(second, 120_750));
    assert!(
        queue.take(second, 120_750).is_none(),
        "the old card's held press cannot decide the next card"
    );
    assert_eq!(queue.status(second).unwrap()["pending"], true);
    assert!(queue.press(second, 120_750));
    queue.deny(second, 120_750);
    assert_eq!(
        queue.status(second).unwrap_err().code,
        Some(ErrorCode::ApprovalDenied)
    );
}

#[test]
fn approval_deadline_is_measured_from_admission_including_time_behind_another_card() {
    let mut queue = ApprovalQueue::default();
    let first = queue.admit(1, 1000).unwrap();
    let second = queue.admit(2, 2000).unwrap();
    queue.expire(120_999);
    assert_eq!(queue.len(), 2);
    queue.expire(121_000);
    assert_eq!(
        queue.status(first).unwrap_err().code,
        Some(ErrorCode::ApprovalTimeout)
    );
    assert_eq!(queue.front().unwrap().id, second);
    queue.expire(122_000);
    assert!(queue.front().is_none());
    assert!(queue.take(second, 122_000).is_none());
    assert_eq!(
        queue.status(second).unwrap_err().code,
        Some(ErrorCode::ApprovalTimeout)
    );
}

#[test]
fn revoking_creation_or_outside_control_refuses_every_pending_request() {
    for code in [ErrorCode::ControlDisabled, ErrorCode::CreationDisabled] {
        let mut queue = ApprovalQueue::default();
        let ids = [queue.admit(1, 0).unwrap(), queue.admit(2, 1).unwrap()];
        queue.refuse_all(code, 10);
        assert!(queue.front().is_none());
        for id in ids {
            assert_eq!(queue.status(id).unwrap_err().code, Some(code));
            assert!(queue.take(id, 10).is_none());
        }
    }
}

#[test]
fn approval_queue_bounds_pending_and_finished_retention() {
    let mut queue = ApprovalQueue::default();
    for _ in 0..64 {
        queue.admit((), 0).unwrap();
    }
    assert!(queue.admit((), 0).is_err());
    let first = queue.front().unwrap().id;
    queue.refuse_all(ErrorCode::CreationDisabled, 1);
    assert!(queue.status(first).is_err());
    queue.expire(600_002);
    assert_eq!(queue.status(first).unwrap_err().code, None);
    let first = queue.admit((), 600_003).unwrap();
    queue.mark_visible(600_003);
    assert!(queue.press(first, 600_753));
    queue.deny(first, 600_753);
    let mut last = first;
    let mut now = 600_753;
    for _ in 0..256 {
        last = queue.admit((), now).unwrap();
        queue.mark_visible(now);
        now += 750;
        assert!(queue.press(last, now));
        queue.deny(last, now);
    }
    assert_eq!(queue.status(first).unwrap_err().code, None);
    assert_eq!(
        queue.status(last).unwrap_err().code,
        Some(ErrorCode::ApprovalDenied)
    );
}

#[test]
fn creation_refusal_codes_roundtrip_and_are_explained_in_the_embedded_skill() {
    let skill = muxel_core::control::skill("muxel");
    for (code, name) in [
        (ErrorCode::ControlDisabled, "control_disabled"),
        (ErrorCode::CreationDisabled, "creation_disabled"),
        (
            ErrorCode::CommandPermissionRequired,
            "command_permission_required",
        ),
        (ErrorCode::ApprovalDenied, "approval_denied"),
        (ErrorCode::ApprovalTimeout, "approval_timeout"),
        (ErrorCode::PromptUnsupported, "prompt_unsupported"),
        (ErrorCode::ParameterUnsupported, "parameter_unsupported"),
    ] {
        assert_eq!(serde_json::to_value(code).unwrap(), name);
        assert_eq!(
            serde_json::from_value::<ErrorCode>(json!(name)).unwrap(),
            code
        );
        assert!(
            skill.contains(&format!("`{name}`")),
            "missing {name} from skill"
        );
    }
}

#[test]
fn plain_new_and_status_require_v2_including_requests_that_can_ask() {
    assert_eq!(
        request_version(&Command::New {
            project: "repo".into(),
            options: NewOptions::default()
        }),
        2
    );
    assert_eq!(
        request_version(&Command::NewStatus {
            request_id: uuid::Uuid::new_v4()
        }),
        2
    );
}

#[test]
fn new_prompt_is_submitted_once_and_not_retyped_after_nonresume_relaunch() {
    let project = Project::new("repo", std::env::current_dir().unwrap());
    for preset in [
        AgentPreset::claude(),
        AgentPreset::grok(),
        AgentPreset::codex(),
    ] {
        let instruction_transport = preset.injection.clone();
        let options = NewOptions {
            prompt: Some("do {{input}} literally\nsecond line".into()),
            ..Default::default()
        };
        let mut instance = options.instance(&project, &[preset], 0).unwrap();
        assert!(instance.is_runner);
        assert_eq!(instance.injection, InjectionMode::TypeIn);
        assert_eq!(instance.instruction_injection, Some(instruction_transport));
        assert_eq!(instance.auto_mode_presses, 0);
        let launch = resolve_launch_for_session(&instance, false);
        assert_eq!(
            launch.startup_input.as_deref(),
            Some("do {{input}} literally\nsecond line")
        );
        assert!(launch.submit);
        assert_eq!(
            resolve_launch_for_session(&instance, true).startup_input,
            None
        );
        // The app consumes this after constructing the first launch plan.
        assert!(instance.consume_runner_prompt());
        assert!(instance.system_prompt.is_none());
        assert!(!instance.consume_runner_prompt());
        let restored = serde_json::from_value(serde_json::to_value(instance).unwrap()).unwrap();
        for resuming in [false, true] {
            let launch = resolve_launch_for_session(&restored, resuming);
            assert!(!launch.submit);
            assert!(
                launch.startup_input.is_none(),
                "no old task left in the input line"
            );
        }
    }
}

#[test]
fn consuming_a_ctl_task_does_not_change_reusable_runner_restore_behavior() {
    let preset = AgentPreset::claude();
    let mut instance = muxel_core::Runner {
        id: uuid::Uuid::new_v4(),
        name: "repeatable".into(),
        preset_id: Some(preset.id),
        auto_mode_presses: 0,
        prompt: "{{input}}".into(),
    }
    .build_instance(uuid::Uuid::new_v4(), &preset, "keep this task");
    assert!(instance.consume_runner_prompt());
    let launch = resolve_launch_for_session(&instance, false);
    assert_eq!(launch.startup_input.as_deref(), Some("keep this task"));
    assert!(!launch.submit);
    let mut saved = serde_json::to_value(instance).unwrap();
    saved.as_object_mut().unwrap().remove("control_prompt");
    let mut legacy: muxel_core::Instance = serde_json::from_value(saved).unwrap();
    assert!(!legacy.consume_runner_prompt());
    assert_eq!(legacy.system_prompt.as_deref(), Some("keep this task"));
}

#[test]
fn cli_parses_prompt_caller_no_wait_and_status_without_changing_literal_argv() {
    let args = [
        "new",
        "repo",
        "--prompt",
        "-",
        "--from",
        "Wire B5",
        "--no-wait",
    ]
    .map(str::to_string);
    let muxel_core::control::CliCommand::Request(command) =
        muxel_core::control::parse_cli(&args).unwrap()
    else {
        panic!()
    };
    let Command::New { options, .. } = &command else {
        panic!()
    };
    assert_eq!(options.prompt.as_deref(), Some("-"));
    assert_eq!(options.from.as_deref(), Some("Wire B5"));
    assert!(options.no_wait);
    assert_eq!(request_version(&command), 2);
    let id = uuid::Uuid::new_v4();
    let status = muxel_core::control::parse_cli(&["new-status".into(), id.to_string()]).unwrap();
    assert_eq!(
        status,
        muxel_core::control::CliCommand::Request(Command::NewStatus { request_id: id })
    );
    for args in [
        vec!["new", "repo", "--prompt"],
        vec!["new", "repo", "--from"],
        vec!["new-status", "bad-id"],
    ] {
        assert!(
            muxel_core::control::parse_cli(
                &args.into_iter().map(str::to_string).collect::<Vec<_>>()
            )
            .is_err()
        );
    }
}

#[test]
fn prompt_is_refused_for_literal_shell_and_browser_launches() {
    let project = Project::new("repo", std::env::current_dir().unwrap());
    let mut browser = AgentPreset::shell();
    browser.kind = muxel_core::PresetKind::Browser;
    for (options, preset) in [
        (
            NewOptions {
                argv: Some(vec!["codex".into()]),
                prompt: Some("task".into()),
                ..Default::default()
            },
            AgentPreset::codex(),
        ),
        (
            NewOptions {
                prompt: Some("task".into()),
                ..Default::default()
            },
            AgentPreset::shell(),
        ),
        (
            NewOptions {
                prompt: Some("task".into()),
                ..Default::default()
            },
            browser,
        ),
    ] {
        assert_eq!(
            options.instance(&project, &[preset], 0).unwrap_err().code,
            Some(ErrorCode::PromptUnsupported)
        );
    }
}
