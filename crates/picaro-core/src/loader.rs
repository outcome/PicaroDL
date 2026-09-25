//! Module discovery and registration. The on-disk `modules/<name>/module.json`
//! is the manifest; the Rust binary then either compiles the module in (if
//! statically linked) or loads it via the dynamic loader (TODO).
//!
//! In practice, PicaroDL ships the modules as a single `picaro-modules`
//! crate, and we use this loader to wire them in by reading each module's
//! `module.json` and pairing it with the constructor exported in
//! `picaro_modules::*::CONSTRUCTOR`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use picaro_utils::error::{Error, Result};
use picaro_utils::models::{ModuleInformation, PicaroOptions};
use picaro_utils::settings as settings_io;
use picaro_utils::{ModuleConstructor, ModuleRegistry, RegisteredModule};
use tracing::{debug, info, warn};

/// Locate the modules directory (picaro-rs/modules/ next to the executable or
/// in the current working directory).
pub fn modules_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let cand = parent.join("modules");
            if cand.is_dir() {
                return cand;
            }
        }
    }
    PathBuf::from("modules")
}

pub fn extensions_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let cand = parent.join("extensions");
            if cand.is_dir() {
                return cand;
            }
        }
    }
    PathBuf::from("extensions")
}

pub fn config_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let cand = parent.join("config");
            if cand.is_dir() {
                return cand;
            }
        }
    }
    PathBuf::from("config")
}

/// Each module folder has a `module.json` that mirrors `module_information`
/// in the original Python. We use that file to discover the module's name,
/// supported modes, and default settings - so users can drop in/out modules
/// without recompiling.
pub fn discover_module_manifests(dir: &Path) -> Result<Vec<ModuleInformation>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if name.starts_with('_') || name == "example" {
            continue;
        }
        let manifest = path.join("module.json");
        if !manifest.is_file() {
            debug!("module folder '{name}' has no module.json - skipping");
            continue;
        }
        let text = std::fs::read_to_string(&manifest)?;
        let info: ModuleInformation = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warn!("module.json for '{name}' is invalid: {e}");
                continue;
            }
        };
        out.push(info);
    }
    Ok(out)
}

/// Build a registry from a list of (info, constructor) tuples.
pub fn build_registry(
    items: Vec<(ModuleInformation, std::sync::Arc<dyn ModuleConstructor>)>,
) -> ModuleRegistry {
    let registry = ModuleRegistry::new();
    for (info, ctor) in items {
        let rm = RegisteredModule {
            name: info.service_name.clone(),
            information: info.clone(),
            current_settings: None,
            constructor: ctor,
        };
        registry.insert(&info.service_name.to_lowercase(), rm);
    }
    registry
}

/// Refresh each module's `current_settings` to the merged schema-defaults +
/// user overrides map stored in `settings.json`.
pub fn apply_user_settings(
    registry: &ModuleRegistry,
    settings: &serde_json::Map<String, serde_json::Value>,
) {
    let modules_settings = match settings.get("modules").and_then(|v| v.as_object()) {
        Some(s) => s,
        None => return,
    };
    let names = registry.names();
    for name in names {
        let user_settings = modules_settings
            .get(&name)
            .and_then(|v| v.as_object())
            .cloned();
        if let Some(mut m) = registry.get(&name) {
            m.current_settings = user_settings;
            registry.insert(&name, m);
        }
    }
}

/// Build a `ModuleController` for the given module using the current settings.
pub fn build_controller(
    registry: &ModuleRegistry,
    module: &str,
    data_folder: &Path,
    session_path: &Path,
    opts: &PicaroOptions,
    progress_bar_enabled: bool,
) -> Result<picaro_utils::models::ModuleController> {
    let rm = registry
        .get(module)
        .ok_or_else(|| Error::InvalidModule(module.to_string()))?;
    let settings = rm.current_settings.clone().unwrap_or_default();
    let tsc = picaro_utils::models::TemporarySettingsController {
        module: module.to_string(),
        session_path: session_path.to_path_buf(),
    };
    let data_folder = data_folder.join("modules").join(module);
    std::fs::create_dir_all(&data_folder).ok();
    Ok(picaro_utils::models::ModuleController {
        module_settings: settings,
        data_folder,
        temporary_settings_controller: tsc,
        picaro_options: opts.clone(),
        get_current_timestamp: now_unix,
        progress_bar_enabled,
        debug_mode: opts.debug_mode,
    })
}

/// Get the current UTC timestamp as Unix seconds.
pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Update the on-disk settings file so it matches the current module list.
/// Mirrors `Picaro.update_module_storage` in OrpheusDL.
pub fn persist_settings_for_modules(data_folder: &Path, registry: &ModuleRegistry) -> Result<()> {
    let mut module_settings = indexmap::IndexMap::new();
    for rm in registry.iter() {
        module_settings.insert(
            rm.information.service_name.to_lowercase(),
            rm.information.clone(),
        );
    }
    settings_io::refresh_settings_storage(data_folder, &module_settings)
}

/// Format the merged global settings (defaults + user overrides) as a
/// `serde_json::Map`.
pub fn merged_globals(data_folder: &Path) -> serde_json::Map<String, serde_json::Value> {
    let stored = settings_io::load_settings(data_folder);
    let global = stored
        .get("global")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    settings_io::merged_global_settings(&global)
}

/// Update a single module's settings in-memory and on disk.
pub fn update_module_setting(
    data_folder: &Path,
    module: &str,
    key: &str,
    value: serde_json::Value,
) -> Result<()> {
    let mut doc = settings_io::load_settings(data_folder);
    let modules = doc
        .entry("modules".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !modules.is_object() {
        *modules = serde_json::Value::Object(serde_json::Map::new());
    }
    let map = modules.as_object_mut().unwrap();
    let entry = map
        .entry(module.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = serde_json::Value::Object(serde_json::Map::new());
    }
    let eo = entry.as_object_mut().unwrap();
    eo.insert(key.to_string(), value);
    settings_io::save_settings(data_folder, &doc)?;
    Ok(())
}

/// Update a global setting in-memory and on disk.
pub fn update_global_setting(
    data_folder: &Path,
    section: &str,
    key: &str,
    value: serde_json::Value,
) -> Result<()> {
    let mut doc = settings_io::load_settings(data_folder);
    let global = doc
        .entry("global".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !global.is_object() {
        *global = serde_json::Value::Object(serde_json::Map::new());
    }
    let g = global.as_object_mut().unwrap();
    let sec = g
        .entry(section.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !sec.is_object() {
        *sec = serde_json::Value::Object(serde_json::Map::new());
    }
    let so = sec.as_object_mut().unwrap();
    so.insert(key.to_string(), value);
    settings_io::save_settings(data_folder, &doc)?;
    Ok(())
}

pub fn ensure_data_dirs(root: &Path) -> std::io::Result<()> {
    settings_io::ensure_data_dirs(root)
}

/// Apply the per-module user-settings map (read from `settings.json`) on
/// top of the registry's stored `module_information` defaults.
pub fn reload_settings_into_registry(data_folder: &Path, registry: &ModuleRegistry) {
    let doc = settings_io::load_settings(data_folder);
    apply_user_settings(
        registry,
        doc.get("modules")
            .and_then(|v| v.as_object())
            .unwrap_or(&serde_json::Map::new()),
    );
    let _ = doc; // silence unused
}
