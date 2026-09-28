//! Certified Mixtapez (certifiedmixtapez.com) module.
//!
//! Search is a JSON POST to `/Main/GetSearchResults` with
//! `{"SearchOptionType":1,"Title":"<q>","ItemsPerPage":12}` (no API key
//! needed). Details live at `/Main/Details?refId=<refId>` and embed a
//! `var model = {...}` JS payload whose `tracks[].trackSource` fields are
//! direct DigitalOcean Spaces CDN MP3s (ID3 + HTTP 206 verified). The
//! CDN URLs contain raw spaces, so they are percent-encoded before
//! download.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use serde_json::{json, Value};

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "CertifiedMixtapez";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://certifiedmixtapez.com";
const CDN: &str = "cmtz.nyc3.cdn.digitaloceanspaces.com";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("certifiedmixtapez.com".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://certifiedmixtapez.com/Main/Details?refId=13391ede".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(Ctor)
}

#[derive(Debug)]
struct Ctor;

impl ModuleConstructor for Ctor {
    fn construct(&self, _c: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(Mod {
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct Mod {
    client: reqwest::Client,
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn enc_path(url: &str) -> String {
    let mut out = String::new();
    for c in url.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '\'' => out.push_str("%27"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '"' => out.push_str("%22"),
            '\\' => {}
            _ => out.push(c),
        }
    }
    out
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("certifiedmixtapez fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "certifiedmixtapez HTTP {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("certifiedmixtapez read: {e}")))
}

/// `(title, mp3 url)` pairs from the `var model = {...}` payload's
/// tracks array (`trackTitle` precedes `trackSource` in each object).
fn parse_tracks(html: &str) -> Vec<(String, String)> {
    let re = Regex::new(r#"(?s)"trackTitle":"([^"]*)".*?"trackSource":"(https://[^"]+\.mp3)""#)
        .unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let title = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
        let url = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
        if url.is_empty() {
            continue;
        }
        out.push((title, enc_path(&url)));
    }
    out
}

fn parse_album_title(html: &str) -> (String, String) {
    let title_re = Regex::new(r#"(?s)<meta property="og:title" content="([^"]+)""#).unwrap();
    let title = title_re
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let artists_re = Regex::new(r#"(?s)<meta property="og:description" content="([^"]*)""#).unwrap();
    let artist = artists_re
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let (artist, name) = match title.find(" - ") {
        Some(i) => (
            title[..i].trim().to_string(),
            title[i + 3..].trim().to_string(),
        ),
        None => (artist, title),
    };
    (artist, name)
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for Mod {
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
        _data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        let url = if track_id.contains(CDN) {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_tracks(&html)
                .first()
                .map(|(_, u)| u.clone())
                .ok_or_else(|| Error::Other("certifiedmixtapez: no tracks".into()))?
        };
        let name = track_id
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("Mixtape track")
            .trim_end_matches(".mp3")
            .replace("%20", " ")
            .replace("%27", "'")
            .replace("%28", "(")
            .replace("%29", ")")
            .replace("%22", "\"");
        Ok(TrackInfo {
            name,
            codec: CodecFlags::MP3,
            id: Some(url),
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
        let url = if track_id.contains(CDN) {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_tracks(&html)
                .first()
                .map(|(_, u)| u.clone())
                .ok_or_else(|| Error::Other("certifiedmixtapez: no tracks".into()))?
        };
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(url),
            file_url_headers: serde_json::Map::new(),
            temp_file_path: None,
            different_codec: None,
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let url = if album_id.starts_with(BASE) {
            album_id.to_string()
        } else {
            format!("{BASE}/Main/Details?refId={}", enc(album_id))
        };
        let html = fetch_page(&self.client, &url).await?;
        let tracks = parse_tracks(&html);
        if tracks.is_empty() {
            return Err(Error::Other("certifiedmixtapez: no tracks".into()));
        }
        let (artist, name) = parse_album_title(&html);
        Ok(AlbumInfo {
            name: if name.is_empty() {
                "Mixtape".to_string()
            } else {
                name
            },
            artist: if artist.is_empty() {
                "Unknown Artist".to_string()
            } else {
                artist
            },
            tracks: tracks
                .into_iter()
                .map(|(name, url)| {
                    TrackRef::Full(Box::new(TrackInfo {
                        name,
                        codec: CodecFlags::MP3,
                        id: Some(url),
                        ..Default::default()
                    }))
                })
                .collect(),
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
        _track_id: &str,
        _cover: &CoverOptions,
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        Err(Error::Other("certifiedmixtapez: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let body = json!({
            "SearchOptionType": 1,
            "Title": query,
            "ItemsPerPage": 24,
        });
        let resp = self
            .client
            .post(format!("{BASE}/Main/GetSearchResults"))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("certifiedmixtapez search: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "certifiedmixtapez HTTP {}",
                resp.status()
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("certifiedmixtapez json: {e}")))?;
        let mut out = Vec::new();
        if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
            for it in data {
                let ref_id = it.get("refId").and_then(|r| r.as_str()).unwrap_or("");
                let title = it.get("title").and_then(|t| t.as_str()).unwrap_or("");
                let artists = it
                    .get("artists")
                    .and_then(|a| a.as_str())
                    .unwrap_or("")
                    .to_string();
                if ref_id.is_empty() || title.is_empty() {
                    continue;
                }
                out.push(SearchResult {
                    result_id: ref_id.to_string(),
                    name: Some(title.to_string()),
                    artists: if artists.is_empty() {
                        None
                    } else {
                        Some(vec![artists])
                    },
                    ..Default::default()
                });
            }
        }
        out.truncate(limit.clamp(1, 50) as usize);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
