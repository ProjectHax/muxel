//! A NOTIFICATIONS row cuts its title and subtitle to one line each. Hovering
//! the row shows both in full, in a tooltip.
//! It skips macOS, where `Root` needs the native window a test window lacks.

use super::{MuxelApp, NotifKind};
use crate::theme;
use gpui::{AppContext as _, Modifiers, MouseButton, TestAppContext, VisualTestContext};
use gpui_component::Root;
use std::time::Duration;
use uuid::Uuid;

const TITLE: &str = "Push failed";
const SUBTITLE: &str = "The remote rejected the update because its branch has moved on since \
     the last fetch. Fetch, rebase onto it, and push again.";

#[test]
fn hovering_a_notification_shows_its_full_text() {
    let root = std::env::temp_dir().join(format!("muxel-notification-feed-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let storage = muxel_store::test_support::set_root(root.clone());
    let mut cx = TestAppContext::single();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let mut app = None;
    let window = cx.add_window(|window, cx| {
        let view = cx.new(|cx| MuxelApp::new(window, cx));
        app = Some(view.clone());
        Root::new(view, window, cx)
    });
    let app = app.unwrap();
    let mut visual = VisualTestContext::from_window(window.into(), &cx);
    let nid = visual.update(|_, cx| {
        app.update(cx, |app, cx| {
            // Advancing the clock past the tooltip's delay would run these too.
            app._status_timer = gpui::Task::ready(());
            app._update_timer = gpui::Task::ready(());
            // Past first run: the terms and workspace screens cover the sidebar.
            app.show_terms = false;
            app.show_workspace_selector = false;
            app.current_workspace = Some(Uuid::new_v4());
            app.add_event(NotifKind::Error, TITLE, SUBTITLE);
            cx.notify();
            app.notifications.last().unwrap().id
        })
    });
    let draw = |visual: &mut VisualTestContext| {
        visual.run_until_parked();
        visual.update(|window, cx| {
            let _ = window.draw(cx);
        });
    };
    draw(&mut visual);
    let tooltip: &'static str = format!("notification tooltip: {TITLE}\n{SUBTITLE}").leak();
    assert!(
        visual.debug_bounds(tooltip).is_none(),
        "the full text shows before the row is hovered"
    );
    let row = visual
        .debug_bounds(format!("notification {}", nid.simple()).leak())
        .expect("the notification's row is drawn");

    visual.simulate_mouse_move(row.center(), None::<MouseButton>, Modifiers::default());
    draw(&mut visual);
    visual.executor().advance_clock(Duration::from_secs(1));
    draw(&mut visual);
    assert!(
        visual.debug_bounds(tooltip).is_some(),
        "hovering the row didn't show its full title and subtitle"
    );

    drop(visual);
    drop(app);
    drop(cx);
    drop(storage);
    std::fs::remove_dir_all(root).unwrap();
}
