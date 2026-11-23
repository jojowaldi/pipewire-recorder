use std::process::Stdio;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, warn};

#[derive(Debug, Clone, PartialEq)]
pub struct PipeWireNode {
    pub id: u32,
    pub serial: u32,
    pub node_name: String,
    pub app_name: String,
    pub binary_name: String,
    pub media_name: String,
    pub media_class: String,
    pub channel_volumes: Vec<f64>,
    pub mute: bool,
}

impl PipeWireNode {
    pub fn is_unity_volume(&self) -> bool {
        !self.mute
            && (self.channel_volumes.is_empty()
                || self
                    .channel_volumes
                    .iter()
                    .all(|&v| (v - 1.0).abs() <= 0.015))
    }

    pub fn display_volume(&self) -> String {
        if self.mute {
            return "MUTED".to_string();
        }
        if self.channel_volumes.is_empty() {
            return "unknown (assuming 100%)".to_string();
        }
        let avg = self.channel_volumes.iter().sum::<f64>() / self.channel_volumes.len() as f64;
        format!("{:.0}%", avg * 100.0)
    }
}

#[derive(Debug, Deserialize)]
struct RawPwObject {
    id: u32,
    #[serde(rename = "type")]
    obj_type: Option<String>,
    info: Option<RawPwInfo>,
}

#[derive(Debug, Deserialize)]
struct RawPwInfo {
    props: Option<serde_json::Value>,
    params: Option<RawPwParams>,
}

#[derive(Debug, Deserialize)]
struct RawPwParams {
    #[serde(rename = "Props")]
    props: Option<Vec<serde_json::Value>>,
}

fn json_str(map: &serde_json::Value, key: &str) -> String {
    map.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn json_u32(map: &serde_json::Value, key: &str) -> Option<u32> {
    let val = map.get(key)?;
    if let Some(n) = val.as_u64() {
        u32::try_from(n).ok()
    } else if let Some(s) = val.as_str() {
        s.parse::<u32>().ok()
    } else {
        None
    }
}

pub fn parse_pw_dump_output(json_str_raw: &str) -> Result<Vec<PipeWireNode>> {
    let objects: Vec<RawPwObject> =
        serde_json::from_str(json_str_raw).context("failed to parse pw-dump JSON output")?;

    let mut nodes = Vec::new();
    for obj in objects {
        if let Some(ref ty) = obj.obj_type
            && ty != "PipeWire:Interface:Node"
        {
            continue;
        }
        let Some(info) = obj.info else { continue };
        let Some(props) = info.props else { continue };

        let media_class = json_str(&props, "media.class");
        if media_class != "Stream/Output/Audio" {
            continue;
        }

        let serial = json_u32(&props, "object.serial").unwrap_or(obj.id);
        let node_name = json_str(&props, "node.name");
        let app_name = json_str(&props, "application.name");
        let binary_name = json_str(&props, "application.process.binary");
        let media_name = json_str(&props, "media.name");

        let mut channel_volumes = Vec::new();
        let mut mute = false;

        if let Some(params) = info.params.and_then(|p| p.props) {
            for entry in params {
                if let Some(m) = entry.get("mute").and_then(|v| v.as_bool()) {
                    mute = m;
                }
                if let Some(vols) = entry.get("channelVolumes").and_then(|v| v.as_array()) {
                    let parsed: Vec<f64> = vols.iter().filter_map(|v| v.as_f64()).collect();
                    if !parsed.is_empty() {
                        channel_volumes = parsed;
                    }
                }
            }
        }

        nodes.push(PipeWireNode {
            id: obj.id,
            serial,
            node_name,
            app_name,
            binary_name,
            media_name,
            media_class,
            channel_volumes,
            mute,
        });
    }

    Ok(nodes)
}

pub fn find_matching_node(nodes: &[PipeWireNode], target: &str) -> Option<PipeWireNode> {
    let needle = target.trim();
    if needle.is_empty() {
        return None;
    }

    if let Ok(num) = needle.parse::<u32>()
        && let Some(node) = nodes.iter().find(|n| n.serial == num || n.id == num)
    {
        return Some(node.clone());
    }

    let needle_lc = needle.to_ascii_lowercase();

    // 1. Exact case-insensitive match on binary_name, app_name, or node_name
    if let Some(node) = nodes.iter().find(|n| {
        n.binary_name.to_ascii_lowercase() == needle_lc
            || n.app_name.to_ascii_lowercase() == needle_lc
            || n.node_name.to_ascii_lowercase() == needle_lc
    }) {
        return Some(node.clone());
    }

    // 2. Substring case-insensitive match across identifying fields
    nodes
        .iter()
        .find(|n| {
            n.app_name.to_ascii_lowercase().contains(&needle_lc)
                || n.binary_name.to_ascii_lowercase().contains(&needle_lc)
                || n.node_name.to_ascii_lowercase().contains(&needle_lc)
                || n.media_name.to_ascii_lowercase().contains(&needle_lc)
        })
        .cloned()
}

pub async fn list_audio_output_streams() -> Result<Vec<PipeWireNode>> {
    let output = Command::new("pw-dump")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to execute pw-dump")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("pw-dump failed ({}): {}", output.status, stderr.trim());
    }

    let stdout = String::from_utf8(output.stdout).context("pw-dump returned non-UTF-8 output")?;
    parse_pw_dump_output(&stdout)
}

