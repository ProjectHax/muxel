# The native dependencies muxel builds and runs against, in one place so the
# package and the dev shell can never drift apart.
#
# This list mirrors the `apt-get install` in `.github/workflows/ci.yml`, which is
# the authoritative set — if you add a system library there, add it here too.
{
  rustPlatform,
  pkg-config,
  cmake,
  fontconfig,
  freetype,
  wayland,
  wayland-protocols,
  wayland-scanner,
  libxkbcommon,
  libGL,
  libx11,
  libxcb,
  libxcursor,
  libxrandr,
  libxi,
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
    # `whisper-rs-sys` generates its FFI bindings with bindgen, which needs
    # libclang at *build* time and finds it through `LIBCLANG_PATH`. This hook sets
    # that and the clang include paths; without it the build dies with "Unable to
    # find libclang". This is what the `clang` in CI's apt list is really for —
    # nothing here is compiled with clang, it is bindgen that needs the library.
    rustPlatform.bindgenHook
  ];

  buildInputs = [
    fontconfig
    freetype
    wayland
    wayland-protocols
    libxkbcommon
    libGL
    libx11
    libxcb
    # Not in CI's apt list — CI never opens a window — but an X11 session needs
    # them the moment muxel actually draws one.
    libxcursor
    libxrandr
    libxi
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
    libx11
    libxcb
    libxcursor
    libxrandr
    libxi
  ];
}
