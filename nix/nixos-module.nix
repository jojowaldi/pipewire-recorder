self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (pkgs.stdenv.hostPlatform) system;
  cfg = config.services.pipewire-recorder;
in
{
  options.services.pipewire-recorder = {
    enable = lib.mkEnableOption "Spotify / PipeWire background stream recorder user service";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.pipewire-recorder;
      description = "The pipewire-recorder package to use.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    systemd.user.services.pipewire-recorder = {
      description = "PipeWire Spotify / Media Stream Recorder";
      after = [
        "pipewire.service"
        "wireplumber.service"
        "dbus.service"
      ];
      wantedBy = [ "default.target" ];
      restartTriggers = [ cfg.package ];

      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} run";
        Restart = "on-failure";
        RestartSec = 5;
      };
    };
  };
}
