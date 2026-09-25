//! Mp3db (mp3db.pro) dedicated site module.
//!
//! Search hits `search.php?s=<query>`, whose result anchors point at
//! `post.php?pid=NNNNN`. Each album post page contains one or more file-host
//! download links (nfile.cc, uploadbox.com, ...) which are surfaced directly as
//! `TrackRef::Id` download sources.

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
const REFERER: &str = "https://mp3db.pro/";
const HOST: &str = "mp3db.pro";
const SERVICE: &str = "Mp3db";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single(HOST.to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("post".to_string(), DownloadType::album);
            m.insert("album".to_string(), DownloadType::album);
            m.insert("singles".to_string(), DownloadType::track);
            m.insert("newsongs".to_string(), DownloadType::track);
            m.insert("lossless".to_string(), DownloadType::track);
            m.insert("flac".to_string(), DownloadType::track);
            m
        },
        test_url: Some("https://mp3db.pro/post.php?pid=37246".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(Mp3dbConstructor)
}

#[derive(Debug)]
struct Mp3dbConstructor;

impl ModuleConstructor for Mp3dbConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(Mp3dbModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct Mp3dbModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_entities(s: &str) -> String {
    s.replace("&#8211;", "-")
        .replace("&#8212;", "-")
        .replace("&#8216;", "'")
        .replace("&#8217;", "'")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&#8220;", "\"")
        .replace("&#8221;", "\"")
        .replace("&#124;", "|")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn strip_tags(html: &str) -> String {
    let re = Regex::new(r"<[^>]*>").unwrap();
    re.replace_all(html, " ").to_string()
}

fn clean_title(raw: &str) -> String {
    let text = decode_entities(&strip_tags(raw));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn split_title(title: &str) -> (String, String, Option<i32>) {
    let year_re = Regex::new(r"\((\d{4})\)").unwrap();
    let title = decode_entities(title).trim().to_string();
    let (artist, album) = if let Some(idx) = title.find(" - ") {
        (
            title[..idx].trim().to_string(),
            title[idx + 3..].trim().to_string(),
        )
    } else {
        ("Unknown Artist".to_string(), title.clone())
    };
    let year = year_re
        .captures(&title)
        .and_then(|c| c.get(1).map(|m| m.as_str().parse::<i32>().ok()))
        .flatten();
    let artist = if artist.is_empty() {
        "Unknown Artist".to_string()
    } else {
        artist
    };
    (artist, album, year)
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("mp3db fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("mp3db HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("mp3db read: {e}")))
}

/// Extract `post.php?pid=` result anchors. The title is the anchor's text after
/// the optional `<img>`/`<br>` markup, e.g.
/// `<a href="https://mp3db.pro/post.php?pid=37246"><img ...><BR>Artist - Album (Year)</a>`.
fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let re = Regex::new(
        r#"(?s)<a\s+href="((?:https?://mp3db\.pro/)?post\.php\?pid=\d+)"[^>]*>(.*?)</a>"#,
    )
    .unwrap();
    let mut out: Vec<SearchResult> = Vec::new();
    for cap in re.captures_iter(html) {
        let raw_href = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if raw_href.is_empty() {
            continue;
        }
        let result_id = if raw_href.starts_with("http") {
            raw_href.to_string()
        } else {
            format!("https://{HOST}/{raw_href}")
        };
        let title = cap
            .get(2)
            .map(|m| clean_title(m.as_str()))
            .unwrap_or_default();
        if title.is_empty() || out.iter().any(|r| r.result_id == result_id) {
            continue;
        }
        let (artist, album, year) = split_title(&title);
        out.push(SearchResult {
            result_id,
            name: Some(album),
            artists: Some(vec![artist]),
            year: year.map(|y| y.to_string()),
            ..Default::default()
        });
    }
    out
}

fn parse_album_meta(html: &str) -> (String, String, String, String, Option<i32>) {
    let og_title = Regex::new(r#"<meta property="og:title" content="([^"]+)""#).unwrap();
    let mut title = og_title
        .captures(html)
        .and_then(|c| c.get(1).map(|m| decode_entities(m.as_str().trim())))
        .unwrap_or_default();
    if title.is_empty() {
        let title_re = Regex::new(r"(?s)<title>(.*?)</title>").unwrap();
        title = title_re
            .captures(html)
            .and_then(|c| c.get(1).map(|m| clean_title(m.as_str())))
            .unwrap_or_else(|| "Unknown".to_string());
    }
    let (artist, album, year) = split_title(&title);
    let cover_re = Regex::new(r#"<meta property="og:image" content="([^"]+)""#).unwrap();
    let cover = cover_re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .unwrap_or_default();
    (artist, album, title, cover, year)
}

fn parse_download_link(html: &str) -> Option<String> {
    let re = Regex::new(
        r#"href="(https?://[^"]*(?:nfile\.cc|uploadbox\.com|zippyshare\.com|mediafire\.com|mega\.nz|mega\.co\.nz|1fichier\.com)[^"]*)""#,
    )
    .unwrap();
    re.captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
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
impl picaro_utils::module::ModuleInterface for Mp3dbModule {
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
            .unwrap_or_else(|| {
                if !artist.is_empty() && !album.is_empty() {
                    format!("{artist} - {album}")
                } else {
                    derived.clone()
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
        if track_id.starts_with("https://") || track_id.starts_with("http://") {
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
            "mp3db: expected direct download URL, got {track_id}"
        )))
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let url = if album_id.starts_with("http://") || album_id.starts_with("https://") {
            album_id.to_string()
        } else if album_id.starts_with('/') {
            format!("https://{HOST}{album_id}")
        } else {
            format!("https://{HOST}/{album_id}")
        };
        let html = fetch_page(&self.client, &url).await?;
        let (artist, album, title, cover, year) = parse_album_meta(&html);
        let download_url = parse_download_link(&html).ok_or_else(|| {
            Error::Other("mp3db: no file-host download link found on post page".to_string())
        })?;
        Ok(AlbumInfo {
            name: album,
            artist: artist.clone(),
            tracks: vec![TrackRef::Id(download_url)],
            release_year: year.unwrap_or(0),
            artist_id: None,
            id: Some(album_id.to_string()),
            quality: Some("MP3".to_string()),
            cover_url: if cover.is_empty() { None } else { Some(cover) },
            cover_type: Some(ImageFileType::Jpg),
            description: Some(title),
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
        let (_, _, cover, _) = album_meta(&data);
        if !cover.is_empty() {
            return Ok(CoverInfo {
                url: cover,
                file_type: ImageFileType::Jpg,
            });
        }
        Err(Error::Other(format!(
            "mp3db: no cover for track {track_id}"
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
        let url = format!("https://{HOST}/search.php?s={encoded}");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("mp3db search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("mp3db search read: {e}")))?;
        let mut results = parse_search_results(&html);
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
