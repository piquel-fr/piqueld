# CI may inherit the previous successful build's Cargo artifacts. They are
# immutable Nix store inputs; normal flake builds still use dependency-only
# artifacts. Workspace timestamps force Cargo to validate the current sources.
{
  source,
  system,
  artifactsFile ? "/nix/var/piqueld-artifacts.json",
}:
let
  flake = builtins.getFlake source;
  pkgs = flake.inputs.nixpkgs.legacyPackages.${system};
  lib = pkgs.lib;
  previous =
    if builtins.pathExists artifactsFile then
      builtins.fromJSON (builtins.readFile artifactsFile)
    else
      { };
  packages = lib.genAttrs [ "cli" "daemon" "combined" ] (
    name:
    let
      package = flake.packages.${system}.${name};
      cached = previous.${name} or "";
    in
    package.overrideAttrs (old: {
      cargoArtifacts =
        if cached != "" && builtins.pathExists cached then
          builtins.storePath cached
        else
          old.cargoArtifacts;
      outputs = [
        "out"
        "artifacts"
      ];
      CARGO_INCREMENTAL = "1";
      CARGO_BUILD_INCREMENTAL = "true";
      # Source and archive timestamps otherwise both normalize to epoch 1.
      postPatch = (old.postPatch or "") + ''
        find . -type f -exec touch -d @2 {} +
      '';
      # Keep snapshots self-contained rather than retaining every old archive.
      doCompressAndInstallFullArchive = true;
      postInstall = (old.postInstall or "") + ''
        prepareAndInstallCargoArtifactsDir "$artifacts"
      '';
    })
  );
  checks = flake.checks.${system};
  vm = import (flake + "/nix/vm-test.nix") {
    inherit pkgs;
    module = flake.nixosModules.default;
    daemon = packages.daemon;
    cli = packages.cli;
  };
in
pkgs.linkFarm "piqueld-ci" (
  lib.concatMap
    (name: [
      {
        inherit name;
        path = packages.${name};
      }
      {
        name = "${name}-artifacts";
        path = packages.${name}.artifacts;
      }
    ])
    [
      "cli"
      "daemon"
      "combined"
    ]
  ++ [
    {
      name = "dashboard";
      path = flake.packages.${system}.combined.PIQUELD_UI_DIST;
    }
    {
      name = "formatting";
      path = checks.formatting;
    }
    {
      name = "dependency-boundary";
      path = checks.dependency-boundary;
    }
    {
      name = "dependencies";
      path = flake.packages.${system}.dependencies;
    }
    {
      name = "nixos-service";
      path = vm;
    }
  ]
)
