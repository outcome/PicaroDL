//! Beatport module - port of `modules/beatport/{interface,beatport_api}.py`.
//!
//! Auth: username/password (OAuth code flow) or anonymous token scraped from
//! the homepage. Downloads: 256k AAC (`medium`) for all tiers; lossless FLAC
//! when the account permits it.

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

const API_URL: &str = "https://api.beatport.com/v4/";
const CLIENT_ID: &str = "Zy2K9Wvy6DkUds7g8s1GNMHfk17E5Ch2BWHlyaGY";
const REDIRECT_URI: &str = "seratodjlite://beatport";
const HOMEPAGE: &str = "https://www.beatport.com/";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "Beatport".to_string(),
        module_supported_modes: ModuleModes::download | ModuleModes::covers,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("username".to_string(), json!(""));
            m.insert("password".to_string(), json!(""));
            m
        },
        session_storage_variables: vec![
            "access_token".to_string(),
            "refresh_token".to_string(),
            "expires".to_string(),
        ],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("beatport".to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("track".to_string(), DownloadType::track);
            m.insert("release".to_string(), DownloadType::album);
            m.insert("chart".to_string(), DownloadType::playlist);
            m.insert("artist".to_string(), DownloadType::artist);
            m.insert("label".to_string(), DownloadType::label);
            m
        },
        test_url: Some("https://www.beatport.com/track/darkside/10844269".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(BeatportConstructor)
}

#[derive(Debug)]
struct BeatportConstructor;

impl ModuleConstructor for BeatportConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        let username = controller
            .module_settings
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let password = controller
            .module_settings
            .get("password")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Restore persisted session (mirrors interface.py set_session()).
        let access = controller
            .temporary_settings_controller
            .read("access_token", TempSettingType::Custom)
            .ok()
            .flatten()
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let refresh = controller
            .temporary_settings_controller
            .read("refresh_token", TempSettingType::Custom)
            .ok()
            .flatten()
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let expires = controller
            .temporary_settings_controller
            .read("expires", TempSettingType::Custom)
            .ok()
            .flatten()
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
            });
        let mut session = BeatportSession::new();
        session.set_session(access, refresh, expires);
        Ok(Arc::new(BeatportModule {
            controller,
            session: Mutex::new(session),
            username,
            password,
            is_pro: Mutex::new(false),
        }))
    }
}

#[derive(Debug, Clone)]
struct BeatportSession {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires: Option<i64>,
    client: reqwest::Client,
}

impl BeatportSession {
    fn new() -> Self {
        Self {
            access_token: None,
            refresh_token: None,
            expires: None,
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }
    }

    fn set_session(
        &mut self,
        access: Option<String>,
        refresh: Option<String>,
        expires: Option<i64>,
    ) {
        self.access_token = access.filter(|s| !s.is_empty());
        self.refresh_token = refresh.filter(|s| !s.is_empty());
        self.expires = expires;
    }

    fn get_session(&self) -> (Option<String>, Option<String>, Option<i64>) {
        (
            self.access_token.clone(),
            self.refresh_token.clone(),
            self.expires,
        )
    }

    fn is_expired(&self) -> bool {
        match self.expires {
            Some(exp) => chrono::Utc::now().timestamp() > exp,
            None => false,
        }
    }

    async fn ensure_token(&mut self) -> Result<()> {
        if self
            .access_token
            .as_ref()
            .map(|t| !t.is_empty())
            .unwrap_or(false)
            && !self.is_expired()
        {
            return Ok(());
        }
        // Persisted refresh token but expired access token: try refresh first
        // (mirrors interface.py __init__ refresh_login path).
        if self.refresh_token.is_some() {
            if self.refresh().await.is_ok() {
                return Ok(());
            }
            // Refresh failed - fall through to anonymous so callers can
            // re-login with credentials via the module layer.
        }
        self.get_anonymous_token().await
    }

