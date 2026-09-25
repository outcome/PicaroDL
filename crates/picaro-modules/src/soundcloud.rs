//! SoundCloud module - port of `modules/soundcloud/{interface,soundcloud_api}.py`.
//!
//! Auth: optional `web_access_token` (OAuth). Anonymous `client_id` works for
//! public metadata + progressive streams.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const API_BASE: &str = "https://api-v2.soundcloud.com/";
// Public web-client id. These rotate every few weeks; when all anonymous
// calls start returning 401, re-extract from the web player assets:
// fetch https://soundcloud.com/, download the a-v2.sndcdn.com asset JS
// files and grep for `client_id:"<32 alnum>"`.
// Refreshed 2026-09-13 (previous WU4b... id returns 401).
const CLIENT_ID: &str = "Pb72ranhoyt6gw7hM7TkzUItXlMWSNSo";

/// True when an API error looks like a 403/restricted response, mirroring the
/// `_is_restricted` check in `soundcloud_api.py`.
fn is_restricted_error(e: &Error) -> bool {
    let m = format!("{e}").to_lowercase();
    m.contains("403") || m.contains("restricted") || m.contains("not available")
}

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "SoundCloud".to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("web_access_token".to_string(), json!(""));
            m
        },
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Multi(vec![
            "soundcloud".to_string(),
            "on.soundcloud".to_string(),
        ]),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("track".to_string(), DownloadType::track);
            m.insert("album".to_string(), DownloadType::album);
            m.insert("playlist".to_string(), DownloadType::playlist);
            m.insert("sets".to_string(), DownloadType::playlist);
            m.insert("user".to_string(), DownloadType::artist);
            m
        },
        test_url: Some(
            "https://soundcloud.com/alanwalker/darkside-feat-tomine-harket-au".to_string(),
        ),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(SoundCloudConstructor)
}

#[derive(Debug)]
struct SoundCloudConstructor;

impl ModuleConstructor for SoundCloudConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        let token = controller
            .module_settings
            .get("web_access_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        Ok(Arc::new(SoundCloudModule {
            controller,
            session: Mutex::new(SoundCloudSession::new(token)),
            // Plan is detected lazily via `me/` on first use (see `ensure_plan`),
            // since the constructor is synchronous and cannot do async I/O.
            plan: Mutex::new(None),
        }))
    }
}

#[derive(Debug, Clone)]
struct SoundCloudSession {
    access_token: String,
    client: reqwest::Client,
}

impl SoundCloudSession {
    fn new(access_token: String) -> Self {
        Self {
            access_token,
            client: picaro_utils::http::build_client_with_user_agent(
                None,
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            ),
        }
    }

