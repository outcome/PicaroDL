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
const REFERER: &str = "https://nodata.tv/";
const BASE: &str = "https://nodata.tv";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "NoData".to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("nodata.tv".to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("search".to_string(), DownloadType::album);
            m
        },
        test_url: Some("https://nodata.tv/?s=radiohead".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(NoDataConstructor)
}

#[derive(Debug)]
struct NoDataConstructor;

impl ModuleConstructor for NoDataConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(NoDataModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct NoDataModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_entities(s: &str) -> String {
    s.replace("&#8211;", "-")
        .replace("&#8212;", "-")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("nodata fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("nodata HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("nodata read: {e}")))
}

// UNVERIFIED: search results are `<section class="post post-type4">` blocks with
// `<div class="title"><h4><a href="https://nodata.tv/<id>">Title [year]</a></h4>`
// (verified against https://nodata.tv/?s=radiohead).
fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let title_re =
        Regex::new(r#"<h4><a href="(https://nodata\.tv/\d+)"[^>]*>([^<]+)</a></h4>"#).unwrap();
    let img_re = Regex::new(r#"<img[^>]+src="([^"]+)""#).unwrap();
    let year_re = Regex::new(r"[\(\[](\d{4})[\)\]]").unwrap();

    let mut out: Vec<SearchResult> = Vec::new();
    for block in html.split(r#"class="post post-type4""#).skip(1) {
        let (url, raw_title) = match title_re.captures(block) {
            Some(c) => (
                c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
                c.get(2)
                    .map(|m| decode_entities(m.as_str()))
                    .unwrap_or_default(),
            ),
            None => continue,
        };
        if url.is_empty() || out.iter().any(|r| r.result_id == url) {
            continue;
        }
        let image_url = img_re
            .captures(block)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let year = year_re
            .captures(&raw_title)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let (artist, album) = match raw_title.find(" / ") {
            Some(idx) => (
                Some(raw_title[..idx].trim().to_string()),
                Some(raw_title[idx + 3..].trim().to_string()),
            ),
            None => (None, Some(raw_title.clone())),
        };
        out.push(SearchResult {
            result_id: url,
            name: album,
            artists: artist.map(|a| vec![a]),
            year,
            image_url,
            ..Default::default()
        });
    }
    out
}

fn parse_post_title(html: &str) -> String {
    let re = Regex::new(r"(?s)<title>(.*?)</title>").unwrap();
    re.captures(html)
        .and_then(|c| c.get(1).map(|m| decode_entities(m.as_str())))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".to_string())
}

fn parse_cover(html: &str) -> Option<String> {
    let re = Regex::new(r#"src="(https://nodata\.tv/wp-content/[^"]+)""#).unwrap();
    re.captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

// Hosters live in `<div class="linkbox"><a href="https://...">TB</a>...</div>`.
fn parse_download_links(html: &str) -> Vec<String> {
    let box_re = Regex::new(r#"(?s)<div class="linkbox">(.*?)</div>"#).unwrap();
    let href_re = Regex::new(r#"href="(https?://[^"]+)""#).unwrap();
    let mut out: Vec<String> = Vec::new();
    if let Some(cap) = box_re.captures(html) {
        if let Some(inner) = cap.get(1) {
            for c in href_re.captures_iter(inner.as_str()) {
                if let Some(m) = c.get(1) {
                    let url = m.as_str().trim().to_string();
                    if !url.is_empty() && !out.contains(&url) {
                        out.push(url);
                    }
                }
            }
        }
    }
    out
}

fn track_name_from_url(url: &str) -> String {
    let file = url
        .split('?')
        .next()
        .unwrap_or(url)
        .split('/')
        .next_back()
        .unwrap_or(url);
    let decoded = percent_decode_str(file).decode_utf8_lossy().to_string();
    let without_ext = decoded
        .strip_suffix(".mp3")
        .or_else(|| decoded.strip_suffix(".flac"))
        .or_else(|| decoded.strip_suffix(".zip"))
        .unwrap_or(&decoded);
    without_ext.trim().to_string()
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for NoDataModule {
    fn name(&self) -> &str {
        "NoData"
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
                if !derived.is_empty() {
                    derived.clone()
                } else {
                    track_id.to_string()
                }
            });
        Ok(TrackInfo {
            name,
            album,
            album_id: String::new(),
            artists: if artist.is_empty() {
                vec![]
            } else {
                vec![artist]
            },
            codec: CodecFlags::FLAC,
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
                different_codec: Some(CodecFlags::FLAC),
            });
        }
        let url = if track_id.starts_with("//") {
            format!("https:{track_id}")
        } else {
            track_id.to_string()
        };
        if !url.starts_with("http") {
            return Err(Error::Other(format!(
                "nodata: expected file-host URL, got {track_id}"
            )));
        }
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(url),
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
        let url = if album_id.starts_with("http") {
            album_id.to_string()
        } else {
            format!("{BASE}/{}", album_id.trim_start_matches('/'))
        };
        let html = fetch_page(&self.client, &url).await?;
        let title = parse_post_title(&html);
        let (artist, album) = match title.find(" / ") {
            Some(idx) => (
                title[..idx].trim().to_string(),
                title[idx + 3..].trim().to_string(),
            ),
            None => (String::new(), title.clone()),
        };
        let links = parse_download_links(&html);
        if links.is_empty() {
            return Err(Error::Other(format!(
                "nodata: no file-host links found at {url}"
            )));
        }
        Ok(AlbumInfo {
            name: album,
            artist,
            tracks: links.into_iter().map(TrackRef::Id).collect(),
            release_year: 0,
            id: Some(album_id.to_string()),
            quality: Some("FLAC".to_string()),
            cover_url: parse_cover(&html),
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
            module: "NoData".to_string(),
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
            module: "NoData".to_string(),
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
            "nodata: no cover for track {track_id}"
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
            .map_err(|e| Error::Other(format!("nodata search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("nodata search read: {e}")))?;
        let mut results = parse_search_results(&html);
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
