{
  config,
  lib,
  pkgs,
  ...
}:
let
  piquelpkgs = (import ./pkgs.nix { inherit pkgs; });
  piqueld = piquelpkgs.piqueld;

  cfg = config.services.piqueld;
  settingsFormat = pkgs.formats.json { };

  ctlConfig = settingsFormat.generate "piquelctl.json" cfg.ctlSettings;
  daemonConfig = settingsFormat.generate "piqueld.json" cfg.daemonSettings;

  piquelctl =
    pkgs.runCommand "piquelctl-wrapped"
      {
        nativeBuildInputs = [ pkgs.makeWrapper ];
      }
      ''
        makeWrapper ${piquelpkgs.piquelctl}/bin/piquelctl $out/bin/piquelctl \
            --add-flags "--config ${ctlConfig}"
      '';
in
{
  options.services.piqueld = {
    enable = lib.mkEnableOption "piquelctl";

    # CTL

    ctlPackage = lib.mkOption {
      type = lib.types.package;
      default = piquelctl;
    };

    ctlSettings = lib.mkOption {
      description = "The configuration passed to the control cli";
      type = lib.types.submodule {
        freeformType = settingsFormat.type;
        options =
          let
            inherit (lib) mkOption types;
          in
          {
            socket_path = mkOption {
              type = types.str;
              default = "/run/piqueld.sock";
              description = "Path to the socket";
            };
          };
      };
    };

    # DAEMON

    enableDaemon = lib.mkOption {
      default = true;
      example = false;
      description = "Whether to enable the daemon (piqueld).";
      type = lib.types.bool;
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "piqueld";
      description = "Group that can access the piqueld socket.";
    };

    daemonPackage = lib.mkOption {
      type = lib.types.package;
      default = piqueld;
    };

    daemonSettings = lib.mkOption {
      description = "The configuration passed to the daemon";
      type = lib.types.submodule {
        freeformType = settingsFormat.type;
        options =
          let
            inherit (lib) mkOption types;
          in
          {
            data_dir = mkOption {
              type = types.str;
              default = "/var/lib/piqueld";
              description = "Path to daemon data";
            };
            socket_path = mkOption {
              type = types.str;
              default = "/run/piqueld.sock";
              description = "Path to the socket";
            };
            listen_addr = mkOption {
              type = types.str;
              default = "0.0.0.0:7854";
              description = "Address to listen to";
            };
          };
      };
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      environment.systemPackages = [ cfg.ctlPackage ];
    })
    (lib.mkIf cfg.enableDaemon {
      environment.etc."piqueld/config.json".source = daemonConfig;

      systemd.services.piqueld = {
        description = "piqueld";
        wantedBy = [ "multi-user.target" ];
        after = [ "network.target" ];
        serviceConfig = {
          ExecStart = "${piqueld}/bin/piqueld --config /etc/piqueld/config.json";
          DynamicUser = true;
          Group = cfg.group;
          StateDirectory = "piqueld";
          RuntimeDirectory = "piqueld";
          RuntimeDirectoryMode = "0750";
          PrivateTmp = true;
          ProtectSystem = "strict";
          ProtectHome = true;
        };
      };
    })
  ];
}
