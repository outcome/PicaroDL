//! Qobuz module - faithful port of `modules/qobuz/{interface,qobuz_api}.py`.
//!
//! Authentication: bearer token (X-User-Auth-Token). Bundle scraping is
//! implemented so users can re-login via OAuth.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::md5_hex;
use crate::registry::register;

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "Qobuz".to_string(),
        module_supported_modes: ModuleModes::download | ModuleModes::credits,
        global_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("app_id".to_string(), json!("798273057"));
            m.insert(
                "app_secret".to_string(),
                json!("abb21364945c0583309667d13ca3d93a"),
            );
            m.insert(
                "quality_format".to_string(),
                json!("{sample_rate}kHz/{bit_depth}bit"),
            );
            m
        },
        global_storage_variables: vec![],
        session_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("username".to_string(), json!(""));
            m.insert("password".to_string(), json!(""));
            m.insert("user_id".to_string(), json!(""));
            m.insert("auth_token".to_string(), json!(""));
            m.insert("use_id_token".to_string(), json!("false"));
            m
        },
        session_storage_variables: vec!["token".to_string(), "user_id".to_string()],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("qobuz".to_string()),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("track".to_string(), DownloadType::track);
            m.insert("album".to_string(), DownloadType::album);
            m.insert("playlist".to_string(), DownloadType::playlist);
            m.insert("artist".to_string(), DownloadType::artist);
            m.insert("interpreter".to_string(), DownloadType::artist);
            m.insert("label".to_string(), DownloadType::label);
            m
        },
        test_url: Some("https://open.qobuz.com/track/52151405".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(QobuzConstructor)
}

#[derive(Debug)]
struct QobuzConstructor;

