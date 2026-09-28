//! Mp3Zona (mp3zona.net) module.
//!
//! DLE song site. Search via `index.php?do=search&subaction=search&story=<q>`;
//! detail pages embed direct `s1.mp3zona.net` MP3 links in `href` /
//! `data-file` attributes (operator CDN, no referer needed, ID3 + HTTP
//! 206 verified). Legacy extension-less paths 403, so only `.mp3`
//! suffixes are accepted; URLs carry raw spaces/parens and are
//! percent-encoded before download.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use serde_json::Value;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "Mp3Zona";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://mp3zona.net";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("mp3zona.net".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://mp3zona.net/index.php?do=search&subaction=search&story=metallica".to_string()),
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
            '&' => out.push_str("%26"),
            '#' => out.push_str("%23"),
            '"' => out.push_str("%22"),
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
        .map_err(|e| Error::Other(format!("mp3zona fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("mp3zona HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("mp3zona read: {e}")))
}

/// Direct `s1.mp3zona.net` MP3 on a song page (`href` or `data-file`).
fn parse_mp3_link(html: &str) -> Option<String> {
    let re = Regex::new(r#"(?:href|data-file)="(https://s1\.mp3zona\.net/[^"]+?\.mp3)""#).unwrap();
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| enc_path(m.as_str()))
}

/// Detail-page links on a DLE search page (absolute or relative; category
/// segments may contain dashes).
fn parse_search_links(html: &str) -> Vec<String> {
    let re = Regex::new(
        r#"href="((?:https://mp3zona\.net)?/(?:[a-z0-9-]+/)*\d+-[^"]+\.html)""#,
    )
    .unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let raw = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        let url = if raw.starts_with("http") {
            raw.to_string()
        } else {
            format!("{BASE}{raw}")
        };
        if !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

fn name_from_url(url: &str) -> String {
    let seg = url
        .trim_end_matches(".html")
        .rsplit('/')
        .next()
        .unwrap_or("");
    let stripped = seg
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start_matches('-');
    stripped.replace('-', " ")
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
        let mp3 = if track_id.starts_with("https://s1.mp3zona.net/") {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_mp3_link(&html)
                .ok_or_else(|| Error::Other("mp3zona: no direct mp3 on page".into()))?
        };
        Ok(TrackInfo {
            name: name_from_url(track_id),
            codec: CodecFlags::MP3,
            id: Some(mp3),
            tags: Tags {
                track_url: Some(track_id.to_string()),
                ..Default::default()
            },
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
        let mp3 = if track_id.starts_with("https://s1.mp3zona.net/") {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_mp3_link(&html)
                .ok_or_else(|| Error::Other("mp3zona: no direct mp3 on page".into()))?
        };
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(mp3),
            file_url_headers: serde_json::Map::new(),
            temp_file_path: None,
            different_codec: None,
        })
    }

    async fn get_album_info(
        &self,
        _album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.to_string(),
            ability: "album".to_string(),
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
        Err(Error::Other("mp3zona: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        // The resolver passes "artist - title"; DLE phrase-matches the
        // separator and fails, so normalise to plain words.
        let q = query.replace(" - ", " ");
        let url = format!(
            "{BASE}/index.php?do=search&subaction=search&story={}",
            enc(&q)
        );
        let html = fetch_page(&self.client, &url).await?;
        // Every DLE page carries a "newest songs" sidebar; drop results
        // whose slug contains none of the query tokens.
        let tokens: Vec<String> = q
            .to_lowercase()
            .split_whitespace()
            .filter(|t| t.len() >= 3)
            .map(|t| t.to_string())
            .collect();
        let mut out = Vec::new();
        for page in parse_search_links(&html) {
            let hay = page.to_lowercase();
            if !tokens.is_empty() && !tokens.iter().any(|t| hay.contains(t)) {
                continue;
            }
            out.push(SearchResult {
                result_id: page.clone(),
                name: Some(name_from_url(&page)),
                ..Default::default()
            });
        }
        out.truncate(limit.clamp(1, 50) as usize);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
