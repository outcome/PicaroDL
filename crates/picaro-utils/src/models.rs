//! Shared types, enums, and models that mirror OrpheusDL's `utils/models.py`.
//!
//! Anything in here is intended to be 1:1-equivalent with the Python originals -
//! the names of fields, their semantics, and the discriminants match.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use bitflags::bitflags;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

// re-exported so downstream crates can `use picaro_utils::*`
pub use serde::{Deserialize as _, Serialize as _};

// ---------------------------------------------------------------------------
//  Codecs
// ---------------------------------------------------------------------------

bitflags! {
    /// Codecs that any module can produce. Stored as a bitmask so we can do
    /// set operations on what a subscription/quality tier permits.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
    pub struct CodecFlags: u32 {
        const FLAC   = 1 << 0;
        const ALAC   = 1 << 1;
        const WAV    = 1 << 2;
        const AIFF   = 1 << 3;
        const MQA    = 1 << 4;
        const OPUS   = 1 << 5;
        const VORBIS = 1 << 6;
        const MP3    = 1 << 7;
        const AAC    = 1 << 8;
        const HEAAC  = 1 << 9;
        const MHA1   = 1 << 10;
        const MHM1   = 1 << 11;
        const EAC3   = 1 << 12;
        const AC4    = 1 << 13;
        const AC3    = 1 << 14;
        const NONE   = 1 << 15;
    }
}

impl CodecFlags {
    pub fn pretty(self) -> &'static str {
        match self {
            CodecFlags::FLAC => "FLAC",
            CodecFlags::ALAC => "ALAC",
            CodecFlags::WAV => "WAVE",
            CodecFlags::AIFF => "AIFF",
            CodecFlags::MQA => "MQA",
            CodecFlags::OPUS => "Opus",
            CodecFlags::VORBIS => "Vorbis",
            CodecFlags::MP3 => "MP3",
            CodecFlags::AAC => "AAC-LC",
            CodecFlags::HEAAC => "HE-AAC",
            CodecFlags::MHA1 => "MPEG-H 3D Audio",
            CodecFlags::MHM1 => "MPEG-H 3D Audio",
            CodecFlags::EAC3 => "E-AC-3 JOC",
            CodecFlags::AC4 => "AC-4 IMS",
            CodecFlags::AC3 => "Dolby Digital",
            CodecFlags::NONE => "Error",
            _ => "Unknown",
        }
    }

    pub fn is_spatial(self) -> bool {
        self.intersects(
            CodecFlags::MHA1
                | CodecFlags::MHM1
                | CodecFlags::EAC3
                | CodecFlags::AC4
                | CodecFlags::AC3,
        )
    }

    pub fn is_proprietary(self) -> bool {
        self.intersects(
            CodecFlags::MQA
                | CodecFlags::HEAAC
                | CodecFlags::MHA1
                | CodecFlags::MHM1
                | CodecFlags::EAC3
                | CodecFlags::AC4
                | CodecFlags::AC3,
        )
    }

    pub fn is_lossless(self) -> bool {
        self.intersects(
            CodecFlags::FLAC
                | CodecFlags::ALAC
                | CodecFlags::WAV
                | CodecFlags::AIFF
                | CodecFlags::MQA,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    Flac,
    Wav,
    Opus,
    Ogg,
    M4a,
    Mp3,
    Aiff,
    Eac3,
    Ac4,
    Ac3,
    Mp4,
    Webm,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Container::Flac => "flac",
            Container::Wav => "wav",
            Container::Opus => "opus",
            Container::Ogg => "ogg",
            Container::M4a => "m4a",
            Container::Mp3 => "mp3",
            Container::Aiff => "aiff",
            Container::Eac3 => "eac3",
            Container::Ac4 => "ac4",
            Container::Ac3 => "ac3",
            Container::Mp4 => "mp4",
            Container::Webm => "webm",
        }
    }
}

impl fmt::Display for Container {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.extension())
    }
}

// ---------------------------------------------------------------------------
//  Quality / module modes / download type
// ---------------------------------------------------------------------------

