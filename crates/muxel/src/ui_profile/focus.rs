//! Content-free focus ownership telemetry for terminal focus edges.
//!
//! The terminal crate owns the exact GPUI edge. This module owns application
//! window identity, explicit focus actions, and platform sampling. The seam is
//! one optional function pointer, so disabled profiling never invokes a sampler.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use gpui::{App, Window};
use uuid::Uuid;

const MAX_WINDOWS: usize = 32;
const MAX_PANES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileWindowKind {
    Main,
    Secondary,
    Popout,
    Auxiliary,
    Unknown,
}

impl ProfileWindowKind {
    fn label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Secondary => "secondary",
            Self::Popout => "popout",
            Self::Auxiliary => "auxiliary",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusActionReason {
    FocusInstance,
    RestoreInstance,
    AppRootChrome,
    PanePointer,
    NativeBrowserGain,
    NativeBrowserAccepted,
    WindowActivated,
    WindowDeactivated,
}

impl FocusActionReason {
    fn label(self) -> &'static str {
        match self {
            Self::FocusInstance => "focus-instance",
            Self::RestoreInstance => "restore-instance",
            Self::AppRootChrome => "app-root-chrome",
            Self::PanePointer => "pane-pointer",
            Self::NativeBrowserGain => "native-browser-gain",
            Self::NativeBrowserAccepted => "native-browser-accepted",
            Self::WindowActivated => "window-activated",
            Self::WindowDeactivated => "window-deactivated",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FocusAction {
    reason: FocusActionReason,
    pane: Option<Uuid>,
    at: Instant,
    generation: u64,
}

#[derive(Clone, Copy, Debug)]
struct WindowState {
    id: u64,
    kind: ProfileWindowKind,
    hwnd: Option<isize>,
    last_action: Option<FocusAction>,
    touched: u64,
}

#[derive(Clone, Copy, Debug)]
struct PaneState {
    pane: Uuid,
    window_id: Option<u64>,
    last_action: Option<FocusAction>,
    touched: u64,
}

#[derive(Default)]
struct FocusRegistry {
    windows: Vec<WindowState>,
    panes: Vec<PaneState>,
    generation: u64,
}

impl FocusRegistry {
    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.generation
    }

    fn register_window(&mut self, id: u64, kind: ProfileWindowKind, hwnd: Option<isize>) {
        let touched = self.next_generation();
        if let Some(window) = self.windows.iter_mut().find(|window| window.id == id) {
            window.kind = kind;
            window.hwnd = hwnd.or(window.hwnd);
            window.touched = touched;
            return;
        }
        if self.windows.len() >= MAX_WINDOWS
            && let Some((index, _)) = self
                .windows
                .iter()
                .enumerate()
                .min_by_key(|(_, window)| window.touched)
        {
            let evicted = self.windows.remove(index).id;
            self.panes.retain(|pane| pane.window_id != Some(evicted));
        }
        self.windows.push(WindowState {
            id,
            kind,
            hwnd,
            last_action: None,
            touched,
        });
    }

    fn unregister_window(&mut self, id: u64) {
        self.windows.retain(|window| window.id != id);
        self.panes.retain(|pane| pane.window_id != Some(id));
    }

    fn register_pane(&mut self, pane: Uuid, window_id: u64) {
        let touched = self.next_generation();
        if let Some(state) = self.panes.iter_mut().find(|state| state.pane == pane) {
            state.window_id = Some(window_id);
            state.touched = touched;
            return;
        }
        if self.panes.len() >= MAX_PANES
            && let Some((index, _)) = self
                .panes
                .iter()
                .enumerate()
                .min_by_key(|(_, pane)| pane.touched)
        {
            self.panes.remove(index);
        }
        self.panes.push(PaneState {
            pane,
            window_id: Some(window_id),
            last_action: None,
            touched,
        });
    }

    fn unregister_pane(&mut self, pane: Uuid) {
        self.panes.retain(|state| state.pane != pane);
    }

    fn clear_panes(&mut self) {
        self.panes.clear();
        for window in &mut self.windows {
            window.last_action = None;
        }
    }

    fn record_window_action(
        &mut self,
        window_id: u64,
        reason: FocusActionReason,
        pane: Option<Uuid>,
        at: Instant,
    ) {
        let generation = self.next_generation();
        let action = FocusAction {
            reason,
            pane,
            at,
            generation,
        };
        if let Some(window) = self
            .windows
            .iter_mut()
            .find(|window| window.id == window_id)
        {
            window.last_action = Some(action);
            window.touched = generation;
        }
        if let Some(pane) = pane {
            self.register_pane(pane, window_id);
            if let Some(state) = self.panes.iter_mut().find(|state| state.pane == pane) {
                state.last_action = Some(action);
                state.touched = generation;
            }
        }
    }

    fn record_pane_action(&mut self, pane: Uuid, reason: FocusActionReason, at: Instant) {
        let generation = self.next_generation();
        let action = FocusAction {
            reason,
            pane: Some(pane),
            at,
            generation,
        };
        let window_id = self
            .panes
            .iter()
            .find(|state| state.pane == pane)
            .and_then(|state| state.window_id);
        if let Some(state) = self.panes.iter_mut().find(|state| state.pane == pane) {
            state.last_action = Some(action);
            state.touched = generation;
        } else {
            if self.panes.len() >= MAX_PANES
                && let Some((index, _)) = self
                    .panes
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, pane)| pane.touched)
            {
                self.panes.remove(index);
            }
            self.panes.push(PaneState {
                pane,
                window_id: None,
                last_action: Some(action),
                touched: generation,
            });
        }
        if let Some(window_id) = window_id
            && let Some(window) = self
                .windows
                .iter_mut()
                .find(|window| window.id == window_id)
        {
            window.last_action = Some(action);
            window.touched = generation;
        }
    }

