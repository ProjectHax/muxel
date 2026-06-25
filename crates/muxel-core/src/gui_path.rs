//! Reconstructing a usable `PATH` for GUI (Dock/Finder) launches on macOS.
//!
//! When a `.app` is launched from the Dock/Finder, launchd hands it a minimal
//! `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`) that omits Homebrew, `~/.local/bin`,
//! and friends — exactly where coding agents (`claude`, `opencode`, `amp`, …)
//! tend to live. Without those entries the agent picker hides installed agents
//! and direct agent spawns fail to resolve. A terminal launch doesn't hit this
//! because the login shell has already sourced the user's profile.
//!
//! This is the pure string transform; the app reads `$PATH`/`$HOME` and applies
//! the result via `env::set_var` at startup.

/// Fixed system dirs a macOS GUI launch routinely drops. Ordered Homebrew-first
/// to match the precedence a login shell would set up.
const MACOS_GUI_PATH_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
];

/// Per-user bin dirs (resolved against `$HOME`) added for the same reason.
const USER_PATH_SUBDIRS: &[&str] = &[".local/bin", "bin", ".cargo/bin"];

/// Returns a `PATH` with the standard macOS GUI-launch dirs prepended, or `None`
/// when `current` already contains all of them (e.g. a terminal launch) so the
/// caller can skip the `set_var`.
///
/// Only dirs missing from `current` are added, preserving the listed order and
/// the existing `PATH` after them. Dirs are added unconditionally (no existence
/// check) — a non-existent `PATH` entry is harmless and keeps this pure.
pub fn augmented_macos_path(current: Option<&str>, home_dir: Option<&str>) -> Option<String> {
    let mut candidates: Vec<String> = MACOS_GUI_PATH_DIRS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Some(home) = home_dir.filter(|h| !h.is_empty()) {
        let home = home.trim_end_matches('/');
        for sub in USER_PATH_SUBDIRS {
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
    use super::augmented_macos_path;

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
}
