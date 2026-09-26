# The native dependencies muxel builds and runs against, in one place so the
# package and the dev shell can never drift apart.
#
# This list mirrors the `apt-get install` in `.github/workflows/ci.yml`, which is
# the authoritative set — if you add a system library there, add it here too.
{
  pkg-config,
  cmake,
  fontconfig,
  freetype,
  wayland,
  wayland-protocols,
  wayland-scanner,
  libxkbcommon,
  libGL,
  xorg,
  alsa-lib,
  vulkan-loader,
  dbus,
  gtk3,
  webkitgtk_4_1,
  glib,
  openssl,
}:

{
  nativeBuildInputs = [
    pkg-config
    # whisper-rs and tree-sitter both drive cmake from their build scripts.
    cmake
    # `wayland-sys`/`wayland-scanner` generate protocol bindings at build time.
    wayland-scanner
  ];

  buildInputs = [
    fontconfig
    freetype
    wayland
    wayland-protocols
    libxkbcommon
    libGL
    xorg.libX11
    xorg.libxcb
    # Not in CI's apt list — CI never opens a window — but an X11 session needs
    # them the moment muxel actually draws one.
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi
    alsa-lib
    vulkan-loader
    dbus
    # The browser pane is a real WebKit child window; the tray talks D-Bus.
    gtk3
    webkitgtk_4_1
    glib
    openssl
  ];

  # Libraries GPUI resolves with `dlopen` at runtime rather than linking against.
  # A binary that links fine will still abort on startup without these on the
  # library path, which is why they are called out separately: the package puts
  # them in its wrapper and the dev shell puts them in `LD_LIBRARY_PATH`.
  runtimeLibraries = [
    vulkan-loader
    wayland
    libGL
    libxkbcommon
    xorg.libX11
    xorg.libxcb
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi
  ];
}
