{
  description = "Closet development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    ...
  }: let
    systems = ["aarch64-darwin" "aarch64-linux" "x86_64-linux"];
    forAllSystems = nixpkgs.lib.genAttrs systems;
  in {
    packages = forAllSystems (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      rustPlatform = pkgs.makeRustPlatform {
        cargo = toolchain;
        rustc = toolchain;
      };
      cli = rustPlatform.buildRustPackage {
        pname = "ha-closet";
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml ./Cargo.lock ./src ./migrations ./web
            ./config ./tests
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;
        cargoBuildFlags = ["--bin" "ha-closet"];
        # The real-binary HTTP tests run in the package build, too.
        cargoTestFlags = ["--test" "cli"];
        # The fixture servers bind only to loopback during the check phase.
        __darwinAllowLocalNetworking = true;
        nativeBuildInputs = [pkgs.pkg-config];
        meta = {
          description = "Wardrobe inventory client";
          license = pkgs.lib.licenses.mit;
          mainProgram = "ha-closet";
          platforms = systems;
        };
      };
    in {
      ha-closet = cli;
      default = cli;
    });
    apps = forAllSystems (system: let
      app = {
        type = "app";
        meta.description = "Wardrobe inventory client";
        program = "${self.packages.${system}.ha-closet}/bin/ha-closet";
      };
    in {
      ha-closet = app;
      default = app;
    });
    devShells = forAllSystems (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
    in {
      default = pkgs.mkShell {
        packages = with pkgs; [
          (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
          python314
          uv
          cmake
          pkg-config
          sqlite
          jq
          just
          actionlint
          shellcheck
          cargo-deny
        ];
      };
    });
  };
}
