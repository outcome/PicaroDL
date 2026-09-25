//! Reusable module template for DLE (DataLife Engine) blog-style sites.
//!
//! A site module is created by declaring a `static DleBlogConfig` and
//! delegating `module_information` / `constructor` / `register_module` to the
//! helpers in this file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use regex::Regex;
use serde_json::{json, Value};

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

// Cloudflare-aware fetch helper. Declared here (rather than in lib.rs, which the
// integrator owns) so this template stays self-contained. The integrator may
// move it to lib.rs as `pub mod cf_http;` and update the references below.
#[path = "cf_http.rs"]
pub mod cf_http;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Configuration describing a single DLE blog site.
pub struct DleBlogConfig {
    pub service_name: &'static str,
    pub host: &'static str,
    pub referer: &'static str,
    pub title_suffix: &'static str,
    pub no_image_marker: &'static str,
    pub get_host: Option<&'static str>,
    pub hash_decode: bool,
    pub url_constants: &'static [(&'static str, bool)],
    pub test_url: &'static str,
}

impl std::fmt::Debug for DleBlogConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DleBlogConfig")
            .field("service_name", &self.service_name)
            .field("host", &self.host)
            .field("referer", &self.referer)
            .field("title_suffix", &self.title_suffix)
            .field("no_image_marker", &self.no_image_marker)
            .field("get_host", &self.get_host)
            .field("hash_decode", &self.hash_decode)
            .field("test_url", &self.test_url)
            .finish()
    }
}

