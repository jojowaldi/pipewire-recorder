# `pipewire-recorder`

Automatic background daemon for Linux / PipeWire that captures isolated audio streams from Spotify (or any other MPRIS2-compatible media player), encodes complete tracks losslessly to **FLAC**, and embeds full Vorbis metadata tags and cover art.

## Features

- **Isolated PipeWire capture**: Captures only the targeted application's `Stream/Output/Audio` node — no desktop notifications, voice chat, or browser audio mixed in.
- **Optional virtual capture sink + playback loopback**: Lets you lower or mute the listening volume of Spotify (or your target player) independently in your desktop mixer while recording remains bit-exact at 100%.
- **Gapless track transitions**: Keeps `pw-record` running across track boundaries and swaps the in-memory PCM buffer on the exact track change so song intros are never clipped.
- **Strict completeness validation**: Saves only tracks that started near `0:00`, played through to full length without seeking/scrubbing or mid-track pauses (`allow_pause: false`), and stayed at unity volume. Advertisements are discarded automatically.
- **Two-stage deduplication**: Checks both a persistent JSON index (`~/.local/share/pipewire-recorder/recorded_index.json`) and whether the target file still exists on disk. If you manually delete a file, playing the track again will re-record it.
- **Lossless FLAC + tags + cover art**: Embeds Vorbis comments (`TITLE`, `ARTIST`, `ALBUM`, `ALBUMARTIST`, `TRACKNUMBER`, `DISCNUMBER`, `DATE`, `TRACK_ID`, `SPOTIFY_TRACK_ID`) and `COVER_FRONT` artwork (`METADATA_BLOCK_PICTURE`).
- **YAML or JSON configuration**: Auto-generates defaults at `~/.config/pipewire-recorder/config.yaml` (also supports `config.json`).
- **Multi-player support**: Preconfigured for Spotify, but configurable for any MPRIS2 player (Firefox, Chromium, VLC, Amberol, Cider, MPD, etc.).
- **NDJSON IPC mode (`run --ipc`)**: Structured JSON event stream on `stdout` and live command input on `stdin` for desktop shell integrations (such as the Corona Shell plugin).

---

## Requirements

Runtime tools expected in `PATH` (automatically bundled when installed via the Nix flake):

- **PipeWire**: `pw-record`, `pw-dump`, `pw-loopback`, `pw-metadata`, `pw-cli`
- **WirePlumber**: `wpctl`
- **FLAC**: `flac`, `metaflac`
- **HTTP cover art**: `curl`

---

## Installation

### Prebuilt Binaries (GitHub Releases)

