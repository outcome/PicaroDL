//! Systems of Romance (systemsofromance.com) module.
//!
//! WordPress coldwave/minimal-wave blog. Search via `/blog/?s=<query>`;
//! newer posts embed a direct same-domain `/SOR/<name>.zip` download
//! ("download it here" anchor, plain GET, HTTP 206 + PK zip magic
//! verified). Old posts (2007-2010 era) have no download link and are
//! rejected at fetch time.
//!
//! The zip is exposed as a one-track album so the downloader's album flow
//! grabs the archive and extracts the audio out of it.

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

const SERVICE: &str = "SystemsOfRomance";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://www.systemsofromance.com";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("systemsofromance.com".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://www.systemsofromance.com/blog/?s=kas+product".to_string()),
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
        .map_err(|e| Error::Other(format!("sor fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("sor HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("sor read: {e}")))
}

fn decode_entities(s: &str) -> String {
    let amp = format!("{}amp;", '&');
    let quot = format!("{}quot;", '&');
    let raquo = format!("{}raquo;", '&');
    let laquo = format!("{}laquo;", '&');
    s.replace("&#8211;", "-")
        .replace("&#8212;", "-")
        .replace("&#8217;", "'")
        .replace("&#8220;", "\"")
        .replace("&#8221;", "\"")
        .replace("&#8216;", "'")
        .replace("&#039;", "'")
        .replace(&quot, "\"")
        .replace(&raquo, "")
        .replace(&laquo, "")
        .replace(&amp, "&")
        .trim()
        .to_string()
}

/// Direct `/SOR/<name>.zip` download link on a post page.
fn parse_zip_link(html: &str) -> Option<String> {
    let re = Regex::new(r#"href="(https?://(?:www\.)?systemsofromance\.com/SOR/[^"]+\.zip)""#)
        .unwrap();
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Search results: `<h2 id="post-71"><a href="..." rel="bookmark"
/// title="...">Title</a></h2>`.
fn parse_post_links(html: &str) -> Vec<(String, String)> {
    let re = Regex::new(r#"<h2 id="post-\d+"><a href="([^"]+)"[^>]*>([^<]+)</a></h2>"#).unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let url = cap.get(1).map(|m| m.as_str()).unwrap_or_default().to_string();
        let title = decode_entities(cap.get(2).map(|m| m.as_str()).unwrap_or_default());
        if url.is_empty() || out.iter().any(|(u, _)| *u == url) {
            continue;
        }
        out.push((url, title));
    }
    out
}

fn parse_title(html: &str) -> String {
    let t = Regex::new(r"(?is)<title>(.*?)</title>").unwrap();
    let raw = t
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    decode_entities(
        raw.split('|')
            .next()
            .unwrap_or(&raw)
            .trim(),
    )
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
        if parse_zip_link(&html).is_none() {
            return Err(Error::Other("sor: post has no direct zip".into()));
        }
        let title = parse_title(&html);
        Ok(TrackInfo {
            name: if title.is_empty() {
                "Systems of Romance release".to_string()
            } else {
                title
            },
            codec: CodecFlags::MP3,
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
        let zip = if track_id.starts_with("https://")
            && track_id.contains("/SOR/")
            && track_id.ends_with(".zip")
        {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_zip_link(&html).ok_or_else(|| Error::Other("sor: post has no direct zip".into()))?
        };
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(zip),
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
        if parse_zip_link(&html).is_none() {
            return Err(Error::Other("sor: post has no direct zip".into()));
        }
        let title = parse_title(&html);
        Ok(AlbumInfo {
            name: if title.is_empty() {
                "Systems of Romance release".to_string()
            } else {
                title
            },
            artist: "Systems of Romance".to_string(),
            tracks: vec![TrackRef::Id(album_id.to_string())],
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
        Err(Error::Other("sor: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let url = format!("{BASE}/blog/?s={}", enc(query));
        let html = fetch_page(&self.client, &url).await?;
        let mut out = Vec::new();
        for (post, title) in parse_post_links(&html) {
            out.push(SearchResult {
                result_id: post,
                name: Some(title),
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
