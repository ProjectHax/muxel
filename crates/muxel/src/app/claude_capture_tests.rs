use super::CLAUDE_LAUNCH_TEST_LOOKUP;
use super::codex_capture_tests::with_app;
use muxel_core::{AgentPreset, Instance, Project};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct ProbeGuard;
impl Drop for ProbeGuard {
    fn drop(&mut self) {
        *CLAUDE_LAUNCH_TEST_LOOKUP.lock().unwrap() = None;
    }
}

#[test]
fn claude_launch_preparation_does_not_wait_for_transcript_discovery() {
    with_app(|cx, window| {
        let pane = window
            .update(cx, |app, _, _| {
                let cwd = muxel_store::data_dir().unwrap().join("project");
                std::fs::create_dir_all(&cwd).unwrap();
                let project = app
                    .workspace
                    .add_project(Project::new("Claude lookup", cwd));
                let preset = AgentPreset::claude();
                let mut instance = Instance::from_preset(project, &preset);
                instance.session_started = true;
                instance.session_id = Some(uuid::Uuid::new_v4().to_string());
                let pane = instance.id;
                app.presets.push(preset);
                app.workspace.add_instance(instance);
                pane
            })
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        *CLAUDE_LAUNCH_TEST_LOOKUP.lock().unwrap() = Some(Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(350));
        }));
        let _guard = ProbeGuard;
        let started = Instant::now();
        window
            .update(cx, |app, window, cx| {
                assert!(app.prepare_terminal_spawn(pane, window, cx).is_some());
            })
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "UI Claude preparation blocked for {:?}",
            started.elapsed()
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "UI probed the transcript store"
        );
    });
}
