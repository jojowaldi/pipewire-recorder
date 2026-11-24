use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_YAML_WITH_COMMENTS: &str = r#"# ~/.config/pipewire-recorder/config.yaml
#
# Default configuration for pipewire-recorder.
# All fields are optional; unset values fall back to built-in defaults.
# Alternatively, you can place a `config.json` in the same directory.

# Output location and file naming
output_dir: "~/Music/Spotify"
path_template: "{artist}/{album}/{track_number} - {title}.flac"
index_path: "~/.local/share/pipewire-recorder/recorded_index.json"

# Target media player & PipeWire output stream
stream:
  # MPRIS2 player name on the session D-Bus (e.g. "spotify", "firefox", "vlc", "amberol", "mpd")
  # Also matches instance suffixes such as org.mpris.MediaPlayer2.firefox.instance_1_23
  mpris_player: "spotify"

  # PipeWire stream matcher checked against application.name, application.process.binary,
  # node.name, media.name, or numeric object.serial / node.id (see `pipewire-recorder list-streams`)
  pipewire_target: "spotify"

  # Capture PCM format (1:1 PipeWire PCM without resampling effects)
  sample_rate: 44100
  channels: 2

  # IMPORTANT (1:1 signal / volume behavior & EasyEffects):
  # - System/master sink volume and EasyEffects act downstream of the app stream and do NOT
  #   affect the recording.
  # - Per-app stream volume (`channelVolumes` / Spotify volume slider / MPRIS Volume) scales
  #   samples directly on the stream. With `enforce_unity_volume: true`, the daemon resets
  #   volume to 1.0 (100%) on track start and discards takes if attenuated or muted mid-track.
  # - In Spotify itself, disable "Settings -> Audio Quality -> Normalize volume" so Spotify
  #   does not apply internal ReplayGain scaling before PipeWire receives the audio.
  enforce_unity_volume: true

  # Optional: Virtual capture sink + independently controllable playback loopback
  # Set `enabled: true` to lower or mute Spotify (or the target player) in your desktop mixer
  # independently of other apps while still recording bit-exact audio at 100%.
  # Keep the player itself at 100% and control listening volume via `playback_name`
  # (e.g. "PipeWire Recorder Playback").
  virtual_sink:
    enabled: false
    sink_name: "pipewire_recorder_capture"
    sink_description: "PipeWire Recorder Capture (100%)"
    playback_name: "PipeWire Recorder Playback"
    # Optional target sink for listening playback (node.name or object.serial); null = system default / EasyEffects
    playback_target: null
    # Automatically route the target stream to the virtual sink via pw-metadata
    auto_route: true

# Completeness validation rules for saving tracks
completeness:
  max_start_offset_ms: 2500
  duration_tolerance_ms: 3000
  min_completion_ratio: 0.97
  allow_pause: false

# Audio encoding & capture process lifecycle
audio:
  flac_compression_level: 5
  # Defaults to false so captured PCM frames are stored 1:1 unmodified (bit-exact).
  trim_silence: false
  silence_threshold_db: -60.0
  max_trim_ms: 1500
  idle_timeout_secs: 15
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Yaml,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub output_dir: String,
    pub path_template: String,
    pub index_path: String,
    pub stream: StreamConfig,
    pub completeness: CompletenessConfig,
    pub audio: AudioConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct StreamConfig {
    pub mpris_player: String,
    pub pipewire_target: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub enforce_unity_volume: bool,
    pub virtual_sink: VirtualSinkConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct VirtualSinkConfig {
    pub enabled: bool,
    pub sink_name: String,
    pub sink_description: String,
    pub playback_name: String,
    pub playback_target: Option<String>,
    pub auto_route: bool,
}

impl VirtualSinkConfig {
    pub fn playback_node_name(&self) -> String {
        format!("{}.playback", self.sink_name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CompletenessConfig {
    pub max_start_offset_ms: u64,
    pub duration_tolerance_ms: u64,
    pub min_completion_ratio: f64,
    pub allow_pause: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioConfig {
    pub flac_compression_level: u8,
    pub trim_silence: bool,
    pub silence_threshold_db: f32,
    pub max_trim_ms: u64,
    pub idle_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output_dir: "~/Music/Spotify".to_string(),
            path_template: "{artist}/{album}/{track_number} - {title}.flac".to_string(),
            index_path: "~/.local/share/pipewire-recorder/recorded_index.json".to_string(),
            stream: StreamConfig::default(),
            completeness: CompletenessConfig::default(),
            audio: AudioConfig::default(),
        }
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            mpris_player: "spotify".to_string(),
            pipewire_target: "spotify".to_string(),
            sample_rate: 44100,
            channels: 2,
            enforce_unity_volume: true,
            virtual_sink: VirtualSinkConfig::default(),
        }
    }
}

impl Default for VirtualSinkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sink_name: "pipewire_recorder_capture".to_string(),
            sink_description: "PipeWire Recorder Capture (100%)".to_string(),
            playback_name: "PipeWire Recorder Playback".to_string(),
            playback_target: None,
            auto_route: true,
        }
    }
}

