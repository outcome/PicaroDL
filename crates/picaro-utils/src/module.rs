//! The `ModuleInterface` trait that every service module implements.
//!
//! 1:1 with `modules/*/interface.py` in OrpheusDL.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Result;
use crate::models::*;

/// Options passed to a module when it should resolve a track for download.
/// Mirrors the `CodecOptions` dataclass in OrpheusDL.
#[derive(Debug, Clone, Default)]
pub struct CodecOptions {
    pub spatial_codecs: bool,
    pub proprietary_codecs: bool,
}

/// The full set of methods a module may implement. Each method is optional,
/// but `get_track_info`, `get_track_download`, `get_album_info`,
/// `get_playlist_info`, `get_artist_info`, and `search` are all mandatory
/// when the corresponding `ModuleModes` flag is set.
#[async_trait]
pub trait ModuleInterface: Send + Sync {
    fn name(&self) -> &str;

    /// Called when the loader is asked to load the module. Anything that
    /// needs to happen eagerly (background workers, logins) goes here.
    fn init(&self) -> Result<()> {
        Ok(())
    }

    /// Returns true if the module is currently authenticated. If it returns
    /// false but the user is mid-OAuth, the loader may prompt to retry.
    fn is_authenticated(&self) -> bool {
        false
    }

    /// Optional - called by the downloader right before a download starts.
    /// Lets a module run its own login flow / ensure_can_download.
    async fn ensure_can_download(&self) -> Result<()> {
        Ok(())
    }

    /// Trigger the module's own login flow (email/pass, OAuth, etc.) and
    /// persist the result. Default is a no-op.
    async fn login(&self, _email: &str, _password: &str) -> Result<()> {
        Ok(())
    }

    /// Drop the active session.
    async fn logout(&self) -> Result<()> {
        Ok(())
    }

    /// Get a track's metadata for the given quality / codec profile.
    async fn get_track_info(
        &self,
        _track_id: &str,
        _quality: Quality,
        _codec_options: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackInfo>;

    /// Get a download descriptor for a track. The downloader will use
    /// `temp_file_path` (decrypting the stream if needed) or `file_url`.
    async fn get_track_download(
        &self,
        _track_id: &str,
        _quality: Quality,
        _codec_options: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo>;

    async fn get_album_info(
        &self,
        _album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo>;

    async fn get_playlist_info(
        &self,
        _playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo>;

    async fn get_artist_info(
        &self,
        _artist_id: &str,
        _get_credited_albums: bool,
        _artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo>;

    async fn get_label_info(
        &self,
        _label_id: &str,
        _get_credited_albums: bool,
    ) -> Result<ArtistInfo> {
        Err(crate::error::Error::ModuleDoesNotSupportAbility {
            module: self.name().to_string(),
            ability: "label".to_string(),
        })
    }

    async fn get_track_credits(
        &self,
        _track_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<Vec<CreditsInfo>>;

    async fn get_track_cover(
        &self,
        _track_id: &str,
        _cover_options: &CoverOptions,
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo>;

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
    ) -> Result<Vec<SearchResult>>;

    /// Custom URL parser; default implementation falls back to the generic
    /// `url_decode::default_decode_url` which uses `url_constants`.
    fn custom_url_parse(&self, _url: &str) -> Result<Option<MediaIdentification>> {
        Ok(None)
    }

    /// Optional preview stream URL for non-authenticated listeners.
    async fn get_preview_stream_url(&self, _track_id: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

pub type ModuleInterfacePtr = Arc<dyn ModuleInterface>;
