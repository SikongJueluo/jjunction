{
  description = "jjunction — a collection of tools for the Jujutsu (jj) version control system";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    devenv = {
      url = "github:cachix/devenv";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  nixConfig = {
    extra-substituters = [ "https://devenv.cachix.org" ];
    extra-trusted-public-keys = [
      "devenv.cachix.org-1:w1cLUi8dv3hnoSPGAuibQv+f9TZLr6cv/Hm9XgU50cw="
    ];
  };

  outputs =
    { self, nixpkgs, devenv, ... }@inputs:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: nixpkgs.legacyPackages.${system};
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          jjunction = pkgs.callPackage ./nix/packages/jjunction.nix { root = self; };

          default = self.packages.${system}.jjunction;
        }
      );

      apps = forAllSystems (system: {
        jjn = {
          type = "app";
          program = nixpkgs.lib.getExe self.packages.${system}.jjunction;
        };
        default = self.apps.${system}.jjn;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = devenv.lib.mkShell {
            inherit inputs pkgs;
            modules = [
              (import ./devenv.nix)
              {
                # flake check evaluates without a usable PWD; pin the root
                # so the devenv module can resolve project files.
                devenv.root = self.outPath;
              }
            ];
          };
        }
      );

      nixosModules = {
        jjunction = import ./nix/modules/jjunction-system.nix;
        default = self.nixosModules.jjunction;
      };

      homeManagerModules = {
        jjunction = import ./nix/modules/jjunction-home.nix;
        default = self.homeManagerModules.jjunction;
      };
    };
}
