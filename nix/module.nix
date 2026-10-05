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
  withoutNulls = lib.filterAttrsRecursive (_: value: value != null);
  tailscale = cfg.settings.tailscale;
  destinations = cfg.settings.notifications.destinations;
  # `_file` settings name host files. systemd copies each into the unit's
  # private $CREDENTIALS_DIRECTORY, so the files may stay root-only, and the
  # daemon resolves the credential name there.
  webhookCredential = index: "webhook-${toString index}";
  # Every unit loading `configuration` needs these, since the daemon resolves
  # credential-backed settings while reading it.
  credentials =
    lib.optional (tailscale.auth_key_file != null) "ts-auth-key:${tailscale.auth_key_file}"
    ++ lib.concatLists (
      lib.imap0 (
        index: destination:
        lib.optional (destination.url_file != null) "${webhookCredential index}:${destination.url_file}"
      ) destinations
    );
  configuration = (pkgs.formats.toml { }).generate "piqueld.toml" (
    lib.recursiveUpdate (withoutNulls cfg.settings) (
      {
        server.data_dir = cfg.dataDir;
        server.runtime_dir = cfg.runtimeDir;
        notifications.destinations = lib.imap0 (
          index: destination:
          withoutNulls (
            destination
            // lib.optionalAttrs (destination.url_file != null) { url_file = webhookCredential index; }
          )
        ) destinations;
      }
      // lib.optionalAttrs (tailscale.auth_key_file != null) {
        tailscale.auth_key_file = "ts-auth-key";
      }
    )
  );
