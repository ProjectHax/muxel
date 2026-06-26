//! System-tray integration for muxel.
//!
//! A tray icon whose menu lists recent notifications and each agent's status,
//! and reports clicks back to the app as [`TrayAction`]s. The [`TrayModel`] is
//! pure data built by the app each tick; the per-platform backend renders it as
//! a menu. The app polls [`TrayController::take_action`] and pushes fresh state
//! with [`TrayController::update`].
//!
//! Backends: Linux uses `ksni` (StatusNotifierItem over D-Bus, no GTK) on its
//! own thread; other platforms fall back to a no-op until wired up. When no tray
//! is available [`TrayController::spawn`] returns `None` and the caller must keep
//! its normal close/quit behavior (never trap the user without a way back).

use uuid::Uuid;

/// Per-agent status (mirrors `muxel_terminal::AgentStatus`); used for menu icons.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayStatus {
    Working,
    Idle,
    Blocked,
    Done,
}

/// Notification category (mirrors the app's `NotifKind`); used for menu icons.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayKind {
    Blocked,
    Done,
    Success,
    Error,
}

/// One agent row in the tray menu. `label` is fully formatted (and localized) by
/// the app; clicking it focuses `iid`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrayAgent {
    pub iid: Uuid,
    pub label: String,
    pub status: TrayStatus,
}

/// One notification row in the tray menu. Clicking focuses `instance` when set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrayNotif {
    pub id: Uuid,
    pub instance: Option<Uuid>,
    pub label: String,
    pub kind: TrayKind,
}

/// The full tray menu contents. Cheap to compare (`PartialEq`) for change
/// detection so the app only pushes updates when something changed.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TrayModel {
    pub agents: Vec<TrayAgent>,
    pub notifications: Vec<TrayNotif>,
}

/// What a tray click asks the app to do (drained on the UI thread).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayAction {
    /// Restore / raise the main window.
    ShowWindow,
    /// Restore the window and focus this instance's project + pane.
    Focus(Uuid),
    /// Quit the application for real.
    Quit,
}

/// Tray icon source. Linux uses `name` (the installed desktop icon); Windows /
/// macOS use `rgba` (raw RGBA8 pixels).
#[derive(Clone)]
pub struct TrayIcon {
    pub name: String,
    pub tooltip: String,
    pub rgba: Option<TrayIconRgba>,
}

/// Raw RGBA8 icon pixels (for backends that need a pixmap rather than a name).
#[derive(Clone)]
pub struct TrayIconRgba {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Static (localized) menu labels, supplied by the app so this crate stays
/// i18n-free. Section headers are omitted when their list is empty.
#[derive(Clone)]
pub struct TrayLabels {
    pub show: String,
    pub quit: String,
    pub agents: String,
    pub notifications: String,
}

/// Per-platform tray backend (owns the icon + its event source). Not `Send`: the
/// controller lives on the app's main thread and is never moved across threads.
trait TrayBackend {
    /// Replace the menu contents with `model`.
    fn update(&self, model: TrayModel);
    /// Take the next queued click action, if any.
    fn take_action(&self) -> Option<TrayAction>;
}

/// A live system-tray handle. Dropping it removes the tray icon.
pub struct TrayController {
    backend: Box<dyn TrayBackend>,
}

impl TrayController {
    /// Start the platform tray. Returns `None` when no tray is available (no
    /// StatusNotifier host, unsupported platform, or init failure) — callers must
    /// treat that as "no tray" and not change their close/quit behavior.
    pub fn spawn(icon: TrayIcon, labels: TrayLabels) -> Option<TrayController> {
        Some(TrayController {
            backend: backend::spawn(icon, labels)?,
        })
    }

    /// Push the latest menu model to the tray.
    pub fn update(&self, model: TrayModel) {
        self.backend.update(model);
    }

    /// Take the next click action, if any (drained by the app each tick).
    pub fn take_action(&self) -> Option<TrayAction> {
        self.backend.take_action()
    }
}

/// Clamp a menu label to a sane width so a chatty agent title can't blow out the
/// tray menu. Adds an ellipsis when truncated. Pure + unit-tested.
pub fn truncate_label(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let cut = max.saturating_sub(1).min(chars.len());
    let mut out: String = chars[..cut].iter().collect();
    out.push('…');
    out
}

// ---- backend selection ----------------------------------------------------

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod backend;

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[path = "desktop.rs"]
mod backend;

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod backend {
    use super::{TrayBackend, TrayIcon, TrayLabels};
    /// No tray on this platform.
    pub fn spawn(_icon: TrayIcon, _labels: TrayLabels) -> Option<Box<dyn TrayBackend>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_label;

    #[test]
    fn truncate_label_keeps_short_strings() {
        assert_eq!(truncate_label("claude", 12), "claude");
        assert_eq!(truncate_label("", 5), "");
    }

    #[test]
    fn truncate_label_ellipsizes_long_strings() {
        assert_eq!(truncate_label("abcdefghij", 5), "abcd…");
        // Counts chars, not bytes (multibyte-safe).
        assert_eq!(truncate_label("ααααααα", 4), "ααα…");
    }
}
