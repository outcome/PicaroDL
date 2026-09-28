//! 1Trance (1trance.org) module.
//!
//! Drupal trance netlabel index. Search uses the exposed-filter endpoint
//! `/f?s=<query>&t=&page=1`; releases live at `/node/<id>/<slug>`. MP3 320
//! releases embed a direct `tune.skin/<token>/<file>.mp3` link (plain GET,
//! no JS, verified audio/mpeg + ID3); FLAC-only releases route to rapidgator
//! and are skipped.

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

const SERVICE: &str = "OneTrance";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://1trance.org";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("1trance.org".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://1trance.org/f?s=psytrance&t=&page=1".to_string()),
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

fn title_from_slug(slug: &str) -> String {
    slug.trim_end_matches("-int")
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("onetrance fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("onetrance HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("onetrance read: {e}")))
}

fn parse_node_links(html: &str) -> Vec<String> {
    let re = Regex::new(r#"href="(/node/\d+/[^"]+)""#).unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        if let Some(m) = cap.get(1) {
            let url = format!("{BASE}{}", m.as_str());
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    out
}

/// Direct MP3 link (tune.skin) on a release page. FLAC-only releases embed
/// only a rapidgator link and return None here.
fn parse_mp3_link(html: &str) -> Option<String> {
    let re =
        Regex::new(r#"href="(https://tune\.skin/[A-Za-z0-9]+/[^"]+\.mp3)""#).unwrap();
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn parse_title(html: &str) -> String {
    let t = Regex::new(r"(?is)<title>(.*?)</title>").unwrap();
    let raw = t
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    raw.split('|')
        .next()
        .unwrap_or(&raw)
        .trim()
        .to_string()
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
        let html = fetch_page(&self.client, track_id).await?;
        let Some(mp3) = parse_mp3_link(&html) else {
            return Err(Error::Other(
                "onetrance: FLAC-only release (rapidgator)".into(),
            ));
        };
        let title = parse_title(&html);
        let slug = track_id.rsplit('/').next().unwrap_or(track_id);
        Ok(TrackInfo {
            name: if title.is_empty() {
                title_from_slug(slug)
            } else {
                title
            },
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
        if track_id.starts_with("https://tune.skin/") {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::Url,
                file_url: Some(track_id.to_string()),
                file_url_headers: serde_json::Map::new(),
                temp_file_path: None,
                different_codec: None,
            });
        }
        let html = fetch_page(&self.client, track_id).await?;
        let Some(mp3) = parse_mp3_link(&html) else {
            return Err(Error::Other(
                "onetrance: FLAC-only release (rapidgator)".into(),
            ));
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
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let html = fetch_page(&self.client, album_id).await?;
        let Some(mp3) = parse_mp3_link(&html) else {
            return Err(Error::Other(
                "onetrance: FLAC-only release (rapidgator)".into(),
            ));
        };
        let title = parse_title(&html);
        let slug = album_id.rsplit('/').next().unwrap_or(album_id);
        Ok(AlbumInfo {
            name: if title.is_empty() {
                title_from_slug(slug)
            } else {
                title
            },
            tracks: vec![TrackRef::Id(mp3)],
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
        Err(Error::Other("onetrance: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let url = format!("{BASE}/f?s={}&t=&page=1", enc(query));
        let html = fetch_page(&self.client, &url).await?;
        let mut out = Vec::new();
        for node in parse_node_links(&html) {
            let slug = node.rsplit('/').next().unwrap_or("").to_string();
            out.push(SearchResult {
                result_id: node,
                name: Some(title_from_slug(&slug)),
                ..Default::default()
            });
        }
        out.truncate(limit.clamp(1, 50) as usize);
        Ok(out)
    }
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
