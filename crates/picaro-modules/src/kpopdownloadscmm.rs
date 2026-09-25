use std::sync::Arc;

use picaro_utils::models::ModuleInformation;
use picaro_utils::ModuleConstructor;

use crate::blogspot::{self, BlogspotConfig};

static CFG: BlogspotConfig = BlogspotConfig {
    service_name: "KpopDownloadSCMM",
    host: "kpopdownloadscmm.blogspot.com",
    referer: "https://kpopdownloadscmm.blogspot.com/",
    // NOTE: this blog has the Atom feed disabled (feeds/posts/default => 404) and its
    // theme emits single-quoted post links (<h3 class='post-title entry-title'>
    // <a href='...'>) which the shared blogspot.rs HTML fallback does not match yet.
    // Server-side /search?q= works and returns those posts.
    url_constants: &[("20", true)],
    test_url: "https://kpopdownloadscmm.blogspot.com/search?q=blackpink",
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
