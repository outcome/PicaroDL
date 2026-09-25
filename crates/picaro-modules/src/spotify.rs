//! Spotify module - Web API metadata/search port.
//!
//! The Python OrpheusDL downloads via librespot (CDN decrypt) which has no
//! Rust equivalent in this workspace yet. This module ports everything that
//! works over the public Web API (track/album/playlist/artist metadata +
//! search + covers + credits) and returns a clear error for audio downloads,
//! directing users to the Python build for Spotify audio.
//!
//! Auth: paste a user OAuth `access_token` into session settings, or set
//! `client_id`/`client_secret` for anonymous client-credentials metadata.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const API_BASE: &str = "https://api.spotify.com/v1/";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "Spotify".to_string(),
        module_supported_modes: ModuleModes::download | ModuleModes::lyrics,
        global_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("client_id".to_string(), json!(""));
            m.insert("client_secret".to_string(), json!(""));
            m
        },
        global_storage_variables: vec![],
        session_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("access_token".to_string(), json!(""));
            m.insert("refresh_token".to_string(), json!(""));
            m
        },
        session_storage_variables: vec!["access_token".to_string(), "refresh_token".to_string()],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Multi(vec![
            "spotify".to_string(),
            "open.spotify".to_string(),
        ]),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("track".to_string(), DownloadType::track);
            m.insert("album".to_string(), DownloadType::album);
            m.insert("playlist".to_string(), DownloadType::playlist);
            m.insert("artist".to_string(), DownloadType::artist);
            m.insert("episode".to_string(), DownloadType::track);
            m.insert("show".to_string(), DownloadType::album);
            m
        },
        test_url: Some("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQ".to_string()),
        url_decoding: ManualEnum::Picaro,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(SpotifyConstructor)
}

#[derive(Debug)]
struct SpotifyConstructor;

impl ModuleConstructor for SpotifyConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        let access = controller
            .module_settings
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let client_id = controller
            .module_settings
            .get("client_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let client_secret = controller
            .module_settings
            .get("client_secret")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(Arc::new(SpotifyModule {
            controller,
            session: Mutex::new(SpotifySession::new(access, client_id, client_secret)),
        }))
    }
}

#[derive(Debug, Clone)]
struct SpotifySession {
    access_token: Option<String>,
    client_id: String,
    client_secret: String,
    client: reqwest::Client,
}

impl SpotifySession {
    fn new(access: String, client_id: String, client_secret: String) -> Self {
        Self {
            access_token: if access.is_empty() {
                None
            } else {
                Some(access)
            },
            client_id,
            client_secret,
            client: picaro_utils::http::build_client(None),
        }
    }

    async fn ensure_token(&mut self) -> Result<String> {
        if let Some(t) = &self.access_token {
            if !t.is_empty() {
                return Ok(t.clone());
            }
        }
        if self.client_id.is_empty() || self.client_secret.is_empty() {
            return Err(Error::Other("Spotify credentials are required. Paste a user OAuth access_token into session settings (Spotify → access_token), or set client_id/client_secret for metadata-only access.".to_string()));
        }
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let creds = B64.encode(format!("{}:{}", self.client_id, self.client_secret));
        let resp = self
            .client
            .post("https://accounts.spotify.com/api/token")
            .header("Authorization", format!("Basic {creds}"))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Spotify token: {e}")))?;
        if !resp.status().is_success() {
            let t = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Spotify token failed: {t}")));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Spotify token JSON: {e}")))?;
        let tok = v
            .get("access_token")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if tok.is_empty() {
            return Err(Error::Other("Spotify token empty".to_string()));
        }
        self.access_token = Some(tok.clone());
        Ok(tok)
    }

    async fn api_get(&mut self, path: &str, params: HashMap<String, String>) -> Result<Value> {
        let tok = self.ensure_token().await?;
        let url = format!("{API_BASE}{path}");
        self.api_get_url(&url, &tok, params, path).await
    }

    async fn api_get_url(
        &self,
        url: &str,
        token: &str,
        params: HashMap<String, String>,
        label: &str,
    ) -> Result<Value> {
        let resp = self
            .client
            .get(url)
            .query(&params)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Spotify {label}: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("Spotify body: {e}")))?;
        if !status.is_success() {
            return Err(Error::Other(format!(
                "Spotify {label} HTTP {status}: {text}"
            )));
        }
        serde_json::from_str(&text).map_err(|e| Error::Other(format!("Spotify JSON: {e}")))
    }

    /// Follow a Spotify `next` URL (absolute Web API URL) with the current token.
    async fn api_get_next(&mut self, next_url: &str) -> Result<Value> {
        let tok = self.ensure_token().await?;
        self.api_get_url(next_url, &tok, HashMap::new(), "next")
            .await
    }

    async fn get_several_tracks(&mut self, ids: &[String]) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(50) {
            let mut p = HashMap::new();
            p.insert("ids".to_string(), chunk.join(","));
            let v = self.api_get("tracks", p).await?;
            out.extend(
                v.get("tracks")
                    .and_then(|t| t.as_array())
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        Ok(out)
    }
}

