//! Stub SoundCloud, Spotify, Apple Music, Amazon, Beatport, Beatsource
//! modules. Each one is a tiny placeholder that the registry knows about
//! but which fails gracefully for any non-trivial operation. Users can
//! implement these by porting the Python modules; until then, the TUI
//! shows them as unavailable.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

pub fn module_information_stub(
    name: &str,
    modes: ModuleModes,
    netloc: Vec<&str>,
    urls: &[(&str, DownloadType)],
) -> ModuleInformation {
    ModuleInformation {
        service_name: name.to_string(),
        module_supported_modes: modes,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Multi(
            netloc.into_iter().map(|s| s.to_string()).collect(),
        ),
        url_constants: urls.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        test_url: None,
        url_decoding: ManualEnum::Picaro,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn make_stub(info: ModuleInformation) -> Arc<dyn ModuleConstructor> {
    Arc::new(StubConstructor { info })
}

#[derive(Debug)]
struct StubConstructor {
    info: ModuleInformation,
}

impl ModuleConstructor for StubConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(StubModule {
            name: self.info.service_name.clone(),
            controller,
        }))
    }
}

#[derive(Debug)]
struct StubModule {
    name: String,
    controller: ModuleController,
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for StubModule {
    fn name(&self) -> &str {
        &self.name
    }
    async fn get_track_info(
        &self,
        _track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: self.name.clone(),
            ability: "download (not yet implemented in Rust port)".into(),
        })
    }
    async fn get_track_download(
        &self,
        _track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: self.name.clone(),
            ability: "download (not yet implemented in Rust port)".into(),
        })
    }
    async fn get_album_info(
        &self,
        _album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: self.name.clone(),
            ability: "album".into(),
        })
    }
    async fn get_playlist_info(
        &self,
        _playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: self.name.clone(),
            ability: "playlist".into(),
        })
    }
    async fn get_artist_info(
        &self,
        _artist_id: &str,
        _get_credited: bool,
        _artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: self.name.clone(),
            ability: "artist".into(),
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
        Err(Error::ModuleDoesNotSupportAbility {
            module: self.name.clone(),
            ability: "cover".into(),
        })
    }
    async fn get_track_lyrics(
        &self,
        _track_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<LyricsInfo> {
        Ok(LyricsInfo::default())
    }
    async fn search(
        &self,
        _query_type: DownloadType,
        _query: &str,
        _track_info: Option<&TrackInfo>,
        _limit: u32,
    ) -> Result<Vec<SearchResult>> {
        Ok(Vec::new())
    }
}

pub fn register_all_stubs(registry: &picaro_utils::ModuleRegistry) {
    // NOTE: SoundCloud, Spotify, Beatport, Beatsource now have real
    // implementations (soundcloud.rs, spotify.rs, beatport.rs, beatsource.rs).
    // Only Apple Music + Amazon Music remain stubs (Widevine DRM).
    let am = module_information_stub(
        "Apple Music",
        ModuleModes::download,
        vec!["music.apple", "itunes.apple"],
        &[
            ("song", DownloadType::track),
            ("album", DownloadType::album),
            ("playlist", DownloadType::playlist),
            ("artist", DownloadType::artist),
        ],
    );
    register(registry, am.clone(), make_stub(am));
    let az = module_information_stub(
        "Amazon Music",
        ModuleModes::download,
        vec!["music.amazon", "amazonmusic"],
        &[
            ("track", DownloadType::track),
            ("album", DownloadType::album),
            ("playlist", DownloadType::playlist),
            ("artist", DownloadType::artist),
        ],
    );
    register(registry, az.clone(), make_stub(az));
}
