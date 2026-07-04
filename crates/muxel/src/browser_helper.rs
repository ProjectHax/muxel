//! The Linux built-in browser: a separate muxel-managed WebKitGTK window.
//!
//! gpui on Linux runs its own X11/Wayland event loop and can't host a WebKitGTK
//! child widget (which needs a GTK main loop; XEmbed tricks are X11-only and
//! dead on Wayland). So the browser runs as a **relaunched copy of the muxel
//! binary** — `muxel --browser <url>` — whose main thread is a plain GTK loop
//! hosting one wry webview window. That keeps it crash- and memory-isolated
//! from the terminal workspace, and works identically on X11 and Wayland.
//!
//! Dispatch happens at the very top of `main()` before gpui initializes.

use gtk::prelude::*;

/// Run one browser window on the current thread until it's closed. Never
/// returns; exits the process (non-zero when GTK/WebKit can't start, so the
/// spawner can fall back to the system browser).
pub fn run(url: &str) -> ! {
    // NVIDIA (proprietary driver) + WebKitGTK on Wayland kills the process with
    // "Error 71 (Protocol error) dispatching to Wayland display" the moment the
    // window maps — WebKit's DMA-BUF renderer path. Disable that renderer when
    // an NVIDIA driver is present (same auto-mitigation Tauri applies), unless
    // the user already set a value themselves.
    if std::path::Path::new("/proc/driver/nvidia").exists()
        && std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none()
    {
        // SAFETY: top of the helper process, before GTK or any thread starts.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }

    if gtk::init().is_err() {
        eprintln!("muxel --browser: GTK failed to initialize");
        std::process::exit(2);
    }

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("muxel browser");
    window.set_default_size(1100, 800);
    // Ties the window to muxel's icon theme entry (grouping/wmclass follows the
    // process name, which is already `muxel`).
    gtk::Window::set_default_icon_name("muxel");
    window.connect_delete_event(|_, _| {
        gtk::main_quit();
        gtk::glib::Propagation::Proceed
    });
    // Also quit if the window is destroyed abnormally (not via the close button)
    // so the process never lingers invisibly behind a dead window.
    window.connect_destroy(|_| gtk::main_quit());

    // The webview is created as a child of the GTK window and fills it.
    use wry::WebViewBuilderExtUnix as _;
    let webview = wry::WebViewBuilder::new().with_url(url).build_gtk(&window);
    let _webview = match webview {
        Ok(wv) => wv,
        Err(e) => {
            eprintln!("muxel --browser: could not create the WebKit webview: {e}");
            std::process::exit(2);
        }
    };

    window.show_all();
    gtk::main();
    std::process::exit(0);
}
