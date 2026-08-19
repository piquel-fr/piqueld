{ config, lib, pkgs, ... }:

let
  cfg = config.services.piqueld;
  toml = pkgs.formats.toml { };
  loopback = value:
    let
      parsed = builtins.match "127\\.([0-9]{1,3})\\.([0-9]{1,3})\\.([0-9]{1,3}):([0-9]{1,5})" value;
    in
    parsed != null && lib.all (octet: lib.toInt octet <= 255) (lib.take 3 parsed)
      && lib.toInt (builtins.elemAt parsed 3) > 0
      && lib.toInt (builtins.elemAt parsed 3) <= 65535;
  dedicatedPath = prefix: value:
    builtins.match ("" + prefix + "/[a-zA-Z0-9._-]+(/[a-zA-Z0-9._-]+)*") (toString value) != null;
  credential = name: value: lib.optional (value != null) "${name}:${value}";
  daemonConfig = toml.generate "piqueld.toml" {
    data_dir = cfg.dataDir;
    server = {
      unix_socket = cfg.server.unixSocket;
      http_listen = cfg.server.httpListen;
      ui_dir = "${cfg.uiPackage}/share/piqueld/ui";
    };
    database.path = "${cfg.dataDir}/piqueld.db";
    docker = {
      socket = "/var/run/docker.sock";
      auto_initialize_swarm = cfg.swarm.autoInitialize;
    };
    registry = {
      address = cfg.registry.address;
      data_dir = cfg.registry.dataDir;
    };
    traefik = {
      image = cfg.traefik.image;
    } // lib.optionalAttrs (cfg.traefik.publishedPort != null) {
      published_port = cfg.traefik.publishedPort;
    };
    security = {
      trusted_loopback_proxy = cfg.security.trustedLoopbackProxy;
      trust_tailscale_headers = cfg.security.trustTailscaleHeaders;
      allowed_origins = cfg.security.allowedOrigins;
      max_body_bytes = cfg.security.maxBodyBytes;
      max_header_bytes = cfg.security.maxHeaderBytes;
      max_headers = cfg.security.maxHeaders;
      request_timeout_seconds = cfg.security.requestTimeoutSeconds;
      max_concurrent_requests = cfg.security.maxConcurrentRequests;
    };
    observability.metrics = cfg.metrics.enable;
    credentials =
      lib.optionalAttrs (cfg.credentials.masterKeyFile != null) {
        encryption_key = { source = "systemd_credential"; name = "master-key"; };
      }
      // lib.optionalAttrs (cfg.credentials.bearerTokenFile != null) {
        bearer_token = { source = "systemd_credential"; name = "bearer-token"; };
      }
      // lib.optionalAttrs (cfg.credentials.gitTokenFile != null) {
        git_token = { source = "systemd_credential"; name = "git-token"; };
      };
  };
