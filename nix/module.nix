{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.piqueld;
  usesTailscale = builtins.elem cfg.settings.server.listen_mode [
    "tailscale"
    "both"
  ];
  configuration = (pkgs.formats.toml { }).generate "piqueld.toml" (
    lib.recursiveUpdate (lib.filterAttrsRecursive (_: value: value != null) cfg.settings) {
      server.data_dir = cfg.dataDir;
      server.runtime_dir = cfg.runtimeDir;
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
    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/piqueld";
      description = "Private state directory below /var/lib; contains the database and user data.";
    };
    runtimeDir = lib.mkOption {
      type = lib.types.str;
      default = "/run/piqueld";
      description = "Runtime directory below /run containing piqueld.sock. Group members have full operator access.";
    };
    settings = lib.mkOption {
      type = lib.types.submodule {
        options = {
          server.listen_mode = lib.mkOption {
            type = lib.types.enum [
              "off"
              "localhost"
              "tailscale"
              "both"
            ];
            default = "off";
            description = "HTTP listen interfaces. Every caller able to reach the listener has full operator access.";
          };
          server.port = lib.mkOption {
            type = lib.types.ints.between 1 65535;
            default = 7845;
            description = "Shared HTTP port for all selected IPv4 and IPv6 addresses.";
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
          build_history.log_max_bytes = lib.mkOption {
            type = lib.types.ints.between 1 67108864;
            default = 4194304;
            description = "Maximum persisted output bytes per build.";
          };
          build_history.log_retention_days = lib.mkOption {
            type = lib.types.ints.between 1 3650;
            default = 30;
            description = "Days to retain output after a build completes.";
          };
          retention.event_days = lib.mkOption {
            type = lib.types.ints.unsigned;
            default = 30;
            description = "Days to retain informational events; zero disables pruning.";
          };
        };
      };
      default = { };
      description = "Typed daemon TOML settings. dataDir and runtimeDir control server.data_dir and server.runtime_dir. Never put credentials here: these settings enter the Nix store.";
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
      {
        assertion =
          lib.hasPrefix "/run/" cfg.runtimeDir
          && lib.all (part: part != "" && part != "." && part != "..") (
            lib.splitString "/" (lib.removePrefix "/run/" cfg.runtimeDir)
          );
        message = "services.piqueld.runtimeDir must be a dedicated directory below /run";
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
      after = [ "docker.service" ] ++ lib.optional usesTailscale "tailscaled.service";
      requires = [ "docker.service" ];
      path = [
        pkgs.git
        pkgs.openssh
        pkgs.docker-client
      ]
      ++ lib.optional usesTailscale config.services.tailscale.package;
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/piqueld --config ${configuration}";
        User = "piqueld";
        Group = "piqueld";
        SupplementaryGroups = [ "docker" ];
        StateDirectory = lib.removePrefix "/var/lib/" cfg.dataDir;
        StateDirectoryMode = "0700";
        RuntimeDirectory = lib.removePrefix "/run/" cfg.runtimeDir;
        RuntimeDirectoryMode = "0750";
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
        ReadWritePaths = [
          cfg.dataDir
          cfg.runtimeDir
        ];
      };
    };
  };
}
