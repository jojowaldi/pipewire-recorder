# `pipewire-recorder`

Automatischer Hintergrund-Daemon für Linux / PipeWire, der isolierte Audio-Streams von Spotify (oder beliebigen anderen MPRIS-fähigen Playern) mitschneidet, verlustfrei als **FLAC** speichert und automatisch vollständige Metadaten sowie Cover-Art einbettet.

## Features

- **Isolierter PipeWire-Abgriff**: Zeichnet ausschließlich den gezielten `Stream/Output/Audio`-Node der App auf (keine Benachrichtigungen, kein Voice-Chat, kein Browser-Audio).
- **Optionaler virtueller Capture-Sink + Loopback**: Lässt dich die Hörlautstärke von Spotify (oder einem anderen Ziel-Player) im Desktop-Mixer unabhängig von anderen Apps leiser drehen oder stummschalten, während die Aufnahme weiterhin 1:1 bei 100 % läuft.
- **Lückenlose Track-Übergänge**: Hält `pw-record` über Trackwechsel hinweg aktiv und tauscht den PCM-Buffer exakt an der Trackgrenze, sodass Song-Anfänge nicht abgeschnitten werden.
- **Strikte Vollständigkeitsprüfung**: Speichert nur Songs, die nahe `0:00` begonnen haben, ohne Seeking/Scrubbing, ohne Pause-Unterbrechungen (`allow_pause: false`) und in voller Track-Länge durchgelaufen sind. Werbung wird automatisch verworfen.
- **Zweistufige Duplikat-Erkennung**: Prüft sowohl einen persistenten Index (`~/.local/share/pipewire-recorder/recorded_index.json`) als auch ob die Zieldatei auf der Festplatte bereits existiert. Wurde eine Datei manuell gelöscht, wird sie bei erneutem Abspielen wieder aufgenommen.
- **Verlustfreies FLAC + Tags + Cover**: Speichert mit Vorbis-Kommentaren (`TITLE`, `ARTIST`, `ALBUM`, `ALBUMARTIST`, `TRACKNUMBER`, `DISCNUMBER`, `DATE`, `TRACK_ID`, `SPOTIFY_TRACK_ID`) und eingebettetem `COVER_FRONT`-Bild (`METADATA_BLOCK_PICTURE`).
- **YAML- oder JSON-Konfiguration**: Automatisch mit Standardwerten unter `~/.config/pipewire-recorder/config.yaml` (oder `config.json`).
- **Multi-Player-fähig**: Standardmäßig für Spotify vorkonfiguriert, aber per Config auf jeden anderen Player (Firefox, Chromium, VLC, Amberol, Cider, MPD, …) umstellbar.

---

## ⚠️ Wichtige Hinweise zu Audio-Qualität, Lautstärke & EasyEffects (Punkt 3 zum Nachschlagen)

Der Daemon nimmt den Audio-Stream **1:1 unverändert** auf (keine Normalisierung, kein DSP, `trim_silence: false` als Standard). Damit das Signal wirklich bit-exakt bleibt, solltest du die folgenden drei Ebenen kennen:

1. **System-/Master-Lautstärke (Kopfhörer, Lautsprecher, Hardware-Sink)**  
   → **Kein Einfluss auf die Aufnahme.** Die System-Lautstärke wirkt erst ganz am Ende auf dem Ausgabegerät (Sink), lange nachdem `pipewire-recorder` den App-Stream abgegriffen hat.

2. **EasyEffects / PipeWire-Filter-Sinks**  
   → **Kein Einfluss auf die Aufnahme.** `pw-record --target <node-serial>` greift den `Stream/Output/Audio`-Node der App direkt an der Quelle ab – **bevor** das Signal durch `easyeffects_sink` oder andere virtuelle Filter geleitet wird. Du kannst also weiterhin beliebige EQs/Effekte beim Hören nutzen, ohne dass sie mit aufgenommen werden.

3. **App-Stream-Lautstärke & Spotify-interne Normalisierung (Bitte beachten!)**
   - **App-Mixer (`channelVolumes` am PipeWire-Node) & Spotify-Lautstärkeregler (MPRIS `Volume`)**:  
     Diese Regler skalieren direkt die Samples, die der Player an PipeWire ausgibt – noch bevor die `output_FL`/`output_FR`-Ports des App-Nodes erreicht werden.  
     Standardmäßig ist `stream.enforce_unity_volume: true` aktiv: Der Daemon setzt beim Aufnahmestart sowohl das PipeWire-Node-Volume (`wpctl set-volume <id> 1.0`) als auch das MPRIS-Volume auf `1.0` (100 %) und verwirft die aktuelle Aufnahme sofort (`VolumeAttenuated`), falls während eines Songs die App-Lautstärke heruntergedreht oder stummgeschaltet wird.
   - **Lösung für unabhängige Hör-Lautstärke (`stream.virtual_sink.enabled: true`)**:  
     Wenn du Spotify **leiser als andere Apps (z. B. Discord/Browser) hören oder stummschalten** willst, ohne die 100-%-Aufnahme zu beeinflussen, aktiviere den eingebauten **virtuellen Capture-Sink + Loopback** (Details siehe unten).
   - **Spotify-App-Einstellung „Lautstärke anpassen“ (Normalize volume)**:  
     In Spotify selbst muss unter **Einstellungen → Audioqualität → „Lautstärke anpassen“** (*Normalize volume*) **deaktiviert** sein! Ist diese Option in Spotify aktiviert, rechnet der Spotify-Client selbst bereits intern eine ReplayGain-Lautstärkeanpassung in den PCM-Output ein, bevor PipeWire oder der Daemon das Audio zu sehen bekommen.

