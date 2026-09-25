use std::sync::Arc;

use picaro_utils::models::ModuleInformation;
use picaro_utils::{ModuleConstructor, ModuleRegistry};

use crate::wordpress_blog::{
    constructor as wp_constructor, module_information as wp_module_information,
    register_module as wp_register, WpBlogConfig,
};

static CFG: WpBlogConfig = WpBlogConfig {
    service_name: "HipHop94",
    host: "94hiphop.com",
    referer: "https://94hiphop.com/",
    no_image_marker: "no-image",
    // NOTE: theme renders search hits as plain <a href="https://94hiphop.com/<slug>">Title</a>
    // (no rel="bookmark" / entry-title / post-title). Album pages download fine, but the
    // shared wordpress_blog.rs search parser may not pick these anchors up yet.
    url_constants: &[("album", true), ("mixtape", true), ("flac", true)],
    test_url: "https://94hiphop.com/?s=nas",
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
