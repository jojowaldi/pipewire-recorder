use std::collections::HashMap;

use anyhow::{Context, Result};
use zbus::names::{BusName, InterfaceName};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
    Unknown(String),
}

impl PlaybackStatus {
    pub fn from_mpris_str(s: &str) -> Self {
        match s.trim() {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            "Stopped" => Self::Stopped,
            other => Self::Unknown(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMetadata {
    pub track_id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    pub album_artists: Vec<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub content_created: Option<String>,
    pub art_url: Option<String>,
    pub length_us: u64,
}

impl TrackMetadata {
    pub fn is_advertisement(&self) -> bool {
        let id_lc = self.track_id.to_ascii_lowercase();
        id_lc.contains("/ad/") || id_lc.contains(":ad:")
    }

    pub fn duration_ms(&self) -> u64 {
        self.length_us / 1000
    }

    pub fn primary_artist(&self) -> &str {
        self.artists
            .first()
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Unknown Artist")
    }

    pub fn primary_album_artist(&self) -> &str {
        self.album_artists
            .first()
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.primary_artist())
    }

    pub fn year(&self) -> Option<&str> {
        let created = self.content_created.as_deref()?.trim();
        let prefix = created.get(0..4)?;
        if prefix.bytes().all(|b| b.is_ascii_digit()) {
            Some(prefix)
        } else {
            None
        }
    }

    pub fn canonical_key(&self) -> String {
        format!(
            "{}|{}|{}",
            self.primary_artist().trim().to_ascii_lowercase(),
            self.album.trim().to_ascii_lowercase(),
            self.title.trim().to_ascii_lowercase()
        )
    }

    pub fn track_identity_key(&self) -> String {
        let id = self.track_id.trim();
        if id.is_empty() || id.ends_with("/NoTrack") {
            format!("canonical:{}|{}", self.canonical_key(), self.length_us)
        } else {
            id.to_string()
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlayerSnapshot {
    pub bus_name: String,
    pub status: PlaybackStatus,
    pub metadata: Option<TrackMetadata>,
    pub position_us: i64,
    pub volume: Option<f64>,
}

pub fn matches_mpris_bus_name(bus_name: &str, target_player: &str) -> bool {
    if !bus_name.starts_with(MPRIS_PREFIX) {
        return false;
    }
    let target = target_player.trim().to_ascii_lowercase();
    if target.is_empty() {
        return false;
    }

    let bus_lc = bus_name.to_ascii_lowercase();
    if target.starts_with("org.mpris.mediaplayer2.") {
        return bus_lc == target || bus_lc.starts_with(&format!("{target}."));
    }

    let suffix = &bus_lc[MPRIS_PREFIX.len()..];
    suffix == target || suffix.starts_with(&format!("{target}."))
}

fn unwrap_value<'a>(mut val: &'a Value<'a>) -> &'a Value<'a> {
    while let Value::Value(inner) = val {
        val = inner.as_ref();
    }
    val
}

fn value_as_string(val: &Value<'_>) -> Option<String> {
    match unwrap_value(val) {
        Value::Str(s) => Some(s.as_str().to_string()),
        Value::ObjectPath(p) => Some(p.as_str().to_string()),
        _ => None,
    }
}

fn value_as_string_list(val: &Value<'_>) -> Vec<String> {
    match unwrap_value(val) {
        Value::Array(arr) => arr
            .iter()
            .filter_map(value_as_string)
            .filter(|s| !s.trim().is_empty())
            .collect(),
        other => value_as_string(other)
            .filter(|s| !s.trim().is_empty())
            .into_iter()
            .collect(),
    }
}

fn value_as_u64(val: &Value<'_>) -> Option<u64> {
    match unwrap_value(val) {
        Value::U64(n) => Some(*n),
        Value::I64(n) if *n >= 0 => Some(*n as u64),
        Value::U32(n) => Some(*n as u64),
        Value::I32(n) if *n >= 0 => Some(*n as u64),
        _ => None,
    }
}

fn value_as_i64(val: &Value<'_>) -> Option<i64> {
    match unwrap_value(val) {
        Value::I64(n) => Some(*n),
        Value::U64(n) => i64::try_from(*n).ok(),
        Value::I32(n) => Some(*n as i64),
        Value::U32(n) => Some(*n as i64),
        _ => None,
    }
}

fn value_as_u32(val: &Value<'_>) -> Option<u32> {
    value_as_u64(val).and_then(|n| u32::try_from(n).ok())
}

fn value_as_f64(val: &Value<'_>) -> Option<f64> {
    match unwrap_value(val) {
        Value::F64(f) if f.is_finite() => Some(*f),
        Value::I64(n) => Some(*n as f64),
        Value::U64(n) => Some(*n as f64),
        _ => None,
    }
}

pub fn parse_metadata_map(map: &HashMap<String, OwnedValue>) -> Option<TrackMetadata> {
    let track_id = map
        .get("mpris:trackid")
        .and_then(|v| value_as_string(v))
        .unwrap_or_default();
    let title = map
        .get("xesam:title")
        .and_then(|v| value_as_string(v))
        .unwrap_or_default();
    let artists = map
        .get("xesam:artist")
        .map(|v| value_as_string_list(v))
        .unwrap_or_default();
    let album = map
        .get("xesam:album")
        .and_then(|v| value_as_string(v))
        .unwrap_or_default();
    let album_artists = map
        .get("xesam:albumArtist")
        .map(|v| value_as_string_list(v))
        .unwrap_or_default();
    let track_number = map.get("xesam:trackNumber").and_then(|v| value_as_u32(v));
    let disc_number = map.get("xesam:discNumber").and_then(|v| value_as_u32(v));
    let content_created = map
        .get("xesam:contentCreated")
        .and_then(|v| value_as_string(v))
        .filter(|s| !s.trim().is_empty());
    let art_url = map
        .get("mpris:artUrl")
        .and_then(|v| value_as_string(v))
        .filter(|s| !s.trim().is_empty());
    let length_us = map
        .get("mpris:length")
        .and_then(|v| value_as_u64(v))
        .unwrap_or(0);

    if title.trim().is_empty() && track_id.trim().is_empty() {
        return None;
    }

    Some(TrackMetadata {
        track_id,
        title,
        artists,
        album,
        album_artists,
        track_number,
        disc_number,
        content_created,
        art_url,
        length_us,
    })
}

pub async fn list_mpris_players(conn: &Connection) -> Result<Vec<String>> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let names = dbus.list_names().await?;
    let mut players: Vec<String> = names
        .into_iter()
        .map(|n| n.to_string())
        .filter(|n| n.starts_with(MPRIS_PREFIX))
        .collect();
    players.sort();
    players.dedup();
    Ok(players)
}

pub async fn find_active_player_bus(
    conn: &Connection,
    target_player: &str,
) -> Result<Option<String>> {
    let players = list_mpris_players(conn).await?;
    let exact_target = if target_player.trim().starts_with(MPRIS_PREFIX) {
        target_player.trim().to_ascii_lowercase()
    } else {
        format!("{MPRIS_PREFIX}{}", target_player.trim()).to_ascii_lowercase()
    };

    if let Some(exact) = players
        .iter()
        .find(|name| name.to_ascii_lowercase() == exact_target)
    {
        return Ok(Some(exact.clone()));
    }

    Ok(players
        .into_iter()
        .find(|name| matches_mpris_bus_name(name, target_player)))
}

pub async fn resolve_name_owner(conn: &Connection, bus_name: &str) -> Result<String> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let name = BusName::try_from(bus_name.to_string())
        .with_context(|| format!("invalid D-Bus name: {bus_name}"))?;
    let owner = dbus.get_name_owner(name).await?;
    Ok(owner.to_string())
}

pub async fn query_player_snapshot(conn: &Connection, bus_name: &str) -> Result<PlayerSnapshot> {
    let dest = BusName::try_from(bus_name.to_string())
        .with_context(|| format!("invalid bus name {bus_name}"))?;
    let props = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(dest)?
        .path(MPRIS_PATH)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;

    let iface = InterfaceName::try_from(MPRIS_PLAYER_IFACE)?;
    let all = props.get_all(iface.clone()).await?;

    let status = all
        .get("PlaybackStatus")
        .and_then(|v| value_as_string(v))
        .map(|s| PlaybackStatus::from_mpris_str(&s))
        .unwrap_or(PlaybackStatus::Stopped);

    let metadata = all
        .get("Metadata")
        .and_then(|v| HashMap::<String, OwnedValue>::try_from(v.clone()).ok())
        .and_then(|map| parse_metadata_map(&map));

    let position_us = match props.get(iface.clone(), "Position").await {
        Ok(val) => value_as_i64(&val).unwrap_or(0),
        Err(_) => all
            .get("Position")
            .and_then(|v| value_as_i64(v))
            .unwrap_or(0),
    };

    let volume = all.get("Volume").and_then(|v| value_as_f64(v));

    Ok(PlayerSnapshot {
        bus_name: bus_name.to_string(),
        status,
        metadata,
        position_us,
        volume,
    })
}

pub async fn set_mpris_volume(conn: &Connection, bus_name: &str, volume: f64) -> Result<()> {
    let dest = BusName::try_from(bus_name.to_string())?;
    let props = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(dest)?
        .path(MPRIS_PATH)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;

    let iface = InterfaceName::try_from(MPRIS_PLAYER_IFACE)?;
    let clamped = volume.clamp(0.0, 1.0);
    props.set(iface, "Volume", Value::F64(clamped)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact_and_instance_mpris_bus_names() {
        assert!(matches_mpris_bus_name(
            "org.mpris.MediaPlayer2.spotify",
            "spotify"
        ));
        assert!(matches_mpris_bus_name(
            "org.mpris.MediaPlayer2.firefox.instance_1_99",
            "firefox"
        ));
        assert!(matches_mpris_bus_name(
            "org.mpris.MediaPlayer2.vlc",
            "org.mpris.MediaPlayer2.vlc"
        ));
        assert!(!matches_mpris_bus_name(
            "org.mpris.MediaPlayer2.chromium",
            "firefox"
        ));
        assert!(!matches_mpris_bus_name("org.freedesktop.DBus", "spotify"));
    }

    #[test]
    fn parses_mpris_metadata_dict_and_detects_ads() {
        let mut map: HashMap<String, OwnedValue> = HashMap::new();
        map.insert(
            "mpris:trackid".to_string(),
            Value::from("/com/spotify/track/5uQ7de4OWjcuGRfMm8e6wX")
                .try_to_owned()
                .unwrap(),
        );
        map.insert(
            "xesam:title".to_string(),
            Value::from("Digital Love").try_to_owned().unwrap(),
        );
        map.insert(
            "xesam:artist".to_string(),
            Value::from(vec!["Daft Punk"]).try_to_owned().unwrap(),
        );
        map.insert(
            "xesam:album".to_string(),
            Value::from("Discovery").try_to_owned().unwrap(),
        );
        map.insert(
            "xesam:trackNumber".to_string(),
            Value::I32(3).try_to_owned().unwrap(),
        );
        map.insert(
            "mpris:length".to_string(),
            Value::I64(298_000_000).try_to_owned().unwrap(),
        );

        let meta = parse_metadata_map(&map).unwrap();
        assert_eq!(meta.title, "Digital Love");
        assert_eq!(meta.primary_artist(), "Daft Punk");
        assert_eq!(meta.album, "Discovery");
        assert_eq!(meta.track_number, Some(3));
        assert_eq!(meta.duration_ms(), 298_000);
        assert!(!meta.is_advertisement());

        map.insert(
            "mpris:trackid".to_string(),
            Value::from("/com/spotify/ad/999").try_to_owned().unwrap(),
        );
        let ad_meta = parse_metadata_map(&map).unwrap();
        assert!(ad_meta.is_advertisement());
    }
}
