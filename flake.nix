{
    description = "piqueld";
    
    inputs = {
        nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-25.11";
        flake-utils.url = "github:numtide/flake-utils";
    };
    
    outputs = { self, nixpkgs, flake-utils }: 
    flake-utils.lib.eachDefaultSystem (system:
        let
            inherit (self) outputs;
            pkgs = import nixpkgs {inherit system;};
        in
        {
            packages = import ./nix/pkgs.nix { inherit pkgs; };
            devShells.default = import ./nix/shell.nix { inherit outputs system pkgs; };
        }
    );
}
