//! TechnicalDeathMetal (technicaldeathmetal.org) module.
//!
//! WordPress metal blog. Search via `?s=<query>`; releases live at flat
//! `/<slug>/` permalinks. A subset of posts embed direct `vk.com` document
//! links (`vk.com/s/v1/doc/<token>` or `vk.com/doc-<id>_<id>?hash=...&dl=...`)
//! that serve a 7z archive on a plain GET — no JS, no captcha, verified 7z
//! magic bytes. Posts pointing only at mega.nz / cloud.mail.ru / icedrive
//! are skipped.
//!
//! The 7z archive is exposed as a one-track album so the downloader's album
//! flow downloads it, extracts it, and picks the audio files out.

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

const SERVICE: &str = "TechnicalDeathMetal";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://technicaldeathmetal.org";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("technicaldeathmetal.org".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://technicaldeathmetal.org/?s=death".to_string()),
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

/// Direct 7z document link on a post page (vk.com hosts it on a plain GET).
fn parse_vk_link(html: &str) -> Option<String> {
    let re1 = Regex::new(r#"href="(https://vk\.com/s/v1/doc/[^"]+)""#).unwrap();
    if let Some(c) = re1.captures(html) {
        if let Some(m) = c.get(1) {
            return Some(m.as_str().to_string());
        }
    }
    let re2 = Regex::new(r#"href="(https://vk\.com/doc-\d+_\d+\?[^"]*)""#).unwrap();
    re2.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn title_from_slug(slug: &str) -> String {
    clean_title(&slug.replace('-', " "))
}

/// Strip trailing scene/quality markers so the resolver's text-overlap
/// score isn't diluted and file/album names read clean.
fn clean_title(t: &str) -> String {
    let mut tokens: Vec<String> = t.split_whitespace().map(|s| s.to_string()).collect();
    while tokens.len() > 1 {
        let last = tokens.last().unwrap();
        let low = last.to_ascii_lowercase();
        let is_year = last.len() == 4 && last.chars().all(|c| c.is_ascii_digit());
        let is_marker = matches!(
            low.as_str(),
            "single" | "web" | "flac" | "mp3" | "ep" | "cdm" | "cd" | "int" | "vbr"
                | "24bit" | "16bit" | "320" | "vinyl" | "remastered"
        );
        let is_caps_tag = last.len() >= 2
            && last.len() <= 8
            && last
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if is_year || is_marker || is_caps_tag {
            tokens.pop();
        } else {
            break;
        }
    }
    tokens.join(" ")
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

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("tdm fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("tdm HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("tdm read: {e}")))
}

fn parse_post_links(html: &str) -> Vec<String> {
    let re = Regex::new(r#"href="(https://technicaldeathmetal\.org/[a-z0-9-]+/)""#).unwrap();
    let nav = Regex::new(r"^/(feed|wp-json|in-search-of-releases|comments)/?$").unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        if let Some(m) = cap.get(1) {
            let url = m.as_str().to_string();
            let path = url.trim_start_matches(BASE).trim_matches('/');
            if nav.is_match(&format!("/{path}/")) {
                continue;
            }
            // Real releases carry a 4-digit year in the slug; nav/technical
            // pages do not.
            if !Regex::new(r"\d{4}").unwrap().is_match(path) {
                continue;
            }
            if !out.contains(&url) {
                out.push(url);
            }
        }
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
        let html = fetch_page(&self.client, track_id).await?;
        let Some(_vk) = parse_vk_link(&html) else {
            return Err(Error::Other("tdm: no direct vk.com archive".into()));
        };
        let title = parse_title(&html);
        let slug = track_id
            .trim_start_matches(BASE)
            .trim_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(track_id);
        Ok(TrackInfo {
            name: if title.is_empty() {
                title_from_slug(slug)
            } else {
                title
            },
            codec: CodecFlags::FLAC,
            id: Some(track_id.to_string()),
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
        if track_id.starts_with("https://vk.com/") {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::Url,
                file_url: Some(track_id.to_string()),
                file_url_headers: serde_json::Map::new(),
                temp_file_path: None,
                different_codec: Some(CodecFlags::FLAC),
            });
        }
        let html = fetch_page(&self.client, track_id).await?;
        let Some(vk) = parse_vk_link(&html) else {
            return Err(Error::Other("tdm: no direct vk.com archive".into()));
        };
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(vk),
            file_url_headers: serde_json::Map::new(),
            temp_file_path: None,
            different_codec: Some(CodecFlags::FLAC),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let html = fetch_page(&self.client, album_id).await?;
        let Some(_vk) = parse_vk_link(&html) else {
            return Err(Error::Other("tdm: no direct vk.com archive".into()));
        };
        let title = parse_title(&html);
        let slug = album_id
            .trim_start_matches(BASE)
            .trim_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(album_id);
        Ok(AlbumInfo {
            name: if title.is_empty() {
                title_from_slug(slug)
            } else {
                title
            },
            tracks: vec![TrackRef::Id(album_id.to_string())],
            quality: Some("FLAC".to_string()),
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
        Err(Error::Other("tdm: no cover".into()))
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
        for post in parse_post_links(&html) {
            let slug = post
                .trim_start_matches(BASE)
                .trim_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
            out.push(SearchResult {
                result_id: post,
                name: Some(title_from_slug(&slug)),
                ..Default::default()
            });
        }
        out.truncate(limit.clamp(1, 50) as usize);
        Ok(out)
    }
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
