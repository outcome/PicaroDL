//! Monochrome (monochrome.st) lossless catalogue module.
//!
//! Monochrome serves FLAC from its own host (no shared Tidal account, no
//! login, no key):
//!   - `GET /search/tracks?q=`   -> `{tracks:[{id, title, artistNames, releaseId,
//!                                  duration (ms), isrc, playable, artwork}]}`
//!   - `GET /search/releases?q=` -> `{releases:[{id, title, artistNames,
//!                                  releaseDate, releaseType, artwork}]}`
//!   - `GET /releases/<id>`      -> release metadata + `tracks[]` in disc order
//!   - `GET /track/<id>`         -> the FLAC file itself (HTTP 206 ok, no expiry)
//! There is no per-track metadata endpoint, so a track id carries its own
//! metadata after a `#` (like Relisten's ids): `<trackId>#<releaseId>|<ms>|
//! <isrc>|<title>|<artists>`; the release is fetched (best effort) for the
//! album name, year, track number and UPC.
//!
//! One operator's free service: the host list is a const so a mirror or a
//! new host is a one-line change. Source list found via
//! kipavy/spotube-plugin-lossless-sources (probes hourly).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "Monochrome";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// Tried in order; a host that errors falls through to the next.
const HOSTS: [&str; 1] = ["https://tracks.monochrome.st"];
/// Artist separator inside an id (percent-encoded, so never ambiguous).
const SEP: char = '\u{1f}';

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("monochrome.st".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://tracks.monochrome.st/health".to_string()),
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

fn dec(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().to_string()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

/// JSON from the first host that answers 2xx.
async fn fetch_json(client: &reqwest::Client, path: &str) -> Result<Value> {
    let mut last = Error::Other("monochrome: no host".into());
    for host in HOSTS {
        match client.get(format!("{host}{path}")).send().await {
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(v) => return Ok(v),
                Err(e) => last = Error::Other(format!("monochrome json: {e}")),
            },
            Ok(r) => last = Error::Other(format!("monochrome HTTP {}", r.status())),
            Err(e) => last = Error::Other(format!("monochrome fetch: {e}")),
        }
    }
    Err(last)
}

fn artist_names(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .filter(|x| !x.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `artists: [{name}]` (release tracks) as plain names.
fn artist_objects(v: &Value) -> Vec<String> {
    v.get("artists")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .filter(|x| !x.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

struct Meta {
    track_id: String,
    release_id: String,
    duration_ms: u64,
    isrc: String,
    title: String,
    artists: Vec<String>,
}

fn encode_id(m: &Meta) -> String {
    format!(
        "{}#{}|{}|{}|{}|{}",
        m.track_id,
        m.release_id,
        m.duration_ms,
        m.isrc,
        enc(&m.title),
        enc(&m.artists.join(&SEP.to_string()))
    )
}

fn decode_id(id: &str) -> Meta {
    let (track_id, frag) = id.split_once('#').unwrap_or((id, ""));
    let mut it = frag.split('|');
    let release_id = it.next().unwrap_or("").to_string();
    let duration_ms = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let isrc = it.next().unwrap_or("").to_string();
    let title = dec(it.next().unwrap_or(""));
    let artists = dec(it.next().unwrap_or(""))
        .split(SEP)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect();
    Meta {
        track_id: track_id.to_string(),
        release_id,
        duration_ms,
        isrc,
        title,
        artists,
    }
}

fn year_of(date: &str) -> i32 {
    date.get(..4).and_then(|y| y.parse().ok()).unwrap_or(0)
}

fn disc_of(t: &Value) -> u64 {
    t.get("discNumber").and_then(|d| d.as_u64()).unwrap_or(1)
}

/// 1-based position of `all[i]` on its own disc. The API's own `trackNumber`
/// is not trustworthy (every track of a release can say 1), the array is in
/// release order.
fn number_on_disc(all: &[Value], i: usize) -> usize {
    all[..i].iter().filter(|x| disc_of(x) == disc_of(&all[i])).count() + 1
}

/// One release track (from `/releases/<id>`) as a full `TrackInfo`.
fn release_track(t: &Value, rel: &Value, number: usize, total: usize, album_artist: &str) -> TrackInfo {
    let release_id = s(rel, "releaseId").to_string();
    let artists = artist_objects(t);
    let meta = Meta {
        track_id: s(t, "trackId").to_string(),
        release_id: release_id.clone(),
        duration_ms: t.get("duration").and_then(|d| d.as_u64()).unwrap_or(0),
        isrc: s(t, "isrc").to_string(),
        title: s(t, "title").to_string(),
        artists: artists.clone(),
    };
    let date = s(rel, "releaseDate");
    TrackInfo {
        name: meta.title.clone(),
        album: s(rel, "title").to_string(),
        album_id: release_id,
        artists,
        codec: CodecFlags::FLAC,
        cover_url: s(rel, "artwork").to_string(),
        release_year: year_of(date),
        duration: Some((meta.duration_ms / 1000) as u32),
        explicit: t.get("explicit").and_then(|e| e.as_bool()),
        id: Some(encode_id(&meta)),
        tags: Tags {
            album_artist: Some(album_artist.to_string()).filter(|a| !a.is_empty()),
            track_number: Some(number as u32),
            total_tracks: Some(total as u32),
            disc_number: Some(disc_of(t) as u32),
            isrc: Some(meta.isrc.clone()).filter(|i| !i.is_empty()),
            upc: rel.get("upc").and_then(|u| u.as_str()).map(str::to_string),
            label: Some(s(rel, "label").to_string()).filter(|l| !l.is_empty()),
            release_date: date.get(..10).map(str::to_string),
            ..Default::default()
        },
        ..Default::default()
    }
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
        let m = decode_id(track_id);
        if m.track_id.is_empty() || !m.track_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Other(format!("monochrome: bad id '{track_id}'")));
        }
        // Best effort: the release gives the album name, year, UPC and the
        // track's position; a miss still downloads (with the id's own
        // title / artist).
        if !m.release_id.is_empty() {
            if let Ok(rel) = fetch_json(&self.client, &format!("/releases/{}", m.release_id)).await {
                if let Some(tracks) = rel.get("tracks").and_then(|t| t.as_array()) {
                    if let Some(i) = tracks.iter().position(|t| s(t, "trackId") == m.track_id) {
                        let album_artist = artist_objects(&rel).first().cloned().unwrap_or_default();
                        let number = number_on_disc(tracks, i);
                        return Ok(release_track(&tracks[i], &rel, number, tracks.len(), &album_artist));
                    }
                }
            }
        }
        Ok(TrackInfo {
            name: m.title.clone(),
            artists: m.artists.clone(),
            codec: CodecFlags::FLAC,
            duration: Some((m.duration_ms / 1000) as u32),
            id: Some(track_id.to_string()),
            tags: Tags {
                isrc: Some(m.isrc.clone()).filter(|i| !i.is_empty()),
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
        let m = decode_id(track_id);
        if m.track_id.is_empty() || !m.track_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Other(format!("monochrome: bad id '{track_id}'")));
        }
        // The file answers 5xx now and then (Cloudflare 521); a HEAD that
        // succeeds picks the host that will actually serve it.
        let mut last = Error::Other("monochrome: no host".into());
        for host in HOSTS {
            let url = format!("{host}/track/{}", m.track_id);
            for attempt in 0..4u64 {
                match self.client.head(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        return Ok(TrackDownloadInfo {
                            download_type: DownloadSource::Url,
                            file_url: Some(url),
                            file_url_headers: serde_json::Map::new(),
                            temp_file_path: None,
                            different_codec: None,
                        });
                    }
                    // 429 is the host rate-limiting us, not "no such file": back off and retry.
                    Ok(r) if r.status().is_client_error() && r.status() != reqwest::StatusCode::TOO_MANY_REQUESTS => {
                        last = Error::Other(format!("monochrome: {} has no file ({})", m.track_id, r.status()));
                        break;
                    }
                    Ok(r) => last = Error::Other(format!("monochrome HTTP {}", r.status())),
                    Err(e) => last = Error::Other(format!("monochrome fetch: {e}")),
                }
                tokio::time::sleep(std::time::Duration::from_millis(2000 * (attempt + 1))).await;
            }
        }
        Err(last)
    }

    async fn get_album_info(&self, album_id: &str, _data: HashMap<String, Value>) -> Result<AlbumInfo> {
        let id = album_id.trim();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Other(format!("monochrome: bad release id '{album_id}'")));
        }
        let rel = fetch_json(&self.client, &format!("/releases/{id}")).await?;
        let album_artist = artist_objects(&rel).first().cloned().unwrap_or_default();
        let all = rel
            .get("tracks")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        // An unplayable catalogue entry has no file behind it; position
        // numbering still counts it so track numbers match the release.
        let total = all.len();
        let tracks: Vec<TrackRef> = all
            .iter()
            .enumerate()
            .filter(|(_, t)| t.get("playable").and_then(|p| p.as_bool()) != Some(false))
            .map(|(i, t)| {
                let number = number_on_disc(&all, i);
                TrackRef::Full(Box::new(release_track(t, &rel, number, total, &album_artist)))
            })
            .collect();
        if tracks.is_empty() {
            return Err(Error::Other("monochrome: release has no playable tracks".into()));
        }
        Ok(AlbumInfo {
            name: s(&rel, "title").to_string(),
            artist: album_artist,
            tracks,
            release_year: year_of(s(&rel, "releaseDate")),
            expected_track_count: Some(total as u32),
            id: Some(id.to_string()),
            quality: Some("FLAC (lossless)".to_string()),
            cover_url: Some(s(&rel, "artwork").to_string()).filter(|c| !c.is_empty()),
            upc: rel.get("upc").and_then(|u| u.as_str()).map(str::to_string),
            label: Some(s(&rel, "label").to_string()).filter(|l| !l.is_empty()),
            explicit: rel.get("explicit").and_then(|e| e.as_bool()),
            ..Default::default()
        })
    }

    async fn get_playlist_info(&self, _id: &str, _data: HashMap<String, Value>) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.to_string(),
            ability: "playlist".to_string(),
        })
    }

    async fn get_artist_info(
        &self,
        _id: &str,
        _credited: bool,
        _name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.to_string(),
            ability: "artist".to_string(),
        })
    }

    async fn get_track_credits(&self, _id: &str, _data: HashMap<String, Value>) -> Result<Vec<CreditsInfo>> {
        Ok(Vec::new())
    }

    async fn get_track_cover(&self, _id: &str, _cover: &CoverOptions, _data: HashMap<String, Value>) -> Result<CoverInfo> {
        Err(Error::Other("monochrome: cover comes with the track info".into()))
    }

    async fn search(
        &self,
        query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        // "artist - title" -> plain words: the dash only hurts the match.
        let q = query.replace(" - ", " ");
        let max = limit.clamp(1, 50) as usize;
        let mut out: Vec<SearchResult> = Vec::new();
        if query_type == DownloadType::album {
            let v = fetch_json(&self.client, &format!("/search/releases?q={}&limit=20", enc(&q))).await?;
            for r in v.get("releases").and_then(|r| r.as_array()).into_iter().flatten() {
                let id = s(r, "id");
                if id.is_empty() {
                    continue;
                }
                out.push(SearchResult {
                    result_id: id.to_string(),
                    name: Some(s(r, "title").to_string()),
                    artists: Some(artist_names(r, "artistNames")),
                    year: s(r, "releaseDate").get(..4).map(str::to_string),
                    explicit: r.get("explicit").and_then(|e| e.as_bool()),
                    image_url: Some(s(r, "artwork").to_string()).filter(|a| !a.is_empty()),
                    ..Default::default()
                });
            }
        } else {
            let v = fetch_json(&self.client, &format!("/search/tracks?q={}&limit=20", enc(&q))).await?;
            let mut seen: Vec<String> = Vec::new();
            // For "artist - title" the title must be the wanted one: the
            // catalogue is huge, and a failed download used to fall through
            // to the next hit ("Waiting for the End" for "In The End").
            let want = query
                .split_once(" - ")
                .map(|(_, t)| crate::lyrics_kit::norm(t))
                .filter(|t| !t.is_empty());
            for t in v.get("tracks").and_then(|t| t.as_array()).into_iter().flatten() {
                if t.get("playable").and_then(|p| p.as_bool()) == Some(false) {
                    continue;
                }
                if let Some(w) = &want {
                    if crate::lyrics_kit::norm(s(t, "title")) != *w {
                        continue;
                    }
                }
                let id = s(t, "id");
                if id.is_empty() {
                    continue;
                }
                // The same recording appears once per release it is on.
                let isrc = s(t, "isrc").to_string();
                if !isrc.is_empty() {
                    if seen.contains(&isrc) {
                        continue;
                    }
                    seen.push(isrc.clone());
                }
                let meta = Meta {
                    track_id: id.to_string(),
                    release_id: s(t, "releaseId").to_string(),
                    duration_ms: t.get("duration").and_then(|d| d.as_u64()).unwrap_or(0),
                    isrc,
                    title: s(t, "title").to_string(),
                    artists: artist_names(t, "artistNames"),
                };
                out.push(SearchResult {
                    result_id: encode_id(&meta),
                    name: Some(meta.title.clone()),
                    artists: Some(meta.artists.clone()),
                    duration: Some((meta.duration_ms / 1000) as u32),
                    explicit: t.get("explicit").and_then(|e| e.as_bool()),
                    image_url: Some(s(t, "artwork").to_string()).filter(|a| !a.is_empty()),
                    ..Default::default()
                });
            }
        }
        out.truncate(max);
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
    fn id_round_trip() {
        let m = Meta {
            track_id: "154041876758925312".into(),
            release_id: "154026472057606144".into(),
            duration_ms: 216880,
            isrc: "USWB10002407".into(),
            title: "In the End | (Live) #1".into(),
            artists: vec!["Linkin Park".into(), "Guest | X".into()],
        };
        let d = decode_id(&encode_id(&m));
        assert_eq!(d.track_id, m.track_id);
        assert_eq!(d.release_id, m.release_id);
        assert_eq!(d.duration_ms, 216880);
        assert_eq!(d.title, m.title);
        assert_eq!(d.artists, m.artists);
        // A bare id (no metadata) still decodes.
        assert_eq!(decode_id("123").track_id, "123");
    }

    /// Live: `cargo test -p picaro-modules monochrome_live -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn monochrome_live() {
        let m = Mod { client: picaro_utils::http::build_client_with_user_agent(None, UA) };
        use picaro_utils::module::ModuleInterface;
        let hits = m.search(DownloadType::track, "Linkin Park - In The End", None, 5).await.unwrap();
        let first = hits.first().expect("no track hit");
        assert_eq!(first.name.as_deref(), Some("In the End"));
        let info = m
            .get_track_info(&first.result_id, Quality::default(), &CodecOptions::default(), HashMap::new())
            .await
            .unwrap();
        eprintln!("album={} no={:?} year={}", info.album, info.tags.track_number, info.release_year);
        let dl = m
            .get_track_download(&first.result_id, Quality::default(), &CodecOptions::default(), HashMap::new())
            .await
            .unwrap();
        assert!(dl.file_url.unwrap().contains("/track/"));
        let albums = m.search(DownloadType::album, "Linkin Park - Hybrid Theory", None, 5).await.unwrap();
        let al = m.get_album_info(&albums[0].result_id, HashMap::new()).await.unwrap();
        assert!(al.tracks.len() >= 12, "tracks {}", al.tracks.len());
    }
}
