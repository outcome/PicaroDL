//! The `Picaro` core. Holds the module registry, the loaded module
//! instances, and the current session. Mirrors `picaro/core.py::Picaro`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use tracing::info;

use picaro_utils::error::Result;
use picaro_utils::models::{
    CoverCompression, CoverOptions, ImageFileType, ModuleController, PicaroOptions, Quality,
};
use picaro_utils::module::CodecOptions;
use picaro_utils::settings as settings_io;
use picaro_utils::{ModuleInterfacePtr, ModuleRegistry, RegisteredModule};

use crate::loader;

pub struct Picaro {
    pub data_folder: PathBuf,
    pub registry: ModuleRegistry,
    pub loaded: RwLock<HashMap<String, ModuleInterfacePtr>>,
    pub settings: serde_json::Map<String, serde_json::Value>,
    pub merged_globals: serde_json::Map<String, serde_json::Value>,
    pub settings_file_mtime: std::time::SystemTime,
}

impl Picaro {
    pub fn new(data_folder: PathBuf, registry: ModuleRegistry) -> Result<Self> {
        settings_io::ensure_data_dirs(&data_folder).ok();
        let settings = settings_io::load_settings(&data_folder);
        let merged_globals = settings_io::merged_global_settings(
            settings
                .get("global")
                .and_then(|v| v.as_object())
                .unwrap_or(&serde_json::Map::new()),
        );
        let settings_file_mtime = std::fs::metadata(settings_io::settings_path(&data_folder))
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| std::time::SystemTime::now());

        loader::apply_user_settings(
            &registry,
            settings
                .get("modules")
                .and_then(|v| v.as_object())
                .unwrap_or(&serde_json::Map::new()),
        );

        Ok(Self {
            data_folder,
            registry,
            loaded: RwLock::new(HashMap::new()),
            settings,
            merged_globals,
            settings_file_mtime,
        })
    }

    pub fn session_path(&self) -> PathBuf {
        settings_io::session_path(&self.data_folder)
    }

    pub fn settings_path(&self) -> PathBuf {
        settings_io::settings_path(&self.data_folder)
    }

    /// Build a `ModuleController` for the named module using the merged
    /// global settings + the module's stored settings.
    pub fn build_controller(&self, module: &str) -> Result<ModuleController> {
        let q = self
            .merged_globals
            .get("general")
            .and_then(|v| v.get("download_quality"))
            .and_then(|v| v.as_str())
            .unwrap_or("hifi")
            .to_string();
        let quality = picaro_utils::format::parse_quality(&q)?;
        let disable_sub_check = self
            .merged_globals
            .get("advanced")
            .and_then(|v| v.get("disable_subscription_checks"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let cover_file_type = self
            .merged_globals
            .get("covers")
            .and_then(|v| v.get("external_format"))
            .and_then(|v| v.as_str())
            .map(|s| match s {
                "png" => ImageFileType::Png,
                "webp" => ImageFileType::Webp,
                _ => ImageFileType::Jpg,
            })
            .unwrap_or(ImageFileType::Png);
        let cover_resolution = self
            .merged_globals
            .get("covers")
            .and_then(|v| v.get("main_resolution"))
            .and_then(|v| v.as_u64())
            .unwrap_or(1400) as u32;
        let cover_compression = self
            .merged_globals
            .get("covers")
            .and_then(|v| v.get("main_compression"))
            .and_then(|v| v.as_str())
            .map(|s| match s {
                "low" => CoverCompression::Low,
                _ => CoverCompression::High,
            })
            .unwrap_or(CoverCompression::High);
        let debug_mode = self
            .merged_globals
            .get("advanced")
            .and_then(|v| v.get("debug_mode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let play_sound = self
            .merged_globals
            .get("general")
            .and_then(|v| v.get("play_sound_on_finish"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let opts = PicaroOptions {
            debug_mode,
            disable_subscription_check: disable_sub_check,
            quality_tier: quality,
            default_cover_options: CoverOptions {
                file_type: cover_file_type,
                resolution: cover_resolution,
                compression: cover_compression,
            },
            play_sound_on_finish: play_sound,
        };
        let progress_bar_enabled = self
            .merged_globals
            .get("general")
            .and_then(|v| v.get("progress_bar"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        loader::build_controller(
            &self.registry,
            module,
            &self.data_folder,
            &self.session_path(),
            &opts,
            progress_bar_enabled,
        )
    }

    /// Load (or return cached) a module instance. Mirrors `Picaro.load_module`.
    pub async fn load_module(&self, module: &str) -> Result<ModuleInterfacePtr> {
        let module_lc = module.to_lowercase();
        if let Some(c) = self.loaded.read().get(&module_lc) {
            return Ok(c.clone());
        }
        let controller = self.build_controller(&module_lc)?;
        let rm: RegisteredModule = self
            .registry
            .get(&module_lc)
            .ok_or_else(|| picaro_utils::error::Error::InvalidModule(module_lc.clone()))?;
        let mi = rm.constructor.construct(controller)?;
        mi.init().ok();
        self.loaded.write().insert(module_lc, mi.clone());
        info!("loaded module {}", mi.name());
        Ok(mi)
    }

    /// Read the current download quality / codec options.
    pub fn codec_options(&self) -> CodecOptions {
        let spatial = self
            .merged_globals
            .get("codecs")
            .and_then(|v| v.get("spatial_codecs"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let proprietary = self
            .merged_globals
            .get("codecs")
            .and_then(|v| v.get("proprietary_codecs"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        CodecOptions {
            spatial_codecs: spatial,
            proprietary_codecs: proprietary,
        }
    }

    pub fn current_quality(&self) -> Quality {
        let q = self
            .merged_globals
            .get("general")
            .and_then(|v| v.get("download_quality"))
            .and_then(|v| v.as_str())
            .unwrap_or("hifi");
        picaro_utils::format::parse_quality(q).unwrap_or(Quality::HIFI)
    }

    /// Reload the on-disk settings file and refresh in-memory caches.
    pub fn reload_settings(&mut self) -> Result<()> {
        let new_doc = settings_io::load_settings(&self.data_folder);
        let new_merged = settings_io::merged_global_settings(
            new_doc
                .get("global")
                .and_then(|v| v.as_object())
                .unwrap_or(&serde_json::Map::new()),
        );
        loader::apply_user_settings(
            &self.registry,
            new_doc
                .get("modules")
                .and_then(|v| v.as_object())
                .unwrap_or(&serde_json::Map::new()),
        );
        self.settings = new_doc;
        self.merged_globals = new_merged;
        self.settings_file_mtime = std::fs::metadata(self.settings_path())
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| std::time::SystemTime::now());
        Ok(())
    }

    pub fn registry(&self) -> &ModuleRegistry {
        &self.registry
    }

    pub fn list_modules(&self) -> Vec<String> {
        self.registry.names()
    }
}