---

## 🎚️ Unabhängige Hör-Lautstärke (Virtueller Capture-Sink + Loopback)

Da PipeWire `channelVolumes` direkt im `Stream/Output/Audio`-Node einer App anwendet, führt das Leiserdrehen von Spotify im App-Mixer normalerweise dazu, dass auch die Aufnahme leiser wird (bzw. bei `enforce_unity_volume: true` verworfen wird).

Mit **`stream.virtual_sink.enabled: true`** entkoppelt `pipewire-recorder` Aufnahme und Hör-Lautstärke automatisch über `pw-loopback` + `pw-metadata`:

```text
[Spotify: Stream/Output/Audio (bleibt bei 100 %)]
   │
   ├──▶ pw-record --target <spotify-serial>           (1:1 bit-exakte Aufnahme bei 100 %)
   │
   └──▶ [pipewire_recorder_capture (virtueller Sink)]
           │
           ▼
        [PipeWire Recorder Playback (Loopback-Stream)] ◀── HIER im Mixer leiser drehen / muten!
           │
           ▼
        [Default Sink / EasyEffects / Kopfhörer]
```

### Aktivieren in `config.yaml`

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
    playback_target: null   # null = System-Default (inkl. EasyEffects / Bluetooth-Wechsel)
    auto_route: true        # routet Spotify automatisch via pw-metadata auf den virtuellen Sink
```

Oder testweise per CLI-Flag:

```bash
pipewire-recorder --virtual-sink run
```

### Bedienung & Verhalten

- **Hörlautstärke regeln / muten**: Stelle im Desktop-Audio-Mixer (`pavucontrol`, GNOME/KDE/Waybar-Mixer) die Lautstärke von **`PipeWire Recorder Playback`** ein (oder schalte ihn komplett stumm). Andere Apps (Browser, Discord, Spiele) bleiben davon völlig unberührt – ebenso EasyEffects und die 100-%-Aufnahme.
- **Spotify selbst**: Den Spotify-internen Regler auf **100 %** lassen und in Spotify **„Lautstärke anpassen“** deaktivieren (diese beiden Schritte passieren im Spotify-Prozess vor PipeWire).
- **Kein ungewollter Default-Sink**: Der virtuelle Sink wird mit `priority.session = 0` und `priority.driver = 0` angelegt, sodass er niemals versehentlich das Standard-Ausgabegerät übernimmt.
- **Sauberes Aufräumen beim Beenden**: Beim Stoppen des Daemons (`Ctrl+C` oder `systemctl --user stop pipewire-recorder`) wird die `target.object`-Zuordnung entfernt und `pw-loopback` beendet, sodass Spotify nahtlos wieder über dein reguläres Standard-Ausgabegerät weiterläuft.
- **Status prüfen**: Mit `pipewire-recorder status` und `pipewire-recorder list-streams` siehst du den aktiven virtuellen Sink sowie die aktuelle Hörlautstärke des Loopback-Streams.

---

## Konfiguration (`~/.config/pipewire-recorder/`)

Beim ersten Start erzeugt `pipewire-recorder` automatisch `~/.config/pipewire-recorder/config.yaml`, falls noch keine Konfiguration existiert. Alternativ kann auch `~/.config/pipewire-recorder/config.json` genutzt werden (Suchreihenfolge: `config.yaml` → `config.yml` → `config.json`, oder explizit per `--config <pfad>`).

Siehe [config.example.yaml](./config.example.yaml) und [config.example.json](./config.example.json).

### Andere Player / Streams auswählen

1. Player starten und Audio abspielen.
2. Alle erkannten MPRIS-Player und PipeWire-Streams anzeigen lassen:
   ```bash
   pipewire-recorder list-streams
   ```
3. In `config.yaml` unter `stream:` eintragen, z. B. für Firefox oder VLC:
   ```yaml
   stream:
     mpris_player: "firefox"
     pipewire_target: "firefox"
   ```

### Speicherort & Pfad-Template

```yaml
output_dir: "~/Music/Spotify"
path_template: "{artist}/{album}/{track_number} - {title}.flac"
```

Verfügbare Platzhalter: `{artist}`, `{album}`, `{album_artist}`, `{track_number}`, `{disc_number}`, `{title}`, `{year}`.

---

## CLI-Befehle

```bash
pipewire-recorder run                  # Daemon im Vordergrund starten (Standardbefehl)
pipewire-recorder list-streams         # Aktive MPRIS-Player & PipeWire Audio-Streams anzeigen
pipewire-recorder init-config          # Standard-Config schreiben (--format yaml|json)
pipewire-recorder status               # Aktuelle Config, aktive Streams & Index-Statistik prüfen
pipewire-recorder list                 # Alle bereits aufgenommenen Tracks aus dem Index auflisten
pipewire-recorder verify-index         # Nicht mehr vorhandene Dateien aus dem Index bereinigen
```

Globale Overrides: `--config <PFAD>`, `--output-dir <DIR>`, `--player <NAME>`, `--pipewire-target <TARGET>`, `--virtual-sink`.

---

## Nix / Home Manager Integration

In deiner Home-Manager-Konfiguration kannst du dieses Flake als Input einbinden:

```nix
# flake.nix
inputs.pipewire-recorder.url = "path:/home/jojowaldi/coding/pipewire-recorder";
```

Und das Modul aktivieren:

```nix
imports = [ inputs.pipewire-recorder.homeModules.default ];

services.pipewire-recorder = {
  enable = true;
  configFormat = "yaml"; # oder "json"
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
