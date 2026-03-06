{ outputs, system, pkgs, ... }:
pkgs.mkShell {
  inputsFrom = [ outputs.packages.${system}.piquelctl ];
  packages = with pkgs; [
    cargo
    rustc
    rustfmt
    clippy
    rust-analyzer
  ];
}
