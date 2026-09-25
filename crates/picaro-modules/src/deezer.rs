//! Deezer module - port of `modules/deezer/{interface,dzapi}.py`.
//!
//! Auth: ARL cookie or email+password. Supports FLAC / MP3_320 / MP3_128 / MP3_MISC.
//! Blowfish CBC stripe decryption for the legacy BF_CBC_STRIPE format.

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

use crate::md5_hex;
use crate::registry::register;

const BF_SECRET_DEFAULT: &str = "g4el58wc0zvf9na1";
const GW_LIGHT_URL: &str = "https://www.deezer.com/ajax/gw-light.php";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "Deezer".to_string(),
        module_supported_modes: ModuleModes::download
            | ModuleModes::lyrics
            | ModuleModes::covers
            | ModuleModes::credits,
        global_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("client_id".to_string(), json!("447462"));
            m.insert(
                "client_secret".to_string(),
                json!("a83bf7f38ad2f137e444727cfc3775cf"),
            );
            m.insert("bf_secret".to_string(), json!(BF_SECRET_DEFAULT));
            m
        },
        global_storage_variables: vec![],
        session_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("email".to_string(), json!(""));
            m.insert("password".to_string(), json!(""));
            m.insert("arl".to_string(), json!(""));
            m.insert("use_arl".to_string(), json!("false"));
            m
        },
        session_storage_variables: vec!["arl".to_string()],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Multi(vec!["deezer".to_string(), "dzr".to_string()]),
        url_constants: {
            let mut m = indexmap::IndexMap::new();
            m.insert("track".to_string(), DownloadType::track);
            m.insert("album".to_string(), DownloadType::album);
            m.insert("playlist".to_string(), DownloadType::playlist);
            m.insert("artist".to_string(), DownloadType::artist);
            m
        },
        test_url: Some("https://www.deezer.com/track/3135556".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(DeezerConstructor)
}

#[derive(Debug)]
struct DeezerConstructor;

impl ModuleConstructor for DeezerConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        let settings = controller.module_settings.clone();
        let arl = settings
            .get("arl")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let email = settings
            .get("email")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let password = settings
            .get("password")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let client_id = settings
            .get("client_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "447462".to_string());
        let client_secret = settings
            .get("client_secret")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "a83bf7f38ad2f137e444727cfc3775cf".to_string());
        let bf_secret = settings
            .get("bf_secret")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| BF_SECRET_DEFAULT.to_string());
        let mut session = DeezerSession::new(client_id, client_secret, bf_secret);
        if !arl.is_empty() {
            session.arl = Some(arl);
        }
        let module = DeezerModule {
            controller,
            session: Mutex::new(session),
            email,
            password,
        };
        Ok(Arc::new(module))
    }
}

// ---------------------------------------------------------------------------
// Low-level session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DeezerSession {
    client_id: String,
    client_secret: String,
    bf_secret: String,
    arl: Option<String>,
    api_token: Option<String>,
    license_token: Option<String>,
    country: Option<String>,
    language: Option<String>,
    available_formats: Vec<String>,
    renew_timestamp: u64,
    client: reqwest::Client,
}

impl DeezerSession {
    fn new(client_id: String, client_secret: String, bf_secret: String) -> Self {
        Self {
            client_id,
            client_secret,
            bf_secret,
            arl: None,
            api_token: None,
            license_token: None,
            country: None,
            language: None,
            available_formats: vec!["MP3_128".to_string()],
            renew_timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            client: picaro_utils::http::build_client_with_user_agent(
                None,
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            ),
        }
    }

    fn is_authenticated(&self) -> bool {
        self.api_token
            .as_ref()
            .map(|t| !t.is_empty())
            .unwrap_or(false)
    }

