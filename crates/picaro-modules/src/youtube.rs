//! YouTube module - thin wrapper over `yt-dlp` (if installed on PATH).
//!
//! For the Rust port we shell out to yt-dlp; this avoids re-implementing
//! YouTube's signing / decipher algorithms.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "YouTube".to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("download_pause_seconds".to_string(), json!(5));
            m.insert("download_mode".to_string(), json!("sequential"));
            m
        },
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Multi(vec![
            "youtube".to_string(),
            "youtu.be".to_string(),
            "music.youtube.com".to_string(),
        ]),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("watch".to_string(), DownloadType::track);
            m.insert("playlist".to_string(), DownloadType::playlist);
            m.insert("channel".to_string(), DownloadType::artist);
            m.insert("@".to_string(), DownloadType::artist);
            m
        },
        test_url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(YoutubeConstructor)
}

#[derive(Debug)]
struct YoutubeConstructor;

impl ModuleConstructor for YoutubeConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(YoutubeModule { controller }))
    }
}

#[derive(Debug)]
struct YoutubeModule {
    controller: ModuleController,
}

impl YoutubeModule {
    fn run_yt_dlp(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("yt-dlp");
        cmd.args(args);
        let output = cmd.output().map_err(|e| Error::Other(format!("yt-dlp not found: {e}. Install with `pip install yt-dlp` or download from https://github.com/yt-dlp/yt-dlp")))?;
        if !output.status.success() {
            return Err(Error::Other(format!(
                "yt-dlp failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Flat-playlist JSON for one `--playlist-items` slice.
    fn flat_playlist_slice(&self, url: &str, range: &str) -> Result<Value> {
        let out = self.run_yt_dlp(&[
            "-J",
            "--flat-playlist",
            "--no-warnings",
            "--playlist-items",
            range,
            url,
        ])?;
        serde_json::from_str(&out).map_err(|e| Error::Other(format!("yt-dlp JSON parse: {e}")))
    }

    /// Fetch all playlist entries following `playlistItems` pagination,
    /// mirroring `youtube_api.get_playlist_info` (yt-dlp `extract_flat` loop).
    /// yt-dlp shell-out has no cursor, so page with `--playlist-items N-M`.
    fn fetch_all_entries(&self, url: &str) -> Result<(Value, Vec<Value>)> {
        const PAGE: usize = 100;
        let mut start = 1usize;
        let mut first_meta = Value::Null;
        let mut all: Vec<Value> = Vec::new();
        loop {
            let range = format!("{start}-{}", start + PAGE - 1);
            let v = self.flat_playlist_slice(url, &range)?;
            if first_meta.is_null() {
                first_meta = v.clone();
            }
            let entries = v
                .get("entries")
                .and_then(|e| e.as_array())
                .cloned()
                .unwrap_or_default();
            // yt-dlp pads missing slots with nulls; drop them.
            let valid: Vec<Value> = entries.into_iter().filter(|e| e.is_object()).collect();
            if valid.is_empty() {
                break;
            }
            all.extend(valid.iter().cloned());
            if valid.len() < PAGE {
                break;
            }
            start += PAGE;
            if start > 5000 {
                break;
            }
        }
        Ok((first_meta, all))
    }

    /// Normalize a channel/artist input to a `/videos` URL, mirroring
    /// `youtube_api.get_channel_info` (+ `@`/`c/` handles, UC channel IDs).
    fn channel_videos_url(artist_id: &str) -> String {
        if artist_id.starts_with("http") {
            let base = artist_id
                .split('?')
                .next()
                .unwrap_or(artist_id)
                .trim_end_matches('/');
            if base.ends_with("/videos") {
                return artist_id.to_string();
            }
            return format!("{base}/videos");
        }
        if artist_id.starts_with('@') || artist_id.starts_with("c/") {
            return format!(
                "https://www.youtube.com/{}/videos",
                artist_id.trim_end_matches('/')
            );
        }
        // Handles like "@name" with full URL handled above; UC/UU/plain IDs:
        format!("https://www.youtube.com/channel/{}/videos", artist_id)
    }

    /// Split "Artist - Title" when the uploader matches, mirroring
    /// `ModuleInterface._parse_title_artist` in interface.py.
    fn parse_title_artist(title: &str, uploader: &str) -> (String, String) {
        let seps = [" - ", " – ", " : ", ": "];
        for sep in seps {
            if let Some(pos) = title.find(sep) {
                let (a, t) = title.split_at(pos);
                let artist = a.trim();
                let name = t[sep.len()..].trim();
                if !artist.is_empty()
                    && !name.is_empty()
                    && (uploader.to_lowercase().contains(&artist.to_lowercase())
                        || artist.to_lowercase().contains(&uploader.to_lowercase()))
                {
                    return (artist.to_string(), name.to_string());
                }
            }
        }
        (uploader.to_string(), title.to_string())
    }

    /// Best thumbnail: largest width×height from `thumbnails`, else `thumbnail`.
    /// Mirrors the `max(thumbnails, key=width*height)` pick in interface.py.
    fn best_thumbnail(v: &Value) -> String {
        let mut cover = v
            .get("thumbnail")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if let Some(thumbs) = v.get("thumbnails").and_then(|t| t.as_array()) {
            let mut best_url: Option<&str> = None;
            let mut best_area = 0u64;
            for t in thumbs {
                let w = t.get("width").and_then(|x| x.as_u64()).unwrap_or(0);
                let h = t.get("height").and_then(|x| x.as_u64()).unwrap_or(0);
                if w * h > best_area {
                    if let Some(u) = t.get("url").and_then(|x| x.as_str()) {
                        if !u.is_empty() {
                            best_area = w * h;
                            best_url = Some(u);
                        }
                    }
                }
            }
            if let Some(b) = best_url {
                cover = b.to_string();
            }
        }
        cover
    }

    /// Duration in whole seconds. yt-dlp emits int or float seconds;
    /// negative/NaN values map to None instead of wrapping on cast.
    fn json_duration(v: &Value) -> Option<u32> {
        let n = v.get("duration")?;
        if let Some(u) = n.as_u64() {
            return u32::try_from(u).ok();
        }
        if let Some(i) = n.as_i64() {
            return u32::try_from(i).ok();
        }
        n.as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f as u32)
    }

    /// Strip " - Topic" and common "Official Video/Audio/Lyrics/HD" tags,
    /// mirroring `_clean_title` (abbreviated tag set for the Rust port).
    fn clean_title(title: &str) -> String {
        let mut t = title.replace('–', "-").replace("–", "-");
        // Drop hashtags and everything after them.
        if let Some(pos) = t.find(" #") {
            t.truncate(pos);
        }
        let tags = [
            "official music video",
            "official lyric video",
            "official video",
            "official audio",
            "official",
            "music video",
            "lyric video",
            "visualizer",
            "visualiser",
            "audio",
            "lyrics",
            "lyric video",
            "m/v",
            "hd",
            "4k",
            "explicit",
            "full album",
            "album stream",
        ];
        for tag in tags {
            for (open, close) in [("(", ")"), ("[", "]"), ("{", "}")] {
                let pattern = format!("{open}{tag}{close}");
                // Case-insensitive removal of " (tag)" suffix-style markers.
                let lower = t.to_lowercase();
                if let Some(pos) = lower.find(&pattern) {
                    t = format!("{}{}", &t[..pos], &t[pos + pattern.len()..]);
                }
            }
            let suffix = format!(" - {tag}");
            let lower = t.to_lowercase();
            if lower.ends_with(&suffix) {
                t.truncate(t.len() - suffix.len());
            }
        }
        t.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// Parse a YouTube URL into (type, id).
/// Mirrors `parse_youtube_url` in `youtube_api.py`, extended to
/// `music.youtube.com` and `youtu.be` short links.
fn parse_youtube_url(url: &str) -> Option<(String, String)> {
    let url_l = url.to_lowercase();
    let is_yt = url_l.contains("youtube.com") || url_l.contains("youtu.be");
    if !is_yt {
        return None;
    }
    let get_param = |key: &str| -> Option<String> {
        url.split(&format!("{key}="))
            .nth(1)
            .map(|s| {
                s.split('&')
                    .next()
                    .unwrap_or("")
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .filter(|s| !s.is_empty())
    };
    // Video: watch?v=, youtu.be/, embed/, /v/, /shorts/
    for marker in ["watch?v=", "youtu.be/", "/embed/", "/v/", "/shorts/"] {
        if let Some(pos) = url.find(marker) {
            let rest = &url[pos + marker.len()..];
            let id: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            // Standard video IDs are 11 chars; accept >= 6 to tolerate new formats.
            if id.len() >= 6 {
                return Some(("video".to_string(), id));
            }
        }
    }
    // Playlist (?list=) wins over video when both present, mirroring Python order
    // for bare playlist URLs; watch+list links still resolve to the video above.
    if url.contains("/playlist") {
        if let Some(list) = get_param("list") {
            return Some(("playlist".to_string(), list));
        }
    }
    if let Some(pos) = url.find("/channel/") {
        let rest = &url[pos + "/channel/".len()..];
        let id: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !id.is_empty() {
            return Some(("channel".to_string(), id));
        }
    }
    if let Some(pos) = url.find("/c/") {
        let rest = &url[pos + "/c/".len()..];
        let id: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
            .collect();
        if !id.is_empty() {
            return Some(("channel".to_string(), format!("c/{id}")));
        }
    }
    if let Some(pos) = url.find("/@") {
        let rest = &url[pos + "/@".len()..];
        let id: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
            .collect();
        if !id.is_empty() {
            return Some(("channel".to_string(), format!("@{id}")));
        }
    }
    // Watch URL with list= but no /playlist path → still a video (handled above);
    // fall through to None.
    None
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for YoutubeModule {
    fn name(&self) -> &str {
        "YouTube"
    }

    async fn get_track_info(
        &self,
        track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        // Accept full URLs (youtu.be / music.youtube.com included) via the parser.
        let video_id = parse_youtube_url(track_id)
            .filter(|(t, _)| t == "video")
            .map(|(_, id)| id)
            .unwrap_or_else(|| track_id.to_string());
        let url = if track_id.starts_with("http") {
            track_id.to_string()
        } else {
            format!("https://www.youtube.com/watch?v={video_id}")
        };
        let out = self.run_yt_dlp(&["-J", "--no-warnings", &url])?;
        let v: Value = serde_json::from_str(&out)
            .map_err(|e| Error::Other(format!("yt-dlp JSON parse: {e}")))?;
        let raw_title = v
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let raw_title = Self::clean_title(&raw_title);
        let mut raw_uploader = v
            .get("uploader")
            .or_else(|| v.get("channel"))
            .and_then(|t| t.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let is_topic = raw_uploader.ends_with(" - Topic");
        if let Some(stripped) = raw_uploader.strip_suffix(" - Topic") {
            raw_uploader = stripped.to_string();
        }
        let (artist, title) = Self::parse_title_artist(&raw_title, &raw_uploader);
        // Only trust the thumbnail as album art for official "Topic" (YouTube
        // Music) uploads. For regular videos the thumbnail is a frame from the
        // music video, so leave it empty and let metadata fill fetch real art.
        let cover = if is_topic {
            Self::best_thumbnail(&v)
        } else {
            String::new()
        };
        let duration = Self::json_duration(&v);
        // upload_date is YYYYMMDD; release_date may be YYYY-MM-DD.
        let (release_year, release_date) = v
            .get("upload_date")
            .and_then(|d| d.as_str())
            .map(|s| {
                let year = s.get(0..4).and_then(|y| y.parse::<i32>().ok()).unwrap_or(0);
                let date = if s.len() == 8 {
                    format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
                } else {
                    String::new()
                };
                (year, if date.is_empty() { None } else { Some(date) })
            })
            .unwrap_or_else(|| {
                v.get("release_date")
                    .and_then(|d| d.as_str())
                    .map(|s| {
                        let year = s
                            .split('-')
                            .next()
                            .and_then(|y| y.parse::<i32>().ok())
                            .unwrap_or(0);
                        (year, Some(s.to_string()))
                    })
                    .unwrap_or((0, None))
            });
        let description = v
            .get("description")
            .and_then(|d| d.as_str())
            .map(|s| s.chars().take(500).collect::<String>());
        Ok(TrackInfo {
            name: title,
            album: String::new(),
            album_id: String::new(),
            artists: vec![artist.clone()],
            artist_id: v
                .get("channel_id")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string()),
            tags: Tags {
                album_artist: Some(artist),
                release_date,
                description,
                track_url: Some(format!("https://www.youtube.com/watch?v={video_id}")),
                ..Default::default()
            },
            codec: CodecFlags::OPUS,
            cover_url: cover,
            release_year,
            duration,
            id: Some(video_id.clone()),
            bit_depth: Some(16),
            sample_rate: Some(48.0),
            bitrate: Some(160),
            preview_url: Some(format!("https://www.youtube.com/watch?v={video_id}")),
            download_extra_kwargs: {
                let mut m = serde_json::Map::new();
                m.insert("video_id".to_string(), Value::String(video_id));
                m
            },
            ..Default::default()
        })
    }

    async fn get_track_download(
        &self,
        track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        let url = if track_id.starts_with("http") {
            track_id.to_string()
        } else {
            format!("https://www.youtube.com/watch?v={track_id}")
        };
        let tmp = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("temp")
            .join(format!("yt-{}.opus", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(tmp.parent().unwrap()).ok();
        let output = Command::new("yt-dlp")
            .args([
                "-x",
                "--audio-format",
                "opus",
                "--audio-quality",
                "0",
                "-o",
                tmp.to_str().unwrap(),
                "--no-warnings",
                &url,
            ])
            .output()
            .map_err(|e| Error::Other(format!("yt-dlp not found: {e}")))?;
        if !output.status.success() {
            return Err(Error::Other(format!(
                "yt-dlp failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        // The actual filename is the one with .opus appended
        let actual = if tmp.exists() {
            tmp.clone()
        } else {
            tmp.with_extension("opus")
        };
        if !actual.exists() {
            return Err(Error::Other("yt-dlp did not produce a file".into()));
        }
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::TempFilePath,
            file_url: None,
            file_url_headers: serde_json::Map::new(),
            temp_file_path: Some(actual),
            different_codec: Some(CodecFlags::OPUS),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        // YouTube has no albums; treat as playlist (mirrors interface.py).
        let pl = self.get_playlist_info(album_id, data).await?;
        Ok(AlbumInfo {
            name: pl.name,
            artist: pl.creator,
            tracks: pl.tracks,
            release_year: pl.release_year,
            id: pl.id,
            cover_url: pl.cover_url,
            description: pl.description,
            track_extra_kwargs: pl.track_extra_kwargs,
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        // Accept full URLs; UC channel IDs resolve to their UU uploads playlist.
        let mut pid = parse_youtube_url(playlist_id)
            .filter(|(t, _)| t == "playlist")
            .map(|(_, id)| id)
            .unwrap_or_else(|| playlist_id.to_string());
        if pid.starts_with("UC") {
            pid = format!("UU{}", &pid[2..]);
        }
        let url = if playlist_id.starts_with("http") && !pid.starts_with("UU") {
            playlist_id.to_string()
        } else {
            format!("https://www.youtube.com/playlist?list={pid}")
        };
        let (meta, entries) = self.fetch_all_entries(&url)?;
        let name = meta
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("Unknown")
            .to_string();
        // Thumbnail fallback: first entry thumbnail (mirrors youtube_api).
        let mut thumb = meta
            .get("thumbnail")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if thumb.is_empty() {
            thumb = entries
                .first()
                .and_then(|e| e.get("thumbnail"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
        }
        let track_ids: Vec<TrackRef> = entries
            .iter()
            .filter_map(|e| {
                e.get("id")
                    .and_then(|i| i.as_str())
                    .map(|s| TrackRef::Id(s.to_string()))
            })
            .collect();
        let track_data: serde_json::Map<String, Value> = entries
            .iter()
            .filter_map(|e| {
                e.get("id")
                    .and_then(|i| i.as_str())
                    .map(|s| (s.to_string(), e.clone()))
            })
            .collect();
        let creator = meta
            .get("uploader")
            .or_else(|| meta.get("channel"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        Ok(PlaylistInfo {
            name,
            creator: creator.clone(),
            tracks: track_ids,
            release_year: 0,
            id: Some(pid),
            cover_url: if thumb.is_empty() { None } else { Some(thumb) },
            description: meta
                .get("description")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string()),
            track_extra_kwargs: {
                let mut m = serde_json::Map::new();
                m.insert("data".to_string(), Value::Object(track_data));
                m.insert("channel_name".to_string(), Value::String(creator));
                m
            },
            ..Default::default()
        })
    }

    async fn get_artist_info(
        &self,
        artist_id: &str,
        _get_credited_albums: bool,
        _artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        // Channel uploads tab, mirroring get_channel_info (playlist_items 1-50 pages).
        let channel_id = parse_youtube_url(artist_id)
            .filter(|(t, _)| t == "channel")
            .map(|(_, id)| id)
            .unwrap_or_else(|| artist_id.to_string());
        let url = Self::channel_videos_url(&channel_id);
        let (meta, entries) = self.fetch_all_entries(&url)?;
        let name = meta
            .get("title")
            .or_else(|| meta.get("uploader"))
            .or_else(|| meta.get("channel"))
            .and_then(|t| t.as_str())
            .unwrap_or("Unknown")
            .to_string();
        // Track IDs + per-track data, mirroring interface.py's tracks/track_extra_kwargs.
        let track_ids: Vec<Value> = entries
            .iter()
            .filter_map(|e| {
                e.get("id")
                    .and_then(|i| i.as_str())
                    .map(|s| Value::String(s.to_string()))
            })
            .collect();
        let track_data: serde_json::Map<String, Value> = entries
            .iter()
            .filter_map(|e| {
                e.get("id")
                    .and_then(|i| i.as_str())
                    .map(|s| (s.to_string(), e.clone()))
            })
            .collect();
        Ok(ArtistInfo {
            name: name.clone(),
            artist_id: Some(channel_id),
            albums: Vec::new(),
            tracks: track_ids,
            track_extra_kwargs: {
                let mut m = serde_json::Map::new();
                m.insert("data".to_string(), Value::Object(track_data));
                m.insert("channel_name".to_string(), Value::String(name));
                m
            },
            ..Default::default()
        })
    }

    async fn get_track_credits(
        &self,
        _track_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<Vec<CreditsInfo>> {
        Ok(Vec::new())
    }

    async fn get_track_cover(
        &self,
        track_id: &str,
        _cover: &CoverOptions,
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        let url = if track_id.starts_with("http") {
            track_id.to_string()
        } else {
            format!("https://www.youtube.com/watch?v={track_id}")
        };
        let out = self.run_yt_dlp(&["-J", "--no-warnings", &url])?;
        let v: Value = serde_json::from_str(&out)
            .map_err(|e| Error::Other(format!("yt-dlp JSON parse: {e}")))?;
        // Pick the best thumbnail from the `thumbnails` array (mirrors
        // get_track_info); the top-level `thumbnail` alone is low-res.
        let cover = Self::best_thumbnail(&v);
        Ok(CoverInfo {
            url: cover,
            file_type: ImageFileType::Jpg,
        })
    }

    async fn search(
        &self,
        query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        // Hard cap 50, mirroring interface.py's limit clamp.
        let limit = limit.clamp(1, 50);
        // yt-dlp only implements `ytsearchN:` — there is no `ytsearchplaylist`
        // or `ytsearchchannel` scheme. Mirror youtube_api.search instead:
        // videos via `ytsearch`, playlists via a filtered results URL
        // (sp=EgIQAw%3D%3D), channels via over-fetched `ytsearch` + dedupe.
        // Albums don't exist on YouTube; search playlists instead.
        let (search_url, is_channel_search, playlist_range) = match query_type {
            DownloadType::artist => (format!("ytsearch{}:{query}", limit * 2), true, None),
            DownloadType::album | DownloadType::playlist => {
                let encoded = percent_encoding::utf8_percent_encode(
                    query,
                    percent_encoding::NON_ALPHANUMERIC,
                )
                .to_string();
                (
                    format!(
                        "https://www.youtube.com/results?search_query={encoded}&sp=EgIQAw%253D%253D"
                    ),
                    false,
                    Some(format!("1-{limit}")),
                )
            }
            _ => (format!("ytsearch{limit}:{query}"), false, None),
        };
        let mut args: Vec<&str> = vec!["-J", "--flat-playlist", "--no-warnings"];
        if let Some(range) = playlist_range.as_deref() {
            args.push("--playlist-items");
            args.push(range);
        }
        args.push(&search_url);
        let out = self.run_yt_dlp(&args)?;
        let v: Value = serde_json::from_str(&out)
            .map_err(|e| Error::Other(format!("yt-dlp JSON parse: {e}")))?;
        let entries = v
            .get("entries")
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default();
        // Channel search: dedupe video hits by channel_id (mirrors Python).
        if is_channel_search {
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();
            for e in &entries {
                let cid = e
                    .get("channel_id")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if cid.is_empty() || !seen.insert(cid.clone()) {
                    continue;
                }
                let name = e
                    .get("channel")
                    .or_else(|| e.get("uploader"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                let image_url = e
                    .get("channel_thumbnail")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        let t = Self::best_thumbnail(e);
                        if t.is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    });
                out.push(SearchResult {
                    result_id: cid,
                    name,
                    artists: None,
                    image_url,
                    ..Default::default()
                });
                if out.len() >= limit as usize {
                    break;
                }
            }
            return Ok(out);
        }
        let mut out = Vec::new();
        for e in entries {
            // Skip empty playlists in playlist searches (mirrors Python filter).
            if query_type == DownloadType::playlist || query_type == DownloadType::album {
                if let Some(n) = e.get("playlist_count").and_then(|x| x.as_u64()) {
                    if n == 0 {
                        continue;
                    }
                }
            }
            // Year from upload_date YYYYMMDD.
            let year = e
                .get("upload_date")
                .and_then(|d| d.as_str())
                .filter(|s| s.len() >= 4)
                .map(|s| s[..4].to_string())
                .or_else(|| {
                    e.get("release_date")
                        .and_then(|d| d.as_str())
                        .filter(|s| s.len() >= 4)
                        .map(|s| s[..4].to_string())
                });
            // Playlist track count in additional (mirrors _additional_for_result).
            // Flat results may carry `n_entries` instead of `playlist_count`.
            let count = e
                .get("playlist_count")
                .and_then(|x| x.as_u64())
                .or_else(|| e.get("n_entries").and_then(|x| x.as_u64()));
            let additional =
                if query_type == DownloadType::playlist || query_type == DownloadType::album {
                    count.map(|n| {
                        vec![if n == 1 {
                            "1 track".to_string()
                        } else {
                            format!("{n} tracks")
                        }]
                    })
                } else {
                    None
                };
            // Keep the Artist column blank when the uploader is unknown.
            let artists = e
                .get("uploader")
                .or_else(|| e.get("channel"))
                .and_then(|t| t.as_str())
                .filter(|s| !s.is_empty() && *s != "Unknown")
                .map(|s| vec![s.to_string()]);
            out.push(SearchResult {
                result_id: e
                    .get("id")
                    .and_then(|i| i.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                name: e
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string()),
                artists,
                year,
                additional,
                duration: Self::json_duration(&e),
                image_url: {
                    let t = Self::best_thumbnail(&e);
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                },
                ..Default::default()
            });
        }
        // Prefer audio / official uploads. Keep music videos only when there is
        // nothing else (per "avoid music videos unless it's the only source").
        let videoish = |r: &SearchResult| {
            let t = r.name.clone().unwrap_or_default().to_lowercase();
            t.contains("official video")
                || t.contains("official music video")
                || t.contains("music video")
                || t.contains("(mv)")
                || t.contains("[mv]")
                || t.contains("videoclip")
                || t.contains("(full album)")
                || t.contains("full album")
                || t.contains("official film")
        };
        let clean: Vec<SearchResult> = out.iter().filter(|r| !videoish(r)).cloned().collect();
        let out = if clean.is_empty() { out } else { clean };
        Ok(out)
    }

    fn custom_url_parse(&self, url: &str) -> Result<Option<MediaIdentification>> {
        let Some((kind, id)) = parse_youtube_url(url) else {
            return Ok(None);
        };
        let media_type = match kind.as_str() {
            "video" => DownloadType::track,
            "playlist" => DownloadType::playlist,
            "channel" => DownloadType::artist,
            _ => return Ok(None),
        };
        Ok(Some(MediaIdentification {
            media_type,
            media_id: id.clone(),
            extra_kwargs: {
                let mut m = serde_json::Map::new();
                m.insert("data".to_string(), json!({ id.clone(): Value::Null }));
                m
            },
        }))
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_video_urls() {
        let cases = [
            (
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                "video",
                "dQw4w9WgXcQ",
            ),
            ("https://youtu.be/dQw4w9WgXcQ", "video", "dQw4w9WgXcQ"),
            (
                "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
                "video",
                "dQw4w9WgXcQ",
            ),
            (
                "https://www.youtube.com/embed/dQw4w9WgXcQ",
                "video",
                "dQw4w9WgXcQ",
            ),
            (
                "https://www.youtube.com/shorts/dQw4w9WgXcQ",
                "video",
                "dQw4w9WgXcQ",
            ),
        ];
        for (url, kind, id) in cases {
            assert_eq!(
                parse_youtube_url(url),
                Some((kind.to_string(), id.to_string())),
                "{url}"
            );
        }
    }

    #[test]
    fn parse_playlist_and_channel_urls() {
        assert_eq!(
            parse_youtube_url("https://www.youtube.com/playlist?list=PL123abc_-"),
            Some(("playlist".to_string(), "PL123abc_-".to_string()))
        );
        assert_eq!(
            parse_youtube_url("https://music.youtube.com/playlist?list=OLAK5uy_test"),
            Some(("playlist".to_string(), "OLAK5uy_test".to_string()))
        );
        assert_eq!(
            parse_youtube_url("https://www.youtube.com/channel/UC123abc"),
            Some(("channel".to_string(), "UC123abc".to_string()))
        );
        assert_eq!(
            parse_youtube_url("https://www.youtube.com/@SomeArtist"),
            Some(("channel".to_string(), "@SomeArtist".to_string()))
        );
        assert_eq!(
            parse_youtube_url("https://www.youtube.com/c/SomeArtist"),
            Some(("channel".to_string(), "c/SomeArtist".to_string()))
        );
        assert_eq!(parse_youtube_url("https://example.com/not-youtube"), None);
    }

    #[test]
    fn channel_videos_url_forms() {
        assert_eq!(
            YoutubeModule::channel_videos_url("UC123"),
            "https://www.youtube.com/channel/UC123/videos"
        );
        assert_eq!(
            YoutubeModule::channel_videos_url("@handle"),
            "https://www.youtube.com/@handle/videos"
        );
        assert_eq!(
            YoutubeModule::channel_videos_url("https://www.youtube.com/channel/UC123"),
            "https://www.youtube.com/channel/UC123/videos"
        );
    }

    #[test]
    fn title_artist_split() {
        assert_eq!(
            YoutubeModule::parse_title_artist("Anne-Marie - Alarm", "Anne-Marie"),
            ("Anne-Marie".to_string(), "Alarm".to_string())
        );
        // Uploader unrelated: keep full title (mirrors Python's containment check).
        assert_eq!(
            YoutubeModule::parse_title_artist("Apples - Oranges", "TechReviewer"),
            ("TechReviewer".to_string(), "Apples - Oranges".to_string())
        );
    }

    #[test]
    fn duration_int_float_and_invalid() {
        let int = json!({"duration": 213});
        let float = json!({"duration": 213.7});
        let neg = json!({"duration": -5});
        let missing: Value = json!({});
        assert_eq!(YoutubeModule::json_duration(&int), Some(213));
        assert_eq!(YoutubeModule::json_duration(&float), Some(213));
        assert_eq!(YoutubeModule::json_duration(&neg), None);
        assert_eq!(YoutubeModule::json_duration(&missing), None);
    }

    #[test]
    fn best_thumbnail_picks_largest() {
        let v = json!({
            "thumbnail": "https://i.ytimg.com/vi/X/default.jpg",
            "thumbnails": [
                {"url": "https://i.ytimg.com/vi/X/small.jpg", "width": 120, "height": 90},
                {"url": "https://i.ytimg.com/vi/X/big.jpg", "width": 1280, "height": 720},
            ],
        });
        assert_eq!(
            YoutubeModule::best_thumbnail(&v),
            "https://i.ytimg.com/vi/X/big.jpg"
        );
        // Falls back to `thumbnail` when the array is absent.
        let flat = json!({"thumbnail": "https://i.ytimg.com/vi/X/default.jpg"});
        assert_eq!(
            YoutubeModule::best_thumbnail(&flat),
            "https://i.ytimg.com/vi/X/default.jpg"
        );
    }
}
