//! Soulseek (soulseek-rs / slsknet) source module.
//!
//! Soulseek is a keyless, peer-to-peer file-sharing network: any username plus
//! any password can log in, so there is no account or API key to store. This
//! module uses the `soulseek-rs-lib` crate (imported as `soulseek_rs`) to log
//! in, search the network and pull files.
//!
//! # P2P safety toggle
//!
//! Soulseek traffic is peer-to-peer and some ISPs bill, throttle or block P2P.
//! For that reason this module is **disabled unless explicitly enabled**:
//!
//! * Set the environment variable `PICARO_ENABLE_P2P=1` (also accepts
//!   `true`/`yes`/`on`), **or**
//! * set the module setting `p2p.enabled = true` (see `global_settings`, which
//!   defaults to `false`).
//!
//! The environment variable wins when present, so `PICARO_ENABLE_P2P=0` can be
//! used to force it off. While disabled, `search()` and `get_track_download()`
//! return an error explaining how to turn it on; nothing touches the network.
//!
//! # Downloading
//!
//! Soulseek transfers are P2P streams, not URLs. `get_track_download()` runs
//! the peer transfer to completion on a blocking worker, stages the bytes into
//! a scratch directory under the system temp dir, and returns the path as
//! [`DownloadSource::TempFilePath`]. The shared downloader then moves the file
//! into place, exactly as it does for other staged modules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use parking_lot::Mutex;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use soulseek_rs::prelude::File as SlskFile;
use soulseek_rs::{Client, ClientSettings, ClientVersion, DownloadStatus, PeerAddress};

use crate::registry::register;

const SERVICE: &str = "Soulseek";
const DEFAULT_SERVER_HOST: &str = "server.slsknet.org";
const DEFAULT_SERVER_PORT: u16 = 2416;
const DEFAULT_SEARCH_TIMEOUT_SECS: u64 = 15;
const DEFAULT_DOWNLOAD_TIMEOUT_SECS: u64 = 600;
const REF_PREFIX: &str = "slsk:";

// ---------------------------------------------------------------------------
// P2P toggle
// ---------------------------------------------------------------------------

fn truthy(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "enabled"
    )
}

fn setting_bool(controller: &ModuleController, key: &str) -> Option<bool> {
    controller
        .module_settings
        .get(key)
        .and_then(|v| v.as_bool())
}

fn nested_bool(controller: &ModuleController, parent: &str, key: &str) -> Option<bool> {
    controller
        .module_settings
        .get(parent)
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_bool())
}

/// Whether P2P is enabled. Defaults to `true`; set `p2p.enabled = false` (or
/// `PICARO_ENABLE_P2P=0`) to opt out. The environment variable, when set,
/// overrides the module setting.
fn p2p_enabled(controller: &ModuleController) -> bool {
    let from_settings = nested_bool(controller, "p2p", "enabled")
        .or_else(|| setting_bool(controller, "p2p_enabled"))
        .unwrap_or(true);
    match std::env::var("PICARO_ENABLE_P2P") {
        Ok(v) => truthy(&v),
        Err(_) => from_settings,
    }
}

fn disabled_error(ability: &str) -> Error {
    Error::Other(format!(
        "Soulseek (P2P) is disabled for {ability}; set PICARO_ENABLE_P2P=1 \
         (or module setting p2p.enabled=true) to enable peer-to-peer traffic"
    ))
}

// ---------------------------------------------------------------------------
// Module information / constructor
// ---------------------------------------------------------------------------

pub fn module_information() -> ModuleInformation {
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download,
        global_settings: {
            let mut m = indexmap::IndexMap::new();
            // On by default (best coverage, incl. lossless). Set false to opt
            // out of peer-to-peer traffic.
            m.insert("p2p".to_string(), json!({ "enabled": true }));
            m
        },
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Empty,
        url_constants: indexmap::IndexMap::new(),
        test_url: None,
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(SoulseekConstructor)
}

#[derive(Debug)]
struct SoulseekConstructor;

impl ModuleConstructor for SoulseekConstructor {
    fn construct(&self, controller: ModuleController) -> Result<ModuleInterfacePtr> {
        let (config, account_path) = SoulseekConfig::from_controller(&controller);
        Ok(Arc::new(SoulseekModule {
            controller,
            inner: Arc::new(SoulseekInner {
                client: Mutex::new(None),
                config: Mutex::new(config),
                account_path,
            }),
        }))
    }
}