    async fn api_call(&mut self, method: &str, payload: Value) -> Result<Value> {
        use rand::Rng;
        let cid: u32 = rand::thread_rng().gen_range(0..1_000_000_000);
        let token = self.api_token.clone().unwrap_or_default();
        let url = format!(
            "{GW_LIGHT_URL}?method={method}&input=3&api_version=1.0&api_token={token}&cid={cid}"
        );
        let mut req = self.client.post(&url).json(&payload);
        if let Some(arl) = self.arl.clone() {
            req = req.header("Cookie", format!("arl={arl}"));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer {method} request failed: {e}")))?;
        let response_text = resp
            .text()
            .await
            .map_err(|e| Error::Other(format!("Deezer {method} invalid response: {e}")))?;
        let v: Value = serde_json::from_str(&response_text)
            .map_err(|e| Error::Other(format!("Deezer {method} invalid JSON: {e}")))?;
        if let Some(err) = v.get("error") {
            if !err.is_null() && !(err.as_object().map(|o| o.is_empty()).unwrap_or(false)) {
                // try to extract message
                let msg = err.to_string();
                return Err(Error::Other(format!("Deezer API {method}: {msg}")));
            }
        }
        let results = v.get("results").cloned().unwrap_or(Value::Null);
        if method == "deezer.getUserData" {
            self.api_token = results.get("checkForm").and_then(|v| v.as_str()).map(|s| {
                println!("Setting API Token: {:?}", s);
                s.to_string()
            });
            self.country = results
                .get("COUNTRY")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            self.language = results
                .get("USER")
                .and_then(|u| u.get("SETTING"))
                .and_then(|s| s.get("global"))
                .and_then(|g| g.get("language"))
                .and_then(|l| l.as_str())
                .map(|s| s.to_string());
            self.license_token = results
                .get("USER")
                .and_then(|u| u.get("OPTIONS"))
                .and_then(|o| o.get("license_token"))
                .and_then(|l| l.as_str())
                .map(|s| s.to_string());
            let mut fmts = vec!["MP3_128".to_string()];
            if let Some(opts) = results.get("USER").and_then(|u| u.get("OPTIONS")) {
                if opts
                    .get("web_hq")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    fmts.push("MP3_320".to_string());
                }
                if opts
                    .get("web_lossless")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    fmts.push("FLAC".to_string());
                }
            }
            self.available_formats = fmts;
            self.renew_timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(self.renew_timestamp);
        }
        Ok(results)
    }

    async fn login_via_arl(&mut self, arl: &str) -> Result<Value> {
        // NOTE: reqwest cookie store handles domain cookies automatically when we
        // hit deezer.com. We set the cookie via a dummy URL header instead.
        let _ = self.client.get("https://www.deezer.com").send().await;
        // Manually inject cookie by requesting with header on next call is complex
        // with cookie_store; instead rely on cookie jar via header insertion:
        // simplest: store arl and let api_call use it implicitly via session file?
        // For the Rust port we set the cookie through the client cookie mechanism
        // by calling the gw endpoint – the server reads `arl` cookie. reqwest's
        // cookie_store does not expose direct set, so we keep arl and append a
        // Cookie header manually in api_call_with_arl path below.
        self.arl = Some(arl.to_string());
        // Perform getUserData with arl cookie: rebuild client with header
        let url = format!(
            "{GW_LIGHT_URL}?method=deezer.getUserData&input=3&api_version=1.0&api_token=&cid=0"
        );
        let resp = self
            .client
            .post(&url)
            .header("Cookie", format!("arl={arl}"))
            .json(&json!({}))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer login_via_arl: {e}")))?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Deezer login JSON: {e}")))?;
        if let Some(err) = v.get("error") {
            if !err.is_null() && !(err.as_object().map(|o| o.is_empty()).unwrap_or(false)) {
                return Err(Error::Other("Invalid arl".to_string()));
            }
        }
        let results = v.get("results").cloned().unwrap_or(Value::Null);
        let user_id = results
            .get("USER")
            .and_then(|u| u.get("USER_ID"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if user_id == 0 {
            // also accept string ids
            let s = results
                .get("USER")
                .and_then(|u| u.get("USER_ID"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if s.is_empty() || s == "0" {
                return Err(Error::Other("Invalid arl".to_string()));
            }
        }
        self.api_token = results
            .get("checkForm")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        self.country = results
            .get("COUNTRY")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        self.language = results
            .get("USER")
            .and_then(|u| u.get("SETTING"))
            .and_then(|s| s.get("global"))
            .and_then(|g| g.get("language"))
            .and_then(|l| l.as_str())
            .map(|s| s.to_string());
        self.license_token = results
            .get("USER")
            .and_then(|u| u.get("OPTIONS"))
            .and_then(|o| o.get("license_token"))
            .and_then(|l| l.as_str())
            .map(|s| s.to_string());
        let mut fmts = vec!["MP3_128".to_string()];
        if let Some(opts) = results.get("USER").and_then(|u| u.get("OPTIONS")) {
            if opts
                .get("web_hq")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                fmts.push("MP3_320".to_string());
            }
            if opts
                .get("web_lossless")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                fmts.push("FLAC".to_string());
            }
        }
        self.available_formats = fmts;
        self.renew_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(self.renew_timestamp);
        Ok(results)
    }

    async fn login_via_email(&mut self, email: &str, password: &str) -> Result<String> {
        let _ = self.client.get("https://www.deezer.com").send().await;
        let pw_hash = md5_hex(password.as_bytes());
        let hash_in = format!(
            "{}{}{}{}",
            self.client_id, email, pw_hash, self.client_secret
        );
        let hash = md5_hex(hash_in.as_bytes());
        let url = "https://connect.deezer.com/oauth/user_auth.php";
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        let resp = self.client.get(url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
            .header("Referer", "https://www.deezer.com")
            .header("Origin", "https://www.deezer.com")
            .query(&[
                ("app_id", self.client_id.as_str()),
                ("login", email),
                ("password", pw_hash.as_str()),
                ("hash", hash.as_str()),
            ])
            .send().await.map_err(|e| Error::Other(format!("Deezer email login: {e}")))?;

        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Deezer login JSON: {e}")))?;
        if v.get("error").is_some() {
            return Err(Error::Other(
                "Deezer authentication failed. Check email/password or use ARL.".to_string(),
            ));
        }
        let access_token = v
            .get("access_token")
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        let arl = v.get("arl").and_then(|t| t.as_str()).unwrap_or_default();
        if access_token.is_empty() && arl.is_empty() {
            return Err(Error::Other(
                "Deezer email login returned no access_token/arl".to_string(),
            ));
        }
        if !access_token.is_empty() {
            self.api_token = Some(access_token.to_string());
        }
        if !arl.is_empty() {
            self.login_via_arl(arl).await?;
            return Ok(arl.to_string());
        }
        Ok(access_token.to_string())
    }

    async fn get_track(&mut self, id: &str) -> Result<Value> {
        self.api_call("deezer.pageTrack", json!({"sng_id": id}))
            .await
    }

    async fn get_track_data(&mut self, id: &str) -> Result<Value> {
        self.api_call("song.getData", json!({"sng_id": id})).await
    }

    async fn get_track_lyrics(&mut self, id: &str) -> Result<Value> {
        self.api_call("song.getLyrics", json!({"sng_id": id})).await
    }

    async fn get_track_data_by_isrc(&self, isrc: &str) -> Result<Value> {
        // Port of `DeezerAPI.get_track_data_by_isrc`: public lookup that maps
        // the response onto the internal SNG_* schema used by search.
        let resp = self
            .client
            .get(&format!("https://api.deezer.com/track/isrc:{isrc}"))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer ISRC lookup: {e}")))?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Deezer ISRC JSON: {e}")))?;
        if v.get("error").is_some() {
            return Err(Error::Other("Track not found by ISRC".to_string()));
        }
        let contributors: Vec<Value> = v
            .get("contributors")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let artists: Vec<Value> = contributors
            .iter()
            .map(|a| json!({"ART_NAME": a.get("name").and_then(|n| n.as_str()).unwrap_or("")}))
            .collect();
        Ok(json!({
            "SNG_ID": v.get("id").and_then(|i| i.as_i64()).unwrap_or(0).to_string(),
            "SNG_TITLE": v.get("title_short").or_else(|| v.get("title")).and_then(|t| t.as_str()).unwrap_or(""),
            "VERSION": v.get("title_version").and_then(|t| t.as_str()).unwrap_or(""),
            "ARTISTS": artists,
            "EXPLICIT_LYRICS": if v.get("explicit_lyrics").and_then(|e| e.as_bool()).unwrap_or(false) { "1" } else { "0" },
            "ALB_TITLE": v.get("album").and_then(|a| a.get("title")).and_then(|t| t.as_str()).unwrap_or(""),
            "ALB_ID": v.get("album").and_then(|a| a.get("id")).and_then(|i| i.as_i64()).map(|i| i.to_string()).unwrap_or_default(),
            "ALB_PICTURE": "",
            "DURATION": v.get("duration").and_then(|d| d.as_u64()).unwrap_or(0).to_string(),
        }))
    }

    async fn get_track_cover_md5(&mut self, id: &str) -> Result<String> {
        let v = self
            .api_call(
                "song.getData",
                json!({"sng_id": id, "array_default": ["ALB_PICTURE"]}),
            )
            .await?;
        Ok(v.get("ALB_PICTURE")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    async fn get_track_contributors(&mut self, id: &str) -> Result<Value> {
        let v = self
            .api_call(
                "song.getData",
                json!({"sng_id": id, "array_default": ["SNG_CONTRIBUTORS"]}),
            )
            .await?;
        Ok(v.get("SNG_CONTRIBUTORS").cloned().unwrap_or(Value::Null))
    }

    async fn get_album(&mut self, id: &str) -> Result<Value> {
        let lang = self.language.clone().unwrap_or_else(|| "en".to_string());
        match self
            .api_call("deezer.pageAlbum", json!({"alb_id": id, "lang": lang}))
            .await
        {
            Ok(v) => Ok(v),
            Err(_) => {
                // fallback handling omitted; surface original
                self.api_call("deezer.pageAlbum", json!({"alb_id": id, "lang": lang}))
                    .await
            }
        }
    }

    async fn get_playlist(&mut self, id: &str) -> Result<Value> {
        let lang = self.language.clone().unwrap_or_else(|| "en".to_string());
        self.api_call("deezer.pagePlaylist", json!({"nb": -1, "start": 0, "playlist_id": id, "lang": lang, "tab": 0, "tags": true, "header": true})).await
    }

    async fn get_artist_name(&mut self, id: &str) -> Result<String> {
        let v = self
            .api_call(
                "artist.getData",
                json!({"art_id": id, "array_default": ["ART_NAME"]}),
            )
            .await?;
        Ok(v.get("ART_NAME")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string())
    }

    async fn get_artist(&mut self, id: &str, credited_albums: bool) -> Result<Value> {
        self.api_call("album.getDiscography", json!({
            "art_id": id, "start": 0, "nb": -1,
            "filter_role_id": if credited_albums { vec![0, 5] } else { vec![0] },
            "nb_songs": 0,
            "discography_mode": if credited_albums { Some("all") } else { None },
            "array_default": ["ALB_ID", "ALB_TITLE", "ART_NAME", "PHYSICAL_RELEASE_DATE", "ORIGINAL_RELEASE_DATE", "ALB_PICTURE", "EXPLICIT_ALBUM", "EXPLICIT_ALBUM_CONTENT", "EXPLICIT_LYRICS"]
        })).await
    }

    async fn get_track_url(
        &mut self,
        id: &str,
        track_token: &str,
        track_token_expiry: Option<i64>,
        format: &str,
    ) -> Result<String> {
        // Port of `DeezerAPI.get_track_url`: renew license token hourly and
        // renew the track token once it has expired.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if self.license_token.is_none() || (now - self.renew_timestamp as i64) >= 3600 {
            let _ = self.api_call("deezer.getUserData", json!({})).await;
        }
        // renews track token
        let expired = match track_token_expiry {
            Some(exp) => now - exp >= 0,
            None => true,
        };
        let fresh_token = if expired || track_token.is_empty() {
            self.api_call(
                "song.getData",
                json!({"sng_id": id, "array_default": ["TRACK_TOKEN"]}),
            )
            .await
            .ok()
            .and_then(|v| {
                v.get("TRACK_TOKEN")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| track_token.to_string())
        } else {
            track_token.to_string()
        };
        let license = self.license_token.clone().unwrap_or_default();
        let body = json!({
            "license_token": license,
            "media": [{"type": "FULL", "formats": [{"cipher": "BF_CBC_STRIPE", "format": format}]}],
            "track_tokens": [fresh_token]
        });
        let resp = self
            .client
            .post("https://media.deezer.com/v1/get_url")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer get_url: {e}")))?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Deezer get_url JSON: {e}")))?;
        v.get("data")
            .and_then(|d| d.get(0))
            .and_then(|d| d.get("media"))
            .and_then(|m| m.get(0))
            .and_then(|m| m.get("sources"))
            .and_then(|s| s.get(0))
            .and_then(|s| s.get("url"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| Error::Other("Deezer: no stream URL in get_url response".to_string()))
    }

    async fn search(&mut self, query: &str, ty: &str, start: u32, nb: u32) -> Result<Value> {
        self.api_call("search.music", json!({"query": query, "start": start, "nb": nb, "filter": "ALL", "output": ty.to_uppercase()})).await
    }

    // ---- public API (no login) ----

    async fn search_public(
        &self,
        query: &str,
        resource: &str,
        index: u32,
        limit: u32,
    ) -> Result<Value> {
        let url = format!("https://api.deezer.com/search/{resource}");
        let resp = self
            .client
            .get(&url)
            .query(&[
                ("q", query),
                ("index", &index.to_string()),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer public search: {e}")))?;
        resp.json()
            .await
            .map_err(|e| Error::Other(format!("Deezer public search JSON: {e}")))
    }

    async fn get_track_public(&self, track_id: &str) -> Result<Value> {
        let resp = self
            .client
            .get(&format!("https://api.deezer.com/track/{track_id}"))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer public track: {e}")))?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Deezer public track JSON: {e}")))?;
        if v.get("error").is_some() {
            return Err(Error::Other("Track not found".to_string()));
        }
        Ok(v)
    }

    async fn get_album_public(&self, album_id: &str) -> Result<Value> {
        let resp = self
            .client
            .get(&format!("https://api.deezer.com/album/{album_id}"))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer public album: {e}")))?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Deezer public album JSON: {e}")))?;
        if v.get("error").is_some() {
            return Err(Error::Other("Album not found".to_string()));
        }
        Ok(v)
    }

    async fn get_playlist_cover_public(&self, playlist_id: &str) -> Option<String> {
        let resp = self
            .client
            .get(&format!("https://api.deezer.com/playlist/{playlist_id}"))
            .send()
            .await
            .ok()?;
        let v: Value = resp.json().await.ok()?;
        let s = v
            .get("picture_xl")
            .and_then(|v| v.as_str())
            .or_else(|| v.get("picture_big").and_then(|v| v.as_str()))
            .or_else(|| v.get("picture_medium").and_then(|v| v.as_str()))
            .unwrap_or("")
            .trim()
            .to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

// ---------------------------------------------------------------------------
// High-level module
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct DeezerModule {
    controller: ModuleController,
    session: Mutex<DeezerSession>,
    email: String,
    password: String,
}

impl DeezerModule {
    fn quality_to_format(&self, quality: Quality) -> &'static str {
        if quality.contains(Quality::ATMOS)
            || quality.contains(Quality::HIFI)
            || quality.contains(Quality::LOSSLESS)
        {
            "FLAC"
        } else if quality.contains(Quality::HIGH) || quality.contains(Quality::MEDIUM) {
            "MP3_320"
        } else {
            "MP3_128"
        }
    }

    fn compression_num(compression: CoverCompression) -> u32 {
        match compression {
            CoverCompression::High => 80,
            CoverCompression::Low => 50,
        }
    }

    fn effective_cover_options(&self) -> CoverOptions {
        let mut opts = self.controller.picaro_options.default_cover_options;
        if opts.file_type == ImageFileType::Webp {
            opts.file_type = ImageFileType::Jpg;
        }
        opts
    }

    /// Port of `get_image_url` in `interface.py`: caps resolution at 3000 and
    /// builds the cdn-images.dzcdn.net filename for the requested file type.
    fn image_url(
        md5: &str,
        kind: &str,
        file_type: ImageFileType,
        resolution: u32,
        compression: u32,
    ) -> String {
        if md5.is_empty() {
            return String::new();
        }
        let mut res = resolution;
        if res > 3000 {
            res = 3000;
        }
        // WebP is not servable by the Deezer image CDN; fall back to JPG
        // (mirrors the Python constructor / get_track_cover guards).
        let ft = if file_type == ImageFileType::Webp {
            ImageFileType::Jpg
        } else {
            file_type
        };
        let filename = match ft {
            ImageFileType::Jpg => format!("{res}x0-000000-{compression}-0-0.jpg"),
            ImageFileType::Png => format!("{res}x0-none-100-0-0.png"),
            ImageFileType::Webp => format!("{res}x0-000000-{compression}-0-0.jpg"),
        };
        format!("https://cdn-images.dzcdn.net/images/{kind}/{md5}/{filename}")
    }

    /// Port of `check_sub` in `interface.py`: warn when the configured quality
    /// tier is not accessible with the current subscription, or when the
    /// subscription exposes no formats at all.
    fn check_sub(&self) {
        let Ok(sess) = self.session.try_lock() else {
            return;
        };
        if self.controller.picaro_options.disable_subscription_check {
            return;
        }
        let format = self.quality_to_format(self.controller.picaro_options.quality_tier);
        if sess.available_formats.is_empty() {
            eprintln!(
                "Deezer: subscription check returned no available formats; downloads may fail"
            );
            return;
        }
        if !sess.available_formats.iter().any(|f| f == format) {
            eprintln!(
                "Deezer: quality set in the settings is not accessible by the current subscription"
            );
        }
    }

    /// Apply `alb_tags` (passed via `data["alb_tags"]`, mirroring the Python
    /// `alb_tags` kwarg / `track_extra_kwargs`) onto a `Tags` struct.
    fn apply_alb_tags(tags: &mut Tags, alb: &Value) {
        let get_u32 = |v: &Value| -> Option<u32> {
            v.as_u64()
                .map(|i| i as u32)
                .or_else(|| v.as_str().and_then(|s| s.parse::<u32>().ok()))
        };
        if tags.total_tracks.is_none() {
            if let Some(v) = alb.get("total_tracks") {
                tags.total_tracks = get_u32(v);
            }
        }
        if tags.total_discs.is_none() {
            if let Some(v) = alb.get("total_discs") {
                tags.total_discs = get_u32(v);
            }
        }
        if tags.upc.is_none() {
            if let Some(s) = alb.get("upc").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    tags.upc = Some(s.to_string());
                }
            }
        }
        if tags.label.is_none() {
            if let Some(s) = alb.get("label").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    tags.label = Some(s.to_string());
                }
            }
        }
        if tags.album_artist.is_none() {
            if let Some(s) = alb.get("album_artist").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    tags.album_artist = Some(s.to_string());
                }
            }
        }
        if tags.release_date.is_none() {
            if let Some(s) = alb.get("release_date").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    tags.release_date = Some(s.to_string());
                }
            }
        }
        if tags.genres.is_none() {
            if let Some(arr) = alb.get("genres").and_then(|v| v.as_array()) {
                let g: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect();
                if !g.is_empty() {
                    tags.genres = Some(g);
                }
            }
        }
    }

    async fn ensure_credentials(&self) -> Result<()> {
        if self.session.lock().await.is_authenticated() {
            return Ok(());
        }
        let (arl, email, password) = {
            let s = self.session.lock().await;
            (
                s.arl.clone().unwrap_or_default(),
                self.email.clone(),
                self.password.clone(),
            )
        };
        // also check temporary settings controller for persisted arl
        let tsc_arl = self
            .controller
            .temporary_settings_controller
            .read("arl", TempSettingType::Custom)
            .ok()
            .flatten()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        let arl = if !tsc_arl.is_empty() { tsc_arl } else { arl };
        if !arl.is_empty() {
            match self.session.lock().await.login_via_arl(&arl).await {
                Ok(_) => {
                    self.check_sub();
                    return Ok(());
                }
                Err(_) => {}
            }
        }
        if !email.is_empty() && !password.is_empty() {
            let arl = self
                .session
                .lock()
                .await
                .login_via_email(&email, &password)
                .await?;
            let _ = self.controller.temporary_settings_controller.set(
                "arl",
                Value::String(arl),
                TempSettingType::Custom,
            );
            self.check_sub();
            return Ok(());
        }
        Err(Error::Other("Deezer credentials are required for downloading. Please fill in either email and password, or arl in the settings.".to_string()))
    }
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for DeezerModule {
    fn name(&self) -> &str {
        "Deezer"
    }

    fn is_authenticated(&self) -> bool {
        self.session
            .try_lock()
            .map(|s| s.is_authenticated())
            .unwrap_or(false)
    }

    async fn ensure_can_download(&self) -> Result<()> {
        self.ensure_credentials().await
    }

    async fn login(&self, email: &str, password: &str) -> Result<()> {
        let arl = self
            .session
            .lock()
            .await
            .login_via_email(email, password)
            .await?;
        let _ = self.controller.temporary_settings_controller.set(
            "arl",
            Value::String(arl),
            TempSettingType::Custom,
        );
        self.check_sub();
        Ok(())
    }

    async fn logout(&self) -> Result<()> {
        let mut s = self.session.lock().await;
        s.api_token = None;
        s.license_token = None;
        s.arl = None;
        Ok(())
    }

    fn custom_url_parse(&self, url: &str) -> Result<Option<MediaIdentification>> {
        // dzr.page.link short links + /{locale}/track|album|artist|playlist/{id}
        let link = url.to_string();
        if link.contains("dzr.page.link") {
            return Ok(None); // let downloader resolve via HTTP; avoid blocking here
        }
        let parsed =
            url::Url::parse(&link).map_err(|e| Error::Other(format!("Invalid URL {url}: {e}")))?;
        let path = parsed.path();
        // match /(xx/)?(track|album|artist|playlist)/(\d+)
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return Err(Error::Other(format!("Invalid URL: {url}")));
        }
        // find type segment
        let mut ty: Option<DownloadType> = None;
        let mut id: Option<String> = None;
        for (i, s) in segs.iter().enumerate() {
            let t = match *s {
                "track" => Some(DownloadType::track),
                "album" => Some(DownloadType::album),
                "artist" => Some(DownloadType::artist),
                "playlist" => Some(DownloadType::playlist),
                _ => None,
            };
            if let Some(t) = t {
                ty = Some(t);
                if i + 1 < segs.len() {
                    id = Some(segs[i + 1].to_string());
                }
                break;
            }
        }
        match (ty, id) {
            (Some(t), Some(i)) => {
                let num = i.split('?').next().unwrap_or(&i).to_string();
                Ok(Some(MediaIdentification {
                    media_type: t,
                    media_id: num,
                    extra_kwargs: Default::default(),
                }))
            }
            _ => Err(Error::Other(format!("Invalid URL: {url}"))),
        }
    }

    async fn get_track_info(
        &self,
        track_id: &str,
        quality: Quality,
        _codec: &CodecOptions,
        data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        // Public fallback when not authenticated
        if !self.session.lock().await.is_authenticated() {
            return self.get_track_info_public(track_id, data).await;
        }
        self.ensure_credentials().await?;
        let is_user_upped = track_id.parse::<i64>().map(|n| n < 0).unwrap_or(false);
        let cached = data.get(track_id).cloned();
        let raw = if let Some(v) = cached {
            // only use cache when it is a full pageTrack payload
            if !is_user_upped && v.get("DATA").is_none() {
                self.session.lock().await.get_track(track_id).await?
            } else {
                v
            }
        } else if is_user_upped {
            self.session.lock().await.get_track_data(track_id).await?
        } else {
            self.session.lock().await.get_track(track_id).await?
        };
        let mut t_data = raw.clone();
        if let Some(inner) = raw.get("DATA") {
            t_data = inner.clone();
        }
        if let Some(fb) = t_data.get("FALLBACK") {
            t_data = fb.clone();
        }
        let mut format = if is_user_upped {
            "MP3_MISC".to_string()
        } else {
            self.quality_to_format(quality).to_string()
        };
        if !is_user_upped {
            if let Some(countries) = t_data
                .get("AVAILABLE_COUNTRIES")
                .and_then(|a| a.get("STREAM_ADS"))
                .and_then(|v| v.as_array())
            {
                if !countries.is_empty() {
                    let country = self
                        .session
                        .lock()
                        .await
                        .country
                        .clone()
                        .unwrap_or_else(|| "US".to_string());
                    if !countries.iter().any(|c| c.as_str() == Some(&country)) {
                        return Err(Error::TrackUnavailable(
                            "Track not available in your country".into(),
                        ));
                    }
                } else {
                    return Err(Error::TrackUnavailable("Track not available".into()));
                }
            }
            // Port of the Python premium-format walk-down: from the requested
            // format downwards, pick the first format whose FILESIZE is not '0'.
            let premium = ["FLAC", "MP3_320"];
            let mut to_check: Vec<&str> = {
                let mut v = Vec::new();
                let mut push = false;
                for f in premium {
                    if f == format {
                        push = true;
                    }
                    if push {
                        v.push(f);
                    }
                }
                // Requested MP3_128 (or unknown): only MP3_128 is eligible.
                if v.is_empty() {
                    v.push("MP3_128");
                }
                v
            };
            let _ = to_check.len();
            let mut temp_f: Option<String> = None;
            for f in to_check.drain(..) {
                let size = t_data
                    .get(format!("FILESIZE_{f}"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("0");
                if size != "0" {
                    temp_f = Some(f.to_string());
                    break;
                }
            }
            format = temp_f.unwrap_or_else(|| "MP3_128".to_string());
            if !self
                .session
                .lock()
                .await
                .available_formats
                .contains(&format)
            {
                return Err(Error::TrackUnavailable(
                    "Format not available by your subscription".into(),
                ));
            }
        }
        let codec = match format.as_str() {
            "FLAC" => CodecFlags::FLAC,
            "MP3_320" => CodecFlags::MP3,
            "MP3_128" => CodecFlags::MP3,
            "MP3_MISC" => CodecFlags::MP3,
            _ => CodecFlags::MP3,
        };
        let bitrate = match format.as_str() {
            "FLAC" => Some(1411),
            "MP3_320" => Some(320),
            "MP3_128" => Some(128),
            _ => None,
        };
        let artists: Vec<String> = t_data
            .get("ARTISTS")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|a| {
                        a.get("ART_NAME")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_else(|| {
                t_data
                    .get("ART_NAME")
                    .and_then(|v| v.as_str())
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_default()
            });
        let cover_opts = self.effective_cover_options();
        let picture = t_data
            .get("ALB_PICTURE")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let cover_url = Self::image_url(
            &picture,
            "cover",
            ImageFileType::Jpg,
            cover_opts.resolution,
            Self::compression_num(cover_opts.compression),
        );
        let track_title = {
            let base = t_data
                .get("SNG_TITLE")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ver = t_data.get("VERSION").and_then(|v| v.as_str()).unwrap_or("");
            if ver.is_empty() {
                base.to_string()
            } else {
                format!("{base} {ver}")
            }
        };
        let sng_id = t_data
            .get("SNG_ID")
            .and_then(|v| v.as_i64())
            .or_else(|| {
                t_data
                    .get("SNG_ID")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<i64>().ok())
            })
            .unwrap_or(0);
        let track_token = t_data
            .get("TRACK_TOKEN")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let track_token_expiry = t_data
            .get("TRACK_TOKEN_EXPIRE")
            .and_then(|v| v.as_i64())
            .or_else(|| {
                t_data
                    .get("TRACK_TOKEN_EXPIRE")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<i64>().ok())
            });
        let mut download_map = serde_json::Map::new();
        download_map.insert("id".to_string(), json!(sng_id));
        download_map.insert("track_token".to_string(), json!(track_token));
        if let Some(exp) = track_token_expiry {
            download_map.insert("track_token_expiry".to_string(), json!(exp));
        }
        download_map.insert("format".to_string(), json!(format));
        let parse_u32 = |v: &Value| -> Option<u32> {
            v.as_u64()
                .map(|i| i as u32)
                .or_else(|| v.as_str().and_then(|s| s.parse::<u32>().ok()))
        };
        let mut tags = Tags {
            track_number: t_data.get("TRACK_NUMBER").and_then(parse_u32),
            disc_number: t_data.get("DISK_NUMBER").and_then(parse_u32),
            isrc: t_data
                .get("ISRC")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            release_date: t_data
                .get("PHYSICAL_RELEASE_DATE")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            copyright: t_data
                .get("COPYRIGHT")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            track_url: Some(format!("https://www.deezer.com/track/{track_id}")),
            replay_gain: t_data
                .get("GAIN")
                .and_then(|v| v.as_f64())
                .map(|f| f as f32)
                .or_else(|| {
                    t_data
                        .get("GAIN")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<f32>().ok())
                }),
            comment: None,
            ..Default::default()
        };
        // alb_tags passthrough (mirrors the Python `alb_tags` kwarg, delivered
        // via `data["alb_tags"]` / `track_extra_kwargs`).
        if let Some(alb) = data.get("alb_tags") {
            Self::apply_alb_tags(&mut tags, alb);
        }
        let alb_id = t_data
            .get("ALB_ID")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .or_else(|| {
                t_data
                    .get("ALB_ID")
                    .and_then(|v| v.as_i64())
                    .map(|i| i.to_string())
            })
            .unwrap_or_default();
        // Album genre/label/UPC enrichment for single-track downloads.
        if (tags.label.is_none() || tags.upc.is_none() || tags.genres.is_none())
            && !alb_id.is_empty()
        {
            if let Ok(album_data) = self.session.lock().await.get_album(&alb_id).await {
                if let Some(a) = album_data.get("DATA") {
                    if tags.label.is_none() {
                        if let Some(s) = a.get("LABEL_NAME").and_then(|v| v.as_str()) {
                            if !s.is_empty() {
                                tags.label = Some(s.to_string());
                            }
                        }
                    }
                    if tags.upc.is_none() {
                        if let Some(s) = a.get("UPC").and_then(|v| v.as_str()) {
                            if !s.is_empty() {
                                tags.upc = Some(s.to_string());
                            }
                        }
                    }
                    if tags.genres.is_none() {
                        if let Some(arr) = a
                            .get("GENRES")
                            .and_then(|g| g.get("data"))
                            .and_then(|v| v.as_array())
                        {
                            let g: Vec<String> = arr
                                .iter()
                                .filter_map(|x| {
                                    x.get("GENRE_NAME")
                                        .and_then(|n| n.as_str())
                                        .map(|s| s.to_string())
                                })
                                .collect();
                            if !g.is_empty() {
                                tags.genres = Some(g);
                            }
                        }
                    }
                }
            }
        }
        if tags.album_artist.is_none() {
            if let Some(s) = t_data.get("ART_NAME").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    tags.album_artist = Some(s.to_string());
                }
            }
        }
        if tags.genres.is_none() {
            if let Some(arr) = t_data
                .get("GENRES")
                .and_then(|g| g.get("data"))
                .and_then(|v| v.as_array())
            {
                let g: Vec<String> = arr
                    .iter()
                    .filter_map(|x| {
                        x.get("GENRE_NAME")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect();
                if !g.is_empty() {
                    tags.genres = Some(g);
                }
            }
        }
        // Robust fallback: public API genres (common for single tracks).
        if tags.genres.is_none() {
            if let Ok(public_track) = self.session.lock().await.get_track_public(track_id).await {
                if let Some(album_id) = public_track
                    .get("album")
                    .and_then(|a| a.get("id"))
                    .and_then(|v| v.as_i64())
                {
                    if let Ok(public_album) = self
                        .session
                        .lock()
                        .await
                        .get_album_public(&album_id.to_string())
                        .await
                    {
                        if let Some(arr) = public_album
                            .get("genres")
                            .and_then(|g| g.get("data"))
                            .and_then(|v| v.as_array())
                        {
                            let g: Vec<String> = arr
                                .iter()
                                .filter_map(|x| {
                                    x.get("name")
                                        .and_then(|n| n.as_str())
                                        .map(|s| s.to_string())
                                })
                                .collect();
                            if !g.is_empty() {
                                tags.genres = Some(g);
                            }
                        }
                    }
                }
            }
        }
        let mut cover_map = serde_json::Map::new();
        cover_map.insert(track_id.to_string(), json!(picture));
        let mut credits_map = serde_json::Map::new();
        credits_map.insert(
            track_id.to_string(),
            t_data
                .get("SNG_CONTRIBUTORS")
                .cloned()
                .unwrap_or(Value::Null),
        );
        let mut lyrics_map = serde_json::Map::new();
        lyrics_map.insert(
            track_id.to_string(),
            raw.get("LYRICS").cloned().unwrap_or(Value::Null),
        );
        let duration = t_data
            .get("DURATION")
            .and_then(|v| v.as_u64().map(|i| i as u32))
            .or_else(|| {
                t_data
                    .get("DURATION")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u32>().ok())
            });
        Ok(TrackInfo {
            id: Some(track_id.to_string()),
            name: track_title,
            album_id: alb_id,
            album: t_data
                .get("ALB_TITLE")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default(),
            artists,
            tags,
            codec,
            cover_url,
            release_year: t_data
                .get("PHYSICAL_RELEASE_DATE")
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0),
            duration,
            explicit: t_data
                .get("EXPLICIT_LYRICS")
                .and_then(|v| v.as_str())
                .map(|s| s == "1"),
            artist_id: t_data
                .get("ART_ID")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .or_else(|| {
                    t_data
                        .get("ART_ID")
                        .and_then(|v| v.as_i64())
                        .map(|i| i.to_string())
                }),
            bit_depth: Some(16),
            sample_rate: Some(44.1),
            bitrate,
            download_extra_kwargs: download_map,
            cover_extra_kwargs: cover_map,
            credits_extra_kwargs: credits_map,
            lyrics_extra_kwargs: lyrics_map,
            ..Default::default()
        })
    }

    async fn get_track_download(
        &self,
        _track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        self.ensure_credentials().await?;
        let id = data
            .get("id")
            .and_then(|v| v.as_i64())
            .or_else(|| {
                data.get("id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<i64>().ok())
            })
            .ok_or_else(|| Error::Other("missing id".to_string()))?;
        let track_token = data
            .get("track_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let track_token_expiry = data
            .get("track_token_expiry")
            .and_then(|v| v.as_i64())
            .or_else(|| {
                data.get("track_token_expiry")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<i64>().ok())
            });
        let format = data
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or("MP3_320")
            .to_string();
        let url = self
            .session
            .lock()
            .await
            .get_track_url(&id.to_string(), &track_token, track_token_expiry, &format)
            .await?;
        let (client, bf_secret) = {
            let s = self.session.lock().await;
            (s.client.clone(), s.bf_secret.clone())
        };
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer download failed: {e}")))?;
        if !response.status().is_success() {
            return Err(Error::Other(format!(
                "Deezer download failed: {}",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| Error::Other(format!("Deezer read body: {e}")))?;
        let decrypted = if format != "FLAC" {
            decrypt_deezer_stripe(&bytes, &id.to_string(), &bf_secret)
        } else {
            bytes.to_vec()
        };
        let hash_prefix = md5_hex(&decrypted[..decrypted.len().min(64)]);
        let temp_path = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join("temp")
            .join(format!(
                "dz-{id}-{}-{}.{}",
                std::process::id(),
                &hash_prefix[..8.min(hash_prefix.len())],
                extension_for(&format)
            ));
        std::fs::create_dir_all(temp_path.parent().unwrap())
            .map_err(|e| Error::Other(format!("mkdir temp: {e}")))?;
        let mut f = std::fs::File::create(&temp_path)
            .map_err(|e| Error::Other(format!("create temp: {e}")))?;
        f.write_all(&decrypted)
            .map_err(|e| Error::Other(format!("write temp: {e}")))?;
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::TempFilePath,
            file_url: None,
            file_url_headers: serde_json::Map::new(),
            temp_file_path: Some(temp_path),
            different_codec: Some(CodecFlags::empty()),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        if !self.session.lock().await.is_authenticated() {
            return self.get_album_info_public(album_id).await;
        }
        self.ensure_credentials().await?;
        let raw = if let Some(v) = data.get(album_id) {
            v.clone()
        } else {
            self.session.lock().await.get_album(album_id).await?
        };
        let a_data = raw.get("DATA").cloned().unwrap_or_else(|| raw.clone());
        let tracks = raw
            .get("SONGS")
            .and_then(|s| s.get("data"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let track_ids: Vec<TrackRef> = tracks
            .iter()
            .filter_map(|t| {
                t.get("SNG_ID")
                    .and_then(|i| i.as_i64().map(|n| n.to_string()))
                    .or_else(|| {
                        t.get("SNG_ID")
                            .and_then(|i| i.as_str().map(|s| s.to_string()))
                    })
                    .map(TrackRef::Id)
            })
            .collect();
        let picture = a_data
            .get("ALB_PICTURE")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Placeholder images can't be requested as pngs (mirrors Python).
        let cover_opts = self.effective_cover_options();
        let cover_type = if picture.is_empty() || picture == "0" {
            ImageFileType::Jpg
        } else {
            cover_opts.file_type
        };
        let cover_comp = Self::compression_num(cover_opts.compression);
        let cover_url = Self::image_url(
            &picture,
            "cover",
            cover_type,
            cover_opts.resolution,
            cover_comp,
        );
        let all_jpg = Self::image_url(
            &picture,
            "cover",
            ImageFileType::Jpg,
            cover_opts.resolution,
            cover_comp,
        );
        let total_tracks = tracks
            .last()
            .and_then(|t| t.get("TRACK_NUMBER"))
            .and_then(|v| {
                v.as_u64()
                    .map(|i| i as u32)
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u32>().ok()))
            })
            .unwrap_or(0);
        let total_discs = tracks
            .last()
            .and_then(|t| t.get("DISK_NUMBER"))
            .and_then(|v| {
                v.as_u64()
                    .map(|i| i as u32)
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u32>().ok()))
            })
            .unwrap_or(0);
        let album_artist = a_data
            .get("ART_NAME")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let genres: Vec<String> = a_data
            .get("GENRES")
            .and_then(|g| g.get("data"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|g| {
                        g.get("GENRE_NAME")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let release_date = a_data
            .get("ORIGINAL_RELEASE_DATE")
            .or_else(|| a_data.get("PHYSICAL_RELEASE_DATE"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let alb_tags = json!({
            "total_tracks": total_tracks,
            "total_discs": total_discs,
            "upc": a_data.get("UPC").and_then(|v| v.as_str()).unwrap_or(""),
            "label": a_data.get("LABEL_NAME").and_then(|v| v.as_str()).unwrap_or(""),
            "album_artist": album_artist,
            "release_date": release_date,
            "genres": genres,
        });
        let mut track_extra = serde_json::Map::new();
        track_extra.insert("alb_tags".to_string(), alb_tags);
        let mut expected_track_count = total_tracks;
        if let Some(n) = a_data.get("NUMBER_TRACK").and_then(|v| {
            v.as_u64()
                .map(|i| i as u32)
                .or_else(|| v.as_str().and_then(|s| s.parse::<u32>().ok()))
        }) {
            expected_track_count = n;
        }
        let explicit = a_data
            .get("EXPLICIT_ALBUM_CONTENT")
            .and_then(|e| e.get("EXPLICIT_LYRICS_STATUS"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            })
            .map(|s| s == 1 || s == 4);
        Ok(AlbumInfo {
            id: Some(album_id.to_string()),
            name: a_data
                .get("ALB_TITLE")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artist: a_data
                .get("ART_NAME")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artist_id: a_data
                .get("ART_ID")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .or_else(|| {
                    a_data
                        .get("ART_ID")
                        .and_then(|v| v.as_i64())
                        .map(|i| i.to_string())
                }),
            tracks: track_ids,
            release_year: a_data
                .get("ORIGINAL_RELEASE_DATE")
                .or_else(|| a_data.get("PHYSICAL_RELEASE_DATE"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0),
            cover_url: Some(cover_url),
            cover_type: Some(cover_type),
            all_track_cover_jpg_url: Some(all_jpg),
            explicit,
            upc: a_data
                .get("UPC")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            label: a_data
                .get("LABEL_NAME")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            album_artist: a_data
                .get("ART_NAME")
                .and_then(|v| v.as_str())
                .map(|s| MultiArtist::Single(s.to_string())),
            expected_track_count: if expected_track_count == 0 {
                None
            } else {
                Some(expected_track_count)
            },
            track_extra_kwargs: track_extra,
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        if !self.session.lock().await.is_authenticated() {
            return self.get_playlist_info_public(playlist_id).await;
        }
        self.ensure_credentials().await?;
        let raw = self.session.lock().await.get_playlist(playlist_id).await?;
        let p_data = raw.get("DATA").cloned().unwrap_or_else(|| raw.clone());
        let songs = raw
            .get("SONGS")
            .and_then(|s| s.get("data"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let track_ids: Vec<TrackRef> = songs
            .iter()
            .filter_map(|t| {
                t.get("SNG_ID")
                    .and_then(|i| i.as_i64().map(|n| n.to_string()))
                    .or_else(|| {
                        t.get("SNG_ID")
                            .and_then(|i| i.as_str().map(|s| s.to_string()))
                    })
                    .map(TrackRef::Id)
            })
            .collect();
        let cover_opts = self.effective_cover_options();
        let cover_comp = Self::compression_num(cover_opts.compression);
        let cover_url = self
            .session
            .lock()
            .await
            .get_playlist_cover_public(playlist_id)
            .await
            .or_else(|| {
                let p_pic = p_data
                    .get("PLAYLIST_PICTURE")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if p_pic.is_empty() {
                    songs
                        .first()
                        .and_then(|s| s.get("ALB_PICTURE"))
                        .and_then(|v| v.as_str())
                        .map(|p| {
                            Self::image_url(
                                p,
                                "cover",
                                ImageFileType::Jpg,
                                cover_opts.resolution,
                                cover_comp,
                            )
                        })
                } else {
                    Some(Self::image_url(
                        p_pic,
                        "playlist",
                        cover_opts.file_type,
                        cover_opts.resolution,
                        cover_comp,
                    ))
                }
            })
            .unwrap_or_default();
        // Placeholder images can't be requested as pngs.
        let p_pic = p_data
            .get("PLAYLIST_PICTURE")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cover_type = if p_pic.is_empty() {
            ImageFileType::Jpg
        } else {
            cover_opts.file_type
        };
        // Only user-uploaded tracks (SNG_ID < 0) go in data; they can't be
        // fetched via pageTrack, so cache them for get_track_info.
        let mut user_upped = serde_json::Map::new();
        for t in &songs {
            let sid = t
                .get("SNG_ID")
                .and_then(|i| i.as_i64())
                .or_else(|| {
                    t.get("SNG_ID")
                        .and_then(|i| i.as_str())
                        .and_then(|s| s.parse::<i64>().ok())
                })
                .unwrap_or(0);
            if sid < 0 {
                user_upped.insert(sid.to_string(), t.clone());
            }
        }
        let mut track_extra = serde_json::Map::new();
        track_extra.insert("data".to_string(), Value::Object(user_upped));
        Ok(PlaylistInfo {
            id: Some(playlist_id.to_string()),
            name: p_data
                .get("TITLE")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            creator: p_data
                .get("PARENT_USERNAME")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            creator_id: p_data
                .get("PARENT_USER_ID")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .or_else(|| {
                    p_data
                        .get("PARENT_USER_ID")
                        .and_then(|v| v.as_i64())
                        .map(|i| i.to_string())
                }),
            tracks: track_ids,
            release_year: p_data
                .get("DATE_ADD")
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0),
            cover_url: Some(cover_url),
            cover_type: Some(cover_type),
            description: p_data
                .get("DESCRIPTION")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            track_extra_kwargs: track_extra,
            ..Default::default()
        })
    }

    async fn get_artist_info(
        &self,
        artist_id: &str,
        _get_credited_albums: bool,
        artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        if !self.session.lock().await.is_authenticated() {
            return self.get_artist_info_public(artist_id, artist_name).await;
        }
        self.ensure_credentials().await?;
        let name = if let Some(n) = artist_name {
            n.to_string()
        } else {
            self.session.lock().await.get_artist_name(artist_id).await?
        };
        let raw = self
            .session
            .lock()
            .await
            .get_artist(artist_id, _get_credited_albums)
            .await?;
        let items = raw
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut albums: Vec<Value> = items.iter().filter_map(|alb| {
            let data = alb.get("DATA").unwrap_or(alb);
            let id = data.get("ALB_ID")
                .and_then(|v| v.as_i64().map(|i| i.to_string()))
                .or_else(|| data.get("ALB_ID").and_then(|v| v.as_str().map(|s| s.to_string())))?;
            let title = data.get("ALB_TITLE").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
            let artist = data.get("ART_NAME").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| name.clone());
            let release_date = data.get("ORIGINAL_RELEASE_DATE").or_else(|| data.get("PHYSICAL_RELEASE_DATE"))
                .and_then(|v| v.as_str()).unwrap_or("");
            let year = if release_date.is_empty() { Value::Null } else {
                Value::String(release_date.split('-').next().unwrap_or("").to_string())
            };
            let cover = data.get("ALB_PICTURE").and_then(|v| v.as_str())
                .map(|s| Self::image_url(s, "cover", ImageFileType::Jpg, 56, 80));
            // Multi-level explicit check, mirroring Python.
            let mut explicit: Option<bool> = None;
            let exp_content = data.get("EXPLICIT_ALBUM_CONTENT").or_else(|| alb.get("EXPLICIT_ALBUM_CONTENT"));
            if let Some(ec) = exp_content {
                if let Some(st) = ec.get("EXPLICIT_LYRICS_STATUS") {
                    let s = if let Some(n) = st.as_u64() { n.to_string() } else { st.as_str().unwrap_or("").to_string() };
                    if s == "1" || s == "4" {
                        explicit = Some(true);
                    }
                }
            }
            if explicit.is_none() {
                for k in ["EXPLICIT_LYRICS", "explicit_lyrics", "EXPLICIT_ALBUM", "explicit_content_lyrics"] {
                    let val = data.get(k).or_else(|| alb.get(k));
                    if let Some(v) = val {
                        let s = if let Some(b) = v.as_bool() {
                            if b { "true".to_string() } else { "false".to_string() }
                        } else if let Some(n) = v.as_u64() {
                            n.to_string()
                        } else {
                            v.as_str().unwrap_or("").to_string()
                        };
                        if s.to_lowercase() == "true" || s == "1" || s == "4" {
                            explicit = Some(true);
                            break;
                        }
                    }
                }
            }
            if explicit.is_none() && (title.to_lowercase().contains("explicit") || title.to_lowercase().contains("(explicit")) {
                explicit = Some(true);
            }
            Some(json!({"id": id, "name": title, "artist": artist, "release_year": year, "cover_url": cover, "explicit": explicit}))
        }).collect();
        // Batch fetch missing durations/years/track counts/explicit status via
        // the public album API (port of the Python ThreadPoolExecutor path;
        // sequential here to avoid extra deps).
        let missing: Vec<usize> = albums
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let no_dur = t.get("duration").is_none();
                let no_year = t
                    .get("release_year")
                    .and_then(|v| v.as_str())
                    .map(|s| s.is_empty())
                    .unwrap_or(true)
                    && t.get("release_year").and_then(|v| v.as_i64()).is_none();
                let no_add = t.get("additional").is_none();
                let not_exp = t.get("explicit").and_then(|v| v.as_bool()) != Some(true);
                if no_dur || no_year || no_add || not_exp {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        for idx in missing {
            let aid = albums[idx]
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if aid.is_empty() {
                continue;
            }
            if let Ok(a_data) = self.session.lock().await.get_album_public(&aid).await {
                let nb = a_data.get("nb_tracks").and_then(|v| v.as_u64());
                let dur = a_data
                    .get("duration")
                    .and_then(|v| v.as_u64())
                    .map(|d| d as u32);
                let year = a_data
                    .get("release_date")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.split('-').next())
                    .map(|s| s.to_string());
                let title = a_data
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let explicit_pub = a_data
                    .get("explicit_lyrics")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                    || a_data
                        .get("explicit_content_lyrics")
                        .and_then(|v| v.as_u64())
                        == Some(1)
                    || a_data
                        .get("explicit_content_lyrics")
                        .and_then(|v| v.as_str())
                        == Some("1")
                    || title.contains("explicit");
                if albums[idx].get("duration").is_none() {
                    if let Some(d) = dur {
                        albums[idx]["duration"] = json!(d);
                    }
                }
                let year_missing = albums[idx]
                    .get("release_year")
                    .and_then(|v| v.as_str())
                    .map(|s| s.is_empty())
                    .unwrap_or(true)
                    && albums[idx]
                        .get("release_year")
                        .and_then(|v| v.as_i64())
                        .is_none();
                if year_missing {
                    if let Some(y) = year {
                        albums[idx]["release_year"] = json!(y);
                    }
                }
                if albums[idx].get("additional").is_none() {
                    if let Some(n) = nb {
                        let s = if n == 1 {
                            "1 track".to_string()
                        } else {
                            format!("{n} tracks")
                        };
                        albums[idx]["additional"] = json!([s]);
                    }
                }
                if albums[idx].get("explicit").and_then(|v| v.as_bool()) != Some(true) {
                    albums[idx]["explicit"] = json!(explicit_pub);
                }
            }
        }
        Ok(ArtistInfo {
            name,
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
        if track_id.parse::<i64>().map(|n| n < 0).unwrap_or(false) {
            return Ok(Vec::new());
        }
        if !self.session.lock().await.is_authenticated() {
            return Ok(Vec::new());
        }
        let cached = data.get(track_id).cloned();
        let credits = if let Some(c) = cached {
            c
        } else {
            self.session
                .lock()
                .await
                .get_track_contributors(track_id)
                .await?
        };
        let mut out = Vec::new();
        if let Value::Object(map) = credits {
            for (k, v) in map {
                if k == "artist" {
                    continue;
                }
                let names: Vec<String> = match v {
                    Value::String(s) => vec![s],
                    Value::Array(arr) => arr
                        .iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect(),
                    _ => continue,
                };
                out.push(CreditsInfo {
                    credit_type: k,
                    names,
                });
            }
        }
        Ok(out)
    }

    async fn get_track_cover(
        &self,
        track_id: &str,
        cover: &CoverOptions,
        data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        if !self.session.lock().await.is_authenticated() {
            let t = self.session.lock().await.get_track_public(track_id).await?;
            let cover_url = t
                .get("album")
                .and_then(|a| a.get("cover_big"))
                .and_then(|v| v.as_str())
                .or_else(|| {
                    t.get("album")
                        .and_then(|a| a.get("cover_medium"))
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("")
                .to_string();
            return Ok(CoverInfo {
                url: cover_url,
                file_type: ImageFileType::Jpg,
            });
        }
        // Cached ALB_PICTURE md5 (from cover_extra_kwargs) or lightweight lookup.
        let cover_md5 = data
            .get(track_id)
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| {
                data.get("data")
                    .and_then(|d| d.get(track_id))
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default()
            });
        let cover_md5 = if cover_md5.is_empty() {
            self.session
                .lock()
                .await
                .get_track_cover_md5(track_id)
                .await
                .unwrap_or_default()
        } else {
            cover_md5
        };
        // Placeholder images can't be requested as pngs; webp falls back to jpg.
        let file_type =
            if cover_md5.is_empty() || cover_md5 == "0" || cover.file_type == ImageFileType::Webp {
                ImageFileType::Jpg
            } else {
                cover.file_type
            };
        let url = Self::image_url(
            &cover_md5,
            "cover",
            file_type,
            cover.resolution,
            Self::compression_num(cover.compression),
        );
        Ok(CoverInfo { url, file_type })
    }

    async fn get_track_lyrics(
        &self,
        track_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<LyricsInfo> {
        if track_id.parse::<i64>().map(|n| n < 0).unwrap_or(false) {
            return Ok(LyricsInfo::default());
        }
        if !self.session.lock().await.is_authenticated() {
            return Ok(LyricsInfo::default());
        }
        self.ensure_credentials().await?;
        let v = self.session.lock().await.get_track_lyrics(track_id).await?;
        let results = v.get("results").cloned().unwrap_or(Value::Null);
        let embedded = results
            .get("LYRICS_TEXT")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mut synced = None;
        if let Some(arr) = results.get("LYRICS_SYNC_JSON").and_then(|v| v.as_array()) {
            let mut text = String::new();
            for line in arr {
                if let Some(ts) = line.get("lrc_timestamp").and_then(|v| v.as_str()) {
                    if let Some(l) = line.get("line").and_then(|v| v.as_str()) {
                        text.push_str(&format!("{ts}{l}\n"));
                    }
                } else {
                    text.push('\n');
                }
            }
            if !text.is_empty() {
                synced = Some(text);
            }
        }
        Ok(LyricsInfo { embedded, synced })
    }

    async fn search(
        &self,
        query_type: DownloadType,
        query: &str,
        track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let ty = match query_type {
            DownloadType::track => "TRACK",
            DownloadType::album => "ALBUM",
            DownloadType::artist => "ARTIST",
            DownloadType::playlist => "PLAYLIST",
            _ => "TRACK",
        };
        // ISRC fast path (mirrors Python `search`: prefer ISRC lookup when the
        // caller already knows it).
        if query_type == DownloadType::track {
            if let Some(ti) = track_info {
                if let Some(isrc) = ti.tags.isrc.as_deref() {
                    if !isrc.is_empty() {
                        if let Ok(one) =
                            self.session.lock().await.get_track_data_by_isrc(isrc).await
                        {
                            let items = vec![one];
                            return Ok(Self::map_search_items(&items, query_type));
                        }
                    }
                }
            }
        }
        let value = if self.session.lock().await.is_authenticated() {
            self.session
                .lock()
                .await
                .search(query, ty, 0, limit)
                .await
                .unwrap_or(json!({"data": []}))
        } else {
            self.session
                .lock()
                .await
                .search_public(query, &ty.to_lowercase(), 0, limit)
                .await
                .unwrap_or(json!({"data": []}))
        };
        let items = value
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(Self::map_search_items(&items, query_type))
    }

    async fn get_preview_stream_url(&self, track_id: &str) -> Result<Option<String>> {
        let v = self
            .session
            .lock()
            .await
            .get_track_public(track_id)
            .await
            .ok();
        Ok(v.and_then(|v| {
            v.get("preview")
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
        }))
    }
}

impl DeezerModule {
    fn map_search_items(items: &[Value], query_type: DownloadType) -> Vec<SearchResult> {
        let mut out = Vec::new();
        for item in items {
            // Support both gw-light schema (SNG_ID/ALB_ID/...) and public schema (id/title/...)
            let id = match query_type {
                DownloadType::track => item
                    .get("SNG_ID")
                    .and_then(json_id)
                    .or_else(|| item.get("id").and_then(json_id)),
                DownloadType::album => item
                    .get("ALB_ID")
                    .and_then(json_id)
                    .or_else(|| item.get("id").and_then(json_id)),
                DownloadType::artist => item
                    .get("ART_ID")
                    .and_then(json_id)
                    .or_else(|| item.get("id").and_then(json_id)),
                DownloadType::playlist => item
                    .get("PLAYLIST_ID")
                    .and_then(json_id)
                    .or_else(|| item.get("id").and_then(json_id)),
                _ => None,
            };
            // Title with version suffix (mirrors Python `title + VERSION/title_version`).
            let version = item
                .get("VERSION")
                .or_else(|| item.get("title_version"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let base = item
                .get("SNG_TITLE")
                .or_else(|| item.get("ALB_TITLE"))
                .or_else(|| item.get("ART_NAME"))
                .or_else(|| item.get("TITLE"))
                .or_else(|| item.get("title"))
                .or_else(|| item.get("title_short"))
                .or_else(|| item.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = if version.is_empty() {
                base.clone()
            } else {
                format!("{base} {version}")
            };
            let artists = match query_type {
                DownloadType::track => item
                    .get("ARTISTS")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|a| {
                                a.get("ART_NAME")
                                    .and_then(|n| n.as_str())
                                    .map(|s| s.to_string())
                            })
                            .collect::<Vec<_>>()
                    })
                    .or_else(|| {
                        item.get("ART_NAME")
                            .and_then(|v| v.as_str())
                            .map(|s| vec![s.to_string()])
                    })
                    .or_else(|| {
                        item.get("artist")
                            .and_then(|a| a.get("name"))
                            .and_then(|v| v.as_str())
                            .map(|s| vec![s.to_string()])
                    }),
                _ => item
                    .get("ART_NAME")
                    .or_else(|| item.get("PARENT_USERNAME"))
                    .and_then(|v| v.as_str())
                    .map(|s| vec![s.to_string()]),
            };
            // Year: gw-light date first, then public `album.release_date` (mirrors Python).
            let year = item
                .get("PHYSICAL_RELEASE_DATE")
                .and_then(|v| v.as_str())
                .and_then(|s| s.split('-').next())
                .map(|s| s.to_string())
                .or_else(|| {
                    item.get("album")
                        .and_then(|a| a.get("release_date"))
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.split('-').next())
                        .map(|s| s.to_string())
                });
            // Album title for track `additional` (mirrors Python).
            let additional = if query_type == DownloadType::track {
                item.get("ALB_TITLE")
                    .or_else(|| item.get("album").and_then(|a| a.get("title")))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| vec![s.to_string()])
            } else {
                None
            };
            out.push(SearchResult {
                result_id: id.unwrap_or_default(),
                name: Some(name),
                artists,
                duration: item
                    .get("DURATION")
                    .and_then(|v| v.as_u64())
                    .map(|i| i as u32)
                    .or_else(|| {
                        item.get("DURATION")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse::<u32>().ok())
                    })
                    .or_else(|| {
                        item.get("duration")
                            .and_then(|v| v.as_u64())
                            .map(|i| i as u32)
                    }),
                explicit: item
                    .get("EXPLICIT_LYRICS")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "1")
                    .or_else(|| item.get("explicit_lyrics").and_then(|v| v.as_bool())),
                year,
                image_url: item
                    .get("ALB_PICTURE")
                    .and_then(|v| v.as_str())
                    .map(|p| Self::image_url(p, "cover", ImageFileType::Jpg, 56, 80))
                    .or_else(|| {
                        item.get("ART_PICTURE")
                            .and_then(|v| v.as_str())
                            .map(|p| Self::image_url(p, "artist", ImageFileType::Jpg, 56, 80))
                    })
                    .or_else(|| {
                        item.get("PLAYLIST_PICTURE")
                            .and_then(|v| v.as_str())
                            .map(|p| Self::image_url(p, "playlist", ImageFileType::Jpg, 56, 80))
                    })
                    .or_else(|| {
                        item.get("picture_medium")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    }),
                preview_url: item
                    .get("preview")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                additional,
                ..Default::default()
            });
        }
        out
    }

    async fn get_track_info_public(
        &self,
        track_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        let t = if let Some(v) = data.get(track_id).cloned() {
            v
        } else {
            self.session.lock().await.get_track_public(track_id).await?
        };
        let album = t.get("album").cloned().unwrap_or(Value::Null);
        let release_date = album
            .get("release_date")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Title mirrors Python: `title or title_short`, plus `title_version` suffix.
        let base = t
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| t.get("title_short").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        let title = match t
            .get("title_version")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            Some(ver) => format!("{base} {ver}"),
            None => base,
        };
        let artist = t
            .get("artist")
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let artist_id = t.get("artist").and_then(|a| a.get("id")).and_then(json_id);
        let cover_url = album
            .get("cover_big")
            .and_then(|v| v.as_str())
            .or_else(|| album.get("cover_medium").and_then(|v| v.as_str()))
            .or_else(|| album.get("cover_small").and_then(|v| v.as_str()))
            .unwrap_or("")
            .trim()
            .to_string();
        let parse_num = |v: &Value| -> Option<u32> {
            v.as_u64()
                .map(|i| i as u32)
                .or_else(|| v.as_str().and_then(|s| s.parse::<u32>().ok()))
        };
        let mut tags = Tags {
            track_number: t.get("track_position").and_then(parse_num),
            disc_number: t.get("disk_number").and_then(parse_num),
            track_url: Some(format!("https://www.deezer.com/track/{track_id}")),
            release_date: Some(release_date.clone()),
            isrc: t
                .get("isrc")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            ..Default::default()
        };
        if let Some(alb) = data.get("alb_tags") {
            Self::apply_alb_tags(&mut tags, alb);
        }
        // Extract genres from track data if not already provided by album tags.
        if tags.genres.is_none() {
            if let Some(arr) = t
                .get("genres")
                .and_then(|g| g.get("data"))
                .and_then(|v| v.as_array())
            {
                let g: Vec<String> = arr
                    .iter()
                    .filter_map(|x| {
                        x.get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    })
                    .collect();
                if !g.is_empty() {
                    tags.genres = Some(g);
                }
            }
        }
        Ok(TrackInfo {
            id: Some(track_id.to_string()),
            name: title,
            album_id: album.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string()).unwrap_or_default(),
            album: album.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            artists: vec![artist],
            tags,
            codec: CodecFlags::MP3,
            cover_url,
            preview_url: t.get("preview").and_then(|v| v.as_str()).map(|s| s.to_string()),
            release_year: release_date.split('-').next().and_then(|s| s.parse::<i32>().ok()).unwrap_or(0),
            duration: t.get("duration").and_then(|v| v.as_u64()).map(|i| i as u32),
            explicit: t.get("explicit_lyrics").and_then(|v| v.as_bool()),
            artist_id,
            bit_depth: Some(16),
            sample_rate: Some(44.1),
            bitrate: Some(128),
            error: Some("Deezer credentials are required for downloading. Please fill in either email and password, or arl in the settings.".to_string()),
            ..Default::default()
        })
    }

    async fn get_album_info_public(&self, album_id: &str) -> Result<AlbumInfo> {
        let raw = self.session.lock().await.get_album_public(album_id).await?;
        let tracks_data = raw
            .get("tracks")
            .and_then(|t| t.get("data"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let track_ids: Vec<TrackRef> = tracks_data
            .iter()
            .filter_map(|t| {
                t.get("id")
                    .and_then(|v| v.as_i64())
                    .map(|i| TrackRef::Id(i.to_string()))
            })
            .collect();
        let release_date = raw
            .get("release_date")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let artist = raw
            .get("artist")
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let cover_url = raw
            .get("cover_big")
            .and_then(|v| v.as_str())
            .or_else(|| raw.get("cover_medium").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        Ok(AlbumInfo {
            id: Some(album_id.to_string()),
            name: raw
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artist: artist.clone(),
            artist_id: raw
                .get("artist")
                .and_then(|a| a.get("id"))
                .and_then(|v| v.as_i64())
                .map(|i| i.to_string()),
            tracks: track_ids,
            release_year: release_date
                .split('-')
                .next()
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0),
            cover_url: Some(cover_url),
            album_artist: Some(MultiArtist::Single(artist)),
            ..Default::default()
        })
    }

    async fn get_playlist_info_public(&self, playlist_id: &str) -> Result<PlaylistInfo> {
        let client = self.session.lock().await.client.clone();
        let resp = client
            .get(&format!("https://api.deezer.com/playlist/{playlist_id}"))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer playlist public: {e}")))?;
        let raw: Value = resp
            .json()
            .await
            .map_err(|e| Error::Other(format!("Deezer playlist JSON: {e}")))?;
        // paginate tracks
        let mut all_tracks = Vec::new();
        let mut url_opt: Option<String> = Some(format!(
            "https://api.deezer.com/playlist/{playlist_id}/tracks?index=0&limit=100"
        ));
        while let Some(u) = url_opt {
            let r = client
                .get(&u)
                .send()
                .await
                .map_err(|e| Error::Other(format!("Deezer playlist tracks: {e}")))?;
            let v: Value = r
                .json()
                .await
                .map_err(|e| Error::Other(format!("Deezer playlist tracks JSON: {e}")))?;
            let batch = v
                .get("data")
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default();
            if batch.is_empty() {
                break;
            }
            all_tracks.extend(batch);
            url_opt = v
                .get("next")
                .and_then(|n| n.as_str())
                .map(|s| s.to_string());
            if all_tracks.len() > 10000 {
                break;
            }
        }
        let track_ids: Vec<TrackRef> = all_tracks
            .iter()
            .filter_map(|t| {
                t.get("id")
                    .and_then(|v| v.as_i64())
                    .map(|i| TrackRef::Id(i.to_string()))
            })
            .collect();
        let user = raw.get("user").cloned().unwrap_or(Value::Null);
        let cover_url = raw
            .get("picture_xl")
            .and_then(|v| v.as_str())
            .or_else(|| raw.get("picture_big").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        Ok(PlaylistInfo {
            id: Some(playlist_id.to_string()),
            name: raw
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            creator: user
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            creator_id: user
                .get("id")
                .and_then(|v| v.as_i64())
                .map(|i| i.to_string()),
            tracks: track_ids,
            release_year: 0,
            cover_url: Some(cover_url),
            cover_type: Some(ImageFileType::Jpg),
            description: raw
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            ..Default::default()
        })
    }

    async fn get_artist_info_public(
        &self,
        artist_id: &str,
        artist_name: Option<&str>,
    ) -> Result<ArtistInfo> {
        let client = self.session.lock().await.client.clone();
        let artist: Value = client
            .get(&format!("https://api.deezer.com/artist/{artist_id}"))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer artist: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Other(format!("Deezer artist JSON: {e}")))?;
        let name = artist_name.map(|s| s.to_string()).unwrap_or_else(|| {
            artist
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        });
        let albums_resp: Value = client
            .get(&format!(
                "https://api.deezer.com/artist/{artist_id}/albums?index=0&limit=100"
            ))
            .send()
            .await
            .map_err(|e| Error::Other(format!("Deezer artist albums: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Other(format!("Deezer artist albums JSON: {e}")))?;
        let albums: Vec<Value> = albums_resp.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default()
            .iter().map(|a| json!({
                "id": a.get("id").and_then(|v| v.as_i64()).map(|i| i.to_string()).unwrap_or_default(),
                "name": a.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "artist": name,
                "release_year": a.get("release_date").and_then(|v| v.as_str()).and_then(|s| s.split('-').next()).map(|s| s.to_string()),
                "cover_url": a.get("cover_medium").and_then(|v| v.as_str()).unwrap_or(""),
            })).collect();
        Ok(ArtistInfo {
            name,
            artist_id: Some(artist_id.to_string()),
            albums,
            ..Default::default()
        })
    }
}

fn extension_for(format: &str) -> &'static str {
    match format {
        "FLAC" => "flac",
        _ => "mp3",
    }
}

/// Extract an ID that may be an int or a numeric string: the gw-light
/// schema mixes both, the public API uses ints (`str(i.get('id'))` in Python).
fn json_id(v: &Value) -> Option<String> {
    v.as_i64()
        .map(|i| i.to_string())
        .or_else(|| v.as_u64().map(|i| i.to_string()))
        .or_else(|| v.as_str().map(|s| s.to_string()))
}

fn decrypt_deezer_stripe(cipher: &[u8], track_id: &str, bf_secret: &str) -> Vec<u8> {
    use blowfish::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
    type BlowfishCbcDec = cbc::Decryptor<blowfish::Blowfish>;
    let digest = md5_hex(track_id.as_bytes());
    let bytes = digest.as_bytes();
    let secret = bf_secret.as_bytes();
    let mut key = [0u8; 16];
    for i in 0..16 {
        key[i] = bytes[i] ^ bytes[i + 16] ^ secret[i % secret.len().max(1)];
    }
    let iv = [0u8; 8];
    let mut out = Vec::with_capacity(cipher.len());
    let mut index = 0;
    for chunk_bytes in cipher.chunks(2048) {
        let n = chunk_bytes.len();
        let mut buf = chunk_bytes.to_vec();
        if n == 2048 && index % 3 == 0 {
            if let Ok(dec) = BlowfishCbcDec::new_from_slices(&key, &iv) {
                if let Ok(p) = dec.decrypt_padded_mut::<NoPadding>(&mut buf) {
                    out.extend_from_slice(p);
                } else {
                    out.extend_from_slice(&buf);
                }
            } else {
                out.extend_from_slice(&buf);
            }
        } else {
            out.extend_from_slice(&buf);
        }
        index += 1;
    }
    out
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
