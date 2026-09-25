//! Musixmatch module - port of `modules/musixmatch/interface.py` +
//! `musixmatch_api.py`.
//!
//! The Python implementation fetches `user_token`s from
//! `apic-desktop.musixmatch.com` and solves CAPTCHAs via
//! `https://apic.musixmatch.com/captcha.html?callback_url=mxm://captcha`.
//! There is no headless CAPTCHA solver in this workspace, so this port reuses
//! stored `user_tokens` (global storage, as in Python) and returns a clear,
//! actionable error when no usable token exists instead of panicking.
//! Lyrics fetching/parsing (track.get, macro.subtitles.get, richsync /
//! subtitle / plain bodies, timestamp formatting) is a 1:1 port.

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

use crate::registry::register;

const API_URL: &str = "https://apic-desktop.musixmatch.com/ws/1.1/";
const CAPTCHA_URL: &str = "https://apic.musixmatch.com/captcha.html?callback_url=mxm://captcha";

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: "Musixmatch".to_string(),
        module_supported_modes: ModuleModes::lyrics,
        global_settings: {
            let mut m = indexmap::IndexMap::new();
            m.insert("token_limit".to_string(), json!(10));
            m.insert("lyrics_format".to_string(), json!("standard"));
            m.insert("custom_time_decimals".to_string(), json!(false));
            m
        },
        global_storage_variables: vec!["user_tokens".to_string()],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("musixmatch".to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: None,
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(MusixmatchConstructor)
}

#[derive(Debug)]
struct MusixmatchConstructor;

impl ModuleConstructor for MusixmatchConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        let lyrics_format = controller
            .module_settings
            .get("lyrics_format")
            .and_then(|v| v.as_str())
            .unwrap_or("standard")
            .to_string();
        let custom_time_decimals = controller
            .module_settings
            .get("custom_time_decimals")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // Stored tokens, mirroring Python's temporary_settings_controller read.
        let user_tokens: Vec<String> = controller
            .temporary_settings_controller
            .read("user_tokens", TempSettingType::Global)
            .ok()
            .flatten()
            .and_then(|v| v.as_array().cloned())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Arc::new(MusixmatchModule {
            controller,
            state: Mutex::new(MusixmatchState {
                client: picaro_utils::http::build_client(None),
                user_tokens,
                token_index: 0,
                lyrics_format,
                custom_time_decimals,
            }),
        }))
    }
}

#[derive(Debug)]
struct MusixmatchState {
    client: reqwest::Client,
    user_tokens: Vec<String>,
    token_index: usize,
    lyrics_format: String,
    custom_time_decimals: bool,
}

#[derive(Debug)]
struct MusixmatchModule {
    #[allow(dead_code)]
    controller: ModuleController,
    state: Mutex<MusixmatchState>,
}

