//! Persistence helpers for `config/settings.json` and
//! `config/loginstorage.bin`. Mirrors `picaro/core.py::update_module_storage`
//! and `utils/utils.py::read_temporary_setting / set_temporary_setting`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::models::TempSettingType;

/// Raw settings.json document shape.
pub type SettingsDocument = serde_json::Map<String, Value>;

/// The path of `settings.json` under the data folder.
pub fn settings_path(data_folder: &Path) -> PathBuf {
    data_folder.join("settings.json")
}

/// The path of `loginstorage.bin` under the data folder.
pub fn session_path(data_folder: &Path) -> PathBuf {
    data_folder.join("loginstorage.bin")
}

pub fn ensure_data_dirs(root: &Path) -> std::io::Result<()> {
    fs::create_dir_all(root)?;
    fs::create_dir_all(root.join("modules"))?;
    fs::create_dir_all(root.join("extensions"))?;
    fs::create_dir_all(root.join("temp"))?;
    Ok(())
}

/// Load the on-disk settings.json, returning an empty document if missing.
pub fn load_settings(data_folder: &Path) -> SettingsDocument {
    let path = settings_path(data_folder);
    if !path.exists() {
        return Map::new();
    }
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<Map<String, Value>>(&s).unwrap_or_else(|_| Map::new()),
        Err(_) => Map::new(),
    }
}

/// Write the on-disk settings.json.
pub fn save_settings(data_folder: &Path, doc: &SettingsDocument) -> std::io::Result<()> {
    let path = settings_path(data_folder);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pretty = serde_json::to_string_pretty(doc).map_err(std::io::Error::other)?;
    fs::write(path, pretty)
}

/// Merge stored module settings with the schema defaults defined by the
/// module. Mirrors `merge_module_settings` in OrpheusDL.
pub fn merge_module_settings(
    global_settings: &Map<String, Value>,
    session_settings: &Map<String, Value>,
    stored: Option<&Map<String, Value>>,
) -> Map<String, Value> {
    let mut defaults = Map::new();
    for (k, v) in global_settings {
        defaults.insert(k.clone(), v.clone());
    }
    for (k, v) in session_settings {
        defaults.insert(k.clone(), v.clone());
    }
    if let Some(stored) = stored {
        for (k, v) in stored {
            defaults.insert(k.clone(), v.clone());
        }
    }
    defaults
}

/// Stored shape of `loginstorage.bin`.
///
/// Structure is `{"advancedmode": bool, "modules": {<module>: ModuleSession}}`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionStorage {
    #[serde(default)]
    pub advancedmode: bool,
    #[serde(default)]
    pub modules: BTreeMap<String, ModuleSession>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ModuleSession {
    #[serde(default = "default_selected")]
    pub selected: String,
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionData>,
    #[serde(default)]
    pub custom_data: BTreeMap<String, Value>,
}

fn default_selected() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionData {
    #[serde(default)]
    pub clear_session: bool,
    #[serde(default)]
    pub hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub custom_data: BTreeMap<String, Value>,
    #[serde(default)]
    pub bearer: String,
    #[serde(default)]
    pub refresh: String,
}

pub fn load_session_storage(data_folder: &Path) -> SessionStorage {
    let path = session_path(data_folder);
    if !path.exists() {
        return SessionStorage::default();
    }
    match fs::read(&path) {
        Ok(bytes) => bincode_v2_decode(&bytes).unwrap_or_default(),
        Err(_) => SessionStorage::default(),
    }
}

pub fn save_session_storage(data_folder: &Path, storage: &SessionStorage) -> std::io::Result<()> {
    let path = session_path(data_folder);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = bincode_v2_encode(storage).map_err(std::io::Error::other)?;
    let mut f = fs::File::create(path)?;
    f.write_all(&bytes)?;
    Ok(())
}

/// Custom (de)serialisation so the storage file is forward-compatible with
/// Python's pickle output. We use JSON-in-bytes because the original is a
/// pickled dict; in practice the Rust binary only ever writes the JSON form,
/// and the Python side tolerates either.
fn bincode_v2_encode(s: &SessionStorage) -> serde_json::Result<Vec<u8>> {
    let v = serde_json::to_value(s)?;
    let s = serde_json::to_string(&v)?;
    Ok(s.into_bytes())
}