    async fn api_get(
        &self,
        path_or_url: &str,
        mut params: HashMap<String, String>,
    ) -> Result<Value> {
        params
            .entry("client_id".to_string())
            .or_insert_with(|| CLIENT_ID.to_string());
        let url = if path_or_url.starts_with("http") {
            path_or_url.to_string()
        } else {
            format!("{API_BASE}{path_or_url}")
        };
        let mut req = self
            .client
            .get(&url)
            .query(&params)
            .header("Origin", "https://soundcloud.com")
            .header("Referer", "https://soundcloud.com/");
        if !self.access_token.is_empty() {
            req = req.header("Authorization", format!("OAuth {}", self.access_token));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Other(format!("SoundCloud request: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("SoundCloud body: {e}")))?;
        if !status.is_success() {
            if status.as_u16() == 403 {
                return Err(Error::Other("This track is not available (e.g. restricted in your country or disabled for API access).".to_string()));
            }
            if status.as_u16() == 401 {
                return Err(Error::Other(format!("SoundCloud: Unauthorized (401). Access token or public client ID is invalid or expired. Response: {text}")));
            }
            return Err(Error::Other(format!("SoundCloud HTTP {status}: {text}")));
        }
        serde_json::from_str(&text).map_err(|e| Error::Other(format!("SoundCloud JSON: {e}")))
    }

    async fn resolve_url(&self, url: &str) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("url".to_string(), url.to_string());
        self.api_get("resolve", p).await
    }

    async fn get_track(&self, id: &str) -> Result<Value> {
        self.api_get(&format!("tracks/{id}"), HashMap::new()).await
    }

    async fn search(&self, query_type: &str, query: &str, limit: u32) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("q".to_string(), query.to_string());
        p.insert("limit".to_string(), limit.to_string());
        p.insert("top_results".to_string(), "v2".to_string());
        self.api_get(&format!("search/{query_type}"), p).await
    }

    async fn get_me(&self) -> Result<Value> {
        self.api_get("me", HashMap::new()).await
    }

    /// Fetch a paginated collection, following `next_href` up to `max_pages`
    /// pages. Port of `SoundCloudWebAPI._get_collection_paginated`: handles both
    /// bare-list responses and `{collection, next_href}` dicts.
    async fn get_collection_paginated(
        &self,
        url: &str,
        params: HashMap<String, String>,
        max_pages: u32,
    ) -> Result<Vec<Value>> {
        let mut all_items = Vec::new();
        let mut page_url: Option<String> = Some(url.to_string());
        let mut first_params = Some(params);
        for _ in 0..max_pages {
            let u = match page_url.take() {
                Some(u) => u,
                None => break,
            };
            // `next_href` is an absolute URL that already carries its own query
            // string; only the first (relative) request uses `params`.
            let resp = if u.starts_with("http") {
                self.api_get(&u, HashMap::new()).await?
            } else {
                self.api_get(&u, first_params.take().unwrap_or_default())
                    .await?
            };
            let (collection, next_href): (Vec<Value>, Option<String>) =
                if let Some(arr) = resp.as_array() {
                    (arr.clone(), None)
                } else if resp.is_object() {
                    let c = resp
                        .get("collection")
                        .and_then(|c| c.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let n = resp
                        .get("next_href")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    (c, n)
                } else {
                    (Vec::new(), None)
                };
            for item in collection {
                if item.get("id").is_some() {
                    all_items.push(item);
                }
            }
            match next_href {
                Some(n) => page_url = Some(n),
                None => break,
            }
        }
        Ok(all_items)
    }

    /// Merge `users/{id}/albums` + `users/{id}/tracks`, keyed by id string.
    /// Port of `SoundCloudWebAPI.get_user_albums_tracks`, including the
    /// `toptracks`/`spotlight` fallback when `/tracks` is 403-restricted and
    /// the permalink -> numeric-id resolve for non-numeric ids.
    async fn get_user_albums_tracks(
        &self,
        user_id: &str,
    ) -> (HashMap<String, Value>, HashMap<String, Value>) {
        // Prefer numeric id; resolve permalink to id when user_id is non-numeric.
        let mut uid = user_id.to_string();
        if !uid.chars().all(|c| c.is_ascii_digit()) {
            let mut p = HashMap::new();
            p.insert("url".to_string(), format!("https://soundcloud.com/{uid}"));
            if let Ok(resolved) = self.api_get("resolve", p).await {
                if let Some(id) = resolved.get("id").and_then(|v| v.as_i64()) {
                    uid = id.to_string();
                } else if let Some(urn) = resolved.get("urn").and_then(|v| v.as_str()) {
                    if let Some(last) = urn.split(':').last() {
                        uid = last.to_string();
                    }
                }
            }
        }
        let mut limit = HashMap::new();
        limit.insert("limit".to_string(), "200".to_string());
        let album_items = self
            .get_collection_paginated(&format!("users/{uid}/albums"), limit.clone(), 50)
            .await;
        let track_items = self
            .get_collection_paginated(&format!("users/{uid}/tracks"), limit, 50)
            .await;
        let mut album_data = HashMap::new();
        let mut album_err: Option<Error> = None;
        match album_items {
            Ok(items) => {
                for i in items {
                    if let Some(id) = i.get("id").and_then(|v| v.as_i64()) {
                        album_data.insert(id.to_string(), i);
                    }
                }
            }
            Err(e) => album_err = Some(e),
        }
        let mut track_data = HashMap::new();
        let mut track_err: Option<Error> = None;
        match track_items {
            Ok(items) => {
                for i in items {
                    if let Some(id) = i.get("id").and_then(|v| v.as_i64()) {
                        track_data.insert(id.to_string(), i);
                    }
                }
            }
            Err(e) => track_err = Some(e),
        }
        // When /tracks is restricted (403), try the toptracks/spotlight
        // endpoints, which may wrap tracks as {"track": {...}}.
        if let Some(ref e) = track_err {
            if is_restricted_error(e) {
                let mut fallback_items = Vec::new();
                for endpoint in ["toptracks", "spotlight"] {
                    let mut p = HashMap::new();
                    p.insert("limit".to_string(), "200".to_string());
                    if let Ok(items) = self
                        .get_collection_paginated(&format!("users/{uid}/{endpoint}"), p, 50)
                        .await
                    {
                        for item in items {
                            if let Some(t) = item.get("track").cloned() {
                                fallback_items.push(t);
                            } else if item.get("id").is_some() {
                                fallback_items.push(item);
                            }
                        }
                    }
                }
                if !fallback_items.is_empty() {
                    for i in fallback_items {
                        if let Some(id) = i.get("id").and_then(|v| v.as_i64()) {
                            track_data.insert(id.to_string(), i);
                        }
                    }
                    track_err = None;
                }
            }
        }
        if album_data.is_empty()
            && track_data.is_empty()
            && (album_err.is_some() || track_err.is_some())
        {
            let restricted = album_err.as_ref().map(is_restricted_error).unwrap_or(false)
                || track_err.as_ref().map(is_restricted_error).unwrap_or(false);
            if restricted {
                eprintln!("[SoundCloud] User uid={uid}: content not available (restricted or disabled for API).");
            } else {
                let mut parts = Vec::new();
                if let Some(e) = album_err {
                    parts.push(format!("albums: {e}"));
                }
                if let Some(e) = track_err {
                    parts.push(format!("tracks: {e}"));
                }
                eprintln!(
                    "[SoundCloud] get_user_albums_tracks(uid={uid}) failed: {}",
                    parts.join("; ")
                );
            }
        }
        (album_data, track_data)
    }

    /// Re-fetch stub tracks (playlist/album entries without `streamable`) via
    /// `GET tracks?ids=...`, 50 ids at a time. Port of
    /// `SoundCloudWebAPI.get_tracks_from_tracklist`.
    async fn get_tracks_from_tracklist(&self, track_data: &[Value]) -> HashMap<String, Value> {
        let ids: Vec<String> = track_data
            .iter()
            .filter(|t| t.get("streamable").is_none())
            .filter_map(|t| t.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string()))
            .collect();
        let mut full: HashMap<String, Value> = HashMap::new();
        for chunk in ids.chunks(50) {
            if chunk.is_empty() {
                continue;
            }
            let mut p = HashMap::new();
            p.insert("ids".to_string(), chunk.join(","));
            let resp = match self.api_get("tracks", p).await {
                Ok(v) => v,
                Err(_) => continue,
            };
            let arr = if let Some(a) = resp.as_array() {
                a.clone()
            } else {
                resp.get("collection")
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default()
            };
            for t in arr {
                if let Some(id) = t.get("id").and_then(|v| v.as_i64()) {
                    full.insert(id.to_string(), t);
                }
            }
        }
        let mut out = HashMap::new();
        for t in track_data {
            if let Some(id) = t.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string()) {
                if t.get("streamable").is_some() {
                    out.insert(id, t.clone());
                } else if let Some(f) = full.get(&id) {
                    out.insert(id.clone(), f.clone());
                } else {
                    // Refill failed; keep the stub so the track id is not lost.
                    out.insert(id, t.clone());
                }
            }
        }
        out
    }

