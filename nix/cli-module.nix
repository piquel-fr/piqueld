{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.piquelctl;
  configuration = (pkgs.formats.toml { }).generate "piquelctl-profiles.toml" (
    lib.filterAttrsRecursive (_: value: value != null) cfg.settings
  );
  package = pkgs.symlinkJoin {
    name = "piquelctl-wrapped";
    paths = [ cfg.package ];
    nativeBuildInputs = [ pkgs.makeWrapper ];
    postBuild = ''
      wrapProgram "$out/bin/piquelctl" \
        --set-default PIQUELD_PROFILES_FILE ${configuration}
    '';
  };
  profileType = lib.types.submodule {
    options = {
      socket = lib.mkOption {
        type = lib.types.nullOr (lib.types.strMatching "/.+");
        default = null;
        example = "/run/piqueld/piqueld.sock";
        description = "Absolute path to the piqueld Unix socket.";
      };
      url = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "http://127.0.0.1:7845";
        description = "URL of the piqueld HTTP API.";
      };
      timeout = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "2m";
        description = "Command timeout as a human-readable duration.";
      };
    };
  };
in
{
  options.programs.piquelctl = {
    enable = lib.mkEnableOption "the piquelctl command-line client";
    package = lib.mkOption {
      type = lib.types.package;
      description = "Package containing piquelctl.";
    };
    settings = lib.mkOption {
      type = lib.types.submodule {
        options.profiles = lib.mkOption {
          type = lib.types.attrsOf profileType;
          default = { };
          description = "Named piqueld connection profiles.";
        };
      };
      default = { };
      description = "Typed piquelctl profiles TOML settings. Never put credentials here: these settings enter the Nix store.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = lib.mapAttrsToList (name: profile: {
      assertion = (profile.socket != null) != (profile.url != null);
      message = "programs.piquelctl.settings.profiles.${name} must contain exactly one of socket or url";
    }) cfg.settings.profiles;
    environment.systemPackages = [ package ];
  };
}