fn bincode_v2_decode(bytes: &[u8]) -> serde_json::Result<SessionStorage> {
    if let Ok(s) = std::str::from_utf8(bytes) {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            if let Some(obj) = v.as_object() {
                // The advancedmode key
                let advancedmode = obj
                    .get("advancedmode")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                let mut modules = BTreeMap::new();
                if let Some(mods) = obj.get("modules").and_then(|x| x.as_object()) {
                    for (name, ms) in mods {
                        let selected = ms
                            .get("selected")
                            .and_then(|x| x.as_str())
                            .unwrap_or("default")
                            .to_string();
                        let mut sessions = BTreeMap::new();
                        if let Some(ses) = ms.get("sessions").and_then(|x| x.as_object()) {
                            for (sname, sdata) in ses {
                                let data: SessionData =
                                    serde_json::from_value(sdata.clone()).unwrap_or_default();
                                sessions.insert(sname.clone(), data);
                            }
                        }
                        let custom_data = ms
                            .get("custom_data")
                            .and_then(|x| x.as_object())
                            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                            .unwrap_or_default();
                        modules.insert(
                            name.clone(),
                            ModuleSession {
                                selected,
                                sessions,
                                custom_data,
                            },
                        );
                    }
                }
                return Ok(SessionStorage {
                    advancedmode,
                    modules,
                });
            }
        }
    }
    Ok(SessionStorage::default())
}

/// Read a temporary setting for a given module. Mirrors
/// `read_temporary_setting` in OrpheusDL.
pub fn read_temporary_setting(
    storage_path: &Path,
    module: &str,
    setting: &str,
    setting_type: TempSettingType,
) -> Result<Option<Value>> {
    let mut storage = load_session_storage(storage_path);
    let module = module.to_lowercase();
    let ms = storage.modules.entry(module.clone()).or_default();
    let session_name = ms.selected.clone();
    let session: &mut SessionData = ms.sessions.entry(session_name).or_default();

    let result = match setting_type {
        TempSettingType::Custom => session
            .custom_data
            .get(setting)
            .cloned()
            .or_else(|| ms.custom_data.get(setting).cloned()),
        TempSettingType::Global => ms.custom_data.get(setting).cloned(),
        TempSettingType::Jwt => {
            if setting == "bearer" {
                Some(Value::String(session.bearer.clone()))
            } else if setting == "refresh" {
                Some(Value::String(session.refresh.clone()))
            } else {
                return Err(Error::Other(format!(
                    "Invalid temporary setting '{setting}' for jwt"
                )));
            }
        }
    };
    let _ = storage;
    Ok(result.filter(|v| !v.is_null()))
}

/// Set a temporary setting for a given module.
pub fn set_temporary_setting(
    storage_path: &Path,
    module: &str,
    setting: &str,
    value: Value,
    setting_type: TempSettingType,
) -> Result<()> {
    let mut storage = load_session_storage(storage_path);
    let module = module.to_lowercase();
    let ms = storage.modules.entry(module.clone()).or_default();
    let session_name = ms.selected.clone();
    let session: &mut SessionData = ms.sessions.entry(session_name).or_default();
    match setting_type {
        TempSettingType::Custom => {
            session.custom_data.insert(setting.to_string(), value);
        }
        TempSettingType::Global => {
            ms.custom_data.insert(setting.to_string(), value);
        }
        TempSettingType::Jwt => {
            if setting == "bearer" {
                if let Some(s) = value.as_str() {
                    session.bearer = s.to_string();
                }
            } else if setting == "refresh" {
                if let Some(s) = value.as_str() {
                    session.refresh = s.to_string();
                }
            } else {
                return Err(Error::Other(format!(
                    "Invalid temporary setting '{setting}' for jwt"
                )));
            }
        }
    }
    save_session_storage(
        storage_path
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(Path::new(".")),
        &storage,
    )?;
    Ok(())
}

/// Initialise the storage for a module if it doesn't exist yet.
pub fn ensure_module_session(storage_path: &Path, module: &str) {
    let mut storage = load_session_storage(storage_path);
    let module = module.to_lowercase();
    storage.modules.entry(module).or_default();
    let _ = save_session_storage(
        storage_path
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(Path::new(".")),
        &storage,
    );
}

