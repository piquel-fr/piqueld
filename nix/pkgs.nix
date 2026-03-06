{ pkgs, ... }:
let
  root = ../.;
  pkg =
    name:
    let
      manifest = (pkgs.lib.importTOML (root + "/${name}/Cargo.toml")).package;
    in
    pkgs.rustPlatform.buildRustPackage {
      pname = manifest.name;
      version = manifest.version;
      src = pkgs.lib.cleanSource root;
      cargoLock.lockFile = root + "/Cargo.lock";
      cargoBuildFlags = [
        "--package"
        manifest.name
      ];
    };
in
rec {
  piqueld = pkg "piqueld";
  piquelctl = pkg "piquelctl";
  default = piquelctl;
}
