//! Free Music Archive (freemusicarchive.org) module.
//!
//! Search via `/search?adv=1&quicksearch=tracks&searchf=<q>`; track pages
//! live at `/music/<Artist>/<Album>/<Track>/` and embed a `"fileUrl"`
//! JSON field pointing at a direct `files.freemusicarchive.org` MP3
//! (plain GET, no JS/captcha, ID3 + HTTP 206 verified).

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

const SERVICE: &str = "FMA";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://freemusicarchive.org";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("freemusicarchive.org".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://freemusicarchive.org/search?adv=1&quicksearch=tracks&searchf=test".to_string()),
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

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("fma fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("fma HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("fma read: {e}")))
}

fn decode_entities(s: &str) -> String {
    let amp = format!("{}amp;", '&');
    let quot = format!("{}quot;", '&');
    s.replace("&#039;", "'")
        .replace("&#8217;", "'")
        .replace(&quot, "\"")
        .replace(&amp, "&")
        .trim()
        .to_string()
}

/// Track-page anchors from search: `/music/<Artist>/<Album>/<Track>/`
/// (artist-only and album links from sidebars are skipped: they carry
/// fewer than 3 path segments after `/music/`).
fn parse_track_links(html: &str) -> Vec<(String, String, String)> {
    let re = Regex::new(r#"href="(https://freemusicarchive\.org/music/[^"]+?)/""#).unwrap();
    let mut out: Vec<(String, String, String)> = Vec::new();
    for cap in re.captures_iter(html) {
        let url = cap.get(1).map(|m| m.as_str()).unwrap_or_default().to_string();
        let rest = url
            .trim_start_matches("https://freemusicarchive.org/music/")
            .trim_matches('/');
        let segs: Vec<&str> = rest.split('/').collect();
        if segs.len() != 3 {
            continue;
        }
        let artist = segs[0].replace('_', " ");
        let name = segs[2].replace('_', " ");
        if out.iter().any(|(u, _, _)| *u == url) {
            continue;
        }
        out.push((format!("{url}/"), name, artist));
    }
    out
}

/// `"fileUrl":"https:\/\/files.freemusicarchive.org\/...mp3"` on a track
/// page (JSON-escaped slashes).
fn parse_file_url(html: &str) -> Option<String> {
    let re = Regex::new(r#""fileUrl":"([^"]+)""#).unwrap();
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().replace("\\/", "/"))
}

fn name_from_track_id(track_id: &str) -> String {
    track_id
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .replace('_', " ")
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
        let mp3 = if track_id.starts_with("https://files.freemusicarchive.org/") {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_file_url(&html).ok_or_else(|| {
                Error::Other("fma: no fileUrl on track page (stream-only?)".into())
            })?
        };
        Ok(TrackInfo {
            name: name_from_track_id(track_id),
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
        let mp3 = if track_id.starts_with("https://files.freemusicarchive.org/") {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_file_url(&html).ok_or_else(|| {
                Error::Other("fma: no fileUrl on track page (stream-only?)".into())
            })?
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
        Err(Error::Other("fma: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let url = format!("{BASE}/search?quicksearch=tracks&q={}", enc(query));
        let html = fetch_page(&self.client, &url).await?;
        let mut out = Vec::new();
        for (track_url, name, artist) in parse_track_links(&html) {
            let name = decode_entities(&name);
            let artist = decode_entities(&artist);
            if name.is_empty() {
                continue;
            }
            out.push(SearchResult {
                result_id: track_url,
                name: Some(name),
                artists: Some(vec![artist]),
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
