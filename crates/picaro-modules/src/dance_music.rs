//! Dance Music Organisation (dance-music.org) site module.
//!
//! The site is backed by Perl CGI scripts under `/cgi-bin/`:
//!   - `/cgi-bin/free-mp3-music-downloads.pl?artist=<id>&track=<id>` lists a
//!     track and exposes **direct** links of the form
//!     `/cgi-bin/download_music.pl?MP3=1&ID=<id>&UID=<token>` (audio/mpeg).
//!   - The homepage / chart is itself served by the same `.pl` script.
//!
//! There is no native full-text search, so `search()` scans the chart listing
//! and filters by substring on the track title.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const REFERER: &str = "https://dance-music.org/";
const BASE: &str = "https://dance-music.org";
const SERVICE: &str = "DanceMusicOrg";

pub fn module_information() -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    url_constants.insert("cgi-bin".to_string(), DownloadType::track);
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("dance-music.org".to_string()),
        url_constants,
        test_url: Some("https://dance-music.org/".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(DanceMusicConstructor)
}

#[derive(Debug)]
struct DanceMusicConstructor;

impl ModuleConstructor for DanceMusicConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(DanceMusicModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct DanceMusicModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn absolute(url: &str) -> String {
    if url.starts_with("http") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("{BASE}{url}")
    } else {
        format!("{BASE}/{url}")
    }
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("dance-music fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("dance-music HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("dance-music read: {e}")))
}

fn parse_track_links(html: &str) -> Vec<(String, String)> {
    let re = Regex::new(
        r#"(?is)<a[^>]*href="(/cgi-bin/free-mp3-music-downloads\.pl\?[^"]*artist=[^"]*track=[^"]*)"[^>]*>(.*?)</a>"#,
    )
    .unwrap();
    let tag = Regex::new(r"<[^>]*>").unwrap();
    let mut out: Vec<(String, String)> = Vec::new();
    for cap in re.captures_iter(html) {
        let url = absolute(cap.get(1).map(|m| m.as_str()).unwrap_or(""));
        let raw = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let name = decode_entities(&tag.replace_all(raw, " "));
        if url.is_empty() || name.is_empty() {
            continue;
        }
        if out.iter().any(|(u, _)| *u == url) {
            continue;
        }
        out.push((url, name));
    }
    out
}

fn parse_download_links(html: &str) -> Vec<String> {
    let re = Regex::new(r#"(?is)href="(/cgi-bin/download_music\.pl\?[^"]+)""#).unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let url = absolute(cap.get(1).map(|m| m.as_str()).unwrap_or(""));
        if !url.is_empty() && !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for DanceMusicModule {
    fn name(&self) -> &str {
        SERVICE
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
        let name = data
            .get("__track_name__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let q = track_id.split('?').nth(1).unwrap_or("");
                let re = Regex::new(r"ID=(\d+)").unwrap();
                re.captures(q)
                    .and_then(|c| c.get(1).map(|m| format!("Track {}", m.as_str())))
                    .unwrap_or_else(|| track_id.to_string())
            });
        Ok(TrackInfo {
            name,
            album: String::new(),
            album_id: String::new(),
            artists: vec![],
            codec: CodecFlags::MP3,
            cover_url: String::new(),
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
        if Path::new(track_id).exists() {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::TempFilePath,
                file_url: None,
                file_url_headers: serde_json::Map::new(),
                temp_file_path: Some(std::path::PathBuf::from(track_id)),
                different_codec: Some(CodecFlags::MP3),
            });
        }
        if !track_id.starts_with("http") {
            return Err(Error::Other(format!(
                "dance-music: expected direct MP3 URL, got {track_id}"
            )));
        }
        let mut headers = serde_json::Map::new();
        headers.insert("Referer".to_string(), json!(REFERER));
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(track_id.to_string()),
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
        let url = absolute(album_id);
        let html = fetch_page(&self.client, &url).await?;
        let downloads = parse_download_links(&html);
        if downloads.is_empty() {
            return Err(Error::Other(format!(
                "dance-music: no download links at {url}"
            )));
        }
        let title = Regex::new(r"(?is)<title>(.*?)</title>")
            .unwrap()
            .captures(&html)
            .and_then(|c| c.get(1).map(|m| decode_entities(m.as_str())))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| album_id.to_string());
        Ok(AlbumInfo {
            name: title,
            artist: String::new(),
            tracks: downloads.into_iter().map(TrackRef::Id).collect(),
            release_year: 0,
            id: Some(album_id.to_string()),
            quality: Some("MP3".to_string()),
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        _playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.to_string(),
            ability: "playlist".to_string(),
        })
    }

    async fn get_artist_info(
        &self,
        _artist_id: &str,
        _get_credited_albums: bool,
        _artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.to_string(),
            ability: "artist".to_string(),
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
        Err(Error::Other(format!(
            "dance-music: no cover for track {track_id}"
        )))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let _ = utf8_percent_encode(query, NON_ALPHANUMERIC);
        let url = format!("{BASE}/cgi-bin/free-mp3-music-downloads.pl");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("dance-music search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("dance-music search read: {e}")))?;
        let needle = query.trim().to_lowercase();
        let mut out: Vec<SearchResult> = Vec::new();
        for (url, name) in parse_track_links(&html) {
            if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
                continue;
            }
            out.push(SearchResult {
                result_id: url,
                name: Some(name),
                ..Default::default()
            });
            if out.len() >= limit as usize {
                break;
            }
        }
        out.truncate(limit as usize);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
