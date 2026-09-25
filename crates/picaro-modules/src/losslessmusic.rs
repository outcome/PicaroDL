use std::sync::Arc;

use picaro_utils::models::ModuleInformation;
use picaro_utils::{ModuleConstructor, ModuleRegistry};

use crate::wordpress_blog::{
    constructor as wp_constructor, module_information as wp_module_information,
    register_module as wp_register, WpBlogConfig,
};

static CFG: WpBlogConfig = WpBlogConfig {
    service_name: "LosslessMusic",
    host: "lossless-music.org",
    referer: "https://lossless-music.org/",
    no_image_marker: "no-image",
    url_constants: &[("album", true), ("blog", true), ("flac", true)],
    test_url: "https://lossless-music.org/",
};

pub fn module_information() -> ModuleInformation {
    wp_module_information(&CFG)
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    wp_constructor(&CFG)
}

pub fn register_module(registry: &ModuleRegistry) {
    wp_register(registry, &CFG)
}
