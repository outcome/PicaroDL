use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::dle_blog::cf_http;
use crate::registry::register;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[derive(Debug)]
pub struct WpBlogConfig {
    pub service_name: &'static str,
    pub host: &'static str,
    pub referer: &'static str,
    pub no_image_marker: &'static str,
    pub url_constants: &'static [(&'static str, bool)],
    pub test_url: &'static str,
}

pub fn module_information(cfg: &WpBlogConfig) -> ModuleInformation {
    ModuleInformation {
        service_name: cfg.service_name.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single(cfg.host.to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            for (name, is_album) in cfg.url_constants.iter() {
                m.insert(
                    name.to_string(),
                    if *is_album {
                        DownloadType::album
                    } else {
                        DownloadType::track
                    },
                );
            }
            m
        },
        test_url: Some(cfg.test_url.to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor(cfg: &'static WpBlogConfig) -> Arc<dyn ModuleConstructor> {
    Arc::new(WpBlogConstructor { cfg })
}

#[derive(Debug)]
struct WpBlogConstructor {
    cfg: &'static WpBlogConfig,
}

impl ModuleConstructor for WpBlogConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(WpBlogModule {
            cfg: self.cfg,
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct WpBlogModule {
    cfg: &'static WpBlogConfig,
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_entities(input: &str) -> String {
    input
        .replace("&#8211;", "-")
        .replace("&#8212;", "-")
        .replace("&#8217;", "'")
        .replace("&#8216;", "'")
        .replace("&#039;", "'")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

fn normalize_url(raw: &str, host: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else if raw.starts_with('/') {
        format!("https://{host}{raw}")
    } else {
        String::new()
    }
}

fn split_title(title: &str) -> (Option<String>, Option<String>, Option<String>) {
    let year_re = Regex::new(r"\((\d{4})\)").unwrap();
    if let Some(idx) = title.find(" - ") {
        let a = title[..idx].trim().to_string();
        let rest = title[idx + 3..].trim().to_string();
        let y = year_re
            .captures(&rest)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        (
            if a.is_empty() { None } else { Some(a) },
            if rest.is_empty() { None } else { Some(rest) },
            y,
        )
    } else {
        let y = year_re
            .captures(title)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let t = title.trim();
        (
            None,
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            },
            y,
        )
    }
}

fn parse_search_results(html: &str, cfg: &WpBlogConfig) -> Vec<SearchResult> {
    let host_esc = regex::escape(cfg.host);
    let patterns = [
        format!(
            r#"<a href="(https?://(?:www\.)?{host_esc}/[^"]+)"[^>]*rel="bookmark"[^>]*>([^<]+)</a>"#
        ),
        r#"<h2[^>]*class="[^"]*entry-title[^"]*"[^>]*>\s*<a href="([^"]+)"[^>]*>([^<]+)</a>"#
            .to_string(),
        r#"<h2[^>]*class="[^"]*post-title[^"]*"[^>]*>\s*<a href="([^"]+)"[^>]*>([^<]+)</a>"#
            .to_string(),
    ];

    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for pat in patterns.iter() {
        let re = match Regex::new(pat) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for cap in re.captures_iter(html) {
            let raw_url = cap
                .get(1)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            let raw_title = cap
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            if raw_url.is_empty() || raw_title.is_empty() {
                continue;
            }
            let url = normalize_url(&raw_url, cfg.host);
            if url.is_empty() {
                continue;
            }
            if !seen.insert(url.clone()) {
                continue;
            }
            let title = decode_entities(&raw_title);
            if title.is_empty() {
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
        }
    }
    out
}

fn clean_page_title(raw: &str, cfg: &WpBlogConfig) -> String {
    let service = cfg.service_name.to_lowercase();
    let host = cfg.host.to_lowercase();
    let mut title = decode_entities(raw.trim());
    let seps = [" | ", " » ", " - ", " – ", " — "];
    for sep in seps.iter() {
        if let Some(idx) = title.rfind(sep) {
            let tail = title[idx + sep.len()..].to_lowercase();
            if tail.contains(&service) || tail.contains(&host) {
                title.truncate(idx);
                break;
            }
        }
    }
    title.trim().to_string()
}

fn parse_album_meta(
    html: &str,
    cfg: &WpBlogConfig,
) -> (String, String, String, Option<String>, Option<i32>) {
    let title_re = Regex::new(r"(?is)<title>(.*?)</title>").unwrap();
    let raw = title_re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .unwrap_or_else(|| "Unknown".to_string());
    let title = clean_page_title(&raw, cfg);
    let (artist, album, year) = split_title(&title);
    let artist = artist.unwrap_or_else(|| "Unknown Artist".to_string());
    let album = album.unwrap_or_else(|| title.clone());
    let year = year.and_then(|y| y.parse::<i32>().ok());

    let cover_re = Regex::new(r#"(?is)<meta property="og:image" content="([^"]+)""#).unwrap();
    let cover = cover_re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .filter(|c| !c.contains(cfg.no_image_marker));

    (artist, album, title, cover, year)
}

fn parse_download_link(html: &str, cfg: &WpBlogConfig) -> Option<String> {
    let href_re = Regex::new(r#"(?is)href="([^"]+)""#).unwrap();
    let host_prefix = format!("https://{}/download", cfg.host);
    // Open (captcha-free) hosts resolved by picaro-downloader::hosters.
    let open_hosts = [
        "mediafire",
        "1fichier",
        "pixeldrain",
        "disk.yandex",
        "yadi.sk",
        "drive.google",
        "dropbox",
        "gofile",
        "catbox",
        "litterbox",
        "transfer.sh",
        "file.io",
        "tmpfiles",
    ];
    // Recognised but captcha / unsupported hosts: kept only as a fallback so a
    // post that offers both an open and a gated host still resolves to the open
    // one.
    let gated_hosts = ["zippy", "mega", "katfile", "rapidgator"];
    let mut gated_hit: Option<String> = None;
    for cap in href_re.captures_iter(html) {
        let url = match cap.get(1) {
            Some(m) => m.as_str().trim(),
            None => continue,
        };
        if url.is_empty() {
            continue;
        }
        let lower = url.to_lowercase();
        if open_hosts.iter().any(|h| lower.contains(h)) {
            return Some(url.to_string());
        }
        let gated = url.contains("filecrypt") || gated_hosts.iter().any(|h| lower.contains(h));
        if (gated || url.starts_with(&host_prefix)) && gated_hit.is_none() {
            gated_hit = Some(url.to_string());
        }
    }
    gated_hit
}

async fn fetch_album_page(
    client: &reqwest::Client,
    url: &str,
    cfg: &WpBlogConfig,
) -> Result<String> {
    cf_http::fetch(client, url, cfg.referer).await.map_err(|e| {
        Error::Other(format!(
            "{} album fetch: {e}",
            cfg.service_name.to_lowercase()
        ))
    })
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for WpBlogModule {
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
            .unwrap_or("")
            .trim()
            .to_string();
        let name = if !track_name.is_empty() {
            if artist.is_empty() {
                format!("{track_name} (FLAC)")
            } else {
                format!("{artist} - {track_name} (FLAC)")
            }
        } else {
            format!("{} - {} (FLAC)", artist, album)
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
        if std::path::Path::new(track_id).exists() {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::TempFilePath,
                file_url: None,
                file_url_headers: Default::default(),
                temp_file_path: Some(std::path::PathBuf::from(track_id)),
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
            self.cfg.service_name.to_lowercase()
        )))
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let url = if album_id.starts_with("https://") {
            album_id.to_string()
        } else {
            format!(
                "https://{}/{}",
                self.cfg.host,
                album_id.trim_start_matches('/')
            )
        };
        let html = fetch_album_page(&self.client, &url, self.cfg).await?;
        let (artist, album, _title, cover, year) = parse_album_meta(&html, self.cfg);
        let link = parse_download_link(&html, self.cfg).ok_or_else(|| {
            Error::Other(format!(
                "{}: download link not found on album page",
                self.cfg.service_name
            ))
        })?;
        Ok(AlbumInfo {
            name: album,
            artist,
            tracks: vec![TrackRef::Id(link)],
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
            if !v.is_empty() && !v.contains(self.cfg.no_image_marker) {
                return Ok(CoverInfo {
                    url: v.to_string(),
                    file_type: ImageFileType::Jpg,
                });
            }
        }
        Err(Error::Other(format!(
            "{}: no cover for track {track_id}",
            self.cfg.service_name.to_lowercase()
        )))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let encoded = query.replace(' ', "+");
        let url = format!("{}?s={encoded}", self.cfg.referer);
        let html = cf_http::fetch(&self.client, &url, self.cfg.referer)
            .await
            .map_err(|e| {
                Error::Other(format!(
                    "{} search: {e}",
                    self.cfg.service_name.to_lowercase()
                ))
            })?;
        let mut results = parse_search_results(&html, self.cfg);
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry, cfg: &'static WpBlogConfig) {
    register(registry, module_information(cfg), constructor(cfg));
}
