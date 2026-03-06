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

  ctlConfig = settingsFormat.generate "piquelctl.json" cfg.ctlConfig;
  daemonConfig = settingsFormat.generate "piqueld.json" cfg.daemonConfig;

  piquelctl =
    pkgs.runCommand "piquelctl-wrapped"
      {
        nativeBuildInputs = [ pkgs.makeWrapper ];
      }
      ''
        makeWrapper ${piquelpkgs.piquelctl { }}/bin/piquelctl $out/bin/piquelctl \
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

    ctlConfig = lib.mkOption {
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

  config =
    (lib.mkIf cfg.enable {
      environment.systemPackages = [ cfg.ctlPackage ];
    })
    // (lib.mkIf cfg.enableDaemon {
      environment.etc."piqueld/config.json".source = daemonConfig;

      systemd.services.your-daemon = {
        description = "piqueld";
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          ExecStart = "${piqueld}/bin/piqueld --config /etc/piqueld/config.json";
          User = "piqueld";
          DynamicUser = true;
          StateDirectory = "piqueld"; # creates /var/lib/piqueld
          RuntimeDirectory = "piqueld"; # creates /run/piqueld
        };
      };
    });
}
