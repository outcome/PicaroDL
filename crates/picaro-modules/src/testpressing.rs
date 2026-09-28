//! TestPressing (testpressing.org) module.
//!
//! Electronic-music mix site with no server search. The full mix index
//! lives in `sitemap.xml` (562 mix URLs); this module fetches the
//! sitemap, filters slugs against the query tokens, and extracts the
//! direct mix MP3 from the mix page's serialized Nuxt payload
//! (`file:"\u002Fassets\u002Fmixes\u002F....mp3"` — every `/` is
//! unicode-escaped; HTTP 206 + MPEG frame sync verified).

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

const SERVICE: &str = "TestPressing";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://testpressing.org";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("testpressing.org".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://testpressing.org/sitemap.xml".to_string()),
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
        .map_err(|e| Error::Other(format!("testpressing fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "testpressing HTTP {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("testpressing read: {e}")))
}

/// Mix titles from the sitemap: `<loc>https://www.testpressing.org/mix/561-femdelic</loc>`
/// -> URL + title derived from the slug.
fn parse_sitemap(xml: &str) -> Vec<(String, String)> {
    let re = Regex::new(r"<loc>https://(?:www\.)?testpressing\.org/mix/([^<]+)</loc>").unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(xml) {
        let slug = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        if slug.is_empty() {
            continue;
        }
        // Strip leading digits ("561-femdelic" -> "femdelic");
        // old format "059cosmospring-affair" -> "cosmospring-affair".
        let title_slug = slug
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches('-');
        let title = title_slug.replace(['-', '_'], " ");
        let url = format!("{BASE}/mix/{slug}");
        if !out.iter().any(|(u, _)| *u == url) {
            out.push((url, title));
        }
    }
    out
}

/// `file:"\u002Fassets\u002Fmixes\u002F....mp3"` from the Nuxt payload.
/// Every `/` is escaped as `\u002F`; the path is relative to the origin.
fn parse_file_url(html: &str) -> Option<String> {
    let re = Regex::new(r#"file:"((?:\\u002F|/)[^"]+\.mp3)""#).unwrap();
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| {
            let raw = m.as_str();
            if raw.starts_with('/') {
                format!("{BASE}{raw}")
            } else {
                format!("{BASE}/{}", raw.replace("\\u002F", "/"))
            }
        })
}

fn parse_title(html: &str) -> String {
    let t = Regex::new(r"(?is)<title>(.*?)</title>").unwrap();
    let raw = t
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    raw.split('|')
        .next()
        .unwrap_or(&raw)
        .trim()
        .to_string()
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
        if track_id.starts_with(BASE) && track_id.ends_with(".mp3") {
            let slug = track_id
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches('-');
            return Ok(TrackInfo {
                name: slug.replace(['-', '_'], " "),
                codec: CodecFlags::MP3,
                id: Some(track_id.to_string()),
                ..Default::default()
            });
        }
        let html = fetch_page(&self.client, track_id).await?;
        let mp3 = parse_file_url(&html)
            .ok_or_else(|| Error::Other("testpressing: no file on mix page".into()))?;
        let t = parse_title(&html);
        Ok(TrackInfo {
            name: if t.is_empty() { "TestPressing mix".to_string() } else { t },
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
        let mp3 = if track_id.starts_with(BASE) && track_id.ends_with(".mp3") {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_file_url(&html)
                .ok_or_else(|| Error::Other("testpressing: no file on mix page".into()))?
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
        Err(Error::Other("testpressing: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let xml = fetch_page(&self.client, &format!("{BASE}/sitemap.xml")).await?;
        let tokens: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .filter(|t| t.len() > 2)
            .map(|t| t.to_string())
            .collect();
        let mut out = Vec::new();
        for (url, title) in parse_sitemap(&xml) {
            let hay = title.to_lowercase();
            if !tokens.is_empty() && !tokens.iter().all(|t| hay.contains(t)) {
                continue;
            }
            out.push(SearchResult {
                result_id: url,
                name: Some(title),
                ..Default::default()
            });
            if out.len() >= limit.clamp(1, 50) as usize {
                break;
            }
        }
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
