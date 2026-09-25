//! LRCLIB module - port of `modules/lrclib/interface.py`.
//!
//! LRCLIB is a free, open lyrics service. No authentication required.

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

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "LRCLIB".to_string(),
        module_supported_modes: ModuleModes::lyrics,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("lrclib".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: None,
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(LrclibConstructor)
}

#[derive(Debug)]
struct LrclibConstructor;

impl ModuleConstructor for LrclibConstructor {
    fn construct(&self, _controller: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(LrclibModule {}))
    }
}

#[derive(Debug)]
struct LrclibModule;

impl LrclibModule {
    fn clean(name: &str) -> String {
        let re = Regex::new(r"[\(\[].*?[\)\]]").unwrap();
        let cleaned = re.replace_all(name, "").trim().to_string();
        if cleaned.is_empty() {
            name.to_string()
        } else {
            cleaned
        }
    }

    async fn try_get(
        client: &reqwest::Client,
        track_name: &str,
        artist_name: &str,
        album_name: Option<&str>,
        duration: Option<u32>,
    ) -> Result<Option<Value>> {
        let mut url = format!(
            "https://lrclib.net/api/get?track_name={}&artist_name={}",
            urlencoded(track_name),
            urlencoded(artist_name)
        );
        if let Some(alb) = album_name {
            url.push_str(&format!("&album_name={}", urlencoded(alb)));
        }
        if let Some(d) = duration {
            url.push_str(&format!("&duration={d}"));
        }
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let v: Value = resp.json().await?;
        Ok(Some(v))
    }
}

fn urlencoded(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for LrclibModule {
    fn name(&self) -> &str {
        "LRCLIB"
    }

    async fn get_track_info(
        &self,
        _track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "LRCLIB".into(),
            ability: "track_info".into(),
        })
    }

    async fn get_track_download(
        &self,
        _track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "LRCLIB".into(),
            ability: "download".into(),
        })
    }

    async fn get_album_info(
        &self,
        _album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "LRCLIB".into(),
            ability: "album".into(),
        })
    }

    async fn get_playlist_info(
        &self,
        _playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "LRCLIB".into(),
            ability: "playlist".into(),
        })
    }

    async fn get_artist_info(
        &self,
        _artist_id: &str,
        _get_credited: bool,
        _artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "LRCLIB".into(),
            ability: "artist".into(),
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
        Err(Error::ModuleDoesNotSupportAbility {
            module: "LRCLIB".into(),
            ability: "cover".into(),
        })
    }

    async fn get_track_lyrics(
        &self,
        _track_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<LyricsInfo> {
        // Mirrors interface.py get_track_lyrics: prefer embedded lyrics_data,
        // else fetch by LRCLIB id (api/get/{id}).
        let lyrics = if let Some(d) = data.get("lyrics_data") {
            Some(d.clone())
        } else if !_track_id.is_empty() {
            let client = reqwest::Client::new();
            let url = format!("https://lrclib.net/api/get/{}", urlencoded(_track_id));
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => resp.json::<Value>().await.ok(),
                _ => None,
            }
        } else {
            None
        };
        let Some(lyrics) = lyrics else {
            return Ok(LyricsInfo::default());
        };
        let embedded = lyrics
            .get("plainLyrics")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let synced = lyrics
            .get("syncedLyrics")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(LyricsInfo { embedded, synced })
    }

    async fn search(
        &self,
        query_type: DownloadType,
        query: &str,
        track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        if query_type != DownloadType::track {
            return Ok(Vec::new());
        }
        let client = reqwest::Client::new();
        let mut results: Vec<Value> = Vec::new();
        if let Some(ti) = track_info {
            let artist = ti
                .artists
                .first()
                .cloned()
                .unwrap_or_else(|| "Unknown Artist".to_string());
            let cleaned = Self::clean(&ti.name);
            // Strategies 1a-1d mirror interface.py: strict → cleaned → no album →
            // no album/duration. Skip 1b when cleaning is a no-op.
            let mut attempts: Vec<(String, String, Option<String>, Option<u32>)> = vec![(
                ti.name.clone(),
                artist.clone(),
                Some(ti.album.clone()),
                ti.duration,
            )];
            if cleaned != ti.name {
                attempts.push((
                    cleaned.clone(),
                    artist.clone(),
                    Some(ti.album.clone()),
                    ti.duration,
                ));
            }
            attempts.push((cleaned.clone(), artist.clone(), None, ti.duration));
            attempts.push((cleaned.clone(), artist.clone(), None, None));
            for (tn, an, alb, dur) in attempts {
                if let Some(v) = Self::try_get(&client, &tn, &an, alb.as_deref(), dur)
                    .await
                    .ok()
                    .flatten()
                {
                    if v.get("syncedLyrics")
                        .and_then(|x| x.as_str())
                        .map(|s| !s.is_empty())
                        .unwrap_or(false)
                    {
                        return Ok(vec![to_search_result(&v)]);
                    }
                    results.push(v);
                }
            }
        }
        // Fallback: search
        let search_query = if let Some(ti) = track_info {
            format!(
                "{} {}",
                Self::clean(&ti.name),
                ti.artists.first().cloned().unwrap_or_default()
            )
        } else {
            query.to_string()
        };
        let url = format!(
            "https://lrclib.net/api/search?q={}",
            urlencoded(&search_query)
        );
        if let Ok(resp) = client.get(&url).send().await {
            if let Ok(arr) = resp.json::<Vec<Value>>().await {
                // Synced results first, mirroring interface.py's insert(0).
                for r in arr {
                    if r.get("syncedLyrics")
                        .and_then(|x| x.as_str())
                        .map(|s| !s.is_empty())
                        .unwrap_or(false)
                    {
                        results.insert(0, r);
                    } else {
                        results.push(r);
                    }
                }
            }
        }
        // Dedupe
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for v in results {
            if let Some(id) = lrclib_id(&v) {
                if seen.insert(id) {
                    out.push(to_search_result(&v));
                }
            }
        }
        Ok(out.into_iter().take(limit as usize).collect())
    }
}