pub async fn find_target_node(target: &str) -> Result<Option<PipeWireNode>> {
    let nodes = list_audio_output_streams().await?;
    Ok(find_matching_node(&nodes, target))
}

pub async fn enforce_node_unity_volume(node: &PipeWireNode) -> Result<()> {
    let id_str = node.id.to_string();

    let mute_res = Command::new("wpctl")
        .args(["set-mute", &id_str, "0"])
        .output()
        .await;
    let vol_res = Command::new("wpctl")
        .args(["set-volume", &id_str, "1.0"])
        .output()
        .await;

    if let (Ok(m), Ok(v)) = (mute_res, vol_res)
        && m.status.success()
        && v.status.success()
    {
        debug!(node_id = node.id, "Set PipeWire stream volume to 100% via wpctl");
        return Ok(());
    }

    let channels_len = node.channel_volumes.len().max(2);
    let vols = vec!["1.0"; channels_len].join(", ");
    let props_arg = format!("{{ mute: false, channelVolumes: [ {vols} ] }}");

    let pw_cli_out = Command::new("pw-cli")
        .args(["set-param", &id_str, "Props", &props_arg])
        .output()
        .await
        .context("failed to invoke wpctl or pw-cli to enforce unity volume")?;

    if !pw_cli_out.status.success() {
        let stderr = String::from_utf8_lossy(&pw_cli_out.stderr);
        anyhow::bail!("failed to enforce unity volume on PipeWire node {}: {}", node.id, stderr.trim());
    }

    Ok(())
}

#[derive(Debug)]
pub enum PwStreamEvent {
    Samples(Vec<i16>),
    Ended(Option<String>),
}

pub struct PwRecorderProcess {
    pub target_serial: u32,
    pub node_id: u32,
    child: Child,
    reader_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
}

