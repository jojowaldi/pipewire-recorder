{
  description = "Background PipeWire audio stream recorder with MPRIS metadata and FLAC tagging";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      ...
    }:
    (flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };

        pkg = pkgs.callPackage ./nix/package.nix { };
      in
      {
        packages = {
          default = pkg;
          pipewire-recorder = pkg;
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ pkg ];
          packages = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
            pkg-config
            pipewire
            wireplumber
            flac
            curl
          ];
        };
      }
    ))
    // {
      homeModules.default = import ./nix/home-module.nix self;
      nixosModules.default = import ./nix/nixos-module.nix self;
    };
}
