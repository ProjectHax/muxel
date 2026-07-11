//! URL helpers shared across the app and the browser panes.

/// Turn a typed/stored address into something a web view can load:
/// `example.com` → `https://example.com`. Already-schemed inputs
/// (`http://`, `https://`, `about:`, `file:`) are left untouched, and empty
/// input stays empty so the caller can decide a fallback.
pub fn normalize_url(input: &str) -> String {
    let s = input.trim();
    if s.is_empty()
        || s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("about:")
        || s.starts_with("file:")
    {
        s.to_string()
    } else {
        format!("https://{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn adds_scheme_to_bare_domain() {
        assert_eq!(normalize_url("duckduckgo.com"), "https://duckduckgo.com");
        assert_eq!(
            normalize_url("example.com/path?q=1"),
            "https://example.com/path?q=1"
        );
    }

    #[test]
    fn leaves_schemed_urls_untouched() {
        assert_eq!(normalize_url("https://x.com"), "https://x.com");
        assert_eq!(normalize_url("http://x.com"), "http://x.com");
        assert_eq!(normalize_url("about:blank"), "about:blank");
        assert_eq!(normalize_url("file:///tmp/x.html"), "file:///tmp/x.html");
    }

    #[test]
    fn trims_and_keeps_empty_empty() {
        assert_eq!(
            normalize_url("  duckduckgo.com  "),
            "https://duckduckgo.com"
        );
        assert_eq!(normalize_url(""), "");
        assert_eq!(normalize_url("   "), "");
    }
}
