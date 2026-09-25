//! Beatsource module - port of `modules/beatsource/{interface,beatsource_api}.py`.
//!
//! Same API shape as Beatport (v4 catalog), different base URL + client ID.
//! Auth flow differs: login POST first (sessionid cookie), then authorize
//! without redirect_uri (see beatsource_api.py).

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

const API_URL: &str = "https://api.beatsource.com/v4/";
const CLIENT_ID: &str = "ryZ8LuyQVPqbK2mBX2Hwt4qSMtnWuTYSqBPO92yQ";
const HOMEPAGE: &str = "https://www.beatsource.com/";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "Beatsource".to_string(),
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
        netlocation_constant: NetlocConstants::Single("beatsource".to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("track".to_string(), DownloadType::track);
            m.insert("release".to_string(), DownloadType::album);
            m.insert("chart".to_string(), DownloadType::playlist);
            m.insert("artist".to_string(), DownloadType::artist);
            m.insert("label".to_string(), DownloadType::label);
            m
        },
        test_url: Some("https://www.beatsource.com/track/sweet-caroline/11575544".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(BeatsourceConstructor)
}

#[derive(Debug)]
struct BeatsourceConstructor;

impl ModuleConstructor for BeatsourceConstructor {
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
        let mut session = BeatsourceSession::new();
        session.set_session(access, refresh, expires);
        Ok(Arc::new(BeatsourceModule {
            controller,
            session: Mutex::new(session),
            username,
            password,
            is_pro: Mutex::new(false),
        }))
    }
}

#[derive(Debug, Clone)]
struct BeatsourceSession {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires: Option<i64>,
    client: reqwest::Client,
}