in
{
  imports = [
    (lib.mkRemovedOptionModule [ "services" "piqueld" "notificationDestinationsFile" ]
      "Declare destinations in services.piqueld.settings.notifications.destinations, using url_file for private URLs."
    )
  ];
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
      description = "Runtime directory below /run containing piqueld.sock. Group members can connect; account authentication is still required.";
    };
    backup = {
      enable = lib.mkEnableOption "scheduled backups with `piqueld backup`, safe while the daemon runs";
      schedule = lib.mkOption {
        type = lib.types.str;
        default = "daily";
        description = "systemd OnCalendar expression; missed runs start at the next boot.";
      };
      directory = lib.mkOption {
        type = lib.types.strMatching "/.+";
        default = "/var/backups/piqueld";
        description = "Private directory receiving timestamped archives. Archives contain the secret master key; copy them off the host.";
      };
      keep = lib.mkOption {
        type = lib.types.ints.positive;
        default = 7;
        description = "Number of newest archives to retain in the directory.";
      };
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
            description = "HTTP listen interfaces. Account authentication is required on every listener.";
          };
          server.allowed_hosts = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            description = "Additional trusted DNS hostnames for HTTP requests, without schemes or ports. IP literals and localhost are always allowed.";
          };
          server.port = lib.mkOption {
            type = lib.types.ints.between 1 65535;
            default = 7845;
            description = "Shared HTTP port for all selected IPv4 and IPv6 addresses.";
          };
          auth.public_url = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Canonical HTTPS website origin for passkeys; HTTP localhost is allowed for development. Defaults to the tailnet node's HTTPS URL when tailscale.enabled is set, otherwise http://localhost:7845.";
          };
          tailscale.enabled = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Join the tailnet as a dedicated node and serve the website over HTTPS on its port 443, with a tailnet-issued certificate. Requires HTTPS certificates to be enabled for the tailnet.";
          };
          tailscale.hostname = lib.mkOption {
            type = lib.types.strMatching "[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?";
            default = "piqueld";
            description = "Node name, which becomes <hostname>.<tailnet>.ts.net.";
          };
          tailscale.auth_key_file = lib.mkOption {
            # A string, not a path, so the secret is never copied into the Nix store.
            type = lib.types.nullOr (lib.types.strMatching "/.+");
            default = null;
            description = "Host file with a Tailscale auth key for the node's first login, such as an agenix secret, passed to piqueld as a systemd credential. Node state in dataDir makes it unnecessary afterwards.";
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
          ingress.enabled = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Manage a Caddy gateway on ports 80/443. Requires Docker 28+. Restart piqueld to apply; disabling stops public routing but retains route configuration and certificates.";
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
            description = "Timeout in seconds for runtime convergence; restarts each time a service converges.";
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
            default = 90;
            description = "Days to retain application events; zero disables pruning.";
          };
          retention.daemon_event_days = lib.mkOption {
            type = lib.types.ints.unsigned;
            default = 90;
            description = "Days to retain daemon diagnostics independently of application deletion; zero disables pruning.";
          };
          metrics.listen = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            description = "Metrics-only socket addresses; empty disables exposure.";
          };
          notifications.enabled = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Deliver webhook notifications to the configured destinations. Re-enabling never replays old events.";
          };
          notifications.destinations = lib.mkOption {
            type = lib.types.listOf (
              lib.types.submodule {
                options = {
                  name = lib.mkOption {
                    type = lib.types.str;
                    description = "Unique destination name, used in the delivery ledger.";
                  };
                  kind = lib.mkOption {
                    type = lib.types.enum [
                      "json"
                      "discord"
                    ];
                    default = "json";
                    description = "Payload format.";
                  };
                  enabled = lib.mkOption {
                    type = lib.types.bool;
                    default = true;
                    description = "Deliver to this destination.";
                  };
                  url = lib.mkOption {
                    type = lib.types.nullOr lib.types.str;
                    default = null;
                    description = "Webhook URL. URLs often contain credentials and this one enters the Nix store; prefer url_file.";
                  };
                  url_file = lib.mkOption {
                    # A string, not a path, so the secret is never copied into the Nix store.
                    type = lib.types.nullOr (lib.types.strMatching "/.+");
                    default = null;
                    description = "Host file with the webhook URL, such as an agenix secret, passed to piqueld as a systemd credential.";
                  };
                };
              }
            );
            default = [ ];
            description = "Webhook destinations. Each needs exactly one of url and url_file.";
          };
          notifications.build_failures = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Notify on build failures.";
          };
          notifications.deployment_failures = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Notify on deployment attempt failures.";
          };
          notifications.service_degradation = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Notify on sustained service degradation.";
          };
          notifications.daemon_failures = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Notify on shared dependency and internal daemon failures.";
          };
          notifications.recovery = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Notify when an alerted condition clears.";
          };
          notifications.failure_threshold_seconds = lib.mkOption {
            type = lib.types.ints.between 0 86400;
            default = 120;
            description = "Seconds a dependency or service failure must be continuously observed before notifying.";
          };
          notifications.retry_window_seconds = lib.mkOption {
            type = lib.types.ints.between 1 604800;
            default = 86400;
            description = "Maximum automatic delivery retry window in seconds.";
          };
        };
      };
      default = { };
      description = "Typed daemon TOML settings. dataDir and runtimeDir control server.data_dir and server.runtime_dir. These settings enter the Nix store, so give credentials through the `_file` variants.";
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
      {
        assertion = lib.all (
          destination: (destination.url == null) != (destination.url_file == null)
        ) destinations;
        message = "services.piqueld.settings.notifications.destinations: each entry needs exactly one of url and url_file";
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
      # The tailnet node runs its own tailscaled and drives it with the CLI.
      ++ lib.optional (usesTailscale || cfg.settings.tailscale.enabled) config.services.tailscale.package;
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/piqueld --config ${configuration}";
        LoadCredential = credentials;
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
    systemd.tmpfiles.rules = lib.mkIf cfg.backup.enable [
      "d ${cfg.backup.directory} 0700 piqueld piqueld -"
    ];
    systemd.services.piqueld-backup = lib.mkIf cfg.backup.enable {
      description = "piqueld state backup";
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${cfg.package}/bin/piqueld --config ${configuration} backup --directory ${cfg.backup.directory} --keep ${toString cfg.backup.keep}";
        LoadCredential = credentials;
        User = "piqueld";
        Group = "piqueld";
        UMask = "0077";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ReadWritePaths = [
          cfg.dataDir
          cfg.backup.directory
        ];
      };
    };
    systemd.timers.piqueld-backup = lib.mkIf cfg.backup.enable {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = cfg.backup.schedule;
        Persistent = true;
      };
    };
  };
}
