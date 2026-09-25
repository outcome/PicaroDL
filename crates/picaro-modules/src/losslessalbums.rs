use std::sync::Arc;

use picaro_utils::models::ModuleInformation;
use picaro_utils::{ModuleConstructor, ModuleRegistry};

use crate::dle_blog::{self, DleBlogConfig};

static CFG: DleBlogConfig = DleBlogConfig {
    service_name: "LosslessAlbums",
    host: "losslessalbums.club",
    referer: "https://losslessalbums.club/",
    title_suffix: "",
    no_image_marker: "no_image",
    get_host: None,
    hash_decode: false,
    url_constants: &[("album", false), ("hi-res", false), ("flac", false)],
    test_url: "https://losslessalbums.club/",
};

pub fn module_information() -> ModuleInformation {
    dle_blog::module_information(&CFG)
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    dle_blog::constructor(&CFG)
}

pub fn register_module(registry: &ModuleRegistry) {
    crate::registry::register(registry, module_information(), constructor());
}
