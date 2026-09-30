//! Khinsider (downloads.khinsider.com) module — WEBVIEW EDITION ONLY
//! (compiles behind the `cf-webview` feature; never built for the
//! browser-free edition or Android/Switch targets).
//!
//! Video-game soundtracks, MP3 **and FLAC**, no login. Search and the
//! file CDN are open to plain HTTP; the ALBUM and TRACK pages sit behind
//! a Cloudflare **WAF block on the TLS fingerprint** (a hard block — no
//! cookie can ever pass it), so those two pages render in the off-screen
//! browser (~10s each) while everything else stays plain `reqwest`.
//!
//! Flow for a single track (the resolver's direct-track path):
//!   1. `GET /search?search=<q>` (plain) -> album slugs
//!   2. render the album page once -> track table (names + per-track
//!      page links); pick the track whose name best matches the query
//!   3. render that per-track page once -> direct CDN file URL
//!      (`<sub>.vgmtreasurechest.com`, FLAC preferred)
//!   4. download the file with plain `reqwest` (CDN is open)
//!
//! The requested track title rides in the result-id's `#t=` fragment —
//! the same trick the Relisten module uses.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use serde_json::Value;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::textmatch;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "Khinsider";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const BASE: &str = "https://downloads.khinsider.com";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("downloads.khinsider.com".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://downloads.khinsider.com/search?search=zelda".to_string()),
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

/// Fetch a khinsider page through the ladder:
///   1. plain reqwest (search is open; pages may open up anytime)
///   2. Chrome TLS emulation (feature `cf-impersonate`) — defeats the
///      TLS-fingerprint WAF with no browser at all
///   3. off-screen browser render (feature `cf-webview`) — last resort
async fn kh_fetch(url: &str) -> Result<String> {
    if let Ok(body) = fetch_plain(&dummy_client(), url).await {
        return Ok(body);
    }
    #[cfg(feature = "cf-impersonate")]
    {
        if let Ok(body) = crate::impersonate::impersonate_fetch(url).await {
            return Ok(body);
        }
    }
    #[cfg(feature = "cf-webview")]
    {
        return picaro_webview::render_url(url, 12)
            .await
            .map_err(|e| Error::Other(format!("khinsider fetch ({url}): {e}")));
    }
    #[allow(unreachable_code)]
    Err(Error::Other(format!(
        "khinsider: {url} is TLS-fingerprint-blocked (build with --features impersonate or webview)"
    )))
}

/// A plain client for ladder step 1 (the module's own client is
/// per-instance; this is enough for a probe fetch).
async fn fetch_plain(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("khinsider fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("khinsider HTTP {}", resp.status())));
    }
    resp.text()
        .await
        .map_err(|e| Error::Other(format!("khinsider read: {e}")))
}

fn dummy_client() -> reqwest::Client {
    picaro_utils::http::build_client_with_user_agent(None, UA)
}

/// `(per-track page url, track name)` rows from a rendered album page.
fn parse_track_rows(html: &str) -> Vec<(String, String)> {
    let re = Regex::new(
        r#"<td class="clickable-row"><a href="(/game-soundtracks/album/[^"]+\.mp3)">([^<]+)</a></td>"#,
    )
    .unwrap();
    let mut out = Vec::new();
    for cap in re.captures_iter(html) {
        let path = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        let name = cap.get(2).map(|m| m.as_str()).unwrap_or_default();
        if path.is_empty() || name.is_empty() {
            continue;
        }
        let url = format!("{BASE}{path}");
        if !out.iter().any(|(u, _)| *u == url) {
            out.push((url, name.to_string()));
        }
    }
    out
}

