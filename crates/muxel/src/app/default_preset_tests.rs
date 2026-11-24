//! Preset selection, editing and direct local launches.

use super::MuxelApp;
use crate::theme;
use gpui::TestAppContext;
use muxel_core::{AgentPreset, Instance, Project, Settings};
use uuid::Uuid;

fn with_app<R>(settings: &Settings, run: impl FnOnce(&mut MuxelApp) -> R) -> R {
    let root = std::env::temp_dir().join(format!("muxel-default-preset-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let storage = muxel_store::test_support::set_root(root.clone());
    muxel_store::save_settings(settings).unwrap();
    let mut cx = TestAppContext::single();
    cx.skip_drawing();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let window = cx.add_window(MuxelApp::new);
    let result = window.update(&mut cx, |app, _, _| run(app)).unwrap();
    // The app and its tasks go first, then its files, then the lease.
    drop(cx);
    std::fs::remove_dir_all(root).unwrap();
    drop(storage);
    result
}

#[test]
fn the_default_preset_opened_with_is_the_starred_one_by_id_or_name() {
    for by_name in [false, true] {
        // Saved before the app loads it: built-in preset ids are minted
        // fresh until settings are written.
        let mut settings = Settings::default();
        let codex = settings
            .presets
            .iter()
            .find(|p| p.name == "Codex")
            .expect("the built-in Codex preset")
            .id;
        settings.default_preset = if by_name {
            "Codex".to_string()
        } else {
            codex.to_string()
        };
        let (opened, starred) = with_app(&settings, |app| {
            (
                app.presets[app.current_preset].id,
                app.active_default_preset_id(),
            )
        });
        assert_eq!(opened, codex, "opens with Codex, by name: {by_name}");
        assert_eq!(starred, Some(codex), "stars Codex, by name: {by_name}");
    }
}

#[cfg(windows)]
#[test]
fn windows_preset_arguments_survive_save_reopen_and_launch() {
    let cases: &[(&str, &[&str])] = &[
        (
            r"-NoExit -File C:\profiles\default\profile.ps1",
            &["-NoExit", "-File", r"C:\profiles\default\profile.ps1"],
        ),
        (
            r#"-NoLogo -NoExit -Command "& 'C:\tools\init.ps1'""#,
            &["-NoLogo", "-NoExit", "-Command", r"& 'C:\tools\init.ps1'"],
        ),
        (
            r#"-File "C:\Program Files\tools\init.ps1""#,
            &["-File", r"C:\Program Files\tools\init.ps1"],
        ),
        (
            r"-File \\server\share\init.ps1",
            &["-File", r"\\server\share\init.ps1"],
        ),
        (
            "-ExecutionPolicy Bypass -NoProfile",
            &["-ExecutionPolicy", "Bypass", "-NoProfile"],
        ),
    ];
    super::codex_capture_tests::with_app(|cx, window| {
        let differences = window
            .update(cx, |app, window, cx| {
                let cwd = muxel_store::data_dir().unwrap();
                let project = app.workspace.add_project(Project::new("fixture", cwd));
                let mut preset = AgentPreset::shell();
                preset.program = Some("powershell.exe".into());
                let id = preset.id;
                let index = app.presets.len();
                app.presets.push(preset);
                let mut differences = Vec::new();
                for &(typed, expected) in cases {
                    app.open_preset_editor(index, window, cx);
                    app.settings_ui
                        .p_args
                        .update(cx, |s, cx| s.set_value(typed, window, cx));
                    app.save_preset(cx);
                    let saved = muxel_store::try_load_settings()
                        .unwrap()
                        .unwrap()
                        .presets
                        .into_iter()
                        .find(|p| p.id == id)
                        .unwrap();
                    let instance = Instance::from_preset(project, &saved);
                    let iid = instance.id;
                    app.workspace.add_instance(instance);
                    let (spec, _) = app.command_for(iid, None).unwrap();
                    app.presets[index] = saved;
                    app.open_preset_editor(index, window, cx);
                    let stored = &app.presets[index].args;
                    let reopened = app.settings_ui.p_args.read(cx).value().to_string();
                    if stored != expected || reopened != typed || spec.args != expected {
                        differences.push(format!(
                            "typed {typed}\nstored {stored:?}\nreopened {reopened}\nlaunch {:?}",
                            spec.args
                        ));
                    }
                }
                differences
            })
            .unwrap();
        assert!(differences.is_empty(), "{}", differences.join("\n\n"));
    });
}

fn add_shell_copy(app: &mut MuxelApp, mut preset: AgentPreset) -> Uuid {
    preset.name = "Shell copy".into();
    preset.program = None;
    let project = app
        .workspace
        .add_project(Project::new("fixture", muxel_store::data_dir().unwrap()));
    let instance = Instance::from_preset(project, &preset);
    let iid = instance.id;
    app.presets.push(preset);
    app.workspace.add_instance(instance);
    iid
}

#[test]
fn default_shell_launch_receives_preset_arguments() {
    let args = with_app(&Settings::default(), |app| {
        let preset = AgentPreset {
            model: Some("small".into()),
            effort: Some("low".into()),
            args: vec!["-NoLogo".into(), "-NoProfile".into()],
            ..AgentPreset::claude()
        };
        let iid = add_shell_copy(app, preset);
        app.command_for(iid, None).unwrap().0.args
    });
    assert_eq!(
        args,
        [
            "--model",
            "small",
            "--effort",
            "low",
            "-NoLogo",
            "-NoProfile"
        ]
    );
}

#[test]
fn cleared_agent_program_opens_a_plain_shell_on_launch_and_restore() {
    let (fresh, restored) = with_app(&Settings::default(), |app| {
        let iid = add_shell_copy(app, AgentPreset::claude());
        let fresh = app.command_for(iid, None).unwrap().0.args;
        let pane = app.workspace.instance_mut(iid).unwrap();
        pane.session_started = true;
        pane.session_id = Some(Uuid::new_v4().to_string());
        muxel_store::save_workspace(&app.workspace).unwrap();
        app.workspace = muxel_store::load_workspace().unwrap();
        // Later preset edits belong to future panes.
        app.presets
            .last_mut()
            .unwrap()
            .args
            .push("-NoProfile".into());
        (fresh, app.command_for(iid, None).unwrap().0.args)
    });
    assert_eq!(
        (fresh, restored),
        (Vec::<String>::new(), Vec::<String>::new())
    );
}
