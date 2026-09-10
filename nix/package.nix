{
  lib,
  stdenv,
  rustPlatform,
  pkg-config,
  cmake,
  fontconfig,
  freetype,
  wayland,
  libxkbcommon,
  libGL,
  xorg,
  alsa-lib,
  vulkan-loader,
  dbus,
  gtk3,
  webkitgtk_4_1,
  glib,
  wrapGAppsHook3,
  makeBinaryWrapper,
}:

rustPlatform.buildRustPackage rec {
  pname = "muxel";
  version = "0.1.9";

  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: type:
      let
        base = baseNameOf path;
      in
      # Drop bulky / irrelevant trees from the build sandbox.
      !(lib.hasInfix "/.git" path)
      && !(lib.hasInfix "/target" path)
      && !(lib.hasInfix "/ios" path)
      && !(lib.hasInfix "/.muxel-dev" path)
      && !(lib.hasInfix "/.hermes" path)
      && base != "result";
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
    allowBuiltinFetchGit = false;
    outputHashes = {
      # Filled in Task 5 from `nix build` errors. Keys are crate-version from Cargo.lock.
      # Unique git remotes (each crate from the same rev shares one hash):
      #   gpui-0.2.2                          zed cc053a4a
      #   gpui-component-0.5.2                longbridge 3cb3cc13
      #   wasm_thread-0.3.3
      #   zed-font-kit-0.14.1-zed
      #   zed-reqwest-0.12.15-zed
      #   zed-scap-0.0.8-zed
      #   xim-parser-0.2.1                    (xim-rs; also xim-ctext, zed-xim)
      "collections-0.1.0" = "sha256-d2GVmZgvJzLk1pbNtPedw0V09+ANZFORZjTSLVxw7jc=";
      "gpui-component-0.5.2" = "sha256-JMpc+UQpqyfxpRJ8GPR7vzjRyCaijQtLzYFVkrcDDsQ=";
      "wasm_thread-0.3.3" = "sha256-+lRLCIk0S6Y5ORYjDKsYYHia2FtoSoh+rWkQh7mnPBE=";
      "zed-font-kit-0.14.1-zed" = "sha256-KXygi0olNQi5yM8eaJVykNDtbPMDjT+cWPBF8UrtXR4=";
      "zed-reqwest-0.12.15-zed" = "sha256-p4SiUrOrbTlk/3bBrzN/mq/t+1Gzy2ot4nso6w6S+F8=";
      "zed-scap-0.0.8-zed" = "sha256-BihiQHlal/eRsktyf0GI3aSWsUCW7WcICMsC2Xvb7kw=";
      "xim-parser-0.2.1" = "sha256-pRT4Sz1JU9ros47/7pmIW9kosWOGMOItcnNd+VrvnpE=";
    };
  };

  nativeBuildInputs = [
    pkg-config
    cmake
    rustPlatform.bindgenHook
    wrapGAppsHook3
    makeBinaryWrapper
  ];

  buildInputs = [
    fontconfig
    freetype
    wayland
    libxkbcommon
    libGL
    xorg.libX11
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi
    xorg.libxcb
    alsa-lib
    vulkan-loader
    dbus
    gtk3
    webkitgtk_4_1
    glib
  ];

  # whisper-rs / tree-sitter invoke cmake from build.rs. The CMake setup hook
  # would otherwise try to configure the workspace root and fail.
  dontUseCmakeConfigure = true;

  # GUI crate only. Do not enable voice-local.
  cargoBuildFlags = [ "--package" "muxel" ];

  # Skip PTY/GUI tests in the sandbox. Pure crates are enough for the package.
  cargoTestFlags = [
    "--package"
    "muxel-core"
    "--package"
    "muxel-store"
  ];

  env = {
    ZSTD_SYS_USE_PKG_CONFIG = "1";
  };

  postInstall = ''
    install -Dm644 ${../packaging/muxel.desktop} \
      $out/share/applications/muxel.desktop
    install -Dm644 ${../crates/muxel/assets/muxel.svg} \
      $out/share/icons/hicolor/scalable/apps/muxel.svg
    install -Dm644 ${../crates/muxel/assets/muxel.png} \
      $out/share/icons/hicolor/256x256/apps/muxel.png
  '';

  preFixup = lib.optionalString stdenv.hostPlatform.isLinux ''
    gappsWrapperArgs+=(
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath [
        vulkan-loader
        wayland
        libGL
        libxkbcommon
      ]}
    )
  '';

  meta = {
    description = "A multi-agent terminal multiplexer for managing many AI coding agents across projects.";
    homepage = "https://github.com/projecthax/muxel";
    license = lib.licenses.gpl3Only;
    mainProgram = "muxel";
    platforms = lib.platforms.linux;
  };
}
