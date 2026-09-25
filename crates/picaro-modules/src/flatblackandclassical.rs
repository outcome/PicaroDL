use std::sync::Arc;

use picaro_utils::models::ModuleInformation;
use picaro_utils::ModuleConstructor;

use crate::blogspot::{self, BlogspotConfig};

static CFG: BlogspotConfig = BlogspotConfig {
    service_name: "FlatBlackAndClassical",
    host: "www.flatblackandclassical.com",
    referer: "https://www.flatblackandclassical.com/",
    // UNVERIFIED: Blogspot post URLs are date based, so this is a best-effort hint only.
    // Blogger custom domain; feed verified at https://www.flatblackandclassical.com/feeds/posts/default?alt=json
    url_constants: &[("20", true)],
    test_url: "https://www.flatblackandclassical.com/",
};

pub fn module_information() -> ModuleInformation {
    blogspot::module_information(&CFG)
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    blogspot::constructor(&CFG)
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    blogspot::register_module(registry, &CFG);
}
