{
  description = "piqueld development environment and workspace checks";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.crane.url = "github:ipetkov/crane";

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      supportedSystems = [
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      nixosModules = {
        piqueld = { lib, pkgs, ... }: {
          imports = [ ./nix/module.nix ];
          services.piqueld.package = lib.mkDefault self.packages.${pkgs.system}.combined;
        };
        piquelctl = { lib, pkgs, ... }: {
          imports = [ ./nix/cli-module.nix ];
          programs.piquelctl.package = lib.mkDefault self.packages.${pkgs.system}.cli;
        };
        default =
          { config, lib, ... }:
          {
            imports = [
              self.nixosModules.piqueld
              self.nixosModules.piquelctl
            ];
            programs.piquelctl.enable = lib.mkDefault config.services.piqueld.enable;
          };
      };

      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          lib = pkgs.lib;
          craneLib = crane.mkLib pkgs;
          # Keep documentation and workflow edits out of package source hashes.
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./apps
              ./crates
              ./migrations
              ./examples
            ];
          };
          commonArgs = {
            inherit src;
            version = "0.1.0";
            cargoVendorDir = craneLib.vendorCargoDeps { inherit src; };
            nativeBuildInputs = [
              pkgs.cmake
              pkgs.lld
              pkgs.pkg-config
              pkgs.rustPlatform.bindgenHook
            ];
            nativeCheckInputs = [ pkgs.git ];
            DATABASE_URL = "sqlite::memory:";
            # Keep release optimization, but avoid repeating whole-program LTO
            # for every package and test executable in native Nix builds.
            CARGO_PROFILE_RELEASE_LTO = "false";
          };
          wasmArgs = commonArgs // {
            pname = "piqueld-ui";
            cargoExtraArgs = "--locked --package piqueld-ui";
            CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
            doCheck = false;
          };
          uiDeps = craneLib.buildDepsOnly wasmArgs;
          uiFiles = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./apps/piqueld-ui
              ./crates/piqueld-client
              ./crates/piqueld-core
            ];
          };
          # Cargo still needs valid daemon/CLI workspace members, but their
          # implementation must not invalidate the dashboard distribution.
          uiSrc = craneLib.mkDummySrc {
            inherit src;
            extraDummyScript = ''
              # Real UI manifests inherit workspace lints removed by dummification.
              install -m644 ${./Cargo.toml} "$out/Cargo.toml"
              for member in apps/piqueld-ui crates/piqueld-client crates/piqueld-core; do
                rm -rf "$out/$member"
                cp -R ${uiFiles}/$member "$out/$member"
              done
            '';
          };
          ui = craneLib.buildTrunkPackage (
            wasmArgs
            // {
              src = uiSrc;
              cargoArtifacts = uiDeps;
              wasm-bindgen-cli = pkgs.wasm-bindgen-cli_0_2_126;
              nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.tailwindcss_4 ];
              trunkExtraBuildArgs = "--offline=true --frozen --public-url /dashboard/";
              preBuild = ''
                unset NO_COLOR
                mkdir -p apps/piqueld-ui/generated
                tailwindcss --input apps/piqueld-ui/tailwind.css \
                  --output apps/piqueld-ui/generated/style.css --minify
                cd apps/piqueld-ui
              '';
            }
          );
          mkPackage =
            {
              name,
              binaries,
              withUi,
            }:
            let
              args = commonArgs // {
                pname = name;
                cargoExtraArgs =
                  "--locked "
                  + lib.concatMapStringsSep " " (binary: "--package ${binary}") binaries
                  + lib.optionalString withUi " --features embedded-ui";
                # Use the same flags for dependency compilation and real tests.
                CARGO_BUILD_TARGET = pkgs.stdenv.hostPlatform.rust.rustcTarget;
              };
              cargoArtifacts = craneLib.buildDepsOnly args;
            in
            craneLib.buildPackage (
              args
              // {
                inherit cargoArtifacts;
                # generate_openapi is tested, but is not a shipped binary.
                cargoBuildExtraArgs = lib.concatMapStringsSep " " (binary: "--bin ${binary}") binaries;
                # Nix sandbox ownership prevents this host-only startup test.
                cargoTestExtraArgs = lib.optionalString (builtins.elem "piqueld" binaries) "-- --skip=competing_daemon_preserves_database_and_live_socket";
                nativeBuildInputs =
                  commonArgs.nativeBuildInputs ++ lib.optional (builtins.elem "piqueld" binaries) pkgs.makeWrapper;
                installPhaseCommand = ''
                  ${lib.concatMapStringsSep "\n" (
                    binary:
                    ''install -Dm755 "target/${args.CARGO_BUILD_TARGET}/release/${binary}" "$out/bin/${binary}"''
                  ) binaries}
                  ${lib.optionalString (builtins.elem "piqueld" binaries) ''
                    install -Dm644 examples/piqueld.toml \
                      "$out/share/piqueld/piqueld.example.toml"
                  ''}
                '';
                postInstall = lib.optionalString (builtins.elem "piqueld" binaries) ''
                  wrapProgram "$out/bin/piqueld" --prefix PATH : ${lib.makeBinPath [ pkgs.git ]}
                '';
              }
              // lib.optionalAttrs withUi {
                PIQUELD_UI_DIST = ui;
              }
            );
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
          # CI roots these build-time inputs explicitly before cache GC.
          dependencies = pkgs.linkFarm "piqueld-dependencies" [
            {
              name = "cli";
              path = self.packages.${system}.cli.cargoArtifacts;
            }
            {
              name = "daemon";
              path = self.packages.${system}.daemon.cargoArtifacts;
            }
            {
              name = "combined";
              path = self.packages.${system}.combined.cargoArtifacts;
            }
            {
              name = "ui";
              path = uiDeps;
            }
          ];
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          nixos-service = import ./nix/vm-test.nix {
            inherit pkgs;
            module = self.nixosModules.default;
            daemon = self.packages.${system}.daemon;
            cli = self.packages.${system}.cli;
          };
          package = self.packages.${system}.default;
          daemon-package = self.packages.${system}.daemon;
          cli-package = self.packages.${system}.cli;
          formatting =
            pkgs.runCommand "piqueld-formatting"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  pkgs.rustfmt
                  pkgs.nixfmt
                ];
                src = pkgs.lib.cleanSource self;
              }
              ''
                cp -R "$src" source
                chmod -R u+w source
                cd source
                cargo fmt --check
                nixfmt --check flake.nix nix/ci.nix nix/vm-test.nix
                touch "$out"
              '';
          # cargo tree must resolve the crates.io dependency graph, so the
          # check vendors all sources up front and stays sandbox-safe.
          dependency-boundary = pkgs.stdenv.mkDerivation {
            name = "piqueld-dependency-boundary";
            src = pkgs.lib.cleanSource self;
            nativeBuildInputs = [
              pkgs.cargo
              pkgs.rustPlatform.cargoSetupHook
            ];
            cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
              name = "piqueld-dependency-boundary-deps";
              src = pkgs.lib.cleanSource self;
              hash = "sha256-PsiPM+QJ1eFNfsBeS7awo3RkRYJS26gD6SqFgHafmeI=";
            };
            dontConfigure = true;
            buildPhase = ''
              bash scripts/check-dependency-boundaries.sh
            '';
            installPhase = ''
              touch "$out"
            '';
          };
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
              cargo-nextest
              cargo-watch
              binaryen
              clippy
              docker-client
              git
              just
              cmake
              lld
              pkg-config
              procps
              util-linux
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
