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
          lib = pkgs.lib;
          rustTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;
          mkPackage =
            {
              name,
              binaries,
              withUi,
            }:
            pkgs.rustPlatform.buildRustPackage {
              pname = name;
              version = "0.1.0";
              src = lib.cleanSource self;
              cargoLock.lockFile = ./Cargo.lock;
              # Only the combined package compiles the workspace UI crate: its
              # daemon enables the embedded dashboard feature and points the
              # daemon build script at a prebuilt Trunk distribution.
              cargoBuildFlags = lib.concatMap (binary: [
                "--package"
                binary
              ]) binaries
              ++ lib.optionals withUi [
                "--features"
                "embedded-ui"
              ];
              cargoTestFlags = lib.concatMap (binary: [
                "--package"
                binary
              ]) binaries;
              nativeBuildInputs = [
                pkgs.cmake
                pkgs.lld
                pkgs.pkg-config
                pkgs.rustPlatform.bindgenHook
              ]
              ++ lib.optionals withUi [
                pkgs.binaryen
                pkgs.tailwindcss_4
                pkgs.trunk
                pkgs.wasm-bindgen-cli_0_2_126
              ];
              # Compile SQLx SQLite query macros against a disposable database
              # provisioned by the daemon build script.
              DATABASE_URL = "sqlite::memory:";
              # The dashboard bundle must exist before the daemon build script
              # runs, so Trunk executes in preBuild and the distribution is
              # handed over through PIQUELD_UI_DIST instead of letting the
              # build script invoke tools inside the sandbox.
              preBuild = lib.optionalString withUi ''
                export HOME="$TMPDIR/trunk-home"
                mkdir -p "$HOME" apps/piqueld-ui/generated
                unset NO_COLOR
                tailwindcss \
                  --input apps/piqueld-ui/tailwind.css \
                  --output apps/piqueld-ui/generated/style.css --minify
                pushd apps/piqueld-ui
                trunk build index.html \
                  --release --offline=true --frozen \
                  --public-url /dashboard/ --dist "$TMPDIR/piqueld-ui-dist"
                popd
                export PIQUELD_UI_DIST="$TMPDIR/piqueld-ui-dist"
              '';
              installPhase = ''
                runHook preInstall
                ${lib.concatStringsSep "\n" (
                  map (
                    binary: ''install -Dm755 "target/${rustTarget}/release/${binary}" "$out/bin/${binary}"''
                  ) binaries
                )}
                install -Dm644 config/piqueld.example.toml \
                  "$out/share/piqueld/piqueld.example.toml"
                runHook postInstall
              '';
              doCheck = true;
            };
        in
        {
          cli = mkPackage {
            name = "piqueld-cli";
            binaries = [ "piquelctl" ];
            withUi = false;
          };
          daemon = mkPackage {
            name = "piqueld-daemon";
            binaries = [ "piqueld" ];
            withUi = false;
          };
          combined = mkPackage {
            name = "piqueld";
            binaries = [
              "piqueld"
              "piquelctl"
            ];
            withUi = true;
          };
          default = self.packages.${system}.combined;
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          package = self.packages.${system}.default;
          daemon-package = self.packages.${system}.daemon;
          cli-package = self.packages.${system}.cli;
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
              cargo-watch
              binaryen
              clippy
              just
              cmake
              lld
              pkg-config
              rustc
              rustfmt
              tailwindcss_4
              trunk
              wasm-bindgen-cli_0_2_126
            ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
