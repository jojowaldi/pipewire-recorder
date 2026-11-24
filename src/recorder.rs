use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_lite::StreamExt;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use zbus::message::Type;
use zbus::{Connection, MatchRule, MessageStream};

use crate::config::Config;
use crate::index::{RecordedIndex, render_track_path};
use crate::ipc::{
    IpcCommand, IpcEmitter, IpcEvent, IpcMprisPlayerInfo, IpcNodeState, IpcPlaybackStatus,
    IpcPlayerState, IpcRecordedEntry, IpcTrackState, IpcVirtualSinkState, spawn_ipc_io,
};
use crate::mpris::{
    PlaybackStatus, PlayerSnapshot, TrackMetadata, find_active_player_bus, list_mpris_players,
    query_player_snapshot, resolve_name_owner, set_mpris_volume,
};
use crate::pipewire::{
    PipeWireNode, PipeWireSink, PwLoopbackProcess, PwRecorderProcess, PwStreamEvent,
    clear_stream_route, enforce_node_unity_volume, find_sink_by_name, find_target_node,
    list_audio_output_streams, route_stream_to_sink, wait_for_virtual_sink,
};
use crate::tagger::{CoverArtCache, encode_and_tag_flac, fetch_cover_art};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidationReason {
    Advertisement,
    Duplicate(PathBuf),
    IncompleteStart { offset_ms: u64, max_allowed_ms: u64 },
    SeekDetected { jump_ms: i64 },
    PausedMidTrack,
    VolumeAttenuated,
    TooShort { captured_ms: u64, expected_ms: u64 },
    MissingMetadata,
}

