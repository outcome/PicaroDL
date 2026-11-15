//! Mp3Tut (one.mp3-tut.click) module.
//!
//! Custom-engine song site. Search at `/search/<query>`; results link to
//! `/handler/<query>/<n>` which 302s to a `/check/<slug>` page whose
//! `data-id` attributes carry the direct file URL
//! (`/media/music/...mp3` on the same host — ID3 + HTTP 206 verified).
//! CDN `?h=` tokens yield M4A and are only used as a fallback; trailing
//! `\` artifacts in tokens are stripped (a mangled token returns 400).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "Mp3Tut";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://one.mp3-tut.click";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("mp3-tut.click".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://one.mp3-tut.click/search/radiohead".to_string()),
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

async fn fetch_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("mp3tut fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("mp3tut HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("mp3tut read: {e}")))
}

/// Fetch a page and also return the FINAL url after redirects, so
/// `/handler/<q>/<n>` resolves to its `/check/<song-slug>` target.
async fn fetch_tracked(client: &reqwest::Client, url: &str) -> Result<(String, String)> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("mp3tut fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("mp3tut HTTP {}", resp.status())));
    }
    let final_url = resp.url().to_string();
    let text = resp
        .text()
        .await
        .map_err(|e| Error::Other(format!("mp3tut read: {e}")))?;
    Ok((text, final_url))
}

/// Handler links on the search page: `/handler/<query>/<n>`.
fn parse_handler_links(html: &str) -> Vec<String> {
    let re = Regex::new(r#"href="/handler/([^"]+)""#).unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let path = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        if path.is_empty() {
            continue;
        }
        let url = format!("{BASE}/handler/{path}");
        if !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// Direct file URL from a `/check/<slug>` page's `data-id` attribute. The
/// FIRST one is the requested song; the `/media/...mp3` ones after it are the
/// page's unrelated "more tracks" list (taking one of those once downloaded
/// the wrong song under the right name).
fn parse_data_id(html: &str) -> Option<String> {
    let re = Regex::new(r#"data-id="([^"]+)""#).unwrap();
    for cap in re.captures_iter(html) {
        let raw = cap.get(1).map(|m| m.as_str()).unwrap_or("").trim();
        let raw = raw.trim_end_matches('\\');
        if raw.starts_with('/') {
            return Some(format!("{BASE}{raw}"));
        }
        if raw.starts_with("http") {
            return Some(raw.to_string());
        }
    }
    None
}

/// "Artist - Title" of the requested song from a `/check/` page.
fn parse_check_title(html: &str) -> Option<String> {
    let re = Regex::new(r"(?s)<b>([^<]+)</b>\s*<strong>([^<]+)</strong>").unwrap();
    let c = re.captures(html)?;
    Some(format!("{} - {}", c[1].trim(), c[2].trim()))
}

/// Result titles on a search page, in the same order as its handler links.
fn parse_result_titles(html: &str) -> Vec<String> {
    let re = Regex::new(r#"<span class="artist_name">([^<]+)</span>"#).unwrap();
    re.captures_iter(html).map(|c| c[1].trim().to_string()).collect()
}

fn name_from_slug(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let seg = base.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    seg.replace('-', " ").replace('_', " ")
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
        let (mp3, name) = if track_id.starts_with("http")
            && (track_id.contains("/media/music/") || track_id.contains("cdn.mp3-tut.click"))
        {
            (track_id.to_string(), name_from_slug(track_id))
        } else {
            let (html, final_url) = fetch_tracked(&self.client, track_id).await?;
            let mp3 = parse_data_id(&html)
                .ok_or_else(|| Error::Other("mp3tut: no data-id on page".into()))?;
            let slug = final_url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("");
            (mp3, parse_check_title(&html).unwrap_or_else(|| slug.replace('-', " ")))
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
        let mp3 = if track_id.starts_with("http")
            && (track_id.contains("/media/music/") || track_id.contains("cdn.mp3-tut.click"))
        {
            track_id.to_string()
        } else {
            let html = fetch_page(&self.client, track_id).await?;
            parse_data_id(&html)
                .ok_or_else(|| Error::Other("mp3tut: no data-id on page".into()))?
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
        Err(Error::Other("mp3tut: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let q = query.split_whitespace().collect::<Vec<_>>().join("-");
        let html = fetch_page(&self.client, &format!("{BASE}/search/{q}")).await?;
        let max = limit.clamp(1, 50) as usize;
        // Each handler link is followed (one HTTP GET each) to learn the
        // real song name, so cap the resolution work regardless of the
        // caller's limit.
        let fetch_cap = max.min(10);
        let mut out = Vec::new();
        let titles = parse_result_titles(&html);
        for (i, handler) in parse_handler_links(&html).into_iter().enumerate() {
            if out.len() >= fetch_cap {
                break;
            }
            // The handler URL and its /check/ slug both echo the QUERY, so
            // the real title is the one the search page lists for this row;
            // the page must still have a download link.
            let Ok((page, final_url)) = fetch_tracked(&self.client, &handler).await else {
                continue;
            };
            if parse_data_id(&page).is_none() {
                continue;
            }
            let slug = final_url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .replace('-', " ");
            // "Artist - Title": give the artist its own field so the resolver
            // rejects a same-title / different-artist row ("Lifeendz - Silver Spoon").
            let full = titles.get(i).cloned().unwrap_or(slug);
            let (artist, name) = match full.split_once(" - ") {
                Some((a, t)) => (Some(vec![a.trim().to_string()]), t.trim().to_string()),
                None => (None, full),
            };
            out.push(SearchResult {
                result_id: handler,
                name: Some(name),
                artists: artist,
                ..Default::default()
            });
        }
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_track_beats_the_more_tracks_list() {
        let html = r#"<b>Jiinzo</b>
 <strong>Silver Spoon</strong>
 <div data-id="https://cdn.mp3-tut.click/?h=MAIN\">
 <div data-id="/media/music/45/1_x.mp3">"#;
        assert_eq!(parse_data_id(html).unwrap(), "https://cdn.mp3-tut.click/?h=MAIN");
        assert_eq!(parse_check_title(html).unwrap(), "Jiinzo - Silver Spoon");
    }
}
