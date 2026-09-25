//! 94hiphop (94hiphop.com) site module.
//!
//! A WordPress hip-hop blog. Search hits are `<article>` blocks whose title is
//! `<div class="ttl"><a href="…">Title</a>`; the shared `wordpress_blog`
//! parser does not match that theme, so this is a standalone module.
//!
//! Search:   GET https://94hiphop.com/?s=<query>
//! Post page: mirrors the release on wayshare.cc, which is behind a Cloudflare
//!            "managed" challenge the downloader will not bypass, so downloads
//!            are best-effort only.

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
const REFERER: &str = "https://94hiphop.com/";
const BASE: &str = "https://94hiphop.com";
const SERVICE: &str = "HipHop94";

pub fn module_information() -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    url_constants.insert("album".to_string(), DownloadType::album);
    url_constants.insert("mixtape".to_string(), DownloadType::album);
    url_constants.insert("flac".to_string(), DownloadType::album);
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("94hiphop.com".to_string()),
        url_constants,
        test_url: Some("https://94hiphop.com/?s=nas".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(HipHop94Constructor)
}

#[derive(Debug)]
struct HipHop94Constructor;

impl ModuleConstructor for HipHop94Constructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(HipHop94Module {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct HipHop94Module {
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_entities(s: &str) -> String {
    s.replace("&#8211;", "-")
        .replace("&#8212;", "-")
        .replace("&#8217;", "'")
        .replace("&#8216;", "'")
        .replace("&#038;", "&")
        .replace("&#38;", "&")
        .replace("&#8230;", "...")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn strip_tags(s: &str) -> String {
    let re = Regex::new(r"<[^>]*>").unwrap();
    decode_entities(&re.replace_all(s, " "))
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
        .map_err(|e| Error::Other(format!("hiphop94 fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("hiphop94 HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("hiphop94 read: {e}")))
}

fn clean_release(title: &str) -> String {
    // Drop trailing "(WEB) (2022) (320 kbps)" style markers.
    let re = Regex::new(r"\s*\([^()]*\)\s*$").unwrap();
    let mut t = title.to_string();
    for _ in 0..4 {
        let next = re.replace(&t, "").to_string();
        if next == t {
            break;
        }
        t = next;
    }
    t.trim().to_string()
}

fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let re =
        Regex::new(r#"(?is)<div class="ttl">\s*<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let mut out: Vec<SearchResult> = Vec::new();
    for cap in re.captures_iter(html) {
        let url = absolute(cap.get(1).map(|m| m.as_str()).unwrap_or(""));
        let title = clean_release(&strip_tags(cap.get(2).map(|m| m.as_str()).unwrap_or("")));
        if url.is_empty() || title.is_empty() {
            continue;
        }
        if out.iter().any(|r| r.result_id == url) {
            continue;
        }
        let (artist, album) = match title.find(" - ") {
            Some(i) => (
                Some(title[..i].trim().to_string()),
                title[i + 3..].trim().to_string(),
            ),
            None => (None, title.clone()),
        };
        out.push(SearchResult {
            result_id: url,
            name: Some(album),
            artists: artist.filter(|a| !a.is_empty()).map(|a| vec![a]),
            ..Default::default()
        });
    }
    out
}

fn parse_host_links(html: &str) -> Vec<String> {
    let re = Regex::new(
        r#"(?is)href="(https?://[^"]*(?:wayshare|mega\.nz|mediafire|1fichier|pixeldrain|gofile|drive\.google|dropbox|yandex|katfile|rapidgator|filecrypt)[^"]*)""#,
    )
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

fn parse_post_title(html: &str) -> (String, String) {
    let raw = Regex::new(r#"(?is)<meta property="og:title" content="([^"]+)""#)
        .unwrap()
        .captures(html)
        .and_then(|c| c.get(1).map(|m| decode_entities(m.as_str())))
        .or_else(|| {
            Regex::new(r"(?is)<title>(.*?)</title>")
                .unwrap()
                .captures(html)
                .and_then(|c| c.get(1).map(|m| decode_entities(m.as_str())))
        })
        .unwrap_or_else(|| "Unknown".to_string());
    let raw = raw.split(" - HQ Hip-Hop Blog").next().unwrap_or(&raw);
    let cleaned = clean_release(raw);
    match cleaned.find(" - ") {
        Some(i) => (
            cleaned[..i].trim().to_string(),
            cleaned[i + 3..].trim().to_string(),
        ),
        None => (String::new(), cleaned.trim().to_string()),
    }
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for HipHop94Module {
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
            album: data
                .get("__album__")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            album_id: String::new(),
            artists: data
                .get("__artist__")
                .and_then(|v| v.as_str())
                .map(|a| vec![a.to_string()])
                .unwrap_or_default(),
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
                "hiphop94: expected file-host URL, got {track_id}"
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
                "hiphop94: no file-host links found at {url}"
            )));
        }
        let (artist, album) = parse_post_title(&html);
        let cover = Regex::new(r#"(?is)<meta property="og:image" content="([^"]+)""#)
            .unwrap()
            .captures(&html)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        Ok(AlbumInfo {
            name: album,
            artist,
            tracks: links.into_iter().map(TrackRef::Id).collect(),
            release_year: 0,
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
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        Err(Error::Other(format!(
            "hiphop94: no cover for track {track_id}"
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
        let html = fetch_page(&self.client, &url).await?;
        let mut results = parse_search_results(&html);
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
