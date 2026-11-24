use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::Config;
use crate::mpris::TrackMetadata;

pub struct CoverArtCache {
    max_entries: usize,
    order: VecDeque<String>,
    entries: HashMap<String, Vec<u8>>,
}

impl Default for CoverArtCache {
    fn default() -> Self {
        Self::new(32)
    }
}

impl CoverArtCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            order: VecDeque::new(),
            entries: HashMap::new(),
        }
    }

    pub fn get(&self, url: &str) -> Option<&Vec<u8>> {
        self.entries.get(url)
    }

    pub fn insert(&mut self, url: String, data: Vec<u8>) {
        if self.entries.contains_key(&url) {
            return;
        }
        if self.entries.len() >= self.max_entries
            && let Some(oldest) = self.order.pop_front()
        {
            self.entries.remove(&oldest);
        }
        self.order.push_back(url.clone());
        self.entries.insert(url, data);
    }
}

pub fn detect_image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("png")
    } else {
        None
    }
}

fn decode_file_url(url: &str) -> Option<PathBuf> {
    let raw = url.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(raw.len());
    let raw_bytes = raw.as_bytes();
    let mut i = 0;
    while i < raw_bytes.len() {
        if raw_bytes[i] == b'%'
            && i + 2 < raw_bytes.len()
            && let Ok(hex_str) = std::str::from_utf8(&raw_bytes[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex_str, 16)
        {
            bytes.push(val);
            i += 3;
            continue;
        }
        bytes.push(raw_bytes[i]);
        i += 1;
    }
    let decoded = String::from_utf8(bytes).ok()?;
    Some(PathBuf::from(decoded))
}

pub async fn fetch_cover_art(art_url: &str, cache: &mut CoverArtCache) -> Result<Option<Vec<u8>>> {
    let trimmed = art_url.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if let Some(cached) = cache.get(trimmed) {
        return Ok(Some(cached.clone()));
    }

    let bytes = if trimmed.starts_with("file://") {
        let Some(path) = decode_file_url(trimmed) else {
            return Ok(None);
        };
        tokio::fs::read(&path)
            .await
            .with_context(|| format!("failed to read local cover art {}", path.display()))?
    } else if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let output = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--fail",
                "--location",
                "--max-time",
                "15",
                trimmed,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to execute curl for album art")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("curl failed fetching {}: {}", trimmed, stderr.trim());
        }
        output.stdout
    } else {
        return Ok(None);
    };

    if detect_image_extension(&bytes).is_none() {
        return Ok(None);
    }

    cache.insert(trimmed.to_string(), bytes.clone());
    Ok(Some(bytes))
}

fn sanitize_tag_value(val: &str) -> String {
    val.replace(['\r', '\n', '\0'], " ").trim().to_string()
}

pub async fn encode_and_tag_flac(
    pcm_samples: &[i16],
    meta: &TrackMetadata,
    config: &Config,
    output_path: &Path,
    cover_bytes: Option<&[u8]>,
) -> Result<()> {
    let parent = output_path
        .parent()
        .context("output path has no parent directory")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("failed to create directory {}", parent.display()))?;

    let unique = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let tmp_flac = parent.join(format!(".tmp-{unique}.flac"));

    let res = write_temp_flac_and_tags(
        pcm_samples,
        meta,
        config,
        &tmp_flac,
        parent,
        &unique,
        cover_bytes,
    )
    .await;

    if let Err(err) = res {
        let _ = tokio::fs::remove_file(&tmp_flac).await;
        return Err(err);
    }

    tokio::fs::rename(&tmp_flac, output_path)
        .await
        .with_context(|| {
            format!(
                "failed to move {} to {}",
                tmp_flac.display(),
                output_path.display()
            )
        })?;

    Ok(())
}

