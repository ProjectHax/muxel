//! Windows / macOS tray backend via `tray-icon` (notification-area icon / status-
//! bar item).
//!
//! Built on the app's main thread — `spawn`/`update` run inside the GPUI tick, so
//! the icon shares the app's native event loop (NSStatusItem requires the main
//! thread; the Win32 message pump delivers its messages). Menu clicks arrive on
//! muda's global `MenuEvent` channel, drained by `take_action`.
//!
//! NOTE: developed on Linux and not runtime-verified on these platforms. If init
//! fails, `spawn` returns `None` and the app keeps its normal close/quit behavior.

use std::cell::RefCell;
use std::collections::HashMap;

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use super::{TrayAction, TrayBackend, TrayIcon as TraySpec, TrayLabels, TrayModel};

struct DesktopBackend {
    tray: TrayIcon,
    labels: TrayLabels,
    /// Menu-item id → action, rebuilt on each `update`.
    map: RefCell<HashMap<MenuId, TrayAction>>,
}

/// Append one row; record its id→action when clickable (a `None` action is a
/// disabled section header).
fn push_item(
    menu: &Menu,
    map: &mut HashMap<MenuId, TrayAction>,
    label: &str,
    action: Option<TrayAction>,
) {
    let item = MenuItem::new(label, action.is_some(), None);
    if let Some(a) = action {
        map.insert(item.id().clone(), a);
    }
    let _ = menu.append(&item);
}

impl DesktopBackend {
    fn build_menu(&self, model: &TrayModel) -> Menu {
        let menu = Menu::new();
        let mut map = self.map.borrow_mut();
        map.clear();
        push_item(
            &menu,
            &mut map,
            &self.labels.show,
            Some(TrayAction::ShowWindow),
        );
        if !model.agents.is_empty() {
            let _ = menu.append(&PredefinedMenuItem::separator());
            push_item(&menu, &mut map, &self.labels.agents, None);
            for a in &model.agents {
                push_item(&menu, &mut map, &a.label, Some(TrayAction::Focus(a.iid)));
            }
        }
        if !model.notifications.is_empty() {
            let _ = menu.append(&PredefinedMenuItem::separator());
            push_item(&menu, &mut map, &self.labels.notifications, None);
            for n in &model.notifications {
                let action = match n.instance {
                    Some(iid) => TrayAction::Focus(iid),
                    None => TrayAction::ShowWindow,
                };
                push_item(&menu, &mut map, &n.label, Some(action));
            }
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
        push_item(&menu, &mut map, &self.labels.quit, Some(TrayAction::Quit));
        menu
    }
}

impl TrayBackend for DesktopBackend {
    fn update(&self, model: TrayModel) {
        let menu = self.build_menu(&model);
        self.tray.set_menu(Some(Box::new(menu)));
    }

    fn take_action(&self) -> Option<TrayAction> {
        // Global channel shared by all menus; muxel has only this one.
        let ev = MenuEvent::receiver().try_recv().ok()?;
        self.map.borrow().get(&ev.id).copied()
    }
}

pub fn spawn(icon: TraySpec, labels: TrayLabels) -> Option<Box<dyn TrayBackend>> {
    let mut builder = TrayIconBuilder::new().with_tooltip(icon.tooltip.as_str());
    if let Some(rgba) = icon.rgba {
        if let Ok(ic) = Icon::from_rgba(rgba.bytes, rgba.width, rgba.height) {
            builder = builder.with_icon(ic);
        }
    }
    // Start with an empty menu; the first `update` fills it in.
    let tray = builder.with_menu(Box::new(Menu::new())).build().ok()?;
    Some(Box::new(DesktopBackend {
        tray,
        labels,
        map: RefCell::new(HashMap::new()),
    }))
}
