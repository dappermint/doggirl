{
  description = "Nix development harness, package wrapper, and nix-darwin / Home Manager modules for doggirl (Ladybird fork)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];

      forAllSystems =
        f:
        lib.genAttrs systems (
          system:
          let
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            };
            rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          in
          f { inherit system pkgs rustToolchain; }
        );

      doggirlLib = import ./nix/lib { inherit lib self; };
      darwinModule = import ./nix/modules/darwin.nix { inherit doggirlLib; };
      homeManagerModule = import ./nix/modules/home-manager.nix { inherit doggirlLib; };
    in
    {
      lib = doggirlLib;

      overlays.default = final: prev: {
        inherit doggirlLib;
        doggirl-config-apply = final.callPackage ./nix/packages/config-apply.nix { };
        doggirl = doggirlLib.mkDoggirlWrapper final { };
        doggirl-from-source =
          args:
          let
            rustToolchain =
              if final ? rust-bin then
                final.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
              else
                prev.rustc;
          in
          doggirlLib.mkDoggirlFromSource final ({ inherit rustToolchain; } // args);
      };

      darwinModules = {
        default = darwinModule;
        doggirl = darwinModule;
      };

      homeManagerModules = {
        default = homeManagerModule;
        doggirl = homeManagerModule;
      };

      packages = forAllSystems (
        { pkgs, ... }:
        let
          doggirl = doggirlLib.mkDoggirlWrapper pkgs { };
          doggirl-config-apply = pkgs.callPackage ./nix/packages/config-apply.nix { };
        in
        {
          default = doggirl;
          inherit doggirl doggirl-config-apply;
        }
      );

      apps = forAllSystems (
        { system, ... }:
        let
          app = {
            type = "app";
            program = "${self.packages.${system}.doggirl}/bin/doggirl";
          };
        in
        {
          default = app;
          doggirl = app;
        }
      );

      devShells = forAllSystems (
        { pkgs, rustToolchain, ... }:
        let
          doggirlConfigApply = pkgs.callPackage ./nix/packages/config-apply.nix { };
        in
        {
          default = import ./nix/devshell.nix {
            inherit pkgs rustToolchain doggirlConfigApply;
          };
        }
      );

      formatter = forAllSystems (
        { pkgs, ... }:
        pkgs.writeShellApplication {
          name = "doggirl-nixfmt";
          runtimeInputs = [ pkgs.nixfmt ];
          text = ''
            if [ "$#" -gt 0 ]; then
              exec nixfmt "$@"
            fi
            files=(flake.nix)
            while IFS= read -r file; do
              files+=("$file")
            done < <(find nix -name '*.nix' | sort)
            exec nixfmt "''${files[@]}"
          '';
        }
      );

      checks = forAllSystems (
        { pkgs, ... }:
        let
          doggirlConfigApply = pkgs.callPackage ./nix/packages/config-apply.nix { };
        in
        import ./nix/checks.nix {
          inherit
            pkgs
            lib
            self
            doggirlLib
            darwinModule
            homeManagerModule
            doggirlConfigApply
            ;
        }
      );
    };
}
