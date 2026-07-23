//! Windows present pump — force `WM_PAINT` under input load (gpui gap).
//!
//! # REMOVE WHEN UPSTREAM IS FIXED
//!
//! **Temporary workaround.** Delete this module (and its call in `main.rs`,
//! `MUXEL_NO_PRESENT_PUMP`, and `muxel_terminal::present_flag`) once:
//!
//! 1. [zed#61469](https://github.com/zed-industries/zed/issues/61469) is fixed
//!    so gpui-on-Windows presents frames during sustained input (not only when
//!    the message queue idles), **and**
//! 2. muxel's gpui pin (via gpui-component) includes that fix, **and**
//! 3. A regression check passes: multi-agent load + key-repeat / paste with
//!    `MUXEL_NO_PRESENT_PUMP=1` still shows continuous `Present()` (PresentMon)
//!    and no multi-second glass freeze.
//!
//! Until then, keep this. Escape hatch for A/B: `MUXEL_NO_PRESENT_PUMP=1`.
//!
//! ## Why it exists
//!
//! gpui-on-Windows presents ONLY from `WM_PAINT` (lowest priority; only when
//! the queue is idle). Under key-repeat + PTY notify the queue never idles, and
//! `dispatch_key_event` draws without presenting — frames render, none reach
//! the screen until input stops.
//!
//! ## v2 behavior (soft-lag fix)
//!
//! v1 posted every 8 ms and invalidated **every** top-level HWND → cured freezes
//! but mushy typing under load. This version:
//!
//! 1. **Dirty-gated** — terminals mark present needed; 33 ms keepalive for settings.
//! 2. **Adaptive interval** — 16 ms when cheap; 33–64 ms under backpressure.
//! 3. **Foreground HWND first** — one invalidate of the focused window.
//! 4. **PostMessage → UI-thread RDW** — never cross-thread `SendMessage`.

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, EnumThreadWindows, GetForegroundWindow,
    GetWindowThreadProcessId, HWND_MESSAGE, PostMessageW, RegisterClassW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_USER, WNDCLASSW,
};
use windows::core::PCWSTR;

/// Posted to our message-only window; handled on the UI thread.
const WM_MUXEL_PRESENT: u32 = WM_USER + 0x6D58; // "mX" — avoid gpui's WM_USER+1..8
/// UI latency probe: wparam = GetTickCount64 at post time.
const WM_MUXEL_PROBE: u32 = WM_USER + 0x6D59;

static PRESENT_PENDING: AtomicBool = AtomicBool::new(false);
static HWND_COUNT: AtomicU32 = AtomicU32::new(0);
static LAST_PUMP_US: AtomicU64 = AtomicU64::new(0);

pub fn spawn() {
    if std::env::var_os("MUXEL_NO_PRESENT_PUMP").is_some() {
        return;
    }

    unsafe extern "system" fn paint_one(hwnd: HWND, _: LPARAM) -> BOOL {
        HWND_COUNT.fetch_add(1, Ordering::Relaxed);
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
            let t0 = Instant::now();
            HWND_COUNT.store(0, Ordering::Relaxed);
            let ui_thread = unsafe { GetCurrentThreadId() };
            let fg = unsafe { GetForegroundWindow() };
            let mut did_fg = false;
            if fg.0 != 0 {
                let mut pid = 0u32;
                let fg_tid = unsafe { GetWindowThreadProcessId(fg, Some(&mut pid)) };
                if fg_tid == ui_thread {
                    HWND_COUNT.fetch_add(1, Ordering::Relaxed);
                    unsafe {
                        let _ = RedrawWindow(fg, None, None, RDW_INVALIDATE | RDW_UPDATENOW);
                    }
                    did_fg = true;
                }
            }
            if !did_fg {
                unsafe {
                    let _ = EnumThreadWindows(ui_thread, Some(paint_one), LPARAM(0));
                }
            }
            let hwnds = HWND_COUNT.load(Ordering::Relaxed);
            let elapsed = t0.elapsed();
            LAST_PUMP_US.store(elapsed.as_micros() as u64, Ordering::Relaxed);
            crate::ui_profile::pump_handled(elapsed, hwnds);
            return LRESULT(0);
        }
        if msg == WM_MUXEL_PROBE {
            let sent = wparam.0 as u64;
            let now = unsafe { GetTickCount64() };
            let rtt_ms = now.saturating_sub(sent);
            crate::ui_profile::probe_completed(rtt_ms.saturating_mul(1000));
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

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
            let mut last_keepalive = Instant::now();
            loop {
                let last_us = LAST_PUMP_US.load(Ordering::Relaxed);
                let interval_ms: u64 = if last_us > 40_000 {
                    64
                } else if last_us > 20_000 {
                    40
                } else if last_us > 12_000 {
                    33
                } else {
                    16
                };

                let needed = muxel_terminal::take_present_needed();
                let keepalive = last_keepalive.elapsed() >= Duration::from_millis(33);
                if needed || keepalive {
                    if keepalive {
                        last_keepalive = Instant::now();
                    }
                    if PRESENT_PENDING
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                    {
                        crate::ui_profile::pump_posted();
                        // Only the wndproc clears PRESENT_PENDING when the
                        // message arrives. If the post fails, clear here or the
                        // pump stays dark for the rest of the run.
                        let posted =
                            unsafe { PostMessageW(sink, WM_MUXEL_PRESENT, WPARAM(0), LPARAM(0)) };
                        if posted.is_err() {
                            PRESENT_PENDING.store(false, Ordering::Release);
                        }
                    } else {
                        crate::ui_profile::pump_coalesced();
                        if needed {
                            muxel_terminal::mark_present_needed();
                        }
                    }
                }

                std::thread::sleep(Duration::from_millis(interval_ms));
            }
        })
        .ok();

    std::thread::Builder::new()
        .name("muxel-ui-probe".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if !crate::ui_profile::is_enabled() {
                    continue;
                }
                let sent = unsafe { GetTickCount64() };
                crate::ui_profile::probe_mark_sent(sent);
                let ok = unsafe {
                    PostMessageW(sink, WM_MUXEL_PROBE, WPARAM(sent as usize), LPARAM(0)).is_ok()
                };
                if !ok {
                    crate::ui_profile::probe_timeout();
                    continue;
                }
                std::thread::sleep(Duration::from_millis(1000));
                if crate::ui_profile::probe_last_sent() == sent {
                    crate::ui_profile::probe_timeout();
                    crate::ui_profile::probe_mark_sent(0);
                }
            }
        })
        .ok();
}
