//! Tidal module - port of `modules/tidal/{interface,tidal_api}.py`.
//!
//! Auth: TV/Mobile/Guest sessions. We support the TV session out of the box
//! (the user's settings.json has those tokens). Guest sessions are used as
//! a fallback for metadata when unauthenticated.
//!
//! Session model (parity with `tidal_api.py::TidalApi`):
//! all known sessions are kept in one map with a single *active* (`default`)
//! session, switched per-request depending on the audio format, exactly like
//! the Python `self.session.default = ...` assignments.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const TIDAL_API_BASE: &str = "https://api.tidal.com/v1";
const TIDAL_AUTH_BASE: &str = "https://auth.tidal.com/v1/";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "Tidal".to_string(),
        module_supported_modes: ModuleModes::download
            | ModuleModes::lyrics
            | ModuleModes::covers
            | ModuleModes::credits,
        global_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("tv_atmos_token".to_string(), json!("4N3n6Q1x95LL5K7p"));
            m.insert(
                "tv_atmos_secret".to_string(),
                json!("oKOXfJW371cX6xaZ0PyhgGNBdNLlBZd4AKKYougMjik="),
            );
            m.insert(
                "mobile_atmos_hires_token".to_string(),
                json!("km8T1xS355y7dd3H"),
            );
            m.insert("mobile_hires_token".to_string(), json!("6BDSRdpK9hqEBTgU"));
            m.insert("guest_token".to_string(), json!("txNoH4kkV41MfH25"));
            m.insert(
                "guest_secret".to_string(),
                json!("dQjy0MinCEvxi1O4UmxvxWnDjt4cgHBPw8ll6nYBk98="),
            );
            m.insert("enable_mobile".to_string(), json!(true));
            m.insert("prefer_ac4".to_string(), json!(false));
            m.insert("fix_mqa".to_string(), json!(true));
            m.insert("throttle".to_string(), json!(true));
            m
        },
        global_storage_variables: vec![],
        session_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("access_token".to_string(), json!(""));
            m.insert("refresh_token".to_string(), json!(""));
            m.insert("user_id".to_string(), json!(""));
            m.insert("country_code".to_string(), json!("US"));
            m
        },
        session_storage_variables: vec![
            "access_token".to_string(),
            "refresh_token".to_string(),
            "user_id".to_string(),
            "country_code".to_string(),
        ],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Multi(vec![
            "tidal".to_string(),
            "tidalhifi".to_string(),
        ]),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("track".to_string(), DownloadType::track);
            m.insert("album".to_string(), DownloadType::album);
            m.insert("playlist".to_string(), DownloadType::playlist);
            m.insert("mix".to_string(), DownloadType::playlist);
            m.insert("artist".to_string(), DownloadType::artist);
            m
        },
        test_url: Some("https://tidal.com/browse/track/12345".to_string()),
        url_decoding: ManualEnum::Picaro,
        login_behaviour: ManualEnum::Picaro,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(TidalConstructor)
}

#[derive(Debug)]
struct TidalConstructor;

fn setting_str(settings: &serde_json::Map<String, Value>, key: &str, default: &str) -> String {
    settings
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

fn setting_bool(settings: &serde_json::Map<String, Value>, key: &str, default: bool) -> bool {
    match settings.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes"),
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0).unwrap_or(default),
        _ => default,
    }
}