    async fn get_anonymous_token(&mut self) -> Result<()> {
        let resp = self
            .client
            .get(HOMEPAGE)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatport homepage: {e}")))?;
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("Beatport homepage body: {e}")))?;
        let re = regex::Regex::new(
            r#"<script id="__NEXT_DATA__" type="application/json">(.*?)</script>"#,
        )
        .map_err(|e| Error::Other(format!("regex: {e}")))?;
        let cap = re.captures(&text).ok_or_else(|| {
            Error::Other("Could not find __NEXT_DATA__ on Beatport homepage".to_string())
        })?;
        let data: Value = serde_json::from_str(&cap[1])
            .map_err(|e| Error::Other(format!("Beatport NEXT_DATA JSON: {e}")))?;
        let (token, expires_in) = find_anon_session(&data).ok_or_else(|| {
            Error::Other("Could not find anonymous access token on Beatport homepage".to_string())
        })?;
        self.access_token = Some(token);
        self.refresh_token = None;
        self.expires = Some(chrono::Utc::now().timestamp() + expires_in);
        Ok(())
    }

    /// OAuth code flow: authorize -> login -> authorize -> token exchange.
    /// Mirrors `BeatportApi.auth()` in beatport_api.py.
    async fn auth(&mut self, username: &str, password: &str) -> Result<Value> {
        if username.is_empty() || password.is_empty() {
            return Err(Error::Other(
                "Beatport credentials are missing in settings.json. Please fill in: username, password.".to_string(),
            ));
        }
        // Dedicated no-redirect client so we can capture the 302 Location
        // headers carrying the login URL / auth code (allow_redirects=False).
        // Cookies only need to live for the duration of this sequence.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .cookie_store(true)
            .user_agent(UA)
            .build()
            .map_err(|e| Error::Other(format!("Beatport auth client: {e}")))?;

        // 1. authorize -> expect 302 with login URL in Location
        let r = client
            .get(format!("{API_URL}auth/o/authorize/"))
            .query(&[
                ("client_id", CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", REDIRECT_URI),
            ])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatport authorize: {e}")))?;
        if r.status() != reqwest::StatusCode::FOUND {
            let t = r.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Beatport authorize failed: {t}")));
        }
        let location = r
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        // Rebuild referer like the Python version (base of request URL + location).
        let referer = if location.starts_with("http") {
            location.clone()
        } else {
            let base = format!("{API_URL}auth/o/authorize/").replace("/auth/o/authorize/", "");
            format!("{base}{location}")
        };

        // 2. login with credentials
        let r = client
            .post(format!("{API_URL}auth/login/"))
            .header("Referer", referer)
            .json(&json!({"username": username, "password": password}))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatport login: {e}")))?;
        if !r.status().is_success() {
            let text = r.text().await.unwrap_or_default();
            // Surface blank-field errors like interface.py does.
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if v.get("username").is_some() && v.get("password").is_some() {
                    return Err(Error::Other(
                        "Beatport credentials are missing in settings.json. Please fill in: username, password.".to_string(),
                    ));
                }
                if let Some(desc) = v.get("error_description") {
                    return Err(Error::Other(format!("Beatport login: {desc}")));
                }
            }
            return Err(Error::Other(format!("Beatport login failed: {text}")));
        }

        // 3. authorize again -> expect 302 with code in Location
        let r = client
            .get(format!("{API_URL}auth/o/authorize/"))
            .query(&[
                ("client_id", CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", REDIRECT_URI),
            ])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatport authorize (code): {e}")))?;
        if r.status() != reqwest::StatusCode::FOUND {
            let t = r.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Beatport authorize (code) failed: {t}"
            )));
        }
        let location = r
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let code = location
            .split("code=")
            .nth(1)
            .unwrap_or("")
            .split('&')
            .next()
            .unwrap_or("")
            .to_string();
        if code.is_empty() {
            return Err(Error::Other(format!(
                "Beatport: no auth code in redirect: {location}"
            )));
        }

        // 4. exchange code for tokens
        let r = client
            .post(format!("{API_URL}auth/o/token/"))
            .form(&[
                ("client_id", CLIENT_ID),
                ("code", code.as_str()),
                ("grant_type", "authorization_code"),
                ("redirect_uri", REDIRECT_URI),
            ])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatport token exchange: {e}")))?;
        if !r.status().is_success() {
            let t = r.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Beatport token exchange failed: {t}")));
        }
        let v: Value = r
            .json()
            .await
            .map_err(|e| Error::Other(format!("Beatport token JSON: {e}")))?;
        if let Some(desc) = v.get("error_description") {
            if !desc.is_null() {
                return Err(Error::Other(format!("Beatport login: {desc}")));
            }
        }
        let access = v
            .get("access_token")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let refresh = v
            .get("refresh_token")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let expires_in = v.get("expires_in").and_then(|e| e.as_i64()).unwrap_or(3600);
        if access.is_empty() {
            return Err(Error::Other(format!(
                "Beatport token response missing access_token: {v}"
            )));
        }
        self.access_token = Some(access);
        self.refresh_token = if refresh.is_empty() {
            None
        } else {
            Some(refresh)
        };
        self.expires = Some(chrono::Utc::now().timestamp() + expires_in);
        Ok(v)
    }

    /// Refresh the access token. Returns Ok on success, Err with the
    /// server payload on failure (caller should clear session + re-login,
    /// mirroring `refresh_login()` in interface.py).
    async fn refresh(&mut self) -> Result<()> {
        let refresh = self.refresh_token.clone().unwrap_or_default();
        if refresh.is_empty() {
            return Err(Error::Other("Beatport: no refresh token".to_string()));
        }
        let r = self
            .client
            .post(format!("{API_URL}auth/o/token/"))
            .form(&[
                ("client_id", CLIENT_ID),
                ("refresh_token", refresh.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatport refresh: {e}")))?;
        if !r.status().is_success() {
            let t = r.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Beatport refresh failed: {t}")));
        }
        let v: Value = r
            .json()
            .await
            .map_err(|e| Error::Other(format!("Beatport refresh JSON: {e}")))?;
        let access = v
            .get("access_token")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let new_refresh = v
            .get("refresh_token")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .or_else(|| self.refresh_token.clone())
            .unwrap_or_default();
        let expires_in = v.get("expires_in").and_then(|e| e.as_i64()).unwrap_or(3600);
        if access.is_empty() {
            return Err(Error::Other(format!(
                "Beatport refresh response missing access_token: {v}"
            )));
        }
        self.access_token = Some(access);
        self.refresh_token = if new_refresh.is_empty() {
            None
        } else {
            Some(new_refresh)
        };
        self.expires = Some(chrono::Utc::now().timestamp() + expires_in);
        Ok(())
    }

    async fn api_get(&mut self, endpoint: &str, params: HashMap<String, String>) -> Result<Value> {
        self.ensure_token().await?;
        for attempt in 0..2 {
            let url = format!("{API_URL}{endpoint}");
            let mut req = self
                .client
                .get(&url)
                .query(&params)
                .header("Referer", HOMEPAGE)
                .header("Origin", "https://www.beatport.com");
            if let Some(t) = &self.access_token {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
            let resp = req
                .send()
                .await
                .map_err(|e| Error::Other(format!("Beatport {endpoint}: {e}")))?;
            let status = resp.status();
            let text = resp
                .text()
                .await
                .map_err(|e| Error::Other(format!("Beatport body: {e}")))?;
            if status.as_u16() == 401 && attempt == 0 {
                if self.refresh_token.is_some() {
                    // Authenticated token expired - refresh once (mirrors _get()).
                    if self.refresh().await.is_err() {
                        return Err(Error::Other(format!(
                            "Beatport {endpoint}: refresh failed: {text}"
                        )));
                    }
                    continue;
                }
                // anonymous token expired – refresh once
                self.access_token = None;
                self.get_anonymous_token().await?;
                continue;
            }
            if status.as_u16() == 403 {
                return Err(map_403(&text));
            }
            if status.as_u16() == 404 {
                return Err(Error::Other(format!(
                    "Beatport {endpoint} not found: {text}"
                )));
            }
            if !status.is_success() {
                return Err(Error::Other(format!(
                    "Beatport {endpoint} HTTP {status}: {text}"
                )));
            }
            return serde_json::from_str(&text)
                .map_err(|e| Error::Other(format!("Beatport JSON: {e}")));
        }
        Err(Error::Other(format!(
            "Beatport {endpoint}: auth retry failed"
        )))
    }

    async fn get_account(&mut self) -> Result<Value> {
        self.api_get("auth/o/introspect", HashMap::new()).await
    }
    async fn get_track(&mut self, id: &str) -> Result<Value> {
        self.api_get(&format!("catalog/tracks/{id}"), HashMap::new())
            .await
    }
    async fn get_release(&mut self, id: &str) -> Result<Value> {
        self.api_get(&format!("catalog/releases/{id}"), HashMap::new())
            .await
    }
    async fn get_release_tracks(&mut self, id: &str, page: i64, per_page: i64) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("page".to_string(), page.to_string());
        p.insert("per_page".to_string(), per_page.to_string());
        self.api_get(&format!("catalog/releases/{id}/tracks"), p)
            .await
    }
    async fn get_playlist(&mut self, id: &str) -> Result<Value> {
        self.api_get(&format!("catalog/playlists/{id}"), HashMap::new())
            .await
    }
    async fn get_playlist_tracks(&mut self, id: &str, page: i64, per_page: i64) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("page".to_string(), page.to_string());
        p.insert("per_page".to_string(), per_page.to_string());
        self.api_get(&format!("catalog/playlists/{id}/tracks"), p)
            .await
    }
    async fn get_chart_tracks(&mut self, id: &str, page: i64, per_page: i64) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("page".to_string(), page.to_string());
        p.insert("per_page".to_string(), per_page.to_string());
        self.api_get(&format!("catalog/charts/{id}/tracks"), p)
            .await
    }
    async fn get_chart(&mut self, id: &str) -> Result<Value> {
        self.api_get(&format!("catalog/charts/{id}"), HashMap::new())
            .await
    }
    async fn get_artist(&mut self, id: &str) -> Result<Value> {
        self.api_get(&format!("catalog/artists/{id}"), HashMap::new())
            .await
    }
    async fn get_artist_tracks(&mut self, id: &str, page: i64, per_page: i64) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("page".to_string(), page.to_string());
        p.insert("per_page".to_string(), per_page.to_string());
        self.api_get(&format!("catalog/artists/{id}/tracks"), p)
            .await
    }
    async fn get_artist_releases(&mut self, id: &str, page: i64, per_page: i64) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("page".to_string(), page.to_string());
        p.insert("per_page".to_string(), per_page.to_string());
        self.api_get(&format!("catalog/artists/{id}/releases"), p)
            .await
    }
    async fn get_label(&mut self, id: &str) -> Result<Value> {
        self.api_get(&format!("catalog/labels/{id}"), HashMap::new())
            .await
    }
    async fn get_label_releases(&mut self, id: &str, page: i64, per_page: i64) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("page".to_string(), page.to_string());
        p.insert("per_page".to_string(), per_page.to_string());
        self.api_get(&format!("catalog/labels/{id}/releases"), p)
            .await
    }
    async fn get_label_tracks(&mut self, id: &str, page: i64, per_page: i64) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("page".to_string(), page.to_string());
        p.insert("per_page".to_string(), per_page.to_string());
        self.api_get(&format!("catalog/labels/{id}/tracks"), p)
            .await
    }
    async fn search(
        &mut self,
        query: &str,
        stype: Option<&str>,
        page: i64,
        per_page: i64,
    ) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("q".to_string(), query.to_string());
        if let Some(t) = stype {
            p.insert("type".to_string(), t.to_string());
            p.insert("page".to_string(), page.to_string());
            p.insert("per_page".to_string(), per_page.to_string());
        }
        // No type filter -> multi-category summary without pagination.
        self.api_get("catalog/search", p).await
    }
    /// 128k preview stream (.m3u8) for GUI previews - NOT the download file.
    async fn get_track_stream(&mut self, id: &str) -> Result<Value> {
        self.api_get(&format!("catalog/tracks/{id}/stream"), HashMap::new())
            .await
    }
    /// Full download file (256k AAC / FLAC depending on quality + subscription).
    async fn get_track_download(&mut self, id: &str, quality: &str) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("quality".to_string(), quality.to_string());
        self.api_get(&format!("catalog/tracks/{id}/download"), p)
            .await
    }
}