    fn context(&self, window_id: u64, pane: Uuid, now: Instant) -> FocusContext {
        let window = self.windows.iter().find(|window| window.id == window_id);
        let pane_action = self
            .panes
            .iter()
            .find(|state| state.pane == pane)
            .and_then(|state| state.last_action);
        let window_action = window.and_then(|window| window.last_action);
        let action = [window_action, pane_action]
            .into_iter()
            .flatten()
            .max_by_key(|action| action.generation)
            .map(|action| FocusActionSnapshot {
                reason: action.reason,
                pane: action.pane,
                age: now.saturating_duration_since(action.at),
                generation: action.generation,
            });
        FocusContext {
            kind: window.map_or(ProfileWindowKind::Unknown, |window| window.kind),
            host_hwnd: window.and_then(|window| window.hwnd),
            action,
            top_level_hwnds: self
                .windows
                .iter()
                .filter_map(|window| window.hwnd)
                .collect(),
        }
    }
}

#[derive(Clone, Debug)]
struct FocusContext {
    kind: ProfileWindowKind,
    host_hwnd: Option<isize>,
    action: Option<FocusActionSnapshot>,
    top_level_hwnds: Vec<isize>,
}

#[derive(Clone, Copy, Debug)]
struct FocusActionSnapshot {
    reason: FocusActionReason,
    pane: Option<Uuid>,
    age: Duration,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OwnerKind {
    MuxelTopLevel,
    MuxelNativeChild,
    MuxelOther,
    External,
}

impl OwnerKind {
    fn label(self) -> &'static str {
        match self {
            Self::MuxelTopLevel => "muxel-top-level",
            Self::MuxelNativeChild => "muxel-native-child",
            Self::MuxelOther => "muxel-other",
            Self::External => "external",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum OwnerSnapshot {
    Unavailable,
    Absent,
    Present {
        hwnd: isize,
        pid: u32,
        tid: u32,
        class: String,
        this_process: bool,
        kind: OwnerKind,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeFocusSnapshot {
    foreground: OwnerSnapshot,
    active: OwnerSnapshot,
    focus: OwnerSnapshot,
    capture: OwnerSnapshot,
    menu_owner: OwnerSnapshot,
    move_size: OwnerSnapshot,
    caret: OwnerSnapshot,
}

impl NativeFocusSnapshot {
    #[cfg(any(not(target_os = "windows"), test))]
    fn unavailable() -> Self {
        Self {
            foreground: OwnerSnapshot::Unavailable,
            active: OwnerSnapshot::Unavailable,
            focus: OwnerSnapshot::Unavailable,
            capture: OwnerSnapshot::Unavailable,
            menu_owner: OwnerSnapshot::Unavailable,
            move_size: OwnerSnapshot::Unavailable,
            caret: OwnerSnapshot::Unavailable,
        }
    }
}

pub(super) struct FocusEvent {
    pane: Uuid,
    focused: bool,
    window_active: bool,
    window_kind: ProfileWindowKind,
    window_id: u64,
    host_hwnd: Option<isize>,
    action: Option<FocusActionSnapshot>,
    native: NativeFocusSnapshot,
}

static REGISTRY: OnceLock<Mutex<FocusRegistry>> = OnceLock::new();

fn registry() -> &'static Mutex<FocusRegistry> {
    REGISTRY.get_or_init(|| Mutex::new(FocusRegistry::default()))
}

/// Return the exact terminal-edge observer only when profiling is enabled.
pub fn terminal_focus_observer() -> Option<muxel_terminal::TerminalFocusObserver> {
    terminal_focus_observer_when(super::is_enabled())
}

fn terminal_focus_observer_when(enabled: bool) -> Option<muxel_terminal::TerminalFocusObserver> {
    enabled.then_some(terminal_focus_edge)
}

fn terminal_focus_edge(pane: Uuid, focused: bool, window: &mut Window, _cx: &mut App) {
    if !super::is_enabled() {
        return;
    }
    super::ensure_flusher();
    let window_id = window.window_handle().window_id().as_u64();
    let host_hwnd = platform_window_hwnd(window);
    let now = Instant::now();
    let context = if let Ok(mut state) = registry().lock() {
        if !state.windows.iter().any(|known| known.id == window_id) {
            state.register_window(window_id, ProfileWindowKind::Unknown, host_hwnd);
        }
        state.register_pane(pane, window_id);
        state.context(window_id, pane, now)
    } else {
        FocusContext {
            kind: ProfileWindowKind::Unknown,
            host_hwnd,
            action: None,
            top_level_hwnds: host_hwnd.into_iter().collect(),
        }
    };
    let native = sample_native_focus(&context.top_level_hwnds);
    super::defer_record(super::DeferredRecord::Focus {
        at: SystemTime::now(),
        event: Box::new(FocusEvent {
            pane,
            focused,
            window_active: window.is_window_active(),
            window_kind: context.kind,
            window_id,
            host_hwnd: context.host_hwnd,
            action: context.action,
            native,
        }),
    });
}

pub fn register_profile_window(kind: ProfileWindowKind, window: &Window) {
    if !super::is_enabled() {
        return;
    }
    let id = window.window_handle().window_id().as_u64();
    let hwnd = platform_window_hwnd(window);
    if let Ok(mut state) = registry().lock() {
        state.register_window(id, kind, hwnd);
    }
}

pub fn unregister_profile_window(window_id: u64) {
    if !super::is_enabled() {
        return;
    }
    if let Ok(mut state) = registry().lock() {
        state.unregister_window(window_id);
    }
}

pub fn register_focus_pane(pane: Uuid, window: &Window) {
    if !super::is_enabled() {
        return;
    }
    let window_id = window.window_handle().window_id().as_u64();
    if let Ok(mut state) = registry().lock() {
        state.register_pane(pane, window_id);
    }
}

pub fn unregister_focus_pane(pane: Uuid) {
    if !super::is_enabled() {
        return;
    }
    if let Ok(mut state) = registry().lock() {
        state.unregister_pane(pane);
    }
}

pub fn clear_focus_panes() {
    if !super::is_enabled() {
        return;
    }
    if let Ok(mut state) = registry().lock() {
        state.clear_panes();
    }
}

pub fn focus_action(reason: FocusActionReason, pane: Option<Uuid>, window: &Window) {
    if !super::is_enabled() {
        return;
    }
    let window_id = window.window_handle().window_id().as_u64();
    if let Ok(mut state) = registry().lock() {
        state.record_window_action(window_id, reason, pane, Instant::now());
    }
}

pub fn focus_action_for_pane(reason: FocusActionReason, pane: Uuid) {
    if !super::is_enabled() {
        return;
    }
    if let Ok(mut state) = registry().lock() {
        state.record_pane_action(pane, reason, Instant::now());
    }
}

pub fn window_activation(kind: ProfileWindowKind, active: bool, window: &Window) {
    if !super::is_enabled() {
        return;
    }
    register_profile_window(kind, window);
    focus_action(
        if active {
            FocusActionReason::WindowActivated
        } else {
            FocusActionReason::WindowDeactivated
        },
        None,
        window,
    );
}

fn classify_owner(
    hwnd_present: bool,
    pid: u32,
    current_pid: u32,
    known_top_level: bool,
    known_native_child: bool,
) -> Option<OwnerKind> {
    if !hwnd_present {
        None
    } else if known_top_level {
        Some(OwnerKind::MuxelTopLevel)
    } else if known_native_child {
        Some(OwnerKind::MuxelNativeChild)
    } else if pid == current_pid {
        Some(OwnerKind::MuxelOther)
    } else {
        Some(OwnerKind::External)
    }
}

pub(super) fn sanitize_class_name(class: &str) -> String {
    let clean: String = class
        .chars()
        .take(96)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        "none".to_string()
    } else {
        clean
    }
}

fn owner_field(owner: &OwnerSnapshot) -> String {
    match owner {
        OwnerSnapshot::Unavailable => "unavailable".to_string(),
        OwnerSnapshot::Absent => "none".to_string(),
        OwnerSnapshot::Present {
            hwnd,
            pid,
            tid,
            class,
            this_process,
            kind,
        } => format!(
            "0x{:x}/{pid}/{tid}/{}/{}/{class}",
            *hwnd as usize,
            if *this_process { "self" } else { "other" },
            kind.label(),
        ),
    }
}

pub(super) fn focus_line(event: &FocusEvent) -> String {
    let host = event.host_hwnd.map_or_else(
        || "none".to_string(),
        |hwnd| format!("0x{:x}", hwnd as usize),
    );
    let (action, action_pane, action_age, action_generation) = event.action.map_or_else(
        || {
            (
                "none".to_string(),
                "none".to_string(),
                "none".to_string(),
                0,
            )
        },
        |action| {
            (
                action.reason.label().to_string(),
                action
                    .pane
                    .map_or_else(|| "none".to_string(), |pane| pane.to_string()),
                format!("{}µs", action.age.as_micros()),
                action.generation,
            )
        },
    );
    format!(
        "ui-prof[focus v2] pane={} focused={} window_active={} window_kind={} window_id={} host_hwnd={} action={} action_pane={} action_age={} action_gen={} foreground={} active={} focus={} capture={} menu_owner={} move_size={} caret={}",
        event.pane,
        event.focused,
        event.window_active,
        event.window_kind.label(),
        event.window_id,
        host,
        action,
        action_pane,
        action_age,
        action_generation,
        owner_field(&event.native.foreground),
        owner_field(&event.native.active),
        owner_field(&event.native.focus),
        owner_field(&event.native.capture),
        owner_field(&event.native.menu_owner),
        owner_field(&event.native.move_size),
        owner_field(&event.native.caret),
    )
}

#[cfg(target_os = "windows")]
fn platform_window_hwnd(window: &Window) -> Option<isize> {
    use wry::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return None;
    };
    Some(handle.hwnd.get())
}

#[cfg(not(target_os = "windows"))]
fn platform_window_hwnd(_window: &Window) -> Option<isize> {
    None
}

#[cfg(target_os = "windows")]
fn sample_native_focus(top_level_hwnds: &[isize]) -> NativeFocusSnapshot {
    // Win32 offers no atomic foreground + GUITHREADINFO snapshot. A foreground
    // switch between those calls can produce a deliberately visible mixed
    // record; analysts should compare every owner field instead of assuming
    // they were captured as one transaction.
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        GUITHREADINFO, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetParent,
        GetWindowThreadProcessId,
    };