    async fn resolve_stream(&self, transcoding_url: &str, track_auth: &str) -> Result<String> {
        // transcoding_url is like https://api-v2.soundcloud.com/media/.../stream/hls
        let rel = transcoding_url
            .split("https://api-v2.soundcloud.com/")
            .nth(1)
            .ok_or_else(|| Error::Other("Invalid SoundCloud stream URL".to_string()))?;
        let mut p = HashMap::new();
        p.insert("track_authorization".to_string(), track_auth.to_string());
        let v = self.api_get(rel, p).await?;
        v.get("url")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                Error::Other("SoundCloud: no stream URL in transcoding response".to_string())
            })
    }
}

#[derive(Debug)]
struct SoundCloudModule {
    controller: ModuleController,
    session: Mutex<SoundCloudSession>,
    /// Subscription plan string detected via `me/` (`Free`, `high_tier`, Go+
    /// product id, ...). `None` until the first `ensure_plan` call.
    plan: Mutex<Option<String>>,
}

impl SoundCloudModule {
    /// Derive the plan string from a `me/` response. Port of the plan
    /// detection in `interface.py::ModuleInterface.__init__`.
    fn detect_plan(me: &Value) -> String {
        if let Some(subs) = me
            .get("consumer_subscriptions")
            .or_else(|| me.get("subscriptions"))
            .and_then(|v| v.as_array())
        {
            if let Some(first) = subs.first() {
                return first
                    .get("product")
                    .and_then(|p| p.get("id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Premium".to_string());
            }
        }
        if let Some(sub) = me.get("consumer_subscription") {
            if !sub.is_null() {
                return sub
                    .get("product")
                    .and_then(|p| p.get("id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Premium".to_string());
            }
        }
        if let Some(quota) = me.get("quota") {
            if quota
                .get("high_tier")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || quota
                    .get("top_tier")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            {
                return "high_tier".to_string();
            }
        }
        if let Some(p) = me.get("plan").and_then(|v| v.as_str()) {
            if !p.eq_ignore_ascii_case("free") {
                return p.to_string();
            }
        }
        "Free".to_string()
    }

    /// Fetch `me/` once (when an access token is configured) and store the
    /// plan string, logging the account status. No-op when unauthenticated
    /// (plan is `Free`) or when already detected.
    async fn ensure_plan(&self) {
        if self.plan.lock().await.is_some() {
            return;
        }
        let has_token = !self.session.lock().await.access_token.is_empty();
        let plan = if has_token {
            match self.session.lock().await.get_me().await {
                Ok(me) => {
                    let username = me
                        .get("username")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown");
                    let p = Self::detect_plan(&me);
                    eprintln!("[SoundCloud] Logged in as {username} ({p})");
                    p
                }
                Err(e) => {
                    eprintln!("[SoundCloud] Authentication check failed: {e}");
                    "Unknown".to_string()
                }
            }
        } else {
            "Free".to_string()
        };
        *self.plan.lock().await = Some(plan);
    }

    /// Build the compact album dict used for artist expansion, mirroring
    /// `interface.py::get_artist_info` (small `-t50x50` thumbnail, track-count
    /// + genre `additional` lines).
    fn summarize_album(aid: &str, a: &Value, fallback_artist: &str) -> Value {
        let title = a.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let release_date = a
            .get("release_date")
            .or_else(|| a.get("display_date"))
            .or_else(|| a.get("created_at"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let release_year = release_date.split('-').next().unwrap_or("").to_string();
        let track_count = a.get("track_count").and_then(|v| v.as_u64()).or_else(|| {
            a.get("tracks")
                .and_then(|t| t.as_array())
                .map(|t| t.len() as u64)
        });
        let genre = a
            .get("genre")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mut additional = Vec::new();
        if let Some(n) = track_count {
            if n > 0 {
                additional.push(if n == 1 {
                    "1 track".to_string()
                } else {
                    format!("{n} tracks")
                });
            }
        }
        if let Some(g) = genre {
            additional.push(g);
        }
        let cover_url = a
            .get("artwork_url")
            .and_then(|v| v.as_str())
            .or_else(|| {
                a.get("user")
                    .and_then(|u| u.get("avatar_url"))
                    .and_then(|v| v.as_str())
            })
            .map(|u| u.replace("-large", "-t50x50"))
            .unwrap_or_default();
        json!({
            "id": aid,
            "name": title,
            "artist": a.get("user").and_then(|u| u.get("username")).and_then(|v| v.as_str()).unwrap_or(fallback_artist),
            "duration": a.get("duration"),
            "release_year": if release_year.is_empty() { Value::Null } else { Value::String(release_year) },
            "cover_url": cover_url,
            "additional": additional,
        })
    }

    fn split_artists(s: &str) -> Vec<String> {
        s.replace(" & ", ", ")
            .replace(" and ", ", ")
            .replace(" x ", ", ")
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    }

    fn original_artwork(url: &str) -> String {
        url.replace("-large", "-original")
    }

    fn release_year(v: &Value) -> i32 {
        for k in ["release_date", "display_date", "created_at"] {
            if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
                if let Some(y) = s.split('-').next().and_then(|y| y.parse::<i32>().ok()) {
                    return y;
                }
            }
        }
        0
    }

    /// Port of `interface.py::_parse_aac_bitrate_from_preset`
    /// (`aac_256k` -> 256, `aac_1_0`/`aac_hq` -> 256, other `aac_*` -> 64).
    fn parse_aac_bitrate(preset: &str) -> i32 {
        if let Some(rest) = preset.strip_prefix("aac_") {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                if let Ok(n) = digits.parse::<i32>() {
                    if rest.chars().nth(digits.len()) == Some('k') {
                        return n;
                    }
                }
            }
            if preset == "aac_1_0" || preset == "aac_hq" {
                return 256;
            }
            return 64;
        }
        0
    }

    /// Port of `interface.py::_parse_progressive_bitrate_from_preset`.
    fn parse_progressive_bitrate(preset: &str, codec_name: &str) -> i32 {
        if codec_name == "OPUS" {
            if preset.contains("abr_hq") {
                return 128;
            }
            if preset.contains("abr_sq") {
                return 96;
            }
        }
        // Generic `(\d+)k` scan (covers mp3_128k, opus_160k, ...).
        let bytes = preset.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() {
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'k' {
                    if let Ok(n) = preset[i..j].parse::<i32>() {
                        return n;
                    }
                }
                i = j;
            } else {
                i += 1;
            }
        }
        if preset == "mp3_0_0" || preset == "mp3_0_1" || preset == "mp3_1_0" {
            return 128;
        }
        if preset == "opus_0_0" {
            return 64;
        }
        let parts: Vec<&str> = preset.split('_').collect();
        if parts.len() > 1 && parts[1].chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = parts[1].parse::<i32>() {
                // Opus `opus_X_Y` quality levels scale like opusenc 0-10.
                if codec_name == "OPUS" && parts[0] == "opus" {
                    if parts.len() > 2 {
                        return n * 12 + 32;
                    }
                    return n;
                }
                if codec_name != "OPUS" {
                    return n;
                }
            }
        }
        0
    }

