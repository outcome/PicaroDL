//! Deezer (public / signed-out) module.
//!
//! Full Deezer tracks require an account, but the public API is keyless and
//! exposes 30-second preview MP3s. This module searches `api.deezer.com` and
//! downloads those previews, so Deezer works with no login at all (low quality).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "DeezerPreview";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub fn module_information() -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    url_constants.insert("track".to_string(), DownloadType::track);
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("deezer.com".to_string()),
        url_constants,
        test_url: Some("https://www.deezer.com/track/3135556".to_string()),
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
        Ok(Arc::new(Mod))
    }
}

#[derive(Debug)]
struct Mod;

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .unwrap_or_default()
}

async fn get_json(url: &str) -> Option<Value> {
    let resp = client().get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

fn str_field(v: &Value, path: &str) -> String {
    v.pointer(path)
        .and_then(|x| x.as_str())
        .unwrap_or("")
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
        let id = track_id
            .rsplit('/')
            .next()
            .unwrap_or(track_id)
            .split('?')
            .next()
            .unwrap_or(track_id);
        let v = get_json(&format!("https://api.deezer.com/track/{id}"))
            .await
            .ok_or_else(|| Error::Other(format!("deezer: track {id} not found")))?;
        let name = str_field(&v, "/title");
        let artist = str_field(&v, "/artist/name");
        let album = str_field(&v, "/album/title");
        let cover = str_field(&v, "/album/cover_xl");
        let duration = v.get("duration").and_then(|d| d.as_u64()).map(|n| n as u32);
        Ok(TrackInfo {
            name,
            album,
            album_id: String::new(),
            artists: if artist.is_empty() {
                vec![]
            } else {
                vec![artist]
            },
            codec: CodecFlags::MP3,
            cover_url: cover,
            release_year: 0,
            duration,
            id: Some(id.to_string()),
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
        let id = track_id
            .rsplit('/')
            .next()
            .unwrap_or(track_id)
            .split('?')
            .next()
            .unwrap_or(track_id);
        let v = get_json(&format!("https://api.deezer.com/track/{id}"))
            .await
            .ok_or_else(|| Error::Other(format!("deezer: track {id} not found")))?;
        let preview = str_field(&v, "/preview");
        if preview.is_empty() {
            return Err(Error::Other(format!(
                "deezer: no preview available for {id} (signed-out mode only serves 30s previews)"
            )));
        }
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(preview),
            file_url_headers: serde_json::Map::new(),
            temp_file_path: None,
            different_codec: Some(CodecFlags::MP3),
        })
    }

    async fn get_album_info(&self, _id: &str, _d: HashMap<String, Value>) -> Result<AlbumInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.into(),
            ability: "album".into(),
        })
    }

    async fn get_playlist_info(
        &self,
        _id: &str,
        _d: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.into(),
            ability: "playlist".into(),
        })
    }

    async fn get_artist_info(
        &self,
        _id: &str,
        _g: bool,
        _n: Option<&str>,
        _d: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.into(),
            ability: "artist".into(),
        })
    }

    async fn get_track_credits(
        &self,
        _id: &str,
        _d: HashMap<String, Value>,
    ) -> Result<Vec<CreditsInfo>> {
        Ok(vec![])
    }

    async fn get_track_cover(
        &self,
        track_id: &str,
        _c: &CoverOptions,
        data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        if let Some(v) = data.get("__cover__").and_then(|v| v.as_str()) {
            return Ok(CoverInfo {
                url: v.to_string(),
                file_type: ImageFileType::Jpg,
            });
        }
        // Reuse the track's album cover.
        let info = self
            .get_track_info(
                track_id,
                Quality::LOSSLESS,
                &CodecOptions::default(),
                HashMap::new(),
            )
            .await?;
        if info.cover_url.is_empty() {
            return Err(Error::Other(format!("deezer: no cover for {track_id}")));
        }
        Ok(CoverInfo {
            url: info.cover_url,
            file_type: ImageFileType::Jpg,
        })
    }

    async fn search(
        &self,
        _qt: DownloadType,
        query: &str,
        _ti: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let url = format!(
            "https://api.deezer.com/search?q={}&limit={}",
            enc(query),
            limit.clamp(1, 25)
        );
        let Some(v) = get_json(&url).await else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
            for t in arr {
                let id = t
                    .get("id")
                    .and_then(|i| i.as_i64())
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                if id.is_empty() {
                    continue;
                }
                out.push(SearchResult {
                    result_id: id,
                    name: t.get("title").and_then(|s| s.as_str()).map(str::to_string),
                    artists: t
                        .pointer("/artist/name")
                        .and_then(|s| s.as_str())
                        .map(|s| vec![s.to_string()]),
                    duration: t.get("duration").and_then(|d| d.as_u64()).map(|n| n as u32),
                    ..Default::default()
                });
            }
        }
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
