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
          ciArtifactsPath = builtins.getEnv "PIQUELD_CI_ARTIFACTS";
          ciArtifacts =
            if ciArtifactsPath == "" then
              null
            else
              builtins.path {
                path = ciArtifactsPath;
                name = "piqueld-ci-artifacts";
              };
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
          # PR checks already run the full Rust tests and production UI build
          # in their owning jobs. In CI, reuse those tested artifacts for the
          # VM so Nix only checks packaging/module wiring and VM behavior. The
          # fallback keeps local `nix flake check` self-contained.
          ci =
            if ciArtifacts == null then
              default.overrideAttrs (_: {
                pname = "piqueld-ci";
                cargoBuildType = "debug";
                cargoBuildFlags = [
                  "-p"
                  "piqueld"
                  "-p"
                  "piquelctl"
                ];
                doCheck = false;
                postBuild = "";
                buildPhase = ''
                  runHook preBuild
                  cargoBuildHook
                  runHook postBuild
                '';
                installPhase = ''
                  runHook preInstall
                  install -Dm755 target/${rustTarget}/debug/piqueld "$out/bin/piqueld"
                  install -Dm755 target/${rustTarget}/debug/piquelctl "$out/bin/piquelctl"
                  install -Dm644 config/piqueld.example.toml \
                    "$out/share/piqueld/piqueld.example.toml"
                  mkdir -p "$out/share/piqueld/ui"
                  wrapProgram "$out/bin/piqueld" \
                    --set PIQUELD_UI_DIR "$out/share/piqueld/ui"
                  runHook postInstall
                '';
              })
            else
              pkgs.runCommand "piqueld-ci"
                {
                  nativeBuildInputs = [
                    pkgs.makeWrapper
                    pkgs.patchelf
                  ];
                }
                ''
                  install -Dm755 ${ciArtifacts}/piqueld "$out/bin/piqueld"
                  install -Dm755 ${ciArtifacts}/piquelctl "$out/bin/piquelctl"
                  # Rust validation runs on Ubuntu, while the VM runs NixOS.
                  # Normalize the host-built ELF binaries before putting them
                  # in the Nix store so they use Nix's runtime libraries.
                  for binary in "$out/bin/piqueld" "$out/bin/piquelctl"; do
                    patchelf \
                      --set-interpreter "${pkgs.stdenv.cc.bintools.dynamicLinker}" \
                      --set-rpath "${
                        pkgs.lib.makeLibraryPath [
                          pkgs.glibc
                          pkgs.gcc.cc.lib
                        ]
                      }" \
                      "$binary"
                  done
                  install -Dm644 ${self}/config/piqueld.example.toml \
                    "$out/share/piqueld/piqueld.example.toml"
                  mkdir -p "$out/share/piqueld/ui"
                  cp -R ${ciArtifacts}/ui/. "$out/share/piqueld/ui/"
                  wrapProgram "$out/bin/piqueld" \
                    --set PIQUELD_UI_DIR "$out/share/piqueld/ui"
                '';
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
          moduleConfig =
            (nixpkgs.lib.nixosSystem {
              inherit system;
              modules = [
                (import ./nix/module.nix)
                {
                  services.piqueld = {
                    enable = true;
                    package = pkgs.emptyDirectory;
                    cliPackage = pkgs.emptyDirectory;
                    uiPackage = pkgs.emptyDirectory;
                  };
                  system.stateVersion = "26.05";
                }
              ];
            }).config.environment.etc."piqueld/config.toml".source;
          # The PR VM uses the focused CI package. The checked production
          # release remains covered by the release workflow.
          vmRelease = self.packages.${system}.ci;
          vmPackage = pkgs.runCommand "piqueld-daemon-vm-0.1.0" { } ''
            mkdir -p "$out/bin"
            cp ${vmRelease}/bin/piqueld "$out/bin/piqueld"
          '';
          vmCliPackage = pkgs.runCommand "piquelctl-vm-0.1.0" { } ''
            mkdir -p "$out/bin"
            cp ${vmRelease}/bin/piquelctl "$out/bin/piquelctl"
          '';
          vmUiPackage = pkgs.runCommand "piqueld-ui-vm-0.1.0" { } ''
            mkdir -p "$out/share/piqueld"
            cp -R ${vmRelease}/share/piqueld/ui "$out/share/piqueld/ui"
          '';
          vmTraefikStub = pkgs.writeShellScriptBin "piqueld-traefik-stub" ''
            while :; do
              /bin/sleep 3600
            done
          '';
          vmTraefikImage = pkgs.dockerTools.buildImage {
            name = "piqueld-traefik-vm";
            tag = "v0";
            copyToRoot = pkgs.buildEnv {
              name = "piqueld-traefik-vm-root";
              paths = [
                pkgs.busybox
                vmTraefikStub
              ];
              pathsToLink = [ "/bin" ];
            };
            config = {
              Entrypoint = [ "/bin/piqueld-traefik-stub" ];
            };
          };
        in
        {
          package = self.packages.${system}.ci;
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
          module-config = pkgs.runCommand "piqueld-module-config" { } ''
            if awk '
              /^\[/ { in_section = 1 }
              !in_section && /^data_dir[[:space:]]*=/ { found = 1 }
              END { exit !found }
            ' ${moduleConfig}; then
              echo "NixOS module emitted the removed top-level data_dir option" >&2
              exit 1
            fi
            grep -q '^path = "/var/lib/piqueld/piqueld.db"' ${moduleConfig}
            touch "$out"
          '';
          nixos-vm = pkgs.testers.runNixOSTest {
            name = "piqueld-module";
            nodes.machine =
              {
                config,
                lib,
                pkgs,
                ...
              }:
              {
                imports = [ self.nixosModules.default ];
                services.piqueld = {
                  enable = true;
                  package = vmPackage;
                  cliPackage = vmCliPackage;
                  uiPackage = vmUiPackage;
                  installCli = true;
                  dataDir = "/var/lib/piqueld-vm";
                  server.unixSocket = "/run/piqueld-vm/control.sock";
                  registry.address = "127.0.0.1:5050";
                  registry.dataDir = "/var/lib/piqueld-registry-vm";
                  metrics.enable = true;
                  credentials.masterKeyFile = "/run/keys/piqueld-master-key";
                  credentials.bearerTokenFile = "/run/keys/piqueld-bearer-token";
                  credentials.gitTokenFile = "/run/keys/piqueld-git-token";
                };
                # The test controls startup so credentials can be created
                # outside the Nix store before the daemon reads them.
                systemd.services.piqueld.wantedBy = lib.mkForce [ ];
                environment.systemPackages = [
                  pkgs.curl
                  pkgs.jq
                ];
                virtualisation.memorySize = 2048;
                virtualisation.diskSize = 4096;
              };
            testScript = ''
              start_all()
              machine.wait_for_unit("docker.service")
              machine.wait_for_unit("docker-registry.service")
              machine.succeed("docker load < ${vmTraefikImage}")
              machine.succeed("image_id=$(docker image inspect --format '{{.Id}}' piqueld-traefik-vm:v0 | cut -d: -f2); sed -i \"s#^image = .*#image = \\\"piqueld-traefik-vm:v0@sha256:$image_id\\\"#\" /etc/piqueld/config.toml")
              machine.succeed("install -d -m 0700 /run/keys")
              machine.succeed("head -c 32 /dev/urandom > /run/keys/piqueld-master-key && chmod 0600 /run/keys/piqueld-master-key")
              machine.succeed("printf '%s' 'vm-bearer-'$(printf 'canary') > /run/keys/piqueld-bearer-token && chmod 0600 /run/keys/piqueld-bearer-token")
              machine.succeed("printf '%s' 'vm-git-'$(printf 'canary') > /run/keys/piqueld-git-token && chmod 0600 /run/keys/piqueld-git-token")
              machine.succeed("systemctl start piqueld.service")
              machine.wait_for_unit("piqueld.service")
              machine.wait_until_succeeds("docker info --format '{{.Swarm.ControlAvailable}}' | grep true", timeout=60)
              machine.wait_until_succeeds("test -S /run/piqueld-vm/control.sock", timeout=30)
              machine.wait_until_succeeds("curl --fail --silent --unix-socket /run/piqueld-vm/control.sock http://localhost/api/v1/system/health | grep alive", timeout=30)
              machine.wait_until_succeeds("curl --fail --silent --unix-socket /run/piqueld-vm/control.sock http://localhost/api/v1/system/readiness | grep '\"ready\":true'", timeout=60)
              machine.succeed("curl --fail --silent --unix-socket /run/piqueld-vm/control.sock http://localhost/api/v1/system/metrics | grep '^piqueld_up 1$'")
              machine.succeed("test $(stat -c %a /var/lib/piqueld-vm) = 750")
              machine.succeed("test $(stat -c %a /run/piqueld-vm) = 750")
              machine.succeed("test ! -e ${vmPackage}/bin/piquelctl && test ! -e ${vmCliPackage}/bin/piqueld && test ! -e ${vmUiPackage}/bin/piqueld")
              machine.succeed("command -v piqueld >/dev/null && command -v piquelctl >/dev/null")
              machine.succeed("! grep -R --binary-files=without-match -F \"$(cat /run/keys/piqueld-bearer-token)\" /nix/store/*piqueld* 2>/dev/null")
              machine.succeed("! grep -R --binary-files=without-match -F \"$(cat /run/keys/piqueld-git-token)\" /nix/store/*piqueld* 2>/dev/null")
              machine.succeed("! journalctl -u piqueld.service --no-pager | grep -F \"$(cat /run/keys/piqueld-bearer-token)\"")
              machine.succeed("curl --fail --silent -H \"Authorization: Bearer $(cat /run/keys/piqueld-bearer-token)\" http://127.0.0.1:7845/api/v1/system/health | grep alive")
              machine.fail("curl --fail --silent http://127.0.0.1:7845/api/v1/system/health")
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