impl ModuleConstructor for QobuzConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        let settings = controller.module_settings.clone();
        let auth_token = controller
            .temporary_settings_controller
            .read("token", TempSettingType::Custom)
            .ok()
            .flatten()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| {
                settings
                    .get("auth_token")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        let user_id = controller
            .temporary_settings_controller
            .read("user_id", TempSettingType::Custom)
            .ok()
            .flatten()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| {
                settings
                    .get("user_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        let app_id = settings
            .get("app_id")
            .and_then(|v| v.as_str())
            .unwrap_or("798273057")
            .to_string();
        let app_secret = settings
            .get("app_secret")
            .and_then(|v| v.as_str())
            .unwrap_or("abb21364945c0583309667d13ca3d93a")
            .to_string();
        Ok(Arc::new(QobuzModule::new(
            controller, app_id, app_secret, auth_token, user_id,
        )))
    }
}

const GUEST_APP_ID: &str = "712109809";
const GUEST_APP_SECRET: &str = "589be88e4538daea11f509d29e4a23b1";
const SECRET_LOGGED_IN: &str = "abb21364945c0583309667d13ca3d93a";

#[derive(Debug)]
struct QobuzSession {
    app_id: String,
    app_secret: String,
    auth_token: Option<String>,
    bundle_info: Mutex<Option<Value>>,
    client: reqwest::Client,
}

impl QobuzSession {
    fn new(app_id: String, app_secret: String) -> Self {
        Self {
            app_id,
            app_secret,
            auth_token: None,
            bundle_info: Mutex::new(None),
            client: picaro_utils::http::build_client(None),
        }
    }

    fn set_token(&mut self, token: Option<String>) {
        self.auth_token = token;
    }

    fn sig(&self, endpoint: &str, params: &HashMap<String, String>) -> (String, String) {
        let unix = chrono::Utc::now().timestamp().to_string();
        let secret = if self.auth_token.is_some() {
            SECRET_LOGGED_IN
        } else {
            GUEST_APP_SECRET
        };
        let mut sig_base = endpoint.replace('/', "");
        let mut keys: Vec<&String> = params.keys().filter(|k| k.as_str() != "app_id").collect();
        keys.sort();
        for k in keys {
            sig_base.push_str(k);
            if let Some(v) = params.get(k) {
                sig_base.push_str(v);
            }
        }
        sig_base.push_str(&unix);
        sig_base.push_str(secret);
        // NOTE: Qobuz signs with MD5 (`hashlib.md5` in qobuz_api.py), not SHA1.
        let sig = md5_hex(sig_base.as_bytes());
        (unix, sig)
    }

    async fn api_call(
        &self,
        endpoint: &str,
        params: HashMap<String, String>,
        signed: bool,
    ) -> Result<Value> {
        let mut params = params;
        if self.auth_token.is_none() {
            params
                .entry("app_id".to_string())
                .or_insert(GUEST_APP_ID.to_string());
        } else if !params.contains_key("app_id") {
            params.insert("app_id".to_string(), self.app_id.clone());
        }
        if signed {
            let (ts, sig) = self.sig(endpoint, &params);
            params.insert("request_ts".to_string(), ts);
            params.insert("request_sig".to_string(), sig);
        }
        let url = format!("https://www.qobuz.com/api.json/0.2/{endpoint}");
        let mut req = self.client.get(&url).query(&params);
        if let Some(token) = &self.auth_token {
            req = req.header("X-User-Auth-Token", token);
        }
        req = req.header("X-App-Id", &self.app_id);
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Qobuz API {endpoint} failed: {txt}")));
        }
        let v: Value = resp.json().await?;
        Ok(v)
    }

    async fn get_file_url(&self, track_id: &str, format_id: u32) -> Result<Value> {
        let mut params = HashMap::new();
        params.insert("track_id".to_string(), track_id.to_string());
        params.insert("format_id".to_string(), format_id.to_string());
        params.insert("intent".to_string(), "stream".to_string());
        self.api_call("track/getFileUrl", params, true).await
    }

    async fn get_sample_url(&self, track_id: &str) -> Result<Option<String>> {
        match self.get_file_url(track_id, 5).await {
            Ok(v) => Ok(v.get("url").and_then(|u| u.as_str()).map(|s| s.to_string())),
            Err(_) => Ok(None),
        }
    }

    async fn get_track(&self, track_id: &str) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("track_id".to_string(), track_id.to_string());
        self.api_call("track/get", p, true).await
    }

    async fn get_album(&self, album_id: &str) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("album_id".to_string(), album_id.to_string());
        p.insert(
            "extra".to_string(),
            "albumsFromSameArtist,focusAll".to_string(),
        );
        self.api_call("album/get", p, true).await
    }

    async fn get_playlist(&self, playlist_id: &str, limit: u32, offset: u32) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("playlist_id".to_string(), playlist_id.to_string());
        p.insert("limit".to_string(), limit.to_string());
        p.insert("offset".to_string(), offset.to_string());
        p.insert(
            "extra".to_string(),
            "tracks,subscribers,focusAll".to_string(),
        );
        self.api_call("playlist/get", p, true).await
    }

    async fn get_artist(&self, artist_id: &str) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("artist_id".to_string(), artist_id.to_string());
        p.insert(
            "extra".to_string(),
            "albums,playlists,tracks_appears_on,albums_with_last_release,focusAll".to_string(),
        );
        p.insert("limit".to_string(), "1000".to_string());
        p.insert("offset".to_string(), "0".to_string());
        self.api_call("artist/get", p, true).await
    }

    async fn get_label(&self, label_id: &str, limit: u32, offset: u32) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("label_id".to_string(), label_id.to_string());
        p.insert("extra".to_string(), "albums,focusAll".to_string());
        p.insert("limit".to_string(), limit.to_string());
        p.insert("offset".to_string(), offset.to_string());
        self.api_call("label/get", p, true).await
    }

    async fn search(&self, query_type: &str, query: &str, limit: u32) -> Result<Value> {
        let mut p = HashMap::new();
        p.insert("query".to_string(), query.to_string());
        p.insert("type".to_string(), format!("{query_type}s"));
        p.insert("limit".to_string(), limit.to_string());
        self.api_call("catalog/search", p, true).await
    }

    /// Mirror of `interface.py::_is_auth_api_error`: auth/app-id failures
    /// that warrant a guest-credential retry in `search`.
    fn is_auth_api_error(msg: &str) -> bool {
        let err = msg.to_lowercase();
        msg.contains("\"code\":401")
            || msg.contains("\"code\":400")
            || err.contains("authentication")
            || err.contains("invalid app_id")
    }

    fn credentials_required_error() -> Error {
        Error::Other(
            "Qobuz credentials are required. Please fill in username and password, or user_id and auth_token in the settings.".to_string(),
        )
    }

    /// `catalog/search` with the Python guest fallback (mirrors
    /// `interface.py::search` + `_with_guest_credentials`): on an
    /// auth/app-id failure, retry once with the web-player guest credentials
    /// applied in full (guest `X-App-Id` header + guest query `app_id` +
    /// guest signing secret). If guest search is also restricted, return an
    /// actionable credentials error instead of leaking the raw API JSON
    /// (the Python build falls back to an Apple Music proxy here, which has
    /// no Rust equivalent yet).
    async fn search_with_guest_fallback(
        &mut self,
        query_type: &str,
        query: &str,
        limit: u32,
    ) -> Result<Value> {
        match self.search(query_type, query, limit).await {
            Ok(v) => Ok(v),
            Err(e) if Self::is_auth_api_error(&e.to_string()) => {
                let (orig_app_id, orig_secret, orig_token) = (
                    self.app_id.clone(),
                    self.app_secret.clone(),
                    self.auth_token.clone(),
                );
                self.app_id = GUEST_APP_ID.to_string();
                self.app_secret = GUEST_APP_SECRET.to_string();
                self.auth_token = None;
                let retry = self.search(query_type, query, limit).await;
                self.app_id = orig_app_id;
                self.app_secret = orig_secret;
                self.auth_token = orig_token;
                match retry {
                    Ok(v) => Ok(v),
                    Err(e2) if Self::is_auth_api_error(&e2.to_string()) => {
                        Err(Self::credentials_required_error())
                    }
                    Err(e2) => Err(e2),
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn scrape_bundle(&self) -> Result<Value> {
        if let Some(b) = self.bundle_info.lock().await.clone() {
            return Ok(b);
        }
        let r = self
            .client
            .get("https://play.qobuz.com/login")
            .send()
            .await?
            .text()
            .await?;
        let re = Regex::new(r#"<script src="(/resources/[\d.]+-[a-z]\d{3}/bundle\.js)">"#).unwrap();
        let bundle_path = re
            .captures(&r)
            .ok_or_else(|| Error::Other("could not find Qobuz bundle.js URL".to_string()))?
            .get(1)
            .unwrap()
            .as_str();
        let bundle_text = self
            .client
            .get(format!("https://play.qobuz.com{bundle_path}"))
            .send()
            .await?
            .text()
            .await?;
        let re = Regex::new(
            r#"production:\{api:\{appId:"(?P<app_id>\d{9})",appSecret:"(?P<secret>\w{32})""#,
        )
        .unwrap();
        let caps = re.captures(&bundle_text);
        let (app_id, secret) = match caps {
            Some(c) => (
                c.name("app_id").unwrap().as_str().to_string(),
                c.name("secret").unwrap().as_str().to_string(),
            ),
            None => (self.app_id.clone(), self.app_secret.clone()),
        };
        // `privateKey` is what `oauth/callback` expects as `private_key`
        // (qobuz_api.py::login_with_oauth_code), distinct from the app secret.
        let private_key = Regex::new(r#"privateKey:\s*"(?P<key>[A-Za-z0-9]{6,30})""#)
            .unwrap()
            .captures(&bundle_text)
            .and_then(|c| c.name("key").map(|m| m.as_str().to_string()));
        let v = json!({ "app_id": app_id, "secret": secret, "private_key": private_key });
        *self.bundle_info.lock().await = Some(v.clone());
        Ok(v)
    }

    async fn login_email(&self, email: &str, password: &str) -> Result<String> {
        // Mirrors qobuz_api.py::login: a very long "password" is actually a
        // raw auth token — use it directly.
        if password.len() > 60 {
            return Ok(password.to_string());
        }
        let url = "https://www.qobuz.com/api.json/0.2/user/login";
        // Standard login — raw password with email.
        let mut p = HashMap::new();
        p.insert("email".to_string(), email.to_string());
        p.insert("password".to_string(), password.to_string());
        p.insert("app_id".to_string(), self.app_id.clone());
        let resp = self.client.post(url).form(&p).send().await?;
        let mut result: Option<Value> = if resp.status().is_success() {
            resp.json().await.ok()
        } else {
            None
        };
        if result
            .as_ref()
            .and_then(|v| v.get("user_auth_token"))
            .and_then(|v| v.as_str())
            .is_none()
        {
            // Fallback: MD5-hashed password with username + extra=partner.
            let mut p = HashMap::new();
            p.insert("username".to_string(), email.to_string());
            p.insert("password".to_string(), md5_hex(password.as_bytes()));
            p.insert("extra".to_string(), "partner".to_string());
            p.insert("app_id".to_string(), self.app_id.clone());
            let resp = self.client.post(url).form(&p).send().await?;
            if !resp.status().is_success() {
                return Err(Error::ModuleAuthError("Qobuz".to_string()));
            }
            result = resp.json().await.ok();
        }
        let result = result.ok_or_else(|| Error::ModuleAuthError("Qobuz".to_string()))?;
        let token = result
            .get("user_auth_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::ModuleAuthError("Qobuz".to_string()))?
            .to_string();
        // Free accounts are not eligible for downloading.
        if result
            .get("user")
            .and_then(|u| u.get("credential"))
            .and_then(|c| c.get("parameters"))
            .is_none()
        {
            return Err(Error::Other(
                "Qobuz: Free accounts are not eligible for downloading".to_string(),
            ));
        }
        Ok(token)
    }

    /// Exchange an OAuth `code` for a session token, mirroring
    /// `qobuz_api.py::login_with_oauth_code` (uses the scraped `privateKey`,
    /// not the app secret, and finalizes with a partner login).
    /// Returns the finalized partner-login JSON (`user_auth_token` + `user`).
    async fn login_with_oauth_code(&mut self, code: &str) -> Result<Value> {
        let bundle = self.scrape_bundle().await?;
        if let Some(app_id) = bundle.get("app_id").and_then(|v| v.as_str()) {
            if app_id != self.app_id {
                self.app_id = app_id.to_string();
            }
        }
        let private_key = bundle
            .get("private_key")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.app_secret)
            .to_string();
        let mut p = HashMap::new();
        p.insert("code".to_string(), code.to_string());
        p.insert("private_key".to_string(), private_key);
        p.insert("app_id".to_string(), self.app_id.clone());
        let url = "https://www.qobuz.com/api.json/0.2/oauth/callback";
        let resp = self.client.get(url).query(&p).send().await?;
        if !resp.status().is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Qobuz OAuth callback failed: {txt}")));
        }
        let v: Value = resp.json().await?;
        let token = v
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::ModuleAuthError("Qobuz".to_string()))?
            .to_string();
        self.auth_token = Some(token);
        // Finalize login (partner login) — required for the token to be fully
        // activated for library access.
        let url = "https://www.qobuz.com/api.json/0.2/user/login";
        let resp = self
            .client
            .post(url)
            .header("Content-Type", "text/plain;charset=UTF-8")
            .body("extra=partner")
            .send()
            .await?;
        if !resp.status().is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(Error::Other(format!("Qobuz partner login failed: {txt}")));
        }
        let info: Value = resp.json().await?;
        // Prefer the user_auth_token from the finalized login when present.
        if let Some(tok) = info.get("user_auth_token").and_then(|v| v.as_str()) {
            self.auth_token = Some(tok.to_string());
        }
        Ok(info)
    }

    /// Refresh `app_id`/`app_secret` from the live web-player bundle (used to
    /// retry email login when the baked-in credentials have rotted).
    async fn refresh_bundle_credentials(&mut self) -> Result<()> {
        let bundle = self.scrape_bundle().await?;
        if let Some(app_id) = bundle.get("app_id").and_then(|v| v.as_str()) {
            self.app_id = app_id.to_string();
        }
        if let Some(secret) = bundle.get("secret").and_then(|v| v.as_str()) {
            self.app_secret = secret.to_string();
        }
        Ok(())
    }

    /// `track/getFileUrl` with quality fallback: if the requested `format_id`
    /// is unavailable, walk down the ladder (27 -> 7 -> 6 -> 5) before erroring.
    async fn get_file_url_with_fallback(&self, track_id: &str, format_id: u32) -> Result<Value> {
        const LADDER: [u32; 4] = [27, 7, 6, 5];
        let mut candidates = vec![format_id];
        for f in LADDER {
            if f < format_id && !candidates.contains(&f) {
                candidates.push(f);
            }
        }
        let mut last_err: Option<Error> = None;
        for f in candidates {
            match self.get_file_url(track_id, f).await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| Error::Other(format!("No stream available for track {track_id}"))))
    }
}

#[derive(Debug)]
struct QobuzModule {
    controller: ModuleController,
    session: Mutex<QobuzSession>,
    quality_parse: HashMap<Quality, u32>,
}

impl QobuzModule {
    fn new(
        controller: ModuleController,
        app_id: String,
        app_secret: String,
        auth_token: String,
        _user_id: String,
    ) -> Self {
        let mut session = QobuzSession::new(app_id, app_secret);
        if !auth_token.is_empty() {
            session.set_token(Some(auth_token));
        }
        let mut quality_parse = HashMap::new();
        quality_parse.insert(Quality::MINIMUM, 5u32);
        quality_parse.insert(Quality::LOW, 5);
        quality_parse.insert(Quality::MEDIUM, 5);
        quality_parse.insert(Quality::HIGH, 5);
        quality_parse.insert(Quality::LOSSLESS, 6);
        quality_parse.insert(Quality::HIFI, 27);
        quality_parse.insert(Quality::ATMOS, 27);
        Self {
            controller,
            session: Mutex::new(session),
            quality_parse,
        }
    }

    async fn ensure_credentials(&self) -> Result<()> {
        if self.session.lock().await.auth_token.is_some() {
            return Ok(());
        }
        let settings = &self.controller.module_settings;
        let user_id = settings
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let auth_token = settings
            .get("auth_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Priority 1: ID/Token (previously saved OAuth or manual token).
        if !user_id.is_empty() && !auth_token.is_empty() {
            self.session.lock().await.set_token(Some(auth_token));
            return Ok(());
        }
        // Priority 2: email/password (settings use `username`; accept `email` too).
        let email = settings
            .get("username")
            .or_else(|| settings.get("email"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let password = settings
            .get("password")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !email.is_empty() && !password.is_empty() {
            // Direct email login first...
            let attempt = self
                .session
                .lock()
                .await
                .login_email(&email, &password)
                .await;
            match attempt {
                Ok(token) => {
                    self.session.lock().await.set_token(Some(token.clone()));
                    let _ = self.controller.temporary_settings_controller.set(
                        "token",
                        Value::String(token),
                        TempSettingType::Custom,
                    );
                    return Ok(());
                }
                Err(_) => {
                    // ...then refresh app credentials from the live bundle
                    // (scrape_bundle) and retry once before giving up to guest mode.
                    let mut sess = self.session.lock().await;
                    if sess.refresh_bundle_credentials().await.is_ok() {
                        if let Ok(token) = sess.login_email(&email, &password).await {
                            sess.set_token(Some(token.clone()));
                            drop(sess);
                            let _ = self.controller.temporary_settings_controller.set(
                                "token",
                                Value::String(token),
                                TempSettingType::Custom,
                            );
                            return Ok(());
                        }
                    }
                }
            }
        }
        // Guest mode: metadata-only calls still work; downloads will fail later.
        Ok(())
    }

    /// Ordered display artist names: main performer plus any
    /// MainArtist/FeaturedArtist/Artist credits from `performers`
    /// (mirrors `_qobuz_display_artist_names` in interface.py).
    fn display_artists(track_data: &Value, album_data: &Value) -> Vec<String> {
        let main_name = track_data
            .get("performer")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                album_data
                    .get("artist")
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("Unknown Artist")
            .to_string();
        let mut artists = vec![main_name.clone()];
        if let Some(perf) = track_data.get("performers").and_then(|v| v.as_str()) {
            for credit in perf.split(" - ") {
                let parts: Vec<&str> = credit.split(", ").collect();
                if parts.len() < 2 {
                    continue;
                }
                let name = parts[0];
                let mut roles: Vec<&str> = parts[1..]
                    .iter()
                    .map(|r| match *r {
                        "Lyricists" | "Vocals" => "Lyricist",
                        "Composers" => "Composer",
                        "Producers" => "Producer",
                        other => other,
                    })
                    .collect();
                for contributor in ["MainArtist", "FeaturedArtist", "Artist"] {
                    if roles.contains(&contributor) && !artists.iter().any(|a| a == name) {
                        artists.push(name.to_string());
                    }
                    roles.retain(|&r| r != contributor);
                }
            }
        }
        if let Some(first) = artists.first_mut() {
            *first = main_name;
        }
        artists
    }

    /// Qobuz ids arrive as ints; stringify either shape (mirrors `str(...)`).
    fn json_id_to_string(v: Option<&Value>) -> String {
        match v {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        }
    }

    /// Mirrors `interface.py::_get_year`: unix timestamps -> %Y, date strings
    /// -> text before the first `-`.
    fn parse_year(v: Option<&Value>) -> i32 {
        match v {
            None => 0,
            Some(Value::Number(n)) => {
                let secs = n
                    .as_i64()
                    .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64);
                chrono::DateTime::from_timestamp(secs, 0)
                    .map(|d| d.format("%Y").to_string().parse::<i32>().unwrap_or(0))
                    .unwrap_or(0)
            }
            Some(Value::String(s)) => s
                .split('-')
                .next()
                .and_then(|y| y.parse::<i32>().ok())
                .unwrap_or(0),
            _ => 0,
        }
    }

    fn pick_cover_url(album_data: &Value) -> Option<String> {
        // Mirrors interface.py: `(large or '').split('_')[0] + '_org.jpg'`.
        let large = album_data.get("image")?.get("large")?.as_str()?;
        let stem = large.split('_').next()?;
        Some(format!("{stem}_org.jpg"))
    }

    /// Complete an OAuth login from a `code` obtained via the Qobuz OAuth
    /// redirect (mirrors `interface.py::login_with_oauth_code`): exchanges the
    /// code via `scrape_bundle` + `login_with_oauth_code` and persists the
    /// resulting token/user_id to session storage.
    /// NOTE: `module_settings` is an owned snapshot in Rust, so unlike Python
    /// we cannot write back `auth_token`/`user_id` or clear `username`/
    /// `password` there — persistence goes to the temporary settings store.
    pub async fn login_with_oauth_code(&self, code: &str) -> Result<()> {
        let info = self
            .session
            .lock()
            .await
            .login_with_oauth_code(code)
            .await?;
        let token = info
            .get("user_auth_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                self.session
                    .try_lock()
                    .ok()
                    .and_then(|s| s.auth_token.clone())
            })
            .ok_or_else(|| Error::ModuleAuthError("Qobuz".to_string()))?;
        let user_id = info
            .get("user")
            .and_then(|u| u.get("id"))
            .map(|v| match v {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        let _ = self.controller.temporary_settings_controller.set(
            "token",
            Value::String(token),
            TempSettingType::Custom,
        );
        if !user_id.is_empty() {
            let _ = self.controller.temporary_settings_controller.set(
                "user_id",
                Value::String(user_id),
                TempSettingType::Custom,
            );
        }
        Ok(())
    }
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for QobuzModule {
    fn name(&self) -> &str {
        "Qobuz"
    }

    fn is_authenticated(&self) -> bool {
        self.session
            .try_lock()
            .map(|s| s.auth_token.is_some())
            .unwrap_or(false)
    }

    async fn ensure_can_download(&self) -> Result<()> {
        self.ensure_credentials().await
    }

    async fn login(&self, email: &str, password: &str) -> Result<()> {
        // ID/Token mode (previously saved OAuth or manual token) takes
        // priority over email/password, mirroring interface.py::login.
        let settings = &self.controller.module_settings;
        let user_id = settings
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let auth_token = settings
            .get("auth_token")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !user_id.is_empty() && !auth_token.is_empty() {
            self.session
                .lock()
                .await
                .set_token(Some(auth_token.to_string()));
            let _ = self.controller.temporary_settings_controller.set(
                "token",
                Value::String(auth_token.to_string()),
                TempSettingType::Custom,
            );
            return Ok(());
        }
        // Email/password mode: try direct login, then refresh app credentials
        // from the live bundle (scrape_bundle) and retry once. OAuth-code
        // logins go through `login_with_oauth_code`.
        let attempt = self.session.lock().await.login_email(email, password).await;
        let token = match attempt {
            Ok(t) => t,
            Err(_) => {
                let mut sess = self.session.lock().await;
                sess.refresh_bundle_credentials().await?;
                sess.login_email(email, password).await?
            }
        };
        self.session.lock().await.set_token(Some(token.clone()));
        let _ = self.controller.temporary_settings_controller.set(
            "token",
            Value::String(token),
            TempSettingType::Custom,
        );
        Ok(())
    }

    async fn logout(&self) -> Result<()> {
        self.session.lock().await.set_token(None);
        Ok(())
    }

    async fn get_track_info(
        &self,
        track_id: &str,
        quality: Quality,
        _codec: &CodecOptions,
        data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        self.ensure_credentials().await?;
        let cached = data.get(track_id).cloned();
        let track_data = if let Some(v) = cached {
            v
        } else {
            self.session.lock().await.get_track(track_id).await?
        };
        Self::build_track_info(
            &self.session,
            &self.quality_parse,
            &self.controller,
            track_id,
            quality,
            track_data,
            &self.controller.picaro_options,
        )
        .await
    }

    async fn get_track_download(
        &self,
        track_id: &str,
        quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        self.ensure_credentials().await?;
        let quality_id = self.quality_parse.get(&quality).copied().unwrap_or(27);
        // Quality fallback: walk down 27 -> 7 -> 6 -> 5 before erroring.
        let v = self
            .session
            .lock()
            .await
            .get_file_url_with_fallback(track_id, quality_id)
            .await?;
        let url = v
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Other(format!("No download URL for track {track_id}")))?
            .to_string();
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
        data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let album_data = if let Some(v) = data.get(album_id) {
            v.clone()
        } else {
            self.session.lock().await.get_album(album_id).await?
        };
        let name = album_data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let artist = album_data
            .get("artist")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Artist")
            .to_string();
        let artist_id = album_data
            .get("artist")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_i64())
            .map(|i| i.to_string())
            .unwrap_or_default();
        let tracks_value = album_data
            .get("tracks")
            .and_then(|v| v.get("items"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let track_ids: Vec<TrackRef> = tracks_value
            .iter()
            .filter_map(|t| {
                t.get("id")
                    .and_then(|i| i.as_i64())
                    .map(|i| TrackRef::Id(i.to_string()))
            })
            .collect();
        let release_year = album_data
            .get("release_date_original")
            .and_then(|v| v.as_str())
            .and_then(|s| s.split('-').next())
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);
        let cover_url = Self::pick_cover_url(&album_data).unwrap_or_default();
        let quality_tier = self
            .quality_parse
            .get(&self.controller.picaro_options.quality_tier)
            .copied()
            .unwrap_or(6);
        let quality = if quality_tier == 27
            && album_data
                .get("hires_streamable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        {
            "HI-RES"
        } else {
            "CD"
        };
        Ok(AlbumInfo {
            name,
            artist,
            artist_id: Some(artist_id),
            tracks: track_ids,
            release_year,
            cover_url: Some(cover_url),
            quality: Some(quality.to_string()),
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        let p = self
            .session
            .lock()
            .await
            .get_playlist(playlist_id, 500, 0)
            .await?;
        let name = p
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let creator = p
            .get("owner")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let tracks = p
            .get("tracks")
            .and_then(|v| v.get("items"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let track_ids: Vec<TrackRef> = tracks
            .iter()
            .filter_map(|t| {
                t.get("id")
                    .and_then(|i| i.as_i64())
                    .map(|i| TrackRef::Id(i.to_string()))
            })
            .collect();
        Ok(PlaylistInfo {
            name,
            creator,
            tracks: track_ids,
            release_year: 0,
            id: Some(playlist_id.to_string()),
            creator_id: p
                .get("owner")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_i64())
                .map(|i| i.to_string()),
            ..Default::default()
        })
    }

    async fn get_artist_info(
        &self,
        artist_id: &str,
        _get_credited_albums: bool,
        _artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        let a = self.session.lock().await.get_artist(artist_id).await?;
        let name = a
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let albums = a
            .get("albums")
            .and_then(|v| v.get("items"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let albums: Vec<Value> = albums
            .iter()
            .filter_map(|alb| {
                let id = alb.get("id").and_then(|v| v.as_i64())?.to_string();
                let title = alb
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string();
                let artist = alb
                    .get("artist")
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&name)
                    .to_string();
                let year = alb
                    .get("release_date_original")
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
            name,
            artist_id: Some(artist_id.to_string()),
            albums,
            ..Default::default()
        })
    }

    async fn get_label_info(
        &self,
        label_id: &str,
        _get_credited_albums: bool,
    ) -> Result<ArtistInfo> {
        self.ensure_credentials().await?;
        let l = self
            .session
            .lock()
            .await
            .get_label(label_id, 500, 0)
            .await?;
        let name = l
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Label")
            .to_string();
        let albums_raw = l
            .get("albums")
            .and_then(|v| v.get("items"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut albums_out: Vec<Value> = Vec::new();
        for album in &albums_raw {
            if !album.is_object() {
                albums_out.push(Value::String(album.to_string()));
                continue;
            }
            let album_id = Self::json_id_to_string(album.get("id"));
            let mut album_name = album
                .get("name")
                .or_else(|| album.get("title"))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Album")
                .to_string();
            if let Some(ver) = album.get("version").and_then(|v| v.as_str()) {
                album_name.push_str(&format!(" ({ver})"));
            }
            let artist_name = album
                .get("artist")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| name.clone());
            let release_year = Self::parse_year(
                album
                    .get("release_date_original")
                    .or_else(|| album.get("released_at"))
                    .or_else(|| album.get("release_date")),
            );
            let cover_url = album
                .get("image")
                .and_then(|i| {
                    i.get("small")
                        .or_else(|| i.get("thumbnail"))
                        .or_else(|| i.get("large"))
                })
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let duration = album.get("duration").and_then(|v| v.as_u64());
            // Quality / sampling info (matches album search "Additional" column).
            let mut additional_parts: Vec<String> = Vec::new();
            if let Some(tc) = album.get("tracks_count").and_then(|v| v.as_u64()) {
                additional_parts.push(if tc == 1 {
                    "1 track".to_string()
                } else {
                    format!("{tc} tracks")
                });
            }
            if let Some(sr) = album.get("maximum_sampling_rate").and_then(|v| v.as_f64()) {
                match album.get("maximum_bit_depth").and_then(|v| v.as_u64()) {
                    Some(bd) => {
                        if sr == 44.1 && (bd == 16 || bd == 24) {
                            // CD/Enhanced baseline: show nothing.
                        } else {
                            let is_hi_res = (bd == 24 && sr >= 88.2) || bd > 24;
                            if is_hi_res {
                                additional_parts.push("🅷 HI-RES".to_string());
                            }
                            additional_parts.push(format!("{sr}kHz/{bd}bit"));
                        }
                    }
                    None => {
                        if sr > 44.1 {
                            additional_parts.push("🅷 HI-RES".to_string());
                        }
                        additional_parts.push(format!("{sr}kHz"));
                    }
                }
            }
            albums_out.push(json!({
                "id": album_id,
                "name": album_name,
                "artist": artist_name,
                "release_year": if release_year == 0 { Value::Null } else { json!(release_year) },
                "cover_url": cover_url,
                "duration": duration,
                "additional": if additional_parts.is_empty() { Value::Null } else { json!(additional_parts) },
            }));
        }
        // Fallback: keep old behaviour (IDs only) when nothing parsed.
        if albums_out.is_empty() {
            albums_out = albums_raw
                .iter()
                .filter_map(|a| {
                    a.get("id")
                        .map(|id| Value::String(Self::json_id_to_string(Some(id))))
                })
                .filter(|s| !s.as_str().unwrap_or("").is_empty())
                .collect();
        }
        Ok(ArtistInfo {
            name,
            artist_id: Some(label_id.to_string()),
            albums: albums_out,
            ..Default::default()
        })
    }

    async fn get_track_credits(
        &self,
        track_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<Vec<CreditsInfo>> {
        let cached = data.get(track_id).cloned();
        let track_data = if let Some(v) = cached {
            v
        } else {
            self.session.lock().await.get_track(track_id).await?
        };
        let mut credits: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        let role_map: HashMap<&str, &str> = [
            ("Lyricists", "Lyricist"),
            ("Vocals", "Lyricist"),
            ("Composers", "Composer"),
            ("Producers", "Producer"),
        ]
        .iter()
        .map(|(a, b)| (*a, *b))
        .collect();
        if let Some(perf) = track_data.get("performers").and_then(|v| v.as_str()) {
            for credit in perf.split(" - ") {
                let parts: Vec<&str> = credit.split(", ").collect();
                if parts.len() < 2 {
                    continue;
                }
                let name = parts[0];
                for role in &parts[1..] {
                    let normalised = role_map.get(role).copied().unwrap_or(role);
                    credits
                        .entry(normalised.to_string())
                        .or_default()
                        .push(name.to_string());
                }
            }
        }
        Ok(credits
            .into_iter()
            .map(|(k, v)| CreditsInfo {
                credit_type: k,
                names: v,
            })
            .collect())
    }

    async fn get_track_cover(
        &self,
        _track_id: &str,
        _cover: &CoverOptions,
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        // We don't have per-track cover URL in the Qobuz API; the album cover
        // is what's embedded into the file. The downloader fetches the album
        // cover separately and the tagger uses that.
        Err(Error::Other("not implemented for Qobuz".into()))
    }

    async fn get_track_lyrics(
        &self,
        _track_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<LyricsInfo> {
        Ok(LyricsInfo::default())
    }

    async fn search(
        &self,
        query_type: DownloadType,
        query: &str,
        track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let qt = match query_type {
            DownloadType::track => "track",
            DownloadType::album => "album",
            DownloadType::artist => "artist",
            DownloadType::playlist => "playlist",
            DownloadType::label => "label",
            _ => "track",
        };
        let mut results: Value = if let Some(ti) = track_info {
            if let Some(isrc) = ti.tags.isrc.as_ref() {
                self.session
                    .lock()
                    .await
                    .search_with_guest_fallback(qt, isrc, limit)
                    .await
                    .unwrap_or(Value::Null)
            } else {
                self.session
                    .lock()
                    .await
                    .search_with_guest_fallback(qt, query, limit)
                    .await?
            }
        } else {
            self.session
                .lock()
                .await
                .search_with_guest_fallback(qt, query, limit)
                .await?
        };
        let key = format!("{qt}s");
        let items = results
            .as_object_mut()
            .and_then(|o| o.get_mut(&key))
            .and_then(|v| v.get_mut("items"))
            .and_then(|v| v.as_array_mut())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for item in items {
            let id = item
                .get("id")
                .and_then(|v| v.as_i64())
                .map(|i| i.to_string());
            let name = item
                .get("title")
                .or_else(|| item.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let artists = item
                .get("artist")
                .or_else(|| item.get("performer"))
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| vec![s.to_string()]);
            out.push(SearchResult {
                result_id: id.unwrap_or_default(),
                name,
                artists,
                duration: item
                    .get("duration")
                    .and_then(|v| v.as_u64())
                    .map(|i| i as u32),
                explicit: item.get("parental_warning").and_then(|v| v.as_bool()),
                year: item
                    .get("release_date_original")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.split('-').next())
                    .map(|s| s.to_string()),
                image_url: item
                    .get("image")
                    .and_then(|i| i.get("small"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                ..Default::default()
            });
        }
        Ok(out)
    }

    async fn get_preview_stream_url(&self, track_id: &str) -> Result<Option<String>> {
        Ok(self.session.lock().await.get_sample_url(track_id).await?)
    }
}

impl QobuzModule {
    async fn build_track_info(
        session: &Mutex<QobuzSession>,
        quality_parse: &HashMap<Quality, u32>,
        _controller: &ModuleController,
        track_id: &str,
        quality: Quality,
        track_data: Value,
        _opts: &PicaroOptions,
    ) -> Result<TrackInfo> {
        let album_data = track_data
            .get("album")
            .cloned()
            .unwrap_or_else(|| track_data.clone());
        // Track/album titles include work + version tags (mirrors interface.py).
        let title = track_data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim_end()
            .to_string();
        let mut track_name = String::new();
        if let Some(work) = track_data.get("work").and_then(|v| v.as_str()) {
            if !work.is_empty() {
                track_name.push_str(work);
                track_name.push_str(" - ");
            }
        }
        track_name.push_str(&title);
        if let Some(ver) = track_data.get("version").and_then(|v| v.as_str()) {
            track_name.push_str(&format!(" ({ver})"));
        }
        let mut album_name = album_data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Album")
            .trim_end()
            .to_string();
        if let Some(ver) = album_data.get("version").and_then(|v| v.as_str()) {
            album_name.push_str(&format!(" ({ver})"));
        }
        let artists = Self::display_artists(&track_data, &album_data);
        let main_artist_id =
            Self::json_id_to_string(track_data.get("performer").and_then(|v| v.get("id")));
        let release_year = Self::parse_year(album_data.get("release_date_original"));
        let cover_url = Self::pick_cover_url(&album_data).unwrap_or_default();
        let duration = track_data
            .get("duration")
            .and_then(|v| v.as_u64())
            .map(|i| i as u32);
        let tags = Tags {
            album_artist: Some(
                album_data
                    .get("artist")
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            composer: track_data
                .get("composer")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            release_date: album_data
                .get("release_date_original")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            track_number: track_data
                .get("track_number")
                .and_then(|v| v.as_u64())
                .map(|i| i as u32),
            total_tracks: album_data
                .get("tracks_count")
                .and_then(|v| v.as_u64())
                .map(|i| i as u32),
            disc_number: track_data
                .get("media_number")
                .and_then(|v| v.as_u64())
                .map(|i| i as u32),
            total_discs: album_data
                .get("media_count")
                .and_then(|v| v.as_u64())
                .map(|i| i as u32),
            isrc: track_data
                .get("isrc")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            upc: album_data
                .get("upc")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            label: album_data
                .get("label")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            copyright: album_data
                .get("copyright")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            genres: Some(
                album_data
                    .get("genre")
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_default(),
            ),
            track_url: Some(format!("https://open.qobuz.com/track/{track_id}")),
            ..Default::default()
        };
        // Stream info with quality fallback (requested -> lower format_ids).
        // Mirrors interface.py, including the 401-on-MP3 dummy fallback.
        let requested = quality_parse.get(&quality).copied().unwrap_or(6);
        let stream = session
            .lock()
            .await
            .get_file_url_with_fallback(track_id, requested)
            .await;
        let stream_data = match stream {
            Ok(v) => Some(v),
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                let is_401 =
                    msg.contains("\"code\":401") || msg.contains("authentication is required");
                if is_401 && requested == 5 {
                    Some(
                        json!({"bit_depth": 16, "sampling_rate": 44.1, "format_id": 5, "url": Value::Null}),
                    )
                } else {
                    return Err(e);
                }
            }
        };
        // bit_depth/sample_rate come from the stream when present, falling back
        // to the track maximums (interface.py uses stream_data only, but the
        // maximums keep guest/partial responses useful).
        let bit_depth = stream_data
            .as_ref()
            .and_then(|v| v.get("bit_depth"))
            .and_then(|v| v.as_u64())
            .map(|i| i as u32)
            .or_else(|| {
                track_data
                    .get("maximum_bit_depth")
                    .and_then(|v| v.as_u64())
                    .map(|i| i as u32)
            })
            .or(Some(16));
        let sample_rate = stream_data
            .as_ref()
            .and_then(|v| v.get("sampling_rate"))
            .and_then(|v| v.as_f64())
            .map(|f| f as f32)
            .or_else(|| {
                track_data
                    .get("maximum_sampling_rate")
                    .and_then(|v| v.as_f64())
                    .map(|f| f as f32)
            })
            .or(Some(44.1));
        let format_id = stream_data
            .as_ref()
            .and_then(|v| v.get("format_id"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let codec = match format_id {
            6 | 7 | 27 => CodecFlags::FLAC,
            0 => CodecFlags::NONE,
            _ => CodecFlags::MP3,
        };
        let bitrate = match codec {
            c if c == CodecFlags::FLAC => Some(
                ((sample_rate.unwrap_or(44.1) as f64 * 1000.0 * bit_depth.unwrap_or(16) as f64 * 2.0)
                    // (sr * 1000 * bd * 2) // 1000, mirroring interface.py
                    / 1000.0) as u32,
            ),
            c if c == CodecFlags::MP3 => Some(320),
            _ => None,
        };
        let stream_url = stream_data
            .as_ref()
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mut download_extra_kwargs = serde_json::Map::new();
        download_extra_kwargs.insert(
            "url_or_track_id".to_string(),
            stream_url.map(Value::String).unwrap_or(Value::Null),
        );
        let mut credits_extra_kwargs = serde_json::Map::new();
        credits_extra_kwargs.insert("data".to_string(), json!({ track_id: track_data }));
        // Guest-mode preview URL (interface.py only resolves these for guests).
        let preview_url = if session.lock().await.auth_token.is_none() {
            session
                .lock()
                .await
                .get_sample_url(track_id)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let error = if !track_data
            .get("streamable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            Some(format!("Track \"{title}\" is not streamable!"))
        } else {
            None
        };
        Ok(TrackInfo {
            name: track_name,
            album: album_name,
            album_id: {
                let id = Self::json_id_to_string(album_data.get("id"));
                if id.is_empty() {
                    // Fall back to numeric-only extraction for odd payloads.
                    album_data
                        .get("id")
                        .and_then(|v| v.as_i64())
                        .map(|i| i.to_string())
                        .unwrap_or_default()
                } else {
                    id
                }
            },
            artists,
            tags,
            codec,
            cover_url,
            release_year,
            duration,
            explicit: Some(
                track_data
                    .get("parental_warning")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            ),
            artist_id: if main_artist_id.is_empty() {
                None
            } else {
                Some(main_artist_id)
            },
            id: Some(track_id.to_string()),
            bit_depth,
            sample_rate,
            bitrate,
            download_extra_kwargs,
            credits_extra_kwargs,
            preview_url,
            error,
            ..Default::default()
        })
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
