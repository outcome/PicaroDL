//! MixtapeMonkey (mixtapemonkey.com) module.
//!
//! Search is a POST to `/mixtapes.inc.php` with form body `name=<q>`
//! (GET returns an empty body). Mixtape detail pages live at
//! `/{id}/{slug}` and list every track as
//! `<li data-title="..." data-type="mp3" data-url="/mixtapes/zip/... .mp3">`
//! — plain GET downloads, ID3 + HTTP 206 verified. The `data-url` paths
//! carry raw spaces/quotes, so they are percent-encoded before download.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use serde_json::Value;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "MixtapeMonkey";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://mixtapemonkey.com";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("mixtapemonkey.com".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://mixtapemonkey.com/69/kid-cudi-a-kid-named-cudi".to_string()),
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
        Ok(Arc::new(Mod {
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct Mod {
    client: reqwest::Client,
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// Encode the path part of a `data-url` (raw spaces, quotes, parens).
fn enc_path(url: &str) -> String {
    let mut out = String::new();
    for c in url.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '\'' => out.push_str("%27"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '&' => out.push_str("%26"),
            '#' => out.push_str("%23"),
            '+' => out.push_str("%2B"),
            '"' => out.push_str("%22"),
            '\\' => {}
            _ => out.push(c),
        }
    }
    out
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("mixtapemonkey fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "mixtapemonkey HTTP {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("mixtapemonkey read: {e}")))
}

/// Search-response items: anchor -> tape title span -> artist div.
/// `/artist/...` links don't match (first path segment must be digits).
fn parse_search_items(html: &str) -> Vec<(String, String, String)> {
    let re = Regex::new(
        r#"(?s)<a href="/(\d+)/([^"]+)">(?:.*?)<span>([^<]*)</span>(?:.*?)searchtype-item-artist'>([^<]*)</div>"#,
    )
    .unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let id = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        let slug = cap.get(2).map(|m| m.as_str()).unwrap_or_default();
        let title = cap.get(3).map(|m| m.as_str()).unwrap_or("").trim();
        let artist = cap.get(4).map(|m| m.as_str()).unwrap_or("").trim();
        if id.is_empty() || slug.is_empty() {
            continue;
        }
        out.push((
            format!("{BASE}/{id}/{slug}"),
            title.to_string(),
            artist.to_string(),
        ));
    }
    out
}

/// `(title, mp3 url)` pairs from a detail page's track list.
fn parse_tracks(html: &str) -> Vec<(String, String)> {
    let paired = Regex::new(r#"data-title="([^"]*)"[^>]*data-url="([^"]+\.mp3)""#).unwrap();
    let mut out = Vec::new();
    for cap in paired.captures_iter(html) {
        let title = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
        let url = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
        if url.is_empty() {
            continue;
        }
        let full = if url.starts_with('/') {
            format!("{BASE}{}", enc_path(&url))
        } else {
            enc_path(&url)
        };
        let title = if title.is_empty() {
            url.trim_end_matches(".mp3").rsplit('/').next().unwrap_or("Track").to_string()
        } else {
            title
        };
        out.push((title, full));
    }
    out
}

/// Track name from a URL's last path segment, with percent-encoding
/// decoded ("Chief%20Keef%20-%20Breaking%20Down.mp3" ->
/// "Chief Keef - Breaking Down").
fn decode_url_name(url: &str) -> String {
    let seg = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("Mixtape track");
    seg.trim_end_matches(".mp3")
        .replace("%20", " ")
        .replace("%27", "'")
        .replace("%28", "(")
        .replace("%29", ")")
        .replace("%26", "&")
        .replace("%2B", "+")
        .replace("%22", "\"")
}
fn parse_title(html: &str) -> String {
    // `<title>MixtapeMonkey  Back From The Dead by Chief Keef</title>`
    // (the og:title is a player banner with junk prefixes — avoid it).
    let t = Regex::new(r"(?is)<title>(.*?)</title>").unwrap();
    if let Some(raw) = t
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
    {
        let raw = raw
            .trim_start_matches("MixtapeMonkey")
            .trim()
            .to_string();
        if !raw.is_empty() {
            return raw;
        }
    }
    let og = Regex::new(r#"(?is)<meta property="og:title" content="([^"]+)""#).unwrap();
    og.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| {
            m.as_str()
                .split('|')
                .last()
                .unwrap_or(m.as_str())
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for Mod {
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
        _data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        let url = if track_id.starts_with(BASE) && track_id.contains("/mixtapes/") {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_tracks(&html)
                .first()
                .map(|(_, u)| u.clone())
                .ok_or_else(|| Error::Other("mixtapemonkey: no tracks on tape".into()))?
        };
        let name = decode_url_name(track_id);
        Ok(TrackInfo {
            name,
            codec: CodecFlags::MP3,
            id: Some(url),
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
        let url = if track_id.starts_with(BASE) && track_id.contains("/mixtapes/") {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_tracks(&html)
                .first()
                .map(|(_, u)| u.clone())
                .ok_or_else(|| Error::Other("mixtapemonkey: no tracks on tape".into()))?
        };
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(url),
            file_url_headers: serde_json::Map::new(),
            temp_file_path: None,
            different_codec: None,
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let html = fetch_page(&self.client, album_id).await?;
        let tracks = parse_tracks(&html);
        if tracks.is_empty() {
            return Err(Error::Other("mixtapemonkey: no tracks on tape".into()));
        }
        let title = parse_title(&html);
        let (artist, album) = if let Some(i) = title.rfind(" by ") {
            (
                title[i + 4..].trim().to_string(),
                title[..i].trim().to_string(),
            )
        } else {
            match title.find(" - ") {
                Some(i) => (
                    title[..i].trim().to_string(),
                    title[i + 3..].trim().to_string(),
                ),
                None => ("Unknown Artist".to_string(), title.clone()),
            }
        };
        Ok(AlbumInfo {
            name: if album.is_empty() { title } else { album },
            artist,
            tracks: tracks
                .into_iter()
                .map(|(name, url)| {
                    TrackRef::Full(Box::new(TrackInfo {
                        name,
                        codec: CodecFlags::MP3,
                        id: Some(url),
                        ..Default::default()
                    }))
                })
                .collect(),
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
        _track_id: &str,
        _cover: &CoverOptions,
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        Err(Error::Other("mixtapemonkey: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        // The site search does full-text matching; "artist - title" with
        // the separator matches nothing, so try the full string then each
        // part and merge.
        let mut candidates: Vec<String> = vec![query.replace(" - ", " ")];
        if let Some(idx) = query.find(" - ") {
            candidates.push(query[idx + 3..].to_string());
            candidates.push(query[..idx].to_string());
        }
        let mut out = Vec::new();
        let max = limit.clamp(1, 50) as usize;
        for cand in &candidates {
            let resp = match self
                .client
                .post(format!("{BASE}/mixtapes.inc.php"))
                .form(&[("name", cand.as_str())])
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => return Err(Error::Other(format!("mixtapemonkey search: {e}"))),
            };
            if !resp.status().is_success() {
                return Err(Error::Other(format!(
                    "mixtapemonkey HTTP {}",
                    resp.status()
                )));
            }
            let html = resp
                .text()
                .await
                .map_err(|e| Error::Other(format!("mixtapemonkey read: {e}")))?;
            for (url, title, artist) in parse_search_items(&html) {
                if out.iter().any(|r: &SearchResult| r.result_id == url) {
                    continue;
                }
                out.push(SearchResult {
                    result_id: url,
                    name: Some(title),
                    artists: if artist.is_empty() {
                        None
                    } else {
                        Some(vec![artist])
                    },
                    ..Default::default()
                });
            }
            if out.len() >= max {
                break;
            }
        }
        out.truncate(max);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
