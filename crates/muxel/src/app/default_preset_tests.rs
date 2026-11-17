//! The preset muxel opens with as the default is the one its preset menu
//! stars, whether the setting holds the preset's id or, as older settings
//! do, its name.

use super::MuxelApp;
use crate::theme;
use gpui::TestAppContext;
use uuid::Uuid;

#[test]
fn the_default_preset_opened_with_is_the_starred_one_by_id_or_name() {
    for by_name in [false, true] {
        let root = std::env::temp_dir().join(format!("muxel-default-preset-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let storage = muxel_store::test_support::set_root(root.clone());
        // Saved before the app loads it: built-in preset ids are minted
        // fresh until settings are written.
        let mut settings = muxel_store::load_settings();
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
        muxel_store::save_settings(&settings).unwrap();
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
        let (opened, starred) = window
            .update(&mut cx, |app, _, _| {
                (
                    app.presets[app.current_preset].id,
                    app.active_default_preset_id(),
                )
            })
            .unwrap();
        assert_eq!(opened, codex, "opens with Codex, by name: {by_name}");
        assert_eq!(starred, Some(codex), "stars Codex, by name: {by_name}");
        // The app and its tasks go first, then its files, then the lease.
        drop(cx);
        std::fs::remove_dir_all(root).unwrap();
        drop(storage);
    }
}
