{
  description = "muxel — multi-agent terminal multiplexer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          muxel = pkgs.callPackage ./nix/package.nix { };
        in
        {
          inherit muxel;
          default = muxel;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.muxel}/bin/muxel";
        };
      });

      overlays.default = final: _prev: {
        muxel = final.callPackage ./nix/package.nix { };
      };
    };
}
