{
  description = "piqueld packages and development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.crane.url = "github:ipetkov/crane";
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
    }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      pkgsFor = system: nixpkgs.legacyPackages.${system}.extend (import rust-overlay);
      # The toolchain rust-toolchain.toml pins, with its components and
      # targets, so Nix builds use the same Rust as rustup and CI.
      toolchainFor = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
    in
    {
      nixosModules = {
        piqueld = { lib, pkgs, ... }: {
          imports = [ ./nix/module.nix ];
          services.piqueld.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.combined;
        };
        piquelctl = { lib, pkgs, ... }: {
          imports = [ ./nix/cli-module.nix ];
          programs.piquelctl.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.cli;
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
          pkgs = pkgsFor system;
          lib = pkgs.lib;
          craneLib = (crane.mkLib pkgs).overrideToolchain toolchainFor;
          # Keep documentation and workflow edits out of package source hashes.
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./apps
              ./crates
              ./migrations
              # A workspace member, so Cargo needs it to build anything.
              ./tools/xtask
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
          # The workspace with only `members` real and every other member
          # replaced by crane's dummy, which depends on manifests alone. A
          # package built from it rebuilds only when those members change.
          # Named like `src` so crane unpacks both into the same build path,
          # which its dependency artifacts' fingerprints require.
          memberSources =
            members:
            pkgs.runCommand "source" { } ''
              cp -r --no-preserve=mode ${craneLib.mkDummySrc { inherit src; }} $out
              cp -r --no-preserve=mode ${
                lib.fileset.toSource {
                  root = ./.;
                  fileset = lib.fileset.unions members;
                }
              }/. $out
            '';
          # The dashboard module the daemon embeds, built on its own so its
          # dependencies stay cached and daemon-only changes don't recompile
          # it. The daemon build script reads it from PIQUELD_DASHBOARD_WASM.
          # Like the build script's nested build, it ignores the release LTO
          # override.
          dashboardArgs = removeAttrs commonArgs [ "CARGO_PROFILE_RELEASE_LTO" ] // {
            pname = "piqueld-dashboard";
            cargoExtraArgs = "--locked --package piqueld-ui";
            CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
            CARGO_PROFILE = "dashboard";
          };
          dashboard = craneLib.buildPackage (
            dashboardArgs
            // {
              # Only the dashboard's own crates are real; the rest of the
              # workspace is crane's dummy, so daemon changes keep it cached.
              src = memberSources [
                # The real root manifest: members inherit its lints.
                ./Cargo.toml
                ./apps/piqueld-ui
                ./crates
              ];
              cargoArtifacts = buildDepsOnly dashboardArgs;
              installPhaseCommand = ''
                install -Dm644 target/wasm32-unknown-unknown/dashboard/piqueld-ui.wasm \
                  "$out/piqueld-ui.wasm"
              '';
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
              args =
                commonArgs
                // {
                  pname = name;
                  cargoExtraArgs =
                    "--locked "
                    + lib.concatMapStringsSep " " (binary: "--package ${binary}") binaries
                    + lib.optionalString withUi " --features embedded-ui";
                  CARGO_BUILD_TARGET = pkgs.stdenv.hostPlatform.rust.rustcTarget;
                }
                // lib.optionalAttrs withUi { PIQUELD_DASHBOARD_WASM = "${dashboard}/piqueld-ui.wasm"; };
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
                  wrapProgram "$out/bin/piqueld" --prefix PATH : ${
                    lib.makeBinPath [
                      pkgs.git
                      pkgs.docker-client
                    ]
                  }
                '';
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
          inherit dashboard;
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

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            packages =
              with pkgs;
              [
                (toolchainFor pkgs)
                cargo-nextest
                git
                just
                curl
                pkg-config
                openssl
              ]
              ++ lib.optionals stdenv.isLinux [
                cargo-deny
                docker-client
                cmake
                lld
                util-linux
                # Only for `just test-wasm`'s browser test runner; the daemon
                # build script binds the dashboard without the CLI.
                wasm-bindgen-cli_0_2_126
              ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
