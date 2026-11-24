use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::index::RecordedEntry;
use crate::mpris::{PlaybackStatus, PlayerSnapshot};
use crate::pipewire::PipeWireNode;
use crate::recorder::{InvalidationReason, RecordingSession};

fn default_recent_limit() -> usize {
    20
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcCommand {
    SetEnabled {
        enabled: bool,
    },
    SetVirtualSink {
        enabled: bool,
    },
    SetEnforceUnityVolume {
        enabled: bool,
    },
    SetTarget {
        mpris_player: String,
        pipewire_target: String,
    },
    GetRecent {
        #[serde(default = "default_recent_limit")]
        limit: usize,
    },
    ListStreams,
    VerifyIndex,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum IpcPlaybackStatus {
    Playing,
    Paused,
    Stopped,
    Unknown,
}

impl From<&PlaybackStatus> for IpcPlaybackStatus {
    fn from(status: &PlaybackStatus) -> Self {
        match status {
            PlaybackStatus::Playing => Self::Playing,
            PlaybackStatus::Paused => Self::Paused,
            PlaybackStatus::Stopped => Self::Stopped,
            PlaybackStatus::Unknown(_) => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IpcPlayerState {
    pub bus: String,
    pub status: IpcPlaybackStatus,
    pub volume: Option<f64>,
}

impl From<&PlayerSnapshot> for IpcPlayerState {
    fn from(snap: &PlayerSnapshot) -> Self {
        Self {
            bus: snap.bus_name.clone(),
            status: IpcPlaybackStatus::from(&snap.status),
            volume: snap.volume,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IpcNodeState {
    pub id: u32,
    pub serial: u32,
    pub app_name: String,
    pub binary_name: String,
    pub node_name: String,
    pub media_name: String,
    pub volume: Option<f64>,
    pub mute: bool,
    pub is_unity: bool,
}

impl From<&PipeWireNode> for IpcNodeState {
    fn from(node: &PipeWireNode) -> Self {
        let volume = if node.channel_volumes.is_empty() {
            None
        } else {
            Some(node.channel_volumes.iter().sum::<f64>() / node.channel_volumes.len() as f64)
        };
        Self {
            id: node.id,
            serial: node.serial,
            app_name: node.app_name.clone(),
            binary_name: node.binary_name.clone(),
            node_name: node.node_name.clone(),
            media_name: node.media_name.clone(),
            volume,
            mute: node.mute,
            is_unity: node.is_unity_volume(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IpcVirtualSinkState {
    pub enabled: bool,
    pub active: bool,
    pub sink_name: String,
    pub playback_name: String,
    pub playback_node_name: String,
    pub routed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IpcTrackStatus {
    Recording,
    Paused,
    Skipped,
    Invalidated,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IpcTrackState {
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub captured_ms: u64,
    pub status: IpcTrackStatus,
    pub reason: Option<String>,
}

impl IpcTrackState {
    pub fn from_session(
        session: &RecordingSession,
        recording_enabled: bool,
        is_paused: bool,
    ) -> Self {
        let (status, reason) = if let Some(ref inv) = session.invalidated {
            let st = match inv {
                InvalidationReason::Advertisement
                | InvalidationReason::Duplicate(_)
                | InvalidationReason::IncompleteStart { .. } => IpcTrackStatus::Skipped,
                _ => IpcTrackStatus::Invalidated,
            };
            (st, Some(inv.to_string()))
        } else if !recording_enabled {
            (
                IpcTrackStatus::Paused,
                Some("recording paused by user".to_string()),
            )
        } else if is_paused {
            (IpcTrackStatus::Paused, None)
        } else {
            (IpcTrackStatus::Recording, None)
        };

        Self {
            track_id: session.meta.track_identity_key(),
            title: session.meta.title.clone(),
            artist: session.meta.primary_artist().to_string(),
            album: session.meta.album.clone(),
            duration_ms: session.meta.duration_ms(),
            captured_ms: session.captured_ms(),
            status,
            reason,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IpcRecordedEntry {
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub file_path: String,
    pub recorded_at_unix: u64,
    pub duration_ms: u64,
}

impl From<&RecordedEntry> for IpcRecordedEntry {
    fn from(entry: &RecordedEntry) -> Self {
        Self {
            track_id: entry.track_id.clone(),
            title: entry.title.clone(),
            artist: entry.artist.clone(),
            album: entry.album.clone(),
            file_path: entry.file_path.to_string_lossy().to_string(),
            recorded_at_unix: entry.recorded_at_unix,
            duration_ms: entry.duration_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IpcMprisPlayerInfo {
    pub bus: String,
    pub short_name: String,
    pub status: IpcPlaybackStatus,
    pub track: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum IpcEvent {
    Snapshot {
        recording_enabled: bool,
        capturing: bool,
        output_dir: String,
        mpris_player: String,
        pipewire_target: String,
        enforce_unity_volume: bool,
        player: Option<IpcPlayerState>,
        pw_node: Option<IpcNodeState>,
        virtual_sink: IpcVirtualSinkState,
        track: Option<IpcTrackState>,
        index_count: usize,
    },
    TrackSaved {
        entry: IpcRecordedEntry,
        index_count: usize,
    },
    RecentTracks {
        entries: Vec<IpcRecordedEntry>,
        total: usize,
    },
    Streams {
        mpris_players: Vec<IpcMprisPlayerInfo>,
        pipewire_nodes: Vec<IpcNodeState>,
    },
    IndexVerified {
        removed: usize,
        remaining: usize,
    },
    Error {
        message: String,
    },
}

#[derive(Clone)]
pub struct IpcEmitter {
    tx: mpsc::UnboundedSender<IpcEvent>,
}

impl IpcEmitter {
    pub fn emit(&self, event: IpcEvent) {
        let _ = self.tx.send(event);
    }

    pub fn emit_error(&self, message: impl Into<String>) {
        self.emit(IpcEvent::Error {
            message: message.into(),
        });
    }
}

pub fn spawn_ipc_io() -> (IpcEmitter, mpsc::UnboundedReceiver<IpcCommand>) {
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<IpcEvent>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<IpcCommand>();

    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(event) = ev_rx.recv().await {
            match serde_json::to_string(&event) {
                Ok(mut line) => {
                    line.push('\n');
                    if let Err(err) = stdout.write_all(line.as_bytes()).await {
                        debug!(error = %err, "IPC stdout write failed; stopping emitter");
                        break;
                    }
                    if let Err(err) = stdout.flush().await {
                        debug!(error = %err, "IPC stdout flush failed; stopping emitter");
                        break;
                    }
                }
                Err(err) => {
                    warn!(error = %err, "Failed serializing IPC event");
                }
            }
        }
    });

    let err_emitter = IpcEmitter { tx: ev_tx.clone() };
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<IpcCommand>(trimmed) {
                Ok(cmd) => {
                    let is_shutdown = matches!(cmd, IpcCommand::Shutdown);
                    if cmd_tx.send(cmd).is_err() || is_shutdown {
                        break;
                    }
                }
                Err(err) => {
                    warn!(raw = %trimmed, error = %err, "Invalid IPC command JSON");
                    err_emitter.emit_error(format!("invalid command: {err}"));
                }
            }
        }
        // Stdin closed (parent exited) -> ensure graceful cleanup
        let _ = cmd_tx.send(IpcCommand::Shutdown);
    });

    (IpcEmitter { tx: ev_tx }, cmd_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipc_commands() {
        let cmd: IpcCommand = serde_json::from_str(r#"{"cmd":"set_enabled","enabled":false}"#)
            .expect("parse set_enabled");
        assert_eq!(cmd, IpcCommand::SetEnabled { enabled: false });

        let cmd: IpcCommand = serde_json::from_str(r#"{"cmd":"get_recent"}"#).expect("default");
        assert_eq!(cmd, IpcCommand::GetRecent { limit: 20 });

        let cmd: IpcCommand = serde_json::from_str(
            r#"{"cmd":"set_target","mpris_player":"firefox","pipewire_target":"firefox"}"#,
        )
        .expect("parse set_target");
        assert_eq!(
            cmd,
            IpcCommand::SetTarget {
                mpris_player: "firefox".to_string(),
                pipewire_target: "firefox".to_string(),
            }
        );

        let cmd: IpcCommand = serde_json::from_str(r#"{"cmd":"verify_index"}"#).expect("verify");
        assert_eq!(cmd, IpcCommand::VerifyIndex);
    }

    #[test]
    fn serializes_snapshot_and_saved_events_as_tagged_json() {
        let ev = IpcEvent::IndexVerified {
            removed: 2,
            remaining: 15,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"index_verified""#));
        assert!(json.contains(r#""removed":2"#));
        assert!(json.contains(r#""remaining":15"#));
    }
}
