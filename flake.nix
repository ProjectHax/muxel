{
  description = "muxel — a multi-agent terminal multiplexer for coding agents";

  # One input on purpose. Every extra flake input is another thing that drifts and
  # another lock entry to explain; `genAttrs` over a system list does everything
  # flake-utils would do here.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # Linux only. GPUI renders through Vulkan here and Metal on macOS, and the
      # macOS build also wants a code-signing/bundling step that nixpkgs' darwin
      # stdenv does not give us for free — packaging that properly is its own job,
      # so `meta.platforms` stays honest instead of promising a build we never ran.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        muxel = pkgs.callPackage ./nix/package.nix { };
        default = muxel;
      });

      apps = forAllSystems (pkgs: rec {
        muxel = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.muxel}/bin/muxel";
          # `nix flake check` warns about an app with no `meta`.
          meta = {
            inherit (self.packages.${pkgs.stdenv.hostPlatform.system}.muxel.meta)
              description
              license
              ;
          };
        };
        default = muxel;
      });

      # `nix develop` — the reason a NixOS contributor can build muxel at all.
      # Cargo finds no system libraries on NixOS, so a plain `cargo run -p muxel`
      # fails at link time without this; the shell carries the same native deps the
      # package builds against, plus the libraries GPUI loads at *runtime* through
      # `dlopen`, which no linker flag can supply.
      devShells = forAllSystems (pkgs: {
        default = pkgs.callPackage ./nix/shell.nix { };
      });

      # `nix flake check` builds the package and runs the workspace's pure tests.
      checks = forAllSystems (pkgs: {
        muxel = self.packages.${pkgs.stdenv.hostPlatform.system}.muxel;
      });

      overlays.default = final: _prev: {
        muxel = final.callPackage ./nix/package.nix { };
      };

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