/// LRCLIB ids are ints, but tolerate string form so results are never dropped.
fn lrclib_id(v: &Value) -> Option<String> {
    v.get("id")
        .and_then(|i| i.as_i64().map(|n| n.to_string()))
        .or_else(|| v.get("id").and_then(|i| i.as_u64().map(|n| n.to_string())))
        .or_else(|| v.get("id").and_then(|i| i.as_str().map(|s| s.to_string())))
}

fn to_search_result(v: &Value) -> SearchResult {
    SearchResult {
        result_id: lrclib_id(v).unwrap_or_default(),
        name: v
            .get("trackName")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string()),
        artists: v
            .get("artistName")
            .and_then(|a| a.as_str())
            .map(|s| vec![s.to_string()]),
        duration: v.get("duration").and_then(|d| {
            d.as_u64()
                .map(|n| n as u32)
                .or_else(|| d.as_f64().map(|f| f as u32))
        }),
        extra_kwargs: {
            let mut m = serde_json::Map::new();
            m.insert("lyrics_data".to_string(), v.clone());
            m
        },
        ..Default::default()
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_version_tags() {
        assert_eq!(LrclibModule::clean("Song (Remastered)"), "Song");
        assert_eq!(LrclibModule::clean("Song [Explicit]"), "Song");
        assert_eq!(LrclibModule::clean("Plain"), "Plain");
    }

    #[test]
    fn search_result_fields_match_python() {
        let v = serde_json::json!({
            "id": 123,
            "trackName": "T",
            "artistName": "A",
            "duration": 200.5,
        });
        let r = to_search_result(&v);
        assert_eq!(r.result_id, "123");
        assert_eq!(r.name.as_deref(), Some("T"));
        assert_eq!(r.artists, Some(vec!["A".to_string()]));
        assert_eq!(r.duration, Some(200));
        assert!(r.extra_kwargs.contains_key("lyrics_data"));
    }
}
