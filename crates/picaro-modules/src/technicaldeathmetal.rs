use std::sync::Arc;

use picaro_utils::models::ModuleInformation;
use picaro_utils::{ModuleConstructor, ModuleRegistry};

use crate::wordpress_blog::{
    constructor as wp_constructor, module_information as wp_module_information,
    register_module as wp_register, WpBlogConfig,
};

static CFG: WpBlogConfig = WpBlogConfig {
    service_name: "TechnicalDeathMetal",
    host: "technicaldeathmetal.org",
    referer: "https://technicaldeathmetal.org/",
    no_image_marker: "no-image",
    // UNVERIFIED: search anchors verified as <a rel="bookmark">.
    url_constants: &[("album", true), ("ep", true), ("flac", true)],
    test_url: "https://technicaldeathmetal.org/?s=death",
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
