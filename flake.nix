{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      craneLib = crane.mkLib pkgs;

      inherit (pkgs) lib;

      htmlFilter = path: _type: null != builtins.match ".*html$" path;
      htmlOrCargo = path: type: (htmlFilter path type) || (craneLib.filterCargoSources path type);
      src = lib.cleanSourceWith {
        src = craneLib.path ./.;
        filter = htmlOrCargo;
      };

      scree = craneLib.buildPackage {
        inherit src;
        name = "scree";
        postInstall = ''
          cp -r static $out/bin
        '';
      };
    in {
      packages.default = scree;
      packages.scree = scree;

      devShells.default = pkgs.mkShell {
        packages = [
            pkgs.nil
        ];
      };
    });
}
