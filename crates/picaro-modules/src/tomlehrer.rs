//! Tom Lehrer Songs (tomlehrersongs.com) module.
//!
//! WordPress site hosting Tom Lehrer's public-domain catalogue. Search
//! via `/?s=<query>`; song pages embed `Audio File: <a href="...mp3">`
//! anchors (plus `<source type="audio/mpeg" src="...mp3">` elements with
//! a `?_=N` cache-buster). Direct MP3 downloads, ID3 + HTTP 206 verified.

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

const SERVICE: &str = "TomLehrer";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://tomlehrersongs.com";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("tomlehrersongs.com".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://tomlehrersongs.com/?s=the+elements".to_string()),
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

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("tomlehrer fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("tomlehrer HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("tomlehrer read: {e}")))
}

fn decode_entities(s: &str) -> String {
    let amp = format!("{}amp;", '&');
    s.replace("&#8211;", "-")
        .replace("&#8217;", "'")
        .replace("&#039;", "'")
        .replace(&amp, "&")
        .trim()
        .to_string()
}

/// Direct MP3 on a song page: prefer the explicit download anchor, fall
/// back to the `<source>` element (strip the `?_=N` cache-buster).
fn parse_mp3_link(html: &str) -> Option<String> {
    let dl = Regex::new(r#"Audio File: <a href="([^"]+\.mp3)""#).unwrap();
    if let Some(c) = dl.captures(html) {
        if let Some(m) = c.get(1) {
            return Some(abs(m.as_str()));
        }
    }
    let src = Regex::new(r#"<source type="audio/mpeg" src="([^"?]+\.mp3)""#).unwrap();
    src.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| abs(m.as_str()))
}

fn abs(raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else if raw.starts_with('/') {
        format!("{BASE}{raw}")
    } else {
        format!("{BASE}/{raw}")
    }
}

/// Search results: `<h2 class="entry-title fusion-post-title"><a
/// href="...">Title</a></h2>`.
fn parse_result_links(html: &str) -> Vec<(String, String)> {
    let re = Regex::new(r#"entry-title[^>]*><a href="([^"]+)"[^>]*>([^<]+)</a>"#).unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let url = abs(cap.get(1).map(|m| m.as_str()).unwrap_or_default());
        let title = decode_entities(cap.get(2).map(|m| m.as_str()).unwrap_or_default());
        if !url.contains("tomlehrersongs.com") || out.iter().any(|(u, _)| *u == url) {
            continue;
        }
        out.push((url, title));
    }
    out
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
        let mp3 = if track_id.ends_with(".mp3") {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_mp3_link(&html)
                .ok_or_else(|| Error::Other("tomlehrer: no mp3 on song page".into()))?
        };
        let name = if track_id.ends_with(".mp3") {
            track_id
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("Song")
                .trim_end_matches(".mp3")
                .replace(['-', '_'], " ")
        } else {
            let slug = track_id
                .trim_start_matches(BASE)
                .trim_matches('/')
                .trim_end_matches('/');
            let cleaned = slug.rsplit('/').next().unwrap_or("Song");
            let words: Vec<String> = cleaned
                .split('-')
                .map(|w| {
                    let mut c = w.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                })
                .collect();
            words.join(" ")
        };
        Ok(TrackInfo {
            name,
            codec: CodecFlags::MP3,
            id: Some(mp3),
            tags: Tags {
                track_url: Some(track_id.to_string()),
                ..Default::default()
            },
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
        let mp3 = if track_id.ends_with(".mp3") {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_mp3_link(&html)
                .ok_or_else(|| Error::Other("tomlehrer: no mp3 on song page".into()))?
        };
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(mp3),
            file_url_headers: serde_json::Map::new(),
            temp_file_path: None,
            different_codec: None,
        })
    }

    async fn get_album_info(
        &self,
        _album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.to_string(),
            ability: "album".to_string(),
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
        Err(Error::Other("tomlehrer: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let url = format!("{BASE}/?s={}", enc(query));
        let html = fetch_page(&self.client, &url).await?;
        let mut out = Vec::new();
        for (song_url, title) in parse_result_links(&html) {
            out.push(SearchResult {
                result_id: song_url,
                name: Some(title),
                artists: Some(vec!["Tom Lehrer".to_string()]),
                ..Default::default()
            });
        }
        out.truncate(limit.clamp(1, 50) as usize);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
