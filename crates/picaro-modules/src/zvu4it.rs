use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const REFERER: &str = "https://zvu4it.org/";
const BASE: &str = "https://zvu4it.org";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "Zvu4it".to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Multi(vec![
            "zvu4it.org".to_string(),
            "data.zvu4it.org".to_string(),
        ]),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("tracks".to_string(), DownloadType::artist);
            m.insert("track".to_string(), DownloadType::track);
            m
        },
        test_url: Some("https://zvu4it.org/?s=radiohead".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(Zvu4itConstructor)
}

#[derive(Debug)]
struct Zvu4itConstructor;

impl ModuleConstructor for Zvu4itConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(Zvu4itModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct Zvu4itModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn normalize_url(raw: &str) -> String {
    if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw.to_string()
    }
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("zvu4it fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("zvu4it HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("zvu4it read: {e}")))
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&apos;", "'")
        .trim()
        .to_string()
}

// UNVERIFIED: each result is a .f-table block containing .artist-name,
// .track-name, an image and a direct <a class="mp3" href="//data.zvu4it.org/...">
// MP3 download link (verified against https://zvu4it.org/?s=radiohead).
fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let track_re = Regex::new(r#"<div class="track-name">([^<]+)</div>"#).unwrap();
    let artist_re =
        Regex::new(r#"<div class="artist-name"><a[^>]*href="([^"]*)"[^>]*>([^<]+)</a></div>"#)
            .unwrap();
    let img_re = Regex::new(r#"<div class="c-img"><img src="([^"]+)""#).unwrap();
    let mp3_re = Regex::new(r#"<a class="mp3" href="([^"]+)""#).unwrap();

    let mut out: Vec<SearchResult> = Vec::new();
    for block in html.split(r#"class="f-table""#).skip(1) {
        let mp3 = match mp3_re.captures(block) {
            Some(c) => c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
            None => continue,
        };
        if mp3.is_empty() {
            continue;
        }
        let track = track_re
            .captures(block)
            .and_then(|c| c.get(1).map(|m| decode_entities(m.as_str())))
            .unwrap_or_default();
        let artist = artist_re
            .captures(block)
            .and_then(|c| c.get(2).map(|m| decode_entities(m.as_str())))
            .unwrap_or_default();
        let image = img_re
            .captures(block)
            .and_then(|c| c.get(1).map(|m| normalize_url(m.as_str())));
        let result_id = normalize_url(&mp3);
        if out.iter().any(|r| r.result_id == result_id) {
            continue;
        }
        let name = if !artist.is_empty() && !track.is_empty() {
            format!("{artist} - {track}")
        } else if !track.is_empty() {
            track
        } else {
            result_id.clone()
        };
        out.push(SearchResult {
            result_id,
            name: Some(name),
            artists: if artist.is_empty() {
                None
            } else {
                Some(vec![artist])
            },
            image_url: image,
            ..Default::default()
        });
    }
    out
}

fn track_name_from_url(url: &str) -> String {
    let file = url.split('/').next_back().unwrap_or(url);
    let decoded = percent_decode_str(file).decode_utf8_lossy().to_string();
    let without_ext = decoded
        .strip_suffix(".mp3")
        .or_else(|| decoded.strip_suffix(".MP3"))
        .unwrap_or(&decoded);
    without_ext.trim().to_string()
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for Zvu4itModule {
    fn name(&self) -> &str {
        "Zvu4it"
    }

    fn is_authenticated(&self) -> bool {
        true
    }

    async fn get_track_info(
        &self,
        track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        let meta = data.get("__album_meta__").cloned();
        let (album, artist, cover) = match meta {
            Some(v) => (
                v.get("album")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("artist")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("cover")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            None => (String::new(), String::new(), String::new()),
        };
        let derived = track_name_from_url(track_id);
        let name = data
            .get("__track_name__")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if !artist.is_empty() && !album.is_empty() {
                    format!("{artist} - {album}")
                } else if !derived.is_empty() {
                    derived.clone()
                } else {
                    track_id.to_string()
                }
            });
        let artists = if !artist.is_empty() {
            vec![artist]
        } else if derived.contains(" - ") {
            let a = derived.split(" - ").next().unwrap_or("").trim();
            if a.is_empty() {
                vec![]
            } else {
                vec![a.to_string()]
            }
        } else {
            vec![]
        };
        Ok(TrackInfo {
            name,
            album,
            album_id: String::new(),
            artists,
            codec: CodecFlags::MP3,
            cover_url: cover,
            release_year: 0,
            id: Some(track_id.to_string()),
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
        let path = Path::new(track_id);
        let mut headers = serde_json::Map::new();
        headers.insert("Referer".to_string(), json!(REFERER));
        if path.exists() {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::TempFilePath,
                file_url: None,
                file_url_headers: headers,
                temp_file_path: Some(path.to_path_buf()),
                different_codec: Some(CodecFlags::MP3),
            });
        }
        let url = normalize_url(track_id);
        if !url.starts_with("http") {
            return Err(Error::Other(format!(
                "zvu4it: expected MP3 URL, got {track_id}"
            )));
        }
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(url),
            file_url_headers: headers,
            temp_file_path: None,
            different_codec: Some(CodecFlags::MP3),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        if album_id.contains("/download-track/") || album_id.ends_with(".mp3") {
            let url = normalize_url(album_id);
            let name = track_name_from_url(&url);
            let artist = name.split(" - ").next().unwrap_or("").trim().to_string();
            return Ok(AlbumInfo {
                name: name.clone(),
                artist: artist.clone(),
                tracks: vec![TrackRef::Id(url)],
                release_year: 0,
                id: Some(album_id.to_string()),
                quality: Some("MP3".to_string()),
                ..Default::default()
            });
        }
        let url = if album_id.starts_with("http") {
            album_id.to_string()
        } else {
            format!("{BASE}/{}", album_id.trim_start_matches('/'))
        };
        let html = fetch_page(&self.client, &url).await?;
        let tracks = parse_search_results(&html);
        if tracks.is_empty() {
            return Err(Error::Other(format!("zvu4it: no tracks found at {url}")));
        }
        let h1_re = Regex::new(r#"<div id="site-h1">([^<]+)</div>"#).unwrap();
        let name = h1_re
            .captures(&html)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
            .map(|s| s.split('.').next().unwrap_or(&s).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| album_id.to_string());
        let cover = tracks.first().and_then(|t| t.image_url.clone());
        Ok(AlbumInfo {
            name,
            artist: String::new(),
            tracks: tracks
                .into_iter()
                .map(|t| TrackRef::Id(t.result_id))
                .collect(),
            release_year: 0,
            id: Some(album_id.to_string()),
            quality: Some("MP3".to_string()),
            cover_url: cover,
            cover_type: Some(ImageFileType::Jpg),
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        _playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "Zvu4it".to_string(),
            ability: "playlist".to_string(),
        })
    }

    async fn get_artist_info(
        &self,
        artist_id: &str,
        _get_credited_albums: bool,
        _artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        let url = if artist_id.starts_with("http") {
            artist_id.to_string()
        } else if artist_id.starts_with("/tracks/") {
            format!("{BASE}{artist_id}")
        } else {
            let enc = utf8_percent_encode(artist_id, NON_ALPHANUMERIC).to_string();
            format!("{BASE}/tracks/{enc}")
        };
        let html = fetch_page(&self.client, &url).await?;
        let tracks = parse_search_results(&html);
        let name = artist_id
            .trim_start_matches("/tracks/")
            .trim_matches('/')
            .to_string();
        Ok(ArtistInfo {
            name,
            artist_id: Some(artist_id.to_string()),
            tracks: tracks
                .into_iter()
                .map(|t| serde_json::Value::String(t.result_id))
                .collect(),
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
        data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        if let Some(v) = data.get("__cover__").and_then(|v| v.as_str()) {
            return Ok(CoverInfo {
                url: v.to_string(),
                file_type: ImageFileType::Jpg,
            });
        }
        Err(Error::Other(format!(
            "zvu4it: no cover for track {track_id}"
        )))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let encoded = utf8_percent_encode(query, NON_ALPHANUMERIC).to_string();
        let url = format!("{BASE}/?s={encoded}");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("zvu4it search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("zvu4it search read: {e}")))?;
        let mut results = parse_search_results(&html);
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