impl std::fmt::Display for InvalidationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Advertisement => write!(f, "advertisement track"),
            Self::Duplicate(path) => write!(f, "already recorded at {}", path.display()),
            Self::IncompleteStart {
                offset_ms,
                max_allowed_ms,
            } => write!(
                f,
                "started mid-track at {}ms (max allowed {}ms)",
                offset_ms, max_allowed_ms
            ),
            Self::SeekDetected { jump_ms } => {
                write!(f, "user seek detected (jump {}ms)", jump_ms)
            }
            Self::PausedMidTrack => write!(f, "playback paused mid-track"),
            Self::VolumeAttenuated => write!(f, "stream or player volume attenuated below 100%"),
            Self::TooShort {
                captured_ms,
                expected_ms,
            } => write!(f, "captured {}ms < expected {}ms", captured_ms, expected_ms),
            Self::MissingMetadata => write!(f, "missing track title or length metadata"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordingSession {
    pub meta: TrackMetadata,
    pub pcm_samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
    pub started_at_offset_ms: u64,
    pub invalidated: Option<InvalidationReason>,
    pub skip_buffering: bool,
    pub last_known_position_us: i64,
    pub last_position_at: Instant,
}

impl RecordingSession {
    pub fn new(
        meta: TrackMetadata,
        sample_rate: u32,
        channels: u16,
        started_at_offset_ms: u64,
        position_us: i64,
        skip_reason: Option<InvalidationReason>,
    ) -> Self {
        let skip_buffering = skip_reason.is_some();
        Self {
            meta,
            pcm_samples: Vec::new(),
            sample_rate,
            channels,
            started_at_offset_ms,
            invalidated: skip_reason,
            skip_buffering,
            last_known_position_us: position_us,
            last_position_at: Instant::now(),
        }
    }

    pub fn push_samples(&mut self, samples: &[i16]) {
        if !self.skip_buffering && self.invalidated.is_none() {
            self.pcm_samples.extend_from_slice(samples);
        }
    }

    pub fn invalidate(&mut self, reason: InvalidationReason) {
        if self.invalidated.is_none() {
            self.invalidated = Some(reason);
        }
        self.skip_buffering = true;
        self.pcm_samples.clear();
        self.pcm_samples.shrink_to_fit();
    }

    pub fn captured_ms(&self) -> u64 {
        let ch = self.channels.max(1) as u64;
        let rate = self.sample_rate.max(1) as u64;
        let frames = (self.pcm_samples.len() as u64) / ch;
        frames.saturating_mul(1000) / rate
    }
}

pub fn validate_completeness(
    session: &RecordingSession,
    config: &Config,
) -> std::result::Result<(), InvalidationReason> {
    if let Some(ref reason) = session.invalidated {
        return Err(reason.clone());
    }

    if session.meta.is_advertisement() {
        return Err(InvalidationReason::Advertisement);
    }

    if session.meta.title.trim().is_empty() || session.meta.length_us == 0 {
        return Err(InvalidationReason::MissingMetadata);
    }

    if session.started_at_offset_ms > config.completeness.max_start_offset_ms {
        return Err(InvalidationReason::IncompleteStart {
            offset_ms: session.started_at_offset_ms,
            max_allowed_ms: config.completeness.max_start_offset_ms,
        });
    }

    let captured_ms = session.captured_ms();
    let expected_ms = session.meta.duration_ms();

    let min_ratio_ms = ((expected_ms as f64) * config.completeness.min_completion_ratio) as u64;
    let min_tol_ms = expected_ms.saturating_sub(config.completeness.duration_tolerance_ms);
    let required_ms = min_ratio_ms.min(min_tol_ms);

    if captured_ms < required_ms {
        return Err(InvalidationReason::TooShort {
            captured_ms,
            expected_ms,
        });
    }

    Ok(())
}

pub fn trim_silence_frames(
    samples: &[i16],
    channels: u16,
    sample_rate: u32,
    threshold_db: f32,
    max_trim_ms: u64,
) -> &[i16] {
    let ch = channels.max(1) as usize;
    if samples.len() < ch {
        return samples;
    }

    let threshold_amp = (10.0_f32.powf(threshold_db / 20.0) * 32768.0)
        .round()
        .clamp(0.0, 32767.0) as u16;
    let max_frames = ((sample_rate as u64).saturating_mul(max_trim_ms) / 1000) as usize;

    let total_frames = samples.len() / ch;
    let usable_len = total_frames * ch;
    let aligned = &samples[..usable_len];

    let mut start_frame = 0usize;
    while start_frame < total_frames && start_frame < max_frames {
        let frame = &aligned[start_frame * ch..(start_frame + 1) * ch];
        if frame.iter().any(|s| s.unsigned_abs() > threshold_amp) {
            break;
        }
        start_frame += 1;
    }

    let mut end_frame = total_frames;
    let mut trimmed_end = 0usize;
    while end_frame > start_frame && trimmed_end < max_frames {
        let frame = &aligned[(end_frame - 1) * ch..end_frame * ch];
        if frame.iter().any(|s| s.unsigned_abs() > threshold_amp) {
            break;
        }
        end_frame -= 1;
        trimmed_end += 1;
    }

    &aligned[start_frame * ch..end_frame * ch]
}

fn same_track(a: &TrackMetadata, b: &TrackMetadata) -> bool {
    a.track_identity_key() == b.track_identity_key()
        && a.title == b.title
        && a.length_us == b.length_us
}

pub async fn run_daemon(config: Config, config_path: PathBuf) -> Result<()> {
    run_daemon_inner(config, config_path, None, None, true).await
}

pub async fn run_daemon_ipc(
    config: Config,
    config_path: PathBuf,
    start_enabled: bool,
) -> Result<()> {
    let (emitter, cmd_rx) = spawn_ipc_io();
    run_daemon_inner(
        config,
        config_path,
        Some(emitter),
        Some(cmd_rx),
        start_enabled,
    )
    .await
}

async fn emit_recent_tracks(emitter: &IpcEmitter, index: &Arc<Mutex<RecordedIndex>>, limit: usize) {
    let guard = index.lock().await;
    let total = guard.entries.len();
    let mut sorted: Vec<_> = guard.entries.iter().collect();
    sorted.sort_by_key(|a| std::cmp::Reverse(a.recorded_at_unix));
    let entries = sorted
        .into_iter()
        .take(limit)
        .map(IpcRecordedEntry::from)
        .collect();
    emitter.emit(IpcEvent::RecentTracks { entries, total });
}

#[allow(clippy::too_many_arguments)]
async fn emit_snapshot_if_changed(
    emitter: Option<&IpcEmitter>,
    last_emitted: &mut Option<IpcEvent>,
    config: &Config,
    recording_enabled: bool,
    capturing: bool,
    last_player: Option<&PlayerSnapshot>,
    current_pw_node: Option<&PipeWireNode>,
    active_virtual_sink: Option<&PipeWireSink>,
    routed_stream: Option<(u32, u32)>,
    active_session: Option<&RecordingSession>,
    paused_since: Option<Instant>,
    index: &Arc<Mutex<RecordedIndex>>,
) {
    let Some(emitter) = emitter else { return };
    let index_count = index.lock().await.entries.len();
    let event = IpcEvent::Snapshot {
        recording_enabled,
        capturing,
        output_dir: config.resolved_output_dir().to_string_lossy().to_string(),
        mpris_player: config.stream.mpris_player.clone(),
        pipewire_target: config.stream.pipewire_target.clone(),
        enforce_unity_volume: config.stream.enforce_unity_volume,
        player: last_player.map(IpcPlayerState::from),
        pw_node: current_pw_node.map(IpcNodeState::from),
        virtual_sink: IpcVirtualSinkState {
            enabled: config.stream.virtual_sink.enabled,
            active: active_virtual_sink.is_some(),
            sink_name: config.stream.virtual_sink.sink_name.clone(),
            playback_name: config.stream.virtual_sink.playback_name.clone(),
            playback_node_name: config.stream.virtual_sink.playback_node_name(),
            routed: routed_stream.is_some(),
        },
        track: active_session
            .map(|s| IpcTrackState::from_session(s, recording_enabled, paused_since.is_some())),
        index_count,
    };

    if last_emitted.as_ref() != Some(&event) {
        emitter.emit(event.clone());
        *last_emitted = Some(event);
    }
}

async fn run_daemon_inner(
    mut config: Config,
    config_path: PathBuf,
    emitter: Option<IpcEmitter>,
    mut cmd_rx: Option<tokio::sync::mpsc::UnboundedReceiver<IpcCommand>>,
    mut recording_enabled: bool,
) -> Result<()> {
    let conn = Connection::session().await?;
    let index_path = config.resolved_index_path();
    let initial_index = RecordedIndex::load(&index_path)?;
    let index = Arc::new(Mutex::new(initial_index));
    let art_cache = Arc::new(Mutex::new(CoverArtCache::default()));

    info!(
        config = %config_path.display(),
        output_dir = %config.resolved_output_dir().display(),
        mpris_player = %config.stream.mpris_player,
        pipewire_target = %config.stream.pipewire_target,
        trim_silence = config.audio.trim_silence,
        enforce_unity_volume = config.stream.enforce_unity_volume,
        virtual_sink_enabled = config.stream.virtual_sink.enabled,
        recording_enabled,
        "Started pipewire-recorder daemon"
    );

    let mpris_rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .path("/org/mpris/MediaPlayer2")?
        .build();
    let mut mpris_stream = MessageStream::for_match_rule(mpris_rule, &conn, None).await?;

    let dbus_rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .build();
    let mut dbus_stream = MessageStream::for_match_rule(dbus_rule, &conn, None).await?;

    let mut active_bus: Option<String> =
        find_active_player_bus(&conn, &config.stream.mpris_player).await?;
    let mut active_owner: Option<String> = match active_bus.as_deref() {
        Some(bus) => resolve_name_owner(&conn, bus).await.ok(),
        None => None,
    };
    let mut last_player: Option<PlayerSnapshot> = None;
    let mut active_session: Option<RecordingSession> = None;
    let mut active_pw: Option<(
        PwRecorderProcess,
        tokio::sync::mpsc::Receiver<PwStreamEvent>,
    )> = None;
    let mut current_pw_node: Option<PipeWireNode> = None;
    let mut active_loopback: Option<PwLoopbackProcess> = None;
    let mut active_virtual_sink: Option<PipeWireSink> = None;
    let mut routed_stream: Option<(u32, u32)> = None;
    let mut paused_since: Option<Instant> = None;
    let mut last_emitted: Option<IpcEvent> = None;

    if config.stream.virtual_sink.enabled {
        ensure_virtual_sink_running(
            &config,
            &mut active_loopback,
            &mut active_virtual_sink,
            &mut routed_stream,
        )
        .await;

        if config.stream.virtual_sink.auto_route
            && let Ok(Some(node)) = find_target_node(&config.stream.pipewire_target).await
        {
            maybe_route_target_stream(&config, &node, &active_virtual_sink, &mut routed_stream)
                .await;
            current_pw_node = Some(node);
        }
    }

    if let Some(ref bus) = active_bus {
        info!(bus_name = %bus, "Found target MPRIS player");
        if let Ok(snap) = query_player_snapshot(&conn, bus).await {
            last_player = Some(snap.clone());
            handle_snapshot(
                &conn,
                &config,
                recording_enabled,
                &index,
                &index_path,
                &art_cache,
                emitter.as_ref(),
                snap,
                &mut active_session,
                &mut paused_since,
            )
            .await;
        }
    } else {
        info!(
            target = %config.stream.mpris_player,
            "Waiting for MPRIS player to appear on D-Bus"
        );
    }

    if let Some(ref em) = emitter {
        emit_recent_tracks(em, &index, 15).await;
    }
    emit_snapshot_if_changed(
        emitter.as_ref(),
        &mut last_emitted,
        &config,
        recording_enabled,
        active_pw.is_some(),
        last_player.as_ref(),
        current_pw_node.as_ref(),
        active_virtual_sink.as_ref(),
        routed_stream,
        active_session.as_ref(),
        paused_since,
        &index,
    )
    .await;

    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            if let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
                return;
            }
        }
        let _ = ctrl_c.await;
    };
    tokio::pin!(shutdown);

    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("Received shutdown signal; stopping recorder");
                if let Some((pw, _)) = active_pw.take() {
                    pw.stop().await;
                }
                if let Some(session) = active_session.take() {
                    maybe_finalize_session(
                        session,
                        &config,
                        &index,
                        &index_path,
                        &art_cache,
                        emitter.clone(),
                    )
                    .await;
                }
                if let Some((node_id, _)) = routed_stream.take()
                    && let Err(err) = clear_stream_route(node_id).await
                {
                    debug!(node_id, error = %err, "Failed clearing target.object route on shutdown");
                }
                if let Some(lb) = active_loopback.take() {
                    lb.stop().await;
                }
                break;
            }

            Some(ipc_cmd) = async {
                match cmd_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match ipc_cmd {
                    IpcCommand::Shutdown => {
                        info!("Received IPC shutdown command; stopping recorder");
                        if let Some((pw, _)) = active_pw.take() {
                            pw.stop().await;
                        }
                        if let Some(session) = active_session.take() {
                            maybe_finalize_session(
                                session,
                                &config,
                                &index,
                                &index_path,
                                &art_cache,
                                emitter.clone(),
                            )
                            .await;
                        }
                        if let Some((node_id, _)) = routed_stream.take()
                            && let Err(err) = clear_stream_route(node_id).await
                        {
                            debug!(node_id, error = %err, "Failed clearing target.object route on IPC shutdown");
                        }
                        if let Some(lb) = active_loopback.take() {
                            lb.stop().await;
                        }
                        break;
                    }
                    IpcCommand::SetEnabled { enabled } => {
                        if recording_enabled != enabled {
                            recording_enabled = enabled;
                            info!(recording_enabled, "Updated recording enabled state via IPC");
                            if !recording_enabled {
                                if let Some((pw, _)) = active_pw.take() {
                                    pw.stop().await;
                                }
                                if let Some(ref mut session) = active_session {
                                    session.skip_buffering = true;
                                    session.pcm_samples.clear();
                                    session.pcm_samples.shrink_to_fit();
                                }
                            } else {
                                // Re-evaluate current player state cleanly (mid-track will skip until next track)
                                active_session = None;
                                if let Some(ref bus) = active_bus
                                    && let Ok(snap) = query_player_snapshot(&conn, bus).await
                                {
                                    last_player = Some(snap.clone());
                                    handle_snapshot(
                                        &conn,
                                        &config,
                                        recording_enabled,
                                        &index,
                                        &index_path,
                                        &art_cache,
                                        emitter.as_ref(),
                                        snap,
                                        &mut active_session,
                                        &mut paused_since,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                    IpcCommand::SetVirtualSink { enabled } => {
                        config.stream.virtual_sink.enabled = enabled;
                        info!(enabled, "Updated virtual sink setting via IPC");
                        if enabled {
                            ensure_virtual_sink_running(
                                &config,
                                &mut active_loopback,
                                &mut active_virtual_sink,
                                &mut routed_stream,
                            )
                            .await;
                            if config.stream.virtual_sink.auto_route
                                && let Ok(Some(node)) = find_target_node(&config.stream.pipewire_target).await
                            {
                                maybe_route_target_stream(
                                    &config,
                                    &node,
                                    &active_virtual_sink,
                                    &mut routed_stream,
                                )
                                .await;
                                current_pw_node = Some(node);
                            }
                        } else {
                            if let Some((node_id, _)) = routed_stream.take()
                                && let Err(err) = clear_stream_route(node_id).await
                            {
                                debug!(node_id, error = %err, "Failed clearing target.object route");
                            }
                            if let Some(lb) = active_loopback.take() {
                                lb.stop().await;
                            }
                            active_virtual_sink = None;
                        }
                    }
                    IpcCommand::SetEnforceUnityVolume { enabled } => {
                        config.stream.enforce_unity_volume = enabled;
                        info!(enabled, "Updated enforce_unity_volume setting via IPC");
                    }
                    IpcCommand::SetTarget {
                        mpris_player,
                        pipewire_target,
                    } => {
                        info!(%mpris_player, %pipewire_target, "Switching recording target via IPC");
                        if let Some((pw, _)) = active_pw.take() {
                            pw.stop().await;
                        }
                        if let Some(session) = active_session.take() {
                            maybe_finalize_session(
                                session,
                                &config,
                                &index,
                                &index_path,
                                &art_cache,
                                emitter.clone(),
                            )
                            .await;
                        }
                        if let Some((node_id, _)) = routed_stream.take() {
                            let _ = clear_stream_route(node_id).await;
                        }
                        config.stream.mpris_player = mpris_player;
                        config.stream.pipewire_target = pipewire_target;
                        active_bus = find_active_player_bus(&conn, &config.stream.mpris_player)
                            .await
                            .ok()
                            .flatten();
                        active_owner = match active_bus.as_deref() {
                            Some(bus) => resolve_name_owner(&conn, bus).await.ok(),
                            None => None,
                        };
                        last_player = None;
                        current_pw_node = None;
                        paused_since = None;

                        if let Some(ref bus) = active_bus
                            && let Ok(snap) = query_player_snapshot(&conn, bus).await
                        {
                            last_player = Some(snap.clone());
                            handle_snapshot(
                                &conn,
                                &config,
                                recording_enabled,
                                &index,
                                &index_path,
                                &art_cache,
                                emitter.as_ref(),
                                snap,
                                &mut active_session,
                                &mut paused_since,
                            )
                            .await;
                        }
                        if config.stream.virtual_sink.enabled
                            && config.stream.virtual_sink.auto_route
                            && let Ok(Some(node)) = find_target_node(&config.stream.pipewire_target).await
                        {
                            maybe_route_target_stream(
                                &config,
                                &node,
                                &active_virtual_sink,
                                &mut routed_stream,
                            )
                            .await;
                            current_pw_node = Some(node);
                        }
                    }
                    IpcCommand::GetRecent { limit } => {
                        if let Some(ref em) = emitter {
                            emit_recent_tracks(em, &index, limit.clamp(1, 200)).await;
                        }
                    }
                    IpcCommand::ListStreams => {
                        if let Some(ref em) = emitter {
                            let mut mpris_players = Vec::new();
                            if let Ok(buses) = list_mpris_players(&conn).await {
                                for bus in buses {
                                    let short_name = bus
                                        .strip_prefix("org.mpris.MediaPlayer2.")
                                        .unwrap_or(&bus)
                                        .to_string();
                                    let (status, track) = match query_player_snapshot(&conn, &bus).await {
                                        Ok(snap) => {
                                            let track = snap.metadata.as_ref().map(|m| {
                                                format!("{} - {}", m.primary_artist(), m.title)
                                            });
                                            (IpcPlaybackStatus::from(&snap.status), track)
                                        }
                                        Err(_) => (IpcPlaybackStatus::Unknown, None),
                                    };
                                    mpris_players.push(IpcMprisPlayerInfo {
                                        bus,
                                        short_name,
                                        status,
                                        track,
                                    });
                                }
                            }
                            let pipewire_nodes = list_audio_output_streams()
                                .await
                                .unwrap_or_default()
                                .iter()
                                .map(IpcNodeState::from)
                                .collect();
                            em.emit(IpcEvent::Streams {
                                mpris_players,
                                pipewire_nodes,
                            });
                        }
                    }
                    IpcCommand::VerifyIndex => {
                        let (removed, remaining) = {
                            let mut guard = index.lock().await;
                            let removed = guard.prune_missing();
                            if let Err(err) = guard.save(&index_path)
                                && let Some(ref em) = emitter
                            {
                                em.emit_error(format!("failed saving index: {err}"));
                            }
                            (removed, guard.entries.len())
                        };
                        if let Some(ref em) = emitter {
                            em.emit(IpcEvent::IndexVerified { removed, remaining });
                            emit_recent_tracks(em, &index, 15).await;
                        }
                    }
                }

                emit_snapshot_if_changed(
                    emitter.as_ref(),
                    &mut last_emitted,
                    &config,
                    recording_enabled,
                    active_pw.is_some(),
                    last_player.as_ref(),
                    current_pw_node.as_ref(),
                    active_virtual_sink.as_ref(),
                    routed_stream,
                    active_session.as_ref(),
                    paused_since,
                    &index,
                )
                .await;
            }

            Some(pw_event) = async {
                match active_pw.as_mut() {
                    Some((_, rx)) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match pw_event {
                    PwStreamEvent::Samples(samples) => {
                        if recording_enabled
                            && let Some(ref mut session) = active_session
                        {
                            session.push_samples(&samples);
                        }
                    }
                    PwStreamEvent::Ended(err_msg) => {
                        warn!(
                            error = ?err_msg,
                            "pw-record stream ended unexpectedly; resetting capture"
                        );
                        if let Some((pw, _)) = active_pw.take() {
                            pw.stop().await;
                        }
                        current_pw_node = None;
                    }
                }
            }

            Some(msg_res) = mpris_stream.next() => {
                let Ok(msg) = msg_res else { continue };
                let Some(ref bus) = active_bus else { continue };

                // Ignore MPRIS signals from unrelated players (e.g. browser tabs)
                if let Some(ref owner) = active_owner
                    && let Some(sender) = msg.header().sender()
                    && sender.as_str() != owner.as_str()
                {
                    continue;
                }

                let member = msg.header().member().map(|m| m.as_str().to_string());
                match member.as_deref() {
                    Some("Seeked") => {
                        let body = msg.body();
                        if let Ok(pos_us) = body.deserialize::<i64>()
                            && let Some(ref mut session) = active_session
                            && recording_enabled
                        {
                                let elapsed_us = session.last_position_at.elapsed().as_micros() as i64;
                                let expected_us = session.last_known_position_us + elapsed_us;
                                let jump_us = pos_us - expected_us;
                                let captured_ms = session.captured_ms();

                                // Ignore benign Seeked(0) emitted right at track start
                                if jump_us.abs() > 1_500_000 && !(pos_us <= 1_500_000 && captured_ms <= 1500) {
                                    warn!(
                                        title = %session.meta.title,
                                        jump_ms = jump_us / 1000,
                                        "Invalidating active track due to user seek"
                                    );
                                    session.invalidate(InvalidationReason::SeekDetected {
                                        jump_ms: jump_us / 1000,
                                    });
                                } else {
                                    session.last_known_position_us = pos_us;
                                    session.last_position_at = Instant::now();
                                }
                        }
                    }
                    Some("PropertiesChanged") => {
                        if let Ok(snap) = query_player_snapshot(&conn, bus).await {
                            last_player = Some(snap.clone());
                            handle_snapshot(
                                &conn,
                                &config,
                                recording_enabled,
                                &index,
                                &index_path,
                                &art_cache,
                                emitter.as_ref(),
                                snap,
                                &mut active_session,
                                &mut paused_since,
                            ).await;
                        }
                    }
                    _ => {}
                }

                emit_snapshot_if_changed(
                    emitter.as_ref(),
                    &mut last_emitted,
                    &config,
                    recording_enabled,
                    active_pw.is_some(),
                    last_player.as_ref(),
                    current_pw_node.as_ref(),
                    active_virtual_sink.as_ref(),
                    routed_stream,
                    active_session.as_ref(),
                    paused_since,
                    &index,
                )
                .await;
            }

            Some(msg_res) = dbus_stream.next() => {
                if let Ok(msg) = msg_res {
                    let body = msg.body();
                    if let Ok((name, _old_owner, new_owner)) =
                        body.deserialize::<(String, String, String)>()
                    {
                        let name: String = name;
                        if crate::mpris::matches_mpris_bus_name(&name, &config.stream.mpris_player) {
                            if !new_owner.is_empty() && active_bus.is_none() {
                                info!(bus_name = %name, "Target MPRIS player appeared");
                                active_bus = Some(name.clone());
                                active_owner = Some(new_owner);
                                if let Ok(snap) = query_player_snapshot(&conn, &name).await {
                                    last_player = Some(snap.clone());
                                    handle_snapshot(
                                        &conn,
                                        &config,
                                        recording_enabled,
                                        &index,
                                        &index_path,
                                        &art_cache,
                                        emitter.as_ref(),
                                        snap,
                                        &mut active_session,
                                        &mut paused_since,
                                    ).await;
                                }
                            } else if new_owner.is_empty() && active_bus.as_deref() == Some(name.as_str()) {
                                info!(bus_name = %name, "Target MPRIS player closed");
                                active_bus = None;
                                active_owner = None;
                                last_player = None;
                                if let Some(session) = active_session.take() {
                                    maybe_finalize_session(
                                        session,
                                        &config,
                                        &index,
                                        &index_path,
                                        &art_cache,
                                        emitter.clone(),
                                    ).await;
                                }
                                if let Some((pw, _)) = active_pw.take() {
                                    pw.stop().await;
                                }
                                current_pw_node = None;
                                routed_stream = None;
                            }
                            emit_snapshot_if_changed(
                                emitter.as_ref(),
                                &mut last_emitted,
                                &config,
                                recording_enabled,
                                active_pw.is_some(),
                                last_player.as_ref(),
                                current_pw_node.as_ref(),
                                active_virtual_sink.as_ref(),
                                routed_stream,
                                active_session.as_ref(),
                                paused_since,
                                &index,
                            )
                            .await;
                        }
                    }
                }
            }

            _ = tick.tick() => {
                if active_bus.is_none() {
                    if let Ok(Some(bus)) = find_active_player_bus(&conn, &config.stream.mpris_player).await {
                        info!(bus_name = %bus, "Discovered target MPRIS player");
                        active_owner = resolve_name_owner(&conn, &bus).await.ok();
                        active_bus = Some(bus.clone());
                        if let Ok(snap) = query_player_snapshot(&conn, &bus).await {
                            last_player = Some(snap.clone());
                            handle_snapshot(
                                &conn,
                                &config,
                                recording_enabled,
                                &index,
                                &index_path,
                                &art_cache,
                                emitter.as_ref(),
                                snap,
                                &mut active_session,
                                &mut paused_since,
                            ).await;
                        }
                    }
                } else if let Some(ref bus) = active_bus {
                    // Periodic poll catches silent seeks or player volume changes that don't emit signals
                    if let Ok(snap) = query_player_snapshot(&conn, bus).await {
                        last_player = Some(snap.clone());
                        if recording_enabled
                            && config.stream.enforce_unity_volume
                            && let Some(vol) = snap.volume
                            && (vol - 1.0).abs() > 0.015
                            && let Some(ref mut session) = active_session
                            && !session.skip_buffering
                        {
                            warn!(
                                title = %session.meta.title,
                                volume = vol,
                                "Invalidating take: MPRIS player volume attenuated below 100%"
                            );
                            session.invalidate(InvalidationReason::VolumeAttenuated);
                        }

                        if recording_enabled
                            && let Some(ref mut session) = active_session
                            && snap.status == PlaybackStatus::Playing
                        {
                            let elapsed_us = session.last_position_at.elapsed().as_micros() as i64;
                            let expected_us = session.last_known_position_us + elapsed_us;
                            if (snap.position_us - expected_us).abs() > 2_500_000
                                && !(snap.position_us <= 1_500_000 && session.captured_ms() <= 1500)
                            {
                                warn!(
                                    title = %session.meta.title,
                                    "Invalidating take: playback position discontinuity detected"
                                );
                                session.invalidate(InvalidationReason::SeekDetected {
                                    jump_ms: (snap.position_us - expected_us) / 1000,
                                });
                            } else {
                                session.last_known_position_us = snap.position_us;
                                session.last_position_at = Instant::now();
                            }
                        }
                    }
                }

                // Keep virtual capture sink + playback loopback alive when enabled
                if config.stream.virtual_sink.enabled {
                    ensure_virtual_sink_running(
                        &config,
                        &mut active_loopback,
                        &mut active_virtual_sink,
                        &mut routed_stream,
                    )
                    .await;
                }

                // Manage PipeWire capture lifecycle, virtual-sink routing, and node volume monitoring
                let should_capture = recording_enabled
                    && active_session
                        .as_ref()
                        .is_some_and(|s| !s.skip_buffering && paused_since.is_none());
                let should_poll_node = should_capture
                    || (config.stream.virtual_sink.enabled && config.stream.virtual_sink.auto_route)
                    || (emitter.is_some() && active_bus.is_some());

                if should_poll_node {
                    match find_target_node(&config.stream.pipewire_target).await {
                        Ok(Some(node)) => {
                            if config.stream.virtual_sink.enabled
                                && config.stream.virtual_sink.auto_route
                            {
                                maybe_route_target_stream(
                                    &config,
                                    &node,
                                    &active_virtual_sink,
                                    &mut routed_stream,
                                )
                                .await;
                            }

                            if should_capture {
                                if config.stream.enforce_unity_volume && !node.is_unity_volume() {
                                    if active_pw.is_some() {
                                        if let Some(ref mut session) = active_session {
                                            warn!(
                                                title = %session.meta.title,
                                                node_volume = %node.display_volume(),
                                                "Invalidating take: PipeWire stream volume attenuated/muted"
                                            );
                                            session.invalidate(InvalidationReason::VolumeAttenuated);
                                        }
                                    } else if let Err(err) = enforce_node_unity_volume(&node).await {
                                        warn!("Failed setting unity volume on PipeWire node: {}", err);
                                    }
                                }

                                let needs_respawn = active_pw
                                    .as_ref()
                                    .is_none_or(|(pw, _)| pw.target_serial != node.serial);

                                if needs_respawn {
                                    if let Some((old_pw, _)) = active_pw.take() {
                                        old_pw.stop().await;
                                    }
                                    if config.stream.enforce_unity_volume && !node.is_unity_volume() {
                                        let _ = enforce_node_unity_volume(&node).await;
                                    }
                                    match PwRecorderProcess::spawn(
                                        &node,
                                        config.stream.sample_rate,
                                        config.stream.channels,
                                    ).await {
                                        Ok((pw, rx)) => {
                                            info!(
                                                node_id = node.id,
                                                serial = node.serial,
                                                app = %node.app_name,
                                                "Attached pw-record to PipeWire stream"
                                            );
                                            active_pw = Some((pw, rx));
                                            current_pw_node = Some(node);
                                        }
                                        Err(err) => {
                                            error!("Failed to spawn pw-record: {}", err);
                                            if let Some(ref em) = emitter {
                                                em.emit_error(format!("failed to spawn pw-record: {err}"));
                                            }
                                        }
                                    }
                                } else {
                                    current_pw_node = Some(node);
                                }
                            } else {
                                current_pw_node = Some(node);
                            }
                        }
                        Ok(None) => {
                            routed_stream = None;
                            if current_pw_node.is_some() {
                                debug!(
                                    target = %config.stream.pipewire_target,
                                    "PipeWire output stream not currently present"
                                );
                                current_pw_node = None;
                            }
                        }
                        Err(err) => {
                            warn!("pw-dump lookup error: {}", err);
                        }
                    }
                }

                if !should_capture
                    && let Some(since) = paused_since
                    && since.elapsed() >= Duration::from_secs(config.audio.idle_timeout_secs)
                    && let Some((pw, _)) = active_pw.take()
                {
                    info!("Stopping pw-record after idle timeout");
                    pw.stop().await;
                }

                emit_snapshot_if_changed(
                    emitter.as_ref(),
                    &mut last_emitted,
                    &config,
                    recording_enabled,
                    active_pw.is_some(),
                    last_player.as_ref(),
                    current_pw_node.as_ref(),
                    active_virtual_sink.as_ref(),
                    routed_stream,
                    active_session.as_ref(),
                    paused_since,
                    &index,
                )
                .await;
            }
        }
    }

    Ok(())
}

async fn ensure_virtual_sink_running(
    config: &Config,
    active_loopback: &mut Option<PwLoopbackProcess>,
    active_virtual_sink: &mut Option<PipeWireSink>,
    routed_stream: &mut Option<(u32, u32)>,
) {
    if !config.stream.virtual_sink.enabled {
        return;
    }

    let needs_spawn = match active_loopback.as_mut() {
        Some(lb) => !lb.is_alive(),
        None => true,
    };

    if !needs_spawn {
        if active_virtual_sink.is_none()
            && let Ok(Some(sink)) = find_sink_by_name(&config.stream.virtual_sink.sink_name).await
        {
            *active_virtual_sink = Some(sink);
        }
        return;
    }

    if let Some(old_lb) = active_loopback.take() {
        warn!(
            sink_name = %old_lb.sink_name,
            "pw-loopback virtual sink exited unexpectedly; respawning"
        );
        old_lb.stop().await;
    }
    *active_virtual_sink = None;
    *routed_stream = None;

    match PwLoopbackProcess::spawn(
        &config.stream.virtual_sink,
        config.stream.sample_rate,
        config.stream.channels,
    )
    .await
    {
        Ok(lb) => {
            let sink = wait_for_virtual_sink(
                &config.stream.virtual_sink.sink_name,
                Duration::from_secs(2),
            )
            .await;
            match sink {
                Some(ref s) => {
                    info!(
                        sink_id = s.id,
                        sink_serial = s.serial,
                        sink_name = %s.node_name,
                        playback_mixer = %config.stream.virtual_sink.playback_name,
                        "Started virtual capture sink + controllable playback loopback"
                    );
                }
                None => {
                    warn!(
                        sink_name = %config.stream.virtual_sink.sink_name,
                        "Spawned pw-loopback, but virtual sink not yet visible in pw-dump"
                    );
                }
            }
            *active_virtual_sink = sink;
            *active_loopback = Some(lb);
        }
        Err(err) => {
            error!("Failed to spawn pw-loopback virtual sink: {}", err);
        }
    }
}

async fn maybe_route_target_stream(
    config: &Config,
    node: &PipeWireNode,
    active_virtual_sink: &Option<PipeWireSink>,
    routed_stream: &mut Option<(u32, u32)>,
) {
    if *routed_stream == Some((node.id, node.serial)) {
        return;
    }

    let target = active_virtual_sink
        .as_ref()
        .map(|s| s.serial.to_string())
        .unwrap_or_else(|| config.stream.virtual_sink.sink_name.clone());

    match route_stream_to_sink(node.id, &target).await {
        Ok(()) => {
            info!(
                node_id = node.id,
                serial = node.serial,
                app = %node.app_name,
                sink = %config.stream.virtual_sink.sink_name,
                playback_mixer = %config.stream.virtual_sink.playback_name,
                "Routed target stream to virtual capture sink (adjust listening volume via playback loopback)"
            );
            *routed_stream = Some((node.id, node.serial));
        }
        Err(err) => {
            warn!(
                node_id = node.id,
                sink = %config.stream.virtual_sink.sink_name,
                error = %err,
                "Failed routing target stream to virtual capture sink"
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_snapshot(
    conn: &Connection,
    config: &Config,
    recording_enabled: bool,
    index: &Arc<Mutex<RecordedIndex>>,
    index_path: &Path,
    art_cache: &Arc<Mutex<CoverArtCache>>,
    emitter: Option<&IpcEmitter>,
    snap: PlayerSnapshot,
    active_session: &mut Option<RecordingSession>,
    paused_since: &mut Option<Instant>,
) {
    match snap.status {
        PlaybackStatus::Playing => {
            let was_paused = paused_since.take().is_some();
            let Some(meta) = snap.metadata else {
                return;
            };

            if let Some(current) = active_session.as_mut() {
                if same_track(&current.meta, &meta) {
                    // Enrich metadata if e.g. artUrl arrived asynchronously
                    if current.meta.art_url.is_none() && meta.art_url.is_some() {
                        current.meta.art_url = meta.art_url;
                    }
                    if current.meta.album.is_empty() && !meta.album.is_empty() {
                        current.meta.album = meta.album;
                    }
                    if was_paused && recording_enabled && !current.skip_buffering {
                        if (snap.position_us - current.last_known_position_us).abs() > 2_500_000 {
                            current.invalidate(InvalidationReason::SeekDetected {
                                jump_ms: (snap.position_us - current.last_known_position_us) / 1000,
                            });
                        } else {
                            current.last_known_position_us = snap.position_us;
                            current.last_position_at = Instant::now();
                        }
                    }
                    return;
                }

                // Track changed! Finalize previous session before starting the new one.
                if let Some(prev) = active_session.take() {
                    maybe_finalize_session(
                        prev,
                        config,
                        index,
                        index_path,
                        art_cache,
                        emitter.cloned(),
                    )
                    .await;
                }
            }

            if recording_enabled && config.stream.enforce_unity_volume {
                if let Some(vol) = snap.volume
                    && (vol - 1.0).abs() > 0.015
                {
                    if let Err(err) = set_mpris_volume(conn, &snap.bus_name, 1.0).await {
                        warn!("Failed resetting MPRIS player volume to 1.0: {}", err);
                    } else {
                        info!("Reset MPRIS player volume to 100% for bit-exact recording");
                    }
                }
                if let Ok(Some(node)) = find_target_node(&config.stream.pipewire_target).await
                    && !node.is_unity_volume()
                {
                    if let Err(err) = enforce_node_unity_volume(&node).await {
                        warn!("Failed resetting PipeWire node volume to 100%: {}", err);
                    } else {
                        info!(node_id = node.id, "Reset PipeWire node volume to 100%");
                    }
                }
            }

            let offset_ms = (snap.position_us.max(0) as u64) / 1000;
            let skip_reason = if !recording_enabled {
                None
            } else if meta.is_advertisement() {
                Some(InvalidationReason::Advertisement)
            } else if let Some(existing) = index.lock().await.find_duplicate(config, &meta) {
                Some(InvalidationReason::Duplicate(existing))
            } else if offset_ms > config.completeness.max_start_offset_ms {
                Some(InvalidationReason::IncompleteStart {
                    offset_ms,
                    max_allowed_ms: config.completeness.max_start_offset_ms,
                })
            } else {
                None
            };

            if recording_enabled {
                if let Some(ref reason) = skip_reason {
                    info!(
                        title = %meta.title,
                        artist = %meta.primary_artist(),
                        reason = %reason,
                        "Skipping track"
                    );
                } else {
                    info!(
                        title = %meta.title,
                        artist = %meta.primary_artist(),
                        album = %meta.album,
                        duration_s = meta.duration_ms() / 1000,
                        "Recording started"
                    );
                }
            }

            let mut session = RecordingSession::new(
                meta,
                config.stream.sample_rate,
                config.stream.channels,
                offset_ms,
                snap.position_us,
                skip_reason,
            );
            if !recording_enabled {
                session.skip_buffering = true;
            }
            *active_session = Some(session);
        }

        PlaybackStatus::Paused | PlaybackStatus::Stopped => {
            if paused_since.is_none() {
                *paused_since = Some(Instant::now());
            }

            if recording_enabled
                && let Some(session) = active_session.as_mut()
                && !session.skip_buffering
            {
                if validate_completeness(session, config).is_ok() {
                    // Track reached its full length (e.g. end of album/queue)
                    if let Some(finished) = active_session.take() {
                        maybe_finalize_session(
                            finished,
                            config,
                            index,
                            index_path,
                            art_cache,
                            emitter.cloned(),
                        )
                        .await;
                    }
                } else if snap.status == PlaybackStatus::Stopped || !config.completeness.allow_pause
                {
                    let reason = if snap.status == PlaybackStatus::Stopped {
                        InvalidationReason::TooShort {
                            captured_ms: session.captured_ms(),
                            expected_ms: session.meta.duration_ms(),
                        }
                    } else {
                        InvalidationReason::PausedMidTrack
                    };
                    info!(
                        title = %session.meta.title,
                        reason = %reason,
                        "Discarding incomplete recording"
                    );
                    session.invalidate(reason);
                }
            }
        }

        PlaybackStatus::Unknown(_) => {}
    }
}

async fn maybe_finalize_session(
    session: RecordingSession,
    config: &Config,
    index: &Arc<Mutex<RecordedIndex>>,
    index_path: &Path,
    art_cache: &Arc<Mutex<CoverArtCache>>,
    emitter: Option<IpcEmitter>,
) {
    if session.skip_buffering && session.invalidated.is_none() {
        return;
    }

    if let Err(reason) = validate_completeness(&session, config) {
        if !matches!(
            reason,
            InvalidationReason::Duplicate(_) | InvalidationReason::Advertisement
        ) {
            info!(
                title = %session.meta.title,
                artist = %session.meta.primary_artist(),
                reason = %reason,
                "Track not saved"
            );
        }
        return;
    }

    // Re-check duplicates right before encoding
    if let Some(existing) = index.lock().await.find_duplicate(config, &session.meta) {
        info!(
            title = %session.meta.title,
            existing = %existing.display(),
            "Skipping duplicate before save"
        );
        return;
    }

    let output_path = render_track_path(config, &session.meta);
    let captured_ms = session.captured_ms();
    let cfg = config.clone();
    let idx = Arc::clone(index);
    let idx_path = index_path.to_path_buf();
    let cache = Arc::clone(art_cache);

    tokio::spawn(async move {
        let cover_bytes = if let Some(ref art_url) = session.meta.art_url {
            let mut guard = cache.lock().await;
            match fetch_cover_art(art_url, &mut guard).await {
                Ok(bytes) => bytes,
                Err(err) => {
                    warn!(url = %art_url, error = %err, "Failed fetching cover art");
                    None
                }
            }
        } else {
            None
        };

        let samples_slice: &[i16] = if cfg.audio.trim_silence {
            trim_silence_frames(
                &session.pcm_samples,
                session.channels,
                session.sample_rate,
                cfg.audio.silence_threshold_db,
                cfg.audio.max_trim_ms,
            )
        } else {
            &session.pcm_samples
        };

        match encode_and_tag_flac(
            samples_slice,
            &session.meta,
            &cfg,
            &output_path,
            cover_bytes.as_deref(),
        )
        .await
        {
            Ok(()) => {
                let (saved_entry, index_count) = {
                    let mut guard = idx.lock().await;
                    guard.record_entry(&session.meta, output_path.clone(), captured_ms);
                    if let Err(err) = guard.save(&idx_path) {
                        error!("Failed saving recorded index: {}", err);
                    }
                    let entry = guard.entries.last().map(IpcRecordedEntry::from);
                    (entry, guard.entries.len())
                };
                info!(
                    title = %session.meta.title,
                    artist = %session.meta.primary_artist(),
                    path = %output_path.display(),
                    duration_s = captured_ms / 1000,
                    "Saved complete track"
                );
                if let Some(ref em) = emitter {
                    if let Some(entry) = saved_entry {
                        em.emit(IpcEvent::TrackSaved { entry, index_count });
                    }
                    emit_recent_tracks(em, &idx, 15).await;
                }
            }
            Err(err) => {
                error!(
                    title = %session.meta.title,
                    error = %err,
                    "Failed encoding/tagging FLAC"
                );
                if let Some(ref em) = emitter {
                    em.emit_error(format!("Failed saving \"{}\": {err}", session.meta.title));
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_meta(length_ms: u64) -> TrackMetadata {
        TrackMetadata {
            track_id: "/com/spotify/track/xyz".to_string(),
            title: "Complete Song".to_string(),
            artists: vec!["Artist".to_string()],
            album: "Album".to_string(),
            album_artists: vec!["Artist".to_string()],
            track_number: Some(1),
            disc_number: Some(1),
            content_created: None,
            art_url: None,
            length_us: length_ms * 1000,
        }
    }

    fn make_session_with_duration(
        meta: TrackMetadata,
        captured_ms: u64,
        start_offset_ms: u64,
    ) -> RecordingSession {
        let sample_rate = 44100u32;
        let channels = 2u16;
        let mut session = RecordingSession::new(
            meta,
            sample_rate,
            channels,
            start_offset_ms,
            (start_offset_ms as i64) * 1000,
            None,
        );
        let frames = ((sample_rate as u64) * captured_ms) / 1000;
        session.pcm_samples = vec![1000; (frames as usize) * (channels as usize)];
        session
    }

    #[test]
    fn accepts_complete_song_from_start() {
        let cfg = Config::default();
        let session = make_session_with_duration(test_meta(180_000), 178_500, 200);
        assert!(validate_completeness(&session, &cfg).is_ok());
    }

    #[test]
    fn rejects_mid_track_start() {
        let cfg = Config::default();
        let session = make_session_with_duration(test_meta(180_000), 150_000, 12_000);
        assert!(matches!(
            validate_completeness(&session, &cfg),
            Err(InvalidationReason::IncompleteStart { .. })
        ));
    }

    #[test]
    fn rejects_early_skip() {
        let cfg = Config::default();
        let session = make_session_with_duration(test_meta(180_000), 90_000, 0);
        assert!(matches!(
            validate_completeness(&session, &cfg),
            Err(InvalidationReason::TooShort { .. })
        ));
    }

    #[test]
    fn rejects_seek_and_pause_and_volume_attenuation() {
        let cfg = Config::default();

        let mut seeked = make_session_with_duration(test_meta(180_000), 180_000, 0);
        seeked.invalidate(InvalidationReason::SeekDetected { jump_ms: 10_000 });
        assert!(matches!(
            validate_completeness(&seeked, &cfg),
            Err(InvalidationReason::SeekDetected { .. })
        ));

        let mut paused = make_session_with_duration(test_meta(180_000), 180_000, 0);
        paused.invalidate(InvalidationReason::PausedMidTrack);
        assert_eq!(
            validate_completeness(&paused, &cfg),
            Err(InvalidationReason::PausedMidTrack)
        );

        let mut attenuated = make_session_with_duration(test_meta(180_000), 180_000, 0);
        attenuated.invalidate(InvalidationReason::VolumeAttenuated);
        assert_eq!(
            validate_completeness(&attenuated, &cfg),
            Err(InvalidationReason::VolumeAttenuated)
        );
    }

    #[test]
    fn rejects_spotify_ads() {
        let cfg = Config::default();
        let mut ad = test_meta(30_000);
        ad.track_id = "/com/spotify/ad/12345".to_string();
        assert!(ad.is_advertisement());
        let session = make_session_with_duration(ad, 30_000, 0);
        assert_eq!(
            validate_completeness(&session, &cfg),
            Err(InvalidationReason::Advertisement)
        );
    }

    #[test]
    fn trims_leading_and_trailing_digital_silence_only_when_requested() {
        let mut samples = vec![0i16; 200]; // 100 silent stereo frames
        samples.extend_from_slice(&[5000, -5000, 3000, -3000]); // 2 audible frames
        samples.extend(vec![0i16; 200]); // 100 silent stereo frames

        let trimmed = trim_silence_frames(&samples, 2, 44100, -60.0, 1500);
        assert_eq!(trimmed, &[5000, -5000, 3000, -3000]);
    }
}