Download the latest release binary or tarball for `linux-x86_64` or `linux-aarch64` from the [GitHub Releases](https://github.com/jojowaldi/pipewire-recorder/releases) page, verify against `SHA256SUMS.txt`, and place `pipewire-recorder` on your `PATH`.

### Build from Source

```bash
cargo build --release
# Binary will be at target/release/pipewire-recorder
```

### Nix Flake / Home Manager / NixOS

Add the flake as an input:

```nix
# flake.nix
inputs.pipewire-recorder = {
  url = "github:jojowaldi/pipewire-recorder";
};
```

#### Option 1: Home Manager background service (`systemd --user`)

```nix
imports = [ inputs.pipewire-recorder.homeModules.default ];

services.pipewire-recorder = {
  enable = true;
  configFormat = "yaml"; # or "json"
  settings = {
    output_dir = "~/Music/Spotify";
    stream = {
      mpris_player = "spotify";
      pipewire_target = "spotify";
      enforce_unity_volume = true;
      virtual_sink = {
        enabled = true;
        playback_name = "PipeWire Recorder Playback";
      };
    };
  };
};
```

#### Option 2: Package-only (e.g. managed by a desktop shell plugin like Corona)

If you use a frontend/plugin that spawns and manages `pipewire-recorder run --ipc` directly, install only the package (without the `systemd` user service so only one instance runs):

```nix
home.packages = [
  inputs.pipewire-recorder.packages.${pkgs.stdenv.hostPlatform.system}.default
];
```

#### Option 3: NixOS module

```nix
imports = [ inputs.pipewire-recorder.nixosModules.default ];

services.pipewire-recorder.enable = true;
```

---

## Audio Quality, Volume & EasyEffects

`pipewire-recorder` captures the raw PCM stream **1:1 unmodified** (no normalization, no DSP, `trim_silence: false` by default). To keep recordings truly bit-exact, understand how volume works at each layer:

1. **System / master volume (headphones, speakers, hardware sink)**  
   → **No effect on recording.** Master volume is applied at the final output sink, downstream from where `pipewire-recorder` taps the application stream.

2. **EasyEffects / PipeWire filter sinks**  
   → **No effect on recording.** `pw-record --target <node-serial>` taps the application's `Stream/Output/Audio` node directly at the source — **before** audio enters `easyeffects_sink` or other virtual filters. You can keep using EQs and effects while listening without affecting recorded files.

3. **Application stream volume & Spotify internal normalization (important!)**
   - **Per-app mixer (`channelVolumes` on the PipeWire node) & Spotify's in-app volume slider (MPRIS `Volume`)**:  
     These scale the PCM samples *before* they leave the application's output ports.  
     With `stream.enforce_unity_volume: true` (the default), the daemon resets both the PipeWire node volume (`wpctl set-volume <id> 1.0`) and MPRIS `Volume` to `1.0` (100%) at track start and immediately discards the take (`VolumeAttenuated`) if the app stream is attenuated or muted mid-track.
   - **Independent listening volume (`stream.virtual_sink.enabled: true`)**:  
     If you want to turn Spotify down or mute it independently of other apps (e.g. Discord, browser) while still recording 1:1 at 100%, enable the built-in **virtual capture sink + playback loopback** described below.
   - **Spotify's "Normalize volume" setting**:  
     Disable **Settings → Audio Quality → Normalize volume** inside Spotify. When enabled, Spotify applies internal ReplayGain scaling to its PCM output before PipeWire ever receives the audio.

---

## Independent Listening Volume (Virtual Capture Sink + Loopback)

Because PipeWire applies `channelVolumes` directly inside an application's `Stream/Output/Audio` node, turning down Spotify in your desktop mixer would normally attenuate (or invalidate) the recording.

With **`stream.virtual_sink.enabled: true`**, `pipewire-recorder` decouples capture from listening volume automatically via `pw-loopback` + `pw-metadata`:

```text
[Spotify: Stream/Output/Audio (stays at 100%)]
   │
   ├──▶ pw-record --target <spotify-serial>           (1:1 bit-exact capture at 100%)
   │
   └──▶ [pipewire_recorder_capture (virtual sink)]
           │
           ▼
        [PipeWire Recorder Playback (loopback stream)] ◀── adjust volume / mute HERE!
           │
           ▼
        [Default Sink / EasyEffects / Headphones]
```

### Enabling in `config.yaml`

```yaml
stream:
  mpris_player: "spotify"
  pipewire_target: "spotify"
  enforce_unity_volume: true
  virtual_sink:
    enabled: true
    sink_name: "pipewire_recorder_capture"
    sink_description: "PipeWire Recorder Capture (100%)"
    playback_name: "PipeWire Recorder Playback"
    playback_target: null   # null = system default (follows EasyEffects / Bluetooth switches)
    auto_route: true        # automatically routes Spotify to the virtual sink via pw-metadata
```

Or via CLI flag:

```bash
pipewire-recorder --virtual-sink run
```

### Usage & Notes

- **Control listening volume / mute**: Adjust **`PipeWire Recorder Playback`** in your desktop audio mixer (`pavucontrol`, Corona, Waybar, GNOME/KDE mixer, hardware mixer like `deej`). Other apps, EasyEffects, and the 100% recording remain unaffected.
- **Keep Spotify itself at 100%**: Leave Spotify's internal volume slider at 100% and disable "Normalize volume" in Spotify settings.
- **Never hijacks default sink**: The virtual sink is created with `priority.session = 0` and `priority.driver = 0` so WirePlumber will never promote it to the system default output device.
- **Clean teardown on exit**: Stopping the daemon (`Ctrl+C`, `SIGTERM`, or IPC `shutdown`) clears the `target.object` metadata override and stops `pw-loopback`, returning Spotify seamlessly to your default audio output.
- **EasyEffects compatibility (important when `virtual_sink` is enabled)**:  
  If **EasyEffects** has *"Process All Outputs"* enabled, it automatically pulls every app stream — including Spotify — straight back onto `easyeffects_sink`, bypassing `PipeWire Recorder Playback`.  
  **Fix:** Add `spotify` to the **EasyEffects blocklist** (*Output → Blocklist*, or in `~/.config/easyeffects/db/easyeffectsrc` under `[StreamOutputs]` set `blocklist=spotify,Spotify` and restart EasyEffects). EasyEffects will still process **`PipeWire Recorder Playback`**, so you keep all EQs/effects while listening while capture stays bit-exact.
- **Check status**: Run `pipewire-recorder status` or `pipewire-recorder list-streams` to inspect the active virtual sink and loopback playback node.

---

## Configuration (`~/.config/pipewire-recorder/`)

On first run, `pipewire-recorder` automatically creates `~/.config/pipewire-recorder/config.yaml` if no config file exists. JSON is also supported (search order: `config.yaml` → `config.yml` → `config.json`, or pass `--config <path>` explicitly).

See [config.example.yaml](./config.example.yaml) and [config.example.json](./config.example.json).

### Selecting a Different Player / Stream

1. Start your player and begin playback.
2. List all detected MPRIS players and PipeWire audio streams:
   ```bash
   pipewire-recorder list-streams
   ```
3. Configure `stream:` in `config.yaml`, e.g. for Firefox or VLC:
   ```yaml
   stream:
     mpris_player: "firefox"
     pipewire_target: "firefox"
   ```

### Output Directory & Path Template

```yaml
output_dir: "~/Music/Spotify"
path_template: "{artist}/{album}/{track_number} - {title}.flac"
```

Supported placeholders: `{artist}`, `{album}`, `{album_artist}`, `{track_number}`, `{disc_number}`, `{title}`, `{year}`.

---

## CLI Reference

```bash
pipewire-recorder run                  # Run the background recorder daemon (default)
pipewire-recorder run --ipc            # Run with NDJSON IPC on stdout/stdin (logs go to stderr)
pipewire-recorder run --ipc --paused   # Start in IPC mode with recording initially paused
pipewire-recorder list-streams         # List active MPRIS players & PipeWire output streams
pipewire-recorder init-config          # Write a default config file (--format yaml|json)
pipewire-recorder status               # Show loaded config, matched streams & index stats
pipewire-recorder list                 # List all recorded tracks in the index
pipewire-recorder verify-index         # Prune entries whose FLAC files no longer exist on disk
```

### Global Override Flags

| Flag | Description |
|---|---|
| `--config <PATH>` | Explicit path to `config.yaml` or `config.json` |
| `--output-dir <DIR>` | Override `output_dir` |
| `--player <NAME>` | Override `stream.mpris_player` |
| `--pipewire-target <TARGET>` | Override `stream.pipewire_target` |
| `--virtual-sink[=true\|false]` | Override `stream.virtual_sink.enabled` |
| `--enforce-unity-volume[=true\|false]` | Override `stream.enforce_unity_volume` |

---

## NDJSON IPC Protocol (`run --ipc`)

When started with `pipewire-recorder run --ipc`, diagnostic logs are directed to `stderr` and `stdout`/`stdin` speak newline-delimited JSON (one JSON object per line). Closing `stdin` triggers a clean shutdown automatically.

### Events (`stdout`, daemon → client)

- `{"type":"snapshot", ...}` — Full state snapshot emitted on startup, state changes, and once per second while recording (`recording_enabled`, `capturing`, `output_dir`, `mpris_player`, `pipewire_target`, `enforce_unity_volume`, `player`, `pw_node`, `virtual_sink`, `track`, `index_count`).
- `{"type":"track_saved","entry":{...},"index_count":42}` — Emitted after a complete track is encoded, tagged, and indexed.
- `{"type":"recent_tracks","entries":[...],"total":42}` — Most recently recorded tracks (newest first).
- `{"type":"streams","mpris_players":[...],"pipewire_nodes":[...]}` — Active MPRIS players and PipeWire output streams.
- `{"type":"index_verified","removed":2,"remaining":40}` — Result of `verify_index`.
- `{"type":"error","message":"..."}` — Runtime error notification.

### Commands (`stdin`, client → daemon)

- `{"cmd":"set_enabled","enabled":true|false}` — Pause or resume recording without stopping the virtual sink.
- `{"cmd":"set_virtual_sink","enabled":true|false}` — Toggle the virtual capture sink and auto-routing at runtime.
- `{"cmd":"set_enforce_unity_volume","enabled":true|false}` — Toggle 100% unity volume enforcement at runtime.
- `{"cmd":"set_target","mpris_player":"spotify","pipewire_target":"spotify"}` — Switch target player and PipeWire stream live.
- `{"cmd":"get_recent","limit":20}` — Request recent recorded entries from the index.
- `{"cmd":"list_streams"}` — Request active MPRIS players and PipeWire output streams.
- `{"cmd":"verify_index"}` — Prune missing files from `recorded_index.json`.
- `{"cmd":"shutdown"}` — Gracefully stop capture, restore stream routing, stop loopback, and exit.

---

## License

MIT
