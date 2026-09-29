//! The Pirate Bay torrent index module (via the keyless `apibay.org` JSON API).
//!
//! Each search hit becomes a magnet link that the downloader's BitTorrent
//! engine consumes. `get_album_info` exposes a release as a one-track album so
//! the resolver's album flow downloads the whole torrent and picks the
//! requested file out of the result.
//!
//! Torrent support is OFF by default (`[torrent] enabled=false`); searching
//! still works and yields results the engine will refuse until enabled.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "PirateBay";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) PicaroDL/0.1";
const API: &str = "https://apibay.org/q.php";
const TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
];

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("thepiratebay.org".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://thepiratebay.org/search.php?q=radiohead".to_string()),
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

fn magnet_for(hash: &str, name: &str) -> String {
    let mut m = format!("magnet:?xt=urn:btih:{hash}&dn={}", enc(name));
    for t in TRACKERS {
        m.push_str("&tr=");
        m.push_str(&enc(t));
    }
    m
}

fn name_of(magnet: &str) -> String {
    let raw = magnet
        .split("&dn=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .and_then(|s| {
            // dn is percent-encoded by magnet_for; decode it so album
            // folders aren't named "Radiohead%20%2D%20In%20Rainbows".
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .ok()
                .map(|s| s.replace('+', " "))
        })
        .unwrap_or_else(|| "Pirate Bay release".to_string());
    raw.trim().to_string()
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
        if !track_id.starts_with("magnet:") {
            return Err(Error::Other(format!("piratebay: bad id '{track_id}'")));
        }
        Ok(TrackInfo {
            name: name_of(track_id),
            codec: CodecFlags::FLAC,
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
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(track_id.to_string()),
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
        Ok(AlbumInfo {
            name: name_of(album_id),
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
        Err(Error::Other("piratebay: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let url = format!("{API}?q={}&cat=0", enc(query));
        let v = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Other(format!("piratebay search: {e}")))?
            .json::<Value>()
            .await
            .map_err(|e| Error::Other(format!("piratebay json: {e}")))?;

        let arr = match v.as_array() {
            Some(a) => a.clone(),
            None => return Ok(Vec::new()),
        };

        let mut out = Vec::new();
        for it in arr {
            let hash = it.get("info_hash").and_then(|x| x.as_str()).unwrap_or("");
            let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("");
            if hash.is_empty() || hash == "0000000000000000000000000000000000000000" || name.is_empty() {
                continue;
            }
            let seeders: u64 = it
                .get("seeders")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let size: u64 = it
                .get("size")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let size_gb = size as f64 / (1024.0 * 1024.0 * 1024.0);
            let mut extra = serde_json::Map::new();
            extra.insert("seeders".to_string(), Value::from(seeders));
            extra.insert("size_gb".to_string(), Value::from(size_gb));
            out.push(SearchResult {
                result_id: magnet_for(hash, name),
                name: Some(name.to_string()),
                additional: Some(vec![format!("{seeders} seeders, {size_gb:.2} GB")]),
                extra_kwargs: extra,
                ..Default::default()
            });
        }
        out.sort_by(|a, b| {
            let sa = a
                .extra_kwargs
                .get("seeders")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let sb = b
                .extra_kwargs
                .get("seeders")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            sb.cmp(&sa)
        });
        out.truncate(limit.clamp(1, 100) as usize);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
