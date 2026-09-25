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

/// Static description of a single Blogspot-hosted site.
///
/// `url_constants` is a list of `(path_segment, is_album)` pairs used by the
/// generic URL decoder. Blogspot post URLs are date based, so these are
/// best-effort hints only (the module sets `url_decoding: Manual`).
#[derive(Debug, Clone)]
pub struct BlogspotConfig {
    pub service_name: &'static str,
    pub host: &'static str,
    pub referer: &'static str,
    pub url_constants: &'static [(&'static str, bool)],
    pub test_url: &'static str,
}

pub fn module_information(cfg: &BlogspotConfig) -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    for (key, is_album) in cfg.url_constants {
        url_constants.insert(
            (*key).to_string(),
            if *is_album {
                DownloadType::album
            } else {
                DownloadType::track
            },
        );
    }
    ModuleInformation {
        service_name: cfg.service_name.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single(cfg.host.to_string()),
        url_constants,
        test_url: Some(cfg.test_url.to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor(cfg: &'static BlogspotConfig) -> Arc<dyn ModuleConstructor> {
    Arc::new(BlogspotConstructor { cfg })
}

#[derive(Debug)]
struct BlogspotConstructor {
    cfg: &'static BlogspotConfig,
}

impl ModuleConstructor for BlogspotConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(BlogspotModule {
            cfg: self.cfg,
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct BlogspotModule {
    cfg: &'static BlogspotConfig,
    controller: ModuleController,
    client: reqwest::Client,
}

fn resolve_url(cfg: &BlogspotConfig, id: &str) -> String {
    if id.starts_with("http://") || id.starts_with("https://") {
        id.to_string()
    } else if id.starts_with('/') {
        format!("https://{}{}", cfg.host, id)
    } else {
        format!("https://{}/{}", cfg.host, id)
    }
}

async fn fetch_page(cfg: &BlogspotConfig, client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", cfg.referer)
        .send()
        .await
        .map_err(|e| Error::Other(format!("{} page fetch: {e}", cfg.service_name)))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "{} HTTP {}",
            cfg.service_name,
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("{} page read: {e}", cfg.service_name)))
}

fn decode_entities_basic(s: &str) -> String {
    s.replace("&#8211;", "-")
        .replace("&#8212;", "-")
        .replace("&#8217;", "'")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&#8216;", "'")
        .replace("&#8220;", "\"")
        .replace("&#8221;", "\"")
        .replace("&#124;", "|")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&nbsp;", " ")
}

fn strip_html_tags(s: &str) -> String {
    let re = Regex::new(r"<[^>]*>").unwrap();
    re.replace_all(s, " ").to_string()
}

/// Extract a Blogger entry title. The Atom/JSON feed exposes it either as a
/// plain string or as `{"type": "text", "$t": "..."}`; both must be handled.
fn entry_title(entry: &Value) -> String {
    let raw = match entry.get("title") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(_)) => entry
            .get("title")
            .and_then(|t| t.get("$t"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    };
    let cleaned = decode_entities_basic(&strip_html_tags(&raw));
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn split_title(title: &str) -> (Option<String>, Option<String>, Option<String>) {
    let year_re = Regex::new(r"\((\d{4})\)").unwrap();
    let title = title.trim();
    if let Some(idx) = title.find(" - ") {
        let artist = title[..idx].trim().to_string();
        let rest = title[idx + 3..].trim().to_string();
        let year = year_re
            .captures(&rest)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let artist = if artist.is_empty() {
            None
        } else {
            Some(artist)
        };
        (artist, Some(rest), year)
    } else {
        let year = year_re
            .captures(title)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let album = if title.is_empty() {
            None
        } else {
            Some(title.to_string())
        };
        (None, album, year)
    }
}

fn parse_feed_entries(root: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let entries = root
        .get("feed")
        .and_then(|f| f.get("entry"))
        .and_then(|e| e.as_array());
    if let Some(entries) = entries {
        for entry in entries {
            let href = entry
                .get("link")
                .and_then(|l| l.as_array())
                .and_then(|links| {
                    links
                        .iter()
                        .find(|l| l.get("rel").and_then(|r| r.as_str()) == Some("alternate"))
                })
                .and_then(|l| l.get("href"))
                .and_then(|h| h.as_str())
                .unwrap_or("");
            let title = entry_title(entry);
            if !href.is_empty() {
                out.push((href.to_string(), title));
            }
        }
    }
    out
}

fn parse_html_entries(html: &str) -> Vec<(String, String)> {
    let re = Regex::new(
        r#"(?s)<h3[^>]*class="[^"]*post-title[^"]*"[^>]*>\s*<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#,
    )
    .unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let href = cap
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let title = cap
            .get(2)
            .map(|m| {
                let cleaned = decode_entities_basic(&strip_html_tags(m.as_str()));
                cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
            })
            .unwrap_or_default();
        if !href.is_empty() {
            out.push((href, title));
        }
    }
    out
}

fn parse_post_title(html: &str) -> String {
    let og = Regex::new(r#"<meta property="og:title" content="([^"]+)""#).unwrap();
    if let Some(c) = og.captures(html) {
        if let Some(m) = c.get(1) {
            return decode_entities_basic(m.as_str().trim());
        }
    }
    let title_re = Regex::new(r"<title>(.*?)</title>").unwrap();
    title_re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| decode_entities_basic(m.as_str().trim())))
        .unwrap_or_else(|| "Unknown".to_string())
}

fn parse_cover(html: &str) -> Option<String> {
    let og = Regex::new(r#"<meta property="og:image" content="([^"]+)""#).unwrap();
    if let Some(c) = og.captures(html) {
        if let Some(m) = c.get(1) {
            let url = m.as_str().trim();
            if !url.is_empty() {
                return Some(url.to_string());
            }
        }
    }
    // UNVERIFIED: "first <img>" fallback may pick a layout/logo image on some themes.
    let img = Regex::new(r#"<img[^>]+src="([^"]+)""#).unwrap();
    img.captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|s| !s.is_empty())
}

fn parse_file_host_link(html: &str) -> Option<String> {
    let re = Regex::new(
        r#"href="(https?://[^"]*(?:mediafire|mega|zippyshare|1fichier|drive\.google|dropbox|pixeldrain|gofile|archive\.org)[^"]*)""#,
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
impl picaro_utils::module::ModuleInterface for BlogspotModule {
    fn name(&self) -> &str {
        self.cfg.service_name
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
        let track_name = data
            .get("__track_name__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(TrackInfo {
            name: track_name.unwrap_or_else(|| format!("{} - {} (FLAC)", artist, album)),
            album: album.clone(),
            album_id: String::new(),
            artists: vec![artist],
            tags: Tags {
                release_date: year.map(|y| format!("{y}-01-01")),
                ..Default::default()
            },
            codec: CodecFlags::FLAC,
            cover_url: cover,
            release_year: year.unwrap_or(0),
            id: Some(track_id.to_string()),
            bit_depth: Some(16),
            sample_rate: Some(44100.0),
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
                different_codec: Some(CodecFlags::FLAC),
            });
        }
        if track_id.starts_with("https://") {
            let mut headers = serde_json::Map::new();
            headers.insert("Referer".to_string(), json!(self.cfg.referer));
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::Url,
                file_url: Some(track_id.to_string()),
                file_url_headers: headers,
                temp_file_path: None,
                different_codec: Some(CodecFlags::FLAC),
            });
        }
        Err(Error::Other(format!(
            "{}: expected direct download URL, got {track_id}",
            self.cfg.service_name
        )))
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let url = resolve_url(self.cfg, album_id);
        let html = fetch_page(self.cfg, &self.client, &url).await?;
        let title = parse_post_title(&html);
        let (artist_opt, album_opt, year) = split_title(&title);
        let artist = artist_opt.unwrap_or_else(|| "Unknown Artist".to_string());
        let album = album_opt.unwrap_or_else(|| title.clone());
        let cover = parse_cover(&html);
        let download_url = parse_file_host_link(&html).ok_or_else(|| {
            Error::Other(format!(
                "{}: file-host download link not found on album page",
                self.cfg.service_name
            ))
        })?;
        Ok(AlbumInfo {
            name: album,
            artist: artist.clone(),
            tracks: vec![TrackRef::Id(download_url)],
            release_year: year
                .as_deref()
                .and_then(|y| y.parse::<i32>().ok())
                .unwrap_or(0),
            artist_id: None,
            id: Some(album_id.to_string()),
            quality: Some("FLAC".to_string()),
            cover_url: cover,
            cover_type: Some(ImageFileType::Jpg),
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        _playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: self.cfg.service_name.to_string(),
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
            module: self.cfg.service_name.to_string(),
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
        Err(Error::Other(format!(
            "{}: no cover for track {track_id}",
            self.cfg.service_name
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
        let feed_url = format!(
            "https://{}/feeds/posts/default?alt=json&q={}",
            self.cfg.host, encoded
        );
        let mut entries: Vec<(String, String)> = Vec::new();
        if let Ok(resp) = self
            .client
            .get(&feed_url)
            .header("Referer", self.cfg.referer)
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(root) = resp.json::<Value>().await {
                    entries = parse_feed_entries(&root);
                }
            }
        }
        if entries.is_empty() {
            let html_url = format!("https://{}/search?q={}", self.cfg.host, encoded);
            if let Ok(resp) = self
                .client
                .get(&html_url)
                .header("Referer", self.cfg.referer)
                .send()
                .await
            {
                if resp.status().is_success() {
                    if let Ok(html) = resp.text().await {
                        entries = parse_html_entries(&html);
                    }
                }
            }
        }

        let mut out: Vec<SearchResult> = Vec::new();
        for (url, title) in entries {
            if out.iter().any(|r| r.result_id == url) {
                continue;
            }
            let (artist, album, year) = split_title(&title);
            out.push(SearchResult {
                result_id: url,
                name: album,
                artists: artist.map(|a| vec![a]),
                year,
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

pub fn register_module(registry: &picaro_utils::ModuleRegistry, cfg: &'static BlogspotConfig) {
    register(registry, module_information(cfg), constructor(cfg));
}
