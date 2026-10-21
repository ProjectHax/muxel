//! Storage construction shared by the embedded browser and Linux helper.

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
pub(crate) fn unix_attributes(isolated: bool) -> wry::WebViewAttributes<'static> {
    // lb-wry 0.53.3 cannot redirect both WebKitGTK's data and cache through
    // WebContext. Darwin's identifier stores require macOS 14+ and otherwise
    // silently use the default store. Explicit incognito selects WebKit's
    // nonpersistent store on both backends, without any default-store fallback.
    // Each view has its own store; isolated Unix logins end with that view.
    wry::WebViewAttributes {
        incognito: isolated,
        ..Default::default()
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_context_dir(
    data_dir: Option<std::path::PathBuf>,
) -> std::io::Result<std::path::PathBuf> {
    let dir = data_dir
        .ok_or_else(|| std::io::Error::other("browser data directory unavailable"))?
        .join("webview2");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    #[test]
    fn isolated_unix_browser_never_constructs_default_persistent_storage() {
        let attrs = super::unix_attributes(true);
        assert!(attrs.incognito);
        assert!(attrs.context.is_none());
        assert!(!super::unix_attributes(false).incognito);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_browser_directory_errors_refuse_default_storage() {
        assert!(super::windows_context_dir(None).is_err());
        let root =
            std::env::temp_dir().join(format!("muxel-browser-storage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("webview2"), "blocked").unwrap();
        assert!(super::windows_context_dir(Some(root.clone())).is_err());
        std::fs::remove_file(root.join("webview2")).unwrap();
        assert_eq!(
            super::windows_context_dir(Some(root.clone())).unwrap(),
            root.join("webview2")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
