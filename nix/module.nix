{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.piqueld;
  configuration = (pkgs.formats.toml { }).generate "piqueld.toml" (
    lib.recursiveUpdate cfg.settings {
      server.data_dir = cfg.dataDir;
    }
  );
in
{
  options.services.piqueld = {
    enable = lib.mkEnableOption "the single-node piqueld control plane";
    package = lib.mkOption {
      type = lib.types.package;
      description = "Daemon package, optionally with its embedded dashboard.";
    };
    cliPackage = lib.mkOption {
      type = lib.types.package;
      description = "Package containing piquelctl.";
    };
    installCli = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Install piquelctl system-wide.";
    };
    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/piqueld";
      description = "Private state directory below /var/lib; includes the database and Unix API socket.";
    };
    settings = lib.mkOption {
      type = (pkgs.formats.toml { }).type;
      default = {
        server.http_listen = "127.0.0.1:7845";
      };
      description = "Daemon configuration. dataDir controls server.data_dir. Credentials must not be placed here: these settings enter the Nix store. Set server = {} to omit TCP when replacing these defaults.";
    };
  };
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          lib.hasPrefix "/var/lib/" cfg.dataDir
          && lib.all (part: part != "" && part != "." && part != "..") (
            lib.splitString "/" (lib.removePrefix "/var/lib/" cfg.dataDir)
          );
        message = "services.piqueld.dataDir must be a dedicated directory below /var/lib";
      }
    ];
    users.groups.piqueld = { };
    users.users.piqueld = {
      isSystemUser = true;
      group = "piqueld";
      extraGroups = [ "docker" ];
      home = cfg.dataDir;
    };
    virtualisation.docker.enable = true;
    environment.systemPackages = lib.optional cfg.installCli cfg.cliPackage;
    systemd.services.piqueld = {
      description = "piqueld application control plane";
      wantedBy = [ "multi-user.target" ];
      after = [ "docker.service" ];
      requires = [ "docker.service" ];
      path = [
        pkgs.git
        pkgs.openssh
        pkgs.docker-client
      ];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/piqueld --config ${configuration}";
        User = "piqueld";
        Group = "piqueld";
        SupplementaryGroups = [ "docker" ];
        StateDirectory = lib.removePrefix "/var/lib/" cfg.dataDir;
        StateDirectoryMode = "0700";
        UMask = "0077";
        Restart = "on-failure";
        RestartSec = "5s";
        TimeoutStopSec = "180s";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        ReadWritePaths = [ cfg.dataDir ];
      };
    };
  };
}
