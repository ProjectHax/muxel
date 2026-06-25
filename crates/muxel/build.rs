fn main() {
    // Bump the Windows main-thread stack to 8 MiB. The MSVC linker defaults the
    // executable's stack reserve to 1 MiB, but GPUI's layout/paint recurse deep
    // enough — especially when rendering a full-screen agent TUI (e.g. Claude) —
    // to overflow that, crashing with a "stack overflow". Linux/macOS default the
    // main thread to 8 MiB, so this only bites on Windows. gpui-component and Zed
    // do the same: gpui-component sets `/STACK:8000000` in its own
    // `.cargo/config.toml`, but that does NOT propagate to downstream crates, so
    // muxel must set it on its own binary. `/STACK` is MSVC-linker syntax, so gate
    // on the windows-msvc target (these CARGO_CFG_* vars describe the target).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg=/STACK:8388608");
    }

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
