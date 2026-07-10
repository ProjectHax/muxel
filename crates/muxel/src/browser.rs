//! The built-in browser pane.
//!
//! macOS/Windows: an embedded system webview (WKWebView / WebView2) hosted as a
//! native child of the gpui window via `gpui-wry`, with a small address-bar
//! toolbar rendered above it. **The native webview always draws on top of gpui
//! content inside its bounds** — the app is responsible for calling
//! [`BrowserView::set_native_visible`] so it hides whenever an overlay/modal
//! covers it or its tab isn't the active one (see `MuxelApp::sync_browser_visibility`).
//!
//! Linux: gpui can't host a WebKitGTK child (no GTK loop; XEmbed is X11-only),
//! so there is no embedded pane — links open in a separate muxel-managed
//! WebKitGTK window instead (`browser_helper.rs`). The `BrowserView` here is a
//! placeholder shown only if a workspace synced from another OS contains
//! browser panes.

use crate::i18n::t;
use gpui::*;
use gpui_component::{ActiveTheme as _, v_flex};

/// Spawn the separate Linux browser window (`muxel --browser <url>`), returning
/// whether the helper was launched (false → caller falls back to the OS browser).
#[cfg(target_os = "linux")]
pub fn spawn_browser_window(url: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    std::process::Command::new(exe)
        .arg("--browser")
        .arg(url)
        .spawn()
        .is_ok()
}

/// A short label for a browser tab: the URL's host (falls back to the URL).
fn tab_label(url: &str) -> String {
    let trimmed = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = trimmed.split(['/', '?']).next().unwrap_or(trimmed);
    if host.is_empty() {
        t("Browser").to_string()
    } else {
        host.to_string()
    }
}

