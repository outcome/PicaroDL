//! Dance Music Organisation (dance-music.org) site module.
//!
//! The site is backed by Perl CGI scripts under `/cgi-bin/`:
//!   - `/cgi-bin/free-mp3-music-downloads.pl?genre=<Genre>` lists albums and
//!     their tracks. Each track is an `<a class="play" …>` with `data-title`,
//!     `data-artist`, `data-album` and an `href`/`data-href` pointing at
//!     `/cgi-bin/download_music.pl?MP3=1&ID=<id>&UID=<token>`.
//!   - That download endpoint returns a `302` to a **static** MP3 under
//!     `/mp3/…` when the `DMO_DOWNLOAD=1` cookie is present (the cookie is what
//!     the site's JS normally sets). The static URL needs no auth, so we
//!     resolve the redirect and hand the direct link to the downloader.
//!
//! There is no native full-text search, so `search()` scans every genre
//! listing concurrently and filters by token overlap with the query.

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
const REFERER: &str = "https://dance-music.org/";
const BASE: &str = "https://dance-music.org";
const SERVICE: &str = "DanceMusicOrg";

pub fn module_information() -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    url_constants.insert("cgi-bin".to_string(), DownloadType::track);
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("dance-music.org".to_string()),
        url_constants,
        test_url: Some("https://dance-music.org/".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(DanceMusicConstructor)
}

#[derive(Debug)]
struct DanceMusicConstructor;

impl ModuleConstructor for DanceMusicConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(DanceMusicModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct DanceMusicModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn absolute(url: &str) -> String {
    if url.starts_with("http") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("{BASE}{url}")
    } else {
        format!("{BASE}/{url}")
    }
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("dance-music fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("dance-music HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("dance-music read: {e}")))
}

/// A parsed track row from a genre listing page.
struct TrackRow {
    artist: String,
    title: String,
    album: String,
    download_url: String,
}

fn parse_track_rows(html: &str) -> Vec<TrackRow> {
    let re = Regex::new(
        r#"(?is)<a[^>]*data-title="([^"]*)"[^>]*data-artist="([^"]*)"[^>]*data-album="([^"]*)"[^>]*(?:data-)?href="(/cgi-bin/download_music\.pl\?[^"]+)""#,
    )
    .unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let title = decode_entities(cap.get(1).map(|m| m.as_str()).unwrap_or(""));
        let artist = decode_entities(cap.get(2).map(|m| m.as_str()).unwrap_or(""));
        let album = decode_entities(cap.get(3).map(|m| m.as_str()).unwrap_or(""));
        let dl = decode_entities(cap.get(4).map(|m| m.as_str()).unwrap_or(""));
        if dl.is_empty() {
            continue;
        }
        let url = absolute(&dl);
        if out.iter().any(|r: &TrackRow| r.download_url == url) {
            continue;
        }
        out.push(TrackRow {
            artist,
            title,
            album,
            download_url: url,
        });
    }
    out
}

fn parse_download_links(html: &str) -> Vec<String> {
    let re = Regex::new(r#"(?is)href="(/cgi-bin/download_music\.pl\?[^"]+)""#).unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let url = absolute(&decode_entities(
            cap.get(1).map(|m| m.as_str()).unwrap_or(""),
        ));
        if !url.is_empty() && !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

fn track_label(r: &TrackRow) -> String {
    if !r.artist.is_empty() && !r.title.is_empty() {
        format!("{} - {}", r.artist, r.title)
    } else if !r.title.is_empty() {
        r.title.clone()
    } else {
        r.album.clone()
    }
}

