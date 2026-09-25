//! Ektoplazm (ektoplazm.com) site module.
//!
//! Ektoplazm is a WordPress-powered free psytrance netlabel portal that hosts
//! releases as *direct* downloadable archives on its own `/files/` path.
//!
//! Search:      GET https://ektoplazm.com/?s=<query>
//! Results:     <a href="https://ektoplazm.com/free-music/<slug>" rel="bookmark">Title</a>
//! Release page: <a href="https://ektoplazm.com/files/<Name>%20-%20MP3.zip"> etc.
//!
//! Each archive link (MP3 / FLAC / WAV) is exposed as a track whose id is the
//! direct file URL, so downloads bypass any file host resolver.

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
const REFERER: &str = "https://ektoplazm.com/";
const BASE: &str = "https://ektoplazm.com";
const SERVICE: &str = "Ektoplazm";

pub fn module_information() -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    url_constants.insert("free-music".to_string(), DownloadType::album);
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("ektoplazm.com".to_string()),
        url_constants,
        test_url: Some("https://ektoplazm.com/?s=psytrance".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(EktoplazmConstructor)
}

#[derive(Debug)]
struct EktoplazmConstructor;

impl ModuleConstructor for EktoplazmConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(EktoplazmModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct EktoplazmModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#8217;", "'")
        .replace("&#8211;", "-")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn strip_tags(s: &str) -> String {
    let re = Regex::new(r"<[^>]*>").unwrap();
    let t = re.replace_all(s, " ");
    decode_entities(&t.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn codec_for_ext(ext: &str) -> CodecFlags {
    match ext.to_lowercase().as_str() {
        "flac" | "wav" | "zip" | "rar" => CodecFlags::FLAC,
        _ => CodecFlags::MP3,
    }
}

fn ext_for_url(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    path.rsplit('.').next().unwrap_or("").to_lowercase()
}

fn track_name_from_url(url: &str) -> String {
    let file = url.split('/').next_back().unwrap_or(url);
    let decoded = percent_encoding::percent_decode_str(file)
        .decode_utf8_lossy()
        .to_string();
    decoded
        .rsplit_once('.')
        .map(|(n, _)| n.to_string())
        .unwrap_or(decoded)
        .trim()
        .to_string()
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("ektoplazm fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("ektoplazm HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("ektoplazm read: {e}")))
}

fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let re = Regex::new(
        r#"(?is)<a[^>]*href="(https://ektoplazm\.com/free-music/[^"]+)"[^>]*rel="bookmark"[^>]*>(.*?)</a>"#,
    )
    .unwrap();
    let mut out: Vec<SearchResult> = Vec::new();
    for cap in re.captures_iter(html) {
        let url = cap
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let title = cap
            .get(2)
            .map(|m| strip_tags(m.as_str()))
            .unwrap_or_default();
        if url.is_empty() || title.is_empty() {
            continue;
        }
        if out.iter().any(|r| r.result_id == url) {
            continue;
        }
        out.push(SearchResult {
            result_id: url,
            name: Some(title),
            ..Default::default()
        });
    }
    out
}

fn parse_album_meta(html: &str) -> (String, String, Option<String>) {
    let og_title = Regex::new(r#"(?is)<meta property="og:title" content="([^"]+)""#).unwrap();
    let title = if let Some(c) = og_title.captures(html) {
        decode_entities(c.get(1).map(|m| m.as_str()).unwrap_or(""))
    } else {
        let t = Regex::new(r"(?is)<title>(.*?)</title>").unwrap();
        let raw = t
            .captures(html)
            .and_then(|c| c.get(1).map(|m| m.as_str()))
            .unwrap_or("Unknown");
        let raw = raw.split(" - Ektoplazm").next().unwrap_or(raw);
        decode_entities(raw)
    };
    let (artist, album) = match title.find(" - ") {
        Some(i) => (
            title[..i].trim().to_string(),
            title[i + 3..].trim().to_string(),
        ),
        None => (String::new(), title.clone()),
    };
    let cover = Regex::new(r#"(?is)<meta property="og:image" content="([^"]+)""#)
        .unwrap()
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .filter(|s| !s.is_empty());
    (artist, album, cover)
}

fn parse_file_links(html: &str) -> Vec<String> {
    let re = Regex::new(
        r#"(?is)href="(https://ektoplazm\.com/files/[^"]+\.(?:zip|rar|7z|flac|wav|mp3))""#,
    )
    .unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str().to_string();
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    out
}

fn album_meta(data: &HashMap<String, Value>) -> (String, String, String) {
    match data.get("__album_meta__") {
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
    }
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for EktoplazmModule {
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
        let (album0, artist0, cover0) = album_meta(&data);
        let derived = track_name_from_url(track_id);
        let cover = data
            .get("__cover__")
            .and_then(|v| v.as_str())
            .unwrap_or(&cover0)
            .to_string();
        let artist = data
            .get("__artist__")
            .and_then(|v| v.as_str())
            .unwrap_or(&artist0)
            .to_string();
        let name = data
            .get("__track_name__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| derived.clone());
        Ok(TrackInfo {
            name,
            album: album0,
            album_id: String::new(),
            artists: if artist.is_empty() {
                vec![]
            } else {
                vec![artist]
            },
            codec: codec_for_ext(&ext_for_url(track_id)),
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
        let mut headers = serde_json::Map::new();
        headers.insert("Referer".to_string(), json!(REFERER));
        if Path::new(track_id).exists() {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::TempFilePath,
                file_url: None,
                file_url_headers: headers,
                temp_file_path: Some(std::path::PathBuf::from(track_id)),
                different_codec: Some(codec_for_ext(&ext_for_url(track_id))),
            });
        }
        if !track_id.starts_with("http") {
            return Err(Error::Other(format!(
                "ektoplazm: expected direct file URL, got {track_id}"
            )));
        }
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(track_id.to_string()),
            file_url_headers: headers,
            temp_file_path: None,
            different_codec: Some(codec_for_ext(&ext_for_url(track_id))),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let url = if album_id.starts_with("http") {
            album_id.to_string()
        } else {
            format!("{BASE}/{}", album_id.trim_start_matches('/'))
        };
        let html = fetch_page(&self.client, &url).await?;
        let (artist, album, cover) = parse_album_meta(&html);
        let links = parse_file_links(&html);
        if links.is_empty() {
            return Err(Error::Other(format!(
                "ektoplazm: no file downloads found at {url}"
            )));
        }
        Ok(AlbumInfo {
            name: album,
            artist,
            tracks: links.into_iter().map(TrackRef::Id).collect(),
            release_year: 0,
            artist_id: None,
            id: Some(album_id.to_string()),
            quality: Some("FLAC".to_string()),
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
            "ektoplazm: no cover for track {track_id}"
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
            .map_err(|e| Error::Other(format!("ektoplazm search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("ektoplazm search read: {e}")))?;
        let mut results = parse_search_results(&html);
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
