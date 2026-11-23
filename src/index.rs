use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::mpris::TrackMetadata;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordedEntry {
    pub track_id: String,
    pub canonical_key: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub file_path: PathBuf,
    pub recorded_at_unix: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordedIndex {
    pub entries: Vec<RecordedEntry>,
}

impl RecordedIndex {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read index {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str::<Self>(&raw)
            .with_context(|| format!("failed to parse index JSON {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create index dir {}", parent.display()))?;
        }
        let tmp_path = path.with_extension("json.tmp");
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp_path, content)
            .with_context(|| format!("failed to write temp index {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("failed to commit index {}", path.display()))?;
        Ok(())
    }

    pub fn find_duplicate(&self, config: &Config, meta: &TrackMetadata) -> Option<PathBuf> {
        let track_id = meta.track_identity_key();
        let canonical = meta.canonical_key();

        for entry in &self.entries {
            let id_match = !track_id.starts_with("canonical:") && entry.track_id == track_id;
            let key_match = entry.canonical_key == canonical;
            if (id_match || key_match) && file_exists_non_empty(&entry.file_path) {
                return Some(entry.file_path.clone());
            }
        }

        let target_path = render_track_path(config, meta);
        if file_exists_non_empty(&target_path) {
            return Some(target_path);
        }

        None
    }

    pub fn record_entry(&mut self, meta: &TrackMetadata, file_path: PathBuf, duration_ms: u64) {
        let track_id = meta.track_identity_key();
        let canonical = meta.canonical_key();

        self.entries.retain(|e| {
            e.file_path != file_path
                && e.canonical_key != canonical
                && (track_id.starts_with("canonical:") || e.track_id != track_id)
        });

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.entries.push(RecordedEntry {
            track_id,
            canonical_key: canonical,
            title: meta.title.clone(),
            artist: meta.primary_artist().to_string(),
            album: meta.album.clone(),
            file_path,
            recorded_at_unix: now,
            duration_ms,
        });
    }

    pub fn prune_missing(&mut self) -> usize {
        let before = self.entries.len();
        let mut seen_paths = HashSet::new();
        self.entries.retain(|e| {
            file_exists_non_empty(&e.file_path) && seen_paths.insert(e.file_path.clone())
        });
        before.saturating_sub(self.entries.len())
    }
}

pub fn file_exists_non_empty(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

pub fn sanitize_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_space = false;

    for ch in raw.chars() {
        let mapped = match ch {
            '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => continue,
            c => c,
        };
        if mapped.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(mapped);
            last_space = false;
        }
    }

    let trimmed = out.trim().trim_matches('.');
    if trimmed.is_empty() {
        "Unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn render_track_path(config: &Config, meta: &TrackMetadata) -> PathBuf {
    let artist = sanitize_component(meta.primary_artist());
    let album_artist = sanitize_component(meta.primary_album_artist());
    let album = sanitize_component(if meta.album.trim().is_empty() {
        "Unknown Album"
    } else {
        &meta.album
    });
    let title = sanitize_component(&meta.title);
    let year = sanitize_component(meta.year().unwrap_or("Unknown"));
    let disc_number = meta
        .disc_number
        .filter(|&d| d > 0)
        .map(|d| d.to_string())
        .unwrap_or_else(|| "1".to_string());

    let mut rendered = config.path_template.clone();

    match meta.track_number.filter(|&t| t > 0) {
        Some(num) => {
            let formatted = format!("{num:02}");
            rendered = rendered.replace("{track_number}", &formatted);
        }
        None => {
            rendered = rendered
                .replace("{track_number} - ", "")
                .replace("{track_number}-", "")
                .replace("{track_number} ", "")
                .replace("{track_number}", "00");
        }
    }

    rendered = rendered
        .replace("{artist}", &artist)
        .replace("{album_artist}", &album_artist)
        .replace("{album}", &album)
        .replace("{disc_number}", &disc_number)
        .replace("{title}", &title)
        .replace("{year}", &year);

    let mut path = config.resolved_output_dir();
    for segment in rendered.split(['/', '\\']) {
        let clean_seg = segment.trim();
        if clean_seg.is_empty() || clean_seg == "." || clean_seg == ".." {
            continue;
        }
        path.push(clean_seg);
    }

    if path.extension().is_none() {
        path.set_extension("flac");
    }

    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta() -> TrackMetadata {
        TrackMetadata {
            track_id: "/com/spotify/track/abc123".to_string(),
            title: "Time: Pink/Floyd?".to_string(),
            artists: vec!["Pink Floyd".to_string()],
            album: "The Dark Side of the Moon".to_string(),
            album_artists: vec!["Pink Floyd".to_string()],
            track_number: Some(4),
            disc_number: Some(1),
            content_created: Some("1973-03-01".to_string()),
            art_url: None,
            length_us: 413_000_000,
        }
    }

    #[test]
    fn renders_sanitized_path_with_track_number() {
        let cfg = Config {
            output_dir: "/music/out".to_string(),
            ..Default::default()
        };
        let meta = sample_meta();

        let rendered = render_track_path(&cfg, &meta);
        assert_eq!(
            rendered,
            PathBuf::from("/music/out/Pink Floyd/The Dark Side of the Moon/04 - Time_ Pink_Floyd_.flac")
        );
    }

    #[test]
    fn omits_missing_track_number_prefix_cleanly() {
        let cfg = Config {
            output_dir: "/music/out".to_string(),
            ..Default::default()
        };
        let mut meta = sample_meta();
        meta.track_number = None;

        let rendered = render_track_path(&cfg, &meta);
        assert_eq!(
            rendered,
            PathBuf::from("/music/out/Pink Floyd/The Dark Side of the Moon/Time_ Pink_Floyd_.flac")
        );
    }

    #[test]
    fn duplicate_check_allows_rerecord_if_file_was_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            output_dir: dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let meta = sample_meta();
        let target = render_track_path(&cfg, &meta);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"flac-data").unwrap();

        let mut index = RecordedIndex::default();
        index.record_entry(&meta, target.clone(), 413_000);
        assert_eq!(index.find_duplicate(&cfg, &meta), Some(target.clone()));

        // Delete the file from disk -> should no longer count as duplicate
        std::fs::remove_file(&target).unwrap();
        assert_eq!(index.find_duplicate(&cfg, &meta), None);
    }
}