/// `search()` appends `&dn=<label>` to the download URL so the album path
/// (which cannot see the original search result) can recover artist/title.
/// The server ignores unknown query parameters.
fn parse_dn(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("dn=") {
            let decoded = percent_decode_str(v).decode_utf8_lossy().to_string();
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

fn split_label(label: &str) -> (String, String) {
    match label.find(" - ") {
        Some(i) => (
            label[..i].trim().to_string(),
            label[i + 3..].trim().to_string(),
        ),
        None => (String::new(), label.trim().to_string()),
    }
}

/// Resolve a `download_music.pl` URL to the static `/mp3/…` file it redirects
/// to. Requires the `DMO_DOWNLOAD=1` cookie; no session state is needed.
async fn resolve_direct(track_id: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Error::Other(format!("dance-music client: {e}")))?;
    let resp = client
        .get(track_id)
        .header("Referer", REFERER)
        .header("Cookie", "DMO_DOWNLOAD=1")
        .send()
        .await
        .map_err(|e| Error::Other(format!("dance-music download: {e}")))?;
    if let Some(loc) = resp.headers().get(reqwest::header::LOCATION) {
        if let Ok(loc) = loc.to_str() {
            let direct = decode_entities(loc);
            if direct.starts_with("http") {
                return Ok(direct);
            }
        }
    }
    // Some links already point straight at the static file.
    if resp.status().is_success() && track_id.contains("/mp3/") {
        return Ok(track_id.to_string());
    }
    Err(Error::Other(format!(
        "dance-music: could not resolve direct file for {track_id} (HTTP {})",
        resp.status()
    )))
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for DanceMusicModule {
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
        let name = data
            .get("__track_name__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                parse_dn(track_id)
                    .map(|l| split_label(&l).1)
                    .unwrap_or_else(|| {
                        let re = Regex::new(r"ID=(\d+)").unwrap();
                        re.captures(track_id)
                            .and_then(|c| c.get(1).map(|m| format!("Track {}", m.as_str())))
                            .unwrap_or_else(|| track_id.to_string())
                    })
            });
        let artist = data
            .get("__artist__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                let a = parse_dn(track_id)
                    .map(|l| split_label(&l).0)
                    .unwrap_or_default();
                if a.is_empty() {
                    None
                } else {
                    Some(a)
                }
            });
        Ok(TrackInfo {
            name,
            album: data
                .get("__album__")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            album_id: String::new(),
            artists: artist.map(|a| vec![a]).unwrap_or_default(),
            codec: CodecFlags::MP3,
            cover_url: String::new(),
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
        if Path::new(track_id).exists() {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::TempFilePath,
                file_url: None,
                file_url_headers: serde_json::Map::new(),
                temp_file_path: Some(std::path::PathBuf::from(track_id)),
                different_codec: Some(CodecFlags::MP3),
            });
        }
        if !track_id.starts_with("http") {
            return Err(Error::Other(format!(
                "dance-music: expected direct MP3 URL, got {track_id}"
            )));
        }
        let direct = if track_id.contains("/mp3/") {
            track_id.to_string()
        } else {
            resolve_direct(track_id).await?
        };
        let mut headers = serde_json::Map::new();
        headers.insert("Referer".to_string(), json!(REFERER));
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(direct),
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
        // A track download URL is treated as a single-track "album" so the
        // album-based resolver path can still fetch it.
        if album_id.starts_with("http")
            && (album_id.contains("download_music.pl") || album_id.contains("/mp3/"))
        {
            let (artist, name) = match parse_dn(album_id) {
                Some(label) => {
                    let (a, n) = split_label(&label);
                    (a, n)
                }
                None => {
                    let re = Regex::new(r"ID=(\d+)").unwrap();
                    let n = re
                        .captures(album_id)
                        .and_then(|c| c.get(1).map(|m| format!("Track {}", m.as_str())))
                        .unwrap_or_else(|| album_id.to_string());
                    (String::new(), n)
                }
            };
            return Ok(AlbumInfo {
                name,
                // Must be non-empty: the album folder template is
                // `{artist}/{name}`, and an empty artist yields a leading `/`
                // that `Path::join` would resolve against the drive root.
                artist: if artist.is_empty() {
                    "Dance Music Organisation".to_string()
                } else {
                    artist
                },
                tracks: vec![TrackRef::Id(album_id.to_string())],
                release_year: 0,
                id: Some(album_id.to_string()),
                quality: Some("MP3".to_string()),
                ..Default::default()
            });
        }
        let url = absolute(album_id);
        let html = fetch_page(&self.client, &url).await?;
        let downloads = parse_download_links(&html);
        if downloads.is_empty() {
            return Err(Error::Other(format!(
                "dance-music: no download links at {url}"
            )));
        }
        let title = Regex::new(r"(?is)<title>(.*?)</title>")
            .unwrap()
            .captures(&html)
            .and_then(|c| c.get(1).map(|m| decode_entities(m.as_str())))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| album_id.to_string());
        Ok(AlbumInfo {
            name: title,
            artist: "Dance Music Organisation".to_string(),
            tracks: downloads.into_iter().map(TrackRef::Id).collect(),
            release_year: 0,
            id: Some(album_id.to_string()),
            quality: Some("MP3".to_string()),
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
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        Err(Error::Other(format!(
            "dance-music: no cover for track {track_id}"
        )))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        // The site's CGI exposes a hidden full-text filter: `?search=<term>`
        // returns a listing with matching tracks (empty for no match). It
        // matches substrings, so a combined "artist title" query can miss;
        // search the title half when the query is in "artist - title" form.
        let term = match query.split_once(" - ") {
            Some((_artist, title)) if !title.trim().is_empty() => title.trim().to_string(),
            _ => query.trim().to_string(),
        };
        let encoded = utf8_percent_encode(&term, NON_ALPHANUMERIC).to_string();
        let url = format!("{BASE}/cgi-bin/free-mp3-music-downloads.pl?search={encoded}");
        let html = fetch_page(&self.client, &url).await?;

        let tokens: Vec<String> = query
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() >= 2)
            .map(|t| t.to_string())
            .collect();

        let mut scored: Vec<(usize, TrackRow)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for row in parse_track_rows(&html) {
            if !seen.insert(row.download_url.clone()) {
                continue;
            }
            let hay = format!("{} {} {}", row.artist, row.title, row.album).to_lowercase();
            let score = tokens.iter().filter(|t| hay.contains(t.as_str())).count();
            if tokens.is_empty() || score > 0 {
                scored.push((score, row));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0));

        let mut out: Vec<SearchResult> = Vec::new();
        for (_, row) in scored {
            let label = track_label(&row);
            let artists = if row.artist.is_empty() {
                None
            } else {
                Some(vec![row.artist.clone()])
            };
            let dn = utf8_percent_encode(&label, NON_ALPHANUMERIC).to_string();
            out.push(SearchResult {
                result_id: format!("{}&dn={dn}", row.download_url),
                name: Some(label),
                artists,
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
