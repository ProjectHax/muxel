//! A shell draws its first prompt before muxel paints its terminal (the pane
//! shows "Starting terminal…" until then), so the PTY has to open at the size
//! its pane is laid out at. A prompt drawn at any other width is reflowed when
//! the terminal resizes, and PowerShell's line editor then draws the first
//! command over the prompt.
//!
//! Each case starts a child that reports its console size as it starts, then
//! compares that with the size the terminal settles at once it paints.

use super::{MuxelApp, PlacementMode, WorktreeChoice};
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::{Instance, PaneNode, Project, SplitDirection};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use uuid::Uuid;

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The app in a window that draws: layout is what these tests are about.
fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>)) {
    let root = std::env::temp_dir().join(format!("muxel-first-prompt-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    // Declared first, dropped last: the lease is held until the root is gone,
    // so the next app test can't start while it is still being removed.
    let _storage = muxel_store::test_support::set_root(root.clone());
    let _temp = TempRoot(root.clone());
    let mut cx = TestAppContext::single();
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

/// A pane whose program prints the console size it starts with, then stays.
fn size_reporter(pid: Uuid) -> Instance {
    let mut instance = Instance::shell(pid);
    instance.preset = String::new();
    instance.preset_id = None;
    instance.program = Some("cmd.exe".into());
    instance.args = vec!["/d".into(), "/k".into(), "mode con".into()];
    instance
}

/// Columns the child reported at startup: `mode con` prints
/// "    Columns:        120".
fn reported_cols(text: &str) -> Option<u16> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("Columns:"))
        .and_then(|cols| cols.trim().parse().ok())
}

fn project(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Uuid {
    window
        .update(cx, |app, _, cx| {
            let cwd = muxel_store::data_dir().unwrap().join("project");
            std::fs::create_dir_all(&cwd).unwrap();
            let pid = app.workspace.add_project(Project::new("first-prompt", cwd));
            app.workspace.active_project = Some(pid);
            app.current_workspace.get_or_insert_with(Uuid::new_v4);
            // Past the first-run screens, which replace the panes.
            app.show_terms = false;
            app.show_workspace_selector = false;
            app.use_tmux = false;
            app.use_worktree = false;
            cx.notify();
            pid
        })
        .unwrap()
}

/// Run the app until `pane`'s child has reported its size and its terminal has
/// painted and settled; returns (reported, settled) columns.
fn first_and_settled_cols(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
) -> (u16, u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let read = |cx: &mut TestAppContext| {
        window
            .update(cx, |app, _, cx| {
                app.terminals.get(&pane).map(|view| {
                    let view = view.read(cx);
                    (
                        reported_cols(&view.recent_text(60)),
                        view.has_visible_content(),
                        view.session().size().0,
                    )
                })
            })
            .unwrap()
    };
    let mut reported = None;
    let mut settled: Option<(u16, u32)> = None;
    loop {
        assert!(
            Instant::now() < deadline,
            "pane {pane} never reported and settled: {:?}",
            read(cx)
        );
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        // Paint again after the settle debounce, which runs on the real clock.
        std::thread::sleep(Duration::from_millis(80));
        cx.update(|cx| cx.refresh_windows());
        cx.run_until_parked();
        let Some((cols, visible, size)) = read(cx) else {
            continue;
        };
        if reported.is_none() {
            reported = cols;
        }
        if reported.is_none() || !visible {
            continue;
        }
        // Settled once the applied size holds across several paints.
        settled = match settled {
            Some((last, stable)) if last == size => Some((size, stable + 1)),
            _ => Some((size, 0)),
        };
        if let Some((size, stable)) = settled
            && stable >= 3
        {
            return (reported.unwrap(), size);
        }
    }
}

fn assert_first_size_is_final(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
    how: &str,
) {
    let (reported, settled) = first_and_settled_cols(cx, window, pane);
    assert_ne!(
        settled, 80,
        "{how}: the pane settles at the 80-column default, so this case proves nothing"
    );
    assert_eq!(
        reported, settled,
        "{how}: the child started at {reported} columns, then its pane settled at {settled}"
    );
}

#[test]
fn a_new_pane_starts_at_the_size_its_pane_paints() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let first = window
            .update(cx, |app, window, cx| {
                let pane = size_reporter(pid);
                let id = pane.id;
                app.place_and_spawn(
                    pid,
                    pane,
                    PlacementMode::Tab,
                    None,
                    Some(WorktreeChoice::None),
                    window,
                    cx,
                );
                id
            })
            .unwrap();
        assert_first_size_is_final(cx, window, first, "first pane of a project");

        let split = window
            .update(cx, |app, window, cx| {
                let pane = size_reporter(pid);
                let id = pane.id;
                app.place_and_spawn(
                    pid,
                    pane,
                    PlacementMode::Split(SplitDirection::Horizontal),
                    Some(first),
                    Some(WorktreeChoice::None),
                    window,
                    cx,
                );
                id
            })
            .unwrap();
        assert_first_size_is_final(cx, window, split, "split right");
    });
}

/// Columns `pane`'s child reported at startup, once it has.
fn reported_cols_of(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, pane: Uuid) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "pane {pane} never reported");
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        let cols = window
            .update(cx, |app, _, cx| {
                app.terminals
                    .get(&pane)
                    .and_then(|view| reported_cols(&view.read(cx).recent_text(60)))
            })
            .unwrap();
        if let Some(cols) = cols {
            return cols;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_tab_opened_into_a_project_off_screen_starts_at_its_panes_size() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let first = window
            .update(cx, |app, window, cx| {
                let pane = size_reporter(pid);
                let id = pane.id;
                app.place_and_spawn(
                    pid,
                    pane,
                    PlacementMode::Tab,
                    None,
                    Some(WorktreeChoice::None),
                    window,
                    cx,
                );
                id
            })
            .unwrap();
        let (_, width) = first_and_settled_cols(cx, window, first);
        assert_ne!(
            width, 80,
            "the pane is 80 columns wide, so this proves nothing"
        );

        // Show another project, then open a tab beside the first pane, the way
        // `muxel ctl new` does by default.
        project(cx, window);
        cx.run_until_parked();
        let tab = window
            .update(cx, |app, window, cx| {
                let pane = size_reporter(pid);
                let id = pane.id;
                app.place_and_spawn_with_focus(
                    pid,
                    pane,
                    PlacementMode::Tab,
                    Some(first),
                    Some(WorktreeChoice::None),
                    false,
                    window,
                    cx,
                );
                id
            })
            .unwrap();
        assert_eq!(
            reported_cols_of(cx, window, tab),
            width,
            "a tab opened off screen should start at its pane's width"
        );
    });
}

#[test]
fn a_restored_pane_starts_at_its_laid_out_size_not_its_saved_grid() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let pane = window
            .update(cx, |app, window, cx| {
                let mut pane = size_reporter(pid);
                // Saved in a smaller window than the one it reopens in.
                pane.grid = Some((40, 10));
                let id = pane.id;
                app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(id));
                app.workspace.add_instance(pane);
                app.active_instance = Some(id);
                app.ensure_project_terminals_deferred(pid, window, cx);
                id
            })
            .unwrap();
        assert_first_size_is_final(cx, window, pane, "restored pane with a stale grid");
    });
}
