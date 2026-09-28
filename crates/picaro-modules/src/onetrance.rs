//! 1Trance (1trance.org) module.
//!
//! Drupal trance netlabel index. Search uses the exposed-filter endpoint
//! `/f?s=<query>&t=&page=1`; releases live at `/node/<id>/<slug>`. MP3 320
//! releases embed a direct `tune.skin/<token>/<file>.mp3` link (plain GET,
//! no JS, verified audio/mpeg + ID3); FLAC-only releases route to rapidgator
//! and are skipped.

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

const SERVICE: &str = "OneTrance";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://1trance.org";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("1trance.org".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://1trance.org/f?s=psytrance&t=&page=1".to_string()),
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

fn title_from_slug(slug: &str) -> String {
    clean_title(
        &slug
            .trim_end_matches("-int")
            .replace('-', " "),
    )
}

/// Strip trailing scene/quality markers ("SINGLE WEB FLAC 2026 SCMT INT").
/// This site is 100% scene releases, so the cleaner is aggressive: trailing
/// years, format markers, all-caps/lowercase group tags (up to 4), and long
/// digit runs (UPCs) all go. Keeps at least 2 words.
fn clean_title(t: &str) -> String {
    let digit_run = |s: &str| s.len() >= 5 && s.chars().all(|c| c.is_ascii_digit());
    let mut tokens: Vec<String> = t.split_whitespace().map(|s| s.to_string()).collect();
    tokens.retain(|tok| !digit_run(tok));
    let mut pops = 0;
    while tokens.len() > 2 && pops < 4 {
        let last = tokens.last().unwrap();
        let low = last.to_ascii_lowercase();
        let is_year = last.len() == 4 && last.chars().all(|c| c.is_ascii_digit());
        let is_marker = matches!(
            low.as_str(),
            "single" | "web" | "flac" | "mp3" | "ep" | "cdm" | "cd" | "int" | "vbr"
                | "24bit" | "16bit" | "320" | "vinyl" | "remastered"
        );
        let is_tag = last.len() >= 2
            && last.len() <= 8
            && last
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_lowercase());
        if is_year || is_marker || is_tag {
            tokens.pop();
            pops += 1;
        } else {
            break;
        }
    }
    tokens.join(" ")
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("onetrance fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("onetrance HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("onetrance read: {e}")))
}

fn parse_node_links(html: &str) -> Vec<(String, String)> {
    // /search/node anchors: link text is the release title. Label `<td>`
    // links (no year in the slug) are dropped.
    let re = Regex::new(
        r#"<a[^>]*href="((?:https://1trance\.org)?/node/\d+/[^"]+)"[^>]*>(.*?)</a>"#,
    )
    .unwrap();
    let year = Regex::new(r"\d{4}").unwrap();
    let mut out: Vec<(String, String)> = Vec::new();
    for cap in re.captures_iter(html) {
        let raw = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        let url = if raw.starts_with("http") {
            raw.to_string()
        } else {
            format!("{BASE}{raw}")
        };
        let title = cap
            .get(2)
            .map(|m| {
                let t = Regex::new(r"<[^>]*>").unwrap().replace_all(m.as_str(), " ");
                // Link text is scene-style ("Z-LEAF_-_Hidden_Temple-..."):
                // underscores/dashes to spaces so the resolver's token
                // overlap sees separate words.
                let t = t.replace('_', " ").replace('-', " ");
                decode_entities(&t.split_whitespace().collect::<Vec<_>>().join(" "))
            })
            .unwrap_or_default();
        let slug = url.rsplit('/').next().unwrap_or("");
        if !year.is_match(slug) || out.iter().any(|(u, _)| *u == url) {
            continue;
        }
        out.push((url, title));
    }
    out
}

fn decode_entities(s: &str) -> String {
    let amp = format!("{}amp;", '&');
    let quot = format!("{}quot;", '&');
    let apos = format!("{}#039;", '&');
    s.replace(&amp, "&")
        .replace(&quot, "\"")
        .replace(&apos, "'")
        .trim()
        .to_string()
}

/// Direct MP3 link (tune.skin) on a release page. FLAC-only releases embed
/// only a rapidgator link and return None here. The link appears both as
/// `href="..."` and as `<source src="...">` (audio preview element).
fn parse_mp3_link(html: &str) -> Option<String> {
    let re =
        Regex::new(r#"(?:href|src)="(https://tune\.skin/[A-Za-z0-9]+/[^"]+\.mp3)""#).unwrap();
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn parse_title(html: &str) -> String {
    let t = Regex::new(r"(?is)<title>(.*?)</title>").unwrap();
    let raw = t
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    // Node <title> tags carry the site's meta description
    // ("... | 1trance - download descargar télécharger trance music ...").
    // Cut at the site junk; the release name is what precedes it.
    let cut = raw
        .find("descargar")
        .or_else(|| raw.find("télécharger"))
        .unwrap_or(raw.len());
    let raw = raw[..cut].trim().to_string();
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
        let html = fetch_page(&self.client, track_id).await?;
        let Some(mp3) = parse_mp3_link(&html) else {
            return Err(Error::Other(
                "onetrance: FLAC-only release (rapidgator)".into(),
            ));
        };
        let title = parse_title(&html);
        let slug = track_id.rsplit('/').next().unwrap_or(track_id);
        let name = if title.is_empty() {
            title_from_slug(slug)
        } else {
            clean_title(&title)
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
        if track_id.starts_with("https://tune.skin/") {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::Url,
                file_url: Some(track_id.to_string()),
                file_url_headers: serde_json::Map::new(),
                temp_file_path: None,
                different_codec: None,
            });
        }
        let html = fetch_page(&self.client, track_id).await?;
        let Some(mp3) = parse_mp3_link(&html) else {
            return Err(Error::Other(
                "onetrance: FLAC-only release (rapidgator)".into(),
            ));
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
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let html = fetch_page(&self.client, album_id).await?;
        let Some(mp3) = parse_mp3_link(&html) else {
            return Err(Error::Other(
                "onetrance: FLAC-only release (rapidgator)".into(),
            ));
        };
        let title = parse_title(&html);
        let slug = album_id.rsplit('/').next().unwrap_or(album_id);
        let name = if title.is_empty() {
            title_from_slug(slug)
        } else {
            clean_title(&title)
        };
        Ok(AlbumInfo {
            name,
            tracks: vec![TrackRef::Id(mp3)],
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
        Err(Error::Other("onetrance: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let url = format!("{BASE}/search/node?keys={}", enc(query));
        let html = fetch_page(&self.client, &url).await?;
        let mut out = Vec::new();
        for (node, title) in parse_node_links(&html) {
            let name = if title.is_empty() {
                let slug = node.rsplit('/').next().unwrap_or("").to_string();
                title_from_slug(&slug)
            } else {
                clean_title(&title)
            };
            out.push(SearchResult {
                result_id: node,
                name: Some(name),
                ..Default::default()
            });
        }
        out.truncate(limit.clamp(1, 50) as usize);
        Ok(out)
    }
}

fn enc(s: &str) -> String {
    // Drupal's node search matches titles joined by literal dashes;
    // spaces become dashes, and dashes stay literal (never %2D).
    utf8_percent_encode(&s.replace(' ', "-"), NON_ALPHANUMERIC)
        .to_string()
        .replace("%2D", "-")
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