    fn class_name(hwnd: HWND) -> String {
        let mut buffer = [0u16; 128];
        let len = unsafe { GetClassNameW(hwnd, &mut buffer) }.max(0) as usize;
        sanitize_class_name(&String::from_utf16_lossy(&buffer[..len.min(buffer.len())]))
    }

    fn native_child(hwnd: HWND) -> bool {
        let mut current = hwnd;
        for _ in 0..16 {
            if current.0 == 0 {
                return false;
            }
            if class_name(current).eq_ignore_ascii_case("WRY_WEBVIEW") {
                return true;
            }
            current = unsafe { GetParent(current) };
        }
        false
    }

    fn owner(hwnd: HWND, current_pid: u32, top_level_hwnds: &[isize]) -> OwnerSnapshot {
        if hwnd.0 == 0 {
            return OwnerSnapshot::Absent;
        }
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let known_top_level = top_level_hwnds.contains(&hwnd.0);
        let known_native_child = native_child(hwnd);
        let kind = classify_owner(true, pid, current_pid, known_top_level, known_native_child)
            .unwrap_or(OwnerKind::External);
        OwnerSnapshot::Present {
            hwnd: hwnd.0,
            pid,
            tid,
            class: class_name(hwnd),
            this_process: pid == current_pid,
            kind,
        }
    }

