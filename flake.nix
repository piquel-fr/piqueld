{
  description = "piqueld packages and development environment";

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
        "aarch64-darwin"
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

      darwinModules.piquelctl = self.nixosModules.piquelctl;

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
            nativeBuildInputs = lib.optionals pkgs.stdenv.isLinux [
              pkgs.cmake
              pkgs.lld
              pkgs.pkg-config
              pkgs.rustPlatform.bindgenHook
            ];
            buildInputs = [ pkgs.openssl ];
            DATABASE_URL = "sqlite::memory:";
            # Rust validation runs outside Nix; package builds only produce the
            # requested binaries.
            doCheck = false;
            # Keep release optimization, but avoid repeating whole-program LTO
            # for every package and test executable in native Nix builds.
            CARGO_PROFILE_RELEASE_LTO = "false";
          };
          # These artifacts feed builds, never `cargo check`. Crane's default
          # check pass compiles a separate set of metadata we don't use.
          buildDepsOnly =
            args:
            craneLib.buildDepsOnly (
              args
              // {
                buildPhaseCargoCommand = "cargoWithProfile build ${args.cargoExtraArgs}";
              }
            );
          wasmArgs = commonArgs // {
            pname = "piqueld-ui";
            cargoExtraArgs = "--locked --package piqueld-ui";
            CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
            doCheck = false;
          };
          uiDeps = buildDepsOnly wasmArgs;
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
          cliDeps = buildDepsOnly (
            commonArgs
            // {
              pname = "piqueld-cli";
              cargoExtraArgs = "--locked --package piquelctl";
              CARGO_BUILD_TARGET = pkgs.stdenv.hostPlatform.rust.rustcTarget;
            }
          );
          daemonDeps = buildDepsOnly (
            commonArgs
            // {
              pname = "piqueld-daemon";
              cargoExtraArgs = "--locked --package piqueld --package piquelctl --features embedded-ui";
              CARGO_BUILD_TARGET = pkgs.stdenv.hostPlatform.rust.rustcTarget;
            }
          );
          mkPackage =
            {
              name,
              binaries,
              withUi,
              cargoArtifacts,
            }:
            let
              args = commonArgs // {
                pname = name;
                cargoExtraArgs =
                  "--locked "
                  + lib.concatMapStringsSep " " (binary: "--package ${binary}") binaries
                  + lib.optionalString withUi " --features embedded-ui";
                CARGO_BUILD_TARGET = pkgs.stdenv.hostPlatform.rust.rustcTarget;
              };
            in
            craneLib.buildPackage (
              args
              // {
                inherit cargoArtifacts;
                meta.mainProgram = builtins.head binaries;
                cargoBuildExtraArgs = lib.concatMapStringsSep " " (binary: "--bin ${binary}") binaries;
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
            cargoArtifacts = cliDeps;
          };
          # The daemon depends on Linux; macOS exposes only the CLI.
          default =
            if pkgs.stdenv.isDarwin then self.packages.${system}.cli else self.packages.${system}.combined;
        }
        // lib.optionalAttrs pkgs.stdenv.isLinux {
          daemon = mkPackage {
            name = "piqueld-daemon";
            binaries = [ "piqueld" ];
            withUi = false;
            cargoArtifacts = daemonDeps;
          };
          combined = mkPackage {
            name = "piqueld";
            binaries = [
              "piqueld"
              "piquelctl"
            ];
            withUi = true;
            cargoArtifacts = daemonDeps;
          };
        }
      );

      checks.x86_64-linux.unix-socket = import ./nix/unix-socket-test.nix {
        pkgs = nixpkgs.legacyPackages.x86_64-linux;
        daemon = self.packages.x86_64-linux.daemon;
        cli = self.packages.x86_64-linux.cli;
      };

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            # The unpinned nixpkgs toolchain can differ from rust-toolchain.toml;
            # rustup users get the pinned one automatically inside the repo.
            packages =
              with pkgs;
              [
                cargo
                cargo-nextest
                clippy
                git
                just
                curl
                rustc
                rustfmt
                pkg-config
                openssl
              ]
              ++ lib.optionals stdenv.isLinux [
                cargo-deny
                cargo-watch
                binaryen
                docker-client
                cmake
                lld
                procps
                util-linux
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
