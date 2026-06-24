//! Theme registration + helpers: load the bundled themes, apply a saved theme,
//! switch themes/mode at runtime, and derive a terminal color palette from the
//! active gpui-component theme.

use gpui::{Action, App, Global, Hsla, Rgba, SharedString, px};
use gpui_component::{ActiveTheme, Theme, ThemeRegistry};
use muxel_terminal::TerminalPalette;
use rust_embed::RustEmbed;

/// Fallback interface font size (gpui-component's `Root` drives the window
/// `rem_size` from `theme.font_size`, so this sizes all non-terminal UI text +
/// spacing). Overridable per-user via [`UiFontSize`].
const DEFAULT_UI_FONT_SIZE: f32 = 16.0;

/// Global UI scale (zoom) factor for the whole app.
pub struct UiScale(pub f32);
impl Global for UiScale {}

/// Interface (non-terminal) base font size. Independent of the terminal font.
pub struct UiFontSize(pub f32);
impl Global for UiFontSize {}

fn ui_scale(cx: &App) -> f32 {
    cx.try_global::<UiScale>().map(|s| s.0).unwrap_or(1.0)
}

fn ui_font_size(cx: &App) -> f32 {
    cx.try_global::<UiFontSize>()
        .map(|s| s.0)
        .unwrap_or(DEFAULT_UI_FONT_SIZE)
}

/// Re-apply the interface font size × zoom to the active theme's font size.
/// Called after every theme/mode change (which resets `font_size`) and whenever
/// the UI font size or zoom changes. The terminal font is sized separately.
fn apply_scale(cx: &mut App) {
    let size = ui_font_size(cx) * ui_scale(cx);
    Theme::global_mut(cx).font_size = px(size);
}

/// Set the whole-app UI scale (zoom) and refresh.
pub fn set_ui_scale(scale: f32, cx: &mut App) {
    cx.set_global(UiScale(scale));
    apply_scale(cx);
    cx.refresh_windows();
}

/// Set the interface (non-terminal) base font size and refresh.
pub fn set_ui_font_size(size: f32, cx: &mut App) {
    cx.set_global(UiFontSize(size));
    apply_scale(cx);
    cx.refresh_windows();
}

/// Dispatched by the theme-switcher menu items; applies the named theme.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = muxel, no_json)]
pub struct SwitchTheme(pub SharedString);

/// The bundled theme JSON files (vendored from gpui-component's `themes/`).
#[derive(RustEmbed)]
#[folder = "assets/themes"]
struct ThemeAssets;

/// Load all bundled theme sets into the global registry.
pub fn register_bundled_themes(cx: &mut App) {
    let mut loaded = 0;
    for file in ThemeAssets::iter() {
        let Some(embedded) = ThemeAssets::get(&file) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&embedded.data) else {
            continue;
        };
        match ThemeRegistry::global_mut(cx).load_themes_from_str(text) {
            Ok(()) => loaded += 1,
            Err(e) => log::warn!("failed to load theme {file}: {e}"),
        }
    }
    log::info!("loaded {loaded} bundled theme file(s)");
}

/// All available theme names, sorted for display in the switcher.
pub fn theme_names(cx: &App) -> Vec<SharedString> {
    ThemeRegistry::global(cx)
        .sorted_themes()
        .iter()
        .map(|c| c.name.clone())
        .collect()
}

/// Apply the saved theme by name at startup (falls back to the default dark theme).
pub fn apply_initial_theme(name: &str, cx: &mut App) {
    let config = {
        let registry = ThemeRegistry::global(cx);
        registry
            .themes()
            .get(name)
            .cloned()
            .unwrap_or_else(|| registry.default_dark_theme().clone())
    };
    Theme::global_mut(cx).apply_config(&config);
    apply_scale(cx);
}

/// Apply a theme by name at runtime and refresh open windows.
pub fn apply_theme(name: &str, cx: &mut App) {
    let config = ThemeRegistry::global(cx).themes().get(name).cloned();
    if let Some(config) = config {
        Theme::global_mut(cx).apply_config(&config);
        apply_scale(cx);
        cx.refresh_windows();
    }
}

fn hsla_to_u32(c: Hsla) -> u32 {
    let rgba: Rgba = c.into();
    let r = (rgba.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (rgba.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (rgba.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 16) | (g << 8) | b
}

/// Build a terminal color palette from the active theme.
///
/// gpui-component themes expose the six ANSI hues (+ `_light` brights) plus
/// background/foreground; black/white are derived from muted/foreground.
pub fn palette_from_theme(cx: &App) -> TerminalPalette {
    let t = cx.theme();
    TerminalPalette {
        background: hsla_to_u32(t.background),
        foreground: hsla_to_u32(t.foreground),
        cursor: hsla_to_u32(t.caret),
        selection: hsla_to_u32(t.selection),
        ansi: [
            hsla_to_u32(t.muted),            // 0  black (~ subtle bg)
            hsla_to_u32(t.red),              // 1  red
            hsla_to_u32(t.green),            // 2  green
            hsla_to_u32(t.yellow),           // 3  yellow
            hsla_to_u32(t.blue),             // 4  blue
            hsla_to_u32(t.magenta),          // 5  magenta
            hsla_to_u32(t.cyan),             // 6  cyan
            hsla_to_u32(t.foreground),       // 7  white (~ fg)
            hsla_to_u32(t.muted_foreground), // 8  bright black
            hsla_to_u32(t.red_light),        // 9  bright red
            hsla_to_u32(t.green_light),      // 10 bright green
            hsla_to_u32(t.yellow_light),     // 11 bright yellow
            hsla_to_u32(t.blue_light),       // 12 bright blue
            hsla_to_u32(t.magenta_light),    // 13 bright magenta
            hsla_to_u32(t.cyan_light),       // 14 bright cyan
            hsla_to_u32(t.foreground),       // 15 bright white
        ],
    }
}