pub fn module_information(cfg: &DleBlogConfig) -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    for (key, is_track) in cfg.url_constants {
        url_constants.insert(
            (*key).to_string(),
            if *is_track {
                DownloadType::track
            } else {
                DownloadType::album
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

pub fn constructor(cfg: &'static DleBlogConfig) -> Arc<dyn ModuleConstructor> {
    Arc::new(DleBlogConstructor { cfg })
}

#[derive(Debug)]
struct DleBlogConstructor {
    cfg: &'static DleBlogConfig,
}

impl ModuleConstructor for DleBlogConstructor {
    fn construct(&self, _controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(DleBlogModule {
            cfg: self.cfg,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct DleBlogModule {
    cfg: &'static DleBlogConfig,
    client: reqwest::Client,
}

fn decode_hash_twice(hash: &str) -> Result<String> {
    let once = base64::engine::general_purpose::STANDARD
        .decode(hash.trim())
        .map_err(|e| Error::Other(format!("dle hash decode (1): {e}")))?;
    let once_str =
        String::from_utf8(once).map_err(|e| Error::Other(format!("dle hash utf8 (1): {e}")))?;
    let twice = base64::engine::general_purpose::STANDARD
        .decode(once_str.trim())
        .map_err(|e| Error::Other(format!("dle hash decode (2): {e}")))?;
    String::from_utf8(twice).map_err(|e| Error::Other(format!("dle hash utf8 (2): {e}")))
}

async fn fetch_album_page(
    client: &reqwest::Client,
    cfg: &DleBlogConfig,
    url: &str,
) -> Result<String> {
    cf_http::fetch(client, url, cfg.referer)
        .await
        .map_err(|e| Error::Other(format!("{} album fetch: {e}", cfg.service_name)))
}

fn parse_album_title(cfg: &DleBlogConfig, html: &str) -> String {
    let re = Regex::new(r"(?s)<title>(.*?)</title>").unwrap();
    let raw = re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str()))
        .unwrap_or("Unknown");
    let mut title = raw.trim().to_string();
    if !cfg.title_suffix.is_empty() {
        if let Some(idx) = title.find(cfg.title_suffix) {
            title = title[..idx].trim().to_string();
        } else {
            title = title.trim_end_matches(cfg.title_suffix).trim().to_string();
        }
    }
    title
        .replace("&#8211;", "-")
        .replace("&ndash;", "-")
        .replace("&#124;", "|")
        .replace("&amp;", "&")
}

fn parse_album_meta(
    cfg: &DleBlogConfig,
    html: &str,
) -> (String, String, String, Option<String>, Option<i32>) {
    let title = parse_album_title(cfg, html);
    let parts: Vec<&str> = title.splitn(2, " - ").collect();
    let (artist, album) = if parts.len() == 2 {
        (parts[0].to_string(), parts[1].to_string())
    } else {
        ("Unknown Artist".to_string(), title.clone())
    };
    let cover_re = Regex::new(r#"<meta property="og:image" content="([^"]+)""#).unwrap();
    let cover = cover_re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
    let year_re = Regex::new(r"\((\d{4})\)").unwrap();
    let year = year_re
        .captures(&title)
        .and_then(|c| c.get(1).map(|m| m.as_str().parse::<i32>().ok()))
        .flatten();
    (artist, album, title, cover, year)
}

fn parse_download_id(cfg: &DleBlogConfig, html: &str) -> Option<String> {
    if let Some(get_host) = cfg.get_host {
        let pattern = format!(
            r#"href=['"]https://get\.{}\/?hash=([^'"\s]+)['"]"#,
            regex::escape(get_host)
        );
        if let Ok(re) = Regex::new(&pattern) {
            if let Some(c) = re.captures(html) {
                if let Some(m) = c.get(1) {
                    return Some(m.as_str().to_string());
                }
            }
        }
    }
    parse_direct_link(html)
}

fn parse_direct_link(html: &str) -> Option<String> {
    let primary = Regex::new(
        r#"(?s)<a[^>]*href="(https?://[^"]+)"[^>]*>[^<]*(?:<[^>]*>[^<]*)*?FLAC\s*(?:<[^>]*>[^<]*)*?</a>"#,
    )
    .ok()?;
    if let Some(c) = primary.captures(html) {
        if let Some(m) = c.get(1) {
            return Some(m.as_str().to_string());
        }
    }
    let titled = Regex::new(r#"href="(https?://[^"]+)"[^>]*title="[^"]*FLAC[^"]*""#).ok()?;
    if let Some(c) = titled.captures(html) {
        if let Some(m) = c.get(1) {
            return Some(m.as_str().to_string());
        }
    }
    // Open (captcha-free) file hosts resolved by picaro-downloader::hosters.
    // Preferred over filecrypt below, which is captcha gated.
    let open_hosts = Regex::new(
        r#"(?i)href="(https?://[^"]*(?:mediafire\.com|disk\.yandex|yadi\.sk|1fichier\.com|pixeldrain\.com|drive\.google\.com|dropbox\.com|gofile\.io|catbox\.moe|litterbox\.catbox\.moe|transfer\.sh|file\.io|tmpfiles\.org)[^"]*)""#,
    )
    .ok()?;
    if let Some(c) = open_hosts.captures(html) {
        if let Some(m) = c.get(1) {
            return Some(m.as_str().to_string());
        }
    }
    let filecrypt = Regex::new(r#"href="(https?://filecrypt[^"]+)""#).ok()?;
    if let Some(c) = filecrypt.captures(html) {
        if let Some(m) = c.get(1) {
            return Some(m.as_str().to_string());
        }
    }
    None
}

fn push_result(
    out: &mut Vec<SearchResult>,
    seen: &mut HashSet<String>,
    url: String,
    title: &str,
    image_url: Option<String>,
) {
    if url.is_empty() || title.is_empty() || !seen.insert(url.clone()) {
        return;
    }
    let year_re = Regex::new(r"\((\d{4})\)").unwrap();
    let (artist, album, year) = if let Some(idx) = title.find(" - ") {
        let a = title[..idx].to_string();
        let rest = &title[idx + 3..];
        let y = year_re
            .captures(rest)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        (Some(a), Some(rest.to_string()), y)
    } else {
        let y = year_re
            .captures(title)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        (None, Some(title.to_string()), y)
    };
    out.push(SearchResult {
        result_id: url,
        name: album,
        artists: artist.map(|a| vec![a]),
        year,
        image_url,
        ..Default::default()
    });
}

fn looks_like_title(s: &str) -> bool {
    let t = s.trim();
    if t.len() < 4 || t.len() > 200 {
        return false;
    }
    let low = t.to_lowercase();
    for bad in [
        "read more",
        "continue",
        "подробнее",
        "читать",
        "download",
        "comments",
        "next",
        "previous",
    ] {
        if low.starts_with(bad) {
            return false;
        }
    }
    true
}

fn decode_entities(s: &str) -> String {
    s.replace("&#8211;", "-")
        .replace("&#8212;", "-")
        .replace("&ndash;", "-")
        .replace("&mdash;", "-")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .replace("&#124;", "|")
}

fn slug_to_title(url: &str) -> String {
    let base = url.split(|c| c == '?' || c == '#').next().unwrap_or(url);
    let seg = base.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let seg = seg.trim_end_matches(".html");
    let seg = seg.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-');
    seg.replace('-', " ").trim().to_string()
}

fn parse_search_results(cfg: &DleBlogConfig, html: &str) -> Vec<SearchResult> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let host = regex::escape(cfg.host);
    let tag_re = Regex::new(r"<[^>]*>").unwrap();
    let attr_re = Regex::new(r#"(?:alt|title)="([^"]+)""#).unwrap();
    let anchor_re = Regex::new(&format!(
        r#"(?s)<a[^>]+href="(https?://{host}(?:/[a-z0-9_\-]+/[a-z0-9_\-]+|\/[a-z0-9_\-]+/\d+-[^"\s]+\.html|\/\d+-[^"\s]+\.html|\/?\?p=\d+))"[^>]*>(.*?)</a>"#
    ))
    .unwrap();
    for cap in anchor_re.captures_iter(html) {
        let url = cap
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        if url.is_empty() || seen.contains(&url) {
            continue;
        }
        let inner = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let text = tag_re.replace_all(inner, " ").trim().to_string();
        let mut title = if looks_like_title(&text) {
            text
        } else {
            String::new()
        };
        if title.is_empty() {
            if let Some(end) = cap.get(0).map(|m| m.end()) {
                let window: String = html[end..].chars().take(800).collect();
                if let Some(c) = attr_re.captures(&window) {
                    let t = c
                        .get(1)
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default();
                    if looks_like_title(&t) {
                        title = t;
                    }
                }
            }
        }
        if title.is_empty() {
            title = slug_to_title(&url);
        }
        if title.is_empty() {
            continue;
        }
        let title = decode_entities(&title);
        push_result(&mut out, &mut seen, url, &title, None);
    }

    if out.is_empty() {
        if let (Ok(item_re), Ok(title_re), Ok(cover_re)) = (
            Regex::new(r#"(?s)<li class="tcarusel-item[^"]*">.*?</li>"#),
            Regex::new(
                r#"(?s)<div class="tcarusel-item-title">.*?href="([^"]+)"[^>]*>([^<]+)</a>"#,
            ),
            Regex::new(
                r#"(?s)<div class="tcarusel-item-image">.*?<a href="[^"]*"><img src="([^"]+)""#,
            ),
        ) {
            for cap in item_re.captures_iter(html) {
                let block = cap.get(0).map(|m| m.as_str()).unwrap_or("");
                if let Some(tc) = title_re.captures(block) {
                    let url = tc
                        .get(1)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                    let title = tc
                        .get(2)
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default();
                    let image_url = cover_re.captures(block).and_then(|c| {
                        let src = c.get(1).map(|m| m.as_str().to_string())?;
                        if !cfg.no_image_marker.is_empty() && src.contains(cfg.no_image_marker) {
                            None
                        } else {
                            Some(src)
                        }
                    });
                    push_result(&mut out, &mut seen, url, &title, image_url);
                }
            }
        }
    }

    out
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for DleBlogModule {
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
        let (album, artist, cover, year) = match data.get("__album_meta__").cloned() {
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
        };
        let track_name = data
            .get("__track_name__")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let name = match track_name {
            Some(n) if !n.is_empty() => n,
            _ => format!("{artist} - {album} (FLAC)"),
        };
        Ok(TrackInfo {
            name,
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
        let mut headers = serde_json::Map::new();
        headers.insert("Referer".to_string(), json!(self.cfg.referer));

        if Path::new(track_id).exists() {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::TempFilePath,
                file_url: None,
                file_url_headers: headers,
                temp_file_path: Some(PathBuf::from(track_id)),
                different_codec: Some(CodecFlags::FLAC),
            });
        }

        let direct_url = if track_id.starts_with("https://") {
            track_id.to_string()
        } else if self.cfg.hash_decode {
            decode_hash_twice(track_id)?
        } else {
            return Err(Error::Other(format!(
                "{}: expected direct download URL, got {track_id}",
                self.cfg.service_name
            )));
        };

        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(direct_url),
            file_url_headers: headers,
            temp_file_path: None,
            different_codec: Some(CodecFlags::FLAC),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let url = if album_id.starts_with("https://") {
            album_id.to_string()
        } else {
            format!("https://{}/{}", self.cfg.host, album_id)
        };
        let html = fetch_album_page(&self.client, self.cfg, &url).await?;
        let (artist, album, _title, cover, year) = parse_album_meta(self.cfg, &html);
        let download_id = parse_download_id(self.cfg, &html).ok_or_else(|| {
            Error::Other(format!(
                "{}: FLAC download link not found on album page",
                self.cfg.service_name
            ))
        })?;
        Ok(AlbumInfo {
            name: album,
            artist: artist.clone(),
            tracks: vec![TrackRef::Id(download_id)],
            release_year: year.unwrap_or(0),
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
        let mut all = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let max_pages = ((limit as usize + 19) / 20).min(5);
        for page in 0..max_pages {
            let encoded = utf8_percent_encode(query, NON_ALPHANUMERIC).to_string();
            let url = format!(
                "{}index.php?do=search&subaction=search&story={}&search_start={}&result_from={}",
                self.cfg.referer,
                encoded,
                page,
                page * 20 + 1
            );
            let html = match cf_http::fetch(&self.client, &url, self.cfg.referer).await {
                Ok(html) => html,
                Err(e) => {
                    tracing::debug!("{} search fetch failed: {e}", self.cfg.service_name);
                    break;
                }
            };
            let results = parse_search_results(self.cfg, &html);
            if results.is_empty() {
                break;
            }
            for r in results {
                if seen.insert(r.result_id.clone()) {
                    all.push(r);
                }
            }
            if all.len() >= limit as usize {
                break;
            }
        }
        all.truncate(limit as usize);
        Ok(all)
    }
}