#[derive(Debug)]
struct SpotifyModule {
    controller: ModuleController,
    session: Mutex<SpotifySession>,
}

impl SpotifyModule {
    fn artists_of(v: &Value) -> Vec<String> {
        v.get("artists")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        a.get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    fn cover_of(v: &Value, album: Option<&Value>) -> String {
        let imgs = v.get("images").and_then(|i| i.as_array()).or_else(|| {
            album
                .and_then(|a| a.get("images"))
                .and_then(|i| i.as_array())
        });
        imgs.and_then(|arr| arr.first())
            .and_then(|i| i.get("url"))
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string()
    }
    /// Map a Web API `episodes/{id}` object to TrackInfo.
    /// Mirrors `SpotifyAPI.get_episode_info` in `spotify_api.py`: show → album,
    /// publisher → artist, show images preferred over episode images.
    fn episode_to_track(id: &str, ep: &Value) -> TrackInfo {
        let show = ep.get("show").cloned().unwrap_or(Value::Null);
        let album_name = show
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Show")
            .to_string();
        let album_id = show
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let publisher = show
            .get("publisher")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Publisher")
            .to_string();
        let show_cover = show
            .get("images")
            .and_then(|i| i.as_array())
            .and_then(|a| a.first())
            .and_then(|i| i.get("url"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let ep_cover = ep
            .get("images")
            .and_then(|i| i.as_array())
            .and_then(|a| a.first())
            .and_then(|i| i.get("url"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let cover = if !show_cover.is_empty() {
            show_cover
        } else {
            ep_cover
        };
        let release_date = ep
            .get("release_date")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let total_episodes = show
            .get("total_episodes")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);
        TrackInfo {
            id: Some(id.to_string()),
            name: ep.get("name").and_then(|v| v.as_str()).unwrap_or("Unknown Episode").to_string(),
            album: album_name.clone(),
            album_id,
            artists: vec![publisher.clone()],
            tags: Tags {
                album_artist: Some(publisher),
                release_date,
                disc_number: Some(1),
                track_number: Some(1),
                total_tracks: total_episodes,
                track_url: Some(format!("https://open.spotify.com/episode/{id}")),
                description: ep.get("description").and_then(|v| v.as_str()).map(|s| s.to_string()),
                ..Default::default()
            },
            codec: CodecFlags::VORBIS,
            cover_url: cover,
            release_year: ep.get("release_date").and_then(|v| v.as_str()).and_then(|s| s.split('-').next()).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0),
            duration: ep.get("duration_ms").and_then(|v| v.as_u64()).map(|ms| (ms / 1000) as u32),
            explicit: ep.get("explicit").and_then(|v| v.as_bool()),
            preview_url: None,
            download_extra_kwargs: {
                let mut m = serde_json::Map::new();
                m.insert("is_episode".to_string(), Value::Bool(true));
                m.insert("episode_id".to_string(), Value::String(id.to_string()));
                m
            },
            error: Some("Spotify audio downloads require librespot/CDN decryption which is not yet ported to Rust. Use the Python OrpheusDL for Spotify audio, or use this module for metadata/search.".to_string()),
            ..Default::default()
        }
    }
    fn is_not_found(e: &Error) -> bool {
        let s = e.to_string().to_lowercase();
        s.contains("404") || s.contains("not found")
    }
}

impl SpotifyModule {
    /// Fetch shows/{id} and map to AlbumInfo (episodes → tracks).
    /// Mirrors `SpotifyAPI.get_show_info`'s album-like dict mapping.
    async fn get_show_as_album(&self, id: &str) -> Result<AlbumInfo> {
        let mut params = HashMap::new();
        params.insert("market".to_string(), "US".to_string());
        let show = self
            .session
            .lock()
            .await
            .api_get(&format!("shows/{id}"), params)
            .await?;
        let name = show
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Show")
            .to_string();
        let publisher = show
            .get("publisher")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mut episodes: Vec<Value> = show
            .get("episodes")
            .and_then(|e| e.get("items"))
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        let mut next = show
            .get("episodes")
            .and_then(|e| e.get("next"))
            .and_then(|n| n.as_str())
            .map(|s| s.to_string());
        let mut guard = 0;
        while let Some(url) = next.take() {
            guard += 1;
            if guard > 50 {
                break;
            }
            let v = self.session.lock().await.api_get_next(&url).await?;
            let items = v
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            let nxt = v
                .get("next")
                .and_then(|n| n.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
            if items.is_empty() {
                next = nxt;
                if next.is_none() {
                    break;
                } else {
                    continue;
                }
            }
            episodes.extend(items);
            next = nxt;
        }
        let tracks: Vec<TrackRef> = episodes
            .iter()
            .filter_map(|e| {
                e.get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| TrackRef::Id(s.to_string()))
            })
            .collect();
        let release_year = episodes
            .first()
            .and_then(|e| e.get("release_date"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.split('-').next())
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);
        Ok(AlbumInfo {
            id: Some(id.to_string()),
            name,
            artist: publisher.clone(),
            tracks,
            release_year,
            cover_url: Self::cover_of(&show, None).into(),
            label: if publisher.is_empty() {
                None
            } else {
                Some(publisher)
            },
            track_extra_kwargs: {
                let mut m = serde_json::Map::new();
                m.insert("is_show".to_string(), Value::Bool(true));
                m
            },
            ..Default::default()
        })
    }
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for SpotifyModule {
    fn name(&self) -> &str {
        "Spotify"
    }

    fn is_authenticated(&self) -> bool {
        self.session
            .try_lock()
            .map(|s| {
                s.access_token
                    .as_ref()
                    .map(|t| !t.is_empty())
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    async fn get_track_info(
        &self,
        track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        let id = track_id
            .split('?')
            .next()
            .unwrap_or(track_id)
            .split('/')
            .last()
            .unwrap_or(track_id);
        // Explicit episode request (URL type or extra kwargs) goes straight to episodes/{id},
        // mirroring interface.py's prefer_episode path.
        let wants_episode = track_id.contains("/episode/")
            || _data
                .get("is_episode")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            || _data
                .get("spotify_media_type")
                .and_then(|v| v.as_str())
                .map(|s| s == "episode")
                .unwrap_or(false);
        if wants_episode {
            let ep = self
                .session
                .lock()
                .await
                .api_get(&format!("episodes/{id}"), HashMap::new())
                .await?;
            return Ok(Self::episode_to_track(id, &ep));
        }
        match self
            .session
            .lock()
            .await
            .api_get(&format!("tracks/{id}"), HashMap::new())
            .await
        {
            Ok(t) => {
                let album = t.get("album").cloned().unwrap_or(Value::Null);
                Ok(TrackInfo {
                    id: Some(id.to_string()),
            name: t.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            album: album.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            album_id: album.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            artists: Self::artists_of(&t),
            tags: Tags {
                track_number: t.get("track_number").and_then(|v| v.as_u64()).map(|n| n as u32),
                disc_number: t.get("disc_number").and_then(|v| v.as_u64()).map(|n| n as u32),
                isrc: t.get("external_ids").and_then(|e| e.get("isrc")).and_then(|v| v.as_str()).map(|s| s.to_string()),
                release_date: album.get("release_date").and_then(|v| v.as_str()).map(|s| s.to_string()),
                track_url: Some(format!("https://open.spotify.com/track/{id}")),
                ..Default::default()
            },
            codec: CodecFlags::VORBIS,
            cover_url: Self::cover_of(&t, Some(&album)),
            release_year: album.get("release_date").and_then(|v| v.as_str()).and_then(|s| s.split('-').next()).and_then(|s| s.parse::<i32>().ok()).unwrap_or(0),
            duration: t.get("duration_ms").and_then(|v| v.as_u64()).map(|ms| (ms / 1000) as u32),
            explicit: t.get("explicit").and_then(|v| v.as_bool()),
            artist_id: t.get("artists").and_then(|a| a.as_array()).and_then(|a| a.first()).and_then(|a| a.get("id")).and_then(|v| v.as_str()).map(|s| s.to_string()),
            preview_url: t.get("preview_url").and_then(|v| v.as_str()).map(|s| s.to_string()),
            error: Some("Spotify audio downloads require librespot/CDN decryption which is not yet ported to Rust. Use the Python OrpheusDL for Spotify audio, or use this module for metadata/search.".to_string()),
            ..Default::default()
        })
            }
            Err(e) if Self::is_not_found(&e) => {
                // Fall back to episodes/{id}, mirroring interface.py's episode fallback.
                let ep = self
                    .session
                    .lock()
                    .await
                    .api_get(&format!("episodes/{id}"), HashMap::new())
                    .await?;
                Ok(Self::episode_to_track(id, &ep))
            }
            Err(e) => Err(e),
        }
    }

    async fn get_track_download(
        &self,
        _track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "Spotify".to_string(),
            ability: "audio download (librespot/CDN decrypt not yet ported to Rust; use Python OrpheusDL for Spotify audio)".to_string(),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let id = album_id
            .split('?')
            .next()
            .unwrap_or(album_id)
            .split('/')
            .last()
            .unwrap_or(album_id);
        let wants_show = album_id.contains("/show/");
        let album_res = if wants_show {
            Err(Error::Other("prefer show".to_string()))
        } else {
            self.session
                .lock()
                .await
                .api_get(&format!("albums/{id}"), HashMap::new())
                .await
        };
        let a = match album_res {
            Ok(a) => a,
            Err(e) if wants_show || Self::is_not_found(&e) => {
                // Show fallback: shows/{id} + paginated episodes, mirroring get_show_info.
                return self.get_show_as_album(id).await;
            }
            Err(e) => return Err(e),
        };
        // paginate tracks
        let mut track_ids: Vec<TrackRef> = Vec::new();
        let mut next: Option<String> = Some(format!("albums/{id}/tracks?limit=50&offset=0"));
        let mut guard = 0;
        while let Some(path) = next.take() {
            guard += 1;
            if guard > 20 {
                break;
            }
            let (ep, query) = match path.split_once('?') {
                Some((e, q)) => (e.to_string(), q.to_string()),
                None => (path.clone(), String::new()),
            };
            let mut p = HashMap::new();
            for kv in query.split('&') {
                if let Some((k, v)) = kv.split_once('=') {
                    p.insert(k.to_string(), v.to_string());
                }
            }
            let v = self.session.lock().await.api_get(&ep, p).await?;
            for t in v
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default()
            {
                if let Some(tid) = t.get("id").and_then(|v| v.as_str()) {
                    track_ids.push(TrackRef::Id(tid.to_string()));
                }
            }
            next = v
                .get("next")
                .and_then(|n| n.as_str())
                .map(|u| u.replace("https://api.spotify.com/v1/", ""))
                .filter(|s| !s.is_empty());
        }
        Ok(AlbumInfo {
            id: Some(id.to_string()),
            name: a
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artist: a
                .get("artists")
                .and_then(|x| x.as_array())
                .and_then(|x| x.first())
                .and_then(|x| x.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tracks: track_ids,
            release_year: a
                .get("release_date")
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0),
            cover_url: Self::cover_of(&a, None).into(),
            upc: a
                .get("external_ids")
                .and_then(|e| e.get("upc"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            label: a
                .get("label")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        let id = playlist_id
            .split('?')
            .next()
            .unwrap_or(playlist_id)
            .split('/')
            .last()
            .unwrap_or(playlist_id);
        let p = self
            .session
            .lock()
            .await
            .api_get(&format!("playlists/{id}"), HashMap::new())
            .await?;
        let mut tracks: Vec<TrackRef> = Vec::new();
        // First page is embedded under tracks.items; follow `next` until exhausted,
        // mirroring the embed-API pagination in spotify_api.py.
        let mut next: Option<String> = p
            .get("tracks")
            .and_then(|t| t.get("next"))
            .and_then(|n| n.as_str())
            .map(|s| s.to_string());
        if let Some(items) = p
            .get("tracks")
            .and_then(|t| t.get("items"))
            .and_then(|i| i.as_array())
        {
            for it in items {
                // Playlist items may be tracks or episodes; both carry track.id.
                if let Some(t) = it
                    .get("track")
                    .and_then(|t| t.get("id"))
                    .and_then(|v| v.as_str())
                {
                    tracks.push(TrackRef::Id(t.to_string()));
                }
            }
        }
        let mut guard = 0;
        while let Some(url) = next.take() {
            guard += 1;
            if guard > 50 {
                break;
            }
            let v = self.session.lock().await.api_get_next(&url).await?;
            // `next` pages return either {items,next} or {tracks:{items,next}}.
            let (items, nxt) = if let Some(arr) = v.get("items").and_then(|i| i.as_array()) {
                (
                    arr.clone(),
                    v.get("next")
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string()),
                )
            } else if let Some(t) = v.get("tracks") {
                (
                    t.get("items")
                        .and_then(|i| i.as_array())
                        .cloned()
                        .unwrap_or_default(),
                    t.get("next")
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string()),
                )
            } else {
                (Vec::new(), None)
            };
            for it in &items {
                // Paginated pages may be bare tracks or {track:{...}} wrappers.
                let tid = it.get("id").and_then(|v| v.as_str()).or_else(|| {
                    it.get("track")
                        .and_then(|t| t.get("id"))
                        .and_then(|v| v.as_str())
                });
                if let Some(t) = tid {
                    tracks.push(TrackRef::Id(t.to_string()));
                }
            }
            next = nxt.filter(|s| !s.is_empty());
            if items.is_empty() {
                break;
            }
        }
        Ok(PlaylistInfo {
            id: Some(id.to_string()),
            name: p
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            creator: p
                .get("owner")
                .and_then(|o| o.get("display_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tracks,
            release_year: 0,
            cover_url: Self::cover_of(&p, None).into(),
            description: p
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            ..Default::default()
        })
    }

    async fn get_artist_info(
        &self,
        artist_id: &str,
        _credited: bool,
        _name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        let id = artist_id
            .split('?')
            .next()
            .unwrap_or(artist_id)
            .split('/')
            .last()
            .unwrap_or(artist_id);
        let a = self
            .session
            .lock()
            .await
            .api_get(&format!("artists/{id}"), HashMap::new())
            .await?;
        let name = a
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(id)
            .to_string();
        let mut p = HashMap::new();
        p.insert(
            "include_groups".to_string(),
            "album,single,compilation,appears_on".to_string(),
        );
        p.insert("limit".to_string(), "50".to_string());
        let alb = self
            .session
            .lock()
            .await
            .api_get(&format!("artists/{id}/albums"), p)
            .await
            .unwrap_or(json!({"items": []}));
        let albums: Vec<Value> = alb
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|x| {
                json!({
                    "id": x.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                    "name": x.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    "artist": name,
                })
            })
            .collect();
        Ok(ArtistInfo {
            name,
            artist_id: Some(id.to_string()),
            albums,
            ..Default::default()
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
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        let id = track_id
            .split('?')
            .next()
            .unwrap_or(track_id)
            .split('/')
            .last()
            .unwrap_or(track_id);
        let t = self
            .session
            .lock()
            .await
            .api_get(&format!("tracks/{id}"), HashMap::new())
            .await?;
        let album = t.get("album").cloned().unwrap_or(Value::Null);
        Ok(CoverInfo {
            url: Self::cover_of(&t, Some(&album)),
            file_type: ImageFileType::Jpg,
        })
    }

    async fn search(
        &self,
        query_type: DownloadType,
        query: &str,
        _ti: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        // Web API search types mirror spotify_api.py's CLI: album/track/artist/
        // playlist/show/episode. Show/episode buckets pluralize regularly.
        let stype = match query_type {
            DownloadType::track => "track",
            DownloadType::album => "album",
            DownloadType::artist => "artist",
            DownloadType::playlist => "playlist",
            _ => "track",
        };
        let mut p = HashMap::new();
        p.insert("q".to_string(), query.to_string());
        p.insert("type".to_string(), stype.to_string());
        p.insert("limit".to_string(), limit.min(50).to_string());
        let v = self.session.lock().await.api_get("search", p).await?;
        let bucket = format!("{stype}s");
        let items = v
            .get(&bucket)
            .and_then(|b| b.get("items"))
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(items
            .into_iter()
            .map(|item| {
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                // Artists: owner display_name for playlists, artists[] otherwise
                // (mirrors interface.py search mapping).
                let artists = if stype == "playlist" {
                    item.get("owner")
                        .and_then(|o| o.get("display_name").or_else(|| o.get("name")))
                        .and_then(|v| v.as_str())
                        .map(|s| vec![s.to_string()])
                } else if stype == "artist" {
                    None
                } else {
                    Some(Self::artists_of(&item))
                };
                // Year: track → album.release_date, otherwise release_date.
                let release_date = if stype == "track" {
                    item.get("album")
                        .and_then(|a| a.get("release_date"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    item.get("release_date")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                };
                let year = release_date
                    .as_deref()
                    .and_then(|s| s.split('-').next())
                    .map(|s| s.to_string());
                // Image: tracks nest images under album.
                let image_url = if stype == "track" {
                    let album = item.get("album").cloned().unwrap_or(Value::Null);
                    let u = Self::cover_of(&item, Some(&album));
                    if u.is_empty() {
                        None
                    } else {
                        Some(u)
                    }
                } else {
                    let u = Self::cover_of(&item, None);
                    if u.is_empty() {
                        None
                    } else {
                        Some(u)
                    }
                };
                // Additional: album name for tracks (mirrors interface.py additional column).
                let additional = if stype == "track" {
                    item.get("album")
                        .and_then(|a| a.get("name"))
                        .and_then(|v| v.as_str())
                        .map(|s| vec![s.to_string()])
                } else if stype == "album" {
                    item.get("total_tracks").and_then(|v| v.as_u64()).map(|n| {
                        vec![if n == 1 {
                            "1 track".to_string()
                        } else {
                            format!("{n} tracks")
                        }]
                    })
                } else {
                    None
                };
                SearchResult {
                    result_id: id,
                    name,
                    artists,
                    year,
                    image_url,
                    // preview_url is often null (deprecated); GUI lazy-loads from embed.
                    preview_url: item
                        .get("preview_url")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    additional,
                    duration: item
                        .get("duration_ms")
                        .and_then(|v| v.as_u64())
                        .map(|ms| (ms / 1000) as u32),
                    explicit: item.get("explicit").and_then(|v| v.as_bool()),
                    ..Default::default()
                }
            })
            .collect())
    }

    async fn get_preview_stream_url(&self, track_id: &str) -> Result<Option<String>> {
        let id = track_id
            .split('?')
            .next()
            .unwrap_or(track_id)
            .split('/')
            .last()
            .unwrap_or(track_id);
        let t = self
            .session
            .lock()
            .await
            .api_get(&format!("tracks/{id}"), HashMap::new())
            .await?;
        Ok(t.get("preview_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()))
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_maps_like_python_get_episode_info() {
        let ep = json!({
            "name": "Ep One",
            "description": "desc",
            "duration_ms": 1800000u64,
            "explicit": true,
            "release_date": "2024-05-01",
            "images": [{"url": "https://ep.img/x.jpg"}],
            "show": {
                "id": "show1",
                "name": "My Show",
                "publisher": "Pub",
                "total_episodes": 10u64,
                "images": [{"url": "https://show.img/y.jpg"}]
            }
        });
        let t = SpotifyModule::episode_to_track("ep1", &ep);
        assert_eq!(t.name, "Ep One");
        assert_eq!(t.album, "My Show");
        assert_eq!(t.album_id, "show1");
        assert_eq!(t.artists, vec!["Pub".to_string()]);
        assert_eq!(t.duration, Some(1800));
        assert_eq!(t.explicit, Some(true));
        assert_eq!(t.release_year, 2024);
        // Show cover preferred over episode cover.
        assert_eq!(t.cover_url, "https://show.img/y.jpg");
        assert_eq!(
            t.tags.track_url,
            Some("https://open.spotify.com/episode/ep1".to_string())
        );
        assert_eq!(t.tags.total_tracks, Some(10));
        assert!(t.error.unwrap_or_default().contains("librespot"));
    }
}
