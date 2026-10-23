use super::MuxelApp;
use super::session_discovery::{
    CodexRestoreFound, CodexRestoreKind, inject_codex_launch_lookup, inject_codex_restore_lookup,
    inject_codex_resume_lookup,
};
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::{AgentPreset, CodexResumePath, CodexSession, Instance, Project, RemoteRef};
use muxel_terminal::{CommandSpec, TerminalLaunch};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::time::Instant;
use uuid::Uuid;

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>)) {
    let root = std::env::temp_dir().join(format!("muxel-codex-capture-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let _temp = TempRoot(root.clone());
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

/// Launch preparation itself never searches history on the UI thread. The
/// one restore-time lookup runs on a worker beforehand (next test); without
/// it, an unbound started pane starts fresh.
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
        let restore_calls = Arc::new(AtomicUsize::new(0));
        let restore_observed = restore_calls.clone();
        let _restore = inject_codex_restore_lookup(Arc::new(move |_| {
            restore_observed.fetch_add(1, Ordering::Relaxed);
            CodexRestoreFound::Skip
        }));
        window
            .update(cx, |app, window, cx| {
                let (meta, spec, _) = app.prepare_terminal_spawn(pane, window, cx).unwrap();
                assert!(!meta.was_resume);
                assert!(!spec.args.iter().any(|arg| arg == "resume"));
                assert!(app.workspace.instance(pane).unwrap().session_id.is_none());
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "restore enumerated session history"
        );
        assert_eq!(
            restore_calls.load(Ordering::Relaxed),
            0,
            "preparation started a history lookup"
        );
    });
}

#[test]
fn unbound_codex_restore_lookup_runs_on_a_worker_once_and_binds_before_launch() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let path = muxel_store::data_dir()
            .unwrap()
            .join(format!("rollout-{id}.jsonl"));
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let candidate = CodexSession {
            id: id.into(),
            path: path.clone(),
        };
        let _restore = inject_codex_restore_lookup(Arc::new(move |query| {
            assert!(matches!(
                query.kind,
                CodexRestoreKind::Latest { limit: 1, .. }
            ));
            observed.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(350));
            CodexRestoreFound::Latest(vec![candidate.clone()])
        }));
        let found = path.clone();
        let _stat = inject_codex_resume_lookup(Arc::new(move |_, saved, _| {
            assert_eq!(saved, id);
            Ok(CodexResumePath::Found(found.clone()))
        }));
        window
            .update(cx, |app, _, cx| app.begin_codex_restore_lookup(pane, cx))
            .unwrap();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "the lookup ran on the UI thread"
        );
        cx.run_until_parked();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        window
            .update(cx, |app, window, cx| {
                let instance = app.workspace.instance(pane).unwrap();
                assert_eq!(instance.session_id.as_deref(), Some(id));
                assert_eq!(instance.session_path.as_ref(), Some(&path));
                let (meta, spec, _) = app.prepare_terminal_spawn(pane, window, cx).unwrap();
                assert!(meta.was_resume);
                assert!(spec.args.iter().any(|arg| arg == id));
                // One lookup per pane in a run, even if it would qualify again.
                app.terminal_launching.remove(&pane);
                app.workspace.instance_mut(pane).unwrap().session_path = None;
                app.begin_codex_restore_lookup(pane, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn codex_restore_lookup_landing_after_launch_preparation_is_dropped() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let _restore = inject_codex_restore_lookup(Arc::new(|_| {
            CodexRestoreFound::Latest(vec![CodexSession {
                id: "01a1543c-9700-7000-8000-000000000001".into(),
                path: "rollout.jsonl".into(),
            }])
        }));
        window
            .update(cx, |app, window, cx| {
                app.begin_codex_restore_lookup(pane, cx);
                // An immediate launch went ahead before the worker landed.
                let (meta, _, _) = app.prepare_terminal_spawn(pane, window, cx).unwrap();
                assert!(!meta.was_resume);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |app, _, _| {
                let instance = app.workspace.instance(pane).unwrap();
                assert!(instance.session_id.is_none());
                assert!(instance.session_path.is_none());
            })
            .unwrap();
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
                let (args, _) = app.session_resume_for(pane).unwrap().unwrap();
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
            Ok(CodexResumePath::Found(expected.clone()))
        }));
        window
            .update(cx, |app, _, _| {
                app.workspace.instance_mut(pane).unwrap().session_id = Some(id.into());
                let (args, _) = app.session_resume_for(pane).unwrap().unwrap();
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
                let (args, _) = app.session_resume_for(pane).unwrap().unwrap();
                assert_eq!(args, vec!["resume", id]);
                assert!(app.workspace.instance(pane).unwrap().session_started);
            })
            .unwrap();
    });
}