    let current_pid = unsafe { GetCurrentProcessId() };
    let foreground = unsafe { GetForegroundWindow() };
    let foreground_owner = owner(foreground, current_pid, top_level_hwnds);
    if foreground.0 == 0 {
        return NativeFocusSnapshot {
            foreground: foreground_owner,
            active: OwnerSnapshot::Absent,
            focus: OwnerSnapshot::Absent,
            capture: OwnerSnapshot::Absent,
            menu_owner: OwnerSnapshot::Absent,
            move_size: OwnerSnapshot::Absent,
            caret: OwnerSnapshot::Absent,
        };
    }
    let mut pid = 0;
    let foreground_tid = unsafe { GetWindowThreadProcessId(foreground, Some(&mut pid)) };
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if foreground_tid == 0 || unsafe { GetGUIThreadInfo(foreground_tid, &mut info) }.is_err() {
        return NativeFocusSnapshot {
            foreground: foreground_owner,
            active: OwnerSnapshot::Unavailable,
            focus: OwnerSnapshot::Unavailable,
            capture: OwnerSnapshot::Unavailable,
            menu_owner: OwnerSnapshot::Unavailable,
            move_size: OwnerSnapshot::Unavailable,
            caret: OwnerSnapshot::Unavailable,
        };
    }
    NativeFocusSnapshot {
        foreground: foreground_owner,
        active: owner(info.hwndActive, current_pid, top_level_hwnds),
        focus: owner(info.hwndFocus, current_pid, top_level_hwnds),
        capture: owner(info.hwndCapture, current_pid, top_level_hwnds),
        menu_owner: owner(info.hwndMenuOwner, current_pid, top_level_hwnds),
        move_size: owner(info.hwndMoveSize, current_pid, top_level_hwnds),
        caret: owner(info.hwndCaret, current_pid, top_level_hwnds),
    }
}

#[cfg(not(target_os = "windows"))]
fn sample_native_focus(_top_level_hwnds: &[isize]) -> NativeFocusSnapshot {
    NativeFocusSnapshot::unavailable()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_classifies_absent_top_native_same_process_and_external() {
        assert_eq!(classify_owner(false, 0, 7, false, false), None);
        assert_eq!(
            classify_owner(true, 7, 7, true, false),
            Some(OwnerKind::MuxelTopLevel)
        );
        assert_eq!(
            classify_owner(true, 70, 7, false, true),
            Some(OwnerKind::MuxelNativeChild)
        );
        assert_eq!(
            classify_owner(true, 7, 7, false, false),
            Some(OwnerKind::MuxelOther)
        );
        assert_eq!(
            classify_owner(true, 70, 7, false, false),
            Some(OwnerKind::External)
        );
    }

    #[test]
    fn focus_format_is_stable_content_free_and_cross_platform_explicit() {
        let event = FocusEvent {
            pane: Uuid::nil(),
            focused: false,
            window_active: true,
            window_kind: ProfileWindowKind::Main,
            window_id: 9,
            host_hwnd: Some(0x12),
            action: Some(FocusActionSnapshot {
                reason: FocusActionReason::PanePointer,
                pane: Some(Uuid::max()),
                age: Duration::from_micros(44),
                generation: 3,
            }),
            native: NativeFocusSnapshot {
                foreground: OwnerSnapshot::Present {
                    hwnd: 0x34,
                    pid: 5,
                    tid: 6,
                    class: sanitize_class_name("Chrome Widget / title"),
                    this_process: false,
                    kind: OwnerKind::MuxelNativeChild,
                },
                active: OwnerSnapshot::Absent,
                focus: OwnerSnapshot::Unavailable,
                capture: OwnerSnapshot::Absent,
                menu_owner: OwnerSnapshot::Absent,
                move_size: OwnerSnapshot::Absent,
                caret: OwnerSnapshot::Absent,
            },
        };
        assert_eq!(
            focus_line(&event),
            "ui-prof[focus v2] pane=00000000-0000-0000-0000-000000000000 focused=false window_active=true window_kind=main window_id=9 host_hwnd=0x12 action=pane-pointer action_pane=ffffffff-ffff-ffff-ffff-ffffffffffff action_age=44µs action_gen=3 foreground=0x34/5/6/other/muxel-native-child/Chrome_Widget___title active=none focus=unavailable capture=none menu_owner=none move_size=none caret=none"
        );
        let line = focus_line(&event);
        for forbidden in ["path=", "url=", "title=", "command=", "clipboard=", "row="] {
            assert!(!line.contains(forbidden));
        }
    }

    #[test]
    fn last_reason_replaces_by_generation_and_cleanup_stays_bounded() {
        let mut registry = FocusRegistry::default();
        let pane = Uuid::nil();
        let now = Instant::now();
        registry.register_window(1, ProfileWindowKind::Main, Some(11));
        registry.register_pane(pane, 1);
        registry.record_window_action(
            1,
            FocusActionReason::FocusInstance,
            Some(pane),
            now - Duration::from_millis(5),
        );
        registry.record_pane_action(pane, FocusActionReason::PanePointer, now);
        let action = registry.context(1, pane, now).action.expect("last action");
        assert_eq!(action.reason, FocusActionReason::PanePointer);
        assert_eq!(action.age, Duration::ZERO);

        for raw in 0..(MAX_PANES + 20) {
            registry.register_pane(Uuid::from_u128(raw as u128 + 1), 1);
        }
        assert!(registry.panes.len() <= MAX_PANES);
        registry.unregister_window(1);
        assert!(registry.windows.is_empty());
        assert!(registry.panes.iter().all(|pane| pane.window_id != Some(1)));
    }

    #[test]
    fn unavailable_snapshot_formats_every_native_owner_explicitly() {
        let event = FocusEvent {
            pane: Uuid::nil(),
            focused: true,
            window_active: true,
            window_kind: ProfileWindowKind::Unknown,
            window_id: 1,
            host_hwnd: None,
            action: None,
            native: NativeFocusSnapshot::unavailable(),
        };
        let line = focus_line(&event);
        assert_eq!(line.matches("unavailable").count(), 7);
        assert!(line.contains("window_kind=unknown"));
        assert!(line.contains("action=none"));
    }

    #[test]
    fn disabled_profiler_injects_no_terminal_focus_observer() {
        assert!(terminal_focus_observer_when(false).is_none());
        assert!(terminal_focus_observer_when(true).is_some());
    }
}
