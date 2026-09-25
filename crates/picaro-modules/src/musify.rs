use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
const REFERER: &str = "https://musify.club/";
const HOST: &str = "musify.club";
const SERVICE: &str = "Musify";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single(HOST.to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("release".to_string(), DownloadType::album);
            m.insert("track".to_string(), DownloadType::track);
            m
        },
        test_url: Some("https://musify.club/".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(MusifyConstructor)
}

#[derive(Debug)]
struct MusifyConstructor;

impl ModuleConstructor for MusifyConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(MusifyModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct MusifyModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn resolve_url(id: &str) -> String {
    if id.starts_with("http://") || id.starts_with("https://") {
        id.to_string()
    } else if id.starts_with('/') {
        format!("https://{HOST}{id}")
    } else {
        format!("https://{HOST}/{id}")
    }
}

fn decode_entities_basic(s: &str) -> String {
    s.replace("&#8211;", "-")
        .replace("&#8217;", "'")
        .replace("&#039;", "'")
        .replace("&amp;", "&")
}

fn split_title(title: &str) -> (Option<String>, Option<String>, Option<String>) {
    let year_re = Regex::new(r"\((\d{4})\)").unwrap();
    let title = title.trim();
    if let Some(idx) = title.find(" - ") {
        let artist = title[..idx].trim().to_string();
        let rest = title[idx + 3..].trim().to_string();
        let year = year_re
            .captures(&rest)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let artist = if artist.is_empty() {
            None
        } else {
            Some(artist)
        };
        (artist, Some(rest), year)
    } else {
        let year = year_re
            .captures(title)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let album = if title.is_empty() {
            None
        } else {
            Some(title.to_string())
        };
        (None, album, year)
    }
}

fn parse_title(html: &str) -> String {
    let og = Regex::new(r#"<meta property="og:title" content="([^"]+)""#).unwrap();
    if let Some(c) = og.captures(html) {
        if let Some(m) = c.get(1) {
            return decode_entities_basic(m.as_str().trim());
        }
    }
    let title_re = Regex::new(r"<title>(.*?)</title>").unwrap();
    title_re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| decode_entities_basic(m.as_str().trim())))
        .unwrap_or_else(|| "Unknown".to_string())
}

fn parse_cover(html: &str) -> Option<String> {
    let og = Regex::new(r#"<meta property="og:image" content="([^"]+)""#).unwrap();
    og.captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|s| !s.is_empty())
}

// UNVERIFIED: musify.club markup is not stable; this collects any anchor whose
// href looks like a per-track / release download endpoint.
fn parse_download_links(html: &str) -> Vec<String> {
    let re = Regex::new(r#"href="([^"]*(?:/download/|/get/|download\.)[^"]*)""#).unwrap();
    let mut out: Vec<String> = Vec::new();
    for cap in re.captures_iter(html) {
        let href = match cap.get(1) {
            Some(m) => m.as_str().trim(),
            None => continue,
        };
        if href.is_empty() || href.starts_with("javascript:") {
            continue;
        }
        let full = resolve_url(href);
        if !out.contains(&full) {
            out.push(full);
        }
    }
    out
}

fn album_meta(data: &HashMap<String, Value>) -> (String, String, String, Option<i32>) {
    match data.get("__album_meta__") {
        Some(v) => {
            let a = v
                .get("album")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let ar = v
                .get("artist")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let c = v
                .get("cover")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let y = v.get("year").and_then(|x| x.as_i64()).map(|i| i as i32);
            (a, ar, c, y)
        }
        None => (String::new(), String::new(), String::new(), None),
    }
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for MusifyModule {
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
        let (album, artist, cover, year) = album_meta(&data);
        let derived = track_id
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(track_id)
            .to_string();
        let name = data
            .get("__track_name__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| derived.clone());
        Ok(TrackInfo {
            name,
            album,
            album_id: String::new(),
            artists: if artist.is_empty() {
                vec![]
            } else {
                vec![artist]
            },
            tags: Tags {
                release_date: year.map(|y| format!("{y}-01-01")),
                ..Default::default()
            },
            codec: CodecFlags::MP3,
            cover_url: cover,
            release_year: year.unwrap_or(0),
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
                temp_file_path: Some(PathBuf::from(track_id)),
                different_codec: Some(CodecFlags::FLAC),
            });
        }
        if track_id.starts_with("https://") {
            let mut headers = serde_json::Map::new();
            headers.insert("Referer".to_string(), json!(REFERER));
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::Url,
                file_url: Some(track_id.to_string()),
                file_url_headers: headers,
                temp_file_path: None,
                different_codec: Some(CodecFlags::MP3),
            });
        }
        Err(Error::Other(format!(
            "musify: expected direct download URL, got {track_id}"
        )))
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let url = resolve_url(album_id);
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("musify album fetch: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!("musify album HTTP {}", resp.status())));
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("musify album read: {e}")))?;
        let title = parse_title(&html);
        let (artist_opt, album_opt, year) = split_title(&title);
        let artist = artist_opt.unwrap_or_else(|| "Unknown Artist".to_string());
        let album = album_opt.unwrap_or_else(|| title.clone());
        let cover = parse_cover(&html);
        let links = parse_download_links(&html);
        if links.is_empty() {
            return Err(Error::Other(
                "musify: no download link found on release page".to_string(),
            ));
        }
        let tracks = links.into_iter().map(TrackRef::Id).collect();
        Ok(AlbumInfo {
            name: album,
            artist: artist.clone(),
            tracks,
            release_year: year
                .as_deref()
                .and_then(|y| y.parse::<i32>().ok())
                .unwrap_or(0),
            artist_id: None,
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
        data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        if let Some(v) = data.get("__cover__").and_then(|v| v.as_str()) {
            return Ok(CoverInfo {
                url: v.to_string(),
                file_type: ImageFileType::Jpg,
            });
        }
        Err(Error::Other(format!(
            "musify: no cover for track {track_id}"
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
        let url = format!("https://{HOST}/search?query={encoded}");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("musify search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("musify search read: {e}")))?;
        // UNVERIFIED: exact search-result anchor markup is a guess.
        let re = Regex::new(r#"<a[^>]+href="(/release/[^"]+)"[^>]*>([^<]+)</a>"#).unwrap();
        let mut out: Vec<SearchResult> = Vec::new();
        for cap in re.captures_iter(&html) {
            let href = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let title = cap
                .get(2)
                .map(|m| decode_entities_basic(m.as_str().trim()))
                .unwrap_or_default();
            if href.is_empty() {
                continue;
            }
            let result_id = format!("https://{HOST}{href}");
            if out.iter().any(|r| r.result_id == result_id) {
                continue;
            }
            let (artist, album, year) = split_title(&title);
            out.push(SearchResult {
                result_id,
                name: album,
                artists: artist.map(|a| vec![a]),
                year,
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
