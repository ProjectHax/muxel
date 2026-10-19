use super::MuxelApp;
use super::session_discovery::{inject_codex_launch_lookup, inject_codex_resume_lookup};
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::{AgentPreset, Instance, Project, RemoteRef};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>)) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.wip/codex-capture-tests")
        .join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let _storage = muxel_store::test_support::set_root(root.clone());
    assert_eq!(muxel_store::data_dir(), Some(root.join("data")));
    assert_eq!(muxel_store::config_dir(), Some(root.join("config")));
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
    run(&mut cx, window);
}

fn codex_pane(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Uuid {
    window
        .update(cx, |app, _, _| {
            let cwd = muxel_store::data_dir().unwrap().join("project");
            std::fs::create_dir_all(&cwd).unwrap();
            let project = app.workspace.add_project(Project::new("capture", cwd));
            let preset = AgentPreset::codex();
            let mut instance = Instance::from_preset(project, &preset);
            instance.session_started = true;
            let pane = instance.id;
            app.presets.push(preset);
            app.workspace.add_instance(instance);
            app.current_workspace.get_or_insert_with(Uuid::new_v4);
            app.use_tmux = false;
            app.use_worktree = false;
            pane
        })
        .unwrap()
}

#[test]
fn codex_launch_preparation_does_not_wait_for_slow_discovery() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let _lookup = inject_codex_launch_lookup(Arc::new(move |_, _| {
            observed.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(350));
            Some("01a0e807-a64d-7502-b922-cff0e0000000".into())
        }));
        let started = Instant::now();
        window
            .update(cx, |app, window, cx| {
                let (meta, spec, _) = app.prepare_terminal_spawn(pane, window, cx).unwrap();
                assert!(!meta.was_resume);
                assert!(!spec.args.iter().any(|arg| arg == "resume"));
                assert!(app.workspace.instance(pane).unwrap().session_id.is_none());
            })
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "UI launch preparation blocked for {:?}",
            started.elapsed()
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "restore enumerated session history"
        );
    });
}

#[test]
fn remote_codex_resume_does_not_touch_the_local_session_store() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let _lookup = inject_codex_launch_lookup(Arc::new(|_, _| panic!("remote history scan")));
        let _stat =
            inject_codex_resume_lookup(Arc::new(|_, _, _| panic!("remote local-store stat")));
        let id = "01a1543c-9700-7000-8000-000000000001";
        window
            .update(cx, |app, _, _| {
                let instance = app.workspace.instance_mut(pane).unwrap();
                instance.session_id = Some(id.into());
                let project = instance.project_id;
                app.workspace.project_mut(project).unwrap().remote = Some(RemoteRef {
                    host_id: Uuid::new_v4(),
                    remote_root: "/remote/project".into(),
                });
                let (args, _) = app.session_resume_for(pane).unwrap();
                assert_eq!(args, vec!["resume", id]);
                assert_eq!(
                    app.workspace.instance(pane).unwrap().session_id.as_deref(),
                    Some(id)
                );
            })
            .unwrap();
    });
}

#[test]
fn exact_codex_identity_is_resumed_and_its_path_is_persisted() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let path = muxel_store::data_dir()
            .unwrap()
            .join(format!("rollout-{id}.jsonl"));
        let expected = path.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let _lookup =
            inject_codex_launch_lookup(Arc::new(|_, _| panic!("exact identity used latest")));
        let _stat = inject_codex_resume_lookup(Arc::new(move |_, saved, _| {
            assert_eq!(saved, id);
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(Some(expected.clone()))
        }));
        window
            .update(cx, |app, _, _| {
                app.workspace.instance_mut(pane).unwrap().session_id = Some(id.into());
                let (args, _) = app.session_resume_for(pane).unwrap();
                assert_eq!(args, vec!["resume", id]);
                assert_eq!(
                    app.workspace.instance(pane).unwrap().session_path.as_ref(),
                    Some(&path)
                );
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let document = window
            .update(cx, |app, _, _| {
                muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap()
            })
            .unwrap();
        let saved = muxel_store::load_workspace_from(&document).unwrap();
        assert_eq!(
            saved.instance(pane).unwrap().session_path.as_ref(),
            Some(&path)
        );
    });
}

#[test]
fn codex_store_error_keeps_the_saved_resume_identity() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let _stat = inject_codex_resume_lookup(Arc::new(|_, _, _| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }));
        let id = "01a1543c-9700-7000-8000-000000000001";
        window
            .update(cx, |app, _, _| {
                app.workspace.instance_mut(pane).unwrap().session_id = Some(id.into());
                let (args, _) = app.session_resume_for(pane).unwrap();
                assert_eq!(args, vec!["resume", id]);
                assert!(app.workspace.instance(pane).unwrap().session_started);
            })
            .unwrap();
    });
}
