use std::collections::HashMap;
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

use crate::registry::register;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const REFERER: &str = "https://get.coreradio.online/";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
 service_name: "CoreRadio".to_string(),
 module_supported_modes: ModuleModes::download,
 global_settings: indexmap::IndexMap::new(),
 global_storage_variables: vec![],
 session_settings: indexmap::IndexMap::new(),
 session_storage_variables: vec![],
 flags: ModuleFlags::empty(),
 netlocation_constant: NetlocConstants::Single("coreradio.online".to_string()),
 url_constants: {
 let mut m = indexmap::IndexMap::new();
 m.insert("album".to_string(), DownloadType::album);
 m.insert("singles".to_string(), DownloadType::track);
 m.insert("metalcore".to_string(), DownloadType::album);
 m.insert("deathcore".to_string(), DownloadType::album);
 m.insert("hardcore".to_string(), DownloadType::album);
 m.insert("post-hardcore".to_string(), DownloadType::album);
 m.insert("mathcore".to_string(), DownloadType::album);
 m.insert("electronic".to_string(), DownloadType::album);
 m.insert("experimental".to_string(), DownloadType::album);
 m.insert("other".to_string(), DownloadType::album);
 m
 },
 test_url: Some("https://coreradio.online/metalcore/57321-like-moths-to-flames-does-heaven-ever-mourn-for-me-ep-2026".to_string()),
 url_decoding: ManualEnum::Manual,
 login_behaviour: ManualEnum::Manual,
 }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(CoreRadioConstructor)
}

#[derive(Debug)]
struct CoreRadioConstructor;

impl ModuleConstructor for CoreRadioConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(CoreRadioModule {
            controller,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct CoreRadioModule {
    controller: ModuleController,
    client: reqwest::Client,
}

fn decode_hash_twice(hash: &str) -> Result<String> {
    let once = base64::engine::general_purpose::STANDARD
        .decode(hash.trim())
        .map_err(|e| Error::Other(format!("coreradio hash decode (1): {e}")))?;
    let once_str = String::from_utf8(once)
        .map_err(|e| Error::Other(format!("coreradio hash utf8 (1): {e}")))?;
    let twice = base64::engine::general_purpose::STANDARD
        .decode(once_str.trim())
        .map_err(|e| Error::Other(format!("coreradio hash decode (2): {e}")))?;
    String::from_utf8(twice).map_err(|e| Error::Other(format!("coreradio hash utf8 (2): {e}")))
}

async fn fetch_album_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .header("Referer", "https://coreradio.online/")
        .send()
        .await
        .map_err(|e| Error::Other(format!("coreradio album fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "coreradio album HTTP {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("coreradio album read: {e}")))
}

fn parse_album_meta(html: &str) -> (String, String, String, Option<String>, Option<i32>) {
    let title_re = Regex::new(r"<title>(.*?)\s*»\s*CORE RADIO</title>").unwrap();
    let title = title_re
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
        .unwrap_or_else(|| "Unknown".to_string());
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

fn parse_quality(html: &str) -> Option<String> {
    let re = Regex::new(r#""quality":\s*"([^"]+)""#).ok()?;
    re.captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

fn parse_download_hash(html: &str, fmt: &str) -> Option<String> {
    let pattern = format!(
        r#"href=['"]https://get\.coreradio\.online/\?hash=([^'"\s]+)['"][^>]*title=['"]DOWNLOAD\s*{}"#,
        regex::escape(fmt)
    );
    let re = Regex::new(&pattern).ok()?;
    re.captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let item_re = Regex::new(r#"(?s)<li class="tcarusel-item main-news">.*?</li>"#).unwrap();
    let title_re =
        Regex::new(r#"(?s)<div class="tcarusel-item-title">.*?href="([^"]+)"[^>]*>([^<]+)</a>"#)
            .unwrap();
    let cover_re =
        Regex::new(r#"(?s)<div class="tcarusel-item-image">.*?<a href="[^"]*"><img src="([^"]+)""#)
            .unwrap();
    let year_re = Regex::new(r"\((\d{4})\)").unwrap();

    let mut out = Vec::new();
    for cap in item_re.captures_iter(html) {
        let block = cap.get(0).map(|m| m.as_str()).unwrap_or("");
        if let Some(tc) = title_re.captures(block) {
            let url = tc
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let title = tc
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
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
                (None, Some(title.clone()), y)
            };
            let image_url = cover_re.captures(block).and_then(|c| {
                let src = c.get(1).map(|m| m.as_str().to_string())?;
                if src.contains("/templates/coredark/dleimages/no_image") {
                    None
                } else {
                    Some(src)
                }
            });
            out.push(SearchResult {
                result_id: url,
                name: album,
                artists: artist.map(|a| vec![a]),
                year,
                image_url,
                ..Default::default()
            });
        }
    }
    out
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for CoreRadioModule {
    fn name(&self) -> &str {
        "CoreRadio"
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
            decode_hash_twice(track_id)?
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
            format!("https://coreradio.online/{album_id}")
        };
        let html = fetch_album_page(&self.client, &url).await?;
        let (artist, album, _title, cover, year) = parse_album_meta(&html);
        let quality = parse_quality(&html).unwrap_or_default();
        let flac_hash = parse_download_hash(&html, "FLAC").ok_or_else(|| {
            Error::Other("coreradio: FLAC download link not found on album page".to_string())
        })?;
        Ok(AlbumInfo {
            name: album,
            artist: artist.clone(),
            tracks: vec![TrackRef::Id(flac_hash)],
            release_year: year.unwrap_or(0),
            artist_id: None,
            id: Some(album_id.to_string()),
            quality: Some(if quality.contains("FLAC") {
                "FLAC".to_string()
            } else {
                quality
            }),
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
            module: "CoreRadio".to_string(),
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
            module: "CoreRadio".to_string(),
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
            "coreradio: no cover for track {track_id}"
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
        let max_pages = ((limit as usize + 19) / 20).min(5);
        for page in 0..max_pages {
            let encoded = utf8_percent_encode(query, NON_ALPHANUMERIC).to_string();
            let url = format!(
 "https://coreradio.online/index.php?do=search&subaction=search&story={}&search_start={}&result_from={}",
 encoded,
 page,
 page * 20 + 1
 );
            let resp = self
                .client
                .get(&url)
                .header("Referer", "https://coreradio.online/")
                .send()
                .await
                .map_err(|e| Error::Other(format!("coreradio search: {e}")))?;
            if !resp.status().is_success() {
                break;
            }
            let html = resp
                .text()
                .await
                .map_err(|e| Error::Other(format!("coreradio search read: {e}")))?;
            let results = parse_search_results(&html);
            if results.is_empty() {
                break;
            }
            all.extend(results);
            if all.len() >= limit as usize {
                break;
            }
        }
        all.truncate(limit as usize);
        Ok(all)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
