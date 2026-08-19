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
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          version = (pkgs.lib.importTOML ./Cargo.toml).workspace.package.version;
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "piqueld";
            inherit version;
            src = pkgs.lib.cleanSource self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [
              pkgs.binaryen
              pkgs.cmake
              pkgs.lld
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
            postInstall = ''
              mkdir -p "$out/share/piqueld/ui"
              cp -R "$TMPDIR/piqueld-ui-dist/." "$out/share/piqueld/ui/"
              # The placeholder piqueld-ui binary is not part of the product.
              rm -f "$out/bin/piqueld-ui"
            '';
            doCheck = true;
          };
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
                ];
                src = pkgs.lib.cleanSource self;
              }
              ''
                cp -R "$src" source
                chmod -R u+w source
                cd source
                bash scripts/check-dependency-boundaries.sh
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
            # The unpinned nixpkgs toolchain can differ from rust-toolchain.toml;
            # rustup users get the pinned one automatically inside the repo.
            packages = with pkgs; [
              cargo
              cargo-deny
              binaryen
              clippy
              just
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
