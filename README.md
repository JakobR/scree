# Scree!

Monitoring of periodically scheduled tasks


## NixOS Configuration

Integration into NixOS `configuration.nix` could look like this:

```nix
{ config, pkgs, ... }:

let
  # TODO: the flake should use the same nixpkgs as the rest of the system.
  #       see also https://github.com/NixOS/nix/pull/11952
  scree = (builtins.getFlake "github:JakobR/scree");
  screeModule = scree.nixosModules.default;
in {
  imports = [
    ./hardware-configuration.nix
    screeModule
  ];

  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  environment.systemPackages = with pkgs; [

    # ... (other packages)

    config.jr.services.scree.package
  ];


  # SSL certificates
  security.acme = {
    acceptTerms = true;
    defaults = {
      email = "my-email@example.com";
    };
  };

  services.nginx = {
    enable = true;
    recommendedTlsSettings = true;
    recommendedOptimisation = true;
    recommendedGzipSettings = true;
    recommendedProxySettings = true;
    commonHttpConfig = ''
      # HTTP Strict Transport Security for 365 days for HTTPS requests.
      # Adding this header to HTTP requests is discouraged.
      map $scheme $hsts_header {
        https   "max-age=31536000; includeSubdomains";
      }
      add_header Strict-Transport-Security $hsts_header;
    '';
    upstreams."scree".servers."unix:/run/scree/scree.sock" = {};
    virtualHosts = {
      "scree.example.com:80" = {
        serverName = "scree.example.com";
        enableACME = true;  # automatically adds this domain to security.acme.certs
        listen = [ { addr = "*"; port = 80; } ];
        locations."/".return = "301 https://$host$request_uri";
      };
      "scree.example.com" = {
        listen = [ { addr = "*"; ssl = true; } ];
        useACMEHost = "scree.example.com";  # use certificate generated from security.acme.certs
        onlySSL = true;
        locations."/ping" = {
          proxyPass = "http://scree";
        };
        locations."/" = {
          proxyPass = "http://scree";
          basicAuthFile = "/etc/nixos/secrets/htpasswd_scree";
        };
      };
    };
  };

  services.postgresql = {
    enable = true;
    package = pkgs.postgresql_17;
    ensureDatabases = [
      "scree"
    ];
    ensureUsers = [
      { name = "scree"; }
    ];
    authentication = pkgs.lib.mkOverride 10 ''
      # type  database  dbuser    auth-method
      local   all       postgres  peer
      local   scree     scree     peer
    '';
    initialScript = pkgs.writeText "postgres-init-script" ''
      ALTER DATABASE "scree" OWNER TO "scree";
    '';
  };

  jr.services.scree = {
    enable = true;
    database = "host=/run/postgresql dbname=scree";
    listen = "/run/scree/scree.sock";
    setRealIp = true;
  };

  systemd.tmpfiles.rules = [
    "d /run/scree 0750 scree nginx"
  ];

  # ... (other NixOS options)

}
```