in
{
  options.services.piqueld = {
    enable = lib.mkEnableOption "piqueld single-host application control plane";
    package = lib.mkPackageOption pkgs "piqueld" { };
    cliPackage = lib.mkOption {
      type = lib.types.package;
      default = cfg.package;
      description = "Package containing piquelctl.";
    };
    uiPackage = lib.mkOption {
      type = lib.types.package;
      default = cfg.package;
      description = "Package containing immutable UI assets.";
    };
    installCli = lib.mkEnableOption "piquelctl system package";
    dataDir = lib.mkOption { type = lib.types.path; default = "/var/lib/piqueld"; };
    server.unixSocket = lib.mkOption {
      type = lib.types.path;
      default = "/run/piqueld/piqueld.sock";
    };
    server.httpListen = lib.mkOption { type = lib.types.str; default = "127.0.0.1:7845"; };
    swarm.autoInitialize = lib.mkOption { type = lib.types.bool; default = true; };
    registry = {
      enable = lib.mkEnableOption "loopback OCI registry" // { default = true; };
      address = lib.mkOption { type = lib.types.str; default = "127.0.0.1:5000"; };
      dataDir = lib.mkOption { type = lib.types.path; default = "/var/lib/piqueld/registry"; };
    };
    traefik = {
      image = lib.mkOption {
        type = lib.types.str;
        default = "traefik:v3.5.0@sha256:4e7175cfe19be83c6b928cae49dde2f2788fb307189a4dc9550b67acf30c11a5";
      };
      publishedPort = lib.mkOption {
        type = lib.types.nullOr lib.types.port;
        default = null;
        description = "Optional explicitly published application origin port.";
      };
    };
    credentials = {
      masterKeyFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Root-protected external 32-byte key; never place it in the Nix store.";
      };
      bearerTokenFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Root-protected external administrative bearer token.";
      };
      gitTokenFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Root-protected external Git token.";
      };
    };
    security = {
      trustedLoopbackProxy = lib.mkOption { type = lib.types.bool; default = false; };
      trustTailscaleHeaders = lib.mkOption { type = lib.types.bool; default = false; };
      allowedOrigins = lib.mkOption { type = lib.types.listOf lib.types.str; default = [ ]; };
      maxBodyBytes = lib.mkOption { type = lib.types.ints.positive; default = 16777216; };
      maxHeaderBytes = lib.mkOption { type = lib.types.ints.positive; default = 32768; };
      maxHeaders = lib.mkOption { type = lib.types.ints.positive; default = 64; };
      requestTimeoutSeconds = lib.mkOption { type = lib.types.ints.positive; default = 120; };
      maxConcurrentRequests = lib.mkOption { type = lib.types.ints.positive; default = 64; };
    };
    metrics.enable = lib.mkEnableOption "private Prometheus endpoint";
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = loopback cfg.server.httpListen;
        message = "services.piqueld.server.httpListen must be loopback";
      }
      {
        assertion = dedicatedPath "/var/lib" cfg.dataDir;
        message = "services.piqueld.dataDir must be a dedicated directory below /var/lib";
      }
      {
        assertion = dedicatedPath "/run" (builtins.dirOf (toString cfg.server.unixSocket));
        message = "services.piqueld.server.unixSocket must be below a dedicated /run directory";
      }
      {
        assertion = loopback cfg.registry.address;
        message = "services.piqueld.registry.address must use IPv4 loopback";
      }
      {
        assertion = dedicatedPath "/var/lib" cfg.registry.dataDir;
        message = "services.piqueld.registry.dataDir must be a dedicated directory below /var/lib";
      }
      {
        assertion = !cfg.security.trustTailscaleHeaders || cfg.security.trustedLoopbackProxy;
        message = "trusted Tailscale headers require a trusted loopback proxy";
      }
      {
        assertion = lib.all (path: path == null || !lib.hasPrefix builtins.storeDir (toString path)) [
          cfg.credentials.masterKeyFile
          cfg.credentials.bearerTokenFile
          cfg.credentials.gitTokenFile
        ];
        message = "piqueld credentials must not be in the Nix store";
      }
    ];

    environment.systemPackages = [ cfg.package ] ++ lib.optional cfg.installCli cfg.cliPackage;
    users.groups.piqueld = { };
    users.users.piqueld = {
      isSystemUser = true;
      group = "piqueld";
      extraGroups = [ "docker" ];
      home = cfg.dataDir;
    };
    virtualisation.docker.enable = true;
    services.dockerRegistry = lib.mkIf cfg.registry.enable {
      enable = true;
      listenAddress = builtins.head (lib.splitString ":" cfg.registry.address);
      port = lib.toInt (builtins.elemAt (lib.splitString ":" cfg.registry.address) 1);
      storagePath = cfg.registry.dataDir;
    };
    environment.etc."piqueld/config.toml".source = daemonConfig;
    systemd.services.piqueld = {
      description = "piqueld declarative application control plane";
      wantedBy = [ "multi-user.target" ];
      after = [ "docker.service" ] ++ lib.optional cfg.registry.enable "docker-registry.service";
      wants = [ "docker.service" ] ++ lib.optional cfg.registry.enable "docker-registry.service";
      serviceConfig = {
        Type = "simple";
        User = "piqueld";
        Group = "piqueld";
        SupplementaryGroups = [ "docker" ];
        ExecStart = "${cfg.package}/bin/piqueld";
        Environment = "PIQUELD_CONFIG=/etc/piqueld/config.toml";
        LoadCredential =
          credential "master-key" cfg.credentials.masterKeyFile
          ++ credential "bearer-token" cfg.credentials.bearerTokenFile
          ++ credential "git-token" cfg.credentials.gitTokenFile;
        StateDirectory = lib.removePrefix "/var/lib/" (toString cfg.dataDir);
        StateDirectoryMode = "0750";
        RuntimeDirectory = lib.removePrefix "/run/" (builtins.dirOf (toString cfg.server.unixSocket));
        RuntimeDirectoryMode = "0750";
        UMask = "0007";
        Restart = "on-failure";
        RestartSec = "5s";
        TimeoutStopSec = "180s";
        KillSignal = "SIGTERM";
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        LockPersonality = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        SystemCallArchitectures = "native";
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
        ReadWritePaths = [ cfg.dataDir (builtins.dirOf (toString cfg.server.unixSocket)) ];
      };
    };
    # No firewall ports are opened. Management remains on the Unix socket or a
    # separately configured private loopback proxy.
  };
}
