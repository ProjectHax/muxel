//! Linux tray backend — StatusNotifierItem via `ksni` (pure zbus, no GTK), on its
//! own background thread. Clicks enqueue [`TrayAction`]s the app drains each tick.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};

use super::{TrayAction, TrayBackend, TrayIcon, TrayLabels, TrayModel};

/// Shared click queue: the ksni service thread pushes, the app drains.
type Actions = Arc<Mutex<VecDeque<TrayAction>>>;

/// The ksni tray object. ksni calls `menu()`/`icon_pixmap()` on its service
/// thread; the app mutates `model` through the [`Handle`].
struct MuxelTray {
    model: TrayModel,
    labels: TrayLabels,
    tooltip: String,
    /// The app icon as SNI pixmaps (empty → fall back to the themed `icon_name`).
    pixmap: Vec<ksni::Icon>,
    actions: Actions,
}

/// Convert RGBA8 pixels to an SNI icon (ARGB32, network byte order).
fn to_sni_icon(rgba: &super::TrayIconRgba) -> ksni::Icon {
    let mut data = rgba.bytes.clone();
    for px in data.chunks_exact_mut(4) {
        // [R, G, B, A] -> [A, R, G, B]
        px.rotate_right(1);
    }
    ksni::Icon {
        width: rgba.width as i32,
        height: rgba.height as i32,
        data,
    }
}

impl MuxelTray {
    fn push(&self, action: TrayAction) {
        if let Ok(mut q) = self.actions.lock() {
            q.push_back(action);
        }
    }

    /// A clickable row that enqueues `action` when activated.
    fn item(&self, label: String, action: TrayAction) -> MenuItem<Self> {
        StandardItem {
            label,
            activate: Box::new(move |t: &mut Self| t.push(action)),
            ..Default::default()
        }
        .into()
    }

    /// A non-clickable section header.
    fn header(&self, label: String) -> MenuItem<Self> {
        StandardItem {
            label,
            enabled: false,
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for MuxelTray {
    fn id(&self) -> String {
        "muxel".into()
    }

    fn title(&self) -> String {
        self.tooltip.clone()
    }

    fn icon_name(&self) -> String {
        // Prefer the real app-icon pixmap; only name-fall-back if we have none.
        if self.pixmap.is_empty() {
            "muxel".into()
        } else {
            String::new()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.pixmap.clone()
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = vec![self.item(self.labels.show.clone(), TrayAction::ShowWindow)];
        if !self.model.agents.is_empty() {
            items.push(MenuItem::Separator);
            items.push(self.header(self.labels.agents.clone()));
            for a in &self.model.agents {
                items.push(self.item(a.label.clone(), TrayAction::Focus(a.iid)));
            }
        }
        if !self.model.notifications.is_empty() {
            items.push(MenuItem::Separator);
            items.push(self.header(self.labels.notifications.clone()));
            for n in &self.model.notifications {
                // A notification tied to an instance focuses it; a generic event
                // just raises the window.
                let action = match n.instance {
                    Some(iid) => TrayAction::Focus(iid),
                    None => TrayAction::ShowWindow,
                };
                items.push(self.item(n.label.clone(), action));
            }
        }
        items.push(MenuItem::Separator);
        items.push(self.item(self.labels.quit.clone(), TrayAction::Quit));
        items
    }
}

struct KsniBackend {
    handle: Handle<MuxelTray>,
    actions: Actions,
}

impl TrayBackend for KsniBackend {
    fn update(&self, model: TrayModel) {
        // `None` when the service has already shut down — harmless to ignore.
        let _ = self.handle.update(move |t| t.model = model);
    }

    fn take_action(&self) -> Option<TrayAction> {
        self.actions.lock().ok()?.pop_front()
    }
}

impl Drop for KsniBackend {
    fn drop(&mut self) {
        // Best-effort: ask the service thread to remove the icon and exit.
        let _ = self.handle.shutdown();
    }
}

pub fn spawn(icon: TrayIcon, labels: TrayLabels) -> Option<Box<dyn TrayBackend>> {
    let actions: Actions = Arc::new(Mutex::new(VecDeque::new()));
    let pixmap = icon.rgba.as_ref().map(to_sni_icon).into_iter().collect();
    let tray = MuxelTray {
        model: TrayModel::default(),
        labels,
        tooltip: icon.tooltip,
        pixmap,
        actions: actions.clone(),
    };
    match tray.spawn() {
        Ok(handle) => Some(Box::new(KsniBackend { handle, actions })),
        Err(e) => {
            log::warn!("system tray unavailable: {e}");
            None
        }
    }
}
