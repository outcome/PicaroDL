use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const REFERER: &str = "https://archive.org/";
const SERVICE: &str = "ArchiveOrg";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("archive.org".to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("details".to_string(), DownloadType::album);
            m.insert("download".to_string(), DownloadType::track);
            m
        },
        test_url: Some("https://archive.org/details/audio".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(ArchiveOrgConstructor)
}

#[derive(Debug)]
struct ArchiveOrgConstructor;

impl ModuleConstructor for ArchiveOrgConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(ArchiveOrgModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct ArchiveOrgModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn first_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => a.first().and_then(first_string),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn album_meta(data: &HashMap<String, Value>) -> (String, String, String, Option<i32>) {
    match data.get("__album_meta__") {
        Some(v) => {
            let a = v
                .get("album")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let ar = v
                .get("artist")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let c = v
                .get("cover")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let y = v.get("year").and_then(|x| x.as_i64()).map(|i| i as i32);
            (a, ar, c, y)
        }
        None => (String::new(), String::new(), String::new(), None),
    }
}

fn identifier_from(album_id: &str) -> String {
    let trimmed = album_id.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for ArchiveOrgModule {
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
        data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        let (album, artist, cover, year) = album_meta(&data);
        let derived = track_id
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(track_id)
            .to_string();
        let name = data
            .get("__track_name__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| derived.clone());
        let codec = if derived.to_lowercase().ends_with(".mp3") {
            CodecFlags::MP3
        } else {
            CodecFlags::FLAC
        };
        Ok(TrackInfo {
            name,
            album,
            album_id: String::new(),
            artists: if artist.is_empty() {
                vec![]
            } else {
                vec![artist]
            },
            tags: Tags {
                release_date: year.map(|y| format!("{y}-01-01")),
                ..Default::default()
            },
            codec,
            cover_url: cover,
            release_year: year.unwrap_or(0),
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
        if Path::new(track_id).exists() {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::TempFilePath,
                file_url: None,
                file_url_headers: serde_json::Map::new(),
                temp_file_path: Some(PathBuf::from(track_id)),
                different_codec: Some(CodecFlags::FLAC),
            });
        }
        if track_id.starts_with("https://") {
            let mut headers = serde_json::Map::new();
            headers.insert("Referer".to_string(), json!(REFERER));
            let codec = if track_id.to_lowercase().ends_with(".mp3") {
                CodecFlags::MP3
            } else {
                CodecFlags::FLAC
            };
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::Url,
                file_url: Some(track_id.to_string()),
                file_url_headers: headers,
                temp_file_path: None,
                different_codec: Some(codec),
            });
        }
        Err(Error::Other(format!(
            "archive_org: expected direct download URL, got {track_id}"
        )))
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let identifier = identifier_from(album_id);
        let url = format!("https://archive.org/metadata/{identifier}");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("archive_org metadata fetch: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "archive_org metadata HTTP {}",
                resp.status()
            )));
        }
        let val: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("archive_org metadata json: {e}")))?;
        let meta = val.get("metadata");
        let title = meta
            .and_then(|m| m.get("title"))
            .and_then(first_string)
            .unwrap_or_else(|| identifier.clone());
        let artist = meta
            .and_then(|m| m.get("creator"))
            .and_then(first_string)
            .unwrap_or_default();
        let year = meta
            .and_then(|m| m.get("year"))
            .and_then(first_string)
            .and_then(|y| y.get(0..4).and_then(|s| s.parse::<i32>().ok()));

        let mut tracks: Vec<TrackRef> = Vec::new();
        let mut has_flac = false;
        if let Some(files) = val.get("files").and_then(|f| f.as_array()) {
            for f in files {
                let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let fmt = f.get("format").and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let lname = name.to_lowercase();
                let lfmt = fmt.to_lowercase();
                let is_flac = lfmt.contains("flac") || lname.ends_with(".flac");
                let is_mp3 = lfmt.contains("mp3") || lname.ends_with(".mp3");
                if is_flac || is_mp3 {
                    if is_flac {
                        has_flac = true;
                    }
                    tracks.push(TrackRef::Id(format!(
                        "https://archive.org/download/{identifier}/{name}"
                    )));
                }
            }
        }
        if tracks.is_empty() {
            return Err(Error::Other(format!(
                "archive_org: no audio files for identifier {identifier}"
            )));
        }
        Ok(AlbumInfo {
            name: title,
            artist: artist.clone(),
            tracks,
            release_year: year.unwrap_or(0),
            artist_id: None,
            id: Some(identifier.clone()),
            quality: Some(if has_flac {
                "FLAC".to_string()
            } else {
                "MP3".to_string()
            }),
            cover_url: Some(format!("https://archive.org/services/img/{identifier}")),
            cover_type: Some(ImageFileType::Jpg),
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
        track_id: &str,
        _cover: &CoverOptions,
        data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        if let Some(v) = data.get("__cover__").and_then(|v| v.as_str()) {
            return Ok(CoverInfo {
                url: v.to_string(),
                file_type: ImageFileType::Jpg,
            });
        }
        Err(Error::Other(format!(
            "archive_org: no cover for track {track_id}"
        )))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let encoded = utf8_percent_encode(query, NON_ALPHANUMERIC).to_string();
        let url = format!(
            "https://archive.org/advancedsearch.php?q={encoded}&fl%5B%5D=identifier&fl%5B%5D=title&fl%5B%5D=creator&fl%5B%5D=year&rows={limit}&page=1&output=json"
        );
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("archive_org search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let root: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("archive_org search json: {e}")))?;
        let mut out = Vec::new();
        if let Some(docs) = root
            .get("response")
            .and_then(|r| r.get("docs"))
            .and_then(|d| d.as_array())
        {
            for doc in docs {
                let identifier = doc
                    .get("identifier")
                    .and_then(first_string)
                    .unwrap_or_default();
                if identifier.is_empty() {
                    continue;
                }
                let title = doc.get("title").and_then(first_string);
                let creator = doc.get("creator").and_then(first_string);
                let year = doc.get("year").and_then(first_string);
                out.push(SearchResult {
                    result_id: identifier,
                    name: title,
                    artists: creator.map(|c| vec![c]),
                    year,
                    ..Default::default()
                });
                if out.len() >= limit as usize {
                    break;
                }
            }
        }
        out.truncate(limit as usize);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
