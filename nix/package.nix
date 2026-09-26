{
  lib,
  callPackage,
  rustPlatform,
  wrapGAppsHook3,
}:

let
  deps = callPackage ./deps.nix { };
  # Read the version from the workspace rather than repeating it. A hardcoded
  # version silently goes stale the next release and then ships a package that
  # lies about what it contains.
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "muxel";
  version = cargoToml.workspace.package.version;

  src = lib.fileset.toSource {
    root = ../.;
    # An allowlist, not a denylist: anything not named here is out, so a new build
    # artifact directory can't quietly start busting the source hash the way an
    # `ignore this, ignore that` filter lets it.
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../rust-toolchain.toml
      ../crates
      ../packaging/muxel.desktop
    ];
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
    # muxel depends on seven git remotes (gpui and gpui_platform from zed, four
    # crates from gpui-component, plus zed's forks of font-kit, reqwest, scap,
    # wasm_thread and xim-rs). Pinning each one's narHash by hand means every gpui
    # bump — which muxel does regularly — breaks this file until someone runs the
    # build, reads the mismatch, and pastes new hashes in. `Cargo.lock` already
    # pins every one of those to an exact rev, so let Nix fetch by that rev and
    # keep the reproducibility guarantee without the manual bookkeeping.
    allowBuiltinFetchGit = true;
  };

  inherit (deps) buildInputs;
  nativeBuildInputs = deps.nativeBuildInputs ++ [
    # The browser pane and the tray pull in GTK/WebKit, which need their schemas
    # and loaders wired up or they abort at startup.
    wrapGAppsHook3
  ];

  # `cmake` is here for whisper-rs and tree-sitter, which invoke it from their own
  # build scripts. Without this the cmake setup hook tries to configure the
  # workspace root as a cmake project first, and fails — there is no CMakeLists.txt.
  dontUseCmakeConfigure = true;

  # Only the GUI binary. `muxel-tray` is a library the binary pulls in, so it is
  # built either way; `voice-local` is off by default and stays off — it compiles a
  # neural TTS voice that nothing in a packaged build needs.
  #
  # Note that offline speech-to-text is *not* optional: `whisper-rs` is a plain
  # dependency on every target except Windows-on-ARM, so this build does compile
  # whisper.cpp through cmake. That is why cmake is a build input rather than
  # something a feature flag could avoid.
  cargoBuildFlags = [
    "--package"
    "muxel"
  ];

  # Only the crates whose tests are pure. `muxel-terminal` spawns real PTYs and
  # `muxel` needs a display, neither of which a build sandbox reliably has — and a
  # package build is the wrong place to discover that. The full suite runs in CI.
  cargoTestFlags = [
    "--package"
    "muxel-core"
    "--package"
    "muxel-store"
  ];

  env = {
    # Link zstd from nixpkgs instead of building the vendored copy.
    ZSTD_SYS_USE_PKG_CONFIG = "1";
  };

  postInstall = ''
    install -Dm644 packaging/muxel.desktop \
      $out/share/applications/muxel.desktop
    install -Dm644 crates/muxel/assets/muxel.svg \
      $out/share/icons/hicolor/scalable/apps/muxel.svg
    install -Dm644 crates/muxel/assets/muxel.png \
      $out/share/icons/hicolor/256x256/apps/muxel.png
  '';

  # GPUI `dlopen`s Vulkan, Wayland and libGL, so they are absent from the binary's
  # rpath however it was linked. `wrapGAppsHook3` builds the wrapper; this appends
  # to the arguments it uses.
  preFixup = ''
    gappsWrapperArgs+=(
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath deps.runtimeLibraries}
    )
  '';

  meta = {
    description = "Multi-agent terminal multiplexer for running coding agents side by side";
    longDescription = ''
      A tiled, tabbed workspace for running coding agents (Claude, opencode, Amp, …)
      and shells side by side, with first-class git worktrees, agent status
      tracking and notifications. Each pane embeds a real terminal emulator running
      a PTY child process.
    '';
    homepage = "https://muxel.sh";
    downloadPage = "https://github.com/ProjectHax/muxel/releases";
    changelog = "https://github.com/ProjectHax/muxel/blob/master/CHANGELOG.md";
    license = lib.licenses.gpl3Only;
    mainProgram = "muxel";
    platforms = lib.platforms.linux;
  };
}
