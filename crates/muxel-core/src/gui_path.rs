//! Reconstructing a usable `PATH` for GUI launches (macOS Dock/Finder, Linux
//! desktop entries / AppImages).
//!
//! A GUI launch inherits a minimal `PATH` that omits Homebrew, `~/.local/bin`,
//! `~/.opencode/bin`, and friends — exactly where coding agents (`claude`,
//! `opencode`, `amp`, …) tend to live. On macOS launchd hands a `.app` a bare
//! `/usr/bin:/bin:/usr/sbin:/sbin`; on Linux a desktop-entry / AppImage launch
//! likewise misses the dirs the login shell would have added from the user's
//! profile. Without those entries the agent picker hides installed agents and
//! direct agent spawns fail to resolve. A terminal launch doesn't hit this
//! because the login shell has already sourced the user's profile.
//!
//! These are pure string transforms; the app reads `$PATH`/`$HOME` and applies
//! the platform-appropriate result via `env::set_var` at startup (so the spawned
//! PTY children inherit the fixed-up `PATH` too).

/// Fixed system dirs a macOS GUI launch routinely drops. Ordered Homebrew-first
/// to match the precedence a login shell would set up.
const MACOS_GUI_PATH_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
];

/// Per-user bin dirs (resolved against `$HOME`) added for the same reason on macOS.
const MACOS_USER_SUBDIRS: &[&str] = &[".local/bin", "bin", ".cargo/bin"];

/// Fixed system dirs a Linux GUI/AppImage launch may drop: the usual
/// `/usr/local`, Linuxbrew, and snap's bin.
const LINUX_GUI_PATH_DIRS: &[&str] = &[
    "/usr/local/bin",
    "/usr/local/sbin",
    "/home/linuxbrew/.linuxbrew/bin",
    "/snap/bin",
];

/// Per-user bin dirs (resolved against `$HOME`) where coding agents commonly
/// install on Linux. `~/.opencode/bin` is opencode's installer default; the rest
/// cover bun/deno/`npm config set prefix` and pip `--user` / generic installs.
const LINUX_USER_SUBDIRS: &[&str] = &[
    ".local/bin",
    "bin",
    ".cargo/bin",
    ".opencode/bin",
    ".bun/bin",
    ".deno/bin",
    ".npm-global/bin",
];

/// Returns a `PATH` with the standard macOS GUI-launch dirs prepended, or `None`
/// when `current` already contains all of them (e.g. a terminal launch) so the
/// caller can skip the `set_var`.
pub fn augmented_macos_path(current: Option<&str>, home_dir: Option<&str>) -> Option<String> {
    augment(MACOS_GUI_PATH_DIRS, MACOS_USER_SUBDIRS, current, home_dir)
}

/// Linux counterpart of [`augmented_macos_path`] for desktop-entry / AppImage
/// launches — prepends the dirs where agents like opencode install. `None` when
/// `current` already has them all.
pub fn augmented_linux_path(current: Option<&str>, home_dir: Option<&str>) -> Option<String> {
    augment(LINUX_GUI_PATH_DIRS, LINUX_USER_SUBDIRS, current, home_dir)
}

/// Shared core: prepend the `system_dirs` and `$HOME/<user_subdirs>` that are
/// missing from `current`, preserving the listed order and the existing `PATH`
/// after them. Dirs are added unconditionally (no existence check) — a
/// non-existent `PATH` entry is harmless and keeps this pure. `None` when nothing
/// is missing.
fn augment(
    system_dirs: &[&str],
    user_subdirs: &[&str],
    current: Option<&str>,
    home_dir: Option<&str>,
) -> Option<String> {
    let mut candidates: Vec<String> = system_dirs.iter().map(|s| (*s).to_string()).collect();
    if let Some(home) = home_dir.filter(|h| !h.is_empty()) {
        let home = home.trim_end_matches('/');
        for sub in user_subdirs {
            candidates.push(format!("{home}/{sub}"));
        }
    }

    let existing: Vec<&str> = current.map(|p| p.split(':').collect()).unwrap_or_default();
    let present: std::collections::HashSet<&str> = existing.iter().copied().collect();

    let missing: Vec<String> = candidates
        .into_iter()
        .filter(|c| !present.contains(c.as_str()))
        .collect();
    if missing.is_empty() {
        return None;
    }

    let mut parts = missing;
    parts.extend(existing.into_iter().map(str::to_string));
    Some(parts.join(":"))
}

