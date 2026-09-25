//! Tancpol (tancpol.net) site module.
//!
//! Search: GET https://tancpol.net/music/{url-encoded-query} (the site's own
//! `artist` search form target). Result items are rendered as
//! `div.item-track ... data-file="//tancpol.net/stream/mym/{base64}"`, where the
//! base64 payload decodes to the real upstream `.mp3` URL. The stream endpoint
//! answers `200 audio/mpeg`, so it is used directly as the download source.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const REFERER: &str = "https://tancpol.net/";
const HOST: &str = "tancpol.net";
const SERVICE: &str = "Tancpol";

pub fn module_information() -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    url_constants.insert("music".to_string(), DownloadType::track);
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single(HOST.to_string()),
        url_constants,
        test_url: Some("https://tancpol.net/".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(TancpolConstructor)
}

#[derive(Debug)]
struct TancpolConstructor;

impl ModuleConstructor for TancpolConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(TancpolModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct TancpolModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn resolve_stream(data_file: &str) -> String {
    if data_file.starts_with("http://") || data_file.starts_with("https://") {
        data_file.to_string()
    } else if let Some(rest) = data_file.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        format!("https://{HOST}{data_file}")
    }
}

fn decode_entities_basic(s: &str) -> String {
    s.replace("&#8211;", "-")
        .replace("&#8217;", "'")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
}

fn split_title(title: &str) -> (Option<String>, Option<String>, Option<String>) {
    let year_re = Regex::new(r"\((\d{4})\)").unwrap();
    let title = title.trim();
    if let Some(idx) = title.find(" - ") {
        let artist = title[..idx].trim().to_string();
        let rest = title[idx + 3..].trim().to_string();
        let year = year_re
            .captures(&rest)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let artist = if artist.is_empty() {
            None
        } else {
            Some(artist)
        };
        (artist, Some(rest), year)
    } else {
        let year = year_re
            .captures(title)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let album = if title.is_empty() {
            None
        } else {
            Some(title.to_string())
        };
        (None, album, year)
    }
}

fn parse_search_items(html: &str) -> Vec<(String, Option<String>, String)> {
    let item_re = Regex::new(
        r#"(?s)<div class="item-track[^"]*"[^>]*data-file="([^"]+)"[^>]*>(.*?)<div class="item-track-time">"#,
    )
    .unwrap();
    let artist_re = Regex::new(r#"<a class="item-title[^"]*"[^>]*>([^<]+)</a>"#).unwrap();
    let track_re = Regex::new(r#"<p class="item-subtitle[^"]*"[^>]*>([^<]+)</p>"#).unwrap();
    let mut out = Vec::new();
    for cap in item_re.captures_iter(html) {
        let data_file = cap
            .get(1)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        if data_file.is_empty() {
            continue;
        }
        let block = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let artist = artist_re.captures(block).and_then(|c| {
            c.get(1)
                .map(|m| decode_entities_basic(m.as_str().trim()))
                .filter(|s| !s.is_empty())
        });
        let track = track_re
            .captures(block)
            .and_then(|c| c.get(1).map(|m| decode_entities_basic(m.as_str().trim())))
            .unwrap_or_default();
        if track.is_empty() {
            continue;
        }
        out.push((resolve_stream(&data_file), artist, track));
    }
    out
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

#[async_trait]
impl picaro_utils::module::ModuleInterface for TancpolModule {
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
        let (album0, artist0, cover, year) = album_meta(&data);
        let name = data
            .get("__track_name__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| track_id.to_string());
        let artist = data
            .get("__artist__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if artist0.is_empty() {
                    None
                } else {
                    Some(artist0)
                }
            });
        let album = data
            .get("__album__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or(album0);
        Ok(TrackInfo {
            name,
            album,
            album_id: String::new(),
            artists: artist.map(|a| vec![a]).unwrap_or_default(),
            tags: Tags {
                release_date: year.map(|y| format!("{y}-01-01")),
                ..Default::default()
            },
            codec: CodecFlags::MP3,
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
                different_codec: Some(CodecFlags::MP3),
            });
        }
        if track_id.starts_with("https://") {
            let mut headers = serde_json::Map::new();
            headers.insert("Referer".to_string(), json!(REFERER));
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::Url,
                file_url: Some(track_id.to_string()),
                file_url_headers: headers,
                temp_file_path: None,
                different_codec: Some(CodecFlags::MP3),
            });
        }
        Err(Error::Other(format!(
            "tancpol: expected direct download URL, got {track_id}"
        )))
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let url = if album_id.starts_with("https://") {
            album_id.to_string()
        } else {
            format!("https://{HOST}/music/{album_id}")
        };
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("tancpol album fetch: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Other(format!(
                "tancpol album HTTP {}",
                resp.status()
            )));
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("tancpol album read: {e}")))?;
        let items = parse_search_items(&html);
        if items.is_empty() {
            return Err(Error::Other("tancpol: no tracks found on page".to_string()));
        }
        let tracks = items
            .iter()
            .map(|(stream, _, _)| TrackRef::Id(stream.clone()))
            .collect();
        let (_, first_artist, first_track) = items.into_iter().next().unwrap();
        Ok(AlbumInfo {
            name: first_track,
            artist: first_artist.unwrap_or_else(|| "Unknown Artist".to_string()),
            tracks,
            release_year: 0,
            artist_id: None,
            id: Some(album_id.to_string()),
            quality: Some("MP3".to_string()),
            cover_url: None,
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
            "tancpol: no cover for track {track_id}"
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
        let url = format!("https://{HOST}/music/{encoded}");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("tancpol search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("tancpol search read: {e}")))?;
        let mut out: Vec<SearchResult> = Vec::new();
        for (stream, artist, track) in parse_search_items(&html) {
            if out.iter().any(|r| r.result_id == stream) {
                continue;
            }
            let (_, album, year) = split_title(&track);
            out.push(SearchResult {
                result_id: stream,
                name: album,
                artists: artist.map(|a| vec![a]),
                year,
                ..Default::default()
            });
            if out.len() >= limit as usize {
                break;
            }
        }
        out.truncate(limit as usize);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
