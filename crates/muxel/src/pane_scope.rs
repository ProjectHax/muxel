//! Linux: start local panes in systemd scopes of their own (see
//! `muxel_core::pane_scope` for why), and read what happened in them.
//!
//! Whether `systemd-run --user --scope` works is probed once, by running
//! `true` through it. Without a systemd user manager (a container, another init,
//! a session without `XDG_RUNTIME_DIR`), or with `MUXEL_PANE_SCOPE=0`, panes
//! start as they always have. Elsewhere than Linux this does nothing.

use std::collections::HashMap;
use uuid::Uuid;

/// Turns pane scopes off when set to `0`, for diagnosing a launch.
const DISABLE_ENV: &str = "MUXEL_PANE_SCOPE";

/// The argv to put in front of a local pane's program, or `None` to start it
/// directly. `limit` is the pane memory limit in bytes; `None` sets none.
pub fn prefix(instance: Uuid, limit: Option<u64>) -> Option<Vec<String>> {
    available().then(|| {
        let unit = muxel_core::pane_scope::unit_name(instance, Uuid::new_v4());
        muxel_core::pane_scope::scope_argv(&unit, limit)
    })
}

/// What each pane's scopes (all its launches still running) report about
/// running out of memory, by pane.
pub fn oom_counts() -> HashMap<Uuid, muxel_core::pane_scope::OomCounts> {
    let mut kills = HashMap::new();
    if !available() {
        return kills;
    }
    let Some(slice) = app_slice() else {
        return kills;
    };
    let Ok(entries) = std::fs::read_dir(slice) else {
        return kills;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(instance) = name.to_str().and_then(muxel_core::pane_scope::instance_of) else {
            continue;
        };
        if let Ok(events) = std::fs::read_to_string(entry.path().join("memory.events")) {
            *kills.entry(instance).or_default() += muxel_core::pane_scope::oom_counts(&events);
        }
    }
    kills
}

/// Start the probe in the background, so the first pane launch doesn't wait on
/// it.
pub fn warm() {
    let _ = std::thread::Builder::new()
        .name("muxel-pane-scope-probe".into())
        .spawn(|| {
            available();
        });
}

/// Whether panes can be started in scopes here. Probed once.
fn available() -> bool {
    #[cfg(all(target_os = "linux", not(test)))]
    {
        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            if std::env::var_os(DISABLE_ENV).is_some_and(|v| v == "0") {
                log::info!("pane scopes: off ({DISABLE_ENV}=0)");
                return false;
            }
            let works = probe();
            if !works {
                log::warn!(
                    "pane scopes: `systemd-run --user --scope` doesn't work here; panes start without one"
                );
            }
            works
        })
    }
    #[cfg(not(all(target_os = "linux", not(test))))]
    {
        let _ = DISABLE_ENV;
        false
    }
}

/// Run `true` the way a pane would be run, within a few seconds.
#[cfg(all(target_os = "linux", not(test)))]
fn probe() -> bool {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut argv = muxel_core::pane_scope::scope_argv(
        &muxel_core::pane_scope::unit_name(Uuid::nil(), Uuid::new_v4()),
        None,
    );
    argv.push("true".into());
    let Ok(mut child) = muxel_core::child_env::command(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// The user manager's `app.slice` cgroup directory, where `systemd-run --user`
/// puts scopes.
fn app_slice() -> Option<std::path::PathBuf> {
    // `0::/user.slice/user-1000.slice/user@1000.service/…` for muxel itself.
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = own.lines().find_map(|line| line.strip_prefix("0::"))?;
    let mut manager = std::path::PathBuf::from("/sys/fs/cgroup");
    for part in path.split('/').filter(|p| !p.is_empty()) {
        manager.push(part);
        if part.starts_with("user@") && part.ends_with(".service") {
            return Some(manager.join("app.slice"));
        }
    }
    None
}
