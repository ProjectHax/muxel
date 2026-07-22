//! muxel — a multi-agent terminal multiplexer built on GPUI.
//!
//! See [`app::MuxelApp`] for the application shell.

// On Windows, release builds use the GUI subsystem so launching muxel doesn't
// pop a console/cmd window alongside the app. Debug builds keep the console so
// logs stay visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod browser;
#[cfg(target_os = "linux")]
mod browser_helper;
mod editor;
mod filetree;
mod i18n;
mod integrations;
mod secrets;
mod settings_view;
mod stt;
mod theme;
mod tts;
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

/// Windows present pump — works around a gpui frame-scheduling gap that froze
/// terminal panes for seconds under sustained key repeat.
///
/// Upstream: <https://github.com/zed-industries/zed/issues/61469>. Removable
/// once that is fixed AND muxel's gpui pin (via gpui-component) includes the
/// fix; until then this stays. It is close to free when nothing is dirty.
///
/// gpui-on-Windows presents ONLY from `WM_PAINT`, the lowest-priority message,
/// synthesized when the message queue is idle. Under key-repeat + PTY notify
/// traffic the queue never idles — and gpui's `dispatch_key_event` draws the
/// window synchronously (consuming the dirty flag) WITHOUT presenting, so
/// frames are rendered but never reach the screen until input stops (proven
/// via PresentMon: 15s of zero `Present()` calls while element paints ticked
/// at 20/s). `RDW_UPDATENOW` delivers `WM_PAINT` through the sent-message
/// channel, which bypasses posted-queue priority entirely — a present
/// opportunity arrives every tick no matter how deep the queue is. Frames
/// with nothing new are a cheap no-op in gpui's request-frame handler.
#[cfg(target_os = "windows")]
fn spawn_present_pump() {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::Graphics::Gdi::{RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::EnumThreadWindows;

    unsafe extern "system" fn pump(hwnd: HWND, _: LPARAM) -> BOOL {
        unsafe {
            let _ = RedrawWindow(hwnd, None, None, RDW_INVALIDATE | RDW_UPDATENOW);
        }
        BOOL(1)
    }

    // Captured on the UI thread (main); the pump enumerates its windows so
    // secondary/pop-out windows are covered automatically.
    let ui_thread = unsafe { GetCurrentThreadId() };
    std::thread::Builder::new()
        .name("muxel-present-pump".to_string())
        .spawn(move || {
            loop {
                unsafe {
                    let _ = EnumThreadWindows(ui_thread, Some(pump), LPARAM(0));
                }
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
        })
        .ok();
}

fn main() {
    // gpui reports real render failures (swap-chain present, scene-too-large
    // draw errors, GPU device loss) through `log` and swallows the Result;
    // without a logger they vanish silently. Errors/warnings go to stderr.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    #[cfg(target_os = "windows")]
    spawn_present_pump();

    // Linux built-in browser: `muxel --browser <url>` relaunches this binary as
    // a standalone WebKitGTK window (gpui can't host one — see browser_helper).
    // Must run before anything gpui-related initializes.
    #[cfg(target_os = "linux")]
    if std::env::args().nth(1).as_deref() == Some("--browser") {
        match std::env::args().nth(2) {
            Some(url) => browser_helper::run(&url),
            None => {
                eprintln!("usage: muxel --browser <url>");
                std::process::exit(2);
            }
        }
    }

    // Windows: the embedded WebView2 browser pane can't share a surface with
    // gpui's DirectComposition path; when the browser is enabled, switch gpui to
    // its non-DirectComposition compositor before it initializes. (Read-only
    // early settings load; the app loads them again normally later.)
    #[cfg(target_os = "windows")]
    if muxel_store::load_settings().browser_enabled {
        // SAFETY: at the top of main, before any thread is spawned.
        unsafe { std::env::set_var("GPUI_DISABLE_DIRECT_COMPOSITION", "true") };
    }

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

    // A Linux desktop-entry / AppImage launch likewise inherits a minimal PATH
    // missing ~/.local/bin, ~/.opencode/bin (opencode's installer default),
    // Linuxbrew, etc. — so agents like opencode go undetected and fail to spawn.
    // Same fix: reconstruct the common dirs before any thread starts.
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME").ok();
        let current = std::env::var("PATH").ok();
        if let Some(path) = muxel_core::augmented_linux_path(current.as_deref(), home.as_deref()) {
            // SAFETY: still single-threaded here (before the GPUI app starts).
            unsafe { std::env::set_var("PATH", path) };
        }
    }

    // The local (Kokoro) voice: phonemize with the bundled CMU dictionary, never
    // with a system espeak-ng.
    //
    // Only compiled in with `--features voice-local`; nothing speaks by default.
    //
    // `kokoro-en` probes for an `espeak-ng` binary at RUNTIME and prefers it if it
    // finds one — and that path mangles word-final phonemes: "by" → "bee",
    // "online" → "onlin", "evening" → "evenin". It is audible as the last sound of
    // every word being clipped. The dictionary gets all of them right, digits
    // included ("12 agents" → twˈɛlv ˈeɪdʒənts).
    //
    // Compiling espeak out (`misaki-lean`, see Cargo.toml) is not enough on its
    // own, because that only drops the *bundled* copy. Without this the voice would
    // also be nondeterministic — muxel would speak differently depending on whether
    // espeak-ng happened to be installed, which is exactly how this went unnoticed.
    //
    // SAFETY: still single-threaded here (before the GPUI app starts) — and this
    // must precede the thread spawned just below.
    #[cfg(feature = "voice-local")]
    unsafe {
        std::env::set_var("KOKORO_ESPEAK_NG", "0")
    };

    // Reap stale muxel AppImage squashfuse mounts left in /tmp by prior instances
    // that crashed or were SIGKILLed before the runtime could unmount them —
    // otherwise a dead mount makes filesystem scans (e.g. a desktop monitor's
    // `df`) stall in the kernel FUSE layer and shows up as a periodic cursor
    // stutter on Wayland. Detached, so it never blocks startup. Must follow the
    // `set_var` blocks above: it spawns a thread, and `set_var` needs the process
    // still single-threaded.
    #[cfg(target_os = "linux")]
    integrations::reap_stale_appimage_mounts();

    gpui_platform::application()
        // Serves muxel's agent icons + gpui-component's bundled SVG icons.
        .with_assets(AppAssets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::register_bundled_themes(cx);
            app::register_actions(cx);

            let settings = muxel_store::load_settings();
            // Localization: pick the UI language (explicit setting → OS locale)
            // and load its catalog before any window renders.
            i18n::set_language(&i18n::detect_language(&settings));
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

            // The single-instance guard is now per-workspace and lives in the app:
            // entering a workspace takes its lock (`MuxelApp::enter_workspace`), so
            // two muxel processes can run side by side on different workspaces but
            // never clobber the same one.
            cx.spawn(async move |cx| {
                let options = WindowOptions {
                    titlebar: Some(TitleBar::title_bar_options()),
                    window_bounds,
                    // Matches the .desktop StartupWMClass so the desktop ties the
                    // window (and its notifications) to muxel's icon.
                    app_id: Some("muxel".to_string()),
                    ..Default::default()
                };
                cx.open_window(options, move |window, cx| {
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
