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
  format =
    if cfg.configFormat == "json" then
      pkgs.formats.json { }
    else
      pkgs.formats.yaml { };
  configFileName = "pipewire-recorder/config.${cfg.configFormat}";
in
{
  options.services.pipewire-recorder = {
    enable = lib.mkEnableOption "Spotify / PipeWire background stream recorder";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.pipewire-recorder;
      description = "The pipewire-recorder package to use.";
    };

    configFormat = lib.mkOption {
      type = lib.types.enum [
        "yaml"
        "json"
      ];
      default = "yaml";
      description = "Configuration file format written to ~/.config/pipewire-recorder/config.{yaml,json}.";
    };

    settings = lib.mkOption {
      default = { };
      description = "Configuration written to ~/.config/pipewire-recorder/config.{yaml,json}.";
      type = lib.types.submodule {
        freeformType = format.type;
        options = {
          output_dir = lib.mkOption {
            type = lib.types.str;
            default = "~/Music/Spotify";
            description = "Target directory where recorded FLAC tracks are stored.";
          };
          path_template = lib.mkOption {
            type = lib.types.str;
            default = "{artist}/{album}/{track_number} - {title}.flac";
            description = "Output file path template relative to output_dir.";
          };
          index_path = lib.mkOption {
            type = lib.types.str;
            default = "~/.local/share/pipewire-recorder/recorded_index.json";
            description = "Path to the persistent deduplication index JSON file.";
          };

          stream = {
            mpris_player = lib.mkOption {
              type = lib.types.str;
              default = "spotify";
              description = "Target MPRIS2 player name or bus name prefix (e.g. spotify, firefox, vlc).";
            };
            pipewire_target = lib.mkOption {
              type = lib.types.str;
              default = "spotify";
              description = "PipeWire Stream/Output/Audio matcher (application.name, binary, node.name, media.name, or serial/id).";
            };
            sample_rate = lib.mkOption {
              type = lib.types.ints.positive;
              default = 44100;
              description = "PCM capture sample rate in Hz.";
            };
            channels = lib.mkOption {
              type = lib.types.ints.positive;
              default = 2;
              description = "Number of audio channels to capture.";
            };
            enforce_unity_volume = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Enforce 1.0 (100%) MPRIS and PipeWire stream volume for bit-exact recording; discard if attenuated.";
            };
          };

          completeness = {
            max_start_offset_ms = lib.mkOption {
              type = lib.types.ints.unsigned;
              default = 2500;
              description = "Maximum allowed playback start position (ms) to accept a track recording.";
            };
            duration_tolerance_ms = lib.mkOption {
              type = lib.types.ints.unsigned;
              default = 3000;
              description = "Maximum allowed deviation (ms) from expected track length.";
            };
            min_completion_ratio = lib.mkOption {
              type = lib.types.float;
              default = 0.97;
              description = "Minimum captured duration ratio (0.0-1.0) required to save a track.";
            };
            allow_pause = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Whether mid-track pausing keeps the take valid (false ensures splice-free captures).";
            };
          };

          audio = {
            flac_compression_level = lib.mkOption {
              type = lib.types.ints.between 0 8;
              default = 5;
              description = "FLAC compression level (0-8).";
            };
            trim_silence = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Trim leading/trailing digital silence (false keeps 1:1 untouched stream frames).";
            };
            silence_threshold_db = lib.mkOption {
              type = lib.types.float;
              default = -60.0;
              description = "Threshold in dBFS used when trim_silence is enabled.";
            };
            max_trim_ms = lib.mkOption {
              type = lib.types.ints.unsigned;
              default = 1500;
              description = "Maximum milliseconds trimmed from start/end when trim_silence is enabled.";
            };
            idle_timeout_secs = lib.mkOption {
              type = lib.types.ints.unsigned;
              default = 15;
              description = "Seconds to keep pw-record alive while playback is paused/stopped.";
            };
          };
        };
      };
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile.${configFileName} = {
      source = format.generate "pipewire-recorder-config.${cfg.configFormat}" cfg.settings;
    };

    systemd.user.services.pipewire-recorder = {
      Unit = {
        Description = "PipeWire Spotify / Media Stream Recorder";
        After = [
          "pipewire.service"
          "wireplumber.service"
          "dbus.service"
        ];
        X-RestartTriggers = [
          "${config.xdg.configFile.${configFileName}.source}"
          "${cfg.package}"
        ];
      };

      Service = {
        ExecStart = "${lib.getExe cfg.package} --config ${config.xdg.configHome}/${configFileName} run";
        Restart = "on-failure";
        RestartSec = 5;
      };

      Install.WantedBy = [ "default.target" ];
    };
  };
}