impl Default for CompletenessConfig {
    fn default() -> Self {
        Self {
            max_start_offset_ms: 2500,
            duration_tolerance_ms: 3000,
            min_completion_ratio: 0.97,
            allow_pause: false,
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            flac_compression_level: 5,
            trim_silence: false,
            silence_threshold_db: -60.0,
            max_trim_ms: 1500,
            idle_timeout_secs: 15,
        }
    }
}

impl Config {
    pub fn default_config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| expand_path("~/.config"))
            .join("pipewire-recorder")
    }

    pub fn load_or_init(custom_path: Option<&Path>) -> Result<(Self, PathBuf)> {
        let dir = Self::default_config_dir();
        Self::load_or_init_in_dir(&dir, custom_path)
    }

    pub fn load_or_init_in_dir(
        config_dir: &Path,
        custom_path: Option<&Path>,
    ) -> Result<(Self, PathBuf)> {
        if let Some(path) = custom_path {
            let expanded = expand_path_str(path.to_string_lossy().as_ref());
            let cfg = Self::load_from_path(&expanded)?;
            return Ok((cfg, expanded));
        }

        let candidates = [
            config_dir.join("config.yaml"),
            config_dir.join("config.yml"),
            config_dir.join("config.json"),
        ];

        for candidate in &candidates {
            if candidate.exists() {
                let cfg = Self::load_from_path(candidate)?;
                return Ok((cfg, candidate.clone()));
            }
        }

        let default_path = config_dir.join("config.yaml");
        Self::write_default_config(&default_path, ConfigFormat::Yaml, false)?;
        Ok((Self::default(), default_path))
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;

        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase());

        let cfg = match ext.as_deref() {
            Some("json") => serde_json::from_str::<Self>(&raw)
                .with_context(|| format!("failed to parse JSON config {}", path.display()))?,
            Some("yaml") | Some("yml") => serde_yaml::from_str::<Self>(&raw)
                .with_context(|| format!("failed to parse YAML config {}", path.display()))?,
            _ => serde_yaml::from_str::<Self>(&raw)
                .or_else(|_| serde_json::from_str::<Self>(&raw))
                .with_context(|| format!("failed to parse config {}", path.display()))?,
        };

        Ok(cfg)
    }

    pub fn write_default_config(path: &Path, format: ConfigFormat, force: bool) -> Result<()> {
        if path.exists() && !force {
            anyhow::bail!("config file already exists at {}", path.display());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }

        let content = match format {
            ConfigFormat::Yaml => DEFAULT_YAML_WITH_COMMENTS.to_string(),
            ConfigFormat::Json => {
                let mut json = serde_json::to_string_pretty(&Self::default())?;
                json.push('\n');
                json
            }
        };

        std::fs::write(path, content)
            .with_context(|| format!("failed to write config file {}", path.display()))?;
        Ok(())
    }

    pub fn resolved_output_dir(&self) -> PathBuf {
        expand_path(&self.output_dir)
    }

    pub fn resolved_index_path(&self) -> PathBuf {
        expand_path(&self.index_path)
    }
}