/// Prefer the `anonSession` object like beatport_api.py, fall back to any
/// nested `access_token`. Returns (token, expires_in).
fn find_anon_session(v: &Value) -> Option<(String, i64)> {
    match v {
        Value::Object(m) => {
            if let Some(anon) = m.get("anonSession").and_then(|a| a.as_object()) {
                if let Some(t) = anon.get("access_token").and_then(|t| t.as_str()) {
                    let exp = anon
                        .get("expires_in")
                        .and_then(|e| e.as_i64())
                        .unwrap_or(3600);
                    return Some((t.to_string(), exp));
                }
            }
            if let Some(t) = m.get("access_token").and_then(|t| t.as_str()) {
                let exp = m.get("expires_in").and_then(|e| e.as_i64()).unwrap_or(3600);
                return Some((t.to_string(), exp));
            }
            for (_, child) in m.iter() {
                if let Some(t) = find_anon_session(child) {
                    return Some(t);
                }
            }
            None
        }
        Value::Array(a) => {
            for child in a {
                if let Some(t) = find_anon_session(child) {
                    return Some(t);
                }
            }
            None
        }
        _ => None,
    }
}

/// Conservative 403 mapping from beatport_api.py: only explicit
/// territory phrases count as region locks; subscription/content errors
/// keep their own messages so callers don't misreport them.
fn map_403(text: &str) -> Error {
    let lower = text.to_lowercase();
    let explicit = [
        "not available in your territory",
        "not available in your region",
        "territory not allowed",
        "region not allowed",
        "territory restricted",
        "region restricted",
    ];
    if explicit.iter().any(|p| lower.contains(p)) {
        return Error::Other("Beatport: region locked".to_string());
    }
    if lower.contains("subscription") {
        return Error::Other("Beatport: subscription required".to_string());
    }
    if lower.contains("not available") && (lower.contains("download") || lower.contains("stream")) {
        return Error::Other("Beatport: content not available".to_string());
    }
    Error::Other(format!("Beatport API error (HTTP 403): {text}"))
}

#[derive(Debug)]
struct BeatportModule {
    controller: ModuleController,
    session: Mutex<BeatportSession>,
    username: String,
    password: String,
    /// Set by valid_account(): true when a Pro subscription is detected,
    /// unlocking `high`/`lossless` qualities (mirrors quality_parse).
    is_pro: Mutex<bool>,
}

