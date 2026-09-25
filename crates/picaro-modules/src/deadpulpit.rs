use std::sync::Arc;

use picaro_utils::models::ModuleInformation;
use picaro_utils::{ModuleConstructor, ModuleRegistry};

use crate::blogspot::{
    constructor as bs_constructor, module_information as bs_module_information,
    register_module as bs_register, BlogspotConfig,
};

static CFG: BlogspotConfig = BlogspotConfig {
    service_name: "DeadPulpit",
    host: "deadpulpit.com",
    referer: "https://deadpulpit.com/",
    url_constants: &[("album", true), ("flac", true)],
    test_url: "https://deadpulpit.com/",
};

pub fn module_information() -> ModuleInformation {
    bs_module_information(&CFG)
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    bs_constructor(&CFG)
}

pub fn register_module(registry: &ModuleRegistry) {
    bs_register(registry, &CFG)
}