    /// Tie-break preference for equal bitrates. Port of the
    /// `codec_preference` table in `interface.py::get_track_info`
    /// (HLS AAC wins ties, HLS MP3 loses).
    fn stream_preference(codec: CodecFlags, is_hls: bool) -> i32 {
        if codec == CodecFlags::AAC && is_hls {
            5
        } else if codec == CodecFlags::OPUS && is_hls {
            4
        } else if codec == CodecFlags::OPUS {
            3
        } else if codec == CodecFlags::AAC {
            2
        } else if codec == CodecFlags::MP3 && !is_hls {
            1
        } else {
            0
        }
    }

    fn is_hls_transcoding(protocol: &str, url: &str) -> bool {
        if protocol == "hls" {
            return true;
        }
        let lower = url.to_lowercase();
        lower.contains("/hls")
            || lower.contains(".m3u8")
            || lower.contains("ctr-encrypted-hls")
            || lower.contains("cbc-encrypted-hls")
    }

    fn is_encrypted_hls(url: &str) -> bool {
        let lower = url.to_lowercase();
        lower.contains("ctr-encrypted-hls") || lower.contains("cbc-encrypted-hls")
    }

    /// Score all transcodings by (bitrate, codec preference) and return the
    /// best unresolved transcoding URL + codec + HLS flag, mirroring
    /// `interface.py::get_track_info`. Replaces the old first-progressive-wins
    /// logic, which picked a 128k progressive MP3 even when a 256k HLS AAC was
    /// available, misclassified Opus as AAC, and silently returned
    /// DRM-encrypted HLS URLs.
    fn select_best_transcoding(track: &Value) -> Option<(i32, String, CodecFlags, bool)> {
        let transcodings = track
            .get("media")
            .and_then(|m| m.get("transcodings"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        // (quality, preference, url, codec, is_hls, preset)
        let mut candidates: Vec<(i32, i32, String, CodecFlags, bool, String)> = Vec::new();
        for t in &transcodings {
            let proto = t
                .get("format")
                .and_then(|f| f.get("protocol"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let url = t
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if url.is_empty() {
                continue;
            }
            // Codec comes from the preset prefix (`mp3_*`, `aac_*`, `opus_*`),
            // not the container mime type (HLS AAC reports `audio/mpeg`).
            let preset = t
                .get("preset")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let codec_name = preset.split('_').next().unwrap_or("").to_uppercase();
            let codec = match codec_name.as_str() {
                "MP3" => CodecFlags::MP3,
                "AAC" => CodecFlags::AAC,
                "OPUS" => CodecFlags::OPUS,
                _ => continue,
            };
            let is_hls = Self::is_hls_transcoding(proto, &url);
            // DRM-encrypted HLS is unplayable without a decryption key;
            // Python penalises it so it is never selected.
            if is_hls && Self::is_encrypted_hls(&url) {
                continue;
            }
            let quality = if is_hls && codec == CodecFlags::AAC {
                Self::parse_aac_bitrate(&preset)
            } else {
                Self::parse_progressive_bitrate(&preset, &codec_name)
            };
            if quality <= 0 {
                continue;
            }
            let pref = Self::stream_preference(codec, is_hls);
            candidates.push((quality, pref, url, codec, is_hls, preset));
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        candidates
            .into_iter()
            .next()
            .map(|(q, _, url, codec, is_hls, _)| (q, url, codec, is_hls))
    }

    async fn pick_progressive_url(&self, track: &Value) -> Result<(String, CodecFlags)> {
        let auth = track
            .get("track_authorization")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match Self::select_best_transcoding(track) {
            Some((_, url, codec, _)) => {
                let resolved = self.session.lock().await.resolve_stream(&url, &auth).await?;
                Ok((resolved, codec))
            }
            None => Err(Error::Other(
                "SoundCloud: no playable transcodings for this track (only DRM-encrypted HLS or unparseable presets)".to_string(),
            )),
        }
    }
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for SoundCloudModule {
    fn name(&self) -> &str {
        "SoundCloud"
    }

    fn is_authenticated(&self) -> bool {
        self.session
            .try_lock()
            .map(|s| !s.access_token.is_empty())
            .unwrap_or(false)
    }

    fn custom_url_parse(&self, url: &str) -> Result<Option<MediaIdentification>> {
        // The SoundCloud `resolve` endpoint maps any public soundcloud.com URL
        // (track / playlist / user, incl. shortened on.soundcloud.com links) to
        // its `kind` + numeric `id`, which is what `interface.py` does. That
        // needs an async HTTP call, but this method is synchronous, so we block
        // the current thread on the Tokio runtime handle when one is available.
        // When there is no handle (e.g. called before the runtime starts), the
        // session lock is busy, or blocking fails (e.g. a single-threaded
        // runtime where `block_in_place` is unavailable), we return `None` and
        // the downloader falls back to generic `url_decode`; resolution then
        // happens lazily inside `get_*_info` via `extra_kwargs`/direct fetches.
        let session = match self.session.try_lock() {
            Ok(s) => s.clone(),
            Err(_) => return Ok(None),
        };
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };
        let url_owned = url.to_string();
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tokio::task::block_in_place(|| handle.block_on(session.resolve_url(&url_owned)))
        }));
        let resolved = match attempt {
            Ok(Ok(v)) => v,
            _ => return Ok(None),
        };
        let kind = resolved.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let id = resolved
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|i| i.to_string())
            .unwrap_or_default();
        if id.is_empty() {
            return Ok(None);
        }
        let media_type = match kind {
            "user" => DownloadType::artist,
            "track" => DownloadType::track,
            // `interface.py`: playlists with `is_album` are albums.
            "playlist"
                if resolved
                    .get("is_album")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false) =>
            {
                DownloadType::album
            }
            "playlist" => DownloadType::playlist,
            _ => return Ok(None),
        };
        let mut data = serde_json::Map::new();
        data.insert(id.clone(), resolved);
        let mut extra = serde_json::Map::new();
        extra.insert("data".to_string(), Value::Object(data));
        Ok(Some(MediaIdentification {
            media_type,
            media_id: id,
            extra_kwargs: extra,
        }))
    }

