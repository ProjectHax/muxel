# `nix develop` — a shell where `cargo run -p muxel` works on NixOS.
#
# The package derivation is not enough for contributors: it builds a fixed source
# tree, and what a NixOS user actually needs is their own checkout to link and run.
{
  lib,
  mkShell,
  callPackage,
  rustc,
  cargo,
  rustfmt,
  clippy,
  rust-analyzer,
  git,
  tmux,
}:

let
  deps = callPackage ./deps.nix { };
  toolchain = lib.importTOML ../rust-toolchain.toml;
in
mkShell {
  # The same native dependencies the package builds against, so a checkout links
  # exactly as the packaged build does.
  inherit (deps) buildInputs;
  nativeBuildInputs = deps.nativeBuildInputs ++ [
    rustc
    cargo
    rustfmt
    clippy
    rust-analyzer
    git
    # muxel drives tmux for persistent and remote panes; without it those panes
    # simply fail to launch, which is a confusing first impression.
    tmux
  ];

  # GPUI `dlopen`s these, so nothing on the link line pulls them in and a freshly
  # built binary aborts at startup without them here.
  LD_LIBRARY_PATH = lib.makeLibraryPath deps.runtimeLibraries;

  shellHook = ''
    echo "muxel dev shell — cargo $(cargo --version | cut -d' ' -f2), rustc $(rustc --version | cut -d' ' -f2)"
  '' + lib.optionalString (toolchain ? toolchain && toolchain.toolchain ? channel) ''
    # rust-toolchain.toml pins a channel for rustup users. Nix supplies its own
    # rustc, so the pin is not honoured here — say so rather than letting a version
    # difference turn into a confusing compile error. `rustup` is deliberately
    # absent: with it on PATH, cargo would read the pin and try to download a
    # non-NixOS binary that will not run.
    if [ "$(rustc --version | cut -d' ' -f2)" != "${toolchain.toolchain.channel}" ]; then
      echo "note: rust-toolchain.toml pins ${toolchain.toolchain.channel}; this shell has $(rustc --version | cut -d' ' -f2) from nixpkgs."
      echo "      If the workspace needs something newer, bump nixpkgs in flake.nix."
    fi
  '';
}