#[cfg(test)]
mod tests {
    use super::{augmented_linux_path, augmented_macos_path};

    #[test]
    fn prepends_missing_dirs_before_existing_path() {
        let out = augmented_macos_path(Some("/usr/bin:/bin"), Some("/Users/x")).unwrap();
        assert_eq!(
            out,
            "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:\
             /Users/x/.local/bin:/Users/x/bin:/Users/x/.cargo/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn returns_none_when_all_present() {
        // A terminal launch already has every dir → nothing to do.
        let current = "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:\
             /Users/x/.local/bin:/Users/x/bin:/Users/x/.cargo/bin:/usr/bin:/bin";
        assert_eq!(augmented_macos_path(Some(current), Some("/Users/x")), None);
    }

    #[test]
    fn adds_only_the_missing_dirs_without_duplicating() {
        // Homebrew already present; only the rest get prepended, once.
        let out =
            augmented_macos_path(Some("/opt/homebrew/bin:/usr/bin"), Some("/Users/x")).unwrap();
        assert_eq!(out.matches("/opt/homebrew/bin").count(), 1);
        assert!(out.starts_with("/opt/homebrew/sbin:"));
        assert!(out.ends_with(":/opt/homebrew/bin:/usr/bin"));
    }

    #[test]
    fn without_home_only_system_dirs_are_added() {
        let out = augmented_macos_path(Some("/usr/bin"), None).unwrap();
        assert_eq!(
            out,
            "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin"
        );
        assert!(!out.contains(".local/bin"));
    }

    #[test]
    fn empty_home_is_treated_as_absent() {
        let out = augmented_macos_path(Some("/usr/bin"), Some("")).unwrap();
        assert!(!out.contains(".local/bin"));
    }

    #[test]
    fn missing_path_yields_just_the_standard_dirs() {
        let out = augmented_macos_path(None, Some("/Users/x")).unwrap();
        assert!(out.starts_with("/opt/homebrew/bin:"));
        assert!(out.ends_with("/Users/x/.cargo/bin"));
    }

    #[test]
    fn trailing_slash_on_home_is_normalized() {
        let out = augmented_macos_path(Some("/usr/bin"), Some("/Users/x/")).unwrap();
        assert!(out.contains("/Users/x/.local/bin"));
        assert!(!out.contains("/Users/x//.local/bin"));
    }

    #[test]
    fn linux_adds_opencode_and_user_bins() {
        // A minimal desktop/AppImage PATH gains the dirs agents install into —
        // notably ~/.opencode/bin (opencode's installer default) and ~/.local/bin.
        let out = augmented_linux_path(Some("/usr/bin:/bin"), Some("/home/x")).unwrap();
        assert!(out.contains("/home/x/.opencode/bin"));
        assert!(out.contains("/home/x/.local/bin"));
        assert!(out.contains("/home/x/.bun/bin"));
        assert!(out.contains("/snap/bin"));
        // Missing dirs are prepended; the original PATH is preserved at the end.
        assert!(out.ends_with(":/usr/bin:/bin"));
    }

    #[test]
    fn linux_returns_none_when_all_present() {
        let current = "/usr/local/bin:/usr/local/sbin:/home/linuxbrew/.linuxbrew/bin:/snap/bin:\
             /home/x/.local/bin:/home/x/bin:/home/x/.cargo/bin:/home/x/.opencode/bin:\
             /home/x/.bun/bin:/home/x/.deno/bin:/home/x/.npm-global/bin:/usr/bin";
        assert_eq!(augmented_linux_path(Some(current), Some("/home/x")), None);
    }

    #[test]
    fn linux_does_not_duplicate_already_present_dirs() {
        // ~/.opencode/bin already on PATH → not re-added; only the rest prepend.
        let out =
            augmented_linux_path(Some("/home/x/.opencode/bin:/usr/bin"), Some("/home/x")).unwrap();
        assert_eq!(out.matches("/home/x/.opencode/bin").count(), 1);
        assert!(out.ends_with(":/home/x/.opencode/bin:/usr/bin"));
    }
}