impl MusixmatchModule {
    fn current_token(state: &MusixmatchState) -> Result<String> {
        state
            .user_tokens
            .get(state.token_index)
            .cloned()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                Error::Other(format!(
                    "Musixmatch needs a user token (none stored). Open {CAPTCHA_URL} to solve the captcha, then retry. Token auto-fetch is not ported to Rust."
                ))
            })
    }

    fn rotate_token(state: &mut MusixmatchState) {
        if state.user_tokens.is_empty() {
            return;
        }
        state.token_index = (state.token_index + 1) % state.user_tokens.len();
    }

    async fn api_get(
        client: &reqwest::Client,
        token: &str,
        method: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<Value>> {
        let mut params: Vec<(String, String)> = vec![
            ("usertoken".to_string(), token.to_string()),
            ("app_id".to_string(), "web-desktop-app-v1.0".to_string()),
        ];
        for (k, v) in query {
            params.push((k.to_string(), v.to_string()));
        }
        let resp: Value = client
            .get(format!("{API_URL}{method}"))
            .query(&params)
            .header("Connection", "Keep-Alive")
            .header("User-Agent", "User-Agent: Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Musixmatch/0.19.4 Chrome/58.0.3029.110 Electron/1.7.6 Safari/537.36")
            .send()
            .await
            .map_err(|e| Error::Other(format!("Musixmatch {method}: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Other(format!("Musixmatch {method} JSON: {e}")))?;
        let header = resp
            .pointer("/message/header")
            .cloned()
            .unwrap_or(Value::Null);
        let code = header
            .get("status_code")
            .and_then(|c| c.as_u64())
            .unwrap_or(0);
        let hint = header.get("hint").and_then(|h| h.as_str()).unwrap_or("");
        if code == 401 && hint == "captcha" {
            return Err(Error::Other(format!(
                "Musixmatch captcha required. Open {CAPTCHA_URL} to solve it, then retry."
            )));
        }
        if code != 200 {
            return Ok(None);
        }
        Ok(resp.pointer("/message/body").cloned())
    }

    /// 1:1 port of `format_timestamp` in interface.py.
    fn format_timestamp(seconds: f64, decimal_places: usize) -> String {
        let factor = 10_u32.pow(decimal_places as u32) as f64;
        format!(
            "{:02}:{:02}.{:0width$}",
            (seconds / 60.0).floor() as i64,
            (seconds % 60.0).floor() as i64,
            ((seconds * factor) % factor).floor() as i64,
            width = decimal_places
        )
    }

    fn decimal_places(input: &serde_json::Value) -> usize {
        let s = if let Some(f) = input.as_f64() {
            format!("{f}")
        } else {
            input.to_string()
        };
        s.split('.').nth(1).map(|d| d.len()).unwrap_or(0)
    }

    /// 1:1 port of `parse_rich_sync_lyrics` (enhanced / lyricsx / custom).
    fn parse_rich_sync(
        rich_sync_lyrics: &[Value],
        output_type: &str,
        custom_time_decimals: bool,
    ) -> Result<String> {
        let mut out_lines: Vec<String> = Vec::new();
        match output_type {
            "lyricsx" => {
                for line in rich_sync_lyrics {
                    let ts = line.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let te = line.get("te").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let x = line.get("x").and_then(|v| v.as_str()).unwrap_or("");
                    let mut rl = format!("[{}]", Self::format_timestamp(ts, 3));
                    rl.push_str(&format!(
                        "{x}\n[{}][tt]<0,0>",
                        Self::format_timestamp(ts, 3)
                    ));
                    let words = line
                        .get("l")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let mut char_count = words
                        .first()
                        .and_then(|w| w.get("c"))
                        .and_then(|c| c.as_str())
                        .map(|s| s.len())
                        .unwrap_or(0);
                    for word in words.iter().skip(1) {
                        let c = word.get("c").and_then(|v| v.as_str()).unwrap_or("");
                        if c == " " {
                            continue;
                        }
                        let o = word.get("o").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let word_ts = (o * 1000.0).round() / 1000.0;
                        rl.push_str(&format!("<{},{}>", (word_ts * 1000.0) as i64, char_count));
                        char_count += c.len() + 1;
                    }
                    rl.push_str(&format!(
                        "<{},{}>",
                        ((te - ts - 0.005) * 1000.0) as i64,
                        char_count
                    ));
                    rl.push_str(&format!("<{}>", ((te - ts) * 1000.0) as i64));
                    out_lines.push(rl);
                }
            }
            "enhanced" => {
                let mut rl = format!("[{}]", Self::format_timestamp(0.0, 2));
                for line in rich_sync_lyrics {
                    let ts = line.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let te = line.get("te").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    for word in line
                        .get("l")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default()
                    {
                        let c = word.get("c").and_then(|v| v.as_str()).unwrap_or("");
                        if c == " " {
                            continue;
                        }
                        let o = word.get("o").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let word_ts = ((ts + o) * 100.0).round() / 100.0;
                        rl.push_str(&format!(" <{}> {c}", Self::format_timestamp(word_ts, 2)));
                    }
                    out_lines.push(rl);
                    rl = format!("[{}]", Self::format_timestamp(te, 2));
                }
            }
            "custom" => {
                let max_dp = if custom_time_decimals {
                    let mut m = 0usize;
                    for line in rich_sync_lyrics {
                        m = m.max(Self::decimal_places(line.get("ts").unwrap_or(&Value::Null)));
                        m = m.max(Self::decimal_places(line.get("te").unwrap_or(&Value::Null)));
                        for w in line
                            .get("l")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default()
                        {
                            m = m.max(Self::decimal_places(w.get("o").unwrap_or(&Value::Null)));
                        }
                    }
                    m
                } else {
                    2
                };
                for line in rich_sync_lyrics {
                    let ts = line.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let te = line.get("te").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let mut rl = String::new();
                    for word in line
                        .get("l")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default()
                    {
                        let c = word.get("c").and_then(|v| v.as_str()).unwrap_or("");
                        let o = word.get("o").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let factor = 10_f64.powi(max_dp as i32);
                        let word_ts = ((ts + o) * factor).round() / factor;
                        let tag = Self::format_timestamp(word_ts, max_dp);
                        if rl.is_empty() {
                            rl.push_str(&format!("[{tag}]"));
                        } else {
                            rl.push_str(&format!("<{tag}>"));
                        }
                        rl.push_str(c);
                    }
                    let factor = 10_f64.powi(max_dp as i32);
                    let te_r = (te * factor).round() / factor;
                    rl.push_str(&format!("<{}>", Self::format_timestamp(te_r, max_dp)));
                    out_lines.push(rl);
                }
            }
            other => {
                return Err(Error::Other(format!(
                    "Output type \"{other}\" not supported, choose between standard, enhanced, lyricsx and custom."
                )));
            }
        }
        Ok(out_lines.join("\n"))
    }

    /// 1:1 port of `get_track_lyrics` body parsing.
    fn lyrics_from_blob(
        lyrics: &Value,
        lyrics_format: &str,
        custom_time_decimals: bool,
    ) -> Result<LyricsInfo> {
        let mut synced = None;
        let mut embedded = None;
        if let Some(body) = lyrics.get("richsync_body").and_then(|v| v.as_str()) {
            let parsed: Vec<Value> = serde_json::from_str(body)
                .map_err(|e| Error::Other(format!("Musixmatch richsync JSON: {e}")))?;
            synced = Some(Self::parse_rich_sync(
                &parsed,
                lyrics_format,
                custom_time_decimals,
            )?);
            embedded = Some(
                parsed
                    .iter()
                    .filter_map(|l| l.get("x").and_then(|x| x.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        } else if let Some(body) = lyrics.get("subtitle_body").and_then(|v| v.as_str()) {
            let s = body.replace("] ", "]");
            let re = Regex::new(r"\[[0-9]+:[0-9]+\.[0-9]+]").unwrap();
            embedded = Some(re.replace_all(&s, "").to_string());
            synced = Some(s);
        } else if let Some(body) = lyrics.get("lyrics_body").and_then(|v| v.as_str()) {
            embedded = Some(body.to_string());
        }
        Ok(LyricsInfo { embedded, synced })
    }
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for MusixmatchModule {
    fn name(&self) -> &str {
        "Musixmatch"
    }

    async fn get_track_info(
        &self,
        _track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "Musixmatch".into(),
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
            module: "Musixmatch".into(),
            ability: "download".into(),
        })
    }
    async fn get_album_info(
        &self,
        _album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "Musixmatch".into(),
            ability: "album".into(),
        })
    }
    async fn get_playlist_info(
        &self,
        _playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: "Musixmatch".into(),
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
            module: "Musixmatch".into(),
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
            module: "Musixmatch".into(),
            ability: "cover".into(),
        })
    }

    async fn get_track_lyrics(
        &self,
        _track_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<LyricsInfo> {
        // `lyrics` blob comes from search's extra_kwargs (mirrors Python).
        let lyrics = data
            .get("lyrics")
            .or_else(|| data.get("lyrics_data"))
            .cloned();
        let Some(lyrics) = lyrics else {
            return Ok(LyricsInfo::default());
        };
        let state = self.state.lock().await;
        Self::lyrics_from_blob(&lyrics, &state.lyrics_format, state.custom_time_decimals)
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        _query: &str,
        track_info: Option<&TrackInfo>,
        _limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let Some(ti) = track_info else {
            return Ok(Vec::new());
        };
        let mut state = self.state.lock().await;
        if state.user_tokens.is_empty() {
            return Err(Error::Other(format!(
                "Musixmatch user tokens are empty. Open {CAPTCHA_URL} to solve the captcha; token auto-fetch is not ported to Rust."
            )));
        }
        let lyrics_format = state.lyrics_format.clone();
        let client = state.client.clone();
        // Retry across stored tokens on captcha errors (mirrors Python loop).
        let attempts = state.user_tokens.len();
        let mut last_err: Option<Error> = None;
        for _ in 0..attempts {
            let token = match Self::current_token(&state) {
                Ok(t) => t,
                Err(e) => return Err(e),
            };
            match Self::search_with_token(&client, &token, &lyrics_format, ti).await {
                Ok(Some((track_id, lyrics))) => {
                    let mut extra = serde_json::Map::new();
                    if let Some(l) = lyrics {
                        extra.insert("lyrics".to_string(), l);
                    }
                    return Ok(vec![SearchResult {
                        result_id: track_id,
                        extra_kwargs: extra,
                        ..Default::default()
                    }]);
                }
                Ok(None) => return Ok(Vec::new()),
                Err(e) if e.to_string().contains("captcha") => {
                    last_err = Some(e);
                    Self::rotate_token(&mut state);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            Error::Other(format!(
                "Captcha error could not be solved, open \"{CAPTCHA_URL}\" to solve the captcha"
            ))
        }))
    }
}

impl MusixmatchModule {
    async fn search_with_token(
        client: &reqwest::Client,
        token: &str,
        lyrics_format: &str,
        ti: &TrackInfo,
    ) -> Result<Option<(String, Option<Value>)>> {
        // ISRC fast path first (mirrors Python search).
        if let Some(isrc) = ti.tags.isrc.as_deref().filter(|s| !s.is_empty()) {
            if let Some(body) =
                Self::api_get(client, token, "track.get", &[("track_isrc", isrc)]).await?
            {
                if let Some(track) = body.get("track") {
                    let track_id = track
                        .get("track_id")
                        .and_then(|v| v.as_u64())
                        .map(|n| n.to_string())
                        .unwrap_or_default();
                    let commontrack_id = track
                        .get("commontrack_id")
                        .and_then(|v| v.as_u64())
                        .map(|n| n.to_string())
                        .unwrap_or_default();
                    let mut lyrics = None;
                    if lyrics_format != "standard"
                        && track.get("has_richsync").and_then(|v| v.as_u64()) == Some(1)
                        && !track_id.is_empty()
                    {
                        if let Some(b) = Self::api_get(
                            client,
                            token,
                            "track.richsync.get",
                            &[("track_id", &track_id)],
                        )
                        .await?
                        {
                            lyrics = b.get("richsync").cloned();
                        }
                    }
                    if lyrics.is_none()
                        && track.get("has_subtitles").and_then(|v| v.as_u64()) == Some(1)
                        && !commontrack_id.is_empty()
                    {
                        if let Some(b) = Self::api_get(
                            client,
                            token,
                            "track.subtitle.get",
                            &[("commontrack_id", &commontrack_id)],
                        )
                        .await?
                        {
                            lyrics = b.get("subtitle").cloned();
                        }
                    }
                    if lyrics.is_none()
                        && track.get("has_lyrics").and_then(|v| v.as_u64()) == Some(1)
                        && !track_id.is_empty()
                    {
                        if let Some(b) = Self::api_get(
                            client,
                            token,
                            "track.lyrics.get",
                            &[("track_id", &track_id)],
                        )
                        .await?
                        {
                            lyrics = b.get("lyrics").cloned();
                        }
                    }
                    if lyrics.is_some() {
                        return Ok(Some((track_id, lyrics)));
                    }
                    if !track_id.is_empty() {
                        return Ok(Some((track_id, None)));
                    }
                }
            }
        }
        // Metadata fallback via macro.subtitles.get (mirrors get_lyrics_by_metadata).
        let artist = ti.artists.first().cloned().unwrap_or_default();
        let body = Self::api_get(
            client,
            token,
            "macro.subtitles.get",
            &[
                ("q_artist", artist.as_str()),
                ("q_track", ti.name.as_str()),
                ("q_album", ti.album.as_str()),
                ("format", "json"),
                ("namespace", "lyrics_richsynched"),
                ("optional_calls", "track.richsync"),
            ],
        )
        .await?;
        let Some(body) = body else {
            return Ok(None);
        };
        let macro_calls = body.get("macro_calls").cloned().unwrap_or(Value::Null);
        let track_id = macro_calls
            .pointer("/matcher.track.get/message/body/track/track_id")
            .and_then(|v| v.as_u64())
            .map(|n| n.to_string())
            .unwrap_or_default();
        if macro_calls
            .pointer("/matcher.track.get/message/header/status_code")
            .and_then(|v| v.as_u64())
            != Some(200)
        {
            return Ok(None);
        }
        let mut lyrics = None;
        if lyrics_format != "standard" {
            if let Some(rs) = macro_calls.pointer("/track.richsync.get/message/body/richsync") {
                if macro_calls
                    .pointer("/track.richsync.get/message/header/status_code")
                    .and_then(|v| v.as_u64())
                    == Some(200)
                {
                    lyrics = Some(rs.clone());
                }
            }
        }
        if lyrics.is_none() {
            if let Some(sub) =
                macro_calls.pointer("/track.subtitles.get/message/body/subtitle_list/0/subtitle")
            {
                if macro_calls
                    .pointer("/track.subtitles.get/message/header/status_code")
                    .and_then(|v| v.as_u64())
                    == Some(200)
                {
                    lyrics = Some(sub.clone());
                }
            }
        }
        if lyrics.is_none() {
            if let Some(l) = macro_calls.pointer("/track.lyrics.get/message/body/lyrics") {
                if macro_calls
                    .pointer("/track.lyrics.get/message/header/status_code")
                    .and_then(|v| v.as_u64())
                    == Some(200)
                {
                    lyrics = Some(l.clone());
                }
            }
        }
        if track_id.is_empty() {
            return Ok(None);
        }
        Ok(Some((track_id, lyrics)))
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_format_matches_python() {
        assert_eq!(MusixmatchModule::format_timestamp(65.5, 2), "01:05.50");
        assert_eq!(MusixmatchModule::format_timestamp(0.0, 2), "00:00.00");
        assert_eq!(MusixmatchModule::format_timestamp(61.123, 3), "01:01.123");
    }

    #[test]
    fn subtitle_and_plain_bodies_parse() {
        let sub = json!({"subtitle_body": "[00:01.00]hello[00:02.00]world"});
        let info = MusixmatchModule::lyrics_from_blob(&sub, "standard", false).unwrap();
        assert_eq!(
            info.synced.as_deref(),
            Some("[00:01.00]hello[00:02.00]world")
        );
        assert_eq!(info.embedded.as_deref(), Some("helloworld"));

        let plain = json!({"lyrics_body": "line1\nline2"});
        let info = MusixmatchModule::lyrics_from_blob(&plain, "standard", false).unwrap();
        assert_eq!(info.embedded.as_deref(), Some("line1\nline2"));
        assert!(info.synced.is_none());
    }

    #[test]
    fn unsupported_richsync_format_errors() {
        let rich = json!({"richsync_body": "[]"});
        assert!(MusixmatchModule::lyrics_from_blob(&rich, "bogus", false).is_err());
    }
}
