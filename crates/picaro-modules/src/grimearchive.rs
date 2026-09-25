//! The Grime Archive (grimearchive.org) site module.
//!
//! Search:   GET https://www.grimearchive.org/search?title=<query>
//! Results:  <a href="/mix/<id>">Mix Title</a>  (plus /dj, /mc, /station links)
//! Mix page: <a download href="/download/<id>">[Download]</a> serves the mix
//!           directly from the archive host (no external file host).

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
const REFERER: &str = "https://www.grimearchive.org/";
const BASE: &str = "https://www.grimearchive.org";
const SERVICE: &str = "GrimeArchive";

pub fn module_information() -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    url_constants.insert("mix".to_string(), DownloadType::album);
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("www.grimearchive.org".to_string()),
        url_constants,
        test_url: Some("https://www.grimearchive.org/search?title=grime".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(GrimeArchiveConstructor)
}

#[derive(Debug)]
struct GrimeArchiveConstructor;

impl ModuleConstructor for GrimeArchiveConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(GrimeArchiveModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct GrimeArchiveModule {
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

fn absolute(url: &str) -> String {
    if url.starts_with("http") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("{BASE}{url}")
    } else {
        format!("{BASE}/{url}")
    }
}

fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let re = Regex::new(r#"(?is)<a[^>]*href="(/mix/[^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let mut out: Vec<SearchResult> = Vec::new();
    for cap in re.captures_iter(html) {
        let url = absolute(cap.get(1).map(|m| m.as_str()).unwrap_or(""));
        let raw = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let tag = Regex::new(r"<[^>]*>").unwrap();
        let name = decode_entities(&tag.replace_all(raw, " "));
        if url.is_empty() || name.is_empty() {
            continue;
        }
        if out.iter().any(|r| r.result_id == url) {
            continue;
        }
        out.push(SearchResult {
            result_id: url,
            name: Some(name),
            ..Default::default()
        });
    }
    out
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("grimearchive fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("grimearchive HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("grimearchive read: {e}")))
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for GrimeArchiveModule {
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
            .unwrap_or_else(|| track_id.rsplit('/').next().unwrap_or(track_id).to_string());
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
                "grimearchive: expected download URL, got {track_id}"
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
        let download = Regex::new(r#"(?is)<a[^>]*href="(/download/\d+)"#)
            .unwrap()
            .captures(&html)
            .and_then(|c| c.get(1).map(|m| absolute(m.as_str())));
        let download = download
            .ok_or_else(|| Error::Other(format!("grimearchive: no download link at {url}")))?;
        let title = Regex::new(r"(?is)<title>(.*?)</title>")
            .unwrap()
            .captures(&html)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
            .map(|t| {
                let t = decode_entities(t.split(" | ").next().unwrap_or(&t).trim());
                match t.split_once(" - ") {
                    Some((a, m)) => format!("{a} - {m}"),
                    None => t,
                }
            })
            .unwrap_or_else(|| album_id.to_string());
        Ok(AlbumInfo {
            name: title,
            artist: String::new(),
            tracks: vec![TrackRef::Id(download)],
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
            "grimearchive: no cover for track {track_id}"
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
        let url = format!("{BASE}/search?title={encoded}");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("grimearchive search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("grimearchive search read: {e}")))?;
        let mut results = parse_search_results(&html);
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
