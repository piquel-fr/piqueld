{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.piqueld;
  configuration = (pkgs.formats.toml { }).generate "piqueld.toml" (
    lib.recursiveUpdate (lib.filterAttrsRecursive (_: value: value != null) cfg.settings) {
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
      default = cfg.enable;
      defaultText = lib.literalExpression "config.services.piqueld.enable";
      description = "Install piquelctl system-wide, independently of the daemon.";
    };
    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/piqueld";
      description = "Private state directory below /var/lib; includes the database and Unix API socket.";
    };
    settings = lib.mkOption {
      type = lib.types.submodule {
        options = {
          server.http_listen = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            example = "127.0.0.1:7845";
            description = "Optional loopback HTTP listener. Null disables TCP; enabling it grants every local user unauthenticated API access.";
          };
          docker.socket = lib.mkOption {
            type = lib.types.strMatching "/.+";
            default = "/var/run/docker.sock";
            description = "Absolute path to the Docker Engine Unix socket.";
          };
          docker.auto_initialize_swarm = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Initialize an inactive Docker Engine as a single-node Swarm.";
          };
          reconciliation.scan_interval_seconds = lib.mkOption {
            type = lib.types.ints.between 1 86400;
            default = 60;
            description = "Seconds between full drift scans.";
          };
          reconciliation.prepare_timeout_seconds = lib.mkOption {
            type = lib.types.ints.between 1 86400;
            default = 300;
            description = "Timeout in seconds for resolving application inputs.";
          };
          reconciliation.convergence_timeout_seconds = lib.mkOption {
            type = lib.types.ints.between 1 86400;
            default = 120;
            description = "Timeout in seconds for runtime convergence.";
          };
          retention.finished_operation_days = lib.mkOption {
            type = lib.types.ints.unsigned;
            default = 10;
            description = "Days to retain finished operations; zero disables pruning.";
          };
          retention.event_days = lib.mkOption {
            type = lib.types.ints.unsigned;
            default = 30;
            description = "Days to retain informational events; zero disables pruning.";
          };
        };
      };
      default = { };
      description = "Typed daemon TOML settings. dataDir controls server.data_dir. Never put credentials here: these settings enter the Nix store.";
    };
  };
  config = lib.mkMerge [
    { environment.systemPackages = lib.optional cfg.installCli cfg.cliPackage; }
    (lib.mkIf cfg.enable {
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
    })
  ];
}