impl BeatsourceSession {
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
        if self.refresh_token.is_some() {
            if self.refresh().await.is_ok() {
                return Ok(());
            }
        }
        self.get_anonymous_token().await
    }

    async fn get_anonymous_token(&mut self) -> Result<()> {
        let resp = self
            .client
            .get(HOMEPAGE)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatsource homepage: {e}")))?;
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("Beatsource homepage body: {e}")))?;
        if text.contains("Cloudflare")
            || text.contains("reCAPTCHA")
            || text.contains("Checking your browser")
        {
            return Err(Error::Other("Beatsource credentials are required because anonymous login is currently unavailable due to Cloudflare protection. Please fill in your username and password in settings.json.".to_string()));
        }
        let re = regex::Regex::new(
            r#"<script id="__NEXT_DATA__" type="application/json">(.*?)</script>"#,
        )
        .map_err(|e| Error::Other(format!("regex: {e}")))?;
        let cap = re.captures(&text).ok_or_else(|| {
            Error::Other("Could not find __NEXT_DATA__ on Beatsource homepage".to_string())
        })?;
        let data: Value = serde_json::from_str(&cap[1])
            .map_err(|e| Error::Other(format!("Beatsource NEXT_DATA JSON: {e}")))?;
        let (token, expires_in) = find_token_with_expiry(&data).ok_or_else(|| {
            Error::Other("Could not find anonymous access token on Beatsource homepage".to_string())
        })?;
        self.access_token = Some(token);
        self.refresh_token = None;
        self.expires = Some(chrono::Utc::now().timestamp() + expires_in);
        Ok(())
    }

    /// Beatsource auth flow (beatsource_api.py): login POST first to obtain
    /// the sessionid cookie, then authorize (no redirect_uri) for the code,
    /// then exchange the code for tokens.
    async fn auth(&mut self, username: &str, password: &str) -> Result<Value> {
        if username.is_empty() || password.is_empty() {
            return Err(Error::Other(
                "Beatsource credentials are missing in settings.json. Please fill in: username, password.".to_string(),
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .cookie_store(true)
            .user_agent(UA)
            .build()
            .map_err(|e| Error::Other(format!("Beatsource auth client: {e}")))?;

        // 1. login -> sessionid cookie (stored in this client's jar)
        let r = client
            .post(format!("{API_URL}auth/login/"))
            .json(&json!({"username": username, "password": password}))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatsource login: {e}")))?;
        if !r.status().is_success() {
            let text = r.text().await.unwrap_or_default();
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if v.get("username").is_some() && v.get("password").is_some() {
                    return Err(Error::Other(
                        "Beatsource credentials are missing in settings.json. Please fill in: username, password.".to_string(),
                    ));
                }
                if let Some(errs) = v
                    .get("non_field_errors")
                    .and_then(|e| e.as_array())
                    .and_then(|a| a.first())
                    .and_then(|e| e.as_str())
                {
                    return Err(Error::Other(format!("Beatsource login failed: {errs}")));
                }
            }
            return Err(Error::Other(format!("Beatsource login failed: {text}")));
        }

        // 2. authorize with session cookie -> expect 302 with code (no redirect_uri)
        let r = client
            .get(format!("{API_URL}auth/o/authorize/"))
            .query(&[("client_id", CLIENT_ID), ("response_type", "code")])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatsource authorize: {e}")))?;
        if r.status() != reqwest::StatusCode::FOUND {
            let t = r.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Beatsource authorize failed (expected 302): {t}"
            )));
        }
        let location = r
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let code = parse_code(&location).ok_or_else(|| {
            Error::Other(format!("Beatsource: no auth code in redirect: {location}"))
        })?;

        // 3. exchange code for tokens (form-encoded, no redirect_uri)
        let r = client
            .post(format!("{API_URL}auth/o/token/"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&[
                ("client_id", CLIENT_ID),
                ("code", code.as_str()),
                ("grant_type", "authorization_code"),
            ])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatsource token exchange: {e}")))?;
        if !r.status().is_success() {
            let t = r.text().await.unwrap_or_default();
            return Err(Error::Other(format!(
                "Beatsource token exchange failed: {t}"
            )));
        }
        let v: Value = r
            .json()
            .await
            .map_err(|e| Error::Other(format!("Beatsource token JSON: {e}")))?;
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
        if access.is_empty() || refresh.is_empty() {
            return Err(Error::Other(format!(
                "Beatsource token response missing fields: {v}"
            )));
        }
        self.access_token = Some(access);
        self.refresh_token = Some(refresh);
        self.expires = Some(chrono::Utc::now().timestamp() + expires_in);
        Ok(v)
    }

    async fn refresh(&mut self) -> Result<()> {
        let refresh = self.refresh_token.clone().unwrap_or_default();
        if refresh.is_empty() {
            return Err(Error::Other("Beatsource: no refresh token".to_string()));
        }
        let r = self
            .client
            .post(format!("{API_URL}auth/o/token/"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&[
                ("client_id", CLIENT_ID),
                ("refresh_token", refresh.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Beatsource refresh: {e}")))?;
        if !r.status().is_success() {
            let t = r.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Beatsource refresh failed: {t}")));
        }
        let v: Value = r
            .json()
            .await
            .map_err(|e| Error::Other(format!("Beatsource refresh JSON: {e}")))?;
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
        let expires_in = v.get("expires_in").and_then(|e| e.as_i64());
        match (access.is_empty(), expires_in) {
            (false, Some(exp)) => {
                self.access_token = Some(access);
                self.refresh_token = if new_refresh.is_empty() {
                    None
                } else {
                    Some(new_refresh)
                };
                self.expires = Some(chrono::Utc::now().timestamp() + exp);
                Ok(())
            }
            _ => Err(Error::Other(format!(
                "Beatsource refresh response missing fields: {v}"
            ))),
        }
    }

    async fn api_get(&mut self, endpoint: &str, params: HashMap<String, String>) -> Result<Value> {
        self.ensure_token().await?;
        for attempt in 0..2 {
            let url = format!("{API_URL}{endpoint}");
            let mut req = self
                .client
                .get(&url)
                .query(&params)
                .header("Referer", HOMEPAGE);
            if let Some(t) = &self.access_token {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
            let resp = req
                .send()
                .await
                .map_err(|e| Error::Other(format!("Beatsource {endpoint}: {e}")))?;
            let status = resp.status();
            let text = resp
                .text()
                .await
                .map_err(|e| Error::Other(format!("Beatsource body: {e}")))?;
            if status.as_u16() == 401 && attempt == 0 {
                if self.refresh_token.is_some() {
                    if self.refresh().await.is_err() {
                        return Err(Error::Other(format!(
                            "Beatsource {endpoint}: refresh failed: {text}"
                        )));
                    }
                    continue;
                }
                self.access_token = None;
                self.get_anonymous_token().await?;
                continue;
            }
            if status.as_u16() == 403 {
                if text.contains("Territory")
                    || text.to_lowercase().contains("territory restricted")
                {
                    return Err(Error::Other("Beatsource: region locked".to_string()));
                }
                return Err(Error::Other(format!(
                    "Beatsource {endpoint} HTTP 403: {text}"
                )));
            }
            if status.as_u16() == 404 {
                return Err(Error::Other(format!(
                    "Beatsource {endpoint} not found: {text}"
                )));
            }
            if !status.is_success() {
                return Err(Error::Other(format!(
                    "Beatsource {endpoint} HTTP {status}: {text}"
                )));
            }
            return serde_json::from_str(&text)
                .map_err(|e| Error::Other(format!("Beatsource JSON: {e}")));
        }
        Err(Error::Other(format!(
            "Beatsource {endpoint}: auth retry failed"
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
    async fn get_chart(&mut self, id: &str) -> Result<Value> {
        self.api_get(&format!("catalog/charts/{id}"), HashMap::new())
            .await
    }
    async fn get_chart_tracks(&mut self, id: &str, page: i64, per_page: i64) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("page".to_string(), page.to_string());
        p.insert("per_page".to_string(), per_page.to_string());
        self.api_get(&format!("catalog/charts/{id}/tracks"), p)
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
        self.api_get("catalog/search", p).await
    }
    /// 128k preview stream (.m3u8) for GUI previews - NOT the download file.
    async fn get_track_stream(&mut self, id: &str) -> Result<Value> {
        self.api_get(&format!("catalog/tracks/{id}/stream"), HashMap::new())
            .await
    }
    /// Full download file (quality depends on subscription).
    async fn get_track_download(&mut self, id: &str, quality: &str) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("quality".to_string(), quality.to_string());
        self.api_get(&format!("catalog/tracks/{id}/download"), p)
            .await
    }
}

fn find_token_with_expiry(v: &Value) -> Option<(String, i64)> {
    match v {
        Value::Object(m) => {
            if let Some(t) = m.get("access_token").and_then(|t| t.as_str()) {
                let exp = m.get("expires_in").and_then(|e| e.as_i64()).unwrap_or(3600);
                return Some((t.to_string(), exp));
            }
            for (_, child) in m.iter() {
                if let Some(t) = find_token_with_expiry(child) {
                    return Some(t);
                }
            }
            None
        }
        Value::Array(a) => {
            for child in a {
                if let Some(t) = find_token_with_expiry(child) {
                    return Some(t);
                }
            }
            None
        }
        _ => None,
    }
}

fn parse_code(location: &str) -> Option<String> {
    // Location looks like `seratodjlite://beatsource?code=XYZ` - parse query.
    let query = location.split('?').nth(1)?;
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some("code") {
            return it.next().map(|s| s.to_string()).filter(|s| !s.is_empty());
        }
    }
    // Fallback: raw split like the Beatport implementation.
    location
        .split("code=")
        .nth(1)
        .map(|s| s.split('&').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
}

#[derive(Debug)]
struct BeatsourceModule {
    controller: ModuleController,
    session: Mutex<BeatsourceSession>,
    username: String,
    password: String,
    /// Set by valid_account(): true when a Pro subscription is detected.
    is_pro: Mutex<bool>,
}

impl BeatsourceModule {
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

    /// Mirrors `valid_account()` in interface.py.
    async fn valid_account(&self) -> Result<()> {
        let account = self.session.lock().await.get_account().await?;
        let sub = account
            .get("subscription")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        if sub.is_empty() {
            return Err(Error::Other(
                "Beatsource: Account does not have an active 'Link' subscription".to_string(),
            ));
        }
        let low = sub.to_lowercase();
        if low == "bp_link_pro" || low.contains("pro") {
            *self.is_pro.lock().await = true;
        }
        Ok(())
    }

    async fn login_inner(&self, email: &str, password: &str) -> Result<()> {
        let data = self.session.lock().await.auth(email, password).await?;
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
                "Beatsource login: {}",
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
            .unwrap_or_else(|| {
                // Beatsource open-format tracks may list remixers instead.
                t.get("remixers")
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
            })
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
        url.map(|u| generate_artwork_url(&u, size))
            .unwrap_or_default()
    }
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

/// Tolerate int or string ids (`str(i.get('id'))` in Python).
fn json_id(v: &Value) -> Option<String> {
    v.as_i64()
        .map(|i| i.to_string())
        .or_else(|| v.as_u64().map(|i| i.to_string()))
        .or_else(|| v.as_str().map(|s| s.to_string()))
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for BeatsourceModule {
    fn name(&self) -> &str {
        "Beatsource"
    }

    async fn login(&self, email: &str, password: &str) -> Result<()> {
        self.clear_session();
        self.login_inner(email, password).await
    }

    fn custom_url_parse(&self, url: &str) -> Result<Option<MediaIdentification>> {
        // Slug-agnostic pattern capturing the final numeric ID; the slug
        // segment(s) between type and id are passed through for track_url
        // reconstruction (mirrors track_slug/artist_slug/label_slug handling).
        let re = regex::Regex::new(r"https?://(?:www\.)?beatsource\.com/(?:[a-z]{2}/)?(?P<type>track|release|artist|playlist|playlists|chart|label)(?P<rest>.*/)(?P<id>\d+)").unwrap();
        let cap = re
            .captures(url)
            .ok_or_else(|| Error::Other(format!("Could not parse Beatsource URL: {url}")))?;
        let ty = &cap["type"];
        let media_type = match ty {
            "track" => DownloadType::track,
            "release" => DownloadType::album,
            "artist" => DownloadType::artist,
            "playlist" | "playlists" | "chart" => DownloadType::playlist,
            "label" => DownloadType::label,
            _ => {
                return Err(Error::Other(format!(
                    "Unknown Beatsource media type in URL: {url}"
                )))
            }
        };
        let mut extra = serde_json::Map::new();
        // Slug passthrough: last non-id segment of the path.
        let rest = cap["rest"].trim_matches('/');
        if let Some(slug) = rest.split('/').filter(|s| !s.is_empty()).last() {
            extra.insert("slug".to_string(), json!(slug.to_string()));
        }
        if ty != "label" {
            extra.insert("is_chart".to_string(), json!(ty == "chart"));
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
            })
            .unwrap_or_else(|| "_".to_string());
        let release = t.get("release").cloned().unwrap_or(Value::Null);
        let q = self.quality_str(quality).await;
        let (bitrate, bit_depth, codec) = match q.as_str() {
            "lossless" => (1411, Some(16), CodecFlags::FLAC),
            "high" => (256, None, CodecFlags::AAC),
            _ => (128, None, CodecFlags::AAC),
        };
        let genre = t
            .get("genre")
            .and_then(|g| g.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let sub = t
            .get("sub_genre")
            .and_then(|g| g.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mut genres = vec![];
        if let Some(g) = genre {
            genres.push(g);
        }
        if let Some(g) = sub {
            genres.push(g);
        }
        let mut extra = std::collections::BTreeMap::new();
        if let Some(bpm) = t.get("bpm").and_then(|v| v.as_u64()) {
            extra.insert("BPM".to_string(), bpm.to_string());
        }
        if let Some(k) = t
            .get("key")
            .and_then(|k| k.get("name"))
            .and_then(|v| v.as_str())
        {
            extra.insert("Key".to_string(), k.to_string());
        }
        if let Some(c) = t.get("catalog_number").and_then(|v| v.as_str()) {
            extra.insert("Catalog number".to_string(), c.to_string());
        }
        let label_name = release
            .get("label")
            .and_then(|l| l.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let release_year_str = t
            .get("publish_date")
            .and_then(|v| v.as_str())
            .or_else(|| release.get("publish_date").and_then(|v| v.as_str()))
            .or_else(|| release.get("release_date").and_then(|v| v.as_str()));
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
        if error.is_none() {
            let authed = self.session.lock().await.refresh_token.is_some();
            if !authed {
                error = Some("Beatsource credentials are required for downloading. Please fill in your username and password in the settings.".to_string());
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
            artists: Self::track_artists(&t),
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
                label: if label_name.is_empty() {
                    None
                } else {
                    Some(label_name.clone())
                },
                genres: if genres.is_empty() {
                    None
                } else {
                    Some(genres)
                },
                release_date: t
                    .get("publish_date")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                catalog_number: t
                    .get("catalog_number")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                track_url: Some(format!(
                    "https://www.beatsource.com/track/{slug}/{track_id}"
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
                .map(|ms| (ms / 1000) as u32),
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
                return Err(Error::Other("Downloading tracks requires a logged-in Beatsource account. Please add your credentials in the settings.".to_string()));
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
        let url = v
            .get("location")
            .and_then(|u| u.as_str())
            .or_else(|| v.get("url").and_then(|u| u.as_str()))
            .map(|s| s.to_string())
            .ok_or_else(|| Error::Other(format!("Beatsource: no download URL: {v}")))?;
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(url),
            file_url_headers: {
                let mut h = serde_json::Map::new();
                h.insert("Referer".to_string(), json!(HOMEPAGE));
                h
            },
            temp_file_path: None,
            different_codec: Some(if q == "lossless" {
                CodecFlags::FLAC
            } else {
                CodecFlags::AAC
            }),
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
                    .get("person")
                    .and_then(|p| p.get("owner_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Beatsource")
                    .to_string(),
                tracks,
                release_year: c
                    .get("change_date")
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
        // Beatsource: try standard playlist endpoint first, fall back to
        // chart endpoint (mirrors interface.py). Wrappers differ: playlist
        // tracks nest under `track`, chart tracks are direct.
        let mut is_chart_endpoint = false;
        let (mut playlist_data, mut tracks_data) = {
            let pd = self
                .session
                .lock()
                .await
                .get_playlist(playlist_id)
                .await
                .ok();
            let td = if pd.is_some() {
                self.session
                    .lock()
                    .await
                    .get_playlist_tracks(playlist_id, 1, 100)
                    .await
                    .ok()
            } else {
                None
            };
            (pd, td)
        };
        if playlist_data.is_none() {
            let pd = self.session.lock().await.get_chart(playlist_id).await.ok();
            let td = if pd.is_some() {
                self.session
                    .lock()
                    .await
                    .get_chart_tracks(playlist_id, 1, 100)
                    .await
                    .ok()
            } else {
                None
            };
            if pd.is_some() {
                playlist_data = pd;
                tracks_data = td;
                is_chart_endpoint = true;
            }
        }
        // Explicit is_chart flag from URL parse forces the chart endpoint.
        if Self::is_chart_data(&data) && !is_chart_endpoint {
            if let Ok(pd) = self.session.lock().await.get_chart(playlist_id).await {
                if let Ok(td) = self
                    .session
                    .lock()
                    .await
                    .get_chart_tracks(playlist_id, 1, 100)
                    .await
                {
                    playlist_data = Some(pd);
                    tracks_data = Some(td);
                    is_chart_endpoint = true;
                }
            }
        }
        let pd = playlist_data.ok_or_else(|| {
            Error::Other(format!(
                "Beatsource: playlist {playlist_id} not found (tried playlist + chart endpoints)"
            ))
        })?;
        let first = tracks_data.unwrap_or(json!({"results": [], "count": 0}));
        let mut raw = first
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut total = first
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(raw.len() as u64) as usize;
        if total == 0 {
            total = raw.len();
        }
        let mut page = 2;
        while raw.len() < total {
            let v = if is_chart_endpoint {
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
        let tracks_all: Vec<Value> = if is_chart_endpoint {
            raw
        } else {
            raw.iter().filter_map(|t| t.get("track").cloned()).collect()
        };
        let mut cache = serde_json::Map::new();
        let mut data_map = serde_json::Map::new();
        for (i, track) in tracks_all.iter().enumerate() {
            let mut t = track.clone();
            if let Some(obj) = t.as_object_mut() {
                obj.insert("track_number".to_string(), json!((i + 1) as u32));
                obj.insert("total_tracks".to_string(), json!(tracks_all.len() as u32));
            }
            if let Some(id) = t.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string()) {
                data_map.insert(id, t);
            }
        }
        cache.insert("data".to_string(), Value::Object(data_map));
        let valid: Vec<&Value> = tracks_all
            .iter()
            .filter(|t| t.get("id").is_some())
            .collect();
        let duration = valid
            .iter()
            .map(|t| t.get("length_ms").and_then(|v| v.as_u64()).unwrap_or(0) / 1000)
            .sum::<u64>() as u32;
        let (creator, release_year, cover_raw) = if is_chart_endpoint {
            let creator = pd
                .get("person")
                .and_then(|p| p.get("owner_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("Beatsource")
                .to_string();
            let year = pd
                .get("change_date")
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);
            let cover = pd
                .get("image")
                .and_then(|i| i.get("dynamic_uri").or_else(|| i.get("uri")))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            (creator, year, cover)
        } else {
            let year = pd
                .get("updated_date")
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);
            let cover = pd
                .get("release_images")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|img| {
                    img.as_object()
                        .and_then(|o| o.get("dynamic_uri"))
                        .or_else(|| img.as_object().and_then(|o| o.get("uri")))
                })
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    pd.get("release_images")
                        .and_then(|v| v.as_array())
                        .and_then(|a| a.first())
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });
            ("User".to_string(), year, cover)
        };
        Ok(PlaylistInfo {
            id: Some(playlist_id.to_string()),
            name: pd
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            creator,
            tracks: valid
                .iter()
                .filter_map(|t| {
                    t.get("id")
                        .and_then(|v| v.as_i64())
                        .map(|i| TrackRef::Id(i.to_string()))
                })
                .collect(),
            release_year,
            duration: Some(duration),
            cover_url: cover_raw.map(|u| generate_artwork_url(&u, 1400)),
            track_extra_kwargs: cache,
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
        let stype = match query_type {
            DownloadType::track => "tracks",
            DownloadType::album => "releases",
            DownloadType::artist => "artists",
            DownloadType::playlist => "charts",
            _ => "tracks",
        };
        // Anonymous: general search (multi-category summary) without type filter.
        let v = if authed {
            self.session
                .lock()
                .await
                .search(query, Some(stype), 1, limit as i64)
                .await?
        } else {
            self.session
                .lock()
                .await
                .search(query, None, 1, limit as i64)
                .await?
        };
        let items: Vec<Value> = v
            .get("results")
            .and_then(|r| r.as_array())
            .cloned()
            .or_else(|| v.get(stype).and_then(|b| b.as_array()).cloned())
            .or_else(|| {
                v.get(stype)
                    .and_then(|b| b.get("results"))
                    .and_then(|r| r.as_array())
                    .cloned()
            })
            .unwrap_or_default();
        Ok(items
            .into_iter()
            .take(limit as usize)
            .map(|item| {
                let mut extra = serde_json::Map::new();
                let mut data_map = serde_json::Map::new();
                if let Some(id) = item
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .map(|i| i.to_string())
                {
                    data_map.insert(id, item.clone());
                }
                extra.insert("data".to_string(), Value::Object(data_map));
                if let Some(slug) = item.get("slug").and_then(|v| v.as_str()) {
                    let key = match query_type {
                        DownloadType::track => "track_slug",
                        DownloadType::artist => "artist_slug",
                        DownloadType::label => "label_slug",
                        _ => "slug",
                    };
                    extra.insert(key.to_string(), json!(slug));
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
                let mut name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(mix) = item.get("mix_name").and_then(|v| v.as_str()) {
                    if !mix.is_empty() {
                        name = format!("{name} ({mix})");
                    }
                }
                // Year parsing mirrors the Beatport interface per query type.
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
                SearchResult {
                    result_id: item.get("id").and_then(json_id).unwrap_or_default(),
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
            })
            .collect())
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
