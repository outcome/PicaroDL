//! ThemFire (DLE blog) site module, built on the reusable `dle_blog` template.

use std::sync::Arc;

use picaro_utils::models::ModuleInformation;
use picaro_utils::ModuleConstructor;

use crate::dle_blog::{self, DleBlogConfig};

static CFG: DleBlogConfig = DleBlogConfig {
    service_name: "ThemFire",
    host: "themfire.pro",
    referer: "https://themfire.pro/",
    title_suffix: "",
    no_image_marker: "no_image",
    get_host: None,
    hash_decode: false,
    url_constants: &[
        ("album", false),
        ("albums", false),
        ("singles", true),
        ("newsongs", true),
        ("lossless", false),
        ("flac", true),
    ],
    test_url: "https://themfire.pro/",
};

pub fn module_information() -> ModuleInformation {
    dle_blog::module_information(&CFG)
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    dle_blog::constructor(&CFG)
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    crate::registry::register(registry, module_information(), constructor());
}
