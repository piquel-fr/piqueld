{
  description = "piqueld development environment and workspace checks";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      nixosModules.default =
        { lib, pkgs, ... }:
        {
          imports = [ (import ./nix/module.nix) ];
          services.piqueld.package = lib.mkDefault self.packages.${pkgs.system}.piqueld;
          services.piqueld.cliPackage = lib.mkDefault self.packages.${pkgs.system}.piquelctl;
          services.piqueld.uiPackage = lib.mkDefault self.packages.${pkgs.system}.piqueld-ui;
        };

      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          rustTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;
        in
        rec {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "piqueld";
            version = "0.1.0";
            src = pkgs.lib.cleanSource self;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "--workspace" ];
            nativeBuildInputs = [
              pkgs.binaryen
              pkgs.cmake
              pkgs.lld
              pkgs.makeWrapper
              pkgs.pkg-config
              pkgs.rustPlatform.bindgenHook
              pkgs.trunk
              pkgs.wasm-bindgen-cli_0_2_126
            ];
            # Compile SQLx SQLite query macros against a disposable database
            # provisioned by the daemon build script.
            DATABASE_URL = "sqlite::memory:";
            postBuild = ''
              export HOME="$TMPDIR/trunk-home"
              mkdir -p "$HOME"
              unset NO_COLOR
              pushd apps/piqueld-ui
              trunk build index.html \
                --release --offline=true --frozen \
                --public-url / --dist "$TMPDIR/piqueld-ui-dist"
              popd
            '';
            installPhase = ''
              runHook preInstall
              install -Dm755 target/${rustTarget}/release/piqueld "$out/bin/piqueld"
              install -Dm755 target/${rustTarget}/release/piquelctl "$out/bin/piquelctl"
              install -Dm644 config/piqueld.example.toml \
                "$out/share/piqueld/piqueld.example.toml"
              mkdir -p "$out/share/piqueld/ui"
              cp -R "$TMPDIR/piqueld-ui-dist/." "$out/share/piqueld/ui/"
              wrapProgram "$out/bin/piqueld" \
                --set PIQUELD_UI_DIR "$out/share/piqueld/ui"
              runHook postInstall
            '';
            doCheck = true;
          };
          release = default;
          piqueld = pkgs.runCommand "piqueld-daemon-0.1.0" { } ''
            mkdir -p "$out/bin"
            cp ${release}/bin/piqueld "$out/bin/piqueld"
          '';
          piquelctl = pkgs.runCommand "piquelctl-0.1.0" { } ''
            mkdir -p "$out/bin"
            cp ${release}/bin/piquelctl "$out/bin/piquelctl"
          '';
          piqueld-ui = pkgs.runCommand "piqueld-ui-0.1.0" { } ''
            mkdir -p "$out/share/piqueld"
            cp -R ${release}/share/piqueld/ui "$out/share/piqueld/ui"
          '';
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          package = self.packages.${system}.default;
          formatting =
            pkgs.runCommand "piqueld-formatting"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  pkgs.rustfmt
                ];
                src = pkgs.lib.cleanSource self;
              }
              ''
                cp -R "$src" source
                chmod -R u+w source
                cd source
                cargo fmt --check
                touch "$out"
              '';
          dependency-boundary =
            pkgs.runCommand "piqueld-dependency-boundary"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  pkgs.jq
                ];
                src = pkgs.lib.cleanSource self;
              }
              ''
                set -o pipefail
                cp -R "$src" source
                chmod -R u+w source
                cd source
                if cargo metadata --offline --no-deps --format-version 1 \
                  | jq -e '.packages[] | select(.name == "piqueld-core")
                    | any(.dependencies[];
                        .name == "axum" or .name == "bollard" or .name == "leptos"
                        or .name == "sqlx")'; then
                  echo "piqueld-core has a forbidden dependency" >&2
                  exit 1
                fi
                touch "$out"
              '';
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              cargo-deny
              binaryen
              clippy
              cmake
              lld
              pkg-config
              rustc
              rustfmt
              trunk
              wasm-bindgen-cli_0_2_126
            ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
