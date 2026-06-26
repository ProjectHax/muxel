//! Manual smoke test: spawn the tray, hold it briefly, print whether it started.
//! `cargo run -p muxel-tray --example tray_smoke` — then check the bus for a
//! StatusNotifierItem. Not part of CI.

fn main() {
    let icon = muxel_tray::TrayIcon {
        name: "muxel".into(),
        tooltip: "muxel".into(),
        rgba: None,
    };
    let labels = muxel_tray::TrayLabels {
        show: "Show muxel".into(),
        quit: "Quit muxel".into(),
        agents: "Agents".into(),
        notifications: "Notifications".into(),
    };
    match muxel_tray::TrayController::spawn(icon, labels) {
        Some(t) => {
            println!("TRAY_SPAWNED");
            t.update(muxel_tray::TrayModel::default());
            std::thread::sleep(std::time::Duration::from_secs(4));
            println!("TRAY_DONE");
        }
        None => println!("TRAY_NONE"),
    }
}