    async fn get_track_info(
        &self,
        track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        self.ensure_plan().await;
        let track = if let Some(v) = data.get(track_id).cloned() {
            v
        } else {
            self.session.lock().await.get_track(track_id).await?
        };
        // Python strips an "Artist - " prefix from the title.
        let raw_title = track
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown");
        let title = raw_title
            .split_once(" - ")
            .map(|(_, t)| t.to_string())
            .unwrap_or_else(|| raw_title.to_string());
        let user = track
            .get("user")
            .and_then(|u| u.get("username"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let artists = Self::split_artists(&user);
        let artwork = track
            .get("artwork_url")
            .and_then(|v| v.as_str())
            .map(Self::original_artwork)
            .unwrap_or_default();
        let duration = track
            .get("duration")
            .and_then(|v| v.as_u64())
            .map(|ms| (ms / 1000) as u32);
        let genre = track
            .get("genre")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // Codec/bitrate mirror the scored transcoding selection (Python sets
        // `final_codec` from the best stream in get_track_info).
        let (codec, bitrate) = match Self::select_best_transcoding(&track) {
            Some((q, _, c, _)) => (c, Some(q as u32)),
            None => (CodecFlags::MP3, Some(128)),
        };
        Ok(TrackInfo {
            id: Some(track_id.to_string()),
            name: title,
            album: String::new(),
            album_id: String::new(),
            artists,
            tags: Tags {
                genres: genre.map(|g| vec![g]),
                release_date: track
                    .get("release_date")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        track
                            .get("display_date")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    }),
                description: track
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                track_url: Some(format!("https://soundcloud.com/track/{track_id}")),
                ..Default::default()
            },
            codec,
            cover_url: artwork,
            release_year: Self::release_year(&track),
            duration,
            explicit: None,
            artist_id: track
                .get("user")
                .and_then(|u| u.get("id"))
                .and_then(|v| v.as_i64())
                .map(|i| i.to_string()),
            bitrate,
            ..Default::default()
        })
    }