impl BeatportModule {
    fn save_session(&self, access: Option<String>, refresh: Option<String>, expires: Option<i64>) {
        let tsc = &self.controller.temporary_settings_controller;
        let _ = tsc.set(
            "access_token",
            access.map(Value::String).unwrap_or(Value::Null),
            TempSettingType::Custom,
        );
        let _ = tsc.set(
            "refresh_token",
            refresh.map(Value::String).unwrap_or(Value::Null),
            TempSettingType::Custom,
        );
        let _ = tsc.set(
            "expires",
            expires.map(|e| json!(e)).unwrap_or(Value::Null),
            TempSettingType::Custom,
        );
    }

    fn clear_session(&self) {
        self.save_session(None, None, None);
    }

    /// Mirrors `valid_account()` in interface.py: requires an active
    /// subscription and enables high/lossless for Pro tiers.
    async fn valid_account(&self) -> Result<()> {
        let account = self.session.lock().await.get_account().await?;
        let sub = account
            .get("subscription")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        if sub.is_empty() {
            return Err(Error::Other(
                "Beatport: Account does not have an active 'Link' subscription".to_string(),
            ));
        }
        let low = sub.to_lowercase();
        if low == "bp_link_pro" || low.contains("pro") {
            *self.is_pro.lock().await = true;
        }
        Ok(())
    }

    /// Mirrors `refresh_login()`: refresh, else clear + re-login with
    /// stored credentials (handles expired/revoked grants, new accounts).
    async fn refresh_login(&self) -> Result<()> {
        let failed = self.session.lock().await.refresh().await.is_err();
        if failed {
            self.clear_session();
            let (u, p) = (self.username.clone(), self.password.clone());
            self.login_inner(&u, &p).await?;
            return Ok(());
        }
        let s = self.session.lock().await;
        let (a, r, e) = s.get_session();
        drop(s);
        self.save_session(a, r, e);
        Ok(())
    }

    async fn login_inner(&self, email: &str, password: &str) -> Result<()> {
        let data = self.session.lock().await.auth(email, password).await?;
        // Persist before valid_account() so a subscription failure still
        // leaves usable tokens for anonymous-grade calls.
        let s = self.session.lock().await;
        let (a, r, e) = s.get_session();
        drop(s);
        self.save_session(a, r, e);
        if data
            .get("error_description")
            .and_then(|v| v.as_str())
            .is_some()
        {
            return Err(Error::Other(format!(
                "Beatport login: {}",
                data["error_description"]
            )));
        }
        self.valid_account().await?;
        Ok(())
    }

    async fn quality_str(&self, q: Quality) -> String {
        let pro = *self.is_pro.lock().await;
        if q.contains(Quality::LOSSLESS) || q.contains(Quality::HIFI) {
            if pro {
                "lossless".to_string()
            } else {
                "medium".to_string()
            }
        } else if q.contains(Quality::HIGH) {
            if pro {
                "high".to_string()
            } else {
                "medium".to_string()
            }
        } else {
            "medium".to_string()
        }
    }

    /// Opportunistic subscription check: only for logged-in sessions, cached.
    async fn ensure_subscription_flag(&self) {
        if *self.is_pro.lock().await {
            return;
        }
        let has_refresh = self.session.lock().await.refresh_token.is_some();
        if !has_refresh {
            return;
        }
        let _ = self.valid_account().await;
    }

    fn track_title(t: &Value) -> String {
        let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("Unknown");
        let mix = t.get("mix_name").and_then(|v| v.as_str()).unwrap_or("");
        if mix.is_empty() {
            name.to_string()
        } else {
            format!("{name} ({mix})")
        }
    }

    fn track_artists(t: &Value) -> Vec<String> {
        t.get("artists")
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

    fn cover_uri(t: &Value) -> Option<String> {
        t.get("release")
            .and_then(|r| r.get("image"))
            .and_then(|i| i.get("dynamic_uri").or_else(|| i.get("uri")))
            .and_then(|u| u.as_str())
            .or_else(|| {
                t.get("image")
                    .and_then(|i| i.get("dynamic_uri").or_else(|| i.get("uri")))
                    .and_then(|u| u.as_str())
            })
            .map(|s| s.to_string())
    }

    fn artwork(url: Option<String>, size: i64) -> String {
        let Some(u) = url else { return String::new() };
        generate_artwork_url(&u, size)
    }

    /// Look up cached track data: direct id key, or nested `data` map
    /// (track_extra_kwargs cache), tolerating numeric-string keys.
    fn cached_track<'a>(data: &'a HashMap<String, Value>, track_id: &str) -> Option<&'a Value> {
        if let Some(v) = data.get(track_id) {
            if !v.is_null() {
                return Some(v);
            }
        }
        if let Some(Value::Object(map)) = data.get("data") {
            if let Some(v) = map.get(track_id) {
                return Some(v);
            }
        }
        None
    }

    fn data_str(data: &HashMap<String, Value>, key: &str) -> Option<String> {
        data.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                data.get("data")
                    .and_then(|d| d.get(key))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
    }

    fn is_chart_data(data: &HashMap<String, Value>) -> bool {
        data.get("is_chart")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || Self::data_str(data, "is_chart")
                .map(|s| s == "true")
                .unwrap_or(false)
    }
}

