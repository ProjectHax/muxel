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
/// fix; until then this stays. Idle cost is small: `draw_window(false)` is a
/// no-op when nothing is dirty / nothing needs present.
///
/// gpui-on-Windows presents ONLY from `WM_PAINT`, the lowest-priority message,
/// synthesized when the message queue is idle. Under key-repeat + PTY notify
/// traffic the queue never idles — and gpui's `dispatch_key_event` draws the
/// window synchronously (consuming the dirty flag) WITHOUT presenting, so
/// frames are rendered but never reach the screen until input stops (proven
/// via PresentMon: 15s of zero `Present()` calls while element paints ticked
/// at 20/s). `RDW_UPDATENOW` delivers `WM_PAINT` through the sent-message
/// channel, which bypasses posted-queue priority.
///
/// Calling `RedrawWindow(RDW_UPDATENOW)` from a **background** thread is a
/// cross-thread `SendMessage` and re-enters the window proc while `App`'s
/// `RefCell` is still borrowed → `ERROR gpui::window: already borrowed`.
/// Calling gpui's `WM_GPUI_FORCE_UPDATE_WINDOW` instead avoids the borrow but
/// sets `force_render` and full-redraws every tick under load (felt like the
/// original freeze, just hotter). So: a message-only HWND on the UI thread
/// receives a normal-priority `PostMessage`; its wndproc then runs
/// `RDW_UPDATENOW` **on the UI thread between handlers**, where App is free
/// and paint goes through `draw_window(false)`.
#[cfg(target_os = "windows")]
fn spawn_present_pump() {
    // Escape hatch for reproducing the upstream bug (and later for verifying
    // its fix before removing the pump): MUXEL_NO_PRESENT_PUMP=1 disables it.
    if std::env::var_os("MUXEL_NO_PRESENT_PUMP").is_some() {
        return;
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, EnumThreadWindows, HWND_MESSAGE, PostMessageW,
        RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_USER, WNDCLASSW,
    };
    use windows::core::PCWSTR;

    /// Posted to our message-only window; handled on the UI thread.
    const WM_MUXEL_PRESENT: u32 = WM_USER + 0x6D58; // "mX" — avoid gpui's WM_USER+1..8

    // At most one present message in the queue — if the UI thread is busy the
    // pump ticks coalesce instead of flooding GetMessage.
    static PRESENT_PENDING: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn paint_one(hwnd: HWND, _: LPARAM) -> BOOL {
        // EnumThreadWindows skips message-only HWNDs, so we only hit real
        // top-level gpui windows. Same-thread RDW_UPDATENOW → nested WM_PAINT
        // → draw_window(false); App is not borrowed at top-of-loop.
        unsafe {
            let _ = RedrawWindow(hwnd, None, None, RDW_INVALIDATE | RDW_UPDATENOW);
        }
        BOOL(1)
    }

    unsafe extern "system" fn present_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_MUXEL_PRESENT {
            PRESENT_PENDING.store(false, Ordering::Release);
            let ui_thread = unsafe { GetCurrentThreadId() };
            unsafe {
                let _ = EnumThreadWindows(ui_thread, Some(paint_one), LPARAM(0));
            }
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    // Message-only window lives on this (UI) thread so its messages are
    // dispatched by gpui's GetMessage loop. Created before Application::new.
    let class_name: Vec<u16> = "muxel_present_pump\0".encode_utf16().collect();
    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .map(|m| m.into())
        .unwrap_or_default();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(present_wndproc),
        hInstance: hinstance,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // Ignore already-registered (hot reload / double main).
    unsafe {
        let _ = RegisterClassW(&wc);
    }
    let sink = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            hinstance,
            None,
        )
    };
    if sink.0 == 0 {
        log::error!("present pump: failed to create message-only window");
        return;
    }

    std::thread::Builder::new()
        .name("muxel-present-pump".to_string())
        .spawn(move || {
            loop {
                // Post, never Send — runs after the current UI handler returns.
                // Coalesce: skip if a present is already queued/in-flight.
                if PRESENT_PENDING
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    unsafe {
                        let _ = PostMessageW(sink, WM_MUXEL_PRESENT, WPARAM(0), LPARAM(0));
                    }
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
