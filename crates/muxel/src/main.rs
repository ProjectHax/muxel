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
#[cfg(target_os = "windows")]
mod present_pump;
mod secrets;
mod session_binding;
mod settings_view;
mod split;
mod stt;
mod theme;
mod tts;
mod ui_profile;
mod update;

use app::MuxelApp;
use gpui::*;
use gpui_component::{Root, TitleBar, *};
use std::borrow::Cow;

/// Persist panic details even when the GUI executable has no useful stderr.
///
/// Panics inside a native WebView/COM callback abort after the hook runs because
/// unwinding cannot cross that boundary. Windows Error Reporting then records
/// only `std::process::abort`, so without this file the actual Rust caller is
/// lost.
fn install_panic_reporter() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = muxel_store::data_dir() {
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("panic.log"))
            {
                use std::io::Write as _;
                let _ = writeln!(
                    file,
                    "\n=== {:?} thread {:?} ===\n{info}\n{}",
                    std::time::SystemTime::now(),
                    std::thread::current().name(),
                    std::backtrace::Backtrace::force_capture()
                );
            }
        }
        default_hook(info);
    }));
}

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

/// Windows present pump — see [`present_pump`].
///
/// **Temporary:** remove this call (and the `present_pump` module) when
/// [zed#61469](https://github.com/zed-industries/zed/issues/61469) lands in our
/// gpui pin and `MUXEL_NO_PRESENT_PUMP=1` no longer freezes under key-repeat.
#[cfg(target_os = "windows")]
fn spawn_present_pump() {
    present_pump::spawn();
}

/// The soft `RLIMIT_NOFILE` to move to, or `None` to leave it alone.
///
/// Only ever raises: a launcher that already handed us a generous soft limit
/// must not be clamped down to the per-process cap. An *unlimited* hard limit is
/// not actually settable — macOS caps NOFILE at `kern.maxfilesperproc` and Linux
/// at `fs.nr_open`, and `setrlimit` past that fails outright — so the cap stands
/// in for infinity.
#[cfg(unix)]
fn fd_limit_target(
    soft: libc::rlim_t,
    hard: libc::rlim_t,
    cap: Option<libc::rlim_t>,
) -> Option<libc::rlim_t> {
    let target = if hard == libc::RLIM_INFINITY {
        cap?
    } else {
        hard
    };
    (target > soft).then_some(target)
}

/// The kernel's per-process descriptor ceiling, when it can be read.
#[cfg(unix)]
fn per_process_fd_cap() -> Option<libc::rlim_t> {
    #[cfg(target_os = "macos")]
    {
        let mut max: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        // SAFETY: `kern.maxfilesperproc` is an int sysctl; both out-params are
        // stack locals sized to match.
        let ok = unsafe {
            libc::sysctlbyname(
                c"kern.maxfilesperproc".as_ptr(),
                (&raw mut max).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            ) == 0
        };
        (ok && max > 0).then_some(max as libc::rlim_t)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string("/proc/sys/fs/nr_open")
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }
}

/// Raise this process's open-file soft limit toward its hard limit (Unix).
///
/// Every pane costs three descriptors: the PTY master, the reader thread's dup,
/// and the writer thread's dup — plus SSH control sockets, fonts, watched files,
/// and whatever gpui holds for the window and GPU. macOS launches GUI apps with
/// a soft `RLIMIT_NOFILE` of **256** (`launchctl limit maxfiles`), which a busy
/// workspace can exhaust on panes alone; the process then hits EMFILE — "Too
/// many open files (os error 24)" — and can no longer spawn *anything*, down to
/// the fallback shell. Every terminal (Alacritty, WezTerm, Zed) raises this the
/// same way at startup.
///
/// Best effort: any failure just leaves the inherited limit in place.
#[cfg(unix)]
fn raise_open_file_limit() {
    // SAFETY: plain libc calls on a stack-local `rlimit`, before any thread that
    // could be spawning children observes the limit.
    unsafe {
        let mut limit = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return;
        }
        let Some(target) = fd_limit_target(limit.rlim_cur, limit.rlim_max, per_process_fd_cap())
        else {
            return;
        };
        limit.rlim_cur = target;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
            log::warn!(
                "could not raise the open-file limit to {target}: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

fn main() {
    match session_binding::hook_instance_from_args(
        std::env::args_os().skip(1),
        std::env::var_os(session_binding::MUXEL_INSTANCE_ID_ENV),
    ) {
        Ok(Some(instance_id)) => {
            let code = match session_binding::run_claude_session_hook(instance_id) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("muxel Claude session hook failed: {error:#}");
                    1
                }
            };
            std::process::exit(code);
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
    install_panic_reporter();
    // gpui reports real render failures (swap-chain present, scene-too-large
    // draw errors, GPU device loss) through `log` and swallows the Result;
    // without a logger they vanish silently. Errors/warnings go to stderr.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    // Opt-in lag harness (no-op unless MUXEL_PROFILE / MUXEL_PROFILE_UI / TERMINAL).
    ui_profile::init();
    // Before any pane spawns: GUI launchers hand us a soft limit far below what
    // a multiplexer needs (256 on macOS).
    #[cfg(unix)]
    raise_open_file_limit();

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

    // Force gpui to present under sustained input on Windows (see the fn). Spawns a
    // watchdog thread, so — like the reap above — it must come AFTER every `set_var`
    // block: `set_var` is only sound while the process is single-threaded.
    #[cfg(target_os = "windows")]
    spawn_present_pump();

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

#[cfg(all(test, unix))]
mod fd_limit_tests {
    use super::fd_limit_target;

    const INFINITY: libc::rlim_t = libc::RLIM_INFINITY;

    #[test]
    fn raises_a_gui_launcher_soft_limit_to_the_hard_limit() {
        // macOS hands GUI apps 256 against an unlimited hard limit.
        assert_eq!(fd_limit_target(256, INFINITY, Some(92_160)), Some(92_160));
        assert_eq!(fd_limit_target(1024, 524_288, None), Some(524_288));
    }

    #[test]
    fn never_lowers_an_already_generous_limit() {
        // The cap standing in for an unlimited hard limit must not clamp a shell
        // that already raised the soft limit above it.
        assert_eq!(fd_limit_target(1_048_576, INFINITY, Some(92_160)), None);
        assert_eq!(fd_limit_target(524_288, 524_288, None), None);
    }

    #[test]
    fn unreadable_cap_leaves_an_unlimited_hard_limit_alone() {
        assert_eq!(fd_limit_target(256, INFINITY, None), None);
    }

    /// End-to-end on the real process: raising must never cost us descriptors,
    /// whatever soft/hard/cap combination this machine happens to have.
    #[test]
    fn raising_the_real_limit_never_lowers_it() {
        // SAFETY: `getrlimit` into a stack-local `rlimit`.
        let soft = || unsafe {
            let mut limit = std::mem::zeroed::<libc::rlimit>();
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
            limit.rlim_cur
        };
        let before = soft();
        super::raise_open_file_limit();
        assert!(soft() >= before);
    }
}