#[test]
fn inconclusive_codex_resume_survives_unrelated_early_exit_recovery() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let _lookup = inject_codex_resume_lookup(Arc::new(|_, _, _| Ok(CodexResumePath::Unknown)));
        let document = window
            .update(cx, |app, _, _| {
                app.workspace.instance_mut(pane).unwrap().session_id = Some(id.into());
                let (args, _) = app.session_resume_for(pane).unwrap().unwrap();
                assert_eq!(args, vec!["resume", id]);
                // This is the decision used by tick after a direct resume exits
                // nonzero within ten seconds for an unrelated reason.
                assert!(!app.should_recover_expired_resume(pane, false, true));
                assert!(app.should_recover_expired_resume(pane, true, false));
                app.persist();
                muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap()
            })
            .unwrap();
        let saved = muxel_store::load_workspace_from(&document).unwrap();
        assert_eq!(
            saved.instance(pane).unwrap().session_id.as_deref(),
            Some(id)
        );
    });
}

#[test]
fn exited_codex_terminal_keeps_inconclusive_resume_through_tick() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let _lookup = inject_codex_resume_lookup(Arc::new(|_, _, _| Ok(CodexResumePath::Unknown)));
        let document = window
            .update(cx, |app, window, cx| {
                app.workspace.instance_mut(pane).unwrap().session_id = Some(id.into());
                let (meta, spec, size) = app.prepare_terminal_spawn(pane, window, cx).unwrap();
                assert!(meta.was_resume);
                assert!(spec.args.iter().any(|arg| arg == "resume"));
                #[cfg(windows)]
                let command = CommandSpec::program("cmd.exe", vec!["/C".into(), "exit 37".into()]);
                #[cfg(not(windows))]
                let command = CommandSpec::program("/bin/sh", vec!["-c".into(), "exit 37".into()]);
                let launch = TerminalLaunch::spawn_for_instance(command, size, pane).unwrap();
                app.finish_terminal_spawn(meta, Ok(launch), window, cx);
                muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap()
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            let exited = window
                .update(cx, |app, _, cx| {
                    app.terminals.get(&pane).unwrap().read(cx).exited()
                })
                .unwrap();
            if exited {
                break;
            }
            assert!(Instant::now() < deadline, "test child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        window
            .update(cx, |app, window, cx| app.tick(window, cx))
            .unwrap();
        let saved = muxel_store::load_workspace_from(&document).unwrap();
        assert_eq!(
            saved.instance(pane).unwrap().session_id.as_deref(),
            Some(id)
        );
        window
            .update(cx, |app, _, cx| {
                let view = app.terminals.get(&pane).unwrap().read(cx);
                assert!(view.exited());
                assert_eq!(view.exit_code(), Some(37));
                assert_eq!(
                    app.workspace.instance(pane).unwrap().session_id.as_deref(),
                    Some(id)
                );
            })
            .unwrap();
    });
}

#[test]
fn inconclusive_codex_lookups_keep_the_saved_resume_identity() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let root = muxel_store::data_dir().unwrap().join("codex-home");
        let v7 = "01a1543c-9700-7000-8000-000000000001";
        let old = Uuid::new_v4().to_string();
        let wrong_day = root.join(".codex/sessions/2020/01/01");
        std::fs::create_dir_all(&wrong_day).unwrap();
        std::fs::write(wrong_day.join(format!("rollout-{v7}.jsonl")), "{}").unwrap();
        let lookup_root = root.clone();
        let _lookup = inject_codex_resume_lookup(Arc::new(move |_, id, saved| {
            muxel_core::codex_resume_path(&lookup_root, id, saved)
        }));
        let invalid_path = root.join("outside").join(format!("rollout-{v7}.jsonl"));
        for (id, path) in [(old.as_str(), None), (v7, Some(invalid_path)), (v7, None)] {
            window
                .update(cx, |app, _, _| {
                    let instance = app.workspace.instance_mut(pane).unwrap();
                    instance.session_id = Some(id.into());
                    instance.session_path = path.clone();
                    instance.session_started = true;
                    let (args, _) = app.session_resume_for(pane).unwrap().unwrap();
                    assert_eq!(args, vec!["resume", id]);
                    assert_eq!(
                        app.workspace.instance(pane).unwrap().session_id.as_deref(),
                        Some(id)
                    );
                })
                .unwrap();
        }
    });
}

#[test]
fn disappeared_saved_codex_path_clears_the_binding() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let root = muxel_store::data_dir().unwrap().join("codex-home");
        let id = "01a1543c-9700-7000-8000-000000000001";
        let path = root
            .join(".codex/sessions/2026/10/19")
            .join(format!("rollout-{id}.jsonl"));
        let lookup_root = root.clone();
        let _lookup = inject_codex_resume_lookup(Arc::new(move |_, id, saved| {
            muxel_core::codex_resume_path(&lookup_root, id, saved)
        }));
        window
            .update(cx, |app, _, _| {
                let instance = app.workspace.instance_mut(pane).unwrap();
                instance.session_id = Some(id.into());
                instance.session_path = Some(path);
                instance.session_started = true;
                assert!(app.session_resume_for(pane).unwrap().is_none());
                let instance = app.workspace.instance(pane).unwrap();
                assert!(instance.session_id.is_none());
                assert!(!instance.session_started);
            })
            .unwrap();
    });
}
