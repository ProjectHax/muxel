//! Theme registration + helpers: load the bundled themes, apply a saved theme,
//! switch themes/mode at runtime, and derive a terminal color palette from the
//! active gpui-component theme.

use crate::settings_model::{Rgb, ThemeInputs};
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

/// One swatch per registered theme, in the order the picker lists them. Each
/// is resolved the way applying it would, so a theme that leaves a colour out
/// shows the same fallback here as it would in use.
pub fn theme_swatches(cx: &App) -> Vec<crate::settings_view::ThemeSwatch> {
    ThemeRegistry::global(cx)
        .sorted_themes()
        .into_iter()
        .map(|config| {
            let mut theme = cx.theme().clone();
            theme.apply_config(config);
            crate::settings_view::ThemeSwatch {
                name: config.name.clone(),
                background: theme.background,
                border: theme.border,
                bars: [theme.foreground, theme.blue, theme.green, theme.yellow],
            }
        })
        .collect()
}

/// A theme colour as opaque sRGB, composited over `backdrop` when translucent.
fn rgb_over(color: Hsla, backdrop: Rgb) -> Rgb {
    let rgba: Rgba = color.into();
    let a = rgba.a.clamp(0.0, 1.0);
    [
        backdrop[0] + (rgba.r.clamp(0.0, 1.0) - backdrop[0]) * a,
        backdrop[1] + (rgba.g.clamp(0.0, 1.0) - backdrop[1]) * a,
        backdrop[2] + (rgba.b.clamp(0.0, 1.0) - backdrop[2]) * a,
    ]
}

/// An opaque sRGB colour as the `Hsla` gpui paints with.
pub fn hsla(rgb: Rgb) -> Hsla {
    Rgba {
        r: rgb[0],
        g: rgb[1],
        b: rgb[2],
        a: 1.0,
    }
    .into()
}

/// The colours of `theme` that the Settings window derives its palette from.
pub fn settings_theme_inputs(theme: &Theme) -> ThemeInputs {
    // A translucent background has nothing behind it but the window.
    let background = rgb_over(theme.background, [0.0; 3]);
    let over = |color: Hsla| rgb_over(color, background);
    let sidebar = over(theme.sidebar);
    ThemeInputs {
        background,
        foreground: over(theme.foreground),
        muted_foreground: over(theme.muted_foreground),
        border: over(theme.border),
        sidebar,
        sidebar_accent: rgb_over(theme.sidebar_accent, sidebar),
        sidebar_accent_foreground: rgb_over(theme.sidebar_accent_foreground, sidebar),
        primary: over(theme.primary),
        warning: over(theme.warning),
        danger: over(theme.danger),
        success: over(theme.success),
    }
}

#[cfg(test)]
mod tests {
    use super::{ThemeAssets, settings_theme_inputs};
    use crate::settings_model::{
        CONTROL_CONTRAST, DIM_CONTRAST, HIGHLIGHT_CONTRAST, contrast, palette,
    };
    use gpui_component::{Theme, ThemeColor, ThemeConfig, ThemeSet};
    use std::rc::Rc;

    /// Every bundled theme, plus gpui-component's two built-in defaults, resolved
    /// the way the running app resolves them.
    fn resolved_themes() -> Vec<(String, Theme)> {
        let mut configs: Vec<ThemeConfig> = Vec::new();
        for file in ThemeAssets::iter() {
            let embedded = ThemeAssets::get(&file).expect("embedded theme");
            let text = std::str::from_utf8(&embedded.data).expect("utf-8 theme");
            let set: ThemeSet =
                serde_json::from_str(text).unwrap_or_else(|e| panic!("{file} does not parse: {e}"));
            configs.extend(set.themes);
        }
        let mut themes: Vec<(String, Theme)> = vec![
            (
                "Default Light".to_string(),
                Theme::from(&*ThemeColor::light()),
            ),
            (
                "Default Dark".to_string(),
                Theme::from(&*ThemeColor::dark()),
            ),
        ];
        for config in configs {
            let mut theme = Theme::from(&*ThemeColor::dark());
            let name = config.name.to_string();
            theme.apply_config(&Rc::new(config));
            themes.push((name, theme));
        }
        themes
    }

    /// The Settings window has to stay usable in every theme muxel ships, not
    /// only the default: text and secondary text legible on every surface it
    /// sits on, controls visible against their cards, and a switch that is on
    /// distinguishable from one that is off.
    #[test]
    fn settings_palette_is_legible_in_every_bundled_theme() {
        let themes = resolved_themes();
        assert!(themes.len() > 30, "only {} themes resolved", themes.len());
        let mut failures: Vec<String> = Vec::new();
        for (name, theme) in &themes {
            let inputs = settings_theme_inputs(theme);
            let p = palette(&inputs);
            let mut check = |what: &str, ratio: f32, min: f32| {
                if ratio + 0.005 < min {
                    failures.push(format!("{name}: {what} is {ratio:.2}, needs {min:.2}"));
                }
            };
            for (surface_name, surface) in [
                ("background", p.background),
                ("card", p.card),
                ("nav", p.nav),
                ("track", p.track),
            ] {
                // Secondary text reads wherever primary text can; a theme whose
                // own text is fainter than that sets the ceiling for both.
                let ceiling = contrast(p.text, surface).min(DIM_CONTRAST);
                check(
                    &format!("secondary text on {surface_name}"),
                    contrast(p.dim, surface),
                    ceiling,
                );
                check(
                    &format!("text on {surface_name}"),
                    contrast(p.text, surface),
                    CONTROL_CONTRAST,
                );
            }
            check(
                "selected section text",
                contrast(p.nav_selected_text, p.nav_selected),
                DIM_CONTRAST,
            );
            check(
                "selected section against the list",
                contrast(p.nav_selected, p.nav),
                1.1,
            );
            check(
                "switch on against its card",
                contrast(p.accent, p.card),
                CONTROL_CONTRAST,
            );
            check(
                "thumb on a switch that is on",
                contrast(p.on_accent, p.accent),
                CONTROL_CONTRAST,
            );
            check(
                "thumb on a switch that is off",
                contrast(p.switch_off_thumb, p.switch_off),
                CONTROL_CONTRAST,
            );
            check(
                "chosen segment against its track",
                contrast(p.track_selected, p.track),
                1.15,
            );
            check(
                "chosen segment text",
                contrast(p.text, p.track_selected),
                CONTROL_CONTRAST,
            );
            check("card border", contrast(p.card_border, p.background), 1.2);
            check(
                "search highlight text",
                contrast(p.text, p.highlight),
                contrast(p.text, p.card).min(HIGHLIGHT_CONTRAST),
            );
            for (status_name, status) in [
                ("warning", p.warning),
                ("danger", p.danger),
                ("success", p.success),
            ] {
                let ceiling = contrast(p.text, p.card).min(DIM_CONTRAST);
                check(
                    &format!("{status_name} text"),
                    contrast(status, p.card),
                    ceiling,
                );
            }
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }
}
