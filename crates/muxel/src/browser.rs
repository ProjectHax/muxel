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
    let url_without_fragment = url.split('#').next().unwrap_or(url);
    if let Some(name) = muxel_terminal::path_from_file_uri(url_without_fragment)
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
    {
        return name;
    }

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

#[cfg(test)]
mod tests {
    use super::tab_label;

    #[test]
    fn local_file_tab_uses_decoded_filename() {
        assert_eq!(
            tab_label("file:///D:/business/report%202026.html#L12"),
            "report 2026.html"
        );
    }
}

// ---------------------------------------------------------------------------
// macOS / Windows: the real embedded webview pane.
// ---------------------------------------------------------------------------
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod imp {
    use super::*;
    use gpui_component::button::{Button, ButtonVariants as _};
    use gpui_component::input::{
        Input, InputEvent, InputState, MoveToEnd, MoveToStart, SelectToStart,
    };
    use gpui_component::{Icon, IconName, Sizable as _, h_flex};
    #[cfg(target_os = "windows")]
    use webview2_com::FocusChangedEventHandler;
    #[cfg(target_os = "windows")]
    use wry::WebViewExtWindows as _;
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

    /// IPC message the page posts when it is clicked. Namespaced so it can't
    /// collide with a site that uses `window.ipc` for its own purposes.
    #[cfg(target_os = "macos")]
    const CLICK_MSG: &str = "muxel:page-click";

    /// Injected into every page (and every frame) before its own scripts run.
    ///
    /// The native webview is a real child window stacked ABOVE gpui, so a click
    /// inside it is consumed by the OS and gpui never dispatches a mouse event for
    /// it — which is why the pane's `on_mouse_down` handler (app.rs) can't see it,
    /// and why the clicked pane never became the active one. The page itself is the
    /// only thing that *can* see the click, so it tells us. Capture phase, so a
    /// page that stops propagation on its own handlers can't hide the click.
    #[cfg(target_os = "macos")]
    const PAGE_CLICK_SCRIPT: &str = r#"
        (function () {
          window.addEventListener('mousedown', function (event) {
            if (event.button === 0 &&
                (window.location.protocol === 'http:' ||
                 window.location.protocol === 'https:')) {
              try { window.ipc.postMessage('muxel:page-click'); } catch (e) {}
            }
          }, true);
        })();
    "#;

    const COPY_SCRIPT: &str = r#"
        (function () {
          // WebView2 hosted as a child window does not always run its built-in
          // copy accelerator. The native context-menu command still works, so
          // invoke that same document copy operation from the standard keys.
          window.addEventListener('keydown', function (event) {
            const key = String(event.key || '').toLowerCase();
            const copy = !event.altKey && !event.shiftKey && (
              ((event.ctrlKey || event.metaKey) && key === 'c') ||
              (event.ctrlKey && (key === 'insert' || event.code === 'Insert'))
            );
            if (copy) {
              try {
                if (document.execCommand('copy')) {
                  event.preventDefault();
                  event.stopPropagation();
                }
              } catch (e) {}
            }
          }, true);
        })();
    "#;

    pub struct BrowserView {
        focus_handle: FocusHandle,
        webview: Option<Entity<gpui_wry::WebView>>,
        /// Set once the deferred build finished *and* failed, so `render` can
        /// tell "still starting" apart from "gave up".
        webview_failed: bool,
        address: Entity<InputState>,
        url: String,
        /// Previous URL while a requested navigation has not committed yet.
        /// WebView2 reports the old page during that gap; do not sync it back.
        pending_navigation_from: Option<String>,
        /// What the app last asked of the native child (dedupes plaform calls).
        native_visible: bool,
        /// Native page-focus events. WebView2 reports these from its controller;
        /// macOS uses guarded page IPC until WKWebView exposes the same seam.
        focus_events: std::sync::mpsc::Receiver<bool>,
        /// URLs requested through target=_blank, window.open, Ctrl+click, or
        /// middle-click. The app turns these into normal Muxel browser tabs.
        new_window_events: std::sync::mpsc::Receiver<String>,
    }

    impl BrowserView {
        pub fn new(url: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
            let address = cx.new(|cx| InputState::new(window, cx).default_value(url.clone()));
            cx.subscribe_in(
                &address,
                window,
                |this: &mut Self, input, event: &InputEvent, window, cx| match event {
                    InputEvent::Focus => {
                        let handle = input.read(cx).focus_handle(cx);
                        // Select end-to-start so the whole URL is selected while
                        // the scheme and host remain at the visible edge.
                        handle.dispatch_action(&MoveToEnd, window, cx);
                        handle.dispatch_action(&SelectToStart, window, cx);
                    }
                    InputEvent::PressEnter { .. } => {
                        let typed = input.read(cx).value().trim().to_string();
                        if !typed.is_empty() {
                            this.navigate(&muxel_core::normalize_url(&typed), cx);
                            // Navigation is done. Return GPUI focus to the pane,
                            // then hand OS keyboard focus back to the page.
                            this.focus_handle.focus(window, cx);
                            this.focus_native(cx);
                        }
                    }
                    InputEvent::Blur => {
                        // An unfocused address bar should show its scheme/host,
                        // not remain scrolled to the tail where editing ended.
                        input
                            .read(cx)
                            .focus_handle(cx)
                            .dispatch_action(&MoveToStart, window, cx);
                    }
                    _ => {}
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
            let (focus_tx, focus_events) = std::sync::mpsc::channel();
            let (new_window_tx, new_window_events) = std::sync::mpsc::channel();

            cx.spawn_in(window, async move |this, cx| {
                #[cfg(target_os = "windows")]
                let mut web_context = muxel_store::data_dir().and_then(|dir| {
                    let dir = dir.join("webview2");
                    std::fs::create_dir_all(&dir)
                        .ok()
                        .map(|()| wry::WebContext::new(Some(dir)))
                });

                let built = match parent.as_ref() {
                    Some(parent) => {
                        #[cfg(target_os = "windows")]
                        let Some(builder) = web_context
                            .as_mut()
                            .map(wry::WebViewBuilder::new_with_web_context)
                        else {
                            let _ = this.update_in(cx, |this, _window, cx| {
                                this.webview_failed = true;
                                cx.notify();
                            });
                            return;
                        };
                        #[cfg(target_os = "macos")]
                        let builder = wry::WebViewBuilder::new();

                        let builder = builder
                            .with_url(&requested)
                            .with_initialization_script(COPY_SCRIPT)
                            .with_new_window_req_handler(move |url, _features| {
                                // A dropped receiver means this BrowserView was already torn down;
                                // still deny the native popup so no orphan OS window escapes.
                                let _ = new_window_tx.send(url);
                                wry::NewWindowResponse::Deny
                            });
                        #[cfg(target_os = "macos")]
                        let builder = builder
                            .with_initialization_script(PAGE_CLICK_SCRIPT)
                            .with_ipc_handler(move |req| {
                                if req.body().as_str() == CLICK_MSG {
                                    let _ = focus_tx.send(true);
                                }
                            });
                        let built = builder.build_as_child_async(parent).await.ok();

                        #[cfg(target_os = "windows")]
                        if let Some(webview) = built.as_ref() {
                            let got_tx = focus_tx.clone();
                            let got_focus =
                                FocusChangedEventHandler::create(Box::new(move |_, _| {
                                    let _ = got_tx.send(true);
                                    Ok(())
                                }));
                            let lost_tx = focus_tx.clone();
                            let lost_focus =
                                FocusChangedEventHandler::create(Box::new(move |_, _| {
                                    let _ = lost_tx.send(false);
                                    Ok(())
                                }));
                            let mut got_token = 0;
                            if let Err(error) = unsafe {
                                webview
                                    .controller()
                                    .add_GotFocus(&got_focus, &mut got_token)
                            } {
                                log::warn!("failed to observe WebView2 focus: {error}");
                            }
                            let mut lost_token = 0;
                            if let Err(error) = unsafe {
                                webview
                                    .controller()
                                    .add_LostFocus(&lost_focus, &mut lost_token)
                            } {
                                log::warn!("failed to observe WebView2 blur: {error}");
                            }
                        }

                        built
                    }
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
                pending_navigation_from: None,
                native_visible: true,
                focus_events,
                new_window_events,
            }
        }

        /// Drain page requests for a separate browsing context. Keeping this at
        /// the app boundary gives all browser entry points the same dedupe and
        /// tab-placement policy.
        pub fn take_new_window_requests(&mut self) -> Vec<String> {
            let mut urls = Vec::new();
            while let Ok(url) = self.new_window_events.try_recv() {
                urls.push(url);
            }
            urls
        }

        /// Whether the page was clicked since the last check (drains the queue).
        ///
        /// The app polls this each tick and makes this pane the active one — the
        /// click never reaches gpui on its own, so without this the pane keeps its
        /// old highlight and keyboard actions (paste, restart, close) go to
        /// whichever pane was focused before.
        pub fn take_native_focus(&mut self) -> Option<bool> {
            let mut focused = None;
            while let Ok(next) = self.focus_events.try_recv() {
                focused = Some(next);
            }
            focused
        }

        /// Hand the OS keyboard focus to the native webview, so typing (and paste)
        /// goes to the page rather than to the pane muxel last focused.
        pub fn focus_native(&self, cx: &App) {
            // Focusing a hidden child would pull focus out of whatever is actually
            // on screen (a modal, another tab).
            if !self.native_visible {
                return;
            }
            if let Some(wv) = &self.webview {
                let _ = wv.read(cx).raw().focus();
            }
        }

        pub fn address_focused(&self, window: &Window, cx: &App) -> bool {
            self.address.read(cx).focus_handle(cx).is_focused(window)
        }

        /// Reload the page the webview is *currently* on.
        ///
        /// This is wry's native reload, not a re-navigation to `self.url` and not a
        /// rebuild of the pane: the user may be several links deep, and refresh must
        /// reload where they actually are.
        pub fn reload(&mut self, cx: &mut Context<Self>) {
            if let Some(wv) = &self.webview {
                let _ = wv.read(cx).raw().reload();
            }
        }

        /// Step back / forward through the page's session history.
        ///
        /// Reload is the only navigation wry exposes natively, so history has to go
        /// through the page itself. That is a genuine limitation, not a shortcut:
        /// it means back/forward do nothing on a document where scripts can't run
        /// (a webview that failed to load, `about:blank`), where a native
        /// `canGoBack` would at least have told us the button was pointless. The
        /// buttons therefore stay enabled and simply no-op at the ends of history,
        /// which is also what they did before.
        fn history_go(&self, delta: i32, cx: &App) {
            if let Some(wv) = &self.webview {
                let _ = wv
                    .read(cx)
                    .raw()
                    .evaluate_script(&format!("history.go({delta});"));
            }
        }

        pub fn back(&mut self, cx: &mut Context<Self>) {
            self.history_go(-1, cx);
        }

        pub fn forward(&mut self, cx: &mut Context<Self>) {
            self.history_go(1, cx);
        }

        pub fn open_external(&self, cx: &mut Context<Self>) {
            cx.open_url(&self.url);
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
            if self.url != url {
                self.pending_navigation_from =
                    Some(std::mem::replace(&mut self.url, url.to_string()));
            }
            cx.notify();
        }

        /// Pull the webview's current URL (the user may have clicked links);
        /// returns it when it changed since the last sync. Called from the app's
        /// tick so `Instance.browser_url` and the address bar stay fresh.
        pub fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
            let wv = self.webview.as_ref()?;
            let current = wv.read(cx).url().ok()?;
            // A newly created WebView2 reports its bootstrap document before
            // the requested navigation commits. Persisting that transient URL
            // loses the resource identity and lets a fast second click create
            // a duplicate pane.
            if current == "about:blank" && self.url != "about:blank" {
                return None;
            }
            // A requested navigation hasn't committed yet: the webview still
            // reports the page we're leaving, so don't sync it back.
            if self.pending_navigation_from.as_deref() == Some(current.as_str()) {
                return None;
            }
            self.pending_navigation_from = None;
            // Deliberately no `current == self.url` early return: an unchanged URL
            // can still leave the address bar stale (a reused pane re-shown at the
            // same URL), and the address resync below is what fixes that.
            if current.is_empty() {
                return None;
            }
            let changed = current != self.url;
            if changed {
                self.url = current.clone();
            }
            // Don't stomp the address bar while the user is editing it.
            let address = self.address.read(cx);
            let address_stale = address.value() != current;
            let address_focused = address.focus_handle(cx).is_focused(window);
            if address_stale && !address_focused {
                let url = current.clone();
                self.address.update(cx, |s, cx| {
                    s.set_value(url, window, cx);
                    s.set_scroll_offset(point(px(0.), px(0.)), cx);
                });
                self.address
                    .read(cx)
                    .focus_handle(cx)
                    .dispatch_action(&MoveToStart, window, cx);
            }
            if changed || address_stale {
                cx.notify();
            }
            changed.then_some(current)
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
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, window, cx| {
                        this.focus_handle.focus(window, cx);
                    }),
                )
                .child(
                    Button::new("browser-back")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowLeft)
                        .tooltip(t("Back"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.back(cx))),
                )
                .child(
                    Button::new("browser-forward")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowRight)
                        .tooltip(t("Forward"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.forward(cx))),
                )
                .child(
                    Button::new("browser-reload")
                        .ghost()
                        .xsmall()
                        .icon(Icon::empty().path("icons/refresh.svg"))
                        .tooltip(t("Reload this page"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.reload(cx))),
                )
                .child(
                    Button::new("browser-open-external")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ExternalLink)
                        .tooltip(t("Open in system browser"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.open_external(cx))),
                )
                .child(
                    // gpui-wry returns focus to the native page for clicks outside
                    // its bounds. Claim this press after Input handles it so the
                    // window-level webview handler cannot steal focus back.
                    div()
                        .flex_1()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(Input::new(&self.address)),
                );

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

        /// No embedded webview here, so clicks land on ordinary gpui elements and
        /// the pane's own `on_mouse_down` already focuses it (see the macOS/Windows
        /// impl for why that isn't true there).
        pub fn take_native_focus(&mut self) -> Option<bool> {
            None
        }

        /// No native child to focus.
        pub fn focus_native(&self, _cx: &App) {}

        pub fn address_focused(&self, _window: &Window, _cx: &App) -> bool {
            false
        }

        // No `reload` here: the placeholder has no toolbar and no page. Reload is
        // called only from the macOS/Windows toolbar, inside that impl.
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