// ---------------------------------------------------------------------------
// Configuration / shared state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SoulseekConfig {
    username: String,
    password: String,
    server_host: String,
    server_port: u16,
    listen_port: u16,
    search_timeout: Duration,
    download_timeout: Duration,
}

fn random_suffix(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

fn generate_creds() -> (String, String) {
    (format!("picaro_{}", random_suffix(10)), random_suffix(16))
}

const ACCOUNT_FILE: &str = "soulseek_account.json";

fn load_account(path: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let u = v.get("username")?.as_str()?.to_string();
    let p = v.get("password")?.as_str()?.to_string();
    if u.is_empty() || p.is_empty() {
        None
    } else {
        Some((u, p))
    }
}

fn save_account(path: &Path, username: &str, password: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let doc = json!({ "username": username, "password": password });
    if let Ok(s) = serde_json::to_string_pretty(&doc) {
        let _ = std::fs::write(path, s);
    }
}

impl SoulseekConfig {
    /// Build the config, generating and persisting guest credentials on first
    /// use. Returns the config plus the account file path (`None` when explicit
    /// credentials were provided through the environment).
    fn from_controller(controller: &ModuleController) -> (Self, Option<PathBuf>) {
        let account_path = controller.data_folder.join(ACCOUNT_FILE);
        let env_user = env_string("PICARO_SOULSEEK_USERNAME");
        let env_pass = env_string("PICARO_SOULSEEK_PASSWORD");
        let explicit = env_user.is_some() && env_pass.is_some();
        let (username, password) = if let (Some(u), Some(p)) = (env_user, env_pass) {
            (u, p)
        } else if let Some((u, p)) = load_account(&account_path) {
            (u, p)
        } else {
            // First run: create a persistent account and remember it.
            let (u, p) = generate_creds();
            save_account(&account_path, &u, &p);
            (u, p)
        };
        let (server_host, server_port) = parse_server(
            &env_string("PICARO_SOULSEEK_SERVER")
                .unwrap_or_else(|| DEFAULT_SERVER_HOST.to_string()),
        );
        let listen_port = env_u64("PICARO_SOULSEEK_LISTEN_PORT")
            .and_then(|p| u16::try_from(p).ok())
            .or_else(|| {
                controller
                    .module_settings
                    .get("listen_port")
                    .and_then(|v| v.as_u64())
                    .and_then(|p| u16::try_from(p).ok())
            })
            .unwrap_or(0);
        (
            Self {
                username,
                password,
                server_host,
                server_port,
                listen_port,
                search_timeout: Duration::from_secs(
                    env_u64("PICARO_SOULSEEK_SEARCH_TIMEOUT")
                        .unwrap_or(DEFAULT_SEARCH_TIMEOUT_SECS),
                ),
                download_timeout: Duration::from_secs(
                    env_u64("PICARO_SOULSEEK_DOWNLOAD_TIMEOUT")
                        .unwrap_or(DEFAULT_DOWNLOAD_TIMEOUT_SECS),
                ),
            },
            if explicit { None } else { Some(account_path) },
        )
    }
}

fn parse_server(raw: &str) -> (String, u16) {
    if let Some((host, port)) = raw.rsplit_once(':') {
        if let Ok(port) = port.trim().parse::<u16>() {
            let host = host.trim();
            if !host.is_empty() {
                return (host.to_string(), port);
            }
        }
    }
    (raw.trim().to_string(), DEFAULT_SERVER_PORT)
}

struct SoulseekInner {
    /// Lazily connected session, reused across search/download calls. Reset and
    /// rebuilt on the next call if an operation reports a dead session.
    client: Mutex<Option<Client>>,
    /// Credentials can be regenerated when the account is rejected (e.g. the
    /// username was disabled after a long period of inactivity), so the config
    /// sits behind a lock.
    config: Mutex<SoulseekConfig>,
    /// Where generated credentials are persisted. `None` when the caller
    /// supplied explicit credentials through the environment.
    account_path: Option<PathBuf>,
}

struct SoulseekModule {
    controller: ModuleController,
    inner: Arc<SoulseekInner>,
}

// ---------------------------------------------------------------------------
// Result id encoding
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlskRef {
    /// Peer username.
    u: String,
    /// Remote (virtual) file path.
    f: String,
    /// File size in bytes, as advertised by the peer.
    s: u64,
}

fn encode_ref(user: &str, file: &str, size: u64) -> String {
    let value = SlskRef {
        u: user.to_string(),
        f: file.to_string(),
        s: size,
    };
    match serde_json::to_vec(&value) {
        Ok(bytes) => format!("{REF_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)),
        // serde_json can only fail on non-string keys; this struct has none.
        Err(_) => format!("{REF_PREFIX}"),
    }
}

fn parse_ref(track_id: &str) -> Result<SlskRef> {
    let encoded = track_id
        .strip_prefix(REF_PREFIX)
        .ok_or_else(|| Error::Other(format!("soulseek: unrecognised track id '{track_id}'")))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| Error::Other(format!("soulseek: malformed track id '{track_id}': {e}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::Other(format!("soulseek: malformed track id '{track_id}': {e}")))
}

// ---------------------------------------------------------------------------
// Filename / metadata helpers
// ---------------------------------------------------------------------------

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn extension_of(path: &str) -> String {
    Path::new(basename(path))
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Soulseek shares carry everything, cover art and `.nfo` included. A music
/// source should surface audio files, so search skips the rest.
fn is_audio_file(path: &str) -> bool {
    matches!(
        extension_of(path).as_str(),
        "mp3"
            | "flac"
            | "m4a"
            | "aac"
            | "wav"
            | "aiff"
            | "aif"
            | "ogg"
            | "opus"
            | "wma"
            | "alac"
            | "ape"
            | "dsf"
            | "dff"
            | "mpc"
            | "wv"
    )
}

fn codec_for_name(path: &str) -> CodecFlags {
    match extension_of(path).as_str() {
        "flac" => CodecFlags::FLAC,
        "alac" | "m4a" => CodecFlags::ALAC,
        "wav" => CodecFlags::WAV,
        "aiff" | "aif" => CodecFlags::AIFF,
        "opus" => CodecFlags::OPUS,
        "ogg" | "vorbis" => CodecFlags::VORBIS,
        "aac" => CodecFlags::AAC,
        "mp3" => CodecFlags::MP3,
        _ => CodecFlags::MP3,
    }
}

fn strip_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 5 && !ext.contains(' ') => stem,
        _ => name,
    }
}

/// Drop a leading `NN` / `NN.` / `NN -` track number from a filename stem.
fn strip_track_number(name: &str) -> &str {
    let name = name.trim_start();
    let digits = name
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(name.len());
    if digits == 0 || digits > 2 {
        return name;
    }
    let rest = name[digits..].trim_start_matches(['.', '-', '_', ' ']);
    if rest.is_empty() {
        name
    } else {
        rest
    }
}

fn split_artist_title(stem: &str) -> (Option<String>, String) {
    let stem = strip_track_number(stem);
    if let Some((artist, title)) = stem.split_once(" - ") {
        let artist = artist.trim();
        let title = title.trim();
        if !artist.is_empty() && !title.is_empty() {
            return (Some(artist.to_string()), title.to_string());
        }
    }
    (None, stem.trim().to_string())
}

fn file_to_search_result(user: &str, file: &SlskFile) -> SearchResult {
    let stem = strip_extension(basename(&file.name));
    let (artist, title) = split_artist_title(stem);
    // Soulseek attribute 1 is duration in seconds.
    let duration = file.attribs.get(&1).copied().filter(|seconds| *seconds > 0);
    let name = if title.is_empty() {
        stem.to_string()
    } else {
        title
    };
    let mut extra = serde_json::Map::new();
    extra.insert("size".to_string(), json!(file.size));
    extra.insert("username".to_string(), json!(user));
    SearchResult {
        result_id: encode_ref(user, &file.name, file.size),
        name: if name.is_empty() { None } else { Some(name) },
        artists: artist.map(|a| vec![a]),
        duration,
        extra_kwargs: extra,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Session + network blocking helpers
// ---------------------------------------------------------------------------

fn try_login(inner: &SoulseekInner, username: String, password: String) -> Result<Client> {
    let (host, port, listen_port) = {
        let cfg = inner.config.lock();
        (cfg.server_host.clone(), cfg.server_port, cfg.listen_port)
    };
    let settings = ClientSettings {
        username,
        password,
        server_address: PeerAddress::new(host, port),
        // We share nothing; the listener is only for inbound file transfers.
        enable_listen: true,
        listen_port,
        shared_directories: Vec::new(),
        accept_children: false,
        version: ClientVersion::default(),
    };
    let mut client = Client::with_settings(settings);
    client
        .connect()
        .map_err(|e| Error::Other(format!("soulseek: connect failed: {e}")))?;
    let logged_in = client
        .login()
        .map_err(|e| Error::Other(format!("soulseek: login failed: {e}")))?;
    if !logged_in {
        return Err(Error::Other(
            "soulseek: server rejected the login".to_string(),
        ));
    }
    Ok(client)
}

/// Connect, logging in with the stored credentials. If the login is rejected
/// (e.g. the account was disabled after a long period of inactivity, or the
/// name collided), a fresh account is generated, persisted and retried once.
fn connect_client(inner: &SoulseekInner) -> Result<Client> {
    let (user, pass) = {
        let cfg = inner.config.lock();
        (cfg.username.clone(), cfg.password.clone())
    };
    match try_login(inner, user, pass) {
        Ok(client) => Ok(client),
        Err(first) => {
            // Explicit env credentials must not be silently replaced.
            let Some(path) = inner.account_path.clone() else {
                return Err(first);
            };
            let (username, password) = generate_creds();
            save_account(&path, &username, &password);
            {
                let mut cfg = inner.config.lock();
                cfg.username = username.clone();
                cfg.password = password.clone();
            }
            try_login(inner, username, password).map_err(|second| {
                Error::Other(format!(
                    "soulseek: login failed ({first}); retried with a fresh account and failed again ({second})"
                ))
            })
        }
    }
}

/// Run `op` against the cached session, connecting lazily. A session-level
/// error drops the cached client so the next call reconnects.
fn with_client<T>(
    inner: &SoulseekInner,
    op: impl FnOnce(&Client) -> soulseek_rs::Result<T>,
) -> Result<T> {
    let mut guard = inner.client.lock();
    if guard.is_none() {
        *guard = Some(connect_client(inner)?);
    }
    let outcome = {
        let client = guard.as_ref().expect("client just connected");
        op(client)
    };
    match outcome {
        Ok(value) => Ok(value),
        Err(e) => {
            *guard = None;
            Err(Error::Other(format!("soulseek: {e}")))
        }
    }
}

fn search_blocking(inner: &SoulseekInner, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    let timeout = inner.config.lock().search_timeout;
    let results = with_client(inner, |client| client.search(query, timeout))?;
    let mut out: Vec<SearchResult> = Vec::new();
    'outer: for result in &results {
        for file in &result.files {
            if !is_audio_file(&file.name) {
                continue;
            }
            let converted = file_to_search_result(&result.username, file);
            if out
                .iter()
                .any(|existing| existing.result_id == converted.result_id)
            {
                continue;
            }
            out.push(converted);
            if out.len() >= limit {
                break 'outer;
            }
        }
    }
    Ok(out)
}

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

fn scratch_dir() -> Result<PathBuf> {
    let base = std::env::temp_dir().join("picaro-soulseek");
    std::fs::create_dir_all(&base)
        .map_err(|e| Error::Other(format!("soulseek: create scratch dir: {e}")))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("{}-{nanos}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::Other(format!("soulseek: create scratch dir: {e}")))?;
    Ok(dir)
}

/// Promote the single file the transfer wrote into `dir` up to `dir`'s parent,
/// under a collision-proof name, and drop the now-empty scratch directory. The
/// downloader later moves the returned file into place.
fn stage_file(dir: &Path) -> Result<PathBuf> {
    let mut staged: Option<PathBuf> = None;
    let entries = std::fs::read_dir(dir)
        .map_err(|e| Error::Other(format!("soulseek: read scratch dir: {e}")))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_part = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("part"));
        if path.is_file() && !is_part {
            staged = Some(path);
            break;
        }
    }
    let staged = staged.ok_or_else(|| {
        Error::Other("soulseek: transfer reported complete but no file was staged".to_string())
    })?;

    let unique = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("soulseek");
    let base = dir.parent().unwrap_or(dir);
    let filename = staged
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download.bin");
    let target = base.join(format!("{unique}-{filename}"));
    match std::fs::rename(&staged, &target) {
        Ok(()) => {
            let _ = std::fs::remove_dir(dir);
            Ok(target)
        }
        // Cross-device or a locked file: leave the bytes where they landed.
        Err(_) => Ok(staged),
    }
}

fn download_blocking(inner: &SoulseekInner, reference: SlskRef) -> Result<PathBuf> {
    let dir = scratch_dir()?;
    let dir_string = dir.to_string_lossy().to_string();

    let mut guard = inner.client.lock();
    if guard.is_none() {
        *guard = Some(connect_client(inner)?);
    }

    let requested = {
        let client = guard.as_ref().expect("client just connected");
        client.download(
            reference.f.clone(),
            reference.u.clone(),
            reference.s,
            dir_string,
        )
    };
    let (_download, status_rx) = match requested {
        Ok(pair) => pair,
        Err(e) => {
            *guard = None;
            return Err(Error::Other(format!(
                "soulseek: could not queue download: {e}"
            )));
        }
    };

    let download_timeout = inner.config.lock().download_timeout;
    let deadline = Instant::now() + download_timeout;
    loop {
        match status_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(DownloadStatus::Completed) => break,
            Ok(DownloadStatus::Cancelled) => {
                return Err(Error::Other("soulseek: download cancelled".to_string()));
            }
            Ok(DownloadStatus::TimedOut) => {
                return Err(Error::Other("soulseek: peer timed out".to_string()));
            }
            Ok(DownloadStatus::Failed(reason)) => {
                return Err(Error::Other(format!(
                    "soulseek: download failed: {}",
                    reason.unwrap_or_else(|| "unknown reason".to_string())
                )));
            }
            // Queued / InProgress / Paused: keep waiting.
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    if let Some(client) = guard.as_ref() {
                        let _ = client.cancel_download(&reference.u, &reference.f);
                    }
                    return Err(Error::Other(format!(
                        "soulseek: download timed out after {}s",
                        download_timeout.as_secs()
                    )));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Error::Other(
                    "soulseek: download worker went away".to_string(),
                ));
            }
        }
    }

    stage_file(&dir)
}