fn generate_artwork_url(cover_url: &str, size: i64) -> String {
    if cover_url.is_empty() {
        return String::new();
    }
    let size = size.clamp(1, 1400);
    let re = regex::Regex::new(r"\d{3,4}x\d{3,4}").unwrap();
    let templ = re.replace_all(cover_url, "{w}x{h}").to_string();
    templ
        .replace("{w}", &size.to_string())
        .replace("{h}", &size.to_string())
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for BeatportModule {
    fn name(&self) -> &str {
        "Beatport"
    }

    async fn login(&self, email: &str, password: &str) -> Result<()> {
        // Clear stale session first (mirrors interface.py subscription-fail path).
        self.clear_session();
        self.login_inner(email, password).await
    }

    fn custom_url_parse(&self, url: &str) -> Result<Option<MediaIdentification>> {
        // Library playlists have no slug: /library/playlists/6099487
        let lib_re = regex::Regex::new(r"https?://(www\.)?beatport\.com/(?P<region>[a-z]{2}/)?library/(?P<type>playlists)/(?P<id>\d+)").unwrap();
        if let Some(cap) = lib_re.captures(url) {
            let mut extra = serde_json::Map::new();
            extra.insert("is_chart".to_string(), json!(false));
            if let Some(region) = cap
                .name("region")
                .map(|m| m.as_str().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
            {
                extra.insert("region".to_string(), json!(region));
            }
            return Ok(Some(MediaIdentification {
                media_type: DownloadType::playlist,
                media_id: cap["id"].to_string(),
                extra_kwargs: extra,
            }));
        }
        // Standard pattern with slug: /track/song-name/123 (slug passed through).
        let re = regex::Regex::new(r"https?://(www\.)?beatport\.com/(?P<region>[a-z]{2}/)?(?P<type>track|release|artist|playlists|chart|label)/(?P<slug>.+)/(?P<id>\d+)").unwrap();
        let cap = re
            .captures(url)
            .ok_or_else(|| Error::Other(format!("Could not parse Beatport URL: {url}")))?;
        let ty = &cap["type"];
        let media_type = match ty {
            "track" => DownloadType::track,
            "release" => DownloadType::album,
            "artist" => DownloadType::artist,
            "playlists" | "chart" => DownloadType::playlist,
            "label" => DownloadType::label,
            _ => return Err(Error::Other(format!("Invalid Beatport URL: {url}"))),
        };
        let mut extra = serde_json::Map::new();
        // Slug passthrough for track_url reconstruction in get_track_info().
        extra.insert("slug".to_string(), json!(cap["slug"].to_string()));
        if ty != "label" {
            extra.insert("is_chart".to_string(), json!(ty == "chart"));
        }
        if let Some(region) = cap
            .name("region")
            .map(|m| m.as_str().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
        {
            extra.insert("region".to_string(), json!(region));
        }
        Ok(Some(MediaIdentification {
            media_type,
            media_id: cap["id"].to_string(),
            extra_kwargs: extra,
        }))
    }

    async fn get_track_info(
        &self,
        track_id: &str,
        quality: Quality,
        _codec: &CodecOptions,
        data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        self.ensure_subscription_flag().await;
        let cached = Self::cached_track(&data, track_id).cloned();
        let t = match cached {
            Some(v) => v,
            None => self.session.lock().await.get_track(track_id).await?,
        };
        let slug = Self::data_str(&data, "slug")
            .or_else(|| Self::data_str(&data, "track_slug"))
            .or_else(|| {
                t.get("slug")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
            });
        let release = t.get("release").cloned().unwrap_or(Value::Null);
        let artists = Self::track_artists(&t);
        let label = release
            .get("label")
            .and_then(|l| l.get("name"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                t.get("label")
                    .and_then(|l| l.get("name"))
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.to_string());
        let genre = t
            .get("genre")
            .and_then(|g| g.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let sub_genre = t
            .get("sub_genre")
            .and_then(|g| g.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mut genres = vec![];
        if let Some(g) = genre {
            genres.push(g);
        }
        if let Some(g) = sub_genre {
            genres.push(g);
        }
        let bpm = t.get("bpm").and_then(|v| v.as_u64()).map(|b| b as u32);
        let key = t
            .get("key")
            .and_then(|k| k.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let catalog = t
            .get("catalog_number")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mut extra = std::collections::BTreeMap::new();
        if let Some(b) = bpm {
            extra.insert("BPM".to_string(), b.to_string());
        }
        if let Some(k) = key {
            extra.insert("Key".to_string(), k);
        }
        if let Some(c) = catalog.clone() {
            extra.insert("Catalog number".to_string(), c);
        }

        let q = self.quality_str(quality).await;
        let (bitrate, bit_depth, codec) = match q.as_str() {
            "lossless" => (1411, Some(16), CodecFlags::FLAC),
            "high" => (256, None, CodecFlags::AAC),
            _ => (128, None, CodecFlags::AAC),
        };
        let release_year_str = t
            .get("publish_date")
            .and_then(|v| v.as_str())
            .or_else(|| release.get("release_date").and_then(|v| v.as_str()))
            .or_else(|| release.get("publish_date").and_then(|v| v.as_str()));
        let preview_url = t
            .get("sample_url")
            .and_then(|v| v.as_str())
            .or_else(|| t.get("preview_url").and_then(|v| v.as_str()))
            .or_else(|| {
                t.get("sample")
                    .and_then(|s| s.get("url"))
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.to_string());
        let mut error: Option<String> = None;
        if t.get("is_available_for_streaming")
            .and_then(|v| v.as_bool())
            == Some(false)
        {
            error = Some(format!(
                "Track '{}' is not streamable!",
                t.get("name").and_then(|v| v.as_str()).unwrap_or("Unknown")
            ));
        } else if t.get("preorder").and_then(|v| v.as_bool()) == Some(true) {
            error = Some(format!(
                "Track '{}' is not yet released!",
                t.get("name").and_then(|v| v.as_str()).unwrap_or("Unknown")
            ));
        }
        // Anonymous sessions can read metadata but not download.
        let has_creds = !self.username.is_empty() && !self.password.is_empty();
        if !has_creds && error.is_none() {
            let authed = self.session.lock().await.refresh_token.is_some();
            if !authed {
                error = Some("Beatport credentials are required for downloading. Please fill in your username and password in settings.".to_string());
            }
        }
        Ok(TrackInfo {
            id: Some(track_id.to_string()),
            name: Self::track_title(&t),
            album: release
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            album_id: release
                .get("id")
                .and_then(|v| v.as_i64())
                .map(|i| i.to_string())
                .unwrap_or_default(),
            artists,
            tags: Tags {
                track_number: t.get("number").and_then(|v| v.as_u64()).map(|n| n as u32),
                total_tracks: release
                    .get("track_count")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32),
                isrc: t
                    .get("isrc")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                upc: release
                    .get("upc")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                label,
                genres: if genres.is_empty() {
                    None
                } else {
                    Some(genres)
                },
                release_date: t
                    .get("publish_date")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                catalog_number: catalog,
                track_url: Some(format!(
                    "https://www.beatport.com/track/{}/{track_id}",
                    slug.unwrap_or_else(|| "track".to_string())
                )),
                extra_tags: extra,
                ..Default::default()
            },
            codec,
            cover_url: Self::artwork(Self::cover_uri(&t), 1400),
            release_year: release_year_str
                .and_then(|s| s.split('-').next().and_then(|y| y.parse::<i32>().ok()))
                .unwrap_or(0),
            duration: t
                .get("length_ms")
                .and_then(|v| v.as_u64())
                .map(|ms| (ms / 1000) as u32)
                .or_else(|| {
                    t.get("duration_ms")
                        .and_then(|v| v.as_u64())
                        .map(|ms| (ms / 1000) as u32)
                })
                .or_else(|| {
                    t.get("length")
                        .and_then(|v| v.as_str())
                        .and_then(|s| parse_mm_ss(s))
                }),
            explicit: None,
            artist_id: t
                .get("artists")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first())
                .and_then(|a| a.get("id"))
                .and_then(|v| v.as_i64())
                .map(|i| i.to_string()),
            bitrate: Some(bitrate),
            bit_depth,
            sample_rate: Some(44.1),
            preview_url,
            error,
            ..Default::default()
        })
    }

    async fn get_track_download(
        &self,
        track_id: &str,
        quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        if self.username.is_empty() || self.password.is_empty() {
            let authed = self.session.lock().await.refresh_token.is_some();
            if !authed {
                return Err(Error::Other("Downloading tracks requires a logged-in Beatport account. Please add your credentials in the settings.".to_string()));
            }
        }
        self.ensure_subscription_flag().await;
        let q = self.quality_str(quality).await;
        let v = match self
            .session
            .lock()
            .await
            .get_track_download(track_id, &q)
            .await
        {
            Ok(v) => v,
            Err(_) if q != "medium" => {
                self.session
                    .lock()
                    .await
                    .get_track_download(track_id, "medium")
                    .await?
            }
            Err(e) => return Err(e),
        };
        // Python reads `location`; accept `url` as an alias.
        let url = v
            .get("location")
            .and_then(|u| u.as_str())
            .or_else(|| v.get("url").and_then(|u| u.as_str()))
            .map(|s| s.to_string())
            .ok_or_else(|| Error::Other(format!("Beatport: no download URL: {v}")))?;
        let codec = if q == "lossless" {
            CodecFlags::FLAC
        } else {
            CodecFlags::AAC
        };
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(url),
            file_url_headers: {
                let mut h = serde_json::Map::new();
                h.insert("Referer".to_string(), json!(HOMEPAGE));
                h
            },
            temp_file_path: None,
            different_codec: Some(codec),
        })
    }

    /// 128k preview stream for GUI playback (mirrors get_track_stream).
    async fn get_preview_stream_url(&self, track_id: &str) -> Result<Option<String>> {
        let v = self.session.lock().await.get_track_stream(track_id).await?;
        Ok(v.get("location")
            .and_then(|u| u.as_str())
            .or_else(|| v.get("url").and_then(|u| u.as_str()))
            .or_else(|| v.get("stream_url").and_then(|u| u.as_str()))
            .map(|s| s.to_string()))
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let is_chart = Self::is_chart_data(&data);
        if is_chart {
            // Chart passed as album: chart tracks carry the track object directly.
            let c = self.session.lock().await.get_chart(album_id).await?;
            let first = self
                .session
                .lock()
                .await
                .get_chart_tracks(album_id, 1, 100)
                .await
                .unwrap_or(json!({"results": [], "count": 0}));
            let mut items = first
                .get("results")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let total = first
                .get("count")
                .and_then(|v| v.as_u64())
                .unwrap_or(items.len() as u64);
            let mut page = 2;
            while items.len() < total as usize {
                let v = self
                    .session
                    .lock()
                    .await
                    .get_chart_tracks(album_id, page, 100)
                    .await?;
                let more = v
                    .get("results")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if more.is_empty() {
                    break;
                }
                items.extend(more);
                page += 1;
            }
            let tracks: Vec<TrackRef> = items
                .iter()
                .filter_map(|t| {
                    t.get("id")
                        .and_then(|v| v.as_i64())
                        .map(|i| TrackRef::Id(i.to_string()))
                })
                .collect();
            return Ok(AlbumInfo {
                id: Some(album_id.to_string()),
                name: c
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                artist: c
                    .get("artist")
                    .and_then(|a| a.get("name"))
                    .and_then(|v| v.as_str())
                    .or_else(|| c.get("curator_name").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .to_string(),
                tracks,
                release_year: c
                    .get("publish_date")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.split('-').next())
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0),
                cover_url: c
                    .get("image")
                    .and_then(|i| i.get("dynamic_uri").or_else(|| i.get("uri")))
                    .and_then(|v| v.as_str())
                    .map(|s| generate_artwork_url(s, 1400)),
                ..Default::default()
            });
        }
        let r = self.session.lock().await.get_release(album_id).await?;
        let first = self
            .session
            .lock()
            .await
            .get_release_tracks(album_id, 1, 100)
            .await
            .unwrap_or(json!({"results": [], "count": 0}));
        let mut items = first
            .get("results")
            .and_then(|v| v.as_array())
            .or_else(|| first.get("tracks").and_then(|v| v.as_array()))
            .cloned()
            .unwrap_or_default();
        let total = first
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(items.len() as u64);
        let mut page = 2;
        while items.len() < total as usize {
            let v = self
                .session
                .lock()
                .await
                .get_release_tracks(album_id, page, 100)
                .await?;
            let more = v
                .get("results")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if more.is_empty() {
                break;
            }
            items.extend(more);
            page += 1;
        }
        let mut cache = serde_json::Map::new();
        let mut data_map = serde_json::Map::new();
        data_map.insert(album_id.to_string(), r.clone());
        for (i, t) in items.iter_mut().enumerate() {
            if let Some(obj) = t.as_object_mut() {
                obj.insert("number".to_string(), json!((i + 1) as u32));
            }
            if let Some(id) = t.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string()) {
                data_map.insert(id, t.clone());
            }
        }
        cache.insert("data".to_string(), Value::Object(data_map));
        let tracks: Vec<TrackRef> = items
            .iter()
            .filter_map(|t| {
                t.get("id")
                    .and_then(|v| v.as_i64())
                    .map(|i| TrackRef::Id(i.to_string()))
            })
            .collect();
        Ok(AlbumInfo {
            id: Some(album_id.to_string()),
            name: r
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artist: r
                .get("artists")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first())
                .and_then(|a| a.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tracks,
            release_year: r
                .get("publish_date")
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0),
            duration: Some(
                items
                    .iter()
                    .map(|t| t.get("length_ms").and_then(|v| v.as_u64()).unwrap_or(0) / 1000)
                    .sum::<u64>() as u32,
            ),
            cover_url: r
                .get("image")
                .and_then(|i| i.get("dynamic_uri").or_else(|| i.get("uri")))
                .and_then(|v| v.as_str())
                .map(|s| generate_artwork_url(s, 1400)),
            upc: r.get("upc").and_then(|v| v.as_str()).map(|s| s.to_string()),
            label: r
                .get("label")
                .and_then(|l| l.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            catalog_number: r
                .get("catalog_number")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            expected_track_count: Some(total as u32),
            track_extra_kwargs: cache,
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        playlist_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        // Charts and user playlists live on different endpoints with different
        // track wrappers (direct vs nested under `track`). Try the flagged
        // type first, fall back to the other (mirrors interface.py).
        let mut is_chart = Self::is_chart_data(&data);
        let mut playlist_data: Option<Value> = None;
        let mut first_page: Option<Value> = None;
        for attempt in 0..2 {
            let use_chart = if attempt == 0 { is_chart } else { !is_chart };
            let (pd, tp) = if use_chart {
                let pd = self.session.lock().await.get_chart(playlist_id).await.ok();
                let tp = if pd.is_some() {
                    self.session
                        .lock()
                        .await
                        .get_chart_tracks(playlist_id, 1, 100)
                        .await
                        .ok()
                } else {
                    None
                };
                (pd, tp)
            } else {
                let pd = self
                    .session
                    .lock()
                    .await
                    .get_playlist(playlist_id)
                    .await
                    .ok();
                let tp = if pd.is_some() {
                    self.session
                        .lock()
                        .await
                        .get_playlist_tracks(playlist_id, 1, 100)
                        .await
                        .ok()
                } else {
                    None
                };
                (pd, tp)
            };
            if pd.is_some() {
                playlist_data = pd;
                first_page = tp;
                is_chart = use_chart;
                break;
            }
        }
        let c = playlist_data.ok_or_else(|| {
            Error::Other(format!("Beatport: playlist/chart {playlist_id} not found"))
        })?;
        let first = first_page.unwrap_or(json!({"results": [], "count": 0}));
        let mut raw = first
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let total = first
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(raw.len() as u64);
        let mut page = 2;
        while raw.len() < total as usize {
            let v = if is_chart {
                self.session
                    .lock()
                    .await
                    .get_chart_tracks(playlist_id, page, 100)
                    .await?
            } else {
                self.session
                    .lock()
                    .await
                    .get_playlist_tracks(playlist_id, page, 100)
                    .await?
            };
            let more = v
                .get("results")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if more.is_empty() {
                break;
            }
            raw.extend(more);
            page += 1;
        }
        // Wrapper difference: playlists nest under `track`, charts are direct.
        let tracks: Vec<TrackRef> = if is_chart {
            raw.iter()
                .filter_map(|t| {
                    t.get("id")
                        .and_then(|v| v.as_i64())
                        .map(|i| TrackRef::Id(i.to_string()))
                })
                .collect()
        } else {
            raw.iter()
                .filter_map(|t| {
                    let inner = t.get("track").unwrap_or(t);
                    inner
                        .get("id")
                        .and_then(|v| v.as_i64())
                        .map(|i| TrackRef::Id(i.to_string()))
                })
                .collect()
        };
        let creator = if is_chart {
            c.get("curator_name")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    c.get("artist")
                        .and_then(|a| a.get("name"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("Beatport")
                .to_string()
        } else {
            c.get("curator_name")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    c.get("user")
                        .and_then(|u| u.get("username"))
                        .and_then(|v| v.as_str())
                })
                .or_else(|| {
                    c.get("person")
                        .and_then(|p| p.get("owner_name"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("Beatport")
                .to_string()
        };
        let creator_id = c
            .get("artist")
            .and_then(|a| a.get("id"))
            .and_then(|v| v.as_i64())
            .map(|i| i.to_string())
            .or_else(|| {
                c.get("user")
                    .and_then(|u| u.get("id"))
                    .and_then(|v| v.as_i64())
                    .map(|i| i.to_string())
            });
        let date = if is_chart {
            c.get("publish_date").and_then(|v| v.as_str())
        } else {
            c.get("created_at")
                .and_then(|v| v.as_str())
                .or_else(|| c.get("created_date").and_then(|v| v.as_str()))
        };
        let mut track_kw = serde_json::Map::new();
        track_kw.insert("is_chart".to_string(), json!(is_chart));
        Ok(PlaylistInfo {
            id: Some(playlist_id.to_string()),
            name: c
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            creator,
            creator_id,
            tracks,
            release_year: date
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0),
            cover_url: c
                .get("image")
                .and_then(|i| i.get("dynamic_uri").or_else(|| i.get("uri")))
                .and_then(|v| v.as_str())
                .map(|s| generate_artwork_url(s, 1400)),
            description: c
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            explicit: c.get("explicit").and_then(|v| v.as_bool()),
            track_extra_kwargs: track_kw,
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
        let a = self.session.lock().await.get_artist(artist_id).await?;
        let name = a
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(artist_id)
            .to_string();
        let rel = self
            .session
            .lock()
            .await
            .get_artist_releases(artist_id, 1, 100)
            .await
            .unwrap_or(json!({"results": []}));
        let items = rel
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let albums: Vec<Value> = items.iter().map(|r| json!({
            "id": r.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string()).unwrap_or_default(),
            "name": r.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            "artist": name,
        })).collect();
        Ok(ArtistInfo {
            name,
            artist_id: Some(artist_id.to_string()),
            albums,
            ..Default::default()
        })
    }

    async fn get_label_info(&self, label_id: &str, _credited: bool) -> Result<ArtistInfo> {
        let label_name = self
            .session
            .lock()
            .await
            .get_label(label_id)
            .await
            .ok()
            .and_then(|l| {
                l.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| format!("Label {label_id}"));
        let rel = self
            .session
            .lock()
            .await
            .get_label_releases(label_id, 1, 100)
            .await?;
        let items = rel
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let albums: Vec<Value> = items.iter().map(|r| json!({
            "id": r.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string()).unwrap_or_default(),
            "name": r.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        })).collect();
        Ok(ArtistInfo {
            name: label_name,
            artist_id: Some(label_id.to_string()),
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
        let t = self.session.lock().await.get_track(track_id).await?;
        Ok(CoverInfo {
            url: Self::artwork(Self::cover_uri(&t), 1400),
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
        let authed = self.session.lock().await.refresh_token.is_some();
        // Anonymous tokens 401 on categorized searches with `type`.
        let stype = if authed {
            match query_type {
                DownloadType::track => Some("tracks"),
                DownloadType::album => Some("releases"),
                DownloadType::artist => Some("artists"),
                DownloadType::playlist => Some("charts"),
                _ => None,
            }
        } else {
            None
        };
        let mut out = Vec::new();
        if query_type == DownloadType::playlist && authed {
            // Fetch both charts and user playlists (mirrors interface.py).
            let charts = self
                .session
                .lock()
                .await
                .search(query, Some("charts"), 1, limit as i64)
                .await
                .ok()
                .and_then(|v| v.get("charts").cloned())
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            let playlists = self
                .session
                .lock()
                .await
                .search(query, Some("playlists"), 1, limit as i64)
                .await
                .ok()
                .and_then(|v| v.get("playlists").cloned())
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            for item in charts
                .into_iter()
                .chain(playlists.into_iter())
                .take(limit as usize)
            {
                out.push(Self::search_item(query_type, &item));
            }
            return Ok(out);
        }
        let v = self
            .session
            .lock()
            .await
            .search(query, stype, 1, limit as i64)
            .await?;
        let items: Vec<Value> = v
            .get("results")
            .and_then(|r| r.as_array())
            .cloned()
            .or_else(|| {
                let key = match query_type {
                    DownloadType::track => "tracks",
                    DownloadType::album => "releases",
                    DownloadType::artist => "artists",
                    _ => "charts",
                };
                v.get(key)
                    .and_then(|b| b.get("results"))
                    .and_then(|r| r.as_array())
                    .cloned()
                    .or_else(|| v.get(key).and_then(|b| b.as_array()).cloned())
            })
            .unwrap_or_default();
        let mut items = items;
        if query_type == DownloadType::playlist {
            // Anonymous summary buckets: charts + user playlists.
            let extra = v
                .get("playlists")
                .and_then(|b| b.get("results"))
                .and_then(|r| r.as_array())
                .cloned()
                .or_else(|| v.get("playlists").and_then(|b| b.as_array()).cloned())
                .unwrap_or_default();
            items.extend(extra);
        }
        for item in items.into_iter().take(limit as usize) {
            out.push(Self::search_item(query_type, &item));
        }
        Ok(out)
    }
}

impl BeatportModule {
    fn search_item(query_type: DownloadType, item: &Value) -> SearchResult {
        // `str(i.get('id'))` in Python: tolerate int or string ids.
        let id = item.get("id").and_then(json_id).unwrap_or_default();
        let mut name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if query_type == DownloadType::track {
            if let Some(mix) = item.get("mix_name").and_then(|v| v.as_str()) {
                if !mix.is_empty() {
                    name = format!("{name} ({mix})");
                }
            }
        }
        // Year parsing mirrors interface.py per query type.
        let year = match query_type {
            DownloadType::track | DownloadType::album => item
                .get("publish_date")
                .and_then(|v| v.as_str())
                .filter(|s| s.len() >= 4)
                .map(|s| s[..4].to_string()),
            DownloadType::playlist => item
                .get("publish_date")
                .or_else(|| item.get("created_date"))
                .and_then(|v| v.as_str())
                .filter(|s| s.len() >= 4)
                .map(|s| s[..4].to_string()),
            DownloadType::label => item
                .get("founded")
                .or_else(|| item.get("created_at"))
                .or_else(|| item.get("founded_date"))
                .and_then(|v| v.as_str())
                .filter(|s| s.len() >= 4)
                .map(|s| s[..4].to_string()),
            _ => None,
        };
        let is_chart = item.get("publish_date").is_some()
            || item.get("person").is_some()
            || item.get("genres").is_some();
        let mut extra = serde_json::Map::new();
        if query_type == DownloadType::playlist {
            extra.insert("is_chart".to_string(), json!(is_chart));
        }
        if let Some(slug) = item.get("slug").and_then(|v| v.as_str()) {
            extra.insert("slug".to_string(), json!(slug));
        }
        let image_data = if query_type == DownloadType::track {
            item.get("release")
                .and_then(|r| r.get("image"))
                .cloned()
                .unwrap_or(Value::Null)
        } else {
            item.get("image").cloned().unwrap_or(Value::Null)
        };
        let image_url = image_data
            .get("dynamic_uri")
            .or_else(|| image_data.get("uri"))
            .and_then(|v| v.as_str())
            .map(|s| generate_artwork_url(s, 56));
        let preview_url = item
            .get("sample_url")
            .and_then(|v| v.as_str())
            .or_else(|| item.get("preview_url").and_then(|v| v.as_str()))
            .map(|s| s.to_string());
        SearchResult {
            result_id: id,
            name: Some(name),
            artists: item.get("artists").and_then(|a| a.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        a.get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            }),
            year,
            explicit: item.get("explicit").and_then(|v| v.as_bool()),
            image_url,
            preview_url,
            duration: item
                .get("length_ms")
                .and_then(|v| v.as_u64())
                .map(|ms| (ms / 1000) as u32),
            extra_kwargs: extra,
            ..Default::default()
        }
    }
}

/// Tolerate int or string ids (`str(i.get('id'))` in Python).
fn json_id(v: &Value) -> Option<String> {
    v.as_i64()
        .map(|i| i.to_string())
        .or_else(|| v.as_u64().map(|i| i.to_string()))
        .or_else(|| v.as_str().map(|s| s.to_string()))
}

fn parse_mm_ss(s: &str) -> Option<u32> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() == 2 {
        if let (Ok(m), Ok(sec)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
            return Some(m * 60 + sec);
        }
    }
    None
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
