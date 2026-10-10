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
  privateIngress = cfg.settings.ingress.private;
  tunnel = cfg.settings.ingress.tunnel;
  destinations = cfg.settings.notifications.destinations;
  # `_file` settings name host files. systemd copies each into the unit's
  # private $CREDENTIALS_DIRECTORY, so the files may stay root-only, and the
  # daemon resolves the credential name there.
  webhookCredential = index: "webhook-${toString index}";
  providers = cfg.settings.dns.providers;
  # Credential files of a DNS provider, by setting name.
  dnsFiles =
    provider: lib.filterAttrs (name: value: lib.hasSuffix "_file" name && value != null) provider;
  dnsCredential = index: setting: "dns-${toString index}-${lib.removeSuffix "_file" setting}";
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
        dns.providers = lib.imap0 (
          index: provider:
          withoutNulls (
            provider // lib.mapAttrs (setting: _: dnsCredential index setting) (dnsFiles provider)
          )
        ) providers;
      }
      // lib.optionalAttrs (tailscale.auth_key_file != null) {
        tailscale.auth_key_file = "ts-auth-key";
      }
      // {
        ingress =
          lib.optionalAttrs (privateIngress.auth_key_file != null) {
            private.auth_key_file = "ts-apps-auth-key";
          }
          // lib.optionalAttrs (tunnel.credentials_file != null) {
            tunnel.credentials_file = "cloudflared-tunnel";
          };
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
          auth.max_token_days = lib.mkOption {
            type = lib.types.nullOr (lib.types.ints.between 1 4294967295);
            default = null;
            description = "Longest lifetime of new API tokens in days. Null allows any lifetime, including tokens that never expire.";
          };
          tailscale.enabled = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Join the tailnet as a dedicated node and serve the website over HTTPS on its https_port, with a tailnet-issued certificate. Requires HTTPS certificates to be enabled for the tailnet.";
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
          tailscale.https_port = lib.mkOption {
            type = lib.types.ints.between 1 65535;
            default = 443;
            description = "Node port serving the website over HTTPS.";
          };
          tailscale.socket = lib.mkOption {
            type = lib.types.nullOr (lib.types.strMatching "/.+");
            default = null;
            description = "Socket of a logged-in tailscaled, running as the piqueld user, to share instead of starting a dedicated one. piqueld then only adds https_port to its Serve configuration.";
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
          ingress.public_addresses = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            example = [
              "203.0.113.10"
              "2001:db8::10"
            ];
            description = "This server's public IPv4 and IPv6 addresses. Where a DNS provider sets manage_records, direct public routes get A/AAAA records to them; without any, those records stay manual. Unused in tunnel mode.";
          };
          ingress.acme.directory = lib.mkOption {
            type = lib.types.strMatching "https://.+";
            default = "https://acme-v02.api.letsencrypt.org/directory";
            description = "ACME directory piqueld orders DNS-01 certificates from.";
          };
          ingress.acme.email = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Optional ACME account contact for expiry and policy notices from the CA.";
          };
          ingress.private.enabled = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Serve private routes to the tailnet through a second tailnet node, run as a container beside the gateway. Private routes need DNS-01 certificates through dns.providers. Restart piqueld to apply.";
          };
          ingress.private.hostname = lib.mkOption {
            type = lib.types.strMatching "[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?";
            default = "piqueld-apps";
            description = "Apps node name, which becomes <hostname>.<tailnet>.ts.net. Must differ from tailscale.hostname.";
          };
          ingress.private.auth_key_file = lib.mkOption {
            # A string, not a path, so the secret is never copied into the Nix store.
            type = lib.types.nullOr (lib.types.strMatching "/.+");
            default = null;
            description = "Host file with a Tailscale auth key for the apps node's first login, such as an agenix secret, passed to piqueld as a systemd credential. Without one, the daemon logs the node's login URL.";
          };
          ingress.tunnel.enabled = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Serve public routes through a locally managed Cloudflare Tunnel instead of ports 80/443, which are then closed. Each public hostname needs a proxied CNAME to <tunnel-id>.cfargotunnel.com. Restart piqueld to apply.";
          };
          ingress.tunnel.credentials_file = lib.mkOption {
            # A string, not a path, so the secret is never copied into the Nix store.
            type = lib.types.nullOr (lib.types.strMatching "/.+");
            default = null;
            description = "Host file with the tunnel's credentials, from `cloudflared tunnel create`, such as an agenix secret, passed to piqueld as a systemd credential. Required while the tunnel is enabled.";
          };
          dns.providers = lib.mkOption {
            type = lib.types.listOf (
              lib.types.submodule {
                options =
                  let
                    # A string, not a path, so the secret is never copied into the Nix store.
                    file =
                      description:
                      lib.mkOption {
                        type = lib.types.nullOr (lib.types.strMatching "/.+");
                        default = null;
                        description = "${description}, such as an agenix secret, passed to piqueld as a systemd credential.";
                      };
                  in
                  {
                    kind = lib.mkOption {
                      type = lib.types.enum [
                        "cloudflare"
                        "ovh"
                      ];
                      description = "Provider API.";
                    };
                    api_token_file = file "Cloudflare: host file with an API token scoped to Zone:Read and DNS:Edit";
                    endpoint = lib.mkOption {
                      type = lib.types.nullOr (
                        lib.types.enum [
                          "ovh-eu"
                          "ovh-ca"
                          "ovh-us"
                        ]
                      );
                      default = null;
                      description = "OVH: API region of the account.";
                    };
                    application_key_file = file "OVH: host file with the application key";
                    application_secret_file = file "OVH: host file with the application secret";
                    consumer_key_file = file "OVH: host file with the consumer key";
                    manage_records = lib.mkOption {
                      type = lib.types.bool;
                      default = false;
                      description = "Create, update and delete routes' A/AAAA/CNAME records in this provider's zones. Off by default, so its zones stay manual.";
                    };
                  };
              }
            );
            default = [ ];
            description = "DNS provider accounts piqueld uses for DNS-01 certificates and, with manage_records, routes' DNS records. Cloudflare needs api_token_file; OVH needs endpoint and the three OVH files.";
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
          retention.audit_days = lib.mkOption {
            type = lib.types.ints.unsigned;
            default = 365;
            description = "Days to retain the audit trail independently of application deletion; zero disables pruning.";
          };
          previews.max_per_application = lib.mkOption {
            type = lib.types.ints.unsigned;
            default = 10;
            description = "Previews one application may have; creating more fails with preview_limit_reached.";
          };
          previews.max_total = lib.mkOption {
            type = lib.types.ints.unsigned;
            default = 30;
            description = "Previews the installation may have; creating more fails with preview_limit_reached.";
          };
          previews.default_cpu_millis = lib.mkOption {
            type = lib.types.ints.between 1 1048576;
            default = 500;
            description = "CPU limit, in millicores, of preview services that set none.";
          };
          previews.default_memory_bytes = lib.mkOption {
            type = lib.types.ints.positive;
            default = 536870912;
            description = "Memory limit, in bytes, of preview services that set none.";
          };
          previews.max_replicas = lib.mkOption {
            type = lib.types.ints.between 1 100;
            default = 1;
            description = "Most replicas a preview service runs; manifests asking for more are capped with a warning.";
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
          notifications.security = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Notify on security-relevant access changes and activity, such as new administrators, privileged tokens, and bursts of refused requests.";
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
        assertion = tunnel.enabled -> tunnel.credentials_file != null;
        message = "services.piqueld.settings.ingress.tunnel.credentials_file is required while the tunnel is enabled";
      }
      {
        assertion = lib.all (
          destination: (destination.url == null) != (destination.url_file == null)
        ) destinations;
        message = "services.piqueld.settings.notifications.destinations: each entry needs exactly one of url and url_file";
      }
      {
        assertion = lib.all (
          provider:
          let
            ovh = [
              provider.endpoint
              provider.application_key_file
              provider.application_secret_file
              provider.consumer_key_file
            ];
          in
          if provider.kind == "cloudflare" then
            provider.api_token_file != null && lib.all (value: value == null) ovh
          else
            provider.api_token_file == null && lib.all (value: value != null) ovh
        ) providers;
        message = "services.piqueld.settings.dns.providers: cloudflare needs only api_token_file; ovh needs only endpoint, application_key_file, application_secret_file and consumer_key_file";
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
      # The tailnet node runs its own tailscaled, unless shared, and drives it with the CLI.
      ++ lib.optional (usesTailscale || cfg.settings.tailscale.enabled) config.services.tailscale.package;
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/piqueld --config ${configuration}";
        LoadCredential =
          lib.optional (tailscale.auth_key_file != null) "ts-auth-key:${tailscale.auth_key_file}"
          ++ lib.optional (
            privateIngress.auth_key_file != null
          ) "ts-apps-auth-key:${privateIngress.auth_key_file}"
          ++ lib.optional (
            tunnel.credentials_file != null
          ) "cloudflared-tunnel:${tunnel.credentials_file}"
          ++ lib.concatLists (
            lib.imap0 (
              index: destination:
              lib.optional (destination.url_file != null) "${webhookCredential index}:${destination.url_file}"
            ) destinations
          )
          ++ lib.concatLists (
            lib.imap0 (
              index: provider:
              lib.mapAttrsToList (setting: path: "${dnsCredential index setting}:${path}") (dnsFiles provider)
            ) providers
          );
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
