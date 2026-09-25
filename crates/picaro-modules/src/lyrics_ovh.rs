//! Lyrics.ovh module - free, keyless lyrics API (https://lyrics.ovh).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "LyricsOvh";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::lyrics,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("lyrics.ovh".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: None,
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
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Extract (artist, title) from the downloader-provided data, or "A - T" id.
pub(crate) fn artist_title(
    track_id: &str,
    data: &HashMap<String, Value>,
) -> Option<(String, String)> {
    let artist = data
        .get("__artist__")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let title = data
        .get("__track_name__")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !artist.is_empty() && !title.is_empty() {
        return Some((artist, title));
    }
    if let Some(idx) = track_id.find(" - ") {
        let a = track_id[..idx].trim().to_string();
        let t = track_id[idx + 3..].trim().to_string();
        if !a.is_empty() && !t.is_empty() {
            return Some((a, t));
        }
    }
    None
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for Mod {
    fn name(&self) -> &str {
        SERVICE
    }

    async fn get_track_info(
        &self,
        _id: &str,
        _q: Quality,
        _c: &CodecOptions,
        _d: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.into(),
            ability: "track_info".into(),
        })
    }

    async fn get_track_download(
        &self,
        _id: &str,
        _q: Quality,
        _c: &CodecOptions,
        _d: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.into(),
            ability: "download".into(),
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
        _id: &str,
        _c: &CoverOptions,
        _d: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.into(),
            ability: "cover".into(),
        })
    }

    async fn get_track_lyrics(
        &self,
        track_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<LyricsInfo> {
        let Some((artist, title)) = artist_title(track_id, &data) else {
            return Ok(LyricsInfo::default());
        };
        let url = format!("https://api.lyrics.ovh/v1/{}/{}", enc(&artist), enc(&title));
        let client = reqwest::Client::new();
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            return Ok(LyricsInfo::default());
        }
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        let embedded = v
            .get("lyrics")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(LyricsInfo {
            embedded,
            synced: None,
        })
    }

    async fn search(
        &self,
        _qt: DownloadType,
        _q: &str,
        _ti: Option<&TrackInfo>,
        _l: u32,
    ) -> Result<Vec<SearchResult>> {
        Ok(vec![])
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
