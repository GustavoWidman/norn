{
  description = "cloudflare dns for a nix fleet, declared next to the services that use it";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    flake-parts.url = "github:hercules-ci/flake-parts";
    devshell.url = "github:numtide/devshell";
  };

  outputs =
    inputs@{
      flake-parts,
      crane,
      fenix,
      devshell,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } (
      { self, flake-parts-lib, ... }:
      let
        common = flake-parts-lib.importApply ./nix/modules/common.nix { inherit self; };
      in
      {
        systems = [
          "aarch64-darwin"
          "aarch64-linux"
          "x86_64-linux"
        ];
        imports = [
          devshell.flakeModule
        ];

        flake = {
          nixosModules.default.imports = [
            common
            ./nix/modules/nixos.nix
          ];
          darwinModules.default.imports = [
            common
            ./nix/modules/darwin.nix
          ];
        };

        perSystem =
          {
            pkgs,
            system,
            lib,
            ...
          }:
          let
            toolchain = fenix.packages.${system}.fromToolchainFile {
              file = ./rust-toolchain.toml;
              sha256 = "sha256-3MyLNjhfHtXhOj0hzUe3wrwfX6h7B3JURIfNHuSu19w=";
            };

            craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;
            norn = pkgs.callPackage ./nix/package.nix { inherit craneLib; };
          in
          {
            packages.default = norn;

            checks = {
              inherit norn;
            }
            // norn.passthru.checks
            // import ./nix/checks.nix {
              inherit
                pkgs
                lib
                self
                norn
                ;
            };

            devshells.default = {
              packages = [
                toolchain
              ];

              motd = "";
            };
          };
      }
    );
}
