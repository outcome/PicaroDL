//! DarkTorrent (darktorrent.org) module.
//!
//! DLE torrent index. Search via
//! `index.php?do=search&subaction=search&story=<q>`; detail pages link
//! same-host `.torrent` metainfo through DLE attachment endpoints
//! (`index.php?do=download&id=<n>`, bencode `d8:announce` verified, no
//! cookie/referer needed). The attachment URL is handed to the
//! downloader's BitTorrent engine, which fetches the metainfo bytes and
//! downloads only the audio files.
//!
//! The torrents announce to `bt3.t-ru.org` (HTTP tracker), so peer
//! discovery works even when UDP/DHT is blocked.
//!
//! Torrent support is OFF by default; the resolver only queries this
//! module when `[torrent] enabled=true`.

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

const SERVICE: &str = "DarkTorrent";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://darktorrent.org";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("darktorrent.org".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://darktorrent.org/index.php?do=search&subaction=search&story=metallica".to_string()),
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
        .map_err(|e| Error::Other(format!("darktorrent fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "darktorrent HTTP {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("darktorrent read: {e}")))
}

/// DLE attachment (`.torrent` metainfo) links on a detail page.
fn parse_torrent_link(html: &str) -> Option<String> {
    let re = Regex::new(r#"href="(https://darktorrent\.org/index\.php\?do=download&id=\d+)""#)
        .unwrap();
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Detail-page links on a DLE search page (discography/music/compilations
/// categories).
fn parse_search_links(html: &str) -> Vec<String> {
    let re = Regex::new(
        r#"href="(https://darktorrent\.org/(?:discography|music|compilations)/\d+-[^"]+\.html)""#,
    )
    .unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let url = cap.get(1).map(|m| m.as_str()).unwrap_or_default().to_string();
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
        if !track_id.starts_with("https://darktorrent.org/") {
            return Err(Error::Other(format!("darktorrent: bad id '{track_id}'")));
        }
        Ok(TrackInfo {
            name: name_from_url(track_id),
            codec: CodecFlags::FLAC,
            id: Some(track_id.to_string()),
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
        // Attachment endpoint: hand straight to the BitTorrent engine.
        if track_id.contains("do=download") {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::Url,
                file_url: Some(track_id.to_string()),
                file_url_headers: serde_json::Map::new(),
                temp_file_path: None,
                different_codec: None,
            });
        }
        let html = fetch_page(&self.client, track_id).await?;
        let torrent = parse_torrent_link(&html)
            .ok_or_else(|| Error::Other("darktorrent: no torrent attachment on page".into()))?;
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(torrent),
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
        if !album_id.starts_with("https://darktorrent.org/") {
            return Err(Error::Other(format!("darktorrent: bad id '{album_id}'")));
        }
        Ok(AlbumInfo {
            name: name_from_url(album_id),
            artist: "DarkTorrent".to_string(),
            tracks: vec![TrackRef::Id(album_id.to_string())],
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
        _track_id: &str,
        _cover: &CoverOptions,
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        Err(Error::Other("darktorrent: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let url = format!(
            "{BASE}/index.php?do=search&subaction=search&story={}",
            enc(query)
        );
        let html = fetch_page(&self.client, &url).await?;
        let mut out = Vec::new();
        for page in parse_search_links(&html) {
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
