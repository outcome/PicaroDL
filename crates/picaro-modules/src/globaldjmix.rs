//! GlobalDJMix (globaldjmix.com) site module.
//!
//! Search uses the site's jQuery-Autocomplete JSON endpoint (verified):
//!   GET https://globaldjmix.com/search-json-reaponse&b_type=mixes&query=<q>
//! which returns `{"suggestions":[{ "value", "artist_name", "mix_name",
//! "page_name", "link", "quality", "size", "duration", "image_name", ... }]}`.
//!
//! Each suggestion's `link` is a URL-encoded **direct** MP3 URL on
//! `files.globaldjmix.com`, so downloads do not need a file-host resolver.

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
const REFERER: &str = "https://globaldjmix.com/";
const BASE: &str = "https://globaldjmix.com";
const SERVICE: &str = "GlobalDJMix";

pub fn module_information() -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    url_constants.insert("mixes".to_string(), DownloadType::album);
    url_constants.insert("track".to_string(), DownloadType::track);
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Multi(vec![
            "globaldjmix.com".to_string(),
            "files.globaldjmix.com".to_string(),
        ]),
        url_constants,
        test_url: Some("https://globaldjmix.com/".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(GlobalDjmixConstructor)
}

#[derive(Debug)]
struct GlobalDjmixConstructor;

impl ModuleConstructor for GlobalDjmixConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(GlobalDjmixModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct GlobalDjmixModule {
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

fn track_name_from_url(url: &str) -> String {
    let file = url
        .split('?')
        .next()
        .unwrap_or(url)
        .split('/')
        .next_back()
        .unwrap_or(url);
    let decoded = percent_decode_str(file).decode_utf8_lossy().to_string();
    decoded
        .rsplit_once('.')
        .map(|(n, _)| n.to_string())
        .unwrap_or(decoded)
        .replace('+', " ")
        .trim()
        .to_string()
}

fn suggestion_to_result(s: &Value) -> Option<SearchResult> {
    let link = s.get("link").and_then(|x| x.as_str()).unwrap_or("");
    if link.is_empty() {
        return None;
    }
    // The JSON `link` is percent-encoded, but literal `+` is used for spaces in
    // the path (`Nicole+Moudaber+-+In+The+MOOD+...mp3`). Decoding leaves the
    // `+` untouched, and the CDN stores the real filenames with spaces, so a
    // literal `+` path 404s. Re-encode spaces in the path as `%20` (query
    // strings are left alone).
    let decoded = percent_decode_str(link).decode_utf8_lossy().to_string();
    let url = match decoded.split_once('?') {
        Some((path, query)) => format!("{}?{}", path.replace('+', "%20"), query),
        None => decoded.replace('+', "%20"),
    };
    let name = s
        .get("value")
        .and_then(|x| x.as_str())
        .or_else(|| s.get("mix_name").and_then(|x| x.as_str()))
        .map(decode_entities)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| track_name_from_url(&url));
    let artist = s
        .get("artist_name")
        .and_then(|x| x.as_str())
        .map(decode_entities)
        .filter(|v| !v.is_empty());
    Some(SearchResult {
        result_id: url,
        name: Some(name),
        artists: artist.map(|a| vec![a]),
        ..Default::default()
    })
}

fn collect_page_tracks(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let direct = Regex::new(r#"https?://files\.globaldjmix\.com/[^"'\s]+\.mp3"#).unwrap();
    for m in direct.find_iter(html) {
        let url = m.as_str().to_string();
        if !out.contains(&url) {
            out.push(url);
        }
    }
    let host_re =
        Regex::new(r#"https?://[^"'\s]*(?:zippy|mediafire|rapidgator|katfile|1fichier)[^"'\s]*"#)
            .unwrap();
    for m in host_re.find_iter(html) {
        let url = m.as_str().to_string();
        if !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for GlobalDjmixModule {
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
        let derived = track_name_from_url(track_id);
        let name = data
            .get("__track_name__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or(derived);
        let artist = data
            .get("__artist__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        let cover = data
            .get("__cover__")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(TrackInfo {
            name,
            album: data
                .get("__album__")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            album_id: String::new(),
            artists: artist.map(|a| vec![a]).unwrap_or_default(),
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
                "globaldjmix: expected direct MP3 URL, got {track_id}"
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
        if album_id.starts_with("http") && album_id.ends_with(".mp3") {
            let name = track_name_from_url(album_id);
            return Ok(AlbumInfo {
                name,
                artist: String::new(),
                tracks: vec![TrackRef::Id(album_id.to_string())],
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
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("globaldjmix album fetch: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "globaldjmix album HTTP {}",
                resp.status()
            )));
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("globaldjmix album read: {e}")))?;
        let tracks = collect_page_tracks(&html);
        if tracks.is_empty() {
            return Err(Error::Other(format!(
                "globaldjmix: no downloads found at {url}"
            )));
        }
        let title = Regex::new(r#"(?is)<meta property="og:title" content="([^"]+)""#)
            .unwrap()
            .captures(&html)
            .and_then(|c| c.get(1).map(|m| decode_entities(m.as_str())))
            .unwrap_or_else(|| album_id.to_string());
        Ok(AlbumInfo {
            name: title,
            artist: String::new(),
            tracks: tracks.into_iter().map(TrackRef::Id).collect(),
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
        data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        if let Some(v) = data.get("__cover__").and_then(|v| v.as_str()) {
            return Ok(CoverInfo {
                url: v.to_string(),
                file_type: ImageFileType::Jpg,
            });
        }
        Err(Error::Other(format!(
            "globaldjmix: no cover for track {track_id}"
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
        let url = format!("{BASE}/search-json-reaponse&b_type=mixes&query={encoded}");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("globaldjmix search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let root: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("globaldjmix search json: {e}")))?;
        let mut out: Vec<SearchResult> = Vec::new();
        if let Some(arr) = root.get("suggestions").and_then(|s| s.as_array()) {
            for s in arr {
                if let Some(r) = suggestion_to_result(s) {
                    if out.iter().any(|x| x.result_id == r.result_id) {
                        continue;
                    }
                    out.push(r);
                }
                if out.len() >= limit as usize {
                    break;
                }
            }
        }
        out.truncate(limit as usize);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
