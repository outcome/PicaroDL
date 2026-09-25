use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const REFERER: &str = "https://a2.freemp3cloud.com/";
const BASE: &str = "https://a2.freemp3cloud.com";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "FreeMp3Cloud".to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Multi(vec![
            "freemp3cloud.com".to_string(),
            "a2.freemp3cloud.com".to_string(),
            "cdnm.meln.top".to_string(),
            "pl.meln.top".to_string(),
        ]),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://a2.freemp3cloud.com/".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(FreeMp3CloudConstructor)
}

#[derive(Debug)]
struct FreeMp3CloudConstructor;

impl ModuleConstructor for FreeMp3CloudConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(FreeMp3CloudModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct FreeMp3CloudModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn extract_token(html: &str) -> Option<String> {
    let re = Regex::new(r#"name="__RequestVerificationToken"[^>]*value="([^"]+)""#).unwrap();
    re.captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

// UNVERIFIED: each result is a `.play-item` block with `.s-artist` / `.s-title`
// text and a `.downl` anchor pointing at a direct MP3 on cdnm.meln.top (verified
// against a POST search for "radiohead" on https://a2.freemp3cloud.com/).
fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let artist_re = Regex::new(r#"<div class="s-artist">([^<]*)</div>"#).unwrap();
    let title_re = Regex::new(r#"<div class="s-title">([^<]*)</div>"#).unwrap();
    let dl_re = Regex::new(r#"(?s)<div class="downl">.*?<a[^>]+href="([^"]+)"[^>]*>"#).unwrap();

    let mut out: Vec<SearchResult> = Vec::new();
    for block in html.split(r#"class="play-item""#).skip(1) {
        let raw_url = match dl_re.captures(block) {
            Some(c) => c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
            None => continue,
        };
        let url = decode_entities(&raw_url);
        if !url.starts_with("http") {
            continue;
        }
        let artist = artist_re
            .captures(block)
            .and_then(|c| c.get(1).map(|m| decode_entities(m.as_str())))
            .unwrap_or_default();
        let title = title_re
            .captures(block)
            .and_then(|c| c.get(1).map(|m| decode_entities(m.as_str())))
            .unwrap_or_default();
        if out.iter().any(|r| r.result_id == url) {
            continue;
        }
        let name = if !artist.is_empty() && !title.is_empty() {
            format!("{artist} - {title}")
        } else if !title.is_empty() {
            title
        } else {
            url.clone()
        };
        out.push(SearchResult {
            result_id: url,
            name: Some(name),
            artists: if artist.is_empty() {
                None
            } else {
                Some(vec![artist])
            },
            ..Default::default()
        });
    }
    out
}

fn track_name_from_url(url: &str) -> String {
    let file = url
        .split('?')
        .next()
        .unwrap_or(url)
        .split('/')
        .next_back()
        .unwrap_or(url);
    let decoded = percent_decode_str(file).decode_utf8_lossy().to_string();
    let without_ext = decoded
        .strip_suffix(".mp3")
        .or_else(|| decoded.strip_suffix(".MP3"))
        .unwrap_or(&decoded);
    without_ext.trim().to_string()
}

async fn run_search(client: &reqwest::Client, query: &str) -> Result<Vec<SearchResult>> {
    let home = client
        .get(BASE)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("freemp3cloud home fetch: {e}")))?;
    if !home.status().is_success() {
        return Err(Error::Other(format!(
            "freemp3cloud home HTTP {}",
            home.status()
        )));
    }
    let home_html = home
        .text()
        .await
        .map_err(|e| Error::Other(format!("freemp3cloud home read: {e}")))?;
    let token = extract_token(&home_html)
        .ok_or_else(|| Error::Other("freemp3cloud: antiforgery token not found".to_string()))?;
    let resp = client
        .post(BASE)
        .header("Referer", REFERER)
        .form(&[
            ("searchSong", query),
            ("__RequestVerificationToken", token.as_str()),
        ])
        .send()
        .await
        .map_err(|e| Error::Other(format!("freemp3cloud search: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "freemp3cloud search HTTP {}",
            resp.status()
        )));
    }
    let html = resp
        .text()
        .await
        .map_err(|e| Error::Other(format!("freemp3cloud search read: {e}")))?;
    Ok(parse_search_results(&html))
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for FreeMp3CloudModule {
    fn name(&self) -> &str {
        "FreeMp3Cloud"
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
        let meta = data.get("__album_meta__").cloned();
        let (album, artist, cover) = match meta {
            Some(v) => (
                v.get("album")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("artist")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("cover")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            None => (String::new(), String::new(), String::new()),
        };
        let derived = track_name_from_url(track_id);
        let name = data
            .get("__track_name__")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if !derived.is_empty() {
                    derived.clone()
                } else {
                    track_id.to_string()
                }
            });
        let artists = if !artist.is_empty() {
            vec![artist]
        } else if derived.contains(" - ") {
            let a = derived.split(" - ").next().unwrap_or("").trim();
            if a.is_empty() {
                vec![]
            } else {
                vec![a.to_string()]
            }
        } else {
            vec![]
        };
        Ok(TrackInfo {
            name,
            album,
            album_id: String::new(),
            artists,
            codec: CodecFlags::MP3,
            cover_url: cover,
            release_year: 0,
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
        let path = Path::new(track_id);
        let mut headers = serde_json::Map::new();
        headers.insert("Referer".to_string(), json!(REFERER));
        if path.exists() {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::TempFilePath,
                file_url: None,
                file_url_headers: headers,
                temp_file_path: Some(path.to_path_buf()),
                different_codec: Some(CodecFlags::MP3),
            });
        }
        let url = if track_id.starts_with("//") {
            format!("https:{track_id}")
        } else {
            track_id.to_string()
        };
        if !url.starts_with("http") {
            return Err(Error::Other(format!(
                "freemp3cloud: expected MP3 URL, got {track_id}"
            )));
        }
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(url),
            file_url_headers: headers,
            temp_file_path: None,
            different_codec: Some(CodecFlags::MP3),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        if album_id.starts_with("http") {
            let name = track_name_from_url(album_id);
            let artist = name.split(" - ").next().unwrap_or("").trim().to_string();
            return Ok(AlbumInfo {
                name: name.clone(),
                artist,
                tracks: vec![TrackRef::Id(album_id.to_string())],
                release_year: 0,
                id: Some(album_id.to_string()),
                quality: Some("MP3".to_string()),
                ..Default::default()
            });
        }
        Err(Error::Other(format!(
            "freemp3cloud: expected direct MP3 URL, got {album_id}"
        )))
    }

    async fn get_playlist_info(
        &self,
        _playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "FreeMp3Cloud".to_string(),
            ability: "playlist".to_string(),
        })
    }

    async fn get_artist_info(
        &self,
        artist_id: &str,
        _get_credited_albums: bool,
        _artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        let results = run_search(&self.client, artist_id).await?;
        Ok(ArtistInfo {
            name: artist_id.to_string(),
            artist_id: Some(artist_id.to_string()),
            tracks: results
                .into_iter()
                .map(|t| serde_json::Value::String(t.result_id))
                .collect(),
            ..Default::default()
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
            "freemp3cloud: no cover for track {track_id}"
        )))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let mut results = run_search(&self.client, query).await?;
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