// ---------------------------------------------------------------------------
// macOS / Windows: the real embedded webview pane.
// ---------------------------------------------------------------------------
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod imp {
    use super::*;
    use gpui_component::button::{Button, ButtonVariants as _};
    use gpui_component::input::{Input, InputEvent, InputState};
    use gpui_component::{IconName, Sizable as _, h_flex};
    use wry::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// The gpui window's native handle, detached from the `&Window` it came from.
    ///
    /// The webview is built from a spawned task (see [`BrowserView::new`]), where
    /// no `&Window` is in scope, so wry is handed this instead.
    struct ParentWindow(RawWindowHandle);

    impl HasWindowHandle for ParentWindow {
        fn window_handle(
            &self,
        ) -> Result<wry::raw_window_handle::WindowHandle<'_>, wry::raw_window_handle::HandleError>
        {
            // SAFETY: the handle belongs to the gpui window hosting this pane,
            // which outlives the pane and therefore the task borrowing it.
            Ok(unsafe { wry::raw_window_handle::WindowHandle::borrow_raw(self.0) })
        }
    }

    pub struct BrowserView {
        focus_handle: FocusHandle,
        webview: Option<Entity<gpui_wry::WebView>>,
        /// Set once the deferred build finished *and* failed, so `render` can
        /// tell "still starting" apart from "gave up".
        webview_failed: bool,
        address: Entity<InputState>,
        url: String,
        /// What the app last asked of the native child (dedupes plaform calls).
        native_visible: bool,
    }

    impl BrowserView {
        pub fn new(url: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
            let address = cx.new(|cx| InputState::new(window, cx).default_value(url.clone()));
            cx.subscribe(
                &address,
                |this: &mut Self, input, event: &InputEvent, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        let typed = input.read(cx).value().trim().to_string();
                        if !typed.is_empty() {
                            this.navigate(&normalize_url(&typed), cx);
                        }
                    }
                },
            )
            .detach();

            // Build the native webview from a spawned task, never inline here.
            //
            // WebView2 (Windows) initialises its controller by running a nested
            // Win32 message pump. Called inline, that pump re-enters gpui while
            // `App`'s RefCell is still mutably borrowed by the update building
            // this view, so the first foreground task it happens to run — a
            // terminal's PTY reader, say — panics with "RefCell already
            // borrowed". The async builder awaits a completion handler rather
            // than pumping, and a task body holds no borrow before its first
            // `update`, so neither hazard applies.
            //
            // Failure (e.g. the WebView2 runtime is missing) degrades to a
            // visible error row instead of crashing the pane.
            //
            // `HasWindowHandle::window_handle` is spelled out because gpui's
            // inherent `Window::window_handle` otherwise wins and yields gpui's
            // own `AnyWindowHandle`.
            let parent = HasWindowHandle::window_handle(&*window)
                .ok()
                .map(|h| ParentWindow(h.as_raw()));
            let requested = url.clone();

            cx.spawn_in(window, async move |this, cx| {
                let built = match parent.as_ref() {
                    Some(parent) => wry::WebViewBuilder::new()
                        .with_url(&requested)
                        .build_as_child_async(parent)
                        .await
                        .ok(),
                    None => None,
                };

                let _ = this.update_in(cx, |this, window, cx| {
                    let Some(wv) = built else {
                        this.webview_failed = true;
                        cx.notify();
                        return;
                    };
                    let wv = cx.new(|cx2| gpui_wry::WebView::new(wv, window, cx2));
                    // Re-apply whatever the pane changed while the build ran.
                    if !this.native_visible {
                        wv.update(cx, |wv, _| wv.hide());
                    }
                    if this.url != requested {
                        let current = this.url.clone();
                        wv.update(cx, |wv, _| wv.load_url(&current));
                    }
                    this.webview = Some(wv);
                    cx.notify();
                });
            })
            .detach();

            Self {
                focus_handle: cx.focus_handle(),
                webview: None,
                webview_failed: false,
                address,
                url,
                native_visible: true,
            }
        }

        pub fn tab_title(&self) -> String {
            super::tab_label(&self.url)
        }

        /// The URL the pane is showing. A native webview child belongs to the
        /// window that created it, so moving a browser pane between windows
        /// (pop-out / re-dock) rebuilds it from this rather than reparenting.
        pub fn url(&self) -> &str {
            &self.url
        }

        /// Navigate the webview and remember the URL.
        pub fn navigate(&mut self, url: &str, cx: &mut Context<Self>) {
            if let Some(wv) = &self.webview {
                wv.update(cx, |wv, _| wv.load_url(url));
            }
            self.url = url.to_string();
            cx.notify();
        }

        /// Pull the webview's current URL (the user may have clicked links);
        /// returns it when it changed since the last sync. Called from the app's
        /// tick so `Instance.browser_url` and the address bar stay fresh.
        pub fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
            let wv = self.webview.as_ref()?;
            let current = wv.read(cx).url().ok()?;
            if current.is_empty() || current == self.url {
                return None;
            }
            self.url = current.clone();
            // Don't stomp the address bar while the user is editing it.
            if !self.address.read(cx).focus_handle(cx).is_focused(window) {
                let url = current.clone();
                self.address
                    .update(cx, |s, cx| s.set_value(url, window, cx));
            }
            cx.notify();
            Some(current)
        }

        /// Show/hide the NATIVE child window. The app drives this every frame:
        /// hidden whenever an overlay covers the pane area or this tab isn't
        /// the active one (the native view otherwise floats above everything).
        pub fn set_native_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
            if self.native_visible == visible {
                return;
            }
            self.native_visible = visible;
            if let Some(wv) = &self.webview {
                wv.update(cx, |wv, _| if visible { wv.show() } else { wv.hide() });
            }
        }
    }

    /// `example.com` → `https://example.com` (typed addresses rarely carry a scheme).
    fn normalize_url(typed: &str) -> String {
        if typed.starts_with("http://") || typed.starts_with("https://") {
            typed.to_string()
        } else {
            format!("https://{typed}")
        }
    }

    impl Focusable for BrowserView {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for BrowserView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let toolbar = h_flex()
                .gap_1()
                .px_2()
                .py_1()
                .bg(cx.theme().secondary)
                .child(
                    Button::new("browser-back")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowLeft)
                        .tooltip(t("Back"))
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            if let Some(wv) = &this.webview {
                                wv.update(cx, |wv, _| {
                                    let _ = wv.back();
                                });
                            }
                        })),
                )
                .child(
                    Button::new("browser-reload")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Redo)
                        .tooltip(t("Reload"))
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            if let Some(wv) = &this.webview {
                                wv.update(cx, |wv, _| {
                                    let _ = wv.evaluate_script("location.reload();");
                                });
                            }
                        })),
                )
                .child(div().flex_1().child(Input::new(&self.address)));

            let content: AnyElement = match &self.webview {
                Some(wv) => div()
                    .flex_1()
                    .min_h_0()
                    .child(wv.clone())
                    .into_any_element(),
                // The native child is still being built; stay blank rather than
                // flash a failure the pane hasn't actually hit.
                None if !self.webview_failed => div().flex_1().into_any_element(),
                None => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(t("The system webview failed to start."))
                    .into_any_element(),
            };

            v_flex()
                .size_full()
                .track_focus(&self.focus_handle)
                .child(toolbar)
                .child(content)
        }
    }
}

// ---------------------------------------------------------------------------
// Linux: placeholder pane (the real browser is a separate window).
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use gpui_component::button::{Button, ButtonVariants as _};

    pub struct BrowserView {
        focus_handle: FocusHandle,
        url: String,
    }

    impl BrowserView {
        pub fn new(url: String, _window: &mut Window, cx: &mut Context<Self>) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
                url,
            }
        }

        pub fn tab_title(&self) -> String {
            super::tab_label(&self.url)
        }

        /// The URL the pane is showing (see the macOS/Windows impl).
        pub fn url(&self) -> &str {
            &self.url
        }

        pub fn sync(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<String> {
            None
        }

        pub fn set_native_visible(&mut self, _visible: bool, _cx: &mut Context<Self>) {}
    }

    impl Focusable for BrowserView {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for BrowserView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let url = self.url.clone();
            v_flex()
                .size_full()
                .track_focus(&self.focus_handle)
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(cx.theme().muted_foreground)
                .child(t("On Linux the built-in browser opens as its own window."))
                .child(
                    Button::new("browser-open-window")
                        .primary()
                        .label(t("Open in browser window"))
                        .on_click(cx.listener(move |_this, _e, _w, cx| {
                            if !super::spawn_browser_window(&url) {
                                cx.open_url(&url);
                            }
                        })),
                )
        }
    }
}

pub use imp::BrowserView;
