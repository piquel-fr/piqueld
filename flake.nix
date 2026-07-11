{
  description = "piqueld development environment and workspace checks";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in {
      packages = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "piqueld";
            version = "0.1.0";
            src = pkgs.lib.cleanSource self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.cmake pkgs.pkg-config pkgs.rustPlatform.bindgenHook ];
            doCheck = true;
          };
        });

      checks = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in {
          package = self.packages.${system}.default;
          formatting = pkgs.runCommand "piqueld-formatting" {
            nativeBuildInputs = [ pkgs.cargo pkgs.rustfmt ];
            src = pkgs.lib.cleanSource self;
          } ''
            cp -R "$src" source
            chmod -R u+w source
            cd source
            cargo fmt --check
            touch "$out"
          '';
          dependency-boundary = pkgs.runCommand "piqueld-dependency-boundary" {
            src = pkgs.lib.cleanSource self;
          } ''
            if grep -E '^(axum|bollard|leptos|libsql|sqlx)([.]workspace)?[[:space:]]*=' \
              "$src/crates/piqueld-core/Cargo.toml"; then
              echo "piqueld-core has a forbidden dependency" >&2
              exit 1
            fi
            touch "$out"
          '';
        });

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              cargo-deny
              clippy
              cmake
              pkg-config
              rustc
              rustfmt
            ];
          };
        });

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
