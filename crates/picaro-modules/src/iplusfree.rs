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
const REFERER: &str = "https://iplusfree.org/";
const BASE: &str = "https://www7.iplusfree.org";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "iPlusfree".to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("iplusfree.org".to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("album".to_string(), DownloadType::album);
            m.insert("category".to_string(), DownloadType::album);
            m.insert("tag".to_string(), DownloadType::album);
            m.insert("single".to_string(), DownloadType::track);
            m
        },
        test_url: Some(
            "https://iplusfree.org/radiohead-kid-a-mnesia-itunes-plus-aac-m4a/".to_string(),
        ),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(IplusfreeConstructor)
}

#[derive(Debug)]
struct IplusfreeConstructor;

impl ModuleConstructor for IplusfreeConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(IplusfreeModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct IplusfreeModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_entities(s: &str) -> String {
    s.replace("&#8211;", "\u{2013}")
        .replace("&#8217;", "'")
        .replace("&#8216;", "'")
        .replace("&#039;", "'")
        .replace("&#038;", "&")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#124;", "|")
        .trim()
        .to_string()
}

fn strip_format_suffix(name: &str) -> String {
    let mut out = name.trim().to_string();
    for suffix in [
        " [iTunes Plus AAC M4A]",
        " [iTunes Plus AAC]",
        " (iTunes Plus AAC M4A)",
        " [iTunes M4A]",
        " [M4A]",
        " [iTunes Plus]",
    ] {
        if out.ends_with(suffix) {
            out.truncate(out.len() - suffix.len());
        }
    }
    out.trim().to_string()
}

fn split_artist_album(title: &str) -> (Option<String>, String) {
    let t = decode_entities(title);
    for sep in [" \u{2013} ", " - "] {
        if let Some(idx) = t.find(sep) {
            let artist = t[..idx].trim().to_string();
            let album = t[idx + sep.len()..].trim().to_string();
            if !artist.is_empty() && !album.is_empty() {
                return (Some(artist), strip_format_suffix(&album));
            }
        }
    }
    (None, strip_format_suffix(&t))
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("iplusfree fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("iplusfree HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("iplusfree read: {e}")))
}

// UNVERIFIED: search anchors verified (rel="bookmark" / class="wpp-post-title"),
// title format "Artist – Album [iTunes Plus AAC M4A]".
fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let patterns = [
        r#"<a href="(https?://[^"]*iplusfree\.org/[^"]+)"[^>]*rel="bookmark"[^>]*>([^<]{3,})</a>"#,
        r#"<a href="(https?://[^"]*iplusfree\.org/[^"]+)"[^>]*class="wpp-post-title"[^>]*>([^<]{3,})</a>"#,
    ];
    let mut out: Vec<SearchResult> = Vec::new();
    for pat in patterns {
        let re = match Regex::new(pat) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for cap in re.captures_iter(html) {
            let url = cap
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let raw_title = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if url.is_empty() || raw_title.trim().is_empty() {
                continue;
            }
            let (artist, album) = split_artist_album(raw_title);
            if out.iter().any(|r| r.result_id == url) {
                continue;
            }
            out.push(SearchResult {
                result_id: url,
                name: Some(album),
                artists: artist.map(|a| vec![a]),
                ..Default::default()
            });
        }
    }
    out
}

// UNVERIFIED: article download links live in <div id="ipf-dl-links"> as
// <a class="dl-link-a" href="...">Link N</a>.
fn parse_dl_links(html: &str) -> Vec<String> {
    let mut links = Vec::new();
    if let Ok(re) = Regex::new(r#"<a[^>]*class="dl-link-a"[^>]*href="([^"]+)""#) {
        for cap in re.captures_iter(html) {
            if let Some(m) = cap.get(1) {
                links.push(m.as_str().to_string());
            }
        }
    }
    if links.is_empty() {
        if let Ok(re) = Regex::new(r#"<a[^>]*href="([^"]+)"[^>]*class="dl-link-a""#) {
            for cap in re.captures_iter(html) {
                if let Some(m) = cap.get(1) {
                    links.push(m.as_str().to_string());
                }
            }
        }
    }
    links
}

fn parse_album_meta(html: &str) -> (String, String, Option<String>) {
    let og_title_re = Regex::new(r#"<meta property="og:title" content="([^"]+)""#).unwrap();
    let title = og_title_re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .unwrap_or_else(|| "Unknown".to_string());
    let title = title.replace(" - iPlusfree", "");
    let (artist, album) = split_artist_album(&title);
    let cover_re = Regex::new(r#"<meta property="og:image" content="([^"]+)""#).unwrap();
    let cover = cover_re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    (
        artist.unwrap_or_else(|| "Unknown Artist".to_string()),
        album,
        cover,
    )
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for IplusfreeModule {
    fn name(&self) -> &str {
        "iPlusfree"
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
        let (album, artist, cover) = match data.get("__album_meta__").cloned() {
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
                (a, ar, c)
            }
            None => (String::new(), String::new(), String::new()),
        };
        let track_name = data
            .get("__track_name__")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let name = track_name.unwrap_or_else(|| format!("{} - {} (M4A)", artist, album));
        Ok(TrackInfo {
            name,
            album: album.clone(),
            album_id: String::new(),
            artists: if artist.is_empty() {
                vec![]
            } else {
                vec![artist]
            },
            codec: CodecFlags::AAC,
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
                different_codec: Some(CodecFlags::AAC),
            });
        }
        if !track_id.starts_with("http") {
            return Err(Error::Other(format!(
                "iplusfree: expected download URL, got {track_id}"
            )));
        }
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(track_id.to_string()),
            file_url_headers: headers,
            temp_file_path: None,
            different_codec: Some(CodecFlags::AAC),
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
        let links = parse_dl_links(&html);
        let download = links
            .into_iter()
            .next()
            .ok_or_else(|| Error::Other("iplusfree: no download link on post".to_string()))?;
        Ok(AlbumInfo {
            name: album,
            artist: artist.clone(),
            tracks: vec![TrackRef::Id(download)],
            release_year: 0,
            artist_id: None,
            id: Some(album_id.to_string()),
            quality: Some("M4A".to_string()),
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
            module: "iPlusfree".to_string(),
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
            module: "iPlusfree".to_string(),
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
            "iplusfree: no cover for track {track_id}"
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
            .map_err(|e| Error::Other(format!("iplusfree search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("iplusfree search read: {e}")))?;
        let mut results = parse_search_results(&html);
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