impl ModuleConstructor for TidalConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        let settings = controller.module_settings.clone();
        let access = setting_str(&settings, "access_token", "");
        let refresh = setting_str(&settings, "refresh_token", "");
        let user_id = setting_str(&settings, "user_id", "");
        let country = setting_str(&settings, "country_code", "US");
        Ok(Arc::new(TidalModule::new(
            controller, access, refresh, user_id, country,
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SessionType {
    Tv,
    MobileDefault,
    MobileAtmos,
    Guest,
}

impl SessionType {
    /// Name matching the Python `SessionType` enum member names.
    #[allow(dead_code)]
    fn name(self) -> &'static str {
        match self {
            SessionType::Tv => "TV",
            SessionType::MobileDefault => "MOBILE_DEFAULT",
            SessionType::MobileAtmos => "MOBILE_ATMOS",
            SessionType::Guest => "GUEST",
        }
    }
}

// ---------------------------------------------------------------------------
// Low-level session: raw Tidal API calls
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TidalSession {
    access_token: Option<String>,
    refresh_token: Option<String>,
    user_id: Option<String>,
    country_code: String,
    session_type: SessionType,
    /// OAuth client id (`X-Tidal-Token`), per session flavour.
    client_id: String,
    /// OAuth client secret (TV + Guest only).
    client_secret: Option<String>,
    client: reqwest::Client,
}

impl TidalSession {
    fn new(
        session_type: SessionType,
        client_id: String,
        client_secret: Option<String>,
        country: String,
    ) -> Self {
        Self {
            access_token: None,
            refresh_token: None,
            user_id: None,
            country_code: country,
            session_type,
            client_id,
            client_secret,
            client: picaro_utils::http::build_client(None),
        }
    }

    fn is_authenticated(&self) -> bool {
        // Guest sessions never carry a user_id (mirrors `authenticated_session()`).
        if self.session_type == SessionType::Guest {
            return false;
        }
        self.access_token
            .as_ref()
            .map(|t| !t.is_empty())
            .unwrap_or(false)
    }

    fn auth_headers(&self) -> Vec<(String, String)> {
        let mut h = Vec::new();
        if !self.client_id.is_empty() {
            h.push(("X-Tidal-Token".to_string(), self.client_id.clone()));
        }
        if let Some(t) = &self.access_token {
            if !t.is_empty() {
                h.push(("Authorization".to_string(), format!("Bearer {t}")));
            }
        }
        let ua = match self.session_type {
            SessionType::Guest => "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/119.0.0.0 Safari/537.36",
            _ => "TIDAL_ANDROID/1039 okhttp/3.14.9",
        };
        h.push(("User-Agent".to_string(), ua.to_string()));
        h
    }

    /// Re-authenticate a Guest session via the `client_credentials` grant
    /// (mirrors `TidalGuestSession.auth()`).
    async fn guest_auth(&mut self) -> bool {
        let url = format!("{TIDAL_AUTH_BASE}oauth2/token");
        let secret = self.client_secret.clone().unwrap_or_default();
        let res = self
            .client
            .post(&url)
            .form(&[
                ("client_id", self.client_id.clone()),
                ("client_secret", secret),
                ("grant_type", "client_credentials".to_string()),
            ])
            .header("Origin", "https://tidal.com")
            .header("Referer", "https://tidal.com/")
            .send()
            .await;
        let resp = match res {
            Ok(r) => r,
            Err(_) => return false,
        };
        if !resp.status().is_success() {
            return false;
        }
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return false,
        };
        if let Some(t) = v.get("access_token").and_then(|t| t.as_str()) {
            self.access_token = Some(t.to_string());
            if self.country_code.is_empty() {
                self.country_code = "US".to_string();
            }
            true
        } else {
            false
        }
    }

    /// Refresh OAuth tokens (mirrors `TidalTvSession.refresh()` /
    /// `TidalMobileSession.refresh()`). Returns true on success.
    async fn refresh_tokens(&mut self) -> bool {
        if self.session_type == SessionType::Guest {
            return self.guest_auth().await;
        }
        let refresh = match &self.refresh_token {
            Some(r) if !r.is_empty() => r.clone(),
            _ => return false,
        };
        let url = format!("{TIDAL_AUTH_BASE}oauth2/token");
        let mut form = vec![
            ("refresh_token".to_string(), refresh),
            ("client_id".to_string(), self.client_id.clone()),
            ("grant_type".to_string(), "refresh_token".to_string()),
        ];
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret".to_string(), secret.clone()));
        }
        let res = self.client.post(&url).form(&form).send().await;
        let resp = match res {
            Ok(r) => r,
            Err(_) => return false,
        };
        if !resp.status().is_success() {
            return false;
        }
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return false,
        };
        if let Some(t) = v.get("access_token").and_then(|t| t.as_str()) {
            self.access_token = Some(t.to_string());
            if let Some(r) = v.get("refresh_token").and_then(|r| r.as_str()) {
                self.refresh_token = Some(r.to_string());
            }
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-session API state: one map of sessions + a single active (`default`)
// session that is switched per-request, mirroring `TidalApi` in Python.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct TidalApiState {
    sessions: HashMap<SessionType, TidalSession>,
    /// Currently active session (switched per-format like the Python code).
    default: SessionType,
    /// Sessions the user enabled (TV always; mobile only if `enable_mobile`).
    available: Vec<SessionType>,
    prefer_ac4: bool,
}

impl TidalApiState {
    fn resolve(&self, session_override: Option<SessionType>) -> SessionType {
        let want = session_override.unwrap_or(self.default);
        if self.sessions.contains_key(&want) {
            return want;
        }
        if self.sessions.contains_key(&SessionType::Guest) {
            return SessionType::Guest;
        }
        self.default
    }

    fn set_default(&mut self, st: SessionType) {
        if self.available.contains(&st) && self.sessions.contains_key(&st) {
            self.default = st;
        }
    }

    #[allow(dead_code)]
    fn is_guest_only(&self) -> bool {
        self.sessions.len() == 1 && self.sessions.contains_key(&SessionType::Guest)
    }

    fn is_authenticated(&self) -> bool {
        self.sessions.values().any(|s| s.is_authenticated())
    }

    /// Raw GET with token refresh on 401/403 (retry once),
    /// mirroring `TidalApi._get(..., refresh=...)`.
    async fn api_get(
        &mut self,
        path: &str,
        params: HashMap<String, String>,
        session_override: Option<SessionType>,
    ) -> Result<Value> {
        self.api_get_inner(path, params, session_override, false)
            .await
    }

    async fn api_get_inner(
        &mut self,
        path: &str,
        mut params: HashMap<String, String>,
        session_override: Option<SessionType>,
        _refreshed: bool,
    ) -> Result<Value> {
        // Loop (instead of recursion) for the single 401/403 refresh retry.
        let mut refreshed = false;
        loop {
            let st = self.resolve(session_override);
            // Lazily (re-)authenticate guest sessions like `_ensure_guest_session`.
            if st == SessionType::Guest {
                let needs_auth = self
                    .sessions
                    .get(&st)
                    .map(|s| {
                        s.access_token
                            .as_ref()
                            .map(|t| t.is_empty())
                            .unwrap_or(true)
                    })
                    .unwrap_or(true);
                if needs_auth {
                    let _ = self
                        .sessions
                        .get_mut(&st)
                        .map(|s| s.guest_auth())
                        .unwrap()
                        .await;
                }
            }
            let (client, headers, country) = {
                let s = self
                    .sessions
                    .get(&st)
                    .ok_or_else(|| Error::Other("Tidal: no sessions available".to_string()))?;
                (s.client.clone(), s.auth_headers(), s.country_code.clone())
            };
            params.entry("countryCode".to_string()).or_insert(country);
            let url = format!("{TIDAL_API_BASE}/{path}");
            let mut req = client
                .get(&url)
                .query(&params)
                .timeout(std::time::Duration::from_secs(60));
            for (k, v) in headers {
                req = req.header(k, v);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| Error::Other(format!("Tidal request failed: {e}")))?;
            let status = resp.status();

            // Retry once after refresh on 401/403, like the Python `_get`.
            if !refreshed && (status.as_u16() == 401 || status.as_u16() == 403) {
                let ok = if let Some(s) = self.sessions.get_mut(&st) {
                    s.refresh_tokens().await
                } else {
                    false
                };
                if ok {
                    refreshed = true;
                    continue;
                }
            }

            let text = resp
                .text()
                .await
                .map_err(|e| Error::Other(format!("Tidal read body: {e}")))?;
            // Some tracks return JSON with leading whitespace (Python strips it).
            let v: Value = serde_json::from_str(text.trim())
                .map_err(|e| Error::Other(format!("Tidal invalid JSON (HTTP {status}): {e}")))?;

            if let Some(st_code) = v.get("status").and_then(|s| s.as_u64()) {
                // Region-locked (mirrors the subStatus==2001 branch).
                if st_code == 404 && v.get("subStatus").and_then(|s| s.as_u64()) == Some(2001) {
                    let msg = v
                        .get("userMessage")
                        .and_then(|m| m.as_str())
                        .unwrap_or("Not available");
                    return Err(Error::Other(format!(
                        "Error: {msg}. This might be region-locked."
                    )));
                }
                // Plain 404 payloads (e.g. missing credits) are returned, not raised.
                if st_code == 404 && v.get("error").and_then(|e| e.as_str()) == Some("Not Found") {
                    return Ok(v);
                }
                if st_code != 200 {
                    let msg = v
                        .get("userMessage")
                        .and_then(|m| m.as_str())
                        .unwrap_or("Tidal request failed");
                    return Err(Error::Other(format!(
                        "Tidal error {st_code}: {msg} (HTTP {status})"
                    )));
                }
            }
            return Ok(v);
        }
    }

    async fn get_track(&mut self, track_id: &str) -> Result<Value> {
        self.api_get(&format!("tracks/{track_id}"), HashMap::new(), None)
            .await
    }

    async fn get_album(&mut self, album_id: &str) -> Result<Value> {
        self.api_get(&format!("albums/{album_id}"), HashMap::new(), None)
            .await
    }

    async fn get_album_items_page(
        &mut self,
        album_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("offset".to_string(), offset.to_string());
        p.insert("limit".to_string(), limit.to_string());
        self.api_get(&format!("albums/{album_id}/items"), p, None)
            .await
    }

    /// Paginated `albums/{id}/items` (verified: loops until `totalNumberOfItems`).
    async fn get_album_items_all(&mut self, album_id: &str) -> Result<Value> {
        let mut all_items = Vec::new();
        let mut offset = 0usize;
        loop {
            let v = self.get_album_items_page(album_id, offset, 100).await?;
            let total = v
                .get("totalNumberOfItems")
                .and_then(|t| t.as_u64())
                .unwrap_or(0) as usize;
            let items = v
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            let n = items.len();
            all_items.extend(items);
            offset += n;
            if n == 0 || offset >= total || total == 0 {
                break;
            }
        }
        Ok(json!({"items": all_items}))
    }

    /// `albums/{id}/items/credits` with pagination — the endpoint backing
    /// `get_album_info` track lists + label extraction in Python.
    async fn get_album_contributors(&mut self, album_id: &str) -> Result<Value> {
        let mut all_items = Vec::new();
        let mut total: Option<u64> = None;
        let mut offset = 0usize;
        loop {
            let mut p = HashMap::new();
            p.insert("replace".to_string(), "true".to_string());
            p.insert("offset".to_string(), offset.to_string());
            p.insert("limit".to_string(), "100".to_string());
            p.insert("includeContributors".to_string(), "true".to_string());
            let v = self
                .api_get(&format!("albums/{album_id}/items/credits"), p, None)
                .await?;
            if total.is_none() {
                total = v.get("totalNumberOfItems").and_then(|t| t.as_u64());
            }
            let items = v
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            let n = items.len();
            all_items.extend(items);
            offset += n;
            let total_usize = total.unwrap_or(0) as usize;
            if n == 0 || (total_usize > 0 && offset >= total_usize) {
                break;
            }
            if total.is_none() {
                break;
            }
        }
        Ok(json!({"items": all_items, "totalNumberOfItems": all_items.len()}))
    }

    /// Label lookup for an album: first track's credits, falling back to the
    /// album copyright string (mirrors `get_album_info` label extraction).
    async fn get_label_info(&mut self, album_id: &str) -> Option<String> {
        if let Ok(contrib) = self.get_album_contributors(album_id).await {
            if let Some(first) = contrib
                .get("items")
                .and_then(|i| i.as_array())
                .and_then(|a| a.first())
            {
                let credits = first.get("credits").and_then(|c| c.as_array());
                if let Some(label) = extract_label_from_credits(credits) {
                    return Some(label);
                }
            }
        }
        // Fallback: album copyright.
        if let Ok(album) = self.get_album(album_id).await {
            let copyright = album.get("copyright").and_then(|c| c.as_str());
            if let Some(label) = extract_label_from_copyright(copyright) {
                return Some(label);
            }
        }
        None
    }

    async fn get_playlist(&mut self, playlist_id: &str) -> Result<Value> {
        self.api_get(&format!("playlists/{playlist_id}"), HashMap::new(), None)
            .await
    }

    /// Paginated `playlists/{id}/items` (verified: loops until
    /// `totalNumberOfItems`; entries wrap tracks in `item`).
    async fn get_playlist_tracks(&mut self, playlist_id: &str) -> Result<Value> {
        let mut all_items = Vec::new();
        let mut offset = 0usize;
        loop {
            let mut p = HashMap::new();
            p.insert("offset".to_string(), offset.to_string());
            p.insert("limit".to_string(), "100".to_string());
            let v = self
                .api_get(&format!("playlists/{playlist_id}/items"), p, None)
                .await?;
            let total = v
                .get("totalNumberOfItems")
                .and_then(|t| t.as_u64())
                .unwrap_or(0) as usize;
            // playlist items wrap tracks in `item`
            let items = v
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            let mut tracks = Vec::new();
            for it in &items {
                if let Some(track) = it.get("item") {
                    tracks.push(track.clone());
                } else {
                    tracks.push(it.clone());
                }
            }
            let n = items.len();
            all_items.extend(tracks);
            offset += n;
            if n == 0 || offset >= total || total == 0 {
                break;
            }
        }
        Ok(json!({"items": all_items}))
    }

    async fn get_artist(&mut self, artist_id: &str) -> Result<Value> {
        self.api_get(&format!("artists/{artist_id}"), HashMap::new(), None)
            .await
    }

    async fn get_all_artist_albums(
        &mut self,
        artist_id: &str,
        filter: Option<&str>,
    ) -> Result<Value> {
        // Python paginates with limit=50 because Tidal caps the page size.
        let mut items = Vec::new();
        let mut first_total: Option<u64> = None;
        let mut offset = 0usize;
        loop {
            let mut p = HashMap::new();
            p.insert("limit".to_string(), "50".to_string());
            p.insert("offset".to_string(), offset.to_string());
            if let Some(f) = filter {
                p.insert("filter".to_string(), f.to_string());
            }
            let v = self
                .api_get(&format!("artists/{artist_id}/albums"), p, None)
                .await?;
            if first_total.is_none() {
                first_total = v.get("totalNumberOfItems").and_then(|t| t.as_u64());
            }
            let page = v
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            if page.is_empty() {
                break;
            }
            items.extend(page);
            offset += 50;
            let total = first_total.unwrap_or(items.len() as u64) as usize;
            if items.len() >= total {
                break;
            }
        }
        Ok(json!({"items": items, "totalNumberOfItems": items.len()}))
    }

    async fn get_artist_albums(&mut self, artist_id: &str) -> Result<Value> {
        self.get_all_artist_albums(artist_id, None).await
    }

    async fn get_artist_albums_ep_singles(&mut self, artist_id: &str) -> Result<Value> {
        self.get_all_artist_albums(artist_id, Some("EPSANDSINGLES"))
            .await
    }

    /// Best-effort fetch of credited (contributor) albums via `pages/contributor`.
    /// Only attempted when a Mobile session exists; failures return empty
    /// (mirrors the try/except in Python `get_artist_info`).
    async fn get_credited_albums(&mut self, artist_id: &str) -> Vec<Value> {
        if !self.sessions.contains_key(&SessionType::MobileDefault) {
            return Vec::new();
        }
        let page: Value = match self
            .api_get(
                "pages/contributor",
                [("artistId".to_string(), artist_id.to_string())]
                    .into_iter()
                    .chain(
                        [
                            ("deviceType".to_string(), "TV".to_string()),
                            ("locale".to_string(), "en_US".to_string()),
                            ("mediaFormats".to_string(), "SONY_360".to_string()),
                        ]
                        .into_iter(),
                    )
                    .collect(),
                Some(SessionType::MobileDefault),
            )
            .await
        {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let rows = page
            .get("rows")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let paged_list = rows
            .last()
            .and_then(|r| r.get("modules"))
            .and_then(|m| m.as_array())
            .and_then(|m| m.first())
            .and_then(|m| m.get("pagedList"))
            .cloned();
        let paged_list = match paged_list {
            Some(p) => p,
            None => return Vec::new(),
        };
        let total = paged_list
            .get("totalNumberOfItems")
            .and_then(|t| t.as_u64())
            .unwrap_or(0) as usize;
        let mut api_path = paged_list
            .get("dataApiPath")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .trim()
            .trim_start_matches('/')
            .to_string();
        if api_path.starts_with("v1/") {
            api_path = api_path[3..].to_string();
        }
        if api_path.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let pages = total / 50 + 1;
        for page_idx in 0..pages {
            let mut p = HashMap::new();
            p.insert("limit".to_string(), "50".to_string());
            p.insert("offset".to_string(), (page_idx * 50).to_string());
            // `pages/` paths need device/locale params or the API returns 400.
            if api_path.starts_with("pages/") {
                p.insert("deviceType".to_string(), "TV".to_string());
                p.insert("locale".to_string(), "en_US".to_string());
                p.insert("mediaFormats".to_string(), "SONY_360".to_string());
            }
            let items_page = match self
                .api_get(&api_path, p, Some(SessionType::MobileDefault))
                .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let batch = items_page
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            if batch.is_empty() {
                break;
            }
            for entry in batch {
                if let Some(album) = entry.get("item").and_then(|i| i.get("album")) {
                    out.push(album.clone());
                }
            }
        }
        out
    }

    async fn search(&mut self, query: &str, limit: u32) -> Result<Value> {
        // Mirrors `get_search_data`: no `types` filter; the caller indexes
        // into `tracks`/`albums`/`artists`/`playlists`.
        let mut p = HashMap::new();
        p.insert("query".to_string(), query.to_string());
        p.insert("offset".to_string(), "0".to_string());
        p.insert("limit".to_string(), limit.to_string());
        p.insert("includeContributors".to_string(), "true".to_string());
        self.api_get("search", p, None).await
    }

    async fn get_stream_url(
        &mut self,
        track_id: &str,
        quality: &str,
        session_override: Option<SessionType>,
    ) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("playbackmode".to_string(), "STREAM".to_string());
        p.insert("assetpresentation".to_string(), "FULL".to_string());
        p.insert("audioquality".to_string(), quality.to_string());
        p.insert("prefetch".to_string(), "false".to_string());
        self.api_get(
            &format!("tracks/{track_id}/playbackinfopostpaywall/v4"),
            p,
            session_override,
        )
        .await
    }

    async fn get_preview_url(&mut self, track_id: &str) -> Result<Value> {
        // 30s preview; works on guest sessions (mirrors `get_track_preview_url`).
        let mut p = HashMap::new();
        p.insert("playbackmode".to_string(), "STREAM".to_string());
        p.insert("assetpresentation".to_string(), "PREVIEW".to_string());
        p.insert("audioquality".to_string(), "LOW".to_string());
        p.insert("prefetch".to_string(), "false".to_string());
        self.api_get(
            &format!("tracks/{track_id}/playbackinfopostpaywall/v4"),
            p,
            Some(SessionType::Guest),
        )
        .await
    }

    /// `tracks/{id}/contributors` — returns empty items on 404 instead of
    /// failing (mirrors the tolerant credits handling in Python).
    async fn get_track_contributors(&mut self, track_id: &str) -> Value {
        match self
            .api_get(
                &format!("tracks/{track_id}/contributors"),
                HashMap::new(),
                None,
            )
            .await
        {
            Ok(v) => v,
            Err(_) => json!({"items": []}),
        }
    }
}

// ---------------------------------------------------------------------------
// Playback manifest helpers (parity with `get_track_info`/`parse_mpd`)
// ---------------------------------------------------------------------------

/// Decode a playback `manifest` (base64, standard or URL-safe alphabet).
fn decode_manifest_b64(manifest_b64: &str) -> Option<Vec<u8>> {
    let s = manifest_b64.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(b) = B64.decode(s) {
        return Some(b);
    }
    // Retry with padding restored (Tidal sometimes omits it).
    let mut padded = s.to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    if let Ok(b) = B64.decode(&padded) {
        return Some(b);
    }
    use base64::engine::general_purpose::URL_SAFE as B64U;
    B64U.decode(s).ok().or_else(|| B64U.decode(padded).ok())
}

/// Detect the output codec from a `playbackinfopostpaywall/v4` response.
///
/// Checks the embedded manifest `codecs` first, then the explicit
/// `codec`/`soundQuality` fields, then `audioMode` + `audioQuality`
/// (covers MQA in FLAC as well as E-AC-3 JOC / AC-4 Atmos).
fn detect_codec_from_playback(v: &Value) -> CodecFlags {
    let audio_mode = v.get("audioMode").and_then(|x| x.as_str()).unwrap_or("");
    let audio_quality = v.get("audioQuality").and_then(|x| x.as_str()).unwrap_or("");
    let mime = v
        .get("manifestMimeType")
        .and_then(|x| x.as_str())
        .unwrap_or("");

    if let Some(manifest_b64) = v.get("manifest").and_then(|x| x.as_str()) {
        if let Some(bytes) = decode_manifest_b64(manifest_b64) {
            if let Ok(text) = String::from_utf8(bytes) {
                let lower = text.to_lowercase();
                // Order matters: check immersive/proprietary codecs before FLAC,
                // since MQA payloads are FLAC containers.
                if lower.contains("mha1") {
                    return CodecFlags::MHA1;
                }
                if lower.contains("eac3") || lower.contains("ec-3") {
                    return CodecFlags::EAC3;
                }
                // AC-4 appears as `"codecs":"ac4"` in BTS manifests.
                if lower.contains("\"ac4\"") || lower.contains("ac-4") || lower.contains("ac4") {
                    // Avoid misfiring on e.g. "aac40": only bare ac4 tokens count.
                    if lower.contains("\"ac4\"")
                        || lower.contains("ac-4")
                        || lower.contains("codecs\":\"ac4")
                    {
                        return CodecFlags::AC4;
                    }
                }
                if lower.contains("mqa") {
                    return CodecFlags::MQA;
                }
                if lower.contains("alac") {
                    return CodecFlags::ALAC;
                }
                if lower.contains("mp4a") {
                    return CodecFlags::AAC;
                }
                if lower.contains("flac") {
                    return CodecFlags::FLAC;
                }
            }
        }
        // DASH manifests carry no inline codec string we can cheaply parse;
        // fall through to audioMode-based detection below.
        let _ = mime;
    }

    // Explicit per-track codec fields (present on some responses).
    for key in ["codec", "soundQuality"] {
        if let Some(c) = v.get(key).and_then(|x| x.as_str()) {
            match c.to_uppercase().as_str() {
                "EAC3" | "E-AC-3" | "EAC-3" | "ATMOS" | "DOLBY_ATMOS" => return CodecFlags::EAC3,
                "AC4" | "AC-4" => return CodecFlags::AC4,
                "MHA1" | "SONY_360RA" | "360RA" => return CodecFlags::MHA1,
                "MQA" => return CodecFlags::MQA,
                "ALAC" => return CodecFlags::ALAC,
                "FLAC" => return CodecFlags::FLAC,
                "AAC" | "MP4A" | "MP4A.40.2" | "MP4A.40.5" => return CodecFlags::AAC,
                "HI_RES" | "HI_RES_LOSSLESS" | "LOSSLESS" => return CodecFlags::FLAC,
                "HIGH" | "LOW" => return CodecFlags::AAC,
                _ => {}
            }
        }
    }

    if audio_mode == "DOLBY_ATMOS" {
        return CodecFlags::EAC3;
    }
    if audio_mode == "SONY_360RA" {
        return CodecFlags::MHA1;
    }
    match audio_quality {
        "HI_RES" | "HI_RES_LOSSLESS" | "LOSSLESS" => CodecFlags::FLAC,
        "MQA" => CodecFlags::MQA,
        "HIGH" | "LOW" => CodecFlags::AAC,
        _ => CodecFlags::FLAC,
    }
}

/// Extract the single direct file URL from a non-DASH (BTS JSON) manifest.
/// DASH manifests need segment downloads (not implemented) and error out.
fn extract_stream_url(v: &Value) -> Result<String> {
    let mime = v
        .get("manifestMimeType")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if mime.contains("dash+xml") {
        return Err(Error::Download(
            "Tidal MPEG-DASH stream requires segment download, which is not implemented yet"
                .to_string(),
        ));
    }
    // Legacy/alternate shape: direct `url` field.
    if let Some(u) = v.get("url").and_then(|u| u.as_str()) {
        return Ok(u.to_string());
    }
    let manifest_b64 = v
        .get("manifest")
        .and_then(|m| m.as_str())
        .ok_or_else(|| Error::Other("Tidal stream manifest missing".to_string()))?;
    let bytes = decode_manifest_b64(manifest_b64)
        .ok_or_else(|| Error::Other("Tidal stream manifest is not valid base64".to_string()))?;
    let manifest: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Other(format!("Tidal stream manifest invalid JSON: {e}")))?;
    // BTS shape: {"urls": [...], "codecs": "..."}.
    if let Some(u) = manifest
        .get("urls")
        .and_then(|u| u.as_array())
        .and_then(|u| u.first())
        .and_then(|u| u.as_str())
    {
        return Ok(u.to_string());
    }
    if let Some(u) = manifest.get("url").and_then(|u| u.as_str()) {
        return Ok(u.to_string());
    }
    Err(Error::Other(
        "Tidal stream URL missing from manifest".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Label helpers (parity with `_extract_label_from_*`)
// ---------------------------------------------------------------------------

/// Extract the record label from a credits list. Raw API credits use `role`
/// (contributors endpoint) or `type` (album items/credits endpoint).
fn extract_label_from_credits(credits: Option<&Vec<Value>>) -> Option<String> {
    let credits = credits?;
    let mut labels: Vec<String> = Vec::new();
    for credit in credits {
        let kind = credit
            .get("type")
            .or_else(|| credit.get("role"))
            .and_then(|k| k.as_str());
        if matches!(kind, Some("Record Label" | "Label" | "Production")) {
            if let Some(contributors) = credit.get("contributors").and_then(|c| c.as_array()) {
                for c in contributors {
                    if let Some(name) = c.get("name").and_then(|n| n.as_str()) {
                        if !name.is_empty() && !labels.iter().any(|l| l == name) {
                            labels.push(name.to_string());
                        }
                    }
                }
            } else if let Some(name) = credit.get("name").and_then(|n| n.as_str()) {
                if !name.is_empty() && !labels.iter().any(|l| l == name) {
                    labels.push(name.to_string());
                }
            }
        }
    }
    if labels.is_empty() {
        None
    } else {
        Some(labels.join("; "))
    }
}

/// Strip `(C)`/`(P)`/`©`/`℗` prefixes and leading years from a copyright
/// string, e.g. `"© 2022 Taylor Swift" -> "Taylor Swift"`.
fn extract_label_from_copyright(copyright: Option<&str>) -> Option<String> {
    let s = copyright?.trim();
    if s.is_empty() {
        return None;
    }
    let mut chars: Vec<char> = s.chars().collect();
    let mut idx = 0;
    while idx < chars.len() {
        let c = chars[idx];
        if c == '©'
            || c == '℗'
            || c == '('
            || c == ')'
            || c == 'C'
            || c == 'P'
            || c.is_whitespace()
            || c.is_ascii_digit()
            || c == '-'
        {
            idx += 1;
        } else {
            break;
        }
    }
    // Mirror the Python `{2,}` minimum: don't strip single stray chars.
    if idx < 2 {
        return Some(s.to_string());
    }
    // Avoid stripping a legitimate leading word (e.g. a label starting with
    // "P..."): only strip when the prefix actually contained a symbol/year.
    let prefix: String = chars.iter().take(idx).collect();
    let has_marker = prefix
        .chars()
        .any(|c| c == '©' || c == '℗' || c.is_ascii_digit() || (c == '(' || c == ')'));
    if !has_marker {
        return Some(s.to_string());
    }
    chars.drain(..idx);
    let res: String = chars.into_iter().collect();
    let res = res.trim().to_string();
    if res.is_empty() {
        None
    } else {
        Some(res)
    }
}

/// Normalise contributor roles to standard tagging keys
/// (mirrors `role_mapping` in Python `get_track_credits`).
fn normalise_credit_role(role: &str) -> &str {
    match role {
        "Lyricist" | "Lyricists" | "Vocals" => "Lyricist",
        "Composer" | "Composers" => "Composer",
        "Producer" | "Producers" => "Producer",
        "Music Publisher" => "Music Publisher",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// High-level module
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct TidalModule {
    controller: ModuleController,
    state: Mutex<TidalApiState>,
    #[allow(dead_code)]
    quality_parse: HashMap<Quality, u32>,
}

impl TidalModule {
    fn new(
        controller: ModuleController,
        access: String,
        refresh: String,
        user_id: String,
        country: String,
    ) -> Self {
        let settings = controller.module_settings.clone();
        let tv_token = setting_str(&settings, "tv_atmos_token", "4N3n6Q1x95LL5K7p");
        let tv_secret = setting_str(
            &settings,
            "tv_atmos_secret",
            "oKOXfJW371cX6xaZ0PyhgGNBdNLlBZd4AKKYougMjik=",
        );
        let mobile_atmos_token =
            setting_str(&settings, "mobile_atmos_hires_token", "km8T1xS355y7dd3H");
        let mobile_token = setting_str(&settings, "mobile_hires_token", "6BDSRdpK9hqEBTgU");
        let guest_token = setting_str(&settings, "guest_token", "txNoH4kkV41MfH25");
        let guest_secret = setting_str(
            &settings,
            "guest_secret",
            "dQjy0MinCEvxi1O4UmxvxWnDjt4cgHBPw8ll6nYBk98=",
        );
        let enable_mobile = setting_bool(&settings, "enable_mobile", true);
        let prefer_ac4 = setting_bool(&settings, "prefer_ac4", false);

        let mut sessions: HashMap<SessionType, TidalSession> = HashMap::new();

        // TV session holds the stored user tokens (like the Python TV login).
        let mut tv = TidalSession::new(SessionType::Tv, tv_token, Some(tv_secret), country.clone());
        if !access.is_empty() {
            tv.access_token = Some(access);
        }
        if !refresh.is_empty() {
            tv.refresh_token = Some(refresh.clone());
        }
        if !user_id.is_empty() {
            tv.user_id = Some(user_id.clone());
        }
        sessions.insert(SessionType::Tv, tv);

        // Mobile sessions share the refresh token: refresh tokens work with
        // any client id, so an existing login can be switched to any client
        // type (mirrors `auth_session` with a `login_session`).
        if enable_mobile {
            let mut mobile = TidalSession::new(
                SessionType::MobileDefault,
                mobile_token,
                None,
                country.clone(),
            );
            let mut mobile_atmos = TidalSession::new(
                SessionType::MobileAtmos,
                mobile_atmos_token,
                None,
                country.clone(),
            );
            if !refresh.is_empty() {
                mobile.refresh_token = Some(refresh.clone());
                mobile_atmos.refresh_token = Some(refresh.clone());
            }
            if !user_id.is_empty() {
                mobile.user_id = Some(user_id.clone());
                mobile_atmos.user_id = Some(user_id.clone());
            }
            // Carry over a valid access token too; the client id header is
            // what differs per session flavour.
            if let Some(tv_session) = sessions.get(&SessionType::Tv) {
                let token = tv_session.access_token.clone();
                if token.as_ref().map(|t| !t.is_empty()).unwrap_or(false) {
                    mobile.access_token = token.clone();
                    mobile_atmos.access_token = token;
                }
            }
            sessions.insert(SessionType::MobileDefault, mobile);
            sessions.insert(SessionType::MobileAtmos, mobile_atmos);
        }

        // Guest session is always available as a metadata fallback.
        let guest_country = if country.is_empty() {
            "US".to_string()
        } else {
            country
        };
        let guest = TidalSession::new(
            SessionType::Guest,
            guest_token,
            Some(guest_secret),
            guest_country,
        );
        sessions.insert(SessionType::Guest, guest);

        let authenticated = sessions.values().any(|s| s.is_authenticated());
        let default = if authenticated {
            SessionType::Tv
        } else {
            SessionType::Guest
        };
        let mut available = vec![SessionType::Tv];
        if enable_mobile {
            available.push(SessionType::MobileDefault);
            available.push(SessionType::MobileAtmos);
        }

        let mut quality_parse = HashMap::new();
        quality_parse.insert(Quality::MINIMUM, 1u32);
        quality_parse.insert(Quality::LOW, 1);
        quality_parse.insert(Quality::MEDIUM, 1);
        quality_parse.insert(Quality::HIGH, 2);
        quality_parse.insert(Quality::LOSSLESS, 2);
        quality_parse.insert(Quality::HIFI, 2);
        quality_parse.insert(Quality::ATMOS, 2);
        Self {
            controller,
            state: Mutex::new(TidalApiState {
                sessions,
                default,
                available,
                prefer_ac4,
            }),
            quality_parse,
        }
    }

    /// Map a quality tier to the Tidal `audioquality` parameter.
    /// Mirrors `quality_parse` in `interface.py` (incl. HI_RES/ATMOS tiers).
    fn quality_to_str(&self, q: Quality) -> &'static str {
        // LOW = 96k AAC, HIGH = 320k AAC, LOSSLESS = 44.1/16 FLAC,
        // HI_RES_LOSSLESS = <= 48/24 FLAC (also used for ATMOS tier + MQA).
        if q.contains(Quality::ATMOS) {
            "HI_RES_LOSSLESS"
        } else if q.contains(Quality::HIFI) {
            "HI_RES_LOSSLESS"
        } else if q.contains(Quality::LOSSLESS) {
            "LOSSLESS"
        } else if q.contains(Quality::HIGH) {
            "HIGH"
        } else if q.contains(Quality::MEDIUM) {
            "HIGH"
        } else {
            "LOW"
        }
    }

    /// Pick the session flavour + audioquality for a track, mirroring the
    /// format/session mapping at the top of Python `get_track_info`:
    /// hi-res/360RA go to Mobile, AC-4 Atmos to MobileAtmos, E-AC-3 Atmos
    /// and plain stereo to TV (TV avoids MPEG-DASH).
    async fn pick_stream_session(
        &self,
        track_data: &Value,
        quality: Quality,
        codec_options: &CodecOptions,
    ) -> (Option<SessionType>, String) {
        let tags: Vec<String> = track_data
            .get("mediaMetadata")
            .and_then(|m| m.get("tags"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|t| t.as_str().map(|s| s.to_string()))
            .collect();
        let st = self.state.lock().await;
        let spatial = codec_options.spatial_codecs && quality.contains(Quality::ATMOS);
        let mut format: Option<&str> = None;
        if spatial {
            if tags.iter().any(|t| t == "SONY_360RA") {
                format = Some("360ra");
            } else if tags.iter().any(|t| t == "DOLBY_ATMOS") {
                // prefer_ac4 False -> TV session -> E-AC-3 JOC (plays in
                // VLC/Audacity). True -> MOBILE_ATMOS -> AC-4.
                format = Some(if st.prefer_ac4 { "ac4" } else { "ac3" });
            }
        }
        if tags.iter().any(|t| t == "HIRES_LOSSLESS")
            && format.is_none()
            && quality.contains(Quality::HIFI)
        {
            format = Some("flac_hires");
        }
        let mut session = match format {
            Some("flac_hires") | Some("360ra") => Some(SessionType::MobileDefault),
            Some("ac4") => Some(SessionType::MobileAtmos),
            Some("ac3") => Some(SessionType::Tv),
            _ => None,
        };
        if session.is_none() && tags.iter().any(|t| t == "DOLBY_ATMOS") {
            // Atmos is available but not requested: don't use the TV session
            // here because it would return Atmos every time.
            session = Some(SessionType::MobileDefault);
        }
        if session.is_none() {
            // TV whenever possible to avoid MPEG-DASH, which slows downloading.
            session = Some(SessionType::Tv);
        }
        // Only use sessions the user enabled; otherwise fall back to default.
        if let Some(want) = session {
            if !st.available.contains(&want) {
                session = None;
            } else if !st.sessions.contains_key(&want) {
                session = None;
            }
        }
        let quality_str = if format == Some("flac_hires") {
            "HI_RES_LOSSLESS".to_string()
        } else {
            self.quality_to_str(quality).to_string()
        };
        (session, quality_str)
    }

    fn tidal_cover(id: &str, size: u32) -> String {
        format!(
            "https://resources.tidal.com/images/{}/{size}x{size}.jpg",
            id.replace('-', "/")
        )
    }
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for TidalModule {
    fn name(&self) -> &str {
        "Tidal"
    }

    fn is_authenticated(&self) -> bool {
        self.state
            .try_lock()
            .map(|s| s.is_authenticated())
            .unwrap_or(false)
    }

    async fn ensure_can_download(&self) -> Result<()> {
        Ok(())
    }

    async fn login(&self, _email: &str, _password: &str) -> Result<()> {
        Err(Error::Other("Tidal login via email/password is unsupported; use OAuth at https://link.tidal.com/XXXXXXX or set the access/refresh tokens in settings.json".into()))
    }

    async fn logout(&self) -> Result<()> {
        let mut s = self.state.lock().await;
        for session in s.sessions.values_mut() {
            session.access_token = None;
            session.refresh_token = None;
            session.user_id = None;
        }
        s.default = SessionType::Guest;
        Ok(())
    }

    async fn get_track_info(
        &self,
        track_id: &str,
        _quality: Quality,
        codec_options: &CodecOptions,
        data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        let tier = self.controller.picaro_options.quality_tier;
        let track_data = if let Some(v) = data.get(track_id) {
            v.clone()
        } else {
            self.state.lock().await.get_track(track_id).await?
        };
        // Album fallback for region-locked albums (mirrors the Python workaround).
        let album_id_guess = track_data
            .get("album")
            .and_then(|a| a.get("id"))
            .and_then(|v| v.as_i64())
            .map(|i| i.to_string())
            .unwrap_or_default();
        let album_data = if let Some(v) = data.get(&album_id_guess) {
            v.clone()
        } else if !album_id_guess.is_empty() {
            match self.state.lock().await.get_album(&album_id_guess).await {
                Ok(a) => a,
                Err(_) => {
                    let mut fallback = track_data.get("album").cloned().unwrap_or(json!({}));
                    if let Some(obj) = fallback.as_object_mut() {
                        obj.entry("artist").or_insert_with(|| {
                            track_data.get("artist").cloned().unwrap_or(Value::Null)
                        });
                        obj.entry("numberOfVolumes").or_insert(json!(1));
                        obj.entry("audioQuality").or_insert(json!("LOSSLESS"));
                        obj.entry("audioModes").or_insert(json!(["STEREO"]));
                    }
                    fallback
                }
            }
        } else {
            track_data.get("album").cloned().unwrap_or(json!({}))
        };
        // Fetch credits if missing (needed for label extraction).
        let mut track_data = track_data;
        if track_data.get("credits").is_none() {
            let contrib = self
                .state
                .lock()
                .await
                .get_track_contributors(track_id)
                .await;
            let items = contrib
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            track_data["credits"] = Value::Array(items);
        }
        let album = track_data.get("album");
        let name = track_data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let version = track_data
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let track_name = match version {
            Some(v) if !v.is_empty() => format!("{name} ({v})"),
            _ => name,
        };
        let artist = track_data
            .get("artist")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let artist_id = track_data
            .get("artist")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_i64())
            .map(|i| i.to_string());
        let album_name = album
            .and_then(|a| a.get("title"))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| {
                album_data
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Album")
            })
            .to_string();
        let album_id = album
            .and_then(|a| a.get("id"))
            .and_then(|v| v.as_i64())
            .map(|i| i.to_string())
            .unwrap_or_else(|| album_id_guess.clone());
        let duration = track_data
            .get("duration")
            .and_then(|v| v.as_u64())
            .map(|i| i as u32);
        let track_number = track_data
            .get("trackNumber")
            .and_then(|v| v.as_u64())
            .map(|i| i as u32);
        let volume_number = track_data
            .get("volumeNumber")
            .and_then(|v| v.as_u64())
            .map(|i| i as u32);
        let isrc = track_data
            .get("isrc")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let cover = album
            .and_then(|a| a.get("cover"))
            .and_then(|v| v.as_str())
            .map(|id| Self::tidal_cover(id, 1280))
            .unwrap_or_default();
        // Label: credits first, copyright fallback (mirrors `convert_tags`).
        let credits_list = track_data.get("credits").and_then(|c| c.as_array());
        let mut label = extract_label_from_credits(credits_list);
        if label.is_none() {
            label =
                extract_label_from_copyright(track_data.get("copyright").and_then(|c| c.as_str()));
        }
        let mut tags = Tags {
            track_number,
            disc_number: volume_number,
            isrc,
            copyright: track_data
                .get("copyright")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            release_date: album
                .and_then(|a| a.get("releaseDate"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            track_url: Some(format!("https://tidal.com/browse/track/{track_id}")),
            label,
            ..Default::default()
        };
        if let Some(total) = album_data.get("numberOfTracks").and_then(|v| v.as_u64()) {
            tags.total_tracks = Some(total as u32);
        }
        if let Some(volumes) = album_data.get("numberOfVolumes").and_then(|v| v.as_u64()) {
            tags.total_discs = Some(volumes as u32);
        }
        if let Some(upc) = album_data.get("upc").and_then(|v| v.as_str()) {
            tags.upc = Some(upc.to_string());
        }

        // Route the stream request to the right session flavour.
        let (session_override, quality_str) = self
            .pick_stream_session(&track_data, tier, codec_options)
            .await;
        {
            let mut st = self.state.lock().await;
            if let Some(want) = session_override {
                st.set_default(want);
            }
        }
        let stream = self
            .state
            .lock()
            .await
            .get_stream_url(track_id, &quality_str, session_override)
            .await;
        let (codec, bit_depth, sample_rate, bitrate) = match &stream {
            Ok(v) => {
                let detected = detect_codec_from_playback(v);
                let audio_quality = v.get("audioQuality").and_then(|x| x.as_str()).unwrap_or("");
                let audio_mode = v.get("audioMode").and_then(|x| x.as_str()).unwrap_or("");
                let bd = if matches!(detected, CodecFlags::FLAC | CodecFlags::ALAC) {
                    Some(
                        if audio_quality == "HI_RES_LOSSLESS" || audio_quality == "HI_RES" {
                            24
                        } else {
                            16
                        },
                    )
                } else {
                    None
                };
                let sr = if detected.is_spatial() {
                    Some(48.0)
                } else if audio_quality == "HI_RES_LOSSLESS" || audio_quality == "HI_RES" {
                    Some(48.0)
                } else {
                    Some(44.1)
                };
                let mut br: Option<u32> = match audio_quality {
                    "LOW" => Some(96),
                    "HIGH" => Some(320),
                    "LOSSLESS" => Some(1411),
                    _ => None,
                };
                if audio_mode == "DOLBY_ATMOS" {
                    br = match detected {
                        CodecFlags::EAC3 => Some(768),
                        CodecFlags::AC4 => Some(256),
                        _ => br,
                    };
                } else if audio_mode == "SONY_360RA" {
                    br = Some(667);
                }
                (detected, bd, sr, br)
            }
            Err(_) => (CodecFlags::NONE, None, None, None),
        };
        Ok(TrackInfo {
            name: track_name,
            album: album_name,
            album_id,
            artists: vec![artist],
            tags,
            codec,
            cover_url: cover,
            release_year: track_data
                .get("streamStartDate")
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0),
            duration,
            explicit: track_data.get("explicit").and_then(|v| v.as_bool()),
            artist_id,
            id: Some(track_id.to_string()),
            bit_depth,
            sample_rate,
            bitrate,
            ..Default::default()
        })
    }

    async fn get_track_download(
        &self,
        track_id: &str,
        quality: Quality,
        codec_options: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        // Route via the format-aware session so Atmos/hi-res resolve correctly.
        let track_data = self.state.lock().await.get_track(track_id).await.ok();
        let (session_override, quality_str) = match &track_data {
            Some(td) => self.pick_stream_session(td, quality, codec_options).await,
            None => (None, self.quality_to_str(quality).to_string()),
        };
        let v = self
            .state
            .lock()
            .await
            .get_stream_url(track_id, &quality_str, session_override)
            .await?;
        let url = extract_stream_url(&v)?;
        let codec = detect_codec_from_playback(&v);
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(url),
            file_url_headers: {
                let mut h = serde_json::Map::new();
                h.insert(
                    "User-Agent".to_string(),
                    Value::String("Tidal/2.0".to_string()),
                );
                h
            },
            temp_file_path: None,
            different_codec: Some(codec),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let album = if let Some(v) = data.get(album_id) {
            v.clone()
        } else {
            self.state.lock().await.get_album(album_id).await?
        };
        // Prefer the credits endpoint (tracks + per-track credits for label
        // extraction), falling back to the plain items endpoint.
        let contrib = self
            .state
            .lock()
            .await
            .get_album_contributors(album_id)
            .await
            .ok();
        let mut label: Option<String> = None;
        let track_ids: Vec<TrackRef> = if let Some(c) = &contrib {
            let items = c
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if let Some(first) = items.first() {
                label = extract_label_from_credits(first.get("credits").and_then(|c| c.as_array()));
            }
            items
                .iter()
                .filter_map(|entry| {
                    let entry_type = entry
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("track");
                    if entry_type != "track" {
                        return None;
                    }
                    let inner = entry.get("item").unwrap_or(entry);
                    inner
                        .get("id")
                        .and_then(|v| v.as_i64())
                        .map(|i| TrackRef::Id(i.to_string()))
                })
                .collect()
        } else {
            let tracks = self
                .state
                .lock()
                .await
                .get_album_items_all(album_id)
                .await?;
            tracks
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|t| {
                    // items endpoint wraps in `item` for some responses
                    let inner = t.get("item").unwrap_or(t);
                    inner
                        .get("id")
                        .and_then(|v| v.as_i64())
                        .map(|i| TrackRef::Id(i.to_string()))
                })
                .collect()
        };
        // Label fallback chain: credits -> copyright (mirrors Python).
        if label.is_none() {
            // `get_label_info` reuses the same endpoints; only call it when we
            // didn't already fetch contributors (avoids a duplicate request).
            if contrib.is_none() {
                label = self.state.lock().await.get_label_info(album_id).await;
            }
            if label.is_none() {
                label =
                    extract_label_from_copyright(album.get("copyright").and_then(|c| c.as_str()));
            }
        }
        let cover = album
            .get("cover")
            .and_then(|v| v.as_str())
            .map(|id| Self::tidal_cover(id, 1280))
            .unwrap_or_default();
        // Quality traits (mirrors the Python quality_list).
        let mut quality_list: Vec<&str> = Vec::new();
        if let Some(modes) = album.get("audioModes").and_then(|m| m.as_array()) {
            let modes: Vec<&str> = modes.iter().filter_map(|m| m.as_str()).collect();
            if modes.contains(&"DOLBY_ATMOS") {
                quality_list.push("ATMOS");
            }
            if modes.contains(&"SONY_360RA") {
                quality_list.push("360 Reality Audio");
            }
        }
        if album.get("audioQuality").and_then(|q| q.as_str()) == Some("HI_RES") {
            quality_list.push("HI-RES");
        }
        let quality = if quality_list.is_empty() {
            None
        } else {
            Some(quality_list.join(" / "))
        };
        Ok(AlbumInfo {
            name: album
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string(),
            artist: album
                .get("artist")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string(),
            artist_id: album
                .get("artist")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_i64())
                .map(|i| i.to_string()),
            tracks: track_ids,
            release_year: album
                .get("releaseDate")
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0),
            cover_url: Some(cover),
            id: Some(album_id.to_string()),
            quality,
            label,
            upc: album
                .get("upc")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            duration: album
                .get("duration")
                .and_then(|v| v.as_u64())
                .map(|d| d as u32),
            explicit: album.get("explicit").and_then(|v| v.as_bool()),
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        let pl = self.state.lock().await.get_playlist(playlist_id).await?;
        let tracks = self
            .state
            .lock()
            .await
            .get_playlist_tracks(playlist_id)
            .await?;
        let track_ids: Vec<TrackRef> = tracks
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|t| {
                t.get("id")
                    .and_then(|v| v.as_i64())
                    .map(|i| TrackRef::Id(i.to_string()))
            })
            .collect();
        let creator = pl
            .get("creator")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if pl.get("type").and_then(|t| t.as_str()) == Some("EDITORIAL") {
                    "Tidal".to_string()
                } else {
                    "Unknown".to_string()
                }
            });
        let (cover_url, cover_type) = match pl.get("squareImage").and_then(|v| v.as_str()) {
            Some(id) => (Some(Self::tidal_cover(id, 1080)), Some(ImageFileType::Jpg)),
            None => (
                Some(
                    "https://tidal.com/browse/assets/images/defaultImages/defaultPlaylistImage.png"
                        .to_string(),
                ),
                Some(ImageFileType::Png),
            ),
        };
        Ok(PlaylistInfo {
            name: pl
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string(),
            creator,
            tracks: track_ids,
            release_year: pl
                .get("created")
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0),
            id: Some(playlist_id.to_string()),
            creator_id: pl
                .get("creator")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_i64())
                .map(|i| i.to_string()),
            duration: pl
                .get("duration")
                .and_then(|v| v.as_u64())
                .map(|d| d as u32),
            cover_url,
            cover_type,
            ..Default::default()
        })
    }

    async fn get_artist_info(
        &self,
        artist_id: &str,
        get_credited_albums: bool,
        _artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        let a = self.state.lock().await.get_artist(artist_id).await?;
        let artist_name = a
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        // Albums + EPs/singles (mirrors Python calling both endpoints).
        let albums_v = self.state.lock().await.get_artist_albums(artist_id).await?;
        let singles_v = self
            .state
            .lock()
            .await
            .get_artist_albums_ep_singles(artist_id)
            .await
            .unwrap_or(json!({"items": []}));
        let mut all: Vec<Value> = albums_v
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        all.extend(
            singles_v
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
        );
        // Credited albums flag passthrough: contributor pages via the Mobile
        // session (best-effort; empty on failure, like Python).
        if get_credited_albums {
            all.extend(self.state.lock().await.get_credited_albums(artist_id).await);
        }
        // Deduplicate by album id, keeping rich dicts for the GUI.
        let mut seen = std::collections::HashSet::new();
        let albums: Vec<Value> = all
            .iter()
            .filter_map(|alb| {
                let id = alb.get("id").and_then(|v| v.as_i64())?.to_string();
                if !seen.insert(id.clone()) {
                    return None;
                }
                let title = alb
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string();
                let artist = alb
                    .get("artist")
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| artist_name.clone());
                let year = alb
                    .get("releaseDate")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.split('-').next())
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                Some(json!({
                    "id": id,
                    "name": title,
                    "artist": artist,
                    "release_year": year,
                }))
            })
            .collect();
        Ok(ArtistInfo {
            name: artist_name,
            artist_id: Some(artist_id.to_string()),
            albums,
            ..Default::default()
        })
    }

    async fn get_track_credits(
        &self,
        track_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<Vec<CreditsInfo>> {
        let mut credits_dict: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(cached) = data.get(track_id) {
            // Cached credits from `get_album_info` use the items/credits shape:
            // [{type, contributors: [{name}]}].
            let list: Vec<Value> = if let Some(arr) = cached.as_array() {
                arr.clone()
            } else if let Some(arr) = cached.get("credits").and_then(|c| c.as_array()) {
                arr.clone()
            } else {
                vec![cached.clone()]
            };
            for contributor in &list {
                let role = contributor
                    .get("type")
                    .or_else(|| contributor.get("role"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("Unknown");
                let role = normalise_credit_role(role).to_string();
                let entry = credits_dict.entry(role).or_default();
                if let Some(subs) = contributor.get("contributors").and_then(|c| c.as_array()) {
                    for c in subs {
                        if let Some(name) = c.get("name").and_then(|n| n.as_str()) {
                            entry.push(name.to_string());
                        }
                    }
                } else if let Some(name) = contributor.get("name").and_then(|n| n.as_str()) {
                    entry.push(name.to_string());
                }
            }
        } else {
            // `tracks/{id}/contributors` shape: [{role, name}]. Returns empty
            // items on 404 instead of failing.
            let resp = self
                .state
                .lock()
                .await
                .get_track_contributors(track_id)
                .await;
            let items = resp
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            for contributor in &items {
                if let Some(role) = contributor.get("role").and_then(|r| r.as_str()) {
                    let role = normalise_credit_role(role).to_string();
                    if let Some(name) = contributor.get("name").and_then(|n| n.as_str()) {
                        credits_dict.entry(role).or_default().push(name.to_string());
                    }
                }
            }
        }
        Ok(credits_dict
            .into_iter()
            .map(|(k, v)| CreditsInfo {
                credit_type: k,
                names: v,
            })
            .collect())
    }

    async fn get_track_cover(
        &self,
        track_id: &str,
        _cover: &CoverOptions,
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        let t = self.state.lock().await.get_track(track_id).await?;
        let cover = t
            .get("album")
            .and_then(|a| a.get("cover"))
            .and_then(|v| v.as_str())
            .map(|id| Self::tidal_cover(id, 1280));
        Ok(CoverInfo {
            url: cover.unwrap_or_default(),
            file_type: ImageFileType::Jpg,
        })
    }

    async fn get_track_lyrics(
        &self,
        track_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<LyricsInfo> {
        // Mirrors `get_lyrics`: TV device params; empty result on failure.
        let mut p = HashMap::new();
        p.insert("deviceType".to_string(), "TV".to_string());
        p.insert("locale".to_string(), "en_US".to_string());
        let v = match self
            .state
            .lock()
            .await
            .api_get(&format!("tracks/{track_id}/lyrics"), p, None)
            .await
        {
            Ok(v) => v,
            Err(_) => return Ok(LyricsInfo::default()),
        };
        if v.get("error").is_some() {
            return Ok(LyricsInfo::default());
        }
        let embedded = v
            .get("lyrics")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let synced = v
            .get("subtitles")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        Ok(LyricsInfo { embedded, synced })
    }

    async fn search(
        &self,
        query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        // ISRC fast-path (mirrors Python `search` with track_info).
        let items = if query_type == DownloadType::track {
            if let Some(ti) = _track_info {
                if let Some(isrc) = &ti.tags.isrc {
                    if !isrc.is_empty() {
                        let mut p = HashMap::new();
                        p.insert("isrc".to_string(), isrc.clone());
                        if let Ok(v) = self.state.lock().await.api_get("tracks", p, None).await {
                            if v.get("items")
                                .and_then(|i| i.as_array())
                                .map(|a| !a.is_empty())
                                .unwrap_or(false)
                            {
                                v.get("items")
                                    .and_then(|v| v.as_array())
                                    .cloned()
                                    .unwrap_or_default()
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let items = if query_type == DownloadType::track && !items.is_empty() {
            items
        } else {
            let v = self.state.lock().await.search(query, limit).await?;
            let key = if query_type == DownloadType::track {
                "tracks"
            } else if query_type == DownloadType::album {
                "albums"
            } else if query_type == DownloadType::artist {
                "artists"
            } else if query_type == DownloadType::playlist {
                "playlists"
            } else {
                "tracks"
            };
            v.get(key)
                .and_then(|v| v.get("items"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        };
        let mut out = Vec::new();
        for item in items {
            let id = item
                .get("id")
                .and_then(|v| v.as_i64())
                .map(|i| i.to_string())
                .or_else(|| {
                    item.get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .or_else(|| {
                    // Playlists are keyed by uuid in Python search results.
                    item.get("uuid")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });
            let name = item
                .get("title")
                .or_else(|| item.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let artist = item
                .get("artist")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| vec![s.to_string()]);
            let cover = item
                .get("cover")
                .or_else(|| item.get("picture"))
                .and_then(|v| v.as_str())
                .map(|id| Self::tidal_cover(id, 320));
            out.push(SearchResult {
                result_id: id.unwrap_or_default(),
                name,
                artists: artist,
                duration: item
                    .get("duration")
                    .and_then(|v| v.as_u64())
                    .map(|i| i as u32),
                explicit: item.get("explicit").and_then(|v| v.as_bool()),
                image_url: cover,
                ..Default::default()
            });
        }
        Ok(out)
    }

    async fn get_preview_stream_url(&self, track_id: &str) -> Result<Option<String>> {
        // PREVIEW assetpresentation on the guest session first (mirrors
        // `get_track_preview_url`), falling back to a LOW full-stream URL.
        if let Ok(v) = self.state.lock().await.get_preview_url(track_id).await {
            if let Ok(url) = extract_stream_url(&v) {
                return Ok(Some(url));
            }
        }
        let v = self
            .state
            .lock()
            .await
            .get_stream_url(track_id, "LOW", None)
            .await
            .ok();
        Ok(v.and_then(|v| extract_stream_url(&v).ok()))
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}

#[allow(dead_code)]
fn _base64_smoke() {
    let _ = B64.encode(b"hello world");
}
