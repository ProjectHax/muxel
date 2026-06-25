//! muxel — a multi-agent terminal multiplexer built on GPUI.
//!
//! See [`app::MuxelApp`] for the application shell.

// On Windows, release builds use the GUI subsystem so launching muxel doesn't
// pop a console/cmd window alongside the app. Debug builds keep the console so
// logs stay visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod editor;
mod filetree;
mod integrations;
mod secrets;
mod settings_view;
mod theme;
mod update;

use app::MuxelApp;
use gpui::*;
use gpui_component::{Root, TitleBar, *};
use std::borrow::Cow;

/// muxel's own bundled SVG assets: agent logos under `icons/agent-*.svg`, plus
/// the app icon `muxel.svg` (shown in the welcome dialog).
#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
#[include = "muxel.svg"]
struct MuxelIcons;

/// Asset source that serves muxel's icons first, then falls back to
/// gpui-component's bundled icon set.
struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(file) = MuxelIcons::get(path) {
            return Ok(Some(file.data));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out = gpui_component_assets::Assets.list(path)?;
        out.extend(MuxelIcons::iter().filter_map(|p| p.starts_with(path).then(|| p.into())));
        Ok(out)
    }
}

fn main() {
    // A macOS Dock/Finder launch inherits a minimal launchd PATH that omits
    // Homebrew and ~/.local/bin, so installed agents would be hidden from the
    // picker and fail to spawn (and the PTY children inherit this env too).
    // Reconstruct the common dirs before any threads start — env::set_var must
    // run while the process is still single-threaded.
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").ok();
        let current = std::env::var("PATH").ok();
        if let Some(path) = muxel_core::augmented_macos_path(current.as_deref(), home.as_deref()) {
            // SAFETY: first statement in main, before any thread is spawned.
            unsafe { std::env::set_var("PATH", path) };
        }
    }

    gpui_platform::application()
        // Serves muxel's agent icons + gpui-component's bundled SVG icons.
        .with_assets(AppAssets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::register_bundled_themes(cx);
            app::register_actions(cx);

            let settings = muxel_store::load_settings();
            cx.set_global(theme::UiScale(settings.zoom));
            cx.set_global(theme::UiFontSize(settings.ui_font_size));
            theme::apply_initial_theme(&settings.theme, cx);
            app::install_keybindings(&settings, cx);

            let window_bounds = muxel_store::load_window_geom().and_then(|g| {
                if g.width > 0.0 && g.height > 0.0 {
                    let bounds = Bounds {
                        origin: point(px(g.x), px(g.y)),
                        size: size(px(g.width), px(g.height)),
                    };
                    Some(if g.maximized {
                        WindowBounds::Maximized(bounds)
                    } else {
                        WindowBounds::Windowed(bounds)
                    })
                } else {
                    None
                }
            });

            cx.spawn(async move |cx| {
                let options = WindowOptions {
                    titlebar: Some(TitleBar::title_bar_options()),
                    window_bounds,
                    // Matches the .desktop StartupWMClass so the desktop ties the
                    // window (and its notifications) to muxel's icon.
                    app_id: Some("muxel".to_string()),
                    ..Default::default()
                };
                cx.open_window(options, |window, cx| {
                    // Give the window an explicit title; without it the compositor
                    // shows "Unknown" in the title bar / window switcher.
                    window.set_window_title("muxel");
                    let view = cx.new(|cx| MuxelApp::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
                })
                .expect("failed to open muxel window");
            })
            .detach();
        });
}
