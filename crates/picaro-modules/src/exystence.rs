use std::collections::HashMap;
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
const REFERER: &str = "https://exystence.net/";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "Exystence".to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("exystence.net".to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("blog".to_string(), DownloadType::album);
            m.insert("category".to_string(), DownloadType::album);
            m.insert("tag".to_string(), DownloadType::album);
            m.insert("flac".to_string(), DownloadType::track);
            m.insert("320".to_string(), DownloadType::track);
            m.insert("mp3".to_string(), DownloadType::track);
            m
        },
        test_url: Some("https://exystence.net/blog/2026/09/24/test-album-2026/".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(ExystenceConstructor)
}

#[derive(Debug)]
struct ExystenceConstructor;

impl ModuleConstructor for ExystenceConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(ExystenceModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct ExystenceModule {
    controller: ModuleController,
    client: reqwest::Client,
}

async fn fetch_album_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", REFERER)
        .send()
        .await
        .map_err(|e| Error::Other(format!("exystence album fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "exystence album HTTP {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("exystence album read: {e}")))
}

fn parse_album_meta(html: &str) -> (String, String, String, Option<String>, Option<i32>) {
    let title_re = Regex::new(r"<title>(.*?)\s*\|\s*exystence</title>").unwrap();
    let title = title_re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .unwrap_or_else(|| "Unknown".to_string());
    let title = title.replace("&#8211;", "-").replace("&#124;", "|");
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

fn parse_download_link(html: &str, fmt: &str) -> Option<String> {
    let pattern = format!(
        r#"<a[^>]*href="(https?://filecrypt[^"]+)"[^>]*>[^<]*(?:<[^>]*>[^<]*)*?{}\s*(?:<[^>]*>[^<]*)*?</a>"#,
        regex::escape(fmt)
    );
    let re = Regex::new(&pattern).ok()?;
    re.captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .or_else(|| {
            let alt_pattern = format!(
                r#"href="(https?://filecrypt[^"]+)"[^>]*title="[^"]*{}[^"]*""#,
                regex::escape(fmt)
            );
            let re = Regex::new(&alt_pattern).ok()?;
            re.captures(html)
                .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        })
}

fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let link_re = Regex::new(
        r#"<a href="(https://exystence\.net/[^"]+)"[^>]*rel="bookmark"[^>]*>([^<]+)</a>"#,
    )
    .unwrap();
    let year_re = Regex::new(r"\((\d{4})\)").unwrap();

    let mut out = Vec::new();
    for cap in link_re.captures_iter(html) {
        let url = cap
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let title = cap
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let (artist, album, year) = if let Some(idx) = title.find(" - ") {
            let a = title[..idx].to_string();
            let rest = &title[idx + 3..];
            let y = year_re
                .captures(rest)
                .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
            (Some(a), Some(rest.to_string()), y)
        } else {
            let y = year_re
                .captures(&title)
                .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
            (None, Some(title), y)
        };
        out.push(SearchResult {
            result_id: url,
            name: album,
            artists: artist.map(|a| vec![a]),
            year,
            ..Default::default()
        });
    }
    out.dedup_by(|a, b| a.result_id == b.result_id);
    out
}
#[async_trait]
impl picaro_utils::module::ModuleInterface for ExystenceModule {
    fn name(&self) -> &str {
        "Exystence"
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
        Ok(TrackInfo {
            name: format!("{} - {} (FLAC)", artist, album),
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
        let direct_url = if track_id.starts_with("https://") {
            track_id.to_string()
        } else {
            return Err(Error::Other(format!(
                "exystence: expected direct download URL, got {track_id}"
            )));
        };
        let mut headers = serde_json::Map::new();
        headers.insert("Referer".to_string(), json!(REFERER));
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
            format!("https://exystence.net/{album_id}")
        };
        let html = fetch_album_page(&self.client, &url).await?;
        let (artist, album, _title, cover, year) = parse_album_meta(&html);
        let flac_url = parse_download_link(&html, "FLAC").ok_or_else(|| {
            Error::Other("exystence: FLAC download link not found on album page".to_string())
        })?;
        Ok(AlbumInfo {
            name: album,
            artist: artist.clone(),
            tracks: vec![TrackRef::Id(flac_url)],
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
            module: "Exystence".to_string(),
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
            module: "Exystence".to_string(),
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
            "exystence: no cover for track {track_id}"
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
        let url = format!("https://exystence.net/?s={encoded}");
        let resp = self
            .client
            .get(&url)
            .header("Referer", REFERER)
            .send()
            .await
            .map_err(|e| Error::Other(format!("exystence search: {e}")))?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let html = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("exystence search read: {e}")))?;
        let mut results = parse_search_results(&html);
        results.truncate(limit as usize);
        Ok(results)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
