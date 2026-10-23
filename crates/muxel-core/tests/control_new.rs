use muxel_core::control::{CliCommand, NewOptions, NewPlacement, parse_cli, place_new_pane};
use muxel_core::tmux::Socket;
use muxel_core::{
    AgentPreset, Instance, Project, SplitDirection, resolve_launch_for_session, session_resume_args,
};
use muxel_core::{PaneNode, add_tab, set_active_tab};
use uuid::Uuid;

fn words(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

#[test]
fn outside_creation_origin_survives_restore_and_local_duplicates_get_ui_policy() {
    let project = Project::new("repo", std::env::current_dir().unwrap());
    let preset = AgentPreset::codex();
    for options in [
        NewOptions::default(),
        NewOptions {
            prompt: Some("task".into()),
            ..Default::default()
        },
        NewOptions {
            argv: Some(words(&["native.exe", "brief"])),
            ..Default::default()
        },
    ] {
        let instance = options
            .instance(&project, std::slice::from_ref(&preset), 0)
            .unwrap();
        assert!(instance.control_created);
        let mut restored: Instance =
            serde_json::from_value(serde_json::to_value(instance).unwrap()).unwrap();
        assert!(restored.control_created);
        restored.reset_conversation_for_duplicate();
        assert!(!restored.control_created);
    }
    let ui = Instance::from_preset(project.id, &preset);
    assert!(!ui.control_created);
    let mut legacy = serde_json::to_value(ui).unwrap();
    legacy.as_object_mut().unwrap().remove("control_created");
    assert!(
        !serde_json::from_value::<Instance>(legacy)
            .unwrap()
            .control_created
    );
}

#[test]
fn new_parameters_parse_round_trip_and_validate_json_values() {
    let CliCommand::Request(command) = parse_cli(&words(&[
        "new",
        "repo",
        "--model",
        "provider/model:v1@2026+fast",
        "--effort",
        "high",
    ]))
    .unwrap() else {
        panic!()
    };
    let json = serde_json::to_value(&command).unwrap();
    assert_eq!(json["model"], "provider/model:v1@2026+fast");
    assert_eq!(json["effort"], "high");
    assert_eq!(
        serde_json::from_value::<muxel_core::control::Command>(json).unwrap(),
        command
    );
    for field in ["model", "effort"] {
        for invalid in ["", "-high", "has space", "x=y", "x\n", "é", &"a".repeat(65)] {
            let options: NewOptions =
                serde_json::from_value(serde_json::json!({field:invalid})).unwrap();
            assert!(options.validate().is_err(), "accepted {field}={invalid:?}");
            assert!(parse_cli(&words(&["new", "repo", &format!("--{field}"), invalid])).is_err());
        }
        assert!(parse_cli(&words(&["new", "repo", &format!("--{field}")])).is_err());
        let options: NewOptions =
            serde_json::from_value(serde_json::json!({field:"a".repeat(64)})).unwrap();
        options.validate().unwrap();
    }
}

#[test]
fn new_name_and_caller_label_are_bounded_visible_text() {
    use muxel_core::control::MAX_LABEL_CHARS;
    for field in ["name", "from"] {
        for invalid in [
            "".to_string(),
            "   ".into(),
            "two\nrows".into(),
            "tab\there".into(),
            "fake\rrow".into(),
            "esc\u{1b}[31m".into(),
            "rtl\u{202e}exe.txt".into(),
            "isolate\u{2066}x\u{2069}".into(),
            "zero\u{200b}width".into(),
            "joiner\u{200d}".into(),
            "bom\u{feff}".into(),
            "line\u{2028}sep".into(),
            "para\u{2029}sep".into(),
            "next\u{85}line".into(),
            "a".repeat(MAX_LABEL_CHARS + 1),
        ] {
            let options: NewOptions =
                serde_json::from_value(serde_json::json!({field: invalid})).unwrap();
            assert!(options.validate().is_err(), "accepted {field}={invalid:?}");
            assert!(
                parse_cli(&words(&["new", "repo", &format!("--{field}"), &invalid])).is_err(),
                "CLI accepted {field}={invalid:?}"
            );
        }
        for valid in [
            "Reviewer".to_string(),
            "claude-code (pane 1a2b3c4d)".into(),
            "Revue · étape 2 ✓".into(),
            "é".repeat(MAX_LABEL_CHARS),
        ] {
            let options: NewOptions =
                serde_json::from_value(serde_json::json!({field: valid})).unwrap();
            options.validate().unwrap();
        }
    }
}

#[test]
fn new_prompt_is_typed_without_trailing_whitespace_and_fits_its_card() {
    use muxel_core::control::{MAX_PROMPT_CHARS, card_prompt};
    let project = Project::new("repo", std::env::current_dir().unwrap());
    let preset = AgentPreset::claude();
    let raw = "\n  fix the pager\n\n  then run tests \t\n\n";
    let instance = NewOptions {
        prompt: Some(raw.into()),
        ..Default::default()
    }
    .instance(&project, std::slice::from_ref(&preset), 0)
    .unwrap();
    // Leading blank lines and indentation are the caller's; trailing
    // whitespace would only be extra Enters.
    assert_eq!(
        instance.system_prompt.as_deref(),
        Some("\n  fix the pager\n\n  then run tests")
    );
    assert_eq!(
        resolve_launch_for_session(&instance, false)
            .startup_input
            .as_deref(),
        Some("\n  fix the pager\n\n  then run tests")
    );

    // Everything accepted is drawn on the card; longer tasks are refused.
    let longest = format!("{}{}", "é".repeat(MAX_PROMPT_CHARS), "\n \n");
    let options = NewOptions {
        prompt: Some(longest.clone()),
        ..Default::default()
    };
    options.validate().unwrap();
    assert_eq!(card_prompt(&longest, MAX_PROMPT_CHARS).omitted_chars, 0);
    for too_long in [
        format!("{}x", "é".repeat(MAX_PROMPT_CHARS)),
        "a".repeat(MAX_PROMPT_CHARS + 1),
    ] {
        let options = NewOptions {
            prompt: Some(too_long.clone()),
            ..Default::default()
        };
        let error = options.validate().unwrap_err();
        assert!(error.contains("limited to"), "{error}");
        assert!(parse_cli(&words(&["new", "repo", "--prompt", &too_long])).is_err());
    }
}

#[test]
fn new_parameters_freeze_both_plain_and_prompt_launches_without_mutating_preset() {
    let project = Project::new("repo", std::env::current_dir().unwrap());
    for preset in [AgentPreset::claude(), AgentPreset::codex()] {
        let original = preset.clone();
        for prompt in [None, Some("do this task".into())] {
            let instance = NewOptions {
                model: Some("chosen-model".into()),
                effort: Some("high".into()),
                prompt,
                ..Default::default()
            }
            .instance(&project, std::slice::from_ref(&preset), 0)
            .unwrap();
            let expected = if preset.name == "Claude" {
                words(&["--model", "chosen-model", "--effort", "high"])
            } else {
                words(&[
                    "--model",
                    "chosen-model",
                    "--config=model_reasoning_effort=high",
                ])
            };
            assert_eq!(instance.args, expected);
            let restored: Instance =
                serde_json::from_value(serde_json::to_value(&instance).unwrap()).unwrap();
            assert_eq!(restored.args, expected);
            assert!(!restored.literal_argv);
        }
        assert_eq!(preset, original);
    }
}

#[test]
fn new_parameters_refuse_missing_flags_literal_commands_and_conflicting_extra_args() {
    use muxel_core::control::ErrorCode;
    let project = Project::new("repo", std::env::current_dir().unwrap());
    for (field, flag, conflicts) in [
        (
            "model",
            "--model",
            vec![words(&["--model", "old"]), words(&["--model=old"])],
        ),
        (
            "effort",
            "--effort",
            vec![words(&["--effort", "low"]), words(&["--effort=low"])],
        ),
        (
            "effort",
            "--config=model_reasoning_effort={}",
            vec![
                words(&["-c model_reasoning_effort=low"]),
                words(&["-c", "model_reasoning_effort=low"]),
                words(&["-cmodel_reasoning_effort=low"]),
                words(&["--config", "model_reasoning_effort=low"]),
                words(&["--config=model_reasoning_effort=low"]),
            ],
        ),
        (
            "model",
            "--model={}",
            vec![words(&["--model", "old"]), words(&["--model=old"])],
        ),
    ] {
        let options: NewOptions =
            serde_json::from_value(serde_json::json!({field:"high"})).unwrap();
        for missing in [None, Some("".into()), Some("  ".into())] {
            let mut preset = AgentPreset::claude();
            if field == "model" {
                preset.model_flag = missing;
            } else {
                preset.effort_flag = missing;
            }
            let error = options.instance(&project, &[preset], 0).unwrap_err();
            assert_eq!(error.code, Some(ErrorCode::ParameterUnsupported));
            assert!(error.message.contains(if field == "model" {
                "Model flag"
            } else {
                "Effort flag"
            }));
        }
        for args in conflicts {
            let mut preset = AgentPreset::claude();
            if field == "model" {
                preset.model_flag = Some(flag.into());
            } else {
                preset.effort_flag = Some(flag.into());
            }
            preset.args = args;
            assert_eq!(
                options.instance(&project, &[preset], 0).unwrap_err().code,
                Some(ErrorCode::ParameterUnsupported)
            );
        }
        let literal = NewOptions {
            argv: Some(words(&["codex"])),
            ..options
        };
        assert_eq!(
            literal.instance(&project, &[], 0).unwrap_err().code,
            Some(ErrorCode::ParameterUnsupported)
        );
    }
}

#[test]
fn preset_parameter_templates_emit_single_arguments_for_model_and_effort() {
    let mut preset = AgentPreset::claude();
    preset.model_flag = Some("--model={}".into());
    preset.model = Some("sonnet".into());
    preset.effort_flag = Some("effort:{}:end".into());
    preset.effort = Some("high".into());
    preset.args = words(&["extra"]);
    assert_eq!(
        preset.compose_args(),
        words(&["--model=sonnet", "effort:high:end", "extra"])
    );
}

#[test]
fn codex_effort_override_preserves_unrelated_config_arguments() {
    let project = Project::new("repo", std::env::current_dir().unwrap());
    let mut preset = AgentPreset::codex();
    preset.args = words(&["-c", "other_config=true", "--config=another_config=false"]);
    let instance = NewOptions {
        effort: Some("low".into()),
        ..Default::default()
    }
    .instance(&project, &[preset], 0)
    .unwrap();
    assert_eq!(
        instance.args,
        words(&[
            "--config=model_reasoning_effort=low",
            "-c",
            "other_config=true",
            "--config=another_config=false"
        ])
    );
}

#[test]
fn parameterized_creation_requires_v3_without_changing_plain_creation_or_status() {
    use muxel_core::control::{Command, request_version};
    for options in [
        NewOptions {
            model: Some("sonnet".into()),
            ..Default::default()
        },
        NewOptions {
            effort: Some("high".into()),
            ..Default::default()
        },
    ] {
        assert_eq!(
            request_version(&Command::New {
                project: "repo".into(),
                options
            }),
            3
        );
    }
    assert_eq!(
        request_version(&Command::New {
            project: "repo".into(),
            options: Default::default()
        }),
        2
    );
    assert_eq!(
        request_version(&Command::NewStatus {
            request_id: Uuid::new_v4()
        }),
        2
    );
}

#[test]
fn new_preserves_literal_argv_in_protocol() {
    let args = words(&[
        "new",
        "repo",
        "--",
        "codex",
        "a brief with spaces",
        "say \"hello\"",
        "",
        "--",
        "--background",
    ]);
    let CliCommand::Request(command) = parse_cli(&args).expect("new parses") else {
        panic!("expected a request");
    };
    let json = serde_json::to_value(&command).unwrap();
    assert_eq!(json["cmd"], "new");
    assert_eq!(json["project"], "repo");
    assert_eq!(json["argv"], serde_json::json!(&args[3..]));
    assert_eq!(json["background"], true);
    assert_eq!(json["focus"], false);
    assert_eq!(
        serde_json::from_value::<muxel_core::control::Command>(json).unwrap(),
        command
    );
}

#[test]
fn new_rejects_invalid_arguments() {
    for args in [
        words(&["new"]),
        words(&["new", "repo", "--preset", "Codex", "--", "codex"]),
        words(&["new", "repo", "--split", "left"]),
        words(&["new", "repo", "--cwd", "relative/path"]),
        words(&["new", "repo", "--tab", "--split", "right"]),
        words(&["new", "repo", "--"]),
        words(&["new", "repo", "--", ""]),
        words(&["new", "repo", "--preset"]),
        words(&["new", "repo", "--cwd"]),
        words(&["new", "repo", "--split"]),
        words(&["new", "repo", "--name"]),
        words(&["new", "repo", "extra"]),
    ] {
        assert!(parse_cli(&args).is_err(), "accepted {args:?}");
    }
}

#[test]
fn new_defaults_and_explicit_options_round_trip() {
    let CliCommand::Request(muxel_core::control::Command::New { project, options }) =
        parse_cli(&words(&["new", "active"])).unwrap()
    else {
        panic!()
    };
    assert_eq!(project, "active");
    assert_eq!(
        options,
        NewOptions {
            background: true,
            ..Default::default()
        }
    );
    let cwd = std::env::current_dir().unwrap();
    let args = vec![
        "new".into(),
        "repo".into(),
        "--cwd".into(),
        cwd.display().to_string(),
        "--name".into(),
        "Builder B3".into(),
        "--background".into(),
        "--split".into(),
        "down".into(),
        "--preset".into(),
        "Codex".into(),
    ];
    let CliCommand::Request(muxel_core::control::Command::New { options, .. }) =
        parse_cli(&args).unwrap()
    else {
        panic!()
    };
    assert_eq!(options.cwd, Some(cwd));
    assert_eq!(options.name.as_deref(), Some("Builder B3"));
    assert_eq!(options.placement, NewPlacement::Down);
    assert!(options.background);
    assert_eq!(options.preset.as_deref(), Some("Codex"));
    let old: muxel_core::control::Command =
        serde_json::from_value(serde_json::json!({"cmd":"new", "project":"repo"})).unwrap();
    assert_eq!(
        old,
        muxel_core::control::Command::New {
            project: "repo".into(),
            options: NewOptions::default()
        }
    );
}

#[test]
fn new_preserves_focus_by_default_and_focus_wins_over_legacy_background() {
    for (flags, focus) in [
        (vec![], false),
        (vec!["--background"], false),
        (vec!["--focus"], true),
        (vec!["--background", "--focus"], true),
        (vec!["--focus", "--background"], true),
    ] {
        let mut args = words(&["new", "repo"]);
        args.extend(words(&flags));
        let CliCommand::Request(muxel_core::control::Command::New { options, .. }) =
            parse_cli(&args).unwrap()
        else {
            panic!("expected new");
        };
        assert_eq!(options.focus, focus);
        // Older hosts only know background. The CLI can still express the same
        // choice without introducing a protocol requirement for a UI preference.
        assert_eq!(options.background, !focus);
        let restored: NewOptions =
            serde_json::from_value(serde_json::to_value(&options).unwrap()).unwrap();
        assert_eq!(restored, options);
    }
    for background in [false, true] {
        let options: NewOptions =
            serde_json::from_value(serde_json::json!({"background":background})).unwrap();
        assert!(!options.focus, "legacy JSON defaults to preserving focus");
        let options: NewOptions =
            serde_json::from_value(serde_json::json!({"background":background,"focus":true}))
                .unwrap();
        assert!(options.focus);
    }
    let skill = muxel_core::control::skill("muxel");
    for help in [muxel_core::control::USAGE, skill.as_str()] {
        assert!(help.contains("--focus"));
        assert!(help.contains("--background"));
    }
}

#[test]
fn new_uses_the_selected_preset_and_allows_a_plain_external_cwd() {
    let project = Project::new("repo", std::env::current_dir().unwrap());
    let presets = vec![AgentPreset::shell(), AgentPreset::codex()];
    let default = NewOptions::default()
        .instance(&project, &presets, 1)
        .unwrap();
    assert_eq!(default.preset_id, Some(presets[1].id));
    assert!(
        default.worktree_path.is_none(),
        "launch cwd must not be owned"
    );
    assert_eq!(default.launch_dir.as_ref(), Some(&project.root_path));
    let options = NewOptions {
        preset: Some(presets[0].id.to_string()),
        cwd: Some(std::env::temp_dir()),
        ..NewOptions::default()
    };
    let external = options.instance(&project, &presets, 1).unwrap();
    assert_eq!(external.preset_id, Some(presets[0].id));
    assert!(
        external.worktree_path.is_none(),
        "external cwd must not be owned"
    );
    assert_eq!(external.launch_dir, options.cwd);
    assert!(!external.use_worktree);
    assert!(external.worktree_id.is_none());
    let named = NewOptions {
        preset: Some(presets[1].name.clone()),
        ..NewOptions::default()
    };
    assert_eq!(
        named.instance(&project, &presets, 0).unwrap().preset_id,
        Some(presets[1].id)
    );
    assert!(
        named
            .instance(&project, &[presets[1].clone(), presets[1].clone()], 0)
            .is_err()
    );
    assert!(
        NewOptions {
            preset: Some("missing".into()),
            ..NewOptions::default()
        }
        .instance(&project, &presets, 0)
        .is_err()
    );
}

#[test]
fn launch_directory_is_never_promoted_to_an_owned_worktree() {
    let project = Project::new("repo", std::env::current_dir().unwrap());
    let instance = NewOptions {
        cwd: Some(std::env::temp_dir()),
        ..NewOptions::default()
    }
    .instance(&project, &[], 0)
    .unwrap();
    let mut workspace = muxel_core::Workspace {
        projects: vec![project],
        instances: vec![instance],
        ..Default::default()
    };
    muxel_core::migrate_worktrees(&mut workspace);
    assert!(
        workspace.worktrees.is_empty(),
        "launch directory acquired ownership"
    );
    assert!(workspace.instances[0].worktree_id.is_none());
    assert_eq!(
        workspace.instances[0].worktree_cleanup(),
        muxel_core::WorktreeCleanup::None
    );
}

#[test]
fn cleanup_tracks_owned_worktrees_separately_from_the_launch_directory() {
    use muxel_core::WorktreeCleanup;
    let mut instance = Instance::shell(Uuid::new_v4());
    instance.launch_dir = Some(std::env::temp_dir().join("caller-owned"));
    assert_eq!(instance.worktree_cleanup(), WorktreeCleanup::None);
    let owned = std::env::temp_dir().join("muxel-owned");
    instance.worktree_path = Some(owned.clone());
    assert_eq!(instance.worktree_cleanup(), WorktreeCleanup::Legacy(owned));
    let id = Uuid::new_v4();
    instance.worktree_id = Some(id);
    assert_eq!(instance.worktree_cleanup(), WorktreeCleanup::Registered(id));
}

#[test]
fn literal_restore_selection_restart_wake_and_recovery_require_explicit_replay() {
    use muxel_core::TerminalLaunchReason::*;
    let mut instance = Instance::shell(Uuid::new_v4());
    instance.literal_argv = true;
    let restored: Instance =
        serde_json::from_str(&serde_json::to_string(&instance).unwrap()).unwrap();
    for reason in [
        Restore,
        ProjectSelection,
        Restart,
        Wake,
        EnsureRunning,
        Recovery,
    ] {
        assert!(
            !restored.permits_launch(reason),
            "literal replay allowed for {reason:?}"
        );
        let mut preset = restored.clone();
        preset.literal_argv = false;
        assert!(
            preset.permits_launch(reason),
            "normal preset recovery refused for {reason:?}"
        );
    }
    assert!(restored.permits_launch(NewPane));
    assert!(restored.permits_launch(RunAgain));
}

#[test]
fn restored_literal_without_a_view_is_stopped_unless_launching() {
    let mut literal = Instance::shell(Uuid::new_v4());
    literal.literal_argv = true;
    // Restoring a literal leaves it without a terminal view or process.
    assert!(literal.is_stopped_literal(false, false));
    assert!(!literal.is_stopped_literal(false, true));
    let mut ordinary = literal.clone();
    ordinary.literal_argv = false;
    assert!(!ordinary.is_stopped_literal(false, false));
    literal.kind = muxel_core::InstanceKind::Editor;
    assert!(!literal.is_stopped_literal(false, false));
}

#[test]
fn retained_literal_view_keeps_its_badge_including_done_after_exit() {
    let mut literal = Instance::shell(Uuid::new_v4());
    literal.literal_argv = true;
    // Running and exited panes both retain a view. An exited view's done badge
    // and completion age must not be replaced by the restored-pane override.
    assert!(!literal.is_stopped_literal(true, false));
    assert!(!literal.is_stopped_literal(true, true));
}

#[test]
fn peer_layout_preserves_literal_launch_and_never_binds_it_to_tmux() {
    let project = Project::new("repo", std::env::current_dir().unwrap());
    let literal = NewOptions {
        argv: Some(words(&["codex", "one-shot brief", ""])),
        ..Default::default()
    }
    .instance(&project, &[], 0)
    .unwrap();
    let normal = Instance::shell(project.id);
    let mut ws = muxel_core::Workspace {
        instances: vec![literal.clone(), normal.clone()],
        ..Default::default()
    };
    let mut project = project;
    project.layout = Some(PaneNode::leaf(literal.id));
    add_tab(&mut project.layout, literal.id, normal.id);
    ws.projects.push(project.clone());
    let doc = muxel_core::RemoteLayout::capture(&project, &ws, 10);
    let peer =
        muxel_core::RemoteLayout::parse(&doc.to_json(), &project.root_path.display().to_string())
            .unwrap()
            .instances;
    let adopted_project = Uuid::new_v4();
    let mut adopted = muxel_core::merge_peer_instances(&[], peer, adopted_project);
    let sessions = [muxel_core::tmux::RemoteSession {
        name: muxel_core::tmux::session_name("peer", literal.id),
        socket: muxel_core::tmux::Socket::Private,
        path: String::new(),
        command: "codex".into(),
        title: String::new(),
    }];
    muxel_core::tmux::bind_peer_sessions(&project.name, &mut adopted, &sessions, Socket::Private);
    let raw = adopted.iter().find(|i| i.id == literal.id).unwrap();
    assert!(raw.literal_argv);
    assert_eq!(raw.project_id, adopted_project);
    assert_eq!(raw.launch_dir, literal.launch_dir);
    assert_eq!(raw.args, literal.args);
    assert!(!raw.use_tmux);
    assert!(raw.tmux_session.is_none());
    assert!(raw.tmux_socket.is_none());
    let normal = adopted.iter().find(|i| i.id == normal.id).unwrap();
    assert!(normal.use_tmux);
    assert!(normal.tmux_session.is_some());
    assert_eq!(normal.tmux_socket, Some(muxel_core::tmux::Socket::Private));
}

#[test]
fn isolated_peer_binding_overrides_foreign_owners_and_leaves_literals_unchanged() {
    use muxel_core::tmux::{RemoteSession, bind_peer_sessions, session_name};
    let project = Project::new("repo", "/repo");
    let socket = Socket::isolated(std::path::Path::new("/isolated-peer"));
    let mut literal = Instance::shell(project.id);
    literal.literal_argv = true;
    literal.tmux_socket = Some(Socket::Default);
    literal.args = words(&["literal task", ""]);
    let saved_literal = serde_json::to_value(&literal).unwrap();
    for prior in [
        None,
        Some(Socket::Private),
        Some(Socket::Default),
        Some(Socket::Isolated(1)),
    ] {
        let mut terminal = Instance::shell(project.id);
        terminal.tmux_socket = prior;
        let mut instances = [literal.clone(), terminal.clone()];
        let own_name = session_name("isolated", terminal.id);
        let rows = [
            RemoteSession {
                name: session_name("foreign", terminal.id),
                socket: Socket::Private,
                path: "/repo".into(),
                command: "zsh".into(),
                title: String::new(),
            },
            RemoteSession {
                name: own_name.clone(),
                socket: socket.clone(),
                path: "/repo".into(),
                command: "zsh".into(),
                title: String::new(),
            },
        ];
        bind_peer_sessions(&project.name, &mut instances, &rows, socket.clone());
        assert_eq!(serde_json::to_value(&instances[0]).unwrap(), saved_literal);
        assert!(instances[1].use_tmux);
        assert_eq!(instances[1].tmux_socket, Some(socket.clone()));
        assert_eq!(
            instances[1].tmux_session.as_deref(),
            Some(own_name.as_str())
        );
    }
}

#[test]
fn linked_launch_worktree_survives_migration_and_close_cleanup_plan() {
    use std::process::Command;
    // The Nix package build runs this crate's tests in a sandbox without git.
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("skipped: git is not installed");
        return;
    }
    let root = std::env::temp_dir().join(format!("muxel-cwd-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    let linked = root.join("linked");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    git(&["init"]);
    git(&["commit", "--allow-empty", "-m", "fixture"]);
    git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
    let marker = linked.join("uncommitted.txt");
    std::fs::write(&marker, "keep this caller-owned directory").unwrap();
    let registered_before = git(&["worktree", "list", "--porcelain"]);
    let project = Project::new("repo", repo.clone());
    let instance = NewOptions {
        cwd: Some(linked.clone()),
        ..Default::default()
    }
    .instance(&project, &[], 0)
    .unwrap();
    let iid = instance.id;
    let mut ws = muxel_core::Workspace {
        projects: vec![project],
        instances: vec![instance],
        ..Default::default()
    };
    muxel_core::migrate_worktrees(&mut ws);
    assert!(ws.worktrees.is_empty());
    let cleanup = ws.instance(iid).unwrap().worktree_cleanup();
    ws.remove_instance_meta(iid);
    // This is the exact plan consumed by teardown_closed_instance. None does
    // neither registered disposal nor legacy remove_worktree/prune.
    assert_eq!(cleanup, muxel_core::WorktreeCleanup::None);
    assert!(marker.is_file());
    assert_eq!(git(&["worktree", "list", "--porcelain"]), registered_before);
    git(&["worktree", "remove", "--force", linked.to_str().unwrap()]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn literal_launch_never_injects_titles_prompts_or_resume_flags() {
    let project = Project::new("repo", std::env::current_dir().unwrap());
    let argv = words(&["codex", "brief with \"quotes\"", "", "--", "--background"]);
    let options = NewOptions {
        argv: Some(argv.clone()),
        ..NewOptions::default()
    };
    let mut instance = options.instance(&project, &[], 0).unwrap();
    // Even stale/changed preset metadata must not alter a literal launch.
    instance.system_prompt = Some("do not type this".into());
    instance.injection = muxel_core::InjectionMode::TypeIn;
    instance.auto_mode_presses = 2;
    instance.session_started = true;
    instance.session_id = Some("saved-session".into());
    let saved = serde_json::to_value(&instance).unwrap();
    let restored: Instance = serde_json::from_value(saved).unwrap();
    for resuming in [false, true] {
        let launch = resolve_launch_for_session(&restored, resuming);
        assert_eq!(launch.program.as_deref(), Some("codex"));
        assert_eq!(launch.args, argv[1..]);
        assert_eq!(launch.startup_input, None);
        assert_eq!(launch.auto_mode_presses, 0);
        assert_eq!(session_resume_args(&AgentPreset::codex(), &restored), None);
    }
    let mut legacy = serde_json::to_value(Instance::shell(project.id)).unwrap();
    legacy.as_object_mut().unwrap().remove("literal_argv");
    legacy.as_object_mut().unwrap().remove("launch_dir");
    let legacy: Instance = serde_json::from_value(legacy).unwrap();
    assert!(!legacy.literal_argv);
    assert!(legacy.launch_dir.is_none());
}

#[test]
fn new_refuses_explicit_and_default_browser_presets() {
    let project = Project::new("repo", std::env::current_dir().unwrap());
    let browser = AgentPreset {
        name: "Browser".into(),
        kind: muxel_core::PresetKind::Browser,
        ..AgentPreset::shell()
    };
    for preset in [
        None,
        Some(browser.name.clone()),
        Some(browser.id.to_string()),
    ] {
        let options = NewOptions {
            preset,
            ..NewOptions::default()
        };
        let error = options
            .instance(&project, std::slice::from_ref(&browser), 0)
            .unwrap_err();
        assert!(
            error.message.contains("terminal panes")
                && error
                    .message
                    .contains("browser presets are not supported yet")
        );
    }
}

#[test]
fn background_tab_preserves_the_selected_tab_and_splits_keep_existing_selection() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let new = Uuid::new_v4();
    let mut layout = Some(PaneNode::leaf(a));
    add_tab(&mut layout, a, b);
    assert!(place_new_pane(&mut layout, Some(a), new, None, true));
    let (_, panes) = muxel_core::control::describe_layout(layout.as_ref().unwrap());
    assert_eq!(panes[&a], panes[&new]);
    let PaneNode::Leaf(leaf) = layout.as_ref().unwrap() else {
        panic!()
    };
    assert_eq!(leaf.active_instance(), b);
    set_active_tab(&mut layout, a);
    for direction in [SplitDirection::Horizontal, SplitDirection::Vertical] {
        let mut split_layout = layout.clone();
        let other = Uuid::new_v4();
        assert!(place_new_pane(
            &mut split_layout,
            Some(b),
            other,
            Some(direction),
            true
        ));
        let PaneNode::Split {
            direction: actual,
            children,
            ..
        } = split_layout.unwrap()
        else {
            panic!()
        };
        assert_eq!(actual, direction);
        let PaneNode::Leaf(leaf) = &children[0] else {
            panic!()
        };
        assert_eq!(leaf.active_instance(), a);
    }
    let before = layout.clone();
    assert!(!place_new_pane(
        &mut layout,
        None,
        Uuid::new_v4(),
        None,
        true
    ));
    assert!(!place_new_pane(
        &mut layout,
        Some(Uuid::new_v4()),
        Uuid::new_v4(),
        None,
        true
    ));
    assert_eq!(layout, before);
    let foreground = Uuid::new_v4();
    assert!(place_new_pane(
        &mut layout,
        Some(a),
        foreground,
        None,
        false
    ));
    let PaneNode::Leaf(leaf) = layout.as_ref().unwrap() else {
        panic!()
    };
    assert_eq!(leaf.active_instance(), foreground);
    let mut empty = None;
    assert!(place_new_pane(&mut empty, None, new, None, true));
    assert_eq!(empty, Some(PaneNode::leaf(new)));
}