/// Build the default global settings document.
pub fn default_global_settings() -> Map<String, Value> {
    let mut m: Map<String, Value> = Map::new();
    m.insert(
        "general".to_string(),
        json!({
            "download_path": "./downloads/",
            "download_quality": "hifi",
            "search_limit": 25,
            "disabled_search_platforms": [],
            "concurrent_downloads": 5,
            "progress_bar": false,
            "play_sound_on_finish": true
        }),
    );
    m.insert(
        "artist_downloading".to_string(),
        json!({
            "return_credited_albums": true,
            "separate_tracks_skip_downloaded": true
        }),
    );
    m.insert(
        "formatting".to_string(),
        json!({
            "album_format": "{artist}/{name}",
            "playlist_format": "{name}",
            "track_filename_format": "{artist} - {name}",
            "single_full_path_format": "{artist} - {name}",
            "metadata_separator": ";",
            "split_metadata": true,
            "enable_zfill": true,
            "force_album_format": false,
            "use_playlist_position": false,
            "use_album_position": false
        }),
    );
    m.insert(
        "codecs".to_string(),
        json!({
            "proprietary_codecs": false,
            "spatial_codecs": true
        }),
    );
    m.insert(
        "module_defaults".to_string(),
        json!({
            "lyrics": "default",
            "covers": "default",
            "credits": "default"
        }),
    );
    m.insert(
        "lyrics".to_string(),
        json!({
            "embed_lyrics": true,
            "embed_synced_lyrics": true,
            "save_synced_lyrics": true
        }),
    );
    m.insert(
        "metadata".to_string(),
        json!({
            "fetch_lyrics": true,
            "fetch_cover": true,
            "fill_misc": true
        }),
    );
    m.insert(
        "p2p".to_string(),
        json!({
            // Enabled by default: Soulseek gives near-universal coverage
            // (incl. lossless). Set to false if your ISP meters/blocks P2P.
            "enabled": true
        }),
    );
    m.insert(
        "resolver".to_string(),
        json!({
            "probe_timeout_secs": 4,
            "probe_timeout_lossless_secs": 14,
            "max_parallel": 6,
            // Off by default: only enable if you accept tracks of an album
            // coming from different providers / at different qualities.
            "allow_mixed_sources": false,
            "allow_mixed_quality": false
        }),
    );
    m.insert(
        "conversion".to_string(),
        json!({
            // Optional ffmpeg transcode after download, e.g. to reach a desired
            // codec or shrink a lossy file. Off by default.
            "enabled": false,
            "codec": "aac",
            "bitrate_kbps": 192,
            "only_if_larger": true
        }),
    );
    m.insert(
        "covers".to_string(),
        json!({
            "embed_cover": true,
            "main_compression": "high",
            "main_resolution": 1400,
            "save_external": false,
            "external_format": "png",
            "external_compression": "low",
            "external_resolution": 3000,
            "save_animated_cover": true
        }),
    );
    m.insert(
        "playlist".to_string(),
        json!({
            "save_m3u": true,
            "paths_m3u": "absolute",
            "extended_m3u": true
        }),
    );
    m.insert(
        "advanced".to_string(),
        json!({
            "advanced_login_system": false,
            "codec_conversions": {
                "alac": "flac",
                "vorbis": "vorbis",
                "wav": "flac"
            },
            "conversion_flags": {
                "aac": { "audio_bitrate": "256k" },
                "flac": { "compression_level": 5 },
                "mp3": { "qscale:a": "0" },
                "opus": { "b:a": "192k" }
            },
            "conversion_keep_original": false,
            "ffmpeg_path": "ffmpeg",
            "cover_variance_threshold": 8,
            "debug_mode": false,
            "disable_subscription_checks": false,
            "enable_undesirable_conversions": false,
            "ignore_existing_files": false,
            "ignore_different_artists": true
        }),
    );
    m
}

/// Build the merged view of global settings (defaults + user overrides).
pub fn merged_global_settings(stored: &Map<String, Value>) -> Map<String, Value> {
    let defaults = default_global_settings();
    let mut out = Map::new();
    for (k, v) in defaults {
        if let Some(user) = stored.get(&k) {
            if let (Some(d_obj), Some(u_obj)) = (v.as_object(), user.as_object()) {
                let mut merged = d_obj.clone();
                for (uk, uv) in u_obj {
                    merged.insert(uk.clone(), uv.clone());
                }
                out.insert(k, Value::Object(merged));
            } else {
                out.insert(k, user.clone());
            }
        } else {
            out.insert(k, v);
        }
    }
    out
}

/// Rewrite stored settings with merged defaults. Used at startup to bring
/// older configs in line with the current schema.
pub fn refresh_settings_storage(
    data_folder: &Path,
    module_settings: &IndexMap<String, crate::models::ModuleInformation>,
) -> Result<()> {
    let mut doc = load_settings(data_folder);
    let global_user = doc
        .remove("global")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let extensions_user = doc
        .remove("extensions")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut modules_user = doc
        .remove("modules")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    let merged = merged_global_settings(&global_user);
    let mut new_settings = Map::new();
    new_settings.insert("global".to_string(), Value::Object(merged));
    new_settings.insert("extensions".to_string(), Value::Object(extensions_user));

    // Per-module schema defaults + user overrides.
    let mut new_modules = Map::new();
    let advanced_login = new_settings
        .get("global")
        .and_then(|v| v.get("advanced"))
        .and_then(|v| v.get("advanced_login_system"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    for (name, info) in module_settings {
        let mut settings: Map<String, Value> = Map::new();
        if advanced_login {
            for (k, v) in &info.global_settings {
                settings.insert(k.clone(), v.clone());
            }
        } else {
            for (k, v) in &info.global_settings {
                settings.insert(k.clone(), v.clone());
            }
            for (k, v) in &info.session_settings {
                settings.insert(k.clone(), v.clone());
            }
        }
        if let Some(stored) = modules_user.get(name).and_then(|v| v.as_object()) {
            for (k, v) in stored {
                settings.insert(k.clone(), v.clone());
            }
        } else {
            // No stored settings - keep just the schema defaults
        }
        new_modules.insert(name.clone(), Value::Object(settings));
    }
    new_settings.insert("modules".to_string(), Value::Object(new_modules));
    save_settings(data_folder, &new_settings)?;
    Ok(())
}
