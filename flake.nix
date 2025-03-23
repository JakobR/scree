{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    flake-compat.url = "github:edolstra/flake-compat";
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
    ...
  }:
  flake-utils.lib.eachDefaultSystem (system:
    let
      pkgs = nixpkgs.legacyPackages.${system};
      craneLib = crane.mkLib pkgs;

      inherit (pkgs) lib;

      htmlFilter = path: _type: null != builtins.match ".*html$" path;
      htmlOrCargo = path: type: (htmlFilter path type) || (craneLib.filterCargoSources path type);
      src = lib.cleanSourceWith {
        src = craneLib.path ./.;
        filter = htmlOrCargo;
      };

      # Common arguments can be set here to avoid repeating them later
      commonArgs = {
        inherit src;
        strictDeps = true;

        nativeBuildInputs = with pkgs; [
          pkg-config
        ];

        buildInputs = with pkgs; [
          openssl
        ];

        # Additional environment variables can be set directly
        # MY_CUSTOM_VAR = "some value";
      };

      # Build *just* the cargo dependencies, so we can reuse
      # all of that work (e.g. via cachix) when running in CI
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      # Build the actual crate itself, reusing the dependency
      # artifacts from above.
      scree = craneLib.buildPackage (commonArgs // {
        inherit cargoArtifacts;
        name = "scree";
        postInstall = ''
          cp -r static $out/bin
        '';
      });
    in {

      checks = {
        # Build the crate as part of `nix flake check` for convenience
        inherit scree;
      };

      packages = {
        default = scree;
        inherit scree;
      };

      nixosModules.default = { config, lib, pkgs, ...}:
      with lib;
      let
        cfg = config.jr.services.scree;
      in
      {
        options.jr.services.scree = {
          enable = mkEnableOption "Scree service";

          database = mkOption {
            type = types.str;
            example = "host=/run/postgresql dbname=scree";
            description = "Database connection string. Note that the database must be configured separately.";
          };

          listen = mkOption {
            type = types.str;
            default = "127.0.0.1:3000";
            example = "/run/scree/scree.sock";
            description = "Socket address on which the service listens to HTTP connections (either IP:Port for a regular socket or an absolute path for a unix socket).";
          };

          setRealIp = mkOption {
            type = types.bool;
            default = false;
            description = "Obtain the client's remote IP address from the header 'x-real-ip'. Useful if connections go through a reverse proxy.";
          };

          user = mkOption {
            type = types.str;
            default = "scree";
            description = "User account under which the service runs.";
          };

          group = mkOption {
            type = types.str;
            default = "scree";
            description = "Group account under which the service runs.";
          };

          package = mkOption {
            type = types.package;
            default = self.packages.${pkgs.system}.default;
            description = "The package to use for this service.";
          };
        };

        config = mkIf cfg.enable {

          users.users = optionalAttrs (cfg.user == "scree") {
            scree = {
              group = cfg.group;
              isSystemUser = true;
            };
          };

          users.groups = optionalAttrs (cfg.group == "scree") {
            scree = {};
          };

          systemd.services.scree = {
            description = "Scree task monitoring";
            wantedBy = [ "multi-user.target" ];

            serviceConfig = {
              User = cfg.user;
              Group = cfg.group;
              ExecStart =
                "${cfg.package}/bin/scree --db ${escapeShellArg cfg.database} run " +
                "--listen ${escapeShellArg cfg.listen} " +
                (if cfg.setRealIp then "--set-real-ip " else "");
            };
          };

        };
      };

      devShells.default = craneLib.devShell {
        # Inherit inputs from checks.
        checks = self.checks.${system};

        # Additional dev-shell environment variables can be set directly
        # MY_CUSTOM_DEVELOPMENT_VAR = "something else";

        # Extra inputs can be added here; cargo and rustc are provided by default.
        packages = with pkgs; [
        ];
      };
    });
}