    async fn get_track_download(
        &self,
        track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        let track = if let Some(v) = data.get(track_id).cloned() {
            v
        } else {
            self.session.lock().await.get_track(track_id).await?
        };
        // direct download endpoint first (original upload); Python requires
        // both `downloadable` and `has_downloads_left`.
        let client = self.session.lock().await.client.clone();
        if track
            .get("downloadable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            && track
                .get("has_downloads_left")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        {
            let v = self
                .session
                .lock()
                .await
                .api_get(&format!("tracks/{track_id}/download"), HashMap::new())
                .await;
            if let Ok(v) = v {
                if let Some(uri) = v.get("redirectUri").and_then(|u| u.as_str()) {
                    return Ok(TrackDownloadInfo {
                        download_type: DownloadSource::Url,
                        file_url: Some(uri.to_string()),
                        file_url_headers: Default::default(),
                        temp_file_path: None,
                        different_codec: Some(CodecFlags::MP3),
                    });
                }
            }
        }
        let (stream_url, codec) = self.pick_progressive_url(&track).await?;
        // Download progressive stream to temp file (HLS URLs would need ffmpeg;
        // progressive mp3/aac can be saved directly).
        let resp = client
            .get(&stream_url)
            .send()
            .await
            .map_err(|e| Error::Other(format!("SoundCloud stream fetch: {e}")))?;
        if !resp.status().is_success() {
            // HLS playlist – return URL and let the downloader/ffmpeg handle it
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::Url,
                file_url: Some(stream_url),
                file_url_headers: Default::default(),
                temp_file_path: None,
                different_codec: Some(codec),
            });
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Other(format!("SoundCloud read: {e}")))?;
        // If it looks like an m3u8 playlist, return as URL instead
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(64)]).to_string();
        if head.contains("#EXTM3U") {
            return Ok(TrackDownloadInfo {
                download_type: DownloadSource::Url,
                file_url: Some(stream_url),
                file_url_headers: Default::default(),
                temp_file_path: None,
                different_codec: Some(codec),
            });
        }
        let ext = if codec.contains(CodecFlags::MP3) {
            "mp3"
        } else if codec.contains(CodecFlags::OPUS) {
            "opus"
        } else {
            "m4a"
        };
        let path =
            std::env::temp_dir().join(format!("sc-{track_id}-{}.{}", std::process::id(), ext));
        let mut f =
            std::fs::File::create(&path).map_err(|e| Error::Other(format!("temp create: {e}")))?;
        f.write_all(&bytes)
            .map_err(|e| Error::Other(format!("temp write: {e}")))?;
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::TempFilePath,
            file_url: None,
            file_url_headers: Default::default(),
            temp_file_path: Some(path),
            different_codec: Some(codec),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        // SoundCloud albums are playlists with is_album=true
        self.ensure_plan().await;
        self.get_playlist_as_album(album_id, data).await
    }

    async fn get_playlist_info(
        &self,
        playlist_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        self.ensure_plan().await;
        let sess = self.session.lock().await.clone();
        let pl = if let Some(v) = data.get(playlist_id).cloned() {
            v
        } else {
            sess.api_get(&format!("playlists/{playlist_id}"), HashMap::new())
                .await?
        };
        let stubs: Vec<Value> = pl
            .get("tracks")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        let refilled = sess.get_tracks_from_tracklist(&stubs).await;
        let mut tracks: Vec<TrackRef> = Vec::new();
        for t in &stubs {
            if let Some(id) = t.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string()) {
                // Keep the original order of the playlist tracklist.
                let _ = refilled.get(&id);
                tracks.push(TrackRef::Id(id));
            }
        }
        let mut track_data = serde_json::Map::new();
        for (k, v) in &refilled {
            track_data.insert(k.clone(), v.clone());
        }
        let mut track_extra = serde_json::Map::new();
        track_extra.insert("data".to_string(), Value::Object(track_data));
        let artwork = pl
            .get("artwork_url")
            .and_then(|v| v.as_str())
            .map(Self::original_artwork)
            .unwrap_or_default();
        Ok(PlaylistInfo {
            id: Some(playlist_id.to_string()),
            name: pl
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            creator: pl
                .get("user")
                .and_then(|u| u.get("username"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            creator_id: pl
                .get("user")
                .and_then(|u| u.get("permalink"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    pl.get("user")
                        .and_then(|u| u.get("id"))
                        .and_then(|v| v.as_i64())
                        .map(|i| i.to_string())
                }),
            tracks,
            release_year: Self::release_year(&pl),
            cover_url: Some(artwork),
            cover_type: Some(ImageFileType::Jpg),
            description: pl
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            track_extra_kwargs: track_extra,
            ..Default::default()
        })
    }

    async fn get_artist_info(
        &self,
        artist_id: &str,
        _credited: bool,
        _name: Option<&str>,
        data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        self.ensure_plan().await;
        // Name + permalink from data, falling back to users/{id}.
        let mut name = String::new();
        let mut permalink: Option<String> = None;
        if let Some(ud) = data.get(artist_id) {
            name = ud
                .get("username")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            permalink = ud
                .get("permalink")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
        let sess = self.session.lock().await.clone();
        if name.is_empty() || permalink.is_none() {
            if let Ok(user_data) = sess
                .api_get(&format!("users/{artist_id}"), HashMap::new())
                .await
            {
                if name.is_empty() {
                    name = user_data
                        .get("username")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Artist")
                        .to_string();
                }
                if permalink.is_none() {
                    permalink = user_data
                        .get("permalink")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
            } else if name.is_empty() {
                name = "Artist".to_string();
            }
        }
        // Prefer the numeric artist_id; only retry via permalink when the id
        // itself is non-numeric and the first attempt came back empty (mirrors
        // interface.py, avoids a duplicate 403 when uid is unchanged).
        let (mut album_data, mut track_data) = sess.get_user_albums_tracks(artist_id).await;
        let is_numeric = artist_id.chars().all(|c| c.is_ascii_digit());
        if album_data.is_empty()
            && track_data.is_empty()
            && !is_numeric
            && permalink
                .as_deref()
                .map(|p| p != artist_id)
                .unwrap_or(false)
        {
            let pl = permalink.clone().unwrap();
            let (a, t) = sess.get_user_albums_tracks(&pl).await;
            album_data = a;
            track_data = t;
        }
        // Compact album summaries for artist expansion.
        let mut album_ids: Vec<String> = album_data.keys().cloned().collect();
        album_ids.sort();
        let mut albums_out: Vec<Value> = album_ids
            .iter()
            .filter_map(|aid| {
                album_data
                    .get(aid)
                    .map(|a| Self::summarize_album(aid, a, &name))
            })
            .collect();
        // Backfill missing durations/years/track-counts/genres, like the
        // ThreadPoolExecutor batch in interface.py (concurrent via join_all).
        let missing: Vec<String> = albums_out
            .iter()
            .filter(|s| {
                s.get("additional")
                    .and_then(|v| v.as_array())
                    .map(|a| a.is_empty())
                    .unwrap_or(true)
                    || s.get("release_year").map(|v| v.is_null()).unwrap_or(true)
            })
            .filter_map(|s| s.get("id").and_then(|v| v.as_str()).map(|i| i.to_string()))
            .collect();
        if !missing.is_empty() {
            let fetched: Vec<(String, Option<Value>)> =
                futures::future::join_all(missing.into_iter().map(|aid| {
                    let sess = sess.clone();
                    async move {
                        let v = sess
                            .api_get(&format!("playlists/{aid}"), HashMap::new())
                            .await
                            .ok();
                        (aid, v)
                    }
                }))
                .await;
            let mut meta: HashMap<String, Value> = HashMap::new();
            for (aid, v) in fetched {
                if let Some(v) = v {
                    meta.insert(aid, v);
                }
            }
            for s in albums_out.iter_mut() {
                let aid = s
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(full) = meta.get(&aid) {
                    let needs_additional = s
                        .get("additional")
                        .and_then(|v| v.as_array())
                        .map(|a| a.is_empty())
                        .unwrap_or(true);
                    let needs_year = s.get("release_year").map(|v| v.is_null()).unwrap_or(true);
                    if needs_additional || needs_year {
                        let fresh = Self::summarize_album(&aid, full, &name);
                        // Only fill in what was missing.
                        if needs_additional
                            && fresh
                                .get("additional")
                                .and_then(|v| v.as_array())
                                .map(|a| !a.is_empty())
                                .unwrap_or(false)
                        {
                            s["additional"] = fresh["additional"].clone();
                        }
                        if needs_year
                            && !fresh
                                .get("release_year")
                                .map(|v| v.is_null())
                                .unwrap_or(true)
                        {
                            s["release_year"] = fresh["release_year"].clone();
                        }
                        if s.get("duration").map(|v| v.is_null()).unwrap_or(true) {
                            s["duration"] = fresh["duration"].clone();
                        }
                    }
                }
            }
        }
        let mut album_map = serde_json::Map::new();
        for (k, v) in &album_data {
            album_map.insert(k.clone(), v.clone());
        }
        let mut album_extra = serde_json::Map::new();
        album_extra.insert("data".to_string(), Value::Object(album_map));
        let mut track_ids: Vec<String> = track_data.keys().cloned().collect();
        track_ids.sort();
        let mut track_map = serde_json::Map::new();
        for (k, v) in &track_data {
            track_map.insert(k.clone(), v.clone());
        }
        let mut track_extra = serde_json::Map::new();
        track_extra.insert("data".to_string(), Value::Object(track_map));
        Ok(ArtistInfo {
            name,
            artist_id: Some(artist_id.to_string()),
            albums: albums_out,
            album_extra_kwargs: album_extra,
            tracks: track_ids.into_iter().map(Value::String).collect(),
            track_extra_kwargs: track_extra,
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
        let track = if let Some(v) = data.get(track_id).cloned() {
            v
        } else {
            self.session.lock().await.get_track(track_id).await?
        };
        let url = track
            .get("artwork_url")
            .and_then(|v| v.as_str())
            .map(Self::original_artwork)
            .unwrap_or_default();
        Ok(CoverInfo {
            url,
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
        let qt = match query_type {
            DownloadType::artist => "users",
            DownloadType::playlist => "playlists_without_albums",
            DownloadType::album => "albums",
            _ => "tracks",
        };
        let v = self.session.lock().await.search(qt, query, limit).await?;
        let collection = v
            .get("collection")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for r in collection {
            let image_url = match qt {
                "users" => r
                    .get("avatar_url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                "tracks" => r
                    .get("artwork_url")
                    .and_then(|v| v.as_str())
                    .or_else(|| {
                        r.get("user")
                            .and_then(|u| u.get("avatar_url"))
                            .and_then(|v| v.as_str())
                    })
                    .map(|s| s.to_string()),
                _ => r
                    .get("artwork_url")
                    .and_then(|v| v.as_str())
                    .or_else(|| r.get("calculated_artwork_url").and_then(|v| v.as_str()))
                    .map(|s| s.to_string()),
            }
            .filter(|u| !u.contains("default_avatar"))
            .map(|u| u.replace("-large", "-t200x200"));
            let duration = r
                .get("duration")
                .and_then(|v| v.as_u64())
                .map(|ms| (ms / 1000) as u32);
            let year = r
                .get("release_date")
                .and_then(|v| v.as_str())
                .or_else(|| r.get("display_date").and_then(|v| v.as_str()))
                .or_else(|| r.get("created_at").and_then(|v| v.as_str()))
                .and_then(|s| s.split('-').next().map(|y| y.to_string()));
            out.push(SearchResult {
                result_id: r
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .map(|i| i.to_string())
                    .unwrap_or_default(),
                name: Some(
                    r.get("title")
                        .and_then(|v| v.as_str())
                        .or_else(|| r.get("username").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .to_string(),
                ),
                artists: if qt != "users" {
                    r.get("user")
                        .and_then(|u| u.get("username"))
                        .and_then(|v| v.as_str())
                        .map(|u| Self::split_artists(u))
                } else {
                    None
                },
                year,
                duration,
                image_url,
                additional: r
                    .get("genre")
                    .and_then(|v| v.as_str())
                    .map(|g| vec![g.to_string()]),
                ..Default::default()
            });
        }
        Ok(out)
    }

    async fn get_preview_stream_url(&self, track_id: &str) -> Result<Option<String>> {
        let track = self.session.lock().await.get_track(track_id).await?;
        Ok(self.pick_progressive_url(&track).await.ok().map(|(u, _)| u))
    }
}

impl SoundCloudModule {
    async fn get_playlist_as_album(
        &self,
        album_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        if album_id.is_empty() {
            return Err(Error::InvalidInput(
                "SoundCloud get_album_info called with an empty album id".to_string(),
            ));
        }
        // Reuse caller-provided data when it is already the album record or
        // holds it; refetch when missing/incomplete (no tracks, or stub tracks
        // without `streamable`), mirroring interface.py.
        let mut playlist_data: Option<Value> = None;
        if data
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|i| i.to_string())
            .as_deref()
            == Some(album_id)
            || data.get("id").and_then(|v| v.as_str()) == Some(album_id)
        {
            playlist_data = Some(Value::Object(data.clone().into_iter().collect()));
        } else if let Some(v) = data.get(album_id).cloned() {
            playlist_data = Some(v);
        }
        let needs_fetch = match &playlist_data {
            None => true,
            Some(p) => match p.get("tracks").and_then(|t| t.as_array()) {
                None => true,
                Some(t) => {
                    t.is_empty()
                        || t.first()
                            .map(|f| f.get("streamable").is_none())
                            .unwrap_or(false)
                }
            },
        };
        let sess = self.session.lock().await.clone();
        if needs_fetch {
            playlist_data = Some(
                sess.api_get(&format!("playlists/{album_id}"), HashMap::new())
                    .await?,
            );
        }
        let pl = playlist_data.unwrap_or(json!({}));
        let stubs: Vec<Value> = pl
            .get("tracks")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        let refilled = sess.get_tracks_from_tracklist(&stubs).await;
        let mut tracks: Vec<TrackRef> = Vec::new();
        for t in &stubs {
            if let Some(id) = t.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string()) {
                let _ = refilled.get(&id);
                tracks.push(TrackRef::Id(id));
            }
        }
        let mut track_data = serde_json::Map::new();
        for (k, v) in &refilled {
            track_data.insert(k.clone(), v.clone());
        }
        let mut track_extra = serde_json::Map::new();
        track_extra.insert("data".to_string(), Value::Object(track_data));
        let cover = pl
            .get("artwork_url")
            .and_then(|v| v.as_str())
            .or_else(|| {
                pl.get("user")
                    .and_then(|u| u.get("avatar_url"))
                    .and_then(|v| v.as_str())
            })
            .map(Self::original_artwork);
        Ok(AlbumInfo {
            id: Some(album_id.to_string()),
            name: pl
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Album")
                .to_string(),
            artist: pl
                .get("user")
                .and_then(|u| u.get("username"))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Artist")
                .to_string(),
            artist_id: pl
                .get("user")
                .and_then(|u| u.get("permalink"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            tracks,
            release_year: Self::release_year(&pl),
            expected_track_count: pl
                .get("track_count")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32),
            cover_url: cover,
            cover_type: Some(ImageFileType::Jpg),
            description: pl
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            track_extra_kwargs: track_extra,
            ..Default::default()
        })
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
