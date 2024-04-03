{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    systems.url = "github:nix-systems/default";

    # Dev tools
    treefmt-nix.url = "github:numtide/treefmt-nix";
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = import inputs.systems;
      imports = [
        inputs.treefmt-nix.flakeModule
      ];
      perSystem = { config, self', pkgs, lib, system, ... }:
        let
          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          nonRustDeps = [
            # pkgs.libiconv
            pkgs.libpulseaudio
          ];
          rust-toolchain = pkgs.symlinkJoin {
            name = "rust-toolchain";
            paths = [
              pkgs.cargo
              pkgs.cargo-outdated
              pkgs.cargo-watch
              pkgs.rust-analyzer
              pkgs.rustPlatform.rustcSrc
              pkgs.rustc
            ];
          };
        in
        {
          # Rust package
          packages.default = pkgs.rustPlatform.buildRustPackage {
            inherit (cargoToml.package) name version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            buildInputs = nonRustDeps;
            nativeBuildInputs = with pkgs; [
              pkg-config
            ];
          };

          # Rust dev environment
          devShells.default = pkgs.mkShell {
            inputsFrom = [
              config.treefmt.build.devShell
            ];
            shellHook = ''
              # For rust-analyzer 'hover' tooltips to work.
              export RUST_SRC_PATH=${pkgs.rustPlatform.rustLibSrc}

              echo
              echo "🍎🍎 Run 'just <recipe>' to get started"
              just
            '';
            buildInputs = nonRustDeps;
            nativeBuildInputs = with pkgs; [
              autoPatchelfHook
              pkg-config

              just
              rust-toolchain
            ];
            RUST_BACKTRACE = 1;
          };

          # Add your auto-formatters here.
          # cf. https://numtide.github.io/treefmt/
          treefmt.config = {
            projectRootFile = "flake.nix";
            programs = {
              nixpkgs-fmt.enable = true;
              rustfmt.enable = true;
            };
          };
        };
    };
}
