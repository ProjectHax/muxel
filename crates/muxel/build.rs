use std::process::Command;

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=MUXEL_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=MUXEL_BUILD_DIRTY");

    if let Some(git_dir) = git_output(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        println!("cargo:rerun-if-changed={git_dir}/index");
    }

    let commit = std::env::var("MUXEL_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| git_output(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = std::env::var("MUXEL_BUILD_DIRTY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            let clean = Command::new("git")
                .args(["diff", "--quiet", "--ignore-submodules", "HEAD", "--"])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if clean { "0" } else { "1" }.to_string()
        });
    println!("cargo:rustc-env=MUXEL_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=MUXEL_BUILD_DIRTY={dirty}");

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