bitflags! {
    /// Quality tier requested from a module.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
    pub struct Quality: u32 {
        const MINIMUM  = 1 << 0;
        const LOW      = 1 << 1;
        const MEDIUM   = 1 << 2;
        const HIGH     = 1 << 3;
        const LOSSLESS = 1 << 4;
        const HIFI     = 1 << 5;
        const ATMOS    = 1 << 6;
    }
}

impl Quality {
    pub fn pretty(self) -> &'static str {
        if self.contains(Quality::HIFI) {
            "HiFi"
        } else if self.contains(Quality::ATMOS) {
            "Atmos"
        } else if self.contains(Quality::LOSSLESS) {
            "Lossless"
        } else if self.contains(Quality::HIGH) {
            "High"
        } else if self.contains(Quality::MEDIUM) {
            "Medium"
        } else if self.contains(Quality::LOW) {
            "Low"
        } else if self.contains(Quality::MINIMUM) {
            "Minimum"
        } else {
            "Unknown"
        }
    }
}

bitflags! {
    /// Where a download is sourced from.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
    pub struct DownloadType: u32 {
        const track    = 1 << 0;
        const playlist = 1 << 1;
        const artist   = 1 << 2;
        const album    = 1 << 3;
        const label    = 1 << 4;
    }
}

bitflags! {
    /// What a module can do.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
    pub struct ModuleModes: u32 {
        const download = 1 << 0;
        const playlist = 1 << 1;
        const lyrics   = 1 << 2;
        const credits  = 1 << 3;
        const covers   = 1 << 4;
    }
}

bitflags! {
    /// Module flags - bitset mirroring `utils.models.ModuleFlags`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
    pub struct ModuleFlags: u32 {
        const startup_load      = 1 << 0;
        const hidden            = 1 << 1;
        const enable_jwt_system = 1 << 2;
        const private           = 1 << 3;
        const uses_data         = 1 << 4;
        const needs_cover_resize= 1 << 5;
    }
}

/// Where a downloaded track comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum DownloadSource {
    Url,
    TempFilePath,
    #[default]
    Mpd,
}

// ---------------------------------------------------------------------------
//  Image / cover helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFileType {
    Jpg,
    Png,
    Webp,
}

