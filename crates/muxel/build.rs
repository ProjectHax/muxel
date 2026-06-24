fn main() {
    // Embed the app icon into the Windows .exe. Only runs when building on
    // Windows (CI builds the Windows targets on a windows runner); a no-op on
    // Linux/macOS, so it doesn't affect the .deb/.rpm/AppImage/.app builds.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/muxel.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/muxel.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=winresource (exe icon) failed: {e}");
        }
    }
}
