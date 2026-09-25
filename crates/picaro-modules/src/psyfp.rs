//! PsyFP (psyfp.ucoz.ru) site module.
//!
//! uCoz-hosted psychedelic trance blog.
//! Search:    GET https://psyfp.ucoz.ru/search/?q=<query>&t=0
//! Results:   <a href="/news/<slug>/<date>-<id>">Title</a>
//! Item page: links out to file hosts (yandex / rapidgator), which are exposed
//!            as download sources for the host resolvers.

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
const REFERER: &str = "https://psyfp.ucoz.ru/";
const BASE: &str = "https://psyfp.ucoz.ru";
const SERVICE: &str = "PsyFP";

const HOST_PATTERN: &str = "rapidgator|yandex|zippy|hitfile|turbobit|mediafire|mega\\.nz|katfile|1fichier|uploaded|gofile|pixeldrain";

pub fn module_information() -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    url_constants.insert("news".to_string(), DownloadType::album);
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("psyfp.ucoz.ru".to_string()),
        url_constants,
        test_url: Some("https://psyfp.ucoz.ru/search/?q=psy&t=0".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(PsyfpConstructor)
}

#[derive(Debug)]
struct PsyfpConstructor;

impl ModuleConstructor for PsyfpConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(PsyfpModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct PsyfpModule {
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
        .map_err(|e| Error::Other(format!("psyfp fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("psyfp HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("psyfp read: {e}")))
}

fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let re = Regex::new(r#"(?is)<a[^>]*href="(/news/[^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let tag = Regex::new(r"<[^>]*>").unwrap();
    let mut out: Vec<SearchResult> = Vec::new();
    for cap in re.captures_iter(html) {
        let url = absolute(cap.get(1).map(|m| m.as_str()).unwrap_or(""));
        let raw = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let name = decode_entities(&tag.replace_all(raw, " "));
        if url.is_empty() || name.len() < 3 {
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

fn parse_host_links(html: &str) -> Vec<String> {
    let re = Regex::new(&format!(
        r#"(?is)href="(https?://[^"]*(?:{HOST_PATTERN})[^"]*)""#
    ))
    .unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str().trim().to_string();
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    out
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for PsyfpModule {
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
            codec: CodecFlags::FLAC,
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
                different_codec: Some(CodecFlags::FLAC),
            });
        }
        if !track_id.starts_with("http") {
            return Err(Error::Other(format!(
                "psyfp: expected file-host URL, got {track_id}"
            )));
        }
        let mut headers = serde_json::Map::new();
        headers.insert("Referer".to_string(), json!(REFERER));
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(track_id.to_string()),
            file_url_headers: headers,
            temp_file_path: None,
            different_codec: Some(CodecFlags::FLAC),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let url = absolute(album_id);
        let html = fetch_page(&self.client, &url).await?;
        let links = parse_host_links(&html);
        if links.is_empty() {
            return Err(Error::Other(format!(
                "psyfp: no file-host links found at {url}"
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
            tracks: links.into_iter().map(TrackRef::Id).collect(),
            release_year: 0,
            id: Some(album_id.to_string()),
            quality: Some("FLAC".to_string()),
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
            "psyfp: no cover for track {track_id}"
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
        let url = format!("{BASE}/search/?q={encoded}&t=0");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("psyfp search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("psyfp search read: {e}")))?;
        let mut results = parse_search_results(&html);
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
