{ flake }:
{ config, lib, pkgs, ...}:
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

    unixSocketMode = mkOption {
      type = types.str;
      default = "700";
      description = "Permissions of the unix socket, if one is used.";
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
      default = flake.packages.${pkgs.system}.default;
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
          "--unix-socket-mode ${escapeShellArg cfg.unixSocketMode} " +
          (if cfg.setRealIp then "--set-real-ip " else "");
        Restart = "on-failure";
        RestartSec = "5s";
      };
    };

  };
}