pub fn expand_path(raw: &str) -> PathBuf {
    expand_path_with_home(raw, dirs::home_dir().as_deref())
}

fn expand_path_str(raw: &str) -> PathBuf {
    expand_path(raw)
}

pub fn expand_path_with_home(raw: &str, home: Option<&Path>) -> PathBuf {
    if raw == "~" {
        return home
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home_dir) = home
    {
        return home_dir.join(rest);
    }
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_preserves_bit_exact_settings() {
        let cfg = Config::default();
        assert!(!cfg.audio.trim_silence);
        assert!(cfg.stream.enforce_unity_volume);
        assert!(!cfg.stream.virtual_sink.enabled);
        assert!(cfg.stream.virtual_sink.auto_route);
        assert_eq!(
            cfg.stream.virtual_sink.sink_name,
            "pipewire_recorder_capture"
        );
        assert_eq!(
            cfg.stream.virtual_sink.playback_node_name(),
            "pipewire_recorder_capture.playback"
        );
        assert_eq!(cfg.stream.mpris_player, "spotify");
        assert_eq!(cfg.stream.pipewire_target, "spotify");
        assert_eq!(cfg.output_dir, "~/Music/Spotify");
    }

    #[test]
    fn partial_virtual_sink_yaml_populates_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "stream:\n  virtual_sink:\n    enabled: true\n    playback_name: \"Spotify Lautstärke\"\n",
        )
        .unwrap();

        let (cfg, _) = Config::load_or_init_in_dir(dir.path(), None).unwrap();
        assert!(cfg.stream.virtual_sink.enabled);
        assert_eq!(cfg.stream.virtual_sink.playback_name, "Spotify Lautstärke");
        assert_eq!(
            cfg.stream.virtual_sink.sink_name,
            "pipewire_recorder_capture"
        );
        assert!(cfg.stream.virtual_sink.auto_route);
        assert!(cfg.stream.virtual_sink.playback_target.is_none());
    }

    #[test]
    fn partial_yaml_populates_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "output_dir: \"/tmp/recordings\"\nstream:\n  mpris_player: \"firefox\"\n",
        )
        .unwrap();

        let (cfg, loaded_path) = Config::load_or_init_in_dir(dir.path(), None).unwrap();
        assert_eq!(loaded_path, path);
        assert_eq!(cfg.output_dir, "/tmp/recordings");
        assert_eq!(cfg.stream.mpris_player, "firefox");
        assert_eq!(cfg.stream.pipewire_target, "spotify");
        assert!(!cfg.audio.trim_silence);
        assert!(cfg.stream.enforce_unity_volume);
    }

    #[test]
    fn json_config_is_discovered_and_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"output_dir": "/data/flac", "stream": {"pipewire_target": "vlc"}, "audio": {"trim_silence": true}}"#,
        )
        .unwrap();

        let (cfg, loaded_path) = Config::load_or_init_in_dir(dir.path(), None).unwrap();
        assert_eq!(loaded_path, path);
        assert_eq!(cfg.output_dir, "/data/flac");
        assert_eq!(cfg.stream.pipewire_target, "vlc");
        assert_eq!(cfg.stream.mpris_player, "spotify");
        assert!(cfg.audio.trim_silence);
    }

    #[test]
    fn missing_config_auto_creates_default_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, created_path) = Config::load_or_init_in_dir(dir.path(), None).unwrap();
        assert_eq!(created_path, dir.path().join("config.yaml"));
        assert!(created_path.exists());
        assert_eq!(cfg, Config::default());

        let reparsed = Config::load_from_path(&created_path).unwrap();
        assert_eq!(reparsed, Config::default());
    }

    #[test]
    fn tilde_expansion_uses_home_dir() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_path_with_home("~/Music/Spotify", Some(home)),
            PathBuf::from("/home/tester/Music/Spotify")
        );
        assert_eq!(
            expand_path_with_home("~", Some(home)),
            PathBuf::from("/home/tester")
        );
        assert_eq!(
            expand_path_with_home("/var/music", Some(home)),
            PathBuf::from("/var/music")
        );
    }
}