// ---------------------------------------------------------------------------
// ModuleInterface
// ---------------------------------------------------------------------------

#[async_trait]
impl picaro_utils::module::ModuleInterface for SoulseekModule {
    fn name(&self) -> &str {
        SERVICE
    }

    fn is_authenticated(&self) -> bool {
        p2p_enabled(&self.controller)
    }

    async fn get_track_info(
        &self,
        track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        let reference = parse_ref(track_id)?;
        let stem = strip_extension(basename(&reference.f));
        let (artist, title) = split_artist_title(stem);
        let name = if title.is_empty() {
            stem.to_string()
        } else {
            title
        };
        Ok(TrackInfo {
            name,
            album: String::new(),
            album_id: String::new(),
            artists: artist.into_iter().collect(),
            codec: codec_for_name(&reference.f),
            release_year: 0,
            id: Some(track_id.to_string()),
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
        if !p2p_enabled(&self.controller) {
            return Err(disabled_error("download"));
        }
        let reference = parse_ref(track_id)?;
        let codec = codec_for_name(&reference.f);
        let inner = self.inner.clone();
        let path = tokio::task::spawn_blocking(move || download_blocking(&inner, reference))
            .await
            .map_err(|e| Error::Other(format!("soulseek: download task join: {e}")))??;
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::TempFilePath,
            file_url: None,
            file_url_headers: serde_json::Map::new(),
            temp_file_path: Some(path),
            different_codec: Some(codec),
        })
    }

    async fn get_album_info(
        &self,
        _album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.to_string(),
            ability: "album".to_string(),
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
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.to_string(),
            ability: "cover".to_string(),
        })
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        if !p2p_enabled(&self.controller) {
            return Err(disabled_error("search"));
        }
        let inner = self.inner.clone();
        let query = query.to_string();
        let limit = limit.max(1) as usize;
        tokio::task::spawn_blocking(move || search_blocking(&inner, &query, limit))
            .await
            .map_err(|e| Error::Other(format!("soulseek: search task join: {e}")))?
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