impl ImageFileType {
    pub fn extension(self) -> &'static str {
        match self {
            ImageFileType::Jpg => "jpg",
            ImageFileType::Png => "png",
            ImageFileType::Webp => "webp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoverCompression {
    Low,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverOptions {
    pub file_type: ImageFileType,
    pub resolution: u32,
    pub compression: CoverCompression,
}

impl Default for CoverOptions {
    fn default() -> Self {
        Self {
            file_type: ImageFileType::Jpg,
            resolution: 1400,
            compression: CoverCompression::High,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoverInfo {
    pub url: String,
    pub file_type: ImageFileType,
}

// ---------------------------------------------------------------------------
//  Module info / settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModuleInformation {
    pub service_name: String,
    pub module_supported_modes: ModuleModes,
    #[serde(default)]
    pub global_settings: IndexMap<String, serde_json::Value>,
    #[serde(default)]
    pub global_storage_variables: Vec<String>,
    #[serde(default)]
    pub session_settings: IndexMap<String, serde_json::Value>,
    #[serde(default)]
    pub session_storage_variables: Vec<String>,
    #[serde(default)]
    pub flags: ModuleFlags,
    #[serde(default)]
    pub netlocation_constant: NetlocConstants,
    #[serde(default)]
    pub url_constants: IndexMap<String, DownloadType>,
    #[serde(default)]
    pub test_url: Option<String>,
    #[serde(default = "default_url_decoding")]
    pub url_decoding: ManualEnum,
    #[serde(default = "default_login_behaviour")]
    pub login_behaviour: ManualEnum,
}

fn default_url_decoding() -> ManualEnum {
    ManualEnum::Picaro
}
fn default_login_behaviour() -> ManualEnum {
    ManualEnum::Picaro
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum NetlocConstants {
    Single(String),
    Multi(Vec<String>),
    #[default]
    Empty,
}

impl NetlocConstants {
    pub fn constants(&self) -> Vec<String> {
        match self {
            NetlocConstants::Single(s) => vec![s.clone()],
            NetlocConstants::Multi(list) => list.clone(),
            NetlocConstants::Empty => vec![],
        }
    }

    pub fn first(&self) -> Option<String> {
        match self {
            NetlocConstants::Single(s) => Some(s.clone()),
            NetlocConstants::Multi(list) => list.first().cloned(),
            NetlocConstants::Empty => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ManualEnum {
    #[default]
    Picaro,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PicaroOptions {
    pub debug_mode: bool,
    pub disable_subscription_check: bool,
    pub quality_tier: Quality,
    pub default_cover_options: CoverOptions,
    #[serde(default = "default_true")]
    pub play_sound_on_finish: bool,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
//  Search results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchResult {
    pub result_id: String,
    pub name: Option<String>,
    pub artists: Option<Vec<String>>,
    pub year: Option<String>,
    pub explicit: Option<bool>,
    pub duration: Option<u32>,
    pub image_url: Option<String>,
    pub preview_url: Option<String>,
    pub additional: Option<Vec<String>>,
    #[serde(default)]
    pub extra_kwargs: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
//  Credits / lyrics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreditsInfo {
    #[serde(rename = "type")]
    pub credit_type: String,
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LyricsInfo {
    pub embedded: Option<String>,
    pub synced: Option<String>,
}

// ---------------------------------------------------------------------------
//  Tags
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tags {
    pub album_artist: Option<String>,
    pub composer: Option<String>,
    pub track_number: Option<u32>,
    pub total_tracks: Option<u32>,
    pub playlist_position: Option<u32>,
    pub copyright: Option<String>,
    pub isrc: Option<String>,
    pub upc: Option<String>,
    pub disc_number: Option<u32>,
    pub total_discs: Option<u32>,
    pub replay_gain: Option<f32>,
    pub replay_peak: Option<f32>,
    pub genres: Option<Vec<String>>,
    pub release_date: Option<String>, // YYYY-MM-DD
    pub description: Option<String>,
    pub comment: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub track_url: Option<String>,
    #[serde(default)]
    pub extra_tags: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
//  Album / Artist / Playlist / Track info
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AlbumInfo {
    pub name: String,
    pub artist: String,
    pub tracks: Vec<TrackRef>,
    pub release_year: i32,
    pub expected_track_count: Option<u32>,
    pub excluded_tracks: Option<Vec<ExcludedTrack>>,
    pub duration: Option<u32>,
    pub explicit: Option<bool>,
    pub artist_id: Option<String>,
    pub id: Option<String>,
    pub quality: Option<String>,
    pub booklet_url: Option<String>,
    pub cover_url: Option<String>,
    pub upc: Option<String>,
    pub cover_type: Option<ImageFileType>,
    pub all_track_cover_jpg_url: Option<String>,
    pub animated_cover_url: Option<String>,
    pub description: Option<String>,
    pub album_artist: Option<MultiArtist>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    #[serde(default)]
    pub track_extra_kwargs: serde_json::Map<String, serde_json::Value>,
}

/// A track entry. Either a bare ID (`String`) or a full `TrackInfo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TrackRef {
    Id(String),
    Full(Box<TrackInfo>),
}

impl Default for TrackRef {
    fn default() -> Self {
        TrackRef::Id(String::new())
    }
}

impl TrackRef {
    pub fn id(&self) -> &str {
        match self {
            TrackRef::Id(id) => id,
            TrackRef::Full(t) => t.id.as_deref().unwrap_or(""),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExcludedTrack {
    pub id: Option<String>,
    pub name: Option<String>,
    pub artists: Option<Vec<String>>,
    pub artist: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub reason: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArtistInfo {
    pub name: String,
    pub artist_id: Option<String>,
    /// Either album ids (as strings), or rich album dicts produced by some modules
    /// (Qobuz/Beatport) - the downloader can pull more fields out of these as
    /// needed.
    pub albums: Vec<serde_json::Value>,
    #[serde(default)]
    pub album_extra_kwargs: serde_json::Map<String, serde_json::Value>,
    pub tracks: Vec<serde_json::Value>,
    #[serde(default)]
    pub track_extra_kwargs: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlaylistInfo {
    pub name: String,
    pub creator: String,
    pub tracks: Vec<TrackRef>,
    pub release_year: i32,
    pub id: Option<String>,
    pub num_tracks: Option<u32>,
    pub num_tracks_from_api: Option<u32>,
    pub excluded_tracks: Option<Vec<ExcludedTrack>>,
    pub duration: Option<u32>,
    pub explicit: Option<bool>,
    pub creator_id: Option<String>,
    pub cover_url: Option<String>,
    pub cover_type: Option<ImageFileType>,
    pub animated_cover_url: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub track_extra_kwargs: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrackInfo {
    pub name: String,
    pub album: String,
    pub album_id: String,
    pub artists: Vec<String>,
    pub tags: Tags,
    pub codec: CodecFlags,
    pub cover_url: String,
    pub release_year: i32,
    pub duration: Option<u32>,
    pub explicit: Option<bool>,
    pub artist_id: Option<String>,
    pub id: Option<String>,
    pub gid_hex: Option<String>,
    pub animated_cover_url: Option<String>,
    pub description: Option<String>,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<f32>,
    pub bitrate: Option<u32>,
    #[serde(default)]
    pub download_extra_kwargs: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub cover_extra_kwargs: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub credits_extra_kwargs: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub lyrics_extra_kwargs: serde_json::Map<String, serde_json::Value>,
    pub lyrics: Option<String>,
    pub synced_lyrics: Option<String>,
    #[serde(default)]
    pub credits_list: Vec<CreditsInfo>,
    pub error: Option<String>,
    pub preview_url: Option<String>,
    pub additional: Option<String>,
}

impl TrackInfo {
    pub fn first_artist(&self) -> String {
        self.artists
            .first()
            .cloned()
            .unwrap_or_else(|| "Unknown Artist".to_string())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrackDownloadInfo {
    pub download_type: DownloadSource,
    pub file_url: Option<String>,
    #[serde(default)]
    pub file_url_headers: serde_json::Map<String, serde_json::Value>,
    pub temp_file_path: Option<PathBuf>,
    pub different_codec: Option<CodecFlags>,
}

// ---------------------------------------------------------------------------
//  Media identification (for CLI / TUI input)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaIdentification {
    pub media_type: DownloadType,
    pub media_id: String,
    #[serde(default)]
    pub extra_kwargs: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
//  Multi-artist (string or list)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MultiArtist {
    Single(String),
    Multi(Vec<String>),
}

impl MultiArtist {
    pub fn first(&self) -> String {
        match self {
            MultiArtist::Single(s) => s.clone(),
            MultiArtist::Multi(v) => v.first().cloned().unwrap_or_default(),
        }
    }

    pub fn joined(&self, sep: &str) -> String {
        match self {
            MultiArtist::Single(s) => s.clone(),
            MultiArtist::Multi(v) => v.join(sep),
        }
    }

    pub fn to_vec(&self) -> Vec<String> {
        match self {
            MultiArtist::Single(s) => vec![s.clone()],
            MultiArtist::Multi(v) => v.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
//  Module controller (passed to each module on construction)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ModuleController {
    pub module_settings: serde_json::Map<String, serde_json::Value>,
    pub data_folder: PathBuf,
    pub temporary_settings_controller: TemporarySettingsController,
    pub picaro_options: PicaroOptions,
    pub get_current_timestamp: fn() -> i64,
    pub progress_bar_enabled: bool,
    pub debug_mode: bool,
}

#[derive(Debug, Clone)]
pub struct TemporarySettingsController {
    pub module: String,
    pub session_path: PathBuf,
}

impl TemporarySettingsController {
    pub fn read(
        &self,
        setting: &str,
        setting_type: TempSettingType,
    ) -> Result<Option<serde_json::Value>, crate::error::Error> {
        crate::settings::read_temporary_setting(
            &self.session_path,
            &self.module,
            setting,
            setting_type,
        )
    }

    pub fn set(
        &self,
        setting: &str,
        value: serde_json::Value,
        setting_type: TempSettingType,
    ) -> Result<(), crate::error::Error> {
        crate::settings::set_temporary_setting(
            &self.session_path,
            &self.module,
            setting,
            value,
            setting_type,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempSettingType {
    Custom,
    Global,
    Jwt,
}