impl PwRecorderProcess {
    pub async fn spawn(
        node: &PipeWireNode,
        sample_rate: u32,
        channels: u16,
    ) -> Result<(Self, mpsc::Receiver<PwStreamEvent>)> {
        let target_arg = node.serial.to_string();
        let rate_arg = sample_rate.to_string();
        let channels_arg = channels.to_string();

        let mut child = Command::new("pw-record")
            .args([
                "--raw",
                "--target",
                &target_arg,
                "--format",
                "s16",
                "--rate",
                &rate_arg,
                "--channels",
                &channels_arg,
                "--volume",
                "1.0",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to spawn pw-record")?;

        let mut stdout = child
            .stdout
            .take()
            .context("pw-record missing stdout pipe")?;
        let stderr = child
            .stderr
            .take()
            .context("pw-record missing stderr pipe")?;

        let (tx, rx) = mpsc::channel::<PwStreamEvent>(64);

        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    debug!(target: "pw-record", "{}", line);
                }
            }
        });

        // Read in chunks aligned to frame boundaries (~100ms chunks for 44100Hz stereo: 17640 bytes)
        let bytes_per_frame = (channels as usize) * 2;
        let chunk_bytes = ((sample_rate as usize / 10) * bytes_per_frame).max(4096);
        let chunk_bytes = chunk_bytes - (chunk_bytes % bytes_per_frame);

        let reader_task = tokio::spawn(async move {
            let mut buf = vec![0u8; chunk_bytes];
            let mut leftover: Option<u8> = None;
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => {
                        let _ = tx.send(PwStreamEvent::Ended(None)).await;
                        break;
                    }
                    Ok(n) => {
                        let mut samples = Vec::with_capacity(n.div_ceil(2));
                        let mut start = 0usize;

                        if let Some(prev) = leftover.take() {
                            samples.push(i16::from_le_bytes([prev, buf[0]]));
                            start = 1;
                        }

                        let remaining = &buf[start..n];
                        let (pairs, rem) = remaining.as_chunks::<2>();
                        for pair in pairs {
                            samples.push(i16::from_le_bytes(*pair));
                        }
                        if let Some(&b) = rem.first() {
                            leftover = Some(b);
                        }

                        if !samples.is_empty() && tx.send(PwStreamEvent::Samples(samples)).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        warn!("pw-record stdout read error: {}", err);
                        let _ = tx.send(PwStreamEvent::Ended(Some(err.to_string()))).await;
                        break;
                    }
                }
            }
        });

        Ok((
            Self {
                target_serial: node.serial,
                node_id: node.id,
                child,
                reader_task,
                stderr_task,
            },
            rx,
        ))
    }

    pub async fn stop(mut self) {
        debug!(
            node_id = self.node_id,
            serial = self.target_serial,
            "Stopping pw-record process"
        );
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        self.reader_task.abort();
        self.stderr_task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PW_DUMP: &str = r#"[
      {
        "id": 42,
        "type": "PipeWire:Interface:Node",
        "info": {
          "props": {
            "media.class": "Audio/Sink",
            "node.name": "easyeffects_sink"
          }
        }
      },
      {
        "id": 77,
        "type": "PipeWire:Interface:Node",
        "info": {
          "props": {
            "media.class": "Stream/Output/Audio",
            "object.serial": 188,
            "application.name": "Spotify",
            "application.process.binary": "spotify",
            "node.name": "spotify",
            "media.name": "Spotify"
          },
          "params": {
            "Props": [
              {
                "mute": false,
                "channelVolumes": [1.0, 1.0]
              }
            ]
          }
        }
      },
      {
        "id": 99,
        "type": "PipeWire:Interface:Node",
        "info": {
          "props": {
            "media.class": "Stream/Output/Audio",
            "object.serial": 205,
            "application.name": "Firefox",
            "application.process.binary": "firefox",
            "node.name": "Firefox",
            "media.name": "AudioStream"
          },
          "params": {
            "Props": [
              {
                "mute": false,
                "channelVolumes": [0.65, 0.65]
              }
            ]
          }
        }
      }
    ]"#;

    #[test]
    fn parses_stream_output_audio_nodes_and_ignores_sinks() {
        let nodes = parse_pw_dump_output(SAMPLE_PW_DUMP).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].id, 77);
        assert_eq!(nodes[0].serial, 188);
        assert!(nodes[0].is_unity_volume());
        assert_eq!(nodes[1].id, 99);
        assert!(!nodes[1].is_unity_volume());
    }

    #[test]
    fn matches_by_name_binary_or_serial() {
        let nodes = parse_pw_dump_output(SAMPLE_PW_DUMP).unwrap();
        assert_eq!(find_matching_node(&nodes, "spotify").unwrap().serial, 188);
        assert_eq!(find_matching_node(&nodes, "FIREFOX").unwrap().serial, 205);
        assert_eq!(find_matching_node(&nodes, "205").unwrap().id, 99);
        assert!(find_matching_node(&nodes, "vlc").is_none());
    }
}
