//! Relisten (relisten.net) live-music archive module.
//!
//! JSON API at `api.relisten.net/api/v2`: `/search?q=` finds artists;
//! `/artists/<slug>/years` lists years, `/artists/<slug>/years/<year>`
//! nests the shows, and `/artists/<slug>/shows/<YYYY-MM-DD>` returns
//! sources -> sets -> tracks with direct `mp3_url` / `flac_url` links into
//! archive.org (ID3 + HTTP 206 verified; archive.org 302s to a CDN node,
//! so redirects must be followed).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "Relisten";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const API: &str = "https://api.relisten.net/api/v2";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("relisten.net".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: Some("https://api.relisten.net/api/v2/search?q=grateful+dead".to_string()),
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

async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<Value> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Other(format!("relisten fetch: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!("relisten HTTP {}", resp.status())));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| Error::Other(format!("relisten json: {e}")))
}

fn artist_name_from_slug(slug: &str) -> String {
    let mut out = String::new();
    for word in slug.split('-') {
        let mut c = word.chars();
        if let Some(f) = c.next() {
            out.push(f.to_ascii_uppercase());
            out.push_str(c.as_str());
        }
        out.push(' ');
    }
    out.trim_end().to_string()
}

/// The album flow re-derives per-track names via `get_track_info(id)`,
/// so the show's real track titles are carried in the id's URL fragment:
/// `"<mp3 url>#<artist>|<title>"` (fragments are never sent to the
/// server; reqwest strips them from the request).
fn frag_enc(s: &str) -> String {
    s.replace('%', "%25").replace(' ', "%20").replace('|', "%7C")
}

fn frag_decode(s: &str) -> String {
    s.replace("%7C", "|").replace("%20", " ").replace("%25", "%")
}

fn split_fragment(id: &str) -> (String, Option<(String, String)>) {
    match id.split_once('#') {
        Some((url, frag)) => {
            let meta = frag
                .split_once('|')
                .map(|(a, t)| (frag_decode(a), frag_decode(t)));
            (url.to_string(), meta)
        }
        None => (id.to_string(), None),
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
        let (url, meta) = split_fragment(track_id);
        if !url.starts_with("http") {
            return Err(Error::Other(format!("relisten: bad id '{track_id}'")));
        }
        let (name, artists) = match meta {
            Some((artist, title)) => (title, vec![artist]),
            None => (
                url.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("Live track")
                    .trim_end_matches(".mp3")
                    .trim_end_matches(".flac")
                    .replace('_', " "),
                Vec::new(),
            ),
        };
        Ok(TrackInfo {
            name,
            artists,
            codec: if url.ends_with(".flac") {
                CodecFlags::FLAC
            } else {
                CodecFlags::MP3
            },
            id: Some(url),
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
        let (url, _) = split_fragment(track_id);
        if !url.starts_with("http") {
            return Err(Error::Other(format!("relisten: bad id '{track_id}'")));
        }
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(url),
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
        let slug = album_id
            .trim_start_matches("https://relisten.net/artists/")
            .trim_matches('/')
            .to_string();
        let artist = artist_name_from_slug(&slug);

        // Most recent year with shows.
        let years = fetch_json(&self.client, &format!("{API}/artists/{slug}/years")).await?;
        let year = years
            .as_array()
            .and_then(|a| a.last())
            .and_then(|y| y.get("year"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Other("relisten: no years".into()))?
            .to_string();

        let year_data = fetch_json(&self.client, &format!("{API}/artists/{slug}/years/{year}")).await?;
        let show = year_data
            .get("shows")
            .and_then(|s| s.as_array())
            .and_then(|a| a.last())
            .cloned()
            .ok_or_else(|| Error::Other("relisten: no shows".into()))?;
        let date = show
            .get("display_date")
            .or_else(|| show.get("date"))
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        if date.is_empty() {
            return Err(Error::Other("relisten: show without date".into()));
        }

        let detail = fetch_json(&self.client, &format!("{API}/artists/{slug}/shows/{date}")).await?;
        let mut tracks: Vec<TrackRef> = Vec::new();
        if let Some(sources) = detail.get("sources").and_then(|s| s.as_array()) {
            for source in sources {
                let Some(sets) = source.get("sets").and_then(|s| s.as_array()) else {
                    continue;
                };
                for set in sets {
                    let Some(set_tracks) = set.get("tracks").and_then(|t| t.as_array()) else {
                        continue;
                    };
                    for t in set_tracks {
                        let url = t
                            .get("mp3_url")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                            .or_else(|| {
                                t.get("flac_url")
                                    .and_then(|v| v.as_str())
                                    .filter(|s| !s.is_empty())
                                    .map(|s| s.to_string())
                            });
                        let Some(url) = url else { continue };
                        let name = t
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Live track")
                            .to_string();
                        if tracks.iter().any(|tr| tr.id() == url) {
                            continue;
                        }
                        tracks.push(TrackRef::Full(Box::new(TrackInfo {
                            name: name.clone(),
                            artists: vec![artist.clone()],
                            codec: if url.ends_with(".flac") {
                                CodecFlags::FLAC
                            } else {
                                CodecFlags::MP3
                            },
                            id: Some(format!(
                                "{url}#{}|{}",
                                frag_enc(&artist),
                                frag_enc(&name)
                            )),
                            ..Default::default()
                        })));
                    }
                }
                if !tracks.is_empty() {
                    break; // first source that yields playable tracks
                }
            }
        }
        if tracks.is_empty() {
            return Err(Error::Other("relisten: show has no playable tracks".into()));
        }
        Ok(AlbumInfo {
            name: format!("{artist} Live {date}"),
            artist: artist.clone(),
            tracks,
            release_year: year.parse().unwrap_or(0),
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
        Err(Error::Other("relisten: no cover".into()))
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        // The resolver passes "artist - title"; the artist search matches
        // one name only, so try the full string, then each part.
        let mut candidates: Vec<String> = vec![query.replace(" - ", " ")];
        if let Some(idx) = query.find(" - ") {
            candidates.push(query[..idx].to_string());
            candidates.push(query[idx + 3..].to_string());
        }
        let mut out = Vec::new();
        let max = limit.clamp(1, 50) as usize;
        for cand in &candidates {
            let Ok(v) = fetch_json(&self.client, &format!("{API}/search?q={}", enc(cand))).await
            else {
                continue;
            };
            if let Some(artists) = v.get("Artists").and_then(|a| a.as_array()) {
                for a in artists {
                    let slug = a.get("slug").and_then(|s| s.as_str()).unwrap_or("");
                    let name = a.get("name").and_then(|s| s.as_str()).unwrap_or("");
                    if slug.is_empty() || name.is_empty() {
                        continue;
                    }
                    let url = format!("https://relisten.net/artists/{slug}");
                    if out.iter().any(|r: &SearchResult| r.result_id == url) {
                        continue;
                    }
                    out.push(SearchResult {
                        result_id: url,
                        name: Some(name.to_string()),
                        artists: Some(vec![name.to_string()]),
                        ..Default::default()
                    });
                }
            }
            if out.len() >= max {
                break;
            }
        }
        out.truncate(max);
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
