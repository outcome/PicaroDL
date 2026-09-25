use std::collections::HashMap;
use std::path::Path;
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
const REFERER: &str = "https://www.soundclick.com/";
const BASE: &str = "https://www.soundclick.com";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "SoundClick".to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("soundclick.com".to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("track".to_string(), DownloadType::track);
            m.insert("songs".to_string(), DownloadType::track);
            m
        },
        test_url: Some(
            "https://www.soundclick.com/track/12867706/teina/creep-radiohead-semi-acoustic"
                .to_string(),
        ),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(SoundclickConstructor)
}

#[derive(Debug)]
struct SoundclickConstructor;

impl ModuleConstructor for SoundclickConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(SoundclickModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct SoundclickModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_entities(s: &str) -> String {
    s.replace("&#38;", "&")
        .replace("&#39;", "'")
        .replace("&#039;", "'")
        .replace("&apos;", "'")
        .replace("&#8217;", "'")
        .replace("&#8211;", "\u{2013}")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("soundclick fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("soundclick HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("soundclick read: {e}")))
}

fn absolutize(url: &str) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else if url.starts_with('/') {
        format!("{BASE}{url}")
    } else {
        url.to_string()
    }
}

// Each search result is a `<section id="playingSong_<songID>" ...>` card,
// verified against /search/default.cfm?type=songs&searchterm=radiohead. Note
// that the id delimiter is consumed by `split`, so the song id is taken from
// the leading digits of each block instead of re-matching `id="playingSong_"`.
fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let title_re =
        Regex::new(r#"(?s)<a class="charts2_songname[^"]*"\s+href="([^"]+)".*?<span>(.*?)</span>"#)
            .unwrap();
    let artist_re = Regex::new(
        r#"<a[^>]*class="sclk_link"[^>]*data-href="/artist/default\.cfm\?bandID=(\d+)"[^>]*>\s*([^<]+?)\s*</a>"#,
    )
    .unwrap();
    let img_re =
        Regex::new(r#"<img class="charts2_section_picture[^"]*"\s+data-src="([^"]+)""#).unwrap();

    let mut out: Vec<SearchResult> = Vec::new();
    for block in html.split("id=\"playingSong_").skip(1) {
        let song_id = block.split('"').next().unwrap_or("").trim().to_string();
        if song_id.is_empty() || !song_id.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let (track_url, title) = match title_re.captures(block) {
            Some(c) => (
                c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
                c.get(2)
                    .map(|m| decode_entities(m.as_str()))
                    .unwrap_or_default(),
            ),
            None => (String::new(), String::new()),
        };
        if track_url.is_empty() {
            continue;
        }
        let (band_id, artist) = match artist_re.captures(block) {
            Some(c) => (
                c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
                c.get(2)
                    .map(|m| decode_entities(m.as_str()))
                    .unwrap_or_default(),
            ),
            None => (String::new(), String::new()),
        };
        let image = img_re
            .captures(block)
            .and_then(|c| c.get(1).map(|m| absolutize(m.as_str())));
        let result_id = absolutize(&track_url);
        if out.iter().any(|r| r.result_id == result_id) {
            continue;
        }
        let name = if !artist.is_empty() && !title.is_empty() {
            format!("{artist} - {title}")
        } else if !title.is_empty() {
            title
        } else {
            result_id.clone()
        };
        out.push(SearchResult {
            result_id,
            name: Some(name),
            artists: if artist.is_empty() {
                None
            } else {
                Some(vec![artist])
            },
            image_url: image,
            extra_kwargs: {
                let mut m = serde_json::Map::new();
                m.insert("song_id".to_string(), json!(song_id));
                m.insert("band_id".to_string(), json!(band_id));
                m
            },
            ..Default::default()
        });
    }
    out
}

// UNVERIFIED: parse the `<script type="application/ld+json">` MusicRecording
// graph to get track name, artist and artwork.
fn extract_recording(html: &str) -> Option<(String, String, String)> {
    let re = Regex::new(r#"(?s)<script type="application/ld\+json">(.*?)</script>"#).ok()?;
    for cap in re.captures_iter(html) {
        let txt = cap.get(1)?.as_str();
        let val: Value = match serde_json::from_str(txt) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let graph: Vec<Value> = match val.get("@graph").and_then(|g| g.as_array()) {
            Some(g) => g.clone(),
            None => vec![val.clone()],
        };
        let mut artist_by_id: HashMap<String, String> = HashMap::new();
        for item in &graph {
            if let (Some(id), Some(name)) = (
                item.get("@id").and_then(|v| v.as_str()),
                item.get("name").and_then(|v| v.as_str()),
            ) {
                artist_by_id.insert(id.to_string(), name.to_string());
            }
        }
        for item in &graph {
            let is_rec = match item.get("@type") {
                Some(Value::String(s)) => s == "MusicRecording",
                Some(Value::Array(a)) => a.iter().any(|v| v.as_str() == Some("MusicRecording")),
                _ => false,
            };
            if !is_rec {
                continue;
            }
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let artist = item
                .get("byArtist")
                .and_then(|v| v.get("@id"))
                .and_then(|v| v.as_str())
                .and_then(|id| artist_by_id.get(id).cloned())
                .unwrap_or_default();
            let image = item
                .get("image")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Some((name, artist, image));
        }
    }
    None
}

fn parse_download_href(html: &str) -> Option<String> {
    let re = Regex::new(r#"/utils_download/download_song\.cfm\?[^"'\s<>]+"#).ok()?;
    re.find(html).map(|m| absolutize(m.as_str()))
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for SoundclickModule {
    fn name(&self) -> &str {
        "SoundClick"
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
        let (album, artist, cover) = match data.get("__album_meta__").cloned() {
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
        let name = data
            .get("__track_name__")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if !artist.is_empty() {
                    format!("{artist} - {album}")
                } else {
                    track_id.to_string()
                }
            });
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
        if !track_id.starts_with("http") {
            return Err(Error::Other(format!(
                "soundclick: expected download URL, got {track_id}"
            )));
        }
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(track_id.to_string()),
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
        let url = if album_id.starts_with("http") {
            album_id.to_string()
        } else {
            format!("{BASE}/{}", album_id.trim_start_matches('/'))
        };
        let html = fetch_page(&self.client, &url).await?;
        let (name, artist, image) =
            extract_recording(&html).unwrap_or((url.clone(), String::new(), String::new()));
        let download = parse_download_href(&html).ok_or_else(|| {
            Error::Other("soundclick: no download link on track page".to_string())
        })?;
        let cover = if image.is_empty() {
            None
        } else {
            Some(absolutize(&image))
        };
        Ok(AlbumInfo {
            name,
            artist,
            tracks: vec![TrackRef::Id(download)],
            release_year: 0,
            artist_id: None,
            id: Some(album_id.to_string()),
            quality: Some("MP3".to_string()),
            cover_url: cover,
            cover_type: Some(ImageFileType::Webp),
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        _playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "SoundClick".to_string(),
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
            module: "SoundClick".to_string(),
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
                file_type: ImageFileType::Webp,
            });
        }
        Err(Error::Other(format!(
            "soundclick: no cover for track {track_id}"
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
        let url = format!("{BASE}/search/default.cfm?type=songs&searchterm={encoded}");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("soundclick search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("soundclick search read: {e}")))?;
        let mut results = parse_search_results(&html);
        if results.is_empty() {
            results = parse_search_fallback(&html);
        }
        results.truncate(limit as usize);
        Ok(results)
    }
}

// Fallback: derive results from the JSON-LD ItemList when the HTML card
// structure changes.
fn parse_search_fallback(html: &str) -> Vec<SearchResult> {
    let re = Regex::new(r#"(?s)<script type="application/ld\+json">(.*?)</script>"#).unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let txt = match cap.get(1) {
            Some(m) => m.as_str(),
            None => continue,
        };
        let val: Value = match serde_json::from_str(txt) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let graph = val.get("@graph").and_then(|g| g.as_array());
        if let Some(graph) = graph {
            for item in graph {
                let is_list = item
                    .get("@type")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "ItemList")
                    .unwrap_or(false);
                if !is_list {
                    continue;
                }
                let elements = item
                    .get("itemListElement")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                for el in elements {
                    let rec = match el.get("item") {
                        Some(r) => r,
                        None => continue,
                    };
                    let name = rec
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let url = rec
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if url.is_empty() || out.iter().any(|r: &SearchResult| r.result_id == url) {
                        continue;
                    }
                    out.push(SearchResult {
                        result_id: url,
                        name: Some(name),
                        ..Default::default()
                    });
                }
            }
        }
    }
    out
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
