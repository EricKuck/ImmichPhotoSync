{
  description = "Headless macOS daemon that syncs the Apple Photos library to an Immich server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      forEachSystem = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forEachSystem (pkgs: rec {
        immich-photo-sync = pkgs.callPackage ./package.nix { };
        default = immich-photo-sync;
      });

      overlays.default = final: _prev: { immich-photo-sync = final.callPackage ./package.nix { }; };

      formatter = forEachSystem (pkgs: pkgs.nixfmt);
    };
}