async fn write_temp_flac_and_tags(
    pcm_samples: &[i16],
    meta: &TrackMetadata,
    config: &Config,
    tmp_flac: &Path,
    parent: &Path,
    unique: &str,
    cover_bytes: Option<&[u8]>,
) -> Result<()> {
    let compression = config.audio.flac_compression_level.min(8);
    let compression_flag = format!("-{compression}");
    let channels_arg = config.stream.channels.to_string();
    let rate_arg = config.stream.sample_rate.to_string();

    let mut child = Command::new("flac")
        .args([
            "--silent",
            "--force",
            &compression_flag,
            "--endian=little",
            "--sign=signed",
            "--channels",
            &channels_arg,
            "--bps=16",
            "--sample-rate",
            &rate_arg,
            "-o",
        ])
        .arg(tmp_flac)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn flac encoder")?;

    let mut stdin = child.stdin.take().context("flac missing stdin pipe")?;
    let mut pcm_bytes = Vec::with_capacity(pcm_samples.len() * 2);
    for &sample in pcm_samples {
        pcm_bytes.extend_from_slice(&sample.to_le_bytes());
    }
    stdin
        .write_all(&pcm_bytes)
        .await
        .context("failed writing PCM samples to flac stdin")?;
    stdin.shutdown().await?;
    drop(stdin);

    let flac_out = child.wait_with_output().await?;
    if !flac_out.status.success() {
        let stderr = String::from_utf8_lossy(&flac_out.stderr);
        anyhow::bail!(
            "flac encoding failed ({}): {}",
            flac_out.status,
            stderr.trim()
        );
    }

    let mut tag_args: Vec<String> = vec!["--remove-all-tags".to_string()];

    let title = sanitize_tag_value(&meta.title);
    if !title.is_empty() {
        tag_args.push(format!("--set-tag=TITLE={title}"));
    }
    for artist in &meta.artists {
        let clean = sanitize_tag_value(artist);
        if !clean.is_empty() {
            tag_args.push(format!("--set-tag=ARTIST={clean}"));
        }
    }
    let album = sanitize_tag_value(&meta.album);
    if !album.is_empty() {
        tag_args.push(format!("--set-tag=ALBUM={album}"));
    }
    for album_artist in &meta.album_artists {
        let clean = sanitize_tag_value(album_artist);
        if !clean.is_empty() {
            tag_args.push(format!("--set-tag=ALBUMARTIST={clean}"));
        }
    }
    if let Some(track_num) = meta.track_number.filter(|&n| n > 0) {
        tag_args.push(format!("--set-tag=TRACKNUMBER={track_num}"));
    }
    if let Some(disc_num) = meta.disc_number.filter(|&n| n > 0) {
        tag_args.push(format!("--set-tag=DISCNUMBER={disc_num}"));
    }
    if let Some(ref created) = meta.content_created {
        let clean = sanitize_tag_value(created);
        if !clean.is_empty() {
            tag_args.push(format!("--set-tag=DATE={clean}"));
        }
    }
    let track_id = sanitize_tag_value(&meta.track_id);
    if !track_id.is_empty() {
        tag_args.push(format!("--set-tag=TRACK_ID={track_id}"));
        if track_id.contains("spotify") {
            tag_args.push(format!("--set-tag=SPOTIFY_TRACK_ID={track_id}"));
        }
    }

    let mut cover_tmp_path: Option<PathBuf> = None;
    if let Some(bytes) = cover_bytes
        && let Some(ext) = detect_image_extension(bytes)
    {
        let cover_path = parent.join(format!(".tmp-cover-{unique}.{ext}"));
        tokio::fs::write(&cover_path, bytes)
            .await
            .with_context(|| format!("failed writing temp cover {}", cover_path.display()))?;
        tag_args.push(format!("--import-picture-from={}", cover_path.display()));
        cover_tmp_path = Some(cover_path);
    }

    let meta_out = Command::new("metaflac")
        .args(&tag_args)
        .arg(tmp_flac)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await;

    if let Some(ref cover_path) = cover_tmp_path {
        let _ = tokio::fs::remove_file(cover_path).await;
    }

    let meta_out = meta_out.context("failed to execute metaflac")?;
    if !meta_out.status.success() {
        let stderr = String::from_utf8_lossy(&meta_out.stderr);
        anyhow::bail!("metaflac failed ({}): {}", meta_out.status, stderr.trim());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal valid 1x1 red PNG
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[tokio::test]
    async fn flac_roundtrip_embeds_vorbis_tags_and_cover_art() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("Artist/Album/01 - Song.flac");
        let cover_file = dir.path().join("cover with space.png");
        std::fs::write(&cover_file, TINY_PNG).unwrap();

        let mut cache = CoverArtCache::default();
        let cover_url = format!(
            "file://{}",
            cover_file.to_string_lossy().replace(' ', "%20")
        );
        let fetched = fetch_cover_art(&cover_url, &mut cache)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched, TINY_PNG);

        // 0.2s stereo 440Hz sine wave at 44100 Hz
        let frames = 8820usize;
        let mut samples = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f32 / 44100.0;
            let val = ((2.0 * std::f32::consts::PI * 440.0 * t).sin() * 8000.0) as i16;
            samples.push(val);
            samples.push(val);
        }

        let cfg = Config::default();
        let meta = TrackMetadata {
            track_id: "/com/spotify/track/test123".to_string(),
            title: "Test Track".to_string(),
            artists: vec!["Test Artist".to_string()],
            album: "Test Album".to_string(),
            album_artists: vec!["Test Artist".to_string()],
            track_number: Some(1),
            disc_number: Some(1),
            content_created: Some("2025-01-01".to_string()),
            art_url: Some(cover_url),
            length_us: 200_000,
        };

        encode_and_tag_flac(&samples, &meta, &cfg, &output, Some(&fetched))
            .await
            .unwrap();

        assert!(output.exists());

        let inspect = std::process::Command::new("metaflac")
            .arg("--list")
            .arg(&output)
            .output()
            .unwrap();
        assert!(inspect.status.success());
        let list_text = String::from_utf8_lossy(&inspect.stdout);
        assert!(list_text.contains("TITLE=Test Track"));
        assert!(list_text.contains("ARTIST=Test Artist"));
        assert!(list_text.contains("ALBUM=Test Album"));
        assert!(list_text.contains("TRACKNUMBER=1"));
        assert!(list_text.contains("SPOTIFY_TRACK_ID=/com/spotify/track/test123"));
        assert!(list_text.contains("type: 6 (PICTURE)"));
    }
}