/// Direct CDN file URL from a rendered per-track page (FLAC preferred).
fn parse_cdn_link(html: &str) -> Option<(String, CodecFlags)> {
    let flac = Regex::new(r#"https://[a-z0-9.]+\.vgmtreasurechest\.com/soundtracks/[^"'\s<>]+?\.flac"#)
        .unwrap();
    if let Some(m) = flac.find(html) {
        return Some((m.as_str().to_string(), CodecFlags::FLAC));
    }
    let mp3 = Regex::new(r#"https://[a-z0-9.]+\.vgmtreasurechest\.com/soundtracks/[^"'\s<>]+?\.mp3"#)
        .unwrap();
    // The page also embeds a preview <audio> for the same file; either
    // occurrence is the real track.
    mp3.find(html).map(|m| (m.as_str().to_string(), CodecFlags::MP3))
}

fn name_from_track_url(url: &str) -> String {
    let seg = url.split('#').next().unwrap_or(url);
    let seg = seg
        .trim_end_matches(".mp3")
        .trim_end_matches(".flac")
        .rsplit('/')
        .next()
        .unwrap_or("Track");
    // Per-track URLs carry double-encoded names ("01%2520-%2520Title").
    let once = percent_encoding::percent_decode_str(seg)
        .decode_utf8_lossy()
        .to_string();
    let twice = percent_encoding::percent_decode_str(&once)
        .decode_utf8_lossy()
        .to_string();
    twice
        .split_whitespace()
        .filter(|t| t.chars().any(|c| c.is_alphanumeric()) && *t != "-")
        .collect::<Vec<_>>()
        .join(" ")
}

/// `(title, Some(artist))` from a per-track page: its title tag reads
/// "Artist - Title MP3 - Album (Year) - Download Soundtracks...".
/// Title-tag text is HTML-encoded, so decode the common entities.
fn track_title_from_page(html: &str) -> Option<(String, Option<String>)> {
    let raw = Regex::new(r"(?is)<title>(.*?)</title>")
        .ok()?
        .captures(html)?
        .get(1)?
        .as_str()
        .to_string();
    let amp = format!("{}amp;", '&');
    let quot = format!("{}quot;", '&');
    let apos039 = format!("{}#039;", '&');
    let apos8217 = format!("{}#8217;", '&');
    let raw = raw
        .replace(&amp, "&")
        .replace(&quot, "\"")
        .replace(&apos039, "'")
        .replace(&apos8217, "'")
        .replace("&nbsp;", " ");
    let head = raw
        .split(" MP3 - ")
        .next()
        .or_else(|| raw.split(" FLAC - ").next())
        .unwrap_or(&raw)
        .trim();
    if let Some(i) = head.rfind(" - ") {
        Some((
            head[i + 3..].trim().to_string(),
            Some(head[..i].trim().to_string()),
        ))
    } else {
        Some((head.to_string(), None))
    }
}

/// Resolve one track. `track_id` can be:
///   1. a direct CDN url (re-entry from the album flow)
///   2. a per-track page (ends in `.mp3`, from the album track list)
///   3. an album page with a `#t=<query>` fragment carrying the
///      requested track title (the resolver's single-track path)
async fn resolve_cdn_url(track_id: &str) -> Result<(String, CodecFlags, String, Option<String>)> {
    // 1. Already a CDN url?
    if track_id.contains("vgmtreasurechest.com/") {
        let codec = if track_id.ends_with(".flac") {
            CodecFlags::FLAC
        } else {
            CodecFlags::MP3
        };
        return Ok((
            track_id.to_string(),
            codec,
            name_from_track_url(track_id),
            None,
        ));
    }
    let (page_url, query) = match track_id.split_once("#t=") {
        Some((url, q)) => (url.to_string(), q.to_string()),
        None => (track_id.to_string(), String::new()),
    };
    // 2. Per-track page: render it and pull the direct file link.
    if page_url.ends_with(".mp3") || page_url.ends_with(".flac") {
        let html = kh_fetch(&page_url).await?;
        let (cdn, codec) = parse_cdn_link(&html)
            .ok_or_else(|| Error::Other("khinsider: no direct file link on track page".into()))?;
        let (name, artist) = track_title_from_page(&html)
            .map(|(n, a)| (n, a))
            .unwrap_or_else(|| (name_from_track_url(&page_url), None));
        return Ok((cdn, codec, name, artist));
    }
    // 3. Album page: pick the track whose name best matches the query.
    let html = kh_fetch(&page_url).await?;
    let rows = parse_track_rows(&html);
    if rows.is_empty() {
        return Err(Error::Other("khinsider: no tracks on album page".into()));
    }
    // Track picker: score against the full query and, when it's an
    // "artist - title" query, the title part alone - the track names are
    // bare titles ("Title Theme"), never "Game - Title".
    let title_part = query
        .find(" - ")
        .map(|i| query[i + 3..].trim().to_string())
        .unwrap_or_default();
    let best_for = |name: &str| -> f64 {
        let full = textmatch::similarity(&query, name);
        if title_part.is_empty() {
            full
        } else {
            full.max(textmatch::similarity(&title_part, name))
        }
    };
    let (track_url, track_name) = if query.is_empty() {
        rows[0].clone()
    } else {
        rows.iter()
            .map(|(u, n)| (best_for(n), (u.clone(), n.clone())))
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, pair)| pair)
            .unwrap_or_else(|| rows[0].clone())
    };
    let track_html = kh_fetch(&track_url).await?;
    let (cdn, codec) = parse_cdn_link(&track_html).ok_or_else(|| {
        Error::Other("khinsider: no direct file link on track page".into())
    })?;
    let (name, artist) = track_title_from_page(&track_html)
        .map(|(n, a)| (n, a))
        .unwrap_or((track_name.clone(), None));
    Ok((cdn, codec, name, artist))
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
        let (cdn, codec, name, artist) = resolve_cdn_url(track_id).await?;
        Ok(TrackInfo {
            name,
            artists: artist.into_iter().collect::<Vec<_>>(),
            codec,
            id: Some(cdn),
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
        let (cdn, _, _, _) = resolve_cdn_url(track_id).await?;
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(cdn),
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
        // Result-ids may carry a `#t=` fragment from the track flow;
        // browsers never send fragments, but strip it to keep the URL
        // clean for logging.
        let album_id = album_id.split('#').next().unwrap_or(album_id);
        let html = kh_fetch(album_id).await?;
        let rows = parse_track_rows(&html);
        if rows.is_empty() {
            return Err(Error::Other("khinsider: no tracks on album page".into()));
        }
        let title = Regex::new(r"(?is)<title>(.*?)</title>")
            .unwrap()
            .captures(&html)
            .and_then(|c| c.get(1))
            .map(|m| {
                m.as_str()
                    .split(" MP3")
                    .next()
                    .unwrap_or(m.as_str())
                    .trim()
                    .to_string()
            })
            .unwrap_or_else(|| "Game soundtrack".to_string());
        Ok(AlbumInfo {
            name: title,
            artist: "Khinsider".to_string(),
            tracks: rows
                .into_iter()
                .map(|(url, name)| {
                    TrackRef::Full(Box::new(TrackInfo {
                        name,
                        codec: CodecFlags::FLAC,
                        id: Some(url),
                        ..Default::default()
                    }))
                })
                .collect(),
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
        Err(Error::Other("khinsider: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        // The search endpoint matches ALBUM titles as a phrase, so an
        // "artist - title" query finds nothing: try the full string and
        // the parts, all IN PARALLEL - the resolver only gives a module
        // a few seconds, and three sequential fetches blow that window.
        let mut candidates: Vec<String> = vec![query.replace(" - ", " ")];
        if let Some(i) = query.find(" - ") {
            candidates.push(query[i + 3..].to_string());
            candidates.push(query[..i].to_string());
        }
        let client = self.client.clone();
        let mut handles = Vec::new();
        for cand in candidates {
            let client = client.clone();
            handles.push(tokio::spawn(async move {
                let url = format!("{BASE}/search?search={}", enc(&cand));
                fetch_plain(&client, &url).await
            }));
        }
        let mut out: Vec<SearchResult> = Vec::new();
        for h in handles {
            let html = match h.await {
                Ok(Ok(html)) => html,
                _ => continue,
            };
            let re =
                Regex::new(r#"<a href="/game-soundtracks/album/([^"]+)">([^<]+)</a>"#).unwrap();
            for cap in re.captures_iter(&html) {
                let slug = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
                let name = cap.get(2).map(|m| m.as_str()).unwrap_or_default();
                if slug.is_empty() || name.is_empty() {
                    continue;
                }
                let album_url = format!("{BASE}/game-soundtracks/album/{slug}");
                if out
                    .iter()
                    .any(|r: &SearchResult| r.result_id.starts_with(&album_url))
                {
                    continue;
                }
                out.push(SearchResult {
                    // The `#t=` fragment carries the requested track
                    // title through to the track page resolution.
                    result_id: format!("{album_url}#t={}", enc(query)),
                    name: Some(name.to_string()),
                    ..Default::default()
                });
            }
            if out.len() >= limit.clamp(1, 50) as usize {
                break;
            }
        }
        out.truncate(limit.clamp(1, 50) as usize);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Does a PLAIN reqwest fetch pass khinsider's album-page WAF?
    /// (Decides whether the module could work browser-free on
    /// Android/Switch.) Live-gated: PICARO_KH_LIVE=1.
    #[tokio::test]
    async fn plain_album_fetch_probe() {
        if std::env::var("PICARO_KH_LIVE").is_err() {
            return;
        }
        let m = Mod {
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        };
        let url = "https://downloads.khinsider.com/game-soundtracks/album/ocarina-of-time-symphony-2016";
        match fetch_plain(&m.client, url).await {
            Ok(h) => println!(
                "PLAIN-OK len={} tracks={}",
                h.len(),
                parse_track_rows(&h).len()
            ),
            Err(e) => println!("PLAIN-FAIL {e}"),
        }
    }
}
