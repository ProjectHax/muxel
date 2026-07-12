//! The locale a spawned child inherits.
//!
//! A GUI app is not launched from a shell, so it has no locale unless the OS
//! session provides one — and on macOS none does: `launchctl getenv LANG` is
//! empty, so an app opened from Finder (or from a `.app` bundle) hands every
//! child it spawns an environment with no `LANG`, `LC_ALL` or `LC_CTYPE` at all.
//!
//! Programs that decide how to encode their output from the locale then fall back
//! to ASCII. tmux is the loud one: with no UTF-8 locale its client replaces every
//! non-ASCII cell with `_` on its way to the terminal, so box-drawing and glyphs
//! come out as garbage no redraw can fix.

/// The locale to fall back to. macOS ships no `C.UTF-8`, but `en_US.UTF-8` is
/// always present; elsewhere `C.UTF-8` is the locale-independent choice.
pub const FALLBACK_UTF8_LOCALE: &str = if cfg!(target_os = "macos") {
    "en_US.UTF-8"
} else {
    "C.UTF-8"
};

/// Whether a child spawned with these locale variables needs [`FALLBACK_UTF8_LOCALE`]
/// forced on it.
///
/// True only when the environment declares no locale whatsoever. A locale that is
/// set but not UTF-8 (`LANG=C`, say) is a deliberate choice and is left alone —
/// overriding it would be muxel second-guessing the user's environment. tmux is
/// covered there anyway: its client is launched with `-u`, which forces UTF-8
/// output regardless of the locale.
pub fn needs_utf8_locale(lc_all: Option<&str>, lc_ctype: Option<&str>, lang: Option<&str>) -> bool {
    // An empty value is how launchd reports "unset", so treat it as unset.
    [lc_all, lc_ctype, lang]
        .iter()
        .all(|v| v.is_none_or(|s| s.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::{FALLBACK_UTF8_LOCALE, needs_utf8_locale};

    #[test]
    fn an_environment_with_no_locale_at_all_needs_one() {
        assert!(needs_utf8_locale(None, None, None));
    }

    #[test]
    fn launchd_reports_unset_as_empty_which_is_still_no_locale() {
        assert!(needs_utf8_locale(Some(""), Some("  "), Some("")));
    }

    #[test]
    fn any_declared_locale_is_left_alone() {
        assert!(!needs_utf8_locale(None, None, Some("en_GB.UTF-8")));
        assert!(!needs_utf8_locale(None, Some("en_US.UTF-8"), None));
        assert!(!needs_utf8_locale(Some("de_DE.UTF-8"), None, None));
        // Deliberately non-UTF-8: the user's business, not ours. `tmux -u` keeps
        // tmux panes readable even here.
        assert!(!needs_utf8_locale(None, None, Some("C")));
    }

    #[test]
    fn the_fallback_is_a_utf8_locale() {
        assert!(FALLBACK_UTF8_LOCALE.contains("UTF-8"));
    }
}
